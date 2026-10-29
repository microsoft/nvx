//! The lifecycle of a sandbox's store beside its caller's files: the locks of a provision and of an
//! existing sandbox, the release of a lock in a directory that holds no provisioned sandbox, and
//! the removal of the store and its directory. The [module documentation](super#lifecycle-locks)
//! describes their rules.

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::caller::{is_report_staging, is_store_temporary};
use super::{
    CLONE_BINDING_NAME, CLONE_FILE_NAMES, CallerFiles, FILE_NAMES, JOURNAL_NAME, LOCK_NAME,
    LockedStore, RUNTIME_NAME, Races, SandboxStore, Taken, absolute, pause, transition_in_progress,
};
use crate::error::{Error, ErrorCode, ErrorKind, Result};
use crate::host::paths::not_unicode;
use crate::host::platform;

#[cfg(test)]
mod tests;

impl SandboxStore {
    /// Takes the lock of the sandbox in `dir` for its provision, by a caller that keeps `files`
    /// in the directory, waiting for it until `deadline`, or for as long as it takes without one.
    ///
    /// The directory, whose parent must exist, is created if needed, private to the current user,
    /// as [`create`](Self::create) creates it, and created again if a concurrent removal deletes
    /// it before the lock is taken, also while it is created and checked. On Windows, a file
    /// system without POSIX deletion keeps a removed directory under its name until its last
    /// handle closes, and refuses to open it, or to create a file in it, until then, so it is
    /// created again once it goes. The caller checks under the lock whether its configuration
    /// record exists already, and writes it.
    ///
    /// Fails with [`ErrorKind::TimedOut`] when the deadline passes first, also while Windows
    /// refuses to open a removed lock file, as [`lock_existing`](Self::lock_existing) describes,
    /// or a removed directory, or while concurrent transitions keep deleting the directory or
    /// replacing the lock file, with [`ErrorKind::NotFound`] if the parent does not exist, and as
    /// [`create`](Self::create) fails for a directory that is not private. A directory or lock
    /// file that Windows refuses to open for good, unlike a removed one, fails the lock with the
    /// denied access.
    pub fn lock_for_provision(
        dir: impl Into<PathBuf>,
        files: &CallerFiles,
        deadline: Option<Instant>,
    ) -> Result<LockedStore> {
        let dir = absolute(dir.into())?;
        Self::provision_lock(
            &dir,
            files,
            deadline,
            |dir| Self::create(dir),
            Self::take_lifecycle_lock,
        )
    }

    /// Takes the lock as [`lock_for_provision`](Self::lock_for_provision) does, with `create`
    /// creating or checking the absolute directory `dir`, as [`create`](Self::create) does, and
    /// `take` taking the lock of its store, as [`take_lifecycle_lock`](Self::take_lifecycle_lock)
    /// does.
    fn provision_lock(
        dir: &Path,
        files: &CallerFiles,
        deadline: Option<Instant>,
        mut create: impl FnMut(&Path) -> Result<Self>,
        mut take: impl FnMut(&Self, Option<Instant>) -> Result<Taken>,
    ) -> Result<LockedStore> {
        let mut races = Races::default();
        // Whether the latest attempt was refused access while the directory was not pending
        // deletion.
        let mut refused = false;
        loop {
            let error = match create(dir) {
                Ok(store) => match take(&store, deadline) {
                    Ok(Taken::Locked(lock)) => {
                        return Ok(LockedStore::new(store, lock, files.clone()));
                    }
                    // A removal deleted the lock file while this waited for its lock, and maybe the
                    // directory, or a transition replaced it as it released its lock: the
                    // directory is created and locked again.
                    Ok(Taken::Replaced) => {
                        refused = false;
                        if !races.lost(deadline) {
                            return Err(transition_in_progress(dir));
                        }
                        continue;
                    }
                    Err(error) => error,
                },
                Err(error) => error,
            };
            if cfg!(windows) && denied(&error) {
                // A removal deleted the directory, which a file system without POSIX deletion
                // keeps under its name until its last handle closes, and refuses to open, or to
                // create a file in, until then: it is created again once it goes.
                if pending_deletion(dir) {
                    refused = false;
                    if !pause(deadline) {
                        return Err(transition_in_progress(dir));
                    }
                    continue;
                }
                // The directory, or its lock file, refuses access for good, unless the removed
                // directory went after the attempt failed, and is created again now.
                if refused {
                    return Err(error);
                }
                refused = true;
                continue;
            }
            // A removal deleted the directory after it was created, before it was checked or its
            // lock file was opened: unless its parent is gone, it is created again.
            if error.kind() != ErrorKind::NotFound || !dir.parent().is_some_and(Path::is_dir) {
                return Err(error);
            }
            refused = false;
            if !races.lost(deadline) {
                return Err(transition_in_progress(dir));
            }
        }
    }

