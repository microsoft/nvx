//! Immutable named snapshots and their external, exclusive locks, and the `snapshot` command.
//!
//! Names map to hexadecimal directory names under `snapshots/`. Each published directory
//! contains only the strict snapshot's three entries; its lock stays under `locks/`.
//! Completed private directories under `staging/` publish with one non-replacing rename.
//!
//! Each start of a restored sandbox links the snapshot's files into the sandbox's view of it
//! under the snapshot's lock, which [`LockedSnapshot::remove_view`] also takes to remove the
//! view.
//!
//! `snapshot capture` publishes a named snapshot, as [`capture`] describes. `snapshot restore`
//! provisions a managed sandbox whose configuration records the snapshot, its store and a
//! workload, then starts it, and each later start of the sandbox restores the snapshot again.

pub(super) mod capture;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::args::{Parser, Receiver, Spec, Syntax, choice};
use super::deadline::Deadline;
use super::sandbox::{self, Source};
use super::session::{ExecRequest, Session};
use super::store::{LockedState, StateDir};
use super::{Exit, Result, hex, io_error, temporary_path};
use crate::host::config::NicConfig;
use crate::host::sandbox::options::SandboxOperation;
use crate::host::snapshot::{OPENVMM_DIR, SNAPSHOT_ARTIFACTS, Snapshot};
use crate::host::vm::VmDir;
use crate::host::{filesystem, paths, platform};
use crate::{Error, ErrorCode};

/// The parser of the snapshot command.
const SYNTAX: Syntax = Syntax {
    command: "snapshot",
    arguments: "[-h] {capture,restore} ...",
    help: "\
positional arguments:
  {capture,restore}
    capture   capture a named snapshot
    restore   restore a managed sandbox from a named snapshot

options:
  -h, --help  show this help message and exit",
};

/// The parser of the restore command.
const RESTORE_SYNTAX: Syntax = Syntax {
    command: "snapshot restore",
    arguments: "[-h] --store-dir PATH --state-dir PATH [OPTIONS] NAME",
    help: "\
Provision a managed sandbox with the options of sandbox provision, then start
it from a named snapshot, under the snapshot's lock, and run its workload, the
entrypoint and its arguments, which must exit with status 0 within the
timeout. Each later start of the sandbox restores the snapshot again.

positional arguments:
  NAME                  the snapshot's name

options:
  -h, --help            show this help message and exit
  --store-dir PATH      the named snapshot store
  --state-dir STATE_DIR
  --layer ROLE,PATH,IMAGE_UUID
  --layer-format {erofs,gpt}
  --scratch SCRATCH
  --upper {scratch,ram}
  --ram-upper-percent PERCENT
  --entrypoint ENTRYPOINT
  --arg SANDBOX_ARG
  --hostname HOSTNAME
  --workload-user UID:GID
  --memory-mib MEMORY_MIB
  --timeout TIMEOUT
  --hypervisor {auto,whp,kvm,mshv}
  --mount GUEST_TARGET,HOST_PATH[,ro|rw]
  --mount-deny HOST_PATH
  --mount-allow HOST_PATH
  --mount-write HOST_PATH
  --mount-owner {vmm,caller}
  --net IPV4/PREFIX
  --network-profile {portable}
  --network-egress {allow,deny}
  --network-ingress {allow,deny}
  --network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]
  --network-egress-deny CIDR[:PROTOCOL[:PORT[-PORT]]]
  --network-egress-policy-file PATH
  --host-loopback {allow,deny}
  --network-proxy IPV4:TCP-PORT
  --host-loopback-forward HOST_LOOPBACK_FORWARD",
};

/// The options of `sandbox` that only an execution or a cold boot takes, which a restore lacks.
const NOT_RESTORED: [&str; 9] = [
    "--cwd",
    "--environment",
    "--environment-file",
    "--inherit-default-environment",
    "--exec-timeout-ms",
    "--wait",
    "--outcome-report",
    "--cmdline",
    "--dry-run",
];

