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

mod caller;
pub(crate) mod files;
mod identity;
pub(crate) mod lease;
mod lifecycle;
mod record;
pub(crate) mod regular;
#[cfg(test)]
mod tests;
mod vmm;

use std::fs::{File, TryLockError};
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

pub use self::caller::CallerFiles;
use self::caller::{is_report_staging, is_store_temporary};
use self::files::{StoreFile, io_error, remove_if_present};
pub use self::identity::{
    ArtifactIdentity, IdentityCheck, IdentityPart, MAX_IDENTITY_LEN, RuntimeIdentity,
};
use self::lease::{Lease, clone_socket_name, is_clone_socket, open_payload};
pub use self::lifecycle::{Lingering, RemovalError};
use self::record::Record;
pub use self::record::{
    LaunchId, LaunchJournal, LaunchState, LogClaim, MAX_ENDPOINT_LEN, RuntimeRecord,
};
use super::filesystem::FileIdentity;
use super::platform;
use super::process::{BootId, Process, ProcessRef};
use super::spawn::Launched;
use crate::error::{Error, ErrorCode, ErrorKind, Result};
use crate::protocol::CAPABILITY_LEN;

/// Version of the store's records.
///
/// It follows the versions of the runtime state that the CLI kept on its own, 1, and that
/// [`instance`](super::instance) kept, up to 3, before both moved to the store, so that a record
/// of theirs fails as unsupported rather than being read as one of the store's.
pub const FORMAT: u32 = 4;
/// Name of the sandbox's lock.
pub const LOCK_NAME: &str = "sandbox.lock";
/// Name of the launch journal.
pub const JOURNAL_NAME: &str = "launch.json";
/// Name of the runtime record.
pub const RUNTIME_NAME: &str = "runtime.json";
/// Name of the file that holds the launch capability.
pub const CAPABILITY_NAME: &str = "control.capability";
/// Name of the Linux control socket, at which the VMM of every launch of the sandbox listens, but
/// that of a clone, which listens beside the snapshot that it restores, at the socket that its
/// lease reserves, as [`template::Store::acquire`](crate::host::template::Store::acquire)
/// describes.
pub const SOCKET_NAME: &str = "control.sock";
/// Name of the VMM's log.
pub const LOG_NAME: &str = "openvmm.log";
/// Name of the guest console's log.
pub const CONSOLE_NAME: &str = "console.log";
/// Name of the VMM's outcome report.
pub const OUTCOME_NAME: &str = "outcome.json";
/// Name of the binding of the sandbox's clone to the generation of a template that it leases,
/// which [`template::Store::acquire`](crate::host::template::Store::acquire) writes, durably,
/// before the clone's launch, and [`template::Store::release`](crate::host::template::Store::release)
/// removes once its VM has exited. A [removal](self#removal) of the sandbox removes it first, and
/// makes its absence durable before the sandbox goes.
pub const CLONE_BINDING_NAME: &str = "clone.json";
/// Name of the file of the `scratch` block device that earlier versions of this crate created for
/// a clone whose upper is in RAM, a sparse file of the template's scratch length. Such a clone now
/// restores with its scratch absent, and nothing creates the file, but a sandbox's directory may
/// still hold one that an earlier version left, which
/// [`template::Store::release`](crate::host::template::Store::release) removes once the clone's VM
/// has exited, as a [removal](self#removal) of the sandbox does.
pub const CLONE_SCRATCH_NAME: &str = "clone-scratch.raw";
/// Name of the activation record of a clone, in its canonical encoding, which the clone's launch,
/// [`template::ClonePlan::launch`](crate::host::template::ClonePlan::launch), writes before it
/// spawns the clone's VMM, so that the restore activates the clone with that record and its
/// transaction whatever happens to the launch's caller, and which
/// [`template::Store::release`](crate::host::template::Store::release) removes once the clone's VM
/// has exited, as a [removal](self#removal) of the sandbox does.
pub const CLONE_ACTIVATION_NAME: &str = "clone-activation.bin";
/// Name of the Linux socket at which the launch of a clone,
/// [`template::ClonePlan::launch`](crate::host::template::ClonePlan::launch), listens for the
/// restore-ready event of its VMM, which OpenVMM sends once it has prepared the restore, before
/// the restored VM runs; on Windows, the endpoint is a named pipe, whose name each launch draws at
/// random. [`template::Store::release`](crate::host::template::Store::release) removes it once the
/// clone's VM has exited, as a [removal](self#removal) of the sandbox does.
pub const RESTORE_READY_NAME: &str = "restore.sock";
/// Name of the progress log of a clone's launch, which the launch,
/// [`template::ClonePlan::launch`](crate::host::template::ClonePlan::launch), creates, and to which
/// it and the clone's restore sequence append a line for each step that the clone reaches, without
/// syncing it, so that a reattach tells how far the clone got once the caller of its launch
/// crashed, and which [`template::Store::release`](crate::host::template::Store::release) removes,
/// as a [removal](self#removal) of the sandbox does. No other launch writes it.
pub const CLONE_PROGRESS_NAME: &str = "clone-progress.log";
/// Name of the record of a clone's failure, which a failure of the clone from its launch on
/// writes before its teardown, and again once its teardown is known, as
/// [`template::CloneFailure`](crate::host::template::CloneFailure) describes, and which
/// [`template::Store::release`](crate::host::template::Store::release) removes, as a
/// [removal](self#removal) of the sandbox does.
pub const CLONE_FAILURE_NAME: &str = "clone-failure.json";
/// The files that the clone of a sandbox keeps of its launch, and the file of the scratch device
/// that an earlier version created for a clone in RAM, which a release removes once the clone's VM
/// has exited, as a [removal](self#removal) of the sandbox does, before the clone binding.
const CLONE_FILE_NAMES: [&str; 5] = [
    CLONE_SCRATCH_NAME,
    CLONE_ACTIVATION_NAME,
    RESTORE_READY_NAME,
    CLONE_PROGRESS_NAME,
    CLONE_FAILURE_NAME,
];
/// Every file that the store keeps in a sandbox's directory. Replacing a record leaves a
/// temporary file named `.NAME.NUMBER.tmp` beside it only if the replacement was interrupted, and
/// a VMM that is killed while it writes its outcome report leaves the file in which it stages it;
/// see [the report staging file](self#the-report-staging-file).
pub const FILE_NAMES: [&str; 14] = [
    LOCK_NAME,
    JOURNAL_NAME,
    RUNTIME_NAME,
    CAPABILITY_NAME,
    SOCKET_NAME,
    LOG_NAME,
    CONSOLE_NAME,
    OUTCOME_NAME,
    CLONE_BINDING_NAME,
    CLONE_SCRATCH_NAME,
    CLONE_ACTIVATION_NAME,
    RESTORE_READY_NAME,
    CLONE_PROGRESS_NAME,
    CLONE_FAILURE_NAME,
];

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

