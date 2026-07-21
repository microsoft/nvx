// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! Borrowed HCN endpoint validation and HCS attachment.

use ::core::ffi::c_void;
use ::std::net::Ipv4Addr;
use ::std::path::Path;
use ::std::ptr;

use ::anyhow::{Context, Result, anyhow, bail};
use ::log::{debug, warn};
use ::serde::{Deserialize, Serialize};
use ::windows::Win32::System::Com::CoTaskMemFree;
use ::windows::Win32::System::HostComputeNetwork::{
    HcnCloseEndpoint, HcnOpenEndpoint, HcnQueryEndpointProperties,
};
use ::windows::core::{Error as WindowsError, GUID, HSTRING, PWSTR};

const QUERY: &str = r#"{"SchemaVersion":{"Major":2,"Minor":0},"Flags":1}"#;

/// User-facing static IPv4 configuration parsed from `--net`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetConfig {
    guest_ip: Ipv4Addr,
    prefix: u8,
    network: Ipv4Addr,
    gateway: Ipv4Addr,
    netmask: Ipv4Addr,
}

impl NetConfig {
    pub fn parse(spec: &str) -> Result<Self> {
        let (address, prefix) = spec
            .split_once('/')
            .with_context(|| format!("--net '{spec}' must be <ipv4>/<prefix>"))?;
        let guest_ip: Ipv4Addr = address
            .parse()
            .with_context(|| format!("--net: invalid IPv4 address '{address}'"))?;
        let prefix: u8 = prefix
            .parse()
            .with_context(|| format!("--net: invalid prefix '{prefix}'"))?;
        if !(1..=30).contains(&prefix) {
            bail!("--net: prefix /{prefix} out of range (use 1..=30)");
        }
        let mask: u32 = u32::MAX << (32 - prefix);
        let network = Ipv4Addr::from(u32::from(guest_ip) & mask);
        let gateway = Ipv4Addr::from(u32::from(network) + 1);
        let broadcast = Ipv4Addr::from(u32::from(network) | !mask);
        if guest_ip == network || guest_ip == broadcast || guest_ip == gateway {
            bail!("--net: guest IP {guest_ip} must be usable and differ from gateway {gateway}");
        }
        Ok(Self {
            guest_ip,
            prefix,
            network,
            gateway,
            netmask: Ipv4Addr::from(mask),
        })
    }
}

/// Stable HCN/HCS networking identity persisted in an HCS snapshot manifest.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct NetworkConfig {
    pub network_id: String,
    pub endpoint_id: String,
    pub adapter_id: String,
    pub guest_ip: Ipv4Addr,
    pub prefix: u8,
    pub network: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub dns_servers: Vec<Ipv4Addr>,
    pub mac_address: Option<String>,
}

impl NetworkConfig {
    pub fn from_external(path: &Path, expected: Option<&NetConfig>) -> Result<Self> {
        let bytes = ::std::fs::read(path)
            .with_context(|| format!("reading external HCN endpoint descriptor {path:?}"))?;
        Self::from_external_document(&bytes, expected)
            .with_context(|| format!("parsing external HCN endpoint descriptor {path:?}"))
    }

