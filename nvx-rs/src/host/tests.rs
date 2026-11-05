//! Helpers that the host's unit tests share.

#[cfg(windows)]
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// How long a test waits for a lock or claim that it released to be free.
///
/// A process that a concurrent test spawns holds a copy of each descriptor of the test process
/// from its fork until it executes its program, and with it the lock or claim of a file that a
/// test had open, for that moment.
const RELEASE_WAIT: Duration = Duration::from_secs(10);

/// Interval at which a test checks a lock or claim that it released again, the interval at which
/// the host retries a lock.
const RELEASE_RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// Returns whether `free` holds before [`RELEASE_WAIT`] passes, checking it again at every
/// [`RELEASE_RETRY_INTERVAL`] until then.
pub(crate) fn released(mut free: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + RELEASE_WAIT;
    while !free() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(RELEASE_RETRY_INTERVAL);
    }
    true
}

/// Spawns `command` under a `SpawnLock`, which it releases once the process exists, so that on
/// Windows the process inherits no handle of a VMM that a concurrent test spawns, such as the
/// VMM's log, which it would hold until it exits.
#[cfg(any(target_os = "linux", windows))]
pub(crate) fn spawn_locked(
    command: &mut std::process::Command,
) -> std::io::Result<std::process::Child> {
    let _lock = super::platform::inherit::lock_spawns();
    command.spawn()
}

/// Makes `link` a junction to the directory `target`, which any user may create, with `cmd`'s
/// `mklink /J`, quoting both paths, since `cmd` splits its arguments at commas and semicolons.
///
/// `cmd` starts under a `SpawnLock`, so that it inherits no handle of a VMM that a concurrent test
/// spawns, such as the VMM's log, which it would hold until it exits.
#[cfg(windows)]
pub(crate) fn junction(link: &Path, target: &Path) {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut command = Command::new("cmd");
    command
        .raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            target.display()
        ))
        .stdout(Stdio::null());
    let status = spawn_locked(&mut command).unwrap().wait().unwrap();
    assert!(status.success(), "cannot link {}", link.display());
}
