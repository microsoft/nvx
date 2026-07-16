// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! Versioned external L2Bridge launch-manifest validation.
//!
//! This module deliberately contains no HCN identity: namespace, endpoint, adapter and policy
//! ownership remain in the Compute Agent. It is platform-neutral so schema compatibility is
//! covered by the ordinary Linux unit-test build as well as the Windows build.

use ::std::collections::HashSet;
use ::std::net::Ipv4Addr;
use ::std::path::Path;

use ::anyhow::{Context, Result, bail};
use ::serde::Deserialize as DeriveDeserialize;
use ::serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use ::serde_json::{Map, Value};
use ::sha2::{Digest, Sha256};

const VERSION: u32 = 1;
const MAX_CONTROL_PIPE: usize = 240;
const MAX_CMDLINE_FRAGMENT: usize = 2048;
/// The initramfs transport must fit the kernel command-line limit without truncation.
pub const MAX_GUEST_CMDLINE: usize = 2048;
/// Smallest IPv4 MTU accepted by the external-network contract.
pub const MIN_EXTERNAL_MTU: u32 = 576;
/// Largest MTU accepted by the version-1 wire contract.
pub const MAX_EXTERNAL_MTU: u32 = 65_521;
/// Largest MTU supported by the AF_XDP data-plane frame layout.
pub const MAX_AFXDP_MTU: u32 = 4082;
/// Largest RSS queue set supported by the external data-plane contract.
pub const MAX_EXTERNAL_QUEUES: usize = 64;

/// The fully validated `--net-config` contract owned by NVX.
#[derive(Clone, Debug, DeriveDeserialize, ::serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct L2BridgeConfig {
    pub version: u32,
    pub attachment: Attachment,
    pub device: Device,
    #[serde(rename = "guestBootstrap")]
    pub guest_bootstrap: GuestBootstrap,
    pub runtime: Runtime,
}

#[derive(Clone, Debug, DeriveDeserialize, ::serde::Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Attachment {
    pub backend: String,
    pub interface_index: u32,
    pub queue_selection: QueueSelection,
}

#[derive(Clone, Debug, DeriveDeserialize, ::serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueueSelection {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queues: Vec<u32>,
}

#[derive(Clone, Debug, DeriveDeserialize, ::serde::Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Device {
    pub mac_address: String,
    pub mtu: u32,
}

#[derive(Clone, Debug, DeriveDeserialize, ::serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestBootstrap {
    pub ipv4: Ipv4,
    pub routes: Vec<Route>,
    pub dns: Dns,
}

#[derive(Clone, Debug, DeriveDeserialize, Eq, PartialEq, ::serde::Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Ipv4 {
    pub address: String,
    pub prefix_length: u8,
    pub gateway: String,
}

#[derive(Clone, Debug, DeriveDeserialize, Eq, PartialEq, ::serde::Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Route {
    pub destination: String,
    pub next_hop: String,
}

#[derive(Clone, Debug, DeriveDeserialize, Eq, PartialEq, ::serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Dns {
    pub servers: Vec<String>,
    pub search: Vec<String>,
}

#[derive(Clone, Debug, DeriveDeserialize, ::serde::Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Runtime {
    pub control_pipe: String,
}

/// A guest-visible external-NIC identity. Host attachment state is intentionally absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalIdentity {
    pub mac: [u8; 6],
    pub mtu: u32,
    pub bootstrap: Vec<u8>,
    pub bootstrap_digest: [u8; 32],
}

impl L2BridgeConfig {
    /// Loads, rejects duplicate JSON keys, and validates every supported v1 field.
    pub fn from_path(path: &Path) -> Result<Self> {
        let bytes: Vec<u8> = ::std::fs::read(path)
            .with_context(|| format!("reading --net-config {}", path.display()))?;
        Self::from_json(&bytes)
    }

