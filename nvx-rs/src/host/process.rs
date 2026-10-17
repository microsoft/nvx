//! Launch and supervision of OpenVMM processes.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
#[cfg(not(windows))]
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::platform;
use crate::protocol::CAPABILITY_LEN;

/// Longest wait for a terminated OpenVMM process to exit.
const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// Interval at which process shutdown and identity are polled.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// An OpenVMM process this caller launched and has not released yet.
///
/// Release it with [`detach_child`], or terminate it with [`kill_child`].
pub struct Launched {
    #[cfg(not(windows))]
    /// Child process retained until it is detached or terminated.
    child: Child,
    #[cfg(windows)]
    /// Owned Windows process handle and identifier.
    process: platform::LaunchedProcess,
}

impl Launched {
    /// Returns the process ID.
    pub fn id(&self) -> u32 {
        #[cfg(not(windows))]
        return self.child.id();
        #[cfg(windows)]
        return self.process.id();
    }

    /// Waits up to `timeout` for the process to exit, and returns its status if it has, as
    /// `Popen.wait` and `Popen.poll` report it: the exit code, or on Unix the negated number of
    /// the signal that ended the process.
    #[cfg(feature = "cli")]
    pub(crate) fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<i64>> {
        #[cfg(windows)]
        return Ok(self.process.wait_timeout(timeout)?.map(i64::from));
        #[cfg(not(windows))]
        {
            let deadline = Instant::now().checked_add(timeout);
            loop {
                if let Some(status) = self.child.try_wait()? {
                    return Ok(Some(status_value(status)));
                }
                let now = Instant::now();
                match deadline {
                    Some(deadline) if now >= deadline => return Ok(None),
                    Some(deadline) => thread::sleep((deadline - now).min(POLL_INTERVAL)),
                    None => thread::sleep(POLL_INTERVAL),
                }
            }
        }
    }

    /// Asks the process to end, as `Popen.terminate` does: with `SIGTERM` on Unix, and by
    /// terminating it with status 1 on Windows. A process that has exited is left alone.
    #[cfg(feature = "cli")]
    pub(crate) fn terminate(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        return self.process.request_termination();
        #[cfg(not(windows))]
        self.signal(nix::sys::signal::Signal::SIGTERM)
    }

    /// Ends the process, as `Popen.kill` does: with `SIGKILL` on Unix, and by terminating it with
    /// status 1 on Windows. A process that has exited is left alone.
    #[cfg(feature = "cli")]
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        return self.process.request_termination();
        #[cfg(not(windows))]
        self.signal(nix::sys::signal::Signal::SIGKILL)
    }

    /// Sends `signal` to the process unless it has exited. The child is not reaped until it is
    /// waited for, so its process ID cannot name another process meanwhile.
    #[cfg(all(feature = "cli", not(windows)))]
    fn signal(&mut self, signal: nix::sys::signal::Signal) -> io::Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        let pid = i32::try_from(self.child.id()).map_err(io::Error::other)?;
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// Returns `Popen.returncode` for `status`: the exit code, or the negated signal number.
#[cfg(all(feature = "cli", not(windows)))]
fn status_value(status: std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;

    match (status.code(), status.signal()) {
        (Some(code), _) => i64::from(code),
        (None, Some(signal)) => -i64::from(signal),
        (None, None) => 0,
    }
}

