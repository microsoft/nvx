# Runtime adapter

The runtime adapter is an independent adapter of the NVX runtime, with a host
program and a guest agent. It lives in this repository so that NVX's own CI
builds and runs it, and it depends on [`nvx`](../../nvx-rs/README.md) the way
an adapter outside the repository does:

- as a path dependency with default features off;
- with a `Cargo.lock` of its own, outside any other workspace;
- through the public API alone, with unsafe code forbidden;
- with the minimum supported Rust version of `nvx`.

The adapter brings its own application framing to the control session between
the host and the guest, in place of NVX's `NVXC` frames. Its host program sends
requests in that framing, and its guest agent serves them. The host program
also runs the guest agent in VMs of NVX's artifacts, in place of NVX's agents,
through NVX's supervisor, and finds a VM again after it restarts, as an
orchestrator does.

A VM may have a layered root: an image under an upper, which the host program
stages and plans with NVX's typed plan for an adapter's own guest, whose staged
bindings a launch checks before it hands OpenVMM the checked files, and which
the guest agent's worker assembles through `nvx::guest::root`, in a process of
its own rather than PID 1, as the host program's root configuration asks. The
plan also holds the VM's [network](#networks), if any. The host program may
share host directories and files with the root, which it exports with NVX's
`filesystem::plan`, and which the worker maps beneath the root from the host's
mapping table, before the host starts the sandbox. The host program then checks
the root from inside the VM with the worker's probes, and runs executions in
it, several at once over one session, in a
[table of scenarios](#layered-scenarios), to which each new check adds a row.

## Configurations

Each configuration enables one side of `nvx`:

| Configuration | Cargo options | `nvx` features | Modules | Binary |
| --- | --- | --- | --- | --- |
| Protocol-only | none | none | `frame`, `root`, `probe`, `exec`, `template` | none |
| Host-only | `--features host` | `host` | `frame`, `root`, `probe`, `exec`, `template`, `host`, `runtime`, `initramfs`, `staged`, `sharing`, `layered`, `capture`, `generations`, `hook` | `runtime-adapter-host` |
| Guest-only | `--features guest --target x86_64-unknown-linux-musl` | `guest` | `frame`, `root`, `probe`, `exec`, `template`, `guest`, `worker` | `runtime-adapter-guest`, a static executable |

## Framing

The `frame` module is the adapter's application framing, which its host program
and guest agent exchange over the control session in place of NVX's `NVXC`
frames. A message is a 5-byte header, then a body. The header holds the body's
length, a little-endian `u32`, then the message's kind:

| Kind | Message | Body |
| --- | --- | --- |
| 1 | `Ping`: asks the guest to answer with `Pong` | none |
| 2 | `Pong`: the guest's answer to `Ping` | none |
| 3 | `Echo`: bytes that the guest sends back in an `Echo` of its own | at most 256 KiB, what four of the session's largest records carry |
| 4 | `Stop`: asks the guest to stop serving | none |
| 5 | `Stopped`: the guest's confirmation of `Stop`, its last message | none |
| 6 | `RootConfig`: the host's [root configuration](#root-configurations) of a layered root, which the guest answers with `Ready` or `RootFailed` | the configuration, at most 4 KiB |
| 7 | `Ready`: the guest assembled the root, or activated a clone | the guest agent's [build](#ready-mapping-tables-and-start), and a clone's activation, at most 4 KiB |
| 8 | `RootFailed`: a stage of the root failed: the guest's answer to `RootConfig`, or its report of a failed teardown before it confirms a `Stop` | the stage, then the problem, at most 4 KiB |
| 9 | `Probe`: asks the guest to run a [probe](#probes) on its root | the probe, at most 4 KiB |
| 10 | `Report`: the guest's answer to `Probe`: the facts that the probe found | at most 256 KiB |
| 11 | `ProbeFailed`: the guest's answer to a `Probe` that it does not know, or that failed | the problem, at most 4 KiB |
| 12 to 21 | The messages of an [execution](#executions) | at most 256 KiB for a start, input, or output, and 64 bytes for the others |
| 22 | `Maps`: a part of the host's [mapping table](#ready-mapping-tables-and-start), which the guest applies beneath the root, in order, and answers with `Mapped` or `MapFailed` | a `MAPS` payload of `nvx::protocol`, at most 256 KiB |
| 23 | `Mapped`: the guest applied the part's mappings | none |
| 24 | `MapFailed`: the guest refused the part, or failed to apply one of its mappings | how many mappings it applied, then the problem, at most 4 KiB |
| 25 | `Start`: asks the guest to start the sandbox's workloads once it applied the mapping table, which it confirms with `Started` | none |
| 26 | `Started`: the guest's confirmation of `Start` | none |
| 27 | `Activate`: the host's activation of a clone, which a restore of a [template's](#template-mode) snapshot started, and which the guest answers with `Ready` | a clone's activation record, as `nvx::activation::ActivationRecord::encode` writes it, at most 1 KiB |

The session keeps no message boundaries, so a message may span several of its
records, and one record may end a message and start the next. `Decoder`
reassembles the messages. It checks each header before it takes any byte of
the body, and holds at most one partial message. A malformed message ends the
stream, because the framing cannot find the next message after it. A clone's
`Activate` whose header announces more than 1 KiB fails the clone's
activation instead, as a record that long does.

The framing carries each body as bytes, within its kind's limit. The bodies of
kinds 6 to 22 and 24 have encodings of their own, which the `root`, `probe`,
and `exec` modules write and read, and that of kind 27 is `nvx::activation`'s.
A body that does not decode leaves the stream intact, at the start of the next
message. The guest answers a root configuration that does not decode with
`RootFailed`, as a configuration that it refuses, and a probe with
`ProbeFailed`, while a message about an execution that does not decode ends the
worker's session, as one that breaks the execution's rules does, and an
activation record that does not decode fails the clone's activation. The host
program's client ends on an answer that does not decode, since it cannot tell
what the guest did.

### Root configurations

The boot tokens of `nvx::protocol::root` carry what NVX's boot contract
defines: the image's format, the identity that the guest expects of it, and
the upper. A root configuration, `root::RootConfig`, carries the rest that the
root needs, which only the adapter defines, once the host attaches. It is
UTF-8 text: one `NAME=VALUE` line per field, each ended by a newline, in any
order, and each at most once. Numbers are decimal, without a sign or leading
zeros:

| Name | Value | Required |
| --- | --- | --- |
| `version` | 1 | yes |
| `runtime` | The runtime directory, below which the guest assembles the root | yes |
| `agent-cgroup` | The agent's cgroup, which holds the guest's own processes | yes |
| `workload-cgroup` | The workload cgroup, the agent's sibling | yes |
| `device-timeout-ms` | How long the guest waits for each block device, at most 600,000 | yes |
| `agent-memory-low` | The agent's `memory.low`, in bytes | no |
| `workload-memory-max` | The workload cgroup's `memory.max`, in bytes | no |
| `workload-pids-max` | The workload cgroup's `pids.max` | no |
| `distro-sectors` | The size of the image's block device, in 512-byte sectors | no |
| `scratch-sectors` | The size of the scratch block device, in 512-byte sectors | no |
| `first-partition-bound` | The most bytes that the first partition of a GPT disk may hold | no |
| `overlay-mode` | The overlay's mode: `trusted-lowers` or `untrusted-lowers` | no |
| `scratch-journal` | Whether an ext4 scratch may have a journal: `allowed` or `forbidden` | no |
| `mapping-count` | The number of mappings of the host's [mapping table](#ready-mapping-tables-and-start), at most 4,096 | no |
| `workload-uid` | The user ID of the sandbox's workload identity, as which every execution runs | with `workload-gid` |
| `workload-gid` | The group ID of the sandbox's workload identity | with `workload-uid` |
| `listeners` | Whether the guest's listeners are published, with which every execution enters the root: `published` or `unpublished` | no |

The paths are absolute and canonical guest paths of at most 4,095 bytes,
without a NUL or a newline: not `/`, and without an empty, `.`, or `..`
component, or a trailing `/`. The runtime directory holds no `,`, `:`, `\`, or
`=`, which the overlay's options reserve, and the two cgroups are siblings,
whose parent is not `/`. The optional numbers are positive. A workload
identity has both IDs, each below 4,294,967,295, which the kernel reads as
"unchanged", and no supplementary groups or capabilities, as a layered
sandbox's identity does; the worker checks its account once it assembled the
root, and every execution then runs as it, naming no identity of its own.
Without an overlay mode or a journal policy, the guest takes the defaults of
`nvx::guest::root`: an overlay that trusts its lowers, and a scratch that may
have a journal. Without a bound of the first partition, the guest bounds it at
`root::DEFAULT_FIRST_PARTITION_BOUND`, 64 KiB. Without a mapping count, the
host's mapping table has no mapping. Without `listeners`, the guest's listeners
are published, as `nvx::guest::entry`'s default has them, which refuses the
most to the executions. The host program and the guest agent ship together, so
the guest refuses every other configuration, one with an unknown field
included, rather than ignore what it does not know.
`RootConfig::default()` holds the values that the host program sends unless its
caller names others: the runtime directory `/run/runtime-adapter`, the cgroups
`/sys/fs/cgroup/agent` and `/sys/fs/cgroup/workload`, a wait of 30 seconds, an
agent's `memory.low` of 16 MiB, as NVX's own guest reserves, and a workload's
`memory.max` of 128 MiB and `pids.max` of 128, with neither a bound of the
first partition, an overlay mode, a journal policy, a workload identity, nor a
publication of listeners, and no mapping. A layered launch names the
publication of its network; see [Networks](#networks).

A stage of the root that fails is a `root::RootFailure`: the stage's code, one
byte, then the problem, as UTF-8 text. The guest's worker, and with it the VM,
then ends with the stage's status:

| Code | Stage | What failed | Status |
| --- | --- | --- | --- |
| 1 | `configuration` | Receiving and checking the root configuration | 11 |
| 2 | `layout` | Reading the boot layout of the root and the host export from the kernel command line, and checking that the worker assembles them, with an export if the configuration announces mappings | 12 |
| 3 | `bootstrap` | Bootstrapping the guest for the root: its cgroups and their resource values | 13 |
| 4 | `assembly` | Assembling the root, such as from an image of another identity than the boot tokens expect, and mounting the host export that the kernel command line names | 14 |
| 5 | `teardown` | Tearing the root down, such as while a process holds it open | 15 |
| 6 | `identity` | Checking the account of the sandbox's workload identity in the assembled root, in existing-account mode, such as one that the image lacks | 16 |

### Ready, mapping tables, and Start

`Ready`, the guest's answer to a root configuration once it assembled the
root, carries its readiness, `root::Ready`: the length of the guest agent's
build, one byte, then the build, 1 to 255 printable ASCII characters without a
space. The guest agent's build is the crate's version, `GUEST_BUILD`, which is
also the build that the host program ships unless it names another; see
[The runtime identity](#the-runtime-identity). The host sends a guest agent of
another build nothing more; see [The host's order](#the-hosts-order).

A clone's `Ready`, its answer to the host's `Activate`, also reports its
activation after the build, as `nvx::activation::ReadyActivation::encode`
writes it: 1, then the transaction ID of the record's restore, 16 bytes.
`root::Ready::with_activation` adds it, and `root::Ready::activation` returns
it. A guest that boots cold reports none, and `root::Ready::decode` refuses
bytes after the build that are not an activation.

The host's mapping table, of as many mappings as the root configuration
announces, travels in parts, each in a `Maps`, whose body is a `MAPS` payload
of `nvx::protocol`, as its `encode_maps_payloads` writes it: the index in the
table of the part's first mapping, then the number of its mappings, each a
little-endian `u32`, then each mapping: its flags, a little-endian `u16` that
is 1 for a read-only mapping and 0 otherwise, then the lengths of its source
and its target, each a little-endian `u16`, then the source, a path relative
to the guest's mount of the host's export, or empty for the export's root
itself, and the target, an absolute path in the root. `root::encode_maps`
splits a table into parts of at most 256 KiB, and an empty table into none.
`root::decode_maps` reads a part with
`nvx::protocol::decode_guest_maps_payload`, which takes the paths as bytes, as
Linux does, and refuses what that decoder refuses, a part without a mapping,
and a part that would end the table past 4,096 mappings.

`Mapped` answers a part whose mappings the guest applied, and `MapFailed` one
that it refused, or one of whose mappings it failed to apply. The body of
`MapFailed`, `root::MapFailure`, holds how many of the table's mappings the
guest applied, in order, a little-endian `u32`, which is the index in the
table of the mapping that failed, then the problem, as UTF-8 text. `Start`
asks the guest to start the sandbox's workloads once it applied the table,
and `Started` confirms it. `Mapped`, `Start`, and `Started` have no body. The
guest's worker takes the table and the start in the order that
[The mapping table and the start](#the-mapping-table-and-the-start)
describes.

### Probes

A probe, `probe::Probe`, is a named check that the host asks the guest's
worker to run on its layered root. It is UTF-8 text: the probe's name, which
starts with a lowercase ASCII letter, and holds lowercase ASCII letters,
digits, and `-`, then each of its arguments after one space, each without
whitespace or control characters. The worker answers with the facts that the
probe found, `probe::Facts`, also UTF-8 text: one `KEY=VALUE` line per fact,
each ended by a newline, in the order in which the probe found them, and each
key at most once. A key holds lowercase ASCII letters, digits, `.`, and `-`,
and a value escapes `\`, newlines, and carriage returns as `\\`, `\n`, and
`\r`.

The worker runs the probes of `worker::probes` while it runs no
[execution](#the-workers-executions). FILE and DIRECTORY are relative paths in
the root, of normal components, and the other probes take no arguments:

| Probe | Facts |
| --- | --- |
| `mounts` | For the mount of each layer, by the name of its mount point, `distro` for an EROFS image, and `pN` for the layer in partition N of a GPT disk, the scratch's, `scratch`, the tmpfs's, `tmpfs`, and the overlay's, `root`: the number of mounts at its mount point, `NAME.count`, and of the topmost, its `NAME.type`, `NAME.source`, `NAME.options`, and `NAME.super`, the file system's own options |
| `contents` | The root's `os-release.id` and `root.mode`. It writes `.runtime-adapter-probe` at the top of the root, and reports what the root, `write.root`, the upper, `write.upper`, and the top layer, `write.lower`, hold at that name, and whether the top layer refuses a new file, `lower.write` |
| `copy-up FILE` | It sets the mode of FILE, a regular file of the layers, to the mode that it has, which copies the file up, and reports its size in the topmost layer that holds it, `lower.size`, and in the upper, `upper.size`; whether the upper's copy holds the file's data, `upper.data`: `copied`, `none` once the overlay copied up the file's metadata alone, or `different`; and whether the root still shows the layer's data, `root.data`: `intact` or `different` |
| `rename DIRECTORY` | It renames DIRECTORY, a directory of the layers, to `.runtime-adapter-renamed` at the top of the root, and back if it can: `outcome` is `allowed`, `refused` once the overlay refused it with `EXDEV`, or `failed: PROBLEM` |
| `fill` | It writes a file in the root until the upper is full, then removes it, and writes a file there again: the upper's size, `total`, and the bytes available in it before, `available`; the bytes that it wrote, `written`; the error that ended the writes, `error`: `ENOSPC` once the upper was full, or the problem; what the second file holds, `after`; and the guest's memory, `memory`, in bytes |
| `devices` | The guest's disks, the block devices that a device backs: `disks`, each as `NAME:SECTORS:ACCESS`, its size in 512-byte sectors and its access mode, `ro` or `rw`, in the order of their names, separated by spaces |
| `reads` | The sectors that the guest read from each of its disks, by the disk's statistics: `disks`, each as `NAME:SECTORS`, in 512-byte sectors, in the order of their names, separated by spaces |
| `read-disks` | It reads each of the guest's disks from its start to its end, through the page cache: `disks`, each as `NAME:BYTES`, the bytes that it read, in the order of their names, separated by spaces |
| `memory` | The guest's memory: the size of its memory blocks, `block-size`, how many are online, `blocks`, and their bytes, `online`, and the memory that the kernel manages, `total`, each in bytes |
| `processors` | The guest's CPUs, as the kernel lists them: those that are online, `online`, present, `present`, and possible, `possible`; and the CPUs that the restore of a clone brought online, as the time ABI's record of the restore names them, `restored`, `none` for none, or `absent` without a record |
| `time` | The guest's time state, `/run/nvx/time/state`, which NVX's time daemon keeps: each line `KEY=VALUE` as a fact, with each `_` of its key as `-`, in order, leaving out a key that no fact may have, and a repeated one |
| `first-partition` | The bytes of the first partition of a GPT disk that the assembly returned, without their trailing padding: how many, `length`, and the bytes, `bytes`, in hexadecimal, or `none` for both for an EROFS image |
| `cgroups` | The file system at the cgroups' parent, `parent.type`, and its `parent.subtree-control`; whether each cgroup exists, `agent.exists` and `workload.exists`; `agent.memory.low`, `workload.memory.max`, and `workload.pids.max`; the number of processes in each, `agent.processes` and `workload.processes`; and `user-namespaces`, the guest's limit of user namespaces, or `unavailable` without them |
| `processes` | For PID 1, `pid1`, the worker, `worker`, and PID 1's unrelated child, `sibling`: `NAME.pid`, and while the process exists, `NAME.state`, `NAME.ppid`, `NAME.root`, `NAME.cgroup`, `NAME.command`, and `NAME.namespaces`: `same` if it shares PID 1's, or the ones that differ |
| `hold` | It opens the root, and keeps it open until the worker ends, so that the teardown finds the root busy: `held`, the path that it holds |

A value that a probe cannot read is `absent` if its file does not exist, and
otherwise `unreadable: PROBLEM`. A new check of a root is a new probe in this
table.

The worker of a clone also answers the probe `activation`, which takes no
arguments, itself, at any time once the clone is ready, executions running or
not, with when the events of the clone's activation and order happened, in
microseconds after the repair's acknowledgement of the restore: whether the
repair acknowledged the restore, `acknowledged`, `yes`, or `none` for a restore
whose host did not gate its input until the acknowledgement; and when the clone
took the reset that the restore queued, `reset`, when the host's first message
came, `message`, which must be the clone's `Activate`, when the clone was
ready, `ready`, when its sandbox started, `started`, and when the start of the
host's first execution came, `execution`. Each event is `none` until it
happened, `before` if it happened before the acknowledgement, and `unknown`
without one. The worker of a VM that boots cold runs no such probe.

The probes of `worker::mapping` inspect the mappings beneath the root, and map
from the host export, as an adapter maps the host paths of its sandboxes. `map`
inspects the mappings that the worker applied from the host's mapping table,
through the root itself, with `nvx::guest::root::Root::map`, which checks their
targets against the container root's reserved set, and the export that they
come from. `map-checks` maps from the export through mappers of the adapter's
own, `nvx::guest::mappings::Mapper`, with reserved sets of its choice, and
tears down what it mapped. The links that it plants in the root lead to
`/tmp/runtime-adapter-outside`, an empty directory outside it.

| Probe | Facts |
| --- | --- |
| `map [NAME=PATH...]` | The export's mount at `hostfs/root` in the runtime directory: the number of mounts there, `export.count`, and of the topmost, its `export.type` and `export.options`; the mode of `hostfs`, in octal, `hostfs.mode`, and its owner, `hostfs.owner`, as `UID:GID`, or `absent` for both; and the mount points below the root, in the mount table's order, `mounts`. For each NAME, at PATH in the root, such as a mapping's target: the options of the topmost mount there, `NAME.options`, or `none`; what it shows, `NAME.kind`; a directory's entries, by name, in order, separated by commas, `NAME.entries`, or a file's contents, `NAME.read`; whether a write is `allowed` or `refused`, `NAME.write`: of a new file `written`, which holds `runtime-adapter`, in a directory, or of the file, opened for writing; and what the overlay's upper holds at PATH, `NAME.upper`, an empty directory or file where the worker created the target. Finally whether the root is latched, `latched` |
| `map-checks DIR FILE` | From the export's directory DIR, which holds a file `marker` and a directory `inner`, and in which the guest may write, and its file FILE, each check through a new mapper: a link at each component of a target, `link.a`, `link.b`, and `link.new`, and to `..`, `link.up`; a magic link of a procfs that it mounts in the root for the checks, `magic.self`, `magic.thread-self`, `magic.root`, `magic.cwd`, and `magic.fd`; a directory onto a file, `kind.directory`, and a file onto a directory, `kind.file`; a mapping onto a file that an earlier one shows, `nested.onto`, one that would create its target inside it, `nested.create`, and their teardown, `nested.teardown`; the runtime's reserved targets, `reserved.run`, `reserved.run-nvx`, `reserved.time`, and `reserved.time-below`, the container root's, `reserved.proc`, `reserved.sys`, `reserved.dev`, `reserved.agent`, `reserved.machine-id`, and `reserved.etc`, and those of a set of the adapter's, with the tree `/srv/adapter` and the path `/srv/checks/exact`, `reserved.adapter-tree`, `reserved.adapter-ancestor`, `reserved.adapter-path`, and `reserved.adapter-below`, and whether a refused one created anything, `reserved.created`; a read-only mapping of DIR attached and unmounted 200 times while another thread tries to open its `marker` for writing, `read-only.attaches`, `read-only.flags`, and `read-only.writable`; and the entries and mounts of the directory outside the root, `outside.entries` and `outside.mounts`, and the mounts that the checks left, `left` |

A NAME holds lowercase ASCII letters, digits, and `-`, but is neither `export`
nor `hostfs`, and comes once. PATH is a relative path in the root, and DIR and
FILE are relative paths in the export, such as `2`, its third child, each of
normal components. A kind is `directory`, `file SIZE`, with the file's size in
bytes, `link`, `other`, `denied` for a path that the guest may not look up,
such as one that the export hides, or `absent`. A check's outcome is
`applied`, or why the mapper refused the mapping: `link` for a link at the
component that the check expects, and otherwise the kind of the error, such as
`reserved` or `target-kind`, with the path that it names, if any. The checks
write nothing to the export.

### Executions

An execution is a program that the guest's worker runs for the host, whose
standard streams and outcome NVX's execution engine, `nvx::exec`, carries. The
host's side of an execution is the engine's `Peer`, and the worker's its
`Runner`, which the worker binds to the program with
`nvx::guest::exec::binding`. Each message of the engine travels in a frame of
its own kind, which the `exec` module encodes and decodes: `exec::HostMessage`
from the host, and `exec::GuestMessage` from the guest. Each body starts with
the execution's key: its ID, then its generation, each a little-endian `u64`.

| Kind | From | Message | Body after the key |
| --- | --- | --- | --- |
| 12 | host | `ExecStart`: starts the execution | the rest of its start |
| 13 | host | `ExecInput`: bytes of the program's standard input | the bytes, at most a chunk |
| 14 | host | `ExecInputEnd`: the end of the input, after its last byte | none |
| 15 | host | `ExecCancel`: ends the execution | the cause |
| 16 | both | `ExecCredit`: credit of an output pool, from the host, or of the input, from the guest | the pool, then the units, a `u32` |
| 17 | guest | `ExecStarted`: the program runs | none |
| 18 | guest | `ExecOutput`: bytes of the program's standard output or error | the stream, then the bytes, at most a chunk |
| 19 | guest | `ExecEnd`: the end of a stream, after its last byte | the stream |
| 20 | guest | `ExecInputClosed`: the program's input closed before the host ended it | none |
| 21 | guest | `ExecTerminal`: the execution's outcome, the guest's last message about it | the outcome |

A start and the messages of kinds 13 and 18 hold at most 256 KiB, and the
others at most 64 bytes. A pool is 0 for the input, 1 for standard output, 2
for standard error, and 3 for the output pool that both streams share; a stream
is 1 for standard output and 2 for standard error; and a cause is 1 for a
cancellation, 2 for a drop, 3 for a timeout, 4 for a reset, and 5 for a
disconnection, of which the host's cancellation names one of the first three.

A start, `exec::Start`, holds the limits on which both sides agree: the chunk,
a `u32` of at most `exec::MAX_CHUNK` bytes, what the longest body holds after
the key and a stream; the input's window, then the output's, each a unit, 0
for bytes or 1 for frames, then its size, a `u32`; and whether the streams
share one output pool, 0 or 1. It then holds the worker's timers, in
milliseconds, each a `u64`: the grace, the settlement time, and whether a
timeout follows, 0 or 1, then the timeout, or 0. Then come whether the host
sends the program's standard input, 0 or 1, and the program's arguments: their
count, a `u16`, then each argument's length, a `u32`, and its bytes. The first
argument is the program's path, which is not searched for in `PATH`, and no
argument holds a NUL byte. The start ends with how the program enters the
worker's root:

- whether it names the program's identity, `exec::Identity`, 0 or 1, and if
  it does, the user ID and the group ID, each a `u32`, the account mode,
  `exec::AccountMode`, 0 for an existing account or 1 for numeric IDs, the
  capabilities, a `u64` whose bit N stands for the capability numbered N, then
  the supplementary groups: their count, a `u16`, then each group, a `u32`.
  Exactly one source names an execution's identity: its start, or the
  sandbox's workload identity, which the root configuration names;
- whether it names the program's working directory, 0 or 1, and if it does,
  the directory's length, a `u32`, and its bytes: an absolute path in the
  root, without a NUL byte. Otherwise the program starts in the root's `/`;
- the program's whole environment: the count of its entries, a `u16`, then
  each entry's length, a `u32`, and its bytes, `NAME=VALUE`, with a name that
  is not empty, and without a NUL byte.

An outcome holds the program's status: 0 for none, 1 for an exit, or 2 for a
signal, then the exit's code or the signal's number, an `i32`, or 0; the cause,
or 0 for none; whether the launch failed, 0 or 1, then its stage, a `u32`, and
its `errno`, an `i32`, or 0 for each; and flags: 1 if standard output lost
bytes, 2 if standard error did, and 4 if the execution is reusable. Stage 0 is
the worker's refusal of the start, and a later stage is the binding's number of
the stage of `nvx::guest::spawn` that failed, such as 21 for its `exec`.

`exec::HostExecution` is the host's side of one execution, in every
configuration: the engine's `Peer`, with a sink that takes the program's output
as it arrives, unless its caller stalls it, so that the guest gets no credit
for more, and a source that hands over the program's input, if the start says
that the host sends it, then ends it. Its caller sends the start, hands it each
of the guest's messages about the execution with `deliver`, and sends the
messages that `serve` returns, until the peer finished, serving it again once
its `deadline`, the peer's next timer, passes. The sink keeps at most
`exec::MAX_CAPTURE`, 16 MiB, of each stream: once more of a stream arrives, the
host's side cancels the execution, and its peer discards the rest once the grace
ends, so that the outcome marks the stream truncated, with the cause
`cancelled`. The host program's [driver](#host-program) and the tests of the
worker run it.

### Root-profile entries

The probe of `worker::entries`, which takes no arguments either, enters the
root as an adapter's root profiles do, through `nvx::guest::entry` and
`nvx::guest::spawn`: each execution in new mount, PID, and UTS namespaces,
with the root moved over `/`, a view of its own, and an identity with only the
capabilities that the entry allows. The worker copies its own executable to
`/.runtime-adapter-guest` at the top of the root, and runs the executions in a
helper, `runtime-adapter-guest entries ROOT CGROUP`, which enters a mount
namespace of its own first, as NVX's managed agent does, so that the runtime's
binds of a generated machine ID and of the time status end with it, before the
worker tears the root down. The helper places each execution in the workload
cgroup, leaks it a descriptor of the guest's own `/` without close-on-exec,
and has it run `runtime-adapter-guest inside DIRECTORY DESCRIPTOR` from the
root, which reports what the execution sees and may do, in its case's
directory below `/tmp/runtime-adapter-entries`. The helper blocks `SIGCHLD`,
as a caller that collects its children through a signal descriptor does,
which no execution inherits.

| Probe | Facts |
| --- | --- |
| `entries` | The capabilities outside the allowlist that the entry refused, `refused.outside`, and any that it accepted, `refused.accepted`; its refusal of `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` while listeners are published, `refused.published.net-bind-service` and `refused.published.net-raw`, of a capability for a user ID other than 0, `refused.non-root`, and of an identity without an account in existing-account mode, `refused.existing`. For each case, `CASE.launch` and `CASE.status`, and what the execution reported, as `CASE.KEY`: the descriptors open in it, `fds`, and whether the leaked one is, `leaked`; `uid`, `gid`, and `groups`; the capability sets, `cap.inh`, `cap.prm`, `cap.eff`, `cap.bnd`, and `cap.amb`, and `no-new-privs`; the signals that it blocks, `signals.blocked`; `pid`; its working directory, `cwd`; `hostname`; the `machine-id` and a mount's options at `/etc/machine-id`, `mount.machine-id`; the first line of the time status's state, `time.state`, the names in `/run/nvx/time`, `time.files`, and a mount's options there, `mount.time`; the unprivileged port start, `threshold`; the outcome of lowering it, `threshold.write`, of writing the hostname through `/proc/sys`, `hostname.write`, of opening `/proc/sysrq-trigger` for writing, `sysrq.write`, of binding UDP port 1023, a privileged source port, from which the gateway accepts listener announcements, `bind`, of opening a raw socket, which could forge such a datagram, `raw`, of changing a new file's owner, `chown`, of reading a new file of mode 0, `dac-override`, of changing the mode of a file of user ID 1000, `fowner`, and of running a program as user ID 4321, `setuid`, and as group ID 4321, `setgid`; the root's entries, `root`; and, last, a nested `chroot` escape, `escape`: `contained` if climbing with `..` from outside a `chroot` ends in the execution's own root. For the working-directory descriptors, only their distinct outcomes, sorted and separated by commas, `cwd-descriptors.launch` and `cwd-descriptors.status`. The generated machine ID, `generated.source`. Once the helper ended, the root's own machine ID, `after.machine-id`, and the mounts below the root, `after.mounts`, in the worker's mount namespace |

The cases, as user ID 0 in numeric-account mode unless they say otherwise, are
each allowed capability alone, without published listeners, `only-NAME`, such
as `only-net-bind-service`; no capability, `none`; every allowed capability
but those that published listeners refuse, with published listeners,
`published`, which stands for both publication modes, private-network ingress
and the host's loopback, since the entry treats them alike; every allowed
capability, without published listeners, `unpublished`; user and group ID
4242, of which the image has no account, `numeric`; with every allowed
capability, without published listeners, the working-directory cases, each of
which names a working directory: the case's own, `cwd-directory`, and, through
a magic link, which the entry refuses before the program runs, the leaked
descriptor's, `cwd-leaked`, the execution's own working directory and root,
`cwd-self` and `cwd-root`, that of the first process of its namespace,
`cwd-init`, a symbolic link to its working directory, `cwd-link`, and the link
of each descriptor from 0 to 63, `cwd-descriptors`; and, once the helper has
bound the runtime's files, a generated machine ID, the time status that the
guest's time daemon publishes at `/run/nvx/time`, and the hostname
`runtime-adapter`, `generated`. The helper first writes a machine ID
in the root, as its own, which the cases before `generated` see. An attempt's
outcome is `allowed`, `refused ERRNO`, such as `refused EPERM`, or `failed
PROBLEM`.

## Guest agent

`guest::serve` serves the host program on a stream to the guest's end of the
control session, through `nvx::guest::nvxs`. It attaches, then:

- acknowledges each reset of the session with the smallest receive window that
  the session allows, once it dropped the earlier epoch's partial message,
  because a new epoch starts with a new message;
- credits each payload back once it holds the payload, so that a message
  longer than the window still arrives whole;
- answers each `Ping` with `Pong`, sends each `Echo` back, answers each
  `Probe` with `ProbeFailed`, since the VM has no layered root to probe, and
  answers `Stop` with `Stopped`, then returns.

A failure of the session, a malformed message, a message about a layered root,
such as its configuration, or about an execution, which the guest agent of a
VM without a layered root does not accept, and a message that only the guest
sends end serving with an error.

`runtime-adapter-guest serve` runs the guest agent on its standard input and
output, which connect to the guest's end of the session, such as a control
console in raw mode. It exits with 0 once the host stopped it or the broker
closed the stream, and with 1, after a diagnostic on standard error, once
serving failed. Without arguments, it reports the contract that it was built
against.

`guest::serve_terminal` serves a tty, such as a VM's control tty, as `serve`
does. It opens the tty with `nvx::guest::terminal`, as NVX's managed agent
does: it waits up to 30 seconds while the kernel sets the tty up, and puts it
in raw mode, so that the tty passes the session's bytes as they are. Once
serving ends, it waits until the tty has sent what serving wrote, such as the
confirmation of a stop.

`runtime-adapter-guest init TTY` runs `serve_terminal` on the tty TTY in place
of NVX's managed agent, as a VM's PID 1; see [VMs](#vms). It ends with 0 once
the host stopped it, and with 1, after a diagnostic on standard error, once
serving failed or the broker closed the tty. As PID 1, which must not exit, it
ends the VM with that status instead, through NVX's `/sbin/nvx-exit`, as the
managed agent does.

### PID 1 and the worker of a layered root

`runtime-adapter-guest init TTY layered` runs PID 1 of a VM with a layered
root, in place of NVX's init agent; see [VMs](#vms). PID 1,
`worker::supervise`, stays minimal:

1. It starts an unrelated child, `runtime-adapter-guest idle`, which sleeps
   until a signal ends it, and stands for a daemon that a guest starts before
   its agent.
2. It starts the worker, `runtime-adapter-guest worker TTY SIBLING`, with the
   process ID of the unrelated child, which the worker's probes check, since
   the guest's kernel does not list a process's children.
3. It collects the status of every child, adopted processes included, until
   the worker ends, and then ends the VM with the worker's status, through
   `/sbin/nvx-exit`. A wait also reports to PID 1, whatever its options, each
   stop of a child's thread that asked PID 1 to trace it, and PID 1 goes on
   past it. A real-time signal, which `nix` decodes in no report, may end the
   worker or a process that PID 1 adopted, such as one that an execution left,
   or stop a thread that PID 1 traces, so PID 1 waits until a child has
   something to report without taking it, then takes the worker's report
   through `nvx::guest::process::Child`, which keeps every signal, and
   another's by the ID that the wait gives if the report decodes. Otherwise,
   PID 1 finds the threads of its children that `/proc` lists as zombies, or
   as stopped while traced, whatever the names of their programs. No wait for
   any child takes the worker's status.

The worker, `worker::work`, is a process of its own, never PID 1. Through
`worker::work_on_terminal`, it opens the tty with `nvx::guest::terminal`, as
`serve_terminal` does, then opens the tty again for itself, nonblocking, and
then:

1. attaches the session;
2. receives the host's root configuration, and checks it;
3. reads the boot layout of the root from the kernel command line, with
   `nvx::protocol::root::RootLayout::parse`, and the host export, if the
   command line names one, with `nvx::protocol::export::HostExport::parse`,
   then bootstraps the guest's cgroups with `nvx::guest::root::bootstrap`,
   which moves the guest's processes into the agent's cgroup, assembles the
   root with `nvx::guest::root::assemble`, and mounts the export out of the
   root with `Root::mount_export`. If the configuration names a workload
   identity, it then checks the identity's account in the assembled root, in
   existing-account mode, with `nvx::guest::entry`;
4. answers `Ready`, with the guest agent's build, or `RootFailed` with the
   stage that failed, and then ends;
5. applies the host's [mapping table](#the-mapping-table-and-the-start)
   beneath the root, from the export, in the parts that the host sends, and
   answers each part with `Mapped` or `MapFailed`;
6. on `Start`, once it applied every mapping that the configuration
   announced, starts the sandbox, which it confirms with `Started`;
7. serves `Ping`, `Echo`, and `Probe`, though it refuses a probe while it runs
   executions, and, once the sandbox started, runs the host's
   [executions](#the-workers-executions);
8. on `Stop`, drops its executions, which ends their programs, tears the root
   down, reports a teardown that failed with `RootFailed`, confirms the stop
   with `Stopped`, and ends.

Before the root configuration, the worker accepts only the configuration, or a
`Stop`, which ends it without a root. It assembles an EROFS image, or the
layers of a GPT disk, whose first partition holds at most the bound of the
configuration, over an ext4 scratch or a tmpfs in RAM, in the overlay mode of
the configuration. At the layout stage, it refuses a configuration that
configures a scratch for an upper in RAM, bounds a first partition for an EROFS
image, or announces mappings while the kernel command line names no host export,
from which they come, and a command line whose tokens of the export break their
rules, as `HostExport::parse` checks them. An export that fails to mount fails
the assembly. A workload identity whose account the assembled root lacks fails
the identity stage. The worker ends with 0 once the host stopped it, after a
teardown that succeeded, with the stage's status, from 11 to 16, once a stage
failed, and with 1 once its session failed or the broker closed the tty, each
but the first after a diagnostic on standard error. Once the broker closed the
stream at a record boundary, which may close only the broker's end, the worker
drops its executions, and writes what it queued, as once the host stopped it.
Before it exits, it closes what its probes hold and the host export's root, then
unmounts a root that it still holds, as `nvx::guest::root::Root` does when it
drops, ignoring failures. A teardown that failed leaves the root mounted until
the VM ends. The worker runs the root's operations through
`worker::RootOperations`, and enters its executions through
`worker::EntryOperations`, so that tests run it against a fake root. In a
template's VM, the worker captures the VM instead, and then activates and serves
each clone that a restore of the snapshot starts, as
[Template mode](#template-mode) describes.

#### The mapping table and the start

The root configuration announces how many mappings the host's mapping table
holds, and the host sends the table once the root is ready, in parts, each in
a `Maps`, in order. The worker applies each part's mappings beneath the root,
in order, from the host export, with `nvx::guest::root::Root::map`, and
answers the part with `Mapped`, or with `MapFailed` and how many of the
table's mappings it applied:

- It refuses a part as a whole, at the mapping that it expects next, if the
  part does not decode, or does not continue the table: if its first mapping
  is not the one that the worker expects next, or its last is past the count
  that the configuration announced.
- A mapping that the root refuses or fails to apply latches the root, which
  keeps the mappings before it, so that the count is that mapping's index in
  the table.
- Once it answered `MapFailed`, the worker refuses every later part, with
  `MapFailed`, and the start: it starts no workload on a partial table, and
  the host tears the VM down.

A table of zero mappings does no work: the host sends no part, and the worker
maps nothing, though it mounts the export if the command line names one.

`Start` starts the sandbox once the worker applied every mapping that the
configuration announced, and none failed, which the worker confirms with
`Started`. Only then does it run the host's executions, and it takes no part
of the table any more. A message about an execution before the start, a
`Start` before every announced mapping is applied, once the table failed, or
once the sandbox started, and a `Maps` once the sandbox started end the
worker's session with an error, as a message that it does not take in its
state does. The worker serves pings, echoes, probes, and the stop before the
start and after it. Its order outlives a reset of the session, as its root
does.

#### The worker's executions

Once the host started the sandbox, the worker runs several executions at once,
each through NVX's execution engine and its guest binding, with the limits and
timers of its start, such as frame-counted credit and one output pool that
standard output and error share, which the adapter's tests use. One loop that
never blocks serves the session and every execution: it takes the host's
messages as they arrive, steps every execution, queues their messages for the
host, and then polls the session's stream and every execution's descriptors
until one is ready or an execution's deadline passes. So a program's blocked
stream, an exhausted window, a stalled sink of the host, or a full stream to
the host holds back only the bytes that wait on it, never a timer, a
cancellation, a reset, a stop, or another execution. Nor does a probe, which
the worker runs only while it runs no execution, and otherwise answers with
`ProbeFailed` and `worker::EXECUTING`. While more than a MiB of its records
waits for the stream, the executions take no more output from their programs,
and go on otherwise, and the worker still takes the host's messages, until
more than 4 MiB wait, which only the replies to the host's messages and the
executions' messages other than output can queue: a host that sends messages
without reading the replies holds the worker's memory to that. The executions
take turns at the room on the stream: in a turn of the loop, each reads at
most an eighth of that MiB of output, and the loop serves another execution
first in each turn, so that a program that always has output, with all the
credit that it needs, never keeps the others' output from the host.

- The host's messages reach an execution by its ID, and the execution's runner
  sorts them by generation: one of an earlier generation is stale, and one of
  an ID that the worker no longer runs, as after the execution's terminal
  record, is late. Neither changes anything. The worker also remembers the
  latest generation that the host started of up to `worker::MAX_HISTORY`,
  1024, IDs, those of the starts that it refused included, and forgets the ID
  that it heard of first beyond that, so that a start of an earlier generation
  is stale even after the execution's terminal record. A message that the
  engine refuses, such as credit beyond the window, a body that does not
  decode, or a second start of the same execution ends the session with an
  error.
- A start of a later generation abandons the execution of the same ID, as a
  reset of the session abandons every execution: the worker drops it, and it
  runs until it ends, detached from the host, which receives nothing more
  about it.
- The worker admits at most `worker::MAX_EXECUTIONS`, 8, executions at once,
  abandoned ones included, and those that keep their places once they ended,
  as the items below describe, and none that would hold more than
  `worker::MAX_EXECUTION_MEMORY`, 1 MiB: its runner's buffers, a chunk of each
  output stream and the input's window. It refuses a start beyond those, one
  whose identity or entry it refuses, as the next item describes, or one whose
  cgroup it cannot prepare, or whose program it cannot spawn, with a terminal
  record whose launch failed at stage 0, with `EAGAIN`, `ENOMEM`, the `errno`
  of the refusal, or that of the preparation or the spawn.
- Each program enters the layered root through `nvx::guest::entry`, as a
  `worker::Entrance` describes it: in new mount, PID, and UTS namespaces, with
  the root moved over `/`, the sandbox's hostname, the entry's default,
  `nvx-sandbox`, after a cold boot, and the record's in a clone, and no time
  status, and with the publication of listeners that the root configuration
  names, published unless it names them unpublished. After a cold boot, it sees
  the image's own machine ID and `/etc/resolv.conf`, so that the worker binds
  nothing into the root, and in a clone, the machine ID that the record names,
  which the clone's application bound for every execution if the record asks
  for a generated one, and, if the record's network names a DNS server, the
  resolver file that names it, which the application bound over the root's
  `/etc/resolv.conf`. It runs as
  the identity that exactly one source names: the sandbox's workload identity,
  with no supplementary groups or capabilities, or, in a sandbox without one,
  the identity that its start names, with its supplementary groups, its
  capabilities, and its account mode. A start that names an identity in a
  sandbox with a workload identity, or none in a sandbox without one, is
  refused with `EINVAL`. Before the worker prepares the execution's cgroup, the
  entry checks the identity, and reads its account in existing-account mode: it
  refuses with `EPERM` a capability outside its allowlist, any capability of a
  user ID other than 0, and `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` while
  listeners are published, with `ENOENT` an identity without the user ID's
  passwd entry or the group ID's group entry, with the error of an account
  database or a root that it cannot read, and with `EINVAL` otherwise. The
  program starts in the working directory of its start, or in the root's `/`,
  with exactly the environment of its start, none of the worker's own, and with
  its standard input from the host or the null device. It leads a session of
  its own, without a controlling terminal, and the process group that the
  session creates, and, as PID 1 of its PID namespace, its end ends every
  process of the namespace. The guest's network device, its addresses, and the
  announcer of its listeners stay with NVX's `/init`, which configures them
  before PID 1 starts, but for the announcer of a clone, which the clone's
  application starts; see [Networks](#networks) and
  [Template mode](#template-mode).
- Each execution runs in a cgroup of its own below the workload cgroup of the
  root configuration, `execution-N`, where N counts the cgroups that the worker
  prepared. The worker prepares it with
  `nvx::guest::cgroup::ExecutionCgroup::prepare`, and the program joins it as
  it spawns, before it runs, so that its descendants start in it, however they
  leave its process group or session. The execution settles through that
  cgroup alone, which leaves the other executions running: once the program
  ended, as its status, which the execution collects itself, tells, the
  execution kills what is left in the cgroup, and settles once the cgroup is
  empty. So no descendant outlives the program or holds its output open, and
  an execution whose program ends on its own loses nothing, however slowly the
  host takes its output, and is reusable.
- An execution keeps its place among `worker::MAX_EXECUTIONS` until its cgroup
  is empty and the worker removed it, which the worker does once the runner
  returned the terminal outcome of an execution that settled. One that did not
  settle, whose outcome is fenced, keeps its place and its cgroup until the
  host stops the worker, as does a start whose program the worker spawned, but
  whose execution it could not have: until the worker collected the status of
  such a program, the program may still join the cgroup, so that a removal
  could hide it once it ended.
- Without cgroups, as where the in-process tests cannot create them, the
  root's operations name none, and the program's process group stands in for
  its cgroup: its execution kills the group as a whole. The worker collects the
  status of every program itself then, without waiting for it, and learns that
  a program ended from a pidfd of its own. Before it collects the status, it
  kills what is left of the program's process group, as the settlement through
  a cgroup kills what is left in the cgroup: until then, the program, a
  zombie, keeps its process ID, which is the group's, from any other process.
  So a descendant that stayed in the group neither outlives the program nor
  holds its output open. Nothing proves that a descendant that left the group
  is gone, though, so each outcome is fenced: once the program ended, a stream
  that such a process may still hold has the settlement time and the grace to
  close, after which the runner gives up on it, while a stream that no process
  holds any more loses nothing, however slowly the host takes it. Such an
  execution keeps its place until the worker collected its program's status.
- Before it hands an execution the host's cancellation, the worker gives the
  execution a turn, as the loop does, which collects the status of a program
  that ended, and reports the program's end to the runner. So the end of a
  program that ended before its timeout while the worker did not run, as when
  the guest's processor did not, stays the program's own, without the cause
  of a timeout or of the cancellation, even if the cancellation comes after
  the timeout's instant.
- The worker forgets an execution once its runner returned the terminal
  outcome, which may come before the program's end, as once the runner gave up
  on a program that a kill did not end in time: forgetting it kills the
  program, and what is left in its cgroup or its process group, again, and the
  worker collects the program's status once it ended. On a stop, or once its
  session ended otherwise, the worker drops every execution, which kills the
  program of each one that has not ended, with what is left in its cgroup or
  its process group, and what is left in the cgroups of the executions that did
  not settle, and waits up to a second in all for those programs to end and
  those cgroups to empty, removing each cgroup that emptied once it collected
  the statuses of those programs.

The worker's `entries` probe runs two more commands of the guest agent:
`runtime-adapter-guest entries ROOT CGROUP`, its helper, which runs the probe's
executions in the root ROOT, each placed in the cgroup CGROUP, in a mount
namespace of its own, reports their facts on standard output, and exits with 0,
or with 1 after a diagnostic on standard error; and
`runtime-adapter-guest inside DIRECTORY DESCRIPTOR`, which each execution runs
from inside the root, and which reports what the execution sees and may do;
see [Probes](#probes).

### Template mode

A template's VM captures a snapshot of its guest before the guest assembles any
root, so that every clone that a restore of the snapshot starts attaches a root
of its own. The host opens no application session with it, and waits for no
`Ready`: a committed capture ends the VM. Its kernel command line has
`nvx_sandbox=1`, so NVX's `/init` runs the time ABI's boot step, then hands the
VM to the guest agent's PID 1 of a layered root, which starts the worker, as in
every VM with a layered root. OpenVMM adds `nvx_snapshot_tier=platform` to the
kernel command line of a VM that captures a platform snapshot.

On x86_64, whose time ABI a capture needs, the worker reads the kernel command
line before anything else, with `template::boot`, which splits it as the kernel
does, with `nvx::protocol::cmdline::parameters`. A command line in which the
kernel finds no parameter `nvx_snapshot_tier=platform` boots cold: the worker
goes on as above, whatever else the command line holds. With it, the VM is a
template's, whose snapshot holds the platform's invariant configuration alone,
which every clone shares. So the worker accepts only the parameters of the
platform tier's allowlist, `template::is_platform_token`: OpenVMM's own check,
which the adapter compiles from the excerpts of OpenVMM's sources that NVX's
tests keep in `nvx-rs/tests/vectors/openvmm/`, so that the worker accepts
exactly what OpenVMM's capture of a platform snapshot admits, as text, byte for
byte:

- `earlycon=xe9`, `console=hvc0`, `console=hvc1`, `reboot=t`, `panic=-1`,
  `nvx_sandbox=1`, `nvx_config=0xd0010000,65536`,
  `nvx_snapshot_tier=platform`, and `nvx_control_tty=hvc2`;
- `nr_cpus=`, `maxcpus=`, `virtio_mmio.device=`, `virtnet_ip=`,
  `virtnet_mask=`, `virtnet_gw=`, `virtnet_dns=`, `virtnet_ip6=`, and
  `virtnet_gw6=`, with any value that the host chooses.

It refuses any other parameter, one that is not UTF-8 text among them, and a
double quote, which the kernel drops from a parameter, or `--`, after which the
kernel hands the rest of the command line to init, though init may still read
it from `/proc/cmdline`: OpenVMM refuses both in the command line of a VM with
a control console. OpenVMM checks the tokens that it finds at ASCII
whitespace, while the kernel also splits at the vertical tab and at 0xa0, so
the worker checks the parameters that the kernel finds. Otherwise, the worker,
through `worker::capture::capture_on_terminal`:

1. opens the control tty, in raw mode, as the worker of a VM that boots cold
   does;
2. bootstraps the guest for the root of every clone, with the template's own
   root configuration, `worker::capture::template_config`, the default one,
   since the host sends a template's VM none: it mounts the cgroup v2
   hierarchy unless it is mounted, and prepares the agent's and the workload
   cgroups, with their resource values, as the worker of a VM that boots cold
   does before it assembles its root. The snapshot holds them, so that a
   clone assembles its root without them;
3. attaches the control session with `nvx::guest::nvxs::GuestConnection::attach`,
   takes the reset with which the broker answers the attachment at once,
   acknowledges it with the guest agent's receive window, and waits until the
   tty has sent the acknowledgement, so that the snapshot holds the guest's
   session active, in the broker's first epoch. A clone then reads no reset but
   the one that the restore queues, of another instance: the restore's broker
   would finish sending a reset that the template's broker began to send, and
   the clone would read one that the template did not. The worker reads nothing
   else from the session before the capture, and serves no application session;
4. prepares the guest for the capture with
   `nvx::guest::clone::CapturePreparation`, which closes the worker's fence of
   workload admission, checks that nothing is mounted from the `distro` or the
   scratch block device, at the MMIO windows from which the worker assembles a
   root, from an overlay, or from OpenVMM's host filesystem device, whose tag
   is `microvm`, then waits for the time ABI's boot checks, runs its capture
   checks, and suppresses the stall detectors;
5. requests the capture with `CaptureBarrier::capture`, with the platform
   tier's request, which keeps no scratch.

Closing the worker's fence, `worker::capture::Admission`, acquires it: a close
fails while the fence is closed, whoever closed it, and leaves it closed. The
worker of a template's VM admits no execution before the capture. The worker
returns from the capture only once the capture ended without a snapshot, or
failed, or in a clone, which a restore of the snapshot started. In a clone, the
worker repairs the restore next, with `nvx::guest::clone::repair`, behind its
fence, which stays closed: the repair brings the CPUs of the restore's
processor target online, activates the restore's memory target, reseeds the
kernel's random number generator with the restore's fresh entropy, and
finishes the restore, with the CPUs that the restore brought online and the
acknowledgement that the host awaits, which hands the deferred time work, the
restore checks, those of the new CPUs among them, and the release of the stall
detectors, to the time daemon. NVX's `/init`
started the daemon in the template, with the time ABI's boot step, before the
guest agent, so every clone holds it, and the guest agent's PID 1 collects its
status. The repair takes the time ABI in strict mode, so that a violation of
the ABI fails the repair, rather than stopping the VM, and the worker ends the
VM with the failure's status. A failure of the deferred time work stops the VM
with the time ABI's own status, since the daemon enforces the ABI.

Once it repaired a clone, the worker activates it, on the same tty, with
`worker::work_clone`, through the clone's `nvx::guest::clone::Activation`,
whose deadline ends `worker::activation::ACTIVATION_TIMEOUT`, 120 seconds,
after the repair's acknowledgement of the restore:

1. It resumes the session that the template attached, and whose first epoch it
   acknowledged, which the snapshot holds, and acknowledges the reset that the
   restore queued, the only one that the clone reads, with its receive window:
   it attaches no session.
2. It takes the host's `Activate`. The activation decodes its record, which
   must be a clone's, with a restore section, and checks that it expects the
   CPU count and the memory size that the repair found, before anything has a
   side effect.
3. It applies the record with `nvx::guest::clone::apply`, with
   `worker::capture::root_template` of the template's configuration and of its
   NIC's IPv4 gateway, which `template::boot` read from the template's kernel
   command line: its runtime directory, the `distro` and scratch block devices,
   its device timeout, the tag of the host filesystem device,
   `worker::capture::FILESYSTEM_TAG`, and the gateway, if the template has a
   NIC. The application assembles the root, an
   EROFS image or a GPT disk over a tmpfs or the scratch, in the record's
   overlay mode, waiting for each device until the activation's deadline at the
   latest, so that it ends by then, and checks the account of the record's
   workload identity, if it has one, in the assembled root, in existing-account
   mode. For a GPT disk, it first asks the kernel to reread the disk's partition
   table, so that the
   block devices of its partitions are the clone's disk's, not the template's
   placeholder's. The upper must be fresh: the assembly refuses one whose
   `upper` or `work` directory exists and is not empty. If the record asks for
   a generated machine ID, the application then generates one, once, after the
   repair's reseed, in `nvx::guest::clone::MACHINE_ID_FILE` of the runtime
   directory. If the record has an export, it mounts the host export through the
   host filesystem device, read-write, at `hostfs/root` of the runtime
   directory, outside the root, where only the guest's root user reaches it, as
   the worker of a VM that boots cold mounts the one that its kernel command
   line names. If the record's network names a DNS server, it writes a resolver
   file that names it, of mode 0444, in `nvx::guest::clone::RESOLVER_FILE` of
   the runtime directory. If the record's network publishes the workload's
   listeners, it starts their announcer,
   `/sbin/nvx-ingress inherit GATEWAY_IPV4`, with
   `nvx::guest::ingress::Announcer::start`: it binds a UDP socket to a
   privileged source port, from which alone OpenVMM's gateway takes
   announcements, and runs the program with that socket as its standard input,
   to the template's gateway, waiting until the program runs, but for no
   announcement. The template, captured under a policy that publishes nothing,
   runs no announcer of its own. It then binds the generated machine ID
   read-only over the root's `/etc/machine-id`, and the resolver file read-only
   over its `/etc/resolv.conf`, in the worker's mount namespace, for every
   execution; with the image's own machine ID and without a DNS server, it binds
   nothing.
4. Once it took what the session received while the application ran, which came
   before `Ready`, it completes the activation, which opens the fence, and
   answers `Ready`, which reports the activation after the guest agent's build,
   to the `Activate` and to each retry of it, byte for byte, that came
   meanwhile, which joins the application. Anything else that came meanwhile, a
   reset included, fails the activation, as before the application, so the host
   waits for `Ready` before it sends the mapping table or the start.
5. It then serves the host's order, as after a cold boot: the mapping table that
   the record announces, which it applies from the record's export, the start,
   the executions, the probes, and the stop. The record's workload identity, if
   it has one, is the whole sandbox's, as a root configuration's is: every
   execution runs as it, and a start that names an identity is refused. Every
   execution takes the record's hostname. The host tells the workloads the
   endpoint of the network's proxy, as after a cold boot: the host program's
   `exec` adds it to each execution's environment, so the worker leaves each
   execution's environment exactly its start's. A clone's executions enter the
   root with the publication of listeners that the record's network names:
   published under a policy that publishes them, which refuses
   `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW`, and unpublished otherwise, a clone
   without a network included. The worker polls the pidfd of the announcer that
   the application started with the session's stream: once the announcer ended,
   before the clone was ready or after, the clone ends with status 57, as one
   whose announcer could not start. On the stop, the worker stops the announcer,
   closes the export's root, and removes the application's binds, which keep the
   root's mounts busy, before it tears the root down, through
   `nvx::guest::clone::Applied::teardown`. It also answers the
   [`activation` probe](#probes), with when the events of its activation and
   order happened after the repair's acknowledgement of the restore.

Until the clone is ready, its session takes nothing but that `Activate`:
another reset, any other message, a `Stop` and a `Ping` included, an
`Activate` that the activation refuses, and the passing of the deadline fail
the activation finally, as does an application that failed, which nothing
retries. Once the clone is ready, a retry of the same `Activate`, byte for
byte, gets the same `Ready` without a second application, and any other
`Activate` fails the activation. A failed activation ends the clone with the
status of its class, which `worker::activation::failure_status` selects, and a
broker that closes the stream before the clone is ready ends the clone with
status 1, once the worker dropped its executions, and waited for its stream,
and then the tty, to take what it wrote, for at most
`worker::UNACTIVATED_DRAIN`, a second each, whatever the stream does, so that a
stream that takes nothing, as once the broker failed, does not keep the clone
from ending; the fence stays closed if the clone was not ready. Once the clone
is ready, a broker that closes the stream ends it as it ends the worker of a VM
that boots cold. The worker of a VM that boots cold refuses an `Activate` as
any message that it does not take in its state. The worker, and with it the VM,
ends with one of these statuses, after a diagnostic on standard error:

| Status | When |
| --- | --- |
| 0 | In a clone, the host stopped the worker once the clone was ready, after a teardown that succeeded |
| 1 | The control tty cannot be opened or put in raw mode, the attachment or the acknowledgement of the broker's reset cannot be sent, or the broker closes the session or sends anything but a reset first, or, in a clone, its session failed or the broker closed it |
| 13 | The bootstrap of the guest for the template's root failed, before the attachment |
| 15 | In a clone, the root's teardown failed once the host stopped the worker |
| 20 | The kernel command line names the platform tier, but holds another parameter, a double quote, `--`, or a gateway that OpenVMM does not render, or the worker cannot read it |
| 21 | The fence of workload admission cannot be closed |
| 22 | Something is mounted from the template's devices or from an overlay, or the mount table cannot be checked |
| 23 | The time ABI's boot checks did not complete |
| 24 | The time ABI's capture checks failed |
| 25 | The stall detectors cannot be suppressed |
| 26 | The VM generation ID cannot be read |
| 27 | The capture request failed, which leaves the fence closed, other than by a violation of the time ABI in a clone |
| 28 | The stall detectors' settings cannot be restored once the capture ended without a snapshot |
| 29 | A stage of the capture failed that this build of the worker does not know |
| 30 | The capture ended without a snapshot, as without a destination, or once OpenVMM rejected it, and the template continued |
| 40 | The clone's restore cannot be repaired: it holds no fresh entropy, or its capture finished it |
| 41 | The clone's online CPUs cannot be read, are not a prefix of the CPUs, exceed the restore's processor target, or the CPUs of the target cannot be brought online |
| 42 | The restore's memory target is refused, before any memory block is added |
| 43 | The activation of the restore's memory target failed |
| 44 | The reseed of the kernel's random number generator failed |
| 45 | The restore's finish failed, other than by a violation of the time ABI |
| 50 | The clone was not ready before its activation's deadline |
| 51 | The host broke the clone's session: it reset the session again, or sent another request, before the clone was ready, or an `Activate` of another transaction, or one that changes the accepted record |
| 52 | The `Activate`'s record does not decode, breaks a limit, or is a cold boot's |
| 53 | The record expects other resources than the clone has |
| 54 | The root's assembly failed, its upper not fresh among its causes, or the kernel did not reread the partition table of the record's GPT disk before it |
| 55 | The clone's workload cannot be activated: the image refuses the record's workload identity, as it holds no account of the identity, or not its group, or its account databases cannot be read, or the machine ID that the record asks for cannot be generated, or the record's hostname or that machine ID cannot be set for every execution |
| 56 | The record's host export cannot be mounted |
| 57 | The record's network settings cannot be applied: the resolver file that names its DNS server cannot be written, or set for every execution, or the announcer of the listeners that its network publishes cannot start, as without the template's gateway; or that announcer ended |
| 58 | The worker's own order misused the activation: it completed an activation that did not run, or started the sandbox before the clone was ready, or again |
| 193 | A conformance check of the time ABI failed in the clone's repair, such as the finish's, which needs a running time daemon |
| 195 | The time ABI's part of the clone's repair failed, such as the validation of the restore packet, or the record of the restore |

The statuses from 21 to 29 are `template::CAPTURE_STATUS_BASE` plus the number
of the stage that failed, in the order in which
`nvx::guest::clone::CaptureStage` declares the stages. A capture request that
fails with a violation of the time ABI, which it does in a clone whose restore
the time ABI could not repair, ends with the violation's status instead, as
`worker::capture::capture_status` tells. `nvx::protocol::status` documents
the statuses of a clone's repair, from 40 to 45, which
`nvx::guest::clone::RepairError::status` selects, the time ABI's own, from 193
to 195, and those of a clone's activation, from 50 to 58. A stage whose
failure may leave the worker some of the permission for the time ABI's ports
fails with the time ABI's port error, and the worker then starts no thread or
process before it ends. A violation of the time ABI in enforcing mode, in the
capture's preparation or in the time daemon, stops the VM with the time ABI's
own status, from 193 to 195, instead.

## Host program

`host::Client` is the host program's client of the guest agent. It runs over
the host's end of the control session, through `nvx::host::nvxs`, on any
transport to the session's host endpoint, such as the one that
`nvx::host::session::connect` opens to a VM's control endpoint:

- `Client::attach` attaches with the VM's launch capability, waits while the
  broker waits for the guest, and reports the identity of the session's epoch
  that the broker attached it to;
- `send` sends a message in as many of the session's records as it takes, and
  `receive` returns the guest agent's next message, whether it spans records
  or shares one with the next message;
- `ping`, `echo` and `stop` each send a request and check its answer: `Pong`,
  an `Echo` of the same bytes, and `Stopped`. `stop` also returns the failure
  of a layered root's teardown, which the guest reports before it confirms.
  `stop_skipping` stops the guest agent as `stop` does, but skips the messages
  before `Stopped` that its caller accepts, such as those that the guest may
  still send about a driver's executions, which `Executions::is_about`
  accepts;
- `configure_root` sends a root configuration, and returns how the guest
  answered: `Ready`, with the guest agent's build, or the stage of the root
  that failed;
- `activate` sends a clone's activation record in an `Activate`, and returns
  the guest's `Ready`, with the guest agent's build and the clone's
  activation. A clone whose activation failed answers nothing, but ends, and
  its session with it;
- `map` sends a part of the host's mapping table, and returns how the guest
  answered: `Mapped`, or `MapFailed`, with how many of the table's mappings it
  applied and the problem; `start` sends `Start`, and checks that the guest
  answers `Started`;
- `probe` sends a probe, and returns the facts that it found, or the problem.

Besides the session's own buffers, the client holds at most the rest of one of
the guest's records and one partial message. A reset of the session ends the
attachment, and its partial message with it, because the next epoch starts
with a new message, in a new attachment.

A failure of the session, a send that fails or times out, a malformed message,
an answer whose body does not decode, and a message that only the host program
sends, an execution's start, input, end of input, or cancellation, and a
clone's `Activate` included, end the client, and its later calls fail. A
receive that times out doesn't: it keeps what arrived. A request ends the
client on any failure after the framing accepted it, a timeout included,
because an answer that arrived later would answer the wrong request.

`host::Executions` is the host program's driver of
[executions](#executions), over one attachment of the client:

- `start` sends an execution's start, once it checks that the start decodes,
  as the guest decodes it. A start that does not, such as one with a relative
  working directory, and one of an ID that the driver started already, fail
  before anything is sent, and leave the client usable: the guest would end its
  session on either. So do a start that says that the host sends the program's
  input, one whose output windows count bytes, or more than
  `host::MAX_OUTPUT_FRAMES`, 32, frames, one beyond `exec::MAX_EXECUTIONS`, 8,
  executions, as the guest runs no more at once, and any once the driver ran:
  a driver runs one batch, whose executions it starts before it runs them;
- `run` carries the guest's messages to each execution's
  `exec::HostExecution`, and theirs to the guest, until every execution
  finished. One thread runs them all: each receive waits at most until the
  earliest timer of the executions, which so fires in time whatever the guest
  sends, and no execution's messages wait for another's. A message of the
  guest about no execution, about one that the driver did not start, whose body
  does not decode, or that breaks the engine's rules, ends the run, as a
  deadline that passes first does: in a receive, which leaves the client
  usable, or in a send. Each send waits, as the client's do, until the
  transport takes the message or the run's deadline passes, and no timer fires
  meanwhile, so a guest that stops reading the session holds the run until its
  deadline, when the send's timeout ends the client.

Even a guest that reads can hold a send while the host does not read: the
session's broker passes the guest's credit for the host's records on only
behind the guest's own records, and takes no more of the host's records than
that credit allows. So the driver sends little before it reads: no program's
input, which one thread could not send while it reads the programs' output; the
starts of at most 8 executions, whose answers wait for the run, as no program's
output flows before the run's credit does; and, in the run, besides a
cancellation for each execution, at most a credit for each message that it
reads, of output windows of at most 32 frames, so that the guest's messages
ahead of its own credit stay few. The guest may still send messages about the
executions of a run that gave up on one or failed, so a later batch takes an
attachment of its own, as each command of the host program does, and a stop in
the same attachment skips them, with `Client::stop_skipping` and
`Executions::is_about`, as `runtime-adapter-host run` stops its clone.

The guest ends its session on a second start of the same key, even in a later
epoch of the session, so the keys of a driver's executions must be new to the
guest.

### The host's order

A VM with a layered root is ready for workloads once its guest assembled the
root, and applied the host's mapping table. The `host` module runs that order
over a client in two steps, between which the host program releases the files
that it handed OpenVMM, and its [step hook](#the-step-hook) may pause it:

1. `host::ready` sends the root configuration, which announces how many
   mappings the table holds, and waits for `Ready`, whose build must be the
   one that the host ships. The host sends a guest agent of another build
   nothing more, since it cannot tell what that build would do with it.
2. `host::start_sandbox` sends the table in the parts that `root::encode_maps`
   splits it into, each once the guest applied the one before, then `Start`,
   and waits for `Started`. A table of no mapping sends no part.

A part that the guest fails ends the order: the host sends no more of the
table, nor `Start`, so the guest starts no workload on a partial table, and
sends `Stop`, which tears the root down, mappings first, as NVX's managed
lifecycle stops a guest whose mappings failed. The order fails with an
`host::OrderError`: the stage of the root that failed, another build, the
mapping that failed with how the stop ended, a table that does not encode,
which fails before anything is sent, or a failure of the client. The host
program [tears the VM down](#commands) after any of them.

A clone, which a restore of a [template's](#template-mode) snapshot started,
takes the host's activation of its record instead of a root configuration:
the guest assembles the clone's root from the record, with the inputs that the
template's capture kept, as it activates the clone. Its first step is
`host::activate` instead of `host::ready`: it sends the clone's activation
record in an `Activate`, the first request of the session, and waits for
`Ready`, whose build it checks as `host::ready` does, and which reports the
clone's activation. The table and `Start` then follow as after a cold boot, in
two steps: `host::map_table` sends the table as `host::start_sandbox` does,
and ends the order in the same way at a part that the guest fails, and
`Client::start` sends `Start`. `host::CloneOrder` runs these steps as the
framing of the restore sequence of `nvx::host::template`, which checks the
activation against the record, sends the table only for a clone with an
export, and `Start` only once the guest applied every mapping, and which
replaces a session that the order takes for lost: one that a reset of the
broker, the end of its stream, or a failure of the transport ended while the
host attached or awaited `Ready`. A clone that was ready answers the same
record, byte for byte, with the same `Ready` in the new session, and one that
took the reset before it was ready has ended. OpenVMM's broker ends the stream
both to end an attachment and to refuse one, so the order takes such a refusal
for a lost session too, whose retries the sequence's limit of sessions bounds.
Any other failure is final: a refusal that the broker sends, a deadline that
passed, another build, and a message that breaks the framing. The guest's
`MapFailed` does not tell whether the request's mappings or a failure of the
guest's own failed the table, so the sequence attributes it, as any other
failure of the table, to what the host saw of it: a broken protocol, unless the
deadline passed or the session was lost.

## VMs

The host program runs the guest agent in VMs of NVX's artifacts, through NVX's
supervisor, `nvx::host::supervisor`, each in a sandbox store of
`nvx::host::store`.

### The guest agent in place of NVX's agents

NVX's initramfs hands a VM of the managed lifecycle to NVX's managed agent: its
`/init` runs
`exec /sbin/nvx-managed-agent /dev/hvc2 - nvx-managed UID GID NAME HOME` as
PID 1. A VM with a layered root, whose kernel command line has
`nvx_sandbox=1`, goes to NVX's init agent instead, which assembles the root:
`/init` runs `exec /sbin/nvx-init-agent`, without arguments.
`initramfs::write` copies that initramfs, and appends to the copy a second
archive, in the `newc` format, which Linux unpacks over the first:

- the guest agent, at `/sbin/runtime-adapter-guest`;
- a script, `/sbin/runtime-adapter-init`, that runs
  `runtime-adapter-guest init "$1"`, with the control tty, the first of the
  arguments that `/init` passes the managed agent;
- a symbolic link from `/sbin/nvx-managed-agent` to that script;
- a script, `/sbin/runtime-adapter-layered`, that runs
  `runtime-adapter-guest init /dev/hvc2 layered`, which names the control tty
  itself, since `/init` passes NVX's init agent none;
- a symbolic link from `/sbin/nvx-init-agent` to that script, so that NVX's
  init agent never assembles the root in the adapter's VMs.

Linux removes a file whose type an archive changes before it creates the new
one, so each link replaces NVX's agent without writing to its file, which could
have other names. The second archive starts at a multiple of 4 bytes, as Linux
requires of an uncompressed archive, after zeros that Linux skips. NVX's own
images don't change.

### The runtime identity

The supervisor records the runtime identity of each launch, and its reattach
compares the recorded identity with the caller's. `runtime::Runtime::from_dir`
takes NVX's artifacts from a repository checkout or an extracted release, with
`nvx::host::artifacts::Artifacts::from_dir`, and their runtime identity from
`nvx::host::artifacts::ArtifactDir::runtime_identity`, which names each
artifact by the SHA-256 digest that the install metadata beside it records,
never from its contents: a release's `SHA256SUMS`, or the build provenance in a
checkout's `build/`, where `nvx-rs download` also installs a release's.

The VMs boot a copy of NVX's initramfs with the guest agent in it, so the
identity of their initramfs names both:
`sha256:DIGEST+runtime-adapter-guest/BUILD`. The guest agent has no install
metadata of its own, so the caller names its build, BUILD, which is the crate's
version unless the caller names another. A host program that ships another
build of the guest agent runs another runtime, and finds a VM of this one
mismatched.

### Staged images

The images of a layered root are staged first, in the sandbox's store, as
`staged::Staged::stage` stages them. Staging verifies the image explicitly,
with `nvx::host::sandbox::distro::DistroBinding::verify`, which hashes the
image only when a manifest is beside it, and compares the manifest's name of
the image and digest with the image's own. It then checks the binding from
metadata, as a launch checks it, so that it stages no binding that every launch
would refuse. It binds the ext4 scratch image of an upper on scratch with
`nvx::host::sandbox::scratch::ScratchBinding::bind`, which neither reads nor
claims the image. Staging is where hashing belongs: a launch never hashes.
Staging also refuses images that no launch could attach, as the plan of a
launch checks them.

Staging records both bindings in the sandbox's store, in
`runtime-adapter-staged.json`, in place of those staged earlier:

```json
{"format": 1,
 "distro": {"path": "/images/distro.erofs", "binding": DISTRO_BINDING},
 "scratch": {"path": "/sandboxes/one/scratch.ext4", "binding": SCRATCH_BINDING}}
```

Each `path` is the absolute path of a file, as a plan names it, and each
binding the serde form of NVX's `DistroBinding` or `ScratchBinding`. `scratch`
is `null` when no scratch image was staged. A reader refuses another format,
an object that lacks a member or has another, and a binding that does not
decode. A launch takes the bindings of its images from the record, by their
paths, and [checks them](#the-checked-handoff) before it spawns the VM.

### Layered roots

`layered::LayeredLaunch` plans a VM with a layered root with NVX's typed plan
of a cold layered launch for an adapter's own guest,
`nvx::host::sandbox::SandboxPlan::adapter`, and checks it with
`SandboxPlan::validated`. The adapter's plan holds none of the settings of
NVX's sandbox profile, such as a workload identity, since the guest agent runs
its executions itself. The plan names the images by absolute paths, since
OpenVMM runs in the sandbox's store, and the image's identity, which a launch
requires, since a binding names it. The VM's kernel command line is
`nvx_sandbox=1`, the launch's own token, with which NVX's `/init` hands the VM
to NVX's init agent, which the guest agent replaces, then the plan's boot
tokens of the root alone: the image's, such as
`nvx_layer=distro,0xd0003000,erofs:UUID`, and the upper's, such as
`nvx_scratch=0xd0006000,ext4`. OpenVMM's arguments are the plan's, which
attach the [checked files](#the-checked-handoff) of the image and the scratch
as block devices, and the VM's [network](#networks), if any, then those of the
export of the launch's [shares](#shares), if any, then the VM in one process,
NVX's default memory, the kernel, the initramfs, that command line, no virtio
console, the control console with the launch capability on standard input,
and the outcome report. They name no workload identity, and no managed
lifecycle: OpenVMM only checks the arguments of the managed lifecycle and
labels its outcome report with it, and only NVX's init agent reads its boot
token. A VM without a layered root keeps the managed lifecycle, with which
NVX's `/init` starts the managed agent, which the guest agent replaces.

The boot tokens carry only what NVX's boot contract defines. The launch sends
the rest in the [root configuration](#root-configurations), once it attaches:
the defaults, with the runtime directory that the caller names, as it is, so
that the guest checks it, the size of each image in sectors, if it is a whole
number of them, which the guest checks against its block device, the overlay
mode and the scratch's journal policy that the caller names, if any, and the
number of the mappings of the export's table, if the launch shares anything.

The launch records what it launched in the sandbox's store, beside NVX's
runtime records, in `runtime-adapter.json`, before it spawns the VMM:

```json
{"format": 1, "mode": "layered", "kernelCommandLine": "nvx_sandbox=1 ...",
 "plan": PLAN, "bindings": {"distro": DISTRO_BINDING, "scratch": SCRATCH_BINDING},
 "rootConfiguration": "version=1\nruntime=/run/runtime-adapter\n..."}
```

`mode` is `layered`, or `blockless` for a VM without a layered root, whose
`plan`, `bindings`, and `rootConfiguration` are `null`. `plan` is the persisted
form of the VM's `SandboxPlan`, whose `network` keeps the VM's
[network](#networks), or is `null` without one. `bindings` holds the bindings
that the launch checked, which the store [stages](#staged-images) for the
launch's images: the image's, and the scratch image's, `null` for an upper in
RAM. `rootConfiguration` is the root configuration that the launch sends,
whether or not the guest accepts it.

### The checked handoff

Before it spawns the VM, a launch checks the bindings of its images from their
metadata alone, with `LayeredLaunch::prepare`: NVX's
`DistroBinding::check` checks the image against its binding, under the distro
policy that this build accepts, and `ScratchBinding::check` checks the scratch
image of an upper on scratch against its own, and claims it for the sandbox.
Neither reads the images, and neither hashes. A launch fails before it begins,
leaving no launch journal, VMM log, or record in the store, if the store stages
no binding for the image or for the scratch image at its path, if a binding no
longer holds, as for an image that changed or was replaced after it was
staged, or if another sandbox's VM still claims the scratch image.

The checks return the files that they opened, which NVX's
`nvx::host::sandbox::PreparedLaunch` hands OpenVMM in place of the images'
paths, so that nothing that takes a path after the checks reaches the VM. On
Linux, OpenVMM inherits the checked files' descriptors, which its arguments
name as `/proc/self/fd/N`, and nothing else of the host program's; on Windows,
its arguments name the checked files by their final paths, which the checked
files keep anything else from taking while they are open, and OpenVMM inherits
the checked files and the scratch image's claim, so that its copies hold the
paths even if the host program ends before OpenVMM has opened the images. The
launch holds the checked files until the guest has assembled the root, when
`PreparedLaunch::release` closes them, and on Windows OpenVMM's copies of them,
or until it has torn down a launch that failed.

### Shares

A launch may share host directories and regular files with the root, as
`sharing::HostFsSharing`: each `sharing::Share` at an absolute guest path in
the root, read-only or read-write. Neither exposes its fields: each field has a
getter and a setter, and each list of `HostFsSharing` a method that adds an
entry, as its option does. `HostFsSharing::plan` plans their export with NVX's
`nvx::host::filesystem::plan`, through an `ExportRequest` of a mapping for each
share at its target, the host paths that it hides inside the shares, the host
paths inside hidden ones that it shows again, with the directories on the way
to them listing only them, and the guest targets that the root keeps: those of
NVX's container root, `ReservedTargets::CONTAINER_ROOT`, and the trees that
the adapter keeps for itself. Each share must lie inside a host directory that
the adapter authorizes, once links are resolved: `HostFsSharing::plan` refuses
one outside them before it plans, and checks the export's directories and
files, which the planner resolves anew, again. The planner refuses what it
cannot export, such as a target that takes or covers a reserved tree, or a
read-only share inside a read-write one, which the host could not keep
read-only, a hidden path that does not exist inside a share, which nothing
would hide once a workload creates it, and a path to show again that lies
inside no hidden one.

`LayeredLaunch::share` attaches the export to the launch, which
`LayeredLaunch::export` returns, and announces its table, of at most 4,096
mappings, in the root configuration. NVX's adapter plan
holds no export, so the launch attaches it itself: OpenVMM's arguments export
the shares through OpenVMM's aggregate virtio-fs device, as
`ExportPlan::openvmm_arguments` renders them, and OpenVMM appends the boot
tokens with which the guest mounts the export, out of the root. Before it
spawns the VM, `LayeredLaunch::prepare` checks the export with
`ExportPlan::verify`: the host still keeps its read-only mappings read-only,
and its directories and files, and the paths that the guest could move inside
the parts that it may write, still name what they named when the export was
planned. Once the guest is ready, the launch sends the table,
`LayeredLaunch::table`, in the [host's order](#the-hosts-order): each mapping
before those inside its target. A [clone's](#clones) start takes the same
shares, whose export NVX attaches to the template's dormant filesystem slot as
an aggregate, as a launch attaches it, and whose table it sends as a launch
sends it.

### Networks

A layered launch may give the VM a network: an
`nvx::host::sandbox::SandboxNetwork`, the guest's address and the policy that
OpenVMM enforces there. `layered::read_network` reads it from a file, in the
serde form of a plan's network, as NVX's plan persists it, such as:

```json
{"guestNetwork": "10.0.0.2/24", "egress": "deny",
 "allow": {"name": "egress.allow",
           "rules": [{"to": [{"cidr": "192.0.2.0/24", "except": []}],
                      "ports": [{"protocol": "tcp", "port": 443, "endPort": null}]}]},
 "deny": {"name": "egress.deny", "rules": []},
 "ingress": "deny", "hostLoopback": "deny", "hostLoopbackForwards": [],
 "proxy": null}
```

The form requires every member and refuses any other. Reading it checks the
network as the plan does: the guest's address must leave a gateway, and the
policy must lower to OpenVMM's arguments. A launch reads its network before
anything else, so it refuses one that the plan refuses before it needs NVX's
artifacts or creates the store.

The launch puts the network on the adapter's plan, with
`LayeredLaunch::with_network`, which renders it after the block devices,
through NVX's one lowering of a policy, `nvx::host::network::openvmm_arguments`:
the portable profile at the guest's address, the defaults of egress and
ingress, the host-loopback access, the egress rules, lowered into OpenVMM's
native ones, the host-loopback forwards, and the proxy of a proxy-only
network, which the guest reaches at its gateway, at the URL that
`SandboxPlan::proxy_url` gives. A policy that lets nothing through attaches no
network device. OpenVMM appends the guest's network settings to the kernel
command line, from which NVX's `/init` configures the network device and its
addresses, and starts the announcer of the guest's listeners if OpenVMM
publishes them, before it hands the VM to the guest agent, which leaves all
three to `/init`. The launch's [record](#layered-roots) keeps the network in
its plan, which `LaunchRecord::read_plan` reads back from the store.

The guest's executions follow the network:

- The [root configuration](#root-configurations) says whether the guest's
  listeners are published, as OpenVMM publishes them: to the host's private
  networks with ingress allowed, or on host loopback with host-loopback access
  allowed, but neither with outbound host-loopback access alone nor through a
  proxy, and not without a network. The executions enter the root with that
  publication, so while listeners are published the entry refuses
  `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW`, with which an execution could bind
  the privileged port from which the announcer sends, or forge its datagrams,
  at stage 0 with `EPERM`.
- `runtime-adapter-host exec` gives each of its executions the URL of a
  proxy-only network's proxy in its environment, from the plan that the
  launch's record keeps, with `layered::with_proxy`, since NVX gives an
  execution no proxy settings, and its environment is exactly its start's: in
  each of `layered::PROXY_VARIABLES`, `http_proxy`, `https_proxy`,
  `HTTP_PROXY`, and `HTTPS_PROXY`, after the start's own entries. A start that
  names any of them keeps its own proxy settings.

In VMs, the [layered scenarios](#layered-scenarios) check what each kind of
network lets the guest's executions reach, and whether the host reaches the
guest's listeners, through publication or a forward.

### The step hook

For tests, `launch`, `reattach`, and `stop` take a step hook, `hook::StepHook`,
from the variable `RUNTIME_ADAPTER_STEP_HOOK`, which pauses the command at a
named step, so that a test can act between two steps, or exits the command
there without unwinding, as a crash would end it:

- `STEP:pause:PATH` creates the file PATH, which holds the step's name, and
  waits until the file is gone, then goes on: a launch leaves the time that it
  waited out of its deadline, so that a test may pause it for as long as it
  needs;
- `STEP:exit` exits at once with status 5: the command ends nothing that it
  started, such as the VMM, and the system releases what it holds, such as its
  descriptors or handles, and their locks.

The steps of a launch, in order, then that of a command that reattaches, are:

| Step | Where the command is |
| --- | --- |
| `checked` | It has checked the bindings of its images, and the export of its shares, if any, and holds the checked files, which claim the scratch image, but has begun nothing in the store, and spawned nothing. A launch without a layered root reaches it too. |
| `spawned` | The VMM runs, and the launch's journal records it, but the launch has not connected to the VM. |
| `ready` | The guest agent, of the build that the host ships, has assembled the layered root, and the launch has released the checked files, but has sent neither the mapping table nor `Start`. |
| `commit` | The guest has started the sandbox, if the VM has a layered root, the guest agent has answered, and the launch is about to commit: its journal still records the VMM, which no runtime record names yet. |
| `found` | `reattach` or `stop` has found the VM running, and holds it, under the sandbox's lock, with the session in which a clone answered the `Activate` of its record again, if the VM is a clone's, but has sent the guest agent nothing more, and stopped nothing. |

A value that names no step or action is a usage error. Without the variable,
the hook does nothing.

`start` takes the hook too, at `checked` alone, which it reaches once NVX's
`ClonePlan` checked the [clone](#clones) against the leased generation: the
bindings of its images, whose checked files the plan holds, and which claim its
scratch image, if it has one, its processor and memory targets, its network,
and the export of its shares, if any. The start has recorded nothing of the
clone yet, and spawned nothing, and the clone's launch checks the export again,
so a test may change a shared object there. A start refuses a hook at another
step as a usage error.

### Templates

The adapter's operations on templates, and on their clones, are commands of
`runtime-adapter-host`, which [Commands](#commands) details:

| Operation | Command | What it does |
| --- | --- | --- |
| Create, refresh | `create`, `refresh` | Serves an eligible generation of the template's capture class that the store holds, or captures a new one, as `refresh` always does, and prints it once the store committed it, then evicts the generations that it supersedes; see [Eviction](#eviction) |
| List, inspect | `list`, `inspect` | Reports the generations, their keys, classes, states, eligibility and its reason, and leases, and the metadata of a capture, without loading VM state or hashing; see [Listings](#listings) |
| Remove, retire | `remove` | Tombstones a generation, so that it takes no new lease, reports busy, with the status 7, while leases remain, and deletes it once they drained, but never moves or deletes a leased generation |
| Start from a template | `start` | Starts a clone of an explicit, eligible generation, with its image, scratch, and settings, which the sandbox stages; reports a selection of no template, of an expired generation, or of an ineligible one, and a clone's failure, attributed to the request, the host, or the template, or whose teardown is unresolved; never captures, boots cold, rehashes, or falls back to another format; see [Clones](#clones) |
| Run from a template | `run` | Plans the clone as `start` does, with its network and shares, stages the clone's images in a preflight that it reports, starts a clone as `start` does, runs one execution in the clone's root, and tears the clone down; see [Running a clone](#running-a-clone) |

`runtime-adapter-host create` serves a template of the guest agent from an NVX
template store, `nvx::host::template::Store`, with `Store::create_template`: a
generation of the template's capture class that the store holds, and that is
eligible at the host's time, or else a new one, which the store captures
through NVX's supervisor and publishes. `capture::Request` holds what the
command's options choose of the template, and `Request::capture` returns NVX's
`Capture` of it:

- the adapter's plan of the placeholder, with the template's network, if any,
  and the placeholder's binding, which the command plans and stages as `stage`
  plans and stages an image. The placeholder is a trusted artifact of the
  platform, whose length is the capacity of every clone's image, and the
  capture refuses one that anyone else may change;
- the capture class, from which the store takes the slot that serializes the
  class's captures, and the template's compatibility key: the runtime identity
  of the adapter's VMs, with the guest agent's build,
  `runtime-adapter-guest/BUILD`, the backend and the CPU profile, the
  processors, those online at boot, 1 unless the command names more, the
  fewest that the template's clones take, the RAM and its capacity, the
  geometries of the placeholder and of the dormant scratch, as OpenVMM presents
  a sandbox block device, which `capture::geometry` gives, and the network, as
  the capture attaches its NIC, which no network's policy changes;
- NVX's artifacts, with the initramfs that puts the guest agent in place of
  NVX's agents, which the command writes into a directory of its own in the
  system's temporary directory, private to the current user, as a launch
  writes it into the sandbox's private store, and removes once the creation
  returns.

NVX's store launches the capture's VM from the capture's transaction, in the
class's slot, whose VMM listens there, beside the snapshot's destination, as
OpenVMM requires. On Linux, the path of that socket,
`captures/C/transaction/control.sock` in the store, must fit a Unix socket's,
so NVX refuses a store whose path is too long for it.

The capture boots the template's VM with the kernel command line
`nvx_sandbox=1`, and for a template whose processors are not all online at
boot, `maxcpus=` with those that are, to which OpenVMM adds the platform tier's
tokens, `nvx_snapshot_tier=platform` among them, so the worker of the guest
agent captures the VM in [template mode](#template-mode) before it assembles
any root, and the capture ends the VM. OpenVMM records the processors online at
boot, from which each restore brings a clone's processor target online. The
store checks what the VM left, and publishes
the snapshot as its next generation, with the record of its capture and its
compatibility key, whose kernel command line holds the platform tier's tokens
alone: neither the placeholder's identity nor a root's tokens. The template has
a NIC exactly when its network lets something through: a network that denies
egress, ingress, and host-loopback access, without a proxy, attaches none, as
no network does. The capture attaches the NIC at the network's guest address,
under OpenVMM's complete deny-all policy, whatever the network's own: egress
denied without rules, ingress and host-loopback access denied, without a proxy
or forwards, the one policy of a snapshot that a restore may replace. So the
template's guest learns no DNS server, and starts no announcer of its
listeners, and the key holds what the capture made of the network alone:
whether there is a NIC, its addresses, and the identity of its deny-all
policy. The networks of every policy at one guest address therefore share one
class, and one template, which serves the [clones](#clones) of each of them.
The key's encoding, version 1, holds the NIC that way and has not been
released, so a template store that an earlier build created is not supported:
delete it and create a new one. There is no migration. The command refuses a
class that no capture takes, such as one of 3 processors, or
of more online at boot than in all, before it creates the store. A capture that
fails leaves the store's
generations as they were, and the command's diagnostic names the log of the VMM
of the class's latest capture, when the class's slot keeps one.

`runtime-adapter-host refresh` takes the options of `create`, and captures a
new generation of the template's class with `Store::refresh_template`, whether
or not the store holds an eligible one. Only `create`, for a class of which the
store holds no eligible generation, and `refresh` capture templates: no other
command captures one, and no restore triggers a capture. Both print the
generation that they served only once the store has committed it.

#### The template store

The adapter keeps its template store where its caller says: in the directory
STORE that each template command names, as the sandbox commands name a
sandbox's store. It is a directory of its own, apart from every sandbox's
store, private to the current user, whose parent exists. `create` and
`refresh` create it if needed, while `list`, `inspect`, and `remove` only open
it, and fail if it holds no template store. On Linux, the control socket of a
capture's VMM, `captures/C/transaction/control.sock` in the store, must fit a
Unix socket's path, so NVX refuses to capture into a store whose absolute path
is longer than 56 bytes.

#### Eviction

The adapter keeps, of each capture class, the generation that it served last,
and none that it supersedes. Once `create` or `refresh` has served a
generation of a class, which the store held already or has just published, it
retires every other generation of the class that the store published before
it, or that a tombstone quarantines, with `generations::evict`, and prints
which: those that are gone, and those that leases keep. `remove` retires the
generation that it names. A retirement, NVX's `Store::retire`, tombstones the
generation first, so that it takes no new lease, and deletes it, with its
payload, once no lease of it remains: it never moves or deletes a generation
that a clone leases. A generation that leases keep stays tombstoned, and the
next retirement of it, by a `remove` that names it, or by the next eviction of
its class, deletes it once its clones have released them. A retirement that a
crash interrupts once it has removed its generation from the store's index
leaves the generation's tombstone alone: the generation is gone, with its
payload, its leases, and the class that its entry recorded. So each eviction
also retires every such tombstone, of a generation below the index's next one
that the index no longer lists, whatever the class that it serves, and lists it
among those that are gone. An eviction that fails leaves the generations that
it did not retire to the next one, and fails nothing that the command served,
which the store has committed.

#### Listings

`runtime-adapter-host list` and `inspect` report what the store records of its
generations, from its index, its tombstones, and the records of its leases
alone, with `generations::entries` and `generations::entry`: each generation's
store generation, the identities of its compatibility key and of its capture
class, its state, `available` or `tombstoned`, whether a clone may restore it at
the host's time, and why, as `Generation::eligibility` decides it, and its
leases, and, from `inspect`, each lease's sandbox and launch, and the metadata
of the capture: its storage generation, its time contract's host, boot, clock,
anchors, and lineage counter, and the lengths of its saved state and of its
RAM. They read no generation's payload, neither its snapshot nor its key, whose
decoding checks the digest of the key's kernel command line, so they load no VM
state, and hash nothing. They take no lock, so each record is as they read it.

### Clones

`runtime-adapter-host start` starts a clone of a generation of a template store
in a sandbox store, through NVX's public API, `nvx::host::template`. Its
library side, `clone`, holds what the command's options choose of the clone,
`clone::Request`: its image, an EROFS layer or a GPT disk no longer than the
template's placeholder, its upper, on an ext4 scratch image or in RAM, its
processor target, the template's processors online at boot unless given, its
memory target, the template's RAM unless given, the workload identity as which
its executions run, its overlay mode, its scratch's journal policy, the bound
of a GPT disk's first partition, the machine ID that its executions see,
the host paths that it shares with its root, as a launch's
[shares](#shares), and its [network](#networks), as a launch reads it, which
must be at the guest's address of the template's NIC, under a policy of its
own.
`Request::plan` plans the image over the upper, with the network, as a launch
plans a layered root, `Request::export` plans the export of the shares as a
launch plans it,
and `Request::clone_request` returns NVX's `CloneRequest` of both, with the
bindings that the sandbox's store stages for the images, as `stage` stages
them, so the start hashes nothing. The clone's activation record names the
hostname `runtime-adapter`, which every execution sees, the image's own machine
ID, unless the request asks for one that the guest generates once for the
clone, which a read-only bind holds at the root's `/etc/machine-id` for every
execution, and the guest's defaults of the other settings, unless the request
names them; for a GPT disk, the bound of its first partition is 64 KiB unless
the request names one, as a launch's guest takes it by default. For a template
with a NIC, the record names the clone's network: the identity of its policy,
a digest of its serde form; the DNS server that OpenVMM serves the guest under
the policy, if any, which NVX finds as OpenVMM does, since the template's guest
learned none, and which the guest writes to a resolver file, of mode 0444, that
a read-only bind holds at the root's `/etc/resolv.conf` for every execution,
beside a generated machine ID; whether the policy publishes the workload's
listeners, as it does with ingress, or with host-loopback access both ways, for
which the clone's guest starts their announcer, and refuses
`CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` to its executions; and the URL of the
policy's proxy, if any, which every execution of the clone finds in its
environment, as an execution of a launch does; see
[Template mode](#template-mode). Without a DNS server, the root keeps the
image's own `/etc/resolv.conf`.
The guest of a GPT clone rereads the disk's partition table before it
assembles the root, since its kernel created the partitions of the template's
placeholder.

The command first reads the clone's network, if given, as a launch reads it,
and refuses one that the plan refuses, then plans the export of the clone's
shares, if it has any, and
refuses those that it cannot export, as a launch refuses them, then reads the
generation from the store's index, and, unless the request names both of the
clone's targets, its compatibility key, before it needs NVX's artifacts: the
processor target is then the key's processors online at boot, all of its
processors for a template that records none, unless the request names one, and
the memory target the key's RAM, unless the request names one. A key that
cannot be read is left to NVX's plan, which reads it again, and attributes the
failure, to a malformed generation, which quarantines it, or to the host, as it
does for every clone. The store then
leases the generation to the sandbox, `Store::acquire`, at the host's time,
which it refuses for a generation that a tombstone quarantines, or that is not
eligible then, by the monotonic clock of the host's boot that captured the
template, and by UTC otherwise. For tests, the host's time may come from the
variable `RUNTIME_ADAPTER_HOST_TIME`, as a clock fixture, such as another
boot's, in the form that `clone::host_time_text` writes:
`host=HOST,boot=BOOT,clock=CLOCK,monotonic=NANOSECONDS,utc=NANOSECONDS`, with
the identities of the host and of its boot, 32 lowercase hexadecimal digits
each, the clock as a manifest spells it, `linux-boottime` or
`windows-interrupt-time`, and the readings of the monotonic clock and of UTC.
It affects the store's check alone: OpenVMM's restore samples the host's own
time.

A selection that the store refuses ends the start before it leases anything,
with the refusal's record, in the form of a failure's, at the stage
`selection`, without a VM, and the status 10: a template store that does not
exist, and a generation that the store never published, or no longer holds,
name no template, `no-template`, by the evidence `no-store`, `unpublished`, or
`retired`; a generation whose capture the restore window of its time contract
no longer admits at the host's time is `expired`; and one that a tombstone
quarantines, or whose capture is after the host's time, by the clock that
measures its downtime, is `ineligible`, by the evidence `tombstone` or
`early`. The record's code is the code of NVX's error: `not-found`,
`template-tombstoned`, or `template-ineligible`.

NVX's `ClonePlan` then checks the clone against the generation from the
metadata of its files alone, under the sandbox's lock, which the lease holds:
its network, which must attach a NIC exactly when the template has one, at the
guest's address of the template's NIC, under a policy of its own, which the
clone's restore installs on the NIC, or, for a template without a NIC, none, or
one that lets nothing through, so that a clone at another guest address finds
no template in it,
the bindings of its images, which no one else may change, the geometry of its
image, which must have the placeholder's block sizes, and be no longer than
the placeholder, whose length is the capacity that the restore presents,
reading zeros past the image's end, and that of its scratch image, which must
be the template's scratch's, its length included, its processor target,
1, 2, 4, or 8, from the template's processors online at boot to its capacity,
which the restore brings online, and the clone's repair verifies, its memory
target, whole blocks of 128 MiB, from the template's RAM to its capacity, and
its export, if it has one, which it checks as a cold launch's: from 1 to 256
directories and files, the outermost shares, each of which the template's
filesystem slot, which its capture left dormant, attaches as a child of an
aggregate, with from 1 to 4,096 mappings, none of them at a target that the
container root keeps, and none of the directories and files may lie inside the
template store's directory or hold it, so that no clone reaches the payload of
a generation. A clone that
the template cannot serve fails there, before any VMM starts. The command then records the clone in the
sandbox's store, as `clone::CloneRecord`, in the format of a launch's
[record](#layered-roots), in the mode `clone`, with the template's kernel
command line, the plan, with the clone's network, and the bindings, so that
`probe` and `exec` read the plan back, and `exec` gives executions the URL of
the network's proxy, if any, as for a launch, no root configuration, and the
template store and the generation, so
that a later stop finds the store whose lease it releases. Once NVX checked
the export again, with `ExportPlan::verify`, OpenVMM restores the
generation's snapshot in place, through NVX's supervisor, as the launch that the
lease reserves, with the clone's image at the capacity of the template's
placeholder, over the scratch image, or, for a clone in RAM, with the
template's scratch absent, which OpenVMM attaches with no file, so that no
file of the host backs it, with the export on the template's filesystem slot,
which OpenVMM attaches as an aggregate, as a cold launch attaches it: each
directory and file a child, read-only or read-write, with the paths that the
export hides, shows again, or lets the guest write, and the template's NIC, if
it has one, under the policy of the clone's
network, which `--restore-network-policy` installs in place of the template's
complete deny-all policy, and the restore sequence activates the clone through
the
adapter's clone order, `host::CloneOrder`, which sends the clone's activation
record in an `Activate`, checks the guest agent's build in its `Ready`, sends
the export's mapping table, which the record announces, in order, as a launch
sends its own, and then `Start`. The record names the export by the identity of
its plan, and the table holds its mappings, each from a source that names its
child by the child's index, as a cold launch's table does. A clone without
shares has no
export: its restore leaves the slot dormant, its record announces no mapping,
and the sequence sends no table. If the guest fails a mapping, the sequence
sends no more of the table, nor `Start`, and the clone fails, once the clone
order asked the guest to stop, which tears down the root. The guest's
`MapFailed` does not tell the request's mappings from a failure of its own, so
the clone order takes it for neither, and NVX attributes the failure to what
the host saw of it: a broken protocol, unless the deadline passed or the
session was lost.

A clone that fails, from its plan to its commit, fails with NVX's
`CloneFailure`, which attributes the failure, by the evidence that the host saw
of it, such as a length that the request supplied, or a status with which the
guest ended the VM, to the request, the staging of its files, the host, the
template, or a cause that the evidence does not show, and which, from the
clone's launch on, records it in the sandbox's store, with the stage at which
the clone failed, how far it got, and how the teardown that followed ended. The
command prints the failure's record, then releases the lease, `Store::release`,
which removes the records with it, once the failure's teardown is resolved, and
nothing of the clone runs: the clone had no VM, its VM exited, or NVX
terminated it. A failure whose teardown is unresolved, such as that of a commit,
or one that NVX could not record before it ended the clone's VM, leaves the
lease, and the records, to a later reattach. Once it released the
sandbox's lock, which comes after the template store's in NVX's lock order, the
command quarantines the generation through its tombstone,
`CloneFailure::quarantine`, for a failure of the template alone. A guest agent
that does not answer the command's `Ping` or `Echo` once the clone's sandbox
started fails the clone so, through `Started::fail`, which ends the launch,
with the evidence that NVX's restore sequence gives the failures of its own
sessions: the passing of the deadline, the loss of the session, or a failure
of the protocol. A start that fails otherwise, at a step of the adapter's own
before the clone's launch, such as the read of the images that the sandbox
stages, releases the lease, since nothing of the clone runs.

The commands that reattach take the lock of a clone's sandbox through the path
of its lease, which they read under the lock, as a release requires. A sandbox
that holds a lease must not move: they refuse one whose lease names another
path, at which no sandbox holds the same lease, as the lease of a sandbox that
moved does, rather than take another sandbox there for it. They then reattach
to the clone with NVX's `reattach_clone`, in a new session of the adapter's
clone order, in which the guest agent answers the `Activate` of the clone's
record again, with the `Ready` that it answered once, and which they then use
as `reattach` attaches one. The reattach ends a clone that runs under another
runtime identity, without a word to its guest, or that fails the check of its
`Ready`, through its verified process identity, and finishes the record of an
earlier failure of the clone, whose teardown it resolves if it can. The
command then releases the lease of a clone whose VM ended without a failure,
and fails; ends as `start` does for a clone that failed; and keeps the lease
and the records, and fails, for a clone whose state cannot be determined. A
stop of a clone releases the lease once the store verified that the VM exited,
and the clone's records, which the release removes, told how the clone ended,
as `reattach_clone` reads them: a VM that had exited when the stop began ended
without its holder, as NVX's `Vmm::stop_unless_exited`, which then stops
nothing, tells by keeping the runtime record of its launch, and the stop ends
as the other commands do for a clone that they find so, while a VM that the
guest ended, once the stop began, with a status that is evidence of a failure,
such as its repair's, failed, which ends the stop as it ends `start`.
The commands release a lease through the template store that the clone's
record names, once the lease is of it. A sandbox whose start ended before it
recorded the clone holds a lease of a template store that no record names: it
records no clone, or an earlier clone's, which a release leaves, of another
template store perhaps. `reattach` and `stop` then release it with
`--template TEMPLATES`, through the template store TEMPLATES, in place of the
record's, which the release refuses unless the lease is of it.
A tombstone of the generation refuses new leases of it, and leaves those that
sandboxes hold to their clones, which reattach, stop, and release them as
before.

#### Running a clone

`runtime-adapter-host run` runs one execution in a clone of a generation, and
tears the clone down. Its preflight, which it reports with `preflight=staged`,
stages the clone's images in the sandbox's store, as `stage` stages them,
verifying the image, which hashes it only when a manifest is beside it, before
the clone's start, which hashes nothing. The command first reads the clone's
network, and plans the clone and the export of its shares, as `start` does,
then checks that the guest takes the execution's start, with the URL of the
proxy of the clone's network, if any, in its environment, as `exec` gives it to
an execution of a clone, so a network, an image, shares, or a start that it
would refuse fail the command before it stages anything. It then starts the
clone as `start` does,
at the host's time, or the one that `RUNTIME_ADAPTER_HOST_TIME` gives, and ends
as a start ends once the template store refuses the selection, or the clone
fails, with the record of the refusal or of the failure. Once the clone runs,
the command runs the execution in its root, the one that the clone's
activation assembled, through `host::Executions`, in the session of the
adapter's clone order in which the clone's sandbox started, as `exec` runs
one. The program, its arguments, and its environment reach the guest in the
execution's start, through the guest agent's execution control, never in the
template's boot tokens. The execution finds the clone's shares beneath the
root, since the restore sequence maps them before the clone's sandbox starts.
Then, whatever the execution did, the command stops the
clone as `stop` does, and releases the lease once the store verified that the
VM exited: a run leaves no clone running, and no lease held, unless its stop
timed out. The stop takes the same session, in which it skips the guest's
messages about the execution that may still arrive once the command finished
it without them, as when it gave up waiting for its outcome. It holds the
sandbox's lock, which the lease took, from the start to
the teardown, so no other command reattaches to its clone meanwhile. A run
whose execution failed to run, such as one whose session ended, still tears the
clone down, then fails, after the execution's diagnostic. Since the session may
have ended with the VM, as when its VMM crashed, the run first waits up to 10
seconds for the VMM to exit, as long as a stop waits for one that it
terminated: its stop then finds that the VM exited without it, and ends the
clone as a reattach that finds it so does, with the record of its failure, so
the run exits with 8, or 9 while the teardown is unresolved. It exits with 4
once the guest reported that a stage of the root failed, and with 1 otherwise.

### Commands

Each command of `runtime-adapter-host` but `stage`, `list`, `inspect`, and
`remove`, which need no artifacts, takes NVX's artifacts from the directory that
`NVX_ARTIFACTS_DIR` names, and the guest agent's build from
`--guest-build BUILD`, the crate's version by default:

- `stage STORE --distro FORMAT:PATH --distro-identity UUID [--upper scratch:PATH]`
  [stages](#staged-images) the images of a layered root for the launches of a
  VM in the sandbox store STORE, which it creates if needed: the image PATH,
  an EROFS file system, `erofs`, or a GPT disk, `gpt`, of which the guest
  expects the identity UUID, and the ext4 scratch image PATH, if given. It
  verifies and binds the images before it takes the sandbox's lock, which it
  holds while it records them, and prints nothing.
- `launch STORE --guest-agent PATH [--hypervisor NAME] [LAYERED]` launches a VM
  in the sandbox store STORE, which it creates if needed, with the guest agent
  PATH, a static `x86_64-unknown-linux-musl` build of `runtime-adapter-guest`,
  on the hypervisor NAME, or else on the one that
  `nvx::Hypervisor::from_env_or_default` selects. Once the launch has checked
  that no VM of the sandbox runs, it writes the VM's initramfs and its
  [record](#layered-roots) into the store. It connects to the VM's control
  endpoint with `nvx::host::session::connect`, attaches with the launch's
  capability, exchanges a `Ping` and the longest `Echo` with the guest agent,
  commits the launch, and exits, leaving the VM running. A launch that fails
  ends its VM.

  LAYERED,
  `--distro FORMAT:PATH --distro-identity UUID --upper (scratch:PATH | ram) [--runtime-dir DIR] [--overlay-mode MODE] [--scratch-journal POLICY] [--first-partition-bound BYTES] [--workload-identity UID:GID] [--network FILE] [SHARES]`,
  gives the VM a [layered root](#layered-roots). Its image is the file PATH, an
  EROFS file system, `erofs`, or a GPT disk, `gpt`, of which the guest expects
  the identity UUID: the file system's UUID, or the disk's GUID. Its upper is
  the ext4 scratch image PATH, or RAM. The guest assembles the root below its
  runtime directory DIR, `/run/runtime-adapter` by default, in the overlay mode
  MODE, `trusted-lowers` or `untrusted-lowers`, checks a scratch's journal
  against the policy POLICY, `allowed` or `forbidden`, which only a scratch
  takes, and refuses a first partition larger than BYTES, which only a GPT disk
  takes, with the guest's defaults unless given. A workload identity, the user
  ID UID and the group ID GID, two numbers from 1 to 4294967294, is the identity
  as which every execution of the sandbox runs: the guest checks its account in
  the assembled root, without which the root's `identity` stage fails. The VM
  has the [network](#networks) that FILE holds, if given, which the launch
  reads before anything else: one that the plan refuses fails the launch before
  it needs NVX's artifacts or creates the store. Before it spawns the VM, the
  launch checks the bindings that STORE stages for the image and the scratch
  image, and fails if STORE stages none for either, or one no longer holds, as
  [the checked handoff](#the-checked-handoff) describes. It records the
  bindings that it checked.

  SHARES,
  `--share HOST:TARGET[:ro]... [--deny PATH]... [--allow PATH]... [--reserved TARGET]... [--share-root DIR]...`,
  shares each host directory or regular file HOST with the root, at the
  absolute guest path TARGET, which starts at the last `:/`, read-only with
  `:ro`, as [Shares](#shares) describes. Each HOST must lie inside a host
  directory DIR that the adapter authorizes, the export hides each host path
  PATH of `--deny` inside the shares, but shows each host path PATH of
  `--allow` inside a hidden one again, and no TARGET may take or cover a guest
  tree that the container root keeps, or a tree TARGET that the adapter keeps.
  The launch refuses shares that it cannot export before it touches STORE, and
  checks the export before it spawns the VM.

  Once attached, the launch follows the [host's order](#the-hosts-order): it
  sends the root configuration, checks that the guest agent that assembled the
  root is of the build that the host program ships, sends the mapping table,
  then `Start`, and exchanges the `Ping` and the `Echo` once the guest started
  the sandbox. A guest agent of another build gets nothing more, and the launch
  fails, which ends the VM. If the guest reports that a stage of the root
  failed instead, the launch waits for the VM to end, abandons the launch, and
  checks that the VM ended with the stage's status. If the guest fails a
  mapping, the launch sends no more of the table, nor `Start`, stops the guest,
  which tears down the root, waits for the VM to end, abandons the launch, and
  checks that the VM ended with a success and a complete teardown.
- `create STORE --guest-agent PATH --distro FORMAT:PATH --distro-identity UUID [--hypervisor NAME] [--cpu-profile PROFILE] [--processors N] [--boot-processors B] [--memory MIB] [--memory-capacity MIB] [--scratch-length BYTES] [--network FILE]`
  serves a [template](#templates) of the guest agent PATH from the template
  store STORE, which it creates if needed: a generation of the template's
  class that the store holds, and that is eligible at the host's time, or else
  a new one, which the store captures and publishes. The placeholder is the
  file PATH, an EROFS file system, `erofs`, or a GPT disk, `gpt`, with the
  identity UUID. The template's VM runs on the hypervisor NAME, as a launch's
  does, with the CPU profile PROFILE, `auto` by default, as OpenVMM's
  `--cpu-profile` takes it, N processors, 1 by default, of which B are online
  at boot, 1 by default, the fewest that its clones take, MIB mebibytes of
  RAM, 256 by default, a RAM capacity of MIB mebibytes, its
  RAM by default, and a dormant scratch of BYTES bytes, 64 MiB by default. It
  has one NIC under the policy of the [network](#networks) FILE, unless the
  policy lets nothing through, and none without a network. The command reads
  the network before anything else, as a launch does, and checks the class
  before it creates the store, then prints the generation once the store has
  committed it, and [evicts](#eviction) the generations of the class that it
  supersedes.
- `refresh STORE --guest-agent PATH --distro FORMAT:PATH --distro-identity UUID [--hypervisor NAME] [--cpu-profile PROFILE] [--processors N] [--boot-processors B] [--memory MIB] [--memory-capacity MIB] [--scratch-length BYTES] [--network FILE]`
  takes the options of `create`, and captures a new generation of the
  template's class in STORE, whether or not the store holds an eligible one,
  then prints it once the store has committed it, and evicts the generations of
  the class that it supersedes, as `create` does.
- `list STORE` reports each generation of the template store STORE, in the
  order of their publication, from the store's records alone, as
  [Listings](#listings) describes.
- `inspect STORE GENERATION` reports the generation of STORE whose store
  generation is GENERATION, a positive number, as `list` does, with its leases
  and the metadata of its capture. It fails for a generation that the store
  does not hold.
- `remove STORE GENERATION` retires the generation GENERATION of STORE: it
  tombstones the generation, so that it takes no new lease, and deletes it once
  no lease of it remains. While leases remain, it reports them, and leaves the
  generation tombstoned, with its payload, which a later `remove`, or the next
  [eviction](#eviction) of its class, deletes once they drained. A generation
  that the store no longer holds is retired already, and one that it never
  published fails the removal.
- `reattach STORE [--template TEMPLATES]` takes the sandbox's lock, and
  reattaches to its VM, which
  another process launched, and which it holds, although the VM is not its
  child, through its verified process identity. It reads the capability from
  the store, attaches a new session, in the session's next epoch, exchanges a
  `Ping`, and exits, leaving the VM running. It reattaches to a
  [clone](#clones) in the session in which the clone answers the `Activate` of
  its record again, and releases the lease of a clone that ended, through the
  template store TEMPLATES, if given, in place of the one that the clone's
  record names.
- `start STORE --template TEMPLATES --generation G --distro FORMAT:PATH --distro-identity UUID --upper (scratch:PATH | ram) [--processors N] [--memory MIB] [--workload-identity UID:GID] [--overlay-mode MODE] [--scratch-journal POLICY] [--first-partition-bound BYTES] [--machine-id (image | generated)] [--network FILE] [SHARES]`
  starts a [clone](#clones) of the generation G of the template store
  TEMPLATES in the sandbox store STORE, which it creates if needed, and whose
  `stage` staged its images, which no one else may change: the image PATH, an
  EROFS file system, `erofs`, or a GPT disk, `gpt`, of which the guest expects
  the identity UUID, the file system's UUID or the disk's GUID, no longer than
  the template's placeholder, whatever the placeholder's format, over the
  ext4 scratch image PATH, or RAM, with a processor target of N processors, the
  template's processors online at boot by default, a memory target of MIB
  mebibytes, the template's RAM by default, the workload identity UID:GID, if
  given, as which every execution runs, and whose account the guest checks
  before the clone is
  ready, the overlay mode MODE and the scratch's journal policy POLICY, as a
  launch takes them, and for a GPT disk, the bound BYTES of its first
  partition, 64 KiB by default, as a launch's guest takes it. Every execution
  of the clone sees the hostname `runtime-adapter`, and the machine ID that
  `--machine-id` chooses: the image's own, `image`, by default, or one that the
  guest generates once for the clone, `generated`. The clone's VM has the
  [network](#networks) that FILE holds, if given, which the start reads before
  anything else, as a launch does, and which must be the template's: NVX's plan
  refuses any other before any VMM starts, as [Clones](#clones) describes.
  SHARES, as `launch` takes
  them, share host paths with the clone's root: the start refuses those that it
  cannot export before anything but its network, and NVX's plan those that the
  template's filesystem slot cannot bind, as [Clones](#clones) describes.
  Once the restore sequence sent the clone's mapping table, if it has one, and
  started the clone's sandbox, it exchanges a `Ping`
  and the longest `Echo` with the guest agent, commits the launch, and exits,
  leaving the VM running, and the sandbox holding the lease. A clone that
  fails, such as one whose activation failed before it was ready, and whose VM
  ended with the status of the failure's class, from 50 to 58, ends as
  [Clones](#clones) describes, with the record of its failure. A selection
  that the template store refuses, of no template, of an expired generation,
  or of an ineligible one, ends the start before it leases anything, with the
  refusal's record, as [Clones](#clones) describes.
- `stop STORE [--template TEMPLATES]` reattaches and attaches as `reattach`
  does, then stops the VM:
  the adapter's `Stop`, which the guest agent confirms with `Stopped` before it
  ends the VM, is the stop's graceful step. It waits for the VM to exit, and
  checks the VM's outcome report: a success, with status 0 and a complete
  teardown. A VM whose guest agent does not confirm the stop is terminated, and
  the stop fails. The worker of a layered root tears the root down before it
  confirms, and reports a teardown that failed first: the stop then checks
  that the VM ended with the teardown's status instead. A stop of a clone
  releases its lease once the VM exited, and the clone's records told how it
  ended, as [Clones](#clones) describes.
- `probe STORE PROBE...` reattaches and attaches as `reattach` does, then runs
  each PROBE in the VM's layered root, in order, and prints the facts that it
  found. A PROBE is the text of a [probe](#probes): its name, then its
  arguments, each after one space. A probe that fails fails the command.
- `exec STORE EXECUTION [; EXECUTION]...` reattaches and attaches as
  `reattach` does, then runs each EXECUTION in the VM's layered root, all at
  once, over that one session, through `host::Executions`, and prints their
  outputs and outcomes once every one finished. It runs at most 8 executions,
  as many as the guest runs at once. An EXECUTION,
  `[--identity UID:GID [--groups GID,...] [--capabilities N,...] [--numeric]] [--cwd DIR] [--env NAME=VALUE]... -- PROGRAM [ARG]...`,
  runs PROGRAM with its ARGs, up to a lone `;`, which a shell takes quoted, as
  `find -exec` takes them, without standard input. Without `--identity`, it
  names no identity, and runs as the sandbox's workload identity, which the
  guest otherwise refuses; with it, it runs as the user ID UID and the group ID
  GID, with the supplementary groups GIDs and the capabilities numbered N, such
  as 0 for `CAP_CHOWN`, if given, and the guest checks its account, unless
  `--numeric` says that the IDs have none. It starts in the root's directory
  DIR, or `/`, with exactly the environment of its `--env` entries, then, if
  the VM's [network](#networks) has a proxy, the proxy's URL as the guest sees
  it in `http_proxy`, `https_proxy`, `HTTP_PROXY`, and `HTTPS_PROXY`, unless
  the entries name any of them. Before it starts any execution, the command
  checks that the framing still carries each start with the proxy's URL, and
  runs none of the executions if one no longer fits. It runs for at most a minute, with 16 KiB
  chunks and the credit of 4 of them for its input and 8 for its output, which
  its standard output and error share. The command keeps at most 16 MiB of each
  stream, and cancels an execution whose stream would exceed them. Execution N,
  from 1, has the key `(B + N, E)`: E is the epoch of the command's attachment,
  and B the first 8 bytes of the broker instance's ID, as a little-endian
  number. The guest refuses a key that it heard of before, and the epochs of a
  broker instance only advance, while a broker restored from a snapshot has a
  new instance ID, so no command repeats the key of another's execution.
- `run STORE --template TEMPLATES --generation G --distro FORMAT:PATH --distro-identity UUID --upper (scratch:PATH | ram) [--processors N] [--memory MIB] [--workload-identity UID:GID] [--overlay-mode MODE] [--scratch-journal POLICY] [--first-partition-bound BYTES] [--machine-id (image | generated)] [--network FILE] [SHARES] EXECUTION`
  runs one EXECUTION, one of `exec`'s, in a [clone](#clones) of the generation
  G of the template store TEMPLATES, in the sandbox store STORE, which it
  creates if needed, then tears the clone down, as
  [running a clone](#running-a-clone) describes. The execution's options may
  come among the start's, in any order, and its PROGRAM and ARGs are every
  argument after `--`. It first plans the clone as `start` does, with its
  network and its shares, and checks the execution's start, with the URL of
  the network's proxy, if any. Its preflight then stages the clone's images in
  STORE, as `stage` stages them, and prints `preflight=staged`. It then starts
  the clone as `start` does, runs the execution in the clone's root as `exec`
  runs one, and prints its output and outcome once it finished. Then it stops
  the clone as `stop` does, whatever the execution did, which releases the
  lease once the VM exited.

`reattach`, `stop`, `probe`, and `exec` find a VM that booted cold under
another runtime identity mismatched, and then signal nothing: the VM keeps
running. They end a clone that runs under another, as [Clones](#clones)
describes. The commands print what they find as `KEY=VALUE` lines: `reattached`
(`live`, `mismatched`, `exited`, or `indeterminate`, and of a clone, `live`,
`ended`, `failed`, or `indeterminate`), `differences` (the parts of the runtime
identity in which a mismatched VM differs), `progress` (the step that a clone,
which ended, reached, from `planned`, `launching`, `spawned`, `event`,
`acknowledged`, `activating`, `ready`, `starting`, and `started` to
`committed`), `pid` (the VMM's process ID), `instance` and
`epoch` (the identity of the session that they attached to), `root` (`ready` or
`failed`: how the guest answered the root configuration, or `failed` when a stop
finds that the root's teardown failed), `build` (the build of the guest agent
that assembled the root), `stage` and `problem` (the stage of the root that
failed, such as `assembly`, and the problem, or the problem of a mapping that
failed), `table` (`applied` or `failed`: how the guest took the mapping table),
`mapped` (how many of the table's mappings the guest applied), `sandbox`
(`started` once the guest started the sandbox), `preflight` (`staged` once
`run` staged the images of its clone), `PROBE.KEY` (the fact KEY that
the probe named PROBE found), `exec.N.KEY` (what execution N did: its `launch`,
`started`, `failed STAGE ERRNO`, or `none`; its `status`, `exit CODE`,
`signal NUMBER`, or `none`; the `cause` for which a side ended it, such as
`timed-out`, or `none`; the streams that it `truncated`, such as
`stdout,stderr`, or `none`; whether it `settled`, `reusable` or `fenced`; and
its `stdout` and `stderr`), `stopped` (`graceful`, `forced`, or `timed-out`, or
`exited` once a clone's VM, which the command found running, had exited when
the stop began, which stopped nothing),
`outcome` (such as `success 0`, or `guest-exit 14` once the assembly failed),
`teardown` (`complete` or `incomplete`: whether the VMM's teardown of the VM
completed, which the outcome report records apart from the root's), `created`
(`captured` once a capture made the template's generation, as a refresh's
always does, or `existing` once the store held it), `generation` (the
generation's number in the template store, which `create` or `refresh` served,
`inspect` reports, or `start` or `run` leased), `key` and `class` (the identities of its
compatibility key and of its capture class, in hexadecimal), `retired` and
`draining` (the generations that the eviction of a creation or a refresh
retired: those of the class that are gone, with those whose interrupted
retirement it completed, and those that leases keep, each list separated by
commas, or `none`),
`lease` (the identity of the lease that `start` or `run` took, 32 hexadecimal
digits),
`downtime` and `source` (the downtime of the leased generation's capture at the
host's time, in nanoseconds, and the clock that measured it, `monotonic` or
`utc`), `released` (the identity of the lease that a stop, a start that failed,
or the teardown of a run released), `failure.KEY` (the record of a clone's
failure, or of the selection of a start or a run that the template store
refused: its
`outcome`, what the failure is attributed to, `request`, `staging`, `host`,
`unknown`, `template`, or `repair`, or `no-template`, `expired`, or
`ineligible` for a refused selection; its `evidence`, what the host saw of it,
which attributes it, such as `supplied-length`, or `no-store`, `unpublished`,
`retired`, `expired`, `tombstone`, or `early` for a refused selection; its
`stage`, where the clone
failed, `plan`, `launch`, `restore`, `activation`, `start`, `commit`, `run`, or
`reattach`, or `selection`; its `progress`, the step that the clone reached, as
`progress` spells it, or `none` for a refused selection; its `status`, the
status with which the guest ended the VM, as the
VM's outcome report gives it, or `none`; its `teardown`, how the teardown that
followed ended, `no-vm`, `exited`, `terminated`, or `unresolved`; and its
`code`, the code of the failure's error, such as `runtime-mismatch`),
`quarantined` (whether the command quarantined the generation of a clone that
failed: `yes`, for a failure of the template, or `no`), `store` (the identity
of the template store), `generations` (the store generations that `list`
found, or `none`), `generation.G.KEY` (what `list` found of the generation G,
with the KEYs `key`, `class`, `state`, `eligibility`, `reason`, and `leases`,
which `inspect` prints of its generation), `state` (`available`, or
`tombstoned` once a tombstone quarantines the generation), `eligibility` and
`reason` (`eligible`, `early`, or `expired`, at the host's time, and NVX's
reason, with the capture's downtime), `leases` (the identities of the
generation's leases, or `none`), `lease.ID.sandbox` and `lease.ID.launch` (the
sandbox that holds the lease ID, and the launch of its clone that the lease
reserves), `storage` (the capture's storage generation), `time.host`,
`time.boot`, `time.clock`, `time.utc`, `time.monotonic`, and `time.lineage`
(the capture's time contract: the identities of the host and of its boot, the
host's monotonic clock, the capture's anchor in UTC and by that clock, in
nanoseconds, and its time lineage counter), `length.state` and `length.memory`
(the lengths of the snapshot's saved state and of its RAM, in bytes), and
`removed` (`retired`, once the generation is gone, or `draining`, with the
leases that keep it). Values
escape `\`, newlines, and carriage returns as `\\`, `\n`, and `\r`, and each
byte of a program's output that is not part of UTF-8 text as `\xHH`. The
commands exit with 0 once they did what they were asked, `exec` once every
execution finished, whatever its outcome, and `run` once its execution finished
so, and the clone stopped as `stop` stops one, with 3 when they find a VM that
booted cold mismatched, with 4 when the guest reported that a stage of the root
failed, and the VM ended with the stage's status and a complete teardown, with
5 when the [step hook](#the-step-hook) ended a command, with 6 when the guest
failed a mapping of the table, and the VM stopped gracefully, with a success
and a complete teardown, with 7 when `remove` found leases that keep the
generation, which it tombstoned, with 8 when a clone failed, once nothing of it
runs and the command released its lease, with 9 when a clone failed, and its
teardown is unresolved, so that the sandbox keeps the lease, with 10 when the
template store refused the selection of a start or a run, so that it leased
and launched nothing, and with 1, after
a diagnostic on standard error, when they
fail, as `run` does once its execution failed to run, after its teardown,
unless the teardown ended with 4, 8, or 9, which `run` then exits with, after
the diagnostic. A
value of `RUNTIME_ADAPTER_HOST_TIME` that is not a host's time is a
usage error, as is a value of `RUNTIME_ADAPTER_STEP_HOOK` that sets no step
hook, or that sets one at another step than `checked` for a start, with
status 2. Without arguments, the program reports the contract that it was
built against.

## Tests

`tests/independent_build.rs` checks that every configuration builds as an
independent adapter's does:

- The adapter is a workspace of its own with its own lockfile, and declares
  the minimum supported Rust version of `nvx`.
- The protocol-only build depends on `nvx` alone, without features. The
  host-only build enables the host side of `nvx` and not its guest side. The
  guest-only build enables only the guest side, and pulls in neither Tokio nor
  a dependency that only the host side of `nvx` declares. The tests read
  these dependency trees from `cargo tree`, which downloads the packages of the
  musl target the first time, on Windows too.
- Each binary runs and reports the contract that it was built against, and the
  guest binary is a static executable: it has no program interpreter, needs no
  shared library, and keeps its stack non-executable.

The unit tests of `frame`, `root`, `probe`, `exec`, and `template` run in every
configuration. They cover each kind's encoding, messages in pieces of any size,
a message of each kind split at every byte, the headers that the framing refuses
before their body arrives, and the root configurations, readinesses, with or
without a clone's activation, parts of mapping tables, failures, probes, facts,
and messages of executions that decode, and those that don't, and the host's
side of an execution, which still sends the cancellation of one that it gives up
on in the step that ends it, without grace or settlement time, and which cancels
one whose stream outgrows the 16 MiB that it keeps. Those of `template` check
the platform tier's allowlist, token by
token, against the tokens that OpenVMM's capture of a platform snapshot allows
and refuses, through NVX's copy of OpenVMM's check, which refuses a token that
is not UTF-8 text, and admits by its prefix one that the kernel splits in two at
a vertical tab or at 0xa0, and template mode: a command
line in which the kernel finds no platform tier boots cold, whatever else it
holds, and one with it boots a template's VM if it holds the tier's tokens
alone, between any of the kernel's whitespace, and is refused otherwise, for a
parameter outside the allowlist at its start, in its middle, or at its end, or
within a platform token's value, after a vertical tab or the byte 0xa0, which
the kernel takes for whitespace and a split at ASCII whitespace does not, for a
platform token that the no-break space's first byte leaves without UTF-8 text,
for a double quote, and for `--`. In the host-only configuration, those of
`generations` check which generations an eviction retires: those of the served
generation's class that it supersedes, and, whatever their class, the
tombstones that interrupted retirements left alone, but not those of the
generations that the store published after the eviction read its index. In
the guest-only configuration, the unit tests of `worker::probes`
cover what the probes read: the mount table with its escapes, the kernel's
records of a process, `os-release`, and the guest's memory, and the probes and
arguments that the worker refuses. Those of
`worker::executions`, which spawn each program without entering a root, as a
fake root does, hand an execution the host's cancellation once its
timeout's instant passed, after its program ended before it, as a worker that
did not run meanwhile would, and check that the program's end stays its own.
Those of `worker` run PID 1's collection alone, in a run of the test binary of
its own, with a thread of each of two children that it traces, which stop
before the worker ends, one with a signal whose report `nix` decodes and one
with a real-time signal, and check that PID 1 takes both stops and goes on
until it collects the worker's status.
Those of `worker::entries` cover the names of capabilities, the fields of a
process's status, the outcomes of attempts, the entry's refusals, and the
probe's arguments. Those of `worker::capture` check that closing the worker's
fence acquires it, the status of each stage of a capture, which no other
outcome of the worker shares, a template's devices, the template's own root
configuration, and the inputs of a clone's root that it gives, the tag of the
host filesystem device among them, and that the
worker attaches its session, takes the broker's reset, and acknowledges it,
which the stream sent, before the capture runs, without reading anything more
of the session, and keeps the session, active in the broker's first epoch, and
that no capture runs once the attachment cannot be written, the broker closes
the stream or sends anything but a reset first, or the acknowledgement cannot
be sent. Those of `worker::activation` check the status of
each class of a clone's activation's failures, and that the activation's
timeout covers the wait for both block devices.

`tests/guest_session.rs` checks, in the guest-only configuration, that the
guest agent serves the control session with the adapter's own framing. A test
broker plays the broker's end of the guest's session over a Unix socket pair
with the public record API of `nvx`, for `guest::serve` and for
`runtime-adapter-guest serve`. It covers messages that span records both ways
and fill the receive window, a record whose bytes arrive one at a time, resets
in the middle of a message, with the next epoch of the instance and with a new
instance, streams that end at and inside a record, and the records and
messages that the guest agent refuses: sequence, epoch, and instance errors,
stale and identity-less resets, record lengths, bootstrap identity, ERROR
payloads, and malformed headers. It also checks that the guest agent of a VM
without a layered root answers probes with a failure, and refuses the messages
about a layered root, such as its configuration, and executions.

`tests/host_session.rs` checks, in the host-only configuration, that the host
program's client attaches to the control session and exchanges the adapter's
own framing with the guest agent. A test broker plays the broker's host-facing
end of the session, and the guest agent behind it, with the public record API
of `nvx`, over an in-memory transport, so that the tests run on Linux and
Windows. They cover:

- the attachment: its bootstrap record with the launch capability, waits for
  the guest, a deadline that passes meanwhile, and refusals, with an ERROR or
  by the end of the stream;
- messages that span records both ways, records that end a message and start
  the next, and records whose bytes arrive in pieces down to one byte;
- a receive that times out inside a message, then returns the whole message;
- a send that times out after it wrote part of a record, and a send whose
  transport fails, each of which ends the client with the message that it
  held;
- resets and disconnects in the middle of a message, with the next epoch of
  the instance and with a new instance, and streams that end inside a record;
- the records that the client refuses: sequence, epoch, and instance errors,
  stale and identity-less resets, record lengths, bootstrap identity, ERROR
  payloads, a payload beyond the session's limit, which fails at its header,
  and header faults;
- the messages that it refuses: malformed headers, which fail before their
  body arrives, and messages that only the host program sends, a clone's
  `Activate` among them; and a message beyond the framing's limit, which it
  refuses to send;
- the answers that its requests check, and a request whose answer arrives too
  late;
- a layered root: the configuration that the client sends, the guest's
  answers, ready with the guest agent's build or the stage that failed, probes
  and their facts, a stop that learns that the teardown failed, and answers
  whose bodies do not decode, which end the client;
- the host's order: the root configuration that announces the table, the
  guest's readiness, then each part of a table of no mapping, of one, and of
  several parts, once the guest applied the one before, then `Start`, also in
  records of 7 bytes; a guest agent of another build, and a root that failed,
  after which the host sends nothing more; a table that does not encode, of
  which the host sends nothing; a mapping that the guest fails at the first, a
  middle, or the last part, after which the host sends no more of the table,
  nor `Start`, but `Stop`, and learns how the stop ended, with or without a
  teardown that failed, or as the session ends; and `Maps` or `Start` answered
  with another kind, or failures whose bodies do not decode, which end the
  client;
- a clone's order: an attachment that waits through the session's bootstrap,
  the record as the first request, after which the host waits for `Ready`,
  whose activation it returns, also none, then the table in parts and `Start`;
  a session that a later reset, the end of the stream, or a failure of the
  transport ended, which the order takes for lost, and whose successor sends
  the same record first; and the failures that are final: a refusal in an
  ERROR, deadlines that pass while attaching or awaiting `Ready`, another
  build, after which the host sends nothing more, an `Activate` answered with
  another kind or with an activation that does not decode, and a record beyond
  the framing's limit, which fails before it is sent;
- the driver of executions, whose guest runs the runner of NVX's execution
  engine for each start, as the worker does, without a program: two
  executions at once, one with more output than its credit holds, each
  byte-exact, with its outcome, and one whose launch the guest refuses, whose
  records carry their messages whole or split; the starts that the guest would
  not decode, a second start of an ID, a start whose input the host would
  send, starts whose output windows count bytes or more than 32 frames, a
  start beyond 8 executions, and a start once the driver ran, of which the
  driver sends nothing, with a client that stays usable; a message about an
  execution that it did not start, about none, whose body does not decode, or
  that breaks the engine's rules, and the end of the stream, each of which ends
  the run; a timeout that fires while the guest says nothing, after which the
  execution is cancelled, then fenced; and a run whose deadline passes first,
  in a receive, which leaves the client usable, or in a send that waits for a
  guest that stopped reading, which ends it; and a stop, once the driver gave
  up on an execution, that skips the guest's late messages about it, before
  and after a teardown that failed, while `stop` takes the first for its
  answer, and neither skips a message about an execution that the driver did
  not start.

Through a relay between its test broker and a pseudo-terminal,
`tests/guest_session.rs` also checks that `guest::serve_terminal` and
`runtime-adapter-guest init` serve a tty in raw mode, with an `Echo` of every
byte value, and that `init` fails at once on a path that is no tty.

`tests/guest_session.rs` also runs the worker against a fake root, behind its
test broker: a root that the worker assembles, probes, and tears down on a
stop, across a reset of the session; an image over RAM, in the overlay mode of
the configuration; a GPT disk, with the configuration's bound of the first
partition; the host export that it mounts, the mapping table that it applies in
order, in parts, across a reset of the session, and the start; a table of zero
mappings, which does no work, with or without an export; the parts that do not
continue the table, which it refuses as a whole, and a mapping that the root
refuses, which latches the root, after either of which it refuses the later
parts and the start; the executions and starts that it refuses out of order,
before the start and after it; the failure of each stage, the tokens of a host
export that break their rules, mappings without an export, an export that fails
to mount, and a workload identity whose account the root lacks among them, which
the worker reports before it ends with the stage's status, without the
operations of the later stages; the account check of a sandbox's workload
identity, once the root is assembled, with the default hostname; a teardown
that fails; before the root, the configuration and the `Stop` that it accepts,
and the messages that it refuses, a clone's `Activate` among them; once the
root is ready, a second configuration, an `Activate`, and a broker that closes
the stream, which leave the root that it assembled to its operations;
and a broker that closes only its end of the stream while the worker probes,
which still receives the replies that the worker queued.
Through the binary, it checks that `runtime-adapter-guest init TTY layered`,
which is not PID 1 there, starts the worker, which fails at the layout stage on
a kernel command line without a layered root, and ends with its status, and
checks `worker`, `idle`, and the usage errors. Its PID 1 collects the status
of its unrelated child once a real-time signal ended it, even when the name of
its program, and so of its children, is no UTF-8, as that of a link to the
guest binary, and supervises the worker on, and ends with the status that a
shell reports for a worker that a real-time signal ended, alone or together
with the unrelated child. Each guest binary that a test starts runs in a
process group of its own, with the processes that it starts, and keeps its
process ID, and with it the group's, until the test has killed the group. A
binary that exits but leaves processes of its group running fails its test. A
test that fails, or gives up on the binary, kills the whole group, even once
the binary exited, so that none of them outlives the test or holds its output
open, which tests check too.

`tests/guest_session.rs` also runs the worker's executions behind its test
broker, whose host side starts the sandbox, then drives each execution's
`exec::HostExecution`, with frame-counted credit and one output pool. The fake
root prepares each execution's entry, as the worker describes it, in a directory
of the tests' own whose account databases hold the accounts that the tests name,
and then spawns the program without entering a root, which needs privileges, in
the tests' own root and identity. Unless a test says otherwise, the sandbox has
a workload identity, as which every execution runs. Where the tests can create
cgroups, as root in a cgroup v2 hierarchy with the `pids` controller, the fake
root gives the worker a workload cgroup of the test's own, below which each
execution runs in a cgroup of its own, so that an execution whose program ends
on its own settles, and is reusable. Elsewhere, the fake root names no cgroups,
and each program runs in a process group alone, so that such an execution is
fenced. The tests cover:

- three executions at once, each byte-exact: standard output beyond a MiB, 1.5
  MiB of input through `cat`, and both streams of a program that fails;
- a sink of the host that stalls, and so stalls only its own execution, while
  another one streams to its end, a timeout ends a third one in time, and a
  ping is answered; the stalled execution's cancellation then ends it within
  its grace, with its output truncated;
- a probe while an execution runs, which the worker refuses, so that the
  execution's timeout ends it in time, and runs once the execution ended;
- a stream to the host that fills up while four executions write without end,
  behind which a cancellation still ends its execution's program, and a stop
  the others, before the stream takes anything again;
- two executions that write without end, each with more credit than the
  worker queues for the stream, which take turns at the stream;
- the starts that the worker refuses: beyond its limit of executions, and
  beyond its memory for one, and a program that it cannot find;
- the messages about executions that end the session: before the root, a body
  that does not decode, a second start, credit that the engine refuses, credit
  of the input from the host, and a message that only the guest sends;
- the generations that the worker remembers: a start of an earlier generation
  after the later one ended, and after the worker refused a later one, which
  are stale, and a second start of a generation that ended or that the worker
  refused, which ends the session;
- a later generation and a reset of the session, which abandon executions
  whose programs end, and a stale message of an abandoned generation;
- a stop that ends the running executions and collects their programs'
  statuses, and a program whose status the worker collects once it ended,
  after its execution's runner gave up on it;
- programs that end while a descendant holds their output: the worker ends
  the descendant, through the execution's cgroup or the program's process
  group, so that the output ends at once, losing nothing, and a stop once the
  worker collected such a program's status leaves no process of its group
  running;
- with cgroups, a descendant in a session of its own, which holds a program's
  output once the program ended, and which the execution's settlement ends, so
  that the output ends at once, losing nothing, and the execution settles;
  the worker removed the execution's cgroup before it sent the outcome, and
  runs its limit of executions at once again;
- with cgroups, an execution that does not settle within its settlement time,
  because a zombie whose parent is outside its cgroup occupies the cgroup: it
  keeps its place, so that the worker runs one execution fewer than its limit,
  even once the cgroup emptied, until the host stops the worker, which then
  removes the cgroup;
- a program that a real-time signal ends, whose status the worker collects;
- a program that runs in the working directory of its start, with exactly the
  environment of its start;
- the identities that starts name in a sandbox without a workload identity,
  with their supplementary groups, capabilities, and account modes, and the
  default hostname, all of which the worker's entry takes;
- the publication of listeners that the root configuration names, published
  without it, with which every execution enters, and the account check of a
  sandbox's workload identity too: while listeners are published, the entry
  refuses `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` at stage 0, with `EPERM`,
  and while they are not, it takes them, as it takes the other capabilities
  of its allowlist either way;
- the starts that the worker refuses at stage 0 for their identities: one that
  names an identity in a sandbox with a workload identity, or none in a
  sandbox without one, with `EINVAL`; a capability outside the entry's
  allowlist, one of a user ID other than 0, and `CAP_NET_BIND_SERVICE` and
  `CAP_NET_RAW` while listeners are published, with `EPERM`; an identity
  without a passwd entry, or whose primary group has no group entry, with
  `ENOENT`; and one whose passwd entry has another primary group, or whose
  user ID the kernel reads as "unchanged", with `EINVAL`. None of them takes a
  place, so the worker then runs its limit of executions at once;
- an execution whose start comes in the payload of the root's configuration
  and the sandbox's start, whose timeout counts from its own start, however
  long the root's assembly takes;
- executions on a tty in raw mode.

A test of the worker's executions that fails before it stops the worker, even
while its broker pauses, ends the worker's session as it drops its host, so
that the worker drops its executions, and their programs end, which a test
checks too.

On x86_64, `tests/guest_session.rs` also runs the worker of a clone, which a
restore of a template's snapshot started, with `worker::work_clone`, behind its
test broker, with the session that the template's VM attached, active in the
epoch of its broker's first reset, and the clone's activation,
`nvx::guest::clone::Activation::with_resources`, behind a closed fence. The fake
root applies each record as `nvx::guest::clone::apply` does, without a root: for
a GPT disk, it stands for the kernel's reread of the disk's partition table,
which a test may fail; it takes as long as a test asks, but no longer than until
the activation's deadline, as the waits for the devices do, checks the account
of the record's workload identity in the tests' directory of account databases,
and stands for the generation of a machine ID that the record asks for, the
mount of the record's export, the write of the resolver file that names its
network's DNS server, and the setting of the record's hostname and machine ID
for every execution, each of which a test may fail. The tests cover:

- the reset that the restore queued, of another instance, which the worker
  acknowledges without an attachment, and the `Activate`, whose record it
  applies once, with the template's inputs, and answers with the `Ready` that
  reports the activation;
- retries of the `Activate`, in records of 7 bytes, after a later reset, and
  once the sandbox started, which get the same `Ready` without a second
  application, and the order that the worker serves once the clone is ready,
  as after a cold boot;
- the messages before the `Activate`, a stop and a ping among them, and another
  reset, which fail the activation with the status of the host's session, with
  nothing answered or applied;
- a record that does not decode, a cold boot's, and one that expects other
  resources, which fail the activation with nothing applied, and a GPT disk
  whose partition table the kernel does not reread, a workload identity whose
  account or group the image lacks, a machine ID that cannot be generated, a
  hostname or machine ID that cannot be set, an export that cannot be mounted,
  and a resolver file that cannot be written, which fail its application, each
  with the status of its class, and no `Ready`;
- an `Activate` whose header announces more than 1 KiB, which the framing
  refuses before it takes any byte of its body, and which fails the activation
  with the record's status, before the clone is ready and once it is;
- the activation's deadline, whether the restore's reset came or not, or part
  of an `Activate`, which the broker sends before the worker runs, so that the
  worker takes it at once, however late it runs: the deadline fails the
  activation with nothing applied, and ends nothing once the clone is ready;
- an application that would outlast the activation's deadline, which gets that
  deadline, and which ends the clone with the deadline's status once the
  deadline passed, with no `Ready`;
- a reset, a request, and a retry of the `Activate` that the host sends while
  the application runs, which the worker takes before it completes the
  activation: the reset and the request fail it, with the status of the host's
  session and no `Ready`, and the retry joins it, so that both get `Ready`,
  after one application;
- a stream that takes nothing more from the worker, as once the broker failed,
  after which the worker still ends once its activation's deadline passed,
  with the deadline's status, or once the broker closed its end of the stream
  before the clone was ready, with status 1, once it waited for the stream for
  `worker::UNACTIVATED_DRAIN`;
- a broker that closes the stream before the clone is ready, whether the
  restore's reset came or not, or part of an `Activate`, which ends the clone
  with status 1, with nothing applied, and once the clone is ready, which ends
  it as it ends the worker of a VM that boots cold;
- a broker that sent the restore's reset, and a ping or nothing more, and is
  gone before the worker runs, so that the worker cannot write its
  acknowledgement: the worker still ends with the activation's status, or with
  status 1, rather than with the failure of its stream;
- an `Activate` of another transaction, or of the same transaction with
  another record, once the clone is ready, or its sandbox started, which fails
  the activation with the status of the host's session;
- the record's workload identity, as which every execution runs, while a start
  that names an identity is refused with `EINVAL`, as is a start that names
  none once the record has none, and the record's hostname, which every
  execution's entry takes;
- the `activation` probe, which the worker answers itself, with no root
  operation, executions running or not: the restore's reset, the host's first
  message, the clone's `Ready`, and its start, in that order, after the
  repair's acknowledgement, and the start of the first execution after them;
- the mapping table of the record's export, which the host sends in parts once
  the clone is ready, and which the worker applies in order, from the export
  that the application mounted, before it starts the sandbox, and the endpoint
  of the network's proxy, which the worker leaves to the host: an execution's
  environment is exactly its start's.

`tests/clone_start.rs` checks, in the host-only configuration, on Linux and
Windows, without a hypervisor:

- the command lines of `start` that the host program refuses, each with status
  2: missing options, a generation that is not a positive number, a scratch's
  journal policy for an upper in RAM, a first-partition bound for an EROFS
  image, invalid values, an option given twice, a share's options without a
  share, and the options of other commands, and the options of `start` that
  other commands do not take, with a clock fixture that is not a host's time
  too;
- a start whose template store does not exist, or does not hold the
  generation, each with status 10 and the record of a selection that names no
  template, and one whose image the plan refuses, or whose shares it cannot
  export, with status 1, each before it needs NVX's artifacts or creates the
  sandbox's store, and the shares before it reads the template store;
- a start whose network cannot be read, or is one that the plan refuses, with
  status 1 before anything else: before its image, which does not exist, its
  shares, which it cannot export, and the template store, which does not exist;
- the command lines of `run` that the host program refuses, each with status
  2: missing options, a missing `--` or program, the qualifiers of an identity
  without one, an execution's start that the guest would refuse, a
  first-partition bound for an EROFS image, a share's options without a share,
  invalid values, an option given twice, arguments before `--`, and the options
  of other commands, with a clock fixture that is not a host's time too;
- a run whose network cannot be read, or is one that the plan refuses, or whose
  shares it cannot export, with status 1, before it stages anything or creates
  the sandbox's store;
- a run whose image the plan refuses, with status 1, before it stages anything
  or creates the sandbox's store, and runs whose template store does not exist,
  or does not hold the generation, each with status 10 and the preflight's
  `staged` before the record of a selection that names no template, once the
  preflight staged the clone's images, before it needs NVX's artifacts, and
  without a lease;
- the text form of a host's time, which `RUNTIME_ADAPTER_HOST_TIME` takes,
  written and read back on both clocks, and the texts that are none;
- a clone's record: its JSON, with the plan that `layered::LaunchRecord`
  reads back, the template store and the generation that a stop reads, a
  launch's record that is no clone's, and records that a reader refuses;
- a clone's network: the plan of a request with a proxy-only network, NVX's
  request of the clone, whose plan names the proxy's URL that NVX's activation
  record of the clone takes, and the clone's record, from whose plan an
  execution finds that URL in each of the proxy's variables, after its own
  entries; and an open network, and none, with no proxy's settings;
- NVX's request of a clone, with the staged bindings, the processor and memory
  targets, and the guest's defaults or the request's settings, the images that
  the sandbox's store does not stage, which it refuses, and the export of the
  request's shares: none without shares, and with them, one directory, the
  root of a read-only share, holding a read-write one, mapped from an empty
  source and from its path.

`tests/initramfs.rs` and `tests/runtime.rs` check, in the host-only
configuration, on Linux and Windows:

- the archive that puts the guest agent in place of NVX's agents: its entries,
  their owners and modes, both launchers, and the padding of its headers and
  data, for guest agents of every length modulo 4;
- the copy of NVX's initramfs, which stays as it is, of every length modulo 4,
  with the archive after it at a multiple of 4 bytes, and the files that a
  failure names;
- the runtime identity of a checkout and of a release, which differs from
  NVX's in the initramfs alone, another build of the guest agent, which differs
  in the initramfs too, and missing install metadata, which fails as NVX
  reports it.

`tests/staged_images.rs` checks, in the host-only configuration, on Linux and
Windows:

- staging: an image without a manifest, one whose manifest matches it, and
  manifests that name another image or digest, which fail; a GPT disk over
  RAM; and images that no launch could attach;
- the bindings that a launch takes from the staged images, and those of other
  images, which it does not;
- the record of the staged images, as written and read, and the forms that a
  reader refuses;
- `runtime-adapter-host stage`, which records the staged images, a staging that
  fails, which leaves them, and a staging that replaces them.

`tests/layered_launch.rs` checks, in the host-only configuration, on Linux and
Windows:

- the adapter's plan of a layered launch: its block devices and boot tokens,
  with none of the sandbox profile's settings, and the sizes of the images;
  and the options that it passes on as they are, for the guest to check;
- the checked handoff: OpenVMM's arguments, which name the checked files, on
  Linux by the descriptors that OpenVMM inherits and on Windows by their final
  paths, with no workload identity and no managed lifecycle, and what OpenVMM
  inherits, over a scratch image and over RAM;
- the launches that the checks refuse: of images that the store does not
  stage, of another format or identity than the staged image's, and of an
  image that changed, a scratch image that another file replaced, and an image
  that is gone, after they were staged;
- the scratch image's claim, which a second launch waits for, then fails to
  take, until the first one releases its checked files;
- the record of a launch, with the bindings that it checked, over a scratch
  image and over RAM, and of a VM without a layered root;
- shares: their text, `HOST:TARGET[:ro]`; the export of a table of shares
  inside an authorized directory, which maps each at its target, parents
  first, hides a denied path, and narrows the writes of a read-only directory
  to the read-write share inside it, and an export that shows an allowed path
  inside a hidden one again; the root configuration that announces the
  table, OpenVMM's arguments that attach the export, a launch that shares
  nothing, the export that the launch verifies before it prepares anything,
  and a table that the guest would refuse;
- the shares that a launch refuses: outside the authorized directories, also
  through a link on Linux, without any, with an authorized directory that is
  not one, or a share that is not accessible; reserved trees that are not
  canonical guest paths, targets that take or cover the adapter's or the
  container root's reserved targets, a read-only share inside a read-write
  one, a hidden path that does not exist inside a share, and an allowed path
  that lies inside no hidden one;
- the networks that a launch reads: the OpenVMM arguments of each element of a
  policy, which NVX's lowering renders after the plan's block devices, a policy
  that lets nothing through, which attaches no network device, the proxy's
  URL, the publication of listeners that the root configuration names for
  each, and the record, which keeps the network as the file gave it; and the
  files that it refuses, each for the plan's reason;
- the URL of a network's proxy in an execution's environment, as the guest
  sees it, with and without a scheme, after the start's own entries, which the
  start carries; a start that names its own proxy settings, which keeps them;
  and a launch without a proxy, which leaves the environment as it is;
- the plan that the record of a launch gives back, from which the host finds
  the proxy's URL, none for a VM without a layered root, and the records that
  it refuses;
- `runtime-adapter-host launch` with shares that it cannot export, which it
  refuses with status 1 before it creates the store, with artifacts that boot
  nothing;
- `runtime-adapter-host launch --network`, which refuses a network that the
  plan refuses, or that it cannot read, with status 1 before it needs NVX's
  artifacts or creates the store, and passes one that the plan takes;
- the command lines that the host program refuses, each with status 2, those of
  `exec`, of `--workload-identity`, and of `create` included.

`tests/template_capture.rs` checks, in the host-only configuration, on Linux
and Windows:

- the capture that `capture::Request::capture` makes of a request: the
  adapter's plan of the placeholder, as the host program stages it, with the
  request's network, over RAM, which no template keeps; the placeholder's
  binding, the policy revision, and the artifacts, with the initramfs that it
  names; and the class, with the runtime identity of the adapter's VMs and the
  guest agent's build, the backend, the `auto` CPU profile, the processors and
  those online at boot, which the request names, 1 by default, whatever the
  capacity, and which name the class, the RAM and its capacity, the geometries
  that OpenVMM presents, of 512-byte logical blocks, and the network, which
  names the class: a class without it is another;
- `runtime-adapter-host create` with a network that the plan refuses, or that
  it cannot read, which it refuses with status 1 before it needs NVX's
  artifacts or creates the store, and with one that the plan takes, or none,
  which goes on to need NVX's artifacts.

`tests/template_operations.rs` checks the template operations, in the
host-only configuration, on Linux and Windows, without a hypervisor:

- `list`, `inspect`, and `remove` of a directory that does not exist, and of a
  private one that holds no template store, which each refuses with status 1,
  and creates nothing;
- `list` of a template store without generations, which prints the store and
  none, and `inspect` and `remove` of a generation that it never published,
  which fail with status 1, and tombstone nothing;
- `refresh` with a network that the plan refuses, which it refuses with status
  1 before it needs NVX's artifacts or creates the store, as `create` does, and
  with one that the plan takes, or none, which goes on to need them;
- the command lines of `refresh`, `list`, `inspect`, and `remove` that the host
  program refuses, each with status 2: a missing store or generation, a
  generation that is not a positive 64-bit number, and arguments or options
  that the command does not take.

`tests/step_hook.rs` checks, in the host-only configuration, on Linux and
Windows, the [step hook](#the-step-hook): the values of
`RUNTIME_ADAPTER_STEP_HOOK` that set it, with each step and action, a path with
colons, and those that set none, which the host program refuses with status
2 for `launch`, `reattach`, and `stop`; a start's, which the host program takes
at `checked`, and refuses at the other steps with status 2; and a pause, which
does nothing, and pauses for no time, at another step, creates its file with
its step's name at its own, waits until the file is gone, on Windows until a
pending deletion of the file completes, and says how long it paused, and which
a file that is
there already fails.

`tests/vm_steps.rs` checks the step hook on a real hypervisor, in the
host-only configuration, with NVX's Ubuntu EROFS image over a copy of the
scratch template:

- a launch paused at `checked`, while files of zeros, which the guest could
  neither assemble nor mount, try to replace the image and the scratch image
  at their paths: on Linux, they replace them, and the guest still assembles
  the checked originals, and writes nothing to the replacements; on Windows,
  the checked files keep the replacements from the paths, and keep the images
  from being deleted or the image from being opened for writing, until the
  guest is ready, after which the images can be renamed again;
- a launch that the hook ends at `checked`, with status 5, which leaves nothing
  of a launch in the store, and whose exit released the scratch image's claim,
  since the next launch takes it;
- a launch paused at each of its steps in turn, at which the store holds what
  the step says, and the scratch image's claim is held at `checked`, and which
  then goes on, and is stopped.

`tests/vm_restart.rs` checks the restart of an orchestrator, on a real
hypervisor, in the host-only configuration:

- `runtime-adapter-host stage` stages NVX's Ubuntu EROFS image over a copy of
  the scratch template, and `runtime-adapter-host launch` launches a VM with
  them as its layered root, whose guest agent, of the build that the host
  program ships, starts the sandbox, and exits;
- another process's `reattach` holds the VM, which is not its child, and
  attaches a new session, in a later epoch of the same broker instance;
- a third process's `stop` stops the VM: gracefully, with a successful outcome,
  and with no runtime record left.

In another test, a host program that ships another build of the guest agent
finds the VM mismatched in its initramfs, and signals nothing: the VM runs on,
under the same runtime record, until the host program of the right runtime
stops it. In a third, a launch of a host program that ships another build than
the one that it boots finds that build in the guest's readiness, sends it
neither the mapping table nor `Start`, and fails, which ends the VM, whose
scratch image the next launch claims.

### Layered scenarios

`tests/vm_layered.rs` launches VMs with a layered root on a real hypervisor, in
the host-only configuration, through the host program's commands, and checks
that the guest's worker, outside PID 1, assembles the root, or reports the
stage that failed, and that the VM still ends cleanly. A launch whose guest
assembled the root also follows the host's order: the guest agent is of the
build that the host program ships, applies the mapping table, which is empty
unless the scenario shares host paths, and starts the sandbox. Each scenario
stages its images, then launches them, and is a row of the table that its
`scenarios!` macro declares, and an ignored test of its own:

| Scenario | Root | Expects |
| --- | --- | --- |
| `erofs_over_scratch` | NVX's Ubuntu EROFS image, with its own identity, over a copy of the scratch template | A root that the worker assembled: the image, read-only, under the overlay, whose upper and work directories are on the scratch, each mounted once below the runtime directory, with the options of the default mode, which trusts its lowers; the merged root's contents, with a file that reaches the upper and not the image; a copy-up of a file's metadata alone, which leaves its data in the image, and the rename of a lower directory; the image's and the scratch's disks; the agent's and the workload's cgroups, with their resource values; a worker that is PID 1's child, and PID 1's unrelated child, untouched; an empty mapping table, which does no work: the launch exports nothing, the guest mounts no export, and nothing is mounted below the root; and a teardown that unmounts the scratch cleanly |
| `erofs_over_scratch_with_untrusted_lowers` | The image over the scratch, in the untrusted-lowers mode, with a journal that the launch allows | The root, whose overlay has none of the trusting mode's options, copies up a file's data too, and refuses the rename of a lower directory with `EXDEV` |
| `erofs_over_ram` | The image over a tmpfs in RAM | The root, whose upper and work directories are on a tmpfs of the kernel's default size, whose root directory has mode 0755, with no scratch mounted, and no disk but the image's |
| `erofs_over_ram_with_untrusted_lowers` | The image over a tmpfs, in the untrusted-lowers mode | The root over the tmpfs, which copies up a file's data too, and refuses the rename |
| `gpt_over_scratch` | A GPT disk that the scenario writes, with its own GUID, over a copy of the scratch template | A root that the worker assembled: the first partition's bytes, as the disk holds them without their padding; each layer, an ext4 file system, mounted once, `ro,nosuid,nodev` with journal loading suppressed (`noload`, reported as `norecovery` by the kernel), from its partition; the overlay, with partition 2's layer at its base and partition 3's at its top, in the default mode, which copies up a file's metadata alone and renames a lower directory; a file that reaches the upper and not the top layer; and the disk's and the scratch's disks |
| `gpt_over_scratch_with_untrusted_lowers` | The disk over the scratch, in the untrusted-lowers mode | The root, which copies up a file's data too, and refuses the rename |
| `gpt_over_ram` | The disk over a tmpfs, with a bound that the first partition meets exactly | The root over the tmpfs, with no scratch mounted, and no disk but the GPT disk's |
| `gpt_over_ram_with_untrusted_lowers` | The disk over a tmpfs, in the untrusted-lowers mode | The root over the tmpfs, which copies up a file's data too, and refuses the rename |
| `full_scratch_refuses_writes` | The image over the scratch | A file that fills the scratch, through the root, until a write fails with `ENOSPC`, and a write that succeeds once the file is gone |
| `full_ram_upper_refuses_writes` | The image over a tmpfs | A file that fills the tmpfs, half of the guest's memory, until a write fails with `ENOSPC`, and a write that succeeds once the file is gone |
| `forbidden_journal_fails_the_assembly` | The image over the scratch, whose journal the launch forbids | The assembly's failure, which the launch reports, and a VM that ends with 14 before the guest mounts the scratch |
| `other_identity_fails_the_assembly` | The image, of which the launch expects another identity | The assembly's failure, which the launch reports, and a VM that ends with 14 before the guest mounts the scratch |
| `other_disk_guid_fails_the_assembly` | The GPT disk, of which the launch expects another GUID | The assembly's failure, which the launch reports, and a VM that ends with 14 before the guest mounts the scratch |
| `first_partition_over_its_bound_fails_the_assembly` | The GPT disk, with a bound one byte smaller than its first partition | The assembly's failure, which the launch reports, and a VM that ends with 14 before the guest mounts the scratch |
| `corrupt_layer_fails_the_assembly` | The GPT disk, whose top layer is corrupt and asks the kernel to panic on an error | The top layer's mount, which the kernel refuses without a panic, the assembly's failure, which the launch reports, and a VM that ends with 14 before the guest mounts the scratch |
| `cyclic_journal_is_not_loaded` | The GPT disk over a tmpfs, in the untrusted-lowers mode, whose top layer has a cyclic journal but no `needs_recovery` bit | Assembly, reads, copy-up, and teardown complete without scanning the journal |
| `recovery_required_layer_fails_the_assembly` | The same cyclic journal with the top layer's `needs_recovery` bit set | The assembly refuses recovery before mounting anything, despite `noload`, and the VM ends cleanly with 14 |
| `invalid_configuration_fails_its_stage` | A relative runtime directory | The configuration's failure, which the launch reports, and a VM that ends with 11 |
| `busy_root_fails_the_teardown` | The image over the scratch, which the `hold` probe keeps open | The teardown's failure, which the stop reports, and a VM that ends with 15, leaving the scratch mounted |
| `erofs_over_scratch_maps_the_table` | The image over the scratch, with the mapping table, from whose export the `map-checks` probe also maps | The export, mounted once, read-write, `nosuid`, and `nodev`, below a `hostfs` of mode 0700 that root owns; the table's mappings, mounted in the plan's order, by the depth of their targets, then by their targets, each with its access, `nosuid`, and `nodev`, on targets that the worker created on the overlay, but none inside another mapping; a hidden file that its directory does not list, and whose lookup the host refuses; and writes that reach the host's writable sources alone, the one inside a read-only mapping included. Through mappers of the adapter's own, from the export: a link or a magic link at each component, the wrong kinds, a target that would be created inside another mapping, and the runtime's, the container root's, and the adapter's own reserved targets refused, and a read-only mapping of a directory that the guest may write through the export, which is never writable. A teardown that unmounts the mappings, the export, then the scratch, cleanly |
| `erofs_over_ram_maps_the_table` | The image over a tmpfs, with the mapping table | The table, as over the scratch |
| `gpt_over_scratch_maps_the_table` | The GPT disk over the scratch, with the mapping table | The table, whose targets the worker created with their parents, which the disk's layers do not hold |
| `gpt_over_ram_maps_the_table` | The disk over a tmpfs, with the mapping table | The table, as over the scratch |
| `a_failed_mapping_tears_the_vm_down` | The image over the scratch, with the mapping table and `out` again at `/usr/lib/os-release`, a file of the image, which the plan orders fourth | The guest's refusal of the fourth mapping, a directory onto a file, which the launch reports with the three mappings that the guest applied; no more of the table, and no `Start`; a stop that the guest confirms once it tore the root down, a VM that ends with success and a complete teardown, an exit with 6, no runtime record, and the host's sources unchanged |
| `a_read_only_share_inside_a_writable_one_is_refused_before_launch` | The image over the scratch, with the mapping table and a read-only share inside `out` | A launch that fails before it spawns the VM, since the host could not keep the share read-only, and that leaves nothing of a launch in the store |
| `a_share_inside_a_reserved_tree_is_refused_before_launch` | The image over the scratch, with the mapping table, a tree `/srv/adapter` that the adapter reserves, and a share inside it | A launch that fails before it spawns the VM, and that leaves nothing of a launch in the store |
| `a_share_over_a_reserved_tree_is_refused_before_launch` | The image over the scratch, with the mapping table, the reserved tree, and a share at `/srv`, which would cover it | A launch that fails before it spawns the VM, and that leaves nothing of a launch in the store |
| `an_unauthorized_host_path_is_refused_before_launch` | The image over the scratch, with the mapping table and a share of a host directory beside the sources | A launch that fails before it spawns the VM, since the adapter authorizes the sources alone, and that leaves nothing of a launch in the store |
| `root_profiles_enter_the_root` | The image over the scratch, which the `entries` probe enters | Executions that start as PID 1 of their namespaces, with the root moved over `/`, only their standard streams, the leaked descriptor closed, no inheritable or ambient capability, and no new privileges, blocking no signal that their caller blocks, at the top of the root unless they name a working directory. A working directory that the execution names is entered, but none through a magic link, such as the leaked descriptor's, its own working directory's or root's, or any descriptor's, nor through a symbolic link to one: each fails the launch before the program runs. User ID 0 with each allowed capability alone holds it alone, and only its own attempt succeeds; with none, nothing does, nor for a numeric identity without an account. No execution writes `/proc/sys`, so none lowers the unprivileged port start, and a nested `chroot` stays in the root. With published listeners, user ID 0 with every other allowed capability can neither bind a privileged source port nor open a raw socket, so it cannot forge a listener announcement in either publication mode; without them, it may. The entry refuses the 51 capabilities outside its allowlist, the binding and raw-socket ones while listeners are published, capabilities for another user ID, and an identity without an account in existing-account mode. The root's own machine ID, then a generated one and the time status that the guest's time daemon publishes, read-only, in which a root workload finds no lock of the daemon's, which the runtime bound in the helper's mount namespace, and which leave the worker's namespace without mounts below the root. A teardown that unmounts the scratch cleanly |
| `unstaged_images_are_refused_before_spawn` | The image over the scratch, which the scenario does not stage | A launch that fails before it spawns the VM, since the store stages no image at its path, and that leaves no launch journal, VMM log, runtime record, or launch record; then, once the scenario stages the images, the root that the worker assembles, and a clean teardown |
| `a_changed_image_is_refused_before_spawn` | The GPT disk over the scratch, which the scenario writes in place after it stages it | A launch that fails before it spawns the VM, since the disk's modification time differs from its binding's, and that leaves nothing of a launch in the store; then, once the scenario stages the disk again, the root, and a clean teardown |
| `a_replaced_scratch_is_refused_before_spawn` | The image over the scratch, which another copy of the template replaces after the scenario stages it | A launch that fails before it spawns the VM, since the scratch image's identity differs from its binding's, and that leaves nothing of a launch in the store; then, once the scenario stages the images again, the root, and a clean teardown |
| `executions_run_in_the_root` | The image over the scratch, without a workload identity, in which `exec` runs executions that name their identities | Over one session, four executions as `nobody` that meet only while all of them run, beside one as root that writes more than a MiB to its standard output and a little to its standard error, each byte-exact, ending on its own with 0, and reusable. Over a later session, with new keys: a program that leaves a descendant in a session of its own, which the program's end, as PID 1 of its namespace, ends, so that no output is lost; `nobody`'s IDs and groups, with empty permitted, effective, and bounding sets; root with `CAP_CHOWN` alone in each of them; exactly the start's environment; the start's working directory; the entry's default hostname, `nvx-sandbox`, and PID 1; and a start without an identity, and one with `CAP_SYS_ADMIN`, which the guest refuses at stage 0 with `EINVAL` and `EPERM`, fenced and without output |
| `executions_run_as_the_workload_identity` | The image over the scratch, with the workload identity of `nobody` | Executions that name no identity, which run as `nobody`, without capabilities, with exactly the start's environment, and a start that names an identity, which the guest refuses at stage 0 with `EINVAL` |
| `accountless_workload_identity_fails_its_stage` | The image over the scratch, with a workload identity whose account the image lacks | The identity stage's failure once the guest assembled the root, which the launch reports, and a VM that ends with 16, which unmounted the scratch |
| `executions_take_the_settings_of_a_proxy_only_network` | The image over the scratch, in a proxy-only network, whose proxy is the test proxy on host loopback | A root configuration whose listeners are unpublished; executions that find the proxy's URL, `http://192.0.2.1:PORT`, at the test proxy's port, in `http_proxy`, `https_proxy`, `HTTP_PROXY`, and `HTTPS_PROXY`, after their own entries, but one that names `HTTPS_PROXY` itself, which keeps exactly its own environment; root with `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW`, which hold them; and, on Linux, a batch with a start that the framing carries alone but not with the proxy's URL, which `exec` refuses before it attaches, running none of its executions |
| `published_listeners_refuse_binding_and_raw_sockets` | The image over the scratch, in a network with ingress allowed, through which OpenVMM publishes the guest's listeners | A root configuration whose listeners are published; root with `CAP_NET_BIND_SERVICE`, and with `CAP_NET_RAW`, which the guest refuses at stage 0 with `EPERM`, fenced and without output, and with `CAP_CHOWN`, which holds it; and an execution with exactly its start's environment, without a proxy |
| `a_denied_network_lets_no_probe_through` | The image over the scratch, in a network that denies everything, without a proxy | After the positive control, no network device, and an execution that reaches neither the proxy nor any destination, none of whose traffic arrives |
| `an_allow_rule_lets_only_its_destination_through` | The image over the scratch, in a network that denies egress but for one rule, TCP to the gateway at the port of a service of host loopback, with outbound host-loopback access | After the positive control, an execution that reaches that service alone: not the proxy, nor the proxy's port over UDP, the host's own address, or host IPv6 loopback |
| `a_proxy_only_network_lets_only_its_proxy_through` | The image over the scratch, in a proxy-only network, whose proxy is the test proxy | After the positive control, an execution that reaches the test proxy through the URL in its environment, and none of the destinations, whose traffic never arrives |
| `outbound_host_loopback_publishes_no_listener` | The image over the scratch, in a network with egress allowed and outbound host-loopback access | After the positive control, an execution that reaches the proxy and every destination, and a listener of the guest that the host never reaches at the guest's port of host loopback while it listens, since OpenVMM does not publish it |
| `allowed_host_loopback_publishes_the_listeners` | The image over the scratch, in a network with host-loopback access in both directions, and egress denied | After the positive control, an execution that reaches neither the proxy nor any destination, since egress governs the guest's own connections, and a listener of the guest that the host reaches at the guest's port of host loopback, and exchanges a line each way with |
| `a_forward_reaches_its_guest_port` | The image over the scratch, in that network, with a forward of a port of host loopback to a guest port | A listener of the guest at that port, which the host reaches through the forward, and exchanges a line each way with |

A row names the root, the image, the GPT disk's top-layer variant, the
identity that the launch expects of the image, the upper, the runtime
directory, the overlay mode, the scratch's journal policy, the bound of the
first partition, the sandbox's workload identity, the VM's network, which the
launch reads from a file that the scenario writes, and the host paths that the
launch shares, and what the scenario expects: a root that the guest assembles,
with a check that probes it or runs executions in it, a teardown that fails
after a check, the stage that fails, with its problem, a mapping of the table
that the guest fails, after how many, with its problem, shares that the host
program refuses before it spawns the VM, with the problem, or a launch that the
host program refuses before it spawns the VM, with the change that the scenario
makes after it stages the images, and the problem. Each scenario that launches a
VM checks the launch's record, with the bindings that the store stages, which
the launch checked, and the number of mappings that the root configuration
announced, and, over a scratch, whether the VM left the scratch's journal to
recover, which tells whether the guest unmounted the scratch.

A scenario with the mapping table writes its sources in its directory, below
`sources`: a directory `dir`, with the files `marker` and `secret` and an empty
directory `inner`; a file `file.txt`; and a directory `out`, with a file
`marker` and an empty directory `inner`. The launch shares `dir` read-only at
`/srv/maps/dir`, `dir/inner` read-write inside it, `file.txt` read-only at
`/srv/maps/file.txt`, and `out` read-write at `/srv/maps/out`, hides
`dir/secret` with `--deny`, and authorizes `sources` alone with
`--share-root`. The plan exports `dir`, `file.txt`, and `out` as the export's
children `0`, `1`, and `2`. Once the `map` probe ran, the sources hold what
the scenario wrote, and the file `written` in `dir/inner` and in `out` alone;
the scenarios of a failed mapping and of refused shares leave the sources as
the scenario wrote them.

The scenarios with a network give the guest the address `192.0.2.2/24`, in a
range reserved for documentation, which no host's own address shares, so that
the guest reaches the host's address through its gateway, `192.0.2.1`. Before
the launch, the test binds the host's side of the network, whose ports the
network names: a test proxy on host IPv4 loopback, which the guest reaches at
the same port of its gateway, and the sockets of the probes' destinations,
another port of host loopback over TCP, the proxy's port over UDP, and the
proxy's port over TCP at the host's own address, if it has one, and on host
IPv6 loopback, if it has that, which the guest reaches at its gateway's IPv6
address. It also draws the port of a listener of the guest, which no socket
holds for TCP or UDP, on every host address or on loopback alone, as OpenVMM
needs to publish the listener on host loopback, and the port of a forward. It
draws each port at random from the dynamic range, since Windows excludes ports
per protocol, in blocks. Scenarios run at the same time, so each holds the
ports that it drew until it ends, or for good if it fails, and draws none that
another holds: OpenVMM binds the ports of a listener and of a forward only once
it runs, and again for the scenario's VM after the positive control's.

Each VM with a network first has its guest's network devices checked, of which
OpenVMM attaches none for a network that lets nothing through, and otherwise
one, which `/init` configures, and that OpenVMM asks `/init` on the kernel
command line to publish the guest's listeners exactly where the root
configuration says that they are published. Then, as NVX's proxy-only scenario
does, an execution requests a URL through the test proxy with `wget`, while the
host serves the proxy, and tries each destination with `nc`, and the host
checks what arrived at each of its sockets. A listener of the guest, `nc` in
the image, greets the host, which tries to reach it until the listener's
execution ends.

A scenario whose network stops traffic begins, as NVX's proxy-only scenario
does, with the positive control: a VM of an open network, with egress allowed
and host-loopback access in both directions, over the same host's side. There
the execution reaches the test proxy, which it names itself, and every
destination, whose traffic arrives at the host's socket, and the host reaches
a listener of the guest at the guest's port of host loopback. Once that VM
ended, the host discards what is left at its sockets, and the scenario's own VM
runs the same checks, so that what does not arrive there shows the network's
policy, not a broken probe.

The scenarios use the fixtures of NVX's own layered scenarios, from NVX's
artifacts: the Ubuntu EROFS image, `ubuntu-distro.erofs`, its manifest, which
holds its UUID, and the scratch template, `ubuntu-smoke-scratch.ext4`, an
empty ext4 file system, of which each scenario boots a copy. Packages ship the
template beside the image, so `nvx-rs download` installs both into `build/`,
and the guest-artifact build makes it with the image;
`python3 scripts/nvx.py build-scratch-template` formats it on Linux, with the
parameters that the packages' template has
([`build-scratch-template`](../../doc/usage.md#build-scratch-template)).

A scenario of a GPT disk writes the disk from the template first: a protective
MBR, and the primary and backup tables of 128 entries, with the first
partition, 4 KiB of bytes that are not UTF-8, then padding; the base layer, in
partition 2, a copy of the template; and the top layer, in partition 3, a small
file system in the ext2 format, which the kernel's ext4 driver mounts, with a
file and a directory. The test writes the disk, and the top layer itself, with
`tests/common/gpt.rs`, since `mkfs.ext4` is not available everywhere that the
tests run, and checks the disk with `nvx::gpt` before it launches the VM. For
`corrupt_layer_fails_the_assembly`,
it corrupts the top layer: the inode of the layer's top directory loses its
mode, an error that the kernel finds when it mounts the layer, and the error
policy of its superblock asks the kernel to panic on an error, which the
assembly's `errors=remount-ro` overrides.

The journal scenarios append an internal journal to the top layer, mapped by
one extent in reserved inode 8. Its nonempty ring holds empty revoke records
of one sequence, with no commit, so a kernel journal scan would cycle forever.
With the file system's `needs_recovery` bit clear, `noload` bypasses that scan
and the root remains usable. With that bit set, the assembly refuses the
layer before any mount: `noload` is not permission to ignore required recovery.

### Template scenarios

`tests/vm_template.rs` creates [templates](#templates) on a real hypervisor, in
the host-only configuration, through `runtime-adapter-host create`, and runs
`refresh`, `list`, `inspect`, and `remove` on them. Each test
stages its placeholder, a copy of NVX's Ubuntu EROFS image with its manifest,
in a directory that only its user may change, as a platform stages its trusted
artifacts, and creates a template of it in a template store of its own:

| Test | Template | Expects |
| --- | --- | --- |
| `a_template_without_a_network_has_no_nic` | 2 processors, 1 of them online at boot, 256 MiB of RAM with a capacity of 512 MiB, a 32 MiB scratch, and no network | A captured template without a NIC, whose key records its 1 processor online at boot, which `maxcpus=1` limits |
| `a_template_whose_processors_are_all_online_at_boot_limits_none` | 2 processors, both online at boot | A captured template whose key records no processors online at boot, without `maxcpus=` |
| `a_network_that_lets_nothing_through_has_no_nic` | A network that denies egress, ingress, and host-loopback access, without a proxy | A captured template without a NIC |
| `the_networks_of_every_policy_at_one_guest_s_address_share_one_template` | Host-loopback access both ways, with a forward of port 18080 of host loopback to the guest's port 8080; then, at the same guest's address, each network that lets something through: egress allowed, with outbound host-loopback access or without, one TCP rule to the gateway at port 8080, with outbound host-loopback access, a proxy at port 3128 of host loopback, ingress allowed, host-loopback access both ways, and that forward | A captured template with a NIC, under the complete deny-all policy, whose key's command line names the NIC's addresses, but neither a DNS server nor a publication of the guest's listeners; then each creation `existing`, with the same generation, key, and class, without a capture |
| `a_class_that_no_capture_takes_is_refused_before_the_store` | 3 processors, then 2 processors with 4 online at boot | NVX's refusal of each class, with status 1, before the store exists |
| `a_refresh_captures_and_retires_the_generations_that_it_supersedes` | A creation, then two refreshes, the second while a clone leases the second generation, then a creation once the clone released it | `list` and `inspect` of the first generation; each refresh captures a new generation, although the store holds an eligible one, and its eviction retires the first generation, and leaves the leased second one, tombstoned, with its payload; the creation serves the third without a capture, and its eviction deletes the second |
| `a_removal_reports_busy_while_leases_keep_the_generation` | A creation, two clones' leases of its generation, a removal, a second creation, then a removal after each release | `list` and `inspect` with the leases; the removal tombstones the generation, which keeps its payload and takes no new lease, and reports busy, with status 7 and its leases; the second creation captures a new generation, and leaves the first to its leases; a removal reports busy while a lease remains, and deletes the generation once none does; a second removal finds it retired, one of a generation that the store never published fails, and a generation that no lease keeps goes at once |
| `a_refresh_refuses_a_class_that_no_capture_takes_before_the_store` | A refresh of 3 processors | NVX's refusal of the class, with status 1, before the store exists |

Unless a test says otherwise, a template has 1 processor, online at boot, 256
MiB of RAM, its capacity, and a 64 MiB scratch. A captured template is the
store's one generation, whose payload holds the record of its capture, its key,
and its snapshot, and whose key holds what the test asked for: the runtime
identity of the adapter's VMs with the guest agent's build, the backend, the
processors and those online at boot, none for a template whose processors are
all online at boot, the RAM and its capacity, the geometries of the placeholder
and of the scratch, the NIC, and a kernel command line of the platform tier's
tokens alone, with the processors, `maxcpus=` with those online at boot
exactly when they are fewer, and, exactly with a NIC, the NIC's addresses, but
neither a DNS server nor a publication of the guest's listeners, which OpenVMM
names to no guest under the NIC's deny-all policy, and which names neither the
placeholder's identity nor a root's tokens. Its class's
slot then holds no transaction and records no launch, and a second creation
prints `existing`, with the same generation, key, and class, without a capture:
the slot's log of the VMM stays as the capture left it.

The scenarios of the operations lease generations to clones through NVX's
store, as a clone's start does, but launch nothing, and release the leases as
a clone's teardown does. They check what `list` and `inspect` print against
what the store records: each generation's key, class, state, eligibility at
the host's time, by its monotonic clock, since this host and boot captured it,
and leases, and, from `inspect`, the capture's storage generation, time
contract, and lengths; and what each eviction and removal printed against the
generations and payloads that the store then holds.

### Clone scenarios

`tests/vm_clone.rs` starts [clones](#clones) of templates on a real hypervisor,
in the host-only configuration, through `runtime-adapter-host start` and
`run`. Each test
captures a template of its own, of 1 processor and 256 MiB of RAM with a
capacity of 1 GiB, in a template store of its own, of a copy of NVX's Ubuntu
EROFS image that a directory of the platform holds, which the clones stage as
their image, or of a GPT disk 16 MiB longer than it that the platform holds
beside it,
without a NIC unless its row names a network, whose guest is at `192.0.2.2/24`,
as in the [template scenarios](#template-scenarios),
and starts its clones, each in a sandbox store of its own, over a copy of the
scratch template, which no one else may change either, or in RAM. The GPT disks
of a template's placeholder and of the clones, which the test writes with
`tests/common/gpt.rs`, as `tests/vm_layered.rs` writes its own, hold their
first partition, then layers, empty but for one disk's regular file `etc`, at
partitions that differ from one disk to another. The host directories that
clones export lie in the template's directory, beside the template store, in a
directory that the starts authorize:

| Test | Clones | Expects |
| --- | --- | --- |
| `one_generation_serves_clones_of_each_upper_and_memory_target` | Two at once, over a scratch image at 256 MiB, and in RAM at 512 MiB with untrusted lowers, then one over a scratch image at 1 GiB | Each started, by the host's monotonic clock, with its own lease, which the store lists with the clone's sandbox, its memory target online, its upper, overlay mode, and image's contents, and the events of its activation in order; a stop releases each lease; the generation's snapshot unchanged, and its payload with nothing but the capture's files once the clones stopped |
| `one_generation_serves_clones_of_each_processor_count_from_its_boot_online_count_to_its_capacity` | Of a template of 4 processors, 1 of them online at boot: in turn, one without a processor target, one of 2, and one of 4; then targets of 3 and 8 | Each started, with the CPUs of its target online, CPU 0 alone by default, of the 4 possible ones, and the CPUs beyond the first that the time ABI's record of the restore names as those that the restore brought online, none by default, and the events of its activation in order; then for each of the others, status 8, with NVX's reason, and the record of a failure of the request, at the plan, without a VM, by the evidence of invalid targets, which quarantines nothing; the lease released, and the generation's snapshot unchanged |
| `a_generation_refuses_clones_of_fewer_processors_than_its_template_booted_with` | Of a template of 4 processors, 2 of them online at boot: a target of 1, then in turn one without a processor target, and one of 4 | Status 8, with NVX's reason, and the record of a failure of the request, at the plan, without a VM, by the evidence of invalid targets; then each started, with CPUs 0 and 1 online by default, which the restore brought none of online, and all 4 online for the target of 4, of which the restore brought CPUs 2 and 3 online |
| `one_generation_serves_clones_of_either_format_over_either_upper` | Of a template whose placeholder is a GPT disk of two layers, longer than the EROFS image, four at once, each image shorter than the placeholder: the EROFS image over a scratch image and in RAM, a GPT disk of one layer, of the EROFS image's length, in RAM, and one of three layers, as short as its layout allows, over a scratch image, with a bound that its first partition meets exactly; then the disk of one layer, with a bound one byte smaller than its first partition | Each started, with its upper and the overlay's options; the EROFS image's root, without a first partition; each GPT disk's layers, ext4 file systems mounted read-only once each, from the partitions that its own table names, stacked in order, and the bytes of its own first partition; the image's read-only disk at the placeholder's length, and the scratch device at the template's scratch length, each of which the guest reads to its end; for a clone in RAM, no file of its sandbox as long as the template's scratch, and on Linux `--restore-absent-scratch` and no `scratch` block device among its VMM's arguments, which name one over a scratch image; then status 8, and the record of a failure of the request, at the activation, once the VM exited, by the evidence of an unusable root, with the guest's status 54; nothing quarantined, and the lease released, with the records |
| `a_start_refuses_what_the_template_cannot_serve_before_any_vmm_starts` | An EROFS image and a GPT disk 4 KiB longer, and an EROFS image 100 bytes longer, than the placeholder, a scratch image of twice the template's scratch, memory targets of 2 GiB and of 320 MiB, unstaged images, and a generation that the store does not hold, then the generation once its key is gone | For those that NVX's plan refuses, status 8, with NVX's reason, and the record of a failure of the request, at the plan, without a VM, by the evidence of a length that the request supplied, of the image's geometry, which OpenVMM cannot attach, or of invalid targets, which quarantines nothing; for unstaged images, status 1, without a record; for the generation, status 10, with the record of a selection that names no template, which the store never published, without a lease; any lease released, and no VMM's log or record in the sandbox; for the generation without its key, status 8, and the record of a failure of the template, at the plan, by the evidence of a malformed generation, which quarantines the generation, so that the store refuses a later start's selection, with status 10, as `ineligible`, by the evidence of its tombstone, without a lease |
| `a_clone_whose_activation_fails_ends_before_it_is_ready` | A scratch image of zeros, a journal that the record forbids, a workload identity whose account the image lacks, and, through NVX's restore sequence, a ping, a start, or the start of an execution before the `Activate` | Status 8, no `Ready`, and the record of a failure, at the activation, once the VM exited: of the request, by the evidence of an unusable root, with the guest's status 54, or of invalid settings, with 55, or, before the `Activate`, of an unknown cause, by the protocol's, with 51, which the sandbox records until the release; nothing quarantined, and the lease released, with the records |
| `a_clone_refuses_a_scratch_image_whose_upper_an_earlier_clone_used` | Over a scratch image, in whose root the `contents` probe writes a file, then over a copy of that scratch image once the first clone stopped | The first started; then status 8, since the copy's upper is not fresh, and the record of a failure of the request, at the activation, once the VM exited, by the evidence of an unusable root, with the guest's status 54; nothing quarantined, and the lease released, with the records |
| `clones_at_once_keep_their_own_uppers_and_machine_ids` | Three at once: over a scratch image and in RAM, each with a generated machine ID, and in RAM with the image's own; two executions in each, as user ID 0, that write a marker in the root, and try to write `/etc/machine-id` | In every execution, the hostname `runtime-adapter`, and no marker of another clone; a generated machine ID that differs between the clones, the same in each execution of a clone, held by one bind that is read-only, `nosuid`, and `nodev`, and that the write cannot change; with the image's own machine ID, none, since the image has none, no bind, and in the later execution, the ID that the first wrote; then each stop releases its lease |
| `a_clone_whose_view_cannot_be_set_ends_before_it_is_ready` | Of a template of the EROFS image, a GPT disk whose layer holds a regular file `etc`, with the image's own machine ID, then with a generated one | The first started, with the disk's root; then status 8, since no bind can hold the machine ID below a file, and the record of a failure of the request, at the activation, once the VM exited, by the evidence of invalid settings, with the guest's status 55; nothing quarantined, and the lease released, with the records |
| `a_clone_runs_its_executions_as_its_workload_identity_once_it_is_ready` | Over a scratch image, as nobody | An execution that runs as nobody, one that names an identity refused with `EINVAL`, and the first execution after the clone's start |
| `a_generation_is_eligible_by_the_clock_of_its_capture` | At the host's time, then with clock fixtures: UTC a day behind, another boot an hour after the capture by UTC, and the restore window passed by each clock, or UTC before the capture | The host's monotonic clock on the boot of the capture, whatever UTC says, UTC after a reboot, with the fixture's downtime, and, outside the window, status 10, with the record of the store's refusal of the selection, `expired` once the window passed, and `ineligible`, by the evidence `early`, before the capture, without a lease |
| `a_clone_reads_no_more_of_its_disks_than_its_root_needs` | Over a scratch image, and in RAM | Less than an eighth of each disk read once the clone is ready, and each disk's whole length once the guest read it to its end |
| `a_clone_s_time_settles_after_it_is_ready` | In RAM | The time daemon's restore check `ok`, after the restore's generation, without a violation, after a downtime that the host's monotonic clock measured, and a clone that still serves |
| `each_command_reattaches_to_the_clone_from_a_new_process` | Over a scratch image, then `reattach`, `probe`, `exec`, and `reattach` again, each from a new process | The launch's records, committed, with the clone's VM and no failure; each command `live`, with the clone's VMM, in a new session of the same broker, in a later epoch; the events of the activation as they were; an execution as nobody; and a stop that releases the lease, with the records |
| `a_host_program_of_another_runtime_ends_the_clone_through_its_verified_identity` | In RAM, then a `reattach` with another guest agent's build | Status 8, `reattached=failed`, without a session, and the record of a failure of the host, at the reattach, by the evidence of the runtime's mismatch, once NVX terminated the VMM, which quarantines nothing; the lease released, with the records; then another clone that starts and stops |
| `a_clone_whose_vm_exits_once_a_stop_found_it_fails_before_its_lease_is_released` | In RAM, then a `stop` that the step hook pauses at `found`, while the test terminates the clone's VMM | Status 8, `reattached=live` and `stopped=exited`, and the record of a failure of an unknown cause, at the run, by the evidence of the clone's lost status, without a status of the guest's, once the VM exited, which quarantines nothing; the lease released, with the records; then another clone that starts and stops |
| `a_tombstone_leaves_the_running_clone_of_its_generation_to_its_lease` | Over a scratch image, then a tombstone of its generation, through NVX's store | A start whose selection the store refuses, with status 10 and the record of an `ineligible` generation, by the evidence of its tombstone, without a lease; the running clone, which reattaches, probes, runs an execution, and stops, which releases its lease; and the generation's snapshot unchanged |
| `the_lease_of_a_start_that_ended_before_its_record_is_released_through_its_template_store` | In RAM, in a sandbox that NVX's store leased a generation to, as a start that ended before it recorded the clone leaves it: first moved, with another sandbox at its lease's path, which holds a lease of its own, then none; then in place, without a record of a clone, then with the record of an earlier clone, of another template | For the moved sandbox, a `stop` and a `reattach` that refuse it, since the sandbox at its lease's path does not hold its lease, and leave that sandbox's lease; in place, a stop that finds the clone `ended` at `planned`, and keeps the lease, whose template store no record names; a stop with another template store, which the release refuses; a `reattach` or a `stop` with `--template` naming the lease's own, which releases it; and a start and a stop in the sandbox after each |
| `a_clone_maps_an_aggregate_of_directories_and_files_beneath_its_root_before_its_sandbox_starts` | Over a scratch image, sharing directories and files of a host directory: a read-only directory, with a read-write directory inside it, at its target there, a read-only file, a read-write file, and a read-write directory, with a file of the first hidden, and of the last a directory hidden, but a file in it shown again | Started, with five mappings, and a record that names the export, an aggregate, by the identity of its plan, with five mappings; on Linux, the VMM's `--mount-aggregate` and one `--mount-child` per directory and file, in order, the first directory read-write, each file with its access and `file`, then the hidden paths, the one shown again, and the one that the guest may write, and no `--mount`; the export mounted once, read-write, `nosuid`, and `nodev`, below a `hostfs` of mode 0700 that root owns; the mappings below the root, and nothing else, in the table's order, each with its access, `nosuid`, and `nodev`, showing what the host holds but for the hidden files, and the hidden directory, which lists the file shown again alone, with the missing targets created empty in the upper; the same from an execution as user ID 0; and writes through the writable mappings alone, which the host's directory holds, before and after the stop |
| `clones_at_once_see_only_their_own_exports` | Four at once: one with a read-write export of its own and a generated machine ID, two over the same read-only export, over a scratch image and in RAM, and one without an export | Each with an export started with one mapping, and a record that names it, an aggregate, by the identity of its plan, the same for the clones of one export, and on Linux, the VMM's `--mount-aggregate`, with one `--mount-child`, read-write or read-only, as the mapping needs; each export mounted read-write, even where its mapping is read-only; below the root, the mapping, beside the bind of a generated machine ID; each clone's own directory, with its access; without an export, no export in the record, no mapping, none of OpenVMM's options of an export on Linux, no export mounted, and nothing below the root; the `entries` probe, whose helper writes the root's machine ID, failing with `EROFS` where the generated one is bound; the host's directories with what the writable export took alone; then each stop releases its lease |
| `a_start_refuses_an_export_whose_mapped_objects_changed_before_any_vmm_starts` | Sharing a read-write directory, a directory inside it at a target of its own, and a read-only file; while the step hook pauses each start at `checked`, a directory replaces the file, then another directory the one inside the read-write directory | Status 8, with NVX's reason, and the record of a failure of the request, at the launch, without a VM, by the evidence of invalid mappings, which quarantines nothing; no VMM's log, launch journal, runtime record, or record of the clone's launch in the sandbox, and the lease released |
| `a_clone_whose_mapping_fails_ends_before_its_sandbox_starts` | Over a scratch image, sharing a host directory whose second mapping puts a directory onto a file of the image | Status 8, a `Ready`, but no start, and the record of a failure of an unknown cause, at the start, at the clone's `Ready`, by the evidence of the protocol, once the VM exited with the guest's success, or NVX terminated it, with the guest's report that the table failed after its first mapping; nothing quarantined, and the lease released, with the records; the host's directory unchanged |
| `a_template_without_a_nic_serves_clones_without_a_network` | Two at once, without a network, and with one that lets nothing through; then, once they stopped, one of each network with a NIC of the template scenarios | The first two started, with no network in their records, on Linux no argument of a network of their VMMs, no network device, and the image's own `/etc/resolv.conf`, an empty file of mode 0644, which no bind holds, and root may write; then for each of the others, status 8, with NVX's reason, and the record of a failure of the request, at the plan, without a VM, by the evidence of invalid settings, which quarantines nothing; the lease released |
| `one_template_serves_the_clones_of_every_network_policy_at_its_guest_s_address` | Of a template of host-loopback access both ways, with a forward: in turn, one of each network with a NIC, of egress allowed, with outbound host-loopback access or without, of one TCP rule to the gateway, with outbound host-loopback access, of a proxy, of ingress allowed, of host-loopback access both ways, and of that access with a forward; then one at the guest's address `192.0.2.3/24`, one of a network that lets nothing through, and one without a network | Each of the first started, with one network device, on Linux the VMM's `--restore-network-policy`, then the arguments of the clone's own policy, without `--net`, and a record that names the policy, the gateway's DNS server for the networks with egress allowed, the proxy's URL for the proxy-only one, and a publication of the guest's listeners for those with ingress or host-loopback access both ways; an execution's TCP connections to the gateway, at the ports of the host's listeners on host loopback, that reach the allow rule's alone, the proxy's alone, every one with egress and outbound host-loopback access, and none otherwise, as the host's listeners confirm; a listener of the guest, as nobody, that the host reaches at its port of host loopback, with a line each way, exactly with host-loopback access both ways, once the clone's announcer announced it, and through the forward's port; and executions as user ID 0 that ask for `CAP_NET_BIND_SERVICE` or `CAP_NET_RAW` refused with `EPERM` where the listeners are published, and started elsewhere, as one that asks for `CAP_CHOWN` everywhere; then for each of the others, status 8, with NVX's reason, and the record of a failure of the request, at the plan, without a VM, by the evidence of invalid settings, which quarantines nothing; the lease released; each clone's policy differs from the others' |
| `a_forward_whose_port_another_socket_holds_fails_the_clone_as_the_host_s` | Of a template of a network with egress allowed, one of host-loopback access both ways, with a forward of a port of host loopback that a listener of the host holds; then, once the listener closed, one of the same network | Status 8, with OpenVMM's cause, that it cannot create the forward's socket, and the record of a failure of the host, at the restore, once the VMM exited before its restore-ready event, by the evidence of the forward's bind, which quarantines nothing; the lease released, with the records; then a clone that starts and stops |
| `a_clone_binds_the_resolver_of_its_policy_s_dns_server_read_only` | Of a template of a proxy-only network, two at once, of a network with egress allowed, with a generated machine ID, and with the image's own; then, once they stopped, a GPT disk whose layer holds a regular file `etc`, with the image's own machine ID | Each of the first two started, with a record that names the gateway's DNS server, one network device, and at `/etc/resolv.conf`, a file of mode 0444 that names the server, held by a bind that is read-only, `nosuid`, and `nodev`, after the bind of the generated machine ID, below the root, whose file system refuses a write as root, with nothing in the upper there; and a TCP connection to the server's DNS port, which the gateway's DNS proxy accepts without the host's resolver; then status 8, since no bind can hold the resolver configuration below a file, and the record of a failure of the request, at the activation, once the VM exited, by the evidence of invalid settings, with the guest's status 57; nothing quarantined, and the lease released, with the records |
| `a_clone_s_executions_find_the_proxy_of_its_policy` | Of a template of a network with egress allowed, one of a proxy-only network, with an execution as user ID 0 that runs `env`, then one that names its own `HTTPS_PROXY` | A record that names the proxy's URL, `http://192.0.2.1:PORT`, at the port of the proxy's listener, and no DNS server; the first execution with its own entry, then the URL in `http_proxy`, `https_proxy`, `HTTP_PROXY`, and `HTTPS_PROXY`; the second with exactly its own environment; one network device, and the image's own `/etc/resolv.conf`, which no bind holds |
| `a_clone_of_a_template_with_a_nic_maps_its_export_beside_its_resolver` | Of a template of a network with egress allowed, one with a generated machine ID, sharing a host directory read-write | Started, with one mapping, and a record that names the export and the gateway's DNS server; on Linux, the VMM's `--mount-aggregate`, with one `--mount-child`, read-write, and its `--restore-network-policy`, then the arguments of the clone's policy; below the root, the binds of the machine ID and of the resolver configuration, then the export's mapping; the export's directory, and a write through it, which the host's directory holds after the stop |
| `a_selection_that_names_no_template_leases_nothing` | A start that names a template store that does not exist, then, once `remove` retired the generation, which no lease kept, a start of it | Status 10, with the record of a selection that names no template, by the evidence `no-store`, then `retired`, without a lease or a VM; and a store that indexes no generation |
| `a_run_runs_one_execution_in_its_clone_then_tears_the_clone_down` | A run over a scratch image, as nobody, whose execution names its environment and its working directory, and ends with status 3, then a run in RAM, as nobody, whose execution names an identity of its own | Each with status 0, in one command: the preflight's `staged`, the clone started as a start starts one, the execution, and a stop that releases the lease, without a VM; the first execution's output, of the Ubuntu image's root, as nobody, in its working directory, with its environment, and its status; the second refused with `EINVAL`; no lease left, and the payload with nothing but the capture's files |
| `a_run_whose_clone_does_not_start_runs_nothing` | A run that names a template store that does not exist, one with a clock fixture past the restore window, and one over a scratch image of zeros | Once the preflight staged the images, status 10, with the record of a selection that names no template, by the evidence `no-store`, then of an `expired` generation, without a lease or a VM; then status 8, with the record of a failure of the request at the activation, once the VM exited, the lease released, and no execution or stop |
| `a_run_in_a_template_of_a_network_finds_its_policy_s_proxy` | Of a template of a proxy-only network, a run of its network, whose execution, as user ID 0, runs `env` with an entry of its own, then a run of a network that lets nothing through | The first with status 0, its clone started and stopped as a run's, and the execution's own entry, then the proxy's URL, `http://192.0.2.1:PORT`, at the port of the proxy's listener, in `http_proxy`, `https_proxy`, `HTTP_PROXY`, and `HTTPS_PROXY`; then, once the preflight staged the images, status 8, with NVX's reason, and the record of a failure of the request, at the plan, without a VM, by the evidence of invalid settings, which quarantines nothing, the lease released, and no execution or stop |
| `a_run_s_execution_finds_its_shares_beneath_the_root` | A run over a scratch image, sharing a host directory read-write, whose execution, as user ID 0, reads a file of the directory and writes another | Status 0, its clone started with one mapping and stopped as a run's; the file's contents in the execution's output; and the written file in the host's directory once the run tore the clone down |
| `a_run_whose_clone_s_vm_exits_during_its_execution_ends_with_the_clone_s_failure` | A run over a scratch image, sharing a host directory read-write, whose execution, as user ID 0, writes a file there, then waits, while the test terminates the clone's VMM | Status 8, its clone started with one mapping, the diagnostic of the executions' failure, `stopped=exited`, and the record of a failure of an unknown cause, at the run, by the evidence of the clone's lost status, without a status of the guest's, once the VM exited, which quarantines nothing, and no execution; the lease released, with the records; then another clone that starts and stops |

The scenarios' networks let nothing through, or allow egress, with outbound
host-loopback access or without, or one rule that allows TCP to the gateway at
the port of a listener of host loopback, with outbound host-loopback access, or
have a proxy at the port of another, or allow ingress, or host-loopback access
both ways, with a forward or without. The tests bind those listeners, and a
third, which no network names, on host IPv4 loopback, at ports that the host
chooses, and hold them until they end. The forward's port of host loopback, and
the guest's port of the listener that the scenarios run in the guest, to which
it forwards, are ports that no socket of the host held when the tests drew
them. A
clone's start names its network by a file that the test writes in the
template's directory.

A test that fails keeps its template's directory, with the template store and
the platform's image, as long as it keeps the directory of a clone that leases
the template, whose VM might still run.

### Stage scenarios

The template and clone stage's scenarios run as one suite, which
[`scenarios.txt`](scenarios.txt) names: each line names a test target of
`tests/`, then one of its tests. CI's runtime adapter step runs the suite on
each backend, KVM, MSHV, and WHP, with
`python3 scripts/nvx.py test-runtime-adapter --backend BACKEND`, in the
host-only configuration: it runs each target's scenarios by name, the VM
scenarios, which are ignored, with the others, and checks that each of them ran
and passed. It fails once a line names a test that its target does not have, a
target that a line names has a test that no line names, the library or another
target has an ignored test, or a named test does not run and pass, so that a
scenario that is renamed, added, or gone fails it rather than drop out of the
suite. With `--check-only`, it checks the names alone, and runs nothing.

The suite holds each operation's outcomes, from `create` to `run`, without a
hypervisor and in VMs, and every scenario of the stage's VMs: launches of
layered roots, captures of templates, and the starts and runs of their clones.

| Target | Scenario | Proves |
| --- | --- | --- |
| `template_capture` | `a_capture_takes_its_class_and_inputs_from_the_request` | A capture's request: the plan of the placeholder over RAM, with the request's network, the placeholder's binding, the policy revision, the artifacts, and the class, which the network names |
| `template_capture` | `the_host_program_refuses_a_template_before_it_creates_anything` | `create` refuses a network that the plan refuses, or that it cannot read, with status 1 before it needs NVX's artifacts or creates the store |
| `template_operations` | `a_refresh_refuses_a_template_before_it_creates_anything` | `refresh` refuses a network that the plan refuses with status 1 before it needs NVX's artifacts or creates the store, as `create` does |
| `template_operations` | `an_empty_template_store_lists_no_generation` | `list` of a store without generations prints none, and `inspect` and `remove` of a generation that it never published fail with status 1, and tombstone nothing |
| `template_operations` | `the_host_program_refuses_command_lines_of_the_operations` | The command lines of `refresh`, `list`, `inspect`, and `remove` that the host program refuses, each with status 2 |
| `template_operations` | `the_operations_open_a_template_store_and_create_none` | `list`, `inspect`, and `remove` of a directory that does not exist, or that holds no template store, fail with status 1, and create nothing |
| `clone_start` | `a_clone_s_plan_request_and_record_carry_its_network` | A clone's network reaches the adapter's plan, NVX's request of the clone, and the clone's record, from which an execution finds the URL of the network's proxy |
| `clone_start` | `a_clone_s_record_names_its_template_store_and_generation_beside_its_plan` | A clone's record names its template store and its generation, which a stop reads back, beside the plan that the other commands read |
| `clone_start` | `a_clone_s_request_takes_the_staged_bindings_and_the_guest_s_defaults` | NVX's request of a clone takes the staged bindings, the processor and memory targets, the guest's defaults or the request's settings, and the export of the request's shares, and refuses images that the store does not stage |
| `clone_start` | `a_host_s_time_keeps_its_text_form` | A host's time keeps the clock fixture's text form, on both clocks, and the texts that are none are refused |
| `clone_start` | `a_run_refuses_a_network_or_shares_that_the_plan_refuses_before_it_stages_anything` | A run refuses a network that it cannot read, or that the plan refuses, and shares that it cannot export, with status 1 before it stages anything or creates the sandbox's store |
| `clone_start` | `a_run_refuses_a_selection_that_names_no_template_once_its_preflight_staged_the_images` | A run refuses an image that the plan refuses with status 1 before it stages anything, and a selection that names no template with status 10 and the refusal's record once its preflight staged the images, without a lease |
| `clone_start` | `a_start_refuses_a_generation_that_the_template_store_does_not_hold_before_anything_else` | A start whose template store does not exist, or does not hold the generation, ends with status 10 and the refusal's record, and one whose image the plan refuses with status 1, before it needs NVX's artifacts or creates the sandbox's store |
| `clone_start` | `a_start_refuses_a_network_that_the_plan_refuses_before_anything_else` | A start refuses a network that it cannot read, or that the plan refuses, with status 1 before its image, its shares, and the template store |
| `clone_start` | `a_start_refuses_shares_that_it_cannot_export_before_anything_else` | A start refuses shares that it cannot export with status 1 before it reads the template store or creates the sandbox's store |
| `clone_start` | `the_host_program_refuses_options_that_do_not_make_a_run` | The command lines of `run` that the host program refuses, its execution's included, each with status 2, and a clock fixture that is not a host's time |
| `clone_start` | `the_host_program_refuses_options_that_do_not_make_a_start` | The command lines of `start` that the host program refuses, each with status 2, and a clock fixture that is not a host's time |
| `host_session` | `a_clone_s_order_ends_its_table_at_a_mapping_that_the_guest_fails_and_stops_the_guest` | A clone's order sends no more of its table, nor `Start`, once the guest fails a mapping, and asks the guest to stop |
| `host_session` | `a_clone_s_order_maps_its_export_s_children_by_their_indices` | A clone's order sends its table as a cold launch's, whose sources name each path by the index of the export's child that holds it |
| `host_session` | `a_clone_s_order_sends_its_record_first_then_starts_its_sandbox_as_after_a_cold_boot` | A clone's order sends its activation record first, returns the activation that `Ready` reports, then sends the table in parts and `Start`, and fails on a refusal, a deadline, another build, or an answer that does not decode |
| `host_session` | `a_clone_s_session_that_ends_after_its_bootstrap_is_lost_and_the_next_takes_the_same_record` | A clone's session that a reset, the end of its stream, or its transport ended is lost, and the next session sends the same record first |
| `host_session` | `a_mapping_that_the_guest_fails_ends_the_table_and_the_host_stops_the_guest` | A mapping that the guest fails, at the first, a middle, or the last part, ends the table without `Start`, and the host stops the guest and learns how the stop ended |
| `host_session` | `a_receive_that_times_out_keeps_the_partial_message` | A receive that times out inside a message keeps what arrived, and a later receive returns the whole message |
| `host_session` | `a_reset_or_a_disconnect_in_the_middle_of_a_message_drops_it` | A reset or a disconnect in the middle of a message drops the message, in the next epoch or with a new instance |
| `host_session` | `a_send_that_fails_or_times_out_ends_the_client_with_what_it_holds` | A send that times out after part of a record, or whose transport fails, ends the client with the message that it held |
| `host_session` | `a_stop_skips_the_guest_s_late_messages_about_the_executions_that_the_driver_gave_up_on` | A stop skips the guest's late messages about the executions that the driver gave up on, before and after a teardown that failed, and no message about an execution that it did not start |
| `host_session` | `answers_whose_bodies_do_not_decode_end_the_client` | Answers whose bodies do not decode end the client |
| `host_session` | `only_a_session_that_ended_is_lost_to_a_clone_s_order` | A clone's order takes only a session that ended for lost, and its other failures for final |
| `host_session` | `requests_check_their_answers_and_end_the_client_when_they_fail` | Requests check the kind of their answers, and end the client when they fail |
| `host_session` | `the_client_attaches_and_exchanges_messages_that_span_records` | The client attaches with the launch capability, and exchanges messages that span records both ways |
| `host_session` | `the_client_configures_a_layered_root_probes_it_and_learns_how_its_teardown_ended` | The client configures a layered root, probes it, and learns how its teardown ended once it stopped |
| `host_session` | `the_client_fails_on_records_that_the_protocol_does_not_allow` | Sequence, epoch, and instance errors, stale and identity-less resets, record lengths, bootstrap identities, ERROR payloads, and header faults end the client |
| `host_session` | `the_client_reassembles_messages_from_partial_and_shared_records` | Records that end a message and start the next, and records whose bytes arrive in pieces down to one byte, give back whole messages |
| `host_session` | `the_client_refuses_messages_that_its_framing_does_not_allow` | The client refuses malformed headers before their body arrives, messages that only the host program sends, and a message beyond the framing's limit, which it does not send |
| `host_session` | `the_client_returns_the_stage_of_the_root_that_failed` | The client returns the stage of the root that failed, with its problem |
| `host_session` | `the_client_waits_for_the_guest_and_reports_refusals_while_attaching` | The client waits for the guest while it attaches, until its deadline, and reports a refusal, in an ERROR or by the end of the stream |
| `host_session` | `the_driver_ends_its_run_on_a_message_that_breaks_the_rules` | A message about an execution that the driver did not start, about none, that does not decode, or that breaks the engine's rules, and the end of the stream, end the driver's run |
| `host_session` | `the_driver_fires_the_timers_of_its_executions_while_the_guest_says_nothing` | An execution's timeout fires while the guest says nothing, after which the driver cancels the execution, then fences it |
| `host_session` | `the_driver_refuses_starts_before_it_sends_them` | The driver refuses the starts that the guest would not decode or run, and a start once it ran, before it sends anything, with a client that stays usable |
| `host_session` | `the_driver_runs_executions_at_once_and_collects_their_output_and_outcomes` | The driver runs executions at once, and collects the output of each, byte-exact, and its outcome, a refused launch's included |
| `host_session` | `the_driver_s_sends_wait_for_a_guest_that_stops_reading_until_the_deadline_of_its_run` | A run whose deadline passes in a receive leaves the client usable, and in a send that waits for a guest that stopped reading, ends it |
| `host_session` | `the_host_orders_a_layered_root_ready_then_its_table_in_parts_then_start` | The host's order: the root configuration that announces the table, the guest's readiness, each part of the table once the guest applied the one before, then `Start` |
| `host_session` | `the_host_sends_nothing_more_to_a_guest_agent_of_another_build_or_whose_root_failed` | The host sends nothing more to a guest agent of another build, or whose root failed |
| `staged_images` | `a_launch_takes_the_bindings_that_the_store_stages_for_its_images` | A launch takes the bindings that the store staged for its images, and not those of other images |
| `staged_images` | `staging_verifies_the_image_and_binds_the_scratch` | Staging verifies an image against the manifest beside it, refuses another image's, stages a GPT disk over RAM, and refuses images that no launch could attach |
| `staged_images` | `the_host_program_stages_images_in_the_store` | `stage` records the staged images, a staging that fails leaves them, and another replaces them |
| `staged_images` | `the_store_records_the_staged_images` | The record of the staged images, as written and read, and the forms that a reader refuses |
| `layered_launch` | `a_launch_claims_its_scratch_image_until_it_releases_its_checked_files` | A launch claims its scratch image, which a second launch waits for, then fails to take, until the first releases its checked files |
| `layered_launch` | `a_launch_reads_only_the_networks_that_the_plan_takes` | A launch refuses the network files that the plan refuses, each for the plan's reason |
| `layered_launch` | `a_launch_records_what_it_launched` | A launch's record holds the bindings that it checked, over a scratch image and over RAM, and a VM without a layered root has one too |
| `layered_launch` | `a_launch_refuses_images_that_are_not_staged_or_changed_since` | A launch refuses images that the store does not stage, of another format or identity, and images that changed, were replaced, or are gone since they were staged |
| `layered_launch` | `a_launch_refuses_shares_that_it_cannot_export` | A launch refuses shares outside the authorized directories, inaccessible ones, reserved targets, a read-only share inside a read-write one, a hidden path that does not exist, and an allowed path inside no hidden one |
| `layered_launch` | `a_launch_renders_each_element_of_its_network_through_the_plan` | OpenVMM's arguments of each element of a policy, after the plan's block devices, the proxy's URL, and the publication of listeners, as NVX's lowering renders them |
| `layered_launch` | `a_launch_shares_host_paths_inside_the_authorized_directories` | The export of shares inside an authorized directory maps each at its target, parents first, hides a denied path, shows an allowed path inside a hidden one again, and comes with the root configuration and OpenVMM's arguments that attach it |
| `layered_launch` | `a_layered_launch_hands_openvmm_the_checked_images` | OpenVMM's arguments name the checked files, by the descriptors that OpenVMM inherits on Linux and by their final paths on Windows, without a workload identity or a managed lifecycle |
| `layered_launch` | `a_layered_launch_passes_what_the_guest_checks_as_it_is` | A launch passes on the options that the guest checks as they are |
| `layered_launch` | `a_layered_launch_plans_its_blocks_and_boot_tokens_with_the_adapters_plan` | The adapter's plan of a layered launch: its block devices and boot tokens, without the sandbox profile's settings, and the sizes of the images |
| `layered_launch` | `an_execution_finds_the_url_of_its_network_s_proxy_in_its_environment` | An execution's start carries the URL of its network's proxy after its own entries, keeps its own proxy settings, and is unchanged without a proxy |
| `layered_launch` | `shares_read_their_host_path_their_target_and_their_access` | The text of a share, `HOST:TARGET[:ro]` |
| `layered_launch` | `the_host_program_refuses_a_network_before_it_launches` | `launch --network` refuses a network that the plan refuses, or that it cannot read, with status 1 before it needs NVX's artifacts or creates the store |
| `layered_launch` | `the_host_program_refuses_options_that_do_not_make_a_command` | The command lines that the host program refuses, each with status 2, those of `exec`, `--workload-identity`, and `create` included |
| `layered_launch` | `the_host_program_refuses_shares_before_it_touches_the_store` | `launch` refuses shares that it cannot export with status 1 before it creates the store |
| `layered_launch` | `the_record_of_a_launch_gives_back_its_plan` | A launch's record gives back its plan, from which the host finds the proxy's URL, none for a VM without a layered root, and a reader refuses other records |
| `vm_restart` | `a_host_program_of_another_runtime_finds_the_vm_mismatched_and_signals_nothing` | A host program that ships another build of the guest agent finds the VM mismatched, and signals nothing, until the host program of its runtime stops it |
| `vm_restart` | `a_launch_tears_down_a_vm_whose_guest_agent_is_of_another_build` | A launch whose VM's guest agent reports another build sends it neither the table nor `Start`, and fails, which ends the VM |
| `vm_restart` | `a_restarted_host_program_reattaches_to_the_vm_and_stops_it` | Another process's `reattach` holds the VM, which is not its child, in a later epoch, and a third's `stop` stops it gracefully, with no runtime record left |
| `vm_steps` | `a_launch_that_the_hook_ends_after_its_checks_leaves_no_launch` | A launch that the step hook ends at `checked`, with status 5, leaves nothing of a launch in the store, and releases the scratch image's claim |
| `vm_steps` | `images_replaced_after_the_checks_never_reach_the_vm` | Images that files of zeros replace after the checks never reach the VM, whose guest assembles the checked originals |
| `vm_steps` | `the_hook_pauses_a_launch_at_each_of_its_steps` | The step hook pauses a launch at each of its steps, at which the store holds what the step says |
| `vm_layered` | `a_changed_image_is_refused_before_spawn` | A GPT disk written in place after it was staged is refused before the VM spawns, and boots once staged again |
| `vm_layered` | `a_denied_network_lets_no_probe_through` | A network that denies everything attaches no network device, and an execution reaches nothing |
| `vm_layered` | `a_failed_mapping_tears_the_vm_down` | A mapping that the guest refuses ends the table, without `Start`, and the VM ends cleanly once the guest tore the root down |
| `vm_layered` | `a_forward_reaches_its_guest_port` | A forward of a port of host loopback reaches a listener of the guest at its port |
| `vm_layered` | `a_proxy_only_network_lets_only_its_proxy_through` | A proxy-only network lets an execution reach its proxy, through the URL in its environment, and nothing else |
| `vm_layered` | `a_read_only_share_inside_a_writable_one_is_refused_before_launch` | A read-only share inside a writable one is refused before the VM spawns |
| `vm_layered` | `a_replaced_scratch_is_refused_before_spawn` | A scratch image that another copy replaced after it was staged is refused before the VM spawns, and boots once staged again |
| `vm_layered` | `a_share_inside_a_reserved_tree_is_refused_before_launch` | A share inside a tree that the adapter reserves is refused before the VM spawns |
| `vm_layered` | `a_share_over_a_reserved_tree_is_refused_before_launch` | A share that would cover a reserved tree is refused before the VM spawns |
| `vm_layered` | `accountless_workload_identity_fails_its_stage` | A workload identity whose account the image lacks fails its stage once the root is assembled, and the VM ends with 16 |
| `vm_layered` | `allowed_host_loopback_publishes_the_listeners` | Host-loopback access both ways publishes the guest's listeners, which the host reaches, while egress still governs the guest's own connections |
| `vm_layered` | `an_allow_rule_lets_only_its_destination_through` | An allow rule lets an execution reach its destination alone |
| `vm_layered` | `an_unauthorized_host_path_is_refused_before_launch` | A share of a host directory that the adapter does not authorize is refused before the VM spawns |
| `vm_layered` | `busy_root_fails_the_teardown` | A root that a probe keeps open fails its teardown, which the stop reports, and the VM ends with 15 |
| `vm_layered` | `corrupt_layer_fails_the_assembly` | A corrupt layer that asks the kernel to panic on an error fails the assembly without a panic, and the VM ends with 14 |
| `vm_layered` | `cyclic_journal_is_not_loaded` | A layer's cyclic journal, without `needs_recovery`, is never scanned: the root assembles, serves, and tears down |
| `vm_layered` | `erofs_over_ram` | The EROFS image over a tmpfs in RAM, without a scratch, and with no disk but the image's |
| `vm_layered` | `erofs_over_ram_maps_the_table` | The mapping table over a tmpfs, as over a scratch image |
| `vm_layered` | `erofs_over_ram_with_untrusted_lowers` | The image over a tmpfs in the untrusted-lowers mode, which copies up a file's data and refuses the rename of a lower directory |
| `vm_layered` | `erofs_over_scratch` | The EROFS image over a scratch image: the root that the worker assembled outside PID 1, its overlay in the default mode, its cgroups, an empty table, and a clean teardown |
| `vm_layered` | `erofs_over_scratch_maps_the_table` | The export, mounted once below a private `hostfs`, and the table's mappings, in the plan's order, each with its access, `nosuid`, and `nodev` |
| `vm_layered` | `erofs_over_scratch_with_untrusted_lowers` | The image over a scratch image in the untrusted-lowers mode, which copies up a file's data and refuses the rename of a lower directory |
| `vm_layered` | `executions_run_as_the_workload_identity` | Executions that name no identity run as the workload identity, without capabilities, with exactly their start's environment, and one that names an identity is refused with `EINVAL` |
| `vm_layered` | `executions_run_in_the_root` | Executions run at once in the root, as the identities that they name, byte-exact, and end on their own, reusable |
| `vm_layered` | `executions_take_the_settings_of_a_proxy_only_network` | The executions of a proxy-only network find its proxy's URL in their environment, after their own entries, unless they name their own |
| `vm_layered` | `first_partition_over_its_bound_fails_the_assembly` | A GPT disk whose first partition is larger than its bound fails the assembly, and the VM ends with 14 |
| `vm_layered` | `forbidden_journal_fails_the_assembly` | A scratch image whose journal the launch forbids fails the assembly before the guest mounts it, and the VM ends with 14 |
| `vm_layered` | `full_ram_upper_refuses_writes` | A tmpfs upper refuses writes with `ENOSPC` once full, and takes them again once freed |
| `vm_layered` | `full_scratch_refuses_writes` | A scratch upper refuses writes with `ENOSPC` once full, and takes them again once freed |
| `vm_layered` | `gpt_over_ram` | A GPT disk over a tmpfs, with a bound that its first partition meets exactly |
| `vm_layered` | `gpt_over_ram_maps_the_table` | The mapping table over a GPT disk over a tmpfs |
| `vm_layered` | `gpt_over_ram_with_untrusted_lowers` | A GPT disk over a tmpfs in the untrusted-lowers mode |
| `vm_layered` | `gpt_over_scratch` | A GPT disk over a scratch image: its first partition's bytes, and its layers, each mounted read-only once, stacked in order |
| `vm_layered` | `gpt_over_scratch_maps_the_table` | The mapping table over a GPT disk, whose targets the worker creates, with their parents |
| `vm_layered` | `gpt_over_scratch_with_untrusted_lowers` | A GPT disk over a scratch image in the untrusted-lowers mode |
| `vm_layered` | `invalid_configuration_fails_its_stage` | A relative runtime directory fails the configuration's stage, and the VM ends with 11 |
| `vm_layered` | `other_disk_guid_fails_the_assembly` | A GPT disk of another GUID than the launch expects fails the assembly, and the VM ends with 14 |
| `vm_layered` | `other_identity_fails_the_assembly` | An image of another identity than the launch expects fails the assembly, and the VM ends with 14 |
| `vm_layered` | `outbound_host_loopback_publishes_no_listener` | Egress with outbound host-loopback access reaches every destination, and publishes no listener of the guest |
| `vm_layered` | `published_listeners_refuse_binding_and_raw_sockets` | While listeners are published, the guest refuses `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` with `EPERM`, and keeps the other capabilities of its allowlist |
| `vm_layered` | `recovery_required_layer_fails_the_assembly` | A layer whose journal needs recovery is refused before any mount, whatever `noload` says, and the VM ends with 14 |
| `vm_layered` | `root_profiles_enter_the_root` | Executions enter the root as PID 1 of their namespaces, with their standard streams alone, no inheritable or ambient capability, and no new privileges |
| `vm_layered` | `unstaged_images_are_refused_before_spawn` | Images that the store does not stage are refused before the VM spawns, with nothing of a launch left, and boot once staged |
| `vm_template` | `a_class_that_no_capture_takes_is_refused_before_the_store` | `create` of a class that no capture takes, of 3 processors, or of more online at boot than in all, fails with status 1 before the store exists |
| `vm_template` | `a_network_that_lets_nothing_through_has_no_nic` | `create` of a network that lets nothing through captures a template without a NIC |
| `vm_template` | `a_refresh_captures_and_retires_the_generations_that_it_supersedes` | `refresh` captures a new generation although the store holds an eligible one, and its eviction retires the one that it supersedes, or tombstones it while a clone leases it, as `list` and `inspect` report |
| `vm_template` | `a_refresh_refuses_a_class_that_no_capture_takes_before_the_store` | `refresh` of a class that no capture takes fails with status 1 before the store exists |
| `vm_template` | `a_removal_reports_busy_while_leases_keep_the_generation` | `remove` tombstones a leased generation, keeps its payload, and reports busy, with status 7 and its leases, until no lease remains, then deletes it |
| `vm_template` | `a_template_whose_processors_are_all_online_at_boot_limits_none` | `create` captures a template whose processors are all online at boot without `maxcpus=`, and whose key records none online at boot |
| `vm_template` | `a_template_without_a_network_has_no_nic` | `create` captures a template without a NIC, whose key holds what it asked for, its processor online at boot among it, and serves it again without a capture |
| `vm_template` | `the_networks_of_every_policy_at_one_guest_s_address_share_one_template` | `create` of a network of any policy captures one template, whose NIC the capture attaches under the complete deny-all policy, and the creations of the networks of every other policy at the same guest's address serve it without a capture |
| `vm_clone` | `a_clone_binds_the_resolver_of_its_policy_s_dns_server_read_only` | A clone of a network with egress allowed binds a resolver file that names the gateway's DNS server read-only at `/etc/resolv.conf`, and a TCP connection to the server's DNS port opens, which the gateway's DNS proxy accepts; one whose resolver cannot be bound ends with status 57 |
| `vm_clone` | `a_clone_maps_an_aggregate_of_directories_and_files_beneath_its_root_before_its_sandbox_starts` | A clone maps an aggregate of host directories and files beneath its root before its sandbox starts, each with its access, a hidden file, and a hidden directory that shows a file again, and writes reach the host's directories and file through the writable ones alone |
| `vm_clone` | `a_clone_of_a_template_with_a_nic_maps_its_export_beside_its_resolver` | A clone of a template with a NIC maps its export below the binds of its machine ID and of its resolver configuration |
| `vm_clone` | `a_clone_reads_no_more_of_its_disks_than_its_root_needs` | A clone's restore reads less than an eighth of each disk, before the guest reads each to its end |
| `vm_clone` | `a_clone_refuses_a_scratch_image_whose_upper_an_earlier_clone_used` | A scratch image whose upper an earlier clone used ends the clone before it is ready, with status 54 |
| `vm_clone` | `a_clone_runs_its_executions_as_its_workload_identity_once_it_is_ready` | A clone runs its executions as its record's workload identity once it is ready, and refuses one that names an identity with `EINVAL` |
| `vm_clone` | `a_clone_s_executions_find_the_proxy_of_its_policy` | The executions of a clone of a proxy-only network find its proxy's URL in their environment, and the clone keeps the image's own `/etc/resolv.conf` |
| `vm_clone` | `a_clone_s_time_settles_after_it_is_ready` | A clone's time settles once it is ready, without a violation, after the downtime that the host's monotonic clock measured |
| `vm_clone` | `a_clone_whose_activation_fails_ends_before_it_is_ready` | A clone whose activation fails ends before it is ready, with the status of the failure's class and the failure's record, quarantines nothing, and releases its lease |
| `vm_clone` | `a_clone_whose_mapping_fails_ends_before_its_sandbox_starts` | A clone whose guest fails a mapping ends before its sandbox starts, with the failure's record, and leaves the host's directory unchanged |
| `vm_clone` | `a_clone_whose_view_cannot_be_set_ends_before_it_is_ready` | A generated machine ID that no bind can hold ends the clone before it is ready, with status 55, while the image's own needs no bind |
| `vm_clone` | `a_clone_whose_vm_exits_once_a_stop_found_it_fails_before_its_lease_is_released` | A clone whose VM exits once a stop found it running fails, with status 8 and the record of a failure by the evidence of its lost status, quarantines nothing, and releases its lease |
| `vm_clone` | `a_forward_whose_port_another_socket_holds_fails_the_clone_as_the_host_s` | A forward whose port of host loopback another socket holds ends the clone before its restore-ready event, with status 8 and the record of a failure of the host, by the evidence of the forward's bind, and the template serves the network once the port is free |
| `vm_clone` | `a_generation_is_eligible_by_the_clock_of_its_capture` | The store checks eligibility by the host's monotonic clock on the boot of the capture, and by UTC after a reboot, and refuses an expired or an early generation with status 10, without a lease |
| `vm_clone` | `a_generation_refuses_clones_of_fewer_processors_than_its_template_booted_with` | NVX's plan refuses a processor target below the template's processors online at boot before any VMM starts, and the generation serves its clones from those up |
| `vm_clone` | `a_host_program_of_another_runtime_ends_the_clone_through_its_verified_identity` | A host program of another runtime ends the clone through its verified identity, without a word to its guest, with status 8, and releases its lease |
| `vm_clone` | `a_run_in_a_template_of_a_network_finds_its_policy_s_proxy` | A run's execution, in a clone of a proxy-only template, finds the proxy's URL in its environment, and a run of a network that lets nothing through runs nothing, with status 8 |
| `vm_clone` | `a_run_runs_one_execution_in_its_clone_then_tears_the_clone_down` | A run stages its images, starts a clone, runs one execution in its root, with the execution's environment and working directory, then stops the clone and releases its lease, whatever the execution did |
| `vm_clone` | `a_run_s_execution_finds_its_shares_beneath_the_root` | A run's execution reads and writes through its share, which the clone mapped beneath its root, and the host's directory keeps what it wrote |
| `vm_clone` | `a_run_whose_clone_does_not_start_runs_nothing` | A run whose selection the store refuses ends with status 10, and one whose clone fails with status 8, without an execution or a stop |
| `vm_clone` | `a_run_whose_clone_s_vm_exits_during_its_execution_ends_with_the_clone_s_failure` | A run whose clone's VM exits during its execution ends with the clone's failure, status 8, once its stop found that the VM exited, quarantines nothing, and releases its lease |
| `vm_clone` | `a_selection_that_names_no_template_leases_nothing` | A start of a template store that does not exist, or of a generation that `remove` retired, ends with status 10 and the refusal's record, without a lease or a VM |
| `vm_clone` | `a_start_refuses_an_export_whose_mapped_objects_changed_before_any_vmm_starts` | An export whose mapped file, or pinned directory, another object replaced once NVX's plan checked the clone is refused with status 8 at the clone's launch, before any VMM starts |
| `vm_clone` | `a_start_refuses_what_the_template_cannot_serve_before_any_vmm_starts` | Images, scratch images, and memory targets that the template cannot serve are refused with status 8 before any VMM starts, unstaged images with status 1, and a generation that the store does not hold with status 10 |
| `vm_clone` | `a_template_without_a_nic_serves_clones_without_a_network` | A template without a NIC serves clones without a network, or with one that lets nothing through, and refuses those of a network with a NIC with status 8 |
| `vm_clone` | `a_tombstone_leaves_the_running_clone_of_its_generation_to_its_lease` | A tombstone refuses new clones of its generation with status 10, while the running clone serves on, and releases its lease once it stops |
| `vm_clone` | `clones_at_once_keep_their_own_uppers_and_machine_ids` | Clones at once keep their own uppers, and their own machine IDs, generated, distinct, and bound read-only, or the image's own |
| `vm_clone` | `clones_at_once_see_only_their_own_exports` | Clones of one generation at once see only their own exports, and a clone without one none |
| `vm_clone` | `each_command_reattaches_to_the_clone_from_a_new_process` | `reattach`, `probe`, and `exec` each reattach to the clone from a new process, in a later epoch, without a second activation |
| `vm_clone` | `one_generation_serves_clones_of_each_processor_count_from_its_boot_online_count_to_its_capacity` | One generation serves clones of each supported processor count from the template's processors online at boot to its capacity, whose repair brings the target's CPUs online, and NVX's plan refuses an unsupported count and one beyond the capacity before any VMM starts |
| `vm_clone` | `one_generation_serves_clones_of_each_upper_and_memory_target` | One generation serves clones over a scratch image and in RAM, at each memory target up to its capacity, each with its own lease, and leaves its snapshot unchanged |
| `vm_clone` | `one_generation_serves_clones_of_either_format_over_either_upper` | A template of a GPT placeholder serves EROFS and GPT clones of other layouts, shorter than the placeholder, at its length, over either upper, those in RAM with no scratch file, and a first partition over its bound ends a clone with status 54 |
| `vm_clone` | `one_template_serves_the_clones_of_every_network_policy_at_its_guest_s_address` | One template with a NIC serves the clones of every network policy at its guest's address, whose restores install their own policies on its NIC, which lets through what each allows, publishes the guest's listeners, through the clone's announcer, and forwards ports where it says, and NVX's plan refuses another address, or no NIC, with status 8 before any VMM starts |
| `vm_clone` | `the_lease_of_a_start_that_ended_before_its_record_is_released_through_its_template_store` | The lease of a start that ended before it recorded its clone is released only through its own template store, which `--template` names |

### Running the VM tests

The VM tests, `tests/vm_restart.rs`, `tests/vm_steps.rs`,
`tests/vm_layered.rs`, `tests/vm_template.rs`, and `tests/vm_clone.rs`, share
their sandboxes and
the fixtures of their layered roots through `tests/common`. The sandboxes start
the host program's commands with `nvx::host::supervisor::spawn_command`, so
that on Windows a command cannot inherit the handles of a VM that a test
launches in its own process meanwhile, such as its log and its capability's
pipe, which it would hold until it exits. A test that fails
ends the VM that it left running, and the host program that it left paused,
and keeps the sandbox's directory, whose path it prints, unless it can confirm
that the VM ended. The tests boot VMs, so they are ignored. They read the
hypervisor from `NVX_HYPERVISOR`, through
`nvx::Hypervisor::from_env_or_default`, NVX's artifacts, with their install
metadata and the fixtures, from `NVX_ARTIFACTS_DIR`, and the guest agent from
`RUNTIME_ADAPTER_GUEST`. In a checkout where `nvx-rs download` installed NVX's
artifacts, the scratch template among them, run them in this directory with:

```bash
cargo build --locked --release --features guest \
  --target x86_64-unknown-linux-musl --bin runtime-adapter-guest
NVX_HYPERVISOR=kvm NVX_ARTIFACTS_DIR="$(cd ../.. && pwd)" \
  RUNTIME_ADAPTER_GUEST="$PWD/target/x86_64-unknown-linux-musl/release/runtime-adapter-guest" \
  cargo test --locked --features host -- --ignored
```

On Windows, take the guest agent from a Linux build. To run the
[stage scenarios](#stage-scenarios) by name, as CI runs them, run in the
repository's root, after the build of the guest agent above, or with
`--guest-agent PATH` naming another:

```bash
python3 scripts/nvx.py test-runtime-adapter --backend kvm
```

Run the checks of each configuration in this directory. The guest-only
configuration needs Linux and the musl target, which
`rustup target add x86_64-unknown-linux-musl` installs:

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
cargo clippy --locked --all-targets --features host -- -D warnings
cargo test --locked --features host
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --features host
cargo clippy --locked --all-targets --features guest \
  --target x86_64-unknown-linux-musl -- -D warnings
cargo test --locked --features guest --target x86_64-unknown-linux-musl
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --features guest \
  --target x86_64-unknown-linux-musl
```

## CI

The [`check-nvx-rs`](../../.github/actions/check-nvx-rs/action.yml) action
runs these checks with `cargo build`, and `cargo check` with the minimum
supported Rust version, on Linux and Windows, where it skips the guest-only
configuration. A change to the adapter is a test input of CI's
[change classification](../../doc/ci.md#change-classification), so it runs the
NVX microVM tests. Their
[workflow](../../.github/workflows/run-nvx-microvm-tests.yml) builds the
host-only configuration on each backend, in the jobs that run the guest and
sandbox steps, and runs the adapter's consolidated suite, the
[stage scenarios](#stage-scenarios), by name, with
`nvx.py test-runtime-adapter`, which names the backend in `NVX_HYPERVISOR`, the
checkout in `NVX_ARTIFACTS_DIR`, into which the job downloaded NVX's
artifacts, their provenance, and the fixtures of the layered scenarios with
the guest artifacts, as NVX's own layered scenarios take them, and the guest
agent in `RUNTIME_ADAPTER_GUEST`. The Linux jobs build the guest agent
first, and the Windows job takes the one that CI's `distro-artifacts` job
builds on Linux.
