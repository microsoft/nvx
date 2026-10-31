//! Processes that this user may query, wait for, and terminate, held through handles that keep
//! their IDs from naming any other process, the bytes that processes read, the identities of this
//! boot and host, and the host's monotonic time.

use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, OwnedHandle};
use std::thread;
use std::time::{Duration, Instant};
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{
    CompareObjectHandles, DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle,
    ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_SUCCESS, FILETIME, HANDLE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessIoCounters, GetProcessTimes, INFINITE, IO_COUNTERS,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
    WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::QueryInterruptTimePrecise;

use super::{last_error, open_handle, owned, raw, wide};
use crate::host::platform::{parse_host_id, unless_exited};

/// Returns the process creation `FILETIME`, used with the PID as a stable identity.
pub(super) fn creation_time(process: &OwnedHandle) -> io::Result<u64> {
    let mut created = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exited = created;
    let mut kernel = created;
    let mut user = created;
    // SAFETY: the handle has query access and every output pointer is valid.
    let succeeded = unsafe {
        GetProcessTimes(
            raw(process),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } != 0;
    if succeeded {
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Polls `process` without waiting and reports whether it has exited.
fn has_exited(process: &OwnedHandle) -> io::Result<bool> {
    // SAFETY: the handle has synchronize access; a zero timeout only polls.
    match unsafe { WaitForSingleObject(raw(process), 0) } {
        WAIT_TIMEOUT => Ok(false),
        WAIT_OBJECT_0 => Ok(true),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Returns how many bytes the process of `process`, a handle with query access, has read through
/// read calls: the `ReadTransferCount` of `GetProcessIoCounters`, which counts every `ReadFile`,
/// whatever it reads, and whether the cache served it or not.
fn read_transfer(process: HANDLE) -> io::Result<u64> {
    let mut counters = IO_COUNTERS::default();
    // SAFETY: the handle has query access, and the output pointer is valid.
    if unsafe { GetProcessIoCounters(process, &mut counters) } != 0 {
        Ok(counters.ReadTransferCount)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Returns how many bytes this process has read through read calls, as [`read_transfer`] counts
/// them.
pub(crate) fn own_bytes_read() -> io::Result<u64> {
    // SAFETY: GetCurrentProcess has no preconditions, and returns a pseudo-handle with every
    // access, which needs no closing.
    read_transfer(unsafe { GetCurrentProcess() })
}

/// Returns the outcome of a termination that Windows refused with `error` through a handle with
/// terminate access: success if the process is exiting or has exited, and `error` otherwise.
///
/// Windows refuses to terminate a process whose exit has begun, through a termination or its own
/// exit, with `ERROR_ACCESS_DENIED`, which a handle that has terminate access gets only then. It
/// signals the process only once the process's last thread has exited, so a check of the handle
/// may not show the exit yet.
pub(super) fn refused_termination(error: io::Error) -> io::Result<()> {
    if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        Ok(())
    } else {
        Err(error)
    }
}

/// Returns how a wait with `left` before its deadline goes on: the whole milliseconds, rounded
/// down, for which it waits for the process, never `INFINITE`, and, when there are none, the
/// remainder below a millisecond that it sleeps before it checks the process a last time.
pub(super) fn wait_step(left: Duration) -> (u32, Duration) {
    let milliseconds =
        u32::try_from(left.as_millis()).map_or(INFINITE - 1, |value| value.min(INFINITE - 1));
    let sleep = if milliseconds == 0 {
        left
    } else {
        Duration::ZERO
    };
    (milliseconds, sleep)
}

/// Opens a process, returning `None` if it does not exist or belongs to another user.
///
/// OpenVMM always runs as the calling user, so a process that denies access cannot be the
/// sandbox's VM, even if it reuses the recorded process ID.
fn open_existing_process(pid: u32, access: u32) -> io::Result<Option<OwnedHandle>> {
    if let Some(process) = open_handle(pid, access) {
        return Ok(Some(process));
    }
    match last_error() {
        ERROR_INVALID_PARAMETER | ERROR_ACCESS_DENIED => Ok(None),
        error => Err(io::Error::from_raw_os_error(error as i32)),
    }
}

/// Returns the creation time of a live process, `None` if the process no longer exists or
/// belongs to another user, or an error if its state cannot be determined.
pub fn process_start_time(pid: u32) -> io::Result<Option<u64>> {
    let Some(process) =
        open_existing_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE)?
    else {
        return Ok(None);
    };
    if has_exited(&process)? {
        return Ok(None);
    }
    creation_time(&process).map(Some)
}

/// A process that this user may query and wait for, and terminate when [`terminate`] says so,
/// held through a handle that keeps its ID from naming any other process.
///
/// [`terminate`]: Self::terminate
pub(crate) struct ProcessHandle {
    /// Process handle with query-limited-information and synchronize access, or with every
    /// access, the right to duplicate the process's handles included, for a process that this
    /// one launched, as [`LaunchedProcess::pin`] holds it.
    ///
    /// [`LaunchedProcess::pin`]: crate::host::platform::windows::LaunchedProcess::pin
    pub(super) process: OwnedHandle,
    /// Whether the handle also has terminate access.
    pub(super) terminate: bool,
}

impl ProcessHandle {
    /// Whether the process's exit wakes its waiters, which it always does on Windows.
    pub(crate) fn has_exit_event(&self) -> bool {
        true
    }

    /// Waits until the process exits or `deadline` passes, and returns whether it exited.
    ///
    /// `WaitForSingleObject` waits for whole milliseconds, which the system timer rounds up to
    /// its ticks of up to about 16 ms, so a remainder below a millisecond is slept rather than
    /// rounded up to a millisecond, before a last check, as [`wait_step`] describes. The sleep
    /// takes about the remainder where `thread::sleep` has a high-resolution timer, as on Windows
    /// 10 version 1803 and later, and up to a tick otherwise, and never ends before the deadline.
    pub(crate) fn wait(&self, deadline: Instant) -> io::Result<Option<bool>> {
        loop {
            let (milliseconds, sleep) =
                wait_step(deadline.saturating_duration_since(Instant::now()));
            if !sleep.is_zero() {
                thread::sleep(sleep);
            }
            // SAFETY: the handle has synchronize access.
            match unsafe { WaitForSingleObject(raw(&self.process), milliseconds) } {
                WAIT_OBJECT_0 => return Ok(Some(true)),
                WAIT_TIMEOUT if milliseconds == 0 || Instant::now() >= deadline => {
                    return Ok(Some(false));
                }
                WAIT_TIMEOUT => {}
                _ => return Err(io::Error::last_os_error()),
            }
        }
    }

    /// Terminates the process with status 1, unless it has exited, and fails if this user may
    /// not terminate process `pid`.
    ///
    /// A process that is exiting already needs no ending either. Windows refuses to terminate it,
    /// as [`refused_termination`] describes, before it signals the process's exit.
    pub(crate) fn kill(&self, pid: u32) -> io::Result<()> {
        end_process(
            self.terminate,
            pid,
            || {
                // SAFETY: `end_process` terminates only through a handle with terminate access.
                if unsafe { TerminateProcess(raw(&self.process), 1) } != 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            },
            || has_exited(&self.process),
        )
    }

    /// Returns how many bytes the process has read through read calls, as [`read_transfer`]
    /// counts them: once the process has exited, those that it read until then, which the handle
    /// keeps.
    pub(crate) fn bytes_read(&self, _pid: u32, _start_time: u64) -> io::Result<u64> {
        read_transfer(raw(&self.process))
    }

    /// Closes the handle of process `pid` that has the value of this process's handle `ours`,
    /// which the process inherited from this one: a handle keeps its value in a child that
    /// inherits it, and a child that never uses it, as a VMM never uses the guards of the checked
    /// files that it inherits, keeps it there.
    ///
    /// The process loses no other handle: the handle is closed only once a copy of it is found to
    /// hold the object of `ours`, so a process that did not inherit `ours` keeps every handle that
    /// it has, and the close fails with [`io::ErrorKind::InvalidInput`], or as `DuplicateHandle`
    /// fails for a value that names no handle of the process. A process that has exited has closed
    /// its handles, so the close succeeds. Fails with [`io::ErrorKind::PermissionDenied`] through
    /// a handle that may not duplicate the handles of the process, as one that [`open_process`]
    /// returns: only that of [`LaunchedProcess::pin`] may.
    ///
    /// [`LaunchedProcess::pin`]: crate::host::platform::windows::LaunchedProcess::pin
    pub(crate) fn close_inherited(&self, pid: u32, ours: BorrowedHandle<'_>) -> io::Result<()> {
        let value = ours.as_raw_handle() as HANDLE;
        let exited = || has_exited(&self.process);
        let mut copy = ptr::null_mut();
        // SAFETY: the value is looked up among the handles of the process alone, and the output
        // pointer is valid.
        let duplicated = unsafe {
            DuplicateHandle(
                raw(&self.process),
                value,
                GetCurrentProcess(),
                &mut copy,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } != 0;
        if !duplicated {
            return unless_exited(Err(io::Error::last_os_error()), exited);
        }
        let copy =
            owned(copy).ok_or_else(|| io::Error::other("DuplicateHandle returned no handle"))?;
        // SAFETY: both handles are open.
        if unsafe { CompareObjectHandles(raw(&copy), value) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "process {pid} did not inherit handle {:#x} of this process",
                    value as usize
                ),
            ));
        }
        drop(copy);
        // SAFETY: without a target process, DuplicateHandle duplicates nothing, and only closes
        // the handle of the process, which holds the object of `ours`.
        let closed = unsafe {
            DuplicateHandle(
                raw(&self.process),
                value,
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                0,
                DUPLICATE_CLOSE_SOURCE,
            )
        } != 0;
        if closed {
            Ok(())
        } else {
            unless_exited(Err(io::Error::last_os_error()), exited)
        }
    }
}

/// Returns the outcome of ending process `pid`, as [`ProcessHandle::kill`] does through a handle
/// that has terminate access if `terminate`: `terminate_process` terminates the process, and
/// `exited` checks the handle without waiting.
///
/// A termination that Windows refuses because the process is exiting succeeds, as
/// [`refused_termination`] describes, and so does any failure once `exited` shows the exit.
pub(super) fn end_process(
    terminate: bool,
    pid: u32,
    terminate_process: impl FnOnce() -> io::Result<()>,
    exited: impl FnOnce() -> io::Result<bool>,
) -> io::Result<()> {
    let result = if terminate {
        terminate_process().or_else(refused_termination)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("this user may not terminate process {pid}"),
        ))
    };
    unless_exited(result, exited)
}

/// Opens the process `pid` if it still runs with the creation time `start_time`, and returns
/// `None` if it has exited, even if another process now has its ID.
///
/// The handle pins the process before its identity is checked. Fails with
/// [`io::ErrorKind::PermissionDenied`] if this user may not query the process that has the ID
/// and wait for it, since its identity then cannot be checked.
pub(crate) fn open_process(pid: u32, start_time: u64) -> io::Result<Option<ProcessHandle>> {
    let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    let mut terminate = true;
    let mut opened = open_handle(pid, access | PROCESS_TERMINATE);
    if opened.is_none() && last_error() == ERROR_ACCESS_DENIED {
        terminate = false;
        opened = open_handle(pid, access);
    }
    let Some(process) = opened else {
        return match last_error() {
            ERROR_INVALID_PARAMETER => Ok(None),
            ERROR_ACCESS_DENIED => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("cannot check the identity of process {pid}: this user may not query it"),
            )),
            error => Err(io::Error::from_raw_os_error(error as i32)),
        };
    };
    if has_exited(&process)? || creation_time(&process)? != start_time {
        return Ok(None);
    }
    Ok(Some(ProcessHandle { process, terminate }))
}

