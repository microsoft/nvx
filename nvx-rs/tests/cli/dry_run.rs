//! The dry runs of `nvx-rs sandbox run` and `nvx-rs run`, which print the OpenVMM command that a
//! run would launch, as `>> COMMAND`, and launch nothing, and the property that the command splits
//! back into the arguments that it holds.

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use crate::harness::{Content, Hosts, LAYER, OPENVMM, Outcome, Root, SCRATCH, in_parallel, native};
use crate::openvmm::{Options, WORKLOAD, child, cold_boot, one_shot, restore, share_token};

/// An egress policy file with an allowed range, an exclusion, a port, and a denied protocol, which
/// compiles into the rules [`COMPILED_ALLOW`] and [`COMPILED_DENY`].
const POLICY: Content = Content::Text(
    r#"{"allow": [{"cidr": "192.0.2.0/24", "except": ["192.0.2.128/25"]}, {"cidr": "2001:db8::/32", "protocol": "tcp", "port": 443}], "deny": [{"cidr": "198.51.100.0/24", "protocol": "udp"}]}"#,
);
/// The allow rules of [`POLICY`], in OpenVMM's form: the range without its exclusion.
const COMPILED_ALLOW: [&str; 2] = ["192.0.2.0/25", "2001:db8::/32:tcp:443"];
/// The deny rules of [`POLICY`], in OpenVMM's form.
const COMPILED_DENY: [&str; 1] = ["198.51.100.0/24:udp"];
/// The files whose host paths hold commas, and a space.
const COMMAS: [(&str, Content); 3] = [
    ("alpha,beta/data.txt", Content::Text("data\n")),
    ("2026, final/data.txt", Content::Text("data\n")),
    ("settings,v2.json", Content::Text("{}\n")),
];

/// A dry run, and the OpenVMM command that it prints.
struct DryRun {
    /// The name of the case.
    name: &'static str,
    /// The hosts on which it runs.
    hosts: Hosts,
    /// The files that the case writes below the root first, with their contents.
    files: Vec<(&'static str, Content)>,
    /// The files that the case removes from the root first.
    removed: Vec<&'static str>,
    /// The arguments after `--root ROOT`, with placeholders.
    arguments: Vec<String>,
    /// The options of the OpenVMM command line that it prints, with placeholders.
    options: Options,
}

/// Returns the dry run `name` of `arguments`, which prints an OpenVMM command line of `options`.
fn dry_run<S: ToString>(
    name: &'static str,
    arguments: impl IntoIterator<Item = S>,
    options: Options,
) -> DryRun {
    DryRun {
        name,
        hosts: Hosts::Every,
        files: Vec::new(),
        removed: Vec::new(),
        arguments: arguments
            .into_iter()
            .map(|argument| argument.to_string())
            .collect(),
        options,
    }
}

impl DryRun {
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

