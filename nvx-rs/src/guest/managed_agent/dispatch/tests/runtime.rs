use std::time::{Duration, Instant};

use crate::guest::mappings::{Mapper, MappingTable, Reserved, SourceRoot, TargetRoot};
use crate::guest::sys::tests::fork_isolated_test;
use crate::guest::sys::{self, ChildStatus, ForkResult, ProcessId, Signal};

use super::*;

struct OwnedChild {
    process: ProcessId,
    descriptor: FileDescriptor,
}

impl OwnedChild {
    fn new(process: ProcessId) -> Self {
        let descriptor = sys::open_pidfd(process).unwrap_or_else(|error| {
            sys::kill_process(process).unwrap();
            let _ = sys::wait(process);
            panic!("cannot retain the test child's identity: {error}");
        });
        Self {
            process,
            descriptor,
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        assert!(matches!(
            sys::kill_through_pidfd(self.descriptor.borrowed(), false),
            Ok(()) | Err(Errno::ESRCH)
        ));
        // The Runtime may already have collected this owned child.
        let _ = sys::wait(self.process);
    }
}

fn isolated(marker: &str, name: &str, check: impl FnOnce() + std::panic::UnwindSafe) {
    if super::super::super::tests::isolate_process_test(marker, name) {
        return;
    }
    match fork_isolated_test().unwrap() {
        ForkResult::Child => {
            sys::reset_child_signal();
            let result = std::panic::catch_unwind(check);
            sys::exit_immediately(u8::from(result.is_err()));
        }
        ForkResult::Parent(child) => {
            assert_eq!(sys::wait(child), Some(ChildStatus::Exited(0)));
        }
    }
}

fn helper() -> OwnedChild {
    match sys::fork().unwrap() {
        ForkResult::Child => {
            std::thread::sleep(Duration::from_secs(30));
            sys::exit_immediately(0);
        }
        ForkResult::Parent(process) => OwnedChild::new(process),
    }
}

fn peer(
    check: impl FnOnce(UnixStream, UnixStream) + std::panic::UnwindSafe,
) -> (FileDescriptor, OwnedChild, UnixStream) {
    let (host, guest) = UnixStream::pair().unwrap();
    let (proof, mut completed) = UnixStream::pair().unwrap();
    host.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    proof
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    match sys::fork().unwrap() {
        ForkResult::Child => {
            drop(proof);
            let probe = guest.try_clone().unwrap();
            drop(guest);
            let result = std::panic::catch_unwind(|| check(host, probe));
            if result.is_ok() {
                completed.write_all(&[1]).unwrap();
            }
            sys::exit_immediately(u8::from(result.is_err()));
        }
        ForkResult::Parent(process) => {
            drop(host);
            drop(completed);
            (
                FileDescriptor::from_owned(guest.into()),
                OwnedChild::new(process),
                proof,
            )
        }
    }
}

fn completed(mut proof: UnixStream) {
    let mut success = [0];
    proof.read_exact(&mut success).unwrap();
    assert_eq!(success, [1]);
}

fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "the owned peer made no progress");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn fragment(host: &mut UnixStream, probe: UnixStream, record: &[u8]) {
    use std::os::fd::AsFd;

    host.write_all(&record[..1]).unwrap();
    // Only the guest reads this endpoint: no readable byte proves it consumed the fragment.
    until(|| !sys::readable_or_hung_up(&[probe.as_fd()], 0).unwrap()[0]);
    drop(probe);
}

fn prepared() -> (
    tempfile::TempDir,
    (MappingTable, Mapper, Option<SourceRoot>),
) {
    let (temporary, mut system, rootfs) = sandbox_root();
    let target = TargetRoot::open_with(&mut system.kernel, &rootfs).unwrap();
    (
        temporary,
        (
            MappingTable::new(0).unwrap(),
            Mapper::new(target, Reserved::container_root()),
            None,
        ),
    )
}

