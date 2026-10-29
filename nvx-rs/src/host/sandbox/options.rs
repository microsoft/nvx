//! The contracts that `nvx-rs sandbox` and `nvx-rs run` apply to their options:
//!
//! - [`SandboxOptions::check`]: the checks of `nvx-rs sandbox` before it builds the launch: the
//!   operations that each option is valid for, the options that need others, and the managed exec
//!   environment, from `--environment` or `--environment-file`.
//! - [`sandbox_mounts`]: the shares of the `--mount` options, with each `--mount-deny`,
//!   `--mount-allow`, and `--mount-write` attributed to its share, and each share's kind.
//! - [`validate_sandbox_entrypoint`]: the refusal of systemd entrypoints, and
//!   [`validate_sandbox_systemd_policy`]: the preflight of `run` and `provision`, which also
//!   verifies the distro layer and returns its [`DistroBinding`].
//! - [`validate_run_share_targets`]: the guest targets of several `nvx-rs run` shares, or of a
//!   shared file.
//! - [`validate_exec_environment`] and [`exec_environment_from_file`]: the environment of a
//!   managed exec.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use super::distro::DistroBinding;
use super::mount::{
    AGGREGATE_MOUNT_TARGET, MountKind, MountOwner, MountPolicy, POLICY_KINDS, PolicyKind,
    SandboxMount, has_forbidden_character, has_irregular_component, mount_kind, targets_overlap,
};
use super::{SandboxLaunch, refused};
use crate::error::Result;
use crate::protocol::root::RootUpper;

/// The entrypoints of systemd, which the sandbox security profile does not run.
pub const SYSTEMD_ENTRYPOINTS: [&str; 2] = ["/usr/lib/systemd/systemd", "/lib/systemd/systemd"];
/// The largest environment file of a managed exec, in bytes.
pub const MAX_ENVIRONMENT_FILE_BYTES: usize = 1024 * 1024;
/// The most entries of a managed exec environment.
pub const MAX_EXEC_ENVIRONMENT: usize = 256;
/// The longest entry of a managed exec environment, in UTF-8 bytes.
pub const MAX_EXEC_ENVIRONMENT_BYTES: usize = 4096;

/// An operation of `nvx-rs sandbox`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SandboxOperation {
    /// `run`: a one-shot sandbox, the default.
    Run,
    /// `provision`: record a managed sandbox's launch in its state directory.
    Provision,
    /// `start`: start a provisioned sandbox.
    Start,
    /// `exec`: run a workload in a started sandbox.
    Exec,
    /// `stop`: stop a started sandbox.
    Stop,
    /// `deprovision`: remove a sandbox's state directory.
    Deprovision,
}

impl SandboxOperation {
    /// Returns the operation's name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Provision => "provision",
            Self::Start => "start",
            Self::Exec => "exec",
            Self::Stop => "stop",
            Self::Deprovision => "deprovision",
        }
    }

    /// Returns whether the operation launches a sandbox: `run` or `provision`.
    fn launches(self) -> bool {
        matches!(self, Self::Run | Self::Provision)
    }
}

impl fmt::Display for SandboxOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A `--mount-deny`, `--mount-allow`, or `--mount-write` option, with the position among the
/// `--mount` options of the one before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyPathOption {
    /// The kind of path, which the option gives.
    pub kind: PolicyKind,
    /// The index of the last `--mount` option before this one, if any.
    pub preceding_mount: Option<usize>,
    /// The host path, as given.
    pub path: String,
}

