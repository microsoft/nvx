//! Locating the OpenVMM executable and the NVX guest artifacts.
//!
//! [`Artifacts`] names the three files that a VM runs. They come from an extracted NVX release
//! archive, an NVX repository checkout, explicit paths, or a bundle that the build script staged
//! with the `bundled` feature. [`Artifacts::discover`] tries these sources in a fixed order so
//! consumers such as MXC need no NVX-specific configuration.
//!
//! [`ArtifactDir`] names every artifact of the guests that `nvx.py` runs, which [`Guest`]
//! describes, in a repository checkout or an extracted release:
//!
//! | Artifact | Repository layout | Release layout |
//! | --- | --- | --- |
//! | OpenVMM | `openvmm/target/release/openvmm[.exe]` | `bin/openvmm[.exe]` |
//! | Kernel | `build/vmlinux` | `guest/vmlinux` |
//! | Alpine initramfs | `build/initramfs.cpio.gz` | `guest/initramfs.cpio.gz` |
//! | Ubuntu initramfs | `build/initramfs-ubuntu.cpio.gz` | `guest/initramfs-ubuntu.cpio.gz` |
//! | Azure Linux initramfs | `build/initramfs-azurelinux.cpio.gz` | `guest/initramfs-azurelinux.cpio.gz` |
//! | Ubuntu distro layer | `build/ubuntu-distro.erofs` | `guest/ubuntu-distro.erofs` |
//!
//! Each initramfs has a package manifest beside it, `NAME.packages.json`, and the distro layer a
//! manifest, `NAME.manifest.json`. A release package holds the guest artifacts that its
//! `SOURCE-MANIFEST.json` declares, and `nvx-rs download` installs them into the repository
//! layout of a checkout, with the release's OpenVMM executable. The paths and messages are those
//! of `artifact_path`, `openvmm_binary_path`, and `require_file` in
//! `scripts/nvx_tools/common.py`, so that `nvx-rs` and the `nvx.py` commands that build,
//! install, benchmark, and test the guests agree on where each artifact is.

mod dir;
mod guest;
mod identity;
mod require;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::contract;
use crate::error::{Error, ErrorCode, Result};
use crate::host::platform;
use crate::host::store::RuntimeIdentity;

pub use self::dir::{ArtifactDir, ArtifactPaths, GuestPaths, Layout};
pub use self::guest::Guest;
#[cfg(feature = "cli")]
pub(crate) use self::require::require_artifact;
pub(crate) use self::require::require_existing;

/// Name of the release manifest at the root of a release archive or repository checkout.
pub const MANIFEST_NAME: &str = "SOURCE-MANIFEST.json";

/// Directory staged by the build script when the `bundled` feature is enabled.
#[cfg(feature = "bundled")]
const BUNDLED_DIR: Option<&str> = option_env!("NVX_BUNDLED_ARTIFACTS_DIR");

/// The OpenVMM executable and the NVX guest kernel and initramfs.
///
/// Every path is used as given. Resolve relative paths with [`absolute`] before a VM runs them,
/// and check that the files exist, because nothing here does.
///
/// Artifacts located in a release or a repository checkout keep its [`ArtifactDir`], whose
/// install metadata names their [`runtime_identity`](Self::runtime_identity). Artifacts named by
/// path, through [`new`](Self::new) or the explicit variables of [`discover`](Self::discover),
/// have none: their caller supplies the runtime identity of a launch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Artifacts {
    /// The `openvmm` executable.
    pub openvmm: PathBuf,
    /// The NVX guest kernel (`vmlinux`).
    pub kernel: PathBuf,
    /// The NVX Alpine initramfs (`initramfs.cpio.gz`): the guest userland and its control agent.
    pub initrd: PathBuf,
    /// The release or checkout in which the artifacts were located, if they were.
    directory: Option<ArtifactDir>,
}

