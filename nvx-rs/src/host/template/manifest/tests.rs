//! Tests of [`TemplateManifest`]: what it reads of a template's manifest, and what it refuses.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::*;
use crate::host::template::test_support::{MIB, ManifestSpec, Proto};

/// Returns the manifest that the encoding of `spec` parses to.
fn parse(spec: &ManifestSpec) -> TemplateManifest {
    TemplateManifest::parse(&spec.encode()).unwrap()
}

/// Returns the message of the refusal of `bytes`, which must have the code `code`.
fn refusal(bytes: &[u8], code: ErrorCode) -> String {
    let error = TemplateManifest::parse(bytes).expect_err("the manifest is refused");
    assert_eq!(error.code(), code, "{error}");
    error.message().to_owned()
}

/// Asserts that the manifest of a template is refused with `code` and a message that holds
/// `expected` once `change` changes it.
fn assert_refused(change: impl FnOnce(&mut ManifestSpec), code: ErrorCode, expected: &str) {
    let mut spec = ManifestSpec::template();
    change(&mut spec);
    let message = refusal(&spec.encode(), code);
    assert!(message.contains(expected), "{message:?} lacks {expected:?}");
}

#[test]
fn a_templates_manifest_is_read_whole() {
    let mut spec = ManifestSpec::template();
    let manifest = parse(&spec);
    assert_eq!(
        manifest.created_at(),
        SystemTime::UNIX_EPOCH + Duration::new(1_761_300_000, 123_456_789)
    );
    assert_eq!(manifest.openvmm_version(), "0.0.0");
    assert_eq!(manifest.hypervisor(), Hypervisor::Kvm);
    assert_eq!(manifest.state_len(), 65_536);
    assert_eq!(manifest.memory_len(), 512 * MIB);
    assert_eq!(
        manifest.time(),
        &TimeContract {
            tsc_frequency_hz: 2_100_000_000,
            tsc_tolerance_ppm: 250,
            apic_frequency_hz: 200_000_000,
            capture_tsc: 12_345_678,
            capture_utc_ns: 1_700_000_000_000_000_000,
            capture_monotonic_ns: 123_000_000_000,
            host_clock: HostClock::LinuxBoottime,
            host_id: [1; 16],
            boot_id: BootId::from_bytes([2; 16]),
            lineage: 3,
            cut_to_anchor_ns: Some(200),
        }
    );
    let machine = &manifest.machine;
    assert_eq!(machine.cpu.id(), "intel.icelake-sp.v1");
    assert_eq!(machine.cpu.digest, [0x5a; 32]);
    assert_eq!(machine.cpu.effective_cpuid, (0..88).collect::<Vec<u8>>());
    assert_eq!(
        machine.topology,
        Topology {
            sockets: 1,
            dies_per_socket: 1,
            cores_per_die: 2,
            threads_per_core: 1,
            apic_ids: vec![0, 1],
        }
    );
    assert_eq!(
        machine.rates,
        Rates {
            tsc_hz: 2_100_000_000,
            apic_hz: 200_000_000,
            tolerance_ppm: 250,
        }
    );
    assert_eq!((machine.processors, machine.boot_processors), (2, 0));
    assert_eq!(
        machine.memory,
        Memory {
            boot: 512 * MIB,
            capacity: 2048 * MIB,
            block_size: 128 * MIB,
            ranges: vec![MemoryRange {
                start: 0,
                length: 512 * MIB,
            }],
            expansion: vec![MemoryRange {
                start: 512 * MIB,
                length: 1536 * MIB,
            }],
        }
    );
    assert_eq!(machine.command_line, spec.contract().command_line);
    assert_eq!(
        machine.command_line_digest,
        <[u8; 32]>::from(Sha256::digest(&machine.command_line))
    );
    assert_eq!(
        (machine.distro, machine.scratch),
        (
            BlockGeometry {
                length: 1024 * MIB,
                logical_block_size: 512,
                physical_block_size: 4096,
            },
            BlockGeometry {
                length: 64 * MIB,
                logical_block_size: 512,
                physical_block_size: 512,
            }
        )
    );
    assert_eq!(
        machine.interrupts,
        Interrupts {
            mode: "edge-shared-status".to_owned(),
            status_page_start: 0x3_0000,
            status_page_length: 0x1000,
        }
    );
    let ids: Vec<&str> = machine
        .devices
        .iter()
        .map(|device| device.stable_id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "partition",
            "vp0",
            "vmtime",
            "pic",
            "ioapic",
            "lapic",
            "pit",
            "rtc",
            "microvm-portb",
            "microvm-shutdown",
            "microvm-snapshot-request",
            "fs:microvm0",
            "console:microvm-virtio0",
            "blk:sandbox:distro",
            "blk:sandbox:scratch",
            "console:microvm-control0",
        ]
    );
    assert_eq!(
        machine.devices[3],
        Device {
            stable_id: "pic".to_owned(),
            kind: "pic".to_owned(),
            state_unit: "pic".to_owned(),
            transport: String::new(),
            ranges: vec![
                DeviceRange {
                    space: AddressSpace::Pmio,
                    start: 0x20,
                    length: 2,
                },
                DeviceRange {
                    space: AddressSpace::Pmio,
                    start: 0xa0,
                    length: 2,
                },
            ],
            irq: None,
            feature_banks: Vec::new(),
            queue_count: 0,
            queue_max_sizes: Vec::new(),
        }
    );
    // An interrupt of 0 is an interrupt, unlike none.
    assert_eq!(machine.devices[6].irq, Some(0));
    assert_eq!(
        machine.devices[13],
        Device {
            stable_id: "blk:sandbox:distro".to_owned(),
            kind: "virtio-blk".to_owned(),
            state_unit: "virtio-blk-3489673216".to_owned(),
            transport: "virtio-mmio".to_owned(),
            ranges: vec![DeviceRange {
                space: AddressSpace::Mmio,
                start: 0xd000_3000,
                length: 0x1000,
            }],
            irq: Some(4),
            feature_banks: vec![0x3000_0664, 3],
            queue_count: 1,
            queue_max_sizes: vec![256],
        }
    );
    assert_eq!(machine.filesystem_slot, 1);
    assert_eq!(machine.nic, None);
}

