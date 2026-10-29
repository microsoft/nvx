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
//!    `nvx::protocol::root::RootLayout::parse`, and the host export, if the command line names one,
//!    with `nvx::protocol::export::HostExport::parse`, then bootstraps the guest, assembles the
//!    root, and mounts the export out of the root. If the configuration names a workload identity
//!    for the whole sandbox, it then checks the identity's account in the assembled root, in
//!    existing-account mode, through `nvx::guest::entry`.
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
//! fails the assembly. A workload identity whose account the assembled root lacks fails the
//! identity stage.
//!
//! The worker, and with it the VM, ends with 0 once the host stopped it, after a teardown that
//! succeeded, with the stage's [exit status](RootStage::exit_status) once a stage failed, from 11
//! for the configuration to 16 for the identity, with [`SESSION_STATUS`] once its session failed
//! or the broker closed it, and, in a clone, with the status of its activation's failure, from 50
//! to 58, as the [section on clones](#clones) describes. Once the broker closed the stream at a
//! record boundary, which may close only the broker's end, the worker drops its executions, and
//! writes what it queued, as once the host stopped it.
//!
//! In a template's VM, whose kernel command line names the platform snapshot tier, the worker
//! assembles no root: on x86_64, the guest agent reads the kernel command line before anything
//! else with `crate::template::boot`, and the worker captures the VM instead, as the `capture`
//! module describes, and then serves each clone that a restore of the snapshot starts, as the
//! [next section](#clones) describes.
//!
//! # Clones
//!
//! A restore of a template's snapshot starts a clone, on x86_64, whose worker, the template's,
//! repairs the restore, as the `capture` module describes, behind its fence of workload admission,
//! which stays closed. The worker then serves the clone's session with `work_clone`, through the
//! clone's activation, `nvx::guest::clone::Activation`, which the `activation` module drives:
//!
//! 1. The session that the template attached before its capture, and whose first epoch it
//!    acknowledged, resumes on the same tty: the worker attaches none, and acknowledges the reset
//!    that the restore queued, the only one that the clone reads, which ends the activation's
//!    transport bootstrap and restricts the session.
//! 2. The host sends one `Activate`, whose body is the canonical encoding of a clone's
//!    `nvx::activation::ActivationRecord`. The activation decodes and checks it, and checks the
//!    resources that it expects against those that the repair found, before anything has a side
//!    effect.
//! 3. The worker applies the record through `RootOperations::apply`, which runs
//!    `nvx::guest::clone::apply`: it assembles the root from the template's devices, with the
//!    record's image, upper, and overlay mode, once the kernel reread the partition table of a
//!    GPT disk, over a fresh upper, waiting for each device until the activation's deadline at the
//!    latest, so that the application ends by then, and checks the account of the record's workload
//!    identity, if it has one, in the assembled root, as the worker of a VM that boots cold does
//!    for a root configuration. If the record asks for a generated machine ID, it then generates
//!    one, once, after the repair's reseed. If the record has an export, it mounts the host export
//!    out of the root, through the template's host filesystem device, `capture::FILESYSTEM_TAG`, as
//!    the worker of a VM that boots cold mounts the one that its command line names, and if the
//!    record's network names a DNS server, it writes a resolver file that names it. If the record's
//!    network publishes the workload's listeners, it starts their announcer,
//!    `/sbin/nvx-ingress inherit GATEWAY_IPV4`, from a UDP socket that it binds to a privileged
//!    source port, to the gateway that the template's kernel command line names, as NVX's `/init`
//!    starts it at a cold boot whose policy publishes them: the template, captured under a policy
//!    that publishes nothing, runs none. It then binds the generated machine ID read-only over the
//!    root's `/etc/machine-id`, and the resolver file over its `/etc/resolv.conf`, in the worker's
//!    mount namespace, which each execution's copies. The template bootstrapped the guest for its
//!    root with its own configuration before its capture, so the clone has its cgroups.
//! 4. Once it took what the session received while the application ran, which came before `Ready`,
//!    it completes the activation, which opens the fence, and answers `Ready`, which reports that
//!    the activation applied, with the record's transaction, after the guest agent's build, to the
//!    `Activate` and to each retry of it, byte for byte, that came meanwhile, which joins the
//!    application. Anything else that came meanwhile, a reset included, fails the activation, as
//!    before the application, so the host waits for `Ready` before it sends the mapping table or
//!    the start.
//! 5. It then serves the host's order as after a cold boot: the mapping table that the record
//!    announces, which it maps from the record's export, the start, the executions, the probes, and
//!    the stop. The record's workload identity, if it has one, is the whole sandbox's, as a root
//!    configuration's is, so that executions name none, every execution takes the record's
//!    hostname, and a clone's executions run in the cgroups of the template's bootstrap. On the
//!    stop, the worker stops the announcer, closes the export's root, and removes the binds of the
//!    application before it tears the root down, since they keep the root's mounts busy, through
//!    `nvx::guest::clone::Applied::teardown`. It also answers the probe
//!    `activation::ACTIVATION_PROBE`, with when the events of the clone's activation and order
//!    happened, after the repair's acknowledgement of the restore.
//!
//! The application leaves out one of the record's settings. The endpoint of its network's proxy,
//! which `nvx::guest::clone::Applied::proxy` returns, is the host's to tell the workloads, as
//! after a cold boot: the host program's `exec` gives each execution the proxy's URL in its
//! environment, from the plan of the VM's network, so that an execution's environment stays
//! exactly its start's. A clone's executions enter the root with the publication of listeners
//! that the record's network names, published under a policy that publishes them, and unpublished
//! otherwise, a clone without a network included. The worker polls the pidfd of the announcer of
//! a record that publishes them with the session's stream, in its loop, and nothing else: once
//! the announcer ended, before the clone is ready or after, the gateway keeps publishing the
//! listeners that it last announced, and learns of no later change of them, so the activation
//! fails finally with `nvx::protocol::status`'s `ACTIVATION_NETWORK`, as an application whose
//! announcer could not start does.
//!
//! Until the clone is ready, its session takes nothing but that `Activate`: another reset, any
//! other message, a `Stop` and a `Ping` included, an `Activate` that the activation refuses, one
//! whose header announces more bytes than a record can have, which the framing refuses before it
//! takes any byte of its body, and the passing of the activation's deadline, which ends
//! `activation::ACTIVATION_TIMEOUT` after the repair's acknowledgement of the restore, fail the
//! activation finally, as does an application that failed, which nothing retries. Once the clone
//! is ready, the worker answers a retry of the same `Activate`, byte for byte, with the same
//! `Ready`, without applying it again, and fails the activation on another. A failed activation
//! ends the worker, and with it the VM, with the status of `nvx::protocol::status` of the
//! failure's class, from 50 to 58, as `activation::failure_status` selects it, once the worker
//! dropped its executions and wrote what it queued, for at most [`UNACTIVATED_DRAIN`], so that a
//! stream that takes nothing, as once the broker failed, does not keep the clone from ending, and
//! whatever the stream does then. A broker that closes the stream before the clone is ready ends
//! it the same way, with [`SESSION_STATUS`], as [`Worked::Abandoned`] tells, and once the clone is
//! ready, as the worker of a VM that boots cold ends. The worker of a VM that boots cold, which
//! has no activation, refuses an `Activate` as it refuses any message that it does not take in
//! its state.
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
//!   describes it: in new mount, PID, and UTS namespaces, with the root moved over `/`, with the
//!   sandbox's hostname, the entry's default, `nvx-sandbox`, after a cold boot, and the record's in
//!   a clone, and no time status, and with the publication of listeners that the root
//!   configuration names, published unless it names them unpublished, as the entry's default is,
//!   after a cold boot, and that the record's network names in a clone, unpublished without one.
//!   After a cold boot, it sees the image's own machine ID and `/etc/resolv.conf`, so that the
//!   worker binds nothing into the root, and in a clone, the machine ID that the record names,
//!   which the clone's application bound for every execution if the record asks for a generated
//!   one, and, if the record's network names a DNS server, the resolver file that names it, which
//!   the application bound over the root's `/etc/resolv.conf`. It runs as
//!   the identity that exactly one source names: the sandbox's workload identity, which the root
//!   configuration names, with no supplementary groups or capabilities, or, in a sandbox without
//!   one, the identity that its start names, with its supplementary groups, its capabilities, and
//!   its account mode. A start that names an identity in a sandbox with a workload identity, or
//!   none in a sandbox without one, is refused with `EINVAL`. Before the worker prepares the
//!   execution's cgroup, the entry checks the identity, and reads its account in existing-account
//!   mode: it refuses with `EPERM` a capability outside its allowlist, any capability of a user ID
//!   other than 0, and `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` while listeners are published,
//!   with `ENOENT` an identity without the user ID's passwd entry or the group ID's group entry,
//!   with the error of an account database or a root that it cannot read, and with `EINVAL`
//!   otherwise. The program starts in the working directory of its start, or in the root's `/`,
//!   with exactly the environment of its start, and leads a session of its own, without a
//!   controlling terminal, and the process group that the session creates. As PID 1 of its PID
//!   namespace, its end ends every process of the namespace. The guest's network device, its
//!   addresses, and the announcer of its listeners stay with NVX's `/init`, which configures them
//!   before PID 1 starts, but for the announcer of a clone, which the clone's application starts
//!   and the worker supervises.
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

