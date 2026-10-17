//! The Win32 calls with which CPython's `nt` module stats files, reads links, and asks for final
//! path names.
//!
//! [`stat`] ports `win32_xstat_slow_impl`: it opens the path for its attributes only, and falls
//! back to `FindFirstFileW` when the path cannot be opened. Without links followed, only
//! `IO_REPARSE_TAG_SYMLINK` reparse points are links, as `os.lstat` reports them.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BAD_NET_NAME, ERROR_CANT_ACCESS_FILE, ERROR_FILE_NOT_FOUND,
    ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_READY, ERROR_NOT_SUPPORTED,
    ERROR_PATH_NOT_FOUND, ERROR_SHARING_VIOLATION, GENERIC_READ, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_CHAR, FILE_TYPE_DISK, FILE_TYPE_PIPE,
    FILE_TYPE_UNKNOWN, FileAttributeTagInfo, FileBasicInfo, FileIdInfo, FindClose, FindFirstFileW,
    GetFileAttributesW, GetFileInformationByHandle, GetFileInformationByHandleEx, GetFileType,
    GetFinalPathNameByHandleW, INVALID_FILE_ATTRIBUTES, OPEN_EXISTING, VOLUME_NAME_DOS,
    WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// The reparse tag of a symbolic link.
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// The reparse tag of a junction, or mount point.
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// The control code that reads a reparse point's data (`winioctl.h`).
const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
/// The largest reparse point that `FSCTL_GET_REPARSE_POINT` returns.
const MAXIMUM_REPARSE_DATA_BUFFER_SIZE: usize = 16 * 1024;

/// The type of file that `st_mode` gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatMode {
    /// `S_IFREG`.
    Regular,
    /// `S_IFDIR`.
    Directory,
    /// `S_IFLNK`: a symbolic link, without links followed.
    Link,
    /// `S_IFCHR`, as `\\.\nul`.
    Character,
    /// `S_IFIFO`, as a named pipe.
    Fifo,
    /// `S_IFBLK`, as a volume.
    Block,
    /// No type, as an unknown device.
    Unknown,
}

/// The fields of `os.stat` and `os.lstat` that NVX reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Status {
    /// The type of file.
    pub(crate) mode: StatMode,
    /// `st_dev`: the volume serial number.
    pub(crate) device: u64,
    /// `st_ino`: the file ID.
    pub(crate) inode: u128,
}

/// Returns the NUL-terminated UTF-16 form of `path`, refusing an embedded NUL as Python does.
fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "embedded null character in path",
        ));
    }
    units.push(0);
    Ok(units)
}

/// Returns the calling thread's last Win32 error.
fn last_error() -> u32 {
    // SAFETY: GetLastError has no preconditions.
    unsafe { GetLastError() }
}

/// Returns the error with Win32 error code `code`.
fn error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

/// Opens `path` with `access`, `share`, and `flags`, or returns the Win32 error code.
fn open(path: &[u16], access: u32, share: u32, flags: u32) -> Result<OwnedHandle, u32> {
    // SAFETY: `path` is NUL-terminated, and the other arguments are plain values or null.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            share,
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(last_error())
    } else {
        // SAFETY: the handle was just opened, and nothing else owns it.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
    }
}

/// Borrows `handle` in the form that `windows-sys` takes.
fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle() as HANDLE
}

/// Reads the information of `class` into `T`, or returns the Win32 error code. `T` must be the
/// plain-data structure of `class`.
fn information<T: Copy>(handle: &OwnedHandle, class: i32) -> Result<T, u32> {
    // SAFETY: the structures that this module reads are plain data, for which all-zero bytes are
    // valid.
    let mut value: T = unsafe { mem::zeroed() };
    // SAFETY: the handle is open, and the buffer is a `T` of the size that is passed.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            raw(handle),
            class,
            (&raw mut value).cast::<c_void>(),
            mem::size_of::<T>() as u32,
        )
    };
    if ok == 0 {
        Err(last_error())
    } else {
        Ok(value)
    }
}

/// The attributes and reparse tag that `FindFirstFileW` gives for `path`, as CPython's
/// `attributes_from_dir` reads them, with the file's real name.
struct FindData {
    /// The file attributes.
    attributes: u32,
    /// The reparse tag, or zero if the file is not a reparse point.
    reparse_tag: u32,
    /// The name of the file as the directory holds it.
    name: Vec<u16>,
}

