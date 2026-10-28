//! The host program starts clones of templates on a real hypervisor, through public APIs only: VMs
//! that a restore of a generation of a template store starts, in place, which the store leases to
//! a sandbox, and which the guest agent's clone mode activates, as `runtime-adapter-host start`
//! asks.
//!
//! Each scenario captures a [`Template`] of its own, with 256 MiB of RAM, of a capacity of
//! 1 GiB, in a template store of its own, as `vm_template` does, of the platform's copy of the
//! fixtures' Ubuntu EROFS image, or of a GPT [`Disk`] of the image's length, and starts its
//! clones, each in a sandbox store of its own, which stages the clone's images, as a launch's
//! does: that copy, or a GPT disk, and for a clone over a scratch image, a copy of the scratch
//! template, which, as NVX requires of a clone's images, no one else may change either.
//!
//! - One generation serves clones over a scratch image and in RAM, at once and in turn, at each
//!   memory target from the template's RAM to its capacity, which the guest has online. The store
//!   leases the generation to each, a stop releases it, and the clones change nothing of the
//!   generation's snapshot, and leave nothing in its payload.
//! - One generation serves clones of either format over either upper, at once: the platform's
//!   EROFS image, and GPT disks of other layouts than the template's placeholder, a GPT disk too,
//!   and than each other's. The guest of a GPT clone rereads its disk's partition table before it
//!   assembles the root, since its kernel created the partitions of the placeholder, so each
//!   clone's root stacks its own disk's layers, from the partitions that its disk's table names.
//!   A disk whose first partition is larger than the clone's bound ends the clone before it is
//!   ready.
//! - The store and NVX's plan refuse a clone that the template cannot serve before any VMM starts,
//!   each a failure of the request, as NVX attributes it by its evidence, with no VM: an image or a
//!   scratch image of another length than the template's, an image of a geometry that OpenVMM
//!   refuses, a memory target beyond its capacity or of a part of a block. The start then releases
//!   the lease, as it does once the adapter refuses images that the sandbox does not stage.
//! - A clone whose activation fails ends before it is ready, with the status of the failure's
//!   class, which attributes the failure, and the VMM's exit: a scratch image that holds no ext4
//!   file system, a journal that the clone's record forbids, a workload identity whose account the
//!   image lacks, and a host that sends anything but the clone's Activate first, a ping, a start, or
//!   an execution. The sandbox records the failure until the release of its lease.
//! - A clone refuses a scratch image whose upper is not fresh, as an earlier clone left it, and
//!   ends before it is ready.
//! - Clones at once each keep a root of their own, and every execution of a clone sees its
//!   hostname and its machine ID: one that its guest generated, distinct in each clone, the same
//!   in each of the clone's executions, which a read-only bind holds at `/etc/machine-id`, or the
//!   image's own, which the guest leaves alone, so that what an execution writes there, the
//!   clone's later executions see.
//! - A clone whose generated machine ID the guest cannot bind, since its image holds a regular
//!   file `etc`, ends before it is ready, with the status of the workload's view, while a clone of
//!   the image that keeps the image's own machine ID, which needs no bind, starts.
//! - A clone maps its export beneath its root once it is ready, before its sandbox starts: one
//!   host directory, the export's root, which NVX binds to the template's dormant filesystem slot,
//!   and whose mappings, the directory's own, from an empty source, and those of paths inside it,
//!   each keep their access, show what the host holds, but for a path that the start hides, and
//!   take what the guest, and the clone's workload, write through the writable ones alone. The
//!   clone's record names the export by the identity of its plan, with the number of its
//!   mappings.
//! - Clones of one generation at once see only their own exports: each its own directory, or two
//!   the same one, which their records name by the same identity, and a clone without an export
//!   none, whose restore leaves the slot dormant. A clone mounts its export read-write, even where
//!   every mapping is read-only, mount tables list the bind of a generated machine ID below the
//!   root beside the mappings, and the bind holds even against the guest's root.
//! - A start refuses an export that the template's dormant filesystem slot cannot bind before any
//!   VMM starts, as a failure of the request: one of two directories, and one of a file.
//! - A clone whose guest fails a mapping of its table ends before its sandbox starts, nothing of
//!   it runs, and the host's directory stays as it was.
//! - A clone runs its executions as its record's workload identity, once it is ready: the repair
//!   acknowledged the restore before the clone took its reset, the host's first message came
//!   after that, and the clone's Ready, its start, and its first execution came in that order.
//! - Each command after the start reattaches to the clone from a new process: the clone answers
//!   the Activate of its record again, in a new session of the same broker, with its Ready, without
//!   activating again, and serves the command.
//! - A host program of another runtime ends the clone through its verified identity, without a
//!   word to its guest, and releases its lease: the template still serves its other clones.
//! - A tombstone quarantines the generation of a running clone: the generation takes no new lease,
//!   while the clone keeps running, reattaches, stops, and releases its lease, and the generation's
//!   snapshot stays as the capture left it.
//! - A start that ended once the store leased the generation, before it recorded the clone,
//!   leaves a lease whose template store no record names, or only the record of an earlier clone,
//!   of another template: a stop keeps it, and only its own template store, which `--template`
//!   names, releases it, after which the sandbox leases a generation again.
//! - The store checks the generation's eligibility at the host's time: on the boot that captured
//!   the template, by the host's monotonic clock, whatever UTC says, and after a reboot by UTC,
//!   which clock fixtures stand for. It refuses a generation that is no longer, or not yet,
//!   eligible, without a lease.
//! - A clone reads no more of its disks on its restore than its root needs, which a read of each
//!   disk to its end shows the guest's statistics count.
//! - A clone's time settles after it is ready, with no violation, after the downtime that the
//!   host's monotonic clock measured.
//!
//! The tests boot VMs, so they are ignored. They read what the `common` module lists, and take
//! the template's image and the scratch template from its fixtures.
//!
//! ```text
//! cargo test --locked --features host --test vm_clone -- --ignored
//! ```
#![cfg(feature = "host")]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::mem;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use nvx::activation::{
    ActivationRecord, Configuration, ExportKind, MachineIdMode, ReadyActivation, StoreGeneration,
};
use nvx::exec::{Key, Limits, Timers, Window};
use nvx::host::artifacts::Artifacts;
#[cfg(target_os = "linux")]
use nvx::host::filesystem::GUEST_EXPORT;
use nvx::host::platform::Transport;
use nvx::host::process::{BootId, Process};
use nvx::host::store::{CallerFiles, LOG_NAME, SandboxStore};
use nvx::host::template::{
    CAPTURE_RECORD_NAME, CloneControl, ClonePlan, CloneRecords, Evidence, GENERATIONS_NAME,
    Generation, HostTime, KEY_NAME, Lease, MAX_DOWNTIME_NS, MEMORY_BLOCK_SIZE, Outcome, Progress,
    RestoreDeadlines, SNAPSHOT_NAME, Stage, Store, Teardown,
};
use nvx::protocol::root::ImageFormat;
use nvx::protocol::status;
use nvx::protocol::{CAPABILITY_LEN, HostMap};
use nvx::uuid::Uuid;
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::capture::DEFAULT_SCRATCH_LENGTH;
use runtime_adapter::clone::{CLONE_HOSTNAME, HOST_TIME_VARIABLE, Request, host_time_text};
use runtime_adapter::exec::{HostMessage, Start};
use runtime_adapter::frame::Message;
use runtime_adapter::host::Client;
use runtime_adapter::layered::{Shares, Staged};
use runtime_adapter::root::DEFAULT_RUNTIME;
use runtime_adapter::runtime::Runtime;

use common::gpt::{self, Contents, Layout, Partition, hexadecimal, small_layer};
use common::{Fixtures, Printed, Sandbox};

/// The exit status of the host program's command that failed, after a diagnostic.
const FAILED: i32 = 1;
/// The exit status of a command whose clone failed, once nothing of the clone runs, and the
/// command released its lease.
const CLONE_FAILED: i32 = 8;
/// The guest agent's build of a host program of another runtime.
const ANOTHER_BUILD: &str = "another-build";
/// Bytes in a mebibyte.
const MIB: u64 = 1024 * 1024;
/// The RAM of the scenarios' templates, in MiB.
const TEMPLATE_MEMORY: u64 = 256;
/// The RAM capacity of the scenarios' templates, in MiB.
const TEMPLATE_CAPACITY: u64 = 1024;
/// The generation of each scenario's template store that its template is.
const GENERATION: StoreGeneration = StoreGeneration::new(NonZeroU64::MIN);
/// The workload identity of the clones that run as one: nobody's, in the Ubuntu image.
const NOBODY: &str = "65534:65534";
/// A workload identity whose account the Ubuntu image lacks.
const STRANGER: &str = "4242:4242";
/// Bytes in a sector of the guest's disks, which their statistics count.
const SECTOR: u64 = 512;
/// How long a clone's time may take to settle once the clone is ready.
const TIME_SETTLEMENT: Duration = Duration::from_secs(30);
/// Nanoseconds in an hour.
const HOUR_NS: u64 = 3_600_000_000_000;
/// The error number with which the guest refuses an execution that names an identity in a
/// sandbox that has its own: `EINVAL`.
const EINVAL: i32 = 22;
/// The machine ID that each execution of [`VIEW`] writes to a clone's `/etc/machine-id`.
const WRITTEN_MACHINE_ID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
/// The program of the executions that show a clone's view of its root, `/bin/sh -c VIEW sh
/// MARKER ID`, which prints, a line each: the hostname, `hostname NAME`; the machine ID at
/// `/etc/machine-id`, or `absent`, `machine-id ID`; the options of each mount there, `bind
/// OPTIONS`; whether a write of ID there succeeded, `write written`, or not, `write refused`; and
/// what the root's `/marker` holds, or `absent`, `marker TEXT`. Then it writes MARKER to `/marker`.
const VIEW: &str = r#"echo "hostname $(uname -n)"
echo "machine-id $(cat /etc/machine-id 2>/dev/null || echo absent)"
while read -r _ _ _ _ point options _; do
    if [ "$point" = /etc/machine-id ]; then echo "bind $options"; fi
done < /proc/self/mountinfo
if { printf '%s\n' "$2" > /etc/machine-id; } 2>/dev/null; then
    echo "write written"
else
    echo "write refused"
fi
echo "marker $(cat /marker 2>/dev/null || echo absent)"
printf '%s' "$1" > /marker
"#;
/// The directory of a template's sandbox that holds the host directories that the scenario's
/// clones export, the one host directory that their starts authorize.
const EXPORTS: &str = "exports";
/// The directories of the host directory that a clone exports in full, with the
/// [`EXPORT_SHARES`]: `dir`, which holds the empty directory `inner`, and `out`.
const EXPORT_DIRECTORIES: [&str; 3] = ["dir", "dir/inner", "out"];
/// The files of the host directory that a clone exports in full, each a path in it, with
/// components separated by `/`, and its contents.
const EXPORT_FILES: [(&str, &str); 4] = [
    ("dir/marker", "marker"),
    ("dir/secret", "secret"),
    ("file.txt", "file"),
    ("out/marker", "marker"),
];
/// The shares of a clone that exports a host directory in full, each a path in it, `.` for the
/// directory itself, and its target in the root, with `:ro` for a read-only share: the directory,
/// read-only, with the read-write directory `dir/inner` inside it, at the target that it has
/// there, the read-only file `file.txt`, and the read-write directory `out`. The plan exports the
/// directory alone, the export's root, whose mapping comes from an empty source, and the start
/// hides `dir/secret`.
const EXPORT_SHARES: [(&str, &str); 4] = [
    (".", "/srv/maps:ro"),
    ("dir/inner", "/srv/maps/dir/inner"),
    ("file.txt", "/srv/file.txt:ro"),
    ("out", "/srv/out"),
];
/// The `map` probe of the [`EXPORT_SHARES`]: each target, and the hidden file.
const MAP_EXPORT: &str = "map maps=srv/maps inner=srv/maps/dir/inner file=srv/file.txt \
                          out=srv/out secret=srv/maps/dir/secret";
/// The mappings of the [`EXPORT_SHARES`], in the order in which the guest applied them, which the
/// root's mount table keeps: by the depth of their targets, then by their targets.
const EXPORT_MOUNTS: &str = "/srv/file.txt,/srv/maps,/srv/out,/srv/maps/dir/inner";
/// The program of the executions that show what a clone's workload finds of the
/// [`EXPORT_SHARES`], `/bin/sh -c EXPORTED sh`, which prints, a line each: the contents of
/// `/srv/maps/dir/marker` and of `/srv/file.txt`, `read PATH TEXT`, or `read PATH refused` for one
/// that cannot be read; whether the hidden `/srv/maps/dir/secret` could be read, `secret read`,
/// or not, `secret refused`; the names in `/srv/maps/dir`, each followed by a comma, `listing
/// NAMES`; and for each of `/srv/maps`, `/srv/maps/dir/inner`, and `/srv/out`, whether a write of
/// the new file `workload` in it succeeded, `write DIRECTORY written`, or not, `write DIRECTORY
/// refused`.
const EXPORTED: &str = r#"for path in /srv/maps/dir/marker /srv/file.txt; do
    if text=$(cat "$path" 2>/dev/null); then
        echo "read $path $text"
    else
        echo "read $path refused"
    fi
