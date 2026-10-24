//! The adapter's host program: a client of the guest agent that attaches to the host's end of the
//! control session between the host and the guest, and exchanges the adapter's own
//! [framing](crate::frame) with the guest agent, through `nvx::host::nvxs` alone, over any platform
//! transport, and the driver of the [executions](crate::exec) that the host program runs over it,
//! [`Executions`].

use std::fmt;
use std::time::Instant;

use nvx::host::nvxs::{HostConnection, HostConnectionError};
use nvx::host::platform::Transport;
use nvx::protocol::CAPABILITY_LEN;
use nvx::protocol::nvxs::Identity;

use crate::frame::{BodyError, Decoder, FrameError, Message};
use crate::probe::{Facts, Probe, ProbeReply};
use crate::root::{RootConfig, RootFailure, RootReply};

mod executions;

pub use executions::{Executions, ExecutionsError, MAX_OUTPUT_FRAMES};

/// A failure of a [`Client`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ClientError {
    /// The control session failed, or its deadline passed.
    Session(HostConnectionError),
    /// A malformed message: one that the guest agent sent, or one that the host program asked to
    /// send, which the client did not send.
    Frame(FrameError),
    /// The guest agent answered a request with a body that does not decode as its kind's.
    Body(BodyError),
    /// The guest agent sent a message of a kind that only the host program sends.
    Unexpected(u8),
    /// The guest agent answered a request with a message of another kind than the request's
    /// answer.
    Reply {
        /// Kind of the request.
        request: u8,
        /// Kind of the message that answered it.
        reply: u8,
    },
    /// The guest agent answered an `Echo` with other bytes than the request's.
    Echo,
    /// An earlier failure ended the client.
    Ended,
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => error.fmt(formatter),
            Self::Frame(error) => error.fmt(formatter),
            Self::Body(error) => error.fmt(formatter),
            Self::Unexpected(kind) => write!(
                formatter,
                "the guest sent a message of kind {kind}, which only the host sends"
            ),
            Self::Reply { request, reply } => write!(
                formatter,
                "the guest answered a message of kind {request} with one of kind {reply}"
            ),
            Self::Echo => formatter.write_str("the guest echoed other bytes than the host sent"),
            Self::Ended => formatter.write_str("an earlier failure ended the client"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Session(error) => Some(error),
            Self::Frame(error) => Some(error),
            Self::Body(error) => Some(error),
            Self::Unexpected(_) | Self::Reply { .. } | Self::Echo | Self::Ended => None,
        }
    }
}

impl From<HostConnectionError> for ClientError {
    fn from(error: HostConnectionError) -> Self {
        Self::Session(error)
    }
}

impl From<FrameError> for ClientError {
    fn from(error: FrameError) -> Self {
        Self::Frame(error)
    }
}

/// The host program's client of the guest agent, over one attachment to the control session.
///
/// [`attach`](Self::attach) attaches with the launch capability. [`send`](Self::send) sends a
/// message in as many of the session's records as it takes, and [`receive`](Self::receive)
/// returns the guest agent's next message, whether it spans records or shares one with the next
/// message. [`ping`](Self::ping), [`echo`](Self::echo), [`configure_root`](Self::configure_root),
/// [`probe`](Self::probe), and [`stop`](Self::stop) each send a request and wait for its answer,
/// and [`Executions`] runs executions over the client.
///
/// Besides the session's own buffers, the client holds at most the rest of one of the guest's
/// records and one partial message, whose header [`Decoder`] checks before it takes any byte of
/// the body.
///
/// A reset of the session ends the attachment, and the partial message with it: the session's next
/// epoch starts with a new message, in a new attachment. `nvx::host::nvxs` reports a reset that
/// the broker announces as [`HostConnectionError::Reset`], and the end of the stream, with which
/// OpenVMM's broker ends an attachment instead, as [`HostConnectionError::Closed`].
///
/// A failure ends the client, and every later call fails with [`ClientError::Ended`]: a failure of
/// the session, a send that fails or times out, because the session cannot resume a message that
/// it took in part, a malformed message, and a message that only the host program sends. A
/// [`receive`](Self::receive) that times out doesn't: it keeps what arrived. A request ends the
/// client on any failure after the framing accepted it, a timeout and an answer whose body does
/// not decode included, because an answer that arrived later would answer the wrong request.
pub struct Client {
    /// The host's end of the control session.
    connection: HostConnection,
    /// Decoder of the guest agent's messages.
    decoder: Decoder,
    /// Payload of the guest's last DATA record.
    payload: Vec<u8>,
    /// Bytes of `payload` that the decoder took.
    taken: usize,
    /// Whether a failure ended the client.
    ended: bool,
}

