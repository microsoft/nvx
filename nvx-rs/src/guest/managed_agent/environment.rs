//! Environments that the agent builds in memory and passes to `execve`.
//!
//! An environment changes as C's own did under musl's environment functions: setting a variable,
//! as `setenv` and `putenv` do, replaces its first entry in place or appends one.

use std::ffi::CString;
use std::os::unix::ffi::OsStringExt;

use crate::guest::sys::{Errno, c_string};

/// Copies the current process environment as `NAME=value` entries.
pub(super) fn inherited() -> Result<Vec<CString>, Errno> {
    std::env::vars_os()
        .map(|(name, value)| {
            let mut entry = name.into_vec();
            entry.push(b'=');
            entry.extend(value.into_vec());
            c_string(&entry)
        })
        .collect()
}

/// Sets `name` to `value`, or `EINVAL` if either contains a NUL.
pub(super) fn set(environment: &mut Vec<CString>, name: &[u8], value: &[u8]) -> Result<(), Errno> {
    let mut entry = Vec::with_capacity(name.len() + value.len() + 1);
    entry.extend_from_slice(name);
    entry.push(b'=');
    entry.extend_from_slice(value);
    let entry = c_string(&entry)?;
    match environment.iter_mut().find(|current| names(current, name)) {
        Some(current) => *current = entry,
        None => environment.push(entry),
    }
    Ok(())
}

/// Removes every inherited entry for `name`.
pub(super) fn remove(environment: &mut Vec<CString>, name: &[u8]) {
    environment.retain(|entry| !names(entry, name));
}

/// Returns whether `entry` sets the variable `name`.
fn names(entry: &CString, name: &[u8]) -> bool {
    entry
        .as_bytes()
        .strip_prefix(name)
        .is_some_and(|rest| rest.first() == Some(&b'='))
}

#[cfg(test)]
mod tests;
