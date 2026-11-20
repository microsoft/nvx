use super::super::*;
use crate::guest::counter::tests::{arm_context, arm_context_at_hz};
use std::collections::VecDeque;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicU64, AtomicUsize};

struct FakeOps {
    bindings: Mutex<VecDeque<Result<ClockContext, SampleError>>>,
    binding_reads: AtomicUsize,
    require_window_lock: bool,
    ticks: AtomicU64,
    hz: u64,
}

impl FakeOps {
    fn new(bindings: impl IntoIterator<Item = Result<ClockContext, SampleError>>) -> Self {
        Self {
            bindings: Mutex::new(bindings.into_iter().collect()),
            binding_reads: AtomicUsize::new(0),
            require_window_lock: false,
            ticks: AtomicU64::new(1_000_000),
            hz: 19_200_001,
        }
    }
}

impl PrivilegedOps for FakeOps {
    fn read_counter(&self) -> u64 {
        self.ticks.fetch_add(self.hz, Ordering::Relaxed)
    }

    fn counter_scale(&self, lock: Option<&FileLock>) -> Result<CounterScale, SampleError> {
        self.clock_context(lock)
            .map(|binding| binding.counter_scale())
    }

    fn clock_context(&self, lock: Option<&FileLock>) -> Result<ClockContext, SampleError> {
        assert!(
            !self.require_window_lock || lock.is_some(),
            "caller lock was not forwarded"
        );
        self.binding_reads.fetch_add(1, Ordering::Relaxed);
        self.bindings
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected binding read")
    }

    fn realtime_ns(&self) -> i64 {
        1_790_841_600_000_000_000
    }

    fn open_time_window(&self) -> Result<TimeAccess<'_>, TimeAccessError> {
        panic!("this fixture must not open a privileged device")
    }

    fn adjust_clock(&self, request: ClockAdjustment) -> Result<ClockDiscipline, Errno> {
        assert_eq!(request, ClockAdjustment::Query);
        Ok(ClockDiscipline::default())
    }

    fn power_off(&self, _: PowerOffStatus) {
        panic!("this fixture must not stop a VM")
    }

    fn write_kernel_log(&self, _: &[u8], _: Option<Instant>) {
        panic!("unexpected diagnostic")
    }

    fn write_console(&self, _: &[u8], _: &[u8], _: Option<Instant>) {
        panic!("unexpected console operation")
    }
}

#[test]
fn failed_arm_binding_is_retryable_with_the_same_caller_window_lock_and_cache_owner() {
    let binding = arm_context();
    let mut ops = FakeOps::new([
        Err(SampleError::Device(Errno::EIO)),
        Err(SampleError::Binding(ClockError::Invalid("binding refused"))),
        Ok(binding.clone()),
    ]);
    ops.require_window_lock = true;
    let runtime = Runtime::new(&ops, true);
    let directory = tempfile::tempdir().unwrap();
    let path = CString::new(directory.path().join("window.lock").as_os_str().as_bytes()).unwrap();
    let lock = FileLock::acquire(&path, PRIVATE_FILE_MODE, None).unwrap();
    assert_eq!(
        runtime.counter_scale_in_window(&lock),
        Err(SampleError::Device(Errno::EIO))
    );
    assert!(runtime.counter_scale.get().is_none());
    assert_eq!(
        runtime.counter_scale_in_window(&lock),
        Err(SampleError::Binding(ClockError::Invalid("binding refused")))
    );
    assert!(runtime.counter_scale.get().is_none());
    assert_eq!(
        runtime.counter_scale_in_window(&lock),
        Ok(binding.counter_scale())
    );
    assert_eq!(runtime.counter_scale(), Ok(binding.counter_scale()));
    assert_eq!(ops.binding_reads.load(Ordering::Relaxed), 3);
}

#[test]
fn checked_arm_binding_repeats_are_idempotent_and_changed_scales_fail_explicitly() {
    let binding = arm_context();
    let ops = FakeOps::new([
        Ok(binding.clone()),
        Ok(binding.clone()),
        Ok(arm_context_at_hz(24_000_000)),
    ]);
    let runtime = Runtime::new(&ops, false);
    assert_eq!(runtime.validated_arm_clock_context(), Ok(binding.clone()));
    assert_eq!(runtime.validated_arm_clock_context(), Ok(binding.clone()));
    assert_eq!(
        runtime.validated_arm_clock_context(),
        Err(SampleError::CounterScaleChanged)
    );
    assert_eq!(runtime.counter_scale(), Ok(binding.counter_scale()));
    assert_eq!(ops.binding_reads.load(Ordering::Relaxed), 3);
}

#[test]
fn concurrent_arm_binding_initialization_refuses_conflicts_without_replacing_the_checked_scale() {
    let ops = FakeOps::new([]);
    let runtime = Runtime::new(&ops, false);
    let barrier = std::sync::Barrier::new(3);
    let first = arm_context().counter_scale();
    let second = arm_context_at_hz(24_000_000).counter_scale();
    let results = std::thread::scope(|scope| {
        let run = |scale| {
            barrier.wait();
            runtime.initialize_counter_scale(scale)
        };
        let left = scope.spawn(move || run(first));
        let right = scope.spawn(move || run(second));
        barrier.wait();
        [left.join().unwrap(), right.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == Err(SampleError::CounterScaleChanged))
            .count(),
        1
    );
    let retained = runtime.counter_scale().unwrap();
    assert!(retained == first || retained == second);
    assert_eq!(ops.binding_reads.load(Ordering::Relaxed), 0);
}

#[test]
fn native_arm_runtime_selects_samples_with_full_hz_and_no_synthetic_x86_rate() {
    struct Window(Mutex<VecDeque<u32>>);
    impl TimeWindow for Window {
        fn window_status(&self) -> Result<u8, Errno> {
            Ok(PORTB_TIME_WINDOW)
        }
        fn select_window(&self, selector: u8) -> Result<(), Errno> {
            assert_eq!(selector, SELECT_TIME_SAMPLE);
            Ok(())
        }
        fn read_window_word(&self) -> Result<u32, Errno> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected sample word"))
        }
    }
    let ops = FakeOps::new([Ok(arm_context())]);
    let runtime = Runtime::new(&ops, false);
    let rates = clock_rates(&runtime).unwrap();
    let mut bytes = wire::tests::valid_sample();
    bytes[1] = 0;
    bytes[4..8].copy_from_slice(&0_u32.to_le_bytes());
    bytes[8..16].copy_from_slice(&(ops.realtime_ns() as u64 - 1_000_000_000 + 1_000).to_le_bytes());
    bytes[16..24].copy_from_slice(&(1_000_000 + ops.hz.div_ceil(2)).to_le_bytes());
    bytes[24..28].fill(0);
    bytes[32..40].fill(0);
    let window = Window(Mutex::new(
        bytes
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .collect(),
    ));
    let (result, sample) = select_sample(&window, &ops, 1, SAMPLE_BOUND_NS, 0, rates).unwrap();
    assert_eq!(result, SampleResult::Wide);
    assert_eq!(sample.theta, 1_000);
    assert_eq!((sample.frequency, sample.limited), (0, false));
    assert_eq!(ops.binding_reads.load(Ordering::Relaxed), 1);
}
