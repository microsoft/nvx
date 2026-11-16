//! Bounded account databases below an already-open root, independent of execution entry.

use std::fmt;
use std::io::{self, Read};

use super::parse::parse_decimal;
use super::sys::{self, Errno, FileDescriptor, Mode, OpenFlags};

/// The largest account database, `/etc/passwd` or `/etc/group`, that existing-account mode reads.
pub(crate) const MAX_ACCOUNT_DATABASE_BYTES: u64 = 1 << 20;
/// The path of the passwd database below the root.
const PASSWD: &std::ffi::CStr = c"etc/passwd";
/// The path of the group database below the root.
const GROUP: &std::ffi::CStr = c"etc/group";

/// The account of an identity, as its passwd entry names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Account {
    /// The user name.
    name: Vec<u8>,
    /// The home directory.
    home: Vec<u8>,
}

impl Account {
    /// Returns the user name, as bytes, since passwd entries need not be UTF-8.
    pub(crate) fn name(&self) -> &[u8] {
        &self.name
    }

    /// Returns the home directory, an absolute path.
    pub(crate) fn home(&self) -> &[u8] {
        &self.home
    }
}

/// An account database of existing-account mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Database {
    /// `/etc/passwd`.
    Passwd,
    /// `/etc/group`.
    Group,
}

impl fmt::Display for Database {
    /// Writes the database's path in the root.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Passwd => "/etc/passwd",
            Self::Group => "/etc/group",
        })
    }
}

/// Why existing-account mode refused an identity.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum AccountError {
    /// The database cannot be opened or read below the root, which a symbolic link on its path
    /// fails too.
    Read(Database, io::Error),
    /// The database is not a regular file.
    NotFile(Database),
    /// The database is larger than [`MAX_ACCOUNT_DATABASE_BYTES`].
    TooLarge(Database),
    /// No passwd entry has the user ID.
    Missing,
    /// Several passwd entries have the user ID.
    Duplicate,
    /// The passwd entry of the user ID lacks a field, or a name.
    Malformed,
    /// The passwd entry's primary group, this group ID, is not the identity's.
    PrimaryGroup(u32),
    /// No group entry has the group ID.
    Group,
    /// The passwd entry's home is not absolute.
    Home,
}

impl fmt::Display for AccountError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(database, error) => write!(formatter, "cannot read {database}: {error}"),
            Self::NotFile(database) => write!(formatter, "{database} is not a regular file"),
            Self::TooLarge(database) => write!(
                formatter,
                "{database} is larger than {MAX_ACCOUNT_DATABASE_BYTES} bytes"
            ),
            Self::Missing => formatter.write_str("no passwd entry has the user ID"),
            Self::Duplicate => formatter.write_str("several passwd entries have the user ID"),
            Self::Malformed => formatter.write_str("the passwd entry of the user ID is malformed"),
            Self::PrimaryGroup(gid) => write!(
                formatter,
                "the passwd entry's primary group is {gid}, not the identity's"
            ),
            Self::Group => formatter.write_str("no group entry has the group ID"),
            Self::Home => formatter.write_str("the passwd entry's home is not absolute"),
        }
    }
}

impl std::error::Error for AccountError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(_, error) => Some(error),
            _ => None,
        }
    }
}

/// Reads the account databases below `root` and checks the account of `uid` and `gid`.
pub(crate) fn existing_account(
    root: &FileDescriptor,
    uid: u32,
    gid: u32,
) -> Result<Account, AccountError> {
    let passwd = read_database(root, Database::Passwd)?;
    let account = passwd_account(&passwd, uid, gid)?;
    let group = read_database(root, Database::Group)?;
    if !has_group(&group, gid) {
        return Err(AccountError::Group);
    }
    Ok(account)
}

/// Reads `database` below `root`, through no symbolic link, if it is a regular file of at most
/// [`MAX_ACCOUNT_DATABASE_BYTES`].
pub(crate) fn read_database(
    root: &FileDescriptor,
    database: Database,
) -> Result<Vec<u8>, AccountError> {
    let path = match database {
        Database::Passwd => PASSWD,
        Database::Group => GROUP,
    };
    let read = |error: Errno| AccountError::Read(database, error.into());
    // A FIFO must not block the open, which the file's kind then refuses.
    let file = sys::open_beneath(
        root,
        path,
        OpenFlags::O_RDONLY | OpenFlags::O_NONBLOCK | OpenFlags::O_NOCTTY,
        Mode::empty(),
        true,
    )
    .map_err(read)?;
    let (regular, size) = sys::file_size(&file).map_err(read)?;
    if !regular {
        return Err(AccountError::NotFile(database));
    }
    if size > MAX_ACCOUNT_DATABASE_BYTES {
        return Err(AccountError::TooLarge(database));
    }
    let mut bytes = Vec::new();
    file.take(MAX_ACCOUNT_DATABASE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AccountError::Read(database, error))?;
    if bytes.len() as u64 > MAX_ACCOUNT_DATABASE_BYTES {
        return Err(AccountError::TooLarge(database));
    }
    Ok(bytes)
}

/// Returns the fields of each entry of an account database, with the ID in its third field: each
/// line that names one. As the C library's files database does, it skips blank lines and
/// comments, whose first character after blanks is `#`.
pub(crate) fn entries(database: &[u8]) -> impl Iterator<Item = (u64, Vec<&[u8]>)> {
    database
        .split(|&byte| byte == b'\n')
        .filter(|line| !matches!(line.trim_ascii_start().first(), None | Some(b'#')))
        .filter_map(|line| {
            let fields: Vec<&[u8]> = line.split(|&byte| byte == b':').collect();
            let id = parse_decimal(fields.get(2)?)?;
            Some((id, fields))
        })
}

/// Finds the one passwd entry of `uid` in `passwd`, and checks that its primary group is `gid`
/// and its home absolute.
fn passwd_account(passwd: &[u8], uid: u32, gid: u32) -> Result<Account, AccountError> {
    let mut matching = entries(passwd).filter(|(id, _)| *id == u64::from(uid));
    let (_, fields) = matching.next().ok_or(AccountError::Missing)?;
    if matching.next().is_some() {
        return Err(AccountError::Duplicate);
    }
    let [name, _, _, primary, _, home, _] = fields[..] else {
        return Err(AccountError::Malformed);
    };
    if name.is_empty() {
        return Err(AccountError::Malformed);
    }
    let primary = parse_decimal(primary)
        .and_then(|primary| u32::try_from(primary).ok())
        .ok_or(AccountError::Malformed)?;
    if primary != gid {
        return Err(AccountError::PrimaryGroup(primary));
    }
    if home.first() != Some(&b'/') {
        return Err(AccountError::Home);
    }
    Ok(Account {
        name: name.to_vec(),
        home: home.to_vec(),
    })
}

/// Returns whether `group` holds an entry of `gid`.
pub(crate) fn has_group(group: &[u8], gid: u32) -> bool {
    entries(group).any(|(id, _)| id == u64::from(gid))
}

#[cfg(test)]
pub(crate) mod tests;
