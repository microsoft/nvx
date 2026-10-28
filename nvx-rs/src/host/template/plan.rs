//! The plan of a template's capture, [`TemplatePlan`]: the VM that a template's capture boots,
//! lowered from an adapter's plan, and the options with which OpenVMM captures it.

use std::path::{Path, PathBuf};

use super::manifest::PROCESSOR_COUNTS;
use super::store::{SNAPSHOT_NAME, StorageGeneration};
use crate::error::{Error, Result};
use crate::host::paths;
#[cfg(doc)]
use crate::host::sandbox::SandboxNetwork;
use crate::host::sandbox::distro::DistroBinding;
use crate::host::sandbox::plan::bound_image;
use crate::host::sandbox::{SandboxPlan, SandboxUpper, reject_container_path, reject_disk_path};

/// Bytes in a mebibyte, the unit in which the plan writes OpenVMM's `--memory` and
/// `--memory-capacity`.
const MIB: u64 = 1024 * 1024;

/// The size of the blocks of a microVM's RAM, in bytes: 128 MiB, as OpenVMM's microVM profile
/// fixes it. A template's RAM and its RAM capacity are whole blocks, and a restore expands the RAM
/// by whole blocks.
pub const MEMORY_BLOCK_SIZE: u64 = 128 * MIB;

/// The name of the dormant scratch device's file in a capture's transaction directory: a sparse
/// file of the template's scratch length, which the template does not keep.
pub const SCRATCH_NAME: &str = "scratch.raw";

/// The kernel command line of a template's VM, with which the guest's init hands the VM to its
/// sandbox control.
const COMMAND_LINE: &str = "nvx_sandbox=1";

/// The kernel command-line parameter that limits the processors that the guest brings online at
/// boot, and with which OpenVMM records them as the template's processors online at boot.
const BOOT_PROCESSORS: &str = "maxcpus";

/// The snapshot tier of a template's capture: OpenVMM's clone point of the invariant
/// configuration alone.
const TIER: &str = "platform";

/// The block identity of a template's capture: the bound-block storage generation, which OpenVMM
/// takes in place of a digest of each block's content.
const BLOCK_IDENTITY: &str = "generation";

/// The logical block size that OpenVMM presents for a sandbox block device, of which the device's
/// length is a multiple.
const LOGICAL_BLOCK_SIZE: u64 = 512;

/// The resources of a template's VM: its processors, of which a prefix is online at boot, and its
/// RAM, with the capacity to which a restore may expand it.
///
/// A template's VM has the processors of the largest of its clones, its processor capacity, of
/// which the first, those online at boot, are online when it is captured. A restore brings a
/// prefix of the others online, so that one template serves clones of each processor count from
/// those online at boot to its capacity. A template whose processors are all online at boot
/// serves clones of its capacity alone. [`TemplatePlan::new`] checks the resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TemplateResources {
    /// The processor capacity: 1, 2, 4, or 8.
    pub processors: u32,
    /// The processors online at boot: 1, 2, 4, or 8, at most [`processors`](Self::processors).
    /// Fewer than the capacity limit those that the guest brings online at boot, as the
    /// [kernel command line](TemplatePlan::kernel_command_line) says.
    pub boot_processors: u32,
    /// The RAM at boot, in bytes: a nonzero multiple of [`MEMORY_BLOCK_SIZE`].
    pub memory: u64,
    /// The RAM capacity, in bytes, to which a restore may expand the RAM: a multiple of
    /// [`MEMORY_BLOCK_SIZE`] of at least [`memory`](Self::memory).
    pub memory_capacity: u64,
}

