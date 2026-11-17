//! Finds the IPv6 TCP listeners on the unspecified address that the kernel's socket diagnostics
//! report as IPv6-only or dual-stack, live or from a test snapshot's recorded dump.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use super::{
    MAX_TRACKED, PATH_BUFFER_SIZE, SCAN_INTERVAL_MS, TCP_LISTEN, bounded_path, open_read_only,
};
use crate::guest::sys::{self, InetDiagMessage};

/// The file of a test snapshot that holds a recorded socket-diagnostics dump, which test mode
/// decodes in place of a live query.
const DIAG_FILE: &str = "/diag";

/// Observed wildcard IPv6 TCP behavior for one port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WildcardListener {
    /// Listener port.
    pub(super) port: u16,
    /// Whether at least one socket on the port accepts IPv4.
    pub(super) dual_stack: bool,
    /// Whether at least one socket on the port is IPv6-only.
    pub(super) ipv6_only: bool,
}

/// Records the `IPV6_V6ONLY` option of one socket of a dump, if the socket is an IPv6 TCP
/// listener on the unspecified address whose message reports the option.
fn note_wildcard_tcp(message: InetDiagMessage, listeners: &mut Vec<WildcardListener>) {
    if !message.ipv6 || u32::from(message.state) != TCP_LISTEN || !message.address.is_unspecified()
    {
        return;
    }
    let Some(ipv6_only) = message.ipv6_only else {
        return;
    };
    if let Some(listener) = listeners
        .iter_mut()
        .find(|listener| listener.port == message.port)
    {
        listener.dual_stack |= !ipv6_only;
        listener.ipv6_only |= ipv6_only;
    } else if listeners.len() < MAX_TRACKED {
        listeners.push(WildcardListener {
            port: message.port,
            dual_stack: !ipv6_only,
            ipv6_only,
        });
    }
}

/// Records the wildcard IPv6 TCP listeners of one part of a dump, and returns whether the dump
/// continues after it.
pub(super) fn parse_wildcard_tcp(buffer: &[u8], listeners: &mut Vec<WildcardListener>) -> bool {
    sys::decode_inet_diag_dump(buffer, |message| note_wildcard_tcp(message, listeners))
}

/// Reads and parses a snapshot's optional recorded dump, at most `sys::INET_DIAG_BUFFER_LENGTH`
/// bytes of it, as the C program's `fread` into its dump buffer did.
pub(super) fn query_snapshot(root: &Path) -> Vec<WildcardListener> {
    let mut listeners = Vec::new();
    let Some(path) = bounded_path(root, DIAG_FILE, PATH_BUFFER_SIZE) else {
        return listeners;
    };
    let Ok(file) = open_read_only(&path) else {
        return listeners;
    };
    let mut dump = Vec::with_capacity(sys::INET_DIAG_BUFFER_LENGTH);
    let _ = file
        .take(sys::INET_DIAG_BUFFER_LENGTH as u64)
        .read_to_end(&mut dump);
    parse_wildcard_tcp(&dump, &mut listeners);
    listeners
}

/// Queries the kernel for its wildcard IPv6 TCP listeners. A failure ends the query with the
/// listeners decoded before it, if any.
///
/// Each receive of the dump waits at most one scan interval, so a dump that never ends cannot
/// stall the scans.
pub(super) fn query_live() -> Vec<WildcardListener> {
    let mut listeners = Vec::new();
    let _ = sys::dump_ipv6_tcp_sockets(
        TCP_LISTEN,
        Duration::from_millis(SCAN_INTERVAL_MS),
        |message| note_wildcard_tcp(message, &mut listeners),
    );
    listeners
}

#[cfg(test)]
mod tests;
