#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import ipaddress
import json
import sys
import tempfile
import unittest
from collections.abc import Iterable
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).parent))
import nvx_tools.egress_policy as egress_policy  # noqa: E402
from nvx_tools.common import ScriptError  # noqa: E402
from nvx_tools.egress_policy import (
    MAX_POLICY_FILE_SIZE,  # noqa: E402
    MAX_RULES_PER_ACTION,  # noqa: E402
    compile_policy_file,  # noqa: E402
)


class EgressPolicyTests(unittest.TestCase):
    def compile(self, value: object):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "policy.json"
            path.write_text(json.dumps(value), encoding="utf-8")
            return compile_policy_file(path)

    def test_compiles_inclusive_tcp_and_udp_ranges(self):
        compiled = self.compile(
            {
                "allow": [
                    {
                        "to": [{"cidr": "192.0.2.7"}],
                        "ports": [
                            {
                                "protocol": "tcp",
                                "port": 8000,
                                "endPort": 8002,
                            }
                        ],
                    },
                    {
                        "to": [{"cidr": "198.51.100.0/24"}],
                        "ports": [
                            {
                                "protocol": "udp",
                                "port": 5000,
                                "endPort": 5001,
                            }
                        ],
                    },
                ]
            }
        )

        self.assertEqual(
            compiled.allow,
            (
                "192.0.2.7/32:tcp:8000-8002",
                "198.51.100.0/24:udp:5000-5001",
            ),
        )
        self.assertEqual(compiled.deny, ())

    def test_lowers_each_range_to_one_native_range_rule(self):
        ranges: tuple[tuple[dict[str, object], tuple[str, ...]], ...] = (
            (
                {"protocol": "tcp", "port": 8000, "endPort": 8010},
                ("192.0.2.7/32:tcp:8000-8010",),
            ),
            (
                {"protocol": "udp", "port": 5000, "endPort": 5010},
                ("192.0.2.7/32:udp:5000-5010",),
            ),
            (
                {"protocol": "tcp", "port": 1, "endPort": 65535},
                ("192.0.2.7/32:tcp:1-65535",),
            ),
            (
                {"protocol": "any", "port": 1, "endPort": 65535},
                ("192.0.2.7/32:tcp:1-65535", "192.0.2.7/32:udp:1-65535"),
            ),
            # A range of one port lowers to that port's rule.
            (
                {"protocol": "tcp", "port": 8000, "endPort": 8000},
                ("192.0.2.7/32:tcp:8000",),
            ),
        )

        for category in ("allow", "deny"):
            for selector, expected in ranges:
                for rule in (
                    {"to": [{"cidr": "192.0.2.7"}], "ports": [selector]},
                    {"cidr": "192.0.2.7", **selector},
                ):
                    with self.subTest(category=category, rule=rule):
                        compiled = self.compile({category: [rule]})
                        self.assertEqual(getattr(compiled, category), expected)

    def test_keeps_a_denied_port_inside_an_allowed_range(self):
        compiled = self.compile(
            {
                "allow": [
                    {
                        "to": [{"cidr": "192.0.2.0/24"}],
                        "ports": [{"protocol": "tcp", "port": 8000, "endPort": 8010}],
                    }
                ],
                "deny": [
                    {
                        "to": [{"cidr": "192.0.2.0/24"}],
                        "ports": [{"protocol": "tcp", "port": 8005}],
                    }
                ],
            }
        )

        # OpenVMM applies deny precedence, so each list keeps its own rule.
        self.assertEqual(compiled.allow, ("192.0.2.0/24:tcp:8000-8010",))
        self.assertEqual(compiled.deny, ("192.0.2.0/24:tcp:8005",))

    def test_merges_adjacent_and_overlapping_ranges_of_one_network(self):
        def ranges(*bounds: tuple[int, int]) -> list[dict[str, object]]:
            return [
                {
                    "cidr": "192.0.2.7",
                    "protocol": "tcp",
                    "port": start,
                    "endPort": end,
                }
                for start, end in bounds
            ]

        for rules, expected in (
            (ranges((8000, 8004), (8005, 8010)), ("192.0.2.7/32:tcp:8000-8010",)),
            (ranges((8000, 8008), (8003, 8010)), ("192.0.2.7/32:tcp:8000-8010",)),
            (
                [
                    *ranges((8000, 8010)),
                    *ranges(*((port, port) for port in range(8000, 8011))),
                ],
                ("192.0.2.7/32:tcp:8000-8010",),
            ),
            (
                ranges((8000, 8004), (8006, 8010)),
                ("192.0.2.7/32:tcp:8000-8004", "192.0.2.7/32:tcp:8006-8010"),
            ),
        ):
            with self.subTest(rules=rules):
                self.assertEqual(self.compile({"allow": rules}).allow, expected)

        # Networks keep their own ranges where their segments differ.
        compiled = self.compile(
            {
                "allow": [
                    {"cidr": "192.0.2.7", "protocol": "udp", "port": 1, "endPort": 10},
                    {
                        "cidr": "198.51.100.9",
                        "protocol": "udp",
                        "port": 5,
                        "endPort": 20,
                    },
                ]
            }
        )
        self.assertEqual(
            compiled.allow,
            ("192.0.2.7/32:udp:1-10", "198.51.100.9/32:udp:5-20"),
        )

    def test_keeps_network_range_whole_when_adjacent_rule_changes_union(self):
        compiled = self.compile(
            {
                "allow": [
                    {
                        "cidr": "10.0.0.0/25",
                        "protocol": "tcp",
                        "port": 1,
                        "endPort": 10,
                    },
                    {
                        "cidr": "10.0.0.128/25",
                        "protocol": "tcp",
                        "port": 5,
                        "endPort": 6,
                    },
                ]
            }
        )

        self.assertEqual(
            compiled.allow,
            ("10.0.0.0/24:tcp:5-6", "10.0.0.0/25:tcp:1-10"),
        )

    def test_rejects_invalid_port_shapes_and_values(self):
        invalid_rules = (
            {"cidr": "192.0.2.0/24", "protocol": "tcp", "endPort": 80},
            {"cidr": "192.0.2.0/24", "protocol": "any", "endPort": 80},
            {
                "cidr": "192.0.2.0/24",
                "protocol": "tcp",
                "port": 81,
                "endPort": 80,
            },
            {"cidr": "192.0.2.0/24", "protocol": "tcp", "port": 0},
            {"cidr": "192.0.2.0/24", "protocol": "udp", "port": 65536},
            {"cidr": "192.0.2.0/24", "protocol": "tcp", "port": True},
            {"cidr": "192.0.2.0/24", "protocol": "any", "port": 0},
            {"cidr": "192.0.2.0/24", "port": 80},
            {"cidr": "192.0.2.0/24", "protocol": "icmp", "port": 8},
            {"cidr": "192.0.2.0/24", "protocol": "icmp", "endPort": 8},
            {"cidr": "192.0.2.0/24", "protocol": "ICMP"},
            {"cidr": "192.0.2.0/24", "protocol": "sctp"},
        )

        for rule in invalid_rules:
            with self.subTest(rule=rule), self.assertRaises(ScriptError):
                self.compile({"allow": [rule]})

    def test_reports_invalid_port_ranges_clearly(self):
        cidr = "192.0.2.0/24"
        cases: tuple[tuple[dict[str, object], str], ...] = (
            (
                {
                    "to": [{"cidr": cidr}],
                    "ports": [{"protocol": "tcp", "port": 8010, "endPort": 8000}],
                },
                r"^deny\[0\]\.ports\[0\]\.endPort 8000 cannot be below port 8010$",
            ),
            (
                {"ports": [{"protocol": "udp", "endPort": 8010}]},
                r"^deny\[0\]\.ports\[0\]\.endPort requires port$",
            ),
            (
                {"ports": [{"endPort": 8010}]},
                r"^deny\[0\]\.ports\[0\]\.endPort requires port$",
            ),
            (
                {"ports": [{"protocol": "tcp", "port": 8000, "endPort": 65536}]},
                r"^deny\[0\]\.ports\[0\]\.endPort must be between 1 and 65535$",
            ),
            (
                {"cidr": cidr, "protocol": "tcp", "port": 8010, "endPort": 8000},
                r"^deny\[0\]\.endPort 8000 cannot be below port 8010$",
            ),
            (
                {"cidr": cidr, "protocol": "udp", "endPort": 8010},
                r"^deny\[0\]\.endPort requires port$",
            ),
            (
                {"cidr": cidr, "endPort": 8010},
                r"^deny\[0\]\.endPort requires port$",
            ),
            (
                {"cidr": cidr, "protocol": "tcp", "port": 8000, "endPort": 65536},
                r"^deny\[0\]\.endPort must be between 1 and 65535$",
            ),
            (
                {"cidr": cidr, "protocol": "tcp", "port": 0, "endPort": 8010},
                r"^deny\[0\]\.port must be between 1 and 65535$",
            ),
        )

        for rule, message in cases:
            with (
                self.subTest(rule=rule),
                self.assertRaisesRegex(ScriptError, message),
            ):
                self.compile({"deny": [rule]})

    def test_compiles_protocol_selectors_with_and_without_ports(self):
        selectors: tuple[tuple[dict[str, object], tuple[str, ...]], ...] = (
            ({"protocol": "tcp"}, ("192.0.2.7/32:tcp",)),
            ({"protocol": "udp"}, ("192.0.2.7/32:udp",)),
            ({"protocol": "icmp"}, ("192.0.2.7/32:icmp",)),
            ({"protocol": "any"}, ("192.0.2.7/32",)),
            ({}, ("192.0.2.7/32",)),
            (
                {"protocol": "any", "port": 443},
                ("192.0.2.7/32:tcp:443", "192.0.2.7/32:udp:443"),
            ),
            (
                {"protocol": "any", "port": 443, "endPort": 444},
                ("192.0.2.7/32:tcp:443-444", "192.0.2.7/32:udp:443-444"),
            ),
        )

        for category in ("allow", "deny"):
            for selector, expected in selectors:
                with self.subTest(category=category, selector=selector):
                    compiled = self.compile(
                        {category: [{"cidr": "192.0.2.7", **selector}]}
                    )
                    self.assertEqual(getattr(compiled, category), expected)

    def test_protocol_wide_rules_cover_their_port_rules_only(self):
        compiled = self.compile(
            {
                "allow": [
                    {"cidr": "192.0.2.7", "protocol": "tcp", "port": 443},
                    {"cidr": "192.0.2.0/24", "protocol": "tcp"},
                    {"cidr": "192.0.2.7", "protocol": "udp", "port": 443},
                    {"cidr": "192.0.2.0/24", "protocol": "icmp"},
                ],
                "deny": [
                    {"cidr": "198.51.100.0/24", "protocol": "any", "port": 53},
                    {"cidr": "198.51.100.0/24", "protocol": "udp"},
                ],
            }
        )

        self.assertEqual(
            compiled.allow,
            (
                "192.0.2.0/24:icmp",
                "192.0.2.0/24:tcp",
                "192.0.2.7/32:udp:443",
            ),
        )
        self.assertEqual(
            compiled.deny,
            ("198.51.100.0/24:tcp:53", "198.51.100.0/24:udp"),
        )

    def test_address_only_rules_cover_protocol_wide_rules(self):
        compiled = self.compile(
            {
                "allow": [
                    {"cidr": "192.0.2.0/24", "protocol": "udp"},
                    {"cidr": "192.0.2.0/25"},
                    {"cidr": "192.0.2.0/25", "protocol": "icmp"},
                    {"cidr": "192.0.2.128/25", "protocol": "udp", "port": 53},
                ]
            }
        )

        # Widening the UDP rule into the address-only half keeps it to one prefix.
        self.assertEqual(compiled.allow, ("192.0.2.0/24:udp", "192.0.2.0/25"))

    def test_protocol_wide_rules_share_the_native_rule_budget(self):
        address_only = [
            {"cidr": f"10.0.{index}.1/32"} for index in range(MAX_RULES_PER_ACTION)
        ]
        accepted = self.compile(
            {"allow": [*address_only[:-1], {"cidr": "192.0.2.0/24", "protocol": "tcp"}]}
        )
        self.assertEqual(len(accepted.allow), MAX_RULES_PER_ACTION)
        self.assertIn("192.0.2.0/24:tcp", accepted.allow)

        with self.assertRaisesRegex(ScriptError, "at most 256"):
            self.compile(
                {
                    "allow": [
                        *address_only,
                        {"cidr": "192.0.2.0/24", "protocol": "icmp"},
                    ]
                }
            )

        # Protocol `any` with a port range lowers to a TCP and a UDP range.
        any_range = {
            "cidr": "192.0.2.1",
            "protocol": "any",
            "port": 1,
            "endPort": 129,
        }
        accepted = self.compile({"deny": [*address_only[:-2], any_range]})
        self.assertEqual(len(accepted.deny), MAX_RULES_PER_ACTION)
        self.assertIn("192.0.2.1/32:udp:1-129", accepted.deny)
        with self.assertRaisesRegex(ScriptError, "at most 256"):
            self.compile({"deny": [*address_only[:-1], any_range]})

    def test_normalizes_host_bits_like_the_native_cidr_parser(self):
        compiled = self.compile({"allow": [{"cidr": "10.0.0.5/24"}]})
        self.assertEqual(compiled.allow, ("10.0.0.0/24",))

    def test_subtracts_rule_local_cidr_exclusions(self):
        compiled = self.compile(
            {
                "allow": [
                    {
                        "to": [
                            {
                                "cidr": "192.0.2.0/24",
                                "except": ["192.0.2.128/25"],
                            }
                        ],
                    },
                    {"to": [{"cidr": "192.0.2.200/32"}]},
                ],
                "deny": [
                    {
                        "to": [
                            {
                                "cidr": "198.51.100.0/24",
                                "except": ["198.51.100.128/25"],
                            }
                        ],
                    }
                ],
            }
        )

        self.assertEqual(
            compiled.allow,
            ("192.0.2.0/25", "192.0.2.200/32"),
        )
        self.assertEqual(compiled.deny, ("198.51.100.0/25",))

    def test_compiles_mxc_destination_and_port_unions(self):
        compiled = self.compile(
            {
                "allow": [
                    {
                        "ports": [
                            {"port": 53},
                            {"protocol": "icmp"},
                        ]
                    }
                ],
                "deny": [
                    {
                        "to": [
                            {"cidr": "198.51.100.0/25"},
                            {"cidr": "198.51.100.128/25"},
                        ],
                        "ports": [
                            {"protocol": "tcp", "port": 80},
                            {"protocol": "udp", "port": 53},
                        ],
                    }
                ],
            }
        )

        # A rule without destinations matches every destination of both
        # families.
        self.assertEqual(
            compiled.allow,
            (
                "0.0.0.0/0:icmp",
                "0.0.0.0/0:tcp:53",
                "0.0.0.0/0:udp:53",
                "::/0:icmp",
                "::/0:tcp:53",
                "::/0:udp:53",
            ),
        )
        self.assertEqual(
            compiled.deny,
            ("198.51.100.0/24:tcp:80", "198.51.100.0/24:udp:53"),
        )

    def test_deduplicates_overlapping_exclusions_and_collapses_safe_prefixes(self):
        compiled = self.compile(
            {
                "allow": [
                    {
                        "cidr": "192.0.2.0/24",
                        "except": [
                            "192.0.2.128/25",
                            "192.0.2.128/25",
                            "192.0.2.192/26",
                        ],
                        "protocol": "tcp",
                        "port": 443,
                    },
                    {
                        "cidr": "198.51.100.0/25",
                        "protocol": "udp",
                        "port": 53,
                    },
                    {
                        "cidr": "198.51.100.128/25",
                        "protocol": "udp",
                        "port": 53,
                    },
                ]
            }
        )

        self.assertEqual(
            compiled.allow,
            (
                "192.0.2.0/25:tcp:443",
                "198.51.100.0/24:udp:53",
            ),
        )

    def test_rejects_exclusions_outside_parent_or_wrong_family(self):
        for cidr, excluded in (
            ("192.0.2.0/24", "198.51.100.0/24"),
            ("192.0.2.0/24", "2001:db8::/32"),
            ("2001:db8::/32", "2001:db9::/48"),
            ("2001:db8::/32", "192.0.2.0/24"),
            ("::/96", "0.0.0.0/8"),
        ):
            with (
                self.subTest(cidr=cidr, excluded=excluded),
                self.assertRaisesRegex(ScriptError, "except"),
            ):
                self.compile(
                    {
                        "allow": [
                            {
                                "to": [
                                    {
                                        "cidr": cidr,
                                        "except": [excluded],
                                    }
                                ],
                            }
                        ]
                    }
                )

    def test_compiles_ipv6_networks_with_every_selector(self):
        selectors: tuple[tuple[dict[str, object], tuple[str, ...]], ...] = (
            ({"ports": [{"protocol": "tcp"}]}, ("2001:db8::7/128:tcp",)),
            ({"ports": [{"protocol": "udp"}]}, ("2001:db8::7/128:udp",)),
            ({"ports": [{"protocol": "icmp"}]}, ("2001:db8::7/128:icmp",)),
            ({}, ("2001:db8::7/128",)),
            (
                {"ports": [{"protocol": "any", "port": 443}]},
                ("2001:db8::7/128:tcp:443", "2001:db8::7/128:udp:443"),
            ),
            (
                {"ports": [{"protocol": "tcp", "port": 8000, "endPort": 8010}]},
                ("2001:db8::7/128:tcp:8000-8010",),
            ),
        )
        for category in ("allow", "deny"):
            for selector, expected in selectors:
                with self.subTest(category=category, selector=selector):
                    compiled = self.compile(
                        {category: [{"to": [{"cidr": "2001:db8::7"}], **selector}]}
                    )
                    self.assertEqual(getattr(compiled, category), expected)

    def test_compiles_one_allowed_ipv6_network_with_a_denied_address(self):
        compiled = self.compile(
            {
                "allow": [
                    {
                        "to": [{"cidr": "2001:db8:1::/64"}],
                        "ports": [{"protocol": "tcp", "port": 443}],
                    },
                    {
                        "to": [{"cidr": "::/0"}],
                        "ports": [{"protocol": "udp", "port": 53}],
                    },
                ],
                "deny": [{"to": [{"cidr": "2001:db8:1::123"}]}],
            }
        )
        self.assertEqual(compiled.allow, ("::/0:udp:53", "2001:db8:1::/64:tcp:443"))
        self.assertEqual(compiled.deny, ("2001:db8:1::123/128",))

    def test_flat_rules_accept_ipv6_networks(self):
        compiled = self.compile(
            {
                "allow": [
                    {
                        "cidr": "2001:db8::/32",
                        "except": ["2001:db8::/33"],
                        "protocol": "tcp",
                        "port": 443,
                    }
                ]
            }
        )
        self.assertEqual(compiled.allow, ("2001:db8:8000::/33:tcp:443",))

    def test_keeps_each_address_family_separate(self):
        # IPv4 0.0.0.0/0 and IPv6 ::/96 span the same integers, so lowering
        # both families together would merge or cover one with the other.
        compiled = self.compile(
            {
                "allow": [
                    {
                        "to": [{"cidr": "::/96"}, {"cidr": "0.0.0.0/0"}],
                        "ports": [{"protocol": "tcp", "port": 80}],
                    },
                    {"to": [{"cidr": "192.0.2.0/24"}]},
                    {
                        "to": [{"cidr": "::c000:200/120"}],
                        "ports": [{"protocol": "udp", "port": 53}],
                    },
                ],
                "deny": [
                    {"to": [{"cidr": "0.0.0.0/0"}]},
                    {"to": [{"cidr": "::/96"}], "ports": [{"protocol": "icmp"}]},
                ],
            }
        )
        self.assertEqual(
            compiled.allow,
            (
                "0.0.0.0/0:tcp:80",
                "192.0.2.0/24",
                "::/96:tcp:80",
                "::c000:200/120:udp:53",
            ),
        )
        self.assertEqual(compiled.deny, ("0.0.0.0/0", "::/96:icmp"))

    def test_lowers_the_destinations_of_each_family_separately(self):
        compiled = self.compile(
            {
                "allow": [
                    {
                        "to": [
                            {"cidr": "192.0.2.0/25"},
                            {"cidr": "2001:db8::/32", "except": ["2001:db8::/33"]},
                            {"cidr": "192.0.2.128/25"},
                        ],
                        "ports": [{"protocol": "tcp", "port": 443}],
                    }
                ]
            }
        )
        self.assertEqual(
            compiled.allow,
            ("192.0.2.0/24:tcp:443", "2001:db8:8000::/33:tcp:443"),
        )

    def test_subtracts_ipv6_exclusions(self):
        compiled = self.compile(
            {
                "deny": [
                    {
                        "to": [
                            {
                                "cidr": "2001:db8::/32",
                                "except": ["2001:db8:1::/48", "2001:db8:1::/64"],
                            }
                        ],
                        "ports": [{"protocol": "tcp", "port": 22}],
                    }
                ]
            }
        )
        expected = sorted(
            ipaddress.IPv6Network("2001:db8::/32").address_exclude(
                ipaddress.IPv6Network("2001:db8:1::/48")
            ),
            key=lambda network: int(network.network_address),
        )
        self.assertEqual(
            compiled.deny, tuple(f"{network}:tcp:22" for network in expected)
        )
        self.assertEqual(len(compiled.deny), 16)

    def test_rejects_ipv6_scopes(self):
        for cidr in ("fe80::%eth0/64", "fe80::1%1"):
            with (
                self.subTest(cidr=cidr),
                self.assertRaisesRegex(ScriptError, "scope"),
            ):
                self.compile({"allow": [{"to": [{"cidr": cidr}]}]})

    def test_both_families_share_the_native_rule_budget(self):
        # Ports that are not adjacent cannot form one range, so each lowers to
        # its own native rule.
        def rules(ipv4_ports: int, ipv6_ports: int) -> dict[str, object]:
            return {
                "allow": [
                    {
                        "to": [{"cidr": cidr}],
                        "ports": [
                            {"protocol": "tcp", "port": 2 * index + 1}
                            for index in range(ports)
                        ],
                    }
                    for cidr, ports in (
                        ("192.0.2.1", ipv4_ports),
                        ("2001:db8::1", ipv6_ports),
                    )
                ]
            }

        self.assertEqual(len(self.compile(rules(128, 128)).allow), 256)
        for ipv4_ports, ipv6_ports in ((129, 128), (128, 129)):
            with (
                self.subTest(ipv4_ports=ipv4_ports, ipv6_ports=ipv6_ports),
                self.assertRaisesRegex(ScriptError, "at most 256"),
            ):
                self.compile(rules(ipv4_ports, ipv6_ports))

    def test_rejects_invalid_mxc_rule_shapes(self):
        invalid_rules: tuple[object, ...] = (
            {"to": list[object]()},
            {"to": [dict[str, object]()]},
            {"to": [{"cidr": "192.0.2.0/24", "unknown": True}]},
            {"ports": list[object]()},
            {"ports": [{"protocol": "icmp", "port": 8}]},
            {"ports": [{"protocol": "ICMP"}]},
            {"ports": [{"protocol": "sctp"}]},
            {"ports": [{"endPort": 80}]},
            {"to": [{"cidr": "192.0.2.0/24"}], "cidr": "192.0.2.0/24"},
        )

        for rule in invalid_rules:
            with self.subTest(rule=rule), self.assertRaises(ScriptError):
                self.compile({"allow": [rule]})

    def test_rejects_unknown_and_malformed_fields(self):
        invalid: tuple[object, ...] = (
            [],
            {"unknown": []},
            {"allow": {}},
            {"allow": ["192.0.2.0/24"]},
            {"allow": [{"cidr": "192.0.2.0/24", "unknown": 1}]},
            {"allow": [{"cidr": 7}]},
            {"allow": [{"cidr": "192.0.2.0/24", "except": "192.0.2.1"}]},
            {"allow": [{"cidr": "192.0.2.0/24", "protocol": None}]},
        )

        for value in invalid:
            with self.subTest(value=value), self.assertRaises(ScriptError):
                self.compile(value)

    def test_rejects_duplicate_json_fields(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "policy.json"
            for text in (
                '{"deny":[{"cidr":"0.0.0.0/0"}],"deny":[]}',
                '{"allow":[{"cidr":"192.0.2.0/24","port":80,"port":443,"protocol":"tcp"}]}',
            ):
                with self.subTest(text=text):
                    path.write_text(text, encoding="utf-8")
                    with self.assertRaisesRegex(ScriptError, "duplicate JSON property"):
                        compile_policy_file(path)

    def test_empty_destination_does_not_expand_port_range(self):
        compiled = self.compile(
            {
                "allow": [
                    {
                        "cidr": "192.0.2.0/24",
                        "except": ["192.0.2.0/24"],
                        "protocol": "tcp",
                        "port": 1,
                        "endPort": 65535,
                    }
                ]
            }
        )
        self.assertEqual(compiled.allow, ())
        self.assertEqual(compiled.deny, ())

    def test_limits_the_actual_policy_read(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "policy.json"
            path.write_bytes(b" " * (MAX_POLICY_FILE_SIZE + 1))
            with mock.patch.object(Path, "stat") as stat:
                stat.return_value.st_size = 0
                with self.assertRaisesRegex(ScriptError, "byte limit"):
                    compile_policy_file(path)

    def test_reports_oversized_json_integer_rejection(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "policy.json"
            path.write_text(
                '{"allow":[{"cidr":' + ("9" * 10000) + "}]}",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                ScriptError, "^JSON integer exceeds 64-digit limit$"
            ):
                compile_policy_file(path)

    def test_many_source_networks_can_collapse_within_native_budget(self):
        start = int(ipaddress.IPv4Address("192.0.2.0"))
        compiled = self.compile(
            {
                "allow": [
                    {
                        "cidr": str(ipaddress.IPv4Address(start + offset)),
                        "protocol": "tcp",
                        "port": 80,
                    }
                    for offset in range(512)
                ]
            }
        )
        self.assertEqual(compiled.allow, ("192.0.2.0/23:tcp:80",))

    def test_accepts_exact_256_rule_boundaries(self):
        def separate_ranges(cidr: str, protocol: str) -> list[dict[str, object]]:
            # Ranges with a port between them cannot merge.
            return [
                {
                    "cidr": cidr,
                    "protocol": protocol,
                    "port": 3 * index + 1,
                    "endPort": 3 * index + 2,
                }
                for index in range(MAX_RULES_PER_ACTION)
            ]

        compiled = self.compile(
            {
                "allow": separate_ranges("192.0.2.1", "tcp"),
                "deny": separate_ranges("198.51.100.1", "udp"),
            }
        )

        self.assertEqual(len(compiled.allow), 256)
        self.assertEqual(len(compiled.deny), 256)
        self.assertEqual(
            compiled.allow[:2], ("192.0.2.1/32:tcp:1-2", "192.0.2.1/32:tcp:4-5")
        )
        self.assertEqual(compiled.deny[-1], "198.51.100.1/32:udp:766-767")

    def test_rejects_257_rules_before_materializing_them(self):
        for category in ("allow", "deny"):
            with (
                self.subTest(category=category),
                self.assertRaisesRegex(ScriptError, "at most 256"),
            ):
                self.compile(
                    {
                        category: [
                            {
                                "cidr": "192.0.2.1",
                                "protocol": "tcp",
                                "port": 2 * index + 1,
                            }
                            for index in range(MAX_RULES_PER_ACTION + 1)
                        ]
                    }
                )

    def test_overlapping_segments_do_not_inflate_native_rule_budget(self):
        internal_ports = range(1000, 1056, 2)
        compiled = self.compile(
            {
                "allow": [
                    {
                        "cidr": "0.0.0.0/0",
                        "except": ["10.0.0.0/8"],
                        "protocol": "tcp",
                        "port": 1,
                        "endPort": 65535,
                    },
                    *(
                        {
                            "cidr": "10.0.0.0/8",
                            "protocol": "tcp",
                            "port": port,
                        }
                        for port in internal_ports
                    ),
                ]
            }
        )

        expected_external = {
            f"{cidr}:tcp:1-65535"
            for cidr in (
                "0.0.0.0/5",
                "8.0.0.0/7",
                "11.0.0.0/8",
                "12.0.0.0/6",
                "16.0.0.0/4",
                "32.0.0.0/3",
                "64.0.0.0/2",
                "128.0.0.0/1",
            )
        }
        expected_internal = {f"0.0.0.0/0:tcp:{port}" for port in internal_ports}
        self.assertEqual(set(compiled.allow), expected_external | expected_internal)
        self.assertEqual(len(compiled.allow), 36)

    def test_bounds_exclusions_times_port_segments(self):
        def rules(count: int) -> list[dict[str, object]]:
            # The exclusion leaves eight networks, and the gap between ports
            # keeps each port a segment of its own.
            return [
                {
                    "cidr": "192.0.2.0/24",
                    "except": ["192.0.2.1/32"],
                    "protocol": "tcp",
                    "port": 2 * index + 1,
                }
                for index in range(count)
            ]

        accepted = self.compile({"allow": rules(32)})
        self.assertEqual(len(accepted.allow), 256)

        with self.assertRaisesRegex(ScriptError, "at most 256"):
            self.compile({"allow": rules(33)})

        # Adjacent ports merge into one range for each network.
        merged = self.compile(
            {
                "allow": [
                    {**rule, "port": index + 1} for index, rule in enumerate(rules(33))
                ]
            }
        )
        self.assertEqual(len(merged.allow), 8)
        self.assertTrue(all(rule.endswith(":tcp:1-33") for rule in merged.allow))

    def test_delays_fragment_conversion_until_after_covering_union(self):
        exclusions = [
            str(ipaddress.IPv4Address(int(ipaddress.IPv4Address("192.0.0.1")) + 2 * i))
            for i in range(4096)
        ]
        fragmented = {
            "cidr": "192.0.0.0/16",
            "except": exclusions,
            "protocol": "tcp",
            "port": 80,
            "endPort": 81,
        }
        covering = {
            "cidr": "192.0.0.0/16",
            "protocol": "tcp",
            "port": 80,
            "endPort": 81,
        }

        original_summarize = ipaddress.summarize_address_range
        with mock.patch.object(
            ipaddress,
            "summarize_address_range",
            wraps=original_summarize,
        ) as summarize:
            for rules in ([fragmented, covering], [covering, fragmented]):
                with self.subTest(order=rules):
                    compiled = self.compile({"allow": rules})
                    self.assertEqual(compiled.allow, ("192.0.0.0/16:tcp:80-81",))

        self.assertEqual(summarize.call_count, 2)

    def test_validates_redundant_rules_before_canonicalization(self):
        with self.assertRaisesRegex(ScriptError, "unknown field"):
            self.compile(
                {
                    "allow": [
                        {"cidr": "192.0.2.0/24"},
                        {"cidr": "192.0.2.1", "unknown": True},
                    ]
                }
            )

    def test_address_only_rule_covers_large_protocol_range(self):
        for rules in (
            [
                {
                    "cidr": "192.0.2.0/24",
                    "protocol": "tcp",
                    "port": 1,
                    "endPort": 65535,
                },
                {"cidr": "192.0.2.0/24"},
            ],
            [
                {"cidr": "192.0.2.0/24"},
                {
                    "cidr": "192.0.2.0/24",
                    "protocol": "tcp",
                    "port": 1,
                    "endPort": 65535,
                },
            ],
        ):
            with self.subTest(rules=rules):
                compiled = self.compile({"allow": rules})
                self.assertEqual(compiled.allow, ("192.0.2.0/24",))

    def test_address_only_rule_prunes_redundant_port_events_before_sweep(self):
        covering = {"cidr": "0.0.0.0/0"}
        first_address = int(ipaddress.IPv4Address("192.0.0.0"))
        protocol_rules = [
            {
                "cidr": f"{ipaddress.IPv4Address(first_address + start)}/32",
                "protocol": "tcp",
                "port": start,
                "endPort": 2001 - start,
            }
            for start in range(1, 1001)
        ]
        original_merge = egress_policy._merge_intervals

        for category in ("allow", "deny"):
            for rules in (
                [covering, *protocol_rules],
                [*protocol_rules, covering],
            ):
                merged_interval_counts: list[int] = []

                def record_merge(
                    intervals: Iterable[tuple[int, int]],
                    counts: list[int] = merged_interval_counts,
                ) -> tuple[tuple[int, int], ...]:
                    materialized = tuple(intervals)
                    counts.append(len(materialized))
                    return original_merge(materialized)

                with (
                    self.subTest(
                        category=category,
                        covering_first=rules[0] is covering,
                    ),
                    mock.patch.object(
                        egress_policy,
                        "_merge_intervals",
                        side_effect=record_merge,
                    ),
                ):
                    compiled = self.compile({category: rules})
                    self.assertEqual(
                        getattr(compiled, category),
                        ("0.0.0.0/0",),
                    )
                    self.assertEqual(sum(merged_interval_counts), 1)

    def test_partial_address_only_coverage_keeps_protocol_union_compact(self):
        address_only_rules = [
            {"cidr": f"192.0.0.{2 * index + 1}/32"} for index in range(128)
        ]
        protocol_rule = {
            "cidr": "192.0.0.0/16",
            "protocol": "tcp",
            "port": 443,
        }

        for category in ("allow", "deny"):
            compiled_orders: list[tuple[str, ...]] = []
            for rules in (
                [*address_only_rules, protocol_rule],
                [protocol_rule, *address_only_rules],
            ):
                with self.subTest(
                    category=category,
                    protocol_first=rules[0] is protocol_rule,
                ):
                    compiled = self.compile({category: rules})
                    compiled_rules: tuple[str, ...] = getattr(compiled, category)
                    self.assertEqual(len(compiled_rules), 129)
                    self.assertIn("192.0.0.0/16:tcp:443", compiled_rules)
                    self.assertEqual(
                        {
                            rule
                            for rule in compiled_rules
                            if not rule.endswith(":tcp:443")
                        },
                        {rule["cidr"] for rule in address_only_rules},
                    )
                    compiled_orders.append(compiled_rules)
            self.assertEqual(*compiled_orders)

    def test_partial_address_only_canonicalization_does_not_bridge_gaps(self):
        compiled = self.compile(
            {
                "allow": [
                    {"cidr": "192.0.2.1/32"},
                    {"cidr": "192.0.2.0/32", "protocol": "tcp", "port": 443},
                    {"cidr": "192.0.2.3/32", "protocol": "tcp", "port": 443},
                ]
            }
        )

        self.assertEqual(
            compiled.allow,
            (
                "192.0.2.0/31:tcp:443",
                "192.0.2.1/32",
                "192.0.2.3/32:tcp:443",
            ),
        )


if __name__ == "__main__":
    unittest.main()
