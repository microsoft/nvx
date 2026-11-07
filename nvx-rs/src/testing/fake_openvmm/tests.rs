use super::*;

#[test]
fn shells_raise_shlvl_like_busybox() {
    let level = |value: &str| {
        let mut environment = vec![("SHLVL".to_owned(), value.to_owned())];
        start_shell(&mut environment, "/work");
        assert_eq!(environment[1], ("PWD".to_owned(), "/work".to_owned()));
        environment.swap_remove(0).1
    };
    assert_eq!(level("1"), "2");
    assert_eq!(level("7x"), "8");
    assert_eq!(level(" +3"), "4");
    assert_eq!(level("nothing"), "1");
    assert_eq!(level("4294967295"), "0");
    assert_eq!(level("-1"), "0");
    assert_eq!(level("-3"), "4294967294");
    let mut fresh = Environment::new();
    start_shell(&mut fresh, "/");
    assert_eq!(fresh[0], ("SHLVL".to_owned(), "1".to_owned()));
}

#[test]
fn the_current_guest_advertises_every_required_feature() {
    assert_eq!(GUEST_FEATURES, GuestFeatures::REQUIRED);
    assert!(GUEST_FEATURES.missing(GuestFeatures::REQUIRED).is_empty());
}

#[test]
fn mount_children_resolve_to_hosts_of_their_kind() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().to_str().unwrap().to_owned();
    let file = temporary.path().join("file").to_str().unwrap().to_owned();
    fs::write(&file, b"").unwrap();
    for (value, index, host) in [
        (format!("0,{directory},rw"), Some(0), &directory),
        (format!("1,{file},ro,file"), Some(1), &file),
        // Without an index, any name goes, as for the children of a sandbox's aggregate.
        (format!("digest,{directory},ro"), None, &directory),
    ] {
        assert_eq!(mount_child_host(&value, index), Ok(PathBuf::from(host)));
    }
    for (value, index) in [
        (format!("1,{directory},rw"), Some(0)),
        (format!("0,{file},rw"), None),
        (format!("0,{directory},ro,file"), None),
        (format!("0,{directory}-missing,rw"), None),
        (format!("0,{directory},wo"), None),
        (format!("0,{directory}"), None),
        ("0".to_owned(), None),
    ] {
        assert_eq!(
            mount_child_host(&value, index),
            Err(format!("invalid --mount-child {value}"))
        );
    }
}

#[test]
fn sandbox_blocks_are_a_distro_layer_and_a_scratch_disk() {
    let temporary = tempfile::tempdir().unwrap();
    let file = temporary.path().join("block");
    fs::write(&file, [0; 512]).unwrap();
    let file = file.to_str().unwrap();
    let distro = format!("distro:file:{file},ro");
    let scratch = format!("scratch:file:{file}");
    let (distro, scratch) = (distro.as_str(), scratch.as_str());
    let roles = |blocks: &[&str], ram_overlay: bool| {
        check_sandbox_blocks(blocks.iter(), ram_overlay).map(|checked| {
            checked
                .into_iter()
                .map(|block| (block.role, block.read_only))
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(
        roles(&[distro, scratch], false),
        Ok(vec![
            ("distro".to_owned(), true),
            ("scratch".to_owned(), false)
        ])
    );
    assert_eq!(
        roles(&[distro], true),
        Ok(vec![("distro".to_owned(), true)])
    );
    // Like OpenVMM, the fake refuses the retired runtime and custom layers.
    for retired in ["runtime", "custom"] {
        let layer = format!("{retired}:file:{file},ro");
        assert_eq!(
            roles(&[distro, &layer, scratch], false),
            Err(format!("invalid --microvm-sandbox-block {layer}"))
        );
    }
    assert_eq!(
        roles(&[distro, distro, scratch], false),
        Err(format!("invalid --microvm-sandbox-block {distro}"))
    );
    for blocks in [[distro], [scratch]] {
        assert_eq!(
            roles(&blocks, false),
            Err("a sandbox needs a distro layer and a scratch disk".to_owned())
        );
    }
    assert_eq!(
        roles(&[distro, scratch], true),
        Err("microVM RAM-backed overlay requires exactly one read-only distro block".to_owned())
    );
}

/// Returns a directory with a kernel, an initramfs, a share with a `hidden` directory, and a
/// snapshot directory, and the parse of the command line that `arguments` gives with the
/// paths of that directory in place of `{dir}`.
fn parse_run(arguments: &[&str]) -> (tempfile::TempDir, Result<Options, String>) {
    parse_run_in(arguments, |_| {})
}

/// Returns the directory of [`parse_run`], and the parse of `arguments` there once `prepare` has
/// changed the directory.
fn parse_run_in(
    arguments: &[&str],
    prepare: impl FnOnce(&Path),
) -> (tempfile::TempDir, Result<Options, String>) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("vmlinux"), b"kernel").unwrap();
    fs::write(root.join("initramfs.cpio.gz"), b"initrd").unwrap();
    fs::create_dir_all(root.join("share").join("hidden")).unwrap();
    fs::create_dir(root.join("snapshot")).unwrap();
    prepare(root);
    let directory = root.to_str().unwrap();
    let arguments: Vec<String> = arguments
        .iter()
        .map(|argument| argument.replace("{dir}", directory))
        .collect();
    let options = parse("openvmm", &arguments);
    (temporary, options)
}

/// The options of every run, before those of its boot or restore.
const RUN: [&str; 7] = [
    "--single-process",
    "--machine",
    "microvm",
    "--processors",
    "4",
    "--hypervisor",
    "kvm",
];

fn run_with<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    RUN.iter().chain(extra).copied().collect()
}

