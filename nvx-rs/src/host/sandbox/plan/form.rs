//! The serde form of a [`SandboxPlan`], in which callers persist plans, as the [plan
//! module](super) describes it.

use serde::{Deserialize, Serialize};

use super::{PLAN_VERSION, SandboxDistro, SandboxNetwork, SandboxPlan, SandboxUpper};
use crate::host::filesystem::ExportPlan;
use crate::host::network::{
    Access, Destination, EgressRule, EgressRules, ForwardProtocol, HostLoopback, LoopbackForward,
    NetworkPolicy, NetworkProxy, PortSelector, Protocol, ProxyScheme,
};
use crate::host::sandbox::{SandboxSettings, parse_decimal};
use crate::protocol::root::{ImageFormat, RootImage};
use crate::uuid::Uuid;

/// The serde form of a [`SandboxPlan`].
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PlanForm {
    /// [`PLAN_VERSION`].
    version: u32,
    /// The image.
    distro: DistroForm,
    /// The upper.
    upper: UpperForm,
    /// The launch settings of the sandbox profile, or `None` for an adapter's plan, which the form
    /// requires as `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    settings: Option<SettingsForm>,
    /// The export plan of the live shares, or `None`, which the form requires as `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    export: Option<ExportPlan>,
    /// The network, or `None`, which the form requires as `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    network: Option<NetworkForm>,
}

/// The serde form of a [`SandboxDistro`].
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DistroForm {
    /// The image's path.
    path: String,
    /// The image's format.
    format: FormatForm,
    /// The canonical text of the identity that the guest expects of the image, or `None`, which
    /// the form requires as `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    identity: Option<String>,
}

/// The serde form of an [`ImageFormat`].
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FormatForm {
    /// [`ImageFormat::Erofs`].
    Erofs,
    /// [`ImageFormat::Gpt`].
    Gpt,
}

/// The serde form of a [`SandboxUpper`], tagged with its kind.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum UpperForm {
    /// An upper on a scratch image.
    Scratch {
        /// The scratch image's path.
        path: String,
    },
    /// An upper in RAM, which has no other field.
    Ram {},
}

/// The serde form of [`SandboxSettings`].
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SettingsForm {
    /// The workload's program.
    entrypoint: String,
    /// The workload's arguments.
    args: Vec<String>,
    /// The workload's hostname.
    hostname: String,
    /// The workload's UID and GID.
    workload_identity: IdentityForm,
    /// The decimal text of the memory limit, or `None`, which the form requires as `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    memory_max: Option<String>,
    /// The decimal text of the process limit, or `None`, which the form requires as `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    pids_max: Option<String>,
}

/// The serde form of a workload identity.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityForm {
    /// The UID.
    uid: u32,
    /// The GID.
    gid: u32,
}

/// The serde form of a [`SandboxNetwork`] and its [`NetworkPolicy`].
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct NetworkForm {
    /// The guest's address and prefix.
    guest_network: String,
    /// The default for guest-initiated connections.
    egress: AccessForm,
    /// The rules that permit destinations.
    allow: RulesForm,
    /// The rules that block destinations.
    deny: RulesForm,
    /// The default for connections that clients initiate to the guest.
    ingress: AccessForm,
    /// The access between the guest and host loopback.
    host_loopback: HostLoopbackForm,
    /// The ports on host loopback that are forwarded to guest ports.
    host_loopback_forwards: Vec<ForwardForm>,
    /// The proxy of a proxy-only network, or `None`, which the form requires as `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    proxy: Option<ProxyForm>,
}

/// The serde form of an [`Access`].
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AccessForm {
    /// [`Access::Allow`].
    Allow,
    /// [`Access::Deny`].
    Deny,
}

/// The serde form of a [`HostLoopback`].
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum HostLoopbackForm {
    /// [`HostLoopback::Deny`].
    Deny,
    /// [`HostLoopback::Outbound`].
    Outbound,
    /// [`HostLoopback::Allow`].
    Allow,
}

/// The serde form of [`EgressRules`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesForm {
    /// The name of the rules in messages.
    name: String,
    /// The rules.
    rules: Vec<RuleForm>,
}

/// The serde form of an [`EgressRule`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleForm {
    /// The destinations, of which none matches every destination.
    to: Vec<DestinationForm>,
    /// The protocols and ports, of which none matches every one.
    ports: Vec<PortForm>,
}

/// The serde form of a [`Destination`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DestinationForm {
    /// The destination address or network.
    cidr: String,
    /// The sub-networks that the destination excludes.
    except: Vec<String>,
}

/// The serde form of a [`PortSelector`].
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PortForm {
    /// The protocol.
    protocol: ProtocolForm,
    /// The port, or the first port of a range, or `None`, which the form requires as `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    port: Option<u16>,
    /// The last port of a range, or `None`, which the form requires as `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    end_port: Option<u16>,
}

/// The serde form of a [`Protocol`].
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ProtocolForm {
    /// [`Protocol::Tcp`].
    Tcp,
    /// [`Protocol::Udp`].
    Udp,
    /// [`Protocol::Icmp`].
    Icmp,
    /// [`Protocol::Any`].
    Any,
}

