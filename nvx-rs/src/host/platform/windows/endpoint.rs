//! The named pipes of OpenVMM's endpoints: its control endpoint and the transport over it, and
//! the listener of a clone's restore-ready event.

use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use std::ptr;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
    GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ALL_ACCESS, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, WaitForSingleObject,
};

use super::private::{CurrentUser, with_owner_only_attributes};
use super::{last_error, owned, raw, wide};
use crate::host::platform::{Transport, time_left};

/// Returns a fresh named-pipe endpoint for OpenVMM to listen on.
pub(crate) fn control_endpoint(_socket_path: &Path) -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("//./pipe/openvmm-microvm-{suffix}"))
}

/// The bytes that the inbound buffer of a restore-ready pipe holds, more than OpenVMM's event.
const READY_BUFFER_LEN: u32 = 4096;

/// The listener at which the launch of a clone awaits the restore-ready event of its VMM: the
/// one instance of a named pipe, inbound and overlapped, which only the current user may open,
/// from this host alone, and which takes the connection of the VMM before the host accepts it.
#[derive(Debug)]
pub(crate) struct ReadyListener {
    /// The pipe's instance, which the VMM finds as long as it is open.
    pipe: OwnedHandle,
    /// The manual-reset event of the overlapped operations on the pipe.
    event: OwnedHandle,
    /// Whether the host accepted the VMM's connection.
    connected: bool,
}

/// Listens at a fresh named pipe, whose name it draws at random, as the restore-ready endpoint of
/// a clone's launch, and returns the endpoint, as OpenVMM's `--restore-ready-path` takes it, and
/// its listener. The first instance of the pipe is the listener's, so no other process holds the
/// name, and the pipe's protected access control list allows the current user alone.
pub(crate) fn listen_ready(_socket_path: &Path) -> io::Result<(String, ReadyListener)> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let endpoint = format!("//./pipe/nvx-restore-ready-{suffix}");
    let name = wide(endpoint.replace('/', "\\"));
    let pipe =
        with_owner_only_attributes(&CurrentUser::get()?, FILE_ALL_ACCESS, 0, |attributes| {
            // SAFETY: the name is NUL-terminated, and the attributes hold a valid descriptor.
            let handle = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    0,
                    READY_BUFFER_LEN,
                    0,
                    attributes,
                )
            };
            owned(handle).ok_or_else(io::Error::last_os_error)
        })??;
    // SAFETY: creates an unnamed manual-reset event with default security.
    let event = owned(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })
        .ok_or_else(io::Error::last_os_error)?;
    let listener = ReadyListener {
        pipe,
        event,
        connected: false,
    };
    Ok((endpoint, listener))
}

