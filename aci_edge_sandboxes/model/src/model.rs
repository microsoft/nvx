//! Lifecycle requests and results: provision and exec requests, their policies, and the results
//! of each operation.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::id::SandboxId;

/// Backend-defined metadata returned by lifecycle operations.
///
/// Callers must not depend on a metadata field unless the backend documents it.
pub type Metadata = serde_json::Map<String, serde_json::Value>;

/// Inputs to `AciEdgeSandbox::provision`.
///
/// The serialized form matches the policy fields of the contract's provision request
/// (`filesystem`, `network`, `runtimeConfig`, and `microvm`). Envelope fields such as `version`
/// and `phase` belong to the caller's wire layer and are rejected here. Every field is optional,
/// so `{}` provisions a sandbox with the backend defaults.
///
/// The sandbox runs the guest's own Alpine Linux userland directly; there are no image layers or
/// scratch disks. Guest state lives in memory and lasts until the sandbox stops.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProvisionRequest {
    /// Host filesystem mappings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<FilesystemPolicy>,
    /// Network posture. `None` attaches no network device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkPolicy>,
    /// Runtime connectivity. The sandbox takes it at provision because its network is fixed
    /// when the microVM starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_config: Option<RuntimeConfig>,
    /// MicroVM configuration.
    #[serde(default, skip_serializing_if = "MicrovmConfig::is_default")]
    pub microvm: MicrovmConfig,
}

impl ProvisionRequest {
    /// Creates a request that uses the backend defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the network posture.
    #[must_use]
    pub fn with_network(mut self, network: NetworkPolicy) -> Self {
        self.network = Some(network);
        self
    }

    /// Sets the host filesystem mappings.
    #[must_use]
    pub fn with_filesystem(mut self, filesystem: FilesystemPolicy) -> Self {
        self.filesystem = Some(filesystem);
        self
    }

    /// Overrides the backend's default guest memory size.
    #[must_use]
    pub fn with_memory_mib(mut self, memory_mib: u32) -> Self {
        self.microvm.provision.memory_mib = Some(memory_mib);
        self
    }

    /// Routes the guest's traffic through a proxy on host loopback, such as
    /// `http://127.0.0.1:8080`; see [`RuntimeConfig::network_proxy`].
    #[must_use]
    pub fn with_network_proxy(mut self, url: impl Into<String>) -> Self {
        self.runtime_config
            .get_or_insert_with(RuntimeConfig::default)
            .network_proxy = Some(url.into());
        self
    }
}

/// The contract's `runtimeConfig` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeConfig {
    /// An HTTP or HTTPS proxy on host loopback with an explicit port, such as
    /// `http://127.0.0.1:8080`. It must be the guest's only way out: the network's egress default
    /// is `deny`, without allow or deny rules. Workloads receive the proxy variables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_proxy: Option<String>,
}

/// The contract's `microvm` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MicrovmConfig {
    /// Provision-time configuration.
    #[serde(default)]
    pub provision: MicrovmProvision,
}

impl MicrovmConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// The contract's `microvm.provision` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MicrovmProvision {
    /// Guest memory in MiB. This is an ACI Edge Sandboxes extension; `None` selects the backend
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
}

/// A host loopback port forwarded to a guest port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostLoopbackForward {
    /// Transport protocol.
    pub protocol: ForwardProtocol,
    /// Port on host loopback (`127.0.0.1`).
    pub host_port: u16,
    /// Port inside the guest.
    pub guest_port: u16,
}

impl HostLoopbackForward {
    /// Forwards `host_port` on host loopback to `guest_port` in the guest.
    pub fn new(protocol: ForwardProtocol, host_port: u16, guest_port: u16) -> Self {
        Self {
            protocol,
            host_port,
            guest_port,
        }
    }
}

/// Transport protocol of a [`HostLoopbackForward`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardProtocol {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
}

impl ForwardProtocol {
    /// Returns the contract spelling of this protocol.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// The contract's `filesystem` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilesystemPolicy {
    /// Host paths whose names and metadata may be queried recursively, without
    /// reading contents or modifying them. Denied entries stay hidden.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enumerate_paths: Vec<PathBuf>,
    /// Host paths exposed read-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly_paths: Vec<PathBuf>,
    /// Host paths exposed read-write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readwrite_paths: Vec<PathBuf>,
    /// Host paths hidden inside exposed paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_paths: Vec<PathBuf>,
}

