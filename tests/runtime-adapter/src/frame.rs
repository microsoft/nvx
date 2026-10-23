//! The adapter's own application framing, which its host program and guest agent exchange over the
//! control session between the host and the guest, in place of NVX's `NVXC` frames.
//!
//! A message is a header of [`HEADER_LEN`] bytes, then a body. The header holds the body's length,
//! a little-endian `u32`, then the message's kind. The session carries bytes and keeps no message
//! boundaries, so a message may span several of its records, and one record may end a message and
//! start the next. [`Decoder`] reassembles the messages: it checks each header before it takes any
//! byte of the body, and holds at most one partial message, whose body has at most [`MAX_BODY`]
//! bytes.
//!
//! The framing cannot find the next message after a malformed one, so a [`FrameError`] ends the
//! stream.

use std::fmt;

use nvx::protocol::OUTER_MAX_PAYLOAD;

/// Bytes of a message's header: the body's length, then the message's kind.
pub const HEADER_LEN: usize = 5;

/// Most bytes of a message's body: as many as four of the session's largest records carry, so that
/// the longest message, with its header, spans five records.
pub const MAX_BODY: usize = 4 * OUTER_MAX_PAYLOAD;

/// A message of the adapter's framing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// Asks the guest to answer with [`Message::Pong`].
    Ping,
    /// The guest's answer to [`Message::Ping`].
    Pong,
    /// Bytes that the guest sends back in an `Echo` of its own.
    Echo(Vec<u8>),
    /// Asks the guest to stop serving, which it confirms with [`Message::Stopped`].
    Stop,
    /// The guest's confirmation of [`Message::Stop`], its last message.
    Stopped,
}

impl Message {
    /// Returns the message's kind, the last byte of its header.
    pub fn kind(&self) -> u8 {
        self.parts().0.code()
    }

    /// Returns the message's encoding: its header, then its body. An `Echo` body has at most
    /// [`MAX_BODY`] bytes.
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        let (kind, body) = self.parts();
        if body.len() > kind.max_body() {
            return Err(FrameError::Length {
                kind: kind.code(),
                length: body.len(),
            });
        }
        // The body has at most `MAX_BODY` bytes, so its length fits the header's field.
        let length = body.len() as u32;
        let mut encoded = Vec::with_capacity(HEADER_LEN + body.len());
        encoded.extend_from_slice(&length.to_le_bytes());
        encoded.push(kind.code());
        encoded.extend_from_slice(body);
        Ok(encoded)
    }

    /// Returns the message's kind and body.
    fn parts(&self) -> (Kind, &[u8]) {
        match self {
            Self::Ping => (Kind::Ping, &[]),
            Self::Pong => (Kind::Pong, &[]),
            Self::Echo(body) => (Kind::Echo, body),
            Self::Stop => (Kind::Stop, &[]),
            Self::Stopped => (Kind::Stopped, &[]),
        }
    }
}

/// Kind of a [`Message`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Kind {
    /// [`Message::Ping`].
    Ping = 1,
    /// [`Message::Pong`].
    Pong = 2,
    /// [`Message::Echo`].
    Echo = 3,
    /// [`Message::Stop`].
    Stop = 4,
    /// [`Message::Stopped`].
    Stopped = 5,
}

impl Kind {
    const ALL: [Self; 5] = [
        Self::Ping,
        Self::Pong,
        Self::Echo,
        Self::Stop,
        Self::Stopped,
    ];

    /// Returns the kind with `code`, if the framing defines one.
    fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.code() == code)
    }

    /// Returns the kind's code, the last byte of a header.
    fn code(self) -> u8 {
        self as u8
    }

    /// Returns the most bytes of a body of this kind.
    fn max_body(self) -> usize {
        match self {
            Self::Echo => MAX_BODY,
            Self::Ping | Self::Pong | Self::Stop | Self::Stopped => 0,
        }
    }

    /// Returns the message of this kind with `body`, which has at most
    /// [`max_body`](Self::max_body) bytes.
    fn message(self, body: Vec<u8>) -> Message {
        match self {
            Self::Ping => Message::Ping,
            Self::Pong => Message::Pong,
            Self::Echo => Message::Echo(body),
            Self::Stop => Message::Stop,
            Self::Stopped => Message::Stopped,
        }
    }
}

/// A malformed message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameError {
    /// A kind that the framing does not define.
    Kind(u8),
    /// A body longer than [`MAX_BODY`] bytes, or a body for a kind that has none.
    Length {
        /// Kind of the message.
        kind: u8,
        /// Bytes of the body.
        length: usize,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kind(kind) => write!(formatter, "unknown message kind {kind}"),
            Self::Length { kind, length } => write!(
                formatter,
                "message of kind {kind} with a body of {length} bytes"
            ),
        }
    }
}

