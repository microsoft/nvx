use super::*;
use crate::guest::sys::{ClockAdjustment, FrequencyHold};
use crate::guest::time::system::tests::FakeOps;

#[test]
fn daemon_timing_constants_match_the_abi() {
    assert_eq!((SAMPLE_BOUND_NS, SAMPLE_ATTEMPTS), (100_000, 3));
    assert_eq!(SYNCHRONIZED_AGE_NS, 128_000_000_000);
    assert_eq!(
        (FAST_PERIOD.as_secs(), SLOW_PERIOD.as_secs(), FAST_POLLS),
        (16, 64, 4)
    );
}

/// A sample of the host clock whose disruption counter is `disruption`, 1.5 µs ahead of the
/// wall clock, and whose host correction asks for 100 ppm.
fn polled(disruption: u32) -> BestSample {
    BestSample {
        theta: 1_500,
        epsilon: 900,
        host: decoders::HostClock {
            disruption,
            correction: 6_553_600,
            ..decoders::HostClock::default()
        },
        frequency: 6_553_600,
        limited: false,
    }
}

#[test]
fn a_poll_records_its_sample_and_reports_a_disruption() {
    let mut state = TimeState::default();
    let accepted = |disruption, step_realtime_ns| PollOutcome::Accepted {
        sample: polled(disruption),
        step_realtime_ns,
    };
    // The first accepted sample sets the disruption counter, and is no disruption.
    assert!(!apply_poll(&mut state, &accepted(5, None), true));
    assert_eq!((state.samples, state.host_disruption), (1, 5));
    assert_eq!((state.offset_ns, state.frequency_ppb), (1_500, 100_000));
    assert!(!apply_poll(&mut state, &accepted(5, None), true));
    assert_eq!(state.discontinuities, 0);
    // A changed counter is a disruption, and a step a discontinuity.
    assert!(apply_poll(&mut state, &accepted(6, Some(42)), true));
    assert_eq!((state.host_disruption, state.discontinuities), (6, 1));
    assert_eq!(state.last_discontinuity, Discontinuity::Step);
    assert_eq!(
        (state.last_step_ns, state.last_step_realtime_ns),
        (1_500, 42)
    );
    // A rejection keeps the counter, and records its reason.
    for reason in [
        SampleRejection::Uncertain,
        SampleRejection::Tsc,
        SampleRejection::Invalid,
    ] {
        assert!(!apply_poll(
            &mut state,
            &PollOutcome::Rejected(reason),
            false
        ));
        assert_eq!(state.last_sample_error, reason);
    }
    assert_eq!(
        (state.samples, state.rejected_samples, state.synchronized),
        (3, 3, false)
    );
    assert!(!apply_poll(&mut state, &accepted(6, None), true));
    assert_eq!(state.last_sample_error, SampleRejection::None);
}

#[test]
fn a_correction_slews_or_steps_while_holding_the_host_frequency() {
    let ops = FakeOps::new([]);
    let runtime = Runtime::new(&ops, false);
    let PollOutcome::Accepted {
        step_realtime_ns: None,
        ..
    } = correct_clock(&runtime, polled(0), true)
    else {
        panic!("the slew was not accepted");
    };
    let far = BestSample {
        theta: -1_128_000_000,
        ..polled(0)
    };
    let PollOutcome::Accepted {
        step_realtime_ns: Some(_),
        ..
    } = correct_clock(&runtime, far, false)
    else {
        panic!("the step was not accepted");
    };
    let hold = FrequencyHold {
        scaled_ppm: 6_553_600,
        maximum_error_us: 1,
        estimated_error_us: 1,
    };
    assert_eq!(
        ops.adjustments.lock().unwrap().as_slice(),
        [
            ClockAdjustment::Slew {
                offset_ns: 1_500,
                time_constant: 4,
                hold: FrequencyHold {
                    maximum_error_us: 3,
                    ..hold
                },
            },
            ClockAdjustment::Step {
                seconds: -2,
                nanoseconds: 872_000_000,
            },
            ClockAdjustment::Frequency(hold),
        ]
    );
}

#[test]
fn poll_deadlines_round_up_to_milliseconds() {
    let now = clock_ns(sys::Clock::Monotonic);
    assert_eq!(timeout_until(now - 1), Duration::ZERO);
    let soon = timeout_until(now + NSEC_PER_MSEC / 2);
    assert!(soon <= Duration::from_millis(1), "{soon:?}");
    assert_eq!(
        timeout_until(i64::MAX),
        Duration::from_millis(i32::MAX as u64)
    );
}

