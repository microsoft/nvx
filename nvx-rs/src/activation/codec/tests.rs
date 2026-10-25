//! Tests of the canonical encoding of an activation record. The tests build their encodings field
//! by field from the encoding's definition, never with the encoder that they check.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU64;

use proptest::prelude::*;

use super::*;
use crate::activation::tests::{configuration, maximal, restore};
use crate::activation::{MAX_HOSTNAME_BYTES, MAX_MAPPINGS, MAX_PROXY_ENDPOINT_BYTES};

/// A field of an encoding, as the tests build it: its tag and its value.
type RawField = (u8, Vec<u8>);

/// Returns the encoding of version `version` with `fields`, in the order given.
fn encoding_of(version: u16, fields: &[RawField]) -> Vec<u8> {
    let mut bytes = version.to_le_bytes().to_vec();
    for (tag, value) in fields {
        bytes.push(*tag);
        bytes.push(u8::try_from(value.len()).unwrap());
        bytes.extend_from_slice(value);
    }
    bytes
}

/// Returns the encoding of version 1 with `fields`, in the order given.
fn encoding(fields: &[RawField]) -> Vec<u8> {
    encoding_of(1, fields)
}

/// Returns the fields of the cold record of the tests' [`configuration`], from the encoding's
/// definition.
fn cold_fields() -> Vec<RawField> {
    vec![
        // An EROFS image, whose binding's identity is 0x11 bytes.
        (0x10, [&[1][..], &[0x11; 32]].concat()),
        // A tmpfs upper.
        (0x20, vec![1]),
        // Trusted lowers.
        (0x30, vec![1]),
        (0x60, b"nvx-sandbox".to_vec()),
        // The image's machine ID.
        (0x61, vec![1]),
        // One CPU and 256 MiB.
        (0x70, vec![1, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0]),
    ]
}

/// Returns `fields` with the field of `tag` set to `value`, in the order of the tags.
fn with(mut fields: Vec<RawField>, tag: u8, value: &[u8]) -> Vec<RawField> {
    fields.retain(|(other, _)| *other != tag);
    let index = fields
        .iter()
        .position(|(other, _)| *other > tag)
        .unwrap_or(fields.len());
    fields.insert(index, (tag, value.to_vec()));
    fields
}

/// Returns `fields` without the field of `tag`.
fn without(mut fields: Vec<RawField>, tag: u8) -> Vec<RawField> {
    fields.retain(|(other, _)| *other != tag);
    fields
}

/// Returns the fields of the cold record with an aggregate export, whose plan's identity is 0x55
/// bytes, and with the mapping count `count`.
fn with_export(count: [u8; 2]) -> Vec<RawField> {
    let fields = with(cold_fields(), 0x40, &[&[2][..], &[0x55; 32]].concat());
    with(fields, 0x41, &count)
}

/// Returns the fields of the cold record with a network, whose policy's identity is 0x66 bytes,
/// and whose listeners are not published.
fn with_network() -> Vec<RawField> {
    with(cold_fields(), 0x50, &[&[0x66; 32][..], &[2]].concat())
}

/// Returns what the decoder makes of `fields`.
fn decode(fields: &[RawField]) -> Result<ActivationRecord, ActivationError> {
    ActivationRecord::decode(&encoding(fields))
}

/// Returns the fields of an encoding, as tag and value, and checks the encoding's version, and
/// that its fields hold nothing past their ends and come in the increasing order of their tags.
fn fields_of(bytes: &[u8]) -> Vec<RawField> {
    assert_eq!(bytes[..2], [1, 0], "the version");
    let mut fields: Vec<RawField> = Vec::new();
    let mut rest = &bytes[2..];
    while let [tag, length, tail @ ..] = rest {
        let (value, tail) = tail.split_at(usize::from(*length));
        assert!(
            fields.last().is_none_or(|(last, _)| last < tag),
            "{tag:#04x}"
        );
        fields.push((*tag, value.to_vec()));
        rest = tail;
    }
    assert!(rest.is_empty());
    fields
}