/// Starts OpenVMM detached from the caller.
///
/// OpenVMM reads its capability from standard input as soon as it starts, so the capability is
/// written into a pipe whose write end is closed before the spawn. The capability never appears
/// in arguments, the environment, or the log. OpenVMM output goes to `log`, and OpenVMM inherits
/// no other handle of the caller, so a caller whose own output is a pipe sees end-of-file when it
/// exits rather than when the VM does.
///
/// On Windows, `breakaway_from_job` starts OpenVMM outside the caller's job object, which must
/// permit breakaway, so that the VM survives the caller.
pub fn spawn(
    openvmm: &Path,
    breakaway_from_job: bool,
    arguments: &[OsString],
    capability: &[u8; CAPABILITY_LEN],
    log: File,
    working_dir: &Path,
) -> io::Result<Launched> {
    let (stdin, mut writer) = io::pipe()?;
    writer.write_all(capability)?;
    drop(writer);
    let stderr = log.try_clone()?;
    #[cfg(windows)]
    {
        platform::spawn_detached(
            openvmm,
            arguments,
            working_dir,
            [stdin.into(), log.into(), stderr.into()],
            breakaway_from_job,
        )
        .map(|process| Launched { process })
    }
    #[cfg(not(windows))]
    {
        // Rust opens its own descriptors close-on-exec, and `platform::detach` marks any other
        // descriptor of the caller, so OpenVMM receives only these.
        let mut command = Command::new(openvmm);
        command
            .args(arguments)
            .current_dir(working_dir)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        platform::detach(&mut command, breakaway_from_job);
        command.spawn().map(|child| Launched { child })
    }
}

/// Releases a launched process without waiting for it.
///
/// On Unix a background thread reaps the child once it exits so it does not linger as a zombie
/// while this process lives. If that thread cannot start, the child becomes a zombie after it
/// exits; liveness checks treat zombies without threads as exited, so only the process table
/// entry leaks. The OpenVMM process keeps running either way.
pub fn detach_child(launched: Launched) {
    #[cfg(not(windows))]
    {
        let mut child = launched.child;
        let _ = thread::Builder::new()
            .name("nvx-openvmm-reaper".to_owned())
            .spawn(move || {
                let _ = child.wait();
            });
    }
    #[cfg(windows)]
    drop(launched);
}

/// Kills a launched process that was never recorded, through its handle, which cannot race with
/// process ID reuse. Returns whether it exited within ten seconds.
pub fn kill_child(launched: Launched) -> bool {
    #[cfg(windows)]
    return launched.process.terminate(KILL_TIMEOUT);
    #[cfg(not(windows))]
    {
        let mut child = launched.child;
        if child.kill().is_ok() {
            let deadline = Instant::now() + KILL_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return true,
                    Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
                    _ => break,
                }
            }
        }
        detach_child(Launched { child });
        false
    }
}

