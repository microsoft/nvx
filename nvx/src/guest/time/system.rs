//! The time tools' runtime: persistent state, violation events, port-B transactions, and the
//! wall clock's discipline, over the system layer's owners.

use std::ffi::CStr;
#[cfg(target_arch = "x86_64")]
use std::ffi::CString;
use std::fmt::{self, Display};
#[cfg(target_arch = "x86_64")]
use std::num::NonZeroU64;
#[cfg(target_arch = "aarch64")]
use std::sync::OnceLock;
#[cfg(target_arch = "x86_64")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use super::Phase;
#[cfg(target_arch = "x86_64")]
use super::decoders;
use super::discipline;
pub(crate) use super::events::PowerOffStatus;
use super::events::{EventSource, Violation, ViolationCode, format_event};
pub(crate) use super::model::BestSample;
use super::model::CounterRates;
use super::state::{SampleRejection, StateReport};
use super::wire::{self, GuestReads, SAMPLE_SIZE, TimeSample, TimeSampleError};
#[cfg(target_arch = "aarch64")]
use crate::guest::counter::ClockContext;
#[cfg(target_arch = "aarch64")]
use crate::guest::counter::ClockError;
use crate::guest::counter::CounterScale;
#[cfg(target_arch = "x86_64")]
use crate::guest::sys::x86::{self, msr};
use crate::guest::sys::{
    self, ClockAdjustment, ClockDiscipline, CreatedFile, Errno, FileLock, FinishTimeAccess,
    ForkResult, FrequencyHold, Mode, OpenFlags, TimeAccess, TimeAccessError, TimeChannel,
    TimeValLike, TimeWindow, kernel,
};
#[cfg(target_arch = "x86_64")]
use crate::guest::sys::{FileDescriptor, MsrAccess, MsrFile, Port, PortAccess, PortError};

#[cfg(target_arch = "aarch64")]
mod aarch64;

/// Selector requesting the restore packet.
#[cfg(target_arch = "x86_64")]
pub(crate) const SELECT_PACKET: u8 = 0xa5;
/// Selector requesting the VM generation identifier.
pub(crate) const SELECT_GENERATION_ID: u8 = 0xa6;
/// Selector requesting a host time sample.
const SELECT_TIME_SAMPLE: u8 = 0xa7;
/// Status bit indicating that a restore packet can be read.
#[cfg(target_arch = "x86_64")]
pub(crate) const PORTB_PACKET_AVAILABLE: u8 = 0x02;
/// Status bit indicating that a generation identifier can be read.
pub(crate) const PORTB_GENERATION_ID_AVAILABLE: u8 = 0x20;
/// Status bit indicating that the time-sample window is supported.
pub(crate) const PORTB_TIME_WINDOW: u8 = 0x40;
/// Snapshot-port value acknowledging completed restore repair.
#[cfg(target_arch = "x86_64")]
pub(crate) const SNAPSHOT_ACKNOWLEDGE: u8 = 2;

/// Top-level NVX runtime directory, which also holds the time tools' locks and private records.
const RUN_DIR: &CStr = c"/run/nvx";
/// Time-status directory, which holds only what any process may read: the published state and
/// the daemon's process identifier, and their temporary files. A runtime may bind it into a
/// workload's root, where even a workload of user ID 0 must reach no lock that it could hold,
/// so the locks and private records stay in [`RUN_DIR`].
const TIME_DIR: &CStr = c"/run/nvx/time";
/// Published time state file.
const STATE_PATH: &CStr = c"/run/nvx/time/state";
/// Temporary state file atomically renamed into place.
const STATE_TEMP_PATH: &CStr = c"/run/nvx/time/.state";
/// Advisory lock protecting state updates.
const STATE_LOCK_PATH: &CStr = c"/run/nvx/time-state.lock";
/// Published time-daemon process identifier.
pub(crate) const DAEMON_PID_PATH: &CStr = c"/run/nvx/time/daemon.pid";
/// Temporary daemon-PID file.
pub(crate) const DAEMON_PID_TEMP_PATH: &CStr = c"/run/nvx/time/daemon.pid.tmp";
/// The time daemon's command name, which it takes whichever program started it, and by which
/// the published process identifier is recognized.
pub(crate) const DAEMON_NAME: &CStr = c"nvx-time";
/// Saved stall-detector settings during capture/restore.
#[cfg(target_arch = "x86_64")]
const SUPPRESSION_PATH: &CStr = c"/run/nvx/time-suppression";
/// Advisory lock protecting suppression state.
#[cfg(target_arch = "x86_64")]
const SUPPRESSION_LOCK_PATH: &CStr = c"/run/nvx/time-suppression.lock";
/// Private restore record consumed by the daemon.
#[cfg(target_arch = "x86_64")]
pub(crate) const RESTORE_PATH: &CStr = c"/run/nvx/time-restore";
/// Temporary restore-record file.
#[cfg(target_arch = "x86_64")]
pub(crate) const RESTORE_TEMP_PATH: &CStr = c"/run/nvx/time-restore.tmp";
/// Advisory lock serializing port-window transactions.
pub(crate) const PORTB_LOCK_PATH: &CStr = c"/run/nvx/portb.lock";
/// Temporary mount point used to force an RCU grace period.
#[cfg(target_arch = "x86_64")]
pub(crate) const RCU_SYNC_PATH: &CStr = c"/run/nvx/rcu-sync";

/// Mode of the runtime directories: `rwxr-xr-x`.
const DIRECTORY_MODE: Mode = Mode::S_IRWXU
    .union(Mode::S_IRGRP)
    .union(Mode::S_IXGRP)
    .union(Mode::S_IROTH)
    .union(Mode::S_IXOTH);
/// Mode of the files that other users may read, such as the state file: `rw-r--r--`.
const PUBLIC_FILE_MODE: Mode = Mode::S_IRUSR
    .union(Mode::S_IWUSR)
    .union(Mode::S_IRGRP)
    .union(Mode::S_IROTH);
/// Mode of the lock files, the suppression record, and the restored entropy: `rw-------`.
pub(crate) const PRIVATE_FILE_MODE: Mode = Mode::S_IRUSR.union(Mode::S_IWUSR);

/// Longest prefix of a path that a restore diagnostic quotes, as C's `%.200s` did.
#[cfg(target_arch = "x86_64")]
const QUOTED_PATH_LIMIT: usize = 200;

/// The stall detectors that capture suppresses, and the value that suppresses each: RCU's stall
/// warnings, the soft-lockup detector, and the hung-task detector. Only the first must exist.
#[cfg(target_arch = "x86_64")]
const SUPPRESSED_DETECTORS: [(&CStr, i64); 3] = [
    (kernel::RCU_STALL_SUPPRESS, 1),
    (kernel::SOFT_WATCHDOG, 0),
    (kernel::HUNG_TASK_TIMEOUT, 0),
];

/// The priority prefix of an event that the program writes to the kernel log: `KERN_CRIT`.
const KERNEL_LOG_PRIORITY: &[u8] = b"<2>";
/// The priority prefix of a diagnostic that the program writes to the kernel log: `KERN_ERR`.
const DIAGNOSTIC_PRIORITY: &[u8] = b"<3>";

