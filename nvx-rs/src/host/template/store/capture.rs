//! The captures of templates, and their publication in a store: [`Store::create_template`],
//! [`Store::refresh_template`], and [`Store::recover_captures`]; see
//! [the store's documentation](Store#captures-and-publication).

#[cfg(test)]
mod tests;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use super::{
    GENERATIONS_NAME, Generation, Index, KEY_NAME, MAX_GENERATIONS, SNAPSHOT_NAME, STORE_FORMAT,
    StorageGeneration, Store, entries, read_private, read_record, refuse_problem, store_error,
    take_lock, write_record,
};
use crate::activation::StoreGeneration;
use crate::error::{Error, ErrorCode, ErrorKind, Result};
use crate::host::artifacts::{self, Artifacts};
use crate::host::instance::generate_capability;
use crate::host::platform;
use crate::host::sandbox::distro::{CheckedDistro, DistroBinding, DistroFormat};
use crate::host::sandbox::{LayerRole, SandboxLayer, SandboxPlan};
use crate::host::store::files::{create_new, remove_if_present, sync_dir, write_private};
use crate::host::store::regular::{identity_of, open_regular, read_at_most};
use crate::host::store::{
    JOURNAL_NAME, LOCK_NAME, LOG_NAME, LaunchId, LockedStore, OUTCOME_NAME, Recovery, SOCKET_NAME,
    SandboxStore,
};
use crate::host::supervisor::{self, VmmCommand};
use crate::host::template::eligibility::HostTime;
use crate::host::template::key::{CaptureClass, ClassIdentity, KeyIdentity, TemplateKey};
use crate::host::template::manifest::{MANIFEST_NAME, TemplateManifest, TimeContract};
use crate::host::template::plan::{SCRATCH_NAME, TemplatePlan, TemplateResources};

/// Name of the directory of a store's captures, in which each capture class has a slot.
pub const CAPTURES_NAME: &str = "captures";
/// Name of the transaction directory of a capture, in the slot of its class, which is also the
/// sandbox store of the capture's VM until the VM has ended.
pub const TRANSACTION_NAME: &str = "transaction";
/// Name of the record of a capture, in its transaction and then in its generation's payload.
pub const CAPTURE_RECORD_NAME: &str = "capture.json";

/// Name of the saved state of a snapshot.
const STATE_NAME: &str = "state.bin";
/// Name of the RAM of a snapshot.
const MEMORY_NAME: &str = "memory.bin";
/// The artifacts of a platform-tier snapshot, in the order of their names: its manifest, its RAM,
/// and its saved state.
pub(in crate::host::template) const SNAPSHOT_ARTIFACTS: [&str; 3] =
    [MANIFEST_NAME, MEMORY_NAME, STATE_NAME];

/// Prefix of the name of the file that backs the RAM of a capture's VM, which OpenVMM creates
/// beside the snapshot's destination, and removes as the VM ends.
const MEMORY_FILE_PREFIX: &str = ".openvmm-microvm-memory-";
/// Prefix of the names of the directories in which OpenVMM stages the snapshot, beside its
/// destination, and which it renames to the destination once the snapshot is complete:
/// `.snapshot.staging-`.
const STAGING_PREFIX: &str = ".snapshot.staging-";

/// Hexadecimal digits of the identity of a capture class that name its slot: few enough that the
/// path of the control socket of a capture's VMM fits a Unix socket's.
const SLOT_NAME_LEN: usize = 16;

/// Most bytes of a Unix socket's path on Linux, `sun_path`, with its terminating NUL.
pub(in crate::host::template) const SOCKET_PATH_LIMIT: usize = 108;

/// Longest record of a capture, in bytes.
const MAX_CAPTURE_RECORD_LEN: u64 = 4 * 1024;

/// Longest outcome report of a capture's VM that the capture reads, in bytes.
const MAX_REPORT_LEN: u64 = 1024 * 1024;

/// Interval at which a rename that Windows refuses, while another process holds a file below the
/// renamed directory open, is tried again, until the capture's deadline.
const RENAME_RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// The record of a capture, in messages.
const RECORD_DESCRIPTION: &str = "record of the capture";
/// A capture's transaction, in messages.
const TRANSACTION_WHAT: &str = "the transaction of the capture";
/// The directory of a store's captures, in messages.
const CAPTURES_WHAT: &str = "the captures of the template store";

/// What a template's capture takes: its class, the adapter's plan of its placeholder and its
/// network, the binding of the placeholder, and the artifacts that run its VM, which
/// [`Store::create_template`] and [`Store::refresh_template`] capture.
///
/// The adapter's plan is lowered as [`TemplatePlan::new`] lowers it, with the processors, those
/// online at boot among them, and RAM of the class, and a scratch of the class's scratch length;
/// its upper is no part of the template. The class and the plan must describe one VM: a
/// placeholder whose binding records the class's `distro` length, and a network of the plan whose
/// NIC, as [`SandboxNetwork::template_arguments`] lowers it for a template, is the class's: none,
/// or one at the same guest's address, under the complete deny-all policy, whatever the policies
/// of the two networks.
///
/// [`SandboxNetwork::template_arguments`]: crate::host::sandbox::SandboxNetwork::template_arguments
#[derive(Debug, Clone)]
pub struct Capture {
    /// The class of the capture, which names its slot, and from which the capture takes its
    /// backend, its CPU profile, its processors and those online at boot, its RAM, its scratch
    /// length, and the runtime identity of its launch.
    pub class: CaptureClass,
    /// The adapter's plan, which [`SandboxPlan::adapter`] makes, of the placeholder `distro` and
    /// the template's network.
    pub adapter: SandboxPlan,
    /// The binding of the placeholder, which the capture checks before its VM starts.
    pub binding: DistroBinding,
    /// The revision of the distro policy that the caller accepts, such as
    /// [`DISTRO_POLICY_REVISION`](crate::host::sandbox::distro::DISTRO_POLICY_REVISION), which
    /// [`DistroBinding::check`] requires of the binding's verdict.
    pub policy_revision: u32,
    /// The OpenVMM executable, the guest kernel, and the initramfs, whose identities the class's
    /// runtime holds, which relative paths name from the current directory.
    pub artifacts: Artifacts,
}

/// How [`Store::create_template`] served a capture class: with a generation that the store held
/// already, or with one that it captured and published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Created {
    /// A generation of the class that the store held already.
    Existing(Generation),
    /// A generation that a capture made, and that the store published.
    Captured(Generation),
}

impl Created {
    /// Returns the generation.
    pub fn generation(&self) -> &Generation {
        match self {
            Self::Existing(generation) | Self::Captured(generation) => generation,
        }
    }

    /// Returns the generation, consuming this.
    pub fn into_generation(self) -> Generation {
        match self {
            Self::Existing(generation) | Self::Captured(generation) => generation,
        }
    }
}

/// Syncs a directory, as [`sync_dir`] does. Captures take it as a parameter, so that tests
/// observe the order of their syncs, and fail them.
type SyncDir<'a> = &'a dyn Fn(&Path) -> Result<()>;

