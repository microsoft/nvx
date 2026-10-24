//! The adapter's worker: the guest agent of a VM with a layered root. It assembles the root through
//! `nvx::guest::root` in a process of its own, never PID 1, and serves the host program until the
//! host stops it.
//!
//! # PID 1 and the worker
//!
//! In a VM with a layered root, NVX's `/init` runs `/sbin/nvx-init-agent`, which the adapter's
//! initramfs replaces with `runtime-adapter-guest init /dev/hvc2 layered`; see the host side's
//! `initramfs` module. That PID 1, [`supervise`], stays minimal:
//!
//! 1. It starts an unrelated child, `runtime-adapter-guest idle`, which stands for a daemon that a
//!    guest starts before its agent, and which the worker leaves as it is.
//! 2. It starts the worker, `runtime-adapter-guest worker /dev/hvc2 SIBLING`, with the process ID
//!    of the unrelated child, which the worker's probes check.
//! 3. It collects the status of every child, adopted processes included, whatever signal ended
//!    them, and goes on past the stops of the threads that it traces, until the worker ends, and
//!    then ends the VM with the worker's status, through NVX's `/sbin/nvx-exit`.
//!
//! The worker, [`work`], runs the session in this order:
//!
//! 1. It opens the tty and attaches the session.
//! 2. It receives the host's root configuration, a [`RootConfig`], and checks it.
//! 3. It reads the boot layout of the root from the kernel command line with
//!    `nvx::protocol::root::RootLayout::parse`, the executions' hostname, and the host export, if
//!    the command line names one, with `nvx::protocol::export::HostExport::parse`, then bootstraps
//!    the guest, assembles the root, and mounts the export out of the root. If the configuration
//!    names a workload identity for the whole sandbox, it then checks the identity's account in the
//!    assembled root, in existing-account mode, through `nvx::guest::entry`.
//! 4. It answers `Ready`, with the guest agent's build, [`GUEST_BUILD`], or `RootFailed` with the
//!    stage that failed, and then ends.
//! 5. It applies the host's mapping table beneath the root, from the export, in the parts that
//!    the host sends, and answers each part with `Mapped` or `MapFailed`.
//! 6. On `Start`, once it applied every mapping that the configuration announced, it starts the
//!    sandbox, which it confirms with `Started`.
//! 7. It serves `Ping`, `Echo`, and `Probe`, whose probes the [`probes`], [`mapping`], and
//!    [`entries`] modules run, and, once the sandbox started, runs the host's executions. It runs
//!    a probe only while it runs no execution, and otherwise answers it with `ProbeFailed` and
//!    [`EXECUTING`].
//! 8. On `Stop`, it drops its executions, which ends their programs, tears the root down, reports
//!    a teardown that failed with `RootFailed`, confirms the stop with `Stopped`, and ends.
//!
//! Before the root configuration, it accepts only the configuration, or a `Stop`, which ends it
//! without a root. It assembles an EROFS image, or the layers of a GPT disk, whose first partition
//! holds at most the bound of the configuration, over an ext4 scratch or over RAM, in the overlay
//! mode that the configuration names. At the layout stage, it refuses a configuration that
//! configures a scratch for a root over RAM, bounds a first partition for an EROFS image, or
//! announces mappings while the kernel command line names no host export, from which they come,
//! and a command line whose tokens of the export break their rules. An export that fails to mount
//! fails the assembly. The executions' hostname is the value of the kernel command line's first
//! `nvx_hostname=` parameter, which NVX's sandbox plans configure, and which NVX's init agent reads
//! too, or `nvx-sandbox`, `nvx::guest::entry::DEFAULT_HOSTNAME`, without one, or with an empty one.
//! A hostname of more than 64 bytes fails the layout stage. A workload identity whose account the
//! assembled root lacks fails the identity stage.
//!
//! The worker, and with it the VM, ends with 0 once the host stopped it, after a teardown that
//! succeeded, with the stage's [exit status](RootStage::exit_status) once a stage failed, from 11
//! for the configuration to 16 for the identity, and with [`SESSION_STATUS`] once its session
//! failed or the broker closed it. Once the broker closed the stream at a record boundary, which
//! may close only the broker's end, the worker drops its executions, and writes what it queued,
//! as once the host stopped it.
//!
//! # The mapping table and the start
//!
//! The root configuration announces how many mappings the host's mapping table holds, and the host
//! sends the table once the root is ready, in parts, each in a `Maps`, in order. The worker
//! applies each part's mappings beneath the root, in order, from the host export, through
//! [`RootOperations::map`], which runs `nvx::guest::root::Root::map`, and answers the part with
//! `Mapped`, or with `MapFailed`, whose [`MapFailure`](crate::root::MapFailure) holds how many of
//! the table's mappings it applied:
//!
//! - It refuses a part as a whole, at the mapping that it expects next, if the part does not
//!   decode, or does not continue the table: if its first mapping is not the one that the worker
//!   expects next, or its last is past the count that the configuration announced.
//! - A mapping that the root refuses or fails to apply latches the root, which keeps the mappings
//!   before it, so that the count is that mapping's index in the table.
//! - Once it answered `MapFailed`, the worker refuses every later part, with `MapFailed`, and the
//!   start: it starts no workload on a partial table, and the host tears the VM down.
//!
//! A table of zero mappings does no work: the host sends no part, and the worker maps nothing,
//! though it mounts the export if the command line names one.
//!
//! `Start` starts the sandbox once the worker applied every mapping that the configuration
//! announced, and none failed, which the worker confirms with `Started`. Only then does it run the
//! host's executions, and it takes no part of the table any more. A message about an execution
//! before the start, a `Start` before every announced mapping is applied, once the table failed,
//! or once the sandbox started, and a `Maps` once the sandbox started end the session with
//! [`ServeError::Unexpected`], as a message that the worker does not take in its state does. The
//! worker serves pings, echoes, probes, and the stop before the start and after it.
//!
//! # Executions
//!
//! Once the host started the sandbox, the worker runs the host's [executions](crate::exec),
//! several at once, each through NVX's execution engine, whose runner the guest binding of
//! `nvx::guest::exec` binds to the program, with the limits and the timers that its start names,
//! such as frame-counted credit and one output pool that standard output and error share. One
//! loop that never blocks serves the session and every execution: it takes the host's messages as
//! they arrive, steps every execution, queues the executions' messages for the host, and then
//! waits until the session's stream or an execution's descriptor polls ready, or an execution's
//! deadline passes. So a program's blocked stream, an exhausted window, a stalled sink of the
//! host, or a full stream to the host holds back only the bytes that wait on it, never a timer, a
//! cancellation, a reset, a stop, or another execution. Nor does a probe, which the worker refuses
//! while it runs executions. While more than a MiB of its records waits for the stream, the
//! executions take no more output from their programs, and go on otherwise, and the worker still
//! takes the host's messages, until more than 4 MiB wait, which only the replies to the host's
//! messages and the executions' messages other than output can queue: a host that sends messages
//! without reading the replies holds the worker's memory to that. The executions take turns at the
//! room on the stream: in a turn of the loop, each reads at most an eighth of that MiB of output,
//! and the loop serves another execution first in each turn, so that a program that always has
//! output, with all the credit that it needs, never keeps the others' output from the host.
//!
//! - The host's messages reach an execution by its ID, and its runner sorts them by generation:
//!   one of an earlier generation is stale, and one of an ID that the worker no longer runs, as
//!   after the execution's terminal record, is late. Neither changes anything. The worker also
//!   remembers the latest generation that the host started of up to [`MAX_HISTORY`] IDs, those
//!   of the starts that it refused included, and forgets the ID that it heard of first beyond
//!   that, so that a start of an earlier generation is stale even after the execution's terminal
//!   record. A message that the engine refuses, a body that does not decode, or a second start
//!   of the same execution ends the session with [`ServeError::Execution`].
//! - A start of a later generation abandons the execution of the same ID, as a reset of the
//!   session abandons every execution: the worker drops it, and it runs until it ends, detached
//!   from the host, which receives nothing more about it.
//! - The worker admits at most [`MAX_EXECUTIONS`] executions at once, abandoned ones included,
//!   and those that keep their places once they ended, as the items below describe, and none
//!   that would hold more than [`MAX_EXECUTION_MEMORY`] bytes. It refuses a start beyond those,
//!   one whose identity or entry it refuses, as the next item describes, or one whose cgroup it
//!   cannot prepare, or whose program it cannot spawn, with a terminal record whose launch failed
//!   at stage 0, with `EAGAIN`, `ENOMEM`, the `errno` of the refusal, or that of the preparation
//!   or the spawn.
//! - Each program enters the assembled root through `nvx::guest::entry`, as an [`Entrance`]
//!   describes it: in new mount, PID, and UTS namespaces, with the root moved over `/`, the
//!   configured hostname, the image's own machine ID, and no time status, so that the entry binds
//!   nothing into the root, and with published listeners, the entry's default. It runs as the
//!   identity that exactly one source names: the sandbox's workload identity, which the root
//!   configuration names, with no supplementary groups or capabilities, or, in a sandbox without
//!   one, the identity that its start names, with its supplementary groups, its capabilities, and
//!   its account mode. A start that names an identity in a sandbox with a workload identity, or
//!   none in a sandbox without one, is refused with `EINVAL`. Before the worker prepares the
//!   execution's cgroup, the entry checks the identity, and reads its account in
//!   existing-account mode: it refuses with `EPERM` a capability outside its allowlist, any
//!   capability of a user ID other than 0, and `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` while
//!   listeners are published, with `ENOENT` an identity without the user ID's passwd entry or
//!   the group ID's group entry, with the error of an account database or a root that it cannot
//!   read, and with `EINVAL` otherwise. The program starts in the working directory of its start,
//!   or in the root's `/`, with exactly the environment of its start, and leads a session of its
//!   own, without a controlling terminal, and the process group that the session creates. As PID
//!   1 of its PID namespace, its end ends every process of the namespace.
//! - Each execution runs in a cgroup of its own below the workload cgroup, `execution-N`, where N
//!   counts the cgroups that the worker prepared. The worker prepares it with
//!   `nvx::guest::cgroup::ExecutionCgroup::prepare`, and the program joins it as it spawns,
//!   before it runs, so that its descendants start in it, however they leave its process group or
//!   session. The execution settles through that cgroup alone, which leaves the other executions
//!   running: once the program ended, as its status, which the execution collects itself,
//!   tells, the execution kills what is left in the cgroup, and settles once the cgroup is empty.
//!   So no descendant outlives the program or holds its output open, and an execution whose
//!   program ends on its own loses nothing, however slowly the host takes its output, and is
//!   reusable.
//! - An execution keeps its place among [`MAX_EXECUTIONS`] until its cgroup is empty and the
//!   worker removed it, which the worker does once the runner returned the terminal outcome of an
//!   execution that settled. One that did not settle, whose outcome is fenced, keeps its place
//!   and its cgroup until the host stops the worker, as does a start whose program the worker
//!   spawned, but whose execution it could not have: until the worker collected the status of
//!   such a program, the program may still join the cgroup, so that a removal could hide it once
//!   it ended.
//! - Without cgroups, as where the in-process tests cannot create them, the [root's
//!   operations](RootOperations::cgroups) name none, and the program's process group stands in
//!   for its cgroup: its execution kills the group as a whole. The worker collects the status of
//!   every program itself then, without waiting for it, and learns that a program ended from a
//!   pidfd of its own. Before it collects the status, it kills what is left of the program's
//!   process group, as the settlement through a cgroup kills what is left in the cgroup: until
//!   then, the program, a zombie, keeps its process ID, which is the group's, from any other
//!   process. So a descendant that stayed in the group neither outlives the program nor holds its
//!   output open. Nothing proves that a descendant that left the group is gone, though, so each
//!   outcome is fenced: once the program ended, a stream that such a process may still hold has
//!   the settlement time and the grace to close, after which the runner gives up on it, while a
//!   stream that no process holds any more loses nothing, however slowly the host takes it. Such
//!   an execution keeps its place until the worker collected its program's status.
//! - Before it hands an execution the host's cancellation, the worker gives the execution a turn,
//!   as the loop does, which collects the status of a program that ended, and reports the
//!   program's end to the runner. So the end of a program that ended before its timeout while the
//!   worker did not run, as when the guest's processor did not, stays the program's own, without
//!   the cause of a timeout or of the cancellation, even if the cancellation comes after the
//!   timeout's instant.
//! - The worker forgets an execution once its runner returned the terminal outcome, which may
//!   come before the program's end, as once the runner gave up on a program that a kill did not
//!   end in time: forgetting it kills the program, and what is left in its cgroup or its process
//!   group, again, and the worker collects the program's status once it ended. On a stop, or
//!   once its session ended otherwise, the worker drops every execution, which kills the program
//!   of each one that has not ended, with what is left in its cgroup or its process group, and
//!   what is left in the cgroups of the executions that did not settle, and waits up to a second
//!   in all for those programs to end and those cgroups to empty, removing each cgroup that
//!   emptied once it collected the statuses of those programs.

