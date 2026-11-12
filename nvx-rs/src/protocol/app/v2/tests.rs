//! Tests of version 2 of the application frames: the bytes of each frame, as the protocol defines
//! them, and its round trip through the codec; the protocol version that `READY` carries; the
//! bounds of the frames of an execution's standard input and output; the frames that version 2
//! refuses; and the payloads of version 1, which its frames carry unchanged and in order.

use proptest::prelude::*;

use super::*;
use crate::protocol::{HostMap, decode_maps_payload, encode_maps_payloads};
use crate::tests::{ACTIVATION_V2, NVXC_V1, vector, vectors};

/// A request ID whose bytes all differ, so that a header that misplaced one would show.
const ID: u64 = 0x0102_0304_0506_0708;

/// A frame of each kind, and one of each outcome of an execution, with the code of its kind, its
/// status and its payload, as the protocol defines them, independently of the codec.
const FRAMES: [(Frame<'static>, u8, i32, &[u8]); 23] = [
    (Frame::Ping, 0x01, 0, b""),
    (Frame::Exec(b"workload"), 0x02, 0, b"workload"),
    (Frame::Stop, 0x03, 0, b""),
    (Frame::Cancel, 0x04, 0, b""),
    (Frame::Maps(b"table"), 0x06, 0, b"table"),
    (Frame::Capture(&[1]), 0x07, 0, &[1]),
    (Frame::Activate(b"record"), 0x08, 0, b"record"),
    (Frame::Stdin(b"input"), 0x09, 0, b"input"),
    (Frame::StdinEnd, 0x0a, 0, b""),
    (ready(2, b""), 0x81, 0, &[2]),
    (ready(2, &[0]), 0x81, 0, &[2, 0]),
    (Frame::Stdout(b"output"), 0x82, 0, b"output"),
    (Frame::Stderr(b"error"), 0x83, 0, b"error"),
    (exit(0, Outcome::Exit), 0x84, 0, b"exit"),
    (exit(137, Outcome::Signal), 0x84, 137, b"signal"),
    (exit(124, Outcome::Timeout), 0x84, 124, b"timeout"),
    (exit(137, Outcome::Cancelled), 0x84, 137, b"cancelled"),
    (exit(125, Outcome::Failed), 0x84, 125, b"failed"),
    (Frame::Stopped, 0x85, 0, b""),
    (Frame::StdinAck(0x0001_0000), 0x86, 0, &[0, 0, 1, 0]),
    (Frame::Busy, 0x87, 0, b""),
    (error(22, b"invalid-request"), 0xff, 22, b"invalid-request"),
    (error(-1, b"x"), 0xff, -1, b"x"),
];

/// Returns a `READY` of `version` that reports `activation`.
const fn ready(version: u8, activation: &[u8]) -> Frame<'_> {
    Frame::Ready {
        version,
        activation,
    }
}

/// Returns an `EXIT` with `status` and `outcome`.
const fn exit(status: i32, outcome: Outcome) -> Frame<'static> {
    Frame::Exit { status, outcome }
}

/// Returns an `ERROR` with `status` and `category`.
const fn error(status: i32, category: &[u8]) -> Frame<'_> {
    Frame::Error { status, category }
}

/// Returns the bytes of a frame of `kind` with [`ID`], `status` and `payload`, as the protocol
/// defines them, independently of the codec.
fn wire(kind: u8, status: i32, payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len()).unwrap();
    [
        &b"NVXC\x02"[..],
        &[kind, 0, 0],
        &ID.to_le_bytes(),
        &status.to_le_bytes(),
        &length.to_le_bytes(),
        payload,
    ]
    .concat()
}

#[test]
fn every_frame_round_trips_in_its_wire_form() {
    for (frame, kind, status, payload) in FRAMES {
        let bytes = wire(kind, status, payload);
        assert_eq!(frame.encode(ID).unwrap(), bytes, "{frame:?}");
        assert_eq!(Frame::decode(&bytes).unwrap(), (ID, frame));
    }
}