    /// Takes the lock of the existing sandbox in `dir`, by a caller that keeps `files` in the
    /// directory, waiting for it until `deadline`, or for as long as it takes without one.
    ///
    /// Fails with [`ErrorKind::NotFound`] if the directory does not exist, also once a concurrent
    /// removal deletes it while this waits: a lock file that is removed or replaced meanwhile
    /// locks nothing, so the lock of the lock file at the path is taken again, as
    /// [`lock`](Self::lock) does, which fails once the directory is gone. A directory that exists
    /// is locked even if it holds no provisioned sandbox, as one that was never provisioned, or
    /// that an interrupted removal emptied, or whose removal open files still keep: the caller
    /// checks its configuration record under the lock, and the release then removes the lock file
    /// again.
    ///
    /// On Windows, a file system without POSIX deletion keeps a removed lock file under its name
    /// until its last handle closes, and refuses to open it until then, so the open is tried
    /// again every 25 ms. Fails with [`ErrorKind::TimedOut`] when the deadline passes first, as
    /// [`lock`](Self::lock) does. A lock file that Windows refuses to open for good, such as a
    /// read-only one, still opens without access rights, unlike a removed one, and fails the lock
    /// with the denied access.
    pub fn lock_existing(
        dir: impl Into<PathBuf>,
        files: &CallerFiles,
        deadline: Option<Instant>,
    ) -> Result<LockedStore> {
        Self::existing_lock(Self::open(dir)?, files, deadline, Self::take_lifecycle_lock)
    }

    /// Takes the lock as [`lock_existing`](Self::lock_existing) does, of the opened `store`, with
    /// `take` taking it, as [`take_lifecycle_lock`](Self::take_lifecycle_lock) does.
    fn existing_lock(
        store: Self,
        files: &CallerFiles,
        deadline: Option<Instant>,
        mut take: impl FnMut(&Self, Option<Instant>) -> Result<Taken>,
    ) -> Result<LockedStore> {
        let mut races = Races::default();
        loop {
            if let Taken::Locked(lock) = take(&store, deadline)? {
                return Ok(LockedStore::new(store, lock, files.clone()));
            }
            if !races.lost(deadline) {
                return Err(transition_in_progress(store.dir()));
            }
        }
    }

    /// Takes the lock as [`take_lock`](Self::take_lock) does, and opens a lock file that Windows
    /// refuses to open again every 25 ms until `deadline`, as a removed one that other handles
    /// still hold, which a file system without POSIX deletion keeps until they close. A lock file
    /// that Windows refuses to open although it opens without access rights is not a removed one,
    /// and fails the lock once a second open at once is refused too.
    fn take_lifecycle_lock(&self, deadline: Option<Instant>) -> Result<Taken> {
        // Whether the latest attempt was refused a lock file that opened without access rights.
        let mut refused_for_good = false;
        loop {
            match self.take_lock(deadline) {
                Err(error) if cfg!(windows) && denied(&error) => {
                    if !pending_deletion(&self.path(LOCK_NAME)) {
                        // The lock file refuses access for good, such as a read-only one, unless
                        // the removed one went, and another replaced it, after the open failed.
                        if refused_for_good {
                            return Err(error);
                        }
                        refused_for_good = true;
                        continue;
                    }
                    refused_for_good = false;
                    if !pause(deadline) {
                        return Err(transition_in_progress(self.dir()));
                    }
                }
                taken => return taken,
            }
        }
    }
}