mod connection;
pub mod entries;
mod executions;
pub mod mapping;
mod order;
pub mod probes;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{self, PollFd, PollFlags, PollTimeout};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{self, Id, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use nvx::gpt::Expected;
use nvx::guest::cgroup::Layout;
use nvx::guest::entry::{self, Entry, Identity, Prepared};
use nvx::guest::mappings::{Mapping, SourceRoot};
use nvx::guest::nvxs::GuestConnectionError;
use nvx::guest::process::Child;
use nvx::guest::root::{
    self, Assembly, Bootstrap, Device, Image, Journal, Resources, Root, Upper, UpperKind,
};
use nvx::guest::spawn::{self, Spawned};
use nvx::protocol::export::HostExport;
use nvx::protocol::root::{
    DISTRO_MMIO_BASE, ImageFormat, RootLayout, RootUpper, SCRATCH_MMIO_BASE,
};

use crate::GUEST_BUILD;
use crate::exec::HostMessage;
use crate::frame::{MAX_BODY, MAX_TEXT_BODY, Message, truncated};
use crate::guest::{self, ServeError, Step};
use crate::probe::{Facts, Probe};
use crate::root::{
    DEFAULT_FIRST_PARTITION_BOUND, JournalPolicy, OverlayMode, Ready, RootConfig, RootFailure,
    RootStage,
};
use connection::{Connection, Received};
use executions::{Executions, Sandbox};
use order::Order;

/// The status with which the worker ends once its session failed or the broker closed it, and
/// with which PID 1 ends the VM if a child cannot start or the worker's status cannot be collected.
pub const SESSION_STATUS: u8 = 1;

/// The problem with which the worker answers a probe while it runs executions.
pub const EXECUTING: &str = "the worker runs executions, and probes nothing until they end";

/// The most executions that the worker runs at once, those that the host abandoned included, and
/// those that ended but keep their places: with cgroups, those whose cgroups the worker has not
/// removed, as those of the executions that did not settle, and without them, those whose
/// programs the worker forgot before it collected their statuses.
pub const MAX_EXECUTIONS: usize = 8;

/// The most bytes that one execution may hold in the worker: what its runner holds, a chunk of
/// each output stream and the input's window. The output that waits for the session's stream
/// does not count: the worker bounds it for every execution at once, as the [module
/// documentation](self) describes.
pub const MAX_EXECUTION_MEMORY: usize = 1024 * 1024;

/// The most execution IDs whose latest generation the worker remembers, so that it tells a stale
/// or second start of an execution from a new one even after the execution's terminal record.
pub const MAX_HISTORY: usize = 1024;

/// The kernel command line, from which the worker reads the boot layout of the root.
const COMMAND_LINE: &str = "/proc/cmdline";

/// The kernel command line's parameter that names the executions' hostname, as NVX's init agent
/// reads it.
const HOSTNAME_PARAMETER: &[u8] = b"nvx_hostname=";

/// How one execution enters the assembled root: the inputs of its `nvx::guest::entry::Entry` that
/// the worker chooses. The entry keeps its defaults for the rest: new mount, PID, and UTS
/// namespaces, with the root moved over `/`, the image's own machine ID and no time status, which
/// leave `Entry::bind_runtime` nothing to bind, published listeners, which refuse
/// `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW`, and every device node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entrance {
    /// The identity: the sandbox's workload identity, or the one that the execution's start names.
    pub identity: Identity,
    /// The hostname that the kernel command line configures.
    pub hostname: Vec<u8>,
}