done
if cat /srv/maps/dir/secret >/dev/null 2>&1; then echo "secret read"; else echo "secret refused"; fi
echo "listing $(ls /srv/maps/dir | tr '\n' ',')"
for directory in /srv/maps /srv/maps/dir/inner /srv/out; do
    if { printf '%s' workload > "$directory/workload"; } 2>/dev/null; then
        echo "write $directory written"
    else
        echo "write $directory refused"
    fi
done
"#;
/// The `map` probe of the clones that share one host directory at `/srv/data`: the directory, and
/// its file `marker`.
const MAP_DATA: &str = "map data=srv/data marker=srv/data/marker";
/// The placeholder of the template of the scenarios of GPT disks: a disk of two layers, whose
/// partitions start at multiples of 1 MiB.
const PLACEHOLDER: Disk = Disk {
    guid: "5c0f2a8e-3b71-4d96-8e04-7a1c9b2d6e30",
    partitions: &[
        "5c0f2a8e-3b71-4d96-8e04-7a1c9b2d6e31",
        "5c0f2a8e-3b71-4d96-8e04-7a1c9b2d6e32",
        "5c0f2a8e-3b71-4d96-8e04-7a1c9b2d6e33",
    ],
    alignment: 2048,
    first_size: 4096,
    first: b"\xffthe placeholder's first partition",
    top: &[],
};
/// A clone's GPT disk of one layer, whose partitions start at multiples of 2 MiB, with a larger
/// first partition than the placeholder's: its layer's partition starts where none of the
/// placeholder's does.
const ONE_LAYER: Disk = Disk {
    guid: "a17c3e90-6d24-4f5b-b8e1-02c9d4f6a8b0",
    partitions: &[
        "a17c3e90-6d24-4f5b-b8e1-02c9d4f6a8b1",
        "a17c3e90-6d24-4f5b-b8e1-02c9d4f6a8b2",
    ],
    alignment: 4096,
    first_size: 8192,
    first: b"\xfe\0one layer\x80",
    top: &[],
};
/// A clone's GPT disk of three layers, whose partitions start at multiples of 640 KiB, with the
/// largest first partition: its layers' partitions start where none of the placeholder's do, nor
/// [`ONE_LAYER`]'s.
const THREE_LAYERS: Disk = Disk {
    guid: "b2e4f6a8-0c1d-4e3f-9a5b-6c7d8e9f0a10",
    partitions: &[
        "b2e4f6a8-0c1d-4e3f-9a5b-6c7d8e9f0a11",
        "b2e4f6a8-0c1d-4e3f-9a5b-6c7d8e9f0a12",
        "b2e4f6a8-0c1d-4e3f-9a5b-6c7d8e9f0a13",
        "b2e4f6a8-0c1d-4e3f-9a5b-6c7d8e9f0a14",
    ],
    alignment: 1280,
    first_size: 12288,
    first: b"\xfd\tthree layers\xc3",
    top: &[],
};
/// A clone's GPT disk of one layer whose top directory holds a regular file `etc`, below which
/// nothing can be bound at `/etc/machine-id`.
const ETC_FILE: Disk = Disk {
    guid: "c3d5e7f9-1a2b-4c3d-8e4f-5a6b7c8d9e00",
    partitions: &[
        "c3d5e7f9-1a2b-4c3d-8e4f-5a6b7c8d9e01",
        "c3d5e7f9-1a2b-4c3d-8e4f-5a6b7c8d9e02",
    ],
    alignment: 2048,
    first_size: 4096,
    first: b"\xfcetc is a file",
    top: &[("etc", b"not a directory\n")],
};

/// A GPT disk of the scenarios, of the length of the platform's image, which
/// [`Template::of_disk`] captures as the template's placeholder, or [`Template::disk`] writes for
/// a clone. Its partitions follow each other, each at a multiple of its alignment: its first
/// partition, then a layer, a [`small_layer`], in each of the others, empty but the top one.
struct Disk {
    /// The disk's GUID, which the guest expects of it.
    guid: &'static str,
    /// The unique GUIDs of its partitions: the first partition's, then each layer's, from the base
    /// up.
    partitions: &'static [&'static str],
    /// The number of sectors of which the first sector of each partition is a multiple.
    alignment: u64,
    /// The size of its first partition, in bytes.
    first_size: u64,
    /// The bytes that its first partition holds, before NUL bytes: those that the guest returns,
    /// which end in neither a NUL byte nor whitespace.
    first: &'static [u8],
    /// The regular files in the top directory of its top layer, each a name and its bytes.
    top: &'static [(&'static str, &'static [u8])],
}

impl Disk {
    /// Writes the disk at `path`, of `length` bytes, a file that no one else may change.
    fn write(&self, path: &Path, length: u64) {
        let (layer, top) = (small_layer(&[], &[]), small_layer(self.top, &[]));
        let partitions = self
            .partitions
            .iter()
            .enumerate()
            .map(|(index, guid)| {
                let contents = match index {
                    0 => self.first,
                    _ if index + 1 == self.partitions.len() => top.as_slice(),
                    _ => layer.as_slice(),
                };
                let size = match index {
                    0 => self.first_size,
                    _ => u64::try_from(contents.len()).unwrap(),
                };
                Partition {
                    guid,
                    size,
                    contents: Contents::Bytes(contents),
                }
            })
            .collect::<Vec<_>>();
        let layout = Layout {
            guid: self.guid,
            partitions: &partitions,
            alignment: self.alignment,
            length: Some(length),
        };
        assert_eq!(gpt::write_disk(path, &layout), self.first);
    }
}

/// The template of a scenario: the generation that a template store of its own captured, in the
/// scenario's sandbox, of the platform's copy of the fixtures' Ubuntu EROFS image, or of a GPT
/// [`Disk`] of its length, with [`TEMPLATE_MEMORY`] MiB of RAM, of a capacity of
/// [`TEMPLATE_CAPACITY`] MiB.
struct Template {
    /// The scenario's sandbox, whose store is the template store, until the template drops.
    sandbox: Option<Sandbox>,
    /// The platform's copy of the image, which the clones stage as their image unless they name
    /// another, and the template's placeholder, unless that is a GPT disk.
    image: PathBuf,
    /// The image's identity.
    identity: String,
    /// The fixtures.
    fixtures: Fixtures,
}

impl Template {
    /// Creates the sandbox of the scenario `name`, stages the platform's copy of the image, and
    /// captures the template of the image in the sandbox's store.
    ///
    /// On Linux, a clone's control socket, `generations/1/LAUNCH.sock` in the template store,
    /// after the 32 hexadecimal digits of the launch, must fit the path of a Unix socket, so the
    /// scenarios' names are short.
    fn create(name: &str) -> Self {
        Self::capture(name, None)
    }

    /// Creates the sandbox of the scenario `name`, stages the platform's copy of the image, and
    /// captures the template of the GPT disk `placeholder`, of the image's length, which the
    /// platform stages beside it, as [`create`](Self::create) does.
    fn of_disk(name: &str, placeholder: &Disk) -> Self {
        Self::capture(name, Some(placeholder))
    }

    /// Creates the sandbox of the scenario `name`, stages the platform's copy of the image, and
    /// captures the template of the GPT disk `placeholder`, if any, and of the image otherwise.
    fn capture(name: &str, placeholder: Option<&Disk>) -> Self {
        let mut sandbox = Sandbox::new(name);
        let fixtures = Fixtures::find();
        let image = stage_platform(&sandbox.root.join("platform"), &fixtures);
        let (distro, identity) = match placeholder {
            None => (erofs(&image), fixtures.identity.as_str()),
            Some(disk) => {
                let path = image.with_file_name("placeholder.gpt");
                disk.write(&path, fs::metadata(&image).unwrap().len());
                (prefixed("gpt:", &path), disk.guid)
            }
        };
        let (memory, capacity) = (TEMPLATE_MEMORY.to_string(), TEMPLATE_CAPACITY.to_string());
        let options = [
            OsStr::new("--distro"),
            &distro,
            OsStr::new("--distro-identity"),
            OsStr::new(identity),
            OsStr::new("--memory"),
            OsStr::new(&memory),
            OsStr::new("--memory-capacity"),
            OsStr::new(&capacity),
        ];
        let created = sandbox.create(&options, 0);
        assert_eq!(created["created"], "captured", "{created:?}");
        assert_eq!(created["generation"], GENERATION.to_string(), "{created:?}");
        Self {
            identity: fixtures.identity.clone(),
            sandbox: Some(sandbox),
            image,
            fixtures,
        }
    }

    /// Returns the template store's directory.
    fn dir(&self) -> &Path {
        let sandbox = self.sandbox.as_ref();
        &sandbox
            .expect("the sandbox lasts until the template drops")
            .store
    }

    /// Returns the directory of the template's sandbox, which holds the template store, the
    /// platform's directory, and the directory of the host directories that the scenario's clones
    /// export, [`EXPORTS`].
    fn root(&self) -> &Path {
        let sandbox = self.sandbox.as_ref();
        &sandbox
            .expect("the sandbox lasts until the template drops")
            .root
    }

    /// Returns the template store.
    fn store(&self) -> Store {
        Store::open(self.dir()).unwrap()
    }

    /// Returns the template's generation, as the store's index lists it.
    fn generation(&self) -> Generation {
        let index = self.store().index().unwrap();
        index.get(GENERATION).cloned().expect("the store holds it")
    }

    /// Returns the leases of the template's generation.
    fn leases(&self) -> Vec<Lease> {
        self.store().leases(GENERATION).unwrap()
    }

    /// Returns the payload of the template's generation.
    fn payload(&self) -> PathBuf {
        self.dir()
            .join(GENERATIONS_NAME)
            .join(GENERATION.to_string())
    }

    /// Returns a copy of the platform's image of `length` bytes, beside it, without a manifest,
    /// at `name`.
    fn variant(&self, name: &str, length: u64) -> PathBuf {
        let copy = self.image.with_file_name(name);
        fs::copy(&self.image, &copy).unwrap();
        File::options()
            .write(true)
            .open(&copy)
            .unwrap()
            .set_len(length)
            .unwrap();
        copy
    }

    /// Writes the GPT disk `disk`, of the length of the platform's image, which a clone restores
    /// in place of the template's placeholder, beside the image, at `name`, and returns its path.
    fn disk(&self, name: &str, disk: &Disk) -> PathBuf {
        let path = self.image.with_file_name(name);
        disk.write(&path, fs::metadata(&self.image).unwrap().len());
        path
    }

    /// Stages the template's image in the store of `sandbox`, which it creates, for a clone in
    /// RAM.
    fn stage(&self, sandbox: &mut Sandbox) {
        let distro = erofs(&self.image);
        let options = [
            OsStr::new("--distro"),
            &distro,
            OsStr::new("--distro-identity"),
            OsStr::new(&self.identity),
        ];
        sandbox.run("stage", &options, 0);
    }

    /// Starts a clone in a new sandbox of the scenario `name`, as `start` asks, checks that the
    /// start exits with `status`, and returns the clone.
    fn start(&self, name: &str, start: StartOf<'_>, status: i32) -> Cloned {
        let mut sandbox = Sandbox::new(name);
        let (distro, identity) = match start.image {
            None => (erofs(&self.image), self.identity.as_str()),
            Some(Image::Erofs(path)) => (erofs(path), self.identity.as_str()),
            Some(Image::Gpt(disk, path)) => (prefixed("gpt:", path), disk.guid),
        };
        // NVX restores a clone only over images that no one else may change, so the scratch image
        // lies in a directory of the sandbox's own, as the platform's image lies in the platform's.
        let scratch = scratch_image(&sandbox);
        SandboxStore::create(scratch.parent().unwrap()).unwrap();
        match start.upper {
            Upper::Ram => {}
            Upper::Scratch => {
                private_copy(&self.fixtures.scratch, &scratch);
            }
            Upper::Resized(length) => {
                let file = private_copy(&self.fixtures.scratch, &scratch);
                file.set_len(length).unwrap();
            }
            Upper::Zeros => private_file(&scratch)
                .set_len(DEFAULT_SCRATCH_LENGTH)
                .unwrap(),
            Upper::Used(used) => {
                private_copy(used, &scratch);
            }
        }
        let upper = match start.upper {
            Upper::Ram => OsString::from("ram"),
            _ => prefixed("scratch:", &scratch),
        };
        let identity = OsStr::new(identity);
        if !start.unstaged {
            let mut staging = vec![
                OsStr::new("--distro"),
                &distro,
                OsStr::new("--distro-identity"),
                identity,
            ];
            if start.upper != Upper::Ram {
                staging.extend([OsStr::new("--upper"), &upper]);
            }
            sandbox.run("stage", &staging, 0);
        }
        let generation = GENERATION.to_string();
        let mut options = vec![
            OsStr::new("--template"),
            self.dir().as_os_str(),
            OsStr::new("--generation"),
            OsStr::new(&generation),
            OsStr::new("--distro"),
            &distro,
            OsStr::new("--distro-identity"),
            identity,
            OsStr::new("--upper"),
            &upper,
        ];
        options.extend(start.options.iter().map(OsStr::new));
        let time = start.time.map(|time| OsString::from(host_time_text(&time)));
        let environment: Vec<(&str, &OsStr)> = time
            .iter()
            .map(|time| (HOST_TIME_VARIABLE, time.as_os_str()))
            .collect();
        let started = sandbox.start("start", &options, &environment);
        let started = sandbox.finish(started, status);
        Cloned { sandbox, started }
    }
}

