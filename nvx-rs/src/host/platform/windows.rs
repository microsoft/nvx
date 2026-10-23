use std::ffi::{OsStr, OsString, c_void};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use std::{mem, ptr};

#[cfg(feature = "cli")]
use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND,
    ERROR_INVALID_PARAMETER, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED, ERROR_SHARING_VIOLATION,
    ERROR_SUCCESS, FILETIME, FreeLibrary, GENERIC_READ, GENERIC_WRITE, GetHandleInformation,
    GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
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
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, GetFileInformationByHandle, OPEN_EXISTING,
    READ_CONTROL, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
#[cfg(feature = "cli")]
use windows_sys::Win32::System::Console::{
    CTRL_C_EVENT, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
#[cfg(feature = "cli")]
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, CreateEventW, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetProcessTimes, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};

use super::Transport;
use crate::host::supervisor::Inherited;
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
    /// Process handle with query-limited-information and synchronize access.
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
    pub(crate) fn wait(&self, deadline: Instant) -> io::Result<Option<bool>> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            // Round up, so that a wait shorter than a millisecond does not spin, and never wait
            // for INFINITE.
            let milliseconds = u32::try_from(left.as_nanos().div_ceil(1_000_000))
                .map_or(INFINITE - 1, |value| value.min(INFINITE - 1));
            // SAFETY: the handle has synchronize access.
            match unsafe { WaitForSingleObject(raw(&self.process), milliseconds) } {
                WAIT_OBJECT_0 => return Ok(Some(true)),
                WAIT_TIMEOUT if Instant::now() >= deadline => return Ok(Some(false)),
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

/// Serializes the spawns that make handles of their caller inheritable, from the moment that one
/// reads the inheritability of its handles until it restores it. Two spawns that allow the same
/// handle would otherwise take each other's temporary inheritability for the handle's own: one
/// could find the handle no longer inheritable when it creates its process, and fail, and then
/// leave the handle inheritable for good.
static INHERITANCE: Mutex<()> = Mutex::new(());

/// Handles of a caller that a spawn makes inheritable for as long as it needs them, and whose
/// earlier inheritability it restores when dropped.
struct InheritableForSpawn {
    /// Each handle, and whether it was inheritable before.
    handles: Vec<(HANDLE, bool)>,
    /// [`INHERITANCE`], held while any of the handles is inheritable for this spawn, and released
    /// after their inheritability is restored.
    _serialized: Option<MutexGuard<'static, ()>>,
}

impl InheritableForSpawn {
    /// Makes each of `handles`, which the caller keeps open until this is dropped, inheritable.
    ///
    /// Fails if a handle cannot be, and then restores the handles that it changed.
    fn new(handles: &[HANDLE]) -> io::Result<Self> {
        let mut inheritable = Self {
            handles: Vec::with_capacity(handles.len()),
            _serialized: (!handles.is_empty())
                .then(|| INHERITANCE.lock().unwrap_or_else(PoisonError::into_inner)),
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

/// Starts OpenVMM without the caller's console so it outlives the caller.
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
/// Spawns that allow handles of their caller run that part one at a time, so two that allow the
/// same handle restore what it was before either. A process that the caller spawns concurrently
/// without a handle list, as `std::process` does, could inherit them in that window; a process
/// that it spawns with one could not.
///
/// Fails with [`io::ErrorKind::InvalidInput`] for a batch file, as [`refuse_batch_file`] says.
pub(crate) fn spawn_detached(
    program: &Path,
    arguments: &[OsString],
    working_dir: &Path,
    stdio: [OwnedHandle; 3],
    inherit: &[Inherited<'_>],
    breakaway_from_job: bool,
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
        flags,
    )
}

/// Starts OpenVMM in the caller's console, process group, and working directory, as
/// `subprocess.run` starts a child, with copies of the caller's standard handles as its own.
///
/// Unlike `std::process::Command`, which lets the child inherit every inheritable handle of the
/// caller, it passes the child no other handle of the caller than the handles `inherit`, which
/// keep their values in the child, through the handle list of [`spawn_detached`], with the same
/// window for processes that the caller spawns concurrently without one. Unlike
/// `std::process::Command` too, which escapes the arguments of a batch file for `cmd.exe`, it
/// fails with [`io::ErrorKind::InvalidInput`] for a batch file, as [`refuse_batch_file`] says.
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
    spawn_listed(program, arguments, None, stdio, inherit, 0)
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
/// and the caller's handles `inherit`, and no other handle of the caller, as [`spawn_detached`]
/// describes. Fails with [`io::ErrorKind::InvalidInput`] for a batch file, as
/// [`refuse_batch_file`] says.
fn spawn_listed(
    program: &Path,
    arguments: &[OsString],
    working_dir: Option<&Path>,
    stdio: [Option<OwnedHandle>; 3],
    inherit: &[Inherited<'_>],
    flags: PROCESS_CREATION_FLAGS,
) -> io::Result<LaunchedProcess> {
    refuse_batch_file(program)?;
    let application = wide(program);
    let mut command_line = command_line(program.as_os_str(), arguments)?;
    let directory = working_dir.map(wide);
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
    // A list cannot be empty: a child that inherits no handle at all is created without one, and
    // without inheritance.
    let mut attributes = if handles.is_empty() {
        None
    } else {
        Some(AttributeList::new(1)?)
    };
    // SAFETY: STARTUPINFOEXW is a plain C structure for which all-zero is a valid value.
    let mut startup: STARTUPINFOEXW = unsafe { mem::zeroed() };
    startup.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
    let mut flags = flags | CREATE_UNICODE_ENVIRONMENT;
    if let Some(attributes) = &mut attributes {
        // SAFETY: the list has room for one attribute, and `handles` outlives CreateProcessW.
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
        startup.lpAttributeList = attributes.as_ptr();
        flags |= EXTENDED_STARTUPINFO_PRESENT;
    }
    // SAFETY: PROCESS_INFORMATION is a plain C structure for which all-zero is a valid value.
    let mut information: PROCESS_INFORMATION = unsafe { mem::zeroed() };
    // SAFETY: every string is NUL-terminated, the command line is mutable, and the startup
    // information and attribute list stay alive for the call.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            i32::from(attributes.is_some()),
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
    // The child holds its own copies of the handles now, so the caller's need not be
    // inheritable any longer.
    drop(inheritable);
    drop(stdio);
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
            && AddAccessAllowedAceEx(
                acl_ptr,
                ACL_REVISION,
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                FILE_ALL_ACCESS,
                sid,
            ) != 0
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
    let name = wide(path);
    // SAFETY: the name is NUL-terminated, and the attributes hold a valid descriptor.
    if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
        let error = last_error();
        if error != ERROR_ALREADY_EXISTS {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
    }
    store_dir_problem(path, &user)
}

/// Returns why the directory `path` cannot hold a sandbox store, if it cannot: unless it is
/// private to the current user, as [`create_store_dir`] makes it. It must belong to the user, or
/// to SYSTEM or the Administrators group, and its access control list must allow no one else, as
/// that of a directory under the user's profile does. Its list must also pass entries on to the
/// files created in it, which would otherwise take their creator's default list.
pub(crate) fn check_store_dir(path: &Path) -> io::Result<Option<String>> {
    store_dir_problem(path, &CurrentUser::get()?)
}

/// Returns why the directory `path` cannot hold a sandbox store of `user`, if it cannot.
fn store_dir_problem(path: &Path, user: &CurrentUser) -> io::Result<Option<String>> {
    let directory = OpenOptions::new()
        .access_mode(READ_CONTROL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let attributes = handle_information(directory.as_raw_handle() as HANDLE)?.dwFileAttributes;
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Ok(Some("is not a plain directory".to_owned()));
    }
    privacy_problem(&directory, user, true)
}

/// Returns why the file that `file` holds open, the lock, a record, or the capability of a
/// sandbox store, is not private to the current user, if it is not.
///
/// A file's own access control list decides who may open it, whatever its directory allows, so
/// a file of a store that allows anyone else could have been written by them.
pub(crate) fn private_file_problem(file: &fs::File) -> io::Result<Option<String>> {
    privacy_problem(file, &CurrentUser::get()?, false)
}

/// Returns why the file or directory that `file` holds is not private to `user`, if it is not: it
/// must belong to the user, or to SYSTEM or the Administrators group, and its access control list
/// must allow no one else. A `directory` must also pass entries on to the files created in it.
fn privacy_problem(
    file: &fs::File,
    user: &CurrentUser,
    directory: bool,
) -> io::Result<Option<String>> {
    let handle = file.as_raw_handle() as HANDLE;
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
                // SAFETY: an entry that allows access holds its identifier from `SidStart` on.
                let sid: PSID =
                    unsafe { (&raw mut (*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast() };
                // The creator owner and the owner rights stand for an owner: the owner, or the
                // creator of a file in the directory, which only those that it allows may be.
                // SAFETY: the identifier is valid.
                let owner_alias = unsafe {
                    IsWellKnownSid(sid, WinCreatorOwnerSid) != 0
                        || IsWellKnownSid(sid, WinCreatorOwnerRightsSid) != 0
                };
                if !owner_alias && !user.trusts(sid) {
                    return Ok(Some("allows other users".to_owned()));
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
    if directory && !inherited_by_files {
        return Ok(Some(
            "passes no access control entries on to the files created in it".to_owned(),
        ));
    }
    Ok(None)
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

/// Returns the volume serial number and file index of a file or directory.
///
/// The object is opened without access rights, which succeeds even while OpenVMM holds it open.
pub fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
    let file = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    handle_identity(file.as_raw_handle() as HANDLE)
}

/// Returns the volume serial number and file index of the file that `file` holds open.
pub(crate) fn opened_file_identity(file: &fs::File) -> io::Result<(u64, u64)> {
    handle_identity(file.as_raw_handle() as HANDLE)
}

/// Returns the volume serial number and file index of the file that `handle` holds open.
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
/// On Windows the claim is the open handle itself, which the VMM inherits: while any process
/// holds the log open, an open that shares nothing fails, so the claim needs no further step.
pub(crate) fn claim_log(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

/// Returns whether a process still holds the claimed log at `path`, which must be the file
/// `identity`: whether any process holds it open.
///
/// An open without access rights takes no part in sharing checks, so the identity is checked
/// first; an open that shares nothing then fails with a sharing violation while any process,
/// such as a VMM that inherited the log, holds it. The identity is checked again on the
/// exclusive handle, because a rename between the opens could have replaced the file.
pub(crate) fn log_claim_held(path: &Path, identity: (u64, u64)) -> io::Result<bool> {
    let replaced = || io::Error::other(format!("{} is not the claimed log", path.display()));
    if file_identity(path)? != identity {
        return Err(replaced());
    }
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
    /// Runs one overlapped operation to completion or timeout.
    ///
    /// `start` issues the operation. The function always waits for the operation to finish,
    /// cancelling it on timeout, so the `OVERLAPPED` and the caller's buffer outlive it.
    fn overlapped(
        &mut self,
        timeout: Option<Duration>,
        start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32,
    ) -> io::Result<usize> {
        let pipe = raw(&self.pipe);
        // SAFETY: OVERLAPPED is plain data for which all-zero bytes are a valid initial state.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        // The event can be reused: ReadFile and WriteFile reset it to nonsignaled before they
        // start an operation.
        overlapped.hEvent = raw(&self.event);
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
        // SAFETY: the event belongs to this transport.
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
    use std::io::Read;

    use super::*;

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
    fn windows_refuses_to_terminate_a_process_that_exited_with_access_denied() {
        let mut child = std::process::Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
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
        use windows_sys::Win32::Foundation::{
            CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle,
        };

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
}
