//! Tests of [`TemplateKey`] and [`CaptureClass`]: what a key holds, what it leaves out, and its
//! canonical encoding.

use std::collections::HashSet;

use super::*;
use crate::ErrorCode;
use crate::host::network::{Access, EgressRule, HostLoopback, NetworkPolicy, NetworkProxy};
use crate::host::template::test_support::{
    self as support, MIB, ManifestSpec, artifact, network_class,
};

/// Returns the manifest that `spec` encodes.
fn manifest(spec: &ManifestSpec) -> TemplateManifest {
    TemplateManifest::parse(&spec.encode()).unwrap()
}

/// Returns the key of the template that a capture of `class` made, whose manifest `spec`
/// encodes.
fn key(class: &CaptureClass, spec: &ManifestSpec) -> TemplateKey {
    TemplateKey::new(class, &manifest(spec)).unwrap()
}

/// Returns the key of the template with a NIC of the tests.
fn network_key() -> TemplateKey {
    key(&network_class(), &ManifestSpec::with_network())
}

/// A change to the class and the manifest of the template with a NIC of the tests.
type Change = fn(&mut CaptureClass, &mut ManifestSpec);

/// Returns the identity of the key of the template with a NIC of the tests once `change` changes
/// it.
fn identity_after(change: Change) -> KeyIdentity {
    let (mut class, mut spec) = (network_class(), ManifestSpec::with_network());
    change(&mut class, &mut spec);
    key(&class, &spec).identity()
}

/// Changes the RAM of a capture to `memory` bytes, below 3 GiB, of the same capacity.
fn set_memory(class: &mut CaptureClass, spec: &mut ManifestSpec, memory: u64) {
    class.memory = memory;
    spec.memory_size = memory;
    let contract = spec.contract();
    contract.memory_ranges = vec![(0, memory, 0)];
    contract.expansion_ranges = vec![(memory, contract.memory_capacity - memory)];
}

