//! Seals of host files: what the host reports about an open file, read from its metadata alone.
//!
//! A [`FileSeal`] records, for one open file, the platform scheme that took it, the device or
//! volume and the file ID that identify the file, its kind, its length, its modification time,
//! and, on Linux, its change time. A caller that records a seal can later open the same path,
//! seal the file again, and compare the seals, to tell without reading the file whether the path
//! still names the object that it sealed, and whether that object changed.
//!
//! A seal detects accidental change: a file that was replaced, rewritten, truncated, or touched.
//! It is no defense against deliberate tampering by an account that can write the file, which can
//! rewrite the file within the resolution of its timestamps, and set its modification time back.
//! Callers keep the files that they seal read-only, in a directory that only their own account
//! can write, and never rewrite them in place.
//!
//! - On Linux, a seal holds `st_dev`, `st_ino`, the file type of `st_mode`, `st_size`, and
//!   `st_mtime` and `st_ctime` with their nanoseconds. Linux sets the change time itself whenever
//!   the file or its metadata changes, as a change of its mode, owner, or links does, and no call
//!   sets it back, so callers finish staging a file, its mode included, before they seal it.
//! - On Windows, it holds the volume serial number and the 128-bit file ID of `FILE_ID_INFO`, or
//!   those of `BY_HANDLE_FILE_INFORMATION` on a volume that has no file ID information, the kind
//!   that the file's attributes give, its length, and its last write time.
//!
//! Other hosts cannot seal files.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows;

/// The version of the seals that this build takes. Seals of different versions never compare
/// equal.
pub const SEAL_VERSION: u32 = 1;

/// The platform scheme of a seal: the host that took it, which fixes what its fields mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SealScheme {
    /// Linux: the device and inode, with the change time.
    Linux,
    /// Windows: the volume serial number and the file ID, without a change time.
    Windows,
}

/// The kind of a sealed file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    /// A regular file.
    Regular,
    /// A directory.
    Directory,
    /// A block device.
    Block,
    /// A character device.
    Character,
    /// A FIFO.
    Fifo,
    /// A socket.
    Socket,
    /// Any other kind.
    Other,
}

/// A timestamp of a sealed file: whole seconds since the Unix epoch, negative before it, and the
/// nanoseconds within the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SealTime {
    /// Whole seconds since 1970-01-01 00:00:00 UTC.
    pub seconds: i64,
    /// Nanoseconds within the second, below 1,000,000,000.
    pub nanoseconds: u32,
}

/// The seal of an open file: what the host reported about it, from its metadata alone.
///
/// [`of`](Self::of) takes a seal, and two seals are equal when they are of the same version and
/// scheme and none of their fields differ. Their serde form, which callers record, holds every
/// field, each required:
///
/// ```json
/// {"version": 1, "scheme": "linux", "device": 2049, "fileId": "0000000000000000000000000004a2f1",
///  "kind": "regular", "length": 4096, "modified": {"seconds": 1792000000, "nanoseconds": 5},
///  "changed": {"seconds": 1792000000, "nanoseconds": 5}}
/// ```
///
/// `fileId` holds 32 hexadecimal digits, and `changed` is `null` on Windows.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileSeal {
    /// [`SEAL_VERSION`].
    version: u32,
    /// The host that took the seal.
    scheme: SealScheme,
    /// `st_dev`, or the volume serial number on Windows.
    device: u64,
    /// `st_ino`, or the file ID on Windows.
    file_id: FileId,
    /// The kind of file.
    kind: FileKind,
    /// The length in bytes.
    length: u64,
    /// The modification time.
    modified: SealTime,
    /// The change time on Linux, and `None` on Windows. The form requires it, as `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    changed: Option<SealTime>,
}

impl FileSeal {
    /// Seals `file`, open, from its metadata as the host reports it now.
    ///
    /// It never reads or maps the file's content, so it also seals a file that was opened without
    /// read access: on Linux with `O_PATH`, and on Windows with `FILE_READ_ATTRIBUTES` alone.
    /// Fails with the host's error, and with [`io::ErrorKind::Unsupported`] on hosts other than
    /// Linux and Windows, and on Windows for a handle that is not a file of a disk volume, such as
    /// a pipe.
    pub fn of(file: &File) -> io::Result<Self> {
        imp::seal(file)
    }

    /// Returns the host that took the seal.
    pub fn scheme(&self) -> SealScheme {
        self.scheme
    }

    /// Returns the device, or on Windows the volume serial number.
    pub fn device(&self) -> u64 {
        self.device
    }

    /// Returns the inode, or on Windows the file ID.
    pub fn file_id(&self) -> u128 {
        self.file_id.0
    }

    /// Returns the kind of file.
    pub fn kind(&self) -> FileKind {
        self.kind
    }

    /// Returns the length in bytes.
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Returns the modification time.
    pub fn modified(&self) -> SealTime {
        self.modified
    }

    /// Returns the change time on Linux, and `None` on Windows.
    pub fn changed(&self) -> Option<SealTime> {
        self.changed
    }

    /// Returns what differs first between this seal and `other`, in the order version, scheme,
    /// identity, kind, length, modification time, and change time, or `None` if they are equal.
    pub(crate) fn difference(&self, other: &Self) -> Option<&'static str> {
        if self.version != other.version {
            Some("seal version")
        } else if self.scheme != other.scheme {
            Some("seal scheme")
        } else if (self.device, self.file_id) != (other.device, other.file_id) {
            Some("identity")
        } else if self.kind != other.kind {
            Some("kind")
        } else if self.length != other.length {
            Some("length")
        } else if self.modified != other.modified {
            Some("modification time")
        } else if self.changed != other.changed {
            Some("change time")
        } else {
            None
        }
    }
}

