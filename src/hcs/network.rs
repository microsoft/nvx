// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! Owned HCN NAT network and endpoint lifecycle for an HCS VM.

use ::core::ffi::c_void;
use ::std::net::Ipv4Addr;
use ::std::ptr;
use ::std::thread;
use ::std::time::{Duration, Instant};

use ::anyhow::{Context, Result, anyhow, bail};
use ::log::{debug, warn};
use ::serde::{Deserialize, Serialize};
use ::windows::Win32::Foundation::{HCN_E_ENDPOINT_NOT_FOUND, HCN_E_NETWORK_NOT_FOUND};
use ::windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, GetIpNetTable2, MIB_IF_TABLE2, MIB_IPNET_TABLE2,
};
use ::windows::Win32::Networking::WinSock::AF_INET;
use ::windows::Win32::System::Com::CoTaskMemFree;
use ::windows::Win32::System::HostComputeNetwork::{
    HcnCloseEndpoint, HcnCloseNamespace, HcnCloseNetwork, HcnCreateEndpoint, HcnCreateNetwork,
    HcnDeleteEndpoint, HcnDeleteNetwork, HcnEnumerateNamespaces, HcnEnumerateNetworks,
    HcnModifyNamespace, HcnOpenNamespace, HcnOpenNetwork, HcnQueryEndpointProperties,
    HcnQueryNamespaceProperties, HcnQueryNetworkProperties,
};
use ::windows::core::{Error as WindowsError, GUID, HSTRING, PWSTR};

use crate::l2bridge::L2BridgeConfig;

const DEFAULT_DNS: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
const QUERY: &str = r#"{"SchemaVersion":{"Major":2,"Minor":0},"Flags":1}"#;
const MAC_POOL_START: &str = "00-15-5D-52-C0-00";
const MAC_POOL_END: &str = "00-15-5D-52-CF-FF";
const VNIC_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const VNIC_WAIT_INTERVAL: Duration = Duration::from_millis(50);

