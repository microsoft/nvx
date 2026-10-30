use std::ffi::{OsStr, OsString, c_void};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{
    AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle, RawHandle,
};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{
    CompareObjectHandles, DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle,
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND,
    ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_IO_PENDING, ERROR_MORE_DATA,
    ERROR_NO_DATA, ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_NOT_CONNECTED, ERROR_SHARING_VIOLATION, ERROR_SUCCESS, FILETIME, FreeLibrary,
    GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, GetHandleInformation, GetLastError, HANDLE,
    HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation, SetLastError, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
    AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetAclInformation, GetKernelObjectSecurity, GetLengthSid, GetSecurityDescriptorDacl,
    GetSecurityDescriptorOwner, GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
    IsWellKnownSid, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
    SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WinBuiltinAdministratorsSid, WinCreatorOwnerRightsSid, WinCreatorOwnerSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, DELETE, FILE_ALL_ACCESS,
    FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_FLAG_OVERLAPPED, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA,
    GetFileInformationByHandle, MoveFileExW, OPEN_EXISTING, PIPE_ACCESS_INBOUND, READ_CONTROL,
    ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WRITE_DAC, WRITE_OWNER, WriteFile,
};
#[cfg(feature = "cli")]
use windows_sys::Win32::System::Console::{
    CTRL_C_EVENT, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
    OpenJobObjectW, QueryInformationJobObject,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
#[cfg(feature = "cli")]
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, CreateEventW, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetProcessTimes, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_CREATION_FLAGS,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    ResetEvent, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::QueryInterruptTimePrecise;

use super::Transport;
use super::inherit::{Inherited, lock_spawns};
use crate::hypervisor::Hypervisor;

/// Whether this host can run NVX microVMs.
pub const SUPPORTED: bool = true;

/// Encodes `value` as a NUL-terminated UTF-16 string for Win32.
fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain([0]).collect()
}

/// Returns the calling thread's Win32 last-error code.
fn last_error() -> u32 {
    // SAFETY: GetLastError has no preconditions.
    unsafe { GetLastError() }
}

/// Takes ownership of a newly returned valid Win32 `handle`.
fn owned(handle: HANDLE) -> Option<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        None
    } else {
        // SAFETY: the caller passes a freshly opened handle that nothing else owns.
        Some(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
    }
}

/// Borrows an owned handle in the form expected by `windows-sys`.
fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle() as HANDLE
}

/// Opens process `pid` with `access`, returning `None` on any Win32 failure.
fn open_handle(pid: u32, access: u32) -> Option<OwnedHandle> {
    // SAFETY: OpenProcess has no memory-safety preconditions.
    owned(unsafe { OpenProcess(access, 0, pid) })
}

/// Returns the process creation `FILETIME`, used with the PID as a stable identity.
fn creation_time(process: &OwnedHandle) -> io::Result<u64> {
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

/// Returns the outcome of a termination that Windows refused with `error` through a handle with
/// terminate access: success if the process is exiting or has exited, and `error` otherwise.
///
/// Windows refuses to terminate a process whose exit has begun, through a termination or its own
/// exit, with `ERROR_ACCESS_DENIED`, which a handle that has terminate access gets only then. It
/// signals the process only once the process's last thread has exited, so a check of the handle
/// may not show the exit yet.
fn refused_termination(error: io::Error) -> io::Result<()> {
    if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        Ok(())
    } else {
        Err(error)
    }
}

/// Returns how a wait with `left` before its deadline goes on: the whole milliseconds, rounded
/// down, for which it waits for the process, never `INFINITE`, and, when there are none, the
/// remainder below a millisecond that it sleeps before it checks the process a last time.
fn wait_step(left: Duration) -> (u32, Duration) {
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
    process: OwnedHandle,
    /// Whether the handle also has terminate access.
    terminate: bool,
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
            return super::unless_exited(Err(io::Error::last_os_error()), exited);
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
            super::unless_exited(Err(io::Error::last_os_error()), exited)
        }
    }
}

/// Returns the outcome of ending process `pid`, as [`ProcessHandle::kill`] does through a handle
/// that has terminate access if `terminate`: `terminate_process` terminates the process, and
/// `exited` checks the handle without waiting.
///
/// A termination that Windows refuses because the process is exiting succeeds, as
/// [`refused_termination`] describes, and so does any failure once `exited` shows the exit.
fn end_process(
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
    super::unless_exited(result, exited)
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
    super::parse_host_id(&String::from_utf16_lossy(&units[..end])).ok_or_else(|| {
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

/// An OpenVMM process launched by this caller, held through its process handle.
pub(crate) struct LaunchedProcess {
    /// Process handle retained for termination and waiting.
    process: OwnedHandle,
    /// Numeric process identifier reported to instance state.
    pid: u32,
}

impl LaunchedProcess {
    /// Returns the numeric process identifier.
    pub(crate) fn id(&self) -> u32 {
        self.pid
    }

    /// Holds the process through a handle of its own, which outlives this one, and returns it
    /// with the process's creation time.
    pub(crate) fn pin(&self) -> io::Result<(ProcessHandle, u64)> {
        let process = self.process.try_clone()?;
        let start_time = creation_time(&process)?;
        Ok((
            ProcessHandle {
                process,
                terminate: true,
            },
            start_time,
        ))
    }

    /// Terminates the process and returns whether it exited within `timeout`.
    pub(crate) fn terminate(self, timeout: Duration) -> bool {
        // SAFETY: CreateProcessW returned the handle with full access.
        unsafe { TerminateProcess(raw(&self.process), 1) };
        let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1);
        // SAFETY: as above.
        unsafe { WaitForSingleObject(raw(&self.process), milliseconds) == WAIT_OBJECT_0 }
    }

    /// Waits up to `timeout` for the process to exit, and returns its exit status if it has,
    /// as `Popen.wait` reads it with `GetExitCodeProcess`.
    #[cfg(feature = "cli")]
    pub(crate) fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<u32>> {
        let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1);
        // SAFETY: CreateProcessW returned the handle with full access.
        match unsafe { WaitForSingleObject(raw(&self.process), milliseconds) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => return Ok(None),
            _ => return Err(io::Error::last_os_error()),
        }
        let mut status = 0u32;
        // SAFETY: as above, and the output pointer is valid.
        if unsafe { GetExitCodeProcess(raw(&self.process), &mut status) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(status))
    }

    /// Asks the process to end with status 1 without waiting for it, as `Popen.terminate` and
    /// `Popen.kill` do on Windows.
    #[cfg(feature = "cli")]
    pub(crate) fn request_termination(&self) -> io::Result<()> {
        // SAFETY: CreateProcessW returned the handle with full access.
        if unsafe { TerminateProcess(raw(&self.process), 1) } == 0 {
            // A process that is exiting or exited cannot be terminated; Python ignores that too.
            let refused = refused_termination(io::Error::last_os_error());
            if refused.is_err() && self.wait_timeout(Duration::ZERO)?.is_none() {
                return refused;
            }
        }
        Ok(())
    }
}

/// Owns an initialized process-thread attribute list.
struct AttributeList {
    /// Backing storage, kept `usize`-aligned for the opaque list structure.
    buffer: Vec<usize>,
}

impl AttributeList {
    /// Allocates and initializes a list with room for `count` attributes.
    fn new(count: u32) -> io::Result<Self> {
        let mut size = 0;
        // SAFETY: a null list with a size pointer only queries the required size.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &mut size) };
        let mut buffer = vec![0; size.div_ceil(mem::size_of::<usize>())];
        // SAFETY: the buffer holds at least `size` bytes.
        if unsafe {
            InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &mut size)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { buffer })
    }

    /// Returns the mutable opaque-list pointer used by Win32 process APIs.
    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: the list was initialized in `new`.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

/// Handles of a caller that a spawn makes inheritable for as long as it needs them, and whose
/// earlier inheritability it restores when dropped. The spawn holds a
/// [`SpawnLock`](super::inherit::SpawnLock) meanwhile, so that no other spawn of a VMM takes their
/// temporary inheritability for their own.
struct InheritableForSpawn {
    /// Each handle, and whether it was inheritable before.
    handles: Vec<(HANDLE, bool)>,
}

impl InheritableForSpawn {
    /// Makes each of `handles`, which the caller keeps open until this is dropped, inheritable.
    ///
    /// Fails if a handle cannot be, and then restores the handles that it changed.
    fn new(handles: &[HANDLE]) -> io::Result<Self> {
        let mut inheritable = Self {
            handles: Vec::with_capacity(handles.len()),
        };
        for &handle in handles {
            let mut flags = 0;
            // SAFETY: the handle is open, and the output pointer is valid.
            if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the handle is open.
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
            inheritable
                .handles
                .push((handle, flags & HANDLE_FLAG_INHERIT != 0));
        }
        Ok(inheritable)
    }
}

impl Drop for InheritableForSpawn {
    fn drop(&mut self) {
        for &(handle, inheritable) in &self.handles {
            let flags = if inheritable { HANDLE_FLAG_INHERIT } else { 0 };
            // SAFETY: the caller keeps the handle open until this is dropped. A handle whose flag
            // cannot be restored stays inheritable, which only widens what later spawns without a
            // handle list may inherit.
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, flags) };
        }
    }
}

