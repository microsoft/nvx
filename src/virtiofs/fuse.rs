//! FUSE protocol server backed directly by a host directory.

use ::std::collections::HashMap;
use ::std::ffi::{OsStr, OsString};
#[cfg(target_os = "windows")]
use ::std::fs::OpenOptions;
use ::std::fs::{self, File, Metadata};
use ::std::io::{Read, Seek, SeekFrom, Write};
use ::std::path::{Component, Path, PathBuf};
use ::std::time::{Duration, SystemTime, UNIX_EPOCH};

use ::anyhow::{Context, Result, bail};
use ::log::{debug, warn};

use super::RequestHandler;

const FUSE_HEADER_LEN: usize = 40;
const FUSE_OUT_HEADER_LEN: usize = 16;
const FUSE_ROOT_ID: u64 = 1;
const FUSE_MAJOR: u32 = 7;
const FUSE_MINOR: u32 = 31;
const MAX_WRITE: u32 = 1 << 20;
const STATE_VERSION: u32 = 4;

const FUSE_LOOKUP: u32 = 1;
const FUSE_FORGET: u32 = 2;
const FUSE_GETATTR: u32 = 3;
const FUSE_SETATTR: u32 = 4;
const FUSE_READLINK: u32 = 5;
const FUSE_SYMLINK: u32 = 6;
const FUSE_MKNOD: u32 = 8;
const FUSE_MKDIR: u32 = 9;
const FUSE_UNLINK: u32 = 10;
const FUSE_RMDIR: u32 = 11;
const FUSE_RENAME: u32 = 12;
const FUSE_LINK: u32 = 13;
const FUSE_OPEN: u32 = 14;
const FUSE_READ: u32 = 15;
const FUSE_WRITE: u32 = 16;
const FUSE_STATFS: u32 = 17;
const FUSE_RELEASE: u32 = 18;
const FUSE_FSYNC: u32 = 20;
const FUSE_SETXATTR: u32 = 21;
const FUSE_GETXATTR: u32 = 22;
const FUSE_LISTXATTR: u32 = 23;
const FUSE_REMOVEXATTR: u32 = 24;
const FUSE_FLUSH: u32 = 25;
const FUSE_INIT: u32 = 26;
const FUSE_OPENDIR: u32 = 27;
const FUSE_READDIR: u32 = 28;
const FUSE_RELEASEDIR: u32 = 29;
const FUSE_FSYNCDIR: u32 = 30;
const FUSE_GETLK: u32 = 31;
const FUSE_SETLK: u32 = 32;
const FUSE_SETLKW: u32 = 33;
const FUSE_ACCESS: u32 = 34;
const FUSE_CREATE: u32 = 35;
const FUSE_INTERRUPT: u32 = 36;
const FUSE_DESTROY: u32 = 38;
const FUSE_IOCTL: u32 = 39;
const FUSE_POLL: u32 = 40;
const FUSE_BATCH_FORGET: u32 = 42;
const FUSE_FALLOCATE: u32 = 43;
const FUSE_READDIRPLUS: u32 = 44;
const FUSE_RENAME2: u32 = 45;
const FUSE_LSEEK: u32 = 46;
const FUSE_COPY_FILE_RANGE: u32 = 47;
const FUSE_SYNCFS: u32 = 50;

const FATTR_MODE: u32 = 1 << 0;
const FATTR_UID: u32 = 1 << 1;
const FATTR_GID: u32 = 1 << 2;
const FATTR_SIZE: u32 = 1 << 3;
const FATTR_ATIME: u32 = 1 << 4;
const FATTR_MTIME: u32 = 1 << 5;
const FATTR_FH: u32 = 1 << 6;
const FATTR_ATIME_NOW: u32 = 1 << 7;
const FATTR_MTIME_NOW: u32 = 1 << 8;

const FUSE_ASYNC_READ: u32 = 1 << 0;
const FUSE_BIG_WRITES: u32 = 1 << 5;
const FUSE_AUTO_INVAL_DATA: u32 = 1 << 12;
const FUSE_DO_READDIRPLUS: u32 = 1 << 13;
const FUSE_MAX_PAGES: u32 = 1 << 22;
const FOPEN_DIRECT_IO: u32 = 1 << 0;

const O_ACCMODE: u32 = 0x3;
const O_WRONLY: u32 = 0x1;
const O_RDWR: u32 = 0x2;
const O_CREAT: u32 = 0x40;
const O_EXCL: u32 = 0x80;
const O_TRUNC: u32 = 0x200;
const O_APPEND: u32 = 0x400;

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
#[cfg(target_os = "windows")]
const S_IFDIR: u32 = 0o040000;
#[cfg(target_os = "windows")]
const S_IFLNK: u32 = 0o120000;

const RENAME_NOREPLACE: u32 = 1;

const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EBADF: i32 = 9;
const EACCES: i32 = 13;
#[cfg(any(target_os = "windows", test))]
const EEXIST: i32 = 17;
const ENOTDIR: i32 = 20;
#[cfg(target_os = "windows")]
const EISDIR: i32 = 21;
const EINVAL: i32 = 22;
#[cfg(target_os = "windows")]
const ENOSPC: i32 = 28;
const EROFS: i32 = 30;
#[cfg(target_os = "windows")]
const ENOTEMPTY: i32 = 39;
const ENOSYS: i32 = 38;
#[cfg(test)]
const ELOOP: i32 = 40;
const EOPNOTSUPP: i32 = 95;
const ESTALE: i32 = 116;

type FuseResult<T> = ::std::result::Result<T, i32>;

#[derive(Clone)]
struct Node {
    path: PathBuf,
    lookups: u64,
    identity: FileIdentity,
}

