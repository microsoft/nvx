//! Binds the privileged UDP source port, or takes the socket that the announcer inherited bound to
//! one, and runs the live scan/send loop.

use std::ffi::CStr;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::Path;
use std::time::Duration;

use super::collect::{self, Collector, DiagnosticInput, IPV6_DUAL_STACK_BY_DEFAULT};
use super::{
    ANNOUNCEMENT_PORT, FIRST_SOURCE_PORT, LAST_SOURCE_PORT, REFRESH_INTERVAL_MS, SCAN_INTERVAL_MS,
    bind_privileged,
};
use crate::guest::sys::{self, Errno};

/// Where the running kernel exposes its socket tables and sysctls: the procfs mount point, which
/// test mode replaces with a snapshot's directory.
const PROC_ROOT: &str = "/proc";

/// Injectable boundary around live operating-system effects.
trait System {
    /// Installs the daemon's ignored signal dispositions.
    fn ignore_signals(&mut self);
    /// Reads the default IPv6 wildcard behavior.
    fn ipv6_dual_stack(&mut self) -> bool;
    /// Creates and owns the UDP announcement socket.
    fn open_socket(&mut self) -> Result<(), Errno>;
    /// Binds the owned socket to one candidate privileged port.
    fn bind(&mut self, port: u16) -> Result<(), Errno>;
    /// Takes the socket that the process inherited as its standard input, and returns the port
    /// to which it is bound.
    fn inherit_socket(&mut self) -> Result<u16, Errno>;
    /// Reads the monotonic clock in milliseconds.
    fn now_ms(&mut self) -> u64;
    /// Collects the announcement for one scan.
    fn collect(&mut self, own_port: u16, now: u64, ipv6_dual_stack: bool) -> Vec<u8>;
    /// Sends one announcement datagram and reports whether every byte was accepted.
    fn send(&mut self, destination: Ipv4Addr, payload: &[u8]) -> bool;
    /// Sleeps between scans.
    fn sleep_ms(&mut self, milliseconds: u64);
}

/// Real live-system implementation of the sender boundary.
struct RealSystem {
    /// Stateful socket-table collector.
    collector: Collector,
    /// Owned UDP socket after `open_socket` succeeds.
    socket: Option<sys::UdpSocket>,
}

impl RealSystem {
    /// Creates an unbound live system.
    fn new() -> Self {
        Self {
            collector: Collector::new(),
            socket: None,
        }
    }
}

impl System for RealSystem {
    fn ignore_signals(&mut self) {
        sys::ignore_interrupt_and_hangup();
    }

    fn ipv6_dual_stack(&mut self) -> bool {
        collect::read_bindv6only(Path::new(PROC_ROOT), IPV6_DUAL_STACK_BY_DEFAULT)
    }

    fn open_socket(&mut self) -> Result<(), Errno> {
        self.socket = Some(sys::UdpSocket::ipv4()?);
        Ok(())
    }

    fn bind(&mut self, port: u16) -> Result<(), Errno> {
        self.socket
            .as_ref()
            .expect("the sender opens its socket before binding")
            .bind_any(port)
    }

    fn inherit_socket(&mut self) -> Result<u16, Errno> {
        let socket = sys::UdpSocket::inherited(sys::take_inherited_descriptor(c"0")?)?;
        let port = socket.port()?;
        self.socket = Some(socket);
        Ok(port)
    }

    /// Returns the C program's whole milliseconds of the monotonic clock, or 0 if the clock
    /// fails, which Linux's monotonic clock does not.
    fn now_ms(&mut self) -> u64 {
        sys::monotonic_ms().unwrap_or(0)
    }

    fn collect(&mut self, own_port: u16, now: u64, ipv6_dual_stack: bool) -> Vec<u8> {
        self.collector.collect_files(
            Path::new(PROC_ROOT),
            DiagnosticInput::Live,
            own_port,
            now,
            ipv6_dual_stack,
        )
    }

    fn send(&mut self, destination: Ipv4Addr, payload: &[u8]) -> bool {
        self.socket
            .as_ref()
            .expect("the sender opens its socket before sending")
            .send_to(SocketAddrV4::new(destination, ANNOUNCEMENT_PORT), payload)
            .is_ok_and(|sent| sent == payload.len())
    }

    /// Sleeps with the standard library, which resumes an interrupted sleep for its remaining
    /// time, as the C program's `nanosleep` loop did.
    fn sleep_ms(&mut self, milliseconds: u64) {
        std::thread::sleep(Duration::from_millis(milliseconds));
    }
}