/// Starts OpenVMM without the caller's console so it outlives the caller, in the job object
/// `job`, if any, which [`create_log_job`] creates for the log of its standard output.
///
/// `std::process::Command` lets the child inherit every inheritable handle of the caller. A
/// caller whose own standard output is a pipe, such as a phase process whose parent collects
/// its output, would then hand that pipe to OpenVMM, and the parent would wait for the VM to
/// exit before seeing end-of-file. The handle list restricts inheritance to `stdio` and the
/// caller's handles `inherit`, which keep their values in the child.
///
/// A handle in the list must be inheritable while the process is created. The `stdio` handles
/// are inheritable from here until the spawn closes them, and the handles `inherit` from here
/// until the spawn restores their earlier inheritability, right after the process is created.
/// The spawn holds a [`SpawnLock`](super::inherit::SpawnLock) throughout that window, so spawns
/// of VMMs run it one at a time, and two that allow the same handle restore what it was before
/// either. A process that the caller spawns concurrently without a handle list, as
/// `std::process` does, inherits them if it is created in that window, and holds them until it
/// exits, the log of OpenVMM's standard output and error among them, unless the caller spawns it
/// under a `SpawnLock`, as [`spawn_command`](super::inherit::spawn_command) does; a process that
/// it spawns with a handle list does not.
///
/// OpenVMM starts in `job`, which the process attribute that names it assigns before OpenVMM
/// runs, and which every process that OpenVMM starts then joins too, but no process that the
/// caller spawns. The handle of `job` is inheritable for the whole window, from before the
/// `stdio` handles until after the spawn closes them, so that a process that inherits the log in
/// the window inherits the job too, and keeps it open while it holds the log: [`log_claim_held`]
/// then decides the log's claim by the processes of the job, of which that process is none. The
/// spawn closes its own handle of `job` before it returns.
///
/// Fails with [`io::ErrorKind::InvalidInput`] for a batch file, as [`refuse_batch_file`] says.
pub(crate) fn spawn_detached(
    program: &Path,
    arguments: &[OsString],
    working_dir: &Path,
    stdio: [OwnedHandle; 3],
    inherit: &[Inherited<'_>],
    breakaway_from_job: bool,
    job: Option<OwnedHandle>,
) -> io::Result<LaunchedProcess> {
    let mut flags = CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    if breakaway_from_job {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    spawn_listed(
        program,
        arguments,
        Some(working_dir),
        stdio.map(Some),
        inherit,
        job,
        flags,
    )
}

/// Starts OpenVMM in the caller's console, process group, and working directory, as
/// `subprocess.run` starts a child, with copies of the caller's standard handles as its own.
///
/// Unlike `std::process::Command`, which lets the child inherit every inheritable handle of the
/// caller, it passes the child no other handle of the caller than the handles `inherit`, which
/// keep their values in the child, through the handle list of [`spawn_detached`], with the same
/// window, under the same lock, for processes that the caller spawns concurrently without one.
/// Unlike `std::process::Command` too, which escapes the arguments of a batch file for `cmd.exe`,
/// it fails with [`io::ErrorKind::InvalidInput`] for a batch file, as [`refuse_batch_file`] says.
#[cfg(feature = "cli")]
pub(crate) fn spawn_foreground(
    program: &Path,
    arguments: &[OsString],
    inherit: &[Inherited<'_>],
) -> io::Result<LaunchedProcess> {
    let stdio = [
        standard_handle(STD_INPUT_HANDLE)?,
        standard_handle(STD_OUTPUT_HANDLE)?,
        standard_handle(STD_ERROR_HANDLE)?,
    ];
    spawn_listed(program, arguments, None, stdio, inherit, None, 0)
}

/// Returns a copy of the caller's standard handle `which`, or `None` if the caller has none, as
/// `std::process::Command` gives a child the standard handles of its caller.
#[cfg(feature = "cli")]
fn standard_handle(which: STD_HANDLE) -> io::Result<Option<OwnedHandle>> {
    // SAFETY: GetStdHandle has no preconditions.
    let handle = unsafe { GetStdHandle(which) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Ok(None);
    }
    let mut copy = ptr::null_mut();
    // SAFETY: the source and target are this process, and the output pointer is valid; a
    // standard handle that is not open fails the copy.
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &mut copy,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(owned(copy))
}

/// Starts `program` with `arguments` in `working_dir`, or in the caller's working directory, with
/// the creation flags `flags`, the handles `stdio` as its standard handles, a missing one null,
/// and the caller's handles `inherit`, and no other handle of the caller, in the job object
/// `job`, if any, as [`spawn_detached`] describes. Fails with [`io::ErrorKind::InvalidInput`] for
/// a batch file, as [`refuse_batch_file`] says.
fn spawn_listed(
    program: &Path,
    arguments: &[OsString],
    working_dir: Option<&Path>,
    stdio: [Option<OwnedHandle>; 3],
    inherit: &[Inherited<'_>],
    job: Option<OwnedHandle>,
    flags: PROCESS_CREATION_FLAGS,
) -> io::Result<LaunchedProcess> {
    refuse_batch_file(program)?;
    let application = wide(program);
    let mut command_line = command_line(program.as_os_str(), arguments)?;
    let directory = working_dir.map(wide);
    // No handle may be inheritable for this spawn outside the lock. Locals are dropped in the
    // reverse of their order, so every return restores the handles `inherit`, closes the `stdio`
    // handles and then the job's, which it moves here for that, before it releases the lock.
    let lock = lock_spawns();
    // The job's handle is inheritable before any other, and closes after them all.
    let job = job;
    if let Some(job) = &job {
        // SAFETY: the handle is open and owned by `job`.
        if unsafe { SetHandleInformation(raw(job), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let stdio = stdio;
    let standard = stdio
        .each_ref()
        .map(|handle| handle.as_ref().map_or(ptr::null_mut(), raw));
    let mut handles: Vec<HANDLE> = standard
        .into_iter()
        .filter(|handle| !handle.is_null())
        .collect();
    for handle in &handles {
        // SAFETY: the handle is open and owned by `stdio`.
        if unsafe { SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let standard_handles = handles.len();
    for inherited in inherit {
        let handle = inherited.as_handle().as_raw_handle() as HANDLE;
        // A handle that the list names twice fails the spawn.
        if !handles.contains(&handle) {
            handles.push(handle);
        }
    }
    let inheritable = InheritableForSpawn::new(&handles[standard_handles..])?;
    let jobs: Vec<HANDLE> = job.iter().map(raw).collect();
    // A list cannot be empty: a child that inherits no handle at all is created without one, and
    // without inheritance.
    let lists = u32::from(!handles.is_empty()) + u32::from(!jobs.is_empty());
    let mut attributes = if lists == 0 {
        None
    } else {
        Some(AttributeList::new(lists)?)
    };
    // SAFETY: STARTUPINFOEXW is a plain C structure for which all-zero is a valid value.
    let mut startup: STARTUPINFOEXW = unsafe { mem::zeroed() };
    startup.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
    let mut flags = flags | CREATE_UNICODE_ENVIRONMENT;
    if let Some(attributes) = &mut attributes {
        if !handles.is_empty() {
            // SAFETY: the list has room for this attribute, and `handles` outlives
            // CreateProcessW.
            if unsafe {
                UpdateProcThreadAttribute(
                    attributes.as_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast(),
                    mem::size_of_val(handles.as_slice()),
                    ptr::null_mut(),
                    ptr::null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = standard[0];
            startup.StartupInfo.hStdOutput = standard[1];
            startup.StartupInfo.hStdError = standard[2];
        }
        if !jobs.is_empty() {
            // SAFETY: the list has room for this attribute, and `jobs`, whose handle `job` keeps
            // open, outlives CreateProcessW.
            if unsafe {
                UpdateProcThreadAttribute(
                    attributes.as_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                    jobs.as_ptr().cast(),
                    mem::size_of_val(jobs.as_slice()),
                    ptr::null_mut(),
                    ptr::null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        startup.lpAttributeList = attributes.as_ptr();
        flags |= EXTENDED_STARTUPINFO_PRESENT;
    }
    // SAFETY: PROCESS_INFORMATION is a plain C structure for which all-zero is a valid value.
    let mut information: PROCESS_INFORMATION = unsafe { mem::zeroed() };
    #[cfg(test)]
    tests::in_window();
    // SAFETY: every string is NUL-terminated, the command line is mutable, and the startup
    // information and attribute list stay alive for the call.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            i32::from(!handles.is_empty()),
            flags,
            ptr::null(),
            directory
                .as_ref()
                .map_or(ptr::null(), |directory| directory.as_ptr()),
            &startup.StartupInfo,
            &mut information,
        )
    };
    let created = if created == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    };
    // The child holds its own copies of the handles now, and is in its job, so the caller's need
    // not be inheritable any longer, and the window closes.
    drop(inheritable);
    drop(stdio);
    drop(job);
    drop(lock);
    created?;
    drop(owned(information.hThread));
    let process = owned(information.hProcess)
        .ok_or_else(|| io::Error::other("CreateProcessW returned no process handle"))?;
    Ok(LaunchedProcess {
        process,
        pid: information.dwProcessId,
    })
}

/// Refuses `program` if it is a batch file, which `CreateProcessW` runs through `cmd.exe`.
/// `cmd.exe` parses the command line by rules of its own, under which an argument that holds `&`
/// or `|`, as a path may, starts another command, and the quoting of [`command_line`], which
/// follows the C runtime's rules, does not prevent that. Tells a batch file as `std::process` does,
/// by the last four characters of the program's full path name: `.bat` or `.cmd` in any case, so
/// a file named only `.cmd` is one too.
fn refuse_batch_file(program: &Path) -> io::Result<()> {
    let full: Vec<u16> = std::path::absolute(program)?
        .as_os_str()
        .encode_wide()
        .collect();
    let suffix: Vec<u8> = full[full.len().saturating_sub(4)..]
        .iter()
        .map(|unit| u8::try_from(*unit).map_or(0, |byte| byte.to_ascii_lowercase()))
        .collect();
    if suffix == b".bat" || suffix == b".cmd" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "cannot start the batch file {}: Windows runs batch files through cmd.exe, which \
                 would read their arguments as shell syntax",
                program.display()
            ),
        ));
    }
    Ok(())
}

/// Builds a NUL-terminated command line that the Microsoft C runtime splits back into
/// `program` and `arguments`.
fn command_line(program: &OsStr, arguments: &[OsString]) -> io::Result<Vec<u16>> {
    let mut line = Vec::new();
    for (index, argument) in std::iter::once(program)
        .chain(arguments.iter().map(OsString::as_os_str))
        .enumerate()
    {
        if index > 0 {
            line.push(u16::from(b' '));
        }
        append_argument(&mut line, argument, index == 0)?;
    }
    line.push(0);
    Ok(line)
}

/// Appends one argument using the Microsoft C runtime's backslash-and-quote rules.
fn append_argument(line: &mut Vec<u16>, argument: &OsStr, always_quote: bool) -> io::Result<()> {
    let units: Vec<u16> = argument.encode_wide().collect();
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command-line arguments cannot contain NUL characters",
        ));
    }
    let (quote, backslash) = (u16::from(b'"'), u16::from(b'\\'));
    let quoted = always_quote
        || units.is_empty()
        || units
            .iter()
            .any(|&unit| unit == u16::from(b' ') || unit == u16::from(b'\t'));
    if quoted {
        line.push(quote);
    }
    let mut backslashes = 0;
    for unit in units {
        if unit == backslash {
            backslashes += 1;
        } else {
            if unit == quote {
                line.extend(std::iter::repeat_n(backslash, backslashes + 1));
            }
            backslashes = 0;
        }
        line.push(unit);
    }
    if quoted {
        line.extend(std::iter::repeat_n(backslash, backslashes));
        line.push(quote);
    }
    Ok(())
}

/// Checks that the Windows Hypervisor Platform is installed and reports a hypervisor.
///
/// `WinHvPlatform.dll` is loaded dynamically so hosts without the optional feature can still
/// load this crate.
pub(crate) fn probe_hypervisor(hypervisor: Hypervisor) -> Result<(), String> {
    if hypervisor != Hypervisor::Whp {
        return Err(format!("{hypervisor} is only available on Linux"));
    }
    type WhvGetCapability = unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32;
    const HYPERVISOR_PRESENT: u32 = 0;

    let name = wide("WinHvPlatform.dll");
    // SAFETY: the name is NUL-terminated and the search is limited to System32.
    let module =
        unsafe { LoadLibraryExW(name.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() {
        return Err(
            "the Windows Hypervisor Platform is not installed (WinHvPlatform.dll is missing)"
                .to_owned(),
        );
    }
    // SAFETY: the module handle is valid and the symbol name is NUL-terminated.
    let symbol = unsafe { GetProcAddress(module, c"WHvGetCapability".as_ptr().cast()) };
    let result = symbol.map(|symbol| {
        // SAFETY: WHvGetCapability has this signature in every WinHvPlatform.dll.
        let get_capability: WhvGetCapability = unsafe { std::mem::transmute(symbol) };
        let mut present = 0i32;
        let mut written = 0u32;
        // SAFETY: the output buffer is a 4-byte BOOL, as the capability code requires.
        let status = unsafe {
            get_capability(
                HYPERVISOR_PRESENT,
                (&raw mut present).cast(),
                4,
                &mut written,
            )
        };
        (status, present)
    });
    // SAFETY: the module was loaded above and no function pointer from it outlives this call.
    unsafe { FreeLibrary(module) };
    match result {
        None => Err("WinHvPlatform.dll does not export WHvGetCapability".to_owned()),
        Some((status, _)) if status < 0 => Err(format!(
            "WHvGetCapability failed with HRESULT {status:#010x}"
        )),
        Some((_, 0)) => {
            Err("the Windows Hypervisor Platform reports that no hypervisor is present".to_owned())
        }
        Some(_) => Ok(()),
    }
}

/// Creates `path` if needed. The directory inherits the ACL of its parent, which is private to
/// the user under the default state root.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if fs::symlink_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} is not a plain directory",
            path.display()
        )))
    }
}

/// The `AceType` of an entry that allows access, which `winnt.h` defines.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
/// The `AceType` of an entry that denies access, which `winnt.h` defines.
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
/// The revision of an absolute security descriptor, which `winnt.h` defines.
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

/// The current user, as the token of this process names it.
struct CurrentUser {
    /// The token's `TOKEN_USER`, followed by the security identifier that it points to, in a
    /// buffer aligned for both.
    token_user: Vec<u64>,
}

impl CurrentUser {
    /// Reads the user from the token of this process.
    fn get() -> io::Result<Self> {
        let mut token = ptr::null_mut();
        // SAFETY: the pseudo-handle of the current process needs no closing, and the output
        // pointer is valid.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = owned(token).ok_or_else(io::Error::last_os_error)?;
        let mut len = 0;
        // SAFETY: an empty buffer asks for the length that the user's information needs.
        unsafe { GetTokenInformation(raw(&token), TokenUser, ptr::null_mut(), 0, &mut len) };
        let mut token_user = vec![0u64; (len as usize).div_ceil(mem::size_of::<u64>())];
        // SAFETY: the buffer holds `len` bytes, aligned for a TOKEN_USER.
        let read = unsafe {
            GetTokenInformation(
                raw(&token),
                TokenUser,
                token_user.as_mut_ptr().cast(),
                len,
                &mut len,
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { token_user })
    }

    /// Returns the user's security identifier, which `self` holds.
    fn sid(&self) -> PSID {
        // SAFETY: the buffer holds the TOKEN_USER that GetTokenInformation wrote.
        unsafe { (*self.token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    /// Returns whether `sid` names the user, or SYSTEM or the Administrators group, which may take
    /// over any file anyway.
    fn trusts(&self, sid: PSID) -> bool {
        // SAFETY: both identifiers are valid.
        unsafe {
            EqualSid(sid, self.sid()) != 0
                || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
        }
    }
}

/// Creates the directory `path` of a sandbox store, private to the current user, or checks that
/// an existing one is, and returns why it is not, if it is not.
///
/// A new directory gets the user as its owner and a protected access control list that allows
/// the user alone, which the files created in it inherit, so it is private whatever its parent
/// allows. An existing directory must already be private, as [`check_store_dir`] checks: one
/// that allows others is not restricted, since a handle that they opened keeps its access.
pub(crate) fn create_store_dir(path: &Path) -> io::Result<Option<String>> {
    let user = CurrentUser::get()?;
    let name = wide(path);
    with_owner_only_attributes(
        &user,
        FILE_ALL_ACCESS,
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        |attributes| {
            // SAFETY: the name is NUL-terminated, and the attributes hold a valid descriptor.
            if unsafe { CreateDirectoryW(name.as_ptr(), attributes) } == 0 {
                let error = last_error();
                if error != ERROR_ALREADY_EXISTS {
                    return Err(io::Error::from_raw_os_error(error as i32));
                }
            }
            Ok(())
        },
    )??;
    store_dir_problem(path, &user)
}

/// Runs `create` with the attributes of a new object that `user` owns, whose protected access
/// control list allows `user` alone, `access`, with an entry that the objects created in it
/// inherit as `inheritance` says, and returns what `create` returns. The attributes let no child
/// process inherit the object's handle.
fn with_owner_only_attributes<T>(
    user: &CurrentUser,
    access: u32,
    inheritance: u32,
    create: impl FnOnce(&SECURITY_ATTRIBUTES) -> T,
) -> io::Result<T> {
    let sid = user.sid();
    // SAFETY: the identifier is valid.
    let sid_len = unsafe { GetLengthSid(sid) } as usize;
    // The list holds one entry, whose identifier takes the place of its last field.
    let acl_len = mem::size_of::<ACL>() + mem::size_of::<ACCESS_ALLOWED_ACE>()
        - mem::size_of::<u32>()
        + sid_len;
    let mut acl = vec![0u32; acl_len.div_ceil(mem::size_of::<u32>())];
    let acl_len = (acl.len() * mem::size_of::<u32>()) as u32;
    let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
    // SAFETY: SECURITY_DESCRIPTOR is plain data for which all-zero bytes are valid.
    let mut descriptor: SECURITY_DESCRIPTOR = unsafe { mem::zeroed() };
    let descriptor_ptr: PSECURITY_DESCRIPTOR = (&raw mut descriptor).cast();
    // SAFETY: the list's buffer holds `acl_len` bytes, aligned for it, and the descriptor and the
    // identifier are valid. The descriptor points at the identifier and the list, which outlive
    // its use below.
    let built = unsafe {
        InitializeAcl(acl_ptr, acl_len, ACL_REVISION) != 0
            && AddAccessAllowedAceEx(acl_ptr, ACL_REVISION, inheritance, access, sid) != 0
            && InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) != 0
            && SetSecurityDescriptorOwner(descriptor_ptr, sid, 0) != 0
            && SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) != 0
            && SetSecurityDescriptorControl(descriptor_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
                != 0
    };
    if !built {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor_ptr,
        bInheritHandle: 0,
    };
    Ok(create(&attributes))
}

/// Returns why the directory `path` cannot hold a sandbox store, if it cannot: unless it is
/// private to the current user, as [`create_store_dir`] makes it. It must belong to the user, or
/// to SYSTEM or the Administrators group, and its access control list must allow no one else, as
/// that of a directory under the user's profile does. Its list must also pass entries on to the
/// files created in it, which would otherwise take their creator's default list.
pub(crate) fn check_store_dir(path: &Path) -> io::Result<Option<String>> {
    store_dir_problem(path, &CurrentUser::get()?)
}

/// Creates the directory `path` private to the current user, or checks that an existing one is,
/// as [`create_store_dir`] does for the directory of a sandbox store.
pub(crate) fn create_owner_only_dir(path: &Path) -> io::Result<Option<String>> {
    create_store_dir(path)
}

/// Returns why the directory `path` is not private to the current user, if it is not, as
/// [`check_store_dir`] does for the directory of a sandbox store: unless it is a directory, not a
/// link or any other reparse point, that belongs to the user, or to SYSTEM or the Administrators
/// group, whose access control list allows no one else, and passes entries on to the files
/// created in it.
pub(crate) fn check_owner_only_dir(path: &Path) -> io::Result<Option<String>> {
    check_store_dir(path)
}

/// Returns why the directory `path` cannot hold a sandbox store of `user`, if it cannot.
fn store_dir_problem(path: &Path, user: &CurrentUser) -> io::Result<Option<String>> {
    plain_dir_problem(path, user, Required::PrivateDirectory)
}

/// Returns why the directory `path` is not a plain directory, not a link or any other reparse
/// point, whose owner `user` trusts, and whose access control list is as `required`, if it is not.
fn plain_dir_problem(
    path: &Path,
    user: &CurrentUser,
    required: Required,
) -> io::Result<Option<String>> {
    let directory = OpenOptions::new()
        .access_mode(READ_CONTROL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let attributes = handle_information(directory.as_raw_handle() as HANDLE)?.dwFileAttributes;
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Ok(Some("is not a plain directory".to_owned()));
    }
    privacy_problem(&directory, user, required)
}

/// Returns why the directory `path`, a snapshot that OpenVMM published, is not private to the
/// current user, if it is not: unless it is a directory, not a link or any other reparse point,
/// that belongs to the user, or to SYSTEM or the Administrators group, whose access control list
/// is present, holds only entries that allow or deny access, and allows no one but those and the
/// owner's aliases, as [`check_owner_only_dir`] requires.
///
/// Unlike [`check_owner_only_dir`], it does not require the directory to pass entries on to the
/// files created in it: OpenVMM gives the directory, and each file that it publishes in it, a
/// protected list of their own, which allows SYSTEM and the owner, and the directory's own list
/// already stops anyone else from creating a file in it.
pub(crate) fn check_snapshot_dir(path: &Path) -> io::Result<Option<String>> {
    plain_dir_problem(path, &CurrentUser::get()?, Required::Private)
}

/// Returns why the file that `file` holds open, the lock, a record, or the capability of a
/// sandbox store, is not private to the current user, if it is not.
///
/// A file's own access control list decides who may open it, whatever its directory allows, so
/// a file of a store that allows anyone else could have been written by them.
pub(crate) fn private_file_problem(file: &fs::File) -> io::Result<Option<String>> {
    privacy_problem(file, &CurrentUser::get()?, Required::Private)
}

/// Returns why another user than the current one, SYSTEM, or the Administrators group may change
/// the file that `file` holds open, if one may: unless it belongs to the user, or to SYSTEM or the
/// Administrators group, and its access control list allows no one else to write or extend it, to
/// change its attributes, its list, or its owner, or to delete it. Others may read it.
pub(crate) fn foreign_writer_problem(file: &fs::File) -> io::Result<Option<String>> {
    privacy_problem(file, &CurrentUser::get()?, Required::Unwritable)
}

/// The rights of an access control entry that change a file or directory: write or extend it, or
/// its extended attributes, delete it or its children, change its attributes, its list, or its
/// owner, and the generic rights that include those.
const CHANGE_RIGHTS: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_DELETE_CHILD
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;

/// What [`privacy_problem`] requires of the access control list of a file or directory, whose
/// owner the user must trust in any case.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Required {
    /// That it allow no one else any access.
    Private,
    /// That it allow no one else any access, and pass entries on to the files created in it, as
    /// a directory of a store must.
    PrivateDirectory,
    /// That it allow no one else any of the [`CHANGE_RIGHTS`].
    Unwritable,
}

/// Returns why the file, directory, or other object that `object` holds open is not private to
/// `user`, if it is not: it must belong to the user, or to SYSTEM or the Administrators group,
/// and its access control list must be as `required`. The handle must have read-control access.
fn privacy_problem(
    object: &impl AsRawHandle,
    user: &CurrentUser,
    required: Required,
) -> io::Result<Option<String>> {
    let handle = object.as_raw_handle() as HANDLE;
    let information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut len = 0;
    // SAFETY: an empty buffer asks for the length that the descriptor needs.
    unsafe { GetKernelObjectSecurity(handle, information, ptr::null_mut(), 0, &mut len) };
    let mut buffer = vec![0u64; (len as usize).div_ceil(mem::size_of::<u64>())];
    let descriptor: PSECURITY_DESCRIPTOR = buffer.as_mut_ptr().cast();
    // SAFETY: the handle has read-control access, and the buffer holds `len` bytes.
    if unsafe { GetKernelObjectSecurity(handle, information, descriptor, len, &mut len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut owner = ptr::null_mut();
    let mut present = 0;
    let mut acl = ptr::null_mut();
    let mut defaulted = 0;
    // SAFETY: the descriptor and the output pointers are valid. The owner and the list point
    // into the descriptor's buffer, which outlives their use.
    let read = unsafe {
        GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted) != 0
            && GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted) != 0
    };
    if !read {
        return Err(io::Error::last_os_error());
    }
    if owner.is_null() || !user.trusts(owner) {
        return Ok(Some("belongs to another user".to_owned()));
    }
    // A missing list allows everyone.
    if present == 0 || acl.is_null() {
        return Ok(Some("allows every user".to_owned()));
    }
    // SAFETY: ACL_SIZE_INFORMATION is plain data for which all-zero bytes are valid.
    let mut size: ACL_SIZE_INFORMATION = unsafe { mem::zeroed() };
    // SAFETY: the list is valid, and the output holds an ACL_SIZE_INFORMATION.
    let sized = unsafe {
        GetAclInformation(
            acl,
            (&raw mut size).cast(),
            mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if sized == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut inherited_by_files = false;
    for index in 0..size.AceCount {
        let mut ace = ptr::null_mut();
        // SAFETY: the index is within the list, and the output pointer is valid.
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every entry starts with its header.
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        if u32::from(header.AceFlags) & OBJECT_INHERIT_ACE != 0 {
            inherited_by_files = true;
        }
        match header.AceType {
            ACCESS_DENIED_ACE_TYPE => {}
            ACCESS_ALLOWED_ACE_TYPE => {
                let entry = ace.cast::<ACCESS_ALLOWED_ACE>();
                // SAFETY: an entry that allows access starts as an ACCESS_ALLOWED_ACE.
                let mask = unsafe { (*entry).Mask };
                if required == Required::Unwritable && mask & CHANGE_RIGHTS == 0 {
                    continue;
                }
                // SAFETY: an entry that allows access holds its identifier from `SidStart` on.
                let sid: PSID = unsafe { (&raw mut (*entry).SidStart).cast() };
                // The creator owner and the owner rights stand for an owner: the owner, or the
                // creator of a file in the directory, which only those that it allows may be.
                // SAFETY: the identifier is valid.
                let owner_alias = unsafe {
                    IsWellKnownSid(sid, WinCreatorOwnerSid) != 0
                        || IsWellKnownSid(sid, WinCreatorOwnerRightsSid) != 0
                };
                if !owner_alias && !user.trusts(sid) {
                    return Ok(Some(
                        match required {
                            Required::Unwritable => "allows other users to change it",
                            Required::Private | Required::PrivateDirectory => "allows other users",
                        }
                        .to_owned(),
                    ));
                }
            }
            other => {
                return Ok(Some(format!(
                    "has an access control entry of type {other}, which is not checked"
                )));
            }
        }
    }
    // A file created without entries to inherit takes its creator's default list instead, which
    // may allow anyone.
    if required == Required::PrivateDirectory && !inherited_by_files {
        return Ok(Some(
            "passes no access control entries on to the files created in it".to_owned(),
        ));
    }
    Ok(None)
}

/// Lets the regular file that `file` holds open for writing take no space for the ranges that
/// nobody writes: marks it sparse, which NTFS otherwise allocates whole as the file is extended.
pub(crate) fn make_sparse(file: &fs::File) -> io::Result<()> {
    /// `FSCTL_SET_SPARSE`, in `winioctl.h`, which marks a file sparse without a buffer.
    const FSCTL_SET_SPARSE: u32 = 0x0009_00c4;
    let mut returned = 0;
    // SAFETY: the handle is open for as long as `file` is borrowed, the control takes no input or
    // output buffer, and the count's pointer is valid.
    let set = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_SET_SPARSE,
            ptr::null(),
            0,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Renames `from` to `to` unless `to` exists, which it leaves as it is, failing with
/// [`io::ErrorKind::AlreadyExists`]: `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`.
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both names are NUL-terminated.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Replaces the access control list of the file or directory `path` with the protected list of
/// the descriptor `sddl`, in the security descriptor definition language.
#[cfg(test)]
pub(crate) fn set_access(path: &Path, sddl: &str) -> io::Result<()> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW};

    let sddl = wide(sddl);
    let name = wide(path);
    let mut descriptor = ptr::null_mut();
    // SAFETY: the string is NUL-terminated, and the output pointer is valid.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    let information = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    // SAFETY: the name is NUL-terminated, and the descriptor is valid.
    let set = unsafe { SetFileSecurityW(name.as_ptr(), information, descriptor) };
    let result = if set == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    };
    // SAFETY: the conversion allocated the descriptor, which nothing uses any more.
    unsafe { LocalFree(descriptor) };
    result
}

/// Returns the volume serial number and 64-bit file index of a file or directory, which a file
/// system such as ReFS does not keep unique.
///
/// The object is opened without access rights, which succeeds even while OpenVMM holds it open.
pub fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
    let file = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    handle_identity(file.as_raw_handle() as HANDLE)
}

/// Returns the identity by which the stores tell a file or directory apart from every other,
/// following its links, as [`opened_file_id`] gives it.
///
/// The object is opened without access rights, which succeeds even while OpenVMM holds it open.
pub(crate) fn file_id(path: &Path) -> io::Result<(u64, u128)> {
    let file = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    opened_file_id(&file)
}

/// Returns whether the file at `path` is deleted: whether an open of it finds no file, or one
/// whose deletion is pending, which a file system without POSIX deletion keeps under its name,
/// refusing every open of it, until the last handle to it closes. A file that opens is not, nor is
/// one whose open fails otherwise, such as one whose access control list refuses the open.
///
/// Win32 reports a pending deletion as a denied access, so the file is opened with the native API,
/// whose status tells the two apart, relative to its directory, which spares a native path.
pub(crate) fn is_deleted(path: &Path) -> bool {
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Foundation::{STATUS_DELETE_PENDING, STATUS_OBJECT_NAME_NOT_FOUND};
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;

    let Some(name) = path.file_name() else {
        return false;
    };
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let dir = match Directory::open(dir) {
        Ok(dir) => dir,
        // The file went with its directory.
        Err(error) => return error.kind() == io::ErrorKind::NotFound,
    };
    match dir.open_entry(name, FILE_READ_ATTRIBUTES, FILE_OPEN_REPARSE_POINT) {
        Ok(_) => false,
        Err(status) => matches!(status, STATUS_DELETE_PENDING | STATUS_OBJECT_NAME_NOT_FOUND),
    }
}

/// An open directory, through which operations name its entries, so that they reach the entries
/// of the directory that it opened, whatever its path names meanwhile.
///
/// They use the native API, whose status tells a file whose deletion is pending from one whose
/// access control list refuses the open, which Win32 both reports as a denied access.
pub(crate) struct Directory(fs::File);

impl Directory {
    /// Opens the directory `path`, following its links, without access rights, which the opens of
    /// its entries do not need.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .map(Self)
    }

    /// Returns the identity of its entry `name`, not following a link or other reparse point, as
    /// [`opened_file_id`] gives it, or `None` if it has no such entry.
    pub(crate) fn entry_id(&self, name: &str) -> io::Result<Option<(u64, u128)>> {
        use windows_sys::Wdk::Storage::FileSystem::{
            FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
        };
        use windows_sys::Win32::Foundation::STATUS_OBJECT_NAME_NOT_FOUND;
        use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, SYNCHRONIZE};

        match self.open_entry(
            OsStr::new(name),
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        ) {
            Ok(entry) => opened_file_id(&fs::File::from(entry)).map(Some),
            Err(STATUS_OBJECT_NAME_NOT_FOUND) => Ok(None),
            Err(status) => Err(status_error(status)),
        }
    }

    /// Opens its entry `name` for reading, not following a link or other reparse point, or
    /// returns `None` if it has no such entry, or one whose deletion is pending, which a file
    /// system without POSIX deletion keeps under its name until its last handle closes, and which
    /// is removed. The caller checks what the entry is.
    pub(crate) fn open_file(&self, name: &str) -> io::Result<Option<fs::File>> {
        use windows_sys::Wdk::Storage::FileSystem::{
            FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
        };
        use windows_sys::Win32::Foundation::{STATUS_DELETE_PENDING, STATUS_OBJECT_NAME_NOT_FOUND};
        use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;

        match self.open_entry(
            OsStr::new(name),
            FILE_GENERIC_READ,
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        ) {
            Ok(entry) => Ok(Some(fs::File::from(entry))),
            Err(STATUS_OBJECT_NAME_NOT_FOUND | STATUS_DELETE_PENDING) => Ok(None),
            Err(status) => Err(status_error(status)),
        }
    }

    /// Opens the directory `path` without following a link or other reparse point at its own
    /// name, though those on the way to it are followed, and without access rights, which the
    /// opens of its entries do not need, or returns `None` if it does not exist. Fails if it is a
    /// link, a junction or another reparse point, or not a directory.
    pub(crate) fn search(path: &Path) -> io::Result<Option<Self>> {
        let opened = OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path);
        let dir = match opened {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let attributes = handle_information(dir.as_raw_handle() as HANDLE)?.dwFileAttributes;
        if attributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(io::Error::other(format!(
                "{} is a link, or not a directory",
                path.display()
            )));
        }
        Ok(Some(Self(dir)))
    }

    /// Opens its subdirectory `name` as [`open_dir`](Self::open_dir) does: Windows needs no right
    /// to list a directory to name its entries.
    pub(crate) fn search_dir(&self, name: &str) -> io::Result<Option<Self>> {
        self.open_dir(name)
    }

    /// Opens its subdirectory `name`, not following a link or other reparse point, or returns
    /// `None` if it has no such entry, or one whose deletion is pending. Fails if the entry is a
    /// link, a junction or another reparse point, or not a directory.
    pub(crate) fn open_dir(&self, name: &str) -> io::Result<Option<Self>> {
        use windows_sys::Wdk::Storage::FileSystem::{
            FILE_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
        };
        use windows_sys::Win32::Foundation::{
            STATUS_DELETE_PENDING, STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_NOT_FOUND,
        };
        use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, SYNCHRONIZE};

        let refused = || io::Error::other(format!("{name} is a link, or not a directory"));
        let entry = match self.open_entry(
            OsStr::new(name),
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        ) {
            Ok(entry) => entry,
            Err(STATUS_OBJECT_NAME_NOT_FOUND | STATUS_DELETE_PENDING) => return Ok(None),
            Err(STATUS_NOT_A_DIRECTORY) => return Err(refused()),
            Err(status) => return Err(status_error(status)),
        };
        if handle_information(raw(&entry))?.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(refused());
        }
        Ok(Some(Self(fs::File::from(entry))))
    }

    /// Removes its file `name`, if it has one. A file whose deletion is pending already, which a
    /// file system without POSIX deletion keeps under its name until its last handle closes,
    /// counts as removed.
    pub(crate) fn remove_file(&self, name: &str) -> io::Result<()> {
        use windows_sys::Wdk::Storage::FileSystem::{
            FILE_NON_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
        };
        use windows_sys::Win32::Foundation::{STATUS_DELETE_PENDING, STATUS_OBJECT_NAME_NOT_FOUND};
        use windows_sys::Win32::Storage::FileSystem::{DELETE, SYNCHRONIZE};

        match self.open_entry(
            OsStr::new(name),
            DELETE | SYNCHRONIZE,
            FILE_OPEN_REPARSE_POINT | FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
        ) {
            Ok(entry) => delete_opened(&entry),
            Err(STATUS_OBJECT_NAME_NOT_FOUND | STATUS_DELETE_PENDING) => Ok(()),
            Err(status) => Err(status_error(status)),
        }
    }

    /// Does nothing: Windows has no portable way to flush a directory, so a removal from it is as
    /// durable as the file system makes it.
    pub(crate) fn sync(&self) -> io::Result<()> {
        Ok(())
    }

    /// Opens its entry `name` with the native API, with the access `access` and the options
    /// `options`, sharing it with every other open, or returns the status with which the open
    /// fails.
    fn open_entry(
        &self,
        name: &OsStr,
        access: u32,
        options: u32,
    ) -> Result<OwnedHandle, windows_sys::Win32::Foundation::NTSTATUS> {
        use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
        use windows_sys::Wdk::Storage::FileSystem::NtOpenFile;
        use windows_sys::Win32::Foundation::{
            OBJ_CASE_INSENSITIVE, STATUS_INVALID_HANDLE, STATUS_OBJECT_NAME_INVALID, UNICODE_STRING,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

        let name: Vec<u16> = name.encode_wide().collect();
        let Ok(len) = u16::try_from(mem::size_of_val(name.as_slice())) else {
            return Err(STATUS_OBJECT_NAME_INVALID);
        };
        let name = UNICODE_STRING {
            Length: len,
            MaximumLength: len,
            Buffer: name.as_ptr().cast_mut(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: self.0.as_raw_handle() as HANDLE,
            ObjectName: &name,
            // As Win32 opens files.
            Attributes: OBJ_CASE_INSENSITIVE,
            ..OBJECT_ATTRIBUTES::default()
        };
        let mut handle = ptr::null_mut();
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: the directory's handle is open, the attributes and the name that they point to
        // outlive the call, and the outputs are valid.
        let status = unsafe {
            NtOpenFile(
                &mut handle,
                access,
                &attributes,
                &mut status_block,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                options,
            )
        };
        if status < 0 {
            return Err(status);
        }
        owned(handle).ok_or(STATUS_INVALID_HANDLE)
    }
}

/// Deletes the file that `file`, opened with the right to delete it, holds: with POSIX semantics
/// where the file system supports them, so that its name goes at once, even while other handles
/// hold it, and otherwise once the last handle to it closes.
fn delete_opened(file: &OwnedHandle) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO,
        FILE_DISPOSITION_INFO_EX, FileDispositionInfo, FileDispositionInfoEx,
        SetFileInformationByHandle,
    };

    let posix = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: the handle is open, and the information is a FILE_DISPOSITION_INFO_EX of its size.
    let set = unsafe {
        SetFileInformationByHandle(
            raw(file),
            FileDispositionInfoEx,
            (&raw const posix).cast(),
            mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if set != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // File systems without POSIX deletion refuse the extended information.
    if !matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION)
    ) {
        return Err(error);
    }
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the handle is open, and the information is a FILE_DISPOSITION_INFO of its size.
    let set = unsafe {
        SetFileInformationByHandle(
            raw(file),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Returns the Win32 error to which the native status `status` maps.
fn status_error(status: windows_sys::Win32::Foundation::NTSTATUS) -> io::Error {
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;

    // SAFETY: the conversion only reads its argument.
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code as i32)
}

/// Returns the volume serial number and 64-bit file index of the file that `file` holds open,
/// which a file system such as ReFS does not keep unique: [`opened_file_id`] tells files apart.
pub(crate) fn opened_file_identity(file: &fs::File) -> io::Result<(u64, u64)> {
    handle_identity(file.as_raw_handle() as HANDLE)
}

/// Returns the identity by which the stores tell the file that `file` holds open apart from every
/// other, as its [`FileSeal`](crate::host::seal::FileSeal) holds it: the volume serial number and
/// the 128-bit file ID of `FILE_ID_INFO`, which a file system such as ReFS keeps unique where the
/// 64-bit file index is not.
pub(crate) fn opened_file_id(file: &fs::File) -> io::Result<(u64, u128)> {
    let seal = crate::host::seal::FileSeal::of(file)?;
    Ok((seal.device(), seal.file_id()))
}

/// Returns the number of names, its hard links, of the file that `file` holds open.
pub(crate) fn link_count(file: &fs::File) -> io::Result<u64> {
    Ok(u64::from(
        handle_information(file.as_raw_handle() as HANDLE)?.nNumberOfLinks,
    ))
}

/// Returns the volume serial number and 64-bit file index of the file that `handle` holds open.
fn handle_identity(handle: HANDLE) -> io::Result<(u64, u64)> {
    let information = handle_information(handle)?;
    Ok((
        u64::from(information.dwVolumeSerialNumber),
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    ))
}

/// Returns the information of the file that `handle` holds open.
fn handle_information(handle: HANDLE) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain data for which all-zero bytes are valid.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
    // SAFETY: the caller passes an open handle, and the output pointer is valid.
    if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(information)
}