impl Entrance {
    /// Returns the entry into the root at `root`.
    pub fn entry(&self, root: impl Into<PathBuf>) -> Entry {
        Entry::new(root, self.identity.clone()).hostname(self.hostname.clone())
    }
}

/// The operations through which the executions enter the assembled root: those of
/// `nvx::guest::entry` in a VM, in [`LibraryRoot`], and a fake's in tests, which cannot enter a
/// root without privileges.
pub trait EntryOperations {
    /// Prepares the entry of an execution into the assembled root, as `entrance` describes it,
    /// with `Entry::prepare`: checks its inputs, and reads and checks the identity's account in
    /// existing-account mode.
    fn prepare(&mut self, entrance: &Entrance) -> Result<Prepared, entry::Error>;

    /// Spawns `command`, whose child enters the execution that `prepared` prepared. A fake that
    /// cannot enter a root spawns the command in its own root instead, as its own identity.
    fn spawn(&mut self, command: spawn::Command, prepared: Prepared) -> io::Result<Spawned>;
}

/// The operations of a layered root that the worker runs: those of `nvx::guest::root` in a VM, in
/// [`LibraryRoot`], and a fake's in tests. Each fails with the problem, as text, and
/// [`map`](Self::map) with the mapping that failed too, but the [entry's](EntryOperations), which
/// fail as `nvx::guest::entry` does.
pub trait RootOperations: EntryOperations {
    /// Returns the kernel command line, from which the worker reads the boot layout of the root,
    /// and the host export.
    fn command_line(&mut self) -> Result<Vec<u8>, String>;

    /// Bootstraps the guest for the root that `config` configures: its cgroups, and their
    /// resource values.
    fn bootstrap(&mut self, config: &RootConfig) -> Result<(), String>;

    /// Assembles the root of `layout`, which the boot tokens name, as `config` configures it.
    fn assemble(&mut self, config: &RootConfig, layout: &RootLayout) -> Result<(), String>;

    /// Mounts the host export `export`, which the kernel command line names, out of the assembled
    /// root, as the source of the host's mapping table.
    fn mount_export(&mut self, export: &HostExport) -> Result<(), String>;

    /// Applies `mappings`, consecutive mappings of the host's mapping table, beneath the assembled
    /// root, in order, from the host export, and keeps those before the first that it refuses or
    /// fails to apply, which latches the root.
    ///
    /// Fails with a [`MapError`] that counts the mappings before the one that failed.
    fn map(&mut self, mappings: &[Mapping]) -> Result<(), MapError>;

    /// Returns the cgroups in which the worker runs its executions once the root is ready: the
    /// layout that the bootstrap prepared, below whose workload cgroup each execution gets a
    /// cgroup of its own. Without them, as in tests that cannot create cgroups, each program runs
    /// in a process group alone.
    fn cgroups(&self) -> Option<Layout>;

