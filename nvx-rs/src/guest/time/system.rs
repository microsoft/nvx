//! The time tools' runtime: persistent state, violation events, port-B transactions, and the
//! wall clock's discipline, over the system layer's owners.

use std::ffi::{CStr, CString};
use std::fmt::{self, Display};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use super::Phase;
use super::api::Violation;
use super::decoders::{
    self, ClockRates, GuestReads, HostClock, SAMPLE_SIZE, TimeSample, TimeSampleError,
};
use super::events::{EventSource, ViolationCode, format_event};
use super::state::{SampleRejection, TimeState};
use crate::guest::sys::x86::{self, msr};
use crate::guest::sys::{
    self, ClockAdjustment, ClockDiscipline, CreatedFile, Errno, FileDescriptor, FileLock,
    ForkResult, FrequencyHold, Mode, MsrAccess, MsrFile, OpenFlags, Port, PortAccess, PortError,
    TimeValLike, kernel,
};
use crate::protocol::status;

/// Statuses with which a fatal time-ABI failure stops the VM through the control port, which
/// `nvx::protocol::status` publishes for the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PowerOffStatus {
    /// A time-ABI conformance check failed.
    Conformance = status::TIME_CONFORMANCE,
    /// The runtime watcher observed a time-ABI violation.
    Violation = status::TIME_VIOLATION,
    /// Restore-time repair could not complete safely.
    Repair = status::TIME_REPAIR,
}

impl PowerOffStatus {
    /// Returns the exact byte written to the control port.
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }
}

/// Selector requesting the restore packet.
pub(crate) const SELECT_PACKET: u8 = 0xa5;
/// Selector requesting the VM generation identifier.
pub(crate) const SELECT_GENERATION_ID: u8 = 0xa6;
/// Selector requesting a host time sample.
pub(crate) const SELECT_TIME_SAMPLE: u8 = 0xa7;
/// Status bit indicating that a restore packet can be read.
pub(crate) const PORTB_PACKET_AVAILABLE: u8 = 0x02;
/// Status bit indicating that a generation identifier can be read.
pub(crate) const PORTB_GENERATION_ID_AVAILABLE: u8 = 0x20;
/// Status bit indicating that the time-sample window is supported.
pub(crate) const PORTB_TIME_WINDOW: u8 = 0x40;
/// Snapshot-port value acknowledging completed restore repair.
pub(crate) const SNAPSHOT_ACKNOWLEDGE: u8 = 2;

/// Top-level NVX runtime directory, which also holds the time tools' locks and private records.
pub(crate) const RUN_DIR: &CStr = c"/run/nvx";
/// Time-status directory, which holds only what any process may read: the published state and
/// the daemon's process identifier, and their temporary files. A runtime may bind it into a
/// workload's root, where even a workload of user ID 0 must reach no lock that it could hold,
/// so the locks and private records stay in [`RUN_DIR`].
pub(crate) const TIME_DIR: &CStr = c"/run/nvx/time";
/// Published time state file.
pub(crate) const STATE_PATH: &CStr = c"/run/nvx/time/state";
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
pub(crate) const SUPPRESSION_PATH: &CStr = c"/run/nvx/time-suppression";
/// Advisory lock protecting suppression state.
const SUPPRESSION_LOCK_PATH: &CStr = c"/run/nvx/time-suppression.lock";
/// Private restore record consumed by the daemon.
pub(crate) const RESTORE_PATH: &CStr = c"/run/nvx/time-restore";
/// Temporary restore-record file.
pub(crate) const RESTORE_TEMP_PATH: &CStr = c"/run/nvx/time-restore.tmp";
/// Advisory lock serializing port-window transactions.
pub(crate) const PORTB_LOCK_PATH: &CStr = c"/run/nvx/portb.lock";
/// Temporary mount point used to force an RCU grace period.
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
const QUOTED_PATH_LIMIT: usize = 200;