/// `READY` carries the current protocol version, then what Ready reports about an activation,
/// unchanged. It decodes another version too, so that its receiver can refuse that version by
/// name.
#[test]
fn ready_round_trips_the_protocol_version() {
    assert_eq!(VERSION, 2);
    let bytes = ready(VERSION, &[]).encode(ID).unwrap();
    assert_eq!(bytes, wire(0x81, 0, &[2]));
    assert_eq!(Frame::decode(&bytes).unwrap(), (ID, ready(VERSION, &[])));
    for vector in vectors(ACTIVATION_V2) {
        if vector.kind == "READY" {
            let bytes = wire(0x81, 0, &[&[2][..], &vector.bytes[..]].concat());
            let decoded = Frame::decode(&bytes).unwrap();
            assert_eq!(
                decoded,
                (ID, ready(VERSION, &vector.bytes)),
                "{}",
                vector.name
            );
        }
    }
    for version in [0, 1, 3, u8::MAX] {
        let bytes = ready(version, &[]).encode(ID).unwrap();
        assert_eq!(Frame::decode(&bytes).unwrap(), (ID, ready(version, &[])));
    }
}

/// A chunk of standard input, output or error fills at most one outer data record, whose payload
/// is smaller than the window of standard input, and an acknowledgement returns 1 to
/// [`STDIN_WINDOW`] bytes to that window.
#[test]
fn stream_frames_keep_their_bounds() {
    assert_eq!((STDIN_WINDOW, MAX_PAYLOAD), (64 * 1024, 65_512));
    let largest = vec![0x5a; MAX_PAYLOAD];
    for frame in [
        Frame::Stdin(&largest),
        Frame::Stdout(&largest),
        Frame::Stderr(&largest),
    ] {
        let bytes = frame.encode(ID).unwrap();
        assert_eq!(bytes.len(), OUTER_MAX_PAYLOAD);
        assert_eq!(Frame::decode(&bytes).unwrap(), (ID, frame));
    }
    let longer = vec![0x5a; MAX_PAYLOAD + 1];
    for frame in [
        Frame::Stdin(&longer),
        Frame::Stdout(&longer),
        Frame::Exec(&longer),
        ready(VERSION, &largest),
    ] {
        assert!(frame.encode(ID).is_err(), "{:?}", frame.encode(ID));
    }
    assert!(Frame::decode(&wire(0x82, 0, &longer)).is_err());
    for count in [1, STDIN_WINDOW] {
        let bytes = Frame::StdinAck(count).encode(ID).unwrap();
        assert_eq!(bytes, wire(0x86, 0, &count.to_le_bytes()));
        assert_eq!(Frame::decode(&bytes).unwrap(), (ID, Frame::StdinAck(count)));
    }
    for count in [0, STDIN_WINDOW + 1] {
        assert!(Frame::StdinAck(count).encode(ID).is_err(), "{count}");
        assert!(Frame::decode(&wire(0x86, 0, &count.to_le_bytes())).is_err());
    }
    for (frame, kind) in [
        (Frame::Stdin(&[]), 0x09),
        (Frame::Stdout(&[]), 0x82),
        (Frame::Stderr(&[]), 0x83),
    ] {
        assert!(frame.encode(ID).is_err(), "{frame:?}");
        assert!(Frame::decode(&wire(kind, 0, &[])).is_err(), "{frame:?}");
    }
}

