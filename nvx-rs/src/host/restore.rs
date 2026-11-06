//! A restore-ready endpoint whose client must be the held VM process.
//!
//! A launch binds before spawning the VM and receives through its verified [`Process`].
//! Parse the completed bytes with
//! [`parse_ready_event`](crate::protocol::restore_ready::parse_ready_event).
//! External I/O users take the native listener with `ReadyListener::into_std` on Linux or
//! Windows and check its client's process identity before reading the event.

use std::io;
#[cfg(not(windows))]
use std::path::Path;
use std::time::Duration;

use super::platform;
use super::process::{Process, ProcessRef};
use super::vm::VmDir;
use crate::{Error, Result};

/// The listener of one restore, bound before its VM starts.
///
/// The first receive fixes the expected process identity. Every receive borrows its held
/// [`Process`], never merely a PID, and refuses an exited process. Dropping the listener closes
/// its handles; on Linux, the socket's directory entry remains for the caller to remove.
#[derive(Debug)]
pub struct ReadyListener {
    /// The endpoint passed to OpenVMM's `--restore-ready-path`.
    endpoint: String,
    /// The existing platform listener and its accepted connection, if any.
    listener: platform::ReadyListener,
    /// The identity supplied by the first receive, which later receives may not change.
    process: Option<ProcessRef>,
}

impl ReadyListener {
    /// Binds `restore-ready.sock` in the private VM directory on Linux, or a fresh, first-instance
    /// owner-only pipe on Windows. The Linux socket has mode 0600, whatever the umask.
    ///
    /// Replaces a previous Linux endpoint. Fails with [`ErrorKind::Io`](crate::ErrorKind::Io)
    /// if binding fails or the directory is no longer private, and with
    /// [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) on unsupported hosts.
    #[cfg_attr(
        not(all(test, any(target_os = "linux", windows))),
        expect(dead_code, reason = "SL2.16 binds a restore's endpoint (#1103)")
    )]
    pub(crate) fn bind(directory: &VmDir) -> Result<Self> {
        let directory = VmDir::open(directory.path())?;
        let endpoint = directory.restore_ready_endpoint()?;
        #[cfg(not(windows))]
        let (_, listener) = platform::listen_ready(Path::new(&endpoint)).map_err(endpoint_error)?;
        #[cfg(windows)]
        let listener = platform::ReadyListener::bind(&endpoint).map_err(endpoint_error)?;
        Ok(Self {
            endpoint,
            listener,
            process: None,
        })
    }

    /// Returns the endpoint for OpenVMM's `--restore-ready-path`.
    #[cfg_attr(
        not(all(test, any(target_os = "linux", windows))),
        expect(dead_code, reason = "SL2.16 renders the restore endpoint (#1103)")
    )]
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Appends at most `limit` total bytes to `received`, waiting at most `timeout`.
    ///
    /// Returns `false` at end of stream, or `true` when more may arrive, including after a
    /// timeout without data. The first client must match `process`; another client, or a later
    /// receive with a different process identity, fails with
    /// [`ErrorKind::Protocol`](crate::ErrorKind::Protocol). An exited held process fails with
    /// [`ErrorKind::Exited`](crate::ErrorKind::Exited), even if its PID has been reused.
    /// Linux authenticates the original socket peer through `SO_PEERPIDFD` (Linux 6.5 or a
    /// backport), failing with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable)
    /// if the host cannot provide it, rather than falling back to a numeric PID.
    #[cfg_attr(
        not(all(test, any(target_os = "linux", windows))),
        expect(
            dead_code,
            reason = "SL4.3's blocking driver receives the restore-ready event"
        )
    )]
    pub(crate) fn receive(
        &mut self,
        process: &Process,
        received: &mut Vec<u8>,
        limit: usize,
        timeout: Duration,
    ) -> Result<bool> {
        if self
            .process
            .as_ref()
            .is_some_and(|expected| expected != process.reference())
        {
            return Err(Error::protocol(
                "the restore-ready process identity changed",
            ));
        }
        check_running(process)?;
        self.process = Some(process.reference().clone());
        #[cfg(target_os = "linux")]
        let open =
            self.listener
                .receive_verified(process.reference().pid, received, limit, timeout);
        #[cfg(not(target_os = "linux"))]
        let open = self
            .listener
            .receive(process.reference().pid, received, limit, timeout);
        let open = open.map_err(listener_error)?;
        // A matching numeric PID is not enough if the held process exited during acceptance.
        check_running(process)?;
        Ok(open)
    }

    /// Transfers the same nonblocking Unix listener, without reopening or duplicating it.
    ///
    /// Call before a receive accepts a client; otherwise fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported). The caller must verify each
    /// accepted stream's original peer through `SO_PEERPIDFD`, refuse an exited peer, and match
    /// its credentials against the held VM process before reading its event. Opening a pidfd
    /// from the credentials' numeric PID does not authenticate an already-reaped peer.
    /// After matching the peer, check [`Process::has_exited`] and refuse an exited VM.
    #[cfg(target_os = "linux")]
    pub fn into_std(self) -> Result<std::os::unix::net::UnixListener> {
        self.listener.into_std().map_err(transfer_error)
    }

    /// Transfers the same overlapped first pipe instance, without reopening or duplicating it.
    ///
    /// Call before a receive accepts a client; otherwise fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported). The caller completes
    /// `ConnectNamedPipe`, checks its client process ID against the held VM process before
    /// reading its event, and owns completion or cancellation of its overlapped operations.
    /// After matching the PID, check [`Process::has_exited`] and refuse an exited process.
    #[cfg(windows)]
    pub fn into_std(self) -> Result<std::os::windows::io::OwnedHandle> {
        self.listener.into_std().map_err(transfer_error)
    }
}

/// Refuses a stale process handle, so a reused PID cannot pass a peer check.
fn check_running(process: &Process) -> Result<()> {
    if process
        .has_exited()
        .map_err(|error| Error::io("cannot check the restore-ready process").with_source(error))?
    {
        return Err(Error::exited("the restore-ready VM process exited"));
    }
    Ok(())
}

/// Keeps the existing clone endpoint's peer mismatch distinct from host I/O failures.
fn listener_error(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::PermissionDenied {
        Error::protocol("another process connected to the restore-ready endpoint")
            .with_source(error)
    } else {
        endpoint_error(error)
    }
}

/// Reports endpoint I/O separately from an unsupported host.
fn endpoint_error(error: io::Error) -> Error {
    let message = "cannot use the restore-ready endpoint";
    if error.kind() == io::ErrorKind::Unsupported {
        Error::unavailable(message).with_source(error)
    } else {
        Error::io(message).with_source(error)
    }
}

/// Reports an accepted listener that can no longer be handed over as a fresh native listener.
#[cfg(any(target_os = "linux", windows))]
fn transfer_error(error: io::Error) -> Error {
    Error::unsupported("cannot transfer a restore-ready listener after acceptance")
        .with_source(error)
}

#[cfg(test)]
mod tests;
