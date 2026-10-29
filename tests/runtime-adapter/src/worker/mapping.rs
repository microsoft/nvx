//! The probes of the mappings beneath the worker's assembled root, which come from the host
//! export, as an adapter maps the host paths of its sandboxes:
//!
//! - `map` inspects the mappings that the worker applied from the host's mapping table, through
//!   the root itself, with `nvx::guest::root::Root::map`, which checks their targets against the
//!   container root's reserved set, and the export that they come from;
//! - `map-checks` maps from the export through mappers of the adapter's own,
//!   `nvx::guest::mappings::Mapper`, with reserved sets of its choice, and tears down what it
//!   mapped. The links that it plants in the root lead to `/tmp/runtime-adapter-outside`, an empty
//!   directory outside it.
//!
//! | Probe | Facts |
//! | --- | --- |
//! | `map [NAME=PATH...]` | The export's mount at `hostfs/root` in the runtime directory: the number of mounts there, `export.count`, and of the topmost, its `export.type` and `export.options`; the mode of `hostfs`, in octal, `hostfs.mode`, and its owner, `hostfs.owner`, as `UID:GID`, or `absent` for both; and the mount points below the root, in the mount table's order, `mounts`. For each NAME, at PATH in the root, such as a mapping's target: the options of the topmost mount there, `NAME.options`, or `none`; what it shows, `NAME.kind`; a directory's entries, by name, in order, separated by commas, `NAME.entries`, or a file's contents, `NAME.read`; whether a write is `allowed` or `refused`, `NAME.write`: of a new file `written`, which holds `runtime-adapter`, in a directory, or of the file, opened for writing; and what the overlay's upper holds at PATH, `NAME.upper`, an empty directory or file where the mapper created the target. Finally whether the root is latched, `latched` |
//! | `map-checks DIR FILE` | From the export's directory DIR, which holds a file `marker` and a directory `inner`, and in which the guest may write, and from its file FILE, through a new mapper for each check: a link at each component of `/srv/checks/tN/a/b/new`, `link.a`, `link.b`, and `link.new`, and a link to `..`, `link.up`; magic links of a procfs that it mounts at `/srv/checks/proc`, then unmounts, `magic.self`, `magic.thread-self`, `magic.root`, `magic.cwd`, and `magic.fd`; a directory onto a file, `kind.directory`, and a file onto a directory, `kind.file`; through one mapper, a mapping onto a file that an earlier one shows, `nested.onto`, one that would create a target inside it, `nested.create`, and its teardown, `nested.teardown`; the runtime's reserved targets, `reserved.run`, `reserved.run-nvx`, `reserved.time`, and `reserved.time-below`, the container root's, `reserved.proc`, `reserved.sys`, `reserved.dev`, `reserved.agent`, `reserved.machine-id`, and `reserved.etc`, and those of a set that the adapter chooses, the tree `/srv/adapter` and the path `/srv/checks/exact`: `reserved.adapter-tree`, `reserved.adapter-ancestor`, `reserved.adapter-path`, and `reserved.adapter-below`, with whether the refused ones created anything, `reserved.created`. A read-only mapping of DIR attached and unmounted again and again while another thread tries to open its `marker` for writing: the attaches, `read-only.attaches`, their flags, `read-only.flags`, and whether the file was ever writable, `read-only.writable`. Finally the entries and the mounts of the directory outside the root, `outside.entries` and `outside.mounts`, and the mounts that the checks left below `/srv/checks`, `left` |
//!
//! A NAME holds lowercase ASCII letters, digits, and `-`, but is neither `export` nor `hostfs`, and
//! comes once. PATH is a relative path in the root, and DIR and FILE are relative paths in the
//! export, each of normal components. A kind is `directory`, `file SIZE`, with the file's size in
//! bytes, `link`, `other`, `denied` for a path that the guest may not look up, such as one that
//! the export hides, or `absent`. A check's outcome is `applied`, or why the mapper refused
//! the mapping: `link` for a link at the component that the check expects, and otherwise the kind
//! of the error, such as `reserved` or `target-kind`, with the path that it names, if any. The
//! checks write nothing to the export, which the host's own sources show.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::thread;

