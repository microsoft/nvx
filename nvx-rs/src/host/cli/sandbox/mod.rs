//! `nvx-rs sandbox`: the port of the former `nvx.py sandbox`, from `command_sandbox` and the
//! `sandbox` parser of `scripts/nvx.py`, with its options, defaults, checks, outputs, and exit
//! statuses.
//!
//! - `run`, the default, boots a one-shot sandbox: it prints the OpenVMM command as `>> COMMAND`,
//!   then runs it on the terminal, unless `--dry-run`, and exits with OpenVMM's status. It runs
//!   OpenVMM through a [`PreparedLaunch`] of the distro layer that it verified and, for an upper
//!   on scratch, of the scratch image, which it claims, so the command names their checked files:
//!   on Linux as `/proc/self/fd/N`, and on Windows by their final paths. A dry run, which launches
//!   nothing, names their paths. `--upper ram` keeps the upper in RAM, on a tmpfs that the guest
//!   mounts, and attaches no scratch image.
//! - `provision`, `start`, `exec`, `stop`, and `deprovision` manage a persistent sandbox in its
//!   `--state-dir`; see [`lifecycle`].
//!
//! The parser reads the options of `nvx.py sandbox` with their names, defaults, types, and
//! choices, and ends a usage error with status 2. The `--mount-deny`, `--mount-allow`, and
//! `--mount-write` options name paths of the `--mount` before them.

mod lifecycle;
mod lock;
mod one_shot;
mod options;
mod state;

use std::time::Instant;

use super::artifacts::{Located, OPENVMM_VARIABLE, locate};
use super::download::{self, Hold};
use super::shell::format_command;
use super::{Exit, Result, output};
use crate::ErrorCode;
use crate::host::artifacts::{Guest, KERNEL_NAME, require_artifact};
use crate::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding};
use crate::host::sandbox::mount::{plan_mounts, validate_mounts};
use crate::host::sandbox::options::{
    SandboxOperation, SandboxOptions, sandbox_mounts, validate_sandbox_systemd_policy,
};
use crate::host::sandbox::scratch::{CLAIM_RELEASE_WAIT, ScratchBinding};
use crate::host::sandbox::{
    PreparedLaunch, SandboxLaunch, SandboxPlan, SandboxSettings, SandboxUpper,
};
use crate::host::store::RuntimeIdentity;
use crate::protocol::root::RootUpper;

pub(crate) use self::lifecycle::ProvisionConfig;
pub(super) use self::one_shot::run_openvmm;
use self::options::sandbox_network;
pub(super) use self::options::{
    HYPERVISORS, MAX_MEMORY_MIB, MOUNT_OWNERS, hypervisor, parse, path_text,
};
#[cfg(test)]
pub(super) use self::options::{OPTIONS, SYNTAX};
pub(crate) use self::options::{Options, path_string};
use self::state::RecordedDistro;

/// The artifacts of a sandbox, found when an operation first needs them.
pub(crate) struct Artifacts<'a> {
    /// `--root`, if given.
    root: Option<&'a str>,
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

