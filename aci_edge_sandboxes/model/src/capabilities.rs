//! The features a backend honors, which double as its policy honor matrix.

use serde::{Deserialize, Serialize};

/// Features a backend can honor.
///
/// `AciEdgeSandbox` rejects requests that use unsupported features with
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
    /// Sandbox spec features.
    #[serde(default)]
    pub spec: SpecCapabilities,
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
    /// Cancels a live execution through its `Canceller`.
    pub cancel: bool,
    /// Honors `process.cwd`.
    pub cwd: bool,
    /// Honors `process.env`: layers its entries over the default environment when
    /// `inheritDefaultEnv` is true, and replaces the default environment otherwise if
    /// [`clear_default_env`](Self::clear_default_env) is set too.
    pub env: bool,
    /// Honors replacing the default environment with `process.env`, including an empty list,
    /// when `inheritDefaultEnv` is omitted or false.
    pub clear_default_env: bool,
    /// Runs multiple executions against one sandbox simultaneously instead of serializing them.
    pub concurrent: bool,
    /// Largest accepted `process.timeout`, in milliseconds. `None` leaves only the contract's
    /// limit, [`ProcessSpec::MAX_TIMEOUT_MS`](crate::ProcessSpec::MAX_TIMEOUT_MS).
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
    /// Honors `network.ingress.hostLoopback: allow` without forwarded ports.
    pub host_loopback_allow: bool,
    /// Honors `network.ingress.hostLoopback: deny`.
    pub host_loopback_deny: bool,
    /// Honors `network.egress.allow` and `network.egress.deny` rules.
    pub egress_rules: bool,
    /// Honors `runtimeConfig.networkProxy`.
    #[serde(default)]
    pub network_proxy: bool,
}

/// Sandbox spec fields a backend can honor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SpecCapabilities {
    /// Honors `image.path`: a local GPT disk image.
    pub image_path: bool,
    /// Honors `image.digest`: an image registered with the backend.
    pub image_digest: bool,
    /// Honors `image.reference`: a container image reference.
    pub image_reference: bool,
    /// Honors `resources.vcpus`.
    pub vcpus: bool,
    /// Honors `resources.memoryMib`.
    pub memory: bool,
    /// Honors `guestNetwork`.
    pub guest_network: bool,
    /// Honors `hostname`.
    pub hostname: bool,
    /// Honors `hostLoopbackForwards`, with `network.ingress.hostLoopback: allow`.
    pub host_loopback_forwards: bool,
}

/// Host filesystem mappings a backend can honor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct FilesystemCapabilities {
    /// Honors `filesystem.enumeratePaths` without granting content reads.
    #[serde(default)]
    pub enumerate_paths: bool,
    /// Honors `filesystem.readonlyPaths`.
    pub readonly_paths: bool,
    /// Honors `filesystem.readwritePaths`.
    pub readwrite_paths: bool,
    /// Honors `filesystem.deniedPaths`.
    pub denied_paths: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_without_the_proxy_or_spec_fields_leave_them_unsupported() {
        let mut capabilities = Capabilities::new("older");
        capabilities.exec.argv = true;
        capabilities.network.egress_deny = true;
        let mut payload = serde_json::to_value(&capabilities).unwrap();
        payload["network"]
            .as_object_mut()
            .unwrap()
            .remove("networkProxy")
            .unwrap();
        payload.as_object_mut().unwrap().remove("spec").unwrap();
        assert_eq!(
            serde_json::from_value::<Capabilities>(payload).unwrap(),
            capabilities
        );
    }

    #[test]
    fn an_older_agent_does_not_advertise_enumeration() {
        let capabilities = Capabilities::new("older");
        let mut payload = serde_json::to_value(&capabilities).unwrap();
        payload["filesystem"]
            .as_object_mut()
            .unwrap()
            .remove("enumeratePaths");
        let decoded: Capabilities = serde_json::from_value(payload).unwrap();
        assert!(!decoded.filesystem.enumerate_paths);
    }
}