impl ReadyListener {
    /// Waits at most `timeout` for what the VMM, the process `pid`, sends to the endpoint, and
    /// appends it to `received`, which then holds at most `limit` bytes. Returns whether more may
    /// arrive: `false` once the VMM closed its end of the pipe.
    ///
    /// The pipe's client must be the VMM: another process fails with
    /// [`io::ErrorKind::PermissionDenied`].
    pub(crate) fn receive(
        &mut self,
        pid: u32,
        received: &mut Vec<u8>,
        limit: usize,
        timeout: Duration,
    ) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        let (pipe, event) = (raw(&self.pipe), raw(&self.event));
        if !self.connected {
            // ConnectNamedPipe does not promise to reset the event before it starts, as ReadFile
            // and WriteFile do, so it is reset here.
            // SAFETY: the event belongs to the listener, and no operation that uses it is pending.
            if unsafe { ResetEvent(event) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // A wait that times out cancels the connection's operation, and the next wait issues
            // it again, which a client that connected meanwhile completes at once.
            // SAFETY: the operation's `OVERLAPPED` outlives it, as `overlapped` guarantees.
            let connection = overlapped(pipe, event, Some(timeout), |pipe, overlapped| unsafe {
                ConnectNamedPipe(pipe, overlapped)
            });
            match connection {
                Ok(_) => {}
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) => {}
                Err(error) if error.kind() == io::ErrorKind::TimedOut => return Ok(true),
                Err(error) => return Err(error),
            }
            let mut client = 0u32;
            // SAFETY: the handle is a pipe's server end, and the output pointer is valid.
            if unsafe { GetNamedPipeClientProcessId(pipe, &mut client) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if client != pid {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "process {client} connected to the restore-ready endpoint, not the VMM, \
                         process {pid}"
                    ),
                ));
            }
            self.connected = true;
        }
        let mut buffer = [0u8; 64];
        let room = limit.saturating_sub(received.len()).min(buffer.len());
        if room == 0 {
            return Ok(true);
        }
        let (data, length) = (buffer.as_mut_ptr(), room as u32);
        let left = deadline.saturating_duration_since(Instant::now());
        // SAFETY: `data` is valid for `length` bytes until the operation completes, which
        // `overlapped` guarantees before it returns.
        let read = overlapped(pipe, event, Some(left), |pipe, overlapped| unsafe {
            ReadFile(pipe, data, length, ptr::null_mut(), overlapped)
        });
        match read {
            Ok(0) => Ok(false),
            Ok(count) => {
                received.extend_from_slice(&buffer[..count]);
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::TimedOut => Ok(true),
            Err(error) => Err(error),
        }
    }
}

/// Connects to a named pipe served by the process `expected_pid`.
pub(crate) fn connect_endpoint(
    endpoint: &str,
    expected_pid: u32,
    timeout: Duration,
) -> io::Result<Box<dyn Transport>> {
    let name = wide(endpoint.replace('/', "\\"));
    // SAFETY: the name is NUL-terminated; SQOS identification prevents the server from
    // impersonating this client.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            ptr::null_mut(),
        )
    };
    let Some(pipe) = owned(handle) else {
        let error = last_error();
        if error == ERROR_PIPE_BUSY {
            let wait = u32::try_from(timeout.as_millis())
                .unwrap_or(u32::MAX)
                .max(1);
            // SAFETY: the name is NUL-terminated.
            unsafe { WaitNamedPipeW(name.as_ptr(), wait) };
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "control endpoint is busy",
            ));
        }
        return Err(io::Error::from_raw_os_error(error as i32));
    };
    let mut server = 0u32;
    // SAFETY: the handle is a connected pipe client and the output pointer is valid.
    if unsafe { GetNamedPipeServerProcessId(raw(&pipe), &mut server) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if server != expected_pid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "control endpoint is served by process {server}, not OpenVMM process {expected_pid}"
            ),
        ));
    }
    // SAFETY: creates an unnamed manual-reset event with default security.
    let event = owned(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })
        .ok_or_else(io::Error::last_os_error)?;
    Ok(Box::new(PipeTransport { pipe, event }))
}