#[test]
fn the_fields_keep_their_tags_and_names() {
    let expected = [
        (Field::Restore, 0x01, "restore"),
        (Field::Image, 0x10, "image"),
        (Field::ImageIdentity, 0x11, "image identity"),
        (Field::Upper, 0x20, "upper"),
        (Field::Overlay, 0x30, "overlay"),
        (Field::Export, 0x40, "export"),
        (Field::Mappings, 0x41, "mappings"),
        (Field::Network, 0x50, "network"),
        (Field::Dns, 0x51, "DNS"),
        (Field::Proxy, 0x52, "proxy"),
        (Field::Hostname, 0x60, "hostname"),
        (Field::MachineId, 0x61, "machine ID"),
        (Field::WorkloadIdentity, 0x62, "workload identity"),
        (Field::Resources, 0x70, "resources"),
    ];
    assert_eq!(Field::ALL.len(), expected.len());
    for (field, (expected, tag, name)) in Field::ALL.into_iter().zip(expected) {
        assert_eq!(field, expected);
        assert_eq!(field.tag(), tag);
        assert_eq!(field.to_string(), name);
        assert_eq!(Field::from_tag(tag), Some(field));
    }
    for tag in u8::MIN..=u8::MAX {
        if !Field::ALL.iter().any(|field| field.tag() == tag) {
            assert_eq!(Field::from_tag(tag), None, "{tag:#04x}");
        }
    }
}

#[test]
fn the_codes_of_the_choices_keep_their_values() {
    /// Checks that `choices` have the codes 1, 2, and so on, and that no other code names one.
    fn check<T: Code + Copy + PartialEq + std::fmt::Debug>(choices: &[T]) {
        for (index, choice) in choices.iter().enumerate() {
            let code = u8::try_from(index + 1).unwrap();
            assert_eq!(choice.code(), code, "{choice:?}");
            assert_eq!(T::from_code(code), Some(*choice));
        }
        for code in [0, u8::try_from(choices.len() + 1).unwrap(), u8::MAX] {
            assert_eq!(T::from_code(code), None, "{code}");
        }
    }
    check(&[ImageFormat::Erofs, ImageFormat::Gpt]);
    check(&[Journal::Allowed, Journal::Forbidden]);
    check(&[OverlayMode::TrustedLowers, OverlayMode::UntrustedLowers]);
    check(&[ExportKind::Directory, ExportKind::Aggregate]);
    check(&[Publication::Published, Publication::Unpublished]);
    check(&[MachineIdMode::Image, MachineIdMode::Generated]);
    assert_eq!((TMPFS, SCRATCH), (1, 2));
}

#[test]
fn a_cold_record_encodes_to_the_bytes_of_its_definition() {
    let record = ActivationRecord::new(None, configuration()).unwrap();
    let bytes = encoding(&cold_fields());
    assert_eq!(record.encode(), bytes);
    assert_eq!(ActivationRecord::decode(&bytes), Ok(record));
}

#[test]
fn a_clone_record_holds_its_restore_section_first() {
    let record = ActivationRecord::new(Some(restore()), configuration()).unwrap();
    // The transaction ID, the store generation, and the identity of the template's key.
    let section = [&[0x22; 16][..], &[0x33; 8], &[0x44; 32]].concat();
    let bytes = encoding(&with(cold_fields(), 0x01, &section));
    assert_eq!(record.encode(), bytes);
    assert_eq!(ActivationRecord::decode(&bytes), Ok(record));
}

