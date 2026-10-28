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
fn parse_gateway(text: &CStr) -> Option<Ipv4Addr> {
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
mod tests {
    //! Sender state-machine tests through the injectable operating-system boundary.

    use std::collections::VecDeque;

    use super::*;

    /// Deterministic record-and-replay implementation of [`System`].
    struct FakeSystem {
        /// Whether ignored signals were installed.
        ignored_signals: bool,
        /// Configured IPv6 wildcard policy.
        ipv6_dual_stack: bool,
        /// Optional socket-creation failure.
        socket_error: Option<Errno>,
        /// The port of the inherited socket, or the failure to take it.
        inherited: Result<u16, Errno>,
        /// Whether the sender took its inherited socket.
        took_inherited: bool,
        /// Scripted candidate-port bind outcomes.
        bind_results: VecDeque<Result<(), Errno>>,
        /// Bind outcome after the scripted queue is empty.
        default_bind: Result<(), Errno>,
        /// Candidate ports observed by the fake.
        bound_ports: Vec<u16>,
        /// Scripted monotonic times.
        times: VecDeque<u64>,
        /// Scripted collector payloads.
        payloads: VecDeque<Vec<u8>>,
        /// Arguments supplied to each collection.
        collections: Vec<(u16, u64, bool)>,
        /// Scripted datagram send outcomes.
        send_results: VecDeque<bool>,
        /// Payloads offered to the sender.
        sends: Vec<Vec<u8>>,
        /// Requested sleeps.
        sleeps: Vec<u64>,
    }

    impl FakeSystem {
        /// Creates a successful system script for one scan.
        fn one_scan() -> Self {
            Self {
                ignored_signals: false,
                ipv6_dual_stack: true,
                socket_error: None,
                inherited: Err(Errno::ENOTSOCK),
                took_inherited: false,
                bind_results: VecDeque::new(),
                default_bind: Ok(()),
                bound_ports: Vec::new(),
                times: VecDeque::from([100]),
                payloads: VecDeque::from([b"packet".to_vec()]),
                collections: Vec::new(),
                send_results: VecDeque::new(),
                sends: Vec::new(),
                sleeps: Vec::new(),
            }
        }
    }

    impl System for FakeSystem {
        fn ignore_signals(&mut self) {
            self.ignored_signals = true;
        }

        fn ipv6_dual_stack(&mut self) -> bool {
            self.ipv6_dual_stack
        }

        fn open_socket(&mut self) -> Result<(), Errno> {
            self.socket_error.map_or(Ok(()), Err)
        }

        fn bind(&mut self, port: u16) -> Result<(), Errno> {
            self.bound_ports.push(port);
            self.bind_results.pop_front().unwrap_or(self.default_bind)
        }

        fn inherit_socket(&mut self) -> Result<u16, Errno> {
            self.took_inherited = true;
            self.inherited
        }

        fn now_ms(&mut self) -> u64 {
            self.times.pop_front().expect("a time for every test scan")
        }

        fn collect(&mut self, own_port: u16, now: u64, ipv6_dual_stack: bool) -> Vec<u8> {
            self.collections.push((own_port, now, ipv6_dual_stack));
            self.payloads
                .pop_front()
                .expect("a payload for every test scan")
        }

        fn send(&mut self, _: Ipv4Addr, payload: &[u8]) -> bool {
            self.sends.push(payload.to_vec());
            self.send_results.pop_front().unwrap_or(true)
        }

        fn sleep_ms(&mut self, milliseconds: u64) {
            self.sleeps.push(milliseconds);
        }
    }

    /// Returns the strict IPv4 loopback destination used by the tests.
    fn loopback_destination() -> Ipv4Addr {
        parse_gateway(c"127.0.0.1").unwrap()
    }

    /// Verifies strict address syntax and the timing constants inherited from C.
    #[test]
    fn address_and_timing_contracts_match_c() {
        assert_eq!(loopback_destination(), Ipv4Addr::LOCALHOST);
        assert_eq!(SCAN_INTERVAL_MS, 100);
        assert_eq!(REFRESH_INTERVAL_MS, 2_000);
        assert!(parse_gateway(c"127.0.0.01").is_none());
    }

    /// Verifies that the gateway parser accepts and rejects what `inet_pton(AF_INET, ...)` does.
    #[test]
    fn gateways_parse_as_inet_pton_does() {
        for (text, expected) in [
            (c"0.0.0.0", Ipv4Addr::UNSPECIFIED),
            (c"10.0.2.2", Ipv4Addr::new(10, 0, 2, 2)),
            (c"255.255.255.255", Ipv4Addr::BROADCAST),
        ] {
            assert_eq!(parse_gateway(text), Some(expected), "{text:?}");
        }
        for text in [
            c"",
            c"1.2.3",
            c"1.2.3.4.",
            c"1.2.3.4.5",
            c" 1.2.3.4",
            c"1.2.3.4 ",
            c"1.2.3.4\n",
            c"01.2.3.4",
            c"1.2.3.00",
            c"1.2.3.0004",
            c"1.2.3.256",
            c"+1.2.3.4",
            c"1.2.3.-4",
            c"0x1.2.3.4",
            c"1..2.3",
            c"1.2.3.4a",
            c"\xc2\xb9.2.3.4",
        ] {
            assert_eq!(parse_gateway(text), None, "{text:?}");
        }
    }

    /// Verifies that only address reuse advances to the next privileged port.
    #[test]
    fn privileged_bind_retries_eaddrinuse_and_uses_the_first_free_port() {
        let mut system = FakeSystem::one_scan();
        system.bind_results =
            VecDeque::from([Err(Errno::EADDRINUSE), Err(Errno::EADDRINUSE), Ok(())]);
        assert_eq!(
            run_with(&mut system, loopback_destination(), Source::Bound, Some(1)),
            Ok(())
        );
        assert!(system.ignored_signals);
        assert_eq!(system.bound_ports, [1023, 1022, 1021]);
        assert_eq!(system.collections, [(1021, 100, true)]);
        assert_eq!(system.sends, [b"packet".to_vec()]);
        assert_eq!(system.sleeps, [SCAN_INTERVAL_MS]);
    }

    /// Verifies distinct typed and rendered socket and bind failures.
    #[test]
    fn socket_and_non_address_in_use_bind_failures_are_reported() {
        for (socket_error, bind_error, prefix) in [
            (
                Some(Errno::EMFILE),
                None,
                b"nvx-ingress: socket: ".as_slice(),
            ),
            (
                None,
                Some(Errno::EACCES),
                b"nvx-ingress: bind privileged port: ".as_slice(),
            ),
        ] {
            let mut system = FakeSystem::one_scan();
            system.socket_error = socket_error;
            if let Some(error) = bind_error {
                system.bind_results.push_back(Err(error));
            }
            let mut err = Vec::new();
            let failure = run_with(&mut system, loopback_destination(), Source::Bound, Some(1))
                .expect_err("must fail");
            failure.report(&mut err);
            let errno = socket_error.or(bind_error).unwrap();
            let mut expected = prefix.to_vec();
            expected.extend_from_slice(errno.desc().as_bytes());
            expected.push(b'\n');
            assert_eq!(err, expected);
            assert!(system.sends.is_empty());
        }
    }

    /// Verifies exhaustion of the complete inclusive privileged-port range.
    #[test]
    fn exhausting_the_privileged_range_reports_no_free_port() {
        let mut system = FakeSystem::one_scan();
        system.default_bind = Err(Errno::EADDRINUSE);
        let mut err = Vec::new();
        let failure = run_with(&mut system, loopback_destination(), Source::Bound, Some(1))
            .expect_err("must fail");
        failure.report(&mut err);
        assert_eq!(system.bound_ports.len(), 512);
        assert_eq!(
            (system.bound_ports.first(), system.bound_ports.last()),
            (Some(&FIRST_SOURCE_PORT), Some(&LAST_SOURCE_PORT))
        );
        assert_eq!(err, b"nvx-ingress: no privileged source port is free\n");
    }

    /// Verifies that the sender takes the socket that it inherited, bound to a privileged source
    /// port, rather than opening and binding one, and refuses one of another port or none.
    #[test]
    fn an_inherited_socket_is_taken_only_at_a_privileged_source_port() {
        for port in [LAST_SOURCE_PORT, 700, FIRST_SOURCE_PORT] {
            let mut system = FakeSystem::one_scan();
            system.socket_error = Some(Errno::EMFILE);
            system.default_bind = Err(Errno::EACCES);
            system.inherited = Ok(port);
            assert_eq!(
                run_with(
                    &mut system,
                    loopback_destination(),
                    Source::Inherited,
                    Some(1)
                ),
                Ok(())
            );
            assert!(system.took_inherited && system.ignored_signals);
            assert!(system.bound_ports.is_empty());
            assert_eq!(system.collections, [(port, 100, true)]);
            assert_eq!(system.sends, [b"packet".to_vec()]);
        }
        for (inherited, expected) in [
            (
                Ok(0),
                b"nvx-ingress: the inherited socket's port 0 is not a privileged source port\n"
                    .to_vec(),
            ),
            (
                Ok(511),
                b"nvx-ingress: the inherited socket's port 511 is not a privileged source port\n"
                    .to_vec(),
            ),
            (
                Ok(1024),
                b"nvx-ingress: the inherited socket's port 1024 is not a privileged source port\n"
                    .to_vec(),
            ),
            (
                Err(Errno::ENOTSOCK),
                [
                    &b"nvx-ingress: inherited socket: "[..],
                    Errno::ENOTSOCK.desc().as_bytes(),
                    b"\n",
                ]
                .concat(),
            ),
        ] {
            let mut system = FakeSystem::one_scan();
            system.inherited = inherited;
            let failure = run_with(
                &mut system,
                loopback_destination(),
                Source::Inherited,
                Some(1),
            )
            .expect_err("must fail");
            let mut err = Vec::new();
            failure.report(&mut err);
            assert_eq!(err, expected, "{inherited:?}");
            assert!(system.bound_ports.is_empty() && system.sends.is_empty());
        }
    }

    /// Verifies retry-after-failure, changed-payload, and periodic refresh sends.
    #[test]
    fn changed_packets_refreshes_and_failed_sends_control_sender_state() {
        let mut system = FakeSystem::one_scan();
        system.times = VecDeque::from([100, 200, 2_100, 2_200, 2_300]);
        system.payloads = VecDeque::from([
            b"A".to_vec(),
            b"A".to_vec(),
            b"A".to_vec(),
            b"A".to_vec(),
            b"B".to_vec(),
        ]);
        system.send_results = VecDeque::from([false, true, true, true]);
        assert_eq!(
            run_with(&mut system, loopback_destination(), Source::Bound, Some(5)),
            Ok(())
        );
        assert_eq!(
            system.sends,
            [b"A".to_vec(), b"A".to_vec(), b"A".to_vec(), b"B".to_vec()]
        );
        assert_eq!(system.sleeps, [SCAN_INTERVAL_MS; 5]);
    }
}