#[cfg(target_arch = "x86_64")]
pub mod activation;
#[cfg(target_arch = "x86_64")]
pub mod capture;
mod connection;
pub mod entries;
mod executions;
pub mod mapping;
mod order;
pub mod probes;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
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
#[cfg(target_arch = "x86_64")]
use nvx::activation::ActivationRecord;
use nvx::gpt::Expected;
use nvx::guest::cgroup::Layout;
#[cfg(target_arch = "x86_64")]
use nvx::guest::clone::{self, Activation, ApplyError, RootTemplate};
use nvx::guest::entry::{self, Entry, Identity, Prepared, Publication};
#[cfg(target_arch = "x86_64")]
use nvx::guest::ingress::Announcer;
use nvx::guest::mappings::{Mapping, SourceRoot};
use nvx::guest::nvxs::GuestConnectionError;
use nvx::guest::process::Child;
use nvx::guest::root::{
    self, Assembly, Bootstrap, Device, Image, Journal, Resources, Root, Upper, UpperKind,
};
use nvx::guest::spawn::{self, Spawned};
use nvx::protocol::export::HostExport;
#[cfg(target_arch = "x86_64")]
use nvx::protocol::nvxs::GuestSession;
use nvx::protocol::root::{
    DISTRO_MMIO_BASE, ImageFormat, RootLayout, RootUpper, SCRATCH_MMIO_BASE,
};
use nvx::protocol::status;