#[test]
fn every_part_of_a_configuration_encodes_to_the_bytes_of_its_definition() {
    let mut configuration = configuration();
    configuration.image = Image::Gpt {
        binding: Identity::from_bytes([0x77; 32]),
        disk_guid: Some("fedcba98-7654-3210-fedc-ba9876543210".parse().unwrap()),
        first_partition_bound: 64 << 10,
    };
    configuration.upper = Upper::Scratch {
        binding: Identity::from_bytes([0x88; 32]),
        journal: Journal::Forbidden,
    };
    configuration.overlay = OverlayMode::UntrustedLowers;
    configuration.export = Some(Export {
        kind: ExportKind::Directory,
        plan: Identity::from_bytes([0x99; 32]),
    });
    configuration.mappings = 0x0102;
    configuration.network = Some(Network {
        policy: Identity::from_bytes([0xaa; 32]),
        dns: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
        publication: Publication::Published,
        proxy: Some("http://10.0.0.1:3128".to_owned()),
    });
    configuration.workload = WorkloadView {
        hostname: b"clone".to_vec(),
        machine_id: MachineIdMode::Generated,
        identity: Some(WorkloadIdentity {
            uid: 1000,
            gid: 0x0102_0304,
        }),
    };
    configuration.resources = ExpectedResources {
        cpus: 4,
        memory: 0x0102_0304_0506_0708,
    };
    let fields = vec![
        (
            0x10,
            [&[2][..], &[0x77; 32], &[0, 0, 1, 0, 0, 0, 0, 0]].concat(),
        ),
        (
            0x11,
            vec![
                0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
                0x32, 0x10,
            ],
        ),
        (0x20, [&[2][..], &[0x88; 32], &[2]].concat()),
        (0x30, vec![2]),
        (0x40, [&[1][..], &[0x99; 32]].concat()),
        (0x41, vec![2, 1]),
        (0x50, [&[0xaa; 32][..], &[1]].concat()),
        (0x51, vec![10, 0, 0, 1]),
        (0x52, b"http://10.0.0.1:3128".to_vec()),
        (0x60, b"clone".to_vec()),
        (0x61, vec![2]),
        (0x62, vec![0xe8, 3, 0, 0, 4, 3, 2, 1]),
        (0x70, vec![4, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1]),
    ];
    let record = ActivationRecord::new(None, configuration.clone()).unwrap();
    assert_eq!(record.encode(), encoding(&fields));
    assert_eq!(decode(&fields), Ok(record));
    // An IPv6 DNS server takes 16 bytes.
    let address = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0xa00, 1);
    configuration.network.as_mut().unwrap().dns = Some(IpAddr::V6(address));
    let fields = with(fields, 0x51, &address.octets());
    let record = ActivationRecord::new(None, configuration).unwrap();
    assert_eq!(record.encode(), encoding(&fields));
    assert_eq!(decode(&fields), Ok(record));
}

#[test]
fn a_maximal_record_holds_602_bytes_within_the_limit() {
    let record = ActivationRecord::new(Some(restore()), maximal()).unwrap();
    let bytes = record.encode();
    assert_eq!(bytes.len(), 602);
    assert!(bytes.len() <= MAX_RECORD_BYTES);
    assert_eq!(
        fields_of(&bytes)
            .iter()
            .map(|(tag, _)| *tag)
            .collect::<Vec<_>>(),
        Field::ALL.map(Field::tag)
    );
    assert_eq!(ActivationRecord::decode(&bytes), Ok(record));
}

#[test]
fn every_prefix_of_an_encoding_is_refused() {
    for record in [
        ActivationRecord::new(None, configuration()).unwrap(),
        ActivationRecord::new(Some(restore()), maximal()).unwrap(),
    ] {
        let bytes = record.encode();
        for length in 0..bytes.len() {
            let error = ActivationRecord::decode(&bytes[..length]).unwrap_err();
            assert!(
                matches!(
                    error,
                    ActivationError::Truncated | ActivationError::MissingField(_)
                ),
                "{length}: {error:?}"
            );
        }
    }
    assert_eq!(
        ActivationRecord::decode(&[]),
        Err(ActivationError::Truncated)
    );
    assert_eq!(
        ActivationRecord::decode(&[1]),
        Err(ActivationError::Truncated)
    );
    assert_eq!(
        ActivationRecord::decode(&[1, 0]),
        Err(ActivationError::MissingField(Field::Image))
    );
}

#[test]
fn bytes_past_the_limit_are_refused_before_anything_else() {
    let mut bytes = encoding(&cold_fields());
    bytes.resize(MAX_RECORD_BYTES + 1, 0);
    assert_eq!(
        ActivationRecord::decode(&bytes),
        Err(ActivationError::TooLong(MAX_RECORD_BYTES + 1))
    );
    let mut bytes = vec![0xff; MAX_RECORD_BYTES * 2];
    assert_eq!(
        ActivationRecord::decode(&bytes),
        Err(ActivationError::TooLong(MAX_RECORD_BYTES * 2))
    );
    // At the limit, the bytes are read: these hold another version.
    bytes.truncate(MAX_RECORD_BYTES);
    assert_eq!(
        ActivationRecord::decode(&bytes),
        Err(ActivationError::Version(0xffff))
    );
}

#[test]
fn other_versions_are_refused() {
    for version in [0, 2, 0x0100, u16::MAX] {
        assert_eq!(
            ActivationRecord::decode(&encoding_of(version, &cold_fields())),
            Err(ActivationError::Version(version)),
            "{version}"
        );
    }
}

#[test]
fn unknown_fields_are_refused() {
    for tag in u8::MIN..=u8::MAX {
        if Field::from_tag(tag).is_some() {
            continue;
        }
        assert_eq!(
            decode(&with(cold_fields(), tag, &[1])),
            Err(ActivationError::UnknownField(tag)),
            "{tag:#04x}"
        );
    }
}