/// Returns whether the store's `error` is an I/O error whose cause is a denied access.
fn denied(error: &Error) -> bool {
    error.kind() == ErrorKind::Io
        && std::error::Error::source(error)
            .and_then(|source| source.downcast_ref::<io::Error>())
            .is_some_and(|source| source.kind() == io::ErrorKind::PermissionDenied)
}

/// Returns whether the file or directory at `path`, which Windows refused to open, or to create a
/// file in, is a removed one that other handles still hold: Windows refuses every open of such a
/// file or directory, also one without access rights, which opens one whose access it refuses
/// otherwise.
fn pending_deletion(path: &Path) -> bool {
    platform::file_identity(path)
        .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
}

impl Drop for LockedStore {
    /// Releases the lock. A holder that leaves the directory without its caller's configuration
    /// record removes its own lock file first, unless it removed it already, so that a directory
    /// that holds no provisioned sandbox keeps no lock file, and empties.
    ///
    /// The lock is released before its file closes: on Linux, a process that another thread is
    /// spawning holds a copy of every descriptor from its fork until its exec, and `flock` keeps
    /// a lock while any copy of its open file description remains, so closing the file alone could
    /// keep the lock held until that process executes its program.
    fn drop(&mut self) {
        self.remove_unprovisioned_lock_file();
        let _ = self.lock.unlock();
    }
}

impl LockedStore {
    /// Removes the lock file, as [dropping](Drop) the lock does first, if the caller declared a
    /// configuration record that the directory does not hold, unless this holder removed it
    /// already, and if the path still names the locked file.
    fn remove_unprovisioned_lock_file(&self) {
        let Some(config) = self.files.config() else {
            return;
        };
        if self.lock_removed
            || !matches!(
                fs::symlink_metadata(self.path(config)),
                Err(error) if error.kind() == io::ErrorKind::NotFound
            )
        {
            return;
        }
        let path = self.path(LOCK_NAME);
        let held = platform::opened_file_id(&self.lock);
        if matches!((held, platform::file_id(&path)), (Ok(held), Ok(current)) if held == current) {
            let _ = fs::remove_file(&path);
        }
    }
}

impl LockedStore {
    /// Removes the sandbox's store, its caller's files, and its directory, which only a sandbox
    /// that no VMM can run in allows, and releases the lock.
    ///
    /// The removal needs the caller's configuration record: a lock that was taken without one, as
    /// with [`CallerFiles::none`] or by [`SandboxStore::lock`], refuses with
    /// [`RemovalError::Undeclared`], and removes nothing, since a transition that waits through
    /// the removal could not tell that the sandbox is gone then, and would keep the lock file that
    /// it creates.
    ///
    /// The removal refuses, and removes nothing, while the store records a VM or a launch: the
    /// caller reattaches to the VM and stops it, or recovers the launch, first, as
    /// [`supervisor::reattach`](crate::host::supervisor::reattach) does. It also refuses, with
    /// [`RemovalError::Io`], and removes nothing, once the directory that its path names holds
    /// another lock file than this lock's, as after a link in the path changed. It makes both
    /// checks, and removes what a clone kept of its launch, as a release does, and the sandbox's
    /// [clone binding](CLONE_BINDING_NAME), if it holds one, through one handle of the directory
    /// that it opens, so that no change of a link in the path meanwhile redirects their removal
    /// to another sandbox, whose VM may run. It makes their absence durable before anything else
    /// goes, so that no crash after the removal of the directory, or of one of its ancestors,
    /// brings back a binding whose lease a retirement may have ended. It then removes the store's
    /// other files but its lock, the caller's files, its configuration record last, so that an
    /// interrupted removal leaves a provisioned sandbox, which can be removed again, and the
    /// temporary files that the store owns. It refuses the entries that are neither the store's
    /// nor the caller's, which it names in the order in which the directory lists them, and keeps
    /// them and the directory; the lock file goes as the lock is released, since the
    /// configuration record is gone. Otherwise it removes its lock file while it still holds the
    /// lock, so that a transition that waited for the lock finds the sandbox gone rather than
    /// halfway removed, and then the directory, as long as the path still names it.
    ///
    /// Lock files of other transitions, a clone binding or a file that the clone kept of its launch
    /// whose deletion is pending, and on NFS other removed files that handles still hold, can keep
    /// the emptied directory, and are not refused: NFS renames a removed file that is still open to
    /// `.nfs*`; a Windows file system without POSIX deletion keeps a removed lock file, or a
    /// removed binding or clone's file that a reader holds, under its own name until its last
    /// handle closes; and a transition that opens the lock file between the removals of the lock
    /// file and of the directory creates a new one. Each of those transitions finds no sandbox, and
    /// removes its lock file as it releases its lock, and the handles of other removed files close,
    /// so the directory empties. The removal then holds the directory's identity, releases the
    /// lock, and returns the [`Lingering`] removal, whose [`wait`](Lingering::wait) removes the
    /// directory once it empties. It returns `None` once the directory is gone, whoever removed it.
    pub fn remove(self) -> std::result::Result<Option<Lingering>, RemovalError> {
        self.remove_contents()?;
        self.remove_directory()
    }

