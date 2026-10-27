//! The probes that the worker runs on its assembled root: named checks of a scenario, each of which
//! reports what it finds as [`Facts`].
//!
//! | Probe | Facts |
//! | --- | --- |
//! | `mounts` | For the mount of each layer, by the name of its mount point, `distro` for an EROFS image, and `pN` for the layer in partition N of a GPT disk, the scratch's, `scratch`, the tmpfs's, `tmpfs`, and the overlay's, `root`: the number of mounts at its mount point, `NAME.count`, and of the topmost, its `NAME.type`, `NAME.source`, `NAME.options`, and `NAME.super`, the file system's own options |
//! | `contents` | The root's `os-release.id` and `root.mode`. It writes [`PROBE_FILE`] in the root, and reports what the root, `write.root`, the upper, `write.upper`, and the top layer, `write.lower`, hold at that name, and whether the top layer refuses a new file, `lower.write` |
//! | `copy-up FILE` | It sets the mode of FILE, a regular file of the layers, to the mode that it has, which copies the file up, and reports its size in the topmost layer that holds it, `lower.size`, and in the upper, `upper.size`; whether the upper's copy holds the file's data, `upper.data`: `copied`, `none` once the overlay copied up the file's metadata alone, or `different`; and whether the root still shows the layer's data, `root.data`: `intact` or `different` |
//! | `rename DIRECTORY` | It renames DIRECTORY, a directory of the layers, to `.runtime-adapter-renamed` at the top of the root, and back if it can: `outcome` is `allowed`, `refused` once the overlay refused it with `EXDEV`, or `failed: PROBLEM` |
//! | `fill` | It writes a file in the root until the upper is full, then removes it, and writes a file there again: the upper's size, `total`, and the bytes available in it before, `available`, in bytes; the bytes that it wrote, `written`; the error that ended the writes, `error`: `ENOSPC` once the upper was full, or the problem; what the second file holds, `after`; and the guest's memory, `memory`, in bytes |
//! | `devices` | The guest's disks, the block devices that a device backs: `disks`, each as `NAME:SECTORS:ACCESS`, its size in 512-byte sectors and its access mode, `ro` or `rw`, in the order of their names, separated by spaces |
//! | `reads` | The sectors that the guest read from each of its disks, by the disk's statistics: `disks`, each as `NAME:SECTORS`, in 512-byte sectors, in the order of their names, separated by spaces |
//! | `read-disks` | It reads each of the guest's disks from its start to its end, through the page cache: `disks`, each as `NAME:BYTES`, the bytes that it read, in the order of their names, separated by spaces |
//! | `memory` | The guest's memory: the size of its memory blocks, `block-size`, how many are online, `blocks`, and their bytes, `online`, and the memory that the kernel manages, `total`, each in bytes |
//! | `processors` | The guest's CPUs, as the kernel lists them: those that are online, `online`, present, `present`, and possible, `possible`; and the CPUs that the restore of a clone brought online, as the time ABI's record of the restore names them, `restored`, `none` for none, or `absent` without a record |
//! | `time` | The guest's time state, [`TIME_STATE`], which NVX's time service keeps: each line `KEY=VALUE` as a fact, with each `_` of its key as `-`, in order, leaving out a key that no fact may have, and a repeated one |
//! | `first-partition` | The bytes of the first partition of a GPT disk that the assembly returned, without their trailing padding: how many, `length`, and the bytes, `bytes`, in hexadecimal, or `none` for both for an EROFS image |
//! | `cgroups` | The file system at the cgroups' parent, `parent.type`, and its `parent.subtree-control`; whether each cgroup exists, `agent.exists` and `workload.exists`; `agent.memory.low`, `workload.memory.max`, and `workload.pids.max`; the number of processes in each, `agent.processes` and `workload.processes`; and `user-namespaces`, the guest's limit of user namespaces, or `unavailable` without them |
//! | `processes` | For PID 1, `pid1`, the worker, `worker`, and PID 1's unrelated child, `sibling`: `NAME.pid`, and while the process exists, `NAME.state`, `NAME.ppid`, `NAME.root`, `NAME.cgroup`, `NAME.command`, and `NAME.namespaces`: `same` if it shares PID 1's, or the ones that differ |
//! | `hold` | It opens the root, and keeps it open until the worker ends, so that the teardown finds the root busy: `held`, the path that it holds |
//!
//! FILE and DIRECTORY are relative paths in the root, of normal components, and the other probes take
//! no arguments. A value that a probe cannot read is `absent` if its file does not exist, and
//! otherwise `unreadable: PROBLEM`. The probes of the mappings beneath the root, and of the host
//! export that they come from, are those of the [`mapping`](super::mapping) module. The worker of
//! a clone also answers the probe of the clone's activation, `activation::ACTIVATION_PROBE`, as
//! the `activation` module describes.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use nix::errno::Errno;
use nix::sys::statvfs;

use crate::probe::{Facts, Probe, valid_key};