/// Typed failure of the live sender.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    /// The gateway is not an IPv4 address in the strict dotted-decimal form.
    InvalidGateway,
    /// Creating the UDP socket failed.
    Socket(Errno),
    /// Binding one candidate failed for a reason other than address reuse.
    Bind(Errno),
    /// Every privileged source port from `FIRST_SOURCE_PORT` down to `LAST_SOURCE_PORT` was
    /// already in use.
    NoPrivilegedPort,
    /// The standard input that the announcer inherited is not an IPv4 datagram socket, or the
    /// port to which it is bound cannot be read.
    Inherited(Errno),
    /// The socket that the announcer inherited is bound to a port outside `LAST_SOURCE_PORT` to
    /// `FIRST_SOURCE_PORT`, or to none, 0, from which the gateway would take no announcement.
    UnprivilegedPort(u16),
}

impl Error {
    /// Writes the C-compatible diagnostic for an operational failure.
    pub(super) fn report(self, err: &mut dyn Write) {
        match self {
            Self::InvalidGateway => {}
            Self::Socket(errno) => report_errno(err, b"nvx-ingress: socket: ", errno),
            Self::Bind(errno) => {
                report_errno(err, b"nvx-ingress: bind privileged port: ", errno);
            }
            Self::NoPrivilegedPort => {
                sys::report(err, &[b"nvx-ingress: no privileged source port is free\n"])
            }
            Self::Inherited(errno) => report_errno(err, b"nvx-ingress: inherited socket: ", errno),
            Self::UnprivilegedPort(port) => sys::report(
                err,
                &[
                    b"nvx-ingress: the inherited socket's port ",
                    port.to_string().as_bytes(),
                    b" is not a privileged source port\n",
                ],
            ),
        }
    }
}

/// Where the sender's socket comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// The sender opens its socket, and binds it to the highest free privileged source port.
    Bound,
    /// The sender takes the socket that it inherited as its standard input, which its starter
    /// bound to a privileged source port.
    Inherited,
}

/// Parses the gateway and runs the live sender forever, from a socket of its own.
pub(super) fn run(gateway: &CStr) -> Result<(), Error> {
    let Some(destination) = parse_gateway(gateway) else {
        return Err(Error::InvalidGateway);
    };
    run_with(&mut RealSystem::new(), destination, Source::Bound, None)
}

/// Parses the gateway and runs the live sender forever, from the socket that it inherited as its
/// standard input, bound to a privileged source port.
pub(super) fn run_inherited(gateway: &CStr) -> Result<(), Error> {
    let Some(destination) = parse_gateway(gateway) else {
        return Err(Error::InvalidGateway);
    };
    run_with(&mut RealSystem::new(), destination, Source::Inherited, None)
}

/// Parses `text` as the C program's `inet_pton(AF_INET, ...)` did: as four decimal octets of at
/// most three digits each, without leading zeros, which is the form that the standard library
/// accepts.
pub(super) fn parse_gateway(text: &CStr) -> Option<Ipv4Addr> {
    text.to_str().ok()?.parse().ok()
}

/// Runs the sender against an injectable system, with a socket from `source`, optionally stopping
/// after a test scan count.
fn run_with(
    system: &mut impl System,
    destination: Ipv4Addr,
    source: Source,
    scan_limit: Option<usize>,
) -> Result<(), Error> {
    system.ignore_signals();
    let ipv6_dual_stack = system.ipv6_dual_stack();
    let own_port = match source {
        Source::Bound => {
            system.open_socket().map_err(Error::Socket)?;
            bind_privileged_port(system)?
        }
        Source::Inherited => {
            let port = system.inherit_socket().map_err(Error::Inherited)?;
            if !(LAST_SOURCE_PORT..=FIRST_SOURCE_PORT).contains(&port) {
                return Err(Error::UnprivilegedPort(port));
            }
            port
        }
    };

    let mut last_payload = Vec::new();
    let mut last_sent = 0_u64;
    let mut scans = 0;
    loop {
        let now = system.now_ms();
        let payload = system.collect(own_port, now, ipv6_dual_stack);
        if (payload != last_payload || now.wrapping_sub(last_sent) >= REFRESH_INTERVAL_MS)
            && system.send(destination, &payload)
        {
            last_payload = payload;
            last_sent = now;
        }
        system.sleep_ms(SCAN_INTERVAL_MS);
        scans += 1;
        if scan_limit.is_some_and(|limit| scans >= limit) {
            return Ok(());
        }
    }
}

/// Binds the system's socket to the highest free privileged source port, as [`bind_privileged`]
/// does, and returns it.
fn bind_privileged_port(system: &mut impl System) -> Result<u16, Error> {
    bind_privileged(|port| system.bind(port))
        .map_err(Error::Bind)?
        .ok_or(Error::NoPrivilegedPort)
}

/// Writes one diagnostic that describes `errno` with `nix`'s text.
fn report_errno(err: &mut dyn Write, prefix: &[u8], errno: Errno) {
    sys::report(err, &[prefix, errno.desc().as_bytes(), b"\n"]);
}

#[cfg(test)]
mod tests;
