#!/usr/bin/env python3
"""Generate the egress policy parity fixture of nvx-rs from the Python compiler.

`nvx::host::network::policy` ports `scripts/nvx_tools/egress_policy.py`: it must
accept the same policy files, compile them to the same native rules, and fail
with the same messages. This script runs the Python compiler over every policy
that `scripts/test_egress_policy.py` compiles, hand-written edge cases, and a
seeded random corpus, and records each input with its outcome in
`nvx-rs/tests/fixtures/egress-policy.json`, which the Rust tests replay without
Python. It also records how `pathlib` prints policy paths.

Python 3.13 or later is required: earlier releases print IPv4-mapped IPv6
addresses such as `::ffff:192.0.2.0` in hexadecimal. The output depends only on
the seed and the corpus size, so `--check` verifies that the fixture is current:

    python3 scripts/generate_egress_policy_fixtures.py [--check]
"""

from __future__ import annotations

import argparse
import inspect
import io
import ipaddress
import json
import random
import re
import sys
import tempfile
import unittest
from collections.abc import Iterable, Iterator
from pathlib import Path, PurePosixPath, PureWindowsPath
from typing import AnyStr

sys.path.insert(0, str(Path(__file__).parent))
import test_egress_policy  # noqa: E402
from nvx_tools.common import ScriptError  # noqa: E402
from nvx_tools.egress_policy import (  # noqa: E402
    MAX_POLICY_FILE_SIZE,
    MAX_RULES_PER_ACTION,
    CompiledEgressPolicy,
    compile_policy_file,
)

DESCRIPTION = "Generate the egress policy parity fixture of nvx-rs."
FIXTURE = (
    Path(__file__).resolve().parent.parent
    / "nvx-rs"
    / "tests"
    / "fixtures"
    / "egress-policy.json"
)
SEED = 359
RANDOM_CASES = 200
# Messages name the policy file; the fixture writes its path as this placeholder.
PATH_PLACEHOLDER = "{path}"
# Runs of one character or byte at least this long are recorded as a repetition.
MIN_REPEAT = 256
# Tests whose policies are too large to record. Their tests in
# `scripts/test_egress_policy.py`, and the Rust ports of those tests, pin the
# complete outcome of each.
LARGE_VECTORS = frozenset(
    (
        "test_address_only_rule_prunes_redundant_port_events_before_sweep",
        "test_delays_fragment_conversion_until_after_covering_union",
    )
)

Entry = dict[str, object]
Rule = dict[str, object]
Policy = dict[str, list[Rule]]
Network = ipaddress.IPv4Network | ipaddress.IPv6Network


def printable(text: str) -> str:
    """Return `text` as Python prints it to UTF-8, with surrogates as `\\udXXX`."""
    return text.encode("utf-8", "backslashreplace").decode("utf-8")


def segments(data: bytes) -> list[Entry]:
    """Describe `data` as text, or else as hexadecimal, with long runs repeated."""
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        return [
            {"hex": run.hex(), **({"repeat": count} if count > 1 else {})}
            for run, count in runs(data)
        ]
    return [
        {"text": run, **({"repeat": count} if count > 1 else {})}
        for run, count in runs(text)
    ]


def runs(items: AnyStr) -> list[tuple[AnyStr, int]]:
    """Split a string or bytes into literal stretches and long runs of one item."""
    result: list[tuple[AnyStr, int]] = []
    start = 0
    index = 0
    while index < len(items):
        end = index
        while end < len(items) and items[end] == items[index]:
            end += 1
        if end - index >= MIN_REPEAT:
            if start < index:
                result.append((items[start:index], 1))
            result.append((items[index : index + 1], end - index))
            start = end
        index = end
    if start < len(items) or not result:
        result.append((items[start:], 1))
    return result


def outcome(path: Path) -> Entry:
    """Compile the policy file at `path` and describe the result."""
    try:
        compiled = compile_policy_file(path)
    except ScriptError as error:
        message = str(error).replace(str(path), PATH_PLACEHOLDER)
        return {"error": printable(message)}
    return {"allow": list(compiled.allow), "deny": list(compiled.deny)}


def test_vectors() -> list[tuple[str, bytes]]:
    """Return every policy file that `scripts/test_egress_policy.py` compiles."""
    captured: list[tuple[str, bytes]] = []
    original = test_egress_policy.compile_policy_file

    def capture(path: Path) -> CompiledEgressPolicy:
        test = next(
            frame.function
            for frame in inspect.stack()
            if frame.function.startswith("test_")
        )
        captured.append((test, path.read_bytes()))
        return original(path)

    test_egress_policy.compile_policy_file = capture
    try:
        suite = unittest.defaultTestLoader.loadTestsFromModule(test_egress_policy)
        result = unittest.TextTestRunner(stream=io.StringIO()).run(suite)
    finally:
        test_egress_policy.compile_policy_file = original
    if not result.wasSuccessful():
        raise SystemExit("scripts/test_egress_policy.py failed")
    seen: set[bytes] = set()
    counts: dict[str, int] = {}
    vectors: list[tuple[str, bytes]] = []
    for test, data in captured:
        if data not in seen and test not in LARGE_VECTORS:
            seen.add(data)
            counts[test] = counts.get(test, 0) + 1
            vectors.append((f"{test} #{counts[test]}", data))
    return vectors


