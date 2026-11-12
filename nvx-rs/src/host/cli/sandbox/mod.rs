//! `nvx-rs sandbox`: the port of the former `nvx.py sandbox`, from `command_sandbox` and the
//! `sandbox` parser of `scripts/nvx.py`, with its options, defaults, checks, outputs, and exit
//! statuses.
//!
//! - `run`, the default, runs one workload in a sandbox of its own, without a state directory: it
//!   launches the VM that a `provision` and a `start` of the same options would launch, in a
//!   temporary VM directory, prints its OpenVMM command as `>> COMMAND`, executes the workload
//!   once over NVXC, as `exec` would, with `exec`'s options, stops the VM, and exits with the
//!   workload's status, after its output. A dry run prints the command and launches nothing. See
//!   [`lifecycle::run`].
//! - `provision`, `start`, `exec`, `stop`, and `deprovision` manage a persistent sandbox in its
//!   `--state-dir`; see [`lifecycle`].
//!
//! The parser reads the options of `nvx.py sandbox` with their names, defaults, types, and
//! choices, and ends a usage error with status 2. The `--mount-deny`, `--mount-allow`, and
//! `--mount-write` options name paths of the `--mount` before them.

mod lifecycle;
mod lock;
mod options;
mod state;

use std::path::{Path, PathBuf};

use super::artifacts::{Located, OPENVMM_VARIABLE, locate};
use super::download::{self, Hold};
use super::session::ExecRequest;
use super::{Exit, Result, io_error, output};
use crate::ErrorCode;
use crate::host::artifacts::{Guest, KERNEL_NAME, require_artifact};
use crate::host::filesystem::{ExportPlan, ReservedTargets};
use crate::host::paths;
use crate::host::runtime::RuntimeIdentity;
use crate::host::sandbox::DEFAULT_ENTRYPOINT;
use crate::host::sandbox::distro::{DistroBinding, DistroFormat};
use crate::host::sandbox::layer::SandboxLayer;
use crate::host::sandbox::mount::{MOUNT_TAG, plan_mounts, validate_mounts};
use crate::host::sandbox::options::{SandboxOperation, SandboxOptions, sandbox_mounts};
use crate::protocol::export::{EXPORT_DIRECTORY, ExportMode};
use crate::protocol::root::RootUpper;

pub(crate) use self::lifecycle::ProvisionConfig;
pub(super) use self::lifecycle::{copy_openvmm_outcome, validate_outcome_destination};
pub(super) use self::options::sandbox_network;
pub(super) use self::options::{
    HYPERVISORS, MAX_MEMORY_MIB, MOUNT_OWNERS, OPTIONS, hypervisor, parse, path_text,
};
pub(crate) use self::options::{Options, path_string};
pub(crate) use self::state::Source;
use self::state::{NetworkRecord, RecordedDistro, check_images, check_workload};

/// The artifacts of a sandbox, found when an operation first needs them.
pub(crate) struct Artifacts<'a> {
    /// `--root`, if given.
    pub(super) root: Option<&'a str>,
}

