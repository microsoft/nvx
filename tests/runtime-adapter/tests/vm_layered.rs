//! The host program launches VMs with a layered root, which the guest agent's worker assembles
//! through `nvx::guest::root` in a process of its own, never PID 1, on a real hypervisor, through
//! public APIs only.
//!
//! Each scenario of the table that [`scenarios!`] declares is an ignored test, which launches a VM
//! with a [`Root`] and checks what it [`Expect`]s: a root that the guest assembles, which the
//! scenario probes, or runs executions in through the host program's `exec`, and the stop tears
//! down, or the stage of the root that fails. A new scenario is a new row, with its own checks,
//! through the probes of the worker or executions.
//!
//! The tests boot VMs, so they are ignored. Besides what the `common` module lists, they read the
//! fixtures that NVX's own layered scenarios use, from NVX's artifacts in `NVX_ARTIFACTS_DIR`:
//!
//! - the Ubuntu EROFS image, `ubuntu-distro.erofs`, and its manifest, which holds its UUID;
//! - the scratch template, `ubuntu-smoke-scratch.ext4`, an empty ext4 file system that NVX's CI
//!   makes with the guest artifacts, of which each scenario boots a copy.
//!
//! A scenario of a GPT disk writes the disk first, from the scratch template: its first
//! partition, opaque bytes, then a copy of the template, the base layer, and a small layer that the
//! test writes, the top one, which holds a file and a directory. The test writes it in the ext2
//! format, which the kernel's ext4 driver mounts, since `mkfs.ext4` is not available everywhere
//! that the tests run. One scenario corrupts that layer, whose superblock then asks the kernel to
//! panic on an error, as [`corrupt`] describes. Others give it a cyclic journal, with and without
//! the file system's `needs_recovery` bit, as [`add_cyclic_journal`] describes.
//!
//! ```text
//! cargo test --locked --features host --test vm_layered -- --ignored
//! ```
#![cfg(feature = "host")]

mod common;

use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use nvx::gpt::{Disk, Expected, ExpectedPartition};
use nvx::host::artifacts::{ArtifactDir, Artifacts, Guest};
use nvx::uuid::Uuid;
use runtime_adapter::layered::RECORD_NAME;
use runtime_adapter::root::{DEFAULT_RUNTIME, JournalPolicy, OverlayMode, RootConfig, RootStage};
use serde_json::{Value, json};

use common::{Printed, Sandbox};

/// The scratch template among NVX's artifacts.
const SCRATCH_TEMPLATE: &str = "ubuntu-smoke-scratch.ext4";
/// The exit status of the host program's command whose guest reported that a stage of the root
/// failed.
const ROOT_FAILED: i32 = 4;
/// The size of the guest's pages, in bytes.
const PAGE: u64 = 4096;
/// How many of the bytes available in an upper a file that fills it may leave, which the file's
/// own blocks of metadata take.
const FULL_SLACK: u64 = 1 << 20;
/// The size of a sector of a disk, its logical block, in bytes.
const SECTOR: u64 = 512;
/// The disk GUID of the scenarios' GPT disks.
const DISK_GUID: &str = "6c1f5e0a-2b7d-4c39-9e81-3a5d0f7b2c64";
/// The unique GUIDs of the partitions of the scenarios' GPT disks, the first partition's first.
const PARTITION_GUIDS: [&str; 3] = [
    "0b7e4a2c-5d16-4f83-a9c0-1e2d3f4a5b61",
    "0b7e4a2c-5d16-4f83-a9c0-1e2d3f4a5b62",
    "0b7e4a2c-5d16-4f83-a9c0-1e2d3f4a5b63",
];
/// The type GUID of every partition of the scenarios' GPT disks: Linux's file system data.
const LINUX_DATA: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";
/// The size of the first partition of the scenarios' GPT disks, in bytes.
const FIRST_PARTITION_SIZE: u64 = 4096;
/// The bytes of the first partition that the guest returns: bytes that are not UTF-8, with a NUL
/// and whitespace among them, but not at their end.
const FIRST_PARTITION: &[u8] = b"\xff\xferuntime-adapter\0\t\x80\xc3";
/// What the first partition holds after [`FIRST_PARTITION`], which the guest trims as the padding
/// that ends its bytes, as it trims the NUL bytes that fill the rest of the partition.
const FIRST_PARTITION_PADDING: &[u8] = b" \0\n\t\r\x0c\0 ";
/// The first sector of each partition of the scenarios' GPT disks is a multiple of this many: 1
/// MiB, as partitioning tools align them.
const ALIGNMENT: u64 = 2048;
/// The regular file that the top layer of the scenarios' GPT disks holds.
const LAYER_FILE: &str = "layer-file";
/// The size of [`LAYER_FILE`], in bytes, which spans three of its layer's blocks.
const LAYER_FILE_SIZE: usize = 2500;
/// The directory that the top layer of the scenarios' GPT disks holds.
const LAYER_DIRECTORY: &str = "layer-directory";
/// The size of a block of the layer that [`small_layer`] writes, in bytes.
const LAYER_BLOCK: usize = 1024;
/// The number of blocks of that layer: 128 KiB.
const LAYER_BLOCKS: usize = 128;
/// The number of inodes of that layer, of which the first ten are reserved, as in every ext2 file
/// system, and the second is its top directory.
const LAYER_INODES: usize = 16;
/// The identity, besides root's, as which the scenarios' executions run: the image's `nobody`, of
/// the group `nogroup`, which NVX's Ubuntu image always holds.
const NOBODY: (u32, u32) = (65534, 65534);
/// An identity whose account the image lacks.
const ACCOUNTLESS: (u32, u32) = (4242, 4242);
/// Linux's error number of an operation that is not permitted.
const EPERM: i32 = 1;
/// Linux's error number of an invalid argument.
const EINVAL: i32 = 22;
/// The number of executions that meet only while all of them run.
const MEETING: usize = 4;
/// A script that prints its process's user ID, group ID, and groups, and its permitted,
/// effective, and bounding capability sets, which the shell reads itself.
const IDENTITY_SCRIPT: &str = "id -u; id -g; id -G; \
    while read -r key value; do \
    case $key in CapPrm:|CapEff:|CapBnd:) echo \"$key $value\";; esac; \
    done </proc/self/status";

/// The fixtures of the scenarios, among NVX's artifacts.
struct Fixtures {
    /// NVX's Ubuntu EROFS image.
    distro: PathBuf,
    /// The image's UUID, from its manifest.
    identity: String,
    /// The scratch template.
    scratch: PathBuf,
}

impl Fixtures {
    /// Finds the fixtures among NVX's artifacts in `NVX_ARTIFACTS_DIR`.
    fn find() -> Self {
        let directory = env::var_os(Artifacts::ENV_DIR)
            .unwrap_or_else(|| panic!("{} must name NVX's artifacts", Artifacts::ENV_DIR));
        let artifacts = ArtifactDir::from_dir(directory).unwrap();
        let distro = artifacts.distro_layer(Guest::Ubuntu).unwrap();
        let manifest = artifacts.distro_manifest(Guest::Ubuntu).unwrap();
        let scratch = artifacts.artifact(SCRATCH_TEMPLATE);
        for (path, fixture) in [
            (&distro, "the Ubuntu EROFS image"),
            (&manifest, "the manifest of the Ubuntu EROFS image"),
            (
                &scratch,
                "the scratch template, an empty ext4 file system, which `mkfs.ext4` makes",
            ),
        ] {
            assert!(path.is_file(), "{fixture} is missing: {}", path.display());
        }
        let manifest: Value = serde_json::from_str(&fs::read_to_string(&manifest).unwrap())
            .expect("the manifest is JSON");
        let identity = manifest["uuid"]
            .as_str()
            .expect("the manifest holds the image's UUID")
            .to_owned();
        Self {
            distro,
            identity,
            scratch,
        }
    }
}

/// The image of a scenario's root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Image {
    /// NVX's Ubuntu EROFS image.
    Erofs,
    /// A GPT disk that the scenario writes; see the [module documentation](self).
    Gpt,
}

/// The top layer of a scenario's GPT disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopLayer {
    /// A [`small_layer`] that holds [`LAYER_FILE`] and [`LAYER_DIRECTORY`].
    Sound,
    /// That layer, corrupt: its top directory's inode has no mode, an error that the kernel finds
    /// when it mounts the layer, and its superblock asks the kernel to panic on an error.
    Corrupt,
    /// The layer with a cyclic journal, but no `needs_recovery` bit.
    CyclicJournal,
    /// The same journal, with `needs_recovery` set, which the assembly must refuse.
    NeedsRecovery,
}

/// The identity that a scenario's launch expects of the image.
#[derive(Clone, Copy, Debug)]
enum Identity {
    /// The image's own, from its manifest.
    Own,
    /// Another, which the image does not have.
    Other,
}

/// The upper of a scenario's root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Upper {
    /// A copy of the scratch template.
    Scratch,
    /// RAM, without a scratch.
    Ram,
}

/// The layered root of a scenario's launch.
#[derive(Clone, Copy, Debug)]
struct Root {
    /// The image.
    image: Image,
    /// The top layer of a GPT disk.
    top_layer: TopLayer,
    /// The identity that the launch expects of the image.
    identity: Identity,
    /// The upper.
    upper: Upper,
    /// The guest's runtime directory, as the launch passes it, unless the default.
    runtime: Option<&'static str>,
    /// The overlay's mode, as the launch names it, unless the guest's default.
    overlay: Option<OverlayMode>,
    /// The scratch's journal policy, as the launch names it, unless the guest's default.
    journal: Option<JournalPolicy>,
    /// The bound of the first partition, as the launch names it, unless the guest's default.
    first_partition_bound: Option<u64>,
    /// The sandbox's workload identity, its user ID and its group ID, as the launch names it, if
    /// any.
    workload: Option<(u32, u32)>,
}

/// The root of the scenarios unless they say otherwise: NVX's EROFS image, with its own identity,
/// over a copy of the scratch template, with the guest's defaults.
const ROOT: Root = Root {
    image: Image::Erofs,
    top_layer: TopLayer::Sound,
    identity: Identity::Own,
    upper: Upper::Scratch,
    runtime: None,
    overlay: None,
    journal: None,
    first_partition_bound: None,
    workload: None,
};

/// The root of the scenarios of a GPT disk unless they say otherwise: the disk, with its own GUID,
/// over a copy of the scratch template, with the guest's defaults.
const GPT: Root = Root {
    image: Image::Gpt,
    ..ROOT
};