#[test]
fn runs_boot_or_restore_without_a_lifecycle() {
    let boot = run_with(&[
        "--memory",
        "128M",
        "--memory-capacity",
        "1024M",
        "--kernel",
        "{dir}/vmlinux",
        "--initrd",
        "{dir}/initramfs.cpio.gz",
        "--virtio-console",
        "console",
        "--cpu-profile",
        "host",
        "--mount",
        "/data,{dir}/share,rw",
        "--mount-deny",
        "hidden",
    ]);
    let (temporary, options) = parse_run(&boot);
    let options = options.unwrap();
    assert_eq!(options.lifecycle, Lifecycle::Run);
    assert!(!options.sandbox);
    assert_eq!(options.restore, None);
    assert_eq!(options.exit_status, None);
    assert_eq!(
        options.args_dump,
        temporary.path().join("fake-openvmm-.json")
    );
    let restore = run_with(&[
        "--restore-snapshot",
        "{dir}/snapshot",
        "--restore-processors",
        "2",
        "--restore-memory",
        "512M",
        "--restore-ready-path",
        "{dir}/ready.sock",
        "--cmdline",
        "fake_exit_status=3",
        "--microvm-report",
        "{dir}/share/report.json",
    ]);
    let (temporary, options) = parse_run(&restore);
    let options = options.unwrap();
    let snapshot = temporary.path().join("snapshot");
    assert_eq!(options.restore.as_ref(), Some(&snapshot));
    assert_eq!(options.exit_status, Some(3));
    // A restore has no kernel, so the command line goes next to the snapshot.
    assert_eq!(
        options.args_dump,
        temporary.path().join("fake-openvmm-share.json")
    );
    // A restored NIC keeps the addressing that its snapshot saved, so a restore takes the
    // network profile and policy without `--net`.
    let networked = run_with(&[
        "--restore-snapshot",
        "{dir}/snapshot",
        "--network-profile",
        "portable",
        "--network-egress",
        "deny",
        "--network-egress-allow",
        "10.0.0.1/32:tcp:443",
        "--host-loopback",
        "deny",
    ]);
    let (temporary, options) = parse_run(&networked);
    let options = options.unwrap();
    assert_eq!(options.restore, Some(temporary.path().join("snapshot")));
    assert!(options.network.requested);
    assert_eq!(options.network.mode, "rules");
    assert_eq!(options.network.allow, 1);
    assert_eq!(options.network.host_loopback, "deny");
    // A restore that replaces the policy of a NIC template may publish the guest's listeners
    // and forward host ports.
    let replaced = run_with(&[
        "--restore-snapshot",
        "{dir}/snapshot",
        "--restore-network-policy",
        "--network-profile",
        "portable",
        "--network-egress",
        "allow",
        "--network-ingress",
        "allow",
        "--host-loopback",
        "allow",
        "--host-loopback-forward",
        "tcp:3000:8080",
    ]);
    let (_temporary, options) = parse_run(&replaced);
    let options = options.unwrap();
    assert_eq!(options.network.mode, "allow-all");
    assert_eq!(options.network.host_loopback, "allow");
}

