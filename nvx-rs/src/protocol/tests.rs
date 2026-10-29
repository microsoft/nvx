//! Tests of the control protocol, including its conformance vectors.
//!
//! `tests/vectors/nvxc-v1.txt` holds the bytes of the records, frames, and payloads that these
//! tests pin, which come from the protocol's definition rather than from this codec. The host's
//! encoders, and the decoders that the guest agent and the fake OpenVMM share with it, must all
//! match them, so a change to the codec that every side would agree on still fails here.

use proptest::prelude::*;

use super::nvxs::{RecordParser, RecordType, encode_record};
use super::*;
use crate::tests::{NVXC_V1, Values, of_kind, vector, vectors};

/// A workload that runs `argv` without a timeout in `/` with the default environment.
fn program<'a>(argv: &[&str]) -> Workload<'a> {
    Workload {
        argv: argv.iter().map(|argument| (*argument).to_owned()).collect(),
        timeout_ms: 0,
        cwd: None,
        environment: WorkloadEnvironment::Default,
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

/// Returns the NVXS record type that a vector names, as [`RecordType::name`] spells it.
fn record_type(name: &str) -> RecordType {
    (u8::MIN..=u8::MAX)
        .filter_map(RecordType::from_code)
        .find(|record_type| record_type.name() == name)
        .unwrap_or_else(|| panic!("no record type is named {name}"))
}

/// Returns the frame kind that a vector names.
fn frame_kind(name: &str) -> u8 {
    match name {
        "PING" => APP_PING,
        "EXEC" => APP_EXEC,
        "STOP" => APP_STOP,
        "CANCEL" => APP_CANCEL,
        "FEATURES" => APP_FEATURES,
        "MAPS" => APP_MAPS,
        "READY" => APP_READY,
        "STDOUT" => APP_STDOUT,
        "STDERR" => APP_STDERR,
        "EXIT" => APP_EXIT,
        "STOPPED" => APP_STOPPED,
        "ERROR" => APP_ERROR,
        _ => panic!("no frame kind is named {name}"),
    }
}

/// Returns the bytes of a vector's text as the host's text, which is UTF-8.
fn utf8(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Each `RECORD` vector decodes, with the outer codec and with the NVXS record parser, to its
/// values, which both encode to its bytes.
#[test]
fn records_match_their_vectors() {
    let names = ["HOST_ATTACH", "DATA_PING", "READY", "ACK", "CREDIT"];
    for vector in of_kind(NVXC_V1, "RECORD", &names) {
        let name = vector.name;
        let mut values = vector.values();
        let record_type = record_type(values.word());
        let instance_id: [u8; 16] = array(&values.bytes());
        let epoch: u64 = values.number();
        let sequence: u64 = values.number();
        let payload = values.bytes();
        drop(values);
        let code = record_type.code();
        let encoded = encode_outer(code, &instance_id, epoch, sequence, &payload).unwrap();
        assert_eq!(encoded, vector.bytes, "{name}");
        let header = OuterHeader {
            record_type: code,
            instance_id,
            epoch,
            sequence,
            length: payload.len(),
        };
        let decoded = decode_outer_header(&array(&vector.bytes[..OUTER_HEADER_LEN])).unwrap();
        assert_eq!(decoded, header, "{name}");
        let mut encoded = Vec::new();
        encode_record(
            record_type,
            &instance_id,
            epoch,
            sequence,
            &payload,
            &mut encoded,
        )
        .unwrap();
        assert_eq!(encoded, vector.bytes, "{name}");
        let (consumed, record) = RecordParser::new().push(&vector.bytes).unwrap();
        let record = record.unwrap();
        assert_eq!(consumed, vector.bytes.len(), "{name}");
        assert_eq!(
            (
                record.record_type(),
                record.instance_id(),
                record.epoch(),
                record.sequence()
            ),
            (record_type, instance_id, epoch, sequence),
            "{name}"
        );
        assert_eq!(record.payload(), payload, "{name}");
        // So do the codecs of the payloads of acknowledgements and credits.
        match record_type {
            RecordType::Ack => {
                let window = decode_ack_payload(&payload).unwrap();
                assert_eq!(encode_ack_payload(window).unwrap()[..], payload, "{name}");
            }
            RecordType::Credit => {
                let credit = decode_credit_payload(&payload).unwrap();
                assert_eq!(
                    encode_credit_payload(credit).unwrap()[..],
                    payload,
                    "{name}"
                );
            }
            _ => {}
        }
    }
    // The DATA record carries the PING frame.
    let data = vector(NVXC_V1, "RECORD", "DATA_PING");
    assert_eq!(
        data.bytes[OUTER_HEADER_LEN..],
        vector(NVXC_V1, "FRAME", "PING").bytes
    );
}

/// Each `FRAME` vector decodes to its values, which encode to its bytes.
#[test]
fn frames_match_their_vectors() {
    let names = ["PING", "STDOUT", "EXIT", "ERROR_CWD_FAILED"];
    for vector in of_kind(NVXC_V1, "FRAME", &names) {
        let name = vector.name;
        let mut values = vector.values();
        let kind = frame_kind(values.word());
        let request_id: u64 = values.number();
        let status: i32 = values.number();
        let payload = values.bytes();
        drop(values);
        let encoded = encode_app(kind, request_id, status, &payload);
        assert_eq!(encoded, vector.bytes, "{name}");
        let frame = AppFrame {
            kind,
            request_id,
            status,
            payload: &payload,
        };
        assert_eq!(decode_app(&vector.bytes).unwrap(), frame, "{name}");
    }
}

/// Each `EXEC` vector decodes, with the host's and with the guest's decoder, to its values, which
/// the host encodes to its bytes. Without an explicit environment, the host's
/// `inherit_default_env` is true, and the guest's, which follows the layering flag alone, false
/// (microsoft/nvx#581).
#[test]
fn exec_payloads_match_their_vectors() {
    let names = [
        "SHELL",
        "LONGEST_TIMEOUT",
        "DIRECTORY",
        "LAYERED_ENVIRONMENT",
        "REPLACED_ENVIRONMENT",
        "EMPTY_ENVIRONMENT",
    ];
    for vector in of_kind(NVXC_V1, "EXEC", &names) {
        let name = vector.name;
        let mut values = vector.values();
        let timeout_ms: u32 = values.number();
        let argv = values.list(Values::text);
        let cwd = values.optional(Values::text);
        let mode = values.word();
        let environment = match mode {
            "DEFAULT" => None,
            "LAYERED" | "REPLACED" => Some(values.list(Values::text)),
            _ => panic!("{name} names the unknown environment {mode}"),
        };
        drop(values);
        let host_argv: Vec<_> = argv.iter().map(|argument| utf8(argument)).collect();
        let host_cwd = cwd.as_deref().map(utf8);
        let host_environment: Option<Vec<_>> = environment
            .as_ref()
            .map(|entries| entries.iter().map(|entry| utf8(entry)).collect());
        let workload = Workload {
            argv: host_argv.clone(),
            timeout_ms,
            cwd: host_cwd.as_deref(),
            environment: match (mode, &host_environment) {
                ("LAYERED", Some(entries)) => WorkloadEnvironment::Layered(entries),
                ("REPLACED", Some(entries)) => WorkloadEnvironment::Replaced(entries),
                _ => WorkloadEnvironment::Default,
            },
        };
        let encoded = encode_exec_payload(&workload).unwrap();
        assert_eq!(encoded, vector.bytes, "{name}");
        let host = ExecPayload {
            timeout_ms,
            argv: host_argv,
            cwd: host_cwd.clone(),
            environment: host_environment.clone(),
            inherit_default_env: mode != "REPLACED",
        };
        assert_eq!(decode_exec_payload(&vector.bytes).unwrap(), host, "{name}");
        let guest = GuestExecPayload {
            timeout_ms,
            argv,
            cwd,
            environment,
            inherit_default_env: mode == "LAYERED",
        };
        let decoded = decode_guest_exec_payload(&vector.bytes).unwrap();
        assert_eq!(decoded, guest, "{name}");
    }
}

/// Each `MAPS` vector decodes, with the host's and with the guest's decoder, to its values, and the
/// host encodes a mapping table that holds its entries to its bytes: the root of the export,
/// which an empty source names, included.
#[test]
fn maps_payloads_match_their_vectors() {
    let names = ["ONE_ENTRY", "TWO_ENTRIES", "CONTINUED", "ROOT_SOURCE"];
    for vector in of_kind(NVXC_V1, "MAPS", &names) {
        let name = vector.name;
        let mut values = vector.values();
        let first: u32 = values.number();
        let entries = values.list(|values| {
            let read_only = match values.word() {
                "RO" => true,
                "RW" => false,
                access => panic!("{name} names the unknown access {access}"),
            };
            GuestHostMap {
                source: values.text(),
                target: values.text(),
                read_only,
            }
        });
        drop(values);
        let maps: Vec<_> = entries
            .iter()
            .map(|entry| HostMap {
                source: utf8(&entry.source),
                target: utf8(&entry.target),
                read_only: entry.read_only,
            })
            .collect();
        // The host encodes a whole table, so the vector's entries follow `first` entries, each of
        // which fills a payload of the vector's length alone.
        let limit = vector.bytes.len();
        let filler = "f".repeat(limit - MAPS_HEADER_LEN - MAP_ENTRY_HEADER_LEN - 2);
        let filler = map("0", &format!("/{filler}"), false);
        let table: Vec<_> = std::iter::repeat_n(filler, first as usize)
            .chain(maps.iter().cloned())
            .collect();
        let payloads = encode_maps_payloads(&table, limit).unwrap();
        assert_eq!(payloads.len(), first as usize + 1, "{name}");
        assert_eq!(payloads.last(), Some(&vector.bytes), "{name}");
        let host = MapsPayload { first, maps };
        assert_eq!(decode_maps_payload(&vector.bytes).unwrap(), host, "{name}");
        let guest = GuestMapsPayload {
            first,
            maps: entries,
        };
        let decoded = decode_guest_maps_payload(&vector.bytes).unwrap();
        assert_eq!(decoded, guest, "{name}");
    }
}

/// Each `FEATURES` vector decodes to its bits, which encode to its bytes.
#[test]
fn features_match_their_vectors() {
    for vector in of_kind(NVXC_V1, "FEATURES", &["ALL"]) {
        let mut values = vector.values();
        let features = GuestFeatures::from_bits(values.number());
        drop(values);
        assert_eq!(features.encode()[..], vector.bytes, "{}", vector.name);
        let decoded = GuestFeatures::decode(&vector.bytes).unwrap();
        assert_eq!(decoded, features, "{}", vector.name);
    }
}

#[test]
fn the_host_requires_every_feature() {
    assert_eq!(
        GuestFeatures::CANCEL
            | GuestFeatures::WORKLOAD_ACCOUNT
            | GuestFeatures::EXEC_CGROUP
            | GuestFeatures::EXEC_ENVIRONMENT
            | GuestFeatures::EXEC_CWD
            | GuestFeatures::HOST_MAPPING_TABLE,
        GuestFeatures::REQUIRED
    );
    assert_eq!(
        GuestFeatures::REQUIRED.encode()[..],
        vector(NVXC_V1, "FEATURES", "ALL").bytes
    );
}

#[test]
fn exec_payload_carries_every_32_bit_timeout() {
    for timeout_ms in [0, 3_600_001, 86_400_000, MAX_TIMEOUT_MS] {
        let workload = Workload {
            timeout_ms,
            cwd: Some("/tmp"),
            ..program(&["/bin/true"])
        };
        let payload = encode_exec_payload(&workload).unwrap();
        assert_eq!(payload[..4], timeout_ms.to_le_bytes(), "{timeout_ms}");
        assert_eq!(
            decode_exec_payload(&payload).unwrap().timeout_ms,
            timeout_ms
        );
    }
}

#[test]
fn exec_payload_carries_the_working_directory_natively() {
    for cwd in ["", "tmp", &format!("/{}", "a".repeat(4096)), "/t\0mp"] {
        let workload = Workload {
            cwd: Some(cwd),
            ..program(&["/bin/pwd"])
        };
        assert!(encode_exec_payload(&workload).is_err(), "{cwd:?}");
    }
    let longest = format!("/{}", "a".repeat(4095));
    let workload = Workload {
        cwd: Some(&longest),
        ..program(&["/bin/pwd"])
    };
    let payload = encode_exec_payload(&workload).unwrap();
    assert_eq!(decode_exec_payload(&payload).unwrap().cwd, Some(longest));
}

#[test]
fn exec_payload_distinguishes_explicit_environments() {
    let environment = strings(&["FOO=a b", "EMPTY="]);
    let empty = Workload {
        environment: WorkloadEnvironment::Replaced(&[]),
        ..program(&["/bin/env"])
    };
    let empty = encode_exec_payload(&empty).unwrap();
    assert_eq!(&empty[6..16], &[1, 0, 2, 0, 0, 0, 0, 0, 0, 0]);
    let layered = Workload {
        environment: WorkloadEnvironment::Layered(&environment),
        ..program(&["/bin/env"])
    };
    let layered = encode_exec_payload(&layered).unwrap();
    assert_eq!(&layered[8..10], &6u16.to_le_bytes());
    let default = encode_exec_payload(&program(&["/bin/env"])).unwrap();
    assert_eq!(&default[6..8], &[0, 0]);
}

#[test]
fn exec_payload_enforces_agent_limits() {
    let encode = |argv: Vec<String>, timeout_ms| {
        encode_exec_payload(&Workload {
            argv,
            timeout_ms,
            ..program(&[])
        })
    };
    let argument = |value: &str| vec![value.to_owned()];
    assert!(encode(argument("bin/sh"), 0).is_err());
    assert!(encode(Vec::new(), 0).is_err());
    assert!(encode(argument(&format!("/{}", "a".repeat(4096))), 0).is_err());
    assert!(encode(vec!["/bin/true".to_owned(); 65], 0).is_err());
    assert!(encode(vec!["/bin/echo".to_owned(), String::new()], 0).is_err());
    assert!(encode(vec!["/bin/true".to_owned(); 64], MAX_TIMEOUT_MS).is_ok());
    let unique =
        |count: usize| -> Vec<String> { (0..count).map(|index| format!("KEY{index}=")).collect() };
    let encode_environment = |environment: &[String]| {
        encode_exec_payload(&Workload {
            environment: WorkloadEnvironment::Replaced(environment),
            ..program(&["/bin/env"])
        })
    };
    assert!(encode_environment(&unique(MAX_ENVIRONMENT)).is_ok());
    assert!(encode_environment(&unique(MAX_ENVIRONMENT + 1)).is_err());
    assert!(encode_environment(&["A=1".to_owned(), "A=2".to_owned()]).is_err());
    assert!(encode_environment(&["A=1".to_owned(), "AB=2".to_owned()]).is_ok());
    for entry in ["NOVALUE", "=value", &format!("A={}", "x".repeat(4096))] {
        assert!(encode_environment(&[entry.to_owned()]).is_err());
    }
    // Everything shares the 64 KiB control record.
    let large = |count: usize| -> Vec<String> {
        (0..count)
            .map(|index| format!("KEY{index:02}={}", "x".repeat(4000)))
            .collect()
    };
    assert!(encode_environment(&large(15)).is_ok());
    assert!(encode_environment(&large(17)).is_err());
    // The working directory counts toward the same limit.
    let full = vec![format!("/{}", "a".repeat(MAX_ARGUMENT_BYTES - 1)); 15];
    assert!(encode(full.clone(), 0).is_ok());
    let cwd = format!("/{}", "d".repeat(MAX_CWD_BYTES - 1));
    let with_cwd = Workload {
        argv: full,
        cwd: Some(&cwd),
        ..program(&[])
    };
    assert!(encode_exec_payload(&with_cwd).is_err());
}

/// Encodes an extended exec request with an explicit environment, without the host's checks.
fn extended(argv: &[&str], cwd: Option<&str>, environment: &[&str]) -> Vec<u8> {
    let length = |text: &str| u32::try_from(text.len()).unwrap().to_le_bytes();
    let mut flags = EXEC_ENVIRONMENT_PRESENT;
    if cwd.is_some() {
        flags |= EXEC_CWD_PRESENT;
    }
    let mut payload = 0u32.to_le_bytes().to_vec();
    payload.extend_from_slice(&u16::try_from(argv.len()).unwrap().to_le_bytes());
    payload.extend_from_slice(&EXEC_EXTENDED.to_le_bytes());
    payload.extend_from_slice(&flags.to_le_bytes());
    payload.extend_from_slice(&u16::try_from(environment.len()).unwrap().to_le_bytes());
    payload.extend_from_slice(&length(cwd.unwrap_or_default()));
    for argument in argv {
        payload.extend_from_slice(&length(argument));
        payload.extend_from_slice(argument.as_bytes());
    }
    payload.extend_from_slice(cwd.unwrap_or_default().as_bytes());
    for entry in environment {
        payload.extend_from_slice(&length(entry));
        payload.extend_from_slice(entry.as_bytes());
    }
    payload
}

#[test]
fn exec_requests_are_refused_where_the_guest_agent_refuses_them() {
    let exec = decode_exec_payload(&extended(
        &["/usr/bin/env", "-0"],
        Some("/tmp"),
        &["FOO=bar", "EMPTY="],
    ))
    .unwrap();
    assert_eq!(exec.argv, ["/usr/bin/env", "-0"]);
    assert_eq!(exec.cwd.as_deref(), Some("/tmp"));
    assert_eq!(
        exec.environment,
        Some(vec!["FOO=bar".to_owned(), "EMPTY=".to_owned()])
    );
    assert!(!exec.inherit_default_env);
    for payload in [
        extended(&["/usr/bin/env"], None, &["FOO=b\0r"]),
        extended(&["/usr/bin/env"], None, &["F\0O=bar"]),
        extended(&["/usr/bin/e\0nv"], None, &[]),
        extended(&["/usr/bin/env", "a\0b"], None, &[]),
        extended(&["/usr/bin/env"], Some("/t\0mp"), &[]),
        extended(&["/usr/bin/env"], Some("tmp"), &[]),
        extended(&["/usr/bin/env"], None, &["FOO=1", "FOO=2"]),
        extended(&["/usr/bin/env"], None, &["NOVALUE"]),
        extended(&["/usr/bin/env"], None, &["=value"]),
        extended(&[], None, &[]),
        extended(&["/bin/true"; MAX_ARGUMENTS + 1], None, &[]),
        extended(&["usr/bin/env"], None, &[]),
        // The argument count is refused before the arguments are read.
        {
            let mut payload = encode_exec_payload(&program(&["/bin/true"])).unwrap();
            payload[4..6].copy_from_slice(&65u16.to_le_bytes());
            payload
        },
        {
            let mut payload = encode_exec_payload(&program(&["/x"])).unwrap();
            payload[12] = b'x';
            payload
        },
        // An unknown flag, and the inherit flag without an environment.
        {
            let mut payload = extended(&["/usr/bin/env"], None, &[]);
            payload[8] = 0x08;
            payload
        },
        {
            let mut payload = encode_exec_payload(&program(&["/usr/bin/env"])).unwrap();
            payload.push(0);
            payload
        },
        vec![0; 7],
    ] {
        assert!(decode_exec_payload(&payload).is_err(), "{payload:?}");
    }
    let mut inherit_only = extended(&["/usr/bin/env"], None, &[]);
    inherit_only[8..10].copy_from_slice(&EXEC_INHERIT_DEFAULT_ENV.to_le_bytes());
    assert!(decode_exec_payload(&inherit_only).is_err());
    let most = decode_exec_payload(&extended(&["/bin/true"; MAX_ARGUMENTS], None, &[])).unwrap();
    assert_eq!(most.argv.len(), MAX_ARGUMENTS);
}

#[test]
fn decoders_reject_malformed_input() {
    let mut header = [0u8; OUTER_HEADER_LEN];
    assert!(decode_outer_header(&header).is_err());
    header[..4].copy_from_slice(&OUTER_MAGIC);
    header[4] = 1;
    header[40..44].copy_from_slice(&(OUTER_MAX_PAYLOAD as u32 + 1).to_le_bytes());
    assert!(decode_outer_header(&header).is_err());

    let mut frame = encode_app(APP_STDOUT, 1, 0, b"abc");
    assert_eq!(decode_app(&frame).unwrap().payload, b"abc");
    frame.push(0);
    assert!(decode_app(&frame).is_err());
    assert!(decode_app(&frame[..10]).is_err());
}

#[test]
fn exit_categories_match_the_guest_agent() {
    for category in [
        ExitCategory::Exit,
        ExitCategory::Timeout,
        ExitCategory::OutputLimit,
        ExitCategory::Signal,
        ExitCategory::Failed,
        ExitCategory::Cancelled,
    ] {
        assert_eq!(
            ExitCategory::parse(category.as_str().as_bytes()).unwrap(),
            category
        );
    }
    assert_eq!(ExitCategory::parse(b"exit").unwrap(), ExitCategory::Exit);
    assert_eq!(
        ExitCategory::parse(b"timeout").unwrap(),
        ExitCategory::Timeout
    );
    assert_eq!(
        ExitCategory::parse(b"output-limit").unwrap(),
        ExitCategory::OutputLimit
    );
    assert_eq!(
        ExitCategory::parse(b"signal").unwrap(),
        ExitCategory::Signal
    );
    assert_eq!(
        ExitCategory::parse(b"failed").unwrap(),
        ExitCategory::Failed
    );
    assert_eq!(
        ExitCategory::parse(b"cancelled").unwrap(),
        ExitCategory::Cancelled
    );
    assert!(ExitCategory::parse(b"canceled").is_err());
}

#[test]
fn guest_features_report_what_a_guest_lacks() {
    let required = GuestFeatures::REQUIRED;
    let complete = GuestFeatures::decode(&required.encode()).unwrap();
    assert!(complete.missing(required).is_empty());
    assert!(complete.contains(required));

    // Trailing bytes belong to later revisions.
    let cancel_only = GuestFeatures::decode(&[1, 0, 0, 0, 0xaa]).unwrap();
    assert_eq!(cancel_only, GuestFeatures::CANCEL);
    assert!(!cancel_only.contains(required));
    assert_eq!(
        cancel_only.missing(required),
        [
            "workload accounts",
            "workload containment",
            "per-execution environments",
            "working directories",
            "host path mapping tables",
        ]
    );
    assert!(GuestFeatures::decode(&[1, 0, 0]).is_err());

    // A guest of the previous release announced its host mappings with bit 1, which no
    // longer counts.
    let previous = GuestFeatures::decode(&0b11_1111u32.to_le_bytes()).unwrap();
    assert_eq!(previous.missing(required), ["host path mapping tables"]);

    // Every required feature has a name, so a refusal can say what is missing.
    assert_eq!(
        GuestFeatures::NONE.missing(required).len(),
        required.bits().count_ones() as usize
    );
}

#[test]
fn guest_features_use_distinct_bits() {
    let mut seen = 1u32 << 1;
    for (feature, name) in GuestFeatures::NAMES {
        assert_eq!(feature.bits().count_ones(), 1, "{name}");
        assert_eq!(seen & feature.bits(), 0, "{name} reuses a feature bit");
        seen |= feature.bits();
    }
}

fn map(source: &str, target: &str, read_only: bool) -> HostMap {
    HostMap {
        source: source.to_owned(),
        target: target.to_owned(),
        read_only,
    }
}

#[test]
fn maps_payloads_are_split_to_fit_the_limit() {
    let maps: Vec<HostMap> = (0..10)
        .map(|index| {
            map(
                &format!("0/{index:02}"),
                &format!("/work/{index:02}"),
                index % 2 == 0,
            )
        })
        .collect();
    // Each entry takes 6 + 4 + 8 bytes, so a 64-byte payload holds 3 entries.
    let payloads = encode_maps_payloads(&maps, 64).unwrap();
    assert_eq!(payloads.len(), 4);
    let mut next = 0u32;
    for payload in &payloads {
        assert!(payload.len() <= 64);
        assert_eq!(u32::from_le_bytes(array(&payload[..4])), next);
        let count = u32::from_le_bytes(array(&payload[4..8]));
        let mut offset = MAPS_HEADER_LEN;
        for index in next..next + count {
            let field = |at: usize| usize::from(u16::from_le_bytes(array(&payload[at..at + 2])));
            let flags = field(offset);
            let source_len = field(offset + 2);
            let target_len = field(offset + 4);
            offset += MAP_ENTRY_HEADER_LEN;
            let expected = &maps[index as usize];
            assert_eq!(flags == usize::from(MAP_READ_ONLY), expected.read_only);
            assert_eq!(
                &payload[offset..offset + source_len],
                expected.source.as_bytes()
            );
            offset += source_len;
            assert_eq!(
                &payload[offset..offset + target_len],
                expected.target.as_bytes()
            );
            offset += target_len;
        }
        assert_eq!(offset, payload.len());
        // The guest agent reads back exactly the entries that the host sent.
        let decoded = decode_maps_payload(payload).unwrap();
        assert_eq!(decoded.first, next);
        assert_eq!(
            decoded.maps,
            maps[next as usize..(next + count) as usize].to_vec()
        );
        next += count;
    }
    assert_eq!(next, 10);

    // The largest table fits the control protocol in a few requests.
    let long = format!("/{}", "t".repeat(MAX_MAP_PATH_BYTES - 1));
    let largest: Vec<HostMap> = (0..MAX_HOST_MAPPINGS)
        .map(|index| map(&format!("0/{index}"), &long, false))
        .collect();
    let payloads = encode_maps_payloads(&largest, OUTER_MAX_PAYLOAD - APP_HEADER_LEN).unwrap();
    assert!(
        payloads
            .iter()
            .all(|payload| APP_HEADER_LEN + payload.len() <= OUTER_MAX_PAYLOAD)
    );
    assert_eq!(
        payloads
            .iter()
            .map(|payload| u32::from_le_bytes(array(&payload[4..8])))
            .sum::<u32>() as usize,
        MAX_HOST_MAPPINGS
    );
}

#[test]
fn maps_payloads_enforce_agent_limits() {
    let limit = OUTER_MAX_PAYLOAD - APP_HEADER_LEN;
    let too_long = format!("/{}", "t".repeat(MAX_MAP_PATH_BYTES));
    for invalid in [
        map("/0", "/w", false),
        map("0", "w", false),
        map("0", &too_long, false),
        map(&"s".repeat(MAX_MAP_PATH_BYTES + 1), "/w", false),
        map("0\0", "/w", false),
        map("0", "/w\0", false),
        // The guest agent refuses empty, `.`, and `..` components in either path.
        map(".", "/w", false),
        map("0/.", "/w", false),
        map("0/../1", "/w", false),
        map("0//1", "/w", false),
        map("0/", "/w", false),
        map("0", "/", false),
        map("0", "//w", false),
        map("0", "/w/", false),
        map("0", "/tmp/../etc", false),
        map("0", "/w/./x", false),
        // Only an empty source names the export's root, whose target is as any other's.
        map("/", "/w", false),
        map("", "", false),
        map("", "w", false),
        map("", "/", false),
        map("", "/w/", false),
    ] {
        assert!(
            encode_maps_payloads(std::slice::from_ref(&invalid), limit).is_err(),
            "{invalid:?}"
        );
    }
    // An empty source names the export's root itself.
    let root = map("", "/data", true);
    let payloads = encode_maps_payloads(std::slice::from_ref(&root), limit).unwrap();
    assert_eq!(decode_maps_payload(&payloads[0]).unwrap().maps, [root]);
    // Names that merely start with dots are ordinary components.
    let dotted = map("0/.config", "/home/..data/...", false);
    let payloads = encode_maps_payloads(std::slice::from_ref(&dotted), limit).unwrap();
    assert_eq!(decode_maps_payload(&payloads[0]).unwrap().maps, [dotted]);
    let longest = format!("/{}", "t".repeat(MAX_MAP_PATH_BYTES - 1));
    let source = "s".repeat(MAX_MAP_PATH_BYTES);
    assert!(encode_maps_payloads(&[map(&source, &longest, false)], limit).is_ok());
    assert!(encode_maps_payloads(&[], limit).is_err());
    let too_many = vec![map("0", "/w", true); MAX_HOST_MAPPINGS + 1];
    assert!(encode_maps_payloads(&too_many, limit).is_err());
    // An entry that cannot fit any request is refused rather than split.
    assert!(encode_maps_payloads(&[map("0", "/abcdefghijklmnop", true)], 20).is_err());
}

/// Both `MAPS` decoders refuse each `INVALID_MAPS` vector. The host's also refuses a source that
/// is not UTF-8, which the guest's accepts.
#[test]
fn maps_decoders_refuse_the_invalid_vectors() {
    let names = [
        "TRUNCATED_HEADER",
        "TRUNCATED_ENTRY",
        "MISSING_ENTRY",
        "TOO_MANY_ENTRIES",
        "TRAILING_BYTE",
        "UNKNOWN_FLAG",
        "DOT_SOURCE",
        "DOT_DOT_SOURCE",
        "NUL_SOURCE",
        "NOT_UTF8_THEN_TRUNCATED",
        "RELATIVE_TARGET",
        "ROOT_TARGET",
        "TRAILING_SLASH",
        "DOT_TARGET",
        "NUL_TARGET",
    ];
    for vector in of_kind(NVXC_V1, "INVALID_MAPS", &names) {
        let name = vector.name;
        assert!(decode_maps_payload(&vector.bytes).is_err(), "{name}");
        assert!(decode_guest_maps_payload(&vector.bytes).is_err(), "{name}");
    }
    let mut not_utf8 = vector(NVXC_V1, "MAPS", "ONE_ENTRY").bytes;
    not_utf8[MAPS_HEADER_LEN + MAP_ENTRY_HEADER_LEN] = 0xff;
    assert!(decode_maps_payload(&not_utf8).is_err());
    assert!(decode_guest_maps_payload(&not_utf8).is_ok());
}

// Hosts and guests share the wire constants, so changing one passes every round trip between
// them yet breaks compatibility with released peers. These tests pin each value literally.

#[test]
fn record_definitions_keep_their_wire_values() {
    assert_eq!(OUTER_HEADER_LEN, 44);
    assert_eq!(OUTER_MAX_PAYLOAD, 65_536);
    assert_eq!(OUTER_VERSION, 1);
    assert_eq!(OUTER_GUEST_ATTACH, 1);
    assert_eq!(OUTER_RESET, 3);
    assert_eq!(OUTER_ACK, 4);
    assert_eq!(OUTER_DATA, 5);
    assert_eq!(OUTER_CREDIT, 9);
    assert_eq!(OUTER_RECEIVE_WINDOW, 1024 * 1024);
    assert_eq!(APP_HEADER_LEN, 24);
    assert_eq!(APP_VERSION, 1);
    assert_eq!(APP_PING, 1);
    assert_eq!(APP_EXEC, 2);
    assert_eq!(APP_STOP, 3);
    assert_eq!(APP_CANCEL, 4);
    assert_eq!(APP_READY, 0x81);
    assert_eq!(APP_STDOUT, 0x82);
    assert_eq!(APP_STDERR, 0x83);
    assert_eq!(APP_EXIT, 0x84);
    assert_eq!(APP_STOPPED, 0x85);
    assert_eq!(APP_ERROR, 0xff);
}

#[test]
fn feature_definitions_keep_their_wire_values() {
    assert_eq!(APP_FEATURES, 5);
    assert_eq!(APP_MAPS, 6);
    assert_eq!(GuestFeatures::CANCEL.bits(), 0x01);
    // Bit 1 is retired and stays unused, so hosts that still need it refuse the image.
    assert_eq!(GuestFeatures::WORKLOAD_ACCOUNT.bits(), 0x04);
    assert_eq!(GuestFeatures::EXEC_CGROUP.bits(), 0x08);
    assert_eq!(GuestFeatures::EXEC_ENVIRONMENT.bits(), 0x10);
    assert_eq!(GuestFeatures::EXEC_CWD.bits(), 0x20);
    assert_eq!(GuestFeatures::HOST_MAPPING_TABLE.bits(), 0x40);
}

#[test]
fn host_mapping_table_keeps_its_wire_values() {
    assert_eq!(MAX_HOST_MAPPINGS, 4096);
    assert_eq!(MAX_MAP_PATH_BYTES, 4095);
    assert_eq!(MAPS_HEADER_LEN, 8);
    assert_eq!(MAP_ENTRY_HEADER_LEN, 6);
    assert_eq!(MAP_READ_ONLY, 1);
    assert_eq!(MAPS_TOKEN, "nvx_maps=");
    assert_eq!(MAPPINGS_INCOMPLETE, "mappings-incomplete");
    assert_eq!(MAPPING_FAILED, "mapping-failed");
}

#[test]
fn exec_extension_keeps_its_wire_values() {
    assert_eq!(EXEC_EXTENDED, 1);
    assert_eq!(EXEC_CWD_PRESENT, 1);
    assert_eq!(EXEC_ENVIRONMENT_PRESENT, 2);
    assert_eq!(EXEC_INHERIT_DEFAULT_ENV, 4);
    assert_eq!(MAX_ARGUMENTS, 64);
    assert_eq!(MAX_ARGUMENT_BYTES, 4096);
    assert_eq!(MAX_ENVIRONMENT, 256);
    assert_eq!(MAX_OUTPUT_BYTES, 1024 * 1024);
    assert_eq!(CWD_FAILED, "cwd-failed");
    assert_eq!(LAUNCH_FAILED, "launch-failed");
    assert_eq!(INVALID_REQUEST, "invalid-request");
    assert_eq!(BUSY, "busy");
    assert_eq!(UNSUPPORTED_OPERATION, b"unsupported-operation");
}

#[test]
fn exec_limits_support_linux_paths() {
    // A working directory may be as long as one argument, and Linux cannot
    // enter a directory longer than that argument limit.
    const { assert!(MAX_CWD_BYTES < MAX_ARGUMENT_BYTES) };
}

#[test]
fn guest_window_payloads_enforce_broker_bounds() {
    for window in [
        OUTER_MIN_RECEIVE_WINDOW,
        OUTER_RECEIVE_WINDOW,
        OUTER_MAX_RECEIVE_WINDOW,
    ] {
        let payload = encode_ack_payload(window).unwrap();
        assert_eq!(decode_ack_payload(&payload).unwrap(), window);
    }
    for window in [
        0,
        OUTER_MIN_RECEIVE_WINDOW - 1,
        OUTER_MAX_RECEIVE_WINDOW + 1,
    ] {
        assert!(encode_ack_payload(window).is_err(), "{window}");
        assert!(
            decode_ack_payload(&window.to_le_bytes()).is_err(),
            "{window}"
        );
    }
    for credit in [1, 24, OUTER_MAX_RECEIVE_WINDOW] {
        let payload = encode_credit_payload(credit).unwrap();
        assert_eq!(decode_credit_payload(&payload).unwrap(), credit);
    }
    for credit in [0, OUTER_MAX_RECEIVE_WINDOW + 1] {
        assert!(encode_credit_payload(credit).is_err(), "{credit}");
        assert!(
            decode_credit_payload(&credit.to_le_bytes()).is_err(),
            "{credit}"
        );
    }
    assert!(decode_ack_payload(&[0; 3]).is_err());
    assert!(decode_credit_payload(&[0; 5]).is_err());
}

#[test]
fn host_mapping_count_matches_the_guest_command_line_parser() {
    for (command_line, expected) in [
        (&b""[..], 0),
        (b"quiet nvx_map=0,/legacy,rw", 0),
        (b"quiet\tnvx_maps=1\nlegacy", 1),
        (b"nvx_maps=0001", 1),
        (b"nvx_maps=4096", 4096),
    ] {
        assert_eq!(
            parse_host_mapping_count(command_line).unwrap(),
            expected,
            "{:?}",
            String::from_utf8_lossy(command_line)
        );
    }
    for command_line in [
        &b"nvx_maps="[..],
        b"nvx_maps=0",
        b"nvx_maps=4097",
        b"nvx_maps=99999999999",
        b"nvx_maps=1x",
        b"nvx_maps=1 nvx_maps=1",
    ] {
        assert!(
            parse_host_mapping_count(command_line).is_err(),
            "{:?}",
            String::from_utf8_lossy(command_line)
        );
    }
}

#[test]
fn host_mapping_count_splits_the_command_line_as_the_guest_init_does() {
    // The init splits the command line at spaces, tabs, and newlines alone, without quoting and
    // past a `--`, unlike the kernel, and the count's token is named `nvx_maps`, byte for byte,
    // and has a value.
    for (command_line, expected) in [
        (&b"nvx_maps nvx_maps=2"[..], 2),
        (b"quiet -- nvx_maps=2", 2),
        (b"\"nvx_maps=2\"", 0),
        (b"=nvx_maps=2", 0),
        (b"nvx-maps=2", 0),
        (b"quiet\rnvx_maps=2", 0),
        (b"quiet\x0bnvx_maps=2", 0),
        (b"quiet\xa0nvx_maps=2", 0),
    ] {
        assert_eq!(
            parse_host_mapping_count(command_line).unwrap(),
            expected,
            "{}",
            command_line.escape_ascii()
        );
    }
    // A byte at which the init does not split stays in the count.
    for command_line in [&b"nvx_maps=2\r"[..], b"nvx_maps=2\xa0", b"nvx_maps=\"2\""] {
        assert!(
            parse_host_mapping_count(command_line).is_err(),
            "{}",
            command_line.escape_ascii()
        );
    }
}

#[test]
fn both_agent_modes_announce_the_host_mapping_table() {
    assert_eq!(GuestFeatures::DIRECT_AGENT.bits(), 0x7d);
    // The layered root creates no account, so it announces no workload accounts (bit 2).
    assert_eq!(GuestFeatures::SANDBOX_AGENT.bits(), 0x51);
    assert!(!GuestFeatures::SANDBOX_AGENT.contains(GuestFeatures::WORKLOAD_ACCOUNT));
    assert!(GuestFeatures::DIRECT_AGENT.contains(GuestFeatures::WORKLOAD_ACCOUNT));
    for features in [GuestFeatures::DIRECT_AGENT, GuestFeatures::SANDBOX_AGENT] {
        assert!(features.contains(GuestFeatures::HOST_MAPPING_TABLE));
    }
    assert_eq!(
        GuestFeatures::DIRECT_AGENT.bits() & (1 << 1),
        0,
        "retired bit one stays clear"
    );
}

#[test]
fn layered_sandboxes_need_per_execution_environments_and_mapping_tables() {
    // The CLI's exec sends working directories and environments, and its start sends the
    // mappings of the shares; it never cancels a workload or asks for a workload account
    // (microsoft/nvx#566).
    assert_eq!(GuestFeatures::LAYERED_SANDBOX.bits(), (1 << 4) | (1 << 6));
    assert!(GuestFeatures::SANDBOX_AGENT.contains(GuestFeatures::LAYERED_SANDBOX));
    assert!(
        GuestFeatures::SANDBOX_AGENT
            .missing(GuestFeatures::LAYERED_SANDBOX)
            .is_empty()
    );
    assert_eq!(
        (GuestFeatures::CANCEL | GuestFeatures::WORKLOAD_ACCOUNT)
            .missing(GuestFeatures::LAYERED_SANDBOX),
        ["per-execution environments", "host path mapping tables"]
    );
}

#[test]
fn host_exec_encoding_round_trips_through_the_guest_decoder() {
    let environment = strings(&["FOO=a b", "EMPTY="]);
    let workload = Workload {
        argv: strings(&["/bin/sh", "-c", "printf ok"]),
        timeout_ms: u32::MAX,
        cwd: Some("/work"),
        environment: WorkloadEnvironment::Layered(&environment),
    };
    let payload = encode_exec_payload(&workload).unwrap();
    let decoded = decode_guest_exec_payload(&payload).unwrap();
    assert_eq!(decoded.timeout_ms, u32::MAX);
    assert_eq!(
        decoded.argv,
        [b"/bin/sh".to_vec(), b"-c".to_vec(), b"printf ok".to_vec()]
    );
    assert_eq!(decoded.cwd, Some(b"/work".to_vec()));
    assert_eq!(
        decoded.environment,
        Some(vec![b"FOO=a b".to_vec(), b"EMPTY=".to_vec()])
    );
    assert!(decoded.inherit_default_env);
}

#[test]
fn guest_exec_decoder_accepts_linux_bytes_and_the_c_cwd_boundary() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&1u16.to_le_bytes());
    payload.extend_from_slice(&EXEC_EXTENDED.to_le_bytes());
    payload.extend_from_slice(&(EXEC_CWD_PRESENT | EXEC_ENVIRONMENT_PRESENT).to_le_bytes());
    payload.extend_from_slice(&1u16.to_le_bytes());
    payload.extend_from_slice(&(MAX_GUEST_CWD_BYTES as u32).to_le_bytes());
    payload.extend_from_slice(&2u32.to_le_bytes());
    payload.extend_from_slice(b"/\xff");
    payload.push(b'/');
    payload.extend(std::iter::repeat_n(b'x', MAX_GUEST_CWD_BYTES - 1));
    payload.extend_from_slice(&3u32.to_le_bytes());
    payload.extend_from_slice(b"K=\xff");

    assert!(decode_exec_payload(&payload).is_err());
    let decoded = decode_guest_exec_payload(&payload).unwrap();
    assert_eq!(decoded.argv, [b"/\xff".to_vec()]);
    assert_eq!(decoded.cwd.as_ref().unwrap().len(), MAX_GUEST_CWD_BYTES);
    assert_eq!(decoded.environment, Some(vec![b"K=\xff".to_vec()]));
}

/// Encodes an extended exec request field by field, so that its flags, environment count, and
/// working-directory length may contradict what follows them.
fn raw_extended(
    flags: u16,
    environment_count: u16,
    cwd: &[u8],
    argv: &[&[u8]],
    environment: &[&[u8]],
) -> Vec<u8> {
    let length = |bytes: &[u8]| u32::try_from(bytes.len()).unwrap().to_le_bytes();
    let mut payload = 0u32.to_le_bytes().to_vec();
    payload.extend_from_slice(&u16::try_from(argv.len()).unwrap().to_le_bytes());
    payload.extend_from_slice(&EXEC_EXTENDED.to_le_bytes());
    payload.extend_from_slice(&flags.to_le_bytes());
    payload.extend_from_slice(&environment_count.to_le_bytes());
    payload.extend_from_slice(&length(cwd));
    for argument in argv {
        payload.extend_from_slice(&length(argument));
        payload.extend_from_slice(argument);
    }
    payload.extend_from_slice(cwd);
    for entry in environment {
        payload.extend_from_slice(&length(entry));
        payload.extend_from_slice(entry);
    }
    payload
}

#[test]
fn guest_exec_decoder_accepts_what_the_c_agent_accepted() {
    fn with_environment(flags: u16, entries: &[&[u8]]) -> GuestExecPayload {
        let count = u16::try_from(entries.len()).unwrap();
        decode_guest_exec_payload(&raw_extended(flags, count, b"", &[b"/bin/true"], entries))
            .unwrap()
    }

    let empty = with_environment(EXEC_ENVIRONMENT_PRESENT, &[]);
    assert_eq!(empty.environment, Some(Vec::new()));
    assert!(!empty.inherit_default_env);
    let exact = with_environment(
        EXEC_ENVIRONMENT_PRESENT,
        &[b"EMPTY=", b"VALUE=space = value"],
    );
    assert_eq!(
        exact.environment,
        Some(vec![b"EMPTY=".to_vec(), b"VALUE=space = value".to_vec()])
    );
    assert!(!exact.inherit_default_env);
    let layered = with_environment(
        EXEC_ENVIRONMENT_PRESENT | EXEC_INHERIT_DEFAULT_ENV,
        &[b"VALUE=layered"],
    );
    assert_eq!(layered.environment, Some(vec![b"VALUE=layered".to_vec()]));
    assert!(layered.inherit_default_env);

    for timeout_ms in [0, 3_600_001, 86_400_000, u32::MAX] {
        let mut payload = raw_extended(EXEC_ENVIRONMENT_PRESENT, 0, b"", &[b"/bin/true"], &[]);
        payload[..4].copy_from_slice(&timeout_ms.to_le_bytes());
        assert_eq!(
            decode_guest_exec_payload(&payload).unwrap().timeout_ms,
            timeout_ms
        );
    }
}

#[test]
fn guest_exec_decoder_refuses_what_the_c_agent_refused() {
    // The agent decodes with its own byte-oriented decoder, so each refusal that the host
    // decoder's table pins is pinned here too, with the cases that the C agent refused.
    const PROGRAM: &[u8] = b"/bin/true";
    fn with_environment(entries: &[&[u8]]) -> Vec<u8> {
        let count = u16::try_from(entries.len()).unwrap();
        raw_extended(EXEC_ENVIRONMENT_PRESENT, count, b"", &[PROGRAM], entries)
    }
    fn with_cwd(flags: u16, environment_count: u16, cwd: &[u8]) -> Vec<u8> {
        raw_extended(flags, environment_count, cwd, &[PROGRAM], &[])
    }

    let mut unknown_extension = 0u32.to_le_bytes().to_vec();
    unknown_extension.extend_from_slice(&1u16.to_le_bytes());
    unknown_extension.extend_from_slice(&2u16.to_le_bytes());
    unknown_extension.extend_from_slice(&9u32.to_le_bytes());
    unknown_extension.extend_from_slice(PROGRAM);
    let mut truncated_header = 0u32.to_le_bytes().to_vec();
    truncated_header.extend_from_slice(&1u16.to_le_bytes());
    truncated_header.extend_from_slice(&EXEC_EXTENDED.to_le_bytes());
    truncated_header.extend_from_slice(&EXEC_CWD_PRESENT.to_le_bytes());
    let longest_cwd = [b"/".as_slice(), &[b'd'; MAX_GUEST_CWD_BYTES]].concat();
    let mut truncated_cwd = with_cwd(EXEC_CWD_PRESENT, 0, b"/work");
    truncated_cwd.pop();
    let mut trailing = with_cwd(EXEC_CWD_PRESENT, 0, b"/work");
    trailing.push(b'x');

    for (case, payload) in [
        (
            "repeated name",
            with_environment(&[b"VALUE=one", b"VALUE=two"]),
        ),
        ("entry without a value", with_environment(&[b"NO_EQUALS"])),
        ("entry without a name", with_environment(&[b"=empty-key"])),
        ("NUL in an entry", with_environment(&[b"KEY=embedded\0nul"])),
        ("unknown extension", unknown_extension),
        ("truncated extended header", truncated_header),
        (
            "unknown flag",
            with_cwd(EXEC_CWD_PRESENT | 0x08, 0, b"/work"),
        ),
        // Only the agent tells its launch helper that it entered the directory.
        (
            "launch-helper flag",
            with_cwd(EXEC_CWD_PRESENT | 0x8000, 0, b"/work"),
        ),
        (
            "layering without an environment",
            with_cwd(EXEC_CWD_PRESENT | EXEC_INHERIT_DEFAULT_ENV, 0, b"/work"),
        ),
        (
            "entries without the environment flag",
            with_cwd(EXEC_CWD_PRESENT, 1, b"/work"),
        ),
        (
            "flag without a directory",
            with_cwd(EXEC_CWD_PRESENT, 0, b""),
        ),
        ("directory without the flag", with_cwd(0, 0, b"/work")),
        (
            "directory over the limit",
            with_cwd(EXEC_CWD_PRESENT, 0, &longest_cwd),
        ),
        ("relative directory", with_cwd(EXEC_CWD_PRESENT, 0, b"work")),
        (
            "NUL in the directory",
            with_cwd(EXEC_CWD_PRESENT, 0, b"/wo\0rk"),
        ),
        ("truncated directory", truncated_cwd),
        ("trailing bytes", trailing),
        ("no arguments", raw_extended(0, 0, b"", &[], &[])),
        (
            "relative program",
            raw_extended(0, 0, b"", &[b"bin/true"], &[]),
        ),
    ] {
        assert!(decode_guest_exec_payload(&payload).is_err(), "{case}");
    }
}

/// Both `EXEC` decoders refuse an argument, a working directory, or an environment entry with the
/// same message, except where they differ by design: the host's requires each string to be UTF-8
/// as soon as it reads it, and they word the refusals of an entry's name differently, the host's
/// naming the variable that repeats.
#[test]
fn exec_decoders_refuse_strings_with_the_same_messages() {
    const PROGRAM: &[u8] = b"/bin/true";
    fn arguments(argv: &[&[u8]]) -> Vec<u8> {
        raw_extended(0, 0, b"", argv, &[])
    }
    fn with_cwd(cwd: &[u8]) -> Vec<u8> {
        raw_extended(EXEC_CWD_PRESENT, 0, cwd, &[PROGRAM], &[])
    }
    fn with_environment(entries: &[&[u8]]) -> Vec<u8> {
        let count = u16::try_from(entries.len()).unwrap();
        raw_extended(EXEC_ENVIRONMENT_PRESENT, count, b"", &[PROGRAM], entries)
    }
    fn truncated(mut payload: Vec<u8>) -> Vec<u8> {
        payload.pop();
        payload
    }

    let truncated_message = "the exec request is truncated";
    let long = vec![b'a'; MAX_ARGUMENT_BYTES + 1];
    let mut trailing = with_cwd(b"/work");
    trailing.push(b'x');
    for (case, payload, expected) in [
        (
            "NUL in an argument",
            arguments(&[PROGRAM, b"a\0b"]),
            "exec arguments must not contain NUL characters",
        ),
        (
            "empty argument",
            arguments(&[PROGRAM, b""]),
            "exec arguments must be 1 to 4096 bytes",
        ),
        (
            "argument over the limit",
            arguments(&[PROGRAM, &long]),
            "exec arguments must be 1 to 4096 bytes",
        ),
        (
            "truncated argument",
            truncated(arguments(&[PROGRAM])),
            truncated_message,
        ),
        (
            "relative program",
            arguments(&[b"bin/true"]),
            "exec program must be an absolute guest path",
        ),
        (
            "NUL in the directory",
            with_cwd(b"/wo\0rk"),
            "exec working directory must not contain NUL characters",
        ),
        (
            "truncated directory",
            truncated(with_cwd(b"/work")),
            truncated_message,
        ),
        (
            "relative directory",
            with_cwd(b"work"),
            "exec working directory must be an absolute guest path",
        ),
        (
            "NUL in an entry",
            with_environment(&[b"KEY=embedded\0nul"]),
            "exec environment entries must not contain NUL characters",
        ),
        (
            "empty entry",
            with_environment(&[b""]),
            "exec environment entries must be 1 to 4096 bytes",
        ),
        (
            "truncated entry",
            truncated(with_environment(&[b"KEY=value"])),
            truncated_message,
        ),
        (
            "trailing bytes",
            trailing,
            "the exec request has trailing bytes",
        ),
    ] {
        assert_eq!(
            decode_exec_payload(&payload).unwrap_err().message(),
            expected,
            "{case}"
        );
        assert_eq!(
            decode_guest_exec_payload(&payload).unwrap_err().message(),
            expected,
            "{case}"
        );
    }

    let unnamed = "exec environment entries must be KEY=VALUE strings";
    let unnamed_bytes = "exec environment entries must be KEY=VALUE byte strings";
    for (case, payload, host, guest) in [
        (
            "entry without a value",
            with_environment(&[b"NO_EQUALS"]),
            unnamed,
            unnamed_bytes,
        ),
        (
            "entry without a name",
            with_environment(&[b"=empty-key"]),
            unnamed,
            unnamed_bytes,
        ),
        (
            "repeated name",
            with_environment(&[b"VALUE=one", b"VALUE=two"]),
            "exec environment names must be unique, but \"VALUE\" repeats",
            "exec environment names must be unique",
        ),
        (
            "argument that is not UTF-8, then a truncated one",
            truncated(arguments(&[PROGRAM, b"\xff", b"x"])),
            "exec arguments must be valid UTF-8",
            truncated_message,
        ),
        (
            "relative program that is not UTF-8",
            arguments(&[b"\xff"]),
            "exec arguments must be valid UTF-8",
            "exec program must be an absolute guest path",
        ),
        (
            "relative directory that is not UTF-8",
            with_cwd(b"\xff"),
            "exec working directory must be valid UTF-8",
            "exec working directory must be an absolute guest path",
        ),
        (
            "entry without a value that is not UTF-8",
            with_environment(&[b"\xff"]),
            "exec environment entries must be valid UTF-8",
            unnamed_bytes,
        ),
    ] {
        assert_eq!(
            decode_exec_payload(&payload).unwrap_err().message(),
            host,
            "{case}"
        );
        assert_eq!(
            decode_guest_exec_payload(&payload).unwrap_err().message(),
            guest,
            "{case}"
        );
    }
}

#[test]
fn host_maps_encoding_round_trips_through_the_guest_decoder() {
    let maps = [
        map("", "/data", true),
        map("0/src", "/work", true),
        map("1/file", "/etc/value", false),
    ];
    let payloads = encode_maps_payloads(&maps, OUTER_MAX_PAYLOAD - APP_HEADER_LEN).unwrap();
    let decoded = decode_guest_maps_payload(&payloads[0]).unwrap();
    assert_eq!(decoded.first, 0);
    assert_eq!(
        decoded.maps,
        [
            GuestHostMap {
                source: Vec::new(),
                target: b"/data".to_vec(),
                read_only: true,
            },
            GuestHostMap {
                source: b"0/src".to_vec(),
                target: b"/work".to_vec(),
                read_only: true,
            },
            GuestHostMap {
                source: b"1/file".to_vec(),
                target: b"/etc/value".to_vec(),
                read_only: false,
            },
        ]
    );
}

#[test]
fn guest_maps_decoder_accepts_non_utf8_linux_paths() {
    let mut payload = encode_maps_payloads(
        &[map("0/x", "/work", false)],
        OUTER_MAX_PAYLOAD - APP_HEADER_LEN,
    )
    .unwrap()
    .remove(0);
    let source_offset = MAPS_HEADER_LEN + MAP_ENTRY_HEADER_LEN;
    payload[source_offset + 2] = 0xff;
    assert!(decode_maps_payload(&payload).is_err());
    let decoded = decode_guest_maps_payload(&payload).unwrap();
    assert_eq!(decoded.maps[0].source, b"0/\xff");
}

/// Both `MAPS` decoders refuse an entry with the same message, except that the host's requires
/// each path to be UTF-8 as soon as it reads it, and the guest's bounds the number of entries
/// before it reads any.
#[test]
fn maps_decoders_refuse_entries_with_the_same_messages() {
    let invalid = |name: &str| vector(NVXC_V1, "INVALID_MAPS", name).bytes;
    let truncated = "the host mapping request is truncated";
    let refused = "a host mapping has unknown flags or a source or target that the guest agent \
                   refuses";
    for (name, expected) in [
        ("TRUNCATED_HEADER", truncated),
        ("TRUNCATED_ENTRY", truncated),
        (
            "TRAILING_BYTE",
            "the host mapping request has trailing bytes",
        ),
        ("UNKNOWN_FLAG", refused),
        ("RELATIVE_TARGET", refused),
    ] {
        let payload = invalid(name);
        assert_eq!(
            decode_maps_payload(&payload).unwrap_err().message(),
            expected,
            "{name}"
        );
        assert_eq!(
            decode_guest_maps_payload(&payload).unwrap_err().message(),
            expected,
            "{name}"
        );
    }
    let not_utf8_then_truncated = invalid("NOT_UTF8_THEN_TRUNCATED");
    assert_eq!(
        decode_maps_payload(&not_utf8_then_truncated)
            .unwrap_err()
            .message(),
        "a host mapping is not valid UTF-8"
    );
    assert_eq!(
        decode_guest_maps_payload(&not_utf8_then_truncated)
            .unwrap_err()
            .message(),
        truncated
    );
    assert_eq!(
        decode_guest_maps_payload(&invalid("TOO_MANY_ENTRIES"))
            .unwrap_err()
            .message(),
        "the host mapping request has too many entries"
    );
}

/// Returns bytes near the vectors of `kinds`: the bytes of one of them, with up to four bytes
/// overwritten, perhaps truncated, and with up to eight bytes appended.
fn near_vectors(kinds: &'static [&'static str]) -> impl Strategy<Value = Vec<u8>> {
    let encodings: Vec<_> = vectors(NVXC_V1)
        .into_iter()
        .filter(|vector| kinds.contains(&vector.kind))
        .map(|vector| vector.bytes)
        .collect();
    (
        prop::sample::select(encodings),
        prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..4),
        prop::option::of(any::<prop::sample::Index>()),
        prop::collection::vec(any::<u8>(), 0..8),
    )
        .prop_map(|(mut bytes, writes, cut, tail)| {
            for (index, byte) in writes {
                if !bytes.is_empty() {
                    let index = index.index(bytes.len());
                    bytes[index] = byte;
                }
            }
            if let Some(cut) = cut {
                bytes.truncate(cut.index(bytes.len() + 1));
            }
            bytes.extend(tail);
            bytes
        })
}

