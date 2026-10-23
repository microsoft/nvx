//! `nvx-rs egress compile PATH`, which prints, as a JSON object, the native OpenVMM rules into
//! which the library lowers an egress policy file, each list in the order in which OpenVMM receives
//! them, or fails with the library's refusal of the file.

use std::path::Path;

use nvx::host::network::policy::compile_policy_file;
use serde_json::{Value, json};

use crate::harness::Root;

/// The allow and deny rules, in OpenVMM's form, into which an egress policy file compiles.
type Rules = (&'static [&'static str], &'static [&'static str]);

/// The code of the error with which the library refuses an egress policy file, and what the error
/// names.
type Refusal = (&'static str, &'static [&'static str]);

/// An egress policy file, and what `egress compile` prints for it.
struct Compilation {
    /// The name of the case.
    name: &'static str,
    /// The content of `policy.json`, which the case writes below the root, if any.
    policy: Option<&'static str>,
    /// The path that `egress compile` compiles, with placeholders.
    path: &'static str,
    /// The rules into which the file compiles, or the library's refusal of it.
    expected: Result<Rules, Refusal>,
}

/// The policy files that `egress compile` compiles, or refuses.
const COMPILATIONS: [Compilation; 9] = [
    Compilation {
        name: "empty",
        policy: Some("{}"),
        path: "{root/policy.json}",
        expected: Ok((&[], &[])),
    },
    Compilation {
        name: "allow",
        policy: Some(r#"{"allow": [{"cidr": "192.0.2.0/24"}]}"#),
        path: "{root/policy.json}",
        expected: Ok((&["192.0.2.0/24"], &[])),
    },
    // An exclusion carves a range out of its rule, and a rule may give its destinations and
    // ports in lists.
    Compilation {
        name: "both",
        policy: Some(
            r#"{"allow": [{"cidr": "192.0.2.0/24", "except": ["192.0.2.128/25"]}, {"to": [{"cidr": "2001:db8::/32"}], "ports": [{"protocol": "tcp", "port": 80, "endPort": 81}]}], "deny": [{"cidr": "198.51.100.0/24", "protocol": "icmp"}]}"#,
        ),
        path: "{root/policy.json}",
        expected: Ok((
            &["192.0.2.0/25", "2001:db8::/32:tcp:80-81"],
            &["198.51.100.0/24:icmp"],
        )),
    },
    Compilation {
        name: "invalid JSON",
        policy: Some("{"),
        path: "{root/policy.json}",
        expected: Err(("unsupported", &["egress policy file", "{root/policy.json}"])),
    },
    Compilation {
        name: "invalid rule",
        policy: Some(r#"{"allow": [{"cidr": "192.0.2.0/24", "port": 70000}]}"#),
        path: "{root/policy.json}",
        expected: Err(("unsupported", &["allow[0].port"])),
    },
    Compilation {
        name: "unknown field",
        policy: Some(r#"{"allow": [], "other": []}"#),
        path: "{root/policy.json}",
        expected: Err(("unsupported", &["unknown field", "'other'"])),
    },
    Compilation {
        name: "missing file",
        policy: None,
        path: "{root/missing.json}",
        expected: Err(("io", &["egress policy file", "{root/missing.json}"])),
    },
    Compilation {
        name: "directory",
        policy: None,
        path: "{root/share}",
        expected: Err(("io", &["egress policy file", "{root/share}"])),
    },
    // A scoped IPv6 range is refused, also one whose address has host bits that normalization
    // would drop with the scope (microsoft/nvx#462).
    Compilation {
        name: "scoped IPv6 range",
        policy: Some(r#"{"allow": [{"cidr": "fe80::1%eth0/64"}]}"#),
        path: "{root/policy.json}",
        expected: Err(("unsupported", &["allow[0].cidr", "scope"])),
    },
];

#[test]
fn egress_compile_prints_the_library_lowering() {
    let root = Root::new();
    for case in &COMPILATIONS {
        if let Some(policy) = case.policy {
            root.write("policy.json", policy);
        }
        let outcome = root.nvx_rs(&["egress", "compile", case.path]);
        let path = root.expand(case.path);
        let lowered = compile_policy_file(Path::new(&path));
        eprintln!("case: {}", case.name);
        match case.expected {
            Ok((allow, deny)) => {
                outcome.assert_success();
                let printed: Value = serde_json::from_slice(&outcome.stdout).unwrap();
                assert_eq!(
                    printed,
                    json!({"allow": allow, "deny": deny}),
                    "{}",
                    case.name
                );
                let lowered = lowered.unwrap();
                assert_eq!(
                    printed,
                    json!({"allow": lowered.allow, "deny": lowered.deny}),
                    "{}",
                    case.name
                );
            }
            Err((code, fragments)) => {
                let fragments: Vec<String> = fragments
                    .iter()
                    .map(|fragment| root.expand(fragment))
                    .collect();
                let fragments: Vec<&str> = fragments.iter().map(String::as_str).collect();
                let message = outcome.assert_error(code, &fragments);
                let refusal = lowered.unwrap_err();
                assert_eq!(refusal.code().as_str(), code, "{}", case.name);
                assert_eq!(message, refusal.message(), "{}", case.name);
            }
        }
    }
}