enum Handle {
    File {
        file: File,
        path: PathBuf,
        flags: u32,
        nodeid: u64,
    },
    Directory {
        file: File,
        path: PathBuf,
        nodeid: u64,
        entries: Option<Vec<OsString>>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity(u64, u64);

struct ChildLocation {
    parent: File,
    path: PathBuf,
    name: OsString,
}

#[derive(Default)]
struct Attr {
    ino: u64,
    size: u64,
    blocks: u64,
    atime: u64,
    mtime: u64,
    ctime: u64,
    atimensec: u32,
    mtimensec: u32,
    ctimensec: u32,
    mode: u32,
    nlink: u32,
    uid: u32,
    gid: u32,
    rdev: u32,
    blksize: u32,
    flags: u32,
}

impl Attr {
    fn encode(&self, output: &mut Vec<u8>) {
        put_u64(output, self.ino);
        put_u64(output, self.size);
        put_u64(output, self.blocks);
        put_u64(output, self.atime);
        put_u64(output, self.mtime);
        put_u64(output, self.ctime);
        put_u32(output, self.atimensec);
        put_u32(output, self.mtimensec);
        put_u32(output, self.ctimensec);
        put_u32(output, self.mode);
        put_u32(output, self.nlink);
        put_u32(output, self.uid);
        put_u32(output, self.gid);
        put_u32(output, self.rdev);
        put_u32(output, self.blksize);
        put_u32(output, self.flags);
    }
}

struct Header {
    opcode: u32,
    unique: u64,
    nodeid: u64,
}

enum DispatchReply {
    Payload(Vec<u8>),
    Empty,
    None,
}

/// A confined, live passthrough filesystem rooted at one host directory.
pub struct PassthroughFs {
    root: PathBuf,
    root_handle: File,
    writable: bool,
    nodes: HashMap<u64, Node>,
    by_path: HashMap<PathBuf, u64>,
    handles: HashMap<u64, Handle>,
    next_nodeid: u64,
    next_handle: u64,
    initialized: bool,
}

impl PassthroughFs {
    /// Creates a live export. The root is canonicalized once and never changed.
    pub fn new(root: &Path, writable: bool) -> Result<Self> {
        if !root.is_dir() {
            bail!("--mount path {root:?} is not a directory");
        }
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing --mount directory {root:?}"))?;
        let root_handle = open_export_root(&root)
            .with_context(|| format!("opening --mount directory {root:?}"))?;
        let mut filesystem = Self {
            root,
            root_handle,
            writable,
            nodes: HashMap::new(),
            by_path: HashMap::new(),
            handles: HashMap::new(),
            next_nodeid: FUSE_ROOT_ID + 1,
            next_handle: 1,
            initialized: false,
        };
        filesystem.install_root();
        Ok(filesystem)
    }

    fn install_root(&mut self) {
        let identity = file_identity(&self.root_handle)
            .expect("opened virtio-fs root must have stable identity");
        self.nodes.insert(
            FUSE_ROOT_ID,
            Node {
                path: PathBuf::new(),
                lookups: u64::MAX,
                identity,
            },
        );
        self.by_path.insert(PathBuf::new(), FUSE_ROOT_ID);
    }

    fn dispatch(&mut self, header: &Header, payload: &[u8]) -> FuseResult<DispatchReply> {
        match header.opcode {
            FUSE_INIT => self.init(payload).map(DispatchReply::Payload),
            FUSE_LOOKUP => self
                .lookup(header.nodeid, payload)
                .map(DispatchReply::Payload),
            FUSE_FORGET => {
                self.forget(header.nodeid, payload)?;
                Ok(DispatchReply::None)
            }
            FUSE_BATCH_FORGET => {
                self.batch_forget(payload)?;
                Ok(DispatchReply::None)
            }
            FUSE_GETATTR => self
                .getattr(header.nodeid, payload)
                .map(DispatchReply::Payload),
            FUSE_SETATTR => self
                .setattr(header.nodeid, payload)
                .map(DispatchReply::Payload),
            FUSE_READLINK => self.readlink(header.nodeid).map(DispatchReply::Payload),
            FUSE_SYMLINK => self
                .symlink(header.nodeid, payload)
                .map(DispatchReply::Payload),
            FUSE_MKNOD => self
                .mknod(header.nodeid, payload)
                .map(DispatchReply::Payload),
            FUSE_MKDIR => self
                .mkdir(header.nodeid, payload)
                .map(DispatchReply::Payload),
            FUSE_UNLINK => {
                self.unlink(header.nodeid, payload, false)?;
                Ok(DispatchReply::Empty)
            }
            FUSE_RMDIR => {
                self.unlink(header.nodeid, payload, true)?;
                Ok(DispatchReply::Empty)
            }
            FUSE_RENAME => {
                self.rename(header.nodeid, payload, false)?;
                Ok(DispatchReply::Empty)
            }
            FUSE_RENAME2 => {
                self.rename(header.nodeid, payload, true)?;
                Ok(DispatchReply::Empty)
            }
            FUSE_LINK => self
                .link(header.nodeid, payload)
                .map(DispatchReply::Payload),
            FUSE_OPEN => self
                .open(header.nodeid, payload)
                .map(DispatchReply::Payload),
            FUSE_CREATE => self
                .create(header.nodeid, payload)
                .map(DispatchReply::Payload),
            FUSE_READ => self.read(payload).map(DispatchReply::Payload),
            FUSE_WRITE => self.write(payload).map(DispatchReply::Payload),
            FUSE_FLUSH => {
                self.flush(payload)?;
                Ok(DispatchReply::Empty)
            }
            FUSE_FSYNC => {
                self.fsync(payload)?;
                Ok(DispatchReply::Empty)
            }
            FUSE_RELEASE => {
                self.release(payload, false)?;
                Ok(DispatchReply::Empty)
            }
            FUSE_OPENDIR => self.opendir(header.nodeid).map(DispatchReply::Payload),
            FUSE_READDIR => self
                .readdir(header.nodeid, payload, false)
                .map(DispatchReply::Payload),
            FUSE_READDIRPLUS => self
                .readdir(header.nodeid, payload, true)
                .map(DispatchReply::Payload),
            FUSE_RELEASEDIR => {
                self.release(payload, true)?;
                Ok(DispatchReply::Empty)
            }
            FUSE_FSYNCDIR => Ok(DispatchReply::Empty),
            FUSE_STATFS => self.statfs().map(DispatchReply::Payload),
            FUSE_ACCESS => self
                .access(header.nodeid, payload)
                .map(|()| DispatchReply::Empty),
            FUSE_FALLOCATE => {
                self.fallocate(payload)?;
                Ok(DispatchReply::Empty)
            }
            FUSE_LSEEK => self.lseek(payload).map(DispatchReply::Payload),
            FUSE_SYNCFS => {
                self.syncfs()?;
                Ok(DispatchReply::Empty)
            }
            FUSE_DESTROY => {
                self.protocol_reset();
                Ok(DispatchReply::None)
            }
            FUSE_INTERRUPT => Ok(DispatchReply::Empty),
            FUSE_SETXATTR | FUSE_GETXATTR | FUSE_LISTXATTR | FUSE_REMOVEXATTR => Err(EOPNOTSUPP),
            FUSE_GETLK | FUSE_SETLK | FUSE_SETLKW | FUSE_IOCTL | FUSE_POLL
            | FUSE_COPY_FILE_RANGE => Err(ENOSYS),
            opcode => {
                debug!("virtio-fs: unsupported FUSE opcode {opcode}");
                Err(ENOSYS)
            }
        }
    }

    fn init(&mut self, payload: &[u8]) -> FuseResult<Vec<u8>> {
        let mut input = WireCursor::new(payload);
        let major = input.u32()?;
        let minor = input.u32()?;
        let max_readahead = input.u32()?;
        let _flags = input.u32()?;
        if major != FUSE_MAJOR {
            let mut output = Vec::new();
            put_u32(&mut output, FUSE_MAJOR);
            put_u32(&mut output, FUSE_MINOR);
            return Ok(output);
        }
        let negotiated_minor = minor.min(FUSE_MINOR);
        let flags = FUSE_ASYNC_READ
            | FUSE_BIG_WRITES
            | FUSE_AUTO_INVAL_DATA
            | FUSE_DO_READDIRPLUS
            | FUSE_MAX_PAGES;
        let mut output = Vec::with_capacity(64);
        put_u32(&mut output, FUSE_MAJOR);
        put_u32(&mut output, negotiated_minor);
        put_u32(&mut output, max_readahead);
        put_u32(&mut output, flags);
        put_u16(&mut output, 64);
        put_u16(&mut output, 48);
        put_u32(&mut output, MAX_WRITE);
        put_u32(&mut output, 1);
        put_u16(&mut output, (MAX_WRITE / 4096) as u16);
        put_u16(&mut output, 0);
        put_u32(&mut output, 0);
        output.resize(64, 0);
        self.initialized = true;
        Ok(output)
    }

    fn lookup(&mut self, parent: u64, payload: &[u8]) -> FuseResult<Vec<u8>> {
        let name = one_name(payload)?;
        let location = self.child_location(parent, &name)?;
        let file =
            open_child_identity(&location.parent, &self.root, &location.path, &location.name)
                .map_err(io_errno)?;
        let metadata = file.metadata().map_err(io_errno)?;
        let identity = file_identity(&file).map_err(io_errno)?;
        let nodeid = self.intern_identity(location.path, 1, identity)?;
        Ok(entry_out(nodeid, &metadata))
    }

    fn forget(&mut self, nodeid: u64, payload: &[u8]) -> FuseResult<()> {
        let mut input = WireCursor::new(payload);
        self.drop_lookups(nodeid, input.u64()?);
        Ok(())
    }

    fn batch_forget(&mut self, payload: &[u8]) -> FuseResult<()> {
        let mut input = WireCursor::new(payload);
        let count = input.u32()? as usize;
        let _dummy = input.u32()?;
        for _ in 0..count {
            let nodeid = input.u64()?;
            let lookups = input.u64()?;
            self.drop_lookups(nodeid, lookups);
        }
        Ok(())
    }

    fn getattr(&self, nodeid: u64, payload: &[u8]) -> FuseResult<Vec<u8>> {
        let mut input = WireCursor::new(payload);
        let flags = input.u32().unwrap_or(0);
        let _dummy = input.u32().unwrap_or(0);
        let file_handle = input.u64().unwrap_or(0);
        let metadata = if flags & 1 != 0 {
            match self.handles.get(&file_handle) {
                Some(Handle::File {
                    file,
                    nodeid: handle_node,
                    ..
                }) if *handle_node == nodeid => file.metadata().map_err(io_errno)?,
                Some(Handle::Directory {
                    file,
                    nodeid: handle_node,
                    ..
                }) if *handle_node == nodeid => file.metadata().map_err(io_errno)?,
                None => return Err(EBADF),
                _ => return Err(EBADF),
            }
        } else {
            self.resolve_node(nodeid)?.1.metadata().map_err(io_errno)?
        };
        Ok(attr_out(nodeid, &metadata))
    }

    fn setattr(&mut self, nodeid: u64, payload: &[u8]) -> FuseResult<Vec<u8>> {
        self.require_writable()?;
        let mut input = WireCursor::new(payload);
        let valid = input.u32()?;
        let _padding = input.u32()?;
        let file_handle = input.u64()?;
        let size = input.u64()?;
        let _lock_owner = input.u64()?;
        let atime = input.u64()?;
        let mtime = input.u64()?;
        let _ctime = input.u64()?;
        let atimensec = input.u32()?;
        let mtimensec = input.u32()?;
        let _ctimensec = input.u32()?;
        let mode = input.u32()?;
        let _unused = input.u32()?;
        let uid = input.u32()?;
        let gid = input.u32()?;

        if valid & FATTR_SIZE != 0 {
            if valid & FATTR_FH != 0 {
                match self.handles.get(&file_handle) {
                    Some(Handle::File {
                        file,
                        nodeid: handle_node,
                        ..
                    }) if *handle_node == nodeid => file.set_len(size).map_err(io_errno)?,
                    _ => return Err(EBADF),
                }
            } else {
                self.open_node_file(nodeid, O_WRONLY)?
                    .1
                    .set_len(size)
                    .map_err(io_errno)?;
            }
        }
        let needs_attributes = valid
            & (FATTR_MODE
                | FATTR_UID
                | FATTR_GID
                | FATTR_ATIME
                | FATTR_MTIME
                | FATTR_ATIME_NOW
                | FATTR_MTIME_NOW)
            != 0;
        let attribute_file = needs_attributes
            .then(|| self.open_node_attribute(nodeid))
            .transpose()?
            .map(|(_, file)| file);
        if valid & FATTR_MODE != 0 {
            setattr_mode(attribute_file.as_ref().ok_or(EIO)?, mode).map_err(io_errno)?;
        }
        if valid & (FATTR_UID | FATTR_GID) != 0 {
            set_owner(
                attribute_file.as_ref().ok_or(EIO)?,
                (valid & FATTR_UID != 0).then_some(uid),
                (valid & FATTR_GID != 0).then_some(gid),
            )
            .map_err(io_errno)?;
        }
        if valid & (FATTR_ATIME | FATTR_MTIME | FATTR_ATIME_NOW | FATTR_MTIME_NOW) != 0 {
            let file = attribute_file.as_ref().ok_or(EIO)?;
            let metadata = file.metadata().map_err(io_errno)?;
            let current_atime = metadata.accessed().unwrap_or(UNIX_EPOCH);
            let current_mtime = metadata.modified().unwrap_or(UNIX_EPOCH);
            let now = SystemTime::now();
            let new_atime = if valid & FATTR_ATIME_NOW != 0 {
                now
            } else if valid & FATTR_ATIME != 0 {
                wire_time(atime, atimensec)?
            } else {
                current_atime
            };
            let new_mtime = if valid & FATTR_MTIME_NOW != 0 {
                now
            } else if valid & FATTR_MTIME != 0 {
                wire_time(mtime, mtimensec)?
            } else {
                current_mtime
            };
            set_times(file, new_atime, new_mtime).map_err(io_errno)?;
        }
        let metadata = match attribute_file {
            Some(file) => file.metadata().map_err(io_errno)?,
            None => self.resolve_node(nodeid)?.1.metadata().map_err(io_errno)?,
        };
        Ok(attr_out(nodeid, &metadata))
    }

    fn readlink(&self, nodeid: u64) -> FuseResult<Vec<u8>> {
        let (path, file) = self.resolve_node(nodeid)?;
        let target = read_link_node(&file, &self.root, &path).map_err(io_errno)?;
        Ok(os_str_bytes(target.as_os_str()))
    }

    fn symlink(&mut self, parent: u64, payload: &[u8]) -> FuseResult<Vec<u8>> {
        self.require_writable()?;
        let mut input = WireCursor::new(payload);
        let name = input.name()?;
        let target = input.name_unchecked()?;
        let location = self.child_location(parent, &name)?;
        create_symlink_at(
            &location.parent,
            &self.root,
            &target,
            &location.path,
            &location.name,
        )
        .map_err(io_errno)?;
        let file =
            open_child_identity(&location.parent, &self.root, &location.path, &location.name)
                .map_err(io_errno)?;
        let metadata = file.metadata().map_err(io_errno)?;
        let identity = file_identity(&file).map_err(io_errno)?;
        let nodeid = self.intern_identity(location.path, 1, identity)?;
        Ok(entry_out(nodeid, &metadata))
    }

    fn mknod(&mut self, parent: u64, payload: &[u8]) -> FuseResult<Vec<u8>> {
        self.require_writable()?;
        let mut input = WireCursor::new(payload);
        let mode = input.u32()?;
        let _rdev = input.u32()?;
        let _umask = input.u32().unwrap_or(0);
        let _padding = input.u32().unwrap_or(0);
        let name = input.name()?;
        if mode & S_IFMT != 0 && mode & S_IFMT != S_IFREG {
            return Err(EOPNOTSUPP);
        }
        let location = self.child_location(parent, &name)?;
        let file = open_child_file(
            &location.parent,
            &self.root,
            &location.path,
            &location.name,
            O_WRONLY | O_CREAT | O_EXCL,
            true,
            mode,
        )
        .map_err(io_errno)?;
        set_created_mode_beneath(&self.root_handle, &self.root, &location.path, &file, mode)
            .map_err(io_errno)?;
        let metadata = file.metadata().map_err(io_errno)?;
        let identity = file_identity(&file).map_err(io_errno)?;
        let nodeid = self.intern_identity(location.path, 1, identity)?;
        Ok(entry_out(nodeid, &metadata))
    }

    fn mkdir(&mut self, parent: u64, payload: &[u8]) -> FuseResult<Vec<u8>> {
        self.require_writable()?;
        let mut input = WireCursor::new(payload);
        let mode = input.u32()?;
        let _umask = input.u32()?;
        let name = input.name()?;
        let location = self.child_location(parent, &name)?;
        create_directory_at(
            &location.parent,
            &self.root,
            &location.path,
            &location.name,
            mode,
        )
        .map_err(io_errno)?;
        let file =
            open_child_directory(&location.parent, &self.root, &location.path, &location.name)
                .map_err(io_errno)?;
        set_created_mode_beneath(&self.root_handle, &self.root, &location.path, &file, mode)
            .map_err(io_errno)?;
        let metadata = file.metadata().map_err(io_errno)?;
        let identity = file_identity(&file).map_err(io_errno)?;
        let nodeid = self.intern_identity(location.path, 1, identity)?;
        Ok(entry_out(nodeid, &metadata))
    }

    fn unlink(&mut self, parent: u64, payload: &[u8], directory: bool) -> FuseResult<()> {
        self.require_writable()?;
        let name = one_name(payload)?;
        let location = self.child_location(parent, &name)?;
        unlink_at(
            &location.parent,
            &self.root,
            &location.path,
            &location.name,
            directory,
        )
        .map_err(io_errno)?;
        self.retire_path(&location.path);
        Ok(())
    }

    fn rename(&mut self, old_parent: u64, payload: &[u8], extended: bool) -> FuseResult<()> {
        self.require_writable()?;
        let mut input = WireCursor::new(payload);
        let new_parent = input.u64()?;
        let flags = if extended {
            let flags = input.u32()?;
            let _padding = input.u32()?;
            flags
        } else {
            0
        };
        if flags & !RENAME_NOREPLACE != 0 {
            return Err(EINVAL);
        }
        let old_name = input.name()?;
        let new_name = input.name()?;
        let old_location = self.child_location(old_parent, &old_name)?;
        let new_location = self.child_location(new_parent, &new_name)?;
        let source_node = self.by_path.get(&old_location.path).copied();
        let destination_node = self.by_path.get(&new_location.path).copied();
        rename_at(
            &old_location.parent,
            &new_location.parent,
            &self.root,
            &old_location.path,
            &new_location.path,
            &old_location.name,
            &new_location.name,
            flags & RENAME_NOREPLACE != 0,
        )
        .map_err(io_errno)?;
        if source_node == destination_node && source_node.is_some() {
            return Ok(());
        }
        self.retire_path(&new_location.path);
        self.rebase_paths(&old_location.path, &new_location.path);
        Ok(())
    }

    fn link(&mut self, new_parent: u64, payload: &[u8]) -> FuseResult<Vec<u8>> {
        self.require_writable()?;
        let mut input = WireCursor::new(payload);
        let old_nodeid = input.u64()?;
        let name = input.name()?;
        let (old_path, source) = self.resolve_node(old_nodeid)?;
        let new_location = self.child_location(new_parent, &name)?;
        hard_link_at(
            &source,
            &new_location.parent,
            &self.root,
            &old_path,
            &new_location.path,
            &new_location.name,
        )
        .map_err(io_errno)?;
        let metadata = source.metadata().map_err(io_errno)?;
        self.by_path.insert(new_location.path, old_nodeid);
        let node = self.nodes.get_mut(&old_nodeid).ok_or(ENOENT)?;
        node.lookups = node.lookups.saturating_add(1);
        Ok(entry_out(old_nodeid, &metadata))
    }

    fn open(&mut self, nodeid: u64, payload: &[u8]) -> FuseResult<Vec<u8>> {
        let mut input = WireCursor::new(payload);
        let flags = input.u32()?;
        self.check_open_mode(flags)?;
        let (path, file) = self.open_node_file(nodeid, flags)?;
        let handle = self.insert_handle(Handle::File {
            file,
            path,
            flags,
            nodeid,
        });
        Ok(open_out(handle, FOPEN_DIRECT_IO))
    }

    fn create(&mut self, parent: u64, payload: &[u8]) -> FuseResult<Vec<u8>> {
        self.require_writable()?;
        let mut input = WireCursor::new(payload);
        let flags = input.u32()?;
        let mode = input.u32()?;
        let _umask = input.u32()?;
        let _open_flags = input.u32()?;
        let name = input.name()?;
        let location = self.child_location(parent, &name)?;
        let file = open_child_file(
            &location.parent,
            &self.root,
            &location.path,
            &location.name,
            flags,
            true,
            mode,
        )
        .map_err(io_errno)?;
        set_created_mode_beneath(&self.root_handle, &self.root, &location.path, &file, mode)
            .map_err(io_errno)?;
        let metadata = file.metadata().map_err(io_errno)?;
        let identity = file_identity(&file).map_err(io_errno)?;
        let nodeid = self.intern_identity(location.path.clone(), 1, identity)?;
        let handle = self.insert_handle(Handle::File {
            file,
            path: location.path,
            flags,
            nodeid,
        });
        let mut output = entry_out(nodeid, &metadata);
        output.extend(open_out(handle, FOPEN_DIRECT_IO));
        Ok(output)
    }

    fn read(&mut self, payload: &[u8]) -> FuseResult<Vec<u8>> {
        let mut input = WireCursor::new(payload);
        let handle = input.u64()?;
        let offset = input.u64()?;
        let size = input.u32()? as usize;
        let (file, path) = match self.handles.get_mut(&handle) {
            Some(Handle::File { file, path, .. }) => (file, path),
            _ => return Err(EBADF),
        };
        file.seek(SeekFrom::Start(offset)).map_err(io_errno)?;
        let mut output = vec![0u8; size.min(MAX_WRITE as usize)];
        let count = file.read(&mut output).map_err(io_errno)?;
        output.truncate(count);
        log::trace!(
            "virtio-fs: read {path:?} offset {offset} -> {:?}",
            String::from_utf8_lossy(&output[..output.len().min(64)])
        );
        Ok(output)
    }

    fn write(&mut self, payload: &[u8]) -> FuseResult<Vec<u8>> {
        self.require_writable()?;
        let mut input = WireCursor::new(payload);
        let handle = input.u64()?;
        let offset = input.u64()?;
        let size = input.u32()? as usize;
        let _write_flags = input.u32()?;
        let _lock_owner = input.u64()?;
        let _flags = input.u32()?;
        let _padding = input.u32()?;
        let data = input.take(size)?;
        let file = match self.handles.get_mut(&handle) {
            Some(Handle::File { file, .. }) => file,
            _ => return Err(EBADF),
        };
        file.seek(SeekFrom::Start(offset)).map_err(io_errno)?;
        let count = file.write(data).map_err(io_errno)?;
        let mut output = Vec::with_capacity(8);
        put_u32(&mut output, count as u32);
        put_u32(&mut output, 0);
        Ok(output)
    }

    fn flush(&self, payload: &[u8]) -> FuseResult<()> {
        let mut input = WireCursor::new(payload);
        let handle = input.u64()?;
        match self.handles.get(&handle) {
            Some(Handle::File { .. }) => Ok(()),
            _ => Err(EBADF),
        }
    }

    fn fsync(&self, payload: &[u8]) -> FuseResult<()> {
        let mut input = WireCursor::new(payload);
        let handle = input.u64()?;
        let fsync_flags = input.u32()?;
        match self.handles.get(&handle) {
            Some(Handle::File { flags, .. }) if flags & O_ACCMODE == 0 => Ok(()),
            Some(Handle::File { file, .. }) if fsync_flags & 1 != 0 => {
                file.sync_data().map_err(io_errno)
            }
            Some(Handle::File { file, .. }) => file.sync_all().map_err(io_errno),
            _ => Err(EBADF),
        }
    }

    fn release(&mut self, payload: &[u8], directory: bool) -> FuseResult<()> {
        let mut input = WireCursor::new(payload);
        let handle = input.u64()?;
        match self.handles.remove(&handle) {
            Some(Handle::Directory { .. }) if directory => Ok(()),
            Some(Handle::File { .. }) if !directory => Ok(()),
            Some(other) => {
                self.handles.insert(handle, other);
                Err(EBADF)
            }
            None => Err(EBADF),
        }
    }

    fn opendir(&mut self, nodeid: u64) -> FuseResult<Vec<u8>> {
        let (path, file) = self.resolve_directory(nodeid)?;
        if !file.metadata().map_err(io_errno)?.is_dir() {
            return Err(ENOTDIR);
        }
        let handle = self.insert_handle(Handle::Directory {
            file,
            path,
            nodeid,
            entries: None,
        });
        Ok(open_out(handle, 0))
    }

    fn readdir(&mut self, nodeid: u64, payload: &[u8], plus: bool) -> FuseResult<Vec<u8>> {
        let mut input = WireCursor::new(payload);
        let handle = input.u64()?;
        let offset = input.u64()? as usize;
        let max_size = input.u32()? as usize;
        let (path, cached_entries, directory_file) = match self.handles.get(&handle) {
            Some(Handle::Directory {
                file,
                path,
                entries,
                ..
            }) => (
                path.clone(),
                entries.clone(),
                entries
                    .is_none()
                    .then(|| file.try_clone())
                    .transpose()
                    .map_err(io_errno)?,
            ),
            _ => return Err(EBADF),
        };
        if self.node_path(nodeid)? != path {
            return Err(EBADF);
        }
        self.ensure_confined(&path, true)?;

        let names = match cached_entries {
            Some(entries) => entries,
            None => {
                let mut children =
                    read_directory_names(directory_file.as_ref().ok_or(EIO)?, &self.root, &path)
                        .map_err(io_errno)?;
                children.sort_by_key(|name| os_str_bytes(name));
                let mut entries = vec![OsString::from("."), OsString::from("..")];
                entries.extend(children);
                match self.handles.get_mut(&handle) {
                    Some(Handle::Directory {
                        entries: cached, ..
                    }) => {
                        *cached = Some(entries.clone());
                    }
                    _ => return Err(EBADF),
                }
                entries
            }
        };

        let mut output = Vec::new();
        for (index, name) in names.into_iter().enumerate().skip(offset) {
            let is_dot = index < 2;
            let entry_path = if index == 0 {
                path.clone()
            } else if index == 1 {
                path.parent().unwrap_or(Path::new("")).to_path_buf()
            } else {
                path.join(&name)
            };
            let metadata = self.host_metadata(&entry_path)?;
            let known_nodeid = self.by_path.get(&entry_path).copied();
            let name_bytes = os_str_bytes(&name);
            let record_size = align_up((if plus { 128 } else { 0 }) + 24 + name_bytes.len(), 8);
            if output.len() + record_size > max_size {
                break;
            }
            let entry_nodeid = if plus {
                match known_nodeid {
                    Some(nodeid) if !is_dot => {
                        self.add_lookup(nodeid, 1)?;
                        nodeid
                    }
                    Some(nodeid) => nodeid,
                    None if index == 0 => nodeid,
                    None if index == 1 => FUSE_ROOT_ID,
                    None => self.intern(entry_path, 1)?,
                }
            } else {
                known_nodeid.unwrap_or(0)
            };
            let mut record = Vec::new();
            if plus {
                record.extend(entry_out(entry_nodeid, &metadata));
            }
            put_u64(&mut record, entry_nodeid);
            put_u64(&mut record, (index + 1) as u64);
            put_u32(&mut record, name_bytes.len() as u32);
            put_u32(&mut record, directory_type(&metadata));
            record.extend(name_bytes);
            record.resize(align_up(record.len(), 8), 0);
            output.extend(record);
        }
        Ok(output)
    }

    fn statfs(&self) -> FuseResult<Vec<u8>> {
        let statistics = host_statfs(&self.root_handle, &self.root).map_err(io_errno)?;
        let mut output = Vec::with_capacity(80);
        put_u64(&mut output, statistics.blocks);
        put_u64(&mut output, statistics.free_blocks);
        put_u64(&mut output, statistics.available_blocks);
        put_u64(&mut output, statistics.files);
        put_u64(&mut output, statistics.free_files);
        put_u32(&mut output, statistics.block_size);
        put_u32(&mut output, 255);
        put_u32(&mut output, statistics.block_size);
        put_u32(&mut output, 0);
        output.resize(80, 0);
        Ok(output)
    }

    fn access(&self, nodeid: u64, payload: &[u8]) -> FuseResult<()> {
        let mut input = WireCursor::new(payload);
        let mask = input.u32()?;
        if mask & 2 != 0 && !self.writable {
            return Err(EROFS);
        }
        self.resolve_node(nodeid)?.1.metadata().map_err(io_errno)?;
        Ok(())
    }

    fn fallocate(&mut self, payload: &[u8]) -> FuseResult<()> {
        self.require_writable()?;
        let mut input = WireCursor::new(payload);
        let handle = input.u64()?;
        let offset = input.u64()?;
        let length = input.u64()?;
        let mode = input.u32()?;
        if mode != 0 {
            return Err(EOPNOTSUPP);
        }
        let end = offset.checked_add(length).ok_or(EINVAL)?;
        match self.handles.get(&handle) {
            Some(Handle::File { file, .. }) => {
                let current = file.metadata().map_err(io_errno)?.len();
                if end > current {
                    file.set_len(end).map_err(io_errno)?;
                }
                Ok(())
            }
            _ => Err(EBADF),
        }
    }

    fn lseek(&mut self, payload: &[u8]) -> FuseResult<Vec<u8>> {
        let mut input = WireCursor::new(payload);
        let handle = input.u64()?;
        let offset = input.u64()?;
        let whence = input.u32()?;
        let file = match self.handles.get_mut(&handle) {
            Some(Handle::File { file, .. }) => file,
            _ => return Err(EBADF),
        };
        let new_offset = match whence {
            0 => offset,
            1 => file
                .stream_position()
                .map_err(io_errno)?
                .checked_add(offset)
                .ok_or(EINVAL)?,
            2 => file
                .metadata()
                .map_err(io_errno)?
                .len()
                .checked_add(offset)
                .ok_or(EINVAL)?,
            3 => {
                let len = file.metadata().map_err(io_errno)?.len();
                if offset >= len {
                    return Err(ENXIO);
                }
                offset
            }
            4 => file.metadata().map_err(io_errno)?.len().max(offset),
            _ => return Err(EINVAL),
        };
        let mut output = Vec::with_capacity(8);
        put_u64(&mut output, new_offset);
        Ok(output)
    }

    fn syncfs(&self) -> FuseResult<()> {
        for handle in self.handles.values() {
            if let Handle::File { file, .. } = handle {
                file.sync_all().map_err(io_errno)?;
            }
        }
        Ok(())
    }

    fn check_open_mode(&self, flags: u32) -> FuseResult<()> {
        if !self.writable && (flags & O_ACCMODE != 0 || flags & O_TRUNC != 0) {
            return Err(EROFS);
        }
        Ok(())
    }

    fn require_writable(&self) -> FuseResult<()> {
        if self.writable { Ok(()) } else { Err(EROFS) }
    }

    fn resolve_node(&self, nodeid: u64) -> FuseResult<(PathBuf, File)> {
        let node = self.nodes.get(&nodeid).ok_or(ENOENT)?;
        let mut aliases: Vec<PathBuf> = self
            .by_path
            .iter()
            .filter_map(|(path, mapped)| (*mapped == nodeid).then(|| path.clone()))
            .collect();
        aliases.sort_by_key(|path| if *path == node.path { 0 } else { 1 });
        for path in aliases {
            let Ok(file) = open_identity_beneath(&self.root_handle, &self.root, &path) else {
                continue;
            };
            if file_identity(&file).ok() == Some(node.identity) {
                return Ok((path, file));
            }
        }
        Err(ESTALE)
    }

    fn node_path(&self, nodeid: u64) -> FuseResult<PathBuf> {
        self.resolve_node(nodeid).map(|(path, _)| path)
    }

    fn resolve_directory(&self, nodeid: u64) -> FuseResult<(PathBuf, File)> {
        let (path, file) = self.resolve_node(nodeid)?;
        if !file.metadata().map_err(io_errno)?.is_dir() {
            return Err(ENOTDIR);
        }
        Ok((path, file))
    }

    fn child_location(&self, parent: u64, name: &OsStr) -> FuseResult<ChildLocation> {
        validate_name(name)?;
        let (parent_path, parent_file) = self.resolve_directory(parent)?;
        Ok(ChildLocation {
            parent: parent_file,
            path: parent_path.join(name),
            name: name.to_os_string(),
        })
    }

    fn absolute(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root.join(relative)
    }

    fn open_host_file(
        &self,
        relative: &Path,
        flags: u32,
        create: bool,
        mode: u32,
    ) -> ::std::io::Result<File> {
        open_beneath(&self.root_handle, &self.root, relative, flags, create, mode)
    }

    fn open_host_directory(&self, relative: &Path) -> ::std::io::Result<File> {
        open_directory_beneath(&self.root_handle, &self.root, relative)
    }

    fn open_node_file(&self, nodeid: u64, flags: u32) -> FuseResult<(PathBuf, File)> {
        let (path, identity_file) = self.resolve_node(nodeid)?;
        let open_flags = flags & !O_TRUNC;
        let file = self
            .open_host_file(&path, open_flags, false, 0)
            .map_err(io_errno)?;
        if !same_file(&identity_file, &file).map_err(io_errno)? {
            return Err(ESTALE);
        }
        if flags & O_TRUNC != 0 {
            file.set_len(0).map_err(io_errno)?;
        }
        Ok((path, file))
    }

    fn open_node_attribute(&self, nodeid: u64) -> FuseResult<(PathBuf, File)> {
        let (path, identity_file) = self.resolve_node(nodeid)?;
        let file =
            open_attribute_beneath(&self.root_handle, &self.root, &path).map_err(io_errno)?;
        if !same_file(&identity_file, &file).map_err(io_errno)? {
            return Err(ESTALE);
        }
        Ok((path, file))
    }

    fn ensure_confined(&self, relative: &Path, include_final: bool) -> FuseResult<()> {
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return Err(EACCES);
        }
        let _ = include_final;
        Ok(())
    }

    fn intern(&mut self, path: PathBuf, lookups: u64) -> FuseResult<u64> {
        let identity = self.path_identity(&path)?;
        self.intern_identity(path, lookups, identity)
    }

    fn intern_identity(
        &mut self,
        path: PathBuf,
        lookups: u64,
        identity: FileIdentity,
    ) -> FuseResult<u64> {
        if let Some(&nodeid) = self.by_path.get(&path) {
            if self
                .nodes
                .get(&nodeid)
                .is_some_and(|node| node.identity == identity)
            {
                let node = self.nodes.get_mut(&nodeid).ok_or(EIO)?;
                node.lookups = node.lookups.saturating_add(lookups);
                return Ok(nodeid);
            }
            self.retire_path(&path);
        }
        let nodeid = self.next_nodeid;
        self.next_nodeid = self.next_nodeid.saturating_add(1);
        self.by_path.insert(path.clone(), nodeid);
        self.nodes.insert(
            nodeid,
            Node {
                path,
                lookups,
                identity,
            },
        );
        Ok(nodeid)
    }

    fn path_identity(&self, path: &Path) -> FuseResult<FileIdentity> {
        let file = open_identity_beneath(&self.root_handle, &self.root, path).map_err(io_errno)?;
        file_identity(&file).map_err(io_errno)
    }

    fn host_metadata(&self, path: &Path) -> FuseResult<Metadata> {
        let file = open_identity_beneath(&self.root_handle, &self.root, path).map_err(io_errno)?;
        file.metadata().map_err(io_errno)
    }

    fn add_lookup(&mut self, nodeid: u64, count: u64) -> FuseResult<()> {
        let node = self.nodes.get_mut(&nodeid).ok_or(ENOENT)?;
        node.lookups = node.lookups.saturating_add(count);
        Ok(())
    }

    fn drop_lookups(&mut self, nodeid: u64, count: u64) {
        if nodeid == FUSE_ROOT_ID {
            return;
        }
        let remove = match self.nodes.get_mut(&nodeid) {
            Some(node) => {
                node.lookups = node.lookups.saturating_sub(count);
                node.lookups == 0
            }
            None => false,
        };
        if remove {
            self.nodes.remove(&nodeid);
            self.by_path.retain(|_, mapped| *mapped != nodeid);
        }
    }

    fn retire_path(&mut self, path: &Path) {
        let Some(nodeid) = self.by_path.remove(path) else {
            return;
        };
        let replacement = self
            .by_path
            .iter()
            .find_map(|(alias, mapped)| (*mapped == nodeid).then(|| alias.clone()));
        match (self.nodes.get_mut(&nodeid), replacement) {
            (Some(node), Some(alias)) if node.path == path => node.path = alias,
            (Some(node), None) if node.path == path => {
                self.nodes.remove(&nodeid);
            }
            _ => {}
        }
    }

    fn insert_handle(&mut self, handle: Handle) -> u64 {
        let id = self.next_handle;
        self.next_handle = self.next_handle.saturating_add(1);
        self.handles.insert(id, handle);
        id
    }

    fn rebase_paths(&mut self, old_path: &Path, new_path: &Path) {
        for node in self.nodes.values_mut() {
            if let Ok(suffix) = node.path.strip_prefix(old_path) {
                node.path = new_path.join(suffix);
            }
        }
        for handle in self.handles.values_mut() {
            let path = match handle {
                Handle::File { path, .. } | Handle::Directory { path, .. } => path,
            };
            if let Ok(suffix) = path.strip_prefix(old_path) {
                *path = new_path.join(suffix);
            }
        }
        let aliases = ::core::mem::take(&mut self.by_path);
        for (path, nodeid) in aliases {
            let path = match path.strip_prefix(old_path) {
                Ok(suffix) => new_path.join(suffix),
                Err(_) => path,
            };
            self.by_path.insert(path, nodeid);
        }
    }

    fn protocol_reset(&mut self) {
        self.nodes.clear();
        self.by_path.clear();
        self.handles.clear();
        self.next_nodeid = FUSE_ROOT_ID + 1;
        self.next_handle = 1;
        self.initialized = false;
        self.install_root();
    }

    fn save_state(&self) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        put_u32(&mut output, STATE_VERSION);
        output.push(u8::from(self.initialized));
        put_u64(&mut output, self.next_nodeid);
        put_u64(&mut output, self.next_handle);

        let mut nodes: Vec<_> = self.nodes.iter().collect();
        nodes.sort_by_key(|(nodeid, _)| **nodeid);
        put_u32(&mut output, nodes.len() as u32);
        for (&nodeid, node) in nodes {
            put_u64(&mut output, nodeid);
            put_u64(&mut output, node.lookups);
            put_bytes(&mut output, &os_str_bytes(node.path.as_os_str()));
        }

        let mut aliases: Vec<_> = self.by_path.iter().collect();
        aliases.sort_by(|(left, _), (right, _)| {
            os_str_bytes(left.as_os_str()).cmp(&os_str_bytes(right.as_os_str()))
        });
        put_u32(&mut output, aliases.len() as u32);
        for (path, &nodeid) in aliases {
            let identity = self.path_identity(path).map_err(|errno| {
                ::anyhow::anyhow!("virtio-fs alias {path:?} cannot be validated: errno {errno}")
            })?;
            if self.nodes.get(&nodeid).map(|node| node.identity) != Some(identity) {
                bail!(
                    "virtio-fs alias {path:?} no longer identifies node {nodeid}; refusing snapshot"
                );
            }
            put_bytes(&mut output, &os_str_bytes(path.as_os_str()));
            put_u64(&mut output, nodeid);
        }

        let mut handles: Vec<_> = self.handles.iter().collect();
        handles.sort_by_key(|(handle, _)| **handle);
        put_u32(&mut output, handles.len() as u32);
        for (&handle, value) in handles {
            put_u64(&mut output, handle);
            match value {
                Handle::File {
                    file,
                    flags,
                    nodeid,
                    ..
                } => {
                    let path = self.reopen_path(*nodeid)?;
                    let alias = self.open_host_file(&path, 0, false, 0).with_context(|| {
                        format!("opening virtio-fs handle {handle} alias {path:?}")
                    })?;
                    if !same_file(file, &alias)? {
                        bail!(
                            "virtio-fs handle {handle} no longer matches its host path; refusing snapshot"
                        );
                    }
                    output.push(0);
                    put_u32(&mut output, *flags);
                    put_u64(&mut output, *nodeid);
                    put_bytes(&mut output, &os_str_bytes(path.as_os_str()));
                }
                Handle::Directory {
                    file,
                    nodeid,
                    entries,
                    ..
                } => {
                    let path = self.reopen_path(*nodeid)?;
                    let alias = self.open_host_directory(&path).with_context(|| {
                        format!("opening virtio-fs directory handle {handle} alias {path:?}")
                    })?;
                    if !same_file(file, &alias)? {
                        bail!(
                            "virtio-fs directory handle {handle} no longer matches its host path; refusing snapshot"
                        );
                    }
                    output.push(1);
                    put_u32(&mut output, 0);
                    put_u64(&mut output, *nodeid);
                    put_bytes(&mut output, &os_str_bytes(path.as_os_str()));
                    match entries {
                        Some(entries) => {
                            put_u32(&mut output, entries.len() as u32);
                            for name in entries {
                                put_bytes(&mut output, &os_str_bytes(name));
                            }
                        }
                        None => put_u32(&mut output, u32::MAX),
                    }
                }
            }
        }
        Ok(output)
    }

