//! The binding of a sandbox's scratch image: what identifies the image, recorded so that a launch
//! checks it from its metadata alone, and claims it for one sandbox.
//!
//! The scratch image holds the writable upper of a layered root, a whole-device ext4 filesystem
//! that the VMM writes. Unlike a distro layer's, its content and times change while a sandbox
//! runs, so a [`ScratchBinding`] records neither, only what tells a replacement apart and what
//! the launch relies on: the platform scheme, the device or volume and the file ID of the image,
//! its kind, a regular file, and its length, which fixes the size of the guest's block device.
//!
//! [`ScratchBinding::bind`] records the image where a caller stages or provisions it, and
//! [`ScratchBinding::check`] checks it before a launch. Neither reads or maps the image's content.
//! The check opens the image, compares it with the binding, claims it, compares it again, since
//! the launch whose claim it waited for may have changed it, and returns it open, as a
//! [`CheckedScratch`], which a [`PreparedLaunch`](super::PreparedLaunch) hands to the VMM: a
//! replacement, or a change of kind or length, fails the check. On Linux, the file ID is the inode
//! number, which a file that is created once the image is removed can take again: a replacement
//! made that way, with the image's length, passes the check.
//!
//! # Claims
//!
//! A scratch image belongs to one sandbox at a time: a check claims it for its launch, and the
//! claim lasts as long as that launch's VMM holds the image, so a second sandbox cannot claim it
//! meanwhile. A check that finds the image claimed waits until its deadline for the claim to be
//! released, then fails with [`ErrorKind::TimedOut`](crate::ErrorKind::TimedOut) and the code
//! [`ErrorCode::ScratchClaimed`].
//!
//! - On Linux, the check opens the image for reading and writing and takes an exclusive `flock`
//!   lock of that open file description. The VMM inherits the descriptor, which it opens again as
//!   `/proc/self/fd/N`, and keeps it until it exits, so the claim lasts until both the caller and
//!   the VMM have closed it. A process that another thread of the caller spawns while the
//!   descriptor is open holds a copy of it from its fork until it executes its program, and with
//!   it the claim, for that moment: a check therefore waits for a claim that should be free, for
//!   [`CLAIM_RELEASE_WAIT`] for example, rather than fail at once.
//! - On Windows, the check's handle is a guard that holds no access to the image's content: it
//!   shares reading and writing, which OpenVMM's open of the image needs, and denies deleting and
//!   renaming it, so that the image's path names the checked image until the guard is released.
//!   Two guards exclude each other, since each asks for the delete access that the other denies.
//!   The check then makes sure that no other process has the image open for writing, as the VMM
//!   of another sandbox does: an open that denies writing succeeds only then. It then opens the
//!   image for writing, sharing every access, as the claim's handle. The VMM inherits the guard
//!   and the claim's handle, and uses neither: so it holds the claim, and its copy of the guard the
//!   image's path, from its start, even if the caller ends before the VMM has opened the image,
//!   which closes the caller's own. Once the VMM has opened the image, at authenticated Ready, the
//!   caller releases the guard, its own and the VMM's copy, through
//!   [`PreparedLaunch::release`](super::PreparedLaunch::release), and the VMM's handles, open for
//!   writing, keep the claim.
//!
//! Claims exclude the launches of NVX: a process that writes the image without claiming it, which
//! on Linux takes no `flock` lock and on Windows does not keep the image open, is not kept out.
//! As for a distro layer, callers stage scratch images in a directory that only their own account
//! can write.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::geometry::BlockGeometry;
use super::mount::os_error;
use super::{block_geometry, refused};
use crate::error::{Error, ErrorCode, Result};
use crate::host::paths;
use crate::host::seal::{FileId, FileKind, FileSeal, SealScheme};

/// The version of the serde form of [`ScratchBinding`] that this build writes and checks.
pub const SCRATCH_BINDING_VERSION: u32 = 1;

/// How long a check of a scratch image that should be free waits for its claim to be released:
/// long enough for a process that another thread of the caller spawned to execute its program,
/// and so to close its copy of a claim, as [Claims](self#claims) explains.
pub const CLAIM_RELEASE_WAIT: Duration = Duration::from_secs(1);

