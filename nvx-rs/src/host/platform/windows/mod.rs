mod endpoint;
mod identity;
#[cfg(feature = "cli")]
mod interrupt;
mod log_claim;
mod private;
mod process;
mod spawn;

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{FreeLibrary, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Threading::OpenProcess;

use crate::hypervisor::Hypervisor;

pub(crate) use self::endpoint::{
    ReadyListener, connect_endpoint, control_endpoint, endpoint_server_pid, listen_ready,
};
pub use self::identity::file_identity;
pub(crate) use self::identity::{
    Directory, file_id, is_deleted, link_count, opened_file_id, opened_file_identity,
};
#[cfg(feature = "cli")]
pub(crate) use self::interrupt::{interrupted, watch_interrupts};
pub(crate) use self::log_claim::{claim_log, create_log_job, log_claim_held, serves_launch};
pub use self::private::create_private_dir;
#[cfg(test)]
pub(crate) use self::private::set_access;
pub(crate) use self::private::{
    check_owner_only_dir, check_snapshot_dir, check_store_dir, create_owner_only_dir,
    create_store_dir, foreign_writer_problem, make_sparse, private_file_problem, rename_no_replace,
};
pub use self::process::process_start_time;
pub(crate) use self::process::{
    ProcessHandle, boot_id, host_id, monotonic_ns, open_process, own_bytes_read,
};
#[cfg(feature = "cli")]
pub(crate) use self::spawn::spawn_foreground;
pub(crate) use self::spawn::{LaunchedProcess, spawn_detached};

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

#[cfg(test)]
mod tests;