/// The options of `nvx-rs sandbox` that [`SandboxOptions::check`] examines, as given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxOptions {
    /// The operation.
    pub operation: SandboxOperation,
    /// `--dry-run`.
    pub dry_run: bool,
    /// `--cwd`.
    pub cwd: Option<String>,
    /// The `--environment` options.
    pub environment: Vec<String>,
    /// `--environment-file`.
    pub environment_file: Option<String>,
    /// `--inherit-default-environment`.
    pub inherit_default_environment: bool,
    /// Whether any network policy option was given: `--net`, `--network-profile`,
    /// `--network-egress`, `--network-ingress`, `--network-egress-policy-file`,
    /// `--host-loopback`, `--network-proxy`, `--network-egress-allow`,
    /// `--network-egress-deny`, or `--host-loopback-forward`.
    pub network_options: bool,
    /// Whether `--net` was given.
    pub net: bool,
    /// Whether `--network-profile` was given.
    pub network_profile: bool,
    /// `--entrypoint`.
    pub entrypoint: String,
    /// Whether `--outcome-report` was given.
    pub outcome_report: bool,
    /// The `--mount` options.
    pub mounts: Vec<String>,
    /// The `--mount-deny`, `--mount-allow`, and `--mount-write` options, in order.
    pub policy_paths: Vec<PolicyPathOption>,
    /// `--mount-owner`.
    pub mount_owner: Option<MountOwner>,
    /// Whether any `--layer` was given.
    pub layers: bool,
    /// Whether `--scratch` was given.
    pub scratch: bool,
    /// `--upper`, if given: a launch keeps its upper on scratch without it.
    pub upper: Option<RootUpper>,
}

impl SandboxOptions {
    /// Returns the options of an operation with the defaults of `nvx-rs sandbox`.
    pub fn new(operation: SandboxOperation) -> Self {
        Self {
            operation,
            dry_run: false,
            cwd: None,
            environment: Vec::new(),
            environment_file: None,
            inherit_default_environment: false,
            network_options: false,
            net: false,
            network_profile: false,
            entrypoint: super::DEFAULT_ENTRYPOINT.to_owned(),
            outcome_report: false,
            mounts: Vec::new(),
            policy_paths: Vec::new(),
            mount_owner: None,
            layers: false,
            scratch: false,
            upper: None,
        }
    }

    /// Applies the checks of `nvx-rs sandbox` that precede the launch, in its order, and
    /// returns the managed exec environment of an `exec`, if one was given.
    ///
    /// It refuses `--dry-run` except for `run`, both `--environment` and `--environment-file`,
    /// the managed exec options except for `exec`, an `exec` environment that
    /// [`exec_environment_from_file`] or [`validate_exec_environment`] refuses, network policy
    /// options except for `run` and `provision`, a systemd entrypoint for `run`, `provision`,
    /// and `exec`, `--outcome-report` except for `run` and `exec`, share options without a
    /// `--mount`, `--mount` and `--upper` except for `run` and `provision`, and for `run` and
    /// `provision`, `--net` without `--network-profile` or the reverse, a missing `--layer`, and
    /// a missing `--scratch` for an upper on scratch, or one given for `--upper ram`, whose upper
    /// is in RAM.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
    pub fn check(&self) -> Result<Option<Vec<String>>> {
        let operation = self.operation;
        if self.dry_run && operation != SandboxOperation::Run {
            return Err(refused("--dry-run is only valid for sandbox run"));
        }
        if !self.environment.is_empty() && self.environment_file.is_some() {
            return Err(refused(
                "--environment and --environment-file are mutually exclusive",
            ));
        }
        if operation != SandboxOperation::Exec
            && (self.cwd.is_some()
                || !self.environment.is_empty()
                || self.environment_file.is_some()
                || self.inherit_default_environment)
        {
            return Err(refused(
                "managed execution options require the sandbox exec operation",
            ));
        }
        let mut environment = None;
        if operation == SandboxOperation::Exec {
            if let Some(file) = &self.environment_file {
                environment = Some(exec_environment_from_file(file)?);
            } else if !self.environment.is_empty() {
                validate_exec_environment(&self.environment)?;
                environment = Some(self.environment.clone());
            }
        }
        if !operation.launches() && self.network_options {
            return Err(refused(
                "network policy options are only valid for sandbox run or provision",
            ));
        }
        if matches!(
            operation,
            SandboxOperation::Run | SandboxOperation::Provision | SandboxOperation::Exec
        ) {
            validate_sandbox_entrypoint(&self.entrypoint)?;
        }
        if self.outcome_report
            && !matches!(operation, SandboxOperation::Run | SandboxOperation::Exec)
        {
            return Err(refused(
                "--outcome-report is only valid for one-shot run or managed exec",
            ));
        }
        for kind in POLICY_KINDS {
            if self.mounts.is_empty() && self.policy_paths.iter().any(|path| path.kind == kind) {
                return Err(refused(format!("{} requires --mount", kind.option())));
            }
        }
        if self.mount_owner.is_some() && self.mounts.is_empty() {
            return Err(refused("--mount-owner requires --mount"));
        }
        if !self.mounts.is_empty() && !operation.launches() {
            return Err(refused(
                "--mount is only valid for sandbox run or provision",
            ));
        }
        if self.upper.is_some() && !operation.launches() {
            return Err(refused(
                "--upper is only valid for sandbox run or provision",
            ));
        }
        if operation.launches() {
            if self.net != self.network_profile {
                return Err(refused(
                    "--net and --network-profile must be specified together",
                ));
            }
            match self.upper.unwrap_or(RootUpper::Scratch) {
                RootUpper::Scratch if !self.layers || !self.scratch => {
                    return Err(refused(format!(
                        "sandbox {operation} requires --layer and --scratch"
                    )));
                }
                RootUpper::Ram if self.scratch => {
                    return Err(refused(
                        "--upper ram and --scratch are mutually exclusive: an upper in RAM has \
                         no scratch image",
                    ));
                }
                RootUpper::Ram if !self.layers => {
                    return Err(refused(format!("sandbox {operation} requires --layer")));
                }
                RootUpper::Scratch | RootUpper::Ram => {}
            }
        }
        Ok(environment)
    }
}

