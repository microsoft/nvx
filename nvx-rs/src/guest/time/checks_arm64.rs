//! Architectural-counter and Linux timer checks for the shared time lifecycle.

use std::time::Instant;

use super::Phase;
use super::deferred::{TIMER_LIST_POLL, TIMER_LIST_WAIT};
use super::events::{CheckId, ViolationCode};
use super::sample::arm64::{Clock, Error};
use super::state::CheckState;
use super::system::{self, Runtime, SampleResult};
use crate::guest::kernel_text::{self, ClockEventState, TickMode, TimerList};
use crate::guest::sys::{self, Errno, aarch64, kernel};

pub(crate) use super::state::wait_for_checks;

/// Evidence passed from architectural checks to the common phase publisher.
pub(crate) struct Checks {
    /// Phase whose failures are being reported.
    phase: Phase,
    /// Actual failed checks, including report-only failures.
    pub(crate) failures: usize,
    /// Actual number of online processors checked.
    capacity: u64,
    /// Architected-counter Hz, kept in the compatible state format's frequency slot.
    pub(crate) tsc_hz: u64,
    /// No local APIC exists on ARM; the compatible state slot remains zero.
    pub(crate) lapic_hz: u64,
}

impl Checks {
    /// Starts one phase without claiming that any architectural check passed.
    pub(crate) fn empty(phase: Phase) -> Self {
        Self {
            phase,
            failures: 0,
            capacity: 0,
            tsc_hz: 0,
            lapic_hz: 0,
        }
    }

    /// Reads the actual online processor inventory before deferred checks.
    pub(crate) fn load(phase: Phase) -> Result<Self, Errno> {
        let text = sys::read_file(kernel::ONLINE_CPUS)?;
        let cpus = kernel_text::parse_cpu_list(text.strip_suffix(b"\n").unwrap_or(&text))
            .ok_or(Errno::EPROTO)?;
        let capacity = cpus.into_iter().flatten().count();
        if capacity == 0 || capacity > 8 {
            return Err(Errno::EPROTO);
        }
        Ok(Self {
            capacity: capacity as u64,
            ..Self::empty(phase)
        })
    }

    /// Counts and reports the same conformance failure as the x86 lifecycle.
    pub(crate) fn fail(
        &mut self,
        runtime: &Runtime<'_>,
        id: CheckId,
        detail: impl AsRef<[u8]>,
    ) -> bool {
        self.failures += 1;
        runtime.conformance_failure(ViolationCode::Conformance(id), self.phase, detail.as_ref());
        false
    }
}

/// Publishes actual failures through the shared phase/state implementation.
pub(crate) fn record_check(
    runtime: &Runtime<'_>,
    checks: &Checks,
    elapsed_us: i64,
    cpu_us: i64,
    counter_hz: u64,
    timer_hz: u64,
) -> Result<(), Errno> {
    system::record_check(
        checks.phase,
        CheckState::completed(
            checks.capacity,
            elapsed_us,
            cpu_us,
            runtime.report_only().then_some(checks.failures as u64),
        ),
        counter_hz,
        timer_hz,
    )
}

/// Checks architectural mechanisms, not a vendor name or a raw CPU-profile allowlist.
pub(crate) fn run_boot_checks(runtime: &Runtime<'_>, checks: &mut Checks) {
    let clock = match Clock::open() {
        Ok(clock) => clock,
        Err(error) => {
            let id = if matches!(error, Error::Clocksource(_)) {
                CheckId::C6
            } else {
                CheckId::C3
            };
            checks.fail(runtime, id, error.to_string());
            return;
        }
    };
    checks.tsc_hz = clock.binding.counter_hz;
    let online = sys::read_file(kernel::ONLINE_CPUS)
        .map_err(|error| error.to_string())
        .and_then(|text| online_cpus(&text, clock.binding.capacity));
    match online {
        Ok(cpus) => {
            checks.capacity = cpus.len() as u64;
            if let Err(error) = check_counters(runtime, &clock, &cpus) {
                checks.fail(runtime, CheckId::C3, error);
            }
        }
        Err(error) => {
            checks.fail(runtime, CheckId::C6, error);
        }
    }
    if let Err(error) = check_timers(clock.binding.capacity) {
        checks.fail(runtime, CheckId::C7, error);
    }
    let stall = || -> Result<(), String> {
        let suppressed = sys::read_number::<u32>(kernel::RCU_STALL_SUPPRESS)
            .map_err(|error| format!("RCU stall suppression: {error}"))?;
        let timeout = sys::read_number::<u32>(kernel::RCU_STALL_TIMEOUT)
            .map_err(|error| format!("RCU stall timeout: {error}"))?;
        if suppressed != 0 || timeout != system::DEFAULT_STALL_TIMEOUT_S as u32 {
            return Err(format!(
                "RCU stall policy suppression={suppressed} timeout={timeout}, expected 0/21"
            ));
        }
        Ok(())
    };
    if let Err(error) = stall() {
        checks.fail(runtime, CheckId::C8, error);
    }
    if let Err(error) = system::check_debug_watchdogs() {
        checks.fail(runtime, CheckId::C11, error);
    }
}