impl FilesystemPolicy {
    /// Returns whether the policy requests no mappings at all.
    pub fn is_empty(&self) -> bool {
        self.readonly_paths.is_empty()
            && self.enumerate_paths.is_empty()
            && self.readwrite_paths.is_empty()
            && self.denied_paths.is_empty()
    }
}

/// The contract's `network` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkPolicy {
    /// Guest-initiated traffic.
    pub egress: EgressPolicy,
    /// Host-initiated traffic.
    pub ingress: IngressPolicy,
}

impl NetworkPolicy {
    /// Denies all egress, ingress, and host-loopback traffic.
    pub fn deny_all() -> Self {
        Self::egress(Access::Deny)
    }

    /// Applies `egress` as the default egress policy and denies ingress and host loopback.
    pub fn egress(egress: Access) -> Self {
        Self {
            egress: EgressPolicy::new(egress),
            ingress: IngressPolicy {
                default: Access::Deny,
                host_loopback: Some(Access::Deny),
            },
        }
    }
}

/// The contract's `network.egress` section.
///
/// A connection matching a `deny` rule is blocked. Otherwise a connection matching an `allow`
/// rule is permitted, and any other connection follows `default`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EgressPolicy {
    /// Default policy for guest-initiated connections.
    pub default: Access,
    /// Destinations permitted despite a `deny` default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<NetworkRule>,
    /// Destinations blocked despite an `allow` default or an `allow` rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<NetworkRule>,
}

impl EgressPolicy {
    /// Creates an egress policy with a default and no rules.
    pub fn new(default: Access) -> Self {
        Self {
            default,
            allow: Vec::new(),
            deny: Vec::new(),
        }
    }

    /// Adds a rule that permits matching destinations.
    #[must_use]
    pub fn with_allow(mut self, rule: NetworkRule) -> Self {
        self.allow.push(rule);
        self
    }

    /// Adds a rule that blocks matching destinations.
    #[must_use]
    pub fn with_deny(mut self, rule: NetworkRule) -> Self {
        self.deny.push(rule);
        self
    }
}

/// One egress rule: it matches a connection to any of `to` on any of `ports`.
///
/// An empty `to` matches every destination, and an empty `ports` matches every protocol and
/// port.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkRule {
    /// Destination networks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<NetworkPeer>,
    /// Destination protocols and ports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<NetworkPort>,
}

impl NetworkRule {
    /// Creates a rule that matches every protocol and port of `cidr`, such as `"10.0.0.0/8"` or
    /// `"192.0.2.1"`.
    pub fn to(cidr: impl Into<String>) -> Self {
        Self {
            to: vec![NetworkPeer::new(cidr)],
            ports: Vec::new(),
        }
    }

    /// Restricts the rule to one port of `protocol`.
    #[must_use]
    pub fn on_port(mut self, protocol: Protocol, port: u16) -> Self {
        self.ports.push(NetworkPort {
            protocol,
            port: Some(port),
            end_port: None,
        });
        self
    }

    /// Restricts the rule to the ports of `protocol` from `port` through `end_port`, both
    /// included.
    #[must_use]
    pub fn on_port_range(mut self, protocol: Protocol, port: u16, end_port: u16) -> Self {
        self.ports.push(NetworkPort {
            protocol,
            port: Some(port),
            end_port: Some(end_port),
        });
        self
    }

    /// Restricts the rule to `protocol` on every port.
    #[must_use]
    pub fn on_protocol(mut self, protocol: Protocol) -> Self {
        self.ports.push(NetworkPort {
            protocol,
            port: None,
            end_port: None,
        });
        self
    }
}

/// A destination network of a [`NetworkRule`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkPeer {
    /// Destination address or CIDR, such as `"10.0.0.0/8"` or `"192.0.2.1"`.
    pub cidr: String,
    /// Sub-networks of `cidr` that the rule does not match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub except: Vec<String>,
}