#[test]
fn repeated_fields_are_refused() {
    for (index, (tag, value)) in cold_fields().into_iter().enumerate() {
        let field = Field::from_tag(tag).unwrap();
        let mut fields = cold_fields();
        fields.insert(index + 1, (tag, value.clone()));
        assert_eq!(
            decode(&fields),
            Err(ActivationError::DuplicateField(field)),
            "{field}"
        );
        // A repetition after other fields is a repetition too.
        let mut fields = cold_fields();
        fields.push((tag, value));
        assert_eq!(
            decode(&fields),
            Err(ActivationError::DuplicateField(field)),
            "{field}"
        );
    }
}

#[test]
fn fields_out_of_order_are_refused() {
    let fields = cold_fields();
    for index in 1..fields.len() {
        let mut swapped = fields.clone();
        swapped.swap(index - 1, index);
        let field = Field::from_tag(swapped[index].0).unwrap();
        assert_eq!(
            decode(&swapped),
            Err(ActivationError::FieldOrder(field)),
            "{field}"
        );
    }
    // The restore section comes first.
    let mut fields = cold_fields();
    fields.push((0x01, [&[0x22; 16][..], &[0x33; 8], &[0x44; 32]].concat()));
    assert_eq!(
        decode(&fields),
        Err(ActivationError::FieldOrder(Field::Restore))
    );
}

#[test]
fn a_dns_server_or_a_proxy_without_a_network_is_refused() {
    let dns = with(cold_fields(), 0x51, &[10, 0, 0, 1]);
    assert_eq!(
        decode(&dns),
        Err(ActivationError::WithoutNetwork(Field::Dns))
    );
    let proxy = with(cold_fields(), 0x52, b"http://10.0.0.1:3128");
    assert_eq!(
        decode(&proxy),
        Err(ActivationError::WithoutNetwork(Field::Proxy))
    );
    // With the network, both are read.
    let fields = with(
        with(with_network(), 0x51, &[10, 0, 0, 1]),
        0x52,
        b"10.0.0.1:3128",
    );
    let network = decode(&fields).unwrap().configuration().network.clone();
    assert_eq!(
        network,
        Some(Network {
            policy: Identity::from_bytes([0x66; 32]),
            dns: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            publication: Publication::Unpublished,
            proxy: Some("10.0.0.1:3128".to_owned()),
        })
    );
}

#[test]
fn values_of_another_length_are_refused() {
    let identity = [0x12; 32];
    let cases: Vec<(Field, Vec<RawField>, usize)> = vec![
        (Field::Restore, with(cold_fields(), 0x01, &[1; 55]), 55),
        (Field::Restore, with(cold_fields(), 0x01, &[1; 57]), 57),
        (Field::Image, with(cold_fields(), 0x10, &[]), 0),
        (
            Field::Image,
            with(cold_fields(), 0x10, &[&[1][..], &[0; 31]].concat()),
            32,
        ),
        (
            Field::Image,
            with(cold_fields(), 0x10, &[&[1][..], &[0; 33]].concat()),
            34,
        ),
        (
            Field::Image,
            with(cold_fields(), 0x10, &[&[2][..], &identity].concat()),
            33,
        ),
        (
            Field::Image,
            with(
                cold_fields(),
                0x10,
                &[&[2][..], &identity, &[1; 9]].concat(),
            ),
            42,
        ),
        (
            Field::ImageIdentity,
            with(cold_fields(), 0x11, &[0; 15]),
            15,
        ),
        (
            Field::ImageIdentity,
            with(cold_fields(), 0x11, &[0; 17]),
            17,
        ),
        (Field::Upper, with(cold_fields(), 0x20, &[]), 0),
        (Field::Upper, with(cold_fields(), 0x20, &[1, 1]), 2),
        (Field::Upper, with(cold_fields(), 0x20, &[2]), 1),
        (
            Field::Upper,
            with(cold_fields(), 0x20, &[&[2][..], &identity].concat()),
            33,
        ),
        (
            Field::Upper,
            with(
                cold_fields(),
                0x20,
                &[&[2][..], &identity, &[1, 1]].concat(),
            ),
            35,
        ),
        (Field::Overlay, with(cold_fields(), 0x30, &[]), 0),
        (Field::Overlay, with(cold_fields(), 0x30, &[1, 1]), 2),
        (
            Field::Export,
            with(with_export([1, 0]), 0x40, &[&[1][..], &[0; 31]].concat()),
            32,
        ),
        (
            Field::Export,
            with(with_export([1, 0]), 0x40, &[&[1][..], &[0; 33]].concat()),
            34,
        ),
        (Field::Mappings, with(with_export([1, 0]), 0x41, &[1]), 1),
        (
            Field::Mappings,
            with(with_export([1, 0]), 0x41, &[1, 0, 0]),
            3,
        ),
        (Field::Network, with(cold_fields(), 0x50, &[0; 32]), 32),
        (
            Field::Network,
            with(cold_fields(), 0x50, &[&[0; 32][..], &[1, 1]].concat()),
            34,
        ),
        (Field::Dns, with(with_network(), 0x51, &[]), 0),
        (Field::Dns, with(with_network(), 0x51, &[10, 0, 0]), 3),
        (Field::Dns, with(with_network(), 0x51, &[10, 0, 0, 1, 0]), 5),
        (Field::Dns, with(with_network(), 0x51, &[0; 15]), 15),
        (Field::Dns, with(with_network(), 0x51, &[0; 17]), 17),
        (Field::MachineId, with(cold_fields(), 0x61, &[]), 0),
        (Field::MachineId, with(cold_fields(), 0x61, &[1, 1]), 2),
        (
            Field::WorkloadIdentity,
            with(cold_fields(), 0x62, &[1; 7]),
            7,
        ),
        (
            Field::WorkloadIdentity,
            with(cold_fields(), 0x62, &[1; 9]),
            9,
        ),
        (Field::Resources, with(cold_fields(), 0x70, &[1; 11]), 11),
        (Field::Resources, with(cold_fields(), 0x70, &[1; 13]), 13),
    ];
    for (field, fields, length) in cases {
        assert_eq!(
            decode(&fields),
            Err(ActivationError::FieldLength(field, length)),
            "{field} of {length} bytes"
        );
    }
}