    /// Empties the directory but for lingering files, its own lock file included, as
    /// [`remove`](Self::remove) does first: it refuses without the caller's configuration record,
    /// removes what a clone kept of its launch and the clone binding as [`unbind`](Self::unbind)
    /// does, then the store's other files and the caller's, and the temporary files that the store
    /// owns, and refuses any other entry.
    fn remove_contents(&self) -> std::result::Result<(), RemovalError> {
        self.remove_contents_with(&|| {}, &platform::Directory::sync)
    }

    /// Empties the directory as [`remove_contents`](Self::remove_contents) does, with `checked`
    /// and `sync` as [`unbind`](Self::unbind) takes them.
    fn remove_contents_with(
        &self,
        checked: &dyn Fn(),
        sync: &dyn Fn(&platform::Directory) -> io::Result<()>,
    ) -> std::result::Result<(), RemovalError> {
        if self.files.config().is_none() {
            return Err(RemovalError::Undeclared);
        }
        self.unbind(checked, sync)?;
        // What a clone kept of its launch, and the binding, go once, through the directory's
        // handle: one whose deletion is pending refuses a second removal, and its path may name
        // another sandbox meanwhile.
        let owned: Vec<PathBuf> = FILE_NAMES
            .into_iter()
            .filter(|&name| {
                name != LOCK_NAME && name != CLONE_BINDING_NAME && !CLONE_FILE_NAMES.contains(&name)
            })
            .chain(self.files.names())
            .map(|name| self.path(name))
            .collect();
        for path in owned {
            remove_entry(path)?;
        }
        let listed = entries(self.dir()).map_err(|error| RemovalError::Io {
            path: self.dir().to_owned(),
            error,
        })?;
        let mut foreign = Vec::new();
        for name in listed {
            // Lock files, a binding or a clone's file whose deletion is pending, and removed files
            // that NFS keeps while handles hold them, go once their holders release them, which
            // the removal waits for, if they keep the directory.
            if lingers(self.dir(), &name) {
                continue;
            }
            if is_store_temporary(&name) || is_report_staging(&name) {
                remove_entry(self.path(&name))?;
            } else {
                foreign.push(name);
            }
        }
        if foreign.is_empty() {
            Ok(())
        } else {
            Err(RemovalError::Foreign(foreign))
        }
    }