/// A parsed `snapshot` command.
pub(super) enum Command {
    /// `snapshot capture ...`.
    Capture(capture::Options),
    /// `snapshot restore ...`.
    Restore(Box<RestoreOptions>),
}

/// The options of `snapshot restore`: the snapshot's name and store, and those of the sandbox
/// that it provisions.
pub(super) struct RestoreOptions {
    /// `NAME`, which the parser requires.
    name: Option<Name>,
    /// `--store-dir`, which the parser requires.
    store_dir: String,
    /// The options of the sandbox, whose operation is `provision`.
    sandbox: sandbox::Options,
}

impl Receiver for RestoreOptions {
    fn take(&mut self, name: &'static str, value: Option<&str>) -> std::result::Result<(), String> {
        let text = value.unwrap_or_default().to_owned();
        match name {
            "NAME" => self.name = Some(Name::try_from(text).map_err(|error| error.to_string())?),
            "--store-dir" => self.store_dir = text,
            _ => self.sandbox.take(name, value)?,
        }
        Ok(())
    }
}

/// Parses a `snapshot` command, and returns it with the arguments that its parser does not
/// recognize.
pub(super) fn parse(arguments: &[String]) -> Result<(Command, Vec<String>)> {
    let (index, mut extras) = Parser::new(&[], &[Spec::one("snapshot_operation")])
        .split_command(arguments, &mut |_: &'static str, _: Option<&str>| Ok(()))
        .map_err(|stop| Exit::from_stop(stop, &SYNTAX))?;
    let operation = choice(&arguments[index], &["capture", "restore"])
        .map_err(|message| Exit::usage(&SYNTAX, ErrorCode::ArgumentInvalid, message))?;
    let arguments = &arguments[index + 1..];
    if operation == "capture" {
        let (options, unrecognized) = capture::parse(arguments)?;
        extras.extend(unrecognized);
        return Ok((Command::Capture(options), extras));
    }
    let mut options = RestoreOptions {
        name: None,
        store_dir: String::new(),
        sandbox: sandbox::Options {
            operation: SandboxOperation::Provision,
            ..sandbox::Options::default()
        },
    };
    extras.extend(
        Parser::new(&restore_options(), &[Spec::one("NAME")])
            .parse(arguments, &mut options)
            .map_err(|stop| Exit::from_stop(stop, &RESTORE_SYNTAX))?,
    );
    for (missing, option) in [
        (options.store_dir.is_empty(), "--store-dir"),
        (options.sandbox.state_dir.is_none(), "--state-dir"),
    ] {
        if missing {
            return Err(Exit::usage(
                &RESTORE_SYNTAX,
                ErrorCode::ArgumentMissing,
                format!("{option} is required"),
            ));
        }
    }
    Ok((Command::Restore(Box::new(options)), extras))
}

/// Returns the options of `snapshot restore`: those of `sandbox`, but those that only an
/// execution or a cold boot takes, and `--store-dir`.
pub(super) fn restore_options() -> Vec<Spec> {
    sandbox::OPTIONS
        .into_iter()
        .filter(|spec| !NOT_RESTORED.contains(&spec.name))
        .chain([Spec::one("--store-dir")])
        .collect()
}

/// Runs a `snapshot` command.
pub(super) fn run(root: Option<&str>, command: Command) -> Result {
    match command {
        Command::Capture(options) => capture::run(root, options),
        Command::Restore(options) => restore(root, *options),
    }
}

/// Restores a new managed sandbox from a named snapshot, as `snapshot restore` does: provisions
/// the sandbox with the source that [`restore_source`] checks, and starts it.
fn restore(root: Option<&str>, options: RestoreOptions) -> Result {
    let RestoreOptions {
        name,
        store_dir,
        sandbox: options,
    } = options;
    let snapshot = name.expect("the parser requires a snapshot name");
    let (source, _) = restore_source(snapshot, &store_dir, &options)?;
    sandbox::restore(root, options, source)
}

/// Returns the source of a sandbox that restores the snapshot `snapshot` of the store at
/// `store_dir` with `options`, as `snapshot restore` and `run --restore-snapshot` provision one,
/// and the snapshot's NIC. Its workload is the sandbox's entrypoint and arguments.
///
/// Before the caller provisions anything, it refuses a state directory in the store, as each start
/// and deprovision of the sandbox refuses one before it removes anything, and a workload that is
/// not a valid exec request: each restored start sends the workload as one, not as boot tokens, so
/// it may contain whitespace, which `sandbox provision` refuses. It then opens the snapshot, so
/// that a missing one provisions nothing.
pub(super) fn restore_source(
    snapshot: Name,
    store_dir: &str,
    options: &sandbox::Options,
) -> Result<(Source, Option<NicConfig>)> {
    let store = paths::resolve(Path::new(store_dir)).map_err(Exit::from_os_error)?;
    outside_store(
        &store,
        Path::new(options.state_dir.as_deref().unwrap_or_default()),
    )?;
    let workload: Vec<String> = std::iter::once(options.entrypoint.clone())
        .chain(options.args.iter().cloned())
        .collect();
    Session::exec_payload(&ExecRequest {
        arguments: workload.clone(),
        timeout_ms: 0,
        cwd: None,
        environment: None,
        inherit_default_environment: false,
    })?;
    let nic = Store::open(&store)?
        .lock(&snapshot, Deadline::after(options.timeout))?
        .open()?
        .nic();
    let source = Source {
        snapshot,
        store,
        workload,
    };
    Ok((source, nic))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct Name(String);

impl TryFrom<String> for Name {
    type Error = Error;

    fn try_from(value: String) -> crate::Result<Self> {
        if !(1..=64).contains(&value.len())
            || !value.as_bytes()[0].is_ascii_alphanumeric()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(Error::unsupported(
                "a snapshot name must be 1 to 64 ASCII letters, digits, dots, underscores or \
                 hyphens, starting with a letter or digit",
            ));
        }
        Ok(Self(value))
    }
}

#[derive(Debug)]
pub(crate) struct Store {
    root: StateDir,
    entries: StateDir,
    locks: StateDir,
    staging: StateDir,
}

impl Store {
    pub(crate) fn create(root: &Path) -> Result<Self> {
        let root = StateDir::create(&std::path::absolute(root).map_err(Exit::from_os_error)?)?;
        Ok(Self {
            entries: StateDir::create(&root.dir().join("snapshots"))?,
            locks: StateDir::create(&root.dir().join("locks"))?,
            staging: StateDir::create(&root.dir().join("staging"))?,
            root,
        })
    }

    /// Opens an existing store, private to the current user, as [`create`](Self::create) makes
    /// it, without creating anything.
    pub(crate) fn open(root: &Path) -> Result<Self> {
        let root = StateDir::open(&std::path::absolute(root).map_err(Exit::from_os_error)?)?;
        Ok(Self {
            entries: StateDir::open(&root.dir().join("snapshots"))?,
            locks: StateDir::open(&root.dir().join("locks"))?,
            staging: StateDir::open(&root.dir().join("staging"))?,
            root,
        })
    }

    fn check(&self) -> Result {
        for directory in [&self.root, &self.entries, &self.locks, &self.staging] {
            StateDir::open(directory.dir())?;
        }
        Ok(())
    }

    pub(crate) fn lock(&self, name: &Name, deadline: Deadline) -> Result<LockedSnapshot<'_>> {
        self.check()?;
        // Hex preserves case distinctions and avoids reserved host filenames.
        let directory = hex(name.0.as_bytes());
        let lock = StateDir::create(&self.locks.dir().join(&directory))?
            .lock(deadline)
            .map_err(|error| match error.code() {
                Some(ErrorCode::SandboxBusy) => Exit::error(
                    ErrorCode::SandboxBusy,
                    format!(
                        "another command still holds the lock of the snapshot {}",
                        name.0
                    ),
                ),
                _ => error,
            })?;
        Ok(LockedSnapshot {
            store: self,
            dir: self.entries.dir().join(directory),
            _lock: lock,
        })
    }
}

