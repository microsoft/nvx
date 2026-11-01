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
mod tests;