fn termination(helper_ends: bool) {
    let helper = helper();
    let parent = sys::open_pidfd(sys::current_process()).unwrap();
    let (descriptor, _peer, proof) = peer(|mut host, probe| {
        assert_eq!(read_outer(&mut host).0.record_type, OUTER_GUEST_ATTACH);
        let reset = encode_outer(OUTER_RESET, &INSTANCE, 1, 0, &[]).unwrap();
        fragment(&mut host, probe, &reset);
        if helper_ends {
            sys::kill_through_pidfd(helper.descriptor.borrowed(), false).unwrap();
            assert!(sys::readable_or_hung_up(&[helper.descriptor.borrowed()], 5000).unwrap()[0]);
        }
        sys::signal_through_pidfd(parent.borrowed(), Signal::SIGTERM as i32, false).unwrap();
        let mut traffic = Vec::new();
        host.read_to_end(&mut traffic).unwrap();
        assert!(
            traffic.is_empty(),
            "SIGTERM must not synthesize NVXC frames"
        );
    });
    let mut runtime =
        Runtime::new(&[CString::new(helper.process.raw().to_string()).unwrap()]).unwrap();
    let (_temporary, mappings) = prepared();
    let config =
        AgentConfig::layered(b"nvx_identity=1000:1000", c"workload".into(), c"/".into()).unwrap();
    let result = if helper_ends {
        let mut session = Session::with_runtime(descriptor, &mut runtime).unwrap();
        let mut system = TestSystem::new(Paths::default());
        let (table, mapper, export) = mappings;
        let mut mappings = HostMappings::layered(table, mapper, export).unwrap();
        assert_eq!(
            run(&mut system, &mut session, &config, &mut mappings).err(),
            Some(Errno::ECHILD)
        );
        Err(session.runtime_failure().unwrap())
    } else {
        config.serve_layered(&mut runtime, descriptor, mappings)
    };
    drop(runtime);
    if helper_ends {
        let failure = result.unwrap_err();
        assert!(failure.contains("the time daemon"), "{failure}");
        assert!(failure.contains("signal 9"), "{failure}");
    } else {
        result.unwrap();
    }
    completed(proof);
}

#[test]
#[ignore = "requires root with CAP_SYS_NICE and CAP_SYS_RESOURCE"]
fn privileged_layered_runtime_stops_during_a_fragment_without_protocol_traffic() {
    isolated(
        "NVX_LAYERED_RUNTIME_STOP_CHILD",
        "guest::managed_agent::dispatch::tests::runtime::privileged_layered_runtime_stops_during_a_fragment_without_protocol_traffic",
        || termination(false),
    );
}

#[test]
#[ignore = "requires root with CAP_SYS_NICE and CAP_SYS_RESOURCE"]
fn privileged_layered_runtime_helper_failure_wins_over_pending_sigterm() {
    isolated(
        "NVX_LAYERED_RUNTIME_HELPER_CHILD",
        "guest::managed_agent::dispatch::tests::runtime::privileged_layered_runtime_helper_failure_wins_over_pending_sigterm",
        || termination(true),
    );
}

fn receive(host: &mut UnixStream) -> Response {
    let (header, payload) = read_outer(host);
    assert_eq!(header.record_type, OUTER_DATA);
    let frame = decode_app(&payload).unwrap();
    response(frame.kind, frame.request_id, frame.status, frame.payload)
}

