//! The announcer of a clone's listeners, which the clone's application starts, as the
//! [module documentation](super) describes.

use std::ffi::{CStr, CString};
use std::fmt;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::BorrowedFd;
use std::process::ExitStatus;

use super::bind_privileged;
use crate::guest::process::Child;
use crate::guest::spawn::{Command, LaunchFailure};
use crate::guest::sys::{self, Errno, ProcessId};

/// The path of the announcer's program in NVX's guests, which NVX's init runs at a cold boot, and
/// [`Announcer::start`] in a clone.
pub const PROGRAM: &CStr = c"/sbin/nvx-ingress";

/// The announcer of the guest's listeners, `nvx-ingress inherit GATEWAY_IPV4`, which
/// [`start`](Self::start) started as a child of the caller, whose status it collects for the
/// caller.
///
/// The announcer runs until it fails, or until [`stop`](Self::stop) ends it: its
/// [pidfd](Self::pidfd) polls readable once it ended, and [`try_wait`](Self::try_wait) then
/// collects its status. Dropping it ends it as `stop` does, ignoring failures.
#[derive(Debug)]
pub struct Announcer {
    /// The announcer's process, which a pidfd names.
    child: Child,
}

impl Announcer {
    /// Starts the announcer of the guest's listeners, which announces them to `gateway`, the
    /// IPv4 gateway of the guest's NIC, as [`PROGRAM`] does at a cold boot:
    ///
    /// 1. it creates an IPv4 UDP socket, and binds it to the highest free privileged source
    ///    port, from 1023 down to 512, as the program binds its own at a cold boot, which needs
    ///    the guest's root user, or `CAP_NET_BIND_SERVICE`: the gateway takes announcements only
    ///    from such a port, which no workload without that privilege can bind;
    /// 2. it spawns [`PROGRAM`] as `nvx-ingress inherit GATEWAY_IPV4`, with the socket as its
    ///    standard input, the caller's standard output and error, every signal's default
    ///    disposition, and none blocked, in a session of its own, without a controlling terminal,
    ///    and waits until the program runs, as its launch report tells, but no longer: nothing
    ///    waits for the program's first scan, nor for any announcement.
    ///
    /// Fails with a [`StartError`] if the socket cannot be created or bound, or the program does
    /// not start, once it collected the status of a child that did not run the program.
    pub fn start(gateway: Ipv4Addr) -> Result<Self, StartError> {
        let socket = sys::UdpSocket::ipv4().map_err(|errno| StartError::Socket(errno.into()))?;
        match bind_privileged(|port| socket.bind_any(port)) {
            Ok(Some(_)) => Self::spawn(PROGRAM, gateway, socket),
            Ok(None) => Err(StartError::NoPrivilegedPort),
            Err(errno) => Err(StartError::Bind(errno.into())),
        }
    }

    /// Spawns `program` as `nvx-ingress inherit GATEWAY_IPV4`, with `socket` as its standard
    /// input, as [`start`](Self::start) does once it bound the socket.
    fn spawn(
        program: &CStr,
        gateway: Ipv4Addr,
        socket: sys::UdpSocket,
    ) -> Result<Self, StartError> {
        let arguments = vec![
            c"nvx-ingress".to_owned(),
            c"inherit".to_owned(),
            CString::new(gateway.to_string()).expect("an address has no NUL"),
        ];
        let spawned = Command::new(program.to_owned(), arguments)
            .stdin(socket)
            .new_session()
            .reset_signals()
            .spawn()
            .map_err(StartError::Spawn)?;
        let id = spawned.id();
        // Nothing but this start collects the status of the child, which keeps its process ID
        // until then.
        let launched = spawned.into_report().wait();
        let started = match launched {
            Ok(None) => Child::with_pidfd(id).map_err(StartError::Spawn),
            Ok(Some(failure)) => Err(StartError::Launch(failure)),
            Err(error) => Err(StartError::Spawn(error)),
        };
        match started {
            Ok(child) => Ok(Self { child }),
            Err(error) => {
                if let Some(process) = ProcessId::from_positive(i64::from(id)) {
                    // A child whose launch failed has exited already.
                    let _ = sys::kill_process(process);
                }
                if let Ok(mut child) = Child::new(id) {
                    let _ = child.wait();
                }
                Err(error)
            }
        }
    }

