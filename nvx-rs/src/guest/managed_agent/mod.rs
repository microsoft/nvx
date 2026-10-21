//! `nvx-managed-agent CONTROL_TTY ROOTFS HOSTNAME UID GID USER HOME`: the managed guest agent,
//! which serves NVX's authenticated control session on `CONTROL_TTY` and runs one workload at a
//! time as `UID` and `GID`.
//!
//! With `ROOTFS` `-`, the direct agent runs as PID 1: it applies the host mapping table, launches
//! each workload in its own cgroup and process group, reaps orphaned processes, and powers the VM
//! off after `STOP`. Otherwise the sandbox agent launches each workload in `ROOTFS` through the
//! container helpers, behind their FIFO barrier, and returns to the init agent after `STOP`. Both
//! keep the C agent's command lines, wire behavior, status and error categories, 1 MiB output and
//! receive windows, timeout and cancellation behavior, and sealed memfd execution configuration.
//!
//! `nvx-managed-agent --exec-config-fd FD -- WORKLOAD...` is the agent's internal execution
//! helper. The launch runs it once the workload's identity applies, and it applies the sealed
//! configuration in descriptor `FD`, then executes the workload. The agent and the helper exit
//! with 0, or with 125 when they fail.
//!
//! # Structure
//!
//! The submodules divide the agent by concern. Mounts, ownership and mode changes, the diagnostic
//! port, TTY operations, the helper's inherited descriptor and `exec`, the workload launch, and
//! the production paths are behind a system seam. Production implements it with the safe `sys`
//! wrappers; tests substitute temporary directories and recording operations, including failures
//! of mount, barrier write, environment export, and `exec`, and spawn workloads through the real
//! launch with its credential calls replaced and a stand-in `setpriv`.
//!
//! The agent launches each workload as one caller of [`spawn`](crate::guest::spawn): it builds
//! the workload's command, with the complete `argv` and `envp`, its descriptors, and in direct
//! mode its working-directory check and cgroup, and the spawned child performs no allocation or
//! Rust cleanup before `execve` or `_exit`. It then drives the workload as one
//! [`exec::Execution`](crate::guest::exec::Execution), its NVXC adapter: the execution reads the
//! workload's output, ends it, and reports its launch and outcome, and the agent forwards the
//! output as NVXC frames, serves the control session while the workload runs, and reports the
//! outcome. The agent keeps its own policy: the null device as the workload's standard input, the
//! identity and the `setpriv` and helper stages, the output cap, the timeout, the precedence of
//! a cancellation, a timeout, and the cap over the workload's own status, and the outcome that it
//! reports for a failed launch stage. The helper builds the workload's environment in memory
//! too, as C's environment functions changed its own, and passes it to `execve`, so neither
//! process changes its environment.
//!
//! The agent is its process's one status collector, as the direct agent must be as PID 1: its own
//! `waitpid(-1)` loop collects every child's status and hands the workload's to the workload's
//! execution, which otherwise collects it by its pidfd. Its cgroups are a
//! [`cgroup::Layout`](crate::guest::cgroup::Layout): the direct agent runs in the hierarchy's
//! root and its workloads in `nvx-exec`, which it kills and settles whole through an
//! [`ExecutionCgroup`](crate::guest::cgroup::ExecutionCgroup), and which each execution kills
//! when it ends its workload, and the sandbox agent places each launcher in the `container`
//! cgroup that the init agent prepared, and ends only the workload's process group.
//!
//! Between requests no workload runs, and an agent that is PID 1 collects the status of every
//! child that ends while it waits for the next one, such as a daemon that `/init` started.
//!
//! Linux arguments, paths, and environment strings are bytes, not necessarily UTF-8, so the
//! protocol's guest `EXEC` and `MAPS` decoders accept what the C guest accepts. The boot contract
//! is one `nvx_maps=COUNT` token from 1 through 4096, and retired `nvx_map=` tokens are ignored.

use std::ffi::CString;
use std::process::ExitCode;

