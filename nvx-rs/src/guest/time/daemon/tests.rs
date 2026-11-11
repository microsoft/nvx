use super::*;

#[cfg(target_arch = "x86_64")]
mod x86;

#[test]
fn malformed_watcher_records_are_not_silently_discarded() {
    let (reader, writer) = sys::create_pipe().unwrap();
    writer
        .write_all(b"malformed structured kernel record\n")
        .unwrap();
    drop(writer);
    let mut consumed = false;
    assert_eq!(kmsg_drain(&reader, |_| consumed = true), Err(Errno::EPROTO));
    assert!(!consumed);
}

fn measurement() -> BestSample {
    BestSample {
        theta: 1500,
        epsilon: 900,
        host: sample::HostClock::default(),
        frequency: 0,
        limited: false,
        test_hooks: false,
    }
}

#[test]
fn common_state_keeps_initial_synchronization_separate_from_runtime_discontinuities() {
    let sample = measurement();
    let mut state = TimeState::default();
    state.record_sample(&sample);
    state.synchronized = true;
    assert_eq!(state.discontinuities, 0);
    assert_eq!(state.last_discontinuity, Discontinuity::None);
    assert!(!apply_poll(
        &mut state,
        &PollOutcome::Rejected(SampleRejection::Invalid),
        true
    ));
    assert_eq!(state.rejected_samples, 1);
    assert_eq!(state.samples, 1);
    assert!(state.synchronized);
}

#[cfg(target_arch = "aarch64")]
struct AdjustmentOps {
    requests: std::sync::Mutex<Vec<sys::ClockAdjustment>>,
    fail_at: usize,
}

#[cfg(target_arch = "aarch64")]
impl system::PrivilegedOps for AdjustmentOps {
    fn adjust_clock(
        &self,
        adjustment: sys::ClockAdjustment,
    ) -> Result<sys::ClockDiscipline, Errno> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(adjustment);
        if requests.len() == self.fail_at {
            Err(Errno::EIO)
        } else {
            Ok(sys::ClockDiscipline::default())
        }
    }
    fn power_off(&self, _status: PowerOffStatus) {
        panic!("a failed adjustment must reject the sample, not stop the VM");
    }
    fn write_kernel_log(&self, _record: &[u8]) {}
    fn write_console(&self, _separator: &[u8], _line: &[u8]) {}
}

#[test]
#[cfg(target_arch = "aarch64")]
fn arm_uses_the_common_slew_step_hold_and_adjustment_failure_policy() {
    for (theta, fail_at, expected_requests) in [
        (1500, 0, 1),
        (200_000_000, 0, 2),
        (1500, 1, 1),
        (200_000_000, 2, 2),
    ] {
        let ops = AdjustmentOps {
            requests: std::sync::Mutex::new(Vec::new()),
            fail_at,
        };
        let runtime = Runtime::new(&ops, false);
        let sample = BestSample {
            theta,
            ..measurement()
        };
        let outcome = correct_clock(&runtime, sample, false);
        assert_eq!(
            matches!(outcome, PollOutcome::Rejected(SampleRejection::Invalid)),
            fail_at != 0
        );
        let requests = ops.requests.lock().unwrap();
        assert_eq!(requests.len(), expected_requests);
        if theta == 1500 {
            assert!(matches!(requests[0], sys::ClockAdjustment::Slew { .. }));
        } else {
            assert!(matches!(requests[0], sys::ClockAdjustment::Step { .. }));
            assert!(matches!(requests[1], sys::ClockAdjustment::Frequency(_)));
        }
    }
}