/// The stall detectors that capture suppresses, and the value that suppresses each: RCU's stall
/// warnings, the soft-lockup detector, and the hung-task detector. Only the first must exist.
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
/// The smallest declared TSC rate, in Hz, that pairs a time sample: any lower rate is a misread
/// of the frequency MSR.
const MIN_TSC_HZ: u64 = 1_000_000;

/// Seconds after which RCU reports a CPU stall in NVX's guest kernels, which the checks require
/// and the daemon assumes when it cannot read the setting.
pub(crate) const DEFAULT_STALL_TIMEOUT_S: i64 = 21;

/// The privileged operations of the time tools, and the clock reads with which the guest pairs a
/// time sample, which a fake replaces in tests, so that their retry and failure paths are
/// testable.
pub(crate) trait PrivilegedOps: Sync {
    /// Reads the time-stamp counter after the instructions before it.
    fn read_tsc(&self) -> u64;
    /// Reads the realtime clock, in nanoseconds since the Unix epoch.
    fn realtime_ns(&self) -> i64;
    /// Takes the calling thread's permission for the time ABI's ports, for one transaction,
    /// which [`PortAccess::release`] ends.
    fn open_ports(&self) -> Result<PortAccess<'_>, PortError>;
    /// Opens the MSR device of `cpu`.
    fn open_msr(&self, cpu: usize, access: MsrAccess) -> Result<MsrDescriptor, Errno>;
    /// Reads the register `msr`.
    fn read_msr(&self, descriptor: &MsrDescriptor, msr: u32) -> Result<u64, Errno>;
    /// Writes `value` to the register `msr`.
    fn write_msr(&self, descriptor: &MsrDescriptor, msr: u32, value: u64) -> Result<(), Errno>;
    /// Applies `adjustment` to the wall clock's discipline.
    fn adjust_clock(&self, adjustment: ClockAdjustment) -> Result<ClockDiscipline, Errno>;
    /// Stops the VM with `status`, which in production does not return.
    fn power_off(&self, status: PowerOffStatus);
    /// Appends `record` to the kernel log, when the process may write it.
    fn write_kernel_log(&self, record: &[u8]);
    /// Writes `separator` and then `line` to the console, or to standard error when the console
    /// does not open.
    fn write_console(&self, separator: &[u8], line: &[u8]);
}

/// An MSR device, or the token of a test's fake.
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
    fn read_tsc(&self) -> u64 {
        x86::rdtsc_lfence()
    }

    fn realtime_ns(&self) -> i64 {
        clock_ns(sys::Clock::Realtime)
    }

    fn open_ports(&self) -> Result<PortAccess<'_>, PortError> {
        PortAccess::request()
    }

    fn open_msr(&self, cpu: usize, access: MsrAccess) -> Result<MsrDescriptor, Errno> {
        MsrFile::open(cpu, access).map(MsrDescriptor::Real)
    }

    fn read_msr(&self, descriptor: &MsrDescriptor, msr: u32) -> Result<u64, Errno> {
        match descriptor {
            MsrDescriptor::Real(file) => file.read(msr),
            #[cfg(test)]
            MsrDescriptor::Fake => Err(Errno::EIO),
        }
    }

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

    fn write_kernel_log(&self, record: &[u8]) {
        let flags = OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC;
        if let Ok(log) = sys::open(kernel::KERNEL_LOG_DEVICE, flags, Mode::empty()) {
            let _ = log.write_all(record);
        }
    }

    fn write_console(&self, separator: &[u8], line: &[u8]) {
        let flags = OpenFlags::O_WRONLY | OpenFlags::O_NOCTTY | OpenFlags::O_CLOEXEC;
        match sys::open(kernel::CONSOLE_DEVICE, flags, Mode::empty()) {
            Ok(console) => {
                let _ = console.write_all(separator);
                let _ = console.write_all(line);
            }
            Err(_) => {
                sys::write_standard_error(separator);
                sys::write_standard_error(line);
            }
        }
    }
}