#[test]
fn runs_refuse_what_their_boot_or_restore_does_not_take() {
    const KERNEL: [&str; 4] = [
        "--kernel",
        "{dir}/vmlinux",
        "--initrd",
        "{dir}/initramfs.cpio.gz",
    ];
    const CONSOLE: [&str; 2] = ["--virtio-console", "console"];
    fn boot(extra: &[&'static str]) -> Vec<&'static str> {
        let mut arguments = vec!["--memory", "128M"];
        arguments.extend(KERNEL);
        arguments.extend(CONSOLE);
        arguments.extend(extra);
        run_with(&arguments)
    }
    fn restore(extra: &[&'static str]) -> Vec<&'static str> {
        let mut arguments = vec!["--restore-snapshot", "{dir}/snapshot"];
        arguments.extend(extra);
        run_with(&arguments)
    }
    let kernel = KERNEL;
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            restore(&["--memory", "128M"]),
            "a restore takes no --memory",
        ),
        (restore(&kernel), "a restore takes no --kernel"),
        (
            restore(&["--restore-processors", "8"]),
            "--restore-processors cannot exceed --processors",
        ),
        (
            restore(&["--restore-memory", "1G"]),
            "--restore-memory must be given in MiB",
        ),
        (
            restore(&["--net", "10.0.0.2/24", "--network-profile", "portable"]),
            "restore takes microVM network addressing from saved state",
        ),
        (
            restore(&["--network-profile", "open"]),
            "--network-profile must be portable, not open",
        ),
        (
            boot(&["--network-profile", "portable"]),
            "--network-profile portable requires --net or --restore-snapshot",
        ),
        (
            boot(&["--restore-network-policy"]),
            "--restore-network-policy requires --restore-snapshot",
        ),
        (
            restore(&[
                "--network-profile",
                "portable",
                "--network-ingress",
                "allow",
            ]),
            "microVM snapshots do not support --network-ingress allow, whose published guest \
                 listeners are live host attachments, except at a restore with \
                 --restore-network-policy",
        ),
        (
            restore(&[
                "--network-profile",
                "portable",
                "--host-loopback",
                "allow",
                "--host-loopback-forward",
                "tcp:3000:8080",
            ]),
            "microVM snapshots do not support --host-loopback allow, whose published guest \
                 listeners are live host attachments, except at a restore with \
                 --restore-network-policy",
        ),
        (
            boot(&["--restore-processors", "2"]),
            "--restore-processors requires --restore-snapshot",
        ),
        (
            boot(&["--memory-capacity", "2"]),
            "--memory-capacity must be given in MiB",
        ),
        (
            boot(&["--cpu-profile", "host", "--cpu-profile", "auto"]),
            "--cpu-profile must be given at most once",
        ),
        (
            boot(&["--microvm-workload-identity", "1000:1000"]),
            "a microVM run takes no --microvm-workload-identity",
        ),
        // A cold boot gives the guest's boot console the terminal, which a restore attaches again
        // only if its snapshot's was there, as the restore checks.
        (
            run_with(&[&["--memory", "128M"][..], &KERNEL[..]].concat()),
            "--virtio-console must be given exactly once",
        ),
        (
            run_with(
                &[
                    &["--memory", "128M"][..],
                    &KERNEL[..],
                    &["--virtio-console", "none"][..],
                ]
                .concat(),
            ),
            "--virtio-console must be console, not none",
        ),
        (
            restore(&["--virtio-console", "stdio"]),
            "--virtio-console must be console or none, not stdio",
        ),
        (
            restore(&[&CONSOLE[..], &CONSOLE[..]].concat()),
            "--virtio-console must be given at most once",
        ),
        (
            boot(&["--mount", "/data,{dir}/share,ro", "--mount-deny", "missing"]),
            "invalid --mount-deny missing",
        ),
        (
            boot(&["--mount-allow", "hidden"]),
            "invalid --mount-allow hidden",
        ),
    ];
    for (arguments, message) in cases {
        let (_temporary, options) = parse_run(&arguments);
        assert_eq!(options.err().as_deref(), Some(message), "{arguments:?}");
    }
    let mut missing = RUN.to_vec();
    missing.truncate(3);
    missing.extend(["--hypervisor", "kvm", "--memory", "128M"]);
    missing.extend(CONSOLE);
    let (_temporary, options) = parse_run(&missing);
    assert_eq!(
        options.err().as_deref(),
        Some("--processors must be given exactly once")
    );
}

/// Returns the manifest of a snapshot whose machine contract holds `attachments`, each the stable
/// ID of its device and its reconnect policy, as `mesh` encodes them, with fields of each wire
/// type that a manifest and its contract hold besides.
fn manifest(attachments: &[(&str, &str)]) -> Vec<u8> {
    fn put_varint(bytes: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            bytes.push(value as u8 | 0x80);
            value >>= 7;
        }
        bytes.push(value as u8);
    }
    fn field(number: u64, value: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        put_varint(&mut bytes, number << 3 | 2);
        put_varint(&mut bytes, value.len() as u64);
        bytes.extend_from_slice(value);
        bytes
    }
    // A device, the time ABI version of the time contract, and a fixed 64-bit and 32-bit integer.
    let mut contract = field(8, &field(1, b"console:microvm-virtio0"));
    contract.extend(field(11, &[0x08, 0x01]));
    contract.extend([0x09, 1, 2, 3, 4, 5, 6, 7, 8, 0x15, 1, 2, 3, 4]);
    for (stable_id, policy) in attachments {
        let mut attachment = field(1, stable_id.as_bytes());
        attachment.extend(field(2, b"virtio-console"));
        attachment.extend([0x18, 0x01]);
        attachment.extend(field(4, policy.as_bytes()));
        contract.extend(field(10, &attachment));
    }
    let mut manifest = vec![0x08, 0x02];
    manifest.extend(field(9, &contract));
    manifest.extend(field(10, b"OPENVMM_SNAPSHOT_V2\0"));
    manifest
}

#[test]
fn restores_take_the_boot_console_that_their_snapshots_had() {
    let temporary = tempfile::tempdir().unwrap();
    let snapshot = temporary.path();
    let check = |manifest: Option<Vec<u8>>, requested: Option<&str>| {
        match manifest {
            Some(manifest) => fs::write(snapshot.join(SNAPSHOT_MANIFEST), manifest).unwrap(),
            None => {
                let _ = fs::remove_file(snapshot.join(SNAPSHOT_MANIFEST));
            }
        }
        check_restored_console(snapshot, requested)
    };
    let refused = |message: &str| Err(message.to_owned());
    let control = ("console:microvm-control0", "broker-authenticated-listener");
    let inherited = ("console:microvm-virtio0", "require-inherited-attachment");
    let disconnected = ("console:microvm-virtio0", "discard-while-disconnected");
    let listener = ("console:microvm-virtio0", "recreate-listener");
    // A snapshot without a boot console, or without a manifest, takes no console.
    let without =
        refused("a restore-time virtio-console cannot be added to a snapshot without one");
    for manifest in [None, Some(manifest(&[])), Some(manifest(&[control]))] {
        assert_eq!(check(manifest.clone(), None), Ok(()));
        assert_eq!(check(manifest.clone(), Some("console")), without);
        assert_eq!(check(manifest, Some("none")), without);
    }
    // One whose boot console was on the terminal takes it there again, and only there.
    let terminal = manifest(&[control, inherited]);
    assert_eq!(check(Some(terminal.clone()), Some("console")), Ok(()));
    assert_eq!(
        check(Some(terminal.clone()), None),
        refused("snapshot requires --virtio-console console as a replacement attachment")
    );
    assert_eq!(
        check(Some(terminal), Some("none")),
        refused("snapshot microVM console identity is not canonical")
    );
    // One whose boot console was disconnected, or listened, keeps its attachment, and refuses the
    // terminal.
    let mismatch = refused("restore-time virtio-console does not match the snapshot attachment");
    let disconnected = manifest(&[disconnected]);
    assert_eq!(check(Some(disconnected.clone()), None), Ok(()));
    assert_eq!(check(Some(disconnected.clone()), Some("none")), Ok(()));
    assert_eq!(check(Some(disconnected), Some("console")), mismatch);
    assert_eq!(check(Some(manifest(&[listener])), None), Ok(()));
    assert_eq!(
        check(Some(manifest(&[listener])), Some("console")),
        mismatch
    );
    // OpenVMM reads the first attachment of the boot console.
    assert_eq!(
        check(Some(manifest(&[inherited, listener])), Some("console")),
        Ok(())
    );
    // A manifest that the fake cannot decode fails the restore.
    let mut truncated = manifest(&[inherited]);
    truncated.pop();
    let path = snapshot.join(SNAPSHOT_MANIFEST);
    assert_eq!(
        check(Some(truncated), None),
        Err(format!("{} is malformed: it is truncated", path.display()))
    );
}

#[test]
fn launches_with_a_lifecycle_take_no_options_of_a_run() {
    for option in ["--processors", "--cpu-profile", "--restore-snapshot"] {
        let arguments = [
            "--single-process",
            "--microvm-lifecycle",
            "managed",
            option,
            "1",
        ];
        let (_temporary, options) = parse_run(&arguments);
        assert_eq!(
            options.err(),
            Some(format!("unsupported argument {option}")),
            "{option}"
        );
    }
}

/// The options of the cold boot of a run, with `{dir}` in place of the directory of [`parse_run`].
const BOOT: [&str; 6] = [
    "--memory",
    "128M",
    "--kernel",
    "{dir}/vmlinux",
    "--initrd",
    "{dir}/initramfs.cpio.gz",
];

/// Returns the options of the cold boot of a run whose kernel command line is `command_line`,
/// followed by `extra`, with the boot console on the terminal unless `extra` names one.
fn boot_with<'a>(command_line: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut arguments = run_with(&BOOT);
    arguments.extend(["--cmdline", command_line]);
    if !extra.contains(&"--virtio-console") {
        arguments.extend(["--virtio-console", "console"]);
    }
    arguments.extend(extra);
    arguments
}

