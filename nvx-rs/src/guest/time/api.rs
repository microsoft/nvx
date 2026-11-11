//! The time ABI's typed calls, through which a guest init or agent runs its steps in process.

#[cfg(target_arch = "x86_64")]
use std::ffi::CString;
use std::fmt::{self, Display};
use std::io;
#[cfg(target_arch = "x86_64")]
use std::net::IpAddr;
#[cfg(target_arch = "x86_64")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_arch = "x86_64")]
use std::path::PathBuf;

#[cfg(target_arch = "x86_64")]
use super::NSEC_PER_USEC;
use super::Phase;
#[cfg(target_arch = "x86_64")]
use super::checks;
use super::checks::Checks;
#[cfg(target_arch = "x86_64")]
use super::cpu_mask::CpuMask;
use super::daemon;
#[cfg(target_arch = "x86_64")]
use super::decoders::{self, PACKET_HEADER_SIZE, PacketError, PacketHeader, RestorePacket};
use super::events::{CheckId, ViolationCode};
#[cfg(target_arch = "aarch64")]
use super::state::TimeState;
#[cfg(target_arch = "x86_64")]
use super::state::{CheckState, CheckStatus, Discontinuity, DowntimeSource, TimeState};
use super::system::{
    self, PowerOffStatus, RealOps, Runtime, SpawnError, clock_ns, cpu_time_ns, current_frequency,
    fork_process, initial_sample, state_store,
};
#[cfg(target_arch = "x86_64")]
use super::system::{
    BestSample, PORTB_GENERATION_ID_AVAILABLE, PORTB_PACKET_AVAILABLE, PRIVATE_FILE_MODE,
    RESTORE_PATH, RESTORE_TEMP_PATH, SAMPLE_ATTEMPTS, SAMPLE_BOUND_NS, SELECT_GENERATION_ID,
    SELECT_PACKET, SampleError, SampleResult, SaveError, portb_read, state_apply, state_load,
};
use crate::guest::GenerationId;
#[cfg(target_arch = "x86_64")]
use crate::guest::reseed::Seed;
use crate::guest::sys::{self, Errno, ForkResult, kernel};
#[cfg(target_arch = "x86_64")]
use crate::guest::sys::{FileDescriptor, OpenFlags, Port, PortAccess, PortError};

/// The shared cold-boot step, supervised time daemon and VM generation identity.
/// On x86, the same API also prepares capture and repairs and finishes restores.
///
/// Calls need write access to `/run/nvx` and the ISA's hardware interface: x86's port permission
/// and MSR devices, or ARM's restricted platform device and architected EL0 counter.
/// Every transaction releases its authority before another process can be started.
/// [`Command`](crate::guest::spawn::Command) refuses to inherit retained authority.
/// No call waits for a child that it did not start.
///
/// A violation of the time ABI is reported by a violation event on the console, in the kernel
/// log, and on OpenVMM's debug console. In enforcing mode, the event stops the VM, so the call
/// does not return. In strict mode, which a caller selects with [`strict`](Self::strict), so
/// that it owns the VM's exit status and its shutdown, such as a guest init or agent that runs a
/// clone's capture and repair in its own process, the call fails with [`TimeError::Violation`]
/// instead, whose [`status`](Violation::status) is the one with which enforcing mode would have
/// stopped the VM, and never stops the VM. In report-only mode, the call fails with
/// [`TimeError::Violation`] when the violation keeps it from doing its work. Otherwise, it goes
/// on as `nvx-time` does, and the event is the only report of the violation, as each call tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeAbi {
    /// Whether violations are reported instead of stopping the VM.
    report_only: bool,
    /// Whether a violation that stops the VM in enforcing mode fails the call instead.
    strict: bool,
}

impl TimeAbi {
    /// Returns the time ABI in the mode that the kernel command line selects: report-only when
    /// it holds `nvx_time_abi=report-only`, and enforcing otherwise.
    pub fn new() -> Self {
        Self::with_report_only(cmdline_requests_report_only())
    }

    /// Returns the time ABI in report-only mode, whatever the kernel command line selects.
    pub fn report_only() -> Self {
        Self::with_report_only(true)
    }

    /// Constructs the mode already resolved by the command dispatcher.
    pub(crate) const fn with_report_only(report_only: bool) -> Self {
        Self {
            report_only,
            strict: false,
        }
    }

    /// Returns the time ABI in strict mode, unless it is in report-only mode, which it keeps: a
    /// violation that stops the VM in enforcing mode fails the call instead, after its event,
    /// with [`TimeError::Violation`], the call's first, so that the caller ends the VM, with the
    /// violation's [`status`](Violation::status). The command tools, such as `nvx-time`, keep
    /// enforcing mode.
    ///
    /// The time daemon that [`start_daemon`](Self::start_daemon) starts enforces the ABI even
    /// then, since no caller owns its failures, and so does the preparation of a capture that
    /// `nvx::guest::clone` runs, as `nvx-time pre-capture` does.
    pub fn strict(self) -> Self {
        Self {
            strict: !self.report_only,
            ..self
        }
    }

    /// Returns whether violations are reported instead of stopping the VM.
    pub fn is_report_only(&self) -> bool {
        self.report_only
    }

    /// Returns whether a violation that stops the VM in enforcing mode fails the call instead.
    pub fn is_strict(&self) -> bool {
        self.strict
    }