/// The image file of a scenario's launch.
struct Distro {
    /// The file.
    path: PathBuf,
    /// The image's format, as the launch names it.
    format: &'static str,
    /// The identity that the launch expects of the image.
    identity: String,
}

impl Root {
    /// Returns the image file of the launch, from the `fixtures`, which writes a GPT disk into
    /// `directory` first.
    fn distro(&self, fixtures: &Fixtures, directory: &Path) -> Distro {
        let (path, format, own) = match self.image {
            Image::Erofs => (fixtures.distro.clone(), "erofs", fixtures.identity.clone()),
            Image::Gpt => {
                let path = directory.join("distro.gpt");
                write_gpt_disk(&path, &fixtures.scratch, self.top_layer);
                (path, "gpt", DISK_GUID.to_owned())
            }
        };
        let identity = match self.identity {
            Identity::Own => own,
            Identity::Other => other_identity(&own),
        };
        Distro {
            path,
            format,
            identity,
        }
    }

    /// Returns the launch's options of the root, with the image `distro`, and the `scratch` image
    /// for a root over a scratch.
    fn options(&self, distro: &Distro, scratch: &Path) -> Vec<OsString> {
        let mut image = OsString::from(format!("{}:", distro.format));
        image.push(&distro.path);
        let upper = match self.upper {
            Upper::Scratch => {
                let mut upper = OsString::from("scratch:");
                upper.push(scratch);
                upper
            }
            Upper::Ram => "ram".into(),
        };
        let mut options = [
            "--distro".into(),
            image,
            "--distro-identity".into(),
            distro.identity.clone().into(),
            "--upper".into(),
            upper,
        ]
        .to_vec();
        if let Some(runtime) = self.runtime {
            options.extend(["--runtime-dir".into(), runtime.into()]);
        }
        if let Some(mode) = self.overlay {
            options.extend(["--overlay-mode".into(), mode.as_str().into()]);
        }
        if let Some(policy) = self.journal {
            options.extend(["--scratch-journal".into(), policy.as_str().into()]);
        }
        if let Some(bound) = self.first_partition_bound {
            options.extend(["--first-partition-bound".into(), bound.to_string().into()]);
        }
        if let Some((uid, gid)) = self.workload {
            options.extend(["--workload-identity".into(), format!("{uid}:{gid}").into()]);
        }
        options
    }

    /// Returns the overlay's mode, the guest's default unless the launch names another.
    fn overlay_mode(&self) -> OverlayMode {
        self.overlay.unwrap_or(OverlayMode::TrustedLowers)
    }
}

/// Returns a UUID other than `identity`, which differs in its last digit.
fn other_identity(identity: &str) -> String {
    let (start, last) = identity.split_at(identity.len() - 1);
    format!("{start}{}", if last == "0" { "1" } else { "0" })
}

