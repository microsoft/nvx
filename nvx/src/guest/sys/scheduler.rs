//! CPU affinity and scheduling policy of threads.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use nix::sched::{CpuSet, sched_getaffinity, sched_getcpu, sched_setaffinity};
use nix::unistd::{Pid, gettid};

use super::{Errno, ProcessId};
use crate::guest::parse::parse_decimal;

/// The `pid` argument of the scheduling calls that names the calling thread.
const CALLING_THREAD: Pid = Pid::from_raw(0);

/// The procfs directory with one entry, named by its thread ID, per thread of this process.
const THREAD_DIRECTORY: &str = "/proc/self/task";

/// How many times [`promote_all_threads`] reads [`THREAD_DIRECTORY`], so that it also promotes a
/// thread that an idle thread started while the first pass read the directory.
const PROMOTION_PASSES: usize = 2;

/// A CPU affinity mask.
#[derive(Clone, Debug)]
pub(crate) struct CpuAffinity(CpuSet);

impl CpuAffinity {
    /// Returns whether the mask contains `cpu`.
    pub(crate) fn contains(&self, cpu: usize) -> bool {
        self.0.is_set(cpu).unwrap_or(false)
    }

    /// Returns the CPUs of the mask in ascending order.
    pub(crate) fn cpus(&self) -> Vec<usize> {
        (0..CpuSet::count())
            .filter(|&cpu| self.contains(cpu))
            .collect()
    }
}

/// Returns the calling thread's CPU affinity mask.
pub(crate) fn current_affinity() -> Result<CpuAffinity, Errno> {
    sched_getaffinity(CALLING_THREAD).map(CpuAffinity)
}

/// Pins the calling thread to `cpu`, and checks that it then runs there, which it does unless
/// the CPU is offline or outside the process's cpuset. A thread that does not is `EINVAL`.
pub(crate) fn pin_current_thread(cpu: usize) -> Result<(), Errno> {
    let mut set = CpuSet::new();
    set.set(cpu)?;
    sched_setaffinity(CALLING_THREAD, &set)?;
    if current_cpu()? == cpu {
        Ok(())
    } else {
        Err(Errno::EINVAL)
    }
}

/// Returns the CPU on which the calling thread runs.
pub(crate) fn current_cpu() -> Result<usize, Errno> {
    sched_getcpu()
}

/// The calling thread's affinity mask, which dropping the owner restores, so that a thread that
/// pins itself to CPU after CPU returns to the CPUs that it could run on before.
#[derive(Debug)]
pub(crate) struct AffinityGuard(CpuSet);

impl AffinityGuard {
    /// Saves the calling thread's affinity mask.
    pub(crate) fn save() -> Result<Self, Errno> {
        sched_getaffinity(CALLING_THREAD).map(Self)
    }
}

impl Drop for AffinityGuard {
    /// Restores the saved mask, best effort, as the C program did.
    fn drop(&mut self) {
        let _ = sched_setaffinity(CALLING_THREAD, &self.0);
    }
}

/// The scheduling policies between which the time tools move their deferred work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SchedulingPolicy {
    /// `SCHED_OTHER`, Linux's default time-sharing policy.
    Normal,
    /// `SCHED_IDLE`, which runs a thread only when no other wants its CPU.
    Idle,
}

impl SchedulingPolicy {
    /// Returns the kernel's number of the policy.
    const fn number(self) -> i32 {
        match self {
            Self::Normal => nix::libc::SCHED_OTHER,
            Self::Idle => nix::libc::SCHED_IDLE,
        }
    }
}

/// Sets the scheduling policy of the thread `thread`, or of the calling thread for `None`, with
/// priority zero, which both policies require, and ignores failures, as the C program did.
///
/// `nix` does not wrap `sched_setscheduler`, and musl's returns `ENOSYS` because Linux applies
/// it to one thread rather than to the process, so this makes the system call through libc's
/// `syscall`. A thread needs `CAP_SYS_NICE`, or an `RLIMIT_NICE` of at least 20, to leave
/// `SCHED_IDLE`.
pub(crate) fn set_scheduling_policy(thread: Option<ProcessId>, policy: SchedulingPolicy) {
    let thread = thread.map_or(CALLING_THREAD.as_raw(), ProcessId::raw);
    // SAFETY: all zeros is the priority-zero `sched_param` of both policies, including musl's
    // extension fields.
    let parameter: nix::libc::sched_param = unsafe { std::mem::zeroed() };
    // SAFETY: the system call reads the initialized `parameter` and retains no pointer.
    unsafe {
        nix::libc::syscall(
            nix::libc::SYS_sched_setscheduler,
            thread,
            policy.number(),
            &raw const parameter,
        );
    }
}

/// Returns the kernel's number of the scheduling policy of the thread `thread`, or of the
/// calling thread for `None`.
///
/// It makes the system call through libc's `syscall` for the reason that
/// [`set_scheduling_policy`] gives.
pub(crate) fn scheduling_policy(thread: Option<ProcessId>) -> Result<i32, Errno> {
    let thread = thread.map_or(CALLING_THREAD.as_raw(), ProcessId::raw);
    // SAFETY: the system call reads only its integer argument.
    let policy =
        Errno::result(unsafe { nix::libc::syscall(nix::libc::SYS_sched_getscheduler, thread) })?;
    i32::try_from(policy).map_err(|_| Errno::EINVAL)
}

/// Returns the calling thread's ID.
pub(crate) fn current_thread() -> ProcessId {
    ProcessId::from_pid(gettid())
}

/// Moves every thread of this process to [`SchedulingPolicy::Normal`].
pub(crate) fn promote_all_threads() {
    for _ in 0..PROMOTION_PASSES {
        let Ok(threads) = std::fs::read_dir(THREAD_DIRECTORY) else {
            return;
        };
        for thread in threads.flatten() {
            let name = thread.file_name();
            let id = parse_decimal(OsStr::as_bytes(&name))
                .and_then(|id| i64::try_from(id).ok())
                .and_then(ProcessId::from_positive);
            if let Some(id) = id {
                set_scheduling_policy(Some(id), SchedulingPolicy::Normal);
            }
        }
    }
}

/// Returns how many threads this process has, as [`THREAD_DIRECTORY`] lists them.
pub(crate) fn thread_count() -> Result<usize, Errno> {
    std::fs::read_dir(THREAD_DIRECTORY)
        .map(Iterator::count)
        .map_err(|error| super::errno_of(&error))
}

/// [`SchedulingPolicy::Idle`] for the calling thread while the owner lives.
///
/// Dropping the owner returns the thread that entered the policy to
/// [`SchedulingPolicy::Normal`]. A child that the thread forks meanwhile inherits the policy, and
/// keeps it when its copy of the owner drops, because the child's thread is another thread.
#[derive(Debug)]
pub(crate) struct IdlePriority(ProcessId);

impl IdlePriority {
    /// Moves the calling thread to [`SchedulingPolicy::Idle`], best effort.
    pub(crate) fn enter() -> Self {
        set_scheduling_policy(None, SchedulingPolicy::Idle);
        Self(current_thread())
    }
}

impl Drop for IdlePriority {
    /// Returns the thread that entered the policy to [`SchedulingPolicy::Normal`], best effort.
    fn drop(&mut self) {
        if current_thread() == self.0 {
            set_scheduling_policy(None, SchedulingPolicy::Normal);
        }
    }
}

#[cfg(test)]
mod tests;