use nix::errno::Errno;
use nix::mount::{self, MsFlags};
use nvx::guest::mappings::{
    Access, InvalidMapping, Mapper, Mapping, MappingError, Reserved, SourceRoot, TargetRoot,
};
use nvx::guest::root::Root;

use super::probes::{self, Context};
use crate::probe::{Facts, Probe};

/// The names of the probes of this module, which the worker runs with its root and the host
/// export.
pub const PROBES: [&str; 2] = ["map", "map-checks"];

/// The mount point of the host export, below the runtime directory, as `nvx::guest::root` names
/// it.
const EXPORT: &str = "hostfs/root";
/// The directory of the export's mount point, below the runtime directory, which only root may
/// enter.
const HOSTFS: &str = "hostfs";
/// The names of the facts of the `map` probe that no mapping's name may take.
const GENERAL: [&str; 2] = ["export", "hostfs"];
/// The directory outside the root to which the links that `map-checks` plants lead.
const OUTSIDE: &str = "/tmp/runtime-adapter-outside";
/// The file that the `map` probe creates in a directory that a mapping shows.
const WRITTEN_FILE: &str = "written";
/// What the `map` probe writes through writable mappings.
const WRITTEN: &str = "runtime-adapter";
/// The number of times that `map-checks` attaches a read-only mapping, and unmounts it again,
/// while another thread tries to open its file for writing.
const READ_ONLY_ATTACHES: usize = 200;

/// Runs `probe`, one of [`PROBES`], with `context`, the worker's `root`, and the root of the host
/// export, `export`, once the root mounted one, and returns its facts.
///
/// Fails with the problem for another probe, one with arguments that the
/// [module documentation](self) does not allow, `map-checks` without an export, and one that
/// fails.
pub fn run(
    probe: &Probe,
    context: &Context<'_>,
    root: &Root,
    export: Option<&SourceRoot>,
) -> Result<Facts, String> {
    let name = probe.name();
    let facts = match (name, probe.arguments()) {
        ("map", arguments) => map(context, root, &mappings(arguments)?),
        ("map-checks", [directory, file]) => {
            let export = export.ok_or(
                "the map-checks probe maps from the host export, which the root has not mounted",
            )?;
            map_checks(context, export, export_path(directory)?, export_path(file)?)
        }
        ("map-checks", _) => return Err("the map-checks probe takes two arguments".to_owned()),
        _ => return Err(format!("the worker runs no probe named {name}")),
    };
    facts.map_err(|error| format!("the {name} probe failed: {error}"))
}

/// Reads the arguments of the `map` probe, each `NAME=PATH`: a name in the facts, and the path that
/// it inspects, a relative path in the root.
///
/// Fails with the problem for an argument that breaks the rules of the
/// [module documentation](self).
fn mappings(arguments: &[String]) -> Result<Vec<(&str, &Path)>, String> {
    let mut mappings: Vec<(&str, &Path)> = Vec::with_capacity(arguments.len());
    for argument in arguments {
        let refused = || {
            format!(
                "the map probe takes NAME=PATH, with a name of lowercase ASCII letters, digits, \
                 and `-`, other than export and hostfs, not {argument:?}"
            )
        };
        let (name, path) = argument.split_once('=').ok_or_else(refused)?;
        let valid = !name.is_empty()
            && !GENERAL.contains(&name)
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !valid {
            return Err(refused());
        }
        let path = probes::path_argument("map", path)?;
        if mappings.iter().any(|(other, _)| *other == name) {
            return Err(format!("the map probe names {name} twice"));
        }
        mappings.push((name, path));
    }
    Ok(mappings)
}

/// Returns `argument`, a path in the host export, once it is a relative path of normal
/// components, or the problem.
fn export_path(argument: &str) -> Result<&str, String> {
    if argument
        .split('/')
        .all(|component| !matches!(component, "" | "." | ".."))
    {
        Ok(argument)
    } else {
        Err(format!(
            "the map-checks probe takes relative paths in the host export, not {argument:?}"
        ))
    }
}

