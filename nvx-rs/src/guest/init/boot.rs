//! The fixed cold-boot sequence of the VM's shared layered root.

use std::ffi::{CString, OsStr};
use std::net::IpAddr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::activation::{self, View};
use crate::gpt::Expected;
use crate::guest::mappings::{Access, Mapper, MappingTable, Reserved, SourceRoot, TargetRoot};
use crate::guest::root::{self, Assembly, Device, Root, ShareChild, ShareKind};
use crate::guest::sys::FileDescriptor;
use crate::guest::terminal::Terminal;
use crate::guest::view;
use crate::protocol::cmdline::{self, Token};
use crate::protocol::export::{EXPORT_DIRECTORY, HostExport};
use crate::protocol::root::{
    DISTRO_MMIO_BASE, ImageFormat, RootLayout, RootUpper, SCRATCH_MMIO_BASE,
};
use crate::protocol::{self, ProtocolError};
use crate::{Error, Result};

mod account;

/// The real switched root and the descriptors and account defaults its control caller inherits.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "SL7.6b takes the initialized layered control handoff"
    )
)]
pub(super) struct Layered {
    pub(super) root: Root,
    pub(super) command_line: Vec<u8>,
    pub(super) control: FileDescriptor,
    pub(super) user: CString,
    pub(super) home: CString,
    pub(super) table: MappingTable,
    pub(super) mapper: Mapper,
    pub(super) export: Option<SourceRoot>,
}

/// Initializes every fixed input before the VM's only switch; MAPS uses the retained handles.
pub(super) fn cold_boot(command_line: &[u8]) -> Result<Layered> {
    let tokens = Tokens::parse(command_line)?;
    let mut root = root::assemble(&tokens.assembly).map_err(system_error)?;
    let export = match &tokens.filesystem {
        Filesystem::Export(export) => Some(
            root.mount_export(&export.tag, export.mode.into())
                .map_err(system_error)?,
        ),
        _ => None,
    };
    // The /run tmpfs must exist before a live share can be placed beneath it.
    root.mount_runtime(Some(Path::new("/run/nvx/time")))
        .map_err(system_error)?;
    if let Filesystem::Shared {
        tag,
        target,
        access,
        children,
    } = &tokens.filesystem
    {
        if children.is_empty() {
            root.mount_share(tag, target, *access)
                .map_err(system_error)?;
        } else {
            root.mount_aggregate(tag, *access, children)
                .map_err(system_error)?;
        }
    }
    let mut target = TargetRoot::open(root.path()).map_err(system_error)?;
    target
        .add_mount(&root.path().join("run"))
        .map_err(system_error)?;
    let mapper = Mapper::new(target, Reserved::container_root());
    let (user, home) = account::resolve(root.path(), tokens.identity, tokens.create_account)?;
    let control = Terminal::open(&tokens.control).map_err(system_error)?;
    control.configure_raw().map_err(system_error)?;
    view::apply(&mut root, &tokens.view, tokens.dns)?;
    root.switch().map_err(system_error)?;
    Ok(Layered {
        root,
        command_line: command_line.to_vec(),
        control: control.into_descriptor(),
        user,
        home,
        table: tokens.table,
        mapper,
        export,
    })
}

fn system_error(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::io(format!("failed to boot the layered root: {error}")).with_source(error)
}

/// The boot inputs after the existing root and view codecs have checked them.
struct Tokens {
    assembly: Assembly,
    view: View,
    dns: Option<IpAddr>,
    filesystem: Filesystem,
    table: MappingTable,
    identity: (u32, u32),
    create_account: bool,
    control: PathBuf,
}

/// OpenVMM's one filesystem device: a managed export or the CLI's live shares.
#[derive(Debug)]
enum Filesystem {
    None,
    Export(HostExport),
    Shared {
        tag: String,
        target: PathBuf,
        access: Access,
        children: Vec<ShareChild>,
    },
}

impl Tokens {
    fn parse(command_line: &[u8]) -> Result<Self> {
        let layout = RootLayout::parse(command_line)
            .map_err(protocol_error)?
            .ok_or_else(|| Error::unsupported("a layered cold boot needs root tokens"))?;
        let view = protocol::view::parse(command_line)
            .map_err(protocol_error)?
            .ok_or_else(|| Error::unsupported("a layered cold boot needs view tokens"))?;
        let table = MappingTable::announced(command_line).map_err(protocol_error)?;
        let filesystem = filesystem(command_line)?;
        if table.expected() != 0 && !matches!(filesystem, Filesystem::Export(_)) {
            return Err(Error::unsupported("nvx_maps needs the host export"));
        }
        let identity = parameter(command_line, b"nvx_identity")?
            .ok_or_else(|| Error::unsupported("a layered cold boot needs nvx_identity=UID:GID"))?;
        let ids: Vec<_> = identity.split(|&byte| byte == b':').collect();
        let [uid, gid] = ids[..] else {
            return Err(Error::unsupported("nvx_identity must be UID:GID"));
        };
        let id = |bytes| {
            super::parse_decimal(bytes)
                .filter(|&value| value != 0 && value < u64::from(u32::MAX))
                .map(|value| value as u32)
                .ok_or_else(|| Error::unsupported("nvx_identity needs nonzero decimal IDs"))
        };
        let create_account = match parameter(command_line, b"nvx_workload_account")? {
            None => false,
            Some(b"create") => true,
            Some(_) => return Err(Error::unsupported("nvx_workload_account must be create")),
        };
        let control = parameter(command_line, b"nvx_control_tty")?
            .ok_or_else(|| Error::unsupported("a layered cold boot needs nvx_control_tty"))?;
        if control.is_empty() || !control.iter().all(u8::is_ascii_alphanumeric) {
            return Err(Error::unsupported(
                "nvx_control_tty must name a device in /dev",
            ));
        }
        Ok(Self {
            assembly: assembly(layout),
            view,
            dns: cmdline::dns_server(command_line).map_err(protocol_error)?,
            filesystem,
            table,
            identity: (id(uid)?, id(gid)?),
            create_account,
            control: Path::new("/dev").join(OsStr::from_bytes(control)),
        })
    }
}