/// Makes `root` private to its user, as the directory of a control console's socket must be on
/// Unix.
fn private_directory(root: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = root;
}

#[test]
fn runs_take_their_mode_and_workload_identity_from_the_cli_tokens() {
    // In the one-shot mode, the default, a run that names an entrypoint runs it, with its
    // arguments, as the identity of its tokens.
    for command_line in [
        "nvx_mode=one-shot nvx_identity=1000:1001 nvx_entrypoint=/bin/echo nvx_arg=a nvx_arg=b",
        "nvx_identity=1000:1001 nvx_entrypoint=/bin/echo nvx_arg=a nvx_arg=b",
    ] {
        let arguments = boot_with(command_line, &["--mount", "/data,{dir}/share,rw"]);
        let (_temporary, options) = parse_run(&arguments);
        let options = options.unwrap();
        assert_eq!(options.lifecycle, Lifecycle::OneShot, "{command_line}");
        assert!(options.run && !options.sandbox);
        assert_eq!(options.workload, ["/bin/echo", "a", "b"]);
        assert_eq!(options.workload_uid, 1000);
        assert!(options.endpoint.is_empty());
    }
    // One that names none boots on the terminal, as a run without the CLI's tokens does.
    let arguments = boot_with("nvx_mode=one-shot nvx_identity=1000:1001", &[]);
    let (_temporary, options) = parse_run(&arguments);
    let options = options.unwrap();
    assert_eq!(options.lifecycle, Lifecycle::Run);
    assert!(options.workload.is_empty());
    // In the managed mode, a run serves the control console for workloads of the identity of its
    // tokens, which may ask the guest for an account, with the targets of its directory shares as
    // directories of its guest. The console's socket is in a directory private to its user.
    let managed = boot_with(
        "nvx_mode=managed nvx_identity=1000:1001 nvx_workload_account=create \
         nvx_share=0,/data,rw nvx_share=1,/etc/app.conf,ro,file",
        &[
            "--mount-aggregate",
            "/run/nvx/shares",
            "--mount-child",
            "0,{dir}/share,rw",
            "--virtio-console",
            "none",
            "--microvm-control-console",
            "listen={dir}/control.sock",
            "--microvm-control-auth-stdin",
            "--microvm-report",
            "{dir}/outcome.json",
        ],
    );
    let (temporary, options) = parse_run_in(&managed, private_directory);
    let options = options.unwrap();
    assert_eq!(options.lifecycle, Lifecycle::Managed);
    assert!(options.run && !options.sandbox);
    assert_eq!(options.workload_uid, 1000);
    assert_eq!(options.share_targets, ["/data"]);
    assert!(options.workload.is_empty());
    assert_eq!(
        Path::new(&options.endpoint),
        temporary.path().join("control.sock")
    );
    assert_eq!(options.report, Some(temporary.path().join("outcome.json")));
}

