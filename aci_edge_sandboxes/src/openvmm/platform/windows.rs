use std::ffi::{OsStr, OsString, c_void};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::time::{Duration, Instant};
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER,
    ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA, ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY,
    ERROR_PIPE_NOT_CONNECTED, FILETIME, FreeLibrary, GENERIC_READ, GENERIC_WRITE, GetLastError,
    HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    GetFileInformationByHandle, OPEN_EXISTING, ReadFile, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, CreateEventW, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetProcessTimes, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};

use super::Transport;
use crate::openvmm::config::Hypervisor;

/// Whether this host can run the OpenVMM backend.
pub(crate) const SUPPORTED: bool = true;

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain([0]).collect()
}

fn last_error() -> u32 {
    // SAFETY: GetLastError has no preconditions.
    unsafe { GetLastError() }
}

fn owned(handle: HANDLE) -> Option<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        None
    } else {
        // SAFETY: the caller passes a freshly opened handle that nothing else owns.
        Some(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
    }
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle() as HANDLE
}

fn open_process(pid: u32, access: u32) -> Option<OwnedHandle> {
    // SAFETY: OpenProcess has no memory-safety preconditions.
    owned(unsafe { OpenProcess(access, 0, pid) })
}

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

fn has_exited(process: &OwnedHandle) -> io::Result<bool> {
    // SAFETY: the handle has synchronize access; a zero timeout only polls.
    match unsafe { WaitForSingleObject(raw(process), 0) } {
        WAIT_TIMEOUT => Ok(false),
        WAIT_OBJECT_0 => Ok(true),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Opens a process, returning `None` if it does not exist or belongs to another user.
///
/// OpenVMM always runs as the calling user, so a process that denies access cannot be the
/// sandbox's VM, even if it reuses the recorded process ID.
fn open_existing_process(pid: u32, access: u32) -> io::Result<Option<OwnedHandle>> {
    if let Some(process) = open_process(pid, access) {
        return Ok(Some(process));
    }
    match last_error() {
        ERROR_INVALID_PARAMETER | ERROR_ACCESS_DENIED => Ok(None),
        error => Err(io::Error::from_raw_os_error(error as i32)),
    }
}

/// Returns the creation time of a live process, `None` if the process no longer exists or
/// belongs to another user, or an error if its state cannot be determined.
pub(crate) fn process_start_time(pid: u32) -> io::Result<Option<u64>> {
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

/// Terminates the process if it still has the recorded identity.
///
/// The open handle pins the process, so the identity check cannot race with PID reuse.
pub(crate) fn kill_process(pid: u32, start_time: u64) -> io::Result<()> {
    let Some(process) = open_existing_process(
        pid,
        PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
    )?
    else {
        return Ok(());
    };
    if has_exited(&process)? || creation_time(&process)? != start_time {
        return Ok(());
    }
    // SAFETY: the handle has terminate access.
    if unsafe { TerminateProcess(raw(&process), 1) } == 0 {
        let error = io::Error::last_os_error();
        return if has_exited(&process)? {
            Ok(())
        } else {
            Err(error)
        };
    }
    // SAFETY: the handle has synchronize access.
    unsafe { WaitForSingleObject(raw(&process), 10_000) };
    Ok(())
}

/// An OpenVMM process launched by this caller, held through its process handle.
pub(crate) struct LaunchedProcess {
    process: OwnedHandle,
    pid: u32,
}

impl LaunchedProcess {
    pub(crate) fn id(&self) -> u32 {
        self.pid
    }

    /// Terminates the process and returns whether it exited within `timeout`.
    pub(crate) fn terminate(self, timeout: Duration) -> bool {
        // SAFETY: CreateProcessW returned the handle with full access.
        unsafe { TerminateProcess(raw(&self.process), 1) };
        let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1);
        // SAFETY: as above.
        unsafe { WaitForSingleObject(raw(&self.process), milliseconds) == WAIT_OBJECT_0 }
    }
}

/// Owns an initialized process-thread attribute list.
struct AttributeList {
    // Backing storage, kept `usize`-aligned for the opaque list structure.
    buffer: Vec<usize>,
}

impl AttributeList {
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

/// Starts OpenVMM without the caller's console so it outlives the caller.
///
/// `std::process::Command` lets the child inherit every inheritable handle of the caller. A
/// caller whose own standard output is a pipe, such as a phase process whose parent collects
/// its output, would then hand that pipe to OpenVMM, and the parent would wait for the VM to
/// exit before seeing end-of-file. The handle list restricts inheritance to `stdio`.
///
/// The `stdio` handles are inheritable only from here until the caller drops them after the
/// spawn, so a process that the caller spawns concurrently through `std::process` could inherit
/// them in that window.
pub(crate) fn spawn_detached(
    program: &Path,
    arguments: &[OsString],
    working_dir: &Path,
    stdio: [OwnedHandle; 3],
    breakaway_from_job: bool,
) -> io::Result<LaunchedProcess> {
    let application = wide(program);
    let mut command_line = command_line(program.as_os_str(), arguments)?;
    let directory = wide(working_dir);
    let handles = stdio.each_ref().map(raw);
    for handle in handles {
        // SAFETY: the handle is open and owned by `stdio`.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let mut attributes = AttributeList::new(1)?;
    // SAFETY: the list has room for one attribute, and `handles` outlives CreateProcessW.
    if unsafe {
        UpdateProcThreadAttribute(
            attributes.as_ptr(),
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            mem::size_of_val(&handles),
            ptr::null_mut(),
            ptr::null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: STARTUPINFOEXW is a plain C structure for which all-zero is a valid value.
    let mut startup: STARTUPINFOEXW = unsafe { mem::zeroed() };
    startup.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    [
        startup.StartupInfo.hStdInput,
        startup.StartupInfo.hStdOutput,
        startup.StartupInfo.hStdError,
    ] = handles;
    startup.lpAttributeList = attributes.as_ptr();
    let mut flags = CREATE_UNICODE_ENVIRONMENT
        | EXTENDED_STARTUPINFO_PRESENT
        | CREATE_NEW_PROCESS_GROUP
        | CREATE_NO_WINDOW;
    if breakaway_from_job {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
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
            1,
            flags,
            ptr::null(),
            directory.as_ptr(),
            &startup.StartupInfo,
            &mut information,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    drop(owned(information.hThread));
    let process = owned(information.hProcess)
        .ok_or_else(|| io::Error::other("CreateProcessW returned no process handle"))?;
    Ok(LaunchedProcess {
        process,
        pid: information.dwProcessId,
    })
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
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
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

/// Returns the volume serial number and file index of a file or directory.
///
/// The object is opened without access rights, which succeeds even while OpenVMM holds it open.
pub(crate) fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
    let file = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain data for which all-zero bytes are valid.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
    // SAFETY: the handle is open and the output pointer is valid.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((
        u64::from(information.dwVolumeSerialNumber),
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    ))
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

struct PipeTransport {
    pipe: OwnedHandle,
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
        while !data.is_empty() {
            let length = u32::try_from(data.len()).unwrap_or(u32::MAX);
            let pointer = data.as_ptr();
            let written = self.overlapped(timeout, |pipe, overlapped| {
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
}
