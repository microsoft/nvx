//! The sandbox launch profile: a microVM that runs one workload in a container over a read-only
//! distro image and a writable upper, with live virtio-fs shares.
//!
//! This is the profile of `nvx-rs sandbox`: its documented limits and option semantics, with
//! native Rust input validation and diagnostics:
//!
//! - The private `layer` (feature `cli`): `SandboxLayer`, the distro layer of a sandbox, the one
//!   `LayerRole` that a sandbox takes: an EROFS filesystem, as `nvx-rs sandbox` names it with
//!   `ROLE,PATH,EROFS_UUID`, or a GPT disk.
//! - [`SandboxMount`]: a requested live share of a host directory or regular file at a guest
//!   target, with its access mode, owner, and access-policy paths; [`mount`] holds the rules of
//!   guest targets and the checks of host paths, and plans the shares of a sandbox through the
//!   planner of every NVX caller, [`host::filesystem`](crate::host::filesystem).
//! - [`options`]: the checks that `nvx-rs sandbox` and `nvx-rs run` apply to their options: the
//!   attribution of access-policy paths to shares, the systemd entrypoint check, the targets of
//!   `nvx-rs run` shares, and the managed exec environment.
//! - The private `distro` (feature `cli`): the binding of the distro layer, which explicit
//!   verification makes from the layer and its manifest, and which a launch checks from metadata
//!   alone.
//! - The private `scratch` (feature `cli`): the binding of the scratch image, its identity, kind,
//!   and length, which a launch checks from metadata alone, and the claim of the image by one
//!   sandbox at a time.
//! - The block geometry that OpenVMM presents for a block device, its length and its logical and
//!   physical block sizes, which the crate reads through an open file of the device's image, as
//!   OpenVMM reads it.
//!
//! Workload IDs are ASCII decimal integers in their documented ranges. Layer UUIDs use their
//! canonical lowercase hyphenated text. Host paths use the host's native flavor.
//!
//! # Errors
//!
//! A request that the profile refuses fails with
//! [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), and a missing image with
//! [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable). A host query that fails
//! is [`ErrorKind::Io`](crate::ErrorKind::Io), with the native I/O error and, where available,
//! the path quoted with Rust's debug formatting.

#[cfg(feature = "cli")]
pub(crate) mod distro;
pub(crate) mod geometry;
#[cfg(feature = "cli")]
pub(crate) mod layer;
pub mod mount;
pub mod options;
#[cfg(feature = "cli")]
pub(crate) mod scratch;

use std::fs::File;
use std::path::Path;

use self::geometry::BlockGeometry;
use self::mount::os_error;
pub use self::mount::{MountAccess, MountKind, MountOwner, MountPolicy, PolicyKind, SandboxMount};
use crate::error::{Error, ErrorCode, Result};
use crate::host::artifacts::require_existing;
use crate::protocol::cmdline;

/// The workload identity by default, `65534:65534`.
pub const DEFAULT_WORKLOAD_IDENTITY: (u32, u32) = (65534, 65534);
/// The entrypoint by default.
pub const DEFAULT_ENTRYPOINT: &str = "/bin/sh";
/// The hostname by default.
pub const DEFAULT_HOSTNAME: &str = "nvx-sandbox";

/// Returns the error of a request that the profile refuses.
pub(crate) fn refused(message: impl Into<String>) -> Error {
    Error::unsupported(message)
}

/// Parses a `--workload-user` value, `UID:GID`, with both IDs between 1 and 4294967294.
///
/// 4294967295 is refused: as `(uid_t)-1` it reads as "leave unchanged" to `setresuid`/`setresgid`,
/// so accepting it would leave a one-shot sandbox workload running as UID 0 and GID 0.
///
/// Each ID contains only ASCII decimal digits, without a sign or whitespace. Fails with
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn parse_workload_identity(value: &str) -> Result<(u32, u32)> {
    let fields: Vec<&str> = value.split(':').collect();
    let [uid, gid] = fields[..] else {
        return Err(refused("--workload-user must be UID:GID"));
    };
    if [uid, gid]
        .iter()
        .any(|field| field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(refused("--workload-user must contain numeric UID and GID"));
    }
    let id = |value: &str, name: &str| {
        parse_decimal::<u32>(value)
            .filter(|&id| id != 0 && id != u32::MAX)
            .ok_or_else(|| {
                refused(format!(
                    "--workload-user {name} must be between 1 and 4294967294"
                ))
            })
    };
    Ok((id(uid, "UID")?, id(gid, "GID")?))
}

