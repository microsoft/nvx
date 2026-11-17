//! Records against OpenVMM's vectors and cases and every payload rule, the sequence numbers that a
//! direction's records take, and the parser across partial reads and record boundaries.

use std::collections::HashSet;

use proptest::prelude::*;

use super::*;
use crate::protocol::{decode_outer_header, encode_outer};
use crate::tests::OPENVMM_CASES;

/// Returns the type of the record of OpenVMM's vector named `name`.
fn openvmm_record_type(name: &str) -> RecordType {
    match name {
        "GUEST_ATTACH" => RecordType::GuestAttach,
        "HOST_ATTACH" => RecordType::HostAttach,
        "RESET" => RecordType::Reset,
        "ACK_INITIAL_CREDIT" => RecordType::Ack,
        "CREDIT" => RecordType::Credit,
        "DATA_EMBEDDED_MAGIC" => RecordType::Data,
        "WAIT" => RecordType::Wait,
        "READY" => RecordType::Ready,
        "ERROR_AUTHENTICATION" => RecordType::Error,
        _ => panic!("OpenVMM's vectors name the unknown record {name}"),
    }
}

/// OpenVMM's vectors hold one record of each type, with the instance ID 00 through 0f, epoch 7,
/// and sequence 9 after the bootstrap. Each decodes to its record, which encodes to its bytes,
/// with the NVXS session's codec and with the outer codec alike.
#[test]
fn records_match_openvmms_vectors() {
    let instance: [u8; 16] = core::array::from_fn(|index| index as u8);
    let mut types = HashSet::new();
    for (name, encoded) in openvmm_vectors() {
        let record_type = openvmm_record_type(name);
        assert!(types.insert(record_type), "{name} repeats {record_type}");
        let parsed = parse(&encoded).unwrap();
        assert_eq!(parsed.record_type(), record_type, "{name}");
        assert_eq!(RecordType::from_code(record_type.code()), Some(record_type));
        let identity = (parsed.instance_id(), parsed.epoch(), parsed.sequence());
        if record_type.is_bootstrap() {
            assert_eq!(identity, ([0; 16], 0, 0), "{name}");
        } else {
            assert_eq!(identity, (instance, 7, 9), "{name}");
        }
        match record_type {
            RecordType::HostAttach => {
                let capability: [u8; CAPABILITY_LEN] = core::array::from_fn(|i| i as u8);
                assert_eq!(parsed.payload(), capability);
            }
            RecordType::Ack => assert_eq!(parsed.window(), Some(OUTER_MAX_RECEIVE_WINDOW)),
            RecordType::Data => assert_eq!(parsed.payload(), b"NVXSNVXS"),
            RecordType::Error => assert_eq!(parsed.error_code(), Some(ErrorCode::Authentication)),
            RecordType::Credit => assert_eq!(parsed.credit(), Some(OUTER_MIN_RECEIVE_WINDOW)),
            _ => assert!(parsed.payload().is_empty()),
        }
        let mut output = Vec::new();
        encode_record(
            record_type,
            &parsed.instance_id(),
            parsed.epoch(),
            parsed.sequence(),
            parsed.payload(),
            &mut output,
        )
        .unwrap();
        assert_eq!(output, encoded, "{name}");
        // The outer codec, which the fake OpenVMM uses, agrees.
        let outer = encode_outer(
            record_type.code(),
            &parsed.instance_id(),
            parsed.epoch(),
            parsed.sequence(),
            parsed.payload(),
        )
        .unwrap();
        assert_eq!(outer, encoded, "{name}");
        let header = decode_outer_header(&encoded[..OUTER_HEADER_LEN].try_into().unwrap()).unwrap();
        assert_eq!(
            (
                header.record_type,
                header.instance_id,
                header.epoch,
                header.sequence,
                header.length
            ),
            (
                record_type.code(),
                parsed.instance_id(),
                parsed.epoch(),
                parsed.sequence(),
                parsed.payload().len()
            ),
            "{name}"
        );
    }
    assert_eq!(types.len(), 9);
    assert_eq!(RecordType::from_code(0), None);
    assert_eq!(RecordType::from_code(10), None);
}

