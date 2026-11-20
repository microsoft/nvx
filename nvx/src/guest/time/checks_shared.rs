//! Shared check-phase evidence, reporting and opaque status waits.

use std::ffi::CStr;
use std::thread;
use std::time::{Duration, Instant};

use super::Phase;
use super::cpu_mask::CpuMask;
#[cfg(target_arch = "x86_64")]
use super::decoders::CpuInfo;
use super::events::{CheckId, ViolationCode};
use super::state::{CheckState, CheckStatus, StateReport};
use super::system::{Runtime, report_load, report_or_pending, report_try_apply_until};
#[cfg(target_arch = "aarch64")]
use crate::guest::counter::{ClockState, DeclaredGuestIsa, PublishedRateContext};
#[cfg(target_arch = "x86_64")]
use crate::guest::sys::ProcessId;
use crate::guest::sys::{self, Errno, kernel};

/// Maximum wait for a pending published conformance phase.
pub(super) const STATUS_WAIT: Duration = Duration::from_secs(30);
/// Poll interval while waiting for a published phase to finish.
pub(super) const STATUS_POLL: Duration = Duration::from_millis(10);
/// Maximum wait for the kernel to expose its timer devices.
pub(crate) const TIMER_LIST_WAIT: Duration = Duration::from_millis(200);
/// Poll interval while waiting for the timer inventory.
pub(super) const TIMER_LIST_POLL: Duration = Duration::from_millis(10);

/// One conformance failure: the check that failed, and its diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Failure {
    /// Stable check identifier used by the violation diagnostic.
    pub(super) id: CheckId,
    /// Diagnostic bytes, which need not be UTF-8.
    pub(super) detail: Vec<u8>,
}

impl Failure {
    /// Binds a diagnostic to its conformance check without publishing it yet.
    pub(super) fn new(id: CheckId, detail: impl Into<Vec<u8>>) -> Self {
        Self {
            id,
            detail: detail.into(),
        }
    }
}

/// A completed check, either successful or retaining one precise failure.
pub(crate) type Verdict = Result<(), Failure>;

/// Mutable evidence and failure count for one conformance-check phase.
#[derive(Clone)]
pub(crate) struct Checks {
    /// Lifecycle phase whose evidence this collection records.
    pub(crate) phase: Phase,
    /// Number of failures already reported for this phase.
    pub(crate) failures: usize,
    /// Count of possible CPUs, read independently of the online mask.
    pub(crate) possible: usize,
    /// Actual online CPU set read from the kernel.
    pub(crate) online: CpuMask,
    /// x86 processor inventory used by the CPUID/CPU-info checks.
    #[cfg(target_arch = "x86_64")]
    pub(crate) cpuinfo: CpuInfo,
    /// Observed x86 TSC rate, zero until checked.
    #[cfg(target_arch = "x86_64")]
    pub(crate) tsc_hz: u64,
    /// Observed x86 local-APIC rate, zero until checked.
    #[cfg(target_arch = "x86_64")]
    pub(crate) lapic_hz: u64,
    /// ARM binding admission state, never reconstructed from a public report.
    #[cfg(target_arch = "aarch64")]
    pub(crate) clock: ClockState,
}

impl Checks {
    /// Starts with no CPU evidence or admitted clock.
    pub(crate) fn empty(phase: Phase) -> Self {
        Self {
            phase,
            failures: 0,
            possible: 0,
            online: CpuMask::EMPTY,
            #[cfg(target_arch = "x86_64")]
            cpuinfo: CpuInfo::default(),
            #[cfg(target_arch = "x86_64")]
            tsc_hz: 0,
            #[cfg(target_arch = "x86_64")]
            lapic_hz: 0,
            #[cfg(target_arch = "aarch64")]
            clock: ClockState::Uninitialized,
        }
    }

    /// Loads nonempty possible/online CPU sets, leaving clock admission to platform checks.
    pub(crate) fn load(phase: Phase) -> Result<Self, Errno> {
        let cpus = |path| {
            let list = sys::read_line(path)?;
            CpuMask::parse(&list)
                .filter(|cpus| !cpus.is_empty())
                .ok_or(Errno::EINVAL)
        };
        Ok(Self {
            possible: cpus(kernel::POSSIBLE_CPUS)?.len(),
            online: cpus(kernel::ONLINE_CPUS)?,
            ..Self::empty(phase)
        })
    }

