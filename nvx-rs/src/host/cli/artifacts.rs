//! `nvx-rs artifacts`: the paths of the artifacts that `nvx.py` runs, as JSON, and the artifact
//! root that every command uses.
//!
//! The root is the directory that `--root` names, in the layout that its built artifacts reveal,
//! as [`ArtifactDir::from_dir`] tells a release from a repository checkout. Without `--root`, it
//! is the release whose `bin/` holds the executable, or the repository checkout whose
//! `nvx-rs/target/` holds it, each with its `SOURCE-MANIFEST.json`. The `NVX_OPENVMM`
//! environment variable names an OpenVMM executable to run instead of the root's, such as a test
//! double.

use std::path::{Path, PathBuf};

use super::args::Parser;
use super::{Exit, Result, output};
use crate::host::artifacts::{ArtifactDir, ArtifactPaths, MANIFEST_NAME, absolute};
use crate::host::python::builtins::Int;
use crate::host::python::dumps::{self, Value};

/// The environment variable that names the OpenVMM executable to run instead of the root's.
pub(crate) const OPENVMM_VARIABLE: &str = "NVX_OPENVMM";

/// The name of the parser in usage messages.
const PROG: &str = "nvx-rs artifacts";
/// The usage of the parser.
const USAGE: &str = "usage: nvx-rs artifacts [-h]";
/// The help of the parser.
const HELP: &str = "\
usage: nvx-rs artifacts [-h]

Print the paths of the NVX artifacts as a JSON object: the layout and root of
the artifact directory, the OpenVMM executable, the guest kernel, and for each
guest, its name and distribution, its initramfs and package manifest, and its
distro layer and manifest, or null.

options:
  -h, --help  show this help message and exit";

/// Parses the arguments of `artifacts` and returns those that it does not recognize.
pub(super) fn parse(arguments: &[String]) -> Result<Vec<String>> {
    Parser::new(&[], &[])
        .parse(arguments, &mut |_: &'static str, _: Option<&str>| Ok(()))
        .map_err(|stop| Exit::from_stop(stop, PROG, USAGE, HELP))
}

/// Prints the paths of the artifacts of the root.
pub(super) fn run(root: Option<&str>) -> Result {
    let located = locate(root)?;
    let mut paths: ArtifactPaths = located.dir.paths();
    paths.openvmm = located.openvmm;
    let value = serde_json::to_value(&paths)
        .map_err(|error| Exit::error(format!("cannot encode the artifact paths: {error}")))?;
    output::print_stdout(&dumps::dumps(&json_value(&value)))
        .map_err(|error| Exit::error(output::stream_error(&error)))
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
                Exit::error(format!("cannot locate the nvx-rs executable: {error}"))
            })?;
            default_dir(&executable).ok_or_else(|| {
                Exit::error(
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
                return Err(Exit::error(format!(
                    "{OPENVMM_VARIABLE} is not valid Unicode"
                )));
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

/// Returns the [`Value`] of a JSON value without floats, as `serde_json` serializes the paths.
fn json_value(value: &serde_json::Value) -> Value {
    use serde_json::Value as Json;

    match value {
        Json::Null => Value::Null,
        Json::Bool(value) => Value::Bool(*value),
        Json::Number(number) => {
            let text = number.to_string();
            Int::parse(&text).map_or(Value::Str(text), Value::Int)
        }
        Json::String(text) => Value::Str(text.clone()),
        Json::Array(items) => Value::List(items.iter().map(json_value).collect()),
        Json::Object(members) => Value::Dict(
            members
                .iter()
                .map(|(name, member)| (name.clone(), json_value(member)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::host::artifacts::Layout;

    #[test]
    fn executables_find_the_release_or_checkout_that_holds_them() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let executable = |path: &str| root.join(path).join("nvx-rs");
        // Without a manifest, nothing is recognized.
        assert!(default_dir(&executable("bin")).is_none());
        fs::write(root.join(MANIFEST_NAME), "{}").unwrap();
        let release = default_dir(&executable("bin")).unwrap().unwrap();
        assert_eq!(release.layout(), Layout::Release);
        assert_eq!(release.root(), absolute(root).unwrap());
        for build in [
            "nvx-rs/target/release",
            "nvx-rs/target/x86_64-unknown-linux-musl/debug",
        ] {
            let checkout = default_dir(&executable(build)).unwrap().unwrap();
            assert_eq!(checkout.layout(), Layout::Repository, "{build}");
            assert_eq!(checkout.root(), absolute(root).unwrap(), "{build}");
        }
        assert!(default_dir(&executable("target/release")).is_none());
        assert!(default_dir(&executable("elsewhere")).is_none());
    }

    #[test]
    fn json_values_keep_their_shape() {
        let value = serde_json::json!({"b": [null, true, "x"], "a": {}});
        assert_eq!(
            dumps::dumps(&json_value(&value)),
            "{\n  \"a\": {},\n  \"b\": [\n    null,\n    true,\n    \"x\"\n  ]\n}"
        );
    }
}