fn protocol_error(error: ProtocolError) -> Error {
    Error::unsupported(error.to_string()).with_source(error)
}

/// Reads one CLI-owned parameter without accepting a bare name or a duplicate.
fn parameter<'a>(command_line: &'a [u8], name: &[u8]) -> Result<Option<&'a [u8]>> {
    let mut value = None;
    for token in cmdline::init_tokens(command_line).filter(|token| token.name == name) {
        if value.is_some() || token.value.is_none() {
            return Err(Error::unsupported(format!(
                "{} is repeated or has no value",
                name.escape_ascii()
            )));
        }
        value = token.value;
    }
    Ok(value)
}

fn assembly(layout: RootLayout) -> Assembly {
    let image = match layout.image.format {
        ImageFormat::Erofs => root::Image::Erofs {
            uuid: layout.image.identity,
        },
        ImageFormat::Gpt => root::Image::Gpt {
            expected: layout
                .image
                .identity
                .map_or_else(Expected::new, |guid| Expected::new().disk_guid(guid)),
            first_partition_bound: usize::try_from(
                layout.gpt_first_partition_bound.unwrap_or(u64::MAX),
            )
            .unwrap_or(usize::MAX),
        },
    };
    let upper = match layout.upper {
        RootUpper::Ram => root::Upper::Tmpfs {
            max_memory_percent: layout.max_memory_percent,
        },
        RootUpper::Scratch => root::Upper::Scratch {
            device: Device {
                mmio: SCRATCH_MMIO_BASE,
                sectors: None,
            },
            journal: match layout.journal.unwrap_or(activation::Journal::Allowed) {
                activation::Journal::Allowed => root::Journal::Allowed,
                activation::Journal::Forbidden => root::Journal::Forbidden,
            },
        },
    };
    Assembly {
        runtime: PathBuf::from("/run/nvx"),
        distro: Device {
            mmio: DISTRO_MMIO_BASE,
            sectors: None,
        },
        image,
        upper,
        overlay: match layout
            .overlay
            .unwrap_or(activation::OverlayMode::TrustedLowers)
        {
            activation::OverlayMode::TrustedLowers => root::OverlayMode::TrustedLowers,
            activation::OverlayMode::UntrustedLowers => root::OverlayMode::UntrustedLowers,
        },
        timeout: Duration::from_secs(30),
    }
}

fn filesystem(command_line: &[u8]) -> Result<Filesystem> {
    let directory = parameter(command_line, b"virtfs_dir")?;
    let tag = parameter(command_line, b"virtfs_tag")?;
    let mode = parameter(command_line, b"virtfs_mode")?;
    let aggregate = parameter(command_line, b"virtfs_aggregate")?;
    let children = cmdline::init_tokens(command_line)
        .filter(|token| token.name == b"nvx_share")
        .map(|Token { value, .. }| {
            value
                .ok_or_else(|| Error::unsupported("nvx_share has no value"))
                .and_then(share_child)
        })
        .collect::<Result<Vec<_>>>()?;
    if directory == Some(EXPORT_DIRECTORY.as_bytes()) && aggregate.is_some() {
        return HostExport::parse(command_line)
            .map_err(protocol_error)
            .map(|export| Filesystem::Export(export.expect("the export names its directory")));
    }
    let (Some(directory), Some(tag), Some(mode)) = (directory, tag, mode) else {
        if directory.is_none()
            && tag.is_none()
            && mode.is_none()
            && aggregate.is_none()
            && children.is_empty()
        {
            return Ok(Filesystem::None);
        }
        return Err(Error::unsupported("the live share bootstrap is incomplete"));
    };
    let tag = std::str::from_utf8(tag).map_err(|error| {
        Error::unsupported("the live share tag is not ASCII").with_source(error)
    })?;
    if tag.is_empty()
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(Error::unsupported("the live share tag is invalid"));
    }
    let target = PathBuf::from(OsStr::from_bytes(directory));
    match aggregate {
        Some(b"1") if directory == b"/run/nvx/shares" && !children.is_empty() => {}
        None if children.is_empty() => {}
        _ => return Err(Error::unsupported("the live share aggregate is invalid")),
    }
    Ok(Filesystem::Shared {
        tag: tag.to_owned(),
        target,
        access: access(mode)?,
        children,
    })
}

fn access(mode: &[u8]) -> Result<Access> {
    match mode {
        b"ro" => Ok(Access::ReadOnly),
        b"rw" => Ok(Access::ReadWrite),
        _ => Err(Error::unsupported("a live share mode must be ro or rw")),
    }
}

fn share_child(value: &[u8]) -> Result<ShareChild> {
    let fields: Vec<_> = value.split(|&byte| byte == b',').collect();
    let (name, target, mode, kind) = match fields[..] {
        [name, target, mode] => (name, target, mode, ShareKind::Directory),
        [name, target, mode, b"file"] => (name, target, mode, ShareKind::File),
        _ => {
            return Err(Error::unsupported(
                "nvx_share must be CHILD,TARGET,MODE[,file]",
            ));
        }
    };
    let name = std::str::from_utf8(name)
        .map_err(|error| Error::unsupported("the share child is not ASCII").with_source(error))?;
    Ok(ShareChild {
        name: name.to_owned(),
        target: PathBuf::from(OsStr::from_bytes(target)),
        access: access(mode)?,
        kind,
    })
}

#[cfg(test)]
mod tests;
