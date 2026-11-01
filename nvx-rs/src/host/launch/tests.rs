use proptest::prelude::*;

use super::*;
use crate::ErrorKind;
use crate::host::filesystem::{Bind, ExportChild, Owner};
use crate::protocol::cmdline::tests::model;

fn spec() -> LaunchSpec {
    LaunchSpec::new(
        Artifacts::new("/o", "/k/vmlinux", "/k/initrd"),
        Hypervisor::Kvm,
    )
}

#[test]
fn kernel_command_line_sets_the_hostname() {
    let mut spec = spec();
    spec.kernel_parameters = "quiet loglevel=0".to_owned();
    assert_eq!(
        spec.kernel_command_line().unwrap(),
        "quiet loglevel=0 hostname=nvx-sandbox"
    );
    spec.kernel_parameters = String::new();
    assert_eq!(spec.kernel_command_line().unwrap(), "hostname=nvx-sandbox");
    spec.kernel_parameters = "x".repeat(COMMAND_LINE_BUDGET);
    assert_eq!(
        spec.kernel_command_line().unwrap_err().kind(),
        ErrorKind::Unsupported
    );
}

#[test]
fn arguments_select_the_managed_direct_guest() {
    let arguments = spec()
        .openvmm_arguments("/state/t/control.sock", Path::new("/state/t/outcome.json"))
        .unwrap();
    let arguments: Vec<&str> = arguments.iter().map(|a| a.to_str().unwrap()).collect();
    assert_eq!(
        arguments,
        [
            "--machine",
            "microvm",
            "--microvm-workload-identity",
            "65534:65534",
            "--microvm-lifecycle",
            "managed",
            "--single-process",
            "--hypervisor",
            "kvm",
            "--memory",
            "256M",
            "--kernel",
            "/k/vmlinux",
            "--initrd",
            "/k/initrd",
            "--cmdline",
            "hostname=nvx-sandbox",
            "--virtio-console",
            "none",
            "--microvm-control-console",
            "listen=/state/t/control.sock",
            "--microvm-control-auth-stdin",
            "--microvm-report",
            "/state/t/outcome.json",
        ]
    );
    assert!(!arguments.contains(&"--microvm-sandbox-block"));
}

#[test]
fn mappings_add_the_export_and_the_mapping_count() {
    let mut spec = spec();
    spec.filesystem = Some(ExportPlan {
        children: vec![ExportChild {
            root: "/host/work".into(),
            file: false,
            writable: false,
            denied: vec!["/host/work/secret".into()],
            denied_identities: vec![None],
            allowed: Vec::new(),
            write: Vec::new(),
        }],
        binds: vec![Bind {
            child: 0,
            source: String::new(),
            target: "/host/work".to_owned(),
            read_only: true,
            identity: None,
        }],
        owner: Owner::Vmm,
    });
    assert_eq!(
        spec.kernel_command_line().unwrap(),
        "hostname=nvx-sandbox nvx_maps=1"
    );
    spec.identity.create_account = true;
    assert_eq!(
        spec.kernel_command_line().unwrap(),
        "hostname=nvx-sandbox nvx_workload_account=create nvx_maps=1"
    );
    spec.identity.create_account = false;
    let arguments = spec.openvmm_arguments("/e", Path::new("/r")).unwrap();
    let arguments: Vec<&str> = arguments.iter().map(|a| a.to_str().unwrap()).collect();
    assert_eq!(
        arguments[arguments.len() - 6..],
        [
            "--mount-aggregate",
            "/run/nvx/hostfs/root",
            "--mount-child",
            "0,/host/work,ro",
            "--mount-deny",
            "/host/work/secret",
        ]
    );
}