impl Artifacts {
    /// Names the three artifacts that a VM needs.
    pub fn new(
        openvmm: impl Into<PathBuf>,
        kernel: impl Into<PathBuf>,
        initrd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            openvmm: openvmm.into(),
            kernel: kernel.into(),
            initrd: initrd.into(),
            directory: None,
        }
    }

    /// Returns the sandbox artifacts of `directory`, which they remember.
    fn located(directory: &ArtifactDir) -> Self {
        Self {
            directory: Some(directory.clone()),
            ..Self::new(
                directory.openvmm(),
                directory.kernel(),
                directory.initramfs(Guest::Alpine),
            )
        }
    }

    /// Returns the release or repository checkout in which the artifacts were located, or `None`
    /// for artifacts named by path.
    pub fn artifact_dir(&self) -> Option<&ArtifactDir> {
        self.directory.as_ref()
    }

    /// Returns the runtime identity of these artifacts, which the install metadata of the release
    /// or checkout in which they were located names, as [`ArtifactDir::runtime_identity`] reads
    /// it, without reading or hashing an artifact.
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for artifacts named by
    /// path, whose caller supplies their identity instead, for artifacts whose paths no longer
    /// name the ones of their directory, and as [`ArtifactDir::runtime_identity`] fails.
    pub fn runtime_identity(&self) -> Result<RuntimeIdentity> {
        let Some(directory) = &self.directory else {
            return Err(Error::unavailable(
                "the artifacts are named by path, so no install metadata identifies them: locate \
                 them in an NVX release or repository checkout, or supply their runtime identity",
            ));
        };
        let located = Self::located(directory);
        for (path, expected, description) in [
            (&self.openvmm, &located.openvmm, "OpenVMM executable"),
            (&self.kernel, &located.kernel, "guest kernel"),
            (&self.initrd, &located.initrd, "guest initramfs"),
        ] {
            if path != expected {
                return Err(Error::unavailable(format!(
                    "the {description} {} is not the one of {}, whose install metadata \
                     identifies the artifacts",
                    path.display(),
                    directory.root().display()
                )));
            }
        }
        directory.runtime_identity()
    }

    /// Checks that this host can run NVX VMs and that the three files exist.
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) otherwise, with the
    /// code [`ErrorCode::ArtifactMissing`] for a missing file. It does not check the hypervisor;
    /// [`probe_hypervisor`](super::platform::probe_hypervisor) does.
    pub fn probe(&self) -> Result<()> {
        if !platform::SUPPORTED {
            return Err(Error::unavailable(
                "the openvmm backend supports Linux and Windows hosts only",
            ));
        }
        for (path, description) in [
            (&self.openvmm, "OpenVMM executable"),
            (&self.kernel, "NVX guest kernel"),
            (&self.initrd, "NVX guest initramfs"),
        ] {
            if !path.is_file() {
                return Err(Error::new(
                    ErrorCode::ArtifactMissing,
                    format!("{description} not found: {}", path.display()),
                ));
            }
        }
        Ok(())
    }

    /// Uses an extracted NVX release archive: `bin/openvmm[.exe]`, `guest/vmlinux`, and
    /// `guest/initramfs.cpio.gz`.
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) when the release's
    /// `SOURCE-MANIFEST.json` declares an incompatible control contract.
    pub fn from_release_dir(release_dir: impl AsRef<Path>) -> Result<Self> {
        Self::from_artifact_dir(&ArtifactDir::release(release_dir.as_ref())?)
    }

    /// Uses an NVX repository checkout after `nvx-rs download` or a local build:
    /// `openvmm/target/release/openvmm[.exe]`, `build/vmlinux`, and `build/initramfs.cpio.gz`.
    pub fn from_repo_layout(repo_root: impl AsRef<Path>) -> Result<Self> {
        Self::from_artifact_dir(&ArtifactDir::repository(repo_root.as_ref())?)
    }

    /// Uses a directory in either the release or the repository layout.
    ///
    /// A repository checkout also has a `guest/` directory, which holds the guest sources, so the
    /// layouts are told apart by their built artifacts instead: a directory that contains
    /// `bin/openvmm[.exe]`, `guest/vmlinux`, or `guest/initramfs.cpio.gz` is a release, and any
    /// other directory is a repository checkout.
    pub fn from_dir(directory: impl AsRef<Path>) -> Result<Self> {
        Self::from_artifact_dir(&ArtifactDir::from_dir(directory.as_ref())?)
    }

    /// Uses the OpenVMM executable, guest kernel, and Alpine initramfs of `directory`, once its
    /// `SOURCE-MANIFEST.json` declares the control contract that this crate speaks.
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) as
    /// [`check_manifest`] does.
    fn from_artifact_dir(directory: &ArtifactDir) -> Result<Self> {
        check_manifest(&directory.root().join(MANIFEST_NAME))?;
        Ok(Self::located(directory))
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
    ///    `DEP_NVX_ARTIFACTS_DIR` for this purpose.
    /// 4. The directory staged by the build script ([`Artifacts::bundled`]).
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) when no source is
    /// present or the chosen source is unusable. It does not check that the files exist.
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