def policy(value: object) -> bytes:
    return json.dumps(value).encode()


def rules(category: str, items: Iterable[object]) -> bytes:
    return policy({category: list(items)})


def json_cases() -> Iterator[tuple[str, bytes]]:
    """Yield cases of the JSON reader: syntax, numbers, hooks, and strings."""
    cidr = "192.0.2.0/24"
    port = f'{{"allow": [{{"cidr": "{cidr}", "protocol": "tcp", "port": %s}}]}}'
    for name, text in (
        ("empty file", ""),
        ("whitespace only", " \t\r\n"),
        ("null", "null"),
        ("array root", "[]"),
        ("number root", "1"),
        ("string root", '"allow"'),
        ("empty object", "{}"),
        ("surrounding whitespace", ' \t\n\r{"allow": []}\r\n '),
        ("form feed after", '{"allow": []}\f'),
        ("vertical tab before", '\v{"allow": []}'),
        ("no-break space after", '{"allow": []}\u00a0'),
        ("trailing comma in object", '{"allow": [],}'),
        ("trailing comma in array", f'{{"allow": [{{"cidr": "{cidr}"}},]}}'),
        ("comment", '{"allow": []} // comment'),
        ("single quotes", "{'allow': []}"),
        ("unquoted name", "{allow: []}"),
        ("missing colon", '{"allow" []}'),
        ("missing comma", '{"allow": [] "deny": []}'),
        ("unclosed object", '{"allow": []'),
        ("unclosed string", '{"allow'),
        ("extra value", '{"allow": []} {}'),
        ("two roots", "{}{}"),
        ("constant prefix", '{"allow": nullx}'),
        ("raw tab in string", f'{{"allow": [{{"cidr": "{cidr}\t"}}]}}'),
        ("escaped tab in string", f'{{"allow": [{{"cidr": "{cidr}\\t"}}]}}'),
        ("raw newline in name", '{"al\nlow": []}'),
        ("invalid escape", '{"allow": [{"cidr": "\\x41"}]}'),
        ("short unicode escape", '{"allow": [{"cidr": "\\u41"}]}'),
        ("solidus escape", '{"allow": [{"cidr": "192.0.2.0\\/24"}]}'),
        ("escaped names", '{"\\u0061llow": [{"\\u0063idr": "192.0.2.7"}]}'),
        ("escaped duplicate", '{"allow": [], "\\u0061llow": []}'),
        ("deep arrays in a rule list", '{"allow": ' + "[" * 100 + "]" * 100 + "}"),
        ("deep arrays", "[" * 500 + "]" * 500),
        ("deep objects", '{"allow": [' + '{"a": ' * 200 + "1" + "}" * 200 + "]}"),
    ):
        yield f"json: {name}", text.encode()
    for value in (
        "01",
        "1.",
        ".5",
        "+80",
        "0x50",
        "8e",
        "8e+",
        "nan",
        "-NaN",
        "Inf",
        "NaN",
        "Infinity",
        "-Infinity",
        "1e400",
        "-1e400",
        "80.0",
        "8e1",
        "1E2",
        "1e-400",
        "-0",
        "0",
        "true",
        "false",
        "null",
        '"80"',
        "[80]",
        "{}",
        "65535",
        "65536",
        "-1",
        "9" * 64,
        "-" + "9" * 64,
        "9" * 65,
        "-" + "9" * 65,
        "1" + "0" * 4300,
    ):
        yield f"json: port {value[:16]}", (port % value).encode()
        yield (
            f"json: endPort {value[:16]}",
            f'{{"deny": [{{"ports": [{{"port": 1, "endPort": {value}}}]}}]}}'.encode(),
        )
    long_digits = "9" * 65
    for name, text in (
        ("duplicate root field", '{"deny": [{"cidr": "0.0.0.0/0"}], "deny": []}'),
        ("duplicate rule field", '{"allow": [{"cidr": "10.0.0.0/8", "cidr": "::/0"}]}'),
        ("duplicate port field", '{"allow": [{"ports": [{"port": 1, "port": 2}]}]}'),
        ("duplicate peer field", '{"allow": [{"to": [{"except": [], "except": []}]}]}'),
        ("first duplicate in order", '{"b": 1, "a": 2, "a": 3, "b": 4}'),
        ("duplicate before long integer", f'{{"a": 1, "a": 2, "b": {long_digits}}}'),
        ("inner duplicate first", f'{{"a": {{"x": 1, "x": 2}}, "b": {long_digits}}}'),
        ("long integer first", f'{{"b": {long_digits}, "a": {{"x": 1, "x": 2}}}}'),
        ("duplicate before syntax error", '{"a": 1, "a": 2,}'),
        ("duplicate before extra data", '{"a": 1, "a": 2} x'),
        ("long integer before syntax error", f"[{long_digits}x]"),
        ("long integer in unknown field", f'{{"unknown": [{long_digits}]}}'),
        ("duplicate in unknown field", '{"unknown": {"x": 1, "x": 2}}'),
        ("duplicate surrogate names", '{"\\ud800": 1, "\\ud800": 2}'),
        ("distinct surrogate names", '{"\\ud800": 1, "\\udc00": 2}'),
        ("surrogate root name", '{"\\ud800": []}'),
        ("surrogate rule name", '{"allow": [{"\\udfff": 1}]}'),
        ("surrogate cidr", '{"allow": [{"cidr": "\\ud800"}]}'),
        ("surrogate scope", '{"allow": [{"cidr": "fe80::1%\\ud800/64"}]}'),
        (
            "surrogate protocol",
            f'{{"allow": [{{"cidr": "{cidr}", "protocol": "\\udc00"}}]}}',
        ),
        ("paired surrogates", '{"allow": [{"cidr": "fe80::1%\\ud83d\\ude00/64"}]}'),
        ("reversed surrogates", '{"allow": [{"cidr": "fe80::1%\\ude00\\ud83d/64"}]}'),
        ("high surrogate before letter", '{"allow": [{"cidr": "\\ud800A"}]}'),
        ("high surrogate before bad escape", '{"allow": [{"cidr": "\\ud800\\u12g4"}]}'),
        ("surrogate exception", '{"allow": [{"cidr": "::/0", "except": ["\\udbff"]}]}'),
    ):
        yield f"json: {name}", text.encode()
    for name, data in (
        ("byte-order mark", b'\xef\xbb\xbf{"allow": []}'),
        ("invalid byte", b'{"allow": [\xff]}'),
        ("invalid byte in string", b'{"allow": [{"cidr": "192.0.2.0/24\xfe"}]}'),
        ("overlong encoding", b'{"allow": [{"cidr": "\xc0\xaf"}]}'),
        ("encoded surrogate", b'{"allow": [{"cidr": "\xed\xa0\x80"}]}'),
        ("truncated sequence", b'{"\xe2\x82": []}'),
        ("utf-16", '{"allow": []}'.encode("utf-16")),
        ("non-ascii text", '{"allow": [{"cidr": "192.0.2.0/24\u00e9"}]}'.encode()),
    ):
        yield f"utf-8: {name}", data
    # Unknown fields are named in code-point order.
    for name, fields in (
        ("ascii", ("b", "a", "B")),
        ("non-ascii", ("\u00e9", "z", "\u00ff")),
        ("astral and surrogate", ("\\ud83d\\ude00", "\\ud800", "\\uffff")),
    ):
        members = ", ".join(f'"{field}": 1' for field in fields)
        yield f"unknown fields: {name}", ("{" + members + "}").encode()
        yield (
            f"unknown rule fields: {name}",
            ('{"allow": [{"cidr": "192.0.2.0/24", ' + members + "}]}").encode(),
        )