/// The plan of a template's capture: the VM that the capture boots, lowered from an adapter's
/// plan, and the options with which OpenVMM captures it, which
/// [`openvmm_arguments`](Self::openvmm_arguments) and
/// [`kernel_command_line`](Self::kernel_command_line) render.
///
/// A template's VM boots its invariant configuration alone, for clones, each of which binds its
/// own image, upper, and export when it is restored. [`new`](Self::new) lowers an adapter's plan,
/// which [`SandboxPlan::adapter`] makes, into that VM:
///
/// - The plan's `distro` is the placeholder: a trusted platform artifact, which its
///   [`DistroBinding`] binds, and whose length is the capacity of every clone's image.
/// - A dormant scratch device takes the place of the plan's upper, which is a clone's: a sparse
///   file of the template's scratch length, [`SCRATCH_NAME`] in the capture's transaction
///   directory, which sets the device's length and block sizes, and which the template does not
///   keep.
/// - The filesystem slot, which every cold boot exposes, stays dormant: the plan exports nothing.
/// - The plan's network gives the template its fixed NIC, at the network's guest address, under
///   the complete deny-all policy, whatever the network's own, as
///   [`SandboxNetwork::template_arguments`] lowers it: egress denied without rules, ingress and
///   host-loopback access denied, without a proxy or forwards. It is the one policy that a
///   restore may replace, so each clone's restore installs the clone's own policy on the NIC. A
///   plan without a network, or with one that lets nothing through, which attaches no NIC, gives
///   the template none.
/// - Its [`TemplateResources`]: its processor capacity, of which those online at boot are online
///   when it is captured, and its RAM, with the capacity to which a restore may expand it,
///   OpenVMM's `--memory-capacity`.
///
/// # The kernel command line
///
/// OpenVMM keeps the effective kernel command line of a template, which its
/// [key](super::TemplateKey) holds and every clone restores, and refuses a capture whose command
/// line holds a token outside the allowlist of its platform tier. The template's own command line
/// is `nvx_sandbox=1`, with which the guest's init hands the VM to its sandbox control, and for a
/// template that boots with fewer processors online than its capacity, `maxcpus=` with the
/// processors online at boot, which limits those that the guest brings online at boot, and which
/// OpenVMM records as the template's, so that each restore may bring a prefix of the others
/// online. OpenVMM writes the other tokens itself, each on the allowlist: the early console,
/// console, reboot, and panic settings, `nr_cpus=` with the processor capacity,
/// `nvx_snapshot_tier=platform`, the virtio devices and the control tty that the guest discovers,
/// and the NIC's addresses. The arguments name no workload identity, lifecycle, share, or
/// published listener, for which OpenVMM would write other tokens, and under the NIC's deny-all
/// policy, OpenVMM names no DNS server either. The command line therefore holds no image
/// identity, hostname, share, proxy, DNS server, entrypoint, or other tenant data, and every
/// clone of the template has the same.
///
/// # The capture
///
/// The capture's options are `--snapshot-tier platform`; `--snapshot-destination`, the
/// destination [`SNAPSHOT_NAME`] in the capture's transaction directory, by which a generation of
/// a [`Store`](super::Store) names its snapshot too; and `--snapshot-block-identity generation`,
/// with the capture's [`StorageGeneration`] as `--snapshot-generation-id`, so that the capture
/// hashes no block device. The template keeps no storage generation: OpenVMM unbinds its
/// `distro`, and its scratch is fresh.
///
/// The transaction directory is the capture's own, and the destination must not exist when the
/// capture launches. OpenVMM backs the VM's RAM with a file that it creates beside the
/// destination, and publishes the snapshot at the destination, so every file of the capture is in
/// the transaction directory.
///
/// The caller creates the transaction directory and the sparse scratch file, checks the
/// placeholder against its binding, as [`DistroBinding::check`] does, and adds its own arguments,
/// such as the hypervisor, the kernel and the initramfs, the CPU profile, and the consoles. It
/// passes [`kernel_command_line`](Self::kernel_command_line) as OpenVMM's `--cmdline`, alone, and
/// names no workload identity, lifecycle, share, or RAM backing file of its own:
///
/// ```no_run
/// use std::path::Path;
///
/// use nvx::host::sandbox::distro::DistroBinding;
/// use nvx::host::sandbox::SandboxPlan;
/// use nvx::host::template::{MEMORY_BLOCK_SIZE, StorageGeneration, TemplatePlan, TemplateResources};
///
/// # fn capture(
/// #     adapter: &SandboxPlan,
/// #     binding: &DistroBinding,
/// #     transaction: &Path,
/// #     generation: StorageGeneration,
/// # ) -> nvx::Result<()> {
/// let resources = TemplateResources {
///     processors: 4,
///     boot_processors: 1,
///     memory: 4 * MEMORY_BLOCK_SIZE,
///     memory_capacity: 16 * MEMORY_BLOCK_SIZE,
/// };
/// let template =
///     TemplatePlan::new(adapter, binding, 64 << 20, resources, transaction, generation)?;
/// let mut arguments = template.openvmm_arguments()?;
/// arguments.extend(["--cmdline".to_owned(), template.kernel_command_line().to_owned()]);
/// // ... the caller's own arguments: the hypervisor, the kernel and the initramfs, the CPU
/// // profile, and the consoles ...
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplatePlan {
    /// The capture's devices but its NIC: an adapter's plan of the placeholder over the dormant
    /// scratch device, without a network.
    plan: SandboxPlan,
    /// OpenVMM's arguments of the template's NIC, under the complete deny-all policy, or none for
    /// a template without a NIC.
    nic: Vec<String>,
    /// The binding of the placeholder.
    binding: DistroBinding,
    /// The length of the dormant scratch device, in bytes.
    scratch_length: u64,
    /// The processors and RAM.
    resources: TemplateResources,
    /// The kernel command line of the capture's VM.
    command_line: String,
    /// The capture's transaction directory, spelled with its components alone.
    transaction: PathBuf,
    /// The snapshot's destination in the transaction directory.
    destination: PathBuf,
    /// The bound-block storage generation of the capture.
    generation: StorageGeneration,
}

