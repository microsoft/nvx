//! Errors of NVX operations, their classification, and their stable codes.

use std::fmt;

/// Result type of NVX operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Classification of an [`Error`], which its [`ErrorCode`] determines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// A runtime dependency is missing or unusable: an artifact, the hypervisor, a release or
    /// guest image without the control contract or features that this crate needs, or a host
    /// that NVX does not support.
    Unavailable,
    /// NVX refuses the request: a value that it does not accept, such as a malformed or
    /// out-of-range one, options that it cannot combine, or a request that it cannot represent or
    /// enforce, for example because it exceeds a limit of OpenVMM or the guest agent.
    Unsupported,
    /// The OpenVMM process of a VM exited.
    Exited,
    /// The exit of a VM's process could not be confirmed; it may still run.
    StillRunning,
    /// A host I/O operation failed, or state that NVX recorded on the host is unreadable or
    /// malformed.
    Io,
    /// The state that an operation names does not exist on the host, such as the store of a
    /// sandbox that was never created or that a removal deleted.
    NotFound,
    /// The control session failed: the peer violated the protocol, closed or reset the session,
    /// or refused a request.
    Protocol,
    /// A deadline passed.
    TimedOut,
    /// A VM runs under another runtime identity than the artifacts', so NVX does not drive it:
    /// only a stop, which terminates it, applies.
    RuntimeMismatch,
    /// Any other failure.
    Other,
}

impl ErrorKind {
    /// Returns the lowercase name of this kind.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Unsupported => "unsupported",
            Self::Exited => "exited",
            Self::StillRunning => "still-running",
            Self::Io => "io",
            Self::NotFound => "not-found",
            Self::Protocol => "protocol",
            Self::TimedOut => "timed-out",
            Self::RuntimeMismatch => "runtime-mismatch",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable, machine-readable code of an [`Error`], finer than its [`ErrorKind`].
///
/// Each code belongs to one kind, which [`kind`](Self::kind) returns. A failure that a caller can
/// act on has a code of its own, such as `sandbox-not-running`; any other failure has its kind's
/// code, which is spelled as the kind, such as `io`, and which [`ErrorKind`] converts into.
///
/// [`as_str`](Self::as_str) spells a code in lowercase ASCII words joined by hyphens: the name of
/// a kind, or the subject of the failure followed by its condition, such as `artifact-missing`.
/// Codes are stable: a code keeps its spelling, its kind, and its meaning, and is never reused for
/// another failure, so a new failure that a caller can act on gets a new code. Messages are not
/// stable, so callers tell errors apart by their codes, and by stable fragments of their messages,
/// such as the names of options and fields.
///
/// The `nvx-rs` CLI prints the code of every error that it reports, including the `argument-*`
/// codes of its usage errors, which no library error has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    /// `unavailable`: a runtime dependency is missing or unusable, other than as a finer code
    /// says, such as install metadata, the hypervisor, a release, or a host that NVX does not
    /// support.
    Unavailable,
    /// `unsupported`: NVX refuses the request, such as options that it cannot combine, a value
    /// that it does not accept, or a policy that it cannot represent or enforce. The message names
    /// the option or field.
    Unsupported,
    /// `exited`: the OpenVMM process of a VM exited, as when a start fails.
    Exited,
    /// `still-running`: the exit of a VM's process could not be confirmed.
    StillRunning,
    /// `io`: a host I/O operation failed, or state that NVX recorded on the host is unreadable or
    /// malformed.
    Io,
    /// `not-found`: state that the operation names does not exist on the host.
    NotFound,
    /// `protocol`: the control session failed: the peer violated the protocol, reset the session,
    /// or failed authentication.
    Protocol,
    /// `timed-out`: a deadline passed.
    TimedOut,
    /// `other`: any other failure.
    Other,
    /// `argument-unrecognized`: the command line holds arguments that the command does not take,
    /// such as an unknown option. A usage error of the CLI.
    ArgumentUnrecognized,
    /// `argument-missing`: the command line lacks a required argument, or the value of an option.
    /// A usage error of the CLI.
    ArgumentMissing,
    /// `argument-invalid`: an argument has an invalid value, such as a value that is not one of
    /// its choices, not a number, not of its form, or not valid Unicode. A usage error of the CLI.
    ArgumentInvalid,
    /// `artifact-missing`: an artifact that the operation runs, such as OpenVMM, the guest kernel,
    /// or an initramfs, does not exist, or no artifact root holds them.
    ArtifactMissing,
    /// `guest-features-missing`: the guest image lacks control features that the operation needs.
    GuestFeaturesMissing,
    /// `sandbox-not-provisioned`: no sandbox is provisioned at its state directory, which does not
    /// exist, is not a plain directory, or holds no configuration.
    SandboxNotProvisioned,
    /// `sandbox-provisioned`: the sandbox is provisioned already, or was provisioned again while
    /// it was being removed.
    SandboxProvisioned,
    /// `sandbox-not-running`: the operation needs the sandbox's VM, which is not running.
    SandboxNotRunning,
    /// `sandbox-running`: the operation needs a stopped sandbox, but its VM, or a launch of it,
    /// may still run.
    SandboxRunning,
    /// `sandbox-exited`: the sandbox's VM exited without a stop. A start launches it again.
    SandboxExited,
    /// `sandbox-busy`: another lifecycle transition, or exec, of the sandbox held its lock until
    /// the deadline passed.
    SandboxBusy,
    /// `runtime-mismatch`: the sandbox's VM runs under another runtime identity than the
    /// artifacts', so it is not driven, and only a stop, which terminates it, applies.
    RuntimeMismatch,
    /// `vmm-indeterminate`: whether the OpenVMM process of the sandbox runs cannot be determined.
    VmmIndeterminate,
    /// `format-unsupported`: a record or report that NVX reads has a format or schema version that
    /// this build does not read, such as the state of a sandbox that another build provisioned or
    /// started.
    FormatUnsupported,
    /// `foreign-files`: a directory of NVX's state, such as a sandbox's state directory or a
    /// template store, holds files that NVX does not own, which the message names, and which NVX
    /// keeps.
    ForeignFiles,
    /// `outcome-report-exists`: a file exists at the path of the outcome report to write.
    OutcomeReportExists,
    /// `scratch-claimed`: another sandbox claims the scratch image.
    ScratchClaimed,
    /// `exec-refused`: the guest agent refused the workload, as for a working directory that it
    /// cannot use.
    ExecRefused,
    /// `endpoint-closed`: OpenVMM closed the control endpoint, as it does when the VM shuts down.
    EndpointClosed,
    /// `template-ineligible`: the restore window of a template's generation, which the time
    /// contract of its capture sets, excludes the host's time, so no clone may restore it now: its
    /// capture is too old, or the host's clock is behind the capture's.
    TemplateIneligible,
    /// `template-tombstoned`: a tombstone quarantines the template's generation, which takes no
    /// new clones, while the clones that lease it keep their leases.
    TemplateTombstoned,
    /// `sandbox-leased`: the sandbox holds a lease of a template's generation, which allows no
    /// other lease, and one launch of its clone, until a release ends it once its VM has exited.
    SandboxLeased,
}

