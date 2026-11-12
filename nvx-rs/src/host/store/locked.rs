//! The store of a sandbox whose lock the caller holds, through which the sandbox's lifecycle
//! transitions change its records.

use std::fs::File;
use std::io;
use std::ops::Deref;
use std::path::Path;
use std::thread;
use std::time::Instant;

use super::caller::CallerFiles;
use super::files::{io_error, remove_if_present};
use super::identity::RuntimeIdentity;
use super::launch::{Launch, Recovery, end_process};
use super::lease::{Lease, clone_socket_name, is_clone_socket, open_payload};
use super::record::{LaunchId, LaunchJournal, LogClaim, RuntimeRecord};
use super::{CLAIM_RELEASE_WAIT, LOCK_RETRY_INTERVAL, SandboxStore, record, store_error, vmm};
use crate::error::{Error, ErrorCode, Result};
use crate::host::filesystem::FileIdentity;
use crate::host::platform;
use crate::host::process::{BootId, Opened, Process, ProcessRef};
use crate::protocol::CAPABILITY_LEN;

/// The store of a sandbox whose lock this holds, through which its lifecycle transitions change
/// its records. Dropping it releases the lock.
///
/// Every transition borrows it exclusively, and a [`Launch`] keeps it borrowed until the launch
/// commits or is abandoned, so transitions never overlap, not even through one guard that
/// threads share. A lock that a [lifecycle lock] took also knows its caller's files: as it is
/// released, it removes its lock file if the caller's configuration record is missing, and it can
/// [remove](Self::remove) the store.
///
/// [lifecycle lock]: crate::host::store#lifecycle-locks
#[derive(Debug)]
pub struct LockedStore {
    /// The sandbox's store.
    store: SandboxStore,
    /// The locked file, whose lock lasts until the drop of this holder releases it.
    pub(super) lock: File,
    /// The caller's files in the directory, which a store's own lock does not know.
    pub(super) files: CallerFiles,
    /// Whether this holder removed its lock file, after which the path may name another's.
    pub(super) lock_removed: bool,
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
    pub(super) fn new(store: SandboxStore, lock: File, files: CallerFiles) -> Self {
        Self {
            store,
            lock,
            files,
            lock_removed: false,
        }
    }

    /// Returns the caller's files that the lock knows: those that a [lifecycle lock] was taken
    /// with, and none for [`SandboxStore::lock`].
    ///
    /// [lifecycle lock]: crate::host::store#lifecycle-locks
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
    /// The control endpoint is a named pipe of a random name on Windows, and the sandbox's control
    /// socket, [`socket_path`](SandboxStore::socket_path), on Linux.
    ///
    /// Fails if the sandbox's VM runs, or a launch was interrupted and is not recovered yet, and
    /// with [`ErrorKind::Unsupported`] for an all-zero capability or an invalid runtime identity.
    /// A sandbox whose clone holds a lease of a template's generation, which its [clone binding]
    /// records, launches nothing: any launch fails with [`ErrorCode::SandboxLeased`].
    ///
    /// [`ErrorKind::Unsupported`]: crate::ErrorKind::Unsupported
    /// [clone binding]: crate::host::store::CLONE_BINDING_NAME
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
        if let Some(lease) = Lease::of(self)? {
            return Err(Error::new(
                ErrorCode::SandboxLeased,
                format!(
                    "the sandbox in {} holds the lease {} of the generation {} of the template \
                     store {}, which reserves the launch {}",
                    self.dir.display(),
                    lease.id(),
                    lease.generation(),
                    lease.store(),
                    lease.launch()
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
        let endpoint = platform::control_endpoint(&socket)
            .map_err(|error| Error::io("cannot choose a control endpoint").with_source(error))?;
        let (log, claim) = self.create_log()?;
        let capability_file = self.capability_file();
        capability_file.replace(capability)?;
        let journal = LaunchJournal::prepared(launch.clone(), endpoint, runtime.clone(), claim);
        if let Err(error) = record::write(&self.journal_file(), &journal) {
            let _ = capability_file.remove_if_present();
            return Err(error);
        }
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
    /// the [module documentation] describes. Once nothing of the launch runs, it removes the
    /// launch's capability, control socket, outcome report that its VMM staged without publishing
    /// it, and journal, and keeps its log. A recovery that cannot prove that is
    /// [`Recovery::Indeterminate`] and keeps them all for a later recovery.
    ///
    /// A journal that a committed runtime record of the same launch supersedes is removed. A
    /// runtime record of another launch fails as inconsistent state.
    ///
    /// [module documentation]: crate::host::store#recovery
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
    pub(super) fn end_launch(&self, journal: &LaunchJournal) -> Result<Recovery> {
        let recovery = match journal.process() {
            Some(process) => self.end_identified(journal, process, |reference| {
                Ok(match Process::open(reference)? {
                    Opened::Running(process) => Some(process),
                    Opened::Exited(_) => None,
                })
            }),
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
    pub(super) fn end_identified(
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
        // cannot be read names none.
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
    /// VMM restores, which the lease that the sandbox's clone binding records reserved, in the
    /// payload of the leased generation of its template store. Any other endpoint stays: the
    /// sandbox's own control socket, which the caller removes, and a named pipe. Neither reads the
    /// binding.
    ///
    /// The socket goes through a handle of the generation's payload, which a handle of the
    /// template store's directory opens: following no link at the store's directory, at its
    /// directory of the generations, or at the payload, and once it checked through that handle
    /// that the directory holds the lease's store, so that neither a link that takes the place of
    /// one of them nor a directory that takes the place of the store ever leads the removal to the
    /// files of another directory. Such a link fails the removal; a payload that is gone holds no
    /// socket, and neither does a directory that holds another store, or none, in the place of the
    /// lease's, whose files stay. The removal is not made durable here.
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