/// Runs the VM of a capture in the slot whose lock the capture holds, as [`run_supervised`] does,
/// and returns once the VM has ended, and nothing of its launch runs. Tests run it in their own
/// process instead.
type RunVm<'a> = &'a dyn Fn(&mut LockedStore, &Vm<'_>) -> Result<()>;

/// A point of a capture at which a test fails it, before the step that follows the point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Point {
    /// The transaction holds the record of the capture and its scratch, and the VM has not
    /// started.
    Prepared,
    /// The VM has ended, and nothing that it left is checked yet.
    Exited,
    /// The transaction is checked: it holds the template's key, durably.
    Checked,
    /// The envelope is renamed into the store, and the index does not list it yet.
    Renamed,
    /// The index lists the new generation.
    Indexed,
}

/// The seams of a capture, through which tests observe and interrupt it.
struct Hooks<'a> {
    /// Syncs a directory.
    sync: SyncDir<'a>,
    /// Runs the capture's VM.
    run: RunVm<'a>,
    /// Fails the capture at a point, as a test asks.
    interrupt: &'a dyn Fn(Point) -> Result<()>,
    /// Whether a failure leaves the capture's files as they are, as a crash of the host would,
    /// rather than retiring its transaction: in tests that crash captures.
    crash: bool,
}

impl Hooks<'static> {
    /// Returns the seams of every capture outside tests.
    fn supervised() -> Self {
        Self {
            sync: &sync_dir,
            run: &run_supervised,
            interrupt: &uninterrupted,
            crash: false,
        }
    }
}

impl Hooks<'_> {
    /// Fails the capture at `point` if a test asks.
    fn at(&self, point: Point) -> Result<()> {
        (self.interrupt)(point)
    }
}

/// Lets a capture pass every point, as captures outside tests do.
fn uninterrupted(_: Point) -> Result<()> {
    Ok(())
}

/// What a capture takes of its inputs once it has checked them, before it opens anything.
struct Inputs {
    /// The identity of the class.
    class: ClassIdentity,
    /// The template's processors, those online at boot among them, and RAM, from the class.
    resources: TemplateResources,
    /// The artifacts, as absolute paths.
    artifacts: Artifacts,
}

impl Inputs {
    /// Checks the inputs of `capture`, without reading or opening anything, and returns what the
    /// capture takes of them.
    ///
    /// Fails with [`ErrorKind::Unsupported`] as [`CaptureClass::identity`] fails, and for a
    /// placeholder whose binding records another length than the class's `distro`, and a plan
    /// whose network attaches another NIC than the class's, as a template attaches it; and with
    /// [`ErrorKind::Unavailable`] for an artifact whose path cannot be resolved.
    fn check(capture: &Capture) -> Result<Self> {
        let class = &capture.class;
        let identity = class.identity()?;
        let length = capture.binding.image().length();
        if length != class.distro.length {
            return Err(Error::unsupported(format!(
                "the placeholder {} is {length} bytes long, as its binding records it, but the \
                 class's distro is {} bytes long",
                capture.adapter.distro().path().display(),
                class.distro.length
            )));
        }
        let planned = match capture.adapter.network() {
            None => Vec::new(),
            Some(network) => network.template_arguments()?,
        };
        let requested = match &class.network {
            None => Vec::new(),
            Some(network) => network.template_arguments()?,
        };
        if planned != requested {
            return Err(Error::unsupported(format!(
                "the adapter's plan attaches the NIC {planned:?}, but the class requests \
                 {requested:?}"
            )));
        }
        let given = &capture.artifacts;
        Ok(Self {
            class: identity,
            resources: TemplateResources {
                processors: class.processors,
                boot_processors: class.boot_processors,
                memory: class.memory,
                memory_capacity: class.memory_capacity,
            },
            artifacts: Artifacts::new(
                artifacts::absolute(&given.openvmm)?,
                artifacts::absolute(&given.kernel)?,
                artifacts::absolute(&given.initrd)?,
            ),
        })
    }
}

/// The VM of a capture, as the capture's source runs it.
struct Vm<'a> {
    /// The inputs of the capture.
    capture: &'a Capture,
    /// The plan of the template.
    template: &'a TemplatePlan,
    /// The OpenVMM executable, the guest kernel, and the initramfs, as absolute paths.
    artifacts: &'a Artifacts,
    /// When the capture must have ended.
    deadline: Instant,
}

impl Vm<'_> {
    /// Returns the command of the capture's VMM, run in the capture's transaction `dir`, whose
    /// control console listens on `endpoint`, and whose outcome report goes to `report`.
    ///
    /// Its arguments are the template's, as [`TemplatePlan::openvmm_arguments`] renders them,
    /// then the backend, the CPU profile, the kernel and the initramfs, the template's kernel
    /// command line, a boot console that discards its output, the control console, and the
    /// outcome report.
    fn command(&self, endpoint: &str, report: &Path, dir: &Path) -> Result<VmmCommand<'static>> {
        let class = &self.capture.class;
        let mut arguments: Vec<OsString> = self
            .template
            .openvmm_arguments()?
            .into_iter()
            .map(OsString::from)
            .collect();
        let listen = format!("listen={endpoint}");
        for argument in [
            OsStr::new("--single-process"),
            OsStr::new("--hypervisor"),
            OsStr::new(class.hypervisor.as_str()),
            OsStr::new("--cpu-profile"),
            OsStr::new(&class.cpu_profile),
            OsStr::new("--kernel"),
            self.artifacts.kernel.as_os_str(),
            OsStr::new("--initrd"),
            self.artifacts.initrd.as_os_str(),
            OsStr::new("--cmdline"),
            OsStr::new(self.template.kernel_command_line()),
            OsStr::new("--virtio-console"),
            OsStr::new("none"),
            OsStr::new("--microvm-control-console"),
            OsStr::new(&listen),
            OsStr::new("--microvm-control-auth-stdin"),
            OsStr::new("--microvm-report"),
            report.as_os_str(),
        ] {
            arguments.push(argument.to_owned());
        }
        Ok(VmmCommand::new(&self.artifacts.openvmm, arguments, dir))
    }
}

/// Runs the VM of a capture through the supervisor, from the sandbox store of `locked`, the
/// capture's transaction, as a launch of its own, until its VMM exits, or until the capture's
/// deadline, at which it ends the VMM, and then ends the launch.
///
/// The capture opens no control session, and waits for no Ready: its guest captures itself, and
/// OpenVMM exits once it has published the snapshot, or once the capture failed. Fails with
/// [`ErrorKind::TimedOut`] if the deadline passes first, and with
/// [`ErrorCode::VmmIndeterminate`] if the launch cannot be ended, whose journal then remains for
/// the recovery of the slot; and as [`supervisor::launch`] fails.
fn run_supervised(locked: &mut LockedStore, vm: &Vm<'_>) -> Result<()> {
    let report = locked.outcome_path();
    let dir = locked.dir().to_owned();
    let capability = generate_capability()?;
    // A slot holds no lease, so each capture reserves its launch itself.
    let launch = LaunchId::generate()?;
    let starting = supervisor::launch(
        locked,
        &launch,
        &vm.capture.class.runtime,
        &capability,
        |endpoint| vm.command(endpoint, &report, &dir),
    )?;
    let exited = starting.process().wait(vm.deadline).map_err(|error| {
        Error::io(format!(
            "cannot wait for the VMM of the capture in {}",
            dir.display()
        ))
        .with_source(error)
    });
    if let Recovery::Indeterminate(reason) = starting.abandon()? {
        return Err(indeterminate(&dir, &reason));
    }
    if !exited? {
        return Err(Error::timed_out(format!(
            "the VM of the capture in {} did not end before the capture's deadline, and was torn \
             down",
            dir.display()
        )));
    }
    Ok(())
}