/// Checks the case `name` of a record of `record_type` with `payload`: unless the name starts with
/// `INVALID_`, both the encoder and the parser accept the record, and otherwise both refuse it with
/// `error`.
fn check_case(
    name: &str,
    record_type: RecordType,
    identity: Identity,
    payload: &[u8],
    error: RecordError,
) {
    let mut output = Vec::new();
    let encoded = encode_record(
        record_type,
        &identity.instance_id,
        identity.epoch,
        9,
        payload,
        &mut output,
    );
    let mut stream = raw_header(record_type.code(), identity, 9, payload.len() as u32);
    stream.extend_from_slice(payload);
    let parsed = RecordParser::new().push(&stream);
    if name.starts_with("INVALID_") {
        assert_eq!(encoded, Err(error.clone()), "{name}");
        assert_eq!(parsed.map(|_| ()), Err(error), "{name}");
    } else {
        assert!(name.starts_with("VALID_"), "{name}");
        assert_eq!(encoded, Ok(()), "{name}");
        assert_eq!(output, stream, "{name}");
        let (consumed, record) = parsed.unwrap();
        assert_eq!(consumed, stream.len(), "{name}");
        assert_eq!(record.unwrap().payload(), payload, "{name}");
    }
}

/// Each of OpenVMM's 37 cases is accepted or refused as it says: the payload lengths of a DATA
/// record, the window of an ACK and the bytes of a CREDIT at their bounds, malformed headers, the
/// code of an ERROR, and malformed records.
#[test]
fn records_follow_openvmms_cases() {
    let identity = Identity {
        instance_id: core::array::from_fn(|index| index as u8),
        epoch: 7,
    };
    let mut cases = 0;
    for line in OPENVMM_CASES.lines().filter(|line| !line.is_empty()) {
        let fields: Vec<_> = line.split('|').collect();
        match fields[..] {
            ["DATA_LENGTH", name, length] => {
                let length = length.parse().unwrap();
                let error = RecordError::Length {
                    record_type: RecordType::Data,
                    length,
                };
                check_case(name, RecordType::Data, identity, &vec![0x5a; length], error);
            }
            ["ACK_CREDIT", name, window] => {
                let window: u32 = window.parse().unwrap();
                let error = RecordError::ReceiveWindow(window);
                check_case(
                    name,
                    RecordType::Ack,
                    identity,
                    &window.to_le_bytes(),
                    error,
                );
            }
            ["CREDIT_INCREMENT", name, credit] => {
                let credit: u32 = credit.parse().unwrap();
                let error = RecordError::Credit(credit);
                check_case(
                    name,
                    RecordType::Credit,
                    identity,
                    &credit.to_le_bytes(),
                    error,
                );
            }
            ["INVALID_HEADER", name, header] => {
                let header = hex(header);
                assert_eq!(header.len(), OUTER_HEADER_LEN, "{name}");
                let expected = match name {
                    "INVALID_MAGIC" => RecordError::Magic(*b"XVXS"),
                    "INVALID_VERSION" => RecordError::Version(2),
                    "INVALID_TYPE" => RecordError::Type(99),
                    "INVALID_FLAGS" => RecordError::Flags(1),
                    "INVALID_DATA_LENGTH" => RecordError::Length {
                        record_type: RecordType::Data,
                        length: 65_537,
                    },
                    _ => panic!("OpenVMM's cases hold the unknown header case {name}"),
                };
                // The parser refuses the header before any payload byte, without a payload buffer.
                let mut parser = RecordParser::new();
                assert_eq!(parser.push(&header), Err(expected), "{name}");
                assert_eq!(parser.payload_capacity(), 0, "{name}");
            }
            ["ERROR_CODE", "AUTHENTICATION", code] => {
                let code = code.parse().unwrap();
                assert_eq!(ErrorCode::from_code(code), Some(ErrorCode::Authentication));
                assert_eq!(ErrorCode::Authentication.code(), code);
            }
            ["INVALID_RECORD", name, error, record] => {
                let refusal = RecordParser::new().push(&hex(record)).unwrap_err();
                let intended = match error {
                    "INVALID_PAYLOAD_LENGTH" => matches!(refusal, RecordError::Length { .. }),
                    "INVALID_BOOTSTRAP_IDENTITY" => {
                        matches!(refusal, RecordError::BootstrapIdentity(_))
                    }
                    "INVALID_ERROR_CODE" => matches!(refusal, RecordError::ErrorCode(_)),
                    _ => panic!("OpenVMM's cases name the unknown error {error}"),
                };
                assert!(intended, "{name} is refused with {refusal:?}");
            }
            _ => panic!("OpenVMM's cases hold the unknown case {line:?}"),
        }
        cases += 1;
    }
    assert_eq!(cases, 37);
}