impl Drop for Template {
    /// Removes the template's sandbox, as a sandbox's drop does, unless a clone whose VM might
    /// still run leases the generation: one whose sandbox a test that failed kept, since it could
    /// not confirm that the clone's VM ended. Its VMM restores the generation's snapshot in place,
    /// and reads the platform's image, which the template's sandbox holds too, so the template
    /// stays with it. A clone's sandbox drops before the template, which the scenario creates
    /// first.
    fn drop(&mut self) {
        let Some(sandbox) = self.sandbox.take() else {
            return;
        };
        let kept = Store::open(&sandbox.store)
            .and_then(|store| store.leases(GENERATION))
            .map(|leases| {
                leases
                    .iter()
                    .map(Lease::sandbox)
                    .filter(|clone| clone.exists())
                    .map(|clone| clone.display().to_string())
                    .collect::<Vec<_>>()
            });
        let reason = match kept {
            Ok(kept) if kept.is_empty() => {
                drop(sandbox);
                return;
            }
            Ok(kept) => format!("the clones in {} might still run", kept.join(", ")),
            Err(error) => format!("its leases cannot be read: {}", common::describe(&error)),
        };
        eprintln!("kept {} because {reason}", sandbox.root.display());
        mem::forget(sandbox);
    }
}

/// The upper of a clone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Upper<'a> {
    /// RAM.
    #[default]
    Ram,
    /// A copy of the scratch template, an empty ext4 file system of the template's scratch
    /// length.
    Scratch,
    /// A copy of the scratch template of this many bytes.
    Resized(u64),
    /// Zeros of the template's scratch length, which hold no file system.
    Zeros,
    /// A copy of the scratch image at this path, which an earlier clone used, so that its upper is
    /// not fresh.
    Used(&'a Path),
}

/// A clone's image other than the platform's.
#[derive(Clone, Copy)]
enum Image<'a> {
    /// An EROFS image at this path, a variant of the platform's, with its identity.
    Erofs(&'a Path),
    /// A GPT disk of this layout at this path, which [`Template::disk`] wrote.
    Gpt(&'a Disk, &'a Path),
}

/// What a scenario asks of the start of a clone.
#[derive(Default)]
struct StartOf<'a> {
    /// The image, or the platform's image by default.
    image: Option<Image<'a>>,
    /// The upper.
    upper: Upper<'a>,
    /// The start's other options.
    options: &'a [&'a str],
    /// The host's time at which the store checks the generation's eligibility, as the clock
    /// fixture, or the time that the host program samples by default.
    time: Option<HostTime>,
    /// Whether the sandbox's store stages no image.
    unstaged: bool,
}

/// A clone of a scenario's template, in a sandbox of its own, as its start left it.
struct Cloned {
    /// The clone's sandbox.
    sandbox: Sandbox,
    /// What the start printed.
    started: Printed,
}

impl Cloned {
    /// Checks that the start started the clone, as the [module documentation](self) describes,
    /// leasing the generation, and recorded its VM, which runs.
    fn assert_started(&self) {
        self.assert_mapped(0);
    }

    /// Checks that the start started the clone, as [`assert_started`](Self::assert_started) does,
    /// once its guest applied `mapped` mappings of its export, none for a clone without one.
    fn assert_mapped(&self, mapped: usize) {
        let started = &self.started;
        assert_eq!(started["generation"], GENERATION.to_string(), "{started:?}");
        let lease = &started["lease"];
        assert!(
            lease.len() == 32 && lease.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{started:?}"
        );
        started["downtime"].parse::<u64>().unwrap();
        let mapped = mapped.to_string();
        let facts = ["root", "build", "table", "mapped", "sandbox"].map(|key| &started[key]);
        assert_eq!(
            facts,
            ["ready", GUEST_BUILD, "applied", mapped.as_str(), "started"],
            "{started:?}"
        );
        let record = self.sandbox.record().expect("the start recorded its VM");
        assert_eq!(started["pid"], record.process().pid.to_string());
        assert!(!started.contains_key("released"), "{started:?}");
    }

    /// Runs `probes` in the clone's root, and returns what they found, each fact as
    /// `PROBE.KEY`.
    fn probe(&mut self, probes: &[&str]) -> Printed {
        let probes = probes.iter().map(OsStr::new).collect::<Vec<_>>();
        let printed = self.sandbox.run("probe", &probes, 0);
        assert_eq!(printed["reattached"], "live");
        assert_eq!(printed["pid"], self.started["pid"]);
        printed
    }

    /// Runs one execution of `arguments`, its options and its program, in the clone's root, and
    /// returns what it did, each fact as `exec.1.KEY`.
    fn exec(&mut self, arguments: &[&str]) -> Printed {
        let arguments = arguments.iter().map(OsStr::new).collect::<Vec<_>>();
        let printed = self.sandbox.run("exec", &arguments, 0);
        assert_eq!(printed["reattached"], "live");
        printed
    }

    /// Stops the clone, and checks that it stopped gracefully, and that the stop released the
    /// clone's lease.
    fn stop(&mut self) {
        let stopped = self.sandbox.run("stop", &[], 0);
        let facts = ["stopped", "outcome", "teardown"].map(|key| &stopped[key]);
        assert_eq!(facts, ["graceful", "success 0", "complete"], "{stopped:?}");
        assert_eq!(stopped["released"], self.started["lease"], "{stopped:?}");
        assert!(self.sandbox.record().is_none());
        let store = SandboxStore::open(&self.sandbox.store).unwrap();
        assert!(Lease::of(&store).unwrap().is_none());
    }

    /// Checks that the start refused the clone before any VMM started, and released the lease
    /// that it took, if it took one, with a diagnostic that says `problem`.
    fn assert_refused(&self, problem: &str) {
        let started = &self.started;
        assert!(!started.contains_key("pid"), "{started:?}");
        assert_eq!(started.get("released"), started.get("lease"), "{started:?}");
        let stderr = self.sandbox.stderr();
        assert!(stderr.contains(problem), "{problem:?} in {stderr}");
        self.sandbox.assert_unlaunched();
        let store = SandboxStore::open(&self.sandbox.store).unwrap();
        assert!(Lease::of(&store).unwrap().is_none());
    }

    /// Checks that the start started the clone's VM, which ended before the clone was ready, with
    /// `status`, the guest's status of its activation's failure, and that the start printed the
    /// failure's record, of the request, by `evidence`, at the activation, once the VM exited,
    /// quarantined nothing, and released the lease of `template`'s generation, which no other
    /// clone holds, with the records of the clone's launch.
    fn assert_unactivated(&self, template: &Template, evidence: &str, status: u8) {
        let printed = &self.started;
        assert!(printed.contains_key("pid"), "{printed:?}");
        assert!(!printed.contains_key("root"), "{printed:?}");
        let status = status.to_string();
        assert_eq!(
            failure(printed),
            [
                "request",
                evidence,
                "activation",
                "activating",
                status.as_str(),
                "exited"
            ],
            "{printed:?}"
        );
        assert_eq!(printed["quarantined"], "no");
        assert_eq!(printed["released"], printed["lease"], "{printed:?}");
        assert!(self.sandbox.record().is_none());
        // The release removed the records of the clone's launch.
        let store = SandboxStore::open(&self.sandbox.store).unwrap();
        assert!(CloneRecords::read(&store).unwrap().is_none());
        assert!(template.leases().is_empty());
    }

    /// Returns the clone's scratch image, for a clone over one.
    fn scratch(&self) -> PathBuf {
        scratch_image(&self.sandbox)
    }

    /// Runs [`VIEW`] in the clone's root, as user ID 0, which writes `marker` to the root's
    /// `/marker` last, and returns what it found.
    fn view(&mut self, marker: &str) -> View {
        let program = ["/bin/sh", "-c", VIEW, "sh", marker, WRITTEN_MACHINE_ID];
        let ran = self.exec(&[&["--identity", "0:0", "--"][..], &program].concat());
        let facts = ["launch", "status", "stderr"].map(|key| &ran[&format!("exec.1.{key}")]);
        assert_eq!(facts, ["started", "exit 0", ""], "{ran:?}");
        // The host program escapes each line feed, and each backslash, which none of the lines
        // holds.
        let lines = ran["exec.1.stdout"]
            .strip_suffix("\\n")
            .filter(|lines| !lines.contains("\\\\"))
            .unwrap_or_else(|| panic!("{ran:?}"))
            .split("\\n")
            .collect::<Vec<_>>();
        let value = |line: &str, key: &str| {
            let value = line
                .strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(' '));
            value
                .unwrap_or_else(|| panic!("{key} in {ran:?}"))
                .to_owned()
        };
        let [hostname, machine_id, binds @ .., write, marker] = lines.as_slice() else {
            panic!("{ran:?}");
        };
        View {
            hostname: value(hostname, "hostname"),
            machine_id: value(machine_id, "machine-id"),
            binds: binds.iter().map(|line| value(line, "bind")).collect(),
            write: value(write, "write"),
            marker: value(marker, "marker"),
        }
    }

    /// Runs `script` with `/bin/sh -c` in the clone's root, as user ID 0, checks that it exited
    /// with status 0, and wrote nothing to its standard error, and returns the lines that it
    /// printed.
    fn script(&mut self, script: &str) -> Vec<String> {
        let program = ["/bin/sh", "-c", script, "sh"];
        let ran = self.exec(&[&["--identity", "0:0", "--"][..], &program].concat());
        let facts = ["launch", "status", "stderr"].map(|key| &ran[&format!("exec.1.{key}")]);
        assert_eq!(facts, ["started", "exit 0", ""], "{ran:?}");
        // The host program escapes each line feed, and each backslash, which none of the lines
        // holds.
        ran["exec.1.stdout"]
            .strip_suffix("\\n")
            .filter(|lines| !lines.contains("\\\\"))
            .unwrap_or_else(|| panic!("{ran:?}"))
            .split("\\n")
            .map(str::to_owned)
            .collect()
    }

    /// Returns the configuration of the clone's activation record, which NVX keeps in the clone's
    /// sandbox, in its canonical encoding, while the clone holds its lease.
    fn configuration(&self) -> Configuration {
        let store = SandboxStore::open(&self.sandbox.store).unwrap();
        let encoded = fs::read(store.clone_activation_path()).unwrap();
        let record = ActivationRecord::decode(&encoded).unwrap();
        record.configuration().clone()
    }

    /// Checks the arguments with which NVX started the clone's VMM, on Linux, where the test, as
    /// root, reads them: one `--mount`, which binds the directory of `export`, as the plan
    /// resolved it, to the template's dormant filesystem slot, with the access `mode`, or none,
    /// for a clone without an export, whose restore leaves the slot dormant. Elsewhere, NVX's own
    /// tests of a clone's launch check the arguments, through a fake VMM.
    fn assert_vmm_mount(&self, export: Option<(&Export, &str)>) {
        #[cfg(target_os = "linux")]
        {
            let pid = &self.started["pid"];
            let command_line = fs::read(format!("/proc/{pid}/cmdline")).unwrap();
            let arguments = command_line
                .split(|byte| *byte == 0)
                .map(|argument| String::from_utf8_lossy(argument).into_owned())
                .collect::<Vec<_>>();
            let mounts = arguments
                .windows(2)
                .filter(|pair| pair[0] == "--mount")
                .map(|pair| pair[1].as_str())
                .collect::<Vec<_>>();
            let expected = export
                .map(|(export, mode)| {
                    let directory = fs::canonicalize(&export.root).unwrap();
                    format!("{GUEST_EXPORT},{},{mode}", directory.display())
                })
                .into_iter()
                .collect::<Vec<_>>();
            assert_eq!(mounts, expected, "{arguments:?}");
        }
        #[cfg(not(target_os = "linux"))]
        let _ = export;
    }
}

/// What an execution of [`VIEW`] found in a clone's root.
#[derive(Clone, Debug, PartialEq, Eq)]
struct View {
    /// The hostname.
    hostname: String,
    /// The machine ID at `/etc/machine-id`, or `absent`.
    machine_id: String,
    /// The options of each mount at `/etc/machine-id`.
    binds: Vec<String>,
    /// Whether a write of [`WRITTEN_MACHINE_ID`] at `/etc/machine-id` was `written` or `refused`.
    write: String,
    /// What the root's `/marker` held, or `absent`.
    marker: String,
}

/// A host directory that a scenario's clones export, in the [`EXPORTS`] of the template's
/// sandbox, beside the template store, which neither holds the other.
struct Export {
    /// The directory.
    root: PathBuf,
}