impl Client {
    /// Attaches to the control session over `transport`, a stream to its host endpoint, with the
    /// launch `capability`, and waits until `deadline` while the broker waits for the guest.
    pub fn attach(
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        deadline: Instant,
    ) -> Result<Self, ClientError> {
        let connection = HostConnection::attach(transport, capability, deadline)?;
        Ok(Self {
            connection,
            decoder: Decoder::new(),
            payload: Vec::new(),
            taken: 0,
            ended: false,
        })
    }

    /// Returns the identity of the session's epoch that the broker attached the client to.
    pub fn identity(&self) -> Identity {
        self.connection.identity()
    }

    /// Sends `message` to the guest agent by `deadline`, or without a time limit, in as many of
    /// the session's records as it takes.
    ///
    /// A message that the framing refuses fails before anything is sent, and the client stays
    /// usable. Any other failure, a timeout included, ends the client, because the session cannot
    /// resume a message that it took in part.
    pub fn send(
        &mut self,
        message: &Message,
        deadline: Option<Instant>,
    ) -> Result<(), ClientError> {
        self.check()?;
        let encoded = message.encode()?;
        self.connection
            .send(&encoded, deadline)
            .map_err(|error| self.fail(error.into()))
    }

    /// Returns the guest agent's next message, waiting until `deadline`, or without a time limit.
    ///
    /// A receive that times out keeps what arrived, so that a later one returns the whole message.
    pub fn receive(&mut self, deadline: Option<Instant>) -> Result<Message, ClientError> {
        self.check()?;
        loop {
            if self.taken < self.payload.len() {
                let (consumed, message) = self
                    .decoder
                    .push(&self.payload[self.taken..])
                    .map_err(|error| self.fail(error.into()))?;
                self.taken += consumed;
                match message {
                    None => {}
                    Some(
                        message @ (Message::Ping
                        | Message::Stop
                        | Message::RootConfig(_)
                        | Message::Probe(_)
                        | Message::ExecStart(_)
                        | Message::ExecInput(_)
                        | Message::ExecInputEnd(_)
                        | Message::ExecCancel(_)),
                    ) => {
                        return Err(self.fail(ClientError::Unexpected(message.kind())));
                    }
                    Some(message) => return Ok(message),
                }
            }
            // The decoder took the whole payload, so the next one replaces it.
            self.payload = match self.connection.receive(deadline) {
                Ok(payload) => payload,
                Err(HostConnectionError::TimedOut) => {
                    return Err(HostConnectionError::TimedOut.into());
                }
                Err(error) => return Err(self.fail(error.into())),
            };
            self.taken = 0;
        }
    }

    /// Sends `Ping`, and waits until `deadline` for the guest agent's `Pong`.
    pub fn ping(&mut self, deadline: Instant) -> Result<(), ClientError> {
        let request = Message::Ping;
        match self.request(&request, deadline)? {
            Message::Pong => Ok(()),
            reply => Err(self.mismatched(&request, &reply)),
        }
    }

    /// Sends `body` in an `Echo`, and waits until `deadline` for the guest agent's `Echo` of the
    /// same bytes.
    ///
    /// A body of more than [`MAX_BODY`](crate::frame::MAX_BODY) bytes fails before anything is
    /// sent, and the client stays usable.
    pub fn echo(&mut self, body: &[u8], deadline: Instant) -> Result<(), ClientError> {
        let request = Message::Echo(body.to_vec());
        match self.request(&request, deadline)? {
            Message::Echo(echoed) if echoed == body => Ok(()),
            Message::Echo(_) => Err(self.fail(ClientError::Echo)),
            reply => Err(self.mismatched(&request, &reply)),
        }
    }