/// Returns the negative `errno` of an MSR access, or zero for one that succeeded, as the time
/// ABI's diagnostics print it.
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
    /// Current VM generation.
    generation: AtomicU32,
    /// The declared TSC rate, `F`, in Hz, once known, or zero.
    tsc_hz: AtomicU64,
    /// Whether an asynchronous child may have interrupted console output.
    async_output: AtomicBool,
    /// Serializes multi-destination violation records.
    report_lock: Mutex<()>,
}

impl<'a> Runtime<'a> {
    /// Creates runtime state around one privileged-operation implementation.
    pub(crate) fn new(ops: &'a dyn PrivilegedOps, report_only: bool) -> Self {
        Self {
            ops,
            report_only,
            strict: false,
            violation: Mutex::new(None),
            generation: AtomicU32::new(0),
            tsc_hz: AtomicU64::new(0),
            async_output: AtomicBool::new(false),
            report_lock: Mutex::new(()),
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

    /// Records the declared TSC rate, `F`, in Hz, that the boot checks validated, unless it is
    /// zero, for none.
    pub(crate) fn set_tsc_hz(&self, value: u64) {
        if value != 0 {
            self.tsc_hz.store(value, Ordering::Relaxed);
        }
    }

    /// Returns the declared TSC rate, `F`, in Hz: the one that the runtime recorded, or else the
    /// frequency MSR's, of CPU 0, which it then records.
    pub(crate) fn tsc_hz(&self) -> Result<u64, SampleError> {
        let recorded = self.tsc_hz.load(Ordering::Relaxed);
        if recorded != 0 {
            return Ok(recorded);
        }
        let read = self
            .ops
            .open_msr(0, MsrAccess::Read)
            .and_then(|descriptor| self.ops.read_msr(&descriptor, msr::HV_TSC_FREQUENCY));
        match read {
            Ok(rate) if rate >= MIN_TSC_HZ => {
                self.tsc_hz.store(rate, Ordering::Relaxed);
                Ok(rate)
            }
            read => Err(SampleError::TscRate(read)),
        }
    }

    /// Marks whether console output may need a leading newline.
    pub(crate) fn set_async_output(&self, value: bool) {
        self.async_output.store(value, Ordering::Relaxed);
    }

    /// Takes the calling thread's permission for the time ABI's ports, for one transaction,
    /// which [`PortAccess::release`] ends.
    pub(crate) fn open_ports(&self) -> Result<PortAccess<'a>, PortError> {
        self.ops.open_ports()
    }

    /// Writes `error`, the failure of a transaction's end, to the kernel log, such as
    /// `nvx-time: ioperm revoke: Cannot allocate memory`, where nothing else reports it: after an
    /// event's write to OpenVMM's debug console, and in the time daemon, whose standard streams
    /// are on the null device. Until a later transaction revokes the permission, the thread
    /// starts nothing, as [`fork_process`] checks.
    pub(crate) fn log_release_failure(&self, error: PortError) {
        let record = format!("nvx-time: {error}\n");
        self.ops
            .write_kernel_log(&[DIAGNOSTIC_PRIORITY, record.as_bytes()].concat());
    }

    /// Emits one serialized violation record to every ABI destination.
    pub(crate) fn emit_event(
        &self,
        code: ViolationCode,
        source: EventSource,
        phase: Phase,
        detail: &[u8],
    ) {
        let _guard = self.report_lock.lock().ok();
        self.emit_event_unlocked(code, source, phase, detail);
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
        self.ops
            .write_kernel_log(&[KERNEL_LOG_PRIORITY, line.as_bytes()].concat());
        if let Ok(ports) = self.open_ports() {
            if self.async_output.load(Ordering::Relaxed) {
                ports.write_u8(Port::PortbData, b'\n');
            }
            for byte in line.bytes() {
                ports.write_u8(Port::PortbData, byte);
            }
            // The event goes on after a failure, which keeps the thread from starting anything,
            // as `fork_process` checks.
            if let Err(error) = ports.release() {
                self.log_release_failure(error);
            }
        }
        let separator: &[u8] = if self.async_output.load(Ordering::Relaxed) {
            b"\n"
        } else {
            b""
        };
        self.ops.write_console(separator, line.as_bytes());
        let _ = count_violation();
    }

    /// Reports a conformance failure, and powers off unless report-only or strict.
    pub(crate) fn conformance_failure(&self, code: ViolationCode, phase: Phase, detail: &[u8]) {
        let _guard = self.report_lock.lock().ok();
        self.emit_event_unlocked(code, EventSource::Conformance, phase, detail);
        self.stop(PowerOffStatus::Conformance, code, detail);
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
    Ports(PortError),
    /// `fork` failed.
    Fork(Errno),
}

impl Display for SpawnError {
    /// Writes the diagnostic: the description of `fork`'s `errno`, or why the thread may hold
    /// port permission, such as `ioperm revoke: Cannot allocate memory`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Ports(error) => error.fmt(formatter),
            Self::Fork(error) => formatter.write_str(error.desc()),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Forks the process, as [`sys::fork`] does, once the calling thread holds no port permission,
/// which the child would inherit.
pub(crate) fn fork_process() -> Result<ForkResult, SpawnError> {
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

/// Opens or creates a runtime lock file and takes its exclusive advisory lock.
pub(crate) fn lock_file(path: &CStr) -> Result<FileLock, Errno> {
    make_runtime_directories();
    sys::lock_file(path, PRIVATE_FILE_MODE)
}

/// Loads and parses the published time state.
pub(crate) fn state_load() -> Result<TimeState, Errno> {
    let text = sys::read_file(STATE_PATH)?;
    TimeState::parse(&text).ok_or(Errno::EINVAL)
}

/// Atomically persists one complete time state.
pub(crate) fn state_store(state: &TimeState) -> Result<(), Errno> {
    replace_file(STATE_PATH, STATE_TEMP_PATH, state.to_string().as_bytes())
}

/// Applies one read-modify-write update to the time state under its lock.
pub(crate) fn state_apply(update: impl FnOnce(&mut TimeState)) -> Result<(), Errno> {
    let _lock = lock_file(STATE_LOCK_PATH)?;
    let mut state = state_load().unwrap_or_default();
    update(&mut state);
    state_store(&state)
}

/// Increments the persisted violation counter when state exists.
fn count_violation() -> Result<(), Errno> {
    if !sys::path_exists(STATE_PATH) {
        return Ok(());
    }
    state_apply(|state| state.violations = state.violations.wrapping_add(1))
}

/// Loads validated TSC and LAPIC rates from state.
pub(crate) fn rates_load() -> Result<(u64, u64), Errno> {
    let state = state_load()?;
    if state.tsc_hz == 0 || state.lapic_hz == 0 {
        Err(Errno::EINVAL)
    } else {
        Ok((state.tsc_hz, state.lapic_hz))
    }
}

/// Saves the stall detectors' settings in the suppression record, and suppresses them for a
/// snapshot capture; see [`Suppression::save`]. A record that exists already, of a pending
/// capture, is `EEXIST`.
pub(crate) fn suppression_save() -> Result<(), SaveError> {
    make_runtime_directories();
    SUPPRESSION.save()
}

/// Why a save of the stall detectors' settings failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
/// see [`Suppression::restore`]. A missing record succeeds when `missing_ok`.
pub(crate) fn suppression_restore(missing_ok: bool) -> Result<(), Vec<u8>> {
    make_runtime_directories();
    SUPPRESSION.restore(missing_ok)
}

/// The suppression of the stall detectors for a capture: the detectors, the record of their
/// saved settings, which exists while a capture is pending, and the lock that serializes the
/// record's saves and restores, so that each restores only the settings that a save recorded.
struct Suppression<'a> {
    /// The detectors, of which only the first must exist, and the value that suppresses each.
    detectors: &'a [(&'a CStr, i64)],
    /// The record of the detectors' saved settings.
    record: &'a CStr,
    /// The lock that serializes the saves and restores of the record.
    lock: &'a CStr,
}

/// The guest's suppression of its stall detectors.
const SUPPRESSION: Suppression<'static> = Suppression {
    detectors: &SUPPRESSED_DETECTORS,
    record: SUPPRESSION_PATH,
    lock: SUPPRESSION_LOCK_PATH,
};