#[test]
fn unknown_codes_are_refused() {
    let identity = [0x12; 32];
    for code in [0, 3, u8::MAX] {
        let cases = [
            (
                Field::Image,
                with(cold_fields(), 0x10, &[&[code][..], &identity].concat()),
            ),
            (Field::Upper, with(cold_fields(), 0x20, &[code])),
            (
                Field::Upper,
                with(
                    cold_fields(),
                    0x20,
                    &[&[2][..], &identity, &[code]].concat(),
                ),
            ),
            (Field::Overlay, with(cold_fields(), 0x30, &[code])),
            (
                Field::Export,
                with(
                    with_export([1, 0]),
                    0x40,
                    &[&[code][..], &identity].concat(),
                ),
            ),
            (
                Field::Network,
                with(cold_fields(), 0x50, &[&identity[..], &[code]].concat()),
            ),
            (Field::MachineId, with(cold_fields(), 0x61, &[code])),
        ];
        for (field, fields) in cases {
            assert_eq!(
                decode(&fields),
                Err(ActivationError::FieldValue(field)),
                "{field} of code {code}"
            );
        }
    }
}

#[test]
fn a_mapping_count_of_zero_is_refused() {
    assert_eq!(
        decode(&with_export([0, 0])),
        Err(ActivationError::FieldValue(Field::Mappings))
    );
    assert_eq!(
        decode(&with(cold_fields(), 0x41, &[0, 0])),
        Err(ActivationError::FieldValue(Field::Mappings))
    );
}

#[test]
fn a_store_generation_of_0_is_refused() {
    let section = [&[0x22; 16][..], &[0; 8], &[0x44; 32]].concat();
    assert_eq!(
        decode(&with(cold_fields(), 0x01, &section)),
        Err(ActivationError::FieldValue(Field::Restore))
    );
    // A store's first generation, 1, is read.
    let section = [&[0x22; 16][..], &[1, 0, 0, 0, 0, 0, 0, 0], &[0x44; 32]].concat();
    let record = decode(&with(cold_fields(), 0x01, &section)).unwrap();
    assert_eq!(
        record.restore().map(|restore| restore.template.generation),
        Some(StoreGeneration::new(NonZeroU64::MIN))
    );
}

#[test]
fn an_export_without_mappings_and_mappings_without_an_export_are_refused() {
    let export = without(with_export([1, 0]), 0x41);
    assert_eq!(decode(&export), Err(ActivationError::ExportWithoutMappings));
    let mappings = without(with_export([1, 0]), 0x40);
    assert_eq!(
        decode(&mappings),
        Err(ActivationError::MappingsWithoutExport)
    );
    for (count, mappings) in [([1, 0], 1), ([0, 0x10], MAX_MAPPINGS)] {
        let record = decode(&with_export(count)).unwrap();
        assert_eq!(record.configuration().mappings, mappings);
    }
}