use crate::GUEST_BUILD;
use crate::exec::HostMessage;
use crate::frame::{MAX_BODY, MAX_TEXT_BODY, Message, truncated};
use crate::guest::{self, ServeError, Step};
use crate::probe::{Facts, Probe};
use crate::root::{
    DEFAULT_FIRST_PARTITION_BOUND, JournalPolicy, Listeners, OverlayMode, Ready, RootConfig,
    RootFailure, RootStage,
};
#[cfg(target_arch = "x86_64")]
use activation::CloneState;
#[cfg(target_arch = "x86_64")]
use capture::Admission;
use connection::{Connection, Received};
use executions::{Executions, Sandbox};
use order::Order;

/// The status with which the worker ends once its session failed or the broker closed it, and
/// with which PID 1 ends the VM if a child cannot start or the worker's status cannot be collected.
pub const SESSION_STATUS: u8 = 1;

/// How long the worker waits for its stream, and then its tty, to take what it wrote once a
/// clone's activation failed, or the broker closed the stream before the clone was ready: the
/// clone then ends, and with it the VM, even if they take nothing, as once the broker failed.
pub const UNACTIVATED_DRAIN: Duration = Duration::from_secs(1);

/// The problem with which the worker answers a probe while it runs executions.
pub const EXECUTING: &str = "the worker runs executions, and probes nothing until they end";

/// The most executions that the worker runs at once, those that the host abandoned included, and
/// those that ended but keep their places: with cgroups, those whose cgroups the worker has not
/// removed, as those of the executions that did not settle, and without them, those whose
/// programs the worker forgot before it collected their statuses. The host's side knows it as
/// [`exec::MAX_EXECUTIONS`](crate::exec::MAX_EXECUTIONS).
pub const MAX_EXECUTIONS: usize = crate::exec::MAX_EXECUTIONS;

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

/// How one execution enters the assembled root: the inputs of its `nvx::guest::entry::Entry` that
/// the worker chooses. The entry keeps its defaults for the rest: new mount, PID, and UTS
/// namespaces, with the root moved over `/`, the image's own machine ID and `/etc/resolv.conf`,
/// and no time status, which leave `Entry::bind_runtime` nothing to bind, and every device node.
/// A clone's application binds the machine ID that its record asks the guest to generate once,
/// and the resolver file that names the DNS server of its record's network, if any, for every
/// execution, before any execution enters the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entrance {
    /// The identity: the sandbox's workload identity, or the one that the execution's start names.
    pub identity: Identity,
    /// The hostname, the sandbox's: the entry's default after a cold boot, and the record's in a
    /// clone.
    pub hostname: Vec<u8>,
    /// Whether the sandbox's listeners are published, as the root configuration says, which
    /// refuses `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` while they are.
    pub publication: Publication,
}