impl Export {
    /// Writes the directory `name` in the [`EXPORTS`] of the sandbox of `template`, with `files`,
    /// each a path in it, with components separated by `/`, and its contents, and the empty
    /// `directories`, and lets anyone create entries in each of its directories, itself included,
    /// so that only a mapping's access refuses what a clone's execution writes there.
    ///
    /// A Linux host shows the guest each host object's owner and mode, which the guest's kernel
    /// checks: on an unprivileged host, the directories belong to the test's user, not to user
    /// ID 0, as which an execution runs without the capabilities that override the check, so they
    /// would refuse its writes. A Windows host shows the guest user ID 0 as each object's owner, so
    /// there the directories keep their access.
    fn write(
        template: &Template,
        name: &str,
        files: &[(&str, &str)],
        directories: &[&str],
    ) -> Self {
        let export = Self {
            root: template.root().join(EXPORTS).join(name),
        };
        fs::create_dir_all(&export.root).unwrap();
        for directory in directories {
            fs::create_dir_all(export.path(directory)).unwrap();
        }
        for (file, contents) in files {
            let path = export.path(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut pending = vec![export.root.clone()];
            while let Some(directory) = pending.pop() {
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o777)).unwrap();
                for entry in fs::read_dir(&directory).unwrap() {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_dir() {
                        pending.push(entry.path());
                    }
                }
            }
        }
        export
    }

    /// Returns the host path of `path`, a path in the directory, with components separated by
    /// `/`, or `.` for the directory itself.
    fn path(&self, path: &str) -> PathBuf {
        path.split('/')
            .filter(|name| *name != ".")
            .fold(self.root.clone(), |host, name| host.join(name))
    }

    /// Returns the options of a start that shares each of `shares` with the clone's root, a path
    /// in the directory, `.` for the directory itself, and its target, with `:ro` for a read-only
    /// share, hides each of `denied`, paths in the directory, and authorizes the [`EXPORTS`].
    fn options(&self, shares: &[(&str, &str)], denied: &[&str]) -> Vec<String> {
        let mut options = Vec::new();
        for (path, target) in shares {
            let share = format!("{}:{target}", self.path(path).display());
            options.extend(["--share".to_owned(), share]);
        }
        for path in denied {
            options.extend(["--deny".to_owned(), self.path(path).display().to_string()]);
        }
        let exports = self.root.parent().unwrap();
        options.extend(["--share-root".to_owned(), exports.display().to_string()]);
        options
    }

    /// Returns what the directory holds, at any depth: each path in it, with components separated
    /// by `/`, and a file's contents, or `None` for a directory.
    fn tree(&self) -> BTreeMap<String, Option<String>> {
        let mut found = BTreeMap::new();
        let mut pending = vec![(self.root.clone(), String::new())];
        while let Some((path, prefix)) = pending.pop() {
            for entry in fs::read_dir(&path).unwrap() {
                let entry = entry.unwrap();
                let name = format!("{prefix}{}", entry.file_name().to_str().unwrap());
                if entry.file_type().unwrap().is_dir() {
                    pending.push((entry.path(), format!("{name}/")));
                    found.insert(name, None);
                } else {
                    found.insert(name, Some(fs::read_to_string(entry.path()).unwrap()));
                }
            }
        }
        found
    }
}

/// Returns the tree of a directory that holds the directories `directories`, and `files`, each a
/// path in it, with components separated by `/`, and its contents, as [`Export::tree`] lists it.
fn tree_of(directories: &[&str], files: &[(&str, &str)]) -> BTreeMap<String, Option<String>> {
    let mut tree = BTreeMap::new();
    for directory in directories {
        tree.insert((*directory).to_owned(), None);
    }
    for (file, contents) in files {
        tree.insert((*file).to_owned(), Some((*contents).to_owned()));
    }
    tree
}

/// Returns the options of a mount, as the `map` probe gives them.
fn mount_options(options: &str) -> BTreeSet<&str> {
    options.split(',').collect()
}

/// Returns the scratch image of a clone over one, in the clone's sandbox, `sandbox`, in a
/// directory of the sandbox's own.
fn scratch_image(sandbox: &Sandbox) -> PathBuf {
    sandbox.root.join("images").join("scratch.ext4")
}

/// Stages the platform's copy of the fixtures' Ubuntu EROFS image in the directory `platform`,
/// beside a copy of its manifest, and returns it.
///
/// A platform stages its trusted artifacts where no one else may change them, and NVX refuses an
/// image that anyone else may change. So the directory is private to the current user, as NVX
/// makes a sandbox's store, whose access control list on Windows passes on to the files created in
/// it.
fn stage_platform(platform: &Path, fixtures: &Fixtures) -> PathBuf {
    SandboxStore::create(platform).unwrap();
    let [image, _] = [&fixtures.distro, &fixtures.manifest].map(|source| {
        let copy = platform.join(source.file_name().unwrap());
        private_copy(source, &copy);
        copy
    });
    image
}

/// Creates the file `path`, which no one else may change, in a directory private to the current
/// user, and returns it, open for writing: of the mode 0644 on Unix, whatever the umask, and on
/// Windows of the directory's access control list.
fn private_file(path: &Path) -> File {
    let file = File::create(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .unwrap();
    }
    file
}

/// Copies the file `source` to `copy`, which [`private_file`] creates, and returns the copy,
/// open for writing.
fn private_copy(source: &Path, copy: &Path) -> File {
    let mut file = private_file(copy);
    io::copy(&mut File::open(source).unwrap(), &mut file).unwrap();
    file
}

/// Returns `path` after `prefix`, as an option's value names a file.
fn prefixed(prefix: &str, path: &Path) -> OsString {
    let mut value = OsString::from(prefix);
    value.push(path);
    value
}

/// Returns the value of `--distro` for the EROFS image at `path`.
fn erofs(path: &Path) -> OsString {
    prefixed("erofs:", path)
}

/// Returns the names of the entries of the directory `path`, sorted.
fn names(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Returns each file below the directory `snapshot`, by its path in it, with its length and its
/// modification time.
fn snapshot_files(snapshot: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut files = Vec::new();
    let mut directories = vec![snapshot.to_owned()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            if metadata.is_dir() {
                directories.push(entry.path());
                continue;
            }
            let path = entry.path().strip_prefix(snapshot).unwrap().to_owned();
            files.push((path, metadata.len(), metadata.modified().unwrap()));
        }
    }
    files.sort();
    files
}

/// Checks that the payload of the template's generation holds what a capture published alone,
/// and, while clones run, their control sockets on Linux: no copy of a scratch image, say.
fn assert_payload(template: &Template, running: bool) {
    let published = [CAPTURE_RECORD_NAME, KEY_NAME, SNAPSHOT_NAME];
    let names = names(&template.payload());
    let (template_files, others): (Vec<_>, Vec<_>) = names
        .iter()
        .partition(|name| published.contains(&name.as_str()));
    assert_eq!(template_files, published, "{names:?}");
    let sockets = running && cfg!(target_os = "linux");
    assert!(
        others
            .iter()
            .all(|name| sockets && name.len() == 37 && name.ends_with(".sock")),
        "{names:?}"
    );
}

/// Checks that a clone's `memory` probe found the memory target `target`, in MiB, online.
fn assert_memory(facts: &Printed, target: u64) {
    let number = |key: &str| facts[&format!("memory.{key}")].parse::<u64>().unwrap();
    let block = number("block-size");
    assert!(
        block > 0 && MEMORY_BLOCK_SIZE.is_multiple_of(block),
        "{facts:?}"
    );
    assert_eq!(number("online"), target * MIB, "{facts:?}");
    assert_eq!(number("blocks") * block, target * MIB, "{facts:?}");
    assert!(number("total") <= target * MIB, "{facts:?}");
}

/// Checks the root of a clone of the platform's image over `upper`, as its `mounts` and `contents`
/// probes found it, in the overlay mode that trusts the lowers, or not.
fn assert_root(facts: &Printed, upper: Upper<'_>, trusted: bool) {
    assert_eq!(facts["mounts.distro.count"], "1", "{facts:?}");
    assert_eq!(facts["mounts.distro.type"], "erofs", "{facts:?}");
    assert_overlay(facts, upper, trusted, "ubuntu");
}

/// Checks the root of a clone of the GPT disk `disk` over `upper`, as its `devices`, `mounts`,
/// `contents`, and `first-partition` probes found it, in the overlay mode that trusts the lowers:
/// each of the disk's layers, an ext4 file system, mounted once, read-only, from the partition of
/// the clone's one read-only disk that the disk's table names, which the overlay stacks, the base
/// last; and the bytes of the disk's first partition.
fn assert_gpt_root(facts: &Printed, disk: &Disk, upper: Upper<'_>) {
    let read_only = facts["devices.disks"]
        .split(' ')
        .filter_map(|device| device.strip_suffix(":ro"))
        .filter_map(|device| device.split(':').next())
        .collect::<Vec<_>>();
    let &[name] = read_only.as_slice() else {
        panic!("the clone has one read-only disk: {facts:?}");
    };
    let numbers = 2..=disk.partitions.len();
    for number in numbers.clone() {
        let fact = |key: &str| facts[&format!("mounts.p{number}.{key}")].as_str();
        let source = format!("/dev/{name}{number}");
        assert_eq!(
            [fact("count"), fact("type"), fact("source")],
            ["1", "ext4", source.as_str()],
            "{facts:?}"
        );
        let options = fact("options").split(',').collect::<Vec<_>>();
        assert!(options.contains(&"ro"), "{facts:?}");
    }
    let lowers = numbers
        .rev()
        .map(|number| format!("{DEFAULT_RUNTIME}/layers/p{number}"))
        .collect::<Vec<_>>();
    let stacked = format!("lowerdir={}", lowers.join(":"));
    let overlay = facts["mounts.root.super"].split(',').collect::<Vec<_>>();
    assert!(
        overlay.contains(&stacked.as_str()),
        "{stacked} in {facts:?}"
    );
    assert_overlay(facts, upper, true, "absent");
    let (length, bytes) = (disk.first.len().to_string(), hexadecimal(disk.first));
    assert_eq!(facts["first-partition.length"], length, "{facts:?}");
    assert_eq!(facts["first-partition.bytes"], bytes, "{facts:?}");
}

/// Checks the upper, the overlay, and the contents of the root of a clone over `upper`, as its
/// `mounts` and `contents` probes found them, in the overlay mode that trusts the lowers, or not,
/// whose `os-release` names `identifier`, or `absent` for a root without one.
fn assert_overlay(facts: &Printed, upper: Upper<'_>, trusted: bool, identifier: &str) {
    let (used, unused, kind) = match upper {
        Upper::Ram => ("tmpfs", "scratch", "tmpfs"),
        _ => ("scratch", "tmpfs", "ext4"),
    };
    assert_eq!(facts[&format!("mounts.{used}.count")], "1", "{facts:?}");
    assert_eq!(facts[&format!("mounts.{used}.type")], kind, "{facts:?}");
    assert_eq!(facts[&format!("mounts.{unused}.count")], "0", "{facts:?}");
    assert_eq!(facts["mounts.root.type"], "overlay", "{facts:?}");
    let overlay = facts["mounts.root.super"].split(',').collect::<Vec<_>>();
    for option in ["metacopy=on", "redirect_dir=on", "xino=on"] {
        assert_eq!(
            overlay.contains(&option),
            trusted,
            "{option} in {overlay:?}"
        );
    }
    let contents = ["os-release.id", "write.root", "write.upper", "lower.write"]
        .map(|key| &facts[&format!("contents.{key}")]);
    assert_eq!(
        contents,
        [identifier, "runtime-adapter", "runtime-adapter", "refused"],
        "{facts:?}"
    );
}

/// Checks that an execution of [`VIEW`] found a machine ID that the clone's guest generated: 32
/// lowercase hexadecimal digits, which one bind holds at `/etc/machine-id`, read-only, without
/// set-user-ID programs or devices, so that the execution's write there was refused.
fn assert_generated(view: &View) {
    let id = &view.machine_id;
    let digit = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
    assert!(id.len() == 32 && id.bytes().all(digit), "{view:?}");
    let [options] = view.binds.as_slice() else {
        panic!("one bind holds the machine ID: {view:?}");
    };
    let options = options.split(',').collect::<Vec<_>>();
    for flag in ["ro", "nosuid", "nodev"] {
        assert!(options.contains(&flag), "{flag} in {view:?}");
    }
    assert_eq!(view.write, "refused", "{view:?}");
}

/// Checks the events of a clone's activation that its `activation` probe found: the repair
/// acknowledged the restore, and the clone took its reset, the host's first message, its Ready,
/// and its start, in that order, after the acknowledgement. Returns when each came, in
/// microseconds after the acknowledgement.
fn assert_timeline(facts: &Printed) -> [u64; 4] {
    assert_eq!(facts["activation.acknowledged"], "yes", "{facts:?}");
    let events = ["reset", "message", "ready", "started"].map(|event| {
        let value = &facts[&format!("activation.{event}")];
        value
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("the clone's {event} is {value}: {facts:?}"))
    });
    assert!(events.is_sorted(), "{facts:?}");
    events
}

/// Returns the record of a clone's failure that a command printed, as `failure.KEY` lines: its
/// outcome, its evidence, the stage at which the clone failed, how far the clone got, the guest's
/// status, and how the teardown ended, each `-` if the command did not print it.
fn failure(printed: &Printed) -> [&str; 6] {
    [
        "outcome", "evidence", "stage", "progress", "status", "teardown",
    ]
    .map(|key| {
        printed
            .get(&format!("failure.{key}"))
            .map_or("-", String::as_str)
    })
}

