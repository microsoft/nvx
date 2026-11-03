//! [`ReportError`]: an error of the reading of OpenVMM's outcome report of a VM.

use std::fmt;

/// What a [`ReportError`] found wrong with an outcome report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReportErrorKind {
    /// A host I/O operation failed, such as a read of the report.
    Io,
    /// The report is not of the form that OpenVMM writes, such as a file that is not JSON.
    Malformed,
}

/// An error of the reading of OpenVMM's outcome report of a VM: its [`kind`](Self::kind), a
/// message, which it displays, and an optional source.
///
/// The message is a complete sentence fragment, without a trailing period, which callers can show
/// as is.
#[derive(Debug)]
pub struct ReportError {
    /// What was found wrong.
    kind: ReportErrorKind,
    /// Complete, user-facing description without a trailing period.
    message: String,
    /// Underlying error, when retaining it aids diagnostics.
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl ReportError {
    /// Creates an error of `kind`, which `message` describes.
    pub fn new(kind: ReportErrorKind, message: impl Into<String>) -> Self {
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
    pub fn kind(&self) -> ReportErrorKind {
        self.kind
    }
}

impl fmt::Display for ReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests;
