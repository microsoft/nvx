//! OpenVMM command lines: their options, each with its values, and the options with which the
//! launches of `nvx-rs` start OpenVMM, as its documentation describes them, from which the tests
//! derive what they expect, and the checks of the images that a launch hands OpenVMM, of the
//! outcome reports that OpenVMM writes, and of the control endpoints of managed starts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::harness::{HYPERVISOR, OPENVMM, Root, is_hex};

/// Caller bytes left by the two-directory fixture, its native bootstrap, and a terminating NUL.
pub(crate) fn two_share_command_line_room() -> usize {
    use nvx::host::filesystem::GUEST_EXPORT;
    use nvx::host::sandbox::mount::{
        AGGREGATE_MOUNT_TARGET, MountAccess, aggregate_command_line_fragment,
    };

    let kernel = "nvx_hostname=6e7678 nvx_machine_id=image nvx_maps=2 nvx_mode=one-shot \
                  nvx_share=0,/a,ro nvx_share=1,/b,rw";
    let fragment =
        aggregate_command_line_fragment(&[MountAccess::ReadOnly, MountAccess::ReadWrite])
            .replace(AGGREGATE_MOUNT_TARGET, GUEST_EXPORT);
    nvx::host::config::COMMAND_LINE_BUDGET - kernel.len() - fragment.len() - 2
}

/// The options of OpenVMM that take no value.
const FLAGS: [&str; 3] = [
    "--single-process",
    "--microvm-control-auth-stdin",
    "--restore-network-policy",
];

/// The kernel command-line tokens with which the sandbox profile selects itself and attaches the
/// distro layer of the tests on the first sandbox block device and the scratch disk on the
/// second.
pub(crate) const SANDBOX: [&str; 3] = [
    "nvx_sandbox=1",
    "nvx_layer=distro,0xd0003000,erofs:11111111-1111-1111-1111-111111111111",
    "nvx_scratch=0xd0006000,ext4",
];

/// The kernel command-line token with which a sandbox keeps its upper in RAM, in place of the
/// scratch disk's, [`SANDBOX`]'s last.
pub(crate) const RAM_UPPER: &str = "nvx_overlay_upper=ramfs";

/// The kernel command-line tokens of the default workload of a sandbox: `/bin/sh` as the
/// entrypoint, and `nvx-sandbox` as the hostname.
pub(crate) const WORKLOAD: [&str; 2] = ["nvx_entrypoint=/bin/sh", "nvx_hostname=nvx-sandbox"];

/// The options of an OpenVMM command line, each with its values in the order in which the
/// command line gives them, or with none for a flag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Options(BTreeMap<String, Vec<String>>);

impl Options {
    /// Returns the options of `arguments`, the arguments of OpenVMM after its program, every one
    /// of which is an option or the value of the option before it.
    pub(crate) fn parse(arguments: &[String]) -> Self {
        let mut options = Self::default();
        let mut iterator = arguments.iter();
        while let Some(name) = iterator.next() {
            assert!(
                name.starts_with("--"),
                "OpenVMM takes no positional argument {name:?}: {arguments:?}"
            );
            let values = options.0.entry(name.clone()).or_default();
            if !FLAGS.contains(&name.as_str()) {
                let value = iterator
                    .next()
                    .unwrap_or_else(|| panic!("{name} has no value: {arguments:?}"));
                values.push(value.clone());
            }
        }
        options
    }

    /// Returns these options with `name` given once with each of `values`, in order, in place of
    /// the values that it had, if any.
    pub(crate) fn with<S: Into<String>>(
        mut self,
        name: &str,
        values: impl IntoIterator<Item = S>,
    ) -> Self {
        self.0.insert(
            name.to_owned(),
            values.into_iter().map(Into::into).collect(),
        );
        self
    }

    /// Returns these options with the flag `name`.
    pub(crate) fn flag(self, name: &str) -> Self {
        self.with(name, Vec::<String>::new())
    }

