//! Tests of the grammar of policy files, one for each rule of the format that `doc/run.md`
//! documents, which check what each refusal names, the field path or the file, a fragment of its
//! reason that tells apart the rules that refusals at one path break, and its error code,
//! [`ErrorCode::Unsupported`] (microsoft/nvx#724).

use std::path::Path;

use serde_json::{Value, json};

use crate::ErrorCode;
use crate::error::Result;
use crate::host::network::policy::{
    CompiledEgressPolicy, MAX_JSON_DEPTH, MAX_POLICY_FILE_SIZE, MAX_RULES_PER_ACTION,
    compile_policy_bytes, compile_policy_file,
};

/// The name of the policy file of the tests, which messages name.
const FILE: &str = "policy.json";

/// What the message of a refusal names.
#[derive(Debug, Clone, Copy)]
enum Names<'a> {
    /// A field path, such as `allow[0].to[1].cidr`, which starts the message, or `egress policy`
    /// for the root.
    Field(&'a str),
    /// The file, [`FILE`].
    File,
}

/// Compiles the policy file `data`.
fn compile(data: impl AsRef<[u8]>) -> Result<CompiledEgressPolicy> {
    compile_policy_bytes(data.as_ref(), Path::new(FILE))
}

// Fragments of the reasons that messages give, which tell apart the rules that refusals at one
// field path break.

/// A value that is not an object.
const NOT_AN_OBJECT: &str = "must be an object";
/// A value that is not an array.
const NOT_AN_ARRAY: &str = "must be an array";
/// An object with a field that its form does not have.
const UNKNOWN_FIELD: &str = "unknown field";
/// A rule with fields of both forms.
const MIXED_FORMS: &str = "mix";
/// An object that repeats a field.
const REPEATED_FIELD: &str = "duplicate";
/// A missing field.
const REQUIRED: &str = "required";
/// A CIDR that is not a string.
const NOT_A_STRING: &str = "string";
/// A string that is not a CIDR.
const NOT_A_CIDR: &str = "not a valid";
/// An IPv6 CIDR with a scope.
const SCOPED: &str = "scope";
/// An exception outside its network or family.
const OUTSIDE: &str = "contained";
/// A protocol that is not one of the format's.
const NOT_A_PROTOCOL: &str = "tcp, udp, icmp, or any";
/// A port that is not an integer.
const NOT_AN_INTEGER: &str = "integer";
/// A port outside the port range.
const OUT_OF_RANGE: &str = "between 1 and 65535";
/// A port of ICMP.
const ICMP_PORT: &str = "with icmp";
/// A port of a flat rule without a protocol.
const NEEDS_PROTOCOL: &str = "requires protocol";
/// An `endPort` without a port.
const NEEDS_PORT: &str = "requires port";
/// An `endPort` below its port.
const BELOW_PORT: &str = "below";
/// An empty `to` or `ports`.
const EMPTY: &str = "at least one";
/// An action that needs more native rules than OpenVMM accepts.
const TOO_MANY_RULES: &str = "at most 256";
/// A file larger than the format's limit.
const TOO_LARGE: &str = "limit";
/// A file that cannot be read.
const UNREADABLE: &str = "failed to read";
/// A file that is not UTF-8.
const NOT_UTF8: &str = "not UTF-8";
/// A file that is not JSON.
const NOT_JSON: &str = "not valid JSON";
/// A file whose arrays and objects nest too deep.
const TOO_DEEP: &str = "deep";

/// Checks that the policy file `data` is refused with the code `unsupported` and a message that
/// names `names` and gives `reason`, and returns the message.
#[track_caller]
fn assert_refused(data: impl AsRef<[u8]>, names: Names<'_>, reason: &str) -> String {
    let data = data.as_ref();
    let text = String::from_utf8_lossy(&data[..data.len().min(200)]);
    let error = match compile(data) {
        Ok(compiled) => panic!("{text} compiles to {compiled:?}"),
        Err(error) => error,
    };
    assert_eq!(error.code(), ErrorCode::Unsupported, "{text}: {error}");
    let message = error.message();
    let named = match names {
        Names::Field(path) => message
            .strip_prefix(path)
            .is_some_and(|rest| rest.starts_with(' ')),
        Names::File => message.contains(FILE),
    };
    assert!(named, "{text}: {message:?} does not name {names:?}");
    assert!(
        message.contains(reason),
        "{text}: {message:?} does not say {reason:?}"
    );
    message.to_owned()
}