/// Waits until the process with the given identity is gone, returning whether it exited before
/// `deadline`. Inconclusive checks count as still running.
pub fn wait_for_exit(pid: u32, start_time: u64, deadline: Instant) -> bool {
    loop {
        match platform::process_start_time(pid) {
            Ok(Some(current)) if current == start_time => {}
            Ok(_) => return true,
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Kills the process and waits up to ten seconds for it to disappear.
pub fn kill(pid: u32, start_time: u64) -> io::Result<bool> {
    platform::kill_process(pid, start_time)?;
    Ok(wait_for_exit(
        pid,
        start_time,
        Instant::now() + KILL_TIMEOUT,
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::fs;
    use std::os::fd::AsRawFd;

    use super::*;
    use crate::host::sys;

    /// Spawns a long-running child while this process holds an inheritable pipe writer, as a
    /// descriptor that the caller's parent or native code left without close-on-exec would be.
    /// Returns whether the child holds a copy, as its own descriptor table shows. Unlike the
    /// pipe reaching end-of-file, that cannot depend on the children of concurrent tests, which
    /// may inherit the writer too.
    fn child_inherits_writer(spawn: impl FnOnce() -> Child) -> bool {
        let (pipe, child) = {
            let (_, writer) = io::pipe().unwrap();
            sys::clear_close_on_exec(&writer).unwrap();
            let pipe = fs::read_link(format!("/proc/self/fd/{}", writer.as_raw_fd())).unwrap();
            (pipe, Reaped(spawn()))
        };
        // `spawn` returns once the child has executed its program, which closed the child's
        // close-on-exec copies, so a copy that remains was inherited.
        fs::read_dir(format!("/proc/{}/fd", child.0.id()))
            .unwrap()
            .any(|entry| fs::read_link(entry.unwrap().path()).is_ok_and(|target| target == pipe))
    }

    fn sleeper() -> Command {
        let mut command = Command::new("sleep");
        command.arg("60").stdin(Stdio::null());
        command
    }

    #[test]
    fn openvmm_inherits_no_descriptor_beyond_its_standard_streams() {
        // A plain child keeps the writer, which shows that the check detects a leak.
        let plain = || sleeper().spawn().unwrap();
        assert!(child_inherits_writer(plain));

        let directory = tempfile::tempdir().unwrap();
        let openvmm = || {
            let log = File::create(directory.path().join("openvmm.log")).unwrap();
            let arguments = [OsString::from("60")];
            let capability = [1; CAPABILITY_LEN];
            let launched = spawn(
                Path::new("sleep"),
                false,
                &arguments,
                &capability,
                log,
                directory.path(),
            );
            launched.unwrap().child
        };
        assert!(!child_inherits_writer(openvmm));

        // Kernels without CLOSE_RANGE_CLOEXEC take the /proc/self/fd fallback.
        let fallback = || {
            let mut command = sleeper();
            sys::mark_listed_descriptors_on_spawn(&mut command);
            command.spawn().unwrap()
        };
        assert!(!child_inherits_writer(fallback));
    }

    #[test]
    fn zombies_run_while_other_threads_remain() {
        // Field 22 of a stat line, the start time, is 22 here; the command name holds a space
        // and a parenthesis.
        let stat = |state: &str| {
            let fields: Vec<String> = (4..=22).map(|field| field.to_string()).collect();
            format!("7 (open vmm)) {state} {}\n", fields.join(" "))
        };
        let unasked = || -> io::Result<bool> { panic!("only zombies count their threads") };
        let start_time = |state: &str, remain: io::Result<bool>| {
            platform::start_time_from_stat(7, &stat(state), || remain)
        };
        for state in ["R", "S", "D", "T"] {
            assert_eq!(
                platform::start_time_from_stat(7, &stat(state), unasked).unwrap(),
                Some(22)
            );
        }
        assert_eq!(
            platform::start_time_from_stat(7, &stat("X"), unasked).unwrap(),
            None
        );
        assert_eq!(start_time("Z", Ok(true)).unwrap(), Some(22));
        assert_eq!(start_time("Z", Ok(false)).unwrap(), None);
        assert!(start_time("Z", Err(io::Error::other("unreadable"))).is_err());
        assert!(platform::start_time_from_stat(7, "7 (vmm) S 1 2", unasked).is_err());
        assert!(platform::start_time_from_stat(7, "garbage", unasked).is_err());
    }

    /// Kills and reaps a child process when dropped, even when a test fails.
    struct Reaped(Child);

    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn a_process_runs_until_its_last_thread_exits() {
        // The main thread leaves through pthread_exit, which turns it into a zombie, while
        // another thread sleeps. Python is the only portable way to arrange that here.
        let spawned = Command::new("python3")
            .args([
                "-c",
                "import ctypes, threading, time\n\
                 threading.Thread(target=time.sleep, args=(60,)).start()\n\
                 ctypes.CDLL(None).pthread_exit(None)\n",
            ])
            .stdin(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(child) => Reaped(child),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                eprintln!("skipping: python3 is not installed");
                return;
            }
            Err(error) => panic!("cannot start python3: {error}"),
        };
        let pid = child.0.id();
        let start_time = platform::process_start_time(pid).unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            if stat[stat.rfind(')').unwrap() + 1..]
                .trim_start()
                .starts_with('Z')
            {
                break;
            }
            assert!(Instant::now() < deadline, "the main thread did not exit");
            thread::sleep(POLL_INTERVAL);
        }
        assert_eq!(platform::process_start_time(pid).unwrap(), Some(start_time));
        assert!(!wait_for_exit(
            pid,
            start_time,
            Instant::now() + Duration::from_millis(100)
        ));
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(wait_for_exit(pid, start_time, Instant::now()));
    }
}
