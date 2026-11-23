//! ARM conformance leaves over the canonical phase, report and time-sample implementation.

use std::time::{Duration, Instant};

pub(crate) use super::checks_shared::{
    BootLog, Checks, Failure, TIMER_LIST_WAIT, Verdict, record_check, wait_for_checks,
};
use super::checks_shared::{
    TIMER_LIST_POLL, check_debug_watchdogs, check_platform, read_boot_log, read_kernel_file,
};
use super::cpu_mask::CpuMask;
use super::events::{CheckId, DETAIL_MAX, ViolationCode};
use super::system::{self, Runtime, SampleResult};
use crate::guest::counter::{ClockContext, ClockState};
use crate::guest::kernel_text::{self, ClockEventState, TickMode, TimerList};
use crate::guest::sys::{self, kernel};
use crate::protocol::cmdline;

/// Requires the actual dense online/possible CPU inventory to match the admitted ARM capacity.
fn inventory_verdict(possible: usize, online: CpuMask, binding: &ClockContext) -> Verdict {
    let capacity = binding
        .arm_capacity()
        .map_err(|error| Failure::new(CheckId::C3, error.to_string()))? as usize;
    if possible != capacity || !online.iter().eq(0..capacity) {
        return Err(Failure::new(
            CheckId::C6,
            format!("online CPUs {online}, possible={possible}, ARM time capacity={capacity}"),
        ));
    }
    Ok(())
}

/// Admits only Linux's architectural generic-counter clocksource.
fn clocksource_verdict(current: &[u8]) -> Verdict {
    if current.trim_ascii() != b"arch_sys_counter" {
        return Err(Failure::new(
            CheckId::C6,
            [b"current clocksource is ".as_slice(), current.trim_ascii()].concat(),
        ));
    }
    Ok(())
}

/// Refuses x86 timer controls and a conflicting explicit ARM clocksource.
fn cmdline_verdict(text: &[u8]) -> Verdict {
    let text = text.strip_suffix(b"\n").unwrap_or(text);
    for parameter in cmdline::parameters(text) {
        let x86_only = [
            "tsc",
            "tsc_early_khz",
            "lapic_timer_hz",
            "nolapic",
            "nolapic_timer",
            "hpet",
        ]
        .iter()
        .any(|name| parameter.is_named(name))
            || parameter.name.starts_with(b"notsc");
        let wrong_source = parameter.is_named("clocksource")
            && parameter.value != Some(b"arch_sys_counter".as_slice());
        if x86_only || wrong_source {
            let mut quoted = parameter.name.to_vec();
            if let Some(value) = parameter.value {
                quoted.push(b'=');
                quoted.extend_from_slice(value);
            }
            quoted.truncate(DETAIL_MAX);
            return Err(Failure::new(
                CheckId::C9,
                [b"command line contains ".as_slice(), &quoted].concat(),
            ));
        }
    }
    Ok(())
}

/// Requires each online CPU's architectural timer to run in high-resolution one-shot mode.
fn timer_verdict(timers: &TimerList, online: CpuMask) -> Verdict {
    if online.is_empty() {
        return Err(Failure::new(
            CheckId::C7,
            "no online CPUs for ARM timer check",
        ));
    }
    for cpu in online.iter() {
        let tick = timers
            .tick_devices
            .get(&cpu)
            .ok_or_else(|| Failure::new(CheckId::C7, format!("CPU {cpu} has no tick device")))?;
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
            return Err(Failure::new(
                CheckId::C7,
                format!("CPU {cpu} lacks the active high-resolution architectural timer: {tick:?}"),
            ));
        }
    }
    Ok(())
}

/// Retries an incomplete timer inventory only until the supplied wait expires.
pub(crate) fn check_timer_list(online: CpuMask, wait: Duration) -> Verdict {
    let deadline = Instant::now() + wait;
    loop {
        let text = read_kernel_file(CheckId::C7, kernel::TIMER_LIST)?;
        match timer_verdict(&TimerList::parse(&text), online) {
            Ok(()) => return Ok(()),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(TIMER_LIST_POLL),
        }
    }
}

