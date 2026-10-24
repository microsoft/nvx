//! Tests of the configuration identity. The conformance vectors give records and their
//! identities from the definitions of the encoding and of the identity, never from the code that
//! they check.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroU64;

use super::*;
use crate::activation::tests::{configuration, maximal, restore};
use crate::activation::{
    ActivationError, Configuration, ExpectedResources, Export, ExportKind, Identity, Image,
    Journal, MachineIdMode, Network, OverlayMode, Publication, RestoreSection, StoreGeneration,
    TemplateReference, TransactionId, Upper, WorkloadIdentity, WorkloadView,
};
use crate::tests::{ACTIVATION_V1, Values, Vector, of_kind, vector};
use crate::uuid::Uuid;

/// The names of the vectors of records, in order, which are also those of the vectors of what
/// Ready reports about their activations.
const RECORDS: [&str; 5] = [
    "COLD",
    "CLONE",
    "SCRATCH_AGGREGATE",
    "GPT_DIRECTORY",
    "LARGEST",
];

/// Reads a byte string of exactly `N` bytes.
fn array<const N: usize>(values: &mut Values<'_>) -> [u8; N] {
    values
        .bytes()
        .try_into()
        .unwrap_or_else(|bytes: Vec<u8>| panic!("{} bytes are not {N}", bytes.len()))
}

/// Reads an identity.
fn identity(values: &mut Values<'_>) -> Identity {
    Identity::from_bytes(array(values))
}

/// Reads a UUID or a GUID, in the order of its canonical text.
fn uuid(values: &mut Values<'_>) -> Uuid {
    Uuid::from_bytes(array(values))
}

/// Reads a word, which must be one of those of `choices`, and returns the choice that it names.
fn choice<T: Copy>(values: &mut Values<'_>, choices: &[(&str, T)]) -> T {
    let word = values.word();
    choices
        .iter()
        .find(|(name, _)| *name == word)
        .map(|(_, choice)| *choice)
        .unwrap_or_else(|| panic!("{word} names no choice of its value"))
}

/// Returns the record that a `RECORD` vector's values give, and the identity that they give it.
fn record_of(vector: &Vector) -> (ActivationRecord, ConfigurationIdentity) {
    let mut values = vector.values();
    let restore = values.optional(|values| RestoreSection {
        transaction: TransactionId::from_bytes(array(values)),
        template: TemplateReference {
            generation: StoreGeneration::new(
                NonZeroU64::new(values.number()).expect("a store generation of 0"),
            ),
            key: identity(values),
        },
    });
    let image = match values.word() {
        "EROFS" => Image::Erofs {
            binding: identity(&mut values),
            uuid: values.optional(uuid),
        },
        "GPT" => Image::Gpt {
            binding: identity(&mut values),
            first_partition_bound: values.number(),
            disk_guid: values.optional(uuid),
        },
        format => panic!("{format} is no image format"),
    };
    let upper = match values.word() {
        "TMPFS" => Upper::Tmpfs,
        "SCRATCH" => Upper::Scratch {
            binding: identity(&mut values),
            journal: choice(
                &mut values,
                &[
                    ("ALLOWED", Journal::Allowed),
                    ("FORBIDDEN", Journal::Forbidden),
                ],
            ),
        },
        upper => panic!("{upper} is no upper"),
    };
    let overlay = choice(
        &mut values,
        &[
            ("TRUSTED_LOWERS", OverlayMode::TrustedLowers),
            ("UNTRUSTED_LOWERS", OverlayMode::UntrustedLowers),
        ],
    );
    let export = values.optional(|values| {
        let kind = choice(
            values,
            &[
                ("DIRECTORY", ExportKind::Directory),
                ("AGGREGATE", ExportKind::Aggregate),
            ],
        );
        let plan = identity(values);
        (Export { kind, plan }, values.number())
    });
    let (export, mappings) = match export {
        Some((export, mappings)) => (Some(export), mappings),
        None => (None, 0),
    };
    let network = values.optional(|values| Network {
        policy: identity(values),
        publication: choice(
            values,
            &[
                ("PUBLISHED", Publication::Published),
                ("UNPUBLISHED", Publication::Unpublished),
            ],
        ),
        dns: values.optional(|values| {
            let bytes = values.bytes();
            match <[u8; 4]>::try_from(&bytes[..]) {
                Ok(address) => IpAddr::from(address),
                Err(_) => IpAddr::from(<[u8; 16]>::try_from(&bytes[..]).unwrap()),
            }
        }),
        proxy: values.optional(|values| String::from_utf8(values.text()).unwrap()),
    });
    let hostname = values.text();
    let machine_id = choice(
        &mut values,
        &[
            ("IMAGE", MachineIdMode::Image),
            ("GENERATED", MachineIdMode::Generated),
        ],
    );
    let workload_identity = values.optional(|values| WorkloadIdentity {
        uid: values.number(),
        gid: values.number(),
    });
    let resources = ExpectedResources {
        cpus: values.number(),
        memory: values.number(),
    };
    let configuration_identity = ConfigurationIdentity::from_bytes(array(&mut values));
    drop(values);
    let configuration = Configuration {
        image,
        upper,
        overlay,
        export,
        mappings,
        network,
        workload: WorkloadView {
            hostname,
            machine_id,
            identity: workload_identity,
        },
        resources,
    };
    let record = ActivationRecord::new(restore, configuration)
        .unwrap_or_else(|error| panic!("RECORD {}: {error}", vector.name));
    (record, configuration_identity)
}