/// User-facing static IPv4 configuration parsed from `--net`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetConfig {
    guest_ip: Ipv4Addr,
    prefix: u8,
    network: Ipv4Addr,
    gateway: Ipv4Addr,
    netmask: Ipv4Addr,
    dns_servers: Vec<Ipv4Addr>,
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
        let network: Ipv4Addr = Ipv4Addr::from(u32::from(guest_ip) & mask);
        let gateway: Ipv4Addr = Ipv4Addr::from(u32::from(network) + 1);
        let broadcast: Ipv4Addr = Ipv4Addr::from(u32::from(network) | !mask);
        if guest_ip == network || guest_ip == broadcast || guest_ip == gateway {
            bail!("--net: guest IP {guest_ip} must be usable and differ from gateway {gateway}");
        }
        Ok(Self {
            guest_ip,
            prefix,
            network,
            gateway,
            netmask: Ipv4Addr::from(mask),
            dns_servers: vec![DEFAULT_DNS],
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
    pub fn new(config: NetConfig) -> Result<Self> {
        Ok(Self {
            network_id: new_guid()?,
            endpoint_id: new_guid()?,
            adapter_id: new_guid()?,
            guest_ip: config.guest_ip,
            prefix: config.prefix,
            network: config.network,
            gateway: config.gateway,
            netmask: config.netmask,
            dns_servers: config.dns_servers,
            mac_address: None,
        })
    }

    pub fn from_l2bridge(config: &L2BridgeConfig) -> Result<Self> {
        if !config.provisions_hcn_vnic() {
            bail!("L2Bridge manifest does not request HCN vNIC provisioning");
        }
        let net = NetConfig::parse(&format!(
            "{}/{}",
            config.guest_bootstrap.ipv4.address, config.guest_bootstrap.ipv4.prefix_length
        ))?;
        if config.guest_bootstrap.ipv4.gateway != net.gateway.to_string() {
            bail!(
                "HCN gateway {} does not match derived subnet gateway {}",
                config.guest_bootstrap.ipv4.gateway,
                net.gateway
            );
        }
        let mut network = Self::new(net)?;
        network.mac_address = Some(config.device.mac_address.clone());
        if !config.guest_bootstrap.dns.servers.is_empty() {
            network.dns_servers = config
                .guest_bootstrap
                .dns
                .servers
                .iter()
                .map(|server| {
                    server
                        .parse()
                        .with_context(|| format!("invalid HCN DNS server {server:?}"))
                })
                .collect::<Result<Vec<_>>>()?;
        }
        network.validate()?;
        Ok(network)
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
            bail!("HCN snapshot network addressing is inconsistent");
        }
        if self.dns_servers.is_empty() {
            bail!("HCN snapshot has no DNS servers");
        }
        if let Some(mac_address) = &self.mac_address {
            normalize_mac(mac_address)?;
        }
        Ok(())
    }

    pub fn cmdline_fragment(&self, mac_address: &str) -> String {
        let dns: String = self
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

    fn network_document(&self) -> Result<String> {
        let document = NetworkDocument {
            schema_version: Version::v2(),
            owner: "nvx",
            name: format!("nvx-{}", self.network_id),
            network_type: "NAT",
            mac_pool: MacPool {
                ranges: vec![MacRange {
                    start_mac_address: MAC_POOL_START,
                    end_mac_address: MAC_POOL_END,
                }],
            },
            ipams: vec![Ipam {
                ipam_type: "Static",
                subnets: vec![Subnet {
                    ip_address_prefix: format!("{}/{}", self.network, self.prefix),
                    routes: vec![Route {
                        destination_prefix: "0.0.0.0/0".to_string(),
                        next_hop: self.gateway.to_string(),
                    }],
                }],
            }],
        };
        ::serde_json::to_string(&document).context("serializing HCN network document")
    }

    fn endpoint_document(&self) -> Result<String> {
        let document = EndpointDocument {
            schema_version: Version::v2(),
            owner: "nvx",
            name: format!("nvx-{}-ep", self.endpoint_id),
            host_compute_network: self.network_id.clone(),
            ip_configurations: vec![IpConfig {
                ip_address: self.guest_ip.to_string(),
                prefix_length: self.prefix,
            }],
            routes: vec![Route {
                destination_prefix: "0.0.0.0/0".to_string(),
                next_hop: self.gateway.to_string(),
            }],
            dns: Dns {
                server_list: self.dns_servers.iter().map(Ipv4Addr::to_string).collect(),
                search: Vec::new(),
            },
            mac_address: self.mac_address.clone(),
        };
        ::serde_json::to_string(&document).context("serializing HCN endpoint document")
    }
}

/// Canonical endpoint information handed to the HCS network adapter schema.
pub struct Attachment<'a> {
    pub adapter_id: &'a str,
    pub endpoint_id: &'a str,
    pub mac_address: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostAttachment {
    pub interface_index: u32,
    pub interface_luid: u64,
    pub gateway_mac: [u8; 6],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InterfaceIdentity {
    interface_index: u32,
    interface_luid: u64,
}

/// Per-run owned HCN objects. Handles are closed before persistent objects are deleted.
pub struct OwnedNetwork {
    config: NetworkConfig,
    network_id: GUID,
    endpoint_id: GUID,
    network_handle: Option<*mut c_void>,
    endpoint_handle: Option<*mut c_void>,
    host_namespace_id: Option<GUID>,
    host_attached: bool,
    network_created: bool,
    endpoint_created: bool,
}

impl OwnedNetwork {
    pub fn create(config: NetworkConfig) -> Result<Self> {
        config.validate()?;
        ensure_no_overlap(&config)?;
        let network_id = GUID::try_from(config.network_id.as_str())?;
        let endpoint_id = GUID::try_from(config.endpoint_id.as_str())?;
        let mut owned = Self {
            config,
            network_id,
            endpoint_id,
            network_handle: None,
            endpoint_handle: None,
            host_namespace_id: None,
            host_attached: false,
            network_created: false,
            endpoint_created: false,
        };

        let network_document: String = owned.config.network_document()?;
        let mut network_handle: *mut c_void = ptr::null_mut();
        let mut error_record: PWSTR = PWSTR::null();
        let network_settings: HSTRING = HSTRING::from(&network_document);
        // SAFETY: IDs, settings, handle out-pointer, and error out-pointer remain valid.
        let result = unsafe {
            HcnCreateNetwork(
                &owned.network_id,
                &network_settings,
                &mut network_handle,
                Some(&mut error_record),
            )
        };
        hcn_result(
            "HcnCreateNetwork",
            &owned.config.network_id,
            result,
            error_record,
        )
        .with_context(|| format!("HCN network settings: {network_document}"))?;
        if network_handle.is_null() {
            bail!("HcnCreateNetwork returned a null handle");
        }
        owned.network_handle = Some(network_handle);
        owned.network_created = true;
        debug!(
            "created HCN network {}: {network_document}",
            owned.config.network_id
        );
        owned.query_network()?;

        let endpoint_document: String = owned.config.endpoint_document()?;
        let mut endpoint_handle: *mut c_void = ptr::null_mut();
        let mut error_record: PWSTR = PWSTR::null();
        let endpoint_settings: HSTRING = HSTRING::from(&endpoint_document);
        // SAFETY: The network handle is live and all out-pointers remain valid.
        let result = unsafe {
            HcnCreateEndpoint(
                network_handle,
                &owned.endpoint_id,
                &endpoint_settings,
                &mut endpoint_handle,
                Some(&mut error_record),
            )
        };
        hcn_result(
            "HcnCreateEndpoint",
            &owned.config.endpoint_id,
            result,
            error_record,
        )
        .with_context(|| format!("HCN endpoint settings: {endpoint_document}"))?;
        if endpoint_handle.is_null() {
            bail!("HcnCreateEndpoint returned a null handle");
        }
        owned.endpoint_handle = Some(endpoint_handle);
        owned.endpoint_created = true;
        debug!(
            "created HCN endpoint {}: {endpoint_document}",
            owned.config.endpoint_id
        );
        let properties: EndpointProperties = owned.query_endpoint()?;
        if !properties
            .id
            .eq_ignore_ascii_case(&owned.config.endpoint_id)
        {
            bail!(
                "HCN endpoint query returned ID {:?}, expected {:?}",
                properties.id,
                owned.config.endpoint_id
            );
        }
        if properties.mac_address.is_empty() {
            bail!("HCN endpoint query returned no MAC address");
        }
        if let Some(requested_mac) = owned.config.mac_address.as_deref()
            && normalize_mac(requested_mac)? != normalize_mac(&properties.mac_address)?
        {
            bail!(
                "HCN restored endpoint MAC {:?} differs from snapshot MAC {:?}",
                properties.mac_address,
                requested_mac
            );
        }
        normalize_mac(&properties.mac_address)?;
        owned.config.mac_address = Some(properties.mac_address);
        Ok(owned)
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

    pub fn attach_to_host(&mut self) -> Result<HostAttachment> {
        if self.host_attached {
            bail!("HCN endpoint {} is already host-attached", self.config.endpoint_id);
        }
        let namespace_id = host_default_namespace()?;
        modify_namespace_endpoint(&namespace_id, &self.config.endpoint_id, "Add")?;
        self.host_namespace_id = Some(namespace_id);
        self.host_attached = true;

        let mac_address = self
            .config
            .mac_address
            .as_deref()
            .context("queried HCN endpoint has no MAC address")?;
        let identity = wait_for_vnic(mac_address, &self.config.endpoint_id).with_context(|| {
            format!(
                "resolving host vNIC for HCN endpoint {} with MAC {}",
                self.config.endpoint_id, mac_address
            )
        })?;
        let gateway_mac = wait_for_gateway_mac(identity.interface_index, self.config.gateway)
            .with_context(|| {
                format!(
                    "resolving HCN gateway {} on host vNIC index {}",
                    self.config.gateway, identity.interface_index
                )
            })?;
        let attachment = HostAttachment {
            interface_index: identity.interface_index,
            interface_luid: identity.interface_luid,
            gateway_mac,
        };
        debug!(
            "attached HCN endpoint {} as host interface index {} LUID {} gateway MAC {}",
            self.config.endpoint_id,
            attachment.interface_index,
            attachment.interface_luid,
            format_mac(&attachment.gateway_mac),
        );
        Ok(attachment)
    }

    pub fn cleanup(&mut self) -> Result<()> {
        let mut failures: Vec<String> = Vec::new();
        if self.host_attached {
            match self.host_namespace_id.as_ref() {
                Some(namespace_id) => {
                    if let Err(error) =
                        modify_namespace_endpoint(namespace_id, &self.config.endpoint_id, "Remove")
                    {
                        failures.push(error.to_string());
                    } else {
                        self.host_attached = false;
                        self.host_namespace_id = None;
                    }
                }
                None => failures.push("host-attached HCN endpoint has no namespace ID".to_string()),
            }
        }
        if let Some(handle) = self.endpoint_handle.take()
            && let Err(error) = unsafe { HcnCloseEndpoint(handle) }
        {
            failures.push(format!("HcnCloseEndpoint: {error}"));
        }
        if self.endpoint_created {
            let mut error_record: PWSTR = PWSTR::null();
            let result = unsafe { HcnDeleteEndpoint(&self.endpoint_id, Some(&mut error_record)) };
            if let Err(error) = hcn_delete_result(
                "HcnDeleteEndpoint",
                &self.config.endpoint_id,
                result,
                error_record,
                HCN_E_ENDPOINT_NOT_FOUND,
            ) {
                failures.push(error.to_string());
            } else {
                self.endpoint_created = false;
            }
        }
        if let Some(handle) = self.network_handle.take()
            && let Err(error) = unsafe { HcnCloseNetwork(handle) }
        {
            failures.push(format!("HcnCloseNetwork: {error}"));
        }
        if self.network_created {
            let mut error_record: PWSTR = PWSTR::null();
            let result = unsafe { HcnDeleteNetwork(&self.network_id, Some(&mut error_record)) };
            if let Err(error) = hcn_delete_result(
                "HcnDeleteNetwork",
                &self.config.network_id,
                result,
                error_record,
                HCN_E_NETWORK_NOT_FOUND,
            ) {
                failures.push(error.to_string());
            } else {
                self.network_created = false;
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(failures.join("; ")))
        }
    }

    fn query_network(&self) -> Result<()> {
        let handle = self
            .network_handle
            .context("HCN network handle is missing")?;
        let mut properties: PWSTR = PWSTR::null();
        let mut error_record: PWSTR = PWSTR::null();
        let query: HSTRING = HSTRING::from(QUERY);
        let result = unsafe {
            HcnQueryNetworkProperties(handle, &query, &mut properties, Some(&mut error_record))
        };
        hcn_result(
            "HcnQueryNetworkProperties",
            &self.config.network_id,
            result,
            error_record,
        )?;
        let document: String = CoTaskWideString::new(properties).to_string()?;
        debug!("queried HCN network {}: {document}", self.config.network_id);
        let properties: NetworkProperties =
            ::serde_json::from_str(&document).context("parsing HCN network properties")?;
        if !properties.id.eq_ignore_ascii_case(&self.config.network_id) {
            bail!(
                "HCN network query returned ID {:?}, expected {:?}",
                properties.id,
                self.config.network_id
            );
        }
        Ok(())
    }

    fn query_endpoint(&self) -> Result<EndpointProperties> {
        let handle = self
            .endpoint_handle
            .context("HCN endpoint handle is missing")?;
        let mut properties: PWSTR = PWSTR::null();
        let mut error_record: PWSTR = PWSTR::null();
        let query: HSTRING = HSTRING::from(QUERY);
        let result = unsafe {
            HcnQueryEndpointProperties(handle, &query, &mut properties, Some(&mut error_record))
        };
        hcn_result(
            "HcnQueryEndpointProperties",
            &self.config.endpoint_id,
            result,
            error_record,
        )?;
        let document: String = CoTaskWideString::new(properties).to_string()?;
        debug!(
            "queried HCN endpoint {}: {document}",
            self.config.endpoint_id
        );
        ::serde_json::from_str(&document).context("parsing HCN endpoint properties")
    }
}

impl Drop for OwnedNetwork {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            warn!("last-resort HCN cleanup failed: {error:#}");
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
    let record: String = CoTaskWideString::new(error_record)
        .to_string()
        .unwrap_or_else(|error| format!("<failed to decode HCN error: {error:#}>"));
    result.map_err(|error: WindowsError| {
        let code: u32 = error.code().0 as u32;
        anyhow!(
            "{operation} failed for {id}: {error} (HRESULT 0x{code:08X}); error record: {}",
            if record.is_empty() { "<none>" } else { &record }
        )
    })
}

fn hcn_delete_result(
    operation: &str,
    id: &str,
    result: ::windows::core::Result<()>,
    error_record: PWSTR,
    not_found: ::windows::core::HRESULT,
) -> Result<()> {
    if result
        .as_ref()
        .is_err_and(|error| error.code() == not_found)
    {
        drop(CoTaskWideString::new(error_record));
        return Ok(());
    }
    hcn_result(operation, id, result, error_record)
}

fn new_guid() -> Result<String> {
    Ok(format!("{:?}", GUID::new()?).to_ascii_lowercase())
}

fn normalize_mac(value: &str) -> Result<String> {
    let normalized: String = value
        .chars()
        .filter(|character| !matches!(character, ':' | '-'))
        .flat_map(char::to_uppercase)
        .collect();
    if normalized.len() != 12 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid HCN MAC address {value:?}");
    }
    Ok(normalized)
}

fn host_default_namespace() -> Result<GUID> {
    let mut identifiers: PWSTR = PWSTR::null();
    let mut error_record: PWSTR = PWSTR::null();
    let query: HSTRING = HSTRING::from(QUERY);
    let result =
        unsafe { HcnEnumerateNamespaces(&query, &mut identifiers, Some(&mut error_record)) };
    hcn_result(
        "HcnEnumerateNamespaces",
        "all namespaces",
        result,
        error_record,
    )?;
    let document = CoTaskWideString::new(identifiers).to_string()?;
    let identifiers: Vec<String> =
        ::serde_json::from_str(&document).context("parsing HCN namespace ID enumeration")?;

    for identifier in identifiers {
        let namespace_id = GUID::try_from(identifier.as_str())
            .with_context(|| format!("parsing enumerated HCN namespace ID {identifier:?}"))?;
        let properties = query_namespace_properties(&namespace_id, &identifier)?;
        if properties.namespace_type == "HostDefault" {
            return Ok(namespace_id);
        }
    }
    bail!("HCN did not report a HostDefault namespace")
}

fn query_namespace_properties(namespace_id: &GUID, identifier: &str) -> Result<NamespaceProperties> {
    let mut handle: *mut c_void = ptr::null_mut();
    let mut error_record: PWSTR = PWSTR::null();
    let result = unsafe { HcnOpenNamespace(namespace_id, &mut handle, Some(&mut error_record)) };
    hcn_result("HcnOpenNamespace", identifier, result, error_record)?;
    if handle.is_null() {
        bail!("HcnOpenNamespace returned a null handle for {identifier}");
    }

    let mut properties: PWSTR = PWSTR::null();
    let mut error_record: PWSTR = PWSTR::null();
    let query: HSTRING = HSTRING::from(QUERY);
    let query_result = unsafe {
        HcnQueryNamespaceProperties(handle, &query, &mut properties, Some(&mut error_record))
    };
    let query_result = hcn_result(
        "HcnQueryNamespaceProperties",
        identifier,
        query_result,
        error_record,
    );
    let close_result = unsafe { HcnCloseNamespace(handle) }
        .with_context(|| format!("HcnCloseNamespace failed for {identifier}"));
    query_result?;
    close_result?;

    let document = CoTaskWideString::new(properties).to_string()?;
    let properties: NamespaceProperties = ::serde_json::from_str(&document)
        .with_context(|| format!("parsing HCN namespace properties for {identifier}"))?;
    if !properties.id.eq_ignore_ascii_case(identifier) {
        bail!(
            "HCN namespace query returned ID {:?}, expected {:?}",
            properties.id,
            identifier
        );
    }
    Ok(properties)
}

fn modify_namespace_endpoint(namespace_id: &GUID, endpoint_id: &str, request: &str) -> Result<()> {
    let identifier = format!("{namespace_id:?}");
    let mut handle: *mut c_void = ptr::null_mut();
    let mut error_record: PWSTR = PWSTR::null();
    let result = unsafe { HcnOpenNamespace(namespace_id, &mut handle, Some(&mut error_record)) };
    hcn_result("HcnOpenNamespace", &identifier, result, error_record)?;
    if handle.is_null() {
        bail!("HcnOpenNamespace returned a null handle for {identifier}");
    }

    let document = namespace_endpoint_document(endpoint_id, request)?;
    let settings: HSTRING = HSTRING::from(&document);
    let mut error_record: PWSTR = PWSTR::null();
    let modify_result = unsafe { HcnModifyNamespace(handle, &settings, Some(&mut error_record)) };
    let modify_result = hcn_result(
        "HcnModifyNamespace",
        &identifier,
        modify_result,
        error_record,
    )
    .with_context(|| format!("HCN namespace settings: {document}"));
    let close_result = unsafe { HcnCloseNamespace(handle) }
        .with_context(|| format!("HcnCloseNamespace failed for {identifier}"));
    modify_result?;
    close_result
}

fn namespace_endpoint_document(endpoint_id: &str, request: &str) -> Result<String> {
    ::serde_json::to_string(&ModifyNamespaceSettingRequest {
        resource_type: "Endpoint",
        request_type: request,
        settings: NamespaceEndpointSetting { endpoint_id },
    })
    .context("serializing HCN namespace endpoint request")
}

fn wait_for_vnic(mac_address: &str, endpoint_id: &str) -> Result<InterfaceIdentity> {
    let expected_mac = mac_bytes(mac_address)?;
    let expected_alias = host_vnic_alias(endpoint_id);
    let deadline = Instant::now() + VNIC_WAIT_TIMEOUT;
    loop {
        let error = match vnic_identity(&expected_mac, &expected_alias) {
            Ok(attachment) => return Ok(attachment),
            Err(error) => error,
        };
        if Instant::now() >= deadline {
            return Err(error);
        }
        thread::sleep(VNIC_WAIT_INTERVAL);
    }
}

struct OwnedIfTable(*mut MIB_IF_TABLE2);

impl Drop for OwnedIfTable {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { FreeMibTable(self.0.cast()) };
        }
    }
}

