//! A guest init's documented workflows through the public API alone: its boot's time daemon, its
//! control session, the capture of its VM, the guest restore of a clone, and the host's mapping
//! table.
//!
//! The example only compiles: CI builds it for Linux guests with the `guest` feature, and nothing
//! calls its functions. `nvx` builds no guest space and runs no execution: each init does both,
//! so the example leaves them to its caller. NVX's own init, nvx-init, builds its space with NVX's
//! internal reference code, whose steps compile in nvx-init's unit tests instead.
//!
//! The example's init frames its own protocol's messages in the bytes of its control connection,
//! and it reads its connection without a deadline. A real init waits for its connection with the
//! guest restore's deadline, and ticks the guest restore then, and gives the guest restore what
//! its connection received while it applied a record before it sends Ready, as
//! [`GuestRestore`]'s documentation shows.

#[cfg(target_os = "linux")]
pub use linux::*;

fn main() {}

/// The workflows, which run in Linux guests only.
#[cfg(target_os = "linux")]
mod linux {
    use std::error::Error;
    use std::io::{self, Read, Write};
    use std::time::{Duration, Instant};

    use nvx::guest::control::{Connection, ConnectionError, Event};
    use nvx::guest::mappings::{Mapping, MappingTable, TableError};
    use nvx::guest::restore::{GuestRestore, GuestRestoreAction, GuestRestoreInput};
    use nvx::guest::snapshot::{
        self, CaptureFailure, Captured, Devices, Disks, Disposition, Repaired,
    };
    use nvx::guest::time::{TimeAbi, TimeError};
    use nvx::guest_restore::{GuestRestoreReady, GuestRestoreRecord};

    /// Starts the time daemon at boot, before anything that it must not inherit opens, and returns
    /// its process ID, which the init supervises: the VM ends when the daemon does.
    ///
    /// The daemon is the one process that `nvx` starts in a guest.
    pub fn boot() -> Result<u32, TimeError> {
        let abi = TimeAbi::new();
        let cold = abi.cold_boot()?;
        abi.start_daemon(cold)
    }

    /// Attaches the guest's end of the control connection on `console`, the control console,
    /// which the init opened in raw mode.
    pub fn attach<S: Read + Write>(console: S) -> Result<Connection<S>, ConnectionError> {
        Connection::attach(console)
    }

