use std::thread;
use std::time::Instant;

use crate::host::session::{ControlSession, ExecEvent, SessionError};
use crate::protocol::{
    CAPABILITY_LEN, ExitCategory, GuestFeatures, HostMap, Workload, WorkloadEnvironment,
};

use super::super::AgentConfig;
use super::super::dispatch;
use super::super::mappings::HostMappings;
use super::super::session::Session;
use super::*;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn run_exec(
    session: &mut ControlSession,
    workload: &Workload<'_>,
) -> (Vec<u8>, Vec<u8>, ExecEvent) {
    let id = session.start_exec(workload, deadline()).unwrap();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        match session.next_exec_event(id, Some(deadline())).unwrap() {
            ExecEvent::Stdout(bytes) => stdout.extend(bytes),
            ExecEvent::Stderr(bytes) => stderr.extend(bytes),
            event @ ExecEvent::Exit { .. } | event @ ExecEvent::Rejected { .. } => {
                return (stdout, stderr, event);
            }
        }
    }
}

#[test]
fn control_session_broker_and_real_agent_loop_back() {
    if super::super::isolate_process_test(
        "NVX_AGENT_LOOPBACK_TEST_CHILD",
        "guest::managed_agent::tests::loopback::tests::control_session_broker_and_real_agent_loop_back",
    ) {
        return;
    }
    let _process = super::super::PROCESS_TEST_LOCK.lock().unwrap();
    let (temporary, mut system) = TestSystem::with_direct();
    system.paths.hostfs_dir = temporary.path().join("hostfs");
    std::fs::create_dir_all(system.paths.hostfs_dir.join("root/0/source")).unwrap();
    // The workloads use the plain-process core with simulated credential calls.
    system.direct_starter = Some(start_as_privileged);
    let target = temporary.path().canonicalize().unwrap().join("mapped");
    let (broker_guest, agent) = UnixStream::pair().unwrap();
    let agent_descriptor = FileDescriptor::from_owned(agent.into());
    let agent_thread = thread::spawn(move || {
        let mut system = system;
        let config = AgentConfig {
            rootfs: c"-".to_owned(),
            hostname: c"loopback".to_owned(),
            uid: c"1000".to_owned(),
            gid: c"1000".to_owned(),
            user: c"workload".to_owned(),
            home: c"/".to_owned(),
            direct: true,
            entry: None,
        };
        let mut session = Session::new(agent_descriptor);
        let mut mappings = HostMappings::announced(1);
        let outcome = dispatch::run(&mut system, &mut session, &config, &mut mappings).unwrap();
        (outcome, system)
    });
    let core = Arc::new(Mutex::new(BrokerCore::new(broker_guest)));
    let broker = BrokerHandle { core };
    let capability = [0x5a; CAPABILITY_LEN];
    let mut session = ControlSession::attach(broker.transport(), &capability, deadline()).unwrap();

    session.ping(deadline()).unwrap();
    assert_eq!(
        session.features(deadline()).unwrap(),
        GuestFeatures::DIRECT_AGENT
    );
    session
        .map_host_paths(
            &[HostMap {
                source: "0/source".to_owned(),
                target: target.to_string_lossy().into_owned(),
                read_only: true,
            }],
            deadline(),
        )
        .unwrap();

    let workload = |script: &str, timeout_ms| Workload {
        argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
        timeout_ms,
        cwd: None,
        environment: WorkloadEnvironment::Default,
    };
    let (stdout, stderr, exit) = run_exec(
        &mut session,
        &workload("printf out; printf err >&2; exit 7", 0),
    );
    assert_eq!((stdout, stderr), (b"out".to_vec(), b"err".to_vec()));
    assert_eq!(
        exit,
        ExecEvent::Exit {
            category: ExitCategory::Exit,
            status: 7
        }
    );

    let (_, _, signal) = run_exec(&mut session, &workload("kill -TERM $$", 0));
    assert_eq!(
        signal,
        ExecEvent::Exit {
            category: ExitCategory::Signal,
            status: 128 + sys::Signal::SIGTERM as i32
        }
    );
    let (_, _, realtime_signal) = run_exec(&mut session, &workload("kill -40 $$", 0));
    assert_eq!(
        realtime_signal,
        ExecEvent::Exit {
            category: ExitCategory::Signal,
            status: 128 + 40
        }
    );
    let (_, _, timeout) = run_exec(&mut session, &workload("sleep 10", 25));
    assert_eq!(
        timeout,
        ExecEvent::Exit {
            category: ExitCategory::Timeout,
            status: 124
        }
    );
    let (_, _, limited) = run_exec(&mut session, &workload("head -c 1048577 /dev/zero", 0));
    assert_eq!(
        limited,
        ExecEvent::Exit {
            category: ExitCategory::OutputLimit,
            status: 125
        }
    );

    let cancel_id = session
        .start_exec(&workload("sleep 10", 0), deadline())
        .unwrap();
    session.cancel_exec(cancel_id, deadline()).unwrap();
    assert_eq!(
        session
            .next_exec_event(cancel_id, Some(deadline()))
            .unwrap(),
        ExecEvent::Exit {
            category: ExitCategory::Cancelled,
            status: 128 + sys::Signal::SIGKILL as i32
        }
    );

    let reset_id = session
        .start_exec(&workload("sleep 10", 0), deadline())
        .unwrap();
    broker.reset();
    assert!(matches!(
        session.next_exec_event(reset_id, Some(deadline())),
        Err(SessionError::Reset)
    ));
    drop(session);
    let mut session = ControlSession::attach(broker.transport(), &capability, deadline()).unwrap();
    session.ping(deadline()).unwrap();
    session.stop(deadline()).unwrap();
    drop(session);

    let (_outcome, system) = agent_thread.join().unwrap();
    assert_eq!(system.kernel.moves(), [target.display().to_string()]);
    assert!(system.diagnostics.is_empty());
}

