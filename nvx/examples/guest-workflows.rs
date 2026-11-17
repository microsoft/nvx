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
//! guest restore's deadline, and ticks the guest restore then, as [`GuestRestore`]'s
//! documentation shows.

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
        /// Returns whether `message` is the host's request for the clone's Ready.
        fn asks_ready(&self, message: &[u8]) -> bool;
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

    /// Activates a restored VM from the record of its restore packet, and answers the host's
    /// request for its Ready on its connection, once [`capture`] returned its repair.
    ///
    /// The host supplied the guest-restore record's bytes, with no framing of its own, as the
    /// clone's activation record, which OpenVMM sealed in the restore packet before the clone's
    /// first restored vCPU ran: the init applies it at once, without waiting for the host. The
    /// restore queued a reset of the session that the guest attached before the capture, which the
    /// init takes and acknowledges, abandoning the earlier session's work; then the host asks for
    /// the clone's Ready. `framing` reads the init's messages from the connection's bytes, `space`
    /// builds the guest's space from the record, which is the init's own code, as nvx-init's is
    /// its own, and `ready` sends Ready in the init's protocol. Every failure is final, and the
    /// init ends the VM.
    pub fn activate<S: Read + Write>(
        connection: &mut Connection<S>,
        repaired: &Repaired,
        framing: &mut impl Framing,
        space: impl FnOnce(&GuestRestoreRecord) -> Result<(), Box<dyn Error + Send + Sync>>,
        mut ready: impl FnMut(&mut Connection<S>, GuestRestoreReady) -> io::Result<()>,
    ) -> Result<GuestRestore, Box<dyn Error>> {
        let mut restore = GuestRestore::new(repaired, Duration::from_secs(30), Instant::now());
        // The record is valid, and nothing happened before: the guest restore's side effects
        // start here. The guest restore completes, or expires past its deadline, when the
        // application ends, and awaits the host's request for Ready.
        let record = restore.accept(Instant::now(), repaired.activation_record())?;
        let applied = space(&record);
        restore.complete(Instant::now(), applied)?;
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
                        if framing.asks_ready(message) {
                            GuestRestoreInput::Ready
                        } else {
                            GuestRestoreInput::Request
                        }
                    })
                    .collect(),
            };
            for input in inputs {
                match restore.input(Instant::now(), input)? {
                    GuestRestoreAction::Acknowledge => connection.acknowledge()?,
                    // The record applied before the init read the session, so no request for
                    // Ready joins the guest restore.
                    GuestRestoreAction::Continue
                    | GuestRestoreAction::Join
                    | GuestRestoreAction::Serve => {}
                    GuestRestoreAction::Ready(established) => {
                        ready(connection, established)?;
                        return Ok(restore);
                    }
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
