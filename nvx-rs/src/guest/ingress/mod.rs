//! Announces the guest's TCP and UDP listeners to the gateway of the portable
//! network profile, which publishes them to the host's private networks or,
//! for host-loopback ingress alone, on host loopback. The init system starts
//! it, as root, when the kernel command line carries `virtnet_ingress=allow`
//! or `virtnet_ingress=host-loopback`.
//!
//! A clone of a template restores the template's NIC, which the template
//! captured under a policy that publishes nothing, so its init started no
//! announcer, with a policy of its own, which may publish the clone's
//! listeners. The clone's application then starts the announcer itself, with
//! [`Announcer::start`]: as root, it binds the announcer's UDP socket to a
//! privileged source port, and runs `nvx-ingress inherit GATEWAY_IPV4`, which
//! announces from that socket, its standard input, rather than from one of its
//! own. A failure to bind the socket, or to run the program, is the start's
//! own, before the clone is ready, and the caller watches the announcer that
//! started, and must end the clone once it ended: the gateway then keeps
//! publishing the listeners that it last announced, and learns of no later
//! change of them.
//!
//! Every 100 ms it reads the kernel's socket tables. A TCP socket in the
//! LISTEN state is a listener, and so is an unconnected UDP socket that
//! persists for a second, which skips short-lived client sockets, such as
//! those of DNS queries. Each UDP socket settles on its own, identified by the
//! kernel's inode for it, and stays announced while it exists, even once a
//! server connects it to its client; another socket that reuses a port, such
//! as a client's, settles anew, and a port announces the families of its
//! announced sockets. Sockets bound to loopback addresses, or to IPv6
//! link-local ones, are unreachable through the gateway. A TCP listener on
//! the unspecified IPv6 address also takes IPv4 connections unless the
//! kernel's socket diagnostics report it IPv6-only; a UDP socket there, which
//! the guest kernel's diagnostics do not cover, follows
//! `net.ipv6.bindv6only`. It sends the canonical set of listeners to UDP port
//! 1021 of the IPv4 gateway from a privileged port, which only root can bind,
//! whenever the set changes and at least every two seconds.
//!
//! An announcement is the magic `NVXL`, version 1, a flags byte whose bit 0
//! marks omitted listeners, a big-endian count, and that many entries in
//! increasing order of protocol and port: the IP protocol number, a family
//! mask (1 for IPv4, 2 for IPv6), and the big-endian port.
//!
//! `nvx-ingress test DIRECTORY` runs the numbered snapshots in `DIRECTORY`,
//! from 0, as successive scans instead, and prints each scan's time and
//! announcement in hexadecimal. A snapshot holds the socket tables under
//! `net/`, optionally `sys/net/ipv6/bindv6only`, optionally a socket
//! diagnostics dump in `diag`, and the scan time in milliseconds in `time`.
//! Like the live tables and sysctl, a snapshot's files must have the form in
//! which the kernel writes them: a row or number that the kernel would not
//! write is rejected, rather than read with the leniency of C's `scanf`.

use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::parse;
use super::status::{CommandError, run_main};
use super::sys::{self, Errno, FileDescriptor, Mode, OpenFlags};

pub use announcer::{Announcer, PROGRAM, StartError};

mod address;
mod announcer;
mod collect;
mod diag;
mod encode;
mod sender;
mod table;
mod test_mode;

/// Gateway UDP port that consumes listener announcements.
///
/// It must equal OpenVMM's `net_backend_resources::ingress::LISTENER_ANNOUNCEMENT_PORT`, the port
/// at which the portable profile's gateway takes announcements in instead of forwarding them.
/// That is 1021, the first of the two ports that RFC 4727 reserves for experiments, which no
/// registered service uses.
const ANNOUNCEMENT_PORT: u16 = 1021;

/// Highest privileged source port that the sender tries to bind, and the first that it tries.
///
/// OpenVMM accepts an announcement only from a source port below 1024, its
/// `UNPRIVILEGED_PORT_START`, which only root can bind in the guest, so that a workload without
/// that privilege cannot forge one. The sender searches downward from here to `LAST_SOURCE_PORT`,
/// as BSD's `rresvport` does, and test mode takes this port as its own, which a live sender
/// usually binds.
const FIRST_SOURCE_PORT: u16 = 1023;

/// Lowest privileged source port that the sender tries to bind, before it gives up.
///
/// Half of the 1,024 privileged ports, the bound of BSD's `rresvport`, which leaves the lower
/// half, where well-known services listen, to them. The sender needs only one free port.
const LAST_SOURCE_PORT: u16 = 512;

/// Maximum listener entries that one announcement carries; the lowest in protocol and port order
/// go, and the flags mark the rest as omitted.
///
/// It must equal OpenVMM's `MAX_ANNOUNCED_LISTENERS`, beyond which the gateway rejects the whole
/// announcement. It also bounds an announcement to 8 + 64 × 4 = 264 bytes, which travel in one
/// unfragmented datagram, the only form in which the gateway recognizes one.
const MAX_ANNOUNCED: usize = 64;