#[test]
fn malformed_frames_are_refused() {
    let valid = wire(0x82, 0, b"output");
    for length in 0..valid.len() {
        assert!(Frame::decode(&valid[..length]).is_err(), "{length}");
    }
    let mut trailing = valid.clone();
    trailing.push(0);
    assert!(Frame::decode(&trailing).is_err());
    for (offset, byte) in [(0, b'X'), (4, 3), (6, 1), (7, 1)] {
        let mut changed = valid.clone();
        changed[offset] = byte;
        assert!(Frame::decode(&changed).is_err(), "{offset}");
    }
    // A frame of version 1, as its conformance vector holds it.
    let refused = Frame::decode(&vector(NVXC_V1, "FRAME", "PING").bytes).unwrap_err();
    assert_eq!(
        refused.message(),
        "the control peer sent a frame of another protocol than NVXC version 2"
    );
    // Unknown kinds, version 1's FEATURES among them.
    for kind in [0x00, 0x05, 0x0b, 0x80, 0x88, 0xfe] {
        assert!(Frame::decode(&wire(kind, 0, &[])).is_err(), "{kind:#x}");
    }
    let mut zero = wire(0x01, 0, &[]);
    zero[8..16].fill(0);
    assert!(Frame::decode(&zero).is_err());
    assert!(Frame::Ping.encode(0).is_err());
    for (frame, kind, _, payload) in FRAMES {
        if !matches!(frame, Frame::Exit { .. } | Frame::Error { .. }) {
            assert!(Frame::decode(&wire(kind, 1, payload)).is_err(), "{frame:?}");
        }
    }
    // A payload in a kind that has none, a READY without a version, an acknowledgement that is
    // not a `u32`, an outcome that version 2 does not have, and an empty error category.
    let payloads: [(u8, &[u8]); 13] = [
        (0x01, &[0]),
        (0x03, &[0]),
        (0x04, &[0]),
        (0x0a, &[0]),
        (0x85, &[0]),
        (0x87, &[0]),
        (0x81, &[]),
        (0x86, &[1, 0, 0]),
        (0x86, &[1, 0, 0, 0, 0]),
        (0x84, b"output-limit"),
        (0x84, b"canceled"),
        (0x84, b""),
        (0xff, b""),
    ];
    for (kind, payload) in payloads {
        let bytes = wire(kind, 0, payload);
        assert!(Frame::decode(&bytes).is_err(), "{kind:#x} {payload:?}");
    }
    assert!(error(22, &[]).encode(ID).is_err());
}

proptest! {
    /// No bytes make decoding panic, and the frames that decode encode back to their bytes, so
    /// that each frame has one encoding.
    #[test]
    fn decoded_frames_encode_back_to_their_bytes(
        kind in any::<u8>(),
        status in prop_oneof![Just(0), any::<i32>()],
        payload in proptest::collection::vec(any::<u8>(), 0..8),
        cut in any::<prop::sample::Index>(),
    ) {
        let bytes = wire(kind, status, &payload);
        if let Ok((id, frame)) = Frame::decode(&bytes) {
            prop_assert_eq!(&frame.encode(id).unwrap(), &bytes);
        }
        prop_assert!(Frame::decode(&bytes[..cut.index(bytes.len())]).is_err());
    }
}

/// The workload of an `EXEC` crosses version 2 unchanged: its arguments, working directory and
/// environment in their order, and its timeout, as the fake OpenVMM's decoder reads them.
#[cfg(all(feature = "cli", feature = "testing"))]
#[test]
fn exec_requests_carry_their_workload_unchanged() {
    use crate::protocol::{
        Workload, WorkloadEnvironment, decode_exec_payload, encode_exec_payload,
    };

    let environment = ["ZED=1".to_owned(), "ALPHA=2".to_owned()];
    let workload = Workload {
        argv: ["/bin/sh", "-c", "cat; echo done"]
            .map(str::to_owned)
            .to_vec(),
        timeout_ms: 30_000,
        cwd: Some("/work"),
        environment: WorkloadEnvironment::Layered(&environment),
    };
    let payload = encode_exec_payload(&workload).unwrap();
    let bytes = Frame::Exec(&payload).encode(ID).unwrap();
    let Ok((ID, Frame::Exec(carried))) = Frame::decode(&bytes) else {
        panic!("the EXEC frame does not decode");
    };
    assert_eq!(carried, payload);
    let decoded = decode_exec_payload(carried).unwrap();
    assert_eq!(decoded.argv, workload.argv);
    assert_eq!(decoded.cwd.as_deref(), Some("/work"));
    assert_eq!(decoded.environment.as_deref(), Some(&environment[..]));
    assert!(decoded.inherit_default_env);
    assert_eq!(decoded.timeout_ms, 30_000);
}

