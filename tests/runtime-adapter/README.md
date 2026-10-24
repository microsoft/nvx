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
host program may share host directories and files with the root, which it
exports with NVX's `filesystem::plan`, and which the worker maps beneath the
root from the host's mapping table, before the host starts the sandbox. The host
program then checks the root from inside the VM with the worker's probes, and
runs executions in it, several at once over one session, in a
[table of scenarios](#layered-scenarios), to which each new check adds a row.

## Configurations

Each configuration enables one side of `nvx`:

| Configuration | Cargo options | `nvx` features | Modules | Binary |
| --- | --- | --- | --- | --- |
| Protocol-only | none | none | `frame`, `root`, `probe`, `exec` | none |
| Host-only | `--features host` | `host` | `frame`, `root`, `probe`, `exec`, `host`, `runtime`, `initramfs`, `layered`, `hook` | `runtime-adapter-host` |
| Guest-only | `--features guest --target x86_64-unknown-linux-musl` | `guest` | `frame`, `root`, `probe`, `exec`, `guest`, `worker` | `runtime-adapter-guest`, a static executable |

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
| 7 | `Ready`: the guest assembled the root | the guest agent's [build](#ready-mapping-tables-and-start), at most 4 KiB |
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

The session keeps no message boundaries, so a message may span several of its
records, and one record may end a message and start the next. `Decoder`
reassembles the messages. It checks each header before it takes any byte of
the body, and holds at most one partial message. A malformed message ends the
stream, because the framing cannot find the next message after it.

The framing carries each body as bytes, within its kind's limit. The bodies of
kinds 6 to 22 and 24 have encodings of their own, which the `root`, `probe`,
and `exec` modules write and read. A body that does not decode leaves the
stream intact, at the start of the next message. The guest answers a root
configuration that does not decode with `RootFailed`, as a configuration that
it refuses, and a probe with `ProbeFailed`, while a message about an execution
that does not decode ends the worker's session, as one that breaks the
execution's rules does. The host program's client ends on an answer that does
not decode, since it cannot tell what the guest did.

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
host's mapping table has no mapping. The host program and the guest agent ship
together, so the guest refuses every other configuration, one with an unknown
field included, rather than ignore what it does not know.
`RootConfig::default()` holds the values that the host program sends unless its
caller names others: the runtime directory `/run/runtime-adapter`, the cgroups
`/sys/fs/cgroup/agent` and `/sys/fs/cgroup/workload`, a wait of 30 seconds, an
agent's `memory.low` of 16 MiB, as NVX's own guest reserves, and a workload's
`memory.max` of 128 MiB and `pids.max` of 128, with neither a bound of the
first partition, an overlay mode, a journal policy, nor a workload identity, and no
mapping.

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

The host's mapping table, of as many mappings as the root configuration
announces, travels in parts, each in a `Maps`, whose body is a `MAPS` payload
of `nvx::protocol`, as its `encode_maps_payloads` writes it: the index in the
table of the part's first mapping, then the number of its mappings, each a
little-endian `u32`, then each mapping: its flags, a little-endian `u16` that
is 1 for a read-only mapping and 0 otherwise, then the lengths of its source
and its target, each a little-endian `u16`, then the source, a path relative
to the guest's mount of the host's export, and the target, an absolute path
in the root. `root::encode_maps` splits a table into parts of at most 256 KiB,
and an empty table into none. `root::decode_maps` reads a part with
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
| `first-partition` | The bytes of the first partition of a GPT disk that the assembly returned, without their trailing padding: how many, `length`, and the bytes, `bytes`, in hexadecimal, or `none` for both for an EROFS image |
| `cgroups` | The file system at the cgroups' parent, `parent.type`, and its `parent.subtree-control`; whether each cgroup exists, `agent.exists` and `workload.exists`; `agent.memory.low`, `workload.memory.max`, and `workload.pids.max`; the number of processes in each, `agent.processes` and `workload.processes`; and `user-namespaces`, the guest's limit of user namespaces, or `unavailable` without them |
| `processes` | For PID 1, `pid1`, the worker, `worker`, and PID 1's unrelated child, `sibling`: `NAME.pid`, and while the process exists, `NAME.state`, `NAME.ppid`, `NAME.root`, `NAME.cgroup`, `NAME.command`, and `NAME.namespaces`: `same` if it shares PID 1's, or the ones that differ |
| `hold` | It opens the root, and keeps it open until the worker ends, so that the teardown finds the root busy: `held`, the path that it holds |

A value that a probe cannot read is `absent` if its file does not exist, and
otherwise `unreadable: PROBLEM`. A new check of a root is a new probe in this
table.

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
its `deadline`, the peer's next timer, passes. The host program's
[driver](#host-program) and the tests of the worker run it.

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
`worker::EntryOperations`, so that tests run it against a fake root.

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
  the root moved over `/`, the entry's default hostname, `nvx-sandbox`, the
  image's own machine ID, and no time status, so that the entry binds nothing
  into the root, and with published listeners, the entry's default, until the
  adapter tells whether its listeners are published. It runs as the identity
  that exactly one source names: the sandbox's workload identity, with no
  supplementary groups or capabilities, or, in a sandbox without one, the
  identity that its start names, with its supplementary groups, its
  capabilities, and its account mode. A start that names an identity in a
  sandbox with a workload identity, or none in a sandbox without one, is refused
  with `EINVAL`. Before the worker prepares the execution's cgroup, the entry
  checks the identity, and reads its account in existing-account mode: it
  refuses with `EPERM` a capability outside its allowlist, any capability of a
  user ID other than 0, and `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` while
  listeners are published, with `ENOENT` an identity without the user ID's
  passwd entry or the group ID's group entry, with the error of an account
  database or a root that it cannot read, and with `EINVAL` otherwise. The
  program starts in the working directory of its start, or in the root's `/`,
  with exactly the environment of its start, none of the worker's own, and with
  its standard input from the host or the null device. It leads a session of its
  own, without a controlling terminal, and the process group that the session
  creates, and, as PID 1 of its PID namespace, its end ends every process of the
  namespace.
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
  of a layered root's teardown, which the guest reports before it confirms;
- `configure_root` sends a root configuration, and returns how the guest
  answered: `Ready`, with the guest agent's build, or the stage of the root
  that failed;
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
sends, an execution's start, input, end of input, or cancellation included,
end the client, and its later calls fail. A receive that times out doesn't: it
keeps what arrived. A request ends the client on any failure after the framing
accepted it, a timeout included, because an answer that arrived later would
answer the wrong request.

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
attachment of its own, as each command of the host program does.

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
checkout's `build/`, where `nvx.py download` also installs a release's.

The VMs boot a copy of NVX's initramfs with the guest agent in it, so the
identity of their initramfs names both:
`sha256:DIGEST+runtime-adapter-guest/BUILD`. The guest agent has no install
metadata of its own, so the caller names its build, BUILD, which is the crate's
version unless the caller names another. A host program that ships another
build of the guest agent runs another runtime, and finds a VM of this one
mismatched.

### Staged images

The images of a layered root are staged first, in the sandbox's store, as
`layered::Staged::stage` stages them. Staging verifies the image explicitly,
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
as block devices, then those of the export of the launch's
[shares](#shares), if any, then the VM in one process, NVX's default memory,
the kernel, the initramfs, that command line, no virtio console, the control
console with the launch capability on standard input, and the outcome report.
They name no workload identity, and no managed lifecycle: OpenVMM only checks
the arguments of the managed lifecycle and labels its outcome report with it,
and only NVX's init agent reads its boot token. A VM without a layered root
keeps the managed lifecycle, with which NVX's `/init` starts the managed agent,
which the guest agent replaces.

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
{"format": 2, "mode": "layered", "kernelCommandLine": "nvx_sandbox=1 ...",
 "plan": PLAN, "bindings": {"distro": DISTRO_BINDING, "scratch": SCRATCH_BINDING},
 "rootConfiguration": "version=1\nruntime=/run/runtime-adapter\n..."}
```

`mode` is `layered`, or `blockless` for a VM without a layered root, whose
`plan`, `bindings`, and `rootConfiguration` are `null`. `plan` is the persisted
form of the VM's `SandboxPlan`. `bindings` holds the bindings that the launch
checked, which the store [stages](#staged-images) for the launch's images: the
image's, and the scratch image's, `null` for an upper in RAM.
`rootConfiguration` is the root configuration that the launch sends, whether
or not the guest accepts it.

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
the scratch image's claim. The launch holds the checked files until the guest
has assembled the root, or until it has torn down a launch that failed.

### Shares

A launch may share host directories and regular files with the root, as
`layered::Shares`: each `layered::Share` at an absolute guest path in the root,
read-only or read-write. `Shares::plan` plans their export with NVX's
`nvx::host::filesystem::plan`, through an `ExportRequest` of a mapping for each
share at its target, the host paths that it hides inside the shares, and the
guest targets that the root keeps: those of NVX's container root,
`ReservedTargets::CONTAINER_ROOT`, and the trees that the adapter keeps for
itself. Each share must lie inside a host directory that the adapter
authorizes, once links are resolved: `Shares::plan` refuses one outside them
before it plans, and checks the export's directories and files, which the
planner resolves anew, again. The planner refuses what it cannot export, such
as a target that takes or covers a reserved tree, or a read-only share inside
a read-write one, which the host could not keep read-only, and a hidden
path that does not exist inside a share, which nothing would hide once a
workload creates it.

`LayeredLaunch::share` attaches the export to the launch, and announces its
table, of at most 4,096 mappings, in the root configuration. NVX's adapter plan
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
before those inside its target.

### The step hook

For tests, `launch` takes a step hook, `hook::StepHook`, from the variable
`RUNTIME_ADAPTER_STEP_HOOK`, which pauses the launch at a named step, so that a
test can act between two steps, or exits the launch there without unwinding,
as a crash would end it:

- `STEP:pause:PATH` creates the file PATH, which holds the step's name, and
  waits until the file is gone, then goes on: the launch leaves the time that
  it waited out of its deadline, so that a test may pause it for as long as it
  needs;
- `STEP:exit` exits at once with status 5: the launch ends nothing that it
  started, such as the VMM, and the system releases what it holds, such as its
  descriptors or handles, and their locks.

The steps of a launch, in order, are:

| Step | Where the launch is |
| --- | --- |
| `checked` | It has checked the bindings of its images, and the export of its shares, if any, and holds the checked files, which claim the scratch image, but has begun nothing in the store, and spawned nothing. A launch without a layered root reaches it too. |
| `spawned` | The VMM runs, and the launch's journal records it, but the launch has not connected to the VM. |
| `ready` | The guest agent, of the build that the host ships, has assembled the layered root, and the launch has released the checked files, but has sent neither the mapping table nor `Start`. |
| `commit` | The guest has started the sandbox, if the VM has a layered root, the guest agent has answered, and the launch is about to commit: its journal still records the VMM, which no runtime record names yet. |

A value that names no step or action is a usage error. Without the variable,
the hook does nothing.

### Commands

Each command of `runtime-adapter-host` but `stage` takes NVX's artifacts from
the directory that `NVX_ARTIFACTS_DIR` names, and the guest agent's build from
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
  `--distro FORMAT:PATH --distro-identity UUID --upper (scratch:PATH | ram) [--runtime-dir DIR] [--overlay-mode MODE] [--scratch-journal POLICY] [--first-partition-bound BYTES] [--workload-identity UID:GID] [SHARES]`,
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
  the assembled root, without which the root's `identity` stage fails. Before it
  spawns the VM, the launch checks the bindings that STORE stages for the image
  and the scratch image, and fails if STORE stages none for either, or one no
  longer holds, as [the checked handoff](#the-checked-handoff) describes. It
  records the bindings that it checked.

  SHARES,
  `--share HOST:TARGET[:ro]... [--deny PATH]... [--reserved TARGET]... [--share-root DIR]...`,
  shares each host directory or regular file HOST with the root, at the
  absolute guest path TARGET, which starts at the last `:/`, read-only with
  `:ro`, as [Shares](#shares) describes. Each HOST must lie inside a host
  directory DIR that the adapter authorizes, the export hides each host path
  PATH inside the shares, and no TARGET may take or cover a guest tree that the
  container root keeps, or a tree TARGET that the adapter keeps. The launch
  refuses shares that it cannot export before it touches STORE, and checks the
  export before it spawns the VM.

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
- `reattach STORE` takes the sandbox's lock, and reattaches to its VM, which
  another process launched, and which it holds, although the VM is not its
  child, through its verified process identity. It reads the capability from
  the store, attaches a new session, in the session's next epoch, exchanges a
  `Ping`, and exits, leaving the VM running.
- `stop STORE` reattaches and attaches as `reattach` does, then stops the VM:
  the adapter's `Stop`, which the guest agent confirms with `Stopped` before it
  ends the VM, is the stop's graceful step. It waits for the VM to exit, and
  checks the VM's outcome report: a success, with status 0 and a complete
  teardown. A VM whose guest agent does not confirm the stop is terminated, and
  the stop fails. The worker of a layered root tears the root down before it
  confirms, and reports a teardown that failed first: the stop then checks
  that the VM ended with the teardown's status instead.
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
  DIR, or `/`, with exactly the environment of its `--env` entries. It runs for
  at most a minute, with 16 KiB chunks and the credit of 4 of them for its
  input and 8 for its output, which its standard output and error share.
  Execution N, from 1, has the key `(B + N, E)`: E is the epoch of the
  command's attachment, and B the first 8 bytes of the broker instance's ID, as
  a little-endian number. The guest refuses a key that it heard of before, and
  the epochs of a broker instance only advance, while a broker restored from a
  snapshot has a new instance ID, so no command repeats the key of another's
  execution.

`reattach`, `stop`, `probe`, and `exec` find a VM that another runtime identity
launched mismatched, and then signal nothing: the VM keeps running. The commands
print what they find as `KEY=VALUE` lines: `reattached` (`live`, `mismatched`,
`exited`, or `indeterminate`), `differences` (the parts of the runtime identity
in which a mismatched VM differs), `pid` (the VMM's process ID), `instance` and
`epoch` (the identity of the session that they attached to), `root` (`ready` or
`failed`: how the guest answered the root configuration, or `failed` when a stop
finds that the root's teardown failed), `build` (the build of the guest agent
that assembled the root), `stage` and `problem` (the stage of the root that
failed, such as `assembly`, and the problem, or the problem of a mapping that
failed), `table` (`applied` or `failed`: how the guest took the mapping table),
`mapped` (how many of the table's mappings the guest applied), `sandbox`
(`started` once the guest started the sandbox), `PROBE.KEY` (the fact KEY that
the probe named PROBE found), `exec.N.KEY` (what execution N did: its `launch`,
`started`, `failed STAGE ERRNO`, or `none`; its `status`, `exit CODE`,
`signal NUMBER`, or `none`; the `cause` for which a side ended it, such as
`timed-out`, or `none`; the streams that it `truncated`, such as
`stdout,stderr`, or `none`; whether it `settled`, `reusable` or `fenced`; and
its `stdout` and `stderr`), `stopped` (`graceful`, `forced`, or `timed-out`),
`outcome` (such as `success 0`, or `guest-exit 14` once the assembly failed),
and `teardown` (`complete` or `incomplete`: whether the VMM's teardown of the VM
completed, which the outcome report records apart from the root's). Values
escape `\`, newlines, and carriage returns as `\\`, `\n`, and `\r`, and each
byte of a program's output that is not part of UTF-8 text as `\xHH`. The
commands exit with 0 once they did what they were asked, `exec` once every
execution finished, whatever its outcome, with 3 when they find the VM
mismatched, with 4 when the guest reported that a stage of the root failed, and
the VM ended with the stage's status and a complete teardown, with 5 when the
[step hook](#the-step-hook) ended a launch, with 6 when the guest failed a
mapping of the table, and the VM stopped gracefully, with a success and a
complete teardown, and with 1, after a diagnostic on standard error, when they
fail. Without arguments, the program reports the contract that it was built
against.

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

The unit tests of `frame`, `root`, `probe`, and `exec` run in every
configuration. They cover each kind's encoding, messages in pieces of any size,
a message of each kind split at every byte, the headers that the framing refuses
before their body arrives, and the root configurations, readinesses, parts of
mapping tables, failures, probes, facts, and messages of executions that decode,
and those that don't, and the host's side of an execution, which still sends the
cancellation of one that it gives up on in the step that ends it, without grace
or settlement time. In the guest-only configuration, the unit tests of
`worker::probes` cover what the probes read: the mount table with its escapes,
the kernel's records of a process, `os-release`, and the guest's memory, and the
probes and arguments that the worker refuses. Those of
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
probe's arguments.

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
  body arrives, and messages that only the host program sends; and a message
  beyond the framing's limit, which it refuses to send;
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
  guest that stopped reading, which ends it.

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
and the messages that it refuses; a second configuration and a broker that
closes the stream, which leave the root that it assembled to its operations;
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

`tests/layered_launch.rs` checks, in the host-only configuration, on Linux and
Windows:

- the adapter's plan of a layered launch: its block devices and boot tokens,
  with none of the sandbox profile's settings, and the sizes of the images;
  and the options that it passes on as they are, for the guest to check;
- the checked handoff: OpenVMM's arguments, which name the checked files, on
  Linux by the descriptors that OpenVMM inherits and on Windows by their final
  paths, with no workload identity and no managed lifecycle, and what OpenVMM
  inherits, over a scratch image and over RAM;
- staging: an image without a manifest, one whose manifest matches it, and
  manifests that name another image or digest, which fail; a GPT disk over
  RAM; and images that no launch could attach;
- the bindings that a launch takes from the staged images, and those of other
  images, which it does not;
- the launches that the checks refuse: of images that the store does not
  stage, of another format or identity than the staged image's, and of an
  image that changed, a scratch image that another file replaced, and an image
  that is gone, after they were staged;
- the scratch image's claim, which a second launch waits for, then fails to
  take, until the first one releases its checked files;
- the record of the staged images, as written and read, and the forms that a
  reader refuses;
- the record of a launch, with the bindings that it checked, over a scratch
  image and over RAM, and of a VM without a layered root;
- shares: their text, `HOST:TARGET[:ro]`; the export of a table of shares
  inside an authorized directory, which maps each at its target, parents
  first, hides a denied path, and narrows the writes of a read-only directory
  to the read-write share inside it; the root configuration that announces the
  table, OpenVMM's arguments that attach the export, a launch that shares
  nothing, the export that the launch verifies before it prepares anything,
  and a table that the guest would refuse;
- the shares that a launch refuses: outside the authorized directories, also
  through a link on Linux, without any, with an authorized directory that is
  not one, or a share that is not accessible; reserved trees that are not
  canonical guest paths, targets that take or cover the adapter's or the
  container root's reserved targets, a read-only share inside a read-write
  one, and a hidden path that does not exist inside a share;
- `runtime-adapter-host stage`, which records the staged images, a staging that
  fails, which leaves them, and a staging that replaces them;
- `runtime-adapter-host launch` with shares that it cannot export, which it
  refuses with status 1 before it creates the store, with artifacts that boot
  nothing;
- the command lines that the host program refuses, each with status 2, those of
  `exec` and of `--workload-identity` included.

`tests/step_hook.rs` checks, in the host-only configuration, on Linux and
Windows, the [step hook](#the-step-hook): the values of
`RUNTIME_ADAPTER_STEP_HOOK` that set it, with each step and action, a path with
colons, and those that set none, which the host program refuses with status
2; and a pause, which does nothing, and pauses for no time, at another step,
creates its file with its step's name at its own, waits until the file is
gone, and says how long it paused, and which a file that is there already
fails.

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
- a launch paused at each step in turn, at which the store holds what the step
  says, and the scratch image's claim is held at `checked`, and which then
  goes on, and is stopped.

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
| `executions_run_in_the_root` | The image over the scratch, without a workload identity, in which `exec` runs executions that name their identities | Over one session, four executions as `nobody` that meet only while all of them run, beside one as root that writes more than a MiB to its standard output and a little to its standard error, each byte-exact, ending on its own with 0, and reusable. Over a later session, with new keys: a program that leaves a descendant in a session of its own, which the program's end, as PID 1 of its namespace, ends, so that no output is lost; `nobody`'s IDs and groups, with empty permitted, effective, and bounding sets; root with `CAP_CHOWN` alone in each of them; exactly the start's environment; the start's working directory; the configured hostname, `nvx-sandbox`, and PID 1; and a start without an identity, and one with `CAP_SYS_ADMIN`, which the guest refuses at stage 0 with `EINVAL` and `EPERM`, fenced and without output |
| `executions_run_as_the_workload_identity` | The image over the scratch, with the workload identity of `nobody` | Executions that name no identity, which run as `nobody`, without capabilities, with exactly the start's environment, and a start that names an identity, which the guest refuses at stage 0 with `EINVAL` |
| `accountless_workload_identity_fails_its_stage` | The image over the scratch, with a workload identity whose account the image lacks | The identity stage's failure once the guest assembled the root, which the launch reports, and a VM that ends with 16, which unmounted the scratch |

A row names the root, the image, the GPT disk's top-layer variant, the
identity that the launch expects of the image, the upper, the runtime
directory, the overlay mode, the scratch's journal policy, the bound of the
first partition, the sandbox's workload identity, and the host paths that the
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

The scenarios use the fixtures of NVX's own layered scenarios, from NVX's
artifacts: the Ubuntu EROFS image, `ubuntu-distro.erofs`, its manifest, which
holds its UUID, and the scratch template, `ubuntu-smoke-scratch.ext4`, an
empty ext4 file system that the guest-artifact build makes, of which each
scenario boots a copy. Downloaded packages do not include the template, so make
it beside the image as the guest-artifact build does:

```bash
truncate --size 64M ../../build/ubuntu-smoke-scratch.ext4
mkfs.ext4 -q -F -m 0 -U 4f3c8bf7-1959-47b4-8811-62b17bbddaf4 \
  -E lazy_itable_init=0,lazy_journal_init=0 ../../build/ubuntu-smoke-scratch.ext4
```

A scenario of a GPT disk writes the disk from the template first: a protective
MBR, and the primary and backup tables of 128 entries, with the first
partition, 4 KiB of bytes that are not UTF-8, then padding; the base layer, in
partition 2, a copy of the template; and the top layer, in partition 3, a small
file system in the ext2 format, which the kernel's ext4 driver mounts, with a
file and a directory. The test writes the top layer itself, since `mkfs.ext4`
is not available everywhere that the tests run, and checks the disk with
`nvx::gpt` before it launches the VM. For `corrupt_layer_fails_the_assembly`,
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

### Running the VM tests

The VM tests, `tests/vm_restart.rs`, `tests/vm_steps.rs`, and
`tests/vm_layered.rs`, share their sandboxes and the fixtures of their layered
roots through `tests/common`. A test that fails ends the VM that it left
running, and the host program that it left paused, and keeps the sandbox's
directory, whose path it prints, unless it can confirm that the VM ended. The
tests boot VMs, so they are ignored. They read the hypervisor from
`NVX_HYPERVISOR`, through `nvx::Hypervisor::from_env_or_default`, NVX's
artifacts, with their install metadata and the fixtures, from
`NVX_ARTIFACTS_DIR`, and the guest agent from `RUNTIME_ADAPTER_GUEST`. In a
checkout where `python3 scripts/nvx.py download` installed NVX's artifacts,
and with the scratch template, run them in this directory with:

```bash
cargo build --locked --release --features guest \
  --target x86_64-unknown-linux-musl --bin runtime-adapter-guest
NVX_HYPERVISOR=kvm NVX_ARTIFACTS_DIR="$(cd ../.. && pwd)" \
  RUNTIME_ADAPTER_GUEST="$PWD/target/x86_64-unknown-linux-musl/release/runtime-adapter-guest" \
  cargo test --locked --features host -- --ignored
```

On Windows, take the guest agent from a Linux build.

Run the checks of each configuration in this directory. The guest-only
configuration needs Linux and the musl target, which
`rustup target add x86_64-unknown-linux-musl` installs:

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo clippy --locked --all-targets --features host -- -D warnings
cargo test --locked --features host
cargo clippy --locked --all-targets --features guest \
  --target x86_64-unknown-linux-musl -- -D warnings
cargo test --locked --features guest --target x86_64-unknown-linux-musl
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
sandbox steps, and runs its ignored tests, the adapter's VM scenarios, with
`NVX_HYPERVISOR` naming the backend, `NVX_ARTIFACTS_DIR` the checkout, into
which the job downloaded NVX's artifacts, their provenance, and the fixtures
of the layered scenarios with the guest artifacts, as NVX's own layered
scenarios take them, and `RUNTIME_ADAPTER_GUEST` the guest agent. The Linux
jobs build the guest agent first, and the Windows job takes the one that CI's
`distro-artifacts` job builds on Linux.