#[test]
fn records_match_their_vectors() {
    for vector in of_kind(ACTIVATION_V1, "RECORD", &RECORDS) {
        let name = vector.name;
        let (record, identity) = record_of(&vector);
        assert_eq!(record.encode(), vector.bytes, "{name}");
        assert_eq!(
            ActivationRecord::decode(&vector.bytes).as_ref(),
            Ok(&record),
            "{name}"
        );
        assert_eq!(record.identity(), identity, "{name}");
    }
}

#[test]
fn ready_reports_the_identity_of_its_record_and_a_clone_s_transaction() {
    for name in RECORDS {
        let (record, identity) = record_of(&vector(ACTIVATION_V1, "RECORD", name));
        let ready = ReadyActivation::of(&record);
        assert_eq!(ready.identity, identity, "{name}");
        assert_eq!(
            ready.transaction,
            record.restore().map(|restore| restore.transaction),
            "{name}"
        );
        assert_eq!(
            ready.encode(),
            vector(ACTIVATION_V1, "READY", name).bytes,
            "{name}"
        );
    }
}

#[test]
fn invalid_records_are_refused() {
    let names = [
        "EMPTY",
        "TRUNCATED_VERSION",
        "NO_FIELDS",
        "VERSION_0",
        "VERSION_2",
        "TOO_LONG",
        "TRUNCATED_FIELD_HEADER",
        "TRUNCATED_FIELD_VALUE",
        "UNKNOWN_FIELD",
        "REPEATED_FIELD",
        "UPPER_BEFORE_IMAGE",
        "RESTORE_LAST",
        "EMPTY_OVERLAY",
        "LONG_OVERLAY",
        "OVERLAY_CODE_0",
        "OVERLAY_CODE_3",
        "IMAGE_FORMAT_3",
        "GPT_WITHOUT_BOUND",
        "SHORT_RESTORE",
        "SHORT_IMAGE_IDENTITY",
        "SCRATCH_WITHOUT_JOURNAL",
        "MACHINE_ID_CODE_0",
        "DNS_OF_5_BYTES",
        "SHORT_RESOURCES",
        "ZERO_GENERATION",
        "MISSING_IMAGE",
        "MISSING_RESOURCES",
        "EXPORT_WITHOUT_MAPPINGS",
        "MAPPINGS_WITHOUT_EXPORT",
        "ZERO_MAPPINGS",
        "DNS_WITHOUT_NETWORK",
        "PROXY_WITHOUT_NETWORK",
        "TOO_MANY_MAPPINGS",
        "EMPTY_PROXY",
        "PROXY_WITH_CREDENTIALS",
        "PROXY_WITH_SPACE",
        "PROXY_NOT_ASCII",
        "EMPTY_HOSTNAME",
        "HOSTNAME_WITH_NUL",
        "LONG_HOSTNAME",
        "ZERO_FIRST_PARTITION_BOUND",
        "ZERO_UID",
        "ZERO_GID",
        "UNCHANGED_UID",
        "UNCHANGED_GID",
        "ZERO_CPUS",
        "ZERO_MEMORY",
    ];
    for vector in of_kind(ACTIVATION_V1, "INVALID_RECORD", &names) {
        let refusal: Result<_, ActivationError> = ActivationRecord::decode(&vector.bytes);
        assert!(refusal.is_err(), "{} is admitted", vector.name);
    }
}

