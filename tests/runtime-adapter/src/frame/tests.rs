use super::{
    Decoder, FrameError, HEADER_LEN, Kind, MAX_BODY, MAX_CONTROL_BODY, MAX_TEXT_BODY, Message,
};

/// Returns one message of each kind, with bodies of no, few, and the most bytes.
fn messages() -> Vec<Message> {
    vec![
        Message::Ping,
        Message::Echo(Vec::new()),
        Message::Echo(b"hello".to_vec()),
        Message::Pong,
        Message::Echo((0..=u8::MAX).cycle().take(MAX_BODY).collect()),
        Message::RootConfig(b"version=1\n".to_vec()),
        Message::RootConfig(vec![b'c'; MAX_TEXT_BODY]),
        Message::Ready(b"\x050.1.0".to_vec()),
        Message::Ready(vec![b'r'; MAX_TEXT_BODY]),
        Message::RootFailed(Vec::new()),
        Message::RootFailed(vec![b'f'; MAX_TEXT_BODY]),
        Message::Probe(b"mounts".to_vec()),
        Message::Probe(vec![b'p'; MAX_TEXT_BODY]),
        Message::Report(b"key=value\n".to_vec()),
        Message::Report((0..=u8::MAX).rev().cycle().take(MAX_BODY).collect()),
        Message::ProbeFailed(b"unknown probe".to_vec()),
        Message::ProbeFailed(vec![b'e'; MAX_TEXT_BODY]),
        Message::ExecStart(vec![b's'; MAX_BODY]),
        Message::ExecInput(Vec::new()),
        Message::ExecInput((0..=u8::MAX).cycle().take(MAX_BODY).collect()),
        Message::ExecInputEnd(vec![1; MAX_CONTROL_BODY]),
        Message::ExecCancel(vec![2; 17]),
        Message::ExecCredit(vec![3; 21]),
        Message::ExecStarted(vec![4; 16]),
        Message::ExecOutput((0..=u8::MAX).rev().cycle().take(MAX_BODY).collect()),
        Message::ExecEnd(vec![5; 17]),
        Message::ExecInputClosed(Vec::new()),
        Message::ExecTerminal(vec![6; MAX_CONTROL_BODY]),
        Message::Maps(Vec::new()),
        Message::Maps((0..=u8::MAX).cycle().take(MAX_BODY).collect()),
        Message::Mapped,
        Message::MapFailed(b"\x02\0\0\0refused".to_vec()),
        Message::MapFailed(vec![b'm'; MAX_TEXT_BODY]),
        Message::Start,
        Message::Started,
        Message::Await,
        Message::Stop,
        Message::Stopped,
    ]
}

/// Returns a message of each kind, in the order of their codes, with a body of a few bytes if
/// the kind has one.
fn one_of_each() -> Vec<Message> {
    let body = |code: u8| vec![code; usize::from(code % 4) + 1];
    vec![
        Message::Ping,
        Message::Pong,
        Message::Echo(body(3)),
        Message::Stop,
        Message::Stopped,
        Message::RootConfig(body(6)),
        Message::Ready(body(7)),
        Message::RootFailed(body(8)),
        Message::Probe(body(9)),
        Message::Report(body(10)),
        Message::ProbeFailed(body(11)),
        Message::ExecStart(body(12)),
        Message::ExecInput(body(13)),
        Message::ExecInputEnd(body(14)),
        Message::ExecCancel(body(15)),
        Message::ExecCredit(body(16)),
        Message::ExecStarted(body(17)),
        Message::ExecOutput(body(18)),
        Message::ExecEnd(body(19)),
        Message::ExecInputClosed(body(20)),
        Message::ExecTerminal(body(21)),
        Message::Maps(body(22)),
        Message::Mapped,
        Message::MapFailed(body(24)),
        Message::Start,
        Message::Started,
        Message::Await,
    ]
}

/// Returns the encodings of `messages`, one after the other.
fn stream(messages: &[Message]) -> Vec<u8> {
    messages
        .iter()
        .flat_map(|message| message.encode().unwrap())
        .collect()
}