impl ErrorCode {
    /// Every code of this release.
    pub const ALL: &'static [ErrorCode] = &[
        Self::Unavailable,
        Self::Unsupported,
        Self::Exited,
        Self::StillRunning,
        Self::Io,
        Self::NotFound,
        Self::Protocol,
        Self::TimedOut,
        Self::Other,
        Self::ArgumentUnrecognized,
        Self::ArgumentMissing,
        Self::ArgumentInvalid,
        Self::ArtifactMissing,
        Self::GuestFeaturesMissing,
        Self::SandboxNotProvisioned,
        Self::SandboxProvisioned,
        Self::SandboxNotRunning,
        Self::SandboxRunning,
        Self::SandboxExited,
        Self::SandboxBusy,
        Self::RuntimeMismatch,
        Self::VmmIndeterminate,
        Self::FormatUnsupported,
        Self::ForeignFiles,
        Self::OutcomeReportExists,
        Self::ScratchClaimed,
        Self::ExecRefused,
        Self::EndpointClosed,
        Self::TemplateIneligible,
        Self::TemplateTombstoned,
        Self::SandboxLeased,
    ];

    /// Returns the stable spelling of this code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => ErrorKind::Unavailable.as_str(),
            Self::Unsupported => ErrorKind::Unsupported.as_str(),
            Self::Exited => ErrorKind::Exited.as_str(),
            Self::StillRunning => ErrorKind::StillRunning.as_str(),
            Self::Io => ErrorKind::Io.as_str(),
            Self::NotFound => ErrorKind::NotFound.as_str(),
            Self::Protocol => ErrorKind::Protocol.as_str(),
            Self::TimedOut => ErrorKind::TimedOut.as_str(),
            Self::Other => ErrorKind::Other.as_str(),
            Self::ArgumentUnrecognized => "argument-unrecognized",
            Self::ArgumentMissing => "argument-missing",
            Self::ArgumentInvalid => "argument-invalid",
            Self::ArtifactMissing => "artifact-missing",
            Self::GuestFeaturesMissing => "guest-features-missing",
            Self::SandboxNotProvisioned => "sandbox-not-provisioned",
            Self::SandboxProvisioned => "sandbox-provisioned",
            Self::SandboxNotRunning => "sandbox-not-running",
            Self::SandboxRunning => "sandbox-running",
            Self::SandboxExited => "sandbox-exited",
            Self::SandboxBusy => "sandbox-busy",
            Self::RuntimeMismatch => ErrorKind::RuntimeMismatch.as_str(),
            Self::VmmIndeterminate => "vmm-indeterminate",
            Self::FormatUnsupported => "format-unsupported",
            Self::ForeignFiles => "foreign-files",
            Self::OutcomeReportExists => "outcome-report-exists",
            Self::ScratchClaimed => "scratch-claimed",
            Self::ExecRefused => "exec-refused",
            Self::EndpointClosed => "endpoint-closed",
            Self::TemplateIneligible => "template-ineligible",
            Self::TemplateTombstoned => "template-tombstoned",
            Self::SandboxLeased => "sandbox-leased",
        }
    }

    /// Returns the kind of the errors that have this code.
    pub const fn kind(self) -> ErrorKind {
        match self {
            Self::Unavailable
            | Self::ArtifactMissing
            | Self::GuestFeaturesMissing
            | Self::TemplateIneligible
            | Self::TemplateTombstoned => ErrorKind::Unavailable,
            Self::Unsupported
            | Self::ArgumentUnrecognized
            | Self::ArgumentMissing
            | Self::ArgumentInvalid => ErrorKind::Unsupported,
            Self::Exited | Self::SandboxExited => ErrorKind::Exited,
            Self::StillRunning => ErrorKind::StillRunning,
            Self::Io | Self::FormatUnsupported => ErrorKind::Io,
            Self::NotFound | Self::SandboxNotProvisioned => ErrorKind::NotFound,
            Self::Protocol | Self::ExecRefused | Self::EndpointClosed => ErrorKind::Protocol,
            Self::TimedOut | Self::SandboxBusy | Self::ScratchClaimed => ErrorKind::TimedOut,
            Self::RuntimeMismatch => ErrorKind::RuntimeMismatch,
            Self::Other
            | Self::SandboxProvisioned
            | Self::SandboxNotRunning
            | Self::SandboxRunning
            | Self::VmmIndeterminate
            | Self::ForeignFiles
            | Self::OutcomeReportExists
            | Self::SandboxLeased => ErrorKind::Other,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Returns the code of a failure of `kind` that has no finer code.
impl From<ErrorKind> for ErrorCode {
    fn from(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::Unavailable => Self::Unavailable,
            ErrorKind::Unsupported => Self::Unsupported,
            ErrorKind::Exited => Self::Exited,
            ErrorKind::StillRunning => Self::StillRunning,
            ErrorKind::Io => Self::Io,
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::Protocol => Self::Protocol,
            ErrorKind::TimedOut => Self::TimedOut,
            ErrorKind::RuntimeMismatch => Self::RuntimeMismatch,
            ErrorKind::Other => Self::Other,
        }
    }
}

/// Error of an NVX operation: its [`ErrorCode`], which determines its [`ErrorKind`], a
/// description, and an optional cause.
///
/// The description is a complete sentence fragment that callers can show as is, so callers map
/// errors onto their own classifications by [`kind`](Self::kind) or [`code`](Self::code) alone.
#[derive(Debug)]
pub struct Error {
    /// Stable code that callers use when telling the failure apart.
    code: ErrorCode,
    /// Complete, user-facing description without a trailing period.
    message: String,
    /// Underlying implementation error, when retaining it aids diagnostics.
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl Error {
    /// Creates an error with the given code, or the code of the given kind, and description.
    pub fn new(code: impl Into<ErrorCode>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            source: None,
        }
    }

    /// Creates an [`ErrorKind::Unavailable`] error.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unavailable, message)
    }

    /// Creates an [`ErrorKind::Unsupported`] error.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }

    /// Creates an [`ErrorKind::Exited`] error.
    pub fn exited(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Exited, message)
    }

    /// Creates an [`ErrorKind::Io`] error.
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, message)
    }

    /// Creates an [`ErrorKind::NotFound`] error.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    /// Creates an [`ErrorKind::Protocol`] error.
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Protocol, message)
    }

    /// Creates an [`ErrorKind::TimedOut`] error.
    pub fn timed_out(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::TimedOut, message)
    }

    /// Creates an [`ErrorKind::Other`] error.
    pub fn other(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Other, message)
    }

    /// Attaches an underlying cause to this error.
    #[must_use]
    pub fn with_source(
        mut self,
        source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Returns the classification of this error, which its code determines.
    pub fn kind(&self) -> ErrorKind {
        self.code.kind()
    }

    /// Returns the stable code of this error.
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// Returns the description of this error.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Splits the error into its kind, description, and cause, so that a caller can rebuild it
    /// as an error of its own without adding a level to the chain of causes. A caller that keeps
    /// the code reads it with [`code`](Self::code) first.
    pub fn into_parts(
        self,
    ) -> (
        ErrorKind,
        String,
        Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) {
        (self.code.kind(), self.message, self.source)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests;