    /// Returns the case for `hosts` alone.
    fn on(mut self, hosts: Hosts) -> Self {
        self.hosts = hosts;
        self
    }
}

/// Returns `nvx-rs sandbox run` of the distro layer and scratch image of the root, with
/// `options`, as a dry run.
fn sandbox_run(options: &[&str]) -> Vec<String> {
    ["sandbox", "run", "--layer", LAYER, "--scratch", SCRATCH]
        .iter()
        .chain(options)
        .chain(&["--dry-run"])
        .map(|argument| (*argument).to_owned())
        .collect()
}

/// Returns `nvx-rs run OPTIONS --dry-run`.
fn run(options: &[&str]) -> Vec<String> {
    ["run"]
        .iter()
        .chain(options)
        .chain(&["--dry-run"])
        .map(|argument| (*argument).to_owned())
        .collect()
}

/// Returns the default workload tokens of a sandbox, then `tokens`.
fn workload(tokens: &[String]) -> Vec<String> {
    WORKLOAD
        .iter()
        .map(|token| (*token).to_owned())
        .chain(tokens.iter().cloned())
        .collect()
}

/// Returns the `--mount-child` value of the aggregate child at `index`, which the guest binds at
/// `target`, of the host path `host`, with the access and kind `mode`, such as `ro` or `ro,file`.
fn mount_child(index: usize, target: &str, host: &str, mode: &str) -> String {
    format!("{},{host},{mode}", child(index, target))
}

/// Returns the dry runs of `nvx-rs sandbox run`.
fn sandbox_dry_runs() -> Vec<DryRun> {
    let network = || {
        one_shot()
            .with("--net", ["10.0.0.2/24"])
            .with("--network-profile", ["portable"])
    };
    vec![
        dry_run("minimal", sandbox_run(&[]), one_shot()),
        dry_run(
            "balanced disk path brackets",
            [
                "sandbox",
                "run",
                "--dry-run",
                "--layer",
                "distro,{root/distro[[1][2]].erofs},11111111-1111-1111-1111-111111111111",
                "--scratch",
                "{root/scratch[][1].ext4}",
            ],
            one_shot().with(
                "--microvm-sandbox-block",
                [
                    "distro:file:{root/distro[[1][2]].erofs},ro",
                    "scratch:file:{root/scratch[][1].ext4}",
                ],
            ),
        )
        .with_file("distro[[1][2]].erofs", Content::Text("distro\n"))
        .with_file("scratch[][1].ext4", Content::Text("scratch\n")),
        dry_run(
            "explicit scratch upper",
            sandbox_run(&["--upper", "scratch"]),
            one_shot(),
        ),
        // An upper in RAM attaches the distro layer alone, and asks the guest for a tmpfs upper.
        dry_run(
            "RAM upper",
            [
                "sandbox",
                "run",
                "--layer",
                LAYER,
                "--upper",
                "ram",
                "--dry-run",
            ],
            one_shot().in_ram(),
        ),
        dry_run(
            "every layer and option",
            sandbox_run(&[
                "--entrypoint",
                "/bin/server",
                "--arg=--port",
                "--arg",
                "8080",
                "--hostname",
                "worker",
                "--workload-user",
                "1000:1000",
                "--memory-max",
                "268435456",
                "--pids-max",
                "64",
                "--memory-mib",
                "512",
                "--hypervisor",
                "mshv",
                "--cmdline",
                "quiet loglevel=3",
                "--outcome-report",
                "{root/reports/openvmm-run.json}",
            ]),
            one_shot()
                .with("--microvm-workload-identity", ["1000:1000"])
                .with("--hypervisor", ["mshv"])
                .with("--memory", ["512M"])
                .with("--microvm-report", ["{root/reports/openvmm-run.json}"])
                // The guest's limit of processes counts its init too.
                .sandbox_kernel(
                    "quiet loglevel=3",
                    &[
                        "nvx_entrypoint=/bin/server",
                        "nvx_hostname=worker",
                        "nvx_arg=--port",
                        "nvx_arg=8080",
                        "nvx_memory_max=268435456",
                        "nvx_pids_max=65",
                    ],
                ),
        ),
        dry_run(
            "explicit hypervisor",
            sandbox_run(&["--hypervisor", "kvm"]),
            one_shot().with("--hypervisor", ["kvm"]),
        ),
        // `--memory-mib` takes from 1 MiB to the most whose count of bytes fits in 64 bits, as
        // OpenVMM reads a size of RAM.
        dry_run(
            "least memory",
            sandbox_run(&["--memory-mib", "1"]),
            one_shot().with("--memory", ["1M"]),
        ),
        dry_run(
            "most memory",
            sandbox_run(&["--memory-mib", "17592186044415"]),
            one_shot().with("--memory", ["17592186044415M"]),
        ),
        // One directory is OpenVMM's own share, read-only by default.
        dry_run(
            "one share",
            sandbox_run(&["--mount", "/data,{root/share}"]),
            one_shot().with("--mount", ["/data,{root/share},ro"]),
        ),
        dry_run(
            "writable share",
            sandbox_run(&["--mount", "/data,{root/share},rw"]),
            one_shot().with("--mount", ["/data,{root/share},rw"]),
        ),
        // A file is a child of the aggregate, which a boot token binds at its target.
        dry_run(
            "file share",
            sandbox_run(&["--mount", "/etc/single.txt,{root/single.txt},ro"]),
            one_shot()
                .with("--mount-aggregate", ["/run/nvx/shares"])
                .with(
                    "--mount-child",
                    [mount_child(
                        0,
                        "/etc/single.txt",
                        "{root/single.txt}",
                        "ro,file",
                    )],
                )
                .sandbox_kernel(
                    "",
                    &workload(&[share_token(0, "/etc/single.txt", "ro", true)]),
                ),
        ),
        dry_run(
            "several shares with policies",
            sandbox_run(&[
                "--mount",
                "/data,{root/share},rw",
                "--mount-deny",
                "{root/share/hidden}",
                "--mount-allow",
                "{root/share/hidden/shown}",
                "--mount-write",
                "{root/share/writable}",
                "--mount",
                "/other,{root/share2}",
            ]),
            one_shot()
                .with("--mount-aggregate", ["/run/nvx/shares"])
                .with(
                    "--mount-child",
                    [
                        mount_child(0, "/data", "{root/share}", "rw"),
                        mount_child(1, "/other", "{root/share2}", "ro"),
                    ],
                )
                .with("--mount-deny", ["{root/share/hidden}"])
                .with("--mount-allow", ["{root/share/hidden/shown}"])
                .with("--mount-write", ["{root/share/writable}"])
                .sandbox_kernel(
                    "",
                    &workload(&[
                        share_token(0, "/data", "rw", false),
                        share_token(1, "/other", "ro", false),
                    ]),
                ),
        ),
        dry_run(
            "network",
            sandbox_run(&[
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-ingress",
                "allow",
                "--network-egress-allow",
                "10.0.0.0/8:tcp:443",
                "--network-egress-deny",
                "10.1.0.0/16",
                "--host-loopback",
                "allow",
            ]),
            network()
                .with("--network-egress", ["deny"])
                .with("--network-ingress", ["allow"])
                .with("--host-loopback", ["allow"])
                .with("--network-egress-allow", ["10.0.0.0/8:tcp:443"])
                .with("--network-egress-deny", ["10.1.0.0/16"]),
        ),
        // The typed policy renders every access, ingress denied by default included.
        dry_run(
            "proxy-only network",
            sandbox_run(&[
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--host-loopback",
                "deny",
                "--network-proxy",
                "10.0.0.1:3128",
            ]),
            network()
                .with("--network-egress", ["deny"])
                .with("--network-ingress", ["deny"])
                .with("--host-loopback", ["deny"])
                .with("--network-proxy", ["10.0.0.1:3128"]),
        ),
        dry_run(
            "egress policy file",
            sandbox_run(&[
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                "{root/policy.json}",
            ]),
            network()
                .with("--network-egress", ["deny"])
                .with("--network-ingress", ["deny"])
                .with("--network-egress-allow", COMPILED_ALLOW)
                .with("--network-egress-deny", COMPILED_DENY),
        )
        .with_file("policy.json", POLICY),
        dry_run(
            "quoted arguments",
            sandbox_run(&["--cmdline", "a=\"b c\" d"]),
            one_shot().sandbox_kernel("a=\"b c\" d", &WORKLOAD),
        ),
        dry_run(
            "caller-owned share",
            sandbox_run(&["--mount", "/data,{root/share}", "--mount-owner", "caller"]),
            one_shot()
                .with("--mount", ["/data,{root/share},ro"])
                .with("--mount-owner", ["caller"]),
        )
        .on(Hosts::Posix),
    ]
}

/// Returns the dry runs of `nvx-rs run`.
fn run_dry_runs() -> Vec<DryRun> {
    let ubuntu = ("build/initramfs-ubuntu.cpio.gz", Content::Text("ubuntu\n"));
    let azure = (
        "build/initramfs-azurelinux.cpio.gz",
        Content::Text("azurelinux\n"),
    );
    let two_shares = || {
        cold_boot()
            .with("--mount-aggregate", ["/run/nvx/shares"])
            .with(
                "--mount-child",
                [
                    mount_child(0, "/data", "{root/share}", "rw"),
                    mount_child(1, "/other", "{root/share2}", "ro"),
                ],
            )
    };
    let two_tokens = || {
        [
            share_token(0, "/data", "rw", false),
            share_token(1, "/other", "ro", false),
        ]
    };
    let mut many = vec![
        "--restore-snapshot".to_owned(),
        "{root/snapshot}".to_owned(),
    ];
    for index in 0..20 {
        many.extend([
            "--mount".to_owned(),
            format!("/share/{index:02},{{root/share}}"),
        ]);
    }
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let many_children: Vec<String> = (0..20)
        .map(|index| mount_child(index, &format!("/share/{index:02}"), "{root/share}", "ro"))
        .collect();
    let long = "x".repeat(1100);
    let fits = "x".repeat(841);
    vec![
        dry_run("run: Alpine default", run(&[]), cold_boot()),
        dry_run(
            "run: Ubuntu default",
            run(&["--guest", "ubuntu"]),
            cold_boot()
                .with("--memory", ["512M"])
                .with("--initrd", ["{root/build/initramfs-ubuntu.cpio.gz}"]),
        )
        .with_file(ubuntu.0, ubuntu.1),
        dry_run(
            "run: Azure Linux default",
            run(&["--guest", "azurelinux"]),
            cold_boot()
                .with("--memory", ["512M"])
                .with("--initrd", ["{root/build/initramfs-azurelinux.cpio.gz}"]),
        )
        .with_file(azure.0, azure.1),
        dry_run(
            "run: Alpine with memory",
            run(&["--guest", "alpine", "--memory-mib", "256"]),
            cold_boot().with("--memory", ["256M"]),
        ),
        dry_run(
            "run: Ubuntu with memory and capacity",
            run(&[
                "--guest",
                "ubuntu",
                "--memory-mib",
                "1024",
                "--memory-capacity-mib",
                "2048",
            ]),
            cold_boot()
                .with("--memory", ["1024M"])
                .with("--initrd", ["{root/build/initramfs-ubuntu.cpio.gz}"])
                .with("--memory-capacity", ["2048M"]),
        )
        .with_file(ubuntu.0, ubuntu.1),
        dry_run(
            "run: Azure Linux with the default capacity",
            run(&["--guest", "azurelinux", "--memory-capacity-mib", "512"]),
            cold_boot()
                .with("--memory", ["512M"])
                .with("--initrd", ["{root/build/initramfs-azurelinux.cpio.gz}"])
                .with("--memory-capacity", ["512M"]),
        )
        .with_file(azure.0, azure.1),
        dry_run(
            "run: last guest",
            run(&["--guest", "ubuntu", "--guest", "azurelinux"]),
            cold_boot()
                .with("--memory", ["512M"])
                .with("--initrd", ["{root/build/initramfs-azurelinux.cpio.gz}"]),
        )
        .with_file(azure.0, azure.1),
        dry_run(
            "run: every boot option",
            run(&[
                "--hypervisor",
                "mshv",
                "--machine",
                "microvm",
                "--memory-mib",
                "1024",
                "--memory-capacity-mib",
                "2048",
                "--processors",
                "8",
                "--cpu-profile",
                "intel.alderlake.v1",
                "--cmdline",
                "quiet loglevel=3",
                "--outcome-report",
                "{root/reports/openvmm-run.json}",
            ]),
            cold_boot()
                .with("--processors", ["8"])
                .with("--hypervisor", ["mshv"])
                .with("--memory", ["1024M"])
                .with("--memory-capacity", ["2048M"])
                .with("--cpu-profile", ["intel.alderlake.v1"])
                .with("--microvm-report", ["{root/reports/openvmm-run.json}"])
                .with("--cmdline", ["quiet loglevel=3"]),
        ),
        dry_run(
            "run: auto hypervisor",
            run(&["--hypervisor", "auto"]),
            cold_boot(),
        ),
        dry_run(
            "run: KVM",
            run(&["--hypervisor", "kvm"]),
            cold_boot().with("--hypervisor", ["kvm"]),
        ),
        dry_run(
            "run: WHP",
            run(&["--hypervisor", "whp"]),
            cold_boot().with("--hypervisor", ["whp"]),
        ),
        // Integers are ASCII decimal digits, and print in their canonical form.
        dry_run(
            "run: leading zeros",
            run(&[
                "--memory-mib",
                "0128",
                "--memory-capacity-mib",
                "0256",
                "--processors",
                "2",
            ]),
            cold_boot()
                .with("--processors", ["2"])
                .with("--memory-capacity", ["256M"]),
        ),
        // The most MiB whose count of bytes fits in 64 bits, as OpenVMM reads a size of RAM.
        dry_run(
            "run: most memory",
            run(&["--memory-mib", "17592186044415"]),
            cold_boot().with("--memory", ["17592186044415M"]),
        ),
        dry_run(
            "run: equal memory and capacity",
            run(&["--memory-mib", "512", "--memory-capacity-mib", "512"]),
            cold_boot()
                .with("--memory", ["512M"])
                .with("--memory-capacity", ["512M"]),
        ),
        dry_run(
            "run: last processors",
            run(&["--processors", "2", "--processors", "4"]),
            cold_boot().with("--processors", ["4"]),
        ),
        dry_run(
            "run: cpu profile auto",
            run(&["--cpu-profile", "auto"]),
            cold_boot().with("--cpu-profile", ["auto"]),
        ),
        dry_run(
            "run: cpu profile host",
            run(&["--cpu-profile", "host"]),
            cold_boot().with("--cpu-profile", ["host"]),
        ),
        dry_run(
            "run: restore",
            run(&["--restore-snapshot", "{root/snapshot}"]),
            restore(),
        ),
        // A restore needs no guest kernel or initramfs.
        dry_run(
            "run: restore without guest artifacts",
            run(&["--restore-snapshot", "{root/snapshot}"]),
            restore(),
        )
        .without("build/vmlinux")
        .without("build/initramfs.cpio.gz"),
        dry_run(
            "run: restore with every option",
            run(&[
                "--hypervisor",
                "mshv",
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
                "host",
                "--outcome-report",
                "{root/reports/restore.json}",
                "--cmdline",
                "quiet",
            ]),
            restore()
                .with("--processors", ["4"])
                .with("--hypervisor", ["mshv"])
                .with("--restore-processors", ["2"])
                .with("--restore-memory", ["1024M"])
                .with("--restore-ready-path", ["{root/ready.sock}"])
                .with("--cpu-profile", ["host"])
                .with("--microvm-report", ["{root/reports/restore.json}"])
                .with("--cmdline", ["quiet"]),
        ),
        dry_run(
            "run: restore of the Alpine guest",
            run(&["--guest", "alpine", "--restore-snapshot", "{root/snapshot}"]),
            restore(),
        ),
        dry_run(
            "run: restore with equal processors",
            run(&[
                "--processors",
                "2",
                "--restore-processors",
                "2",
                "--restore-snapshot",
                "{root/snapshot}",
            ]),
            restore()
                .with("--processors", ["2"])
                .with("--restore-processors", ["2"]),
        ),
        dry_run(
            "run: restore ignores the memory",
            run(&[
                "--memory-mib",
                "512",
                "--restore-snapshot",
                "{root/snapshot}",
            ]),
            restore(),
        ),
        // Paths print with their components alone, as `Path::components` reads them, relative
        // ones included: a leading `.` stays.
        dry_run(
            "run: restore paths",
            run(&[
                "--restore-snapshot",
                "./snapshot/",
                "--restore-ready-path",
                "run//ready.sock",
            ]),
            restore()
                .with("--restore-snapshot", [native("./snapshot")])
                .with("--restore-ready-path", [native("run/ready.sock")]),
        ),
        dry_run(
            "run: one share",
            run(&["--mount", "/data,{root/share}"]),
            cold_boot().with("--mount", ["/data,{root/share},ro"]),
        ),
        dry_run(
            "run: one writable share",
            run(&["--mount", "/data,{root/share},rw"]),
            cold_boot().with("--mount", ["/data,{root/share},rw"]),
        ),
        dry_run(
            "run: one relative share",
            run(&["--mount", "/data,share"]),
            cold_boot().with("--mount", ["/data,share,ro"]),
        ),
        // OpenVMM resolves relative policy paths in the only share.
        dry_run(
            "run: one share with relative policy paths",
            run(&[
                "--mount",
                "/data,{root/share},rw",
                "--mount-write",
                "writable",
                "--mount-allow",
                "hidden/shown",
                "--mount-deny",
                "./hidden/",
            ]),
            cold_boot()
                .with("--mount", ["/data,{root/share},rw"])
                .with("--mount-deny", [native("./hidden")])
                .with("--mount-allow", [native("hidden/shown")])
                .with("--mount-write", ["writable"]),
        ),
        dry_run(
            "run: policy paths without a share",
            run(&["--mount-deny", "{root/share/hidden}", "--mount-allow", "x"]),
            cold_boot()
                .with("--mount-deny", ["{root/share/hidden}"])
                .with("--mount-allow", ["x"]),
        ),
        dry_run(
            "run: several shares with policy paths",
            run(&[
                "--mount",
                "/data,{root/share},rw",
                "--mount",
                "/other,{root/share2}",
                "--mount-write",
                "{root/share/writable}",
                "--mount-allow",
                "{root/share/hidden/shown}",
                "--mount-deny",
                "{root/share/hidden}",
                "--mount-deny",
                "{root/share2/./data.txt}",
            ]),
            two_shares()
                .with(
                    "--mount-deny",
                    ["{root/share/hidden}", "{root/share2/data.txt}"],
                )
                .with("--mount-allow", ["{root/share/hidden/shown}"])
                .with("--mount-write", ["{root/share/writable}"])
                .kernel(&two_tokens()),
        ),
        dry_run(
            "run: several shares with a command line",
            run(&[
                "--mount",
                "/data,{root/share},rw",
                "--mount",
                "/other,{root/share2}",
                "--cmdline",
                "quiet",
            ]),
            two_shares().kernel(&[
                "quiet".to_owned(),
                share_token(0, "/data", "rw", false),
                share_token(1, "/other", "ro", false),
            ]),
        ),
        dry_run(
            "run: one shared file",
            run(&["--mount", "/etc/single.txt,{root/single.txt},ro"]),
            cold_boot()
                .with("--mount-aggregate", ["/run/nvx/shares"])
                .with(
                    "--mount-child",
                    [mount_child(
                        0,
                        "/etc/single.txt",
                        "{root/single.txt}",
                        "ro,file",
                    )],
                )
                .kernel(&[share_token(0, "/etc/single.txt", "ro", true)]),
        ),
        dry_run(
            "run: one relative shared file",
            run(&["--mount", "/etc/single.txt,single.txt"]),
            cold_boot()
                .with("--mount-aggregate", ["/run/nvx/shares"])
                .with(
                    "--mount-child",
                    [mount_child(0, "/etc/single.txt", "single.txt", "ro,file")],
                )
                .kernel(&[share_token(0, "/etc/single.txt", "ro", true)]),
        ),
        dry_run(
            "run: a directory and a file",
            run(&[
                "--mount",
                "/work,{root/share},rw",
                "--mount",
                "/config/single.txt,{root/single.txt}",
            ]),
            cold_boot()
                .with("--mount-aggregate", ["/run/nvx/shares"])
                .with(
                    "--mount-child",
                    [
                        mount_child(0, "/work", "{root/share}", "rw"),
                        mount_child(1, "/config/single.txt", "{root/single.txt}", "ro,file"),
                    ],
                )
                .kernel(&[
                    share_token(0, "/work", "rw", false),
                    share_token(1, "/config/single.txt", "ro", true),
                ]),
        ),
        // The mode follows the last comma, so a host path may hold commas.
        dry_run(
            "run: host paths with commas",
            run(&[
                "--mount",
                "/work,{root/alpha,beta},rw",
                "--mount",
                "/reports,{root/2026, final},ro",
                "--mount",
                "/config/settings.json,{root/settings,v2.json},ro",
                "--mount-deny",
                "{root/alpha,beta/data.txt}",
            ]),
            cold_boot()
                .with("--mount-aggregate", ["/run/nvx/shares"])
                .with(
                    "--mount-child",
                    [
                        mount_child(0, "/work", "{root/alpha,beta}", "rw"),
                        mount_child(1, "/reports", "{root/2026, final}", "ro"),
                        mount_child(
                            2,
                            "/config/settings.json",
                            "{root/settings,v2.json}",
                            "ro,file",
                        ),
                    ],
                )
                .with("--mount-deny", ["{root/alpha,beta/data.txt}"])
                .kernel(&[
                    share_token(0, "/work", "rw", false),
                    share_token(1, "/reports", "ro", false),
                    share_token(2, "/config/settings.json", "ro", true),
                ]),
        )
        .with_file(COMMAS[0].0, COMMAS[0].1)
        .with_file(COMMAS[1].0, COMMAS[1].1)
        .with_file(COMMAS[2].0, COMMAS[2].1),
        dry_run(
            "run: one directory with commas",
            run(&["--mount", "/work,{root/alpha,beta},rw"]),
            cold_boot().with("--mount", ["/work,{root/alpha,beta},rw"]),
        )
        .with_file(COMMAS[0].0, COMMAS[0].1),
        dry_run(
            "run: host path ending in a mode",
            run(&["--mount", "/data,{root/share},rw,ro"]),
            cold_boot().with("--mount", ["/data,{root/share},rw,ro"]),
        ),
        dry_run(
            "run: host path ending in a comma",
            run(&["--mount", "/data,{root/share},,rw"]),
            cold_boot().with("--mount", ["/data,{root/share},,rw"]),
        ),
        dry_run(
            "run: vmm owner",
            run(&["--mount", "/data,{root/share}", "--mount-owner", "vmm"]),
            cold_boot()
                .with("--mount", ["/data,{root/share},ro"])
                .with("--mount-owner", ["vmm"]),
        ),
        dry_run(
            "run: vmm owner of several shares",
            run(&[
                "--mount",
                "/data,{root/share},rw",
                "--mount",
                "/other,{root/share2}",
                "--mount-owner",
                "vmm",
            ]),
            two_shares()
                .with("--mount-owner", ["vmm"])
                .kernel(&two_tokens()),
        ),
        // A restore adds no boot tokens: its guest keeps the binds that its boot made.
        dry_run(
            "run: restore with shares",
            run(&[
                "--restore-snapshot",
                "{root/snapshot}",
                "--mount",
                "/data,{root/share},rw",
                "--mount",
                "/other,{root/share2}",
                "--mount",
                "/etc/single.txt,{root/single.txt}",
            ]),
            restore()
                .with("--mount-aggregate", ["/run/nvx/shares"])
                .with(
                    "--mount-child",
                    [
                        mount_child(0, "/data", "{root/share}", "rw"),
                        mount_child(1, "/other", "{root/share2}", "ro"),
                        mount_child(2, "/etc/single.txt", "{root/single.txt}", "ro,file"),
                    ],
                ),
        ),
        dry_run(
            "run: restore with shares and a command line",
            run(&[
                "--restore-snapshot",
                "{root/snapshot}",
                "--mount",
                "/data,{root/share},rw",
                "--mount",
                "/other,{root/share2}",
                "--cmdline",
                "quiet",
            ]),
            restore()
                .with("--mount-aggregate", ["/run/nvx/shares"])
                .with(
                    "--mount-child",
                    [
                        mount_child(0, "/data", "{root/share}", "rw"),
                        mount_child(1, "/other", "{root/share2}", "ro"),
                    ],
                )
                .with("--cmdline", ["quiet"]),
        ),
        // `nvx-rs run` passes the network options to OpenVMM as they are.
        dry_run(
            "run: network",
            run(&[
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-ingress",
                "allow",
                "--network-egress-allow",
                "10.0.0.0/8:tcp:443",
                "--network-egress-allow",
                "2001:db8::/32",
                "--network-egress-deny",
                "10.1.0.0/16",
                "--host-loopback",
                "allow",
                "--network-proxy",
                "10.0.0.1:3128",
                "--host-loopback-forward",
                "tcp:8080:80",
                "--host-loopback-forward",
                "udp:5353:53",
            ]),
            cold_boot()
                .with("--net", ["10.0.0.2/24"])
                .with("--network-profile", ["portable"])
                .with("--network-egress", ["deny"])
                .with("--network-ingress", ["allow"])
                .with(
                    "--network-egress-allow",
                    ["10.0.0.0/8:tcp:443", "2001:db8::/32"],
                )
                .with("--network-egress-deny", ["10.1.0.0/16"])
                .with("--host-loopback", ["allow"])
                .with("--network-proxy", ["10.0.0.1:3128"])
                .with("--host-loopback-forward", ["tcp:8080:80", "udp:5353:53"]),
        ),
        dry_run(
            "run: egress default",
            run(&["--network-egress", "deny"]),
            cold_boot().with("--network-egress", ["deny"]),
        ),
        // A restore takes the addressing of its NIC from the snapshot, so it passes the profile
        // and the policy without `--net`.
        dry_run(
            "run: restore of a networked snapshot",
            run(&[
                "--restore-snapshot",
                "{root/snapshot}",
                "--network-profile",
                "portable",
            ]),
            restore().with("--network-profile", ["portable"]),
        ),
        dry_run(
            "run: restore with a network policy",
            run(&[
                "--restore-snapshot",
                "{root/snapshot}",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-egress-allow",
                "10.0.0.0/8:tcp:443",
                "--network-egress-deny",
                "10.1.0.0/16",
                "--host-loopback",
                "deny",
                "--network-proxy",
                "10.0.0.1:3128",
            ]),
            restore()
                .with("--network-profile", ["portable"])
                .with("--network-egress", ["deny"])
                .with("--network-egress-allow", ["10.0.0.0/8:tcp:443"])
                .with("--network-egress-deny", ["10.1.0.0/16"])
                .with("--host-loopback", ["deny"])
                .with("--network-proxy", ["10.0.0.1:3128"]),
        ),
        dry_run(
            "run: restore with an egress policy file",
            run(&[
                "--restore-snapshot",
                "{root/snapshot}",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                "{root/policy.json}",
            ]),
            restore()
                .with("--network-profile", ["portable"])
                .with("--network-egress", ["deny"])
                .with("--network-egress-allow", COMPILED_ALLOW)
                .with("--network-egress-deny", COMPILED_DENY),
        )
        .with_file("policy.json", POLICY),
        dry_run(
            "run: host loopback forward",
            run(&["--host-loopback-forward", "tcp:8080:80"]),
            cold_boot().with("--host-loopback-forward", ["tcp:8080:80"]),
        ),
        dry_run(
            "run: egress policy file",
            run(&[
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                "{root/policy.json}",
            ]),
            cold_boot()
                .with("--network-egress", ["deny"])
                .with("--network-egress-allow", COMPILED_ALLOW)
                .with("--network-egress-deny", COMPILED_DENY),
        )
        .with_file("policy.json", POLICY),
        dry_run(
            "run: empty egress policy file",
            run(&[
                "--network-egress",
                "allow",
                "--network-egress-policy-file",
                "{root/./policy.json}",
            ]),
            cold_boot().with("--network-egress", ["allow"]),
        )
        .with_file("policy.json", Content::Text("{}")),
        dry_run(
            "run: outcome report path",
            run(&["--outcome-report", "reports//./outcome.json"]),
            cold_boot().with("--microvm-report", [native("reports/outcome.json")]),
        ),
        dry_run(
            "run: quoted command line",
            run(&["--cmdline", "a=\"b c\" d"]),
            cold_boot().with("--cmdline", ["a=\"b c\" d"]),
        ),
        dry_run(
            "run: command line starting with a dash",
            run(&["--cmdline=-x"]),
            cold_boot().with("--cmdline", ["-x"]),
        ),
        dry_run(
            "run: command line of help",
            run(&["--cmdline=--help"]),
            cold_boot().with("--cmdline", ["--help"]),
        ),
        dry_run(
            "run: empty command line",
            run(&["--cmdline", ""]),
            cold_boot(),
        ),
        dry_run(
            "run: blank command line",
            run(&["--cmdline", " "]),
            cold_boot().with("--cmdline", [" "]),
        ),
        dry_run(
            "run: last command line",
            run(&["--cmdline", "a", "--cmdline", "b"]),
            cold_boot().with("--cmdline", ["b"]),
        ),
        // One directory takes no boot token, so no budget limits the command line.
        dry_run(
            "run: long command line of one directory share",
            run(&["--mount", "/data,{root/share}", "--cmdline", &long]),
            cold_boot()
                .with("--mount", ["/data,{root/share},ro"])
                .with("--cmdline", [long.as_str()]),
        ),
        dry_run(
            "run: restore without the budget",
            run(&many),
            restore()
                .with("--mount-aggregate", ["/run/nvx/shares"])
                .with("--mount-child", many_children),
        ),
        // The command line and the boot tokens fill the 1024 bytes that OpenVMM leaves them.
        dry_run(
            "run: budget fits",
            run(&[
                "--mount",
                "/a,{root/share}",
                "--mount",
                "/b,{root/share2},rw",
                "--cmdline",
                &fits,
            ]),
            cold_boot()
                .with("--mount-aggregate", ["/run/nvx/shares"])
                .with(
                    "--mount-child",
                    [
                        mount_child(0, "/a", "{root/share}", "ro"),
                        mount_child(1, "/b", "{root/share2}", "rw"),
                    ],
                )
                .kernel(&[
                    fits.clone(),
                    share_token(0, "/a", "ro", false),
                    share_token(1, "/b", "rw", false),
                ]),
        ),
        dry_run(
            "run: caller owner",
            run(&["--mount", "/data,{root/share}", "--mount-owner", "caller"]),
            cold_boot()
                .with("--mount", ["/data,{root/share},ro"])
                .with("--mount-owner", ["caller"]),
        )
        .on(Hosts::Posix),
    ]
}

#[test]
fn dry_runs_print_the_openvmm_command_and_launch_nothing() {
    let cases: Vec<DryRun> = sandbox_dry_runs()
        .into_iter()
        .chain(run_dry_runs())
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
        let outcome = root.nvx_rs(&arguments);
        eprintln!("case: {}", case.name);
        outcome.assert_success();
        let (command, rest) = outcome.command();
        assert!(rest.is_empty(), "{}: {}", case.name, outcome.describe());
        let (program, arguments) = command.split_first().unwrap();
        assert_eq!(
            *program,
            root.below(OPENVMM).to_str().unwrap(),
            "{}",
            case.name
        );
        case.options
            .assert_matches(&root, &Options::parse(arguments), case.name);
        // The run launched no OpenVMM, which records its command line once it has accepted it.
        assert_eq!(root.snapshot(), before, "{}", case.name);
    });
}