#[test]
fn each_part_of_the_key_changes_it() {
    let changes: &[(&str, Change)] = &[
        // Bundle and contracts.
        ("OpenVMM", |class, _| class.runtime.openvmm = artifact('d')),
        ("kernel", |class, _| class.runtime.kernel = artifact('d')),
        ("initramfs", |class, _| {
            class.runtime.initramfs = artifact('d')
        }),
        ("boot contract", |class, _| class.runtime.boot_contract += 1),
        ("control contract", |class, _| {
            class.runtime.control_contract = "other-revision".to_owned();
        }),
        ("control session", |class, _| {
            class.runtime.control_session += 1
        }),
        ("guest build", |class, _| {
            class.guest_build = ArtifactIdentity::new("guest-build-2").unwrap();
        }),
        // CPU and time.
        ("backend", |class, spec| {
            class.hypervisor = Hypervisor::Mshv;
            spec.contract().source_hypervisor = "mshv".to_owned();
        }),
        ("CPU vendor", |_, spec| {
            spec.cpu().id = "amd.icelake-sp.v1".to_owned();
        }),
        ("CPU generation", |_, spec| {
            spec.cpu().id = "intel.sapphirerapids.v1".to_owned();
        }),
        ("CPU profile revision", |_, spec| {
            spec.cpu().id = "intel.icelake-sp.v2".to_owned();
        }),
        ("CPU profile digest", |_, spec| spec.cpu().digest[31] ^= 1),
        ("effective CPUID", |_, spec| {
            spec.cpu().effective_cpuid[87] ^= 1
        }),
        ("sockets", |_, spec| spec.contract().topology.0 = 2),
        ("dies", |_, spec| spec.contract().topology.1 = 2),
        ("cores", |_, spec| spec.contract().topology.2 = 1),
        ("threads", |_, spec| spec.contract().topology.3 = 2),
        ("APIC IDs", |_, spec| {
            spec.contract().topology.4 = vec![0, 2]
        }),
        ("TSC rate", |_, spec| spec.time().tsc_hz += 1),
        ("LAPIC timer rate", |_, spec| spec.time().apic_hz += 1),
        ("rate policy", |_, spec| spec.time().tolerance_ppm += 1),
        // Resources.
        ("processors", |class, spec| {
            class.processors = 4;
            class.boot_processors = 4;
            spec.vp_count = 4;
            spec.contract().topology = (1, 1, 4, 1, vec![0, 1, 2, 3]);
        }),
        ("processors online at boot", |_, spec| {
            spec.contract().boot_online_vp_count = 2;
        }),
        ("RAM", |class, spec| set_memory(class, spec, 640 * MIB)),
        ("RAM capacity", |class, spec| {
            class.memory_capacity = 1024 * MIB;
            let contract = spec.contract();
            contract.memory_capacity = 1024 * MIB;
            contract.expansion_ranges = vec![(512 * MIB, 512 * MIB)];
        }),
        ("RAM block size", |_, spec| {
            spec.contract().memory_block_size = 64 * MIB;
        }),
        ("RAM ranges", |_, spec| {
            spec.contract().memory_ranges =
                vec![(0, 256 * MIB, 0), (4096 * MIB, 256 * MIB, 256 * MIB)];
        }),
        ("RAM expansion ranges", |_, spec| {
            spec.contract().expansion_ranges =
                vec![(512 * MIB, 512 * MIB), (4096 * MIB, 1024 * MIB)];
        }),
        // Boot.
        ("command line", |_, spec| {
            spec.contract().command_line.push_str(" quiet");
        }),
        // Disks.
        ("distro capacity", |class, spec| {
            class.distro.length *= 2;
            spec.contract().blocks[0].length *= 2;
        }),
        ("distro block sizes", |class, spec| {
            class.distro.physical_block_size = 512;
            spec.contract().blocks[0].physical_block_size = 512;
        }),
        ("scratch length", |class, spec| {
            class.scratch.length *= 2;
            spec.contract().blocks[1].length *= 2;
        }),
        ("scratch geometry", |class, spec| {
            class.scratch.logical_block_size = 4096;
            class.scratch.physical_block_size = 4096;
            let scratch = &mut spec.contract().blocks[1];
            scratch.logical_block_size = 4096;
            scratch.physical_block_size = 4096;
        }),
        // Devices and isolation.
        ("device ID", |_, spec| {
            spec.contract().devices[2].stable_id = "clock".to_owned();
        }),
        ("device kind", |_, spec| {
            spec.contract().devices[2].kind = "timer".to_owned();
        }),
        ("state unit", |_, spec| {
            spec.contract().devices[2].state_unit = "time".to_owned();
        }),
        ("device transport", |_, spec| {
            spec.contract().devices[14].transport = "virtio-pci".to_owned();
        }),
        ("MMIO", |_, spec| {
            spec.contract().devices[4].ranges[0].1 += 0x1000
        }),
        ("address space", |_, spec| {
            spec.contract().devices[8].ranges[0].0 = "mmio".to_owned();
        }),
        ("device ranges", |_, spec| {
            spec.contract().devices[3].ranges.pop();
        }),
        ("IRQ", |_, spec| spec.contract().devices[14].irq = Some(5)),
        ("an IRQ of 0", |_, spec| {
            spec.contract().devices[2].irq = Some(0)
        }),
        ("features", |_, spec| {
            spec.contract().devices[14].feature_banks[0] ^= 1;
        }),
        ("queue count", |_, spec| {
            let device = &mut spec.contract().devices[14];
            device.queue_count = 2;
            device.queue_max_sizes = vec![256, 256];
        }),
        ("queue sizes", |_, spec| {
            spec.contract().devices[14].queue_max_sizes[0] = 128;
        }),
        ("device order", |_, spec| {
            let contract = spec.contract();
            contract.devices.swap(3, 4);
            contract.renumber();
        }),
        ("interrupt delivery", |_, spec| {
            spec.contract().interrupt_mode = "edge".to_owned();
        }),
        ("interrupt-status page", |_, spec| {
            spec.contract().status_page.0 += 0x1000;
        }),
        ("NIC presence", |class, spec| {
            *class = support::class();
            *spec = ManifestSpec::template();
        }),
        ("network profile", |_, spec| {
            spec.network().profile = "strict".to_owned();
        }),
        ("guest MAC", |_, spec| spec.network().guest_mac[5] = 3),
        ("gateway MAC", |_, spec| spec.network().gateway_mac[5] = 3),
        ("IPv4 address", |class, spec| {
            class.network = Some(
                SandboxNetwork::new("10.0.0.3/24", NetworkPolicy::new(Access::Allow)).unwrap(),
            );
            spec.network().ipv4 = [10, 0, 0, 3];
        }),
        ("IPv4 prefix", |class, spec| {
            class.network = Some(
                SandboxNetwork::new("10.0.0.2/16", NetworkPolicy::new(Access::Allow)).unwrap(),
            );
            spec.network().prefix_length = 16;
        }),
        ("IPv4 gateway", |_, spec| {
            spec.network().gateway = [10, 0, 0, 9]
        }),
        ("IPv6 address", |_, spec| spec.network().ipv6[15] = 3),
        ("IPv6 prefix", |_, spec| {
            spec.network().ipv6_prefix_length = 64
        }),
        ("IPv6 gateway", |_, spec| {
            spec.network().ipv6_gateway[15] = 9
        }),
        ("IPv6 presence", |_, spec| {
            let network = spec.network();
            network.ipv6.clear();
            network.ipv6_prefix_length = 0;
            network.ipv6_gateway.clear();
        }),
        ("policy mode", |_, spec| {
            spec.network().policy_mode = "deny-all".to_owned();
        }),
        ("policy digest", |_, spec| {
            spec.network().policy_digest[0] ^= 1
        }),
        ("policy enforcement", |_, spec| {
            spec.network().policy_required = false;
        }),
        ("policy encoding", |_, spec| {
            spec.network().policy_encoding_version = 3;
        }),
        ("invariant setting", |class, _| {
            class.invariants = Invariants::new()
                .with("isolation.seccomp", "strict")
                .unwrap();
        }),
    ];
    let base = network_key().identity();
    let mut identities = HashSet::from([base]);
    for (part, change) in changes {
        assert!(
            identities.insert(identity_after(*change)),
            "a change of the {part} leaves the key as it is, or as another change makes it"
        );
    }
    // Two values of one setting make two keys.
    let mut class = network_class();
    class.invariants = Invariants::new()
        .with("isolation.seccomp", "relaxed")
        .unwrap();
    assert!(identities.insert(key(&class, &ManifestSpec::with_network()).identity()));
}

