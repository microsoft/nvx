//! The time daemon: deferred work, violation watching, the wall clock's discipline, and repair
//! step 7.

#[cfg(target_arch = "x86_64")]
use std::ffi::CStr;
use std::thread;
use std::time::Duration;

use super::api::ColdBoot;
#[cfg(target_arch = "x86_64")]
use super::checks::BootLog;
use super::checks::{self, Checks};
#[cfg(target_arch = "x86_64")]
use super::cpu_mask::CpuMask;
#[cfg(target_arch = "x86_64")]
use super::decoders;
pub(crate) use super::deferred::Promoter;
use super::events::{CheckId, EventSource, ViolationCode};
use super::sample;
use super::sample::{FAST_PERIOD, FAST_POLLS, SLOW_PERIOD, SYNCHRONIZED_AGE_NS};
use super::state::{Discontinuity, SampleRejection, TimeState};
use super::system::{
    self, BestSample, DAEMON_NAME, DAEMON_PID_PATH, DAEMON_PID_TEMP_PATH, PowerOffStatus, Runtime,
    SampleError, SampleResult, clock_ns, cpu_time_ns, state_apply,
};
#[cfg(target_arch = "x86_64")]
use super::system::{
    DEFAULT_STALL_TIMEOUT_S, RCU_SYNC_PATH, RESTORE_PATH, SNAPSHOT_ACKNOWLEDGE, SpawnError,
    fork_process, state_load, suppression_restore,
};
use super::{DEFERRED_START_NS, NSEC_PER_USEC, Phase};
#[cfg(target_arch = "x86_64")]
use super::{IDLE_BOUND_NS, NSEC_PER_MSEC, NSEC_PER_SEC};
use crate::guest::kernel_text::KmsgRecord;
use crate::guest::sys::{
    self, Errno, FileDescriptor, Mode, OpenFlags, Pollable, Signal, SignalDescriptor, Timer, kernel,
};
#[cfg(target_arch = "x86_64")]
use crate::guest::sys::{ChildStatus, ForkResult, Port, PortAccess, PortError, ProcessId};

/// Pause before the daemon polls again after `poll` failed for a reason other than a signal.
const POLL_RETRY: Duration = Duration::from_secs(1);
/// The largest structured kernel log record, the kernel's `CONSOLE_EXT_LOG_MAX`.
const KERNEL_LOG_RECORD_LIMIT: usize = 8192;
/// Longest prefix of a kernel message that a violation event quotes.
const WATCHED_MESSAGE_LIMIT: usize = 400;
/// The daemon's out-of-memory score adjustment, Linux's `OOM_SCORE_ADJ_MIN`, which exempts it
/// from the out-of-memory killer.
const OOM_SCORE_ADJUSTMENT: &[u8] = b"-1000";
/// The signals that the daemon reads from its signal descriptor: a queued restore, a stop, and
/// the end of a restore worker.
#[cfg(target_arch = "x86_64")]
const DAEMON_SIGNALS: [Signal; 3] = [Signal::SIGUSR1, Signal::SIGTERM, Signal::SIGCHLD];
/// Cold-boot ARM never acknowledges or schedules a restore.
#[cfg(target_arch = "aarch64")]
const DAEMON_SIGNALS: [Signal; 2] = [Signal::SIGUSR1, Signal::SIGTERM];
/// The signals that the daemon ignores: the hangup of its session, and writes to a closed pipe.
const IGNORED_SIGNALS: [Signal; 2] = [Signal::SIGHUP, Signal::SIGPIPE];
/// The source name of the tmpfs that forces an RCU grace period.
#[cfg(target_arch = "x86_64")]
const RCU_SYNC_SOURCE: &CStr = c"nvx-rcu-sync";
/// The mount options of that tmpfs: the smallest size, one page.
#[cfg(target_arch = "x86_64")]
const RCU_SYNC_OPTIONS: &CStr = c"size=4k";
/// The value of `rcu_normal` that forces normal, rather than expedited, grace periods.
#[cfg(target_arch = "x86_64")]
const RCU_NORMAL_FORCED: i64 = 1;

/// Drains structured kernel log records and supplies each one's message.
///
/// It returns when no record is left, or with the error of a failed read, such as `EPIPE` for
/// records that the kernel overwrote before the daemon read them.
fn kmsg_drain(log: &FileDescriptor, mut consume: impl FnMut(&[u8])) -> Result<(), Errno> {
    let mut buffer = [0_u8; KERNEL_LOG_RECORD_LIMIT];
    loop {
        let count = match log.read(&mut buffer) {
            Ok(0) | Err(Errno::EAGAIN) => return Ok(()),
            Ok(count) => count,
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(error),
        };
        let record = KmsgRecord::parse(&buffer[..count]).ok_or(Errno::EPROTO)?;
        consume(record.message);
    }
}

/// What one discipline poll did with its time sample.
enum PollOutcome {
    /// It rejected the sample, or could not take or apply one, for this reason.
    Rejected(SampleRejection),
    /// It accepted the sample and corrected the wall clock.
    Accepted {
        /// The accepted sample, with the frequency correction that the discipline holds.
        sample: BestSample,
        /// The wall clock right after a step, or `None` for a slew.
        step_realtime_ns: Option<i64>,
    },
}

/// Applies one poll's outcome, and whether the discipline is synchronized, to the time state,
/// and returns whether the accepted sample's disruption counter differs from the last accepted
/// sample's: host UTC stepped, the host resumed from a suspension, or the guest TSC's offset
/// from the host's changed.
fn apply_poll(state: &mut TimeState, outcome: &PollOutcome, synchronized: bool) -> bool {
    let mut disrupted = false;
    match *outcome {
        PollOutcome::Accepted {
            sample,
            step_realtime_ns,
        } => {
            disrupted =
                state.samples != 0 && state.host_disruption != u64::from(sample.host.disruption);
            state.record_sample(&sample);
            if let Some(realtime_ns) = step_realtime_ns {
                state.discontinuities += 1;
                state.last_discontinuity = Discontinuity::Step;
                state.last_step_ns = sample.theta;
                state.last_step_realtime_ns = realtime_ns;
            }
        }
        PollOutcome::Rejected(reason) => {
            state.rejected_samples += 1;
            state.last_sample_error = reason;
        }
    }
    state.synchronized = synchronized;
    disrupted
}

