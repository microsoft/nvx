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

/// Collects the status of every child that has ended once the workload's status is known, when
/// every child left is an orphaned descendant. Like the C agent's loop, it stops once no ended
/// child is left to collect, and has no failure to report.
fn reap_orphans() {
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
mod tests;
