//! Selection and merging of wildcard listeners from encoded dumps.

use std::net::Ipv6Addr;

use super::*;

/// Linux's `TCP_ESTABLISHED` state number, of a socket that is not listening.
const TCP_ESTABLISHED: u8 = 1;

/// Encodes the dump message of an IPv6 TCP listener on `address` and `port`.
fn listener(address: &str, port: u16, ipv6_only: bool) -> Vec<u8> {
    sys::tests::encode_inet_diag_socket(TCP_LISTEN as u8, address.parse().unwrap(), port, ipv6_only)
}

/// Verifies same-port merging, address and state filtering, and DONE termination.
#[test]
fn diagnostics_merge_dual_stack_and_stop_at_done() {
    let mut dump = listener("::", 7001, true);
    dump.extend(listener("::", 7001, false));
    dump.extend(listener("fd00::a00:2", 7002, true));
    dump.extend(sys::tests::encode_inet_diag_socket(
        TCP_ESTABLISHED,
        Ipv6Addr::UNSPECIFIED,
        7004,
        true,
    ));
    dump.extend(sys::tests::encode_inet_diag_done());
    dump.extend(listener("::", 7003, true));
    let mut listeners = Vec::new();
    assert!(!parse_wildcard_tcp(&dump, &mut listeners));
    assert_eq!(
        listeners,
        vec![WildcardListener {
            port: 7001,
            dual_stack: true,
            ipv6_only: true,
        }]
    );
}

/// Verifies that the tracking bound caps new ports but still merges into tracked ones.
#[test]
fn tracking_stops_at_the_bound_but_merges_tracked_ports() {
    let mut dump = Vec::new();
    for port in 0..=MAX_TRACKED as u16 {
        dump.extend(listener("::", 10_000 + port, true));
    }
    dump.extend(listener("::", 10_000, false));
    let mut listeners = Vec::new();
    assert!(parse_wildcard_tcp(&dump, &mut listeners));
    assert_eq!(listeners.len(), MAX_TRACKED);
    assert!(
        listeners
            .iter()
            .all(|listener| listener.port != 10_000 + MAX_TRACKED as u16)
    );
    assert_eq!(
        listeners[0],
        WildcardListener {
            port: 10_000,
            dual_stack: true,
            ipv6_only: true,
        }
    );
}