/// Number of host samples that boot, restore repair, and each discipline poll take at most.
pub(crate) const SAMPLE_ATTEMPTS: usize = 3;
/// The largest uncertainty of a time sample that the guest accepts, in nanoseconds: the host
/// pairs its UTC with the guest TSC within 100 µs.
pub(crate) const SAMPLE_BOUND_NS: i64 = 100_000;

/// Seconds after which RCU reports a CPU stall in NVX's guest kernels, which the checks require
/// and the daemon assumes when it cannot read the setting.
pub(crate) const DEFAULT_STALL_TIMEOUT_S: i64 = 21;

#[cfg(target_arch = "x86_64")]
/// Preserves the x86 MSR/rate diagnostic while admitting only a usable TSC arithmetic scale.
fn checked_x86_scale(read: Result<u64, Errno>) -> Result<CounterScale, SampleError> {
    if let Ok(rate) = read
        && let Ok(scale) = CounterScale::x86_counter(rate)
    {
        return Ok(scale);
    }
    Err(SampleError::TscRate(read))
}

/// The privileged operations of the time tools, and the clock reads with which the guest pairs a
/// time sample, which a fake replaces in tests, so that their retry and failure paths are
/// testable.
pub(crate) trait PrivilegedOps: Sync {
    /// Reads the actual guest counter after the instructions before it.
    fn read_counter(&self) -> u64;
    /// Independently admits the counter's actual arithmetic scale, not a published report's rate.
    fn counter_scale(&self, window_lock: Option<&FileLock>) -> Result<CounterScale, SampleError>;
    #[cfg(target_arch = "aarch64")]
    /// Reads the complete ARM binding without deriving admission from published status.
    fn clock_context(&self, window_lock: Option<&FileLock>) -> Result<ClockContext, SampleError>;
    /// Reads the realtime clock, in nanoseconds since the Unix epoch.
    fn realtime_ns(&self) -> i64;
    /// Takes the calling thread's permission for the time ABI's ports, for one transaction,
    /// which [`PortAccess::release`] ends.
    #[cfg(target_arch = "x86_64")]
    fn open_ports(&self) -> Result<PortAccess<'_>, PortError>;
    #[cfg(target_arch = "aarch64")]
    /// Opens the restricted platform descriptor for a bounded transaction.
    fn open_time_window(&self) -> Result<TimeAccess<'_>, TimeAccessError>;
    /// Opens the MSR device of `cpu`.
    #[cfg(target_arch = "x86_64")]
    fn open_msr(&self, cpu: usize, access: MsrAccess) -> Result<MsrDescriptor, Errno>;
    /// Reads the register `msr`.
    #[cfg(target_arch = "x86_64")]
    fn read_msr(&self, descriptor: &MsrDescriptor, msr: u32) -> Result<u64, Errno>;
    /// Writes `value` to the register `msr`.
    #[cfg(target_arch = "x86_64")]
    fn write_msr(&self, descriptor: &MsrDescriptor, msr: u32, value: u64) -> Result<(), Errno>;
    /// Applies `adjustment` to the wall clock's discipline.
    fn adjust_clock(&self, adjustment: ClockAdjustment) -> Result<ClockDiscipline, Errno>;
    /// Stops the VM with `status`, which in production does not return.
    fn power_off(&self, status: PowerOffStatus);
    /// Appends `record` to the kernel log, when the process may write it.
    fn write_kernel_log(&self, record: &[u8], deadline: Option<Instant>);
    /// Writes `separator` and then `line` to the console, or to standard error when the console
    /// does not open.
    fn write_console(&self, separator: &[u8], line: &[u8], deadline: Option<Instant>);
}

/// An MSR device, or the token of a test's fake.
#[cfg(target_arch = "x86_64")]
pub(crate) enum MsrDescriptor {
    /// A device that the kernel opened.
    Real(MsrFile),
    /// A test's token, which owns no descriptor.
    #[cfg(test)]
    Fake,
}

/// The production privileged operations.
///
/// They take the port permission for each transaction, on the thread that runs it, rather than
/// for the whole command: Linux grants it per thread, and the threads and processes that a
/// holder starts inherit it.
pub(crate) struct RealOps;

impl PrivilegedOps for RealOps {
    #[cfg(target_arch = "x86_64")]
    fn read_counter(&self) -> u64 {
        x86::rdtsc_lfence()
    }

    #[cfg(target_arch = "x86_64")]
    fn counter_scale(&self, _: Option<&FileLock>) -> Result<CounterScale, SampleError> {
        checked_x86_scale(
            self.open_msr(0, MsrAccess::Read)
                .and_then(|descriptor| self.read_msr(&descriptor, msr::HV_TSC_FREQUENCY)),
        )
    }

    #[cfg(target_arch = "aarch64")]
    fn read_counter(&self) -> u64 {
        sys::aarch64::counter_ticks()
    }

    #[cfg(target_arch = "aarch64")]
    fn counter_scale(&self, window_lock: Option<&FileLock>) -> Result<CounterScale, SampleError> {
        self.clock_context(window_lock)
            .map(|context| context.counter_scale())
    }

    #[cfg(target_arch = "aarch64")]
    fn clock_context(&self, window_lock: Option<&FileLock>) -> Result<ClockContext, SampleError> {
        aarch64::clock_context(window_lock)
    }

    fn realtime_ns(&self) -> i64 {
        clock_ns(sys::Clock::Realtime)
    }