    /// Returns these options with `tokens`, joined by spaces, as the kernel command line.
    pub(crate) fn kernel<S: AsRef<str>>(self, tokens: &[S]) -> Self {
        let tokens: Vec<&str> = tokens.iter().map(AsRef::as_ref).collect();
        self.with("--cmdline", [tokens.join(" ")])
    }

    /// Returns these options with the kernel command line of a sandbox: the user's `--cmdline`,
    /// `user`, if any, then the tokens that select the sandbox profile, then `tokens`, those of
    /// its workload, its limits, and its shares.
    pub(crate) fn sandbox_kernel<S: AsRef<str>>(self, user: &str, tokens: &[S]) -> Self {
        let mut line = user.to_owned();
        for token in SANDBOX
            .iter()
            .copied()
            .chain(tokens.iter().map(AsRef::as_ref))
        {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(token);
        }
        self.with("--cmdline", [line])
    }

    /// Returns these options of a sandbox with its upper in RAM: the distro layer of the root
    /// alone on a sandbox block device, and [`RAM_UPPER`] in place of the scratch disk's token on
    /// the kernel command line.
    pub(crate) fn in_ram(self) -> Self {
        let line = self.value("--cmdline").replace(SANDBOX[2], RAM_UPPER);
        self.with(
            "--microvm-sandbox-block",
            ["distro:file:{root/distro.erofs},ro"],
        )
        .with("--cmdline", [line])
    }

    /// Returns the values of `name`, none if the command line does not give it.
    pub(crate) fn values(&self, name: &str) -> &[String] {
        self.0.get(name).map_or(&[], Vec::as_slice)
    }

    /// Returns the value of `name`, which the command line must give once.
    pub(crate) fn value(&self, name: &str) -> &str {
        match self.values(name) {
            [value] => value,
            values => panic!("{name} has the values {values:?}, not one"),
        }
    }

    /// Removes `name`, and returns its values.
    pub(crate) fn take(&mut self, name: &str) -> Vec<String> {
        self.0.remove(name).unwrap_or_default()
    }

    /// Returns these options with the placeholders of each value replaced as `root` replaces
    /// them.
    pub(crate) fn expanded(&self, root: &Root) -> Self {
        Self(
            self.0
                .iter()
                .map(|(name, values)| {
                    (
                        name.clone(),
                        values.iter().map(|value| root.expand(value)).collect(),
                    )
                })
                .collect(),
        )
    }

    /// Asserts that `actual` has these options, with the placeholders of their values replaced as
    /// `root` replaces them, and no other, each with the same values in the same order.
    pub(crate) fn assert_matches(&self, root: &Root, actual: &Self, context: &str) {
        let expected = self.expanded(root);
        if expected == *actual {
            return;
        }
        let names: BTreeSet<&String> = expected.0.keys().chain(actual.0.keys()).collect();
        let mut differences = String::new();
        for name in names {
            let (wanted, got) = (expected.0.get(name), actual.0.get(name));
            if wanted != got {
                writeln!(differences, "  {name}: expected {wanted:?}, got {got:?}").unwrap();
            }
        }
        panic!("{context}: the OpenVMM options differ:\n{differences}");
    }

