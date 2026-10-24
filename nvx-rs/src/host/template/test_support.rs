//! Builders of the manifests and capture classes of templates, for their tests.
//!
//! [`Proto`] writes the protobuf wire format as OpenVMM's `mesh` does, independently of the reader
//! that the tests check, and [`ManifestSpec`] holds each field of a manifest, so that a test
//! changes one field and encodes the others as they were.

use sha2::{Digest, Sha256};

use super::{BlockGeometry, CaptureClass, Invariants};
use crate::host::network::{Access, NetworkPolicy};
use crate::host::sandbox::SandboxNetwork;
use crate::host::store::{ArtifactIdentity, RuntimeIdentity};
use crate::hypervisor::Hypervisor;

/// Bytes in a MiB.
pub(super) const MIB: u64 = 1024 * 1024;

/// A protobuf message under construction, whose fields are written in the order of the calls.
///
/// Like `mesh`, the typed methods omit a field whose value is zero or empty, but for
/// [`message`](Self::message) and [`some`](Self::some), which write an `Option` that is `Some`.
#[derive(Debug, Clone, Default)]
pub(super) struct Proto(Vec<u8>);

impl Proto {
    /// Appends `value` to `bytes` as a varint.
    pub(super) fn put_varint(bytes: &mut Vec<u8>, mut value: u64) {
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
    pub(super) fn key(mut self, number: u32, wire_type: u8) -> Self {
        Self::put_varint(&mut self.0, u64::from(number) << 3 | u64::from(wire_type));
        self
    }

    /// Appends field `number` as the varint `value`, even if it is 0.
    pub(super) fn some(self, number: u32, value: u64) -> Self {
        let mut message = self.key(number, 0);
        Self::put_varint(&mut message.0, value);
        message
    }

    /// Appends field `number` as the varint `value`, unless it is 0.
    pub(super) fn varint(self, number: u32, value: u64) -> Self {
        if value == 0 {
            self
        } else {
            self.some(number, value)
        }
    }

    /// Appends field `number` as the bytes `bytes`, even if they are empty.
    pub(super) fn always_bytes(self, number: u32, bytes: &[u8]) -> Self {
        let mut message = self.key(number, 2);
        Self::put_varint(&mut message.0, bytes.len() as u64);
        message.0.extend_from_slice(bytes);
        message
    }

    /// Appends field `number` as the bytes `bytes`, unless they are empty.
    pub(super) fn bytes(self, number: u32, bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            self
        } else {
            self.always_bytes(number, bytes)
        }
    }

    /// Appends field `number` as the text `text`, unless it is empty.
    pub(super) fn text(self, number: u32, text: &str) -> Self {
        self.bytes(number, text.as_bytes())
    }

    /// Appends field `number` as the message `message`, even if it is empty, as `mesh` writes an
    /// `Option` that is `Some`, or an element of a repeated field.
    pub(super) fn message(self, number: u32, message: Proto) -> Self {
        self.always_bytes(number, &message.0)
    }

    /// Appends field `number` as the packed varints `values`, unless there are none.
    pub(super) fn packed(self, number: u32, values: &[u32]) -> Self {
        let mut packed = Vec::new();
        for &value in values {
            Self::put_varint(&mut packed, u64::from(value));
        }
        self.bytes(number, &packed)
    }

    /// Appends `bytes` as they are.
    pub(super) fn raw(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }

    /// Returns the message's bytes.
    pub(super) fn finish(self) -> Vec<u8> {
        self.0
    }
}