impl NetworkPeer {
    /// Creates a peer for `cidr` without exceptions.
    pub fn new(cidr: impl Into<String>) -> Self {
        Self {
            cidr: cidr.into(),
            except: Vec::new(),
        }
    }
}

/// A destination protocol and port range of a [`NetworkRule`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkPort {
    /// Transport protocol. Defaults to any protocol.
    #[serde(default)]
    pub protocol: Protocol,
    /// First destination port. `None` matches every port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Last destination port of a range that starts at `port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_port: Option<u16>,
}

/// Transport protocol of a [`NetworkPort`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
    /// ICMP.
    Icmp,
    /// Any protocol.
    #[default]
    Any,
}

impl Protocol {
    /// Returns the contract spelling of this protocol.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Icmp => "icmp",
            Self::Any => "any",
        }
    }
}

/// The contract's `network.ingress` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IngressPolicy {
    /// Default policy for host-initiated connections.
    pub default: Access,
    /// Guest access to host loopback services. `None` selects the backend default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_loopback: Option<Access>,
}

/// Allow or deny decision of a network rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Permit the traffic.
    Allow,
    /// Block the traffic.
    Deny,
}

impl Access {
    /// Returns the contract spelling of this decision.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

impl fmt::Display for Access {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Inputs to `AciEdgeSandbox::exec`.
///
/// The serialized form is the contract's exec request body, `{ "process": { ... } }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecRequest {
    /// Process to run.
    pub process: ProcessSpec,
    /// Standard input wiring. This is a transport option and is not part of the serialized
    /// request.
    #[serde(skip)]
    pub stdin: StdinMode,
}

impl ExecRequest {
    /// Runs `command_line`, the contract's `process.commandLine`.
    pub fn command_line(command_line: impl Into<String>) -> Self {
        Self::from_command(Command::CommandLine(command_line.into()))
    }

    /// Runs an exact argument vector without shell interpretation.
    ///
    /// This is an ACI Edge Sandboxes extension.
    pub fn argv<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::from_command(Command::Argv(argv.into_iter().map(Into::into).collect()))
    }

    fn from_command(command: Command) -> Self {
        Self {
            process: ProcessSpec {
                command,
                cwd: None,
                env: None,
                inherit_default_env: None,
                timeout: None,
            },
            stdin: StdinMode::Null,
        }
    }

    /// Sets the workload timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.process.timeout = Some(timeout);
        self
    }

    /// Sets the working directory inside the sandbox.
    ///
    /// Without one, the workload starts in the backend's default directory; the openvmm backend
    /// uses `/` and accepts only absolute guest paths.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.process.cwd = Some(cwd.into());
        self
    }

    /// Adds one `KEY=VALUE` environment entry.
    ///
    /// Supplying entries makes them the workload's complete environment, as [`ProcessSpec::env`]
    /// describes. Add [`with_inherit_default_env`](Self::with_inherit_default_env) to layer them
    /// over the sandbox's default environment instead.
    #[must_use]
    pub fn with_env(mut self, entry: impl Into<String>) -> Self {
        self.process
            .env
            .get_or_insert_with(Vec::new)
            .push(entry.into());
        self
    }

    /// Sets the `KEY=VALUE` environment entries, replacing any added before.
    ///
    /// An empty list is an explicitly empty environment, which differs from not supplying an
    /// environment at all; see [`ProcessSpec::env`].
    #[must_use]
    pub fn with_envs<I, S>(mut self, entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.process.env = Some(entries.into_iter().map(Into::into).collect());
        self
    }

    /// Selects whether the environment entries are layered over the sandbox's default
    /// environment instead of replacing it. Ignored without environment entries.
    #[must_use]
    pub fn with_inherit_default_env(mut self, inherit: bool) -> Self {
        self.process.inherit_default_env = Some(inherit);
        self
    }

    /// Selects how standard input is wired.
    #[must_use]
    pub fn with_stdin(mut self, stdin: StdinMode) -> Self {
        self.stdin = stdin;
        self
    }
}

/// How an execution's standard input is wired.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum StdinMode {
    /// The workload reads end-of-file from standard input.
    #[default]
    Null,
    /// The caller writes the workload's standard input through
    /// `Execution::take_stdin`.
    Piped,
}