    /// Runs `probe` on the assembled root, and returns what it found.
    fn probe(&mut self, probe: &Probe) -> Result<Facts, String>;

    /// Tears the assembled root down.
    fn teardown(&mut self) -> Result<(), String>;
}

/// A mapping that [`RootOperations::map`] refused or failed to apply, once it applied those before
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapError {
    /// How many of the mappings the operations applied, in order, before the one that failed: its
    /// index among them.
    pub applied: usize,
    /// What failed.
    pub problem: String,
}

/// How the worker's session ended, when it did not fail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Worked {
    /// The host stopped the worker, which tore its root down, if it had assembled one.
    Stopped,
    /// A stage of the root failed, which the worker reported to the host.
    Failed(RootFailure),
    /// The broker closed the stream at a record boundary.
    Closed,
}

impl Worked {
    /// Returns the status with which the worker ends: 0 once the host stopped it, the stage's
    /// [exit status](RootStage::exit_status) once a stage failed, and [`SESSION_STATUS`] once the
    /// broker closed the stream.
    pub fn status(&self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Failed(failure) => failure.stage().exit_status(),
            Self::Closed => SESSION_STATUS,
        }
    }
}

/// Runs the worker's session on `stream`, the guest's end of the control session, which it makes
/// nonblocking, with the root operations `operations`, as the [module documentation](self)
/// describes, until the host stops the worker, a stage of the root fails, or the broker closes the
/// stream.
///
/// A failure of the session, a malformed message, and a message that the worker does not accept
/// in its state end the session with an error, and leave a root that it assembled to
/// `operations`. However the session ends, the worker drops its executions, which ends their
/// programs. Once the host stopped it, or the broker closed the stream at a record boundary, it
/// then writes what it queued for the stream, waiting while the stream takes none, before it
/// returns.
pub fn work<S: Read + Write + AsFd>(
    stream: S,
    operations: &mut dyn RootOperations,
) -> Result<Worked, ServeError> {
    let mut connection = Connection::new(stream)?;
    let mut executions = Executions::new();
    // The worker's order, once its root is ready.
    let mut order = None;
    loop {
        connection.flush()?;
        while !connection.is_overloaded() {
            let message = match connection.receive()? {
                None => break,
                Some(Received::Closed) => {
                    // The broker may have closed only its end of the stream, and read on: what
                    // the worker queued reaches it, as once the host stopped the worker, after
                    // the worker dropped its executions, which a stalled reader then holds back
                    // no more.
                    executions.clear();
                    connection.finish()?;
                    return Ok(Worked::Closed);
                }
                Some(Received::Reset) => {
                    executions.reset(Instant::now());
                    continue;
                }
                Some(Received::Message(message)) => message,
            };
            // Each message takes its own instant, so that a root's assembly, a probe, or a
            // launch that came before it takes nothing from the timers of an execution that it
            // starts.
            let now = Instant::now();
            let step = match order.as_mut() {
                Some(order) => serve_root(
                    operations,
                    order,
                    &mut executions,
                    &mut connection,
                    now,
                    message,
                )?,
                None => {
                    let (step, ready) = configure_root(operations, &mut executions, message)?;
                    order = ready;
                    step
                }
            };
            let (replies, ended) = match step {
                Step::Continue(replies) => (replies, None),
                Step::End(replies, worked) => (replies, Some(worked)),
            };
            for reply in &replies {
                connection.send(reply)?;
            }
            if let Some(worked) = ended {
                connection.finish()?;
                return Ok(worked);
            }
        }
        executions.run(Instant::now(), &mut connection)?;
        connection.credit()?;
        // While records back up on the stream, the executions read no output, and may keep what
        // they read in their runners, which no descriptor tells: once a flush ends the backlog,
        // the next turn comes at once.
        let backlogged = connection.is_backlogged();
        connection.flush()?;
        if backlogged && !connection.is_backlogged() {
            continue;
        }
        wait(&connection, &executions)?;
    }
}

/// Runs the worker's session on the tty at `tty`, as [`work`] does, through
/// [`guest::on_terminal`], which opens the tty in raw mode, as NVX's managed agent does, and
/// waits once the session ended until the tty has sent what the worker wrote. The worker opens
/// the tty again for itself, so that the open file that it makes nonblocking is its own.
///
/// It fails with [`ServeError::Terminal`] if the tty cannot be opened or put in raw mode, and
/// otherwise as [`work`] does.
pub fn work_on_terminal(
    tty: &Path,
    operations: &mut dyn RootOperations,
) -> Result<Worked, ServeError> {
    guest::on_terminal(tty, |_| {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(OFlag::O_NOCTTY.bits())
            .open(tty)
            .map_err(|error| {
                let problem = format!("cannot open {}: {error}", tty.display());
                ServeError::Terminal(io::Error::new(error.kind(), problem))
            })?;
        work(file, operations)
    })
}

/// Waits until the session's stream or an execution's descriptor polls ready, or the earliest
/// deadline of the executions passes; at once if the connection holds bytes that the worker
/// takes next. The worker waits for the host's messages unless the connection is overloaded,
/// and for the executions' output unless it is backlogged.
fn wait<S: Read + Write + AsFd>(
    connection: &Connection<S>,
    executions: &Executions,
) -> Result<(), ServeError> {
    let overloaded = connection.is_overloaded();
    let mut events = PollFlags::empty();
    if !overloaded {
        events |= PollFlags::POLLIN;
    }
    if connection.wants_write() {
        events |= PollFlags::POLLOUT;
    }
    let mut descriptors = vec![PollFd::new(connection.descriptor(), events)];
    executions.poll_descriptors(&mut descriptors, connection.is_backlogged());
    let timeout = if connection.has_input() && !overloaded {
        PollTimeout::ZERO
    } else {
        match executions.deadline() {
            None => PollTimeout::NONE,
            Some(deadline) => {
                let timeout = deadline.saturating_duration_since(Instant::now());
                if timeout.is_zero() {
                    PollTimeout::ZERO
                } else {
                    // Poll counts whole milliseconds, so a shorter wait would return before the
                    // deadline.
                    PollTimeout::try_from(timeout.max(Duration::from_millis(1)))
                        .unwrap_or(PollTimeout::MAX)
                }
            }
        }
    };
    match poll::poll(&mut descriptors, timeout) {
        Ok(_) | Err(Errno::EINTR) => Ok(()),
        Err(error) => Err(ServeError::Session(GuestConnectionError::Io(error.into()))),
    }
}