/// Returns the error of a capture whose VMM, in the slot `dir`, may still run, for `reason`,
/// [`ErrorCode::VmmIndeterminate`].
fn indeterminate(dir: &Path, reason: &str) -> Error {
    Error::new(
        ErrorCode::VmmIndeterminate,
        format!(
            "the VMM of the capture in {} may still run, and its launch remains for recovery: \
             {reason}",
            dir.display()
        ),
    )
}

impl Store {
    /// Returns a generation of the class of `capture` that the store holds, or captures one and
    /// publishes it, waiting for the capture's slot and the store's lock until `deadline`, which
    /// bounds the capture too.
    ///
    /// Captures of one class are serialized by the lock of the class's slot, under which a
    /// creation first ends a capture of the class that a crash interrupted, and completes the
    /// store's publications that a crash interrupted, as
    /// [`recover_captures`](Self::recover_captures) does. It then calls `now` for the host's time,
    /// once, and looks for a generation: the latest that the store indexes of the class, of the
    /// key of the class's latest generation, that no tombstone quarantines, and that is eligible
    /// at that time, as [`Generation::eligibility`] decides it, as [`select`](Self::select)
    /// chooses among the generations of a key. Without one, it captures one, as
    /// [`refresh_template`](Self::refresh_template) does. Since `now` is called once the creation
    /// holds the slot, a generation that a racing creation captured while this one waited is
    /// eligible at that time, so creations that race for one class make one generation between
    /// them. [`HostTime::now`] samples this host's time; a caller that keeps a margin, as before
    /// the window of a generation closes, returns a later time.
    ///
    /// Fails as `now` fails, and otherwise as [`refresh_template`](Self::refresh_template) does.
    pub fn create_template(
        &self,
        capture: &Capture,
        now: impl Fn() -> Result<HostTime>,
        deadline: Instant,
    ) -> Result<Created> {
        self.capture(capture, Some(&now), deadline, &Hooks::supervised())
    }

    /// Captures a template of the class of `capture`, and publishes it as the store's next
    /// generation, which later selections of its key take before the older ones, waiting for the
    /// capture's slot and the store's lock until `deadline`, which bounds the capture too.
    ///
    /// The capture cold-boots the template's VM through [`supervisor::launch`], from its
    /// transaction, in the slot of its class, and waits for it to exit: its guest captures itself,
    /// and OpenVMM publishes the snapshot into the capture's transaction and exits. The capture
    /// checks the files before the VM starts, and what the VM left once it has exited, publishes
    /// the transaction in two commits, and leaves the older generations, and their leases,
    /// untouched; see [the store's documentation](Store#captures-and-publication).
    ///
    /// Fails, and publishes nothing, but a transaction that it checked, which the next recovery
    /// of the slot publishes:
    ///
    /// - with [`ErrorKind::Unsupported`] for inputs that [`Capture`] does not admit, as
    ///   [`TemplatePlan::new`] refuses them, for a store whose path is too long for the control
    ///   socket of a capture's VMM, on Linux, and for a store that holds
    ///   [`MAX_GENERATIONS`] generations;
    /// - with [`ErrorKind::Unavailable`] for a placeholder that its binding's check refuses, as
    ///   [`DistroBinding::check`] fails, that is a link, or on Windows any other reparse point,
    ///   or that is not a regular file, and for one that belongs to another user than the current
    ///   one or an administrator, or that anyone else may change;
    /// - with [`ErrorKind::Exited`] if the VM ended without a capture, as when its guest failed,
    ///   or OpenVMM did, even after it published the snapshot;
    /// - with [`ErrorKind::TimedOut`] if the deadline passes before the VM ends, which the
    ///   capture then tears down, or before the capture takes its slot's lock or the store's;
    /// - with [`ErrorCode::VmmIndeterminate`] if the capture cannot prove that the VMM of a
    ///   capture of the class, its own or an interrupted one, no longer runs: the slot then keeps
    ///   the launch and its transaction for a later recovery;
    /// - with [`ErrorKind::Io`] for files of the slot, of the transaction, or of the store that
    ///   are links, that are not private to the current user, or that cannot be created, read,
    ///   synced, or renamed, and for a capture that did not leave what its class requests: a
    ///   snapshot whose manifest [`TemplateManifest::read`] refuses, that is not the class's, as
    ///   [`TemplateKey::new`] checks it, or whose artifacts are not exactly its manifest, RAM, and
    ///   saved state, each a regular file of the length that the manifest records.
    pub fn refresh_template(&self, capture: &Capture, deadline: Instant) -> Result<Generation> {
        self.capture(capture, None, deadline, &Hooks::supervised())
            .map(Created::into_generation)
    }

    /// Ends the captures that a crash interrupted, waiting for the lock of each slot and for the
    /// store's until `deadline`, and returns the generations that it published, with those whose
    /// publications it completed, in the order of their store generations.
    ///
    /// In each slot, it ends the launch of the capture's VMM, as [`LockedStore::recover`] ends
    /// it, then publishes the capture's transaction if the capture had checked it, and retires it
    /// otherwise. It then completes a publication that a crash interrupted once it had renamed
    /// its envelope into the store, and drops from the index the generations whose payloads a
    /// crash lost. A capture recovers the slot of its class, and the store's publications, the
    /// same way, so an adapter recovers the other slots once, as it starts.
    ///
    /// Fails with [`ErrorCode::ForeignFiles`] if the directory of the captures holds entries that
    /// are not slots, and otherwise as [`refresh_template`](Self::refresh_template) fails.
    pub fn recover_captures(&self, deadline: Instant) -> Result<Vec<Generation>> {
        self.recover_with(deadline, &Hooks::supervised())
    }