impl Artifacts<'_> {
    /// Downloads the release of a checkout without artifacts for `hypervisor` before an
    /// operation runs them, as [`download::ensure`] decides, and returns what the operation holds
    /// while it reads them and launches OpenVMM on them.
    pub(crate) fn ensure(&self, hypervisor: &str) -> Result<Hold> {
        download::ensure(&locate(self.root)?, Some(Guest::Alpine), hypervisor)
    }

    /// Downloads the release of a checkout without artifacts for `hypervisor`, as
    /// [`download::ensure_under`] decides while the operation holds `held`, which
    /// [`hold`](Self::hold) returned before the operation read anything of the root, and returns
    /// what the operation holds while it reads the artifacts and launches OpenVMM on them.
    pub(crate) fn ensure_under(&self, held: Hold, hypervisor: &str) -> Result<Hold> {
        download::ensure_under(held, &locate(self.root)?, Some(Guest::Alpine), hypervisor)
    }

    /// Returns what an operation holds while it reads the runtime identity of the artifacts,
    /// which runs none of them, as [`download::hold`] says: it downloads nothing.
    pub(crate) fn hold(&self) -> Result<Hold> {
        download::hold(&locate(self.root)?)
    }

    /// Returns the OpenVMM executable, kernel, and Alpine initramfs, checked in that order as
    /// `nvx.py sandbox run` and `start` checked them.
    pub(crate) fn require(&self) -> Result<[String; 3]> {
        let Located { dir, openvmm } = locate(self.root)?;
        require_artifact(&openvmm, "OpenVMM release binary")?;
        let kernel = dir.artifact(KERNEL_NAME);
        require_artifact(&kernel, "Linux direct kernel")?;
        let initrd = dir.initramfs(Guest::Alpine);
        require_artifact(&initrd, "initramfs")?;
        Ok([
            path_string(&openvmm)?,
            path_string(&kernel)?,
            path_string(&initrd)?,
        ])
    }

    /// Returns the runtime identity of the artifacts, which the install metadata of the root
    /// names, as [`ArtifactDir::runtime_identity`](crate::host::artifacts::ArtifactDir::runtime_identity)
    /// reads it.
    pub(crate) fn runtime_identity(&self) -> Result<RuntimeIdentity> {
        locate(self.root)?
            .dir
            .runtime_identity()
            .map_err(error_with_cause)
    }

    /// Returns the runtime identity under which a managed start launches OpenVMM, as
    /// [`runtime_identity`](Self::runtime_identity) does. The install metadata names the root's
    /// own OpenVMM alone, so the start refuses another one that `NVX_OPENVMM` names, which the
    /// runtime record would otherwise misname.
    pub(crate) fn launch_identity(&self) -> Result<RuntimeIdentity> {
        let Located { dir, openvmm } = locate(self.root)?;
        if openvmm != dir.openvmm() {
            return Err(Exit::error(
                ErrorCode::Unsupported,
                format!(
                    "a managed sandbox runs the OpenVMM of the artifact root, which its install \
                     metadata identifies, and not the one that {OPENVMM_VARIABLE} names"
                ),
            ));
        }
        dir.runtime_identity().map_err(error_with_cause)
    }
}

/// Returns the error that prints the message of `error`, followed by the I/O error or other
/// cause that it keeps, if any, as Rust writes it, with the code of `error`.
pub(crate) fn error_with_cause(error: crate::Error) -> Exit {
    match std::error::Error::source(&error).map(ToString::to_string) {
        Some(cause) => Exit::error(error.code(), format!("{}: {cause}", error.message())),
        None => Exit::from(error),
    }
}

/// Returns the bootstrap tokens that OpenVMM appends to a VM's kernel command line for `plan`,
/// its export at [`EXPORT_DIRECTORY`], as [`HostExport`](crate::protocol::export::HostExport)
/// reads them: they take part of the line's budget.
pub(crate) fn export_tokens(plan: &ExportPlan) -> String {
    let mode = if plan.children.iter().any(|child| child.writable) {
        ExportMode::ReadWrite
    } else {
        ExportMode::ReadOnly
    };
    format!(
        " virtfs_dir={EXPORT_DIRECTORY} virtfs_tag={MOUNT_TAG} virtfs_mode={mode} \
         virtfs_aggregate=1"
    )
}

/// Runs the operation of `options`, as `command_sandbox` did.
pub(super) fn run(root: Option<&str>, options: Options) -> Result {
    run_from(root, options, None)
}

/// Provisions the sandbox of `options`, whose operation is `provision`, with `source`, the named
/// snapshot that each of its starts restores, then starts it, as `snapshot restore` does.
pub(super) fn restore(root: Option<&str>, options: Options, source: Source) -> Result {
    let (state_dir, timeout) = (options.state_dir.clone(), options.timeout);
    run_from(root, options, Some(&source))?;
    lifecycle::start(
        state_dir
            .as_deref()
            .expect("provision required --state-dir"),
        &Artifacts { root },
        timeout,
        false,
    )
}

/// Restores `source` once, as `run --restore-snapshot` does: provisions the sandbox of `options`
/// and starts it as [`restore`] does, but a workload that does not exit with status 0 ends the
/// start with its status, stops the VM once the workload has exited with status 0, and
/// deprovisions the sandbox once provisioned, whatever happened, so that the command ends as the
/// workload or the restore did. A sandbox that `options` names already is never removed.
///
/// A command that dies before it deprovisions the sandbox leaves it, which `sandbox stop` and
/// `sandbox deprovision` of its directory end and remove, and so does Ctrl-C while the stop waits
/// for the VM, after which the deprovision refuses a VM that still runs.
pub(super) fn restore_once(root: Option<&str>, options: Options, source: Source) -> Result {
    let (state_dir, timeout) = (
        options.state_dir.clone().unwrap_or_default(),
        options.timeout,
    );
    run_from(root, options, Some(&source))?;
    let artifacts = Artifacts { root };
    let restored = lifecycle::start(&state_dir, &artifacts, timeout, true)
        .and_then(|()| lifecycle::stop(&state_dir, &artifacts, timeout));
    let removed = lifecycle::deprovision(&state_dir, &artifacts, timeout).map_err(|error| {
        match error.message() {
            Some(message) => Exit::error(
                error.code().unwrap_or(ErrorCode::Io),
                format!("cannot remove the restore's sandbox {state_dir}: {message}"),
            ),
            None => error,
        }
    });
    match (restored, removed) {
        (Err(error), Err(removal)) => {
            if let Some(message) = removal.message() {
                output::print_stderr(&format!("warning: {message}"));
            }
            Err(error)
        }
        (restored, removed) => restored.and(removed),
    }
}