/// The store of a sandbox whose lock this holds, through which its lifecycle transitions change
/// its records. Dropping it releases the lock.
///
/// Every transition borrows it exclusively, and a [`Launch`] keeps it borrowed until the launch
/// commits or is abandoned, so transitions never overlap, not even through one guard that
/// threads share. A lock that a [lifecycle lock](self#lifecycle-locks) took also knows its
/// caller's files: as it is released, it removes its lock file if the caller's configuration
/// record is missing, and it can [remove](Self::remove) the store.
#[derive(Debug)]
pub struct LockedStore {
    /// The sandbox's store.
    store: SandboxStore,
    /// The locked file, whose lock lasts until the drop of this holder releases it.
    lock: File,
    /// The caller's files in the directory, which a store's own lock does not know.
    files: CallerFiles,
    /// Whether this holder removed its lock file, after which the path may name another's.
    lock_removed: bool,
    /// The launch of the sandbox's clone that the lease acquired under this lock reserves, until
    /// [`begin_launch`](Self::begin_launch) begins it, or a release ends the lease.
    reserved: Option<Reservation>,
}

/// The launch of a sandbox's clone that a lease reserves, as
/// [`template::Store::acquire`](crate::host::template::Store::acquire) acquires the lease, and
/// where the clone's VMM listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reservation {
    /// The identity of the launch, [`Lease::launch`].
    pub(crate) launch: LaunchId,
    /// The path of the Linux socket at which the clone's VMM listens, its control endpoint: beside
    /// the snapshot that it restores, in the payload of the leased generation, since OpenVMM
    /// requires a restore's control socket in the snapshot's parent, rather than the sandbox's
    /// own control socket. Windows endpoints are named pipes, which [`begin_launch`] draws as it
    /// does for every launch.
    ///
    /// [`begin_launch`]: LockedStore::begin_launch
    pub(crate) socket: PathBuf,
}

