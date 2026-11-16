//! Live shares, virtio-fs devices that the VMM exports from the host, mounted at targets in a
//! layered root through the root's mapper, after the checks that the
//! [module](super#live-shares-and-mappings) lists, and the host export, the device from which a
//! caller's mappings come.

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::{
    AGGREGATE_DIRECTORY, EXPORT_DIRECTORY, Error, HOSTFS_DIRECTORY, HOSTFS_DIRECTORY_MODE, Mount,
    Root, RuntimeMounts, STAGING_DIRECTORY,
};
use crate::guest::mappings::{
    self, Access, Kernel as MappingKernel, Kind, Mapper, Mapping, MappingError, Reserved,
    SourceRoot, TargetRoot,
};
use crate::guest::sys::{self, Errno, FileDescriptor, Mode, OpenFlags};
use crate::protocol::export::ExportMode;
use crate::protocol::share::{self, RESERVED_TARGET_CHARACTERS};

/// The file system type of a live share.
const VIRTIOFS: &[u8] = b"virtiofs";

/// What a child of an aggregate share is: a host directory, or a host file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum ShareKind {
    /// A directory, bound at a directory of the root.
    #[default]
    Directory,
    /// A regular file, bound at a regular file of the root.
    File,
}

impl ShareKind {
    /// Returns the kind of the mapping that binds a child of this kind.
    fn kind(self) -> Kind {
        match self {
            Self::Directory => Kind::Directory,
            Self::File => Kind::File,
        }
    }
}

/// A child of an aggregate share: an entry of the aggregate, bound at a target in the root.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ShareChild {
    /// The name of the child's entry in the aggregate: letters, digits, `.`, `_`, and `-`, other
    /// than `.` and `..`.
    pub(crate) name: String,
    /// The absolute path in the root at which the child is bound.
    pub(crate) target: PathBuf,
    /// Whether the workload may write the child.
    pub(crate) access: Access,
    /// What the child is.
    pub(crate) kind: ShareKind,
}

/// Why a live share, or the host export, was refused before anything was mounted for it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub(crate) enum ShareRefusal {
    /// The virtio-fs tag is not a nonempty name of letters, digits, `_`, and `-`.
    Tag(String),
    /// An aggregate has no children.
    NoChildren,
    /// A child's name is not a nonempty name of letters, digits, `.`, `_`, and `-` of at most
    /// [`MAX_PATH_BYTES`](mappings::MAX_PATH_BYTES) bytes, or is `.` or `..`.
    ChildName(String),
    /// Two children of an aggregate have this name.
    DuplicateChild(String),
    /// The target is not an absolute path.
    Relative(PathBuf),
    /// The target does not name a path below the root: it is `/`, has an empty, `.`, or `..`
    /// component, a trailing `/`, or a NUL, or is longer than
    /// [`MAX_PATH_BYTES`](mappings::MAX_PATH_BYTES) bytes.
    NotCanonical(PathBuf),
    /// The target holds a reserved character: `\`, `=`, `,`, or `"`.
    ReservedCharacter(PathBuf),
    /// The container runtime reserves the target, as [`share::reservation`] tells: it mounts
    /// there or below it, binds over it, or creates it.
    Reserved(PathBuf),
    /// The target is the target of another share or mapping, or one inside it or that holds it.
    Overlap {
        /// The other share's or mapping's target.
        claimed: PathBuf,
        /// The refused target.
        target: PathBuf,
    },
    /// The root owns an aggregate already, whose children may or may not all be bound: it mounts
    /// every aggregate at the same place.
    SecondAggregate,
    /// The root owns the host export already: it mounts every export at the same place.
    SecondExport,
}