/// The file that the `contents` probe writes at the top of the root.
pub const PROBE_FILE: &str = ".runtime-adapter-probe";
/// What the `contents` probe writes in [`PROBE_FILE`].
pub const PROBE_CONTENTS: &str = "runtime-adapter";
/// The file that the `contents` probe tries to create at the top of the top layer.
const LOWER_FILE: &str = ".runtime-adapter-lower";
/// The name to which the `rename` probe renames its directory, at the top of the root.
const RENAMED_TO: &str = ".runtime-adapter-renamed";
/// The file that the `fill` probe writes until the upper is full.
const FILL_FILE: &str = ".runtime-adapter-fill";
/// How many bytes the `fill` probe writes at a time.
const FILL_CHUNK: usize = 1 << 20;
/// How many bytes the `fill` probe writes beyond those available in the upper before it stops,
/// which a file system that keeps its size never takes.
const FILL_SLACK: u64 = 16 << 20;
/// The guest's memory information, whose `MemTotal` the `fill` and `memory` probes report.
const MEMORY_INFORMATION: &str = "/proc/meminfo";
/// The guest's memory blocks, of which the `memory` probe counts those online.
const MEMORY_BLOCKS: &str = "/sys/devices/system/memory";
/// The guest's CPUs, which the `processors` probe lists.
const CPUS: &str = "/sys/devices/system/cpu";
/// The time ABI's record of the restore of a clone, whose `new_cpus=` line names the CPUs that the
/// restore brought online.
const TIME_RESTORE: &str = "/run/nvx/time-restore";
/// The guest's block devices.
const BLOCK_DEVICES: &str = "/sys/block";
/// The directory of the guest's device nodes, through which the `read-disks` probe reads its disks.
const DEVICE_NODES: &str = "/dev";
/// How many bytes the `read-disks` probe reads at a time.
const READ_CHUNK: usize = 1 << 20;
/// The guest's time state, which the `time` probe reports.
pub const TIME_STATE: &str = "/run/nvx/time/state";
/// The mount table of the worker's mount namespace.
pub(super) const MOUNTINFO: &str = "/proc/self/mountinfo";
/// The guest's limit of user namespaces, which only a kernel with user namespaces has.
const USER_NAMESPACES: &str = "/proc/sys/user/max_user_namespaces";
/// The namespaces that the `processes` probe compares with PID 1's, of those that the guest's
/// kernel has.
const NAMESPACES: [&str; 7] = ["cgroup", "ipc", "mnt", "net", "pid", "user", "uts"];

/// What the probes check: the worker's root and cgroups, and PID 1's unrelated child.
#[derive(Clone, Copy, Debug)]
pub struct Context<'a> {
    /// The runtime directory, below which `nvx::guest::root` mounts the root: the layers below
    /// `layers`, the upper, an ext4 scratch at `scratch` or a tmpfs at `tmpfs`, and the overlay,
    /// the root, at `rootfs`.
    pub runtime: &'a Path,
    /// The root, the overlay's mount point.
    pub root: &'a Path,
    /// The mount points of the root's layers, the base first: `layers/distro`, the one of an EROFS
    /// image, or `layers/p2` to `layers/pN`, those of the layers of a GPT disk.
    pub lowers: &'a [PathBuf],
    /// The upper's mount point, which holds the overlay's `upper` directory.
    pub upper: &'a Path,
    /// The bytes of the first partition of a GPT disk that the assembly returned, or `None` for an
    /// EROFS image.
    pub first_partition: Option<&'a [u8]>,
    /// The agent's cgroup.
    pub agent: &'a Path,
    /// The workload cgroup.
    pub workload: &'a Path,
    /// The process ID of PID 1's unrelated child, if PID 1 named one.
    pub sibling: Option<u32>,
}

impl Context<'_> {
    /// Returns the top layer's mount point, which an assembled root has.
    fn top(&self) -> io::Result<&Path> {
        self.lowers
            .last()
            .map(PathBuf::as_path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the root has no layer"))
    }

    /// Returns the scratch's mount point.
    fn scratch(&self) -> PathBuf {
        self.runtime.join("scratch")
    }

    /// Returns the tmpfs's mount point.
    fn tmpfs(&self) -> PathBuf {
        self.runtime.join("tmpfs")
    }

    /// Returns the overlay's `upper` directory.
    pub(super) fn upper_directory(&self) -> PathBuf {
        self.upper.join("upper")
    }
}

/// Runs `probe` with `context`, and returns its facts. `held` keeps the descriptors that a probe
/// keeps open until the worker ends.
///
/// Fails with the problem for a probe that the [module documentation](self) does not list, one
/// with arguments, and one that fails.
pub fn run(probe: &Probe, context: &Context<'_>, held: &mut Vec<File>) -> Result<Facts, String> {
    let name = probe.name();
    let probes = [
        "mounts",
        "contents",
        "copy-up",
        "rename",
        "fill",
        "devices",
        "reads",
        "read-disks",
        "memory",
        "processors",
        "time",
        "first-partition",
        "cgroups",
        "processes",
        "hold",
    ];
    if !probes.contains(&name) {
        return Err(format!("the worker runs no probe named {name}"));
    }
    let path = match (name, probe.arguments()) {
        ("copy-up" | "rename", [argument]) => Some(path_argument(name, argument)?),
        ("copy-up" | "rename", _) => return Err(format!("the {name} probe takes one argument")),
        (_, []) => None,
        _ => return Err(format!("the {name} probe takes no arguments")),
    };
    let facts = match (name, path) {
        ("mounts", _) => mounts(context),
        ("contents", _) => contents(context),
        ("copy-up", Some(file)) => copy_up(context, file),
        ("rename", Some(directory)) => rename(context, directory),
        ("fill", _) => fill(context),
        ("devices", _) => devices(),
        ("reads", _) => reads(),
        ("read-disks", _) => read_disks(),
        ("memory", _) => memory(),
        ("processors", _) => Ok(processors()),
        ("time", _) => time(),
        ("first-partition", _) => Ok(first_partition(context)),
        ("cgroups", _) => cgroups(context),
        ("processes", _) => processes(context),
        _ => hold(context, held),
    };
    facts.map_err(|error| format!("the {name} probe failed: {error}"))
}