fn vnic_identity(expected_mac: &[u8; 6], expected_alias: &str) -> Result<InterfaceIdentity> {
    let mut table = ptr::null_mut();
    let status = unsafe { GetIfTable2(&mut table) };
    if status.0 != 0 {
        bail!("GetIfTable2 failed with Win32 status {}", status.0);
    }
    if table.is_null() {
        bail!("GetIfTable2 returned a null table");
    }
    let table = OwnedIfTable(table);
    let rows = unsafe {
        ::std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize)
    };
    let mut candidates = Vec::new();
    let mut details = Vec::new();
    for row in rows {
        if row.PhysicalAddressLength != 6 || row.InterfaceIndex == 0 {
            continue;
        }
        let mut mac = [0; 6];
        mac.copy_from_slice(&row.PhysicalAddress[..6]);
        if &mac == expected_mac {
            let interface_luid = unsafe { row.InterfaceLuid.Value };
            let alias = utf16z(&row.Alias);
            if alias == expected_alias {
                candidates.push((row.InterfaceIndex, interface_luid, mac));
            }
            details.push(format!(
                "ifIndex={} luid={} interfaceGuid={:?} networkGuid={:?} alias={:?} description={:?} type={} operStatus={:?}",
                row.InterfaceIndex,
                interface_luid,
                row.InterfaceGuid,
                row.NetworkGuid,
                alias,
                utf16z(&row.Description),
                row.Type,
                row.OperStatus,
            ));
        }
    }
    select_vnic(expected_mac, candidates).with_context(|| {
        format!(
            "expected base adapter alias {:?}; matching IP Helper rows: {}",
            expected_alias,
            if details.is_empty() {
                "<none>".to_string()
            } else {
                details.join("; ")
            }
        )
    })
}