use super::status::{CommandError, ExitStatus};
use super::sys::{self, Errno};

/// Direct-mode cgroup preparation and workload cleanup.
mod cgroup;
/// C-compatible diagnostics.
mod diagnostics;
/// Long-lived control request dispatcher.
mod dispatch;
/// Environments that the agent and its helper build for `execve`.
mod environment;
/// The NVXC adapter, which runs each workload as an execution, and its output cap, cancellation,
/// and timeout.
mod exec;
/// Sealed private configuration shared with helper mode.
mod exec_config;
/// Short-lived post-privilege-drop helper mode.
mod helper;
/// The wait between requests, in which PID 1 collects the children that end.
mod idle;
/// Direct and sandbox launch commands.
mod launch;
#[cfg(all(test, feature = "host"))]
/// In-process host/broker/agent integration harness.
mod loopback;
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

#[cfg(test)]
/// Serializes tests that temporarily alter process-global state.
pub(crate) static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs a process-global agent test in a dedicated test-harness subprocess.
///
/// Returns `true` in the parent after the isolated run succeeds and `false` in the child that
/// should execute the test body. Isolation prevents PID-1-style `waitpid(-1)` behavior from
/// reaping children that unrelated parallel host tests own.
#[cfg(test)]
pub(crate) fn isolate_process_test(marker: &str, test: &str) -> bool {
    if std::env::var_os(marker).is_some() {
        return false;
    }
    let status = std::process::Command::new(
        std::env::current_exe().expect("the test executable has a path"),
    )
    .args(["--exact", test, "--nocapture"])
    .env(marker, "1")
    .status()
    .expect("the isolated test process starts");
    assert!(status.success(), "the isolated test process failed");
    true
}

/// Immutable command-line configuration of a long-lived agent.
pub(crate) struct AgentConfig {
    /// Sandbox root filesystem, or `-` in direct mode.
    rootfs: CString,
    /// Sandbox hostname.
    hostname: CString,
    /// Decimal workload user ID.
    uid: CString,
    /// Decimal workload group ID.
    gid: CString,
    /// Workload user name.
    user: CString,
    /// Workload home directory.
    home: CString,
    /// Whether this process is the direct PID-1 agent.
    direct: bool,
}

/// Runs `nvx-managed-agent` with the process's arguments. It exits with 0, or 125 when the agent
/// or its execution helper fails.
pub fn main() -> ExitCode {
    sys::restore_sigpipe();
    let mut system = RealSystem::new(Paths::default());
    ExitStatus::from(run(&mut system, &sys::args())).into()
}

/// Dispatches the executable's helper or long-lived agent mode.
fn run(system: &mut impl System, arguments: &[CString]) -> Result<(), CommandError> {
    if arguments
        .get(1)
        .is_some_and(|argument| argument.as_bytes() == b"--exec-config-fd")
    {
        return helper::run(system, arguments);
    }
    if arguments.len() != 8 {
        return Err(CommandError::Agent);
    }
    let config = AgentConfig {
        rootfs: arguments[2].clone(),
        hostname: arguments[3].clone(),
        uid: arguments[4].clone(),
        gid: arguments[5].clone(),
        user: arguments[6].clone(),
        home: arguments[7].clone(),
        direct: arguments[2].as_bytes() == b"-",
    };
    let result = serve(system, &arguments[1], &config);
    finish(system, &config, result)
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
    if config.direct
        && let Err(error) = mappings::prepare(system, &mut mappings)
    {
        return Err(fatal(system, "host-mappings", error));
    }
    if let Err(error) = tty::configure(system, &fd) {
        return Err(fatal(system, "control-tty", error));
    }
    let mut session = Session::new(fd);
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

/// Powers off a direct-mode VM with the agent's exit status, and returns the agent's result.
fn finish(
    system: &mut impl System,
    config: &AgentConfig,
    result: Result<(), CommandError>,
) -> Result<(), CommandError> {
    if !config.direct {
        return result;
    }
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