    #[cfg(target_arch = "x86_64")]
    fn open_ports(&self) -> Result<PortAccess<'_>, PortError> {
        PortAccess::request()
    }

    #[cfg(target_arch = "aarch64")]
    fn open_time_window(&self) -> Result<TimeAccess<'_>, TimeAccessError> {
        sys::aarch64::PlatformDevice::open()
    }

    #[cfg(target_arch = "x86_64")]
    fn open_msr(&self, cpu: usize, access: MsrAccess) -> Result<MsrDescriptor, Errno> {
        MsrFile::open(cpu, access).map(MsrDescriptor::Real)
    }

    #[cfg(target_arch = "x86_64")]
    fn read_msr(&self, descriptor: &MsrDescriptor, msr: u32) -> Result<u64, Errno> {
        match descriptor {
            MsrDescriptor::Real(file) => file.read(msr),
            #[cfg(test)]
            MsrDescriptor::Fake => Err(Errno::EIO),
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn write_msr(&self, descriptor: &MsrDescriptor, msr: u32, value: u64) -> Result<(), Errno> {
        match descriptor {
            MsrDescriptor::Real(file) => file.write(msr, value),
            #[cfg(test)]
            MsrDescriptor::Fake => Err(Errno::EIO),
        }
    }

    fn adjust_clock(&self, adjustment: ClockAdjustment) -> Result<ClockDiscipline, Errno> {
        sys::adjust_clock(adjustment)
    }

    fn power_off(&self, status: PowerOffStatus) {
        sys::power_off(status.code())
    }

    fn write_kernel_log(&self, record: &[u8], deadline: Option<Instant>) {
        let flags = OpenFlags::O_WRONLY
            | OpenFlags::O_CLOEXEC
            | deadline.map_or(OpenFlags::empty(), |_| OpenFlags::O_NONBLOCK);
        if FileLock::check_deadline(deadline).is_ok()
            && let Ok(log) = sys::open(kernel::KERNEL_LOG_DEVICE, flags, Mode::empty())
        {
            let _ = log.write_all_until(record, deadline);
        }
    }

    fn write_console(&self, separator: &[u8], line: &[u8], deadline: Option<Instant>) {
        write_console_at(kernel::CONSOLE_DEVICE, separator, line, deadline);
    }
}

/// Best-effort duplicate diagnostics cannot delay the structured failure returned to the caller.
fn write_console_at(path: &CStr, separator: &[u8], line: &[u8], deadline: Option<Instant>) {
    if FileLock::check_deadline(deadline).is_err() {
        return;
    }
    let flags = OpenFlags::O_WRONLY
        | OpenFlags::O_NOCTTY
        | OpenFlags::O_CLOEXEC
        | deadline.map_or(OpenFlags::empty(), |_| OpenFlags::O_NONBLOCK);
    match sys::open(path, flags, Mode::empty()) {
        Ok(console) => {
            let _ = console.write_all_until(separator, deadline);
            let _ = console.write_all_until(line, deadline);
        }
        Err(_) => match deadline {
            None => {
                sys::write_standard_error(separator);
                sys::write_standard_error(line);
            }
            Some(_) => {
                // Reopen rather than duplicate: changing a shared stderr description's flags
                // would also change other writers.
                if let Ok(stderr) = sys::open(c"/proc/self/fd/2", flags, Mode::empty()) {
                    let _ = stderr.write_all_until(separator, deadline);
                    let _ = stderr.write_all_until(line, deadline);
                }
            }
        },
    }
}

/// Returns the negative `errno` of an MSR access, or zero for one that succeeded, as the time
/// ABI's diagnostics print it.
#[cfg(target_arch = "x86_64")]
pub(crate) fn msr_error<T>(result: &Result<T, Errno>) -> i32 {
    result.as_ref().err().map_or(0, |&error| -(error as i32))
}

/// Shared runtime state and injectable privileged operations.
pub(crate) struct Runtime<'a> {
    /// Privileged-operation implementation.
    pub(crate) ops: &'a dyn PrivilegedOps,
    /// Whether violations are reported instead of powering off the VM.
    report_only: bool,
    /// Whether a violation that powers off the VM in enforcing mode fails the call instead, which
    /// the runtime keeps the violation for.
    strict: bool,
    /// The first violation that the call reported in strict mode, which fails the call.
    violation: Mutex<Option<Violation>>,
    /// The first port-revocation errno, or `u32::MAX` when none occurred.
    #[cfg(target_arch = "x86_64")]
    release_failure: AtomicU32,
    /// Current VM generation.
    generation: AtomicU32,
    /// x86's recorded declared rate, with its original positive-overwrite and zero-ignore policy.
    #[cfg(target_arch = "x86_64")]
    tsc_hz: AtomicU64,
    /// ARM's independently checked scale, absent until successful binding admission.
    #[cfg(target_arch = "aarch64")]
    counter_scale: OnceLock<CounterScale>,
    /// Whether an asynchronous child may have interrupted console output.
    async_output: AtomicBool,
    /// Serializes multi-destination violation records.
    report_lock: Mutex<()>,
    /// One absolute pre-request deadline, absent for legacy calls and capture requests.
    pub(crate) deadline: Option<Instant>,
}

impl<'a> Runtime<'a> {
    /// Creates runtime state around one privileged-operation implementation.
    pub(crate) fn new(ops: &'a dyn PrivilegedOps, report_only: bool) -> Self {
        Self {
            ops,
            report_only,
            strict: false,
            violation: Mutex::new(None),
            #[cfg(target_arch = "x86_64")]
            release_failure: AtomicU32::new(u32::MAX),
            generation: AtomicU32::new(0),
            #[cfg(target_arch = "x86_64")]
            tsc_hz: AtomicU64::new(0),
            #[cfg(target_arch = "aarch64")]
            counter_scale: OnceLock::new(),
            async_output: AtomicBool::new(false),
            report_lock: Mutex::new(()),
            deadline: None,
        }
    }

    /// Returns the runtime in strict mode, unless it is report-only: a violation that powers off
    /// the VM in enforcing mode is reported by its event alone, and the runtime keeps the first
    /// such violation, with which the call fails, as [`take_violation`](Self::take_violation)
    /// tells. The runtime then never powers off the VM.
    pub(crate) fn strict(mut self) -> Self {
        self.strict = !self.report_only;
        self
    }

    /// Returns whether conformance failures are report-only.
    pub(crate) fn report_only(&self) -> bool {
        self.report_only
    }

    /// Returns whether a violation fails the call instead of powering off the VM.
    pub(crate) fn is_strict(&self) -> bool {
        self.strict
    }