    fn reopen_path(&self, nodeid: u64) -> Result<PathBuf> {
        let path = self
            .by_path
            .iter()
            .find_map(|(path, mapped)| (*mapped == nodeid).then(|| path.clone()))
            .with_context(|| {
                format!(
                    "virtio-fs node {nodeid} has no host name; close unlinked handles before snapshot"
                )
            })?;
        self.ensure_confined(&path, true).map_err(|errno| {
            ::anyhow::anyhow!("virtio-fs node {nodeid} is not confined: errno {errno}")
        })?;
        Ok(path)
    }

    fn load_state(&mut self, state: &[u8]) -> Result<()> {
        let mut input = StateReader::new(state);
        let version = input.u32()?;
        if !(1..=STATE_VERSION).contains(&version) {
            bail!("unsupported virtio-fs protocol state version {version}");
        }
        let initialized = input.u8()? != 0;
        let next_nodeid = input.u64()?;
        let next_handle = input.u64()?;

        let node_count = input.u32()? as usize;
        let mut nodes = HashMap::with_capacity(node_count);
        for _ in 0..node_count {
            let nodeid = input.u64()?;
            let lookups = input.u64()?;
            let path = decode_state_path(input.bytes()?)?;
            let identity = self.path_identity(&path).map_err(|errno| {
                ::anyhow::anyhow!(
                    "restored virtio-fs node {nodeid} path {path:?} is unavailable: errno {errno}"
                )
            })?;
            if nodes
                .insert(
                    nodeid,
                    Node {
                        path,
                        lookups,
                        identity,
                    },
                )
                .is_some()
            {
                bail!("virtio-fs snapshot contains duplicate node state");
            }
        }
        let mut by_path = HashMap::with_capacity(node_count);
        if version >= 2 {
            let alias_count = input.u32()? as usize;
            by_path.reserve(alias_count);
            for _ in 0..alias_count {
                let path = decode_state_path(input.bytes()?)?;
                let nodeid = input.u64()?;
                if !nodes.contains_key(&nodeid) {
                    bail!("virtio-fs snapshot alias references unknown node {nodeid}");
                }
                let identity = self.path_identity(&path).map_err(|errno| {
                    ::anyhow::anyhow!(
                        "restored virtio-fs alias {path:?} is unavailable: errno {errno}"
                    )
                })?;
                if nodes.get(&nodeid).map(|node| node.identity) != Some(identity) {
                    bail!("restored virtio-fs alias {path:?} does not identify node {nodeid}");
                }
                if by_path.insert(path, nodeid).is_some() {
                    bail!("virtio-fs snapshot contains duplicate path aliases");
                }
            }
        } else {
            for (&nodeid, node) in &nodes {
                if by_path.insert(node.path.clone(), nodeid).is_some() {
                    bail!("virtio-fs snapshot contains duplicate node paths");
                }
            }
        }
        if nodes
            .get(&FUSE_ROOT_ID)
            .is_none_or(|node| !node.path.as_os_str().is_empty())
            || by_path.get(Path::new("")) != Some(&FUSE_ROOT_ID)
        {
            bail!("virtio-fs snapshot has no valid root node");
        }

        let handle_count = input.u32()? as usize;
        let mut handles = HashMap::with_capacity(handle_count);
        for _ in 0..handle_count {
            let handle = input.u64()?;
            let kind = input.u8()?;
            let flags = input.u32()?;
            let saved_nodeid = if version >= 3 {
                Some(input.u64()?)
            } else {
                None
            };
            let path = decode_state_path(input.bytes()?)?;
            let nodeid = saved_nodeid
                .or_else(|| by_path.get(&path).copied())
                .with_context(|| format!("virtio-fs handle {handle} has no node identity"))?;
            if by_path.get(&path) != Some(&nodeid) {
                bail!("virtio-fs handle {handle} path does not match node {nodeid}");
            }
            let absolute = self.absolute(&path);
            let value = match kind {
                0 => {
                    self.ensure_confined(&path, true).map_err(|errno| {
                        ::anyhow::anyhow!("reopening virtio-fs handle failed: errno {errno}")
                    })?;
                    let reopen_flags = flags & !(O_CREAT | O_EXCL | O_TRUNC);
                    let file = self
                        .open_host_file(&path, reopen_flags, false, 0)
                        .with_context(|| format!("reopening virtio-fs handle {absolute:?}"))?;
                    Handle::File {
                        file,
                        path,
                        flags,
                        nodeid,
                    }
                }
                1 => {
                    let file = self
                        .open_host_directory(&path)
                        .with_context(|| format!("reopening virtio-fs directory {absolute:?}"))?;
                    let entries = if version >= 4 {
                        match input.u32()? {
                            u32::MAX => None,
                            count => {
                                let mut entries = Vec::with_capacity(count as usize);
                                for _ in 0..count {
                                    entries.push(os_string(input.bytes()?).map_err(|_| {
                                        ::anyhow::anyhow!("invalid virtio-fs directory entry")
                                    })?);
                                }
                                Some(entries)
                            }
                        }
                    } else {
                        None
                    };
                    Handle::Directory {
                        file,
                        path,
                        nodeid,
                        entries,
                    }
                }
                _ => bail!("virtio-fs snapshot has invalid handle type {kind}"),
            };
            if handles.insert(handle, value).is_some() {
                bail!("virtio-fs snapshot contains duplicate handle {handle}");
            }
        }
        if !input.is_empty() {
            bail!("virtio-fs protocol snapshot has trailing bytes");
        }

        self.nodes = nodes;
        self.by_path = by_path;
        self.handles = handles;
        self.next_nodeid = next_nodeid;
        self.next_handle = next_handle;
        self.initialized = initialized;
        Ok(())
    }
}

