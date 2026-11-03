//! [`RestoreError`]: an error of a VM's restore from a snapshot.

use std::fmt;

/// What ended a restore that failed with a [`RestoreError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RestoreErrorKind {
    /// An operation that the caller performed for the restore failed, such as a connection to the
    /// VM or a read from it.
    OperationFailed,
    /// The restore's deadline passed.
    Expired,
    /// The caller cancelled the restore.
    Cancelled,
    /// The VM's process exited before the restore ended.
    Exited,
    /// OpenVMM or the guest broke the restore's protocol, such as with a Ready that reports another
    /// transaction.
    Protocol,
}

/// An error of a VM's restore from a snapshot: its [`kind`](Self::kind), a message, which it
/// displays, and an optional source.
///
/// The message is a complete sentence fragment, without a trailing period, which callers can show
/// as is.
#[derive(Debug)]
pub struct RestoreError {
    /// What ended the restore.
    kind: RestoreErrorKind,
    /// Complete, user-facing description without a trailing period.
    message: String,
    /// Underlying error, when retaining it aids diagnostics.
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl RestoreError {
    /// Creates an error of `kind`, which `message` describes.
    pub fn new(kind: RestoreErrorKind, message: impl Into<String>) -> Self {
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

    /// Returns what ended the restore.
    pub fn kind(&self) -> RestoreErrorKind {
        self.kind
    }
}

impl fmt::Display for RestoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RestoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests;
