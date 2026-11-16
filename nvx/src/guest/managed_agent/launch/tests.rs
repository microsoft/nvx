use std::ffi::CString;

use crate::guest::sys;
use crate::protocol::GuestExecPayload;

use super::*;

fn config() -> AgentConfig {
    AgentConfig {
        uid: c"1000".to_owned(),
        gid: c"1001".to_owned(),
        user: c"workload".to_owned(),
        home: c"/home/workload".to_owned(),
        restored_proxy: None,
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

/// Prepares the direct agent's plain-process child without starting it.
fn prepare_direct(config: &AgentConfig, execution: &GuestExecPayload) -> ExecRequest {
    direct_request(config, execution).unwrap()
}

fn strings(strings: &[CString]) -> Vec<&[u8]> {
    strings.iter().map(|string| string.to_bytes()).collect()
}

#[test]
fn direct_launch_uses_the_plain_process_core_without_a_helper_or_cgroup() {
    let config = config();
    let execution = execution();
    let plan = prepare_direct(&config, &execution);
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
    assert!(!plan.input);
}

#[test]
fn direct_launch_applies_exact_empty_and_layered_environments_in_order() {
    let mut config = config();
    config.restored_proxy = Some(Some("http://restore.example:3128".to_owned()));
    let exact = GuestExecPayload {
        environment: Some(vec![
            b"EMPTY=".to_vec(),
            b"VALUE=a=b".to_vec(),
            b"VALUE=last".to_vec(),
        ]),
        ..execution()
    };
    let plan = prepare_direct(&config, &exact);
    assert_eq!(
        strings(&plan.environment),
        [b"EMPTY=".as_slice(), b"VALUE=last"]
    );
    assert!(!plan.physical_pwd);

    let empty = GuestExecPayload {
        environment: Some(Vec::new()),
        ..execution()
    };
    assert!(prepare_direct(&config, &empty).environment.is_empty());
    let layered = GuestExecPayload {
        environment: Some(vec![
            b"HOME=/layered".to_vec(),
            b"ADDED=1".to_vec(),
            b"ADDED=2".to_vec(),
        ]),
        inherit_default_env: true,
        ..execution()
    };
    let defaults = prepare_direct(&config, &execution());
    let mut expected = strings(&defaults.environment);
    let home = expected
        .iter()
        .position(|entry| *entry == b"HOME=/home/workload")
        .unwrap();
    expected[home] = b"HOME=/layered";
    expected.push(b"ADDED=2");
    assert_eq!(
        strings(&prepare_direct(&config, &layered).environment),
        expected
    );
}

#[test]
fn restored_proxy_defaults_replace_template_values_before_exec_overrides() {
    const CHILD: &str = "NVX_RESTORED_PROXY_DEFAULTS_CHILD";
    let names = ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"];
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "guest::managed_agent::launch::tests::restored_proxy_defaults_replace_template_values_before_exec_overrides",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1");
        for name in names {
            child.env(name, "http://template.example:8080");
        }
        assert!(child.status().unwrap().success());
        return;
    }
    let mut config = config();
    config.restored_proxy = Some(Some("http://restore.example:3128".to_owned()));
    let plan = direct_request(&config, &execution()).unwrap();
    for name in names {
        let expected = format!("{name}=http://restore.example:3128");
        assert!(strings(&plan.environment).contains(&expected.as_bytes()));
        assert_eq!(std::env::var(name).unwrap(), "http://template.example:8080");
    }
    let layered = GuestExecPayload {
        environment: Some(vec![
            b"HTTP_PROXY=http://first.example:1".to_vec(),
            b"HTTP_PROXY=http://exec.example:2".to_vec(),
        ]),
        inherit_default_env: true,
        ..execution()
    };
    let plan = direct_request(&config, &layered).unwrap();
    assert!(strings(&plan.environment).contains(&b"HTTP_PROXY=http://exec.example:2".as_slice()));
    assert!(
        strings(&plan.environment).contains(&b"http_proxy=http://restore.example:3128".as_slice())
    );

    config.restored_proxy = Some(None);
    let plan = direct_request(&config, &execution()).unwrap();
    for name in names {
        let prefix = format!("{name}=");
        assert!(
            strings(&plan.environment)
                .iter()
                .all(|entry| !entry.starts_with(prefix.as_bytes()))
        );
    }
    assert!(strings(&plan.environment).contains(&b"HOME=/home/workload".as_slice()));
    config.restored_proxy = None;
    let plan = direct_request(&config, &execution()).unwrap();
    for name in names {
        let expected = format!("{name}=http://template.example:8080");
        assert!(strings(&plan.environment).contains(&expected.as_bytes()));
    }
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
    let config = config();
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
            Some(Event::Ended(status)) => {
                assert_eq!(status.code(), Some(0));
                break;
            }
            Some(Event::ReadFailed) => panic!("an output read failed"),
            Some(Event::KillFailed(_, error)) => panic!("{error}"),
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
    let config = config();
    for cwd in [b"/a/./b".as_slice(), b"/a/../b", b"/a//b", b"/a/b/"] {
        let execution = GuestExecPayload {
            cwd: Some(cwd.to_vec()),
            ..execution()
        };
        let plan = prepare_direct(&config, &execution);
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
    let plan = prepare_direct(&config, &execution);
    assert!(!plan.physical_pwd);
    assert!(strings(&plan.environment).contains(&b"PWD=/symlinked/component".as_slice()));
}