    /// Counts and reports a failure through the runtime's enforcing/report-only policy.
    pub(crate) fn fail_code(
        &mut self,
        runtime: &Runtime<'_>,
        code: ViolationCode,
        detail: impl AsRef<[u8]>,
    ) -> bool {
        self.failures += 1;
        runtime.conformance_failure(code, self.phase, detail.as_ref());
        false
    }

    /// Reports one numbered conformance failure and returns false.
    pub(crate) fn fail(
        &mut self,
        runtime: &Runtime<'_>,
        id: CheckId,
        detail: impl AsRef<[u8]>,
    ) -> bool {
        self.fail_code(runtime, ViolationCode::Conformance(id), detail)
    }

    /// Reports a failed verdict once, or returns true for success.
    pub(crate) fn record(&mut self, runtime: &Runtime<'_>, verdict: Verdict) -> bool {
        match verdict {
            Ok(()) => true,
            Err(Failure { id, detail }) => self.fail(runtime, id, detail),
        }
    }

    /// Publishes evidence and timing without promoting uninitialized ARM clock metadata.
    fn apply_report(
        &self,
        state: &mut StateReport,
        report_only: bool,
        elapsed_us: i64,
        cpu_us: i64,
    ) -> Result<(), Errno> {
        let phase = self.phase;
        if phase == Phase::Boot {
            #[cfg(target_arch = "x86_64")]
            state.record_x86_observations(self.tsc_hz, self.lapic_hz);
            #[cfg(target_arch = "aarch64")]
            {
                let context = match &self.clock {
                    ClockState::Uninitialized => {
                        PublishedRateContext::Uninitialized(DeclaredGuestIsa::Aarch64)
                    }
                    ClockState::Ready(binding) => {
                        if binding.declared_isa() != DeclaredGuestIsa::Aarch64 {
                            return Err(Errno::EINVAL);
                        }
                        PublishedRateContext::Validated(binding)
                    }
                };
                state
                    .record_clock(&self.clock, context)
                    .map_err(|_| Errno::EINVAL)?;
            }
        }
        let failures = report_only.then_some(self.failures as u64);
        let status = if failures.is_some_and(|failures| failures > 0) {
            CheckStatus::Fail
        } else {
            CheckStatus::Ok
        };
        let generation = if phase == Phase::Boot {
            0
        } else {
            state.generation
        };
        if let Some(check) = state.check_mut(phase) {
            *check = CheckState {
                status: Some(status),
                cpus: self.online.len() as u64,
                elapsed_us,
                cpu_us,
                generation,
                failures,
            };
        }
        Ok(())
    }
}

/// A preallocated kernel-log buffer supplied by the existing cold-boot orchestration.
pub(crate) struct BootLog<'a> {
    /// Size reported by the kernel before the buffer was allocated.
    pub(crate) size: i64,
    /// Caller-owned buffer, absent when allocation failed.
    pub(crate) buffer: Option<&'a mut [u8]>,
}

/// Publishes one phase without interpreting the opaque report as hardware admission.
pub(crate) fn record_check(
    runtime: &Runtime<'_>,
    checks: &Checks,
    elapsed_us: i64,
    cpu_us: i64,
) -> Result<(), Errno> {
    let phase = checks.phase;
    if phase.check_index().is_none() {
        return Err(Errno::EINVAL);
    }
    report_try_apply_until(runtime.deadline, |state| {
        checks.apply_report(state, runtime.report_only(), elapsed_us, cpu_us)
    })
}

/// Waits for published checks to finish; the boolean means completed, not necessarily passed.
pub(crate) fn wait_for_checks(boot_only: bool) -> Result<(StateReport, bool), Errno> {
    wait_for_checks_until(boot_only, None)
}

/// Bounds the status wait by both the caller's deadline and the normal status timeout.
pub(crate) fn wait_for_checks_until(
    boot_only: bool,
    deadline: Option<Instant>,
) -> Result<(StateReport, bool), Errno> {
    wait_for_checks_until_from(boot_only, deadline, report_load)
}

