//! A guest init's documented workflows through the public API alone: its boot's time daemon, its
//! control session, the capture of its VM, the activation of a restore, and the host's mapping
//! table.
//!
//! The example only compiles: CI builds it for Linux guests with the `guest` feature, and nothing
//! calls its functions. `nvx` builds no guest space and runs no execution: each init does both,
//! so the example leaves them to its caller. NVX's own init, nvx-init, builds its space with NVX's
//! internal reference code, whose steps compile in nvx-init's unit tests instead.
//!
//! The example's init speaks its own protocol over the session's DATA, in which one payload
//! carries one message, and it reads its session without a deadline. A real init waits for its
//! session with the activation's deadline, and ticks the activation then, and gives the
//! activation what its session received while it applied a record before it sends Ready, as
//! [`Activation`]'s documentation shows.

#[cfg(target_os = "linux")]
pub use linux::*;

fn main() {}

/// The workflows, which run in Linux guests only.
#[cfg(target_os = "linux")]
mod linux {
    use std::error::Error;
    use std::io::{self, Read, Write};
    use std::time::{Duration, Instant};

    use nvx::activation::{ActivationRecord, ReadyActivation};
    use nvx::guest::activation::{Activation, ActivationAction, ActivationInput};
    use nvx::guest::mappings::{Mapping, MappingTable, TableError};
    use nvx::guest::nvxs::{GuestConnection, GuestConnectionError};
    use nvx::guest::snapshot::{
        self, CaptureFailure, Captured, Devices, Disks, Disposition, Repaired,
    };
    use nvx::guest::time::{TimeAbi, TimeError};
    use nvx::protocol::OUTER_RECEIVE_WINDOW;
    use nvx::protocol::nvxs::GuestEvent;

    /// Starts the time daemon at boot, before anything that it must not inherit opens, and returns
    /// its process ID, which the init supervises: the VM ends when the daemon does.
    ///
    /// The daemon is the one process that `nvx` starts in a guest.
    pub fn boot() -> Result<u32, TimeError> {
        let abi = TimeAbi::new();
        let cold = abi.cold_boot()?;
        abi.start_daemon(cold)
    }

    /// Attaches the guest's side of the control session on `console`, the control console, which
    /// the init opened in raw mode.
    pub fn attach<S: Read + Write>(console: S) -> Result<GuestConnection<S>, GuestConnectionError> {
        let mut connection = GuestConnection::new(console);
        connection.attach()?;
        Ok(connection)
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

    /// Activates a restored VM on its session, once [`capture`] returned its repair.
    ///
    /// The restore queued a reset of the session that the guest attached before the capture,
    /// which the init takes and acknowledges, abandoning the earlier session's work; then the
    /// host's Activate brings the restore's own settings. `record` reads the bytes of an Activate's
    /// record from one of the init's messages, `space` builds the guest's space from the record,
    /// which is the init's own code, as nvx-init's is its own, and `ready` sends Ready in the
    /// init's protocol. Every failure is final, and the init ends the VM.
    pub fn activate<S: Read + Write>(
        connection: &mut GuestConnection<S>,
        repaired: &Repaired,
        record: impl Fn(&[u8]) -> Option<&[u8]>,
        space: impl FnOnce(&ActivationRecord) -> Result<(), Box<dyn Error + Send + Sync>>,
        mut ready: impl FnMut(&mut GuestConnection<S>, ReadyActivation) -> io::Result<()>,
    ) -> Result<Activation, Box<dyn Error>> {
        let mut activation = Activation::new(repaired, Duration::from_secs(30), Instant::now());
        let mut space = Some(space);
        loop {
            let action = match connection.next_event()? {
                GuestEvent::Reset(_) => activation.input(Instant::now(), ActivationInput::Reset)?,
                GuestEvent::Data(payload) => {
                    connection.credit(u32::try_from(payload.len())?)?;
                    let input = record(&payload).map_or(ActivationInput::Request, |record| {
                        ActivationInput::Activate(record)
                    });
                    activation.input(Instant::now(), input)?
                }
            };
            match action {
                ActivationAction::Acknowledge => connection.acknowledge(OUTER_RECEIVE_WINDOW)?,
                ActivationAction::Continue | ActivationAction::Join | ActivationAction::Serve => {}
                ActivationAction::Apply(record) => {
                    let space = space.take().ok_or("the activation applied twice")?;
                    // The activation completes, or expires past its deadline, when the application
                    // ends.
                    let applied = space(&record);
                    let established = activation.complete(Instant::now(), applied)?;
                    ready(connection, established)?;
                    return Ok(activation);
                }
                ActivationAction::Ready(established) => ready(connection, established)?,
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