/// Maximum listeners, UDP socket identities, or wildcard IPv6 TCP ports that one scan tracks.
///
/// The C program kept each in a static array of this many entries, which bounded its memory, and
/// the linear searches that merge entries, against a guest with very many sockets. It far
/// exceeds `MAX_ANNOUNCED`, so it decides only which sockets of such a guest the scan sees, whose
/// announcement is then marked as omitting listeners.
const MAX_TRACKED: usize = 1024;

/// Delay between socket-table scans, and the receive timeout of each socket-diagnostics query.
///
/// It bounds how long a new or closed listener goes unreported, which `doc/run.md` documents as
/// 100 milliseconds, while keeping the cost of reading four procfs tables and one netlink dump
/// ten times a second small. A diagnostics dump that never ends times out after one interval,
/// so it cannot stall the scans.
const SCAN_INTERVAL_MS: u64 = 100;

/// Maximum delay between successful sends of an unchanged announcement.
///
/// Announcements are unacknowledged UDP datagrams, and the gateway retries a host listener that
/// it could not bind, such as on a port that a host process holds, only when the next
/// announcement arrives. Repeating the announcement every 2 seconds, as `doc/run.md` documents,
/// recovers from a lost datagram and retries such a listener within that time, for one small
/// datagram every 2 seconds.
const REFRESH_INTERVAL_MS: u64 = 2_000;

/// How long an unconnected UDP socket must persist before it is announced.
///
/// A client, such as a DNS resolver, binds an unconnected UDP socket for each query, which a scan
/// may catch, but which must not become a listener on the host. A server's socket persists, so
/// one second, as `doc/run.md` documents, skips short-lived client sockets at the cost of
/// announcing a UDP server about a second after it starts.
const UDP_SETTLE_MS: u64 = 1_000;

/// Family-mask bit of an announcement entry for a listener reachable over IPv4, for which
/// OpenVMM publishes an IPv4 host listener. Part of the NVXL wire format, so it must equal
/// OpenVMM's.
const FAMILY_IPV4: u8 = 1;
/// Family-mask bit of an announcement entry for a listener reachable over IPv6, for which
/// OpenVMM publishes an IPv6 host listener. Part of the NVXL wire format, so it must equal
/// OpenVMM's.
const FAMILY_IPV6: u8 = 2;
/// IANA's IP protocol number for TCP, which identifies a TCP listener in an announcement entry.
const PROTOCOL_TCP: u8 = 6;
/// IANA's IP protocol number for UDP, which identifies a UDP listener in an announcement entry.
const PROTOCOL_UDP: u8 = 17;

/// Linux's `TCP_LISTEN` socket state (`include/net/tcp_states.h`), that of a TCP socket that
/// accepts connections. The kernel renders it in the `st` column of its TCP tables, and its
/// socket diagnostics filter and report sockets by it.
const TCP_LISTEN: u32 = 0x0a;
/// Linux's `TCP_ESTABLISHED` socket state, which the kernel reuses, in the `st` column of its UDP
/// tables, for a UDP socket that `connect` gave a peer.
const UDP_CONNECTED: u32 = 0x01;
/// Linux's `TCP_CLOSE` socket state, which the kernel reuses, in the `st` column of its UDP
/// tables, for a UDP socket without a peer, as a server's is.
const UDP_UNCONNECTED: u32 = 0x07;

/// Size of the C program's path buffers, `char path[4096]`, which is Linux's `PATH_MAX`.
///
/// The C program formatted each path that it opened into such a buffer with `snprintf`, and
/// skipped the file when the path did not fit. The port compares the same byte lengths, so that
/// it skips the same files.
const PATH_BUFFER_SIZE: usize = 4096;

/// One canonical protocol/port listener entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Listener {
    /// IP protocol number.
    protocol: u8,
    /// Bit mask of reachable address families.
    families: u8,
    /// Guest listener port.
    port: u16,
}

/// One reachable UDP socket observed in a kernel table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UdpSocket {
    /// Guest socket port.
    port: u16,
    /// Bit mask of reachable address families.
    families: u8,
    /// Whether the kernel table shows a peer for the socket.
    connected: bool,
    /// Kernel identity retained for the lifetime of this socket.
    inode: u64,
}

/// Runs `nvx-ingress` with the process's arguments: `GATEWAY_IPV4`, the announcer, from a socket
/// of its own; `inherit GATEWAY_IPV4`, the announcer, from the socket that it inherited as its
/// standard input, which must be an IPv4 datagram socket bound to a privileged source port from
/// 512 to 1023; or `test DIRECTORY`. Test mode exits with 0 after the last snapshot, or with 1
/// when a snapshot fails. The announcer runs until it fails, and exits with 1 when it cannot open
/// or bind its socket, or take the one that it inherited, or with 2 for a usage error.
pub fn main() -> ExitCode {
    run_main(|args| run(args, &mut sys::Stdout::new(), &mut io::stderr()))
}

