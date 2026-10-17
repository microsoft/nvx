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
//! `SOURCE-MANIFEST.json` declares, and `nvx.py download` installs them into the repository
//! layout of a checkout, with the release's OpenVMM executable. The paths and messages are those
//! of `artifact_path`, `openvmm_binary_path`, and `require_file` in
//! `scripts/nvx_tools/common.py`, so the `nvx.py` commands can look their artifacts up here.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::contract;
use crate::error::{Error, Result};
use crate::host::platform;
use crate::host::python::builtins::repr;
use crate::host::sandbox::require_file;

/// Name of the release manifest at the root of a release archive or repository checkout.
pub const MANIFEST_NAME: &str = "SOURCE-MANIFEST.json";
/// Release-layout path of the guest kernel.
const RELEASE_KERNEL: [&str; 2] = ["guest", "vmlinux"];
/// Release-layout path of the Alpine initramfs.
const RELEASE_INITRD: [&str; 2] = ["guest", "initramfs.cpio.gz"];
/// Repository-layout directory of guest artifacts.
const REPO_BUILD_DIR: &str = "build";

/// Directory staged by the build script when the `bundled` feature is enabled.
#[cfg(feature = "bundled")]
const BUNDLED_DIR: Option<&str> = option_env!("NVX_BUNDLED_ARTIFACTS_DIR");

/// The OpenVMM executable and the NVX guest kernel and initramfs.
///
/// Every path is used as given. Resolve relative paths with [`absolute`] before a VM runs them,
/// and check that the files exist, because nothing here does.
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
        }
    }

    /// Checks that this host can run NVX VMs and that the three files exist.
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) otherwise. It does not
    /// check the hypervisor; [`probe_hypervisor`](super::platform::probe_hypervisor) does.
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
                return Err(Error::unavailable(format!(
                    "{description} not found: {}",
                    path.display()
                )));
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
        if is_release_dir(directory) {
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
    Err(Error::unavailable(format!(
        "cannot locate the OpenVMM executable and NVX guest artifacts: set {} or {}, {}, and {}, \
         place an NVX release in a {:?} directory next to the executable, or build the nvx-rs \
         crate with the bundled feature",
        Artifacts::ENV_DIR,
        Artifacts::ENV_OPENVMM,
        Artifacts::ENV_KERNEL,
        Artifacts::ENV_INITRD,
        Artifacts::BESIDE_EXE_DIR,
    )))
}