/// Runs the `map` probe, of `mappings`, each a name and the path that it inspects, a relative path
/// in the root.
fn map(context: &Context<'_>, root: &Root, mappings: &[(&str, &Path)]) -> io::Result<Facts> {
    let table = probes::mount_table(&fs::read_to_string(probes::MOUNTINFO)?)?;
    // Of the mounts at a point, the table lists the topmost last.
    let topmost = |point: &Path| table.iter().rev().find(|mount| mount.point == point);
    let mut facts = Facts::new();
    let export = context.runtime.join(EXPORT);
    let exports = table.iter().filter(|mount| mount.point == export).count();
    facts.push("export.count", exports.to_string());
    if let Some(mount) = topmost(&export) {
        facts.push("export.type", mount.kind.as_str());
        facts.push("export.options", mount.options.as_str());
    }
    let (mode, owner) = match fs::symlink_metadata(context.runtime.join(HOSTFS)) {
        Ok(metadata) => (
            format!("{:04o}", metadata.mode() & 0o7777),
            format!("{}:{}", metadata.uid(), metadata.gid()),
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ("absent".to_owned(), "absent".to_owned())
        }
        Err(error) => return Err(error),
    };
    facts.push("hostfs.mode", mode);
    facts.push("hostfs.owner", owner);
    let below = table
        .iter()
        .filter(|mount| mount.point.starts_with(context.root) && mount.point != context.root)
        .map(|mount| in_root(context.root, &mount.point))
        .collect::<Vec<_>>();
    facts.push("mounts", below.join(","));
    for (name, path) in mappings {
        let target = context.root.join(path);
        let options = topmost(&target).map_or("none", |mount| mount.options.as_str());
        facts.push(format!("{name}.options"), options);
        let kind = Kind::of(&target)?;
        facts.push(format!("{name}.kind"), kind.to_string());
        match kind {
            Kind::Directory => {
                facts.push(format!("{name}.entries"), entries(&target));
                facts.push(format!("{name}.write"), create(&target.join(WRITTEN_FILE)));
            }
            Kind::File(_) => {
                facts.push(format!("{name}.read"), probes::read(&target));
                facts.push(format!("{name}.write"), open_for_writing(&target));
            }
            Kind::Absent | Kind::Link | Kind::Denied | Kind::Other => {}
        }
        let upper = Kind::of(&context.upper_directory().join(path))?;
        facts.push(format!("{name}.upper"), upper.to_string());
    }
    facts.push("latched", yes(root.is_latched()));
    Ok(facts)
}

