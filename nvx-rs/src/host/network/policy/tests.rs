//! Tests of the egress policy compiler: ports of the cases of the former
//! `scripts/test_egress_policy.py`, and, in submodules, tests whose expectations come from the
//! format that `doc/run.md` documents (microsoft/nvx#729): a reference [`model`] of policies, a
//! [`generator`] of policies, the [`properties`] of the rules that policies lower to, and tests of
//! the format's [`grammar`].

mod generator;
mod grammar;
mod model;
mod properties;

use std::path::Path;

use serde_json::{Value, json};

use super::*;
use crate::ErrorKind;
use crate::host::network::address::probe;

// Ports of the former `scripts/test_egress_policy.py`, one test for each of its tests, under the
// same names.

/// Compiles `value` as `EgressPolicyTests.compile` does, from its JSON text in a policy file, and
/// checks that compiling the value itself agrees.
fn compile(value: Value) -> Result<CompiledEgressPolicy> {
    let data = serde_json::to_vec(&value).unwrap();
    let compiled = compile_policy_bytes(&data, Path::new("policy.json"));
    let parsed = compile_policy(&value);
    match (&compiled, &parsed) {
        (Ok(compiled), Ok(parsed)) => assert_eq!(compiled, parsed, "{value}"),
        (Err(compiled), Err(parsed)) => {
            assert_eq!(compiled.message(), parsed.message(), "{value}");
        }
        _ => panic!("{value}: the file gives {compiled:?} and the value {parsed:?}"),
    }
    compiled
}

/// Compiles the text of a policy file.
fn compile_text(text: &str) -> Result<CompiledEgressPolicy> {
    compile_policy_bytes(text.as_bytes(), Path::new("policy.json"))
}

/// Returns the message of a compilation that must fail.
fn message(result: Result<CompiledEgressPolicy>) -> String {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    error.message().to_owned()
}

/// Returns the rules of `category`, `allow` or `deny`.
fn action<'a>(compiled: &'a CompiledEgressPolicy, category: &str) -> &'a [String] {
    match category {
        "allow" => &compiled.allow,
        "deny" => &compiled.deny,
        _ => panic!("unknown category {category}"),
    }
}

/// Returns `object` with the fields of `extra` added.
fn merged(object: &Value, extra: &Value) -> Value {
    let mut object = object.as_object().unwrap().clone();
    object.extend(extra.as_object().unwrap().clone());
    Value::Object(object)
}

#[test]
fn test_compiles_inclusive_tcp_and_udp_ranges() {
    let compiled = compile(json!({
        "allow": [
            {
                "to": [{"cidr": "192.0.2.7"}],
                "ports": [{"protocol": "tcp", "port": 8000, "endPort": 8002}],
            },
            {
                "to": [{"cidr": "198.51.100.0/24"}],
                "ports": [{"protocol": "udp", "port": 5000, "endPort": 5001}],
            },
        ]
    }))
    .unwrap();
    assert_eq!(
        compiled.allow,
        [
            "192.0.2.7/32:tcp:8000-8002",
            "198.51.100.0/24:udp:5000-5001"
        ]
    );
    assert!(compiled.deny.is_empty());
}

#[test]
fn test_lowers_each_range_to_one_native_range_rule() {
    let ranges = [
        (
            json!({"protocol": "tcp", "port": 8000, "endPort": 8010}),
            vec!["192.0.2.7/32:tcp:8000-8010"],
        ),
        (
            json!({"protocol": "udp", "port": 5000, "endPort": 5010}),
            vec!["192.0.2.7/32:udp:5000-5010"],
        ),
        (
            json!({"protocol": "tcp", "port": 1, "endPort": 65535}),
            vec!["192.0.2.7/32:tcp:1-65535"],
        ),
        (
            json!({"protocol": "any", "port": 1, "endPort": 65535}),
            vec!["192.0.2.7/32:tcp:1-65535", "192.0.2.7/32:udp:1-65535"],
        ),
        // A range of one port lowers to that port's rule.
        (
            json!({"protocol": "tcp", "port": 8000, "endPort": 8000}),
            vec!["192.0.2.7/32:tcp:8000"],
        ),
    ];
    for category in ["allow", "deny"] {
        for (selector, expected) in &ranges {
            for rule in [
                json!({"to": [{"cidr": "192.0.2.7"}], "ports": [selector]}),
                merged(&json!({"cidr": "192.0.2.7"}), selector),
            ] {
                let compiled = compile(json!({category: [rule]})).unwrap();
                assert_eq!(action(&compiled, category), expected, "{category} {rule}");
            }
        }
    }
}

#[test]
fn test_keeps_a_denied_port_inside_an_allowed_range() {
    let compiled = compile(json!({
        "allow": [{
            "to": [{"cidr": "192.0.2.0/24"}],
            "ports": [{"protocol": "tcp", "port": 8000, "endPort": 8010}],
        }],
        "deny": [{
            "to": [{"cidr": "192.0.2.0/24"}],
            "ports": [{"protocol": "tcp", "port": 8005}],
        }],
    }))
    .unwrap();
    // OpenVMM applies deny precedence, so each list keeps its own rule.
    assert_eq!(compiled.allow, ["192.0.2.0/24:tcp:8000-8010"]);
    assert_eq!(compiled.deny, ["192.0.2.0/24:tcp:8005"]);
}

