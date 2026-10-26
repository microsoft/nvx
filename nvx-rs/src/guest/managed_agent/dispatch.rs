use crate::guest::sys::Errno;
use crate::protocol::{
    self, APP_CANCEL, APP_EXEC, APP_FEATURES, APP_MAPS, APP_PING, APP_READY, APP_STOP, APP_STOPPED,
    GuestFeatures, INVALID_REQUEST, MAPPINGS_INCOMPLETE, UNSUPPORTED_OPERATION,
};

use super::diagnostics;
use super::mappings::{HostMappings, MappingError};
use super::seam::System;
use super::session::{Event, ReceiveFailure, Session};
use super::{AgentConfig, exec, tty};

/// Terminal outcome of the long-lived dispatch loop.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The host requested and acknowledged an orderly stop.
    Stopped,
}

/// Serves control requests until STOP or a fatal session error.
pub(crate) fn run(
    system: &mut impl System,
    session: &mut Session,
    config: &AgentConfig,
    mappings: &mut HostMappings,
) -> Result<Outcome, Errno> {
    session.send_attach()?;
    loop {
        let payload = match session.next_event() {
            Ok(Event::Reset) => {
                session.acknowledge().inspect_err(|error| {
                    diagnostics::error(system, "reset-ack", *error as i32);
                })?;
                continue;
            }
            Ok(Event::Data(payload)) => payload,
            Err(failure) => return Err(report_failure(system, failure)),
        };
        // As in the C agent, a DATA record that fails also reports the `data` stage, after any
        // more specific stage. Its status is the error that ends the session, which
        // `control-session` reports too: `EPROTO` where C's `errno` was incidental, as after a
        // validation failure.
        match handle_data(system, session, config, mappings, &payload) {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
            Err(error) => {
                diagnostics::error(system, "data", error as i32);
                return Err(error);
            }
        }
    }
}

/// Reports a record from the broker that ended the session at the C agent's stage, and returns
/// the error that ends the session.
fn report_failure(system: &mut impl System, failure: ReceiveFailure) -> Errno {
    match failure {
        ReceiveFailure::Read(error) => {
            diagnostics::error(system, "outer-read", error as i32);
            error
        }
        ReceiveFailure::Type(code) => {
            diagnostics::error(system, "outer-type", i32::from(code));
            Errno::EPROTO
        }
        ReceiveFailure::Reset => {
            diagnostics::error(system, "reset-ack", Errno::EPROTO as i32);
            Errno::EPROTO
        }
        ReceiveFailure::Data(failure) => {
            diagnostics::error(system, failure.kind.stage(), failure.status);
            let error = failure.into_error();
            diagnostics::error(system, "data", error as i32);
            error
        }
    }
}