/// Runs the `map-checks` probe, from the root of the host export, `export`, with its directory
/// `directory`, and its file `file`.
fn map_checks(
    context: &Context<'_>,
    export: &SourceRoot,
    directory: &str,
    file: &str,
) -> io::Result<Facts> {
    let outside = outside()?;
    let base = context.root.join("srv/checks");
    fs::create_dir_all(&base)?;
    let checks = Checks {
        root: context.root,
        source: export,
        directory,
    };
    let mut facts = Facts::new();

    // A link at each component in turn, to the directory outside the root, and one to `..`.
    let components = ["a", "b", "new"];
    for (index, link) in components.iter().enumerate() {
        let mut parent = base.join(format!("t{index}"));
        parent.extend(&components[..index]);
        fs::create_dir_all(&parent)?;
        symlink(&outside, parent.join(link))?;
        let target = format!("/srv/checks/t{index}/{}", components.join("/"));
        let linked = format!("/srv/checks/t{index}/{}", components[..=index].join("/"));
        let found = checks.one(Reserved::default(), directory, &target)?;
        facts.push(format!("link.{link}"), at_link(found, &linked));
    }
    symlink("..", base.join("up"))?;
    let found = checks.one(Reserved::default(), directory, "/srv/checks/up/new")?;
    facts.push("link.up", at_link(found, "/srv/checks/up"));

    // Magic links of a procfs in the root, which comes off again whatever the checks found.
    let proc = base.join("proc");
    fs::create_dir(&proc)?;
    mount::mount(
        Some("proc"),
        &proc,
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )?;
    let held = File::open(&outside);
    let magic = held.and_then(|held| {
        let process = std::process::id();
        let descriptor = held.as_raw_fd();
        [
            ("self", "/srv/checks/proc/self".to_owned()),
            ("thread-self", "/srv/checks/proc/thread-self".to_owned()),
            ("root", format!("/srv/checks/proc/{process}/root")),
            ("cwd", format!("/srv/checks/proc/{process}/cwd")),
            ("fd", format!("/srv/checks/proc/{process}/fd/{descriptor}")),
        ]
        .into_iter()
        .map(|(name, link)| {
            let found = checks.one(Reserved::default(), directory, &format!("{link}/new"))?;
            Ok((name, at_link(found, &link)))
        })
        .collect::<io::Result<Vec<_>>>()
    });
    mount::umount(&proc)?;
    for (name, found) in magic? {
        facts.push(format!("magic.{name}"), found);
    }

    // Kinds.
    fs::write(base.join("plain"), "")?;
    fs::create_dir(base.join("directory"))?;
    let directory_on_file = checks.one(Reserved::default(), directory, "/srv/checks/plain")?;
    facts.push("kind.directory", directory_on_file);
    let file_on_directory = checks.one(Reserved::default(), file, "/srv/checks/directory")?;
    facts.push("kind.file", file_on_directory);

    // A mapping may go onto what an earlier one shows, but creates nothing inside it.
    let mut nested = checks.mapper(Reserved::default())?;
    let onto = [
        mapping(directory, "/srv/checks/m", Access::ReadWrite)?,
        mapping(file, "/srv/checks/m/marker", Access::ReadOnly)?,
    ];
    facts.push("nested.onto", applied(&mut nested, export, &onto));
    let inner = format!("{directory}/inner");
    let inside = [mapping(&inner, "/srv/checks/m/new", Access::ReadOnly)?];
    facts.push("nested.create", applied(&mut nested, export, &inside));
    facts.push("nested.teardown", torn_down(nested));

    // The runtime's reserved targets, the container root's, and a set of the adapter's.
    for (name, target) in [
        ("run", "/run"),
        ("run-nvx", "/run/nvx"),
        ("time", "/run/nvx/time"),
        ("time-below", "/run/nvx/time/x"),
    ] {
        let found = checks.one(Reserved::default(), directory, target)?;
        facts.push(format!("reserved.{name}"), found);
    }
    for (name, target) in [
        ("proc", "/proc/x"),
        ("sys", "/sys"),
        ("dev", "/dev"),
        ("agent", "/.nvx-agent"),
        ("machine-id", "/etc/machine-id"),
        ("etc", "/etc"),
    ] {
        let found = checks.one(Reserved::container_root(), directory, target)?;
        facts.push(format!("reserved.{name}"), found);
    }
    for (name, target) in [
        ("adapter-tree", "/srv/adapter/x"),
        ("adapter-ancestor", "/srv"),
        ("adapter-path", "/srv/checks/exact"),
        ("adapter-below", "/srv/checks/exact/below"),
    ] {
        let adapter = Reserved::new(["/srv/adapter"], ["/srv/checks/exact"]).map_err(invalid)?;
        let found = checks.one(adapter, directory, target)?;
        facts.push(format!("reserved.{name}"), found);
    }
    facts.push(
        "reserved.created",
        exists(&context.root.join("srv/adapter")),
    );

    let (attaches, flags, writable) = checks.read_only(&base.join("ro"))?;
    facts.push("read-only.attaches", attaches.to_string());
    facts.push("read-only.flags", flags);
    facts.push("read-only.writable", writable);

    facts.push(
        "outside.entries",
        fs::read_dir(&outside)?.count().to_string(),
    );
    facts.push("outside.mounts", mounts_below(&outside)?.len().to_string());
    facts.push("left", mounts_below(&base)?.len().to_string());
    Ok(facts)
}

/// The checks of `map-checks`, each with a mapper of its own beneath the root.
struct Checks<'a> {
    /// The root, the overlay's mount point.
    root: &'a Path,
    /// The root of the host export.
    source: &'a SourceRoot,
    /// The export's directory, which holds the file `marker`, and in which the guest may write.
    directory: &'a str,
}

