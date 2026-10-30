//! The prepared launch of a sandbox: the files that the checks of its plan returned, held open
//! until its VMM has them, the descriptors or handles that the VMM inherits, and the OpenVMM
//! arguments and kernel command line that name them.
//!
//! A [`SandboxPlan`] names its block devices by path, and persists. A launch that gave the VMM
//! those paths would let anything that took a path's place after the check, and before the VMM
//! opened the path, take the image's place. A [`PreparedLaunch`] instead takes the files that the
//! checks returned, the [`CheckedDistro`] of
//! [`DistroBinding::check`](super::distro::DistroBinding::check) and, for an upper on scratch,
//! the [`CheckedScratch`] of [`ScratchBinding::check`](super::scratch::ScratchBinding::check), and
//! hands the VMM exactly those objects:
//!
//! - On Linux, the VMM inherits the checked descriptors, through the allowlist of
//!   [`VmmCommand::inherit`](crate::host::supervisor::VmmCommand::inherit), and the OpenVMM
//!   arguments name each `/proc/self/fd/N`, which OpenVMM opens: the link reaches the checked file
//!   whatever later takes its path. No other descriptor of the caller reaches the VMM. A checked
//!   file on a standard stream's descriptor, which no spawn can pass under its number, as when the
//!   caller had closed that stream, first moves above the standard streams, with its open file
//!   description.
//! - On Windows, the arguments name each checked file by its final path, the absolute `\\?\` path
//!   that the file's handle gives, which resolves every link and junction that the plan's path
//!   went through, and does not depend on the VMM's directory. The checked files hold that path in
//!   place until they are closed: the distro layer's file denies other opens for writing or
//!   deleting the layer, the scratch image's guard denies deleting or renaming it, and NTFS renames
//!   no directory while a file below it is open. OpenVMM's read-only open of the layer, and its
//!   open of the scratch image for reading and writing, share them. The VMM also inherits their
//!   handles, and the claim's handle of the scratch image, though no argument names a handle, so
//!   that its copies hold the paths, and the claim, from its start, even if the caller ends before
//!   the VMM has opened the images, as when it crashes: a replacement at their paths then fails
//!   until the VMM exits, and never reaches it unchecked. A final path that OpenVMM could not take
//!   as the path of a raw image, with a comma, semicolon, unbalanced brackets, or a container's
//!   extension, which a link on the way, the directory of a relative path, or the case of a name
//!   on disk can bring, fails the preparation.
//! - Other hosts cannot check files, so no launch is prepared there.
//!
//! Images are raw: OpenVMM attaches a file's bytes as they are, and the launch never converts an
//! image or reads its footer, as [`SandboxDistro`](super::SandboxDistro) says.
//!
//! The paths of descriptors exist only in the launch: [`PreparedLaunch::openvmm_arguments`]
//! renders them, and the plan, which callers persist, keeps the real paths. The caller adds its own
//! arguments, such as its boot console, CPU profile, and guest control, starts the VMM with
//! [`PreparedLaunch::inherited`] as its allowlist, and holds the prepared launch until the guest
//! reports authenticated Ready, when [`PreparedLaunch::release`] closes the checked files, which
//! releases the guards, or until it has torn down a launch that failed, when dropping the prepared
//! launch closes them. The VMM keeps its own opens of the images, and the claim of the scratch
//! image, which it inherited. On Windows, the release also closes the VMM's copies of the checked
//! files, through the handle of the VMM that its launch holds; a prepared launch that is dropped
//! without one, as when its caller ends before the guest is ready, leaves them to the VMM until it
//! exits.
//!
//! A plan never names a root identity for the workload: [`SandboxPlan::new`] refuses a zero UID
//! or GID, so the VMM's `--microvm-workload-identity` never encodes root, and an adapter's plan,
//! [`SandboxPlan::adapter`], names no workload identity, which the adapter's own guest chooses.

use std::fs::File;
use std::path::Path;