/// Parses nonempty ASCII decimal digits into a native integer, refusing signs and whitespace.
pub(crate) fn parse_decimal<T: std::str::FromStr>(value: &str) -> Option<T> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// Returns whether a value cannot stay one unquoted kernel token: whitespace, a control
/// character, a quote, or a UTF-8 byte that Linux treats as whitespace.
pub(crate) fn has_kernel_separator(value: &str) -> bool {
    value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
        || value
            .bytes()
            .any(|byte| byte == b'"' || cmdline::is_space(byte))
}

/// Checks that `path` is a regular file, following links, as `require_file` of `nvx.py` does.
///
/// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) and the message
/// `{description} not found: {path}`, with `path` spelled with its components alone, as
/// [`Path::components`] reads them.
pub fn require_file(path: &Path, description: &str) -> Result<()> {
    require_existing(path, description, ErrorCode::Unavailable)
}

/// The logical block size that OpenVMM presents for a sandbox block device, in bytes, of which the
/// device's length is a nonzero multiple.
const LOGICAL_BLOCK_SIZE: u32 = 512;

/// The physical block size that OpenVMM presents for a sandbox block device on Windows, in bytes.
#[cfg(windows)]
const WINDOWS_PHYSICAL_BLOCK_SIZE: u32 = 4096;

/// Returns the block geometry that OpenVMM presents for a sandbox block device whose file `file`
/// holds open, which messages call `description` at `path`, read through `file` as OpenVMM reads
/// it through its own open of the file: the file's length, which must be a nonzero multiple of
/// 512 bytes; logical blocks of 512 bytes; and physical blocks of the I/O block size of the file's
/// filesystem, its `st_blksize`, on Linux, of 4096 bytes on Windows, and of the logical block size
/// on other hosts, which must be a power of two of at least 512 bytes. It reads the file's
/// metadata alone.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a length or a physical
/// block size that OpenVMM refuses, and with [`ErrorKind::Io`](crate::ErrorKind::Io) if the file's
/// metadata cannot be read.
pub(crate) fn block_geometry(file: &File, path: &Path, description: &str) -> Result<BlockGeometry> {
    let shown = path.display();
    let metadata = file
        .metadata()
        .map_err(|error| os_error(&error, Some(path)))?;
    let length = metadata.len();
    if length == 0 || !length.is_multiple_of(u64::from(LOGICAL_BLOCK_SIZE)) {
        return Err(refused(format!(
            "the {description} {shown} of {length} bytes cannot be attached: OpenVMM attaches \
             block devices of nonzero multiples of {LOGICAL_BLOCK_SIZE} bytes"
        )));
    }
    #[cfg(target_os = "linux")]
    let (reported, physical) = {
        use std::os::unix::fs::MetadataExt;

        let size = metadata.blksize();
        (size, u32::try_from(size).ok())
    };
    #[cfg(windows)]
    let (reported, physical) = (
        u64::from(WINDOWS_PHYSICAL_BLOCK_SIZE),
        Some(WINDOWS_PHYSICAL_BLOCK_SIZE),
    );
    #[cfg(not(any(target_os = "linux", windows)))]
    let (reported, physical) = (u64::from(LOGICAL_BLOCK_SIZE), Some(LOGICAL_BLOCK_SIZE));
    match physical {
        Some(physical) if physical >= LOGICAL_BLOCK_SIZE && physical.is_power_of_two() => {
            Ok(BlockGeometry {
                length,
                logical_block_size: LOGICAL_BLOCK_SIZE,
                physical_block_size: physical,
            })
        }
        _ => Err(refused(format!(
            "the {description} {shown} cannot be attached: its filesystem's block size of \
             {reported} bytes is not a physical block size that OpenVMM presents, a power of two \
             from {LOGICAL_BLOCK_SIZE} bytes to 2 GiB"
        ))),
    }
}

#[cfg(test)]
mod tests;
