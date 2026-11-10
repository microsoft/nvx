//! The runs that launch OpenVMM on the terminal: one-shot sandboxes of `nvx-rs sandbox run`, and
//! the microVMs of `nvx-rs run`. Each prints the OpenVMM command as `>> COMMAND`, runs it with the
//! standard streams, which carry the workload's output, or OpenVMM's, byte for byte, and exits
//! with OpenVMM's status, which a one-shot sandbox's OpenVMM takes from its workload.

use crate::harness::{Content, LAYER, OPENVMM, Root, SCRATCH, in_parallel, native};
use crate::openvmm::{
    DISCONNECTED_CONSOLE, NO_CONSOLE, NO_NETWORK, NetworkPolicy, Options, TERMINAL_CONSOLE,
    WORKLOAD, assert_checked_blocks, assert_checked_distro, assert_opened_distro,
    assert_opened_images, assert_outcome_report, child, cold_boot, one_shot, restore, share_token,
    snapshot,
};

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
    /// A specific checked-configuration refusal, with no OpenVMM launch.
    refusal: Option<&'static str>,
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
        refusal: None,
    }
}

impl Launch {
    fn refusing(mut self, message: &'static str) -> Self {
        self.refusal = Some(message);
        self.status = 1;
        self
    }

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
        .with_file(snapshot(&NO_CONSOLE))
        .recorded_in(Some("fake-openvmm-.json")),
        launch(
            "run: restore without guest artifacts",
            run(&["--restore-snapshot", "{root/snapshot}"]),
            restore(),
            0,
            restored,
        )
        .with_file(snapshot(&NO_CONSOLE))
        .without("build/vmlinux")
        .without("build/initramfs.cpio.gz")
        .recorded_in(Some("fake-openvmm-.json")),
        // A restore attaches the boot console to the terminal again where its capture did, as
        // OpenVMM requires, and leaves any other boot console as its snapshot records it.
        launch(
            "run: restore of a boot console on the terminal",
            run(&["--restore-snapshot", "{root/snapshot}"]),
            restore().with("--virtio-console", ["console"]),
            0,
            restored,
        )
        .with_file(snapshot(&TERMINAL_CONSOLE))
        .recorded_in(Some("fake-openvmm-.json")),
        launch(
            "run: restore of a disconnected boot console",
            run(&["--restore-snapshot", "{root/snapshot}"]),
            restore(),
            0,
            restored,
        )
        .with_file(snapshot(&DISCONNECTED_CONSOLE))
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
        .with_file(snapshot(&NO_CONSOLE))
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
        .with_file(snapshot(&NO_CONSOLE))
        .recorded_in(Some("fake-openvmm-reports.json"))
        .reporting(Report {
            path: "reports/openvmm-run.json",
            category: "success",
            status: 0,
            network: ("applied", "rules", 1, 0, "deny"),
        }),
        // A restore that replaces the policy of a NIC template may publish the guest's listeners
        // and forward host ports, which OpenVMM refuses at every other restore.
        launch(
            "run: restore that replaces the network policy",
            run(&[
                "--restore-snapshot",
                "{root/snapshot}",
                "--restore-network-policy",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-egress-allow",
                "10.0.0.1/32:udp:53",
                "--network-ingress",
                "allow",
                "--host-loopback",
                "allow",
                "--host-loopback-forward",
                "tcp:8080:80",
                "--outcome-report",
                "{root/reports/openvmm-run.json}",
            ]),
            restore()
                .flag("--restore-network-policy")
                .with("--network-profile", ["portable"])
                .with("--network-egress", ["deny"])
                .with("--network-egress-allow", ["10.0.0.1/32:udp:53"])
                .with("--network-ingress", ["allow"])
                .with("--host-loopback", ["allow"])
                .with("--host-loopback-forward", ["tcp:8080:80"])
                .with("--microvm-report", ["{root/reports/openvmm-run.json}"]),
            0,
            restored,
        )
        .with_file(snapshot(&NO_CONSOLE))
        .recorded_in(Some("fake-openvmm-reports.json"))
        .reporting(Report {
            path: "reports/openvmm-run.json",
            category: "success",
            status: 0,
            network: ("applied", "rules", 1, 0, "allow"),
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
        .recorded_in(None)
        .refusing("network policy options require --net"),
        launch(
            "run: OpenVMM exits with a usage status",
            run(&["--cmdline", "fake_exit_on_start=2"]),
            cold_boot().with("--cmdline", ["fake_exit_on_start=2"]),
            2,
            "",
        )
        .with_stderr("openvmm: failing the launch as requested\n"),
        // A relative NVX_OPENVMM is relative to the working directory, and the command names it
        // by its absolute path.
        launch(
            "run: relative OpenVMM override",
            run(&[]),
            cold_boot(),
            0,
            booted,
        )
        .with_variable("NVX_OPENVMM", native(OPENVMM))
        .without("build/openvmm.provenance.json")
        .without("build/vmlinux.provenance.json")
        .without("build/initramfs.provenance.json"),
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
        let cold = case
            .arguments
            .first()
            .is_some_and(|command| command == "run")
            && !case
                .arguments
                .iter()
                .any(|argument| argument == "--restore-snapshot");
        let before = case.refusal.map(|_| root.snapshot());
        let outcome = root.nvx_rs_with(&arguments, &environment);
        eprintln!("case: {}", case.name);
        if let Some(message) = case.refusal {
            assert_eq!(outcome.status, Some(case.status), "{}", outcome.describe());
            assert!(outcome.stdout.is_empty(), "{}", outcome.describe());
            assert!(
                outcome.stderr().starts_with("error[unsupported]:"),
                "{}",
                outcome.describe()
            );
            assert!(outcome.stderr().contains(message), "{}", outcome.describe());
            assert_eq!(Some(root.snapshot()), before);
            return;
        }
        let expected_cold = cold.then(|| case.options.cold_run_expected(&root, "one-shot"));
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
        let mut options = Options::parse(arguments);
        let record = case.record.map(|record| {
            if cold {
                let report = std::path::Path::new(options.value("--microvm-report"));
                let token = report
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap();
                format!("build/fake-openvmm-{token}.json")
            } else {
                record.to_owned()
            }
        });
        if let Some(record) = record {
            assert_eq!(root.openvmm_arguments(&record), arguments, "{}", case.name);
        }
        let mut expected = expected_cold
            .map(|expected| expected.unwrap())
            .unwrap_or_else(|| case.options.clone());
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
        if cold {
            options = options.normalized_cold_outputs(true);
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

#[test]
fn both_cold_modes_keep_their_foreground_output_and_owned_exit_status() {
    for (mode, caller, status, stdout) in [
        (
            "one-shot",
            "fake_exit_status=7",
            7,
            "openvmm: booted the microVM\n",
        ),
        ("managed", "fake_crash_after_ms=100", 3, ""),
    ] {
        let root = Root::new();
        let identity = if mode == "managed" {
            " nvx_identity=1001:1001"
        } else {
            ""
        };
        let outcome = root.nvx_rs(&[
            "run",
            "--cmdline",
            &format!("{caller} nvx_mode={mode}{identity}"),
        ]);
        assert_eq!(outcome.status, Some(status), "{}", outcome.describe());
        let (command, output) = outcome.command();
        assert_eq!(output, stdout.as_bytes(), "{}", outcome.describe());
        assert_eq!(command[0], root.below(OPENVMM).to_str().unwrap());
        let mut expected = cold_boot()
            .with(
                "--cmdline",
                [format!(
                    "{caller}{}",
                    if identity.is_empty() { " " } else { "  " }
                )],
            )
            .cold_run_expected(&root, mode)
            .unwrap();
        if !identity.is_empty() {
            let line = format!("{}{identity}", expected.value("--cmdline"));
            expected = expected.with("--cmdline", [line]);
        }
        let actual = Options::parse(&command[1..]).normalized_cold_outputs(true);
        expected.assert_matches(&root, &actual, mode);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cold_terminal_runs_forward_raw_output_and_restore_the_callers_terminal() {
    use std::fs::File;
    use std::io::{self, Read};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use nix::pty::openpty;
    use nix::sys::termios::{LocalFlags, OutputFlags, tcgetattr};

    for status in [0, 7] {
        let root = Root::new();
        let terminal = openpty(None, None).unwrap();
        let original = tcgetattr(&terminal.slave).unwrap();
        let mut process = root
            .command(&[
                "run",
                "--cmdline",
                &format!("fake_boot_delay_ms=250 fake_exit_status={status}"),
            ])
            .stdin(Stdio::from(terminal.slave.try_clone().unwrap()))
            .stdout(Stdio::from(terminal.slave.try_clone().unwrap()))
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut observed_raw = false;
        while process.try_wait().unwrap().is_none() {
            let current = tcgetattr(&terminal.slave).unwrap();
            observed_raw |= !current
                .local_flags
                .intersects(LocalFlags::ICANON | LocalFlags::ECHO)
                && !current.output_flags.contains(OutputFlags::OPOST);
            if Instant::now() >= deadline {
                process.kill().unwrap();
                process.wait().unwrap();
                panic!("terminal run did not exit within its deadline");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let outcome = process.wait_with_output().unwrap();
        assert_eq!(
            outcome.status.code(),
            Some(status),
            "{}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        assert!(observed_raw, "cold run left its terminal cooked");
        let restored = tcgetattr(&terminal.slave).unwrap();
        assert_eq!(restored.input_flags, original.input_flags);
        assert_eq!(restored.output_flags, original.output_flags);
        assert_eq!(restored.local_flags, original.local_flags);
        assert_eq!(restored.control_flags, original.control_flags);
        assert_eq!(restored.control_chars, original.control_chars);
        fcntl(&terminal.master, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();
        let mut master = File::from(terminal.master);
        let mut output = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            match master.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("cannot read terminal output: {error}"),
            }
        }
        assert!(
            output.ends_with(b"openvmm: booted the microVM\n"),
            "{output:?}"
        );
    }
}

#[test]
fn cold_runs_refuse_existing_and_missing_parent_reports_before_launch() {
    let root = Root::new();
    root.write("reports/existing.json", "keep\n");
    for (path, message) in [
        (
            "{root/reports/existing.json}",
            "error[outcome-report-exists]:",
        ),
        (
            "{root/missing/outcome.json}",
            "outcome report parent is not a plain directory",
        ),
    ] {
        let before = root.snapshot();
        let outcome = root.nvx_rs(&["run", "--outcome-report", path]);
        assert_eq!(outcome.status, Some(1), "{}", outcome.describe());
        assert!(outcome.stdout.is_empty(), "{}", outcome.describe());
        assert!(outcome.stderr().contains(message), "{}", outcome.describe());
        assert_eq!(root.snapshot(), before);
    }
}

#[test]
fn missing_cold_reports_do_not_replace_the_owned_child_status() {
    for line in [
        "fake_exit_on_start=3",
        "fake_crash_after_ms=100 nvx_mode=managed nvx_identity=1001:1001",
    ] {
        let root = Root::new();
        let outcome = root.nvx_rs(&[
            "run",
            "--cmdline",
            line,
            "--outcome-report",
            "{root/reports/outcome.json}",
        ]);
        assert_eq!(outcome.status, Some(3), "{}", outcome.describe());
        assert!(
            outcome.stderr().contains("outcome.json"),
            "{}",
            outcome.describe()
        );
        assert!(!root.below("reports/outcome.json").exists());
    }
}

#[test]
fn cold_report_publication_never_clobbers_a_destination_created_during_boot() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    for symlink in [false, true]
        .into_iter()
        .filter(|symlink| !symlink || cfg!(unix))
    {
        let root = Root::new();
        let destination = root.below("reports/outcome.json");
        root.write("reports/victim.txt", "keep\n");
        let mut process = root
            .command(&[
                "run",
                "--cmdline",
                "fake_boot_delay_ms=250 fake_exit_status=7",
                "--outcome-report",
                destination.to_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(process.stdout.take().unwrap());
        let mut command = String::new();
        stdout.read_line(&mut command).unwrap();
        assert!(command.starts_with(">> "), "{command}");
        if symlink {
            #[cfg(unix)]
            std::os::unix::fs::symlink(root.below("reports/victim.txt"), &destination).unwrap();
        } else {
            root.write("reports/outcome.json", "keep\n");
        }
        let output = process.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(7), "{output:?}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("outcome report already exists"),
            "the publication failure must be reported"
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"keep\n");
        assert_eq!(
            std::fs::read(root.below("reports/victim.txt")).unwrap(),
            b"keep\n"
        );
        if symlink {
            assert!(
                std::fs::symlink_metadata(&destination)
                    .unwrap()
                    .is_symlink()
            );
        }
    }
}

#[test]
fn cold_console_backpressure_does_not_block_output_or_replace_the_child_status() {
    use std::io::{self, Write};
    use std::process::Stdio;

    let root = Root::new();
    let arguments = [
        "run",
        "--cmdline",
        "fake_boot_delay_ms=2500 fake_exit_status=7",
    ];
    let mut process = root
        .command(&arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = process.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&vec![b'x'; 16 * 1024 * 1024]));
    let output = process.wait_with_output().unwrap();
    if let Err(error) = writer.join().unwrap() {
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe, "{error}");
    }
    let outcome = crate::harness::Outcome::new(
        arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        output,
    );
    assert_eq!(outcome.status, Some(7), "{}", outcome.describe());
    assert_eq!(
        outcome.command().1,
        b"openvmm: booted the microVM\n",
        "{}",
        outcome.describe()
    );
}

#[test]
fn cold_console_output_preserves_non_utf8_bytes() {
    let root = Root::new();
    let outcome = root.nvx_rs(&[
        "run",
        "--cmdline",
        "fake_binary_console=1 fake_exit_status=7",
    ]);
    assert_eq!(outcome.status, Some(7), "{}", outcome.describe());
    assert_eq!(
        outcome.command().1,
        &[b'A', 0xff, 0x80, b'\n'],
        "{}",
        outcome.describe()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cold_runs_keep_the_native_child_in_the_callers_session_and_process_group() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use nix::unistd::{Pid, getpgid, getsid};

    let root = Root::new();
    let process = root
        .command(&["run", "--cmdline", "fake_boot_delay_ms=500"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let caller = Pid::from_raw(process.id() as i32);
    let deadline = Instant::now() + Duration::from_secs(10);
    let child = loop {
        let children = std::fs::read_to_string(format!(
            "/proc/{}/task/{}/children",
            caller.as_raw(),
            caller.as_raw()
        ))
        .unwrap();
        let native = children.split_ascii_whitespace().find_map(|pid| {
            let arguments = match std::fs::read(format!("/proc/{pid}/cmdline")) {
                Ok(arguments) => arguments,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
                Err(error) => panic!("cannot inspect the owned child {pid}: {error}"),
            };
            arguments
                .split(|byte| *byte == 0)
                .any(|argument| argument == b"--microvm-control-console")
                .then(|| Pid::from_raw(pid.parse().unwrap()))
        });
        if let Some(child) = native {
            break child;
        }
        assert!(
            Instant::now() < deadline,
            "the owned native child did not appear"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let caller_session = getsid(Some(caller)).unwrap();
    let caller_group = getpgid(Some(caller)).unwrap();
    let child_session = getsid(Some(child)).unwrap();
    let child_group = getpgid(Some(child)).unwrap();
    let output = process.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(child_session, caller_session);
    assert_eq!(child_group, caller_group);
}

/// A run leaves an outcome report that the caller can read whatever its umask, even one that
/// clears the owner's own permissions, and whose group's and others' permissions the caller's
/// umask narrows: OpenVMM starts with the caller's umask without the owner's bits.
#[cfg(target_os = "linux")]
#[test]
fn runs_leave_an_outcome_report_that_the_caller_reads_whatever_its_umask() {
    use crate::harness::{HYPERVISOR, json, mode};
    use crate::openvmm::FAKE_REPORT_MODE;

    for umask in [0o022, 0o400, 0o200, 0o777] {
        let root = Root::new();
        let outcome = root.nvx_rs_with_umask(
            &run(&["--outcome-report", "{root/reports/openvmm-run.json}"]),
            umask,
        );
        assert_eq!(outcome.status, Some(0), "{}", outcome.describe());
        let report = root.below("reports/openvmm-run.json");
        let (permissions, expected) = (mode(&report), FAKE_REPORT_MODE & !(umask & 0o077));
        assert_eq!(
            permissions, expected,
            "umask {umask:03o}: the report has mode {permissions:03o}, not {expected:03o}"
        );
        assert_outcome_report(
            &json(&report),
            HYPERVISOR,
            ("run", "success", 0),
            NO_NETWORK,
        );
    }
}