/// Polls the same published-report owner with its ISA-specific read-recovery contract.
fn wait_for_checks_until_from(
    boot_only: bool,
    deadline: Option<Instant>,
    mut read: impl FnMut() -> Result<StateReport, Errno>,
) -> Result<(StateReport, bool), Errno> {
    let limit = Instant::now() + STATUS_WAIT;
    let deadline = deadline.map_or(limit, |deadline| deadline.min(limit));
    loop {
        let state = report_or_pending(read())?;
        if !state.pending(boot_only) {
            return Ok((state, true));
        }
        if Instant::now() >= deadline {
            return Ok((state, false));
        }
        thread::sleep(STATUS_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Reads kernel evidence, retaining its path and errno in the named check's diagnostic.
pub(crate) fn read_kernel_file(id: CheckId, path: &CStr) -> Result<Vec<u8>, Failure> {
    sys::read_file(path).map_err(|error| {
        Failure::new(
            id,
            format!("read {}: {}", path.to_string_lossy(), error.desc()),
        )
    })
}

/// Reads into the supplied buffer or reports its allocation/read failure as a boot-log check.
pub(crate) fn read_boot_log<'a>(log: Option<BootLog<'a>>) -> Result<Option<&'a [u8]>, Failure> {
    let Some(BootLog { size, buffer }) = log else {
        return Ok(None);
    };
    let buffer = buffer.ok_or_else(|| {
        Failure::new(
            CheckId::C4,
            format!("no buffer for the {size}-byte kernel log"),
        )
    })?;
    let count = sys::kernel::read_kernel_log(buffer).map_err(|error| {
        Failure::new(
            CheckId::C4,
            format!("read the kernel log: {}", error.desc()),
        )
    })?;
    Ok(Some(&buffer[..count]))
}

/// Checks shared VMBus, frequency-scaling and RCU-stall invariants on both guest ISAs.
pub(crate) fn check_platform() -> Verdict {
    if sys::path_exists(kernel::VMBUS) {
        return Err(Failure::new(
            CheckId::C8,
            format!("{} exists", kernel::VMBUS.to_string_lossy()),
        ));
    }
    if sys::path_exists(kernel::CPU_FREQUENCY_POLICY) {
        return Err(Failure::new(CheckId::C8, "cpufreq policy0 exists"));
    }
    let suppress = sys::read_number::<i64>(kernel::RCU_STALL_SUPPRESS).unwrap_or(-1);
    if suppress != 0 {
        return Err(Failure::new(
            CheckId::C8,
            format!("rcu_cpu_stall_suppress is {suppress}"),
        ));
    }
    let timeout = sys::read_number::<i64>(kernel::RCU_STALL_TIMEOUT).unwrap_or(-1);
    if timeout != super::system::DEFAULT_STALL_TIMEOUT_S {
        return Err(Failure::new(
            CheckId::C8,
            format!("rcu_cpu_stall_timeout is {timeout}"),
        ));
    }
    Ok(())
}

/// Checks the enabled debug detectors when the kernel exposes their controls.
pub(crate) fn check_debug_watchdogs() -> Verdict {
    let setting =
        |path| sys::path_exists(path).then(|| sys::read_number::<i64>(path).unwrap_or(-1));
    let (soft, hung) = (
        setting(kernel::SOFT_WATCHDOG),
        setting(kernel::HUNG_TASK_TIMEOUT),
    );
    if soft.is_none() && hung.is_none() {
        return Ok(());
    }
    if soft != Some(1) {
        return Err(Failure::new(
            CheckId::C11,
            format!("soft_watchdog is {}", soft.unwrap_or(-1)),
        ));
    }
    match hung {
        Some(timeout) if timeout != 0 => Ok(()),
        _ => Err(Failure::new(
            CheckId::C11,
            format!("hung_task_timeout_secs is {}", hung.unwrap_or(-1)),
        )),
    }
}

#[cfg(target_arch = "x86_64")]
/// Resolves the published daemon PID only when its live process name matches.
pub(crate) fn daemon_pid() -> Option<ProcessId> {
    let pid = sys::read_number::<i64>(super::system::DAEMON_PID_PATH).ok()?;
    let process = ProcessId::from_positive(pid).filter(|_| pid > 1)?;
    if !sys::process_exists(process) {
        return None;
    }
    is_daemon_name(&sys::process_name(process).ok()?).then_some(process)
}

#[cfg(target_arch = "x86_64")]
/// Checks the complete procfs command name, including its terminating newline.
pub(super) fn is_daemon_name(comm: &[u8]) -> bool {
    comm.strip_suffix(b"\n") == Some(super::system::DAEMON_NAME.to_bytes())
}

#[cfg(test)]
mod tests;
