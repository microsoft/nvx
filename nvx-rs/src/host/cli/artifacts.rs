//! `nvx-rs artifacts`: the paths of the NVX artifacts, as JSON, and the artifact root that every
//! command uses.
//!
//! The root is the directory that `--root` names, in the layout that its built artifacts reveal,
//! as [`ArtifactDir::from_dir`] tells a release from a repository checkout. Without `--root`, it
//! is the release whose `bin/` holds the executable, or the repository checkout whose
//! `nvx-rs/target/` holds it, each with its `SOURCE-MANIFEST.json`. The `NVX_OPENVMM`
//! environment variable names an OpenVMM executable to run instead of the root's, such as a test
//! double.

use std::path::{Path, PathBuf};

use super::args::{Parser, Syntax};
use super::json::pretty_serde;
use super::{Exit, Result, output};
use crate::ErrorCode;
use crate::host::artifacts::{ArtifactDir, ArtifactPaths, MANIFEST_NAME, absolute};

/// The environment variable that names the OpenVMM executable to run instead of the root's.
pub(crate) const OPENVMM_VARIABLE: &str = "NVX_OPENVMM";

/// The usage and help of the parser.
pub(super) const SYNTAX: Syntax = Syntax {
    command: "artifacts",
    arguments: "[-h]",
    help: "\
Print the paths of the NVX artifacts as a JSON object: the layout and root of
the artifact directory, the OpenVMM executable, the guest kernel, and for each
guest, its name and distribution, its initramfs and package manifest, and its
distro layer and manifest, or null.

options:
  -h, --help  show this help message and exit",
};

/// Parses the arguments of `artifacts`, and returns those that it does not recognize.
pub(super) fn parse(arguments: &[String]) -> Result<Vec<String>> {
    Parser::new(&[], &[])
        .parse(arguments, &mut |_: &'static str, _: Option<&str>| Ok(()))
        .map_err(|stop| Exit::from_stop(stop, &SYNTAX))
}

/// Prints the paths of the artifacts of the root.
pub(super) fn run(root: Option<&str>) -> Result {
    let located = locate(root)?;
    let mut paths: ArtifactPaths = located.dir.paths();
    paths.openvmm = located.openvmm;
    let text = pretty_serde(&paths, "artifact paths")?;
    output::print_stdout(&text).map_err(Exit::from_os_error)
}

/// The artifacts that a command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Located {
    /// The artifact directory.
    pub(crate) dir: ArtifactDir,
    /// The OpenVMM executable: [`OPENVMM_VARIABLE`] if set, or the directory's.
    pub(crate) openvmm: PathBuf,
}

/// Returns the artifacts of `--root`, or of the directory that holds the executable.
pub(crate) fn locate(root: Option<&str>) -> Result<Located> {
    let dir = match root {
        Some(root) => ArtifactDir::from_dir(root)?,
        None => {
            let executable = std::env::current_exe().map_err(|error| {
                Exit::error(
                    ErrorCode::Io,
                    format!("cannot locate the nvx-rs executable: {error}"),
                )
            })?;
            default_dir(&executable).ok_or_else(|| {
                Exit::error(
                    ErrorCode::ArtifactMissing,
                    "cannot locate the NVX artifacts: pass --root with an NVX repository \
                     checkout or extracted release, or run the nvx-rs of a release's bin \
                     directory or of a checkout's nvx-rs/target directory",
                )
            })??
        }
    };
    let openvmm = match std::env::var_os(OPENVMM_VARIABLE).filter(|value| !value.is_empty()) {
        Some(value) => {
            if value.to_str().is_none() {
                return Err(Exit::error(
                    ErrorCode::Unavailable,
                    format!("{OPENVMM_VARIABLE} is not valid Unicode"),
                ));
            }
            absolute(Path::new(&value))?
        }
        None => dir.openvmm(),
    };
    Ok(Located { dir, openvmm })
}

/// Returns the artifact directory that holds `executable`, if any: the release whose `bin/`
/// holds it, or the repository checkout whose `nvx-rs/target/` holds it, at any depth below, as
/// for a build for another target. Either must have its `SOURCE-MANIFEST.json`.
fn default_dir(executable: &Path) -> Option<crate::Result<ArtifactDir>> {
    let directory = executable.parent()?;
    let has_manifest = |root: &Path| root.join(MANIFEST_NAME).is_file();
    if directory.file_name().is_some_and(|name| name == "bin")
        && let Some(root) = directory.parent()
        && has_manifest(root)
    {
        return Some(ArtifactDir::release(root));
    }
    let target = directory.ancestors().find(|ancestor| {
        ancestor.file_name().is_some_and(|name| name == "target")
            && ancestor
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "nvx-rs")
    })?;
    let root = target.parent()?.parent()?;
    has_manifest(root).then(|| ArtifactDir::repository(root))
}

#[cfg(test)]
mod tests;