impl fmt::Display for ShareRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tag(tag) => write!(formatter, "live share tag is invalid: {tag}"),
            Self::NoChildren => formatter.write_str("the aggregate live share has no children"),
            Self::ChildName(name) => write!(formatter, "live share child is invalid: {name}"),
            Self::DuplicateChild(name) => {
                write!(formatter, "live share child is not unique: {name}")
            }
            Self::Relative(target) => write!(
                formatter,
                "live share target must be an absolute path: {}",
                target.display()
            ),
            Self::NotCanonical(target) => write!(
                formatter,
                "live share target is not canonical: {}",
                target.display()
            ),
            Self::ReservedCharacter(target) => write!(
                formatter,
                "live share target contains a reserved character: {}",
                target.display()
            ),
            Self::Reserved(target) => write!(
                formatter,
                "live share target is reserved: {}",
                target.display()
            ),
            Self::Overlap { claimed, target } => write!(
                formatter,
                "live share targets overlap: {} and {}",
                claimed.display(),
                target.display()
            ),
            Self::SecondAggregate => {
                formatter.write_str("the root has an aggregate live share already")
            }
            Self::SecondExport => formatter.write_str("the root has a host export already"),
        }
    }
}

impl From<ExportMode> for Access {
    /// Returns the access of the guest's mount of a host export of `mode`, which
    /// `Root::mount_export` takes: read-only for `ro`, and read-write for `rw`.
    fn from(mode: ExportMode) -> Self {
        match mode {
            ExportMode::ReadOnly => Self::ReadOnly,
            ExportMode::ReadWrite => Self::ReadWrite,
        }
    }
}

impl Root {
    /// Mounts the virtio-fs device `tag` at `target` in the root, a directory, with `access`,
    /// `nosuid`, and `nodev`.
    ///
    /// The target is checked as the [module](super#live-shares-and-mappings) describes, against
    /// the targets of the root's other shares and mappings; nothing is mounted for a refused one.
    /// The device is mounted at `staging` in the runtime directory, which may not be a mount
    /// point already, with its flags, then the root's mapper moves it onto the target, which it
    /// creates on the overlay, or on the runtime's `/run` tmpfs, if it is missing. The mount
    /// belongs to the root, which unmounts it first when it is torn down.
    ///
    /// A failure once the target passed its checks latches the root, and unmounts the device from
    /// `staging`, or leaves it there for the teardown if that unmount fails too.
    pub(crate) fn mount_share(
        &mut self,
        tag: &str,
        target: &Path,
        access: Access,
    ) -> Result<(), Error> {
        let mount = Mount::Share {
            tag: tag.to_owned(),
            target: target.to_owned(),
        };
        self.check_latch(&mount)?;
        check_tag(tag)?;
        check_target(target)?;
        self.check_overlap(target, &[])?;
        let relative = mappings::below_its_root(target, true)
            .ok_or_else(|| Error::Share(ShareRefusal::NotCanonical(target.to_owned())))?
            .to_path_buf();
        let mounted = self.stage_share(&mount, tag, &relative, access);
        self.latched |= mounted.is_err();
        mounted
    }

