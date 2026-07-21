// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! Minimal typed models for HCS JSON documents.

use ::std::collections::BTreeMap;

use ::anyhow::{Context, Result, bail};
use ::serde::{Deserialize, Serialize};

/// HCS schema version advertised by the host service.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct ComputeSystem {
    owner: &'static str,
    schema_version: Version,
    should_terminate_on_last_handle_closed: bool,
    virtual_machine: VirtualMachine,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct VirtualMachine {
    stop_on_reset: bool,
    chipset: Chipset,
    compute_topology: Topology,
    devices: Devices,
    #[serde(skip_serializing_if = "Option::is_none")]
    restore_state: Option<RestoreState>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Chipset {
    linux_kernel_direct: LinuxKernelDirect,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct LinuxKernelDirect {
    kernel_file_path: String,
    init_rd_path: String,
    kernel_cmd_line: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Topology {
    memory: Memory,
    processor: Processor,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Memory {
    #[serde(rename = "SizeInMB")]
    size_in_mb: u64,
    allow_overcommit: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Processor {
    count: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Devices {
    com_ports: BTreeMap<String, ComPort>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    network_adapters: BTreeMap<String, NetworkAdapter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan9: Option<Plan9>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hv_socket: Option<HvSocket>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct ComPort {
    named_pipe: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct NetworkAdapter {
    endpoint_id: String,
    mac_address: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Plan9 {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    shares: Vec<Plan9Share>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Plan9Share {
    name: String,
    access_name: String,
    path: String,
    port: u32,
    flags: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct HvSocket {
    hv_socket_config: HvSocketConfig,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct HvSocketConfig {
    default_bind_security_descriptor: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct ModifySettingRequest {
    request_type: &'static str,
    resource_path: String,
    settings: NetworkAdapter,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Plan9ModifySettingRequest {
    request_type: &'static str,
    resource_path: &'static str,
    settings: Plan9Share,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct RestoreState {
    save_state_file_path: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct PauseOptions {
    suspension_level: &'static str,
    hosted_notification: PauseNotification,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct PauseNotification {
    reason: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct SaveOptions {
    save_type: &'static str,
    save_state_file_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BasicInformation {
    supported_schema_versions: Vec<Version>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ServiceProperties {
    properties: Vec<BasicInformation>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SystemExitStatus {
    status: i32,
    exit_type: String,
}

#[derive(Default)]
pub struct ComputeSystemOptions<'a> {
    pub control_pipe: Option<&'a str>,
    pub restore_state: Option<&'a str>,
    pub network_adapter: Option<(&'a str, &'a str, &'a str)>,
    pub plan9_share: Option<(&'a str, bool)>,
}

/// Parses the schema versions returned by a basic HCS service-property query.
pub fn supported_versions(document: &str) -> Result<Vec<Version>> {
    let properties: ServiceProperties =
        ::serde_json::from_str(document).context("parsing HCS basic service properties")?;
    let Some(basic) = properties.properties.into_iter().next() else {
        bail!("HCS basic service properties did not include a Properties entry");
    };
    if basic.supported_schema_versions.is_empty() {
        bail!("HCS basic service properties advertised no schema versions");
    }
    Ok(basic.supported_schema_versions)
}

/// Serializes the minimal schema 2.2 kernel-direct VM document.
pub fn compute_system_document(
    kernel: &str,
    initrd: &str,
    cmdline: &str,
    memory_mib: u64,
    console_pipe: &str,
    options: ComputeSystemOptions<'_>,
) -> Result<String> {
    let mut com_ports: BTreeMap<String, ComPort> = BTreeMap::new();
    com_ports.insert(
        "0".to_string(),
        ComPort {
            named_pipe: console_pipe.to_string(),
        },
    );
    if let Some(control_pipe) = options.control_pipe {
        com_ports.insert(
            "1".to_string(),
            ComPort {
                named_pipe: control_pipe.to_string(),
            },
        );
    }
    let mut network_adapters: BTreeMap<String, NetworkAdapter> = BTreeMap::new();
    if options.restore_state.is_none()
        && let Some((adapter_id, endpoint_id, mac_address)) = options.network_adapter
    {
        network_adapters.insert(
            adapter_id.to_string(),
            NetworkAdapter {
                endpoint_id: endpoint_id.to_string(),
                mac_address: mac_address.to_string(),
            },
        );
    }
    let (plan9, hv_socket) = match options.plan9_share {
        Some(_) => (
            Some(Plan9 { shares: Vec::new() }),
            Some(HvSocket {
                hv_socket_config: HvSocketConfig {
                    default_bind_security_descriptor: "D:P(A;;FA;;;SY)(A;;FA;;;BA)",
                },
            }),
        ),
        None => (None, None),
    };
    let document = ComputeSystem {
        owner: "nvx",
        schema_version: Version { major: 2, minor: 2 },
        should_terminate_on_last_handle_closed: true,
        virtual_machine: VirtualMachine {
            stop_on_reset: true,
            chipset: Chipset {
                linux_kernel_direct: LinuxKernelDirect {
                    kernel_file_path: kernel.to_string(),
                    init_rd_path: initrd.to_string(),
                    kernel_cmd_line: cmdline.to_string(),
                },
            },
            compute_topology: Topology {
                memory: Memory {
                    size_in_mb: memory_mib,
                    allow_overcommit: true,
                },
                processor: Processor { count: 1 },
            },
            devices: Devices {
                com_ports,
                network_adapters,
                plan9,
                hv_socket,
            },
            restore_state: options.restore_state.map(|path| RestoreState {
                save_state_file_path: path.to_string(),
            }),
        },
    };
    ::serde_json::to_string(&document).context("serializing HCS compute-system document")
}

/// Serializes the suspend-level pause required before saving an HCS VM.
pub fn pause_options() -> Result<String> {
    ::serde_json::to_string(&PauseOptions {
        suspension_level: "Suspend",
        hosted_notification: PauseNotification { reason: "Save" },
    })
    .context("serializing HCS pause options")
}

/// Serializes an HCS save-to-file request.
pub fn save_options(path: &str) -> Result<String> {
    ::serde_json::to_string(&SaveOptions {
        save_type: "ToFile",
        save_state_file_path: path.to_string(),
    })
    .context("serializing HCS save options")
}

/// Serializes removal of an HCN-backed network adapter from a live HCS system.
pub fn network_adapter_remove(
    adapter_id: &str,
    endpoint_id: &str,
    mac_address: &str,
) -> Result<String> {
    network_adapter_modify("Remove", adapter_id, endpoint_id, mac_address)
}

/// Serializes recreation of a restored network adapter with its stable identity.
pub fn network_adapter_add(
    adapter_id: &str,
    endpoint_id: &str,
    mac_address: &str,
) -> Result<String> {
    network_adapter_modify("Add", adapter_id, endpoint_id, mac_address)
}

/// Serializes hot-addition of a host directory to the running HCS Plan9 provider.
pub fn plan9_share_add(name: &str, path: &str, read_only: bool) -> Result<String> {
    ::serde_json::to_string(&Plan9ModifySettingRequest {
        request_type: "Add",
        resource_path: "VirtualMachine/Devices/Plan9/Shares",
        settings: Plan9Share {
            name: name.to_string(),
            access_name: name.to_string(),
            path: path.to_string(),
            port: 564,
            flags: 0x0000_0004 | u32::from(read_only),
        },
    })
    .context("serializing HCS Plan9 share addition")
}

fn network_adapter_modify(
    request_type: &'static str,
    adapter_id: &str,
    endpoint_id: &str,
    mac_address: &str,
) -> Result<String> {
    ::serde_json::to_string(&ModifySettingRequest {
        request_type,
        resource_path: format!("VirtualMachine/Devices/NetworkAdapters/{adapter_id}"),
        settings: NetworkAdapter {
            endpoint_id: endpoint_id.to_string(),
            mac_address: mac_address.to_string(),
        },
    })
    .context("serializing HCS network-adapter removal")
}

/// Rejects failed or unexpected natural compute-system exits.
pub fn validate_exit_document(document: &str) -> Result<()> {
    let status: SystemExitStatus =
        ::serde_json::from_str(document).context("parsing HCS system exit status")?;
    if status.status != 0 || status.exit_type != "GracefulExit" {
        bail!(
            "HCS guest exited unsuccessfully: status=0x{:08X}, type={}; document: {}",
            status.status as u32,
            status.exit_type,
            document
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_service_properties() {
        let versions: Vec<Version> = supported_versions(
            r#"{"Properties":[{"SupportedSchemaVersions":[{"Major":1,"Minor":0},{"Major":2,"Minor":6}]}]}"#,
        )
        .unwrap();
        assert_eq!(
            versions,
            [
                Version { major: 1, minor: 0 },
                Version { major: 2, minor: 6 }
            ]
        );
    }

    #[test]
    fn rejects_missing_basic_property() {
        let error = supported_versions(r#"{"Properties":[]}"#).unwrap_err();
        assert!(error.to_string().contains("Properties entry"));
    }

    #[test]
    fn serializes_minimal_kernel_direct_document() {
        let document = compute_system_document(
            r"C:\build\vmlinux",
            r"C:\build\initramfs.cpio.gz",
            "console=ttyS0,115200 panic=-1",
            512,
            r"\\.\pipe\nvx-test-com1",
            ComputeSystemOptions::default(),
        )
        .unwrap();
        let actual: ::serde_json::Value = ::serde_json::from_str(&document).unwrap();
        assert_eq!(
            actual,
            ::serde_json::json!({
                "Owner": "nvx",
                "SchemaVersion": { "Major": 2, "Minor": 2 },
                "ShouldTerminateOnLastHandleClosed": true,
                "VirtualMachine": {
                    "StopOnReset": true,
                    "Chipset": {
                        "LinuxKernelDirect": {
                            "KernelFilePath": r"C:\build\vmlinux",
                            "InitRdPath": r"C:\build\initramfs.cpio.gz",
                            "KernelCmdLine": "console=ttyS0,115200 panic=-1"
                        }
                    },
                    "ComputeTopology": {
                        "Memory": { "SizeInMB": 512, "AllowOvercommit": true },
                        "Processor": { "Count": 1 }
                    },
                    "Devices": {
                        "ComPorts": {
                            "0": { "NamedPipe": r"\\.\pipe\nvx-test-com1" }
                        }
                    }
                }
            })
        );

        assert_eq!(
            ::serde_json::from_str::<::serde_json::Value>(
                &network_adapter_add(
                    "01234567-89ab-4cde-8f01-23456789abcd",
                    "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                    "00-15-5D-52-C0-10",
                )
                .unwrap()
            )
            .unwrap(),
            ::serde_json::json!({
                "RequestType": "Add",
                "ResourcePath": "VirtualMachine/Devices/NetworkAdapters/01234567-89ab-4cde-8f01-23456789abcd",
                "Settings": {
                    "EndpointId": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                    "MacAddress": "00-15-5D-52-C0-10"
                }
            })
        );
    }

    #[test]
    fn serializes_snapshot_lifecycle_documents() {
        assert_eq!(
            ::serde_json::from_str::<::serde_json::Value>(&pause_options().unwrap()).unwrap(),
            ::serde_json::json!({
                "SuspensionLevel": "Suspend",
                "HostedNotification": { "Reason": "Save" }
            })
        );
        assert_eq!(
            ::serde_json::from_str::<::serde_json::Value>(
                &save_options(r"C:\snap\runtime.vmrs").unwrap()
            )
            .unwrap(),
            ::serde_json::json!({
                "SaveType": "ToFile",
                "SaveStateFilePath": r"C:\snap\runtime.vmrs"
            })
        );

        let cold_document = compute_system_document(
            r"C:\build\vmlinux",
            r"C:\build\initramfs.cpio.gz",
            "console=ttyS0,115200",
            512,
            r"\\.\pipe\nvx-test-com1",
            ComputeSystemOptions {
                control_pipe: Some(r"\\.\pipe\nvx-test-com2"),
                restore_state: None,
                network_adapter: Some((
                    "01234567-89ab-4cde-8f01-23456789abcd",
                    "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                    "00-15-5D-52-C0-10",
                )),
                plan9_share: None,
            },
        )
        .unwrap();
        let cold: ::serde_json::Value = ::serde_json::from_str(&cold_document).unwrap();
        assert_eq!(
            cold["VirtualMachine"]["Devices"]["ComPorts"]["1"]["NamedPipe"],
            r"\\.\pipe\nvx-test-com2"
        );
        assert_eq!(
            cold["VirtualMachine"]["Devices"]["NetworkAdapters"]["01234567-89ab-4cde-8f01-23456789abcd"],
            ::serde_json::json!({
                "EndpointId": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                "MacAddress": "00-15-5D-52-C0-10"
            })
        );

        let restore_document = compute_system_document(
            r"C:\build\vmlinux",
            r"C:\build\initramfs.cpio.gz",
            "console=ttyS0,115200",
            512,
            r"\\.\pipe\nvx-test-com1",
            ComputeSystemOptions {
                restore_state: Some(r"C:\snap\runtime.vmrs"),
                network_adapter: Some((
                    "01234567-89ab-4cde-8f01-23456789abcd",
                    "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                    "00-15-5D-52-C0-10",
                )),
                ..Default::default()
            },
        )
        .unwrap();
        let restore: ::serde_json::Value = ::serde_json::from_str(&restore_document).unwrap();
        assert_eq!(
            restore["VirtualMachine"]["RestoreState"]["SaveStateFilePath"],
            r"C:\snap\runtime.vmrs"
        );
        assert!(
            restore["VirtualMachine"]["Devices"]
                .get("NetworkAdapters")
                .is_none()
        );

        assert_eq!(
            ::serde_json::from_str::<::serde_json::Value>(
                &network_adapter_remove(
                    "01234567-89ab-4cde-8f01-23456789abcd",
                    "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                    "00-15-5D-52-C0-10",
                )
                .unwrap()
            )
            .unwrap(),
            ::serde_json::json!({
                "RequestType": "Remove",
                "ResourcePath": "VirtualMachine/Devices/NetworkAdapters/01234567-89ab-4cde-8f01-23456789abcd",
                "Settings": {
                    "EndpointId": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                    "MacAddress": "00-15-5D-52-C0-10"
                }
            })
        );
    }

    #[test]
    fn serializes_plan9_share_with_hyper_v_socket_transport() {
        let document = compute_system_document(
            r"C:\build\vmlinux",
            r"C:\build\initramfs.cpio.gz",
            "console=ttyS0,115200",
            512,
            r"\\.\pipe\nvx-test-com1",
            ComputeSystemOptions {
                plan9_share: Some((r"C:\host\share", false)),
                ..Default::default()
            },
        )
        .unwrap();
        let actual: ::serde_json::Value = ::serde_json::from_str(&document).unwrap();
        assert_eq!(
            actual["VirtualMachine"]["Devices"]["Plan9"],
            ::serde_json::json!({})
        );
        assert_eq!(
            actual["VirtualMachine"]["Devices"]["HvSocket"]["HvSocketConfig"]
                ["DefaultBindSecurityDescriptor"],
            "D:P(A;;FA;;;SY)(A;;FA;;;BA)"
        );
        assert_eq!(
            ::serde_json::from_str::<::serde_json::Value>(
                &plan9_share_add("0", r"C:\host\share", false).unwrap()
            )
            .unwrap(),
            ::serde_json::json!({
                "RequestType": "Add",
                "ResourcePath": "VirtualMachine/Devices/Plan9/Shares",
                "Settings": {
                    "Name": "0",
                    "AccessName": "0",
                    "Path": r"C:\host\share",
                    "Port": 564,
                    "Flags": 4
                }
            })
        );
    }

    #[test]
    fn validates_only_successful_graceful_exits() {
        validate_exit_document(r#"{"Status":0,"ExitType":"GracefulExit"}"#).unwrap();
        assert!(
            validate_exit_document(r#"{"Status":-2143878906,"ExitType":"UnexpectedExit"}"#)
                .is_err()
        );
        assert!(validate_exit_document(r#"{"Status":0,"ExitType":"Unknown"}"#).is_err());
    }
}