/// Claims a launch's fresh log, which `file` holds open.
///
/// On Windows the claim needs no further step: it is the job object of the log, in which
/// [`spawn_detached`] starts the VMM, or else the log's handles that the VMM inherits, as
/// [`log_claim_held`] describes.
pub(crate) fn claim_log(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

/// The right to query a job object's information, which `winnt.h` defines.
const JOB_OBJECT_QUERY: u32 = 0x0004;
/// Every right to a job object, which `winnt.h` defines.
const JOB_OBJECT_ALL_ACCESS: u32 = 0x001f_003f;

/// Returns the name of the job object of the log `id`, NUL-terminated: in the namespace of this
/// session, after the volume serial number and the file ID of the log, as [`opened_file_id`]
/// reads them. The names of every store of the session share that namespace, so they take the
/// 128-bit file ID, which a file system such as ReFS keeps unique where the 64-bit file index of
/// a log's journal is not.
fn log_job_name(id: (u64, u128)) -> Vec<u16> {
    wide(format!(r"Local\nvx-log-{:016x}-{:032x}", id.0, id.1))
}

/// Returns the identity that names the job of the log that `log` holds open, as
/// [`opened_file_id`] reads it, or `None` for a log that is not a file of a disk volume, such as
/// `NUL` or a pipe, which has no file ID to name a job after, nor a claim of a store, and so has
/// no job, as both a spawn and a claim check take it.
fn log_job_id(log: &fs::File) -> io::Result<Option<(u64, u128)>> {
    match opened_file_id(log) {
        Ok(id) => Ok(Some(id)),
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(None),
        Err(error) => Err(error),
    }
}

/// Creates the job object of the log that `log` holds open, in which [`spawn_detached`] starts
/// the VMM that writes the log, so that the processes of the job are the log's claim, as
/// [`log_claim_held`] describes. The job limits no resource, but it lets no process that the VMM
/// starts break away from it: one that asks to, with `CREATE_BREAKAWAY_FROM_JOB`, fails with
/// access denied, so that every process that the VMM starts joins it. It belongs to the user, and
/// its protected access control list allows the user alone, so that a claim check tells it from a
/// job that another user creates under its name.
///
/// A name that is taken is judged as a claim check judges it, by [`log_job_named`], so that the
/// VMM never starts outside a job that a claim check would take for the log's. Returns `None`,
/// so that the VMM starts without a job, if what takes the name is not the user's job, which a
/// claim check leaves aside too, as it does a job that a symbolic link under the name led the
/// creation to, and for a log that is not a file of a disk volume, such as `NUL` or a pipe, which
/// has no file ID to name a job after, nor a claim of a store. Fails with
/// [`io::ErrorKind::AlreadyExists`] if a job of the user takes the name, as one of an earlier log
/// of the same file ID, or of the same file before it was truncated, that a process still holds
/// would: the VMM would share it.
pub(crate) fn create_log_job(log: &fs::File) -> io::Result<Option<OwnedHandle>> {
    let Some(id) = log_job_id(log)? else {
        return Ok(None);
    };
    let name = log_job_name(id);
    let user = CurrentUser::get()?;
    let mut error = ERROR_SUCCESS;
    // A job that ends between a creation that found the name taken and the judgment leaves the
    // name free, and the creation is tried again.
    for _ in 0..LOG_JOB_ATTEMPTS {
        let (handle, created) =
            with_owner_only_attributes(&user, JOB_OBJECT_ALL_ACCESS, 0, |attributes| {
                // SAFETY: SetLastError has no preconditions.
                unsafe { SetLastError(ERROR_SUCCESS) };
                // SAFETY: the name is NUL-terminated, and the attributes hold a valid descriptor.
                let handle = unsafe { CreateJobObjectW(attributes, name.as_ptr()) };
                (handle, last_error())
            })?;
        error = created;
        if let Some(job) = owned(handle)
            && error != ERROR_ALREADY_EXISTS
        {
            // A link under the name leads the creation elsewhere, where a claim check takes the
            // job for none of the log's: the VMM starts without one, and the job ends.
            return Ok(holds_name(&job, &name)?.then_some(job));
        }
        match log_job_named(&name)? {
            LogJobName::Users(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "the job object of the log {:016x}-{:032x} exists already, held by \
                         another spawn with the same log file or by a process that inherited it \
                         in one; give each VMM a log file of its own, created anew",
                        id.0, id.1
                    ),
                ));
            }
            LogJobName::Foreign => return Ok(None),
            LogJobName::Free => {}
        }
    }
    Err(io::Error::from_raw_os_error(error as i32))
}