/// What a scenario expects of its VM.
enum Expect {
    /// The guest assembles the root, the check probes the VM, and the stop tears the root down,
    /// which leaves a scratch unmounted.
    Ready(fn(&mut Vm<'_>)),
    /// The guest assembles the root, the check keeps it busy, and the stop reports the teardown
    /// that failed with this problem, which leaves a scratch mounted when the VM ends.
    TeardownFails(fn(&mut Vm<'_>), &'static str),
    /// The guest reports that the stage failed with this problem, and the VM ends with its
    /// status, without a scratch mounted: the guest fails each stage but the identity's before it
    /// mounts the scratch, and unmounts the root that it assembled once the identity's failed.
    Fails(RootStage, &'static str),
}

/// A scenario: a VM with a layered root, and what the scenario expects of it.
struct Scenario {
    /// The root of the VM.
    root: Root,
    /// What the scenario expects.
    expect: Expect,
}

/// A VM of a scenario, whose guest assembled its root.
struct Vm<'a> {
    /// The scenario's sandbox.
    sandbox: &'a mut Sandbox,
    /// What the launch printed.
    launched: Printed,
    /// The guest's runtime directory, below which it assembled the root.
    runtime: &'static str,
    /// The scenario's root.
    root: Root,
    /// The root configuration that the launch sent.
    config: RootConfig,
}

impl Vm<'_> {
    /// Runs `probes` in the VM's root, and returns what they found, each fact as `PROBE.KEY`.
    fn probe(&mut self, probes: &[&str]) -> Printed {
        let probes = probes.iter().map(OsStr::new).collect::<Vec<_>>();
        let printed = self.sandbox.run("probe", &probes, 0);
        assert_eq!(printed["reattached"], "live");
        assert_eq!(printed["pid"], self.launched["pid"]);
        printed
    }

    /// Runs `executions` at once in the VM's root, each the options and the arguments of one
    /// execution of the host program's `exec`, and returns what they did, each fact as
    /// `exec.N.KEY`, for execution N, from 1.
    fn exec(&mut self, executions: &[Vec<String>]) -> Printed {
        let mut options = Vec::new();
        for (index, execution) in executions.iter().enumerate() {
            if index > 0 {
                options.push(OsStr::new(";"));
            }
            options.extend(execution.iter().map(OsStr::new));
        }
        let printed = self.sandbox.run("exec", &options, 0);
        assert_eq!(printed["reattached"], "live");
        assert_eq!(printed["pid"], self.launched["pid"]);
        printed
    }
}

/// Declares the table of scenarios: each row, `NAME: SCENARIO;`, is an ignored test, `NAME`, which
/// runs `SCENARIO`.
macro_rules! scenarios {
    ($($(#[doc = $doc:literal])* $name:ident: $scenario:expr;)*) => {
        $(
            $(#[doc = $doc])*
            #[test]
            #[ignore = "boots a VM; see the module documentation for what it needs"]
            fn $name() {
                run(stringify!($name), $scenario);
            }
        )*
    };
}

scenarios! {
    /// An EROFS image over an ext4 scratch, of the identity that the launch expects: the merged
    /// root's contents and mounts, a copy-up of metadata alone, the guest's disks, the agent's and
    /// the workload's cgroups, PID 1's unrelated child, and a clean teardown.
    erofs_over_scratch: Scenario {
        root: ROOT,
        expect: Expect::Ready(check_erofs_over_scratch),
    };
    /// The image over the scratch, in the untrusted-lowers mode, with a journal that the launch
    /// allows: a copy-up of the data too, and a rename of a lower directory that fails.
    erofs_over_scratch_with_untrusted_lowers: Scenario {
        root: Root {
            overlay: Some(OverlayMode::UntrustedLowers),
            journal: Some(JournalPolicy::Allowed),
            ..ROOT
        },
        expect: Expect::Ready(check_root),
    };
    /// The image over a tmpfs in RAM, of the kernel's default size, without a scratch device.
    erofs_over_ram: Scenario {
        root: Root { upper: Upper::Ram, ..ROOT },
        expect: Expect::Ready(check_root),
    };
    /// The image over a tmpfs in RAM, in the untrusted-lowers mode.
    erofs_over_ram_with_untrusted_lowers: Scenario {
        root: Root {
            upper: Upper::Ram,
            overlay: Some(OverlayMode::UntrustedLowers),
            ..ROOT
        },
        expect: Expect::Ready(check_root),
    };
    /// A GPT disk over an ext4 scratch, of the disk GUID that the launch expects: its first
    /// partition, intact; its layers, each mounted read-only from its partition, with partition
    /// 2's at the overlay's base and partition 3's at its top; a copy-up of metadata alone, the
    /// rename of a lower directory, and the guest's disks.
    gpt_over_scratch: Scenario {
        root: GPT,
        expect: Expect::Ready(check_root),
    };
    /// The disk over the scratch, in the untrusted-lowers mode: a copy-up of the data too, and a
    /// rename of a lower directory that fails.
    gpt_over_scratch_with_untrusted_lowers: Scenario {
        root: Root {
            overlay: Some(OverlayMode::UntrustedLowers),
            ..GPT
        },
        expect: Expect::Ready(check_root),
    };
    /// The disk over a tmpfs in RAM, without a scratch device, with a bound that the first
    /// partition meets exactly.
    gpt_over_ram: Scenario {
        root: Root {
            upper: Upper::Ram,
            first_partition_bound: Some(FIRST_PARTITION_SIZE),
            ..GPT
        },
        expect: Expect::Ready(check_root),
    };
    /// The disk over a tmpfs in RAM, in the untrusted-lowers mode.
    gpt_over_ram_with_untrusted_lowers: Scenario {
        root: Root {
            upper: Upper::Ram,
            overlay: Some(OverlayMode::UntrustedLowers),
            ..GPT
        },
        expect: Expect::Ready(check_root),
    };
    /// A scratch that a file fills, which then refuses writes until the file is gone.
    full_scratch_refuses_writes: Scenario {
        root: ROOT,
        expect: Expect::Ready(check_full_upper),
    };
    /// A tmpfs that a file fills, at half of the guest's memory, which then refuses writes until
    /// the file is gone.
    full_ram_upper_refuses_writes: Scenario {
        root: Root { upper: Upper::Ram, ..ROOT },
        expect: Expect::Ready(check_full_upper),
    };
    /// A scratch with a journal, which the launch forbids, and the assembly refuses.
    forbidden_journal_fails_the_assembly: Scenario {
        root: Root { journal: Some(JournalPolicy::Forbidden), ..ROOT },
        expect: Expect::Fails(
            RootStage::Assembly,
            "has an ext4 journal, which the assembly forbids",
        ),
    };
    /// An image of another identity than the launch expects, which the assembly refuses.
    other_identity_fails_the_assembly: Scenario {
        root: Root { identity: Identity::Other, ..ROOT },
        expect: Expect::Fails(RootStage::Assembly, "the distro layer's UUID is "),
    };
    /// A GPT disk of another GUID than the launch expects, which the assembly refuses.
    other_disk_guid_fails_the_assembly: Scenario {
        root: Root { identity: Identity::Other, ..GPT },
        expect: Expect::Fails(
            RootStage::Assembly,
            "invalid partition table: the disk GUID is 6c1f5e0a-2b7d-4c39-9e81-3a5d0f7b2c64, not \
             the expected ",
        ),
    };
    /// A first partition one byte larger than the launch's bound, which the assembly refuses.
    first_partition_over_its_bound_fails_the_assembly: Scenario {
        root: Root { first_partition_bound: Some(FIRST_PARTITION_SIZE - 1), ..GPT },
        expect: Expect::Fails(
            RootStage::Assembly,
            "the first partition has 4096 bytes, more than the bound of 4095",
        ),
    };
    /// A GPT disk whose top layer is corrupt and asks the kernel to panic on an error, which the
    /// kernel refuses to mount, without a panic, and the assembly with it.
    corrupt_layer_fails_the_assembly: Scenario {
        root: Root { top_layer: TopLayer::Corrupt, ..GPT },
        expect: Expect::Fails(
            RootStage::Assembly,
            "failed to mount the layer in partition 3: ",
        ),
    };
    /// A cyclic journal must not hold up assembly, reads, copy-up, or teardown.
    cyclic_journal_is_not_loaded: Scenario {
        root: Root {
            top_layer: TopLayer::CyclicJournal,
            upper: Upper::Ram,
            overlay: Some(OverlayMode::UntrustedLowers),
            ..GPT
        },
        expect: Expect::Ready(check_root),
    };
    /// Suppressing journal loading must not let a layer that needs recovery mount.
    recovery_required_layer_fails_the_assembly: Scenario {
        root: Root { top_layer: TopLayer::NeedsRecovery, ..GPT },
        expect: Expect::Fails(
            RootStage::Assembly,
            "needs a journal recovery, which its read-only device can't take",
        ),
    };
    /// A relative runtime directory, which the guest refuses with the configuration.
    invalid_configuration_fails_its_stage: Scenario {
        root: Root { runtime: Some("run/runtime-adapter"), ..ROOT },
        expect: Expect::Fails(RootStage::Configuration, "which is not absolute"),
    };
    /// A root that a probe holds open, whose teardown fails.
    busy_root_fails_the_teardown: Scenario {
        root: ROOT,
        expect: Expect::TeardownFails(hold_root, "failed to unmount the overlay: "),
    };
    /// Host paths mapped beneath the assembled root, as an adapter maps them. Through the root:
    /// zero mappings, then a table in order, each with its access, `nosuid`, and `nodev`, on
    /// targets created on the overlay. Through mappers of the adapter's own: the refusal of a
    /// link and of a magic link at each component, of the wrong kinds, of a target that would be
    /// created inside another mapping, and of reserved targets, the runtime's, the container
    /// root's, and the adapter's own; and a read-only mapping that is never writable. The stop
    /// unmounts the root's mappings before the overlay.
    mappings_apply_beneath_the_root: Scenario {
        root: ROOT,
        expect: Expect::Ready(check_mappings),
    };
    /// A mapping through a symbolic link in the root, which the root refuses, and after which it
    /// refuses every mapping.
    a_refused_mapping_latches_the_root: Scenario {
        root: ROOT,
        expect: Expect::Ready(check_refused_mapping),
    };
    /// Executions that enter the root as root profiles do, through `nvx::guest::entry`: user ID
    /// 0 with each allowed capability alone, with none, and with every one in each publication
    /// state, and a numeric identity, each PID 1 of its namespaces, with the root moved over `/`,
    /// without the descriptor that its caller leaked or a signal that it blocks, and contained by
    /// a nested `chroot`. No
    /// working directory reached through a magic link starts a program. No root
    /// workload lowers the unprivileged port start or binds a privileged port while listeners are
    /// published, and the entry refuses the capabilities outside its allowlist. The root's own
    /// machine ID, then a generated one, with the time status that the guest's time daemon
    /// publishes, in which a root workload finds no lock of the daemon's, both of which the
    /// runtime binds in a mount namespace of its own.
    root_profiles_enter_the_root: Scenario {
        root: ROOT,
        expect: Expect::Ready(check_root_profiles),
    };
    /// Executions that the host program runs at once over one session, in a sandbox without a
    /// workload identity, so that each start names its own: output beyond 1 MiB, whole; several
    /// executions that meet only while all of them run; a descendant in a session of its own,
    /// which holds its program's output, and which the end of its program ends; a user other than
    /// root, and root with an allowed capability alone; a working directory, an exact
    /// environment, and the configured hostname, each as PID 1 of its namespaces. The guest
    /// refuses a start without an identity, and one with a capability outside the entry's
    /// allowlist, at stage 0.
    executions_run_in_the_root: Scenario {
        root: ROOT,
        expect: Expect::Ready(check_executions),
    };
    /// Executions in a sandbox whose workload identity the launch names, which every execution
    /// runs as: the guest refuses a start that names an identity of its own.
    executions_run_as_the_workload_identity: Scenario {
        root: Root { workload: Some(NOBODY), ..ROOT },
        expect: Expect::Ready(check_workload_executions),
    };
    /// A workload identity without an account in the image, which fails the identity stage once
    /// the guest assembled the root, before it answers the root configuration.
    accountless_workload_identity_fails_its_stage: Scenario {
        root: Root { workload: Some(ACCOUNTLESS), ..ROOT },
        expect: Expect::Fails(
            RootStage::Identity,
            "cannot enter the root as the workload identity 4242:4242: ",
        ),
    };
}

/// Runs the scenario `name`, `scenario`.
fn run(name: &str, scenario: Scenario) {
    let fixtures = Fixtures::find();
    let mut sandbox = Sandbox::new(name);
    let root = scenario.root;
    let scratch = sandbox.root.join("scratch.ext4");
    let over_scratch = root.upper == Upper::Scratch;
    if over_scratch {
        fs::copy(&fixtures.scratch, &scratch).unwrap();
    }
    let distro = root.distro(&fixtures, &sandbox.root);
    let options = root.options(&distro, &scratch);
    let options = options.iter().map(OsString::as_os_str).collect::<Vec<_>>();
    let (check, failure) = match scenario.expect {
        Expect::Fails(stage, problem) => {
            let launched = sandbox.launch(&options, ROOT_FAILED);
            assert_failed(&launched, stage, problem);
            assert_eq!(launched["stopped"], "graceful");
            // The launch recorded what it launched, with the root configuration that it sent, which
            // the guest refused, or accepted before a later stage failed.
            check_record(&sandbox, &root, &distro, &scratch);
            // The launch ended: no runtime record names the VM.
            assert_eq!(sandbox.record(), None);
            assert!(
                !(over_scratch && needs_recovery(&scratch)),
                "the guest left the scratch mounted"
            );
            return;
        }
        Expect::Ready(check) => (check, None),
        Expect::TeardownFails(check, problem) => (check, Some(problem)),
    };
    let launched = sandbox.launch(&options, 0);
    assert_eq!(launched["root"], "ready");
    let config = check_record(&sandbox, &root, &distro, &scratch);
    let vmm = sandbox
        .vmm()
        .expect("the VM runs after its launcher exited");
    let mut vm = Vm {
        sandbox: &mut sandbox,
        launched: launched.clone(),
        runtime: root.runtime.unwrap_or(DEFAULT_RUNTIME),
        root,
        config,
    };
    check(&mut vm);

    let status = if failure.is_some() { ROOT_FAILED } else { 0 };
    let stopped = sandbox.run("stop", &[], status);
    assert_eq!(stopped["reattached"], "live");
    assert_eq!(stopped["pid"], launched["pid"]);
    assert_eq!(stopped["stopped"], "graceful");
    assert!(vmm.has_exited().unwrap());
    assert_eq!(
        sandbox.record(),
        None,
        "the stop cleared the runtime record"
    );
    match failure {
        None => {
            assert!(!stopped.contains_key("root"), "{stopped:?}");
            assert_eq!(stopped["outcome"], "success 0");
            assert_eq!(stopped["teardown"], "complete");
            assert!(
                !(over_scratch && needs_recovery(&scratch)),
                "the stop left the scratch mounted"
            );
        }
        Some(problem) => {
            assert_failed(&stopped, RootStage::Teardown, problem);
            assert!(
                !over_scratch || needs_recovery(&scratch),
                "the failed teardown unmounted the scratch"
            );
        }
    }
}

/// Checks that a command printed that the guest reported that `stage` failed with `problem`, and
/// that the VM then ended with the stage's status and a complete teardown.
fn assert_failed(printed: &Printed, stage: RootStage, problem: &str) {
    assert_eq!(printed["root"], "failed", "{printed:?}");
    assert_eq!(printed["stage"], stage.as_str(), "{printed:?}");
    assert!(printed["problem"].contains(problem), "{printed:?}");
    let outcome = format!("guest-exit {}", stage.exit_status());
    assert_eq!(printed["outcome"], outcome, "{printed:?}");
    assert_eq!(printed["teardown"], "complete", "{printed:?}");
}

/// Checks what the launch recorded in the sandbox's store: a VM with the layered `root` of the
/// image `distro`, over `scratch` or RAM, the image of the identity that the launch expects, the
/// boot tokens of its plan, and the root configuration that the host sent, whether or not the
/// guest accepted it, which it returns.
fn check_record(sandbox: &Sandbox, root: &Root, distro: &Distro, scratch: &Path) -> RootConfig {
    let record = fs::read_to_string(sandbox.store.join(RECORD_NAME)).unwrap();
    let record: Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["format"], 1);
    assert_eq!(record["mode"], "layered");
    let plan = &record["plan"];
    assert_eq!(plan["distro"]["format"], distro.format);
    assert_eq!(plan["distro"]["identity"], distro.identity.as_str());
    let path = |value: &Value| PathBuf::from(value.as_str().unwrap());
    assert_eq!(path(&plan["distro"]["path"]), distro.path);
    let upper = match root.upper {
        Upper::Scratch => {
            assert_eq!(plan["upper"]["kind"], "scratch");
            assert_eq!(path(&plan["upper"]["path"]), scratch);
            "nvx_scratch=0xd0006000,ext4"
        }
        Upper::Ram => {
            assert_eq!(plan["upper"], json!({"kind": "ram"}));
            "nvx_overlay_upper=ramfs"
        }
    };
    let tokens = record["kernelCommandLine"]
        .as_str()
        .unwrap()
        .split(' ')
        .collect::<BTreeSet<_>>();
    let layer = format!(
        "nvx_layer=distro,0xd0003000,{}:{}",
        distro.format, distro.identity
    );
    for token in ["nvx_sandbox=1", &layer, upper] {
        assert!(tokens.contains(token), "{token} in {tokens:?}");
    }
    let uppers = tokens
        .iter()
        .filter(|token| {
            token.starts_with("nvx_scratch=") || token.starts_with("nvx_overlay_upper=")
        })
        .count();
    assert_eq!(uppers, 1, "{tokens:?}");
    let sectors = |path: &Path| Some(fs::metadata(path).unwrap().len() / 512);
    let config = RootConfig {
        runtime: root.runtime.unwrap_or(DEFAULT_RUNTIME).to_owned(),
        distro_sectors: sectors(&distro.path),
        scratch_sectors: (root.upper == Upper::Scratch)
            .then(|| sectors(scratch))
            .flatten(),
        first_partition_bound: root.first_partition_bound,
        overlay_mode: root.overlay,
        scratch_journal: root.journal,
        workload_identity: root.workload,
        ..RootConfig::default()
    };
    let sent = String::from_utf8(config.encode()).unwrap();
    assert_eq!(record["rootConfiguration"], sent.as_str());
    config
}

/// Returns whether the ext4 file system at `path` needs its journal recovered: whether a mount
/// left its superblock's `needs_recovery` feature set, which a clean unmount clears.
fn needs_recovery(path: &Path) -> bool {
    let mut superblock = [0; 1024];
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(1024)).unwrap();
    file.read_exact(&mut superblock).unwrap();
    let magic = u16::from_le_bytes([superblock[0x38], superblock[0x39]]);
    assert_eq!(
        magic,
        0xef53,
        "{} holds an ext4 file system",
        path.display()
    );
    let incompatible = u32::from_le_bytes(superblock[0x60..0x64].try_into().unwrap());
    incompatible & 0x4 != 0
}

/// Returns the options of a list of mount options, separated by commas.
fn options(list: &str) -> BTreeSet<&str> {
    list.split(',').collect()
}

/// Checks the root of a scenario's VM, which the guest assembled, as its image, its upper, and its
/// overlay mode make it: its mounts, its contents, its first partition, a copy-up, a rename, and
/// the guest's disks.
fn check_root(vm: &mut Vm<'_>) {
    let image = vm.root.image;
    let (lowers, file, directory) = match image {
        Image::Erofs => (&["distro"][..], "usr/lib/os-release", "mnt"),
        Image::Gpt => (&["p2", "p3"][..], LAYER_FILE, LAYER_DIRECTORY),
    };
    let copy_up = format!("copy-up {file}");
    let rename = format!("rename {directory}");
    let facts = vm.probe(&[
        "mounts",
        "contents",
        &copy_up,
        &rename,
        "devices",
        "first-partition",
    ]);
    let runtime = vm.runtime;
    let (upper, kind, other) = match vm.root.upper {
        Upper::Scratch => ("scratch", "ext4", "tmpfs"),
        Upper::Ram => ("tmpfs", "tmpfs", "scratch"),
    };

    // The guest's disks, whatever their names: the image's, read-only, and the scratch's,
    // writable, unless the upper is in RAM.
    let disks = facts["devices.disks"].split(' ').collect::<Vec<_>>();
    let mut sizes = disks
        .iter()
        .map(|disk| disk.split_once(':').map_or(*disk, |(_, size)| size))
        .collect::<Vec<_>>();
    sizes.sort_unstable();
    let mut expected = vec![format!("{}:ro", vm.config.distro_sectors.unwrap())];
    expected.extend(
        vm.config
            .scratch_sectors
            .map(|sectors| format!("{sectors}:rw")),
    );
    expected.sort_unstable();
    assert_eq!(sizes, expected, "{facts:?}");

    // The image's layers, read-only, under the overlay, whose upper and work directories are on
    // the upper, all once, below the runtime directory, where no other upper is mounted. The
    // layers of a GPT disk are ext4 file systems on its partitions, whose journals are not loaded.
    let layer = match image {
        Image::Erofs => ("erofs", &["ro"][..]),
        Image::Gpt => ("ext4", &["ro", "nosuid", "nodev"][..]),
    };
    let mounts = lowers
        .iter()
        .map(|lower| (*lower, layer.0, layer.1))
        .chain([
            (upper, kind, &["rw", "nosuid", "nodev"][..]),
            ("root", "overlay", &["rw", "nosuid", "nodev"]),
        ]);
    for (mount, kind, flags) in mounts {
        assert_eq!(facts[&format!("mounts.{mount}.count")], "1", "{mount}");
        assert_eq!(facts[&format!("mounts.{mount}.type")], kind, "{mount}");
        let found = options(&facts[&format!("mounts.{mount}.options")]);
        assert!(
            found.is_superset(&flags.iter().copied().collect()),
            "{mount}: {found:?}"
        );
    }
    assert_eq!(facts[&format!("mounts.{other}.count")], "0", "{other}");
    if image == Image::Gpt {
        let disk = disks
            .iter()
            .find_map(|disk| disk.strip_suffix(":ro"))
            .and_then(|disk| disk.split_once(':'))
            .map(|(name, _)| name)
            .expect("the guest has the disk's device");
        for lower in lowers {
            let partition = lower.trim_start_matches('p');
            let source = &facts[&format!("mounts.{lower}.source")];
            assert_eq!(source, &format!("/dev/{disk}{partition}"), "{lower}");
            let found = options(&facts[&format!("mounts.{lower}.super")]);
            assert!(
                found.contains("noload") || found.contains("norecovery"),
                "{lower}: {found:?}"
            );
        }
    }
    if vm.root.upper == Upper::Ram {
        // A tmpfs of the kernel's default size, whose root directory has mode 0755.
        let tmpfs = options(&facts["mounts.tmpfs.super"]);
        assert!(tmpfs.contains("mode=755"), "{tmpfs:?}");
        let sized = tmpfs.iter().any(|option| option.starts_with("size="));
        assert!(!sized, "{tmpfs:?}");
    }
    // The overlay stacks the layers with the base last, after the top.
    let stacked = lowers
        .iter()
        .rev()
        .map(|lower| format!("{runtime}/layers/{lower}"))
        .collect::<Vec<_>>()
        .join(":");
    let layers = [
        format!("lowerdir={stacked}"),
        format!("upperdir={runtime}/{upper}/upper"),
        format!("workdir={runtime}/{upper}/work"),
    ];
    let overlay = options(&facts["mounts.root.super"]);
    for option in &layers {
        assert!(overlay.contains(option.as_str()), "{option} in {overlay:?}");
    }

    // The overlay's mode. Trusting its lowers, the overlay copies up a file's metadata alone,
    // which leaves the data in its layer, and renames a lower directory, with the options that
    // the mode and metadata copy-up set. Otherwise, it copies up the data too, and refuses the
    // rename, with these options off.
    let trusting = ["metacopy=on", "redirect_dir=on", "xino=on"];
    let trusted = vm.root.overlay_mode() == OverlayMode::TrustedLowers;
    for option in trusting {
        assert_eq!(overlay.contains(option), trusted, "{option} in {overlay:?}");
    }
    let (data, rename) = if trusted {
        ("none", "allowed")
    } else {
        ("copied", "refused")
    };
    assert_eq!(facts["copy-up.upper.size"], facts["copy-up.lower.size"]);
    assert_eq!(facts["copy-up.upper.data"], data, "{facts:?}");
    assert_eq!(facts["copy-up.root.data"], "intact");
    assert_eq!(facts["rename.outcome"], rename, "{facts:?}");

    // The merged root: the image's files, and a file that the worker wrote, which reaches the
    // upper and not the top layer, which refuses writes. A GPT disk's layers hold no
    // `os-release`.
    let identifier = match image {
        Image::Erofs => "ubuntu",
        Image::Gpt => "absent",
    };
    assert_eq!(facts["contents.os-release.id"], identifier);
    assert_eq!(facts["contents.root.mode"], "755");
    assert_eq!(facts["contents.write.root"], "runtime-adapter");
    assert_eq!(facts["contents.write.upper"], "runtime-adapter");
    assert_eq!(facts["contents.write.lower"], "absent");
    assert_eq!(facts["contents.lower.write"], "refused");

    // The first partition of a GPT disk, which the assembly returned as it was, without its
    // padding.
    let (length, bytes) = match image {
        Image::Erofs => ("none".to_owned(), "none".to_owned()),
        Image::Gpt => (
            FIRST_PARTITION.len().to_string(),
            hexadecimal(FIRST_PARTITION),
        ),
    };
    assert_eq!(facts["first-partition.length"], length, "{facts:?}");
    assert_eq!(facts["first-partition.bytes"], bytes, "{facts:?}");
}

/// Fills the upper of a scenario's root through the root, and checks that the upper refuses
/// writes once full, until the file that filled it is gone.
fn check_full_upper(vm: &mut Vm<'_>) {
    let facts = vm.probe(&["fill"]);
    let number = |fact: &str| facts[&format!("fill.{fact}")].parse::<u64>().unwrap();
    assert_eq!(facts["fill.error"], "ENOSPC", "{facts:?}");
    let (available, written) = (number("available"), number("written"));
    assert!(
        written <= available && available - written < FULL_SLACK,
        "{facts:?}"
    );
    assert_eq!(facts["fill.after"], "runtime-adapter");
    if vm.root.upper == Upper::Ram {
        // A tmpfs of the kernel's default size: half of the guest's memory, in whole pages.
        let pages = number("memory") / PAGE;
        assert_eq!(number("total"), pages / 2 * PAGE, "{facts:?}");
    }
}

/// Checks the VM of the `erofs_over_scratch` scenario: its root, the agent's and the workload's
/// cgroups, and the guest's processes.
fn check_erofs_over_scratch(vm: &mut Vm<'_>) {
    check_root(vm);
    let facts = vm.probe(&["cgroups", "processes"]);

    // The agent's and the workload's cgroups, siblings in the cgroup v2 hierarchy, with the
    // configuration's resource values, and no user namespaces.
    let config = &vm.config;
    assert_eq!(facts["cgroups.parent.type"], "cgroup2");
    assert_eq!(facts["cgroups.agent.exists"], "yes");
    assert_eq!(facts["cgroups.workload.exists"], "yes");
    let values = [
        ("cgroups.agent.memory.low", config.agent_memory_low),
        ("cgroups.workload.memory.max", config.workload_memory_max),
        ("cgroups.workload.pids.max", config.workload_pids_max),
    ];
    for (fact, value) in values {
        assert_eq!(facts[fact], value.unwrap().to_string(), "{fact}");
    }
    assert_eq!(facts["cgroups.workload.processes"], "0");
    let user_namespaces = facts["cgroups.user-namespaces"].as_str();
    assert!(
        ["0", "unavailable"].contains(&user_namespaces),
        "{user_namespaces}"
    );

    // The worker is not PID 1, but its child. PID 1's unrelated child runs on, untouched: in PID
    // 1's root directory and namespaces, like the worker, and in the agent's cgroup, into which
    // the bootstrap moved every process of the guest.
    let agent = Path::new(&config.agent_cgroup);
    let parent = agent.parent().unwrap();
    let agent = format!("/{}", agent.strip_prefix(parent).unwrap().display());
    assert_ne!(facts["processes.worker.pid"], "1");
    assert_eq!(facts["processes.worker.ppid"], "1");
    assert_eq!(facts["processes.sibling.ppid"], "1");
    let state = facts["processes.sibling.state"].as_str();
    assert!(["S", "R"].contains(&state), "{state}");
    let commands = [
        ("pid1", "init /dev/hvc2 layered"),
        ("worker", "worker /dev/hvc2"),
        ("sibling", "idle"),
    ];
    for (process, command) in commands {
        let found = &facts[&format!("processes.{process}.command")];
        let expected = format!("/sbin/runtime-adapter-guest {command}");
        assert!(found.starts_with(&expected), "{process}: {found}");
        assert_eq!(
            facts[&format!("processes.{process}.root")],
            "/",
            "{process}"
        );
        let namespaces = &facts[&format!("processes.{process}.namespaces")];
        assert_eq!(namespaces, "same", "{process}");
        assert_eq!(
            facts[&format!("processes.{process}.cgroup")],
            agent,
            "{process}"
        );
    }
    let sibling = &facts["processes.sibling.pid"];
    assert!(
        facts["processes.worker.command"].ends_with(&format!(" {sibling}")),
        "{facts:?}"
    );
}

/// Keeps the root of the `busy_root_fails_the_teardown` scenario busy.
fn hold_root(vm: &mut Vm<'_>) {
    let facts = vm.probe(&["hold"]);
    assert_eq!(facts["hold.held"], format!("{}/rootfs", vm.runtime));
}

/// Checks the VM of the `mappings_apply_beneath_the_root` scenario.
fn check_mappings(vm: &mut Vm<'_>) {
    let facts = vm.probe(&["map", "map-checks"]);
    let fact = |key: &str| facts[&format!("map.{key}")].as_str();

    // Zero mappings do no work; a table applies in order.
    assert_eq!((fact("zero"), fact("zero.mounts")), ("applied", "0"));
    assert_eq!(fact("table"), "applied");
    assert_eq!(
        fact("mounts"),
        "/srv/maps/dir,/srv/maps/dir/inner,/srv/maps/file.txt,/srv/maps/out"
    );
    // Each mapping has its access, `nosuid`, and `nodev`, and writes reach the sources of the
    // writable ones only, the one inside a read-only mapping included.
    for (name, access) in [
        ("dir", "ro"),
        ("inner", "rw"),
        ("file", "ro"),
        ("out", "rw"),
    ] {
        let found = options(fact(&format!("{name}.options")));
        for flag in [access, "nosuid", "nodev"] {
            assert!(found.contains(flag), "{name}: {found:?}");
        }
    }
    assert_eq!((fact("dir.read"), fact("dir.write")), ("source", "refused"));
    assert_eq!((fact("file.read"), fact("file.write")), ("file", "refused"));
    for name in ["inner", "out"] {
        let written = (
            fact(&format!("{name}.write")),
            fact(&format!("{name}.source")),
        );
        assert_eq!(written, ("allowed", "runtime-adapter"), "{name}");
    }
    // The missing targets were created on the overlay, in its upper.
    assert_eq!(
        (fact("upper.directory"), fact("upper.file")),
        ("yes", "yes")
    );
    assert_eq!(fact("latched"), "no");

    let check = |key: &str| facts[&format!("map-checks.{key}")].as_str();
    // No link or magic link at any component leads a mapping anywhere.
    for key in [
        "link.a",
        "link.b",
        "link.new",
        "link.up",
        "magic.self",
        "magic.thread-self",
        "magic.root",
        "magic.cwd",
        "magic.fd",
    ] {
        assert_eq!(check(key), "link", "{key}");
    }
    assert_eq!(check("kind.directory"), "target-kind");
    assert_eq!(check("kind.file"), "target-kind");
    // A mapping may go onto what an earlier one shows, but creates nothing inside it.
    assert_eq!(check("nested.onto"), "applied");
    assert_eq!(check("nested.create"), "foreign /srv/checks/m/new");
    assert_eq!(check("nested.source"), "absent");
    assert_eq!(check("nested.teardown"), "complete");
    // The runtime's, the container root's, and the adapter's own reserved targets, and the
    // ancestors of reserved trees, but not what is below a reserved path.
    for key in [
        "run",
        "run-nvx",
        "time",
        "time-below",
        "proc",
        "sys",
        "dev",
        "agent",
        "machine-id",
        "etc",
        "adapter-tree",
        "adapter-ancestor",
        "adapter-path",
    ] {
        assert_eq!(check(&format!("reserved.{key}")), "reserved", "{key}");
    }
    assert_eq!(check("reserved.adapter-below"), "applied");
    assert_eq!(check("reserved.created"), "absent");
    // A read-only mapping is restricted before it is attached, so never writable.
    assert_eq!(check("read-only.attaches"), "200");
    assert_eq!(check("read-only.flags"), "ro,nosuid,nodev");
    assert_eq!(check("read-only.writable"), "never");
    assert_eq!(check("read-only.source"), "source");
    // Nothing went outside the root, and the checks unmounted what they mapped.
    assert_eq!(check("outside.entries"), "0");
    assert_eq!(check("outside.mounts"), "0");
    assert_eq!(check("left"), "0");
}

/// Checks the VM of the `a_refused_mapping_latches_the_root` scenario.
fn check_refused_mapping(vm: &mut Vm<'_>) {
    let facts = vm.probe(&["map-refused"]);
    let fact = |key: &str| facts[&format!("map-refused.{key}")].as_str();
    assert_eq!(fact("through-link"), "link /srv/link");
    assert_eq!(fact("latched"), "yes");
    assert_eq!(fact("later"), "latched");
    assert_eq!(fact("later.target"), "absent");
    assert_eq!(fact("mounts"), "0");
    assert_eq!(fact("outside"), "0");
}

/// Writes the GPT disk of the scenarios at `path`, as UEFI lays out a disk, with a protective MBR,
/// the primary GPT header and its array of 128 entries, and the backup array and header at the
/// disk's end, of its partitions in this order, each aligned to [`ALIGNMENT`]:
///
/// 1. the first partition, of [`FIRST_PARTITION_SIZE`] bytes, which holds [`FIRST_PARTITION`],
///    then [`FIRST_PARTITION_PADDING`], then NUL bytes;
/// 2. the base layer, a copy of the scratch template `template`, an ext4 file system that
///    `mkfs.ext4` made, with a journal;
/// 3. the top layer, a [`small_layer`] that holds [`LAYER_FILE`] and [`LAYER_DIRECTORY`], with
///    the corruption or journal that `top_layer` names.
///
/// Then checks the disk as the guest's assembly does, through `nvx::gpt`, with the GUIDs and
/// sizes that it wrote.
fn write_gpt_disk(path: &Path, template: &Path, top_layer: TopLayer) {
    let template_size = fs::metadata(template).unwrap().len();
    assert_eq!(
        template_size % SECTOR,
        0,
        "the scratch template is whole sectors"
    );
    let contents = b"runtime-adapter "
        .iter()
        .copied()
        .cycle()
        .take(LAYER_FILE_SIZE)
        .collect::<Vec<_>>();
    let mut top = small_layer(&[(LAYER_FILE, contents.as_slice())], &[LAYER_DIRECTORY]);
    match top_layer {
        TopLayer::Sound => {}
        TopLayer::Corrupt => corrupt(&mut top),
        TopLayer::CyclicJournal => add_cyclic_journal(&mut top, false),
        TopLayer::NeedsRecovery => add_cyclic_journal(&mut top, true),
    }
    let sizes = [
        FIRST_PARTITION_SIZE,
        template_size,
        u64::try_from(top.len()).unwrap(),
    ];
    let mut extents = Vec::new();
    let mut next = ALIGNMENT;
    for size in sizes {
        let last = next + size / SECTOR - 1;
        extents.push((next, last));
        next = (last + 1).next_multiple_of(ALIGNMENT);
    }
    // The backup array's 32 sectors and the backup header end the disk, after the usable sectors.
    let sectors = next + ALIGNMENT;
    let (first_usable, last_usable) = (34_u64, sectors - 34);
    let (backup_entries, backup_header) = (sectors - 33, sectors - 1);
    let guid = |text: &str| Uuid::parse(text).unwrap().to_bytes_le();

    let mut entries = vec![0; 128 * 128];
    for (entry, ((first, last), unique)) in entries
        .chunks_exact_mut(128)
        .zip(extents.iter().zip(PARTITION_GUIDS))
    {
        entry[0..16].copy_from_slice(&guid(LINUX_DATA));
        entry[16..32].copy_from_slice(&guid(unique));
        entry[32..40].copy_from_slice(&first.to_le_bytes());
        entry[40..48].copy_from_slice(&last.to_le_bytes());
    }
    let entries_crc = crc32(&entries);
    let header = |own: u64, alternate: u64, array: u64| {
        let mut header = vec![0; 92];
        header[0..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
        header[12..16].copy_from_slice(&92_u32.to_le_bytes());
        header[24..32].copy_from_slice(&own.to_le_bytes());
        header[32..40].copy_from_slice(&alternate.to_le_bytes());
        header[40..48].copy_from_slice(&first_usable.to_le_bytes());
        header[48..56].copy_from_slice(&last_usable.to_le_bytes());
        header[56..72].copy_from_slice(&guid(DISK_GUID));
        header[72..80].copy_from_slice(&array.to_le_bytes());
        header[80..84].copy_from_slice(&128_u32.to_le_bytes());
        header[84..88].copy_from_slice(&128_u32.to_le_bytes());
        header[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32(&header);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
        header
    };
    // The protective MBR: one record of type 0xEE, from LBA 1 over the rest of the disk, with the
    // addresses that UEFI gives it.
    let mut mbr = vec![0; 512];
    let protective = u32::try_from(sectors - 1).unwrap_or(u32::MAX);
    mbr[446..450].copy_from_slice(&[0x00, 0x00, 0x02, 0x00]);
    mbr[450..454].copy_from_slice(&[0xee, 0xff, 0xff, 0xff]);
    mbr[454..458].copy_from_slice(&1_u32.to_le_bytes());
    mbr[458..462].copy_from_slice(&protective.to_le_bytes());
    mbr[510..512].copy_from_slice(&[0x55, 0xaa]);

    let mut disk = File::create(path).unwrap();
    disk.set_len(sectors * SECTOR).unwrap();
    let parts = [
        (0, mbr),
        (1, header(1, backup_header, 2)),
        (2, entries.clone()),
        (backup_entries, entries),
        (backup_header, header(backup_header, 1, backup_entries)),
        (
            extents[0].0,
            [FIRST_PARTITION, FIRST_PARTITION_PADDING].concat(),
        ),
        (extents[2].0, top),
    ];
    for (sector, bytes) in parts {
        disk.seek(SeekFrom::Start(sector * SECTOR)).unwrap();
        disk.write_all(&bytes).unwrap();
    }
    disk.seek(SeekFrom::Start(extents[1].0 * SECTOR)).unwrap();
    io::copy(&mut File::open(template).unwrap(), &mut disk).unwrap();
    disk.sync_all().unwrap();
    drop(disk);

    let disk = File::open(path).unwrap();
    let partitions = PARTITION_GUIDS
        .iter()
        .zip(sizes)
        .map(|(unique, size)| {
            ExpectedPartition::new()
                .unique_guid(Uuid::parse(unique).unwrap())
                .size(size)
        })
        .collect();
    let expected = Expected::new()
        .disk_guid(Uuid::parse(DISK_GUID).unwrap())
        .partitions(partitions);
    let read = Disk::read(&disk, 512, sectors * SECTOR, &expected).unwrap();
    let first_partition = read
        .read_first_partition(&disk, usize::try_from(FIRST_PARTITION_SIZE).unwrap())
        .unwrap();
    assert_eq!(first_partition, FIRST_PARTITION);
}

/// Returns a small file system in the ext2 format, of [`LAYER_BLOCKS`] blocks of [`LAYER_BLOCK`]
/// bytes, which the kernel's ext4 driver mounts as an ext4 file system without features: its top
/// directory holds the regular `files`, each a name and its bytes, of at most 12 blocks each,
/// then the empty `directories`, each with mode 0755, and at most [`LAYER_INODES`] inodes in all.
///
/// Block 1 holds the superblock, 2 the group descriptor, 3 and 4 the block and inode bitmaps, 5
/// and 6 the inode table, 7 the top directory, and the blocks after them the files' bytes and the
/// directories.
fn small_layer(files: &[(&str, &[u8])], directories: &[&str]) -> Vec<u8> {
    /// Writes `value` at `offset` of `image`, in little-endian order.
    fn put<const N: usize>(image: &mut [u8], offset: usize, value: [u8; N]) {
        image[offset..offset + N].copy_from_slice(&value);
    }
    /// Writes the inode `number` of `image`, of `mode`, `size` bytes, and `links` links, whose data
    /// is in `blocks`, at most the 12 that an inode addresses directly.
    fn inode(
        image: &mut [u8],
        number: usize,
        mode: u16,
        size: usize,
        links: usize,
        blocks: &[usize],
    ) {
        assert!(
            blocks.len() <= 12,
            "an inode of the layer addresses 12 blocks"
        );
        let offset = 5 * LAYER_BLOCK + (number - 1) * 128;
        put(image, offset, mode.to_le_bytes());
        put(
            image,
            offset + 4,
            u32::try_from(size).unwrap().to_le_bytes(),
        );
        put(
            image,
            offset + 26,
            u16::try_from(links).unwrap().to_le_bytes(),
        );
        // The number of 512-byte sectors that the data takes.
        let sectors = u32::try_from(blocks.len() * LAYER_BLOCK / 512).unwrap();
        put(image, offset + 28, sectors.to_le_bytes());
        for (index, block) in blocks.iter().enumerate() {
            let block = u32::try_from(*block).unwrap();
            put(image, offset + 40 + 4 * index, block.to_le_bytes());
        }
    }
    /// Writes the directory `entries` into `block` of `image`, each an inode, a name, and a file
    /// type, the last of which spans the rest of the block.
    fn directory(image: &mut [u8], block: usize, entries: &[(usize, &str, u8)]) {
        let (mut offset, end) = (block * LAYER_BLOCK, (block + 1) * LAYER_BLOCK);
        for (index, (inode, name, kind)) in entries.iter().enumerate() {
            let length = if index + 1 == entries.len() {
                end - offset
            } else {
                (8 + name.len()).next_multiple_of(4)
            };
            put(image, offset, u32::try_from(*inode).unwrap().to_le_bytes());
            put(
                image,
                offset + 4,
                u16::try_from(length).unwrap().to_le_bytes(),
            );
            image[offset + 6] = u8::try_from(name.len()).unwrap();
            image[offset + 7] = *kind;
            image[offset + 8..offset + 8 + name.len()].copy_from_slice(name.as_bytes());
            offset += length;
        }
    }
    /// Sets the bits `bits` of the bitmap in `block` of `image`.
    fn set(image: &mut [u8], block: usize, bits: std::ops::Range<usize>) {
        for bit in bits {
            image[block * LAYER_BLOCK + bit / 8] |= 1 << (bit % 8);
        }
    }

    let (top, regular, directory_type) = (2, 1, 2);
    let mut image = vec![0; LAYER_BLOCK * LAYER_BLOCKS];
    // The first block and the first inode that are free.
    let (mut block, mut number) = (8, 11);
    let mut entries = vec![(top, ".", directory_type), (top, "..", directory_type)];
    for (name, bytes) in files {
        let blocks = (block..block + bytes.len().div_ceil(LAYER_BLOCK)).collect::<Vec<_>>();
        image[block * LAYER_BLOCK..block * LAYER_BLOCK + bytes.len()].copy_from_slice(bytes);
        inode(&mut image, number, 0o100_644, bytes.len(), 1, &blocks);
        entries.push((number, *name, regular));
        block += blocks.len();
        number += 1;
    }
    for name in directories {
        directory(
            &mut image,
            block,
            &[(number, ".", directory_type), (top, "..", directory_type)],
        );
        inode(&mut image, number, 0o040_755, LAYER_BLOCK, 2, &[block]);
        entries.push((number, *name, directory_type));
        block += 1;
        number += 1;
    }
    directory(&mut image, 7, &entries);
    inode(
        &mut image,
        top,
        0o040_755,
        LAYER_BLOCK,
        2 + directories.len(),
        &[7],
    );
    assert!(
        block <= LAYER_BLOCKS && number <= LAYER_INODES + 1,
        "the layer is too small"
    );

    // The bitmaps of the used blocks, from block 1, and inodes, from inode 1, whose bits past the
    // last block and inode are set, as `mke2fs` sets them.
    set(&mut image, 3, 0..block - 1);
    set(&mut image, 3, LAYER_BLOCKS - 1..LAYER_BLOCK * 8);
    set(&mut image, 4, 0..number - 1);
    set(&mut image, 4, LAYER_INODES..LAYER_BLOCK * 8);
    let free_blocks = LAYER_BLOCKS - block;
    let free_inodes = LAYER_INODES + 1 - number;

    // The superblock: one group of blocks, inodes of 128 bytes, the first of them not reserved,
    // 11, and only the `filetype` feature.
    let superblock = LAYER_BLOCK;
    for (offset, value) in [
        (0x00, LAYER_INODES),
        (0x04, LAYER_BLOCKS),
        (0x0c, free_blocks),
        (0x10, free_inodes),
        (0x14, 1),
        (0x20, LAYER_BLOCK * 8),
        (0x24, LAYER_BLOCK * 8),
        (0x28, LAYER_INODES),
        (0x4c, 1),
        (0x54, 11),
        (0x60, 0x2),
    ] {
        let value = u32::try_from(value).unwrap();
        put(&mut image, superblock + offset, value.to_le_bytes());
    }
    for (offset, value) in [
        (0x36, 0xffff),
        (0x38, 0xef53),
        (0x3a, 1),
        (0x3c, 1),
        (0x58, 128),
    ] {
        put(&mut image, superblock + offset, u16::to_le_bytes(value));
    }
    let uuid = Uuid::parse("2d4e6f80-91a2-4b3c-8d5e-6f708192a3b4").unwrap();
    image[superblock + 0x68..superblock + 0x78].copy_from_slice(uuid.as_bytes());

    // The group descriptor: where the bitmaps and the inode table are, and what is free.
    let descriptor = 2 * LAYER_BLOCK;
    for (offset, value) in [(0x00, 3), (0x04, 4), (0x08, 5)] {
        put(&mut image, descriptor + offset, u32::to_le_bytes(value));
    }
    for (offset, value) in [
        (0x0c, free_blocks),
        (0x0e, free_inodes),
        (0x10, 1 + directories.len()),
    ] {
        let value = u16::try_from(value).unwrap();
        put(&mut image, descriptor + offset, value.to_le_bytes());
    }
    image
}

/// Corrupts `layer`, a [`small_layer`]: its top directory's inode, inode 2, loses its mode, while
/// it keeps its links, an error that the kernel finds when it mounts the layer, and the error
/// policy of its superblock, `s_errors`, becomes 3, which asks the kernel to panic on an error.
fn corrupt(layer: &mut [u8]) {
    let policy = LAYER_BLOCK + 0x3c;
    layer[policy..policy + 2].copy_from_slice(&3_u16.to_le_bytes());
    // The inode table starts at block 5, with inodes of 128 bytes, from inode 1.
    let mode = 5 * LAYER_BLOCK + 128;
    layer[mode..mode + 2].fill(0);
}

/// Adds an internal journal whose nonempty ring contains only empty revoke records of one
/// sequence, so a journal scan wraps forever without finding a commit or another sequence.
/// Even with `needs_recovery` clear, loading the journal scans it to discard its old contents.
fn add_cyclic_journal(layer: &mut Vec<u8>, needs_recovery: bool) {
    const JOURNAL_BLOCKS: usize = 1024;
    assert_eq!(layer.len(), LAYER_BLOCKS * LAYER_BLOCK);
    layer.resize((LAYER_BLOCKS + JOURNAL_BLOCKS) * LAYER_BLOCK, 0);

    // The appended blocks are allocated. The original bitmap already marks everything past
    // the original file system's end used, so its free counts do not change.
    let superblock = LAYER_BLOCK;
    for (offset, value) in [
        (0x04, u32::try_from(LAYER_BLOCKS + JOURNAL_BLOCKS).unwrap()),
        (0x5c, 0x04),                                         // has_journal
        (0x60, 0x42 | if needs_recovery { 0x04 } else { 0 }), // filetype, extents
        (0xe0, 8),                                            // journal inode
    ] {
        layer[superblock + offset..superblock + offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    // Reserved inode 8 maps the whole journal with one extent.
    let inode = 5 * LAYER_BLOCK + 7 * 128;
    for (offset, value) in [
        (0x00, 0o100_600_u16),
        (0x1a, 1),
        (0x28, 0xf30a), // extent header
        (0x2a, 1),
        (0x2c, 4),
        (0x38, u16::try_from(JOURNAL_BLOCKS).unwrap()),
    ] {
        layer[inode + offset..inode + offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    for (offset, value) in [
        (0x04, u32::try_from(JOURNAL_BLOCKS * LAYER_BLOCK).unwrap()),
        (
            0x1c,
            u32::try_from(JOURNAL_BLOCKS * LAYER_BLOCK / 512).unwrap(),
        ),
        (0x20, 0x80000), // EXT4_EXTENTS_FL
        (0x3c, u32::try_from(LAYER_BLOCKS).unwrap()),
    ] {
        layer[inode + offset..inode + offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    let journal = LAYER_BLOCKS * LAYER_BLOCK;
    for (offset, value) in [
        (0x00, 0xc03b_3998_u32),
        (0x04, 4), // JBD2_SUPERBLOCK_V2
        (0x0c, u32::try_from(LAYER_BLOCK).unwrap()),
        (0x10, u32::try_from(JOURNAL_BLOCKS).unwrap()),
        (0x14, 1), // first block of the ring
        (0x18, 1), // sequence
        (0x1c, 1), // nonempty ring's start
        (0x28, 1), // revoke feature; no journal checksums
        (0x40, 1), // one user
    ] {
        layer[journal + offset..journal + offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    layer.copy_within(superblock + 0x68..superblock + 0x78, journal + 0x30);
    for block in layer[journal + LAYER_BLOCK..].chunks_exact_mut(LAYER_BLOCK) {
        for (offset, value) in [(0, 0xc03b_3998_u32), (4, 5), (8, 1), (12, 16)] {
            block[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        }
    }
}

/// Returns the CRC-32 of `bytes`, which GPT's headers and entry arrays hold: of the reflected
/// polynomial 0xEDB88320, from all ones, inverted.
fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0, |crc, byte| {
        (0..8).fold(crc ^ u32::from(*byte), |crc: u32, _| {
            (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg())
        })
    })
}

/// Returns `bytes` in lowercase hexadecimal, as the `first-partition` probe reports them.
fn hexadecimal(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0xf])
        .map(|digit| char::from(DIGITS[usize::from(digit)]))
        .collect()
}

/// The capabilities that an entry allows, by their names in the cases of the `entries` probe, each
/// with its number and the attempt of the probe that it alone allows, if any.
const CAPABILITIES: [(&str, u8, Option<&str>); 13] = [
    ("chown", 0, Some("chown")),
    ("dac-override", 1, Some("dac-override")),
    ("fowner", 3, Some("fowner")),
    ("fsetid", 4, None),
    ("kill", 5, None),
    ("setgid", 6, Some("setgid")),
    ("setuid", 7, Some("setuid")),
    ("setpcap", 8, None),
    ("net-bind-service", 10, Some("bind")),
    ("net-raw", 13, Some("raw")),
    ("sys-chroot", 18, Some("escape")),
    ("audit-write", 29, None),
    ("setfcap", 31, None),
];

/// The attempts of the `entries` probe that a capability allows, each with its outcome without
/// the capability.
const ATTEMPTS: [(&str, &str); 8] = [
    ("chown", "refused EPERM"),
    ("dac-override", "refused EACCES"),
    ("fowner", "refused EPERM"),
    ("setuid", "refused EPERM"),
    ("setgid", "refused EPERM"),
    ("bind", "refused EACCES"),
    ("raw", "refused EPERM"),
    ("escape", "refused EPERM"),
];

/// The machine ID that the `entries` probe writes in the root, as the root's own.
const ROOT_MACHINE_ID: &str = "0123456789abcdef0123456789abcdef";

/// Returns the outcome of the `entries` probe's `attempt` with the capability that allows it.
fn allowed(attempt: &str) -> &'static str {
    if attempt == "escape" {
        "contained"
    } else {
        "allowed"
    }
}

/// Returns a capability set as `/proc/PID/status` writes it: 16 hexadecimal digits.
fn capability_set(mask: u64) -> String {
    format!("{mask:016x}")
}

/// Checks the VM of the `root_profiles_enter_the_root` scenario.
fn check_root_profiles(vm: &mut Vm<'_>) {
    let facts = vm.probe(&["entries"]);
    let fact = |key: &str| {
        facts
            .get(&format!("entries.{key}"))
            .unwrap_or_else(|| panic!("entries.{key} is missing: {facts:?}"))
            .as_str()
    };

    // The entry refuses every capability outside its allowlist, the binding and raw-socket ones
    // while listeners are published, any for a user ID other than 0, and an identity that has no
    // account in existing-account mode, before an execution starts.
    assert_eq!(fact("refused.outside"), "51");
    assert_eq!(fact("refused.accepted"), "none");
    assert_eq!(
        fact("refused.published.net-bind-service"),
        "published CAP_NET_BIND_SERVICE"
    );
    assert_eq!(fact("refused.published.net-raw"), "published CAP_NET_RAW");
    assert_eq!(fact("refused.non-root"), "non-root-capabilities");
    assert_eq!(fact("refused.existing"), "account-missing");

    let cases = CAPABILITIES
        .iter()
        .map(|(name, ..)| format!("only-{name}"))
        .chain(["none", "published", "unpublished", "numeric", "generated"].map(str::to_owned))
        .collect::<Vec<_>>();
    let none = capability_set(0);
    for case in &cases {
        let fact = |key: &str| fact(&format!("{case}.{key}"));
        // Each execution starts, as PID 1 of its namespaces, and keeps only its standard streams:
        // the descriptor that its caller leaked closed. It holds no inheritable or ambient
        // capability, and no new privileges.
        assert_eq!((fact("launch"), fact("status")), ("started", "exit 0"));
        assert_eq!(fact("pid"), "1", "{case}");
        assert_eq!(fact("fds"), "0,1,2", "{case}");
        assert_eq!(fact("leaked"), "closed", "{case}");
        // Without a working directory, it runs in its root's top.
        assert_eq!(fact("cwd"), "/", "{case}");
        assert_eq!(fact("no-new-privs"), "1", "{case}");
        assert_eq!(
            (fact("cap.inh"), fact("cap.amb")),
            (&*none, &*none),
            "{case}"
        );
        // It inherits no signal that its caller blocks, `SIGCHLD`.
        assert_eq!(fact("signals.blocked"), none, "{case}");
        // Its `/proc/sys` and `/proc/sysrq-trigger` refuse writes, read-only for user ID 0, so no
        // execution lowers the unprivileged port start, which the runtime owns.
        let refused = if case == "numeric" {
            "refused EACCES"
        } else {
            "refused EROFS"
        };
        for write in ["threshold.write", "hostname.write", "sysrq.write"] {
            assert_eq!(fact(write), refused, "{case}: {write}");
        }
        assert_eq!(fact("threshold"), "1024", "{case}");
        // The root, moved over `/`, holds the staged executable, and a nested `chroot` ends in
        // it, when the execution may `chroot` at all.
        let root = fact("root").split(',').collect::<BTreeSet<_>>();
        assert!(root.contains(".runtime-adapter-guest"), "{case}: {root:?}");
        assert!(
            root.contains("etc") && root.contains("usr"),
            "{case}: {root:?}"
        );
        let hostname = if case == "generated" {
            "runtime-adapter"
        } else {
            "nvx-sandbox"
        };
        assert_eq!(fact("hostname"), hostname, "{case}");
        // Before the runtime binds a generated machine ID, every execution sees the root's own.
        if case != "generated" {
            assert_eq!(fact("machine-id"), ROOT_MACHINE_ID, "{case}");
            assert_eq!(fact("mount.machine-id"), "none", "{case}");
            assert_eq!(fact("time.state"), "absent", "{case}");
            assert_eq!(fact("time.files"), "absent", "{case}");
            assert_eq!(fact("mount.time"), "none", "{case}");
        }
    }

    // User ID 0 holds each allowed capability alone in its permitted, effective, and bounding
    // sets, and only it: its own attempt succeeds, and every other one fails.
    for (name, number, allows) in CAPABILITIES {
        let case = format!("only-{name}");
        let fact = |key: &str| fact(&format!("{case}.{key}"));
        assert_eq!((fact("uid"), fact("gid"), fact("groups")), ("0", "0", ""));
        for set in ["cap.prm", "cap.eff", "cap.bnd"] {
            assert_eq!(fact(set), capability_set(1 << number), "{case}: {set}");
        }
        for (attempt, refused) in ATTEMPTS {
            let expected = if allows == Some(attempt) {
                allowed(attempt)
            } else {
                refused
            };
            assert_eq!(fact(attempt), expected, "{case}: {attempt}");
        }
    }
    // Without capabilities, user ID 0 may do none of it, nor may the numeric identity, which has
    // no account in the image.
    for case in ["none", "numeric"] {
        let fact = |key: &str| fact(&format!("{case}.{key}"));
        for set in ["cap.prm", "cap.eff", "cap.bnd"] {
            assert_eq!(fact(set), none, "{case}: {set}");
        }
        for (attempt, refused) in ATTEMPTS {
            assert_eq!(fact(attempt), refused, "{case}: {attempt}");
        }
    }
    let ids = |case: &str| {
        let fact = |key: &str| fact(&format!("{case}.{key}"));
        (fact("uid"), fact("gid"), fact("groups"))
    };
    assert_eq!(ids("none"), ("0", "0", ""));
    assert_eq!(ids("numeric"), ("4242", "4242", ""));

    // In both publication modes, private-network ingress and the host's loopback, which the entry
    // treats alike, a root workload with every other allowed capability can neither bind a
    // privileged source port, from which the gateway accepts listener announcements, nor open a
    // raw socket, which could forge one. Without published listeners, it may.
    let every = CAPABILITIES
        .iter()
        .fold(0u64, |mask, (_, number, _)| mask | 1 << number);
    let refused_while_published = 1 << 10 | 1 << 13;
    for (case, mask) in [
        ("published", every & !refused_while_published),
        ("unpublished", every),
    ] {
        let fact = |key: &str| fact(&format!("{case}.{key}"));
        for set in ["cap.prm", "cap.eff", "cap.bnd"] {
            assert_eq!(fact(set), capability_set(mask), "{case}: {set}");
        }
        for (attempt, refused) in ATTEMPTS {
            let expected = if case == "published" && ["bind", "raw"].contains(&attempt) {
                refused
            } else {
                allowed(attempt)
            };
            assert_eq!(fact(attempt), expected, "{case}: {attempt}");
        }
    }

    // An execution enters the working directory that it names in the root, but through no magic
    // link: neither the link of the descriptor that its caller leaked, which it closed, nor that
    // of its own or its first process's working directory or root, nor a symbolic link to one,
    // nor that of any descriptor, may place it outside the root, or run its program.
    assert_eq!(
        (fact("cwd-directory.launch"), fact("cwd-directory.status")),
        ("started", "exit 0")
    );
    assert_eq!(
        fact("cwd-directory.cwd"),
        "/tmp/runtime-adapter-entries/cwd-directory"
    );
    for (case, error) in [
        ("cwd-leaked", "ENOENT"),
        ("cwd-self", "ELOOP"),
        ("cwd-root", "ELOOP"),
        ("cwd-init", "ELOOP"),
        ("cwd-link", "ELOOP"),
    ] {
        let fact = |key: &str| fact(&format!("{case}.{key}"));
        assert_eq!(
            (fact("launch"), fact("status")),
            (&*format!("working directory {error}"), "exit 125"),
            "{case}"
        );
    }
    assert_eq!(
        fact("cwd-descriptors.launch"),
        "working directory ELOOP,working directory ENOENT"
    );
    assert_eq!(fact("cwd-descriptors.status"), "exit 125");

    // A generated machine ID and the time status that the guest's time daemon publishes, which the
    // runtime bound once, read-only, in the helper's mount namespace, reach the execution, and end
    // with the helper: the worker's own namespace keeps the root's machine ID, without mounts
    // below the root.
    let generated = fact("generated.source");
    assert_eq!(generated.len(), 32, "{generated}");
    assert!(
        generated
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{generated}"
    );
    assert_ne!(generated, ROOT_MACHINE_ID);
    assert_eq!(fact("generated.machine-id"), generated);
    assert_eq!(fact("generated.time.state"), "version=1");
    // The time status holds only what any process may read: no lock of the daemon's, which user
    // ID 0 could open and hold to block the daemon, and no private record.
    let files = fact("generated.time.files");
    let names = files.split(',').collect::<BTreeSet<_>>();
    assert!(names.contains("state"), "{files}");
    assert!(
        names.is_subset(&BTreeSet::from([
            "state",
            ".state",
            "daemon.pid",
            "daemon.pid.tmp"
        ])),
        "{files}"
    );
    for mount in ["generated.mount.machine-id", "generated.mount.time"] {
        let found = options(fact(mount));
        for flag in ["ro", "nosuid", "nodev"] {
            assert!(found.contains(flag), "{mount}: {found:?}");
        }
    }
    assert_eq!(fact("after.machine-id"), ROOT_MACHINE_ID);
    assert_eq!(fact("after.mounts"), "0");
}

/// Returns the options and the arguments of an execution of the host program's `exec`: `options`,
/// then `--`, then the program and its arguments, `command`.
fn execution(options: &[&str], command: &[&str]) -> Vec<String> {
    options
        .iter()
        .chain(&["--"])
        .chain(command)
        .map(|argument| (*argument).to_owned())
        .collect()
}

/// Returns `identity` as the value of the option `--identity` of `exec`: `UID:GID`.
fn ids((uid, gid): (u32, u32)) -> String {
    format!("{uid}:{gid}")
}

/// Returns `text` as the host program prints a value: with `\`, newlines, and carriage returns
/// escaped.
fn escaped(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Returns the output of `seq first last`.
fn lines(first: usize, last: usize) -> String {
    (first..=last).map(|number| format!("{number}\n")).collect()
}

/// Returns what the [`IDENTITY_SCRIPT`] prints for the user ID `uid`, the group ID `gid`, the
/// groups `groups`, and the capabilities `capabilities`, a set of each capability's bit.
fn identity_facts(uid: u32, gid: u32, groups: &str, capabilities: u64) -> String {
    let set = capability_set(capabilities);
    format!("{uid}\n{gid}\n{groups}\nCapPrm: {set}\nCapEff: {set}\nCapBnd: {set}\n")
}

/// Returns the fact KEY of execution `number` that `exec` printed in `printed`.
fn fact<'a>(printed: &'a Printed, number: usize, key: &str) -> &'a str {
    let key = format!("exec.{number}.{key}");
    printed
        .get(&key)
        .unwrap_or_else(|| panic!("{key} is missing: {printed:?}"))
}

/// Checks that execution `number` of what `exec` printed in `printed` started, ended on its own
/// with the exit code `code`, lost no byte, and settled, so that its place is reusable, and that
/// it wrote `stdout` and `stderr`.
fn assert_ran(printed: &Printed, number: usize, code: i32, stdout: &str, stderr: &str) {
    let status = format!("exit {code}");
    let facts =
        ["launch", "status", "cause", "truncated", "settled"].map(|key| fact(printed, number, key));
    assert_eq!(
        facts,
        ["started", &*status, "none", "none", "reusable"],
        "{number}: {printed:?}"
    );
    assert_eq!(fact(printed, number, "stdout"), escaped(stdout), "{number}");
    assert_eq!(fact(printed, number, "stderr"), escaped(stderr), "{number}");
}

/// Checks that the guest refused execution `number` of what `exec` printed in `printed` at stage
/// 0 with the error number `error`, before its program started: the execution is fenced, and has
/// no output.
fn assert_refused(printed: &Printed, number: usize, error: i32) {
    let launch = format!("failed 0 {error}");
    let facts =
        ["launch", "status", "settled", "stdout", "stderr"].map(|key| fact(printed, number, key));
    assert_eq!(
        facts,
        [&*launch, "none", "fenced", "", ""],
        "{number}: {printed:?}"
    );
}

/// Checks the VM of the `executions_run_in_the_root` scenario, whose starts name their identities.
fn check_executions(vm: &mut Vm<'_>) {
    let nobody = ids(NOBODY);

    // Several executions at once, which meet only while all of them run: each leaves its mark in
    // the root's `/tmp`, and waits for the others' marks. Beside them, one writes more than a MiB
    // to its standard output, and a little to its standard error, which arrive whole.
    let meet = format!(
        "mkdir -p /tmp/meeting && touch /tmp/meeting/$NUMBER && \
         until [ \"$(ls /tmp/meeting | wc -l)\" -ge {MEETING} ]; do sleep 0.01; done && \
         echo \"$NUMBER met\""
    );
    let mut executions = (1..=MEETING)
        .map(|number| {
            let environment = format!("NUMBER={number}");
            execution(
                &["--identity", &nobody, "--env", &environment],
                &["/bin/sh", "-c", &meet],
            )
        })
        .collect::<Vec<_>>();
    let writer = "seq 1 200000; seq 1 1000 >&2";
    executions.push(execution(
        &["--identity", "0:0"],
        &["/bin/sh", "-c", writer],
    ));
    let printed = vm.exec(&executions);
    for number in 1..=MEETING {
        assert_ran(&printed, number, 0, &format!("{number} met\n"), "");
    }
    let output = lines(1, 200_000);
    assert!(output.len() > 1 << 20, "{}", output.len());
    assert_ran(&printed, MEETING + 1, 0, &output, &lines(1, 1000));

    // In a later session, whose keys are new to the guest:
    // 1. a descendant in a session of its own, which holds its program's output, and which the
    //    end of its program, PID 1 of its namespace, ends, so that the execution loses no byte;
    // 2. a user other than root, without capabilities, in existing-account mode;
    // 3. root with `CAP_CHOWN` alone, in its permitted, effective, and bounding sets;
    // 4. exactly the environment that the start names;
    // 5. the working directory that the start names;
    // 6. the configured hostname, as PID 1 of the execution's namespaces;
    // 7. no identity, in a sandbox without a workload identity, which the guest refuses;
    // 8. a capability outside the entry's allowlist, `CAP_SYS_ADMIN`, which the entry refuses.
    let executions = [
        execution(
            &["--identity", "0:0"],
            &["/bin/sh", "-c", "setsid sleep 600 & echo started"],
        ),
        execution(
            &["--identity", &nobody],
            &["/bin/sh", "-c", IDENTITY_SCRIPT],
        ),
        execution(
            &["--identity", "0:0", "--capabilities", "0"],
            &["/bin/sh", "-c", IDENTITY_SCRIPT],
        ),
        execution(
            &[
                "--identity",
                &nobody,
                "--env",
                "FIRST=1",
                "--env",
                "SECOND=two words",
            ],
            &["/usr/bin/env"],
        ),
        execution(
            &["--identity", &nobody, "--cwd", "/usr/share"],
            &["/bin/pwd"],
        ),
        execution(
            &["--identity", &nobody],
            &["/bin/sh", "-c", "cat /proc/sys/kernel/hostname; echo $$"],
        ),
        execution(&[], &["/bin/true"]),
        execution(
            &["--identity", "0:0", "--capabilities", "21"],
            &["/bin/true"],
        ),
    ];
    let printed = vm.exec(&executions);
    assert_ran(&printed, 1, 0, "started\n", "");
    let nobody_facts = identity_facts(NOBODY.0, NOBODY.1, "65534", 0);
    assert_ran(&printed, 2, 0, &nobody_facts, "");
    assert_ran(&printed, 3, 0, &identity_facts(0, 0, "0", 1), "");
    assert_ran(&printed, 4, 0, "FIRST=1\nSECOND=two words\n", "");
    assert_ran(&printed, 5, 0, "/usr/share\n", "");
    assert_ran(&printed, 6, 0, "nvx-sandbox\n1\n", "");
    assert_refused(&printed, 7, EINVAL);
    assert_refused(&printed, 8, EPERM);
}

/// Checks the VM of the `executions_run_as_the_workload_identity` scenario, whose sandbox's
/// workload identity is [`NOBODY`].
fn check_workload_executions(vm: &mut Vm<'_>) {
    assert_eq!(vm.config.workload_identity, Some(NOBODY));
    let executions = [
        execution(&[], &["/bin/sh", "-c", IDENTITY_SCRIPT]),
        execution(&["--env", "ONLY=this"], &["/usr/bin/env"]),
        execution(&["--identity", &ids(NOBODY)], &["/bin/true"]),
    ];
    let printed = vm.exec(&executions);
    let nobody_facts = identity_facts(NOBODY.0, NOBODY.1, "65534", 0);
    assert_ran(&printed, 1, 0, &nobody_facts, "");
    assert_ran(&printed, 2, 0, "ONLY=this\n", "");
    assert_refused(&printed, 3, EINVAL);
}