impl Checks<'_> {
    /// Returns a new mapper beneath the root, with `reserved`.
    fn mapper(&self, reserved: Reserved) -> io::Result<Mapper> {
        Ok(Mapper::new(TargetRoot::open(self.root)?, reserved))
    }

    /// Applies the read-write mapping of `source` at `target` with a new mapper with `reserved`,
    /// tears down what it attached, and returns its outcome.
    fn one(&self, reserved: Reserved, source: &str, target: &str) -> io::Result<String> {
        let mut mapper = self.mapper(reserved)?;
        let found = applied(
            &mut mapper,
            self.source,
            &[mapping(source, target, Access::ReadWrite)?],
        );
        mapper.teardown().map_err(io::Error::other)?;
        Ok(found)
    }

    /// Attaches the export's directory read-only at `target` and unmounts it again,
    /// [`READ_ONLY_ATTACHES`] times, while another thread tries to open its `marker` for writing,
    /// and returns the number of attaches, their flags, `ro,nosuid,nodev` if each had them, and
    /// whether the file was ever `writable` or `never`. The guest may write the directory through
    /// the export, so only the mapping keeps the file read-only.
    fn read_only(&self, target: &Path) -> io::Result<(usize, String, &'static str)> {
        let relative = in_root(self.root, target);
        let marker = target.join("marker");
        let stop = Arc::new(AtomicBool::new(false));
        let wrote = Arc::new(AtomicBool::new(false));
        // The thread's attempts hold the lock, which a teardown takes, so that no attempt keeps
        // the mapping busy while it is unmounted.
        let attempts = Arc::new(RwLock::new(()));
        let racer = {
            let (stop, wrote, attempts) = (stop.clone(), wrote.clone(), attempts.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _attempt = attempts.read().unwrap_or_else(PoisonError::into_inner);
                    if OpenOptions::new().write(true).open(&marker).is_ok() {
                        wrote.store(true, Ordering::Relaxed);
                    }
                }
            })
        };
        let mut attached = 0;
        let mut missing = None;
        let attached_all = (|| -> io::Result<()> {
            for _ in 0..READ_ONLY_ATTACHES {
                let mut mapper = self.mapper(Reserved::default())?;
                let mapping = mapping(self.directory, &relative, Access::ReadOnly)?;
                mapper
                    .apply(self.source, &[mapping])
                    .map_err(io::Error::other)?;
                attached += 1;
                let options = options_at(target)?;
                let found = options.split(',').collect::<Vec<_>>();
                if !["ro", "nosuid", "nodev"]
                    .iter()
                    .all(|flag| found.contains(flag))
                {
                    missing.get_or_insert_with(|| options.clone());
                }
                let _teardown = attempts.write().unwrap_or_else(PoisonError::into_inner);
                mapper.teardown().map_err(io::Error::other)?;
            }
            Ok(())
        })();
        stop.store(true, Ordering::Relaxed);
        racer
            .join()
            .map_err(|_| io::Error::other("the writing thread panicked"))?;
        attached_all?;
        let flags = match missing {
            None => "ro,nosuid,nodev".to_owned(),
            Some(options) => format!("missing: {options}"),
        };
        let writable = if wrote.load(Ordering::Relaxed) {
            "writable"
        } else {
            "never"
        };
        Ok((attached, flags, writable))
    }
}

/// Creates [`OUTSIDE`], if it does not exist, and returns it.
fn outside() -> io::Result<PathBuf> {
    fs::create_dir_all(OUTSIDE)?;
    Ok(PathBuf::from(OUTSIDE))
}

/// Returns the mapping of `source` at `target` with `access`.
fn mapping(source: &str, target: &str, access: Access) -> io::Result<Mapping> {
    Mapping::new(source, target, access).map_err(invalid)
}

/// Returns `error`, a mapping that a probe names, as an I/O error.
fn invalid(error: InvalidMapping) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

