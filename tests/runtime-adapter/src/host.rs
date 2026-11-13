//! The adapter's host program: a client of the guest agent that attaches to the host's end of the
//! control session between the host and the guest, and exchanges the adapter's own
//! [framing](crate::frame) with the guest agent, through `nvx::host::nvxs` alone, over any platform
//! transport, and the driver of the [executions](crate::exec) that the host program runs over it,
//! [`Executions`].
//!
//! # The host's order
//!
//! A VM with a layered root is ready for workloads once its guest assembled the root and applied
//! the host's mapping table. The host runs that order over a [`Client`] in two steps, between
//! which it may do what the root's readiness allows, such as releasing the files that it handed
//! the VMM:
//!
//! 1. [`ready`] sends the root configuration, and waits for the guest agent's `Ready`, whose
//!    build must be the one that the host ships: the host sends a guest agent of another build
//!    nothing more, since it cannot tell what that build would do with it.
//! 2. [`start_sandbox`] sends the mapping table, in order, in parts that the guest applies one
//!    after the other, then `Start`, which the guest confirms with `Started`.
//!
//! A part that the guest fails ends the order: the host sends no more of the table, nor `Start`,
//! so the guest starts no workload on a partial table, and asks the guest to stop, which tears
//! its root down, as NVX's managed lifecycle stops a guest whose mappings failed. The caller then
//! waits for the VM to end, and ends a VM whose guest did not stop, as it ends one after any other
//! failure of the order, an [`OrderError`].
//!
//! # A clone's order
//!
//! A clone, which a restore of a template's snapshot started, takes the host's activation of its
//! record instead of a root configuration: the guest assembles the clone's root from the record,
//! with the inputs that the template's capture kept, as it activates the clone. Its first step is
//! [`activate`], which sends the clone's activation record in an `Activate`, the first request of
//! the session, and waits for the guest agent's `Ready`, whose build it checks as [`ready`] does,
//! and which reports the clone's activation, for the host to check against the record. Then the
//! restore sequence sends the clone's mapping table, which [`map_table`] sends as [`start_sandbox`]
//! does after a cold boot, and `Start`. [`CloneOrder`] runs these steps as the adapter's framing of
//! `nvx::host::template`'s restore sequence, which checks the activation, orders the steps, and
//! which replaces a session that the order lost, as [`CloneOrder`] tells, while the clone was not
//! ready or its `Ready` had not arrived: the clone answers the same record, byte for byte, with the
//! same `Ready` in a new session, and has ended once the reset of a session that it took before it
//! was ready failed its activation.

mod executions;
mod map_reply;
mod order_error;

use std::fmt;
use std::time::Instant;

use nvx::activation::ReadyActivation;
use nvx::host::nvxs::{HostConnection, HostConnectionError};
use nvx::host::platform::Transport;
use nvx::host::template::CloneControl;
use nvx::protocol::nvxs::Identity;
use nvx::protocol::{CAPABILITY_LEN, HostMap};

use crate::frame::{BodyError, Decoder, FrameError, Message};
use crate::probe::{Facts, Probe, ProbeReply};
use crate::root::{MapFailure, Ready, RootConfig, RootFailure, RootReply, encode_maps};