/// How often a check tries a claim that is held again.
const CLAIM_RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// How messages name a scratch image.
const DESCRIPTION: &str = "ext4 scratch image";

/// How the operations of a binding open the scratch image: for reading and writing, as this
/// module opens it, or, in tests, for writing alone, so that no read or mapping of its content
/// can succeed.
#[derive(Clone, Copy)]
pub(crate) struct Access<'a> {
    /// Opens the image for writing, to bind it, and on hosts other than Windows to check it.
    pub(crate) open: &'a dyn Fn(&Path) -> io::Result<File>,
}

impl Access<'static> {
    /// The access of binds and checks: [`imp::open_writable`].
    pub(crate) const WRITABLE: Self = Self {
        open: &imp::open_writable,
    };
}

/// The binding of a scratch image: its platform scheme, device or volume, file ID, kind, and
/// length.
///
/// [`bind`](Self::bind) makes a binding, and [`check`](Self::check) checks the image against it,
/// and claims it, before a launch. Its serde form, which callers record, holds every field, each
/// required:
///
/// ```json
/// {"version": 1, "scheme": "linux", "device": 2049, "fileId": "0000000000000000000000000004a2f1",
///  "kind": "regular", "length": 67108864}
/// ```
///
/// with the fields of a [`FileSeal`] of the same names, and no times, which the VMM changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScratchBinding {
    /// [`SCRATCH_BINDING_VERSION`].
    version: u32,
    /// The host that took the binding.
    scheme: SealScheme,
    /// `st_dev`, or the volume serial number on Windows.
    device: u64,
    /// `st_ino`, or the file ID on Windows.
    file_id: FileId,
    /// The kind of file: a regular file.
    kind: FileKind,
    /// The length in bytes.
    length: u64,
}

/// A scratch image that [`ScratchBinding::check`] found unchanged and claimed, held open.
///
/// On Linux, the file is the image, open for reading and writing, whose open file description
/// holds the claim, and which the VMM inherits. On Windows, it is the image's guard, which holds
/// no access to its content and denies deleting and renaming it, so that the image's path names
/// the checked image while the guard is held, and the checked scratch also holds the claim's
/// handle, open for writing; the VMM inherits both. Dropping it closes them, which releases the
/// claim, and on Windows the guard, unless a VMM that inherited them still holds them; see
/// [Claims](self#claims).
///
/// A [`PreparedLaunch`](super::PreparedLaunch) takes it, with the path at which the check opened
/// the image, and hands the image to the VMM.
#[derive(Debug)]
pub struct CheckedScratch {
    /// The checked image, or on Windows its guard.
    file: File,
    /// The claim's handle on Windows: the image, open for writing and sharing every access,
    /// which the VMM inherits so that it holds the claim from its start.
    #[cfg(windows)]
    claim: File,
    /// The path at which the check opened the image.
    path: PathBuf,
}

impl CheckedScratch {
    /// Returns the checked image: on Linux, the image open for reading and writing, and on Windows
    /// its guard.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Returns the path at which the check opened the image.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the block geometry that OpenVMM presents for the image as the `scratch` block
    /// device, read through the checked file, as
    /// [`CheckedDistro::geometry`](super::distro::CheckedDistro::geometry) reads that of a distro
    /// layer: the image's length, logical blocks of 512 bytes, and physical blocks of the I/O
    /// block size of the image's filesystem on Linux, of 4096 bytes on Windows, and of the logical
    /// block size on other hosts. On Windows, the checked file is the image's guard, which may
    /// read the image's metadata.
    ///
    /// It reads the file's metadata alone, never the image's content. The scratch image of a
    /// clone must present the geometry of its template's scratch, its length included, as
    /// [`ClonePlan`](crate::host::template::ClonePlan) checks before the clone's launch.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a length or a
    /// physical block size that OpenVMM refuses, and with [`ErrorKind::Io`](crate::ErrorKind::Io)
    /// if the metadata cannot be read.
    pub fn geometry(&self) -> Result<BlockGeometry> {
        block_geometry(&self.file, &self.path, DESCRIPTION)
    }