#[test]
fn each_contract_version_changes_the_key() {
    let base = network_key();
    assert!(base.speaks_current_contracts());
    let changes: [fn(&mut Contracts); 5] = [
        |contracts| contracts.snapshot += 1,
        |contracts| contracts.device_state += 1,
        |contracts| contracts.time += 1,
        |contracts| contracts.restore_packet += 1,
        |contracts| contracts.activation += 1,
    ];
    let mut identities = HashSet::from([base.identity()]);
    for change in changes {
        let mut other = base.clone();
        change(&mut other.contracts);
        assert!(!other.speaks_current_contracts());
        assert!(identities.insert(other.identity()));
        // A key of other versions reads back as it is, so that a store can name what it holds.
        assert_eq!(TemplateKey::from_bytes(&other.to_bytes()).unwrap(), other);
    }
}

#[test]
fn a_generations_own_metadata_leaves_the_key_as_it_is() {
    let changes: &[(&str, Change)] = &[
        ("capture time", |_, spec| spec.created_at.0 += 3600),
        ("OpenVMM version", |_, spec| {
            spec.openvmm_version = "1.2.3".to_owned();
        }),
        ("saved state length", |_, spec| spec.state_size *= 2),
        ("TSC anchor", |_, spec| spec.time().capture_tsc += 1),
        ("UTC anchor", |_, spec| spec.time().capture_utc_ns += 1),
        ("monotonic anchor", |_, spec| {
            spec.time().capture_monotonic_ns += 1
        }),
        ("host clock", |_, spec| {
            spec.time().host_clock = "windows-interrupt-time".to_owned();
        }),
        ("host ID", |_, spec| spec.time().host_id = vec![9; 16]),
        ("boot ID", |_, spec| spec.time().boot_id = vec![9; 16]),
        ("lineage counter", |_, spec| spec.time().lineage += 1),
        ("cut to the anchor", |_, spec| {
            spec.time().cut_to_anchor_ns = None
        }),
        ("a cut of 0", |_, spec| {
            spec.time().cut_to_anchor_ns = Some(0)
        }),
        ("attachments", |_, spec| spec.contract().attachments.clear()),
        ("state units", |_, spec| {
            spec.contract().state_units.push("extra".to_owned());
        }),
        ("CPU profile document", |_, spec| {
            spec.cpu().document = b"another document".to_vec();
        }),
        ("capture CPU signature", |_, spec| spec.cpu().signature += 1),
        // The class requests a profile, which the key names as OpenVMM selected it.
        ("requested CPU profile", |class, _| {
            class.cpu_profile = "intel.icelake-sp.v1".to_owned();
        }),
    ];
    let base = network_key();
    for (part, change) in changes {
        assert_eq!(
            identity_after(*change),
            base.identity(),
            "a change of the {part} changes the key"
        );
    }
}

#[test]
fn a_key_holds_what_its_capture_made() {
    let made = network_key();
    let class = network_class();
    assert_eq!(made.runtime(), &class.runtime);
    assert_eq!(made.guest_build(), &class.guest_build);
    assert_eq!(made.hypervisor(), Hypervisor::Kvm);
    assert_eq!(made.cpu_profile(), "intel.icelake-sp.v1");
    assert_eq!((made.processors(), made.boot_processors()), (2, 0));
    assert_eq!(
        (
            made.memory(),
            made.memory_capacity(),
            made.memory_block_size()
        ),
        (512 * MIB, 2048 * MIB, 128 * MIB)
    );
    assert_eq!(
        made.command_line(),
        ManifestSpec::with_network().contract().command_line
    );
    assert_eq!(
        (made.distro(), made.scratch()),
        (class.distro, class.scratch)
    );
    assert!(made.has_network());
    assert_eq!(made.guest_network().as_deref(), Some("10.0.0.2/24"));
    assert_eq!(
        made.ipv6_gateway(),
        Some(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0xa00, 1))
    );
    let template = key(&support::class(), &ManifestSpec::template());
    assert!(!template.has_network());
    assert_eq!(
        (template.guest_network(), template.ipv6_gateway()),
        (None, None)
    );
    let mut spec = ManifestSpec::with_network();
    let network = spec.network();
    network.ipv6.clear();
    network.ipv6_prefix_length = 0;
    network.ipv6_gateway.clear();
    assert_eq!(key(&network_class(), &spec).ipv6_gateway(), None);
    assert_eq!(made.filesystem_tag(), "microvm");
    assert_eq!(made.invariants(), &Invariants::new());
}

