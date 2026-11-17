//! The checks that the artifacts that a launch runs are regular files.

use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorCode, Result};
use crate::host::paths;

/// Checks that `path` is a regular file, as `require_file` of `nvx.py` does, and returns it, or
/// fails with [`ErrorCode::ArtifactMissing`].
pub(super) fn required(path: PathBuf, description: &str) -> Result<PathBuf> {
    require_artifact(&path, description)?;
    Ok(path)
}

/// Checks that an artifact that a launch runs, such as the OpenVMM executable, is a regular
/// file, as [`require_existing`] does, and fails with [`ErrorCode::ArtifactMissing`].
pub(crate) fn require_artifact(path: &Path, description: &str) -> Result<()> {
    require_existing(path, description, ErrorCode::ArtifactMissing)
}

/// Checks that `path` is a regular file, following links, as `require_file` of `nvx.py` does,
/// and fails with `code` and the message `{description} not found: {path}`, with `path` spelled
/// with its components alone, as [`Path::components`] reads them.
pub(crate) fn require_existing(path: &Path, description: &str, code: ErrorCode) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }
    Err(Error::new(
        code,
        format!(
            "{description} not found: {}",
            paths::normalized(path).display()
        ),
    ))
}