fn host_vnic_alias(endpoint_id: &str) -> String {
    format!("vEthernet (nvx-{endpoint_id}-ep)")
}

fn utf16z(value: &[u16]) -> String {
    let length = value.iter().position(|character| *character == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..length])
}

fn select_vnic(
    expected_mac: &[u8; 6],
    interfaces: impl IntoIterator<Item = (u32, u64, [u8; 6])>,
) -> Result<InterfaceIdentity> {
    let mut found = None;
    for (interface_index, interface_luid, mac) in interfaces {
        if &mac != expected_mac {
            continue;
        }
        if found.is_some() {
            bail!(
                "multiple host interfaces expose HCN endpoint MAC {}",
                format_mac(expected_mac)
            );
        }
        found = Some(InterfaceIdentity {
            interface_index,
            interface_luid,
        });
    }
    found.with_context(|| {
        format!(
            "no host interface exposes HCN endpoint MAC {}",
            format_mac(expected_mac)
        )
    })
}

struct OwnedIpNetTable(*mut MIB_IPNET_TABLE2);

impl Drop for OwnedIpNetTable {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { FreeMibTable(self.0.cast()) };
        }
    }
}

fn wait_for_gateway_mac(interface_index: u32, gateway: Ipv4Addr) -> Result<[u8; 6]> {
    let deadline = Instant::now() + VNIC_WAIT_TIMEOUT;
    loop {
        let error = match gateway_mac(interface_index, gateway) {
            Ok(mac) => return Ok(mac),
            Err(error) => error,
        };
        if Instant::now() >= deadline {
            return Err(error);
        }
        thread::sleep(VNIC_WAIT_INTERVAL);
    }
}