    /// Serves `capture` as [`create_template`](Self::create_template) does with the host's time
    /// that `now` returns, or, without it, as [`refresh_template`](Self::refresh_template) does,
    /// through `hooks`.
    fn capture(
        &self,
        capture: &Capture,
        now: Option<&dyn Fn() -> Result<HostTime>>,
        deadline: Instant,
        hooks: &Hooks<'_>,
    ) -> Result<Created> {
        let inputs = Inputs::check(capture)?;
        let slot = self.open_slot(inputs.class)?;
        let locked = lock_slot(&slot, deadline)?;
        self.recover_slot(&locked, deadline, hooks)?;
        // The index then lists every generation that a publication renamed into the store, and
        // none whose payload a crash lost, for the lookup and the store's capacity alike.
        self.recover_publications(deadline, hooks)?;
        if let Some(now) = now
            && let Some(existing) = self.existing(inputs.class, &now()?)?
        {
            // The commit that listed it may have been interrupted before it made the index, or
            // the store's own entry in its parent, durable.
            self.sync_store(hooks)?;
            return Ok(Created::Existing(existing));
        }
        if self.index()?.generations.len() >= MAX_GENERATIONS {
            return Err(self.full());
        }
        let transaction = locked.dir().join(TRANSACTION_NAME);
        let template = TemplatePlan::new(
            &capture.adapter,
            &capture.binding,
            capture.class.scratch.length,
            inputs.resources,
            &transaction,
            StorageGeneration::generate()?,
        )?;
        let envelope =
            match self.check_capture(capture, &inputs, &template, &locked, deadline, hooks) {
                Ok(envelope) => envelope,
                Err(error) => {
                    // A transaction stays while its sandbox store records a launch, whose VMM may
                    // still write it, for the slot's recovery to retire once the VMM has ended;
                    // and so does one that a retirement interrupted, as a crash would leave it.
                    if !hooks.crash && !records_launch(&transaction) {
                        let _ = keep_diagnostics(&transaction, locked.dir());
                        let _ = retire(&transaction);
                    }
                    return Err(error);
                }
            };
        hooks.at(Point::Checked)?;
        self.publish(&transaction, &envelope, deadline, hooks)
            .map(|(published, _)| Created::Captured(published))
    }

    /// Runs the capture of `template`, with the inputs `capture`, checked as `inputs`, in the slot
    /// whose lock `slot` holds, through `hooks`, until the capture checks its transaction, or until
    /// `deadline`, and returns its envelope.
    ///
    /// The transaction is the sandbox store of the capture's VM: OpenVMM requires the control
    /// socket of a VM that captures a snapshot, on Linux, beside the snapshot's destination, and a
    /// sandbox store's VM listens in the store's directory.
    fn check_capture(
        &self,
        capture: &Capture,
        inputs: &Inputs,
        template: &TemplatePlan,
        slot: &LockedStore,
        deadline: Instant,
        hooks: &Hooks<'_>,
    ) -> Result<Envelope> {
        let transaction = template.transaction();
        // The slot keeps the log and outcome report of its class's latest capture, this one, so
        // that the capture never reads an earlier VM's report for its own.
        for name in [LOG_NAME, OUTCOME_NAME] {
            remove_if_present(&slot.dir().join(name))?;
        }
        let mut launch = SandboxStore::create(transaction)?.lock(Some(deadline))?;
        write_record(
            &transaction.join(CAPTURE_RECORD_NAME),
            &CaptureRecord::new(self, inputs.class, template.generation()),
            &format!("the {RECORD_DESCRIPTION}"),
        )?;
        let scratch = create_scratch(template.scratch_path(), template.scratch_length())?;
        hooks.at(Point::Prepared)?;
        let placeholder = check_files(capture, template, scratch)?;
        let vm = Vm {
            capture,
            template,
            artifacts: &inputs.artifacts,
            deadline,
        };
        (hooks.run)(&mut launch, &vm)?;
        drop(placeholder);
        keep_diagnostics(transaction, slot.dir())?;
        hooks.at(Point::Exited)?;
        check_exit(&slot.outcome_path())?;
        let snapshot = template.destination();
        let manifest = check_snapshot(snapshot)?;
        let key = TemplateKey::new(&capture.class, &manifest)?;
        // The host closes its handles before it publishes: the transaction's lock goes, and its
        // file with what else the capture leaves.
        drop(launch);
        clean_transaction(transaction)?;
        write_private(&transaction.join(KEY_NAME), &key.to_bytes())?;
        // The checked transaction must survive a crash, for the slot's recovery to publish it,
        // and so must the store, whose own entry in its parent its creation does not sync.
        for dir in [transaction, slot.dir(), &self.captures_dir()] {
            (hooks.sync)(dir)?;
        }
        self.sync_store(hooks)?;
        Ok(Envelope::new(
            &key,
            inputs.class,
            template.generation(),
            &manifest,
        ))
    }

    /// Publishes the checked transaction `transaction`, whose envelope is `envelope`, as the
    /// store's next generation, under the store's lock, which it waits for until `deadline`,
    /// through `hooks`, and returns the generation, with those of the publications that it
    /// completed first.
    ///
    /// It first recovers the index, as [`recover_index`](Self::recover_index) does. Then it
    /// renames the transaction to the payload of the next store generation, which it never
    /// replaces, makes the rename durable, and commits the index, which lists the generation with
    /// its full key, as [`commit_index`](Self::commit_index) does.
    fn publish(
        &self,
        transaction: &Path,
        envelope: &Envelope,
        deadline: Instant,
        hooks: &Hooks<'_>,
    ) -> Result<(Generation, Vec<Generation>)> {
        let _lock = take_lock(&self.dir, Some(deadline))?;
        let mut index = self.index()?;
        let (completed, changed) = self.recover_index(&mut index, hooks)?;
        if index.generations.len() >= MAX_GENERATIONS {
            if changed {
                self.commit_index(&index, hooks)?;
            }
            return Err(self.full());
        }
        let generation = index.next;
        rename_until(transaction, &self.payload_dir(generation), deadline)?;
        (hooks.sync)(&self.dir.join(GENERATIONS_NAME))?;
        if let Some(slot) = transaction.parent() {
            (hooks.sync)(slot)?;
        }
        hooks.at(Point::Renamed)?;
        let published = envelope.generation(generation);
        index.generations.push(published.clone());
        index.next = following(generation)?;
        self.commit_index(&index, hooks)?;
        hooks.at(Point::Indexed)?;
        Ok((published, completed))
    }

    /// Writes `index`, which the store's lock holds, and makes it durable through `hooks`, as
    /// [`sync_store`](Self::sync_store) does, so that a crash of the host loses neither a
    /// generation that it lists nor the store.
    fn commit_index(&self, index: &Index, hooks: &Hooks<'_>) -> Result<()> {
        self.write_index(index)?;
        self.sync_store(hooks)
    }

    /// Syncs the store's directory, and then its parent, through `hooks`: [`Store::create`]
    /// syncs neither, so the store's own entry in its parent may not be durable yet.
    fn sync_store(&self, hooks: &Hooks<'_>) -> Result<()> {
        (hooks.sync)(&self.dir)?;
        if let Some(parent) = self.dir.parent() {
            (hooks.sync)(parent)?;
        }
        Ok(())
    }

    /// Completes the store's publications that a crash interrupted, and drops from the index the
    /// generations whose payloads a crash lost, as [`recover_index`](Self::recover_index) does,
    /// under the store's lock, which it waits for until `deadline`, through `hooks`. Commits the
    /// index if that changed it, and returns the generations that it completed.
    fn recover_publications(
        &self,
        deadline: Instant,
        hooks: &Hooks<'_>,
    ) -> Result<Vec<Generation>> {
        let _lock = take_lock(&self.dir, Some(deadline))?;
        let mut index = self.index()?;
        let (completed, changed) = self.recover_index(&mut index, hooks)?;
        if changed {
            self.commit_index(&index, hooks)?;
        }
        Ok(completed)
    }