/// Returns the path that the argument `argument` of the probe `name` names in the root: a relative
/// path of normal components, or the problem.
pub(super) fn path_argument<'a>(name: &str, argument: &'a str) -> Result<&'a Path, String> {
    let path = Path::new(argument);
    let mut components = path.components().peekable();
    let normal = components.peek().is_some()
        && components.all(|component| matches!(component, Component::Normal(_)));
    if normal {
        Ok(path)
    } else {
        Err(format!(
            "the {name} probe takes a relative path in the root, not {argument:?}"
        ))
    }
}

/// Runs the `mounts` probe.
fn mounts(context: &Context<'_>) -> io::Result<Facts> {
    let table = mount_table(&fs::read_to_string(MOUNTINFO)?)?;
    let mut facts = Facts::new();
    let lowers = context.lowers.iter().map(|lower| {
        let name = lower.file_name().unwrap_or_default().to_string_lossy();
        (name.into_owned(), lower.clone())
    });
    let others = [
        ("scratch", context.scratch()),
        ("tmpfs", context.tmpfs()),
        ("root", context.root.to_owned()),
    ];
    let points = lowers.chain(others.map(|(name, point)| (name.to_owned(), point)));
    for (name, point) in points {
        let mounts = table
            .iter()
            .filter(|mount| mount.point == point)
            .collect::<Vec<_>>();
        facts.push(format!("{name}.count"), mounts.len().to_string());
        // Of the mounts at a point, the table lists the topmost last.
        if let Some(mount) = mounts.last() {
            facts.push(format!("{name}.type"), mount.kind.as_str());
            facts.push(format!("{name}.source"), mount.source.as_str());
            facts.push(format!("{name}.options"), mount.options.as_str());
            facts.push(format!("{name}.super"), mount.super_options.as_str());
        }
    }
    Ok(facts)
}

/// Runs the `contents` probe.
fn contents(context: &Context<'_>) -> io::Result<Facts> {
    let root = context.root;
    let mut facts = Facts::new();
    // The image's own file first: `/etc/os-release` may be a link that resolves outside the root.
    let identifier = ["usr/lib/os-release", "etc/os-release"]
        .iter()
        .find_map(|path| fs::read_to_string(root.join(path)).ok())
        .and_then(|text| os_release_id(&text).map(str::to_owned));
    facts.push("os-release.id", identifier.as_deref().unwrap_or("absent"));
    let mode = fs::metadata(root)?.permissions().mode() & 0o7777;
    facts.push("root.mode", format!("{mode:o}"));
    fs::write(root.join(PROBE_FILE), PROBE_CONTENTS)?;
    facts.push("write.root", read(&root.join(PROBE_FILE)));
    facts.push(
        "write.upper",
        read(&context.upper_directory().join(PROBE_FILE)),
    );
    let top = context.top()?;
    facts.push("write.lower", read(&top.join(PROBE_FILE)));
    let lower = match File::create_new(top.join(LOWER_FILE)) {
        Ok(_) => "allowed".to_owned(),
        Err(error) if error.raw_os_error() == Some(Errno::EROFS as i32) => "refused".to_owned(),
        Err(error) => format!("failed: {error}"),
    };
    facts.push("lower.write", lower);
    Ok(facts)
}

/// Runs the `copy-up` probe on `file`.
fn copy_up(context: &Context<'_>, file: &Path) -> io::Result<Facts> {
    let merged = context.root.join(file);
    let lower = context
        .lowers
        .iter()
        .rev()
        .map(|layer| layer.join(file))
        .find(|path| path.exists())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no layer holds {}", file.display()),
            )
        })?;
    let lower = fs::read(lower)?;
    // A change of the file's metadata alone, which copies the file up.
    let permissions = fs::metadata(&merged)?.permissions();
    fs::set_permissions(&merged, permissions)?;
    let copy = context.upper_directory().join(file);
    let mut facts = Facts::new();
    facts.push("lower.size", lower.len().to_string());
    facts.push("upper.size", fs::metadata(&copy)?.len().to_string());
    let copied = fs::read(&copy)?;
    let data = if copied == lower {
        "copied"
    } else if copied.iter().all(|byte| *byte == 0) {
        "none"
    } else {
        "different"
    };
    facts.push("upper.data", data);
    let shown = if fs::read(&merged)? == lower {
        "intact"
    } else {
        "different"
    };
    facts.push("root.data", shown);
    Ok(facts)
}