    /// Sends the root configuration `config` of a VM with a layered root, and waits until
    /// `deadline` for the guest agent's answer: `RootReady` once it assembled the root, or
    /// `RootFailed` with the stage that failed, after which the guest ends.
    ///
    /// A configuration whose encoding is longer than
    /// [`MAX_TEXT_BODY`](crate::frame::MAX_TEXT_BODY) bytes fails before anything is sent, and the
    /// client stays usable.
    pub fn configure_root(
        &mut self,
        config: &RootConfig,
        deadline: Instant,
    ) -> Result<RootReply, ClientError> {
        let request = Message::RootConfig(config.encode());
        match self.request(&request, deadline)? {
            Message::RootReady => Ok(RootReply::Ready),
            Message::RootFailed(body) => {
                self.decoded(RootFailure::decode(&body).map(RootReply::Failed))
            }
            reply => Err(self.mismatched(&request, &reply)),
        }
    }

    /// Sends `probe` to the guest agent of a VM with a layered root, and waits until `deadline`
    /// for its answer: the facts that the probe found, or the problem.
    pub fn probe(&mut self, probe: &Probe, deadline: Instant) -> Result<ProbeReply, ClientError> {
        let request = Message::Probe(probe.encode());
        match self.request(&request, deadline)? {
            Message::Report(body) => self.decoded(Facts::decode(&body).map(ProbeReply::Report)),
            Message::ProbeFailed(body) => {
                let problem = String::from_utf8(body)
                    .map_err(|_| BodyError::new("the probe's failure is not UTF-8 text"));
                self.decoded(problem.map(ProbeReply::Failed))
            }
            reply => Err(self.mismatched(&request, &reply)),
        }
    }

    /// Sends `Stop`, and waits until `deadline` for the guest agent's `Stopped`, its last message.
    ///
    /// The guest agent of a VM with a layered root tears the root down first, and reports a
    /// teardown that failed with `RootFailed` before `Stopped`: the client returns that failure.
    pub fn stop(&mut self, deadline: Instant) -> Result<Option<RootFailure>, ClientError> {
        let request = Message::Stop;
        let failure = match self.request(&request, deadline)? {
            Message::Stopped => return Ok(None),
            Message::RootFailed(body) => self.decoded(RootFailure::decode(&body))?,
            reply => return Err(self.mismatched(&request, &reply)),
        };
        match self
            .receive(Some(deadline))
            .map_err(|error| self.fail(error))?
        {
            Message::Stopped => Ok(Some(failure)),
            reply => Err(self.mismatched(&request, &reply)),
        }
    }

    /// Returns `decoded`, the body of an answer, or ends the client because it does not decode.
    fn decoded<T>(&mut self, decoded: Result<T, BodyError>) -> Result<T, ClientError> {
        decoded.map_err(|error| self.fail(ClientError::Body(error)))
    }

    /// Sends `request` by `deadline`, and returns the guest agent's next message. Once the framing
    /// accepted the request, any failure ends the client.
    fn request(&mut self, request: &Message, deadline: Instant) -> Result<Message, ClientError> {
        self.send(request, Some(deadline))?;
        self.receive(Some(deadline))
            .map_err(|error| self.fail(error))
    }

    /// Ends the client because the guest agent answered `request` with `reply`, of another kind.
    fn mismatched(&mut self, request: &Message, reply: &Message) -> ClientError {
        self.fail(ClientError::Reply {
            request: request.kind(),
            reply: reply.kind(),
        })
    }

    /// Fails if an earlier failure ended the client.
    fn check(&self) -> Result<(), ClientError> {
        if self.ended {
            Err(ClientError::Ended)
        } else {
            Ok(())
        }
    }

    /// Ends the client with `error`, dropping the partial message, and returns it.
    fn fail(&mut self, error: ClientError) -> ClientError {
        self.ended = true;
        self.decoder.clear();
        self.payload = Vec::new();
        self.taken = 0;
        error
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("identity", &self.identity())
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}