    /// Returns the runtime of one call.
    pub(crate) fn runtime(self) -> Runtime<'static> {
        let runtime = self.enforcing_runtime();
        if self.strict {
            runtime.strict()
        } else {
            runtime
        }
    }

    /// Returns the runtime of a call that enforces the ABI even in strict mode: in enforcing or
    /// report-only mode, as the kernel command line selects.
    fn enforcing_runtime(self) -> Runtime<'static> {
        Runtime::new(&RealOps, self.report_only)
    }

    /// Runs the cold-boot time step synchronously: takes the initial host time sample, steps
    /// the wall clock to it, holds the frequency correction that the host's clock correction
    /// asks for, and publishes the time state, which starts at generation zero.
    ///
    /// The sample uses the declared counter rate: x86's frequency MSR or ARM's a8/CNTFRQ binding.
    /// The hardware device must be available first. A sample that cannot be taken within
    /// its bound is a violation of check C12, which does not fail the call in report-only mode:
    /// the step then leaves the wall clock unsynchronized, which [`ColdBoot::synchronized`]
    /// tells. The step starts no process: [`start_daemon`](Self::start_daemon) starts the time
    /// daemon from its outcome, separately, which `nvx-time boot` does next.
    pub fn cold_boot(&self) -> Result<ColdBoot, TimeError> {
        let runtime = self.runtime();
        settled(&runtime, cold_boot(&runtime))
    }

    /// Starts the time daemon of `boot`, the outcome of [`cold_boot`](Self::cold_boot), in a
    /// child process, and returns the child's process ID.
    ///
    /// The child runs the deferred boot checks in the background, at idle priority, and then
    /// becomes the time daemon: it disciplines the wall clock, watches the kernel log for
    /// violations, and on x86-64 runs repair step 7 of the restores that
    /// `restore_finish` hands it. It runs in a session of its own, with
    /// its standard streams on the null device, until the VM stops or `SIGTERM` stops it, and
    /// waits only for the children that it starts itself. It takes the command name `nvx-time`,
    /// by which the calls of the time ABI recognize it. The caller supervises it and collects
    /// its status: no call of the time ABI waits for it. The child enforces the ABI even when it
    /// is [strict](Self::strict), since no caller owns its failures: a violation that it reports
    /// stops the VM, unless the ABI is report-only.
    ///
    /// The call returns when the child starts, so the boot checks' failures, which the child
    /// reports by their events, do not fail it.
    ///
    /// The child is a copy of the calling process, with the process's descriptors, so the
    /// caller starts it early, as an init does, before it opens descriptors that the daemon
    /// must not hold. The process must have no other thread, without which the copy could not
    /// run safely: in a process with more than one thread, such as one that runs an async
    /// runtime, the call fails with [`TimeError::Spawn`], whose error is `EBUSY`.
    pub fn start_daemon(&self, boot: ColdBoot) -> Result<u32, TimeError> {
        match fork_process() {
            Ok(ForkResult::Child) => {
                sys::reset_child_signal();
                daemon::boot_checker(&self.enforcing_runtime(), &boot)
            }
            Ok(ForkResult::Parent(child)) => Ok(child.raw().unsigned_abs()),
            #[cfg(target_arch = "x86_64")]
            Err(SpawnError::Ports(error)) => Err(error.into()),
            #[cfg(target_arch = "aarch64")]
            Err(SpawnError::Authority(error)) => Err(TimeError::PlatformRelease(error.into())),
            Err(SpawnError::Fork(error)) => Err(TimeError::Spawn(error.into())),
        }
    }

    /// Reads the VM generation ID, also used by x86-64's `capture`.
    pub fn generation_id(&self) -> Result<GenerationId, TimeError> {
        let runtime = self.runtime();
        settled(&runtime, read_generation_id(&runtime))
    }

    /// Requests a capture through the snapshot port, preparation step 5, and, in a VM that a
    /// snapshot restored, repairs the restore: reads and validates the restore packet, steps and
    /// slews the wall clock, hands over the packet's fresh entropy, and records the restore,
    /// repair steps 1 to 3.
    ///
    /// The request's mode selects how the entropy is handed over. In the file mode of
    /// [`CaptureRequest::new`], the call creates, or truncates, the entropy file before it
    /// requests the capture, writes the entropy to it, and removes it again when the VM
    /// continues without a restore. In the in-memory mode of [`CaptureRequest::in_memory`], it
    /// writes no file: the [`Restore`] holds the entropy, as an owned [`Seed`], until
    /// [`Restore::take_seed`] hands it over. A capture that succeeds ends the VM process that
    /// requested it, in which the call does not return. With [`CaptureRequest::finish`],
    /// [`Restore::finished`] tells whether the call finished the restore, and how it reports a
    /// failed handoff of repair step 7.
    #[cfg(target_arch = "x86_64")]
    pub fn capture(&self, request: CaptureRequest) -> Result<CaptureOutcome, TimeError> {
        let runtime = self.runtime();
        settled(&runtime, capture(&runtime, request))
    }

    /// Finishes a restore that [`capture`](Self::capture) left unfinished, after the caller
    /// activated its processors and memory and refreshed its entropy and identity: records the
    /// CPUs that it brought online, acknowledges it when asked, repair step 6, and hands repair
    /// step 7, the restore checks and the release of the stall detectors, to the time daemon.
    ///
    /// Without a running daemon, an enforcing or strict call reports a violation of check C10,
    /// and a report-only call runs step 7 itself, which needs a process that has no other
    /// thread. Step 7 forces an RCU grace period in a grandchild, which a child of the call waits
    /// for until the RCU stall timeout at the latest. The call waits for that child alone,
    /// whatever the process's disposition of `SIGCHLD`, so a release that outlives the timeout
    /// passes to init, or to the nearest subreaper, without the process's descriptors. The call
    /// returns when step 7 ends, which reports its failures by their events only, as
    /// `nvx-time restore-finish` does.
    #[cfg(target_arch = "x86_64")]
    pub fn restore_finish(&self, request: RestoreFinish) -> Result<(), TimeError> {
        let runtime = self.runtime();
        settled(&runtime, restore_finish(&runtime, request))
    }

    /// Prepares the guest for a capture, as `nvx-time pre-capture` does: waits until the boot
    /// checks have completed, for 30 seconds at most, since they start the time daemon that the
    /// capture checks require, runs the capture checks, saves the stall detectors' settings and
    /// suppresses the detectors, and records the capture checks.
    ///
    /// A capture check that fails reports a violation, which stops the VM in enforcing mode, and
    /// in strict mode too, in which the preparation enforces the ABI, as `nvx-time pre-capture`
    /// does; a report-only call goes on, as `nvx-time` does. A call that fails leaves the
    /// detectors as they were, and never touches those of a capture that is pending already,
    /// [`PreCaptureError::Pending`], unless it
    /// [holds their suppression](PreCaptureError::holds_suppression), which the caller then
    /// releases with [`cancel_capture`](Self::cancel_capture), as after a call that succeeded.
    /// Like the other calls, it fails with [`TimeError::PortRelease`], as
    /// [`PreCaptureError::PortRelease`], if the thread may still hold some of the permission for
    /// the ports.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn pre_capture(&self) -> Result<(), PreCaptureError> {
        released_preparation(
            pre_capture(&self.enforcing_runtime()),
            sys::check_released(),
        )
    }

    /// Restores the stall detectors' settings that [`pre_capture`](Self::pre_capture) saved, and
    /// removes their record, as `nvx-time cancel-capture` does, or fails with its diagnostic,
    /// such as `read /run/nvx/time-suppression: No such file or directory`. It takes no
    /// permission for the ports.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn cancel_capture(&self) -> Result<(), Vec<u8>> {
        system::suppression_restore(false)
    }
}

impl Default for TimeAbi {
    /// Returns [`TimeAbi::new`].
    fn default() -> Self {
        Self::new()
    }
}

/// Returns whether the kernel command line selects report-only mode: whether it holds the
/// `nvx_time_abi=report-only` token, among the tokens into which
/// [`crate::protocol::cmdline::init_tokens`] splits
/// it as the guest's init does.
fn cmdline_requests_report_only() -> bool {
    let text = match sys::read_file(kernel::COMMAND_LINE) {
        Ok(text) => text,
        Err(error) => {
            sys::write_standard_error(
                format!("nvx-time: read kernel command line: {error}; keeping enforcing mode\n")
                    .as_bytes(),
            );
            return false;
        }
    };
    super::sample::report_only(&text, false)
}

/// Ends a call with `result`, unless the calling thread may still hold port permission that a
/// failed revocation left, which the call reports instead.
fn released<T>(result: Result<T, TimeError>) -> Result<T, TimeError> {
    #[cfg(target_arch = "x86_64")]
    sys::check_released().map_err(TimeError::from)?;
    #[cfg(target_arch = "aarch64")]
    sys::check_released().map_err(|error| TimeError::PlatformRelease(error.into()))?;
    result
}

