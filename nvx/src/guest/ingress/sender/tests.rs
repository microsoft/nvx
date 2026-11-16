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
    system.bind_results = VecDeque::from([Err(Errno::EADDRINUSE), Err(Errno::EADDRINUSE), Ok(())]);
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