/// A valid payload of `length` bytes for `record_type`: the smallest window of an ACK, one byte of
/// a CREDIT, the authentication code of an ERROR, and zeros otherwise.
fn valid_payload(record_type: RecordType, length: usize) -> Vec<u8> {
    match record_type {
        RecordType::Ack => OUTER_MIN_RECEIVE_WINDOW.to_le_bytes().to_vec(),
        RecordType::Credit => 1u32.to_le_bytes().to_vec(),
        RecordType::Error => ErrorCode::Authentication.code().to_le_bytes().to_vec(),
        _ => vec![0; length],
    }
}

#[test]
fn each_record_type_allows_only_its_payload_lengths() {
    let cases: [(RecordType, &[usize], &[usize]); 9] = [
        (RecordType::GuestAttach, &[0], &[1, 4, CAPABILITY_LEN]),
        (
            RecordType::HostAttach,
            &[CAPABILITY_LEN],
            &[0, 4, CAPABILITY_LEN - 1, CAPABILITY_LEN + 1],
        ),
        (RecordType::Reset, &[0], &[1, 4]),
        (RecordType::Ack, &[4], &[0, 3, 5]),
        (
            RecordType::Data,
            &[1, 2, OUTER_MAX_PAYLOAD - 1, OUTER_MAX_PAYLOAD],
            &[0, OUTER_MAX_PAYLOAD + 1],
        ),
        (RecordType::Wait, &[0], &[1, 4]),
        (RecordType::Ready, &[0], &[1, 4]),
        (RecordType::Error, &[4], &[0, 3, 5]),
        (RecordType::Credit, &[4], &[0, 3, 5]),
    ];
    for (record_type, valid, invalid) in cases {
        let identity = if record_type.is_bootstrap() {
            NONE
        } else {
            IDENTITY
        };
        for &length in valid {
            assert!(record_type.allows_length(length), "{record_type} {length}");
            let payload = valid_payload(record_type, length);
            let parsed = parse(&record(record_type, identity, 0, &payload)).unwrap();
            assert_eq!(parsed.record_type(), record_type);
            assert_eq!(parsed.payload(), payload);
        }
        for &length in invalid {
            assert!(!record_type.allows_length(length), "{record_type} {length}");
            let expected = RecordError::Length {
                record_type,
                length,
            };
            let mut output = vec![0xee];
            let encoded = encode_record(
                record_type,
                &identity.instance_id,
                identity.epoch,
                0,
                &vec![1; length],
                &mut output,
            );
            assert_eq!(encoded, Err(expected.clone()));
            assert_eq!(output, [0xee], "a failed encoding leaves the output alone");
            // The parser refuses the header before any payload byte arrives, and allocates no
            // payload buffer.
            let header = raw_header(record_type.code(), identity, 0, length as u32);
            let mut parser = RecordParser::new();
            let last = OUTER_HEADER_LEN - 1;
            assert_eq!(parser.push(&header[..last]), Ok((last, None)));
            assert_eq!(parser.push(&header[last..]), Err(expected));
            assert_eq!((parser.buffered(), parser.payload_capacity()), (0, 0));
        }
    }
    // So is a DATA header that declares the largest length that the field holds.
    let mut parser = RecordParser::new();
    let header = raw_header(RecordType::Data.code(), IDENTITY, 0, u32::MAX);
    let expected = RecordError::Length {
        record_type: RecordType::Data,
        length: u32::MAX as usize,
    };
    assert_eq!(parser.push(&header), Err(expected));
    assert_eq!(parser.payload_capacity(), 0);
}

#[test]
fn headers_are_checked_before_any_payload_byte() {
    let valid = record(RecordType::Data, IDENTITY, 0, b"x");
    let cases = [
        (0, b'X', RecordError::Magic(*b"XVXS")),
        (3, 0, RecordError::Magic(*b"NVX\0")),
        (4, 2, RecordError::Version(2)),
        (5, 1, RecordError::Version(0x0101)),
        (6, 0, RecordError::Type(0)),
        (6, 10, RecordError::Type(10)),
        (6, 0xff, RecordError::Type(0xff)),
        (7, 1, RecordError::Flags(1)),
        (7, 0x80, RecordError::Flags(0x80)),
    ];
    for (offset, value, expected) in cases {
        let mut malformed = valid.clone();
        malformed[offset] = value;
        let mut parser = RecordParser::new();
        assert_eq!(parser.push(&malformed[..OUTER_HEADER_LEN]), Err(expected));
        assert_eq!(parser.payload_capacity(), 0);
    }
}