#[test]
fn a_fixed_nic_is_read_with_the_identity_of_its_policy() {
    let manifest = parse(&ManifestSpec::with_network());
    assert_eq!(
        manifest.machine.nic,
        Some(Nic {
            profile: "portable".to_owned(),
            guest_mac: [0x52, 0x54, 0, 0, 0, 2],
            gateway_mac: [0x52, 0x54, 0, 0, 0, 1],
            ipv4: Ipv4Addr::new(10, 0, 0, 2),
            prefix_length: 24,
            gateway: Ipv4Addr::new(10, 0, 0, 1),
            ipv6: Some(Ipv6Identity {
                address: "fd00::a00:2".parse().unwrap(),
                prefix_length: 120,
                gateway: "fd00::a00:1".parse().unwrap(),
            }),
            policy: PolicyIdentity {
                mode: "allow-list".to_owned(),
                digest: [7; 32],
                required: true,
                encoding_version: 4,
            },
        })
    );
    assert_eq!(manifest.machine.devices[11].stable_id, "net:microvm0");
}

#[test]
fn the_fields_of_a_manifest_need_not_be_written_in_their_order() {
    // `mesh` writes a message's fields in the order of its type's declaration, which may differ
    // from the order of their numbers, as the network's profile, field 9, comes first.
    let mut spec = ManifestSpec::template();
    let reversed = Proto::default()
        .varint(17, 1)
        .text(16, "clone")
        .text(15, "platform")
        .text(14, "openvmm.SavedState")
        .varint(13, 1)
        .bytes(12, b"OPENVMM_SNAPSHOT_V6\0");
    spec.tier.clear();
    spec.restore_policy.clear();
    spec.root_type.clear();
    spec.schema_version = 0;
    spec.consumed_sections = 0;
    spec.magic.clear();
    let bytes = spec.encode_with(reversed);
    assert_eq!(
        TemplateManifest::parse(&bytes).unwrap(),
        parse(&ManifestSpec::template())
    );
}

