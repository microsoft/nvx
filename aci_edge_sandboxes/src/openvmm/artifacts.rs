//! Locating the OpenVMM executable and the NVX guest kernel and initramfs.
//!
//! [`Artifacts`] names the three files the backend runs. They come from an extracted NVX release
//! archive, an NVX repository checkout, explicit paths, or a bundle that the build script staged
//! with the `bundled` feature. [`Artifacts::discover`] tries these sources in a fixed order so
//! consumers such as MXC need no NVX-specific configuration.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::contract;
use crate::error::{Error, Result};

/// Name of the release manifest at the root of a release archive or repository checkout.
pub(crate) const MANIFEST_NAME: &str = "SOURCE-MANIFEST.json";
/// Release-layout path of the guest kernel.
const RELEASE_KERNEL: [&str; 2] = ["guest", "vmlinux"];
/// Release-layout path of the Alpine initramfs.
const RELEASE_INITRD: [&str; 2] = ["guest", "initramfs.cpio.gz"];
/// Repository-layout directory of guest artifacts.
const REPO_BUILD_DIR: &str = "build";

/// Directory staged by the build script when the `bundled` feature is enabled.
#[cfg(feature = "bundled")]
const BUNDLED_DIR: Option<&str> = option_env!("ACI_EDGE_SANDBOXES_BUNDLED_ARTIFACTS_DIR");

/// The OpenVMM executable and the NVX guest kernel and initramfs.
///
/// Every path is used as given; [`OpenVmmBackend`](super::OpenVmmBackend) resolves relative paths
/// and reports missing files as
/// [`ErrorCode::BackendUnavailable`](crate::ErrorCode::BackendUnavailable).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Artifacts {
    /// The `openvmm` executable.
    pub openvmm: PathBuf,
    /// The NVX guest kernel (`vmlinux`).
    pub kernel: PathBuf,
    /// The NVX Alpine initramfs (`initramfs.cpio.gz`): the guest userland and its control agent.
    pub initrd: PathBuf,
}