#[test]
fn test_merges_adjacent_and_overlapping_ranges_of_one_network() {
    let ranges = |bounds: &[(u16, u16)]| -> Vec<Value> {
        bounds
            .iter()
            .map(|&(start, end)| {
                json!({"cidr": "192.0.2.7", "protocol": "tcp", "port": start, "endPort": end})
            })
            .collect()
    };
    let singles: Vec<(u16, u16)> = (8000..=8010).map(|port| (port, port)).collect();
    for (rules, expected) in [
        (
            ranges(&[(8000, 8004), (8005, 8010)]),
            vec!["192.0.2.7/32:tcp:8000-8010"],
        ),
        (
            ranges(&[(8000, 8008), (8003, 8010)]),
            vec!["192.0.2.7/32:tcp:8000-8010"],
        ),
        (
            [ranges(&[(8000, 8010)]), ranges(&singles)].concat(),
            vec!["192.0.2.7/32:tcp:8000-8010"],
        ),
        (
            ranges(&[(8000, 8004), (8006, 8010)]),
            vec!["192.0.2.7/32:tcp:8000-8004", "192.0.2.7/32:tcp:8006-8010"],
        ),
    ] {
        let compiled = compile(json!({"allow": rules})).unwrap();
        assert_eq!(compiled.allow, expected);
    }
    // Networks keep their own ranges where their segments differ.
    let compiled = compile(json!({
        "allow": [
            {"cidr": "192.0.2.7", "protocol": "udp", "port": 1, "endPort": 10},
            {"cidr": "198.51.100.9", "protocol": "udp", "port": 5, "endPort": 20},
        ]
    }))
    .unwrap();
    assert_eq!(
        compiled.allow,
        ["192.0.2.7/32:udp:1-10", "198.51.100.9/32:udp:5-20"]
    );
}

#[test]
fn test_keeps_network_range_whole_when_adjacent_rule_changes_union() {
    let compiled = compile(json!({
        "allow": [
            {"cidr": "10.0.0.0/25", "protocol": "tcp", "port": 1, "endPort": 10},
            {"cidr": "10.0.0.128/25", "protocol": "tcp", "port": 5, "endPort": 6},
        ]
    }))
    .unwrap();
    assert_eq!(
        compiled.allow,
        ["10.0.0.0/24:tcp:5-6", "10.0.0.0/25:tcp:1-10"]
    );
}

#[test]
fn test_rejects_invalid_port_shapes_and_values() {
    for rule in [
        json!({"cidr": "192.0.2.0/24", "protocol": "tcp", "endPort": 80}),
        json!({"cidr": "192.0.2.0/24", "protocol": "any", "endPort": 80}),
        json!({"cidr": "192.0.2.0/24", "protocol": "tcp", "port": 81, "endPort": 80}),
        json!({"cidr": "192.0.2.0/24", "protocol": "tcp", "port": 0}),
        json!({"cidr": "192.0.2.0/24", "protocol": "udp", "port": 65536}),
        json!({"cidr": "192.0.2.0/24", "protocol": "tcp", "port": true}),
        json!({"cidr": "192.0.2.0/24", "protocol": "any", "port": 0}),
        json!({"cidr": "192.0.2.0/24", "port": 80}),
        json!({"cidr": "192.0.2.0/24", "protocol": "icmp", "port": 8}),
        json!({"cidr": "192.0.2.0/24", "protocol": "icmp", "endPort": 8}),
        json!({"cidr": "192.0.2.0/24", "protocol": "ICMP"}),
        json!({"cidr": "192.0.2.0/24", "protocol": "sctp"}),
    ] {
        message(compile(json!({"allow": [rule]})));
    }
}

#[test]
fn test_reports_invalid_port_ranges_clearly() {
    let cidr = "192.0.2.0/24";
    for (rule, expected) in [
        (
            json!({
                "to": [{"cidr": cidr}],
                "ports": [{"protocol": "tcp", "port": 8010, "endPort": 8000}],
            }),
            "deny[0].ports[0].endPort 8000 cannot be below port 8010",
        ),
        (
            json!({"ports": [{"protocol": "udp", "endPort": 8010}]}),
            "deny[0].ports[0].endPort requires port",
        ),
        (
            json!({"ports": [{"endPort": 8010}]}),
            "deny[0].ports[0].endPort requires port",
        ),
        (
            json!({"ports": [{"protocol": "tcp", "port": 8000, "endPort": 65536}]}),
            "deny[0].ports[0].endPort must be between 1 and 65535",
        ),
        (
            json!({"cidr": cidr, "protocol": "tcp", "port": 8010, "endPort": 8000}),
            "deny[0].endPort 8000 cannot be below port 8010",
        ),
        (
            json!({"cidr": cidr, "protocol": "udp", "endPort": 8010}),
            "deny[0].endPort requires port",
        ),
        (
            json!({"cidr": cidr, "endPort": 8010}),
            "deny[0].endPort requires port",
        ),
        (
            json!({"cidr": cidr, "protocol": "tcp", "port": 8000, "endPort": 65536}),
            "deny[0].endPort must be between 1 and 65535",
        ),
        (
            json!({"cidr": cidr, "protocol": "tcp", "port": 0, "endPort": 8010}),
            "deny[0].port must be between 1 and 65535",
        ),
    ] {
        assert_eq!(message(compile(json!({"deny": [rule]}))), expected);
    }
}