/// Implements artifact discovery with injectable environment and bundle sources for tests.
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
            return Err(Error::unavailable(format!(
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
    Err(Error::new(
        ErrorCode::ArtifactMissing,
        format!(
            "cannot locate the OpenVMM executable and NVX guest artifacts: set {} or {}, {}, and \
             {}, place an NVX release in a {:?} directory next to the executable, or build the \
             nvx crate with the bundled feature",
            Artifacts::ENV_DIR,
            Artifacts::ENV_OPENVMM,
            Artifacts::ENV_KERNEL,
            Artifacts::ENV_INITRD,
            Artifacts::BESIDE_EXE_DIR,
        ),
    ))
}

/// Returns whether `directory` is in the release layout: whether it contains `bin/openvmm[.exe]`,
/// `guest/vmlinux`, or `guest/initramfs.cpio.gz`.
fn is_release_dir(directory: &Path) -> bool {
    let guest = directory.join(RELEASE_GUEST_DIR);
    [
        directory.join(RELEASE_BIN_DIR).join(openvmm_executable()),
        guest.join(KERNEL_NAME),
        guest.join(Guest::Alpine.initramfs_name()),
    ]
    .iter()
    .any(|path| path.is_file())
}

/// Returns the file name of the OpenVMM executable on this host.
pub fn openvmm_executable() -> &'static str {
    if cfg!(windows) {
        "openvmm.exe"
    } else {
        "openvmm"
    }
}

/// Resolves a relative path against the current directory, failing with
/// [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable).
pub fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|error| {
        Error::unavailable(format!("cannot resolve {}", path.display())).with_source(error)
    })
}

/// Reads and parses the JSON file at `path`, naming it with `description` in errors.
fn read_json(path: &Path, description: &str) -> Result<Value> {
    let text = fs::read_to_string(path).map_err(|error| {
        Error::unavailable(format!("cannot read {description} {}", path.display()))
            .with_source(error)
    })?;
    serde_json::from_str(&text).map_err(|error| {
        Error::unavailable(format!(
            "{description} {} is not valid JSON",
            path.display()
        ))
        .with_source(error)
    })
}

/// Checks that an NVX `SOURCE-MANIFEST.json` declares the control contract this crate speaks,
/// failing with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) that names each
/// mismatching contract field.
pub fn check_manifest(path: &Path) -> Result<()> {
    let manifest = read_json(path, "NVX manifest")?;
    let mismatches = contract::manifest_mismatches(&manifest);
    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(Error::unavailable(format!(
            "NVX manifest {} does not declare the compatible OpenVMM contracts ({})",
            path.display(),
            mismatches.join("; ")
        )))
    }
}

/// File name of the guest kernel in both layouts.
pub const KERNEL_NAME: &str = "vmlinux";
/// Suffix of the package manifest that lists the packages of an initramfs.
pub const PACKAGE_MANIFEST_SUFFIX: &str = ".packages.json";
/// Suffix of the manifest of a distro layer, which binds the layer's digest and packages.
pub const DISTRO_MANIFEST_SUFFIX: &str = ".manifest.json";
/// Repository-layout directory of guest artifacts.
pub(crate) const REPO_BUILD_DIR: &str = "build";
/// Repository-layout path of the OpenVMM build output directory.
const REPO_OPENVMM_DIR: [&str; 3] = ["openvmm", "target", "release"];
/// Release-layout directory of the OpenVMM executable.
const RELEASE_BIN_DIR: &str = "bin";
/// Release-layout directory of the guest artifacts.
const RELEASE_GUEST_DIR: &str = "guest";
/// Name of the list of the SHA-256 digests of the files of a release, at its root.
pub const CHECKSUMS_NAME: &str = "SHA256SUMS";
/// Name of the build provenance of the OpenVMM executable, beside the guest artifacts of a
/// repository checkout.
pub const OPENVMM_PROVENANCE_NAME: &str = "openvmm.provenance.json";
/// Name of the build provenance of the guest kernel, beside it.
pub const KERNEL_PROVENANCE_NAME: &str = "vmlinux.provenance.json";
/// Name of the build provenance of the Alpine initramfs, beside it.
pub const INITRAMFS_PROVENANCE_NAME: &str = "initramfs.provenance.json";
/// Format of the build provenance that this crate reads.
const PROVENANCE_FORMAT: u64 = 1;
/// Longest file of install metadata that this crate reads, in bytes. A release's checksum list
/// holds one line for each of its files, and a build provenance a few fields.
const MAX_METADATA_LEN: u64 = 1 << 20;

#[cfg(test)]
mod tests;