/// Once the output cap is hit, the agent forwards nothing more: each stream the client
/// receives stays a prefix of what the workload wrote, with no silent gap where a later
/// chunk fit the leftover room.
#[test]
fn output_limit_forwards_nothing_after_the_cap() {
    if super::super::isolate_process_test(
        "NVX_AGENT_OUTPUT_LIMIT_CHILD",
        "guest::managed_agent::tests::loopback::tests::output_limit_forwards_nothing_after_the_cap",
    ) {
        return;
    }
    let _process = super::super::PROCESS_TEST_LOCK.lock().unwrap();
    let (temporary, mut system) = TestSystem::with_direct();
    system.paths.hostfs_dir = temporary.path().join("hostfs");
    std::fs::create_dir_all(system.paths.hostfs_dir.join("root/0/source")).unwrap();
    // The workload uses the plain-process core with simulated credential calls.
    system.direct_starter = Some(start_as_privileged);
    let target = temporary.path().canonicalize().unwrap().join("mapped");
    let (broker_guest, agent) = UnixStream::pair().unwrap();
    let agent_descriptor = FileDescriptor::from_owned(agent.into());
    let agent_thread = thread::spawn(move || {
        let mut system = system;
        let config = AgentConfig {
            rootfs: c"-".to_owned(),
            hostname: c"loopback".to_owned(),
            uid: c"1000".to_owned(),
            gid: c"1000".to_owned(),
            user: c"workload".to_owned(),
            home: c"/".to_owned(),
            direct: true,
            entry: None,
        };
        let mut session = Session::new(agent_descriptor);
        let mut mappings = HostMappings::announced(1);
        let outcome = dispatch::run(&mut system, &mut session, &config, &mut mappings).unwrap();
        (outcome, system)
    });
    let core = Arc::new(Mutex::new(BrokerCore::new(broker_guest)));
    let broker = BrokerHandle { core };
    let capability = [0x5a; CAPABILITY_LEN];
    let mut session = ControlSession::attach(broker.transport(), &capability, deadline()).unwrap();
    session.ping(deadline()).unwrap();
    session
        .map_host_paths(
            &[HostMap {
                source: "0/source".to_owned(),
                target: target.to_string_lossy().into_owned(),
                read_only: true,
            }],
            deadline(),
        )
        .unwrap();

    // 32 KiB of 'b', which is one full read of the agent, then 50 bytes of 'c', in one
    // write, after 1 MiB minus `room` 'a's that the agent forwards first.
    let chunk = temporary.path().join("chunk");
    let mut bytes = vec![b'b'; 32 * 1024];
    bytes.extend_from_slice(&[b'c'; 50]);
    std::fs::write(&chunk, &bytes).unwrap();
    let room = 100usize;
    let script = format!(
        "head -c {} /dev/zero | tr '\\0' a; sleep 1; dd if={} bs=65536 count=1 2>/dev/null; sleep 10",
        crate::protocol::MAX_OUTPUT_BYTES - room,
        chunk.display()
    );
    let workload = Workload {
        argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
        timeout_ms: 0,
        cwd: None,
        environment: WorkloadEnvironment::Default,
    };
    let (stdout, stderr, exit) = run_exec(&mut session, &workload);
    let count = |byte: u8| stdout.iter().filter(|&&each| each == byte).count();
    assert_eq!(
        exit,
        ExecEvent::Exit {
            category: ExitCategory::OutputLimit,
            status: 125
        }
    );
    assert!(stderr.is_empty());
    assert_eq!(count(b'a'), crate::protocol::MAX_OUTPUT_BYTES - room);
    assert!(count(b'b') <= room);
    assert_eq!(count(b'c'), 0, "output after a dropped chunk was forwarded");
    let tail = &stdout[stdout.len().saturating_sub(room)..];
    assert!(tail.iter().all(|&each| each == b'a' || each == b'b'));
    session.stop(deadline()).unwrap();
    drop(session);
    let _ = agent_thread.join().unwrap();
}