    /// Removes what the clone of the sandbox kept of its launch, as a release does, and then the
    /// sandbox's clone binding, if it holds one, and makes their absence durable with `sync`,
    /// before anything else of the sandbox goes, through one handle of the directory that the path
    /// names. Through that handle, it checks first that the directory holds this lock's file, and
    /// refuses otherwise, then that the store records no VM and no launch, and runs `checked` once
    /// both checks pass, before it removes anything. No change of a link in the path meanwhile
    /// redirects these steps to another sandbox, whose VM may run, and whose files then stay.
    ///
    /// The binding's absence, after this removal or after a release that a crash interrupted
    /// before its sync, is then durable while the directory, which keeps the sandbox provisioned,
    /// remains. A binding, or a file of the clone, whose deletion is pending already counts as
    /// removed.
    fn unbind(
        &self,
        checked: &dyn Fn(),
        sync: &dyn Fn(&platform::Directory) -> io::Result<()>,
    ) -> std::result::Result<(), RemovalError> {
        let dir = self.dir();
        let failed = |error: io::Error| RemovalError::Io {
            path: dir.to_owned(),
            error,
        };
        let entry_failed = |name: &str| {
            let path = self.path(name);
            move |error: io::Error| RemovalError::Io { path, error }
        };
        let directory = platform::Directory::open(dir).map_err(failed)?;
        let held = platform::opened_file_id(&self.lock).map_err(entry_failed(LOCK_NAME))?;
        if directory
            .entry_id(LOCK_NAME)
            .map_err(entry_failed(LOCK_NAME))?
            != Some(held)
        {
            return Err(failed(io::Error::other(
                "the directory that the path names does not hold the sandbox's lock",
            )));
        }
        for name in [JOURNAL_NAME, RUNTIME_NAME] {
            if directory
                .entry_id(name)
                .map_err(entry_failed(name))?
                .is_some()
            {
                return Err(RemovalError::Launched);
            }
        }
        checked();
        // What a clone kept of its launch goes before its binding, as a release removes it, so
        // that the binding's durable absence makes theirs durable too.
        for name in CLONE_FILE_NAMES {
            directory.remove_file(name).map_err(entry_failed(name))?;
        }
        directory
            .remove_file(CLONE_BINDING_NAME)
            .map_err(entry_failed(CLONE_BINDING_NAME))?;
        sync(&directory).map_err(failed)
    }

    /// Removes the lock file while it still holds the lock, and then the directory, as
    /// [`remove`](Self::remove) does once the directory holds nothing else of the sandbox.
    fn remove_directory(self) -> std::result::Result<Option<Lingering>, RemovalError> {
        self.remove_directory_after(|| {})
    }

    /// Removes the lock file and the directory as [`remove_directory`](Self::remove_directory)
    /// does, and runs `unlinked` between the two, where the transitions that the removal races
    /// can act.
    fn remove_directory_after(
        mut self,
        unlinked: impl FnOnce(),
    ) -> std::result::Result<Option<Lingering>, RemovalError> {
        let dir = self.dir().to_owned();
        // While the lock is held, its file keeps the directory, which the path then names.
        let (identity, held) = hold_directory(&dir).map_err(|error| RemovalError::Io {
            path: dir.clone(),
            error,
        })?;
        let lock = self.path(LOCK_NAME);
        fs::remove_file(&lock).map_err(|error| RemovalError::Io { path: lock, error })?;
        self.lock_removed = true;
        unlinked();
        match remove_held_directory(&dir, identity) {
            // The directory is gone: this removal removed it, or another one did once it emptied,
            // after which the path may name another directory, or none.
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if directory_not_empty(&error) => {}
            Err(error) => return Err(RemovalError::Io { path: dir, error }),
        }
        let config = self.files.config().map(str::to_owned);
        drop(self);
        Ok(Some(Lingering {
            dir,
            identity,
            _held: held,
            config,
            retried_at_once: false,
        }))
    }
}