impl RequestHandler for PassthroughFs {
    fn handle(&mut self, request: &[u8]) -> Option<Vec<u8>> {
        let (header, payload) = match decode_request(request) {
            Ok(decoded) => decoded,
            Err(error) => {
                warn!("virtio-fs: malformed FUSE request: errno {error}");
                return None;
            }
        };
        match self.dispatch(&header, payload) {
            Ok(DispatchReply::Payload(payload)) => Some(success_reply(header.unique, &payload)),
            Ok(DispatchReply::Empty) => Some(success_reply(header.unique, &[])),
            Ok(DispatchReply::None) => None,
            Err(errno) => {
                debug!(
                    "virtio-fs: FUSE opcode {} node {} failed with errno {errno}",
                    header.opcode, header.nodeid
                );
                Some(error_reply(header.unique, errno))
            }
        }
    }

    fn reset(&mut self) {
        self.protocol_reset();
    }

    fn save(&self) -> Result<Vec<u8>> {
        self.save_state()
    }

    fn load(&mut self, state: &[u8]) -> Result<()> {
        self.load_state(state)
    }
}

fn decode_request(request: &[u8]) -> FuseResult<(Header, &[u8])> {
    let mut input = WireCursor::new(request);
    let length = input.u32()? as usize;
    let opcode = input.u32()?;
    let unique = input.u64()?;
    let nodeid = input.u64()?;
    let _uid = input.u32()?;
    let _gid = input.u32()?;
    let _pid = input.u32()?;
    let _padding = input.u32()?;
    if length < FUSE_HEADER_LEN || length > request.len() {
        return Err(EINVAL);
    }
    Ok((
        Header {
            opcode,
            unique,
            nodeid,
        },
        &request[FUSE_HEADER_LEN..length],
    ))
}

fn success_reply(unique: u64, payload: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(FUSE_OUT_HEADER_LEN + payload.len());
    put_u32(&mut output, (FUSE_OUT_HEADER_LEN + payload.len()) as u32);
    put_i32(&mut output, 0);
    put_u64(&mut output, unique);
    output.extend(payload);
    output
}

fn error_reply(unique: u64, errno: i32) -> Vec<u8> {
    let mut output = Vec::with_capacity(FUSE_OUT_HEADER_LEN);
    put_u32(&mut output, FUSE_OUT_HEADER_LEN as u32);
    put_i32(&mut output, -errno.abs());
    put_u64(&mut output, unique);
    output
}

fn entry_out(nodeid: u64, metadata: &Metadata) -> Vec<u8> {
    let mut output = Vec::with_capacity(128);
    put_u64(&mut output, nodeid);
    put_u64(&mut output, 1);
    put_u64(&mut output, 0);
    put_u64(&mut output, 0);
    put_u32(&mut output, 0);
    put_u32(&mut output, 0);
    metadata_attr(metadata, nodeid).encode(&mut output);
    output
}

fn attr_out(nodeid: u64, metadata: &Metadata) -> Vec<u8> {
    let mut output = Vec::with_capacity(104);
    put_u64(&mut output, 0);
    put_u32(&mut output, 0);
    put_u32(&mut output, 0);
    metadata_attr(metadata, nodeid).encode(&mut output);
    output
}

fn open_out(handle: u64, flags: u32) -> Vec<u8> {
    let mut output = Vec::with_capacity(16);
    put_u64(&mut output, handle);
    put_u32(&mut output, flags);
    put_i32(&mut output, 0);
    output
}

#[cfg(target_os = "linux")]
fn metadata_attr(metadata: &Metadata, nodeid: u64) -> Attr {
    use ::std::os::unix::fs::MetadataExt;
    Attr {
        ino: nodeid,
        size: metadata.size(),
        blocks: metadata.blocks(),
        atime: metadata.atime().max(0) as u64,
        mtime: metadata.mtime().max(0) as u64,
        ctime: metadata.ctime().max(0) as u64,
        atimensec: metadata.atime_nsec().max(0) as u32,
        mtimensec: metadata.mtime_nsec().max(0) as u32,
        ctimensec: metadata.ctime_nsec().max(0) as u32,
        mode: metadata.mode(),
        nlink: metadata.nlink() as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: metadata.rdev() as u32,
        blksize: metadata.blksize() as u32,
        flags: 0,
    }
}

#[cfg(target_os = "windows")]
fn metadata_attr(metadata: &Metadata, nodeid: u64) -> Attr {
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        S_IFLNK
    } else if file_type.is_dir() {
        S_IFDIR
    } else {
        S_IFREG
    };
    let permissions = if file_type.is_dir() {
        0o755
    } else if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    };
    let (atime, atimensec) = system_time_parts(metadata.accessed().unwrap_or(UNIX_EPOCH));
    let (mtime, mtimensec) = system_time_parts(metadata.modified().unwrap_or(UNIX_EPOCH));
    let (ctime, ctimensec) = system_time_parts(metadata.created().unwrap_or(UNIX_EPOCH));
    Attr {
        ino: nodeid,
        size: metadata.len(),
        blocks: metadata.len().div_ceil(512),
        atime,
        mtime,
        ctime,
        atimensec,
        mtimensec,
        ctimensec,
        mode: kind | permissions,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn directory_type(metadata: &Metadata) -> u32 {
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        4
    } else if file_type.is_symlink() {
        10
    } else {
        8
    }
}

#[cfg(target_os = "linux")]
fn open_export_root(path: &Path) -> ::std::io::Result<File> {
    File::open(path)
}

#[cfg(target_os = "windows")]
fn open_export_root(path: &Path) -> ::std::io::Result<File> {
    open_directory_pinned(path)
}