impl TemplatePlan {
    /// Lowers `adapter`, an adapter's plan, which [`SandboxPlan::adapter`] makes, into the plan of
    /// a template's capture: of its placeholder, which `binding` binds, over a dormant scratch
    /// device of `scratch_length` bytes, with the NIC of its network, the processors and RAM of
    /// `resources`, the capture's transaction directory `transaction`, spelled with its components
    /// alone, as [`Path::components`] reads them, and its bound-block storage generation
    /// `generation`, which is apart from the other [namespaces](super::Store#four-namespaces) of a
    /// template's generations.
    ///
    /// The plan's upper is no part of the template, which attaches its dormant scratch device in
    /// the upper's place, whatever the upper.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), checking in this
    /// order: a plan of the sandbox profile, with workload settings; a plan whose image is not the
    /// format and identity that `binding` names, as for a binding whose identity is not a UUID in
    /// its canonical text; a placeholder whose path is relative, which OpenVMM would resolve
    /// against the directory in which it runs, holds a comma or a semicolon, which OpenVMM's block
    /// device options would split, or whose name ends in `.vhd`, `.vhdx`, `.vmgs`, or `.iso`,
    /// which OpenVMM would not attach as a raw image; a placeholder whose binding records
    /// a length, or a scratch length, that is not a nonzero multiple of the 512 bytes of the
    /// logical blocks that OpenVMM presents;
    /// processors other than 1, 2, 4, or 8, or processors online at boot other than 1, 2, 4, or
    /// 8, or more than the processors; RAM or a RAM capacity that is not a nonzero multiple
    /// of [`MEMORY_BLOCK_SIZE`], or a capacity below the RAM; and a transaction directory that is
    /// not valid Unicode, that is relative, which OpenVMM would resolve against the directory in
    /// which it runs, or whose path holds a comma or a semicolon, which OpenVMM's block device
    /// options would split.
    ///
    /// It reads, opens, and creates nothing.
    pub fn new(
        adapter: &SandboxPlan,
        binding: &DistroBinding,
        scratch_length: u64,
        resources: TemplateResources,
        transaction: &Path,
        generation: StorageGeneration,
    ) -> Result<Self> {
        if adapter.settings().is_some() {
            return Err(Error::unsupported(
                "a template lowers an adapter's plan, not a plan of the sandbox profile, whose \
                 workload settings no template captures",
            ));
        }
        let distro = adapter.distro();
        if bound_image(binding)? != distro.image() {
            return Err(Error::unsupported(format!(
                "the placeholder {} is not the {} layer with identity {} that its distro binding \
                 names",
                distro.path().display(),
                binding.format(),
                binding.identity()
            )));
        }
        // OpenVMM attaches the placeholder at the path that its arguments name, which they must
        // hold whole, as a raw image, and which OpenVMM would resolve against the directory in
        // which it runs if it were relative.
        let placeholder = distro.path();
        if !placeholder.is_absolute() {
            return Err(Error::unsupported(format!(
                "the placeholder of a template's capture must be absolute, since OpenVMM would \
                 resolve it against the directory in which it runs: {}",
                placeholder.display()
            )));
        }
        let shown = placeholder.display().to_string();
        reject_disk_path(placeholder, &shown)?;
        reject_container_path(placeholder, &shown)?;
        let capacity = binding.image().length();
        if !is_block_length(capacity) {
            return Err(Error::unsupported(format!(
                "the placeholder {} of {capacity} bytes cannot be attached: OpenVMM attaches block \
                 devices of nonzero multiples of {LOGICAL_BLOCK_SIZE} bytes",
                distro.path().display()
            )));
        }
        if !is_block_length(scratch_length) {
            return Err(Error::unsupported(format!(
                "a template's scratch of {scratch_length} bytes cannot be attached: OpenVMM \
                 attaches block devices of nonzero multiples of {LOGICAL_BLOCK_SIZE} bytes"
            )));
        }
        check_resources(&resources)?;
        let nic = match adapter.network() {
            Some(network) => network.template_arguments()?,
            None => Vec::new(),
        };
        let transaction = transaction_directory(transaction)?;
        let scratch = transaction.join(SCRATCH_NAME);
        reject_disk_path(&scratch, &scratch.display().to_string())?;
        let destination = transaction.join(SNAPSHOT_NAME);
        let plan = SandboxPlan::adapter(distro.clone(), SandboxUpper::scratch_at(scratch));
        // A template whose processors are all online at boot needs no limit, and records none,
        // as OpenVMM records none without the parameter.
        let command_line = if resources.boot_processors < resources.processors {
            format!(
                "{COMMAND_LINE} {BOOT_PROCESSORS}={}",
                resources.boot_processors
            )
        } else {
            COMMAND_LINE.to_owned()
        };
        Ok(Self {
            plan,
            nic,
            binding: binding.clone(),
            scratch_length,
            resources,
            command_line,
            transaction,
            destination,
            generation,
        })
    }