#[test]
fn test_compiles_protocol_selectors_with_and_without_ports() {
    let selectors = [
        (json!({"protocol": "tcp"}), vec!["192.0.2.7/32:tcp"]),
        (json!({"protocol": "udp"}), vec!["192.0.2.7/32:udp"]),
        (json!({"protocol": "icmp"}), vec!["192.0.2.7/32:icmp"]),
        (json!({"protocol": "any"}), vec!["192.0.2.7/32"]),
        (json!({}), vec!["192.0.2.7/32"]),
        (
            json!({"protocol": "any", "port": 443}),
            vec!["192.0.2.7/32:tcp:443", "192.0.2.7/32:udp:443"],
        ),
        (
            json!({"protocol": "any", "port": 443, "endPort": 444}),
            vec!["192.0.2.7/32:tcp:443-444", "192.0.2.7/32:udp:443-444"],
        ),
    ];
    for category in ["allow", "deny"] {
        for (selector, expected) in &selectors {
            let rule = merged(&json!({"cidr": "192.0.2.7"}), selector);
            let compiled = compile(json!({category: [rule]})).unwrap();
            assert_eq!(
                action(&compiled, category),
                expected,
                "{category} {selector}"
            );
        }
    }
}

#[test]
fn test_protocol_wide_rules_cover_their_port_rules_only() {
    let compiled = compile(json!({
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
    }))
    .unwrap();
    assert_eq!(
        compiled.allow,
        [
            "192.0.2.0/24:icmp",
            "192.0.2.0/24:tcp",
            "192.0.2.7/32:udp:443"
        ]
    );
    assert_eq!(
        compiled.deny,
        ["198.51.100.0/24:tcp:53", "198.51.100.0/24:udp"]
    );
}

#[test]
fn test_address_only_rules_cover_protocol_wide_rules() {
    let compiled = compile(json!({
        "allow": [
            {"cidr": "192.0.2.0/24", "protocol": "udp"},
            {"cidr": "192.0.2.0/25"},
            {"cidr": "192.0.2.0/25", "protocol": "icmp"},
            {"cidr": "192.0.2.128/25", "protocol": "udp", "port": 53},
        ]
    }))
    .unwrap();
    // Widening the UDP rule into the address-only half keeps it to one prefix.
    assert_eq!(compiled.allow, ["192.0.2.0/24:udp", "192.0.2.0/25"]);
}

/// One address-only rule for each of [`MAX_RULES_PER_ACTION`] hosts in distinct networks.
fn address_only() -> Vec<Value> {
    (0..MAX_RULES_PER_ACTION)
        .map(|index| json!({"cidr": format!("10.0.{index}.1/32")}))
        .collect()
}

#[test]
fn test_protocol_wide_rules_share_the_native_rule_budget() {
    let address_only = address_only();
    let mut rules = address_only[..MAX_RULES_PER_ACTION - 1].to_vec();
    rules.push(json!({"cidr": "192.0.2.0/24", "protocol": "tcp"}));
    let accepted = compile(json!({"allow": rules})).unwrap();
    assert_eq!(accepted.allow.len(), MAX_RULES_PER_ACTION);
    assert!(accepted.allow.contains(&"192.0.2.0/24:tcp".to_owned()));

    let mut rules = address_only.clone();
    rules.push(json!({"cidr": "192.0.2.0/24", "protocol": "icmp"}));
    assert!(message(compile(json!({"allow": rules}))).contains("at most 256"));

    // Protocol `any` with a port range lowers to a TCP and a UDP range.
    let any_range = json!({"cidr": "192.0.2.1", "protocol": "any", "port": 1, "endPort": 129});
    let mut rules = address_only[..MAX_RULES_PER_ACTION - 2].to_vec();
    rules.push(any_range.clone());
    let accepted = compile(json!({"deny": rules})).unwrap();
    assert_eq!(accepted.deny.len(), MAX_RULES_PER_ACTION);
    assert!(accepted.deny.contains(&"192.0.2.1/32:udp:1-129".to_owned()));
    let mut rules = address_only[..MAX_RULES_PER_ACTION - 1].to_vec();
    rules.push(any_range);
    assert!(message(compile(json!({"deny": rules}))).contains("at most 256"));
}

#[test]
fn test_normalizes_host_bits_like_the_native_cidr_parser() {
    let compiled =
        compile(json!({"allow": [{"cidr": "10.0.0.5/24"}, {"cidr": "fe80::1/64"}]})).unwrap();
    assert_eq!(compiled.allow, ["10.0.0.0/24", "fe80::/64"]);
}