#[test]
fn other_versions_of_the_format_are_unsupported() {
    let unsupported = ErrorCode::FormatUnsupported;
    for version in [0, 1, 5, 7, u32::MAX] {
        assert_refused(
            |spec| spec.version = version,
            unsupported,
            &format!(
                "the snapshot manifest has format version {version}, which this build does not \
                 read; capture the template again"
            ),
        );
    }
    assert_refused(
        |spec| spec.magic = b"OPENVMM_SNAPSHOT_V5\0".to_vec(),
        unsupported,
        "has another format magic than its version's",
    );
    assert_refused(
        |spec| spec.magic.clear(),
        unsupported,
        "has another format magic",
    );
    assert_refused(
        |spec| spec.schema_version = 2,
        unsupported,
        "has saved-state schema version 2",
    );
    assert_refused(
        |spec| spec.root_type = "openvmm.OtherState".to_owned(),
        unsupported,
        "of root type \"openvmm.OtherState\"",
    );
    assert_refused(
        |spec| spec.contract().machine_profile = "standard".to_owned(),
        unsupported,
        "describes a machine of profile \"standard\"",
    );
    for version in [1, 3] {
        assert_refused(
            |spec| spec.contract().abi_version = version,
            unsupported,
            &format!("ABI version {version}"),
        );
    }
    assert_refused(
        |spec| spec.contract().boot_layout_version = 1,
        unsupported,
        "boot layout version 1",
    );
    assert_refused(
        |spec| spec.time().abi_version = 2,
        unsupported,
        "has a time contract of time ABI version 2",
    );
    // A manifest of another version is reported as such, even with fields that this build does
    // not know, such as a field that version 6 retired.
    let mut spec = ManifestSpec::template();
    spec.version = 5;
    let message = refusal(
        &spec.encode_with(Proto::default().varint(9, 1).varint(40, 1)),
        unsupported,
    );
    assert!(message.contains("has format version 5"), "{message}");
}

#[test]
fn fields_that_the_format_does_not_have_are_refused() {
    let template = ManifestSpec::template();
    // Field 9 is retired, and field 18 does not exist yet.
    for number in [9, 10, 18, 63, 64, 1000] {
        let message = refusal(
            &template.encode_with(Proto::default().varint(number, 1)),
            ErrorCode::Io,
        );
        assert_eq!(
            message,
            format!("the snapshot manifest is malformed: it holds the unknown field {number}")
        );
    }
    let unknown = Proto::default().varint(40, 1).finish();
    for (decoded, what) in [
        (decode_contract(&unknown).map(drop), "its machine contract"),
        (decode_time(&unknown).map(drop), "its time contract"),
        (decode_cpu_profile(&unknown).map(drop), "its CPU profile"),
        (decode_network(&unknown).map(drop), "its network"),
        (decode_block(&unknown).map(drop), "its sandbox block"),
        (decode_device(&unknown, 2).map(drop), "its device 2"),
        (
            decode_device_range(&unknown, 2).map(drop),
            "a range of its device 2",
        ),
        (decode_attachment(&unknown, 1), "its attachment 1"),
        (decode_memory_range(&unknown).map(drop), "its memory range"),
        (
            decode_expansion_range(&unknown).map(drop),
            "its memory expansion range",
        ),
        (
            decode_topology(&unknown).map(drop),
            "its processor topology",
        ),
        (decode_timestamp(&unknown).map(drop), "its creation time"),
    ] {
        assert_eq!(decoded, Err(format!("{what} holds the unknown field 40")));
    }
    // The CPU signature, field 6, is known, but field 5 is not.
    assert_eq!(
        decode_cpu_profile(&Proto::default().varint(5, 1).finish()).map(drop),
        Err("its CPU profile holds the unknown field 5".to_owned())
    );
}