fn gateway_mac(interface_index: u32, gateway: Ipv4Addr) -> Result<[u8; 6]> {
    let mut table = ptr::null_mut();
    let status = unsafe { GetIpNetTable2(AF_INET, &mut table) };
    if status.0 != 0 {
        bail!("GetIpNetTable2 failed with Win32 status {}", status.0);
    }
    if table.is_null() {
        bail!("GetIpNetTable2 returned a null table");
    }
    let table = OwnedIpNetTable(table);
    let rows = unsafe {
        ::std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize)
    };
    for row in rows {
        if row.InterfaceIndex != interface_index || row.PhysicalAddressLength != 6 {
            continue;
        }
        let address = unsafe { row.Address.Ipv4 };
        if address.sin_family != AF_INET {
            continue;
        }
        let octets = unsafe { address.sin_addr.S_un.S_un_b };
        if Ipv4Addr::new(octets.s_b1, octets.s_b2, octets.s_b3, octets.s_b4) != gateway {
            continue;
        }
        let mut mac = [0; 6];
        mac.copy_from_slice(&row.PhysicalAddress[..6]);
        if mac == [0; 6] {
            bail!("HCN gateway neighbor {gateway} has an empty MAC address");
        }
        return Ok(mac);
    }
    bail!("HCN gateway neighbor {gateway} is not present on interface {interface_index}")
}

