//! The checks that `nvx-rs sandbox` and `nvx-rs run` make of their options and inputs before
//! anything changes: each refusal exits with status 1, names what it refuses, leaves the root as it
//! was, with no state directory, and launches no OpenVMM.

use crate::harness::{Content, Hosts, LAYER, Outcome, Root, SCRATCH, UUID, in_parallel};
use crate::lifecycle::provision;

/// An environment file of one entry.
const ENVIRONMENT: Content = Content::Text(r#"["A=1"]"#);
/// An egress policy file with an allowed range, an exclusion, a port, and a denied protocol.
const POLICY: Content = Content::Text(
    r#"{"allow": [{"cidr": "192.0.2.0/24", "except": ["192.0.2.128/25"]}, {"cidr": "2001:db8::/32", "protocol": "tcp", "port": 443}], "deny": [{"cidr": "198.51.100.0/24", "protocol": "udp"}]}"#,
);
/// An egress policy file whose rule names a port without a protocol.
const INVALID_POLICY: Content = Content::Text(r#"{"allow": [{"cidr": "10.0.0.0/8", "port": 0}]}"#);
/// The distro manifest of the distro layer of a root, beside it.
const MANIFEST: &str = "distro.erofs.manifest.json";
/// A manifest of the distro layer of a root that lists systemd.
const SYSTEMD_MANIFEST: Content = Content::Text(
    r#"{"format": 1, "artifact": "distro.erofs", "artifact_sha256": "8a475efd5fd711d481a9a19de6d12b88dff0cd81cb8135ffcd5b65b062c7bf04", "packages": [{"name": "busybox", "version": "1"}, {"name": "systemd", "version": "1"}]}"#,
);
/// A manifest of another layer than the distro layer of a root, whose digest it names.
const MISMATCHED_MANIFEST: Content = Content::Text(
    r#"{"format": 1, "artifact": "distro.erofs", "artifact_sha256": "0000000000000000000000000000000000000000000000000000000000000000", "packages": []}"#,
);
/// A manifest of the distro layer of a root without its packages.
const PACKAGELESS_MANIFEST: Content = Content::Text(
    r#"{"format": 1, "artifact": "distro.erofs", "artifact_sha256": "8a475efd5fd711d481a9a19de6d12b88dff0cd81cb8135ffcd5b65b062c7bf04"}"#,
);
/// A guest initramfs, which a case adds to the root.
const INITRAMFS: Content = Content::Text("initramfs\n");

/// A command line that fails a check before it changes anything.
struct Refusal {
    /// The name of the case.
    name: &'static str,
    /// The hosts that refuse it.
    hosts: Hosts,
    /// The files that the case writes below the root first, with their contents.
    files: Vec<(&'static str, Content)>,
    /// The files that the case removes from the root first.
    removed: Vec<&'static str>,
    /// The environment variables that the case sets, with placeholders.
    environment: Vec<(&'static str, &'static str)>,
    /// The arguments after `--root ROOT`, with placeholders.
    arguments: Vec<String>,
    /// The code of the error.
    code: &'static str,
    /// What the error names, with placeholders.
    fragments: Vec<&'static str>,
}

/// Returns the refusal `name` of `arguments`, whose error has the code `code` and names each of
/// `fragments`.
fn refusal<S: ToString>(
    name: &'static str,
    code: &'static str,
    arguments: impl IntoIterator<Item = S>,
    fragments: &[&'static str],
) -> Refusal {
    Refusal {
        name,
        code,
        hosts: Hosts::Every,
        files: Vec::new(),
        removed: Vec::new(),
        environment: Vec::new(),
        arguments: arguments
            .into_iter()
            .map(|argument| argument.to_string())
            .collect(),
        fragments: fragments.to_vec(),
    }
}

impl Refusal {
    /// Returns the case with the file `relative` written first, with `content`.
    fn with_file(mut self, relative: &'static str, content: Content) -> Self {
        self.files.push((relative, content));
        self
    }

    /// Returns the case with the file `relative` removed first.
    fn without(mut self, relative: &'static str) -> Self {
        self.removed.push(relative);
        self
    }

    /// Returns the case with the variable `name` set to `value`.
    fn with_variable(mut self, name: &'static str, value: &'static str) -> Self {
        self.environment.push((name, value));
        self
    }

    /// Returns the case for `hosts` alone.
    fn on(mut self, hosts: Hosts) -> Self {
        self.hosts = hosts;
        self
    }
}

/// Returns `nvx-rs sandbox run` of the distro layer and scratch image of the root with `options`.
fn sandbox_run(options: &[&str]) -> Vec<String> {
    ["sandbox", "run", "--layer", LAYER, "--scratch", SCRATCH]
        .iter()
        .chain(options)
        .map(|argument| (*argument).to_owned())
        .collect()
}

/// Returns `nvx-rs run OPTIONS --dry-run`.
fn run_dry(options: &[&str]) -> Vec<String> {
    ["run"]
        .iter()
        .chain(options)
        .chain(&["--dry-run"])
        .map(|argument| (*argument).to_owned())
        .collect()
}

/// Returns the `--mount` options of `count` shares of the root's `share` directory at
/// `/share/00` onwards.
fn shares(count: usize) -> Vec<String> {
    (0..count)
        .flat_map(|index| {
            [
                "--mount".to_owned(),
                format!("/share/{index:02},{{root/share}}"),
            ]
        })
        .collect()
}

/// Returns the refusals of `nvx-rs sandbox`.
fn sandbox_refusals() -> Vec<Refusal> {
    let network_outside =
        &["network policy options are only valid for sandbox run or provision"][..];
    let exec_options = &["managed execution options", "sandbox exec"][..];
    vec![
        refusal(
            "dry run outside run",
            "unsupported",
            ["sandbox", "provision", "--dry-run"],
            &["--dry-run", "sandbox run"],
        ),
        refusal(
            "environment and environment file",
            "unsupported",
            [
                "sandbox",
                "exec",
                "--environment",
                "A=1",
                "--environment-file",
                "{root/env.json}",
            ],
            &["--environment", "--environment-file"],
        )
        .with_file("env.json", ENVIRONMENT),
        refusal(
            "cwd outside exec",
            "unsupported",
            ["sandbox", "run", "--cwd", "/"],
            exec_options,
        ),
        refusal(
            "environment outside exec",
            "unsupported",
            ["sandbox", "start", "--environment", "A=1"],
            exec_options,
        ),
        refusal(
            "environment file outside exec",
            "unsupported",
            [
                "sandbox",
                "provision",
                "--environment-file",
                "{root/env.json}",
            ],
            exec_options,
        )
        .with_file("env.json", ENVIRONMENT),
        refusal(
            "inherited environment outside exec",
            "unsupported",
            ["sandbox", "stop", "--inherit-default-environment"],
            exec_options,
        ),
        refusal(
            "environment file too large",
            "unsupported",
            ["sandbox", "exec", "--environment-file", "{root/env.json}"],
            &["environment file", "1048576"],
        )
        .with_file("env.json", Content::Padded("", 1_048_577)),
        // A file at the limit is read, and the exec then fails for its missing state directory.
        refusal(
            "environment file at the limit",
            "unsupported",
            ["sandbox", "exec", "--environment-file", "{root/env.json}"],
            &["--state-dir"],
        )
        .with_file("env.json", Content::Padded(r#"["A=1"]"#, 1_048_576)),
        refusal(
            "environment file missing",
            "unsupported",
            [
                "sandbox",
                "exec",
                "--environment-file",
                "{root/missing.json}",
            ],
            &["environment", "{root/missing.json:?}"],
        ),
        refusal(
            "environment file directory",
            "unsupported",
            ["sandbox", "exec", "--environment-file", "{root/share}"],
            &["environment", "{root/share:?}"],
        ),
        refusal(
            "environment file not UTF-8",
            "unsupported",
            ["sandbox", "exec", "--environment-file", "{root/env.json}"],
            &["environment", "{root/env.json:?}"],
        )
        .with_file("env.json", Content::Bytes(b"[\"\xff\"]")),
        refusal(
            "environment file not JSON",
            "unsupported",
            ["sandbox", "exec", "--environment-file", "{root/env.json}"],
            &["environment", "{root/env.json:?}"],
        )
        .with_file("env.json", Content::Text(r#"["A=1""#)),
        refusal(
            "environment file object",
            "unsupported",
            ["sandbox", "exec", "--environment-file", "{root/env.json}"],
            &["environment", "{root/env.json:?}", "expected a sequence"],
        )
        .with_file("env.json", Content::Text(r#"{"A": "1"}"#)),
        refusal(
            "environment file non-string",
            "unsupported",
            ["sandbox", "exec", "--environment-file", "{root/env.json}"],
            &["environment", "{root/env.json:?}", "expected a string"],
        )
        .with_file("env.json", Content::Text(r#"["A=1", 2]"#)),
        refusal(
            "environment file entry without equals",
            "unsupported",
            ["sandbox", "exec", "--environment-file", "{root/env.json}"],
            &["KEY=VALUE"],
        )
        .with_file("env.json", Content::Text(r#"["A"]"#)),
        refusal(
            "environment file entry with NUL",
            "unsupported",
            ["sandbox", "exec", "--environment-file", "{root/env.json}"],
            &["KEY=VALUE"],
        )
        .with_file("env.json", Content::Text(r#"["A=\u0000"]"#)),
        refusal(
            "environment file duplicate entry",
            "unsupported",
            ["sandbox", "exec", "--environment-file", "{root/env.json}"],
            &["duplicate key: A"],
        )
        .with_file("env.json", Content::Text(r#"["A=1", "A=2"]"#)),
        refusal(
            "environment entry without name",
            "unsupported",
            ["sandbox", "exec", "--environment", "=x"],
            &["KEY=VALUE"],
        ),
        // A name with a space is an entry all the same.
        refusal(
            "environment entry with space",
            "unsupported",
            ["sandbox", "exec", "--environment", "A B=x"],
            &["--state-dir"],
        ),
        refusal(
            "network outside run",
            "unsupported",
            ["sandbox", "start", "--net", "10.0.0.2/24"],
            network_outside,
        ),
        refusal(
            "egress rule outside run",
            "unsupported",
            ["sandbox", "exec", "--network-egress-allow", "10.0.0.0/8"],
            network_outside,
        ),
        refusal(
            "forward outside run",
            "unsupported",
            ["sandbox", "stop", "--host-loopback-forward", "80"],
            network_outside,
        ),
        refusal(
            "policy file outside run",
            "unsupported",
            [
                "sandbox",
                "deprovision",
                "--network-egress-policy-file",
                "{root/policy.json}",
            ],
            network_outside,
        ),
        refusal(
            "systemd entrypoint of run",
            "unsupported",
            sandbox_run(&["--entrypoint", "/usr/lib/systemd/systemd"]),
            &["systemd entrypoints"],
        ),
        refusal(
            "systemd entrypoint of exec",
            "unsupported",
            ["sandbox", "exec", "--entrypoint", "/lib/systemd/systemd"],
            &["systemd entrypoints"],
        ),
        refusal(
            "outcome report outside run",
            "unsupported",
            [
                "sandbox",
                "provision",
                "--outcome-report",
                "{root/reports/exec.json}",
            ],
            &["--outcome-report", "run", "exec"],
        ),
        refusal(
            "deny without mount",
            "unsupported",
            ["sandbox", "run", "--mount-deny", "{root/share/hidden}"],
            &["--mount-deny requires --mount"],
        ),
        refusal(
            "allow without mount",
            "unsupported",
            ["sandbox", "run", "--mount-allow", "{root/share/hidden}"],
            &["--mount-allow requires --mount"],
        ),
        refusal(
            "write without mount",
            "unsupported",
            ["sandbox", "run", "--mount-write", "{root/share/writable}"],
            &["--mount-write requires --mount"],
        ),
        refusal(
            "owner without mount",
            "unsupported",
            ["sandbox", "run", "--mount-owner", "vmm"],
            &["--mount-owner requires --mount"],
        ),
        refusal(
            "mount outside run",
            "unsupported",
            ["sandbox", "start", "--mount", "/data,{root/share}"],
            &["--mount", "run or provision"],
        ),
        refusal(
            "net without profile",
            "unsupported",
            sandbox_run(&["--net", "10.0.0.2/24"]),
            &["--net", "--network-profile"],
        ),
        refusal(
            "profile without net",
            "unsupported",
            sandbox_run(&["--network-profile", "portable"]),
            &["--net", "--network-profile"],
        ),
        refusal(
            "run without layers",
            "unsupported",
            ["sandbox", "run"],
            &["sandbox run", "--layer", "--scratch"],
        ),
        refusal(
            "provision without scratch",
            "unsupported",
            ["sandbox", "provision", "--layer", LAYER],
            &["sandbox provision", "--layer", "--scratch"],
        ),
        refusal(
            "run without layer",
            "unsupported",
            ["sandbox", "run", "--scratch", SCRATCH],
            &["sandbox run", "--layer", "--scratch"],
        ),
        refusal(
            "explicit scratch upper without scratch",
            "unsupported",
            ["sandbox", "run", "--layer", LAYER, "--upper", "scratch"],
            &["sandbox run", "--layer", "--scratch"],
        ),
        // An upper in RAM has no scratch image, and needs the distro layer alone.
        refusal(
            "RAM upper with scratch",
            "unsupported",
            sandbox_run(&["--upper", "ram"]),
            &["--upper ram", "--scratch", "mutually exclusive"],
        ),
        refusal(
            "RAM upper without layer",
            "unsupported",
            ["sandbox", "provision", "--upper", "ram"],
            &["sandbox provision requires --layer"],
        ),
        refusal(
            "upper outside run",
            "unsupported",
            ["sandbox", "start", "--upper", "ram"],
            &["--upper", "run or provision"],
        ),
        refusal(
            "policy file with rules",
            "unsupported",
            sandbox_run(&[
                "--network-egress",
                "allow",
                "--network-egress-policy-file",
                "{root/policy.json}",
                "--network-egress-deny",
                "10.0.0.0/8",
            ]),
            &["--network-egress-policy-file", "--network-egress-deny"],
        ),
        refusal(
            "policy file without egress",
            "unsupported",
            sandbox_run(&["--network-egress-policy-file", "{root/policy.json}"]),
            &[
                "--network-egress is required",
                "--network-egress-policy-file",
            ],
        ),
        refusal(
            "policy file missing",
            "io",
            sandbox_run(&[
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                "{root/missing.json}",
            ]),
            &["egress policy file", "{root/missing.json}"],
        ),
        refusal(
            "policy file invalid",
            "unsupported",
            sandbox_run(&[
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                "{root/policy.json}",
            ]),
            &["allow[0].port"],
        )
        .with_file("policy.json", INVALID_POLICY),
        refusal(
            "provision without state directory",
            "unsupported",
            [
                "sandbox",
                "provision",
                "--layer",
                LAYER,
                "--scratch",
                SCRATCH,
            ],
            &["sandbox provision requires --state-dir"],
        ),
        refusal(
            "start without state directory",
            "unsupported",
            ["sandbox", "start"],
            &["sandbox start requires --state-dir"],
        ),
        refusal(
            "exec without state directory",
            "unsupported",
            ["sandbox", "exec"],
            &["sandbox exec requires --state-dir"],
        ),
        refusal(
            "stop without state directory",
            "unsupported",
            ["sandbox", "stop"],
            &["sandbox stop requires --state-dir"],
        ),
        refusal(
            "deprovision without state directory",
            "unsupported",
            ["sandbox", "deprovision"],
            &["sandbox deprovision requires --state-dir"],
        ),
        refusal(
            "run with state directory",
            "unsupported",
            sandbox_run(&["--state-dir", "{root/state}"]),
            &["--state-dir"],
        ),
        refusal(
            "missing layer",
            "unavailable",
            [
                "sandbox",
                "run",
                "--layer",
                "distro,{root/missing.erofs},11111111-1111-1111-1111-111111111111",
                "--scratch",
                SCRATCH,
            ],
            &["distro", "{root/missing.erofs}"],
        ),
        refusal(
            "duplicate layer",
            "unsupported",
            [
                "sandbox",
                "run",
                "--layer",
                LAYER,
                "--layer",
                LAYER,
                "--scratch",
                SCRATCH,
            ],
            &["duplicate", "distro"],
        ),
        refusal(
            "missing scratch",
            "unavailable",
            [
                "sandbox",
                "run",
                "--layer",
                LAYER,
                "--scratch",
                "{root/missing.ext4}",
            ],
            &["scratch", "{root/missing.ext4}"],
        ),
        refusal(
            "relative entrypoint",
            "unsupported",
            sandbox_run(&["--entrypoint", "bin/sh"]),
            &["entrypoint", "absolute"],
        ),
        refusal(
            "invalid hostname",
            "unsupported",
            sandbox_run(&["--hostname", "bad host"]),
            &["hostname", "RFC 1123"],
        ),
        refusal(
            "argument with space",
            "unsupported",
            sandbox_run(&["--arg", "a b"]),
            &["arguments", "whitespace"],
        ),
        refusal(
            "relative mount target",
            "unsupported",
            sandbox_run(&["--mount", "data,{root/share}"]),
            &["mount target", "\"data\""],
        ),
        // A `"` in a target that reached the kernel command line would hide the virtio-mmio
        // devices that OpenVMM appends after it from the guest's kernel.
        refusal(
            "quoted mount target of an aggregate",
            "unsupported",
            sandbox_run(&[
                "--mount",
                "/data\"x,{root/share}",
                "--mount",
                "/logs,{root/share2}",
            ]),
            &["mount target", "\"/data\\\"x\"", "quotes"],
        ),
        refusal(
            "quoted mount target of one share",
            "unsupported",
            sandbox_run(&["--mount", "/data\"x,{root/share}"]),
            &["mount target", "\"/data\\\"x\"", "quotes"],
        ),
        refusal(
            "missing mount host",
            "unsupported",
            sandbox_run(&["--mount", "/data,{root/missing}"]),
            &["mount host path", "{root/missing}"],
        ),
        refusal(
            "invalid mount mode",
            "unsupported",
            sandbox_run(&["--mount", "/data,{root/share},rx"]),
            &["--mount mode", "\"rx\""],
        ),
        refusal(
            "deny outside share",
            "unsupported",
            sandbox_run(&[
                "--mount",
                "/data,{root/share}",
                "--mount-deny",
                "{root/share2}",
            ]),
            &["--mount-deny {root/share2}", "--mount /data"],
        ),
        refusal(
            "allow outside denied path",
            "unsupported",
            sandbox_run(&[
                "--mount",
                "/data,{root/share}",
                "--mount-allow",
                "{root/share/hidden/shown}",
            ]),
            &["--mount-allow", "--mount-deny"],
        ),
        refusal(
            "write in read-only share",
            "unsupported",
            sandbox_run(&[
                "--mount",
                "/data,{root/share},ro",
                "--mount-write",
                "{root/share/writable}",
            ]),
            &["--mount-write", "--mount /data", "read-only"],
        ),
        refusal(
            "reserved command-line token",
            "unsupported",
            sandbox_run(&["--cmdline", "nvx_sandbox=0"]),
            &["nvx_sandbox"],
        ),
        refusal(
            "systemd distro image",
            "unsupported",
            sandbox_run(&[]),
            &["systemd images"],
        )
        .with_file(MANIFEST, SYSTEMD_MANIFEST),
        refusal(
            "mismatched distro manifest",
            "unsupported",
            sandbox_run(&[]),
            &["distro manifest", "artifact"],
        )
        .with_file(MANIFEST, MISMATCHED_MANIFEST),
        refusal(
            "invalid distro manifest",
            "unsupported",
            sandbox_run(&[]),
            &["distro manifest"],
        )
        .with_file(MANIFEST, Content::Text("{")),
        refusal(
            "distro manifest without packages",
            "unsupported",
            sandbox_run(&[]),
            &["distro manifest", "package"],
        )
        .with_file(MANIFEST, PACKAGELESS_MANIFEST),
        refusal(
            "caller-owned share on Windows",
            "unsupported",
            sandbox_run(&["--mount", "/data,{root/share}", "--mount-owner", "caller"]),
            &["--mount-owner caller", "Linux"],
        )
        .on(Hosts::Windows),
        // Caller ownership squashes guest root to the owner of a share, so it refuses one that
        // root owns, as `/` is whoever runs the tests.
        refusal(
            "caller owner of a root-owned share",
            "unsupported",
            sandbox_run(&["--mount", "/data,/", "--mount-owner", "caller"]),
            &["--mount-owner caller", "UID 0 or GID 0"],
        )
        .on(Hosts::Posix),
        refusal(
            "caller owner of a root-owned share at provision",
            "unsupported",
            provision(
                "{root/state}",
                &["--mount", "/data,/", "--mount-owner", "caller"],
            ),
            &["--mount-owner caller", "UID 0 or GID 0"],
        )
        .on(Hosts::Posix),
        refusal(
            "network options without --net",
            "unsupported",
            sandbox_run(&["--host-loopback-forward", "8080"]),
            &["network policy options require --net"],
        ),
        refusal(
            "missing OpenVMM",
            "artifact-missing",
            sandbox_run(&[]),
            &["OpenVMM", "{root/openvmm/target/release/openvmm{exe}}"],
        )
        .without(crate::harness::OPENVMM),
        refusal(
            "missing kernel",
            "artifact-missing",
            sandbox_run(&[]),
            &["kernel", "{root/build/vmlinux}"],
        )
        .without("build/vmlinux"),
        refusal(
            "missing initramfs",
            "artifact-missing",
            sandbox_run(&[]),
            &["initramfs", "{root/build/initramfs.cpio.gz}"],
        )
        .without("build/initramfs.cpio.gz"),
    ]
}

/// Returns the refusals of `nvx-rs run`.
fn run_refusals() -> Vec<Refusal> {
    let targets = "with several shares, or a shared file";
    let budget = &["--mount", "--cmdline", "1024"][..];
    let two_shares = |cmdline: &str| {
        run_dry(&[
            "--mount",
            "/a,{root/share}",
            "--mount",
            "/b,{root/share2},rw",
            "--cmdline",
            cmdline,
        ])
    };
    let mut many = vec!["run".to_owned()];
    many.extend(shares(20));
    many.push("--dry-run".to_owned());
    vec![
        refusal(
            "run: net without profile",
            "unsupported",
            run_dry(&["--net", "10.0.0.2/24"]),
            &["--net", "--network-profile"],
        ),
        refusal(
            "run: profile without net",
            "unsupported",
            run_dry(&["--network-profile", "portable"]),
            &["--net", "--network-profile"],
        ),
        // A restored NIC keeps the addressing that its snapshot saved.
        refusal(
            "run: net of a restore",
            "unsupported",
            run_dry(&[
                "--restore-snapshot",
                "{root/snapshot}",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
            ]),
            &["--net", "restore", "network addressing"],
        ),
        refusal(
            "run: net without profile of a restore",
            "unsupported",
            run_dry(&[
                "--restore-snapshot",
                "{root/snapshot}",
                "--net",
                "10.0.0.2/24",
            ]),
            &["--net", "restore", "network addressing"],
        ),
        refusal(
            "run: ready path without snapshot",
            "unsupported",
            run_dry(&["--restore-ready-path", "{root/ready.sock}"]),
            &["--restore-ready-path requires --restore-snapshot"],
        ),
        refusal(
            "run: restore processors without snapshot",
            "unsupported",
            run_dry(&["--restore-processors", "1"]),
            &["--restore-processors requires --restore-snapshot"],
        ),
        refusal(
            "run: restore memory without snapshot",
            "unsupported",
            run_dry(&["--restore-memory-mib", "1024"]),
            &["--restore-memory-mib requires --restore-snapshot"],
        ),
        refusal(
            "run: memory capacity of a restore",
            "unsupported",
            run_dry(&[
                "--restore-snapshot",
                "{root/snapshot}",
                "--memory-capacity-mib",
                "1024",
            ]),
            &["--memory-capacity-mib", "fresh boot"],
        ),
        refusal(
            "run: Ubuntu guest of a restore",
            "unsupported",
            run_dry(&["--guest", "ubuntu", "--restore-snapshot", "{root/snapshot}"]),
            &["--guest", "restore"],
        ),
        refusal(
            "run: Azure Linux guest of a restore",
            "unsupported",
            run_dry(&[
                "--restore-snapshot",
                "{root/snapshot}",
                "--guest",
                "azurelinux",
            ]),
            &["--guest", "restore"],
        ),
        refusal(
            "run: memory capacity below memory",
            "unsupported",
            run_dry(&["--memory-mib", "512", "--memory-capacity-mib", "256"]),
            &["--memory-capacity-mib", "--memory-mib"],
        ),
        // The capacity is checked against the memory that the guest boots with by default.
        refusal(
            "run: memory capacity below the guest default",
            "unsupported",
            run_dry(&["--guest", "ubuntu", "--memory-capacity-mib", "256"]),
            &["--memory-capacity-mib", "--memory-mib"],
        )
        .with_file("build/initramfs-ubuntu.cpio.gz", INITRAMFS),
        refusal(
            "run: restore processors above processors",
            "unsupported",
            run_dry(&[
                "--processors",
                "2",
                "--restore-snapshot",
                "{root/snapshot}",
                "--restore-processors",
                "4",
            ]),
            &["--restore-processors", "--processors"],
        ),
        refusal(
            "run: policy file with allow rules",
            "unsupported",
            run_dry(&[
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                "{root/policy.json}",
                "--network-egress-allow",
                "10.0.0.0/8",
            ]),
            &["--network-egress-policy-file", "--network-egress-allow"],
        )
        .with_file("policy.json", POLICY),
        refusal(
            "run: policy file with deny rules",
            "unsupported",
            run_dry(&[
                "--network-egress-deny",
                "10.0.0.0/8",
                "--network-egress-policy-file",
                "{root/policy.json}",
            ]),
            &["--network-egress-policy-file", "--network-egress-deny"],
        )
        .with_file("policy.json", POLICY),
        refusal(
            "run: policy file without egress",
            "unsupported",
            run_dry(&["--network-egress-policy-file", "{root/policy.json}"]),
            &[
                "--network-egress is required",
                "--network-egress-policy-file",
            ],
        )
        .with_file("policy.json", POLICY),
        // The refusal names the file as it was given, without normalizing its path.
        refusal(
            "run: policy file missing",
            "io",
            run_dry(&[
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                "{root/./missing.json}",
            ]),
            &["egress policy file", "{root/./missing.json}"],
        ),
        refusal(
            "run: policy file directory",
            "io",
            run_dry(&[
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                "{root/share}",
            ]),
            &["egress policy file", "{root/share}"],
        ),
        refusal(
            "run: policy file invalid",
            "unsupported",
            run_dry(&[
                "--network-egress",
                "allow",
                "--network-egress-policy-file",
                "policy.json",
            ]),
            &["allow[0].port"],
        )
        .with_file("policy.json", INVALID_POLICY),
        refusal(
            "run: policy file not JSON",
            "unsupported",
            run_dry(&[
                "--network-egress",
                "allow",
                "--network-egress-policy-file",
                "policy.json",
            ]),
            &["egress policy file", "policy.json"],
        )
        .with_file("policy.json", Content::Text("{")),
        refusal(
            "run: policy errors before missing artifacts",
            "unsupported",
            run_dry(&["--network-egress-policy-file", "{root/policy.json}"]),
            &[
                "--network-egress is required",
                "--network-egress-policy-file",
            ],
        )
        .with_file("policy.json", POLICY)
        .without(crate::harness::OPENVMM)
        .without("build/vmlinux"),
        refusal(
            "run: missing OpenVMM",
            "artifact-missing",
            run_dry(&[]),
            &["OpenVMM", "{root/openvmm/target/release/openvmm{exe}}"],
        )
        .without(crate::harness::OPENVMM),
        refusal(
            "run: missing kernel",
            "artifact-missing",
            run_dry(&[]),
            &["kernel", "{root/build/vmlinux}"],
        )
        .without("build/vmlinux"),
        refusal(
            "run: missing Alpine initramfs",
            "artifact-missing",
            run_dry(&[]),
            &["Alpine Linux initramfs", "{root/build/initramfs.cpio.gz}"],
        )
        .without("build/initramfs.cpio.gz"),
        refusal(
            "run: missing Ubuntu initramfs",
            "artifact-missing",
            run_dry(&["--guest", "ubuntu"]),
            &[
                "Ubuntu Base initramfs",
                "{root/build/initramfs-ubuntu.cpio.gz}",
            ],
        ),
        refusal(
            "run: missing Azure Linux initramfs",
            "artifact-missing",
            run_dry(&["--guest", "azurelinux"]),
            &[
                "Azure Linux initramfs",
                "{root/build/initramfs-azurelinux.cpio.gz}",
            ],
        ),
        refusal(
            "run: missing artifacts before invalid shares",
            "artifact-missing",
            run_dry(&["--mount", "/data"]),
            &["OpenVMM", "{root/openvmm/target/release/openvmm{exe}}"],
        )
        .without(crate::harness::OPENVMM),
        refusal(
            "run: mount without host path",
            "unsupported",
            run_dry(&["--mount", "/data"]),
            &["--mount", "GUEST_TARGET,HOST_PATH"],
        ),
        refusal(
            "run: mount with empty host path",
            "unsupported",
            run_dry(&["--mount", "/data,"]),
            &["--mount host path", "empty"],
        ),
        refusal(
            "run: mount with empty host path and mode",
            "unsupported",
            run_dry(&["--mount", "/data,,rw"]),
            &["--mount host path", "empty"],
        ),
        refusal(
            "run: mount with invalid mode",
            "unsupported",
            run_dry(&["--mount", "/data,{root/share},rx"]),
            &["--mount mode", "\"rx\""],
        ),
        refusal(
            "run: mount with commas but no mode",
            "unsupported",
            run_dry(&["--mount", "/data,{root/alpha,beta}"]),
            &["--mount mode", "\"beta\""],
        )
        .with_file("alpha,beta/data.txt", Content::Text("data\n")),
        refusal(
            "run: invalid second mount",
            "unsupported",
            run_dry(&["--mount", "/a,{root/share}", "--mount", "relative"]),
            &["--mount", "GUEST_TARGET,HOST_PATH"],
        ),
        refusal(
            "run: invalid mount before invalid targets",
            "unsupported",
            run_dry(&["--mount", "a,{root/share}", "--mount", "/b"]),
            &["--mount", "GUEST_TARGET,HOST_PATH"],
        ),
        refusal(
            "run: owner without mount",
            "unsupported",
            run_dry(&["--mount-owner", "vmm"]),
            &["--mount-owner requires --mount"],
        ),
        refusal(
            "run: relative deny path with several shares",
            "unsupported",
            run_dry(&[
                "--mount",
                "/a,{root/share}",
                "--mount",
                "/b,{root/share2}",
                "--mount-deny",
                "secrets",
            ]),
            &["--mount-deny secrets", "absolute host path"],
        ),
        refusal(
            "run: relative allow path with a shared file",
            "unsupported",
            run_dry(&[
                "--mount",
                "/etc/single.txt,{root/single.txt}",
                "--mount-allow",
                "secrets",
            ]),
            &["--mount-allow secrets", "absolute host path"],
        ),
        refusal(
            "run: relative write path with several shares",
            "unsupported",
            run_dry(&[
                "--mount",
                "/a,{root/share}",
                "--mount",
                "/b,{root/share2}",
                "--mount-write",
                "./out/",
            ]),
            &[
                if cfg!(windows) {
                    r"--mount-write .\out"
                } else {
                    "--mount-write ./out"
                },
                "absolute host path",
            ],
        ),
        // Deny paths are checked before write paths, whatever their order.
        refusal(
            "run: relative paths in kind order",
            "unsupported",
            run_dry(&[
                "--mount",
                "/a,{root/share}",
                "--mount",
                "/b,{root/share2}",
                "--mount-write",
                "out",
                "--mount-deny",
                "secrets",
            ]),
            &["--mount-deny secrets", "absolute host path"],
        ),
        refusal(
            "run: relative target with several shares",
            "unsupported",
            run_dry(&["--mount", "a,{root/share}", "--mount", "/b,{root/share2}"]),
            &["--mount target", "\"a\"", targets],
        ),
        refusal(
            "run: relative target of a shared file",
            "unsupported",
            run_dry(&["--mount", "single.txt,{root/single.txt}"]),
            &["--mount target", "\"single.txt\"", targets],
        ),
        refusal(
            "run: root target with several shares",
            "unsupported",
            run_dry(&["--mount", "/,{root/share}", "--mount", "/b,{root/share2}"]),
            &["--mount target", "\"/\"", targets],
        ),
        refusal(
            "run: target with a space",
            "unsupported",
            run_dry(&[
                "--mount",
                "/a b,{root/share}",
                "--mount",
                "/b,{root/share2}",
            ]),
            &["--mount target", "\"/a b\""],
        ),
        refusal(
            "run: target with an equals sign",
            "unsupported",
            run_dry(&[
                "--mount",
                "/a=b,{root/share}",
                "--mount",
                "/b,{root/share2}",
            ]),
            &["--mount target", "\"/a=b\""],
        ),
        refusal(
            "run: target with a backslash",
            "unsupported",
            run_dry(&[
                "--mount",
                "/a\\b,{root/share}",
                "--mount",
                "/b,{root/share2}",
            ]),
            &["--mount target", "\"/a\\\\b\""],
        ),
        refusal(
            "run: target with a quote",
            "unsupported",
            run_dry(&[
                "--mount",
                "/a\"b,{root/share}",
                "--mount",
                "/b,{root/share2}",
            ]),
            &["--mount target", "\"/a\\\"b\""],
        ),
        // U+00E0 is C3 A0 in UTF-8, and Linux reads 0xa0 as whitespace, so it would split the
        // target's nvx_share= token.
        refusal(
            "run: target with a byte that Linux reads as whitespace",
            "unsupported",
            run_dry(&[
                "--mount",
                "/voil\u{e0},{root/share}",
                "--mount",
                "/b,{root/share2}",
            ]),
            &["--mount target", "\"/voil\u{e0}\"", targets],
        ),
        // The target of one directory share reaches the kernel command line in OpenVMM's
        // virtfs_dir= token.
        refusal(
            "run: quoted target of one share",
            "unsupported",
            run_dry(&["--mount", "/a\"b,{root/share}"]),
            &["--mount target", "\"/a\\\"b\"", "one kernel token"],
        ),
        refusal(
            "run: target with a dot component",
            "unsupported",
            run_dry(&[
                "--mount",
                "/a/./b,{root/share}",
                "--mount",
                "/b,{root/share2}",
            ]),
            &["--mount target", "\"/a/./b\""],
        ),
        refusal(
            "run: target with a parent component",
            "unsupported",
            run_dry(&[
                "--mount",
                "/a/../b,{root/share}",
                "--mount",
                "/c,{root/share2}",
            ]),
            &["--mount target", "\"/a/../b\""],
        ),
        refusal(
            "run: target with an empty component",
            "unsupported",
            run_dry(&[
                "--mount",
                "/a//b,{root/share}",
                "--mount",
                "/c,{root/share2}",
            ]),
            &["--mount target", "\"/a//b\""],
        ),
        refusal(
            "run: equal targets",
            "unsupported",
            run_dry(&["--mount", "/a,{root/share}", "--mount", "/a,{root/share2}"]),
            &["--mount targets /a and /a overlap"],
        ),
        refusal(
            "run: nested targets",
            "unsupported",
            run_dry(&[
                "--mount",
                "/workspace,{root/share}",
                "--mount",
                "/workspace/cache,{root/share2}",
            ]),
            &["--mount targets /workspace and /workspace/cache overlap"],
        ),
        refusal(
            "run: target containing another",
            "unsupported",
            run_dry(&[
                "--mount",
                "/workspace/cache,{root/share}",
                "--mount",
                "/workspace,{root/share2}",
            ]),
            &["--mount targets /workspace/cache and /workspace overlap"],
        ),
        // The aggregate of the shares is at /run/nvx/shares.
        refusal(
            "run: target containing the aggregate",
            "unsupported",
            run_dry(&[
                "--mount",
                "/run/nvx,{root/share}",
                "--mount",
                "/b,{root/share2}",
            ]),
            &["--mount targets /run/nvx/shares and /run/nvx overlap"],
        ),
        refusal(
            "run: target in the aggregate",
            "unsupported",
            run_dry(&[
                "--mount",
                "/run/nvx/shares/x,{root/share}",
                "--mount",
                "/b,{root/share2}",
            ]),
            &["--mount targets /run/nvx/shares and /run/nvx/shares/x overlap"],
        ),
        refusal(
            "run: too many shares for the budget",
            "unsupported",
            many,
            budget,
        ),
        refusal(
            "run: budget exceeded by a byte",
            "unsupported",
            two_shares(&"x".repeat(842)),
            budget,
        ),
        // The budget is of bytes: each of these characters takes two.
        refusal(
            "run: budget in UTF-8 bytes",
            "unsupported",
            two_shares(&"\u{e9}".repeat(421)),
            budget,
        ),
        refusal(
            "run: missing OpenVMM override",
            "artifact-missing",
            run_dry(&[]),
            &["OpenVMM", "{root/alt/openvmm{exe}}"],
        )
        .with_variable("NVX_OPENVMM", "{root/alt/openvmm{exe}}"),
        refusal(
            "run: caller owner on Windows",
            "unsupported",
            run_dry(&["--mount", "/data,{root/share}", "--mount-owner", "caller"]),
            &["--mount-owner caller", "Linux"],
        )
        .on(Hosts::Windows),
        refusal(
            "run: missing OpenVMM at launch",
            "artifact-missing",
            ["run"],
            &["OpenVMM", "{root/openvmm/target/release/openvmm{exe}}"],
        )
        .without(crate::harness::OPENVMM),
        refusal(
            "run: missing kernel at launch",
            "artifact-missing",
            ["run"],
            &["kernel", "{root/build/vmlinux}"],
        )
        .without("build/vmlinux"),
        refusal(
            "run: missing initramfs at launch",
            "artifact-missing",
            ["run"],
            &["Alpine Linux initramfs", "{root/build/initramfs.cpio.gz}"],
        )
        .without("build/initramfs.cpio.gz"),
        refusal(
            "run: missing Ubuntu initramfs at launch",
            "artifact-missing",
            ["run", "--guest", "ubuntu"],
            &[
                "Ubuntu Base initramfs",
                "{root/build/initramfs-ubuntu.cpio.gz}",
            ],
        ),
    ]
}

#[test]
fn unbalanced_disk_paths_fail_before_launch_or_provision() {
    let root = Root::new();
    for directory in ["images[1", "images]1", "images][1"] {
        let relative = format!("{directory}/image.img");
        root.write(&relative, b"image\n");
        let before = root.snapshot();
        let resolved = root.below(&relative).to_str().unwrap().to_owned();
        for distro in [true, false] {
            for path in [resolved.as_str(), "image.img"] {
                let layer = if distro {
                    format!("distro,{path},{UUID}")
                } else {
                    root.expand(LAYER)
                };
                let scratch = if distro {
                    root.expand(SCRATCH)
                } else {
                    path.to_owned()
                };
                for (operation, dry_run) in [("run", false), ("run", true), ("provision", false)] {
                    let mut arguments = vec![
                        "sandbox".to_owned(),
                        operation.to_owned(),
                        "--layer".to_owned(),
                        layer.clone(),
                        "--scratch".to_owned(),
                        scratch.clone(),
                    ];
                    if dry_run {
                        arguments.push("--dry-run".to_owned());
                    }
                    if operation == "provision" {
                        arguments.extend(["--state-dir".to_owned(), root.expand("{root/state}")]);
                    }
                    let output = root
                        .command(&arguments)
                        .current_dir(root.below(directory))
                        .output()
                        .unwrap();
                    let outcome = Outcome::new(arguments, output);
                    let message =
                        outcome.assert_error("unsupported", &["unbalanced brackets", &resolved]);
                    if path == "image.img" {
                        assert!(message.contains("resolves to"), "{message}");
                    }
                    assert_eq!(root.snapshot(), before, "{}", outcome.describe());
                }
            }
        }
    }
}

#[test]
fn validation_errors_exit_1_before_any_side_effect() {
    let cases: Vec<Refusal> = sandbox_refusals()
        .into_iter()
        .chain(run_refusals())
        .filter(|case| case.hosts.include_this_host())
        .collect();
    in_parallel(&cases, |case| {
        let root = Root::new();
        for (relative, content) in &case.files {
            root.write_content(relative, *content);
        }
        for relative in &case.removed {
            root.remove(relative);
        }
        let before = root.snapshot();
        let arguments: Vec<&str> = case.arguments.iter().map(String::as_str).collect();
        let outcome = root.nvx_rs_with(&arguments, &case.environment);
        let fragments: Vec<String> = case
            .fragments
            .iter()
            .map(|fragment| root.expand(fragment))
            .collect();
        let fragments: Vec<&str> = fragments.iter().map(String::as_str).collect();
        eprintln!("case: {}", case.name);
        outcome.assert_error(case.code, &fragments);
        // Nothing changed: no state directory, no outcome report, and no record of an OpenVMM,
        // which records its command line once it has accepted it.
        assert_eq!(root.snapshot(), before, "{}", case.name);
    });
}
