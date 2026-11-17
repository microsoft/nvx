//! Scans the four socket tables, settles UDP sockets, and builds an announcement.

use std::path::Path;

use super::address::AddressContext;
use super::diag::{self, WildcardListener};
use super::encode::{self, ListenerTable};
use super::{
    DecimalFile, PATH_BUFFER_SIZE, PROTOCOL_TCP, PROTOCOL_UDP, UDP_SETTLE_MS, UdpSocket,
    bounded_path, open_read_only, read_decimal_file,
};

/// The kernel's socket tables below the procfs root, each with the protocol and whether its
/// sockets are IPv6 ones, in the order in which the C program scanned them.
///
/// The order decides which rows fill the scan's 1,024-entry bounds first when a guest has more
/// sockets than that, so it stays the C program's: TCP before UDP, and IPv4 before IPv6.
const SOCKET_TABLES: [(&str, u8, bool); 4] = [
    ("/net/tcp", PROTOCOL_TCP, false),
    ("/net/tcp6", PROTOCOL_TCP, true),
    ("/net/udp", PROTOCOL_UDP, false),
    ("/net/udp6", PROTOCOL_UDP, true),
];

/// The `net.ipv6.bindv6only` sysctl below the procfs root, which decides whether a UDP socket on
/// the unspecified IPv6 address also accepts IPv4. The guest kernel's socket diagnostics do not
/// cover UDP sockets, so the sysctl is all that the program knows of them.
const BINDV6ONLY_FILE: &str = "/sys/net/ipv6/bindv6only";

/// Whether a socket on the unspecified IPv6 address also accepts IPv4 when `net.ipv6.bindv6only`
/// cannot be read: Linux's default for the sysctl is 0, under which it does, and the C program
/// assumed that default.
pub(super) const IPV6_DUAL_STACK_BY_DEFAULT: bool = true;

/// Source of socket-diagnostics bytes for one collection.
pub(super) enum DiagnosticInput {
    /// Query the running kernel.
    Live,
    /// Read the optional `diag` snapshot file.
    Snapshot,
}

/// Remembered settling state of one UDP socket identity.
#[derive(Clone, Copy)]
struct UdpState {
    /// Socket port.
    port: u16,
    /// Whether this identity has already satisfied the settling delay.
    announced: bool,
    /// Kernel identity of the socket.
    inode: u64,
    /// First monotonic scan time at which this identity appeared unconnected.
    first_seen_ms: u64,
}

/// Stateful collector whose UDP identities persist across scans.
pub(super) struct Collector {
    /// UDP identities retained from the preceding scan.
    udp_states: Vec<UdpState>,
}

/// Transient listeners and UDP sockets found during one scan.
struct ScanState<'a> {
    /// Reachability policy and socket-diagnostics results.
    addresses: AddressContext<'a>,
    /// Canonical listener table under construction.
    listeners: ListenerTable,
    /// Reachable UDP socket identities awaiting settlement.
    udp_sockets: Vec<UdpSocket>,
}

impl<'a> ScanState<'a> {
    /// Creates an empty scan with the current IPv6 wildcard policy.
    fn new(ipv6_dual_stack: bool, wildcard_tcp: &'a [WildcardListener]) -> Self {
        Self {
            addresses: AddressContext {
                ipv6_dual_stack,
                wildcard_tcp,
            },
            listeners: ListenerTable::default(),
            udp_sockets: Vec::new(),
        }
    }

    /// Scans one procfs socket table into this transient state.
    fn scan(&mut self, reader: impl std::io::Read, protocol: u8, ipv6: bool, own_port: u16) {
        super::table::scan(
            reader,
            protocol,
            ipv6,
            own_port,
            &self.addresses,
            &mut self.listeners,
            &mut self.udp_sockets,
        );
    }
}

impl Collector {
    /// Creates a collector with no remembered UDP sockets.
    pub(super) fn new() -> Self {
        Self {
            udp_states: Vec::new(),
        }
    }

    /// Reads all available tables below `root` and returns one encoded announcement.
    pub(super) fn collect_files(
        &mut self,
        root: &Path,
        diagnostics: DiagnosticInput,
        own_port: u16,
        now: u64,
        ipv6_dual_stack: bool,
    ) -> Vec<u8> {
        let wildcard_tcp = match diagnostics {
            DiagnosticInput::Live => diag::query_live(),
            DiagnosticInput::Snapshot => diag::query_snapshot(root),
        };
        let mut scan = ScanState::new(ipv6_dual_stack, &wildcard_tcp);
        for (table, protocol, ipv6) in SOCKET_TABLES {
            let Some(path) = bounded_path(root, table, PATH_BUFFER_SIZE) else {
                continue;
            };
            let Ok(file) = open_read_only(&path) else {
                continue;
            };
            scan.scan(file, protocol, ipv6, own_port);
        }
        self.finish(scan, now)
    }

    /// Settles the scan's UDP identities and encodes the resulting listener table.
    fn finish(&mut self, mut scan: ScanState<'_>, now: u64) -> Vec<u8> {
        let mut next = Vec::with_capacity(scan.udp_sockets.len());
        for socket in scan.udp_sockets {
            let previous = self
                .udp_states
                .iter()
                .find(|state| state.port == socket.port && state.inode == socket.inode);
            let first_seen_ms = previous.map_or(now, |state| state.first_seen_ms);
            let announced = if previous.is_some_and(|state| state.announced) {
                true
            } else if socket.connected {
                continue;
            } else {
                now.wrapping_sub(first_seen_ms) >= UDP_SETTLE_MS
            };
            next.push(UdpState {
                port: socket.port,
                announced,
                inode: socket.inode,
                first_seen_ms,
            });
            if announced {
                scan.listeners
                    .add(PROTOCOL_UDP, socket.port, socket.families);
            }
        }
        self.udp_states = next;
        encode::encode(scan.listeners)
    }
}

/// Reads `bindv6only`, returning whether unspecified IPv6 UDP sockets also accept IPv4, which
/// they do when it is 0, or `default` when it cannot be read or holds anything but a number.
pub(super) fn read_bindv6only(root: &Path, default: bool) -> bool {
    let Some(path) = bounded_path(root, BINDV6ONLY_FILE, PATH_BUFFER_SIZE) else {
        return default;
    };
    match read_decimal_file(&path) {
        DecimalFile::Value(value) => value == 0,
        DecimalFile::Missing | DecimalFile::Malformed => default,
    }
}

#[cfg(test)]
mod tests;