/// What a path shows, without following a final link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Nothing.
    Absent,
    /// A directory.
    Directory,
    /// A regular file, of this many bytes.
    File(u64),
    /// A symbolic link.
    Link,
    /// Something that the guest may not look up, such as a path that the host export hides.
    Denied,
    /// Something else, such as a device.
    Other,
}

impl Kind {
    /// Returns what `path` shows: [`Absent`](Self::Absent) too if a component of its parent is
    /// not a directory.
    fn of(path: &Path) -> io::Result<Self> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => Ok(Self::Directory),
            Ok(metadata) if metadata.is_file() => Ok(Self::File(metadata.len())),
            Ok(metadata) if metadata.is_symlink() => Ok(Self::Link),
            Ok(_) => Ok(Self::Other),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(Self::Absent)
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(Self::Denied),
            Err(error) => Err(error),
        }
    }
}

impl fmt::Display for Kind {
    /// Writes `absent`, `directory`, `file SIZE`, `link`, `denied`, or `other`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => formatter.write_str("absent"),
            Self::Directory => formatter.write_str("directory"),
            Self::File(size) => write!(formatter, "file {size}"),
            Self::Link => formatter.write_str("link"),
            Self::Denied => formatter.write_str("denied"),
            Self::Other => formatter.write_str("other"),
        }
    }
}

/// Returns the names of the entries of the directory `path`, in order, separated by commas, or
/// `unreadable: PROBLEM`.
fn entries(path: &Path) -> String {
    let names = fs::read_dir(path).and_then(|entries| {
        entries
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<io::Result<Vec<_>>>()
    });
    match names {
        Ok(mut names) => {
            names.sort();
            names.join(",")
        }
        Err(error) => format!("unreadable: {error}"),
    }
}

/// Applies `mappings` from `source` with `mapper`, and returns `applied`, or why it refused one.
fn applied(mapper: &mut Mapper, source: &SourceRoot, mappings: &[Mapping]) -> String {
    match mapper.apply(source, mappings) {
        Ok(()) => "applied".to_owned(),
        Err(error) => refusal(error.error()),
    }
}

/// Returns `complete` once `mapper`'s teardown unmounted every mapping, or the failures.
fn torn_down(mapper: Mapper) -> String {
    match mapper.teardown() {
        Ok(()) => "complete".to_owned(),
        Err(error) => format!("failed: {error}"),
    }
}

/// Returns the kind of `error`, with the path that it names, if any.
fn refusal(error: &MappingError) -> String {
    let at = |kind: &str, path: &Path| format!("{kind} {}", path.display());
    match error {
        MappingError::Latched => "latched".to_owned(),
        MappingError::Reserved => "reserved".to_owned(),
        MappingError::Overlap { mapped } => at("overlap", mapped),
        MappingError::SourceKind => "source-kind".to_owned(),
        MappingError::Link { path } => at("link", path),
        MappingError::NotDirectory { path } => at("not-directory", path),
        MappingError::TargetKind => "target-kind".to_owned(),
        MappingError::Foreign { path } => at("foreign", path),
        MappingError::MountPoint => "mount-point".to_owned(),
        MappingError::Moved { .. } => format!("moved: {error}"),
        MappingError::System { .. } => format!("system: {error}"),
        error => format!("other: {error}"),
    }
}

/// Returns `link` if `found` is a refusal at the link `linked`, and otherwise `found`.
fn at_link(found: String, linked: &str) -> String {
    if found == format!("link {linked}") {
        "link".to_owned()
    } else {
        found
    }
}

/// Returns `path`, a path below `root`, as an absolute path in the root.
fn in_root(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    format!("/{}", relative.display())
}

/// Returns the mounts below `path`, not at it, in the mount table's order: each mount point, and
/// its options.
fn mounts_below(path: &Path) -> io::Result<Vec<(PathBuf, String)>> {
    let table = probes::mount_table(&fs::read_to_string(probes::MOUNTINFO)?)?;
    Ok(table
        .into_iter()
        .filter(|mount| mount.point.starts_with(path) && mount.point != path)
        .map(|mount| (mount.point, mount.options))
        .collect())
}