#[cfg(target_os = "linux")]
fn open_identity_beneath(
    root: &File,
    _root_path: &Path,
    relative: &Path,
) -> ::std::io::Result<File> {
    use ::std::os::fd::{AsRawFd, FromRawFd};
    use ::std::os::unix::ffi::OsStrExt;

    if relative.as_os_str().is_empty() {
        return root.try_clone();
    }
    let parent_path = relative.parent().unwrap_or(Path::new(""));
    let parent = linux_open_beneath(root, parent_path, 0, false, 0, true)?;
    let name = relative
        .file_name()
        .ok_or_else(|| ::std::io::Error::from(::std::io::ErrorKind::InvalidInput))?;
    let name = ::std::ffi::CString::new(name.as_bytes())?;
    let fd = unsafe {
        ::libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            ::libc::O_PATH | ::libc::O_NOFOLLOW | ::libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(::std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "windows")]
fn open_identity_beneath(
    root: &File,
    root_path: &Path,
    relative: &Path,
) -> ::std::io::Result<File> {
    if relative.as_os_str().is_empty() {
        return root.try_clone();
    }
    let parent = relative.parent().unwrap_or(Path::new(""));
    let _guards = pin_windows_directories(root_path, parent)?;
    open_identity_beneath_inner(root_path, relative)
}

#[cfg(target_os = "windows")]
fn open_identity_beneath_inner(root_path: &Path, relative: &Path) -> ::std::io::Result<File> {
    use ::std::os::windows::fs::OpenOptionsExt;
    use ::windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(root_path.join(relative))
}

#[cfg(target_os = "linux")]
fn open_beneath(
    root: &File,
    _root_path: &Path,
    relative: &Path,
    flags: u32,
    create: bool,
    mode: u32,
) -> ::std::io::Result<File> {
    linux_open_beneath(root, relative, flags, create, mode, false)
}

#[cfg(target_os = "linux")]
fn open_directory_beneath(
    root: &File,
    _root_path: &Path,
    relative: &Path,
) -> ::std::io::Result<File> {
    linux_open_beneath(root, relative, 0, false, 0, true)
}

#[cfg(target_os = "linux")]
fn open_attribute_beneath(
    root: &File,
    _root_path: &Path,
    relative: &Path,
) -> ::std::io::Result<File> {
    linux_open_beneath(root, relative, 0, false, 0, false)
}

#[cfg(target_os = "linux")]
fn linux_open_beneath(
    root: &File,
    relative: &Path,
    flags: u32,
    create: bool,
    mode: u32,
    directory: bool,
) -> ::std::io::Result<File> {
    use ::std::os::fd::{AsRawFd, FromRawFd};
    use ::std::os::unix::ffi::OsStrExt;

    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    let access = flags & O_ACCMODE;
    let mut host_flags = match access {
        O_WRONLY => ::libc::O_WRONLY,
        O_RDWR => ::libc::O_RDWR,
        _ => ::libc::O_RDONLY,
    } | ::libc::O_CLOEXEC
        | ::libc::O_NOFOLLOW;
    if flags & O_APPEND != 0 {
        host_flags |= ::libc::O_APPEND;
    }
    if create {
        host_flags |= ::libc::O_CREAT;
        if flags & O_EXCL != 0 {
            host_flags |= ::libc::O_EXCL;
        }
    }
    if directory {
        host_flags |= ::libc::O_DIRECTORY;
    }
    let path = if relative.as_os_str().is_empty() {
        OsStr::new(".")
    } else {
        relative.as_os_str()
    };
    let path = ::std::ffi::CString::new(path.as_bytes())?;
    let how = OpenHow {
        flags: host_flags as u64,
        mode: if create { u64::from(mode & 0o7777) } else { 0 },
        // RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH.
        resolve: 0x02 | 0x04 | 0x08,
    };
    let fd = unsafe {
        ::libc::syscall(
            ::libc::SYS_openat2,
            root.as_raw_fd(),
            path.as_ptr(),
            &how,
            ::core::mem::size_of::<OpenHow>(),
        )
    } as ::libc::c_int;
    if fd < 0 {
        return Err(::std::io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if flags & O_TRUNC != 0 {
        file.set_len(0)?;
    }
    Ok(file)
}

#[cfg(target_os = "windows")]
fn open_beneath(
    _root: &File,
    root_path: &Path,
    relative: &Path,
    flags: u32,
    create: bool,
    mode: u32,
) -> ::std::io::Result<File> {
    let parent = relative.parent().unwrap_or(Path::new(""));
    let _guards = pin_windows_directories(root_path, parent)?;
    open_file(&root_path.join(relative), flags, create, mode)
}

#[cfg(target_os = "windows")]
fn open_directory_beneath(
    root: &File,
    root_path: &Path,
    relative: &Path,
) -> ::std::io::Result<File> {
    if relative.as_os_str().is_empty() {
        return root.try_clone();
    }
    let mut guards = pin_windows_directories(root_path, relative)?;
    guards
        .pop()
        .ok_or_else(|| ::std::io::Error::from(::std::io::ErrorKind::NotFound))
}

#[cfg(target_os = "windows")]
fn open_attribute_beneath(
    _root: &File,
    root_path: &Path,
    relative: &Path,
) -> ::std::io::Result<File> {
    use ::std::os::windows::fs::OpenOptionsExt;
    use ::windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    };

    let parent = relative.parent().unwrap_or(Path::new(""));
    let _guards = pin_windows_directories(root_path, parent)?;
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0 | FILE_WRITE_ATTRIBUTES.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(root_path.join(relative))?;
    if metadata_is_link(&file.metadata()?) {
        return Err(::std::io::Error::from(
            ::std::io::ErrorKind::PermissionDenied,
        ));
    }
    Ok(file)
}

#[cfg(target_os = "windows")]
fn pin_windows_directories(root: &Path, relative: &Path) -> ::std::io::Result<Vec<File>> {
    let mut guards = Vec::new();
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(::std::io::Error::from(
                ::std::io::ErrorKind::PermissionDenied,
            ));
        };
        current.push(name);
        let guard = open_directory_pinned(&current)?;
        let resolved = fs::canonicalize(&current)?;
        if !resolved.starts_with(root) {
            return Err(::std::io::Error::from(
                ::std::io::ErrorKind::PermissionDenied,
            ));
        }
        guards.push(guard);
    }
    Ok(guards)
}

#[cfg(target_os = "windows")]
fn open_directory_pinned(path: &Path) -> ::std::io::Result<File> {
    use ::std::os::windows::fs::OpenOptionsExt;
    use ::windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    if metadata_is_link(&file.metadata()?) {
        return Err(::std::io::Error::from(
            ::std::io::ErrorKind::PermissionDenied,
        ));
    }
    Ok(file)
}

#[cfg(target_os = "windows")]
fn open_file(path: &Path, flags: u32, create: bool, _mode: u32) -> ::std::io::Result<File> {
    let access = flags & O_ACCMODE;
    let mut create = create;
    if create && access == 0 {
        let mut options = host_open_options();
        let created = options.write(true).create_new(true).open(path);
        match created {
            Ok(file) => drop(file),
            Err(error)
                if flags & O_EXCL == 0 && error.kind() == ::std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        create = false;
    }
    let mut options = host_open_options();
    options.read(access != O_WRONLY);
    options.write(access == O_WRONLY || access == O_RDWR);
    options.append(flags & O_APPEND != 0);
    if create {
        if flags & O_EXCL != 0 {
            options.create_new(true);
        } else {
            options.create(true);
        }
    }
    let file = options.open(path)?;
    if metadata_is_link(&file.metadata()?) {
        return Err(::std::io::Error::from(
            ::std::io::ErrorKind::PermissionDenied,
        ));
    }
    if flags & O_TRUNC != 0 {
        file.set_len(0)?;
    }
    Ok(file)
}

#[cfg(target_os = "windows")]
fn host_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    {
        use ::std::os::windows::fs::OpenOptionsExt;
        use ::windows::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    options
}

#[cfg(target_os = "linux")]
fn linux_open_child(
    parent: &File,
    name: &OsStr,
    flags: u32,
    create: bool,
    mode: u32,
    directory: bool,
    path_only: bool,
) -> ::std::io::Result<File> {
    use ::std::os::fd::{AsRawFd, FromRawFd};
    use ::std::os::unix::ffi::OsStrExt;
    let name = ::std::ffi::CString::new(name.as_bytes())?;
    let mut host_flags = if path_only {
        ::libc::O_PATH
    } else {
        match flags & O_ACCMODE {
            O_WRONLY => ::libc::O_WRONLY,
            O_RDWR => ::libc::O_RDWR,
            _ => ::libc::O_RDONLY,
        }
    } | ::libc::O_CLOEXEC
        | ::libc::O_NOFOLLOW;
    if flags & O_APPEND != 0 {
        host_flags |= ::libc::O_APPEND;
    }
    if flags & O_TRUNC != 0 {
        host_flags |= ::libc::O_TRUNC;
    }
    if create {
        host_flags |= ::libc::O_CREAT;
        if flags & O_EXCL != 0 {
            host_flags |= ::libc::O_EXCL;
        }
    }
    if directory {
        host_flags |= ::libc::O_DIRECTORY;
    }
    let fd =
        unsafe { ::libc::openat(parent.as_raw_fd(), name.as_ptr(), host_flags, mode & 0o7777) };
    if fd < 0 {
        return Err(::std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn open_child_identity(
    parent: &File,
    _root_path: &Path,
    _path: &Path,
    name: &OsStr,
) -> ::std::io::Result<File> {
    linux_open_child(parent, name, 0, false, 0, false, true)
}

#[cfg(target_os = "windows")]
fn open_child_identity(
    _parent: &File,
    root_path: &Path,
    path: &Path,
    _name: &OsStr,
) -> ::std::io::Result<File> {
    open_identity_beneath_inner(root_path, path)
}

#[cfg(target_os = "linux")]
fn open_child_file(
    parent: &File,
    _root_path: &Path,
    _path: &Path,
    name: &OsStr,
    flags: u32,
    create: bool,
    mode: u32,
) -> ::std::io::Result<File> {
    linux_open_child(parent, name, flags, create, mode, false, false)
}

#[cfg(target_os = "windows")]
fn open_child_file(
    _parent: &File,
    root_path: &Path,
    path: &Path,
    _name: &OsStr,
    flags: u32,
    create: bool,
    mode: u32,
) -> ::std::io::Result<File> {
    open_file(&root_path.join(path), flags, create, mode)
}

#[cfg(target_os = "linux")]
fn open_child_directory(
    parent: &File,
    _root_path: &Path,
    _path: &Path,
    name: &OsStr,
) -> ::std::io::Result<File> {
    linux_open_child(parent, name, 0, false, 0, true, true)
}

#[cfg(target_os = "windows")]
fn open_child_directory(
    _parent: &File,
    root_path: &Path,
    path: &Path,
    _name: &OsStr,
) -> ::std::io::Result<File> {
    open_directory_pinned(&root_path.join(path))
}

#[cfg(target_os = "linux")]
fn create_directory_at(
    parent: &File,
    _root_path: &Path,
    _relative: &Path,
    name: &OsStr,
    mode: u32,
) -> ::std::io::Result<()> {
    use ::std::os::fd::AsRawFd;
    use ::std::os::unix::ffi::OsStrExt;
    let name = ::std::ffi::CString::new(name.as_bytes())?;
    if unsafe { ::libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode & 0o7777) } == 0 {
        Ok(())
    } else {
        Err(::std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "windows")]
fn create_directory_at(
    _parent: &File,
    root_path: &Path,
    relative: &Path,
    _name: &OsStr,
    _mode: u32,
) -> ::std::io::Result<()> {
    fs::create_dir(root_path.join(relative))
}

#[cfg(target_os = "linux")]
fn unlink_at(
    parent: &File,
    _root_path: &Path,
    _relative: &Path,
    name: &OsStr,
    directory: bool,
) -> ::std::io::Result<()> {
    use ::std::os::fd::AsRawFd;
    use ::std::os::unix::ffi::OsStrExt;
    let name = ::std::ffi::CString::new(name.as_bytes())?;
    let flags = if directory { ::libc::AT_REMOVEDIR } else { 0 };
    if unsafe { ::libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(::std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "windows")]
fn unlink_at(
    _parent: &File,
    root_path: &Path,
    relative: &Path,
    _name: &OsStr,
    directory: bool,
) -> ::std::io::Result<()> {
    windows_delete_path(&root_path.join(relative), directory)
}

#[cfg(target_os = "linux")]
fn rename_at(
    old_parent: &File,
    new_parent: &File,
    _root_path: &Path,
    _old: &Path,
    _new: &Path,
    old_name: &OsStr,
    new_name: &OsStr,
    no_replace: bool,
) -> ::std::io::Result<()> {
    use ::std::os::fd::AsRawFd;
    use ::std::os::unix::ffi::OsStrExt;
    let old_name = ::std::ffi::CString::new(old_name.as_bytes())?;
    let new_name = ::std::ffi::CString::new(new_name.as_bytes())?;
    let result = unsafe {
        ::libc::syscall(
            ::libc::SYS_renameat2,
            old_parent.as_raw_fd(),
            old_name.as_ptr(),
            new_parent.as_raw_fd(),
            new_name.as_ptr(),
            if no_replace { 1u32 } else { 0u32 },
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(::std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "windows")]
fn rename_at(
    _old_parent: &File,
    _new_parent: &File,
    root_path: &Path,
    old: &Path,
    new: &Path,
    _old_name: &OsStr,
    _new_name: &OsStr,
    no_replace: bool,
) -> ::std::io::Result<()> {
    rename_replace(&root_path.join(old), &root_path.join(new), no_replace)
}

#[cfg(target_os = "linux")]
fn hard_link_at(
    source: &File,
    new_parent: &File,
    _root_path: &Path,
    _old: &Path,
    _new: &Path,
    new_name: &OsStr,
) -> ::std::io::Result<()> {
    use ::std::os::fd::AsRawFd;
    use ::std::os::unix::ffi::OsStrExt;
    if source.metadata()?.file_type().is_symlink() {
        return Err(::std::io::Error::from(::std::io::ErrorKind::Unsupported));
    }
    let source_path = ::std::ffi::CString::new(format!("/proc/self/fd/{}", source.as_raw_fd()))?;
    let new_name = ::std::ffi::CString::new(new_name.as_bytes())?;
    if unsafe {
        ::libc::linkat(
            ::libc::AT_FDCWD,
            source_path.as_ptr(),
            new_parent.as_raw_fd(),
            new_name.as_ptr(),
            ::libc::AT_SYMLINK_FOLLOW,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(::std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "windows")]
fn hard_link_at(
    _source: &File,
    _new_parent: &File,
    root_path: &Path,
    old: &Path,
    new: &Path,
    _new_name: &OsStr,
) -> ::std::io::Result<()> {
    fs::hard_link(root_path.join(old), root_path.join(new))
}

#[cfg(target_os = "linux")]
fn create_symlink_at(
    parent: &File,
    _root_path: &Path,
    target: &OsStr,
    _relative: &Path,
    name: &OsStr,
) -> ::std::io::Result<()> {
    use ::std::os::fd::AsRawFd;
    use ::std::os::unix::ffi::OsStrExt;
    let name = ::std::ffi::CString::new(name.as_bytes())?;
    let target = ::std::ffi::CString::new(target.as_bytes())?;
    if unsafe { ::libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(::std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "windows")]
fn create_symlink_at(
    _parent: &File,
    root_path: &Path,
    target: &OsStr,
    relative: &Path,
    _name: &OsStr,
) -> ::std::io::Result<()> {
    create_symlink(target, &root_path.join(relative), root_path)
}

#[cfg(target_os = "linux")]
fn read_link_node(file: &File, _root_path: &Path, _relative: &Path) -> ::std::io::Result<PathBuf> {
    use ::std::os::fd::AsRawFd;
    use ::std::os::unix::ffi::OsStringExt;
    let empty = c"";
    let mut buffer = vec![0u8; 256];
    loop {
        let count = unsafe {
            ::libc::readlinkat(
                file.as_raw_fd(),
                empty.as_ptr(),
                buffer.as_mut_ptr().cast::<::libc::c_char>(),
                buffer.len(),
            )
        };
        if count < 0 {
            return Err(::std::io::Error::last_os_error());
        }
        let count = count as usize;
        if count < buffer.len() {
            buffer.truncate(count);
            return Ok(PathBuf::from(OsString::from_vec(buffer)));
        }
        buffer.resize(buffer.len().saturating_mul(2), 0);
    }
}

#[cfg(target_os = "windows")]
fn read_link_node(_file: &File, root_path: &Path, relative: &Path) -> ::std::io::Result<PathBuf> {
    fs::read_link(root_path.join(relative))
}

#[cfg(target_os = "linux")]
fn read_directory_names(
    directory: &File,
    _root_path: &Path,
    _relative: &Path,
) -> ::std::io::Result<Vec<OsString>> {
    use ::std::os::fd::AsRawFd;
    let path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect()
}

#[cfg(target_os = "windows")]
fn read_directory_names(
    _directory: &File,
    root_path: &Path,
    relative: &Path,
) -> ::std::io::Result<Vec<OsString>> {
    let _guards = pin_windows_directories(root_path, relative)?;
    fs::read_dir(root_path.join(relative))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect()
}

#[cfg(target_os = "windows")]
fn windows_delete_path(path: &Path, directory: bool) -> ::std::io::Result<()> {
    use ::core::ffi::c_void;
    use ::std::os::windows::fs::OpenOptionsExt;
    use ::std::os::windows::io::AsRawHandle;
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX,
        FILE_DISPOSITION_INFO_EX_FLAGS, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfoEx,
        SetFileInformationByHandle,
    };
    let file = OpenOptions::new()
        .access_mode(DELETE.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    use ::std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let attributes = file.metadata()?.file_attributes();
    let real_directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0
        && attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0;
    if directory && !real_directory {
        return Err(::std::io::Error::from(::std::io::ErrorKind::NotADirectory));
    }
    if !directory && real_directory {
        return Err(::std::io::Error::from(::std::io::ErrorKind::IsADirectory));
    }
    let information = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_INFO_EX_FLAGS(
            FILE_DISPOSITION_FLAG_DELETE.0
                | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS.0
                | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE.0,
        ),
    };
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfoEx,
            (&information as *const FILE_DISPOSITION_INFO_EX).cast::<c_void>(),
            ::core::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    }
    .map_err(|_| ::std::io::Error::last_os_error())
}

#[cfg(target_os = "linux")]
fn set_created_mode_beneath(
    _root: &File,
    _root_path: &Path,
    _relative: &Path,
    file: &File,
    mode: u32,
) -> ::std::io::Result<()> {
    use ::std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode & 0o7777))
}

#[cfg(target_os = "windows")]
fn set_created_mode_beneath(
    root: &File,
    root_path: &Path,
    relative: &Path,
    _file: &File,
    mode: u32,
) -> ::std::io::Result<()> {
    let file = open_attribute_beneath(root, root_path, relative)?;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(mode & 0o222 == 0);
    file.set_permissions(permissions)
}

#[cfg(target_os = "linux")]
fn setattr_mode(file: &File, mode: u32) -> ::std::io::Result<()> {
    use ::std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode & 0o7777))
}

#[cfg(target_os = "windows")]
fn setattr_mode(_file: &File, _mode: u32) -> ::std::io::Result<()> {
    Err(::std::io::Error::from(::std::io::ErrorKind::Unsupported))
}

#[cfg(target_os = "linux")]
fn set_owner(file: &File, uid: Option<u32>, gid: Option<u32>) -> ::std::io::Result<()> {
    use ::std::os::fd::AsRawFd;
    let result = unsafe {
        ::libc::fchown(
            file.as_raw_fd(),
            uid.unwrap_or(u32::MAX),
            gid.unwrap_or(u32::MAX),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(::std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "windows")]
fn set_owner(_file: &File, _uid: Option<u32>, _gid: Option<u32>) -> ::std::io::Result<()> {
    Err(::std::io::Error::from(::std::io::ErrorKind::Unsupported))
}

fn set_times(file: &File, accessed: SystemTime, modified: SystemTime) -> ::std::io::Result<()> {
    file.set_times(
        ::std::fs::FileTimes::new()
            .set_accessed(accessed)
            .set_modified(modified),
    )
}

#[cfg(target_os = "windows")]
fn create_symlink(target: &OsStr, path: &Path, root: &Path) -> ::std::io::Result<()> {
    let parent = path
        .parent()
        .and_then(|parent| parent.strip_prefix(root).ok())
        .ok_or_else(|| ::std::io::Error::from(::std::io::ErrorKind::PermissionDenied))?;
    let mut components: Vec<OsString> = parent
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_os_string()),
            _ => None,
        })
        .collect();
    for component in Path::new(target).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => components.push(name.to_os_string()),
            Component::ParentDir if components.pop().is_some() => {}
            _ => {
                return Err(::std::io::Error::from(
                    ::std::io::ErrorKind::PermissionDenied,
                ));
            }
        }
    }
    let mut candidate = root.to_path_buf();
    candidate.extend(components);
    let target_is_dir = match fs::canonicalize(&candidate) {
        Ok(resolved) if resolved.starts_with(root) => fs::metadata(resolved)?.is_dir(),
        Ok(_) => {
            return Err(::std::io::Error::from(
                ::std::io::ErrorKind::PermissionDenied,
            ));
        }
        Err(error) if error.kind() == ::std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if target_is_dir {
        ::std::os::windows::fs::symlink_dir(target, path)
    } else {
        ::std::os::windows::fs::symlink_file(target, path)
    }
}

#[cfg(target_os = "windows")]
fn rename_replace(old: &Path, new: &Path, no_replace: bool) -> ::std::io::Result<()> {
    use ::core::ffi::c_void;
    use ::std::os::windows::ffi::OsStrExt;
    use ::std::os::windows::fs::OpenOptionsExt;
    use ::std::os::windows::io::AsRawHandle;
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_RENAME_INFO,
        FILE_RENAME_INFO_0, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileRenameInfoEx,
        SetFileInformationByHandle,
    };

    let source = OpenOptions::new()
        .access_mode(DELETE.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(old)?;
    let name: Vec<u16> = new.as_os_str().encode_wide().collect();
    let name_bytes = name
        .len()
        .checked_mul(2)
        .ok_or_else(|| ::std::io::Error::from(::std::io::ErrorKind::InvalidInput))?;
    let header = ::core::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = header
        .checked_add(name_bytes)
        .ok_or_else(|| ::std::io::Error::from(::std::io::ErrorKind::InvalidInput))?;
    let mut buffer = vec![0u8; size];
    let information = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*information).Anonymous = FILE_RENAME_INFO_0 {
            // REPLACE_IF_EXISTS | POSIX_SEMANTICS | IGNORE_READONLY_ATTRIBUTE.
            Flags: u32::from(!no_replace) | 0x2 | 0x40,
        };
        (*information).RootDirectory = HANDLE::default();
        (*information).FileNameLength = name_bytes as u32;
        ::core::ptr::copy_nonoverlapping(
            name.as_ptr().cast::<u8>(),
            buffer.as_mut_ptr().add(header),
            name_bytes,
        );
        SetFileInformationByHandle(
            HANDLE(source.as_raw_handle()),
            FileRenameInfoEx,
            information.cast::<c_void>(),
            size as u32,
        )
    }
    .map_err(|_| ::std::io::Error::last_os_error())
}