/// Decodes the NVXC request of one DATA payload and serves it, returning the outcome that ends
/// the session, if any.
fn handle_data(
    system: &mut impl System,
    session: &mut Session,
    config: &AgentConfig,
    mappings: &mut HostMappings,
    payload: &[u8],
) -> Result<Option<Outcome>, Errno> {
    let request = match session.accept_request(payload) {
        Ok(request) => request,
        Err(failure) => {
            diagnostics::error(system, failure.kind.stage(), failure.status);
            return Err(failure.into_error());
        }
    };
    match request.kind {
        APP_PING => {
            if request.payload.is_empty() {
                session
                    .send_app(APP_READY, request.request_id, 0, &[])
                    .inspect_err(|error| {
                        diagnostics::error(system, "ping-ready", *error as i32);
                    })?;
            } else {
                diagnostics::error(system, "ping-payload", request.payload.len() as i32);
                session.send_error(request.request_id, Errno::EINVAL as i32, INVALID_REQUEST)?;
            }
        }
        APP_EXEC => {
            if mappings.applied != mappings.expected {
                session.send_error(
                    request.request_id,
                    Errno::EAGAIN as i32,
                    MAPPINGS_INCOMPLETE,
                )?;
                return Ok(None);
            }
            let execution = match protocol::decode_guest_exec_payload(request.payload) {
                Ok(execution) => execution,
                Err(_) => {
                    session.send_error(
                        request.request_id,
                        Errno::EINVAL as i32,
                        INVALID_REQUEST,
                    )?;
                    return Ok(None);
                }
            };
            exec::run(system, session, config, request.request_id, &execution)?;
        }
        APP_CANCEL => {
            // The targeted workload has already reported its outcome.
        }
        APP_FEATURES => {
            if request.payload.is_empty() {
                let features = if config.direct {
                    GuestFeatures::DIRECT_AGENT
                } else {
                    GuestFeatures::SANDBOX_AGENT
                };
                session.send_app(APP_READY, request.request_id, 0, &features.encode())?;
            } else {
                session.send_error(request.request_id, Errno::EINVAL as i32, INVALID_REQUEST)?;
            }
        }
        APP_MAPS => match super::mappings::apply_request(system, mappings, request.payload) {
            Ok(()) => session.send_app(APP_READY, request.request_id, 0, &[])?,
            Err(MappingError::Invalid) => {
                session.send_error(request.request_id, Errno::EINVAL as i32, INVALID_REQUEST)?
            }
            Err(MappingError::Mount { status, entry }) => {
                diagnostics::mapping_error(system, status, entry);
                session.send_error(request.request_id, status, protocol::MAPPING_FAILED)?;
            }
        },
        APP_STOP => {
            if !request.payload.is_empty() {
                return Err(Errno::EPROTO);
            }
            session.send_app(APP_STOPPED, request.request_id, 0, &[])?;
            tty::drain(system, session.descriptor());
            return Ok(Some(Outcome::Stopped));
        }
        _ => session.send_error(
            request.request_id,
            Errno::ENOTSUP as i32,
            std::str::from_utf8(UNSUPPORTED_OPERATION).expect("the category is ASCII"),
        )?,
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::io::{Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::thread;

    use crate::protocol::{
        APP_ERROR, APP_EXIT, APP_STDERR, APP_STDOUT, OUTER_ACK, OUTER_CREDIT, OUTER_DATA,
        OUTER_GUEST_ATTACH, OUTER_HEADER_LEN, OUTER_MAX_PAYLOAD, OUTER_RECEIVE_WINDOW, OUTER_RESET,
        OUTER_WAIT, OuterHeader, ProtocolError, Workload, WorkloadEnvironment, decode_ack_payload,
        decode_app, decode_credit_payload, decode_outer_header, encode_app, encode_exec_payload,
        encode_outer,
    };

    use super::super::seam::{Paths, TestSystem, attach_calls};
    use super::*;
    use crate::guest::mappings::Access;
    use crate::guest::sys::{
        CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, CAP_SETGID, CAP_SETUID, FileDescriptor,
        holds_capability,
    };

    const INSTANCE: [u8; 16] = [7; 16];

    #[derive(Debug, PartialEq, Eq)]
    struct Response {
        kind: u8,
        request_id: u64,
        status: i32,
        payload: Vec<u8>,
    }

    struct Harness {
        host: UnixStream,
        thread: thread::JoinHandle<(Result<Outcome, Errno>, TestSystem)>,
        epoch: u64,
        host_sequence: u64,
        guest_sequence: u64,
    }

    impl Harness {
        fn new(direct: bool) -> Self {
            Self::with_system(direct, TestSystem::new(Paths::default()), 0)
        }

        fn with_system(direct: bool, system: TestSystem, expected_mappings: u32) -> Self {
            let mut harness = Self::spawn(direct, system, expected_mappings);
            harness.reset(1, 7);
            harness
        }

        /// Starts a sandbox agent whose assembled root is `rootfs`, with a table of
        /// `expected_mappings` announced mappings, and resets its session.
        fn in_sandbox(rootfs: &Path, system: TestSystem, expected_mappings: u32) -> Self {
            let mut harness = Self::spawn_at(Some(rootfs), system, expected_mappings);
            harness.reset(1, 7);
            harness
        }

        /// Starts the agent, which attaches and waits for the broker's first RESET.
        fn spawn(direct: bool, system: TestSystem, expected_mappings: u32) -> Self {
            let rootfs = (!direct).then_some(Path::new("/rootfs"));
            Self::spawn_at(rootfs, system, expected_mappings)
        }

        /// Starts a direct agent, or a sandbox agent whose assembled root is `rootfs`, which
        /// attaches and waits for the broker's first RESET.
        fn spawn_at(rootfs: Option<&Path>, mut system: TestSystem, expected_mappings: u32) -> Self {
            let (host, guest) = UnixStream::pair().unwrap();
            let descriptor = FileDescriptor::from_owned(guest.into());
            let config = AgentConfig::for_test(rootfs);
            let thread = thread::spawn(move || {
                let mut session = Session::new(descriptor);
                let mut mappings = HostMappings::announced_for(&config, expected_mappings);
                let outcome = run(&mut system, &mut session, &config, &mut mappings);
                (outcome, system)
            });
            let mut harness = Self {
                host,
                thread,
                epoch: 0,
                host_sequence: 0,
                guest_sequence: 0,
            };
            let (header, payload) = harness.read_outer();
            assert_eq!(header.record_type, OUTER_GUEST_ATTACH);
            assert!(payload.is_empty());
            harness
        }

        /// Resets the session to `epoch` with a RESET at `sequence`, which the agent acknowledges.
        fn reset(&mut self, epoch: u64, sequence: u64) {
            self.host
                .write_all(&encode_outer(OUTER_RESET, &INSTANCE, epoch, sequence, &[]).unwrap())
                .unwrap();
            let (header, payload) = self.read_outer();
            assert_eq!(
                (header.record_type, header.epoch, header.sequence),
                (OUTER_ACK, epoch, 0)
            );
            assert_eq!(decode_ack_payload(&payload).unwrap(), OUTER_RECEIVE_WINDOW);
            self.epoch = epoch;
            self.host_sequence = sequence + 1;
            self.guest_sequence = 1;
        }

        fn request(&mut self, kind: u8, payload: &[u8]) -> Response {
            let mut responses = self.request_all(42, kind, payload);
            assert_eq!(responses.len(), 1, "{responses:?}");
            responses.pop().unwrap()
        }

        fn request_all(&mut self, request_id: u64, kind: u8, payload: &[u8]) -> Vec<Response> {
            self.send(request_id, kind, payload);
            self.responses()
        }

        /// Sends one request, and reads the credit that returns its bytes, before any response.
        fn send(&mut self, request_id: u64, kind: u8, payload: &[u8]) {
            let frame = encode_app(kind, request_id, 0, payload);
            self.host
                .write_all(
                    &encode_outer(
                        OUTER_DATA,
                        &INSTANCE,
                        self.epoch,
                        self.host_sequence,
                        &frame,
                    )
                    .unwrap(),
                )
                .unwrap();
            self.host_sequence += 1;
            let (credit, payload) = self.read_outer();
            assert_eq!(
                (credit.record_type, credit.epoch, credit.sequence),
                (OUTER_CREDIT, self.epoch, self.guest_sequence)
            );
            assert_eq!(decode_credit_payload(&payload).unwrap(), frame.len() as u32);
            self.guest_sequence += 1;
        }

        /// Reads the next response, terminal or not.
        fn next_response(&mut self) -> Response {
            let (data, frame) = self.read_outer();
            assert_eq!(
                (data.record_type, data.sequence),
                (OUTER_DATA, self.guest_sequence)
            );
            self.guest_sequence += 1;
            let frame = decode_app(&frame).unwrap();
            Response {
                kind: frame.kind,
                request_id: frame.request_id,
                status: frame.status,
                payload: frame.payload.to_vec(),
            }
        }

        /// Reads responses up to the first terminal one, which it returns last.
        fn responses(&mut self) -> Vec<Response> {
            let mut responses = Vec::new();
            loop {
                let response = self.next_response();
                let terminal = matches!(
                    response.kind,
                    APP_READY | APP_ERROR | APP_EXIT | APP_STOPPED
                );
                responses.push(response);
                if terminal {
                    return responses;
                }
            }
        }

        fn stop(mut self) -> TestSystem {
            let response = self.request(APP_STOP, &[]);
            assert_eq!(
                (
                    response.kind,
                    response.request_id,
                    response.status,
                    response.payload.as_slice()
                ),
                (APP_STOPPED, 42, 0, b"".as_slice())
            );
            let (outcome, system) = self.thread.join().unwrap();
            assert!(matches!(outcome, Ok(Outcome::Stopped)));
            system
        }

        fn fail(mut self, record: &[u8]) -> Vec<u8> {
            self.host.write_all(record).unwrap();
            let (outcome, system) = self.thread.join().unwrap();
            assert!(outcome.is_err());
            system.diagnostics
        }

        /// Ends the stream after `partial`, a part of a record or nothing.
        fn close(mut self, partial: &[u8]) -> Vec<u8> {
            self.host.write_all(partial).unwrap();
            self.host.shutdown(std::net::Shutdown::Write).unwrap();
            let (outcome, system) = self.thread.join().unwrap();
            assert_eq!(outcome.err(), Some(Errno::EIO));
            system.diagnostics
        }

        fn read_outer(&mut self) -> (OuterHeader, Vec<u8>) {
            let mut header = [0u8; OUTER_HEADER_LEN];
            self.host.read_exact(&mut header).unwrap();
            let header = decode_outer_header(&header).unwrap();
            let mut payload = vec![0; header.length];
            self.host.read_exact(&mut payload).unwrap();
            (header, payload)
        }
    }

    fn response(kind: u8, request_id: u64, status: i32, payload: &[u8]) -> Response {
        Response {
            kind,
            request_id,
            status,
            payload: payload.to_vec(),
        }
    }

    fn exec_payload(cwd: Option<&str>) -> Vec<u8> {
        encode_exec_payload(&Workload {
            argv: vec!["/bin/true".to_owned()],
            timeout_ms: 0,
            cwd,
            environment: WorkloadEnvironment::Default,
        })
        .unwrap()
    }

    fn maps_payload(first: u32, entries: &[(&[u8], &[u8], u16)], count: Option<u32>) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&first.to_le_bytes());
        payload.extend_from_slice(&count.unwrap_or(entries.len() as u32).to_le_bytes());
        for (source, target, flags) in entries {
            payload.extend_from_slice(&flags.to_le_bytes());
            payload.extend_from_slice(&(source.len() as u16).to_le_bytes());
            payload.extend_from_slice(&(target.len() as u16).to_le_bytes());
            payload.extend_from_slice(source);
            payload.extend_from_slice(target);
        }
        payload
    }

    fn assert_environment_failure(mut harness: Harness, diagnostic_prefix: &[u8]) {
        let replies = harness.request_all(42, APP_EXEC, &exec_payload(None));
        assert_eq!(replies.len(), 2, "{replies:?}");
        assert_eq!(
            (replies[0].kind, replies[0].request_id, replies[0].status),
            (APP_STDERR, 42, 0)
        );
        assert!(
            replies[0].payload.starts_with(diagnostic_prefix),
            "{:?}",
            replies[0].payload
        );
        assert_eq!(replies[1], response(APP_EXIT, 42, 125, b"exit"));
        let system = harness.stop();
        assert_eq!(system.spawn_calls, 0);
    }

    #[test]
    fn agent_advertises_the_control_features_of_its_mode_and_stops() {
        for (direct, expected) in [
            (true, GuestFeatures::DIRECT_AGENT),
            (false, GuestFeatures::SANDBOX_AGENT),
        ] {
            let mut harness = Harness::new(direct);
            let response = harness.request(APP_FEATURES, &[]);
            assert_eq!(
                (response.kind, response.request_id, response.status),
                (APP_READY, 42, 0)
            );
            assert_eq!(GuestFeatures::decode(&response.payload).unwrap(), expected);
            let system = harness.stop();
            assert_eq!(system.tty_drained, 1);
        }
    }

    #[test]
    fn agent_refuses_feature_payloads_and_unannounced_mapping_tables() {
        let mut harness = Harness::new(true);
        for (kind, payload) in [
            (APP_FEATURES, b"abc".as_slice()),
            (APP_MAPS, b"\0\0\0\0\x01\0\0\0\0\0\x01\0\x02\0".as_slice()),
        ] {
            let response = harness.request(kind, payload);
            assert_eq!(
                (response.kind, response.request_id, response.status),
                (APP_ERROR, 42, Errno::EINVAL as i32)
            );
            assert_eq!(response.payload, INVALID_REQUEST.as_bytes());
        }
        harness.stop();
    }

    #[test]
    fn containment_failures_refuse_consecutive_execs_until_verified_empty() {
        if super::super::isolate_process_test(
            "NVX_DISPATCH_CONTAINMENT_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::containment_failures_refuse_consecutive_execs_until_verified_empty",
        ) {
            return;
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock().unwrap();
        let payload = exec_payload(None);
        for state in ["missing", "directory", "malformed", "charged"] {
            let (_temporary, system) = TestSystem::with_cgroup();
            let tasks = system.paths.cgroup_root.join("nvx-exec/pids.current");
            match state {
                "missing" => std::fs::remove_file(&tasks).unwrap(),
                "directory" => {
                    std::fs::remove_file(&tasks).unwrap();
                    std::fs::create_dir(&tasks).unwrap();
                }
                "malformed" => std::fs::write(&tasks, b"max\n").unwrap(),
                "charged" => std::fs::write(&tasks, b"1\n").unwrap(),
                _ => unreachable!(),
            }
            let mut harness = Harness::with_system(true, system, 0);
            for request_id in [42, 43] {
                assert_eq!(
                    harness.request_all(request_id, APP_EXEC, &payload),
                    vec![response(APP_ERROR, request_id, 125, b"containment-failed")],
                    "{state}"
                );
            }

            if state == "directory" {
                std::fs::remove_dir(&tasks).unwrap();
            }
            std::fs::write(&tasks, b"0\n").unwrap();
            assert_eq!(
                harness.request_all(44, APP_EXEC, &payload),
                vec![response(
                    APP_ERROR,
                    44,
                    125,
                    protocol::LAUNCH_FAILED.as_bytes()
                )],
                "{state}"
            );
            let system = harness.stop();
            assert_eq!(system.spawn_calls, 1, "{state}");
        }
    }

    #[test]
    fn mappings_mount_in_order_and_unblock_workloads_through_dispatch() {
        let (temporary, system) = TestSystem::with_host_export();
        let export = system.paths.hostfs_dir.join("root").canonicalize().unwrap();
        let guest = temporary.path().canonicalize().unwrap().join("guest");
        let source = guest.join("src");
        let notes = guest.join("notes.txt");
        let out = guest.join("out");
        let first = maps_payload(
            0,
            &[
                (
                    b"0/src",
                    source.as_os_str().as_bytes(),
                    protocol::MAP_READ_ONLY,
                ),
                (b"0/notes.txt", notes.as_os_str().as_bytes(), 0),
            ],
            None,
        );
        let second = maps_payload(2, &[(b"1", out.as_os_str().as_bytes(), 0)], None);
        let mut harness = Harness::with_system(true, system, 3);
        let mut replies = Vec::new();
        replies.extend(harness.request_all(1, APP_EXEC, &[]));
        replies.extend(harness.request_all(2, APP_MAPS, &first));
        replies.extend(harness.request_all(3, APP_EXEC, &[]));
        replies.extend(harness.request_all(4, APP_MAPS, &second));
        replies.extend(harness.request_all(5, APP_EXEC, &[]));
        assert_eq!(
            replies,
            vec![
                response(APP_ERROR, 1, Errno::EAGAIN as i32, b"mappings-incomplete"),
                response(APP_READY, 2, 0, b""),
                response(APP_ERROR, 3, Errno::EAGAIN as i32, b"mappings-incomplete"),
                response(APP_READY, 4, 0, b""),
                response(APP_ERROR, 5, Errno::EINVAL as i32, b"invalid-request"),
            ]
        );

        let system = harness.stop();
        let expected: Vec<_> = [
            (export.join("0/src"), &source, Access::ReadOnly),
            (export.join("0/notes.txt"), &notes, Access::ReadWrite),
            (export.join("1"), &out, Access::ReadWrite),
        ]
        .into_iter()
        .flat_map(|(from, to, access)| attach_calls(&from, to, access))
        .collect();
        assert_eq!(system.kernel.calls(), expected);
        assert!(source.is_dir());
        assert!(notes.is_file());
        assert!(out.is_dir());
        assert_eq!(system.spawn_calls, 0);
    }

    #[test]
    fn mapping_sequence_errors_are_reported_through_dispatch() {
        let (temporary, system) = TestSystem::with_host_export();
        let target = temporary.path().canonicalize().unwrap().join("guest/src");
        let moved = target.display().to_string();
        let target = target.as_os_str().as_bytes();
        let entry = [(b"0/src".as_slice(), target, 0)];
        let mut harness = Harness::with_system(true, system, 1);
        let mut replies = Vec::new();
        replies.extend(harness.request_all(1, APP_MAPS, &maps_payload(1, &entry, None)));
        replies.extend(harness.request_all(
            2,
            APP_MAPS,
            &maps_payload(0, &[(b"0/src", target, 0), (b"0/src", target, 0)], None),
        ));
        replies.extend(harness.request_all(3, APP_MAPS, &maps_payload(0, &entry, None)));
        replies.extend(harness.request_all(4, APP_MAPS, &maps_payload(1, &entry, None)));
        assert_eq!(
            replies,
            vec![
                response(APP_ERROR, 1, Errno::EINVAL as i32, b"invalid-request"),
                response(APP_ERROR, 2, Errno::EINVAL as i32, b"invalid-request"),
                response(APP_READY, 3, 0, b""),
                response(APP_ERROR, 4, Errno::EINVAL as i32, b"invalid-request"),
            ]
        );
        let system = harness.stop();
        assert_eq!(system.kernel.moves(), [moved]);
        assert_eq!(system.spawn_calls, 0);
    }

    #[test]
    fn a_failed_mapping_keeps_refusing_workloads_through_dispatch() {
        let (temporary, system) = TestSystem::with_host_export();
        let target = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("guest/missing");
        let missing = maps_payload(0, &[(b"0/missing", target.as_os_str().as_bytes(), 0)], None);
        let valid = maps_payload(0, &[(b"0/src", target.as_os_str().as_bytes(), 0)], None);
        let mut harness = Harness::with_system(true, system, 2);
        let mut replies = Vec::new();
        replies.extend(harness.request_all(1, APP_MAPS, &missing));
        replies.extend(harness.request_all(2, APP_EXEC, &[]));
        replies.extend(harness.request_all(3, APP_MAPS, &valid));
        assert_eq!(
            replies,
            vec![
                response(APP_ERROR, 1, Errno::ENOENT as i32, b"mapping-failed"),
                response(APP_ERROR, 2, Errno::EAGAIN as i32, b"mappings-incomplete"),
                response(APP_ERROR, 3, Errno::EINVAL as i32, b"invalid-request"),
            ]
        );
        let system = harness.stop();
        assert!(system.kernel.calls().is_empty());
        assert_eq!(system.spawn_calls, 0);
    }

    #[test]
    fn unannounced_mapping_tables_are_refused_through_dispatch() {
        let (temporary, system) = TestSystem::with_host_export();
        let target = temporary.path().canonicalize().unwrap().join("guest/src");
        let mapping = maps_payload(0, &[(b"0/src", target.as_os_str().as_bytes(), 0)], None);
        let mut harness = Harness::with_system(true, system, 0);
        assert_eq!(
            harness.request_all(1, APP_MAPS, &mapping),
            vec![response(
                APP_ERROR,
                1,
                Errno::EINVAL as i32,
                b"invalid-request"
            )]
        );
        assert_eq!(
            harness.request_all(2, APP_EXEC, &[]),
            vec![response(
                APP_ERROR,
                2,
                Errno::EINVAL as i32,
                b"invalid-request"
            )]
        );
        let system = harness.stop();
        assert!(system.kernel.calls().is_empty());
        assert_eq!(system.spawn_calls, 0);
    }

    /// Returns a system with a host export, and the canonical path of a sandbox's assembled root
    /// beside it, which is the root of a mount.
    fn sandbox_root() -> (tempfile::TempDir, TestSystem, std::path::PathBuf) {
        let (temporary, system) = TestSystem::with_host_export();
        let rootfs = temporary.path().canonicalize().unwrap().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        system.kernel.add_mount(&rootfs);
        (temporary, system, rootfs)
    }

    #[test]
    fn sandbox_mappings_mount_in_order_beneath_its_root_and_unblock_workloads() {
        let (_temporary, system, rootfs) = sandbox_root();
        let export = system.paths.hostfs_dir.join("root").canonicalize().unwrap();
        // The targets are the container's paths, below its assembled root, even below the
        // runtime's exact paths, such as `/etc`.
        let first = maps_payload(
            0,
            &[
                (b"0/src", b"/workspace", protocol::MAP_READ_ONLY),
                (b"0/notes.txt", b"/etc/app/notes.txt", 0),
            ],
            None,
        );
        let second = maps_payload(2, &[(b"1", b"/out", 0)], None);
        let mut harness = Harness::in_sandbox(&rootfs, system, 3);
        let mut replies = Vec::new();
        replies.extend(harness.request_all(1, APP_EXEC, &[]));
        replies.extend(harness.request_all(2, APP_MAPS, &first));
        replies.extend(harness.request_all(3, APP_EXEC, &[]));
        replies.extend(harness.request_all(4, APP_MAPS, &second));
        replies.extend(harness.request_all(5, APP_EXEC, &[]));
        assert_eq!(
            replies,
            vec![
                response(APP_ERROR, 1, Errno::EAGAIN as i32, b"mappings-incomplete"),
                response(APP_READY, 2, 0, b""),
                response(APP_ERROR, 3, Errno::EAGAIN as i32, b"mappings-incomplete"),
                response(APP_READY, 4, 0, b""),
                // With the table complete, the request reaches its decoder.
                response(APP_ERROR, 5, Errno::EINVAL as i32, b"invalid-request"),
            ]
        );

        let system = harness.stop();
        let [workspace, notes, out] =
            ["workspace", "etc/app/notes.txt", "out"].map(|target| rootfs.join(target));
        let expected: Vec<_> = [
            (export.join("0/src"), &workspace, Access::ReadOnly),
            (export.join("0/notes.txt"), &notes, Access::ReadWrite),
            (export.join("1"), &out, Access::ReadWrite),
        ]
        .into_iter()
        .flat_map(|(from, to, access)| attach_calls(&from, to, access))
        .collect();
        assert_eq!(system.kernel.calls(), expected);
        assert!(workspace.is_dir() && notes.is_file() && out.is_dir());
        assert_eq!(system.spawn_calls, 0);
    }

    #[test]
    fn sandbox_mappings_refuse_the_targets_of_the_container_runtime() {
        // The runtime's trees, their ancestors, and its exact paths.
        for target in [
            &b"/proc"[..],
            b"/dev/shm",
            b"/.nvx-agent",
            b"/etc/machine-id",
            b"/etc/resolv.conf",
            b"/run/nvx/time/state",
            b"/run/nvx",
            b"/run",
            b"/etc",
        ] {
            let (_temporary, system, rootfs) = sandbox_root();
            let mapping = maps_payload(0, &[(b"0/src", target, 0)], None);
            let mut harness = Harness::in_sandbox(&rootfs, system, 1);
            let mut replies = Vec::new();
            replies.extend(harness.request_all(1, APP_MAPS, &mapping));
            replies.extend(harness.request_all(2, APP_EXEC, &[]));
            assert_eq!(
                replies,
                vec![
                    response(APP_ERROR, 1, Errno::EPERM as i32, b"mapping-failed"),
                    response(APP_ERROR, 2, Errno::EAGAIN as i32, b"mappings-incomplete"),
                ],
                "{}",
                String::from_utf8_lossy(target)
            );
            let system = harness.stop();
            assert!(system.kernel.calls().is_empty());
            assert_eq!(std::fs::read_dir(&rootfs).unwrap().count(), 0);
        }
    }

    #[test]
    fn a_failed_sandbox_mapping_keeps_refusing_workloads() {
        let (_temporary, system, rootfs) = sandbox_root();
        let missing = maps_payload(0, &[(b"0/missing", b"/workspace", 0)], None);
        let valid = maps_payload(0, &[(b"0/src", b"/workspace", 0)], None);
        let mut harness = Harness::in_sandbox(&rootfs, system, 2);
        let mut replies = Vec::new();
        replies.extend(harness.request_all(1, APP_MAPS, &missing));
        replies.extend(harness.request_all(2, APP_EXEC, &[]));
        replies.extend(harness.request_all(3, APP_MAPS, &valid));
        replies.extend(harness.request_all(4, APP_EXEC, &[]));
        assert_eq!(
            replies,
            vec![
                response(APP_ERROR, 1, Errno::ENOENT as i32, b"mapping-failed"),
                response(APP_ERROR, 2, Errno::EAGAIN as i32, b"mappings-incomplete"),
                response(APP_ERROR, 3, Errno::EINVAL as i32, b"invalid-request"),
                response(APP_ERROR, 4, Errno::EAGAIN as i32, b"mappings-incomplete"),
            ]
        );
        let system = harness.stop();
        assert!(system.kernel.calls().is_empty());
        assert_eq!(system.spawn_calls, 0);
    }

    #[test]
    fn a_sandbox_without_mappings_maps_nothing_and_admits_workloads() {
        let (_temporary, system, rootfs) = sandbox_root();
        let mapping = maps_payload(0, &[(b"0/src", b"/workspace", 0)], None);
        let mut harness = Harness::in_sandbox(&rootfs, system, 0);
        let mut replies = Vec::new();
        replies.extend(harness.request_all(1, APP_MAPS, &mapping));
        replies.extend(harness.request_all(2, APP_EXEC, &[]));
        assert_eq!(
            replies,
            vec![
                response(APP_ERROR, 1, Errno::EINVAL as i32, b"invalid-request"),
                // No table holds the request back, so it reaches its decoder.
                response(APP_ERROR, 2, Errno::EINVAL as i32, b"invalid-request"),
            ]
        );
        let system = harness.stop();
        assert!(system.kernel.calls().is_empty());
        assert_eq!(std::fs::read_dir(&rootfs).unwrap().count(), 0);
    }

    #[test]
    fn sandbox_does_not_launch_when_an_environment_export_fails() {
        let mut system = TestSystem::new(Paths::default());
        system.fail_environment = Some(b"HOME".to_vec());
        assert_environment_failure(
            Harness::with_system(false, system, 0),
            b"nvx-managed-agent: cannot configure workload environment: ",
        );
    }

    /// Spawns a sandbox launch command, whose entry stages the test system simulates, without
    /// privileges.
    fn spawn_sandbox(
        command: crate::guest::spawn::Command,
    ) -> Result<crate::guest::spawn::Spawned, Errno> {
        command
            .spawn()
            .map_err(|error| crate::guest::sys::errno_of(&error))
    }

    /// Returns a sandbox agent's test system that spawns its workloads, with their entry stages
    /// simulated, and the path of its `container` cgroup's membership file.
    fn sandbox_system() -> (tempfile::TempDir, TestSystem, std::path::PathBuf) {
        let (temporary, mut system) = TestSystem::with_container_cgroup();
        system.spawner = Some(spawn_sandbox);
        let procs = system.paths.cgroup_root.join("container/cgroup.procs");
        (temporary, system, procs)
    }

    #[test]
    fn sandbox_launch_places_its_workload_before_releasing_it() -> TestResult {
        if super::super::isolate_process_test(
            "NVX_DISPATCH_SANDBOX_PLACEMENT_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::sandbox_launch_places_its_workload_before_releasing_it",
        ) {
            return Ok(());
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock()?;
        let (_temporary, system, procs) = sandbox_system();
        // The workload sees its own process ID in the cgroup's membership file as soon as it
        // runs, leads its own session, and gets the workload's environment.
        let script = "[ \"$(cat \"$0\")\" = \"$$\" ] || exit 3; \
                      set -- $(cat /proc/$$/stat); [ \"$6\" = \"$$\" ] || exit 4; \
                      printf '%s %s %s %s' \"$HOME\" \"$USER\" \"$LOGNAME\" \"$PWD\"";
        let mut harness = Harness::with_system(false, system, 0);
        assert_eq!(
            harness.request_all(
                42,
                APP_EXEC,
                &workload_payload(&["/bin/sh", "-c", script, utf8(&procs)?], Some("/tmp"))?,
            ),
            vec![
                response(APP_STDOUT, 42, 0, b"/home/workload workload workload /tmp"),
                response(APP_EXIT, 42, 0, b"exit"),
            ]
        );
        let system = harness.stop();
        assert_eq!((system.entries_prepared, system.spawn_calls), (1, 1));
        assert!(system.diagnostics.is_empty());
        Ok(())
    }

    #[test]
    fn sandbox_launch_reports_unusable_directories_and_programs_as_its_helper_did() -> TestResult {
        if super::super::isolate_process_test(
            "NVX_DISPATCH_SANDBOX_REFUSAL_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::sandbox_launch_reports_unusable_directories_and_programs_as_its_helper_did",
        ) {
            return Ok(());
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock()?;
        let (temporary, system, _) = sandbox_system();
        let missing = temporary.path().join("missing");
        let missing = utf8(&missing)?;
        let mut harness = Harness::with_system(false, system, 0);
        let diagnostic = super::super::diagnostics::with_description(
            format!("nvx-managed-agent: cannot use working directory {missing}: ").as_bytes(),
            Errno::ENOENT,
        );
        assert_eq!(
            harness.request_all(
                42,
                APP_EXEC,
                &workload_payload(&["/bin/true"], Some(missing))?
            ),
            vec![
                response(APP_STDERR, 42, 0, &diagnostic),
                response(APP_EXIT, 42, 125, b"exit"),
            ]
        );
        let diagnostic = super::super::diagnostics::with_description(
            b"nvx-managed-agent: cannot execute workload: ",
            Errno::ENOENT,
        );
        assert_eq!(
            harness.request_all(43, APP_EXEC, &workload_payload(&[missing], None)?),
            vec![
                response(APP_STDERR, 43, 0, &diagnostic),
                response(APP_EXIT, 43, 125, b"exit"),
            ]
        );
        let system = harness.stop();
        assert!(system.diagnostics.is_empty());
        Ok(())
    }

    #[test]
    fn sandbox_launch_fails_on_its_entry_and_its_placement_without_running() -> TestResult {
        if super::super::isolate_process_test(
            "NVX_DISPATCH_SANDBOX_ENTRY_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::sandbox_launch_fails_on_its_entry_and_its_placement_without_running",
        ) {
            return Ok(());
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock()?;
        let launch_failed = |request_id| {
            response(
                APP_ERROR,
                request_id,
                125,
                protocol::LAUNCH_FAILED.as_bytes(),
            )
        };
        let (temporary, mut system, _) = sandbox_system();
        let marker = temporary.path().join("ran");
        let payload = workload_payload(&["/bin/touch", utf8(&marker)?], None)?;

        // A refused preparation names its reason and starts nothing.
        system.refuse_entry = true;
        let mut harness = Harness::with_system(false, system, 0);
        let diagnostic = b"nvx-managed-agent: cannot prepare the workload's execution: the \
                           account is unavailable: no passwd entry has the user ID\n";
        assert_eq!(
            harness.request_all(42, APP_EXEC, &payload),
            vec![response(APP_STDERR, 42, 0, diagnostic), launch_failed(42)]
        );
        let system = harness.stop();
        assert_eq!((system.entries_prepared, system.spawn_calls), (1, 0));

        // A failed entry stage, which names itself, and a failed placement, which never releases
        // the workload, end the launch before the workload runs.
        for (request_id, error) in [(43, Errno::EPERM), (44, Errno::ENOENT)] {
            let (_cgroup, mut system, procs) = sandbox_system();
            let mut expected = Vec::new();
            if error == Errno::EPERM {
                system.entry_failure = Some((crate::guest::spawn::LaunchStage::MoveRoot, error));
                expected.push(response(
                    APP_STDERR,
                    request_id,
                    0,
                    b"nvx-managed-agent: cannot enter the workload's execution: the root move \
                      launch stage failed: Operation not permitted (os error 1)\n",
                ));
            } else {
                std::fs::remove_file(&procs)?;
            }
            expected.push(launch_failed(request_id));
            let mut harness = Harness::with_system(false, system, 0);
            assert_eq!(
                harness.request_all(request_id, APP_EXEC, &payload),
                expected
            );
            let system = harness.stop();
            assert_eq!(
                system.diagnostics,
                format!(
                    "NVX-MANAGED-ERROR: stage=exec-launch status={}\n",
                    error as i32
                )
                .as_bytes()
            );
        }
        assert!(!marker.exists());
        Ok(())
    }

    #[test]
    fn direct_does_not_launch_when_identity_export_fails() {
        // Direct mode verifies the exec cgroup first, which reaps any child of this process.
        if super::super::isolate_process_test(
            "NVX_DISPATCH_HOME_EXPORT_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::direct_does_not_launch_when_identity_export_fails",
        ) {
            return;
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock().unwrap();
        let (_temporary, mut system) = TestSystem::with_cgroup();
        system.fail_environment = Some(b"HOME".to_vec());
        assert_environment_failure(
            Harness::with_system(true, system, 0),
            b"nvx-managed-agent: cannot configure workload environment: ",
        );
    }

    #[test]
    fn pwd_export_failures_refuse_launch_without_spawning() {
        // Direct mode verifies the exec cgroup first, which reaps any child of this process.
        if super::super::isolate_process_test(
            "NVX_DISPATCH_PWD_EXPORT_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::pwd_export_failures_refuse_launch_without_spawning",
        ) {
            return;
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock().unwrap();
        let (temporary, mut system) = TestSystem::with_cgroup();
        system.fail_environment = Some(b"PWD".to_vec());
        let mut harness = Harness::with_system(true, system, 0);
        for (request_id, cwd) in [(42, None), (43, Some(temporary.path().to_str().unwrap()))] {
            assert_eq!(
                harness.request_all(request_id, APP_EXEC, &exec_payload(cwd)),
                vec![response(
                    APP_ERROR,
                    request_id,
                    125,
                    protocol::LAUNCH_FAILED.as_bytes()
                )]
            );
        }
        let system = harness.stop();
        assert_eq!(system.spawn_calls, 0);
    }

    #[test]
    fn agent_refuses_malformed_working_directories() {
        let mut harness = Harness::new(true);
        let argument = [9u32.to_le_bytes().as_slice(), b"/bin/true"].concat();
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.extend_from_slice(&2u16.to_le_bytes());
        payload.extend_from_slice(&argument);
        let response = harness.request(APP_EXEC, &payload);
        assert_eq!(
            (response.kind, response.status, response.payload.as_slice()),
            (APP_ERROR, Errno::EINVAL as i32, INVALID_REQUEST.as_bytes())
        );
        harness.stop();
    }

    #[test]
    fn data_validation_diagnostics_preserve_the_c_failure_stage_and_status() {
        let data = format!(
            "NVX-MANAGED-ERROR: stage=data status={}\n",
            Errno::EPROTO as i32
        );
        let frame = encode_app(APP_PING, 42, 0, &[]);
        let diagnostics =
            Harness::new(true).fail(&encode_outer(OUTER_DATA, &INSTANCE, 1, 9, &frame).unwrap());
        assert_eq!(
            diagnostics,
            [
                b"NVX-MANAGED-ERROR: stage=data-sequence status=9\n",
                data.as_bytes()
            ]
            .concat()
        );

        let diagnostics =
            Harness::new(true).fail(&encode_outer(OUTER_DATA, &INSTANCE, 2, 8, &frame).unwrap());
        assert_eq!(
            diagnostics,
            [
                b"NVX-MANAGED-ERROR: stage=data-identity status=2\n",
                data.as_bytes()
            ]
            .concat()
        );

        let diagnostics =
            Harness::new(true).fail(&encode_outer(OUTER_DATA, &INSTANCE, 1, 8, b"x").unwrap());
        assert_eq!(
            diagnostics,
            [
                b"NVX-MANAGED-ERROR: stage=data-application status=1\n",
                data.as_bytes()
            ]
            .concat()
        );
    }

    #[test]
    fn a_request_that_fails_after_validation_reports_the_data_stage() {
        let frame = encode_app(APP_STOP, 42, 0, b"x");
        let diagnostics =
            Harness::new(true).fail(&encode_outer(OUTER_DATA, &INSTANCE, 1, 8, &frame).unwrap());
        assert_eq!(
            diagnostics,
            format!(
                "NVX-MANAGED-ERROR: stage=data status={}\n",
                Errno::EPROTO as i32
            )
            .as_bytes()
        );
    }

    /// Outcome of a test that fails with the first error that it meets.
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Returns the payload of an `EXEC` request for `argv` in `cwd`, with the default environment,
    /// or why the workload cannot be encoded.
    fn workload_payload(argv: &[&str], cwd: Option<&str>) -> Result<Vec<u8>, ProtocolError> {
        encode_exec_payload(&Workload {
            argv: argv.iter().map(|argument| (*argument).to_owned()).collect(),
            timeout_ms: 0,
            cwd,
            environment: WorkloadEnvironment::Default,
        })
    }

    /// Returns `path` as UTF-8, or an error that names it.
    fn utf8(path: &std::path::Path) -> Result<&str, Box<dyn std::error::Error>> {
        path.to_str()
            .ok_or_else(|| format!("{} is not UTF-8", path.display()).into())
    }

    /// A directory that only a process that bypasses directory permissions may search while
    /// this lives, after which its owner may search it again, so that an unprivileged test can
    /// remove its temporary directory.
    struct Unsearchable(std::path::PathBuf);

    impl Unsearchable {
        /// Creates the directory `path` without any permissions.
        fn new(path: std::path::PathBuf) -> std::io::Result<Self> {
            use std::os::unix::fs::PermissionsExt;

            std::fs::create_dir(&path)?;
            let unsearchable = Self(path);
            std::fs::set_permissions(&unsearchable.0, std::fs::Permissions::from_mode(0o000))?;
            Ok(unsearchable)
        }
    }

    impl Drop for Unsearchable {
        fn drop(&mut self) {
            use std::io::Write;
            use std::os::unix::fs::PermissionsExt;

            if let Err(error) =
                std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700))
            {
                // Logging is best effort: a test guard has no caller to report to.
                let _ = writeln!(
                    std::io::stderr(),
                    "cannot restore the permissions of {}: {error}",
                    self.0.display()
                );
            }
        }
    }

    /// Spawns a launch command with its real credential calls.
    fn spawn_with_real_credentials(
        command: crate::guest::spawn::Command,
    ) -> Result<crate::guest::spawn::Spawned, Errno> {
        command
            .spawn()
            .map_err(|error| crate::guest::sys::errno_of(&error))
    }

    #[test]
    fn direct_launch_refuses_working_directories_that_the_workload_cannot_enter() -> TestResult {
        if super::super::isolate_process_test(
            "NVX_DISPATCH_CWD_REFUSAL_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::direct_launch_refuses_working_directories_that_the_workload_cannot_enter",
        ) {
            return Ok(());
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock()?;
        let (temporary, mut system) = TestSystem::with_cgroup();
        let root = temporary.path();
        // With the privilege to change credentials, the child takes the workload's real
        // identity, which must reach the directories. Otherwise its credential calls are
        // replaced, and the test's own identity meets the same refusals unless it may bypass
        // directory permissions, which leaves nothing to check.
        if holds_capability(CAP_SETUID)? && holds_capability(CAP_SETGID)? {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755))?;
            system.spawner = Some(spawn_with_real_credentials);
        } else if holds_capability(CAP_DAC_OVERRIDE)? || holds_capability(CAP_DAC_READ_SEARCH)? {
            return Ok(());
        } else {
            system.spawner = Some(super::super::seam::spawn_as_privileged);
        }
        system.paths.setpriv = super::super::seam::setpriv_shim(root, ":");
        let file = root.join("file");
        std::fs::write(&file, b"")?;
        let unsearchable = Unsearchable::new(root.join("unsearchable"))?;
        let marker = root.join("ran");
        let marker = utf8(&marker)?;
        let mut harness = Harness::with_system(true, system, 0);
        for (request_id, directory, error) in [
            (42, root.join("missing"), Errno::ENOENT),
            (43, file, Errno::ENOTDIR),
            (44, unsearchable.0.clone(), Errno::EACCES),
        ] {
            let directory = utf8(&directory)?;
            let diagnostic = super::super::diagnostics::with_description(
                format!("nvx-managed-agent: cannot enter working directory {directory}: ")
                    .as_bytes(),
                error,
            );
            assert_eq!(
                harness.request_all(
                    request_id,
                    APP_EXEC,
                    &workload_payload(&["/bin/touch", marker], Some(directory))?,
                ),
                vec![
                    response(APP_STDERR, request_id, 0, &diagnostic),
                    response(APP_ERROR, request_id, error as i32, b"cwd-failed"),
                ],
                "{directory}"
            );
        }
        let system = harness.stop();
        assert_eq!(system.spawn_calls, 3);
        assert!(system.diagnostics.is_empty());
        assert!(!std::path::Path::new(marker).exists());
        Ok(())
    }

    #[test]
    fn direct_launch_refuses_a_failed_identity_change_and_runs_nothing() -> TestResult {
        if super::super::isolate_process_test(
            "NVX_DISPATCH_IDENTITY_FAILURE_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::direct_launch_refuses_a_failed_identity_change_and_runs_nothing",
        ) {
            return Ok(());
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock()?;
        let (temporary, mut system) = TestSystem::with_cgroup();
        system.paths.setpriv = super::super::seam::setpriv_shim(temporary.path(), ":");
        system.spawner = Some(super::super::seam::spawn_with_failing_credentials);
        let marker = temporary.path().join("ran");
        let payload = workload_payload(&["/bin/touch", utf8(&marker)?], None)?;
        let mut harness = Harness::with_system(true, system, 0);
        assert_eq!(
            harness.request_all(42, APP_EXEC, &payload),
            vec![response(
                APP_ERROR,
                42,
                125,
                protocol::LAUNCH_FAILED.as_bytes()
            )]
        );
        let system = harness.stop();
        assert_eq!(
            system.diagnostics,
            format!(
                "NVX-MANAGED-ERROR: stage=exec-launch status={}\n",
                Errno::EPERM as i32
            )
            .as_bytes()
        );
        assert!(!marker.exists());
        Ok(())
    }

    #[test]
    fn a_workload_status_that_the_agent_cannot_collect_still_ends_its_request() -> TestResult {
        // While `SIGCHLD` is ignored, which applies to the whole process, the kernel collects
        // every child's status itself, so the test runs in a process of its own.
        if super::super::isolate_process_test(
            "NVX_DISPATCH_LOST_STATUS_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::a_workload_status_that_the_agent_cannot_collect_still_ends_its_request",
        ) {
            return Ok(());
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock()?;
        let (temporary, mut harness) = lost_status_harness()?;
        let payload = encode_exec_payload(&Workload {
            argv: vec!["/bin/sleep".to_owned(), "30".to_owned()],
            timeout_ms: 100,
            cwd: None,
            environment: WorkloadEnvironment::Default,
        })?;
        assert_eq!(
            harness.request_all(42, APP_EXEC, &payload),
            vec![response(APP_EXIT, 42, 124, b"timeout")]
        );
        // Without a cause, the agent knows no outcome but its failure. The workload exits on its
        // own, but only once its output has shown that the agent named it by its pidfd: the
        // kernel collects its status as it ends, and `Execution::new` would fail with `ESRCH`
        // if that came first.
        let exit = Gate::new(temporary.path().join("exit"))?;
        let payload = encode_exec_payload(&Workload {
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "exec 3<\"$0\"; echo ready; read -r line <&3; exit 3".to_owned(),
                utf8(&exit.path)?.to_owned(),
            ],
            timeout_ms: 0,
            cwd: None,
            environment: WorkloadEnvironment::Default,
        })?;
        harness.send(43, APP_EXEC, &payload);
        assert_eq!(
            harness.next_response(),
            response(APP_STDOUT, 43, 0, b"ready\n")
        );
        drop(exit);
        assert_eq!(
            harness.responses(),
            vec![response(APP_EXIT, 43, 125, b"failed")]
        );
        let system = harness.stop();
        assert_eq!(system.diagnostics, lost_status_diagnostics(2));
        Ok(())
    }

    #[test]
    fn a_lost_status_keeps_its_outcome_through_a_later_cancellation_or_timeout() -> TestResult {
        if super::super::isolate_process_test(
            "NVX_DISPATCH_LOST_OUTCOME_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::a_lost_status_keeps_its_outcome_through_a_later_cancellation_or_timeout",
        ) {
            return Ok(());
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock()?;
        let (temporary, mut harness) = lost_status_harness()?;
        let kill = temporary.path().join("cgroup/nvx-exec/cgroup.kill");
        let timeout_ms = 1000;
        let timeout = std::time::Duration::from_millis(u64::from(timeout_ms));
        // Each workload exits on its own once the test opens `exit`, while a descendant holds
        // its output open until the test opens `output`. In between, the test cancels the first
        // workload, and lets the timeout of the second pass.
        for (request_id, cancelled) in [(42, true), (43, false)] {
            let exit = Gate::new(temporary.path().join(format!("exit-{request_id}")))?;
            let output = Gate::new(temporary.path().join(format!("output-{request_id}")))?;
            // The previous request's settlement killed the cgroup.
            std::fs::write(&kill, b"")?;
            let payload = encode_exec_payload(&Workload {
                argv: vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "exec 3<\"$0\" 4<\"$1\"; echo ready; read -r line <&3; \
                     (read -r line <&4; echo late) & exit 3"
                        .to_owned(),
                    utf8(&exit.path)?.to_owned(),
                    utf8(&output.path)?.to_owned(),
                ],
                timeout_ms: if cancelled { 0 } else { timeout_ms },
                cwd: None,
                environment: WorkloadEnvironment::Default,
            })?;
            let sent = std::time::Instant::now();
            harness.send(request_id, APP_EXEC, &payload);
            assert_eq!(
                harness.next_response(),
                response(APP_STDOUT, request_id, 0, b"ready\n")
            );
            let ready = std::time::Instant::now();
            drop(exit);
            // The `ECHILD` of the collection tells the agent that the workload ended with its
            // output open, so it kills the cgroup, and then notes the lost status, before its
            // next turn takes a record.
            wait_for_cgroup_kill(&kill)?;
            if cancelled {
                // The credit tells that the agent took the cancellation, before the output
                // closes.
                harness.send(request_id, APP_CANCEL, &[]);
            } else {
                assert!(
                    sent.elapsed() < timeout,
                    "the workload ended after its timeout"
                );
                // Every turn of the agent's loop checks the timeout, at least once a poll.
                let passed = ready + timeout + std::time::Duration::from_millis(200);
                thread::sleep(passed.saturating_duration_since(std::time::Instant::now()));
            }
            drop(output);
            assert_eq!(
                harness.responses(),
                vec![
                    response(APP_STDOUT, request_id, 0, b"late\n"),
                    response(APP_EXIT, request_id, 125, b"failed")
                ]
            );
        }
        let system = harness.stop();
        assert_eq!(system.diagnostics, lost_status_diagnostics(2));
        Ok(())
    }

    /// Starts a direct agent in a process that ignores `SIGCHLD`, which applies to the whole
    /// process: the kernel collects every status of the agent's children itself. Returns the
    /// temporary directory that holds the agent's fake cgroup root, and the harness.
    fn lost_status_harness() -> Result<(tempfile::TempDir, Harness), Box<dyn std::error::Error>> {
        let (temporary, mut system) = TestSystem::with_cgroup();
        system.paths.setpriv = super::super::seam::setpriv_shim(temporary.path(), ":");
        system.spawner = Some(super::super::seam::spawn_as_privileged);
        crate::guest::sys::ignore_signals(&[crate::guest::sys::Signal::SIGCHLD]);
        let harness = Harness::with_system(true, system, 0);
        // An agent that waited for a lost status would never answer.
        harness
            .host
            .set_read_timeout(Some(std::time::Duration::from_secs(20)))?;
        Ok((temporary, harness))
    }

    /// Returns the diagnostics of `count` lost workload statuses.
    fn lost_status_diagnostics(count: usize) -> Vec<u8> {
        format!(
            "NVX-MANAGED-ERROR: stage=exec-wait status={}\n",
            Errno::ECHILD as i32
        )
        .repeat(count)
        .into_bytes()
    }

    /// A FIFO that the test holds open. A workload that reads it waits until the test drops the
    /// gate, which a failing test does too, so that no workload outlives the test.
    struct Gate {
        /// The FIFO's path, which the workload opens.
        path: std::path::PathBuf,
        /// The test's end, which keeps the FIFO open for writing.
        _end: std::fs::File,
    }

    impl Gate {
        /// Creates the FIFO at `path`, and opens the test's end.
        fn new(path: std::path::PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
            crate::guest::sys::create_fifo(
                &CString::new(path.as_os_str().as_bytes())?,
                crate::guest::sys::Mode::S_IRUSR | crate::guest::sys::Mode::S_IWUSR,
            )?;
            // Linux opens a FIFO for reading and writing without waiting for another end.
            let end = std::fs::File::options()
                .read(true)
                .write(true)
                .open(&path)?;
            Ok(Self { path, _end: end })
        }
    }

    /// Waits until the agent kills its fake execution cgroup through `kill`, the cgroup's
    /// `cgroup.kill` file, which the test emptied.
    fn wait_for_cgroup_kill(kill: &std::path::Path) -> TestResult {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::fs::read(kill)?.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the agent never killed the cgroup"
            );
            thread::sleep(std::time::Duration::from_millis(5));
        }
        Ok(())
    }

    #[test]
    fn direct_launch_starts_the_workload_in_the_directory_that_it_checked() -> TestResult {
        if super::super::isolate_process_test(
            "NVX_DISPATCH_CHECKED_DIRECTORY_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::direct_launch_starts_the_workload_in_the_directory_that_it_checked",
        ) {
            return Ok(());
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock()?;
        let (temporary, mut system) = TestSystem::with_cgroup();
        let root = std::fs::canonicalize(temporary.path())?;
        let checked = root.join("checked");
        let moved = root.join("moved");
        let first = root.join("first");
        let second = root.join("second");
        let link = root.join("link");
        for directory in [&checked, &first, &second] {
            std::fs::create_dir(directory)?;
        }
        std::os::unix::fs::symlink(&first, &link)?;
        // Between the check and the workload, the stand-in `setpriv` renames the checked
        // directory and retargets the link.
        system.paths.setpriv = super::super::seam::setpriv_shim(
            &root,
            &format!(
                "if [ -d '{checked}' ]; then mv '{checked}' '{moved}'; fi\nln -sfn '{second}' '{link}'",
                checked = checked.display(),
                moved = moved.display(),
                second = second.display(),
                link = link.display(),
            ),
        );
        system.spawner = Some(super::super::seam::spawn_as_privileged);
        let mut harness = Harness::with_system(true, system, 0);
        let renamed = harness.request_all(
            42,
            APP_EXEC,
            &workload_payload(&["/bin/sh", "-c", "pwd -P"], Some(utf8(&checked)?))?,
        );
        std::fs::remove_file(&link)?;
        std::os::unix::fs::symlink(&first, &link)?;
        let retargeted = harness.request_all(
            43,
            APP_EXEC,
            &workload_payload(&["/bin/sh", "-c", "pwd -P"], Some(utf8(&link)?))?,
        );
        let system = harness.stop();
        let line = |path: &std::path::Path| [path.as_os_str().as_bytes(), b"\n"].concat();
        assert_eq!(
            renamed,
            vec![
                response(APP_STDOUT, 42, 0, &line(&moved)),
                response(APP_EXIT, 42, 0, b"exit"),
            ]
        );
        assert_eq!(
            retargeted,
            vec![
                response(APP_STDOUT, 43, 0, &line(&first)),
                response(APP_EXIT, 43, 0, b"exit"),
            ]
        );
        assert!(system.diagnostics.is_empty());
        Ok(())
    }

    /// Prepares a direct agent whose stand-in `setpriv` runs each workload in the test's own
    /// identity, in a fake execution cgroup, and returns it with the temporary directory that
    /// holds them.
    fn direct_harness() -> (tempfile::TempDir, Harness) {
        let (temporary, mut system) = TestSystem::with_cgroup();
        system.paths.setpriv = super::super::seam::setpriv_shim(temporary.path(), ":");
        system.spawner = Some(super::super::seam::spawn_as_privileged);
        (temporary, Harness::with_system(true, system, 0))
    }

    #[test]
    fn output_beyond_the_cap_ends_the_workload_after_the_reads_that_fit() -> TestResult {
        if super::super::isolate_process_test(
            "NVX_DISPATCH_OUTPUT_CAP_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::output_beyond_the_cap_ends_the_workload_after_the_reads_that_fit",
        ) {
            return Ok(());
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock()?;
        let (temporary, mut harness) = direct_harness();
        // As the C agent did, the agent forwards each read of up to 32 KiB while the output
        // stays within 1 MiB, and ends the workload at the first read beyond it, before the
        // workload could leave its marker.
        let chunk = 32 * 1024;
        let marker = temporary.path().join("survived");
        let script = format!(
            "head -c {} /dev/zero; touch '{}'",
            protocol::MAX_OUTPUT_BYTES + protocol::MAX_OUTPUT_BYTES / 2,
            marker.display()
        );
        let mut replies = harness.request_all(
            42,
            APP_EXEC,
            &workload_payload(&["/bin/sh", "-c", &script], None)?,
        );
        assert_eq!(
            replies.pop(),
            Some(response(APP_EXIT, 42, 125, b"output-limit"))
        );
        let mut forwarded = 0;
        for reply in &replies {
            assert_eq!(
                (reply.kind, reply.request_id, reply.status),
                (APP_STDOUT, 42, 0)
            );
            assert!((1..=chunk).contains(&reply.payload.len()));
            assert!(reply.payload.iter().all(|&byte| byte == 0));
            forwarded += reply.payload.len();
        }
        assert!(
            protocol::MAX_OUTPUT_BYTES - chunk < forwarded
                && forwarded <= protocol::MAX_OUTPUT_BYTES,
            "{forwarded}"
        );
        assert!(harness.stop().diagnostics.is_empty());
        assert!(!marker.exists());
        Ok(())
    }

    #[test]
    fn a_running_workload_refuses_other_requests_and_ends_when_cancelled_or_late() -> TestResult {
        if super::super::isolate_process_test(
            "NVX_DISPATCH_CANCEL_TEST_CHILD",
            "guest::managed_agent::dispatch::tests::a_running_workload_refuses_other_requests_and_ends_when_cancelled_or_late",
        ) {
            return Ok(());
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock()?;
        let (_temporary, mut harness) = direct_harness();
        let started = std::time::Instant::now();
        harness.send(
            42,
            APP_EXEC,
            &workload_payload(&["/bin/sh", "-c", "exec sleep 30"], None)?,
        );
        assert_eq!(
            harness.request_all(43, APP_PING, &[]),
            vec![response(
                APP_ERROR,
                43,
                Errno::EBUSY as i32,
                protocol::BUSY.as_bytes()
            )]
        );
        // A cancellation of another request leaves the workload running.
        harness.send(41, APP_CANCEL, &[]);
        assert_eq!(
            harness.request_all(42, APP_CANCEL, &[]),
            vec![response(APP_EXIT, 42, 128 + 9, b"cancelled")]
        );
        let late = encode_exec_payload(&Workload {
            argv: ["/bin/sh", "-c", "exec sleep 30"]
                .map(str::to_owned)
                .to_vec(),
            timeout_ms: 100,
            cwd: None,
            environment: WorkloadEnvironment::Default,
        })?;
        assert_eq!(
            harness.request_all(44, APP_EXEC, &late),
            vec![response(APP_EXIT, 44, 124, b"timeout")]
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert!(harness.stop().diagnostics.is_empty());
        Ok(())
    }

    #[test]
    fn refused_records_report_the_stage_at_which_the_c_agent_reported_them() {
        let line = |stage: &str, status: i32| {
            format!("NVX-MANAGED-ERROR: stage={stage} status={status}\n")
        };
        let eproto = Errno::EPROTO as i32;
        let outer = |record_type, instance: &[u8; 16], epoch, sequence, payload: &[u8]| {
            encode_outer(record_type, instance, epoch, sequence, payload).unwrap()
        };
        let ping = encode_app(APP_PING, 42, 0, &[]);
        let mut bad_magic = outer(OUTER_DATA, &INSTANCE, 1, 8, &ping);
        bad_magic[0] = b'X';
        let mut oversized = outer(OUTER_DATA, &INSTANCE, 1, 8, &[]);
        oversized[40..44].copy_from_slice(&(OUTER_MAX_PAYLOAD as u32 + 1).to_le_bytes());
        let window = OUTER_RECEIVE_WINDOW.to_le_bytes();
        let cases = [
            // Headers that the C agent refused.
            (bad_magic, line("outer-read", eproto)),
            (oversized, line("outer-read", eproto)),
            // Types that it does not take, whatever their payloads.
            (outer(0, &INSTANCE, 1, 8, &[]), line("outer-type", 0)),
            (outer(10, &INSTANCE, 1, 8, b"abc"), line("outer-type", 10)),
            (
                outer(OUTER_ACK, &INSTANCE, 1, 8, &window),
                line("outer-type", i32::from(OUTER_ACK)),
            ),
            (
                outer(OUTER_CREDIT, &INSTANCE, 1, 8, &[0; 4]),
                line("outer-type", i32::from(OUTER_CREDIT)),
            ),
            (
                outer(OUTER_WAIT, &INSTANCE, 1, 8, b"x"),
                line("outer-type", i32::from(OUTER_WAIT)),
            ),
            (
                outer(OUTER_GUEST_ATTACH, &INSTANCE, 1, 8, &[]),
                line("outer-type", i32::from(OUTER_GUEST_ATTACH)),
            ),
            // RESET records with a payload or without an epoch.
            (
                outer(OUTER_RESET, &INSTANCE, 2, 0, b"x"),
                line("reset-ack", eproto),
            ),
            (
                outer(OUTER_RESET, &INSTANCE, 0, 0, &[]),
                line("reset-ack", eproto),
            ),
            // DATA records without an NVXC frame, or of another instance.
            (
                outer(OUTER_DATA, &INSTANCE, 1, 8, &[]),
                line("data-application", 0) + &line("data", eproto),
            ),
            (
                outer(OUTER_DATA, &[8; 16], 1, 8, &ping),
                line("data-identity", 1) + &line("data", eproto),
            ),
            // Faults that only the shared session refuses: a RESET without an instance ID, or of an
            // epoch that does not advance the instance's.
            (
                outer(OUTER_RESET, &[0; 16], 2, 0, &[]),
                line("reset-ack", eproto),
            ),
            (
                outer(OUTER_RESET, &INSTANCE, 1, 0, &[]),
                line("reset-ack", eproto),
            ),
        ];
        for (record, expected) in cases {
            let diagnostics = Harness::new(true).fail(&record);
            let header = &record[..OUTER_HEADER_LEN];
            assert_eq!(
                String::from_utf8(diagnostics).unwrap(),
                expected,
                "{header:?}"
            );
        }
    }

    #[test]
    fn data_before_the_first_reset_is_a_type_that_the_agent_does_not_take_yet() {
        let harness = Harness::spawn(true, TestSystem::new(Paths::default()), 0);
        let frame = encode_app(APP_PING, 42, 0, &[]);
        let diagnostics = harness.fail(&encode_outer(OUTER_DATA, &INSTANCE, 1, 0, &frame).unwrap());
        assert_eq!(
            diagnostics,
            format!("NVX-MANAGED-ERROR: stage=outer-type status={OUTER_DATA}\n").as_bytes()
        );
    }

    #[test]
    fn data_at_the_last_sequence_number_is_out_of_sequence() {
        // After a RESET at the second-to-last sequence number, DATA at the last one leaves none
        // for the broker's next record.
        let mut harness = Harness::spawn(true, TestSystem::new(Paths::default()), 0);
        harness.reset(1, u64::MAX - 1);
        let frame = encode_app(APP_PING, 42, 0, &[]);
        let record = encode_outer(OUTER_DATA, &INSTANCE, 1, u64::MAX, &frame).unwrap();
        assert_eq!(
            String::from_utf8(harness.fail(&record)).unwrap(),
            format!(
                "NVX-MANAGED-ERROR: stage=data-sequence status=-1\n\
                 NVX-MANAGED-ERROR: stage=data status={}\n",
                Errno::EPROTO as i32
            )
        );
    }

    #[test]
    fn a_stream_that_ends_reports_a_read_failure_inside_a_record_or_not() {
        let expected = format!(
            "NVX-MANAGED-ERROR: stage=outer-read status={}\n",
            Errno::EIO as i32
        );
        let frame = encode_app(APP_PING, 42, 0, &[]);
        let record = encode_outer(OUTER_DATA, &INSTANCE, 1, 8, &frame).unwrap();
        for length in [0, 20, OUTER_HEADER_LEN + 1] {
            let diagnostics = Harness::new(true).close(&record[..length]);
            assert_eq!(diagnostics, expected.as_bytes(), "after {length} bytes");
        }
    }

    #[test]
    fn a_reset_between_requests_starts_a_new_epoch() {
        let mut harness = Harness::new(true);
        assert_eq!(
            harness.request(APP_PING, &[]),
            response(APP_READY, 42, 0, b"")
        );
        // A RESET of a later epoch may come at any sequence number.
        for (epoch, sequence) in [(2, 0), (5, 11)] {
            harness.reset(epoch, sequence);
            assert_eq!(
                harness.request(APP_PING, &[]),
                response(APP_READY, 42, 0, b"")
            );
        }
        let system = harness.stop();
        assert!(system.diagnostics.is_empty());
    }
}