/// Returns the process that serves a named-pipe endpoint, or `None` if the pipe does not exist.
///
/// OpenVMM keeps one pipe instance for its whole lifetime, so a missing pipe means no OpenVMM
/// serves the endpoint. The instance is busy while a client is attached and briefly between
/// clients, which is waited out for up to five seconds.
pub(crate) fn endpoint_server_pid(endpoint: &str) -> io::Result<Option<u32>> {
    let name = wide(endpoint.replace('/', "\\"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // SAFETY: the name is NUL-terminated; SQOS identification prevents impersonation.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        };
        if let Some(pipe) = owned(handle) {
            let mut server = 0u32;
            // SAFETY: the handle is a connected pipe client and the output pointer is valid.
            if unsafe { GetNamedPipeServerProcessId(raw(&pipe), &mut server) } == 0 {
                return Err(io::Error::last_os_error());
            }
            return Ok(Some(server));
        }
        match last_error() {
            ERROR_FILE_NOT_FOUND => return Ok(None),
            ERROR_PIPE_BUSY if Instant::now() < deadline => {
                // SAFETY: the name is NUL-terminated.
                unsafe { WaitNamedPipeW(name.as_ptr(), 100) };
            }
            error => return Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
}

/// Overlapped byte transport over OpenVMM's named-pipe control endpoint.
struct PipeTransport {
    /// Connected named-pipe client handle.
    pipe: OwnedHandle,
    /// Manual event reused to wait for each overlapped operation.
    event: OwnedHandle,
}

impl PipeTransport {
    /// Runs one overlapped operation on the pipe to completion or timeout, as [`overlapped`]
    /// runs it.
    fn overlapped(
        &mut self,
        timeout: Option<Duration>,
        start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32,
    ) -> io::Result<usize> {
        overlapped(raw(&self.pipe), raw(&self.event), timeout, start)
    }
}

/// Runs one overlapped operation on `pipe` to completion or timeout, waiting on `event`, a
/// manual-reset event that no other operation uses meanwhile, and which the operation resets to
/// nonsignaled before it starts, as ReadFile and WriteFile do.
///
/// `start` issues the operation. The function always waits for the operation to finish,
/// cancelling it on timeout, so the `OVERLAPPED` and the caller's buffer outlive it.
fn overlapped(
    pipe: HANDLE,
    event: HANDLE,
    timeout: Option<Duration>,
    start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32,
) -> io::Result<usize> {
    // SAFETY: OVERLAPPED is plain data for which all-zero bytes are a valid initial state.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event;
    if start(pipe, &raw mut overlapped) == 0 {
        let error = last_error();
        if error != ERROR_IO_PENDING {
            return completion_error(error, 0, false);
        }
    }
    let wait = timeout.map_or(INFINITE, |timeout| {
        u32::try_from(timeout.as_millis())
            .unwrap_or(INFINITE - 1)
            .clamp(1, INFINITE - 1)
    });
    // SAFETY: the event belongs to the caller.
    let signaled = unsafe { WaitForSingleObject(overlapped.hEvent, wait) } == WAIT_OBJECT_0;
    if !signaled {
        // SAFETY: cancels only the operation that uses this OVERLAPPED.
        unsafe { CancelIoEx(pipe, &overlapped) };
    }
    let mut transferred = 0u32;
    // SAFETY: waits for the operation to finish, so nothing references `overlapped` after.
    if unsafe { GetOverlappedResult(pipe, &overlapped, &mut transferred, 1) } != 0 {
        return Ok(transferred as usize);
    }
    completion_error(last_error(), transferred as usize, !signaled)
}

/// Maps a completed pipe operation's Win32 status to Rust I/O semantics.
fn completion_error(error: u32, transferred: usize, timed_out: bool) -> io::Result<usize> {
    match error {
        ERROR_OPERATION_ABORTED if timed_out => Err(io::ErrorKind::TimedOut.into()),
        ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA => Ok(0),
        ERROR_MORE_DATA => Ok(transferred),
        _ => Err(io::Error::from_raw_os_error(error as i32)),
    }
}

impl Transport for PipeTransport {
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        let length = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let data = buffer.as_mut_ptr();
        self.overlapped(timeout, |pipe, overlapped| {
            // SAFETY: `data` is valid for `length` bytes until the operation completes, which
            // `overlapped` guarantees before returning.
            unsafe { ReadFile(pipe, data, length, ptr::null_mut(), overlapped) }
        })
    }

    fn write_all(&mut self, mut data: &[u8], timeout: Option<Duration>) -> io::Result<()> {
        // One deadline bounds the whole write, however the endpoint splits it.
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        while !data.is_empty() {
            let left = time_left(deadline)?;
            let length = u32::try_from(data.len()).unwrap_or(u32::MAX);
            let pointer = data.as_ptr();
            let written = self.overlapped(left, |pipe, overlapped| {
                // SAFETY: as for reads, the buffer outlives the operation.
                unsafe { WriteFile(pipe, pointer, length, ptr::null_mut(), overlapped) }
            })?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            data = &data[written..];
        }
        Ok(())
    }
}
