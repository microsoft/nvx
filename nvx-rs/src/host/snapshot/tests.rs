//! Builders of the manifests and capture classes of snapshots, for their tests.
//!
//! [`Proto`] writes the protobuf wire format as OpenVMM's `mesh` does, independently of the reader
//! that the tests check, and [`ManifestSpec`] holds each field of a manifest, so that a test
//! changes one field and encodes the others as they were.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{
    BlockGeometry, CaptureClass, DESCRIPTION_FILE, Invariants, KEY_FILE, MANIFEST_NAME,
    OPENVMM_DIR, Snapshot, SnapshotKey, SnapshotManifest,
};
use crate::ErrorKind;
use crate::activation::{Journal, MachineIdMode, OverlayMode, View};
use crate::host::config::{
    ConsoleAttachment, LayeredRoot, NetworkConfig, NicConfig, RootConfig, UpperConfig, VmConfig,
};
use crate::host::network::{Access, NetworkPolicy};
use crate::host::process::BootId;
use crate::host::runtime::{LaunchOptions, Runtime};
use crate::host::sandbox::SandboxNetwork;
use crate::host::store::{ArtifactIdentity, RuntimeIdentity};
use crate::hypervisor::Hypervisor;
use crate::net::Ipv4Cidr;
use crate::protocol::root::RootImage;

/// Bytes in a MiB.
pub(in crate::host) const MIB: u64 = 1024 * 1024;

/// A protobuf message under construction, whose fields are written in the order of the calls.
///
/// Like `mesh`, the typed methods omit a field whose value is zero or empty, but for
/// [`message`](Self::message) and [`some`](Self::some), which write an `Option` that is `Some`.
#[derive(Debug, Clone, Default)]
pub(in crate::host) struct Proto(Vec<u8>);

impl Proto {
    /// Appends `value` to `bytes` as a varint.
    pub(in crate::host) fn put_varint(bytes: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                bytes.push(byte);
                return;
            }
            bytes.push(byte | 0x80);
        }
    }

    /// Appends the key of field `number` of wire type `wire_type`.
    pub(in crate::host) fn key(mut self, number: u32, wire_type: u8) -> Self {
        Self::put_varint(&mut self.0, u64::from(number) << 3 | u64::from(wire_type));
        self
    }

    /// Appends field `number` as the varint `value`, even if it is 0.
    pub(in crate::host) fn some(self, number: u32, value: u64) -> Self {
        let mut message = self.key(number, 0);
        Self::put_varint(&mut message.0, value);
        message
    }

    /// Appends field `number` as the varint `value`, unless it is 0.
    pub(in crate::host) fn varint(self, number: u32, value: u64) -> Self {
        if value == 0 {
            self
        } else {
            self.some(number, value)
        }
    }

    /// Appends field `number` as the bytes `bytes`, even if they are empty.
    pub(in crate::host) fn always_bytes(self, number: u32, bytes: &[u8]) -> Self {
        let mut message = self.key(number, 2);
        Self::put_varint(&mut message.0, bytes.len() as u64);
        message.0.extend_from_slice(bytes);
        message
    }

    /// Appends field `number` as the bytes `bytes`, unless they are empty.
    pub(in crate::host) fn bytes(self, number: u32, bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            self
        } else {
            self.always_bytes(number, bytes)
        }
    }

    /// Appends field `number` as the text `text`, unless it is empty.
    pub(in crate::host) fn text(self, number: u32, text: &str) -> Self {
        self.bytes(number, text.as_bytes())
    }

    /// Appends field `number` as the message `message`, even if it is empty, as `mesh` writes an
    /// `Option` that is `Some`, or an element of a repeated field.
    pub(in crate::host) fn message(self, number: u32, message: Proto) -> Self {
        self.always_bytes(number, &message.0)
    }

    /// Appends field `number` as the packed varints `values`, unless there are none.
    pub(in crate::host) fn packed(self, number: u32, values: &[u32]) -> Self {
        let mut packed = Vec::new();
        for &value in values {
            Self::put_varint(&mut packed, u64::from(value));
        }
        self.bytes(number, &packed)
    }

    /// Appends `bytes` as they are.
    pub(in crate::host) fn raw(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }

    /// Returns the message's bytes.
    pub(in crate::host) fn finish(self) -> Vec<u8> {
        self.0
    }
}

/// `openvmm.snapshot.SnapshotDevice`.
#[derive(Debug, Clone)]
pub(in crate::host) struct DeviceSpec {
    /// Field 1.
    pub(in crate::host) stable_id: String,
    /// Field 2.
    pub(in crate::host) state_unit: String,
    /// Field 3.
    pub(in crate::host) kind: String,
    /// Field 4.
    pub(in crate::host) order: u32,
    /// Field 5: each range's address space, start, and length.
    pub(in crate::host) ranges: Vec<(String, u64, u64)>,
    /// Field 6.
    pub(in crate::host) irq: Option<u32>,
    /// Field 7.
    pub(in crate::host) transport: String,
    /// Field 8.
    pub(in crate::host) feature_banks: Vec<u32>,
    /// Field 9.
    pub(in crate::host) queue_count: u32,
    /// Field 10.
    pub(in crate::host) queue_max_sizes: Vec<u32>,
}

impl DeviceSpec {
    /// Returns a platform device, which has no transport, features, or queues.
    fn platform(
        stable_id: &str,
        state_unit: &str,
        kind: &str,
        ranges: &[(&str, u64, u64)],
        irq: Option<u32>,
    ) -> Self {
        Self {
            stable_id: stable_id.to_owned(),
            state_unit: state_unit.to_owned(),
            kind: kind.to_owned(),
            order: 0,
            ranges: ranges
                .iter()
                .map(|&(space, start, length)| (space.to_owned(), start, length))
                .collect(),
            irq,
            transport: String::new(),
            feature_banks: Vec::new(),
            queue_count: 0,
            queue_max_sizes: Vec::new(),
        }
    }

    /// Returns a virtio-mmio device of `kind` at `base`, whose state unit `unit` names.
    fn virtio(
        stable_id: &str,
        kind: &str,
        unit: &str,
        base: u64,
        irq: u32,
        features: u64,
        queues: u32,
    ) -> Self {
        Self {
            stable_id: stable_id.to_owned(),
            state_unit: format!("{unit}-{base}"),
            kind: kind.to_owned(),
            order: 0,
            ranges: vec![("mmio".to_owned(), base, 0x1000)],
            irq: Some(irq),
            transport: "virtio-mmio".to_owned(),
            feature_banks: vec![features as u32, (features >> 32) as u32],
            queue_count: queues,
            queue_max_sizes: vec![256; queues as usize],
        }
    }

    /// Encodes the device.
    fn encode(&self) -> Proto {
        let mut message = Proto::default()
            .text(1, &self.stable_id)
            .text(2, &self.state_unit)
            .text(3, &self.kind)
            .varint(4, self.order.into());
        for (space, start, length) in &self.ranges {
            message = message.message(
                5,
                Proto::default()
                    .text(1, space)
                    .varint(2, *start)
                    .varint(3, *length),
            );
        }
        if let Some(irq) = self.irq {
            message = message.some(6, irq.into());
        }
        message
            .text(7, &self.transport)
            .packed(8, &self.feature_banks)
            .varint(9, self.queue_count.into())
            .packed(10, &self.queue_max_sizes)
    }
}

/// `openvmm.snapshot.SnapshotAttachment`.
#[derive(Debug, Clone)]
pub(in crate::host) struct AttachmentSpec {
    /// Field 1.
    pub(in crate::host) stable_id: String,
    /// Field 2.
    pub(in crate::host) kind: String,
    /// Field 3.
    pub(in crate::host) required: bool,
    /// Field 4.
    pub(in crate::host) reconnect_policy: String,
    /// Field 5.
    pub(in crate::host) identity_kind: String,
    /// Field 6.
    pub(in crate::host) identity: Vec<u8>,
    /// Field 7.
    pub(in crate::host) length: u64,
    /// Field 8.
    pub(in crate::host) reconnect_timeout_ms: u64,
}