    /// Mounts the aggregate virtio-fs device `tag`, with `access`, `nosuid`, and `nodev`, at
    /// `shares` in the runtime directory, outside the root, where the workload cannot list its
    /// children, and which may not be a symbolic link. Then the root's mapper binds each child, in
    /// order, at its target in the root: a copy of the mount of the child's entry in the
    /// aggregate, with the child's access, `nosuid`, and `nodev` set before it is attached.
    ///
    /// Every child is checked first: its name, which no other child has, and its target, as the
    /// [module](super#live-shares-and-mappings) describes, against the other children's and the
    /// root's other shares' and mappings' targets. Nothing is mounted unless every child passes.
    /// Each target is created on the overlay, or on the runtime's `/run` tmpfs, if it is missing.
    /// A child whose entry in the aggregate is missing, or is not of the child's kind, fails to
    /// mount, and latches the root, as any failure once the children passed their checks does.
    /// The mounts belong to the root, which unmounts them first when it is torn down, the bound
    /// children in reverse order, then the aggregate, those of an aggregate that failed part of
    /// the way included.
    ///
    /// A root has one aggregate at most, since every aggregate is mounted at the same place: once
    /// the root has mounted one, whether or not its children were all bound, another is refused.
    pub(crate) fn mount_aggregate(
        &mut self,
        tag: &str,
        access: Access,
        children: &[ShareChild],
    ) -> Result<(), Error> {
        let aggregate = Mount::Aggregate {
            tag: tag.to_owned(),
        };
        self.check_latch(&aggregate)?;
        if self
            .shares
            .iter()
            .any(|(mount, _)| matches!(mount, Mount::Aggregate { .. }))
        {
            return Err(Error::Share(ShareRefusal::SecondAggregate));
        }
        check_tag(tag)?;
        if children.is_empty() {
            return Err(Error::Share(ShareRefusal::NoChildren));
        }
        let mut targets: Vec<PathBuf> = Vec::with_capacity(children.len());
        let mut binds = Vec::with_capacity(children.len());
        for (index, child) in children.iter().enumerate() {
            check_name(&child.name)?;
            if children[..index]
                .iter()
                .any(|earlier| earlier.name == child.name)
            {
                return Err(Error::Share(ShareRefusal::DuplicateChild(
                    child.name.clone(),
                )));
            }
            check_target(&child.target)?;
            self.check_overlap(&child.target, &targets)?;
            targets.push(child.target.clone());
            // The checks leave only a name longer than a path to refuse.
            let mapping = Mapping::new(&child.name, &child.target, child.access)
                .map_err(|_| Error::Share(ShareRefusal::ChildName(child.name.clone())))?;
            let mount = Mount::Child {
                name: child.name.clone(),
                target: child.target.clone(),
            };
            binds.push((mount, mapping.with_kind(child.kind.kind())));
        }
        let mounted = self.bind_children(&aggregate, tag, access, &binds);
        self.latched |= mounted.is_err();
        mounted
    }

    /// Mounts the host export, the aggregate virtio-fs device `tag` from which the caller maps host
    /// paths into the root, and returns the export's root, the [`SourceRoot`] of those mappings. A
    /// guest reads the export's tag and [`ExportMode`] from its kernel command line with
    /// [`HostExport::parse`](crate::protocol::export::HostExport::parse), and mounts it with the
    /// mode's access.
    ///
    /// The device is mounted with `access`, `nosuid`, and `nodev` at `hostfs/root` in the runtime
    /// directory, outside the root, where the workload cannot list the export's children: below
    /// `hostfs`, a directory that the root creates with mode 0700, whatever the umask, so that only
    /// root may enter it. A `hostfs` that is there already, such as one that an earlier root left,
    /// must be as private: owned by the process's effective user, root in a guest, with no
    /// permission for its group or others. Another is refused, and keeps its owner and mode.
    /// Neither `hostfs` nor `hostfs/root` may be a symbolic link. The tag is checked first, as a
    /// live share's is; nothing is created or mounted for a refused one.
    ///
    /// The mount belongs to the root, which unmounts it when it is torn down, after the live
    /// shares. The returned [`SourceRoot`] holds a descriptor of the export's root, which keeps the
    /// mount busy: drop it before the teardown, which otherwise fails to unmount the export, and
    /// leaves it mounted.
    ///
    /// A root has one export at most, since every export is mounted at the same place: once the
    /// root has mounted one, another is refused. A failure once the tag passed its check latches
    /// the root; an export that is mounted by then stays for the teardown.
    pub(crate) fn mount_export(&mut self, tag: &str, access: Access) -> Result<SourceRoot, Error> {
        let export = Mount::Export {
            tag: tag.to_owned(),
        };
        self.check_latch(&export)?;
        if self
            .shares
            .iter()
            .any(|(mount, _)| matches!(mount, Mount::Export { .. }))
        {
            return Err(Error::Share(ShareRefusal::SecondExport));
        }
        check_tag(tag)?;
        let mounted = self.mount_export_at(&export, tag, access);
        self.latched |= mounted.is_err();
        mounted
    }