/// A file ID, which the serde form of a seal writes as 32 lowercase hexadecimal digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileId(pub(crate) u128);

impl fmt::Display for FileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:032x}", self.0)
    }
}

impl Serialize for FileId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for FileId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let digits = String::deserialize(deserializer)?;
        if digits.len() != 32 || !digits.bytes().all(|digit| digit.is_ascii_hexdigit()) {
            return Err(serde::de::Error::custom(
                "a file ID must be 32 hexadecimal digits",
            ));
        }
        u128::from_str_radix(&digits, 16)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// Opens `path` for reading, following links, as a caller holds a file that it checked.
///
/// On Linux, the open does not wait for a writer of a FIFO, which the seal then shows. On
/// Windows, it also opens a directory, and denies other opens for writing or deleting the file
/// until the file is closed, so that nothing modifies, renames, or replaces the file meanwhile;
/// opens that only read it, such as OpenVMM's open of a read-only disk, share it.
pub(crate) fn open_guarded(path: &Path) -> io::Result<File> {
    imp::open_guarded(path)
}

/// Opens `path` for reading, following links, as verification reads a file: as
/// [`File::open`] does, except that on Linux the open does not wait for a writer of a FIFO.
pub(crate) fn open_read(path: &Path) -> io::Result<File> {
    imp::open_read(path)
}

/// Returns the final path of the open `file`, as `GetFinalPathNameByHandleW` gives it: the
/// absolute path, in its `\\?\` form, that names the file with every link and junction on the way
/// to it resolved.
#[cfg(windows)]
pub(crate) fn final_path(file: &File) -> io::Result<std::path::PathBuf> {
    imp::final_path(file)
}

/// Returns the final path of the open `file` as [`final_path`] does, but with its volume named by
/// its GUID, `\\?\Volume{GUID}\`, which names a volume that has no drive letter or mount point,
/// and so no path by a drive, too.
#[cfg(windows)]
pub(crate) fn final_volume_path(file: &File) -> io::Result<std::path::PathBuf> {
    imp::final_volume_path(file)
}

/// Opens `path` for its metadata alone, so that no read or mapping of its content can succeed
/// through the file: with `O_PATH` on Linux, and on Windows with `FILE_READ_ATTRIBUTES` alone,
/// sharing every access.
#[cfg(all(test, any(target_os = "linux", windows)))]
pub(crate) fn open_metadata_only(path: &Path) -> io::Result<File> {
    imp::open_metadata_only(path)
}

#[cfg(target_os = "linux")]
/// The Linux seal, from `statx` or `fstat` of the open file.
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
    use std::path::Path;

    use nix::fcntl::OFlag;

    use super::{FileId, FileKind, FileSeal, SEAL_VERSION, SealScheme, SealTime};

    /// See [`FileSeal::of`].
    pub(super) fn seal(file: &File) -> io::Result<FileSeal> {
        let metadata = file.metadata()?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_file() {
            FileKind::Regular
        } else if file_type.is_dir() {
            FileKind::Directory
        } else if file_type.is_block_device() {
            FileKind::Block
        } else if file_type.is_char_device() {
            FileKind::Character
        } else if file_type.is_fifo() {
            FileKind::Fifo
        } else if file_type.is_socket() {
            FileKind::Socket
        } else {
            FileKind::Other
        };
        Ok(FileSeal {
            version: SEAL_VERSION,
            scheme: SealScheme::Linux,
            device: metadata.dev(),
            file_id: FileId(u128::from(metadata.ino())),
            kind,
            length: metadata.size(),
            modified: time(metadata.mtime(), metadata.mtime_nsec())?,
            changed: Some(time(metadata.ctime(), metadata.ctime_nsec())?),
        })
    }

    /// Returns the time of `seconds` and `nanoseconds`, as the kernel reports a file's time.
    fn time(seconds: i64, nanoseconds: i64) -> io::Result<SealTime> {
        let nanoseconds = u32::try_from(nanoseconds)
            .ok()
            .filter(|&nanoseconds| nanoseconds < 1_000_000_000)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the host reported an invalid time",
                )
            })?;
        Ok(SealTime {
            seconds,
            nanoseconds,
        })
    }

    /// See [`super::open_guarded`].
    pub(super) fn open_guarded(path: &Path) -> io::Result<File> {
        open_read(path)
    }

    /// See [`super::open_read`]. A regular file ignores `O_NONBLOCK`.
    pub(super) fn open_read(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(OFlag::O_NONBLOCK.bits())
            .open(path)
    }

    /// See [`super::open_metadata_only`]. Linux fails every read and mapping of an `O_PATH`
    /// descriptor with `EBADF`.
    #[cfg(test)]
    pub(super) fn open_metadata_only(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(OFlag::O_PATH.bits())
            .open(path)
    }
}

#[cfg(windows)]
use windows as imp;

#[cfg(not(any(target_os = "linux", windows)))]
/// Hosts other than Linux and Windows, which cannot seal files.
mod imp {
    use std::fs::File;
    use std::io;
    use std::path::Path;

    use super::FileSeal;

    /// See [`FileSeal::of`].
    pub(super) fn seal(_file: &File) -> io::Result<FileSeal> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this host cannot seal files",
        ))
    }

    /// See [`super::open_guarded`].
    pub(super) fn open_guarded(path: &Path) -> io::Result<File> {
        File::open(path)
    }

    /// See [`super::open_read`].
    pub(super) fn open_read(path: &Path) -> io::Result<File> {
        File::open(path)
    }
}

#[cfg(all(test, any(target_os = "linux", windows)))]
mod tests;
