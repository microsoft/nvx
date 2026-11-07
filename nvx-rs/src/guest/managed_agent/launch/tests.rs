use std::ffi::CString;

use crate::guest::spawn::LaunchStage;
use crate::guest::sys::PreparedChild;
use crate::protocol::GuestExecPayload;

use super::*;

fn config(direct: bool) -> AgentConfig {
    AgentConfig {
        rootfs: CString::new(if direct { "-" } else { "/rootfs" }).unwrap(),
        hostname: c"sandbox".to_owned(),
        uid: c"1000".to_owned(),
        gid: c"1001".to_owned(),
        user: c"workload".to_owned(),
        home: c"/home/workload".to_owned(),
        direct,
        entry: None,
    }
}

fn execution() -> GuestExecPayload {
    GuestExecPayload {
        timeout_ms: 0,
        argv: vec![b"/bin/echo".to_vec(), b"ok".to_vec()],
        cwd: Some(b"/work".to_vec()),
        environment: None,
        inherit_default_env: false,
    }
}

fn descriptor() -> FileDescriptor {
    FileDescriptor::from_owned(tempfile::tempfile().unwrap().into())
}

fn request<'a>(config: &'a AgentConfig, execution: &'a GuestExecPayload) -> LaunchRequest<'a> {
    LaunchRequest {
        config,
        execution,
        stdout: descriptor(),
        stderr: descriptor(),
    }
}

/// Returns a direct agent's paths with no cgroup hierarchy.
fn direct_paths(directory: &std::path::Path) -> Paths {
    Paths {
        cgroup_root: directory.join("absent-cgroup"),
        ..Paths::default()
    }
}

/// Prepares the direct agent's plain-process child without starting it.
fn prepare_direct(
    _paths: &Paths,
    config: &AgentConfig,
    execution: &GuestExecPayload,
) -> ExecRequest {
    direct_request(config, execution).unwrap()
}

/// Prepares the child that launches `execution` with the sandbox agent's configuration.
fn prepare_sandbox(execution: &GuestExecPayload) -> PreparedChild {
    let config = config(false);
    let prepared = Prepared::simulated(None);
    sandbox_command(&Paths::default(), request(&config, execution), prepared)
        .unwrap()
        .prepare()
}

fn strings(strings: &[CString]) -> Vec<&[u8]> {
    strings.iter().map(|string| string.to_bytes()).collect()
}

#[test]
fn direct_launch_uses_the_plain_process_core_without_a_helper_or_cgroup() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = direct_paths(temporary.path());
    let config = config(true);
    let execution = execution();
    let plan = prepare_direct(&paths, &config, &execution);
    assert_eq!(strings(&plan.arguments), [b"/bin/echo".as_slice(), b"ok"]);
    assert_eq!(plan.program.as_bytes(), b"/bin/echo");
    let environment = strings(&plan.environment);
    for expected in [
        b"HOME=/home/workload".as_slice(),
        b"USER=workload",
        b"LOGNAME=workload",
        b"PWD=/work",
    ] {
        assert!(environment.contains(&expected), "{expected:?}");
    }
    assert!(!paths.cgroup_root.exists());
    assert!(!plan.input);
    assert_eq!(plan.account, AccountMode::Numeric);
}

#[test]
fn direct_launch_applies_exact_empty_and_layered_environments_in_order() {
    let paths = Paths::default();
    let config = config(true);
    let exact = GuestExecPayload {
        environment: Some(vec![
            b"EMPTY=".to_vec(),
            b"VALUE=a=b".to_vec(),
            b"VALUE=last".to_vec(),
        ]),
        ..execution()
    };
    let plan = prepare_direct(&paths, &config, &exact);
    assert_eq!(
        strings(&plan.environment),
        [b"EMPTY=".as_slice(), b"VALUE=last"]
    );
    assert!(!plan.physical_pwd);

    let empty = GuestExecPayload {
        environment: Some(Vec::new()),
        ..execution()
    };
    assert!(
        prepare_direct(&paths, &config, &empty)
            .environment
            .is_empty()
    );
    let layered = GuestExecPayload {
        environment: Some(vec![
            b"HOME=/layered".to_vec(),
            b"ADDED=1".to_vec(),
            b"ADDED=2".to_vec(),
        ]),
        inherit_default_env: true,
        ..execution()
    };
    let defaults = prepare_direct(&paths, &config, &execution());
    let mut expected = strings(&defaults.environment);
    let home = expected
        .iter()
        .position(|entry| *entry == b"HOME=/home/workload")
        .unwrap();
    expected[home] = b"HOME=/layered";
    expected.push(b"ADDED=2");
    assert_eq!(
        strings(&prepare_direct(&paths, &config, &layered).environment),
        expected
    );
}