/// The removal of a sandbox's directory that open files, such as the lock files of other
/// transitions, keep, which [`LockedStore::remove`] returns once it has removed everything else
/// and released the lock.
///
/// It holds the identity of the directory that it removes: on Unix, its open handle keeps a
/// directory that replaces it from reusing its inode number, and NTFS and ReFS do not reuse file
/// IDs. The path may name another directory once this one is gone, such as one that a provision
/// created again, which is not this removal's to remove.
#[derive(Debug)]
pub struct Lingering {
    /// The directory.
    dir: PathBuf,
    /// The identity of the directory, which `identity_of` gives.
    identity: Identity,
    /// The open directory on Unix, which keeps its identity.
    _held: Option<File>,
    /// The caller's configuration record, which a provision of the path writes.
    config: Option<String>,
    /// Whether the latest attempt was the one that followed at once a failure without remaining
    /// entries.
    retried_at_once: bool,
}

impl Lingering {
    /// Returns the directory that the removal waits for.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Removes the directory once open files no longer keep it, trying again every 25 ms until
    /// `deadline`, or for as long as it takes without one. A deadline that has passed still tries
    /// once, so a caller that waits on its own, as for an interruption, calls this again, and a
    /// removal that timed out can go on.
    ///
    /// Returns once the directory is gone, whoever removed it, or once the path names another
    /// directory, which it checks before each attempt to remove it. Fails, and keeps the
    /// directory, with [`RemovalError::ProvisionedAgain`] once the caller's configuration record
    /// appears in it, as a provision that took the lock after the removal released it writes it,
    /// which it looks up by its path, so that a file system that ignores case finds it whatever
    /// the case of the name that the provision gave it, with [`RemovalError::Foreign`] for other
    /// entries than lock files, a clone binding or a clone's file whose deletion is pending, and
    /// the removed files that NFS keeps, sorted, and with [`RemovalError::TimedOut`] once the
    /// deadline passes.
    pub fn wait(&mut self, deadline: Option<Instant>) -> std::result::Result<(), RemovalError> {
        loop {
            let error = match remove_held_directory(&self.dir, self.identity) {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => error,
            };
            if directory_status(&self.dir) != Some(self.identity) {
                return Ok(());
            }
            let remaining = match entries(&self.dir) {
                Ok(remaining) => remaining,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => {
                    return Err(RemovalError::Io {
                        path: self.dir.clone(),
                        error,
                    });
                }
            };
            let mut unexpected: Vec<String> = remaining
                .iter()
                .filter(|name| !lingers(&self.dir, name))
                .cloned()
                .collect();
            unexpected.sort();
            if !unexpected.is_empty() {
                // A provision that took the lock after the removal released it, before the
                // directory went, provisions the directory again. Its record is looked up by its
                // path, which a file system that ignores case resolves to the name that the
                // provision gave it, and one that does not keeps apart from other names.
                if let Some(config) = &self.config {
                    let path = self.dir.join(config);
                    match fs::symlink_metadata(&path) {
                        Ok(_) => return Err(RemovalError::ProvisionedAgain),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(RemovalError::Io { path, error }),
                    }
                }
                return Err(RemovalError::Foreign(unexpected));
            }
            if !directory_not_empty(&error) {
                return Err(RemovalError::Io {
                    path: self.dir.clone(),
                    error,
                });
            }
            // Without remaining entries, the last open file went away after the removal failed,
            // so the next attempt follows at once. Another such failure right after it pauses
            // first, so that a file system that does not report emptiness right away cannot spin.
            if remaining.is_empty() && !self.retried_at_once {
                self.retried_at_once = true;
                continue;
            }
            self.retried_at_once = false;
            if !pause(deadline) {
                return Err(RemovalError::TimedOut(error));
            }
        }
    }
}