/// Runs the `rename` probe on `directory`.
fn rename(context: &Context<'_>, directory: &Path) -> io::Result<Facts> {
    let (from, to) = (context.root.join(directory), context.root.join(RENAMED_TO));
    let outcome = match fs::rename(&from, &to) {
        Ok(()) => {
            fs::rename(&to, &from)?;
            "allowed".to_owned()
        }
        Err(error) if error.raw_os_error() == Some(Errno::EXDEV as i32) => "refused".to_owned(),
        Err(error) => format!("failed: {error}"),
    };
    let mut facts = Facts::new();
    facts.push("outcome", outcome);
    Ok(facts)
}

/// Runs the `fill` probe.
fn fill(context: &Context<'_>) -> io::Result<Facts> {
    let space = statvfs::statvfs(context.upper)?;
    let total = space.blocks() * space.fragment_size();
    let available = space.blocks_available() * space.fragment_size();
    let path = context.root.join(FILL_FILE);
    let mut file = File::create_new(&path)?;
    let chunk = vec![0xa5; FILL_CHUNK];
    let mut written = 0;
    let error = loop {
        if written > available + FILL_SLACK {
            break format!("the upper took {written} bytes, beyond the {available} available");
        }
        match file.write(&chunk) {
            Ok(0) => break "a write wrote nothing".to_owned(),
            Ok(count) => written += count as u64,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.raw_os_error() == Some(Errno::ENOSPC as i32) => {
                break "ENOSPC".to_owned();
            }
            Err(error) => break error.to_string(),
        }
    };
    drop(file);
    fs::remove_file(&path)?;
    fs::write(&path, PROBE_CONTENTS)?;
    let after = read(&path);
    fs::remove_file(&path)?;
    let memory = fs::read_to_string(MEMORY_INFORMATION)?;
    let memory = memory_total(&memory).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{MEMORY_INFORMATION} has no MemTotal in kB"),
        )
    })?;
    let mut facts = Facts::new();
    facts.push("total", total.to_string());
    facts.push("available", available.to_string());
    facts.push("written", written.to_string());
    facts.push("error", error);
    facts.push("after", after);
    facts.push("memory", memory.to_string());
    Ok(facts)
}

/// Runs the `devices` probe.
fn devices() -> io::Result<Facts> {
    let mut found = Vec::new();
    for (name, directory) in disks()? {
        let access = match read(&directory.join("ro")).as_str() {
            "0" => "rw".to_owned(),
            "1" => "ro".to_owned(),
            other => other.to_owned(),
        };
        let sectors = read(&directory.join("size"));
        found.push(format!("{name}:{sectors}:{access}"));
    }
    let mut facts = Facts::new();
    facts.push("disks", found.join(" "));
    Ok(facts)
}

/// Runs the `reads` probe.
fn reads() -> io::Result<Facts> {
    let mut found = Vec::new();
    for (name, directory) in disks()? {
        let stat = read(&directory.join("stat"));
        let sectors = sectors_read(&stat).map_or(stat, |sectors| sectors.to_string());
        found.push(format!("{name}:{sectors}"));
    }
    let mut facts = Facts::new();
    facts.push("disks", found.join(" "));
    Ok(facts)
}

/// Runs the `read-disks` probe.
fn read_disks() -> io::Result<Facts> {
    let mut found = Vec::new();
    let mut chunk = vec![0; READ_CHUNK];
    for (name, _) in disks()? {
        let path = Path::new(DEVICE_NODES).join(&name);
        let mut disk = File::open(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot open {}: {error}", path.display()),
            )
        })?;
        let mut bytes = 0u64;
        loop {
            match disk.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => bytes += count as u64,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    let problem = format!("cannot read {}: {error}", path.display());
                    return Err(io::Error::new(error.kind(), problem));
                }
            }
        }
        found.push(format!("{name}:{bytes}"));
    }
    let mut facts = Facts::new();
    facts.push("disks", found.join(" "));
    Ok(facts)
}

/// Runs the `memory` probe.
fn memory() -> io::Result<Facts> {
    let blocks = Path::new(MEMORY_BLOCKS);
    let text = fs::read_to_string(blocks.join("block_size_bytes"))?;
    let block_size = u64::from_str_radix(text.trim(), 16).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{MEMORY_BLOCKS}/block_size_bytes holds no block size: {text:?}"),
        )
    })?;
    let mut online = 0u64;
    for entry in fs::read_dir(blocks)? {
        let entry = entry?;
        let name = entry.file_name();
        let block = name
            .to_str()
            .and_then(|name| name.strip_prefix("memory"))
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()));
        if block && read(&entry.path().join("online")) == "1" {
            online += 1;
        }
    }
    let total = fs::read_to_string(MEMORY_INFORMATION)?;
    let total = memory_total(&total).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{MEMORY_INFORMATION} has no MemTotal in kB"),
        )
    })?;
    let mut facts = Facts::new();
    facts.push("block-size", block_size.to_string());
    facts.push("blocks", online.to_string());
    facts.push("online", online.saturating_mul(block_size).to_string());
    facts.push("total", total.to_string());
    Ok(facts)
}