/// Returns whether `directory` is in the release layout: whether it contains `bin/openvmm[.exe]`,
/// `guest/vmlinux`, or `guest/initramfs.cpio.gz`.
fn is_release_dir(directory: &Path) -> bool {
    [
        directory.join("bin").join(openvmm_executable()),
        directory.join(RELEASE_KERNEL[0]).join(RELEASE_KERNEL[1]),
        directory.join(RELEASE_INITRD[0]).join(RELEASE_INITRD[1]),
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
/// failing with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable).
pub fn check_manifest(path: &Path) -> Result<()> {
    let manifest = read_json(path, "NVX manifest")?;
    if contract::manifest_is_compatible(&manifest) {
        Ok(())
    } else {
        Err(Error::unavailable(format!(
            "NVX manifest {} does not declare the {} control contract",
            path.display(),
            contract::CONTROL_CONTRACT_REVISION
        )))
    }
}

/// File name of the guest kernel in both layouts.
pub const KERNEL_NAME: &str = "vmlinux";
/// Suffix of the package manifest that lists the packages of an initramfs.
pub const PACKAGE_MANIFEST_SUFFIX: &str = ".packages.json";
/// Suffix of the manifest of a distro layer, which binds the layer's digest and packages.
pub const DISTRO_MANIFEST_SUFFIX: &str = ".manifest.json";
/// Repository-layout path of the OpenVMM build output directory.
const REPO_OPENVMM_DIR: [&str; 3] = ["openvmm", "target", "release"];
/// Release-layout directory of the OpenVMM executable.
const RELEASE_BIN_DIR: &str = "bin";
/// Release-layout directory of the guest artifacts.
const RELEASE_GUEST_DIR: &str = "guest";

/// A Linux guest that `nvx.py` builds and runs, as `scripts/nvx_tools/guests.py` describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Guest {
    /// Alpine Linux: the default guest of `nvx.py run`, and the one that runs sandboxes.
    Alpine,
    /// Ubuntu Base, which also provides the distro layer of sandbox workloads.
    Ubuntu,
    /// Azure Linux.
    AzureLinux,
}

impl Guest {
    /// Every guest, in the order in which `nvx.py` lists them.
    pub const ALL: [Self; 3] = [Self::Alpine, Self::Ubuntu, Self::AzureLinux];

    /// Returns the guest's name in `nvx.py` options: `alpine`, `ubuntu`, or `azurelinux`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Alpine => "alpine",
            Self::Ubuntu => "ubuntu",
            Self::AzureLinux => "azurelinux",
        }
    }

    /// Returns the guest named `name`.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) and the message of
    /// `guest_descriptor` in `nvx.py`: `unsupported guest 'NAME'; choose alpine, ubuntu,
    /// azurelinux`.
    pub fn from_name(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|guest| guest.name() == name)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|guest| guest.name()).collect();
                Error::unsupported(format!(
                    "unsupported guest {}; choose {}",
                    repr(name),
                    names.join(", ")
                ))
            })
    }

    /// Returns the name of the guest's distribution, as messages show it: `Alpine Linux`,
    /// `Ubuntu Base`, or `Azure Linux`.
    pub const fn distribution(self) -> &'static str {
        match self {
            Self::Alpine => "Alpine Linux",
            Self::Ubuntu => "Ubuntu Base",
            Self::AzureLinux => "Azure Linux",
        }
    }

    /// Returns the file name of the guest's initramfs.
    pub const fn initramfs_name(self) -> &'static str {
        match self {
            Self::Alpine => "initramfs.cpio.gz",
            Self::Ubuntu => "initramfs-ubuntu.cpio.gz",
            Self::AzureLinux => "initramfs-azurelinux.cpio.gz",
        }
    }

    /// Returns the file name of the package manifest of the guest's initramfs.
    pub fn package_manifest_name(self) -> String {
        format!("{}{PACKAGE_MANIFEST_SUFFIX}", self.initramfs_name())
    }

    /// Returns the file name of the guest's EROFS distro layer, `GUEST-distro.erofs`, if the
    /// guest provides one: only Ubuntu does.
    pub const fn distro_layer_name(self) -> Option<&'static str> {
        match self {
            Self::Ubuntu => Some("ubuntu-distro.erofs"),
            Self::Alpine | Self::AzureLinux => None,
        }
    }

    /// Returns the file name of the manifest of the guest's distro layer, if it has one.
    pub fn distro_manifest_name(self) -> Option<String> {
        self.distro_layer_name()
            .map(|name| format!("{name}{DISTRO_MANIFEST_SUFFIX}"))
    }
}

impl fmt::Display for Guest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// The layout of a directory of NVX artifacts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// A repository checkout: the guest artifacts in `build/`, and OpenVMM in
    /// `openvmm/target/release/`, where a local build writes them and where `nvx.py download`
    /// installs a release.
    Repository,
    /// An extracted release package: the guest artifacts in `guest/`, and OpenVMM in `bin/`.
    Release,
}

/// A directory of the artifacts that `nvx.py` runs: a repository checkout or an extracted
/// release, with the paths that `artifact_path` and `openvmm_binary_path` of
/// `scripts/nvx_tools/common.py` give for a checkout.
///
/// Paths are joined to the directory, made absolute, without checking that they exist; the
/// `require_*` methods check them with the messages of `nvx.py`. Unlike [`Artifacts`], nothing
/// here reads the release's `SOURCE-MANIFEST.json`, which `nvx.py` does not check either.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactDir {
    /// The absolute directory.
    root: PathBuf,
    /// Its layout.
    layout: Layout,
}