#[test]
fn malformed_manifests_are_refused() {
    let template = ManifestSpec::template();
    let malformed = |bytes: &[u8]| refusal(bytes, ErrorCode::Io);
    // A field that is not repeated, twice.
    assert_eq!(
        malformed(&template.encode_with(Proto::default().varint(5, 2))),
        "the snapshot manifest is malformed: it holds field 5 more than once"
    );
    // A field of another type than its own.
    assert_eq!(
        decode_topology(&Proto::default().text(1, "one").finish()).map(drop),
        Err("its processor topology holds field 1, which is not an integer".to_owned())
    );
    assert_eq!(
        decode_contract(&Proto::default().varint(4, 1).finish()).map(drop),
        Err("its machine contract holds field 4, which is not bytes".to_owned())
    );
    // A text that is not UTF-8, and a number that does not fit its type.
    let mut spec = template.clone();
    spec.openvmm_version.clear();
    assert_eq!(
        malformed(&spec.encode_with(Proto::default().bytes(3, &[0xff, 0xfe]))),
        "the snapshot manifest is malformed: it holds field 3, which is not UTF-8"
    );
    spec = template.clone();
    spec.vp_count = 0;
    assert_eq!(
        malformed(&spec.encode_with(Proto::default().varint(5, 1 << 32))),
        "the snapshot manifest is malformed: it holds field 5, which does not fit 32 bits"
    );
    assert_eq!(
        decode_block(&Proto::default().varint(2, 2).finish()).map(drop),
        Err("its sandbox block holds field 2, which is 2, not a boolean".to_owned())
    );
    // An address space that does not exist.
    let range = Proto::default().text(1, "io").varint(3, 1).finish();
    assert_eq!(
        decode_device_range(&range, 4),
        Err("a range of its device 4 is in the unknown address space \"io\"".to_owned())
    );
    // An expansion range has no file offset, field 3, not even an explicit zero one.
    for offset in [4096, 0] {
        let expansion = Proto::default()
            .varint(1, 4096)
            .varint(2, 4096)
            .some(3, offset)
            .finish();
        assert_eq!(
            decode_contract(&Proto::default().always_bytes(30, &expansion).finish()).map(drop),
            Err("its memory expansion range holds the unknown field 3".to_owned())
        );
    }
}

#[test]
fn truncated_manifests_are_refused() {
    let bytes = ManifestSpec::template().encode();
    for len in 0..bytes.len() {
        let error =
            TemplateManifest::parse(&bytes[..len]).expect_err("a truncated manifest is refused");
        // A manifest that ends between its fields lacks some, such as its format magic.
        assert!(
            matches!(error.code(), ErrorCode::Io | ErrorCode::FormatUnsupported),
            "{len}: {error}"
        );
    }
    // A value that runs past its message is truncated.
    assert_eq!(
        refusal(&bytes[..bytes.len() - 1], ErrorCode::Io),
        "the snapshot manifest is malformed: it is truncated"
    );
}

#[test]
fn bounded_lists_are_refused_past_their_bounds() {
    let mut contract = Proto::default();
    for _ in 0..=MAX_DEVICES {
        contract = contract.message(
            8,
            Proto::default()
                .text(1, "device")
                .text(2, "unit")
                .text(3, "kind"),
        );
    }
    assert_eq!(
        decode_contract(&contract.finish()).map(drop),
        Err(format!("its devices hold more than {MAX_DEVICES} items"))
    );
    let mut contract = Proto::default();
    for _ in 0..=MAX_STATE_UNITS {
        contract = contract.text(9, "unit");
    }
    assert_eq!(
        decode_contract(&contract.finish()).map(drop),
        Err(format!(
            "its state units hold more than {MAX_STATE_UNITS} items"
        ))
    );
    let mut contract = Proto::default();
    for _ in 0..=MAX_BLOCKS {
        contract = contract.message(21, Proto::default().text(1, "distro"));
    }
    assert_eq!(
        decode_contract(&contract.finish()).map(drop),
        Err(format!(
            "its sandbox blocks hold more than {MAX_BLOCKS} items"
        ))
    );
    let identity = vec![1; MAX_ATTACHMENT_IDENTITY_LEN + 1];
    assert_eq!(
        decode_attachment(&Proto::default().bytes(6, &identity).finish(), 0),
        Err(format!(
            "its attachment 0 has an identity of more than {MAX_ATTACHMENT_IDENTITY_LEN} bytes"
        ))
    );
}

#[test]
fn oversized_manifests_are_refused() {
    let bytes = vec![0; MAX_MANIFEST_LEN as usize + 1];
    assert_eq!(
        refusal(&bytes, ErrorCode::Io),
        format!(
            "the snapshot manifest holds {} bytes, more than the {MAX_MANIFEST_LEN} that a \
             manifest may hold",
            MAX_MANIFEST_LEN + 1
        )
    );
}