use super::distro::CheckedDistro;
#[cfg(any(target_os = "linux", windows))]
use super::mount::os_error;
use super::plan::{self, BlockRole, Delivery, SandboxPlan};
use super::refused;
use super::scratch::CheckedScratch;
#[cfg(windows)]
use super::{reject_container_path, reject_disk_path};
#[cfg(windows)]
use crate::error::Error;
use crate::error::Result;
use crate::host::process::Process;
#[cfg(windows)]
#[cfg(windows)]
use crate::host::seal;
use crate::host::supervisor::Inherited;
#[cfg(target_os = "linux")]
use crate::host::sys;

/// A launch that holds the checked files of its plan, and the OpenVMM arguments and kernel
/// command line that hand them to the VMM.
///
/// [`new`](Self::new) prepares it from a plan and the files that the checks of its block devices
/// returned. The caller then starts the VMM with [`openvmm_arguments`](Self::openvmm_arguments),
/// its own arguments, and [`kernel_command_line`](Self::kernel_command_line), with
/// [`inherited`](Self::inherited) as the allowlist of its
/// [`VmmCommand`](crate::host::supervisor::VmmCommand), and [releases](Self::release) the
/// prepared launch once the guest reports authenticated Ready, or drops it once a launch that
/// failed is torn down:
///
/// ```no_run
/// use std::ffi::OsString;
/// use std::fs::File;
/// use std::time::Instant;
///
/// use nvx::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding};
/// use nvx::host::sandbox::scratch::{CLAIM_RELEASE_WAIT, ScratchBinding};
/// use nvx::host::sandbox::{PreparedLaunch, SandboxLaunch};
/// use nvx::host::supervisor::{self, VmmCommand};
///
/// # fn launch(
/// #     launch: &SandboxLaunch,
/// #     distro_binding: &DistroBinding,
/// #     scratch_binding: Option<&ScratchBinding>,
/// #     capability: &[u8; nvx::protocol::CAPABILITY_LEN],
/// # ) -> Result<(), Box<dyn std::error::Error>> {
/// // Check the bindings that the caller recorded when it staged the images: from their metadata
/// // alone, and claiming the scratch image of an upper on scratch for this sandbox.
/// let distro = distro_binding.check(launch.distro(), DISTRO_POLICY_REVISION)?;
/// let scratch = match (launch.scratch(), scratch_binding) {
///     (Some(path), Some(binding)) => Some(binding.check(path, Instant::now() + CLAIM_RELEASE_WAIT)?),
///     _ => None,
/// };
/// let prepared = PreparedLaunch::new(launch.bound_plan(distro_binding)?, distro, scratch, "")?;
///
/// let mut arguments: Vec<OsString> =
///     prepared.openvmm_arguments().iter().map(OsString::from).collect();
/// arguments.extend(["--cmdline".into(), prepared.kernel_command_line().into()]);
/// // ... the caller's own arguments: hypervisor, memory, kernel, initrd, and guest control ...
/// let mut command = VmmCommand::new("openvmm", arguments, ".");
/// command.inherit = prepared.inherited();
/// // A log file of its own, created anew, as `supervisor::spawn` asks of each VMM on Windows.
/// let _ = std::fs::remove_file("openvmm.log");
/// let launched = supervisor::spawn(&command, capability, File::create_new("openvmm.log")?)?;
/// // ... record the VMM, and wait for the guest's authenticated Ready ...
/// prepared.release(&launched.process()?)?;
/// launched.detach();
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct PreparedLaunch {
    /// The plan, which names the images by their real paths.
    plan: SandboxPlan,
    /// The checked distro layer.
    distro: CheckedDistro,
    /// The checked scratch image, for an upper on scratch.
    scratch: Option<CheckedScratch>,
    /// The OpenVMM arguments of the plan, which name the checked files.
    openvmm_arguments: Vec<String>,
    /// The kernel command line of the plan.
    kernel_command_line: String,
}