    /// Mounts the device `tag` at `staging` and moves it onto the relative `target`, as
    /// [`mount_share`](Self::mount_share) describes.
    fn stage_share(
        &mut self,
        mount: &Mount,
        tag: &str,
        target: &Path,
        access: Access,
    ) -> Result<(), Error> {
        self.mapper()?;
        let staging = self.runtime.join(STAGING_DIRECTORY);
        super::create_directory(&staging).map_err(|error| Error::MountPoint {
            path: staging.clone(),
            error,
        })?;
        match self.kernel.is_mount_point(&staging) {
            Ok(false) => {}
            Ok(true) => return Err(Error::Occupied { path: staging }),
            Err(error) => {
                return Err(Error::MountPoint {
                    path: staging,
                    error,
                });
            }
        }
        self.mount_at(
            mount,
            Some(tag.as_bytes()),
            &staging,
            Some(VIRTIOFS),
            access.flags(),
            None,
        )?;
        let moved = match open_mount_point(&staging) {
            Ok(staged) => {
                let (kernel, mapper) = self.mapper()?;
                mapper
                    .move_mount_with(kernel, &staged, target, access)
                    .map_err(|error| Error::Mapping {
                        mount: mount.clone(),
                        error,
                    })
            }
            Err(error) => Err(Error::MountPoint {
                path: staging.clone(),
                error: error.into(),
            }),
        };
        match moved {
            Ok(()) => {
                self.mapped.push(mount.clone());
                Ok(())
            }
            Err(error) => {
                match &error {
                    // A share that a rename moved is detached already, or the mapper's to detach
                    // at the teardown.
                    Error::Mapping {
                        error: MappingError::Moved { mounted },
                        ..
                    } => self.strayed(mount, *mounted),
                    // Any other is at `staging`.
                    _ => {
                        let unmounted = sys::path_c_string(&staging)
                            .and_then(|path| self.kernel.unmount(&path));
                        if unmounted.is_err() {
                            self.shares.push((mount.clone(), staging));
                        }
                    }
                }
                Err(error)
            }
        }
    }

    /// Mounts the aggregate device `tag` at `shares` and binds each of `binds` from it, as
    /// [`mount_aggregate`](Self::mount_aggregate) describes.
    fn bind_children(
        &mut self,
        aggregate: &Mount,
        tag: &str,
        access: Access,
        binds: &[(Mount, Mapping)],
    ) -> Result<(), Error> {
        self.mapper()?;
        let directory = self.runtime.join(AGGREGATE_DIRECTORY);
        super::create_directory(&directory).map_err(|error| Error::MountPoint {
            path: directory.clone(),
            error,
        })?;
        self.mount_at(
            aggregate,
            Some(tag.as_bytes()),
            &directory,
            Some(VIRTIOFS),
            access.flags(),
            None,
        )?;
        self.shares.push((aggregate.clone(), directory.clone()));
        // The descriptor of the aggregate's root, which keeps it busy, closes on return.
        let source = SourceRoot::open(&directory).map_err(|error| Error::MountPoint {
            path: directory.clone(),
            error,
        })?;
        for (mount, mapping) in binds {
            self.apply_one(&source, mount, mapping)?;
        }
        Ok(())
    }

    /// Mounts the export's device `tag` at `hostfs/root` and opens its root, as
    /// [`mount_export`](Self::mount_export) describes.
    fn mount_export_at(
        &mut self,
        export: &Mount,
        tag: &str,
        access: Access,
    ) -> Result<SourceRoot, Error> {
        let hostfs = self.runtime.join(HOSTFS_DIRECTORY);
        create_private_directory(&hostfs).map_err(|error| Error::MountPoint {
            path: hostfs,
            error,
        })?;
        let directory = self.runtime.join(EXPORT_DIRECTORY);
        super::create_directory(&directory).map_err(|error| Error::MountPoint {
            path: directory.clone(),
            error,
        })?;
        self.mount_at(
            export,
            Some(tag.as_bytes()),
            &directory,
            Some(VIRTIOFS),
            access.flags(),
            None,
        )?;
        self.shares.push((export.clone(), directory.clone()));
        SourceRoot::open(&directory).map_err(|error| Error::MountPoint {
            path: directory,
            error,
        })
    }