#[test]
fn process_command_lines_are_measured_as_windows_quotes_them() {
    let cases: [(&str, &[&str], &str); 5] = [
        (
            r"C:\Program Files\openvmm.exe",
            &["--a", "b c", ""],
            r#""C:\Program Files\openvmm.exe" --a "b c" """#,
        ),
        ("x", &[r#"say "hi""#], r#""x" "say \"hi\"""#),
        ("x", &[r#"a"b"#], r#""x" a\"b"#),
        ("x", &[r"dir\ with\"], r#""x" "dir\ with\\""#),
        ("x", &[r"a\\b", "é"], r#""x" a\\b é"#),
    ];
    for (program, arguments, line) in cases {
        let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
        assert_eq!(
            process_command_line_units(OsStr::new(program), &arguments),
            line.encode_utf16().count() + 1,
            "{line}"
        );
    }
}

#[test]
fn oversized_process_command_lines_are_rejected() {
    let mut spec = spec();
    let report = Path::new("/state/t/outcome.json");
    spec.check_process_command_line("/state/t/control.sock", report)
        .unwrap();
    let denied: Vec<PathBuf> = (0..128)
        .map(|index| format!("/host/work/{index:03}-{}", "d".repeat(240)).into())
        .collect();
    spec.filesystem = Some(ExportPlan {
        children: vec![ExportChild {
            root: "/host/work".into(),
            file: false,
            writable: false,
            denied_identities: vec![None; denied.len()],
            denied,
            allowed: Vec::new(),
            write: Vec::new(),
        }],
        binds: Vec::new(),
        owner: Owner::Vmm,
    });
    let error = spec
        .check_process_command_line("/state/t/control.sock", report)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
}

#[test]
fn launches_may_use_the_reserve_that_provisioning_keeps() {
    let endpoint = "/state/t/control.sock";
    let report = Path::new("/state/t/outcome.json");
    let mut spec = spec();
    let arguments = spec.openvmm_arguments(endpoint, report).unwrap();
    let base = process_command_line_units(spec.openvmm.as_os_str(), &arguments);
    // Lengthening the program's path lengthens the command line alike.
    let program = |extra: usize| PathBuf::from(format!("/o{}", "o".repeat(extra)));
    spec.openvmm = program(PROCESS_COMMAND_LINE_LIMIT - base);
    assert_eq!(
        process_command_line_units(spec.openvmm.as_os_str(), &arguments),
        PROCESS_COMMAND_LINE_LIMIT
    );
    spec.fit_process_command_line(&arguments, 0).unwrap();
    let reserved = spec
        .check_process_command_line(endpoint, report)
        .unwrap_err();
    assert!(
        reserved
            .to_string()
            .contains("allows, with 1024 held in reserve; map"),
        "{reserved}"
    );
    spec.openvmm = program(PROCESS_COMMAND_LINE_LIMIT - base + 1);
    let error = spec.fit_process_command_line(&arguments, 0).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(
        error
            .to_string()
            .contains("a Windows process command line allows; map"),
        "{error}"
    );
}

#[test]
fn a_process_command_line_fits_while_it_leaves_its_reserve() {
    // Any program and arguments, such as an adapter's: a command line of the limit fits,
    // one more code unit does not, and a reserve takes its part of the limit.
    let arguments: Vec<OsString> = ["--a", "b c"].map(OsString::from).into();
    let base = process_command_line_units(OsStr::new("/o"), &arguments);
    let program = |extra: usize| OsString::from(format!("/o{}", "o".repeat(extra)));
    let fitting = program(PROCESS_COMMAND_LINE_LIMIT - base);
    assert_eq!(
        process_command_line_units(&fitting, &arguments),
        PROCESS_COMMAND_LINE_LIMIT
    );
    fit_process_command_line(&fitting, &arguments, 0).unwrap();
    let held = fit_process_command_line(&fitting, &arguments, 1).unwrap_err();
    assert_eq!(held.kind(), ErrorKind::Unsupported);
    assert!(
        held.to_string()
            .contains("would take 32767 of the 32767 characters"),
        "{held}"
    );
    assert!(
        held.to_string().contains("with 1 held in reserve"),
        "{held}"
    );
    let longer = program(PROCESS_COMMAND_LINE_LIMIT - base + 1);
    let error = fit_process_command_line(&longer, &arguments, 0).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    // A reserve past the limit leaves no room for any command line.
    assert!(fit_process_command_line(OsStr::new("o"), &[], usize::MAX).is_err());
    // The arguments alone, which a plan renders, take what they take on the command line.
    let alone = process_command_line_units(OsStr::new(""), &arguments)
        - process_command_line_units(OsStr::new(""), &[]);
    let reserve = PROCESS_COMMAND_LINE_LIMIT - alone;
    fit_arguments(&arguments, reserve).unwrap();
    let held = fit_arguments(&arguments, reserve + 1).unwrap_err();
    assert!(
        held.to_string()
            .contains("held in reserve for the rest of the launch"),
        "{held}"
    );
}

#[test]
fn every_launch_names_its_own_inputs_alike() {
    let launch = VmmLaunch {
        openvmm: Path::new("/o/openvmm"),
        hypervisor: Hypervisor::Mshv,
        memory_mib: 512,
        kernel: Path::new("/k/vmlinux"),
        initrd: Path::new("/k/initramfs.cpio.gz"),
        endpoint: "/state/t/control.sock",
        report: Path::new("/state/t/outcome.json"),
    };
    let arguments = launch.arguments("quiet nvx_sandbox=1");
    let arguments: Vec<&str> = arguments.iter().map(|a| a.to_str().unwrap()).collect();
    assert_eq!(
        arguments,
        [
            "--single-process",
            "--hypervisor",
            "mshv",
            "--memory",
            "512M",
            "--kernel",
            "/k/vmlinux",
            "--initrd",
            "/k/initramfs.cpio.gz",
            "--cmdline",
            "quiet nvx_sandbox=1",
            "--virtio-console",
            "none",
            "--microvm-control-console",
            "listen=/state/t/control.sock",
            "--microvm-control-auth-stdin",
            "--microvm-report",
            "/state/t/outcome.json",
        ]
    );
    // A managed launch names them after its lifecycle, with its own command line.
    let spec = spec();
    let endpoint = "/state/t/control.sock";
    let report = Path::new("/state/t/outcome.json");
    let launch = spec.vmm_launch(endpoint, report);
    assert_eq!(
        launch,
        VmmLaunch {
            openvmm: Path::new("/o"),
            hypervisor: Hypervisor::Kvm,
            memory_mib: DEFAULT_MEMORY_MIB,
            kernel: Path::new("/k/vmlinux"),
            initrd: Path::new("/k/initrd"),
            endpoint,
            report,
        }
    );
    let managed = spec.openvmm_arguments(endpoint, report).unwrap();
    let own = launch.arguments(&spec.kernel_command_line().unwrap());
    assert_eq!(managed[6..6 + own.len()], own);
    assert_eq!(managed[4..6], ["--microvm-lifecycle", "managed"]);
}

#[test]
fn the_reserve_of_a_plan_holds_the_rest_of_a_launch_of_inputs_of_max_path() {
    // Each input of `MAX_PATH` code units, with spaces, which the command line quotes.
    let path = |name: &str| format!("C:\\{name} {}", "x".repeat(260 - 4 - name.len()));
    let [openvmm, kernel, initrd, endpoint, report] =
        ["openvmm", "vmlinux", "initrd", "control", "report"].map(path);
    for input in [&openvmm, &kernel, &initrd, &endpoint, &report] {
        assert_eq!(input.encode_utf16().count(), 260);
    }
    let launch = VmmLaunch {
        openvmm: Path::new(&openvmm),
        hypervisor: Hypervisor::Mshv,
        memory_mib: u32::MAX,
        kernel: Path::new(&kernel),
        initrd: Path::new(&initrd),
        endpoint: &endpoint,
        report: Path::new(&report),
    };
    // The plan's options count the kernel command line, whatever it is: the rest is the
    // program and the other arguments.
    let kernel_command_line = "nvx_sandbox=1";
    let own = launch.arguments(kernel_command_line);
    let cmdline: Vec<OsString> = ["--cmdline", kernel_command_line]
        .map(OsString::from)
        .into();
    let rest =
        process_command_line_units(launch.openvmm.as_os_str(), &own) - arguments_units(&cmdline);
    // A checked file's name that takes the most more than its path in the plan: a quoted
    // final path of `MAX_PATH` behind `\\?\`, for a path of 4.
    let named = |path: &str| arguments_units(&[OsString::from(format!("distro:file:{path}"))]);
    let final_path = format!(r"\\?\{}", path("distro"));
    let growth = named(&final_path) - named(r"C:\d");
    assert_eq!(growth, 262);
    assert!(
        rest + 2 * growth <= VmmLaunch::RESERVE,
        "{rest} + 2 * {growth}"
    );
}

#[test]
fn kernel_parameters_are_checked() {
    let hostname = DEFAULT_HOSTNAME;
    assert!(kernel_parameters_problem("quiet loglevel=0", hostname).is_none());
    for reserved in [
        "tsc=reliable",
        "hostname=other",
        "quiet nvx_exec=/bin/sh",
        "a -- b",
        "say \"hi\"",
        "nul\0",
    ] {
        assert!(
            kernel_parameters_problem(reserved, hostname).is_some(),
            "{reserved:?}"
        );
    }
    // The parameters, a space, the hostname token, and a NUL fill the command line exactly.
    let room = COMMAND_LINE_BUDGET - base_command_line("", hostname).len() - 2;
    assert!(kernel_parameters_problem(&"x".repeat(room), hostname).is_none());
    assert_eq!(
        kernel_parameters_problem(&"x".repeat(room + 1), hostname).unwrap(),
        "and the hostname leave no room in the 1024-byte kernel command line"
    );
}

#[test]
fn kernel_parameters_are_read_as_the_kernel_reads_them() {
    let hostname = DEFAULT_HOSTNAME;
    // `à`, U+00E0, is C3 A0 in UTF-8, and the kernel takes the byte 0xa0 for whitespace, so it
    // reads the text after it as another parameter (microsoft/nvx#723). So do U+00A0 and
    // U+0120, and the vertical tab, the form feed, and the carriage return.
    for (extra, problem) in [
        (
            "quiet\u{e0}tsc=unstable",
            "uses the reserved token \"tsc=unstable\"",
        ),
        ("quiet\u{e0}--", "must not contain the -- delimiter"),
        (
            "quiet\u{a0}-- loglevel=0",
            "must not contain the -- delimiter",
        ),
        (
            "quiet\u{a0}hostname=other",
            "uses the reserved token \"hostname=other\"",
        ),
        (
            "quiet\u{120}nvx_maps=5",
            "uses the reserved token \"nvx_maps=5\"",
        ),
        (
            "quiet\u{b}nvx_workload_account=create",
            "uses the reserved token \"nvx_workload_account=create\"",
        ),
        (
            "quiet\u{c}tsc=reliable",
            "uses the reserved token \"tsc=reliable\"",
        ),
        (
            "quiet\rhostname=other",
            "uses the reserved token \"hostname=other\"",
        ),
        // The kernel matches names whether they spell a dash or an underscore. A launch also
        // reserves `tsc` and `hostname` without a value: the early handler of `hostname`
        // copies the value that such a parameter lacks.
        ("nvx-maps=5", "uses the reserved token \"nvx-maps=5\""),
        ("tsc", "uses the reserved token \"tsc\""),
        ("quiet hostname", "uses the reserved token \"hostname\""),
        // The command line starts with the parameters trimmed of any whitespace.
        (
            "\u{2003}tsc=unstable",
            "uses the reserved token \"tsc=unstable\"",
        ),
    ] {
        assert_eq!(
            kernel_parameters_problem(extra, hostname).as_deref(),
            Some(problem),
            "{extra:?}"
        );
    }
    // The kernel reads one parameter where none of its whitespace separates two, and a `--`
    // with a value is no delimiter.
    for extra in [
        "quiet\u{2003}tsc=unstable",
        "quiet\u{3000}nvx_maps=5",
        "quiet\u{800}tsc=unstable",
        "x=tsc=unstable",
        "tscx=1",
        "hostnames=a",
        "xnvx_maps=5",
        "a--",
        "--=1",
    ] {
        assert_eq!(
            kernel_parameters_problem(extra, hostname),
            None,
            "{extra:?}"
        );
    }
}

/// Returns a strategy for what the kernel takes for whitespace between two parameters: an
/// ASCII byte that `isspace` accepts, or a character above U+007F whose code point ends with
/// the six bits 0x20, which UTF-8 encodes in a last byte of 0xa0.
fn kernel_separator() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::sample::select(vec![' ', '\t', '\n', '\u{b}', '\u{c}', '\r']),
        (0x80_u32..0x11_0000)
            .prop_filter_map("a surrogate", |code| char::from_u32((code & !0x3f) | 0x20)),
    ]
    .prop_map(String::from)
}

/// Returns a strategy for extra kernel parameters: mostly letters, equals signs, dashes,
/// underscores, the names that a launch reserves, and the whitespace of the kernel and of
/// Unicode, and now and then a quote or a NUL.
fn extra_parameters() -> impl Strategy<Value = String> {
    /// The pieces of the parameters. U+0800 holds the byte 0xa0 before its last byte, and
    /// U+2003 and U+3000 hold none.
    const PIECES: [&str; 23] = [
        "a", "b", "=", "-", "--", "_", "nvx_", "nvx-", "tsc", "hostname", " ", "\t", "\n", "\u{b}",
        "\u{c}", "\r", "\u{a0}", "\u{e0}", "\u{120}", "\u{2020}", "\u{800}", "\u{2003}",
        "\u{3000}",
    ];
    prop::collection::vec(
        prop_oneof![
            24 => prop::sample::select(PIECES.to_vec()),
            1 => prop::sample::select(vec!["\"", "'", "\0"]),
        ],
        0..24,
    )
    .prop_map(|pieces| pieces.concat())
}

proptest! {
    /// No reserved parameter passes, whatever separates it from the parameter before it.
    #[test]
    fn reserved_parameters_are_refused_whatever_separates_them(
        before in "[a-z]{1,8}",
        separator in kernel_separator(),
        reserved in prop::sample::select(vec![
            "tsc=unstable",
            "tsc",
            "hostname=other",
            "hostname",
            "nvx_maps=5",
            "nvx-maps=5",
            "nvx_workload_account=create",
            "--",
        ]),
        after in "( [a-z]{1,8})?",
    ) {
        let extra = format!("{before}{separator}{reserved}{after}");
        prop_assert!(kernel_parameters_problem(&extra, DEFAULT_HOSTNAME).is_some(), "{:?}", extra);
    }

    /// The parameters that a launch accepts are those in which the kernel reads no reserved
    /// parameter, as it compares names, and after which it reads the `hostname=` parameter:
    /// the parameters hold no NUL and no quote, and no `--` ends them.
    #[test]
    fn the_kernel_reads_the_hostname_after_accepted_parameters(extra in extra_parameters()) {
        let hostname = DEFAULT_HOSTNAME;
        let reading = model::read(base_command_line(&extra, hostname).as_bytes());
        let reserved = |name: &[u8]| {
            model::parameqn(name, b"nvx_", 4)
                || model::parameq(name, b"tsc")
                || model::parameq(name, b"hostname")
        };
        let readable = !extra.contains(['\0', '"', '\''])
            && reading.rest.is_none()
            && reading.parameters.split_last().is_some_and(|((name, value), others)| {
                name == b"hostname"
                    && value.as_deref() == Some(hostname.as_bytes())
                    && !others.iter().any(|(name, _)| reserved(name))
            });
        prop_assert_eq!(
            kernel_parameters_problem(&extra, hostname).is_none(),
            readable,
            "{:?}",
            extra
        );
    }
}

#[test]
fn every_command_line_checks_the_specification() {
    let endpoint = "/state/t/control.sock";
    let report = Path::new("/state/t/outcome.json");
    spec().check().unwrap();
    let changes: [fn(&mut LaunchSpec); 11] = [
        |spec| spec.memory_mib = 0,
        |spec| spec.identity.uid = 0,
        |spec| spec.identity.gid = 0,
        // A hostname cannot smuggle further kernel parameters in.
        |spec| spec.hostname = "a nvx_maps=1".to_owned(),
        |spec| spec.hostname = "Sandbox".to_owned(),
        |spec| spec.guest_network = "10.0.0.1/24".to_owned(),
        |spec| spec.kernel_parameters = "nvx_maps=5".to_owned(),
        |spec| spec.kernel_parameters = "nvx_workload_account=create".to_owned(),
        |spec| spec.kernel_parameters = "quiet hostname=other".to_owned(),
        // The kernel reads the byte 0xa0 of `à` as whitespace (microsoft/nvx#723).
        |spec| spec.kernel_parameters = "quiet\u{e0}tsc=unstable".to_owned(),
        |spec| spec.kernel_parameters = "quiet\u{e0}--".to_owned(),
    ];
    for change in changes {
        let mut spec = spec();
        change(&mut spec);
        for error in [
            spec.check().unwrap_err(),
            spec.kernel_command_line().unwrap_err(),
            spec.openvmm_arguments(endpoint, report).unwrap_err(),
            spec.check_process_command_line(endpoint, report)
                .unwrap_err(),
        ] {
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{spec:?}");
        }
    }
    let mut root = spec();
    root.identity.gid = 0;
    assert_eq!(
        root.check().unwrap_err().to_string(),
        "workloads must run as a non-root UID and GID"
    );
    let mut reserved = spec();
    reserved.kernel_parameters = "nvx_maps=5".to_owned();
    assert_eq!(
        reserved.check().unwrap_err().to_string(),
        "kernel_parameters uses the reserved token \"nvx_maps=5\""
    );
}

#[test]
fn timeouts_are_bounded() {
    assert_eq!(timeout_problem(Duration::ZERO, false), None);
    assert_eq!(
        timeout_problem(Duration::ZERO, true).unwrap(),
        "must be positive"
    );
    assert_eq!(timeout_problem(MAX_TIMEOUT, true), None);
    for beyond in [MAX_TIMEOUT + Duration::from_nanos(1), Duration::MAX] {
        assert_eq!(
            timeout_problem(beyond, false).unwrap(),
            "must be at most 30 days"
        );
    }
}

#[test]
fn workloads_that_map_host_paths_take_the_host_identity() {
    let configured = WorkloadIdentity::select(1000, 1000, false, true);
    assert_eq!(
        configured,
        WorkloadIdentity {
            uid: 1000,
            gid: 1000,
            create_account: false,
        }
    );
    assert_eq!(WorkloadIdentity::select(7, 8, true, false).uid, 7);
    let mapped = WorkloadIdentity::select(7, 8, true, true);
    match host_identity(true) {
        Some((uid, gid)) => assert_eq!(
            mapped,
            WorkloadIdentity {
                uid,
                gid,
                create_account: true,
            }
        ),
        None => assert_eq!(mapped, WorkloadIdentity::select(7, 8, false, false)),
    }
}