def format_items(cidr: str) -> Iterator[tuple[str, list[object]]]:
    """Yield rule lists that reach each message of the format, in check order."""
    tcp: Rule = {"cidr": cidr, "protocol": "tcp"}
    items: tuple[tuple[str, object], ...] = (
        ("rule string", cidr),
        ("rule number", 7),
        ("rule null", None),
        ("rule array", [{"cidr": cidr}]),
        ("empty rule", Rule()),
        ("mixed forms", {"to": [{"cidr": cidr}], "cidr": cidr}),
        ("mixed ports", {"ports": list[Rule](), "port": 80}),
        ("legacy unknown field", {"cidr": cidr, "unknown": 1}),
        ("legacy unknown before missing cidr", {"port": 80, "unknown": 1}),
        ("legacy missing cidr", {"protocol": "tcp", "port": 80}),
        ("legacy except only", {"except": list[str]()}),
        ("legacy cidr number", {"cidr": 7}),
        ("legacy cidr null", {"cidr": None}),
        ("legacy cidr invalid", {"cidr": "192.0.2.0/33"}),
        ("legacy cidr scoped", {"cidr": "fe80::1%eth0/64"}),
        ("legacy except string", {"cidr": cidr, "except": "192.0.2.1"}),
        ("legacy except null", {"cidr": cidr, "except": None}),
        ("legacy except number", {"cidr": cidr, "except": [7]}),
        ("legacy except outside", {"cidr": cidr, "except": ["198.51.100.0/24"]}),
        ("legacy except family", {"cidr": cidr, "except": ["2001:db8::/32"]}),
        ("legacy except wider", {"cidr": cidr, "except": ["192.0.0.0/16"]}),
        ("legacy except whole", {"cidr": cidr, "except": [cidr]}),
        ("legacy except invalid", {"cidr": cidr, "except": ["192.0.2.0/24", "x"]}),
        ("legacy endPort without port", {"cidr": cidr, "endPort": 80}),
        ("legacy endPort with protocol", {**tcp, "endPort": 80}),
        ("legacy port without protocol", {"cidr": cidr, "port": 80}),
        ("legacy protocol upper", {"cidr": cidr, "protocol": "TCP"}),
        ("legacy protocol unknown", {"cidr": cidr, "protocol": "sctp"}),
        ("legacy protocol empty", {"cidr": cidr, "protocol": ""}),
        ("legacy protocol null", {"cidr": cidr, "protocol": None}),
        ("legacy protocol number", {"cidr": cidr, "protocol": 6}),
        ("legacy icmp port", {"cidr": cidr, "protocol": "icmp", "port": 8}),
        ("legacy icmp endPort", {"cidr": cidr, "protocol": "icmp", "endPort": 8}),
        ("legacy port zero", {**tcp, "port": 0}),
        ("legacy port bool", {**tcp, "port": True}),
        ("legacy port string", {**tcp, "port": "80"}),
        ("legacy endPort zero", {**tcp, "port": 1, "endPort": 0}),
        ("legacy endPort bool", {**tcp, "port": 1, "endPort": False}),
        ("legacy endPort below", {**tcp, "port": 10, "endPort": 5}),
        (
            "legacy any range",
            {"cidr": cidr, "protocol": "any", "port": 1, "endPort": 9},
        ),
        ("legacy any", {"cidr": cidr, "protocol": "any"}),
        ("legacy icmp", {"cidr": cidr, "protocol": "icmp"}),
        ("mxc unknown field", {"to": [{"cidr": cidr}], "unknown": 1}),
        ("mxc unknown only", {"unknown": 1}),
        ("mxc to object", {"to": {"cidr": cidr}}),
        ("mxc to null", {"to": None}),
        ("mxc to empty", {"to": list[Rule]()}),
        ("mxc peer string", {"to": [cidr]}),
        ("mxc peer unknown", {"to": [{"cidr": cidr, "unknown": 1}]}),
        ("mxc peer empty", {"to": [Rule()]}),
        ("mxc peer except only", {"to": [{"except": list[str]()}]}),
        ("mxc peer cidr number", {"to": [{"cidr": 7}]}),
        ("mxc peer second invalid", {"to": [{"cidr": cidr}, {"cidr": "x"}]}),
        ("mxc peer except null", {"to": [{"cidr": cidr, "except": None}]}),
        ("mxc peer except outside", {"to": [{"cidr": cidr, "except": ["10.0.0.0/8"]}]}),
        ("mxc ports object", {"ports": {"port": 80}}),
        ("mxc ports empty", {"ports": list[Rule]()}),
        ("mxc port string", {"ports": ["tcp"]}),
        ("mxc port unknown", {"ports": [{"protocol": "tcp", "unknown": 1}]}),
        ("mxc port empty", {"ports": [Rule()]}),
        ("mxc port protocol upper", {"ports": [{"protocol": "Udp"}]}),
        ("mxc port protocol null", {"ports": [{"protocol": None}]}),
        ("mxc port endPort only", {"ports": [{"endPort": 80}]}),
        ("mxc port icmp endPort", {"ports": [{"protocol": "icmp", "endPort": 80}]}),
        ("mxc port icmp port", {"ports": [{"protocol": "icmp", "port": 8}]}),
        ("mxc port null", {"ports": [{"port": None}]}),
        ("mxc port float", {"ports": [{"port": 80.0}]}),
        ("mxc port large", {"ports": [{"port": 70000}]}),
        ("mxc endPort large", {"ports": [{"port": 80, "endPort": 70000}]}),
        (
            "mxc endPort below",
            {"ports": [{"protocol": "udp", "port": 9, "endPort": 8}]},
        ),
        ("mxc second port invalid", {"ports": [{"port": 1}, {"port": 0}]}),
        ("mxc to before ports", {"to": [{"cidr": "x"}], "ports": [{"port": 0}]}),
        ("mxc every destination", {"ports": [{"protocol": "udp", "port": 53}]}),
        ("mxc every port", {"to": [{"cidr": cidr}]}),
    )
    for name, item in items:
        yield name, [item]
    yield "second rule invalid", [{"cidr": cidr}, {"cidr": cidr, "protocol": "TCP"}]