#[test]
fn bootstrap_records_carry_no_identity() {
    let mut last_byte = [0; 16];
    last_byte[15] = 1;
    let identities = [
        ([1; 16], 0, 0),
        (last_byte, 0, 0),
        ([0; 16], 1, 0),
        ([0; 16], 0, 1),
        ([0; 16], 0, u64::MAX),
    ];
    for record_type in [RecordType::GuestAttach, RecordType::HostAttach] {
        let payload = if record_type == RecordType::HostAttach {
            vec![1; CAPABILITY_LEN]
        } else {
            Vec::new()
        };
        let expected = RecordError::BootstrapIdentity(record_type);
        for (instance_id, epoch, sequence) in identities {
            let mut output = Vec::new();
            let encoded = encode_record(
                record_type,
                &instance_id,
                epoch,
                sequence,
                &payload,
                &mut output,
            );
            assert_eq!(encoded, Err(expected.clone()));
            assert!(output.is_empty());
            let identity = Identity { instance_id, epoch };
            let mut stream =
                raw_header(record_type.code(), identity, sequence, payload.len() as u32);
            stream.extend_from_slice(&payload);
            assert_eq!(RecordParser::new().push(&stream), Err(expected.clone()));
        }
    }
}

#[test]
fn error_records_carry_the_authentication_code() {
    let parsed = parse(&record(RecordType::Error, IDENTITY, 0, &1u32.to_le_bytes())).unwrap();
    assert_eq!(parsed.error_code(), Some(ErrorCode::Authentication));
    assert_eq!(ErrorCode::from_code(1), Some(ErrorCode::Authentication));
    assert_eq!(ErrorCode::Authentication.code(), 1);
    for code in [0, 2, 3, 4, 5, 6, u32::MAX] {
        assert_eq!(ErrorCode::from_code(code), None);
        let mut output = Vec::new();
        let encoded = encode_record(
            RecordType::Error,
            &INSTANCE,
            1,
            0,
            &code.to_le_bytes(),
            &mut output,
        );
        assert_eq!(encoded, Err(RecordError::ErrorCode(code)));
        let mut stream = raw_header(RecordType::Error.code(), IDENTITY, 0, 4);
        stream.extend_from_slice(&code.to_le_bytes());
        let expected = Err(RecordError::ErrorCode(code));
        assert_eq!(RecordParser::new().push(&stream), expected);
    }
    // Only an ERROR carries a code, only an ACK a window, and only a CREDIT a byte count.
    let ack = parse(&sample(RecordType::Ack, IDENTITY, 0)).unwrap();
    assert_eq!((ack.error_code(), ack.credit()), (None, None));
    let credit = parse(&sample(RecordType::Credit, IDENTITY, 0)).unwrap();
    assert_eq!((credit.error_code(), credit.window()), (None, None));
    assert_eq!(parsed.window(), None);
}

#[test]
fn acks_and_credits_stay_within_the_window_bounds() {
    let cases = [
        (RecordType::Ack, 0, false),
        (RecordType::Ack, OUTER_MIN_RECEIVE_WINDOW - 1, false),
        (RecordType::Ack, OUTER_MIN_RECEIVE_WINDOW, true),
        (RecordType::Ack, OUTER_MAX_RECEIVE_WINDOW, true),
        (RecordType::Ack, OUTER_MAX_RECEIVE_WINDOW + 1, false),
        (RecordType::Credit, 0, false),
        (RecordType::Credit, 1, true),
        (RecordType::Credit, OUTER_MAX_RECEIVE_WINDOW, true),
        (RecordType::Credit, OUTER_MAX_RECEIVE_WINDOW + 1, false),
        (RecordType::Credit, u32::MAX, false),
    ];
    for (record_type, value, valid) in cases {
        let error = if record_type == RecordType::Ack {
            RecordError::ReceiveWindow(value)
        } else {
            RecordError::Credit(value)
        };
        let mut output = Vec::new();
        let encoded = encode_record(
            record_type,
            &INSTANCE,
            1,
            0,
            &value.to_le_bytes(),
            &mut output,
        );
        let mut stream = raw_header(record_type.code(), IDENTITY, 0, 4);
        stream.extend_from_slice(&value.to_le_bytes());
        let parsed = RecordParser::new().push(&stream);
        if valid {
            assert_eq!(encoded, Ok(()));
            let parsed = parsed.unwrap().1.unwrap();
            assert_eq!(parsed.window().or(parsed.credit()), Some(value));
        } else {
            assert_eq!(encoded, Err(error.clone()));
            assert_eq!(parsed, Err(error));
        }
    }
}