impl Deref for LockedStore {
    type Target = SandboxStore;

    fn deref(&self) -> &SandboxStore {
        &self.store
    }
}

impl LockedStore {
    /// Returns the held lock of `store`, on its locked lock file `lock`, whose caller keeps
    /// `files`.
    fn new(store: SandboxStore, lock: File, files: CallerFiles) -> Self {
        Self {
            store,
            lock,
            files,
            lock_removed: false,
            reserved: None,
        }
    }

    /// Lets [`begin_launch`](Self::begin_launch) begin the launch that `reservation` names, once,
    /// as the launch of the sandbox's clone that the lease acquired under this lock reserves,
    /// whose VMM listens at the reservation's socket, or, given `None`, no launch of a clone, once
    /// a release ends the lease.
    pub(crate) fn reserve(&mut self, reservation: Option<Reservation>) {
        self.reserved = reservation;
    }

    /// Returns the launch of the sandbox's clone that the lease acquired under this lock reserves,
    /// until [`begin_launch`](Self::begin_launch) begins it, or a release ends the lease.
    pub(crate) fn reserved(&self) -> Option<&LaunchId> {
        self.reserved
            .as_ref()
            .map(|reservation| &reservation.launch)
    }

    /// Returns the path of the Linux socket at which the VMM of the launch that
    /// [`reserved`](Self::reserved) returns listens.
    pub(crate) fn reserved_socket(&self) -> Option<&Path> {
        self.reserved
            .as_ref()
            .map(|reservation| reservation.socket.as_path())
    }

    /// Returns the paths at which the store reaches the entries of the sandbox's directory that
    /// it owns, which its transitions replace and remove: its files, [`FILE_NAMES`], at its own
    /// paths, whether or not they exist, which the file system resolves as it resolves every
    /// name, without regard to case on Windows, say; and the temporary files of its records and
    /// the files in which a VMM staged an outcome report, under the names by which the directory
    /// lists them now, as a [removal](self#removal) finds them.
    pub(crate) fn owned_entries(&self) -> io::Result<Vec<PathBuf>> {
        let mut owned: Vec<PathBuf> = FILE_NAMES.iter().map(|name| self.path(name)).collect();
        for entry in std::fs::read_dir(self.dir())? {
            let entry = entry?;
            let name = entry.file_name();
            if let Some(name) = name.to_str()
                && (is_store_temporary(name) || is_report_staging(name))
            {
                owned.push(entry.path());
            }
        }
        Ok(owned)
    }

    /// Opens the sandbox's directory through its path, as one handle through which a release of
    /// the lease of its clone names the directory's entries, and checks through that handle that
    /// the directory holds this lock's file, as a [removal](self#removal) checks it: no change of
    /// the path meanwhile, of a link in it or of one of its directories, then makes the release
    /// reach the entries of another sandbox, whose lock it does not hold.
    ///
    /// Fails with [`ErrorKind::Other`] if the directory that the path names does not hold this
    /// lock's file.
    pub(crate) fn open_dir(&self) -> Result<platform::Directory> {
        let dir = self.dir();
        let directory = platform::Directory::open(dir).map_err(io_error(format!(
            "cannot open the directory {}",
            dir.display()
        )))?;
        let lock = self.path(LOCK_NAME);
        let examine = || io_error(format!("cannot examine {}", lock.display()));
        let held = platform::opened_file_id(&self.lock).map_err(examine())?;
        if directory.entry_id(LOCK_NAME).map_err(examine())? != Some(held) {
            return Err(Error::other(format!(
                "the directory that {} names does not hold the lock of its sandbox: the path names \
                 another directory than when the lock was taken",
                dir.display()
            )));
        }
        Ok(directory)
    }