#[test]
fn test_subtracts_rule_local_cidr_exclusions() {
    let compiled = compile(json!({
        "allow": [
            {"to": [{"cidr": "192.0.2.0/24", "except": ["192.0.2.128/25"]}]},
            {"to": [{"cidr": "192.0.2.200/32"}]},
        ],
        "deny": [
            {"to": [{"cidr": "198.51.100.0/24", "except": ["198.51.100.128/25"]}]},
        ],
    }))
    .unwrap();
    assert_eq!(compiled.allow, ["192.0.2.0/25", "192.0.2.200/32"]);
    assert_eq!(compiled.deny, ["198.51.100.0/25"]);
}

#[test]
fn test_compiles_mxc_destination_and_port_unions() {
    let compiled = compile(json!({
        "allow": [{"ports": [{"port": 53}, {"protocol": "icmp"}]}],
        "deny": [{
            "to": [{"cidr": "198.51.100.0/25"}, {"cidr": "198.51.100.128/25"}],
            "ports": [{"protocol": "tcp", "port": 80}, {"protocol": "udp", "port": 53}],
        }],
    }))
    .unwrap();
    // A rule without destinations matches every destination of both families.
    assert_eq!(
        compiled.allow,
        [
            "0.0.0.0/0:icmp",
            "0.0.0.0/0:tcp:53",
            "0.0.0.0/0:udp:53",
            "::/0:icmp",
            "::/0:tcp:53",
            "::/0:udp:53",
        ]
    );
    assert_eq!(
        compiled.deny,
        ["198.51.100.0/24:tcp:80", "198.51.100.0/24:udp:53"]
    );
}

#[test]
fn test_deduplicates_overlapping_exclusions_and_collapses_safe_prefixes() {
    let compiled = compile(json!({
        "allow": [
            {
                "cidr": "192.0.2.0/24",
                "except": ["192.0.2.128/25", "192.0.2.128/25", "192.0.2.192/26"],
                "protocol": "tcp",
                "port": 443,
            },
            {"cidr": "198.51.100.0/25", "protocol": "udp", "port": 53},
            {"cidr": "198.51.100.128/25", "protocol": "udp", "port": 53},
        ]
    }))
    .unwrap();
    assert_eq!(
        compiled.allow,
        ["192.0.2.0/25:tcp:443", "198.51.100.0/24:udp:53"]
    );
}

#[test]
fn test_rejects_exclusions_outside_parent_or_wrong_family() {
    for (cidr, excluded) in [
        ("192.0.2.0/24", "198.51.100.0/24"),
        ("192.0.2.0/24", "2001:db8::/32"),
        ("2001:db8::/32", "2001:db9::/48"),
        ("2001:db8::/32", "192.0.2.0/24"),
        ("::/96", "0.0.0.0/8"),
    ] {
        let policy = json!({"allow": [{"to": [{"cidr": cidr, "except": [excluded]}]}]});
        assert!(
            message(compile(policy)).contains("except"),
            "{cidr} {excluded}"
        );
    }
}

#[test]
fn test_compiles_ipv6_networks_with_every_selector() {
    let selectors = [
        (
            json!({"ports": [{"protocol": "tcp"}]}),
            vec!["2001:db8::7/128:tcp"],
        ),
        (
            json!({"ports": [{"protocol": "udp"}]}),
            vec!["2001:db8::7/128:udp"],
        ),
        (
            json!({"ports": [{"protocol": "icmp"}]}),
            vec!["2001:db8::7/128:icmp"],
        ),
        (json!({}), vec!["2001:db8::7/128"]),
        (
            json!({"ports": [{"protocol": "any", "port": 443}]}),
            vec!["2001:db8::7/128:tcp:443", "2001:db8::7/128:udp:443"],
        ),
        (
            json!({"ports": [{"protocol": "tcp", "port": 8000, "endPort": 8010}]}),
            vec!["2001:db8::7/128:tcp:8000-8010"],
        ),
    ];
    for category in ["allow", "deny"] {
        for (selector, expected) in &selectors {
            let rule = merged(&json!({"to": [{"cidr": "2001:db8::7"}]}), selector);
            let compiled = compile(json!({category: [rule]})).unwrap();
            assert_eq!(
                action(&compiled, category),
                expected,
                "{category} {selector}"
            );
        }
    }
}

#[test]
fn test_compiles_one_allowed_ipv6_network_with_a_denied_address() {
    let compiled = compile(json!({
        "allow": [
            {"to": [{"cidr": "2001:db8:1::/64"}], "ports": [{"protocol": "tcp", "port": 443}]},
            {"to": [{"cidr": "::/0"}], "ports": [{"protocol": "udp", "port": 53}]},
        ],
        "deny": [{"to": [{"cidr": "2001:db8:1::123"}]}],
    }))
    .unwrap();
    assert_eq!(compiled.allow, ["::/0:udp:53", "2001:db8:1::/64:tcp:443"]);
    assert_eq!(compiled.deny, ["2001:db8:1::123/128"]);
}