/// Asserts that the capture of `class` whose manifest is `spec` does not match its class, with
/// a message that holds `expected`.
fn assert_mismatched(class: &CaptureClass, spec: &ManifestSpec, expected: &str) {
    let error = TemplateKey::new(class, &manifest(spec)).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Io, "{error}");
    assert!(
        error
            .message()
            .starts_with("the capture does not match its class: ")
            && error.message().contains(expected),
        "{:?} lacks {expected:?}",
        error.message()
    );
}

#[test]
fn a_capture_that_does_not_match_its_class_is_refused() {
    let (template, with_network) = (ManifestSpec::template(), ManifestSpec::with_network());
    let base = support::class();
    let mut class = base.clone();
    class.hypervisor = Hypervisor::Mshv;
    assert_mismatched(
        &class,
        &template,
        "its backend is kvm, but the class requests mshv",
    );
    for requested in ["amd.genoa.v1", "intel.icelake-sp.v2", "host"] {
        class = base.clone();
        class.cpu_profile = requested.to_owned();
        assert_mismatched(
            &class,
            &template,
            &format!("its CPU profile is intel.icelake-sp.v1, but the class requests {requested}"),
        );
    }
    class = base.clone();
    class.processors = 4;
    class.boot_processors = 4;
    assert_mismatched(
        &class,
        &template,
        "it has 2 processors, 2 online at boot, but the class requests 4 processors, 4 online at \
         boot",
    );
    // A capture whose processors are all online at boot does not serve a class that keeps some
    // offline.
    class = base.clone();
    class.boot_processors = 1;
    assert_mismatched(
        &class,
        &template,
        "it has 2 processors, 2 online at boot, but the class requests 2 processors, 1 online",
    );
    let mut spec = template.clone();
    spec.contract().boot_online_vp_count = 1;
    assert_mismatched(
        &base,
        &spec,
        "it has 2 processors, 1 online at boot, but the class requests 2 processors, 2 online",
    );
    assert!(TemplateKey::new(&class, &manifest(&spec)).is_ok());
    class = base.clone();
    class.memory = 1024 * MIB;
    assert_mismatched(
        &class,
        &template,
        "it has 536870912 bytes of RAM, of a capacity of 2147483648, but the class requests \
         1073741824 bytes of RAM",
    );
    class = base.clone();
    class.memory_capacity = 4096 * MIB;
    assert_mismatched(&class, &template, "of a capacity of 4294967296");
    class = base.clone();
    class.distro.length *= 2;
    assert_mismatched(&class, &template, "its distro geometry is");
    class = base.clone();
    class.scratch.physical_block_size = 4096;
    assert_mismatched(&class, &template, "its scratch geometry is");
    assert_mismatched(
        &base,
        &with_network,
        "its NIC network is 10.0.0.2/24, but the class requests none",
    );
    assert_mismatched(
        &network_class(),
        &template,
        "its NIC network is none, but the class requests 10.0.0.2/24",
    );
    // A network whose policy lets nothing through attaches no NIC.
    class = base.clone();
    class.network =
        Some(SandboxNetwork::new("10.0.0.2/24", NetworkPolicy::new(Access::Deny)).unwrap());
    assert_mismatched(&class, &with_network, "but the class requests none");
    assert_eq!(key(&class, &template), key(&base, &template));
    class.network =
        Some(SandboxNetwork::new("10.0.0.3/24", NetworkPolicy::new(Access::Allow)).unwrap());
    assert_mismatched(
        &class,
        &with_network,
        "its NIC network is 10.0.0.2/24, but the class requests 10.0.0.3/24",
    );
}

#[test]
fn a_class_selects_a_profile_as_openvmm_does() {
    let mut spec = ManifestSpec::template();
    spec.cpu().id = "intel.host.v1".to_owned();
    let mut class = support::class();
    // `auto` falls back to a profile that the host derives when no built-in profile serves the
    // host's CPU.
    for requested in ["auto", "host", "intel.host.v1"] {
        class.cpu_profile = requested.to_owned();
        assert_eq!(key(&class, &spec).cpu_profile(), "intel.host.v1");
    }
    class.cpu_profile = "intel.icelake-sp.v1".to_owned();
    assert_mismatched(&class, &spec, "its CPU profile is intel.host.v1");
}