/// Each `EXEC` payload of version 1's conformance vectors crosses version 2 byte for byte.
#[test]
fn exec_payloads_cross_unchanged() {
    for vector in vectors(NVXC_V1) {
        if vector.kind == "EXEC" {
            let bytes = Frame::Exec(&vector.bytes).encode(ID).unwrap();
            assert_eq!(bytes, wire(0x02, 0, &vector.bytes), "{}", vector.name);
            let decoded = Frame::decode(&bytes).unwrap();
            assert_eq!(decoded, (ID, Frame::Exec(&vector.bytes)), "{}", vector.name);
        }
    }
}

/// A host mapping table that takes several `MAPS` frames reaches the guest whole, in table order,
/// each frame's payload continuing the table where the one before it ended.
#[test]
fn maps_tables_cross_unchanged_in_order() {
    let table: Vec<_> = (0..400)
        .map(|index| HostMap {
            source: format!("share-{index:03}/{}", "s".repeat(200)),
            target: format!("/mnt/{index:03}/{}", "t".repeat(200)),
            read_only: index % 3 == 0,
        })
        .collect();
    let payloads = encode_maps_payloads(&table, MAX_PAYLOAD).unwrap();
    assert!(payloads.len() > 2, "{}", payloads.len());
    let mut received = Vec::new();
    for payload in &payloads {
        let bytes = Frame::Maps(payload).encode(ID).unwrap();
        let Ok((ID, Frame::Maps(carried))) = Frame::decode(&bytes) else {
            panic!("the MAPS frame does not decode");
        };
        assert_eq!(carried, &payload[..]);
        let decoded = decode_maps_payload(carried).unwrap();
        assert_eq!(decoded.first as usize, received.len());
        received.extend(decoded.maps);
    }
    assert_eq!(received, table);
}

/// The capture request's flag crosses version 2 unchanged, from the host's encoder to the guest's
/// decoder.
#[cfg(all(
    feature = "cli",
    feature = "guest",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[test]
fn capture_requests_carry_their_flag_unchanged() {
    use crate::protocol::{decode_capture_payload, encode_capture_payload};

    for kill_executions in [false, true] {
        let payload = encode_capture_payload(kill_executions);
        let bytes = Frame::Capture(&payload).encode(ID).unwrap();
        let Ok((ID, Frame::Capture(carried))) = Frame::decode(&bytes) else {
            panic!("the CAPTURE frame does not decode");
        };
        assert_eq!(decode_capture_payload(carried).unwrap(), kill_executions);
    }
}

/// The activate exchange crosses version 2 unchanged: the host's activate request reaches the
/// guest's decoder with its record and workload identity, and the guest's `READY` brings the
/// host what Ready reports about that record's activation.
#[cfg(all(
    feature = "cli",
    feature = "guest",
    target_os = "linux",
    target_arch = "x86_64"
))]
#[test]
fn the_activate_exchange_carries_its_payloads_unchanged() {
    use crate::activation::{ActivationRecord, ReadyActivation};
    use crate::protocol::workload::WorkloadIdentity;
    use crate::protocol::{decode_activate_payload, encode_activate_payload};

    let workload = WorkloadIdentity {
        uid: 1000,
        gid: 1001,
        create_account: true,
    };
    for vector in vectors(ACTIVATION_V2) {
        if vector.kind != "RECORD" {
            continue;
        }
        let name = vector.name;
        let record = ActivationRecord::decode(&vector.bytes).unwrap();
        let payload = encode_activate_payload(&record, &workload).unwrap();
        let bytes = Frame::Activate(&payload).encode(ID).unwrap();
        let Ok((ID, Frame::Activate(carried))) = Frame::decode(&bytes) else {
            panic!("the ACTIVATE frame of {name} does not decode");
        };
        let (applied, identity) = decode_activate_payload(carried).unwrap();
        assert_eq!((&applied, identity), (&record, workload), "{name}");
        let reported = ReadyActivation::of(&applied).encode();
        let bytes = ready(VERSION, &reported).encode(ID).unwrap();
        let Ok((
            ID,
            Frame::Ready {
                version,
                activation,
            },
        )) = Frame::decode(&bytes)
        else {
            panic!("the READY of {name} does not decode");
        };
        assert_eq!(version, VERSION, "{name}");
        let activation = ReadyActivation::decode(activation).unwrap();
        assert_eq!(activation, ReadyActivation::of(&record), "{name}");
    }
}
