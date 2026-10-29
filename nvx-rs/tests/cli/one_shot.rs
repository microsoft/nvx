//! The runs that launch OpenVMM on the terminal: one-shot sandboxes of `nvx-rs sandbox run`, and
//! the microVMs of `nvx-rs run`. Each prints the OpenVMM command as `>> COMMAND`, runs it with the
//! standard streams, which carry the workload's output, or OpenVMM's, byte for byte, and exits
//! with OpenVMM's status, which a one-shot sandbox's OpenVMM takes from its workload.

use crate::harness::{Content, LAYER, OPENVMM, Root, SCRATCH, in_parallel, native};
use crate::openvmm::{
    NO_NETWORK, NetworkPolicy, Options, WORKLOAD, assert_checked_blocks, assert_checked_distro,
    assert_opened_distro, assert_opened_images, assert_outcome_report, child, cold_boot, one_shot,
    restore, share_token,
};

/// The snapshot that a restore restores.
const SNAPSHOT: (&str, Content) = ("snapshot/state.bin", Content::Text("snapshot\n"));

/// The outcome report that OpenVMM writes for a run that asks for one.
struct Report {
    /// Its path below the root.
    path: &'static str,
    /// The category of the outcome.
    category: &'static str,
    /// Its status code.
    status: i64,
    /// Its network policy.
    network: NetworkPolicy,
}

