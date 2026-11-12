//! The runs of `nvx-rs sandbox run`, each in a sandbox of its own, without a state directory: a
//! run launches the VM that a `provision` and a `start` of its options would launch, in a fresh
//! temporary VM directory, prints its OpenVMM command as `>> COMMAND`, executes its workload once
//! over NVXC with the execution options of `exec`, stops the VM as `stop` does, removes the VM
//! directory, and exits with the workload's status after the workload's output.

use std::fs;
use std::path::Path;

use serde_json::json;

use crate::harness::{
    HYPERVISOR, LAYER, LINE_END, OPENVMM, Outcome, Root, SCRATCH, Snapshot, UUID, in_parallel,
};
use crate::lifecycle::{OWN_FILES, exported, mapped, own_files_root, share_host};
use crate::openvmm::{
    NO_NETWORK, Options, assert_opened_distro, assert_opened_images, assert_outcome_report,
    assert_stable_blocks, managed_run,
};

/// Returns `nvx-rs sandbox run` of the distro layer and scratch image of the root with `options`.
fn sandbox_run(options: &[&str]) -> Vec<String> {
    ["sandbox", "run", "--layer", LAYER, "--scratch", SCRATCH]
        .iter()
        .chain(options)
        .map(|argument| (*argument).to_owned())
        .collect()
}

/// What a run printed of the VM that it launched.
struct Ran {
    /// The options of the OpenVMM command that it printed, with the outputs of its VM directory
    /// named as [`managed_run`] names them.
    options: Options,
    /// The name of its VM directory, which names the records of the fake OpenVMM.
    token: String,
}

/// Returns `snapshot`, a snapshot of a root, without the records of the fake OpenVMM, the outcome
/// reports, and the lock file of the root's downloads.
fn kept(snapshot: &Snapshot) -> Snapshot {
    snapshot
        .iter()
        .filter(|(path, _)| {
            !path.starts_with("build/fake-openvmm-")
                && !path.starts_with("reports/")
                && *path != "build/.nvx-download.lock"
        })
        .map(|(path, node)| (path.clone(), node.clone()))
        .collect()
}

/// Returns the options of the OpenVMM command that `outcome`, a run in `root`, printed, with the
/// outputs of its VM directory named as [`managed_run`] names them, once it has checked that the
/// run exited with `status` after the workload's standard output `stdout`, that the fake OpenVMM
/// received the command, and that the run left the root as it held `before`, but for the records
/// of the fake OpenVMM and the outcome reports: without a VM directory or a state directory.
fn assert_launched(
    root: &Root,
    before: &Snapshot,
    outcome: &Outcome,
    status: i32,
    stdout: &str,
) -> Ran {
    assert_eq!(outcome.status, Some(status), "{}", outcome.describe());
    let (command, rest) = outcome.command();
    assert_eq!(rest, stdout.as_bytes(), "{}", outcome.describe());
    let (program, arguments) = command.split_first().unwrap();
    assert_eq!(*program, root.below(OPENVMM).to_str().unwrap());
    let options = Options::parse(arguments);
    let report = Path::new(options.value("--microvm-report"));
    let token = report
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap()
        .to_owned();
    assert_eq!(
        root.openvmm_arguments(&format!("build/fake-openvmm-{token}.json")),
        arguments,
        "{}",
        outcome.describe()
    );
    assert_eq!(
        kept(&root.snapshot()),
        kept(before),
        "{}",
        outcome.describe()
    );
    Ran {
        options: options.normalized_run_outputs(),
        token,
    }
}

/// Asserts that `outcome`, a run in `root`, which held `before`, ran its workload as
/// [`assert_launched`] says, in a VM whose command line holds `nvx_sandbox=1` and the CLI's managed
/// mode, and no OpenVMM lifecycle or workload option, or token of the workload, and that the run
/// attached to the VM once, through which it executed its workload and then stopped the VM.
fn assert_ran(root: &Root, before: &Snapshot, outcome: &Outcome, status: i32, stdout: &str) -> Ran {
    let ran = assert_launched(root, before, outcome, status, stdout);
    for option in ["--microvm-lifecycle", "--microvm-workload-identity"] {
        assert!(
            ran.options.values(option).is_empty(),
            "{option}: {}",
            outcome.describe()
        );
    }
    assert_eq!(ran.options.value("--virtio-console"), "none");
    assert_eq!(ran.options.value("--guest-shutdown-action"), "exit");
    let line = ran.options.value("--cmdline");
    let tokens: Vec<&str> = line.split_ascii_whitespace().collect();
    assert_eq!(
        tokens
            .iter()
            .filter(|&&token| token == "nvx_sandbox=1")
            .count(),
        1,
        "{line}"
    );
    assert!(tokens.contains(&"nvx_mode=managed"), "{line}");
    // The workload is the execution's, which no token of the kernel command line carries.
    assert!(
        !tokens
            .iter()
            .any(|token| { token.starts_with("nvx_entrypoint=") || token.starts_with("nvx_arg=") }),
        "{line}"
    );
    assert_eq!(
        fs::read_to_string(root.below(&format!("build/fake-openvmm-{}-sessions.log", ran.token)))
            .unwrap(),
        "connection\n",
        "{}",
        outcome.describe()
    );
    ran
}