    /// Returns the capture's devices but its NIC: an adapter's plan of the placeholder over the
    /// dormant scratch device, without a network, whose NIC [`nic_arguments`](Self::nic_arguments)
    /// attaches.
    pub fn plan(&self) -> &SandboxPlan {
        &self.plan
    }

    /// Returns OpenVMM's arguments of the template's NIC, at the guest address of the adapter's
    /// network, under the complete deny-all policy, as [`SandboxNetwork::template_arguments`]
    /// lowers them, or none for a template without a NIC.
    pub fn nic_arguments(&self) -> &[String] {
        &self.nic
    }

    /// Returns the binding of the placeholder.
    pub fn binding(&self) -> &DistroBinding {
        &self.binding
    }

    /// Returns the length of the placeholder, in bytes, as its binding records it: the capacity
    /// of every clone's image.
    pub fn image_capacity(&self) -> u64 {
        self.binding.image().length()
    }

    /// Returns the dormant scratch device's file, [`SCRATCH_NAME`] in the transaction directory.
    pub fn scratch_path(&self) -> &Path {
        self.plan
            .upper()
            .scratch_path()
            .expect("a template's upper is its dormant scratch device")
    }

    /// Returns the length of the dormant scratch device, in bytes, which its sparse file sets.
    pub fn scratch_length(&self) -> u64 {
        self.scratch_length
    }

    /// Returns the processors and RAM.
    pub fn resources(&self) -> TemplateResources {
        self.resources
    }

    /// Returns the capture's transaction directory.
    pub fn transaction(&self) -> &Path {
        self.transaction.as_path()
    }

    /// Returns the snapshot's destination, [`SNAPSHOT_NAME`] in the transaction directory.
    pub fn destination(&self) -> &Path {
        self.destination.as_path()
    }

    /// Returns the bound-block storage generation of the capture.
    pub fn generation(&self) -> StorageGeneration {
        self.generation
    }

    /// Returns the kernel command line of the capture's VM, which the caller passes as OpenVMM's
    /// `--cmdline`, alone: `nvx_sandbox=1`, then, for a template that boots with fewer processors
    /// online than its capacity, `maxcpus=` with the processors online at boot, as
    /// [The kernel command line](Self#the-kernel-command-line) describes.
    ///
    /// It takes the place of the command line of [`plan`](Self::plan), which
    /// [`SandboxPlan::kernel_command_line`] and a [`PreparedLaunch`] render: that one names the
    /// boot tokens of the plan's root, the identity of its image among them, which a clone binds
    /// when it is restored, and which OpenVMM's platform allowlist refuses.
    ///
    /// [`PreparedLaunch`]: crate::host::sandbox::PreparedLaunch
    pub fn kernel_command_line(&self) -> &str {
        &self.command_line
    }

    /// Returns OpenVMM's arguments of the capture: those of its devices, as
    /// [`SandboxPlan::openvmm_arguments`] renders them for [`plan`](Self::plan), the microVM
    /// machine, the placeholder as the read-only `distro` block device, and the dormant scratch
    /// device as the `scratch` one; then those of the NIC, if any,
    /// [`nic_arguments`](Self::nic_arguments); then those of
    /// [`capture_arguments`](Self::capture_arguments).
    ///
    /// The arguments name the block devices by their paths, which OpenVMM opens itself. A capture
    /// opens each without following a link at its path, and on Windows without sharing write
    /// access, so each path must name the file itself, which no other process holds open for
    /// writing. The arguments hold neither the kernel command line, which
    /// [`kernel_command_line`](Self::kernel_command_line) gives, nor the caller's own arguments.
    ///
    /// Fails as [`SandboxPlan::openvmm_arguments`] does.
    pub fn openvmm_arguments(&self) -> Result<Vec<String>> {
        let mut arguments = self.plan.openvmm_arguments()?;
        arguments.extend_from_slice(&self.nic);
        arguments.extend(self.capture_arguments());
        Ok(arguments)
    }

