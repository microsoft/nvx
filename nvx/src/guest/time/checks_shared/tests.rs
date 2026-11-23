use super::*;
use crate::guest::counter::{ClockContext, ClockState, PublishedRateContext};

#[test]
fn published_wait_recovers_read_errors_with_the_existing_isa_contract() {
    for error in [Errno::ENOENT, Errno::EACCES, Errno::EINVAL, Errno::EIO] {
        let mut completed = StateReport::default();
        completed.check_mut(Phase::Boot).unwrap().status = Some(CheckStatus::Ok);
        assert!(!completed.pending(true));
        let mut reports = std::collections::VecDeque::from([Err(error), Ok(completed.clone())]);
        let result =
            wait_for_checks_until_from(true, Some(Instant::now() + Duration::from_secs(1)), || {
                reports.pop_front().expect("unexpected status read")
            });
        #[cfg(target_arch = "x86_64")]
        let recovers = true;
        #[cfg(target_arch = "aarch64")]
        let recovers = error == Errno::ENOENT;
        if recovers {
            assert_eq!(result, Ok((completed, true)));
            assert!(reports.is_empty());
        } else {
            assert_eq!(result, Err(error));
            assert_eq!(reports.len(), 1);
        }
    }
}

#[test]
fn published_wait_keeps_boot_only_separate_from_pending_capture_checks() {
    let mut report = StateReport::default();
    report.check_mut(Phase::Boot).unwrap().status = Some(CheckStatus::Ok);
    report.check_mut(Phase::Capture).unwrap().status = Some(CheckStatus::Pending);
    for boot_only in [true, false] {
        let (actual, completed) =
            wait_for_checks_until_from(boot_only, Some(Instant::now()), || Ok(report.clone()))
                .unwrap();
        assert_eq!(completed, boot_only);
        assert_eq!(actual, report);
    }
}

#[test]
fn a_failed_status_read_at_the_deadline_cannot_become_completed_or_admit_a_clock() {
    for error in [Errno::ENOENT, Errno::EACCES, Errno::EINVAL, Errno::EIO] {
        let result = wait_for_checks_until_from(true, Some(Instant::now()), || Err(error));
        #[cfg(target_arch = "x86_64")]
        let recovers = true;
        #[cfg(target_arch = "aarch64")]
        let recovers = error == Errno::ENOENT;
        if recovers {
            let (pending, completed) = result.unwrap();
            assert!(!completed);
            assert!(pending.pending(true));
            assert!(!pending.synchronized);
            assert_eq!(pending, StateReport::default());
        } else {
            assert_eq!(result, Err(error));
        }
    }
}

#[test]
fn non_boot_phase_publication_preserves_clock_fields_and_reported_synchronization() {
    for phase in [Phase::Capture, Phase::Restore] {
        let binding = ClockContext::x86(2_400_000_123, 200_000_000).unwrap();
        let mut state = StateReport::default();
        state
            .record_clock(
                &ClockState::Ready(binding.clone()),
                PublishedRateContext::Validated(&binding),
            )
            .unwrap();
        state.synchronized = true;
        state.generation = 17;
        let mut checks = Checks::empty(phase);
        checks.online = CpuMask::parse(b"0").unwrap();
        checks.failures = 1;
        #[cfg(target_arch = "x86_64")]
        {
            checks.tsc_hz = 3_000_000_000;
            checks.lapic_hz = 1_000_000_000;
        }
        checks.apply_report(&mut state, true, 123, 45).unwrap();
        assert_eq!(
            state.clock(PublishedRateContext::Validated(&binding)),
            Ok(ClockState::Ready(binding.clone()))
        );
        assert!(state.synchronized);
        let recorded = state.check_mut(phase).unwrap();
        assert_eq!(recorded.status, Some(CheckStatus::Fail));
        assert_eq!(recorded.failures, Some(1));
        assert_eq!((recorded.generation, recorded.cpus), (17, 1));
        assert_eq!((recorded.elapsed_us, recorded.cpu_us), (123, 45));
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn boot_keeps_partial_x86_hardware_observations_report_only() {
    let mut checks = Checks::empty(Phase::Boot);
    checks.tsc_hz = 2_400_000_123;
    checks.failures = 1;
    let mut state = StateReport::default();
    checks.apply_report(&mut state, true, 123, 45).unwrap();
    assert!(
        state
            .to_string()
            .contains("\ntsc_hz=2400000123\nlapic_hz=0\n")
    );
    let full = ClockContext::x86(checks.tsc_hz, 200_000_000).unwrap();
    assert!(state.clock(PublishedRateContext::Validated(&full)).is_err());
    assert_eq!(state.check_mut(Phase::Boot).unwrap().failures, Some(1));
    assert!(!state.synchronized);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn arm_boot_publication_requires_its_declared_context_and_does_not_attest_synchronization() {
    let mut checks = Checks::empty(Phase::Boot);
    checks.failures = 1;
    let mut state = StateReport::default();
    checks.apply_report(&mut state, true, 123, 45).unwrap();
    assert_eq!(
        state.clock(PublishedRateContext::Uninitialized(
            DeclaredGuestIsa::Aarch64
        )),
        Ok(ClockState::Uninitialized)
    );
    assert!(!state.synchronized);
    let binding = crate::guest::counter::tests::arm_context();
    checks.clock = ClockState::Ready(binding.clone());
    checks.apply_report(&mut state, true, 123, 45).unwrap();
    assert_eq!(
        state.clock(PublishedRateContext::Validated(&binding)),
        Ok(ClockState::Ready(binding))
    );
    assert!(!state.synchronized);
    let before = state.to_string();
    checks.clock = ClockState::Ready(ClockContext::x86(2_400_000_123, 200_000_000).unwrap());
    assert_eq!(
        checks.apply_report(&mut state, true, 1, 1),
        Err(Errno::EINVAL)
    );
    assert_eq!(state.to_string(), before);
}
