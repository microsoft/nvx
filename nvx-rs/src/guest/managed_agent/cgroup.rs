use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use crate::guest::cgroup::{ExecutionCgroup, Layout, Scope};
use crate::guest::exec::{Execution, Options};
use crate::guest::spawn::Cgroup;
use crate::guest::sys::{self, Errno, Mode, MountFlags, ProcessId};

use super::seam::{Paths, System, path_c_string};

/// How long settlement waits for the execution cgroup to empty: as long as the C agent's 200
/// retries, ten milliseconds apart.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(2);

/// Returns the agent's cgroups in its mode. The direct agent runs in the hierarchy's root, and its
/// workloads in `nvx-exec`, which it prepares and settles whole. The sandbox agent runs in
/// `agent`, and its workloads in `container`, which the init agent prepares.
fn layout(paths: &Paths, direct: bool) -> Result<Layout, Errno> {
    let root = &paths.cgroup_root;
    let layout = if direct {
        Layout::new(root, root.join("nvx-exec"))
    } else {
        Layout::new(root.join("agent"), root.join("container"))
    };
    layout.map_err(|error| sys::errno_of(&error))
}

/// Returns the handle of the direct agent's execution cgroup, `nvx-exec`, in which each workload
/// runs, one at a time.
fn execution(paths: &Paths) -> Result<ExecutionCgroup, Errno> {
    ExecutionCgroup::open(&layout(paths, true)?, None).map_err(|error| sys::errno_of(&error))
}

/// Mounts cgroup v2 when needed and prepares the per-execution cgroup.
pub(crate) fn prepare(system: &mut impl System) -> Result<(), Errno> {
    let layout = layout(system.paths(), true)?;
    let root = layout.agent();
    if std::fs::metadata(root.join("cgroup.procs")).is_err() {
        mkdir(root)?;
        let target = path_c_string(root)?;
        system.mount(
            Some(c"cgroup2"),
            &target,
            Some(c"cgroup2"),
            MountFlags::MS_NOSUID | MountFlags::MS_NODEV | MountFlags::MS_NOEXEC,
        )?;
    }
    ExecutionCgroup::prepare(&layout, None, &[])
        .map(drop)
        .map_err(|error| sys::errno_of(&error))
}

/// Returns the cgroup that each direct workload joins before its program runs.
pub(crate) fn placement(paths: &Paths) -> Result<Cgroup, Errno> {
    execution(paths)?
        .placement()
        .map_err(|error| sys::errno_of(&error))
}

/// Returns whether the execution cgroup reports one or more tasks.
pub(crate) fn has_tasks(system: &impl System) -> Result<bool, Errno> {
    execution(system.paths())?
        .is_occupied(Scope::Workload)
        .map_err(|error| sys::errno_of(&error))
}

/// Requests atomic killing of every process in the execution cgroup.
pub(crate) fn kill(system: &mut impl System) -> Result<(), Errno> {
    let result = execution(system.paths()).and_then(|execution| {
        execution
            .kill(Scope::Workload)
            .map_err(|error| sys::errno_of(&error))
    });
    if let Err(error) = result {
        super::diagnostics::error(system, "exec-cgroup-kill", error as i32);
    }
    result
}

/// Returns how a workload's execution ends it: it kills the process group that the workload
/// leads and, in direct mode, every process of the execution cgroup, which also holds the
/// workload's descendants that left the group.
pub(crate) fn options(paths: &Paths, direct: bool) -> Result<Options, Errno> {
    let options = Options::new().process_group();
    if direct {
        Ok(options.cgroup(&execution(paths)?, Scope::Workload))
    } else {
        Ok(options)
    }
}

/// Collects the status of every child that has ended, as the agent is its process's one status
/// collector, and hands the workload's status to its execution. The direct agent is PID 1, so its
/// children include every orphaned descendant of its workloads.
pub(crate) fn reap(workload: &mut Execution) {
    while let Some(reaped) = sys::reap_any_nonblocking() {
        workload.observe(
            reaped.process.raw().unsigned_abs(),
            ExitStatus::from_raw(reaped.wait_status),
        );
    }
}

/// Collects the status of every child that has ended, when every child left is an orphaned
/// descendant, or a daemon that the PID 1 agent inherited: once the workload's status is known,
/// and between requests, when no workload runs. Like the C agent's loop, it stops once no ended
/// child is left to collect, and has no failure to report.
pub(super) fn reap_orphans() {
    while sys::reap_any_nonblocking().is_some() {}
}

/// Reaps orphaned descendants and returns the cgroup's current occupancy.
pub(crate) fn verify_empty(system: &impl System) -> Result<bool, Errno> {
    reap_orphans();
    has_tasks(system)
}

/// Kills and reaps the execution cgroup until it is empty or the deadline expires.
pub(crate) fn settle(system: &mut impl System) -> Result<(), Errno> {
    kill(system)?;
    execution(system.paths())?
        .settle(Scope::Workload, SETTLE_TIMEOUT, || {
            reap_orphans();
            Ok(())
        })
        .map_err(|error| sys::errno_of(&error))
}

/// Moves a sandbox launcher into the container cgroup.
pub(crate) fn write_container_pid(system: &impl System, child: ProcessId) -> Result<(), Errno> {
    ExecutionCgroup::open(&layout(system.paths(), false)?, None)
        .and_then(|container| container.place(child.raw().unsigned_abs()))
        .map_err(|error| sys::errno_of(&error))
}