#[derive(Debug)]
pub(crate) struct LockedSnapshot<'a> {
    store: &'a Store,
    dir: PathBuf,
    _lock: LockedState,
}

impl LockedSnapshot<'_> {
    pub(crate) fn stage(&self) -> Result<StateDir> {
        self.store.check()?;
        StateDir::create(&temporary_path(&self.store.staging.dir().join("snapshot"))?)
    }

    pub(crate) fn open(&self) -> Result<Snapshot> {
        self.store.check()?;
        Snapshot::open(&self.dir).map_err(Into::into)
    }

    /// Removes the view of this snapshot that a restore created in `vm`, and on Linux the
    /// restore-ready socket of that restore, which this lock must cover, since removing the view
    /// changes the links of the snapshot's files. Refuses a view that is a link or holds anything
    /// else, which it leaves, and anything else in place of the socket, and, before it removes
    /// anything, a VM directory in the store, whose view could be a snapshot's own files.
    pub(crate) fn remove_view(&self, vm: &VmDir) -> Result {
        outside_store(self.store.root.dir(), vm.path())?;
        let foreign = |path: &Path| {
            Exit::error(
                ErrorCode::ForeignFiles,
                format!("{} is not a restore's own", path.display()),
            )
        };
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::FileTypeExt;

            let ready = PathBuf::from(vm.restore_ready_endpoint()?);
            match fs::symlink_metadata(&ready) {
                Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&ready)
                    .map_err(|error| io_error(ErrorCode::Io, &error, &ready))?,
                Ok(_) => return Err(foreign(&ready)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(ErrorCode::Io, &error, &ready)),
            }
        }
        let view = vm.path().join(OPENVMM_DIR);
        match fs::symlink_metadata(&view) {
            Ok(metadata) if metadata.is_dir() && !filesystem::is_reparse_point(&metadata) => {}
            Ok(_) => return Err(foreign(&view)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error(ErrorCode::Io, &error, &view)),
        }
        for name in SNAPSHOT_ARTIFACTS {
            let path = view.join(name);
            match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(io_error(ErrorCode::Io, &error, &path));
                }
                _ => {}
            }
        }
        match fs::remove_dir(&view) {
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Err(foreign(&view)),
            removed => removed.map_err(|error| io_error(ErrorCode::Io, &error, &view)),
        }
    }

    pub(crate) fn publish(&self, staged: StateDir) -> Result<Snapshot> {
        self.store.check()?;
        if staged.dir().parent() != Some(self.store.staging.dir()) {
            return Err(Exit::error(
                ErrorCode::Io,
                "only a private staging directory can publish a named snapshot",
            ));
        }
        Snapshot::open(staged.dir())?;
        platform::sync_dir(staged.dir())?;
        platform::rename_no_replace(staged.dir(), &self.dir)
            .map_err(|error| io_error(ErrorCode::Io, &error, &self.dir))?;
        platform::sync_dir(self.store.entries.dir())?;
        platform::sync_dir(self.store.staging.dir())?;
        self.open()
    }
}

/// Refuses `dir` in the snapshot store at `root`, once the links of both are resolved: the view
/// of a restore in that directory could take the files of the store's snapshots for its own.
fn outside_store(root: &Path, dir: &Path) -> Result {
    let resolve = |path: &Path| paths::resolve(path).map_err(Exit::from_os_error);
    if resolve(dir)?.starts_with(resolve(root)?) {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            "the sandbox's state directory must be outside the snapshot store",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
