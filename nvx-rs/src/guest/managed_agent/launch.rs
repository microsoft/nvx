use std::ffi::{CStr, CString};

use crate::guest::init::{AccountMode, ExecRequest};
use crate::guest::parse::parse_decimal;
use crate::guest::sys::{Errno, c_string};
use crate::protocol::GuestExecPayload;

use super::AgentConfig;
use super::environment;

/// Describes one workload for the init-owned plain-process core.
///
/// The request reads the null device as standard input, which the agent's launch replaces with a
/// pipe, since every execution streams its input; its standard output and error are the core's
/// pipes. The plain-process core resets the child's priority and credentials, enters its working
/// directory, and executes the workload in its own process group.
pub(crate) fn direct_request(
    config: &AgentConfig,
    execution: &GuestExecPayload,
) -> Result<ExecRequest, Errno> {
    let directory = working_directory(execution)?;
    let (environment, physical_pwd) = workload_environment(config, execution, &directory)?;
    let arguments = execution
        .argv
        .iter()
        .map(|argument| c_string(argument))
        .collect::<Result<Vec<_>, _>>()?;
    let program = arguments.first().cloned().ok_or(Errno::EINVAL)?;
    Ok(ExecRequest {
        program,
        arguments,
        environment,
        directory,
        uid: parse_id(config.uid.as_bytes())?,
        gid: parse_id(config.gid.as_bytes())?,
        account: AccountMode::Numeric,
        timeout: None,
        input: false,
        search_path: None,
        physical_pwd,
    })
}

/// Returns the request's working directory, or `/` without one.
fn working_directory(execution: &GuestExecPayload) -> Result<CString, Errno> {
    c_string(execution.cwd.as_deref().unwrap_or(b"/"))
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
    if let Some(proxy) = &config.restored_proxy {
        for name in [
            b"http_proxy".as_slice(),
            b"https_proxy",
            b"HTTP_PROXY",
            b"HTTPS_PROXY",
        ] {
            match proxy {
                Some(proxy) => environment::set(&mut environment, name, proxy.as_bytes())?,
                None => environment::remove(&mut environment, name),
            }
        }
    }
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

/// Applies the request's exact or layered entries after the workload defaults, in order.
fn workload_environment(
    config: &AgentConfig,
    execution: &GuestExecPayload,
    directory: &CStr,
) -> Result<(Vec<CString>, bool), Errno> {
    let (mut environment, mut physical_pwd) = defaults(config, execution, directory)?;
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
            physical_pwd &= name != b"PWD";
            environment::set(&mut environment, name, value)?;
        }
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