impl PreparedLaunch {
    /// Prepares the launch of `plan` with the checked files of its block devices: `distro`, which
    /// [`DistroBinding::check`](super::distro::DistroBinding::check) returned for the plan's
    /// image, and `scratch`, which [`ScratchBinding::check`](super::scratch::ScratchBinding::check)
    /// returned for the scratch image of an upper on scratch, or `None` for an upper in RAM. It
    /// renders the plan's OpenVMM arguments, naming the checked files, and its kernel command line
    /// with `user_command_line`, as [`SandboxPlan::kernel_command_line`] does.
    ///
    /// The checked files must be those of the plan: the distro layer checked at the path of the
    /// plan's image, with the format and identity of its image, and the scratch image checked at
    /// the path of its upper. Fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for files that are not, and as
    /// [`SandboxPlan::openvmm_arguments`] and [`SandboxPlan::kernel_command_line`] fail; on
    /// Windows, also with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a checked
    /// file whose final path has a comma, semicolon, or unbalanced brackets, or ends in `.vhd`,
    /// `.vhdx`, `.vmgs`, or `.iso`, which OpenVMM could not attach as a raw image; and with
    /// [`ErrorKind::Io`](crate::ErrorKind::Io) if a checked file cannot be moved above the
    /// standard streams on Linux, or its final path is unavailable on Windows. It never opens,
    /// reads, or maps a file.
    pub fn new(
        plan: SandboxPlan,
        distro: CheckedDistro,
        scratch: Option<CheckedScratch>,
        user_command_line: &str,
    ) -> Result<Self> {
        Self::prepare(
            plan,
            distro,
            scratch,
            user_command_line,
            Delivery::MappingTable,
        )
    }

    /// Prepares the one-shot launch of `plan`, as [`new`](Self::new) does, but with the shares as
    /// the one-shot lifecycle delivers them, which
    /// [`SandboxPlan::one_shot_openvmm_arguments`] and
    /// [`SandboxPlan::one_shot_kernel_command_line`] render: as the guest's shell binds them at
    /// boot.
    #[cfg(feature = "cli")]
    pub(crate) fn one_shot(
        plan: SandboxPlan,
        distro: CheckedDistro,
        scratch: Option<CheckedScratch>,
        user_command_line: &str,
    ) -> Result<Self> {
        Self::prepare(
            plan,
            distro,
            scratch,
            user_command_line,
            Delivery::BootTokens,
        )
    }

    /// Implements [`new`](Self::new) and [`one_shot`](Self::one_shot), with the shares as
    /// `delivery` delivers them.
    fn prepare(
        plan: SandboxPlan,
        distro: CheckedDistro,
        scratch: Option<CheckedScratch>,
        user_command_line: &str,
        delivery: Delivery,
    ) -> Result<Self> {
        let planned = plan.distro().path();
        if !same_path(distro.path(), planned) {
            return Err(refused(format!(
                "the checked distro layer {} is not the plan's image {}",
                distro.path().display(),
                planned.display()
            )));
        }
        let (format, identity) = distro.bound();
        if plan::image_of(format, identity)? != plan.distro().image() {
            return Err(refused(format!(
                "the plan's image is not the {format} layer with identity {identity} that the \
                 distro binding of {} names",
                planned.display()
            )));
        }
        match (plan.upper().scratch_path(), &scratch) {
            (Some(planned), Some(checked)) if !same_path(checked.path(), planned) => {
                return Err(refused(format!(
                    "the checked scratch image {} is not the plan's scratch image {}",
                    checked.path().display(),
                    planned.display()
                )));
            }
            (Some(planned), None) => {
                return Err(refused(format!(
                    "the plan's upper is on the scratch image {}, which was not checked",
                    planned.display()
                )));
            }
            (None, Some(checked)) => {
                return Err(refused(format!(
                    "the plan's upper is in RAM, which takes no scratch image, not {}",
                    checked.path().display()
                )));
            }
            _ => {}
        }
        #[cfg(target_os = "linux")]
        let (distro, scratch) = above_standard_streams(distro, scratch)?;
        let distro_block = checked_path(distro.file(), distro.path())?;
        let scratch_block = match &scratch {
            Some(scratch) => Some(checked_path(scratch.file(), scratch.path())?),
            None => None,
        };
        let openvmm_arguments = plan.render_arguments(
            &|role, _| match (role, &scratch_block) {
                (BlockRole::Distro, _) => distro_block.clone(),
                (BlockRole::Scratch, Some(block)) => block.clone(),
                (BlockRole::Scratch, None) => {
                    unreachable!("a plan with a scratch block has its checked scratch image")
                }
            },
            delivery,
        )?;
        let kernel_command_line = plan.command_line(user_command_line, delivery)?;
        Ok(Self {
            plan,
            distro,
            scratch,
            openvmm_arguments,
            kernel_command_line,
        })
    }