/// How many times [`create_log_job`] tries to create a job whose name it finds taken, and then
/// free.
const LOG_JOB_ATTEMPTS: usize = 3;

/// What takes the name of a log's job.
enum LogJobName {
    /// Nothing.
    Free,
    /// What is not the user's job: an object of another type, a job that the user cannot query,
    /// a job that another user owns or may use, as [`privacy_problem`] tells it, or what a
    /// symbolic link under the name leads to, which another user may have put there, even a job
    /// of the user, as [`holds_name`] tells it.
    Foreign,
    /// The user's job, open to query its processes.
    Users(OwnedHandle),
}

/// The class of the information of `NtQueryObject` that names an object, which `ntifs.h`
/// defines.
const OBJECT_NAME_INFORMATION_CLASS: windows_sys::Wdk::Foundation::OBJECT_INFORMATION_CLASS = 1;

/// Returns the name of the object that `handle` holds open, as the object manager names it: the
/// name that the object holds, whatever symbolic link the name that opened it led through.
fn object_name(handle: HANDLE) -> io::Result<Vec<u16>> {
    use windows_sys::Wdk::Foundation::{NtQueryObject, OBJECT_NAME_INFORMATION};
    use windows_sys::Win32::Foundation::{
        STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL, STATUS_INFO_LENGTH_MISMATCH,
    };

    let mut len: u32 = 1024;
    loop {
        let mut buffer = vec![0u64; (len as usize).div_ceil(mem::size_of::<u64>())];
        let mut needed = 0;
        // SAFETY: the handle is open, and the buffer holds `len` bytes, aligned for an
        // OBJECT_NAME_INFORMATION.
        let status = unsafe {
            NtQueryObject(
                handle,
                OBJECT_NAME_INFORMATION_CLASS,
                buffer.as_mut_ptr().cast(),
                len,
                &mut needed,
            )
        };
        if matches!(
            status,
            STATUS_BUFFER_OVERFLOW | STATUS_BUFFER_TOO_SMALL | STATUS_INFO_LENGTH_MISMATCH
        ) && needed > len
        {
            len = needed;
            continue;
        }
        if status < 0 {
            return Err(status_error(status));
        }
        // SAFETY: the query wrote an OBJECT_NAME_INFORMATION at the start of the buffer.
        let name = unsafe { (*buffer.as_ptr().cast::<OBJECT_NAME_INFORMATION>()).Name };
        if name.Buffer.is_null() {
            return Ok(Vec::new());
        }
        // SAFETY: the name's units lie in the buffer, which outlives this borrow of them.
        let units =
            unsafe { std::slice::from_raw_parts(name.Buffer, usize::from(name.Length) / 2) };
        return Ok(units.to_vec());
    }
}

