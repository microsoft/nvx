# The nvx library

[`nvx`](../nvx-rs/README.md) is the Rust library with which a program runs NVX
microVMs on OpenVMM. It holds mechanisms and value types: a VM's configuration,
its launch in a directory that the program owns, one snapshot format, a
restore that the program drives, verified connections to a running VM, and the
guest's side of a capture and of a restore. It keeps no state, runs no
workload, and builds no guest space: whatever a deployment needs beyond the
mechanisms, its program owns, as [What the caller owns](#what-the-caller-owns)
lists. NVX's own `nvx-rs` command-line interface is one such program, which
keeps its sandboxes and snapshots in stores of its own, private to the crate.

This guide follows a host program through the three workflows of a VM: a
[cold boot](#cold-boot), a [restore](#restore-from-a-snapshot) from a snapshot,
and a [reattach](#reattach-after-a-restart) once the program restarted. Code
that CI compiles through the public API alone shows each one, and the guide
links it rather than repeat it. The crate's [README](../nvx-rs/README.md#layout)
says what each module holds, and its rustdoc gives the contract of each item.
An init agent, which runs in the guest, follows the
[guest-agent guidelines](guest-agent-guidelines.md).

## The library

`nvx` is published on crates.io as a preview, whose API can change in any
release, so a program requires an exact version, as the crate's
[README](../nvx-rs/README.md) explains. Its default feature, `host`, holds the
host's side, and `guest` the guest's, for Linux guests.

A launch takes two values:

- a `host::runtime::Runtime`: the paths of the OpenVMM executable, the guest
  kernel, and the initramfs that it runs, their `RuntimeIdentity`, the
  hypervisor, and the launch options;
- a `host::config::VmConfig`: the VM's processors and memory, its root,
  blockless or an image under an upper, the view that its workload has of it,
  its network, the host paths that it exports, its boot console, and a `Boot`,
  which only a cold boot takes, with the program's kernel command-line tokens
  and, for a capture, a `CaptureConfig`.

`VmConfig::check` checks a configuration without reading the host, and
`host::vm::create` checks a whole launch before it starts anything.

## Cold boot

1. Create the VM's directory with `vm::create_private_dir`, private to the
   current user, and open it with `VmDir::open`: one directory for each VM.
2. `vm::create(&runtime, &config, None, &dir)`, for a `config` with a `Boot`,
   checks the launch, creates the VM's log, and returns a `Prepared` launch
   without starting anything. A program that recovers interrupted launches
   records its `capability()` and `outputs()` now.
3. `vm::spawn(prepared)` starts OpenVMM, detached, and returns the `Launched`
   VM: its verified `Process`, its `VmOutputs`, and its `Capability`, which
   OpenVMM receives on its standard input and the library never writes to a
   file. A program with a launcher of its own takes `Prepared::into_command`
   instead, and follows the launch contract that `VmmCommand` documents.
4. The program speaks its own protocol with its guest, over the `Connection`
   that `Launched::connect` opens: it connects to the VM's control endpoint
   once it has checked that the VM's own process serves it, and authenticates
   with the capability.
5. `vm::stop` asks the guest to end the VM, through a callback that sends the
   request in the program's protocol, terminates the process once a grace
   deadline passed, and returns once the process exited, with the result of
   reading OpenVMM's outcome report, which never hides the exit.

The example of the crate's documentation, in
[`lib.rs`](../nvx-rs/src/lib.rs), which rustdoc compiles, boots and stops a VM
this way, and so do `cold_boot` and `stop` in the compile-only
[`host-workflows`](../nvx-rs/examples/host-workflows.rs) example.
[`external-launch`](../nvx-rs/examples/external-launch.rs) runs a cold boot end
to end, through a launcher of its own.

## Restore from a snapshot

A restore starts a VM from a snapshot: a directory of exactly `key.bin`,
`snapshot.json`, and `openvmm/`, which holds OpenVMM's manifest, memory, and
saved state.

A capture makes one. Its VM boots cold, from a `VmConfig` whose `Boot` holds a
`CaptureConfig`, with a blockless root and no export. The program asks its
guest for a snapshot in its own protocol, and the guest captures the VM with
`guest::snapshot::capture`, after which the VM exits. With the proof of that
exit, from `Process::wait`, `snapshot::finish_capture` checks the capture and
writes its key and description beside `openvmm/`, and the program moves those
three entries into a new, empty directory of its own. `capture` in
[`host-workflows`](../nvx-rs/examples/host-workflows.rs) captures a snapshot
this way, and `capture` in
[`guest-workflows`](../nvx-rs/examples/guest-workflows.rs) takes the guest's
part.

A restore then:

1. takes the program's own lock of the snapshot, on Linux, and opens the
   snapshot with `Snapshot::open`, which checks its strict format;
2. calls `vm::create(&runtime, &config, Some(&snapshot), &dir)`, for a
   `config` without a `Boot`: creation checks the request against the
   snapshot, with `Snapshot::accepts`, at the host's current time, hard-links
   the snapshot's OpenVMM files into the VM directory's `openvmm/`, and binds
   the listener of OpenVMM's restore-ready event;
3. builds the `GuestRestoreRecord` that the guest applies with
   `Prepared::guest_restore_record` and a `TransactionId`, 16 random bytes that
   the program picks for the restore, and starts the VM with `vm::spawn`,
   whose `Launched::ready` holds the listener;
4. drives a `host::restore::RestoreMachine` of the record, which says what to
   do next, while the program does it with its own I/O and reports what it
   observed: it receives the restore-ready event from the VM's own process
   with `ReadyListener::receive`, connects and authenticates, sends the
   record's bytes in its protocol, and reports the guest's Ready, until the
   machine is `Done` or fails. In the guest, the init activates the restore
   with a `guest::restore::GuestRestore`, applies the record in its space,
   and reports Ready;
5. sends the guest its mapping table, if it holds any mapping, and its
   workload in its own protocol once the restore is `Done`, or stops the VM
   when the restore failed.

The program keeps the VM directory's `openvmm/` until the VM exited, then
removes it under its lock. `restore` in
[`host-workflows`](../nvx-rs/examples/host-workflows.rs) drives a restore to
Ready this way, and `activate` in
[`guest-workflows`](../nvx-rs/examples/guest-workflows.rs) is the guest's side.

## Reattach after a restart

Dropping a `Launched` closes the handle of the VM's process, not the VM, which
outlives the program that launched it; on Windows, a program that runs in a job
object starts its VMs outside the job, with the runtime's `windows_breakaway`,
which the job must permit, for them to outlive it. To find the VM again once it
restarted, the program saves, durably and privately, a `vm::SavedVm` when the
spawn returns: the VM directory, the control endpoint, the capability's bytes,
the process's `ProcessRef`, and the runtime's identity, with the `VmOutputs`
that a stop reads. Then:

- `vm::attach(&saved, &runtime, deadline)` checks the runtime's identity,
  holds the process by its reference, checks that it serves the endpoint, and
  authenticates with the saved capability: its `Attached` holds the process
  and the `Connection`.
- `vm::verify(&saved, &identity)` makes the same checks of the runtime and the
  process without connecting, and returns the held `Process`, which the
  program can wait for or stop.
- Both fail with `ErrorKind::RuntimeMismatch` for a VM that another runtime
  launched: the program drives no such VM, but may stop it, through
  `Process::open` of the saved reference.

Their failures neither signal the VM nor change the saved values, which the
program keeps to try again. `attach` in
[`host-workflows`](../nvx-rs/examples/host-workflows.rs) reattaches this way,
and the example of [`host::vm::Connection`](../nvx-rs/src/host/vm/connection.rs),
which rustdoc compiles, connects to a VM that the program launched.

## What the caller owns

Earlier releases of `nvx` kept stores of sandboxes and templates, with their
leases and locks, a blocking clone driver, and the path and image rules of
NVX's sandboxes. The library keeps none of them: in their place, the program
owns the following.

**Paths and image checks.**

- The runtime's files: the program locates OpenVMM, the guest kernel, and the
  initramfs, and labels each with the identity that its install metadata
  records, such as the file's own SHA-256 digest that a release's
  `SHA256SUMS` lists, which the library compares and never computes.
- The runtime's compatibility: the program names the contracts that it speaks
  with its guests, such as its boot tokens and its protocol, in a
  `CompatibilityIdentity` that the library compares and never interprets, and
  changes it whenever it can no longer drive the VMs that it launched, or
  restore the snapshots that it captured, under the earlier one.
- The images: OpenVMM opens a layered root's image and scratch at their paths
  when it starts. The library reads none of their content: a cold boot checks
  only their paths, and a restore their metadata, against the snapshot. The
  program checks the images that it launches, and keeps each path naming the
  image that it checked until OpenVMM opened it, for example by staging its
  images where no other account can write. GPT parsing is internal to NVX's
  guest programs, not a public image-checking utility; the chosen init owns
  interpreting and mounting its images, and NVX's bundled init keeps its GPT
  validation and root assembly.
- The export: an `ExportPlan` holds paths only, which the library checks and
  lowers without reading the host, and which OpenVMM resolves when it starts.
  The program keeps them naming the objects that it means: across that race;
  through a recheck, before each later launch, of the paths that lie in parts
  that a workload may write; and for as long as each denial is to hold, as
  [`host::filesystem`](../nvx-rs/src/host/filesystem/mod.rs) details.
- The VM directory and the runtime's files stay where they are, unchanged,
  from `vm::create` through the spawn, and while the VM runs.

**Durable state.**

- The library keeps no record of VMs, launches, captures, or snapshots. The
  program saves, durably and privately, what it needs to recover: a launch's
  capability and outputs before its spawn, and its `SavedVm` after it; a
  capture's `CaptureContext`, to finish the capture after a restart; and its
  snapshots, which it names and keeps.
- The capability is a secret, which the library never writes to a file: the
  program keeps it private.

**Locks.**

- The library takes no lock that the program sees. On Linux, every user of a
  snapshot takes the program's own lock of it before it opens it, and a
  restore holds that lock from `vm::create` through the restore-ready event or
  the VM's verified exit, and around the removal of the VM directory's
  `openvmm/`. Windows needs no such lock.
- On Windows, a process that the program starts while `vm::spawn` runs may
  inherit the VM's handles: the program keeps its other process starts out of
  the spawn, for example with a lock of its own around both. The spawn's own
  lock is internal to it.
- The program orders its own operations on each VM and each snapshot.

**I/O.**

- The program speaks its own protocol with its guest, over the ordered byte
  stream of a `Connection`, and frames its own messages in it. NVX's own
  control protocol is private to NVX's programs.
- A `RestoreMachine` does no I/O: the program receives the restore-ready event,
  connects, sends the activation record, and reads Ready. Then it sends the
  mapping table, which `VmConfig::mapping_table` returns and
  `protocol::encode_maps_payloads` encodes, but none when the table is empty,
  as it is without an export, and the workload, in its own protocol.
- A program with an event loop of its own polls, on Linux, the descriptor of
  each `Connection`, which is `AsFd`, and takes the native handles
  `Process::exit_handle` and `ReadyListener::into_std`, and checks what their
  documentation asks it to: the library depends on no asynchronous runtime. A
  `Connection` is `Send`, so a program runs it on a thread of its own where
  the platform has no such descriptor, as on Windows.

**Timing.**

- The calls that wait on a VM or its guest take a deadline that the program
  chooses: a connection, its attachment, its sends and receives, a wait for a
  process's exit, and a stop, with its grace period. `ReadyListener::receive`
  takes a timeout instead, which the program computes from its own deadline
  before each receive.
- A `RestoreMachine` keeps no clock and counts no retries: the program reports
  `Expired` once its budget passed, or `Cancelled`, and decides how often a lost
  session reconnects.
- The library reads the host's time when `vm::create` checks that a snapshot
  may be restored now; the program restores a snapshot within its window,
  which the capture's time contract sets.

**Recovery.**

- After a restart, the program finds its VMs again from what it saved, with
  `vm::verify` or `vm::attach`, and decides what becomes of a launch that it
  recorded before a spawn that never returned.
- A failed restore's `RestoreError` stops no process and writes no record: it
  tells the failed phase, the evidence and what it attributes the failure to,
  and whether the guest may have run, and the program decides whether it
  stops the VM, keeps it, or tries again.
- `vm::spawn` and `vm::stop` fail with `ErrorKind::StillRunning` when they
  cannot confirm a process's exit: the program keeps the process of a stop to
  try again, and knows that a spawn's may still run.
- A `finish_capture` that failed can be retried once its cause is corrected,
  while the VM directory and the capture context stay intact.

**Outputs.**

- The VM directory and what the library and OpenVMM create in it are the
  program's: OpenVMM's log and outcome report, a restore's `openvmm/`, a
  capture's files, and, on Linux, the sockets of the control and console
  endpoints and of a restore's restore-ready event, which are named pipes
  outside the directory on Windows. The library removes none of them once a
  creation succeeded: the program removes them when it no longer needs them.
- The program reads the outcome report, through `Stopped::report` or
  `report::read`, to tell how a VM ended.
- The program moves a finished capture's three entries into its own private
  directory, and keeps them private and immutable while it restores from them.

**Process lifecycle.**

- On the host, the library starts OpenVMM, in `vm::spawn`, and no other
  process, and ends one on its own only when its spawn fails. The program
  stops each VM that it launched, with `vm::stop`, or keeps its reference to
  stop it later.
- On Windows, a program that runs in a job object sets the runtime's
  `windows_breakaway`, which its job must permit, for its VMs to outlive it.
- A capture or restore that fails leaves its VM to the program, which stops
  it, as `host-workflows` does.
- In the guest, the library builds no space and runs no execution: the init
  owns its processes, and the
  [guest-agent guidelines](guest-agent-guidelines.md#ownership) name the calls
  that start one.