    fn from_external_document(bytes: &[u8], expected: Option<&NetConfig>) -> Result<Self> {
        let descriptor: ExternalEndpointDescriptor =
            ::serde_json::from_slice(bytes).context("invalid external HCN endpoint descriptor")?;
        if descriptor.version != 1 {
            bail!(
                "unsupported external HCN endpoint descriptor version {}",
                descriptor.version
            );
        }
        if descriptor.host_attached {
            bail!("HCS requires an endpoint that is not attached to the host namespace");
        }
        let parsed = NetConfig::parse(&format!(
            "{}/{}",
            descriptor.guest_address, descriptor.prefix_length
        ))?;
        if let Some(expected) = expected
            && expected != &parsed
        {
            bail!("--net addressing differs from external HCN endpoint descriptor");
        }
        if descriptor.network_address != parsed.network || descriptor.gateway != parsed.gateway {
            bail!("external HCN endpoint descriptor addressing is inconsistent");
        }
        let config = Self {
            network_id: descriptor.network_id,
            endpoint_id: descriptor.endpoint_id,
            adapter_id: descriptor.adapter_id,
            guest_ip: parsed.guest_ip,
            prefix: parsed.prefix,
            network: parsed.network,
            gateway: parsed.gateway,
            netmask: parsed.netmask,
            dns_servers: descriptor.dns_servers,
            mac_address: Some(descriptor.mac_address),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("network", &self.network_id),
            ("endpoint", &self.endpoint_id),
            ("adapter", &self.adapter_id),
        ] {
            GUID::try_from(value.as_str())
                .with_context(|| format!("invalid HCN {name} ID {value:?}"))?;
        }
        let reparsed = NetConfig::parse(&format!("{}/{}", self.guest_ip, self.prefix))?;
        if reparsed.network != self.network
            || reparsed.gateway != self.gateway
            || reparsed.netmask != self.netmask
        {
            bail!("HCN network addressing is inconsistent");
        }
        if self.dns_servers.is_empty() {
            bail!("HCN endpoint descriptor has no DNS servers");
        }
        let mac = self
            .mac_address
            .as_deref()
            .context("HCN endpoint descriptor has no MAC address")?;
        normalize_mac(mac)?;
        Ok(())
    }

    pub fn cmdline_fragment(&self, mac_address: &str) -> String {
        let dns = self
            .dns_servers
            .iter()
            .map(Ipv4Addr::to_string)
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "virtnet_required=1 virtnet_ip={} virtnet_prefix={} virtnet_mask={} \
             virtnet_mac={} virtnet_mtu=1500 virtnet_routes=0.0.0.0/0@{} \
             virtnet_dns={} virtnet_search=",
            self.guest_ip,
            self.prefix,
            self.netmask,
            mac_address.replace('-', ":"),
            self.gateway,
            dns
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExternalEndpointDescriptor {
    version: u32,
    network_id: String,
    endpoint_id: String,
    adapter_id: String,
    guest_address: Ipv4Addr,
    prefix_length: u8,
    network_address: Ipv4Addr,
    gateway: Ipv4Addr,
    dns_servers: Vec<Ipv4Addr>,
    mac_address: String,
    host_attached: bool,
}

/// Canonical endpoint information handed to the HCS network adapter schema.
pub struct Attachment<'a> {
    pub adapter_id: &'a str,
    pub endpoint_id: &'a str,
    pub mac_address: &'a str,
}

/// A borrowed HCN endpoint. The external owner controls its persistent lifecycle.
pub struct BorrowedEndpoint {
    config: NetworkConfig,
    endpoint_handle: Option<*mut c_void>,
}

impl BorrowedEndpoint {
    pub fn open(config: NetworkConfig) -> Result<Self> {
        config.validate()?;
        let endpoint_id = GUID::try_from(config.endpoint_id.as_str())?;
        let mut borrowed = Self {
            config,
            endpoint_handle: None,
        };

        let mut endpoint_handle = ptr::null_mut();
        let mut error_record = PWSTR::null();
        let result =
            unsafe { HcnOpenEndpoint(&endpoint_id, &mut endpoint_handle, Some(&mut error_record)) };
        hcn_result(
            "HcnOpenEndpoint",
            &borrowed.config.endpoint_id,
            result,
            error_record,
        )?;
        if endpoint_handle.is_null() {
            bail!("HcnOpenEndpoint returned a null handle");
        }
        borrowed.endpoint_handle = Some(endpoint_handle);

        let properties = borrowed.query_endpoint()?;
        if !properties
            .id
            .eq_ignore_ascii_case(&borrowed.config.endpoint_id)
        {
            bail!(
                "HCN endpoint query returned ID {:?}, expected {:?}",
                properties.id,
                borrowed.config.endpoint_id
            );
        }
        if !properties
            .host_compute_network
            .eq_ignore_ascii_case(&borrowed.config.network_id)
        {
            bail!(
                "HCN endpoint belongs to network {:?}, expected {:?}",
                properties.host_compute_network,
                borrowed.config.network_id
            );
        }
        let expected_mac = borrowed
            .config
            .mac_address
            .as_deref()
            .context("external HCN endpoint has no configured MAC address")?;
        if normalize_mac(expected_mac)? != normalize_mac(&properties.mac_address)? {
            bail!(
                "external HCN endpoint MAC {:?} differs from configured MAC {:?}",
                properties.mac_address,
                expected_mac
            );
        }
        let address_matches = properties.ip_configurations.iter().any(|address| {
            address.ip_address == borrowed.config.guest_ip
                && address.prefix_length == borrowed.config.prefix
        });
        if !address_matches {
            bail!(
                "external HCN endpoint does not contain expected address {}/{}",
                borrowed.config.guest_ip,
                borrowed.config.prefix
            );
        }
        borrowed.config.mac_address = Some(properties.mac_address);
        Ok(borrowed)
    }