impl Entrance {
    /// Returns the entry into the root at `root`.
    pub fn entry(&self, root: impl Into<PathBuf>) -> Entry {
        Entry::new(root, self.identity.clone())
            .hostname(self.hostname.clone())
            .publication(self.publication)
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

    /// Tears the assembled root down, once it stopped the announcer that a clone's application,
    /// through `apply`, started, if any, and removed the runtime's binds that the application made
    /// on it, which keep the root's mount busy.
    fn teardown(&mut self) -> Result<(), String>;

    /// Returns the pidfd of the announcer of the workload's listeners that a clone's application,
    /// through `apply`, started for a record whose network publishes them, which polls readable
    /// once the announcer ended, or `None` without one, or once its status was collected.
    fn announcer(&self) -> Option<BorrowedFd<'_>>;

    /// Collects the status of the announcer of the workload's listeners, if it ended, without
    /// blocking, and returns how it ended, as a problem, or `None` while it runs, or without one.
    fn announcer_ended(&mut self) -> Option<String>;

    /// Applies `record`, the record of the Activate that a clone's activation accepted, to the
    /// clone, as `nvx::guest::clone::apply` does, with `template`, the inputs of the root's
    /// assembly that the clone's template captured, by `deadline`, the activation's, if any, until
    /// which it waits for each device at the latest, and keeps the root that it assembled, with the
    /// runtime's binds that it made on it, which the [teardown](Self::teardown) removes first, the
    /// root of the host export that it mounted, if the record has one, from which
    /// [`map`](Self::map) maps the host's mapping table, and the announcer of the workload's
    /// listeners that it started, if the record's network publishes them, whose
    /// [pidfd](Self::announcer) the worker polls: in place of a root configuration's
    /// [layout](Self::command_line), assembly, [export](Self::mount_export), and account check.
    /// The template's VM [bootstrapped](Self::bootstrap) the guest before its capture.
    ///
    /// Returns the identity in which every execution runs, that of the record's workload identity,
    /// whose account the application checked in the assembled root, if the record has one. Fails
    /// as `nvx::guest::clone::apply` does, with an [`ApplyError`].
    #[cfg(target_arch = "x86_64")]
    fn apply(
        &mut self,
        record: &ActivationRecord,
        template: &RootTemplate,
        deadline: Option<Instant>,
    ) -> Result<Option<Identity>, ApplyError>;
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
    /// The broker closed the stream at a record boundary before a clone was ready, whose
    /// activation can complete no more, which left the worker's fence of workload admission
    /// closed.
    Abandoned,
    /// A clone's activation failed, finally: before the clone was ready, which left the worker's
    /// fence of workload admission closed, or once it was ready, as once the announcer of the
    /// workload's listeners that its application started ended.
    Unactivated {
        /// The status of `nvx::protocol::status` of the failure's class, with which the worker
        /// ends.
        status: u8,
        /// The failure, as `nvx::guest::clone::ActivationFailure` describes it: the state in which
        /// the activation failed, and its cause.
        problem: String,
    },
}

impl Worked {
    /// Returns the status with which the worker ends: 0 once the host stopped it, the stage's
    /// [exit status](RootStage::exit_status) once a stage failed, [`SESSION_STATUS`] once the
    /// broker closed the stream, before a clone was ready too, and the status of the failure's
    /// class once a clone's activation failed, from 50 to 58.
    pub fn status(&self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Failed(failure) => failure.stage().exit_status(),
            Self::Closed | Self::Abandoned => SESSION_STATUS,
            Self::Unactivated { status, .. } => *status,
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
    serve(Connection::new(stream)?, operations, Phase::Configuring)
}

/// Runs the session of a clone, which a restore of a template's snapshot started, on `stream`, the
/// guest's end of the control session, which it makes nonblocking, with the root operations
/// `operations`, as [`work`] does for a VM that boots cold, but through the clone's `activation`,
/// as the [module documentation](self#clones) describes, until the host stops the worker, the
/// activation fails, or the broker closes the stream.
///
/// `activation` is `nvx::guest::clone::Activation::new` of the clone that the worker repaired,
/// behind the fence that its capture's barrier handed over closed, in its transport bootstrap.
/// `session` is the guest's role of the session that the template's VM attached on the same tty
/// before its capture, and whose first epoch it acknowledged, which the reset that the restore
/// queued starts again: the worker attaches no session. `template` holds the inputs of the root's
/// assembly that the template captured, with which the worker [applies](RootOperations::apply)
/// the record of the host's `Activate`. `acknowledged` is when the repair acknowledged the
/// restore, `nvx::guest::clone::Repaired::acknowledged_at`, if it did, from which the
/// `activation::ACTIVATION_PROBE` reports the events of the clone.
///
/// It ends as [`work`] does, and with [`Worked::Unactivated`] once the activation failed, or
/// [`Worked::Abandoned`] once the broker closed the stream before the clone was ready, after which
/// the worker drops its executions, which ends their programs, and writes what it queued, waiting
/// while the stream takes none, for at most [`UNACTIVATED_DRAIN`], and whatever the stream does.
#[cfg(target_arch = "x86_64")]
pub fn work_clone<S: Read + Write + AsFd>(
    stream: S,
    session: GuestSession,
    operations: &mut dyn RootOperations,
    activation: Activation<Admission>,
    template: RootTemplate,
    acknowledged: Option<Instant>,
) -> Result<Worked, ServeError> {
    let state = CloneState::new(activation, template, acknowledged);
    serve(
        Connection::resume(stream, session)?,
        operations,
        Phase::Clone(Box::new(state)),
    )
}

/// Where the worker is in its order: before its root is ready, through a root configuration or a
/// clone's activation, and once it is, as the [module documentation](self) describes.
enum Phase {
    /// The worker of a VM that boots cold awaits its root configuration.
    Configuring,
    /// The root is ready: the worker's order.
    Ready(Order),
    /// The worker of a clone takes the host's messages through the clone's activation, before the
    /// clone is ready, and once it is, in its order then.
    #[cfg(target_arch = "x86_64")]
    Clone(Box<CloneState>),
}

impl Phase {
    /// Takes the session's reset, which the connection acknowledged already, and returns how the
    /// worker ends once the reset ended the session: once it failed a clone's activation.
    fn reset(&mut self) -> Option<Worked> {
        match self {
            Self::Configuring | Self::Ready(_) => None,
            #[cfg(target_arch = "x86_64")]
            Self::Clone(state) => state.reset(),
        }
    }