/// Handles `message` before the root is ready: a root configuration, whose root it assembles
/// through `operations`, after which `executions` run in the cgroups that the operations prepared,
/// as the sandbox that the configuration and the kernel command line describe, and which it answers
/// with its readiness, or a `Stop`. Returns the step, with the worker's order once the root is
/// ready.
fn configure_root(
    operations: &mut dyn RootOperations,
    executions: &mut Executions,
    message: Message,
) -> Result<(Step<Worked>, Option<Order>), ServeError> {
    match message {
        Message::RootConfig(body) => Ok(match assemble(operations, &body) {
            Ok((config, sandbox)) => {
                executions.run_in(operations.cgroups(), sandbox);
                let ready = Ready::new(GUEST_BUILD).expect("this crate's version is a build");
                let step = Step::Continue(vec![Message::Ready(ready.encode())]);
                (step, Some(Order::new(config.mapping_count)))
            }
            Err(failure) => {
                let replies = vec![Message::RootFailed(failure.encode())];
                (Step::End(replies, Worked::Failed(failure)), None)
            }
        }),
        Message::Stop => Ok((Step::End(vec![Message::Stopped], Worked::Stopped), None)),
        message => Err(ServeError::Unexpected(message.kind())),
    }
}

/// Handles `message` at `now` once the root is ready, through `operations`, in the worker's
/// `order`, and hands a message about an execution to `executions`, which enter the root through
/// `operations`, and queue what they answer on `connection`.
fn serve_root<S: Read + Write + AsFd>(
    operations: &mut dyn RootOperations,
    order: &mut Order,
    executions: &mut Executions,
    connection: &mut Connection<S>,
    now: Instant,
    message: Message,
) -> Result<Step<Worked>, ServeError> {
    if let Some(decoded) = HostMessage::decode(&message) {
        // The worker runs no execution before the host started the sandbox.
        if !order.is_started() {
            return Err(ServeError::Unexpected(message.kind()));
        }
        let decoded = decoded.map_err(|error| ServeError::Execution(error.problem().to_owned()))?;
        executions.deliver(now, decoded, operations, connection)?;
        return Ok(Step::Continue(Vec::new()));
    }
    Ok(match message {
        Message::Ping => Step::Continue(vec![Message::Pong]),
        Message::Echo(body) => Step::Continue(vec![Message::Echo(body)]),
        // The loop runs a probe to its end, which may take a while, as for one that fills the
        // upper. So while the worker runs executions, it answers a probe with a failure instead,
        // and no probe holds back their timers, the host's cancellations, or its other messages.
        Message::Probe(_) if !executions.is_empty() => {
            Step::Continue(vec![Message::ProbeFailed(EXECUTING.as_bytes().to_vec())])
        }
        Message::Probe(body) => Step::Continue(vec![run_probe(operations, &body)]),
        // The mapping table comes before the start, which comes once the worker applied it.
        Message::Maps(body) if !order.is_started() => {
            Step::Continue(vec![order.map(operations, &body)])
        }
        Message::Start if order.may_start() => Step::Continue(vec![order.start()]),
        Message::Stop => {
            executions.clear();
            match operations.teardown() {
                Ok(()) => Step::End(vec![Message::Stopped], Worked::Stopped),
                Err(problem) => {
                    let failure = RootFailure::new(RootStage::Teardown, problem);
                    Step::End(
                        vec![Message::RootFailed(failure.encode()), Message::Stopped],
                        Worked::Failed(failure),
                    )
                }
            }
        }
        message => return Err(ServeError::Unexpected(message.kind())),
    })
}

/// Checks the root configuration `body`, reads the boot layout of the root, the executions'
/// hostname, and the host export, bootstraps the guest, assembles the root, mounts the export out
/// of it, if the kernel command line names one, and checks the account of the sandbox's workload
/// identity there, if the configuration names one, through `operations`. Returns the configuration,
/// with the sandbox in which the executions run, or the failure of the stage that failed: the
/// export's mount ends the assembly.
fn assemble(
    operations: &mut dyn RootOperations,
    body: &[u8],
) -> Result<(RootConfig, Sandbox), RootFailure> {
    let config = RootConfig::decode(body)
        .map_err(|error| RootFailure::new(RootStage::Configuration, error.problem()))?;
    let (layout, export, hostname) = layout(operations, &config)
        .map_err(|problem| RootFailure::new(RootStage::Layout, problem))?;
    operations
        .bootstrap(&config)
        .map_err(|problem| RootFailure::new(RootStage::Bootstrap, problem))?;
    operations
        .assemble(&config, &layout)
        .map_err(|problem| RootFailure::new(RootStage::Assembly, problem))?;
    if let Some(export) = &export {
        operations
            .mount_export(export)
            .map_err(|problem| RootFailure::new(RootStage::Assembly, problem))?;
    }
    if let Some((uid, gid)) = config.workload_identity {
        let entrance = Entrance {
            identity: Identity::new(uid, gid),
            hostname: hostname.clone(),
        };
        operations.prepare(&entrance).map_err(|error| {
            RootFailure::new(
                RootStage::Identity,
                format!("cannot enter the root as the workload identity {uid}:{gid}: {error}"),
            )
        })?;
    }
    let sandbox = Sandbox {
        hostname,
        identity: config.workload_identity,
    };
    Ok((config, sandbox))
}

/// Reads the boot layout of the root, the executions' hostname, and the host export, if any, from
/// the kernel command line, through `operations`, and checks that the worker assembles them as
/// `config` configures it: an EROFS image or a GPT disk, over an ext4 scratch, or over RAM if
/// `config` configures no scratch, with a bound of the first partition for a GPT disk alone, and
/// with a host export, the source of the mapping table, if `config` announces mappings.
fn layout(
    operations: &mut dyn RootOperations,
    config: &RootConfig,
) -> Result<(RootLayout, Option<HostExport>, Vec<u8>), String> {
    let command_line = operations.command_line()?;
    let layout = RootLayout::parse(&command_line)
        .map_err(|error| format!("the kernel command line has invalid boot tokens: {error}"))?
        .ok_or("the kernel command line names no layered root")?;
    let export = HostExport::parse(&command_line).map_err(|error| {
        format!("the kernel command line has invalid tokens of the host export: {error}")
    })?;
    if layout.image.format == ImageFormat::Erofs && config.first_partition_bound.is_some() {
        return Err(
            "the root configuration bounds the first partition, but the boot tokens name an \
             EROFS image"
                .to_owned(),
        );
    }
    if layout.upper == RootUpper::Ram
        && (config.scratch_sectors.is_some() || config.scratch_journal.is_some())
    {
        return Err(
            "the root configuration configures a scratch, but the boot tokens keep the upper in RAM"
                .to_owned(),
        );
    }
    if export.is_none() && config.mapping_count > 0 {
        let count = config.mapping_count;
        let plural = if count == 1 { "" } else { "s" };
        return Err(format!(
            "the root configuration announces {count} mapping{plural}, but the kernel command \
             line names no host export to map from"
        ));
    }
    Ok((layout, export, hostname(&command_line)?))
}