/// Checks that the policy file `data` compiles to the native rules `allow` and `deny`.
#[track_caller]
fn assert_compiles(data: impl AsRef<[u8]>, allow: &[&str], deny: &[&str]) {
    let data = data.as_ref();
    let text = String::from_utf8_lossy(&data[..data.len().min(200)]);
    let compiled = compile(data).unwrap_or_else(|error| panic!("{text}: {error}"));
    assert_eq!(compiled.allow, allow, "{text}");
    assert_eq!(compiled.deny, deny, "{text}");
}

/// Returns the text of the policy whose action `category` has the one rule `rule`.
fn rule(category: &str, rule: Value) -> String {
    json!({category: [rule]}).to_string()
}

#[test]
fn the_root_is_an_object_of_allow_and_deny_arrays() {
    for (text, path, reason) in [
        ("[]", "egress policy", NOT_AN_OBJECT),
        ("null", "egress policy", NOT_AN_OBJECT),
        ("1", "egress policy", NOT_AN_OBJECT),
        (r#""allow""#, "egress policy", NOT_AN_OBJECT),
        (r#"{"allowed": []}"#, "egress policy", UNKNOWN_FIELD),
        (
            r#"{"allow": [], "Deny": []}"#,
            "egress policy",
            UNKNOWN_FIELD,
        ),
        (r#"{"allow": {}}"#, "allow", NOT_AN_ARRAY),
        (r#"{"allow": null}"#, "allow", NOT_AN_ARRAY),
        (r#"{"deny": "192.0.2.0/24"}"#, "deny", NOT_AN_ARRAY),
        (r#"{"deny": 1}"#, "deny", NOT_AN_ARRAY),
        // `allow` is checked before `deny`.
        (r#"{"deny": 1, "allow": 1}"#, "allow", NOT_AN_ARRAY),
    ] {
        assert_refused(text, Names::Field(path), reason);
    }
    assert_compiles("{}", &[], &[]);
    assert_compiles(r#"{"allow": [], "deny": []}"#, &[], &[]);
}

#[test]
fn rules_are_objects() {
    for (text, path) in [
        (r#"{"allow": ["192.0.2.0/24"]}"#, "allow[0]"),
        (r#"{"allow": [7]}"#, "allow[0]"),
        (r#"{"deny": [null]}"#, "deny[0]"),
        (r#"{"deny": [[{"cidr": "192.0.2.0/24"}]]}"#, "deny[0]"),
        (r#"{"allow": [{}, "192.0.2.0/24"]}"#, "allow[1]"),
    ] {
        assert_refused(text, Names::Field(path), NOT_AN_OBJECT);
    }
    // A rule without fields selects every destination of both families, and every protocol.
    assert_compiles(r#"{"allow": [{}]}"#, &["0.0.0.0/0", "::/0"], &[]);
}

#[test]
fn mxc_and_flat_fields_do_not_mix() {
    let mxc = [
        ("to", json!([{"cidr": "192.0.2.0/24"}])),
        ("ports", json!([{"protocol": "tcp"}])),
    ];
    let flat = [
        ("cidr", json!("192.0.2.0/24")),
        ("except", json!([])),
        ("protocol", json!("tcp")),
        ("port", json!(80)),
        ("endPort", json!(81)),
    ];
    for category in ["allow", "deny"] {
        for (mxc_field, mxc_value) in &mxc {
            for (flat_field, flat_value) in &flat {
                let rule = json!({*mxc_field: mxc_value, *flat_field: flat_value});
                let path = format!("{category}[0]");
                let rule = self::rule(category, rule);
                assert_refused(rule, Names::Field(&path), MIXED_FORMS);
            }
        }
    }
}

#[test]
fn unknown_fields_are_refused() {
    let cidr = "192.0.2.0/24";
    for (text, path) in [
        (json!({"allow": [], "unknownField": []}), "egress policy"),
        (
            json!({"allow": [{"cidr": cidr, "unknownField": 1}]}),
            "allow[0]",
        ),
        (
            json!({"deny": [{"to": [{"cidr": cidr}], "unknownField": 1}]}),
            "deny[0]",
        ),
        (json!({"deny": [{"unknownField": 1}]}), "deny[0]"),
        (
            json!({"allow": [{"to": [{"cidr": cidr, "unknownField": 1}]}]}),
            "allow[0].to[0]",
        ),
        (
            json!({"allow": [{"ports": [{"protocol": "tcp", "unknownField": 1}]}]}),
            "allow[0].ports[0]",
        ),
    ] {
        let message = assert_refused(text.to_string(), Names::Field(path), UNKNOWN_FIELD);
        assert!(message.ends_with("\"unknownField\""), "{message}");
    }
    // Field names are case-sensitive.
    for (text, path) in [
        (json!({"Allow": []}), "egress policy"),
        (json!({"allow": [{"CIDR": cidr}]}), "allow[0]"),
        (
            json!({"allow": [{"cidr": cidr, "Protocol": "tcp"}]}),
            "allow[0]",
        ),
        (
            json!({"allow": [{"to": [{"cidr": cidr, "Except": []}]}]}),
            "allow[0].to[0]",
        ),
        (
            json!({"allow": [{"ports": [{"EndPort": 80}]}]}),
            "allow[0].ports[0]",
        ),
    ] {
        assert_refused(text.to_string(), Names::Field(path), UNKNOWN_FIELD);
    }
}

#[test]
fn fields_appear_once() {
    for (text, object, field) in [
        (r#"{"deny": [], "deny": []}"#, "egress policy", "deny"),
        (
            r#"{"allow": [], "\u0061llow": []}"#,
            "egress policy",
            "allow",
        ),
        (
            r#"{"allow": [{"cidr": "10.0.0.0/8", "cidr": "::/0"}]}"#,
            "allow[0]",
            "cidr",
        ),
        (
            r#"{"allow": [{"to": [{"cidr": "::/0"}], "to": []}]}"#,
            "allow[0]",
            "to",
        ),
        (
            r#"{"allow": [{"to": [{"except": [], "except": []}]}]}"#,
            "allow[0].to[0]",
            "except",
        ),
        (
            r#"{"allow": [{"ports": [{"port": 1, "port": 2}]}]}"#,
            "allow[0].ports[0]",
            "port",
        ),
        // Even in an object that is refused anyway.
        (r#"{"unknown": {"x": 1, "x": 2}}"#, "unknown", "x"),
    ] {
        let message = assert_refused(text, Names::Field(object), REPEATED_FIELD);
        assert!(message.ends_with(&format!("{field:?}")), "{message}");
    }
}

#[test]
fn destinations_need_one_cidr_string() {
    let cidr = "192.0.2.0/24";
    for (rule, path, reason) in [
        (json!({"to": [{}]}), "allow[0].to[0].cidr", REQUIRED),
        (
            json!({"to": [{"except": []}]}),
            "allow[0].to[0].cidr",
            REQUIRED,
        ),
        (
            json!({"to": [{"cidr": cidr}, {}]}),
            "allow[0].to[1].cidr",
            REQUIRED,
        ),
        (
            json!({"protocol": "tcp", "port": 80}),
            "allow[0].cidr",
            REQUIRED,
        ),
        (json!({"except": []}), "allow[0].cidr", REQUIRED),
        (
            json!({"to": [{"cidr": 7}]}),
            "allow[0].to[0].cidr",
            NOT_A_STRING,
        ),
        (
            json!({"to": [{"cidr": null}]}),
            "allow[0].to[0].cidr",
            NOT_A_STRING,
        ),
        (json!({"cidr": [cidr]}), "allow[0].cidr", NOT_A_STRING),
        (json!({"cidr": {}}), "allow[0].cidr", NOT_A_STRING),
        (
            json!({"cidr": cidr, "except": "192.0.2.1"}),
            "allow[0].except",
            NOT_AN_ARRAY,
        ),
        (
            json!({"cidr": cidr, "except": null}),
            "allow[0].except",
            NOT_AN_ARRAY,
        ),
        (
            json!({"to": [{"cidr": cidr, "except": {}}]}),
            "allow[0].to[0].except",
            NOT_AN_ARRAY,
        ),
        (
            json!({"cidr": cidr, "except": [7]}),
            "allow[0].except[0]",
            NOT_A_STRING,
        ),
        (
            json!({"to": [{"cidr": cidr, "except": ["192.0.2.1", null]}]}),
            "allow[0].to[0].except[1]",
            NOT_A_STRING,
        ),
    ] {
        assert_refused(self::rule("allow", rule), Names::Field(path), reason);
    }
}

#[test]
fn cidrs_are_ipv4_or_ipv6_networks() {
    for cidr in [
        "",
        "x",
        "/8",
        "192.0.2.0/33",
        "256.0.0.0",
        "010.0.0.5",
        "10.0.0",
        "10.0.0.0.5",
        " 10.0.0.5",
        "10.0.0.5 ",
        "10.0.0.5/",
        "10.0.0.5//8",
        "10.0.0.5/8/8",
        "10.0.0.5/+8",
        "10.0.0.5/0x8",
        "10.0.0.5/255.0.255.0",
        "10.0.0.\u{665}",
        "::/129",
        "1::2::3",
        ":::",
        "00000::",
        "[::1]",
        "2001:db8::g",
        "1:2:3:4:5:6:7:1.2.3.4",
        "fe80::1%",
    ] {
        for (rule, path) in [
            (json!({"to": [{"cidr": cidr}]}), "deny[0].to[0].cidr"),
            (json!({"cidr": cidr}), "deny[0].cidr"),
            (
                json!({"to": [{"cidr": "::/0", "except": ["::/1", cidr]}]}),
                "deny[0].to[0].except[1]",
            ),
            (
                json!({"cidr": "0.0.0.0/0", "except": [cidr]}),
                "deny[0].except[0]",
            ),
        ] {
            let rule = self::rule("deny", rule);
            let message = assert_refused(rule, Names::Field(path), NOT_A_CIDR);
            assert!(message.ends_with(&format!(": {cidr:?}")), "{message}");
        }
    }
    // Addresses take every form of their family, and IPv4 prefixes may be netmasks or hostmasks.
    for (cidr, native) in [
        ("192.0.2.7", "192.0.2.7/32"),
        ("10.0.0.0/255.255.255.0", "10.0.0.0/24"),
        ("10.0.0.0/0.0.0.255", "10.0.0.0/24"),
        ("10.0.0.0/255.255.255.255", "10.0.0.0/32"),
        ("10.0.0.0/0.0.0.0", "0.0.0.0/0"),
        ("10.0.0.0/08", "10.0.0.0/8"),
        ("2001:DB8::/32", "2001:db8::/32"),
        ("2001:0db8:0000:0000::/32", "2001:db8::/32"),
        ("fe80::1", "fe80::1/128"),
        ("::ffff:192.0.2.1/120", "::ffff:192.0.2.0/120"),
        ("::FFFF:c000:201/120", "::ffff:192.0.2.0/120"),
        ("64:ff9b::192.0.2.1/120", "64:ff9b::c000:200/120"),
        ("1:2:3:4:5:6:1.2.3.4", "1:2:3:4:5:6:102:304/128"),
    ] {
        assert_compiles(self::rule("allow", json!({"cidr": cidr})), &[native], &[]);
    }
}

#[test]
fn host_bits_are_cleared() {
    for (cidr, native) in [
        ("10.0.0.5/24", "10.0.0.0/24"),
        ("192.0.2.255/0", "0.0.0.0/0"),
        ("10.0.0.5/255.0.0.0", "10.0.0.0/8"),
        ("fe80::1/64", "fe80::/64"),
        ("::ffff:192.0.2.1/120", "::ffff:192.0.2.0/120"),
    ] {
        assert_compiles(self::rule("allow", json!({"cidr": cidr})), &[native], &[]);
        let mxc = json!({"to": [{"cidr": cidr}], "ports": [{"protocol": "udp"}]});
        assert_compiles(self::rule("deny", mxc), &[], &[&format!("{native}:udp")]);
    }
    // An exception with host bits is its network.
    let peer = |exception: &str| json!({"to": [{"cidr": "10.0.0.0/8", "except": [exception]}]});
    let cleared = compile(rule("allow", peer("10.1.0.0/16"))).unwrap();
    assert_eq!(cleared.allow.len(), 8);
    assert_eq!(
        compile(rule("allow", peer("10.1.2.3/16"))).unwrap(),
        cleared
    );
}

#[test]
fn ipv6_scopes_are_refused() {
    // Clearing host bits would clear the scope too, so scopes with host bits are refused as well
    // (microsoft/nvx#462).
    for cidr in [
        "fe80::%eth0/64",
        "fe80::1%1",
        "fe80::1%eth0/64",
        "fe80::1%1/64",
        "::ffff:192.0.2.1%eth0/120",
    ] {
        for category in ["allow", "deny"] {
            for (rule, field) in [
                (json!({"to": [{"cidr": cidr}]}), "to[0].cidr"),
                (
                    json!({"to": [{"cidr": "::/0", "except": [cidr]}]}),
                    "to[0].except[0]",
                ),
                (json!({"cidr": cidr}), "cidr"),
                (json!({"cidr": "::/0", "except": [cidr]}), "except[0]"),
            ] {
                let path = format!("{category}[0].{field}");
                let rule = self::rule(category, rule);
                assert_refused(rule, Names::Field(&path), SCOPED);
            }
        }
    }
}

#[test]
fn exceptions_lie_within_their_network_and_family() {
    for (cidr, exception) in [
        ("192.0.2.0/24", "198.51.100.0/24"),
        ("192.0.2.0/24", "192.0.0.0/16"),
        ("192.0.2.0/24", "192.0.2.0/23"),
        ("192.0.2.0/24", "2001:db8::/32"),
        ("2001:db8::/32", "2001:db9::/48"),
        ("2001:db8::/32", "192.0.2.0/24"),
        // IPv4 networks and the IPv6 networks of the same integers are of other families.
        ("::/96", "0.0.0.0/8"),
        ("0.0.0.0/0", "::/96"),
        ("::ffff:0:0/96", "10.0.0.0/8"),
    ] {
        for (rule, path) in [
            (
                json!({"to": [{"cidr": cidr, "except": [exception]}]}),
                "allow[0].to[0].except[0]",
            ),
            (
                json!({"cidr": cidr, "except": [cidr, exception]}),
                "allow[0].except[1]",
            ),
        ] {
            assert_refused(self::rule("allow", rule), Names::Field(path), OUTSIDE);
        }
    }
    // An exception may be its whole network, which leaves nothing.
    let whole = json!({"cidr": "192.0.2.0/24", "except": ["192.0.2.0/24"], "protocol": "tcp"});
    assert_compiles(rule("allow", whole), &[], &[]);
    let half = json!({"to": [{"cidr": "192.0.2.0/24", "except": ["192.0.2.128/25"]}]});
    assert_compiles(rule("deny", half), &[], &["192.0.2.0/25"]);
}

#[test]
fn protocols_are_tcp_udp_icmp_or_any() {
    let cidr = "192.0.2.0/24";
    for protocol in [
        json!(null),
        json!("TCP"),
        json!("Udp"),
        json!("sctp"),
        json!(""),
        json!(6),
        json!(true),
        json!(["tcp"]),
    ] {
        for (rule, path) in [
            (
                json!({"ports": [{"protocol": protocol}]}),
                "deny[0].ports[0].protocol",
            ),
            (
                json!({"cidr": cidr, "protocol": protocol}),
                "deny[0].protocol",
            ),
        ] {
            assert_refused(self::rule("deny", rule), Names::Field(path), NOT_A_PROTOCOL);
        }
    }
    for (protocol, native) in [
        (Some("tcp"), "192.0.2.0/24:tcp"),
        (Some("udp"), "192.0.2.0/24:udp"),
        (Some("icmp"), "192.0.2.0/24:icmp"),
        (Some("any"), "192.0.2.0/24"),
        // A port selector selects any protocol by default.
        (None, "192.0.2.0/24"),
    ] {
        let selector = match protocol {
            Some(protocol) => json!({"protocol": protocol}),
            None => json!({}),
        };
        let mxc = json!({"to": [{"cidr": cidr}], "ports": [selector]});
        assert_compiles(rule("allow", mxc), &[native], &[]);
    }
}

#[test]
fn ports_need_tcp_udp_or_any() {
    let cidr = "192.0.2.0/24";
    for (rule, path, reason) in [
        (
            json!({"ports": [{"protocol": "icmp", "port": 8}]}),
            "allow[0].ports[0].port",
            ICMP_PORT,
        ),
        (
            json!({"ports": [{"protocol": "tcp"}, {"protocol": "icmp", "port": 8, "endPort": 9}]}),
            "allow[0].ports[1].port",
            ICMP_PORT,
        ),
        (
            json!({"cidr": cidr, "protocol": "icmp", "port": 8}),
            "allow[0].port",
            ICMP_PORT,
        ),
        // A flat rule without a protocol selects every protocol, and so no port.
        (
            json!({"cidr": cidr, "port": 80}),
            "allow[0].port",
            NEEDS_PROTOCOL,
        ),
    ] {
        assert_refused(self::rule("allow", rule), Names::Field(path), reason);
    }
    for (protocol, natives) in [
        ("tcp", &["192.0.2.0/24:tcp:443"][..]),
        ("udp", &["192.0.2.0/24:udp:443"]),
        ("any", &["192.0.2.0/24:tcp:443", "192.0.2.0/24:udp:443"]),
    ] {
        let flat = json!({"cidr": cidr, "protocol": protocol, "port": 443});
        assert_compiles(rule("deny", flat), &[], natives);
        let mxc = json!({"to": [{"cidr": cidr}], "ports": [{"protocol": protocol, "port": 443}]});
        assert_compiles(rule("deny", mxc), &[], natives);
    }
    let default = json!({"to": [{"cidr": cidr}], "ports": [{"port": 443}]});
    assert_compiles(
        rule("allow", default),
        &["192.0.2.0/24:tcp:443", "192.0.2.0/24:udp:443"],
        &[],
    );
}

#[test]
fn ports_are_integers_from_1_to_65535() {
    let nines = "9".repeat(64);
    let negative_nines = format!("-{nines}");
    let nines_65 = "9".repeat(65);
    for (port, reason) in [
        ("0", OUT_OF_RANGE),
        ("-1", OUT_OF_RANGE),
        ("65536", OUT_OF_RANGE),
        ("70000", OUT_OF_RANGE),
        (nines.as_str(), OUT_OF_RANGE),
        (negative_nines.as_str(), OUT_OF_RANGE),
        (nines_65.as_str(), OUT_OF_RANGE),
        ("1e30", OUT_OF_RANGE),
        ("true", NOT_AN_INTEGER),
        ("false", NOT_AN_INTEGER),
        ("null", NOT_AN_INTEGER),
        (r#""80""#, NOT_AN_INTEGER),
        ("80.0", NOT_AN_INTEGER),
        ("8e1", NOT_AN_INTEGER),
        ("[80]", NOT_AN_INTEGER),
        ("{}", NOT_AN_INTEGER),
        // JSON reads `-0` as a double, as it reads numbers with a fraction or an exponent.
        ("-0", NOT_AN_INTEGER),
    ] {
        for (text, path) in [
            (
                format!(r#"{{"allow": [{{"ports": [{{"port": {port}}}]}}]}}"#),
                "allow[0].ports[0].port",
            ),
            (
                format!(r#"{{"allow": [{{"ports": [{{"port": 1, "endPort": {port}}}]}}]}}"#),
                "allow[0].ports[0].endPort",
            ),
            (
                format!(r#"{{"allow": [{{"cidr": "::/0", "protocol": "udp", "port": {port}}}]}}"#),
                "allow[0].port",
            ),
        ] {
            assert_refused(text, Names::Field(path), reason);
        }
    }
    for (port, native) in [(1, "::/0:tcp:1"), (65535, "::/0:tcp:65535")] {
        let flat = json!({"cidr": "::/0", "protocol": "tcp", "port": port});
        assert_compiles(rule("allow", flat), &[native], &[]);
    }
}

#[test]
fn end_ports_follow_their_ports() {
    let cidr = "192.0.2.0/24";
    for (rule, path, reason) in [
        (
            json!({"ports": [{"endPort": 80}]}),
            "deny[0].ports[0].endPort",
            NEEDS_PORT,
        ),
        (
            json!({"ports": [{"protocol": "udp", "endPort": 80}]}),
            "deny[0].ports[0].endPort",
            NEEDS_PORT,
        ),
        (
            json!({"ports": [{"protocol": "icmp", "endPort": 80}]}),
            "deny[0].ports[0].endPort",
            NEEDS_PORT,
        ),
        (
            json!({"cidr": cidr, "endPort": 80}),
            "deny[0].endPort",
            NEEDS_PORT,
        ),
        (
            json!({"cidr": cidr, "protocol": "tcp", "endPort": 80}),
            "deny[0].endPort",
            NEEDS_PORT,
        ),
        (
            json!({"cidr": cidr, "protocol": "icmp", "endPort": 80}),
            "deny[0].endPort",
            NEEDS_PORT,
        ),
        (
            json!({"ports": [{"protocol": "tcp", "port": 81, "endPort": 80}]}),
            "deny[0].ports[0].endPort",
            BELOW_PORT,
        ),
        (
            json!({"cidr": cidr, "protocol": "any", "port": 81, "endPort": 80}),
            "deny[0].endPort",
            BELOW_PORT,
        ),
    ] {
        assert_refused(self::rule("deny", rule), Names::Field(path), reason);
    }
    // A range of one port is that port, and ranges run up to the last port.
    for (port, end_port, native) in [
        (80, 80, "192.0.2.0/24:tcp:80"),
        (80, 81, "192.0.2.0/24:tcp:80-81"),
        (1, 65535, "192.0.2.0/24:tcp:1-65535"),
    ] {
        let flat = json!({"cidr": cidr, "protocol": "tcp", "port": port, "endPort": end_port});
        assert_compiles(rule("allow", flat), &[native], &[]);
    }
}

#[test]
fn destinations_and_ports_are_nonempty_arrays() {
    for (rule, path, reason) in [
        (json!({"to": []}), "allow[0].to", EMPTY),
        (
            json!({"to": {"cidr": "192.0.2.0/24"}}),
            "allow[0].to",
            NOT_AN_ARRAY,
        ),
        (json!({"to": null}), "allow[0].to", NOT_AN_ARRAY),
        (
            json!({"to": ["192.0.2.0/24"]}),
            "allow[0].to[0]",
            NOT_AN_OBJECT,
        ),
        (json!({"ports": []}), "allow[0].ports", EMPTY),
        (
            json!({"ports": {"port": 80}}),
            "allow[0].ports",
            NOT_AN_ARRAY,
        ),
        (json!({"ports": "tcp"}), "allow[0].ports", NOT_AN_ARRAY),
        (
            json!({"ports": ["tcp"]}),
            "allow[0].ports[0]",
            NOT_AN_OBJECT,
        ),
        (
            json!({"ports": [{"protocol": "tcp"}, 80]}),
            "allow[0].ports[1]",
            NOT_AN_OBJECT,
        ),
    ] {
        assert_refused(self::rule("allow", rule), Names::Field(path), reason);
    }
    // Without `to`, a rule selects every destination of both families; without `ports`, every
    // protocol and port.
    let every_destination = json!({"ports": [{"protocol": "udp", "port": 53}]});
    assert_compiles(
        rule("allow", every_destination),
        &["0.0.0.0/0:udp:53", "::/0:udp:53"],
        &[],
    );
    let every_port = json!({"to": [{"cidr": "192.0.2.0/24"}]});
    assert_compiles(rule("allow", every_port), &["192.0.2.0/24"], &[]);
}

#[test]
fn each_action_lowers_to_at_most_256_native_rules() {
    // Odd addresses, which no two rules merge into one network.
    let hosts = |count: usize| -> Vec<Value> {
        (0..count)
            .map(|index| json!({"cidr": format!("10.{}.0.{}", index / 128, 2 * (index % 128) + 1)}))
            .collect()
    };
    for category in ["allow", "deny"] {
        let at_the_limit = compile(json!({category: hosts(MAX_RULES_PER_ACTION)}).to_string());
        let compiled = at_the_limit.unwrap();
        assert_eq!(
            compiled.allow.len() + compiled.deny.len(),
            MAX_RULES_PER_ACTION
        );
        let over = json!({category: hosts(MAX_RULES_PER_ACTION + 1)}).to_string();
        assert_refused(over, Names::Field(category), TOO_MANY_RULES);
    }
    // Both families share the limit.
    let ports = |count: u16| -> Vec<Value> {
        (0..count)
            .map(|index| json!({"protocol": "tcp", "port": 2 * index + 1}))
            .collect()
    };
    let families = |ipv4: u16, ipv6: u16| {
        json!({"deny": [
            {"to": [{"cidr": "192.0.2.1"}], "ports": ports(ipv4)},
            {"to": [{"cidr": "2001:db8::1"}], "ports": ports(ipv6)},
        ]})
        .to_string()
    };
    assert_eq!(compile(families(128, 128)).unwrap().deny.len(), 256);
    assert_refused(families(129, 128), Names::Field("deny"), TOO_MANY_RULES);
    assert_refused(families(128, 129), Names::Field("deny"), TOO_MANY_RULES);
}

#[test]
fn unreadable_files_fail_with_an_io_error() {
    let directory = tempfile::tempdir().unwrap();
    for path in [
        directory.path().join("missing.json"),
        directory.path().to_path_buf(),
    ] {
        let error = compile_policy_file(&path).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Io, "{error}");
        let message = error.message();
        assert!(message.contains(UNREADABLE), "{message}");
        assert!(message.contains(&path.display().to_string()), "{message}");
    }
}

#[test]
fn files_are_at_most_1_mib() {
    let valid = br#"{"allow": [{"cidr": "192.0.2.0/24"}]}"#;
    let padded = |size: usize| {
        let mut data = valid.to_vec();
        data.resize(size, b' ');
        data
    };
    assert_compiles(padded(MAX_POLICY_FILE_SIZE), &["192.0.2.0/24"], &[]);
    assert_refused(padded(MAX_POLICY_FILE_SIZE + 1), Names::File, TOO_LARGE);
    // The limit applies before the contents are read.
    assert_refused(vec![0xff; MAX_POLICY_FILE_SIZE + 1], Names::File, TOO_LARGE);
}

#[test]
fn files_are_utf8() {
    for data in [
        &b"{\"allow\": [\xff]}"[..],
        b"{\"allow\": [{\"cidr\": \"192.0.2.0/24\xfe\"}]}",
        b"{\"allow\": [{\"cidr\": \"\xc0\xaf\"}]}",
        b"{\"allow\": [{\"cidr\": \"\xed\xa0\x80\"}]}",
        b"{\"\xe2\x82\": []}",
        b"\xff\xfe{\x00}\x00",
    ] {
        assert_refused(data, Names::File, NOT_UTF8);
    }
    // A byte-order mark is UTF-8, but not JSON.
    assert_refused(b"\xef\xbb\xbf{\"allow\": []}", Names::File, NOT_JSON);
    // Text that is not ASCII is read, and refused where the format expects ASCII.
    let accented = r#"{"allow": [{"cidr": "192.0.2.0/24é"}]}"#;
    assert_refused(accented, Names::Field("allow[0].cidr"), NOT_A_CIDR);
}

#[test]
fn files_are_json() {
    let digits = format!(
        r#"{{"allow": [{{"ports": [{{"port": {}}}]}}]}}"#,
        "9".repeat(400)
    );
    for text in [
        "",
        " \t\r\n",
        "{",
        r#"{"allow": []"#,
        r#"{"allow": []} {}"#,
        "{}{}",
        r#"{"allow": [],}"#,
        r#"{"allow": [{"cidr": "192.0.2.0/24"},]}"#,
        r#"{"allow": []} // comment"#,
        "{'allow': []}",
        "{allow: []}",
        r#"{"allow" []}"#,
        r#"{"allow": [] "deny": []}"#,
        r#"{"allow": nullx}"#,
        "{\"allow\": [{\"cidr\": \"192.0.2.0/24\t\"}]}",
        r#"{"allow": [{"cidr": "\x41"}]}"#,
        r#"{"allow": [{"cidr": "\u41"}]}"#,
        "\u{c}{\"allow\": []}",
        "{\"allow\": []}\u{a0}",
        // Strict JSON has no constants for doubles that are not numbers, no doubles beyond their
        // range, and no unpaired surrogates.
        r#"{"allow": [{"ports": [{"port": NaN}]}]}"#,
        r#"{"allow": [{"ports": [{"port": Infinity}]}]}"#,
        r#"{"allow": [{"ports": [{"port": -Infinity}]}]}"#,
        r#"{"allow": [{"ports": [{"port": 1e400}]}]}"#,
        &digits,
        r#"{"allow": [{"cidr": "\ud800"}]}"#,
        r#"{"\udc00": []}"#,
    ] {
        assert_refused(text, Names::File, NOT_JSON);
    }
    // Escapes are read before names are compared.
    assert_compiles(
        r#"{"\u0061llow": [{"\u0063idr": "192.0.2.0\/24"}]}"#,
        &["192.0.2.0/24"],
        &[],
    );
}

#[test]
fn arrays_and_objects_nest_at_most_32_deep() {
    // The limit counts the root.
    let nested = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    let objects = |depth: usize| format!("{}1{}", r#"{"a": "#.repeat(depth), "}".repeat(depth));
    // Up to the limit, JSON is read, and here refused by the format.
    assert_refused(
        nested(MAX_JSON_DEPTH),
        Names::Field("egress policy"),
        NOT_AN_OBJECT,
    );
    let action = format!(r#"{{"allow": {}}}"#, nested(MAX_JSON_DEPTH - 1));
    assert_refused(action, Names::Field("allow[0]"), NOT_AN_OBJECT);
    assert_refused(
        objects(MAX_JSON_DEPTH),
        Names::Field("egress policy"),
        UNKNOWN_FIELD,
    );
    // Beyond it, JSON is refused, also far beyond the depth that a stack would allow.
    for depth in [MAX_JSON_DEPTH + 1, 100_000] {
        assert_refused(nested(depth), Names::File, TOO_DEEP);
        let action = format!(r#"{{"allow": {}}}"#, nested(depth));
        assert_refused(action, Names::File, TOO_DEEP);
        assert_refused(objects(depth), Names::File, TOO_DEEP);
    }
}