fn prelaunch_termination(helper_ends: bool) {
    let helper = helper();
    let mut runtime =
        Runtime::new(&[CString::new(helper.process.raw().to_string()).unwrap()]).unwrap();
    let (mut host, guest) = UnixStream::pair().unwrap();
    let mut session =
        Session::with_runtime(FileDescriptor::from_owned(guest.into()), &mut runtime).unwrap();
    session.send_attach().unwrap();
    assert_eq!(read_outer(&mut host).0.record_type, OUTER_GUEST_ATTACH);
    host.write_all(&encode_outer(OUTER_RESET, &INSTANCE, 1, 0, &[]).unwrap())
        .unwrap();
    assert!(matches!(session.next_event(), Ok(Event::Reset)));
    session.acknowledge().unwrap();
    assert_eq!(read_outer(&mut host).0.record_type, OUTER_ACK);
    let execution = workload_payload(&["/bin/true"], Some("/")).unwrap();
    host.write_all(
        &encode_outer(
            OUTER_DATA,
            &INSTANCE,
            1,
            1,
            &encode_app(APP_EXEC, 9, 0, &execution),
        )
        .unwrap(),
    )
    .unwrap();
    let Ok(Event::Data(payload)) = session.next_event() else {
        panic!("the complete EXEC record was not received");
    };
    let Ok(request) = session.accept_request(&payload) else {
        panic!("the complete EXEC request was not accepted");
    };
    assert_eq!(read_outer(&mut host).0.record_type, OUTER_CREDIT);
    let execution = crate::protocol::decode_guest_exec_payload(request.payload).unwrap();
    if helper_ends {
        sys::kill_through_pidfd(helper.descriptor.borrowed(), false).unwrap();
        assert!(sys::readable_or_hung_up(&[helper.descriptor.borrowed()], 5000).unwrap()[0]);
    } else {
        let process = sys::open_pidfd(sys::current_process()).unwrap();
        sys::signal_through_pidfd(process.borrowed(), Signal::SIGTERM as i32, false).unwrap();
    }
    let mut system = TestSystem::new(Paths::default());
    let config =
        AgentConfig::layered(b"nvx_identity=1000:1000", c"workload".into(), c"/".into()).unwrap();
    let error = super::super::super::exec::run(
        &mut system,
        &mut session,
        &config,
        request.request_id,
        &execution,
    )
    .err();
    drop(session);
    drop(runtime);
    let mut traffic = Vec::new();
    host.read_to_end(&mut traffic).unwrap();
    assert_eq!(
        error,
        Some(if helper_ends {
            Errno::ECHILD
        } else {
            Errno::ECANCELED
        })
    );
    assert!(
        traffic.is_empty(),
        "prelaunch shutdown must not send an application error"
    );
    assert_eq!(system.spawn_calls, 0);
    assert!(system.diagnostics.is_empty(), "{:?}", system.diagnostics);
}

#[test]
#[ignore = "requires root with CAP_SYS_NICE and CAP_SYS_RESOURCE"]
fn privileged_layered_runtime_prelaunch_sigterm_sends_no_application_error() {
    isolated(
        "NVX_LAYERED_RUNTIME_PRELAUNCH_STOP_CHILD",
        "guest::managed_agent::dispatch::tests::runtime::privileged_layered_runtime_prelaunch_sigterm_sends_no_application_error",
        || prelaunch_termination(false),
    );
}

#[test]
#[ignore = "requires root with CAP_SYS_NICE and CAP_SYS_RESOURCE"]
fn privileged_layered_runtime_prelaunch_helper_failure_sends_no_application_error() {
    isolated(
        "NVX_LAYERED_RUNTIME_PRELAUNCH_HELPER_CHILD",
        "guest::managed_agent::dispatch::tests::runtime::privileged_layered_runtime_prelaunch_helper_failure_sends_no_application_error",
        || prelaunch_termination(true),
    );
}