impl Suppression<'_> {
    /// Saves the detectors' settings in the record, and suppresses the detectors, under the
    /// lock, so that no restore runs between its reads and its writes. A record that exists
    /// already, of a pending capture, is `EEXIST`. A save that fails before it created the
    /// record changes nothing; one that fails after undoes its changes and removes the record,
    /// or keeps the record if it can't.
    fn save(&self) -> Result<(), SaveError> {
        self.save_with(sys::write_number)
    }

    /// Implements [`save`](Self::save), with `write` writing a detector's setting.
    fn save_with(
        &self,
        mut write: impl FnMut(&CStr, i64) -> Result<(), Errno>,
    ) -> Result<(), SaveError> {
        let _lock = sys::lock_file(self.lock, PRIVATE_FILE_MODE).map_err(SaveError::Undone)?;
        let mut saved = Vec::new();
        for (index, &(path, suppressed)) in self.detectors.iter().enumerate() {
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
        let record =
            CreatedFile::create(self.record, true, PRIVATE_FILE_MODE).map_err(SaveError::Undone)?;
        record.write_all(&text).map_err(SaveError::Undone)?;
        // As in the C program, a record that fails to close stays, and the suppression goes on.
        let _ = record.keep();
        for (written, &(path, _, suppressed)) in saved.iter().enumerate() {
            if let Err(error) = write(path, suppressed) {
                // The record stays unless every earlier write is undone, so that a restore can
                // still restore the settings.
                let mut undone = true;
                for &(path, value, _) in &saved[..written] {
                    undone &= write(path, value).is_ok();
                }
                if undone && sys::unlink(self.record).is_ok() {
                    return Err(SaveError::Undone(error));
                }
                return Err(SaveError::Kept(error));
            }
        }
        Ok(())
    }

    /// Restores the detectors' settings from the record, and removes it, under the lock. A
    /// missing record succeeds when `missing_ok`.
    fn restore(&self, missing_ok: bool) -> Result<(), Vec<u8>> {
        let _lock = sys::lock_file(self.lock, PRIVATE_FILE_MODE)
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
        sys::unlink(self.record)
            .map_err(|error| describe(b"remove /run/nvx/time-suppression: ", error))
    }
}

