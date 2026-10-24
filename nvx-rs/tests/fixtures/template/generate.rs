
// Writes the template manifests of nvx-rs/tests/fixtures/template/ with OpenVMM's own code, as
// README.md there describes: append this file to openvmm/openvmm_helpers/src/snapshot/microvm.rs
// in a scratch checkout of the pinned OpenVMM, and run its ignored test. It is not part of either
// build.
#[cfg(test)]
mod nvx_fixture {
    use super::*;
    use crate::snapshot::SnapshotManifest;
    use crate::snapshot::format::SAVED_STATE_ROOT_TYPE;
    use crate::snapshot::format::SAVED_STATE_SCHEMA_VERSION;
    use crate::snapshot::format::SNAPSHOT_CONFIG_INVARIANTS;
    use crate::snapshot::format::SNAPSHOT_FORMAT_MAGIC;
    use crate::snapshot::format::SNAPSHOT_RESTORE_POLICY_CLONE;
    use crate::snapshot::format::SNAPSHOT_TIER_PLATFORM;
    use mesh::payload::Timestamp;
    use openvmm_defs::microvm as defs;

    fn attachment(
        stable_id: &str,
        kind: &str,
        policy: &str,
        identity_kind: &str,
        identity: &[u8],
    ) -> SnapshotAttachment {
        SnapshotAttachment {
            stable_id: stable_id.to_owned(),
            kind: kind.to_owned(),
            required: false,
            reconnect_policy: policy.to_owned(),
            identity_kind: identity_kind.to_owned(),
            identity: identity.to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        }
    }