/// Decodes `pieces` in order, and returns the messages that they complete.
fn decode<'a>(decoder: &mut Decoder, pieces: impl IntoIterator<Item = &'a [u8]>) -> Vec<Message> {
    let mut decoded = Vec::new();
    for piece in pieces {
        let mut rest = piece;
        while !rest.is_empty() {
            let (consumed, message) = decoder.push(rest).unwrap();
            assert_ne!(consumed, 0, "a push consumes bytes");
            rest = &rest[consumed..];
            decoded.extend(message);
        }
    }
    decoded
}

#[test]
fn the_header_holds_the_body_length_then_the_kind() {
    assert_eq!(Message::Ping.encode().unwrap(), [0, 0, 0, 0, 1]);
    assert_eq!(Message::Pong.encode().unwrap(), [0, 0, 0, 0, 2]);
    assert_eq!(
        Message::Echo(b"abc".to_vec()).encode().unwrap(),
        [3, 0, 0, 0, 3, b'a', b'b', b'c']
    );
    assert_eq!(Message::Stop.encode().unwrap(), [0, 0, 0, 0, 4]);
    assert_eq!(Message::Stopped.encode().unwrap(), [0, 0, 0, 0, 5]);
    assert_eq!(
        Message::RootConfig(b"c".to_vec()).encode().unwrap(),
        [1, 0, 0, 0, 6, b'c']
    );
    assert_eq!(
        Message::Ready(b"\x01r".to_vec()).encode().unwrap(),
        [2, 0, 0, 0, 7, 1, b'r']
    );
    assert_eq!(
        Message::RootFailed(b"f".to_vec()).encode().unwrap(),
        [1, 0, 0, 0, 8, b'f']
    );
    assert_eq!(
        Message::Probe(b"p".to_vec()).encode().unwrap(),
        [1, 0, 0, 0, 9, b'p']
    );
    assert_eq!(
        Message::Report(b"r".to_vec()).encode().unwrap(),
        [1, 0, 0, 0, 10, b'r']
    );
    assert_eq!(
        Message::ProbeFailed(b"e".to_vec()).encode().unwrap(),
        [1, 0, 0, 0, 11, b'e']
    );
    let exec = [
        Message::ExecStart(b"x".to_vec()),
        Message::ExecInput(b"x".to_vec()),
        Message::ExecInputEnd(b"x".to_vec()),
        Message::ExecCancel(b"x".to_vec()),
        Message::ExecCredit(b"x".to_vec()),
        Message::ExecStarted(b"x".to_vec()),
        Message::ExecOutput(b"x".to_vec()),
        Message::ExecEnd(b"x".to_vec()),
        Message::ExecInputClosed(b"x".to_vec()),
        Message::ExecTerminal(b"x".to_vec()),
    ];
    for (kind, message) in (12..).zip(exec) {
        assert_eq!(message.encode().unwrap(), [1, 0, 0, 0, kind, b'x']);
    }
    assert_eq!(
        Message::Maps(b"x".to_vec()).encode().unwrap(),
        [1, 0, 0, 0, 22, b'x']
    );
    assert_eq!(Message::Mapped.encode().unwrap(), [0, 0, 0, 0, 23]);
    assert_eq!(
        Message::MapFailed(b"x".to_vec()).encode().unwrap(),
        [1, 0, 0, 0, 24, b'x']
    );
    assert_eq!(Message::Start.encode().unwrap(), [0, 0, 0, 0, 25]);
    assert_eq!(Message::Started.encode().unwrap(), [0, 0, 0, 0, 26]);
    assert_eq!(Message::Await.encode().unwrap(), [0, 0, 0, 0, 28]);
    // The kinds are numbered from 1, without gaps but 27, the retired Activate.
    for (kind, message) in DEFINED.into_iter().zip(one_of_each()) {
        assert_eq!(message.kind(), kind, "{message:?}");
    }
    assert_eq!(one_of_each().len(), DEFINED.len());
    for message in messages().into_iter().chain(one_of_each()) {
        assert_eq!(message.encode().unwrap()[HEADER_LEN - 1], message.kind());
    }
}