#[test]
fn invalid_classes_are_refused() {
    type ClassChange = fn(&mut CaptureClass);
    let cases: &[(ClassChange, &str)] = &[
        (
            |class| class.runtime.control_contract = "two words".to_owned(),
            "control contract revision \"two words\" must hold 1 to 128 printable ASCII \
             characters and no space",
        ),
        (
            |class| class.cpu_profile = "fastest".to_owned(),
            "CPU profile \"fastest\" must be \"auto\", \"host\", or a profile ID",
        ),
        (
            |class| class.processors = 3,
            "a capture of 3 processors with 2 online at boot needs 1, 2, 4, or 8 of each",
        ),
        (
            |class| class.boot_processors = 0,
            "a capture of 2 processors with 0 online at boot",
        ),
        (
            |class| class.boot_processors = 4,
            "a capture of 2 processors with 4 online at boot",
        ),
        (
            |class| class.memory = 0,
            "a capture of 0 bytes of RAM needs a nonzero RAM",
        ),
        (
            |class| class.memory_capacity = class.memory - 1,
            "needs a nonzero RAM and a capacity of at least it",
        ),
        (
            |class| class.distro.logical_block_size = 256,
            "the distro device of a capture is invalid: its logical block size 256 is not a \
             power of two of at least 512",
        ),
        (
            |class| class.scratch.length = 0,
            "the scratch device of a capture is invalid: its length 0",
        ),
    ];
    for (change, expected) in cases {
        let mut class = network_class();
        change(&mut class);
        for error in [
            class.check().unwrap_err(),
            class.to_bytes().unwrap_err(),
            class.identity().unwrap_err(),
            TemplateKey::new(&class, &manifest(&ManifestSpec::with_network())).unwrap_err(),
        ] {
            assert_eq!(error.code(), ErrorCode::Unsupported, "{error}");
            assert!(
                error.message().contains(expected),
                "{:?} lacks {expected:?}",
                error.message()
            );
        }
    }
}

#[test]
fn each_field_of_a_class_changes_its_identity() {
    type ClassChange = fn(&mut CaptureClass);
    let changes: &[ClassChange] = &[
        |class| class.runtime.openvmm = artifact('d'),
        |class| class.runtime.kernel = artifact('d'),
        |class| class.runtime.initramfs = artifact('d'),
        |class| class.runtime.boot_contract += 1,
        |class| class.runtime.control_contract = "other".to_owned(),
        |class| class.runtime.control_session += 1,
        |class| class.guest_build = artifact('e'),
        |class| class.hypervisor = Hypervisor::Whp,
        |class| class.cpu_profile = "host".to_owned(),
        |class| class.processors = 4,
        |class| class.boot_processors = 1,
        |class| class.memory = 1024 * MIB,
        |class| class.memory_capacity = 4096 * MIB,
        |class| class.distro.length *= 2,
        |class| class.distro.physical_block_size = 512,
        |class| class.scratch.length *= 2,
        |class| {
            class.scratch.logical_block_size = 4096;
            class.scratch.physical_block_size = 4096;
        },
        |class| class.network = None,
        |class| {
            let policy = NetworkPolicy::new(Access::Allow);
            class.network = Some(SandboxNetwork::new("10.0.0.3/24", policy).unwrap());
        },
        |class| {
            class.invariants = Invariants::new()
                .with("isolation.seccomp", "strict")
                .unwrap();
        },
    ];
    let base = network_class().identity().unwrap();
    let mut identities = HashSet::from([base]);
    for (index, change) in changes.iter().enumerate() {
        let mut class = network_class();
        change(&mut class);
        assert!(
            identities.insert(class.identity().unwrap()),
            "change {index} leaves the class's identity as it is"
        );
    }
    // A network that attaches no NIC is no network.
    let mut denied = network_class();
    denied.network =
        Some(SandboxNetwork::new("10.0.0.2/24", NetworkPolicy::new(Access::Deny)).unwrap());
    let mut none = network_class();
    none.network = None;
    assert_eq!(denied.identity().unwrap(), none.identity().unwrap());
    // The template captures its NIC under the complete deny-all policy, which each clone's restore
    // replaces, so the policy of a network that attaches one is no part of its class.
    for policy in [
        NetworkPolicy::new(Access::Allow).with_deny(EgressRule::to("192.0.2.0/24")),
        NetworkPolicy::new(Access::Deny).with_allow(EgressRule::to("192.0.2.0/24")),
        NetworkPolicy {
            ingress: Access::Allow,
            ..NetworkPolicy::new(Access::Deny)
        },
        NetworkPolicy {
            host_loopback: HostLoopback::Allow,
            ..NetworkPolicy::new(Access::Deny)
        },
        NetworkPolicy::proxy_only(NetworkProxy::new(3128)),
    ] {
        let mut class = network_class();
        class.network = Some(SandboxNetwork::new("10.0.0.2/24", policy).unwrap());
        assert_eq!(class.identity().unwrap(), base, "{:?}", class.network);
    }
    // The identity digests the canonical encoding after the class's domain.
    let class = network_class();
    let mut digest = Sha256::new();
    digest.update(b"nvx capture class\0");
    digest.update(class.to_bytes().unwrap());
    assert_eq!(
        class.identity().unwrap().as_bytes(),
        &<[u8; 32]>::from(digest.finalize())
    );
}