/// Returns the hostname that the kernel command line `command_line` configures, as NVX's init
/// agent reads it: the value of its first `nvx_hostname=` parameter, or
/// [`DEFAULT_HOSTNAME`](entry::DEFAULT_HOSTNAME) if it has none, or an empty one. A hostname of
/// more than [`MAX_HOSTNAME_BYTES`](entry::MAX_HOSTNAME_BYTES), or with a NUL, is refused.
fn hostname(command_line: &[u8]) -> Result<Vec<u8>, String> {
    let named = command_line
        .split(u8::is_ascii_whitespace)
        .find_map(|parameter| parameter.strip_prefix(HOSTNAME_PARAMETER));
    match named {
        None | Some([]) => Ok(entry::DEFAULT_HOSTNAME.as_bytes().to_vec()),
        Some(hostname) if hostname.len() > entry::MAX_HOSTNAME_BYTES || hostname.contains(&0) => {
            Err(format!(
                "the kernel command line names a hostname that is longer than {} bytes, or \
                 holds a NUL: {:?}",
                entry::MAX_HOSTNAME_BYTES,
                String::from_utf8_lossy(hostname)
            ))
        }
        Some(hostname) => Ok(hostname.to_vec()),
    }
}

/// Runs the probe `body` through `operations`, and returns the worker's answer: a report of what
/// the probe found, or the problem.
fn run_probe(operations: &mut dyn RootOperations, body: &[u8]) -> Message {
    let found = Probe::decode(body)
        .map_err(|error| error.problem().to_owned())
        .and_then(|probe| operations.probe(&probe));
    let problem = match found {
        Ok(facts) => {
            let report = facts.encode();
            if report.len() <= MAX_BODY {
                return Message::Report(report);
            }
            format!(
                "the probe's report takes {} bytes, more than {MAX_BODY}",
                report.len()
            )
        }
        Err(problem) => problem,
    };
    Message::ProbeFailed(truncated(&problem, MAX_TEXT_BODY).as_bytes().to_vec())
}

/// The root operations of `nvx::guest::root` on the guest's kernel, whose probes the [`probes`]
/// module runs.
///
/// Dropping it closes the descriptors that probes keep open and the host export's root, then
/// unmounts a root that it still holds, as `nvx::guest::root::Root` does when it drops, ignoring
/// failures.
pub struct LibraryRoot {
    /// The process ID of PID 1's unrelated child, which the probes check, if PID 1 named one.
    sibling: Option<u32>,
    /// What the bootstrap prepared, once it ran.
    bootstrap: Option<Bootstrap>,
    /// The descriptors that probes keep open until the worker ends, such as those of `hold`.
    /// They close before the root drops.
    held: Vec<File>,
    /// The root of the host export, from which the worker maps the host's mapping table, once the
    /// root mounted the export. It keeps the export busy, so it closes before the root drops, and
    /// before its teardown.
    export: Option<SourceRoot>,
    /// The root, from its assembly until its teardown.
    root: Option<Root>,
}

impl LibraryRoot {
    /// Returns the operations, with the process ID of PID 1's unrelated child, if any.
    pub fn new(sibling: Option<u32>) -> Self {
        Self {
            sibling,
            bootstrap: None,
            held: Vec::new(),
            export: None,
            root: None,
        }
    }
}

impl RootOperations for LibraryRoot {
    /// Reads `/proc/cmdline`.
    fn command_line(&mut self) -> Result<Vec<u8>, String> {
        fs::read(COMMAND_LINE).map_err(|error| format!("cannot read {COMMAND_LINE}: {error}"))
    }

    /// Runs `nvx::guest::root::bootstrap` with the configuration's cgroups and resource values.
    fn bootstrap(&mut self, config: &RootConfig) -> Result<(), String> {
        let layout = Layout::new(&config.agent_cgroup, &config.workload_cgroup)
            .map_err(|error| error.to_string())?;
        let resources = Resources {
            agent_memory_low: config.agent_memory_low,
            workload_memory_max: config.workload_memory_max,
            workload_pids_max: config.workload_pids_max,
        };
        let bootstrap = root::bootstrap(&layout, &resources).map_err(|error| error.to_string())?;
        self.bootstrap = Some(bootstrap);
        Ok(())
    }

    /// Runs `nvx::guest::root::assemble` on the block devices at the MMIO bases of their roles:
    /// the EROFS image, with the identity that the boot tokens expect of it, or the GPT disk, with
    /// the disk GUID that they expect of it, whose first partition holds at most the
    /// configuration's bound, over the ext4 scratch, with the configuration's journal policy, or
    /// over a tmpfs, in the configuration's overlay mode, with the configuration's runtime
    /// directory, device sizes, and timeout. A bound, a policy, or a mode that the configuration
    /// does not name is the guest's default: the first partition holds at most
    /// [`DEFAULT_FIRST_PARTITION_BOUND`] bytes, a scratch may have a journal, and the overlay
    /// trusts its lowers.
    fn assemble(&mut self, config: &RootConfig, layout: &RootLayout) -> Result<(), String> {
        let image = match layout.image.format {
            ImageFormat::Erofs => Image::Erofs {
                uuid: layout.image.identity,
            },
            ImageFormat::Gpt => {
                let bound = config
                    .first_partition_bound
                    .unwrap_or(DEFAULT_FIRST_PARTITION_BOUND);
                let first_partition_bound = usize::try_from(bound).map_err(|_| {
                    format!("the guest cannot hold a first partition of {bound} bytes")
                })?;
                let expected = match layout.image.identity {
                    Some(guid) => Expected::new().disk_guid(guid),
                    None => Expected::new(),
                };
                Image::Gpt {
                    expected,
                    first_partition_bound,
                }
            }
        };
        let upper = match layout.upper {
            RootUpper::Scratch => Upper::Scratch {
                device: Device {
                    mmio: SCRATCH_MMIO_BASE,
                    sectors: config.scratch_sectors,
                },
                journal: match config.scratch_journal {
                    None | Some(JournalPolicy::Allowed) => Journal::Allowed,
                    Some(JournalPolicy::Forbidden) => Journal::Forbidden,
                },
            },
            RootUpper::Ram => Upper::Tmpfs,
        };
        let overlay = match config.overlay_mode {
            None => root::OverlayMode::default(),
            Some(OverlayMode::TrustedLowers) => root::OverlayMode::TrustedLowers,
            Some(OverlayMode::UntrustedLowers) => root::OverlayMode::UntrustedLowers,
        };
        let assembly = Assembly {
            runtime: PathBuf::from(&config.runtime),
            distro: Device {
                mmio: DISTRO_MMIO_BASE,
                sectors: config.distro_sectors,
            },
            image,
            upper,
            overlay,
            timeout: config.device_timeout,
        };
        let root = root::assemble(&assembly).map_err(|error| error.to_string())?;
        self.root = Some(root);
        Ok(())
    }