/// Calls `FindFirstFileW` for `path`, which must be NUL-terminated, or returns the Win32 error.
fn find_first_file(path: &[u16]) -> Result<FindData, u32> {
    // SAFETY: WIN32_FIND_DATAW is plain data, for which all-zero bytes are valid.
    let mut data: WIN32_FIND_DATAW = unsafe { mem::zeroed() };
    // SAFETY: the path is NUL-terminated and the output pointer is valid.
    let handle = unsafe { FindFirstFileW(path.as_ptr(), &mut data) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    // SAFETY: the handle is a find handle that was just opened.
    unsafe { FindClose(handle) };
    let length = data
        .cFileName
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(data.cFileName.len());
    Ok(FindData {
        attributes: data.dwFileAttributes,
        reparse_tag: if data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            data.dwReserved0
        } else {
            0
        },
        name: data.cFileName[..length].to_vec(),
    })
}

/// Returns the find data of `path` as `attributes_from_dir` reads it: a path that ends in
/// separators is queried without them, and a root has nothing to query.
fn attributes_from_dir(path: &[u16]) -> Result<FindData, u32> {
    let separator = |unit: u16| unit == u16::from(b'\\') || unit == u16::from(b'/');
    let text = &path[..path.len() - 1];
    if text.last().is_some_and(|&unit| separator(unit)) {
        let mut end = text.len() - 1;
        while end > 0 && separator(text[end]) {
            end -= 1;
        }
        if end == 0 || (end == 1 && text[1] == u16::from(b':')) {
            // Nothing is left to query; CPython leaves the last error as it was.
            return Err(last_error());
        }
        let mut trimmed = text[..=end].to_vec();
        trimmed.push(0);
        return find_first_file(&trimmed);
    }
    find_first_file(path)
}

/// Returns whether a reparse tag names another file, as links and junctions do.
fn is_name_surrogate(tag: u32) -> bool {
    tag & 0x2000_0000 != 0
}

/// Returns the type of file that a file's attributes and reparse tag give.
fn mode(attributes: u32, reparse_tag: u32) -> StatMode {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && reparse_tag == IO_REPARSE_TAG_SYMLINK {
        StatMode::Link
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        StatMode::Directory
    } else {
        StatMode::Regular
    }
}

/// Returns `os.stat(path)`, or `os.lstat(path)` without `traverse`, as CPython's
/// `win32_xstat_slow_impl` computes it.
pub(crate) fn stat(path: &Path, traverse: bool) -> io::Result<Status> {
    stat_wide(&wide(path)?, traverse)
}

