# Guest-agent guidelines

`nvx` parses the contracts, captures the guest and drives a guest restore, but
builds no space and runs no executions: each init agent does both, and `nvx`
enforces none of the obligations below. The one exception is the time daemon,
which `TimeAbi::start_daemon` forks.

These guidelines give an init agent's contract: how it builds its guest's
space, runs its executions, coordinates a capture with its host and completes
a restore. `nvx-init`, NVX's PID 1 for layered roots, is the worked example
throughout. The design's
[Guest Agents and Workload Model](design/4.2-guest-agents-and-workload-model.md)
and [Isolation and Resource Model](design/4.3-isolation-and-resource-model.md)
give the architecture and its boundaries, and the rustdoc of
[`guest`](../nvx-rs/src/guest/mod.rs),
[`guest::snapshot`](../nvx-rs/src/guest/snapshot.rs),
[`guest::restore`](../nvx-rs/src/guest/restore.rs) and
[`guest_restore`](../nvx-rs/src/guest_restore/mod.rs) the public mechanisms.
The [`guest-workflows`](../nvx-rs/examples/guest-workflows.rs) example compiles
an init's calls of them, through the public API alone: its time daemon, its
control session, a capture, a guest restore and the host's mapping table.

## Ownership

An init reads the public boot contracts at a cold boot, or a restore's
guest-restore record, and builds the VM's one space from them. It owns the
root, with its runtime mounts and view binds; the mapper and the descriptors of
the mappings' source and target roots; its helpers; its control session; and
every execution. It keeps each owner for as long as what it owns is in use: a
reset of the transport is neither a new root nor a new guest restore.

The library's calls leave all of this to the init:

- `guest::snapshot::capture` stops no execution, drives no transport and never
  powers the VM off: it returns how the capture ended, or a failure with its
  disposition.
- `guest::restore::GuestRestore` holds no workspace or admission fence, makes
  no system call and starts no process: the init feeds it what its session
  receives, and does what it returns.
- `TimeAbi::start_daemon` forks the time daemon, which the init supervises as
  a helper. nvx-init's early boot, the shell `/init`, starts it with
  `nvx-time boot`, and the announcer when the network publishes the workload's
  listeners, then replaces itself with nvx-init, which inherits both as its
  children.

One other call starts processes, which an init can avoid: `capture`, whose
repair finishes the restore through the time ABI's private
[`TimeAbi::restore_finish`](../nvx-rs/src/guest/time/api.rs). In report-only
mode, the finish runs the restore's deferred time work itself when
no time daemon runs, in a calling process that must have no other thread: it
forks a child, which waits for a grandchild that forces an RCU grace period,
and collects only the child, so a grandchild that outlives the RCU stall
timeout passes to PID 1, or to the nearest subreaper. No call starts the
announcer, which is a program, `/sbin/nvx-ingress`: an init runs it itself,
or has `nvx-ingress start` run it from a socket that it binds for it.
nvx-init's time daemon runs for the VM's life.

## Build the space

NVX's space is internal reference code that nvx-init implements, not public
API: the modules that hold it, `guest::root`, `guest::view`, `guest::gpt`, the
mapper of `guest::mappings`, and `guest::restore::apply` with its types,
are private to the crate. Another init builds its own space, or adapts this
code; it builds on no space module of `nvx`.

The space's inputs are public contracts:

- At a cold boot, nvx-init reads them from the kernel command line: the root's
  tokens, which `protocol::root::RootLayout` parses; the view's, which
  `protocol::view` parses; the host export's, which
  `protocol::export::HostExport` parses, or the live shares of NVX's CLI,
  whose targets `protocol::share` restricts; the DNS server, which
  `protocol::cmdline::dns_server` reads; and the number of host mappings,
  which `MappingTable::announced` reads.
- After a restore, the accepted `guest_restore::GuestRestoreRecord` holds the
  launch's settings, and the configuration that the snapshot captured, the
  same for each restore, holds the assembly's fixed inputs: the runtime
  directory, the block devices, the host filesystem device's tag, the NIC's
  gateway, and how long the assembly waits for the devices. Never take a
  captured launch's view or network in place of the restore's own record.
- The host's mapping table, when it holds any mapping, arrives once the init
  is ready, in `MAPS` parts, which `protocol::maps` decodes.

nvx-init's space keeps these rules:

- **The root.** The image on the `distro` device, an EROFS file system or a
  GPT disk, whose identity the root checks when the contract names one, lies
  under an overlay with its upper, mounted `nosuid,nodev` below the runtime
  directory, `/run/nvx`. The upper is the ext4 scratch, or a tmpfs whose cap
  the contract may set from 1 to 90 percent of the VM's memory, and which
  otherwise takes the kernel's default, half of it. A restore's upper is
  fresh: the assembly refuses one whose `upper` or `work` directory holds
  anything.
- **The runtime file systems.** The root makes every mount of its tree
  `nosuid` and `nodev`, then mounts `/proc`, a read-only `/sys`, a `/dev` that
  holds only the null, zero, full, random, urandom and tty devices, with
  `/dev/pts` and `/dev/shm`, and `/run`, and binds the time status read-only
  at `/run/nvx/time`. They come before any live share or mapping, since the
  `/run` tmpfs would hide one beneath `/run`: a cold boot's live shares follow
  them.
- **The view.** It is VM-wide: the hostname; the image's own machine ID
  (`MachineIdMode::Image`), or one that the init generates and binds read-only
  over it (`Generated`); and, when the boot contract or this restore's record
  names a DNS server, a resolver file that names it, bound over the image's.
  The root owns these binds, those made before a later failure included.
- **The mappings.** The host export, from which mappings come, is mounted
  outside the root, below the runtime directory, where only the guest's root
  user reaches it. The init opens the root's descriptor as the mappings'
  target root, adds the runtime's `/run` mount to it, and makes the mapper of
  it, which refuses the targets that the runtime reserves; it keeps the
  export's source root. A mapping binds beneath these descriptors, never by
  path, so both outlive the switch.