/// The codes of the kinds that the framing defines, in order: every code from 1 to 28 but 27,
/// which the retired Activate had.
const DEFINED: [u8; 27] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    28,
];

#[test]
fn each_kind_decodes_from_its_code_alone() {
    for code in 0..=u8::MAX {
        let defined = DEFINED.contains(&code).then_some(code);
        assert_eq!(Kind::from_code(code).map(Kind::code), defined, "{code}");
    }
}

#[test]
fn messages_arrive_whole_in_pieces_of_any_size() {
    let messages = messages();
    let stream = stream(&messages);
    for size in [1, 2, 3, HEADER_LEN, 7, 4096, 64 * 1024, stream.len()] {
        let mut decoder = Decoder::new();
        assert_eq!(
            decode(&mut decoder, stream.chunks(size)),
            messages,
            "{size}"
        );
        assert!(!decoder.is_partial());
    }
}

#[test]
fn messages_arrive_whole_across_a_split_at_every_byte() {
    let mut messages = vec![
        Message::Echo(b"split".to_vec()),
        Message::Ping,
        Message::Stop,
    ];
    // And a message of each kind, those without a body included.
    messages.extend(one_of_each());
    let stream = stream(&messages);
    for split in 0..=stream.len() {
        let mut decoder = Decoder::new();
        let pieces = [&stream[..split], &stream[split..]];
        assert_eq!(decode(&mut decoder, pieces), messages, "{split}");
        assert!(!decoder.is_partial());
    }
}

#[test]
fn a_push_stops_at_the_end_of_a_message() {
    let stream = stream(&[Message::Ping, Message::Echo(b"next".to_vec())]);
    let mut decoder = Decoder::new();
    assert_eq!(decoder.push(&stream), Ok((HEADER_LEN, Some(Message::Ping))));
    assert!(!decoder.is_partial());
    assert_eq!(
        decoder.push(&stream[HEADER_LEN..]),
        Ok((HEADER_LEN + 4, Some(Message::Echo(b"next".to_vec()))))
    );
}

#[test]
fn malformed_headers_fail_before_their_body_is_taken() {
    let too_long = u32::try_from(MAX_BODY + 1).unwrap().to_le_bytes();
    let too_long_text = u32::try_from(MAX_TEXT_BODY + 1).unwrap().to_le_bytes();
    let too_long_control = u32::try_from(MAX_CONTROL_BODY + 1).unwrap().to_le_bytes();
    let header = |length: [u8; 4], kind: u8| [length[0], length[1], length[2], length[3], kind];
    let text = |kind: u8| header(too_long_text, kind);
    let mut cases = vec![
        ([0, 0, 0, 0, 0], FrameError::Kind(0)),
        // The retired Activate, whose record the restore packet carries since.
        ([4, 0, 0, 0, 27], FrameError::Kind(27)),
        ([0, 0, 0, 0, 29], FrameError::Kind(29)),
        ([9, 0, 0, 0, 0xff], FrameError::Kind(0xff)),
        (
            [0xff, 0xff, 0xff, 0xff, 3],
            FrameError::Length {
                kind: 3,
                length: u32::MAX as usize,
            },
        ),
    ];
    // The kinds without a body.
    for kind in [1, 2, 4, 5, 23, 25, 26, 28] {
        let error = FrameError::Length { kind, length: 1 };
        cases.push(([1, 0, 0, 0, kind], error));
    }
    for kind in [3, 10, 12, 13, 18, 22] {
        let length = MAX_BODY + 1;
        cases.push((header(too_long, kind), FrameError::Length { kind, length }));
    }
    for kind in [6, 7, 8, 9, 11, 24] {
        let length = MAX_TEXT_BODY + 1;
        cases.push((text(kind), FrameError::Length { kind, length }));
    }
    for kind in [14, 15, 16, 17, 19, 20, 21] {
        let length = MAX_CONTROL_BODY + 1;
        let error = FrameError::Length { kind, length };
        cases.push((header(too_long_control, kind), error));
    }
    for (header, error) in cases {
        let mut decoder = Decoder::new();
        assert_eq!(decoder.push(&header[..2]), Ok((2, None)));
        assert!(decoder.is_partial());
        let mut rest = header[2..].to_vec();
        rest.extend_from_slice(b"body");
        assert_eq!(decoder.push(&rest), Err(error), "{header:?}");
        assert!(!decoder.is_partial(), "{header:?}");
        assert_eq!(decoder.body.capacity(), 0, "{header:?}");
    }
}