#[test]
fn runs_of_every_format_and_upper_execute_once_and_stop_their_vm() {
    let mut cases = Vec::new();
    for format in ["erofs", "gpt"] {
        for (upper, cap, expected_cap) in [
            ("scratch", None, None),
            ("ram", None, Some(50)),
            ("ram", Some("37"), Some(37)),
        ] {
            cases.push((format, upper, cap, expected_cap));
        }
    }
    in_parallel(&cases, |&(format, upper, cap, expected_cap)| {
        let context = format!("{format}/{upper}/{cap:?}");
        let root = Root::new();
        let mut arguments = vec![
            "sandbox",
            "run",
            "--layer",
            LAYER,
            "--layer-format",
            format,
            "--upper",
            upper,
        ];
        if upper == "scratch" {
            arguments.extend(["--scratch", SCRATCH]);
        } else {
            // A RAM upper needs no scratch image.
            root.remove("scratch.ext4");
        }
        if let Some(cap) = cap {
            arguments.extend(["--ram-upper-percent", cap]);
        }
        arguments.extend([
            "--entrypoint",
            "/bin/echo",
            "--arg",
            "hello",
            "--arg",
            "world",
            "--outcome-report",
            "{root/reports/run.json}",
        ]);
        let before = root.snapshot();
        let outcome = root.nvx_rs(&arguments);
        let mut ran = assert_ran(&root, &before, &outcome, 0, "hello world\n");
        assert!(outcome.stderr.is_empty(), "{}", outcome.describe());
        assert_stable_blocks(
            &root,
            &ran.options.take("--microvm-sandbox-block"),
            upper == "scratch",
        );
        if upper == "scratch" {
            assert_opened_images(&root, &ran.token);
        } else {
            assert_opened_distro(&root, &ran.token);
        }
        let line = ran.options.value("--cmdline");
        let tokens: Vec<&str> = line.split_ascii_whitespace().collect();
        assert!(
            tokens.contains(&format!("nvx_layer=distro,0xd0003000,{format}:{UUID}").as_str()),
            "{context}: {line}"
        );
        if let Some(cap) = expected_cap {
            assert!(
                tokens.contains(&format!("nvx_ram_upper_percent={cap}").as_str()),
                "{context}: {line}"
            );
            assert!(
                tokens.contains(&"nvx_overlay_upper=ramfs"),
                "{context}: {line}"
            );
            assert!(
                !tokens.iter().any(|token| token.starts_with("nvx_scratch")),
                "{context}: {line}"
            );
        } else {
            assert!(
                tokens.contains(&"nvx_scratch=0xd0006000,ext4"),
                "{context}: {line}"
            );
            assert!(
                !tokens
                    .iter()
                    .any(|token| token.starts_with("nvx_ram_upper_percent=")),
                "{context}: {line}"
            );
        }
        // The guest stopped the VM, as `stop` asks it to, and the run forwards OpenVMM's report.
        assert_outcome_report(
            &root.json("reports/run.json"),
            HYPERVISOR,
            ("run", "success", 0),
            NO_NETWORK,
        );
    });
}