    /// Returns the checked image, which the caller then holds with its claim: an owned descriptor
    /// on Linux, and on Windows an owned handle, the image's guard, which then holds the claim
    /// alone, since this closes the claim's handle.
    pub fn into_file(self) -> File {
        self.file
    }

    /// Returns the claim's handle, which the VMM inherits on Windows.
    #[cfg(windows)]
    pub(crate) fn claim(&self) -> &File {
        &self.claim
    }

    /// Returns the checked image, for a prepared launch to move.
    #[cfg(target_os = "linux")]
    pub(crate) fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

impl ScratchBinding {
    /// Binds the scratch image at `path`, which is spelled with its components alone, as
    /// [`Path::components`] reads them, without reading it, and returns its binding.
    ///
    /// The image must be a regular file that this process may open for writing, as the VMM
    /// opens it. Binding claims nothing. Fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a path that is not Unicode,
    /// with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for an image that is missing
    /// or not a regular file, and with [`ErrorKind::Io`](crate::ErrorKind::Io) if the image cannot
    /// be opened or sealed, as on hosts other than Linux and Windows.
    pub fn bind(path: &Path) -> Result<Self> {
        bind_with(&normalized(path)?, Access::WRITABLE)
    }

    /// Checks the scratch image at `path`, spelled with its components alone, as
    /// [`Path::components`] reads them, against this binding from its metadata alone, claims it,
    /// and returns it open, as the check found it.
    ///
    /// The binding must be of [`SCRATCH_BINDING_VERSION`], and the image must have the binding's
    /// scheme, identity, kind, and length. The check then claims the image, as
    /// [Claims](self#claims) describes, waiting until `deadline` for a claim that another launch
    /// holds to be released, and compares the image with the binding again once it holds the
    /// claim. It never reads or maps the image's content.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a path that is not
    /// Unicode; with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for a binding of
    /// another version, a missing image, and an image that changed; with
    /// [`ErrorKind::TimedOut`](crate::ErrorKind::TimedOut), and the code
    /// [`ErrorCode::ScratchClaimed`], for an image that another launch still claims at
    /// `deadline`; and with [`ErrorKind::Io`](crate::ErrorKind::Io) if the image cannot be opened,
    /// sealed, or claimed.
    pub fn check(&self, path: &Path, deadline: Instant) -> Result<CheckedScratch> {
        self.check_with(&normalized(path)?, deadline, Access::WRITABLE)
    }

    /// Returns the host that took the binding.
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

    /// Returns the binding's version, which a [check](Self::check) requires to be
    /// [`SCRATCH_BINDING_VERSION`].
    pub(crate) fn version(&self) -> u32 {
        self.version
    }

    /// Returns the binding of the image that `seal` seals.
    fn of(seal: &FileSeal) -> Self {
        Self {
            version: SCRATCH_BINDING_VERSION,
            scheme: seal.scheme(),
            device: seal.device(),
            file_id: FileId(seal.file_id()),
            kind: seal.kind(),
            length: seal.length(),
        }
    }

    /// Returns what differs first between the image that this binding records and the one that
    /// `seal` seals, in the order scheme, identity, kind, and length, or `None` if nothing does.
    fn difference(&self, seal: &FileSeal) -> Option<&'static str> {
        if self.scheme != seal.scheme() {
            Some("seal scheme")
        } else if (self.device, self.file_id.0) != (seal.device(), seal.file_id()) {
            Some("identity")
        } else if self.kind != seal.kind() {
            Some("kind")
        } else if self.length != seal.length() {
            Some("length")
        } else {
            None
        }
    }

    /// Implements [`check`](Self::check), opening the image through `access`.
    pub(crate) fn check_with(
        &self,
        path: &Path,
        deadline: Instant,
        access: Access<'_>,
    ) -> Result<CheckedScratch> {
        if self.version != SCRATCH_BINDING_VERSION {
            return Err(Error::unavailable(format!(
                "the scratch binding of {} has version {}, which this build does not check",
                path.display(),
                self.version
            )));
        }
        #[cfg(windows)]
        let (file, claim) = imp::open_checked(self, path, deadline, access)?;
        #[cfg(not(windows))]
        let file = imp::open_checked(self, path, deadline, access)?;
        Ok(CheckedScratch {
            file,
            #[cfg(windows)]
            claim,
            path: path.to_path_buf(),
        })
    }