#[test]
fn workloads_of_runs_can_enter_the_targets_of_their_direct_shares() {
    // A run's single directory share needs no nvx_share= token: the guest mounts it at its target,
    // which a workload of either mode can enter as its working directory.
    let managed = [
        "--virtio-console",
        "none",
        "--microvm-control-console",
        "listen={dir}/control.sock",
        "--microvm-control-auth-stdin",
        "--microvm-report",
        "{dir}/outcome.json",
    ];
    for (command_line, extra) in [
        ("nvx_identity=1000:1001 nvx_entrypoint=/bin/true", &[][..]),
        ("nvx_mode=managed nvx_identity=1000:1001", &managed[..]),
    ] {
        let arguments = boot_with(
            command_line,
            &[&["--mount", "/data,{dir}/share,rw"][..], extra].concat(),
        );
        let (_temporary, options) = parse_run_in(&arguments, private_directory);
        let options = options.unwrap();
        assert_eq!(options.share_targets, ["/data"], "{command_line}");
        assert!(
            Guest::new(&options).directories.contains("/data"),
            "{command_line}"
        );
    }
}

#[test]
fn restores_of_runs_resume_their_saved_guest_whatever_the_cli_tokens() {
    // The guest that a snapshot saved read its tokens when it booted, so the CLI's tokens on a
    // restore's command line run no workload: the restore resumes that guest, and one whose
    // snapshot directory does not exist fails, as a restore without the tokens does.
    for snapshot in ["{dir}/snapshot", "{dir}/missing"] {
        let arguments = run_with(&[
            "--restore-snapshot",
            snapshot,
            "--cmdline",
            "nvx_identity=1000:1000 nvx_entrypoint=/bin/true",
        ]);
        let (_temporary, options) = parse_run(&arguments);
        let options = options.unwrap();
        assert_eq!(options.lifecycle, Lifecycle::Run, "{snapshot}");
        assert!(options.workload.is_empty(), "{snapshot}");
        if snapshot.ends_with("missing") {
            assert_eq!(run_microvm(&options), 1);
        }
    }
}

#[test]
fn runs_refuse_the_cli_tokens_that_their_guest_refuses() {
    let identity_required = "the kernel command line must set one nvx_identity=";
    let cases = [
        ("nvx_mode=sideways", "unsupported mode sideways"),
        (
            "nvx_mode=managed nvx_mode=managed nvx_identity=1000:1001",
            "nvx_mode= must be given at most once",
        ),
        // A workload of either mode runs as the non-root identity of the CLI's tokens.
        ("nvx_entrypoint=/bin/true", identity_required),
        ("nvx_mode=managed", identity_required),
        (
            "nvx_entrypoint=/bin/true nvx_identity=0:1001",
            "invalid workload identity 0:1001",
        ),
        (
            "nvx_entrypoint=/bin/true nvx_identity=1000",
            "invalid workload identity 1000",
        ),
        // The shell `/init` reads IDs of decimal digits only.
        (
            "nvx_entrypoint=/bin/true nvx_identity=+1000:1001",
            "invalid workload identity +1000:1001",
        ),
        (
            "nvx_mode=managed nvx_identity=1000:+1001",
            "invalid workload identity 1000:+1001",
        ),
        (
            "nvx_entrypoint=/bin/true nvx_entrypoint=/bin/false nvx_identity=1000:1001",
            "the kernel command line must set one nvx_entrypoint",
        ),
        (
            "nvx_entrypoint=/bin/true nvx_identity=1000:1001 nvx_share=0,/data",
            "invalid nvx_share=0,/data",
        ),
        // The shell `/init` runs only an absolute entrypoint.
        (
            "nvx_entrypoint=bin/true nvx_identity=1000:1001",
            "nvx_entrypoint must be an absolute path",
        ),
        (
            "nvx_entrypoint= nvx_identity=1000:1001",
            "nvx_entrypoint must be an absolute path",
        ),
        // A managed run takes the control console of a managed launch.
        (
            "nvx_mode=managed nvx_identity=1000:1001",
            "--virtio-console must be none, not console",
        ),
    ];
    for (command_line, message) in cases {
        let (_temporary, options) = parse_run(&boot_with(command_line, &[]));
        assert_eq!(options.err().as_deref(), Some(message), "{command_line}");
    }
}

