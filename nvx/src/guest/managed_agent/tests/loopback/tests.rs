use std::thread;
use std::time::Instant;

use crate::host::session::{ControlSession, ExecEvent, SessionError};
use crate::protocol::v2::Outcome;
use crate::protocol::{
    CAPABILITY_LEN, HostMap, Workload, WorkloadEnvironment, encode_exec_payload,
};

use super::super::AgentConfig;
use super::super::dispatch;
use super::super::mappings::HostMappings;
use super::super::session::Session;
use super::*;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

/// Runs `workload` as the execution of `id`, with `input` as its standard input, and returns its
/// output and how it ended.
fn run_exec_with(
    session: &mut ControlSession,
    id: u64,
    workload: &Workload<'_>,
    input: &[u8],
) -> (Vec<u8>, Vec<u8>, ExecEvent) {
    session
        .start_exec(id, &encode_exec_payload(workload).unwrap(), deadline())
        .unwrap();
    let (mut stdout, mut stderr, mut rest, mut open) = (Vec::new(), Vec::new(), input, true);
    loop {
        // The window takes more of the input as the agent acknowledges what it wrote.
        let sent = session.send_stdin(id, rest, deadline()).unwrap();
        rest = &rest[sent..];
        if rest.is_empty() && open {
            session.close_stdin(id, deadline()).unwrap();
            open = false;
        }
        match session.next_exec_event(id, Some(deadline())).unwrap() {
            ExecEvent::Stdout(bytes) => stdout.extend(bytes),
            ExecEvent::Stderr(bytes) => stderr.extend(bytes),
            ExecEvent::Acknowledged(_) => {}
            event @ (ExecEvent::Exit { .. } | ExecEvent::Rejected { .. }) => {
                return (stdout, stderr, event);
            }
        }
    }
}

fn run_exec(
    session: &mut ControlSession,
    workload: &Workload<'_>,
) -> (Vec<u8>, Vec<u8>, ExecEvent) {
    run_exec_with(session, 1, workload, &[])
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
            uid: c"1000".to_owned(),
            gid: c"1000".to_owned(),
            user: c"workload".to_owned(),
            home: c"/".to_owned(),
            restored_proxy: None,
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
            category: Outcome::Exit,
            status: 7
        }
    );

    let (_, _, signal) = run_exec(&mut session, &workload("kill -TERM $$", 0));
    assert_eq!(
        signal,
        ExecEvent::Exit {
            category: Outcome::Signal,
            status: 128 + sys::Signal::SIGTERM as i32
        }
    );
    let (_, _, realtime_signal) = run_exec(&mut session, &workload("kill -40 $$", 0));
    assert_eq!(
        realtime_signal,
        ExecEvent::Exit {
            category: Outcome::Signal,
            status: 128 + 40
        }
    );
    let (_, _, timeout) = run_exec(&mut session, &workload("sleep 10", 25));
    assert_eq!(
        timeout,
        ExecEvent::Exit {
            category: Outcome::Timeout,
            status: 124
        }
    );
    // Version 2 caps no output: the old cap of 1 MiB passes whole.
    let (stdout, _, uncapped) = run_exec(&mut session, &workload("head -c 1048577 /dev/zero", 0));
    assert_eq!(stdout, vec![0; 1024 * 1024 + 1]);
    assert_eq!(
        uncapped,
        ExecEvent::Exit {
            category: Outcome::Exit,
            status: 0
        }
    );
    // The workload reads the standard input that the host streams, more than the window holds,
    // until its end.
    let input: Vec<u8> = (0..200 * 1024).map(|index| (index % 251) as u8).collect();
    let (stdout, _, echoed) = run_exec_with(&mut session, 2, &workload("cat", 0), &input);
    assert_eq!(stdout, input);
    assert_eq!(
        echoed,
        ExecEvent::Exit {
            category: Outcome::Exit,
            status: 0
        }
    );

    let cancel_id = 3;
    session
        .start_exec(
            cancel_id,
            &encode_exec_payload(&workload("sleep 10", 0)).unwrap(),
            deadline(),
        )
        .unwrap();
    session.cancel_exec(cancel_id, deadline()).unwrap();
    assert_eq!(
        session
            .next_exec_event(cancel_id, Some(deadline()))
            .unwrap(),
        ExecEvent::Exit {
            category: Outcome::Cancelled,
            status: 128 + sys::Signal::SIGKILL as i32
        }
    );

    let reset_id = 4;
    session
        .start_exec(
            reset_id,
            &encode_exec_payload(&workload("sleep 10", 0)).unwrap(),
            deadline(),
        )
        .unwrap();
    broker.reset();
    assert!(matches!(
        session.next_exec_event(reset_id, Some(deadline())),
        Err(SessionError::Reset)
    ));
    drop(session);
    let mut session = ControlSession::attach(broker.transport(), &capability, deadline()).unwrap();
    session.ping(deadline()).unwrap();
    // A stop during an execution ends it first.
    session
        .start_exec(
            5,
            &encode_exec_payload(&workload("sleep 10", 0)).unwrap(),
            deadline(),
        )
        .unwrap();
    session.stop(deadline()).unwrap();
    drop(session);

    let (_outcome, system) = agent_thread.join().unwrap();
    assert_eq!(system.kernel.moves(), [target.display().to_string()]);
    assert!(system.diagnostics.is_empty());
}