/// Returns the identity of the current boot: the boot counter that Windows keeps in the registry
/// value `BootId` of [`BOOT_ID_KEY`] and changes at every boot, little-endian and zero-extended
/// to 16 bytes.
pub(crate) fn boot_id() -> io::Result<[u8; 16]> {
    let key = wide(BOOT_ID_KEY);
    let name = wide("BootId");
    let mut value = 0u32;
    let mut size = mem::size_of::<u32>() as u32;
    // SAFETY: the names are NUL-terminated, the type flag admits only a REG_DWORD, and the
    // output buffer holds `size` bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            ptr::null_mut(),
            (&raw mut value).cast(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS {
        let error = io::Error::from_raw_os_error(status as i32);
        return Err(io::Error::new(
            error.kind(),
            format!("cannot read the registry value HKLM\\{BOOT_ID_KEY}\\BootId: {error}"),
        ));
    }
    let mut id = [0; 16];
    id[..4].copy_from_slice(&value.to_le_bytes());
    Ok(id)
}

/// The registry key of the boot counter, under `HKEY_LOCAL_MACHINE`.
const BOOT_ID_KEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters";

/// The registry key of the machine GUID, under `HKEY_LOCAL_MACHINE`.
const MACHINE_GUID_KEY: &str = r"SOFTWARE\Microsoft\Cryptography";

/// Returns the identity of this host, as OpenVMM records it in a capture's time contract: the
/// machine GUID that Windows keeps in the registry value `MachineGuid` of [`MACHINE_GUID_KEY`], as
/// 16 bytes in the order of its text.
pub(crate) fn host_id() -> io::Result<[u8; 16]> {
    let key = wide(MACHINE_GUID_KEY);
    let name = wide("MachineGuid");
    let mut value = [0u16; 64];
    let mut size = mem::size_of_val(&value) as u32;
    // SAFETY: the names are NUL-terminated, the type flag admits only a REG_SZ, and the output
    // buffer holds `size` bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            value.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS {
        let error = io::Error::from_raw_os_error(status as i32);
        return Err(io::Error::new(
            error.kind(),
            format!(
                "cannot read the registry value HKLM\\{MACHINE_GUID_KEY}\\MachineGuid: {error}"
            ),
        ));
    }
    let units = &value[..(size as usize / 2).min(value.len())];
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    parse_host_id(&String::from_utf16_lossy(&units[..end])).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the registry value HKLM\\{MACHINE_GUID_KEY}\\MachineGuid has an unexpected format"
            ),
        )
    })
}

/// Returns the host's monotonic time, as OpenVMM measures the downtime of a capture within one
/// boot: the interrupt time, which counts the time that the host was asleep, in nanoseconds.
pub(crate) fn monotonic_ns() -> io::Result<u64> {
    let mut intervals = 0u64;
    // SAFETY: the output pointer is valid.
    unsafe { QueryInterruptTimePrecise(&mut intervals) };
    intervals
        .checked_mul(100)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the interrupt time overflows"))
}