/// Ends a call of `runtime` with `result`, as `released` does, unless the call reported a
/// violation in strict mode, with which it fails instead: the first that it reported, with which
/// enforcing mode would have stopped the VM.
fn settled<T>(runtime: &Runtime<'_>, result: Result<T, TimeError>) -> Result<T, TimeError> {
    released(match runtime.take_violation() {
        Some(violation) => Err(TimeError::Violation(violation)),
        None => result,
    })
}

/// Ends a preparation of a capture with `result`, unless the calling thread may still hold port
/// permission, as `released` tells, which the preparation reports instead, with whether it holds
/// the stall detectors' suppression.
#[cfg(target_arch = "x86_64")]
fn released_preparation(
    result: Result<(), PreCaptureError>,
    released: Result<(), PortError>,
) -> Result<(), PreCaptureError> {
    let Err(error) = released else {
        return result;
    };
    let suppressed = result
        .as_ref()
        .map_or_else(PreCaptureError::holds_suppression, |()| true);
    Err(PreCaptureError::PortRelease {
        error: error.into(),
        suppressed,
    })
}

/// The outcome of the cold-boot time step, from which [`TimeAbi::start_daemon`] starts the time
/// daemon.
#[derive(Debug, PartialEq, Eq)]
pub struct ColdBoot {
    /// The monotonic time at which the step started.
    pub(crate) start_ns: i64,
    /// The CPU time that the step took.
    pub(crate) cpu_ns: i64,
    /// The monotonic time at which the step accepted the initial sample, if it did.
    pub(crate) accepted_ns: Option<i64>,
    /// The number of checks that failed: one when check C12 did.
    pub(crate) failures: usize,
}

impl ColdBoot {
    /// Returns whether the step accepted an initial sample and stepped the wall clock to it.
    pub fn synchronized(&self) -> bool {
        self.accepted_ns.is_some()
    }
}

/// The cold-boot time step of [`TimeAbi::cold_boot`].
fn cold_boot(runtime: &Runtime<'_>) -> Result<ColdBoot, TimeError> {
    let start = clock_ns(sys::Clock::Monotonic);
    let cpu_start = cpu_time_ns();
    runtime.set_generation(0);
    system::make_runtime_directories();
    let mut checks = Checks::empty(Phase::Boot);
    let initial = match initial_sample(runtime) {
        Ok(initial) => Some((initial, clock_ns(sys::Clock::Monotonic))),
        Err(detail) => {
            checks.fail(runtime, CheckId::C12, detail);
            // A strict call stops where enforcing mode stops the VM.
            if let Some(violation) = runtime.take_violation() {
                return Err(TimeError::Violation(violation));
            }
            None
        }
    };
    let mut state = TimeState::default();
    state.synchronized = initial.is_some();
    state.violations = checks.failures as u64;
    match &initial {
        Some((sample, _)) => state.record_sample(sample),
        None => state.frequency_ppb = super::sample::frequency_to_ppb(current_frequency(runtime)),
    }
    if let Err(error) = state_store(&state) {
        return Err(check_failed(
            runtime,
            Phase::Boot,
            CheckId::C10,
            format!("cannot publish the time state: {}", error.desc()),
        ));
    }
    Ok(ColdBoot {
        start_ns: start,
        cpu_ns: cpu_time_ns().wrapping_sub(cpu_start),
        accepted_ns: initial.map(|(_, accepted_ns)| accepted_ns),
        failures: checks.failures,
    })
}

/// The read of [`TimeAbi::generation_id`].
#[cfg(target_arch = "x86_64")]
fn read_generation_id(runtime: &Runtime<'_>) -> Result<GenerationId, TimeError> {
    let ports = runtime.open_ports()?;
    if ports.read_u8(Port::PortbStatus) & PORTB_GENERATION_ID_AVAILABLE == 0 {
        return Err(TimeError::GenerationIdUnavailable);
    }
    let mut id = [0_u8; GenerationId::LENGTH];
    {
        let _lock = system::lock_portb().map_err(|error| TimeError::PortLock(error.into()))?;
        ports.write_u8(Port::PortbStatus, SELECT_GENERATION_ID);
        portb_read(&ports, Port::PortbData, &mut id);
    }
    Ok(GenerationId::from_bytes(id))
}

/// Reads the same a6 generation record through the restricted ARM transport.
#[cfg(target_arch = "aarch64")]
fn read_generation_id(_runtime: &Runtime<'_>) -> Result<GenerationId, TimeError> {
    sys::platform::generation_id().map_err(|error| match error {
        Errno::EOPNOTSUPP => TimeError::GenerationIdUnavailable,
        error => TimeError::PlatformAccess(error.into()),
    })
}

/// Why [`TimeAbi::pre_capture`] failed.
#[derive(Debug)]
#[cfg(target_arch = "x86_64")]
pub(crate) enum PreCaptureError {
    /// The boot checks were still pending when the wait ended: a violation of check C10, which
    /// a report-only call reports with its event.
    BootChecks(TimeError),
    /// The capture checks could not read the CPU sets: a violation of check C6, which a
    /// report-only call reports with its event.
    CaptureChecks(TimeError),
    /// The stall detectors' settings are saved already, by a capture that is still pending.
    Pending,
    /// The stall detectors' settings could not be saved, or the detectors not suppressed, with
    /// this error. The call left the settings as they were.
    Suppression(Errno),
    /// The stall detectors could not be suppressed, with this error, nor the ones that the call
    /// suppressed restored, so it kept the record of their settings: the call
    /// [holds their suppression](PreCaptureError::holds_suppression).
    Unrestored(Errno),
    /// The thread may still hold some of the permission for the ports, a
    /// [`TimeError::PortRelease`], after which it must start neither a thread nor a process.
    PortRelease {
        /// The error.
        error: TimeError,
        /// Whether the call [holds the suppression](PreCaptureError::holds_suppression) of the
        /// stall detectors, as one that succeeded does.
        suppressed: bool,
    },
}

#[cfg(target_arch = "x86_64")]
impl PreCaptureError {
    /// Returns whether the call that failed holds the suppression of the stall detectors, as a
    /// call that succeeded does: their record stays, and the caller restores their settings with
    /// [`TimeAbi::cancel_capture`].
    pub(crate) fn holds_suppression(&self) -> bool {
        matches!(
            self,
            Self::Unrestored(_)
                | Self::PortRelease {
                    suppressed: true,
                    ..
                }
        )
    }
}

