//! [`LaunchError`]: an error of a VM's launch, stop, verification, or reattachment, or of the probe
//! of its hypervisor.

use std::fmt;

/// What a [`LaunchError`] stopped at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LaunchErrorKind {
    /// A runtime dependency is missing or unusable: the hypervisor, an artifact that the runtime
    /// names, or a host that NVX does not support.
    Unavailable,
    /// The launch refuses its request as given, such as a VM directory that is not private, or a
    /// configuration or a snapshot that it cannot launch.
    Refused,
    /// A host I/O operation failed, such as the creation of the VM's files or the start of its
    /// process.
    Io,
    /// The VM's process exited, as when it fails to start, or when the process that a caller saved
    /// is gone.
    Exited,
    /// The VM runs under another runtime identity than the caller's, so the caller may only stop
    /// it, through its verified process.
    RuntimeMismatch,
    /// The VM's exit was not confirmed by the deadline, so its process may still run.
    StillRunning,
}

/// An error of a VM's launch, stop, verification, or reattachment, or of the probe of its
/// hypervisor: its [`kind`](Self::kind), a message, which it displays, and an optional source.
///
/// The message is a complete sentence fragment, without a trailing period, which callers can show
/// as is.
#[derive(Debug)]
pub struct LaunchError {
    /// What the operation stopped at.
    kind: LaunchErrorKind,
    /// Complete, user-facing description without a trailing period.
    message: String,
    /// Underlying error, when retaining it aids diagnostics.
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl LaunchError {
    /// Creates an error of `kind`, which `message` describes.
    pub fn new(kind: LaunchErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// Attaches the underlying error that caused this one.
    #[must_use]
    pub fn with_source(
        mut self,
        source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Returns what the operation stopped at.
    pub fn kind(&self) -> LaunchErrorKind {
        self.kind
    }
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LaunchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests;