#[test]
#[ignore = "requires a guest's root credentials and priority capabilities"]
fn privileged_direct_launch_hardens_credentials_and_resets_priority() {
    use std::os::unix::process::ExitStatusExt;
    use std::time::{Duration, Instant};

    use crate::guest::init::{Event, Exec, Stream};

    let name = "guest::managed_agent::launch::tests::privileged_direct_launch_hardens_credentials_and_resets_priority";
    if std::env::var_os("NVX_DIRECT_CREDENTIAL_TEST_CHILD").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--ignored", "--nocapture"])
            .env("NVX_DIRECT_CREDENTIAL_TEST_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    sys::write_number(sys::kernel::OOM_SCORE_ADJUSTMENT, -1000).unwrap();
    sys::kernel::set_nice(Some(sys::current_process()), -10).unwrap();
    let config = config(true);
    let execution = GuestExecPayload {
        argv: vec![
            b"/bin/sh".to_vec(),
            b"-c".to_vec(),
            b"cat /proc/self/status; printf 'oom='; cat /proc/self/oom_score_adj; \
              printf 'nice='; cut -d ' ' -f 19 /proc/self/stat; \
              if (printf '%s\\n' -1000 > /proc/self/oom_score_adj) 2>/dev/null; \
              then exit 99; fi; printf 'floor=locked\\n'"
                .to_vec(),
        ],
        cwd: None,
        ..execution()
    };
    let mut workload = Exec::start(direct_request(&config, &execution).unwrap()).unwrap();
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let mut buffer = [0; 4096];
    let started = Instant::now();
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "direct child stalled"
        );
        while let Some(reaped) = sys::reap_any_nonblocking() {
            workload.observe(
                reaped.process.raw().unsigned_abs(),
                std::process::ExitStatus::from_raw(reaped.wait_status),
            );
        }
        match workload.next_event(&mut buffer).unwrap() {
            Some(Event::Launch(report)) => assert!(report.unwrap().is_none()),
            Some(Event::Output(Stream::Stdout, bytes)) => output.extend_from_slice(bytes),
            Some(Event::Output(Stream::Stderr, bytes)) => errors.extend_from_slice(bytes),
            Some(Event::Ended(outcome)) => {
                assert_eq!(outcome.status.code(), Some(0));
                break;
            }
            Some(Event::ReadFailed(_, error) | Event::KillFailed(_, error)) => {
                panic!("{error}");
            }
            Some(_) => {}
            None => {
                let descriptors = workload.descriptors().map(|(_, fd)| fd).collect::<Vec<_>>();
                sys::readable_or_hung_up(&descriptors, 25).unwrap();
            }
        }
    }
    let output = String::from_utf8(output).unwrap();
    for expected in [
        "Uid:\t1000\t1000\t1000\t1000\n",
        "Gid:\t1001\t1001\t1001\t1001\n",
        "NoNewPrivs:\t1\n",
        "SigBlk:\t0000000000000000\n",
        "SigIgn:\t0000000000000000\n",
        "oom=0\n",
        "nice=0\n",
        "floor=locked\n",
    ] {
        assert!(
            output.contains(expected),
            "missing {expected:?} in {output}"
        );
    }
    assert!(
        output
            .lines()
            .find_map(|line| line.strip_prefix("Groups:"))
            .unwrap()
            .trim()
            .is_empty()
    );
    for set in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        assert!(
            output.contains(&format!("{set}:\t0000000000000000\n")),
            "{output}"
        );
    }
    assert!(errors.is_empty(), "{errors:?}");
    sys::write_standard_error(format!("NVX-DIRECT-CREDENTIAL-CONTROL\n{output}").as_bytes());
}

#[test]
fn sandbox_launch_runs_the_workload_itself_through_its_entry() {
    let plan = prepare_sandbox(&execution());
    assert_eq!(plan.program().as_bytes(), b"/bin/echo");
    assert_eq!(strings(plan.arguments()), [b"/bin/echo".as_slice(), b"ok"]);
    let environment = strings(plan.environment());
    for expected in [
        b"HOME=/home/workload".as_slice(),
        b"USER=workload",
        b"LOGNAME=workload",
        b"PWD=/work",
    ] {
        assert!(environment.contains(&expected), "{expected:?}");
    }
    assert!(plan.physical_pwd().is_none());
    // The agent places the child at its barrier, so the child joins no cgroup itself.
    assert!(!plan.has_cgroup());
    assert_eq!(plan.barrier_entry_and_session(), (true, true, true));
}