/// `openvmm.snapshot.SnapshotDevice`.
#[derive(Debug, Clone)]
pub(super) struct DeviceSpec {
    /// Field 1.
    pub(super) stable_id: String,
    /// Field 2.
    pub(super) state_unit: String,
    /// Field 3.
    pub(super) kind: String,
    /// Field 4.
    pub(super) order: u32,
    /// Field 5: each range's address space, start, and length.
    pub(super) ranges: Vec<(String, u64, u64)>,
    /// Field 6.
    pub(super) irq: Option<u32>,
    /// Field 7.
    pub(super) transport: String,
    /// Field 8.
    pub(super) feature_banks: Vec<u32>,
    /// Field 9.
    pub(super) queue_count: u32,
    /// Field 10.
    pub(super) queue_max_sizes: Vec<u32>,
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
pub(super) struct AttachmentSpec {
    /// Field 1.
    pub(super) stable_id: String,
    /// Field 2.
    pub(super) kind: String,
    /// Field 3.
    pub(super) required: bool,
    /// Field 4.
    pub(super) reconnect_policy: String,
    /// Field 5.
    pub(super) identity_kind: String,
    /// Field 6.
    pub(super) identity: Vec<u8>,
    /// Field 7.
    pub(super) length: u64,
    /// Field 8.
    pub(super) reconnect_timeout_ms: u64,
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
pub(super) struct NetworkSpec {
    /// Field 1.
    pub(super) ipv4: [u8; 4],
    /// Field 2.
    pub(super) prefix_length: u32,
    /// Field 3.
    pub(super) gateway: [u8; 4],
    /// Field 4.
    pub(super) guest_mac: Vec<u8>,
    /// Field 5.
    pub(super) gateway_mac: Vec<u8>,
    /// Field 6.
    pub(super) policy_mode: String,
    /// Field 7.
    pub(super) policy_digest: Vec<u8>,
    /// Field 8.
    pub(super) policy_required: bool,
    /// Field 9.
    pub(super) profile: String,
    /// Field 10.
    pub(super) policy_encoding_version: u32,
    /// Field 11.
    pub(super) ipv6: Vec<u8>,
    /// Field 12.
    pub(super) ipv6_prefix_length: u32,
    /// Field 13.
    pub(super) ipv6_gateway: Vec<u8>,
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
pub(super) struct BlockSpec {
    /// Field 1.
    pub(super) role: String,
    /// Field 2.
    pub(super) read_only: bool,
    /// Field 3.
    pub(super) length: u64,
    /// Field 4.
    pub(super) identity_kind: String,
    /// Field 5.
    pub(super) identity: Vec<u8>,
    /// Field 6.
    pub(super) artifact: String,
    /// Field 7.
    pub(super) logical_block_size: u32,
    /// Field 8.
    pub(super) physical_block_size: u32,
    /// Field 9.
    pub(super) restore_mode: String,
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
pub(super) struct TimeSpec {
    /// Field 1.
    pub(super) abi_version: u32,
    /// Field 2.
    pub(super) tsc_hz: u64,
    /// Field 3.
    pub(super) tolerance_ppm: u32,
    /// Field 4.
    pub(super) apic_hz: u64,
    /// Field 5.
    pub(super) capture_tsc: u64,
    /// Field 6.
    pub(super) capture_utc_ns: u64,
    /// Field 7.
    pub(super) capture_monotonic_ns: u64,
    /// Field 8.
    pub(super) host_clock: String,
    /// Field 9.
    pub(super) host_id: Vec<u8>,
    /// Field 10.
    pub(super) boot_id: Vec<u8>,
    /// Field 11.
    pub(super) lineage: u32,
    /// Field 12.
    pub(super) cut_to_anchor_ns: Option<u64>,
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
pub(super) struct CpuSpec {
    /// Field 1.
    pub(super) id: String,
    /// Field 2.
    pub(super) digest: Vec<u8>,
    /// Field 3.
    pub(super) document: Vec<u8>,
    /// Field 4.
    pub(super) effective_cpuid: Vec<u8>,
    /// Field 6.
    pub(super) signature: u32,
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
pub(super) struct ContractSpec {
    /// Field 1.
    pub(super) machine_profile: String,
    /// Field 2.
    pub(super) abi_version: u32,
    /// Field 3.
    pub(super) source_hypervisor: String,
    /// Field 4.
    pub(super) command_line: String,
    /// Field 5, or `None` for the SHA-256 digest of the command line.
    pub(super) command_line_digest: Option<Vec<u8>>,
    /// Field 6: each range's start, length, and file offset.
    pub(super) memory_ranges: Vec<(u64, u64, u64)>,
    /// Field 7: the sockets, the dies of each socket, the cores of each die, the threads of each
    /// core, and the APIC IDs.
    pub(super) topology: (u32, u32, u32, u32, Vec<u32>),
    /// Field 8.
    pub(super) devices: Vec<DeviceSpec>,
    /// Field 9.
    pub(super) state_units: Vec<String>,
    /// Field 10.
    pub(super) attachments: Vec<AttachmentSpec>,
    /// Field 16.
    pub(super) boot_layout_version: u32,
    /// Field 18.
    pub(super) network: Option<NetworkSpec>,
    /// Field 19: the encoded policy of an attached filesystem.
    pub(super) filesystem: Option<Vec<u8>>,
    /// Field 21.
    pub(super) blocks: Vec<BlockSpec>,
    /// Field 22.
    pub(super) filesystem_slot_version: u32,
    /// Field 23.
    pub(super) boot_online_vp_count: u32,
    /// Field 24.
    pub(super) interrupt_mode: String,
    /// Fields 25 and 26: the start and the length of the shared interrupt-status page.
    pub(super) status_page: (u64, u64),
    /// Field 27.
    pub(super) expansion_version: u32,
    /// Field 28.
    pub(super) memory_capacity: u64,
    /// Field 29.
    pub(super) memory_block_size: u64,
    /// Field 30: each range's start and length.
    pub(super) expansion_ranges: Vec<(u64, u64)>,
    /// Field 31.
    pub(super) time: Option<TimeSpec>,
    /// Field 32.
    pub(super) cpu: Option<CpuSpec>,
}

impl ContractSpec {
    /// Gives each device its index as its order.
    pub(super) fn renumber(&mut self) {
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
        message = message.varint(16, self.boot_layout_version.into());
        if let Some(network) = &self.network {
            message = message.message(18, network.encode());
        }
        if let Some(filesystem) = &self.filesystem {
            message = message.always_bytes(19, filesystem);
        }
        for block in &self.blocks {
            message = message.message(21, block.encode());
        }
        message = message
            .varint(22, self.filesystem_slot_version.into())
            .varint(23, self.boot_online_vp_count.into())
            .text(24, &self.interrupt_mode)
            .varint(25, self.status_page.0)
            .varint(26, self.status_page.1)
            .varint(27, self.expansion_version.into())
            .varint(28, self.memory_capacity)
            .varint(29, self.memory_block_size);
        for &(start, length) in &self.expansion_ranges {
            message = message.message(30, Proto::default().varint(1, start).varint(2, length));
        }
        if let Some(time) = &self.time {
            message = message.message(31, time.encode());
        }
        if let Some(cpu) = &self.cpu {
            message = message.message(32, cpu.encode());
        }
        message
    }
}

/// `openvmm.snapshot.SnapshotManifest`.
#[derive(Debug, Clone)]
pub(super) struct ManifestSpec {
    /// Field 1.
    pub(super) version: u32,
    /// Field 2: seconds and nanoseconds.
    pub(super) created_at: (i64, i32),
    /// Field 3.
    pub(super) openvmm_version: String,
    /// Field 4.
    pub(super) memory_size: u64,
    /// Field 5.
    pub(super) vp_count: u32,
    /// Field 6.
    pub(super) page_size: u32,
    /// Field 7.
    pub(super) architecture: String,
    /// Field 8.
    pub(super) state_size: u64,
    /// Field 11.
    pub(super) contract: Option<ContractSpec>,
    /// Field 12.
    pub(super) magic: Vec<u8>,
    /// Field 13.
    pub(super) schema_version: u32,
    /// Field 14.
    pub(super) root_type: String,
    /// Field 15.
    pub(super) tier: String,
    /// Field 16.
    pub(super) restore_policy: String,
    /// Field 17.
    pub(super) consumed_sections: u32,
}

/// The kernel command line of the template of [`ManifestSpec::template`].
const COMMAND_LINE: &str = "earlycon=xe9 console=hvc1 reboot=t panic=-1 nvx_sandbox=1 \
                            nvx_config=0xd0010000,65536 nr_cpus=2 nvx_snapshot_tier=platform \
                            virtio_mmio.device=0x1000@0xd0001000:6 \
                            virtio_mmio.device=0x1000@0xd0002000:7 \
                            virtio_mmio.device=0x1000@0xd0003000:4 \
                            virtio_mmio.device=0x1000@0xd0006000:11 \
                            virtio_mmio.device=0x1000@0xd0007000:3 nvx_control_tty=hvc2";

impl ManifestSpec {
    /// Returns the manifest of a template: a KVM microVM of 2 processors, all online at boot, and
    /// 512 MiB of RAM, of a capacity of 2 GiB, with a `distro` of 1 GiB, a scratch of 64 MiB, the
    /// dormant filesystem slot, a console, a control console, and no NIC.
    pub(super) fn template() -> Self {
        let (pmio, mmio) = ("pmio", "mmio");
        let platform = DeviceSpec::platform;
        let mut contract = ContractSpec {
            machine_profile: "microvm".to_owned(),
            abi_version: 2,
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
                    6,
                    0x3_3000_0000,
                    2,
                ),
                DeviceSpec::virtio(
                    "console:microvm-virtio0",
                    "virtio-console",
                    "virtio-console",
                    0xd000_2000,
                    7,
                    0x3_3000_0001,
                    2,
                ),
                DeviceSpec::virtio(
                    "blk:sandbox:distro",
                    "virtio-blk",
                    "virtio-blk",
                    0xd000_3000,
                    4,
                    0x3_3000_0664,
                    1,
                ),
                DeviceSpec::virtio(
                    "blk:sandbox:scratch",
                    "virtio-blk",
                    "virtio-blk",
                    0xd000_6000,
                    11,
                    0x3_3000_0644,
                    1,
                ),
                DeviceSpec::virtio(
                    "console:microvm-control0",
                    "virtio-control-console",
                    "virtio-control-console",
                    0xd000_7000,
                    3,
                    0x3_3000_0001,
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
            boot_layout_version: 2,
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
                abi_version: 1,
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
            version: 6,
            created_at: (1_761_300_000, 123_456_789),
            openvmm_version: "0.0.0".to_owned(),
            memory_size: 512 * MIB,
            vp_count: 2,
            page_size: 4096,
            architecture: "x86_64".to_owned(),
            state_size: 65_536,
            contract: Some(contract),
            magic: b"OPENVMM_SNAPSHOT_V6\0".to_vec(),
            schema_version: 1,
            root_type: "openvmm.SavedState".to_owned(),
            tier: "platform".to_owned(),
            restore_policy: "clone".to_owned(),
            consumed_sections: 1,
        }
    }