impl Artifacts {
    /// Names the three artifacts the backend needs.
    pub fn new(
        openvmm: impl Into<PathBuf>,
        kernel: impl Into<PathBuf>,
        initrd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            openvmm: openvmm.into(),
            kernel: kernel.into(),
            initrd: initrd.into(),
        }
    }

    /// Uses an extracted NVX release archive: `bin/openvmm[.exe]`, `guest/vmlinux`, and
    /// `guest/initramfs.cpio.gz`.
    ///
    /// Fails with [`ErrorCode::BackendUnavailable`](crate::ErrorCode::BackendUnavailable) when
    /// the release's `SOURCE-MANIFEST.json` declares an incompatible control contract.
    pub fn from_release_dir(release_dir: impl AsRef<Path>) -> Result<Self> {
        let root = absolute(release_dir.as_ref())?;
        check_manifest(&root.join(MANIFEST_NAME))?;
        let join = |parts: [&str; 2]| root.join(parts[0]).join(parts[1]);
        Ok(Self::new(
            root.join("bin").join(openvmm_executable()),
            join(RELEASE_KERNEL),
            join(RELEASE_INITRD),
        ))
    }

    /// Uses an NVX repository checkout after `nvx.py download` or a local build:
    /// `openvmm/target/release/openvmm[.exe]`, `build/vmlinux`, and `build/initramfs.cpio.gz`.
    pub fn from_repo_layout(repo_root: impl AsRef<Path>) -> Result<Self> {
        let root = absolute(repo_root.as_ref())?;
        check_manifest(&root.join(MANIFEST_NAME))?;
        let build = root.join(REPO_BUILD_DIR);
        Ok(Self::new(
            root.join("openvmm")
                .join("target")
                .join("release")
                .join(openvmm_executable()),
            build.join(RELEASE_KERNEL[1]),
            build.join(RELEASE_INITRD[1]),
        ))
    }

    /// Uses a directory in either the release or the repository layout.
    ///
    /// A repository checkout also has a `guest/` directory, which holds the guest sources, so the
    /// layouts are told apart by their built artifacts instead: a directory that contains
    /// `bin/openvmm[.exe]`, `guest/vmlinux`, or `guest/initramfs.cpio.gz` is a release, and any
    /// other directory is a repository checkout.
    pub fn from_dir(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        let release_artifacts = [
            directory.join("bin").join(openvmm_executable()),
            directory.join(RELEASE_KERNEL[0]).join(RELEASE_KERNEL[1]),
            directory.join(RELEASE_INITRD[0]).join(RELEASE_INITRD[1]),
        ];
        if release_artifacts.iter().any(|path| path.is_file()) {
            Self::from_release_dir(directory)
        } else {
            Self::from_repo_layout(directory)
        }
    }

    /// Returns the artifacts staged by the build script, if the crate was built with the
    /// `bundled` feature for Linux or Windows and the staged directory still exists.
    ///
    /// The staged directory lives in Cargo's build output, so it suits development and tests on
    /// the build host. Deployments copy it next to their executable instead; see
    /// [`Artifacts::discover`].
    pub fn bundled() -> Result<Option<Self>> {
        #[cfg(feature = "bundled")]
        if let Some(directory) = BUNDLED_DIR.map(Path::new)
            && directory.is_dir()
        {
            return Self::from_release_dir(directory).map(Some);
        }
        Ok(None)
    }

    /// Locates the artifacts on this host.
    ///
    /// Sources are tried in this order, and the first one present wins:
    ///
    /// 1. The [`ENV_OPENVMM`](Self::ENV_OPENVMM), [`ENV_KERNEL`](Self::ENV_KERNEL), and
    ///    [`ENV_INITRD`](Self::ENV_INITRD) variables, which must be set together.
    /// 2. The [`ENV_DIR`](Self::ENV_DIR) variable, naming a release or repository directory.
    /// 3. The [`BESIDE_EXE_DIR`](Self::BESIDE_EXE_DIR) directory next to the current executable,
    ///    in the release layout. Consumers ship the bundle there; with the `bundled` feature the
    ///    build script exports its staged directory to dependent build scripts as
    ///    `DEP_ACI_EDGE_SANDBOXES_ARTIFACTS_DIR` for this purpose.
    /// 4. The directory staged by the build script ([`Artifacts::bundled`]).
    ///
    /// Fails with [`ErrorCode::BackendUnavailable`](crate::ErrorCode::BackendUnavailable) when no
    /// source is present or the chosen source is unusable. It does not check that the files
    /// exist; [`AciEdgeSandbox::probe`](crate::AciEdgeSandbox::probe) does.
    pub fn discover() -> Result<Self> {
        let exe_dir = env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        discover_with(|name| env::var_os(name), exe_dir.as_deref(), Self::bundled)
    }

    /// Environment variable naming the `openvmm` executable.
    pub const ENV_OPENVMM: &'static str = "NVX_OPENVMM";
    /// Environment variable naming the guest kernel.
    pub const ENV_KERNEL: &'static str = "NVX_KERNEL";
    /// Environment variable naming the control initramfs.
    pub const ENV_INITRD: &'static str = "NVX_INITRD";
    /// Environment variable naming a release or repository directory.
    pub const ENV_DIR: &'static str = "NVX_ARTIFACTS_DIR";
    /// Name of the release-layout directory that [`Artifacts::discover`] looks for next to the
    /// current executable.
    pub const BESIDE_EXE_DIR: &'static str = "nvx";
}

fn discover_with(
    variable: impl Fn(&str) -> Option<OsString>,
    exe_dir: Option<&Path>,
    bundled: impl FnOnce() -> Result<Option<Artifacts>>,
) -> Result<Artifacts> {
    let explicit = [
        Artifacts::ENV_OPENVMM,
        Artifacts::ENV_KERNEL,
        Artifacts::ENV_INITRD,
    ]
    .map(|name| variable(name).filter(|value| !value.is_empty()));
    match explicit {
        [Some(openvmm), Some(kernel), Some(initrd)] => {
            return Ok(Artifacts::new(openvmm, kernel, initrd));
        }
        [None, None, None] => {}
        _ => {
            return Err(Error::backend_unavailable(format!(
                "set all or none of {}, {}, and {}",
                Artifacts::ENV_OPENVMM,
                Artifacts::ENV_KERNEL,
                Artifacts::ENV_INITRD
            )));
        }
    }
    if let Some(directory) = variable(Artifacts::ENV_DIR).filter(|value| !value.is_empty()) {
        return Artifacts::from_dir(PathBuf::from(directory));
    }
    if let Some(directory) = exe_dir.map(|directory| directory.join(Artifacts::BESIDE_EXE_DIR))
        && directory.join(MANIFEST_NAME).is_file()
    {
        return Artifacts::from_release_dir(directory);
    }
    if let Some(artifacts) = bundled()? {
        return Ok(artifacts);
    }
    Err(Error::backend_unavailable(format!(
        "cannot locate the OpenVMM executable and NVX guest artifacts: set {} or {}, {}, and {}, \
         place an NVX release in a {:?} directory next to the executable, or build the \
         aci_edge_sandboxes crate with the bundled feature",
        Artifacts::ENV_DIR,
        Artifacts::ENV_OPENVMM,
        Artifacts::ENV_KERNEL,
        Artifacts::ENV_INITRD,
        Artifacts::BESIDE_EXE_DIR,
    )))
}