#[test]
fn a_direction_advances_its_sequence_only_past_the_records_that_it_encodes() {
    let mut output = Vec::new();
    let mut sequence = 7;
    let taken = encode_data(&IDENTITY, &mut sequence, b"xy", &mut output);
    assert_eq!(taken, Ok(2));
    assert_eq!((records(&output)[0].sequence(), sequence), (7, 8));
    // A malformed record, or one that would take the last sequence number, takes none.
    let malformed = encode_data(&IDENTITY, &mut sequence, &[], &mut output);
    let length = RecordError::Length {
        record_type: RecordType::Data,
        length: 0,
    };
    assert_eq!(malformed, Err(SessionError::Record(length)));
    let mut last = u64::MAX;
    let overflowed = encode_data(&IDENTITY, &mut last, b"x", &mut output);
    let overflow = SessionError::SequenceOverflow(RecordType::Data);
    assert_eq!(overflowed, Err(overflow));
    assert_eq!((sequence, last, records(&output).len()), (8, u64::MAX, 1));
}

#[test]
fn the_parser_returns_the_same_records_for_every_split_of_a_stream() {
    let stream = host_stream();
    let expected = records(&stream);
    assert_eq!(expected.len(), 6);
    for cut in 0..=stream.len() {
        let mut parser = RecordParser::new();
        let parsed = feed(
            |piece| parser.push(piece).map_err(SessionError::from),
            &stream,
            &[cut],
        )
        .unwrap();
        assert_eq!(parsed, expected, "split at {cut}");
    }
    let mut parser = RecordParser::new();
    let cuts: Vec<usize> = (1..stream.len()).collect();
    let parsed = feed(
        |piece| parser.push(piece).map_err(SessionError::from),
        &stream,
        &cuts,
    )
    .unwrap();
    assert_eq!(parsed, expected, "one byte at a time");
}

#[test]
fn the_parser_stops_at_each_record_boundary() {
    let first = record(RecordType::Credit, IDENTITY, 0, &17u32.to_le_bytes());
    let second = record(RecordType::Data, IDENTITY, 1, b"xNVXSy");
    let stream = [first.as_slice(), &second].concat();
    let mut parser = RecordParser::new();
    let (consumed, parsed) = parser.push(&stream).unwrap();
    assert_eq!(consumed, first.len());
    assert_eq!(parsed.unwrap().credit(), Some(17));
    let (consumed, parsed) = parser.push(&stream[first.len()..]).unwrap();
    assert_eq!(consumed, second.len());
    assert_eq!(parsed.unwrap().payload(), b"xNVXSy");
    assert_eq!(parser.push(&[]), Ok((0, None)));
}

#[test]
fn the_parser_holds_at_most_one_record() {
    let encoded = record(RecordType::Data, IDENTITY, 0, &[7; OUTER_MAX_PAYLOAD]);
    let mut parser = RecordParser::new();
    assert_eq!((parser.needed(), parser.buffered()), (OUTER_HEADER_LEN, 0));
    assert_eq!(parser.push(&encoded[..10]), Ok((10, None)));
    assert_eq!(
        (
            parser.needed(),
            parser.buffered(),
            parser.payload_capacity()
        ),
        (OUTER_HEADER_LEN - 10, 10, 0)
    );
    let header_rest = OUTER_HEADER_LEN - 10;
    assert_eq!(
        parser.push(&encoded[10..OUTER_HEADER_LEN]),
        Ok((header_rest, None))
    );
    // The payload buffer has exactly the length that the validated header declares.
    assert_eq!(
        (
            parser.needed(),
            parser.buffered(),
            parser.payload_capacity()
        ),
        (OUTER_MAX_PAYLOAD, OUTER_HEADER_LEN, OUTER_MAX_PAYLOAD)
    );
    let last = encoded.len() - 1;
    assert_eq!(
        parser.push(&encoded[OUTER_HEADER_LEN..last]),
        Ok((last - OUTER_HEADER_LEN, None))
    );
    assert_eq!((parser.needed(), parser.buffered()), (1, last));
    // Bytes after the record stay with the caller.
    let rest = [&encoded[last..], &[0xab; 100][..]].concat();
    let (consumed, parsed) = parser.push(&rest).unwrap();
    assert_eq!(consumed, 1);
    assert_eq!(parsed.unwrap().payload(), [7; OUTER_MAX_PAYLOAD]);
    assert_eq!(
        (
            parser.needed(),
            parser.buffered(),
            parser.payload_capacity()
        ),
        (OUTER_HEADER_LEN, 0, 0)
    );
}

