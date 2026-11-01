use std::ffi::{CStr, CString};

use crate::guest::entry::Prepared;
use crate::guest::parse::parse_decimal;
use crate::guest::spawn::Command;
use crate::guest::sys::{self, Errno, FileDescriptor, Mode, OpenFlags, c_string};
use crate::protocol::GuestExecPayload;

use super::AgentConfig;
use super::environment;
use super::seam::{Paths, path_c_string};

/// One workload's launch inputs: its borrowed request, and the descriptors that its child takes.
pub(crate) struct LaunchRequest<'a> {
    /// Agent mode and configured workload identity.
    pub(crate) config: &'a AgentConfig,
    /// Decoded workload request.
    pub(crate) execution: &'a GuestExecPayload,
    /// Writer of the workload's standard output.
    pub(crate) stdout: FileDescriptor,
    /// Writer of the workload's standard error.
    pub(crate) stderr: FileDescriptor,
}

/// Builds the command that launches one workload of the direct agent.
///
/// The workload's standard input is the null device, its standard output and error are the
/// request's pipes, the sealed configuration `config_fd` stays open at its descriptor number, and
/// the child leads a new process group. It runs `setpriv`, which takes the workload's identity
/// and runs the helper; the child first joins the execution cgroup, then enters the working
/// directory as the workload and records the physical path in `PWD` for a noncanonical name. The
/// helper then applies the request's environment entries.
pub(crate) fn direct_command(
    paths: &Paths,
    request: LaunchRequest<'_>,
    config_fd: FileDescriptor,
) -> Result<Command, Errno> {
    let config = request.config;
    let directory = working_directory(request.execution)?;
    let uid = parse_id(config.uid.as_bytes())?;
    let gid = parse_id(config.gid.as_bytes())?;
    let helper = path_c_string(&paths.helper)?;
    let setpriv = path_c_string(&paths.setpriv)?;
    let config_argument = config_fd.argument();
    let mut arguments = Vec::new();
    for value in [
        b"setpriv".as_slice(),
        b"--reuid",
        config.uid.as_bytes(),
        b"--regid",
        config.gid.as_bytes(),
        b"--clear-groups",
        b"--no-new-privs",
        b"--bounding-set=-all",
        b"--inh-caps=-all",
        b"--ambient-caps=-all",
        helper.as_bytes(),
        b"--exec-config-fd",
        config_argument.as_bytes(),
        b"--",
    ] {
        arguments.push(c_string(value)?);
    }
    for argument in &request.execution.argv {
        arguments.push(c_string(argument)?);
    }
    let (environment, physical_pwd) = defaults(config, request.execution, &directory)?;
    let cgroup = super::cgroup::placement(paths)?;
    let mut command = Command::new(setpriv, arguments)
        .environment(environment)
        .stdin(null_device(paths)?)
        .stdout(request.stdout)
        .stderr(request.stderr)
        .inherit(config_fd)
        .new_process_group()
        .working_directory(directory)
        .check_working_directory_as(uid, gid)
        .cgroup(cgroup);
    if physical_pwd {
        command = command.export_physical_working_directory();
    }
    Ok(command)
}

/// Builds the command that launches one workload of the sandbox agent, which enters `prepared`.
///
/// The workload's standard input is the null device, and its standard output and error are the
/// request's pipes. The child waits at its barrier until the agent has placed it in its cgroup,
/// and then enters the execution, as the first process of its PID namespace and the leader of a
/// new session, enters the working directory as the workload, records the physical path in `PWD`
/// for a noncanonical name, and runs the workload itself. The agent builds the environment as the
/// direct agent and its helper do: its own, with the workload's `HOME`, `USER`, `LOGNAME`, and
/// `PWD`, and then the request's entries in order, which replace it unless they layer over it.
pub(crate) fn sandbox_command(
    paths: &Paths,
    request: LaunchRequest<'_>,
    prepared: Prepared,
) -> Result<Command, Errno> {
    let execution = request.execution;
    let directory = working_directory(execution)?;
    let (mut environment, mut physical_pwd) = defaults(request.config, execution, &directory)?;
    if let Some(entries) = &execution.environment {
        if !execution.inherit_default_env {
            environment.clear();
        }
        for entry in entries {
            let equals = entry
                .iter()
                .position(|&byte| byte == b'=')
                .ok_or(Errno::EINVAL)?;
            let (name, value) = (&entry[..equals], &entry[equals + 1..]);
            // An entry's `PWD` replaces the physical path, as it replaced the helper's.
            physical_pwd &= name != b"PWD";
            environment::set(&mut environment, name, value)?;
        }
    }
    let arguments = execution
        .argv
        .iter()
        .map(|argument| c_string(argument))
        .collect::<Result<Vec<_>, _>>()?;
    let program = arguments.first().cloned().ok_or(Errno::EINVAL)?;
    let mut command = Command::new(program, arguments)
        .environment(environment)
        .stdin(null_device(paths)?)
        .stdout(request.stdout)
        .stderr(request.stderr)
        .working_directory(directory)
        .barrier()
        .enter(prepared);
    if physical_pwd {
        command = command.export_physical_working_directory();
    }
    Ok(command)
}

/// Returns the request's working directory, or `/` without one.
fn working_directory(execution: &GuestExecPayload) -> Result<CString, Errno> {
    c_string(execution.cwd.as_deref().unwrap_or(b"/"))
}

/// Opens the null device, the workload's standard input.
fn null_device(paths: &Paths) -> Result<FileDescriptor, Errno> {
    sys::open(
        &path_c_string(&paths.null_device)?,
        OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )
}

/// Returns the agent's environment with the workload's `HOME`, `USER`, and `LOGNAME`, and, unless
/// the request replaces the environment, its `PWD`: the name of the working directory `directory`
/// when it is canonical, and otherwise a placeholder whose place the directory's physical path
/// takes once the child has entered it, which the returned flag asks for.
fn defaults(
    config: &AgentConfig,
    execution: &GuestExecPayload,
    directory: &CStr,
) -> Result<(Vec<CString>, bool), Errno> {
    let mut environment = environment::inherited()?;
    environment::set(&mut environment, b"HOME", config.home.as_bytes())?;
    environment::set(&mut environment, b"USER", config.user.as_bytes())?;
    environment::set(&mut environment, b"LOGNAME", config.user.as_bytes())?;
    let mut physical_pwd = false;
    if execution.environment.is_none() || execution.inherit_default_env {
        let directory = directory.to_bytes();
        environment::set(&mut environment, b"PWD", directory)?;
        physical_pwd = directory != b"/" && !safe_path(directory);
    }
    Ok((environment, physical_pwd))
}

/// Returns whether an absolute path has only canonical nonempty components.
pub(crate) fn safe_path(path: &[u8]) -> bool {
    let Some(components) = path.strip_prefix(b"/") else {
        return false;
    };
    !components.is_empty()
        && components
            .split(|&byte| byte == b'/')
            .all(|component| !matches!(component, b"" | b"." | b".."))
}

/// Parses one nonzero decimal UID or GID while excluding the unchanged-ID sentinel.
pub(super) fn parse_id(text: &[u8]) -> Result<u32, Errno> {
    parse_decimal(text)
        .filter(|&id| id != 0 && id < u64::from(u32::MAX))
        .and_then(|id| u32::try_from(id).ok())
        .ok_or(Errno::EINVAL)
}
#[cfg(test)]
mod tests;
