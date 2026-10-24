//! Tests of the activation record's types and limits.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU64;

use super::*;

/// The disk GUID of the tests' GPT images.
const DISK_GUID: &str = "fedcba98-7654-3210-fedc-ba9876543210";

/// Returns the configuration of the tests: an EROFS image over a tmpfs, without an export, a
/// network, or a workload identity.
fn configuration() -> Configuration {
    Configuration {
        image: Image::Erofs {
            binding: Identity::from_bytes([0x11; 32]),
            uuid: None,
        },
        upper: Upper::Tmpfs,
        overlay: OverlayMode::TrustedLowers,
        export: None,
        mappings: 0,
        network: None,
        workload: WorkloadView {
            hostname: b"nvx-sandbox".to_vec(),
            machine_id: MachineIdMode::Image,
            identity: None,
        },
        resources: ExpectedResources {
            cpus: 1,
            memory: 256 << 20,
        },
    }
}

/// Returns a configuration whose every part is at its limit: a GPT image over a scratch, an
/// aggregate export of [`MAX_MAPPINGS`] mappings, a network with an IPv6 DNS server and a proxy
/// endpoint of [`MAX_PROXY_ENDPOINT_BYTES`], a hostname of [`MAX_HOSTNAME_BYTES`], and the largest
/// IDs and resources.
fn maximal() -> Configuration {
    Configuration {
        image: Image::Gpt {
            binding: Identity::from_bytes([0xff; 32]),
            disk_guid: Some(DISK_GUID.parse().unwrap()),
            first_partition_bound: u64::MAX,
        },
        upper: Upper::Scratch {
            binding: Identity::from_bytes([0xee; 32]),
            journal: Journal::Forbidden,
        },
        overlay: OverlayMode::UntrustedLowers,
        export: Some(Export {
            kind: ExportKind::Aggregate,
            plan: Identity::from_bytes([0xdd; 32]),
        }),
        mappings: MAX_MAPPINGS,
        network: Some(Network {
            policy: Identity::from_bytes([0xcc; 32]),
            dns: Some(IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0xa00, 1))),
            publication: Publication::Published,
            proxy: Some("~".repeat(MAX_PROXY_ENDPOINT_BYTES)),
        }),
        workload: WorkloadView {
            hostname: vec![0xff; MAX_HOSTNAME_BYTES],
            machine_id: MachineIdMode::Generated,
            identity: Some(WorkloadIdentity {
                uid: u32::MAX - 1,
                gid: u32::MAX - 1,
            }),
        },
        resources: ExpectedResources {
            cpus: u32::MAX,
            memory: u64::MAX,
        },
    }
}

/// Returns the restore section of the tests' clones.
fn restore() -> RestoreSection {
    RestoreSection {
        transaction: TransactionId::from_bytes([0x22; 16]),
        template: TemplateReference {
            generation: StoreGeneration::new(NonZeroU64::new(0x3333_3333_3333_3333).unwrap()),
            key: Identity::from_bytes([0x44; 32]),
        },
    }
}

/// Returns the cold boot's record of the tests' configuration once `change` has changed it.
fn admit(change: impl FnOnce(&mut Configuration)) -> Result<ActivationRecord, ActivationError> {
    let mut configuration = configuration();
    change(&mut configuration);
    ActivationRecord::new(None, configuration)
}

/// Gives a configuration an export of `mappings` mappings.
fn export(configuration: &mut Configuration, mappings: usize) {
    configuration.export = Some(Export {
        kind: ExportKind::Directory,
        plan: Identity::from_bytes([0x55; 32]),
    });
    configuration.mappings = mappings;
}

/// Gives a configuration a network whose proxy endpoint is `proxy`.
fn proxy(configuration: &mut Configuration, proxy: &str) {
    configuration.network = Some(Network {
        policy: Identity::from_bytes([0x66; 32]),
        dns: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
        publication: Publication::Unpublished,
        proxy: Some(proxy.to_owned()),
    });
}