    /// Returns the manifest of [`template`](Self::template) with a fixed NIC at `10.0.0.2/24`,
    /// with an IPv6 identity, under an allow-list policy.
    pub(super) fn with_network() -> Self {
        let mut spec = Self::template();
        let contract = spec.contract();
        contract.devices.insert(
            11,
            DeviceSpec::virtio(
                "net:microvm0",
                "virtio-net",
                "virtio-net",
                0xd000_0000,
                10,
                0x1_0000_0020,
                2,
            ),
        );
        contract.renumber();
        contract
            .state_units
            .push("virtio-net-3489660928".to_owned());
        contract.command_line = contract.command_line.replacen(
            "virtio_mmio.device=0x1000@0xd0001000:6",
            "virtio_mmio.device=0x1000@0xd0000000:10 virtio_mmio.device=0x1000@0xd0001000:6",
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
            policy_encoding_version: 4,
            ipv6: ipv6.to_vec(),
            ipv6_prefix_length: 120,
            ipv6_gateway: ipv6_gateway.to_vec(),
        });
        spec
    }

    /// Returns the machine contract.
    pub(super) fn contract(&mut self) -> &mut ContractSpec {
        self.contract.as_mut().expect("the spec has a contract")
    }

    /// Returns the time contract.
    pub(super) fn time(&mut self) -> &mut TimeSpec {
        self.contract()
            .time
            .as_mut()
            .expect("the spec has a time contract")
    }

