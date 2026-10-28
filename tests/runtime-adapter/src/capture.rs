//! The host program's captures of templates: VMs of the guest agent's [`template`](crate::template)
//! mode, which NVX's template store captures through NVX's supervisor, and publishes as its
//! generations, with `nvx::host::template::Store::create_template`, as `runtime-adapter-host
//! create` asks.
//!
//! A [`Request`] holds what the host program's options choose of a template: its placeholder, the
//! backend, the CPU profile, the processors and those online at boot, the RAM and its capacity,
//! the length of the dormant scratch, and the network, if any. [`Request::capture`] returns NVX's
//! `Capture` of it: the adapter's plan of the placeholder, which the host program plans as it
//! plans an image, with the network, the placeholder's binding, which the host program stages as
//! it stages an image, NVX's artifacts with the initramfs of the adapter's VMs, and the class of
//! the capture, from which NVX's store takes the slot that serializes the captures of the class,
//! and checks the template's compatibility key:
//!
//! - the runtime identity of the adapter's VMs, which [`crate::runtime`] gives, and the guest
//!   agent's build, as `runtime-adapter-guest/BUILD`;
//! - the backend and the CPU profile, as OpenVMM's `--hypervisor` and `--cpu-profile` take them;
//! - the processors, the template's processor capacity, and those online at boot, the fewest that
//!   its clones take, from which a restore brings each clone's processor target online, and the
//!   RAM and its capacity;
//! - the geometries of the placeholder and of the dormant scratch, as OpenVMM presents a sandbox
//!   block device, which [`geometry`] gives;
//! - the network, of which the class takes the NIC alone, at the guest's address, under the
//!   complete deny-all policy, whatever the network's own, which each clone's restore replaces
//!   with the clone's, and no other invariant setting.
//!
//! The capture's VM boots NVX's kernel with the initramfs of the adapter's VMs, which puts the
//! guest agent in place of NVX's agents; see [`crate::initramfs`]. NVX gives it the kernel command
//! line `nvx_sandbox=1`, with which NVX's `/init` hands it to the guest agent's PID 1 of a layered
//! root, and for a template whose processors are not all online at boot, `maxcpus=` with those
//! that are, and OpenVMM adds the platform tier's tokens, `nvx_snapshot_tier=platform` among them:
//! the worker then captures the VM before it assembles any root, and the capture ends the VM.

use std::path::{Path, PathBuf};

use nvx::host::artifacts::Artifacts;
use nvx::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding};
use nvx::host::sandbox::{SandboxNetwork, SandboxPlan};
use nvx::host::store::ArtifactIdentity;
use nvx::host::template::{BlockGeometry, Capture, CaptureClass, Invariants};
use nvx::protocol::root::ImageFormat;
use nvx::uuid::Uuid;
use nvx::{Hypervisor, Result};

use crate::layered::plan;
use crate::runtime::Runtime;

/// The CPU profile of a template, unless the host program names another: `auto`, which OpenVMM's
/// `--cpu-profile` takes for the built-in profile of the host's CPU, and which OpenVMM also
/// selects without the option.
pub const DEFAULT_CPU_PROFILE: &str = "auto";

/// The processors of a template, its processor capacity, unless the host program names another
/// number.
pub const DEFAULT_PROCESSORS: u32 = 1;

/// The processors of a template that are online at boot, unless the host program names another
/// number: 1, with which the template serves clones of every processor count up to its capacity.
pub const DEFAULT_BOOT_PROCESSORS: u32 = 1;

/// The length of a template's dormant scratch, in bytes, unless the host program names another:
/// 64 MiB, the length of the scratch template that NVX's CI makes with the guest artifacts.
pub const DEFAULT_SCRATCH_LENGTH: u64 = 64 << 20;

/// The size of a logical block of a sandbox block device, as OpenVMM presents it, in bytes.
pub const LOGICAL_BLOCK_SIZE: u32 = 512;

/// The size of a physical block of a sandbox block device, as OpenVMM presents it on Windows, in
/// bytes.
#[cfg(windows)]
const WINDOWS_PHYSICAL_BLOCK_SIZE: u32 = 4096;

/// What the host program asks of a template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The placeholder: the image whose length is the capacity of every clone's image, a trusted
    /// artifact of the platform, which the host program stages before the capture.
    pub distro: PathBuf,
    /// The placeholder's format.
    pub format: ImageFormat,
    /// The identity that the guest would expect of the placeholder, which a template names nowhere
    /// that a clone restores.
    pub identity: Uuid,
    /// The backend.
    pub hypervisor: Hypervisor,
    /// The CPU profile, as OpenVMM's `--cpu-profile` takes it: `auto`, `host`, or a profile ID.
    pub cpu_profile: String,
    /// The processors: the processor capacity, the most that a clone of the template takes.
    pub processors: u32,
    /// The processors online at boot, at most [`processors`](Self::processors): the fewest that a
    /// clone of the template takes.
    pub boot_processors: u32,
    /// The RAM at boot, in bytes.
    pub memory: u64,
    /// The RAM capacity, in bytes, to which a restore may expand the RAM.
    pub memory_capacity: u64,
    /// The length of the dormant scratch, in bytes.
    pub scratch_length: u64,
    /// The network at whose guest's address the template has its fixed NIC, under the complete
    /// deny-all policy, or `None` for a template without a NIC.
    pub network: Option<SandboxNetwork>,
}