#[test]
fn the_limits_keep_their_values() {
    assert_eq!(VERSION, 1);
    assert_eq!(MAX_RECORD_BYTES, 1024);
    assert_eq!(MAX_MAPPINGS, 4096);
    assert_eq!(MAX_MAPPINGS, crate::protocol::MAX_HOST_MAPPINGS);
    assert_eq!(MAX_HOSTNAME_BYTES, 64);
    assert_eq!(MAX_PROXY_ENDPOINT_BYTES, 255);
    assert_eq!(Identity::LEN, 32);
    assert_eq!(ConfigurationIdentity::LEN, 32);
    assert_eq!(TransactionId::LEN, 16);
}

#[test]
fn a_cold_boot_record_has_no_restore_section_and_a_clone_record_has_one() {
    let cold = ActivationRecord::new(None, configuration()).unwrap();
    assert_eq!(cold.version(), VERSION);
    assert_eq!(cold.restore(), None);
    assert_eq!(cold.configuration(), &configuration());
    let clone = ActivationRecord::new(Some(restore()), configuration()).unwrap();
    assert_eq!(clone.version(), VERSION);
    assert_eq!(clone.restore(), Some(&restore()));
    assert_eq!(clone.configuration(), &configuration());
    assert_ne!(cold, clone);
}

#[test]
fn a_configuration_with_every_part_at_its_limit_is_admitted() {
    let configuration = maximal();
    assert_eq!(configuration.check(), Ok(()));
    let record = ActivationRecord::new(Some(restore()), configuration.clone()).unwrap();
    assert_eq!(record.configuration(), &configuration);
}

#[test]
fn mappings_are_admitted_up_to_the_limit_with_an_export() {
    for mappings in [1, 2, MAX_MAPPINGS - 1, MAX_MAPPINGS] {
        let record = admit(|configuration| export(configuration, mappings)).unwrap();
        assert_eq!(record.configuration().mappings, mappings);
    }
    for mappings in [MAX_MAPPINGS + 1, usize::MAX] {
        assert_eq!(
            admit(|configuration| export(configuration, mappings)),
            Err(ActivationError::TooManyMappings(mappings))
        );
    }
}

#[test]
fn an_export_without_mappings_is_refused() {
    assert_eq!(
        admit(|configuration| export(configuration, 0)),
        Err(ActivationError::ExportWithoutMappings)
    );
}

#[test]
fn mappings_without_an_export_are_refused() {
    for mappings in [1, MAX_MAPPINGS, MAX_MAPPINGS + 1] {
        assert_eq!(
            admit(|configuration| configuration.mappings = mappings),
            Err(ActivationError::MappingsWithoutExport),
            "{mappings}"
        );
    }
}

#[test]
fn hostnames_are_admitted_up_to_the_limit_without_a_nul() {
    for length in [1, 2, MAX_HOSTNAME_BYTES - 1, MAX_HOSTNAME_BYTES] {
        let record =
            admit(|configuration| configuration.workload.hostname = vec![b'a'; length]).unwrap();
        assert_eq!(record.configuration().workload.hostname.len(), length);
    }
    // The guest's entry sets any hostname without a NUL, so a record holds any other byte.
    admit(|configuration| configuration.workload.hostname = b"\x01 \xff.Host".to_vec()).unwrap();
    for hostname in [
        Vec::new(),
        vec![b'a'; MAX_HOSTNAME_BYTES + 1],
        b"\0".to_vec(),
        b"nvx\0sandbox".to_vec(),
        b"nvx-sandbox\0".to_vec(),
    ] {
        assert_eq!(
            admit(|configuration| configuration.workload.hostname = hostname.clone()),
            Err(ActivationError::Hostname),
            "{hostname:?}"
        );
    }
}