    /// Returns the CPU profile record.
    pub(super) fn cpu(&mut self) -> &mut CpuSpec {
        self.contract()
            .cpu
            .as_mut()
            .expect("the spec has a CPU profile")
    }

    /// Returns the fixed NIC.
    pub(super) fn network(&mut self) -> &mut NetworkSpec {
        self.contract()
            .network
            .as_mut()
            .expect("the spec has a network")
    }

    /// Encodes the manifest, with the fields of `extra` after the others.
    pub(super) fn encode_with(&self, extra: Proto) -> Vec<u8> {
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
            message = message.message(11, contract.encode());
        }
        message
            .bytes(12, &self.magic)
            .varint(13, self.schema_version.into())
            .text(14, &self.root_type)
            .text(15, &self.tier)
            .text(16, &self.restore_policy)
            .varint(17, self.consumed_sections.into())
            .raw(&extra.finish())
            .finish()
    }

    /// Encodes the manifest.
    pub(super) fn encode(&self) -> Vec<u8> {
        self.encode_with(Proto::default())
    }
}

/// Returns the artifact identity `sha256:` and 64 times `digit`.
pub(super) fn artifact(digit: char) -> ArtifactIdentity {
    ArtifactIdentity::sha256(&digit.to_string().repeat(64)).unwrap()
}

/// Returns the class of the capture of [`ManifestSpec::template`].
pub(super) fn class() -> CaptureClass {
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
pub(super) fn network_class() -> CaptureClass {
    CaptureClass {
        network: Some(
            SandboxNetwork::new("10.0.0.2/24", NetworkPolicy::new(Access::Allow)).unwrap(),
        ),
        ..class()
    }
}