- **The helpers.** Every helper runs, and every descriptor that later work
  needs is open, before the switch: the root holds neither the initramfs's
  programs nor the control device. nvx-init's early boot starts its helpers,
  and nvx-init opens its control terminal before it switches; a restore starts
  its announcer there too. PID 1 and its helpers outrank every execution, as
  [Run the executions](#run-the-executions) describes.
- **The one switch.** The init moves the root onto `/`, then enters it with
  `chroot` and `chdir`, once: executions need no root of their own. From then
  on nothing unmounts the root, and the VM ends by syncing and powering off.
  Before the switch, a step that fails admits no workload: the root tries to
  unmount what it mounted, in reverse, and nvx-init ends the VM, whatever the
  cleanup leaves mounted.

### The space's steps, checked

These excerpts are nvx-init's unit test
[`the_reference_space_steps_compile`](../nvx-rs/src/guest/init/boot/tests.rs),
which compiles them without mounting anything or switching the developer
host's root. They use the space's own names, of `root`, `view`, the mapper and
`restore::{apply, Applied, RootTemplate}`, which are NVX's internal
reference code, not public API. A cold boot's space takes these steps in the
order of [`boot::cold_boot`](../nvx-rs/src/guest/init/boot.rs), which also
mounts the live shares of NVX's CLI after the runtime file systems:

```rust
use std::error::Error;
use std::net::IpAddr;
use std::path::Path;
use std::time::Instant;

use crate::guest::mappings::{Mapper, Reserved, SourceRoot, TargetRoot};
use crate::guest::restore::{self, Applied, RootTemplate};
use crate::guest::root::{self, Assembly, Root};
use crate::guest::view;
use crate::guest_restore::GuestRestoreRecord;
use crate::protocol::export::HostExport;
use crate::protocol::view::View;

// A cold boot, as `boot::cold_boot` builds it.
let _cold = |assembly: &Assembly,
             export: &HostExport,
             vm_view: &View,
             dns: Option<IpAddr>|
 -> Result<(Root, Mapper, SourceRoot), Box<dyn Error>> {
    let mut root = root::assemble(assembly)?;
    let source = root.mount_export(&export.tag, export.mode.into())?;
    root.mount_runtime(Some(Path::new("/run/nvx/time")))?;
    let mut target = TargetRoot::open(root.path())?;
    target.add_mount(&root.path().join("run"))?;
    let mapper = Mapper::new(target, Reserved::container_root());
    view::apply(&mut root, vm_view, dns)?;
    // Start the helpers and open the descriptors that later work needs before the switch.
    root.switch()?;
    Ok((root, mapper, source))
};
```

A restore's application assembles the record's root, mounts its export and
applies its view, with its DNS server, and starts no process; the init then
mounts the runtime file systems, opens the mappings' handles, starts the
announcer when the record publishes the workload's listeners, and switches:

```rust
// A restore, as `boot::Restored::apply` builds it from the accepted record.
let _restored = |record: &GuestRestoreRecord,
                 template: &RootTemplate,
                 deadline: Option<Instant>|
 -> Result<(Applied, Mapper, Option<SourceRoot>), Box<dyn Error>> {
    let mut applied = restore::apply(record, template, deadline)?;
    applied
        .root_mut()
        .mount_runtime(Some(Path::new("/run/nvx/time")))?;
    let mut target = TargetRoot::open(applied.root().path())?;
    target.add_mount(&applied.root().path().join("run"))?;
    let mapper = Mapper::new(target, Reserved::container_root());
    let source = applied.take_export();
    // Start the announcer here when `applied.publication()` publishes the listeners.
    applied.root_mut().switch()?;
    Ok((applied, mapper, source))
};
```

### The host's mapping table

The public [`MappingTable`](../nvx-rs/src/guest/mappings/table.rs) keeps the
account of the host's mapping table, and the init's mapper binds the mappings
that it hands out: one does not replace the other. The host announces how many
mappings its table holds, at a cold boot with `nvx_maps=COUNT`, which it leaves
out for none, and after a restore in the record. Once the init is ready, it
sends a table that holds any mapping in parts of consecutive entries, and an
empty one not at all:

- **Parts in order.** `MappingTable::apply` refuses a part as a whole, and
  changes nothing, when it does not decode, holds no entry, does not start at
  the entry that the table expects next, ends past the announced count, or
  holds an entry whose source is empty, which names the export's root, or
  whose path a `Mapping` refuses. Otherwise it hands the part's mappings, in
  order, to the init's mapper.
- **A latch after a failure.** A mapping that fails keeps the mappings before
  it and latches the table, which refuses every later part and is never
  complete; the init reports the failure and its entry. An init whose protocol
  ends the table at a refused part latches it then, with `MappingTable::latch`.
- **A complete table before any workload.** The init starts no workload until
  `MappingTable::is_complete`: until it applied every announced mapping and
  none failed. A table of no mappings is complete at once.

nvx-init answers a refused part with `invalid-request`, a failed mapping with
`mapping-failed`, after a diagnostic that names its entry, and refuses every
execution with `mappings-incomplete` until its table is complete. The
guidelines quote this excerpt of the [`guest`](../nvx-rs/src/guest/mod.rs)
rustdoc, where it compiles as a public-API doc test:

```rust
use nvx::guest::mappings::{Mapping, MappingTable, TableError};

fn apply_part<E>(
    table: &mut MappingTable,
    part: &[u8],
    mount: impl FnOnce(&[Mapping]) -> Result<(), (usize, E)>,
) -> Result<bool, TableError<E>> {
    table.apply(part, mount)?;
    Ok(table.is_complete())
}
```

## Run the executions

An init runs its executions itself. nvx-init runs them one at a time, at the
host's request, as plain processes in the VM's one space, with its own
plain-process core:

- **Before the fork**, it builds the program's arguments and environment. The
  default environment is nvx-init's own, with the workload account's `HOME`,
  `USER` and `LOGNAME` and the working directory's `PWD`; the request's
  entries replace it, or layer over it when the request asks. The account is
  the VM's workload identity, the `nvx_identity=UID:GID` of a cold boot or the
  identity of a restore's activate request, which nvx-init resolves in the
  root's user and group databases once, before its switch, and creates when
  the boot, with `nvx_workload_account=create`, or the activate request asks
  for it.
- **In the child**, which leads a process group of its own, with a pipe of
  the input that the host streams as its standard input, which nvx-init
  closes after the input's end, and pipes as its standard output and error,
  the execution resets its priority, sets its credentials, changes to its
  working directory and execs. It resets its out-of-memory score adjustment
  and its nice value to 0, then drops its capability bounding set and takes
  the account's user and group IDs, with no supplementary groups, empty
  capability sets and `no_new_privs`. It then enters the requested working
  directory, `/` by default, with those credentials, resets its signals, and
  execs. A step that fails reports its stage and `errno` to the init and exits
  with status 125: the execution fails, rather than run as another identity or
  in another directory.
- **End, cancellation, timeout and reset** each kill the execution's process
  group: nvx-init kills it when the execution's leader ends, normally or not,
  when the host cancels the execution or shuts the VM down while it runs,
  when its timeout passes, and when a session reset abandons it. None of an
  abandoned execution's output reaches the new session: nvx-init reads no
  more of it, and what it had already queued goes out before the reset's
  acknowledgement, in the earlier epoch, whose records the broker drops.
  PID 1 collects the status of every child, orphans included. A process
  group is a cleanup scope, not containment: a process that leaves its group
  escapes it, as the capture's sweep allows for.

This is priority, not limits. PID 1 and its helpers run with an out-of-memory
score adjustment of -1000 and a nice value of -10, ahead of the executions'
0, which each child restores before it drops its privileges, so that no
workload keeps them. There are no cgroups, namespaces, per-execution roots or
per-execution limits: the VM is the limit, and the one limit within it is the
RAM upper's cap.

## Coordinate a capture

An init that runs concurrent executions keeps them in workspaces, under a
contract that is the init's own: `nvx` provides no workspace trait and
enforces none of it.

- `freeze(deadline)` refuses new executions, then stops those that run. A
  frozen workspace refuses a start rather than hold it until the thaw.
- A failed freeze says whether it undid its work (`undone`). When it did not,
  the VM ends: no work resumes in a partly frozen state.
- Several workspaces freeze in order, all or nothing, and thaw in reverse.
  After a failure with `undone`, those already frozen thaw, in reverse;
  without it, the VM ends and nothing thaws.
- `thaw()` reverses a freeze, and `after_restore()` renews what the
  executions need after a restore, before Ready.

nvx-init has no workspace: it refuses a capture while an execution runs, or
kills the process groups and sweeps when the host asks, as its capture
request says. Without permission to kill, it refuses the request as `busy`
while any process but PID 1 and its registered helpers runs; with it, it ends
a running execution's process group, then sweeps, killing every process but
PID 1 and its helpers again and again until none remains. The checks, the
sweep and the capture's own preparation share one 30-second deadline. Today
nvx-init captures only the Fresh boot of a platform capture,
`nvx_snapshot_tier=platform`, which assembles no root and admits no
execution: a session with a root, cold-booted or restored, refuses the request
with `ENOTSUP` before it checks or kills anything.

An init captures before it mounts anything from the devices that a restore
replaces: the `distro` image, the scratch and the host filesystem device, and
any overlay. `capture` refuses while one is mounted, but the decision to
capture, and the cleanup before it, are the init's.

## Keep the transport silent

The init answers the host's capture request, then sends nothing on its
transport until `capture` returns: no progress, output or other response.
When the call returns:

- After `Captured::Continued`, it thaws its workspaces in reverse, where they
  exist, and reports on the same session. nvx-init reports
  `capture-continued`.
- After a failure whose disposition is `Disposition::Resumed`, it thaws and
  reports the failure on the same session. nvx-init writes the failure to the
  console, after `NVX-INIT-CAPTURE:`, and reports `capture-failed`.
- After `Captured::Restored`, it takes the session's reset and attaches again:
  the restored broker has a fresh instance, restarts at its first epoch with a
  reset queued, and ignores the guest's records until the guest acknowledges
  it. nvx-init waits for the reset, within the guest restore's deadline, fails
  on anything else, and acknowledges it with `GuestConnection::acknowledge`
  before it takes the guest-restore record.

The guidelines quote this excerpt of the
[`guest::snapshot`](../nvx-rs/src/guest/snapshot.rs) rustdoc, where it
compiles as a public-API doc test:

```rust
use std::time::{Duration, Instant};

use nvx::guest::restore::GuestRestore;
use nvx::guest::snapshot::{self, Captured, Devices, Disks, Disposition};

/// What the caller does next; it sent nothing on its transport while `capture` ran.
enum Next {
    /// Thaw the workspaces in reverse, where they exist, and report on the same session.
    Report,
    /// Take the session's reset and acknowledge it, then drive this guest restore to Ready.
    Activate(GuestRestore),
    /// Start nothing, and power the VM off with this status.
    PowerOff(u8),
}

fn capture(devices: &Devices, deadline: Instant, timeout: Duration) -> Next {
    match snapshot::capture(devices, Disks::Fresh, deadline) {
        Ok(Captured::Continued) => Next::Report,
        Ok(Captured::Restored(repaired)) => {
            Next::Activate(GuestRestore::new(&repaired, timeout, Instant::now()))
        }
        Err(failure) => match failure.disposition {
            Disposition::Resumed => Next::Report,
            Disposition::MustEnd { status } => Next::PowerOff(status),
            // A disposition that a later version adds cannot be known to be safe.
            _ => Next::PowerOff(125),
        },
        // Nor can an outcome that a later version adds.
        Ok(_) => Next::PowerOff(125),
    }
}
```

## Complete a restore before Ready

`capture` repairs a restored guest before it returns
`Captured::Restored(repaired)`: it activates the restore's processor and
memory targets, reseeds the kernel's random number generator, and finishes
the restore with its acknowledgement. The init makes one `GuestRestore` of that
`Repaired`, whose deadline counts from the repair's acknowledgement, or from
when the init makes it for a restore that the host did not gate on one, and
never from a session or a retry. A restore then takes these steps, in this
order:

1. `GuestRestore::new`. nvx-init makes it when `capture` returns, with a
   30-second timeout, takes the session's reset, and accepts the activate
   request: the guest restore decodes its record, and checks its restore section
   and the CPUs and memory that the repair reports, before anything has a side
   effect.
2. The space: the application of the record, with this restore's own network
   policy, then the runtime file systems, the announcer and the switch, as
   [the excerpt](#the-spaces-steps-checked) shows. The init applies the
   record's DNS server, and, when the record publishes the workload's
   listeners, starts the announcer before Ready and owns it as a helper.
   nvx-init forks it from PID 1 before its switch, and runs
   `/sbin/nvx-ingress` with the captured NIC's gateway, as the early boot runs
   it at a cold boot. nvx-init also tells the record's proxy to the executions
   that keep their default environment.
3. `after_restore()` on each workspace, where workspaces exist.
4. `GuestRestore::complete`, with the result.
5. The thaw of the workspaces, in reverse, where they exist.
6. Ready. nvx-init answers the activate request, and each retry that joined
   it, with Ready, which carries the record's transaction ID.

nvx-init has no workspace, so it skips the third and fifth steps, not the
order around them. It keeps the applied root, the mapper and its
`MappingTable`, the export's source root, its helpers and the `GuestRestore`
for the VM's life. The host then sends the mapping table, unless it is empty,
and nvx-init records its first execution, once the table is complete, with
`GuestRestore::start`.

A byte-equal retry joins the guest restore in progress, or gets its cached
Ready, on a later session too, and never applies or switches again: nvx-init
keeps the guest restore and its original deadline across sessions, and a reset
on a later session only checks that deadline. A changed request, another
transaction, and an application that failed, wholly or partly, are final.

The packet with which a platform restore resumes the Fresh boot carries no
network settings, and `capture` refuses one that does: the guest-restore
record gives the restore its network.

## Failures

- After `Disposition::Resumed`, the init thaws and reports the failure on the
  same session.
- After `Disposition::MustEnd { status }`, it starts nothing, thaws nothing,
  and powers the VM off with that status: the library returns the disposition
  rather than power the VM off itself. nvx-init writes the failure to the
  console, then syncs and powers off with the status.
- A failed guest restore, wholly or partly applied, never reports Ready: the
  init reports the failure and ends the VM. nvx-init writes it to the console,
  after `NVX-INIT-ACTIVATION:`, and powers off with the status of the
  failure's class, which `nvx::protocol::status` defines, such as 54,
  `GUEST_RESTORE_ROOT`, for a root that does not assemble over its upper, from
  which the host attributes the failure.

## Replay

Each restore resumes the captured memory, so:

- An init never captures live external connections, or any other state that
  a restored guest cannot reuse safely: it closes them before the capture, or
  rebuilds them after the restore.
- It never repeats an irreversible effect because execution resumed from
  captured memory.
- The repair reseeds the kernel's random number generator, but not a runtime's
  own random state: a runtime repairs its randomness before it accepts work
  after a restore, in its workspace's `after_restore()` where it has one.

nvx-init captures a Fresh boot, before any workload runs, so a restore resumes
no workload's state, and its view generates a machine ID only after the
repair's reseed, when the record asks for one.