/// Returns the shares of the `--mount` values in `mounts`, each with its access-policy paths, the
/// `owner` (`vmm` by default), and the kind that [`mount_kind`] finds for its host path.
///
/// With one `--mount`, every policy path names a path in it. With several, each names a path in
/// the `--mount` before it. Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported)
/// for a policy path without a `--mount` before it among several, and as
/// [`SandboxMount::parse`] and [`SandboxMount::with_kind`] do.
pub fn sandbox_mounts(
    mounts: &[String],
    policy_paths: &[PolicyPathOption],
    owner: Option<MountOwner>,
) -> Result<Vec<SandboxMount>> {
    sandbox_mounts_with(mounts, policy_paths, owner, &|mount| {
        mount_kind(mount.host_path())
    })
}

/// Implements [`sandbox_mounts`] with `kind_of` as [`mount_kind`].
pub(crate) fn sandbox_mounts_with(
    mounts: &[String],
    policy_paths: &[PolicyPathOption],
    owner: Option<MountOwner>,
    kind_of: &dyn Fn(&SandboxMount) -> MountKind,
) -> Result<Vec<SandboxMount>> {
    let mut policies = vec![MountPolicy::default(); mounts.len()];
    for kind in POLICY_KINDS {
        for option in policy_paths.iter().filter(|option| option.kind == kind) {
            let index = match (mounts.len(), option.preceding_mount) {
                (1, _) => 0,
                (_, Some(index)) => index,
                (_, None) => {
                    return Err(refused(format!(
                        "with several --mount options, each {} must follow the --mount whose \
                         host directory it names",
                        kind.option()
                    )));
                }
            };
            let policy = policies.get_mut(index).ok_or_else(|| {
                refused(format!(
                    "{} follows --mount number {}, which is missing",
                    kind.option(),
                    index + 1
                ))
            })?;
            match kind {
                PolicyKind::Denied => &mut policy.denied,
                PolicyKind::Allowed => &mut policy.allowed,
                PolicyKind::Writable => &mut policy.writable,
            }
            .push(option.path.clone());
        }
    }
    let owner = owner.unwrap_or(MountOwner::Vmm);
    mounts
        .iter()
        .zip(policies)
        .map(|(value, policy)| {
            let mount = SandboxMount::parse(value, policy, owner, MountKind::Directory)?;
            mount.with_kind(kind_of(&mount))
        })
        .collect()
}

/// Refuses the guest targets of `nvx-rs run` shares that the guest cannot bind from an
/// aggregate: targets that would not survive the kernel command line, or that hide each other or
/// the aggregate itself. `nvx-rs run` checks them when it attaches several shares, or a file.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn validate_run_share_targets(targets: &[&str]) -> Result<()> {
    for (index, target) in targets.iter().enumerate() {
        if !target.starts_with('/')
            || *target == "/"
            || has_forbidden_character(target)
            || has_irregular_component(target)
        {
            return Err(refused(format!(
                "invalid --mount target {target:?}: with several shares, or a shared file, each target \
                 must be an absolute non-root path without empty, dot, or parent components, \
                 whitespace, control characters, quotes, '\\', '=', or ','",
            )));
        }
        for other in std::iter::once(&AGGREGATE_MOUNT_TARGET).chain(&targets[..index]) {
            if targets_overlap(target, other) {
                return Err(refused(format!(
                    "--mount targets {other} and {target} overlap; a share cannot hide another"
                )));
            }
        }
    }
    Ok(())
}