#[test]
fn manifests_that_are_not_a_templates_are_refused() {
    let io = ErrorCode::Io;
    for (tier, policy, sections) in [
        ("workload-start", "clone", 1),
        ("instance-checkpoint", "resume", 7),
        ("platform", "resume", 1),
        ("platform", "clone", 7),
        ("platform", "clone", 0),
        ("", "", 0),
    ] {
        assert_refused(
            |spec| {
                spec.tier = tier.to_owned();
                spec.restore_policy = policy.to_owned();
                spec.consumed_sections = sections;
            },
            io,
            &format!(
                "is not a template's: it was captured at tier {tier:?} with restore policy \
                 {policy:?} after consuming configuration sections {sections:#x}"
            ),
        );
    }
    let blocks = "is not a template's: its sandbox blocks are not a read-only distro whose \
                  identity is unbound and a fresh scratch";
    let bound: [fn(&mut ManifestSpec); 8] = [
        // A distro bound to an image.
        |spec| {
            let distro = &mut spec.contract().blocks[0];
            distro.identity_kind = "sha256".to_owned();
            distro.identity = vec![1; 32];
        },
        |spec| spec.contract().blocks[0].read_only = false,
        // A scratch paired with the capture.
        |spec| {
            let scratch = &mut spec.contract().blocks[1];
            scratch.identity_kind = "sha256".to_owned();
            scratch.identity = vec![1; 32];
            scratch.artifact = "scratch.img".to_owned();
        },
        |spec| spec.contract().blocks[1].restore_mode = "private-copy".to_owned(),
        |spec| {
            spec.contract().blocks.pop();
        },
        |spec| spec.contract().blocks.reverse(),
        |spec| {
            let mut runtime = spec.contract().blocks[0].clone();
            runtime.role = "runtime".to_owned();
            spec.contract().blocks.insert(1, runtime);
        },
        |spec| spec.contract().blocks.clear(),
    ];
    for change in bound {
        assert_refused(change, io, blocks);
    }
    let slot = "is not a template's: its filesystem slot is not the dormant slot of version 1";
    assert_refused(
        |spec| spec.contract().filesystem = Some(Proto::default().text(1, "/mnt/share").finish()),
        io,
        slot,
    );
    assert_refused(
        |spec| spec.contract().filesystem = Some(Vec::new()),
        io,
        slot,
    );
    assert_refused(|spec| spec.contract().filesystem_slot_version = 0, io, slot);
    assert_refused(
        |spec| spec.contract().expansion_version = 0,
        io,
        "is not a template's: it declares memory expansion version 0",
    );
    assert_refused(|spec| spec.contract = None, io, "has no machine contract");
    assert_refused(
        |spec| spec.contract().time = None,
        io,
        "has no time contract",
    );
    assert_refused(|spec| spec.contract().cpu = None, io, "has no CPU profile");
}

/// A change to the manifest of a template.
type Change = fn(&mut ManifestSpec);

