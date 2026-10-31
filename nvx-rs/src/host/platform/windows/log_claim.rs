//! The claim of a launch's log: the job object, named after the log, in which its VMM starts, and
//! the checks of whether a process of the launch still holds the log.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE,
    ERROR_SHARING_VIOLATION, ERROR_SUCCESS, HANDLE, SetLastError,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, READ_CONTROL};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
    OpenJobObjectW, QueryInformationJobObject,
};
use windows_sys::Win32::System::Threading::CreateEventW;

use super::identity::{opened_file_id, opened_file_identity, status_error};
use super::private::{CurrentUser, Required, privacy_problem, with_owner_only_attributes};
use super::{last_error, owned, raw, wide};

/// Claims a launch's fresh log, which `file` holds open.
///
/// On Windows the claim needs no further step: it is the job object of the log, in which
/// [`spawn_detached`] starts the VMM, or else the log's handles that the VMM inherits, as
/// [`log_claim_held`] describes.
///
/// [`spawn_detached`]: crate::host::platform::windows::spawn_detached
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
pub(super) fn log_job_name(id: (u64, u128)) -> Vec<u16> {
    wide(format!(r"Local\nvx-log-{:016x}-{:032x}", id.0, id.1))
}

/// Returns the identity that names the job of the log that `log` holds open, as
/// [`opened_file_id`] reads it, or `None` for a log that is not a file of a disk volume, such as
/// `NUL` or a pipe, which has no file ID to name a job after, nor a claim of a store, and so has
/// no job, as both a spawn and a claim check take it.
pub(super) fn log_job_id(log: &fs::File) -> io::Result<Option<(u64, u128)>> {
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
///
/// [`spawn_detached`]: crate::host::platform::windows::spawn_detached
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
pub(super) fn object_name(handle: HANDLE) -> io::Result<Vec<u16>> {
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
pub(super) fn local_directory() -> io::Result<Vec<u16>> {
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
pub(super) fn log_job_processes(id: (u64, u128)) -> io::Result<Option<u32>> {
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
///
/// [`spawn_detached`]: crate::host::platform::windows::spawn_detached
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
///
/// [`control_endpoint`]: crate::host::platform::windows::control_endpoint
pub(crate) fn serves_launch(_pid: u32, _start_time: u64, _log: (u64, u64)) -> io::Result<bool> {
    Ok(true)
}