CIDR_FORMS = (
    "192.0.2.7",
    "192.0.2.7/32",
    "192.0.2.7/24",
    "10.0.0.5/255.255.255.0",
    "10.0.0.5/0.0.0.255",
    "10.0.0.5/255.255.255.255",
    "10.0.0.5/0.0.0.0",
    "10.0.0.5/0",
    "10.0.0.5/08",
    "10.0.0.5/0008",
    "10.0.0.5/" + "0" * 4299 + "8",
    "10.0.0.5/" + "0" * 4300 + "8",
    "10.0.0.5/255.0.255.0",
    "10.0.0.5/255.255.255.000",
    "10.0.0.5/+8",
    "10.0.0.5/-8",
    "10.0.0.5/8.0",
    "10.0.0.5/0x8",
    "10.0.0.5/",
    "10.0.0.5//8",
    "10.0.0.5/8/8",
    "10.0.0.5/33",
    " 10.0.0.5",
    "10.0.0.5 ",
    "010.0.0.5",
    "10.0.0.05",
    "256.0.0.0",
    "10.0.0",
    "10.0.0.0.5",
    "10.0.0.\u0665",
    "",
    "/8",
    "::",
    "::/0",
    "::/000128",
    "::/129",
    "2001:DB8::/32",
    "2001:0db8:0000:0000::/32",
    "2001:db8::7/64",
    "2001:db8:0:0:1:0:0:1",
    "1:0:0:2:0:0:0:3",
    "fe80::1",
    "fe80::1%eth0",
    "fe80::1%eth0/64",
    "fe80::%1/10",
    "fe80::1%",
    "fe80::1%a%b",
    "fe80::1%eth0/129",
    "::ffff:192.0.2.1",
    "::ffff:192.0.2.1/120",
    "::ffff:c000:201/120",
    "::FFFF:192.0.2.1/96",
    "::ffff:0:0/95",
    "::192.0.2.1",
    "::c000:200/120",
    "64:ff9b::192.0.2.1/120",
    "1:2:3:4:5:6:1.2.3.4",
    "1:2:3:4:5::1.2.3.4",
    "1:2:3:4:5:6:7:1.2.3.4",
    "1:2:3:4:5:6::1.2.3.4",
    "::1.2.3",
    "::ffff:1.2.3.04",
    "1:2:3:4:5:6:7:8",
    "1:2:3:4:5:6:7::",
    "::2:3:4:5:6:7:8",
    "1:2:3:4:5:6:7:8:9",
    "::1:2:3:4:5:6:7:8",
    "1::2::3",
    ":1::2",
    "1::2:",
    ":::",
    "00000::",
    "0000::",
    "[::1]",
    "::1/0x8",
    "2001:db8::g",
    "2001:db8::\uff11",
)