    /// Recovers `index`, which the store's lock holds, from the publications and retirements that
    /// a crash interrupted, through `hooks`, and returns the generations that it indexed, and
    /// whether it changed the index, which the caller writes.
    ///
    /// A generation whose payload is gone was lost in a crash, as one that a publication renamed
    /// on a host that cannot flush a directory, or whose retirement removed its payload before it
    /// wrote the index: the index first drops it, so that it takes none of the store's capacity.
    /// Its tombstone, if any, stays for its retirement to remove. A publication renames its
    /// envelope to the payload of the index's next store generation before it writes the index,
    /// so a payload there is one whose publication was interrupted: the index then takes it, as
    /// the publication would have, once the recovery has made the rename durable, as the
    /// publication would have too: it syncs the payload, the directory of the payloads, and the
    /// slot that the envelope left.
    fn recover_index(
        &self,
        index: &mut Index,
        hooks: &Hooks<'_>,
    ) -> Result<(Vec<Generation>, bool)> {
        self.check_generations_dir()?;
        let listed = index.generations.len();
        let mut kept = Vec::with_capacity(listed);
        for generation in index.generations.drain(..) {
            match fs::symlink_metadata(self.payload_dir(generation.generation)) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => kept.push(generation),
            }
        }
        index.generations = kept;
        let dropped = index.generations.len() != listed;
        let mut completed = Vec::new();
        loop {
            let payload = self.payload_dir(index.next);
            match fs::symlink_metadata(&payload) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(store_error(format!(
                        "cannot open the payload of the generation {}",
                        payload.display()
                    ))(error));
                }
                Ok(_) => {}
            }
            if index.generations.len() >= MAX_GENERATIONS {
                return Err(self.full());
            }
            let generation = self.read_envelope(&payload)?.generation(index.next);
            (hooks.sync)(&payload)?;
            index.generations.push(generation.clone());
            index.next = following(index.next)?;
            completed.push(generation);
        }
        if !completed.is_empty() {
            (hooks.sync)(&self.dir.join(GENERATIONS_NAME))?;
            let mut slots: Vec<String> = completed
                .iter()
                .map(|generation| slot_name(generation.class))
                .collect();
            slots.sort();
            slots.dedup();
            for name in slots {
                let slot = self.captures_dir().join(name);
                match fs::symlink_metadata(&slot) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(store_error(format!(
                            "cannot open the slot {}",
                            slot.display()
                        ))(error));
                    }
                    Ok(metadata) if metadata.is_dir() => (hooks.sync)(&slot)?,
                    Ok(_) => {}
                }
            }
        }
        let changed = dropped || !completed.is_empty();
        Ok((completed, changed))
    }

    /// Reads the envelope of a checked capture in the directory `dir`, its transaction or its
    /// payload, and returns what the index lists of it: the record of the capture, the template's
    /// key, and the manifest of its snapshot, whose artifacts it checks again by their kinds and
    /// lengths.
    fn read_envelope(&self, dir: &Path) -> Result<Envelope> {
        let what = "the envelope of the capture";
        let problem = platform::check_owner_only_dir(dir)
            .map_err(store_error(format!("cannot open {what} {}", dir.display())))?;
        refuse_problem(dir, what, problem)?;
        let mut names = entries(dir)?;
        names.sort();
        if names != [CAPTURE_RECORD_NAME, KEY_NAME, SNAPSHOT_NAME] {
            return Err(Error::io(format!(
                "{what} {} holds {}, not exactly the {RECORD_DESCRIPTION}, its key, and its \
                 snapshot",
                dir.display(),
                names.join(", ")
            )));
        }
        let path = dir.join(CAPTURE_RECORD_NAME);
        let record =
            read_record::<CaptureRecord>(&path, RECORD_DESCRIPTION, MAX_CAPTURE_RECORD_LEN)?
                .ok_or_else(|| {
                    Error::io(format!(
                        "the {RECORD_DESCRIPTION} {} is missing",
                        path.display()
                    ))
                })?;
        let malformed = |problem: &str| {
            Error::io(format!(
                "the {RECORD_DESCRIPTION} {} is malformed: {problem}",
                path.display()
            ))
        };
        if record.store != self.id.to_string() {
            return Err(malformed(&format!(
                "it is a capture of the template store {}",
                record.store
            )));
        }
        let class: ClassIdentity = record
            .class
            .parse()
            .map_err(|error: Error| malformed(error.message()))?;
        let storage: StorageGeneration = record
            .storage_generation
            .parse()
            .map_err(|error: Error| malformed(error.message()))?;
        let path = dir.join(KEY_NAME);
        let description = "template key";
        let bytes = read_private(&path, description, TemplateKey::MAX_LEN as u64)?
            .ok_or_else(|| Error::io(format!("the {description} {} is missing", path.display())))?;
        let key = TemplateKey::from_bytes(&bytes).map_err(|error| {
            Error::new(
                error.code(),
                format!("cannot read {}: {}", path.display(), error.message()),
            )
        })?;
        let manifest = check_snapshot(&dir.join(SNAPSHOT_NAME))?;
        Ok(Envelope::new(&key, class, storage, &manifest))
    }

    /// Ends the capture that a crash interrupted in the slot whose lock `slot` holds, if any,
    /// through `hooks`: ends its VMM, which the transaction's sandbox store records, as
    /// [`LockedStore::recover`] does, and then publishes its transaction if the capture checked
    /// it, waiting for the store's lock until `deadline`, and retires it otherwise, once it has
    /// moved the VMM's log and outcome report to the slot. Returns the generations that it
    /// published: those of the publications that it completed first, as
    /// [`publish`](Self::publish) does, and then the transaction's.
    fn recover_slot(
        &self,
        slot: &LockedStore,
        deadline: Instant,
        hooks: &Hooks<'_>,
    ) -> Result<Vec<Generation>> {
        let transaction = slot.dir().join(TRANSACTION_NAME);
        match platform::check_owner_only_dir(&transaction) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            checked => {
                let problem = checked.map_err(store_error(format!(
                    "cannot open {TRANSACTION_WHAT} {}",
                    transaction.display()
                )))?;
                refuse_problem(&transaction, TRANSACTION_WHAT, problem)?;
            }
        }
        end_launch(&transaction, deadline)?;
        let key = transaction.join(KEY_NAME);
        match fs::symlink_metadata(&key) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                keep_diagnostics(&transaction, slot.dir())?;
                retire(&transaction)?;
                Ok(Vec::new())
            }
            Err(error) => Err(store_error(format!("cannot open {}", key.display()))(error)),
            Ok(_) => {
                let envelope = self.read_envelope(&transaction)?;
                // The crash may have come before the capture synced the transaction that it
                // checked, whose entries must be durable before the rename publishes them.
                (hooks.sync)(&transaction)?;
                let (published, mut completed) =
                    self.publish(&transaction, &envelope, deadline, hooks)?;
                completed.push(published);
                Ok(completed)
            }
        }
    }

    /// Recovers the store's captures as [`recover_captures`](Self::recover_captures) does,
    /// through `hooks`.
    fn recover_with(&self, deadline: Instant, hooks: &Hooks<'_>) -> Result<Vec<Generation>> {
        let captures = self.captures_dir();
        let mut published = Vec::new();
        let exists = match platform::check_owner_only_dir(&captures) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            checked => {
                let problem = checked.map_err(store_error(format!(
                    "cannot open {CAPTURES_WHAT} {}",
                    captures.display()
                )))?;
                refuse_problem(&captures, CAPTURES_WHAT, problem)?;
                true
            }
        };
        if exists {
            let mut slots = entries(&captures)?;
            slots.sort();
            let foreign: Vec<&str> = slots
                .iter()
                .map(String::as_str)
                .filter(|name| !is_slot_name(name))
                .collect();
            if !foreign.is_empty() {
                return Err(Error::new(
                    ErrorCode::ForeignFiles,
                    format!(
                        "{CAPTURES_WHAT} {} holds entries that are not the store's: {}",
                        captures.display(),
                        foreign.join(", ")
                    ),
                ));
            }
            for name in slots {
                let slot = SandboxStore::open(captures.join(name))?;
                let locked = lock_slot(&slot, deadline)?;
                published.extend(self.recover_slot(&locked, deadline, hooks)?);
            }
        }
        published.extend(self.recover_publications(deadline, hooks)?);
        published.sort_by_key(Generation::generation);
        Ok(published)
    }

    /// Returns the generation with which a creation serves the class `class` at the host's time
    /// `now`, if the store holds one: the latest of the class, of the key of the class's latest
    /// generation, that no tombstone quarantines, and that is eligible at `now`.
    fn existing(&self, class: ClassIdentity, now: &HostTime) -> Result<Option<Generation>> {
        let index = self.index()?;
        let Some(latest) = index
            .generations
            .iter()
            .rev()
            .find(|generation| generation.class == class)
        else {
            return Ok(None);
        };
        let key = latest.key;
        let tombstoned = self.tombstones()?;
        Ok(index
            .generations
            .iter()
            .rev()
            .find(|generation| {
                generation.class == class
                    && generation.key == key
                    && !tombstoned.contains(&generation.generation)
                    && generation.eligibility(now).is_eligible()
            })
            .cloned())
    }

    /// Creates the directory of the store's captures if needed, and the slot of the class
    /// `class` in it, and returns the slot's store.
    fn open_slot(&self, class: ClassIdentity) -> Result<SandboxStore> {
        let captures = self.captures_dir();
        create_private_dir(&captures, CAPTURES_WHAT)?;
        let slot = SandboxStore::create(captures.join(slot_name(class)))?;
        // A capture's VM listens in its transaction, the sandbox store from which it launches.
        let socket = slot.dir().join(TRANSACTION_NAME).join(SOCKET_NAME);
        if cfg!(target_os = "linux") && socket.as_os_str().len() >= SOCKET_PATH_LIMIT {
            return Err(Error::unsupported(format!(
                "the template store {} is too deep for the control socket of a capture's VMM, \
                 {}, whose path must be shorter than {SOCKET_PATH_LIMIT} bytes",
                self.dir.display(),
                socket.display()
            )));
        }
        Ok(slot)
    }

    /// Returns the path of the directory of the store's captures.
    fn captures_dir(&self) -> PathBuf {
        self.dir.join(CAPTURES_NAME)
    }

    /// Returns the error of a store that holds [`MAX_GENERATIONS`] generations, which takes no
    /// other.
    fn full(&self) -> Error {
        Error::unsupported(format!(
            "the template store {} holds {MAX_GENERATIONS} generations, the most that a store \
             holds: retire some before a capture publishes another",
            self.dir.display()
        ))
    }
}