/// Reads a byte buffer using one scalar four-byte port access per chunk.
pub(crate) fn portb_read(ports: &PortAccess<'_>, port: Port, buffer: &mut [u8]) {
    for chunk in buffer.chunks_mut(size_of::<u32>()) {
        let bytes = ports.read_u32(port).to_ne_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

/// Locks the shared port-window transaction.
pub(crate) fn lock_portb() -> Result<FileLock, Errno> {
    lock_file(PORTB_LOCK_PATH)
}

/// Formats a failure of [`lock_portb`]: `portb lock: `, then the description of `error`.
pub(crate) fn portb_lock_failure(error: Errno) -> Vec<u8> {
    describe(b"portb lock: ", error)
}

/// Latches and decodes one host time sample: brackets the selector write with TSC reads, then
/// reads the realtime clock between two more, with which the guest pairs the sample's guest TSC,
/// before it reads the time window.
pub(crate) fn take_sample(
    ports: &PortAccess<'_>,
    ops: &dyn PrivilegedOps,
) -> Result<(TimeSample, GuestReads), TimeSampleError> {
    let mut bytes = [0_u8; SAMPLE_SIZE];
    let before = ops.read_tsc();
    ports.write_u8(Port::PortbStatus, SELECT_TIME_SAMPLE);
    let after = ops.read_tsc();
    let realtime_ns = ops.realtime_ns();
    let end = ops.read_tsc();
    portb_read(ports, Port::PortbWindow, &mut bytes);
    let half_cycles = end.saturating_sub(after).div_ceil(2);
    let reads = GuestReads {
        before,
        after,
        realtime_ns,
        realtime_tsc: after.saturating_add(half_cycles),
        realtime_half_cycles: half_cycles,
    };
    Ok((TimeSample::parse(&bytes)?, reads))
}

/// Narrowest valid sample observed during one attempt group, and the frequency correction that
/// its host clock correction asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BestSample {
    /// Guest-to-host offset in nanoseconds.
    pub(crate) theta: i64,
    /// Pairing uncertainty in nanoseconds.
    pub(crate) epsilon: i64,
    /// The discipline of host UTC that the sample published.
    pub(crate) host: HostClock,
    /// The frequency correction, in scaled ppm, that runs the wall clock at host UTC's rate.
    pub(crate) frequency: i64,
    /// Whether Linux's ±500 ppm limit clamped `frequency`.
    pub(crate) limited: bool,
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
    /// The thread could not take, or give up, the permission for the ports.
    Ports(PortError),
    /// The guest could not read the declared TSC rate, with which it pairs a sample, from the
    /// frequency MSR: the error, or the implausible rate that it read.
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
            Self::Ports(error) => error.fmt(formatter),
            Self::TscRate(Err(error)) => write!(
                formatter,
                "MSR {:#010x} read: {}",
                msr::HV_TSC_FREQUENCY,
                error.desc()
            ),
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

/// Returns the rates with which the guest's realtime clock counts its TSC: the declared TSC rate,
/// and the kernel's frequency correction.
pub(crate) fn clock_rates(runtime: &Runtime<'_>) -> Result<ClockRates, SampleError> {
    Ok(ClockRates {
        tsc_hz: runtime.tsc_hz()?,
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
    let ports = runtime.open_ports().map_err(SampleError::Ports)?;
    let selected = select_sample(&ports, runtime.ops, attempts, bound_ns, generation, rates);
    ports.release().map_err(SampleError::Ports)?;
    selected
}

/// Selects the lowest-uncertainty valid sample through `ports`, pairing each with the guest's
/// clocks at `rates`, as [`best_sample`] does.
pub(crate) fn select_sample(
    ports: &PortAccess<'_>,
    ops: &dyn PrivilegedOps,
    attempts: usize,
    bound_ns: i64,
    generation: u32,
    rates: ClockRates,
) -> Result<(SampleResult, BestSample), SampleError> {
    let status = ports.read_u8(Port::PortbStatus);
    if status & PORTB_TIME_WINDOW == 0 {
        return Err(SampleError::NoTimeWindow(status));
    }
    let mut best: Option<BestSample> = None;
    let mut invalid = None;
    for _ in 0..attempts {
        let (sample, reads) = match take_sample(ports, ops) {
            Ok(taken) => taken,
            Err(error) => {
                invalid = Some(error);
                continue;
            }
        };
        if sample.generation != generation {
            return Err(SampleError::Generation {
                sample: sample.generation,
                expected: generation,
            });
        }
        let (theta, epsilon) = match decoders::pair_sample(&sample, &reads, rates) {
            Ok(pairing) => pairing,
            Err(error) => {
                invalid = Some(error);
                continue;
            }
        };
        let (frequency, limited) = decoders::guest_frequency(sample.host.correction, rates.tsc_hz);
        let candidate = BestSample {
            theta,
            epsilon,
            host: sample.host,
            frequency,
            limited,
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
pub(crate) fn step_clock(runtime: &Runtime<'_>, offset_ns: i64) -> Result<(), Errno> {
    let (seconds, nanoseconds) = decoders::split_offset(offset_ns);
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
        decoders::frequency_hold(0, sample.epsilon, sample.frequency, sample.host.error_us()),
    )
}

/// Takes the initial sample within the bound, steps realtime to it, and holds the frequency
/// correction that the host's clock correction asks for.
pub(crate) fn initial_sample(runtime: &Runtime<'_>) -> Result<BestSample, Vec<u8>> {
    let (result, sample) = {
        let _lock = lock_portb().map_err(portb_lock_failure)?;
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

#[cfg(test)]
pub(crate) mod tests;