/// Why [`LockedStore::remove`] or [`Lingering::wait`] kept a sandbox's directory.
#[derive(Debug)]
#[non_exhaustive]
pub enum RemovalError {
    /// The lock was taken without the caller's configuration record, which the removal needs, and
    /// nothing was removed: see [`LockedStore::remove`].
    Undeclared,
    /// The store records a VM or a launch, so a VMM of the sandbox may still run, and nothing was
    /// removed.
    Launched,
    /// The directory holds entries that are neither the store's nor its caller's, which the
    /// removal keeps, with the directory: their names, in the order in which the directory lists
    /// them, or sorted once the removal waited for open files.
    Foreign(Vec<String>),
    /// The sandbox was provisioned again while the removal waited for open files: the caller's
    /// configuration record appeared in the directory, which the removal keeps.
    ProvisionedAgain,
    /// Open files still kept the directory when the deadline passed: the error with which the
    /// latest attempt to remove it failed.
    TimedOut(io::Error),
    /// An operation on one of the sandbox's files, or on its directory, failed.
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The error of the operation.
        error: io::Error,
    },
}

impl fmt::Display for RemovalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undeclared => formatter.write_str(
                "the removal of a sandbox needs its caller's configuration record, which its lock \
                 was not taken with",
            ),
            Self::Launched => {
                formatter.write_str("the sandbox's store records a VM or a launch, which may run")
            }
            Self::Foreign(names) => write!(
                formatter,
                "the sandbox's directory holds entries that are neither the store's nor its \
                 caller's: {}",
                names.join(", ")
            ),
            Self::ProvisionedAgain => formatter
                .write_str("the sandbox was provisioned again while its directory was removed"),
            Self::TimedOut(_) => {
                formatter.write_str("files that are still open keep the sandbox's directory")
            }
            Self::Io { path, .. } => write!(formatter, "cannot remove {}", path.display()),
        }
    }
}

impl std::error::Error for RemovalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TimedOut(error) | Self::Io { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Keeps the message of a removal's error, and its I/O cause: a removal without the caller's
/// configuration record is of [`ErrorKind::Unsupported`], a timeout of [`ErrorKind::TimedOut`], an
/// I/O failure of [`ErrorKind::Io`], and every other refusal of [`ErrorKind::Other`], with the
/// code [`ErrorCode::SandboxRunning`] for a sandbox that may run, [`ErrorCode::ForeignFiles`] for
/// entries that are not NVX's, and [`ErrorCode::SandboxProvisioned`] for a sandbox that was
/// provisioned again.
impl From<RemovalError> for Error {
    fn from(error: RemovalError) -> Self {
        let message = error.to_string();
        match error {
            RemovalError::Undeclared => Error::unsupported(message),
            RemovalError::Launched => Error::new(ErrorCode::SandboxRunning, message),
            RemovalError::Foreign(_) => Error::new(ErrorCode::ForeignFiles, message),
            RemovalError::ProvisionedAgain => Error::new(ErrorCode::SandboxProvisioned, message),
            RemovalError::TimedOut(source) => Error::timed_out(message).with_source(source),
            RemovalError::Io { error: source, .. } => Error::io(message).with_source(source),
        }
    }
}

/// Removes the file at `path` if it exists.
fn remove_entry(path: PathBuf) -> std::result::Result<(), RemovalError> {
    match fs::remove_file(&path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(RemovalError::Io { path, error })
        }
        _ => Ok(()),
    }
}

/// Returns whether `name`, an entry of the directory `dir`, is a lock file, or a removed file that
/// a handle still keeps, which goes once its holder releases or closes it: NFS renames a removed
/// file that is still open to `.nfs*`, and Windows file systems without POSIX deletion keep a
/// removed lock file under its own name, as they keep the lock files that waiting transitions
/// create until they release them, and a removed clone binding, or a removed file that the clone
/// kept of its launch, until its readers close it, whose deletion is then pending.
fn lingers(dir: &Path, name: &str) -> bool {
    name == LOCK_NAME
        || name.starts_with(".nfs")
        || ((name == CLONE_BINDING_NAME || CLONE_FILE_NAMES.contains(&name))
            && platform::is_deleted(&dir.join(name)))
}