EXCEPTION_FORMS = (
    "10.0.0.0/8",
    "10.1.2.3/16",
    "10.255.255.255",
    "10.0.0.0/255.255.0.0",
    "10.0.0.0/7",
    "11.0.0.0/8",
    "0.0.0.0/0",
    "::/96",
    "::ffff:10.0.0.0/104",
    "fe80::1%eth0",
    "10.0.0.0/+16",
)


def format_cases() -> Iterator[tuple[str, bytes]]:
    """Yield cases of the policy format: every message, networks, and limits."""
    for category in ("allow", "deny"):
        for name, items in format_items("192.0.2.0/24"):
            yield f"{category}: {name}", rules(category, items)
    hosts: list[Rule] = [{"cidr": f"10.0.{i}.1"} for i in range(300)]
    policies: tuple[tuple[str, object], ...] = (
        ("allow object", {"allow": {}}),
        ("allow null", {"allow": None}),
        ("deny string", {"deny": "192.0.2.0/24"}),
        ("deny number", {"deny": 1}),
        ("unknown root field", {"allow": [], "denied": []}),
        ("allow before deny errors", {"allow": [{"cidr": 7}], "deny": [{"cidr": 8}]}),
        ("later rule error before lowering", {"allow": [*hosts, {"cidr": 7}]}),
        ("allow limit before deny errors", {"allow": hosts[:257], "deny": [7]}),
    )
    for name, value in policies:
        yield f"policy: {name}", policy(value)
    for value in CIDR_FORMS:
        yield f"cidr: {value[:40]}", rules("allow", [{"cidr": value}])
    for value in EXCEPTION_FORMS:
        peer = {"cidr": "10.0.0.0/8", "except": [value]}
        yield f"exception: {value}", rules("deny", [{"to": [peer]}])
    limit = MAX_RULES_PER_ACTION
    many: list[Rule] = [
        {"cidr": f"10.{i // 256}.{i % 256}.1"} for i in range(limit + 1)
    ]
    yield "limit: address-only at the limit", rules("allow", many[:limit])
    yield "limit: address-only over the limit", rules("deny", many)
    for protocol in ("tcp", "udp", "icmp"):
        wide: list[Rule] = [
            {"cidr": "192.0.2.0/24", "protocol": protocol},
            {"cidr": "198.51.100.0/24", "protocol": protocol},
        ]
        yield (
            f"limit: {protocol} for every port over the limit",
            rules("allow", [*many[: limit - 1], *wide]),
        )
    for count in (limit, limit + 1):
        selectors = [{"port": 2 * i + 1, "protocol": "udp"} for i in range(count)]
        yield (
            f"limit: {count} separate ports",
            rules("allow", [{"to": [{"cidr": "192.0.2.1"}], "ports": selectors}]),
        )
    for ipv4, ipv6 in ((128, 128), (129, 128), (128, 129), (200, 56), (0, 257)):
        yield (
            f"limit: {ipv4} IPv4 and {ipv6} IPv6 ports",
            rules(
                "deny",
                (
                    {
                        "to": [{"cidr": peer}],
                        "ports": [{"port": 2 * i + 1} for i in range(n)],
                    }
                    for peer, n in (("192.0.2.1", ipv4), ("2001:db8::1", ipv6))
                    if n
                ),
            ),
        )
    yield (
        "limit: ranges of any protocol over the limit",
        rules(
            "allow",
            (
                {"cidr": "192.0.2.1", "protocol": "any", "port": p, "endPort": p + 1}
                for p in range(1, 3 * 129, 3)
            ),
        ),
    )
    yield (
        "limit: fragmented exclusions",
        rules(
            "allow",
            [{"cidr": "0.0.0.0/0", "except": [f"{2 * i}.0.0.1" for i in range(64)]}],
        ),
    )
    yield (
        "limit: fragments covered by a wider rule",
        rules(
            "allow",
            [
                {"cidr": "0.0.0.0/0", "except": [f"{i}.0.0.1" for i in range(256)]},
                {"cidr": "0.0.0.0/0"},
            ],
        ),
    )
    # The size limit applies to the bytes read.
    valid = b'{"allow": [{"cidr": "192.0.2.0/24"}]}'
    padding = MAX_POLICY_FILE_SIZE - len(valid)
    yield "size: exactly the limit", valid + b" " * padding
    yield "size: one byte over the limit", valid + b" " * (padding + 1)
    yield "size: invalid and over the limit", b"\xff" * (MAX_POLICY_FILE_SIZE + 1)
    yield "size: spaces up to the limit", b" " * MAX_POLICY_FILE_SIZE