    /// Fails if the image that `seal` seals, which the check opened at `path`, is not the one
    /// that this binding records.
    fn compare(&self, seal: &FileSeal, path: &Path) -> Result<()> {
        match self.difference(seal) {
            Some(field) => Err(Error::unavailable(format!(
                "{} changed after its scratch binding was made: its {field} differs",
                path.display()
            ))),
            None => Ok(()),
        }
    }
}

/// Implements [`ScratchBinding::bind`], opening the image through `access`.
pub(crate) fn bind_with(path: &Path, access: Access<'_>) -> Result<ScratchBinding> {
    let file = match (access.open)(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::IsADirectory => return Err(missing(path)),
        Err(error) => return Err(open_error(&error, path)),
    };
    let seal = FileSeal::of(&file).map_err(|error| os_error(&error, Some(path)))?;
    if seal.kind() != FileKind::Regular {
        return Err(missing(path));
    }
    Ok(ScratchBinding::of(&seal))
}

/// Returns `path` spelled with its components alone, as [`paths::normalized`] spells it.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a path that is not
/// Unicode, which no plan names.
fn normalized(path: &Path) -> Result<PathBuf> {
    if path.to_str().is_none() {
        return Err(refused(format!(
            "{DESCRIPTION} path is not valid Unicode: {}",
            path.display()
        )));
    }
    Ok(paths::normalized(path))
}

/// Returns the error of a scratch image that is missing, or that is not a regular file, as
/// `require_file` refuses its path.
fn missing(path: &Path) -> Error {
    Error::unavailable(format!("{DESCRIPTION} not found: {}", path.display()))
}

/// Returns the error of a scratch image that cannot be opened: unavailable if it is missing.
fn open_error(error: &io::Error, path: &Path) -> Error {
    if error.kind() == io::ErrorKind::NotFound {
        missing(path)
    } else {
        os_error(error, Some(path))
    }
}

/// Returns the error of a scratch image that another launch still claims, of the code
/// [`ErrorCode::ScratchClaimed`].
fn claimed(path: &Path) -> Error {
    Error::new(
        ErrorCode::ScratchClaimed,
        format!(
            "another sandbox claims the {DESCRIPTION} {}",
            path.display()
        ),
    )
}

/// Runs `attempt` until it returns a value, fails, or `deadline` passes, at every
/// [`CLAIM_RETRY_INTERVAL`]: `attempt` returns `None` while another launch claims the image at
/// `path`.
fn until_unclaimed<T>(
    path: &Path,
    deadline: Instant,
    mut attempt: impl FnMut() -> Result<Option<T>>,
) -> Result<T> {
    loop {
        if let Some(value) = attempt()? {
            return Ok(value);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(claimed(path));
        }
        thread::sleep(left.min(CLAIM_RETRY_INTERVAL));
    }
}

#[cfg(not(windows))]
/// The opens and claims of hosts other than Windows: Linux, and hosts that cannot seal files, on
/// which every bind and check fails.
mod imp {
    use std::fs::{File, OpenOptions, TryLockError};
    use std::io;
    use std::path::Path;
    use std::time::Instant;

    use super::{Access, ScratchBinding, open_error, os_error, until_unclaimed};
    use crate::error::Result;
    use crate::host::seal::{self, FileSeal};

    /// Opens `path` for reading and writing, following links, and on Unix without waiting for a
    /// writer of a FIFO, which the seal then shows.
    pub(super) fn open_writable(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::custom_flags(
            &mut options,
            nix::fcntl::OFlag::O_NONBLOCK.bits(),
        );
        options.open(path)
    }