#[test]
fn identities_change_with_every_field() {
    /// Returns the record of the largest clone once `change` has changed it.
    fn changed(
        change: impl FnOnce(&mut Option<RestoreSection>, &mut Configuration),
    ) -> ActivationRecord {
        let (mut section, mut configuration) = (Some(restore()), maximal());
        change(&mut section, &mut configuration);
        ActivationRecord::new(section, configuration).unwrap()
    }
    /// Returns the restore section of a record that has one.
    fn restore_section(restore: &mut Option<RestoreSection>) -> &mut RestoreSection {
        restore.as_mut().unwrap()
    }
    /// Returns the network of a configuration that has one.
    fn network(configuration: &mut Configuration) -> &mut Network {
        configuration.network.as_mut().unwrap()
    }
    /// Returns the workload identity of a configuration that has one.
    fn ids(configuration: &mut Configuration) -> &mut WorkloadIdentity {
        configuration.workload.identity.as_mut().unwrap()
    }
    let other = Identity::from_bytes([0x5a; 32]);
    let records = [
        changed(|_, _| {}),
        changed(|restore, _| *restore = None),
        changed(|restore, _| {
            restore_section(restore).transaction = TransactionId::from_bytes([1; 16]);
        }),
        changed(|restore, _| {
            restore_section(restore).template.generation = StoreGeneration::new(NonZeroU64::MIN);
        }),
        changed(|restore, _| restore_section(restore).template.key = other),
        changed(|_, configuration| {
            configuration.image = Image::Erofs {
                binding: *configuration.image.binding(),
                uuid: configuration.image.expected(),
            };
        }),
        changed(|_, configuration| {
            if let Image::Gpt { binding, .. } = &mut configuration.image {
                *binding = other;
            }
        }),
        changed(|_, configuration| {
            if let Image::Gpt { disk_guid, .. } = &mut configuration.image {
                *disk_guid = None;
            }
        }),
        changed(|_, configuration| {
            if let Image::Gpt { disk_guid, .. } = &mut configuration.image {
                *disk_guid = Some(Uuid::from_bytes([1; 16]));
            }
        }),
        changed(|_, configuration| {
            if let Image::Gpt {
                first_partition_bound,
                ..
            } = &mut configuration.image
            {
                *first_partition_bound = 1;
            }
        }),
        changed(|_, configuration| configuration.upper = Upper::Tmpfs),
        changed(|_, configuration| {
            if let Upper::Scratch { binding, .. } = &mut configuration.upper {
                *binding = other;
            }
        }),
        changed(|_, configuration| {
            if let Upper::Scratch { journal, .. } = &mut configuration.upper {
                *journal = Journal::Allowed;
            }
        }),
        changed(|_, configuration| configuration.overlay = OverlayMode::TrustedLowers),
        changed(|_, configuration| {
            configuration.export = None;
            configuration.mappings = 0;
        }),
        changed(|_, configuration| {
            configuration.export.as_mut().unwrap().kind = ExportKind::Directory
        }),
        changed(|_, configuration| configuration.export.as_mut().unwrap().plan = other),
        changed(|_, configuration| configuration.mappings = 1),
        changed(|_, configuration| configuration.network = None),
        changed(|_, configuration| network(configuration).policy = other),
        changed(|_, configuration| network(configuration).dns = None),
        changed(|_, configuration| {
            network(configuration).dns = Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        }),
        changed(|_, configuration| network(configuration).publication = Publication::Unpublished),
        changed(|_, configuration| network(configuration).proxy = None),
        changed(|_, configuration| network(configuration).proxy = Some("10.0.0.1:3128".to_owned())),
        changed(|_, configuration| configuration.workload.hostname = b"other".to_vec()),
        changed(|_, configuration| configuration.workload.machine_id = MachineIdMode::Image),
        changed(|_, configuration| configuration.workload.identity = None),
        changed(|_, configuration| ids(configuration).uid = 1000),
        changed(|_, configuration| ids(configuration).gid = 1000),
        changed(|_, configuration| configuration.resources.cpus = 1),
        changed(|_, configuration| configuration.resources.memory = 1),
    ];
    let identities: HashSet<_> = records.iter().map(ActivationRecord::identity).collect();
    assert_eq!(identities.len(), records.len());
}

#[test]
fn a_clone_s_restore_section_is_part_of_its_identity() {
    let cold = ActivationRecord::new(None, configuration()).unwrap();
    let clone = ActivationRecord::new(Some(restore()), configuration()).unwrap();
    assert_ne!(cold.identity(), clone.identity());
    assert_eq!(
        ReadyActivation::of(&cold),
        ReadyActivation {
            identity: cold.identity(),
            transaction: None,
        }
    );
    assert_eq!(
        ReadyActivation::of(&clone),
        ReadyActivation {
            identity: clone.identity(),
            transaction: Some(restore().transaction),
        }
    );
}

#[test]
fn the_label_sets_the_identity_apart_from_the_digest_of_the_record() {
    let record = ActivationRecord::new(Some(restore()), maximal()).unwrap();
    assert_ne!(
        record.identity().as_bytes()[..],
        Sha256::digest(record.encode())[..]
    );
    assert_eq!(LABEL, b"nvx activation configuration identity v1\0");
}