/// Creates one cgroup directory with mode 0755, accepting an existing path.
fn mkdir(path: &Path) -> Result<(), Errno> {
    let path = path_c_string(path)?;
    match sys::create_directory(&path, Mode::from_bits_retain(0o755)) {
        Ok(()) | Err(Errno::EEXIST) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::super::seam::TestSystem;
    use super::*;

    #[test]
    fn task_count_requires_a_canonical_decimal() {
        let _process = super::super::PROCESS_TEST_LOCK.lock().unwrap();
        let (_temporary, system) = TestSystem::with_cgroup();
        let tasks = system.paths.cgroup_root.join("nvx-exec/pids.current");
        for (contents, expected) in [
            (b"0\n".as_slice(), false),
            (b"1\n", true),
            (b"10\n", true),
            (b"4194304\n", true),
        ] {
            std::fs::write(&tasks, contents).unwrap();
            assert_eq!(has_tasks(&system).unwrap(), expected, "{contents:?}");
        }
        for contents in [
            b"".as_slice(),
            b"\n",
            b"0",
            b"00\n",
            b"01\n",
            b"-1\n",
            b"+1\n",
            b" 1\n",
            b"1 \n",
            b"0\n0\n",
            b"1\0\n",
            b"max\n",
        ] {
            std::fs::write(&tasks, contents).unwrap();
            assert_eq!(
                has_tasks(&system).unwrap_err(),
                Errno::EPROTO,
                "{contents:?}"
            );
        }
        std::fs::write(&tasks, [b'1'; 40]).unwrap();
        assert_eq!(has_tasks(&system).unwrap_err(), Errno::EOVERFLOW);
    }

    #[test]
    fn missing_and_unreadable_task_counts_are_errors() {
        let _process = super::super::PROCESS_TEST_LOCK.lock().unwrap();
        let (_temporary, system) = TestSystem::with_cgroup();
        let tasks = system.paths.cgroup_root.join("nvx-exec/pids.current");
        std::fs::remove_file(&tasks).unwrap();
        assert_eq!(has_tasks(&system).unwrap_err(), Errno::ENOENT);
        std::fs::create_dir(&tasks).unwrap();
        assert!(has_tasks(&system).is_err());
    }

    #[test]
    fn preparation_enables_the_pids_controller_once() {
        let _process = super::super::PROCESS_TEST_LOCK.lock().unwrap();
        let (_temporary, mut system) = TestSystem::with_cgroup();
        let root = system.paths.cgroup_root.clone();
        prepare(&mut system).unwrap();
        assert_eq!(
            std::fs::read(root.join("cgroup.subtree_control")).unwrap(),
            b""
        );
        std::fs::remove_file(root.join("nvx-exec/pids.current")).unwrap();
        prepare(&mut system).unwrap();
        assert_eq!(
            std::fs::read(root.join("cgroup.subtree_control")).unwrap(),
            b"+pids"
        );
        std::fs::remove_file(root.join("cgroup.subtree_control")).unwrap();
        assert_eq!(prepare(&mut system).unwrap_err(), Errno::ENOENT);
    }

    #[test]
    fn settlement_verifies_emptiness_and_propagates_failures() {
        if super::super::isolate_process_test(
            "NVX_CGROUP_SETTLEMENT_TEST_CHILD",
            "guest::managed_agent::cgroup::tests::settlement_verifies_emptiness_and_propagates_failures",
        ) {
            return;
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock().unwrap();
        let (_temporary, mut system) = TestSystem::with_cgroup();
        let execution = system.paths.cgroup_root.join("nvx-exec");
        settle(&mut system).unwrap();
        assert_eq!(std::fs::read(execution.join("cgroup.kill")).unwrap(), b"1");

        std::fs::remove_file(execution.join("cgroup.kill")).unwrap();
        assert_eq!(settle(&mut system).unwrap_err(), Errno::ENOENT);
        std::fs::write(execution.join("cgroup.kill"), b"").unwrap();
        std::fs::write(execution.join("pids.current"), b"bad\n").unwrap();
        assert_eq!(settle(&mut system).unwrap_err(), Errno::EPROTO);
    }

    #[test]
    fn verification_reaps_orphaned_children() {
        if super::super::isolate_process_test(
            "NVX_CGROUP_REAP_TEST_CHILD",
            "guest::managed_agent::cgroup::tests::verification_reaps_orphaned_children",
        ) {
            return;
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock().unwrap();
        let (_temporary, system) = TestSystem::with_cgroup();
        let mut child = Command::new("/bin/true").spawn().unwrap();
        let process = ProcessId::from_child(&child);
        while !sys::has_exited(process).unwrap() {
            std::thread::yield_now();
        }
        assert!(!verify_empty(&system).unwrap());
        assert_eq!(sys::wait(process), None);
        let _ = child.wait();
    }

    #[test]
    fn unreaped_tasks_at_deadline_are_not_settled() {
        if super::super::isolate_process_test(
            "NVX_CGROUP_DEADLINE_TEST_CHILD",
            "guest::managed_agent::cgroup::tests::unreaped_tasks_at_deadline_are_not_settled",
        ) {
            return;
        }
        let _process = super::super::PROCESS_TEST_LOCK.lock().unwrap();
        let (_temporary, mut system) = TestSystem::with_cgroup();
        std::fs::write(
            system.paths.cgroup_root.join("nvx-exec/pids.current"),
            b"1\n",
        )
        .unwrap();
        assert_eq!(settle(&mut system).unwrap_err(), Errno::ETIMEDOUT);
    }
}