    #[test]
    #[ignore = "writes NVX's template manifest fixtures"]
    fn write_nvx_template_fixtures() {
        const MB: u64 = 1024 * 1024;
        let directory = std::path::PathBuf::from(std::env::var("NVX_FIXTURE_DIR").unwrap());
        for (name, hypervisor, with_network) in [
            ("platform-kvm", "kvm", false),
            ("platform-mshv-network", "mshv", true),
        ] {
            let mut command_line = defs::build_microvm_control_command_line(
                &[
                    "nvx_sandbox=1".to_owned(),
                    "nvx_config=0xd0010000,65536".to_owned(),
                ],
                true,
            )
            .unwrap();
            defs::append_microvm_processor_limit(&mut command_line, 2).unwrap();
            command_line.push_str(" nvx_snapshot_tier=platform");
            let network: defs::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
            let egress_policy = network
                .bind_egress_policy(
                    net_backend_resources::egress::EgressPolicyMode::AllowList(vec![
                        "192.0.2.0/24".parse().unwrap(),
                    ]),
                )
                .unwrap();
            let irq = defs::microvm_virtio_net_irq(Some(hypervisor)).unwrap();
            let blocks = [
                defs::MicrovmSandboxBlockConfig {
                    role: defs::MicrovmSandboxBlockRole::Distro,
                    read_only: true,
                },
                defs::MicrovmSandboxBlockConfig {
                    role: defs::MicrovmSandboxBlockRole::Scratch,
                    read_only: false,
                },
            ];
            defs::append_microvm_virtio_discovery(
                &mut command_line,
                with_network.then(|| (&network, irq, egress_policy.gateway_dns_server(), None)),
                true,
                &[],
                true,
                true,
                &blocks,
            )
            .unwrap();
            let sandbox_blocks = vec![
                SnapshotMicrovmSandboxBlock {
                    role: "distro".to_owned(),
                    read_only: true,
                    length: 1024 * MB,
                    identity_kind: "unbound".to_owned(),
                    identity: Vec::new(),
                    artifact: String::new(),
                    logical_block_size: 512,
                    physical_block_size: 4096,
                    restore_mode: String::new(),
                },
                SnapshotMicrovmSandboxBlock {
                    role: "scratch".to_owned(),
                    read_only: false,
                    length: 64 * MB,
                    identity_kind: "fresh".to_owned(),
                    identity: Vec::new(),
                    artifact: String::new(),
                    logical_block_size: 512,
                    physical_block_size: 512,
                    restore_mode: String::new(),
                },
            ];
            let mut state_unit_names: Vec<String> = [
                "partition",
                "vmtime",
                "pic",
                "ioapic",
                "pit",
                "rtc",
                "microvm-portb",
                "microvm-shutdown",
                "microvm-snapshot-request",
            ]
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
            if with_network {
                state_unit_names.push(format!(
                    "virtio-net-{}",
                    defs::MICROVM_VIRTIO_NET_MMIO_BASE
                ));
            }
            state_unit_names.push(format!(
                "virtiofs-{}",
                defs::MICROVM_FILESYSTEM_SLOTS[0].mmio_base
            ));
            state_unit_names.push(format!(
                "virtio-console-{}",
                defs::MICROVM_VIRTIO_CONSOLE_MMIO_BASE
            ));
            for block in &blocks {
                state_unit_names.push(format!("virtio-blk-{}", block.role.mmio_base()));
            }
            state_unit_names.push(format!(
                "virtio-control-console-{}",
                defs::MICROVM_VIRTIO_CONTROL_CONSOLE_MMIO_BASE
            ));
            let contract = microvm_machine_contract(
                hypervisor,
                MICROVM_BOOT_LAYOUT_VERSION,
                command_line,
                with_network.then(|| {
                    (
                        &network,
                        &egress_policy,
                        attachment(
                            "net:microvm0",
                            "virtio-net",
                            "recreate-endpoint",
                            "user-mode-nat",
                            b"consomme",
                        ),
                    )
                }),
                true,
                None,
                Some(attachment(
                    "console:microvm-virtio0",
                    "virtio-console",
                    "discard-while-disconnected",
                    "disconnected",
                    b"discard",
                )),
                Some(attachment(
                    "console:microvm-control0",
                    "virtio-control-console",
                    "broker-authenticated-listener",
                    "unix-socket",
                    b"/run/nvx/control.sock",
                )),
                sandbox_blocks,
                2,
                512 * MB,
                Some(2048 * MB),
                state_unit_names,
                crate::snapshot::time::tests::test_time_contract(),
                crate::snapshot::time::tests::test_cpu_profile(),
            )
            .unwrap();
            validate_supported_microvm_contract(&contract).unwrap();
            let manifest = SnapshotManifest {
                version: crate::snapshot::MANIFEST_VERSION,
                created_at: Timestamp {
                    seconds: 1_761_300_000,
                    nanos: 123_456_789,
                },
                openvmm_version: "0.0.0".to_owned(),
                memory_size_bytes: 512 * MB,
                vp_count: 2,
                page_size: 4096,
                architecture: "x86_64".to_owned(),
                state_size_bytes: 65_536,
                machine_contract: Some(contract),
                format_magic: SNAPSHOT_FORMAT_MAGIC.to_vec(),
                saved_state_schema_version: SAVED_STATE_SCHEMA_VERSION,
                saved_state_root_type: SAVED_STATE_ROOT_TYPE.to_owned(),
                snapshot_tier: SNAPSHOT_TIER_PLATFORM.to_owned(),
                restore_policy: SNAPSHOT_RESTORE_POLICY_CLONE.to_owned(),
                consumed_config_sections: SNAPSHOT_CONFIG_INVARIANTS,
            };
            crate::snapshot::format::validate_manifest_header(&manifest).unwrap();
            crate::snapshot::format::validate_manifest_contents(&manifest).unwrap();
            let bytes = mesh::payload::encode(manifest);
            let decoded: SnapshotManifest = mesh::payload::decode(&bytes).unwrap();
            crate::snapshot::format::validate_manifest_header(&decoded).unwrap();
            std::fs::write(directory.join(format!("{name}.bin")), bytes).unwrap();
            std::fs::write(
                directory.join(format!("{name}.txt")),
                format!("{:#?}\n", decoded.machine_contract),
            )
            .unwrap();
        }
    }
}
