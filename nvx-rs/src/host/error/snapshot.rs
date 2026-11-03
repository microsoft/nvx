//! [`SnapshotError`]: an error of a snapshot's capture, of its opening, or of its acceptance of a
//! VM.

use std::fmt;

/// What a [`SnapshotError`] found wrong with a snapshot or its capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SnapshotErrorKind {
    /// A host I/O operation failed, such as a read or a write of the snapshot's files, or a read of
    /// the host's clock.
    Io,
    /// The snapshot is not in its strict format: an entry is missing, extra, not a regular file, or
    /// not private, a length differs from its manifest's, or the manifest or the key is malformed
    /// or does not match the other.
    Malformed,
    /// The snapshot cannot restore the VM here: the VM's configuration, the runtime, the host, or
    /// the time is not one that the snapshot accepts.
    Incompatible,
    /// The capture produced no snapshot: its VM did not end with a successful capture.
    CaptureFailed,
}

/// An error of a snapshot's capture, of its opening, or of its acceptance of a VM: its
/// [`kind`](Self::kind), a message, which it displays, and an optional source.
///
/// The message is a complete sentence fragment, without a trailing period, which callers can show
/// as is.
#[derive(Debug)]
pub struct SnapshotError {
    /// What was found wrong.
    kind: SnapshotErrorKind,
    /// Complete, user-facing description without a trailing period.
    message: String,
    /// Underlying error, when retaining it aids diagnostics.
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl SnapshotError {
    /// Creates an error of `kind`, which `message` describes.
    pub fn new(kind: SnapshotErrorKind, message: impl Into<String>) -> Self {
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

    /// Returns what was found wrong.
    pub fn kind(&self) -> SnapshotErrorKind {
        self.kind
    }
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests;