#[test]
fn clearing_drops_a_partial_message() {
    let echo = Message::Echo(vec![1; 100]).encode().unwrap();
    let mut decoder = Decoder::new();
    assert_eq!(decoder.push(&echo[..50]), Ok((50, None)));
    assert!(decoder.is_partial());
    decoder.clear();
    assert!(!decoder.is_partial());
    let ping = Message::Ping.encode().unwrap();
    assert_eq!(decoder.push(&ping), Ok((HEADER_LEN, Some(Message::Ping))));
}

#[test]
fn encoding_refuses_a_body_beyond_the_limit() {
    let longest = Message::Echo(vec![0; MAX_BODY]).encode().unwrap();
    assert_eq!(longest.len(), HEADER_LEN + MAX_BODY);
    let report = Message::Report(vec![0; MAX_BODY]).encode().unwrap();
    assert_eq!(report.len(), HEADER_LEN + MAX_BODY);
    let maps = Message::Maps(vec![0; MAX_BODY]).encode().unwrap();
    assert_eq!(maps.len(), HEADER_LEN + MAX_BODY);
    let ready = Message::Ready(vec![0; MAX_TEXT_BODY]).encode().unwrap();
    assert_eq!(ready.len(), HEADER_LEN + MAX_TEXT_BODY);
    let cases = [
        (Message::Echo(vec![0; MAX_BODY + 1]), 3, MAX_BODY + 1),
        (Message::Report(vec![0; MAX_BODY + 1]), 10, MAX_BODY + 1),
        (
            Message::RootConfig(vec![0; MAX_TEXT_BODY + 1]),
            6,
            MAX_TEXT_BODY + 1,
        ),
        (
            Message::Ready(vec![0; MAX_TEXT_BODY + 1]),
            7,
            MAX_TEXT_BODY + 1,
        ),
        (
            Message::RootFailed(vec![0; MAX_TEXT_BODY + 1]),
            8,
            MAX_TEXT_BODY + 1,
        ),
        (
            Message::Probe(vec![0; MAX_TEXT_BODY + 1]),
            9,
            MAX_TEXT_BODY + 1,
        ),
        (
            Message::ProbeFailed(vec![0; MAX_TEXT_BODY + 1]),
            11,
            MAX_TEXT_BODY + 1,
        ),
        (Message::ExecStart(vec![0; MAX_BODY + 1]), 12, MAX_BODY + 1),
        (Message::ExecInput(vec![0; MAX_BODY + 1]), 13, MAX_BODY + 1),
        (Message::ExecOutput(vec![0; MAX_BODY + 1]), 18, MAX_BODY + 1),
        (
            Message::ExecCredit(vec![0; MAX_CONTROL_BODY + 1]),
            16,
            MAX_CONTROL_BODY + 1,
        ),
        (
            Message::ExecTerminal(vec![0; MAX_CONTROL_BODY + 1]),
            21,
            MAX_CONTROL_BODY + 1,
        ),
        (Message::Maps(vec![0; MAX_BODY + 1]), 22, MAX_BODY + 1),
        (
            Message::MapFailed(vec![0; MAX_TEXT_BODY + 1]),
            24,
            MAX_TEXT_BODY + 1,
        ),
    ];
    for (message, kind, length) in cases {
        assert_eq!(
            message.encode(),
            Err(FrameError::Length { kind, length }),
            "{kind}"
        );
    }
}
