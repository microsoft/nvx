//! The Windows seal, from the file information of the open handle, and the opens of sealed files.

use std::ffi::{OsString, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{FILETIME, GetLastError, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO,
    FILE_SHARE_READ, FILE_TYPE_DISK, FILE_TYPE_UNKNOWN, FileIdInfo, GETFINALPATHNAMEBYHANDLE_FLAGS,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFileType,
    GetFinalPathNameByHandleW, VOLUME_NAME_DOS, VOLUME_NAME_GUID,
};

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
        // SAFETY: GetLastError has no preconditions.
        if file_type == FILE_TYPE_UNKNOWN && unsafe { GetLastError() } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "only a file of a disk volume has a seal",
        ));
    }
    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain data, for which all-zero bytes are valid.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
    // SAFETY: the handle is open and the output pointer is valid.
    if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: FILE_ID_INFO is plain data, for which all-zero bytes are valid.
    let mut id: FILE_ID_INFO = unsafe { mem::zeroed() };
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

/// See [`super::final_path`].
pub(super) fn final_path(file: &File) -> io::Result<PathBuf> {
    final_path_by(file, VOLUME_NAME_DOS)
}

/// See [`super::final_volume_path`].
pub(super) fn final_volume_path(file: &File) -> io::Result<PathBuf> {
    final_path_by(file, VOLUME_NAME_GUID)
}

/// Returns the final path of the open `file`, which names its volume as `volume` asks.
fn final_path_by(file: &File, volume: GETFINALPATHNAMEBYHANDLE_FLAGS) -> io::Result<PathBuf> {
    let handle = file.as_raw_handle() as HANDLE;
    let mut buffer = vec![0u16; 260];
    loop {
        // SAFETY: the handle is open for as long as `file` is borrowed, and the buffer holds
        // `buffer.len()` units.
        let length = unsafe {
            GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, volume)
        } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        // The path fits, and the length leaves out the NUL that follows it; or the buffer is too
        // small, and the length is what the path needs, with its NUL.
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(PathBuf::from(OsString::from_wide(&buffer)));
        }
        buffer.resize(length, 0);
    }
}

/// See [`super::open_metadata_only`]. Without `FILE_READ_DATA`, Windows refuses every read of
/// the handle, and every mapping of the file through it.
#[cfg(test)]
pub(super) fn open_metadata_only(path: &Path) -> io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_WRITE, SYNCHRONIZE,
    };

    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_times_count_from_the_unix_epoch() {
        let at = |ticks: i128| {
            time(FILETIME {
                dwLowDateTime: (ticks & 0xffff_ffff) as u32,
                dwHighDateTime: (ticks >> 32) as u32,
            })
        };
        assert_eq!(
            at(UNIX_EPOCH_TICKS),
            SealTime {
                seconds: 0,
                nanoseconds: 0
            }
        );
        assert_eq!(
            at(UNIX_EPOCH_TICKS + 15 * TICKS_PER_SECOND + 1),
            SealTime {
                seconds: 15,
                nanoseconds: 100
            }
        );
        // Times before 1970 count back from it, with nanoseconds forward within their second.
        assert_eq!(
            at(UNIX_EPOCH_TICKS - 1),
            SealTime {
                seconds: -1,
                nanoseconds: 999_999_900
            }
        );
        assert_eq!(
            at(0),
            SealTime {
                seconds: -11_644_473_600,
                nanoseconds: 0
            }
        );
    }

    #[test]
    fn final_paths_hold_no_nul_whatever_their_length() {
        use std::os::windows::ffi::OsStrExt;

        // A path shorter than the first buffer, and one longer, which takes a second call with
        // the length, terminator included, that the first one asked for.
        let directory = tempfile::tempdir().unwrap();
        let mut deep = directory.path().to_owned();
        while deep.as_os_str().len() < 300 {
            deep.push("a-directory-that-lengthens-the-final-path");
        }
        std::fs::create_dir_all(&deep).unwrap();
        for path in [directory.path().join("short"), deep.join("long")] {
            std::fs::write(&path, b"").unwrap();
            let file = File::open(&path).unwrap();
            let found = final_path(&file).unwrap();
            assert!(!found.as_os_str().encode_wide().any(|unit| unit == 0));
            assert_eq!(found, std::fs::canonicalize(&path).unwrap());
        }
        let file = File::open(deep.join("long")).unwrap();
        assert!(final_path(&file).unwrap().as_os_str().len() > 260);
    }

    #[test]
    fn final_volume_paths_name_the_file_by_its_volume() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, b"").unwrap();
        let found = final_volume_path(&File::open(&path).unwrap()).unwrap();
        assert!(
            found.to_string_lossy().starts_with(r"\\?\Volume{"),
            "{}",
            found.display()
        );
        // The path names the file, which its final path by its drive names too.
        let reopened = File::open(&found).unwrap();
        assert_eq!(
            final_path(&reopened).unwrap(),
            std::fs::canonicalize(&path).unwrap()
        );
    }
}
