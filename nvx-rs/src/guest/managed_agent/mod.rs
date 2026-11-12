//! `nvx-managed-agent CONTROL_TTY ROOTFS HOSTNAME UID GID USER HOME`: the managed guest agent,
//! which serves NVX's authenticated control session on `CONTROL_TTY` and runs one workload at a
//! time as `UID` and `GID`.
//!
//! With `ROOTFS` `-`, the direct agent runs as PID 1: it applies the host mapping table beneath
//! its own root, `/`, launches each workload through nvx-init's plain-process core, reaps orphaned
//! processes, and powers the VM off after `STOP`. It refuses any other `ROOTFS`. It refuses
//! workloads until the table is complete, and keeps the C agent's command lines, status and error
//! categories, 1 MiB receive window, and timeout and cancellation behavior.
//!
//! It serves NVXC version 2, whose frames carry the requests of version 1, which the C agent
//! spoke, and refuses a frame of any other version. An execution runs alone, and a second `EXEC`
//! gets `BUSY`; its workload reads a pipe of the standard input that the host streams, which the
//! agent acknowledges as it writes it, up to its explicit end; its output has no cap; and `STOP`
//! ends it. While it runs, the agent's records wait in a send queue rather than for the console,
//! and the agent reads none of the workload's output while the queue is full.
//!
//! The agent exits with 0, or with 125 when it fails.
//!
//! # Structure
//!
//! The submodules divide the agent by concern. Mounts, ownership and mode changes, the diagnostic
//! port, TTY operations, VM shutdown, the workload launch, the kernel of the host mapping table's
//! [mapper](crate::guest::mappings), and the production paths are behind a system seam.
//! Production implements it with the safe `sys` wrappers; tests substitute temporary directories
//! and recording operations, including failures of mount, environment export, and `exec`, the
//! root operations' fake kernel for the mapper, and spawn workloads through the real launch with
//! its credential calls replaced. The agent keeps the table's account in a
//! [`MappingTable`](crate::guest::mappings::MappingTable), as any guest agent may, which checks
//! each part that the host sends and hands its mappings to the agent's mapper.
//!
//! The direct agent uses [`init`](crate::guest::init)'s child builder and execution driver.
//! It builds complete `argv` and `envp` before the child starts; the child performs no allocation
//! or Rust cleanup before `execve` or `_exit`. The execution reads the workload's output,
//! ends it, and reports its launch and outcome, and the
//! agent forwards the output as NVXC frames, serves the control session while the workload runs,
//! and reports the outcome. The agent keeps its own policy: the identity,
//! the timeout, the precedence of a cancellation and a timeout over the workload's own
//! status, and the outcome that it reports for a failed launch stage. It builds the workload's
//! environment in memory, as C's environment functions changed it, and passes it to `execve`, so
//! that no process changes its
//! environment.
//!
//! The agent is its process's one status collector, as the direct agent must be as PID 1: its own
//! `waitpid(-1)` loop collects every child's status and hands the workload's to the workload's
//! execution. The direct core kills each workload's process group, including on normal completion.
//!
//! Linux arguments, paths, and environment strings are bytes, not necessarily UTF-8, so the
//! protocol's guest `EXEC` and `MAPS` decoders accept what the C guest accepts. The boot contract
//! is one `nvx_maps=COUNT` token from 1 through 4096, and retired `nvx_map=` tokens are ignored.

use std::ffi::CString;
use std::process::ExitCode;

use super::status::{CommandError, ExitStatus};
use super::sys::{self, Errno};

/// C-compatible diagnostics.
mod diagnostics;
/// Long-lived control request dispatcher.
mod dispatch;
/// Environments that the agent builds for `execve`.
mod environment;
/// The NVXC adapter, which runs each workload as an execution, and its cancellation and timeout.
mod exec;
/// Direct launch commands.
mod launch;
/// Ordered host-filesystem mapping application.
mod mappings;
/// Replaceable privileged-system seam.
mod seam;
/// The control session: the shared NVXS session's guest role, and the NVXC frames that it carries.
mod session;
/// Control-terminal setup and drain behavior.
mod tty;

use diagnostics::write_stderr;
use mappings::HostMappings;
use seam::{Paths, RealSystem, System, path_c_string};
use session::Session;

/// Immutable command-line configuration of a long-lived agent.
pub(crate) struct AgentConfig {
    /// Decimal workload user ID.
    uid: CString,
    /// Decimal workload group ID.
    gid: CString,
    /// Workload user name.
    user: CString,
    /// Workload home directory.
    home: CString,
    /// An accepted restore's proxy; outer `None` preserves cold/legacy defaults.
    restored_proxy: Option<Option<String>>,
}

/// Runs `nvx-managed-agent` with the process's arguments. It exits with 0, or 125 when the agent
/// fails.
pub fn main() -> ExitCode {
    sys::restore_sigpipe();
    let mut system = RealSystem::new(Paths::default());
    ExitStatus::from(run(&mut system, &sys::args())).into()
}

/// Runs the long-lived agent.
fn run(system: &mut impl System, arguments: &[CString]) -> Result<(), CommandError> {
    if arguments.len() != 8 || arguments[2].as_bytes() != b"-" {
        return Err(CommandError::Agent);
    }
    let config = AgentConfig {
        uid: arguments[4].clone(),
        gid: arguments[5].clone(),
        user: arguments[6].clone(),
        home: arguments[7].clone(),
        restored_proxy: None,
    };
    let result = serve(system, &arguments[1], &config);
    finish(system, result)
}

/// Serves the control session on `control_tty` until the host stops the agent.
fn serve(
    system: &mut impl System,
    control_tty: &CString,
    config: &AgentConfig,
) -> Result<(), CommandError> {
    let fd = match tty::open(control_tty) {
        Ok(fd) => fd,
        Err(error) => return Err(fatal(system, "control-open", error)),
    };
    let mut mappings = HostMappings::default();
    if let Err(error) = mappings::prepare(system, &mut mappings) {
        return Err(fatal(system, "host-mappings", error));
    }
    if let Err(error) = tty::configure(system, &fd) {
        return Err(fatal(system, "control-tty", error));
    }
    let mut session = Session::new(fd);
    // The agent powers the VM off with its mappings in place.
    match dispatch::run(system, &mut session, config, &mut mappings) {
        Ok(dispatch::Outcome::Stopped) => Ok(()),
        Err(error) => Err(fatal(system, "control-session", error)),
    }
}

/// Emits a fatal diagnostic on both the port and standard error, and returns the failure.
fn fatal(system: &mut impl System, stage: &str, error: Errno) -> CommandError {
    let status = error as i32;
    diagnostics::error(system, stage, status);
    let message = format!("NVX-MANAGED-ERROR: stage={stage} status={status}\n");
    write_stderr(message.as_bytes());
    CommandError::Agent
}

/// Powers off the VM with the agent's exit status, and returns the agent's result.
fn finish(system: &mut impl System, result: Result<(), CommandError>) -> Result<(), CommandError> {
    let Ok(path) = path_c_string(&system.paths().exit) else {
        return result;
    };
    let code = (ExitStatus::from(result) as u8).to_string();
    let arguments = [
        c"nvx-exit".to_owned(),
        CString::new(code).expect("a decimal status has no NUL"),
    ];
    let _ = system.exec(&path, &arguments, None);
    result
}

#[cfg(test)]
pub(crate) mod tests;