#[test]
fn a_malformed_record_ends_the_stream() {
    let valid = record(RecordType::Data, IDENTITY, 0, b"NVXS");
    // The parser does not look for the magic of a later record.
    let stream = [&[0; OUTER_HEADER_LEN][..], &valid].concat();
    let mut parser = RecordParser::new();
    assert_eq!(parser.push(&stream), Err(RecordError::Magic([0; 4])));
    assert_eq!((parser.needed(), parser.buffered()), (OUTER_HEADER_LEN, 0));
    assert_eq!(parser.push(&valid), Err(RecordError::Magic([0; 4])));
    // A payload with an invalid value ends it once the payload completes.
    let mut parser = RecordParser::new();
    let mut stream = raw_header(RecordType::Credit.code(), IDENTITY, 0, 4);
    stream.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(parser.push(&stream[..46]), Ok((46, None)));
    assert_eq!(parser.push(&stream[46..]), Err(RecordError::Credit(0)));
    assert_eq!(parser.push(&valid), Err(RecordError::Credit(0)));
}

#[test]
fn debug_output_shows_no_payload_bytes() {
    let capability = *b"NVX-CAPABILITY-SECRET-0123456789";
    let encoded = record(RecordType::HostAttach, NONE, 0, &capability);
    let parsed = parse(&encoded).unwrap();
    let mut parser = RecordParser::new();
    parser.push(&encoded[..OUTER_HEADER_LEN + 8]).unwrap();
    let decimal = format!("{:?}", &capability[..4]);
    for debug in [format!("{parsed:?}"), format!("{parser:?}")] {
        assert!(!debug.contains("CAPABILITY"), "{debug}");
        assert!(!debug.contains(decimal.trim_matches(['[', ']'])), "{debug}");
    }
}

#[test]
fn errors_describe_the_violation() {
    let cases = [
        (
            SessionError::from(RecordError::Magic(*b"XVXS")),
            "NVXS record has an invalid magic, 0x58565853",
        ),
        (
            RecordError::Length {
                record_type: RecordType::Data,
                length: 0,
            }
            .into(),
            "NVXS DATA record has an invalid payload length of 0 bytes",
        ),
        (
            SessionError::Sequence {
                expected: 2,
                actual: 1,
            },
            "NVXS record has sequence 1 instead of 2",
        ),
        (
            SessionError::Unexpected(RecordType::GuestAttach),
            "unexpected NVXS GUEST_ATTACH record",
        ),
    ];
    for (error, message) in cases {
        assert_eq!(error.to_string(), message);
    }
    let error = SessionError::from(RecordError::Version(2));
    assert_eq!(
        std::error::Error::source(&error).unwrap().to_string(),
        "NVXS record has an unsupported version 2"
    );
}