/// Runs the `processors` probe.
fn processors() -> Facts {
    let cpus = Path::new(CPUS);
    let mut facts = Facts::new();
    for list in ["online", "present", "possible"] {
        facts.push(list, read(&cpus.join(list)));
    }
    let restored = match fs::read_to_string(TIME_RESTORE) {
        Ok(record) => restored_cpus(&record),
        Err(error) if error.kind() == io::ErrorKind::NotFound => "absent".to_owned(),
        Err(error) => format!("unreadable: {error}"),
    };
    facts.push("restored", restored);
    facts
}

/// Returns the CPUs that the time ABI's record of a restore, `record`, names as the CPUs that the
/// restore brought online, after `new_cpus=`, or `unreadable: PROBLEM` for a record without them.
fn restored_cpus(record: &str) -> String {
    record
        .lines()
        .find_map(|line| line.strip_prefix("new_cpus="))
        .map_or_else(
            || "unreadable: the record names no new CPUs".to_owned(),
            str::to_owned,
        )
}

/// Runs the `time` probe.
fn time() -> io::Result<Facts> {
    Ok(time_facts(&fs::read_to_string(TIME_STATE)?))
}

/// Returns the facts of the guest's time state `state`, as the `time` probe reports them.
fn time_facts(state: &str) -> Facts {
    let mut facts = Facts::new();
    for line in state.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.replace('_', "-");
        if valid_key(&key) && facts.get(&key).is_none() {
            facts.push(key, value);
        }
    }
    facts
}

/// Returns the guest's disks, the block devices that a device backs, rather than loop or RAM
/// devices, each with its directory of [`BLOCK_DEVICES`], in the order of their names.
fn disks() -> io::Result<Vec<(String, PathBuf)>> {
    let mut disks = Vec::new();
    for entry in fs::read_dir(BLOCK_DEVICES)? {
        let directory = entry?.path();
        if !directory.join("device").exists() {
            continue;
        }
        let name = directory.file_name().unwrap_or_default().to_string_lossy();
        disks.push((name.into_owned(), directory));
    }
    disks.sort();
    Ok(disks)
}

/// Returns the sectors read of a block device's statistics, `/sys/block/NAME/stat`, its third
/// field.
fn sectors_read(stat: &str) -> Option<u64> {
    stat.split_whitespace().nth(2)?.parse().ok()
}

/// Runs the `first-partition` probe.
fn first_partition(context: &Context<'_>) -> Facts {
    let (length, bytes) = match context.first_partition {
        Some(bytes) => (bytes.len().to_string(), hexadecimal(bytes)),
        None => ("none".to_owned(), "none".to_owned()),
    };
    let mut facts = Facts::new();
    facts.push("length", length);
    facts.push("bytes", bytes);
    facts
}

/// Runs the `cgroups` probe.
fn cgroups(context: &Context<'_>) -> io::Result<Facts> {
    let parent = context.agent.parent().unwrap_or(Path::new("/"));
    let table = mount_table(&fs::read_to_string(MOUNTINFO)?)?;
    let parent_type = table
        .iter()
        .rev()
        .find(|mount| mount.point == parent)
        .map_or("none", |mount| mount.kind.as_str());
    let mut facts = Facts::new();
    facts.push("parent.type", parent_type);
    facts.push(
        "parent.subtree-control",
        read(&parent.join("cgroup.subtree_control")),
    );
    for (name, cgroup) in [("agent", context.agent), ("workload", context.workload)] {
        let exists = if cgroup.is_dir() { "yes" } else { "no" };
        facts.push(format!("{name}.exists"), exists);
    }
    facts.push("agent.memory.low", read(&context.agent.join("memory.low")));
    for value in ["memory.max", "pids.max"] {
        facts.push(
            format!("workload.{value}"),
            read(&context.workload.join(value)),
        );
    }
    for (name, cgroup) in [("agent", context.agent), ("workload", context.workload)] {
        let processes = fs::read_to_string(cgroup.join("cgroup.procs"))?;
        facts.push(
            format!("{name}.processes"),
            processes.lines().count().to_string(),
        );
    }
    let user_namespaces = Path::new(USER_NAMESPACES);
    let limit = if user_namespaces.exists() {
        read(user_namespaces)
    } else {
        "unavailable".to_owned()
    };
    facts.push("user-namespaces", limit);
    Ok(facts)
}