    /// Returns how the worker ends once the broker closed the stream at a record boundary:
    /// [`Worked::Abandoned`] before a clone was ready, and [`Worked::Closed`] otherwise.
    fn closed(&self) -> Worked {
        match self {
            Self::Configuring | Self::Ready(_) => Worked::Closed,
            #[cfg(target_arch = "x86_64")]
            Self::Clone(state) if !state.is_ready() => Worked::Abandoned,
            #[cfg(target_arch = "x86_64")]
            Self::Clone(_) => Worked::Closed,
        }
    }

    /// Returns how the worker ends once the session's receipt failed with `error`, if its phase
    /// takes the failure: a clone's activation takes an `Activate` that the framing refused
    /// unread, as longer than a record can be, which fails it. Otherwise the error ends the
    /// session.
    #[cfg_attr(not(target_arch = "x86_64"), allow(unused_variables))]
    fn refused(&mut self, error: &ServeError) -> Option<Worked> {
        match self {
            Self::Configuring | Self::Ready(_) => None,
            #[cfg(target_arch = "x86_64")]
            Self::Clone(state) => state.refused(error),
        }
    }

    /// Completes what the phase defers until the session took everything that it received, and
    /// returns the worker's step then, if it deferred anything: a clone's activation, with the
    /// application that ran meanwhile, through `operations`, after which `executions` run in the
    /// clone's sandbox.
    #[cfg_attr(not(target_arch = "x86_64"), allow(unused_variables))]
    fn settle(
        &mut self,
        operations: &mut dyn RootOperations,
        executions: &mut Executions,
    ) -> Option<Step<Worked>> {
        match self {
            Self::Configuring | Self::Ready(_) => None,
            #[cfg(target_arch = "x86_64")]
            Self::Clone(state) => state.settle(operations, executions),
        }
    }

    /// Handles `message` at `now`, through `operations`, and hands a message about an execution to
    /// `executions`, which queue what they answer on `connection`, and returns the worker's step.
    fn handle<S: Read + Write + AsFd>(
        &mut self,
        operations: &mut dyn RootOperations,
        executions: &mut Executions,
        connection: &mut Connection<S>,
        now: Instant,
        message: Message,
    ) -> Result<Step<Worked>, ServeError> {
        match self {
            Self::Configuring => {
                let (step, ready) = configure_root(operations, executions, message)?;
                if let Some(order) = ready {
                    *self = Self::Ready(order);
                }
                Ok(step)
            }
            Self::Ready(order) => {
                serve_root(operations, order, executions, connection, now, message)
            }
            #[cfg(target_arch = "x86_64")]
            Self::Clone(state) => state.handle(operations, executions, connection, now, message),
        }
    }

    /// Returns when the worker's session ends unless something happens, if it may: a clone's
    /// activation's deadline, until the clone is ready.
    fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Configuring | Self::Ready(_) => None,
            #[cfg(target_arch = "x86_64")]
            Self::Clone(state) => state.deadline(),
        }
    }

    /// Returns how the worker ends once its session's deadline passed: a clone's activation's,
    /// before the clone was ready.
    fn tick(&mut self) -> Option<Worked> {
        match self {
            Self::Configuring | Self::Ready(_) => None,
            #[cfg(target_arch = "x86_64")]
            Self::Clone(state) => state.tick(),
        }
    }
}