/// Returns a valid record: its type, its identity and sequence, which are zero for a bootstrap
/// record, and a payload that its type allows.
fn valid_record() -> impl Strategy<Value = (RecordType, Identity, u64, Vec<u8>)> {
    let types: Vec<_> = (u8::MIN..=u8::MAX)
        .filter_map(RecordType::from_code)
        .collect();
    (
        prop::sample::select(types),
        any::<[u8; 16]>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_flat_map(|(record_type, instance_id, epoch, sequence)| {
            let payload = match record_type {
                RecordType::GuestAttach
                | RecordType::Reset
                | RecordType::Wait
                | RecordType::Ready => Just(Vec::new()).boxed(),
                RecordType::HostAttach => {
                    prop::collection::vec(any::<u8>(), CAPABILITY_LEN).boxed()
                }
                RecordType::Ack => (OUTER_MIN_RECEIVE_WINDOW..=OUTER_MAX_RECEIVE_WINDOW)
                    .prop_map(|window| window.to_le_bytes().to_vec())
                    .boxed(),
                RecordType::Credit => (1..=OUTER_MAX_RECEIVE_WINDOW)
                    .prop_map(|credit| credit.to_le_bytes().to_vec())
                    .boxed(),
                RecordType::Error => {
                    Just(ErrorCode::Authentication.code().to_le_bytes().to_vec()).boxed()
                }
                RecordType::Data => prop::collection::vec(any::<u8>(), 1..=1024).boxed(),
            };
            let (identity, sequence) = if record_type.is_bootstrap() {
                (NONE, 0)
            } else {
                (Identity { instance_id, epoch }, sequence)
            };
            (Just(record_type), Just(identity), Just(sequence), payload)
        })
}

/// Returns bytes that a parser may meet: arbitrary bytes, arbitrary bytes after the magic and the
/// version, and valid records with bytes overwritten.
fn streams() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..256),
        prop::collection::vec(any::<u8>(), 0..256)
            .prop_map(|rest| [&b"NVXS\x01\x00"[..], &rest].concat()),
        (
            valid_record(),
            prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..4),
        )
            .prop_map(|((record_type, identity, sequence, payload), writes)| {
                let mut encoded = record(record_type, identity, sequence, &payload);
                for (index, byte) in writes {
                    let index = index.index(encoded.len());
                    encoded[index] = byte;
                }
                encoded
            }),
    ]
}

proptest! {
    /// Every valid record encodes, with the NVXS session's codec and with the outer codec alike, to
    /// bytes that parse back to it, whichever two pieces they arrive in.
    #[test]
    fn records_round_trip(
        (record_type, identity, sequence, payload) in valid_record(),
        cut in any::<prop::sample::Index>(),
    ) {
        let encoded = record(record_type, identity, sequence, &payload);
        let outer = encode_outer(
            record_type.code(),
            &identity.instance_id,
            identity.epoch,
            sequence,
            &payload,
        )
        .unwrap();
        prop_assert_eq!(&outer, &encoded);
        let mut parser = RecordParser::new();
        let parsed = feed(
            |piece| parser.push(piece).map_err(SessionError::from),
            &encoded,
            &[cut.index(encoded.len() + 1)],
        )
        .unwrap();
        prop_assert_eq!(parsed.len(), 1);
        prop_assert_eq!(parsed[0].record_type(), record_type);
        prop_assert_eq!(parsed[0].instance_id(), identity.instance_id);
        prop_assert_eq!(parsed[0].epoch(), identity.epoch);
        prop_assert_eq!(parsed[0].sequence(), sequence);
        prop_assert_eq!(parsed[0].payload(), &payload[..]);
    }

    /// The parser returns an error, never panics, on bytes that hold no valid record, whichever
    /// two pieces they arrive in, and then fails every later call with that error. A record that
    /// it accepts encodes to exactly the bytes that it consumed for it.
    #[test]
    fn the_parser_never_panics(stream in streams(), cut in any::<prop::sample::Index>()) {
        let cut = cut.index(stream.len() + 1);
        let mut parser = RecordParser::new();
        let mut offset = 0;
        for mut piece in [&stream[..cut], &stream[cut..]] {
            while !piece.is_empty() {
                match parser.push(piece) {
                    Ok((consumed, parsed)) => {
                        prop_assert!(consumed > 0);
                        offset += consumed;
                        piece = &piece[consumed..];
                        if let Some(parsed) = parsed {
                            let mut encoded = Vec::new();
                            let result = encode_record(
                                parsed.record_type(),
                                &parsed.instance_id(),
                                parsed.epoch(),
                                parsed.sequence(),
                                parsed.payload(),
                                &mut encoded,
                            );
                            prop_assert_eq!(result, Ok(()));
                            prop_assert_eq!(&stream[offset - encoded.len()..offset], &encoded[..]);
                        }
                    }
                    Err(error) => {
                        prop_assert_eq!(parser.push(&stream), Err(error.clone()));
                        prop_assert_eq!(parser.push(&[]), Err(error));
                        return Ok(());
                    }
                }
            }
        }
    }
}