#[test]
fn manifests_of_invalid_machines_are_refused() {
    let cases: &[(Change, &str)] = &[
        (
            |spec| spec.page_size = 8192,
            "describes a VM of architecture \"x86_64\" with 8192-byte pages",
        ),
        (
            |spec| spec.architecture = "aarch64".to_owned(),
            "describes a VM of architecture \"aarch64\"",
        ),
        (
            |spec| spec.state_size = 0,
            "has a saved state of 0 bytes, not 1 to 268435456",
        ),
        (
            |spec| spec.state_size = 256 * MIB + 1,
            "has a saved state of 268435457 bytes",
        ),
        (
            |spec| spec.created_at.1 = 1_000_000_000,
            "has the invalid creation time of 1761300000 seconds and 1000000000 nanoseconds",
        ),
        (
            |spec| spec.created_at = (-1, 0),
            "has the invalid creation time of -1 seconds",
        ),
        (
            |spec| spec.contract().source_hypervisor = "hyperv".to_owned(),
            "was captured by the unknown backend \"hyperv\"",
        ),
        (
            |spec| spec.cpu().id = "intel.icelake-sp".to_owned(),
            "has the CPU profile \"intel.icelake-sp\", which is not VENDOR.GENERATION.vREVISION",
        ),
        (
            |spec| spec.cpu().id = "Intel.icelake-sp.v1".to_owned(),
            "which is not VENDOR.GENERATION.vREVISION",
        ),
        (
            |spec| spec.cpu().id = "intel.icelake-sp.v0".to_owned(),
            "which is not VENDOR.GENERATION.vREVISION",
        ),
        (
            |spec| spec.cpu().id = "intel.icelake-sp.v01".to_owned(),
            "which is not VENDOR.GENERATION.vREVISION",
        ),
        (
            |spec| spec.cpu().digest.truncate(31),
            "has a CPU profile whose digest is not 32 bytes",
        ),
        (
            |spec| spec.cpu().effective_cpuid.push(0),
            "has an effective CPUID of 89 bytes, not 1 to 1024 entries of 44 bytes",
        ),
        (
            |spec| spec.cpu().effective_cpuid.clear(),
            "has an effective CPUID of 0 bytes",
        ),
        (
            |spec| spec.cpu().effective_cpuid = vec![0; 1025 * 44],
            "has an effective CPUID of 45100 bytes",
        ),
        (
            |spec| spec.contract().command_line_digest = Some(vec![0; 32]),
            "has an effective command line whose digest does not match it",
        ),
        (
            |spec| spec.contract().command_line_digest = Some(vec![0; 31]),
            "has an effective command line whose digest is not 32 bytes",
        ),
        (
            |spec| spec.contract().command_line = "x".repeat(2048),
            "has an effective command line of 2048 bytes, more than the 2047",
        ),
        (
            |spec| spec.contract().memory_ranges[0].1 -= 4096,
            "has 536870912 bytes of RAM in 1 ranges that do not hold it",
        ),
        (
            |spec| spec.contract().memory_ranges.clear(),
            "bytes of RAM in 0 ranges",
        ),
        (
            |spec| {
                spec.contract().memory_ranges = vec![(0, 256 * MIB, 0), (4096 * MIB, 256 * MIB, 0)];
            },
            "has memory ranges that do not fill its memory file in order",
        ),
        (
            |spec| spec.contract().memory_capacity = 256 * MIB,
            "has a RAM capacity of 268435456 bytes in blocks of 134217728 bytes",
        ),
        (
            |spec| spec.contract().memory_block_size = 0,
            "in blocks of 0 bytes",
        ),
        (
            |spec| spec.contract().memory_block_size = 384 * MIB,
            "in blocks of 402653184 bytes",
        ),
        (
            |spec| spec.contract().expansion_ranges[0].1 -= 128 * MIB,
            "has memory expansion ranges that do not span its RAM capacity",
        ),
        (
            |spec| spec.contract().topology.4.push(2),
            "has 2 processors, 3 APIC IDs, and 0 processors online at boot",
        ),
        (
            |spec| {
                spec.vp_count = 3;
                spec.contract().topology.4.push(2);
            },
            "has 3 processors, 3 APIC IDs",
        ),
        (
            |spec| spec.contract().boot_online_vp_count = 4,
            "and 4 processors online at boot, which a microVM cannot have",
        ),
        (
            |spec| spec.time().tsc_hz = 0,
            "declares no TSC or LAPIC timer rate",
        ),
        (
            |spec| spec.time().apic_hz = 0,
            "declares no TSC or LAPIC timer rate",
        ),
        (
            |spec| spec.contract().devices[2].order = 7,
            "lists device 2 with the order 7",
        ),
        (
            |spec| spec.contract().devices[2].stable_id = "vp0".to_owned(),
            "has the device ID \"vp0\" more than once",
        ),
        (
            |spec| spec.contract().devices[2].kind.clear(),
            "has a device without an ID, a kind, or a state unit",
        ),
        (
            |spec| spec.contract().devices[2].transport = "t".repeat(257),
            "or with one of more than 256 bytes: \"vmtime\"",
        ),
        (
            |spec| spec.contract().devices[4].feature_banks = vec![0; 65],
            "has the device \"ioapic\" with more than 16 ranges, or more than 64 feature banks",
        ),
        (
            |spec| spec.contract().devices.clear(),
            "has 0 devices, not 1 to 64",
        ),
        (
            |spec| spec.contract().interrupt_mode.clear(),
            "names no interrupt delivery mode",
        ),
        (
            |spec| spec.contract().blocks[0].logical_block_size = 256,
            "has a distro block whose logical block size 256 is not a power of two of at least \
             512",
        ),
        (
            |spec| spec.contract().blocks[1].physical_block_size = 256,
            "has a scratch block whose physical block size 256 is not a power of two",
        ),
        (
            |spec| spec.contract().blocks[1].length = 1000,
            "has a scratch block whose length 1000 is not a nonzero multiple of its logical \
             block size",
        ),
        (
            |spec| spec.time().host_clock = "utc".to_owned(),
            "has a time contract of the unknown host clock \"utc\"",
        ),
        (
            |spec| spec.time().host_id.truncate(15),
            "has a time contract whose host identity is 15 bytes, not 16",
        ),
        (
            |spec| spec.time().boot_id.push(0),
            "has a time contract whose boot identity is 17 bytes, not 16",
        ),
    ];
    for (change, expected) in cases {
        assert_refused(change, ErrorCode::Io, expected);
    }
}

