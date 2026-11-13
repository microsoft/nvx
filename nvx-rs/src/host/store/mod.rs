//! The store of a sandbox's runtime records, which every host that supervises the sandbox
//! shares: one private directory per sandbox, with one lock, the runtime record of its running
//! VM, and the journal of its launch, so that a VMM whose launch crashed midway is never lost.
//!
//! ```text
//! <dir>/                   one sandbox, private to the current user
//!   sandbox.lock           the lock that serializes its lifecycle transitions
//!   launch.json            the journal of a launch in progress, or of one that was interrupted
//!   runtime.json           the runtime record of its running VM
//!   control.capability     the launch capability of its VM
//!   control.sock           the control endpoint of its VM, on Linux, but for a clone, whose VMM
//!                          listens beside the snapshot that it restores
//!   openvmm.log            the VMM's output, claimed by the launch that created it
//!   console.log            the guest console of the latest launch, or of every launch, if the
//!                          caller captures it
//!   outcome.json           the VMM's outcome report of the latest launch
//!   .tmpXXXXXX             the outcome report while the VMM writes it, which it then publishes
//!   clone.json             the binding of a clone to the generation of a template that it leases
//!   clone-scratch.raw      the file of the scratch device that earlier versions created for a
//!                          clone whose upper is in RAM, which a release removes if one remains
//!   clone-activation.bin   the activation record of a clone, which its launch writes
//!   clone-progress.log     the progress log of a clone's launch and restore sequence
//!   clone-failure.json     the record of a clone's failure
//!   restore.sock           the endpoint at which the launch of a clone awaits the restore-ready
//!                          event of its VMM, on Linux
//!   ...                    the caller's configuration record and other files, which it declares
//! ```
//!
//! The directory is private to the current user, since its records decide which process
//! recovery signals: it belongs to the user, and no one else may access it. On Unix it has mode
//! 0700, which OpenVMM also requires of the directory of its control socket, and its files mode
//! 0600. On Windows, a directory that the store creates allows the user alone, through a
//! protected access control list that its files inherit; one that it did not create may also
//! allow SYSTEM and the Administrators group, as one under the user's profile does, and must pass
//! entries on to the files created in it. A file's own list decides who may open it on Windows,
//! whatever its directory allows, so the store also refuses a lock, a record, or a capability
//! whose list allows anyone else. [`SandboxStore::create`] and [`SandboxStore::open`] refuse an
//! existing directory that is not private, rather than change it, since others could have placed
//! records in it, or still hold its files open. The store names its files by their paths, so the
//! directories above it must be ones that only the user, or an administrator, may change. A lock,
//! a record, or a capability that is not a regular file, such as a FIFO, is refused at once,
//! rather than waited on while the sandbox is locked. So is a lock file that is a link, or on
//! Windows any other reparse point, since a lock taken through it would hold the file that it
//! names, which a launch may replace while the lock is held.
//! Records are replaced atomically, so readers that hold no lock, such as an execution, see whole
//! records. The store holds runtime records only: configuration belongs to its caller, which may
//! keep it in the same directory under a name of its own, and declares it, with its other files
//! there, as [`CallerFiles`].
//!
//! # Records
//!
//! The [`RuntimeRecord`] and the [`LaunchJournal`] are JSON in format [`FORMAT`]. A record in any
//! other format fails with a message that names the format, and is neither read nor migrated:
//! switching from earlier formats takes one stop of every sandbox. Both records hold the
//! [`RuntimeIdentity`] of their launch, which decides whether a host may reattach to the VM; see
//! [`RuntimeRecord::check_identity`].
//!
//! # Launches
//!
//! A launch holds the sandbox's lock throughout, under an identity that the caller reserved
//! before it began, with [`LaunchId::generate`], a fresh one for each launch, or that a clone's
//! lease reserved, which records it durably before any VMM of the clone exists, for the one
//! launch of the clone that the lease allows:
//!
//! 1. [`LockedStore::begin_launch`] stores the capability, creates a fresh log and claims it, and
//!    writes the journal's marker, which names the launch, before anything is spawned.
//! 2. The caller spawns the VMM with [`Launch::take_log`] as its standard output and error, and
//!    [`Launch::record_process`] records the VMM's [`ProcessRef`] right after the spawn.
//! 3. Once the VM is ready, [`Launch::commit`] replaces the journal with the runtime record. A
//!    launch that fails ends with [`Launch::abandon`].
//!
//! [`supervisor::launch`](super::supervisor::launch) runs these steps. A caller that runs them
//! itself does so as follows:
//!
//! ```no_run
//! use std::time::{Duration, Instant};
//!
//! use nvx::host::launch::LaunchSpec;
//! use nvx::host::store::{LaunchId, RuntimeIdentity, SandboxStore};
//! use nvx::host::supervisor::{self, VmmCommand};
//! use nvx::protocol::CAPABILITY_LEN;
//!
//! fn launch(
//!     spec: &LaunchSpec,
//!     runtime: &RuntimeIdentity,
//!     capability: &[u8; CAPABILITY_LEN],
//! ) -> Result<(), Box<dyn std::error::Error>> {
//!     let store = SandboxStore::create("/var/lib/example/sandbox")?;
//!     let mut locked = store.lock(Some(Instant::now() + Duration::from_secs(10)))?;
//!     locked.recover()?;
//!     let mut launch = locked.begin_launch(&LaunchId::generate()?, runtime, capability)?;
//!     let arguments = spec.openvmm_arguments(launch.endpoint(), &store.outcome_path())?;
//!     let command = VmmCommand::new(&spec.openvmm, arguments, store.dir());
//!     let log = launch.take_log()?;
//!     let child = supervisor::spawn(&command, capability, log)?;
//!     launch.record_process(&child)?;
//!     // Wait until the VM is ready, then commit, or else abandon the launch.
//!     launch.commit()?;
//!     child.detach();
//!     Ok(())
//! }
//! ```
//!
//! The launch borrows the locked store exclusively until it commits or is abandoned, so no other
//! transition of the sandbox overlaps it, not even through the same guard:
//!
//! ```compile_fail
//! # use nvx::host::store::{LaunchId, RuntimeIdentity, SandboxStore};
//! # use nvx::protocol::CAPABILITY_LEN;
//! # fn overlap(
//! #     store: &SandboxStore,
//! #     runtime: &RuntimeIdentity,
//! #     capability: &[u8; CAPABILITY_LEN],
//! # ) -> nvx::Result<()> {
//! let mut locked = store.lock(None)?;
//! let launch = locked.begin_launch(&LaunchId::generate()?, runtime, capability)?;
//! locked.recover()?; // `locked` is still borrowed by `launch`.
//! launch.commit()?;
//! # Ok(())
//! # }
//! ```
//!
//! # The log claim
//!
//! The VMM inherits its launch's claim on the log, so the claim lasts as long as any process that
//! holds the log open from the launch. On Linux it is an exclusive `flock` lock of the log's open
//! file description, which the VMM shares through its standard output and error, and which only
//! the last close of that description releases. On Windows it is the processes of the job object
//! of the log, in which the VMM runs, while that job is open, as described below, and otherwise
//! the VMM's handles themselves: while any process holds the log open, an open of the log that
//! shares nothing fails. A released claim proves that the launch has exited, even one that never
//! recorded its VMM or published its endpoint. The claim relies on the VMM keeping its standard
//! output and error until it exits, as OpenVMM does, and on the directory being on a local file
//! system.
//!
//! On Linux, a process that another thread spawns while the log is open, such as another
//! sandbox's VMM, also holds the claim, from its fork until it executes its program, which closes
//! its copy. Recovery therefore checks a held claim again for up to a second before it concludes
//! that a process of the launch may still run.
//!
//! On Windows, a process that another thread creates while the VMM's spawn has the log's handles
//! inheritable, with inheritance but without a list of handles, as `std::process::Command`
//! creates one, inherits them, and holds the log until it exits. Each spawn of a VMM holds a
//! [`SpawnLock`](super::supervisor::SpawnLock) while the handles are inheritable, so a process
//! that the caller spawns under one, as [`spawn_command`](super::supervisor::spawn_command) does,
//! inherits none. One that the caller spawns otherwise is no process of the launch, though, and
//! does not hold the claim: [`spawn`](super::supervisor::spawn) starts the VMM in a job object of
//! its own, named after the log, which every process that the VMM starts joins and no other
//! process does, and keeps the job's handle inheritable for as long as the log's. Such a process
//! therefore holds the job open for as long as it holds the log, and while the job is open, the
//! claim is held only while the job holds a process. Recovery thus ends a launch whose VMM has
//! exited, whatever such a process holds, if the recovery runs in the Windows session of the
//! spawn: the job is named in that session's namespace, so a recovery from another session does
//! not find it, and checks the log's handles, as for a VMM that an earlier version started outside
//! a job, which such a process then holds until it exits. The job belongs to the user and allows
//! the user alone, and what takes its name but is not the user's job, such as another user's job,
//! an object of another type, or a symbolic link to another job of the user, leaves the claim to
//! the log's handles, so that another user cannot end a launch whose VMM still runs by creating an
//! empty job, or a link to one, under that name. If such an object takes the name at the spawn or
//! at a check, though, a process spawned in the window holds the claim again until it exits, as
//! for a VMM that an earlier version started outside a job, and recovery leaves the launch to a
//! later recovery until then.
//!
//! # Recovery
//!
//! A caller that dies during a launch leaves its journal, and [`LockedStore::recover`] ends the
//! launch, signaling only a VMM that it holds through a verified [`Process`]:
//!
//! - A crash before the spawn leaves the claim released, which proves that nothing of the launch
//!   runs.
//! - After a crash between the spawn and the identity, the VMM holds the claim. Recovery finds
//!   the VMM through the control endpoint that the journal names, holds it, confirms that the
//!   endpoint still names it once held, and terminates it.
//!   On Windows the endpoint is a pipe whose name each launch draws at random, which names the
//!   VMM by itself. On Linux it is the same socket path for every launch of the sandbox, but the
//!   one launch of a clone, whose VMM listens at a socket that its lease reserves, so the
//!   VMM must also hold the claimed log as its standard output. Until the VMM serves its
//!   endpoint, recovery is indeterminate and keeps the journal; once the VMM exits, the released
//!   claim proves it.
//! - After a crash between the identity and the endpoint, the recorded [`ProcessRef`] decides:
//!   the process ID, its start time, and the host's boot. [`Process::open`] holds a live VMM,
//!   which is terminated through its handle, and never another process that reuses its ID, nor
//!   any process when the reference comes from an earlier boot, whose processes have all exited.
//!   Within the current boot, a VMM that it cannot hold, because no process matches the
//!   reference or the process that has its ID, such as another user's, cannot be examined, has
//!   exited once the claim is released; until then, a process of the launch may still run, and
//!   recovery is indeterminate.
//!
//! The runtime record of a committed launch names its running VM, and
//! [`supervisor::reattach`](super::supervisor::reattach) holds that VM again after its
//! orchestrator restarts.
//!
//! # The report staging file
//!
//! The VMM writes its outcome report to a file of its own in the directory, `.tmp` and six ASCII
//! letters or digits, as the `tempfile` crate names it, and then publishes it as `outcome.json`.
//! A VMM that is killed while it writes the report leaves that file. The store owns files of
//! exactly that form, and removes them wherever it clears a launch's files once no process of
//! the launch runs: when [`LockedStore::recover`] or [`Launch::abandon`] ends a launch, unless
//! that is [`Recovery::Indeterminate`], in [`LockedStore::clear_runtime`], when
//! [`LockedStore::begin_launch`] begins the next launch, and in a [removal](#removal). It keeps
//! them while a process of the launch may still run. A file of any other name is not the
//! store's.
//!
//! # Lifecycle locks
//!
//! A caller that keeps a configuration record in the sandbox's directory, whose presence means
//! that the sandbox is provisioned, declares it, with any other files of its own there, as
//! [`CallerFiles`], and takes the lock of each lifecycle transition with them. Provisions, other
//! transitions, and removals of the sandbox then keep its directory consistent, from any process:
//!
//! - [`SandboxStore::lock_for_provision`] creates the directory if needed, and again if a
//!   concurrent removal deletes it before the lock is taken, even while it is created.
//! - [`SandboxStore::lock_existing`] fails with [`ErrorKind::NotFound`] once the directory is gone,
//!   also when a concurrent removal deletes it while the lock is awaited. The caller checks its
//!   configuration record under the lock: a directory that was never provisioned, that an
//!   interrupted removal emptied, or whose removal open files still keep, holds none.
//! - A holder that leaves the directory without the configuration record removes its own lock
//!   file as it releases the lock, unless it removed it already, so that such a directory keeps
//!   no lock file.
//!
//! Both locks wait for another holder until the caller's deadline, trying again every 25 ms, and
//! a deadline that has passed still tries once, so that a caller that waits on its own, as for an
//! interruption, can retry them itself. A lock file that another transition removes or replaces
//! after an attempt opened it is locked again at once, and after a pause, until the deadline, when
//! that happens again right after it, so that transitions that keep replacing the lock file keep
//! no caller past its deadline.
//!
//! # Removal
//!
//! [`LockedStore::remove`] removes a sandbox that no VMM can run in: it refuses while the store
//! records a VM or a launch, which the caller reattaches to, or recovers, first. It removes what a
//! clone kept of its launch, as a release does, and the sandbox's
//! [clone binding](CLONE_BINDING_NAME) first, and makes their absence durable before anything
//! else goes. It checks for those records, and removes those files, through one handle of the
//! directory, once it has checked through that handle that the directory holds its lock, so that
//! no change of a link in the path meanwhile makes it remove another sandbox's files. It then
//! removes the store's other files and the caller's, the configuration record last, and the
//! temporary files that the store owns: those that an interrupted replacement of one of its files
//! leaves, `.NAME.NUMBER.tmp`, and the VMM's [report staging file](#the-report-staging-file). It
//! then refuses entries that are neither the store's nor the caller's, naming them, and keeps the
//! directory, or removes its lock file while it still holds the lock, and then the directory. A
//! removal needs the caller's configuration record, whose absence tells the transitions that wait
//! through it that the sandbox is gone: a lock taken without one refuses it.
//!
//! Files that are still open can keep the emptied directory for a while: NFS renames a removed
//! file that is still open, such as the lock file of a waiting transition, to `.nfs*`, Windows
//! file systems without POSIX deletion keep a removed lock file, or a removed clone binding that a
//! reader holds, under its name until its last handle closes, and a transition that opens the lock
//! file after the removal removed it creates a new one. Each such transition finds no sandbox, and
//! removes its lock file as it releases its lock. The removal leaves those files, then holds the
//! directory's identity, releases its lock, and returns a [`Lingering`] removal, which waits until
//! they go, and reports a provision that took the lock meanwhile as
//! [`RemovalError::ProvisionedAgain`]. A removal removes the directory only while the path still
//! names it, which it checks before each attempt.
//!
//! [`ProcessRef`]: crate::host::process::ProcessRef
//! [`Process`]: crate::host::process::Process
//! [`Process::open`]: crate::host::process::Process::open