/// Returns the strategy of kernel command lines: text of the characters that shells and the C
/// runtime treat specially, and others, which may start with a dash.
fn command_lines() -> impl Strategy<Value = String> {
    let mut characters: Vec<char> =
        " \t\"'\\$`!*?&|;<>(){}[]#~=,%^-_./:@+aZ09\u{e9}\u{20ac}\u{4e2d}"
            .chars()
            .collect();
    // A Windows `>> ` line ends at a line break, which no command line there may hold.
    if cfg!(unix) {
        characters.push('\n');
    }
    prop::collection::vec(prop::sample::select(characters), 0..24)
        .prop_map(|characters| characters.into_iter().collect())
}

#[test]
fn the_openvmm_command_of_a_run_splits_back_into_its_arguments() {
    // `nvx-rs run` passes its kernel command line to OpenVMM as it is, so the `>> ` line, split as
    // this host splits command lines, must give it back whatever characters it holds: with a
    // shell-word splitter on POSIX, and with `CommandLineToArgvW` on Windows.
    let root = Root::new();
    let openvmm = root.below(OPENVMM).to_str().unwrap().to_owned();
    let mut runner = TestRunner::new(Config {
        cases: 64,
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&command_lines(), |command_line| {
            let arguments = vec![
                "run".to_owned(),
                format!("--cmdline={command_line}"),
                "--dry-run".to_owned(),
            ];
            let output = root.command(&arguments).output().unwrap();
            let outcome = Outcome::new(arguments, output);
            outcome.assert_success();
            let (command, rest) = outcome.command();
            prop_assert!(rest.is_empty(), "{}", outcome.describe());
            prop_assert_eq!(&command[0], &openvmm);
            let mut expected = cold_boot().expanded(&root);
            if !command_line.is_empty() {
                expected = expected.with("--cmdline", [command_line.clone()]);
            }
            prop_assert_eq!(
                Options::parse(&command[1..]),
                expected,
                "{}",
                outcome.describe()
            );
            Ok(())
        })
        .unwrap();
}