#[test]
fn test_flat_rules_accept_ipv6_networks() {
    let compiled = compile(json!({
        "allow": [{
            "cidr": "2001:db8::/32",
            "except": ["2001:db8::/33"],
            "protocol": "tcp",
            "port": 443,
        }]
    }))
    .unwrap();
    assert_eq!(compiled.allow, ["2001:db8:8000::/33:tcp:443"]);
}

#[test]
fn test_keeps_each_address_family_separate() {
    // IPv4 0.0.0.0/0 and IPv6 ::/96 span the same integers, so lowering both families together
    // would merge or cover one with the other.
    let compiled = compile(json!({
        "allow": [
            {
                "to": [{"cidr": "::/96"}, {"cidr": "0.0.0.0/0"}],
                "ports": [{"protocol": "tcp", "port": 80}],
            },
            {"to": [{"cidr": "192.0.2.0/24"}]},
            {"to": [{"cidr": "::c000:200/120"}], "ports": [{"protocol": "udp", "port": 53}]},
        ],
        "deny": [
            {"to": [{"cidr": "0.0.0.0/0"}]},
            {"to": [{"cidr": "::/96"}], "ports": [{"protocol": "icmp"}]},
        ],
    }))
    .unwrap();
    assert_eq!(
        compiled.allow,
        [
            "0.0.0.0/0:tcp:80",
            "192.0.2.0/24",
            "::/96:tcp:80",
            "::c000:200/120:udp:53",
        ]
    );
    assert_eq!(compiled.deny, ["0.0.0.0/0", "::/96:icmp"]);
}

#[test]
fn test_lowers_the_destinations_of_each_family_separately() {
    let compiled = compile(json!({
        "allow": [{
            "to": [
                {"cidr": "192.0.2.0/25"},
                {"cidr": "2001:db8::/32", "except": ["2001:db8::/33"]},
                {"cidr": "192.0.2.128/25"},
            ],
            "ports": [{"protocol": "tcp", "port": 443}],
        }]
    }))
    .unwrap();
    assert_eq!(
        compiled.allow,
        ["192.0.2.0/24:tcp:443", "2001:db8:8000::/33:tcp:443"]
    );
}

#[test]
fn test_subtracts_ipv6_exclusions() {
    let compiled = compile(json!({
        "deny": [{
            "to": [{"cidr": "2001:db8::/32", "except": ["2001:db8:1::/48", "2001:db8:1::/64"]}],
            "ports": [{"protocol": "tcp", "port": 22}],
        }]
    }))
    .unwrap();
    // `IPv6Network("2001:db8::/32").address_exclude(IPv6Network("2001:db8:1::/48"))`, sorted.
    assert_eq!(
        compiled.deny,
        [
            "2001:db8::/48:tcp:22",
            "2001:db8:2::/47:tcp:22",
            "2001:db8:4::/46:tcp:22",
            "2001:db8:8::/45:tcp:22",
            "2001:db8:10::/44:tcp:22",
            "2001:db8:20::/43:tcp:22",
            "2001:db8:40::/42:tcp:22",
            "2001:db8:80::/41:tcp:22",
            "2001:db8:100::/40:tcp:22",
            "2001:db8:200::/39:tcp:22",
            "2001:db8:400::/38:tcp:22",
            "2001:db8:800::/37:tcp:22",
            "2001:db8:1000::/36:tcp:22",
            "2001:db8:2000::/35:tcp:22",
            "2001:db8:4000::/34:tcp:22",
            "2001:db8:8000::/33:tcp:22",
        ]
    );
    assert_eq!(compiled.deny.len(), 16);
}

#[test]
fn test_rejects_ipv6_scopes() {
    // Clearing host bits also clears the scope of the network address, so the scoped addresses
    // with host bits must not compile as unscoped.
    let cidrs = [
        "fe80::%eth0/64",
        "fe80::1%1",
        "fe80::1%eth0/64",
        "fe80::1%1/64",
        "::ffff:192.0.2.1%eth0/120",
    ];
    for category in ["allow", "deny"] {
        for cidr in cidrs {
            for (field, rule) in [
                ("to[0].cidr", json!({"to": [{"cidr": cidr}]})),
                (
                    "to[0].except[0]",
                    json!({"to": [{"cidr": "::/0", "except": [cidr]}]}),
                ),
                ("cidr", json!({"cidr": cidr})),
                ("except[0]", json!({"cidr": "::/0", "except": [cidr]})),
            ] {
                assert_eq!(
                    message(compile(json!({category: [rule]}))),
                    format!("{category}[0].{field} must not name an IPv6 scope: {cidr:?}")
                );
            }
        }
    }
}

