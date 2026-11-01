//! Reachability tests for IPv4, IPv6, and wildcard addresses.

use super::*;
use crate::guest::ingress::PROTOCOL_UDP;

/// Builds an address context for one test policy.
fn context<'a>(ipv6_dual_stack: bool, wildcard_tcp: &'a [WildcardListener]) -> AddressContext<'a> {
    AddressContext {
        ipv6_dual_stack,
        wildcard_tcp,
    }
}

/// Formats an IP address as Linux procfs renders its native-endian words.
fn kernel_address(address: std::net::IpAddr) -> String {
    let octets = match address {
        std::net::IpAddr::V4(address) => address.octets().to_vec(),
        std::net::IpAddr::V6(address) => address.octets().to_vec(),
    };
    octets
        .chunks(4)
        .map(|word| format!("{:08X}", u32::from_ne_bytes(word.try_into().unwrap())))
        .collect()
}

/// Verifies strict word parsing and IPv4 loopback exclusion.
#[test]
fn ipv4_requires_eight_hex_digits_and_excludes_loopback() {
    let context = context(true, &[]);
    assert_eq!(
        context.reachable_families(b"00000000", false, PROTOCOL_TCP, 80),
        FAMILY_IPV4
    );
    assert_eq!(
        context.reachable_families(b"010200C0", false, PROTOCOL_TCP, 80),
        FAMILY_IPV4
    );
    assert_eq!(
        context.reachable_families(b"0100007F", false, PROTOCOL_TCP, 80),
        0
    );
    // The kernel never signs or prefixes a word, as C's `strtoul` would have accepted.
    for malformed in [
        b"0".as_slice(),
        b"000000000",
        b"0000000g",
        b"-0000001",
        b"+0000001",
        b"0x000001",
        b" 0000001",
    ] {
        assert_eq!(
            context.reachable_families(malformed, false, PROTOCOL_TCP, 80),
            0,
            "{}",
            malformed.escape_ascii()
        );
    }
}

/// Verifies unreachable IPv6 classes and mapped-address family selection.
#[test]
fn ipv6_excludes_loopback_link_local_and_mapped_loopback() {
    let context = context(true, &[]);
    for excluded in ["::1", "fe80::1", "febf::abcd", "::ffff:127.0.0.1"] {
        let address = kernel_address(excluded.parse().unwrap());
        assert_eq!(
            context.reachable_families(address.as_bytes(), true, PROTOCOL_TCP, 80),
            0,
            "{excluded}"
        );
    }
    let mapped = kernel_address("::ffff:192.0.2.2".parse().unwrap());
    assert_eq!(
        context.reachable_families(mapped.as_bytes(), true, PROTOCOL_TCP, 80),
        FAMILY_IPV4
    );
    let specific = kernel_address("fd00::a00:2".parse().unwrap());
    assert_eq!(
        context.reachable_families(specific.as_bytes(), true, PROTOCOL_TCP, 80),
        FAMILY_IPV6
    );
    let mut signed_word = [b'0'; 32];
    signed_word[..8].copy_from_slice(b"-0000001");
    assert_eq!(
        context.reachable_families(&signed_word, true, PROTOCOL_TCP, 80),
        0
    );
}

/// Verifies that TCP diagnostics override the global wildcard default only per port.
#[test]
fn wildcard_tcp_uses_diagnostics_then_the_sysctl_default() {
    let listeners = [
        WildcardListener {
            port: 80,
            dual_stack: false,
            ipv6_only: true,
        },
        WildcardListener {
            port: 81,
            dual_stack: true,
            ipv6_only: true,
        },
    ];
    let wildcard = kernel_address("::".parse().unwrap());
    let context = context(false, &listeners);
    assert_eq!(
        context.reachable_families(wildcard.as_bytes(), true, PROTOCOL_TCP, 80),
        FAMILY_IPV6
    );
    assert_eq!(
        context.reachable_families(wildcard.as_bytes(), true, PROTOCOL_TCP, 81),
        FAMILY_IPV4 | FAMILY_IPV6
    );
    assert_eq!(
        context.reachable_families(wildcard.as_bytes(), true, PROTOCOL_TCP, 82),
        FAMILY_IPV6
    );
    assert_eq!(
        context.reachable_families(wildcard.as_bytes(), true, PROTOCOL_UDP, 81),
        FAMILY_IPV6
    );
}
