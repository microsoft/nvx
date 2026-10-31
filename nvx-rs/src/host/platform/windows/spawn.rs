//! The spawns of OpenVMM, which pass it no handle of the caller but those that they list, and
//! the processes that they launch.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::path::Path;
use std::time::Duration;
use std::{mem, ptr};

#[cfg(feature = "cli")]
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Foundation::{
    GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0,
};
#[cfg(feature = "cli")]
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};
#[cfg(feature = "cli")]
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetExitCodeProcess};

#[cfg(feature = "cli")]
use super::process::refused_termination;
use super::process::{ProcessHandle, creation_time};
use super::{owned, raw, wide};
use crate::host::platform::inherit::{Inherited, lock_spawns};

/// An OpenVMM process launched by this caller, held through its process handle.
pub(crate) struct LaunchedProcess {
    /// Process handle retained for termination and waiting.
    pub(super) process: OwnedHandle,
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
/// earlier inheritability it restores when dropped. The spawn holds a [`SpawnLock`] meanwhile, so
/// that no other spawn of a VMM takes their temporary inheritability for their own.
///
/// [`SpawnLock`]: crate::host::platform::inherit::SpawnLock
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
/// The spawn holds a [`SpawnLock`] throughout that window, so spawns of VMMs run it one at a time,
/// and two that allow the same handle restore what it was before either. A process that the caller
/// spawns concurrently without a handle list, as `std::process` does, inherits them if it is
/// created in that window, and holds them until it exits, the log of OpenVMM's standard output and
/// error among them, unless the caller spawns it under a `SpawnLock`, as [`spawn_command`] does; a
/// process that it spawns with a handle list does not.
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
///
/// [`create_log_job`]: crate::host::platform::windows::create_log_job
/// [`SpawnLock`]: crate::host::platform::inherit::SpawnLock
/// [`spawn_command`]: crate::host::platform::inherit::spawn_command
/// [`log_claim_held`]: crate::host::platform::windows::log_claim_held
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
    super::tests::in_window();
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
pub(super) fn command_line(program: &OsStr, arguments: &[OsString]) -> io::Result<Vec<u16>> {
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