/// Returns the disks of a probe's `disks` fact, `NAME:NUMBER` or `NAME:NUMBER:ACCESS` each, with
/// their numbers.
fn disks(fact: &str) -> BTreeMap<String, u64> {
    fact.split(' ')
        .map(|disk| {
            let mut parts = disk.split(':');
            let name = parts.next().unwrap().to_owned();
            let number = parts.next().and_then(|number| number.parse().ok());
            (
                name,
                number.unwrap_or_else(|| panic!("{disk:?} in {fact:?}")),
            )
        })
        .collect()
}

/// Returns a boot of this host other than `boot`.
fn other_boot(boot: BootId) -> BootId {
    let text = boot.to_string();
    let first = if text.starts_with('0') { "1" } else { "0" };
    format!("{first}{}", &text[1..]).parse().unwrap()
}

/// The adapter's framing of a clone's control session that sends `early`, which the host must not
/// send before the clone is ready, in place of the clone's Activate, and, once the clone ended its
/// session, waits for the clone's VMM to exit, so that the restore sequence, which then ends the
/// launch, ends it once the VM ended on its own.
struct Early {
    /// What the host sends first.
    early: Message,
    /// The clone's VMM.
    vmm: Process,
    /// Whether the clone's VMM exited once the clone ended the session.
    exited: bool,
}

impl CloneControl for Early {
    type Session = Client;
    type Error = io::Error;

    fn attach(
        &mut self,
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        deadline: Instant,
    ) -> io::Result<Client> {
        Client::attach(transport, capability, deadline).map_err(io::Error::other)
    }

    fn activate(
        &mut self,
        session: &mut Client,
        _record: &[u8],
        deadline: Instant,
    ) -> io::Result<Option<ReadyActivation>> {
        session
            .send(&self.early, Some(deadline))
            .map_err(io::Error::other)?;
        let answer = session.receive(Some(deadline));
        self.exited = self.vmm.wait(deadline)?;
        Err(match answer {
            Ok(message) => io::Error::other(format!("the clone answered {message:?}")),
            Err(error) => io::Error::other(error),
        })
    }

    fn map(
        &mut self,
        _session: &mut Client,
        _table: &[HostMap],
        _deadline: Instant,
    ) -> io::Result<()> {
        unreachable!("a clone that is never ready gets no mapping table")
    }

    fn start(&mut self, _session: &mut Client, _deadline: Instant) -> io::Result<()> {
        Err(io::Error::other("the clone is never ready"))
    }

    fn lost(&self, _error: &io::Error) -> bool {
        false
    }
}

/// Starts a clone of `template` in RAM, in a sandbox of its own, through NVX's public API, whose
/// host sends `early` before the clone's Activate, and checks that the clone ended before it was
/// ready, with the status of the host's session, which attributes the failure to no one, as the
/// sandbox records it, and that the VMM exited, then releases the clone's lease, which removes the
/// record.
fn refuses_early(template: &Template, early: Message) {
    let mut sandbox = Sandbox::new("cl-e");
    template.stage(&mut sandbox);
    let store = template.store();
    let artifacts = env::var_os(Artifacts::ENV_DIR).unwrap();
    let runtime = Runtime::from_dir(artifacts, GUEST_BUILD).unwrap();
    let request = Request {
        distro: template.image.clone(),
        format: ImageFormat::Erofs,
        identity: Uuid::parse(&template.identity).unwrap(),
        scratch: None,
        memory: None,
        workload_identity: None,
        overlay_mode: None,
        scratch_journal: None,
        first_partition_bound: None,
        machine_id: MachineIdMode::Image,
        shares: Shares::default(),
    };
    let plan = request.plan().unwrap();
    let staged = Staged::read(&sandbox.store).unwrap().unwrap();
    let key = store.read_key(&template.generation()).unwrap();
    let clone = request
        .clone_request(plan.clone(), None, &staged.bindings(&plan), key.memory())
        .unwrap();
    let now = HostTime::now().unwrap();
    let mut acquired = store
        .acquire(GENERATION, &sandbox.store, &CallerFiles::none(), &now, None)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let planned = ClonePlan::new(&store, &mut acquired, &clone, deadline).unwrap();
    let restoring = planned
        .launch(&mut acquired, &runtime.artifacts.openvmm, &runtime.identity)
        .unwrap();
    let vmm = Process::open(restoring.starting().process().reference())
        .unwrap()
        .expect("the clone's VMM runs");
    let mut control = Early {
        early: early.clone(),
        vmm,
        exited: false,
    };
    let failure = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap_err();
    let log = || fs::read_to_string(sandbox.store.join(LOG_NAME)).unwrap_or_default();
    let problem = common::describe(&failure);
    assert!(
        !problem.contains("the clone answered"),
        "{early:?}: {problem}"
    );
    assert!(control.exited, "{early:?}: {problem}\n{}", log());
    let record = failure.record();
    let ended = (
        record.outcome,
        record.evidence,
        record.stage,
        record.progress,
        record.status,
        &record.teardown,
    );
    let expected = (
        Outcome::Unknown,
        Evidence::Protocol,
        Stage::Activation,
        Progress::Activating,
        Some(status::ACTIVATION_PROTOCOL),
        &Teardown::Exited,
    );
    assert_eq!(ended, expected, "{early:?}: {problem}\n{}", log());
    assert_eq!(record.lease, acquired.lease.id());
    // The sandbox records the failure, which keeps the clone from launching again, until the
    // release of its lease.
    let records = CloneRecords::read(&acquired.locked).unwrap().unwrap();
    assert_eq!(records.failure.as_ref(), Some(record));
    assert!(records.journal.is_none() && records.runtime.is_none());
    let released = store.release(&mut acquired.locked).unwrap();
    assert_eq!(released.map(|lease| lease.id()), Some(acquired.lease.id()));
    assert!(CloneRecords::read(&acquired.locked).unwrap().is_none());
    assert!(template.leases().is_empty());
}