/// Checks every retained CPU's effective counter and shared-counter pairing.
fn check_counters(runtime: &Runtime<'_>, clock: &Clock, cpus: &[usize]) -> Result<(), String> {
    let _affinity =
        sys::AffinityGuard::save().map_err(|error| format!("save affinity: {error}"))?;
    for &cpu in cpus {
        sys::pin_current_thread(cpu).map_err(|error| format!("pin CPU {cpu}: {error}"))?;
        let frequency = aarch64::counter_frequency_hz()
            .map_err(|error| format!("CPU {cpu} CNTFRQ: {error}"))?;
        if frequency != clock.binding.counter_hz {
            return Err(format!(
                "CPU {cpu} CNTFRQ {frequency}, expected {}",
                clock.binding.counter_hz
            ));
        }
        let before = aarch64::virtual_counter();
        let after = aarch64::virtual_counter();
        if after < before {
            return Err(format!(
                "CPU {cpu} counter moved backward: {before}..{after}"
            ));
        }
        let (result, sample) = system::best_sample(
            runtime,
            system::SAMPLE_ATTEMPTS,
            system::SAMPLE_BOUND_NS,
            runtime.generation(),
        )
        .map_err(|error| format!("CPU {cpu} shared-counter pairing: {error}"))?;
        if result == SampleResult::Wide {
            return Err(String::from_utf8_lossy(&system::wide_sample(sample.epsilon)).into_owned());
        }
    }
    let current = Clock::open().map_err(|error| error.to_string())?;
    if current.binding != clock.binding {
        return Err("time binding changed after initial synchronization".into());
    }
    Ok(())
}

/// Validates Linux numbering against the capacity declared by the admitted machine binding.
fn online_cpus(text: &[u8], capacity: u32) -> Result<Vec<usize>, String> {
    let text = text.strip_suffix(b"\n").unwrap_or(text);
    let ranges = kernel_text::parse_cpu_list(text).ok_or("malformed online CPU list")?;
    let cpus = ranges.into_iter().flatten().collect::<Vec<_>>();
    if cpus != (0..capacity as usize).collect::<Vec<_>>() {
        return Err(format!(
            "online CPUs {cpus:?}, declared capacity {capacity}"
        ));
    }
    Ok(cpus)
}

/// Uses the standard Linux timer-list interface for every online architectural timer.
fn check_timers(capacity: u32) -> Result<(), String> {
    let deadline = Instant::now() + TIMER_LIST_WAIT;
    loop {
        let text =
            sys::read_file(kernel::TIMER_LIST).map_err(|error| format!("timer list: {error}"))?;
        let timers = TimerList::parse(&text);
        match check_timer_list(&timers, capacity) {
            Ok(()) => return Ok(()),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(TIMER_LIST_POLL),
        }
    }
}

/// Requires the same architectural high-resolution clock-event state on every CPU.
fn check_timer_list(timers: &TimerList, capacity: u32) -> Result<(), String> {
    for cpu in 0..capacity as usize {
        let tick = timers
            .tick_devices
            .get(&cpu)
            .ok_or_else(|| format!("CPU {cpu} has no tick device"))?;
        if tick.device.as_deref() != Some("arch_sys_timer")
            || tick.tick_mode() != Some(TickMode::Oneshot)
            || !matches!(
                tick.clock_event_state(),
                Some(ClockEventState::Oneshot | ClockEventState::OneshotStopped)
            )
            || timers
                .timer_bases
                .get(&cpu)
                .and_then(|base| base.hres_active)
                != Some(1)
        {
            return Err(format!(
                "CPU {cpu} lacks the active high-resolution architectural timer: {tick:?}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
