//! The identities of artifacts, as the install metadata of a release or a repository checkout
//! names them.

use std::fs;
use std::io::{self, Read};
use std::path::Path;

use serde_json::Value;

use super::{MAX_METADATA_LEN, PROVENANCE_FORMAT};
use crate::error::{Error, Result};
use crate::host::runtime::{ArtifactIdentity, SHA256_HEX_LEN};

/// Returns whether `text` is a SHA-256 digest as `nvx.py` writes digests and requires them, in
/// build provenance and in a release's `SHA256SUMS`: [`SHA256_HEX_LEN`] lowercase hexadecimal
/// digits.
pub(crate) fn is_sha256_digest(text: &str) -> bool {
    text.len() == SHA256_HEX_LEN
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Returns the identity of an artifact whose SHA-256 digest is `digest`, if
/// [`is_sha256_digest`] accepts it.
pub(super) fn sha256_identity(digest: &str) -> Option<ArtifactIdentity> {
    if is_sha256_digest(digest) {
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