/// Implements [`stat`] for a NUL-terminated path.
fn stat_wide(path: &[u16], mut traverse: bool) -> io::Result<Status> {
    let mut unhandled_tag = false;
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if !traverse {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    let handle = match open(path, FILE_READ_ATTRIBUTES, 0, flags) {
        Ok(handle) => handle,
        Err(code @ (ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION)) => {
            // Read the attributes from the parent directory instead.
            let data = attributes_from_dir(path).map_err(|find_error| match find_error {
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_NOT_READY
                | ERROR_BAD_NET_NAME => error(find_error),
                _ => error(code),
            })?;
            if data.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                && (traverse || !is_name_surrogate(data.reparse_tag))
            {
                // The link would have to be followed, but cannot be.
                return Err(error(code));
            }
            return Ok(Status {
                mode: mode(data.attributes, data.reparse_tag),
                device: 0,
                inode: 0,
            });
        }
        Err(code @ ERROR_INVALID_PARAMETER) => {
            // Devices such as `\\.\con` need read or write access.
            open(
                path,
                FILE_READ_ATTRIBUTES | GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                flags,
            )
            .map_err(|_| error(code))?
        }
        Err(code @ ERROR_CANT_ACCESS_FILE) if traverse => {
            // Open a reparse point that Windows cannot follow as itself.
            traverse = false;
            unhandled_tag = true;
            open(
                path,
                FILE_READ_ATTRIBUTES,
                0,
                flags | FILE_FLAG_OPEN_REPARSE_POINT,
            )
            .map_err(|_| error(code))?
        }
        Err(code) => return Err(error(code)),
    };

    // SAFETY: the handle is open.
    let file_type = unsafe { GetFileType(raw(&handle)) };
    if file_type != FILE_TYPE_DISK {
        if file_type == FILE_TYPE_UNKNOWN && last_error() != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the path is NUL-terminated.
        let attributes = unsafe { GetFileAttributesW(path.as_ptr()) };
        let mode = if attributes != INVALID_FILE_ATTRIBUTES
            && attributes & FILE_ATTRIBUTE_DIRECTORY != 0
        {
            StatMode::Directory
        } else if file_type == FILE_TYPE_CHAR {
            StatMode::Character
        } else if file_type == FILE_TYPE_PIPE {
            StatMode::Fifo
        } else {
            StatMode::Unknown
        };
        return Ok(Status {
            mode,
            device: 0,
            inode: 0,
        });
    }

    let mut reparse_tag = 0;
    if !traverse {
        match information::<FILE_ATTRIBUTE_TAG_INFO>(&handle, FileAttributeTagInfo) {
            Ok(tag) if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 => {
                if is_name_surrogate(tag.ReparseTag) {
                    if unhandled_tag {
                        return Err(error(ERROR_CANT_ACCESS_FILE));
                    }
                } else if !unhandled_tag {
                    // Follow a reparse point that is not a link.
                    drop(handle);
                    return stat_wide(path, true);
                }
                reparse_tag = tag.ReparseTag;
            }
            Ok(_) => {}
            // Devices without reparse tags are ordinary files.
            Err(ERROR_INVALID_PARAMETER | ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED) => {}
            Err(code) => return Err(error(code)),
        }
    }

    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain data, for which all-zero bytes are valid.
    let mut file: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
    // SAFETY: the handle is open and the output pointer is valid.
    let queried = if unsafe { GetFileInformationByHandle(raw(&handle), &mut file) } == 0 {
        Err(last_error())
    } else {
        information::<FILE_BASIC_INFO>(&handle, FileBasicInfo).map(|_| ())
    };
    match queried {
        Ok(()) => {}
        // Volumes and physical disks are block devices.
        Err(ERROR_INVALID_PARAMETER | ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED) => {
            return Ok(Status {
                mode: StatMode::Block,
                device: 0,
                inode: 0,
            });
        }
        Err(code) => return Err(error(code)),
    }
    let id = information::<FILE_ID_INFO>(&handle, FileIdInfo).ok();
    let device = id.map_or(u64::from(file.dwVolumeSerialNumber), |id| {
        id.VolumeSerialNumber
    });
    let inode = match id.map_or(0, |id| u128::from_le_bytes(id.FileId.Identifier)) {
        0 => (u128::from(file.nFileIndexHigh) << 32) | u128::from(file.nFileIndexLow),
        inode => inode,
    };
    Ok(Status {
        mode: mode(file.dwFileAttributes, reparse_tag),
        device,
        inode,
    })
}

/// Returns `os.fstat` of an open file, as CPython's `_Py_fstat_noraise` computes it: a disk file's
/// type, identity, and link count come from `GetFileInformationByHandle` and `FILE_ID_INFO`, as
/// [`stat`] reads them, and its attributes tell whether the handle is a reparse point that was
/// opened as itself.
#[cfg(feature = "cli")]
pub(crate) fn fstat(file: &impl AsRawHandle) -> io::Result<HandleStatus> {
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: the handle is open for as long as `file` is borrowed.
    let file_type = unsafe { GetFileType(handle) };
    if file_type != FILE_TYPE_DISK {
        if file_type == FILE_TYPE_UNKNOWN && last_error() != 0 {
            return Err(io::Error::last_os_error());
        }
        let mode = match file_type {
            FILE_TYPE_CHAR => StatMode::Character,
            FILE_TYPE_PIPE => StatMode::Fifo,
            _ => StatMode::Unknown,
        };
        return Ok(HandleStatus {
            mode,
            device: 0,
            inode: 0,
            links: 0,
            reparse_point: false,
        });
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
    let device = if has_id {
        id.VolumeSerialNumber
    } else {
        u64::from(information.dwVolumeSerialNumber)
    };
    let inode = match has_id.then(|| u128::from_le_bytes(id.FileId.Identifier)) {
        Some(inode) if inode != 0 => inode,
        _ => (u128::from(information.nFileIndexHigh) << 32) | u128::from(information.nFileIndexLow),
    };
    Ok(HandleStatus {
        mode: mode(information.dwFileAttributes, 0),
        device,
        inode,
        links: information.nNumberOfLinks,
        reparse_point: information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
    })
}

/// The fields of `os.fstat` that NVX reads.
#[cfg(feature = "cli")]
#[derive(Clone, Copy, Debug)]
pub(crate) struct HandleStatus {
    /// The type of file. `os.fstat` reads no reparse tag, so a link opened as itself is no
    /// [`StatMode::Link`].
    pub(crate) mode: StatMode,
    /// `st_dev`: the volume serial number.
    pub(crate) device: u64,
    /// `st_ino`: the file ID.
    pub(crate) inode: u128,
    /// `st_nlink`: the number of names that link the file.
    pub(crate) links: u32,
    /// Whether `st_file_attributes` has `FILE_ATTRIBUTE_REPARSE_POINT`.
    pub(crate) reparse_point: bool,
}

/// Returns `nt._getfinalpathname(path)`: the path that Windows gives for the file, with its
/// `\\?\` prefix.
pub(crate) fn final_path_name(path: &Path) -> io::Result<String> {
    let path = wide(path)?;
    let handle = open(&path, 0, 0, FILE_FLAG_BACKUP_SEMANTICS).map_err(error)?;
    let mut buffer = vec![0u16; 260];
    loop {
        // SAFETY: the handle is open and the buffer holds `buffer.len()` units.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                raw(&handle),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                VOLUME_NAME_DOS,
            )
        } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length < buffer.len() {
            buffer.truncate(length);
            break;
        }
        buffer.resize(length, 0);
    }
    String::from_utf16(&buffer).map_err(|_| super::not_unicode("a final path name"))
}