/// The serde form of a [`LoopbackForward`].
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ForwardForm {
    /// The protocol.
    protocol: ForwardProtocolForm,
    /// The port on host loopback.
    host_port: u16,
    /// The guest port.
    guest_port: u16,
}

/// The serde form of a [`ForwardProtocol`].
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ForwardProtocolForm {
    /// [`ForwardProtocol::Tcp`].
    Tcp,
    /// [`ForwardProtocol::Udp`].
    Udp,
}

/// The serde form of a [`NetworkProxy`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProxyForm {
    /// The proxy's port on host loopback.
    port: u16,
    /// The scheme of the proxy's URL, which [`ProxyScheme::from_name`] reads, or `None`, which
    /// the form requires as `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    scheme: Option<String>,
}

impl From<SandboxPlan> for PlanForm {
    fn from(plan: SandboxPlan) -> Self {
        let SandboxPlan {
            distro,
            upper,
            settings,
            export,
            network,
        } = plan;
        let RootImage { format, identity } = distro.image;
        Self {
            version: PLAN_VERSION,
            distro: DistroForm {
                path: distro.path.as_str().to_owned(),
                format: match format {
                    ImageFormat::Erofs => FormatForm::Erofs,
                    ImageFormat::Gpt => FormatForm::Gpt,
                },
                identity: identity.map(|identity| identity.to_string()),
            },
            upper: match upper.scratch {
                Some(path) => UpperForm::Scratch {
                    path: path.as_str().to_owned(),
                },
                None => UpperForm::Ram {},
            },
            settings: settings.map(SettingsForm::from),
            export,
            network: network.map(NetworkForm::from),
        }
    }
}

impl TryFrom<PlanForm> for SandboxPlan {
    type Error = String;

    /// Reads a plan, which must be of [`PLAN_VERSION`], hold an identity in its canonical text and
    /// limits in decimal text, and pass the checks of [`SandboxPlan::new`], or be an adapter's
    /// plan, [`SandboxPlan::adapter`], and then pass those of [`SandboxPlan::with_export`] and
    /// [`SandboxNetwork::new`].
    fn try_from(form: PlanForm) -> Result<Self, Self::Error> {
        if form.version != PLAN_VERSION {
            return Err(format!(
                "the sandbox plan has version {}, which this build does not read",
                form.version
            ));
        }
        let DistroForm {
            path,
            format,
            identity,
        } = form.distro;
        let identity = identity
            .map(|text| {
                Uuid::parse(&text).ok_or_else(|| {
                    format!(
                        "the distro identity of the sandbox plan, {text:?}, is not a UUID in its \
                         canonical text",
                    )
                })
            })
            .transpose()?;
        let format = match format {
            FormatForm::Erofs => ImageFormat::Erofs,
            FormatForm::Gpt => ImageFormat::Gpt,
        };
        let distro = SandboxDistro::new(&path, RootImage { format, identity });
        let upper = match form.upper {
            UpperForm::Scratch { path } => SandboxUpper::scratch(&path),
            UpperForm::Ram {} => SandboxUpper::RAM,
        };
        let plan = match form.settings {
            Some(settings) => {
                let settings = SandboxSettings::try_from(settings)?;
                Self::new(distro, upper, settings)
            }
            None => Ok(Self::adapter(distro, upper)),
        }
        .and_then(|plan| plan.with_export(form.export))
        .map_err(|error| error.to_string())?;
        let network = form.network.map(SandboxNetwork::try_from).transpose()?;
        Ok(plan.with_network(network))
    }
}

impl From<SandboxNetwork> for NetworkForm {
    fn from(network: SandboxNetwork) -> Self {
        let NetworkPolicy {
            egress,
            allow,
            deny,
            ingress,
            host_loopback,
            host_loopback_forwards,
            proxy,
        } = network.policy;
        let access = |access| match access {
            Access::Allow => AccessForm::Allow,
            Access::Deny => AccessForm::Deny,
        };
        Self {
            guest_network: network.guest_network,
            egress: access(egress),
            allow: RulesForm::from(allow),
            deny: RulesForm::from(deny),
            ingress: access(ingress),
            host_loopback: match host_loopback {
                HostLoopback::Deny => HostLoopbackForm::Deny,
                HostLoopback::Outbound => HostLoopbackForm::Outbound,
                HostLoopback::Allow => HostLoopbackForm::Allow,
            },
            host_loopback_forwards: host_loopback_forwards
                .into_iter()
                .map(|forward| ForwardForm {
                    protocol: match forward.protocol {
                        ForwardProtocol::Tcp => ForwardProtocolForm::Tcp,
                        ForwardProtocol::Udp => ForwardProtocolForm::Udp,
                    },
                    host_port: forward.host_port,
                    guest_port: forward.guest_port,
                })
                .collect(),
            proxy: proxy.map(|proxy| ProxyForm {
                port: proxy.port,
                scheme: proxy.scheme.map(|scheme| scheme.as_str().to_owned()),
            }),
        }
    }
}