class Corpus:
    """A seeded generator of random policies that exercise the lowering."""

    def __init__(self, seed: int) -> None:
        self.random = random.Random(seed)

    def chance(self, probability: float) -> bool:
        return self.random.random() < probability

    def network(self) -> Network:
        pick = self.random.random()
        if pick < 0.45:
            # A dense IPv4 neighborhood, so that rules overlap and merge.
            address = (10 << 24) + self.random.randrange(1024)
            return ipaddress.IPv4Network((address, self.random.randint(22, 32)), False)
        if pick < 0.75:
            address = (0x20010DB8 << 96) + self.random.randrange(1024)
            prefix = self.random.randint(118, 128)
            return ipaddress.IPv6Network((address, prefix), False)
        if pick < 0.88:
            prefix = self.random.choice((0, 1, 2, 4, 8, 12, 16, 24, 31, 32))
            address = self.random.getrandbits(32)
            return ipaddress.IPv4Network((address, prefix), False)
        prefix = self.random.choice((0, 1, 3, 32, 64, 96, 104, 120, 127, 128))
        address = self.random.getrandbits(128)
        if self.chance(0.3):
            # An IPv4-mapped address.
            address = (0xFFFF << 32) | (address & 0xFFFFFFFF)
        return ipaddress.IPv6Network((address, prefix), False)

    def spell(self, network: Network) -> str:
        """Write a network as a policy may: with host bits, masks, or other forms."""
        address = int(network.network_address)
        if network.num_addresses > 1 and self.chance(0.3):
            address += self.random.randrange(network.num_addresses)
        prefix = str(network.prefixlen)
        if self.chance(0.1):
            prefix = "0" * self.random.randint(1, 3) + prefix
        if isinstance(network, ipaddress.IPv4Network):
            text = str(ipaddress.IPv4Address(address))
            if self.chance(0.1):
                prefix = str(network.netmask)
            elif self.chance(0.05):
                prefix = str(network.hostmask)
            if network.prefixlen == 32 and self.chance(0.5):
                return text
            return f"{text}/{prefix}"
        ipv6 = ipaddress.IPv6Address(address)
        pick = self.random.random()
        if pick < 0.15:
            text = ipv6.exploded
        elif pick < 0.25:
            text = str(ipv6).upper()
        elif pick < 0.3:
            embedded = ipaddress.IPv4Address(address & 0xFFFFFFFF)
            text = f"{ipv6.exploded[:30]}{embedded}"
        else:
            text = str(ipv6)
        if network.prefixlen == 128 and self.chance(0.5):
            return text
        return f"{text}/{prefix}"

    def exception(self, parent: Network) -> str:
        if self.chance(0.01):
            return self.random.choice(("x", "10.0.0.0/33", "fe80::1%eth0", "::/0"))
        if self.chance(0.01):
            return self.spell(self.network())
        maximum = parent.max_prefixlen
        prefix = self.random.randint(
            parent.prefixlen, min(maximum, parent.prefixlen + 8)
        )
        size = 1 << (maximum - prefix)
        offset = self.random.randrange(parent.num_addresses // size) * size
        address = int(parent.network_address) + offset
        if isinstance(parent, ipaddress.IPv4Network):
            return self.spell(ipaddress.IPv4Network((address, prefix)))
        return self.spell(ipaddress.IPv6Network((address, prefix)))

    def destination(self, rule: Rule) -> None:
        network = self.network()
        rule["cidr"] = self.spell(network)
        if self.chance(0.3):
            count = self.random.randint(0, 3)
            rule["except"] = [self.exception(network) for _ in range(count)]

    def selector(self, rule: Rule, *, mxc: bool) -> None:
        pick = self.random.random()
        protocol = (
            "tcp"
            if pick < 0.35
            else "udp"
            if pick < 0.6
            else "any"
            if pick < 0.8
            else "icmp"
            if pick < 0.9
            else None
        )
        if protocol is None and not mxc:
            return
        if protocol is not None:
            rule["protocol"] = protocol
        if self.chance(0.3) or (protocol == "icmp" and self.chance(0.98)):
            return
        start = self.random.choice(
            (
                self.random.randint(1, 12),
                self.random.randint(78, 82),
                self.random.randint(440, 445),
                self.random.randint(65530, 65535),
            )
        )
        rule["port"] = start
        if self.chance(0.5):
            if self.chance(0.1):
                rule["endPort"] = self.random.randint(start, 65535)
            else:
                rule["endPort"] = min(65535, start + self.random.randint(0, 6))

    def rule(self) -> Rule:
        rule: Rule = {}
        if self.chance(0.35):
            self.destination(rule)
            self.selector(rule, mxc=False)
            return rule
        if self.chance(0.85):
            peers: list[Rule] = []
            for _ in range(self.random.randint(1, 4)):
                peer: Rule = {}
                self.destination(peer)
                peers.append(peer)
            rule["to"] = peers
        if self.chance(0.75):
            selectors: list[Rule] = []
            for _ in range(self.random.randint(1, 4)):
                selector: Rule = {}
                self.selector(selector, mxc=True)
                selectors.append(selector)
            rule["ports"] = selectors
        return rule

    def policy(self) -> Policy:
        value: Policy = {}
        for category in ("allow", "deny"):
            if self.chance(0.85):
                count = self.random.choice((0, 1, 2, 3, 4, 6, 8))
                if self.chance(0.03):
                    count = self.random.randint(20, 60)
                value[category] = [self.rule() for _ in range(count)]
                if self.chance(0.03):
                    value[category].extend(self.crowd())
        return value

    def crowd(self) -> list[Rule]:
        """Return many rules on scattered ports, which approach the rule limit."""
        crowd: list[Rule] = []
        for _ in range(self.random.randint(80, 300)):
            if self.chance(0.7):
                address = (10 << 24) + self.random.randrange(1024)
                cidr = str(
                    ipaddress.IPv4Network(
                        (address, 32 - self.random.randint(0, 2)), False
                    )
                )
            else:
                address = (0x20010DB8 << 96) + self.random.randrange(1024)
                cidr = str(ipaddress.IPv6Network((address, 128), False))
            port = 2 * self.random.randrange(200) + 1
            rule: Rule = {
                "cidr": cidr,
                "protocol": self.random.choice(("tcp", "udp", "any")),
                "port": port,
            }
            if self.chance(0.3):
                rule["endPort"] = port + self.random.randint(0, 4)
            crowd.append(rule)
        return crowd

    def mutate(self, value: Policy) -> object:
        """Break one part of a policy, as an invalid file would."""
        candidates = [rule for rules in value.values() for rule in rules]
        if not candidates:
            return self.random.choice(([value], {**value, "extra": []}, None))
        rule = self.random.choice(candidates)
        broken: tuple[tuple[str, object], ...] = (
            ("unknown", 1),
            ("cidr", "192.0.2.0/24"),
            ("to", []),
            ("ports", []),
            ("port", True),
            ("protocol", "TCP"),
            ("endPort", 0),
            ("port", 99999),
            ("to", "192.0.2.0/24"),
            ("ports", [{"protocol": "icmp", "port": 1}]),
            ("ports", [{"port": 9, "endPort": 8}]),
            ("to", [{"cidr": "192.0.2.0/24", "except": ["10.0.0.0/8"]}]),
        )
        index = self.random.randrange(len(broken) + 1)
        if index == len(broken):
            rule.pop("cidr", None)
        else:
            field, replacement = broken[index]
            rule[field] = replacement
        return value

    def text(self, value: object) -> bytes:
        """Write a policy as JSON with random formatting, and sometimes break it."""
        indent = self.random.choice((None, None, None, 0, 1, "\t"))
        separators = self.random.choice((None, (",", ":"), (", ", ": ")))
        text = json.dumps(
            value,
            indent=indent,
            separators=separators,
            ensure_ascii=self.chance(0.8),
        )
        if self.chance(0.04):
            replacement = self.random.choice(
                ("NaN", "Infinity", "1e400", "80.0", "9" * 65, "9" * 64, "true")
            )
            text = re.sub(
                r'("(?:port|endPort)"\s*:\s*)\d+', rf"\g<1>{replacement}", text, count=1
            )
        if self.chance(0.03):
            text = text.replace("{", '{"allow": [], ', 1)
        data = text.encode()
        if self.chance(0.03):
            data = data[: self.random.randrange(len(data) + 1)]
        return data

    def cases(self, count: int) -> Iterator[tuple[str, bytes]]:
        for index in range(count):
            value = self.policy()
            item = self.mutate(value) if self.chance(0.12) else value
            yield f"random #{index}", self.text(item)


PATHS = (
    "",
    ".",
    "./",
    "a",
    "a/",
    "a//b",
    "a/./b/.",
    "./a",
    "../a",
    "a/../b",
    "/",
    "//",
    "///",
    "////a",
    "//a/b",
    "///a/b",
    "a\\b",
    "c:",
    "c:a",
    "c:/a",
    "c:\\a\\",
    "C:/",
    "/a",
    "\\a",
    "\\\\server",
    "\\\\server\\share",
    "\\\\server\\share\\x",
    "//server/share/x/",
    "\\\\?\\c:\\x",
    "\\\\?\\UNC\\server\\share\\x",
    "\\\\?\\unc\\server\\share",
    "\\\\.\\device\\x",
    "\\\\\\share",
    "\\\\?.\\x\\y",
    "./c:foo",
    "c:foo/./bar",
    "1:x",
    "::x",
    "a:b:c",
    "\\\\server\\share\\",
    "//?/UNC/a",
    "//?/UNC/a/b",
    "//?/UNC/a/b/c",
    "\\\\?\\",
    "\\\\.\\",
    "\\\\?\\x",
    "a/.",
    ".\\.\\",
    "x\\.\\y",
    "\\\\a\\b\\c\\d\\e",
    " a ",
    "a /b",
    "\u00e9:x",
    "dir/\u00e9.json",
    "policy.json",
)


def build(random_cases: int) -> dict[str, list[Entry]]:
    """Return the fixture: compiled policies, unreadable files, and paths."""
    if str(ipaddress.IPv6Address("::ffff:192.0.2.1")) != "::ffff:192.0.2.1":
        raise SystemExit(
            "generating the fixture needs Python 3.13 or later, whose ipaddress "
            "module prints IPv4-mapped IPv6 addresses in dotted form"
        )
    named = [
        *((f"vector: {name}", data) for name, data in test_vectors()),
        *json_cases(),
        *format_cases(),
        *Corpus(SEED).cases(random_cases),
    ]
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        path = directory / "policy.json"
        cases: list[Entry] = []
        for name, data in named:
            path.write_bytes(data)
            cases.append(
                {"name": name, "input": segments(data), "expected": outcome(path)}
            )
        files: list[Entry] = [
            {"name": "missing", "expected": outcome(directory / "missing.json")},
            {"name": "directory", "expected": outcome(directory)},
        ]
    paths: list[Entry] = [
        {
            "path": value,
            "posix": str(PurePosixPath(value)),
            "windows": str(PureWindowsPath(value)),
        }
        for value in PATHS
    ]
    return {"cases": cases, "files": files, "paths": paths}


def dump(fixture: dict[str, list[Entry]]) -> str:
    """Write the fixture as ASCII JSON with one entry per line."""
    lines = [
        "{",
        '  "comment": "Generated by scripts/generate_egress_policy_fixtures.py; '
        'do not edit.",',
    ]
    sections = list(fixture.items())
    for position, (name, entries) in enumerate(sections):
        lines.append(f'  "{name}": [')
        for index, entry in enumerate(entries):
            comma = "," if index + 1 < len(entries) else ""
            lines.append(f"    {json.dumps(entry, separators=(',', ':'))}{comma}")
        lines.append("  ]," if position + 1 < len(sections) else "  ]")
    lines.append("}")
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=DESCRIPTION)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail if the fixture differs from what this script generates",
    )
    parser.add_argument(
        "--random-cases",
        type=int,
        default=RANDOM_CASES,
        help=f"number of random policies (default: {RANDOM_CASES})",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=FIXTURE,
        help="fixture to write or check (default: the nvx-rs fixture)",
    )
    args = parser.parse_args()
    output: Path = args.output
    text = dump(build(args.random_cases))
    if args.check:
        current = output.read_text(encoding="utf-8").replace("\r\n", "\n")
        if current != text:
            print(f"{output} is out of date; regenerate it", file=sys.stderr)
            return 1
        return 0
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(text, encoding="utf-8", newline="\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