/// The preparation of [`TimeAbi::pre_capture`].
#[cfg(target_arch = "x86_64")]
fn pre_capture(runtime: &Runtime<'_>) -> Result<(), PreCaptureError> {
    let (state, completed) = checks::wait_for_checks(true).map_err(|error| {
        PreCaptureError::BootChecks(check_failed(
            runtime,
            Phase::Capture,
            CheckId::C10,
            format!("cannot read the time state: {error}"),
        ))
    })?;
    if !completed {
        return Err(PreCaptureError::BootChecks(check_failed(
            runtime,
            Phase::Capture,
            CheckId::C10,
            "the boot checks are still pending",
        )));
    }
    let start = clock_ns(sys::Clock::Monotonic);
    let cpu_start = cpu_time_ns();
    runtime.set_generation(state.generation as u32);
    let mut checks = Checks::load(Phase::Capture).map_err(|_| {
        PreCaptureError::CaptureChecks(check_failed(
            runtime,
            Phase::Capture,
            CheckId::C6,
            "cannot read the CPU sets",
        ))
    })?;
    checks::run_capture_checks(runtime, &mut checks);
    let elapsed = clock_ns(sys::Clock::Monotonic).wrapping_sub(start);
    let cpu = cpu_time_ns().wrapping_sub(cpu_start);
    match system::suppression_save() {
        Ok(()) => {}
        Err(SaveError::Undone(Errno::EEXIST)) => return Err(PreCaptureError::Pending),
        Err(SaveError::Undone(error)) => return Err(PreCaptureError::Suppression(error)),
        Err(SaveError::Kept(error)) => return Err(PreCaptureError::Unrestored(error)),
    }
    if let Err(error) = checks::record_check(
        runtime,
        &checks,
        elapsed / NSEC_PER_USEC,
        cpu / NSEC_PER_USEC,
        0,
        0,
    ) {
        checks.fail(
            runtime,
            CheckId::C10,
            format!("cannot record the capture check: {}", error.desc()),
        );
    }
    Ok(())
}

/// How a snapshot treats the guest's scratch disk, which the capture request selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg(target_arch = "x86_64")]
pub enum ScratchPolicy {
    /// The snapshot keeps no scratch disk: the capture happens before the scratch disk is
    /// mounted, and a restore attaches a new one.
    Fresh,
    /// The snapshot keeps the scratch disk's exact contents, which a restore attaches.
    Paired,
}

#[cfg(target_arch = "x86_64")]
impl ScratchPolicy {
    /// Returns the byte that requests the capture through the snapshot port.
    pub(crate) fn request(self) -> u8 {
        match self {
            Self::Fresh => 0,
            Self::Paired => 1,
        }
    }
}

/// A capture request of [`TimeAbi::capture`], in one of two modes, which differ in how a restore
/// hands over the fresh entropy of its restore packet: the file mode of [`new`](Self::new),
/// which writes it to a file, as `nvx-time capture` does, and the in-memory mode of
/// [`in_memory`](Self::in_memory), which writes no file, and returns it as the [`Restore`]'s
/// owned [`Seed`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(target_arch = "x86_64")]
pub struct CaptureRequest {
    /// The snapshot's scratch policy.
    scratch: ScratchPolicy,
    /// The VM generation ID before the capture, which a restore must replace.
    generation_id: GenerationId,
    /// How a restore hands over its fresh entropy.
    entropy: EntropyMode,
    /// Whether to finish a restore that has nothing to activate.
    finish: bool,
}

/// How a restore hands over the fresh entropy of its restore packet: the mode of a
/// [`CaptureRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(target_arch = "x86_64")]
enum EntropyMode {
    /// The file mode: the restore writes the entropy to the file at this path, which only its
    /// owner may read and write.
    File(PathBuf),
    /// The in-memory mode: the restore returns the entropy, as its owned seed.
    InMemory,
}

#[cfg(target_arch = "x86_64")]
impl CaptureRequest {
    /// Describes a capture in the file mode, with the `scratch` policy, by the VM process whose
    /// generation ID, from [`TimeAbi::generation_id`], is `generation_id`. A restore writes the
    /// 64 bytes of fresh entropy of its restore packet, which start with the new generation ID,
    /// to a file at `entropy_path` that only its owner may read and write.
    pub fn new(scratch: ScratchPolicy, generation_id: GenerationId, entropy_path: PathBuf) -> Self {
        Self {
            scratch,
            generation_id,
            entropy: EntropyMode::File(entropy_path),
            finish: false,
        }
    }

    /// Describes a capture in the in-memory mode, with the `scratch` policy, by the VM process
    /// whose generation ID, from [`TimeAbi::generation_id`], is `generation_id`. A restore
    /// writes no file: its [`Restore`] holds the 64 bytes of fresh entropy of its restore
    /// packet, which start with the new generation ID, as an owned [`Seed`], which
    /// [`Restore::take_seed`] hands over.
    pub fn in_memory(scratch: ScratchPolicy, generation_id: GenerationId) -> Self {
        Self {
            scratch,
            generation_id,
            entropy: EntropyMode::InMemory,
            finish: false,
        }
    }

    /// Also finishes a restore that has no processors or memory to activate, when the time
    /// daemon runs, as [`TimeAbi::restore_finish`] would: acknowledges it when the host gated
    /// its input, and hands repair step 7 to the daemon. The restore's entropy and identity
    /// then stay with the caller, who has no further step to run before the acknowledgement.
    /// Without a running daemon, or when the calling thread cannot take the permission for the
    /// acknowledgement, the restore stays unfinished, as [`Restore::finished`] tells.
    pub fn finish(mut self) -> Self {
        self.finish = true;
        self
    }
}

/// How a capture request ended, in the VM process in which it returned.
#[derive(Debug)]
#[cfg(target_arch = "x86_64")]
pub enum CaptureOutcome {
    /// The VM continued without a restore: no snapshot was taken, or the restore packet was of
    /// the recorded generation. The entropy file of the file mode is removed.
    Continued,
    /// A snapshot restored the VM, and the call repaired its time and handed over its entropy:
    /// to the entropy file in the file mode, and to the restore's seed in the in-memory mode.
    Restored(Restore),
}

/// A restore that a capture request repaired, as its restore packet describes it.
///
/// In the in-memory mode, it also holds the restore's fresh entropy, as an owned [`Seed`], until
/// [`take_seed`](Self::take_seed) hands it over, so it is neither `Clone` nor comparable: the
/// seed has one owner.
#[derive(Debug)]
#[cfg(target_arch = "x86_64")]
pub struct Restore {
    /// The packet's flags.
    flags: u8,
    /// The packet's online processor count, or zero for no processor target.
    online_vp_count: u8,
    /// The new VM generation ID.
    generation_id: GenerationId,
    /// The memory ranges to activate.
    memory_ranges: Vec<MemoryRange>,
    /// The network settings that the restore packet carries, if any.
    network: Option<RestoreNetwork>,
    /// The activation record that the restore packet ends with, if any, undecoded.
    activation: Option<Vec<u8>>,
    /// Whether the call finished the restore.
    finished: bool,
    /// The restore's fresh entropy, in the in-memory mode, until the caller takes it.
    seed: Option<Seed>,
}

#[cfg(target_arch = "x86_64")]
impl Restore {
    /// Returns whether the call finished the restore, as [`CaptureRequest::finish`] asked.
    /// Otherwise, the caller activates the restore's processors and memory, applies its network
    /// settings, refreshes its entropy and identity, and finishes it with
    /// [`TimeAbi::restore_finish`].
    ///
    /// A restore whose handoff of step 7 fails after its acknowledgement counts as finished,
    /// since finishing it again would acknowledge it twice, as `nvx-time capture --finish`
    /// does: in report-only mode, a violation of check C10 is the failure's only report.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Returns whether the host gates its input until the guest acknowledges the restore, which
    /// [`RestoreFinish::acknowledge`] does.
    pub fn acknowledgement_required(&self) -> bool {
        self.flags & decoders::PACKET_ACK_REQUIRED != 0
    }