#[test]
fn keys_read_back_from_their_canonical_encoding() {
    for made in [
        network_key(),
        key(&support::class(), &ManifestSpec::template()),
    ] {
        let bytes = made.to_bytes();
        assert_eq!(&bytes[..2], &[1, 0]);
        let read = TemplateKey::from_bytes(&bytes).unwrap();
        assert_eq!(read, made);
        assert_eq!(read.to_bytes(), bytes);
        let mut digest = Sha256::new();
        digest.update(b"nvx template key\0");
        digest.update(&bytes);
        assert_eq!(
            made.identity().as_bytes(),
            &<[u8; 32]>::from(digest.finalize())
        );
    }
}

/// Returns the message of the refusal of the key bytes `bytes`, which must have the code `code`.
fn key_refusal(bytes: &[u8], code: ErrorCode) -> String {
    let error = TemplateKey::from_bytes(bytes).unwrap_err();
    assert_eq!(error.code(), code, "{error}");
    error.message().to_owned()
}

#[test]
fn other_encodings_of_a_key_are_refused() {
    let key = network_key();
    let bytes = key.to_bytes();
    for version in [0u16, 2, u16::MAX] {
        let mut other = bytes.clone();
        other[..2].copy_from_slice(&version.to_le_bytes());
        assert_eq!(
            key_refusal(&other, ErrorCode::FormatUnsupported),
            format!(
                "the template key has encoding version {version}; this build reads version 1 \
                 only"
            )
        );
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert_eq!(
        key_refusal(&longer, ErrorCode::Io),
        "the template key is malformed: a byte follows the value"
    );
    for len in 0..bytes.len() {
        key_refusal(&bytes[..len], ErrorCode::Io);
    }
    assert_eq!(
        key_refusal(&[], ErrorCode::Io),
        "the template key is malformed: the version is truncated"
    );
    let oversized = vec![0; TemplateKey::MAX_LEN + 1];
    assert!(key_refusal(&oversized, ErrorCode::Io).contains("is longer than the"));
}

#[test]
fn a_key_that_holds_its_network_s_lowering_after_its_invariants_is_refused() {
    // A build that kept each network policy's template apart wrote version 1 with one more part:
    // OpenVMM's arguments to which the class lowered the network, a list of texts after the
    // invariant settings, empty for a template without a NIC. Its keys are malformed here, with
    // or without a NIC, rather than read without that part, so that a store that it created is
    // never misread.
    for (made, lowered) in [
        (
            network_key(),
            vec!["--net", "10.0.0.2/24", "--network-profile", "portable"],
        ),
        (
            key(&support::class(), &ManifestSpec::template()),
            Vec::new(),
        ),
    ] {
        let mut writer = Writer::default();
        writer.list(&lowered, |writer, argument| writer.text(argument));
        let part = writer.finish();
        let bytes = [made.to_bytes(), part.clone()].concat();
        assert!(part.len() > 1, "{part:?}");
        assert_eq!(
            key_refusal(&bytes, ErrorCode::Io),
            format!(
                "the template key is malformed: {} bytes follow the value",
                part.len()
            )
        );
    }
}

#[test]
fn keys_of_machines_that_no_manifest_describes_are_refused() {
    type KeyChange = fn(&mut TemplateKey);
    let cases: &[(KeyChange, &str)] = &[
        (
            |key| key.machine.command_line.push_str(" quiet"),
            "the machine has an effective command line whose digest does not match it",
        ),
        (
            |key| key.machine.devices.clear(),
            "the machine has 0 devices, not 1 to 64",
        ),
        (
            |key| key.machine.cpu.effective_cpuid.pop().map(drop).unwrap(),
            "the machine has an effective CPUID of 87 bytes",
        ),
        (
            |key| key.machine.processors = 3,
            "the machine has 3 processors",
        ),
        (
            |key| key.machine.distro.logical_block_size = 256,
            "the machine has a distro block whose logical block size 256",
        ),
        (
            |key| key.machine.filesystem_slot = 0,
            "the filesystem slot is not the dormant slot of version 1, with a tag",
        ),
        (
            |key| key.filesystem_tag.clear(),
            "the filesystem slot is not the dormant slot of version 1, with a tag",
        ),
        (
            |key| key.machine.cpu.revision = 0,
            "the machine has the invalid CPU profile vendor",
        ),
    ];
    for (change, expected) in cases {
        let mut changed = network_key();
        change(&mut changed);
        let message = key_refusal(&changed.to_bytes(), ErrorCode::Io);
        assert!(
            message.starts_with(&format!("the template key is malformed: {expected}")),
            "{message:?} lacks {expected:?}"
        );
    }
    // Invariant settings out of the order of their names, which a key never writes.
    let mut class = network_class();
    class.invariants = Invariants::new()
        .with("a", "1")
        .unwrap()
        .with("b", "2")
        .unwrap();
    let mut bytes = key(&class, &ManifestSpec::with_network()).to_bytes();
    let len = bytes.len();
    let tail = [
        1, 0, 0, 0, b'b', 1, 0, 0, 0, b'2', 1, 0, 0, 0, b'a', 1, 0, 0, 0, b'1',
    ];
    bytes[len - tail.len()..].copy_from_slice(&tail);
    assert_eq!(
        key_refusal(&bytes, ErrorCode::Io),
        "the template key is malformed: the invariant settings are not in the strict order of \
         their names"
    );
    // An artifact identity that a runtime identity refuses.
    let mut bytes = network_key().to_bytes();
    bytes[2 + 4 + "sha256:".len()] = b' ';
    assert!(
        key_refusal(&bytes, ErrorCode::Io).contains("artifact identity"),
        "{bytes:?}"
    );
}

#[test]
fn invariant_settings_have_one_form() {
    let settings = Invariants::new()
        .with("isolation.seccomp", "strict")
        .unwrap()
        .with("limits.pids", "512")
        .unwrap()
        .with("isolation.seccomp", "relaxed")
        .unwrap();
    assert_eq!(settings.len(), 2);
    assert!(!settings.is_empty());
    assert_eq!(settings.get("isolation.seccomp"), Some("relaxed"));
    assert_eq!(settings.get("limits.cpu"), None);
    assert_eq!(
        settings.iter().collect::<Vec<_>>(),
        [("isolation.seccomp", "relaxed"), ("limits.pids", "512")]
    );
    let long_name = "a".repeat(Invariants::MAX_NAME_LEN + 1);
    let long_value = "v".repeat(Invariants::MAX_VALUE_LEN + 1);
    for (name, value) in [
        ("", "value"),
        ("Isolation", "value"),
        ("1st", "value"),
        ("with space", "value"),
        (long_name.as_str(), "value"),
        ("name", long_value.as_str()),
        ("name", "line\nbreak"),
        ("name", "tab\there"),
        ("name", "é"),
    ] {
        let error = Invariants::new().with(name, value).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unsupported, "{name:?} {value:?}");
    }
    assert!(Invariants::new().with("name", "").is_ok());
    let mut full = Invariants::new();
    for index in 0..Invariants::MAX_SETTINGS {
        full = full.with(&format!("setting{index}"), "1").unwrap();
    }
    assert!(full.clone().with("setting0", "2").is_ok());
    assert_eq!(
        full.with("another", "1").unwrap_err().message(),
        "a template holds at most 64 invariant settings"
    );
}