/// Runs the operation of `options`, as `command_sandbox` did.
pub(super) fn run(root: Option<&str>, options: Options) -> Result {
    let operation = options.operation;
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
    // A one-shot run holds the lock of the downloads into a checkout from before it reads the
    // distro layer, which may be a release's, until it has launched OpenVMM, so that no download
    // replaces the layer or the artifacts meanwhile. It reports a failure to take it where it
    // decides to download, after its options. A provision holds it while it reads the layer and
    // records the sandbox, where it can take it: it runs no artifact, so it needs no artifact root.
    // Ctrl-C ends the wait for a download, and the command with it.
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
        let settings = SandboxSettings {
            entrypoint: options.entrypoint.clone(),
            args: options.args.clone(),
            hostname: options.hostname.clone(),
            workload_identity: options.workload_user,
            memory_max: options.memory_max,
            pids_max: options.pids_max,
        };
        // The checks above require --scratch for an upper on scratch, and refuse it for one in
        // RAM.
        let upper = match options.upper.unwrap_or(RootUpper::Scratch) {
            RootUpper::Scratch => {
                SandboxUpper::scratch(options.scratch.as_deref().unwrap_or_default())
            }
            RootUpper::Ram => SandboxUpper::RAM,
        };
        let launch = SandboxLaunch::new(options.layers.clone(), upper, settings)?;
        validate_mounts(&mounts)?;
        launch.validated()?;
        // The shares are planned once, from their resolved host paths, and `provision` records
        // the plan, whose identities each `start` checks again.
        let launch = launch.with_export(plan_mounts(&mounts)?)?;
        // `run` and `provision` verify the distro layer explicitly, at the path given;
        // `provision` records its binding, which `start` checks without reading the layer again.
        let binding = validate_sandbox_systemd_policy(&launch)?;
        // The network's policy is checked before anything launches or is recorded.
        let launch = launch.with_network(sandbox_network(&options, &rules)?);
        Some((launch, binding))
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
    match (operation, launch) {
        (SandboxOperation::Provision, Some((launch, binding))) => {
            // `provision` records the layer at its resolved path, where `start` checks the
            // binding, with the manifest beside that path.
            let distro = RecordedDistro::new(&launch, binding)?;
            let state_dir = state_dir("provision")?;
            lifecycle::provision(
                state_dir,
                &launch,
                &distro,
                &ProvisionConfig {
                    hypervisor: hypervisor(options.hypervisor),
                    memory_mib: options.memory_mib,
                    cmdline: &options.cmdline,
                },
                options.timeout,
            )
        }
        (SandboxOperation::Start, _) => {
            lifecycle::start(state_dir("start")?, &artifacts, options.timeout)
        }
        (SandboxOperation::Exec, _) => {
            let state_dir = state_dir("exec")?;
            if let Some(report) = &options.outcome_report {
                lifecycle::validate_outcome_destination(report)?;
            }
            let mut arguments = vec![options.entrypoint.clone()];
            arguments.extend(options.args.iter().cloned());
            let result = lifecycle::exec_workload(
                state_dir,
                &artifacts,
                &super::session::ExecRequest {
                    arguments,
                    timeout_ms: options.exec_timeout_ms,
                    cwd: options.cwd.clone(),
                    environment,
                    inherit_default_environment: options.inherit_default_environment,
                },
                options.timeout,
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
        (SandboxOperation::Run, Some((launch, binding))) => {
            if options.state_dir.is_some() {
                return Err(Exit::error(
                    ErrorCode::Unsupported,
                    "one-shot sandbox execution rejects persistent --state-dir settings",
                ));
            }
            let plan = run_plan(&launch, &binding)?;
            // The shares and the command line depend on the options alone, and a missing artifact
            // is reported before them, as before, but a run that they refuse downloads nothing.
            let refused = plan.one_shot_openvmm_arguments().is_err()
                || plan.one_shot_kernel_command_line(&options.cmdline).is_err();
            let held = early.transpose()?.unwrap_or_else(Hold::none);
            let hold = if refused {
                held
            } else {
                artifacts.ensure_under(held, hypervisor(options.hypervisor))?
            };
            let [openvmm, kernel, initrd] = artifacts.require()?;
            // A one-shot run delivers the shares as boot tokens. A dry run launches nothing, so it
            // shows the plan's own paths; a run launches through the checked files, which it holds
            // until OpenVMM exits.
            let (arguments, command_line, prepared) = if options.dry_run {
                (
                    plan.one_shot_openvmm_arguments()?,
                    plan.one_shot_kernel_command_line(&options.cmdline)?,
                    None,
                )
            } else {
                let prepared = prepare_run(&launch, &binding, plan, &options.cmdline)?;
                (
                    prepared.openvmm_arguments().to_vec(),
                    prepared.kernel_command_line().to_owned(),
                    Some(prepared),
                )
            };
            let mut command = vec![openvmm];
            command.extend(arguments);
            command.extend(
                [
                    "--microvm-lifecycle",
                    "one-shot",
                    "--single-process",
                    "--hypervisor",
                    hypervisor(options.hypervisor),
                ]
                .map(str::to_owned),
            );
            command.extend([
                "--memory".to_owned(),
                format!("{}M", options.memory_mib),
                "--kernel".to_owned(),
                kernel,
                "--initrd".to_owned(),
                initrd,
                "--cmdline".to_owned(),
                command_line,
            ]);
            if let Some(report) = &options.outcome_report {
                command.extend(["--microvm-report".to_owned(), path_text(report)]);
            }
            output::print_stdout(&format!(">> {}", format_command(&command)))
                .map_err(Exit::from_os_error)?;
            let Some(prepared) = prepared else {
                return Ok(());
            };
            Err(Exit::Status(run_openvmm(
                &command,
                &prepared.inherited(),
                hold,
            )?))
        }
        (SandboxOperation::Run | SandboxOperation::Provision, None) => {
            unreachable!("run and provision build their launch")
        }
    }
}

/// Checks the launch of a one-shot run again, and returns its plan with the format and identity of
/// the distro layer that `binding`, which the run's verification of the layer made, records.
///
/// The verification read all of the layer after the shares were planned, so the launch must still
/// pass [`SandboxLaunch::validated`], as `provision` checks it before recording it and `start`
/// checks a recorded one: the directories and files of its export plan must still not overlap, by
/// path or through a hard link or bind mount, and must pass [`ExportPlan::verify`].
///
/// [`ExportPlan::verify`]: crate::host::filesystem::ExportPlan::verify
fn run_plan(launch: &SandboxLaunch, binding: &DistroBinding) -> Result<SandboxPlan> {
    launch.validated()?;
    launch.bound_plan(binding).map_err(Exit::from)
}

/// Checks the distro layer of a one-shot run against `binding`, which the run's verification of
/// the layer made, binds and claims the scratch image of an upper on scratch, and prepares the
/// one-shot launch of `plan`, which delivers the shares as boot tokens, with the checked files and
/// the user's kernel command line `cmdline`.
///
/// The checks read no content, so the layer that the run verified is the one that OpenVMM gets:
/// one that changed meanwhile fails its check.
fn prepare_run(
    launch: &SandboxLaunch,
    binding: &DistroBinding,
    plan: SandboxPlan,
    cmdline: &str,
) -> Result<PreparedLaunch> {
    let distro = binding.check(launch.distro(), DISTRO_POLICY_REVISION)?;
    let scratch = match launch.scratch() {
        Some(scratch) => Some(
            ScratchBinding::bind(scratch)?.check(scratch, Instant::now() + CLAIM_RELEASE_WAIT)?,
        ),
        None => None,
    };
    PreparedLaunch::one_shot(plan, distro, scratch, cmdline).map_err(Exit::from)
}

#[cfg(test)]
mod tests;
