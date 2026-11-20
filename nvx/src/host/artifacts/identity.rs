//! The identities of artifacts, as the install metadata of a release or a repository checkout
//! names them.

use std::fs;
use std::io::{self, Read};
use std::path::Path;

use serde_json::Value;

use super::{MAX_METADATA_LEN, PROVENANCE_FORMAT};
use crate::error::{Error, Result};
use crate::host::runtime::ArtifactIdentity;

/// Returns the identity of an artifact whose SHA-256 digest is `digest`, if it holds 64 lowercase
/// hexadecimal digits, as `nvx.py` writes digests and requires them.
pub(super) fn sha256_identity(digest: &str) -> Option<ArtifactIdentity> {
    if digest
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        ArtifactIdentity::sha256(digest).ok()
    } else {
        None
    }
}

/// Reads the file of install metadata at `path`, which messages call `description`, as UTF-8 text
/// of at most [`MAX_METADATA_LEN`] bytes, a limit that is checked before the encoding. A missing
/// file fails with `advice` on how to provide it.
///
/// The open does not wait, as it would on Unix for a FIFO without a writer, and a file that is
/// not a regular file is refused before anything is read from it, as a FIFO whose writer keeps it
/// open would keep a read waiting: a sandbox's transitions read the metadata while they hold its
/// lock.
pub(super) fn read_metadata(path: &Path, description: &str, advice: &str) -> Result<String> {
    let unreadable = |error: io::Error| {
        if error.kind() == io::ErrorKind::NotFound {
            Error::unavailable(format!(
                "the {description} {} is missing, so the runtime cannot be identified: {advice}",
                path.display()
            ))
        } else {
            Error::unavailable(format!("cannot read the {description} {}", path.display()))
                .with_source(error)
        }
    };
    let not_regular = || {
        Error::unavailable(format!(
            "the {description} {} is not a regular file",
            path.display()
        ))
    };
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // Reads of a regular file do not depend on the flag.
        options.custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits());
    }
    let opened = options.open(path);
    #[cfg(unix)]
    {
        // A socket refuses an open.
        if let Err(error) = &opened
            && error.raw_os_error() == Some(nix::errno::Errno::ENXIO as i32)
        {
            return Err(not_regular());
        }
    }
    let file = opened.map_err(unreadable)?;
    if !file.metadata().map_err(unreadable)?.file_type().is_file() {
        return Err(not_regular());
    }
    let mut bytes = Vec::new();
    file.take(MAX_METADATA_LEN + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > MAX_METADATA_LEN {
        return Err(Error::unavailable(format!(
            "the {description} {} is larger than {MAX_METADATA_LEN} bytes",
            path.display()
        )));
    }
    String::from_utf8(bytes).map_err(|error| {
        Error::unavailable(format!(
            "the {description} {} is not UTF-8 text",
            path.display()
        ))
        .with_source(error)
    })
}

/// Returns the identity of the artifact `what` that the build provenance at `path` records: the
/// SHA-256 digest of its `field`, in a provenance of format [`PROVENANCE_FORMAT`].
pub(super) fn provenance_identity(
    path: &Path,
    what: &str,
    field: &str,
) -> Result<ArtifactIdentity> {
    let description = format!("{what} build provenance");
    let text = read_metadata(
        path,
        &description,
        "build the artifact with nvx.py, or install a release with nvx download",
    )?;
    let invalid = |problem: &str| {
        Error::unavailable(format!("the {description} {} {problem}", path.display()))
    };
    let provenance: Value = serde_json::from_str(&text)
        .map_err(|error| invalid("is not valid JSON").with_source(error))?;
    if provenance.get("format").and_then(Value::as_u64) != Some(PROVENANCE_FORMAT) {
        return Err(invalid(&format!(
            "does not have format {PROVENANCE_FORMAT}"
        )));
    }
    provenance
        .get(field)
        .and_then(Value::as_str)
        .and_then(sha256_identity)
        .ok_or_else(|| invalid(&format!("has no SHA-256 digest in {field}")))
}

/// Opens a bounded-header input without waiting for a substituted pipe or socket.
fn binary_input(path: &Path) -> Result<fs::File> {
    let invalid = |error| {
        Error::unavailable(format!("cannot inspect binary artifact {}", path.display()))
            .with_source(error)
    };
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits());
    }
    let file = options.open(path).map_err(invalid)?;
    if !file.metadata().map_err(invalid)?.is_file() {
        return Err(Error::unavailable(format!(
            "binary artifact {} is not a regular file",
            path.display()
        )));
    }
    Ok(file)
}

/// Rejects foreign or truncated native VMM products at the CLI staging boundary.
#[cfg(windows)]
pub(crate) fn check_arm64_pe(path: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom};

    let invalid =
        |problem: &str| Error::unavailable(format!("ARM64 OpenVMM {}: {problem}", path.display()));
    let mut file = binary_input(path)?;
    let length = file
        .metadata()
        .map_err(|error| invalid("cannot inspect executable length").with_source(error))?
        .len();
    let mut dos = [0; 64];
    file.read_exact(&mut dos)
        .map_err(|error| invalid("truncated DOS header").with_source(error))?;
    if &dos[..2] != b"MZ" {
        return Err(invalid("not a Windows PE executable"));
    }
    let offset = u64::from(u32::from_le_bytes(
        dos[60..64].try_into().expect("fixed header"),
    ));
    if offset < 64 || offset.checked_add(6).is_none_or(|end| end > length) {
        return Err(invalid("PE header offset is outside the executable"));
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| invalid("cannot seek PE header").with_source(error))?;
    let mut pe = [0; 6];
    file.read_exact(&mut pe)
        .map_err(|error| invalid("truncated PE header").with_source(error))?;
    if &pe[..4] != b"PE\0\0" || u16::from_le_bytes([pe[4], pe[5]]) != 0xaa64 {
        return Err(invalid(
            "executable is not native ARM64 (PE machine 0xaa64)",
        ));
    }
    Ok(())
}

/// Checks the same direct-Image header contract as the normal ARM kernel producer.
pub(super) fn check_arm64_image(path: &Path) -> Result<()> {
    let mut file = binary_input(path)?;
    let length = file
        .metadata()
        .map_err(|error| {
            Error::unavailable(format!("cannot inspect ARM Image {}", path.display()))
                .with_source(error)
        })?
        .len();
    let mut header = [0; 64];
    file.read_exact(&mut header).map_err(|error| {
        Error::unavailable(format!("truncated ARM Image header {}", path.display()))
            .with_source(error)
    })?;
    arm64_image_header(&header, length).map_err(|problem| {
        Error::unsupported(format!("ARM64 kernel {}: {problem}", path.display()))
    })
}

/// Checks the uncompressed little-endian 4-KiB ARM Image header and reserved memory footprint.
fn arm64_image_header(header: &[u8; 64], length: u64) -> std::result::Result<(), &'static str> {
    if header[56..60] != *b"ARM\x64" {
        return Err("not an uncompressed ARM64 Linux Image");
    }
    let word =
        |offset| u64::from_le_bytes(header[offset..offset + 8].try_into().expect("fixed header"));
    let offset = word(8);
    let size = word(16);
    let flags = word(24);
    if flags & 1 != 0 || (flags >> 1) & 3 != 1 || flags & !0xf != 0 || flags & 8 == 0 {
        return Err("Image requires little-endian 4-KiB pages and reservation-safe placement");
    }
    if size < length || offset % 4096 != 0 || offset.checked_add(size).is_none() {
        return Err("invalid Image footprint or text offset");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