    /// Runs `nvx::guest::root::Root::mount_export` with the export's tag, and the access of its
    /// mode, and keeps the export's root, from which the worker maps the host's mapping table.
    fn mount_export(&mut self, export: &HostExport) -> Result<(), String> {
        let Some(root) = &mut self.root else {
            return Err("the worker holds no root to mount the host export for".to_owned());
        };
        let source = root
            .mount_export(&export.tag, export.mode.into())
            .map_err(|error| error.to_string())?;
        self.export = Some(source);
        Ok(())
    }

    /// Runs `nvx::guest::root::Root::map` from the host export's root, with each mapping in turn,
    /// so that a failure tells how many of the mappings before it the root applied.
    fn map(&mut self, mappings: &[Mapping]) -> Result<(), MapError> {
        let (Some(root), Some(export)) = (&mut self.root, &self.export) else {
            return Err(MapError {
                applied: 0,
                problem: "the worker holds no host export to map from".to_owned(),
            });
        };
        for (applied, mapping) in mappings.iter().enumerate() {
            root.map(export, std::slice::from_ref(mapping))
                .map_err(|error| MapError {
                    applied,
                    problem: error.to_string(),
                })?;
        }
        Ok(())
    }

    /// Returns the cgroups that the bootstrap prepared: those of the root configuration.
    fn cgroups(&self) -> Option<Layout> {
        self.bootstrap
            .as_ref()
            .map(|bootstrap| bootstrap.layout().clone())
    }

    /// Runs one of the [`probes`], of the [`mapping`] probes, which inspect the root's mappings
    /// and map from the host export, or of the [`entries`] probes, which enter it.
    fn probe(&mut self, probe: &Probe) -> Result<Facts, String> {
        let (Some(bootstrap), Some(root)) = (&self.bootstrap, &mut self.root) else {
            return Err("the worker holds no root to probe".to_owned());
        };
        // The upper's mount point, below the runtime directory, as `nvx::guest::root` names it.
        let upper = match root.upper() {
            UpperKind::Scratch => "scratch",
            UpperKind::Tmpfs => "tmpfs",
            kind => return Err(format!("the worker probes no root over a {kind:?} upper")),
        };
        let (runtime, path) = (root.runtime().to_owned(), root.path().to_owned());
        let upper = runtime.join(upper);
        let lowers = root
            .lowers()
            .iter()
            .map(|lower| lower.path().to_owned())
            .collect::<Vec<_>>();
        let first_partition = root.first_partition().map(<[u8]>::to_vec);
        let context = probes::Context {
            runtime: &runtime,
            root: &path,
            lowers: &lowers,
            upper: &upper,
            first_partition: first_partition.as_deref(),
            agent: bootstrap.layout().agent(),
            workload: bootstrap.layout().workload(),
            sibling: self.sibling,
        };
        if mapping::PROBES.contains(&probe.name()) {
            return mapping::run(probe, &context, root, self.export.as_ref());
        }
        if entries::PROBES.contains(&probe.name()) {
            return entries::run(probe, &context);
        }
        probes::run(probe, &context, &mut self.held)
    }

    /// Runs `nvx::guest::root::Root::teardown`, once it closed the host export's root, which keeps
    /// the export busy. A root whose teardown failed keeps its mounts.
    fn teardown(&mut self) -> Result<(), String> {
        self.export = None;
        match self.root.take() {
            Some(root) => root.teardown().map_err(|error| error.to_string()),
            None => Ok(()),
        }
    }
}

impl EntryOperations for LibraryRoot {
    /// Runs `Entry::prepare` on the assembled root.
    fn prepare(&mut self, entrance: &Entrance) -> Result<Prepared, entry::Error> {
        let Some(root) = &self.root else {
            let missing = io::Error::new(io::ErrorKind::NotFound, "the worker holds no root");
            return Err(entry::Error::Root(missing));
        };
        entrance.entry(root.path()).prepare()
    }

    /// Spawns `command`, whose child enters the execution with `Command::enter`.
    fn spawn(&mut self, command: spawn::Command, prepared: Prepared) -> io::Result<Spawned> {
        command.enter(prepared).spawn()
    }
}

/// Runs PID 1 of a VM with a layered root, as the [module documentation](self) describes, with
/// the worker on the tty `tty`, and returns the worker's status, or [`SESSION_STATUS`] if a child
/// cannot start or the worker's status cannot be collected.
///
/// As PID 1, it leaves the unrelated child to the end of the VM. Elsewhere, as in tests, it then
/// kills the unrelated child and collects its status.
pub fn supervise(tty: &Path) -> u8 {
    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(error) => {
            eprintln!("runtime-adapter-guest: cannot find its own executable: {error}");
            return SESSION_STATUS;
        }
    };
    let sibling = match start(Command::new(&program).arg(crate::GUEST_IDLE_COMMAND)) {
        Ok(sibling) => sibling,
        Err(error) => {
            eprintln!("runtime-adapter-guest: cannot start PID 1's unrelated child: {error}");
            return SESSION_STATUS;
        }
    };
    let worker = start(
        Command::new(&program)
            .arg(crate::GUEST_WORKER_COMMAND)
            .arg(tty)
            .arg(sibling.to_string()),
    );
    let status = match worker {
        Ok(worker) => collect(worker),
        Err(error) => {
            eprintln!("runtime-adapter-guest: cannot start the worker: {error}");
            SESSION_STATUS
        }
    };
    if std::process::id() != 1 {
        // A child keeps its process ID until its status is collected, so the signal reaches the
        // unrelated child unless `collect` collected its status already.
        let sibling = pid(sibling);
        if wait::waitpid(sibling, Some(WaitPidFlag::WNOHANG)) == Ok(WaitStatus::StillAlive) {
            let _ = signal::kill(sibling, Signal::SIGKILL);
            let _ = wait::waitpid(sibling, None);
        }
    }
    status
}