/// Takes the lock of the slot `slot`, waiting for it until `deadline`, as another capture of its
/// class may hold it.
fn lock_slot(slot: &SandboxStore, deadline: Instant) -> Result<LockedStore> {
    slot.lock(Some(deadline)).map_err(|error| {
        if error.code() == ErrorCode::SandboxBusy {
            Error::new(
                ErrorKind::TimedOut,
                format!(
                    "another capture of the class of the slot {} is still in progress",
                    slot.dir().display()
                ),
            )
        } else {
            error
        }
    })
}

/// Returns the name of the slot of the class `class`: the first [`SLOT_NAME_LEN`] digits of its
/// identity.
fn slot_name(class: ClassIdentity) -> String {
    class.to_string()[..SLOT_NAME_LEN].to_owned()
}

/// Returns whether `name` is the name of a slot.
fn is_slot_name(name: &str) -> bool {
    name.len() == SLOT_NAME_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Creates the directory `path`, which `what` names, private to the current user, unless it
/// exists, which must be private already.
fn create_private_dir(path: &Path, what: &str) -> Result<()> {
    let problem = platform::create_owner_only_dir(path).map_err(store_error(format!(
        "cannot create {what} {}",
        path.display()
    )))?;
    refuse_problem(path, what, problem)
}

/// Creates the dormant scratch device's file at `path`, private to the current user: a sparse
/// file of `length` bytes, and returns its identity, as [`identity_of`] returns it.
fn create_scratch(path: &Path, length: u64) -> Result<(u64, u128)> {
    let failed = |error| {
        Error::io(format!(
            "cannot create the scratch of the capture {}",
            path.display()
        ))
        .with_source(error)
    };
    let file = create_new(path).map_err(failed)?;
    platform::make_sparse(&file).map_err(failed)?;
    file.set_len(length).map_err(failed)?;
    identity_of(&file).map_err(failed)
}

/// Checks the files of the capture of `template`, with the inputs `capture`, before its VM
/// starts: its transaction directory, its placeholder, and its scratch, which the capture
/// created with the identity `scratch`. Returns the placeholder that the check of its binding
/// opened, which the capture holds, for reading alone, until its VM has ended.
///
/// OpenVMM opens the block devices of a capture itself, at their paths, so the capture holds no
/// other handle of them as its VM starts, through which anything could write them.
fn check_files(
    capture: &Capture,
    template: &TemplatePlan,
    scratch: (u64, u128),
) -> Result<CheckedDistro> {
    let transaction = template.transaction();
    let problem = platform::check_owner_only_dir(transaction).map_err(store_error(format!(
        "cannot open {TRANSACTION_WHAT} {}",
        transaction.display()
    )))?;
    refuse_problem(transaction, TRANSACTION_WHAT, problem)?;
    let placeholder = check_placeholder(capture, template)?;
    check_scratch(template, scratch)?;
    Ok(placeholder)
}

/// Checks the placeholder of `template` against the binding of `capture`, as
/// [`DistroBinding::check`] does, and then at its path: a regular file, not a link, nor on
/// Windows any other reparse point, that is the file that the check found, that belongs to the
/// current user or to an administrator, and that no one else may change. Returns the file that
/// the binding's check opened.
fn check_placeholder(capture: &Capture, template: &TemplatePlan) -> Result<CheckedDistro> {
    let path = template.plan().distro().path();
    let refused = |problem: String| {
        Error::unavailable(format!("the placeholder {} {problem}", path.display()))
    };
    let text = path
        .to_str()
        .ok_or_else(|| refused("has a path that is not valid Unicode".to_owned()))?;
    let binding = &capture.binding;
    let layer = match binding.format() {
        DistroFormat::Erofs => SandboxLayer::new(LayerRole::Distro, text, binding.identity()),
        DistroFormat::Gpt => SandboxLayer::gpt(LayerRole::Distro, text, binding.identity()),
    };
    let checked = binding.check(&layer, capture.policy_revision)?;
    let file = open_regular(path).map_err(|error| refused(format!("cannot be used: {error}")))?;
    let identity =
        |file| identity_of(file).map_err(|error| refused(format!("cannot be identified: {error}")));
    if identity(&file)? != identity(checked.file())? {
        return Err(refused(
            "changed while its binding was checked: its path names another file".to_owned(),
        ));
    }
    if let Some(problem) = platform::foreign_writer_problem(&file)
        .map_err(|error| refused(format!("cannot be checked: {error}")))?
    {
        return Err(refused(problem));
    }
    Ok(checked)
}

/// Checks the scratch of `template` at its path, which the capture created with the identity
/// `created`: a regular file of the template's scratch length, not a link, nor on Windows any
/// other reparse point, that is the file that the capture created, and that no one else may
/// change. The check's file is closed as it returns.
fn check_scratch(template: &TemplatePlan, created: (u64, u128)) -> Result<()> {
    let path = template.scratch_path();
    let refused = |problem: String| {
        Error::io(format!(
            "the scratch of the capture {} {problem}",
            path.display()
        ))
    };
    let file = open_regular(path).map_err(|error| refused(format!("cannot be used: {error}")))?;
    let identity =
        identity_of(&file).map_err(|error| refused(format!("cannot be identified: {error}")))?;
    if identity != created {
        return Err(refused(
            "is not the file that the capture created".to_owned(),
        ));
    }
    let length = file
        .metadata()
        .map_err(|error| refused(format!("cannot be examined: {error}")))?
        .len();
    if length != template.scratch_length() {
        return Err(refused(format!(
            "is {length} bytes long, not the {} bytes of the template's scratch",
            template.scratch_length()
        )));
    }
    if let Some(problem) = platform::foreign_writer_problem(&file)
        .map_err(|error| refused(format!("cannot be checked: {error}")))?
    {
        return Err(refused(problem));
    }
    Ok(())
}

/// The outcome report of a VM, as OpenVMM writes it, of which a capture reads the outcome.
#[derive(Deserialize)]
struct OutcomeReport {
    /// The outcome of the VM.
    outcome: Outcome,
}

/// The outcome of a VM, as its outcome report records it.
#[derive(Deserialize)]
struct Outcome {
    /// The operation of the VM: `capture` for a VM that OpenVMM captures.
    operation: String,
    /// The category of the outcome: `success` for a capture that OpenVMM published.
    category: String,
    /// The status code of the outcome: 0 for a capture that OpenVMM published.
    status_code: i64,
}

/// Checks the source's exit through the outcome report of the capture's VM at `path`: that
/// OpenVMM wrote one, for a capture that succeeded, as it does once it has published the snapshot
/// and torn the VM down, and before it exits. The launch removed the report of the slot's previous
/// VM, as [`LockedStore::begin_launch`] does, and OpenVMM publishes its report without replacing
/// a file, so the report is the VM's own.
fn check_exit(path: &Path) -> Result<()> {
    let description = format!("the outcome report {}", path.display());
    let file = match open_regular(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(Error::exited(format!(
                "the VM of the capture ended without {description}"
            )));
        }
        Err(error) => {
            return Err(Error::io(format!("cannot read {description}")).with_source(error));
        }
    };
    let bytes = read_at_most(file, MAX_REPORT_LEN, "an outcome report")
        .map_err(|error| Error::io(format!("cannot read {description}")).with_source(error))?;
    let report: OutcomeReport = serde_json::from_slice(&bytes)
        .map_err(|error| Error::io(format!("{description} is malformed")).with_source(error))?;
    let Outcome {
        operation,
        category,
        status_code,
    } = report.outcome;
    if operation != "capture" || category != "success" || status_code != 0 {
        return Err(Error::exited(format!(
            "the VM of the capture ended without a capture: {description} records the outcome \
             {category} with status {status_code} of the operation {operation}"
        )));
    }
    Ok(())
}