    /// Takes the first violation that the call reported in strict mode, which fails the call,
    /// if any.
    pub(crate) fn take_violation(&self) -> Option<Violation> {
        self.violation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Returns the current VM generation.
    pub(crate) fn generation(&self) -> u32 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Publishes the current VM generation to concurrent checks.
    pub(crate) fn set_generation(&self, value: u32) {
        self.generation.store(value, Ordering::Relaxed);
    }

    /// Uses x86's existing declared-rate cache or ARM's independently admitted scale.
    pub(crate) fn counter_scale(&self) -> Result<CounterScale, SampleError> {
        #[cfg(target_arch = "x86_64")]
        {
            self.counter_hz().map(|rate| {
                CounterScale::x86_recorded(
                    NonZeroU64::new(rate).expect("recorded or validated nonzero rate"),
                )
            })
        }
        #[cfg(target_arch = "aarch64")]
        {
            self.admit_counter_scale(None)
        }
    }

    #[cfg(target_arch = "aarch64")]
    /// Admits a missing scale under the caller's window lock, avoiding recursive lock acquisition.
    pub(crate) fn counter_scale_in_window(
        &self,
        lock: &FileLock,
    ) -> Result<CounterScale, SampleError> {
        self.admit_counter_scale(Some(lock))
    }

    /// Initializes the cache only after the backend has returned a checked arithmetic scale.
    #[cfg(target_arch = "aarch64")]
    fn admit_counter_scale(&self, lock: Option<&FileLock>) -> Result<CounterScale, SampleError> {
        if let Some(scale) = self.counter_scale.get() {
            return Ok(*scale);
        }
        let measured = self.ops.counter_scale(lock)?;
        self.initialize_counter_scale(measured)
    }

    /// Accepts equal reinitialization and refuses a conflicting concurrent result without mutation.
    #[cfg(target_arch = "aarch64")]
    fn initialize_counter_scale(
        &self,
        measured: CounterScale,
    ) -> Result<CounterScale, SampleError> {
        let recorded = *self.counter_scale.get_or_init(|| measured);
        if recorded != measured {
            return Err(SampleError::CounterScaleChanged);
        }
        Ok(recorded)
    }

    #[cfg(target_arch = "x86_64")]
    /// Records a nonzero boot/persisted x86 rate, preserving legacy positive replacement.
    pub(crate) fn set_tsc_hz(&self, value: u64) {
        if value != 0 {
            self.tsc_hz.store(value, Ordering::Relaxed);
        }
    }

    #[cfg(target_arch = "x86_64")]
    /// Uses the recorded x86 rate, otherwise reads only the frequency MSR and records success.
    pub(crate) fn counter_hz(&self) -> Result<u64, SampleError> {
        let recorded = self.tsc_hz.load(Ordering::Relaxed);
        if recorded != 0 {
            return Ok(recorded);
        }
        let measured = self.ops.counter_scale(None)?.declared_hz().get();
        self.tsc_hz.store(measured, Ordering::Relaxed);
        Ok(measured)
    }

    #[cfg(target_arch = "aarch64")]
    /// Rechecks the full ARM binding and requires its scale to agree with the successful cache.
    pub(crate) fn validated_arm_clock_context(&self) -> Result<ClockContext, SampleError> {
        FileLock::check_deadline(self.deadline).map_err(SampleError::Device)?;
        let context = self.ops.clock_context(None)?;
        self.initialize_counter_scale(context.counter_scale())?;
        FileLock::check_deadline(self.deadline).map_err(SampleError::Device)?;
        Ok(context)
    }

    /// Marks whether console output may need a leading newline.
    pub(crate) fn set_async_output(&self, value: bool) {
        self.async_output.store(value, Ordering::Relaxed);
    }

    /// Takes the calling thread's permission for the time ABI's ports, for one transaction,
    /// which [`PortAccess::release`] ends.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn open_ports(&self) -> Result<PortAccess<'a>, PortError> {
        if let Err(error) = sys::check_released() {
            self.retain_release_failure(error);
        }

        self.ops
            .open_ports()
            .inspect_err(|&error| self.retain_release_failure(error))
    }

    /// Opens the architecture's real time-window owner through the privileged backend.
    pub(crate) fn open_time_window(&self) -> Result<TimeAccess<'a>, TimeAccessError> {
        #[cfg(target_arch = "x86_64")]
        {
            self.open_ports()
        }
        #[cfg(target_arch = "aarch64")]
        {
            self.ops.open_time_window()
        }
    }

    /// Reports the first port-revocation failure, otherwise the thread's current
    /// permission state. A later successful release cannot erase the call's original failure.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn check_released(&self) -> Result<(), TimeAccessError> {
        #[cfg(target_arch = "x86_64")]
        {
            match self.release_failure.load(Ordering::Relaxed) {
                u32::MAX => sys::check_released(),
                errno => Err(PortError::Revoke(Errno::from_raw(errno as i32))),
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            // ARM owns a CLOEXEC descriptor, not a thread-inheritable I/O bitmap.
            Ok(())
        }
    }

    /// Keeps the first revocation failure before diagnostics or later transactions can replace
    /// the calling thread's current error.
    #[cfg(target_arch = "x86_64")]
    fn retain_release_failure(&self, error: PortError) {
        if let PortError::Revoke(errno) = error {
            let _ = self.release_failure.compare_exchange(
                u32::MAX,
                errno as u32,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
    }

    /// Writes `error`, the failure of a transaction's end, to the kernel log, such as
    /// `nvx-time: ioperm revoke: Cannot allocate memory`, where nothing else reports it: after an
    /// event's write to OpenVMM's debug console, and in the time daemon, whose standard streams
    /// are on the null device. Until a later transaction revokes the permission, the thread
    /// starts nothing, as [`fork_process`] checks.
    pub(crate) fn log_release_failure(&self, error: TimeAccessError) {
        #[cfg(target_arch = "x86_64")]
        self.retain_release_failure(error);
        #[cfg(target_arch = "x86_64")]
        let record = format!("nvx-time: {error}\n");
        #[cfg(target_arch = "aarch64")]
        let record = format!("nvx-time: platform time device close: {}\n", error.desc());
        self.ops.write_kernel_log(
            &[DIAGNOSTIC_PRIORITY, record.as_bytes()].concat(),
            self.deadline,
        );
    }

    /// Acquires the reporting lock by the caller deadline. A poisoned lock keeps the legacy
    /// guardless reporting behavior.
    fn lock_reports(&self) -> Result<Option<MutexGuard<'_, ()>>, Errno> {
        let Some(deadline) = self.deadline else {
            return Ok(self.report_lock.lock().ok());
        };
        loop {
            FileLock::check_deadline(Some(deadline))?;
            match self.report_lock.try_lock() {
                Ok(guard) => {
                    FileLock::check_deadline(Some(deadline))?;
                    return Ok(Some(guard));
                }
                Err(TryLockError::Poisoned(_)) => return Ok(None),
                Err(TryLockError::WouldBlock) => thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(1)),
                ),
            }
        }
    }

    /// Emits one serialized violation record to every ABI destination.
    fn emit_event(&self, code: ViolationCode, source: EventSource, phase: Phase, detail: &[u8]) {
        if let Ok(_guard) = self.lock_reports() {
            self.emit_event_unlocked(code, source, phase, detail);
        }
    }

    /// Emits an event while the caller already owns `report_lock`.
    fn emit_event_unlocked(
        &self,
        code: ViolationCode,
        source: EventSource,
        phase: Phase,
        detail: &[u8],
    ) {
        let line = format_event(
            self.report_only(),
            code,
            source,
            phase,
            self.generation(),
            clock_ns(sys::Clock::Boottime),
            detail,
        );
        self.ops.write_kernel_log(
            &[KERNEL_LOG_PRIORITY, line.as_bytes()].concat(),
            self.deadline,
        );
        if FileLock::check_deadline(self.deadline).is_ok()
            && let Ok(access) = self.open_time_window()
        {
            let written = (|| -> Result<(), Errno> {
                if self.async_output.load(Ordering::Relaxed) {
                    access.write_debug_byte(b'\n')?;
                }
                for byte in line.bytes() {
                    if FileLock::check_deadline(self.deadline).is_err() {
                        break;
                    }
                    access.write_debug_byte(byte)?;
                }
                Ok(())
            })();
            if let Err(error) = written {
                let detail = format!("nvx-time: platform debug console: {}\n", error.desc());
                self.ops.write_kernel_log(
                    &[DIAGNOSTIC_PRIORITY, detail.as_bytes()].concat(),
                    self.deadline,
                );
            }
            // The event goes on after a failure, which keeps the thread from starting anything,
            // as `fork_process` checks.
            if let Err(error) = access.finish() {
                self.log_release_failure(error);
            }
        }
        let separator: &[u8] = if self.async_output.load(Ordering::Relaxed) {
            b"\n"
        } else {
            b""
        };
        self.ops
            .write_console(separator, line.as_bytes(), self.deadline);
        let _ = count_violation(self.deadline);
    }

    /// Reports a conformance failure, and powers off unless report-only or strict.
    pub(crate) fn conformance_failure(&self, code: ViolationCode, phase: Phase, detail: &[u8]) {
        match self.lock_reports() {
            Ok(_guard) => {
                self.emit_event_unlocked(code, EventSource::Conformance, phase, detail);
                self.stop(PowerOffStatus::Conformance, code, detail);
            }
            Err(_) => self.stop(PowerOffStatus::Conformance, code, detail),
        }
    }

    /// Emits a fatal event and requests its typed power-off status, unless report-only or
    /// strict.
    pub(crate) fn fail_fatal(
        &self,
        status: PowerOffStatus,
        code: ViolationCode,
        source: EventSource,
        phase: Phase,
        detail: &[u8],
    ) {
        self.emit_event(code, source, phase, detail);
        self.stop(status, code, detail);
    }

    /// Reports that restore-time repair failed, a fatal event of the restore phase, and powers off
    /// with the repair status unless report-only or strict.
    pub(crate) fn repair_failure(&self, code: ViolationCode, detail: &[u8]) {
        self.fail_fatal(
            PowerOffStatus::Repair,
            code,
            EventSource::Repair,
            Phase::Restore,
            detail,
        );
    }

    /// Stops the VM with `status` once its event reported the violation of `code`, with `detail`:
    /// powers it off in enforcing mode, keeps the call's first violation in strict mode, and does
    /// neither in report-only mode.
    fn stop(&self, status: PowerOffStatus, code: ViolationCode, detail: &[u8]) {
        if self.is_strict() {
            self.violation
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert_with(|| Violation::new(status, code, detail));
        } else if !self.report_only {
            self.ops.power_off(status);
        }
    }
}