#[test]
fn proxy_endpoints_are_admitted_up_to_the_limit_without_credentials() {
    for endpoint in [
        "http://10.0.0.1:3128".to_owned(),
        "10.0.0.1:3128".to_owned(),
        "https://[fd00::a00:1]:8443/".to_owned(),
        "!".to_owned(),
        "~".repeat(MAX_PROXY_ENDPOINT_BYTES - 1),
        "~".repeat(MAX_PROXY_ENDPOINT_BYTES),
    ] {
        let record = admit(|configuration| proxy(configuration, &endpoint)).unwrap();
        let network = record.configuration().network.as_ref().unwrap();
        assert_eq!(network.proxy.as_deref(), Some(endpoint.as_str()));
    }
    for endpoint in [
        String::new(),
        "~".repeat(MAX_PROXY_ENDPOINT_BYTES + 1),
        "http://user:secret@10.0.0.1:3128".to_owned(),
        "http://@10.0.0.1:3128".to_owned(),
        "http://10.0.0.1:3128 ".to_owned(),
        "http://10.0.0.1:3128\n".to_owned(),
        "http://10.0.0.1:3128\0".to_owned(),
        "http://10.0.0.1:3128\u{7f}".to_owned(),
        "http://proxy.ex\u{e1}mple:3128".to_owned(),
    ] {
        assert_eq!(
            admit(|configuration| proxy(configuration, &endpoint)),
            Err(ActivationError::ProxyEndpoint),
            "{endpoint:?}"
        );
    }
    // A network without a proxy needs no endpoint.
    admit(|configuration| {
        proxy(configuration, "unused");
        configuration.network.as_mut().unwrap().proxy = None;
    })
    .unwrap();
}

#[test]
fn a_uid_or_gid_of_0_or_u32_max_is_refused() {
    for (uid, gid) in [
        (1, 1),
        (1000, 1001),
        (65534, 65534),
        (u32::MAX - 1, u32::MAX - 1),
    ] {
        let identity = WorkloadIdentity { uid, gid };
        let record =
            admit(|configuration| configuration.workload.identity = Some(identity)).unwrap();
        assert_eq!(record.configuration().workload.identity, Some(identity));
    }
    for (uid, gid) in [
        (0, 1000),
        (1000, 0),
        (u32::MAX, 1000),
        (1000, u32::MAX),
        (0, 0),
        (u32::MAX, u32::MAX),
        (0, u32::MAX),
    ] {
        assert_eq!(
            admit(|configuration| {
                configuration.workload.identity = Some(WorkloadIdentity { uid, gid });
            }),
            Err(ActivationError::WorkloadIdentity),
            "{uid}:{gid}"
        );
    }
}

#[test]
fn zero_resources_are_refused() {
    assert_eq!(
        admit(|configuration| configuration.resources.cpus = 0),
        Err(ActivationError::Cpus)
    );
    assert_eq!(
        admit(|configuration| configuration.resources.memory = 0),
        Err(ActivationError::Memory)
    );
}

#[test]
fn a_zero_first_partition_bound_is_refused() {
    let gpt = |first_partition_bound| Image::Gpt {
        binding: Identity::from_bytes([0x77; 32]),
        disk_guid: None,
        first_partition_bound,
    };
    for bound in [1, 64 << 10, u64::MAX] {
        admit(|configuration| configuration.image = gpt(bound)).unwrap();
    }
    assert_eq!(
        admit(|configuration| configuration.image = gpt(0)),
        Err(ActivationError::FirstPartitionBound)
    );
}

#[test]
fn the_first_broken_limit_is_reported_in_the_order_of_the_fields() {
    let mut configuration = maximal();
    configuration.resources.memory = 0;
    assert_eq!(configuration.check(), Err(ActivationError::Memory));
    configuration.resources.cpus = 0;
    assert_eq!(configuration.check(), Err(ActivationError::Cpus));
    configuration.workload.identity = Some(WorkloadIdentity { uid: 0, gid: 0 });
    assert_eq!(
        configuration.check(),
        Err(ActivationError::WorkloadIdentity)
    );
    configuration.workload.hostname.clear();
    assert_eq!(configuration.check(), Err(ActivationError::Hostname));
    configuration.network.as_mut().unwrap().proxy = Some(String::new());
    assert_eq!(configuration.check(), Err(ActivationError::ProxyEndpoint));
    configuration.mappings = MAX_MAPPINGS + 1;
    assert_eq!(
        configuration.check(),
        Err(ActivationError::TooManyMappings(MAX_MAPPINGS + 1))
    );
    configuration.mappings = 0;
    assert_eq!(
        configuration.check(),
        Err(ActivationError::ExportWithoutMappings)
    );
    configuration.image = Image::Gpt {
        binding: Identity::from_bytes([0; 32]),
        disk_guid: None,
        first_partition_bound: 0,
    };
    assert_eq!(
        configuration.check(),
        Err(ActivationError::FirstPartitionBound)
    );
}