#[test]
#[ignore = "requires root with CAP_SYS_NICE and CAP_SYS_RESOURCE"]
fn privileged_layered_runtime_delivers_a_fragment_collected_status_before_driver_events() {
    isolated(
        "NVX_LAYERED_RUNTIME_STATUS_CHILD",
        "guest::managed_agent::dispatch::tests::runtime::privileged_layered_runtime_delivers_a_fragment_collected_status_before_driver_events",
        || {
            let temporary = tempfile::tempdir().unwrap();
            let pid_path = temporary.path().join("leader");
            let gate = temporary.path().join("gate");
            crate::guest::sys::tests::create_fifo(
                &sys::path_c_string(&gate).unwrap(),
                sys::Mode::from_bits_retain(0o600),
            )
            .unwrap();
            let payload = workload_payload(
                &[
                    "/bin/sh",
                    "-c",
                    "printf '%s' \"$$\" > \"$1\"; read -r go < \"$2\"; printf observed; exit 7",
                    "sh",
                    pid_path.to_str().unwrap(),
                    gate.to_str().unwrap(),
                ],
                Some("/"),
            )
            .unwrap();
            let helper = helper();
            let (descriptor, _peer, proof) = peer(|mut host, probe| {
                assert_eq!(read_outer(&mut host).0.record_type, OUTER_GUEST_ATTACH);
                host.write_all(&encode_outer(OUTER_RESET, &INSTANCE, 1, 0, &[]).unwrap())
                    .unwrap();
                assert_eq!(read_outer(&mut host).0.record_type, OUTER_ACK);
                let execution = encode_app(APP_EXEC, 9, 0, &payload);
                host.write_all(&encode_outer(OUTER_DATA, &INSTANCE, 1, 1, &execution).unwrap())
                    .unwrap();
                assert_eq!(read_outer(&mut host).0.record_type, OUTER_CREDIT);
                until(|| pid_path.exists() && std::fs::metadata(&pid_path).unwrap().len() != 0);
                let id: i64 = std::fs::read_to_string(&pid_path).unwrap().parse().unwrap();
                let leader = sys::open_pidfd(ProcessId::from_positive(id).unwrap()).unwrap();
                let ping = encode_outer(
                    OUTER_DATA,
                    &INSTANCE,
                    1,
                    2,
                    &encode_app(APP_PING, 98, 0, &[]),
                )
                .unwrap();
                fragment(&mut host, probe, &ping);
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&gate)
                    .unwrap()
                    .write_all(b"go\n")
                    .unwrap();
                assert!(sys::readable_or_hung_up(&[leader.borrowed()], 5000).unwrap()[0]);
                // The driver never reaps: disappearance here proves Runtime collected while
                // the same dispatcher remained inside the incomplete control record.
                until(|| !Path::new(&format!("/proc/{id}")).exists());
                host.write_all(&ping[1..]).unwrap();
                assert_eq!(read_outer(&mut host).0.record_type, OUTER_CREDIT);
                assert_eq!(
                    receive(&mut host),
                    response(APP_ERROR, 98, Errno::EBUSY as i32, b"busy")
                );
                let mut output = Vec::new();
                loop {
                    let frame = receive(&mut host);
                    if frame.kind == APP_EXIT {
                        assert_eq!(frame, response(APP_EXIT, 9, 7, b"exit"));
                        break;
                    }
                    assert_eq!((frame.kind, frame.request_id), (APP_STDOUT, 9));
                    output.extend(frame.payload);
                }
                assert_eq!(output, b"observed");
                host.write_all(
                    &encode_outer(
                        OUTER_DATA,
                        &INSTANCE,
                        1,
                        3,
                        &encode_app(APP_STOP, 99, 0, &[]),
                    )
                    .unwrap(),
                )
                .unwrap();
                assert_eq!(read_outer(&mut host).0.record_type, OUTER_CREDIT);
                assert_eq!(receive(&mut host), response(APP_STOPPED, 99, 0, &[]));
            });
            let mut runtime =
                Runtime::new(&[CString::new(helper.process.raw().to_string()).unwrap()]).unwrap();
            let mut session = Session::with_runtime(descriptor, &mut runtime).unwrap();
            let mut system = TestSystem::new(Paths::default());
            system.direct_starter = Some(super::super::super::seam::tests::start_as_privileged);
            let config =
                AgentConfig::layered(b"nvx_identity=1000:1000", c"workload".into(), c"/".into())
                    .unwrap();
            let mut mappings = HostMappings::announced_for(&config, 0);
            assert!(matches!(
                run(&mut system, &mut session, &config, &mut mappings),
                Ok(Outcome::Stopped)
            ));
            assert!(system.diagnostics.is_empty(), "{:?}", system.diagnostics);
            drop(session);
            drop(runtime);
            completed(proof);
        },
    );
}