/// Reads one Linux clock in nanoseconds, or zero when it cannot be read.
pub(crate) fn clock_ns(clock: sys::Clock) -> i64 {
    sys::clock_time(clock).map_or(0, |time| time.num_nanoseconds())
}

/// Returns process-wide CPU time in wrapping nanoseconds.
pub(crate) fn cpu_time_ns() -> i64 {
    clock_ns(sys::Clock::ProcessCpuTime)
}

/// Why the time tools could not start a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpawnError {
    /// The calling thread may hold port permission, which the process would inherit.
    #[cfg(target_arch = "x86_64")]
    Ports(PortError),
    /// `fork` failed.
    Fork(Errno),
}

impl Display for SpawnError {
    /// Writes the diagnostic: the description of `fork`'s `errno`, or why the thread may hold
    /// port permission, such as `ioperm revoke: Cannot allocate memory`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            #[cfg(target_arch = "x86_64")]
            Self::Ports(error) => error.fmt(formatter),
            Self::Fork(error) => formatter.write_str(error.desc()),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Forks the process, as [`sys::fork`] does, once the calling thread holds no port permission,
/// which the child would inherit.
pub(crate) fn fork_process() -> Result<ForkResult, SpawnError> {
    #[cfg(target_arch = "x86_64")]
    sys::check_released().map_err(SpawnError::Ports)?;
    sys::fork().map_err(SpawnError::Fork)
}

/// Creates the NVX runtime directories, ignoring preexistence and failures.
pub(crate) fn make_runtime_directories() {
    let _ = sys::create_directory(RUN_DIR, DIRECTORY_MODE);
    let _ = sys::create_directory(TIME_DIR, DIRECTORY_MODE);
}

/// Durably writes `text` to `temporary`, and atomically renames it to `path`. A failure after
/// the file was created removes it.
pub(crate) fn replace_file(path: &CStr, temporary: &CStr, text: &[u8]) -> Result<(), Errno> {
    let file = CreatedFile::create(temporary, false, PUBLIC_FILE_MODE)?;
    file.write_all(text)?;
    file.sync()?;
    file.rename(path)
}

/// Takes a runtime lock using the caller's explicit pre-request deadline, if any.
pub(crate) fn lock_file(path: &CStr, deadline: Option<Instant>) -> Result<FileLock, Errno> {
    FileLock::check_deadline(deadline)?;
    make_runtime_directories();
    FileLock::acquire(path, PRIVATE_FILE_MODE, deadline)
}

/// Loads and parses the published time state.
pub(crate) fn report_load() -> Result<StateReport, Errno> {
    let text = sys::read_file(STATE_PATH)?;
    StateReport::parse(&text).ok_or(Errno::EINVAL)
}

/// Preserves x86's pending-report recovery for every read error, without admitting clock rates.
/// ARM keeps its newer distinction between absent status and an unreadable/invalid report.
pub(super) fn report_or_pending(read: Result<StateReport, Errno>) -> Result<StateReport, Errno> {
    match read {
        Ok(state) => Ok(state),
        #[cfg(target_arch = "x86_64")]
        Err(_) => Ok(StateReport::default()),
        #[cfg(target_arch = "aarch64")]
        Err(Errno::ENOENT) => Ok(StateReport::default()),
        #[cfg(target_arch = "aarch64")]
        Err(error) => Err(error),
    }
}

/// Atomically persists one complete time state.
pub(crate) fn report_store(state: &StateReport) -> Result<(), Errno> {
    replace_file(STATE_PATH, STATE_TEMP_PATH, state.to_string().as_bytes())
}

/// Applies one read-modify-write update to the time state under its lock.
pub(crate) fn report_apply(update: impl FnOnce(&mut StateReport)) -> Result<(), Errno> {
    report_apply_until(None, update)
}

/// Updates state under a runtime lock bounded by the same pre-request deadline.
pub(crate) fn report_apply_until(
    deadline: Option<Instant>,
    update: impl FnOnce(&mut StateReport),
) -> Result<(), Errno> {
    report_try_apply_until(deadline, |state| {
        update(state);
        Ok(())
    })
}

/// Rejects a fallible update before any replacement of the published report.
pub(crate) fn report_try_apply_until(
    deadline: Option<Instant>,
    update: impl FnOnce(&mut StateReport) -> Result<(), Errno>,
) -> Result<(), Errno> {
    let _lock = lock_file(STATE_LOCK_PATH, deadline)?;
    let mut state = report_or_pending(report_load())?;
    FileLock::check_deadline(deadline)?;
    update(&mut state)?;
    report_store(&state)
}

/// Increments the persisted violation counter when state exists.
fn count_violation(deadline: Option<Instant>) -> Result<(), Errno> {
    if !sys::path_exists(STATE_PATH) {
        return Ok(());
    }
    report_apply_until(deadline, |state| {
        state.violations = state.violations.wrapping_add(1)
    })
}

/// Loads x86's persisted boot rates with the original nonzero-only check and no hardware I/O.
#[cfg(target_arch = "x86_64")]
pub(crate) fn rates_load() -> Result<(u64, u64), Errno> {
    report_load()?.x86_persisted_rates()
}

/// Saves suppression with explicitly bounded runtime-lock and detector operations.
#[cfg(target_arch = "x86_64")]
pub(crate) fn suppression_save_until(deadline: Option<Instant>) -> Result<(), SaveError> {
    FileLock::check_deadline(deadline).map_err(SaveError::Undone)?;
    make_runtime_directories();
    SUPPRESSION.save_until(deadline, sys::write_number)
}

/// Why a save of the stall detectors' settings failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(target_arch = "x86_64")]
pub(crate) enum SaveError {
    /// The save failed with this error, and left the settings as they were: it changed none, or
    /// undid every change and removed its record. A record that exists already, of a pending
    /// capture, is `EEXIST`, and stays that capture's.
    Undone(Errno),
    /// The save failed to suppress a detector with this error, then to undo every change, so it
    /// kept its record, from which [`suppression_restore`] restores the settings.
    Kept(Errno),
}

/// Formats a diagnostic: `prefix`, then the description of `error`.
fn describe(prefix: &[u8], error: Errno) -> Vec<u8> {
    [prefix, error.desc().as_bytes()].concat()
}

/// Restores the stall detectors' settings from the suppression record, and removes the record;
/// see [`Suppression::restore_until`]. A missing record succeeds when `missing_ok`.
#[cfg(target_arch = "x86_64")]
pub(crate) fn suppression_restore(missing_ok: bool) -> Result<(), Vec<u8>> {
    suppression_restore_until(missing_ok, None)
}

/// Restores suppression by an explicit deadline, absent after a capture request.
#[cfg(target_arch = "x86_64")]
pub(crate) fn suppression_restore_until(
    missing_ok: bool,
    deadline: Option<Instant>,
) -> Result<(), Vec<u8>> {
    FileLock::check_deadline(deadline).map_err(|error| describe(b"restore detectors: ", error))?;
    make_runtime_directories();
    SUPPRESSION.restore_until(missing_ok, deadline)
}

/// The suppression of the stall detectors for a capture: the detectors, the record of their
/// saved settings, which exists while a capture is pending, and the lock that serializes the
/// record's saves and restores, so that each restores only the settings that a save recorded.
#[cfg(target_arch = "x86_64")]
struct Suppression<'a> {
    /// The detectors, of which only the first must exist, and the value that suppresses each.
    detectors: &'a [(&'a CStr, i64)],
    /// The record of the detectors' saved settings.
    record: &'a CStr,
    /// The lock that serializes the saves and restores of the record.
    lock: &'a CStr,
}