mod caller;
pub(crate) mod files;
mod identity;
mod launch;
pub(crate) mod lease;
mod lifecycle;
mod locked;
mod names;
mod record;
pub(crate) mod regular;
#[cfg(test)]
mod tests;
mod vmm;

use std::fs::{File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

pub use self::caller::CallerFiles;
use self::caller::is_report_staging;
use self::files::{StoreFile, io_error, remove_if_present};
pub use self::identity::{
    ArtifactIdentity, IdentityCheck, IdentityPart, MAX_IDENTITY_LEN, RuntimeIdentity,
};
pub use self::launch::{Launch, Recovery};
pub use self::lifecycle::{Lingering, RemovalError};
pub use self::locked::LockedStore;
pub(crate) use self::locked::Reservation;
use self::names::CLONE_FILE_NAMES;
pub use self::names::{
    CAPABILITY_NAME, CLONE_ACTIVATION_NAME, CLONE_BINDING_NAME, CLONE_FAILURE_NAME,
    CLONE_PROGRESS_NAME, CLONE_SCRATCH_NAME, CONSOLE_NAME, FILE_NAMES, FORMAT, JOURNAL_NAME,
    LOCK_NAME, LOG_NAME, OUTCOME_NAME, RESTORE_READY_NAME, RUNTIME_NAME, SOCKET_NAME,
};
use self::record::Record;
pub use self::record::{
    LaunchId, LaunchJournal, LaunchState, LogClaim, MAX_ENDPOINT_LEN, RuntimeRecord,
};
use super::platform;
use crate::error::{Error, ErrorCode, ErrorKind, Result};
use crate::protocol::CAPABILITY_LEN;

/// Interval at which a lock that another holder has is tried again: the sandbox's lock, by a
/// caller that waits for it until a deadline, and a launch's claim on its log, by a recovery that
/// waits for the claim's release. A release is noticed at most this late.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// How long recovery waits for a held claim on a launch's log to be released before it concludes
/// that a process of the launch may still run.
///
/// On Linux, a process holds a copy of each descriptor of its parent from its fork until it
/// executes its program. A spawn by another thread of the process that holds the log, such as
/// the launch of another sandbox's VMM, therefore holds the claim for that moment, which lasted
/// about 5 ms on a busy host; a second leaves a wide margin for a more heavily loaded one.
///
/// Recovery, and the abandon of a failed launch, wait only while they find the claim held: a
/// launch that succeeds and a stop never check it. A claim that stays held is still reported held
/// when the wait ends, so the wait can delay them by up to a second, but never makes them end a
/// launch that may still run.
const CLAIM_RELEASE_WAIT: Duration = Duration::from_secs(1);

/// Returns `dir` as an absolute path, which a relative one names from the current directory.
fn absolute(dir: PathBuf) -> Result<PathBuf> {
    std::path::absolute(&dir).map_err(io_error(format!(
        "cannot resolve the sandbox store {}",
        dir.display()
    )))
}

/// Builds an error mapper that keeps an I/O source under `context`, as an error of
/// [`ErrorKind::NotFound`] if the source says that a path does not exist, and of
/// [`ErrorKind::Io`] otherwise.
fn store_error(context: String) -> impl FnOnce(io::Error) -> Error {
    move |error| {
        let kind = match error.kind() {
            io::ErrorKind::NotFound => ErrorKind::NotFound,
            _ => ErrorKind::Io,
        };
        Error::new(kind, context).with_source(error)
    }
}

/// Returns the error of a wait for another lifecycle transition of the sandbox in `dir` that is
/// still in progress past its deadline, [`ErrorCode::SandboxBusy`].
fn transition_in_progress(dir: &Path) -> Error {
    Error::new(
        ErrorCode::SandboxBusy,
        format!(
            "another lifecycle transition of the sandbox in {} is still in progress",
            dir.display()
        ),
    )
}

/// Waits before the next try of a step that `deadline` bounds: returns `false` at once if the
/// deadline has passed, and otherwise sleeps for [`LOCK_RETRY_INTERVAL`] or the time left,
/// whichever is shorter, and returns `true`.
fn pause(deadline: Option<Instant>) -> bool {
    let left = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
    if left.is_some_and(|left| left.is_zero()) {
        return false;
    }
    thread::sleep(left.map_or(LOCK_RETRY_INTERVAL, |left| left.min(LOCK_RETRY_INTERVAL)));
    true
}

/// The races that the attempts to take a lock lose to other transitions, which remove or replace
/// the lock file, or the directory, after an attempt opens it. The attempt after a lost race
/// follows at once, and one after another race lost right after it pauses first, until the
/// deadline, so that transitions that keep replacing the lock file can neither keep a caller past
/// its deadline nor make it spin.
#[derive(Default)]
struct Races {
    /// Whether the latest attempt followed at once one that lost a race.
    retried_at_once: bool,
}

impl Races {
    /// Records that an attempt lost a race, and returns whether the lock may be tried again
    /// before `deadline`, which it pauses for first if the attempt followed one that lost too.
    fn lost(&mut self, deadline: Option<Instant>) -> bool {
        if self.retried_at_once && !pause(deadline) {
            return false;
        }
        self.retried_at_once = !self.retried_at_once;
        true
    }
}

/// How an attempt to take a store's lock ended, if it did not fail.
enum Taken {
    /// The lock of the lock file that the path names.
    Locked(File),
    /// The lock of a lock file that was removed or replaced after it was opened, which locks
    /// nothing.
    Replaced,
}

/// The store of one sandbox: its private directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxStore {
    /// The sandbox's private directory.
    dir: PathBuf,
}