/// Returns the directory in which `Local\` names objects for this process, as the object manager
/// names it, such as `\Sessions\1\BaseNamedObjects`: that of a fresh event, named at random so
/// that no link can take its name first.
fn local_directory() -> io::Result<Vec<u16>> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let name = wide(format!(r"Local\nvx-namespace-{suffix}"));
    // SAFETY: SetLastError has no preconditions.
    unsafe { SetLastError(ERROR_SUCCESS) };
    // SAFETY: the name is NUL-terminated, and no attributes ask for default security.
    let event = owned(unsafe { CreateEventW(ptr::null(), 1, 0, name.as_ptr()) })
        .ok_or_else(io::Error::last_os_error)?;
    if last_error() == ERROR_ALREADY_EXISTS {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the random name of a fresh event is taken",
        ));
    }
    let mut directory = object_name(raw(&event))?;
    let separator = directory
        .iter()
        .rposition(|&unit| unit == u16::from(b'\\'))
        .ok_or_else(|| io::Error::other("an event of the Local namespace has no directory"))?;
    directory.truncate(separator);
    Ok(directory)
}

/// Returns whether `object`, which the name `name` of the `Local\` namespace, NUL-terminated,
/// opened or created, holds that name itself, rather than one that a symbolic link under it led
/// to. Another user that may create objects in the namespace can put such a link under a name
/// that no object holds, to another object, such as another job of the user.
fn holds_name(object: &OwnedHandle, name: &[u16]) -> io::Result<bool> {
    let name = name.strip_suffix(&[0]).unwrap_or(name);
    let leaf = name
        .iter()
        .rposition(|&unit| unit == u16::from(b'\\'))
        .map_or(name, |separator| &name[separator + 1..]);
    let mut expected = local_directory()?;
    expected.push(u16::from(b'\\'));
    expected.extend_from_slice(leaf);
    Ok(object_name(raw(object))? == expected)
}

/// Returns what takes the name `name` of a log's job, NUL-terminated, as a claim check and a
/// spawn both judge it. A job that the name leads to through a symbolic link is not the log's,
/// even one of the user: once no process holds the log's job, another user that may create
/// objects in the session's namespace can put a link under its name, to an empty job of the user.
fn log_job_named(name: &[u16]) -> io::Result<LogJobName> {
    use windows_sys::Win32::Foundation::{
        ERROR_BAD_PATHNAME, ERROR_CANT_RESOLVE_FILENAME, ERROR_INVALID_NAME, ERROR_PATH_NOT_FOUND,
    };

    // SAFETY: the name is NUL-terminated.
    let opened = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY | READ_CONTROL, 0, name.as_ptr()) };
    let Some(job) = owned(opened) else {
        let error = io::Error::last_os_error();
        return match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_FILE_NOT_FOUND) => Ok(LogJobName::Free),
            // An object that allows the user nothing, or of another type, or what a link under
            // the name leads to instead of an object: a missing directory, a malformed name, or a
            // loop of links.
            Some(
                ERROR_ACCESS_DENIED
                | ERROR_INVALID_HANDLE
                | ERROR_PATH_NOT_FOUND
                | ERROR_BAD_PATHNAME
                | ERROR_INVALID_NAME
                | ERROR_CANT_RESOLVE_FILENAME,
            ) => Ok(LogJobName::Foreign),
            _ => Err(error),
        };
    };
    if !holds_name(&job, name)? {
        return Ok(LogJobName::Foreign);
    }
    Ok(
        match privacy_problem(&job, &CurrentUser::get()?, Required::Private)? {
            Some(_) => LogJobName::Foreign,
            None => LogJobName::Users(job),
        },
    )
}

/// Returns how many processes the job object of the log `id` holds, or `None` if no job of the
/// user takes its name, as [`log_job_named`] judges it.
fn log_job_processes(id: (u64, u128)) -> io::Result<Option<u32>> {
    let LogJobName::Users(job) = log_job_named(&log_job_name(id))? else {
        return Ok(None);
    };
    // SAFETY: the structure is plain data for which all-zero bytes are valid.
    let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { mem::zeroed() };
    // SAFETY: the handle is a job object open for querying, and the information has the size
    // given.
    if unsafe {
        QueryInformationJobObject(
            raw(&job),
            JobObjectBasicAccountingInformation,
            (&raw mut accounting).cast(),
            mem::size_of_val(&accounting) as u32,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(accounting.ActiveProcesses))
}

/// Returns whether a process of its launch still holds the claimed log at `path`, which must be
/// the file `identity`.
///
/// While the job object of the log can be opened by its name, the claim is its processes.
/// [`spawn_detached`] starts the VMM in the job, every process that the VMM starts joins it, and
/// no other process does. The job keeps its name while a process holds a handle to it, not merely
/// while it holds processes, and the spawn closes its own: a process that another thread created
/// while the VMM's spawn had the log's handles inheritable holds the log, and the job's handle with
/// it, so the job can be opened by its name for as long as that process holds the log, and the
/// process, which never joins the job, does not hold the claim. What takes the name but is not the
/// user's job, as [`log_job_named`] tells it, is not the log's job, and leaves the claim to the
/// log's handles: another user cannot end a launch whose VMM still runs by creating an empty job
/// under that name, or a symbolic link under it to an empty job of the user.
///
/// Without the user's job, as for a VMM that no such process outlives, that a launch of an earlier
/// version started outside one, or whose job's name something else took, and for a log that is
/// not a file of a disk volume, which has no job, the claim is whether any process holds the log
/// open. The log is opened first without access rights, which takes no part in sharing checks, to
/// check its identity and to read the file ID that names its job from the same handle; an open
/// that shares nothing then fails with a sharing violation while any process, such as a VMM that
/// inherited the log, holds it. The identity is checked again on the exclusive handle, because a
/// rename between the opens could have replaced the file.
pub(crate) fn log_claim_held(path: &Path, identity: (u64, u64)) -> io::Result<bool> {
    let replaced = || io::Error::other(format!("{} is not the claimed log", path.display()));
    let log = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if opened_file_identity(&log)? != identity {
        return Err(replaced());
    }
    if let Some(id) = log_job_id(&log)?
        && let Some(processes) = log_job_processes(id)?
    {
        return Ok(processes > 0);
    }
    drop(log);
    let exclusive = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path);
    match exclusive {
        Ok(file) if opened_file_identity(&file)? == identity => Ok(false),
        Ok(_) => Err(replaced()),
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => Ok(true),
        Err(error) => Err(error),
    }
}

/// Returns whether the live process `pid` that started at `start_time`, which serves the control
/// endpoint of a launch whose log is the file `log`, is that launch's VMM.
///
/// On Windows it is: the endpoint is a named pipe whose name [`control_endpoint`] draws at random
/// for each launch, so only the VMM that the launch passed it to serves it. The objects of the
/// process are not examined, as a query of another process's synchronous pipe could wait forever.
pub(crate) fn serves_launch(_pid: u32, _start_time: u64, _log: (u64, u64)) -> io::Result<bool> {
    Ok(true)
}

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
            let left = super::time_left(deadline)?;
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

/// Whether Ctrl-C reached the console since [`watch_interrupts`] installed its handler.
#[cfg(feature = "cli")]
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The console control handler, which records Ctrl-C and leaves every other event, such as
/// Ctrl-Break, to the next handler, which terminates the process.
#[cfg(feature = "cli")]
unsafe extern "system" fn record_interrupt(control: u32) -> windows_sys::core::BOOL {
    if control == CTRL_C_EVENT {
        INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
        1
    } else {
        0
    }
}

