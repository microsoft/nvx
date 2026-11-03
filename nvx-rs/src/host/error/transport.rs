//! [`TransportError`]: an error of the connection to a VM's control endpoint, or of the session
//! over it.

use std::fmt;

/// What ended a [`TransportError`]'s connection or operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TransportErrorKind {
    /// A deadline passed, such as that of a connection while the endpoint does not exist yet, or
    /// that of a read.
    TimedOut,
    /// The VM's process exited, so it will not serve its endpoint.
    Exited,
    /// The endpoint refused the host: a process other than the VM's serves it, or the session
    /// refused the host's capability.
    Refused,
    /// The peer closed the stream, as OpenVMM does when the VM shuts down.
    Closed,
    /// The session was reset, which ended the host's attachment to it.
    Reset,
    /// The peer broke the protocol, or the stream ended inside a record.
    Protocol,
    /// The transport failed.
    Io,
}

/// An error of the connection to a VM's control endpoint, or of the session over it: its
/// [`kind`](Self::kind), a message, which it displays, and an optional source.
///
/// The message is a complete sentence fragment, without a trailing period, which callers can show
/// as is.
#[derive(Debug)]
pub struct TransportError {
    /// What ended the connection or the operation.
    kind: TransportErrorKind,
    /// Complete, user-facing description without a trailing period.
    message: String,
    /// Underlying error, when retaining it aids diagnostics.
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl TransportError {
    /// Creates an error of `kind`, which `message` describes.
    pub fn new(kind: TransportErrorKind, message: impl Into<String>) -> Self {
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

    /// Returns what ended the connection or the operation.
    pub fn kind(&self) -> TransportErrorKind {
        self.kind
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests;