/// Returns the limits of the executions of the scenarios that the host starts itself.
fn limits() -> Limits {
    Limits::new(16 * 1024, Window::frames(4), Window::frames(8))
        .unwrap()
        .shared_output()
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn one_generation_serves_clones_of_each_upper_and_memory_target() {
    let template = Template::create("cl-reuse");
    let snapshot = template.payload().join(SNAPSHOT_NAME);
    let captured = snapshot_files(&snapshot);

    // Two clones at once: one over a scratch image, at the template's RAM, which trusts its
    // lowers, as the guest does by default, and one in RAM, at twice the template's RAM, which
    // does not.
    let scratch = StartOf {
        upper: Upper::Scratch,
        ..StartOf::default()
    };
    let mut over_scratch = template.start("cl-a", scratch, 0);
    let options = ["--memory", "512", "--overlay-mode", "untrusted-lowers"];
    let in_ram = StartOf {
        options: &options,
        ..StartOf::default()
    };
    let mut in_ram = template.start("cl-b", in_ram, 0);
    for cloned in [&over_scratch, &in_ram] {
        cloned.assert_started();
        // On the boot that captured the template, the host's monotonic clock measures the
        // downtime.
        assert_eq!(
            cloned.started["source"], "monotonic",
            "{:?}",
            cloned.started
        );
    }
    assert_ne!(over_scratch.started["lease"], in_ram.started["lease"]);

    // The store leases the generation to both, by their sandboxes' paths.
    let mut leases = template
        .leases()
        .iter()
        .map(|lease| (lease.id().to_string(), lease.sandbox().to_owned()))
        .collect::<Vec<_>>();
    leases.sort();
    let mut expected = [&over_scratch, &in_ram].map(|cloned| {
        let root = cloned.sandbox.root.file_name().unwrap();
        (
            cloned.started["lease"].clone(),
            Path::new(root).join("store"),
        )
    });
    expected.sort();
    assert_eq!(leases.len(), expected.len(), "{leases:?}");
    for ((lease, sandbox), (expected, store)) in leases.iter().zip(&expected) {
        assert_eq!(lease, expected);
        assert!(sandbox.ends_with(store), "{}", sandbox.display());
    }
    assert_payload(&template, true);

    let probes = ["memory", "mounts", "contents", "activation"];
    let facts = over_scratch.probe(&probes);
    assert_memory(&facts, TEMPLATE_MEMORY);
    assert_root(&facts, Upper::Scratch, true);
    assert_timeline(&facts);
    let facts = in_ram.probe(&probes);
    assert_memory(&facts, 512);
    assert_root(&facts, Upper::Ram, false);
    assert_timeline(&facts);
    over_scratch.stop();
    assert_eq!(template.leases().len(), 1);
    in_ram.stop();
    assert!(template.leases().is_empty());

    // Then another over a scratch image, at the template's RAM capacity.
    let options = ["--memory", "1024"];
    let at_capacity = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };
    let mut at_capacity = template.start("cl-c", at_capacity, 0);
    at_capacity.assert_started();
    let facts = at_capacity.probe(&["memory", "mounts", "contents"]);
    assert_memory(&facts, TEMPLATE_CAPACITY);
    assert_root(&facts, Upper::Scratch, true);
    at_capacity.stop();
    assert!(template.leases().is_empty());

    // The clones changed nothing of the generation's snapshot, which they restored in place, and
    // left nothing in its payload.
    assert_eq!(snapshot_files(&snapshot), captured);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn one_generation_serves_clones_of_either_format_over_either_upper() {
    let template = Template::of_disk("cl-gpt", &PLACEHOLDER);
    let one = template.disk("one.gpt", &ONE_LAYER);
    let three = template.disk("three.gpt", &THREE_LAYERS);

    // At once: the platform's image over a scratch image and in RAM, and two GPT disks, of other
    // layouts than the placeholder's and than each other's, one in RAM, and one over a scratch
    // image, with a bound that its first partition meets exactly.
    let bound = THREE_LAYERS.first_size.to_string();
    let exact = ["--first-partition-bound", bound.as_str()];
    let starts = [
        ("cl-e", None, Upper::Scratch, &[][..]),
        ("cl-f", None, Upper::Ram, &[][..]),
        (
            "cl-g",
            Some(Image::Gpt(&ONE_LAYER, &one)),
            Upper::Ram,
            &[][..],
        ),
        (
            "cl-h",
            Some(Image::Gpt(&THREE_LAYERS, &three)),
            Upper::Scratch,
            &exact[..],
        ),
    ];
    let mut clones = starts.map(|(name, image, upper, options)| {
        let start = StartOf {
            image,
            upper,
            options,
            ..StartOf::default()
        };
        let cloned = template.start(name, start, 0);
        cloned.assert_started();
        (image, upper, cloned)
    });
    assert_eq!(template.leases().len(), clones.len());
    assert_payload(&template, true);
    for (image, upper, cloned) in &mut clones {
        let facts = cloned.probe(&["devices", "mounts", "contents", "first-partition"]);
        match image {
            Some(Image::Gpt(disk, _)) => assert_gpt_root(&facts, disk, *upper),
            _ => {
                assert_root(&facts, *upper, true);
                assert_eq!(facts["first-partition.length"], "none", "{facts:?}");
            }
        }
    }
    for (_, _, cloned) in &mut clones {
        cloned.stop();
    }
    assert!(template.leases().is_empty());

    // A disk whose first partition is larger than the clone's bound ends the clone before it is
    // ready.
    let under = (ONE_LAYER.first_size - 1).to_string();
    let options = ["--first-partition-bound", under.as_str()];
    let start = StartOf {
        image: Some(Image::Gpt(&ONE_LAYER, &one)),
        options: &options,
        ..StartOf::default()
    };
    let ended = template.start("cl-g", start, CLONE_FAILED);
    ended.assert_unactivated(&template, "unusable-root", status::ACTIVATION_ROOT);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_start_refuses_what_the_template_cannot_serve_before_any_vmm_starts() {
    let template = Template::create("cl-refuse");
    let length = fs::metadata(&template.image).unwrap().len();
    let longer = template.variant("longer.erofs", length + 4096);
    let shorter = template.variant("shorter.erofs", length - 4096);
    // OpenVMM attaches block devices of whole 512-byte sectors alone.
    let ragged = template.variant("ragged.erofs", length + 100);
    let placeholder = |found: u64| {
        format!("is {found} bytes long, not the {length} bytes of the template's placeholder")
    };
    let resized = 2 * DEFAULT_SCRATCH_LENGTH;
    let beyond = ["--memory", "2048"];
    let part = ["--memory", "320"];
    let cases = [
        (
            StartOf {
                image: Some(Image::Erofs(&longer)),
                ..StartOf::default()
            },
            "supplied-length",
            placeholder(length + 4096),
        ),
        (
            StartOf {
                image: Some(Image::Erofs(&shorter)),
                upper: Upper::Scratch,
                ..StartOf::default()
            },
            "supplied-length",
            placeholder(length - 4096),
        ),
        (
            StartOf {
                image: Some(Image::Erofs(&ragged)),
                ..StartOf::default()
            },
            "supplied-geometry",
            format!("of {} bytes cannot be attached", length + 100),
        ),
        (
            StartOf {
                upper: Upper::Resized(resized),
                ..StartOf::default()
            },
            "supplied-length",
            format!(
                "is {resized} bytes long, not the {DEFAULT_SCRATCH_LENGTH} bytes of the \
                 template's scratch"
            ),
        ),
        (
            StartOf {
                options: &beyond,
                ..StartOf::default()
            },
            "invalid-targets",
            format!("cannot restore with {} bytes of RAM", 2048 * MIB),
        ),
        (
            StartOf {
                options: &part,
                ..StartOf::default()
            },
            "invalid-targets",
            format!("cannot restore with {} bytes of RAM", 320 * MIB),
        ),
    ];
    for (start, evidence, problem) in cases {
        let refused = template.start("cl-r", start, CLONE_FAILED);
        // NVX's plan refused the request before anything of the clone's launch, so the start
        // released the lease that the store granted.
        assert!(refused.started.contains_key("lease"), "{problem}");
        assert_eq!(
            failure(&refused.started),
            ["request", evidence, "plan", "planned", "none", "no-vm"],
            "{problem}: {:?}",
            refused.started
        );
        assert_eq!(refused.started["quarantined"], "no");
        refused.assert_refused(&problem);
        assert!(template.leases().is_empty());
    }

    // The adapter refuses images that the sandbox does not stage, and releases the lease.
    let start = StartOf {
        unstaged: true,
        ..StartOf::default()
    };
    let refused = template.start("cl-r", start, FAILED);
    assert!(refused.started.contains_key("lease"));
    assert!(!refused.started.contains_key("failure.outcome"));
    refused.assert_refused("; stage the images first");
    assert!(template.leases().is_empty());

    // The store holds no other generation.
    let mut sandbox = Sandbox::new("cl-r");
    template.stage(&mut sandbox);
    let options = [
        OsStr::new("--template"),
        template.dir().as_os_str(),
        OsStr::new("--generation"),
        OsStr::new("2"),
        OsStr::new("--distro"),
        &erofs(&template.image),
        OsStr::new("--distro-identity"),
        OsStr::new(&template.identity),
        OsStr::new("--upper"),
        OsStr::new("ram"),
    ];
    let printed = sandbox.run("start", &options, FAILED);
    assert!(printed.is_empty(), "{printed:?}");
    assert!(sandbox.stderr().contains("holds no generation 2"));
    sandbox.assert_unlaunched();
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_whose_activation_fails_ends_before_it_is_ready() {
    let template = Template::create("cl-unready");
    let forbidden = ["--scratch-journal", "forbidden"];
    let stranger = ["--workload-identity", STRANGER];
    let cases = [
        (
            StartOf {
                upper: Upper::Zeros,
                ..StartOf::default()
            },
            ("unusable-root", status::ACTIVATION_ROOT),
        ),
        (
            StartOf {
                upper: Upper::Scratch,
                options: &forbidden,
                ..StartOf::default()
            },
            ("unusable-root", status::ACTIVATION_ROOT),
        ),
        (
            StartOf {
                options: &stranger,
                ..StartOf::default()
            },
            ("invalid-settings", status::ACTIVATION_WORKLOAD),
        ),
    ];
    for (start, (evidence, expected)) in cases {
        let ended = template.start("cl-u", start, CLONE_FAILED);
        // The VM started, and ended before the clone was ready, with the status of the failure's
        // class, which attributes the failure to the request, and the start released the lease.
        ended.assert_unactivated(&template, evidence, expected);
    }

    // Until the clone is ready, it takes nothing but its Activate: a ping, a start, and the start
    // of an execution, before it, each end the clone with the status of the host's session.
    let timers = Timers::new(Duration::from_secs(1), Duration::from_secs(5));
    let execution = Start::new(Key::new(1, 1), limits(), timers, [b"/bin/true".to_vec()]);
    for early in [
        Message::Ping,
        Message::Start,
        HostMessage::Start(execution).encode(),
    ] {
        refuses_early(&template, early);
    }
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_refuses_a_scratch_image_whose_upper_an_earlier_clone_used() {
    let template = Template::create("cl-fresh");
    // A clone over a scratch image, in whose root the `contents` probe writes a file, which
    // reaches the upper.
    let start = StartOf {
        upper: Upper::Scratch,
        ..StartOf::default()
    };
    let mut earlier = template.start("cl-f", start, 0);
    earlier.assert_started();
    let facts = earlier.probe(&["contents"]);
    assert_eq!(
        facts["contents.write.upper"], "runtime-adapter",
        "{facts:?}"
    );
    earlier.stop();

    // A clone over a copy of its scratch image, whose upper is not fresh, ends before it is
    // ready.
    let used = earlier.scratch();
    let start = StartOf {
        upper: Upper::Used(&used),
        ..StartOf::default()
    };
    let ended = template.start("cl-u", start, CLONE_FAILED);
    ended.assert_unactivated(&template, "unusable-root", status::ACTIVATION_ROOT);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn clones_at_once_keep_their_own_uppers_and_machine_ids() {
    let template = Template::create("cl-view");
    // Three clones at once: two whose guests generate their machine IDs, over a scratch image and
    // in RAM, and one in RAM that keeps the image's own.
    let generated = ["--machine-id", "generated"];
    let starts = [
        ("cl-s", Upper::Scratch, &generated[..]),
        ("cl-r", Upper::Ram, &generated[..]),
        ("cl-i", Upper::Ram, &[][..]),
    ];
    let mut clones = starts.map(|(name, upper, options)| {
        let start = StartOf {
            upper,
            options,
            ..StartOf::default()
        };
        let cloned = template.start(name, start, 0);
        cloned.assert_started();
        (name, cloned)
    });
    assert_eq!(template.leases().len(), clones.len());

    // Each clone's first execution sees the clone's hostname, and a root of the clone's own,
    // without the marker that the executions before it left in the other clones' roots.
    let firsts = clones.each_mut().map(|(name, cloned)| {
        let view = cloned.view(name);
        let seen = [view.hostname.as_str(), view.marker.as_str()];
        assert_eq!(seen, [CLONE_HOSTNAME, "absent"], "{view:?}");
        view
    });
    let [scratch, ram, image] = &firsts;
    assert_generated(scratch);
    assert_generated(ram);
    assert_ne!(scratch.machine_id, ram.machine_id);
    // The guest leaves the image's own machine ID alone: the Ubuntu image has none, and an
    // execution may write one.
    let image_view = (
        image.machine_id.as_str(),
        image.binds.len(),
        image.write.as_str(),
    );
    assert_eq!(image_view, ("absent", 0, "written"), "{image:?}");

    // A later execution of each clone sees the machine ID that its first saw, or, with the image's
    // own, the one that its first wrote, and its first's marker.
    for ((name, cloned), first) in clones.iter_mut().zip(&firsts) {
        let machine_id = match first.binds.as_slice() {
            [] => WRITTEN_MACHINE_ID,
            _ => first.machine_id.as_str(),
        };
        let expected = View {
            machine_id: machine_id.to_owned(),
            marker: (*name).to_owned(),
            ..first.clone()
        };
        assert_eq!(cloned.view(name), expected);
    }
    for (_, cloned) in &mut clones {
        cloned.stop();
    }
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_whose_view_cannot_be_set_ends_before_it_is_ready() {
    // The template of the EROFS image serves clones of a GPT disk of its length too.
    let template = Template::create("cl-noview");
    let disk = template.disk("etc.gpt", &ETC_FILE);
    let image = Some(Image::Gpt(&ETC_FILE, &disk));

    // With the image's own machine ID, the guest binds nothing in the root, so a clone of a disk
    // whose top layer holds a regular file `etc` starts.
    let start = StartOf {
        image,
        ..StartOf::default()
    };
    let mut cloned = template.start("cl-n", start, 0);
    cloned.assert_started();
    let facts = cloned.probe(&["devices", "mounts", "contents", "first-partition"]);
    assert_gpt_root(&facts, &ETC_FILE, Upper::Ram);
    cloned.stop();

    // A generated machine ID, which the guest binds at `/etc/machine-id`, cannot be bound below a
    // file, so a clone of the disk that asks for one ends before it is ready, with the status of
    // the workload's view, which attributes the failure to the request's invalid settings.
    let options = ["--machine-id", "generated"];
    let start = StartOf {
        image,
        options: &options,
        ..StartOf::default()
    };
    let ended = template.start("cl-n", start, CLONE_FAILED);
    ended.assert_unactivated(&template, "invalid-settings", status::ACTIVATION_WORKLOAD);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_runs_its_executions_as_its_workload_identity_once_it_is_ready() {
    let template = Template::create("cl-ident");
    let options = ["--workload-identity", NOBODY];
    let start = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };
    let mut cloned = template.start("cl-i", start, 0);
    cloned.assert_started();
    let facts = cloned.probe(&["activation"]);
    let events = assert_timeline(&facts);
    assert_eq!(facts["activation.execution"], "none", "{facts:?}");

    // Every execution runs as the record's workload identity, and one that names its own is
    // refused before its program starts.
    let ran = cloned.exec(&["--", "/bin/sh", "-c", "id -u; id -g"]);
    let facts = ["launch", "status", "stdout", "stderr"].map(|key| &ran[&format!("exec.1.{key}")]);
    assert_eq!(
        facts,
        ["started", "exit 0", "65534\\n65534\\n", ""],
        "{ran:?}"
    );
    let refused = cloned.exec(&["--identity", "1000:1000", "--", "/bin/true"]);
    let launch = format!("failed 0 {EINVAL}");
    assert_eq!(refused["exec.1.launch"], launch, "{refused:?}");

    // The first execution came once the clone's sandbox started.
    let facts = cloned.probe(&["activation"]);
    let execution = facts["activation.execution"].parse::<u64>().unwrap();
    assert!(execution >= events[3], "{facts:?}");
    assert_eq!(assert_timeline(&facts), events);
    cloned.stop();
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_generation_is_eligible_by_the_clock_of_its_capture() {
    let template = Template::create("cl-clock");
    let contract = template.generation().time().clone();
    let now = HostTime::now().unwrap();
    // The scenario runs on the host's boot that captured the template.
    assert_eq!(
        (now.host_id, now.boot_id, now.clock),
        (contract.host_id, contract.boot_id, contract.host_clock)
    );
    let started = |time: Option<HostTime>| {
        let start = StartOf {
            time,
            ..StartOf::default()
        };
        let mut cloned = template.start("cl-k", start, 0);
        cloned.assert_started();
        cloned.stop();
        cloned.started
    };

    // On that boot, the host's monotonic clock measures the downtime, whatever UTC says.
    let printed = started(None);
    assert_eq!(printed["source"], "monotonic", "{printed:?}");
    let behind = HostTime {
        utc_ns: contract.capture_utc_ns - 24 * HOUR_NS,
        ..now
    };
    let printed = started(Some(behind));
    assert_eq!(printed["source"], "monotonic", "{printed:?}");
    let downtime = behind.monotonic_ns - contract.capture_monotonic_ns;
    assert_eq!(printed["downtime"], downtime.to_string());

    // After a reboot, UTC does.
    let rebooted = HostTime {
        boot_id: other_boot(contract.boot_id),
        monotonic_ns: 1,
        utc_ns: contract.capture_utc_ns + HOUR_NS,
        ..now
    };
    let printed = started(Some(rebooted));
    assert_eq!(printed["source"], "utc", "{printed:?}");
    assert_eq!(printed["downtime"], HOUR_NS.to_string());

    // A generation that is no longer eligible, or not yet, takes no lease.
    let cases = [
        (
            HostTime {
                monotonic_ns: contract.capture_monotonic_ns + MAX_DOWNTIME_NS + 1,
                ..now
            },
            "is no longer eligible: after a downtime of",
            "by the host's monotonic clock",
        ),
        (
            HostTime {
                utc_ns: contract.capture_utc_ns + MAX_DOWNTIME_NS + 1,
                ..rebooted
            },
            "is no longer eligible: after a downtime of",
            "by UTC",
        ),
        (
            HostTime {
                utc_ns: contract.capture_utc_ns - 1,
                ..rebooted
            },
            "is not eligible yet: by UTC",
            "1 ns before the anchor of the capture",
        ),
    ];
    for (time, state, measured) in cases {
        let start = StartOf {
            time: Some(time),
            ..StartOf::default()
        };
        let refused = template.start("cl-k", start, FAILED);
        assert!(refused.started.is_empty(), "{:?}", refused.started);
        let generation = format!("the generation {GENERATION} of the template store");
        refused.assert_refused(&generation);
        let stderr = refused.sandbox.stderr();
        assert!(
            stderr.contains(state) && stderr.contains(measured),
            "{stderr}"
        );
        assert!(template.leases().is_empty());
    }
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_reads_no_more_of_its_disks_than_its_root_needs() {
    let template = Template::create("cl-reads");
    for upper in [Upper::Scratch, Upper::Ram] {
        let start = StartOf {
            upper,
            ..StartOf::default()
        };
        let mut cloned = template.start("cl-d", start, 0);
        cloned.assert_started();
        let facts = cloned.probe(&["devices", "reads"]);
        let sizes = disks(&facts["devices.disks"]);
        let restored = disks(&facts["reads.disks"]);
        assert_eq!(
            sizes.keys().collect::<Vec<_>>(),
            restored.keys().collect::<Vec<_>>(),
            "{facts:?}"
        );
        // The image and the scratch device, whose lengths are the template's.
        let mut lengths = sizes
            .values()
            .map(|sectors| sectors * SECTOR)
            .collect::<Vec<_>>();
        lengths.sort_unstable();
        let mut expected = [
            fs::metadata(&template.image).unwrap().len(),
            DEFAULT_SCRATCH_LENGTH,
        ];
        expected.sort_unstable();
        assert_eq!(lengths, expected, "{facts:?}");
        for (disk, sectors) in &sizes {
            assert!(
                restored[disk] * 8 < *sectors,
                "{upper:?}: the clone read {} of the {sectors} sectors of {disk}",
                restored[disk]
            );
        }

        // A read of each disk to its end, which the statistics count.
        let facts = cloned.probe(&["read-disks"]);
        let read = disks(&facts["read-disks.disks"]);
        let whole = sizes
            .iter()
            .map(|(disk, sectors)| (disk.clone(), sectors * SECTOR))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(read, whole, "{facts:?}");
        let facts = cloned.probe(&["reads"]);
        let after = disks(&facts["reads.disks"]);
        for (disk, sectors) in &sizes {
            assert!(
                after[disk] >= restored[disk] + sectors / 2,
                "{upper:?}: {disk} read {} sectors, then {}, of {sectors}",
                restored[disk],
                after[disk]
            );
        }
        cloned.stop();
    }
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_s_time_settles_after_it_is_ready() {
    let template = Template::create("cl-time");
    let mut cloned = template.start("cl-t", StartOf::default(), 0);
    cloned.assert_started();
    let deadline = Instant::now() + TIME_SETTLEMENT;
    let facts = loop {
        let facts = cloned.probe(&["time"]);
        let status = facts.get("time.restore-status").map(String::as_str);
        if !matches!(status, None | Some("pending")) {
            break facts;
        }
        assert!(Instant::now() < deadline, "{facts:?}");
        thread::sleep(Duration::from_millis(100));
    };
    let settled = [
        "restore-status",
        "violations",
        "last-downtime-source",
        "restore-generation",
    ]
    .map(|key| &facts[&format!("time.{key}")]);
    let generation = &facts["time.generation"];
    assert_eq!(
        settled,
        ["ok", "0", "monotonic", generation.as_str()],
        "{facts:?}"
    );
    let downtime = facts["time.last-downtime-ns"].parse::<u64>().unwrap();
    assert!(downtime > 0, "{facts:?}");

    // The clone still serves its order.
    let facts = cloned.probe(&["activation", "memory"]);
    assert_timeline(&facts);
    assert_memory(&facts, TEMPLATE_MEMORY);
    cloned.stop();
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn each_command_reattaches_to_the_clone_from_a_new_process() {
    let template = Template::create("cl-again");
    let start = StartOf {
        upper: Upper::Scratch,
        ..StartOf::default()
    };
    let mut cloned = template.start("cl-g", start, 0);
    cloned.assert_started();
    // The launch committed: the sandbox records the clone's VM, and how far the clone got, and no
    // failure.
    let store = SandboxStore::open(&cloned.sandbox.store).unwrap();
    let records = CloneRecords::read(&store).unwrap().unwrap();
    assert_eq!(records.lease.id().to_string(), cloned.started["lease"]);
    assert_eq!(records.progress, Progress::Committed);
    assert!(records.runtime.is_some(), "{records:?}");
    assert!(records.journal.is_none() && records.failure.is_none());

    // Each command, from a new process, reattaches to the clone in a new session of the same
    // broker, in which the clone answers the Activate of its record again with its Ready, without
    // activating again, so that the events of its activation stay as they were.
    let first = cloned.sandbox.run("reattach", &[], 0);
    assert_eq!(first["reattached"], "live", "{first:?}");
    assert_eq!(first["pid"], cloned.started["pid"]);
    let facts = cloned.probe(&["activation"]);
    let events = assert_timeline(&facts);
    let ran = cloned.exec(&["--identity", NOBODY, "--", "/bin/sh", "-c", "id -u"]);
    let facts_ran = ["launch", "status", "stdout"].map(|key| &ran[&format!("exec.1.{key}")]);
    assert_eq!(facts_ran, ["started", "exit 0", "65534\\n"], "{ran:?}");
    let last = cloned.sandbox.run("reattach", &[], 0);
    let epochs = [&first, &facts, &ran, &last].map(|printed| {
        assert_eq!(printed["instance"], first["instance"], "{printed:?}");
        printed["epoch"].parse::<u64>().unwrap()
    });
    assert!(
        epochs.windows(2).all(|pair| pair[0] < pair[1]),
        "{epochs:?}"
    );
    let facts = cloned.probe(&["activation"]);
    let again = assert_timeline(&facts);
    assert_eq!(again, events, "{facts:?}");
    let execution = facts["activation.execution"].parse::<u64>().unwrap();
    assert!(execution >= events[3], "{facts:?}");
    cloned.stop();
    let store = SandboxStore::open(&cloned.sandbox.store).unwrap();
    assert!(CloneRecords::read(&store).unwrap().is_none());
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_host_program_of_another_runtime_ends_the_clone_through_its_verified_identity() {
    let template = Template::create("cl-other");
    let cloned = template.start("cl-o", StartOf::default(), 0);
    cloned.assert_started();
    let vmm = cloned.sandbox.vmm().expect("the clone's VMM runs");
    let mut sandbox = cloned.sandbox;

    // The reattach finds the clone's VMM under another runtime identity, which might not speak
    // this host's protocol, so it ends it through its verified identity, without a session, and
    // the clone fails, by the host's evidence, which quarantines nothing.
    let options = [OsStr::new("--guest-build"), OsStr::new(ANOTHER_BUILD)];
    let ended = sandbox.run("reattach", &options, CLONE_FAILED);
    assert_eq!(ended["reattached"], "failed", "{ended:?}");
    assert_eq!(
        failure(&ended),
        [
            "host",
            "runtime-mismatch",
            "reattach",
            "committed",
            "none",
            "terminated"
        ],
        "{ended:?}"
    );
    assert_eq!(ended["failure.code"], "runtime-mismatch");
    assert_eq!(ended["quarantined"], "no");
    assert_eq!(ended["released"], cloned.started["lease"], "{ended:?}");
    assert!(!ended.contains_key("epoch"), "{ended:?}");
    assert!(sandbox.stderr().contains("differs in initramfs"));
    assert!(
        vmm.wait(Instant::now() + Duration::from_secs(10)).unwrap(),
        "the clone's VMM still runs"
    );
    assert!(sandbox.record().is_none());
    let store = SandboxStore::open(&sandbox.store).unwrap();
    assert!(CloneRecords::read(&store).unwrap().is_none());
    assert!(template.leases().is_empty());

    // The template still serves clones.
    let mut another = template.start("cl-p", StartOf::default(), 0);
    another.assert_started();
    another.stop();
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_tombstone_leaves_the_running_clone_of_its_generation_to_its_lease() {
    let template = Template::create("cl-tomb");
    let snapshot = template.payload().join(SNAPSHOT_NAME);
    let captured = snapshot_files(&snapshot);
    let start = StartOf {
        upper: Upper::Scratch,
        ..StartOf::default()
    };
    let mut cloned = template.start("cl-q", start, 0);
    cloned.assert_started();
    let store = template.store();
    let deadline = Instant::now() + Duration::from_secs(10);
    store.tombstone(GENERATION, Some(deadline)).unwrap();
    assert_eq!(store.tombstones().unwrap(), [GENERATION]);

    // The generation takes no new lease.
    let refused = template.start("cl-n", StartOf::default(), FAILED);
    assert!(refused.started.is_empty(), "{:?}", refused.started);
    refused.assert_refused(&format!(
        "a tombstone quarantines the generation {GENERATION} of the template store"
    ));

    // The clone keeps running on its lease: it reattaches, serves its order, and stops, which
    // releases the lease.
    let facts = cloned.probe(&["activation", "memory"]);
    assert_timeline(&facts);
    assert_memory(&facts, TEMPLATE_MEMORY);
    let ran = cloned.exec(&["--identity", NOBODY, "--", "/bin/true"]);
    assert_eq!(ran["exec.1.status"], "exit 0", "{ran:?}");
    assert_eq!(template.leases().len(), 1);
    cloned.stop();
    assert!(template.leases().is_empty());

    // The tombstone changed nothing of the generation's snapshot.
    assert_eq!(snapshot_files(&snapshot), captured);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn the_lease_of_a_start_that_ended_before_its_record_is_released_through_its_template_store() {
    let template = Template::create("cl-lost");
    // A start that ended once the store leased the generation, before it recorded the clone,
    // leaves the lease, and no record that names the template store.
    let mut sandbox = Sandbox::new("cl-l");
    template.stage(&mut sandbox);
    let lease = lease_to(&template, &sandbox);

    // The stop finds that the clone never launched, and keeps the lease without its template
    // store.
    let kept = sandbox.run("stop", &[], FAILED);
    assert_eq!(kept["reattached"], "ended", "{kept:?}");
    assert_eq!(kept["progress"], "planned", "{kept:?}");
    assert!(!kept.contains_key("released"), "{kept:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("records no clone"), "{stderr}");
    assert!(stderr.contains("--template TEMPLATES"), "{stderr}");
    assert_eq!(template.leases().len(), 1);

    // Another template store does not release it.
    let other = sandbox.root.join("other");
    Store::create(&other).unwrap();
    let options = [OsStr::new("--template"), other.as_os_str()];
    let refused = sandbox.run("stop", &options, FAILED);
    assert!(!refused.contains_key("released"), "{refused:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("not of this one"), "{stderr}");
    assert_eq!(template.leases().len(), 1);

    // Its own does, and the sandbox leases the generation again, to a clone whose record the
    // release then leaves.
    let options = [OsStr::new("--template"), template.dir().as_os_str()];
    let released = sandbox.run("reattach", &options, FAILED);
    assert_eq!(released["released"], lease, "{released:?}");
    assert!(template.leases().is_empty());
    let started = start_in(&mut sandbox, &template, template.dir());
    assert_ne!(started["lease"], lease, "{started:?}");
    let stopped = sandbox.run("stop", &[], 0);
    assert_eq!(stopped["released"], started["lease"], "{stopped:?}");
    assert!(template.leases().is_empty());

    // A start of another template's clone in the sandbox that ended as the first did leaves a
    // lease of that template's store, which the earlier clone's record does not name.
    let another = Template::create("cl-gone");
    let lease = lease_to(&another, &sandbox);
    let kept = sandbox.run("stop", &[], FAILED);
    assert!(!kept.contains_key("released"), "{kept:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("which did not grant its lease"), "{stderr}");
    assert!(stderr.contains("--template TEMPLATES"), "{stderr}");
    assert_eq!(another.leases().len(), 1);
    let options = [OsStr::new("--template"), another.dir().as_os_str()];
    let released = sandbox.run("stop", &options, FAILED);
    assert_eq!(released["released"], lease, "{released:?}");
    assert!(another.leases().is_empty());
    let started = start_in(&mut sandbox, &template, another.dir());
    let stopped = sandbox.run("stop", &[], 0);
    assert_eq!(stopped["released"], started["lease"], "{stopped:?}");
    assert!(another.leases().is_empty());
    assert_payload(&template, false);
    assert_payload(&another, false);
}

/// Leases the generation of `template` to the store of `sandbox`, through NVX's store, as a start
/// does before it records its clone, and returns the lease's identity.
fn lease_to(template: &Template, sandbox: &Sandbox) -> String {
    let now = HostTime::now().unwrap();
    let acquired = template
        .store()
        .acquire(GENERATION, &sandbox.store, &CallerFiles::none(), &now, None)
        .unwrap();
    acquired.lease.id().to_string()
}

/// Starts a clone in RAM, in the store of `sandbox`, which staged the image of `template`, of the
/// generation of the template store `templates`, checks that it started, and returns what the
/// start printed.
fn start_in(sandbox: &mut Sandbox, template: &Template, templates: &Path) -> Printed {
    let generation = GENERATION.to_string();
    let distro = erofs(&template.image);
    let options = [
        OsStr::new("--template"),
        templates.as_os_str(),
        OsStr::new("--generation"),
        OsStr::new(&generation),
        OsStr::new("--distro"),
        &distro,
        OsStr::new("--distro-identity"),
        OsStr::new(&template.identity),
        OsStr::new("--upper"),
        OsStr::new("ram"),
    ];
    sandbox.run("start", &options, 0)
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_maps_its_export_beneath_its_root_before_its_sandbox_starts() {
    let template = Template::create("cl-export");
    let export = Export::write(&template, "full", &EXPORT_FILES, &EXPORT_DIRECTORIES);
    let shares = export.options(&EXPORT_SHARES, &["dir/secret"]);
    let options = shares.iter().map(String::as_str).collect::<Vec<_>>();
    let start = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };
    let mut clone = template.start("cl-export-1", start, 0);
    clone.assert_mapped(EXPORT_SHARES.len());

    // The clone's record names the export, a directory, by the identity of its plan, and the
    // number of its mappings, which the restore sequence sent once the clone was ready. NVX bound
    // the directory to the template's dormant filesystem slot, read-write, since some of the
    // mappings are.
    let configuration = clone.configuration();
    let recorded = configuration.export.expect("the record names the export");
    assert_eq!(recorded.kind, ExportKind::Directory);
    assert_eq!(configuration.mappings, EXPORT_SHARES.len());
    clone.assert_vmm_mount(Some((&export, "rw")));

    // The export, mounted once, read-write, without set-user-ID programs or devices, below a
    // directory of the runtime's that only root may enter, and the table's mappings below the
    // root, and nothing else, in their order, as a launch's: each with its access, `nosuid`, and
    // `nodev`, the guest writing the writable ones alone, the one inside a read-only mapping
    // included, each showing what the host holds, but the file that the start hides. The guest
    // created the missing targets in the upper, empty, but none inside another mapping.
    let facts = clone.probe(&[MAP_EXPORT]);
    let fact = |key: &str| facts[&format!("map.{key}")].as_str();
    assert_eq!(
        (fact("export.count"), fact("export.type")),
        ("1", "virtiofs")
    );
    let options = mount_options(fact("export.options"));
    assert!(
        options.is_superset(&BTreeSet::from(["rw", "nosuid", "nodev"])),
        "{options:?}"
    );
    assert_eq!((fact("hostfs.mode"), fact("hostfs.owner")), ("0700", "0:0"));
    assert_eq!(fact("mounts"), EXPORT_MOUNTS);
    for (name, access, kind, write, upper) in [
        ("maps", "ro", "directory", "refused", "directory"),
        ("inner", "rw", "directory", "allowed", "absent"),
        ("file", "ro", "file 4", "refused", "file 0"),
        ("out", "rw", "directory", "allowed", "directory"),
    ] {
        let options = mount_options(fact(&format!("{name}.options")));
        for flag in [access, "nosuid", "nodev"] {
            assert!(options.contains(flag), "{name}: {options:?}");
        }
        let found = ["kind", "write", "upper"].map(|key| fact(&format!("{name}.{key}")));
        assert_eq!(found, [kind, write, upper], "{name}");
    }
    assert_eq!(fact("maps.entries"), "dir,file.txt,out");
    assert_eq!(fact("inner.entries"), "");
    assert_eq!(fact("file.read"), "file");
    assert_eq!(fact("out.entries"), "marker");
    let secret = ["options", "kind", "upper"].map(|key| fact(&format!("secret.{key}")));
    assert_eq!(secret, ["none", "denied", "absent"]);
    assert_eq!(fact("latched"), "no");

    // The clone's workload finds the same, as user ID 0: the files that the mappings show, but
    // not the hidden one, and the writable mappings alone take what it writes.
    assert_eq!(
        clone.script(EXPORTED),
        [
            "read /srv/maps/dir/marker marker",
            "read /srv/file.txt file",
            "secret refused",
            "listing inner,marker,",
            "write /srv/maps refused",
            "write /srv/maps/dir/inner written",
            "write /srv/out written",
        ]
    );

    // The host's directory holds what the guest wrote through the writable mappings, and nothing
    // else changed, before the stop, which tears the root down, and after it.
    let mut expected = tree_of(&EXPORT_DIRECTORIES, &EXPORT_FILES);
    for (file, contents) in [
        ("dir/inner/written", "runtime-adapter"),
        ("dir/inner/workload", "workload"),
        ("out/written", "runtime-adapter"),
        ("out/workload", "workload"),
    ] {
        expected.insert(file.to_owned(), Some(contents.to_owned()));
    }
    assert_eq!(export.tree(), expected);
    clone.stop();
    assert_eq!(export.tree(), expected);
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn clones_at_once_see_only_their_own_exports() {
    let template = Template::create("cl-exports");
    let own = Export::write(&template, "own", &[("marker", "own")], &[]);
    let common = Export::write(&template, "common", &[("marker", "common")], &[]);
    let writable = own.options(&[(".", "/srv/data")], &[]);
    let mut generated = writable.iter().map(String::as_str).collect::<Vec<_>>();
    generated.extend(["--machine-id", "generated"]);
    let read_only = common.options(&[(".", "/srv/data:ro")], &[]);
    let read_only = read_only.iter().map(String::as_str).collect::<Vec<_>>();
    // Four clones of one generation at once: one with a read-write export of its own and a
    // generated machine ID, two over the same read-only export, over a scratch image and in RAM,
    // and one without an export.
    let starts = [
        (
            "cl-own",
            StartOf {
                options: &generated,
                ..StartOf::default()
            },
        ),
        (
            "cl-common",
            StartOf {
                upper: Upper::Scratch,
                options: &read_only,
                ..StartOf::default()
            },
        ),
        (
            "cl-common-2",
            StartOf {
                options: &read_only,
                ..StartOf::default()
            },
        ),
        ("cl-none", StartOf::default()),
    ];
    let mut clones = starts.map(|(name, start)| template.start(name, start, 0));
    let [with_own, with_common, with_same, without] = &mut clones;
    for clone in [&*with_own, &*with_common, &*with_same] {
        clone.assert_mapped(1);
    }
    without.assert_started();

    // Each record names its clone's export, a directory, by the identity of its plan, which the
    // clones of the same export share, and one mapping; the clone without an export has none, and
    // no mapping, and NVX restored it with no `--mount`, which leaves the template's filesystem
    // slot dormant. NVX bound each directory to the slot with the access that its mapping needs.
    let exported = |clone: &Cloned| {
        let configuration = clone.configuration();
        let export = configuration
            .export
            .map(|export| (export.kind, export.plan));
        (export, configuration.mappings)
    };
    let (own_export, own_mappings) = exported(with_own);
    let (common_export, common_mappings) = exported(with_common);
    let (own_export, common_export) = (own_export.unwrap(), common_export.unwrap());
    assert_eq!((own_export.0, own_mappings), (ExportKind::Directory, 1));
    assert_eq!(
        (common_export.0, common_mappings),
        (ExportKind::Directory, 1)
    );
    assert_ne!(own_export.1, common_export.1);
    assert_eq!(exported(with_same), (Some(common_export), 1));
    assert_eq!(exported(without), (None, 0));
    with_own.assert_vmm_mount(Some((&own, "rw")));
    with_common.assert_vmm_mount(Some((&common, "ro")));
    with_same.assert_vmm_mount(Some((&common, "ro")));
    without.assert_vmm_mount(None);

    // Each clone with an export mounted it read-write, even where every mapping of its table is
    // read-only, as a cold launch's would not be, since OpenVMM keeps the export's access, and each
    // mapping carries its own. Below the root, beside its mapping, the mount table shows the bind
    // of a generated machine ID. Each shows its own directory, and takes what its access allows.
    for (clone, marker, access, write, mounts) in [
        (
            &mut *with_own,
            "own",
            "rw",
            "allowed",
            "/etc/machine-id,/srv/data",
        ),
        (&mut *with_common, "common", "ro", "refused", "/srv/data"),
        (&mut *with_same, "common", "ro", "refused", "/srv/data"),
    ] {
        let facts = clone.probe(&[MAP_DATA]);
        let fact = |key: &str| facts[&format!("map.{key}")].as_str();
        assert_eq!(fact("export.count"), "1", "{facts:?}");
        let options = mount_options(fact("export.options"));
        assert!(
            options.is_superset(&BTreeSet::from(["rw", "nosuid", "nodev"])),
            "{facts:?}"
        );
        assert_eq!(fact("mounts"), mounts, "{facts:?}");
        assert!(
            mount_options(fact("data.options")).contains(access),
            "{facts:?}"
        );
        let data = ["kind", "entries", "write"].map(|key| fact(&format!("data.{key}")));
        assert_eq!(data, ["directory", "marker", write], "{facts:?}");
        let size = format!("file {}", marker.len());
        let found = ["kind", "read"].map(|key| fact(&format!("marker.{key}")));
        assert_eq!(found, [size.as_str(), marker], "{facts:?}");
    }
    // The clone without an export mounted none, and maps nothing.
    let facts = without.probe(&[MAP_DATA]);
    let fact = |key: &str| facts[&format!("map.{key}")].as_str();
    assert_eq!(fact("export.count"), "0", "{facts:?}");
    assert!(!facts.contains_key("map.export.type"), "{facts:?}");
    assert_eq!(fact("hostfs.mode"), "absent", "{facts:?}");
    assert_eq!(fact("mounts"), "", "{facts:?}");
    let found = ["data.options", "data.kind", "marker.kind"].map(fact);
    assert_eq!(found, ["none", "absent", "absent"], "{facts:?}");

    // The read-only bind of the generated machine ID holds even against the guest's root: the
    // helper of the `entries` probe, which writes the root's own machine ID first, finds its file
    // system read-only.
    with_own
        .sandbox
        .run("probe", &[OsStr::new("entries")], FAILED);
    let stderr = with_own.sandbox.stderr();
    assert!(stderr.contains("Read-only file system"), "{stderr}");

    // The host's directories hold what the clone of the writable export wrote, and nothing else.
    let written = [("marker", "own"), ("written", "runtime-adapter")];
    assert_eq!(own.tree(), tree_of(&[], &written));
    assert_eq!(common.tree(), tree_of(&[], &[("marker", "common")]));
    for clone in &mut clones {
        clone.stop();
    }
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_start_refuses_an_export_that_a_dormant_filesystem_slot_cannot_bind_before_any_vmm_starts() {
    let template = Template::create("cl-unbound");
    let one = Export::write(&template, "one", &[("marker", "one")], &[]);
    let two = Export::write(&template, "two", &[("marker", "two")], &[]);
    // Two directories, neither inside the other, which the plan exports as two children of an
    // aggregate export, and a file alone, which the plan exports as its one child.
    let mut several = one.options(&[(".", "/srv/one")], &[]);
    several.extend([
        "--share".to_owned(),
        format!("{}:/srv/two", two.root.display()),
    ]);
    let file = one.options(&[("marker", "/srv/marker:ro")], &[]);
    let slot = "the export of a clone binds one directory to its template's dormant filesystem \
                slot, but its export plan exports";
    for (name, options, problem) in [
        (
            "cl-several",
            several,
            format!("{slot} 2 directories and files"),
        ),
        ("cl-file", file, format!("{slot} the file ")),
    ] {
        let options = options.iter().map(String::as_str).collect::<Vec<_>>();
        let start = StartOf {
            options: &options,
            ..StartOf::default()
        };
        let refused = template.start(name, start, CLONE_FAILED);
        refused.assert_refused(&problem);
        let printed = &refused.started;
        assert_eq!(
            failure(printed),
            [
                "request",
                "invalid-mappings",
                "plan",
                "planned",
                "none",
                "no-vm"
            ],
            "{printed:?}"
        );
        assert_eq!(printed["quarantined"], "no", "{printed:?}");
    }
    assert!(template.leases().is_empty());
    assert_eq!(one.tree(), tree_of(&[], &[("marker", "one")]));
    assert_eq!(two.tree(), tree_of(&[], &[("marker", "two")]));
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_whose_mapping_fails_ends_before_its_sandbox_starts() {
    let template = Template::create("cl-mapfail");
    let export = Export::write(&template, "data", &[("out/marker", "marker")], &["out"]);
    // The table's second mapping, after the directory's own, puts the directory `out` onto
    // `/usr/lib/os-release`, a file of NVX's Ubuntu image, which the guest refuses once it
    // applied the first.
    let shares = export.options(
        &[(".", "/srv/maps:ro"), ("out", "/usr/lib/os-release")],
        &[],
    );
    let options = shares.iter().map(String::as_str).collect::<Vec<_>>();
    let start = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };
    let failed = template.start("cl-mapfail-1", start, CLONE_FAILED);
    let printed = &failed.started;

    // The clone was ready, but the restore sequence sent no start once its guest failed the
    // table: the failure is the start's, at the clone's Ready, by the evidence of the protocol,
    // since the guest's refusal does not tell the request's mappings from a failure of its own.
    // The clone order asked the guest to stop, which tears the root down, and NVX then ended the
    // launch at once, which found the VM exited, with the guest's success, or terminated it,
    // whether or not OpenVMM had reported the guest's success yet.
    assert!(printed.contains_key("pid"), "{printed:?}");
    assert_eq!(
        (printed["root"].as_str(), printed["build"].as_str()),
        ("ready", GUEST_BUILD)
    );
    for key in ["table", "mapped", "sandbox"] {
        assert!(!printed.contains_key(key), "{printed:?}");
    }
    let [outcome, evidence, stage, progress, status, teardown] = failure(printed);
    assert_eq!(
        [outcome, evidence, stage, progress],
        ["unknown", "protocol", "start", "ready"],
        "{printed:?}"
    );
    assert!(
        matches!(
            (status, teardown),
            ("0", "exited" | "terminated") | ("none", "terminated")
        ),
        "{printed:?}"
    );
    let stderr = failed.sandbox.stderr();
    // The guest applied the directory's own mapping, the table's first, and refused the second:
    // the guest's own report, which no other failure of the table prints.
    assert!(
        stderr.contains(
            "the clone did not apply its mapping table: the mapping table failed after 1 \
             mapping: "
        ),
        "{stderr}"
    );
    // Nothing of the clone runs: the failure quarantined nothing, and the start released the
    // lease, with the records of the clone's launch.
    assert_eq!(printed["quarantined"], "no", "{printed:?}");
    assert_eq!(printed["released"], printed["lease"], "{printed:?}");
    assert!(failed.sandbox.record().is_none());
    let store = SandboxStore::open(&failed.sandbox.store).unwrap();
    assert!(CloneRecords::read(&store).unwrap().is_none());
    assert!(template.leases().is_empty());
    // The guest wrote nothing to the host's directory.
    assert_eq!(
        export.tree(),
        tree_of(&["out"], &[("out/marker", "marker")])
    );
    assert_payload(&template, false);
}