#[test]
fn a_key_identity_is_the_identity_that_names_a_clones_template() {
    let identity = network_key().identity();
    let named = activation::Identity::from(identity);
    assert_eq!(named.as_bytes(), identity.as_bytes());
    assert_eq!(named.to_string(), identity.to_string());
    assert_eq!(KeyIdentity::from(named), identity);
    // A clone's activation record names the template by its generation and this identity.
    let reference = activation::TemplateReference {
        generation: activation::StoreGeneration::new(std::num::NonZeroU64::MIN),
        key: identity.into(),
    };
    assert_eq!(KeyIdentity::from(reference.key), identity);
}

#[test]
fn identities_have_a_text_form() {
    let identity = network_key().identity();
    let text = identity.to_string();
    assert_eq!(text.len(), 64);
    assert!(
        text.bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
    );
    assert_eq!(text.parse::<KeyIdentity>().unwrap(), identity);
    assert_eq!(
        text.to_uppercase().parse::<KeyIdentity>().unwrap(),
        identity
    );
    assert_eq!(format!("{identity:?}"), format!("KeyIdentity({text})"));
    assert_eq!(KeyIdentity::from_bytes(*identity.as_bytes()), identity);
    let invalid = [
        text[1..].to_owned(),
        format!("{text}0"),
        format!("g{}", &text[1..]),
        String::new(),
        "é".to_owned(),
    ];
    for invalid in &invalid {
        let error = invalid.parse::<KeyIdentity>().unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unsupported);
        assert_eq!(
            error.message(),
            format!("{invalid:?} is not a template key's identity, which is 64 hexadecimal digits")
        );
    }
    let class = network_class().identity().unwrap();
    assert_eq!(class.to_string().parse::<ClassIdentity>().unwrap(), class);
    assert!(
        "x".parse::<ClassIdentity>()
            .unwrap_err()
            .message()
            .contains("is not a capture class's identity")
    );
}