/// Returns whether `error` is how the removal of a directory reports one that is not empty.
fn directory_not_empty(error: &io::Error) -> bool {
    #[cfg(windows)]
    let not_empty = matches!(error.raw_os_error(), Some(145 | 80 | 183));
    #[cfg(unix)]
    let not_empty = matches!(
        error.raw_os_error(),
        Some(code) if code == nix::errno::Errno::ENOTEMPTY as i32
            || code == nix::errno::Errno::EEXIST as i32
    );
    #[cfg(not(any(windows, unix)))]
    let not_empty = error.kind() == io::ErrorKind::DirectoryNotEmpty;
    not_empty
}

/// Returns the names of the entries of `directory`, in the order in which it lists them.
fn entries(directory: &Path) -> io::Result<Vec<String>> {
    fs::read_dir(directory)?
        .map(|entry| {
            entry?
                .file_name()
                .into_string()
                .map_err(|_| not_unicode("a file name"))
        })
        .collect()
}

/// The identity of a file or directory: its device and inode on Unix, and its volume serial number
/// and file ID on Windows.
type Identity = (u64, u128);

/// Returns the identity of the directory `path`, which [`identity_of`] gives, and a handle that
/// keeps it: on Unix, the open directory keeps a directory that replaces it from reusing its inode
/// number, and NTFS and ReFS do not reuse file IDs.
fn hold_directory(path: &Path) -> io::Result<(Identity, Option<File>)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags((nix::fcntl::OFlag::O_DIRECTORY | nix::fcntl::OFlag::O_NOFOLLOW).bits())
            .open(path)?;
        // The open directory's status gives the identity that `identity_of` gives for its path.
        let metadata = file.metadata()?;
        Ok(((metadata.dev(), u128::from(metadata.ino())), Some(file)))
    }
    #[cfg(not(unix))]
    {
        Ok((identity_of(path)?, None))
    }
}

/// Returns the identity of the file or directory that `path` names, without following a link
/// that it names: its device and inode on Unix, and on Windows the volume serial number and the
/// 128-bit file ID of `FILE_ID_INFO`, which tell the files of ReFS volumes apart too, as the
/// [`FileSeal`](crate::host::seal::FileSeal) of the object that `path` opens holds them.
///
/// Windows refuses to open a directory that a removal deleted while another handle keeps it, as
/// it refuses some system files. Its entry still exists, as [`fs::symlink_metadata`] finds it in
/// its parent, but has no identity that can be read, so it gets the identity `(0, 0)`, which no
/// directory that a store holds has.
fn identity_of(path: &Path) -> io::Result<Identity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::symlink_metadata(path)?;
        Ok((metadata.dev(), u128::from(metadata.ino())))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        };

        let opened = fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path);
        match opened {
            Ok(file) => {
                let seal = crate::host::seal::FileSeal::of(&file)?;
                Ok((seal.device(), seal.file_id()))
            }
            Err(error)
                if [ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION]
                    .map(|code| Some(code as i32))
                    .contains(&error.raw_os_error())
                    && fs::symlink_metadata(path).is_ok() =>
            {
                Ok((0, 0))
            }
            Err(error) => Err(error),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        platform::file_identity(path).map(|(device, index)| (device, u128::from(index)))
    }
}

/// Returns the identity that [`identity_of`] gives for `path`, or `None` if it cannot be
/// examined.
fn directory_status(path: &Path) -> Option<Identity> {
    identity_of(path).ok()
}

/// Removes the directory `dir` if the path still names the one that the caller holds, whose
/// identity, which [`identity_of`] gives, is `identity`, so that a directory that replaced it
/// stays. Returns whether the path named it, and fails as the check of the path or the removal
/// fails.
///
/// The check and the removal are two steps: a directory that replaces the held one between them,
/// which only a concurrent removal of the held one and a creation of another at the path both
/// allow, still goes while it is empty, and its creator then finds it gone, as a provision does,
/// which creates it again.
fn remove_held_directory(dir: &Path, identity: Identity) -> io::Result<bool> {
    if identity_of(dir)? != identity {
        return Ok(false);
    }
    fs::remove_dir(dir)?;
    Ok(true)
}