/// Starts `command` with no standard input, and returns its process ID, whose status the caller
/// collects.
fn start(command: &mut Command) -> std::io::Result<u32> {
    // PID 1 collects the status of every child through `wait`, rather than through the handle.
    #[allow(clippy::zombie_processes)]
    let child = command.stdin(Stdio::null()).spawn()?;
    Ok(child.id())
}

/// Collects the status of every child until the child `worker` ends, and returns the worker's
/// status: its exit status, or 128 plus the signal that ended it, as a shell reports it.
///
/// A wait reports to PID 1 the end of each child, and, whatever its options, each stop of a
/// child's thread that asked PID 1 to trace it: a stop ends nothing, and PID 1 goes on past it.
/// `nix` decodes no real-time signal in a report, though a wait that fails to decode one has
/// taken it, and both the worker and the processes that PID 1 adopts, such as those that an
/// execution leaves, may end with one, or stop with one while traced. So PID 1 waits until a
/// child has something to report, without taking it, and takes each report through a wait that
/// names its thread, which keeps every signal: the worker's through the worker's own handle, and
/// another's by the ID that the first wait gives if the report decodes. Otherwise, PID 1 finds the
/// threads that may have one in `/proc`. Only the worker's end, or a wait for the worker that
/// fails, ends the collection.
fn collect(worker: u32) -> u8 {
    // The pause after a wait whose report PID 1 may not have taken, which the next wait would give
    // again at once.
    const PAUSE: Duration = Duration::from_millis(10);
    let mut handle = match Child::new(worker) {
        Ok(handle) => handle,
        Err(error) => return uncollected(&error),
    };
    loop {
        if let Some(status) = worker_status(&mut handle) {
            return status;
        }
        let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT;
        let took = match wait::waitid(Id::All, flags) {
            // The worker's handle takes the worker's report.
            Ok(status) if status.pid() == Some(pid(worker)) => continue,
            Ok(status) => status
                .pid()
                .is_some_and(|id| take(id.as_raw().unsigned_abs()).is_ok()),
            Err(Errno::EINVAL) => {
                // The report that does not decode may be the worker's end.
                if let Some(status) = worker_status(&mut handle) {
                    return status;
                }
                let mut took = false;
                for id in reporting_threads().into_iter().filter(|&id| id != worker) {
                    // A wait for a thread that stopped takes its stop, if it has one to report,
                    // and returns nothing either way: only an end shows that a report was taken.
                    took |= matches!(take(id), Ok(Some(_)));
                }
                took
            }
            Err(Errno::EINTR) => continue,
            Err(error) => return uncollected(&error.into()),
        };
        if !took {
            std::thread::sleep(PAUSE);
        }
    }
}

/// Takes the report of the thread `id`, a child of this process, or a thread of a child that this
/// process traces: its status, once it ended, or nothing for a stop.
fn take(id: u32) -> io::Result<Option<ExitStatus>> {
    Child::new(id)?.try_wait()
}

/// Returns the IDs of the threads of this process's children that may have something to report to
/// it, as `/proc` lists them: a child that ended, a zombie, and a thread that stopped while traced,
/// which reports its stops to this process if it asked its parent to trace it.
fn reporting_threads() -> Vec<u32> {
    let parent = std::process::id();
    let mut reporting = Vec::new();
    for process in numbered("/proc") {
        if state_and_parent(&format!("/proc/{process}")).is_none_or(|(_, of)| of != parent) {
            continue;
        }
        // A thread may end, and its report be taken, while the scan reads it.
        for thread in numbered(&format!("/proc/{process}/task")) {
            let state = state_and_parent(&format!("/proc/{process}/task/{thread}"));
            if state.is_some_and(|(state, _)| matches!(state, 'Z' | 't')) {
                reporting.push(thread);
            }
        }
    }
    reporting
}

/// Returns the numbered entries of the directory `path` of `/proc`, the IDs of processes or
/// threads, or none if it cannot be read.
fn numbered(path: &str) -> impl Iterator<Item = u32> {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
}

/// Returns the state, and the parent's process ID, of the process or thread whose directory of
/// `/proc` is `path`, from its `stat`, or `None` once it is gone.
fn state_and_parent(path: &str) -> Option<(char, u32)> {
    let stat = fs::read(format!("{path}/stat")).ok()?;
    // The state, then the parent's process ID, follow the command's name after its last `)`: the
    // name may hold any byte, and so need not be UTF-8, while the fields are ASCII.
    let end = stat.iter().rposition(|&byte| byte == b')')?;
    let mut fields = std::str::from_utf8(&stat[end + 1..])
        .ok()?
        .split_whitespace();
    let state = fields.next()?.chars().next()?;
    Some((state, fields.next()?.parse().ok()?))
}

/// Collects the worker's status through `handle` without blocking, and returns the status with
/// which PID 1 ends, once the worker ended, or once its status cannot be collected.
fn worker_status(handle: &mut Child) -> Option<u8> {
    match handle.try_wait() {
        Ok(status) => status.map(shell_status),
        Err(error) => Some(uncollected(&error)),
    }
}

/// Returns the status with which a shell reports `status`: the exit status, or 128 plus the signal
/// that ended the process.
fn shell_status(status: ExitStatus) -> u8 {
    let code = status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal));
    code.and_then(|code| u8::try_from(code).ok())
        .unwrap_or(SESSION_STATUS)
}

/// Reports that PID 1 cannot collect the worker's status, with `error`, and returns
/// [`SESSION_STATUS`].
fn uncollected(error: &io::Error) -> u8 {
    eprintln!("runtime-adapter-guest: cannot collect the worker's status: {error}");
    SESSION_STATUS
}

/// Returns the process ID `id` as `nix` takes it.
fn pid(id: u32) -> Pid {
    // Linux process IDs are at most 2^22.
    Pid::from_raw(i32::try_from(id).unwrap_or(i32::MAX))
}

/// Runs PID 1's unrelated child, which sleeps until a signal ends it.
pub fn idle() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

#[cfg(test)]
mod tests;