/// Runs the worker's session on `connection`, from `phase` on, with the root operations
/// `operations`, as [`work`] and [`work_clone`] describe.
fn serve<S: Read + Write + AsFd>(
    mut connection: Connection<S>,
    operations: &mut dyn RootOperations,
    mut phase: Phase,
) -> Result<Worked, ServeError> {
    let mut executions = Executions::new();
    loop {
        connection.flush()?;
        while !connection.is_overloaded() {
            let received = match connection.receive() {
                Ok(received) => received,
                Err(error) => match phase.refused(&error) {
                    Some(worked) => {
                        executions.clear();
                        return end(&mut connection, worked);
                    }
                    None => return Err(error),
                },
            };
            let message = match received {
                // The session took everything that it received: a clone's activation completes
                // with the application that ran meanwhile, if any.
                None => {
                    if let Some(step) = phase.settle(operations, &mut executions)
                        && let Some(worked) = replied(&mut connection, step)?
                    {
                        return end(&mut connection, worked);
                    }
                    break;
                }
                Some(Received::Closed) => {
                    // The broker may have closed only its end of the stream, and read on: what
                    // the worker queued reaches it, as once the host stopped the worker, after
                    // the worker dropped its executions, which a stalled reader then holds back
                    // no more.
                    executions.clear();
                    return end(&mut connection, phase.closed());
                }
                Some(Received::Reset) => {
                    executions.reset(Instant::now());
                    if let Some(worked) = phase.reset() {
                        executions.clear();
                        return end(&mut connection, worked);
                    }
                    continue;
                }
                Some(Received::Message(message)) => message,
            };
            // Each message takes its own instant, so that a root's assembly, a probe, or a
            // launch that came before it takes nothing from the timers of an execution that it
            // starts.
            let now = Instant::now();
            let step = phase.handle(operations, &mut executions, &mut connection, now, message)?;
            if let Some(worked) = replied(&mut connection, step)? {
                return end(&mut connection, worked);
            }
        }
        executions.run(Instant::now(), &mut connection)?;
        connection.credit()?;
        // A clone's activation fails once its deadline passed before the clone was ready, whether
        // the host sent anything or not.
        if let Some(worked) = phase.tick() {
            executions.clear();
            return end(&mut connection, worked);
        }
        // While records back up on the stream, the executions read no output, and may keep what
        // they read in their runners, which no descriptor tells: once a flush ends the backlog,
        // the next turn comes at once.
        let backlogged = connection.is_backlogged();
        connection.flush()?;
        if backlogged && !connection.is_backlogged() {
            continue;
        }
        let announcer = operations.announcer();
        // Once a clone's announcer ended, whenever it ends, the gateway keeps publishing the
        // listeners that it last announced, and learns of no later change of them: the clone's
        // activation fails finally with the network's class, which ends the clone.
        if wait(&connection, &executions, phase.deadline(), announcer)?
            && let Some(problem) = operations.announcer_ended()
        {
            executions.clear();
            let worked = Worked::Unactivated {
                status: status::ACTIVATION_NETWORK,
                problem,
            };
            return end(&mut connection, worked);
        }
    }
}

/// Queues the replies of `step` on `connection`, and returns how the worker ends, if the step ends
/// it.
fn replied<S: Read + Write + AsFd>(
    connection: &mut Connection<S>,
    step: Step<Worked>,
) -> Result<Option<Worked>, ServeError> {
    let (replies, ended) = match step {
        Step::Continue(replies) => (replies, None),
        Step::End(replies, worked) => (replies, Some(worked)),
    };
    for reply in &replies {
        connection.send(reply)?;
    }
    Ok(ended)
}

/// Ends the session on `connection` as `worked` tells, once the worker dropped its executions:
/// writes what the worker queued for the stream, waiting while the stream takes none, and returns
/// `worked`. A clone that ends before it is ready, or whose activation failed, ends with its own
/// status whatever its stream does: the worker waits for the stream for at most
/// [`UNACTIVATED_DRAIN`], and a failure of the stream changes nothing.
fn end<S: Read + Write + AsFd>(
    connection: &mut Connection<S>,
    worked: Worked,
) -> Result<Worked, ServeError> {
    match worked {
        Worked::Abandoned | Worked::Unactivated { .. } => {
            let now = Instant::now();
            let _ = connection.finish(Some(now.checked_add(UNACTIVATED_DRAIN).unwrap_or(now)));
        }
        _ => connection.finish(None)?,
    }
    Ok(worked)
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
    guest::on_terminal(tty, |_| work(open_tty(tty)?, operations))
}