/// The command line of a template's capture, with `{dir}` in place of the directory of
/// [`parse_capture`].
const CAPTURE: &[&str] = &[
    "--machine",
    "microvm",
    "--microvm-sandbox-block",
    "distro:file:{dir}/placeholder,ro",
    "--microvm-sandbox-block",
    "scratch:file:{dir}/transaction/scratch.raw",
    "--processors",
    "2",
    "--memory",
    "512M",
    "--memory-capacity",
    "2048M",
    "--snapshot-destination",
    "{dir}/transaction/snapshot",
    "--snapshot-tier",
    "platform",
    "--snapshot-block-identity",
    "generation",
    "--snapshot-generation-id",
    "0123456789abcdef0123456789ABCDEF",
    "--single-process",
    "--hypervisor",
    "kvm",
    "--cpu-profile",
    "auto",
    "--kernel",
    "{dir}/vmlinux",
    "--initrd",
    "{dir}/initramfs.cpio.gz",
    "--cmdline",
    "nvx_sandbox=1",
    "--virtio-console",
    "none",
    "--microvm-control-console",
    "listen={dir}/transaction/control.sock",
    "--microvm-control-auth-stdin",
    "--microvm-report",
    "{dir}/transaction/outcome.json",
];

/// Returns a directory with a kernel, an initramfs, a placeholder, and a transaction with a
/// scratch disk, the directory and the transaction private to the current user, as the
/// directory of a control console's socket must be, and the parse of the command line that
/// `arguments` gives with the path of that directory in place of `{dir}`.
fn parse_capture(arguments: &[&str]) -> (tempfile::TempDir, Result<Options, String>) {
    parse_capture_in(arguments, |_| {})
}

/// Returns the directory of [`parse_capture`], and the parse of `arguments` there once
/// `prepare` has changed the directory.
fn parse_capture_in(
    arguments: &[&str],
    prepare: impl FnOnce(&Path),
) -> (tempfile::TempDir, Result<Options, String>) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("vmlinux"), b"kernel").unwrap();
    fs::write(root.join("initramfs.cpio.gz"), b"initrd").unwrap();
    fs::write(root.join("placeholder"), [0; 512]).unwrap();
    fs::create_dir(root.join("transaction")).unwrap();
    fs::write(root.join("transaction").join("scratch.raw"), [0; 512]).unwrap();
    #[cfg(unix)]
    for directory in [root.to_owned(), root.join("transaction")] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    prepare(root);
    let directory = root.to_str().unwrap();
    let arguments: Vec<String> = arguments
        .iter()
        .map(|argument| argument.replace("{dir}", directory))
        .collect();
    let options = parse("openvmm", &arguments);
    (temporary, options)
}

