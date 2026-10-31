//! Directories and files that only the current user may access: their creation, the checks of
//! their owners and access control lists, and the sparse files and the renames without
//! replacement of the stores.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GENERIC_ALL, GENERIC_WRITE, HANDLE};
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
    CreateDirectoryW, DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA,
    MoveFileExW, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::identity::handle_information;
use super::{last_error, owned, raw, wide};

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
pub(super) struct CurrentUser {
    /// The token's `TOKEN_USER`, followed by the security identifier that it points to, in a
    /// buffer aligned for both.
    token_user: Vec<u64>,
}

impl CurrentUser {
    /// Reads the user from the token of this process.
    pub(super) fn get() -> io::Result<Self> {
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
pub(super) fn with_owner_only_attributes<T>(
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
pub(super) enum Required {
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
pub(super) fn privacy_problem(
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