    /// Returns the plan, which names the images by their real paths.
    pub fn plan(&self) -> &SandboxPlan {
        &self.plan
    }

    /// Returns the checked distro layer.
    pub fn distro(&self) -> &CheckedDistro {
        &self.distro
    }

    /// Returns the checked scratch image, or `None` for an upper in RAM.
    pub fn scratch(&self) -> Option<&CheckedScratch> {
        self.scratch.as_ref()
    }

    /// Returns the OpenVMM arguments of the plan, as [`SandboxPlan::openvmm_arguments`] renders
    /// them, or for the CLI's one-shot launch with the shares as the guest's shell binds them at
    /// boot, except that each block device names its checked file: on Linux as `/proc/self/fd/N`,
    /// which [`inherited`](Self::inherited) passes the VMM under the number `N`, and on Windows by
    /// its final path.
    ///
    /// They hold for this launch only: never persist them.
    pub fn openvmm_arguments(&self) -> &[String] {
        &self.openvmm_arguments
    }

    /// Returns the kernel command line of the plan, with the user's command line given to
    /// [`new`](Self::new), as [`SandboxPlan::kernel_command_line`] renders it, or for the CLI's
    /// one-shot launch with the shares' boot tokens.
    pub fn kernel_command_line(&self) -> &str {
        &self.kernel_command_line
    }

    /// Returns the descriptors or handles that the VMM inherits, for the allowlist of its
    /// [`VmmCommand::inherit`](crate::host::supervisor::VmmCommand::inherit): those of the checked
    /// distro layer and of the checked scratch image, if any, which on Linux
    /// [`openvmm_arguments`](Self::openvmm_arguments) names, and on Windows, where the VMM opens the
    /// checked files by their final paths, hold those paths for the VMM, with the claim's handle of
    /// the scratch image, which no argument names; elsewhere none.
    pub fn inherited(&self) -> Vec<Inherited<'_>> {
        #[cfg(any(target_os = "linux", windows))]
        {
            let mut inherited = vec![Inherited::new(self.distro.file())];
            if let Some(scratch) = &self.scratch {
                inherited.push(Inherited::new(scratch.file()));
                #[cfg(windows)]
                inherited.push(Inherited::new(scratch.claim()));
            }
            inherited
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        Vec::new()
    }