    /// Parses the immutable manifest supplied to a VM process.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let mut deserializer = ::serde_json::Deserializer::from_slice(bytes);
        let value: Value = StrictValue::deserialize(&mut deserializer)
            .context("parsing --net-config JSON")?
            .0;
        deserializer
            .end()
            .context("trailing data in --net-config")?;
        let config: Self =
            ::serde_json::from_value(value).context("invalid --net-config schema")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            bail!(
                "unsupported --net-config version {} (only {VERSION} is supported)",
                self.version
            );
        }
        if self.attachment.backend != "afxdp-l2bridge" {
            bail!(
                "unsupported external network backend {:?}",
                self.attachment.backend
            );
        }
        if self.attachment.interface_index == 0 {
            bail!("attachment.interfaceIndex must be a non-zero Windows interface index");
        }
        match self.attachment.queue_selection.mode.as_str() {
            "auto" if self.attachment.queue_selection.queues.is_empty() => {}
            "explicit" => {
                let queues = &self.attachment.queue_selection.queues;
                if queues.is_empty()
                    || queues.len() > MAX_EXTERNAL_QUEUES
                    || queues.windows(2).any(|q| q[0] >= q[1])
                {
                    bail!(
                        "explicit queueSelection.queues must contain 1..={MAX_EXTERNAL_QUEUES} strictly ascending entries"
                    );
                }
            }
            "auto" => bail!("queueSelection.mode=auto must not include queues"),
            _ => bail!("queueSelection.mode must be auto or explicit"),
        }

        let _ = self.mac()?;
        if !(MIN_EXTERNAL_MTU..=MAX_EXTERNAL_MTU).contains(&self.device.mtu) {
            bail!("device.mtu must be in {MIN_EXTERNAL_MTU}..={MAX_EXTERNAL_MTU}");
        }
        let ip: Ipv4Addr = self
            .guest_bootstrap
            .ipv4
            .address
            .parse()
            .context("guestBootstrap.ipv4.address must be IPv4")?;
        let gateway: Ipv4Addr = self
            .guest_bootstrap
            .ipv4
            .gateway
            .parse()
            .context("guestBootstrap.ipv4.gateway must be IPv4")?;
        if !(1..=32).contains(&self.guest_bootstrap.ipv4.prefix_length) {
            bail!("guestBootstrap.ipv4.prefixLength must be in 1..=32");
        }
        if ip.is_unspecified() || gateway.is_unspecified() {
            bail!("guestBootstrap IPv4 address and gateway must not be unspecified");
        }
        if self.guest_bootstrap.routes.len() > 32 {
            bail!("guestBootstrap.routes exceeds the 32-route limit");
        }
        for route in &self.guest_bootstrap.routes {
            parse_ipv4_cidr(&route.destination)
                .with_context(|| format!("invalid route destination {:?}", route.destination))?;
            if !route.next_hop.is_empty() {
                let _: Ipv4Addr = route
                    .next_hop
                    .parse()
                    .with_context(|| format!("invalid route nextHop {:?}", route.next_hop))?;
            }
        }
        if self.guest_bootstrap.dns.servers.len() > 16 || self.guest_bootstrap.dns.search.len() > 16
        {
            bail!("guestBootstrap DNS lists exceed their 16-entry limits");
        }
        for server in &self.guest_bootstrap.dns.servers {
            let _: Ipv4Addr = server.parse().context("DNS servers must be IPv4 in v1")?;
        }
        for domain in &self.guest_bootstrap.dns.search {
            validate_token(domain, "DNS search domain")?;
        }
        if !self.runtime.control_pipe.starts_with(r"\\.\pipe\")
            || self.runtime.control_pipe.len() > MAX_CONTROL_PIPE
            || self.runtime.control_pipe.contains('\0')
        {
            bail!("runtime.controlPipe must be a bounded local Windows named-pipe path");
        }
        let _ = self.guest_cmdline_fragment()?;
        Ok(())
    }

    /// Returns the exact CNI/HCN endpoint MAC; it is never derived from the address.
    pub fn mac(&self) -> Result<[u8; 6]> {
        let parts: Vec<&str> = self.device.mac_address.split('-').collect();
        if parts.len() != 6 || parts.iter().any(|p| p.len() != 2) {
            bail!(
                "device.macAddress must use six uppercase-or-lowercase hexadecimal octets separated by '-'"
            );
        }
        let mut mac = [0; 6];
        for (out, part) in mac.iter_mut().zip(parts) {
            *out = u8::from_str_radix(part, 16).context("invalid device.macAddress")?;
        }
        if mac[0] & 1 != 0 || mac == [0; 6] {
            bail!("device.macAddress must be a nonzero unicast MAC");
        }
        Ok(mac)
    }

    /// A bounded kernel-command-line transport for the minimal Alpine guest.
    pub fn guest_cmdline_fragment(&self) -> Result<String> {
        let ipv4 = &self.guest_bootstrap.ipv4;
        let explicit_routes = self
            .guest_bootstrap
            .routes
            .iter()
            .map(|r| format!("{}@{}", r.destination, r.next_hop))
            .collect::<Vec<_>>()
            .join(",");
        if !explicit_routes.is_empty() {
            validate_token(&explicit_routes, "guest network field")?;
        }
        let effective_routes = effective_routes(ipv4, &self.guest_bootstrap.routes);
        let routes = effective_routes
            .iter()
            .map(|r| format!("{}@{}", r.destination, r.next_hop))
            .collect::<Vec<_>>()
            .join(",");
        let dns = self.guest_bootstrap.dns.servers.join(",");
        let search = self.guest_bootstrap.dns.search.join(",");
        for token in [&dns, &search] {
            if !token.is_empty() {
                validate_token(token, "guest network field")?;
            }
        }
        let mac = self.device.mac_address.replace('-', ":");
        let prefix = ipv4.prefix_length;
        let mask = Ipv4Addr::from(if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        });
        let fragment = format!(
            "virtio_mmio.device={:#x}@{:#x}:{} virtnet_required=1 virtnet_ip={} \
             virtnet_prefix={} virtnet_mask={} virtnet_mac={} virtnet_mtu={} virtnet_routes={} \
             virtnet_dns={} virtnet_search={}",
            0x1000_u64,
            0xd000_0000_u64,
            5,
            ipv4.address,
            ipv4.prefix_length,
            mask,
            mac,
            self.device.mtu,
            routes,
            dns,
            search,
        );
        if fragment.len() > MAX_CMDLINE_FRAGMENT {
            bail!("guest network command-line transport exceeds {MAX_CMDLINE_FRAGMENT} bytes");
        }
        Ok(fragment)
    }

    /// Produces the snapshot-safe guest-visible identity, excluding interface and XDP resources.
    pub fn external_identity(&self) -> Result<ExternalIdentity> {
        let effective_routes =
            effective_routes(&self.guest_bootstrap.ipv4, &self.guest_bootstrap.routes);
        let bootstrap = ::serde_json::to_vec(&BootstrapIdentity {
            version: VERSION,
            ipv4: &self.guest_bootstrap.ipv4,
            routes: &effective_routes,
            dns: &self.guest_bootstrap.dns,
        })
        .context("serializing guest bootstrap identity")?;
        let bootstrap_digest: [u8; 32] = Sha256::digest(&bootstrap).into();
        Ok(ExternalIdentity {
            mac: self.mac()?,
            mtu: self.device.mtu,
            bootstrap,
            bootstrap_digest,
        })
    }
}