    /// Opens the image at `path` through `access`, compares it with `binding`, and claims it with
    /// an exclusive `flock` lock of the open file description, waiting until `deadline` for
    /// another launch to release its claim, then compares it again.
    pub(super) fn open_checked(
        binding: &ScratchBinding,
        path: &Path,
        deadline: Instant,
        access: Access<'_>,
    ) -> Result<File> {
        let sealed = |file: &File| FileSeal::of(file).map_err(|error| os_error(&error, Some(path)));
        let file = match (access.open)(path) {
            Ok(file) => file,
            // A directory took the image's place: its seal tells how it differs.
            Err(error) if error.kind() == io::ErrorKind::IsADirectory => {
                let directory = seal::open_read(path).map_err(|error| open_error(&error, path))?;
                binding.compare(&sealed(&directory)?, path)?;
                return Err(open_error(&error, path));
            }
            Err(error) => return Err(open_error(&error, path)),
        };
        binding.compare(&sealed(&file)?, path)?;
        until_unclaimed(path, deadline, || match file.try_lock() {
            Ok(()) => Ok(Some(())),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(os_error(&error, Some(path))),
        })?;
        // The launch whose claim the check waited for may have changed the image meanwhile.
        binding.compare(&sealed(&file)?, path)?;
        Ok(file)
    }
}

#[cfg(windows)]
/// The opens and claims of Windows, through share modes.
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;
    use std::time::Instant;

    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA, SYNCHRONIZE,
    };

    use super::{Access, ScratchBinding, open_error, os_error, until_unclaimed};
    use crate::error::Result;
    use crate::host::seal::FileSeal;

    /// Opens `path` for writing and for its attributes, but not for reading, following links,
    /// sharing every access, as a bind does, and as a check opens the claim's handle.
    pub(super) fn open_writable(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_DATA | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }

    /// Opens the guard of the image at `path`: for its attributes and for deleting it, which no
    /// other guard shares, sharing reading and writing, and denying deleting and renaming the
    /// image while it is held.
    fn open_guard(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | DELETE | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }

    /// Opens the image at `path` for writing, denying writing, which succeeds only while no other
    /// process has the image open for writing.
    fn open_sole_writer(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_DATA | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }

    /// Returns whether `error` is a sharing violation, which an open whose share mode or access
    /// conflicts with another handle of the file fails with.
    fn sharing_violation(error: &io::Error) -> bool {
        error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
    }

    /// Opens the guard of the image at `path`, compares the image with `binding`, and claims it,
    /// as [Claims](super#claims) describes, waiting until `deadline` for another launch to release
    /// its claim, then compares it again, and returns the guard and the claim's handle. The image
    /// is opened for its attributes, and for writing or deleting it, but never for reading, so
    /// `access`, which binds use, does not apply.
    pub(super) fn open_checked(
        binding: &ScratchBinding,
        path: &Path,
        deadline: Instant,
        _access: Access<'_>,
    ) -> Result<(File, File)> {
        let sealed = |file: &File| FileSeal::of(file).map_err(|error| os_error(&error, Some(path)));
        let guard = until_unclaimed(path, deadline, || match open_guard(path) {
            Ok(guard) => Ok(Some(guard)),
            Err(error) if sharing_violation(&error) => Ok(None),
            Err(error) => Err(open_error(&error, path)),
        })?;
        binding.compare(&sealed(&guard)?, path)?;
        until_unclaimed(path, deadline, || match open_sole_writer(path) {
            Ok(writer) => {
                // The guard keeps the path naming the image, so this is the guarded image, which
                // the writer that the check waited for may have changed meanwhile. The probe keeps
                // any other writer out while the check compares it again.
                binding.compare(&sealed(&writer)?, path)?;
                Ok(Some(()))
            }
            Err(error) if sharing_violation(&error) => Ok(None),
            Err(error) => Err(open_error(&error, path)),
        })?;
        // The probe is closed, and no other check probes the image while this one holds the guard.
        // The claim's handle, open for writing, then keeps the probes of other checks out for as
        // long as this process, or a VMM that inherits it, holds it. A link on the way that now
        // leads elsewhere fails the comparison.
        let claim = open_writable(path).map_err(|error| open_error(&error, path))?;
        binding.compare(&sealed(&claim)?, path)?;
        Ok((guard, claim))
    }
}

#[cfg(all(test, any(target_os = "linux", windows)))]
mod tests;