    /// Removes what the clone of the sandbox kept of its launch in the sandbox's directory, whose
    /// VM has exited, through `directory`, the handle of the directory that
    /// [`open_dir`](Self::open_dir) returned: the file of its scratch device,
    /// [`CLONE_SCRATCH_NAME`], if an earlier version left one, its activation record,
    /// [`CLONE_ACTIVATION_NAME`], the socket of its restore-ready event, [`RESTORE_READY_NAME`],
    /// its progress log, [`CLONE_PROGRESS_NAME`], and the record of its failure,
    /// [`CLONE_FAILURE_NAME`]. A release, as a [removal](self#removal) does, removes them before
    /// the clone binding, whose removal, once durable, makes theirs durable too, since all are
    /// entries of the sandbox's directory. A file whose deletion is pending, as a Windows file
    /// system without POSIX deletion leaves a removed file that another handle holds, is removed
    /// already, so that a release that failed once it removed the file can be retried.
    pub(crate) fn remove_clone_files(&self, directory: &platform::Directory) -> Result<()> {
        for name in CLONE_FILE_NAMES {
            directory.remove_file(name).map_err(io_error(format!(
                "cannot remove {}",
                self.path(name).display()
            )))?;
        }
        Ok(())
    }

    /// Returns the caller's files that the lock knows: those that a [lifecycle
    /// lock](self#lifecycle-locks) was taken with, and none for [`SandboxStore::lock`].
    pub fn files(&self) -> &CallerFiles {
        &self.files
    }