/// Refuses a systemd entrypoint, which the sandbox security profile does not run.
///
/// This is the cheap check of every launch, which reads nothing: it applies whether or not the
/// distro layer has a manifest, and never reads the manifest's package list. Fails with
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn validate_sandbox_entrypoint(entrypoint: &str) -> Result<()> {
    if SYSTEMD_ENTRYPOINTS.contains(&entrypoint) {
        return Err(refused(
            "systemd entrypoints are unsupported by the sandbox security profile",
        ));
    }
    Ok(())
}

/// Refuses a launch that would run systemd, and returns the binding of its distro layer: the
/// preflight of `nvx-rs sandbox run` and `provision`.
///
/// It refuses a systemd entrypoint with [`validate_sandbox_entrypoint`], and then verifies the
/// distro layer explicitly with [`DistroBinding::verify`], which refuses a layer whose manifest,
/// `LAYER.manifest.json` next to it, lists the `systemd` package, and hashes the layer only when
/// that manifest exists. Fails as they do.
pub fn validate_sandbox_systemd_policy(launch: &SandboxLaunch) -> Result<DistroBinding> {
    validate_sandbox_entrypoint(&launch.settings().entrypoint)?;
    DistroBinding::verify(launch.distro())
}

/// Checks a managed exec environment, as `encode_exec_environment` does: at most
/// [`MAX_EXEC_ENVIRONMENT`] entries, each a `KEY=VALUE` string with a nonempty key, without NUL,
/// of at most [`MAX_EXEC_ENVIRONMENT_BYTES`] bytes, and no key twice.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn validate_exec_environment(entries: &[String]) -> Result<()> {
    if entries.len() > MAX_EXEC_ENVIRONMENT {
        return Err(refused(format!(
            "managed exec environment exceeds {MAX_EXEC_ENVIRONMENT} entries"
        )));
    }
    let mut names = std::collections::HashSet::new();
    for entry in entries {
        let (name, separator) = match entry.split_once('=') {
            Some((name, _)) => (name, true),
            None => (entry.as_str(), false),
        };
        if !separator
            || name.is_empty()
            || entry.contains('\0')
            || entry.len() > MAX_EXEC_ENVIRONMENT_BYTES
        {
            return Err(refused(format!(
                "managed exec environment entries must be non-empty KEY=VALUE strings of at most \
                 {MAX_EXEC_ENVIRONMENT_BYTES} bytes"
            )));
        }
        if !names.insert(name) {
            return Err(refused(format!(
                "managed exec environment contains duplicate key: {name}"
            )));
        }
    }
    Ok(())
}

/// Reads the managed exec environment of `--environment-file`: a JSON array of `KEY=VALUE`
/// strings of at most [`MAX_ENVIRONMENT_FILE_BYTES`] bytes, which must then pass
/// [`validate_exec_environment`].
///
/// The file must be strict UTF-8 JSON; non-finite numbers and unpaired surrogate escapes are
/// refused. Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), naming the
/// file and the native I/O or JSON error when it cannot be read or decoded.
pub fn exec_environment_from_file(path: &str) -> Result<Vec<String>> {
    let path = Path::new(path);
    let mut data = Vec::new();
    File::open(path)
        .and_then(|file| {
            file.take(MAX_ENVIRONMENT_FILE_BYTES as u64 + 1)
                .read_to_end(&mut data)
        })
        .map_err(|error| {
            refused(format!(
                "failed to read managed execution environment {path:?}: {error}"
            ))
        })?;
    if data.len() > MAX_ENVIRONMENT_FILE_BYTES {
        return Err(refused(format!(
            "managed execution environment file exceeds {MAX_ENVIRONMENT_FILE_BYTES}-byte limit"
        )));
    }
    let entries: Vec<String> = serde_json::from_slice(&data).map_err(|error| {
        refused(format!(
            "invalid managed execution environment {path:?}: {error}"
        ))
    })?;
    validate_exec_environment(&entries)?;
    Ok(entries)
}

#[cfg(test)]
mod tests;