#[test]
#[ignore = "requires root with CAP_SYS_NICE and CAP_SYS_RESOURCE"]
fn privileged_layered_runtime_keeps_helper_policy_and_first_error_during_failed_start() {
    isolated(
        "NVX_LAYERED_RUNTIME_FAILED_START_CHILD",
        "guest::managed_agent::dispatch::tests::runtime::privileged_layered_runtime_keeps_helper_policy_and_first_error_during_failed_start",
        || {
            let temporary = tempfile::tempdir().unwrap();
            let marker = temporary.path().join("failed-child");
            let gate = temporary.path().join("gate");
            crate::guest::sys::tests::create_fifo(
                &sys::path_c_string(&gate).unwrap(),
                sys::Mode::from_bits_retain(0o600),
            )
            .unwrap();
            let payload = workload_payload(
                &[
                    "/bin/sh",
                    "-c",
                    "read -r go < \"$1\"; exit 19",
                    "_",
                    gate.to_str().unwrap(),
                    marker.to_str().unwrap(),
                ],
                Some("/"),
            )
            .unwrap();
            let helper = helper();
            let (descriptor, _peer, proof) = peer(|mut host, probe| {
                drop(probe);
                assert_eq!(read_outer(&mut host).0.record_type, OUTER_GUEST_ATTACH);
                host.write_all(&encode_outer(OUTER_RESET, &INSTANCE, 1, 0, &[]).unwrap())
                    .unwrap();
                assert_eq!(read_outer(&mut host).0.record_type, OUTER_ACK);
                host.write_all(
                    &encode_outer(
                        OUTER_DATA,
                        &INSTANCE,
                        1,
                        1,
                        &encode_app(APP_EXEC, 9, 0, &payload),
                    )
                    .unwrap(),
                )
                .unwrap();
                assert_eq!(read_outer(&mut host).0.record_type, OUTER_CREDIT);
                until(|| marker.exists() && std::fs::metadata(&marker).unwrap().len() != 0);
                let id = std::fs::read_to_string(&marker)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                let failed = OwnedChild::new(ProcessId::from_positive(id).unwrap());
                assert!(!sys::readable_or_hung_up(&[failed.descriptor.borrowed()], 0).unwrap()[0]);
                sys::kill_through_pidfd(helper.descriptor.borrowed(), false).unwrap();
                let mut traffic = Vec::new();
                host.read_to_end(&mut traffic).unwrap();
                assert!(
                    traffic.is_empty(),
                    "helper failure must end the control loop"
                );
                drop(failed);
            });
            let mut runtime =
                Runtime::new(&[CString::new(helper.process.raw().to_string()).unwrap()]).unwrap();
            let mut session = Session::with_runtime(descriptor, &mut runtime).unwrap();
            let mut system = TestSystem::new(Paths::default());
            system.direct_starter = Some(start_with_failed_pidfd_and_cleanup);
            let config =
                AgentConfig::layered(b"nvx_identity=1000:1000", c"workload".into(), c"/".into())
                    .unwrap();
            let mut mappings = HostMappings::announced_for(&config, 0);
            assert_eq!(
                run(&mut system, &mut session, &config, &mut mappings).err(),
                Some(Errno::ECHILD)
            );
            let failure = session.runtime_failure().unwrap();
            assert!(failure.contains("the time daemon"), "{failure}");
            let diagnostics = String::from_utf8(system.diagnostics).unwrap();
            assert!(diagnostics.starts_with(&format!(
                "NVX-MANAGED-ERROR: stage=exec-launch status={}\n\
                 NVX-MANAGED-ERROR: stage=exec-kill status={}\n",
                Errno::EPERM as i32,
                Errno::EPERM as i32
            )));
            assert!(!diagnostics.contains("stage=exec-wait"), "{diagnostics}");
            drop(session);
            drop(runtime);
            completed(proof);
            let id = std::fs::read_to_string(&marker)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(
                sys::wait(ProcessId::from_positive(id).unwrap()),
                Some(ChildStatus::Signaled(Signal::SIGKILL as i32))
            );
        },
    );
}