    pub fn config(&self) -> &NetworkConfig {
        &self.config
    }

    pub fn attachment(&self) -> Attachment<'_> {
        Attachment {
            adapter_id: &self.config.adapter_id,
            endpoint_id: &self.config.endpoint_id,
            mac_address: self
                .config
                .mac_address
                .as_deref()
                .expect("queried HCN endpoint has no MAC"),
        }
    }

    pub fn cleanup(&mut self) -> Result<()> {
        if let Some(handle) = self.endpoint_handle.take() {
            unsafe { HcnCloseEndpoint(handle) }.context("HcnCloseEndpoint")?;
        }
        Ok(())
    }

    fn query_endpoint(&self) -> Result<EndpointProperties> {
        let handle = self
            .endpoint_handle
            .context("HCN endpoint handle is missing")?;
        let mut properties = PWSTR::null();
        let mut error_record = PWSTR::null();
        let query = HSTRING::from(QUERY);
        let result = unsafe {
            HcnQueryEndpointProperties(handle, &query, &mut properties, Some(&mut error_record))
        };
        hcn_result(
            "HcnQueryEndpointProperties",
            &self.config.endpoint_id,
            result,
            error_record,
        )?;
        let document = CoTaskWideString::new(properties).to_string()?;
        debug!("queried external HCN endpoint: {document}");
        ::serde_json::from_str(&document).context("parsing HCN endpoint properties")
    }
}

impl Drop for BorrowedEndpoint {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            warn!("last-resort HCN endpoint close failed: {error:#}");
        }
    }
}

struct CoTaskWideString(PWSTR);

impl CoTaskWideString {
    fn new(value: PWSTR) -> Self {
        Self(value)
    }

    fn to_string(&self) -> Result<String> {
        if self.0.is_null() {
            return Ok(String::new());
        }
        unsafe { self.0.to_string() }.context("decoding HCN UTF-16 result")
    }
}

impl Drop for CoTaskWideString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0.0.cast::<c_void>())) };
        }
    }
}

fn hcn_result(
    operation: &str,
    id: &str,
    result: ::windows::core::Result<()>,
    error_record: PWSTR,
) -> Result<()> {
    let record = CoTaskWideString::new(error_record)
        .to_string()
        .unwrap_or_else(|error| format!("<failed to decode HCN error: {error:#}>"));
    result.map_err(|error: WindowsError| {
        let code = error.code().0 as u32;
        anyhow!(
            "{operation} failed for {id}: {error} (HRESULT 0x{code:08X}); error record: {}",
            if record.is_empty() { "<none>" } else { &record }
        )
    })
}