struct HostStatFs {
    blocks: u64,
    free_blocks: u64,
    available_blocks: u64,
    files: u64,
    free_files: u64,
    block_size: u32,
}

#[cfg(target_os = "linux")]
fn host_statfs(file: &File, _path: &Path) -> ::std::io::Result<HostStatFs> {
    use ::std::os::fd::AsRawFd;
    let mut stat: ::libc::statvfs = unsafe { ::core::mem::zeroed() };
    if unsafe { ::libc::fstatvfs(file.as_raw_fd(), &mut stat) } != 0 {
        return Err(::std::io::Error::last_os_error());
    }
    Ok(HostStatFs {
        blocks: stat.f_blocks,
        free_blocks: stat.f_bfree,
        available_blocks: stat.f_bavail,
        files: stat.f_files,
        free_files: stat.f_ffree,
        block_size: stat.f_frsize as u32,
    })
}

#[cfg(target_os = "windows")]
fn host_statfs(_file: &File, path: &Path) -> ::std::io::Result<HostStatFs> {
    use ::std::iter;
    use ::std::os::windows::ffi::OsStrExt;
    use ::windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use ::windows::core::PCWSTR;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let mut available = 0u64;
    let mut total = 0u64;
    let mut free = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut available),
            Some(&mut total),
            Some(&mut free),
        )
    }
    .map_err(|_| ::std::io::Error::last_os_error())?;
    const BLOCK_SIZE: u64 = 4096;
    Ok(HostStatFs {
        blocks: total / BLOCK_SIZE,
        free_blocks: free / BLOCK_SIZE,
        available_blocks: available / BLOCK_SIZE,
        files: u64::MAX / 2,
        free_files: u64::MAX / 2,
        block_size: BLOCK_SIZE as u32,
    })
}

#[cfg(target_os = "linux")]
fn io_errno(error: ::std::io::Error) -> i32 {
    error.raw_os_error().unwrap_or(EIO)
}

#[cfg(target_os = "windows")]
fn io_errno(error: ::std::io::Error) -> i32 {
    use ::std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => ENOENT,
        ErrorKind::PermissionDenied => EACCES,
        ErrorKind::AlreadyExists => EEXIST,
        ErrorKind::InvalidInput => EINVAL,
        ErrorKind::NotADirectory => ENOTDIR,
        ErrorKind::IsADirectory => EISDIR,
        ErrorKind::DirectoryNotEmpty => ENOTEMPTY,
        ErrorKind::ReadOnlyFilesystem => EROFS,
        ErrorKind::StorageFull => ENOSPC,
        ErrorKind::Unsupported => EOPNOTSUPP,
        _ => EIO,
    }
}

fn validate_name(name: &OsStr) -> FuseResult<()> {
    let path = Path::new(name);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => return Err(EINVAL),
    }
    #[cfg(target_os = "windows")]
    {
        let bytes = os_str_bytes(name);
        if bytes.contains(&b'\\') || bytes.contains(&b':') {
            return Err(EINVAL);
        }
    }
    Ok(())
}

fn decode_state_path(bytes: &[u8]) -> Result<PathBuf> {
    let path = PathBuf::from(os_string(bytes).map_err(|_| ::anyhow::anyhow!("invalid path"))?);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        bail!("virtio-fs snapshot path escapes the export root");
    }
    Ok(path)
}

fn one_name(payload: &[u8]) -> FuseResult<OsString> {
    WireCursor::new(payload).name()
}

#[cfg(target_os = "linux")]
fn os_string(bytes: &[u8]) -> FuseResult<OsString> {
    use ::std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(bytes.to_vec()))
}

#[cfg(target_os = "windows")]
fn os_string(bytes: &[u8]) -> FuseResult<OsString> {
    String::from_utf8(bytes.to_vec())
        .map(OsString::from)
        .map_err(|_| EINVAL)
}

#[cfg(target_os = "linux")]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    use ::std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(target_os = "windows")]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

fn wire_time(seconds: u64, nanoseconds: u32) -> FuseResult<SystemTime> {
    if nanoseconds >= 1_000_000_000 {
        return Err(EINVAL);
    }
    UNIX_EPOCH
        .checked_add(Duration::new(seconds, nanoseconds))
        .ok_or(EINVAL)
}

#[cfg(target_os = "windows")]
fn system_time_parts(time: SystemTime) -> (u64, u32) {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    (duration.as_secs(), duration.subsec_nanos())
}

fn align_up(value: usize, alignment: usize) -> usize {
    value.saturating_add(alignment - 1) & !(alignment - 1)
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend(value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend(value.to_le_bytes());
}

fn put_i32(output: &mut Vec<u8>, value: i32) {
    output.extend(value.to_le_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend(value.to_le_bytes());
}

fn put_bytes(output: &mut Vec<u8>, value: &[u8]) {
    put_u32(output, value.len() as u32);
    output.extend(value);
}

struct StateReader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> StateReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .context("virtio-fs state overflow")?;
        let value = self
            .data
            .get(self.offset..end)
            .context("virtio-fs state truncated")?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four-byte slice"),
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight-byte slice"),
        ))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    fn is_empty(&self) -> bool {
        self.offset == self.data.len()
    }
}

struct WireCursor<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> WireCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn take(&mut self, count: usize) -> FuseResult<&'a [u8]> {
        let end = self.offset.checked_add(count).ok_or(EINVAL)?;
        let output = self.data.get(self.offset..end).ok_or(EINVAL)?;
        self.offset = end;
        Ok(output)
    }

    fn u32(&mut self) -> FuseResult<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(
            bytes.try_into().expect("four-byte slice"),
        ))
    }

    fn u64(&mut self) -> FuseResult<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(
            bytes.try_into().expect("eight-byte slice"),
        ))
    }

    fn name(&mut self) -> FuseResult<OsString> {
        let remaining = self.data.get(self.offset..).ok_or(EINVAL)?;
        let length = remaining.iter().position(|byte| *byte == 0).ok_or(EINVAL)?;
        let name = os_string(&remaining[..length])?;
        self.offset += length + 1;
        validate_name(&name)?;
        Ok(name)
    }

    fn name_unchecked(&mut self) -> FuseResult<OsString> {
        let remaining = self.data.get(self.offset..).ok_or(EINVAL)?;
        let length = remaining.iter().position(|byte| *byte == 0).ok_or(EINVAL)?;
        let name = os_string(&remaining[..length])?;
        self.offset += length + 1;
        Ok(name)
    }
}

const ENXIO: i32 = 6;

#[cfg(test)]
mod tests {
    use super::*;

    fn request(opcode: u32, unique: u64, nodeid: u64, payload: &[u8]) -> Vec<u8> {
        let mut input = Vec::new();
        put_u32(&mut input, (FUSE_HEADER_LEN + payload.len()) as u32);
        put_u32(&mut input, opcode);
        put_u64(&mut input, unique);
        put_u64(&mut input, nodeid);
        put_u32(&mut input, 0);
        put_u32(&mut input, 0);
        put_u32(&mut input, 1);
        put_u32(&mut input, 0);
        input.extend(payload);
        input
    }