    /// Returns the number of online processors that the restore requests, 1, 2, 4, or 8, or
    /// `None` for none.
    pub fn processor_target(&self) -> Option<u8> {
        (self.online_vp_count != 0).then_some(self.online_vp_count)
    }

    /// Returns whether the restore requests an explicit memory target, whose expansion ranges
    /// [`memory_ranges`](Self::memory_ranges) lists.
    pub fn memory_target(&self) -> bool {
        self.flags & decoders::PACKET_MEMORY_TARGET != 0
    }

    /// Returns the memory ranges that the guest adds and onlines, in order.
    pub fn memory_ranges(&self) -> &[MemoryRange] {
        &self.memory_ranges
    }

    /// Returns the new VM generation ID, with which the restore's fresh entropy starts.
    pub fn generation_id(&self) -> GenerationId {
        self.generation_id
    }

    /// Takes the restore's fresh entropy: 64 bytes that start with the new VM generation ID,
    /// which a capture in the in-memory mode of [`CaptureRequest::in_memory`] returns instead of
    /// writing them to a file, or `None` in the file mode, or once the seed was taken.
    ///
    /// The seed moves to the caller without a copy of its bytes, which it zeroes when it is
    /// dropped, so the caller drops it once it reseeded the kernel's random number generator
    /// with it, through [`reseed`](crate::guest::reseed::reseed).
    pub fn take_seed(&mut self) -> Option<Seed> {
        self.seed.take()
    }

    /// Returns the restore packet's flags, which `nvx-time capture` prints.
    pub(crate) fn flags(&self) -> u8 {
        self.flags
    }

    /// Returns the restore packet's online processor count, which `nvx-time capture` prints.
    pub(crate) fn online_vp_count(&self) -> u8 {
        self.online_vp_count
    }

    /// Returns the network settings that the restore packet carries: those of a restore that
    /// replaced the network policy of a snapshot without a tier, which the guest applies before it
    /// finishes the restore, or `None` for a restore that carries none.
    pub fn network(&self) -> Option<RestoreNetwork> {
        self.network
    }

    /// Returns the activation record that the restore packet ends with: the bytes, 1 to
    /// [`MAX_RECORD_BYTES`](crate::activation::MAX_RECORD_BYTES) of them, that the host handed a
    /// platform clone with its restore, which the time ABI does not decode, or `None` for a
    /// restore that handed over none. A clone's repair hands them to the repaired clone, whose
    /// activation decodes them with
    /// [`ActivationRecord::decode`](crate::activation::ActivationRecord::decode).
    pub fn activation_record(&self) -> Option<&[u8]> {
        self.activation.as_deref()
    }
}

/// The network settings of a restore that replaced the network policy of a snapshot without a
/// tier, which its restore packet carries: the DNS server of the new policy, and whether the guest
/// announces its listeners under it, as a cold boot under that policy learns them from the
/// `virtnet_dns=` and `virtnet_ingress=` tokens of its command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg(target_arch = "x86_64")]
pub struct RestoreNetwork {
    /// The DNS server, one of the gateway's addresses, or `None` when the policy lets the guest
    /// reach no DNS service of the gateway.
    dns_server: Option<IpAddr>,
    /// Whether the guest announces its listeners to the gateway, which publishes them.
    announce_listeners: bool,
}

#[cfg(target_arch = "x86_64")]
impl RestoreNetwork {
    /// Returns the network settings with `dns_server` and `announce_listeners`.
    pub(crate) const fn new(dns_server: Option<IpAddr>, announce_listeners: bool) -> Self {
        Self {
            dns_server,
            announce_listeners,
        }
    }

    /// Returns the DNS server that the guest's resolver names, or `None` for none.
    pub fn dns_server(&self) -> Option<IpAddr> {
        self.dns_server
    }

    /// Returns whether the guest announces its listeners to its IPv4 gateway, which publishes
    /// them.
    pub fn announces_listeners(&self) -> bool {
        self.announce_listeners
    }
}

/// A range of guest physical memory that a restore adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg(target_arch = "x86_64")]
pub struct MemoryRange {
    /// The range's first guest physical address.
    start: u64,
    /// The range's length in bytes.
    length: u64,
}

#[cfg(target_arch = "x86_64")]
impl MemoryRange {
    /// Returns the range's first guest physical address.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Returns the range's length in bytes, which is not zero.
    pub fn length(&self) -> u64 {
        self.length
    }
}

/// The flags with which a capture in the file mode creates the entropy file: write-only, created
/// or truncated, and never through a symbolic link.
#[cfg(target_arch = "x86_64")]
const ENTROPY_FLAGS: OpenFlags = OpenFlags::O_WRONLY
    .union(OpenFlags::O_CREAT)
    .union(OpenFlags::O_TRUNC)
    .union(OpenFlags::O_NOFOLLOW)
    .union(OpenFlags::O_CLOEXEC);

/// The capture request and repair of [`TimeAbi::capture`].
#[cfg(target_arch = "x86_64")]
fn capture(runtime: &Runtime<'_>, request: CaptureRequest) -> Result<CaptureOutcome, TimeError> {
    let CaptureRequest {
        scratch,
        generation_id,
        entropy,
        finish,
    } = request;
    let path = match entropy {
        EntropyMode::File(entropy_path) => {
            let Ok(path) = CString::new(entropy_path.as_os_str().as_bytes()) else {
                return Err(TimeError::EntropyFile {
                    path: entropy_path,
                    source: io::ErrorKind::InvalidInput.into(),
                });
            };
            Some((path, entropy_path))
        }
        EntropyMode::InMemory => None,
    };
    if state_load().is_err() {
        return Err(TimeError::StateUnavailable);
    }
    load_generation(runtime);
    // The port transaction of the capture request and of repair steps 1 and 2 starts before
    // the entropy file opens, so that a guest without the permission fails first.
    let ports = runtime.open_ports()?;
    let daemon_pid = if finish { checks::daemon_pid() } else { None };
    let entropy = match path {
        Some((path, entropy_path)) => match sys::open(&path, ENTROPY_FLAGS, PRIVATE_FILE_MODE) {
            Ok(file) => EntropySink::File { path, file },
            Err(error) => {
                return Err(TimeError::EntropyFile {
                    path: entropy_path,
                    source: error.into(),
                });
            }
        },
        None => EntropySink::Seed,
    };
    let _lock = match system::lock_portb() {
        Ok(lock) => lock,
        Err(error) => {
            entropy.discard();
            return Err(TimeError::PortLock(error.into()));
        }
    };
    ports.write_u8(Port::Snapshot, scratch.request());
    let start = clock_ns(sys::Clock::Monotonic);
    let read = (ports.read_u8(Port::PortbStatus) & PORTB_PACKET_AVAILABLE != 0)
        .then(|| read_packet(runtime, &ports, runtime.generation()));
    drop(ports);
    let Some(read) = read else {
        entropy.discard();
        return Ok(CaptureOutcome::Continued);
    };
    let target = RepairTarget {
        previous_id: generation_id,
        entropy,
        finish,
        daemon_pid,
    };
    repair_restore(runtime, read, target, start)
}