pub(crate) fn openvmm_executable() -> &'static str {
    if cfg!(windows) {
        "openvmm.exe"
    } else {
        "openvmm"
    }
}

pub(crate) fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|error| {
        Error::backend_unavailable(format!("cannot resolve {}", path.display())).with_source(error)
    })
}

fn read_json(path: &Path, description: &str) -> Result<Value> {
    let text = fs::read_to_string(path).map_err(|error| {
        Error::backend_unavailable(format!("cannot read {description} {}", path.display()))
            .with_source(error)
    })?;
    serde_json::from_str(&text).map_err(|error| {
        Error::backend_unavailable(format!(
            "{description} {} is not valid JSON",
            path.display()
        ))
        .with_source(error)
    })
}

/// Checks that an NVX `SOURCE-MANIFEST.json` declares the control contract this crate speaks.
pub(crate) fn check_manifest(path: &Path) -> Result<()> {
    let manifest = read_json(path, "NVX manifest")?;
    if contract::manifest_is_compatible(&manifest) {
        Ok(())
    } else {
        Err(Error::backend_unavailable(format!(
            "NVX manifest {} does not declare the {} control contract",
            path.display(),
            contract::CONTROL_CONTRACT_REVISION
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::ErrorCode;

    const COMPATIBLE_MANIFEST: &str = r#"{"openvmm":{"microvm_abi_version":2,
        "control_session_protocol_version":1,
        "control_contract_revision":"nvx-microvm-v2-control-v2"}}"#;

    fn release(root: &Path) {
        for artifact in [
            Path::new("bin").join(openvmm_executable()),
            Path::new("guest").join("vmlinux"),
            Path::new("guest").join("initramfs.cpio.gz"),
        ] {
            let path = root.join(artifact);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"artifact").unwrap();
        }
        fs::write(root.join(MANIFEST_NAME), COMPATIBLE_MANIFEST).unwrap();
    }

    /// Creates a repository checkout with its tracked guest sources and no build outputs.
    fn checkout(root: &Path) {
        let sources = root.join("guest").join("common");
        fs::create_dir_all(&sources).unwrap();
        fs::write(sources.join("init"), b"#!/bin/sh").unwrap();
        fs::write(root.join(MANIFEST_NAME), COMPATIBLE_MANIFEST).unwrap();
    }

    fn no_bundle() -> Result<Option<Artifacts>> {
        Ok(None)
    }

    fn variables(pairs: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.as_os_str().to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn release_layout_names_the_guest_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        release(directory.path());
        let artifacts = Artifacts::from_dir(directory.path()).unwrap();
        let root = std::path::absolute(directory.path()).unwrap();
        assert_eq!(
            artifacts.openvmm,
            root.join("bin").join(openvmm_executable())
        );
        assert_eq!(artifacts.kernel, root.join("guest").join("vmlinux"));
        assert_eq!(
            artifacts.initrd,
            root.join("guest").join("initramfs.cpio.gz")
        );
    }

    #[test]
    fn repository_layout_is_detected_beside_the_guest_sources() {
        let directory = tempfile::tempdir().unwrap();
        checkout(directory.path());
        let root = std::path::absolute(directory.path()).unwrap();
        let expected = |artifacts: &Artifacts| {
            assert_eq!(artifacts.kernel, root.join("build").join("vmlinux"));
            assert_eq!(
                artifacts.initrd,
                root.join("build").join("initramfs.cpio.gz")
            );
            assert_eq!(
                artifacts.openvmm,
                root.join("openvmm")
                    .join("target")
                    .join("release")
                    .join(openvmm_executable())
            );
        };
        expected(&Artifacts::from_dir(directory.path()).unwrap());

        // Build outputs do not turn a checkout into a release.
        let build = directory.path().join("build");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("vmlinux"), b"kernel").unwrap();
        fs::write(build.join("initramfs.cpio.gz"), b"initrd").unwrap();
        expected(&Artifacts::from_dir(directory.path()).unwrap());
    }

    #[test]
    fn the_directory_variable_accepts_a_repository_checkout() {
        let directory = tempfile::tempdir().unwrap();
        checkout(directory.path());
        let artifacts = discover_with(
            variables(&[(Artifacts::ENV_DIR, directory.path())]),
            None,
            no_bundle,
        )
        .unwrap();
        let build = std::path::absolute(directory.path()).unwrap().join("build");
        assert_eq!(artifacts.kernel, build.join("vmlinux"));
    }

    #[test]
    fn this_repository_uses_the_repository_layout() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        if root.join(MANIFEST_NAME).is_file() {
            let artifacts = Artifacts::from_dir(root).unwrap();
            let kernel = std::path::absolute(root).unwrap().join("build");
            assert_eq!(artifacts.kernel, kernel.join("vmlinux"));
        }
    }

    #[test]
    fn explicit_variables_win_and_must_be_complete() {
        let directory = tempfile::tempdir().unwrap();
        release(directory.path());
        let (openvmm, kernel, initrd) = (Path::new("o"), Path::new("k"), Path::new("i"));
        let artifacts = discover_with(
            variables(&[
                (Artifacts::ENV_OPENVMM, openvmm),
                (Artifacts::ENV_KERNEL, kernel),
                (Artifacts::ENV_INITRD, initrd),
                (Artifacts::ENV_DIR, directory.path()),
            ]),
            None,
            no_bundle,
        )
        .unwrap();
        assert_eq!(artifacts, Artifacts::new(openvmm, kernel, initrd));

        let error = discover_with(
            variables(&[(Artifacts::ENV_OPENVMM, openvmm)]),
            None,
            no_bundle,
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendUnavailable);
    }

    #[test]
    fn the_directory_variable_precedes_the_executable_directory() {
        let variable_dir = tempfile::tempdir().unwrap();
        release(variable_dir.path());
        let exe_dir = tempfile::tempdir().unwrap();
        release(&exe_dir.path().join(Artifacts::BESIDE_EXE_DIR));

        let artifacts = discover_with(
            variables(&[(Artifacts::ENV_DIR, variable_dir.path())]),
            Some(exe_dir.path()),
            no_bundle,
        )
        .unwrap();
        assert!(
            artifacts
                .kernel
                .starts_with(std::path::absolute(variable_dir.path()).unwrap())
        );

        let artifacts = discover_with(variables(&[]), Some(exe_dir.path()), no_bundle).unwrap();
        assert!(artifacts.kernel.starts_with(
            std::path::absolute(exe_dir.path().join(Artifacts::BESIDE_EXE_DIR)).unwrap()
        ));
    }

    #[test]
    fn the_bundle_is_the_last_resort() {
        let exe_dir = tempfile::tempdir().unwrap();
        let bundle = Artifacts::new("bundled-openvmm", "bundled-kernel", "bundled-initrd");
        let expected = bundle.clone();
        let artifacts =
            discover_with(variables(&[]), Some(exe_dir.path()), || Ok(Some(bundle))).unwrap();
        assert_eq!(artifacts, expected);

        let error = discover_with(variables(&[]), Some(exe_dir.path()), no_bundle).unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendUnavailable);
        assert!(error.message().contains(Artifacts::ENV_DIR));
        assert!(
            error
                .message()
                .contains("build the aci_edge_sandboxes crate with the bundled feature")
        );
    }

    #[test]
    fn incompatible_manifests_are_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = directory.path().join(MANIFEST_NAME);
        fs::write(
            &manifest,
            r#"{"openvmm":{"microvm_abi_version":2,"control_session_protocol_version":2,
               "control_contract_revision":"nvx-microvm-v2-control-v2"}}"#,
        )
        .unwrap();
        let error = check_manifest(&manifest).unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendUnavailable);
        let error = check_manifest(&directory.path().join("missing.json")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendUnavailable);
    }

    #[test]
    fn repository_manifest_declares_the_supported_contract() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join(MANIFEST_NAME);
        if manifest.is_file() {
            check_manifest(&manifest).unwrap();
        }
    }

    #[cfg(feature = "bundled")]
    #[test]
    fn the_bundle_is_a_compatible_release() {
        if let Some(artifacts) = Artifacts::bundled().unwrap() {
            for path in [&artifacts.openvmm, &artifacts.kernel, &artifacts.initrd] {
                assert!(path.is_file(), "{} is missing", path.display());
            }
        }
    }
}