    fn temporary_root(name: &str) -> PathBuf {
        let root = ::std::env::temp_dir().join(format!(
            "nvx-live-virtfs-{name}-{}-{:?}",
            ::std::process::id(),
            ::std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn reply_error(reply: &[u8]) -> i32 {
        i32::from_le_bytes(reply[4..8].try_into().unwrap())
    }

    fn cleanup_root(root: PathBuf, filesystem: PassthroughFs) {
        drop(filesystem);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    fn create_directory_symlink(target: &Path, link: &Path) -> ::std::io::Result<()> {
        ::std::os::unix::fs::symlink(target, link)
    }

    #[cfg(target_os = "windows")]
    fn create_directory_symlink(target: &Path, link: &Path) -> ::std::io::Result<()> {
        ::std::os::windows::fs::symlink_dir(target, link)
    }

    #[test]
    fn lookup_and_read_observe_live_host_changes() {
        let root = temporary_root("live");
        fs::write(root.join("value.txt"), b"first").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();

        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"value.txt\0"))
            .unwrap();
        assert_eq!(reply_error(&lookup), 0);
        let nodeid = u64::from_le_bytes(lookup[16..24].try_into().unwrap());

        let mut open_payload = Vec::new();
        put_u32(&mut open_payload, 0);
        put_u32(&mut open_payload, 0);
        let open = filesystem
            .handle(&request(FUSE_OPEN, 2, nodeid, &open_payload))
            .unwrap();
        let handle = u64::from_le_bytes(open[16..24].try_into().unwrap());

        fs::write(root.join("value.txt"), b"second").unwrap();
        let mut read_payload = Vec::new();
        put_u64(&mut read_payload, handle);
        put_u64(&mut read_payload, 0);
        put_u32(&mut read_payload, 16);
        read_payload.resize(40, 0);
        let read = filesystem
            .handle(&request(FUSE_READ, 3, nodeid, &read_payload))
            .unwrap();
        assert_eq!(&read[FUSE_OUT_HEADER_LEN..], b"second");
        cleanup_root(root, filesystem);
    }

    #[test]
    fn read_only_export_rejects_guest_creation() {
        let root = temporary_root("readonly");
        let mut filesystem = PassthroughFs::new(&root, false).unwrap();
        let mut payload = Vec::new();
        put_u32(&mut payload, O_WRONLY | O_CREAT);
        put_u32(&mut payload, 0o644);
        put_u32(&mut payload, 0);
        put_u32(&mut payload, 0);
        payload.extend(b"new.txt\0");
        let reply = filesystem
            .handle(&request(FUSE_CREATE, 1, FUSE_ROOT_ID, &payload))
            .unwrap();
        assert_eq!(reply_error(&reply), -EROFS);
        assert!(!root.join("new.txt").exists());
        cleanup_root(root, filesystem);
    }

    #[test]
    fn writable_export_can_create_read_only_handle() {
        let root = temporary_root("create-readonly");
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let mut payload = Vec::new();
        put_u32(&mut payload, 0);
        put_u32(&mut payload, 0o644);
        put_u32(&mut payload, 0);
        put_u32(&mut payload, 0);
        payload.extend(b"readonly-handle\0");
        let reply = filesystem
            .handle(&request(FUSE_CREATE, 1, FUSE_ROOT_ID, &payload))
            .unwrap();
        assert_eq!(reply_error(&reply), 0);
        assert!(root.join("readonly-handle").is_file());
        cleanup_root(root, filesystem);
    }

    #[test]
    fn readdirplus_acquires_lookups_only_for_returned_entries() {
        let root = temporary_root("readdirplus-lookups");
        for name in ["a", "b", "c"] {
            fs::write(root.join(name), name).unwrap();
        }
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let open = filesystem
            .handle(&request(FUSE_OPENDIR, 1, FUSE_ROOT_ID, &[0; 8]))
            .unwrap();
        let handle = u64::from_le_bytes(open[16..24].try_into().unwrap());
        let mut payload = Vec::new();
        put_u64(&mut payload, handle);
        put_u64(&mut payload, 2);
        put_u32(&mut payload, 160);
        payload.resize(40, 0);

        let reply = filesystem
            .handle(&request(FUSE_READDIRPLUS, 2, FUSE_ROOT_ID, &payload))
            .unwrap();

        assert_eq!(reply_error(&reply), 0);
        assert!(reply.len() > FUSE_OUT_HEADER_LEN);
        assert_eq!(
            filesystem.nodes.len(),
            2,
            "only root and one returned child"
        );
        cleanup_root(root, filesystem);
    }

    #[test]
    fn readdir_cookies_are_stable_across_host_insertions() {
        let root = temporary_root("readdir-stable-cookies");
        fs::write(root.join("a"), b"a").unwrap();
        fs::write(root.join("b"), b"b").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let open = filesystem
            .handle(&request(FUSE_OPENDIR, 1, FUSE_ROOT_ID, &[0; 8]))
            .unwrap();
        let handle = u64::from_le_bytes(open[16..24].try_into().unwrap());

        let mut first = Vec::new();
        put_u64(&mut first, handle);
        put_u64(&mut first, 2);
        put_u32(&mut first, 32);
        first.resize(40, 0);
        let reply = filesystem
            .handle(&request(FUSE_READDIR, 2, FUSE_ROOT_ID, &first))
            .unwrap();
        assert_eq!(&reply[FUSE_OUT_HEADER_LEN + 24..], b"a\0\0\0\0\0\0\0");

        fs::write(root.join("0"), b"0").unwrap();
        let mut second = Vec::new();
        put_u64(&mut second, handle);
        put_u64(&mut second, 3);
        put_u32(&mut second, 32);
        second.resize(40, 0);
        let reply = filesystem
            .handle(&request(FUSE_READDIR, 3, FUSE_ROOT_ID, &second))
            .unwrap();
        assert_eq!(&reply[FUSE_OUT_HEADER_LEN + 24..], b"b\0\0\0\0\0\0\0");
        cleanup_root(root, filesystem);
    }

    #[test]
    fn readdirplus_dot_entries_do_not_acquire_lookups() {
        let root = temporary_root("readdirplus-dot-lookups");
        fs::create_dir(root.join("child")).unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let child = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"child\0"))
            .unwrap();
        let child_node = u64::from_le_bytes(child[16..24].try_into().unwrap());
        let open = filesystem
            .handle(&request(FUSE_OPENDIR, 2, child_node, &[0; 8]))
            .unwrap();
        let handle = u64::from_le_bytes(open[16..24].try_into().unwrap());
        let mut payload = Vec::new();
        put_u64(&mut payload, handle);
        put_u64(&mut payload, 0);
        put_u32(&mut payload, 320);
        payload.resize(40, 0);
        filesystem
            .handle(&request(FUSE_READDIRPLUS, 3, child_node, &payload))
            .unwrap();
        assert_eq!(filesystem.nodes.get(&child_node).unwrap().lookups, 1);
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_rejects_unrepresentable_posix_metadata_changes() {
        let root = temporary_root("windows-setattr");
        fs::write(root.join("value"), b"value").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"value\0"))
            .unwrap();
        let nodeid = u64::from_le_bytes(lookup[16..24].try_into().unwrap());

        let mut mode = vec![0u8; 88];
        mode[0..4].copy_from_slice(&FATTR_MODE.to_le_bytes());
        mode[68..72].copy_from_slice(&0o600u32.to_le_bytes());
        let reply = filesystem
            .handle(&request(FUSE_SETATTR, 2, nodeid, &mode))
            .unwrap();
        assert_eq!(reply_error(&reply), -EOPNOTSUPP);