#[test]
fn runs_execute_their_workload_with_the_options_of_exec() {
    let path = "/usr/sbin:/usr/bin:/sbin:/bin\n";
    let cases: [(&[&str], i32, &str, &str); 9] = [
        (&["--entrypoint", "/bin/pwd"], 0, "/\n", ""),
        (
            &["--entrypoint", "/bin/pwd", "--cwd", "/tmp"],
            0,
            "/tmp\n",
            "",
        ),
        (
            &[
                "--entrypoint",
                "/usr/bin/env",
                "--environment",
                "A=1",
                "--environment",
                "B=two words",
            ],
            0,
            "A=1\nB=two words\n",
            "",
        ),
        (
            &[
                "--entrypoint",
                "/usr/bin/env",
                "--environment-file",
                "{root/env.json}",
            ],
            0,
            "X=1\nY=two words\n",
            "",
        ),
        // The guest's default environment, without an environment of the run, beneath it with
        // --inherit-default-environment, and replaced by it otherwise.
        (
            &["--entrypoint", "/usr/bin/printenv", "--arg", "PATH"],
            0,
            path,
            "",
        ),
        (
            &[
                "--entrypoint",
                "/usr/bin/printenv",
                "--arg",
                "PATH",
                "--environment",
                "A=1",
                "--inherit-default-environment",
            ],
            0,
            path,
            "",
        ),
        (
            &[
                "--entrypoint",
                "/usr/bin/printenv",
                "--arg",
                "PATH",
                "--environment",
                "A=1",
            ],
            1,
            "",
            "",
        ),
        // The arguments may hold whitespace, and the workload's status and standard error reach
        // the caller.
        (
            &[
                "--entrypoint",
                "/bin/sh",
                "--arg=-c",
                "--arg",
                "echo two  words; echoerr oops; exit 7",
            ],
            7,
            "two  words\n",
            "oops\n",
        ),
        (
            &[
                "--entrypoint",
                "/bin/sh",
                "--arg=-c",
                "--arg",
                "sleep 60000",
                "--exec-timeout-ms",
                "50",
            ],
            124,
            "",
            "",
        ),
    ];
    in_parallel(&cases, |&(options, status, stdout, stderr)| {
        let root = Root::new();
        root.write("env.json", r#"["X=1","Y=two words"]"#);
        let before = root.snapshot();
        let outcome = root.nvx_rs(&sandbox_run(options));
        assert_ran(&root, &before, &outcome, status, stdout);
        assert_eq!(outcome.stderr(), stderr, "{}", outcome.describe());
    });
}

#[test]
fn runs_map_their_shares_before_they_execute_their_workload() {
    let root = Root::new();
    let before = root.snapshot();
    let outcome = root.nvx_rs(&sandbox_run(&[
        "--mount",
        "/data,{root/share}",
        "--entrypoint",
        "/bin/pwd",
        "--cwd",
        "/data",
    ]));
    let mut ran = assert_ran(&root, &before, &outcome, 0, "/data\n");
    assert!(outcome.stderr.is_empty(), "{}", outcome.describe());
    // The VM is the one that a start of the same sandbox launches.
    assert_stable_blocks(&root, &ran.options.take("--microvm-sandbox-block"), true);
    let mut expected = exported(managed_run(), &["0,{root/share},ro"]).persistent_expected();
    expected.take("--microvm-sandbox-block");
    expected.assert_matches(&root, &ran.options, "one share");
    assert_eq!(
        root.json(&format!("build/fake-openvmm-{}-maps.json", ran.token)),
        json!([mapped("0", "/data", true, true)])
    );
}

/// A run refuses a share that reaches the image, scratch image, OpenVMM, kernel, or initramfs of
/// its VM before it launches anything, as `start` does. It has no state directory, so a share of
/// a directory that would be a sandbox's state directory is one like any other.
#[test]
fn runs_refuse_shares_of_their_own_files() {
    in_parallel(&OWN_FILES, |&(host, name)| {
        let root = own_files_root();
        fs::create_dir_all(root.below("control/state/inside")).unwrap();
        let host = share_host(&root, host);
        let layer = format!("distro,{{root/images/distro.erofs}},{UUID}");
        let mount = format!("/data,{host}");
        let arguments = [
            "sandbox",
            "run",
            "--layer",
            &layer,
            "--scratch",
            "{root/upper/scratch.ext4}",
            "--mount",
            &mount,
            "--entrypoint",
            "/bin/pwd",
            "--cwd",
            "/data",
        ];
        let before = root.snapshot();
        let outcome = root.nvx_rs(&arguments);
        if name == "state directory" {
            assert_ran(&root, &before, &outcome, 0, "/data\n");
        } else {
            outcome.assert_error("unsupported", &[&format!("overlaps the sandbox's {name} ")]);
            assert_eq!(root.snapshot(), before, "{host}");
        }
    });
}

/// A run runs the OpenVMM that `NVX_OPENVMM` names, if set, rather than the root's, so it refuses a
/// share that reaches that one too, through which the workload could change what the host runs.
#[test]
fn a_run_refuses_a_share_of_the_openvmm_that_the_variable_names() {
    let root = Root::new();
    root.write("tools/openvmm{exe}", "openvmm\n");
    let before = root.snapshot();
    root.nvx_rs_with(
        &sandbox_run(&["--mount", "/data,{root/tools}"]),
        &[("NVX_OPENVMM", "{root/tools/openvmm{exe}}")],
    )
    .assert_error("unsupported", &["overlaps the sandbox's OpenVMM "]);
    assert_eq!(root.snapshot(), before);
}

/// A run refuses a share that reaches the VM directory that it would create in the temporary
/// directory, which holds what OpenVMM writes, as `start` refuses one of a state directory.
#[test]
fn a_run_refuses_a_share_of_its_vm_directory() {
    let root = Root::new();
    fs::create_dir(root.below("temporary")).unwrap();
    let before = root.snapshot();
    root.nvx_rs_with(
        &sandbox_run(&["--mount", "/data,{root/temporary}"]),
        &[
            ("TMPDIR", "{root/temporary}"),
            ("TMP", "{root/temporary}"),
            ("TEMP", "{root/temporary}"),
        ],
    )
    .assert_error("unsupported", &["overlaps the sandbox's VM directory "]);
    assert_eq!(root.snapshot(), before);
}

#[test]
fn a_run_whose_guest_exits_during_startup_fails_as_a_start_does() {
    let root = Root::new();
    let before = root.snapshot();
    let outcome = root.nvx_rs(&sandbox_run(&[
        "--entrypoint",
        "/bin/pwd",
        "--cmdline",
        "fake_exit_during_attach=5",
        "--outcome-report",
        "{root/reports/run.json}",
    ]));
    assert_launched(&root, &before, &outcome, 1, "");
    assert_eq!(
        outcome.stderr(),
        format!("error[exited]: OpenVMM exited during startup: guest-exit status 5{LINE_END}")
    );
    assert_outcome_report(
        &root.json("reports/run.json"),
        HYPERVISOR,
        ("run", "guest-exit", 5),
        NO_NETWORK,
    );
}

/// A run streams its workload's standard input, which the command's ends, and its output.
#[test]
fn a_run_streams_its_standard_input_and_output() {
    let root = Root::new();
    let before = root.snapshot();
    let outcome = root.nvx_rs_with_input(
        &sandbox_run(&[
            "--entrypoint",
            "/bin/sh",
            "--arg=-c",
            "--arg",
            "cat; echoerr ended",
        ]),
        b"line one\nline two\n",
    );
    assert_ran(&root, &before, &outcome, 0, "line one\nline two\n");
    assert_eq!(outcome.stderr(), "ended\n", "{}", outcome.describe());
}

#[test]
fn a_run_whose_workload_the_guest_refuses_stops_its_vm() {
    let root = Root::new();
    let before = root.snapshot();
    let outcome = root.nvx_rs(&sandbox_run(&[
        "--entrypoint",
        "/bin/pwd",
        "--cwd",
        "/missing",
        "--outcome-report",
        "{root/reports/run.json}",
    ]));
    assert_ran(&root, &before, &outcome, 1, "");
    // The guest agent's diagnostic streams before the refusal, which the run reports.
    assert_eq!(
        outcome.stderr(),
        format!(
            "nvx-managed-agent: cannot enter working directory /missing: No such file or \
             directory\nerror[exec-refused]: managed guest rejected exec (status=2, \
             category=cwd-failed){LINE_END}"
        )
    );
    assert_outcome_report(
        &root.json("reports/run.json"),
        HYPERVISOR,
        ("run", "success", 0),
        NO_NETWORK,
    );
}

/// A run whose guest closes its control endpoint as it stops, before its acknowledgement leaves,
/// waits up to `--timeout` for its VM to exit, as a stop does, rather than forcing it down at
/// once: the VM exits on its own with its report, and the run warns only of the failed request.
#[test]
fn a_run_waits_for_a_vm_whose_endpoint_closes_as_it_stops() {
    let root = Root::new();
    let before = root.snapshot();
    let outcome = root.nvx_rs(&sandbox_run(&[
        "--entrypoint",
        "/bin/echo",
        "--arg",
        "once",
        "--cmdline",
        "fake_drop_stop=1 fake_exit_delay_ms=500",
        "--outcome-report",
        "{root/reports/run.json}",
    ]));
    assert_ran(&root, &before, &outcome, 0, "once\n");
    assert_eq!(
        outcome.stderr(),
        format!("warning: managed guest shutdown failed: EndpointClosed{LINE_END}"),
        "{}",
        outcome.describe()
    );
    assert_outcome_report(
        &root.json("reports/run.json"),
        HYPERVISOR,
        ("run", "success", 0),
        NO_NETWORK,
    );
}

/// A run whose VM does not stop when asked to forces it down once `--timeout` has passed, and
/// warns that it did, but exits with its workload's status all the same.
#[test]
fn a_run_forces_down_a_vm_that_does_not_stop() {
    let root = Root::new();
    let before = root.snapshot();
    let outcome = root.nvx_rs(&sandbox_run(&[
        "--entrypoint",
        "/bin/echo",
        "--arg",
        "once",
        "--timeout",
        "1",
        "--cmdline",
        "fake_ignore_stop=1",
    ]));
    assert_ran(&root, &before, &outcome, 0, "once\n");
    let stderr = outcome.stderr();
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 2, "{}", outcome.describe());
    assert!(
        lines[0].starts_with("warning: managed guest shutdown failed: "),
        "{}",
        outcome.describe()
    );
    assert_eq!(
        lines[1],
        "warning: OpenVMM was forcibly terminated after the run",
        "{}",
        outcome.describe()
    );
}