/// Loads the persisted generation, and the declared TSC rate that the boot checks recorded, with
/// which repair pairs its time sample without reading the frequency MSR, into the runtime.
#[cfg(target_arch = "x86_64")]
pub(crate) fn load_generation(runtime: &Runtime<'_>) {
    if let Ok(state) = state_load() {
        runtime.set_generation(state.generation as u32);
        runtime.set_tsc_hz(state.tsc_hz);
    }
}

/// Where a capture hands over a restore's fresh entropy, in its request's mode.
#[cfg(target_arch = "x86_64")]
enum EntropySink {
    /// The file mode's entropy file, which the capture created.
    File {
        /// The file's path.
        path: CString,
        /// The created file.
        file: FileDescriptor,
    },
    /// The in-memory mode's seed, which the restore returns.
    Seed,
}

#[cfg(target_arch = "x86_64")]
impl EntropySink {
    /// Closes and removes the file mode's entropy file, of a capture that restored no packet.
    fn discard(self) {
        if let Self::File { path, file } = self {
            let _ = file.close();
            let _ = sys::unlink(&path);
        }
    }

    /// Hands over `entropy`, a restore's: writes it to the entropy file and drops it, which
    /// zeroes it, or returns it, as the restore's seed. A failure to write or close the file is
    /// reported with the file's path.
    fn deliver(self, entropy: Seed) -> Result<Option<Seed>, Vec<u8>> {
        match self {
            Self::File { path, file } => {
                system::write_entropy(file, &path, entropy.bytes()).map(|()| None)
            }
            Self::Seed => Ok(Some(entropy)),
        }
    }
}

/// What repair step 2's port reads returned, which repair validates once their transaction
/// ended.
#[cfg(target_arch = "x86_64")]
enum PacketRead {
    /// The packet's header is invalid.
    InvalidHeader(PacketError),
    /// The packet is of the recorded generation: the VM continued without a restore.
    Stale,
    /// The packet's generation, which does not follow the recorded generation.
    Generation(u32),
    /// The packet, with the time sample of its generation that repair took right after it, in
    /// the same transaction, or why it could take none within the bound; or why the packet's
    /// body is invalid.
    Packet(Result<(RestorePacket, SampleRead), PacketError>),
}

/// The time sample that repair selected, or why it could select none.
#[cfg(target_arch = "x86_64")]
type SampleRead = Result<(SampleResult, BestSample), SampleError>;

/// Repair step 2's port reads, after the capture read the status, step 1: selects the restore
/// packet, and reads its header and, when the header is valid and follows the `recorded`
/// generation, its body, with four-byte reads, and then, for a valid packet, takes the time
/// sample of the packet's generation, to which repair steps the wall clock.
#[cfg(target_arch = "x86_64")]
fn read_packet(runtime: &Runtime<'_>, ports: &PortAccess<'_>, recorded: u32) -> PacketRead {
    ports.write_u8(Port::PortbStatus, SELECT_PACKET);
    let mut header = [0_u8; PACKET_HEADER_SIZE];
    portb_read(ports, Port::PortbData, &mut header);
    let header = match PacketHeader::parse(&header) {
        Ok(header) => header,
        Err(error) => return PacketRead::InvalidHeader(error),
    };
    if header.generation == recorded {
        return PacketRead::Stale;
    }
    if u64::from(header.generation) != u64::from(recorded) + 1 {
        return PacketRead::Generation(header.generation);
    }
    let mut body = vec![0_u8; header.body_size()];
    portb_read(ports, Port::PortbData, &mut body);
    // The activation record follows the body, which ends with its length, a multiple of four
    // bytes after the header, so its reads start on a four-byte boundary of the packet.
    let packet = RestorePacket::parse(header, &body, |size| {
        let mut record = vec![0_u8; size];
        portb_read(ports, Port::PortbData, &mut record);
        record
    });
    // The packet owns its entropy now, in the seed's allocation.
    sys::zeroize(&mut body);
    PacketRead::Packet(packet.map(|packet| {
        let sample = system::clock_rates(runtime).and_then(|rates| {
            system::select_sample(
                ports,
                runtime.ops,
                SAMPLE_ATTEMPTS,
                SAMPLE_BOUND_NS,
                header.generation,
                rates,
            )
        });
        (packet, sample)
    }))
}

/// What a restore transaction writes, and where.
#[cfg(target_arch = "x86_64")]
struct RepairTarget {
    /// The VM generation ID before the snapshot, which the packet must replace.
    previous_id: GenerationId,
    /// Where the restore's fresh entropy goes.
    entropy: EntropySink,
    /// Whether to hand repair steps 6 and 7 to the daemon.
    finish: bool,
    /// The daemon that would run step 7.
    daemon_pid: Option<sys::ProcessId>,
}

/// Repair steps 2 and 3, once the port transaction that read the packet and its time sample
/// ended: validates them, steps the clock to the sample and holds the frequency correction that
/// its host clock correction asks for, hands over the packet's entropy, and records the restore.
#[cfg(target_arch = "x86_64")]
fn repair_restore(
    runtime: &Runtime<'_>,
    read: PacketRead,
    target: RepairTarget,
    start_ns: i64,
) -> Result<CaptureOutcome, TimeError> {
    let packet = match read {
        PacketRead::InvalidHeader(error) => {
            return Err(repair_failed(
                runtime,
                ViolationCode::RepairPacket,
                error.to_string(),
            ));
        }
        PacketRead::Stale => {
            target.entropy.discard();
            return Ok(CaptureOutcome::Continued);
        }
        PacketRead::Generation(generation) => {
            return Err(repair_failed(
                runtime,
                ViolationCode::RepairGeneration,
                format!(
                    "restore packet generation {} does not follow recorded generation {}",
                    generation,
                    runtime.generation()
                ),
            ));
        }
        PacketRead::Packet(packet) => packet,
    };
    let (packet, sample) = packet
        .map_err(|error| repair_failed(runtime, ViolationCode::RepairPacket, error.to_string()))?;
    let RestorePacket {
        header,
        ranges,
        entropy,
        network,
        activation,
    } = packet;
    let generation_id = entropy.generation_id();
    if generation_id == target.previous_id {
        return Err(repair_failed(
            runtime,
            ViolationCode::RepairGeneration,
            "the VM generation ID did not change",
        ));
    }
    runtime.set_generation(header.generation);
    let sample = match sample {
        Ok((SampleResult::Accepted, sample)) => sample,
        Ok((SampleResult::Wide, sample)) => {
            return Err(repair_failed(
                runtime,
                ViolationCode::RepairSample,
                system::wide_sample(sample.epsilon),
            ));
        }
        Err(error @ SampleError::Generation { .. }) => {
            return Err(repair_failed(
                runtime,
                ViolationCode::RepairGeneration,
                error.to_string(),
            ));
        }
        Err(error) => {
            return Err(repair_failed(
                runtime,
                ViolationCode::RepairSample,
                error.to_string(),
            ));
        }
    };
    if let Err(error) = system::synchronize(runtime, &sample) {
        return Err(repair_failed(
            runtime,
            ViolationCode::RepairClock,
            format!("adjtimex: {}", error.desc()),
        ));
    }
    let step_realtime_ns = clock_ns(sys::Clock::Realtime);
    let elapsed_us = clock_ns(sys::Clock::Monotonic).wrapping_sub(start_ns) / NSEC_PER_USEC;
    let finishing = target.finish
        && target.daemon_pid.is_some()
        && header.online_vp_count == 0
        && header.range_count == 0
        && !header.memory_target()
        && !header.network_repair();
    let seed = match target.entropy.deliver(entropy) {
        Ok(seed) => seed,
        Err(detail) => return Err(repair_failed(runtime, ViolationCode::RepairPacket, detail)),
    };
    if let Err(error) = state_apply(|state| {
        apply_repair(state, &header, &sample, step_realtime_ns, elapsed_us);
    }) {
        return Err(repair_failed(
            runtime,
            ViolationCode::RepairClock,
            format!("record the restore: {}", error.desc()),
        ));
    }
    // Repair steps 6 and 7 on the readiness path, when the call finishes the restore. Without a
    // running daemon, or without the permission for the acknowledgement, the restore stays
    // unfinished, for the caller to finish.
    let daemon = if finishing {
        daemon::running_daemon(target.daemon_pid)
    } else {
        None
    };
    let finished = daemon.is_some_and(|daemon| {
        daemon::finish_restore(runtime, daemon, header.acknowledgement_required(), start_ns)
    });
    Ok(CaptureOutcome::Restored(Restore {
        flags: header.flags,
        online_vp_count: header.online_vp_count,
        generation_id,
        memory_ranges: ranges
            .iter()
            .map(|&(start, length)| MemoryRange { start, length })
            .collect(),
        network,
        activation,
        finished,
        seed,
    }))
}