/// Returns the command line of [`CAPTURE`] with `value` in place of the value of `option`.
fn capture_with(option: &str, value: &'static str) -> Vec<&'static str> {
    let mut arguments = CAPTURE.to_vec();
    let at = arguments.iter().position(|given| *given == option).unwrap();
    arguments[at + 1] = value;
    arguments
}

#[test]
fn captures_boot_their_template_cold_on_their_block_devices() {
    let (temporary, options) = parse_capture(CAPTURE);
    let options = options.unwrap();
    let root = temporary.path();
    assert_eq!(options.lifecycle, Lifecycle::Capture);
    assert!(options.sandbox);
    assert_eq!(options.restore, None);
    let blocks: Vec<_> = options
        .blocks
        .iter()
        .map(|block| (block.role.as_str(), block.path.clone(), block.read_only))
        .collect();
    assert_eq!(
        blocks,
        [
            ("distro", root.join("placeholder"), true),
            (
                "scratch",
                root.join("transaction").join("scratch.raw"),
                false
            ),
        ]
    );
    let capture = options.capture.as_ref().unwrap();
    assert_eq!(
        capture.destination,
        root.join("transaction").join("snapshot")
    );
    assert_eq!(capture.memory, 512 * MIB);
    assert_eq!(capture.scenario, root.join(CAPTURE_SCENARIO_NAME));
    assert_eq!(
        options.endpoint,
        format!("{}/transaction/control.sock", root.to_str().unwrap())
    );
    assert_eq!(
        options.blocks_dump,
        root.join("fake-openvmm-transaction-blocks.json")
    );
    assert!(options.workload.is_empty());
    assert!(options.children.is_empty());
}

#[test]
fn captures_may_limit_the_processors_online_at_boot() {
    // The capture of `CAPTURE` has 2 processors.
    for command_line in ["nvx_sandbox=1 maxcpus=1", "nvx_sandbox=1 maxcpus=2"] {
        let (_temporary, options) = parse_capture(&capture_with("--cmdline", command_line));
        assert_eq!(options.err(), None, "{command_line}");
    }
}

#[cfg(unix)]
#[test]
fn captures_listen_in_the_directory_of_their_snapshot_destination() {
    let (temporary, options) = parse_capture(&capture_with(
        "--microvm-control-console",
        "listen={dir}/control.sock",
    ));
    let namespace = fs::canonicalize(temporary.path().join("transaction")).unwrap();
    assert_eq!(
        options.err(),
        Some(format!(
            "microVM console socket must be inside the snapshot parent namespace {}",
            namespace.display()
        ))
    );
}

#[cfg(unix)]
#[test]
fn control_consoles_listen_in_a_private_directory_of_their_user() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let refusal = "microVM control endpoint parent must be a non-symlink directory owned by \
                       OpenVMM with mode 0700";
    // OpenVMM takes exactly the mode 0700.
    for mode in [0o750, 0o701, 0o1700] {
        let (_temporary, options) = parse_capture_in(CAPTURE, |root| {
            let transaction = root.join("transaction");
            fs::set_permissions(transaction, fs::Permissions::from_mode(mode)).unwrap();
        });
        assert_eq!(options.err().as_deref(), Some(refusal), "{mode:o}");
    }
    // It follows no link to the socket's directory, even to a private one.
    let (_temporary, options) = parse_capture_in(
        &capture_with(
            "--microvm-control-console",
            "listen={dir}/link/control.sock",
        ),
        |root| symlink(root.join("transaction"), root.join("link")).unwrap(),
    );
    assert_eq!(options.err().as_deref(), Some(refusal));
    let (temporary, options) = parse_capture(&capture_with(
        "--microvm-control-console",
        "listen={dir}/missing/control.sock",
    ));
    let missing = format!(
        "failed to inspect microVM control endpoint parent {}: ",
        temporary.path().join("missing").display()
    );
    let error = options.err().unwrap();
    assert!(error.starts_with(&missing), "{error}");
}

#[cfg(unix)]
#[test]
fn a_restore_listens_in_the_directory_of_the_snapshot_that_it_restores() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let (clone, generation) = (
        temporary.path().join("clone"),
        temporary.path().join("generation"),
    );
    for directory in [&clone, &generation] {
        fs::create_dir(directory).unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let endpoint = clone.join("control.sock");
    let endpoint = endpoint.to_str().unwrap();
    assert_eq!(
        check_control_console(endpoint, Some(&clone.join("snapshot"))),
        Ok(())
    );
    // A VM that restores a snapshot elsewhere cannot listen in its own directory.
    assert_eq!(
        check_control_console(endpoint, Some(&generation.join("snapshot"))),
        Err(format!(
            "microVM console socket must be inside the snapshot parent namespace {}",
            fs::canonicalize(&generation).unwrap().display()
        ))
    );
    // Without a snapshot, the socket's directory alone counts.
    assert_eq!(check_control_console(endpoint, None), Ok(()));
}

