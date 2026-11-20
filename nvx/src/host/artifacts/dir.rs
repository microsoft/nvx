//! The directories of the artifacts that `nvx.py` runs, repository checkouts and extracted
//! releases, and the paths of their artifacts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::compatibility::compatibility;
use super::identity::{provenance_identity, read_metadata, sha256_identity};
use super::require::required;
use super::{
    CHECKSUMS_NAME, Guest, INITRAMFS_PROVENANCE_NAME, KERNEL_NAME, KERNEL_PROVENANCE_NAME,
    OPENVMM_PROVENANCE_NAME, RELEASE_BIN_DIR, RELEASE_GUEST_DIR, REPO_BUILD_DIR, REPO_OPENVMM_DIR,
    absolute, is_release_dir, openvmm_executable,
};
use crate::error::{Error, Result};
use crate::host::runtime::{ArtifactIdentity, GuestArchitecture, RuntimeIdentity};

/// The layout of a directory of NVX artifacts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Layout {
    /// A repository checkout: the guest artifacts in `build/`, and OpenVMM in
    /// `openvmm/target/release/`, where a local build writes them and where `nvx download`
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
/// `require_*` methods check them with the messages of `nvx.py`. Nothing here reads the release's
/// `SOURCE-MANIFEST.json`, which `nvx.py` does not check either.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArtifactDir {
    /// The absolute directory.
    root: PathBuf,
    /// Its layout.
    layout: Layout,
    /// Declared guest ISA; the legacy constructor is explicitly x86-64.
    architecture: GuestArchitecture,
}

