//! The text of the host paths of an export plan, which OpenVMM's options carry.

use std::path::Path;

use crate::error::Result;
use crate::host::sandbox::refused;

/// Returns the text of a host path of an export plan, which OpenVMM's options carry.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a path that is not
/// valid UTF-8.
pub(super) fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| refused(format!("{} is not valid UTF-8", path.display())))
}