    /// Projects the old persistent-case inputs onto the CLI-owned managed boot contract.
    pub(crate) fn persistent_expected(mut self) -> Self {
        let identities = self.take("--microvm-workload-identity");
        let [identity] = identities.as_slice() else {
            panic!("a persistent fixture names one workload identity");
        };
        assert_eq!(self.take("--microvm-lifecycle"), ["managed"]);
        let line = self.value("--cmdline").to_owned();
        let tokens: Vec<_> = line.split_ascii_whitespace().collect();
        let hostname = tokens
            .iter()
            .find_map(|token| token.strip_prefix("nvx_hostname="))
            .unwrap();
        let mut kernel: Vec<String> = tokens
            .iter()
            .filter(|token| {
                token.starts_with("nvx_layer=")
                    || token.starts_with("nvx_scratch=")
                    || token.starts_with("nvx_overlay_upper=")
            })
            .map(|token| (*token).to_owned())
            .collect();
        kernel.push("nvx_overlay_mode=trusted-lowers".to_owned());
        kernel.push(
            if tokens.contains(&RAM_UPPER) {
                "nvx_ram_upper_percent=50"
            } else {
                "nvx_scratch_journal=allowed"
            }
            .to_owned(),
        );
        kernel.push(format!(
            "nvx_hostname={}",
            hostname
                .bytes()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        kernel.push("nvx_machine_id=generated".to_owned());
        kernel.extend(
            tokens
                .iter()
                .filter(|token| token.starts_with("nvx_maps="))
                .map(|token| (*token).to_owned()),
        );
        kernel.extend([
            "nvx_sandbox=1".to_owned(),
            "nvx_mode=managed".to_owned(),
            format!("nvx_identity={identity}"),
        ]);
        kernel.extend(
            tokens
                .iter()
                .filter(|token| !token.starts_with("nvx_"))
                .map(|token| (*token).to_owned()),
        );
        self.with("--processors", ["1"])
            .with("--guest-shutdown-action", ["exit"])
            .kernel(&kernel)
    }

    /// Projects an old cold-run fixture onto the checked public VM contract, independently of
    /// the CLI parser and its private token renderer. Restore and sandbox fixtures stay old.
    pub(crate) fn cold_run_expected(&self, root: &Root, mode: &str) -> nvx::Result<Self> {
        use nvx::activation::{MachineIdMode, View};
        use nvx::host::config::{Boot, ConsoleAttachment, NetworkConfig, RootConfig, VmConfig};
        use nvx::host::filesystem::{
            self, ExportRequest, Mapping, Owner, PathList, ReservedTargets, Target,
        };
        use nvx::host::network::{
            Access, EgressRule, EgressRules, HostLoopback, LoopbackForward, NetworkPolicy,
            NetworkProxy,
        };
        use nvx::host::runtime::{ArtifactIdentity, LaunchOptions, Runtime, RuntimeIdentity};
        use nvx::host::sandbox::mount::{MountAccess, parse_mount_value};
        use nvx::host::vm::{self, VmDir};

        use nvx::host::network::{ForwardProtocol, Protocol};

        let old = self.expanded(root);
        let mut caller = old.values("--cmdline").first().cloned().unwrap_or_default();
        let mut shares = Vec::new();
        let mut files = BTreeSet::new();
        for value in old.values("--mount") {
            let (target, host, access) = parse_mount_value(value)?;
            let host = PathBuf::from(host);
            let host = if host.is_absolute() {
                host
            } else {
                root.below(host.to_str().unwrap())
            };
            if host.is_file() {
                files.insert(target.clone());
            }
            shares.push((target, host, access));
        }
        let tokens: Vec<_> = caller.split_ascii_whitespace().map(str::to_owned).collect();
        let mut generated = Vec::new();
        for value in old.values("--mount-child") {
            let child = value.strip_suffix(",file").unwrap_or(value);
            let (name, host, access) = parse_mount_value(child)?;
            let token = tokens
                .iter()
                .find(|token| {
                    token
                        .strip_prefix("nvx_share=")
                        .is_some_and(|share| share.starts_with(&format!("{name},")))
                })
                .expect("the aggregate fixture names every child's target");
            let parts: Vec<_> = token
                .strip_prefix("nvx_share=")
                .unwrap()
                .split(',')
                .collect();
            let target = parts[1].to_owned();
            let host = PathBuf::from(host);
            let host = if host.is_absolute() {
                host
            } else {
                root.below(host.to_str().unwrap())
            };
            if value.ends_with(",file") {
                files.insert(target.clone());
            }
            shares.push((target, host, access));
            generated.push(token.clone());
        }
        for token in generated.iter().rev() {
            caller = if caller == *token {
                String::new()
            } else {
                caller
                    .strip_suffix(&format!(" {token}"))
                    .expect("fixture boot shares follow caller text")
                    .to_owned()
            };
        }
        let aggregate = shares.len() > 1 || !files.is_empty();
        let mut policies: [Vec<PathBuf>; 3] = Default::default();
        for (index, option) in ["--mount-deny", "--mount-allow", "--mount-write"]
            .into_iter()
            .enumerate()
        {
            for value in old.values(option) {
                let path = PathBuf::from(value);
                if shares.is_empty() || (aggregate && !path.is_absolute()) {
                    return Err(nvx::Error::unsupported(
                        "boot share policies need a mapped path and absolute aggregate paths",
                    ));
                }
                policies[index].push(if path.is_absolute() {
                    path
                } else {
                    shares[0].1.join(path)
                });
            }
        }
        let mappings: Vec<_> = shares
            .iter()
            .map(|(target, host, access)| Mapping {
                name: "--mount",
                path: host,
                target: Target::Guest(target),
                read_only: *access == MountAccess::ReadOnly,
            })
            .collect();
        let export = filesystem::plan(&ExportRequest {
            mappings: &mappings,
            denied: PathList {
                name: "--mount-deny",
                paths: &policies[0],
            },
            allowed: PathList {
                name: "--mount-allow",
                paths: &policies[1],
            },
            write: PathList {
                name: "--mount-write",
                paths: &policies[2],
            },
            owner: if old
                .values("--mount-owner")
                .first()
                .is_some_and(|value| value == "caller")
            {
                Owner::Caller
            } else {
                Owner::Vmm
            },
            reserved: ReservedTargets::BLOCKLESS_ROOT,
        })?;
        let mut boot_tokens = vec![format!("nvx_mode={mode}")];
        if let Some(export) = &export {
            boot_tokens.extend(export.guest_table().into_iter().map(|row| {
                let access = if row.read_only { "ro" } else { "rw" };
                let kind = if files.contains(&row.target) {
                    ",file"
                } else {
                    ""
                };
                format!("nvx_share={},{},{access}{kind}", row.source, row.target)
            }));
        }
        if !caller.is_empty() {
            boot_tokens.insert(0, caller);
        }
        if old.values("--net").is_empty()
            && [
                "--network-egress",
                "--network-ingress",
                "--network-egress-allow",
                "--network-egress-deny",
                "--host-loopback",
                "--host-loopback-forward",
                "--network-proxy",
            ]
            .into_iter()
            .any(|option| !old.values(option).is_empty())
        {
            return Err(nvx::Error::unsupported(
                "network policy options require --net",
            ));
        }
        let network = if let Some(net) = old.values("--net").first() {
            let access = |option, default| match old.values(option).first().map(String::as_str) {
                Some("allow") => Access::Allow,
                Some("deny") => Access::Deny,
                _ => default,
            };
            let mut policy = NetworkPolicy::new(access("--network-egress", Access::Allow));
            for (option, destination) in [
                ("--network-egress-allow", &mut policy.allow),
                ("--network-egress-deny", &mut policy.deny),
            ] {
                *destination = EgressRules {
                    name: option.to_owned(),
                    rules: old
                        .values(option)
                        .iter()
                        .map(|value| {
                            for (suffix, protocol) in [
                                (":tcp", Protocol::Tcp),
                                (":udp", Protocol::Udp),
                                (":icmp", Protocol::Icmp),
                            ] {
                                if let Some((destination, ports)) = value.split_once(suffix) {
                                    let rule = EgressRule::to(destination);
                                    return if ports.is_empty() {
                                        rule.on_protocol(protocol)
                                    } else {
                                        let ports = ports.strip_prefix(':').unwrap();
                                        if let Some((first, last)) = ports.split_once('-') {
                                            rule.on_port_range(
                                                protocol,
                                                first.parse().unwrap(),
                                                last.parse().unwrap(),
                                            )
                                        } else {
                                            rule.on_port(protocol, ports.parse().unwrap())
                                        }
                                    };
                                }
                            }
                            EgressRule::to(value)
                        })
                        .collect(),
                };
            }
            policy.ingress = access("--network-ingress", Access::Deny);
            policy.host_loopback = match old.values("--host-loopback").first().map(String::as_str) {
                Some("allow") => HostLoopback::Allow,
                Some("deny") => HostLoopback::Deny,
                _ => HostLoopback::Outbound,
            };
            policy.host_loopback_forwards = old
                .values("--host-loopback-forward")
                .iter()
                .map(|value| {
                    let fields: Vec<_> = value.split(':').collect();
                    LoopbackForward {
                        protocol: match fields[0] {
                            "tcp" => ForwardProtocol::Tcp,
                            "udp" => ForwardProtocol::Udp,
                            _ => panic!("a fixture names tcp or udp"),
                        },
                        host_port: fields[1].parse().unwrap(),
                        guest_port: fields[2].parse().unwrap(),
                    }
                })
                .collect();
            if let Some(proxy) = old.values("--network-proxy").first() {
                let (gateway, port) = proxy.rsplit_once(':').unwrap();
                assert_eq!(gateway, nvx::net::guest_gateway(net).unwrap().to_string());
                policy.proxy = Some(NetworkProxy::new(port.parse().unwrap()));
            }
            Some(NetworkConfig {
                guest_network: Some(nvx::net::Ipv4Cidr::parse(net).unwrap()),
                policy,
            })
        } else {
            None
        };
        let config = VmConfig {
            processors: old.value("--processors").parse().unwrap(),
            memory_mib: old.value("--memory").trim_end_matches('M').parse().unwrap(),
            root: RootConfig::Blockless,
            view: View {
                hostname: b"nvx".to_vec(),
                machine_id: MachineIdMode::Image,
            },
            network,
            filesystem: export,
            console: ConsoleAttachment::Listener,
            boot: Some(Boot {
                command_line: boot_tokens.join(" "),
                memory_capacity_mib: old
                    .values("--memory-capacity")
                    .first()
                    .map(|value| value.trim_end_matches('M').parse().unwrap()),
                capture: None,
            }),
        };
        config.check_command_line()?;
        let identity = ArtifactIdentity::new("test-fixture").unwrap();
        let runtime = Runtime {
            openvmm: root.below(OPENVMM),
            kernel: PathBuf::from(old.value("--kernel")),
            initramfs: PathBuf::from(old.value("--initrd")),
            identity: RuntimeIdentity::new(identity.clone(), identity.clone(), identity),
            hypervisor: old.value("--hypervisor").parse().unwrap(),
            cpu_profile: old.values("--cpu-profile").first().cloned(),
            windows_breakaway: false,
            options: LaunchOptions::default(),
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("expected-vm");
        vm::create_private_dir(&path)?;
        let prepared = vm::create(&runtime, &config, None, &VmDir::open(&path)?)?;
        let arguments: Vec<_> = prepared
            .arguments()
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect();
        let expected = Self::parse(&arguments).normalized_cold_outputs(false);
        drop(prepared);
        Ok(expected)
    }

    /// Checks fresh output names rather than making random endpoint values an assertion escape.
    pub(crate) fn normalized_cold_outputs(&self, removed: bool) -> Self {
        let mut options = self.clone();
        let report = PathBuf::from(options.value("--microvm-report"));
        assert!(report.is_absolute());
        assert_eq!(report.file_name().unwrap(), "outcome.json");
        let parent = report.parent().unwrap();
        if removed {
            let name = parent.file_name().unwrap().to_str().unwrap();
            let suffix = name
                .strip_prefix("nvx-run-")
                .expect("a cold CLI run owns a fresh VM directory");
            assert_eq!(suffix.len(), 32);
            assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert!(
                !parent.exists(),
                "the foreground run left its VM directory: {}",
                parent.display()
            );
        }
        let console = options
            .value("--virtio-console")
            .strip_prefix("listen=")
            .unwrap();
        let control = options
            .value("--microvm-control-console")
            .strip_prefix("listen=")
            .unwrap();
        assert_ne!(console, control);
        for (endpoint, name) in [(console, "console.sock"), (control, "control.sock")] {
            if cfg!(windows) {
                let suffix = endpoint.strip_prefix("//./pipe/openvmm-microvm-").unwrap();
                assert_eq!(suffix.len(), 32);
                assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
            } else {
                assert_eq!(Path::new(endpoint), parent.join(name));
            }
        }
        options = options
            .with("--virtio-console", ["listen=<console>"])
            .with("--microvm-control-console", ["listen=<control>"])
            .with("--microvm-report", ["<outcome>"]);
        options
    }

    /// Returns these options of a `sandbox run` with the outputs of its VM directory named as
    /// [`managed_run`] names them, once it has checked that the directory is a fresh one of the
    /// run, which is gone once the run has ended, and that it holds the control endpoint on POSIX
    /// hosts.
    pub(crate) fn normalized_run_outputs(&self) -> Self {
        let report = PathBuf::from(self.value("--microvm-report"));
        assert!(report.is_absolute(), "{}", report.display());
        assert_eq!(report.file_name().unwrap(), "outcome.json");
        let parent = report.parent().unwrap();
        let name = parent.file_name().unwrap().to_str().unwrap();
        let suffix = name
            .strip_prefix("nvx-sandbox-run-")
            .expect("a run owns a fresh VM directory");
        assert!(is_hex(suffix, 32), "{name}");
        assert!(
            !parent.exists(),
            "the run left its VM directory: {}",
            parent.display()
        );
        let control = self
            .value("--microvm-control-console")
            .strip_prefix("listen=")
            .unwrap();
        if cfg!(windows) {
            let pipe = control.strip_prefix("//./pipe/openvmm-microvm-").unwrap();
            assert!(is_hex(pipe, 32), "{control}");
        } else {
            assert_eq!(Path::new(control), parent.join("control.sock"));
        }
        self.clone()
            .with("--microvm-control-console", ["listen=<control>"])
            .with("--microvm-report", ["<outcome>"])
    }
}

/// Returns the options with which a one-shot run of the minimal sandbox, `sandbox run --layer
/// LAYER --scratch SCRATCH`, started OpenVMM, from which [`managed`] and [`managed_run`] derive
/// those of its managed launches.
pub(crate) fn one_shot() -> Options {
    Options::default()
        .with("--machine", ["microvm"])
        .with(
            "--microvm-sandbox-block",
            [
                "distro:file:{root/distro.erofs},ro",
                "scratch:file:{root/scratch.ext4}",
            ],
        )
        .with("--microvm-workload-identity", ["65534:65534"])
        .with("--microvm-lifecycle", ["one-shot"])
        .with("--virtio-console", ["console"])
        .flag("--single-process")
        .with("--hypervisor", [HYPERVISOR])
        .with("--memory", ["256M"])
        .with("--kernel", ["{root/build/vmlinux}"])
        .with("--initrd", ["{root/build/initramfs.cpio.gz}"])
        .sandbox_kernel("", &WORKLOAD)
}

/// Returns the options with which a start of the minimal sandbox in the state directory `state`,
/// below the root, starts OpenVMM with the control endpoint `endpoint`.
pub(crate) fn managed(state: &str, endpoint: &str) -> Options {
    one_shot()
        .with("--microvm-lifecycle", ["managed"])
        .with("--virtio-console", ["none"])
        .with("--microvm-control-console", [format!("listen={endpoint}")])
        .flag("--microvm-control-auth-stdin")
        .with(
            "--microvm-report",
            [format!("{{root/{state}/outcome.json}}")],
        )
}

/// Returns the options with which a run of the minimal sandbox, `sandbox run --layer LAYER
/// --scratch SCRATCH`, starts OpenVMM, in the form that [`Options::persistent_expected`] projects:
/// those of a start of the same sandbox, with the control endpoint and outcome report of the run's
/// own VM directory, as [`Options::normalized_run_outputs`] names them.
pub(crate) fn managed_run() -> Options {
    one_shot()
        .with("--microvm-lifecycle", ["managed"])
        .with("--virtio-console", ["none"])
        .with("--microvm-control-console", ["listen=<control>"])
        .flag("--microvm-control-auth-stdin")
        .with("--microvm-report", ["<outcome>"])
}

/// Returns the options with which `nvx-rs run` cold-boots its default guest, Alpine Linux, with
/// its boot console on the terminal.
pub(crate) fn cold_boot() -> Options {
    Options::default()
        .flag("--single-process")
        .with("--machine", ["microvm"])
        .with("--processors", ["1"])
        .with("--hypervisor", [HYPERVISOR])
        .with("--memory", ["128M"])
        .with("--kernel", ["{root/build/vmlinux}"])
        .with("--initrd", ["{root/build/initramfs.cpio.gz}"])
        .with("--virtio-console", ["console"])
}

/// Returns the name of the aggregate child at `index` that the guest binds at `target`: the
/// index, a hyphen, and the first 32 hexadecimal digits of the SHA-256 digest of the target, which
/// a snapshot pins.
pub(crate) fn child(index: usize, target: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(target.as_bytes());
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{index}-{hex}")
}

/// Returns the `nvx_share=` token that binds the aggregate child at `index` at `target`, with the
/// access `access`, `ro` or `rw`, and as a file if `file`.
pub(crate) fn share_token(index: usize, target: &str, access: &str, file: bool) -> String {
    let suffix = if file { ",file" } else { "" };
    format!(
        "nvx_share={},{target},{access}{suffix}",
        child(index, target)
    )
}

/// Asserts that `blocks`, the `--microvm-sandbox-block` values of a launch, attach the distro
/// and optional scratch at the stable caller-owned paths, not inherited descriptors.
pub(crate) fn assert_stable_blocks(root: &Root, blocks: &[String], scratch: bool) {
    let mut expected = vec![root.expand("distro:file:{root/distro.erofs},ro")];
    if scratch {
        expected.push(root.expand("scratch:file:{root/scratch.ext4}"));
    }
    let actual: Vec<_> = blocks
        .iter()
        .map(|block| block.replace("file:\\\\?\\", "file:"))
        .collect();
    assert_eq!(actual, expected);
}

/// Asserts that the fake OpenVMM of the launch whose records are named for `token` read the
/// distro layer and then the scratch image of the root, whole, through its sandbox block devices.
pub(crate) fn assert_opened_images(root: &Root, token: &str) {
    assert_opened(
        root,
        token,
        &[("distro", "distro\n"), ("scratch", "scratch\n")],
    );
}

/// Asserts that the fake OpenVMM of the launch whose records are named for `token`, whose upper is
/// in RAM, read the distro layer of the root alone, whole, through its sandbox block device.
pub(crate) fn assert_opened_distro(root: &Root, token: &str) {
    assert_opened(root, token, &[("distro", "distro\n")]);
}

/// Asserts that the fake OpenVMM of the launch whose records are named for `token` read the block
/// devices of `expected`, each with its role and content, whole, in order.
fn assert_opened(root: &Root, token: &str, expected: &[(&str, &str)]) {
    let opened = root.json(&format!("build/fake-openvmm-{token}-blocks.json"));
    let opened: Vec<(&str, &str)> = opened
        .as_array()
        .unwrap()
        .iter()
        .map(|block| {
            (
                block["role"].as_str().unwrap(),
                block["content"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(opened, expected);
}

/// What an outcome report of OpenVMM's says of a VM's network policy: its status, such as
/// `applied` or `not-requested`, its mode, such as `rules` or `none`, its numbers of allow and
/// deny rules, and its host loopback, `allow`, `deny`, or `none`.
pub(crate) type NetworkPolicy = (&'static str, &'static str, u64, u64, &'static str);

/// The network policy of an outcome report of a VM without a network.
pub(crate) const NO_NETWORK: NetworkPolicy = ("not-requested", "none", 0, 0, "none");

/// The mode with which the fake OpenVMM creates an outcome report, before the umask narrows it:
/// `OpenOptions`' default, which, unlike the 0600 of OpenVMM's own reports, has bits for the group
/// and others, so that the tests see how the umask that OpenVMM starts with narrows them.
#[cfg(target_os = "linux")]
pub(crate) const FAKE_REPORT_MODE: u32 = 0o666;

/// The steps of the teardown of a VM that an outcome report of OpenVMM's records, as callers
/// require them, each a member that is `true` once the step succeeded.
const TEARDOWN: [&str; 7] = [
    "control_channels_closed",
    "guest_workload_stopped",
    "network_released",
    "openvmm_process_terminated",
    "temporary_storage_removed",
    "virtiofs_released",
    "vm_stopped",
];

/// Asserts that `report` is an outcome report of OpenVMM's in its schema version 1: with an
/// `instance_id` of 32 hexadecimal digits, `backend` as its backend, an `outcome` of `operation`
/// with `category` and `status_code`, the network policy `network`, with the status code 0, and a
/// teardown whose every step succeeded.
pub(crate) fn assert_outcome_report(
    report: &Value,
    backend: &str,
    (operation, category, status_code): (&str, &str, i64),
    network: NetworkPolicy,
) {
    let members = |value: &Value| -> Vec<String> {
        let mut names: Vec<String> = value
            .as_object()
            .unwrap_or_else(|| panic!("not an object: {value}"))
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    };
    assert_eq!(
        members(report),
        [
            "backend",
            "instance_id",
            "network_policy",
            "outcome",
            "schema_version",
            "teardown"
        ],
        "{report}"
    );
    assert_eq!(report["schema_version"], 1, "{report}");
    assert!(
        report["instance_id"]
            .as_str()
            .is_some_and(|id| is_hex(id, 32)),
        "{report}"
    );
    assert_eq!(report["backend"], backend, "{report}");
    assert_eq!(
        report["outcome"],
        json!({"category": category, "operation": operation, "status_code": status_code}),
        "{report}"
    );
    let (status, mode, allow, deny, host_loopback) = network;
    assert_eq!(
        report["network_policy"],
        json!({
            "allow_rule_count": allow,
            "deny_rule_count": deny,
            "host_loopback": host_loopback,
            "mode": mode,
            "status": status,
            "status_code": 0,
        }),
        "{report}"
    );
    let teardown: serde_json::Map<String, Value> = TEARDOWN
        .iter()
        .map(|step| ((*step).to_owned(), Value::Bool(true)))
        .collect();
    assert_eq!(report["teardown"], Value::Object(teardown), "{report}");
}

/// Asserts that `endpoint` is the control endpoint of a managed start of the sandbox in the state
/// directory `state`, below the root: `STATE/control.sock` on POSIX hosts, and a named pipe of
/// OpenVMM's on Windows.
pub(crate) fn assert_endpoint(root: &Root, state: &str, endpoint: &str) {
    if cfg!(windows) {
        let name = endpoint
            .strip_prefix("//./pipe/openvmm-microvm-")
            .unwrap_or_else(|| panic!("not a named pipe of OpenVMM's: {endpoint}"));
        assert!(is_hex(name, 32), "{endpoint}");
    } else {
        assert_eq!(
            endpoint,
            root.expand(&format!("{{root/{state}/control.sock}}"))
        );
    }
}