impl ArtifactDir {
    /// Uses a repository checkout.
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) if a relative
    /// `root` meets an unreadable current directory.
    pub fn repository(root: impl AsRef<Path>) -> Result<Self> {
        Self::new(root.as_ref(), Layout::Repository)
    }

    /// Uses an extracted release package.
    ///
    /// Fails as [`repository`](Self::repository) does.
    pub fn release(root: impl AsRef<Path>) -> Result<Self> {
        Self::new(root.as_ref(), Layout::Release)
    }

    /// Uses a directory in the layout that its built artifacts reveal, as
    /// [`Artifacts::from_dir`] tells them apart: a directory that contains `bin/openvmm[.exe]`,
    /// `guest/vmlinux`, or `guest/initramfs.cpio.gz` is a release, and any other directory is a
    /// repository checkout.
    ///
    /// Fails as [`repository`](Self::repository) does.
    pub fn from_dir(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        let layout = if is_release_dir(directory) {
            Layout::Release
        } else {
            Layout::Repository
        };
        Self::new(directory, layout)
    }

    /// Returns the directory of `layout` at `root`, made absolute.
    fn new(root: &Path, layout: Layout) -> Result<Self> {
        Ok(Self {
            root: absolute(root)?,
            layout,
        })
    }

    /// Returns the absolute directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the directory's layout.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Returns the path of the guest artifact `name`, `artifact_path(name)` of `nvx.py`:
    /// `build/NAME` in a checkout, and `guest/NAME` in a release.
    pub fn artifact(&self, name: &str) -> PathBuf {
        let directory = match self.layout {
            Layout::Repository => REPO_BUILD_DIR,
            Layout::Release => RELEASE_GUEST_DIR,
        };
        self.root.join(directory).join(name)
    }

    /// Returns the path of the OpenVMM executable, `openvmm_binary_path()` of `nvx.py`:
    /// `openvmm/target/release/openvmm[.exe]` in a checkout, and `bin/openvmm[.exe]` in a
    /// release.
    pub fn openvmm(&self) -> PathBuf {
        let directory = match self.layout {
            Layout::Repository => REPO_OPENVMM_DIR.iter().collect(),
            Layout::Release => PathBuf::from(RELEASE_BIN_DIR),
        };
        self.root.join(directory).join(openvmm_executable())
    }

    /// Returns the path of the guest kernel, which every guest boots.
    pub fn kernel(&self) -> PathBuf {
        self.artifact(KERNEL_NAME)
    }

    /// Returns the path of a guest's initramfs.
    pub fn initramfs(&self, guest: Guest) -> PathBuf {
        self.artifact(guest.initramfs_name())
    }

    /// Returns the path of the package manifest of a guest's initramfs.
    pub fn package_manifest(&self, guest: Guest) -> PathBuf {
        self.artifact(&guest.package_manifest_name())
    }

    /// Returns the path of a guest's distro layer, if the guest provides one.
    pub fn distro_layer(&self, guest: Guest) -> Option<PathBuf> {
        guest.distro_layer_name().map(|name| self.artifact(name))
    }

    /// Returns the path of the manifest of a guest's distro layer, if the guest provides one.
    pub fn distro_manifest(&self, guest: Guest) -> Option<PathBuf> {
        guest
            .distro_manifest_name()
            .map(|name| self.artifact(&name))
    }

    /// Checks that the OpenVMM executable is a regular file and returns its path.
    ///
    /// Fails as [`require_file`] does, with the message `OpenVMM release binary not found:
    /// PATH`.
    pub fn require_openvmm(&self) -> Result<PathBuf> {
        required(self.openvmm(), "OpenVMM release binary")
    }

    /// Checks that the guest kernel is a regular file and returns its path.
    ///
    /// Fails as [`require_file`] does, with the message `Linux direct kernel not found: PATH`.
    pub fn require_kernel(&self) -> Result<PathBuf> {
        required(self.kernel(), "Linux direct kernel")
    }

    /// Checks that a guest's initramfs is a regular file and returns its path, as `nvx.py run`
    /// does before it boots the guest.
    ///
    /// Fails as [`require_file`] does, with the message `DISTRIBUTION initramfs not found:
    /// PATH`, such as `Ubuntu Base initramfs not found: PATH`.
    pub fn require_initramfs(&self, guest: Guest) -> Result<PathBuf> {
        required(
            self.initramfs(guest),
            &format!("{} initramfs", guest.distribution()),
        )
    }

    /// Returns the artifacts of a sandbox, as `nvx.py sandbox run` and `nvx.py sandbox start`
    /// require them: the OpenVMM executable, the kernel, and the Alpine initramfs, in that order.
    ///
    /// Fails as [`require_file`] does for the first one that is not a regular file, with the
    /// messages of [`require_openvmm`](Self::require_openvmm) and
    /// [`require_kernel`](Self::require_kernel), or `initramfs not found: PATH`.
    pub fn sandbox_artifacts(&self) -> Result<Artifacts> {
        let openvmm = self.require_openvmm()?;
        let kernel = self.require_kernel()?;
        let initrd = required(self.initramfs(Guest::Alpine), "initramfs")?;
        Ok(Artifacts::new(openvmm, kernel, initrd))
    }

    /// Returns the paths of every artifact, for a caller to show or record.
    pub fn paths(&self) -> ArtifactPaths {
        ArtifactPaths {
            layout: self.layout,
            root: self.root.clone(),
            openvmm: self.openvmm(),
            kernel: self.kernel(),
            guests: Guest::ALL
                .iter()
                .map(|&guest| GuestPaths {
                    name: guest.name(),
                    distribution: guest.distribution(),
                    initramfs: self.initramfs(guest),
                    package_manifest: self.package_manifest(guest),
                    distro_layer: self.distro_layer(guest),
                    distro_manifest: self.distro_manifest(guest),
                })
                .collect(),
        }
    }
}