impl TryFrom<NetworkForm> for SandboxNetwork {
    type Error = String;

    /// Reads a network whose proxy's scheme, if any, [`ProxyScheme::from_name`] reads, and which
    /// passes the checks of [`SandboxNetwork::new`].
    fn try_from(form: NetworkForm) -> Result<Self, Self::Error> {
        let message = |error: crate::Error| error.to_string();
        let access = |access| match access {
            AccessForm::Allow => Access::Allow,
            AccessForm::Deny => Access::Deny,
        };
        let proxy = form
            .proxy
            .map(|proxy| {
                let scheme = proxy
                    .scheme
                    .as_deref()
                    .map(ProxyScheme::from_name)
                    .transpose()
                    .map_err(message)?;
                Ok::<_, String>(NetworkProxy {
                    port: proxy.port,
                    scheme,
                })
            })
            .transpose()?;
        let policy = NetworkPolicy {
            egress: access(form.egress),
            allow: EgressRules::from(form.allow),
            deny: EgressRules::from(form.deny),
            ingress: access(form.ingress),
            host_loopback: match form.host_loopback {
                HostLoopbackForm::Deny => HostLoopback::Deny,
                HostLoopbackForm::Outbound => HostLoopback::Outbound,
                HostLoopbackForm::Allow => HostLoopback::Allow,
            },
            host_loopback_forwards: form
                .host_loopback_forwards
                .into_iter()
                .map(|forward| LoopbackForward {
                    protocol: match forward.protocol {
                        ForwardProtocolForm::Tcp => ForwardProtocol::Tcp,
                        ForwardProtocolForm::Udp => ForwardProtocol::Udp,
                    },
                    host_port: forward.host_port,
                    guest_port: forward.guest_port,
                })
                .collect(),
            proxy,
        };
        Self::new(&form.guest_network, policy).map_err(message)
    }
}

impl From<EgressRules> for RulesForm {
    fn from(rules: EgressRules) -> Self {
        Self {
            name: rules.name,
            rules: rules
                .rules
                .into_iter()
                .map(|rule| RuleForm {
                    to: rule
                        .to
                        .into_iter()
                        .map(|destination| DestinationForm {
                            cidr: destination.cidr,
                            except: destination.except,
                        })
                        .collect(),
                    ports: rule
                        .ports
                        .into_iter()
                        .map(|port| PortForm {
                            protocol: match port.protocol {
                                Protocol::Tcp => ProtocolForm::Tcp,
                                Protocol::Udp => ProtocolForm::Udp,
                                Protocol::Icmp => ProtocolForm::Icmp,
                                Protocol::Any => ProtocolForm::Any,
                            },
                            port: port.port,
                            end_port: port.end_port,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

impl From<RulesForm> for EgressRules {
    fn from(form: RulesForm) -> Self {
        Self {
            name: form.name,
            rules: form
                .rules
                .into_iter()
                .map(|rule| EgressRule {
                    to: rule
                        .to
                        .into_iter()
                        .map(|destination| Destination {
                            cidr: destination.cidr,
                            except: destination.except,
                        })
                        .collect(),
                    ports: rule
                        .ports
                        .into_iter()
                        .map(|port| PortSelector {
                            protocol: match port.protocol {
                                ProtocolForm::Tcp => Protocol::Tcp,
                                ProtocolForm::Udp => Protocol::Udp,
                                ProtocolForm::Icmp => Protocol::Icmp,
                                ProtocolForm::Any => Protocol::Any,
                            },
                            port: port.port,
                            end_port: port.end_port,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

impl From<SandboxSettings> for SettingsForm {
    fn from(settings: SandboxSettings) -> Self {
        let (uid, gid) = settings.workload_identity;
        Self {
            entrypoint: settings.entrypoint,
            args: settings.args,
            hostname: settings.hostname,
            workload_identity: IdentityForm { uid, gid },
            memory_max: settings.memory_max.map(|limit| limit.to_string()),
            pids_max: settings.pids_max.map(|limit| limit.to_string()),
        }
    }
}

impl TryFrom<SettingsForm> for SandboxSettings {
    type Error = String;

    /// Reads settings whose limits are positive integers in decimal text, without a sign or
    /// leading zeros.
    fn try_from(form: SettingsForm) -> Result<Self, Self::Error> {
        fn limit<T: std::str::FromStr>(
            name: &str,
            text: Option<String>,
        ) -> Result<Option<T>, String> {
            text.map(|text| {
                (!text.starts_with('0'))
                    .then(|| parse_decimal(&text))
                    .flatten()
                    .ok_or_else(|| {
                        format!(
                            "the {name} of the sandbox plan, {text:?}, is not a positive integer in \
                             decimal text",
                        )
                    })
            })
            .transpose()
        }
        let IdentityForm { uid, gid } = form.workload_identity;
        Ok(Self {
            entrypoint: form.entrypoint,
            args: form.args,
            hostname: form.hostname,
            workload_identity: (uid, gid),
            memory_max: limit("memoryMax", form.memory_max)?,
            pids_max: limit("pidsMax", form.pids_max)?,
        })
    }
}
