//! The Windows seal, from the file information of the open handle, and the opens of sealed files.

use std::ffi::c_void;
use std::fs::File;
#[cfg(feature = "cli")]
use std::fs::OpenOptions;
use std::io;
use std::mem;
#[cfg(feature = "cli")]
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{FILETIME, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ID_INFO, FILE_TYPE_DISK,
    FILE_TYPE_UNKNOWN, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetFileType,
};
#[cfg(feature = "cli")]
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ};

use super::{FileId, FileKind, FileSeal, SEAL_VERSION, SealScheme, SealTime};

/// The 100-nanosecond intervals from 1601-01-01, the epoch of `FILETIME`, to the Unix epoch.
const UNIX_EPOCH_TICKS: i128 = 116_444_736_000_000_000;
/// The 100-nanosecond intervals in a second.
const TICKS_PER_SECOND: i128 = 10_000_000;

/// See [`FileSeal::of`].
pub(super) fn seal(file: &File) -> io::Result<FileSeal> {
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: the handle is open for as long as `file` is borrowed.
    let file_type = unsafe { GetFileType(handle) };
    if file_type != FILE_TYPE_DISK {
        if file_type == FILE_TYPE_UNKNOWN && io::Error::last_os_error().raw_os_error() != Some(0) {
            return Err(io::Error::last_os_error());
        }
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "only a file of a disk volume has a seal",
        ));
    }
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is open and the output pointer is valid.
    if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut id = FILE_ID_INFO::default();
    // SAFETY: the handle is open, and the buffer is a FILE_ID_INFO of the size that is passed.
    let has_id = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&raw mut id).cast::<c_void>(),
            mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } != 0;
    // Volumes without file ID information, and those that report a zero ID, identify files by
    // their 32-bit serial number and 64-bit file index, as `os.stat` does.
    let file_id = u128::from_le_bytes(id.FileId.Identifier);
    let (device, file_id) = if has_id && file_id != 0 {
        (id.VolumeSerialNumber, file_id)
    } else {
        (
            u64::from(information.dwVolumeSerialNumber),
            (u128::from(information.nFileIndexHigh) << 32) | u128::from(information.nFileIndexLow),
        )
    };
    let kind = if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        FileKind::Directory
    } else {
        FileKind::Regular
    };
    Ok(FileSeal {
        version: SEAL_VERSION,
        scheme: SealScheme::Windows,
        device,
        file_id: FileId(file_id),
        kind,
        length: (u64::from(information.nFileSizeHigh) << 32) | u64::from(information.nFileSizeLow),
        modified: time(information.ftLastWriteTime),
        changed: None,
    })
}

/// Returns the time of a `FILETIME`, in seconds and nanoseconds since the Unix epoch.
fn time(time: FILETIME) -> SealTime {
    let ticks = ((i128::from(time.dwHighDateTime) << 32) | i128::from(time.dwLowDateTime))
        - UNIX_EPOCH_TICKS;
    SealTime {
        // A `FILETIME` spans fewer than 2^41 seconds, and the remainder is below a second.
        seconds: ticks.div_euclid(TICKS_PER_SECOND) as i64,
        nanoseconds: (ticks.rem_euclid(TICKS_PER_SECOND) * 100) as u32,
    }
}

/// See [`super::open_guarded`].
#[cfg(feature = "cli")]
pub(super) fn open_guarded(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// See [`super::open_read`].
pub(super) fn open_read(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(test)]
mod tests;