#[test]
fn test_both_families_share_the_native_rule_budget() {
    // Ports that are not adjacent cannot form one range, so each lowers to its own native rule.
    let rules = |ipv4_ports: u16, ipv6_ports: u16| {
        let rule = |cidr: &str, ports: u16| {
            let selectors: Vec<Value> = (0..ports)
                .map(|index| json!({"protocol": "tcp", "port": 2 * index + 1}))
                .collect();
            json!({"to": [{"cidr": cidr}], "ports": selectors})
        };
        json!({"allow": [rule("192.0.2.1", ipv4_ports), rule("2001:db8::1", ipv6_ports)]})
    };
    assert_eq!(compile(rules(128, 128)).unwrap().allow.len(), 256);
    for (ipv4_ports, ipv6_ports) in [(129, 128), (128, 129)] {
        assert!(message(compile(rules(ipv4_ports, ipv6_ports))).contains("at most 256"));
    }
}

#[test]
fn test_rejects_invalid_mxc_rule_shapes() {
    for rule in [
        json!({"to": []}),
        json!({"to": [{}]}),
        json!({"to": [{"cidr": "192.0.2.0/24", "unknown": true}]}),
        json!({"ports": []}),
        json!({"ports": [{"protocol": "icmp", "port": 8}]}),
        json!({"ports": [{"protocol": "ICMP"}]}),
        json!({"ports": [{"protocol": "sctp"}]}),
        json!({"ports": [{"endPort": 80}]}),
        json!({"to": [{"cidr": "192.0.2.0/24"}], "cidr": "192.0.2.0/24"}),
    ] {
        message(compile(json!({"allow": [rule]})));
    }
}

#[test]
fn test_rejects_unknown_and_malformed_fields() {
    for value in [
        json!([]),
        json!({"unknown": []}),
        json!({"allow": {}}),
        json!({"allow": ["192.0.2.0/24"]}),
        json!({"allow": [{"cidr": "192.0.2.0/24", "unknown": 1}]}),
        json!({"allow": [{"cidr": 7}]}),
        json!({"allow": [{"cidr": "192.0.2.0/24", "except": "192.0.2.1"}]}),
        json!({"allow": [{"cidr": "192.0.2.0/24", "protocol": null}]}),
    ] {
        message(compile(value));
    }
}

#[test]
fn test_rejects_duplicate_json_fields() {
    for text in [
        r#"{"deny":[{"cidr":"0.0.0.0/0"}],"deny":[]}"#,
        r#"{"allow":[{"cidr":"192.0.2.0/24","port":80,"port":443,"protocol":"tcp"}]}"#,
    ] {
        assert!(
            message(compile_text(text)).contains("duplicate field"),
            "{text}"
        );
    }
}

#[test]
fn test_empty_destination_does_not_expand_port_range() {
    let compiled = compile(json!({
        "allow": [{
            "cidr": "192.0.2.0/24",
            "except": ["192.0.2.0/24"],
            "protocol": "tcp",
            "port": 1,
            "endPort": 65535,
        }]
    }))
    .unwrap();
    assert!(compiled.allow.is_empty());
    assert!(compiled.deny.is_empty());
}

#[test]
fn test_limits_the_actual_policy_read() {
    // The compiler reads one byte past the limit instead of trusting the file's metadata, which
    // the Python test fakes as empty.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("policy.json");
    std::fs::write(&path, vec![b' '; MAX_POLICY_FILE_SIZE + 1]).unwrap();
    assert!(message(compile_policy_file(&path)).contains("byte limit"));
}