#[test]
fn sandbox_launch_applies_exact_and_layered_environments_in_order() {
    let exact = GuestExecPayload {
        environment: Some(vec![b"EMPTY=".to_vec(), b"VALUE=a=b".to_vec()]),
        ..execution()
    };
    let plan = prepare_sandbox(&exact);
    assert_eq!(
        strings(plan.environment()),
        [b"EMPTY=".as_slice(), b"VALUE=a=b"]
    );
    assert!(plan.physical_pwd().is_none());

    let layered = GuestExecPayload {
        environment: Some(vec![
            b"HOME=/layered".to_vec(),
            b"ADDED=1".to_vec(),
            b"ADDED=2".to_vec(),
        ]),
        inherit_default_env: true,
        ..execution()
    };
    // The request's entries layer over the defaults, wherever the agent's own environment
    // put them: a name that the defaults hold keeps its place, and a new one is appended once,
    // with its last value.
    let defaults = prepare_sandbox(&execution());
    let mut expected = strings(defaults.environment());
    let home = expected
        .iter()
        .position(|entry| *entry == b"HOME=/home/workload")
        .unwrap();
    expected[home] = b"HOME=/layered".as_slice();
    expected.push(b"ADDED=2");
    let plan = prepare_sandbox(&layered);
    assert_eq!(strings(plan.environment()), expected);
}

#[test]
fn sandbox_pwd_is_physical_only_for_noncanonical_names_without_an_entry() {
    let noncanonical = GuestExecPayload {
        cwd: Some(b"/a/../b".to_vec()),
        ..execution()
    };
    let plan = prepare_sandbox(&noncanonical);
    let index = plan.physical_pwd_index().expect("PWD is deferred");
    assert_eq!(&plan.physical_pwd().unwrap()[..4], b"PWD=");
    assert!(
        strings(plan.environment())
            .iter()
            .all(|entry| !entry.starts_with(b"PWD="))
    );
    assert!(index <= plan.environment().len());

    let named = GuestExecPayload {
        environment: Some(vec![b"PWD=/named".to_vec()]),
        inherit_default_env: true,
        ..noncanonical
    };
    let plan = prepare_sandbox(&named);
    assert!(plan.physical_pwd().is_none());
    assert!(strings(plan.environment()).contains(&b"PWD=/named".as_slice()));
}

#[test]
fn sandbox_launch_reports_a_simulated_stage_without_privileges() {
    let config = config(false);
    let execution = execution();
    let prepared = Prepared::simulated(Some((LaunchStage::Proc, Errno::EPERM)));
    let mut spawned = sandbox_command(&Paths::default(), request(&config, &execution), prepared)
        .unwrap()
        .spawn()
        .unwrap();
    let process = spawned.process();
    spawned.take_barrier().unwrap().release().unwrap();
    let failure = spawned.into_report().wait().unwrap().unwrap();
    assert_eq!(
        (failure.stage(), failure.raw_os_error()),
        (LaunchStage::Proc, Errno::EPERM as i32)
    );
    assert_eq!(sys::wait(process), Some(sys::ChildStatus::Exited(125)));
}

#[test]
fn workload_ids_are_nonzero_decimal_and_not_the_unchanged_value() {
    for (text, expected) in [
        (b"1".as_slice(), Some(1)),
        (b"1000", Some(1000)),
        (b"4294967294", Some(u32::MAX - 1)),
        (b"0", None),
        (b"4294967295", None),
        (b"4294967296", None),
        (b"+1", None),
        (b"-1", None),
        (b" 1", None),
        (b"1x", None),
    ] {
        assert_eq!(parse_id(text).ok(), expected, "{text:?}");
    }
}

#[test]
fn safe_working_directory_names_match_the_c_helper() {
    for path in [b"/".as_slice(), b"/tmp", b"/a/b", b"relative"] {
        assert_eq!(
            safe_path(path),
            path != b"/" && path != b"relative",
            "{path:?}"
        );
    }
    for path in [b"/a//b".as_slice(), b"/a/./b", b"/a/../b", b"/a/"] {
        assert!(!safe_path(path), "{path:?}");
    }
}

#[test]
fn direct_pwd_is_deferred_only_for_noncanonical_names() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = direct_paths(temporary.path());
    let config = config(true);
    for cwd in [b"/a/./b".as_slice(), b"/a/../b", b"/a//b", b"/a/b/"] {
        let execution = GuestExecPayload {
            cwd: Some(cwd.to_vec()),
            ..execution()
        };
        let plan = prepare_direct(&paths, &config, &execution);
        assert!(plan.physical_pwd, "PWD is deferred");
        assert!(
            strings(&plan.environment)
                .iter()
                .any(|entry| entry.strip_prefix(b"PWD=") == Some(cwd)),
            "{cwd:?}"
        );
    }
    let execution = GuestExecPayload {
        cwd: Some(b"/symlinked/component".to_vec()),
        ..execution()
    };
    let plan = prepare_direct(&paths, &config, &execution);
    assert!(!plan.physical_pwd);
    assert!(strings(&plan.environment).contains(&b"PWD=/symlinked/component".as_slice()));
}
