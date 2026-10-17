//! The identity of the OpenVMM process of a running sandbox, as `sandbox_lifecycle.py` records and
//! checks it: `_process_start_time` and `_process_running`.
//!
//! A runtime record names OpenVMM by its process ID and start time, which together never repeat:
//! on Linux the `starttime` field of `/proc/PID/stat`, in clock ticks since boot, and on Windows
//! the creation time of the process as a 64-bit `FILETIME`. Both implementations compute the same
//! value, so a record that either wrote identifies the process for the other.

use crate::host::cli::{Exit, Result};
use crate::host::python::builtins::Int;
#[cfg(any(target_os = "linux", windows))]
use crate::host::python::fs::os_error_message;

/// Returns the start time of a live process, or `None` if no live process has the ID, as
/// `_process_start_time` does, or the message of the error with which the state of the process
/// cannot be determined.
///
/// A zombie counts as exited. On Windows, so does a process that the caller cannot open: OpenVMM
/// runs as the caller, so such a process cannot be its VM. Other hosts have no `/proc`, so every
/// process counts as exited there, as in Python.
pub(crate) fn process_start_time(pid: &Int) -> std::result::Result<Option<u64>, String> {
    #[cfg(target_os = "linux")]
    {
        let path = format!("/proc/{pid}/stat");
        let stat = match std::fs::read(&path) {
            Ok(stat) => stat,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(os_error_message(&error, Some(&path))),
        };
        linux_start_time(&stat)
    }
    #[cfg(windows)]
    {
        let Some(pid) = pid.to_u32() else {
            return Ok(None);
        };
        crate::host::platform::process_start_time(pid)
            .map_err(|error| os_error_message(&error, None))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = pid;
        Ok(None)
    }
}

/// Reads the start time from the `/proc/PID/stat` line of a process, as `_linux_start_time`
/// does: the command name may contain spaces and parentheses, so the fields after it start at
/// its last `)`, with the state as field 3 and `starttime` as field 22.
#[cfg(any(target_os = "linux", test))]
fn linux_start_time(stat: &[u8]) -> std::result::Result<Option<u64>, String> {
    let unexpected = || "process status has an unexpected format".to_owned();
    let fields: Vec<&[u8]> = match stat.iter().rposition(|&byte| byte == b')') {
        Some(end) => stat[end + 1..]
            .split(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
            .filter(|field| !field.is_empty())
            .collect(),
        None => Vec::new(),
    };
    let start = fields.get(19).copied().unwrap_or_default();
    if fields.len() < 20 || start.is_empty() || !start.iter().all(u8::is_ascii_digit) {
        return Err(unexpected());
    }
    if matches!(fields[0], b"Z" | b"X") {
        return Ok(None);
    }
    std::str::from_utf8(start)
        .ok()
        .and_then(|start| start.parse().ok())
        .map(Some)
        .ok_or_else(unexpected)
}

/// Returns whether the OpenVMM process that a runtime record names still runs, as
/// `_process_running` does. A record without a start time, written by an earlier NVX version,
/// identifies OpenVMM by its process ID alone.
pub(crate) fn process_running(pid: &Int, start_time: Option<&Int>) -> Result<bool> {
    if !pid.is_positive() {
        return Ok(false);
    }
    let current = process_start_time(pid).map_err(|message| {
        Exit::error(format!(
            "cannot determine whether OpenVMM process {pid} is running: {message}"
        ))
    })?;
    Ok(match start_time {
        None => current.is_some(),
        Some(start_time) => current.map(Int::from).as_ref() == Some(start_time),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_lines_give_the_start_time_of_live_processes() {
        let line = |state: &str| {
            format!(
                "123 (my (odd) prog) {state} 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242 19 20\n"
            )
        };
        assert_eq!(linux_start_time(line("S").as_bytes()), Ok(Some(4242)));
        assert_eq!(linux_start_time(line("R").as_bytes()), Ok(Some(4242)));
        assert_eq!(linux_start_time(line("Z").as_bytes()), Ok(None));
        assert_eq!(linux_start_time(line("X").as_bytes()), Ok(None));
        for malformed in [
            &b"123 (prog) S 1 2"[..],
            b"no parenthesis",
            b"1 (p) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 x",
        ] {
            assert_eq!(
                linux_start_time(malformed),
                Err("process status has an unexpected format".to_owned())
            );
        }
    }

    #[test]
    fn this_process_runs_and_others_are_identified_by_start_time() {
        let pid = Int::from(std::process::id());
        let start_time = process_start_time(&pid).unwrap();
        if cfg!(any(target_os = "linux", windows)) {
            let start_time = Int::from(start_time.expect("this process runs"));
            assert!(process_running(&pid, Some(&start_time)).unwrap());
            assert!(!process_running(&pid, Some(&start_time.plus_one())).unwrap());
            assert!(process_running(&pid, None).unwrap());
        }
        assert!(!process_running(&Int::zero(), None).unwrap());
        assert!(!process_running(&Int::parse("-5").unwrap(), None).unwrap());
        assert!(!process_running(&Int::parse("99999999999999999999").unwrap(), None).unwrap());
    }
}