    /// The framing of the init's own protocol, which carries its messages in the connection's
    /// bytes, and is the init's own code.
    pub trait Framing {
        /// Abandons the bytes of the earlier session that complete no message.
        fn reset(&mut self);
        /// Takes the next bytes that the connection received, and returns the messages that they
        /// complete, in order.
        fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>>;
        /// Returns the bytes of the guest-restore record of `message`, if it is the host's Activate.
        fn record<'a>(&self, message: &'a [u8]) -> Option<&'a [u8]>;
    }

    /// What the init does once [`capture`] returned.
    pub enum AfterCapture {
        /// The guest goes on, without a snapshot, or after a failure that left it running: the
        /// init reports either on the same session.
        Continue(Option<CaptureFailure>),
        /// A restore started this guest, and repaired it: the init activates it with
        /// [`activate`].
        Activate(Repaired),
        /// The guest cannot go on: the init starts nothing more, and ends the VM with this
        /// status.
        End(u8),
    }

    /// Captures the VM once the host asked for a snapshot and the init answered the request.
    ///
    /// The init keeps its executions still, and its session sends nothing until `capture`
    /// returns.
    pub fn capture(devices: &Devices, deadline: Instant) -> AfterCapture {
        match snapshot::capture(devices, Disks::Fresh, deadline) {
            Ok(Captured::Continued) => AfterCapture::Continue(None),
            Ok(Captured::Restored(repaired)) => AfterCapture::Activate(repaired),
            Err(failure) => match failure.disposition {
                Disposition::Resumed => AfterCapture::Continue(Some(failure)),
                Disposition::MustEnd { status } => AfterCapture::End(status),
                // A disposition of a later release: the guest cannot tell that it may go on.
                _ => AfterCapture::End(nvx::protocol::status::TIME_REPAIR),
            },
            // An outcome of a later release: the guest cannot tell that it may go on.
            Ok(_) => AfterCapture::End(nvx::protocol::status::TIME_REPAIR),
        }
    }

    /// Activates a restored VM on its connection, once [`capture`] returned its repair.
    ///
    /// The restore queued a reset of the session that the guest attached before the capture,
    /// which the init takes and acknowledges, abandoning the earlier session's work; then the
    /// host's Activate brings the restore's own settings. `framing` reads the init's messages from
    /// the connection's bytes, `space` builds the guest's space from the record, which is the
    /// init's own code, as nvx-init's is its own, and `ready` sends Ready in the init's protocol.
    /// Every failure is final, and the init ends the VM.
    pub fn activate<S: Read + Write>(
        connection: &mut Connection<S>,
        repaired: &Repaired,
        framing: &mut impl Framing,
        space: impl FnOnce(&GuestRestoreRecord) -> Result<(), Box<dyn Error + Send + Sync>>,
        mut ready: impl FnMut(&mut Connection<S>, GuestRestoreReady) -> io::Result<()>,
    ) -> Result<GuestRestore, Box<dyn Error>> {
        let mut restore = GuestRestore::new(repaired, Duration::from_secs(30), Instant::now());
        let mut space = Some(space);
        loop {
            let messages = match connection.receive()? {
                Event::Reset => {
                    framing.reset();
                    None
                }
                Event::Data(bytes) => Some(framing.push(&bytes)),
            };
            let inputs: Vec<_> = match &messages {
                None => vec![GuestRestoreInput::Reset],
                Some(messages) if messages.is_empty() => vec![GuestRestoreInput::Flow],
                Some(messages) => messages
                    .iter()
                    .map(|message| {
                        let record = framing.record(message);
                        record.map_or(GuestRestoreInput::Request, GuestRestoreInput::Activate)
                    })
                    .collect(),
            };
            let mut inputs = inputs.into_iter();
            while let Some(input) = inputs.next() {
                match restore.input(Instant::now(), input)? {
                    GuestRestoreAction::Acknowledge => connection.acknowledge()?,
                    GuestRestoreAction::Continue
                    | GuestRestoreAction::Join
                    | GuestRestoreAction::Serve => {}
                    GuestRestoreAction::Apply(record) => {
                        let space = space.take().ok_or("the activation applied twice")?;
                        // The guest restore completes, or expires past its deadline, when the
                        // application ends.
                        let applied = space(&record);
                        // The messages after the Activate came before Ready: a retry of the
                        // Activate joins the application, and anything else fails it.
                        let mut joined = 0;
                        for input in inputs {
                            if restore.input(Instant::now(), input)? == GuestRestoreAction::Join {
                                joined += 1;
                            }
                        }
                        let established = restore.complete(Instant::now(), applied)?;
                        // Ready answers the Activate, and each retry that joined its application.
                        for _ in 0..=joined {
                            ready(connection, established)?;
                        }
                        return Ok(restore);
                    }
                    GuestRestoreAction::Ready(established) => ready(connection, established)?,
                }
            }
        }
    }

    /// Applies one part of the host's mapping table, in order, and returns whether the table is
    /// complete, after which the init may start workloads, and not before.
    ///
    /// `mount` binds one mapping beneath the init's root, from the host's export, which is the
    /// init's own code. A mapping that fails latches the table, which refuses every later part.
    pub fn map(
        table: &mut MappingTable,
        part: &[u8],
        mut mount: impl FnMut(&Mapping) -> io::Result<()>,
    ) -> Result<bool, TableError<io::Error>> {
        table.apply(part, |mappings| {
            mappings
                .iter()
                .enumerate()
                .try_for_each(|(index, mapping)| mount(mapping).map_err(|error| (index, error)))
        })?;
        Ok(table.is_complete())
    }
}