#[test]
fn values_past_a_limit_are_refused() {
    assert_eq!(
        decode(&with_export([1, 0x10])),
        Err(ActivationError::TooManyMappings(MAX_MAPPINGS + 1))
    );
    assert_eq!(
        decode(&with_export([0xff, 0xff])),
        Err(ActivationError::TooManyMappings(usize::from(u16::MAX)))
    );
    let hostname = vec![b'a'; MAX_HOSTNAME_BYTES + 1];
    assert_eq!(
        decode(&with(cold_fields(), 0x60, &hostname)),
        Err(ActivationError::Hostname)
    );
    let hostname = vec![b'a'; MAX_HOSTNAME_BYTES];
    decode(&with(cold_fields(), 0x60, &hostname)).unwrap();
    let proxy = vec![b'~'; MAX_PROXY_ENDPOINT_BYTES];
    decode(&with(with_network(), 0x52, &proxy)).unwrap();
}

#[test]
fn values_that_a_configuration_refuses_are_refused() {
    let identity = [0x12; 32];
    let cases = [
        (
            with(
                cold_fields(),
                0x10,
                &[&[2][..], &identity, &[0; 8]].concat(),
            ),
            ActivationError::FirstPartitionBound,
        ),
        (
            with(with_network(), 0x52, b""),
            ActivationError::ProxyEndpoint,
        ),
        (
            with(with_network(), 0x52, b"http://user:secret@10.0.0.1:3128"),
            ActivationError::ProxyEndpoint,
        ),
        (
            with(with_network(), 0x52, b"http://10.0.0.1:3128 "),
            ActivationError::ProxyEndpoint,
        ),
        (
            with(with_network(), 0x52, b"http://proxy\xff:3128"),
            ActivationError::ProxyEndpoint,
        ),
        (
            with(with_network(), 0x52, "http://\u{e1}:3128".as_bytes()),
            ActivationError::ProxyEndpoint,
        ),
        (with(cold_fields(), 0x60, b""), ActivationError::Hostname),
        (
            with(cold_fields(), 0x60, b"nvx\0sandbox"),
            ActivationError::Hostname,
        ),
        (
            with(cold_fields(), 0x62, &[0, 0, 0, 0, 1, 0, 0, 0]),
            ActivationError::WorkloadIdentity,
        ),
        (
            with(cold_fields(), 0x62, &[1, 0, 0, 0, 0, 0, 0, 0]),
            ActivationError::WorkloadIdentity,
        ),
        (
            with(cold_fields(), 0x62, &[0xff, 0xff, 0xff, 0xff, 1, 0, 0, 0]),
            ActivationError::WorkloadIdentity,
        ),
        (
            with(cold_fields(), 0x62, &[1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]),
            ActivationError::WorkloadIdentity,
        ),
        (
            with(cold_fields(), 0x70, &[0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]),
            ActivationError::Cpus,
        ),
        (
            with(cold_fields(), 0x70, &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            ActivationError::Memory,
        ),
    ];
    for (fields, error) in cases {
        assert_eq!(decode(&fields), Err(error.clone()), "{error}");
    }
}

#[test]
fn missing_fields_are_refused() {
    for tag in [0x10, 0x20, 0x30, 0x60, 0x61, 0x70] {
        let field = Field::from_tag(tag).unwrap();
        assert_eq!(
            decode(&without(cold_fields(), tag)),
            Err(ActivationError::MissingField(field)),
            "{field}"
        );
    }
}

#[test]
fn refusals_follow_the_order_of_their_rules() {
    // A field's tag is read before its value's end.
    let mut bytes = encoding(&cold_fields());
    bytes.extend_from_slice(&[0xff, 1]);
    assert_eq!(
        ActivationRecord::decode(&bytes),
        Err(ActivationError::UnknownField(0xff))
    );
    // A field whose tag and length end past the bytes is truncated, whatever its tag.
    bytes.truncate(bytes.len() - 1);
    assert_eq!(
        ActivationRecord::decode(&bytes),
        Err(ActivationError::Truncated)
    );
    // A known field whose value ends past the bytes is truncated.
    let mut bytes = encoding(&without(cold_fields(), 0x70));
    bytes.extend_from_slice(&[0x70, 12, 1, 0, 0, 0]);
    assert_eq!(
        ActivationRecord::decode(&bytes),
        Err(ActivationError::Truncated)
    );
    // A field's own refusal comes before the record's missing fields.
    let fields = with(without(cold_fields(), 0x10), 0x61, &[3]);
    assert_eq!(
        decode(&fields),
        Err(ActivationError::FieldValue(Field::MachineId))
    );
    // Of the missing fields, the first in the order of the tags.
    let fields = without(without(cold_fields(), 0x70), 0x20);
    assert_eq!(
        decode(&fields),
        Err(ActivationError::MissingField(Field::Upper))
    );
    // A missing field comes before a DNS server without a network.
    let fields = with(without(cold_fields(), 0x60), 0x51, &[10, 0, 0, 1]);
    assert_eq!(
        decode(&fields),
        Err(ActivationError::MissingField(Field::Hostname))
    );
    // A DNS server without a network comes before a broken limit.
    let fields = with(with(cold_fields(), 0x51, &[10, 0, 0, 1]), 0x70, &[0; 12]);
    assert_eq!(
        decode(&fields),
        Err(ActivationError::WithoutNetwork(Field::Dns))
    );
}

#[test]
fn refusals_name_the_rule_that_they_break() {
    for (error, message) in [
        (
            ActivationError::TooLong(1025),
            "the record's 1025 bytes are more than the 1024 that a record holds",
        ),
        (
            ActivationError::Truncated,
            "the record ends inside its version or one of its fields",
        ),
        (
            ActivationError::Version(2),
            "the record's version is 2, not 1",
        ),
        (
            ActivationError::UnknownField(0x0f),
            "the record holds a field of the unknown tag 0x0f",
        ),
        (
            ActivationError::DuplicateField(Field::Hostname),
            "the record repeats its hostname field",
        ),
        (
            ActivationError::FieldOrder(Field::Upper),
            "the record's upper field is out of order",
        ),
        (
            ActivationError::FieldLength(Field::Dns, 5),
            "the record's DNS field holds 5 bytes, which its value never takes",
        ),
        (
            ActivationError::FieldValue(Field::MachineId),
            "the record's machine ID field holds a value that no record takes",
        ),
        (
            ActivationError::MissingField(Field::Resources),
            "the record has no resources field",
        ),
        (
            ActivationError::WithoutNetwork(Field::Proxy),
            "the record has a proxy field but no network",
        ),
    ] {
        assert_eq!(error.to_string(), message);
    }
}

/// Returns arbitrary identities.
fn identities() -> impl Strategy<Value = Identity> {
    any::<[u8; 32]>().prop_map(Identity::from_bytes)
}

/// Returns arbitrary UUIDs, or none.
fn uuids() -> impl Strategy<Value = Option<Uuid>> {
    any::<Option<[u8; 16]>>().prop_map(|bytes| bytes.map(Uuid::from_bytes))
}

/// Returns arbitrary valid configurations.
fn configurations() -> impl Strategy<Value = Configuration> {
    let image = prop_oneof![
        (identities(), uuids()).prop_map(|(binding, uuid)| Image::Erofs { binding, uuid }),
        (identities(), uuids(), 1..=u64::MAX).prop_map(|(binding, disk_guid, bound)| {
            Image::Gpt {
                binding,
                disk_guid,
                first_partition_bound: bound,
            }
        }),
    ];
    let upper = prop_oneof![
        Just(Upper::Tmpfs),
        (
            identities(),
            prop_oneof![Just(Journal::Allowed), Just(Journal::Forbidden)]
        )
            .prop_map(|(binding, journal)| Upper::Scratch { binding, journal }),
    ];
    let overlay = prop_oneof![
        Just(OverlayMode::TrustedLowers),
        Just(OverlayMode::UntrustedLowers)
    ];
    let export = prop_oneof![
        Just((None::<Export>, 0_usize)),
        (
            prop_oneof![Just(ExportKind::Directory), Just(ExportKind::Aggregate)],
            identities(),
            1..=MAX_MAPPINGS,
        )
            .prop_map(|(kind, plan, mappings)| (Some(Export { kind, plan }), mappings)),
    ];
    let address = prop_oneof![
        any::<[u8; 4]>().prop_map(IpAddr::from),
        any::<[u8; 16]>().prop_map(IpAddr::from),
    ];
    // Visible ASCII characters other than `@`.
    let proxy = prop::collection::vec(
        prop_oneof![0x21_u8..=0x3f, 0x41_u8..=0x7e],
        1..=MAX_PROXY_ENDPOINT_BYTES,
    )
    .prop_map(|bytes| String::from_utf8(bytes).unwrap());
    let network = prop::option::of(
        (
            identities(),
            prop::option::of(address),
            prop_oneof![Just(Publication::Published), Just(Publication::Unpublished)],
            prop::option::of(proxy),
        )
            .prop_map(|(policy, dns, publication, proxy)| Network {
                policy,
                dns,
                publication,
                proxy,
            }),
    );
    let workload = (
        prop::collection::vec(1_u8..=u8::MAX, 1..=MAX_HOSTNAME_BYTES),
        prop_oneof![Just(MachineIdMode::Image), Just(MachineIdMode::Generated)],
        prop::option::of(
            (1..u32::MAX, 1..u32::MAX).prop_map(|(uid, gid)| WorkloadIdentity { uid, gid }),
        ),
    )
        .prop_map(|(hostname, machine_id, identity)| WorkloadView {
            hostname,
            machine_id,
            identity,
        });
    let resources =
        (1..=u32::MAX, 1..=u64::MAX).prop_map(|(cpus, memory)| ExpectedResources { cpus, memory });
    (image, upper, overlay, export, network, workload, resources).prop_map(
        |(image, upper, overlay, (export, mappings), network, workload, resources)| Configuration {
            image,
            upper,
            overlay,
            export,
            mappings,
            network,
            workload,
            resources,
        },
    )
}

/// Returns arbitrary valid records, of cold boots and of clones.
fn records() -> impl Strategy<Value = ActivationRecord> {
    let restore = prop::option::of(
        (any::<[u8; 16]>(), any::<NonZeroU64>(), identities()).prop_map(
            |(transaction, generation, key)| RestoreSection {
                transaction: TransactionId::from_bytes(transaction),
                template: TemplateReference {
                    generation: StoreGeneration::new(generation),
                    key,
                },
            },
        ),
    );
    (restore, configurations())
        .prop_map(|(restore, configuration)| ActivationRecord::new(restore, configuration).unwrap())
}

/// Returns bytes that a decoder may meet: arbitrary bytes, arbitrary fields after the version,
/// mostly of known tags, and valid encodings with bytes overwritten, removed, or repeated.
fn inputs() -> impl Strategy<Value = Vec<u8>> {
    let tag = prop_oneof![
        any::<u8>(),
        prop::sample::select(Field::ALL.map(Field::tag).to_vec()),
    ];
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..64),
        prop::collection::vec((tag, prop::collection::vec(any::<u8>(), 0..48)), 0..16)
            .prop_map(|fields| encoding(&fields)),
        (
            records(),
            prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..4),
        )
            .prop_map(|(record, writes)| {
                let mut bytes = record.encode();
                for (index, byte) in writes {
                    let index = index.index(bytes.len());
                    bytes[index] = byte;
                }
                bytes
            }),
        (records(), any::<prop::sample::Index>(), any::<bool>()).prop_map(
            |(record, index, remove)| {
                let mut bytes = record.encode();
                let index = index.index(bytes.len());
                if remove {
                    bytes.remove(index);
                } else {
                    bytes.insert(index, bytes[index]);
                }
                bytes
            }
        ),
    ]
}

proptest! {
    /// Every valid record encodes to at most `MAX_RECORD_BYTES` bytes, whose fields come in the
    /// order of their tags, and which decode back to the record, while every prefix of them is
    /// refused.
    #[test]
    fn records_round_trip_through_their_canonical_encoding(
        record in records(),
        cut in any::<prop::sample::Index>(),
    ) {
        let bytes = record.encode();
        prop_assert!(bytes.len() <= MAX_RECORD_BYTES);
        fields_of(&bytes);
        prop_assert_eq!(ActivationRecord::decode(&bytes), Ok(record));
        prop_assert!(ActivationRecord::decode(&bytes[..cut.index(bytes.len())]).is_err());
    }

    /// The decoder returns, never panics, on any bytes, and accepts only the one encoding of the
    /// record that it returns.
    #[test]
    fn the_decoder_accepts_only_canonical_encodings(bytes in inputs()) {
        if let Ok(record) = ActivationRecord::decode(&bytes) {
            prop_assert_eq!(record.encode(), bytes);
        }
    }
}
