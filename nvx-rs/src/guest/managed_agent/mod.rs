//! `nvx-managed-agent CONTROL_TTY ROOTFS HOSTNAME UID GID USER HOME`: the managed guest agent,
//! which serves NVX's authenticated control session on `CONTROL_TTY` and runs one workload at a
//! time as `UID` and `GID`.
//!
//! With `ROOTFS` `-`, the direct agent runs as PID 1: it applies the host mapping table beneath
//! its own root, `/`, launches each workload through nvx-init's plain-process core, reaps orphaned
//! processes, and powers the VM off after `STOP`. Otherwise the sandbox agent applies the host
//! mapping table beneath `ROOTFS`, the container's assembled root, has each workload enter
//! `ROOTFS` as an [`entry`](crate::guest::entry) execution, as `UID` and `GID` with their existing
//! account, with `HOSTNAME`, held at a barrier until the agent has placed it in its cgroup, and
//! after `STOP` unmounts the table and returns to the init agent. When it starts, it generates the
//! workload's machine ID and binds it, with the guest's time status, into `ROOTFS` in a mount
//! namespace of its own, of which each execution's starts as a copy, so that the binds end with
//! the agent, and it maps the table in that namespace too. Both refuse workloads until the table
//! is complete, and both keep the C agent's command lines, wire behavior, status and error
//! categories, 1 MiB output and receive windows, and timeout and cancellation behavior.
//!
//! The agent exits with 0, or with 125 when it fails.
//!
//! # Structure
//!
//! The submodules divide the agent by concern. Mounts, ownership and mode changes, the diagnostic
//! port, TTY operations, VM shutdown, the workload launch, the
//! preparation of each sandbox execution, the kernel of the host mapping table's
//! [mapper](crate::guest::mappings), and the production paths are behind a system seam.
//! Production implements it with the safe `sys` wrappers; tests substitute temporary directories
//! and recording operations, including failures of mount, environment export, and `exec`, the
//! root operations' fake kernel for the mapper, and spawn workloads through the real launch with
//! its credential calls replaced, or with the stages of the sandbox
//! agent's entry simulated. The agent keeps the table's account in a
//! [`MappingTable`](crate::guest::mappings::MappingTable), as any guest agent may, which checks
//! each part that the host sends and hands its mappings to the agent's mapper.
//!
//! The direct agent uses [`init`](crate::guest::init)'s child builder and execution driver.
//! The sandbox agent keeps its entry, barrier, and [`exec::Execution`](crate::guest::exec::Execution).
//! Both build complete `argv` and `envp` before the child starts; the child performs no allocation
//! or Rust cleanup before `execve` or `_exit`. The execution reads the workload's output,
//! ends it, and reports its launch and outcome, and the
//! agent forwards the output as NVXC frames, serves the control session while the workload runs,
//! and reports the outcome. The agent keeps its own policy: the null device as the workload's
//! standard input, the identity, the output cap,
//! the timeout, the precedence of a cancellation, a timeout, and the cap over the workload's own
//! status, and the outcome that it reports for a failed launch stage. Both build the workload's
//! environment in memory, as C's environment functions changed it, and pass it to `execve`, so
//! that no process changes its
//! environment.
//!
//! The agent is its process's one status collector, as the direct agent must be as PID 1: its own
//! `waitpid(-1)` loop collects every child's status and hands the workload's to the workload's
//! execution. The direct core kills each workload's process group, including on normal completion.
//! The sandbox agent places each workload in the `container`
//! cgroup that the init agent prepared, and ends only the workload, whose end ends every process
//! of its PID namespace, and its process group.
//!
//! Linux arguments, paths, and environment strings are bytes, not necessarily UTF-8, so the
//! protocol's guest `EXEC` and `MAPS` decoders accept what the C guest accepts. The boot contract
//! is one `nvx_maps=COUNT` token from 1 through 4096, and retired `nvx_map=` tokens are ignored.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

use super::entry::{Entry, Identity, MachineId};
use super::status::{CommandError, ExitStatus};
use super::sys::{self, Errno};