/// Checks the snapshot at `snapshot` that OpenVMM published: a directory private to the current
/// user, as [`platform::check_snapshot_dir`] checks it, whose manifest
/// [`TemplateManifest::read`] reads, and whose artifacts are exactly its manifest, RAM, and saved
/// state, each a regular file private to the current user, of the length that the manifest
/// records. Reads no artifact but the manifest. Returns the manifest.
fn check_snapshot(snapshot: &Path) -> Result<TemplateManifest> {
    let what = "the snapshot of the capture";
    // A snapshot that is missing once its capture reported it is lost, not merely absent.
    let problem = platform::check_snapshot_dir(snapshot).map_err(|error| {
        Error::io(format!("cannot open {what} {}", snapshot.display())).with_source(error)
    })?;
    refuse_problem(snapshot, what, problem)?;
    // So is a manifest that is missing from the snapshot.
    let manifest = TemplateManifest::read(snapshot).map_err(|error| {
        if error.kind() != ErrorKind::NotFound {
            return error;
        }
        let (_, message, source) = error.into_parts();
        let error = Error::io(message);
        match source {
            Some(source) => error.with_source(source),
            None => error,
        }
    })?;
    let mut names = entries(snapshot)?;
    names.sort();
    if names != SNAPSHOT_ARTIFACTS {
        return Err(Error::io(format!(
            "{what} {} holds {}, not exactly the manifest, RAM, and saved state of a platform-tier \
             snapshot",
            snapshot.display(),
            names.join(", ")
        )));
    }
    for (name, length) in [
        (MANIFEST_NAME, None),
        (MEMORY_NAME, Some(manifest.memory_len())),
        (STATE_NAME, Some(manifest.state_len())),
    ] {
        let path = snapshot.join(name);
        let refused = |problem: String| {
            Error::io(format!(
                "the artifact {} of {what} {problem}",
                path.display()
            ))
        };
        let file =
            open_regular(&path).map_err(|error| refused(format!("cannot be used: {error}")))?;
        if let Some(problem) = platform::private_file_problem(&file)
            .map_err(|error| refused(format!("cannot be checked: {error}")))?
        {
            return Err(refused(problem));
        }
        let found = file
            .metadata()
            .map_err(|error| refused(format!("cannot be examined: {error}")))?
            .len();
        if let Some(length) = length
            && found != length
        {
            return Err(refused(format!(
                "is {found} bytes long, not the {length} bytes that its manifest records"
            )));
        }
    }
    Ok(manifest)
}