/// Opens the tty at `tty` again, for reading and writing, without making it the controlling
/// terminal, so that the open file that the worker makes nonblocking is its own, while
/// [`guest::on_terminal`] holds the tty open in raw mode.
///
/// Fails with [`ServeError::Terminal`] if the tty cannot be opened.
fn open_tty(tty: &Path) -> Result<File, ServeError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(OFlag::O_NOCTTY.bits())
        .open(tty)
        .map_err(|error| {
            let problem = format!("cannot open {}: {error}", tty.display());
            ServeError::Terminal(io::Error::new(error.kind(), problem))
        })
}

/// Waits until the session's stream, an execution's descriptor, or `announcer`, the pidfd of the
/// announcer of a clone's listeners, if any, polls ready, the earliest deadline of the executions
/// passes, or `deadline`, the session's own, if it has one; at once if the connection holds bytes
/// that the worker takes next. The worker waits for the host's messages unless the connection is
/// overloaded, and for the executions' output unless it is backlogged. Returns whether the
/// announcer's pidfd polled readable, once the announcer ended.
fn wait<S: Read + Write + AsFd>(
    connection: &Connection<S>,
    executions: &Executions,
    deadline: Option<Instant>,
    announcer: Option<BorrowedFd<'_>>,
) -> Result<bool, ServeError> {
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
    let announcing = announcer.map(|pidfd| {
        descriptors.push(PollFd::new(pidfd, PollFlags::POLLIN));
        descriptors.len() - 1
    });
    let deadline = [executions.deadline(), deadline]
        .into_iter()
        .flatten()
        .min();
    let timeout = if connection.has_input() && !overloaded {
        PollTimeout::ZERO
    } else {
        match deadline {
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
        Ok(_) | Err(Errno::EINTR) => Ok(announcing.is_some_and(|index| {
            descriptors[index]
                .revents()
                .is_some_and(|events| !events.is_empty())
        })),
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

/// Checks the root configuration `body`, reads the boot layout of the root and the host export,
/// bootstraps the guest, assembles the root, mounts the export out of it, if the kernel command
/// line names one, and checks the account of the sandbox's workload identity there, if the
/// configuration names one, through `operations`. Returns the configuration, with the sandbox in
/// which the executions run, with the publication of listeners that the configuration names, or
/// the failure of the stage that failed: the export's mount ends the assembly.
fn assemble(
    operations: &mut dyn RootOperations,
    body: &[u8],
) -> Result<(RootConfig, Sandbox), RootFailure> {
    let config = RootConfig::decode(body)
        .map_err(|error| RootFailure::new(RootStage::Configuration, error.problem()))?;
    let (layout, export) = layout(operations, &config)
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
    let publication = match config.listeners {
        Some(Listeners::Published) | None => Publication::Published,
        Some(Listeners::Unpublished) => Publication::Unpublished,
    };
    let sandbox = Sandbox::new(config.workload_identity, publication);
    if let Some((uid, gid)) = sandbox.identity {
        let entrance = Entrance {
            identity: Identity::new(uid, gid),
            hostname: sandbox.hostname.clone(),
            publication: sandbox.publication,
        };
        operations.prepare(&entrance).map_err(|error| {
            RootFailure::new(
                RootStage::Identity,
                format!("cannot enter the root as the workload identity {uid}:{gid}: {error}"),
            )
        })?;
    }
    Ok((config, sandbox))
}

/// Reads the boot layout of the root, and the host export, if any, from the kernel command line,
/// through `operations`, and checks that the worker assembles them as `config` configures it: an
/// EROFS image or a GPT disk, over an ext4 scratch, or over RAM if `config` configures no scratch,
/// with a bound of the first partition for a GPT disk alone, and with a host export, the source of
/// the mapping table, if `config` announces mappings.
fn layout(
    operations: &mut dyn RootOperations,
    config: &RootConfig,
) -> Result<(RootLayout, Option<HostExport>), String> {
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
    Ok((layout, export))
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
/// failures, once it removed the runtime's binds that a clone's application made on it.
pub struct LibraryRoot {
    /// The process ID of PID 1's unrelated child, which the probes check, if PID 1 named one.
    sibling: Option<u32>,
    /// What the bootstrap prepared, once it ran.
    bootstrap: Option<Bootstrap>,
    /// The descriptors that probes keep open until the worker ends, such as those of `hold`.
    /// They close before the root drops.
    held: Vec<File>,
    /// The root of the host export, from which the worker maps the host's mapping table, once the
    /// root mounted the export, as the worker of a VM that boots cold mounts it, or as a clone's
    /// application does. It keeps the export busy, so it closes before the root drops, and before
    /// its teardown.
    export: Option<SourceRoot>,
    /// The root, from its assembly until its teardown.
    root: Option<Assembled>,
}

/// A root that the worker assembled: a cold boot's, from its root configuration, or a clone's,
/// which the application of its activation record assembled.
// The worker holds one root for its life, so the larger variant's size costs nothing, and boxing
// either would add an allocation for nothing.
#[allow(clippy::large_enum_variant)]
enum Assembled {
    /// The root of a VM that boots cold.
    Cold(Root),
    /// A clone's root and workload, with the runtime's binds that the application made on the
    /// root, which its teardown, and its drop, remove before they unmount the root.
    #[cfg(target_arch = "x86_64")]
    Clone(clone::Applied),
}

impl Assembled {
    /// Returns the root.
    fn root(&self) -> &Root {
        match self {
            Self::Cold(root) => root,
            #[cfg(target_arch = "x86_64")]
            Self::Clone(applied) => applied.root(),
        }
    }

    /// Returns the root, to mount the host export and mappings in it.
    fn root_mut(&mut self) -> &mut Root {
        match self {
            Self::Cold(root) => root,
            #[cfg(target_arch = "x86_64")]
            Self::Clone(applied) => applied.root_mut(),
        }
    }

    /// Tears the root down: a clone's once it removed the runtime's binds, which keep the root's
    /// mount busy, in the worker's mount namespace, where the application made them. Fails with
    /// the problem, the binds that stay mounted and the unmounts that failed, whose mounts stay.
    fn teardown(self) -> Result<(), String> {
        match self {
            Self::Cold(root) => root.teardown().map_err(|error| error.to_string()),
            #[cfg(target_arch = "x86_64")]
            Self::Clone(applied) => applied.teardown().map_err(|error| error.to_string()),
        }
    }
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
        self.root = Some(Assembled::Cold(root));
        Ok(())
    }

    /// Runs `nvx::guest::root::Root::mount_export` with the export's tag, and the access of its
    /// mode, and keeps the export's root, from which the worker maps the host's mapping table.
    fn mount_export(&mut self, export: &HostExport) -> Result<(), String> {
        let Some(root) = self.root.as_mut().map(Assembled::root_mut) else {
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
        let root = self.root.as_mut().map(Assembled::root_mut);
        let (Some(root), Some(export)) = (root, &self.export) else {
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
        let root = self.root.as_mut().map(Assembled::root_mut);
        let (Some(bootstrap), Some(root)) = (&self.bootstrap, root) else {
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
    /// the export busy, and, for a clone, `nvx::guest::clone::Applied::teardown`, which stops the
    /// announcer that the application started, if any, and removes the runtime's binds that the
    /// application made on the root first, since they keep the root's mount busy too. A root whose
    /// teardown failed keeps its mounts.
    fn teardown(&mut self) -> Result<(), String> {
        self.export = None;
        match self.root.take() {
            Some(root) => root.teardown(),
            None => Ok(()),
        }
    }

    /// Lends the pidfd of the announcer that a clone's application started, through
    /// `nvx::guest::clone::Applied::announcer`.
    fn announcer(&self) -> Option<BorrowedFd<'_>> {
        match &self.root {
            #[cfg(target_arch = "x86_64")]
            Some(Assembled::Clone(applied)) => applied.announcer().map(Announcer::pidfd),
            _ => None,
        }
    }

    /// Collects the status of the announcer that a clone's application started, with
    /// `nvx::guest::ingress::Announcer::try_wait`.
    fn announcer_ended(&mut self) -> Option<String> {
        match &mut self.root {
            #[cfg(target_arch = "x86_64")]
            Some(Assembled::Clone(applied)) => match applied.announcer_mut()?.try_wait() {
                Ok(None) => None,
                Ok(Some(status)) => Some(format!(
                    "the announcer of the workload's listeners ended: {status}"
                )),
                Err(error) => Some(format!(
                    "the status of the announcer of the workload's listeners cannot be collected: \
                     {error}"
                )),
            },
            _ => None,
        }
    }

    /// Runs `nvx::guest::clone::apply`, and keeps the root and the runtime's binds that it made,
    /// the announcer that it started, if any, and the root of the host export that it mounted, if
    /// the record has one, from which the worker maps the host's mapping table, as after a cold
    /// boot.
    #[cfg(target_arch = "x86_64")]
    fn apply(
        &mut self,
        record: &ActivationRecord,
        template: &RootTemplate,
        deadline: Option<Instant>,
    ) -> Result<Option<Identity>, ApplyError> {
        let mut applied = clone::apply(record, template, deadline)?;
        let identity = applied.identity().cloned();
        self.export = applied.take_export();
        self.root = Some(Assembled::Clone(applied));
        Ok(identity)
    }
}

impl EntryOperations for LibraryRoot {
    /// Runs `Entry::prepare` on the assembled root.
    fn prepare(&mut self, entrance: &Entrance) -> Result<Prepared, entry::Error> {
        let Some(root) = self.root.as_ref().map(Assembled::root) else {
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