fn mac_bytes(value: &str) -> Result<[u8; 6]> {
    let normalized = normalize_mac(value)?;
    let mut mac = [0; 6];
    for (index, byte) in mac.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&normalized[index * 2..index * 2 + 2], 16)
            .context("decoding normalized HCN MAC address")?;
    }
    Ok(mac)
}

fn format_mac(value: &[u8; 6]) -> String {
    value
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join("-")
}

fn ensure_no_overlap(config: &NetworkConfig) -> Result<()> {
    let mut identifiers: PWSTR = PWSTR::null();
    let mut error_record: PWSTR = PWSTR::null();
    let query: HSTRING = HSTRING::from(QUERY);
    let result = unsafe { HcnEnumerateNetworks(&query, &mut identifiers, Some(&mut error_record)) };
    hcn_result("HcnEnumerateNetworks", "all networks", result, error_record)?;
    let document: String = CoTaskWideString::new(identifiers).to_string()?;
    let identifiers: Vec<String> =
        ::serde_json::from_str(&document).context("parsing HCN network ID enumeration")?;
    let requested = ipv4_range(config.network, config.prefix);

    for identifier in identifiers {
        if identifier.eq_ignore_ascii_case(&config.network_id) {
            bail!("an HCN network with requested ID {identifier} already exists");
        }
        let guid = GUID::try_from(identifier.as_str())
            .with_context(|| format!("parsing enumerated HCN network ID {identifier:?}"))?;
        let mut handle: *mut c_void = ptr::null_mut();
        let mut error_record: PWSTR = PWSTR::null();
        let result = unsafe { HcnOpenNetwork(&guid, &mut handle, Some(&mut error_record)) };
        hcn_result("HcnOpenNetwork", &identifier, result, error_record)?;
        if handle.is_null() {
            bail!("HcnOpenNetwork returned a null handle for {identifier}");
        }
        let properties = query_network_properties(handle, &identifier);
        unsafe { HcnCloseNetwork(handle) }
            .with_context(|| format!("HcnCloseNetwork failed for {identifier}"))?;
        let properties = properties?;
        for ipam in properties.ipams {
            for subnet in ipam.subnets {
                let Some((address, prefix)) = parse_prefix(&subnet.ip_address_prefix) else {
                    continue;
                };
                if ranges_overlap(requested, ipv4_range(address, prefix)) {
                    bail!(
                        "requested HCN prefix {}/{} overlaps network {} prefix {}",
                        config.network,
                        config.prefix,
                        identifier,
                        subnet.ip_address_prefix
                    );
                }
            }
        }
    }
    Ok(())
}