#[test]
fn captures_refuse_what_a_template_does_not_take() {
    let mut lifecycle = CAPTURE.to_vec();
    lifecycle.extend(["--microvm-lifecycle", "managed"]);
    // OpenVMM refuses the retired runtime layer before the capture's own checks.
    let mut runtime = CAPTURE.to_vec();
    runtime.extend([
        "--microvm-sandbox-block",
        "runtime:file:{dir}/placeholder,ro",
    ]);
    let mut aggregate = CAPTURE.to_vec();
    aggregate.extend(["--mount-aggregate", "/run/nvx/shares"]);
    let mut identity = CAPTURE.to_vec();
    identity.extend(["--microvm-workload-identity", "1000:1000"]);
    let mut unbounded = CAPTURE.to_vec();
    let at = unbounded
        .iter()
        .position(|given| *given == "--memory-capacity")
        .unwrap();
    unbounded.drain(at..at + 2);
    let mut without_blocks = CAPTURE.to_vec();
    without_blocks.drain(2..6);
    let invalid_generation = "invalid --snapshot-generation-id";
    let capacity = "--memory-capacity must be a multiple of 128 MiB no smaller than --memory";
    let cases: Vec<(Vec<&str>, String)> = vec![
        (
            lifecycle,
            "a capture takes no --microvm-lifecycle".to_owned(),
        ),
        (
            runtime,
            "invalid --microvm-sandbox-block runtime:file:{dir}/placeholder,ro".to_owned(),
        ),
        (aggregate, "a capture takes no --mount-aggregate".to_owned()),
        (
            identity,
            "a capture takes no --microvm-workload-identity".to_owned(),
        ),
        (
            unbounded,
            "--memory-capacity must be given exactly once".to_owned(),
        ),
        (
            without_blocks,
            "a capture takes sandbox block devices".to_owned(),
        ),
        (
            capture_with("--memory-capacity", "2000M"),
            capacity.to_owned(),
        ),
        (
            capture_with("--memory-capacity", "256M"),
            capacity.to_owned(),
        ),
        (
            capture_with("--snapshot-tier", "process"),
            "--snapshot-tier must be platform, not process".to_owned(),
        ),
        (
            capture_with("--snapshot-block-identity", "path"),
            "--snapshot-block-identity must be generation, not path".to_owned(),
        ),
        (
            capture_with(
                "--snapshot-generation-id",
                "00000000000000000000000000000000",
            ),
            format!("{invalid_generation} 00000000000000000000000000000000"),
        ),
        (
            capture_with(
                "--snapshot-generation-id",
                "0123456789abcdef0123456789abcde",
            ),
            format!("{invalid_generation} 0123456789abcdef0123456789abcde"),
        ),
        (
            capture_with(
                "--snapshot-generation-id",
                "0123456789abcdef0123456789abcdeg",
            ),
            format!("{invalid_generation} 0123456789abcdef0123456789abcdeg"),
        ),
        (
            capture_with("--cmdline", "nvx_sandbox=1 nvx_hostname=tenant"),
            CAPTURE_COMMAND_LINE.to_owned(),
        ),
        // OpenVMM records a limit of the processors online at boot in its canonical form
        // alone, of a count that a microVM supports, and at most its processors.
        (
            capture_with("--cmdline", "nvx_sandbox=1 maxcpus=01"),
            CAPTURE_COMMAND_LINE.to_owned(),
        ),
        (
            capture_with("--cmdline", "nvx_sandbox=1 maxcpus=3"),
            CAPTURE_COMMAND_LINE.to_owned(),
        ),
        (
            capture_with("--cmdline", "nvx_sandbox=1 maxcpus=4"),
            CAPTURE_COMMAND_LINE.to_owned(),
        ),
        (
            capture_with("--cmdline", "nvx_sandbox=1 maxcpus=1 maxcpus=1"),
            CAPTURE_COMMAND_LINE.to_owned(),
        ),
        (
            capture_with("--snapshot-destination", "{dir}/transaction"),
            "the snapshot destination {dir}/transaction exists".to_owned(),
        ),
        (
            capture_with("--snapshot-destination", "{dir}/missing/snapshot"),
            "the directory of the snapshot destination {dir}/missing/snapshot does not exist"
                .to_owned(),
        ),
        (
            capture_with("--virtio-console", "stdio"),
            "--virtio-console must be none, not stdio".to_owned(),
        ),
        (
            capture_with("--microvm-control-console", "listen="),
            "--microvm-control-console must name its endpoint after listen=".to_owned(),
        ),
    ];
    for (arguments, message) in cases {
        let (temporary, options) = parse_capture(&arguments);
        let message = message.replace("{dir}", temporary.path().to_str().unwrap());
        assert_eq!(options.err(), Some(message), "{arguments:?}");
    }
}

#[test]
fn capture_scenarios_take_defaults_and_refuse_unknown_fields() {
    let scenario: Scenario = serde_json::from_str(r#"{"manifest": "manifest.bin"}"#).unwrap();
    assert_eq!(scenario.manifest, Path::new("manifest.bin"));
    assert_eq!(scenario.state_len, 0);
    assert_eq!(scenario.outcome, CaptureOutcome::Publish);
    assert_eq!(scenario.status, 1);
    assert_eq!(scenario.active, None);
    assert_eq!(scenario.delay_ms, 0);
    assert!(!scenario.keep_memory);
    let scenario: Scenario = serde_json::from_str(
        r#"{"manifest": "m", "stateLen": 512, "outcome": "publish-then-hang", "status": 3,
                "active": "a", "delayMs": 5, "keepMemory": true}"#,
    )
    .unwrap();
    assert_eq!(
        (
            scenario.state_len,
            scenario.outcome,
            scenario.status,
            scenario.delay_ms,
            scenario.keep_memory
        ),
        (512, CaptureOutcome::PublishThenHang, 3, 5, true)
    );
    assert_eq!(scenario.active.as_deref(), Some(Path::new("a")));
    for refused in [
        r#"{}"#,
        r#"{"manifest": "m", "outcome": "succeed"}"#,
        r#"{"manifest": "m", "memory": 1}"#,
    ] {
        assert!(
            serde_json::from_str::<Scenario>(refused).is_err(),
            "{refused}"
        );
    }
}
