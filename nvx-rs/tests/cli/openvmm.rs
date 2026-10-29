//! OpenVMM command lines: their options, each with its values, and the options with which the
//! launches of `nvx-rs` start OpenVMM, as its documentation describes them, from which the tests
//! derive what they expect, and the checks of the images that a launch hands OpenVMM, of the
//! outcome reports that OpenVMM writes, and of the control endpoints of managed starts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use serde_json::{Value, json};

use crate::harness::{HYPERVISOR, Root, is_hex};

/// The options of OpenVMM that take no value.
const FLAGS: [&str; 2] = ["--single-process", "--microvm-control-auth-stdin"];

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
}

/// Returns the options with which a one-shot run of the minimal sandbox, `sandbox run --layer
/// LAYER --scratch SCRATCH`, starts OpenVMM, with the paths of the images that a dry run shows.
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

/// Returns the options with which `nvx-rs run` cold-boots its default guest, Alpine Linux.
pub(crate) fn cold_boot() -> Options {
    Options::default()
        .flag("--single-process")
        .with("--machine", ["microvm"])
        .with("--processors", ["1"])
        .with("--hypervisor", [HYPERVISOR])
        .with("--memory", ["128M"])
        .with("--kernel", ["{root/build/vmlinux}"])
        .with("--initrd", ["{root/build/initramfs.cpio.gz}"])
}

/// Returns the options with which `nvx-rs run --restore-snapshot {root/snapshot}` restores the
/// snapshot, without a guest kernel or initramfs.
pub(crate) fn restore() -> Options {
    Options::default()
        .flag("--single-process")
        .with("--machine", ["microvm"])
        .with("--processors", ["1"])
        .with("--hypervisor", [HYPERVISOR])
        .with("--restore-snapshot", ["{root/snapshot}"])
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
/// layer of the root read-only and then its scratch image through files that the launch checked:
/// on Linux, descriptors that OpenVMM inherits, which it names as `/proc/self/fd/N`, and on
/// Windows, the images' final paths.
pub(crate) fn assert_checked_blocks(root: &Root, blocks: &[String]) {
    let [distro, scratch] = blocks else {
        panic!("not two sandbox block devices: {blocks:?}");
    };
    let distro = checked_distro(distro);
    let scratch = scratch
        .strip_prefix("scratch:file:")
        .unwrap_or_else(|| panic!("not a scratch disk: {scratch}"));
    if cfg!(windows) {
        assert_eq!(
            distro,
            format!(r"\\?\{}", root.expand("{root/distro.erofs}"))
        );
        assert_eq!(
            scratch,
            format!(r"\\?\{}", root.expand("{root/scratch.ext4}"))
        );
    } else {
        assert_ne!(descriptor(distro), descriptor(scratch));
    }
}

/// Asserts that `blocks`, the `--microvm-sandbox-block` values of a launch whose upper is in RAM,
/// attach the distro layer of the root alone, read-only, through a file that the launch checked,
/// as [`assert_checked_blocks`] says.
pub(crate) fn assert_checked_distro(root: &Root, blocks: &[String]) {
    let [distro] = blocks else {
        panic!("not one sandbox block device: {blocks:?}");
    };
    let distro = checked_distro(distro);
    if cfg!(windows) {
        assert_eq!(
            distro,
            format!(r"\\?\{}", root.expand("{root/distro.erofs}"))
        );
    } else {
        descriptor(distro);
    }
}

/// Returns the checked file of `block`, a `--microvm-sandbox-block` value that attaches a
/// read-only distro layer.
fn checked_distro(block: &str) -> &str {
    block
        .strip_prefix("distro:file:")
        .and_then(|rest| rest.strip_suffix(",ro"))
        .unwrap_or_else(|| panic!("not a read-only distro layer: {block}"))
}

/// Returns the number of the descriptor that `path`, `/proc/self/fd/N`, names.
fn descriptor(path: &str) -> u32 {
    path.strip_prefix("/proc/self/fd/")
        .and_then(|number| number.parse::<u32>().ok())
        .unwrap_or_else(|| panic!("not an inherited descriptor: {path}"))
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