/// Returns `nt._findfirstfile(path)`: the name of the file as its directory holds it.
pub(crate) fn find_first_file_name(path: &Path) -> io::Result<String> {
    let data = find_first_file(&wide(path)?).map_err(error)?;
    String::from_utf16(&data.name).map_err(|_| super::not_unicode("a file name"))
}

/// Returns `nt.readlink(path)`: the substitute name of a symbolic link or junction, with `\??\`
/// turned into `\\?\`, or `None` for another kind of reparse point.
pub(crate) fn read_link(path: &Path) -> io::Result<Option<String>> {
    let path = wide(path)?;
    let handle = open(
        &path,
        0,
        0,
        FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
    )
    .map_err(error)?;
    let mut buffer = vec![0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE];
    let mut returned = 0u32;
    // SAFETY: the handle is open, there is no input buffer, and the output buffer holds
    // `buffer.len()` bytes.
    let ok = unsafe {
        DeviceIoControl(
            raw(&handle),
            FSCTL_GET_REPARSE_POINT,
            ptr::null(),
            0,
            buffer.as_mut_ptr().cast::<c_void>(),
            buffer.len() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let data = &buffer[..(returned as usize).min(buffer.len())];
    let u16_at = |offset: usize| -> Option<usize> {
        Some(usize::from(u16::from_le_bytes(
            data.get(offset..offset + 2)?.try_into().ok()?,
        )))
    };
    let tag = data.get(..4).map_or(0, |bytes| {
        u32::from_le_bytes(bytes.try_into().unwrap_or_default())
    });
    // The substitute name follows the fixed fields of REPARSE_DATA_BUFFER: a symbolic link has
    // flags before its path buffer, and a junction does not.
    let path_buffer = match tag {
        IO_REPARSE_TAG_SYMLINK => 20,
        IO_REPARSE_TAG_MOUNT_POINT => 16,
        _ => return Ok(None),
    };
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "malformed reparse point");
    let (offset, length) = (
        u16_at(8).ok_or_else(malformed)?,
        u16_at(10).ok_or_else(malformed)?,
    );
    let start = path_buffer + offset;
    let bytes = data.get(start..start + length).ok_or_else(malformed)?;
    let mut name: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let nt_prefix = ['\\', '?', '?', '\\'].map(|c| c as u16);
    if name.len() > 4 && name[..4] == nt_prefix {
        name[1] = u16::from(b'\\');
    }
    String::from_utf16(&name)
        .map(Some)
        .map_err(|_| super::not_unicode("a symbolic link's target"))
}