pub use executions::{Executions, ExecutionsError, MAX_OUTPUT_FRAMES};
pub use map_reply::MapReply;
pub use order_error::OrderError;

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
/// [`activate`](Self::activate), [`map`](Self::map), [`start`](Self::start),
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
                        | Message::Maps(_)
                        | Message::Start
                        | Message::ExecStart(_)
                        | Message::ExecInput(_)
                        | Message::ExecInputEnd(_)
                        | Message::ExecCancel(_)
                        | Message::Activate(_)),
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
    /// `deadline` for the guest agent's answer: `Ready`, with the guest agent's build, once it
    /// assembled the root, or `RootFailed` with the stage that failed, after which the guest ends.
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
            Message::Ready(body) => self.decoded(Ready::decode(&body).map(RootReply::Ready)),
            Message::RootFailed(body) => {
                self.decoded(RootFailure::decode(&body).map(RootReply::Failed))
            }
            reply => Err(self.mismatched(&request, &reply)),
        }
    }

    /// Sends `record`, the activation record of a clone, which a restore of a template's snapshot
    /// started, in its canonical encoding, in an `Activate`, and waits until `deadline` for the
    /// guest agent's answer: `Ready`, with the guest agent's build and the activation that it
    /// reports, once it activated the clone. A clone whose activation failed answers nothing, but
    /// ends, and its session with it.
    ///
    /// A record of more than [`MAX_ACTIVATE_BODY`](crate::frame::MAX_ACTIVATE_BODY) bytes fails
    /// before anything is sent, and the client stays usable.
    pub fn activate(&mut self, record: &[u8], deadline: Instant) -> Result<Ready, ClientError> {
        let request = Message::Activate(record.to_vec());
        match self.request(&request, deadline)? {
            Message::Ready(body) => self.decoded(Ready::decode(&body)),
            reply => Err(self.mismatched(&request, &reply)),
        }
    }

    /// Sends `part`, a part of the host's mapping table as [`encode_maps`] encodes it, in a
    /// `Maps`, and waits until `deadline` for the guest agent's answer: `Mapped` once it applied the
    /// part's mappings, or `MapFailed` once it refused the part or failed to apply one of them,
    /// after which it refuses the rest of the table and the start.
    ///
    /// A part of more than [`MAX_BODY`](crate::frame::MAX_BODY) bytes fails before anything is
    /// sent, and the client stays usable.
    pub fn map(&mut self, part: &[u8], deadline: Instant) -> Result<MapReply, ClientError> {
        let request = Message::Maps(part.to_vec());
        match self.request(&request, deadline)? {
            Message::Mapped => Ok(MapReply::Mapped),
            Message::MapFailed(body) => {
                self.decoded(MapFailure::decode(&body).map(MapReply::Failed))
            }
            reply => Err(self.mismatched(&request, &reply)),
        }
    }

    /// Sends `Start`, and waits until `deadline` for the guest agent's `Started`, with which it
    /// confirms that it started the sandbox, once it applied every mapping that the root
    /// configuration announced.
    pub fn start(&mut self, deadline: Instant) -> Result<(), ClientError> {
        let request = Message::Start;
        match self.request(&request, deadline)? {
            Message::Started => Ok(()),
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
        self.stop_skipping(deadline, |_| false)
    }

    /// Stops the guest agent as [`stop`](Self::stop) does, but skips the guest's messages before
    /// `Stopped` that `late` accepts: those that the guest may still send, in this attachment,
    /// about executions that a driver finished without them, as [`Executions::is_about`] accepts
    /// them. Any other message answers the `Stop`, as it does for [`stop`](Self::stop).
    pub fn stop_skipping(
        &mut self,
        deadline: Instant,
        mut late: impl FnMut(&Message) -> bool,
    ) -> Result<Option<RootFailure>, ClientError> {
        let request = Message::Stop;
        self.send(&request, Some(deadline))?;
        let mut failure = None;
        loop {
            match self
                .receive(Some(deadline))
                .map_err(|error| self.fail(error))?
            {
                Message::Stopped => return Ok(failure),
                Message::RootFailed(body) if failure.is_none() => {
                    failure = Some(self.decoded(RootFailure::decode(&body))?);
                }
                reply if late(&reply) => {}
                reply => return Err(self.mismatched(&request, &reply)),
            }
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

/// Runs the first step of the host's order of a VM with a layered root over `client`: sends the
/// root configuration `config`, and waits until `deadline` for the guest agent's `Ready`, which it
/// returns once it names `build`, the guest agent's build that the host ships.
///
/// Fails with [`OrderError::Root`] once the guest reports that a stage of the root failed, with
/// [`OrderError::Build`] for a guest agent of another build, to which it sends nothing more, and
/// with [`OrderError::Client`] once the client fails.
pub fn ready(
    client: &mut Client,
    config: &RootConfig,
    build: &str,
    deadline: Instant,
) -> Result<Ready, OrderError> {
    match client
        .configure_root(config, deadline)
        .map_err(OrderError::Client)?
    {
        RootReply::Ready(ready) if ready.build() == build => Ok(ready),
        RootReply::Ready(ready) => Err(OrderError::Build {
            reported: ready.build().to_owned(),
            expected: build.to_owned(),
        }),
        RootReply::Failed(failure) => Err(OrderError::Root(failure)),
    }
}

/// Runs the rest of the host's order over `client`, once the guest agent is [`ready`]: sends the
/// mapping table `table`, which the root configuration announced, with [`map_table`], then
/// `Start`, and waits until `deadline` for `Started`. A table of no mapping sends no part.
///
/// Fails with [`OrderError::Table`], before it sends anything, for a table that does not encode;
/// with [`OrderError::Mapping`] once the guest fails a part, after which it sends no more of the
/// table, nor `Start`, but asks the guest to stop; and with [`OrderError::Client`] once the client
/// fails.
pub fn start_sandbox(
    client: &mut Client,
    table: &[HostMap],
    deadline: Instant,
) -> Result<(), OrderError> {
    map_table(client, table, deadline)?;
    client.start(deadline).map_err(OrderError::Client)
}

/// Sends the mapping table `table` over `client`, once the guest agent is ready, in the parts that
/// [`encode_maps`] splits it into, each once the guest applied the one before, until `deadline`.
/// A table of no mapping sends no part.
///
/// Fails as [`start_sandbox`] does before its `Start`: with [`OrderError::Table`], before it sends
/// anything, for a table that does not encode; with [`OrderError::Mapping`] once the guest fails a
/// part, after which it sends no more of the table, but asks the guest to stop; and with
/// [`OrderError::Client`] once the client fails.
pub fn map_table(
    client: &mut Client,
    table: &[HostMap],
    deadline: Instant,
) -> Result<(), OrderError> {
    let parts = encode_maps(table).map_err(OrderError::Table)?;
    for part in &parts {
        match client.map(part, deadline).map_err(OrderError::Client)? {
            MapReply::Mapped => {}
            MapReply::Failed(failure) => {
                // The guest starts no workload on a partial table, and tears its root down as it
                // stops, which unmounts the mappings that it applied.
                let stop = client.stop(deadline);
                return Err(OrderError::Mapping { failure, stop });
            }
        }
    }
    Ok(())
}

/// Runs the first step of [a clone's order](self#a-clones-order) over `client`, in place of
/// [`ready`]: sends `record`, the clone's activation record in its canonical encoding, in an
/// `Activate`, which must be the first request of the client's attachment, and waits until
/// `deadline` for the guest agent's `Ready`, which it returns once it names `build`, the guest
/// agent's build that the host ships, with the activation that it reports, which the caller checks
/// against the record.
///
/// Fails with [`OrderError::Build`] for a guest agent of another build, to which it sends nothing
/// more, and with [`OrderError::Client`] once the client fails: as the session ends once the
/// clone's activation failed, or a reset ended the attachment.
pub fn activate(
    client: &mut Client,
    record: &[u8],
    build: &str,
    deadline: Instant,
) -> Result<Ready, OrderError> {
    let ready = client
        .activate(record, deadline)
        .map_err(OrderError::Client)?;
    if ready.build() != build {
        return Err(OrderError::Build {
            reported: ready.build().to_owned(),
            expected: build.to_owned(),
        });
    }
    Ok(ready)
}

/// [A clone's order](self#a-clones-order), as the adapter's framing of the clone's control
/// session, through which `nvx::host::template`'s restore sequence activates the clone and starts
/// its sandbox: it attaches a [`Client`] over each connection to the clone's control endpoint,
/// runs [`activate`] with the guest agent's build that the host ships, then [`map_table`] with the
/// mapping table that the restore sequence gives it, which the clone's record announces, and
/// `Start`.
///
/// It takes a session for lost once a reset of the broker, the end of the broker's stream, or a
/// failure of the transport ended it, while the client attached or awaited the clone's `Ready`:
/// the clone may answer the same record in a new session. OpenVMM's broker ends the stream both to
/// end an attachment and to refuse one, as it refuses a host whose authentication outlasts the
/// broker's deadline, so the order takes such a refusal for a lost session too, whose retries the
/// restore sequence's limit of sessions bounds. Any other failure is final: a refusal that the
/// broker sends, a deadline that passed, a guest agent of another build, and a message that breaks
/// the framing.
///
/// It takes no failure for the guest's refusal of the request's mapping table, which
/// [`CloneControl::refused`] tells: the guest's `MapFailed` does not tell whether the request's
/// mappings or a failure of the guest's own failed the table, so the restore sequence attributes
/// it, as any other failure of the table, to what the host saw of it, a broken protocol, unless
/// the deadline passed or the session was lost.
#[derive(Clone, Copy, Debug)]
pub struct CloneOrder<'a> {
    /// The guest agent's build that the host ships.
    pub build: &'a str,
}

impl CloneControl for CloneOrder<'_> {
    type Session = Client;
    type Error = OrderError;

    fn attach(
        &mut self,
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        deadline: Instant,
    ) -> Result<Client, OrderError> {
        Client::attach(transport, capability, deadline).map_err(OrderError::Client)
    }

    fn activate(
        &mut self,
        session: &mut Client,
        record: &[u8],
        deadline: Instant,
    ) -> Result<Option<ReadyActivation>, OrderError> {
        activate(session, record, self.build, deadline).map(|ready| ready.activation())
    }

    fn map(
        &mut self,
        session: &mut Client,
        table: &[HostMap],
        deadline: Instant,
    ) -> Result<(), OrderError> {
        map_table(session, table, deadline)
    }

    fn start(&mut self, session: &mut Client, deadline: Instant) -> Result<(), OrderError> {
        session.start(deadline).map_err(OrderError::Client)
    }

    fn lost(&self, error: &OrderError) -> bool {
        matches!(
            error,
            OrderError::Client(ClientError::Session(
                HostConnectionError::Reset(_)
                    | HostConnectionError::Closed
                    | HostConnectionError::Io(_)
            ))
        )
    }
}