/// The conformance vectors of version 1 of the canonical encodings of keys and classes.
const VECTORS: &str = include_str!("../../../../tests/vectors/template-key-v1.txt");

/// The versions of the contracts that the vectors hold.
const VECTOR_CONTRACTS: Contracts = Contracts {
    snapshot: 6,
    device_state: 1,
    time: 1,
    restore_packet: 4,
    activation: 1,
};

/// Returns each vector of `kind`: its name, its bytes, and its values.
fn vectors(kind: &str) -> Vec<(&'static str, Vec<u8>, Vec<&'static str>)> {
    VECTORS
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split('|');
            (fields.next() == Some(kind)).then(|| {
                let name = fields.next().expect("a vector has a name");
                let hex = fields.next().expect("a vector has bytes");
                let bytes = (0..hex.len())
                    .step_by(2)
                    .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
                    .collect();
                (name, bytes, fields.collect())
            })
        })
        .collect()
}

/// Returns the class of the capture that the vectors call `name`, with the runtime that they
/// hold.
fn vector_class(name: &str) -> CaptureClass {
    let mut class = if name == "NETWORK" {
        let class = network_class();
        CaptureClass {
            invariants: Invariants::new()
                .with("isolation.seccomp", "strict")
                .unwrap()
                .with("limits.pids", "512")
                .unwrap(),
            ..class
        }
    } else {
        support::class()
    };
    class.runtime = RuntimeIdentity {
        openvmm: artifact('a'),
        kernel: artifact('b'),
        initramfs: artifact('c'),
        boot_contract: 3,
        control_contract: "nvx-microvm-v2-control-v2".to_owned(),
        control_session: 1,
    };
    class
}

/// Returns the manifest of the capture that the vectors call `name`.
fn vector_spec(name: &str) -> ManifestSpec {
    match name {
        "TEMPLATE" => ManifestSpec::template(),
        "NETWORK" => ManifestSpec::with_network(),
        "HOST_PROFILE" => {
            let mut spec = ManifestSpec::template();
            spec.cpu().id = "intel.host.v1".to_owned();
            spec
        }
        _ => panic!("no capture is named {name}"),
    }
}

#[test]
fn the_conformance_vectors_decode_and_encode() {
    let keys = vectors("KEY");
    let names: Vec<&str> = keys.iter().map(|(name, ..)| *name).collect();
    assert_eq!(names, ["TEMPLATE", "NETWORK", "HOST_PROFILE", "MINIMAL"]);
    for (name, bytes, values) in keys {
        let key = TemplateKey::from_bytes(&bytes).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(key.to_bytes(), bytes, "{name}");
        assert_eq!(values, [key.identity().to_string()], "{name}");
        if name != "MINIMAL" {
            // The key that this crate derives from the capture is the vector's, but for the
            // versions of the contracts, which are this crate's.
            let derived = TemplateKey::new(&vector_class(name), &manifest(&vector_spec(name)))
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let derived = TemplateKey {
                contracts: VECTOR_CONTRACTS,
                ..derived
            };
            assert_eq!(derived, key, "{name}");
        }
    }
    let classes = vectors("CLASS");
    let names: Vec<&str> = classes.iter().map(|(name, ..)| *name).collect();
    assert_eq!(names, ["TEMPLATE", "NETWORK"]);
    for (name, bytes, values) in classes {
        let class = vector_class(name);
        assert_eq!(class.encode(&VECTOR_CONTRACTS).unwrap(), bytes, "{name}");
        assert_eq!(
            values,
            [ClassIdentity(digest(CLASS_DOMAIN, &bytes)).to_string()],
            "{name}"
        );
    }
    let versions = vectors("INVALID_KEY_VERSION");
    assert_eq!(versions.len(), 3);
    for (name, bytes, _) in versions {
        let error = TemplateKey::from_bytes(&bytes).expect_err(name);
        assert_eq!(
            error.code(),
            ErrorCode::FormatUnsupported,
            "{name}: {error}"
        );
    }
    let invalid = vectors("INVALID_KEY");
    assert_eq!(invalid.len(), 23);
    for (name, bytes, _) in invalid {
        let error = TemplateKey::from_bytes(&bytes).expect_err(name);
        assert_eq!(error.code(), ErrorCode::Io, "{name}: {error}");
    }
}
