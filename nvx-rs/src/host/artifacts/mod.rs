//! Locating the OpenVMM executable and the NVX guest artifacts.
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

#[cfg(feature = "cli")]
mod dir;
#[cfg(feature = "cli")]
mod guest;
#[cfg(feature = "cli")]
mod identity;
mod require;

#[cfg(feature = "cli")]
use std::path::{Path, PathBuf};

#[cfg(feature = "cli")]
use crate::error::{Error, Result};

#[cfg(feature = "cli")]
pub(crate) use self::dir::{ArtifactDir, ArtifactPaths, Layout};
#[cfg(feature = "cli")]
pub(crate) use self::guest::Guest;
#[cfg(feature = "cli")]
pub(crate) use self::require::require_artifact;
pub(crate) use self::require::require_existing;

/// Name of the release manifest at the root of a release archive or repository checkout.
#[cfg(feature = "cli")]
pub(crate) const MANIFEST_NAME: &str = "SOURCE-MANIFEST.json";

/// Returns whether `directory` is in the release layout: whether it contains `bin/openvmm[.exe]`,
/// `guest/vmlinux`, or `guest/initramfs.cpio.gz`.
#[cfg(feature = "cli")]
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
#[cfg(feature = "cli")]
pub(crate) fn openvmm_executable() -> &'static str {
    if cfg!(windows) {
        "openvmm.exe"
    } else {
        "openvmm"
    }
}

/// Resolves a relative path against the current directory, failing with
/// [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable).
#[cfg(feature = "cli")]
pub(crate) fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|error| {
        Error::unavailable(format!("cannot resolve {}", path.display())).with_source(error)
    })
}

/// File name of the guest kernel in both layouts.
#[cfg(feature = "cli")]
pub(crate) const KERNEL_NAME: &str = "vmlinux";
/// Suffix of the package manifest that lists the packages of an initramfs.
#[cfg(feature = "cli")]
pub(crate) const PACKAGE_MANIFEST_SUFFIX: &str = ".packages.json";
/// Suffix of the manifest of a distro layer, which binds the layer's digest and packages.
#[cfg(feature = "cli")]
pub(crate) const DISTRO_MANIFEST_SUFFIX: &str = ".manifest.json";
/// Repository-layout directory of guest artifacts.
#[cfg(feature = "cli")]
pub(crate) const REPO_BUILD_DIR: &str = "build";
/// Repository-layout path of the OpenVMM build output directory.
#[cfg(feature = "cli")]
const REPO_OPENVMM_DIR: [&str; 3] = ["openvmm", "target", "release"];
/// Release-layout directory of the OpenVMM executable.
#[cfg(feature = "cli")]
const RELEASE_BIN_DIR: &str = "bin";
/// Release-layout directory of the guest artifacts.
#[cfg(feature = "cli")]
const RELEASE_GUEST_DIR: &str = "guest";
/// Name of the list of the SHA-256 digests of the files of a release, at its root.
#[cfg(feature = "cli")]
pub(crate) const CHECKSUMS_NAME: &str = "SHA256SUMS";
/// Name of the build provenance of the OpenVMM executable, beside the guest artifacts of a
/// repository checkout.
#[cfg(feature = "cli")]
pub(crate) const OPENVMM_PROVENANCE_NAME: &str = "openvmm.provenance.json";
/// Name of the build provenance of the guest kernel, beside it.
#[cfg(feature = "cli")]
pub(crate) const KERNEL_PROVENANCE_NAME: &str = "vmlinux.provenance.json";
/// Name of the build provenance of the Alpine initramfs, beside it.
#[cfg(feature = "cli")]
pub(crate) const INITRAMFS_PROVENANCE_NAME: &str = "initramfs.provenance.json";
/// Format of the build provenance that this crate reads.
#[cfg(feature = "cli")]
const PROVENANCE_FORMAT: u64 = 1;
/// Longest file of install metadata that this crate reads, in bytes. A release's checksum list
/// holds one line for each of its files, and a build provenance a few fields.
#[cfg(feature = "cli")]
const MAX_METADATA_LEN: u64 = 1 << 20;

#[cfg(all(test, feature = "cli"))]
mod tests;