    /// Begins the launch `launch`, which the caller reserved with [`LaunchId::generate`], of the
    /// runtime `runtime`, whose VMM receives `capability`.
    ///
    /// It stores the capability, replaces the log of the previous launch with a fresh one, which
    /// it claims, removes the outcome report, console log, and control socket of the previous
    /// launch, and any outcome report that its VMM staged without publishing it, chooses a control
    /// endpoint, and writes the journal's marker, which names the launch. It keeps the console log
    /// of a caller that keeps the console of every launch, as [`CallerFiles::keeping_console`]
    /// declares. Nothing is spawned yet: the caller spawns the VMM with [`Launch::take_log`].
    ///
    /// The control endpoint is a named pipe of a random name on Windows. On Linux, it is the
    /// sandbox's control socket, [`socket_path`](SandboxStore::socket_path), but for the launch
    /// that the lease of the sandbox's clone reserves: its VMM restores the leased generation's
    /// snapshot, and listens beside it, at the socket that the lease reserves in the generation's
    /// payload, as [`template::Store::acquire`](crate::host::template::Store::acquire) describes.
    ///
    /// Fails if the sandbox's VM runs, or a launch was interrupted and is not recovered yet, and
    /// with [`ErrorKind::Unsupported`] for an all-zero capability or an invalid runtime identity.
    /// A sandbox whose clone holds a lease of a template's generation, which its
    /// [clone binding](CLONE_BINDING_NAME) records, launches once, as the launch that the lease
    /// reserves, under the lock that acquired the lease, as
    /// [`template::Store::acquire`](crate::host::template::Store::acquire) returns it: any other
    /// launch, and any launch under another lock or after that one began, fails with
    /// [`ErrorCode::SandboxLeased`] until a release ends the lease.
    pub fn begin_launch(
        &mut self,
        launch: &LaunchId,
        runtime: &RuntimeIdentity,
        capability: &[u8; CAPABILITY_LEN],
    ) -> Result<Launch<'_>> {
        runtime.check()?;
        if *capability == [0; CAPABILITY_LEN] {
            return Err(Error::unsupported(
                "a control capability cannot be all zero",
            ));
        }
        if self.journal()?.is_some() {
            return Err(Error::other(format!(
                "a launch of the sandbox in {} was interrupted and must be recovered first",
                self.dir.display()
            )));
        }
        if self.runtime()?.is_some() {
            return Err(Error::other(format!(
                "the sandbox in {} is already running",
                self.dir.display()
            )));
        }
        if let Some(lease) = Lease::of(self)?
            && (lease.launch() != launch || self.reserved() != Some(launch))
        {
            let reservation = if lease.launch() == launch {
                format!("whose launch {launch} begins once, under the lock that acquired the lease")
            } else {
                format!("which reserves the launch {}, not {launch}", lease.launch())
            };
            return Err(Error::new(
                ErrorCode::SandboxLeased,
                format!(
                    "the sandbox in {} holds the lease {} of the generation {} of the template \
                     store {}, {reservation}",
                    self.dir.display(),
                    lease.id(),
                    lease.generation(),
                    lease.store()
                ),
            ));
        }
        let socket = self.socket_path();
        for path in [&socket, &self.outcome_path()] {
            remove_if_present(path)?;
        }
        if !self.files.keeps_console() {
            remove_if_present(&self.console_path())?;
        }
        self.remove_report_staging()?;
        // The VMM of a clone listens at the socket that its lease reserves, beside the snapshot
        // that it restores.
        let listening = match &self.reserved {
            Some(reservation) if reservation.launch == *launch => reservation.socket.as_path(),
            _ => &socket,
        };
        let endpoint = platform::control_endpoint(listening)
            .map_err(|error| Error::io("cannot choose a control endpoint").with_source(error))?;
        let (log, claim) = self.create_log()?;
        let capability_file = self.capability_file();
        capability_file.replace(capability)?;
        let journal = LaunchJournal::prepared(launch.clone(), endpoint, runtime.clone(), claim);
        if let Err(error) = record::write(&self.journal_file(), &journal) {
            let _ = capability_file.remove_if_present();
            return Err(error);
        }
        // A lease reserves one launch, which has begun.
        self.reserved = None;
        Ok(Launch {
            locked: self,
            journal,
            log: Some(log),
        })
    }

    /// Replaces the log of the previous launch with a fresh file, and claims it.
    fn create_log(&self) -> Result<(File, LogClaim)> {
        let file = self.log_file();
        let failed = |error| {
            io_error(format!(
                "cannot create the {} {}",
                file.description(),
                file.path().display()
            ))(error)
        };
        file.remove_if_present()?;
        let log = file.create_new().map_err(failed)?;
        platform::claim_log(&log).map_err(failed)?;
        let (device, index) = platform::opened_file_identity(&log).map_err(failed)?;
        let claim = LogClaim {
            identity: FileIdentity { device, index },
        };
        Ok((log, claim))
    }

    /// Ends a launch that was interrupted, if its journal remains.
    ///
    /// Recovery signals only a VMM that it holds through a verified [`Process`]: the one that the
    /// journal's [`ProcessRef`] names, or the one that serves the journal's control endpoint, as
    /// the [module documentation](self#recovery) describes. Once nothing of the launch runs, it
    /// removes the launch's capability, control socket, outcome report that its VMM staged
    /// without publishing it, and journal, and keeps its log. A recovery that cannot prove that is
    /// [`Recovery::Indeterminate`] and keeps them all for a later recovery.
    ///
    /// A journal that a committed runtime record of the same launch supersedes is removed. A
    /// runtime record of another launch fails as inconsistent state.
    pub fn recover(&mut self) -> Result<Recovery> {
        let Some(journal) = self.journal()? else {
            return Ok(Recovery::Clean);
        };
        match self.runtime()? {
            None => self.end_launch(&journal),
            Some(runtime) if runtime.launch() == journal.launch() => {
                self.journal_file().remove_if_present()?;
                Ok(Recovery::Clean)
            }
            Some(runtime) => Err(Error::io(format!(
                "the launch journal of the sandbox in {} names launch {}, but its runtime record \
                 names launch {}",
                self.dir.display(),
                journal.launch(),
                runtime.launch()
            ))),
        }
    }

    /// Ends the launch that `journal` records, and removes its files once nothing of it runs.
    fn end_launch(&self, journal: &LaunchJournal) -> Result<Recovery> {
        let recovery = match journal.process() {
            Some(process) => self.end_identified(journal, process, Process::open),
            None => self.end_unidentified(journal),
        };
        if !matches!(recovery, Recovery::Indeterminate(_)) {
            self.capability_file().remove_if_present()?;
            remove_if_present(&self.socket_path())?;
            self.remove_clone_socket(journal.endpoint())?;
            self.remove_report_staging()?;
            // The journal goes last, so that an interrupted removal leaves it for a later
            // recovery to finish.
            self.journal_file().remove_if_present()?;
        }
        Ok(recovery)
    }

    /// Returns whether a process still holds the claim of the launch of `journal` on its log, or
    /// why that cannot be determined.
    ///
    /// A held claim is checked again at every [`LOCK_RETRY_INTERVAL`] until [`CLAIM_RELEASE_WAIT`]
    /// passes, so that a process that another thread spawned while the log was open, which holds
    /// the claim only until it executes its program, is not taken for a process of the launch.
    fn claim_held(&self, journal: &LaunchJournal) -> std::result::Result<bool, String> {
        let path = self.log_path();
        let identity = journal.log().identity;
        let deadline = Instant::now() + CLAIM_RELEASE_WAIT;
        loop {
            match platform::log_claim_held(&path, (identity.device, identity.index)) {
                Ok(true) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(true);
                    }
                    thread::sleep(left.min(LOCK_RETRY_INTERVAL));
                }
                Ok(false) => return Ok(false),
                Err(error) => {
                    return Err(format!(
                        "its claim on the log {} cannot be checked: {error}",
                        path.display()
                    ));
                }
            }
        }
    }

    /// Ends a launch through the recorded `reference` to its VMM, which `open`, [`Process::open`]
    /// outside tests, holds only while the process that it names runs.
    ///
    /// A reference from an earlier boot names a process that has exited. Within the boot that it
    /// names, a VMM that `open` cannot hold, because no process matches the reference or the
    /// process that has its ID cannot be examined, has exited once the launch's claim on its log
    /// is released: until then, a process of the launch may still run.
    fn end_identified(
        &self,
        journal: &LaunchJournal,
        reference: &ProcessRef,
        open: impl FnOnce(&ProcessRef) -> io::Result<Option<Process>>,
    ) -> Recovery {
        let pid = reference.pid;
        match BootId::current() {
            Ok(boot) if boot != reference.boot => return Recovery::Exited,
            Ok(_) => {}
            Err(error) => {
                return Recovery::Indeterminate(format!(
                    "the current boot cannot be identified: {error}"
                ));
            }
        }
        let unheld = match open(reference) {
            Ok(Some(process)) => return end_process(&process),
            Ok(None) => format!("no process matches its VMM process {pid}"),
            Err(error) => format!("its VMM process {pid} cannot be examined: {error}"),
        };
        match self.claim_held(journal) {
            Ok(false) => Recovery::Exited,
            Ok(true) => {
                Recovery::Indeterminate(format!("a process still holds its log, and {unheld}"))
            }
            Err(reason) => Recovery::Indeterminate(format!("{unheld}, and {reason}")),
        }
    }

    /// Ends a launch that did not record its VMM's identity, through its claim on its log.
    fn end_unidentified(&self, journal: &LaunchJournal) -> Recovery {
        match self.claim_held(journal) {
            Ok(false) => return Recovery::Exited,
            Ok(true) => {}
            Err(reason) => return Recovery::Indeterminate(reason),
        }
        // A VMM that the launch spawned may hold the claim. Once it serves its control endpoint,
        // the endpoint names it.
        let endpoint = journal.endpoint();
        let (pid, candidate) = match vmm::hold_endpoint_server(
            || platform::endpoint_server_pid(endpoint),
            vmm::find,
        ) {
            Ok(found) => found,
            Err(reason) => return Recovery::Indeterminate(reason),
        };
        let identity = journal.log().identity;
        match vmm::serves_launch(&candidate, (identity.device, identity.index)) {
            Ok(true) => end_process(&candidate),
            Ok(false) => Recovery::Indeterminate(format!(
                "process {pid} serves its control endpoint, but does not hold its log as its \
                 standard output, so it is not signaled"
            )),
            Err(error) => Recovery::Indeterminate(format!(
                "process {pid} serves its control endpoint, and whether it is its VMM cannot be \
                 determined: {error}"
            )),
        }
    }

    /// Removes the runtime record, the launch capability, and the control socket of a VM that has
    /// exited, and any outcome report that its VMM staged without publishing it, the runtime
    /// record last. The control socket of a clone's VMM, beside the snapshot that it restores, goes
    /// too, and the clearing fails, keeping the runtime record, if a link takes the place of the
    /// template store's directory, of its directory of the generations, or of the payload of the
    /// socket's generation. The log, console log, and outcome report outlive the run.
    pub fn clear_runtime(&mut self) -> Result<()> {
        // Only a Linux endpoint is a file, which may be a clone's beside its snapshot. A record that
        // cannot be read names none, and a release removes a clone's socket all the same.
        let record = if cfg!(unix) {
            self.runtime().ok().flatten()
        } else {
            None
        };
        self.clear_runtime_of(record.as_ref())
    }

    /// Clears the runtime record of a VM that has exited, as [`clear_runtime`](Self::clear_runtime)
    /// does, given `record`, the runtime record that the caller read under this lock, if any, whose
    /// control endpoint is the VM's.
    pub(crate) fn clear_runtime_of(&mut self, record: Option<&RuntimeRecord>) -> Result<()> {
        self.capability_file().remove_if_present()?;
        remove_if_present(&self.socket_path())?;
        if let Some(record) = record {
            self.remove_clone_socket(record.endpoint())?;
        }
        self.remove_report_staging()?;
        self.runtime_file().remove_if_present()
    }

    /// Removes the control socket at `endpoint`, the control endpoint that a launch of the
    /// sandbox recorded, if it is the socket of the sandbox's clone beside the snapshot that its
    /// VMM restores, which the lease that the sandbox's clone binding records reserved, as
    /// [`template::Store::clone_socket_path`](crate::host::template::Store::clone_socket_path)
    /// names it. Any other endpoint stays: the sandbox's own control socket, which the caller
    /// removes, and a named pipe. Neither reads the binding.
    ///
    /// The socket goes through a handle of the generation's payload, which a handle of the
    /// template store's directory opens, as a release opens it: following no link at the store's
    /// directory, at its directory of the generations, or at the payload, and once it checked
    /// through that handle that the directory holds the lease's store, so that neither a link that
    /// takes the place of one of them nor a directory that takes the place of the store ever leads
    /// the removal to the files of another directory. Such a link fails the removal; a payload
    /// that is gone holds no socket, and neither does a directory that holds another store, or
    /// none, in the place of the lease's, whose files stay.
    ///
    /// The removal is not made durable here:
    /// [`template::Store::release`](crate::host::template::Store::release) makes it durable before
    /// the lease ends, and a retirement removes whatever a crash brings back with the generation's
    /// payload.
    fn remove_clone_socket(&self, endpoint: &str) -> Result<()> {
        let socket = Path::new(endpoint);
        if !cfg!(unix) || socket == self.socket_path() {
            return Ok(());
        }
        let Some(lease) = Lease::of(self)?.filter(|lease| is_clone_socket(lease, socket)) else {
            return Ok(());
        };
        // The socket is `<store>/generations/<generation>/<launch>.sock`, as the check found.
        let Some(store) = socket.ancestors().nth(3) else {
            return Ok(());
        };
        match open_payload(store, lease.store(), lease.generation())? {
            Some(payload) => payload
                .remove_file(&clone_socket_name(lease.launch()))
                .map_err(store_error(format!("cannot remove {}", socket.display()))),
            None => Ok(()),
        }
    }
}