    /// Applies `mapping` from `source` through the root's mapper, and owns it as `mount`.
    fn apply_one(
        &mut self,
        source: &SourceRoot,
        mount: &Mount,
        mapping: &Mapping,
    ) -> Result<(), Error> {
        let (kernel, mapper) = self.mapper()?;
        let applied = mapper.apply_with(kernel, source, std::slice::from_ref(mapping));
        match applied {
            Ok(()) => {
                self.mapped.push(mount.clone());
                Ok(())
            }
            Err(failure) => {
                if let MappingError::Moved { mounted } = failure.error() {
                    self.strayed(mount, *mounted);
                }
                Err(Error::Mapping {
                    mount: mount.clone(),
                    error: failure.error().clone(),
                })
            }
        }
    }

    /// Owns `mount`, which a rename moved away from its target, as the last of the mapper's
    /// mounts, if it stays `mounted`: the mapper detaches it at the teardown, and reports it with
    /// that index if it fails to.
    fn strayed(&mut self, mount: &Mount, mounted: bool) {
        if mounted {
            self.mapped.push(mount.clone());
        }
    }

    /// Returns the kernel and the root's mapper, which it opens beneath the overlay, with NVX's
    /// container root's reserved targets, unless the root has one already. The mapper's own
    /// mounts, on which it creates what a target lacks, are the overlay and, if the root holds
    /// it, the runtime's `/run` tmpfs, as a guest's mapper owns its `/tmp`.
    fn mapper(&mut self) -> Result<(&mut dyn MappingKernel, &mut Mapper), Error> {
        let mapper = match self.mapper.take() {
            Some(mapper) => mapper,
            None => {
                let root =
                    TargetRoot::open_with(self.kernel.as_mut(), &self.path).map_err(|error| {
                        Error::MountPoint {
                            path: self.path.clone(),
                            error,
                        }
                    })?;
                Mapper::new(self.add_run(root)?, Reserved::container_root())
            }
        };
        let mapper = self.mapper.insert(mapper);
        Ok((self.kernel.as_mut(), mapper))
    }

    /// Adds the runtime's `/run` tmpfs, if the root holds it, to `root`'s own mounts.
    fn add_run(&mut self, mut root: TargetRoot) -> Result<TargetRoot, Error> {
        let run = Mount::Runtime(RuntimeMounts::Run);
        if let Some((_, path)) = self.shares.iter().find(|(mount, _)| *mount == run) {
            root.add_mount_with(self.kernel.as_mut(), path)
                .map_err(|error| Error::MountPoint {
                    path: path.clone(),
                    error,
                })?;
        }
        Ok(root)
    }

    /// Refuses `mount` if a share or a mapping failed before.
    fn check_latch(&self, mount: &Mount) -> Result<(), Error> {
        if self.latched {
            return Err(Error::Mapping {
                mount: mount.clone(),
                error: MappingError::Latched,
            });
        }
        Ok(())
    }

    /// Refuses `target` if it overlaps the target of one of the root's shares or mappings, or one
    /// of `pending`.
    fn check_overlap(&self, target: &Path, pending: &[PathBuf]) -> Result<(), Error> {
        let below = |inner: &Path, outer: &Path| {
            let mut inner = inner.as_os_str().as_bytes().to_vec();
            let mut outer = outer.as_os_str().as_bytes().to_vec();
            inner.push(b'/');
            outer.push(b'/');
            inner.starts_with(&outer)
        };
        let mapped = self
            .mapper
            .iter()
            .flat_map(Mapper::targets)
            .map(|mapped| Path::new("/").join(mapped));
        match mapped
            .chain(pending.iter().cloned())
            .find(|claimed| below(target, claimed) || below(claimed, target))
        {
            Some(claimed) => Err(Error::Share(ShareRefusal::Overlap {
                claimed,
                target: target.to_owned(),
            })),
            None => Ok(()),
        }
    }
}