/// The contract's `process` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ProcessSpecWire", into = "ProcessSpecWire")]
pub struct ProcessSpec {
    /// What to run.
    pub command: Command,
    /// Working directory inside the sandbox. `None` selects the backend's default directory.
    pub cwd: Option<String>,
    /// `KEY=VALUE` environment entries.
    ///
    /// `None` gives the workload the sandbox's default environment. `Some`, including an empty
    /// list, is the workload's complete environment, unless
    /// [`inherit_default_env`](Self::inherit_default_env) is `Some(true)`, which layers the
    /// entries over the default environment instead. The serialized form keeps the distinction:
    /// an omitted `env` is `None`, `"env": []` is `Some` of an empty list, and `"env": null` is
    /// malformed, because MXC's schema allows only an array.
    pub env: Option<Vec<String>>,
    /// Whether [`env`](Self::env) is layered over the sandbox's default environment instead of
    /// replacing it; an entry then replaces the default variable of the same name. `None` means
    /// `false`, and the value has no effect without `env`.
    pub inherit_default_env: Option<bool>,
    /// Workload timeout. `None` or zero disables it. Serialized in milliseconds, at most
    /// [`MAX_TIMEOUT_MS`](Self::MAX_TIMEOUT_MS).
    pub timeout: Option<Duration>,
}

impl ProcessSpec {
    /// Largest [`timeout`](Self::timeout), in milliseconds, that MXC's schema allows for
    /// `process.timeout`: 4,294,967,295, about 49.7 days.
    pub const MAX_TIMEOUT_MS: u64 = u32::MAX as u64;
}

/// What an execution runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Command {
    /// The contract's `process.commandLine`, interpreted as a shell command.
    CommandLine(String),
    /// An exact argument vector. This is an ACI Edge Sandboxes extension, serialized as
    /// `process.argv`.
    Argv(Vec<String>),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProcessSpecWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command_line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    argv: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    // MXC's schema allows only an array, so `null` must not read as an omitted environment.
    #[serde(
        default,
        deserialize_with = "present_array",
        skip_serializing_if = "Option::is_none"
    )]
    env: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inherit_default_env: Option<bool>,
    #[serde(
        default,
        deserialize_with = "timeout_millis",
        skip_serializing_if = "Option::is_none"
    )]
    timeout: Option<u64>,
}

impl TryFrom<ProcessSpecWire> for ProcessSpec {
    type Error = &'static str;

    fn try_from(wire: ProcessSpecWire) -> Result<Self, Self::Error> {
        let command = match (wire.command_line, wire.argv) {
            (Some(command_line), None) => Command::CommandLine(command_line),
            (None, Some(argv)) => Command::Argv(argv),
            (None, None) => return Err("process requires commandLine or argv"),
            (Some(_), Some(_)) => return Err("process accepts only one of commandLine and argv"),
        };
        Ok(Self {
            command,
            cwd: wire.cwd,
            env: wire.env,
            inherit_default_env: wire.inherit_default_env,
            timeout: wire.timeout.map(Duration::from_millis),
        })
    }
}

impl From<ProcessSpec> for ProcessSpecWire {
    fn from(spec: ProcessSpec) -> Self {
        let (command_line, argv) = match spec.command {
            Command::CommandLine(command_line) => (Some(command_line), None),
            Command::Argv(argv) => (None, Some(argv)),
        };
        Self {
            command_line,
            argv,
            cwd: spec.cwd,
            env: spec.env,
            inherit_default_env: spec.inherit_default_env,
            timeout: spec.timeout.map(duration_millis),
        }
    }
}

/// Reads a field that `#[serde(default)]` makes optional as `Some`, so that only an omitted field
/// is `None` and an explicit `null` is an error.
fn present_array<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Vec::deserialize(deserializer).map(Some)
}