/// Records each Ctrl-C for [`interrupted`] instead of letting it terminate the process, as
/// Python's `SIGINT` handler, which raises `KeyboardInterrupt`, does. Ctrl-Break still
/// terminates it, as it terminates Python.
#[cfg(feature = "cli")]
pub(crate) fn watch_interrupts() -> io::Result<()> {
    // SAFETY: the handler only stores to an atomic, and stays valid for the process's lifetime.
    if unsafe { SetConsoleCtrlHandler(Some(record_interrupt), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Returns whether Ctrl-C reached the console since [`watch_interrupts`].
#[cfg(feature = "cli")]
pub(crate) fn interrupted() -> bool {
    INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;
    use crate::host::platform::inherit::spawn_command;

    thread_local! {
        /// What a spawn of this thread runs in its window, once its handles are inheritable and
        /// before it creates its process.
        static IN_WINDOW: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Runs what the test of this thread asked its spawns to run in their window, if anything.
    pub(super) fn in_window() {
        IN_WINDOW.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook();
            }
        });
    }

    fn line(program: &str, arguments: &[&str]) -> String {
        let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
        let mut units = command_line(OsStr::new(program), &arguments).unwrap();
        assert_eq!(units.pop(), Some(0));
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn a_process_that_is_exiting_counts_as_killed_before_its_exit_shows() {
        let denied = || Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));
        let invalid = || Err(io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER as i32));
        // Windows refused to terminate a process whose exit began, which the handle does not
        // show yet.
        end_process(true, 7, denied, || Ok(false)).unwrap();
        // Other failures stand until the exit shows.
        let failure = end_process(true, 7, invalid, || Ok(false)).unwrap_err();
        assert_eq!(failure.raw_os_error(), Some(ERROR_INVALID_PARAMETER as i32));
        end_process(true, 7, invalid, || Ok(true)).unwrap();
        // Without terminate access, nothing is terminated, and only a shown exit succeeds.
        let refused = end_process(false, 7, || panic!("terminated"), || Ok(false)).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        end_process(false, 7, || panic!("terminated"), || Ok(true)).unwrap();
    }

    #[test]
    fn a_restore_ready_client_that_closed_before_its_connection_was_accepted_is_read_to_its_end() {
        let pid = std::process::id();
        let receive_all = |listener: &mut ReadyListener| {
            let mut received = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            while listener
                .receive(pid, &mut received, 64, Duration::from_millis(50))
                .unwrap()
            {
                assert!(Instant::now() < deadline, "the endpoint did not close");
            }
            received
        };
        // A client that sent nothing, as a VMM that fails as it starts, and one that sent its
        // event, each closed before the listener accepted their connection: what they sent
        // remains to read, and then the end of the pipe.
        for sent in [&b""[..], b"the event\n"] {
            let (endpoint, mut listener) = listen_ready(Path::new("unused")).unwrap();
            let mut client = fs::OpenOptions::new().write(true).open(&endpoint).unwrap();
            client.write_all(sent).unwrap();
            drop(client);
            assert_eq!(receive_all(&mut listener), sent);
        }
        // A client that closes once the listener accepted its connection.
        let (endpoint, mut listener) = listen_ready(Path::new("unused")).unwrap();
        let client = fs::OpenOptions::new().write(true).open(&endpoint).unwrap();
        let mut received = Vec::new();
        assert!(
            listener
                .receive(pid, &mut received, 64, Duration::from_millis(50))
                .unwrap()
        );
        drop(client);
        assert_eq!(receive_all(&mut listener), b"");
    }

    #[test]
    fn windows_refuses_to_terminate_a_process_that_exited_with_access_denied() {
        let mut child = spawn_command(&mut sleeper()).unwrap();
        let pid = child.id();
        let start_time = process_start_time(pid).unwrap().unwrap();
        let process = open_process(pid, start_time).unwrap().unwrap();
        assert!(process.terminate);
        child.kill().unwrap();
        child.wait().unwrap();
        // SAFETY: the handle has terminate access.
        let terminated = unsafe { TerminateProcess(raw(&process.process), 1) } != 0;
        let error = io::Error::last_os_error();
        assert!(!terminated);
        assert_eq!(error.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32));
        process.kill(pid).unwrap();
    }

    #[test]
    fn a_wait_sleeps_a_remainder_below_a_millisecond_instead_of_waiting_a_tick() {
        assert_eq!(wait_step(Duration::ZERO), (0, Duration::ZERO));
        for remainder in [Duration::from_micros(300), Duration::from_micros(999)] {
            assert_eq!(wait_step(remainder), (0, remainder));
        }
        assert_eq!(wait_step(Duration::from_micros(1_999)), (1, Duration::ZERO));
        let infinite = Duration::from_millis(u64::from(INFINITE));
        assert_eq!(wait_step(infinite), (INFINITE - 1, Duration::ZERO));
        assert_eq!(wait_step(Duration::MAX), (INFINITE - 1, Duration::ZERO));
    }

    #[test]
    fn a_wait_with_less_than_a_millisecond_left_ends_at_its_deadline() {
        let mut child = spawn_command(&mut sleeper()).unwrap();
        let pid = child.id();
        let start_time = process_start_time(pid).unwrap().unwrap();
        let process = open_process(pid, start_time).unwrap().unwrap();
        let started = Instant::now();
        let mut waits = Vec::new();
        for _ in 0..20 {
            let deadline = Instant::now() + Duration::from_micros(300);
            waits.push((process.wait(deadline), Instant::now() >= deadline));
        }
        let elapsed = started.elapsed();
        child.kill().unwrap();
        child.wait().unwrap();
        for (wait, ended_after_deadline) in waits {
            assert_eq!(wait.unwrap(), Some(false));
            assert!(ended_after_deadline);
        }
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
        assert!(process.wait(Instant::now()).unwrap().unwrap());
    }

    #[test]
    fn command_lines_follow_the_c_runtime_rules() {
        assert_eq!(
            line(r"C:\Program Files\openvmm.exe", &["--a", "b c", ""]),
            r#""C:\Program Files\openvmm.exe" --a "b c" """#
        );
        assert_eq!(line("x", &[r#"say "hi""#]), r#""x" "say \"hi\"""#);
        assert_eq!(line("x", &[r#"a"b"#]), r#""x" a\"b"#);
        assert_eq!(line("x", &[r"dir\ with\"]), r#""x" "dir\ with\\""#);
        assert_eq!(line("x", &[r"a\\b"]), r#""x" a\\b"#);
        assert!(command_line(OsStr::new("x"), &[OsString::from("a\0b")]).is_err());
    }

    #[test]
    fn children_inherit_only_their_standard_handles() {
        // An inheritable pipe stands in for a caller's own redirected standard output.
        let (mut leaked_reader, leaked_writer) = io::pipe().unwrap();
        let leaked = OwnedHandle::from(leaked_writer);
        // SAFETY: the handle is open and owned above.
        assert_ne!(
            unsafe { SetHandleInformation(raw(&leaked), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) },
            0
        );
        let (stdin, _stdin_writer) = io::pipe().unwrap();
        let (_stdout_reader, stdout) = io::pipe().unwrap();
        let stderr = stdout.try_clone().unwrap();
        let system = std::env::var_os("SystemRoot").unwrap();
        let cmd = Path::new(&system).join("System32").join("cmd.exe");
        let child = spawn_detached(
            &cmd,
            &[OsString::from("/c"), OsString::from("ping -n 30 127.0.0.1")],
            &std::env::temp_dir(),
            [stdin.into(), stdout.into(), stderr.into()],
            &[],
            false,
            None,
        )
        .unwrap();

        // The running child holds no copy of the leaked pipe, so closing ours ends it.
        drop(leaked);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut rest = Vec::new();
            let _ = sender.send(leaked_reader.read_to_end(&mut rest).map(|_| rest));
        });
        let rest = receiver.recv_timeout(Duration::from_secs(5));
        assert!(child.terminate(Duration::from_secs(10)));
        assert!(
            rest.expect("the child inherited the caller's pipe")
                .unwrap()
                .is_empty()
        );
    }

    /// Returns whether the handle `value` of `process` refers to the object that `ours` holds, as
    /// a handle that the process inherited from this one does, under the same value.
    fn holds(process: &OwnedHandle, value: HANDLE, ours: &impl AsRawHandle) -> bool {
        let mut copy = ptr::null_mut();
        // SAFETY: the process handle has full access, the value is only looked up in that
        // process, and the output pointer is valid.
        let duplicated = unsafe {
            DuplicateHandle(
                raw(process),
                value,
                GetCurrentProcess(),
                &mut copy,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } != 0;
        // A value that names no handle of the process names none of ours.
        let Some(copy) = duplicated.then(|| owned(copy)).flatten() else {
            return false;
        };
        // SAFETY: both handles are open.
        unsafe { CompareObjectHandles(raw(&copy), ours.as_raw_handle() as HANDLE) != 0 }
    }

    /// Returns whether `handle` is inheritable.
    fn inheritable(handle: &impl AsRawHandle) -> bool {
        let mut flags = 0;
        // SAFETY: the handle is open, and the output pointer is valid.
        let read = unsafe { GetHandleInformation(handle.as_raw_handle() as HANDLE, &mut flags) };
        assert_ne!(read, 0, "{}", io::Error::last_os_error());
        flags & HANDLE_FLAG_INHERIT != 0
    }

    /// Makes `handle` inheritable, as native code or a parent may have left it.
    fn make_inheritable(handle: &impl AsRawHandle) {
        // SAFETY: the handle is open.
        let set = unsafe {
            SetHandleInformation(
                handle.as_raw_handle() as HANDLE,
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            )
        };
        assert_ne!(set, 0, "{}", io::Error::last_os_error());
    }

    #[test]
    fn a_child_loses_only_the_inherited_handle_that_is_closed_in_it() {
        let directory = tempfile::tempdir().unwrap();
        let closed = std::fs::File::create(directory.path().join("closed")).unwrap();
        let kept = std::fs::File::create(directory.path().join("kept")).unwrap();
        let (_other_reader, other) = io::pipe().unwrap();
        let (stdin, _stdin_writer) = io::pipe().unwrap();
        let (_stdout_reader, stdout) = io::pipe().unwrap();
        let stderr = stdout.try_clone().unwrap();
        let system = std::env::var_os("SystemRoot").unwrap();
        let ping = Path::new(&system).join("System32").join("PING.EXE");
        let child = spawn_detached(
            &ping,
            &["-n", "30", "127.0.0.1"].map(OsString::from),
            &std::env::temp_dir(),
            [stdin.into(), stdout.into(), stderr.into()],
            &[Inherited::new(&closed), Inherited::new(&kept)],
            false,
            None,
        )
        .unwrap();
        let pid = child.id();
        let (process, start_time) = child.pin().unwrap();
        let value = |handle: &std::fs::File| handle.as_raw_handle() as HANDLE;
        let held = |handle: &std::fs::File| holds(&child.process, value(handle), handle);
        let inherited = (held(&closed), held(&kept));
        // A handle that the child did not inherit is not closed in it, and neither is any of its
        // own.
        let refused = process.close_inherited(pid, other.as_handle());
        // A process found again, through a handle that may not duplicate its handles, keeps them.
        let found = open_process(pid, start_time).unwrap().unwrap();
        let denied = found.close_inherited(pid, closed.as_handle());
        let closing = process.close_inherited(pid, closed.as_handle());
        let after = (held(&closed), held(&kept));
        assert!(child.terminate(Duration::from_secs(10)));
        // Once the child has exited, it holds no handle, so a close succeeds.
        let exited = process.close_inherited(pid, kept.as_handle());
        assert_eq!(inherited, (true, true), "the child did not inherit both");
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(denied.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        closing.unwrap();
        assert_eq!(
            after,
            (false, true),
            "the close did not take the closed handle alone"
        );
        exited.unwrap();
    }

    #[test]
    fn children_inherit_the_allowed_handles_while_they_are_created() {
        let (_allowed_reader, allowed) = io::pipe().unwrap();
        // A handle that was inheritable before the spawn stays so.
        let (_kept_reader, kept) = io::pipe().unwrap();
        make_inheritable(&kept);
        let (_other_reader, other) = io::pipe().unwrap();
        make_inheritable(&other);
        assert!(!inheritable(&allowed));
        let (stdin, _stdin_writer) = io::pipe().unwrap();
        let (_stdout_reader, stdout) = io::pipe().unwrap();
        let stderr = stdout.try_clone().unwrap();
        let system = std::env::var_os("SystemRoot").unwrap();
        let ping = Path::new(&system).join("System32").join("PING.EXE");
        // A handle that the list names twice is inherited once.
        let inherit = [
            Inherited::new(&allowed),
            Inherited::new(&kept),
            Inherited::new(&allowed),
        ];
        let child = spawn_detached(
            &ping,
            &["-n", "30", "127.0.0.1"].map(OsString::from),
            &std::env::temp_dir(),
            [stdin.into(), stdout.into(), stderr.into()],
            &inherit,
            false,
            None,
        )
        .unwrap();
        let value = |handle: &io::PipeWriter| handle.as_raw_handle() as HANDLE;
        let (allowed_held, kept_held, other_held) = (
            holds(&child.process, value(&allowed), &allowed),
            holds(&child.process, value(&kept), &kept),
            holds(&child.process, value(&other), &other),
        );
        let (allowed_inheritable, kept_inheritable) = (inheritable(&allowed), inheritable(&kept));
        assert!(child.terminate(Duration::from_secs(10)));
        assert!(
            allowed_held && kept_held,
            "an allowed handle was not inherited"
        );
        assert!(!other_held, "a handle that was not allowed was inherited");
        assert!(!allowed_inheritable && kept_inheritable);
    }

    #[cfg(feature = "cli")]
    #[test]
    fn foreground_children_inherit_only_the_allowed_handles_besides_their_standard_ones() {
        // A handle that this process leaves inheritable, as its parent may have passed it, and
        // that `std::process::Command` would pass on.
        let (_other_reader, other) = io::pipe().unwrap();
        make_inheritable(&other);
        let (_allowed_reader, allowed) = io::pipe().unwrap();
        let system = std::env::var_os("SystemRoot").unwrap();
        let cmd = Path::new(&system).join("System32").join("cmd.exe");
        // The child shares this process's console and standard handles, so it writes nothing.
        let child = spawn_foreground(
            &cmd,
            &["/c", "ping -n 30 127.0.0.1 >NUL"].map(OsString::from),
            &[Inherited::new(&allowed)],
        )
        .unwrap();
        let value = |handle: &io::PipeWriter| handle.as_raw_handle() as HANDLE;
        let (allowed_held, other_held) = (
            holds(&child.process, value(&allowed), &allowed),
            holds(&child.process, value(&other), &other),
        );
        let allowed_inheritable = inheritable(&allowed);
        assert!(child.terminate(Duration::from_secs(10)));
        assert!(allowed_held, "an allowed handle was not inherited");
        assert!(!other_held, "a handle that was not allowed was inherited");
        assert!(
            !allowed_inheritable,
            "the spawn left the allowed handle inheritable"
        );
    }

    #[test]
    fn batch_files_are_refused_before_cmd_could_read_their_arguments() {
        // Batch files that would leave a marker if they ran, with an argument that cmd.exe would
        // read as a second command.
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("ran");
        let injected = directory.path().join("injected");
        let arguments = [OsString::from(format!("x&echo>{}", injected.display()))];
        // A file named only `.cmd` is a batch file too, and Windows drops the trailing dot of a
        // name, so the last path names `trailing.cmd`.
        for (name, program) in [
            ("plain.cmd", "plain.cmd"),
            ("SHOUT.BAT", "SHOUT.BAT"),
            (".cmd", ".cmd"),
            ("trailing.cmd", "trailing.cmd."),
        ] {
            let script = format!("@echo ran>\"{}\"\r\n", marker.display());
            std::fs::write(directory.path().join(name), script).unwrap();
            let program = directory.path().join(program);
            let refused = |result: io::Result<LaunchedProcess>| {
                let error = result.err().expect("a batch file started");
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
            };
            #[cfg(feature = "cli")]
            refused(spawn_foreground(&program, &arguments, &[]));
            let (stdin, _stdin_writer) = io::pipe().unwrap();
            let (_stdout_reader, stdout) = io::pipe().unwrap();
            let stderr = stdout.try_clone().unwrap();
            refused(spawn_detached(
                &program,
                &arguments,
                directory.path(),
                [stdin.into(), stdout.into(), stderr.into()],
                &[],
                false,
                None,
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
        assert!(!marker.exists() && !injected.exists());
    }

    #[test]
    fn concurrent_spawns_that_allow_one_handle_all_inherit_it_and_restore_it() {
        /// Threads that spawn at once.
        const THREADS: usize = 4;
        /// Spawns of each thread.
        const SPAWNS: usize = 16;
        let (_shared_reader, shared) = io::pipe().unwrap();
        assert!(!inheritable(&shared));
        let system = std::env::var_os("SystemRoot").unwrap();
        let ping = Path::new(&system).join("System32").join("PING.EXE");
        // Each spawn reads the handle's inheritability while others may have made it inheritable
        // for their own spawns, and must neither take that for the handle's own nor find the
        // handle no longer inheritable when it creates its process.
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    let value = shared.as_raw_handle() as HANDLE;
                    for _ in 0..SPAWNS {
                        let (stdin, _stdin_writer) = io::pipe().unwrap();
                        let (_stdout_reader, stdout) = io::pipe().unwrap();
                        let stderr = stdout.try_clone().unwrap();
                        let child = spawn_detached(
                            &ping,
                            &["-n", "30", "127.0.0.1"].map(OsString::from),
                            &std::env::temp_dir(),
                            [stdin.into(), stdout.into(), stderr.into()],
                            &[Inherited::new(&shared)],
                            false,
                            None,
                        )
                        .unwrap();
                        let held = holds(&child.process, value, &shared);
                        assert!(child.terminate(Duration::from_secs(10)));
                        assert!(held, "a spawn did not inherit the allowed handle");
                    }
                });
            }
        });
        assert!(
            !inheritable(&shared),
            "the spawns left the handle inheritable"
        );
    }

    #[test]
    fn a_spawn_that_panics_in_its_window_restores_the_handles_and_lets_other_spawns_run() {
        /// Spawns a stand-in VMM, `cmd /c exit 0`, that is allowed the handles `inherit`.
        fn stand_in(inherit: &[Inherited<'_>]) -> io::Result<LaunchedProcess> {
            let system = std::env::var_os("SystemRoot").unwrap();
            let cmd = Path::new(&system).join("System32").join("cmd.exe");
            let (stdin, _stdin_writer) = io::pipe()?;
            let (_stdout_reader, stdout) = io::pipe()?;
            let stderr = stdout.try_clone()?;
            spawn_detached(
                &cmd,
                &["/c", "exit", "0"].map(OsString::from),
                &std::env::temp_dir(),
                [stdin.into(), stdout.into(), stderr.into()],
                inherit,
                false,
                None,
            )
        }

        let (_shared_reader, shared) = io::pipe().unwrap();
        // The panic unwinds out of the window with the handles inheritable and the lock held,
        // which it poisons, and this thread lives on.
        IN_WINDOW.set(Some(Box::new(|| panic!("a panic in the window"))));
        let panicked = std::panic::catch_unwind(|| stand_in(&[Inherited::new(&shared)])).is_err();
        IN_WINDOW.set(None);
        assert!(panicked, "the spawn did not panic in its window");
        assert!(
            !inheritable(&shared),
            "the spawn left the handle inheritable"
        );
        // Another thread's spawn takes the lock all the same.
        let (sender, spawned) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(stand_in(&[]));
        });
        let other = spawned
            .recv_timeout(WINDOW_TIMEOUT)
            .expect("a spawn after the panic did not take the lock")
            .unwrap();
        assert!(other.terminate(WINDOW_TIMEOUT), "the stand-in did not exit");
    }

    /// The longest wait of the tests of a spawn's window.
    const WINDOW_TIMEOUT: Duration = Duration::from_secs(30);

    /// A process that a test spawned, which is killed and waited for when it is dropped, also
    /// when the test fails.
    struct Running(std::process::Child);

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Returns a command that runs for a minute, without standard streams.
    fn sleeper() -> std::process::Command {
        let mut command = std::process::Command::new("ping");
        command
            .args(["-n", "60", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
    }

    /// A spawn of a stand-in VMM, with a log as its standard output and error, as
    /// `supervisor::spawn` passes the VMM its launch's log, that another thread runs and holds in
    /// its window, with the log's handles inheritable, until it is released.
    struct HeldSpawn {
        /// The log.
        log: std::path::PathBuf,
        /// The log's identity, as its claim is checked.
        identity: (u64, u64),
        /// Releases the spawn from its window when it sends, or when it is dropped.
        release: std::sync::mpsc::Sender<()>,
        /// The thread of the spawn, which returns the stand-in.
        spawn: thread::JoinHandle<io::Result<LaunchedProcess>>,
    }

    impl HeldSpawn {
        /// Starts the spawn of `cmd /c exit 0`, outside any job, with its log in `directory`, and
        /// returns once it is in its window.
        fn start(directory: &Path) -> Self {
            let system = std::env::var_os("SystemRoot").unwrap();
            let cmd = Path::new(&system).join("System32").join("cmd.exe");
            Self::start_with(directory, cmd, &["/c", "exit", "0"], false)
        }

        /// Starts the spawn of a stand-in that runs for a minute, `ping`, in the job of its log,
        /// as `supervisor::spawn` starts the VMM, with its log in `directory`, and returns once it
        /// is in its window.
        fn start_in_job(directory: &Path) -> Self {
            let system = std::env::var_os("SystemRoot").unwrap();
            let ping = Path::new(&system).join("System32").join("PING.EXE");
            Self::start_with(directory, ping, &["-n", "60", "127.0.0.1"], true)
        }

        /// Starts the spawn of `program` with `arguments`, in the job of its log if `in_job`, with
        /// its log in `directory`, and returns once it is in its window.
        fn start_with(
            directory: &Path,
            program: std::path::PathBuf,
            arguments: &[&str],
            in_job: bool,
        ) -> Self {
            let log = directory.join("vmm.log");
            let file = fs::File::create(&log).unwrap();
            let identity = file_identity(&log).unwrap();
            let job = in_job.then(|| {
                create_log_job(&file)
                    .unwrap()
                    .expect("a log on a disk volume has a job")
            });
            let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
            let (entered, in_window) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel::<()>();
            let spawn = thread::spawn(move || {
                IN_WINDOW.set(Some(Box::new(move || {
                    let _ = entered.send(());
                    // A test that fails drops its sender, which ends the wait.
                    let _ = released.recv_timeout(WINDOW_TIMEOUT);
                })));
                let (stdin, _stdin_writer) = io::pipe()?;
                let stderr = file.try_clone()?;
                spawn_detached(
                    &program,
                    &arguments,
                    &std::env::temp_dir(),
                    [stdin.into(), file.into(), stderr.into()],
                    &[],
                    false,
                    job,
                )
            });
            in_window
                .recv_timeout(WINDOW_TIMEOUT)
                .expect("the spawn did not reach its window");
            Self {
                log,
                identity,
                release,
                spawn,
            }
        }

        /// Releases the spawn from its window, and returns the stand-in once it is created, with
        /// the log and its identity.
        fn stand_in(self) -> (LaunchedProcess, std::path::PathBuf, (u64, u64)) {
            let _ = self.release.send(());
            let stand_in = self.spawn.join().unwrap().unwrap();
            (stand_in, self.log, self.identity)
        }

        /// Releases the spawn from its window, and returns once the stand-in has exited, with the
        /// log and its identity.
        fn finish(self) -> (std::path::PathBuf, (u64, u64)) {
            let (stand_in, log, identity) = self.stand_in();
            assert!(
                stand_in.terminate(WINDOW_TIMEOUT),
                "the stand-in did not exit"
            );
            (log, identity)
        }
    }

    /// Returns whether the claim on the log `log`, the file `identity`, is released within
    /// `timeout`.
    fn claim_released_within(log: &Path, identity: (u64, u64), timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !log_claim_held(log, identity).unwrap() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn a_process_spawned_under_the_spawn_lock_does_not_inherit_a_vmms_log() {
        let directory = tempfile::tempdir().unwrap();
        let held = HeldSpawn::start(directory.path());
        // The spawn of the other process waits for the window to close.
        let other = thread::spawn(|| spawn_command(&mut sleeper()).map(Running));
        thread::sleep(Duration::from_millis(500));
        let waited = !other.is_finished();
        let (log, identity) = held.finish();
        let mut other = other.join().unwrap().unwrap();
        // The log's claim is released once the stand-in has exited, while the other process
        // runs.
        let released = claim_released_within(&log, identity, WINDOW_TIMEOUT);
        let running = other.0.try_wait().unwrap().is_none();
        drop(other);
        assert!(
            waited,
            "a spawn under the spawn lock did not wait for the window"
        );
        assert!(
            released,
            "a process spawned under the spawn lock held the log"
        );
        assert!(
            running,
            "the process spawned under the spawn lock exited early"
        );
    }

    #[test]
    fn a_process_spawned_without_the_spawn_lock_in_a_window_holds_the_vmms_log() {
        let directory = tempfile::tempdir().unwrap();
        let held = HeldSpawn::start(directory.path());
        // `std::process::Command` passes the process every handle of this one that is
        // inheritable, the log's among them in the window.
        let other = sleeper().spawn().map(Running);
        let (log, identity) = held.finish();
        let mut other = other.unwrap();
        // The stand-in ran outside a job, so the log's handles are its claim, which the process
        // holds after the stand-in has exited, for longer than a recovery checks a held claim
        // again, as long as it runs.
        let kept = !claim_released_within(&log, identity, Duration::from_secs(2));
        let running = other.0.try_wait().unwrap().is_none();
        drop(other);
        let released = claim_released_within(&log, identity, WINDOW_TIMEOUT);
        assert!(running, "the process spawned in the window exited early");
        assert!(kept, "a process spawned in the window did not hold the log");
        assert!(released, "the log stayed held after the process exited");
    }

    #[test]
    fn a_process_spawned_in_the_window_of_a_vmm_in_its_logs_job_holds_the_log_but_not_its_claim() {
        let directory = tempfile::tempdir().unwrap();
        let held = HeldSpawn::start_in_job(directory.path());
        // The process inherits the log in the window, and the job's handle with it, but does not
        // join the job.
        let other = sleeper().spawn().map(Running);
        let (stand_in, log, identity) = held.stand_in();
        let mut other = other.unwrap();
        let held_while_running = log_claim_held(&log, identity).unwrap();
        assert!(
            stand_in.terminate(WINDOW_TIMEOUT),
            "the stand-in did not exit"
        );
        // Once the stand-in has exited, the job holds no process, and the claim is released, while
        // the process still holds the log.
        let released = claim_released_within(&log, identity, WINDOW_TIMEOUT);
        let holds = held_open(&log);
        let running = other.0.try_wait().unwrap().is_none();
        drop(other);
        assert!(running, "the process spawned in the window exited early");
        assert!(
            held_while_running,
            "the claim was released while the stand-in ran in its log's job"
        );
        assert!(
            holds,
            "the process spawned in the window did not hold the log"
        );
        assert!(
            released,
            "a process spawned in the window held the claim of a VMM in its log's job"
        );
    }

    #[test]
    fn a_log_has_one_job_at_a_time() {
        let directory = tempfile::tempdir().unwrap();
        let log = fs::File::create(directory.path().join("vmm.log")).unwrap();
        let job = create_log_job(&log)
            .unwrap()
            .expect("a log on a disk volume has a job");
        let error = create_log_job(&log).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
        // A job that no process holds is gone.
        drop(job);
        assert!(create_log_job(&log).unwrap().is_some());
    }

    /// Creates a job object named `name`, NUL-terminated, whose security descriptor is `sddl`, in
    /// the security descriptor definition language.
    fn job_with_descriptor(name: &[u16], sddl: &str) -> OwnedHandle {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };

        let sddl = wide(sddl);
        let mut descriptor = ptr::null_mut();
        // SAFETY: the string is NUL-terminated, and the output pointer is valid.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        assert_ne!(converted, 0, "{}", io::Error::last_os_error());
        let attributes = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        // SAFETY: the name is NUL-terminated, and the attributes hold a valid descriptor.
        let job = owned(unsafe { CreateJobObjectW(&attributes, name.as_ptr()) });
        // SAFETY: the conversion allocated the descriptor, which nothing uses any more.
        unsafe { LocalFree(descriptor) };
        job.unwrap_or_else(|| panic!("cannot create a job: {}", io::Error::last_os_error()))
    }

    #[test]
    fn a_job_that_is_not_the_users_under_the_name_of_a_logs_job_is_not_the_logs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vmm.log");
        let log = fs::File::create(&path).unwrap();
        let identity = file_identity(&path).unwrap();
        let id = opened_file_id(&log).unwrap();
        // As jobs of other users look to this one: one that allows it nothing, and one that allows
        // every user, whose processes another user could choose.
        for sddl in ["D:P", "D:P(A;;GA;;;WD)"] {
            let other = job_with_descriptor(&log_job_name(id), sddl);
            // A spawn starts the VMM without a job rather than in that one...
            assert!(create_log_job(&log).unwrap().is_none(), "{sddl}");
            // ...and the claim is the log's handles, which this test holds, whatever that job
            // holds: none of its processes.
            assert_eq!(log_job_processes(id).unwrap(), None, "{sddl}");
            assert!(log_claim_held(&path, identity).unwrap(), "{sddl}");
            drop(other);
        }
        drop(log);
        assert!(!log_claim_held(&path, identity).unwrap());
    }

    #[test]
    fn an_object_of_another_type_under_the_name_of_a_logs_job_is_not_the_logs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vmm.log");
        let log = fs::File::create(&path).unwrap();
        let identity = file_identity(&path).unwrap();
        let id = opened_file_id(&log).unwrap();
        let name = log_job_name(id);
        // An event shares the namespace of jobs, so both the creation and the opening of a job
        // under its name fail, with ERROR_INVALID_HANDLE.
        // SAFETY: the name is NUL-terminated.
        let event = owned(unsafe { CreateEventW(ptr::null(), 1, 0, name.as_ptr()) })
            .unwrap_or_else(|| panic!("cannot create an event: {}", io::Error::last_os_error()));
        // A spawn starts the VMM without a job...
        assert!(create_log_job(&log).unwrap().is_none());
        // ...and the claim is the log's handles, which this test holds.
        assert_eq!(log_job_processes(id).unwrap(), None);
        assert!(log_claim_held(&path, identity).unwrap());
        drop(log);
        assert!(!log_claim_held(&path, identity).unwrap());
        drop(event);
    }

    /// Puts a symbolic link of the object manager under the name `name` of the `Local\`
    /// namespace, NUL-terminated, to the object that the object manager names `target`, as
    /// another user that may create objects in the namespace could, and returns it: the link
    /// lasts until it is closed.
    fn local_link(name: &[u16], target: &[u16]) -> OwnedHandle {
        use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
        use windows_sys::Win32::Foundation::{NTSTATUS, OBJ_CASE_INSENSITIVE, UNICODE_STRING};

        #[link(name = "ntdll", kind = "raw-dylib")]
        unsafe extern "system" {
            fn NtCreateSymbolicLinkObject(
                link: *mut HANDLE,
                access: u32,
                attributes: *const OBJECT_ATTRIBUTES,
                target: *const UNICODE_STRING,
            ) -> NTSTATUS;
        }
        /// Every right to a symbolic link, which `ntifs.h` defines.
        const SYMBOLIC_LINK_ALL_ACCESS: u32 = 0x000f_0001;

        let name = name.strip_suffix(&[0]).unwrap_or(name);
        let separator = name
            .iter()
            .rposition(|&unit| unit == u16::from(b'\\'))
            .unwrap();
        let mut path = local_directory().unwrap();
        path.extend_from_slice(&name[separator..]);
        let string = |units: &[u16]| UNICODE_STRING {
            Length: u16::try_from(mem::size_of_val(units)).unwrap(),
            MaximumLength: u16::try_from(mem::size_of_val(units)).unwrap(),
            Buffer: units.as_ptr().cast_mut(),
        };
        let (path_string, target_string) = (string(&path), string(target));
        let attributes = OBJECT_ATTRIBUTES {
            Length: mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            ObjectName: &path_string,
            Attributes: OBJ_CASE_INSENSITIVE,
            ..OBJECT_ATTRIBUTES::default()
        };
        let mut link = ptr::null_mut();
        // SAFETY: the attributes, the names that they and the target point to, and the output
        // outlive the call.
        let status = unsafe {
            NtCreateSymbolicLinkObject(
                &mut link,
                SYMBOLIC_LINK_ALL_ACCESS,
                &attributes,
                &target_string,
            )
        };
        assert!(
            status >= 0,
            "cannot create a link: {}",
            status_error(status)
        );
        owned(link).unwrap()
    }

    #[test]
    fn what_a_link_under_the_name_of_a_logs_job_leads_to_is_not_the_logs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vmm.log");
        let log = fs::File::create(&path).unwrap();
        let identity = file_identity(&path).unwrap();
        let id = opened_file_id(&log).unwrap();
        let name = log_job_name(id);
        // An empty job of the user, as that of another log whose VMM has exited while a process
        // spawned in its window still holds the job.
        let other = fs::File::create(directory.path().join("other.log")).unwrap();
        let other = create_log_job(&other)
            .unwrap()
            .expect("a log on a disk volume has a job");
        // A link under the name of the log's job, which no job holds, to that one.
        let link = local_link(&name, &object_name(raw(&other)).unwrap());
        // A claim check leaves the job that the link leads to aside, and the claim is the log's
        // handles, which this test holds, where that empty job would have released it...
        assert_eq!(log_job_processes(id).unwrap(), None);
        assert!(log_claim_held(&path, identity).unwrap());
        // ...and a spawn starts the VMM without a job, rather than in the other log's.
        assert!(create_log_job(&log).unwrap().is_none());
        drop(link);
        // A link into a directory that does not exist leads to no object at all.
        let directory_name = local_directory().unwrap();
        let missing: Vec<u16> = directory_name
            .iter()
            .copied()
            .chain(format!(r"\nvx-missing-{:032x}\job", id.1).encode_utf16())
            .collect();
        let link = local_link(&name, &missing);
        assert_eq!(log_job_processes(id).unwrap(), None);
        assert!(log_claim_held(&path, identity).unwrap());
        assert!(create_log_job(&log).unwrap().is_none());
        drop(link);
        // A link to a name that no object holds leads the creation of a job there, which a spawn
        // drops, and a claim check, which finds no job at the end of the link, follows the log's
        // handles.
        let elsewhere: Vec<u16> = directory_name
            .iter()
            .copied()
            .chain(format!(r"\nvx-elsewhere-{:032x}", id.1).encode_utf16())
            .collect();
        let link = local_link(&name, &elsewhere);
        assert!(create_log_job(&log).unwrap().is_none());
        assert_eq!(log_job_processes(id).unwrap(), None);
        assert!(log_claim_held(&path, identity).unwrap());
        drop(link);
        // Without the links, the log takes a job of its own.
        drop(other);
        assert!(create_log_job(&log).unwrap().is_some());
        drop(log);
        assert!(!log_claim_held(&path, identity).unwrap());
    }

    #[test]
    fn a_vmm_never_starts_outside_a_job_that_a_claim_check_takes_for_its_logs() {
        let directory = tempfile::tempdir().unwrap();
        let log = fs::File::create(directory.path().join("vmm.log")).unwrap();
        let id = opened_file_id(&log).unwrap();
        // A job of the user that allows the user to query it alone, which a creation of the log's
        // job cannot open, but which a claim check takes for the log's.
        let job = job_with_descriptor(&log_job_name(id), "D:P(A;;0x20004;;;OW)");
        assert_eq!(log_job_processes(id).unwrap(), Some(0));
        let error = create_log_job(&log).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
        drop(job);
        assert!(create_log_job(&log).unwrap().is_some());
    }

    #[test]
    fn a_vmm_whose_log_is_no_file_of_a_disk_volume_starts_without_a_job() {
        use crate::host::supervisor::{self, VmmCommand};
        use crate::protocol::CAPABILITY_LEN;

        let null = OpenOptions::new().write(true).open("NUL").unwrap();
        assert!(create_log_job(&null).unwrap().is_none());
        let (_reader, writer) = io::pipe().unwrap();
        let pipe = fs::File::from(OwnedHandle::from(writer));
        assert!(create_log_job(&pipe).unwrap().is_none());
        // A pipe has an identity that a claim check can match, but no file ID: a claim check, as
        // a spawn does, takes it for a log without a job.
        assert!(opened_file_identity(&pipe).is_ok());
        assert_eq!(log_job_id(&null).unwrap(), None);
        assert_eq!(log_job_id(&pipe).unwrap(), None);
        let system = std::env::var_os("SystemRoot").unwrap();
        let cmd = Path::new(&system).join("System32").join("cmd.exe");
        let command = VmmCommand::new(
            cmd,
            ["/c", "exit", "0"].map(OsString::from).to_vec(),
            std::env::temp_dir(),
        );
        for log in [null, pipe] {
            let launched = supervisor::spawn(&command, &[7; CAPABILITY_LEN], log).unwrap();
            assert!(launched.kill_and_wait(), "the stand-in did not exit");
        }
    }

    #[test]
    fn the_jobs_of_logs_whose_file_ids_share_their_low_64_bits_are_distinct() {
        // ReFS keeps the 128-bit file IDs of its files unique, but not their low 64 bits, which
        // its file indices are, and every store of a session names its jobs in one namespace.
        assert_ne!(log_job_name((7, 1)), log_job_name((7, (1 << 64) | 1)));
        assert_ne!(log_job_name((7, 1)), log_job_name((8, 1)));
    }

    /// Returns whether any process holds the file at `path` open: an open that shares nothing
    /// then fails with a sharing violation.
    fn held_open(path: &Path) -> bool {
        match OpenOptions::new().read(true).share_mode(0).open(path) {
            Ok(_) => false,
            Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => true,
            Err(error) => panic!("cannot open {}: {error}", path.display()),
        }
    }

    #[test]
    fn a_launch_whose_vmm_exited_ends_while_a_process_spawned_in_its_window_holds_its_log() {
        use crate::host::store::{
            ArtifactIdentity, LaunchId, Recovery, RuntimeIdentity, SandboxStore,
        };
        use crate::host::supervisor::{self, VmmCommand};
        use crate::protocol::CAPABILITY_LEN;

        let directory = tempfile::tempdir().unwrap();
        let store = SandboxStore::create(directory.path().join("sandbox")).unwrap();
        let artifact =
            |digit: char| ArtifactIdentity::sha256(&digit.to_string().repeat(64)).unwrap();
        let runtime = RuntimeIdentity::new(artifact('a'), artifact('b'), artifact('c'));
        let capability = [7; CAPABILITY_LEN];
        let mut locked = store.lock(None).unwrap();
        let mut launch = locked
            .begin_launch(&LaunchId::generate().unwrap(), &runtime, &capability)
            .unwrap();
        let system = std::env::var_os("SystemRoot").unwrap();
        let cmd = Path::new(&system).join("System32").join("cmd.exe");
        let command = VmmCommand::new(
            cmd,
            ["/c", "exit", "0"].map(OsString::from).to_vec(),
            store.dir(),
        );
        // While the VMM is created, another part of the caller spawns a process without the
        // spawn lock, as `std::process::Command` spawns one, which inherits the VMM's log.
        let other = std::rc::Rc::new(std::cell::RefCell::new(None));
        let spawned = std::rc::Rc::clone(&other);
        IN_WINDOW.set(Some(Box::new(move || {
            *spawned.borrow_mut() = Some(sleeper().spawn().map(Running));
        })));
        let launched = supervisor::spawn(&command, &capability, launch.take_log().unwrap());
        IN_WINDOW.set(None);
        let launched = launched.unwrap();
        let mut other = other
            .take()
            .expect("no process was spawned in the window")
            .unwrap();
        let vmm = launch.record_process(&launched).unwrap();
        launched.detach();
        assert!(
            vmm.wait(Instant::now() + WINDOW_TIMEOUT).unwrap(),
            "the stand-in did not exit"
        );
        // The VMM has exited, so the launch ends, whatever the other process holds.
        let ended = launch.abandon().unwrap();
        let holds = held_open(&store.log_path());
        let running = other.0.try_wait().unwrap().is_none();
        drop(other);
        assert!(running, "the process spawned in the window exited early");
        assert!(
            holds,
            "the process spawned in the window did not hold the log"
        );
        assert_eq!(ended, Recovery::Exited);
        assert!(store.journal().unwrap().is_none());
    }

    #[test]
    fn a_thread_that_holds_the_spawn_lock_spawns_under_it_until_its_last_lock_is_dropped() {
        let first = lock_spawns();
        let second = lock_spawns();
        // Neither a spawn of a VMM nor a spawn under the lock waits for the thread's own hold.
        let (stdin, _stdin_writer) = io::pipe().unwrap();
        let (_stdout_reader, stdout) = io::pipe().unwrap();
        let stderr = stdout.try_clone().unwrap();
        let system = std::env::var_os("SystemRoot").unwrap();
        let cmd = Path::new(&system).join("System32").join("cmd.exe");
        let vmm = spawn_detached(
            &cmd,
            &["/c", "exit", "0"].map(OsString::from),
            &std::env::temp_dir(),
            [stdin.into(), stdout.into(), stderr.into()],
            &[],
            false,
            None,
        )
        .unwrap();
        let other = spawn_command(&mut sleeper()).map(Running).unwrap();
        // The hold lasts until the thread's last lock is dropped, whichever that is.
        let (taken, acquired) = std::sync::mpsc::channel();
        let waiter = thread::spawn(move || {
            let _lock = lock_spawns();
            let _ = taken.send(());
        });
        drop(first);
        let early = acquired.recv_timeout(Duration::from_millis(200)).is_ok();
        drop(second);
        let late = acquired.recv_timeout(WINDOW_TIMEOUT).is_ok();
        waiter.join().unwrap();
        assert!(vmm.terminate(WINDOW_TIMEOUT), "the stand-in did not exit");
        drop(other);
        assert!(
            !early,
            "another thread took the lock while this one held it"
        );
        assert!(late, "another thread could not take the lock once released");
    }
}
