use serde::{Deserialize, Serialize};

/// Features a backend can honor.
///
/// [`AciEdgeSandbox`](crate::AciEdgeSandbox) rejects requests that use unsupported features with
/// [`ErrorCode::PolicyValidation`](crate::ErrorCode::PolicyValidation) before the backend runs
/// anything. The structure doubles as the backend's policy honor matrix.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Capabilities {
    /// Backend name.
    pub backend: String,
    /// Exec features.
    pub exec: ExecCapabilities,
    /// Network posture features.
    pub network: NetworkCapabilities,
    /// Host filesystem mapping features.
    pub filesystem: FilesystemCapabilities,
}

impl Capabilities {
    /// Creates a capability set for `backend` in which every feature is unsupported.
    pub fn new(backend: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            ..Self::default()
        }
    }
}

/// Exec features a backend can honor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ExecCapabilities {
    /// Runs [`Command::CommandLine`](crate::Command::CommandLine).
    pub command_line: bool,
    /// Runs [`Command::Argv`](crate::Command::Argv).
    pub argv: bool,
    /// Streams live standard input ([`StdinMode::Piped`](crate::StdinMode::Piped)).
    pub stdin: bool,
    /// Cancels a live execution through its [`Canceller`](crate::Canceller).
    pub cancel: bool,
    /// Honors `process.cwd`.
    pub cwd: bool,
    /// Honors `process.env`.
    pub env: bool,
    /// Honors `process.inheritDefaultEnv: false`.
    pub clear_default_env: bool,
    /// Runs multiple executions against one sandbox simultaneously instead of serializing them.
    pub concurrent: bool,
    /// Largest accepted `process.timeout`, in milliseconds. `None` means unbounded.
    pub max_timeout_ms: Option<u64>,
    /// Largest combined stdout and stderr volume of one execution, in bytes. `None` means
    /// unbounded.
    pub max_output_bytes: Option<u64>,
}

/// Network postures a backend can honor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct NetworkCapabilities {
    /// Honors `network.egress.default: allow`.
    pub egress_allow: bool,
    /// Honors `network.egress.default: deny`.
    pub egress_deny: bool,
    /// Honors `network.ingress.default: allow`.
    pub ingress_allow: bool,
    /// Honors `network.ingress.default: deny`.
    pub ingress_deny: bool,
    /// Honors `network.ingress.hostLoopback: allow`.
    pub host_loopback_allow: bool,
    /// Honors `network.ingress.hostLoopback: deny`.
    pub host_loopback_deny: bool,
    /// Honors `network.egress.allow` and `network.egress.deny` rules.
    pub egress_rules: bool,
}

/// Host filesystem mappings a backend can honor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct FilesystemCapabilities {
    /// Honors `filesystem.readonlyPaths`.
    pub readonly_paths: bool,
    /// Honors `filesystem.readwritePaths`.
    pub readwrite_paths: bool,
    /// Honors `filesystem.deniedPaths`.
    pub denied_paths: bool,
}