    /// Returns OpenVMM's arguments of the capture besides those of its devices: `--processors`
    /// with the processor capacity, `--memory` and `--memory-capacity` in MiB, and the capture's
    /// options, `--snapshot-destination`, `--snapshot-tier platform`,
    /// `--snapshot-block-identity generation`, and `--snapshot-generation-id`. The processors
    /// online at boot are the kernel command line's.
    ///
    /// A caller that renders the arguments of the devices itself adds these to them.
    pub fn capture_arguments(&self) -> Vec<String> {
        let TemplateResources {
            processors,
            memory,
            memory_capacity,
            ..
        } = self.resources;
        [
            ("--processors", processors.to_string()),
            ("--memory", mebibytes(memory)),
            ("--memory-capacity", mebibytes(memory_capacity)),
            (
                "--snapshot-destination",
                self.destination.display().to_string(),
            ),
            ("--snapshot-tier", TIER.to_owned()),
            ("--snapshot-block-identity", BLOCK_IDENTITY.to_owned()),
            ("--snapshot-generation-id", self.generation.to_string()),
        ]
        .into_iter()
        .flat_map(|(option, value)| [option.to_owned(), value])
        .collect()
    }
}

/// Returns whether OpenVMM attaches a sandbox block device of `length` bytes: a nonzero multiple
/// of [`LOGICAL_BLOCK_SIZE`].
fn is_block_length(length: u64) -> bool {
    length != 0 && length.is_multiple_of(LOGICAL_BLOCK_SIZE)
}

/// Refuses `resources` that no template's VM has: processors other than 1, 2, 4, or 8, processors
/// online at boot other than 1, 2, 4, or 8, or more than the processors, and RAM or a RAM capacity
/// that is not a nonzero multiple of [`MEMORY_BLOCK_SIZE`], or a capacity below the RAM.
fn check_resources(resources: &TemplateResources) -> Result<()> {
    let TemplateResources {
        processors,
        boot_processors,
        memory,
        memory_capacity,
    } = *resources;
    if !PROCESSOR_COUNTS.contains(&processors) {
        return Err(Error::unsupported(format!(
            "a template's VM has 1, 2, 4, or 8 processors, not {processors}"
        )));
    }
    if !PROCESSOR_COUNTS.contains(&boot_processors) || boot_processors > processors {
        return Err(Error::unsupported(format!(
            "a template's VM boots with 1, 2, 4, or 8 of its {processors} processors online, not \
             {boot_processors}"
        )));
    }
    if memory == 0
        || !memory.is_multiple_of(MEMORY_BLOCK_SIZE)
        || !memory_capacity.is_multiple_of(MEMORY_BLOCK_SIZE)
        || memory_capacity < memory
    {
        return Err(Error::unsupported(format!(
            "a template's VM needs nonzero RAM, and a RAM capacity of at least it, each a multiple \
             of {} MiB, not {memory} bytes of RAM and a capacity of {memory_capacity} bytes",
            MEMORY_BLOCK_SIZE / MIB
        )));
    }
    Ok(())
}

/// Returns the transaction directory `path`, spelled with its components alone, as
/// [`Path::components`] reads them, which must be valid Unicode and absolute.
fn transaction_directory(path: &Path) -> Result<PathBuf> {
    if path.to_str().is_none() {
        return Err(Error::unsupported(format!(
            "the transaction directory of a template's capture is not valid Unicode: {}",
            path.display()
        )));
    }
    let directory = paths::normalized(path);
    if !directory.is_absolute() {
        return Err(Error::unsupported(format!(
            "the transaction directory of a template's capture must be absolute, since OpenVMM \
             would resolve it against the directory in which it runs: {}",
            directory.display()
        )));
    }
    Ok(directory)
}

/// Returns `bytes`, a multiple of [`MIB`], as OpenVMM's memory sizes write it: the number of MiB
/// and `M`.
fn mebibytes(bytes: u64) -> String {
    format!("{}M", bytes / MIB)
}

#[cfg(test)]
mod tests;