#[test]
fn errors_name_the_limits_that_they_break() {
    for (error, message) in [
        (
            ActivationError::FirstPartitionBound,
            "the bound of a GPT disk's first partition is 0",
        ),
        (
            ActivationError::ExportWithoutMappings,
            "the export has no mappings",
        ),
        (
            ActivationError::MappingsWithoutExport,
            "the mappings have no export",
        ),
        (
            ActivationError::TooManyMappings(4097),
            "4097 mappings are more than the 4096 that a record announces",
        ),
        (
            ActivationError::ProxyEndpoint,
            "the proxy endpoint is not 1 to 255 visible ASCII characters without an @",
        ),
        (
            ActivationError::Hostname,
            "the hostname is not 1 to 64 bytes without a NUL",
        ),
        (
            ActivationError::WorkloadIdentity,
            "the workload identity's UID or GID is 0 or 4294967295",
        ),
        (ActivationError::Cpus, "the expected CPU count is 0"),
        (ActivationError::Memory, "the expected memory size is 0"),
    ] {
        assert_eq!(error.to_string(), message);
    }
}

#[test]
fn opaque_values_keep_their_bytes_and_display_them_in_hexadecimal() {
    let bytes: [u8; 32] = std::array::from_fn(|index| index as u8 * 8 + 1);
    let digits = "0109111921293139414951596169717981899199a1a9b1b9c1c9d1d9e1e9f1f9";
    let identity = Identity::from_bytes(bytes);
    assert_eq!(identity.as_bytes(), &bytes);
    assert_eq!(identity.to_string(), digits);
    assert_eq!(format!("{identity:?}"), format!("Identity({digits})"));
    let configuration = ConfigurationIdentity::from_bytes(bytes);
    assert_eq!(configuration.as_bytes(), &bytes);
    assert_eq!(configuration.to_string(), digits);
    assert_eq!(
        format!("{configuration:?}"),
        format!("ConfigurationIdentity({digits})")
    );
    let bytes: [u8; 16] = std::array::from_fn(|index| 0xf0 - index as u8);
    let digits = "f0efeeedecebeae9e8e7e6e5e4e3e2e1";
    let transaction = TransactionId::from_bytes(bytes);
    assert_eq!(transaction.as_bytes(), &bytes);
    assert_eq!(transaction.to_string(), digits);
    assert_eq!(
        format!("{transaction:?}"),
        format!("TransactionId({digits})")
    );
}

#[test]
fn a_store_generation_is_a_nonzero_number_in_the_order_of_publication() {
    let first = StoreGeneration::new(NonZeroU64::MIN);
    let number = NonZeroU64::new(1_000_000_007).unwrap();
    let later = StoreGeneration::new(number);
    assert_eq!(first.get(), NonZeroU64::MIN);
    assert_eq!(later.get(), number);
    assert!(first < later);
    assert_eq!(later.to_string(), "1000000007");
    assert_eq!(format!("{later:?}"), "StoreGeneration(1000000007)");
    let last = StoreGeneration::new(NonZeroU64::MAX);
    assert!(later < last);
    assert_eq!(last.to_string(), "18446744073709551615");
}

#[test]
fn an_image_reports_its_format_binding_and_expected_identity() {
    let binding = Identity::from_bytes([0x88; 32]);
    let guid: Uuid = DISK_GUID.parse().unwrap();
    let erofs = Image::Erofs {
        binding,
        uuid: Some(guid),
    };
    assert_eq!(
        (erofs.format(), erofs.binding(), erofs.expected()),
        (ImageFormat::Erofs, &binding, Some(guid))
    );
    let gpt = Image::Gpt {
        binding,
        disk_guid: None,
        first_partition_bound: 1,
    };
    assert_eq!(
        (gpt.format(), gpt.binding(), gpt.expected()),
        (ImageFormat::Gpt, &binding, None)
    );
}

#[test]
fn ready_reports_a_transaction_only_for_a_clone() {
    let identity = ConfigurationIdentity::from_bytes([0x99; 32]);
    let cold = ReadyActivation {
        identity,
        transaction: None,
    };
    let clone = ReadyActivation {
        identity,
        transaction: Some(restore().transaction),
    };
    assert_eq!(cold.identity, clone.identity);
    assert_eq!(
        clone.transaction,
        Some(TransactionId::from_bytes([0x22; 16]))
    );
    assert_ne!(cold, clone);
}