impl ExternalIdentity {
    /// Compares guest-visible identity while accepting legacy v1 snapshots whose route list
    /// omitted the default route represented by `ipv4.gateway`.
    pub fn equivalent(&self, other: &Self) -> bool {
        if self.mac != other.mac || self.mtu != other.mtu {
            return false;
        }
        if self.bootstrap == other.bootstrap && self.bootstrap_digest == other.bootstrap_digest {
            return true;
        }
        canonical_bootstrap(&self.bootstrap) == canonical_bootstrap(&other.bootstrap)
    }
}

#[derive(::serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapIdentity<'a> {
    version: u32,
    ipv4: &'a Ipv4,
    routes: &'a [Route],
    dns: &'a Dns,
}

#[derive(DeriveDeserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct OwnedBootstrapIdentity {
    version: u32,
    ipv4: Ipv4,
    routes: Vec<Route>,
    dns: Dns,
}

fn effective_routes(ipv4: &Ipv4, routes: &[Route]) -> Vec<Route> {
    let mut effective = routes.to_vec();
    if !effective
        .iter()
        .any(|route| route.destination == "0.0.0.0/0")
    {
        effective.push(Route {
            destination: "0.0.0.0/0".to_owned(),
            next_hop: ipv4.gateway.clone(),
        });
    }
    effective
}