/// Applies one successful restore packet, and the time sample to which repair stepped the wall
/// clock, to persistent state.
#[cfg(target_arch = "x86_64")]
fn apply_repair(
    state: &mut TimeState,
    packet: &PacketHeader,
    sample: &BestSample,
    step_realtime_ns: i64,
    elapsed_us: i64,
) {
    state.generation = u64::from(packet.generation);
    state.discontinuities += 1;
    state.last_discontinuity = Discontinuity::Restore;
    state.last_step_ns = sample.theta;
    state.last_step_realtime_ns = step_realtime_ns;
    state.last_downtime_ns = packet.downtime_ns;
    state.last_downtime_source = if packet.downtime_from_utc() {
        DowntimeSource::Utc
    } else {
        DowntimeSource::Monotonic
    };
    state.synchronized = true;
    state.record_sample(sample);
    // Repair step 3 marks this restore's check pending, so `nvx-time status` waits for the
    // daemon's step 7. Until then, `restore_elapsed_us` is the readiness path up to the clock
    // step, at `last_step_realtime_ns`; `restore_cpu_us` counts only step 7, which records it.
    let generation = state.generation;
    if let Some(restore) = state.check_mut(Phase::Restore) {
        *restore = CheckState {
            status: Some(CheckStatus::Pending),
            elapsed_us,
            generation,
            ..CheckState::default()
        };
    }
}

/// A set of the CPUs that the time ABI covers: CPUs 0 to 63.
///
/// Its `Display` form is a CPU list in the kernel's syntax, such as `1-3,6`, or `none` for the
/// empty set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg(target_arch = "x86_64")]
pub struct CpuSet(CpuMask);

#[cfg(target_arch = "x86_64")]
impl CpuSet {
    /// Returns the empty set.
    pub const fn new() -> Self {
        Self(CpuMask::EMPTY)
    }

    /// Returns the set with `cpu` added, or `None` for a CPU that the time ABI does not cover.
    #[must_use]
    pub fn with(self, cpu: usize) -> Option<Self> {
        (cpu < super::MAX_CPUS).then(|| Self(self.0.with(cpu)))
    }

    /// Returns whether the set has `cpu`.
    pub fn contains(&self, cpu: usize) -> bool {
        self.0.contains(cpu)
    }

    /// Returns the number of CPUs in the set.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether the set has no CPUs.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the set's CPUs in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + use<> {
        self.0.iter()
    }
}

#[cfg(target_arch = "x86_64")]
impl From<CpuMask> for CpuSet {
    /// Returns the set of the mask's CPUs.
    fn from(mask: CpuMask) -> Self {
        Self(mask)
    }
}

#[cfg(target_arch = "x86_64")]
impl Display for CpuSet {
    /// Writes the set as a CPU list in the kernel's syntax, or `none` for the empty set.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The finish of a restore, [`TimeAbi::restore_finish`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg(target_arch = "x86_64")]
pub struct RestoreFinish {
    /// Whether to acknowledge the restore through the snapshot port.
    acknowledge: bool,
    /// The CPUs that the restore brought online.
    new_cpus: CpuSet,
}

#[cfg(target_arch = "x86_64")]
impl RestoreFinish {
    /// Describes a finish that records no new CPUs and sends no acknowledgement.
    pub fn new() -> Self {
        Self::default()
    }

    /// Acknowledges the restore through the snapshot port, which a restore whose host gates its
    /// input until then needs.
    pub fn acknowledge(mut self) -> Self {
        self.acknowledge = true;
        self
    }

    /// Records `cpus` as the CPUs that the restore brought online, which repair step 7's
    /// restore checks check too.
    pub fn new_cpus(mut self, cpus: CpuSet) -> Self {
        self.new_cpus = cpus;
        self
    }
}

/// The finish of [`TimeAbi::restore_finish`].
#[cfg(target_arch = "x86_64")]
fn restore_finish(runtime: &Runtime<'_>, request: RestoreFinish) -> Result<(), TimeError> {
    let RestoreFinish {
        acknowledge,
        new_cpus: CpuSet(new_cpus),
    } = request;
    let Ok(state) = state_load() else {
        return Err(repair_failed(
            runtime,
            ViolationCode::RepairClock,
            "the time state is unreadable",
        ));
    };
    runtime.set_generation(state.generation as u32);
    let record = format!("generation={}\nnew_cpus={new_cpus}\n", state.generation);
    if let Err(error) = system::replace_file(RESTORE_PATH, RESTORE_TEMP_PATH, record.as_bytes()) {
        return Err(repair_failed(
            runtime,
            ViolationCode::RepairClock,
            format!("record the restore: {}", error.desc()),
        ));
    }
    let ports = if acknowledge {
        Some(runtime.open_ports()?)
    } else {
        None
    };
    let start = clock_ns(sys::Clock::Monotonic)
        .wrapping_sub(clock_ns(sys::Clock::Realtime).wrapping_sub(state.last_step_realtime_ns))
        .wrapping_sub(
            state
                .check(Phase::Restore)
                .map_or(0, |restore| restore.elapsed_us)
                * NSEC_PER_USEC,
        );
    let daemon = daemon::running_daemon(checks::daemon_pid());
    if daemon.is_none() && !runtime.report_only() {
        // The failure's event writes the debug console in a transaction of its own.
        drop(ports);
        return Err(check_failed(
            runtime,
            Phase::Restore,
            CheckId::C10,
            "the time daemon is not running",
        ));
    }
    if let Some(ports) = ports {
        daemon::acknowledge_restore(ports);
    }
    let Some(daemon) = daemon else {
        let timing = daemon::inline_restore_timing(start);
        let _ = daemon::restore_work(runtime, new_cpus, true, &timing);
        return Ok(());
    };
    daemon::queue_restore(daemon, start).map_err(|_| {
        check_failed(
            runtime,
            Phase::Restore,
            CheckId::C10,
            "the time daemon is not running",
        )
    })
}

/// Reports that restore-time repair failed, which stops the VM unless report-only or strict,
/// and returns the violation.
#[cfg(target_arch = "x86_64")]
fn repair_failed(
    runtime: &Runtime<'_>,
    code: ViolationCode,
    detail: impl AsRef<[u8]>,
) -> TimeError {
    runtime.repair_failure(code, detail.as_ref());
    TimeError::Violation(Violation::new(
        PowerOffStatus::Repair,
        code,
        detail.as_ref(),
    ))
}

/// Reports that check `id` failed in `phase`, which stops the VM unless report-only or strict,
/// and returns the violation.
fn check_failed(
    runtime: &Runtime<'_>,
    phase: Phase,
    id: CheckId,
    detail: impl AsRef<[u8]>,
) -> TimeError {
    Checks::empty(phase).fail(runtime, id, detail.as_ref());
    TimeError::Violation(Violation::new(
        PowerOffStatus::Conformance,
        ViolationCode::Conformance(id),
        detail.as_ref(),
    ))
}

/// A violation of the time ABI, which a violation event reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The status with which enforcing mode stops the VM for the violation.
    status: PowerOffStatus,
    /// The ABI's code of the violation.
    code: String,
    /// The event's detail.
    detail: String,
}