impl SandboxStore {
    /// Creates the directory `dir` if needed, whose parent must exist, private to the current
    /// user, and opens the store in it.
    ///
    /// An existing directory must already be private, as [`open`](Self::open) requires. One that
    /// others could access is refused rather than restricted: they could have placed records in
    /// it, which would decide what recovery signals, or still hold its files open.
    ///
    /// A relative `dir` is resolved against the current directory, so that the store's paths, and
    /// the control endpoint that a launch records, name the same files from any working directory.
    /// Fails with [`ErrorKind::NotFound`] if the parent does not exist.
    pub fn create(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = absolute(dir.into())?;
        let problem = platform::create_store_dir(&dir).map_err(store_error(format!(
            "cannot create the sandbox store {}",
            dir.display()
        )))?;
        Self::private(dir, problem)
    }

    /// Opens the store in the existing directory `dir`, which a relative path names from the
    /// current directory, and which must be private to the current user, as
    /// [`create`](Self::create) makes it. Fails with [`ErrorKind::NotFound`] if `dir` does not
    /// exist.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = absolute(dir.into())?;
        let problem = platform::check_store_dir(&dir).map_err(store_error(format!(
            "cannot open the sandbox store {}",
            dir.display()
        )))?;
        Self::private(dir, problem)
    }

    /// Returns the store in `dir`, unless `problem` says why `dir` is not private to the current
    /// user.
    fn private(dir: PathBuf, problem: Option<String>) -> Result<Self> {
        match problem {
            None => Ok(Self { dir }),
            Some(problem) => Err(Error::io(format!(
                "the sandbox store {} {problem}",
                dir.display()
            ))),
        }
    }

    /// Returns the sandbox's directory, as an absolute path.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the path of the store's file `name`, one of [`FILE_NAMES`].
    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Returns the store's file `name`, one of [`FILE_NAMES`], which messages call `description`.
    fn file(&self, name: &str, description: &'static str) -> StoreFile {
        StoreFile::new(self.path(name), description)
    }

    /// Returns the sandbox's lock file, [`LOCK_NAME`].
    fn lock_file(&self) -> StoreFile {
        self.file(LOCK_NAME, "sandbox lock")
    }

    /// Returns the launch journal, [`JOURNAL_NAME`].
    fn journal_file(&self) -> StoreFile {
        self.file(JOURNAL_NAME, LaunchJournal::DESCRIPTION)
    }

    /// Returns the runtime record, [`RUNTIME_NAME`].
    fn runtime_file(&self) -> StoreFile {
        self.file(RUNTIME_NAME, RuntimeRecord::DESCRIPTION)
    }

    /// Returns the file that holds the launch capability, [`CAPABILITY_NAME`].
    fn capability_file(&self) -> StoreFile {
        self.file(CAPABILITY_NAME, "sandbox control capability")
    }

    /// Returns the VMM's log, [`LOG_NAME`].
    fn log_file(&self) -> StoreFile {
        self.file(LOG_NAME, "log")
    }

    /// Returns the path of the VMM's log.
    pub fn log_path(&self) -> PathBuf {
        self.path(LOG_NAME)
    }

    /// Returns the path of the guest console's log, which a launch whose caller captures the
    /// console points the VMM at.
    pub fn console_path(&self) -> PathBuf {
        self.path(CONSOLE_NAME)
    }

    /// Returns the path of the VMM's outcome report.
    pub fn outcome_path(&self) -> PathBuf {
        self.path(OUTCOME_NAME)
    }

    /// Returns the path of the file that holds the launch capability.
    pub fn capability_path(&self) -> PathBuf {
        self.path(CAPABILITY_NAME)
    }

    /// Returns the path of the Linux control socket.
    pub fn socket_path(&self) -> PathBuf {
        self.path(SOCKET_NAME)
    }

    /// Returns the path of the binding of the sandbox's clone to the generation of a template that
    /// it leases, [`CLONE_BINDING_NAME`].
    pub fn binding_path(&self) -> PathBuf {
        self.path(CLONE_BINDING_NAME)
    }

    /// Returns the path of the activation record of a clone, [`CLONE_ACTIVATION_NAME`].
    pub fn clone_activation_path(&self) -> PathBuf {
        self.path(CLONE_ACTIVATION_NAME)
    }

    /// Returns the path of the Linux socket at which the launch of a clone awaits the
    /// restore-ready event of its VMM, [`RESTORE_READY_NAME`].
    pub fn restore_ready_path(&self) -> PathBuf {
        self.path(RESTORE_READY_NAME)
    }

    /// Returns the path of the progress log of a clone's launch, [`CLONE_PROGRESS_NAME`].
    pub fn clone_progress_path(&self) -> PathBuf {
        self.path(CLONE_PROGRESS_NAME)
    }

    /// Returns the path of the record of a clone's failure, [`CLONE_FAILURE_NAME`].
    pub fn clone_failure_path(&self) -> PathBuf {
        self.path(CLONE_FAILURE_NAME)
    }

    /// Reads the runtime record, which names the sandbox's VM from the commit of its launch until
    /// its runtime is cleared.
    pub fn runtime(&self) -> Result<Option<RuntimeRecord>> {
        record::read(&self.runtime_file())
    }

    /// Reads the journal of a launch in progress, or of one that was interrupted.
    pub fn journal(&self) -> Result<Option<LaunchJournal>> {
        record::read(&self.journal_file())
    }

    /// Reads the launch capability of the sandbox's VM.
    pub fn read_capability(&self) -> Result<[u8; CAPABILITY_LEN]> {
        let file = self.capability_file();
        let Some(bytes) = file.read()? else {
            return Err(file.read_failed()(io::Error::from(io::ErrorKind::NotFound)));
        };
        match <[u8; CAPABILITY_LEN]>::try_from(bytes.as_slice()) {
            Ok(capability) if capability != [0; CAPABILITY_LEN] => Ok(capability),
            _ => Err(Error::io(format!(
                "the {} {} is invalid",
                file.description(),
                file.path().display()
            ))),
        }
    }

    /// Takes the sandbox's lock, waiting for it until `deadline`, or for as long as it takes
    /// without one.
    ///
    /// Fails with [`ErrorKind::TimedOut`] when the deadline passes first, and with
    /// [`ErrorKind::NotFound`] if the directory does not exist. A lock file that is removed or
    /// replaced while this waits locks nothing, so the lock is then taken again, at once the first
    /// time, and after a pause, until the deadline, when that happens again right after it. A lock
    /// file that is not private to the current user is refused, as a record is, since others could
    /// hold its lock, and one that is a link, or not a regular file, is refused at once.
    ///
    /// This lock knows nothing of its caller's files. A caller that keeps a configuration record
    /// in the directory, or removes the store, takes the [lifecycle locks](self#lifecycle-locks)
    /// instead.
    pub fn lock(&self, deadline: Option<Instant>) -> Result<LockedStore> {
        let mut races = Races::default();
        loop {
            if let Taken::Locked(file) = self.take_lock(deadline)? {
                return Ok(LockedStore::new(self.clone(), file, CallerFiles::none()));
            }
            if !races.lost(deadline) {
                return Err(transition_in_progress(&self.dir));
            }
        }
    }

    /// Opens the lock file, which it creates if needed, takes its lock, waiting for it until
    /// `deadline`, or for as long as it takes without one, and checks that the path still names
    /// the locked file.
    ///
    /// Fails as [`lock`](Self::lock) does. On Windows, a file system without POSIX deletion keeps
    /// a removed file under its name until its last handle closes, and refuses to open it until
    /// then: the open then fails with an I/O error whose cause is the denied access, while a
    /// removed lock file that the lock's own handle keeps is taken for a replaced one.
    fn take_lock(&self, deadline: Option<Instant>) -> Result<Taken> {
        let lock = self.lock_file();
        let path = lock.path();
        let failed = |error| store_error(format!("cannot lock {}", path.display()))(error);
        let file = lock.open_lock(failed)?;
        match deadline {
            None => file.lock().map_err(failed)?,
            Some(deadline) => loop {
                match file.try_lock() {
                    Ok(()) => break,
                    Err(TryLockError::WouldBlock) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            return Err(transition_in_progress(&self.dir));
                        }
                        thread::sleep(left.min(LOCK_RETRY_INTERVAL));
                    }
                    Err(TryLockError::Error(error)) => return Err(failed(error)),
                }
            },
        }
        let locked = platform::opened_file_id(&file).map_err(failed)?;
        match platform::file_id(path) {
            Ok(current) if current == locked => Ok(Taken::Locked(file)),
            Ok(_) => Ok(Taken::Replaced),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Taken::Replaced),
            Err(error) if cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied => {
                Ok(Taken::Replaced)
            }
            Err(error) => Err(failed(error)),
        }
    }

    /// Removes the files in which a VMM of the sandbox staged an outcome report that it never
    /// published, which only a VMM that was killed while it wrote the report leaves. The caller
    /// knows that no process of the sandbox's launches runs. Fails with [`ErrorKind::NotFound`]
    /// if the directory is gone, as [`open`](Self::open) does.
    fn remove_report_staging(&self) -> Result<()> {
        let failed = |error| {
            store_error(format!(
                "cannot list the sandbox store {}",
                self.dir.display()
            ))(error)
        };
        for entry in std::fs::read_dir(&self.dir).map_err(failed)? {
            let name = entry.map_err(failed)?.file_name();
            if name.to_str().is_some_and(is_report_staging) {
                remove_if_present(&self.dir.join(name))?;
            }
        }
        Ok(())
    }
}