fn canonical_bootstrap(bytes: &[u8]) -> Option<OwnedBootstrapIdentity> {
    let mut identity: OwnedBootstrapIdentity = ::serde_json::from_slice(bytes).ok()?;
    if identity.version != VERSION {
        return None;
    }
    identity.routes = effective_routes(&identity.ipv4, &identity.routes);
    Some(identity)
}

fn parse_ipv4_cidr(value: &str) -> Result<()> {
    let (address, prefix) = value.split_once('/').context("missing '/'")?;
    let _: Ipv4Addr = address.parse().context("address is not IPv4")?;
    let prefix: u8 = prefix.parse().context("prefix is not an integer")?;
    if prefix > 32 {
        bail!("prefix exceeds 32");
    }
    Ok(())
}

fn validate_token(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 253
        || !value.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b',' | b'/' | b'@' | b':')
        })
    {
        bail!("{field} contains unsupported characters");
    }
    Ok(())
}

/// A JSON value deserializer that detects duplicate keys at every object level before serde
/// maps them into structs (where duplicate keys would otherwise be silently overwritten).
struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> ::std::result::Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> ::std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(value)))
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> ::std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> ::std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> ::std::result::Result<Self::Value, E> {
        let number =
            ::serde_json::Number::from_f64(value).ok_or_else(|| E::custom("non-finite number"))?;
        Ok(StrictValue(Value::Number(number)))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> ::std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value.to_owned())))
    }
    fn visit_string<E: de::Error>(self, value: String) -> ::std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value)))
    }
    fn visit_none<E: de::Error>(self) -> ::std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }
    fn visit_unit<E: de::Error>(self) -> ::std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }
    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut sequence: A,
    ) -> ::std::result::Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictValue>()? {
            values.push(value.0);
        }
        Ok(StrictValue(Value::Array(values)))
    }
    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> ::std::result::Result<Self::Value, A::Error> {
        let mut object = Map::new();
        let mut names = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !names.insert(key.clone()) {
                return Err(de::Error::custom(format!("duplicate JSON key {key:?}")));
            }
            let value = map.next_value::<StrictValue>()?;
            object.insert(key, value.0);
        }
        Ok(StrictValue(Value::Object(object)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"{
      "version": 1,
      "attachment": {"backend":"afxdp-l2bridge","interfaceIndex":123,"queueSelection":{"mode":"auto"}},
      "device": {"macAddress":"00-15-5D-01-02-03","mtu":1500},
      "guestBootstrap": {
        "ipv4":{"address":"192.168.0.12","prefixLength":24,"gateway":"192.168.0.1"},
        "routes":[{"destination":"0.0.0.0/0","nextHop":"192.168.0.1"}],
        "dns":{"servers":["168.63.129.16"],"search":["internal.example"]}
      },
      "runtime":{"controlPipe":"\\\\.\\pipe\\aci-nvx-network-sandbox"}
    }"#;

    #[test]
    fn manifest_preserves_exact_identity() {
        let cfg = L2BridgeConfig::from_json(CONFIG.as_bytes()).unwrap();
        assert_eq!(cfg.mac().unwrap(), [0, 0x15, 0x5d, 1, 2, 3]);
        assert_eq!(cfg.device.mtu, 1500);
        let identity = cfg.external_identity().unwrap();
        assert_eq!(
            identity.bootstrap_digest,
            Sha256::digest(&identity.bootstrap).as_slice()
        );
        assert!(
            cfg.guest_cmdline_fragment()
                .unwrap()
                .contains("virtnet_prefix=24")
        );
        let encoded = ::serde_json::to_value(&cfg).unwrap();
        assert_eq!(encoded["attachment"]["backend"], "afxdp-l2bridge");
        assert_eq!(encoded["attachment"]["interfaceIndex"], 123);
        assert_eq!(encoded["attachment"]["queueSelection"]["mode"], "auto");
        assert!(
            encoded["attachment"]["queueSelection"]
                .get("queues")
                .is_none()
        );
        assert_eq!(encoded["device"]["macAddress"], "00-15-5D-01-02-03");
        assert_eq!(encoded["guestBootstrap"]["ipv4"]["prefixLength"], 24);
        assert_eq!(encoded["guestBootstrap"]["ipv4"]["gateway"], "192.168.0.1");
        assert_eq!(
            encoded["guestBootstrap"]["routes"][0]["nextHop"],
            "192.168.0.1"
        );
        assert_eq!(
            encoded["runtime"]["controlPipe"],
            r"\\.\pipe\aci-nvx-network-sandbox"
        );
    }

    #[test]
    fn manifest_rejects_duplicate_and_unknown_fields() {
        assert!(
            L2BridgeConfig::from_json(
                CONFIG
                    .replacen("\"version\": 1", "\"version\": 1,\"version\": 1", 1)
                    .as_bytes()
            )
            .is_err()
        );
        assert!(
            L2BridgeConfig::from_json(
                CONFIG
                    .replacen("\"version\": 1", "\"unknown\":true,\"version\": 1", 1)
                    .as_bytes()
            )
            .is_err()
        );
    }

    #[test]
    fn manifest_accepts_maximum_external_mtu() {
        assert!(
            L2BridgeConfig::from_json(CONFIG.replace("\"mtu\":1500", "\"mtu\":65521").as_bytes())
                .is_ok()
        );
        assert!(
            L2BridgeConfig::from_json(CONFIG.replace("\"mtu\":1500", "\"mtu\":65522").as_bytes())
                .is_err()
        );
    }

    #[test]
    fn manifest_preserves_v1_gateway_in_snapshot_identity() {
        let cfg = L2BridgeConfig::from_json(CONFIG.as_bytes()).unwrap();
        let identity: Value =
            ::serde_json::from_slice(&cfg.external_identity().unwrap().bootstrap).unwrap();
        assert_eq!(identity["ipv4"]["gateway"], "192.168.0.1");
    }

    #[test]
    fn manifest_encodes_on_link_route() {
        let on_link = CONFIG.replace("\"nextHop\":\"192.168.0.1\"", "\"nextHop\":\"\"");
        let cfg = L2BridgeConfig::from_json(on_link.as_bytes()).unwrap();
        assert_eq!(cfg.guest_bootstrap.routes[0].next_hop, "");
        assert!(
            cfg.guest_cmdline_fragment()
                .unwrap()
                .contains("virtnet_routes=0.0.0.0/0@ ")
        );
    }

    #[test]
    fn external_cmdline_requires_network_without_gateway_token() {
        let fragment = L2BridgeConfig::from_json(CONFIG.as_bytes())
            .unwrap()
            .guest_cmdline_fragment()
            .unwrap();
        assert!(fragment.contains(" virtnet_required=1 "));
        assert!(!fragment.contains("virtnet_gw="));
    }

    #[test]
    fn gateway_supplies_only_a_missing_default_route() {
        let without_default = CONFIG.replace(
            r#""routes":[{"destination":"0.0.0.0/0","nextHop":"192.168.0.1"}]"#,
            r#""routes":[{"destination":"10.0.0.0/8","nextHop":""}]"#,
        );
        let fragment = L2BridgeConfig::from_json(without_default.as_bytes())
            .unwrap()
            .guest_cmdline_fragment()
            .unwrap();
        assert!(fragment.contains("virtnet_routes=10.0.0.0/8@,0.0.0.0/0@192.168.0.1"));
    }

    #[test]
    fn canonical_identity_accepts_legacy_v1_route_omission() {
        let without_default = CONFIG.replace(
            r#""routes":[{"destination":"0.0.0.0/0","nextHop":"192.168.0.1"}]"#,
            r#""routes":[{"destination":"10.0.0.0/8","nextHop":""}]"#,
        );
        let cfg = L2BridgeConfig::from_json(without_default.as_bytes()).unwrap();
        let current = cfg.external_identity().unwrap();
        let legacy_bootstrap = ::serde_json::to_vec(&BootstrapIdentity {
            version: VERSION,
            ipv4: &cfg.guest_bootstrap.ipv4,
            routes: &cfg.guest_bootstrap.routes,
            dns: &cfg.guest_bootstrap.dns,
        })
        .unwrap();
        let legacy = ExternalIdentity {
            mac: current.mac,
            mtu: current.mtu,
            bootstrap_digest: Sha256::digest(&legacy_bootstrap).into(),
            bootstrap: legacy_bootstrap,
        };
        assert_ne!(legacy.bootstrap, current.bootstrap);
        assert!(legacy.equivalent(&current));
    }

    #[test]
    fn synthesized_default_does_not_reduce_v1_explicit_route_budget() {
        let routes = (0..9)
            .map(|index| Route {
                destination: format!("10.{index}.0.0/16"),
                next_hop: "192.168.100.100".to_owned(),
            })
            .collect::<Vec<_>>();
        let explicit = routes
            .iter()
            .map(|route| format!("{}@{}", route.destination, route.next_hop))
            .collect::<Vec<_>>()
            .join(",");
        assert!(explicit.len() <= 253);
        assert!(explicit.len() + ",0.0.0.0/0@192.168.0.1".len() > 253);

        let mut value: Value = ::serde_json::from_str(CONFIG).unwrap();
        value["guestBootstrap"]["routes"] = ::serde_json::to_value(routes).unwrap();
        let cfg = L2BridgeConfig::from_json(&::serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            cfg.guest_cmdline_fragment()
                .unwrap()
                .contains("0.0.0.0/0@192.168.0.1")
        );
    }

    #[test]
    fn manifest_rejects_invalid_queue_combinations() {
        assert!(
            L2BridgeConfig::from_json(
                CONFIG
                    .replace("\"mode\":\"auto\"", "\"mode\":\"auto\",\"queues\":[0]")
                    .as_bytes()
            )
            .is_err()
        );
        assert!(
            L2BridgeConfig::from_json(
                CONFIG
                    .replace(
                        "\"mode\":\"auto\"",
                        "\"mode\":\"explicit\",\"queues\":[2,1]"
                    )
                    .as_bytes()
            )
            .is_err()
        );
        let too_many = (0..=MAX_EXTERNAL_QUEUES)
            .map(|queue| queue.to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            L2BridgeConfig::from_json(
                CONFIG
                    .replace(
                        "\"mode\":\"auto\"",
                        &format!("\"mode\":\"explicit\",\"queues\":[{too_many}]"),
                    )
                    .as_bytes(),
            )
            .is_err()
        );
    }
}