/// Removes from the capture's transaction `transaction` what the template does not keep: the lock
/// file of its sandbox store, which the capture closed once its VM had ended, the dormant scratch
/// device's file, the file that backed the VM's RAM, which OpenVMM removes as the VM ends unless
/// it ends abruptly, and directories in which OpenVMM staged a snapshot that it did not publish.
/// Fails unless the transaction then holds the record of the capture and its snapshot alone.
fn clean_transaction(transaction: &Path) -> Result<()> {
    for name in entries(transaction)? {
        let path = transaction.join(&name);
        if name == LOCK_NAME || name == SCRATCH_NAME || name.starts_with(MEMORY_FILE_PREFIX) {
            remove_if_present(&path)?;
        } else if name.starts_with(STAGING_PREFIX) {
            match fs::remove_dir_all(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(store_error(format!("cannot remove {}", path.display()))(
                        error,
                    ));
                }
                _ => {}
            }
        }
    }
    let mut names = entries(transaction)?;
    names.sort();
    if names != [CAPTURE_RECORD_NAME, SNAPSHOT_NAME] {
        return Err(Error::io(format!(
            "{TRANSACTION_WHAT} {} holds {}, not exactly the {RECORD_DESCRIPTION} and its \
             snapshot",
            transaction.display(),
            names.join(", ")
        )));
    }
    Ok(())
}

/// Returns whether the sandbox store of the capture's transaction `transaction` records a launch,
/// whose VMM may still run, or whether that cannot be told: whether the journal of a launch is
/// there, unless nothing is.
fn records_launch(transaction: &Path) -> bool {
    !matches!(
        fs::symlink_metadata(transaction.join(JOURNAL_NAME)),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    )
}

/// Ends the launch that the sandbox store of the capture's transaction `transaction` records, if
/// any, as [`LockedStore::recover`] ends it, waiting for the store's lock until `deadline`.
///
/// A transaction without the store's lock file records no launch: a capture creates the lock file
/// before it launches, and removes it once its VM has ended, before it checks the transaction.
/// Fails with [`ErrorCode::VmmIndeterminate`] if the recovery cannot prove that the VMM no longer
/// runs, and the store then keeps the launch.
fn end_launch(transaction: &Path, deadline: Instant) -> Result<()> {
    let lock = transaction.join(LOCK_NAME);
    match fs::symlink_metadata(&lock) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(store_error(format!("cannot open {}", lock.display()))(
                error,
            ));
        }
        Ok(_) => {}
    }
    let mut launch = SandboxStore::open(transaction)?.lock(Some(deadline))?;
    match launch.recover()? {
        Recovery::Indeterminate(reason) => Err(indeterminate(transaction, &reason)),
        _ => Ok(()),
    }
}

/// Moves the VMM's log and outcome report of the capture whose transaction is `transaction` to
/// its slot, `slot`, which keeps those of its class's latest capture, once the capture's launch
/// has ended. A file that the transaction lacks, such as one that this moved already, stays as it
/// is.
fn keep_diagnostics(transaction: &Path, slot: &Path) -> Result<()> {
    for name in [LOG_NAME, OUTCOME_NAME] {
        let from = transaction.join(name);
        match fs::rename(&from, slot.join(name)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(store_error(format!(
                    "cannot move {} to the slot {}",
                    from.display(),
                    slot.display()
                ))(error));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Removes the transaction `transaction` of a capture that did not check it, with everything in
/// it, once nothing of the capture runs, if it exists. One that is a link, or that is not private
/// to the current user, is refused, and kept.
fn retire(transaction: &Path) -> Result<()> {
    match platform::check_owner_only_dir(transaction) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        checked => {
            let problem = checked.map_err(store_error(format!(
                "cannot open {TRANSACTION_WHAT} {}",
                transaction.display()
            )))?;
            refuse_problem(transaction, TRANSACTION_WHAT, problem)?;
        }
    }
    match fs::remove_dir_all(transaction) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(store_error(format!(
            "cannot retire {TRANSACTION_WHAT} {}",
            transaction.display()
        ))(error)),
        _ => Ok(()),
    }
}

/// Renames the directory `from` to `to` unless `to` exists, which it leaves as it is, as
/// [`platform::rename_no_replace`] does. On Windows, which renames no directory while a file below
/// it is open, as a scanner may hold one, a refused rename is tried again until `deadline`.
fn rename_until(from: &Path, to: &Path, deadline: Instant) -> Result<()> {
    loop {
        match platform::rename_no_replace(from, to) {
            Ok(()) => return Ok(()),
            Err(error)
                if cfg!(windows)
                    && error.kind() == io::ErrorKind::PermissionDenied
                    && Instant::now() < deadline =>
            {
                thread::sleep(RENAME_RETRY_INTERVAL);
            }
            Err(error) => {
                return Err(store_error(format!(
                    "cannot publish {} as {}",
                    from.display(),
                    to.display()
                ))(error));
            }
        }
    }
}

/// Returns the store generation that follows `generation`.
fn following(generation: StoreGeneration) -> Result<StoreGeneration> {
    generation
        .get()
        .checked_add(1)
        .map(StoreGeneration::new)
        .ok_or_else(|| Error::unsupported("the template store has assigned every store generation"))
}

/// What a checked capture records of itself, which the index lists of its generation once a
/// publication gives it a store generation.
struct Envelope {
    /// The identity of the template's key.
    key: KeyIdentity,
    /// The identity of the capture's class.
    class: ClassIdentity,
    /// The capture's storage generation.
    storage: StorageGeneration,
    /// When OpenVMM created the snapshot.
    created_at: SystemTime,
    /// The time contract of the capture.
    time: TimeContract,
    /// The length of the saved state, in bytes.
    state_len: u64,
    /// The length of the RAM, in bytes.
    memory_len: u64,
}

impl Envelope {
    /// Returns the envelope of a capture of the class `class`, with the storage generation
    /// `storage`, whose key is `key`, and whose snapshot's manifest is `manifest`.
    fn new(
        key: &TemplateKey,
        class: ClassIdentity,
        storage: StorageGeneration,
        manifest: &TemplateManifest,
    ) -> Self {
        Self {
            key: key.identity(),
            class,
            storage,
            created_at: manifest.created_at(),
            time: manifest.time().clone(),
            state_len: manifest.state_len(),
            memory_len: manifest.memory_len(),
        }
    }

    /// Returns the generation of the envelope, as the store generation `generation`.
    fn generation(&self, generation: StoreGeneration) -> Generation {
        Generation {
            generation,
            key: self.key,
            class: self.class,
            storage: self.storage,
            created_at: self.created_at,
            time: self.time.clone(),
            state_len: self.state_len,
            memory_len: self.memory_len,
        }
    }
}

/// The record of a capture, as JSON, which the capture writes in its transaction before its VM
/// starts: the store, the class, and the storage generation of the capture.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CaptureRecord {
    /// [`STORE_FORMAT`].
    format: u32,
    /// The identity of the store.
    store: String,
    /// The identity of the capture's class.
    class: String,
    /// The capture's storage generation.
    storage_generation: String,
}

impl CaptureRecord {
    /// Returns the record of a capture of the class `class` in `store`, with the storage
    /// generation `storage`.
    fn new(store: &Store, class: ClassIdentity, storage: StorageGeneration) -> Self {
        Self {
            format: STORE_FORMAT,
            store: store.id.to_string(),
            class: class.to_string(),
            storage_generation: storage.to_string(),
        }
    }
}