proptest! {
    /// The host's and the guest's `EXEC` and `MAPS` decoders return, never panic, on arbitrary
    /// bytes and on bytes near their vectors, which reach past the checks of the headers.
    #[test]
    fn decoders_never_panic(
        payload in prop_oneof![
            prop::collection::vec(any::<u8>(), 0..256),
            near_vectors(&["EXEC", "MAPS", "INVALID_MAPS"]),
        ]
    ) {
        let _ = decode_exec_payload(&payload);
        let _ = decode_guest_exec_payload(&payload);
        let _ = decode_maps_payload(&payload);
        let _ = decode_guest_maps_payload(&payload);
    }

    /// Whatever bytes the host's `EXEC` decoder accepts, the guest's accepts with the same
    /// strings, whereas the guest's also accepts strings that are not UTF-8.
    #[test]
    fn the_guest_decodes_every_exec_payload_that_the_host_decodes(
        payload in near_vectors(&["EXEC"])
    ) {
        if let Ok(host) = decode_exec_payload(&payload) {
            let guest = decode_guest_exec_payload(&payload).unwrap();
            let bytes = |strings: &[String]| -> Vec<Vec<u8>> {
                strings.iter().map(|string| string.as_bytes().to_vec()).collect()
            };
            prop_assert_eq!(guest.timeout_ms, host.timeout_ms);
            prop_assert_eq!(guest.argv, bytes(&host.argv));
            prop_assert_eq!(guest.cwd, host.cwd.map(String::into_bytes));
            prop_assert_eq!(guest.environment, host.environment.as_deref().map(bytes));
        }
    }
}