    /// Returns the announcer's process ID.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Lends the announcer's pidfd, which polls readable once the announcer ended.
    pub fn pidfd(&self) -> BorrowedFd<'_> {
        self.child
            .pidfd()
            .expect("an announcer is named by its pidfd")
    }

    /// Collects the announcer's status if it ended, without blocking, and returns it, or `None`
    /// while it runs. Once it has a status, this returns it again.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Ends the announcer with `SIGKILL`, unless it ended already, and returns its status, which
    /// it collects.
    pub fn stop(mut self) -> io::Result<ExitStatus> {
        self.end()
    }

    /// Ends the announcer, as [`stop`](Self::stop) does.
    fn end(&mut self) -> io::Result<ExitStatus> {
        if self.child.status().is_none() {
            match sys::kill_through_pidfd(self.pidfd(), false) {
                // An announcer that ended, but whose status nothing collected yet, takes the
                // signal as a zombie does.
                Ok(()) | Err(Errno::ESRCH) => {}
                Err(errno) => return Err(errno.into()),
            }
        }
        self.child.wait()
    }
}

impl Drop for Announcer {
    fn drop(&mut self) {
        let _ = self.end();
    }
}

/// Why [`Announcer::start`] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum StartError {
    /// The announcer's UDP socket could not be created, as this error tells.
    Socket(io::Error),
    /// The announcer's socket could not be bound to a privileged source port, for another reason
    /// than the port's use, as this error tells: as without the privilege to bind one.
    Bind(io::Error),
    /// Every privileged source port, from 1023 down to 512, is in use.
    NoPrivilegedPort,
    /// The announcer could not be spawned, or its launch could not be followed, as this error
    /// tells.
    Spawn(io::Error),
    /// The announcer's launch failed at a stage, as when its program does not exist, which its
    /// exec stage then tells.
    Launch(LaunchFailure),
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let program = PROGRAM.to_string_lossy();
        match self {
            Self::Socket(error) => {
                write!(
                    formatter,
                    "the announcer's socket cannot be created: {error}"
                )
            }
            Self::Bind(error) => write!(
                formatter,
                "the announcer's socket cannot be bound to a privileged source port: {error}"
            ),
            Self::NoPrivilegedPort => formatter.write_str(
                "no privileged source port, from 1023 down to 512, is free for the announcer's \
                 socket",
            ),
            Self::Spawn(error) => write!(formatter, "{program} cannot be started: {error}"),
            Self::Launch(failure) => write!(formatter, "{program} did not start: {failure}"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Socket(error) | Self::Bind(error) | Self::Spawn(error) => Some(error),
            Self::Launch(failure) => Some(failure),
            Self::NoPrivilegedPort => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::guest::spawn::LaunchStage;

    /// How long a test waits for an announcer to do what it expects.
    const PATIENCE: Duration = Duration::from_secs(10);

    /// The gateway of the tests' announcers.
    const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);

    /// Writes an executable shell script at `path` whose body is `script`, which stands for the
    /// announcer's program, and returns its path.
    fn program(directory: &Path, name: &str, script: &str) -> CString {
        let path = directory.join(name);
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        CString::new(path.as_os_str().as_bytes()).unwrap()
    }

    /// Returns a socket that stands for the announcer's, bound to a port that needs no privilege.
    fn socket() -> sys::UdpSocket {
        let socket = sys::UdpSocket::ipv4().unwrap();
        socket.bind_any(0).unwrap();
        socket
    }

    /// Returns whether `announcer`'s pidfd polls readable within `milliseconds`.
    fn ended(announcer: &Announcer, milliseconds: u16) -> bool {
        sys::readable_or_hung_up(&[announcer.pidfd()], milliseconds).unwrap() == [true]
    }

    #[test]
    fn an_announcer_runs_its_program_with_its_socket_until_it_is_stopped() {
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("report");
        let script = format!(
            "{{ printf '%s\\n' \"$0\" \"$@\"; stat -L -c %F /dev/stdin; }} >{0}.tmp\n\
             mv {0}.tmp {0}\nexec sleep 60",
            report.display()
        );
        let program = program(directory.path(), "announcer", &script);
        let announcer = Announcer::spawn(&program, GATEWAY, socket()).unwrap();
        let deadline = Instant::now() + PATIENCE;
        while !report.exists() {
            assert!(Instant::now() < deadline, "the announcer reported nothing");
            std::thread::sleep(Duration::from_millis(10));
        }
        // The program takes the gateway after `inherit`, and the socket as its standard input.
        let reported = fs::read_to_string(&report).unwrap();
        let lines: Vec<_> = reported.lines().collect();
        assert_eq!(lines[1..], ["inherit", "10.0.0.1", "socket"], "{reported}");
        assert!(!ended(&announcer, 0));
        let status = announcer.stop().unwrap();
        assert_eq!(status.signal(), Some(nix::libc::SIGKILL));
    }

    #[test]
    fn an_announcer_that_ends_polls_readable_and_keeps_its_status() {
        let directory = tempfile::tempdir().unwrap();
        let program = program(directory.path(), "announcer", "exit 3");
        let mut announcer = Announcer::spawn(&program, GATEWAY, socket()).unwrap();
        assert!(ended(&announcer, 10_000));
        let status = announcer.try_wait().unwrap().unwrap();
        assert_eq!(status.code(), Some(3));
        assert_eq!(announcer.try_wait().unwrap(), Some(status));
        // Its stop signals nothing, and returns the status again.
        assert_eq!(announcer.stop().unwrap(), status);
    }

    #[test]
    fn a_dropped_announcer_ends() {
        let directory = tempfile::tempdir().unwrap();
        let program = program(directory.path(), "announcer", "exec sleep 60");
        let announcer = Announcer::spawn(&program, GATEWAY, socket()).unwrap();
        let process = ProcessId::from_positive(i64::from(announcer.id())).unwrap();
        drop(announcer);
        // The drop ended it and collected its status, so its process ID names no process.
        assert!(!sys::process_exists(process));
    }

    #[test]
    fn an_announcer_whose_program_does_not_run_does_not_start() {
        let directory = tempfile::tempdir().unwrap();
        let missing =
            CString::new(directory.path().join("missing").as_os_str().as_bytes()).unwrap();
        let error = Announcer::spawn(&missing, GATEWAY, socket()).unwrap_err();
        let StartError::Launch(failure) = &error else {
            panic!("{error}");
        };
        assert_eq!(failure.stage(), LaunchStage::Exec);
        assert_eq!(failure.raw_os_error(), nix::libc::ENOENT);
        assert!(
            error
                .to_string()
                .starts_with("/sbin/nvx-ingress did not start: the exec launch stage failed: "),
            "{error}"
        );
    }

    #[test]
    fn start_errors_name_their_cause() {
        let denied = io::Error::from(Errno::EACCES);
        for (error, expected) in [
            (
                StartError::Socket(io::Error::from(Errno::EMFILE)),
                "the announcer's socket cannot be created: ",
            ),
            (
                StartError::Bind(denied),
                "the announcer's socket cannot be bound to a privileged source port: ",
            ),
            (
                StartError::NoPrivilegedPort,
                "no privileged source port, from 1023 down to 512, is free for the announcer's \
                 socket",
            ),
            (
                StartError::Spawn(io::Error::from(Errno::EAGAIN)),
                "/sbin/nvx-ingress cannot be started: ",
            ),
        ] {
            assert!(error.to_string().starts_with(expected), "{error}");
        }
    }
}