impl std::error::Error for FrameError {}

/// Reassembles messages from bytes that arrive in pieces of any size, such as the payloads of the
/// control session's records.
#[derive(Clone, Debug, Default)]
pub struct Decoder {
    /// Header of the message in progress.
    header: [u8; HEADER_LEN],
    /// Bytes of the header that arrived.
    header_len: usize,
    /// Kind of the message in progress, once its header arrived and checked out.
    kind: Option<Kind>,
    /// Bytes of the body that the header announced.
    body_len: usize,
    /// Bytes of the body that arrived.
    body: Vec<u8>,
}

impl Decoder {
    /// Creates a decoder that expects the start of a message.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes bytes until a message completes or `input` runs out.
    ///
    /// Returns the number of bytes consumed and the message that the last of them completed, if
    /// any. Bytes after that message are not consumed, so the caller passes them again. A
    /// malformed header fails before any byte of its body is taken, and leaves the decoder
    /// empty.
    pub fn push(&mut self, input: &[u8]) -> Result<(usize, Option<Message>), FrameError> {
        let mut consumed = 0;
        let kind = match self.kind {
            Some(kind) => kind,
            None => {
                consumed = (HEADER_LEN - self.header_len).min(input.len());
                self.header[self.header_len..][..consumed].copy_from_slice(&input[..consumed]);
                self.header_len += consumed;
                if self.header_len < HEADER_LEN {
                    return Ok((consumed, None));
                }
                let [b0, b1, b2, b3, code] = self.header;
                // A length that `usize` cannot hold exceeds every limit.
                let length =
                    usize::try_from(u32::from_le_bytes([b0, b1, b2, b3])).unwrap_or(usize::MAX);
                let kind = Kind::from_code(code).ok_or(FrameError::Kind(code));
                let checked = kind.and_then(|kind| {
                    if length > kind.max_body() {
                        Err(FrameError::Length { kind: code, length })
                    } else {
                        Ok(kind)
                    }
                });
                let kind = checked.inspect_err(|_| self.clear())?;
                self.kind = Some(kind);
                self.body_len = length;
                self.body.reserve_exact(length);
                kind
            }
        };
        let taken = (self.body_len - self.body.len()).min(input.len() - consumed);
        self.body
            .extend_from_slice(&input[consumed..consumed + taken]);
        consumed += taken;
        if self.body.len() < self.body_len {
            return Ok((consumed, None));
        }
        let body = std::mem::take(&mut self.body);
        self.clear();
        Ok((consumed, Some(kind.message(body))))
    }

    /// Drops the message in progress, if any, such as when the session resets and its new epoch
    /// starts with a new message.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Returns whether part of a message arrived.
    pub fn is_partial(&self) -> bool {
        self.header_len != 0
    }
}

#[cfg(test)]
mod tests {
    use super::{Decoder, FrameError, HEADER_LEN, MAX_BODY, Message};

    /// Returns one message of each kind, with bodies of no, few, and the most bytes.
    fn messages() -> Vec<Message> {
        vec![
            Message::Ping,
            Message::Echo(Vec::new()),
            Message::Echo(b"hello".to_vec()),
            Message::Pong,
            Message::Echo((0..=u8::MAX).cycle().take(MAX_BODY).collect()),
            Message::Stop,
            Message::Stopped,
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
    fn decode<'a>(
        decoder: &mut Decoder,
        pieces: impl IntoIterator<Item = &'a [u8]>,
    ) -> Vec<Message> {
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
        for message in messages() {
            assert_eq!(message.encode().unwrap()[HEADER_LEN - 1], message.kind());
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
        let messages = [
            Message::Echo(b"split".to_vec()),
            Message::Ping,
            Message::Stop,
        ];
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
        let cases = [
            ([0, 0, 0, 0, 0], FrameError::Kind(0)),
            ([0, 0, 0, 0, 6], FrameError::Kind(6)),
            ([9, 0, 0, 0, 0xff], FrameError::Kind(0xff)),
            ([1, 0, 0, 0, 1], FrameError::Length { kind: 1, length: 1 }),
            ([1, 0, 0, 0, 4], FrameError::Length { kind: 4, length: 1 }),
            (
                [too_long[0], too_long[1], too_long[2], too_long[3], 3],
                FrameError::Length {
                    kind: 3,
                    length: MAX_BODY + 1,
                },
            ),
            (
                [0xff, 0xff, 0xff, 0xff, 3],
                FrameError::Length {
                    kind: 3,
                    length: u32::MAX as usize,
                },
            ),
        ];
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
        assert_eq!(
            Message::Echo(vec![0; MAX_BODY + 1]).encode(),
            Err(FrameError::Length {
                kind: 3,
                length: MAX_BODY + 1,
            })
        );
    }
}