/// Pins to every online CPU to check binding stability, forward counter reads and real samples.
fn check_counters(runtime: &Runtime<'_>, binding: &ClockContext, online: CpuMask) -> Verdict {
    let failure = |message| Failure::new(CheckId::C3, message);
    let _affinity =
        sys::AffinityGuard::save().map_err(|error| failure(format!("save affinity: {error}")))?;
    for cpu in online.iter() {
        sys::pin_current_thread(cpu).map_err(|error| failure(format!("pin CPU {cpu}: {error}")))?;
        let current = runtime
            .validated_arm_clock_context()
            .map_err(|error| failure(format!("CPU {cpu} ARM binding: {error}")))?;
        if current != *binding {
            return Err(failure(format!("CPU {cpu} ARM clock binding changed")));
        }
        let before = runtime.ops.read_counter();
        let after = runtime.ops.read_counter();
        if after < before {
            return Err(failure(format!(
                "CPU {cpu} counter moved backward: {before}..{after}"
            )));
        }
        let (result, sample) = {
            let lock = system::lock_portb()
                .map_err(|error| failure(format!("platform window lock: {error}")))?;
            runtime
                .counter_scale_in_window(&lock)
                .map_err(|error| failure(error.to_string()))?;
            system::best_sample(
                runtime,
                system::SAMPLE_ATTEMPTS,
                system::SAMPLE_BOUND_NS,
                runtime.generation(),
            )
            .map_err(|error| failure(error.to_string()))?
        };
        if result == SampleResult::Wide {
            return Err(Failure::new(
                CheckId::C3,
                system::wide_sample(sample.epsilon),
            ));
        }
    }
    let current = runtime
        .validated_arm_clock_context()
        .map_err(|error| failure(error.to_string()))?;
    if current != *binding {
        return Err(failure(
            "ARM clock binding changed after sampling".to_owned(),
        ));
    }
    Ok(())
}

/// Records actual ARM cold-boot conformance, without admitting capture or restore.
pub(crate) fn run_boot_checks(
    runtime: &Runtime<'_>,
    checks: &mut Checks,
    log: Option<BootLog<'_>>,
) {
    for verdict in [check_platform(), check_debug_watchdogs()] {
        checks.record(runtime, verdict);
    }
    let source = read_kernel_file(CheckId::C6, kernel::CURRENT_CLOCKSOURCE)
        .and_then(|source| clocksource_verdict(&source));
    checks.record(runtime, source);
    let command =
        read_kernel_file(CheckId::C9, kernel::COMMAND_LINE).and_then(|text| cmdline_verdict(&text));
    checks.record(runtime, command);
    match read_boot_log(log) {
        Err(failure) => {
            checks.record(runtime, Err(failure));
        }
        Ok(Some(log)) => {
            if !kernel_text::contains(
                log,
                b"clocksource: Switched to clocksource arch_sys_counter",
            ) {
                checks.fail(
                    runtime,
                    CheckId::C4,
                    "ARM counter selection is absent from the boot log",
                );
            }
            for line in log.split(|byte| *byte == b'\n') {
                if kernel_text::contains(line, b"Found insecure W+X mapping") {
                    checks.fail_code(
                        runtime,
                        ViolationCode::KernelWx,
                        &line[..line.len().min(DETAIL_MAX)],
                    );
                    break;
                }
            }
        }
        Ok(None) => {}
    }
    let binding = match runtime.validated_arm_clock_context() {
        Ok(binding) => binding,
        Err(error) => {
            checks.fail(runtime, CheckId::C3, error.to_string());
            return;
        }
    };
    checks.clock = ClockState::Ready(binding.clone());
    if checks.record(
        runtime,
        inventory_verdict(checks.possible, checks.online, &binding),
    ) {
        checks.record(runtime, check_counters(runtime, &binding, checks.online));
        checks.record(runtime, check_timer_list(checks.online, TIMER_LIST_WAIT));
    }
}

#[cfg(test)]
mod tests;