/// Repair step 7 work that a queued signal scheduled, which starts once it is due.
#[derive(Clone, Copy, Debug)]
#[cfg(target_arch = "x86_64")]
struct PendingRestore {
    /// Monotonic time at which the work may start.
    due_ns: i64,
    /// The readiness path's wall time, which the signal carried.
    readiness_us: i64,
}

/// The child that runs repair step 7, and its outstanding deadlines on the monotonic clock.
#[derive(Clone, Copy, Debug)]
#[cfg(target_arch = "x86_64")]
struct RestoreWorker {
    /// The worker process.
    process: ProcessId,
    /// When the worker moves to normal scheduling, until it has.
    promote_ns: Option<i64>,
    /// When the daemon releases the stall suppression, until it has.
    release_ns: Option<i64>,
}

/// Owned resources and mutable counters of the long-running time daemon.
pub(crate) struct Daemon {
    /// Nonblocking kernel-log stream.
    kmsg: FileDescriptor,
    /// Blocked-signal delivery descriptor.
    signals: SignalDescriptor,
    /// Discipline-poll timer descriptor.
    timer: Timer,
    /// Completed discipline polls.
    polls: i32,
    /// Monotonic time of the latest accepted sample, if any.
    last_accepted_ns: Option<i64>,
    /// Restore work that awaits its start.
    #[cfg(target_arch = "x86_64")]
    restore: Option<PendingRestore>,
    /// The running restore worker.
    #[cfg(target_arch = "x86_64")]
    worker: Option<RestoreWorker>,
}

/// Arms the daemon timer with the current fast or slow interval.
fn arm_timer(daemon: &Daemon) -> Result<(), Errno> {
    daemon.timer.arm_after(if daemon.polls < FAST_POLLS {
        FAST_PERIOD
    } else {
        SLOW_PERIOD
    })
}

/// Drains new kernel messages and raises matching runtime violations.
fn watch_kernel_log(runtime: &Runtime<'_>, daemon: &Daemon) {
    let result = kmsg_drain(&daemon.kmsg, |message| {
        if let Some(code) = super::events::match_watch_rule(message) {
            runtime.fail_fatal(
                PowerOffStatus::Violation,
                code,
                EventSource::Watcher,
                Phase::Runtime,
                &message[..message.len().min(WATCHED_MESSAGE_LIMIT)],
            );
        }
    });
    if let Err(error) = result {
        let (code, detail) = if error == Errno::EPIPE {
            (
                ViolationCode::KmsgOverrun,
                "kernel log records were lost (EPIPE)".into(),
            )
        } else {
            (
                ViolationCode::Conformance(CheckId::C4),
                format!("read kernel watcher: {error}"),
            )
        };
        runtime.fail_fatal(
            PowerOffStatus::Violation,
            code,
            EventSource::Watcher,
            Phase::Runtime,
            detail.as_bytes(),
        );
    }
}

