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

/// Returns a direct agent's paths, whose cgroup root, in `directory`, holds `nvx-exec`.
fn direct_paths(directory: &std::path::Path) -> Paths {
    std::fs::create_dir(directory.join("nvx-exec")).unwrap();
    std::fs::write(directory.join("nvx-exec/cgroup.procs"), b"").unwrap();
    Paths {
        cgroup_root: directory.to_owned(),
        setpriv: "/test/setpriv".into(),
        helper: "/test/nvx-managed-agent".into(),
        ..Paths::default()
    }
}

/// Prepares the child that launches `execution` with the direct agent's `config`, and
/// returns it with the number of its sealed configuration descriptor.
fn prepare_direct(
    paths: &Paths,
    config: &AgentConfig,
    execution: &GuestExecPayload,
) -> (PreparedChild, CString) {
    let config_fd = descriptor();
    let argument = config_fd.argument();
    let command = direct_command(paths, request(config, execution), config_fd).unwrap();
    (command.prepare(), argument)
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
fn direct_launch_uses_setpriv_and_clears_every_capability_set() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = direct_paths(temporary.path());
    let config = config(true);
    let execution = execution();
    let (plan, descriptor_argument) = prepare_direct(&paths, &config, &execution);
    assert_eq!(
        strings(plan.arguments()),
        [
            b"setpriv".as_slice(),
            b"--reuid",
            b"1000",
            b"--regid",
            b"1001",
            b"--clear-groups",
            b"--no-new-privs",
            b"--bounding-set=-all",
            b"--inh-caps=-all",
            b"--ambient-caps=-all",
            b"/test/nvx-managed-agent",
            b"--exec-config-fd",
            descriptor_argument.as_bytes(),
            b"--",
            b"/bin/echo",
            b"ok",
        ]
    );
    assert_eq!(plan.program().as_bytes(), b"/test/setpriv");
    let environment = strings(plan.environment());
    for expected in [
        b"HOME=/home/workload".as_slice(),
        b"USER=workload",
        b"LOGNAME=workload",
        b"PWD=/work",
    ] {
        assert!(environment.contains(&expected), "{expected:?}");
    }
    assert!(plan.has_cgroup());
    assert_eq!(plan.barrier_entry_and_session(), (false, false, false));
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
        let (plan, _) = prepare_direct(&paths, &config, &execution);
        let slot = plan.physical_pwd().expect("PWD is deferred");
        assert_eq!(&slot[..4], b"PWD=");
        assert!(slot[4..].iter().all(|&byte| byte == 0));
        assert!(plan.physical_pwd_index().is_some());
        assert!(
            strings(plan.environment())
                .iter()
                .all(|entry| !entry.starts_with(b"PWD=")),
            "{cwd:?}"
        );
    }
    let execution = GuestExecPayload {
        cwd: Some(b"/symlinked/component".to_vec()),
        ..execution()
    };
    let (plan, _) = prepare_direct(&paths, &config, &execution);
    assert!(plan.physical_pwd().is_none());
    assert!(strings(plan.environment()).contains(&b"PWD=/symlinked/component".as_slice()));
    assert!(plan.has_cgroup());
}