/// A run that launches OpenVMM, and how it ends.
struct Launch {
    /// The name of the case.
    name: &'static str,
    /// The files that the case writes below the root first, with their contents.
    files: Vec<(&'static str, Content)>,
    /// The files that the case removes from the root first.
    removed: Vec<&'static str>,
    /// The environment variables that the case sets, with placeholders.
    environment: Vec<(&'static str, String)>,
    /// The arguments after `--root ROOT`, with placeholders.
    arguments: Vec<String>,
    /// The options of the OpenVMM command, with placeholders, and with the paths of a sandbox's
    /// images, which the run passes as files that it checked.
    options: Options,
    /// The record of the arguments that the fake OpenVMM received, below the root, unless it
    /// refuses them.
    record: Option<&'static str>,
    /// The status with which the run exits.
    status: i32,
    /// What follows the `>> ` line on standard output: the output of the workload or OpenVMM.
    stdout: &'static str,
    /// What OpenVMM or its workload writes to standard error, with placeholders.
    stderr: &'static str,
    /// The outcome report that OpenVMM writes, if the run asks for one.
    report: Option<Report>,
}

/// Returns the launch `name` of `arguments`, which starts OpenVMM with `options` and exits with
/// `status` and the output `stdout` of the workload or OpenVMM.
fn launch<S: ToString>(
    name: &'static str,
    arguments: impl IntoIterator<Item = S>,
    options: Options,
    status: i32,
    stdout: &'static str,
) -> Launch {
    Launch {
        name,
        files: Vec::new(),
        removed: Vec::new(),
        environment: Vec::new(),
        arguments: arguments
            .into_iter()
            .map(|argument| argument.to_string())
            .collect(),
        options,
        record: Some("build/fake-openvmm-.json"),
        status,
        stdout,
        stderr: "",
        report: None,
    }
}

impl Launch {
    /// Returns the case with the file `relative` written first, with `content`.
    fn with_file(mut self, (relative, content): (&'static str, Content)) -> Self {
        self.files.push((relative, content));
        self
    }

    /// Returns the case with the file `relative` removed first.
    fn without(mut self, relative: &'static str) -> Self {
        self.removed.push(relative);
        self
    }

    /// Returns the case with the variable `name` set to `value`.
    fn with_variable(mut self, name: &'static str, value: String) -> Self {
        self.environment.push((name, value));
        self
    }

    /// Returns the case whose OpenVMM records its arguments in `record`, or refuses them.
    fn recorded_in(mut self, record: Option<&'static str>) -> Self {
        self.record = record;
        self
    }

    /// Returns the case that writes `stderr` to standard error.
    fn with_stderr(mut self, stderr: &'static str) -> Self {
        self.stderr = stderr;
        self
    }

    /// Returns the case whose OpenVMM writes `report`.
    fn reporting(mut self, report: Report) -> Self {
        self.report = Some(report);
        self
    }
}

/// Returns `nvx-rs sandbox run` of the distro layer and scratch image of the root that runs
/// `entrypoint` with `options`.
fn sandbox_run(entrypoint: &str, options: &[&str]) -> Vec<String> {
    [
        "sandbox",
        "run",
        "--layer",
        LAYER,
        "--scratch",
        SCRATCH,
        "--entrypoint",
        entrypoint,
    ]
    .iter()
    .chain(options)
    .map(|argument| (*argument).to_owned())
    .collect()
}

/// Returns the OpenVMM options of a one-shot sandbox that runs `entrypoint` with `arguments`,
/// after the user's kernel command line `user`.
fn workload(user: &str, entrypoint: &str, arguments: &[&str]) -> Options {
    let mut tokens = vec![
        format!("nvx_entrypoint={entrypoint}"),
        WORKLOAD[1].to_owned(),
    ];
    tokens.extend(
        arguments
            .iter()
            .map(|argument| format!("nvx_arg={argument}")),
    );
    one_shot().sandbox_kernel(user, &tokens)
}

/// Returns whether the launch is a cold boot of `nvx-rs run` that fails, which `run` follows with
/// its CPU-profile guidance on a host whose CPU no built-in profile serves, as
/// `run_guides_failed_cold_boots_on_cpus_that_no_built_in_profile_serves` checks.
fn fails_a_cold_boot(case: &Launch) -> bool {
    case.arguments
        .first()
        .is_some_and(|command| command == "run")
        && case.status != 0
        && !case
            .arguments
            .iter()
            .any(|argument| argument == "--restore-snapshot")
}

/// Returns `nvx-rs run OPTIONS`.
fn run(options: &[&str]) -> Vec<String> {
    ["run"]
        .iter()
        .chain(options)
        .map(|argument| (*argument).to_owned())
        .collect()
}

/// Returns the one-shot runs of `nvx-rs sandbox run`.
fn sandbox_launches() -> Vec<Launch> {
    let echo = || workload("", "/bin/echo", &["hello", "world"]);
    let mut launches = vec![
        launch(
            "echo",
            sandbox_run("/bin/echo", &["--arg", "hello", "--arg", "world"]),
            echo(),
            0,
            "hello world\n",
        ),
        // The operation is `run` by default.
        launch(
            "default operation",
            [
                "sandbox",
                "--layer",
                LAYER,
                "--scratch",
                SCRATCH,
                "--entrypoint",
                "/bin/echo",
                "--arg",
                "hello",
                "--arg",
                "world",
            ],
            echo(),
            0,
            "hello world\n",
        ),
        launch(
            "failure",
            sandbox_run("/bin/false", &[]),
            workload("", "/bin/false", &[]),
            1,
            "",
        ),
        // An upper in RAM attaches the distro layer alone, and needs no scratch image.
        launch(
            "RAM upper",
            [
                "sandbox",
                "run",
                "--layer",
                LAYER,
                "--upper",
                "ram",
                "--entrypoint",
                "/bin/echo",
                "--arg",
                "hello",
                "--arg",
                "world",
            ],
            echo().in_ram(),
            0,
            "hello world\n",
        )
        .without("scratch.ext4"),
        launch(
            "missing program",
            sandbox_run("/bin/missing", &[]),
            workload("", "/bin/missing", &[]),
            127,
            "",
        )
        .with_stderr("openvmm: /bin/missing: not found\n"),
        launch(
            "outcome report",
            sandbox_run(
                "/bin/echo",
                &[
                    "--arg",
                    "hello",
                    "--arg",
                    "world",
                    "--outcome-report",
                    "{root/reports/openvmm-run.json}",
                ],
            ),
            echo().with("--microvm-report", ["{root/reports/openvmm-run.json}"]),
            0,
            "hello world\n",
        )
        .recorded_in(Some("build/fake-openvmm-reports.json"))
        .reporting(Report {
            path: "reports/openvmm-run.json",
            category: "success",
            status: 0,
            network: NO_NETWORK,
        }),
        launch(
            "network outcome report",
            sandbox_run(
                "/bin/true",
                &[
                    "--net",
                    "10.0.0.2/24",
                    "--network-profile",
                    "portable",
                    "--network-egress",
                    "deny",
                    "--network-egress-allow",
                    "10.0.0.0/8:tcp:443",
                    "--outcome-report",
                    "{root/reports/openvmm-run.json}",
                ],
            ),
            workload("", "/bin/true", &[])
                .with("--net", ["10.0.0.2/24"])
                .with("--network-profile", ["portable"])
                .with("--network-egress", ["deny"])
                .with("--network-ingress", ["deny"])
                .with("--network-egress-allow", ["10.0.0.0/8:tcp:443"])
                .with("--microvm-report", ["{root/reports/openvmm-run.json}"]),
            0,
            "",
        )
        .recorded_in(Some("build/fake-openvmm-reports.json"))
        .reporting(Report {
            path: "reports/openvmm-run.json",
            category: "success",
            status: 0,
            network: ("applied", "rules", 1, 0, "allow"),
        }),
    ];
    // OpenVMM exits with the status that its workload exits with, which the fake OpenVMM's
    // `fake_exit_status=STATUS` sets.
    for (name, status, command_line) in [
        ("exit status", 7, "fake_exit_status=7"),
        ("exit status 3", 3, "fake_exit_status=3"),
        ("timeout status", 124, "fake_exit_status=124"),
        ("killed status", 137, "fake_exit_status=137"),
    ] {
        launches.push(launch(
            name,
            sandbox_run("/bin/true", &["--cmdline", command_line]),
            workload(command_line, "/bin/true", &[]),
            status,
            "",
        ));
    }
    launches
}

/// Returns the runs of `nvx-rs run`.
fn run_launches() -> Vec<Launch> {
    let booted = "openvmm: booted the microVM\n";
    let restored = "openvmm: restored the microVM\n";
    vec![
        launch("run: boot", run(&[]), cold_boot(), 0, booted),
        launch(
            "run: exit status",
            run(&["--cmdline", "fake_exit_status=7"]),
            cold_boot().with("--cmdline", ["fake_exit_status=7"]),
            7,
            booted,
        ),
        launch(
            "run: failed boot",
            run(&["--cmdline", "quiet fake_exit_status=1"]),
            cold_boot().with("--cmdline", ["quiet fake_exit_status=1"]),
            1,
            booted,
        ),
        launch(
            "run: Ubuntu",
            run(&["--guest", "ubuntu", "--memory-capacity-mib", "1024"]),
            cold_boot()
                .with("--memory", ["512M"])
                .with("--initrd", ["{root/build/initramfs-ubuntu.cpio.gz}"])
                .with("--memory-capacity", ["1024M"]),
            0,
            booted,
        )
        .with_file(("build/initramfs-ubuntu.cpio.gz", Content::Text("ubuntu\n"))),
        launch(
            "run: every boot option",
            run(&[
                "--hypervisor",
                "mshv",
                "--processors",
                "8",
                "--cpu-profile",
                "host",
                "--memory-mib",
                "256",
                "--memory-capacity-mib",
                "512",
            ]),
            cold_boot()
                .with("--processors", ["8"])
                .with("--hypervisor", ["mshv"])
                .with("--memory", ["256M"])
                .with("--memory-capacity", ["512M"])
                .with("--cpu-profile", ["host"]),
            0,
            booted,
        ),
        // A restore records its command line next to its snapshot, since it has no kernel.
        launch(
            "run: restore",
            run(&[
                "--processors",
                "4",
                "--restore-snapshot",
                "{root/snapshot}",
                "--restore-processors",
                "2",
                "--restore-memory-mib",
                "1024",
                "--restore-ready-path",
                "{root/ready.sock}",
                "--cpu-profile",
                "auto",
            ]),
            restore()
                .with("--processors", ["4"])
                .with("--restore-processors", ["2"])
                .with("--restore-memory", ["1024M"])
                .with("--restore-ready-path", ["{root/ready.sock}"])
                .with("--cpu-profile", ["auto"]),
            0,
            restored,
        )
        .with_file(SNAPSHOT)
        .recorded_in(Some("fake-openvmm-.json")),
        launch(
            "run: restore without guest artifacts",
            run(&["--restore-snapshot", "{root/snapshot}"]),
            restore(),
            0,
            restored,
        )
        .with_file(SNAPSHOT)
        .without("build/vmlinux")
        .without("build/initramfs.cpio.gz")
        .recorded_in(Some("fake-openvmm-.json")),
        launch(
            "run: missing snapshot",
            run(&["--restore-snapshot", "{root/missing}", "--cmdline", "quiet"]),
            restore()
                .with("--restore-snapshot", ["{root/missing}"])
                .with("--cmdline", ["quiet"]),
            1,
            "",
        )
        .with_stderr("openvmm: cannot restore {root/missing}: no such snapshot directory\n")
        .recorded_in(Some("fake-openvmm-.json")),
        launch(
            "run: outcome report",
            run(&["--outcome-report", "{root/reports/openvmm-run.json}"]),
            cold_boot().with("--microvm-report", ["{root/reports/openvmm-run.json}"]),
            0,
            booted,
        )
        .recorded_in(Some("build/fake-openvmm-reports.json"))
        .reporting(Report {
            path: "reports/openvmm-run.json",
            category: "success",
            status: 0,
            network: NO_NETWORK,
        }),
        launch(
            "run: restore outcome report",
            run(&[
                "--restore-snapshot",
                "{root/snapshot}",
                "--outcome-report",
                "{root/reports/openvmm-run.json}",
                "--cmdline",
                "fake_exit_status=4",
            ]),
            restore()
                .with("--microvm-report", ["{root/reports/openvmm-run.json}"])
                .with("--cmdline", ["fake_exit_status=4"]),
            4,
            restored,
        )
        .with_file(SNAPSHOT)
        .recorded_in(Some("fake-openvmm-reports.json"))
        .reporting(Report {
            path: "reports/openvmm-run.json",
            category: "guest-exit",
            status: 4,
            network: NO_NETWORK,
        }),
        // A restore passes the network profile and policy of a snapshot that has a NIC without
        // `--net`, which OpenVMM refuses on a restore.
        launch(
            "run: restore of a networked snapshot",
            run(&[
                "--restore-snapshot",
                "{root/snapshot}",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-egress-allow",
                "10.0.0.0/8:tcp:443",
                "--host-loopback",
                "deny",
                "--outcome-report",
                "{root/reports/openvmm-run.json}",
            ]),
            restore()
                .with("--network-profile", ["portable"])
                .with("--network-egress", ["deny"])
                .with("--network-egress-allow", ["10.0.0.0/8:tcp:443"])
                .with("--host-loopback", ["deny"])
                .with("--microvm-report", ["{root/reports/openvmm-run.json}"]),
            0,
            restored,
        )
        .with_file(SNAPSHOT)
        .recorded_in(Some("fake-openvmm-reports.json"))
        .reporting(Report {
            path: "reports/openvmm-run.json",
            category: "success",
            status: 0,
            network: ("applied", "rules", 1, 0, "deny"),
        }),
        launch(
            "run: network outcome report",
            run(&[
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-egress-allow",
                "10.0.0.0/8:tcp:443",
                "--host-loopback",
                "deny",
                "--outcome-report",
                "{root/reports/openvmm-run.json}",
            ]),
            cold_boot()
                .with("--net", ["10.0.0.2/24"])
                .with("--network-profile", ["portable"])
                .with("--network-egress", ["deny"])
                .with("--network-egress-allow", ["10.0.0.0/8:tcp:443"])
                .with("--host-loopback", ["deny"])
                .with("--microvm-report", ["{root/reports/openvmm-run.json}"]),
            0,
            booted,
        )
        .recorded_in(Some("build/fake-openvmm-reports.json"))
        .reporting(Report {
            path: "reports/openvmm-run.json",
            category: "success",
            status: 0,
            network: ("applied", "rules", 1, 0, "deny"),
        }),
        // OpenVMM resolves the relative policy paths of a run in its only share.
        launch(
            "run: one share",
            run(&[
                "--mount",
                "/data,{root/share},rw",
                "--mount-deny",
                "hidden",
                "--mount-allow",
                "hidden/shown",
                "--mount-write",
                "writable",
            ]),
            cold_boot()
                .with("--mount", ["/data,{root/share},rw"])
                .with("--mount-deny", ["hidden"])
                .with("--mount-allow", [native("hidden/shown")])
                .with("--mount-write", ["writable"]),
            0,
            booted,
        ),
        launch(
            "run: several shares and a file",
            run(&[
                "--mount",
                "/data,{root/share}",
                "--mount",
                "/single.txt,{root/single.txt}",
                "--mount-deny",
                "{root/share/hidden}",
                "--cmdline",
                "fake_exit_status=3",
            ]),
            cold_boot()
                .with("--mount-aggregate", ["/run/nvx/shares"])
                .with(
                    "--mount-child",
                    [
                        format!("{},{{root/share}},ro", child(0, "/data")),
                        format!("{},{{root/single.txt}},ro,file", child(1, "/single.txt")),
                    ],
                )
                .with("--mount-deny", ["{root/share/hidden}"])
                .kernel(&[
                    "fake_exit_status=3".to_owned(),
                    share_token(0, "/data", "ro", false),
                    share_token(1, "/single.txt", "ro", true),
                ]),
            3,
            booted,
        ),
        // OpenVMM refuses its command line, which it records only once it accepts it, with the
        // status of a usage error, which the run exits with.
        launch(
            "run: OpenVMM refuses its options",
            run(&["--host-loopback-forward", "tcp:8080:80"]),
            cold_boot().with("--host-loopback-forward", ["tcp:8080:80"]),
            2,
            "",
        )
        .with_stderr("openvmm: --host-loopback-forward requires explicit --host-loopback allow\n")
        .recorded_in(None),
        // A relative NVX_OPENVMM is relative to the working directory, and the command names it
        // by its absolute path.
        launch(
            "run: relative OpenVMM override",
            run(&[]),
            cold_boot(),
            0,
            booted,
        )
        .with_variable("NVX_OPENVMM", native(OPENVMM)),
    ]
}

#[test]
fn one_shot_runs_exit_with_the_workload_or_openvmm_status() {
    let cases: Vec<Launch> = sandbox_launches()
        .into_iter()
        .chain(run_launches())
        .collect();
    let (vendor, family, model, stepping) = crate::reported_cpu();
    let unserved = !crate::served(&vendor, family, model, stepping);
    in_parallel(&cases, |case| {
        let root = Root::new();
        for (relative, content) in &case.files {
            root.write_content(relative, *content);
        }
        for relative in &case.removed {
            root.remove(relative);
        }
        let arguments: Vec<&str> = case.arguments.iter().map(String::as_str).collect();
        let environment: Vec<(&str, &str)> = case
            .environment
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let outcome = root.nvx_rs_with(&arguments, &environment);
        eprintln!("case: {}", case.name);
        assert_eq!(outcome.status, Some(case.status), "{}", outcome.describe());
        let (command, rest) = outcome.command();
        // The workload's output follows the command byte for byte, as does OpenVMM's.
        assert_eq!(rest, case.stdout.as_bytes(), "{}", outcome.describe());
        let stderr = outcome.stderr();
        let guidance = stderr
            .strip_prefix(&root.expand(case.stderr))
            .unwrap_or_else(|| panic!("{}", outcome.describe()));
        if unserved && fails_a_cold_boot(case) {
            assert!(
                guidance.starts_with("nvx: no built-in CPU profile serves this host's CPU"),
                "{}",
                outcome.describe()
            );
        } else {
            assert_eq!(guidance, "", "{}", outcome.describe());
        }
        let (program, arguments) = command.split_first().unwrap();
        assert_eq!(
            *program,
            root.below(OPENVMM).to_str().unwrap(),
            "{}",
            case.name
        );
        // The `>> ` line splits back into the arguments that OpenVMM received.
        if let Some(record) = case.record {
            assert_eq!(root.openvmm_arguments(record), arguments, "{}", case.name);
        }
        let mut options = Options::parse(arguments);
        let mut expected = case.options.clone();
        // A sandbox's OpenVMM opens the images through the files that the run checked: the distro
        // layer alone for an upper in RAM, and the scratch image too for one on scratch.
        let blocks = expected.take("--microvm-sandbox-block");
        if !blocks.is_empty() {
            let checked = options.take("--microvm-sandbox-block");
            let token = case
                .record
                .and_then(|record| record.strip_prefix("build/fake-openvmm-"))
                .and_then(|name| name.strip_suffix(".json"))
                .unwrap();
            if blocks.len() == 1 {
                assert_checked_distro(&root, &checked);
                assert_opened_distro(&root, token);
            } else {
                assert_checked_blocks(&root, &checked);
                assert_opened_images(&root, token);
            }
        }
        expected.assert_matches(&root, &options, case.name);
        if let Some(report) = &case.report {
            assert_outcome_report(
                &root.json(report.path),
                options.value("--hypervisor"),
                ("run", report.category, report.status),
                report.network,
            );
        }
    });
}