fn normalize_mac(value: &str) -> Result<String> {
    let normalized = value
        .chars()
        .filter(|character| !matches!(character, ':' | '-'))
        .flat_map(char::to_uppercase)
        .collect::<String>();
    if normalized.len() != 12 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid HCN MAC address {value:?}");
    }
    Ok(normalized)
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct EndpointProperties {
    #[serde(rename = "ID")]
    id: String,
    host_compute_network: String,
    mac_address: String,
    #[serde(default)]
    ip_configurations: Vec<EndpointIpConfiguration>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct EndpointIpConfiguration {
    ip_address: Ipv4Addr,
    prefix_length: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_DNS: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

    fn config() -> NetworkConfig {
        NetworkConfig {
            network_id: "11111111-2222-4333-8444-555555555555".to_string(),
            endpoint_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".to_string(),
            adapter_id: "01234567-89ab-4cde-8f01-23456789abcd".to_string(),
            guest_ip: Ipv4Addr::new(10, 0, 0, 2),
            prefix: 24,
            network: Ipv4Addr::new(10, 0, 0, 0),
            gateway: Ipv4Addr::new(10, 0, 0, 1),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            dns_servers: vec![DEFAULT_DNS],
            mac_address: Some("00-15-5D-52-C0-10".to_string()),
        }
    }

    fn descriptor(host_attached: bool) -> Vec<u8> {
        format!(
            r#"{{
              "version":1,
              "networkId":"11111111-2222-4333-8444-555555555555",
              "endpointId":"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
              "adapterId":"01234567-89ab-4cde-8f01-23456789abcd",
              "guestAddress":"10.0.0.2",
              "prefixLength":24,
              "networkAddress":"10.0.0.0",
              "gateway":"10.0.0.1",
              "dnsServers":["1.1.1.1"],
              "macAddress":"00-15-5D-52-C0-10",
              "hostAttached":{host_attached}
            }}"#
        )
        .into_bytes()
    }

    #[test]
    fn parses_static_ipv4_and_rejects_reserved_addresses() {
        let parsed = NetConfig::parse("10.0.0.2/24").unwrap();
        assert_eq!(parsed.network, Ipv4Addr::new(10, 0, 0, 0));
        assert_eq!(parsed.gateway, Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(parsed.netmask, Ipv4Addr::new(255, 255, 255, 0));
        assert!(NetConfig::parse("10.0.0.1/24").is_err());
        assert!(NetConfig::parse("10.0.0.255/24").is_err());
        assert!(NetConfig::parse("10.0.0.2/31").is_err());
    }

    #[test]
    fn parses_external_descriptor_and_rejects_host_attachment() {
        let expected = NetConfig::parse("10.0.0.2/24").unwrap();
        assert_eq!(
            NetworkConfig::from_external_document(&descriptor(false), Some(&expected)).unwrap(),
            config()
        );
        assert!(NetworkConfig::from_external_document(&descriptor(true), None).is_err());
        let different = NetConfig::parse("10.0.1.2/24").unwrap();
        assert!(
            NetworkConfig::from_external_document(&descriptor(false), Some(&different)).is_err()
        );
    }

    #[test]
    fn snapshot_config_validates_and_builds_guest_cmdline() {
        let mut config = config();
        config.validate().unwrap();
        assert_eq!(
            config.cmdline_fragment(config.mac_address.as_deref().unwrap()),
            "virtnet_required=1 virtnet_ip=10.0.0.2 virtnet_prefix=24 \
             virtnet_mask=255.255.255.0 virtnet_mac=00:15:5D:52:C0:10 virtnet_mtu=1500 \
             virtnet_routes=0.0.0.0/0@10.0.0.1 virtnet_dns=1.1.1.1 virtnet_search="
        );
        config.mac_address = Some("invalid".to_string());
        assert!(config.validate().is_err());
    }

    #[test]
    fn normalizes_hcn_mac_addresses() {
        assert_eq!(normalize_mac("00-15-5d-52-c0-10").unwrap(), "00155D52C010");
        assert_eq!(normalize_mac("00:15:5D:52:C0:10").unwrap(), "00155D52C010");
        assert!(normalize_mac("not-a-mac").is_err());
    }
}