/// Ends a launch by terminating its verified VMM `process`.
fn end_process(process: &Process) -> Recovery {
    let pid = process.reference().pid;
    match vmm::terminate(process) {
        Ok(true) => Recovery::Terminated(process.reference().clone()),
        Ok(false) => Recovery::Indeterminate(format!(
            "its VMM process {pid} did not exit after it was terminated"
        )),
        Err(error) => Recovery::Indeterminate(format!(
            "its VMM process {pid} cannot be terminated: {error}"
        )),
    }
}

/// How [`LockedStore::recover`] or [`Launch::abandon`] ended a launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// No launch was interrupted.
    Clean,
    /// The launch never spawned its VMM, or its VMM has exited; its files are removed.
    Exited,
    /// Recovery terminated the launch's VMM, whose reference this holds, after verifying it; its
    /// files are removed.
    Terminated(ProcessRef),
    /// Recovery cannot prove that nothing of the launch runs, for the reason that this gives, and
    /// keeps its files for a later recovery.
    Indeterminate(String),
}

/// A launch in progress, which borrows its sandbox's locked store exclusively from its journal's
/// marker until its runtime record.
///
/// Dropping it before [`commit`](Self::commit) or [`abandon`](Self::abandon) leaves its journal,
/// as a crash would, for [`LockedStore::recover`] to end.
#[derive(Debug)]
pub struct Launch<'a> {
    /// The sandbox's locked store.
    locked: &'a mut LockedStore,
    /// The launch's journal, as written.
    journal: LaunchJournal,
    /// The claimed log, until the caller takes it for the VMM.
    log: Option<File>,
}

