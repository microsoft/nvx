//! The identities of files and directories, whether a file is deleted, and the open directories
//! through which operations name their entries.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::Path;
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
};

use super::{owned, raw};

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
pub(super) fn status_error(status: windows_sys::Win32::Foundation::NTSTATUS) -> io::Error {
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
pub(super) fn handle_information(handle: HANDLE) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain data for which all-zero bytes are valid.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
    // SAFETY: the caller passes an open handle, and the output pointer is valid.
    if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(information)
}