/// A run's own warnings start a line of their own after standard error that its workload left
/// without a line end, which the run otherwise leaves as it is.
#[test]
fn a_runs_warnings_start_lines_of_their_own_after_its_workloads_output() {
    let root = Root::new();
    let before = root.snapshot();
    let outcome = root.nvx_rs(&sandbox_run(&[
        "--entrypoint",
        "/bin/sh",
        "--arg=-c",
        "--arg",
        "printerr partial",
        "--timeout",
        "1",
        "--cmdline",
        "fake_ignore_stop=1",
    ]));
    assert_ran(&root, &before, &outcome, 0, "");
    let stderr = outcome.stderr();
    assert!(
        stderr.starts_with(&format!(
            "partial{LINE_END}warning: managed guest shutdown failed: "
        )),
        "{}",
        outcome.describe()
    );
    assert!(
        stderr.ends_with(&format!(
            "{LINE_END}warning: OpenVMM was forcibly terminated after the run{LINE_END}"
        )),
        "{}",
        outcome.describe()
    );
    let root = Root::new();
    let before = root.snapshot();
    let outcome = root.nvx_rs(&sandbox_run(&[
        "--entrypoint",
        "/bin/sh",
        "--arg=-c",
        "--arg",
        "printerr partial",
    ]));
    assert_ran(&root, &before, &outcome, 0, "");
    assert_eq!(outcome.stderr(), "partial", "{}", outcome.describe());
}