/// Reads `process.timeout`, which MXC's schema bounds to whole milliseconds from 0 through
/// [`ProcessSpec::MAX_TIMEOUT_MS`], so that a negative or larger value is refused with that
/// range. Like an omitted timeout, `null` disables it.
fn timeout_millis<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Millis;

    impl<'de> serde::de::Visitor<'de> for Millis {
        type Value = Option<u64>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "process.timeout in whole milliseconds from 0 through {}",
                ProcessSpec::MAX_TIMEOUT_MS
            )
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        // A deserializer that buffered the request, as a flattening envelope does, reports
        // `null` as a unit.
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            deserializer.deserialize_u64(self)
        }

        fn visit_u64<E: serde::de::Error>(self, millis: u64) -> Result<Self::Value, E> {
            if millis <= ProcessSpec::MAX_TIMEOUT_MS {
                Ok(Some(millis))
            } else {
                Err(E::invalid_value(
                    serde::de::Unexpected::Unsigned(millis),
                    &self,
                ))
            }
        }

        fn visit_i64<E: serde::de::Error>(self, millis: i64) -> Result<Self::Value, E> {
            match u64::try_from(millis) {
                Ok(millis) => self.visit_u64(millis),
                Err(_) => Err(E::invalid_value(
                    serde::de::Unexpected::Signed(millis),
                    &self,
                )),
            }
        }
    }

    deserializer.deserialize_option(Millis)
}

/// Converts a duration to whole milliseconds, rounding a nonzero sub-millisecond duration up.
pub fn duration_millis(duration: Duration) -> u64 {
    let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    if millis == 0 && !duration.is_zero() {
        1
    } else {
        millis
    }
}

/// Result of `AciEdgeSandbox::provision`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvisionResult {
    /// Identifier of the new sandbox.
    pub sandbox_id: SandboxId,
    /// Backend-defined metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

/// Result of `AciEdgeSandbox::start`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartResult {
    /// Backend-defined metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

/// Result of `AciEdgeSandbox::stop`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopResult {
    /// Backend-defined metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