/// Checks that `path` is a regular file, as `require_file` of `nvx.py` does, and returns it.
fn required(path: PathBuf, description: &str) -> Result<PathBuf> {
    require_file(&path, description)?;
    Ok(path)
}

/// The paths of the artifacts of an [`ArtifactDir`], which serialize as a JSON object such as:
///
/// ```json
/// {"layout": "repository", "root": "/nvx", "openvmm": "/nvx/openvmm/target/release/openvmm",
///  "kernel": "/nvx/build/vmlinux", "guests": [{"name": "alpine",
///  "distribution": "Alpine Linux", "initramfs": "/nvx/build/initramfs.cpio.gz",
///  "package_manifest": "/nvx/build/initramfs.cpio.gz.packages.json", "distro_layer": null,
///  "distro_manifest": null}, ...]}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ArtifactPaths {
    /// The directory's layout.
    pub layout: Layout,
    /// The absolute directory.
    pub root: PathBuf,
    /// The OpenVMM executable.
    pub openvmm: PathBuf,
    /// The guest kernel.
    pub kernel: PathBuf,
    /// The artifacts of each guest, in the order of [`Guest::ALL`].
    pub guests: Vec<GuestPaths>,
}

/// The paths of the artifacts of one guest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GuestPaths {
    /// The guest's name, [`Guest::name`].
    pub name: &'static str,
    /// The guest's distribution, [`Guest::distribution`].
    pub distribution: &'static str,
    /// The initramfs.
    pub initramfs: PathBuf,
    /// The package manifest of the initramfs.
    pub package_manifest: PathBuf,
    /// The distro layer, if the guest provides one.
    pub distro_layer: Option<PathBuf>,
    /// The manifest of the distro layer, if the guest provides one.
    pub distro_manifest: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::ErrorKind;

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
            let directory = ArtifactDir::from_dir(root).unwrap();
            assert_eq!(directory.layout(), Layout::Repository);
            assert_eq!(directory.kernel(), artifacts.kernel);
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
        assert_eq!(error.kind(), ErrorKind::Unavailable);
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
        assert_eq!(error.kind(), ErrorKind::Unavailable);
        assert!(error.message().contains(Artifacts::ENV_DIR));
        assert!(
            error
                .message()
                .contains("build the nvx-rs crate with the bundled feature")
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
        assert_eq!(error.kind(), ErrorKind::Unavailable);
        let error = check_manifest(&directory.path().join("missing.json")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable);
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

    #[test]
    fn guests_name_their_artifacts_as_nvx_py_does() {
        assert_eq!(
            Guest::ALL.map(Guest::name),
            ["alpine", "ubuntu", "azurelinux"]
        );
        for guest in Guest::ALL {
            assert_eq!(Guest::from_name(guest.name()).unwrap(), guest);
            assert_eq!(guest.to_string(), guest.name());
        }
        let error = Guest::from_name("Ubuntu").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert_eq!(
            error.message(),
            "unsupported guest 'Ubuntu'; choose alpine, ubuntu, azurelinux"
        );
        assert_eq!(
            Guest::ALL.map(Guest::distribution),
            ["Alpine Linux", "Ubuntu Base", "Azure Linux"]
        );
        assert_eq!(
            Guest::ALL.map(Guest::package_manifest_name),
            [
                "initramfs.cpio.gz.packages.json",
                "initramfs-ubuntu.cpio.gz.packages.json",
                "initramfs-azurelinux.cpio.gz.packages.json",
            ]
        );
        assert_eq!(
            Guest::ALL.map(Guest::distro_layer_name),
            [None, Some("ubuntu-distro.erofs"), None]
        );
        assert_eq!(
            Guest::Ubuntu.distro_manifest_name().as_deref(),
            Some("ubuntu-distro.erofs.manifest.json")
        );
        assert_eq!(Guest::Alpine.distro_manifest_name(), None);
    }

    #[test]
    fn a_checkout_holds_the_guest_artifacts_in_build() {
        let directory = tempfile::tempdir().unwrap();
        checkout(directory.path());
        let artifacts = ArtifactDir::from_dir(directory.path()).unwrap();
        let root = std::path::absolute(directory.path()).unwrap();
        let build = root.join("build");
        assert_eq!(artifacts.layout(), Layout::Repository);
        assert_eq!(artifacts.root(), root);
        assert_eq!(
            artifacts.openvmm(),
            root.join("openvmm")
                .join("target")
                .join("release")
                .join(openvmm_executable())
        );
        assert_eq!(artifacts.kernel(), build.join("vmlinux"));
        assert_eq!(
            Guest::ALL.map(|guest| artifacts.initramfs(guest)),
            [
                build.join("initramfs.cpio.gz"),
                build.join("initramfs-ubuntu.cpio.gz"),
                build.join("initramfs-azurelinux.cpio.gz"),
            ]
        );
        assert_eq!(
            artifacts.package_manifest(Guest::Ubuntu),
            build.join("initramfs-ubuntu.cpio.gz.packages.json")
        );
        assert_eq!(
            Guest::ALL.map(|guest| artifacts.distro_layer(guest)),
            [None, Some(build.join("ubuntu-distro.erofs")), None]
        );
        assert_eq!(
            artifacts.distro_manifest(Guest::Ubuntu),
            Some(build.join("ubuntu-distro.erofs.manifest.json"))
        );
        assert_eq!(
            artifacts.artifact("vmlinux.provenance.json"),
            build.join("vmlinux.provenance.json")
        );

        // A relative directory is made absolute, and an explicit layout overrides detection.
        let relative = ArtifactDir::repository(".").unwrap();
        assert_eq!(relative.root(), std::path::absolute(".").unwrap());
        release(directory.path());
        assert_eq!(
            ArtifactDir::repository(directory.path()).unwrap().layout(),
            Layout::Repository
        );
    }

    #[test]
    fn a_release_holds_the_guest_artifacts_in_guest() {
        let directory = tempfile::tempdir().unwrap();
        release(directory.path());
        let artifacts = ArtifactDir::from_dir(directory.path()).unwrap();
        let root = std::path::absolute(directory.path()).unwrap();
        let guest = root.join("guest");
        assert_eq!(artifacts.layout(), Layout::Release);
        assert_eq!(artifacts, ArtifactDir::release(directory.path()).unwrap());
        assert_eq!(
            artifacts.openvmm(),
            root.join("bin").join(openvmm_executable())
        );
        assert_eq!(artifacts.kernel(), guest.join("vmlinux"));
        assert_eq!(
            Guest::ALL.map(|guest| artifacts.initramfs(guest)),
            [
                guest.join("initramfs.cpio.gz"),
                guest.join("initramfs-ubuntu.cpio.gz"),
                guest.join("initramfs-azurelinux.cpio.gz"),
            ]
        );
        assert_eq!(
            artifacts.distro_layer(Guest::Ubuntu),
            Some(guest.join("ubuntu-distro.erofs"))
        );

        // The release's artifacts are the ones that discovery finds for its directory.
        let discovered = Artifacts::from_dir(directory.path()).unwrap();
        assert_eq!(artifacts.sandbox_artifacts().unwrap(), discovered);
    }

    #[test]
    fn guest_artifacts_are_required_in_the_order_and_words_of_nvx_py() {
        let directory = tempfile::tempdir().unwrap();
        checkout(directory.path());
        let artifacts = ArtifactDir::repository(directory.path()).unwrap();
        fn missing<T: std::fmt::Debug>(result: Result<T>, description: &str, path: PathBuf) {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unavailable);
            assert_eq!(
                error.message(),
                format!("{description} not found: {}", path.display())
            );
        }
        let create = |path: PathBuf| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"artifact").unwrap();
        };
        missing(
            artifacts.sandbox_artifacts(),
            "OpenVMM release binary",
            artifacts.openvmm(),
        );
        create(artifacts.openvmm());
        missing(
            artifacts.sandbox_artifacts(),
            "Linux direct kernel",
            artifacts.kernel(),
        );
        create(artifacts.kernel());
        missing(
            artifacts.sandbox_artifacts(),
            "initramfs",
            artifacts.initramfs(Guest::Alpine),
        );
        create(artifacts.initramfs(Guest::Alpine));
        assert_eq!(
            artifacts.sandbox_artifacts().unwrap(),
            Artifacts::new(
                artifacts.openvmm(),
                artifacts.kernel(),
                artifacts.initramfs(Guest::Alpine)
            )
        );

        assert_eq!(artifacts.require_openvmm().unwrap(), artifacts.openvmm());
        assert_eq!(artifacts.require_kernel().unwrap(), artifacts.kernel());
        missing(
            artifacts.require_initramfs(Guest::Ubuntu),
            "Ubuntu Base initramfs",
            artifacts.initramfs(Guest::Ubuntu),
        );
        missing(
            artifacts.require_initramfs(Guest::AzureLinux),
            "Azure Linux initramfs",
            artifacts.initramfs(Guest::AzureLinux),
        );
        create(artifacts.initramfs(Guest::Ubuntu));
        assert_eq!(
            artifacts.require_initramfs(Guest::Ubuntu).unwrap(),
            artifacts.initramfs(Guest::Ubuntu)
        );

        // A directory is not a regular file.
        fs::create_dir_all(artifacts.initramfs(Guest::AzureLinux)).unwrap();
        missing(
            artifacts.require_initramfs(Guest::AzureLinux),
            "Azure Linux initramfs",
            artifacts.initramfs(Guest::AzureLinux),
        );
    }

    #[test]
    fn artifact_paths_serialize_every_guest_in_order() {
        let directory = tempfile::tempdir().unwrap();
        release(directory.path());
        let artifacts = ArtifactDir::from_dir(directory.path()).unwrap();
        let text = |path: PathBuf| Value::from(path.to_str().unwrap());
        let value = serde_json::to_value(artifacts.paths()).unwrap();
        assert_eq!(value["layout"], "release");
        assert_eq!(value["root"], text(artifacts.root().to_path_buf()));
        assert_eq!(value["openvmm"], text(artifacts.openvmm()));
        assert_eq!(value["kernel"], text(artifacts.kernel()));
        let guests = value["guests"].as_array().unwrap();
        assert_eq!(guests.len(), Guest::ALL.len());
        for (paths, guest) in guests.iter().zip(Guest::ALL) {
            assert_eq!(paths["name"], guest.name());
            assert_eq!(paths["distribution"], guest.distribution());
            assert_eq!(paths["initramfs"], text(artifacts.initramfs(guest)));
            assert_eq!(
                paths["package_manifest"],
                text(artifacts.package_manifest(guest))
            );
            assert_eq!(
                paths["distro_layer"],
                artifacts.distro_layer(guest).map_or(Value::Null, text)
            );
            assert_eq!(
                paths["distro_manifest"],
                artifacts.distro_manifest(guest).map_or(Value::Null, text)
            );
        }
        let repository = ArtifactDir::repository(directory.path()).unwrap();
        assert_eq!(
            serde_json::to_value(repository.paths()).unwrap()["layout"],
            "repository"
        );
    }

    #[cfg(feature = "bundled")]
    #[test]
    fn the_bundle_is_a_compatible_release() {
        let bundled = Artifacts::bundled().unwrap();
        // A build that was asked to stage a release must have staged it.
        let requested = env::var_os("NVX_BUNDLE_DIR").is_some()
            && env::var_os("NVX_BUNDLE").is_none_or(|value| value != "skip");
        if requested && (cfg!(target_os = "linux") || cfg!(windows)) {
            assert!(bundled.is_some(), "NVX_BUNDLE_DIR was not staged");
        }
        if let Some(artifacts) = bundled {
            for path in [&artifacts.openvmm, &artifacts.kernel, &artifacts.initrd] {
                assert!(path.is_file(), "{} is missing", path.display());
            }
        }
    }
}
