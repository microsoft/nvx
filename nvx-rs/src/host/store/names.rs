//! The version of the store's records, and the names of the files that the store keeps in a
//! sandbox's directory.

/// Version of the store's records.
///
/// It follows the versions of the runtime state that the CLI kept on its own, 1, and that
/// [`instance`] kept, up to 3, before both moved to the store, so that a record of theirs fails as
/// unsupported rather than being read as one of the store's.
///
/// [`instance`]: crate::host::instance
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
/// removes once its VM has exited. A [removal] of the sandbox removes it first, and makes its
/// absence durable before the sandbox goes.
///
/// [removal]: crate::host::store#removal
pub const CLONE_BINDING_NAME: &str = "clone.json";
/// Name of the file of the `scratch` block device that earlier versions of this crate created for
/// a clone whose upper is in RAM, a sparse file of the template's scratch length. Such a clone now
/// restores with its scratch absent, and nothing creates the file, but a sandbox's directory may
/// still hold one that an earlier version left, which
/// [`template::Store::release`](crate::host::template::Store::release) removes once the clone's VM
/// has exited, as a [removal] of the sandbox does.
///
/// [removal]: crate::host::store#removal
pub const CLONE_SCRATCH_NAME: &str = "clone-scratch.raw";
/// Name of the activation record of a clone, in its canonical encoding, which the clone's launch,
/// [`template::ClonePlan::launch`](crate::host::template::ClonePlan::launch), writes before it
/// spawns the clone's VMM, so that the restore activates the clone with that record and its
/// transaction whatever happens to the launch's caller, and which
/// [`template::Store::release`](crate::host::template::Store::release) removes once the clone's VM
/// has exited, as a [removal] of the sandbox does.
///
/// [removal]: crate::host::store#removal
pub const CLONE_ACTIVATION_NAME: &str = "clone-activation.bin";
/// Name of the Linux socket at which the launch of a clone,
/// [`template::ClonePlan::launch`](crate::host::template::ClonePlan::launch), listens for the
/// restore-ready event of its VMM, which OpenVMM sends once it has prepared the restore, before
/// the restored VM runs; on Windows, the endpoint is a named pipe, whose name each launch draws at
/// random. [`template::Store::release`](crate::host::template::Store::release) removes it once the
/// clone's VM has exited, as a [removal] of the sandbox does.
///
/// [removal]: crate::host::store#removal
pub const RESTORE_READY_NAME: &str = "restore.sock";
/// Name of the progress log of a clone's launch, which the launch,
/// [`template::ClonePlan::launch`](crate::host::template::ClonePlan::launch), creates, and to which
/// it and the clone's restore sequence append a line for each step that the clone reaches, without
/// syncing it, so that a reattach tells how far the clone got once the caller of its launch
/// crashed, and which [`template::Store::release`](crate::host::template::Store::release) removes,
/// as a [removal] of the sandbox does. No other launch writes it.
///
/// [removal]: crate::host::store#removal
pub const CLONE_PROGRESS_NAME: &str = "clone-progress.log";
/// Name of the record of a clone's failure, which a failure of the clone from its launch on
/// writes before its teardown, and again once its teardown is known, as
/// [`template::CloneFailure`](crate::host::template::CloneFailure) describes, and which
/// [`template::Store::release`](crate::host::template::Store::release) removes, as a [removal] of
/// the sandbox does.
///
/// [removal]: crate::host::store#removal
pub const CLONE_FAILURE_NAME: &str = "clone-failure.json";
/// The files that the clone of a sandbox keeps of its launch, and the file of the scratch device
/// that an earlier version created for a clone in RAM, which a release removes once the clone's VM
/// has exited, as a [removal] of the sandbox does, before the clone binding.
///
/// [removal]: crate::host::store#removal
pub(super) const CLONE_FILE_NAMES: [&str; 5] = [
    CLONE_SCRATCH_NAME,
    CLONE_ACTIVATION_NAME,
    RESTORE_READY_NAME,
    CLONE_PROGRESS_NAME,
    CLONE_FAILURE_NAME,
];
/// Every file that the store keeps in a sandbox's directory. Replacing a record leaves a
/// temporary file named `.NAME.NUMBER.tmp` beside it only if the replacement was interrupted, and
/// a VMM that is killed while it writes its outcome report leaves the file in which it stages it;
/// see [the report staging file].
///
/// [the report staging file]: crate::host::store#the-report-staging-file
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
