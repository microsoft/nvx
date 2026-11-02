use std::ffi::{CStr, CString};

use crate::guest::status::CommandError;
use crate::guest::sys::{Errno, c_string};

use super::diagnostics::{with_description, write_stderr};
use super::environment;
use super::exec_config::{self, HelperConfig};
use super::seam::System;

/// Runs the direct agent's short-lived execution-helper mode, which applies the environment of
/// its sealed configuration and executes the workload, in the working directory that the agent's
/// child entered.
///
/// Every failure is [`CommandError::Agent`]. As in the C helper, only an `exec` failure prints
/// why.
pub(crate) fn run(system: &mut impl System, arguments: &[CString]) -> Result<(), CommandError> {
    if arguments.len() < 5
        || arguments[1].as_bytes() != b"--exec-config-fd"
        || arguments[3].as_bytes() != b"--"
        || arguments[4].as_bytes().first() != Some(&b'/')
    {
        return Err(CommandError::Agent);
    }
    let config = read_config(system, &arguments[2]).map_err(|_| CommandError::Agent)?;
    let mut environment = environment::inherited().map_err(|_| CommandError::Agent)?;
    if let Some(entries) = &config.environment {
        if !config.inherit_default_env {
            environment.clear();
        }
        for entry in entries {
            put(system, &mut environment, entry).map_err(|_| CommandError::Agent)?;
        }
    }
    if let Err(error) = system.exec(&arguments[4], &arguments[4..], Some(&environment)) {
        write_stderr(&with_description(
            b"nvx-managed-agent: cannot execute workload: ",
            error,
        ));
        return Err(CommandError::Agent);
    }
    Ok(())
}

/// Reads the sealed configuration through the inherited descriptor that `text` names.
///
/// The configuration's descriptors close no later than the workload's `exec`, as the C helper
/// closed its descriptor before it.
fn read_config(system: &mut impl System, text: &CStr) -> Result<HelperConfig, Errno> {
    let descriptor = system.take_inherited_descriptor(text)?;
    exec_config::read(&descriptor)
}

/// Sets the variable of one validated `NAME=value` configuration entry, as C's `putenv` did.
fn put(
    system: &mut impl System,
    environment: &mut Vec<CString>,
    entry: &[u8],
) -> Result<(), Errno> {
    let equals = entry
        .iter()
        .position(|&byte| byte == b'=')
        .ok_or(Errno::EINVAL)?;
    let (name, value) = (&entry[..equals], &entry[equals + 1..]);
    system.validate_environment(&c_string(name)?, &c_string(value)?)?;
    environment::set(environment, name, value)
}

#[cfg(test)]
mod tests;