/// Opens the mount point `path` with `O_PATH`, not through a final symbolic link.
pub(super) fn open_mount_point(path: &Path) -> Result<FileDescriptor, Errno> {
    let flags =
        OpenFlags::O_PATH | OpenFlags::O_DIRECTORY | OpenFlags::O_NOFOLLOW | OpenFlags::O_CLOEXEC;
    sys::open(&sys::path_c_string(path)?, flags, Mode::empty())
}

/// Creates the directory `path` with [`HOSTFS_DIRECTORY_MODE`], whatever the umask, unless a
/// directory, and not a symbolic link, is there already, which must be private to the process's
/// effective user, as [`check_private`] requires; another is refused, and stays unchanged. It is
/// created with no permission beyond that mode at any time.
fn create_private_directory(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new()
        .mode(HOSTFS_DIRECTORY_MODE)
        .create(path)
    {
        // The umask may have withheld some of the mode's permissions.
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(HOSTFS_DIRECTORY_MODE)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)?;
            if !metadata.is_dir() {
                return Err(Errno::ENOTDIR.into());
            }
            check_private(
                metadata.uid(),
                metadata.mode(),
                nix::unistd::geteuid().as_raw(),
            )
        }
        Err(error) => Err(error),
    }
}

/// Refuses a directory that the user `owner` owns with `mode` unless it is private to `user`:
/// unless `user` owns it, and it grants its group and others no permission.
fn check_private(owner: u32, mode: u32, user: u32) -> io::Result<()> {
    if owner == user && mode & (Mode::S_IRWXG | Mode::S_IRWXO).bits() == 0 {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "the directory there already is not private to user {user}: user {owner} owns it \
             with mode {:04o}",
            mode & 0o7777
        ),
    ))
}

/// Returns `error` as the error of a teardown's failed unmount: the call's own for a failed
/// system call.
pub(super) fn io_error(error: &MappingError) -> io::Error {
    match error {
        MappingError::System { error, .. } => io::Error::from_raw_os_error(*error),
        error => io::Error::other(error.clone()),
    }
}

/// Refuses a virtio-fs tag that is not a nonempty name of letters, digits, `_`, and `-`.
fn check_tag(tag: &str) -> Result<(), Error> {
    if tag.is_empty()
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(Error::Share(ShareRefusal::Tag(tag.to_owned())));
    }
    Ok(())
}

/// Refuses an aggregate child's name that is not a nonempty name of letters, digits, `.`, `_`,
/// and `-`, or that is `.` or `..`.
fn check_name(name: &str) -> Result<(), Error> {
    if matches!(name, "" | "." | "..")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(Error::Share(ShareRefusal::ChildName(name.to_owned())));
    }
    Ok(())
}

/// Refuses a target that is not canonical and absolute, that holds a reserved character, or that
/// the container runtime reserves.
fn check_target(target: &Path) -> Result<(), Error> {
    let bytes = target.as_os_str().as_bytes();
    let refusal = if !bytes.starts_with(b"/") {
        Some(ShareRefusal::Relative(target.to_owned()))
    } else if mappings::below_its_root(target, true).is_none() {
        Some(ShareRefusal::NotCanonical(target.to_owned()))
    } else if bytes
        .iter()
        .any(|byte| RESERVED_TARGET_CHARACTERS.as_bytes().contains(byte))
    {
        Some(ShareRefusal::ReservedCharacter(target.to_owned()))
    } else if share::reservation(bytes).is_some() {
        Some(ShareRefusal::Reserved(target.to_owned()))
    } else {
        None
    };
    refusal.map_or(Ok(()), |refusal| Err(Error::Share(refusal)))
}

#[cfg(test)]
mod tests;