/// The guest's suppression of its stall detectors.
#[cfg(target_arch = "x86_64")]
const SUPPRESSION: Suppression<'static> = Suppression {
    detectors: &SUPPRESSED_DETECTORS,
    record: SUPPRESSION_PATH,
    lock: SUPPRESSION_LOCK_PATH,
};

#[cfg(target_arch = "x86_64")]
impl Suppression<'_> {
    /// Saves with one deadline shared by locking, reads, writes and any rollback.
    fn save_until(
        &self,
        deadline: Option<Instant>,
        mut write: impl FnMut(&CStr, i64) -> Result<(), Errno>,
    ) -> Result<(), SaveError> {
        let _lock =
            FileLock::acquire(self.lock, PRIVATE_FILE_MODE, deadline).map_err(SaveError::Undone)?;
        let mut saved = Vec::new();
        for (index, &(path, suppressed)) in self.detectors.iter().enumerate() {
            FileLock::check_deadline(deadline).map_err(SaveError::Undone)?;
            if let Err(error) = sys::check_exists(path) {
                if index == 0 {
                    return Err(SaveError::Undone(error));
                }
                continue;
            }
            let value = sys::read_number::<i64>(path).map_err(SaveError::Undone)?;
            saved.push((path, value, suppressed));
        }
        let mut text = Vec::new();
        for (path, value, _) in &saved {
            text.extend_from_slice(path.to_bytes());
            text.extend_from_slice(format!("={value}\n").as_bytes());
        }
        FileLock::check_deadline(deadline).map_err(SaveError::Undone)?;
        let record =
            CreatedFile::create(self.record, true, PRIVATE_FILE_MODE).map_err(SaveError::Undone)?;
        record.write_all(&text).map_err(SaveError::Undone)?;
        // As in the C program, a record that fails to close stays, and the suppression goes on.
        let _ = record.keep();
        for (written, &(path, _, suppressed)) in saved.iter().enumerate() {
            if let Err(error) =
                FileLock::check_deadline(deadline).and_then(|()| write(path, suppressed))
            {
                // The record stays unless every earlier write is undone, so that a restore can
                // still restore the settings.
                let mut undone = true;
                for &(path, value, _) in &saved[..written] {
                    undone &= FileLock::check_deadline(deadline)
                        .and_then(|()| write(path, value))
                        .is_ok();
                }
                if undone
                    && FileLock::check_deadline(deadline)
                        .and_then(|()| sys::unlink(self.record))
                        .is_ok()
                {
                    return Err(SaveError::Undone(error));
                }
                return Err(SaveError::Kept(error));
            }
        }
        Ok(())
    }

    /// Restores under the same explicit deadline as the failed preparation.
    fn restore_until(&self, missing_ok: bool, deadline: Option<Instant>) -> Result<(), Vec<u8>> {
        let expired = |error| describe(b"restore detectors: ", error);
        let _lock = FileLock::acquire(self.lock, PRIVATE_FILE_MODE, deadline)
            .map_err(|error| describe(b"lock /run/nvx/time-suppression.lock: ", error))?;
        let text = match sys::read_file(self.record) {
            Ok(text) => text,
            Err(Errno::ENOENT) if missing_ok => return Ok(()),
            Err(error) => return Err(describe(b"read /run/nvx/time-suppression: ", error)),
        };
        for line in text
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            FileLock::check_deadline(deadline).map_err(expired)?;
            let Some(equal) = line.iter().position(|byte| *byte == b'=') else {
                break;
            };
            let Ok(path) = CString::new(&line[..equal]) else {
                break;
            };
            if let Err(error) = sys::write_file(&path, &line[equal + 1..]) {
                let quoted = &line[..equal.min(QUOTED_PATH_LIMIT)];
                return Err(describe(
                    &[b"restore ".as_slice(), quoted, b": "].concat(),
                    error,
                ));
            }
        }
        FileLock::check_deadline(deadline).map_err(expired)?;
        sys::unlink(self.record)
            .map_err(|error| describe(b"remove /run/nvx/time-suppression: ", error))
    }
}