impl Violation {
    /// Returns the violation of `code`, with `detail`, for which enforcing mode stops the VM
    /// with `status`.
    pub(crate) fn new(status: PowerOffStatus, code: ViolationCode, detail: &[u8]) -> Self {
        Self {
            status,
            code: code.to_string(),
            detail: String::from_utf8_lossy(detail).into_owned(),
        }
    }

    /// Returns the exit status with which enforcing mode stops the VM for the violation, and
    /// with which a [strict](TimeAbi::strict) caller ends it: 193,
    /// [`TIME_CONFORMANCE`](crate::protocol::status::TIME_CONFORMANCE), for a conformance check
    /// that failed, or 195, [`TIME_REPAIR`](crate::protocol::status::TIME_REPAIR), for the
    /// repair of a restore that failed. Only the time daemon stops the VM with 194,
    /// [`TIME_VIOLATION`](crate::protocol::status::TIME_VIOLATION).
    pub fn status(&self) -> u8 {
        self.status.code()
    }

    /// Returns the time ABI's code of the violation, such as `G_REPAIR_PACKET`.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the event's detail, such as `restore packet version is 3`.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl Display for Violation {
    /// Writes the code and the detail, such as `G_REPAIR_PACKET: restore packet version is 3`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

/// Why a call of the time ABI failed.
///
/// Its `Display` form is a one-line diagnostic, such as `ioperm: Operation not permitted`.
#[derive(Debug)]
#[non_exhaustive]
pub enum TimeError {
    /// The calling thread could not take the permission for OpenVMM's ports, which needs
    /// `CAP_SYS_RAWIO`.
    #[cfg(target_arch = "x86_64")]
    PortAccess(io::Error),
    /// The kernel did not revoke the calling thread's permission for OpenVMM's ports. The
    /// thread may still hold some of it, and must start neither a thread nor a process, which
    /// [`Command`](crate::guest::spawn::Command) refuses to start from it.
    #[cfg(target_arch = "x86_64")]
    PortRelease(io::Error),
    /// The lock that serializes the transactions of port B's window failed.
    #[cfg(target_arch = "x86_64")]
    PortLock(io::Error),
    /// The restricted ARM platform transaction could not start or complete.
    #[cfg(target_arch = "aarch64")]
    PlatformAccess(io::Error),
    /// The thread may retain platform authority and must not start another process.
    #[cfg(target_arch = "aarch64")]
    PlatformRelease(io::Error),
    /// The time state, which the cold-boot time step publishes, is unavailable.
    StateUnavailable,
    /// The entropy file could not be created.
    #[cfg(target_arch = "x86_64")]
    EntropyFile {
        /// The file's path.
        path: PathBuf,
        /// Why the file could not be created.
        source: io::Error,
    },
    /// Port B offers no VM generation ID.
    GenerationIdUnavailable,
    /// The time daemon's process could not be started.
    Spawn(io::Error),
    /// The guest violates the time ABI, which a report-only or [strict](TimeAbi::strict) call
    /// reports with a violation event before it fails. Its
    /// [`status`](Violation::status) is the one with which enforcing mode stops the VM.
    Violation(Violation),
}

#[cfg(target_arch = "x86_64")]
impl From<PortError> for TimeError {
    /// Returns the error of a port transaction that could not start or end.
    fn from(error: PortError) -> Self {
        match error {
            PortError::Busy => Self::PortAccess(Errno::EBUSY.into()),
            PortError::Grant(error) => Self::PortAccess(error.into()),
            PortError::Revoke(error) => Self::PortRelease(error.into()),
        }
    }
}

impl Display for TimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(target_arch = "x86_64")]
            Self::PortAccess(error) => write!(formatter, "ioperm: {}", sys::describe(error)),
            #[cfg(target_arch = "x86_64")]
            Self::PortRelease(error) => {
                write!(formatter, "ioperm revoke: {}", sys::describe(error))
            }
            #[cfg(target_arch = "x86_64")]
            Self::PortLock(error) => write!(formatter, "portb lock: {}", sys::describe(error)),
            #[cfg(target_arch = "aarch64")]
            Self::PlatformAccess(error) => write!(formatter, "platform: {}", sys::describe(error)),
            #[cfg(target_arch = "aarch64")]
            Self::PlatformRelease(error) => {
                write!(formatter, "platform release: {}", sys::describe(error))
            }
            Self::StateUnavailable => formatter.write_str("the time state is unavailable"),
            #[cfg(target_arch = "x86_64")]
            Self::EntropyFile { path, source } => write!(
                formatter,
                "open {}: {}",
                path.display(),
                sys::describe(source)
            ),
            Self::GenerationIdUnavailable => {
                formatter.write_str("the VM generation ID is unavailable")
            }
            Self::Spawn(error) => write!(formatter, "fork: {}", sys::describe(error)),
            Self::Violation(violation) => violation.fmt(formatter),
        }
    }
}

impl std::error::Error for TimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(target_arch = "x86_64")]
            Self::PortAccess(error) | Self::PortRelease(error) | Self::PortLock(error) => {
                Some(error)
            }
            Self::Spawn(error) => Some(error),
            #[cfg(target_arch = "x86_64")]
            Self::EntropyFile { source: error, .. } => Some(error),
            #[cfg(target_arch = "aarch64")]
            Self::PlatformAccess(error) | Self::PlatformRelease(error) => Some(error),
            Self::StateUnavailable | Self::GenerationIdUnavailable | Self::Violation(_) => None,
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests;
