//! `nvx-managed-agent CONTROL_TTY ROOTFS HOSTNAME UID GID USER HOME`: the managed guest agent,
//! which serves NVX's authenticated control session on `CONTROL_TTY` and runs one workload at a
//! time as `UID` and `GID`.
//!
//! With `ROOTFS` `-`, the direct agent runs as PID 1: it applies the host mapping table beneath
//! its own root, `/`, launches each workload in its own cgroup and process group, reaps orphaned
//! processes, and powers the VM off after `STOP`. Otherwise the sandbox agent applies the host
//! mapping table beneath `ROOTFS`, the container's assembled root, has each workload enter
//! `ROOTFS` as an [`entry`](crate::guest::entry) execution, as `UID` and `GID` with their existing
//! account, with `HOSTNAME`, held at a barrier until the agent has placed it in its cgroup, and
//! after `STOP` unmounts the table and returns to the init agent. On a guest with `/dev/kvm`, which
//! only nested virtualization gives it, each workload's `/dev` holds `/dev/kvm` too, which only the
//! workload's group may open. When it starts, it generates the
//! workload's machine ID and binds it, with the guest's time status, into `ROOTFS` in a mount
//! namespace of its own, of which each execution's starts as a copy, so that the binds end with
//! the agent, and it maps the table in that namespace too. Both refuse workloads until the table
//! is complete, and both keep the C agent's command lines, wire behavior, status and error
//! categories, 1 MiB output and receive windows, and timeout and cancellation behavior.
//!
//! `nvx-managed-agent --exec-config-fd FD -- WORKLOAD...` is the direct agent's internal
//! execution helper. The launch runs it once the workload's identity applies, and it applies the
//! environment of the sealed configuration in descriptor `FD`, then executes the workload. The
//! agent and the helper exit with 0, or with 125 when they fail.
//!
//! # Structure
//!
//! The submodules divide the agent by concern. Mounts, ownership and mode changes, the diagnostic
//! port, TTY operations, the helper's inherited descriptor and `exec`, the workload launch, the
//! preparation of each sandbox execution, the kernel of the host mapping table's
//! [mapper](crate::guest::mappings), and the production paths are behind a system seam.
//! Production implements it with the safe `sys` wrappers; tests substitute temporary directories
//! and recording operations, including failures of mount, environment export, and `exec`, the
//! root operations' fake kernel for the mapper, and spawn workloads through the real launch with
//! its credential calls replaced and a stand-in `setpriv`, or with the stages of the sandbox
//! agent's entry simulated. The agent keeps the table's account in a
//! [`MappingTable`](crate::guest::mappings::MappingTable), as any guest agent may, which checks
//! each part that the host sends and hands its mappings to the agent's mapper.
//!
//! The agent launches each workload as one caller of [`spawn`](crate::guest::spawn): it builds
//! the workload's command, with the complete `argv` and `envp`, its descriptors, in direct mode
//! its working-directory check and cgroup, and in sandbox mode its entry and barrier, and the
//! spawned child performs no allocation or Rust cleanup before `execve` or `_exit`. It then drives
//! the workload as one [`exec::Execution`](crate::guest::exec::Execution), its NVXC adapter: the
//! execution reads the workload's output, ends it, and reports its launch and outcome, and the
//! agent forwards the output as NVXC frames, serves the control session while the workload runs,
//! and reports the outcome. The agent keeps its own policy: the null device as the workload's
//! standard input, the identity, the direct agent's `setpriv` and helper stages, the output cap,
//! the timeout, the precedence of a cancellation, a timeout, and the cap over the workload's own
//! status, and the outcome that it reports for a failed launch stage. The sandbox agent and the
//! direct agent's helper build the workload's environment in memory, as C's environment
//! functions changed the helper's own, and pass it to `execve`, so that no process changes its
//! environment.
//!
//! The agent is its process's one status collector, as the direct agent must be as PID 1: its own
//! `waitpid(-1)` loop collects every child's status and hands the workload's to the workload's
//! execution, which otherwise collects it by its pidfd. Its cgroups are a
//! [`cgroup::Layout`](crate::guest::cgroup::Layout): the direct agent runs in the hierarchy's
//! root and its workloads in `nvx-exec`, which it kills and settles whole through an
//! [`ExecutionCgroup`](crate::guest::cgroup::ExecutionCgroup), and which each execution kills
//! when it ends its workload, and the sandbox agent places each workload in the `container`
//! cgroup that the init agent prepared, and ends only the workload, whose end ends every process
//! of its PID namespace, and its process group.
//!
//! Linux arguments, paths, and environment strings are bytes, not necessarily UTF-8, so the
//! protocol's guest `EXEC` and `MAPS` decoders accept what the C guest accepts. The boot contract
//! is one `nvx_maps=COUNT` token from 1 through 4096, and retired `nvx_map=` tokens are ignored.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

use super::entry::{DeviceNode, Entry, Identity, MachineId};
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
/// Sealed private configuration of the direct agent's helper.
mod exec_config;
/// Short-lived post-privilege-drop helper mode of the direct agent.
mod helper;
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
/// at its path, the guest's time status, and, on a guest whose KVM device is a character device,
/// `/dev/kvm`.
fn sandbox_entry(paths: &Paths, config: &AgentConfig) -> Result<Entry, Errno> {
    let uid = launch::parse_id(config.uid.as_bytes())?;
    let gid = launch::parse_id(config.gid.as_bytes())?;
    let rootfs = std::ffi::OsStr::from_bytes(config.rootfs.as_bytes());
    let mut entry = Entry::new(rootfs, Identity::new(uid, gid))
        .hostname(config.hostname.as_bytes())
        .time_status(&paths.time_status);
    if has_character_device(&paths.kvm_device)? {
        entry = entry.devices(DeviceNode::ALL.into_iter().chain([DeviceNode::Kvm]));
    }
    entry.check().map_err(|_| Errno::EINVAL)?;
    let machine_id =
        MachineId::generate(&paths.machine_id).map_err(|error| sys::errno_of(&error))?;
    Ok(entry.machine_id(machine_id))
}

/// Returns whether `path` is a character device, without following a symbolic link: the guest's
/// KVM device exists only where its machine has nested virtualization, and its kernel KVM.
fn has_character_device(path: &std::path::Path) -> Result<bool, Errno> {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_char_device()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(sys::errno_of(&error)),
    }
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