/// Returns the options of the topmost mount at `path`, or `none`.
fn options_at(path: &Path) -> io::Result<String> {
    let table = probes::mount_table(&fs::read_to_string(probes::MOUNTINFO)?)?;
    // Of the mounts at a point, the table lists the topmost last.
    Ok(table
        .iter()
        .rev()
        .find(|mount| mount.point == path)
        .map_or_else(|| "none".to_owned(), |mount| mount.options.clone()))
}

/// Creates the file `path` with [`WRITTEN`], and returns `allowed`, `refused` if its file system
/// is read-only, or the failure.
fn create(path: &Path) -> String {
    written(fs::write(path, WRITTEN))
}

/// Opens the existing file `path` for writing, and returns `allowed`, `refused` if its file
/// system is read-only, or the failure.
fn open_for_writing(path: &Path) -> String {
    written(OpenOptions::new().write(true).open(path).map(drop))
}

/// Returns the outcome of a write: `allowed`, `refused` for a read-only file system, or the
/// failure.
fn written(result: io::Result<()>) -> String {
    match result {
        Ok(()) => "allowed".to_owned(),
        Err(error) if error.raw_os_error() == Some(Errno::EROFS as i32) => "refused".to_owned(),
        Err(error) => format!("failed: {error}"),
    }
}

/// Returns `exists` if something is at `path`, without following a final link, and otherwise
/// `absent`.
fn exists(path: &Path) -> &'static str {
    if fs::symlink_metadata(path).is_ok() {
        "exists"
    } else {
        "absent"
    }
}

/// Returns `yes` or `no`.
fn yes(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_name_their_kind_and_path() {
        assert_eq!(refusal(&MappingError::Reserved), "reserved");
        assert_eq!(refusal(&MappingError::TargetKind), "target-kind");
        let link = MappingError::Link {
            path: PathBuf::from("/srv/link"),
        };
        assert_eq!(refusal(&link), "link /srv/link");
        assert_eq!(at_link(refusal(&link), "/srv/link"), "link");
        assert_eq!(at_link(refusal(&link), "/srv"), "link /srv/link");
        let foreign = MappingError::Foreign {
            path: PathBuf::from("/m/new"),
        };
        assert_eq!(refusal(&foreign), "foreign /m/new");
        assert_eq!(
            in_root(Path::new("/run/a/rootfs"), Path::new("/run/a/rootfs/srv/x")),
            "/srv/x"
        );
    }

    #[test]
    fn the_probes_take_named_targets_and_paths_in_the_export() {
        let arguments = |list: &[&str]| {
            list.iter()
                .map(|&argument| argument.to_owned())
                .collect::<Vec<_>>()
        };
        let named = arguments(&["dir=srv/maps/dir", "file-2=srv/maps/file.txt"]);
        let expected = [
            ("dir", Path::new("srv/maps/dir")),
            ("file-2", Path::new("srv/maps/file.txt")),
        ];
        assert_eq!(mappings(&named).unwrap(), expected);
        assert!(mappings(&[]).unwrap().is_empty());
        for refused in [
            "dir",
            "=srv/maps/dir",
            "Dir=srv/maps/dir",
            "dir.x=srv/maps/dir",
            "export=srv/maps/dir",
            "hostfs=srv/maps/dir",
            "dir=/srv/maps/dir",
            "dir=srv/../dir",
            "dir=",
        ] {
            assert!(mappings(&arguments(&[refused])).is_err(), "{refused}");
        }
        let twice = mappings(&arguments(&["dir=a", "dir=b"])).unwrap_err();
        assert_eq!(twice, "the map probe names dir twice");

        assert_eq!(export_path("2"), Ok("2"));
        assert_eq!(export_path("0/inner"), Ok("0/inner"));
        for refused in ["", "/2", "2/", "2//inner", "./2", "2/..", ".."] {
            assert!(export_path(refused).is_err(), "{refused}");
        }

        assert_eq!(Kind::File(4).to_string(), "file 4");
        assert_eq!(Kind::Directory.to_string(), "directory");
        assert_eq!(Kind::Denied.to_string(), "denied");
        assert_eq!(Kind::Absent.to_string(), "absent");
    }
}