/// Legacy sandbox workload placement.
mod cgroup;
/// C-compatible diagnostics.
mod diagnostics;
/// Long-lived control request dispatcher.
mod dispatch;
/// Environments that the agent builds for `execve`.
mod environment;
/// The NVXC adapter, which runs each workload as an execution, and its output cap, cancellation,
/// and timeout.
mod exec;
/// Direct and sandbox launch commands.
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
    /// The entry of the sandbox agent's executions, which the direct agent has none of.
    entry: Option<Entry>,
}

impl AgentConfig {
    /// Takes a layered cold boot's CLI identity and its already-resolved account defaults.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "SL7.7c selects layered cold boots")
    )]
    pub(crate) fn layered(
        command_line: &[u8],
        user: CString,
        home: CString,
    ) -> Result<Self, Errno> {
        let mut identities = crate::protocol::cmdline::init_tokens(command_line)
            .filter(|token| token.name == b"nvx_identity")
            .filter_map(|token| token.value);
        let identity = identities.next().ok_or(Errno::EINVAL)?;
        let mut ids = identity.split(|&byte| byte == b':');
        let (Some(uid), Some(gid), None) = (ids.next(), ids.next(), ids.next()) else {
            return Err(Errno::EINVAL);
        };
        if identities.next().is_some() {
            return Err(Errno::EINVAL);
        }
        launch::parse_id(uid)?;
        launch::parse_id(gid)?;
        Ok(Self {
            rootfs: c"-".to_owned(),
            hostname: c"nvx-managed".to_owned(),
            uid: sys::c_string(uid)?,
            gid: sys::c_string(gid)?,
            user,
            home,
            direct: true,
            entry: None,
        })
    }
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
    if arguments.len() != 8 {
        return Err(CommandError::Agent);
    }
    let mut config = AgentConfig {
        rootfs: arguments[2].clone(),
        hostname: arguments[3].clone(),
        uid: arguments[4].clone(),
        gid: arguments[5].clone(),
        user: arguments[6].clone(),
        home: arguments[7].clone(),
        direct: arguments[2].as_bytes() == b"-",
        entry: None,
    };
    if !config.direct {
        let entry = match sandbox_entry(system.paths(), &config) {
            Ok(entry) => entry,
            Err(error) => return Err(fatal(system, "workload-entry", error)),
        };
        if let Err(error) = system.bind_runtime(&entry) {
            return Err(fatal(system, "workload-entry", error));
        }
        config.entry = Some(entry);
    }
    let result = serve(system, &arguments[1], &config);
    finish(system, &config, result)
}

/// Returns the entry of the sandbox agent's executions: `ROOTFS`, entered as `UID` and `GID`
/// with their existing account, with `HOSTNAME`, the machine ID that it generates for this boot
/// at its path, and the guest's time status.
fn sandbox_entry(paths: &Paths, config: &AgentConfig) -> Result<Entry, Errno> {
    let uid = launch::parse_id(config.uid.as_bytes())?;
    let gid = launch::parse_id(config.gid.as_bytes())?;
    let rootfs = std::ffi::OsStr::from_bytes(config.rootfs.as_bytes());
    let entry = Entry::new(rootfs, Identity::new(uid, gid))
        .hostname(config.hostname.as_bytes())
        .time_status(&paths.time_status);
    entry.check().map_err(|_| Errno::EINVAL)?;
    let machine_id =
        MachineId::generate(&paths.machine_id).map_err(|error| sys::errno_of(&error))?;
    Ok(entry.machine_id(machine_id))
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
    let mut mappings = HostMappings::new(config);
    if let Err(error) = mappings::prepare(system, &mut mappings) {
        return Err(fatal(system, "host-mappings", error));
    }
    if let Err(error) = tty::configure(system, &fd) {
        return Err(fatal(system, "control-tty", error));
    }
    let mut session = Session::new(fd);
    let served = dispatch::run(system, &mut session, config, &mut mappings);
    // The sandbox agent unmounts its mappings before it returns to the init agent, which then
    // unmounts the root, and the direct agent powers the VM off with its mappings in place.
    let unmapped = if config.direct {
        Ok(())
    } else {
        mappings::teardown(system, &mut mappings)
    };
    match served {
        // The teardown reported each mapping that stays mounted.
        Ok(dispatch::Outcome::Stopped) => unmapped.map_err(|_| CommandError::Agent),
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

#[cfg(test)]
pub(crate) mod tests;