/// Reads a byte buffer using one scalar four-byte port access per chunk.
#[cfg(target_arch = "x86_64")]
pub(crate) fn portb_read(ports: &PortAccess<'_>, port: Port, buffer: &mut [u8]) {
    for chunk in buffer.chunks_mut(size_of::<u32>()) {
        let bytes = ports.read_u32(port).to_ne_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

/// Locks the shared port-window transaction.
pub(crate) fn lock_portb() -> Result<FileLock, Errno> {
    lock_file(PORTB_LOCK_PATH, None)
}

/// Formats a failure of [`lock_portb`]: `portb lock: `, then the description of `error`.
pub(crate) fn portb_lock_failure(error: Errno) -> Vec<u8> {
    describe(b"portb lock: ", error)
}

/// Latches and decodes one host time sample: brackets the selector write with TSC reads, then
/// reads the realtime clock between two more, with which the guest pairs the sample's guest TSC,
/// before it reads the time window.
pub(crate) fn take_sample(
    window: &dyn TimeWindow,
    ops: &dyn PrivilegedOps,
) -> Result<(TimeSample, GuestReads), SampleError> {
    let mut bytes = [0_u8; SAMPLE_SIZE];
    let before = ops.read_counter();
    window
        .select_window(SELECT_TIME_SAMPLE)
        .map_err(SampleError::Device)?;
    let after = ops.read_counter();
    let realtime_ns = ops.realtime_ns();
    let end = ops.read_counter();
    window
        .read_window(&mut bytes)
        .map_err(SampleError::Device)?;
    let half_cycles = end.saturating_sub(after).div_ceil(2);
    let reads = GuestReads {
        before,
        after,
        realtime_ns,
        realtime_tsc: after.saturating_add(half_cycles),
        realtime_half_cycles: half_cycles,
    };
    let sample = TimeSample::parse(&bytes).map_err(|error| SampleError::Invalid(Some(error)))?;
    Ok((sample, reads))
}

/// Whether the selected sample satisfied the requested uncertainty bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleResult {
    /// The sample was within the bound.
    Accepted,
    /// A valid sample was selected but remained wider than the bound.
    Wide,
}

/// Why no host time sample could be selected. Its `Display` is the diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleError {
    /// A backend, descriptor or scalar-window operation failed.
    Device(Errno),
    #[cfg(target_arch = "aarch64")]
    /// Actual ARM clocksource, identity or counter metadata failed admission.
    Binding(ClockError),
    /// Concurrent initial backend reads disagreed; no reported rate may repair that admission.
    #[cfg(target_arch = "aarch64")]
    CounterScaleChanged,
    /// The thread could not take, or give up, the permission for the ports.
    #[cfg(target_arch = "x86_64")]
    Ports(PortError),
    /// The guest could not read the declared TSC rate, with which it pairs a sample, from the
    /// frequency MSR: the error, or the implausible rate that it read.
    #[cfg(target_arch = "x86_64")]
    TscRate(Result<u64, Errno>),
    /// The port window, of this status, offers no time sample.
    NoTimeWindow(u8),
    /// A sample came from another VM generation, which a restore must handle first.
    Generation {
        /// The sample's generation.
        sample: u32,
        /// The generation that the guest expected.
        expected: u32,
    },
    /// No attempt took a valid sample: the last attempt's error, if any.
    Invalid(Option<TimeSampleError>),
}

impl SampleError {
    /// Returns how the discipline records the rejection of a poll's sample that failed so.
    pub(crate) fn rejection(&self) -> SampleRejection {
        match self {
            Self::Invalid(Some(TimeSampleError::Tsc { .. })) => SampleRejection::Tsc,
            _ => SampleRejection::Invalid,
        }
    }
}

impl Display for SampleError {
    /// Writes the diagnostic, such as `time sample generation 7, expected 6`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            #[cfg(target_arch = "aarch64")]
            Self::CounterScaleChanged => {
                formatter.write_str("counter scale changed during admission")
            }
            Self::Device(error) => write!(formatter, "platform time window: {}", error.desc()),
            #[cfg(target_arch = "aarch64")]
            Self::Binding(error) => error.fmt(formatter),
            #[cfg(target_arch = "x86_64")]
            Self::Ports(error) => error.fmt(formatter),
            #[cfg(target_arch = "x86_64")]
            Self::TscRate(Err(error)) => write!(
                formatter,
                "MSR {:#010x} read: {}",
                msr::HV_TSC_FREQUENCY,
                error.desc()
            ),
            #[cfg(target_arch = "x86_64")]
            Self::TscRate(Ok(rate)) => write!(
                formatter,
                "MSR {:#010x} (TSC rate) is {rate} Hz",
                msr::HV_TSC_FREQUENCY
            ),
            Self::NoTimeWindow(status) => write!(
                formatter,
                "portb status 0x{status:02x} has no time-sample window"
            ),
            Self::Generation { sample, expected } => write!(
                formatter,
                "time sample generation {sample}, expected {expected}"
            ),
            Self::Invalid(Some(error)) => error.fmt(formatter),
            Self::Invalid(None) => formatter.write_str("no valid time sample"),
        }
    }
}

impl std::error::Error for SampleError {}

#[cfg(target_arch = "x86_64")]
impl From<TimeAccessError> for SampleError {
    fn from(error: TimeAccessError) -> Self {
        Self::Ports(error)
    }
}

#[cfg(target_arch = "aarch64")]
impl From<TimeAccessError> for SampleError {
    fn from(error: TimeAccessError) -> Self {
        Self::Device(error)
    }
}

/// Returns the actual admitted counter scale and the kernel's frequency correction.
pub(crate) fn clock_rates(runtime: &Runtime<'_>) -> Result<CounterRates, SampleError> {
    Ok(CounterRates {
        scale: runtime.counter_scale()?,
        frequency: current_frequency(runtime),
    })
}

/// Selects the lowest-uncertainty valid sample from a bounded attempt count, in one port
/// transaction, which ends before this returns.
pub(crate) fn best_sample(
    runtime: &Runtime<'_>,
    attempts: usize,
    bound_ns: i64,
    generation: u32,
) -> Result<(SampleResult, BestSample), SampleError> {
    let rates = clock_rates(runtime)?;
    let access = runtime.open_time_window().map_err(SampleError::from)?;
    let selected = select_sample(&access, runtime.ops, attempts, bound_ns, generation, rates);
    access.finish().map_err(SampleError::from)?;
    selected
}

/// Selects the lowest-uncertainty valid sample through `ports`, pairing each with the guest's
/// clocks at `rates`, as [`best_sample`] does.
pub(crate) fn select_sample(
    window: &dyn TimeWindow,
    ops: &dyn PrivilegedOps,
    attempts: usize,
    bound_ns: i64,
    generation: u32,
    rates: CounterRates,
) -> Result<(SampleResult, BestSample), SampleError> {
    let status = window.window_status().map_err(SampleError::Device)?;
    if status & PORTB_TIME_WINDOW == 0 {
        return Err(SampleError::NoTimeWindow(status));
    }
    let mut best: Option<BestSample> = None;
    let mut invalid = None;
    for _ in 0..attempts {
        let (sample, reads) = match take_sample(window, ops) {
            Ok(taken) => taken,
            Err(SampleError::Invalid(error)) => {
                invalid = error;
                continue;
            }
            Err(error) => return Err(error),
        };
        if sample.generation != generation {
            return Err(SampleError::Generation {
                sample: sample.generation,
                expected: generation,
            });
        }
        let (theta, epsilon) = match wire::pair_bound_sample(&sample, &reads, rates) {
            Ok(pairing) => pairing,
            Err(error) => {
                invalid = Some(error);
                continue;
            }
        };
        let (frequency, limited) = rates.requested_frequency(sample.host.correction);
        let candidate = BestSample {
            theta,
            epsilon,
            host: sample.host,
            frequency,
            limited,
            test_hooks: sample.test_hooks(),
        };
        if best.is_none_or(|best| epsilon < best.epsilon) {
            best = Some(candidate);
        }
        if epsilon <= bound_ns {
            return Ok((SampleResult::Accepted, candidate));
        }
    }
    best.map(|best| (SampleResult::Wide, best))
        .ok_or(SampleError::Invalid(invalid))
}

