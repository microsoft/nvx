//! The workload's account in the root's user and group databases, which the layered root's PID 1
//! looks up with the checks and messages of NVX's shell init.

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// The root's user database.
const PASSWD: &[u8] = b"/etc/passwd";
/// The root's group database.
const GROUP: &[u8] = b"/etc/group";

/// The workload's account: the name and home directory of its user ID's record.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Account {
    /// The user name.
    pub(super) name: Vec<u8>,
    /// The home directory, an absolute path in the root.
    pub(super) home: Vec<u8>,
}

/// Looks up the account of the user ID `uid` and the group ID `gid`, nonzero decimal texts, in
/// the root `rootfs`, or returns the shell init's message for the first check that fails:
///
/// - each database is a regular file, not a symbolic link;
/// - the user database has exactly one record of the user ID, and its name is not empty;
/// - the record's user and primary group IDs are the texts `uid` and `gid`;
/// - the group database has a record of the group ID;
/// - and the home directory is absolute, and a directory in the root that is not a symbolic link.
///
/// A record's ID matches when it is the same decimal number, leading zeros aside, as the shell
/// init's `awk` compared the decimal IDs of a database. The databases are read from outside the
/// root, as the shell init read them.
pub(super) fn lookup(rootfs: &Path, uid: &[u8], gid: &[u8]) -> Result<Account, &'static str> {
    let passwd = in_root(rootfs, PASSWD);
    let group = in_root(rootfs, GROUP);
    if !is_regular_file(&passwd) {
        return Err("workload user database is unavailable");
    }
    if !is_regular_file(&group) {
        return Err("workload group database is unavailable");
    }
    let users = fs::read(&passwd).map_err(|_| "configured workload UID is unavailable")?;
    let mut records = users
        .split(|&byte| byte == b'\n')
        .filter(|record| same_id(field(record, 2), uid));
    let (Some(record), None) = (records.next(), records.next()) else {
        return Err("configured workload UID is unavailable");
    };
    let name = field(record, 0);
    if name.is_empty() {
        return Err("configured workload user record is invalid");
    }
    if field(record, 2) != uid || field(record, 3) != gid {
        return Err("configured workload identity does not match the user database");
    }
    let groups = fs::read(&group).map_err(|_| "configured workload GID is unavailable")?;
    if !groups
        .split(|&byte| byte == b'\n')
        .any(|record| same_id(field(record, 2), gid))
    {
        return Err("configured workload GID is unavailable");
    }
    let home = field(record, 5);
    if !home.starts_with(b"/") {
        return Err("configured workload home is invalid");
    }
    if !fs::symlink_metadata(in_root(rootfs, home)).is_ok_and(|metadata| metadata.is_dir()) {
        return Err("configured workload home is unavailable");
    }
    Ok(Account {
        name: name.to_vec(),
        home: home.to_vec(),
    })
}

/// Returns the path of the absolute path `path` in the root `rootfs`, as the shell init joined
/// them: `rootfs`, then `path`.
fn in_root(rootfs: &Path, path: &[u8]) -> PathBuf {
    let mut joined = rootfs.as_os_str().as_bytes().to_vec();
    joined.extend_from_slice(path);
    PathBuf::from(OsString::from_vec(joined))
}

/// Returns whether `path` is a regular file, and not a symbolic link to one.
fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Returns the field `index`, from 0, of a database record, whose fields colons separate, or an
/// empty field when the record has fewer.
fn field(record: &[u8], index: usize) -> &[u8] {
    record
        .split(|&byte| byte == b':')
        .nth(index)
        .unwrap_or_default()
}

/// Returns whether `field` and `id` are the same decimal number.
fn same_id(field: &[u8], id: &[u8]) -> bool {
    /// Returns the digits of a decimal number after its leading zeros, or `None` for another
    /// text.
    fn significant(digits: &[u8]) -> Option<&[u8]> {
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let start = digits
            .iter()
            .position(|&digit| digit != b'0')
            .unwrap_or(digits.len());
        Some(&digits[start..])
    }
    matches!((significant(field), significant(id)), (Some(field), Some(id)) if field == id)
}
