//! Resolve the shared root's account once, before any workload can run.

use std::ffi::{CStr, CString};
use std::path::Path;

use crate::guest::account::{self, AccountError, Database};
use crate::guest::sys::{self, Errno, FileDescriptor, Mode, OpenFlags};
use crate::{Error, Result};

pub(super) fn resolve(
    path: &Path,
    (uid, gid): (u32, u32),
    create: bool,
) -> Result<(CString, CString)> {
    let root = sys::open(
        &sys::path_c_string(path).map_err(failed)?,
        OpenFlags::O_PATH | OpenFlags::O_DIRECTORY | OpenFlags::O_NOFOLLOW | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(failed)?;
    if create {
        drop(account::read_database(&root, Database::Passwd).map_err(database_error)?);
        let mut group = account::read_database(&root, Database::Group).map_err(database_error)?;
        if !account::has_group(&group, gid) {
            newline(&mut group);
            group.extend_from_slice(format!("nvx:x:{gid}:\n").as_bytes());
            write(&root, c"group", &group).map_err(failed)?;
        }
    }
    match account::existing_account(&root, uid, gid) {
        Ok(account) => match home(&root, account.home()) {
            Ok(()) => return strings(&account),
            Err(error)
                if create && matches!(error, Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP) => {}
            Err(error) => return Err(failed(error)),
        },
        Err(
            error @ (AccountError::Read(..) | AccountError::NotFile(_) | AccountError::TooLarge(_)),
        ) => {
            return Err(Error::io(error.to_string()).with_source(error));
        }
        Err(error) if !create => {
            return Err(Error::unsupported(error.to_string()).with_source(error));
        }
        Err(
            AccountError::Missing
            | AccountError::Duplicate
            | AccountError::Malformed
            | AccountError::PrimaryGroup(_)
            | AccountError::Group
            | AccountError::Home,
        ) => {}
    }
    let passwd = account::read_database(&root, Database::Passwd).map_err(database_error)?;
    let mut kept = Vec::new();
    for line in passwd.split_inclusive(|&byte| byte == b'\n') {
        if !account::entries(line).any(|(id, _)| id == u64::from(uid)) {
            kept.extend_from_slice(line);
        }
    }
    newline(&mut kept);
    kept.extend_from_slice(
        format!("nvx:x:{uid}:{gid}:NVX workload:/tmp:/sbin/nologin\n").as_bytes(),
    );
    let mode = Mode::from_bits_retain(0o1777);
    match sys::create_directory_at(&root, c"tmp", mode) {
        Ok(()) => sys::open_beneath(
            &root,
            c"tmp",
            OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY,
            Mode::empty(),
            false,
        )
        .and_then(|directory| directory.set_mode(mode))
        .map_err(failed)?,
        Err(Errno::EEXIST) => home(&root, b"/tmp").map_err(failed)?,
        Err(error) => return Err(failed(error)),
    }
    write(&root, c"passwd", &kept).map_err(failed)?;
    strings(&account::existing_account(&root, uid, gid).map_err(database_error)?)
}

fn home(root: &FileDescriptor, home: &[u8]) -> std::result::Result<(), Errno> {
    root.open_directory_in_root(&sys::c_string(home)?).map(drop)
}

fn strings(account: &account::Account) -> Result<(CString, CString)> {
    Ok((
        CString::new(account.name()).map_err(failed)?,
        CString::new(account.home()).map_err(failed)?,
    ))
}

fn newline(bytes: &mut Vec<u8>) {
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
}

fn write(root: &FileDescriptor, name: &CStr, bytes: &[u8]) -> std::result::Result<(), Errno> {
    if bytes.len() as u64 > account::MAX_ACCOUNT_DATABASE_BYTES {
        return Err(Errno::EFBIG);
    }
    sys::open_beneath(
        root,
        c"etc",
        OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY,
        Mode::empty(),
        false,
    )?
    .replace_file(name, bytes)
}

fn database_error(error: AccountError) -> Error {
    Error::io(error.to_string()).with_source(error)
}

fn failed(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::io(format!("failed to prepare the workload account: {error}")).with_source(error)
}