impl ArtifactDir {
    /// Uses a repository checkout.
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) if a relative
    /// `root` meets an unreadable current directory.
    pub(crate) fn repository(root: impl AsRef<Path>) -> Result<Self> {
        Self::new(root.as_ref(), Layout::Repository)
    }

    /// Uses an extracted release package.
    ///
    /// Fails as [`repository`](Self::repository) does.
    pub(crate) fn release(root: impl AsRef<Path>) -> Result<Self> {
        Self::new(root.as_ref(), Layout::Release)
    }

    /// Uses a directory in the layout that its built artifacts reveal.
    ///
    /// A repository checkout also has a `guest/` directory, which holds the guest sources, so the
    /// layouts are told apart by their built artifacts instead: a directory that contains
    /// `bin/openvmm[.exe]`, `guest/vmlinux`, or `guest/initramfs.cpio.gz` is a release, and any
    /// other directory is a repository checkout.
    ///
    /// Fails as [`repository`](Self::repository) does.
    pub(crate) fn from_dir(directory: impl AsRef<Path>) -> Result<Self> {
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
            architecture: GuestArchitecture::X86_64,
        })
    }

    /// Selects guest products without changing the native OpenVMM location.
    pub(crate) fn with_architecture(mut self, architecture: GuestArchitecture) -> Result<Self> {
        if architecture == GuestArchitecture::Aarch64 && self.layout == Layout::Release {
            return Err(Error::unsupported(
                "AArch64 release artifacts are not qualified; use explicit local ARM products",
            ));
        }
        self.architecture = architecture;
        Ok(self)
    }

    /// Returns the absolute directory.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the directory's layout.
    pub(crate) fn layout(&self) -> Layout {
        self.layout
    }

    /// Returns the path of the guest artifact `name`, `artifact_path(name)` of `nvx.py`:
    /// `build/NAME` in a checkout, and `guest/NAME` in a release.
    pub(crate) fn artifact(&self, name: &str) -> PathBuf {
        let directory = match self.layout {
            Layout::Repository => REPO_BUILD_DIR,
            Layout::Release => RELEASE_GUEST_DIR,
        };
        let directory = self.root.join(directory);
        let directory = if self.layout == Layout::Repository
            && self.architecture == GuestArchitecture::Aarch64
            && name != OPENVMM_PROVENANCE_NAME
        {
            directory.join("aarch64")
        } else {
            directory
        };
        directory.join(name)
    }

    /// Returns the path of the OpenVMM executable, `openvmm_binary_path()` of `nvx.py`:
    /// `openvmm/target/release/openvmm[.exe]` in a checkout, and `bin/openvmm[.exe]` in a
    /// release.
    pub(crate) fn openvmm(&self) -> PathBuf {
        let directory = match self.layout {
            Layout::Repository => REPO_OPENVMM_DIR.iter().collect(),
            Layout::Release => PathBuf::from(RELEASE_BIN_DIR),
        };
        self.root.join(directory).join(openvmm_executable())
    }

    /// Returns the path of the guest kernel, which every guest boots.
    pub(crate) fn kernel(&self) -> PathBuf {
        self.artifact(self.kernel_name())
    }

    /// Selects the kernel filename from the explicit guest ISA, never from the host CPU.
    fn kernel_name(&self) -> &'static str {
        match self.architecture {
            GuestArchitecture::X86_64 => KERNEL_NAME,
            GuestArchitecture::Aarch64 => "Image",
        }
    }

    /// Returns the path of a guest's initramfs.
    pub(crate) fn initramfs(&self, guest: Guest) -> PathBuf {
        self.artifact(guest.initramfs_name())
    }

    /// Returns the path of the package manifest of a guest's initramfs.
    pub(crate) fn package_manifest(&self, guest: Guest) -> PathBuf {
        self.artifact(&guest.package_manifest_name())
    }

    /// Returns the path of a guest's distro layer, if the guest provides one.
    pub(crate) fn distro_layer(&self, guest: Guest) -> Option<PathBuf> {
        guest.distro_layer_name().map(|name| self.artifact(name))
    }

    /// Returns the path of the manifest of a guest's distro layer, if the guest provides one.
    pub(crate) fn distro_manifest(&self, guest: Guest) -> Option<PathBuf> {
        guest
            .distro_manifest_name()
            .map(|name| self.artifact(&name))
    }

    /// Checks that the guest kernel is a regular file and returns its path.
    ///
    /// Fails with [`ErrorCode::ArtifactMissing`] and the message `Linux direct kernel not found:
    /// PATH`.
    ///
    /// [`ErrorCode::ArtifactMissing`]: crate::ErrorCode::ArtifactMissing
    pub(crate) fn require_kernel(&self) -> Result<PathBuf> {
        let kernel = required(self.kernel(), "Linux direct kernel")?;
        if self.architecture == GuestArchitecture::Aarch64 {
            super::identity::check_arm64_image(&kernel)?;
        }
        Ok(kernel)
    }

    /// Checks that a guest's initramfs is a regular file and returns its path, as `nvx run`
    /// does before it boots the guest.
    ///
    /// Fails with [`ErrorCode::ArtifactMissing`] and the message `DISTRIBUTION initramfs not
    /// found: PATH`, such as `Ubuntu Base initramfs not found: PATH`.
    ///
    /// [`ErrorCode::ArtifactMissing`]: crate::ErrorCode::ArtifactMissing
    pub(crate) fn require_initramfs(&self, guest: Guest) -> Result<PathBuf> {
        if self.architecture == GuestArchitecture::Aarch64 && guest == Guest::AzureLinux {
            return Err(Error::unsupported(
                "AArch64 cold boot supports Alpine or Ubuntu, not Azure Linux",
            ));
        }
        let initramfs = required(
            self.initramfs(guest),
            &format!("{} initramfs", guest.distribution()),
        )?;
        if self.architecture == GuestArchitecture::Aarch64 {
            let path = self.package_manifest(guest);
            let metadata = read_metadata(
                &path,
                "ARM guest package manifest",
                "build the matching architecture-qualified initramfs with nvx.py",
            )?;
            let value: serde_json::Value = serde_json::from_str(&metadata).map_err(|error| {
                Error::unavailable(format!("invalid ARM package manifest {}", path.display()))
                    .with_source(error)
            })?;
            if value
                .get("architecture")
                .and_then(serde_json::Value::as_str)
                != Some("aarch64")
            {
                return Err(Error::unsupported(format!(
                    "guest package manifest {} does not declare architecture aarch64",
                    path.display()
                )));
            }
        }
        Ok(initramfs)
    }

    /// Returns the paths of every artifact, for a caller to show or record.
    pub(crate) fn paths(&self) -> ArtifactPaths {
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

    /// Returns the identity of the sandbox runtime of this directory: its OpenVMM executable, its
    /// kernel, and its Alpine initramfs, as the directory's install metadata names them, and the
    /// [compatibility](super::compatibility::compatibility) of the CLI's contracts with its
    /// bundled guests.
    ///
    /// The metadata names each artifact by the SHA-256 digest that its build or its package
    /// recorded, so nothing here reads or hashes an artifact:
    ///
    /// - A release lists the digest of each of its files in its `SHA256SUMS`, under the file's
    ///   path in the release: `bin/openvmm[.exe]`, `guest/vmlinux`, and
    ///   `guest/initramfs.cpio.gz`.
    /// - A repository checkout keeps the build provenance of each artifact beside its guest
    ///   artifacts: `build/openvmm.provenance.json`, `build/vmlinux.provenance.json`, and
    ///   `build/initramfs.provenance.json`, of format 1, whose `executable_sha256`,
    ///   `kernel_sha256`, and `initramfs_sha256` are the digests. `nvx.py` records them as it
    ///   builds an artifact, and `nvx download` installs those of a release.
    ///
    /// The artifacts of a release therefore have the same identity in the release and in a
    /// checkout that installed it. Fails with
    /// [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) when the metadata is missing or
    /// unreadable, or names no valid digest of an artifact. A file of metadata that is not a
    /// regular file, such as a FIFO, is refused without being waited on.
    pub(crate) fn runtime_identity(&self) -> Result<RuntimeIdentity> {
        let [openvmm, kernel, initramfs] = match self.layout {
            Layout::Release => self.listed_identities()?,
            Layout::Repository => [
                provenance_identity(
                    &self.artifact(OPENVMM_PROVENANCE_NAME),
                    "OpenVMM",
                    "executable_sha256",
                )?,
                provenance_identity(
                    &self.artifact(if self.architecture == GuestArchitecture::Aarch64 {
                        "Image.provenance.json"
                    } else {
                        KERNEL_PROVENANCE_NAME
                    }),
                    "kernel",
                    "kernel_sha256",
                )?,
                provenance_identity(
                    &self.artifact(INITRAMFS_PROVENANCE_NAME),
                    "initramfs",
                    "initramfs_sha256",
                )?,
            ],
        };
        Ok(RuntimeIdentity::new(
            openvmm,
            kernel,
            initramfs,
            compatibility()?,
        ))
    }

    /// Returns the identities of the OpenVMM executable, kernel, and Alpine initramfs of a
    /// release, as its `SHA256SUMS` lists their digests: one line for each file, its 64
    /// lowercase hexadecimal digits, two spaces, and its path in the release.
    fn listed_identities(&self) -> Result<[ArtifactIdentity; 3]> {
        let path = self.root.join(CHECKSUMS_NAME);
        let description = "release checksum list";
        let text = read_metadata(&path, description, "the release is incomplete")?;
        let mut identities = HashMap::new();
        for line in text.lines() {
            let malformed = || {
                Error::unavailable(format!(
                    "the {description} {} has a malformed line: {line:?}",
                    path.display()
                ))
            };
            let (digest, name) = line.split_once("  ").ok_or_else(malformed)?;
            let identity = sha256_identity(digest)
                .filter(|_| !name.is_empty())
                .ok_or_else(malformed)?;
            if identities.insert(name, identity).is_some() {
                return Err(Error::unavailable(format!(
                    "the {description} {} lists {name} twice",
                    path.display()
                )));
            }
        }
        let mut listed = |name: String| {
            identities.remove(name.as_str()).ok_or_else(|| {
                Error::unavailable(format!(
                    "the {description} {} does not list {name}",
                    path.display()
                ))
            })
        };
        Ok([
            listed(format!("{RELEASE_BIN_DIR}/{}", openvmm_executable()))?,
            listed(format!("{RELEASE_GUEST_DIR}/{KERNEL_NAME}"))?,
            listed(format!(
                "{RELEASE_GUEST_DIR}/{}",
                Guest::Alpine.initramfs_name()
            ))?,
        ])
    }
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
pub(crate) struct ArtifactPaths {
    /// The directory's layout.
    pub(crate) layout: Layout,
    /// The absolute directory.
    pub(crate) root: PathBuf,
    /// The OpenVMM executable.
    pub(crate) openvmm: PathBuf,
    /// The guest kernel.
    pub(crate) kernel: PathBuf,
    /// The artifacts of each guest, in the order of [`Guest::ALL`].
    pub(crate) guests: Vec<GuestPaths>,
}

/// The paths of the artifacts of one guest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub(crate) struct GuestPaths {
    /// The guest's name, [`Guest::name`].
    pub(crate) name: &'static str,
    /// The guest's distribution, [`Guest::distribution`].
    pub(crate) distribution: &'static str,
    /// The initramfs.
    pub(crate) initramfs: PathBuf,
    /// The package manifest of the initramfs.
    pub(crate) package_manifest: PathBuf,
    /// The distro layer, if the guest provides one.
    pub(crate) distro_layer: Option<PathBuf>,
    /// The manifest of the distro layer, if the guest provides one.
    pub(crate) distro_manifest: Option<PathBuf>,
}