#[test]
fn manifests_of_invalid_nics_are_refused() {
    let cases: &[(Change, &str)] = &[
        (
            |spec| spec.network().guest_mac.truncate(5),
            "has a NIC whose MAC addresses are not 6 bytes",
        ),
        (
            |spec| spec.network().gateway_mac.clear(),
            "has a NIC whose MAC addresses are not 6 bytes",
        ),
        (
            |spec| spec.network().prefix_length = 31,
            "has a NIC with an invalid prefix length",
        ),
        (
            |spec| spec.network().prefix_length = 0,
            "has a NIC with an invalid prefix length",
        ),
        (
            |spec| spec.network().ipv6_prefix_length = 129,
            "has a NIC with an invalid prefix length",
        ),
        (
            |spec| spec.network().ipv6_gateway.clear(),
            "has a NIC with an invalid IPv6 identity",
        ),
        (
            |spec| spec.network().policy_digest.truncate(31),
            "has a NIC whose policy digest is not 32 bytes",
        ),
        (
            |spec| spec.network().profile.clear(),
            "has a NIC without a network profile or a policy mode",
        ),
    ];
    for (change, expected) in cases {
        let mut spec = ManifestSpec::with_network();
        change(&mut spec);
        let message = refusal(&spec.encode(), ErrorCode::Io);
        assert!(message.contains(expected), "{message:?} lacks {expected:?}");
    }
    // A NIC without an IPv6 identity has none.
    let mut spec = ManifestSpec::with_network();
    let network = spec.network();
    network.ipv6.clear();
    network.ipv6_prefix_length = 0;
    network.ipv6_gateway.clear();
    assert_eq!(parse(&spec).machine.nic.unwrap().ipv6, None);
}

#[test]
fn profile_ids_have_one_form() {
    assert_eq!(
        parse_profile_id("amd.genoa.v12"),
        Some(("amd".to_owned(), "genoa".to_owned(), 12))
    );
    assert_eq!(
        parse_profile_id("intel.host.v1"),
        Some(("intel".to_owned(), "host".to_owned(), 1))
    );
    let long = format!("{}.genoa.v1", "a".repeat(33));
    for id in [
        "",
        "amd",
        "amd.genoa",
        "amd.genoa.1",
        "amd.genoa.v",
        "amd.genoa.v0",
        "amd.genoa.v01",
        "amd.genoa.v1.extra",
        "amd..v1",
        ".genoa.v1",
        "AMD.genoa.v1",
        "amd.gen_oa.v1",
        "amd.genoa.v99999999999",
        &long,
    ] {
        assert_eq!(parse_profile_id(id), None, "{id:?}");
    }
}

/// Writes `bytes` as the manifest of a new snapshot directory, and returns the directory.
fn snapshot(bytes: &[u8]) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join(MANIFEST_NAME), bytes).unwrap();
    directory
}

/// Returns the directory of the template fixture `name`, whose manifest OpenVMM wrote.
fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/template")
        .join(name)
}

#[test]
fn the_manifests_that_openvmm_writes_are_read() {
    // The fixtures describe the captures of the specs of these tests, which OpenVMM's own code
    // encoded, but for the CPU profile's records, OpenVMM's test records of a profile, and the
    // digest of the network policy, which OpenVMM computed.
    for (name, mut spec) in [
        ("platform-kvm", ManifestSpec::template()),
        ("platform-mshv-network", ManifestSpec::with_network()),
    ] {
        let manifest = TemplateManifest::read(&fixture(name)).unwrap();
        let machine = &manifest.machine;
        assert_eq!(machine.cpu.id(), "intel.icelake-sp.v1", "{name}");
        assert_eq!(machine.cpu.effective_cpuid.len() % CPUID_ENTRY_LEN, 0);
        let cpu = spec.cpu();
        cpu.digest = machine.cpu.digest.to_vec();
        cpu.effective_cpuid = machine.cpu.effective_cpuid.clone();
        if let Some(nic) = &machine.nic {
            spec.network().policy_digest = nic.policy.digest.to_vec();
            spec.contract().source_hypervisor = "mshv".to_owned();
        }
        assert_eq!(manifest, parse(&spec), "{name}");
    }
}