/// Reads ARM's historical structured boot log through the same drain and event policy.
#[cfg(target_arch = "aarch64")]
fn check_boot_log(runtime: &Runtime<'_>, checks: &mut Checks) {
    let mut read = || -> Result<(), Errno> {
        let log = sys::open(
            kernel::KERNEL_LOG_DEVICE,
            OpenFlags::O_RDONLY | OpenFlags::O_NONBLOCK | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        kmsg_drain(&log, |message| {
            if message.starts_with(b"NVX-TIME-") {
                return;
            }
            let code =
                if crate::guest::kernel_text::contains(message, b"Found insecure W+X mapping") {
                    Some(ViolationCode::KernelWx)
                } else {
                    super::events::match_watch_rule(message)
                        .filter(|code| *code != ViolationCode::ClocksourceSwitch)
                };
            if let Some(code) = code {
                checks.failures += 1;
                runtime.conformance_failure(code, Phase::Boot, message);
            }
        })
    };
    if let Err(error) = read() {
        checks.fail(
            runtime,
            CheckId::C4,
            format!("read historical boot log: {error}"),
        );
    }
}

/// Takes one host sample and updates Linux clock discipline and state.
fn discipline_poll(runtime: &Runtime<'_>, daemon: &mut Daemon) {
    let stalls = match sys::read_number::<i64>(kernel::RCU_STALL_COUNT) {
        Ok(stalls) => stalls,
        Err(error) => {
            runtime.fail_fatal(
                PowerOffStatus::Violation,
                ViolationCode::Conformance(CheckId::C8),
                EventSource::Watcher,
                Phase::Runtime,
                format!("RCU stall count: {error}").as_bytes(),
            );
            return;
        }
    };
    if stalls != 0 {
        runtime.fail_fatal(
            PowerOffStatus::Violation,
            ViolationCode::RcuStall,
            EventSource::Watcher,
            Phase::Runtime,
            format!("rcu_stall_count is {stalls}").as_bytes(),
        );
    }
    let fast = daemon.polls < FAST_POLLS;
    daemon.polls += 1;
    // The port-B lock serializes the sample with the other port transactions, and the state
    // update with the sample.
    let (selected, lock) = system::discipline_sample(runtime);
    let outcome = match selected {
        Ok((SampleResult::Accepted, sample)) => correct_clock(runtime, sample, fast),
        Ok((SampleResult::Wide, _)) => PollOutcome::Rejected(SampleRejection::Uncertain),
        #[cfg(target_arch = "x86_64")]
        Err(SampleError::Generation { .. }) => return,
        #[cfg(target_arch = "aarch64")]
        Err(
            error @ (SampleError::Generation { .. }
            | SampleError::Arm(
                sample::arm64::Error::Frequency { .. }
                | sample::arm64::Error::Clocksource(_)
                | sample::arm64::Error::Binding(_),
            )),
        ) => {
            let id = match &error {
                SampleError::Arm(sample::arm64::Error::Clocksource(_)) => CheckId::C6,
                SampleError::Generation { .. } => CheckId::C10,
                _ => CheckId::C3,
            };
            runtime.fail_fatal(
                PowerOffStatus::Violation,
                ViolationCode::Conformance(id),
                EventSource::Watcher,
                Phase::Runtime,
                error.to_string().as_bytes(),
            );
            PollOutcome::Rejected(error.rejection())
        }
        Err(error) => {
            // Nothing else would report it, and the daemon goes on without the sample.
            #[cfg(target_arch = "x86_64")]
            if let SampleError::Ports(error @ PortError::Revoke(_)) = error {
                runtime.log_release_failure(error);
            }
            runtime.log_sample_failure(&error);
            PollOutcome::Rejected(error.rejection())
        }
    };
    if matches!(outcome, PollOutcome::Accepted { .. }) {
        daemon.last_accepted_ns = Some(clock_ns(sys::Clock::Monotonic));
    }
    let synchronized = daemon.last_accepted_ns.is_some_and(|accepted_ns| {
        clock_ns(sys::Clock::Monotonic).wrapping_sub(accepted_ns) <= SYNCHRONIZED_AGE_NS
    });
    let mut disrupted = false;
    if let Err(error) = state_apply(|state| disrupted = apply_poll(state, &outcome, synchronized)) {
        runtime.fail_fatal(
            PowerOffStatus::Violation,
            ViolationCode::Conformance(CheckId::C10),
            EventSource::Watcher,
            Phase::Runtime,
            format!("publish discipline state: {error}").as_bytes(),
        );
    }
    // A disruption restarts the discipline at the fast cadence, as a restore does.
    if disrupted {
        daemon.polls = 0;
    }
    drop(lock);
}

/// Corrects the wall clock to the accepted `sample`, which the discipline took at the fast or
/// slow cadence: steps it to host UTC and then holds the frequency correction that the host's
/// clock correction asks for, or slews it while holding that frequency.
fn correct_clock(runtime: &Runtime<'_>, sample: BestSample, fast: bool) -> PollOutcome {
    let host_error_us = sample.host.error_us();
    let correction = sample::plan_correction(
        sample.theta,
        sample.epsilon,
        fast,
        sample.frequency,
        host_error_us,
    );
    let stepped = matches!(correction, sys::ClockAdjustment::Step { .. });
    let hold = sample::frequency_hold(0, sample.epsilon, sample.frequency, host_error_us);
    let applied = runtime.ops.adjust_clock(correction).and_then(|_| {
        if stepped {
            system::hold_frequency(runtime, hold)
        } else {
            Ok(())
        }
    });
    if let Err(error) = applied {
        runtime.log_sample_failure(&format!("adjtimex: {error}"));
        return PollOutcome::Rejected(SampleRejection::Invalid);
    }
    PollOutcome::Accepted {
        sample,
        step_realtime_ns: stepped.then(|| clock_ns(sys::Clock::Realtime)),
    }
}

/// A kernel setting forced to a value while the owner lives. Dropping the owner restores the
/// setting's saved value, best effort, unless [`restore`](Self::restore) did.
#[cfg(target_arch = "x86_64")]
struct ForcedSetting {
    /// The setting's file.
    path: &'static CStr,
    /// The value to restore, until it is restored.
    saved: Option<i64>,
}

#[cfg(target_arch = "x86_64")]
impl ForcedSetting {
    /// Saves the setting at `path` and forces it to `value`, or returns `None` when either
    /// fails, which leaves the setting unchanged.
    fn force(path: &'static CStr, value: i64) -> Option<Self> {
        let saved = sys::read_number::<i64>(path).ok()?;
        sys::write_number(path, value).ok()?;
        Some(Self {
            path,
            saved: Some(saved),
        })
    }

    /// Restores the saved value, and reports a failure.
    fn restore(mut self) -> Result<(), Errno> {
        self.saved
            .take()
            .map_or(Ok(()), |saved| sys::write_number(self.path, saved))
    }
}

#[cfg(target_arch = "x86_64")]
impl Drop for ForcedSetting {
    /// Restores the saved value, best effort, unless `restore` did.
    fn drop(&mut self) {
        if let Some(saved) = self.saved.take() {
            let _ = sys::write_number(self.path, saved);
        }
    }
}

/// Forces an RCU grace period with a tmpfs mount and unmount in a private mount namespace,
/// under normal grace periods, and returns whether every step succeeded.
#[cfg(target_arch = "x86_64")]
fn grace_period_release() -> bool {
    let Some(normal) = ForcedSetting::force(kernel::RCU_NORMAL, RCU_NORMAL_FORCED) else {
        return false;
    };
    let released = sys::enter_private_mount_namespace().is_ok() && {
        let directory = sys::ScratchDirectory::create(RCU_SYNC_PATH, Mode::S_IRWXU);
        sys::TmpfsMount::mount(RCU_SYNC_SOURCE, directory.path(), RCU_SYNC_OPTIONS)
            .and_then(sys::TmpfsMount::unmount)
            .is_ok()
    };
    normal.restore().is_ok() && released
}

/// Runs the grace-period release with a deadline of `timeout_s` seconds, after which the
/// release counts as done, so that a stalled release cannot hold the suppression back.
#[cfg(target_arch = "x86_64")]
fn release_with_deadline(timeout_s: i64) -> bool {
    run_with_deadline(timeout_s, grace_period_release)
}

/// Runs `work` with a deadline of `timeout_s` seconds, after which it counts as done, and
/// returns whether it succeeded.
///
/// A child of the caller, the waiter, runs `work` in a child of its own, waits for it until the
/// deadline at the latest, and writes its verdict to a pipe, which the caller reads whatever its
/// disposition of `SIGCHLD`, even one that reaps its children on its own. The caller waits for
/// the waiter alone, so work that outlives the deadline leaves the caller no child to reap: it
/// passes to init, or to the nearest subreaper, as when the caller is `nvx-time restore-finish`
/// and exits. The work holds none of the caller's descriptors, which it would keep open after
/// the call, and has the null device as its standard streams.
#[cfg(target_arch = "x86_64")]
fn run_with_deadline(timeout_s: i64, work: fn() -> bool) -> bool {
    let Ok((reader, writer)) = sys::create_pipe() else {
        return false;
    };
    let waiter = match fork_process() {
        Ok(ForkResult::Child) => {
            drop(reader);
            run_waiter(timeout_s, work, writer)
        }
        Ok(ForkResult::Parent(waiter)) => waiter,
        Err(_) => return false,
    };
    drop(writer);
    let mut verdict = [0_u8];
    let succeeded = reader.read_exact(&mut verdict).is_ok() && verdict == [1];
    // The waiter ends once it has written, or it ended without a verdict.
    let _ = sys::wait(waiter);
    succeeded
}

/// The waiter of [`run_with_deadline`]: restores the default disposition of `SIGCHLD`, so that
/// it reaps the work itself, runs `work` in a child, waits for it for at most `timeout_s`
/// seconds, after which it counts as done, and writes whether it succeeded to `verdict`.
#[cfg(target_arch = "x86_64")]
fn run_waiter(timeout_s: i64, work: fn() -> bool, verdict: FileDescriptor) -> ! {
    sys::reset_child_signal();
    let deadline = clock_ns(sys::Clock::Monotonic).wrapping_add(timeout_s * NSEC_PER_SEC);
    let succeeded = match sys::ChildSignalWait::new() {
        Ok(wait) => match fork_process() {
            Ok(ForkResult::Child) => {
                // The caller's read must not wait for work that outlives the deadline, nor any
                // other descriptor of the caller's stay open with it.
                drop(verdict);
                let _ = wait.restore();
                sys::close_inherited_descriptors();
                sys::exit_immediately(u8::from(!work()));
            }
            Ok(ForkResult::Parent(process)) => await_work(process, deadline, &wait),
            Err(_) => false,
        },
        Err(_) => false,
    };
    let _ = verdict.write_all(&[u8::from(succeeded)]);
    sys::exit_immediately(0)
}

/// Waits for the work `process` until the monotonic `deadline`, after which it counts as done,
/// and returns whether it succeeded.
#[cfg(target_arch = "x86_64")]
fn await_work(process: ProcessId, deadline: i64, wait: &sys::ChildSignalWait) -> bool {
    loop {
        let remaining = deadline.wrapping_sub(clock_ns(sys::Clock::Monotonic));
        match sys::try_wait(process) {
            Ok(Some(ChildStatus::Exited(status))) => return status == 0,
            Ok(Some(_)) => return false,
            Ok(None) | Err(Errno::EINTR) => {}
            Err(_) => return false,
        }
        if remaining <= 0 {
            return true;
        }
        wait.wait(Duration::from_nanos(remaining as u64));
    }
}

/// The times that repair step 7 records. The wall time is `readiness_us`, the readiness
/// path's, plus the checks' since `start_ns`, when they were due. The CPU time is step 7's
/// alone: this process's since `cpu_base_ns`, plus the daemon's preparation, `prior_cpu_ns` up
/// to the fork, and the fork's own share, which the daemon writes to `fork_reader` once the
/// fork returns.
#[cfg(target_arch = "x86_64")]
pub(crate) struct RestoreTiming {
    /// Monotonic start used for elapsed accounting.
    pub(crate) start_ns: i64,
    /// Readiness latency measured before deferred work.
    pub(crate) readiness_us: i64,
    /// CPU time accumulated before the fork.
    pub(crate) prior_cpu_ns: i64,
    /// Child process CPU-time baseline.
    cpu_base_ns: i64,
    /// Child-owned reader for the parent's at-fork CPU share.
    fork_reader: Option<FileDescriptor>,
}

/// Repair step 7's CPU time so far, in a process that `timing` describes. The read waits for
/// the daemon's fork share if the daemon has not yet written it.
#[cfg(target_arch = "x86_64")]
pub(crate) fn deferred_restore_cpu_ns(timing: &RestoreTiming) -> i64 {
    let mut cpu = timing
        .prior_cpu_ns
        .wrapping_add(cpu_time_ns())
        .wrapping_sub(timing.cpu_base_ns);
    if let Some(reader) = &timing.fork_reader {
        let mut bytes = [0_u8; size_of::<i64>()];
        if reader.read_exact(&mut bytes).is_ok() {
            let fork_share = i64::from_ne_bytes(bytes);
            if fork_share > 0 {
                cpu = cpu.wrapping_add(fork_share);
            }
        }
    }
    cpu
}

/// Forks the repair step 7 worker at `SCHED_IDLE` for a daemon whose preparation started at
/// `cpu_start_ns`, its CPU clock, and completes `timing`'s CPU fields for the child. The parent
/// returns to normal priority as soon as the fork does, after `at_fork`, which stands for a
/// `pthread_atfork` parent handler. The fork costs the parent CPU time after the child's copy
/// of `timing` was taken, so the parent writes that share to a pipe that the child reads.
#[cfg(target_arch = "x86_64")]
pub(crate) fn fork_deferred_restore(
    cpu_start_ns: i64,
    timing: &mut RestoreTiming,
    at_fork: impl FnOnce(),
) -> Result<ForkResult, SpawnError> {
    let pipe = sys::create_pipe().ok();
    timing.fork_reader = None;
    timing.cpu_base_ns = 0;
    let forked = {
        let _idle = sys::IdlePriority::enter();
        timing.prior_cpu_ns = cpu_time_ns().wrapping_sub(cpu_start_ns);
        let forked = fork_process();
        if matches!(forked, Ok(ForkResult::Parent(_))) {
            at_fork();
        }
        forked
    };
    match forked {
        Ok(ForkResult::Child) => {
            timing.fork_reader = pipe.map(|(reader, _writer)| reader);
        }
        Ok(ForkResult::Parent(_)) => {
            if let Some((_reader, writer)) = pipe {
                let share = cpu_time_ns()
                    .wrapping_sub(cpu_start_ns)
                    .wrapping_sub(timing.prior_cpu_ns);
                let _ = writer.write_all(&share.to_ne_bytes());
            }
        }
        Err(_) => {}
    }
    forked
}

/// Repair step 7, fail-fast: the restore checks, the grace-period release, the saved watchdog
/// settings, the deferred C7, and the restore check's record. A child of the daemon runs it at
/// `SCHED_IDLE` from `DEFERRED_START_NS` after the acknowledgement; the daemon raises it to
/// normal priority `IDLE_BOUND_NS` later and releases the suppression itself at the stall
/// timeout. `restore-finish` runs it inline, with its own deadline (`with_deadline`), in
/// report-only mode when no daemon runs. Neither recorded time counts the grace period wait or
/// C7. Returns whether the grace period was released.
#[cfg(target_arch = "x86_64")]
pub(crate) fn restore_work(
    runtime: &Runtime<'_>,
    new_cpus: CpuMask,
    with_deadline: bool,
    timing: &RestoreTiming,
) -> bool {
    let mut checks = match Checks::load(Phase::Restore) {
        Ok(checks) => checks,
        Err(_) => {
            let mut checks = Checks::empty(Phase::Restore);
            checks.fail(runtime, CheckId::C6, "cannot read the CPU sets");
            checks
        }
    };
    if !checks.online.is_empty() {
        checks::run_restore_checks(runtime, &mut checks, new_cpus);
    }
    let elapsed = clock_ns(sys::Clock::Monotonic).wrapping_sub(timing.start_ns);
    let cpu = deferred_restore_cpu_ns(timing);
    let timeout =
        sys::read_number::<i64>(kernel::RCU_STALL_TIMEOUT).unwrap_or(DEFAULT_STALL_TIMEOUT_S);
    let released = if with_deadline {
        release_with_deadline(timeout)
    } else {
        grace_period_release()
    };
    if !released {
        runtime.repair_failure(
            ViolationCode::RepairSuppression,
            b"the RCU grace-period release failed",
        );
    }
    release_suppression(runtime);
    let verdict = checks::check_timer_list(checks.online, checks::TIMER_LIST_WAIT);
    checks.record(runtime, verdict);
    if let Err(error) = checks::record_check(
        runtime,
        &checks,
        timing.readiness_us.wrapping_add(elapsed / NSEC_PER_USEC),
        cpu / NSEC_PER_USEC,
        0,
        0,
    ) {
        checks.fail(
            runtime,
            CheckId::C10,
            format!("cannot record the restore check: {}", error.desc()),
        );
    }
    released
}

/// Reads the private restore record, and returns the CPUs that the restore brought online, none
/// for a missing record or one of another generation, and the recorded generation.
#[cfg(target_arch = "x86_64")]
fn parse_restore_state() -> Result<(CpuMask, u64), Vec<u8>> {
    let state = state_load().map_err(|_| b"the time state is unreadable".to_vec())?;
    let Ok(text) = sys::read_file(RESTORE_PATH) else {
        return Ok((CpuMask::EMPTY, state.generation));
    };
    let (generation, cpus) = decoders::parse_restore_record(&text)
        .ok_or_else(|| b"the restore record is malformed".to_vec())?;
    let cpus = if generation == state.generation {
        cpus
    } else {
        CpuMask::EMPTY
    };
    Ok((cpus, state.generation))
}

/// Starts `restore`, repair step 7, which is due `DEFERRED_START_NS` after the
/// acknowledgement, in a child at `SCHED_IDLE`, so the watcher and the discipline keep normal
/// priority; the fork itself also runs at `SCHED_IDLE`. The readiness path's wall time came
/// with the signal.
#[cfg(target_arch = "x86_64")]
fn start_restore(runtime: &Runtime<'_>, daemon: &mut Daemon, restore: PendingRestore) {
    let cpu_start = cpu_time_ns();
    let (new_cpus, generation) = match parse_restore_state() {
        Ok(value) => value,
        Err(detail) => {
            runtime.repair_failure(ViolationCode::RepairSuppression, &detail);
            return;
        }
    };
    runtime.set_generation(generation as u32);
    let timeout =
        sys::read_number::<i64>(kernel::RCU_STALL_TIMEOUT).unwrap_or(DEFAULT_STALL_TIMEOUT_S);
    let mut timing = RestoreTiming {
        start_ns: restore.due_ns,
        readiness_us: restore.readiness_us,
        prior_cpu_ns: 0,
        cpu_base_ns: 0,
        fork_reader: None,
    };
    let process = match fork_deferred_restore(cpu_start, &mut timing, || {}) {
        Ok(ForkResult::Child) => {
            let released = restore_work(runtime, new_cpus, false, &timing);
            sys::exit_immediately(u8::from(!released));
        }
        Ok(ForkResult::Parent(process)) => process,
        Err(error) => {
            runtime.repair_failure(
                ViolationCode::RepairSuppression,
                format!("cannot start the restore work: {error}").as_bytes(),
            );
            return;
        }
    };
    daemon.worker = Some(RestoreWorker {
        process,
        promote_ns: Some(restore.due_ns.wrapping_add(IDLE_BOUND_NS)),
        release_ns: Some(clock_ns(sys::Clock::Monotonic).wrapping_add(timeout * NSEC_PER_SEC)),
    });
}

/// Restores the saved stall-detector settings when repair step 7 ends, or when its worker passes
/// its deadline or fails, and reports a repair failure if it cannot.
#[cfg(target_arch = "x86_64")]
fn release_suppression(runtime: &Runtime<'_>) {
    if let Err(detail) = suppression_restore(true) {
        runtime.repair_failure(ViolationCode::RepairSuppression, &detail);
    }
}

/// Reaps every available child and handles the active restore worker.
#[cfg(target_arch = "x86_64")]
fn reap_children(runtime: &Runtime<'_>, daemon: &mut Daemon) {
    while let Some(child) = sys::reap_any_nonblocking() {
        if daemon.worker.map(|worker| worker.process) != Some(child.process) {
            continue;
        }
        daemon.worker = None;
        if !matches!(child.status, ChildStatus::Exited(_)) {
            release_suppression(runtime);
            let signal = match child.status {
                ChildStatus::Signaled(signal) => signal,
                _ => 0,
            };
            runtime.repair_failure(
                ViolationCode::RepairSuppression,
                format!("the restore work ended with signal {signal}").as_bytes(),
            );
        }
        // Repair step 7 ends by restarting the discipline at the fast cadence.
        daemon.polls = 0;
        let _ = arm_timer(daemon);
    }
}

/// Consumes queued daemon signals and schedules restore work.
fn handle_signals(runtime: &Runtime<'_>, daemon: &mut Daemon) {
    while let Some(event) = daemon.signals.read() {
        match event.signal {
            Some(Signal::SIGTERM) => sys::exit_immediately(0),
            // `sigqueue` carries the readiness path's wall time. Step 7 is due
            // `DEFERRED_START_NS` later; the repair stepped the clock to a time sample now.
            Some(Signal::SIGUSR1) => {
                #[cfg(target_arch = "x86_64")]
                {
                    let now = clock_ns(sys::Clock::Monotonic);
                    daemon.last_accepted_ns = Some(now);
                    daemon.restore = Some(PendingRestore {
                        due_ns: now.wrapping_add(DEFERRED_START_NS),
                        readiness_us: if event.queued {
                            i64::from(event.value)
                        } else {
                            0
                        },
                    });
                }
                #[cfg(target_arch = "aarch64")]
                {
                    runtime.fail_fatal(
                        PowerOffStatus::Violation,
                        ViolationCode::Conformance(CheckId::C10),
                        EventSource::Watcher,
                        Phase::Runtime,
                        b"ARM cold boot does not support restore repair",
                    );
                    sys::exit_immediately(1);
                }
            }
            _ => {}
        }
    }
    #[cfg(target_arch = "x86_64")]
    reap_children(runtime, daemon);
}

/// Creates all daemon-owned signal and timer resources.
fn setup_daemon(kmsg: FileDescriptor, last_accepted_ns: Option<i64>) -> Result<Daemon, Errno> {
    sys::ignore_signals(&IGNORED_SIGNALS);
    let signals = SignalDescriptor::new(&DAEMON_SIGNALS)?;
    let timer = Timer::new()?;
    let daemon = Daemon {
        kmsg,
        signals,
        timer,
        polls: 0,
        last_accepted_ns,
        #[cfg(target_arch = "x86_64")]
        restore: None,
        #[cfg(target_arch = "x86_64")]
        worker: None,
    };
    sys::write_file(kernel::OOM_SCORE_ADJUSTMENT, OOM_SCORE_ADJUSTMENT)?;
    arm_timer(&daemon)?;
    Ok(daemon)
}

/// Opens the kernel log at its end, so that the violation watcher reads only later records.
fn open_kernel_log(runtime: &Runtime<'_>, checks: &mut Checks) -> Option<FileDescriptor> {
    let flags = OpenFlags::O_RDONLY | OpenFlags::O_NONBLOCK | OpenFlags::O_CLOEXEC;
    let log = sys::open(kernel::KERNEL_LOG_DEVICE, flags, Mode::empty()).and_then(|log| {
        log.seek_end()?;
        Ok(log)
    });
    match log {
        Ok(log) => Some(log),
        Err(error) => {
            checks.fail(
                runtime,
                CheckId::C4,
                format!(
                    "open {}: {}",
                    kernel::KERNEL_LOG_DEVICE.to_string_lossy(),
                    error.desc()
                ),
            );
            None
        }
    }
}

/// Runs the deferred boot checks from the monotonic `start_ns`, at idle priority until they end
/// or `IDLE_BOUND_NS` passes, and returns them with the kernel log that the daemon watches.
fn deferred_boot_checks(
    runtime: &Runtime<'_>,
    start_ns: i64,
    c12_failures: usize,
) -> (Checks, Option<FileDescriptor>) {
    let _promoter = match Promoter::start(start_ns) {
        Some(promoter) => promoter,
        None => {
            runtime.conformance_failure(
                ViolationCode::Conformance(CheckId::C10),
                Phase::Boot,
                b"cannot start deferred priority promoter",
            );
            sys::exit_immediately(1);
        }
    };
    let _idle = sys::IdlePriority::enter();
    sys::sleep_until_monotonic(start_ns);
    let mut checks = match Checks::load(Phase::Boot) {
        Ok(checks) => checks,
        Err(_) => {
            let mut checks = Checks::empty(Phase::Boot);
            #[cfg(target_arch = "x86_64")]
            let id = CheckId::C1;
            #[cfg(target_arch = "aarch64")]
            let id = CheckId::C6;
            checks.fail(runtime, id, "cannot read the CPU sets");
            sys::exit_immediately(1);
        }
    };
    checks.failures = c12_failures;
    // The buffer is allocated before the checks start their threads, as the C program did.
    #[cfg(target_arch = "x86_64")]
    let log_size = sys::kernel::kernel_log_size();
    #[cfg(target_arch = "x86_64")]
    let mut log_buffer = Vec::new();
    #[cfg(target_arch = "x86_64")]
    let buffer = match log_size {
        Ok(size) if size > 0 && log_buffer.try_reserve_exact(size).is_ok() => {
            log_buffer.resize(size, 0);
            Some(log_buffer.as_mut_slice())
        }
        _ => None,
    };
    let kmsg = open_kernel_log(runtime, &mut checks);
    #[cfg(target_arch = "x86_64")]
    let log = kmsg.is_some().then_some(BootLog {
        // A size that `klogctl` could not report is its failure result, -1.
        size: log_size.map_or(-1, |size| size as i64),
        buffer,
    });
    #[cfg(target_arch = "x86_64")]
    checks::run_boot_checks(runtime, &mut checks, log);
    #[cfg(target_arch = "aarch64")]
    {
        checks::run_boot_checks(runtime, &mut checks);
        check_boot_log(runtime, &mut checks);
    }
    (checks, kmsg)
}

/// Runs the deferred boot checks of the cold boot `boot`, in the child that
/// [`TimeAbi::start_daemon`](super::TimeAbi::start_daemon) forked, and then becomes the time
/// daemon, whose discipline starts from the cold boot's initial sample, if any.
pub(crate) fn boot_checker(runtime: &Runtime<'_>, boot: &ColdBoot) -> ! {
    let c12_ns = clock_ns(sys::Clock::Monotonic).wrapping_sub(boot.start_ns);
    let start = clock_ns(sys::Clock::Monotonic).wrapping_add(DEFERRED_START_NS);
    let cpu_start = cpu_time_ns();
    runtime.set_async_output(true);
    let detached = {
        let flags = OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC;
        sys::open(kernel::NULL_DEVICE, flags, Mode::empty())
            .and_then(|null| sys::detach(Some(&null)))
    };
    if let Err(error) = detached {
        runtime.conformance_failure(
            ViolationCode::Conformance(CheckId::C10),
            Phase::Boot,
            format!("detach time daemon: {error}").as_bytes(),
        );
        sys::exit_immediately(1);
    }
    let _exclusive = match sys::try_lock_file(system::DAEMON_LOCK_PATH, system::PRIVATE_FILE_MODE) {
        Ok(owner) => owner,
        Err(error) => {
            runtime.conformance_failure(
                ViolationCode::Conformance(CheckId::C10),
                Phase::Boot,
                format!("time daemon ownership: {error}").as_bytes(),
            );
            sys::exit_immediately(1);
        }
    };
    let (mut checks, kmsg) = deferred_boot_checks(runtime, start, boot.failures);
    let daemon = kmsg.and_then(|kmsg| match become_daemon(kmsg, boot.accepted_ns) {
        Ok(daemon) => Some(daemon),
        Err(error) => {
            checks.fail(
                runtime,
                CheckId::C10,
                format!("cannot start the time daemon: {}", error.desc()),
            );
            None
        }
    });
    if let Err(error) = checks::record_check(
        runtime,
        &checks,
        c12_ns.wrapping_add(clock_ns(sys::Clock::Monotonic).wrapping_sub(start)) / NSEC_PER_USEC,
        boot.cpu_ns
            .wrapping_add(cpu_time_ns().wrapping_sub(cpu_start))
            / NSEC_PER_USEC,
        checks.tsc_hz,
        checks.lapic_hz,
    ) {
        checks.fail(
            runtime,
            CheckId::C10,
            format!("cannot publish the time state: {}", error.desc()),
        );
    }
    if let Some(daemon) = daemon {
        daemon_loop(runtime, daemon);
    }
    sys::exit_immediately(1)
}

/// Completes daemon setup, takes the daemon's command name, by which the published process
/// identifier is recognized whichever program started the daemon, and atomically publishes the
/// identifier. The discipline starts from the sample accepted at the monotonic
/// `last_accepted_ns`, if any.
pub(crate) fn become_daemon(
    kmsg: FileDescriptor,
    last_accepted_ns: Option<i64>,
) -> Result<Daemon, Errno> {
    let daemon = setup_daemon(kmsg, last_accepted_ns)?;
    sys::set_command_name(DAEMON_NAME)?;
    system::replace_file(
        DAEMON_PID_PATH,
        DAEMON_PID_TEMP_PATH,
        &sys::current_process().line(),
    )?;
    Ok(daemon)
}

/// Converts an absolute monotonic nanosecond deadline to a poll timeout, rounded up to whole
/// milliseconds, the unit of `poll`, so that the daemon does not wake before it.
#[cfg(target_arch = "x86_64")]
fn timeout_until(deadline_ns: i64) -> Duration {
    /// The longest timeout that `poll` takes, in milliseconds.
    const POLL_TIMEOUT_MAX_MS: u64 = i32::MAX as u64;
    let remaining = deadline_ns.wrapping_sub(clock_ns(sys::Clock::Monotonic));
    let remaining = u64::try_from(remaining).unwrap_or(0);
    Duration::from_millis(
        remaining
            .div_ceil(NSEC_PER_MSEC as u64)
            .min(POLL_TIMEOUT_MAX_MS),
    )
}

/// Runs the kernel watcher, discipline poller, and restore-worker coordinator.
pub(crate) fn daemon_loop(runtime: &Runtime<'_>, mut daemon: Daemon) -> ! {
    loop {
        #[cfg(target_arch = "x86_64")]
        let worker = daemon.worker;
        #[cfg(target_arch = "x86_64")]
        let due = daemon.restore.filter(|_| worker.is_none());
        #[cfg(target_arch = "x86_64")]
        let timeout = [
            worker.and_then(|worker| worker.promote_ns),
            worker.and_then(|worker| worker.release_ns),
            due.map(|restore| restore.due_ns),
        ]
        .into_iter()
        .flatten()
        .min()
        .map(timeout_until);
        #[cfg(target_arch = "aarch64")]
        let timeout = None;
        let events = match sys::poll(
            [
                Pollable::File(&daemon.kmsg),
                Pollable::Signals(&daemon.signals),
                Pollable::Timer(&daemon.timer),
            ],
            timeout,
        ) {
            Ok(events) => events,
            Err(Errno::EINTR) => Default::default(),
            Err(_) => {
                thread::sleep(POLL_RETRY);
                continue;
            }
        };
        let [log, signals, timer] = events;
        if signals.error || timer.error {
            runtime.fail_fatal(
                PowerOffStatus::Violation,
                ViolationCode::Conformance(CheckId::C10),
                EventSource::Watcher,
                Phase::Runtime,
                b"time-daemon signal or timer descriptor failed",
            );
            sys::exit_immediately(1);
        }
        if log.readable || log.error {
            watch_kernel_log(runtime, &daemon);
        }
        if signals.readable {
            handle_signals(runtime, &mut daemon);
        }
        if timer.readable {
            daemon.timer.consume();
            discipline_poll(runtime, &mut daemon);
            if let Err(error) = arm_timer(&daemon) {
                runtime.fail_fatal(
                    PowerOffStatus::Violation,
                    ViolationCode::Conformance(CheckId::C10),
                    EventSource::Watcher,
                    Phase::Runtime,
                    format!("arm discipline timer: {error}").as_bytes(),
                );
                sys::exit_immediately(1);
            }
        }
        #[cfg(target_arch = "x86_64")]
        {
            let mut now = clock_ns(sys::Clock::Monotonic);
            // Step 7 starts `DEFERRED_START_NS` after the acknowledgement, and continues at normal
            // priority `IDLE_BOUND_NS` after it starts.
            if daemon.worker.is_none()
                && let Some(restore) = daemon.restore.filter(|restore| now >= restore.due_ns)
            {
                daemon.restore = None;
                start_restore(runtime, &mut daemon, restore);
                now = clock_ns(sys::Clock::Monotonic);
            }
            if let Some(worker) = &mut daemon.worker {
                if worker
                    .promote_ns
                    .is_some_and(|promote_ns| now >= promote_ns)
                {
                    worker.promote_ns = None;
                    sys::set_scheduling_policy(Some(worker.process), sys::SchedulingPolicy::Normal);
                }
                if worker
                    .release_ns
                    .is_some_and(|release_ns| now >= release_ns)
                {
                    worker.release_ns = None;
                    release_suppression(runtime);
                }
            }
        }
    }
}

/// Returns the recorded `daemon` while it runs.
#[cfg(target_arch = "x86_64")]
pub(crate) fn running_daemon(daemon: Option<ProcessId>) -> Option<ProcessId> {
    daemon.filter(|&process| sys::process_exists(process))
}

/// Repair step 6: acknowledges the restore through the snapshot port, when the host gated its
/// input, and ends the port transaction.
#[cfg(target_arch = "x86_64")]
pub(crate) fn acknowledge_restore(ports: PortAccess<'_>) {
    ports.write_u8(Port::Snapshot, SNAPSHOT_ACKNOWLEDGE);
}

/// Repair step 7 on the readiness path: hands the step to `daemon`, after any acknowledgement.
/// The `sigqueue` value is the readiness path's wall time since `start_ns`, on the monotonic
/// clock, in microseconds clamped to 32 bits, read after the acknowledgement so that the read
/// does not delay it.
#[cfg(target_arch = "x86_64")]
pub(crate) fn queue_restore(daemon: ProcessId, start_ns: i64) -> Result<(), Errno> {
    let elapsed = clock_ns(sys::Clock::Monotonic).wrapping_sub(start_ns) / NSEC_PER_USEC;
    let value = elapsed.clamp(0, i64::from(u32::MAX)) as u32;
    sys::queue_user_signal(daemon, value)
}

/// Repair steps 6 and 7 of a restore that a capture finishes, on the readiness path: the
/// acknowledgement through the snapshot port when `acknowledge`, in a port transaction of its
/// own, and then the handoff of step 7 to `daemon`. Returns whether it finished the restore.
///
/// When the acknowledgement cannot take the port permission, it neither acknowledges nor
/// signals, and the restore stays unfinished, as without a daemon: the caller's own finish then
/// takes the permission again, and reports a failure. A handoff that fails after the
/// acknowledgement fails check C10, which stops the VM unless report-only, and the restore
/// still counts as finished, so that the caller does not acknowledge it again.
#[cfg(target_arch = "x86_64")]
pub(crate) fn finish_restore(
    runtime: &Runtime<'_>,
    daemon: ProcessId,
    acknowledge: bool,
    start_ns: i64,
) -> bool {
    if acknowledge {
        let Ok(ports) = runtime.open_ports() else {
            return false;
        };
        acknowledge_restore(ports);
    }
    if queue_restore(daemon, start_ns).is_err() {
        let mut checks = Checks::empty(Phase::Restore);
        checks.fail(runtime, CheckId::C10, "the time daemon is not running");
    }
    true
}

/// Constructs accounting for an inline report-only restore.
#[cfg(target_arch = "x86_64")]
pub(crate) fn inline_restore_timing(start_ns: i64) -> RestoreTiming {
    let now = clock_ns(sys::Clock::Monotonic);
    RestoreTiming {
        start_ns: now,
        readiness_us: now.wrapping_sub(start_ns) / NSEC_PER_USEC,
        prior_cpu_ns: 0,
        cpu_base_ns: cpu_time_ns(),
        fork_reader: None,
    }
}

/// Returns the accounting that a test of `fork_deferred_restore` starts from, which the fork
/// completes.
#[cfg(target_arch = "x86_64")]
pub(crate) fn empty_restore_timing() -> RestoreTiming {
    RestoreTiming {
        start_ns: 0,
        readiness_us: 0,
        prior_cpu_ns: 0,
        cpu_base_ns: 0,
        fork_reader: None,
    }
}

#[cfg(test)]
mod tests;
