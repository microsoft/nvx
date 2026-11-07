//! The VMM processes of the store's launches, which recovery holds through verified process
//! references before it signals one.

use std::io;
use std::time::Instant;

use crate::host::platform;
use crate::host::process::{BootId, KILL_TIMEOUT, Process, ProcessRef};

/// Finds the live process `pid` of the current boot and holds it, or returns `None` if it has
/// exited.
///
/// A process that reuses the ID meanwhile does not have the start time that this read, so it is
/// not held.
pub(super) fn find(pid: u32) -> io::Result<Option<Process>> {
    let Some(start_time) = platform::process_start_time(pid)? else {
        return Ok(None);
    };
    Process::open(&ProcessRef {
        pid,
        start_time,
        boot: BootId::current()?,
    })
}

/// Returns whether `process`, which serves the control endpoint of a launch whose log is the file
/// `log`, is that launch's VMM.
pub(super) fn serves_launch(process: &Process, log: (u64, u64)) -> io::Result<bool> {
    let reference = process.reference();
    platform::serves_launch(reference.pid, reference.start_time, log)
}

/// Terminates `process` through its handle, and returns whether it exited within
/// [`KILL_TIMEOUT`], the wait that the termination of other OpenVMM processes allows.
pub(super) fn terminate(process: &Process) -> io::Result<bool> {
    process.kill()?;
    process.wait_bool(Instant::now() + KILL_TIMEOUT)
}

/// Holds the process that serves a launch's control endpoint, whose ID `server_pid` returns, with
/// `hold`, which [`find`] is outside tests, and returns its ID and the held process, or why it
/// cannot be held.
///
/// The endpoint is examined again once the process is held. A held process keeps its ID on
/// Windows, so an endpoint that still names the ID names the held process, and not another that
/// reused the ID after the first lookup, before it was held. On Linux, [`serves_launch`] then
/// examines the held process itself.
pub(super) fn hold_endpoint_server<P>(
    mut server_pid: impl FnMut() -> io::Result<Option<u32>>,
    hold: impl FnOnce(u32) -> io::Result<Option<P>>,
) -> Result<(u32, P), String> {
    let pid = match server_pid() {
        Ok(Some(pid)) => pid,
        Ok(None) => {
            return Err(
                "a process holds its log, and no process serves its control endpoint yet"
                    .to_owned(),
            );
        }
        Err(error) => {
            return Err(format!(
                "a process holds its log, and its control endpoint cannot be examined: {error}"
            ));
        }
    };
    let held = match hold(pid) {
        Ok(Some(held)) => held,
        Ok(None) => {
            return Err(format!(
                "process {pid}, which served its control endpoint, exited before it could be \
                 verified"
            ));
        }
        Err(error) => {
            return Err(format!(
                "process {pid}, which serves its control endpoint, cannot be held: {error}"
            ));
        }
    };
    match server_pid() {
        Ok(Some(again)) if again == pid => Ok((pid, held)),
        Ok(_) => Err(format!(
            "process {pid} stopped serving its control endpoint while it was held"
        )),
        Err(error) => Err(format!(
            "process {pid} was held, and its control endpoint cannot be examined again: {error}"
        )),
    }
}