struct KillParentOnPanic<'a>(&'a FileDescriptor);

impl Drop for KillParentOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            assert!(matches!(
                sys::kill_through_pidfd(self.0.borrowed(), false),
                Ok(()) | Err(Errno::ESRCH)
            ));
        }
    }
}

fn narrow_output(probe: &UnixStream) {
    use nix::sys::socket::{getsockopt, setsockopt, sockopt::SndBuf};

    setsockopt(probe, SndBuf, &1024).unwrap();
    assert!(getsockopt(probe, SndBuf).unwrap() < 6000);
}

fn output_is_blocked(probe: &UnixStream) {
    use std::os::fd::AsFd;

    use nix::poll::{PollFd, PollFlags};

    until(|| {
        let mut descriptors = [PollFd::new(probe.as_fd(), PollFlags::POLLOUT)];
        nix::poll::poll(&mut descriptors, 0u16).unwrap();
        !descriptors[0]
            .revents()
            .unwrap()
            .contains(PollFlags::POLLOUT)
    });
}

fn reset_peer(host: &mut UnixStream) {
    assert_eq!(read_outer(host).0.record_type, OUTER_GUEST_ATTACH);
    host.write_all(&encode_outer(OUTER_RESET, &INSTANCE, 1, 0, &[]).unwrap())
        .unwrap();
    assert_eq!(read_outer(host).0.record_type, OUTER_ACK);
}

fn only_stdout_prefix(mut traffic: &[u8]) {
    assert!(!traffic.is_empty());
    while traffic.len() >= OUTER_HEADER_LEN {
        let header = decode_outer_header(&traffic[..OUTER_HEADER_LEN]).unwrap();
        assert_eq!(header.record_type, OUTER_DATA);
        let length = OUTER_HEADER_LEN + header.length;
        if traffic.len() < length {
            return;
        }
        let frame = decode_app(&traffic[OUTER_HEADER_LEN..length]).unwrap();
        assert_eq!((frame.kind, frame.request_id), (APP_STDOUT, 9));
        traffic = &traffic[length..];
    }
}

fn backpressured_termination(helper_ends: bool) {
    let helper = helper();
    let parent = sys::open_pidfd(sys::current_process()).unwrap();
    let (descriptor, _peer, proof) = peer(|mut host, probe| {
        let _guard = KillParentOnPanic(&parent);
        narrow_output(&probe);
        reset_peer(&mut host);
        output_is_blocked(&probe);
        drop(probe);
        if helper_ends {
            sys::kill_through_pidfd(helper.descriptor.borrowed(), false).unwrap();
            assert!(sys::readable_or_hung_up(&[helper.descriptor.borrowed()], 5000).unwrap()[0]);
        }
        sys::signal_through_pidfd(parent.borrowed(), Signal::SIGTERM as i32, false).unwrap();
        let mut traffic = Vec::new();
        host.read_to_end(&mut traffic).unwrap();
        only_stdout_prefix(&traffic);
    });
    let mut runtime =
        Runtime::new(&[CString::new(helper.process.raw().to_string()).unwrap()]).unwrap();
    let mut session = Session::with_runtime(descriptor, &mut runtime).unwrap();
    session.send_attach().unwrap();
    assert!(matches!(session.next_event(), Ok(Event::Reset)));
    session.acknowledge().unwrap();
    assert_eq!(
        session.send_app(APP_STDOUT, 9, 0, &[0; 32768]),
        Err(if helper_ends {
            Errno::ECHILD
        } else {
            Errno::ECANCELED
        })
    );
    if helper_ends {
        let failure = session.runtime_failure().unwrap();
        assert!(failure.contains("the time daemon"), "{failure}");
        assert!(failure.contains("signal 9"), "{failure}");
    } else {
        assert!(session.stopping());
        assert!(session.runtime_failure().is_none());
    }
    drop(session);
    drop(runtime);
    completed(proof);
}