/// Runs test mode or the announcer, as `args` select.
fn run(args: &[CString], out: &mut dyn Write, err: &mut dyn Write) -> Result<(), CommandError> {
    let announced = match args {
        [_, mode, directory] if mode.as_bytes() == b"test" => {
            return test_mode::run(directory, out).map_err(|_| CommandError::Operation);
        }
        [_, mode, gateway] if mode.as_bytes() == b"inherit" => sender::run_inherited(gateway),
        [_, gateway] => sender::run(gateway),
        _ => return Err(usage(err)),
    };
    match announced {
        Ok(()) => Ok(()),
        Err(sender::Error::InvalidGateway) => Err(usage(err)),
        Err(error) => {
            error.report(err);
            Err(CommandError::Operation)
        }
    }
}

/// Prints the usage message and returns the usage error.
fn usage(err: &mut dyn Write) -> CommandError {
    sys::report(err, &[b"usage: nvx-ingress [inherit] GATEWAY_IPV4\n"]);
    CommandError::Usage
}

/// Binds a socket, through `bind`, to the highest free privileged source port, from
/// [`FIRST_SOURCE_PORT`] down to [`LAST_SOURCE_PORT`], and returns it, or `None` if every one of
/// them is in use. Only a port in use moves the search to the next one: any other failure of
/// `bind` is the search's.
fn bind_privileged(mut bind: impl FnMut(u16) -> Result<(), Errno>) -> Result<Option<u16>, Errno> {
    for port in (LAST_SOURCE_PORT..=FIRST_SOURCE_PORT).rev() {
        match bind(port) {
            Ok(()) => return Ok(Some(port)),
            Err(Errno::EADDRINUSE) => {}
            Err(errno) => return Err(errno),
        }
    }
    Ok(None)
}

/// Returns `root` followed by `suffix`, a path that starts with `/`, if the C program's
/// `snprintf(path, size, "%s%s", root, suffix)` fit a buffer of `size` bytes with its NUL.
///
/// It compares C's byte lengths even where `Path::join` would not double a `/` that ends `root`.
fn bounded_path(root: &Path, suffix: &str, size: usize) -> Option<PathBuf> {
    if root.as_os_str().as_bytes().len() + suffix.len() >= size {
        return None;
    }
    Some(root.join(suffix.trim_start_matches('/')))
}

/// Opens `path` for reading, close-on-exec as `std::fs::File::open` would open it, through `sys`,
/// so that every descriptor that the program uses, a file's as well as a socket's, comes from
/// `nix` and has one owner.
fn open_read_only(path: &Path) -> Result<FileDescriptor, Errno> {
    sys::open(
        &sys::path_c_string(path)?,
        OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )
}

/// The outcome of reading a file that holds one decimal number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecimalFile {
    /// The file could not be opened.
    Missing,
    /// The file could not be read, or holds anything but one decimal number.
    Malformed,
    /// The number.
    Value(u64),
}

/// Reads a file that holds one decimal number, as the kernel writes a sysctl's: its digits, then
/// a newline. White space may surround the number, but nothing else, not even a sign.
fn read_decimal_file(path: &Path) -> DecimalFile {
    let Ok(mut file) = open_read_only(path) else {
        return DecimalFile::Missing;
    };
    let mut contents = Vec::new();
    if file.read_to_end(&mut contents).is_err() {
        return DecimalFile::Malformed;
    }
    parse::parse_decimal(contents.trim_ascii()).map_or(DecimalFile::Malformed, DecimalFile::Value)
}

#[cfg(test)]
mod tests {
    //! Checks of the shared file helpers.

    use super::*;

    /// Verifies that decimal files hold exactly one plain number, and their failure outcomes.
    #[test]
    fn decimal_files_hold_one_plain_number() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("value");
        for (contents, expected) in [
            (&b"0\n"[..], DecimalFile::Value(0)),
            (b"1000\n", DecimalFile::Value(1000)),
            (b" \t42 \n", DecimalFile::Value(42)),
            (b"18446744073709551615\n", DecimalFile::Value(u64::MAX)),
            (b"18446744073709551616\n", DecimalFile::Malformed),
            (b"+7\n", DecimalFile::Malformed),
            (b"-1\n", DecimalFile::Malformed),
            (b"0x10\n", DecimalFile::Malformed),
            (b"0trailing\n", DecimalFile::Malformed),
            (b"1\n2\n", DecimalFile::Malformed),
            (b"\x0b7\n", DecimalFile::Malformed),
            (b"\n", DecimalFile::Malformed),
            (b"", DecimalFile::Malformed),
        ] {
            std::fs::write(&path, contents).unwrap();
            assert_eq!(
                read_decimal_file(&path),
                expected,
                "{}",
                contents.escape_ascii()
            );
        }
        assert_eq!(
            read_decimal_file(&directory.path().join("missing")),
            DecimalFile::Missing
        );
        assert_eq!(read_decimal_file(directory.path()), DecimalFile::Malformed);
    }
}
