//! [`ConfigError`]: an error of the checks of a VM's configuration and of the values in it.

use std::fmt;

/// What a [`ConfigError`] found wrong with a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConfigErrorKind {
    /// The value breaks its own rules: it is malformed, such as a label with a space or an address
    /// that is not of its form, it lies outside its range, or it combines fields that cannot go
    /// together.
    Invalid,
    /// The value is well formed, but NVX cannot represent or enforce it, such as more egress rules
    /// than OpenVMM takes, or a command line longer than its limit.
    Unsupported,
}

/// An error of the checks of a VM's configuration, or of a value in it, which read nothing on the
/// host: its [`kind`](Self::kind), a message, which it displays, and an optional source.
///
/// The message is a complete sentence fragment, without a trailing period, which callers can show
/// as is.
#[derive(Debug)]
pub struct ConfigError {
    /// What the check found wrong.
    kind: ConfigErrorKind,
    /// Complete, user-facing description without a trailing period.
    message: String,
    /// Underlying error, when retaining it aids diagnostics.
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl ConfigError {
    /// Creates an error of `kind`, which `message` describes.
    pub fn new(kind: ConfigErrorKind, message: impl Into<String>) -> Self {
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

    /// Returns what the check found wrong.
    pub fn kind(&self) -> ConfigErrorKind {
        self.kind
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests;