/// A run forwards an outcome report that the caller can read whatever its umask, even one that
/// clears the owner's own permissions, and whose group's and others' permissions the caller's
/// umask narrows: OpenVMM starts with the caller's umask without the owner's bits, and the run
/// keeps the permissions of OpenVMM's report.
#[cfg(target_os = "linux")]
#[test]
fn runs_forward_an_outcome_report_that_the_caller_reads_whatever_its_umask() {
    use crate::harness::mode;
    use crate::openvmm::FAKE_REPORT_MODE;

    for umask in [0o022, 0o400, 0o200, 0o777] {
        let root = Root::new();
        let outcome = root.nvx_rs_with_umask(
            &sandbox_run(&[
                "--entrypoint",
                "/bin/echo",
                "--arg",
                "once",
                "--outcome-report",
                "{root/reports/run.json}",
            ]),
            umask,
        );
        assert_eq!(outcome.status, Some(0), "{}", outcome.describe());
        let report = root.below("reports/run.json");
        let (permissions, expected) = (mode(&report), FAKE_REPORT_MODE & !(umask & 0o077));
        assert_eq!(
            permissions, expected,
            "umask {umask:03o}: the report has mode {permissions:03o}, not {expected:03o}"
        );
        assert_outcome_report(
            &root.json("reports/run.json"),
            HYPERVISOR,
            ("run", "success", 0),
            NO_NETWORK,
        );
    }
}