/// Result of `AciEdgeSandbox::deprovision`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeprovisionResult {
    /// Backend-defined metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provision_request_parses_the_contract_example() {
        let json = r#"{
            "filesystem": {
                "readonlyPaths": ["C:\\workspace\\source"],
                "readwritePaths": ["C:\\workspace\\output"],
                "deniedPaths": ["C:\\workspace\\source\\secrets"]
            },
            "network": {
                "egress": { "default": "deny" },
                "ingress": { "default": "deny", "hostLoopback": "deny" }
            },
            "microvm": { "provision": { "memoryMib": 512 } }
        }"#;
        let request: ProvisionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.microvm.provision.memory_mib, Some(512));
        assert_eq!(request.network, Some(NetworkPolicy::deny_all()));
        assert_eq!(
            request.filesystem.as_ref().unwrap().denied_paths,
            [PathBuf::from("C:\\workspace\\source\\secrets")]
        );
        let round_trip: ProvisionRequest =
            serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        assert_eq!(round_trip, request);
    }

    #[test]
    fn egress_rules_parse_the_contract_shape() {
        let json = r#"{
            "egress": {
                "default": "deny",
                "allow": [{
                    "to": [{ "cidr": "10.0.0.0/8", "except": ["10.1.0.0/16"] }],
                    "ports": [{ "protocol": "tcp", "port": 443, "endPort": 444 }]
                }],
                "deny": [{ "to": [{ "cidr": "192.0.2.1" }] }]
            },
            "ingress": { "default": "deny" }
        }"#;
        let policy: NetworkPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(policy.egress.default, Access::Deny);
        assert_eq!(policy.egress.allow[0].to[0].except, ["10.1.0.0/16"]);
        assert_eq!(policy.egress.allow[0].ports[0].end_port, Some(444));
        assert_eq!(policy.egress.deny, [NetworkRule::to("192.0.2.1")]);
        let round_trip: NetworkPolicy =
            serde_json::from_str(&serde_json::to_string(&policy).unwrap()).unwrap();
        assert_eq!(round_trip, policy);
        let port: NetworkPort = serde_json::from_str("{}").unwrap();
        assert_eq!(port.protocol, Protocol::Any);
        // A protocol without a port matches every port of that protocol.
        let rule = NetworkRule::to("192.0.2.1").on_protocol(Protocol::Icmp);
        assert_eq!(
            serde_json::to_value(&rule).unwrap(),
            serde_json::json!({ "to": [{ "cidr": "192.0.2.1" }], "ports": [{ "protocol": "icmp" }] })
        );
        assert_eq!(
            serde_json::from_str::<NetworkRule>(
                r#"{ "to": [{ "cidr": "192.0.2.1" }], "ports": [{ "protocol": "icmp" }] }"#
            )
            .unwrap(),
            rule
        );
        // A port range carries its inclusive last port as `endPort`.
        let rule = NetworkRule::to("192.0.2.1").on_port_range(Protocol::Tcp, 8000, 8010);
        let json = serde_json::json!({
            "to": [{ "cidr": "192.0.2.1" }],
            "ports": [{ "protocol": "tcp", "port": 8000, "endPort": 8010 }]
        });
        assert_eq!(serde_json::to_value(&rule).unwrap(), json);
        assert_eq!(serde_json::from_value::<NetworkRule>(json).unwrap(), rule);
    }

    #[test]
    fn empty_provision_requests_use_the_defaults() {
        let request: ProvisionRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(request, ProvisionRequest::new());
        assert_eq!(serde_json::to_string(&request).unwrap(), "{}");
        let request: ProvisionRequest =
            serde_json::from_str(r#"{ "microvm": { "provision": {} } }"#).unwrap();
        assert_eq!(request, ProvisionRequest::new());
    }

    #[test]
    fn enumeration_only_policy_is_nonempty_and_round_trips() {
        let json = serde_json::json!({
            "filesystem": { "enumeratePaths": ["C:\\catalog"] }
        });
        let request: ProvisionRequest = serde_json::from_value(json.clone()).unwrap();
        assert!(!request.filesystem.as_ref().unwrap().is_empty());
        assert_eq!(serde_json::to_value(&request).unwrap(), json);
        assert!(
            serde_json::from_str::<FilesystemPolicy>("{}")
                .unwrap()
                .enumerate_paths
                .is_empty()
        );
    }

    #[test]
    fn provision_request_rejects_unknown_fields() {
        for json in [
            r#"{ "phase": "provision" }"#,
            r#"{ "microvm": { "provision": { "scratchPath": "/s" } } }"#,
            r#"{ "microvm": { "provision": { "layers": [] } } }"#,
        ] {
            assert!(
                serde_json::from_str::<ProvisionRequest>(json).is_err(),
                "{json}"
            );
        }
    }

    #[test]
    fn exec_request_parses_the_contract_example() {
        let json = r#"{
            "process": {
                "commandLine": "python app.py",
                "cwd": "/workspace",
                "env": ["MODE=test"],
                "inheritDefaultEnv": true,
                "timeout": 30000
            }
        }"#;
        let request: ExecRequest = serde_json::from_str(json).unwrap();
        assert_eq!(
            request,
            ExecRequest::command_line("python app.py")
                .with_cwd("/workspace")
                .with_env("MODE=test")
                .with_inherit_default_env(true)
                .with_timeout(Duration::from_secs(30))
        );
        let serialized = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serialized,
            serde_json::from_str::<serde_json::Value>(json).unwrap()
        );
    }

    #[test]
    fn exec_environment_distinguishes_omitted_and_empty() {
        let omitted: ExecRequest =
            serde_json::from_str(r#"{ "process": { "commandLine": "env" } }"#).unwrap();
        assert_eq!(omitted, ExecRequest::command_line("env"));
        assert_eq!(omitted.process.env, None);
        assert_eq!(
            serde_json::to_value(&omitted).unwrap(),
            serde_json::json!({"process": {"commandLine": "env"}})
        );

        let empty: ExecRequest =
            serde_json::from_str(r#"{ "process": { "commandLine": "env", "env": [] } }"#).unwrap();
        assert_eq!(
            empty,
            ExecRequest::command_line("env").with_envs(Vec::<String>::new())
        );
        assert_eq!(empty.process.env, Some(Vec::new()));
        let serialized = serde_json::to_value(&empty).unwrap();
        assert_eq!(
            serialized,
            serde_json::json!({"process": {"commandLine": "env", "env": []}})
        );
        assert_eq!(
            serde_json::from_value::<ExecRequest>(serialized).unwrap(),
            empty
        );
        assert_ne!(omitted, empty);

        // MXC's schema allows only an array, so `null` is malformed rather than omitted.
        let null = r#"{ "process": { "commandLine": "env", "env": null } }"#;
        assert!(serde_json::from_str::<ExecRequest>(null).is_err());
    }

    #[test]
    fn environment_builders_add_or_replace_entries() {
        let request = ExecRequest::command_line("env")
            .with_env("A=1")
            .with_env("B=");
        assert_eq!(
            request.process.env,
            Some(vec!["A=1".to_owned(), "B=".to_owned()])
        );
        let replaced = request.with_envs(["C=3"]);
        assert_eq!(replaced.process.env, Some(vec!["C=3".to_owned()]));
        assert_eq!(ExecRequest::command_line("env").process.env, None);
    }

    #[test]
    fn exec_request_requires_exactly_one_command_form() {
        for json in [
            r#"{ "process": {} }"#,
            r#"{ "process": { "commandLine": "a", "argv": ["/bin/a"] } }"#,
            r#"{ "process": { "commandLine": "a", "shell": true } }"#,
        ] {
            assert!(serde_json::from_str::<ExecRequest>(json).is_err(), "{json}");
        }
        let request: ExecRequest =
            serde_json::from_str(r#"{ "process": { "argv": ["/bin/echo", "hi"] } }"#).unwrap();
        assert_eq!(request, ExecRequest::argv(["/bin/echo", "hi"]));
    }

    #[test]
    fn sub_millisecond_timeouts_round_up() {
        assert_eq!(duration_millis(Duration::from_micros(10)), 1);
        assert_eq!(duration_millis(Duration::ZERO), 0);
        assert_eq!(duration_millis(Duration::from_millis(1500)), 1500);
    }

    #[test]
    fn exec_timeouts_follow_the_mxc_schema() {
        let parse = |timeout: &str| {
            serde_json::from_str::<ExecRequest>(&format!(
                r#"{{ "process": {{ "commandLine": "true", "timeout": {timeout} }} }}"#
            ))
        };
        assert_eq!(ProcessSpec::MAX_TIMEOUT_MS, 4_294_967_295);
        for millis in [0, 3_600_001, 86_400_000, ProcessSpec::MAX_TIMEOUT_MS] {
            let request = parse(&millis.to_string()).unwrap();
            assert_eq!(
                request,
                ExecRequest::command_line("true").with_timeout(Duration::from_millis(millis))
            );
            assert_eq!(
                serde_json::to_value(&request).unwrap()["process"]["timeout"],
                millis
            );
        }
        // Like an omitted timeout, `null` disables it.
        assert_eq!(parse("null").unwrap(), ExecRequest::command_line("true"));
        // Also when a caller's envelope flattens the request, which buffers it.
        #[derive(Deserialize)]
        struct Envelope {
            phase: String,
            #[serde(flatten)]
            request: ExecRequest,
        }
        for (timeout, expected) in [
            ("null", ExecRequest::command_line("true")),
            (
                "4294967295",
                ExecRequest::command_line("true")
                    .with_timeout(Duration::from_millis(ProcessSpec::MAX_TIMEOUT_MS)),
            ),
        ] {
            let json = format!(
                r#"{{ "phase": "exec", "process": {{ "commandLine": "true", "timeout": {timeout} }} }}"#
            );
            let envelope = serde_json::from_str::<Envelope>(&json).unwrap();
            assert_eq!(envelope.phase, "exec");
            assert_eq!(envelope.request, expected, "{timeout}");
        }
        let flattened = serde_json::from_str::<Envelope>(
            r#"{ "phase": "exec", "process": { "commandLine": "true", "timeout": -1 } }"#,
        );
        let error = flattened.err().unwrap().to_string();
        assert!(error.contains("from 0 through 4294967295"), "{error}");
        for invalid in [
            "-1",
            "4294967296",
            "18446744073709551615",
            "1.5",
            r#""1000""#,
        ] {
            let error = parse(invalid).unwrap_err().to_string();
            assert!(
                error.contains("process.timeout in whole milliseconds from 0 through 4294967295"),
                "{invalid}: {error}"
            );
        }
    }
}