#[test]
fn a_dropped_promoter_cancels_its_promotion() {
    let far = clock_ns(sys::Clock::Monotonic) + 10 * NSEC_PER_SEC;
    let started = std::time::Instant::now();
    drop(Promoter::start(far).unwrap());
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_failed_revocation_stops_the_daemon_before_it_starts_anything() {
    let mut ops = FakeOps::new([]);
    ops.revoke_error = Some(Errno::ENOMEM);
    Runtime::new(&ops, true).repair_failure(ViolationCode::RepairSuppression, b"revoke");
    let refused = Err(SpawnError::Ports(PortError::Revoke(Errno::ENOMEM)));
    // Without the check, `fork` itself would refuse with `EBUSY`, in a process with more
    // than one thread, such as the test harness.
    let mut timing = empty_restore_timing();
    let mut forked = false;
    assert_eq!(
        fork_deferred_restore(0, &mut timing, || forked = true),
        refused
    );
    assert!(!forked);
    assert!(Promoter::start(clock_ns(sys::Clock::Monotonic)).is_none());
}

/// Returns a process ID above Linux's largest, which no process has, so that a signal to it
/// fails.
fn absent_process() -> ProcessId {
    ProcessId::from_positive(i64::from(i32::MAX)).unwrap()
}

#[test]
fn a_restore_whose_acknowledgement_cannot_take_the_ports_stays_unfinished() {
    let mut ops = FakeOps::new([]);
    ops.grant_error = Some(Errno::ENOMEM);
    let runtime = Runtime::new(&ops, false);
    assert!(!finish_restore(&runtime, absent_process(), true, 0));
    // Neither an acknowledgement nor the event of a failed handoff.
    assert!(ops.output.lock().unwrap().is_empty());
    assert!(ops.console.lock().unwrap().is_empty());
    assert_eq!(ops.power.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(sys::check_released(), Ok(()));
}

#[test]
fn a_finished_restore_is_acknowledged_before_its_handoff() {
    let ops = FakeOps::new([]);
    let runtime = Runtime::new(&ops, true);
    // The handoff to a process that does not exist fails check C10 after the
    // acknowledgement, and the restore still counts as finished.
    assert!(finish_restore(&runtime, absent_process(), true, 0));
    assert_eq!(
        ops.output.lock().unwrap().first(),
        Some(&(Port::Snapshot, SNAPSHOT_ACKNOWLEDGE))
    );
    let console = String::from_utf8(ops.console.lock().unwrap().clone()).unwrap();
    assert!(console.contains("G_CONFORMANCE_C10"), "{console}");
    assert_eq!(sys::check_released(), Ok(()));

    // Without an acknowledgement, the call takes no port transaction of its own.
    let ops = FakeOps::new([]);
    assert!(finish_restore(
        &Runtime::new(&ops, true),
        absent_process(),
        false,
        0
    ));
    assert!(
        !ops.output
            .lock()
            .unwrap()
            .contains(&(Port::Snapshot, SNAPSHOT_ACKNOWLEDGE))
    );
}

/// Work for [`run_with_deadline`] that succeeds.
fn succeeding_work() -> bool {
    true
}

/// Work for [`run_with_deadline`] that fails.
fn failing_work() -> bool {
    false
}

/// Work for [`run_with_deadline`] that would fail, after a deadline of one second.
fn stalled_work() -> bool {
    thread::sleep(Duration::from_secs(3));
    false
}

/// Runs [`run_with_deadline`] with work that succeeds, work that fails, and work that
/// outlives its deadline, in a process that has only the calling thread, and returns whether
/// a call went wrong: returned the wrong verdict, waited for the stalled work, left the
/// process a child, or left the stalled work a descriptor of the process's.
fn deadline_calls_fail() -> bool {
    // The reader must see its end of file once the process closes the writer, a
    // close-on-exec descriptor, while the stalled work still runs.
    let Ok((reader, writer)) = sys::create_pipe() else {
        return true;
    };
    let start = std::time::Instant::now();
    let wrong = !run_with_deadline(1, succeeding_work)
        || run_with_deadline(1, failing_work)
        // Work that outlives the deadline counts as done.
        || !run_with_deadline(1, stalled_work);
    drop(writer);
    let held = reader.set_nonblocking().is_err() || reader.read(&mut [0]) != Ok(0);
    wrong || held || start.elapsed() >= Duration::from_secs(3) || sys::tests::has_child()
}

#[test]
fn work_with_a_deadline_leaves_no_child_whatever_the_child_signal_disposition() {
    if crate::guest::managed_agent::tests::isolate_process_test(
        "NVX_TIME_DEADLINE_TEST_CHILD",
        "guest::time::daemon::tests::work_with_a_deadline_leaves_no_child_whatever_the_child_signal_disposition",
    ) {
        return;
    }
    // `fork_process` refuses the harness's process, which has more than one thread, so a
    // child that has only this thread makes the calls. Its standard streams are on the null
    // device, which the stalled work, left to init, keeps open after the test.
    match sys::tests::fork_isolated_test().expect("fork the caller") {
        ForkResult::Child => {
            let flags = OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC;
            let null = sys::open(kernel::NULL_DEVICE, flags, Mode::empty()).ok();
            let detached = null.is_some() && sys::detach(null.as_ref()).is_ok();
            let failures = [
                !detached,
                deadline_calls_fail(),
                sys::tests::reap_children_automatically(true).is_err() || deadline_calls_fail(),
                sys::tests::reap_children_automatically(false).is_err() || deadline_calls_fail(),
            ];
            let status = failures
                .iter()
                .enumerate()
                .fold(0, |status, (bit, &failed)| status | u8::from(failed) << bit);
            sys::exit_immediately(status);
        }
        ForkResult::Parent(caller) => {
            assert_eq!(
                sys::wait(caller),
                Some(ChildStatus::Exited(0)),
                "failure bits, from the lowest: detach, then the calls under SIGCHLD's \
                     default disposition, SIG_IGN, and SA_NOCLDWAIT"
            );
        }
    }
}