/// Runs the `processes` probe.
fn processes(context: &Context<'_>) -> io::Result<Facts> {
    let init = namespaces(1)?;
    let mut facts = Facts::new();
    let processes = [
        ("pid1", Some(1)),
        ("worker", Some(std::process::id())),
        ("sibling", context.sibling),
    ];
    for (name, id) in processes {
        let Some(id) = id else {
            facts.push(format!("{name}.pid"), "none");
            continue;
        };
        facts.push(format!("{name}.pid"), id.to_string());
        let directory = PathBuf::from(format!("/proc/{id}"));
        let stat = match fs::read_to_string(directory.join("stat")) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                facts.push(format!("{name}.state"), "gone");
                continue;
            }
            stat => stat?,
        };
        let (state, parent) = parse_stat(&stat).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the status of process {id} is malformed"),
            )
        })?;
        facts.push(format!("{name}.state"), state);
        facts.push(format!("{name}.ppid"), parent);
        let root = fs::read_link(directory.join("root"))?;
        facts.push(format!("{name}.root"), root.display().to_string());
        let cgroup = fs::read_to_string(directory.join("cgroup"))?;
        facts.push(
            format!("{name}.cgroup"),
            unified_cgroup(&cgroup).unwrap_or("none"),
        );
        let command = fs::read(directory.join("cmdline"))?;
        facts.push(format!("{name}.command"), command_text(&command));
        let differ = namespaces(id)?
            .into_iter()
            .zip(&init)
            .filter(|((_, own), (_, first))| own != first)
            .map(|((namespace, _), _)| namespace)
            .collect::<Vec<_>>();
        let shared = if differ.is_empty() {
            "same".to_owned()
        } else {
            format!("differ:{}", differ.join(","))
        };
        facts.push(format!("{name}.namespaces"), shared);
    }
    Ok(facts)
}

/// Runs the `hold` probe, which keeps its descriptor in `held`.
fn hold(context: &Context<'_>, held: &mut Vec<File>) -> io::Result<Facts> {
    held.push(File::open(context.root)?);
    let mut facts = Facts::new();
    facts.push("held", context.root.display().to_string());
    Ok(facts)
}

/// Returns the namespaces of the process `id`, each as its name and its identity, in the order
/// of [`NAMESPACES`], with `None` for one that the kernel does not have.
fn namespaces(id: u32) -> io::Result<Vec<(&'static str, Option<PathBuf>)>> {
    NAMESPACES
        .into_iter()
        .map(
            |namespace| match fs::read_link(format!("/proc/{id}/ns/{namespace}")) {
                Ok(identity) => Ok((namespace, Some(identity))),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok((namespace, None)),
                Err(error) => Err(error),
            },
        )
        .collect()
}

/// Returns the trimmed contents of the file at `path`, `absent` if it does not exist, or
/// `unreadable: PROBLEM`.
pub(super) fn read(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).trim().to_owned(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => "absent".to_owned(),
        Err(error) => format!("unreadable: {error}"),
    }
}

/// A mount of a mount table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Mount {
    /// The mount point.
    pub(super) point: PathBuf,
    /// The mount's options.
    pub(super) options: String,
    /// The file system's type.
    pub(super) kind: String,
    /// The mount's source.
    source: String,
    /// The file system's own options.
    super_options: String,
}

/// Parses a mount table in the format of `/proc/PID/mountinfo`, in its order:
/// `ID PARENT MAJOR:MINOR ROOT POINT OPTIONS [OPTIONAL...] - TYPE SOURCE SUPER`, with `\NNN` octal
/// escapes in its paths and source.
pub(super) fn mount_table(table: &str) -> io::Result<Vec<Mount>> {
    table
        .lines()
        .map(|line| {
            let fields = line.split(' ').collect::<Vec<_>>();
            let separator = fields.iter().position(|field| *field == "-");
            let mount = match (separator, fields.as_slice()) {
                (Some(separator), [_, _, _, _, point, options, ..]) if separator >= 6 => {
                    match fields[separator + 1..] {
                        [kind, source, super_options] => Some(Mount {
                            point: PathBuf::from(OsString::from_vec(unescape(point))),
                            options: (*options).to_owned(),
                            kind: kind.to_owned(),
                            source: String::from_utf8_lossy(&unescape(source)).into_owned(),
                            super_options: super_options.to_owned(),
                        }),
                        _ => None,
                    }
                }
                _ => None,
            };
            mount.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the mount table has a malformed line: {line:?}"),
                )
            })
        })
        .collect()
}

/// Reverses the `\NNN` octal escapes of a field of a mount table.
fn unescape(field: &str) -> Vec<u8> {
    let bytes = field.as_bytes();
    let mut unescaped = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = bytes.get(index + 1..index + 4).and_then(|digits| {
            let octal = digits.iter().all(|digit| (b'0'..=b'7').contains(digit));
            let text = std::str::from_utf8(digits).ok()?;
            octal.then(|| u8::from_str_radix(text, 8).ok()).flatten()
        });
        match escape {
            Some(byte) if bytes[index] == b'\\' => {
                unescaped.push(byte);
                index += 4;
            }
            _ => {
                unescaped.push(bytes[index]);
                index += 1;
            }
        }
    }
    unescaped
}

/// Returns the state and the parent's process ID from a process's `/proc/PID/stat`, whose command
/// is parenthesized and may hold any character, `)` and spaces included.
fn parse_stat(stat: &str) -> Option<(&str, &str)> {
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    Some((fields.next()?, fields.next()?))
}

/// Returns a process's cgroup of the unified hierarchy from its `/proc/PID/cgroup`: the path of
/// the line `0::PATH`.
fn unified_cgroup(cgroup: &str) -> Option<&str> {
    cgroup.lines().find_map(|line| line.strip_prefix("0::"))
}