#[test]
fn test_reports_oversized_json_integer_rejection() {
    // Strict JSON refuses an integer beyond the range of a double, which Python read, wherever it
    // is.
    let text = format!(r#"{{"allow":[{{"cidr":{}}}]}}"#, "9".repeat(10000));
    assert_eq!(
        message(compile_text(&text)),
        "egress policy file policy.json is not valid JSON: number out of range at line 1 column \
         10018"
    );
}

#[test]
fn test_many_source_networks_can_collapse_within_native_budget() {
    let start = u32::from(std::net::Ipv4Addr::new(192, 0, 2, 0));
    let rules: Vec<Value> = (0..512)
        .map(|offset| {
            let address = std::net::Ipv4Addr::from(start + offset);
            json!({"cidr": address.to_string(), "protocol": "tcp", "port": 80})
        })
        .collect();
    let compiled = compile(json!({"allow": rules})).unwrap();
    assert_eq!(compiled.allow, ["192.0.2.0/23:tcp:80"]);
}

#[test]
fn test_accepts_exact_256_rule_boundaries() {
    // Ranges with a port between them cannot merge.
    let separate_ranges = |cidr: &str, protocol: &str| -> Vec<Value> {
        (0..MAX_RULES_PER_ACTION)
            .map(|index| {
                json!({
                    "cidr": cidr,
                    "protocol": protocol,
                    "port": 3 * index + 1,
                    "endPort": 3 * index + 2,
                })
            })
            .collect()
    };
    let compiled = compile(json!({
        "allow": separate_ranges("192.0.2.1", "tcp"),
        "deny": separate_ranges("198.51.100.1", "udp"),
    }))
    .unwrap();
    assert_eq!(compiled.allow.len(), 256);
    assert_eq!(compiled.deny.len(), 256);
    assert_eq!(
        compiled.allow[..2],
        ["192.0.2.1/32:tcp:1-2", "192.0.2.1/32:tcp:4-5"]
    );
    assert_eq!(compiled.deny.last().unwrap(), "198.51.100.1/32:udp:766-767");
}

#[test]
fn test_rejects_257_rules_before_materializing_them() {
    for category in ["allow", "deny"] {
        let rules: Vec<Value> = (0..=MAX_RULES_PER_ACTION)
            .map(|index| json!({"cidr": "192.0.2.1", "protocol": "tcp", "port": 2 * index + 1}))
            .collect();
        assert!(message(compile(json!({category: rules}))).contains("at most 256"));
    }
}

#[test]
fn test_overlapping_segments_do_not_inflate_native_rule_budget() {
    let internal_ports = (1000..1056).step_by(2);
    let mut rules = vec![json!({
        "cidr": "0.0.0.0/0",
        "except": ["10.0.0.0/8"],
        "protocol": "tcp",
        "port": 1,
        "endPort": 65535,
    })];
    rules.extend(
        internal_ports
            .clone()
            .map(|port| json!({"cidr": "10.0.0.0/8", "protocol": "tcp", "port": port})),
    );
    let compiled = compile(json!({"allow": rules})).unwrap();
    let mut expected: Vec<String> = [
        "0.0.0.0/5",
        "8.0.0.0/7",
        "11.0.0.0/8",
        "12.0.0.0/6",
        "16.0.0.0/4",
        "32.0.0.0/3",
        "64.0.0.0/2",
        "128.0.0.0/1",
    ]
    .iter()
    .map(|cidr| format!("{cidr}:tcp:1-65535"))
    .chain(internal_ports.map(|port| format!("0.0.0.0/0:tcp:{port}")))
    .collect();
    let mut actual = compiled.allow.clone();
    expected.sort();
    actual.sort();
    assert_eq!(actual, expected);
    assert_eq!(compiled.allow.len(), 36);
}

#[test]
fn test_bounds_exclusions_times_port_segments() {
    // The exclusion leaves eight networks, and the gap between ports keeps each port a segment
    // of its own.
    let rules = |count: usize, port: &dyn Fn(usize) -> usize| -> Vec<Value> {
        (0..count)
            .map(|index| {
                json!({
                    "cidr": "192.0.2.0/24",
                    "except": ["192.0.2.1/32"],
                    "protocol": "tcp",
                    "port": port(index),
                })
            })
            .collect()
    };
    let separate = |index: usize| 2 * index + 1;
    let accepted = compile(json!({"allow": rules(32, &separate)})).unwrap();
    assert_eq!(accepted.allow.len(), 256);
    assert!(message(compile(json!({"allow": rules(33, &separate)}))).contains("at most 256"));

    // Adjacent ports merge into one range for each network.
    let merged = compile(json!({"allow": rules(33, &|index| index + 1)})).unwrap();
    assert_eq!(merged.allow.len(), 8);
    assert!(merged.allow.iter().all(|rule| rule.ends_with(":tcp:1-33")));
}

#[test]
fn test_delays_fragment_conversion_until_after_covering_union() {
    let first = u32::from(std::net::Ipv4Addr::new(192, 0, 0, 1));
    let exclusions: Vec<String> = (0..4096)
        .map(|index| std::net::Ipv4Addr::from(first + 2 * index).to_string())
        .collect();
    let fragmented = json!({
        "cidr": "192.0.0.0/16",
        "except": exclusions,
        "protocol": "tcp",
        "port": 80,
        "endPort": 81,
    });
    let covering = json!({"cidr": "192.0.0.0/16", "protocol": "tcp", "port": 80, "endPort": 81});
    // The Python test counts calls of `ipaddress.summarize_address_range` over both orders: one
    // for each compilation, of the union, and none for the fragments.
    let ((), summaries, _) = probe::count(|| {
        for rules in [
            [fragmented.clone(), covering.clone()],
            [covering.clone(), fragmented.clone()],
        ] {
            let policy = serde_json::to_vec(&json!({"allow": rules})).unwrap();
            let compiled = compile_policy_bytes(&policy, Path::new("policy.json")).unwrap();
            assert_eq!(compiled.allow, ["192.0.0.0/16:tcp:80-81"]);
        }
    });
    assert_eq!(summaries, 2);
}

#[test]
fn test_validates_redundant_rules_before_canonicalization() {
    let policy = json!({
        "allow": [
            {"cidr": "192.0.2.0/24"},
            {"cidr": "192.0.2.1", "unknown": true},
        ]
    });
    assert!(message(compile(policy)).contains("unknown field"));
}

#[test]
fn test_address_only_rule_covers_large_protocol_range() {
    let ranged = json!({"cidr": "192.0.2.0/24", "protocol": "tcp", "port": 1, "endPort": 65535});
    let address_only = json!({"cidr": "192.0.2.0/24"});
    for rules in [
        [ranged.clone(), address_only.clone()],
        [address_only.clone(), ranged.clone()],
    ] {
        let compiled = compile(json!({"allow": rules})).unwrap();
        assert_eq!(compiled.allow, ["192.0.2.0/24"]);
    }
}

#[test]
fn test_address_only_rule_prunes_redundant_port_events_before_sweep() {
    let covering = json!({"cidr": "0.0.0.0/0"});
    let first = u32::from(std::net::Ipv4Addr::new(192, 0, 0, 0));
    let protocol_rules: Vec<Value> = (1..=1000u32)
        .map(|start| {
            json!({
                "cidr": format!("{}/32", std::net::Ipv4Addr::from(first + start)),
                "protocol": "tcp",
                "port": start,
                "endPort": 2001 - start,
            })
        })
        .collect();
    for category in ["allow", "deny"] {
        for covering_first in [true, false] {
            let mut rules = protocol_rules.clone();
            if covering_first {
                rules.insert(0, covering.clone());
            } else {
                rules.push(covering.clone());
            }
            let policy = serde_json::to_vec(&json!({category: rules})).unwrap();
            // The Python test counts the intervals that `_merge_intervals` receives: only the
            // address-only rule's, because its coverage drops every port rule before the sweep.
            let (compiled, _, merged_intervals) =
                probe::count(|| compile_policy_bytes(&policy, Path::new("policy.json")));
            assert_eq!(action(&compiled.unwrap(), category), ["0.0.0.0/0"]);
            assert_eq!(merged_intervals, 1, "{category} {covering_first}");
        }
    }
}

#[test]
fn test_partial_address_only_coverage_keeps_protocol_union_compact() {
    let address_only_rules: Vec<Value> = (0..128)
        .map(|index| json!({"cidr": format!("192.0.0.{}/32", 2 * index + 1)}))
        .collect();
    let protocol_rule = json!({"cidr": "192.0.0.0/16", "protocol": "tcp", "port": 443});
    for category in ["allow", "deny"] {
        let mut compiled_orders = Vec::new();
        for protocol_first in [false, true] {
            let mut rules = address_only_rules.clone();
            if protocol_first {
                rules.insert(0, protocol_rule.clone());
            } else {
                rules.push(protocol_rule.clone());
            }
            let compiled = compile(json!({category: rules})).unwrap();
            let compiled_rules = action(&compiled, category).to_vec();
            assert_eq!(compiled_rules.len(), 129);
            assert!(compiled_rules.contains(&"192.0.0.0/16:tcp:443".to_owned()));
            let others: std::collections::BTreeSet<&str> = compiled_rules
                .iter()
                .filter(|rule| !rule.ends_with(":tcp:443"))
                .map(String::as_str)
                .collect();
            let hosts: std::collections::BTreeSet<&str> = address_only_rules
                .iter()
                .map(|rule| rule["cidr"].as_str().unwrap())
                .collect();
            assert_eq!(others, hosts);
            compiled_orders.push(compiled_rules);
        }
        assert_eq!(compiled_orders[0], compiled_orders[1]);
    }
}

#[test]
fn test_partial_address_only_canonicalization_does_not_bridge_gaps() {
    let compiled = compile(json!({
        "allow": [
            {"cidr": "192.0.2.1/32"},
            {"cidr": "192.0.2.0/32", "protocol": "tcp", "port": 443},
            {"cidr": "192.0.2.3/32", "protocol": "tcp", "port": 443},
        ]
    }))
    .unwrap();
    assert_eq!(
        compiled.allow,
        [
            "192.0.2.0/31:tcp:443",
            "192.0.2.1/32",
            "192.0.2.3/32:tcp:443",
        ]
    );
}

// Tests of the port that the Python tests do not have.

#[test]
fn rules_share_their_destinations_among_their_selectors() {
    // 8192 destinations, none adjacent to another, for 8192 selectors: copying the destinations
    // for each selector would take 2 GiB. Python 3.14 compiles each policy as asserted here.
    let first = u32::from(std::net::Ipv4Addr::new(10, 0, 0, 0));
    let destinations: Vec<Value> = (0..8192u32)
        .map(|index| json!({"cidr": std::net::Ipv4Addr::from(first + 2 * index).to_string()}))
        .collect();
    let tcp: Vec<Value> = (0..8192u32)
        .map(|index| json!({"protocol": "tcp", "port": 2 * index + 1}))
        .collect();
    let mixed: Vec<Value> = [
        vec![json!({"protocol": "any"}); 2048],
        vec![json!({"protocol": "icmp"}); 2048],
        tcp[..4096].to_vec(),
    ]
    .concat();
    let compiled = compile(json!({
        "allow": [
            {"to": destinations, "ports": mixed},
            {"to": [{"cidr": "0.0.0.0/0"}]},
        ]
    }))
    .unwrap();
    assert_eq!(compiled.allow, ["0.0.0.0/0"]);
    assert!(compiled.deny.is_empty());
    assert_eq!(
        message(compile(
            json!({"deny": [{"to": destinations, "ports": tcp}]})
        )),
        "deny emits at most 256 native rules"
    );
    let any = vec![json!({"protocol": "any"}); 8192];
    assert_eq!(
        message(compile(
            json!({"allow": [{"to": destinations, "ports": any}]})
        )),
        "allow emits at most 256 native rules"
    );
}