impl AttachmentSpec {
    /// Returns an attachment that is not required, which `policy` reconnects.
    fn new(
        stable_id: &str,
        kind: &str,
        policy: &str,
        identity_kind: &str,
        identity: &[u8],
    ) -> Self {
        Self {
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

    /// Encodes the attachment.
    fn encode(&self) -> Proto {
        Proto::default()
            .text(1, &self.stable_id)
            .text(2, &self.kind)
            .varint(3, self.required.into())
            .text(4, &self.reconnect_policy)
            .text(5, &self.identity_kind)
            .bytes(6, &self.identity)
            .varint(7, self.length)
            .varint(8, self.reconnect_timeout_ms)
    }
}

/// `openvmm.snapshot.SnapshotMicrovmNetwork`.
#[derive(Debug, Clone)]
pub(in crate::host) struct NetworkSpec {
    /// Field 1.
    pub(in crate::host) ipv4: [u8; 4],
    /// Field 2.
    pub(in crate::host) prefix_length: u32,
    /// Field 3.
    pub(in crate::host) gateway: [u8; 4],
    /// Field 4.
    pub(in crate::host) guest_mac: Vec<u8>,
    /// Field 5.
    pub(in crate::host) gateway_mac: Vec<u8>,
    /// Field 6.
    pub(in crate::host) policy_mode: String,
    /// Field 7.
    pub(in crate::host) policy_digest: Vec<u8>,
    /// Field 8.
    pub(in crate::host) policy_required: bool,
    /// Field 9.
    pub(in crate::host) profile: String,
    /// Field 10.
    pub(in crate::host) policy_encoding_version: u32,
    /// Field 11.
    pub(in crate::host) ipv6: Vec<u8>,
    /// Field 12.
    pub(in crate::host) ipv6_prefix_length: u32,
    /// Field 13.
    pub(in crate::host) ipv6_gateway: Vec<u8>,
}

impl NetworkSpec {
    /// Encodes the network, its profile first, as `mesh` writes it.
    fn encode(&self) -> Proto {
        Proto::default()
            .text(9, &self.profile)
            .varint(1, u32::from_be_bytes(self.ipv4).into())
            .varint(2, self.prefix_length.into())
            .varint(3, u32::from_be_bytes(self.gateway).into())
            .bytes(4, &self.guest_mac)
            .bytes(5, &self.gateway_mac)
            .text(6, &self.policy_mode)
            .bytes(7, &self.policy_digest)
            .varint(8, self.policy_required.into())
            .varint(10, self.policy_encoding_version.into())
            .bytes(11, &self.ipv6)
            .varint(12, self.ipv6_prefix_length.into())
            .bytes(13, &self.ipv6_gateway)
    }
}

/// `openvmm.snapshot.SnapshotMicrovmSandboxBlock`.
#[derive(Debug, Clone)]
pub(in crate::host) struct BlockSpec {
    /// Field 1.
    pub(in crate::host) role: String,
    /// Field 2.
    pub(in crate::host) read_only: bool,
    /// Field 3.
    pub(in crate::host) length: u64,
    /// Field 4.
    pub(in crate::host) identity_kind: String,
    /// Field 5.
    pub(in crate::host) identity: Vec<u8>,
    /// Field 6.
    pub(in crate::host) artifact: String,
    /// Field 7.
    pub(in crate::host) logical_block_size: u32,
    /// Field 8.
    pub(in crate::host) physical_block_size: u32,
    /// Field 9.
    pub(in crate::host) restore_mode: String,
}

impl BlockSpec {
    /// Encodes the block.
    fn encode(&self) -> Proto {
        Proto::default()
            .text(1, &self.role)
            .varint(2, self.read_only.into())
            .varint(3, self.length)
            .text(4, &self.identity_kind)
            .bytes(5, &self.identity)
            .text(6, &self.artifact)
            .varint(7, self.logical_block_size.into())
            .varint(8, self.physical_block_size.into())
            .text(9, &self.restore_mode)
    }
}

/// `openvmm.snapshot.SnapshotTimeContract`.
#[derive(Debug, Clone)]
pub(in crate::host) struct TimeSpec {
    /// Field 1.
    pub(in crate::host) abi_version: u32,
    /// Field 2.
    pub(in crate::host) tsc_hz: u64,
    /// Field 3.
    pub(in crate::host) tolerance_ppm: u32,
    /// Field 4.
    pub(in crate::host) apic_hz: u64,
    /// Field 5.
    pub(in crate::host) capture_tsc: u64,
    /// Field 6.
    pub(in crate::host) capture_utc_ns: u64,
    /// Field 7.
    pub(in crate::host) capture_monotonic_ns: u64,
    /// Field 8.
    pub(in crate::host) host_clock: String,
    /// Field 9.
    pub(in crate::host) host_id: Vec<u8>,
    /// Field 10.
    pub(in crate::host) boot_id: Vec<u8>,
    /// Field 11.
    pub(in crate::host) lineage: u32,
    /// Field 12.
    pub(in crate::host) cut_to_anchor_ns: Option<u64>,
}

impl TimeSpec {
    /// Encodes the time contract.
    fn encode(&self) -> Proto {
        let message = Proto::default()
            .varint(1, self.abi_version.into())
            .varint(2, self.tsc_hz)
            .varint(3, self.tolerance_ppm.into())
            .varint(4, self.apic_hz)
            .varint(5, self.capture_tsc)
            .varint(6, self.capture_utc_ns)
            .varint(7, self.capture_monotonic_ns)
            .text(8, &self.host_clock)
            .bytes(9, &self.host_id)
            .bytes(10, &self.boot_id)
            .varint(11, self.lineage.into());
        match self.cut_to_anchor_ns {
            Some(cut) => message.some(12, cut),
            None => message,
        }
    }
}

/// `openvmm.snapshot.SnapshotCpuProfile`.
#[derive(Debug, Clone)]
pub(in crate::host) struct CpuSpec {
    /// Field 1.
    pub(in crate::host) id: String,
    /// Field 2.
    pub(in crate::host) digest: Vec<u8>,
    /// Field 3.
    pub(in crate::host) document: Vec<u8>,
    /// Field 4.
    pub(in crate::host) effective_cpuid: Vec<u8>,
    /// Field 6.
    pub(in crate::host) signature: u32,
}

impl CpuSpec {
    /// Encodes the CPU profile record.
    fn encode(&self) -> Proto {
        Proto::default()
            .text(1, &self.id)
            .bytes(2, &self.digest)
            .bytes(3, &self.document)
            .bytes(4, &self.effective_cpuid)
            .varint(6, self.signature.into())
    }
}

/// `openvmm.snapshot.SnapshotMachineContract`.
#[derive(Debug, Clone)]
pub(in crate::host) struct ContractSpec {
    /// Field 1.
    pub(in crate::host) machine_profile: String,
    /// Field 2.
    pub(in crate::host) abi_version: u32,
    /// Field 3.
    pub(in crate::host) source_hypervisor: String,
    /// Field 4.
    pub(in crate::host) command_line: String,
    /// Field 5, or `None` for the SHA-256 digest of the command line.
    pub(in crate::host) command_line_digest: Option<Vec<u8>>,
    /// Field 6: each range's start, length, and file offset.
    pub(in crate::host) memory_ranges: Vec<(u64, u64, u64)>,
    /// Field 7: the sockets, the dies of each socket, the cores of each die, the threads of each
    /// core, and the APIC IDs.
    pub(in crate::host) topology: (u32, u32, u32, u32, Vec<u32>),
    /// Field 8.
    pub(in crate::host) devices: Vec<DeviceSpec>,
    /// Field 9.
    pub(in crate::host) state_units: Vec<String>,
    /// Field 10.
    pub(in crate::host) attachments: Vec<AttachmentSpec>,
    /// Field 16.
    pub(in crate::host) boot_layout_version: u32,
    /// Field 18.
    pub(in crate::host) network: Option<NetworkSpec>,
    /// Field 19: the encoded policy of an attached filesystem.
    pub(in crate::host) filesystem: Option<Vec<u8>>,
    /// Field 21.
    pub(in crate::host) blocks: Vec<BlockSpec>,
    /// Field 22.
    pub(in crate::host) filesystem_slot_version: u32,
    /// Field 23.
    pub(in crate::host) boot_online_vp_count: u32,
    /// Field 24.
    pub(in crate::host) interrupt_mode: String,
    /// Fields 25 and 26: the start and the length of the shared interrupt-status page.
    pub(in crate::host) status_page: (u64, u64),
    /// Field 27.
    pub(in crate::host) expansion_version: u32,
    /// Field 28.
    pub(in crate::host) memory_capacity: u64,
    /// Field 29.
    pub(in crate::host) memory_block_size: u64,
    /// Field 30: each range's start and length.
    pub(in crate::host) expansion_ranges: Vec<(u64, u64)>,
    /// Field 31.
    pub(in crate::host) time: Option<TimeSpec>,
    /// Field 32.
    pub(in crate::host) cpu: Option<CpuSpec>,
}

impl ContractSpec {
    /// Gives each device its index as its order.
    pub(in crate::host) fn renumber(&mut self) {
        for (index, device) in self.devices.iter_mut().enumerate() {
            device.order = index as u32;
        }
    }

    /// Encodes the contract.
    fn encode(&self) -> Proto {
        let digest = self
            .command_line_digest
            .clone()
            .unwrap_or_else(|| Sha256::digest(&self.command_line).to_vec());
        let mut message = Proto::default()
            .text(1, &self.machine_profile)
            .varint(2, self.abi_version.into())
            .text(3, &self.source_hypervisor)
            .text(4, &self.command_line)
            .bytes(5, &digest);
        for &(start, length, offset) in &self.memory_ranges {
            message = message.message(
                6,
                Proto::default()
                    .varint(1, start)
                    .varint(2, length)
                    .varint(3, offset),
            );
        }
        let (sockets, dies, cores, threads, apic_ids) = &self.topology;
        message = message.message(
            7,
            Proto::default()
                .varint(1, (*sockets).into())
                .varint(2, (*dies).into())
                .varint(3, (*cores).into())
                .varint(4, (*threads).into())
                .packed(5, apic_ids),
        );
        for device in &self.devices {
            message = message.message(8, device.encode());
        }
        for unit in &self.state_units {
            message = message.always_bytes(9, unit.as_bytes());
        }
        for attachment in &self.attachments {
            message = message.message(10, attachment.encode());
        }
        message = message.varint(13, self.boot_layout_version.into());
        if let Some(network) = &self.network {
            message = message.message(14, network.encode());
        }
        if let Some(filesystem) = &self.filesystem {
            message = message.always_bytes(15, filesystem);
        }
        for block in &self.blocks {
            message = message.message(16, block.encode());
        }
        message = message
            .varint(17, self.filesystem_slot_version.into())
            .varint(18, self.boot_online_vp_count.into())
            .text(19, &self.interrupt_mode)
            .varint(20, self.status_page.0)
            .varint(21, self.status_page.1)
            .varint(22, self.expansion_version.into())
            .varint(23, self.memory_capacity)
            .varint(24, self.memory_block_size);
        for &(start, length) in &self.expansion_ranges {
            message = message.message(25, Proto::default().varint(1, start).varint(2, length));
        }
        if let Some(time) = &self.time {
            message = message.message(11, time.encode());
        }
        if let Some(cpu) = &self.cpu {
            message = message.message(12, cpu.encode());
        }
        message
    }
}

/// `openvmm.snapshot.SnapshotManifest`.
#[derive(Debug, Clone)]
pub(in crate::host) struct ManifestSpec {
    /// Field 1.
    pub(in crate::host) version: u32,
    /// Field 2: seconds and nanoseconds.
    pub(in crate::host) created_at: (i64, i32),
    /// Field 3.
    pub(in crate::host) openvmm_version: String,
    /// Field 4.
    pub(in crate::host) memory_size: u64,
    /// Field 5.
    pub(in crate::host) vp_count: u32,
    /// Field 6.
    pub(in crate::host) page_size: u32,
    /// Field 7.
    pub(in crate::host) architecture: String,
    /// Field 8.
    pub(in crate::host) state_size: u64,
    /// Field 11.
    pub(in crate::host) contract: Option<ContractSpec>,
    /// Field 12.
    pub(in crate::host) magic: Vec<u8>,
    /// Field 13.
    pub(in crate::host) schema_version: u32,
    /// Field 14.
    pub(in crate::host) root_type: String,
    /// Field 15.
    pub(in crate::host) tier: String,
    /// Field 16.
    pub(in crate::host) restore_policy: String,
    /// Field 17.
    pub(in crate::host) consumed_sections: u32,
    /// Field 18: the snapshot ID, which a microVM capture leaves zero, if it is written.
    pub(in crate::host) snapshot_id: Option<Vec<u8>>,
}

/// The kernel command line of the template of [`ManifestSpec::template`].
const COMMAND_LINE: &str = "earlycon=xe9 console=hvc1 reboot=t panic=-1 nvx_sandbox=1 \
                            nvx_config=0xd0010000,65536 nr_cpus=2 nvx_snapshot_tier=platform \
                            virtio_mmio.device=0x1000@0xd0001000:17 \
                            virtio_mmio.device=0x1000@0xd0002000:18 \
                            virtio_mmio.device=0x1000@0xd0003000:19 \
                            virtio_mmio.device=0x1000@0xd0006000:22 \
                            virtio_mmio.device=0x1000@0xd0007000:23 nvx_control_tty=hvc2";

impl ManifestSpec {
    /// Returns the manifest of a template: a KVM microVM of 2 processors, all online at boot, and
    /// 512 MiB of RAM, of a capacity of 2 GiB, with a `distro` of 1 GiB, a scratch of 64 MiB, the
    /// dormant filesystem slot, a console, a control console, and no NIC.
    pub(in crate::host) fn template() -> Self {
        let (pmio, mmio) = ("pmio", "mmio");
        let platform = DeviceSpec::platform;
        let mut contract = ContractSpec {
            machine_profile: "microvm".to_owned(),
            abi_version: crate::contract::MICROVM_ABI_VERSION as u32,
            source_hypervisor: "kvm".to_owned(),
            command_line: COMMAND_LINE.to_owned(),
            command_line_digest: None,
            memory_ranges: vec![(0, 512 * MIB, 0)],
            topology: (1, 1, 2, 1, vec![0, 1]),
            devices: vec![
                platform("partition", "partition", "partition", &[], None),
                platform("vp0", "partition", "vcpu", &[], None),
                platform("vmtime", "vmtime", "clock", &[], None),
                platform(
                    "pic",
                    "pic",
                    "pic",
                    &[(pmio, 0x20, 2), (pmio, 0xa0, 2)],
                    None,
                ),
                platform(
                    "ioapic",
                    "ioapic",
                    "ioapic",
                    &[(mmio, 0xfec0_0000, 0x1000)],
                    None,
                ),
                platform(
                    "lapic",
                    "partition",
                    "lapic",
                    &[(mmio, 0xfee0_0000, 0x1000)],
                    None,
                ),
                platform("pit", "pit", "pit", &[(pmio, 0x40, 4)], Some(0)),
                platform("rtc", "rtc", "rtc", &[(pmio, 0x70, 2)], Some(8)),
                platform(
                    "microvm-portb",
                    "microvm-portb",
                    "portb",
                    &[(pmio, 0xe9, 2)],
                    None,
                ),
                platform(
                    "microvm-shutdown",
                    "microvm-shutdown",
                    "shutdown",
                    &[(pmio, 0x604, 1)],
                    None,
                ),
                platform(
                    "microvm-snapshot-request",
                    "microvm-snapshot-request",
                    "snapshot-request",
                    &[(pmio, 0x605, 1)],
                    None,
                ),
                DeviceSpec::virtio(
                    "fs:microvm0",
                    "virtio-fs",
                    "virtiofs",
                    0xd000_1000,
                    17,
                    0x1_3000_0000,
                    2,
                ),
                DeviceSpec::virtio(
                    "console:microvm-virtio0",
                    "virtio-console",
                    "virtio-console",
                    0xd000_2000,
                    18,
                    0x1_3000_0000,
                    2,
                ),
                DeviceSpec::virtio(
                    "blk:sandbox:distro",
                    "virtio-blk",
                    "virtio-blk",
                    0xd000_3000,
                    19,
                    0x1_3000_0664,
                    1,
                ),
                DeviceSpec::virtio(
                    "blk:sandbox:scratch",
                    "virtio-blk",
                    "virtio-blk",
                    0xd000_6000,
                    22,
                    0x1_3000_0644,
                    1,
                ),
                DeviceSpec::virtio(
                    "console:microvm-control0",
                    "virtio-control-console",
                    "virtio-control-console",
                    0xd000_7000,
                    23,
                    0x1_3000_0000,
                    2,
                ),
            ],
            state_units: Vec::new(),
            attachments: vec![
                AttachmentSpec::new(
                    "console:microvm-virtio0",
                    "virtio-console",
                    "discard-while-disconnected",
                    "disconnected",
                    b"discard",
                ),
                AttachmentSpec::new(
                    "console:microvm-control0",
                    "virtio-control-console",
                    "broker-authenticated-listener",
                    "unix-socket",
                    b"/run/nvx/control.sock",
                ),
            ],
            boot_layout_version: 1,
            network: None,
            filesystem: None,
            blocks: vec![
                BlockSpec {
                    role: "distro".to_owned(),
                    read_only: true,
                    length: 1024 * MIB,
                    identity_kind: "unbound".to_owned(),
                    identity: Vec::new(),
                    artifact: String::new(),
                    logical_block_size: 512,
                    physical_block_size: 4096,
                    restore_mode: String::new(),
                },
                BlockSpec {
                    role: "scratch".to_owned(),
                    read_only: false,
                    length: 64 * MIB,
                    identity_kind: "fresh".to_owned(),
                    identity: Vec::new(),
                    artifact: String::new(),
                    logical_block_size: 512,
                    physical_block_size: 512,
                    restore_mode: String::new(),
                },
            ],
            filesystem_slot_version: 1,
            boot_online_vp_count: 0,
            interrupt_mode: "edge-shared-status".to_owned(),
            status_page: (0x3_0000, 0x1000),
            expansion_version: 1,
            memory_capacity: 2048 * MIB,
            memory_block_size: 128 * MIB,
            expansion_ranges: vec![(512 * MIB, 1536 * MIB)],
            time: Some(TimeSpec {
                abi_version: crate::contract::TIME_ABI_VERSION as u32,
                tsc_hz: 2_100_000_000,
                tolerance_ppm: 250,
                apic_hz: 200_000_000,
                capture_tsc: 12_345_678,
                capture_utc_ns: 1_700_000_000_000_000_000,
                capture_monotonic_ns: 123_000_000_000,
                host_clock: "linux-boottime".to_owned(),
                host_id: vec![1; 16],
                boot_id: vec![2; 16],
                lineage: 3,
                cut_to_anchor_ns: Some(200),
            }),
            cpu: Some(CpuSpec {
                id: "intel.icelake-sp.v1".to_owned(),
                digest: vec![0x5a; 32],
                document: b"canonical profile document".to_vec(),
                effective_cpuid: (0..88).collect(),
                signature: 0x0006_06a6,
            }),
        };
        contract.renumber();
        for index in 0..contract.devices.len() {
            let unit = contract.devices[index].state_unit.clone();
            if !contract.state_units.contains(&unit) {
                contract.state_units.push(unit);
            }
        }
        Self {
            version: 2,
            created_at: (1_761_300_000, 123_456_789),
            openvmm_version: "0.0.0".to_owned(),
            memory_size: 512 * MIB,
            vp_count: 2,
            page_size: 4096,
            architecture: "x86_64".to_owned(),
            state_size: 65_536,
            contract: Some(contract),
            magic: b"OPENVMM_SNAPSHOT_V2\0".to_vec(),
            schema_version: 1,
            root_type: "openvmm.SavedState".to_owned(),
            tier: "platform".to_owned(),
            restore_policy: "clone".to_owned(),
            consumed_sections: 1,
            snapshot_id: Some(vec![0; 16]),
        }
    }

    /// Returns the manifest of [`template`](Self::template) with a fixed NIC at `10.0.0.2/24`,
    /// with an IPv6 identity, under an allow-list policy.
    pub(in crate::host) fn with_network() -> Self {
        let mut spec = Self::template();
        let contract = spec.contract();
        contract.devices.insert(
            11,
            DeviceSpec::virtio(
                "net:microvm0",
                "virtio-net",
                "virtio-net",
                0xd000_0000,
                16,
                0x1_0000_0020,
                2,
            ),
        );
        contract.renumber();
        contract
            .state_units
            .push("virtio-net-3489660928".to_owned());
        contract.command_line = contract.command_line.replacen(
            "virtio_mmio.device=0x1000@0xd0001000:17",
            "virtio_mmio.device=0x1000@0xd0000000:16 virtio_mmio.device=0x1000@0xd0001000:17",
            1,
        ) + " virtnet_ip=10.0.0.2 virtnet_mask=255.255.255.0 virtnet_gw=10.0.0.1 \
             virtnet_ip6=fd00::a00:2/120 virtnet_gw6=fd00::a00:1";
        contract.attachments.insert(
            0,
            AttachmentSpec::new(
                "net:microvm0",
                "virtio-net",
                "recreate-endpoint",
                "user-mode-nat",
                b"consomme",
            ),
        );
        let mut ipv6 = [0; 16];
        ipv6[0] = 0xfd;
        ipv6[12] = 0x0a;
        let mut ipv6_gateway = ipv6;
        ipv6[15] = 2;
        ipv6_gateway[15] = 1;
        contract.network = Some(NetworkSpec {
            ipv4: [10, 0, 0, 2],
            prefix_length: 24,
            gateway: [10, 0, 0, 1],
            guest_mac: vec![0x52, 0x54, 0, 0, 0, 2],
            gateway_mac: vec![0x52, 0x54, 0, 0, 0, 1],
            policy_mode: "allow-list".to_owned(),
            policy_digest: vec![7; 32],
            policy_required: true,
            profile: "portable".to_owned(),
            policy_encoding_version: 1,
            ipv6: ipv6.to_vec(),
            ipv6_prefix_length: 120,
            ipv6_gateway: ipv6_gateway.to_vec(),
        });
        spec
    }

    /// Returns the machine contract.
    pub(in crate::host) fn contract(&mut self) -> &mut ContractSpec {
        self.contract.as_mut().expect("the spec has a contract")
    }

    /// Returns the time contract.
    pub(in crate::host) fn time(&mut self) -> &mut TimeSpec {
        self.contract()
            .time
            .as_mut()
            .expect("the spec has a time contract")
    }

    /// Returns the CPU profile record.
    pub(in crate::host) fn cpu(&mut self) -> &mut CpuSpec {
        self.contract()
            .cpu
            .as_mut()
            .expect("the spec has a CPU profile")
    }

    /// Returns the fixed NIC.
    pub(in crate::host) fn network(&mut self) -> &mut NetworkSpec {
        self.contract()
            .network
            .as_mut()
            .expect("the spec has a network")
    }

    /// Encodes the manifest, with the fields of `extra` after the others.
    pub(in crate::host) fn encode_with(&self, extra: Proto) -> Vec<u8> {
        let (seconds, nanos) = self.created_at;
        // `mesh` writes an `i64` and an `i32` as varints of their two's complement in 64 bits.
        let created_at = Proto::default()
            .varint(1, seconds as u64)
            .varint(2, i64::from(nanos) as u64);
        let mut message = Proto::default()
            .varint(1, self.version.into())
            .message(2, created_at)
            .text(3, &self.openvmm_version)
            .varint(4, self.memory_size)
            .varint(5, self.vp_count.into())
            .varint(6, self.page_size.into())
            .text(7, &self.architecture)
            .varint(8, self.state_size);
        if let Some(contract) = &self.contract {
            message = message.message(9, contract.encode());
        }
        message = message
            .bytes(10, &self.magic)
            .varint(11, self.schema_version.into())
            .text(12, &self.root_type)
            .text(13, &self.tier)
            .text(14, &self.restore_policy)
            .varint(15, self.consumed_sections.into());
        // `mesh` writes the snapshot ID, a fixed-size value, even when it is zero.
        if let Some(snapshot_id) = &self.snapshot_id {
            message = message.always_bytes(18, snapshot_id);
        }
        message.raw(&extra.finish()).finish()
    }

    /// Encodes the manifest.
    pub(in crate::host) fn encode(&self) -> Vec<u8> {
        self.encode_with(Proto::default())
    }
}

/// Returns the artifact identity `sha256:` and 64 times `digit`.
pub(in crate::host) fn artifact(digit: char) -> ArtifactIdentity {
    ArtifactIdentity::sha256(&digit.to_string().repeat(64)).unwrap()
}

/// Returns the class of the capture of [`ManifestSpec::template`].
pub(in crate::host) fn class() -> CaptureClass {
    CaptureClass {
        runtime: RuntimeIdentity::new(artifact('a'), artifact('b'), artifact('c')),
        guest_build: ArtifactIdentity::new("guest-build-1").unwrap(),
        hypervisor: Hypervisor::Kvm,
        cpu_profile: "auto".to_owned(),
        processors: 2,
        boot_processors: 2,
        memory: 512 * MIB,
        memory_capacity: 2048 * MIB,
        distro: BlockGeometry {
            length: 1024 * MIB,
            logical_block_size: 512,
            physical_block_size: 4096,
        },
        scratch: BlockGeometry {
            length: 64 * MIB,
            logical_block_size: 512,
            physical_block_size: 512,
        },
        network: None,
        invariants: Invariants::new(),
    }
}

/// Returns the class of the capture of [`ManifestSpec::with_network`].
pub(in crate::host) fn network_class() -> CaptureClass {
    CaptureClass {
        network: Some(
            SandboxNetwork::new("10.0.0.2/24", NetworkPolicy::new(Access::Allow)).unwrap(),
        ),
        ..class()
    }
}

pub(in crate::host) struct SnapshotFixture {
    root: tempfile::TempDir,
    pub(in crate::host) dir: PathBuf,
    spec: ManifestSpec,
    key: SnapshotKey,
}

impl SnapshotFixture {
    pub(in crate::host) fn new() -> Self {
        use crate::host::platform;
        use crate::host::store::files::{create_new, write_private};

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("snapshot");
        assert_eq!(platform::create_owner_only_dir(&dir).unwrap(), None);
        let openvmm = dir.join(OPENVMM_DIR);
        assert_eq!(platform::create_owner_only_dir(&openvmm).unwrap(), None);
        let spec = ManifestSpec::template();
        let manifest = SnapshotManifest::parse(&spec.encode()).unwrap();
        let key = SnapshotKey::new(&class(), &manifest).unwrap();
        write_private(&dir.join(KEY_FILE), &key.to_bytes()).unwrap();
        write_private(&dir.join(DESCRIPTION_FILE), b"not JSON").unwrap();
        write_private(&openvmm.join(MANIFEST_NAME), &spec.encode()).unwrap();
        for (name, length) in [
            ("memory.bin", spec.memory_size),
            ("state.bin", spec.state_size),
        ] {
            let file = create_new(&openvmm.join(name)).unwrap();
            platform::make_sparse(&file).unwrap();
            file.set_len(length).unwrap();
        }
        Self {
            root,
            dir,
            spec,
            key,
        }
    }

    fn files(&self) -> [PathBuf; 5] {
        let openvmm = self.dir.join(OPENVMM_DIR);
        [
            self.dir.join(KEY_FILE),
            self.dir.join(DESCRIPTION_FILE),
            openvmm.join(MANIFEST_NAME),
            openvmm.join("memory.bin"),
            openvmm.join("state.bin"),
        ]
    }
}

fn refused(path: &Path) {
    let error = Snapshot::open(path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
}

#[test]
fn snapshot_open_checks_the_complete_directory_and_preserves_its_metadata() {
    let fixture = SnapshotFixture::new();
    let snapshot = Snapshot::open(&fixture.dir).unwrap();
    assert_eq!(snapshot.dir, fixture.dir);
    assert_eq!(snapshot.key(), &fixture.key);
    assert_eq!(
        snapshot.manifest(),
        &SnapshotManifest::parse(&fixture.spec.encode()).unwrap()
    );
    for suffix in ["", std::path::MAIN_SEPARATOR_STR, "."] {
        let path = if suffix == "." {
            fixture.dir.join(".")
        } else {
            let mut path = fixture.dir.as_os_str().to_os_string();
            path.push(suffix);
            PathBuf::from(path)
        };
        assert_eq!(Snapshot::open(&path).unwrap().key(), &fixture.key);
    }
}

#[test]
fn snapshot_open_refuses_every_missing_entry() {
    for index in 0..6 {
        let fixture = SnapshotFixture::new();
        let missing = if index == 5 {
            fixture.dir.join(OPENVMM_DIR)
        } else {
            fixture.files()[index].clone()
        };
        fs::rename(&missing, fixture.root.path().join("missing")).unwrap();
        refused(&fixture.dir);
    }
    let fixture = SnapshotFixture::new();
    refused(&fixture.root.path().join("absent"));
}

#[test]
fn snapshot_open_refuses_extra_entries_and_alternate_names() {
    use crate::host::store::files::write_private;

    for nested in [false, true] {
        let fixture = SnapshotFixture::new();
        let dir = if nested {
            fixture.dir.join(OPENVMM_DIR)
        } else {
            fixture.dir.clone()
        };
        write_private(&dir.join("extra"), b"extra").unwrap();
        refused(&fixture.dir);
    }
    let fixture = SnapshotFixture::new();
    fs::rename(fixture.dir.join(KEY_FILE), fixture.dir.join("KEY.BIN")).unwrap();
    refused(&fixture.dir);
}

#[test]
fn snapshot_open_refuses_every_directory_in_place_of_a_regular_file() {
    for index in 0..5 {
        let fixture = SnapshotFixture::new();
        let path = &fixture.files()[index];
        fs::remove_file(path).unwrap();
        fs::create_dir(path).unwrap();
        refused(&fixture.dir);
    }
}

#[test]
fn snapshot_open_refuses_files_in_place_of_its_directories() {
    use crate::host::store::files::write_private;

    let fixture = SnapshotFixture::new();
    let file = fixture.root.path().join("file");
    write_private(&file, b"file").unwrap();
    refused(&file);
    fs::rename(
        fixture.dir.join(OPENVMM_DIR),
        fixture.root.path().join("saved"),
    )
    .unwrap();
    write_private(&fixture.dir.join(OPENVMM_DIR), b"file").unwrap();
    refused(&fixture.dir);
}

#[test]
fn snapshot_open_refuses_both_artifact_length_mismatches() {
    for name in ["memory.bin", "state.bin"] {
        for difference in [-1i64, 1] {
            let fixture = SnapshotFixture::new();
            let path = fixture.dir.join(OPENVMM_DIR).join(name);
            let file = fs::OpenOptions::new().write(true).open(path).unwrap();
            let length = file.metadata().unwrap().len();
            file.set_len(length.checked_add_signed(difference).unwrap())
                .unwrap();
            refused(&fixture.dir);
        }
    }
}

#[test]
fn snapshot_open_uses_the_strict_manifest_and_bounded_key_decoders() {
    use crate::host::store::files::write_private;

    for bytes in [vec![0x80], vec![0; super::MAX_MANIFEST_LEN as usize + 1]] {
        let fixture = SnapshotFixture::new();
        write_private(&fixture.dir.join(OPENVMM_DIR).join(MANIFEST_NAME), &bytes).unwrap();
        assert!(Snapshot::open(&fixture.dir).is_err());
    }
    let fixture = SnapshotFixture::new();
    let mut trailing = fixture.key.to_bytes();
    trailing.push(0);
    let mut version = fixture.key.to_bytes();
    version[0] += 1;
    for bytes in [
        vec![0],
        trailing,
        version,
        vec![0; SnapshotKey::MAX_LEN + 1],
    ] {
        write_private(&fixture.dir.join(KEY_FILE), &bytes).unwrap();
        assert!(Snapshot::open(&fixture.dir).is_err());
    }
}

#[test]
fn snapshot_open_refuses_a_key_from_another_manifest_machine() {
    use crate::host::store::files::write_private;

    let mutations: [fn(&mut ManifestSpec); 4] = [
        |spec| spec.cpu().digest[0] ^= 1,
        |spec| spec.contract().command_line.push_str(" extra"),
        |spec| spec.contract().devices[0].state_unit.push_str("-other"),
        |spec| spec.time().tsc_hz += 1,
    ];
    for mutate in mutations {
        let fixture = SnapshotFixture::new();
        let mut spec = fixture.spec.clone();
        mutate(&mut spec);
        let manifest = SnapshotManifest::parse(&spec.encode()).unwrap();
        let key = SnapshotKey::new(&class(), &manifest).unwrap();
        write_private(&fixture.dir.join(KEY_FILE), &key.to_bytes()).unwrap();
        refused(&fixture.dir);
    }
}

#[test]
fn snapshot_description_contents_do_not_affect_opening() {
    use crate::host::store::files::write_private;

    let fixture = SnapshotFixture::new();
    for contents in [b"".as_slice(), b"{broken JSON", &[0xff, 0, 0x80]] {
        write_private(&fixture.dir.join(DESCRIPTION_FILE), contents).unwrap();
        assert_eq!(Snapshot::open(&fixture.dir).unwrap().key(), &fixture.key);
    }
}

#[test]
fn snapshot_open_refuses_a_compatible_key_with_another_capture_time() {
    use crate::host::store::files::write_private;

    let mutations: &[fn(&mut ManifestSpec)] = &[
        |spec| spec.time().capture_tsc += 1,
        |spec| spec.time().capture_utc_ns += 1,
        |spec| spec.time().capture_monotonic_ns += 1,
        |spec| spec.time().host_clock = "windows-interrupt-time".to_owned(),
        |spec| spec.time().host_id[0] ^= 1,
        |spec| spec.time().boot_id[0] ^= 1,
        |spec| spec.time().lineage += 1,
        |spec| spec.time().cut_to_anchor_ns = None,
        |spec| spec.time().cut_to_anchor_ns = Some(0),
    ];
    for mutate in mutations {
        let fixture = SnapshotFixture::new();
        let mut spec = fixture.spec.clone();
        mutate(&mut spec);
        let manifest = SnapshotManifest::parse(&spec.encode()).unwrap();
        let key = SnapshotKey::new(&class(), &manifest).unwrap();
        assert_eq!(key.identity(), fixture.key.identity());
        write_private(&fixture.dir.join(KEY_FILE), &key.to_bytes()).unwrap();
        refused(&fixture.dir);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn snapshot_open_refuses_every_shared_file_and_both_shared_directories() {
    use std::os::unix::fs::PermissionsExt;

    for index in 0..5 {
        for mode in [0o640, 0o602, 0o610] {
            let fixture = SnapshotFixture::new();
            let path = &fixture.files()[index];
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            refused(&fixture.dir);
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
    }
    for nested in [false, true] {
        let fixture = SnapshotFixture::new();
        let path = if nested {
            fixture.dir.join(OPENVMM_DIR)
        } else {
            fixture.dir.clone()
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
        refused(&fixture.dir);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires root to change only the owned snapshot fixture's entry owners"]
fn snapshot_open_refuses_entries_owned_by_another_user() {
    use nix::unistd::{Uid, chown};

    assert!(Uid::effective().is_root());
    for index in 0..7 {
        let fixture = SnapshotFixture::new();
        let path = match index {
            5 => fixture.dir.clone(),
            6 => fixture.dir.join(OPENVMM_DIR),
            _ => fixture.files()[index].clone(),
        };
        chown(&path, Some(Uid::from_raw(1)), None).unwrap();
        refused(&fixture.dir);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn snapshot_open_refuses_file_links_fifos_and_sockets_without_reading_them() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    for index in 0..5 {
        let fixture = SnapshotFixture::new();
        let path = &fixture.files()[index];
        let saved = fixture.root.path().join("saved");
        fs::rename(path, &saved).unwrap();
        symlink(&saved, path).unwrap();
        refused(&fixture.dir);
        fs::remove_file(path).unwrap();
        nix::unistd::mkfifo(path, nix::sys::stat::Mode::S_IRUSR).unwrap();
        refused(&fixture.dir);
        fs::remove_file(path).unwrap();
        let _socket = UnixListener::bind(path).unwrap();
        refused(&fixture.dir);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn snapshot_open_refuses_directory_links_even_with_trailing_separators() {
    use std::os::unix::fs::symlink;

    let fixture = SnapshotFixture::new();
    let link = fixture.root.path().join("link");
    symlink(&fixture.dir, &link).unwrap();
    for suffix in ["", "/", "/.", "/./"] {
        let mut path = link.as_os_str().to_os_string();
        path.push(suffix);
        refused(Path::new(&path));
    }
    let saved = fixture.root.path().join("saved");
    let openvmm = fixture.dir.join(OPENVMM_DIR);
    fs::rename(&openvmm, &saved).unwrap();
    symlink(&saved, &openvmm).unwrap();
    refused(&fixture.dir);
}

#[cfg(target_os = "linux")]
#[test]
fn snapshot_description_needs_no_read_permission() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = SnapshotFixture::new();
    let path = fixture.dir.join(DESCRIPTION_FILE);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
    if !nix::unistd::Uid::effective().is_root() {
        assert_eq!(
            fs::read(&path).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
    assert_eq!(Snapshot::open(&fixture.dir).unwrap().key(), &fixture.key);
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o200
    );
}

#[cfg(windows)]
#[test]
fn snapshot_open_refuses_every_shared_file_and_both_shared_directory_acls() {
    use crate::host::platform::tests::{access, set_access, user_sid};

    let sid = user_sid();
    let shared = format!("D:P(A;;FA;;;{sid})(A;;FR;;;WD)");
    for index in 0..5 {
        let fixture = SnapshotFixture::new();
        let path = &fixture.files()[index];
        set_access(path, &shared).unwrap();
        let before = access(path).unwrap();
        refused(&fixture.dir);
        assert_eq!(access(path).unwrap(), before);
    }
    for nested in [false, true] {
        let fixture = SnapshotFixture::new();
        let path = if nested {
            fixture.dir.join(OPENVMM_DIR)
        } else {
            fixture.dir.clone()
        };
        set_access(&path, &shared).unwrap();
        let before = access(&path).unwrap();
        refused(&fixture.dir);
        assert_eq!(access(&path).unwrap(), before);
    }
}

#[cfg(windows)]
#[test]
fn snapshot_open_refuses_directory_junctions_and_every_reparse_file_entry() {
    use crate::host::tests::junction;

    let fixture = SnapshotFixture::new();
    let link = fixture.root.path().join("link");
    junction(&link, &fixture.dir);
    for path in [link.clone(), link.join("."), link.join("")] {
        refused(&path);
    }
    let saved = fixture.root.path().join("saved");
    let openvmm = fixture.dir.join(OPENVMM_DIR);
    fs::rename(&openvmm, &saved).unwrap();
    junction(&openvmm, &saved);
    refused(&fixture.dir);
    for index in 0..5 {
        let fixture = SnapshotFixture::new();
        let path = &fixture.files()[index];
        fs::remove_file(path).unwrap();
        junction(path, fixture.root.path());
        refused(&fixture.dir);
        fs::remove_dir(path).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn snapshot_description_needs_no_data_read_access() {
    use crate::host::platform::tests::{set_access, user_sid};

    let fixture = SnapshotFixture::new();
    let path = fixture.dir.join(DESCRIPTION_FILE);
    let sid = user_sid();
    set_access(&path, &format!("D:P(D;;0x1;;;{sid})(A;;FA;;;{sid})")).unwrap();
    assert_eq!(
        fs::read(&path).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(Snapshot::open(&fixture.dir).unwrap().key(), &fixture.key);
}

pub(in crate::host) struct RestoreFixture {
    snapshot: SnapshotFixture,
    pub(in crate::host) config: VmConfig,
    pub(in crate::host) runtime: Runtime,
    now: super::HostTime,
}

impl RestoreFixture {
    #[cfg(any(target_os = "linux", windows))]
    pub(in crate::host) fn current(nic: bool) -> Self {
        let mut fixture = Self::new(nic);
        fixture.retime(super::HostTime::now().unwrap());
        fixture
    }

    #[cfg(any(target_os = "linux", windows))]
    pub(in crate::host) fn retime(&mut self, now: super::HostTime) {
        self.now = now;
        let time = self.snapshot.spec.time();
        time.capture_utc_ns = now.utc_ns;
        time.capture_monotonic_ns = now.monotonic_ns;
        time.host_clock = now.clock.as_str().to_owned();
        time.host_id = now.host_id.to_vec();
        time.boot_id = now.boot_id.as_bytes().to_vec();
        time.cut_to_anchor_ns = Some(0);
        self.write_metadata();
    }

    pub(in crate::host) fn all_processors_online(&mut self) {
        self.snapshot.spec.contract().boot_online_vp_count = 0;
        self.config.processors = self.snapshot.spec.vp_count;
        self.write_metadata();
    }

    pub(in crate::host) fn console_listener(&mut self) {
        self.config.console = ConsoleAttachment::Listener;
        let console = &mut self.snapshot.spec.contract().attachments[0];
        console.reconnect_policy = "recreate-listener".to_owned();
        console.identity_kind = "unix-socket".to_owned();
        console.identity = b"/captured/boot-console".to_vec();
        self.write_metadata();
    }

    pub(in crate::host) fn new(nic: bool) -> Self {
        let mut snapshot = SnapshotFixture::new();
        snapshot.spec = if nic {
            let mut spec = ManifestSpec::with_network();
            spec.network().ipv4 = [10, 42, 0, 2];
            spec.network().gateway = [10, 42, 0, 1];
            spec
        } else {
            ManifestSpec::template()
        };
        snapshot.spec.vp_count = 4;
        snapshot.spec.contract().topology = (1, 1, 4, 1, vec![0, 1, 2, 3]);
        snapshot.spec.contract().boot_online_vp_count = 2;
        let image = snapshot.root.path().join("image.raw");
        let scratch = snapshot.root.path().join("scratch.raw");
        for (index, path, length) in [(0, &image, MIB), (1, &scratch, 2 * MIB)] {
            let file = crate::host::store::files::create_new(path).unwrap();
            crate::host::platform::make_sparse(&file).unwrap();
            file.set_len(length).unwrap();
            let geometry = crate::host::sandbox::block_geometry(&file, path, "fixture").unwrap();
            let block = &mut snapshot.spec.contract().blocks[index];
            block.length = if index == 0 { 4 * MIB } else { length };
            block.logical_block_size = geometry.logical_block_size;
            block.physical_block_size = geometry.physical_block_size;
        }
        let config = VmConfig {
            processors: 2,
            memory_mib: 512,
            root: RootConfig::Layered(LayeredRoot {
                image,
                format: RootImage::erofs(None),
                gpt_first_partition_bound: None,
                upper: UpperConfig::Scratch {
                    path: scratch,
                    journal: Journal::Forbidden,
                },
                overlay: OverlayMode::TrustedLowers,
            }),
            view: View {
                hostname: b"restored".to_vec(),
                machine_id: MachineIdMode::Generated,
            },
            network: nic.then(|| NetworkConfig {
                guest_network: None,
                policy: NetworkPolicy::new(Access::Deny),
            }),
            filesystem: None,
            console: ConsoleAttachment::Disconnected,
            boot: None,
        };
        let runtime = Runtime {
            openvmm: snapshot.root.path().join("unread-openvmm"),
            kernel: snapshot.root.path().join("unread-kernel"),
            initramfs: snapshot.root.path().join("unread-initramfs"),
            identity: class().runtime,
            hypervisor: Hypervisor::Kvm,
            cpu_profile: Some("intel.icelake-sp.v1".to_owned()),
            windows_breakaway: false,
            options: LaunchOptions::default(),
        };
        let now = super::HostTime {
            host_id: [1; 16],
            boot_id: BootId::from_bytes([2; 16]),
            clock: super::HostClock::LinuxBoottime,
            monotonic_ns: 123_000_001_000,
            utc_ns: 1_700_000_000_000_001_000,
        };
        let mut fixture = Self {
            snapshot,
            config,
            runtime,
            now,
        };
        fixture.write_metadata();
        fixture
    }

    fn write_metadata(&mut self) {
        let manifest = SnapshotManifest::parse(&self.snapshot.spec.encode()).unwrap();
        let mut class = class();
        class.processors = self.snapshot.spec.vp_count;
        class.boot_processors = match manifest.machine.boot_processors {
            0 => class.processors,
            count => count,
        };
        class.distro = manifest.machine.distro;
        class.scratch = manifest.machine.scratch;
        class.network = self
            .snapshot
            .spec
            .contract
            .as_ref()
            .unwrap()
            .network
            .as_ref()
            .map(|network| {
                SandboxNetwork::new(
                    &format!(
                        "{}/{}",
                        std::net::Ipv4Addr::from(network.ipv4),
                        network.prefix_length
                    ),
                    NetworkPolicy::new(Access::Allow),
                )
                .unwrap()
            });
        self.snapshot.key = SnapshotKey::new(&class, &manifest).unwrap();
        crate::host::store::files::write_private(
            &self.snapshot.dir.join(KEY_FILE),
            &self.snapshot.key.to_bytes(),
        )
        .unwrap();
        crate::host::store::files::write_private(
            &self.snapshot.dir.join(OPENVMM_DIR).join(MANIFEST_NAME),
            &self.snapshot.spec.encode(),
        )
        .unwrap();
    }

    pub(in crate::host) fn opened(&self) -> Snapshot {
        Snapshot::open(&self.snapshot.dir).unwrap()
    }

    fn refused(&self, field: &str) {
        let error = self
            .opened()
            .accepts(&self.config, &self.runtime, &self.now)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{field}: {error}");
        assert!(error.message().contains(field), "{field}: {error}");
    }
}

#[test]
fn a_restore_view_hard_links_only_the_fixed_openvmm_inventory() {
    let fixture = SnapshotFixture::new();
    let snapshot = Snapshot::open(&fixture.dir).unwrap();
    let destination = fixture.root.path().join("view");
    fs::create_dir(&destination).unwrap();
    let mut created = Vec::new();
    snapshot
        .link_restore_view(&destination, &mut created)
        .unwrap();
    assert_eq!(created.len(), super::SNAPSHOT_ARTIFACTS.len());
    assert_eq!(fs::read_dir(&destination).unwrap().count(), created.len());
    for path in &created {
        let source = fixture
            .dir
            .join(OPENVMM_DIR)
            .join(path.file_name().unwrap());
        assert_eq!(
            fs::metadata(path).unwrap().len(),
            fs::metadata(&source).unwrap().len()
        );
        assert_eq!(
            crate::host::platform::file_identity(path).unwrap(),
            crate::host::platform::file_identity(&source).unwrap()
        );
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            let original = fs::metadata(&source).unwrap();
            let linked = fs::metadata(path).unwrap();
            assert_eq!(
                (original.dev(), original.ino()),
                (linked.dev(), linked.ino())
            );
            assert_eq!(original.nlink(), 2);
        }
    }
    assert!(!destination.join(KEY_FILE).exists());
    assert!(!destination.join(DESCRIPTION_FILE).exists());
}

#[test]
fn partial_restore_links_report_the_original_error_and_only_created_entries() {
    use std::error::Error as _;

    let fixture = SnapshotFixture::new();
    let snapshot = Snapshot::open(&fixture.dir).unwrap();
    let destination = fixture.root.path().join("view");
    fs::create_dir(&destination).unwrap();
    let existing = destination.join("memory.bin");
    fs::write(&existing, b"caller-owned").unwrap();
    let mut created = Vec::new();
    let error = snapshot
        .link_restore_view(&destination, &mut created)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        error
            .source()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(created, [destination.join(MANIFEST_NAME)]);
    for path in created {
        fs::remove_file(path).unwrap();
    }
    assert_eq!(fs::read(&existing).unwrap(), b"caller-owned");
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
}

#[test]
fn snapshot_accepts_matching_real_disk_geometry_and_every_resource_boundary() {
    for nic in [false, true] {
        let mut fixture = RestoreFixture::new(nic);
        let snapshot = fixture.opened();
        for processors in [2, 4] {
            for memory_mib in (512..=2048).step_by(128) {
                fixture.config.processors = processors;
                fixture.config.memory_mib = memory_mib;
                snapshot
                    .accepts(&fixture.config, &fixture.runtime, &fixture.now)
                    .unwrap();
            }
        }
        let RootConfig::Layered(root) = &mut fixture.config.root else {
            unreachable!()
        };
        fs::OpenOptions::new()
            .write(true)
            .open(&root.image)
            .unwrap()
            .set_len(4 * MIB)
            .unwrap();
        root.upper = UpperConfig::Ram {
            max_memory_percent: 50,
        };
        snapshot
            .accepts(&fixture.config, &fixture.runtime, &fixture.now)
            .unwrap();
    }
}

#[test]
fn snapshot_accepts_refuses_a_blockless_root_and_each_processor_mismatch() {
    let mut fixture = RestoreFixture::new(false);
    fixture.config.root = RootConfig::Blockless;
    fixture.refused("layered root");
    for processors in [1, 8, 3] {
        let mut fixture = RestoreFixture::new(false);
        fixture.config.processors = processors;
        fixture.refused("processors");
    }
}

#[test]
fn snapshot_accepts_treats_zero_boot_processors_as_all_capacity_online() {
    let mut fixture = RestoreFixture::new(false);
    fixture.snapshot.spec.contract().boot_online_vp_count = 0;
    fixture.write_metadata();
    assert_eq!(fixture.opened().key().boot_processors(), 0);
    fixture.refused("processors");
    fixture.config.processors = 4;
    fixture
        .opened()
        .accepts(&fixture.config, &fixture.runtime, &fixture.now)
        .unwrap();
}

#[test]
fn snapshot_accepts_refuses_each_memory_mismatch_without_rounding_or_overflow() {
    for memory_mib in [384, 2176, 513] {
        let mut fixture = RestoreFixture::new(false);
        fixture.config.memory_mib = memory_mib;
        fixture.refused("memory");
    }
    for memory_mib in [u64::MAX, (1 << 44) + 512] {
        let mut fixture = RestoreFixture::new(false);
        fixture.config.memory_mib = memory_mib;
        fixture.refused("overflows");
    }
}

#[test]
fn snapshot_accepts_reports_missing_image_and_scratch_files_as_io() {
    for scratch in [false, true] {
        let fixture = RestoreFixture::new(false);
        let snapshot = fixture.opened();
        let RootConfig::Layered(root) = &fixture.config.root else {
            unreachable!()
        };
        let (path, description) = if scratch {
            let UpperConfig::Scratch { path, .. } = &root.upper else {
                unreachable!()
            };
            (path, "scratch")
        } else {
            (&root.image, "image")
        };
        fs::remove_file(path).unwrap();
        let error = snapshot
            .accepts(&fixture.config, &fixture.runtime, &fixture.now)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{description}: {error}");
        assert!(
            error.message().contains(description),
            "{description}: {error}"
        );
    }
}

#[test]
fn snapshot_accepts_refuses_an_image_longer_than_its_placeholder() {
    let fixture = RestoreFixture::new(false);
    let RootConfig::Layered(root) = &fixture.config.root else {
        unreachable!()
    };
    fs::OpenOptions::new()
        .write(true)
        .open(&root.image)
        .unwrap()
        .set_len(4 * MIB + 512)
        .unwrap();
    fixture.refused("image length");
}

#[test]
fn snapshot_accepts_refuses_each_image_block_size_mismatch() {
    for logical in [true, false] {
        let mut fixture = RestoreFixture::new(false);
        let block = &mut fixture.snapshot.spec.contract().blocks[0];
        if logical {
            block.logical_block_size *= 2;
        } else {
            block.physical_block_size *= 2;
        }
        fixture.write_metadata();
        fixture.refused("image block sizes");
    }
}

#[test]
fn snapshot_accepts_refuses_each_scratch_geometry_mismatch() {
    for field in 0..3 {
        let mut fixture = RestoreFixture::new(false);
        let block = &mut fixture.snapshot.spec.contract().blocks[1];
        match field {
            0 => block.length *= 2,
            1 => block.logical_block_size *= 2,
            2 => block.physical_block_size *= 2,
            _ => unreachable!(),
        }
        fixture.write_metadata();
        fixture.refused("scratch geometry");
    }
}

#[test]
fn snapshot_accepts_refuses_both_nic_presence_mismatches_and_each_named_address_mismatch() {
    for captured in [false, true] {
        let mut fixture = RestoreFixture::new(captured);
        fixture.config.network = (!captured).then(|| NetworkConfig {
            guest_network: None,
            policy: NetworkPolicy::new(Access::Deny),
        });
        fixture.refused("NIC presence");
    }
    for address in ["10.42.0.3/24", "10.42.0.2/25"] {
        let mut fixture = RestoreFixture::new(true);
        fixture.config.network.as_mut().unwrap().guest_network =
            Some(Ipv4Cidr::parse(address).unwrap());
        fixture.refused("NIC address");
    }
}

#[test]
fn snapshot_accepts_inherits_the_captured_address_and_does_not_compare_policies() {
    let mut fixture = RestoreFixture::new(true);
    let snapshot = fixture.opened();
    for address in [None, Some(Ipv4Cidr::parse("10.42.0.2/24").unwrap())] {
        for access in [Access::Deny, Access::Allow] {
            fixture.config.network = Some(NetworkConfig {
                guest_network: address,
                policy: NetworkPolicy::new(access),
            });
            snapshot
                .accepts(&fixture.config, &fixture.runtime, &fixture.now)
                .unwrap();
        }
    }
}

#[test]
fn snapshot_nic_returns_only_the_captured_presence_and_address_without_rereading() {
    for captured in [false, true] {
        let fixture = RestoreFixture::new(captured);
        let snapshot = fixture.opened();
        let expected = captured.then(|| NicConfig {
            guest_network: Ipv4Cidr::parse("10.42.0.2/24").unwrap(),
        });
        assert_eq!(snapshot.nic(), expected);
        fs::rename(
            &fixture.snapshot.dir,
            fixture.snapshot.root.path().join("saved"),
        )
        .unwrap();
        assert_eq!(snapshot.nic(), expected);
    }
}

#[test]
fn snapshot_open_refuses_captured_network_gateway_and_broadcast_addresses_as_io() {
    use crate::host::store::files::write_private;

    for address in [[10, 42, 0, 0], [10, 42, 0, 1], [10, 42, 0, 255]] {
        let mut fixture = RestoreFixture::new(true);
        let mut bytes = fixture.snapshot.key.to_bytes();
        let positions: Vec<_> = bytes
            .windows(4)
            .enumerate()
            .filter_map(|(index, bytes)| (bytes == [10, 42, 0, 2]).then_some(index))
            .collect();
        assert_eq!(
            positions.len(),
            1,
            "the fixture has one encoded NIC address"
        );
        bytes[positions[0]..positions[0] + 4].copy_from_slice(&address);
        fixture.snapshot.spec.network().ipv4 = address;
        let manifest = SnapshotManifest::parse(&fixture.snapshot.spec.encode()).unwrap();
        let key = SnapshotKey::from_bytes(&bytes).unwrap();
        key.check_manifest(&manifest).unwrap();
        write_private(&fixture.snapshot.dir.join(KEY_FILE), &bytes).unwrap();
        write_private(
            &fixture.snapshot.dir.join(OPENVMM_DIR).join(MANIFEST_NAME),
            &fixture.snapshot.spec.encode(),
        )
        .unwrap();
        let error = Snapshot::open(&fixture.snapshot.dir).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert!(error.message().contains("captured NIC address"), "{error}");
    }
}

#[test]
fn snapshot_accepts_requires_the_boot_console_attachment_not_the_control_listener_or_device() {
    let mut fixture = RestoreFixture::new(false);
    fixture.config.console = ConsoleAttachment::Listener;
    fixture.refused("boot console");
    let console = &mut fixture.snapshot.spec.contract().attachments[0];
    console.reconnect_policy = "recreate-listener".to_owned();
    console.identity_kind = "unix-socket".to_owned();
    console.identity = b"/captured/boot-console".to_vec();
    fixture.write_metadata();
    let snapshot = fixture.opened();
    snapshot
        .accepts(&fixture.config, &fixture.runtime, &fixture.now)
        .unwrap();
    fixture.config.console = ConsoleAttachment::Disconnected;
    fixture.refused("boot console");
}

#[test]
fn snapshot_accepts_refuses_missing_and_unknown_boot_console_policies() {
    for missing in [false, true] {
        let mut fixture = RestoreFixture::new(false);
        if missing {
            fixture
                .snapshot
                .spec
                .contract()
                .attachments
                .retain(|attachment| attachment.stable_id != "console:microvm-virtio0");
        } else {
            fixture.snapshot.spec.contract().attachments[0].reconnect_policy = "unknown".to_owned();
        }
        fixture.write_metadata();
        for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
            fixture.config.console = console;
            fixture.refused("boot console");
        }
    }
}

#[test]
fn snapshot_accepts_refuses_a_different_backend_and_each_cpu_profile_mismatch() {
    let mut fixture = RestoreFixture::new(false);
    fixture.runtime.hypervisor = Hypervisor::Mshv;
    fixture.refused("backend");
    for profile in ["host", "amd.genoa.v2"] {
        let mut fixture = RestoreFixture::new(false);
        fixture.runtime.cpu_profile = Some(profile.to_owned());
        fixture.refused("CPU profile");
    }
}

#[test]
fn snapshot_accepts_the_default_auto_and_matching_host_cpu_profile_requests() {
    for captured in ["intel.icelake-sp.v1", "intel.host.v1"] {
        let mut fixture = RestoreFixture::new(false);
        fixture.snapshot.spec.cpu().id = captured.to_owned();
        fixture.write_metadata();
        let snapshot = fixture.opened();
        for requested in [None, Some("auto"), Some(captured)] {
            fixture.runtime.cpu_profile = requested.map(str::to_owned);
            snapshot
                .accepts(&fixture.config, &fixture.runtime, &fixture.now)
                .unwrap();
        }
        if captured == "intel.host.v1" {
            fixture.runtime.cpu_profile = Some("host".to_owned());
            snapshot
                .accepts(&fixture.config, &fixture.runtime, &fixture.now)
                .unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn snapshot_accepts_refuses_nonregular_disks_without_waiting_for_a_fifo_writer() {
    use std::sync::mpsc;
    use std::time::Duration;

    for scratch in [false, true] {
        for fifo in [false, true] {
            let fixture = RestoreFixture::new(false);
            let snapshot = fixture.opened();
            let RootConfig::Layered(root) = &fixture.config.root else {
                unreachable!()
            };
            let path = if scratch {
                let UpperConfig::Scratch { path, .. } = &root.upper else {
                    unreachable!()
                };
                path
            } else {
                &root.image
            };
            fs::remove_file(path).unwrap();
            if fifo {
                nix::unistd::mkfifo(path, nix::sys::stat::Mode::S_IRUSR).unwrap();
            } else {
                fs::create_dir(path).unwrap();
            }
            let config = fixture.config.clone();
            let runtime = fixture.runtime.clone();
            let now = fixture.now;
            let (send, receive) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                send.send(snapshot.accepts(&config, &runtime, &now))
                    .unwrap();
            });
            let error = receive
                .recv_timeout(Duration::from_secs(5))
                .expect("disk geometry must not wait for a FIFO writer")
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert!(error.message().contains("regular file"), "{error}");
            worker.join().unwrap();
        }
    }
}

#[test]
fn snapshot_accepts_refuses_every_runtime_identity_part_independently() {
    let changes: [fn(&mut RuntimeIdentity); 6] = [
        |identity| identity.openvmm = artifact('d'),
        |identity| identity.kernel = artifact('d'),
        |identity| identity.initramfs = artifact('d'),
        |identity| identity.boot_contract += 1,
        |identity| identity.control_contract.push_str("-other"),
        |identity| identity.control_session += 1,
    ];
    for change in changes {
        let mut fixture = RestoreFixture::new(false);
        change(&mut fixture.runtime.identity);
        fixture.refused("runtime identity");
    }
}

#[test]
fn snapshot_accepts_reports_early_and_expired_time_as_unavailable() {
    let fixture = RestoreFixture::new(false);
    let snapshot = fixture.opened();
    let anchor = snapshot.manifest().time();
    for monotonic_ns in [
        anchor.capture_monotonic_ns - 1,
        anchor.capture_monotonic_ns + super::MAX_DOWNTIME_NS,
    ] {
        let now = super::HostTime {
            monotonic_ns,
            ..fixture.now
        };
        let error = snapshot
            .accepts(&fixture.config, &fixture.runtime, &now)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    }
    let now = super::HostTime {
        monotonic_ns: anchor.capture_monotonic_ns + super::MAX_DOWNTIME_NS - 200,
        ..fixture.now
    };
    snapshot
        .accepts(&fixture.config, &fixture.runtime, &now)
        .unwrap();
    let now = super::HostTime {
        host_id: [3; 16],
        utc_ns: anchor.capture_utc_ns - 1,
        ..fixture.now
    };
    assert_eq!(
        snapshot
            .accepts(&fixture.config, &fixture.runtime, &now)
            .unwrap_err()
            .kind(),
        ErrorKind::Unavailable
    );
}

#[test]
fn snapshot_accepts_never_reopens_snapshot_paths_or_reads_runtime_artifacts() {
    let fixture = RestoreFixture::new(true);
    let snapshot = fixture.opened();
    fs::rename(
        &fixture.snapshot.dir,
        fixture.snapshot.root.path().join("saved"),
    )
    .unwrap();
    snapshot
        .accepts(&fixture.config, &fixture.runtime, &fixture.now)
        .unwrap();
}