impl Launch<'_> {
    /// Returns the launch's journal.
    pub fn journal(&self) -> &LaunchJournal {
        &self.journal
    }

    /// Returns the control endpoint that the launch chose, on which the VMM must listen.
    pub fn endpoint(&self) -> &str {
        self.journal.endpoint()
    }

    /// Takes the claimed log, which the caller passes to the VMM as its standard output and
    /// error, as [`spawn`](super::supervisor::spawn) does with its `log`, once.
    ///
    /// The launch keeps no copy of the log, so the claim lasts as long as the processes that hold
    /// it.
    pub fn take_log(&mut self) -> Result<File> {
        self.log
            .take()
            .ok_or_else(|| Error::other("the log of the launch was taken already"))
    }

    /// Records the reference to the VMM process `child`, which the caller spawned and has not
    /// reaped, right after the spawn, and returns the process, held through a handle of its own
    /// as [`Launched::process`] holds it.
    ///
    /// Fails if the launch has recorded its VMM already, or if the process's identity cannot be
    /// read; [`abandon`](Self::abandon) then ends the launch.
    pub fn record_process(&mut self, child: &Launched) -> Result<Process> {
        if self.journal.process().is_some() {
            return Err(Error::other(
                "the launch has recorded its VMM process already",
            ));
        }
        let process = child
            .process()
            .map_err(|error| Error::io("cannot identify the VMM process").with_source(error))?;
        let journal = self.journal.spawned(process.reference().clone());
        record::write(&self.locked.journal_file(), &journal)?;
        self.journal = journal;
        Ok(process)
    }

    /// Commits the launch of a VM that is ready: writes its runtime record, which then names the
    /// VM, and removes the journal.
    ///
    /// The runtime record is the commit, so a journal that remains because its removal failed is
    /// removed by [`LockedStore::recover`]. Fails if the launch has not recorded its VMM process,
    /// or if the runtime record cannot be written; the journal then remains, as that of an
    /// interrupted launch, for [`LockedStore::recover`] to end.
    pub fn commit(self) -> Result<RuntimeRecord> {
        let Some(process) = self.journal.process() else {
            return Err(Error::other(
                "a launch cannot commit before it records its VMM process",
            ));
        };
        let runtime = RuntimeRecord::committed(&self.journal, process.clone());
        record::write(&self.locked.runtime_file(), &runtime)?;
        let _ = self.locked.journal_file().remove_if_present();
        Ok(runtime)
    }

    /// Ends a launch that failed, as [`LockedStore::recover`] ends an interrupted one: it
    /// terminates the VMM if its identity is verified, and removes the launch's files once
    /// nothing of it runs.
    ///
    /// A caller that still holds the VMM's [`Launched`] process releases it as usual. If the
    /// launch never recorded that process, the caller ends it with [`Launched::kill_and_wait`]
    /// first: an unrecorded VMM that serves no endpoint yet holds the log's claim, so the launch
    /// would end as [`Recovery::Indeterminate`].
    pub fn abandon(mut self) -> Result<Recovery> {
        // A log that no VMM received holds no claim once it closes.
        drop(self.log.take());
        self.locked.end_launch(&self.journal)
    }
}