/// Returns the message of the refusal of the manifest of the snapshot directory `directory`,
/// which must have the code `code`, and the error that caused it.
fn read_refusal(directory: &Path, code: ErrorCode) -> (String, String) {
    let error = TemplateManifest::read(directory).expect_err("the manifest is refused");
    assert_eq!(error.code(), code, "{error}");
    let cause = std::error::Error::source(&error)
        .map(ToString::to_string)
        .unwrap_or_default();
    (error.message().to_owned(), cause)
}

#[test]
fn read_reads_the_manifest_of_a_snapshot() {
    let spec = ManifestSpec::with_network();
    let directory = snapshot(&spec.encode());
    assert_eq!(
        TemplateManifest::read(directory.path()).unwrap(),
        parse(&spec)
    );
    // The messages of a manifest that is read name its path.
    let mut invalid = spec;
    invalid.tier = "workload-start".to_owned();
    let directory = snapshot(&invalid.encode());
    let (message, _) = read_refusal(directory.path(), ErrorCode::Io);
    let path = directory.path().join(MANIFEST_NAME);
    assert!(
        message.starts_with(&format!(
            "the snapshot manifest {} is not a template's",
            path.display()
        )),
        "{message}"
    );
}

#[test]
fn read_refuses_a_missing_manifest_as_not_found() {
    let directory = tempfile::tempdir().unwrap();
    let (message, _) = read_refusal(directory.path(), ErrorCode::NotFound);
    let path = directory.path().join(MANIFEST_NAME);
    assert_eq!(
        message,
        format!("cannot read the snapshot manifest {}", path.display())
    );
}

#[test]
fn read_refuses_more_than_a_manifest_may_hold() {
    let directory = snapshot(&vec![0; MAX_MANIFEST_LEN as usize + 1]);
    let (_, cause) = read_refusal(directory.path(), ErrorCode::Io);
    assert_eq!(
        cause,
        format!("it holds more than the {MAX_MANIFEST_LEN} bytes that a manifest may hold")
    );
}

#[test]
fn read_refuses_a_directory() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join(MANIFEST_NAME)).unwrap();
    read_refusal(directory.path(), ErrorCode::Io);
}

#[test]
fn read_refuses_a_link() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target.bin");
    fs::write(&target, ManifestSpec::template().encode()).unwrap();
    let path = directory.path().join(MANIFEST_NAME);
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&target, &path);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&target, &path);
    if linked.is_err() {
        // Windows lets only Developer Mode or a privilege create a symbolic link.
        eprintln!("skipped: this host does not let the test create a symbolic link");
        return;
    }
    let (_, cause) = read_refusal(directory.path(), ErrorCode::Io);
    assert_eq!(cause, "it is a link, or not a regular file");
}

#[cfg(windows)]
#[test]
fn read_refuses_a_junction() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(
        target.join(MANIFEST_NAME),
        ManifestSpec::template().encode(),
    )
    .unwrap();
    let path = directory.path().join(MANIFEST_NAME);
    crate::host::test_support::junction(&path, &target);
    read_refusal(directory.path(), ErrorCode::Io);
}

#[cfg(unix)]
#[test]
fn read_refuses_a_fifo_without_waiting_for_a_writer() {
    use std::sync::mpsc;
    use std::thread;

    use nix::sys::stat::Mode;

    let directory = tempfile::tempdir().unwrap();
    nix::unistd::mkfifo(
        &directory.path().join(MANIFEST_NAME),
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .unwrap();
    // A read that waited for a writer would never end, so it runs on a thread that the test can
    // leave behind.
    let (sender, receiver) = mpsc::channel();
    let path = directory.path().to_owned();
    thread::spawn(move || {
        sender
            .send(TemplateManifest::read(&path).map(drop))
            .unwrap()
    });
    let error = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the read does not wait for a writer")
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Io, "{error}");
    let cause = std::error::Error::source(&error).map(ToString::to_string);
    assert_eq!(
        cause.as_deref(),
        Some("it is a link, or not a regular file")
    );
}
