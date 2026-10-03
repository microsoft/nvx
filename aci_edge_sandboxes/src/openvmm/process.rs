//! Launch and supervision of OpenVMM processes.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
#[cfg(not(windows))]
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::config::OpenVmmConfig;
use super::platform;
use super::protocol::CAPABILITY_LEN;

const KILL_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// An OpenVMM process this caller launched and has not released yet.
pub(crate) struct Launched {
    #[cfg(not(windows))]
    child: Child,
    #[cfg(windows)]
    process: platform::LaunchedProcess,
}

impl Launched {
    pub(crate) fn id(&self) -> u32 {
        #[cfg(not(windows))]
        return self.child.id();
        #[cfg(windows)]
        return self.process.id();
    }
}

/// Starts OpenVMM detached from the caller.
///
/// OpenVMM reads its capability from standard input as soon as it starts, so the capability is
/// written into a pipe whose write end is closed before the spawn. The capability never appears
/// in arguments, the environment, or the log. OpenVMM output goes to `log`, and OpenVMM inherits
/// no other handle of the caller, so a caller whose own output is a pipe sees end-of-file when it
/// exits rather than when the VM does.
pub(crate) fn spawn(
    config: &OpenVmmConfig,
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
            &config.openvmm,
            arguments,
            working_dir,
            [stdin.into(), log.into(), stderr.into()],
            config.breakaway_from_job,
        )
        .map(|process| Launched { process })
    }
    #[cfg(not(windows))]
    {
        // Rust opens its own descriptors close-on-exec, and `platform::detach` marks any other
        // descriptor of the caller, so OpenVMM receives only these.
        let mut command = Command::new(&config.openvmm);
        command
            .args(arguments)
            .current_dir(working_dir)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        platform::detach(&mut command, config.breakaway_from_job);
        command.spawn().map(|child| Launched { child })
    }
}

/// Releases a launched process without waiting for it.
///
/// On Unix a background thread reaps the child once it exits so it does not linger as a zombie
/// while this process lives. If that thread cannot start, the child becomes a zombie after it
/// exits; liveness checks treat zombies as exited, so only the process table entry leaks. The
/// OpenVMM process keeps running either way.
pub(crate) fn detach_child(launched: Launched) {
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
pub(crate) fn kill_child(launched: Launched) -> bool {
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
pub(crate) fn wait_for_exit(pid: u32, start_time: u64, deadline: Instant) -> bool {
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
pub(crate) fn kill(pid: u32, start_time: u64) -> io::Result<bool> {
    platform::kill_process(pid, start_time)?;
    Ok(wait_for_exit(
        pid,
        start_time,
        Instant::now() + KILL_TIMEOUT,
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::sync::mpsc;

    use super::*;
    use crate::openvmm::Hypervisor;

    /// Spawns a long-running child while this process holds an inheritable pipe writer, as a
    /// descriptor that the caller's parent or native code left without close-on-exec would be.
    /// Returns whether the child keeps a copy, which shows as the reader not reaching end-of-file
    /// once this process closes its own.
    fn child_keeps_inherited_writer(spawn: impl FnOnce() -> Child) -> bool {
        let (mut reader, writer) = io::pipe().unwrap();
        // SAFETY: clearing FD_CLOEXEC only changes the flags of a descriptor this test owns.
        assert_eq!(
            unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFD, 0) },
            0
        );
        let mut child = spawn();
        drop(writer);
        let (finished, received) = mpsc::channel();
        let drain = thread::spawn(move || {
            let _ = reader.read_to_end(&mut Vec::new());
            let _ = finished.send(());
        });
        let reached_end = received.recv_timeout(Duration::from_secs(2)).is_ok();
        child.kill().unwrap();
        child.wait().unwrap();
        drain.join().unwrap();
        !reached_end
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
        assert!(child_keeps_inherited_writer(plain));

        let directory = tempfile::tempdir().unwrap();
        let config = OpenVmmConfig::new(
            "sleep",
            "vmlinux",
            "initramfs.cpio.gz",
            Hypervisor::Kvm,
            directory.path(),
        );
        let openvmm = || {
            let log = File::create(directory.path().join("openvmm.log")).unwrap();
            let arguments = [OsString::from("60")];
            let capability = [1; CAPABILITY_LEN];
            let launched = spawn(&config, &arguments, &capability, log, directory.path());
            launched.unwrap().child
        };
        assert!(!child_keeps_inherited_writer(openvmm));

        // Kernels without CLOSE_RANGE_CLOEXEC take the /proc/self/fd fallback.
        let fallback = || {
            let mut command = sleeper();
            // SAFETY: the fallback allocates nothing and makes only async-signal-safe calls.
            unsafe { command.pre_exec(platform::mark_listed_descriptors_close_on_exec) };
            command.spawn().unwrap()
        };
        assert!(!child_keeps_inherited_writer(fallback));
    }
}