#[test]
#[ignore = "requires root with CAP_SYS_NICE and CAP_SYS_RESOURCE"]
fn privileged_layered_runtime_stops_with_backpressured_control_output() {
    isolated(
        "NVX_LAYERED_RUNTIME_OUTPUT_STOP_CHILD",
        "guest::managed_agent::dispatch::tests::runtime::privileged_layered_runtime_stops_with_backpressured_control_output",
        || backpressured_termination(false),
    );
}

#[test]
#[ignore = "requires root with CAP_SYS_NICE and CAP_SYS_RESOURCE"]
fn privileged_layered_runtime_helper_failure_wins_with_backpressured_control_output() {
    isolated(
        "NVX_LAYERED_RUNTIME_OUTPUT_HELPER_CHILD",
        "guest::managed_agent::dispatch::tests::runtime::privileged_layered_runtime_helper_failure_wins_with_backpressured_control_output",
        || backpressured_termination(true),
    );
}

#[test]
#[ignore = "requires root with CAP_SYS_NICE and CAP_SYS_RESOURCE"]
fn privileged_layered_runtime_stops_before_attach_on_pending_sigterm() {
    isolated(
        "NVX_LAYERED_RUNTIME_ATTACH_STOP_CHILD",
        "guest::managed_agent::dispatch::tests::runtime::privileged_layered_runtime_stops_before_attach_on_pending_sigterm",
        || {
            let helper = helper();
            let mut runtime =
                Runtime::new(&[CString::new(helper.process.raw().to_string()).unwrap()]).unwrap();
            let (mut host, guest) = UnixStream::pair().unwrap();
            let mut session =
                Session::with_runtime(FileDescriptor::from_owned(guest.into()), &mut runtime)
                    .unwrap();
            let process = sys::open_pidfd(sys::current_process()).unwrap();
            sys::signal_through_pidfd(process.borrowed(), Signal::SIGTERM as i32, false).unwrap();
            let mut system = TestSystem::new(Paths::default());
            let config =
                AgentConfig::layered(b"nvx_identity=1000:1000", c"workload".into(), c"/".into())
                    .unwrap();
            let mut mappings = HostMappings::announced_for(&config, 0);
            assert!(matches!(
                run(&mut system, &mut session, &config, &mut mappings),
                Ok(Outcome::Stopped)
            ));
            assert!(system.diagnostics.is_empty(), "{:?}", system.diagnostics);
            drop(session);
            drop(runtime);
            let mut traffic = Vec::new();
            host.read_to_end(&mut traffic).unwrap();
            assert!(
                traffic.is_empty(),
                "pending termination must not send ATTACH"
            );
        },
    );
}

