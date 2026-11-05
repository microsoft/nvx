//! The kernel's log, its auxiliary vector, the procfs and sysfs files that the time tools read
//! and write, and the system-wide calls with which `nvx-init` runs as PID 1: the nice value of a
//! process, and `sync`.
//!
//! `nix` wraps neither `syslog(2)`, which the C library calls `klogctl`, nor `getauxval`, nor
//! `setpriority`, so this module calls them through libc.

use std::ffi::{CStr, c_int};

use super::Errno;

/// The kernel's structured log: each read returns one record, and each write adds one.
pub(crate) const KERNEL_LOG_DEVICE: &CStr = c"/dev/kmsg";
/// The kernel's command line.
pub(crate) const COMMAND_LINE: &CStr = c"/proc/cmdline";
/// The kernel's description of each processor.
pub(crate) const CPU_INFORMATION: &CStr = c"/proc/cpuinfo";
/// The kernel's timers, clock-event devices, and tick devices.
pub(crate) const TIMER_LIST: &CStr = c"/proc/timer_list";
/// The CPUs that are online, as a CPU list.
pub(crate) const ONLINE_CPUS: &CStr = c"/sys/devices/system/cpu/online";
/// The CPUs that could ever be online, as a CPU list.
pub(crate) const POSSIBLE_CPUS: &CStr = c"/sys/devices/system/cpu/possible";
/// The TSC frequency in kHz that the kernel determined, when it exposes it.
pub(crate) const TSC_FREQUENCY_KHZ: &CStr = c"/sys/devices/system/cpu/cpu0/tsc_freq_khz";
/// The first frequency-scaling policy, present when the kernel scales CPU frequency.
pub(crate) const CPU_FREQUENCY_POLICY: &CStr = c"/sys/devices/system/cpu/cpufreq/policy0";
/// The clocksource that timekeeping uses.
pub(crate) const CURRENT_CLOCKSOURCE: &CStr =
    c"/sys/devices/system/clocksource/clocksource0/current_clocksource";
/// The clocksources that timekeeping could use.
pub(crate) const AVAILABLE_CLOCKSOURCES: &CStr =
    c"/sys/devices/system/clocksource/clocksource0/available_clocksource";
/// The VMBus bus, present when the kernel found Hyper-V's VMBus.
pub(crate) const VMBUS: &CStr = c"/sys/bus/vmbus";
/// Whether RCU suppresses its CPU stall warnings.
pub(crate) const RCU_STALL_SUPPRESS: &CStr =
    c"/sys/module/rcupdate/parameters/rcu_cpu_stall_suppress";
/// Seconds after which RCU reports a CPU stall.
pub(crate) const RCU_STALL_TIMEOUT: &CStr =
    c"/sys/module/rcupdate/parameters/rcu_cpu_stall_timeout";
/// Whether RCU uses normal rather than expedited grace periods.
pub(crate) const RCU_NORMAL: &CStr = c"/sys/kernel/rcu_normal";
/// How many RCU CPU stalls the kernel has reported.
pub(crate) const RCU_STALL_COUNT: &CStr = c"/sys/kernel/rcu_stall_count";
/// Whether the soft-lockup and hard-lockup detectors run.
pub(crate) const WATCHDOG: &CStr = c"/proc/sys/kernel/watchdog";
/// Whether the soft-lockup detector runs.
pub(crate) const SOFT_WATCHDOG: &CStr = c"/proc/sys/kernel/soft_watchdog";
/// Whether the hard-lockup detector runs.
pub(crate) const NMI_WATCHDOG: &CStr = c"/proc/sys/kernel/nmi_watchdog";
/// Seconds after which the lockup detectors report a lockup.
pub(crate) const WATCHDOG_THRESHOLD: &CStr = c"/proc/sys/kernel/watchdog_thresh";
/// Seconds after which the kernel reports a task that has not run as hung.
pub(crate) const HUNG_TASK_TIMEOUT: &CStr = c"/proc/sys/kernel/hung_task_timeout_secs";
/// The calling process's adjustment of its out-of-memory score.
pub(crate) const OOM_SCORE_ADJUSTMENT: &CStr = c"/proc/self/oom_score_adj";
/// The null device.
pub(crate) const NULL_DEVICE: &CStr = c"/dev/null";
/// The console device.
pub(crate) const CONSOLE_DEVICE: &CStr = c"/dev/console";

/// `syslog(2)`'s `SYSLOG_ACTION_READ_ALL`, which copies the whole log without consuming it.
const SYSLOG_ACTION_READ_ALL: c_int = 3;
/// `syslog(2)`'s `SYSLOG_ACTION_SIZE_BUFFER`, which returns the log buffer's size.
const SYSLOG_ACTION_SIZE_BUFFER: c_int = 10;

/// Returns the size in bytes of the kernel's log buffer.
pub(crate) fn kernel_log_size() -> Result<usize, Errno> {
    // SAFETY: `SYSLOG_ACTION_SIZE_BUFFER` reads neither the null buffer nor its length.
    let size = Errno::result(unsafe {
        nix::libc::klogctl(SYSLOG_ACTION_SIZE_BUFFER, std::ptr::null_mut(), 0)
    })?;
    usize::try_from(size).map_err(|_| Errno::EINVAL)
}

/// Copies as much of the kernel's log as fits into `buffer`, and returns the number of bytes
/// copied, without consuming the log.
pub(crate) fn read_kernel_log(buffer: &mut [u8]) -> Result<usize, Errno> {
    let length = c_int::try_from(buffer.len()).unwrap_or(c_int::MAX);
    // SAFETY: the kernel writes at most `length` bytes, which `buffer` holds, during the call.
    let count = Errno::result(unsafe {
        nix::libc::klogctl(SYSLOG_ACTION_READ_ALL, buffer.as_mut_ptr().cast(), length)
    })?;
    Ok(usize::try_from(count).map_or(0, |count| count.min(buffer.len())))
}

/// Returns whether the kernel mapped a vDSO into this process, which its auxiliary vector
/// reports.
pub(crate) fn has_vdso() -> bool {
    // SAFETY: `getauxval` reads the process's immutable auxiliary vector.
    unsafe { nix::libc::getauxval(nix::libc::AT_SYSINFO_EHDR) != 0 }
}

/// Sets the nice value of `process` to `nice`, or fails with `EINVAL` for `None` without a system
/// call: `setpriority` would read the process ID 0 as the calling thread.
///
/// Linux applies `setpriority`'s `PRIO_PROCESS` to the one thread whose ID it names: for a
/// process ID, the process's first thread, whose later threads and children start with its nice
/// value. Lowering a nice value needs `CAP_SYS_NICE`.
#[cfg(target_arch = "x86_64")]
pub(crate) fn set_nice(process: Option<super::ProcessId>, nice: c_int) -> Result<(), Errno> {
    let process = process.ok_or(Errno::EINVAL)?.raw().unsigned_abs();
    // SAFETY: `setpriority` reads only its integer arguments.
    Errno::result(unsafe { nix::libc::setpriority(nix::libc::PRIO_PROCESS, process, nice) })
        .map(drop)
}

/// Writes the modified data and metadata of every file system to its storage, as `sync(2)` does.
#[cfg(target_arch = "x86_64")]
pub(crate) fn sync() {
    nix::unistd::sync();
}

#[cfg(test)]
pub(super) mod tests;