        let mut owner = vec![0u8; 88];
        owner[0..4].copy_from_slice(&FATTR_UID.to_le_bytes());
        owner[76..80].copy_from_slice(&1000u32.to_le_bytes());
        let reply = filesystem
            .handle(&request(FUSE_SETATTR, 3, nodeid, &owner))
            .unwrap();
        assert_eq!(reply_error(&reply), -EOPNOTSUPP);
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_read_only_fsync_succeeds() {
        let root = temporary_root("windows-readonly-fsync");
        fs::write(root.join("value"), b"value").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"value\0"))
            .unwrap();
        let nodeid = u64::from_le_bytes(lookup[16..24].try_into().unwrap());
        let open = filesystem
            .handle(&request(FUSE_OPEN, 2, nodeid, &[0; 8]))
            .unwrap();
        let handle = u64::from_le_bytes(open[16..24].try_into().unwrap());
        let mut payload = Vec::new();
        put_u64(&mut payload, handle);
        put_u32(&mut payload, 0);
        put_u32(&mut payload, 0);
        let reply = filesystem
            .handle(&request(FUSE_FSYNC, 3, nodeid, &payload))
            .unwrap();
        assert_eq!(reply_error(&reply), 0);
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_timestamp_setattr_uses_attribute_access() {
        let root = temporary_root("windows-settime");
        fs::write(root.join("value"), b"value").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"value\0"))
            .unwrap();
        let nodeid = u64::from_le_bytes(lookup[16..24].try_into().unwrap());
        let mut payload = vec![0u8; 88];
        payload[0..4].copy_from_slice(&FATTR_MTIME.to_le_bytes());
        payload[40..48].copy_from_slice(&1_700_000_000u64.to_le_bytes());
        let reply = filesystem
            .handle(&request(FUSE_SETATTR, 2, nodeid, &payload))
            .unwrap();
        assert_eq!(reply_error(&reply), 0);
        let seconds = fs::metadata(root.join("value"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(seconds, 1_700_000_000);
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_unlink_removes_directory_symlink_not_target() {
        let root = temporary_root("windows-unlink-dirlink");
        fs::create_dir(root.join("target")).unwrap();
        if ::std::os::windows::fs::symlink_dir(root.join("target"), root.join("link")).is_err() {
            fs::remove_dir_all(root).unwrap();
            return;
        }
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let reply = filesystem
            .handle(&request(FUSE_UNLINK, 1, FUSE_ROOT_ID, b"link\0"))
            .unwrap();
        assert_eq!(reply_error(&reply), 0);
        assert!(!root.join("link").exists());
        assert!(root.join("target").is_dir());
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_unlink_rejects_real_directory() {
        let root = temporary_root("windows-unlink-directory");
        fs::create_dir(root.join("directory")).unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let reply = filesystem
            .handle(&request(FUSE_UNLINK, 1, FUSE_ROOT_ID, b"directory\0"))
            .unwrap();
        assert_eq!(reply_error(&reply), -EISDIR);
        assert!(root.join("directory").is_dir());
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_rmdir_rejects_regular_file() {
        let root = temporary_root("windows-rmdir-file");
        fs::write(root.join("file"), b"file").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let reply = filesystem
            .handle(&request(FUSE_RMDIR, 1, FUSE_ROOT_ID, b"file\0"))
            .unwrap();
        assert_eq!(reply_error(&reply), -ENOTDIR);
        assert!(root.join("file").is_file());
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_renames_directories() {
        let root = temporary_root("windows-rename-directory");
        fs::create_dir(root.join("source")).unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let mut payload = Vec::new();
        put_u64(&mut payload, FUSE_ROOT_ID);
        payload.extend(b"source\0destination\0");
        let reply = filesystem
            .handle(&request(FUSE_RENAME, 1, FUSE_ROOT_ID, &payload))
            .unwrap();
        assert_eq!(reply_error(&reply), 0);
        assert!(root.join("destination").is_dir());
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_updates_directory_timestamp() {
        let root = temporary_root("windows-directory-time");
        fs::create_dir(root.join("directory")).unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"directory\0"))
            .unwrap();
        let nodeid = u64::from_le_bytes(lookup[16..24].try_into().unwrap());
        let mut payload = vec![0u8; 88];
        payload[0..4].copy_from_slice(&FATTR_MTIME.to_le_bytes());
        payload[40..48].copy_from_slice(&1_700_000_100u64.to_le_bytes());
        let reply = filesystem
            .handle(&request(FUSE_SETATTR, 2, nodeid, &payload))
            .unwrap();
        assert_eq!(reply_error(&reply), 0);
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_unlinks_read_only_file() {
        let root = temporary_root("windows-unlink-readonly");
        fs::write(root.join("value"), b"value").unwrap();
        let mut permissions = fs::metadata(root.join("value")).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(root.join("value"), permissions).unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let reply = filesystem
            .handle(&request(FUSE_UNLINK, 1, FUSE_ROOT_ID, b"value\0"))
            .unwrap();
        assert_eq!(reply_error(&reply), 0);
        assert!(!root.join("value").exists());
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_replaces_read_only_destination() {
        let root = temporary_root("windows-replace-readonly");
        fs::write(root.join("source"), b"source").unwrap();
        fs::write(root.join("destination"), b"destination").unwrap();
        let mut permissions = fs::metadata(root.join("destination"))
            .unwrap()
            .permissions();
        permissions.set_readonly(true);
        fs::set_permissions(root.join("destination"), permissions).unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let mut payload = Vec::new();
        put_u64(&mut payload, FUSE_ROOT_ID);
        payload.extend(b"source\0destination\0");
        let reply = filesystem
            .handle(&request(FUSE_RENAME, 1, FUSE_ROOT_ID, &payload))
            .unwrap();
        assert_eq!(reply_error(&reply), 0);
        assert_eq!(fs::read(root.join("destination")).unwrap(), b"source");
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_relative_directory_symlink_uses_link_parent() {
        let root = temporary_root("windows-relative-symlink");
        let parent = root.join("parent");
        fs::create_dir_all(parent.join("sibling")).unwrap();
        let link = parent.join("link");
        if create_symlink(OsStr::new("sibling"), &link, &root).is_err() {
            fs::remove_dir_all(root).unwrap();
            return;
        }
        assert!(fs::metadata(&link).unwrap().is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_symlink_targets_are_confined_and_may_dangle() {
        let root = temporary_root("windows-confined-symlink");
        let absolute = create_symlink(OsStr::new(r"C:\outside"), &root.join("absolute"), &root);
        assert_eq!(
            absolute.unwrap_err().kind(),
            ::std::io::ErrorKind::PermissionDenied
        );

        let dangling = root.join("dangling");
        if create_symlink(OsStr::new("missing"), &dangling, &root).is_ok() {
            assert_eq!(fs::read_link(&dangling).unwrap(), Path::new("missing"));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parent_components_are_rejected() {
        let root = temporary_root("escape");
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let reply = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"..\0"))
            .unwrap();
        assert_eq!(reply_error(&reply), -EINVAL);
        cleanup_root(root, filesystem);
    }

    #[test]
    fn host_cannot_rebind_retained_ancestor_outside_export() {
        let root = temporary_root("rebind-root");
        let outside = temporary_root("rebind-outside");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/value"), b"inside").unwrap();
        fs::write(outside.join("b"), b"outside").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let nodeid = filesystem.intern(PathBuf::from("a/b/value"), 1).unwrap();

        fs::remove_dir_all(root.join("a")).unwrap();
        if create_directory_symlink(&outside, &root.join("a")).is_err() {
            drop(filesystem);
            fs::remove_dir_all(root).unwrap();
            fs::remove_dir_all(outside).unwrap();
            return;
        }
        let mut payload = Vec::new();
        put_u32(&mut payload, 0);
        put_u32(&mut payload, 0);
        let reply = filesystem
            .handle(&request(FUSE_OPEN, 1, nodeid, &payload))
            .unwrap();
        assert_eq!(reply_error(&reply), -ESTALE);

        let _ = fs::remove_file(root.join("a"));
        let _ = fs::remove_dir(root.join("a"));
        drop(filesystem);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn setattr_cannot_follow_rebound_ancestor_outside_export() {
        let root = temporary_root("setattr-rebind-root");
        let outside = temporary_root("setattr-rebind-outside");
        fs::create_dir(root.join("a")).unwrap();
        fs::write(root.join("a/value"), b"inside").unwrap();
        fs::write(outside.join("value"), b"outside").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"a\0"))
            .unwrap();
        let a_node = u64::from_le_bytes(lookup[16..24].try_into().unwrap());
        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 2, a_node, b"value\0"))
            .unwrap();
        let value_node = u64::from_le_bytes(lookup[16..24].try_into().unwrap());

        fs::remove_dir_all(root.join("a")).unwrap();
        if create_directory_symlink(&outside, &root.join("a")).is_err() {
            cleanup_root(root, filesystem);
            fs::remove_dir_all(outside).unwrap();
            return;
        }
        let mut payload = vec![0u8; 88];
        payload[0..4].copy_from_slice(&FATTR_SIZE.to_le_bytes());
        let reply = filesystem
            .handle(&request(FUSE_SETATTR, 3, value_node, &payload))
            .unwrap();
        assert_ne!(reply_error(&reply), 0);
        assert_eq!(fs::read(outside.join("value")).unwrap(), b"outside");
        let _ = fs::remove_file(root.join("a"));
        let _ = fs::remove_dir(root.join("a"));
        cleanup_root(root, filesystem);
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn rename_noreplace_rejects_dangling_symlink() {
        let root = temporary_root("rename-noreplace-dangling");
        fs::write(root.join("source"), b"source").unwrap();
        let link_result = {
            #[cfg(target_os = "linux")]
            {
                ::std::os::unix::fs::symlink("missing", root.join("destination"))
            }
            #[cfg(target_os = "windows")]
            {
                ::std::os::windows::fs::symlink_file("missing", root.join("destination"))
            }
        };
        if link_result.is_err() {
            fs::remove_dir_all(root).unwrap();
            return;
        }
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let mut payload = Vec::new();
        put_u64(&mut payload, FUSE_ROOT_ID);
        put_u32(&mut payload, RENAME_NOREPLACE);
        put_u32(&mut payload, 0);
        payload.extend(b"source\0destination\0");
        let reply = filesystem
            .handle(&request(FUSE_RENAME2, 1, FUSE_ROOT_ID, &payload))
            .unwrap();
        assert_eq!(reply_error(&reply), -EEXIST);
        assert!(root.join("source").is_file());
        assert_eq!(
            fs::read_link(root.join("destination")).unwrap(),
            Path::new("missing")
        );
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_mutations_stay_on_pinned_root_after_path_rebind() {
        let root = temporary_root("linux-root-rebind");
        let moved = root.with_file_name(format!(
            "{}-moved",
            root.file_name().unwrap().to_string_lossy()
        ));
        let _ = fs::remove_dir_all(&moved);
        fs::write(root.join("value"), b"inside").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"value\0"))
            .unwrap();
        let nodeid = u64::from_le_bytes(lookup[16..24].try_into().unwrap());

        fs::rename(&root, &moved).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("value"), b"outside").unwrap();
        let mut setattr = vec![0u8; 88];
        setattr[0..4].copy_from_slice(&FATTR_SIZE.to_le_bytes());
        let reply = filesystem
            .handle(&request(FUSE_SETATTR, 2, nodeid, &setattr))
            .unwrap();
        assert_eq!(reply_error(&reply), 0);
        assert!(fs::read(moved.join("value")).unwrap().is_empty());
        assert_eq!(fs::read(root.join("value")).unwrap(), b"outside");

        let mut rename = Vec::new();
        put_u64(&mut rename, FUSE_ROOT_ID);
        rename.extend(b"value\0renamed\0");
        let reply = filesystem
            .handle(&request(FUSE_RENAME, 3, FUSE_ROOT_ID, &rename))
            .unwrap();
        assert_eq!(reply_error(&reply), 0);
        assert!(moved.join("renamed").is_file());
        assert!(root.join("value").is_file());

        drop(filesystem);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(moved).unwrap();
    }

    #[test]
    fn create_cannot_follow_final_symlink_outside_export() {
        let root = temporary_root("create-symlink-root");
        let outside = temporary_root("create-symlink-outside");
        let target = outside.join("target");
        fs::write(&target, b"outside").unwrap();
        let link = root.join("link");
        let link_result = {
            #[cfg(target_os = "linux")]
            {
                ::std::os::unix::fs::symlink(&target, &link)
            }
            #[cfg(target_os = "windows")]
            {
                ::std::os::windows::fs::symlink_file(&target, &link)
            }
        };
        if link_result.is_err() {
            fs::remove_dir_all(root).unwrap();
            fs::remove_dir_all(outside).unwrap();
            return;
        }
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let mut payload = Vec::new();
        put_u32(&mut payload, O_RDWR | O_CREAT | O_TRUNC);
        put_u32(&mut payload, 0o644);
        put_u32(&mut payload, 0);
        put_u32(&mut payload, 0);
        payload.extend(b"link\0");
        let reply = filesystem
            .handle(&request(FUSE_CREATE, 1, FUSE_ROOT_ID, &payload))
            .unwrap();
        let error = reply_error(&reply);
        assert!(error == -EACCES || error == -ELOOP);
        assert_eq!(fs::read(&target).unwrap(), b"outside");
        drop(filesystem);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn hard_link_preserves_node_identity_and_alias() {
        let root = temporary_root("hard-link");
        fs::write(root.join("original"), b"content").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"original\0"))
            .unwrap();
        let nodeid = u64::from_le_bytes(lookup[16..24].try_into().unwrap());

        let mut link_payload = Vec::new();
        put_u64(&mut link_payload, nodeid);
        link_payload.extend(b"alias\0");
        let link = filesystem
            .handle(&request(FUSE_LINK, 2, FUSE_ROOT_ID, &link_payload))
            .unwrap();
        assert_eq!(u64::from_le_bytes(link[16..24].try_into().unwrap()), nodeid);

        let unlink = filesystem
            .handle(&request(FUSE_UNLINK, 3, FUSE_ROOT_ID, b"original\0"))
            .unwrap();
        assert_eq!(reply_error(&unlink), 0);
        assert_eq!(filesystem.node_path(nodeid).unwrap(), Path::new("alias"));

        let mut open_payload = Vec::new();
        put_u32(&mut open_payload, 0);
        put_u32(&mut open_payload, 0);
        let open = filesystem
            .handle(&request(FUSE_OPEN, 4, nodeid, &open_payload))
            .unwrap();
        assert_eq!(reply_error(&open), 0);
        cleanup_root(root, filesystem);
    }

    #[test]
    fn host_replaced_hard_link_alias_gets_distinct_node() {
        let root = temporary_root("hard-link-replaced-alias");
        fs::write(root.join("a"), b"original").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"a\0"))
            .unwrap();
        let original_node = u64::from_le_bytes(lookup[16..24].try_into().unwrap());
        let mut link_payload = Vec::new();
        put_u64(&mut link_payload, original_node);
        link_payload.extend(b"b\0");
        filesystem
            .handle(&request(FUSE_LINK, 2, FUSE_ROOT_ID, &link_payload))
            .unwrap();
        let state = filesystem.save().unwrap();

        fs::rename(root.join("b"), root.join("old-b")).unwrap();
        fs::write(root.join("b"), b"replacement").unwrap();
        assert!(filesystem.save().is_err());

        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 3, FUSE_ROOT_ID, b"b\0"))
            .unwrap();
        let replacement_node = u64::from_le_bytes(lookup[16..24].try_into().unwrap());
        assert_ne!(replacement_node, original_node);
        assert_eq!(filesystem.by_path.get(Path::new("a")), Some(&original_node));
        assert_eq!(
            filesystem.by_path.get(Path::new("b")),
            Some(&replacement_node)
        );
        assert_eq!(fs::read(root.join("a")).unwrap(), b"original");

        let mut restored = PassthroughFs::new(&root, true).unwrap();
        assert!(restored.load(&state).is_err());
        drop(restored);
        cleanup_root(root, filesystem);
    }

    #[test]
    fn retained_file_node_never_retargets_host_replacement() {
        let root = temporary_root("retained-file-replacement");
        fs::write(root.join("value"), b"original").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"value\0"))
            .unwrap();
        let nodeid = u64::from_le_bytes(lookup[16..24].try_into().unwrap());

        fs::rename(root.join("value"), root.join("old-value")).unwrap();
        fs::write(root.join("value"), b"replacement").unwrap();

        let getattr = filesystem
            .handle(&request(FUSE_GETATTR, 2, nodeid, &[0; 16]))
            .unwrap();
        assert_eq!(reply_error(&getattr), -ESTALE);

        let mut open = Vec::new();
        put_u32(&mut open, O_RDWR | O_TRUNC);
        put_u32(&mut open, 0);
        let reply = filesystem
            .handle(&request(FUSE_OPEN, 3, nodeid, &open))
            .unwrap();
        assert_eq!(reply_error(&reply), -ESTALE);

        let mut setattr = vec![0u8; 88];
        setattr[0..4].copy_from_slice(&FATTR_SIZE.to_le_bytes());
        let reply = filesystem
            .handle(&request(FUSE_SETATTR, 4, nodeid, &setattr))
            .unwrap();
        assert_eq!(reply_error(&reply), -ESTALE);

        let mut link = Vec::new();
        put_u64(&mut link, nodeid);
        link.extend(b"alias\0");
        let reply = filesystem
            .handle(&request(FUSE_LINK, 5, FUSE_ROOT_ID, &link))
            .unwrap();
        assert_eq!(reply_error(&reply), -ESTALE);
        assert_eq!(fs::read(root.join("value")).unwrap(), b"replacement");
        assert!(!root.join("alias").exists());
        cleanup_root(root, filesystem);
    }

    #[test]
    fn retained_directory_node_never_retargets_host_replacement() {
        let root = temporary_root("retained-directory-replacement");
        fs::create_dir(root.join("directory")).unwrap();
        fs::write(root.join("directory/old"), b"old").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"directory\0"))
            .unwrap();
        let nodeid = u64::from_le_bytes(lookup[16..24].try_into().unwrap());

        fs::rename(root.join("directory"), root.join("old-directory")).unwrap();
        fs::create_dir(root.join("directory")).unwrap();
        fs::write(root.join("directory/victim"), b"replacement").unwrap();

        let mut create = Vec::new();
        put_u32(&mut create, O_RDWR | O_CREAT);
        put_u32(&mut create, 0o644);
        put_u32(&mut create, 0);
        put_u32(&mut create, 0);
        create.extend(b"new\0");
        let reply = filesystem
            .handle(&request(FUSE_CREATE, 2, nodeid, &create))
            .unwrap();
        assert_eq!(reply_error(&reply), -ESTALE);

        let reply = filesystem
            .handle(&request(FUSE_UNLINK, 3, nodeid, b"victim\0"))
            .unwrap();
        assert_eq!(reply_error(&reply), -ESTALE);

        let mut rename = Vec::new();
        put_u64(&mut rename, FUSE_ROOT_ID);
        rename.extend(b"victim\0moved\0");
        let reply = filesystem
            .handle(&request(FUSE_RENAME, 4, nodeid, &rename))
            .unwrap();
        assert_eq!(reply_error(&reply), -ESTALE);
        assert!(!root.join("directory/new").exists());
        assert_eq!(
            fs::read(root.join("directory/victim")).unwrap(),
            b"replacement"
        );
        assert!(!root.join("moved").exists());
        cleanup_root(root, filesystem);
    }

    #[test]
    fn rename_replacement_retires_destination_node() {
        let root = temporary_root("rename-replace");
        fs::write(root.join("source"), b"source").unwrap();
        fs::write(root.join("destination"), b"destination").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let source = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"source\0"))
            .unwrap();
        let source_node = u64::from_le_bytes(source[16..24].try_into().unwrap());
        let destination = filesystem
            .handle(&request(FUSE_LOOKUP, 2, FUSE_ROOT_ID, b"destination\0"))
            .unwrap();
        let destination_node = u64::from_le_bytes(destination[16..24].try_into().unwrap());
        assert_eq!(
            filesystem.nodes.get(&destination_node).unwrap().path,
            Path::new("destination")
        );

        let mut payload = Vec::new();
        put_u64(&mut payload, FUSE_ROOT_ID);
        payload.extend(b"source\0destination\0");
        let rename = filesystem
            .handle(&request(FUSE_RENAME, 3, FUSE_ROOT_ID, &payload))
            .unwrap();
        assert_eq!(reply_error(&rename), 0);
        assert_eq!(
            filesystem.by_path.get(Path::new("destination")),
            Some(&source_node)
        );
        assert!(!filesystem.nodes.contains_key(&destination_node));
        assert!(!filesystem.by_path.contains_key(Path::new("source")));
        cleanup_root(root, filesystem);
    }

    #[test]
    fn snapshot_rejects_open_replaced_destination() {
        let root = temporary_root("snapshot-replaced-destination");
        fs::write(root.join("source"), b"source").unwrap();
        fs::write(root.join("destination"), b"destination").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let destination = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"destination\0"))
            .unwrap();
        let destination_node = u64::from_le_bytes(destination[16..24].try_into().unwrap());
        let mut open_payload = Vec::new();
        put_u32(&mut open_payload, 0);
        put_u32(&mut open_payload, 0);
        filesystem
            .handle(&request(FUSE_OPEN, 2, destination_node, &open_payload))
            .unwrap();

        let mut rename_payload = Vec::new();
        put_u64(&mut rename_payload, FUSE_ROOT_ID);
        rename_payload.extend(b"source\0destination\0");
        let rename = filesystem
            .handle(&request(FUSE_RENAME, 3, FUSE_ROOT_ID, &rename_payload))
            .unwrap();
        assert_eq!(reply_error(&rename), 0);

        assert_eq!(filesystem.handles.len(), 1);
        assert!(!filesystem.nodes.contains_key(&destination_node));
        assert!(
            !filesystem
                .by_path
                .values()
                .any(|nodeid| *nodeid == destination_node)
        );
        assert!(filesystem.save().is_err());
        cleanup_root(root, filesystem);
    }

    #[test]
    fn snapshot_rejects_host_replacement_of_open_file() {
        let root = temporary_root("snapshot-host-replacement");
        fs::write(root.join("value"), b"original").unwrap();
        let mut filesystem = PassthroughFs::new(&root, true).unwrap();
        let lookup = filesystem
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"value\0"))
            .unwrap();
        let nodeid = u64::from_le_bytes(lookup[16..24].try_into().unwrap());
        let mut open_payload = Vec::new();
        put_u32(&mut open_payload, 0);
        put_u32(&mut open_payload, 0);
        filesystem
            .handle(&request(FUSE_OPEN, 2, nodeid, &open_payload))
            .unwrap();

        fs::rename(root.join("value"), root.join("old-value")).unwrap();
        fs::write(root.join("value"), b"replacement").unwrap();

        assert!(filesystem.save().is_err());
        cleanup_root(root, filesystem);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_open_file_allows_posix_replacement() {
        let root = temporary_root("windows-open-replace");
        fs::write(root.join("source"), b"source").unwrap();
        fs::write(root.join("destination"), b"destination").unwrap();
        let _open = open_file(&root.join("destination"), 0, false, 0).unwrap();
        let result = rename_replace(&root.join("source"), &root.join("destination"), false);
        assert!(result.is_ok(), "replace failed: {result:?}");
        drop(_open);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn open_handles_survive_protocol_state_restore() {
        let root = temporary_root("restore");
        fs::write(root.join("value.txt"), b"before").unwrap();
        let mut original = PassthroughFs::new(&root, true).unwrap();
        let lookup = original
            .handle(&request(FUSE_LOOKUP, 1, FUSE_ROOT_ID, b"value.txt\0"))
            .unwrap();
        let nodeid = u64::from_le_bytes(lookup[16..24].try_into().unwrap());
        let mut open_payload = Vec::new();
        put_u32(&mut open_payload, 0);
        put_u32(&mut open_payload, 0);
        let open = original
            .handle(&request(FUSE_OPEN, 2, nodeid, &open_payload))
            .unwrap();
        let handle = u64::from_le_bytes(open[16..24].try_into().unwrap());
        let state = original.save().unwrap();

        fs::write(root.join("value.txt"), b"after").unwrap();
        let mut restored = PassthroughFs::new(&root, true).unwrap();
        restored.load(&state).unwrap();
        let mut read_payload = Vec::new();
        put_u64(&mut read_payload, handle);
        put_u64(&mut read_payload, 0);
        put_u32(&mut read_payload, 16);
        read_payload.resize(40, 0);
        let read = restored
            .handle(&request(FUSE_READ, 3, nodeid, &read_payload))
            .unwrap();
        assert_eq!(&read[FUSE_OUT_HEADER_LEN..], b"after");
        drop(restored);
        drop(original);
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(target_os = "linux")]
fn file_identity(file: &File) -> ::std::io::Result<FileIdentity> {
    use ::std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(FileIdentity(metadata.dev(), metadata.ino()))
}

#[cfg(target_os = "windows")]
fn file_identity(file: &File) -> ::std::io::Result<FileIdentity> {
    use ::std::os::windows::io::AsRawHandle;
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
        .map_err(|_| ::std::io::Error::last_os_error())?;
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok(FileIdentity(
        u64::from(information.dwVolumeSerialNumber),
        index,
    ))
}

fn same_file(left: &File, right: &File) -> ::std::io::Result<bool> {
    Ok(file_identity(left)? == file_identity(right)?)
}

#[cfg(target_os = "windows")]
fn metadata_is_link(metadata: &Metadata) -> bool {
    use ::std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}