/// Steps realtime by a signed nanosecond offset.
fn step_clock(runtime: &Runtime<'_>, offset_ns: i64) -> Result<(), Errno> {
    let (seconds, nanoseconds) = discipline::split_offset(offset_ns);
    runtime
        .ops
        .adjust_clock(ClockAdjustment::Step {
            seconds,
            nanoseconds,
        })
        .map(drop)
}

/// Holds Linux's scaled-ppm frequency correction, with its error estimates.
pub(crate) fn hold_frequency(runtime: &Runtime<'_>, hold: FrequencyHold) -> Result<(), Errno> {
    runtime
        .ops
        .adjust_clock(ClockAdjustment::Frequency(hold))
        .map(drop)
}

/// Queries the current scaled-ppm frequency, returning zero on failure.
pub(crate) fn current_frequency(runtime: &Runtime<'_>) -> i64 {
    runtime
        .ops
        .adjust_clock(ClockAdjustment::Query)
        .map_or(0, |discipline| discipline.scaled_ppm)
}

/// Steps the wall clock to host UTC by `sample`'s offset, and then holds the frequency
/// correction that its host clock correction asks for, with error estimates of its uncertainty
/// and the host's error.
pub(crate) fn synchronize(runtime: &Runtime<'_>, sample: &BestSample) -> Result<(), Errno> {
    step_clock(runtime, sample.theta)?;
    hold_frequency(
        runtime,
        discipline::frequency_hold(0, sample.epsilon, sample.frequency, sample.host.error_us()),
    )
}

/// What a capture's [hold on the clock's rate](hold_declared_rate) interrupted: the frequency
/// correction that the discipline held before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(target_arch = "x86_64")]
pub(crate) struct RateHold {
    /// The held frequency correction, in scaled ppm, that a capture that continues resumes.
    frequency: i64,
}

/// Holds the wall clock at the declared TSC rate for a capture request, without the host's
/// clock correction, and without a phase offset in flight, until [`resume_rate`] or the repair
/// of a restore holds the host's correction again.
///
/// The kernel counts a TSC advance at the rate of its clocks' multiplier at that moment, whole,
/// when it first reads the TSC afterwards. A restore advances the TSC by the downtime at the
/// declared rate, so a clock whose frequency correction is the host's, as the discipline sets it,
/// counts the downtime times one plus that correction, which differs from the downtime by up to
/// the correction's 500 ppm of it. Holding the correction that counts the TSC at exactly the
/// declared rate, `F_k / F - 1` for the kernel's rounding `F_k` of the rate `F` to the kHz, makes
/// the monotonic and boot-time clocks advance by the downtime.
///
/// The discipline's phase-locked loop adds its slew of a phase offset to the clock's rate, and
/// the kernel caches that slew in its tick length until its next second, so the hold steps the
/// clock by zero, which clears the cache, as [`ClockAdjustment::Pin`] describes. It keeps the
/// discipline's status and error estimates, and the caller holds the port-B lock, so that no
/// poll of the time daemon changes the rate before the capture's anchor.
#[cfg(target_arch = "x86_64")]
pub(crate) fn hold_declared_rate(runtime: &Runtime<'_>) -> Result<RateHold, Vec<u8>> {
    let tsc_hz = runtime
        .counter_hz()
        .map_err(|error| error.to_string().into_bytes())?;
    let kept = runtime
        .ops
        .adjust_clock(ClockAdjustment::Query)
        .map_err(|error| describe(b"adjtimex: ", error))?;
    let (frequency, _) = decoders::guest_frequency(0, tsc_hz);
    runtime
        .ops
        .adjust_clock(ClockAdjustment::Pin {
            hold: kept.hold(frequency),
            status: kept.status,
        })
        .map_err(|error| describe(b"adjtimex: ", error))?;
    Ok(RateHold {
        frequency: kept.scaled_ppm,
    })
}

/// Resumes the frequency correction that a [hold on the clock's rate](hold_declared_rate)
/// interrupted, for a capture that continues without a restore. The discipline's next poll slews
/// the phase that the hold cleared.
#[cfg(target_arch = "x86_64")]
pub(crate) fn resume_rate(runtime: &Runtime<'_>, hold: RateHold) -> Result<(), Vec<u8>> {
    runtime
        .ops
        .adjust_clock(ClockAdjustment::SetFrequency(hold.frequency))
        .map(drop)
        .map_err(|error| describe(b"adjtimex: ", error))
}

/// Takes the initial sample within the bound, steps realtime to it, and holds the frequency
/// correction that the host's clock correction asks for.
pub(crate) fn initial_sample(runtime: &Runtime<'_>) -> Result<BestSample, Vec<u8>> {
    let (result, sample) = {
        let _lock = lock_portb().map_err(portb_lock_failure)?;
        #[cfg(target_arch = "aarch64")]
        runtime
            .counter_scale_in_window(&_lock)
            .map_err(|error| error.to_string().into_bytes())?;
        best_sample(
            runtime,
            SAMPLE_ATTEMPTS,
            SAMPLE_BOUND_NS,
            runtime.generation(),
        )
        .map_err(|error| error.to_string().into_bytes())?
    };
    if result == SampleResult::Wide {
        return Err(wide_sample(sample.epsilon));
    }
    synchronize(runtime, &sample).map_err(|error| describe(b"adjtimex: ", error))?;
    Ok(sample)
}

/// Formats the diagnostic of a selected sample of the uncertainty `epsilon`, wider than the
/// bound.
pub(crate) fn wide_sample(epsilon: i64) -> Vec<u8> {
    format!("time sample uncertainty {epsilon} ns exceeds {SAMPLE_BOUND_NS} ns").into_bytes()
}

/// Writes `entropy` at the start of the restored entropy file, and reports a failure to write
/// or close it with the file's path.
#[cfg(target_arch = "x86_64")]
pub(crate) fn write_entropy(
    file: FileDescriptor,
    path: &CStr,
    entropy: &[u8],
) -> Result<(), Vec<u8>> {
    let written = loop {
        match file.pwrite(entropy, 0) {
            Err(Errno::EINTR) => {}
            result => break result,
        }
    };
    let closed = file.close();
    let error = match (written, closed) {
        (Ok(count), Ok(())) if count == entropy.len() => return Ok(()),
        (Err(error), _) | (Ok(_), Err(error)) => error,
        (Ok(_), Ok(())) => Errno::EIO,
    };
    let path = &path.to_bytes()[..path.to_bytes().len().min(QUOTED_PATH_LIMIT)];
    Err(describe(
        &[b"write ".as_slice(), path, b": "].concat(),
        error,
    ))
}

#[cfg(all(test, target_arch = "x86_64"))]
pub(crate) mod tests;