impl Request {
    /// Returns the adapter's plan of the placeholder, over RAM, with the network: the plan that
    /// the host program makes of an image that it stages, as `staged::Staged::stage` makes it,
    /// with the placeholder's path absolute and normalized. Its upper is no part of the template,
    /// whose capture attaches its dormant scratch in the upper's place.
    ///
    /// Fails as `staged::Staged::stage` plans an image: with
    /// [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) for a path that is not Unicode, and
    /// otherwise as `SandboxPlan::validated` does.
    pub fn plan(&self) -> Result<SandboxPlan> {
        let plan = plan(&self.distro, self.format, self.identity, None)?;
        Ok(plan.with_network(self.network.clone()))
    }

    /// Returns NVX's capture of the template that this requests, with the placeholder's binding
    /// `binding`, the runtime of the adapter's VMs `runtime`, with the guest agent's build
    /// `guest_build`, and the initramfs `initramfs`, which puts that guest agent in place of NVX's
    /// agents, and whose dormant scratch is on the file system of the directory `scratch`, the
    /// template store's, as the [module documentation](self) describes.
    ///
    /// Fails as [`plan`](Self::plan) and [`geometry`] do, and with
    /// [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) if the guest agent's build is not
    /// part of an artifact identity. It checks nothing else of the class, which
    /// `CaptureClass::check` checks.
    pub fn capture(
        &self,
        binding: DistroBinding,
        runtime: &Runtime,
        guest_build: &str,
        initramfs: &Path,
        scratch: &Path,
    ) -> Result<Capture> {
        let adapter = self.plan()?;
        let class = CaptureClass {
            runtime: runtime.identity.clone(),
            guest_build: ArtifactIdentity::new(format!("runtime-adapter-guest/{guest_build}"))?,
            hypervisor: self.hypervisor,
            cpu_profile: self.cpu_profile.clone(),
            processors: self.processors,
            boot_processors: self.boot_processors,
            memory: self.memory,
            memory_capacity: self.memory_capacity,
            distro: geometry(adapter.distro().path(), binding.image().length())?,
            scratch: geometry(scratch, self.scratch_length)?,
            network: self.network.clone(),
            invariants: Invariants::new(),
        };
        Ok(Capture {
            class,
            adapter,
            binding,
            policy_revision: DISTRO_POLICY_REVISION,
            artifacts: Artifacts::new(
                &runtime.artifacts.openvmm,
                &runtime.artifacts.kernel,
                initramfs,
            ),
        })
    }
}

/// Returns the geometry that OpenVMM presents for a sandbox block device of `length` bytes, whose
/// file is `path`, or is in the directory `path`: logical blocks of [`LOGICAL_BLOCK_SIZE`] bytes,
/// and physical blocks of the size that the file system prefers for I/O, `st_blksize`, on Linux,
/// of 4096 bytes on Windows, and of a logical block elsewhere.
///
/// A file in a directory is on the directory's file system, whose preferred size the directory
/// reports too, unless the file is a mount point.
///
/// Fails with [`ErrorKind::Io`](nvx::ErrorKind::Io) if the file's metadata cannot be read on
/// Linux, and with [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) if its size does not
/// fit 32 bits.
pub fn geometry(path: &Path, length: u64) -> Result<BlockGeometry> {
    Ok(BlockGeometry {
        length,
        logical_block_size: LOGICAL_BLOCK_SIZE,
        physical_block_size: physical_block_size(path)?,
    })
}

/// Returns the size of a physical block that OpenVMM presents for a sandbox block device whose
/// file is, or is in, `path`: the file system's preferred size for I/O.
#[cfg(target_os = "linux")]
fn physical_block_size(path: &Path) -> Result<u32> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::metadata(path).map_err(|error| {
        nvx::Error::io(format!("cannot read the metadata of {}", path.display())).with_source(error)
    })?;
    u32::try_from(metadata.blksize()).map_err(|_| {
        nvx::Error::unsupported(format!(
            "the block size {} of {} does not fit 32 bits",
            metadata.blksize(),
            path.display()
        ))
    })
}

/// Returns the size of a physical block that OpenVMM presents for a sandbox block device on
/// Windows.
#[cfg(windows)]
fn physical_block_size(_: &Path) -> Result<u32> {
    Ok(WINDOWS_PHYSICAL_BLOCK_SIZE)
}

/// Returns the size of a physical block that OpenVMM presents for a sandbox block device on hosts
/// other than Linux and Windows: a logical block.
#[cfg(not(any(target_os = "linux", windows)))]
fn physical_block_size(_: &Path) -> Result<u32> {
    Ok(LOGICAL_BLOCK_SIZE)
}
