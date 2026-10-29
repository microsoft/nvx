//! The OpenVMM process of a one-shot run, which runs on the terminal until it exits or the run is
//! interrupted.

#[cfg(not(windows))]
use std::process::{Child, Command};
use std::thread;
use std::time::Duration;
#[cfg(not(windows))]
use std::time::Instant;

use crate::ErrorCode;
use crate::host::cli::download::Hold;
use crate::host::cli::{Exit, Result};
use crate::host::platform;
use crate::host::supervisor::Inherited;

/// Runs OpenVMM on the terminal, as `subprocess.run(command)` does, and returns its status. The
/// command keeps `hold` until OpenVMM has started, so that no download replaces the artifacts that
/// it read for OpenVMM before then, and not while OpenVMM runs, however long it runs.
///
/// OpenVMM inherits the descriptors or handles `inherit`, under their numbers in this process, and
/// no other of this process beyond its standard streams, as with `subprocess.run`, which closes
/// the others on Linux and passes none on Windows: `inherit` names the checked files of the run,
/// and on Windows the claim's handle of its scratch image too, so that OpenVMM's handle list holds
/// copies of this process's standard handles and those handles alone. On Windows, OpenVMM opens
/// the checked files by their final paths, which the checked files, and OpenVMM's copies of them,
/// hold in place. `nvx-rs run`, which holds no checked files, passes none, as `nvx.py run` passed
/// none to `subprocess.run`.
///
/// OpenVMM shares the terminal and its process group, so Ctrl-C reaches it too. As
/// `subprocess.run` does, the CLI then gives it 0.25 s to exit on Unix before it kills it, and on
/// Windows, where Python cannot interrupt its wait, waits for it to exit; either way the command
/// ends as interrupted, also when OpenVMM exited first, since Python's wait raises
/// `KeyboardInterrupt` once Ctrl-C arrives.
pub(in crate::host::cli) fn run_openvmm(
    command: &[String],
    inherit: &[Inherited<'_>],
    hold: Hold,
) -> Result<i32> {
    let mut openvmm = OneShot::spawn(command, inherit)?;
    drop(hold);
    loop {
        let status = openvmm.poll()?;
        if platform::interrupted() {
            if status.is_none() {
                end_interrupted(&mut openvmm)?;
            }
            return Err(Exit::Interrupted);
        }
        if let Some(status) = status {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// The OpenVMM process of a one-shot run.
struct OneShot {
    /// The process, which `std::process` started.
    #[cfg(not(windows))]
    child: Child,
    /// The process, which [`platform::spawn_foreground`] started.
    #[cfg(windows)]
    process: platform::LaunchedProcess,
}

impl OneShot {
    /// Starts OpenVMM with `command`, with the descriptors or handles `inherit`, as
    /// [`run_openvmm`] says.
    fn spawn(command: &[String], inherit: &[Inherited<'_>]) -> Result<Self> {
        let failed = |error: std::io::Error| {
            // The host's own refusals of a command, such as of a batch file on Windows, carry no
            // OS error.
            let refused =
                error.kind() == std::io::ErrorKind::InvalidInput && error.raw_os_error().is_none();
            let code = if refused {
                ErrorCode::Unsupported
            } else {
                ErrorCode::Io
            };
            Exit::error(code, spawn_error(&error, &command[0]))
        };
        #[cfg(windows)]
        {
            let arguments: Vec<std::ffi::OsString> = command[1..].iter().map(Into::into).collect();
            platform::spawn_foreground(std::path::Path::new(&command[0]), &arguments, inherit)
                .map(|process| Self { process })
                .map_err(failed)
        }
        #[cfg(not(windows))]
        {
            let mut child = Command::new(&command[0]);
            child.args(&command[1..]);
            platform::inherit_only(&mut child, inherit).map_err(failed)?;
            child.spawn().map(|child| Self { child }).map_err(failed)
        }
    }

    /// Returns OpenVMM's status if it has exited: `Popen.returncode`, which on Unix is the
    /// negated signal of a process that a signal ended, and on Windows the status of
    /// `GetExitCodeProcess`, as `std::process::ExitStatus::code` reads it.
    fn poll(&mut self) -> Result<Option<i32>> {
        #[cfg(windows)]
        {
            let status = self
                .process
                .wait_timeout(Duration::ZERO)
                .map_err(Exit::from_os_error)?;
            Ok(status.map(|status| status as i32))
        }
        #[cfg(not(windows))]
        {
            let status = self.child.try_wait().map_err(Exit::from_os_error)?;
            Ok(status.map(|status| {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;

                    status
                        .code()
                        .unwrap_or_else(|| -status.signal().unwrap_or_default())
                }
                #[cfg(not(unix))]
                status.code().unwrap_or_default()
            }))
        }
    }
}

/// Ends the OpenVMM of an interrupted run as `subprocess.run` does: on Unix, it waits 0.25 s for
/// OpenVMM to exit and then kills it, and on Windows, it waits for OpenVMM to exit.
fn end_interrupted(openvmm: &mut OneShot) -> Result {
    #[cfg(not(windows))]
    let deadline = Instant::now() + Duration::from_millis(250);
    while openvmm.poll()?.is_none() {
        #[cfg(not(windows))]
        if cfg!(unix) && Instant::now() >= deadline {
            let _ = openvmm.child.kill();
            let _ = openvmm.child.wait();
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Returns the message of the I/O error with which the host fails to start `program`: the
/// program, quoted, and the error, as [`io_message`](crate::host::cli::io_message) writes them,
/// or the error alone for a refusal of the host's own, such as of a batch file on Windows, which
/// names the program itself.
pub(crate) fn spawn_error(error: &std::io::Error, program: &str) -> String {
    match error.raw_os_error() {
        Some(_) => crate::host::cli::io_message(error, std::path::Path::new(program)),
        None => error.to_string(),
    }
}