/// Runs the operation of `options`, recording `source` with the sandbox that it provisions.
fn run_from(root: Option<&str>, options: Options, source: Option<&Source>) -> Result {
    let operation = options.operation;
    if !matches!(
        operation,
        SandboxOperation::Run | SandboxOperation::Provision
    ) && (options.layer_format.is_some() || options.ram_upper_percent.is_some())
    {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            "--layer-format and --ram-upper-percent are only valid for sandbox run or provision",
        ));
    }
    if options.ram_upper_percent.is_some() && options.upper != Some(RootUpper::Ram) {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            "--ram-upper-percent requires --upper ram",
        ));
    }
    let checks = SandboxOptions {
        operation,
        dry_run: options.dry_run,
        cwd: options.cwd.clone(),
        environment: options.environment.clone(),
        environment_file: options.environment_file.clone(),
        inherit_default_environment: options.inherit_default_environment,
        network_options: options.network.any(),
        net: options.network.net.is_some(),
        network_profile: options.network.network_profile.is_some(),
        entrypoint: options.entrypoint.clone(),
        outcome_report: options.outcome_report.is_some(),
        mounts: options.mounts.clone(),
        policy_paths: options.policy_paths.clone(),
        mount_owner: options.mount_owner,
        layers: !options.layers.is_empty(),
        scratch: options.scratch.is_some(),
        upper: options.upper,
    };
    let environment = checks.check()?;
    let artifacts = Artifacts { root };
    // A run holds the lock of the downloads into a checkout from before it reads the distro
    // layer, which may be a release's, until its VM is ready, so that no download replaces the
    // layer or the artifacts meanwhile. It reports a failure to take it where it decides to
    // download, after its options. A provision holds it while it reads the layer and records the
    // sandbox, where it can take it: it runs no artifact, so it needs no artifact root. Ctrl-C
    // ends the wait for a download, and the command with it.
    let early = match operation {
        SandboxOperation::Run | SandboxOperation::Provision => match artifacts.hold() {
            Err(Exit::Interrupted) => return Err(Exit::Interrupted),
            Err(_) if matches!(operation, SandboxOperation::Provision) => Some(Ok(Hold::none())),
            held => Some(held),
        },
        _ => None,
    };
    let launch = if matches!(
        operation,
        SandboxOperation::Run | SandboxOperation::Provision
    ) {
        let rules = options.network.egress_rules()?;
        let mounts = sandbox_mounts(&options.mounts, &options.policy_paths, options.mount_owner)?;
        let layers = options
            .layers
            .iter()
            .map(|layer| {
                Ok(match options.layer_format {
                    Some(DistroFormat::Gpt) => {
                        SandboxLayer::gpt(layer.role(), &path_string(layer.path())?, layer.uuid())
                    }
                    _ => layer.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // A sandbox takes one read-only layer, of the one role of a layer: its distro layer.
        let layer = match &layers[..] {
            [layer] => layer.clone(),
            layers => {
                let message = match layers.first() {
                    Some(layer) => format!("duplicate sandbox layer role(s): {}", layer.role()),
                    None => "a sandbox requires exactly one read-only distro layer".to_owned(),
                };
                return Err(Exit::error(ErrorCode::Unsupported, message));
            }
        };
        // The workload of a run, and that of a restore, is an exec request, which no boot token
        // carries: a run's control session checks it, and `snapshot restore` a restore's.
        let (entrypoint, args) = if operation == SandboxOperation::Run || source.is_some() {
            (DEFAULT_ENTRYPOINT, &[][..])
        } else {
            (options.entrypoint.as_str(), &options.args[..])
        };
        check_workload(entrypoint, args, &options.hostname, options.workload_user)?;
        // The checks above require --scratch for an upper on scratch, and refuse it for one in
        // RAM.
        let scratch = match options.upper.unwrap_or(RootUpper::Scratch) {
            RootUpper::Scratch => Some(paths::normalized(Path::new(
                options.scratch.as_deref().unwrap_or_default(),
            ))),
            RootUpper::Ram => None,
        };
        validate_mounts(&mounts)?;
        check_images(&layer, scratch.as_deref(), None)?;
        // The shares are planned once, from their resolved host paths, and `provision` records
        // the plan, whose identities each `start` checks again.
        let export = plan_mounts(&mounts)?;
        if let Some(export) = &export {
            export.check(ReservedTargets::CONTAINER_ROOT)?;
        }
        // `run` and `provision` verify the distro layer explicitly, at the path given;
        // `provision` records its binding, which `start` checks without reading the layer again.
        let binding = DistroBinding::verify(&layer)?;
        // The network's policy is checked before anything launches or is recorded.
        sandbox_network(&options.network, &rules)?;
        let network = NetworkRecord::new(&options.network, &rules);
        Some((layer, scratch, export, binding, network))
    } else {
        None
    };
    let state_dir = |operation: &str| {
        options.state_dir.as_deref().ok_or_else(|| {
            Exit::error(
                ErrorCode::Unsupported,
                format!("sandbox {operation} requires --state-dir"),
            )
        })
    };
    // The workload that `exec` runs, or the one that `run` runs once.
    let mut arguments = vec![options.entrypoint.clone()];
    arguments.extend(options.args.iter().cloned());
    let request = ExecRequest {
        arguments,
        timeout_ms: options.exec_timeout_ms,
        cwd: options.cwd.clone(),
        environment,
        inherit_default_environment: options.inherit_default_environment,
    };
    match (operation, launch) {
        (SandboxOperation::Provision, Some((layer, scratch, export, binding, network))) => {
            // `provision` records the layer at its resolved path, where `start` checks the
            // binding, with the manifest beside that path.
            let distro = RecordedDistro::new(&layer, binding)?;
            let state_dir = state_dir("provision")?;
            // A provision runs no artifact, so it needs no artifact root; without one, `start`
            // checks the shares against the artifacts that it runs.
            let located = locate(root).ok();
            refuse_own_files(
                Some(("state directory", Path::new(state_dir))),
                layer.path(),
                scratch.as_deref(),
                export.as_ref(),
                located.as_ref(),
            )?;
            lifecycle::provision(
                state_dir,
                &layer,
                scratch.as_deref(),
                &distro,
                &recorded(&options, export.as_ref(), network.as_ref()),
                source,
                options.timeout,
            )
        }
        (SandboxOperation::Start, _) => {
            lifecycle::start(state_dir("start")?, &artifacts, options.timeout, false)
        }
        (SandboxOperation::Exec, _) => {
            let state_dir = state_dir("exec")?;
            if let Some(report) = &options.outcome_report {
                lifecycle::validate_outcome_destination(report)?;
            }
            let result = lifecycle::exec_workload(
                state_dir,
                &artifacts,
                &request,
                options.timeout,
                options.wait,
            )?;
            output::write_stdout(&result.stdout)
                .and_then(|()| output::write_stderr(&result.stderr))
                .map_err(Exit::from_os_error)?;
            if let Some(report) = &options.outcome_report {
                // An error of the report follows the workload's output, and starts a line of its
                // own even after standard error that the workload left without a line end, so
                // that callers find it as they find any other.
                lifecycle::write_exec_outcome(report, &result)
                    .inspect_err(|_| output::end_stderr_line(&result.stderr))?;
            }
            Err(Exit::Status(result.returncode))
        }
        (SandboxOperation::Stop, _) => {
            lifecycle::stop(state_dir("stop")?, &artifacts, options.timeout)
        }
        (SandboxOperation::Deprovision, _) => {
            lifecycle::deprovision(state_dir("deprovision")?, &artifacts, options.timeout)
        }
        (SandboxOperation::Run, Some((layer, scratch, export, binding, network))) => {
            if options.state_dir.is_some() {
                return Err(Exit::error(
                    ErrorCode::Unsupported,
                    "sandbox run keeps no state: it rejects --state-dir",
                ));
            }
            // A run launches the layer at the path that a provision would record.
            let distro = RecordedDistro::new(&layer, binding)?;
            let held = early.transpose()?.unwrap_or_else(Hold::none);
            let Some(result) = lifecycle::run(
                &distro,
                scratch.as_deref(),
                &recorded(&options, export.as_ref(), network.as_ref()),
                &artifacts,
                &options,
                &request,
                held,
            )?
            else {
                return Ok(());
            };
            output::write_stdout(&result.stdout)
                .and_then(|()| output::write_stderr(&result.stderr))
                .map_err(Exit::from_os_error)?;
            Err(Exit::Status(result.returncode))
        }
        (SandboxOperation::Run | SandboxOperation::Provision, None) => {
            unreachable!("run and provision build their launch")
        }
    }
}

/// Returns the options of `options` that `provision` records beside the images of its launch, with
/// `export`, the export plan of its shares, and `network`, the record of its network options,
/// which a run records and reads back too.
fn recorded<'a>(
    options: &'a Options,
    export: Option<&'a ExportPlan>,
    network: Option<&'a NetworkRecord>,
) -> ProvisionConfig<'a> {
    ProvisionConfig {
        hypervisor: hypervisor(options.hypervisor),
        memory_mib: options.memory_mib,
        cmdline: &options.cmdline,
        ram_upper_percent: options.ram_upper_percent,
        hostname: &options.hostname,
        workload_identity: options.workload_user,
        export,
        network,
    }
}

/// Refuses a share of the sandbox's own files: one whose host path equals, contains, or lies
/// inside `directory`, if the sandbox has one, with its name, its state directory, a run's VM
/// directory, or a restored sandbox's snapshot store; its image, `image`, or its scratch image,
/// `scratch`, if any; or, if known, the OpenVMM, kernel, or Alpine initramfs of the artifact root
/// of `artifacts`, or the OpenVMM that `NVX_OPENVMM` names instead, which a run runs. The shares
/// are the children of `export`, if any. Through them, the guest could read or change the
/// sandbox's records, its storage, its snapshot, or what the host runs.
///
/// The paths are compared once [`paths::resolve`] has resolved them, without a Windows verbatim
/// prefix. Resolution follows symbolic links and junctions, but not hard links or bind mounts.
/// `provision` checks before it records the sandbox, and `start` again before it launches it,
/// since the shares' paths may have come to resolve elsewhere; `run`, which has no state
/// directory, checks before it creates its VM directory and launches its VM.
fn refuse_own_files(
    directory: Option<(&str, &Path)>,
    image: &Path,
    scratch: Option<&Path>,
    export: Option<&ExportPlan>,
    artifacts: Option<&Located>,
) -> Result {
    let Some(export) = export else {
        return Ok(());
    };
    let mut owned: Vec<_> = directory
        .map(|(name, path)| (name, path.to_owned()))
        .into_iter()
        .collect();
    owned.push(("image", image.to_owned()));
    owned.extend(scratch.map(|path| ("scratch image", path.to_owned())));
    if let Some(Located { dir, openvmm }) = artifacts {
        owned.extend([
            ("OpenVMM", dir.openvmm()),
            ("OpenVMM", openvmm.clone()),
            ("kernel", dir.kernel()),
            ("initramfs", dir.initramfs(Guest::Alpine)),
        ]);
    }
    let resolve = |path: &Path| {
        paths::resolve(path)
            .map(without_verbatim_prefix)
            .map_err(|error| io_error(ErrorCode::Io, &error, path))
    };
    let owned = owned
        .into_iter()
        .map(|(name, path)| Ok((name, resolve(&path)?)))
        .collect::<Result<Vec<_>>>()?;
    for child in &export.children {
        let shared = resolve(&child.root)?;
        let overlap = owned
            .iter()
            .find(|(_, path)| shared.starts_with(path) || path.starts_with(&shared));
        if let Some((name, path)) = overlap {
            return Err(Exit::error(
                ErrorCode::Unsupported,
                format!(
                    "sandbox mount host path {} overlaps the sandbox's {name} {}; a share cannot \
                     expose the sandbox's own files",
                    shared.display(),
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Returns `path` without the verbatim prefix, `\\?\`, with which [`paths::resolve`] keeps a
/// Windows path that only that prefix names, such as one whose name ends in a period: its
/// components would otherwise differ from those of a path that it lies inside. A network path
/// keeps its prefix, since no share is one.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with(r"UNC\") => PathBuf::from(rest),
        _ => path,
    }
}

#[cfg(test)]
pub(super) mod tests;