/// Returns a process's command line from its `/proc/PID/cmdline`, whose arguments end with NULs,
/// with the arguments separated by spaces.
fn command_text(command: &[u8]) -> String {
    command
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns the `MemTotal` of `/proc/meminfo`, in bytes.
fn memory_total(text: &str) -> Option<u64> {
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?;
    let kibibytes = line
        .trim()
        .strip_suffix(" kB")?
        .trim()
        .parse::<u64>()
        .ok()?;
    kibibytes.checked_mul(1024)
}

/// Returns the `ID` of an `os-release` file, without its quotes.
fn os_release_id(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| line.strip_prefix("ID="))
        .map(|value| value.trim().trim_matches(['"', '\'']))
}

/// Returns `bytes` in lowercase hexadecimal, two digits each.
fn hexadecimal(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0xf])
        .map(|digit| char::from(DIGITS[usize::from(digit)]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mount_table_is_read_with_its_escapes() {
        let table = "\
22 1 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:5 - proc proc rw
41 30 254:0 / /run/a\\040b/layers/distro ro,relatime - erofs /dev/vda ro,user_xattr
42 30 254:16 / /run/a\\040b/scratch rw,nosuid,nodev,relatime - ext4 /dev/vdb rw
43 30 0:40 / /run/a\\040b/rootfs rw,nosuid,nodev,relatime - overlay overlay \
rw,lowerdir=/run/a\\040b/layers/distro,metacopy=on,xino=on
";
        let table = mount_table(table).unwrap();
        assert_eq!(table.len(), 4);
        assert_eq!(
            table[1],
            Mount {
                point: PathBuf::from("/run/a b/layers/distro"),
                options: "ro,relatime".to_owned(),
                kind: "erofs".to_owned(),
                source: "/dev/vda".to_owned(),
                super_options: "ro,user_xattr".to_owned(),
            }
        );
        assert_eq!(table[0].point, Path::new("/proc"));
        assert_eq!(table[3].kind, "overlay");
        assert_eq!(mount_table("").unwrap(), []);
        for line in [
            "1 2 3 4 5 6 - a b",
            "1 2 3 4 - a b c",
            "1 2 3 4 5 6 7 a b c",
        ] {
            let error = mount_table(line).unwrap_err();
            assert!(error.to_string().contains("malformed line"), "{line:?}");
        }
    }

    #[test]
    fn escapes_are_three_octal_digits() {
        assert_eq!(unescape("a\\040b\\134c"), b"a b\\c");
        assert_eq!(unescape("\\012\\011"), b"\n\t");
        // What starts no escape stays as it is.
        assert_eq!(unescape("a\\08b\\04"), b"a\\08b\\04");
        assert_eq!(unescape("\\"), b"\\");
    }

    #[test]
    fn process_records_are_read_as_the_kernel_writes_them() {
        assert_eq!(parse_stat("17 (a) b (c)) S 1 17 17 0 -1"), Some(("S", "1")));
        assert_eq!(parse_stat("17 (idle) R 1"), Some(("R", "1")));
        assert_eq!(parse_stat("17 (idle) R"), None);
        assert_eq!(parse_stat("17 idle R 1"), None);
        assert_eq!(unified_cgroup("0::/agent\n"), Some("/agent"));
        assert_eq!(unified_cgroup("1:name=x:/a\n0::/\n"), Some("/"));
        assert_eq!(unified_cgroup("1:name=x:/a\n"), None);
        assert_eq!(
            command_text(b"/sbin/runtime-adapter-guest\0idle\0"),
            "/sbin/runtime-adapter-guest idle"
        );
        assert_eq!(command_text(b""), "");
    }

    #[test]
    fn the_memory_total_is_read_in_bytes() {
        let text = "MemTotal:         241540 kB\nMemFree:  1 kB\n";
        assert_eq!(memory_total(text), Some(241_540 * 1024));
        assert_eq!(memory_total("MemFree: 1 kB\n"), None);
        assert_eq!(memory_total("MemTotal: 1 MB\n"), None);
        assert_eq!(memory_total("MemTotal: x kB\n"), None);
    }

    #[test]
    fn the_os_release_id_loses_its_quotes() {
        let text = "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n";
        assert_eq!(os_release_id(text), Some("ubuntu"));
        assert_eq!(os_release_id("ID=\"alpine\"\n"), Some("alpine"));
        assert_eq!(os_release_id("ID='x'"), Some("x"));
        assert_eq!(os_release_id("NAME=x\n"), None);
    }

    #[test]
    fn unknown_probes_and_arguments_are_refused() {
        let lowers = [PathBuf::from("/run/runtime-adapter/layers/distro")];
        let context = Context {
            runtime: Path::new("/run/runtime-adapter"),
            root: Path::new("/run/runtime-adapter/rootfs"),
            lowers: &lowers,
            upper: Path::new("/run/runtime-adapter/scratch"),
            first_partition: None,
            agent: Path::new("/sys/fs/cgroup/agent"),
            workload: Path::new("/sys/fs/cgroup/workload"),
            sibling: None,
        };
        let mut held = Vec::new();
        let unknown = Probe::parse("mapping").unwrap();
        assert_eq!(
            run(&unknown, &context, &mut held),
            Err("the worker runs no probe named mapping".to_owned())
        );
        let with_argument = Probe::parse("hold /").unwrap();
        assert_eq!(
            run(&with_argument, &context, &mut held),
            Err("the hold probe takes no arguments".to_owned())
        );
        let with_argument = Probe::parse("first-partition 2").unwrap();
        assert_eq!(
            run(&with_argument, &context, &mut held),
            Err("the first-partition probe takes no arguments".to_owned())
        );
        for name in ["reads", "read-disks", "memory", "processors", "time"] {
            let with_argument = Probe::parse(&format!("{name} vda")).unwrap();
            assert_eq!(
                run(&with_argument, &context, &mut held),
                Err(format!("the {name} probe takes no arguments"))
            );
        }
        for (text, problem) in [
            ("copy-up", "the copy-up probe takes one argument"),
            ("rename a b", "the rename probe takes one argument"),
            (
                "copy-up /etc/os-release",
                "the copy-up probe takes a relative path in the root, not \"/etc/os-release\"",
            ),
            (
                "rename ../mnt",
                "the rename probe takes a relative path in the root, not \"../mnt\"",
            ),
            (
                "rename ./mnt",
                "the rename probe takes a relative path in the root, not \"./mnt\"",
            ),
            (
                "copy-up usr/../etc",
                "the copy-up probe takes a relative path in the root, not \"usr/../etc\"",
            ),
        ] {
            let probe = Probe::parse(text).unwrap();
            assert_eq!(run(&probe, &context, &mut held), Err(problem.to_owned()));
        }
        assert_eq!(
            path_argument("rename", "lost+found"),
            Ok(Path::new("lost+found"))
        );
        assert_eq!(
            path_argument("copy-up", "usr/lib/os-release"),
            Ok(Path::new("usr/lib/os-release"))
        );
        assert!(held.is_empty());
    }

    #[test]
    fn the_sectors_read_are_the_third_field_of_a_disk_s_statistics() {
        let stat = "     163        0     8530      120        0        0        0        0 \
                    0      168      120        0        0        0        0        0        0";
        assert_eq!(sectors_read(stat), Some(8530));
        assert_eq!(sectors_read("1 2"), None);
        assert_eq!(sectors_read("1 2 x"), None);
        assert_eq!(sectors_read("1 2 -3"), None);
        assert_eq!(sectors_read("absent"), None);
    }

    #[test]
    fn the_time_state_s_lines_are_facts_with_their_keys_in_the_facts_form() {
        let state = "version=1\nrestore_status=ok\nlast_downtime_source=monotonic\n\
                     Bad_Key=x\nempty=\nrestore_status=fail\nno value\nlast_step_ns=-2\n";
        let facts = time_facts(state);
        let expected = [
            ("version", "1"),
            ("restore-status", "ok"),
            ("last-downtime-source", "monotonic"),
            ("empty", ""),
            ("last-step-ns", "-2"),
        ];
        assert_eq!(facts.iter().collect::<Vec<_>>(), expected);
        assert!(time_facts("").is_empty());
    }

    #[test]
    fn the_first_partition_is_reported_in_hexadecimal() {
        let lowers = [
            PathBuf::from("/run/runtime-adapter/layers/p2"),
            PathBuf::from("/run/runtime-adapter/layers/p3"),
        ];
        let bytes = b"\xff\x00a \x80";
        let mut context = Context {
            runtime: Path::new("/run/runtime-adapter"),
            root: Path::new("/run/runtime-adapter/rootfs"),
            lowers: &lowers,
            upper: Path::new("/run/runtime-adapter/tmpfs"),
            first_partition: Some(bytes),
            agent: Path::new("/sys/fs/cgroup/agent"),
            workload: Path::new("/sys/fs/cgroup/workload"),
            sibling: None,
        };
        let probe = Probe::parse("first-partition").unwrap();
        let mut held = Vec::new();
        let mut expected = Facts::new();
        expected.push("length", "5");
        expected.push("bytes", "ff00612080");
        assert_eq!(run(&probe, &context, &mut held), Ok(expected));
        context.first_partition = Some(b"");
        let mut expected = Facts::new();
        expected.push("length", "0");
        expected.push("bytes", "");
        assert_eq!(run(&probe, &context, &mut held), Ok(expected));
        context.first_partition = None;
        let mut expected = Facts::new();
        expected.push("length", "none");
        expected.push("bytes", "none");
        assert_eq!(run(&probe, &context, &mut held), Ok(expected));
        assert_eq!(hexadecimal(&[0x0f, 0xa0, 0x5c]), "0fa05c");
    }

    #[test]
    fn the_cpus_that_a_restore_brought_online_are_those_of_its_record() {
        assert_eq!(restored_cpus("generation=3\nnew_cpus=1-3\n"), "1-3");
        assert_eq!(restored_cpus("generation=3\nnew_cpus=none\n"), "none");
        assert_eq!(
            restored_cpus("generation=3\n"),
            "unreadable: the record names no new CPUs"
        );
    }
}