    /// Releases the checked files once the VMM has opened the images, when the guest reports
    /// authenticated Ready, which releases their guards: on Windows, it first closes the VMM's
    /// copies of the checked files, those of the distro layer and of the scratch image's guard,
    /// then, on every host, the checked files themselves, as dropping the prepared launch does. The
    /// VMM keeps its own opens of the images, and the scratch image's claim, which it inherited.
    ///
    /// `vmm` must be the VMM that inherited [`inherited`](Self::inherited), held as its launch
    /// holds it: through [`Starting::process`](crate::host::supervisor::Starting::process), or
    /// [`Launched::process`](crate::host::supervisor::Launched::process). A VMM that has exited
    /// has closed its copies. A launch that failed needs no release: dropping the prepared launch
    /// once the launch is torn down closes the checked files, and the VMM's copies went with it. A
    /// prepared launch that is dropped without a release, as when its caller ends before the guest
    /// is ready, leaves the VMM its copies until it exits.
    ///
    /// Fails on Windows with [`ErrorKind::Io`](crate::ErrorKind::Io) if the VMM's copy of a
    /// checked file cannot be closed: for a `vmm` that did not inherit it, which keeps all of its
    /// handles, or one held without the right to close them, as [`Process::open`] holds a process.
    /// The checked files are closed whatever the outcome, and the VMM keeps the copies that remain
    /// until it exits.
    pub fn release(self, vmm: &Process) -> Result<()> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsHandle;

            let mut checked = vec![(self.distro.file(), self.distro.path())];
            checked.extend(
                self.scratch
                    .as_ref()
                    .map(|scratch| (scratch.file(), scratch.path())),
            );
            for (file, path) in checked {
                vmm.close_inherited(file.as_handle()).map_err(|error| {
                    Error::io(format!(
                        "cannot release the guard of {}, which VMM process {} inherited",
                        path.display(),
                        vmm.reference().pid
                    ))
                    .with_source(error)
                })?;
            }
        }
        #[cfg(not(windows))]
        let _ = vmm;
        drop(self);
        Ok(())
    }
}

/// Returns whether `checked` and `planned`, two paths spelled with their components alone, as
/// [`Path::components`] reads them, are the same text.
fn same_path(checked: &Path, planned: &Path) -> bool {
    checked.as_os_str() == planned.as_os_str()
}

/// Moves the checked files that are on a standard stream's descriptor, under whose number no spawn
/// passes them to the VMM, above the standard streams, as when the caller had closed that stream
/// before the checks opened them. A moved file keeps its open file description, and with it the
/// claim of a scratch image.
#[cfg(target_os = "linux")]
fn above_standard_streams(
    mut distro: CheckedDistro,
    mut scratch: Option<CheckedScratch>,
) -> Result<(CheckedDistro, Option<CheckedScratch>)> {
    sys::move_above_standard_streams(distro.file_mut())
        .map_err(|error| os_error(&error, Some(distro.path())))?;
    if let Some(scratch) = &mut scratch {
        sys::move_above_standard_streams(scratch.file_mut())
            .map_err(|error| os_error(&error, Some(scratch.path())))?;
    }
    Ok((distro, scratch))
}

/// Returns how the OpenVMM arguments name the checked `file`, which a check opened at `path`: on
/// Linux, as `/proc/self/fd/N`; on Windows, by its final path, which resolves no link, and which
/// holds whatever the VMM's directory, refusing one that OpenVMM could not take as the path of a
/// raw image, as [`SandboxPlan::validated`] refuses the plan's paths; elsewhere, as `path`.
fn checked_path(file: &File, path: &Path) -> Result<String> {
    #[cfg(target_os = "linux")]
    {
        let _ = path;
        Ok(Inherited::new(file).proc_path().display().to_string())
    }
    #[cfg(windows)]
    {
        let resolved = seal::final_path(file)
            .map_err(|error| os_error(&error, Some(path)))?
            .into_os_string()
            .into_string()
            .map_err(|_| {
                refused(format!(
                    "the final path of {} is not valid Unicode",
                    path.display()
                ))
            })?;
        // OpenVMM reads the delimiters and the extension of the path that it is given, which a
        // link on the way, the directory of a relative path, or the case of a name on disk can
        // make differ from the plan's.
        let shown = format!("{} resolves to {resolved}", path.display());
        reject_disk_path(Path::new(&resolved), &shown)?;
        reject_container_path(Path::new(&resolved), &shown)?;
        Ok(resolved)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = file;
        Ok(path.to_string_lossy().into_owned())
    }
}

#[cfg(all(test, any(target_os = "linux", windows)))]
mod tests;