#[test]
#[ignore = "requires root with CAP_SYS_NICE and CAP_SYS_RESOURCE"]
fn privileged_layered_runtime_delivers_a_write_collected_status_before_driver_events() {
    isolated(
        "NVX_LAYERED_RUNTIME_OUTPUT_STATUS_CHILD",
        "guest::managed_agent::dispatch::tests::runtime::privileged_layered_runtime_delivers_a_write_collected_status_before_driver_events",
        || {
            let temporary = tempfile::tempdir().unwrap();
            let pid_path = temporary.path().join("leader");
            let gate = sys::path_c_string(&temporary.path().join("gate")).unwrap();
            crate::guest::sys::tests::create_fifo(&gate, sys::Mode::from_bits_retain(0o600))
                .unwrap();
            let payload = workload_payload(
                &[
                    "/bin/sh",
                    "-c",
                    "printf '%s' \"$$\" > \"$1\"; head -c 6000 /dev/zero; read -r go < \"$2\"; exit 7",
                    "sh",
                    pid_path.to_str().unwrap(),
                    gate.to_str().unwrap(),
                ],
                Some("/"),
            )
            .unwrap();
            let helper = helper();
            let parent = sys::open_pidfd(sys::current_process()).unwrap();
            let (descriptor, _peer, proof) = peer(|mut host, probe| {
                let _guard = KillParentOnPanic(&parent);
                narrow_output(&probe);
                reset_peer(&mut host);
                host.write_all(
                    &encode_outer(
                        OUTER_DATA,
                        &INSTANCE,
                        1,
                        1,
                        &encode_app(APP_EXEC, 9, 0, &payload),
                    )
                    .unwrap(),
                )
                .unwrap();
                assert_eq!(read_outer(&mut host).0.record_type, OUTER_CREDIT);
                output_is_blocked(&probe);
                drop(probe);
                let id: i64 = std::fs::read_to_string(&pid_path).unwrap().parse().unwrap();
                let leader = sys::open_pidfd(ProcessId::from_positive(id).unwrap()).unwrap();
                let mut release = None;
                until(|| {
                    match sys::open(
                        &gate,
                        sys::OpenFlags::O_WRONLY | sys::OpenFlags::O_NONBLOCK,
                        sys::Mode::empty(),
                    ) {
                        Ok(writer) => {
                            release = Some(writer);
                            true
                        }
                        Err(Errno::ENXIO) => false,
                        Err(error) => panic!("cannot release the owned leader: {error}"),
                    }
                });
                release.unwrap().write_all(b"go\n").unwrap();
                assert!(sys::readable_or_hung_up(&[leader.borrowed()], 5000).unwrap()[0]);
                until(|| !Path::new(&format!("/proc/{id}")).exists());
                let mut output = Vec::new();
                loop {
                    let frame = receive(&mut host);
                    if frame.kind == APP_EXIT {
                        assert_eq!(frame, response(APP_EXIT, 9, 7, b"exit"));
                        break;
                    }
                    assert_eq!((frame.kind, frame.request_id), (APP_STDOUT, 9));
                    output.extend(frame.payload);
                }
                assert_eq!(output, vec![0; 6000]);
                host.write_all(
                    &encode_outer(
                        OUTER_DATA,
                        &INSTANCE,
                        1,
                        2,
                        &encode_app(APP_STOP, 99, 0, &[]),
                    )
                    .unwrap(),
                )
                .unwrap();
                assert_eq!(read_outer(&mut host).0.record_type, OUTER_CREDIT);
                assert_eq!(receive(&mut host), response(APP_STOPPED, 99, 0, &[]));
            });
            let mut runtime =
                Runtime::new(&[CString::new(helper.process.raw().to_string()).unwrap()]).unwrap();
            let mut session = Session::with_runtime(descriptor, &mut runtime).unwrap();
            let mut system = TestSystem::new(Paths::default());
            system.direct_starter = Some(super::super::super::seam::tests::start_as_privileged);
            let config =
                AgentConfig::layered(b"nvx_identity=1000:1000", c"workload".into(), c"/".into())
                    .unwrap();
            let mut mappings = HostMappings::announced_for(&config, 0);
            assert!(matches!(
                run(&mut system, &mut session, &config, &mut mappings),
                Ok(Outcome::Stopped)
            ));
            assert!(system.diagnostics.is_empty(), "{:?}", system.diagnostics);
            drop(session);
            drop(runtime);
            completed(proof);
        },
    );
}