fn query_network_properties(handle: *mut c_void, identifier: &str) -> Result<NetworkProperties> {
    let mut properties: PWSTR = PWSTR::null();
    let mut error_record: PWSTR = PWSTR::null();
    let query: HSTRING = HSTRING::from(QUERY);
    let result = unsafe {
        HcnQueryNetworkProperties(handle, &query, &mut properties, Some(&mut error_record))
    };
    hcn_result(
        "HcnQueryNetworkProperties",
        identifier,
        result,
        error_record,
    )?;
    let document: String = CoTaskWideString::new(properties).to_string()?;
    ::serde_json::from_str(&document)
        .with_context(|| format!("parsing HCN network properties for {identifier}"))
}

fn parse_prefix(value: &str) -> Option<(Ipv4Addr, u8)> {
    let (address, prefix) = value.split_once('/')?;
    let address: Ipv4Addr = address.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    (prefix <= 32).then_some((address, prefix))
}

fn ipv4_range(address: Ipv4Addr, prefix: u8) -> (u32, u32) {
    let mask: u32 = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let start: u32 = u32::from(address) & mask;
    (start, start | !mask)
}

fn ranges_overlap(left: (u32, u32), right: (u32, u32)) -> bool {
    left.0 <= right.1 && right.0 <= left.1
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct NetworkDocument {
    schema_version: Version,
    owner: &'static str,
    name: String,
    #[serde(rename = "Type")]
    network_type: &'static str,
    mac_pool: MacPool,
    ipams: Vec<Ipam>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct MacPool {
    ranges: Vec<MacRange>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct MacRange {
    start_mac_address: &'static str,
    end_mac_address: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct EndpointDocument {
    schema_version: Version,
    owner: &'static str,
    name: String,
    host_compute_network: String,
    ip_configurations: Vec<IpConfig>,
    routes: Vec<Route>,
    dns: Dns,
    #[serde(skip_serializing_if = "Option::is_none")]
    mac_address: Option<String>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Version {
    major: u32,
    minor: u32,
}

impl Version {
    fn v2() -> Self {
        Self { major: 2, minor: 0 }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Ipam {
    #[serde(rename = "Type")]
    ipam_type: &'static str,
    subnets: Vec<Subnet>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Subnet {
    ip_address_prefix: String,
    routes: Vec<Route>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct IpConfig {
    ip_address: String,
    prefix_length: u8,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Route {
    destination_prefix: String,
    next_hop: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Dns {
    server_list: Vec<String>,
    search: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ModifyNamespaceSettingRequest<'a> {
    resource_type: &'static str,
    request_type: &'a str,
    settings: NamespaceEndpointSetting<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct NamespaceEndpointSetting<'a> {
    endpoint_id: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct EndpointProperties {
    #[serde(rename = "ID")]
    id: String,
    mac_address: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NamespaceProperties {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Type")]
    namespace_type: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NetworkProperties {
    #[serde(rename = "ID")]
    id: String,
    #[serde(default)]
    ipams: Vec<IpamProperties>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct IpamProperties {
    #[serde(default)]
    subnets: Vec<SubnetProperties>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SubnetProperties {
    ip_address_prefix: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
            mac_address: None,
        }
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
    fn serializes_hcn_network_and_endpoint_documents() {
        let config = config();
        let network: ::serde_json::Value =
            ::serde_json::from_str(&config.network_document().unwrap()).unwrap();
        assert_eq!(network["Type"], "NAT");
        assert!(network.get("Flags").is_none());
        assert_eq!(
            network["MacPool"]["Ranges"][0]["StartMacAddress"],
            MAC_POOL_START
        );
        assert_eq!(
            network["MacPool"]["Ranges"][0]["EndMacAddress"],
            MAC_POOL_END
        );
        assert_eq!(
            network["Ipams"][0]["Subnets"][0]["IpAddressPrefix"],
            "10.0.0.0/24"
        );
        assert_eq!(
            network["Ipams"][0]["Subnets"][0]["Routes"][0]["NextHop"],
            "10.0.0.1"
        );

        let endpoint: ::serde_json::Value =
            ::serde_json::from_str(&config.endpoint_document().unwrap()).unwrap();
        assert_eq!(endpoint["HostComputeNetwork"], config.network_id);
        assert_eq!(endpoint["IpConfigurations"][0]["IpAddress"], "10.0.0.2");
        assert!(endpoint.get("MacAddress").is_none());
    }

    #[test]
    fn snapshot_config_validates_and_builds_guest_cmdline() {
        let mut config = config();
        config.mac_address = Some("00-15-5D-52-C0-10".to_string());
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
        assert_eq!(
            mac_bytes("00:15:5D:52:C0:10").unwrap(),
            [0x00, 0x15, 0x5d, 0x52, 0xc0, 0x10]
        );
        assert!(normalize_mac("not-a-mac").is_err());
    }

    #[test]
    fn selects_unique_host_vnic_by_mac() {
        let expected = [0x00, 0x15, 0x5d, 0x52, 0xc0, 0x10];
        let attachment = select_vnic(
            &expected,
            [
                (7, 70, [0x00, 0x15, 0x5d, 0x52, 0xc0, 0x11]),
                (8, 80, expected),
            ],
        )
        .unwrap();
        assert_eq!(attachment.interface_index, 8);
        assert_eq!(attachment.interface_luid, 80);
        assert!(select_vnic(&expected, []).is_err());
        assert!(select_vnic(&expected, [(8, 80, expected), (9, 90, expected)]).is_err());
        assert_eq!(
            host_vnic_alias("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"),
            "vEthernet (nvx-aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee-ep)"
        );
    }

    #[test]
    fn serializes_host_namespace_endpoint_requests() {
        let value: ::serde_json::Value = ::serde_json::from_str(
            &namespace_endpoint_document("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee", "Add")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value["ResourceType"], "Endpoint");
        assert_eq!(value["RequestType"], "Add");
        assert_eq!(
            value["Settings"]["EndpointId"],
            "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
        );
    }

    #[test]
    fn detects_ipv4_prefix_overlap() {
        assert!(ranges_overlap(
            ipv4_range(Ipv4Addr::new(10, 0, 0, 0), 24),
            ipv4_range(Ipv4Addr::new(10, 0, 0, 128), 25)
        ));
        assert!(!ranges_overlap(
            ipv4_range(Ipv4Addr::new(10, 0, 0, 0), 24),
            ipv4_range(Ipv4Addr::new(10, 0, 1, 0), 24)
        ));
        assert_eq!(
            parse_prefix("10.0.0.0/24"),
            Some((Ipv4Addr::new(10, 0, 0, 0), 24))
        );
        assert_eq!(parse_prefix("2001:db8::/64"), None);
    }
}
