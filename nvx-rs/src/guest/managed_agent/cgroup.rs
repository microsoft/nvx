use crate::guest::cgroup::{ExecutionCgroup, Layout};
use crate::guest::sys::{self, Errno, ProcessId};

use super::seam::System;

/// Moves a sandbox launcher into the container cgroup.
pub(crate) fn write_container_pid(system: &impl System, child: ProcessId) -> Result<(), Errno> {
    let root = &system.paths().cgroup_root;
    let layout = Layout::new(root.join("agent"), root.join("container"))
        .map_err(|error| sys::errno_of(&error))?;
    ExecutionCgroup::open(&layout, None)
        .and_then(|container| container.place(child.raw().unsigned_abs()))
        .map_err(|error| sys::errno_of(&error))
}
