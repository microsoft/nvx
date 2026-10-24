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

A VM may have a layered root: an image under an upper, which the guest agent's
worker assembles through `nvx::guest::root`, in a process of its own rather
than PID 1, as the host program's root configuration asks. The host program
then checks the root from inside the VM with the worker's probes, in a
[table of scenarios](#layered-scenarios), to which each new check adds a row.

## Configurations

Each configuration enables one side of `nvx`:

| Configuration | Cargo options | `nvx` features | Modules | Binary |
| --- | --- | --- | --- | --- |
| Protocol-only | none | none | `frame`, `root`, `probe`, `exec` | none |
| Host-only | `--features host` | `host` | `frame`, `root`, `probe`, `exec`, `host`, `runtime`, `initramfs`, `layered` | `runtime-adapter-host` |
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
| 6 | `RootConfig`: the host's [root configuration](#root-configurations) of a layered root, which the guest answers with `RootReady` or `RootFailed` | the configuration, at most 4 KiB |
| 7 | `RootReady`: the guest assembled the root | none |
| 8 | `RootFailed`: a stage of the root failed: the guest's answer to `RootConfig`, or its report of a failed teardown before it confirms a `Stop` | the stage, then the problem, at most 4 KiB |
| 9 | `Probe`: asks the guest to run a [probe](#probes) on its root | the probe, at most 4 KiB |
| 10 | `Report`: the guest's answer to `Probe`: the facts that the probe found | at most 256 KiB |
| 11 | `ProbeFailed`: the guest's answer to a `Probe` that it does not know, or that failed | the problem, at most 4 KiB |
| 12 to 21 | The messages of an [execution](#executions) | at most 256 KiB for a start, input, or output, and 64 bytes for the others |

The session keeps no message boundaries, so a message may span several of its
records, and one record may end a message and start the next. `Decoder`
reassembles the messages. It checks each header before it takes any byte of
the body, and holds at most one partial message. A malformed message ends the
stream, because the framing cannot find the next message after it.

The framing carries each body as bytes, within its kind's limit. The bodies of
kinds 6 and 8 to 21 have encodings of their own, which the `root`, `probe`, and
`exec` modules write and read. A body that does not decode leaves the stream
intact, at the start of the next message. The guest answers a root
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
`root::DEFAULT_FIRST_PARTITION_BOUND`, 64 KiB. The host program and the guest
agent ship together, so the guest refuses every other configuration, one with
an unknown field included, rather than ignore what it does not know.
`RootConfig::default()` holds the values that the host program sends unless its
caller names others: the runtime directory `/run/runtime-adapter`, the cgroups
`/sys/fs/cgroup/agent` and `/sys/fs/cgroup/workload`, a wait of 30 seconds, an
agent's `memory.low` of 16 MiB, as NVX's own guest reserves, and a workload's
`memory.max` of 128 MiB and `pids.max` of 128, with neither a bound of the
first partition, an overlay mode, a journal policy, nor a workload identity.

A stage of the root that fails is a `root::RootFailure`: the stage's code, one
byte, then the problem, as UTF-8 text. The guest's worker, and with it the VM,
then ends with the stage's status:

| Code | Stage | What failed | Status |
| --- | --- | --- | --- |
| 1 | `configuration` | Receiving and checking the root configuration | 11 |
| 2 | `layout` | Reading the boot layout of the root and the executions' hostname from the kernel command line, and checking that the worker assembles the layout | 12 |
| 3 | `bootstrap` | Bootstrapping the guest for the root: its cgroups and their resource values | 13 |
| 4 | `assembly` | Assembling the root, such as from an image of another identity than the boot tokens expect | 14 |
| 5 | `teardown` | Tearing the root down, such as while a process holds it open | 15 |
| 6 | `identity` | Checking the account of the sandbox's workload identity in the assembled root, in existing-account mode, such as one that the image lacks | 16 |

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

The probes of `worker::mapping`, none of which takes arguments either, map
beneath the root as an adapter maps the host paths of its sandboxes: through
the root itself, with `nvx::guest::root::Root::map`, which checks the targets
against the container root's reserved set, and through mappers of the
adapter's own, `nvx::guest::mappings::Mapper`, with reserved sets of its
choice. Each writes its sources below `/tmp/runtime-adapter-sources`, on the
guest's tmpfs, in place of the guest's mount of a host's export: a directory
`dir`, with a file `marker` and an empty directory `inner`, a file `file.txt`,
and an empty directory `out`. The links that they plant in the root lead to
`/tmp/runtime-adapter-outside`, an empty directory outside it.

| Probe | Facts |
| --- | --- |
| `map` | Through the root: zero mappings, `zero`, with the mounts below the root then, `zero.mounts`; then a table, `table`, of `dir` read-only at `/srv/maps/dir`, `dir/inner` read-write inside it, `file.txt` read-only at `/srv/maps/file.txt`, and `out` read-write at `/srv/maps/out`. The mount points below the root, in the mount table's order, `mounts`; for each mapping, `dir`, `inner`, `file`, and `out`, its options, `NAME.options`, whether a write is `allowed` or `refused`, `NAME.write`, what a write reached in its source, `NAME.source`, or what a read found, `NAME.read`; whether the overlay's upper holds the directory and the empty file that the mapper created, `upper.directory` and `upper.file`; and whether the root is latched, `latched` |
| `map-refused` | Through the root: a mapping below the symbolic link `/srv/link`, `through-link`; whether the root is latched then, `latched`; a later mapping, `later`, and whether its target exists, `later.target`; and the mounts below the root, `mounts`, and the entries of the directory outside it, `outside` |
| `map-checks` | Each check through a new mapper: a link at each component of a target, `link.a`, `link.b`, and `link.new`, and to `..`, `link.up`; a magic link of a procfs that it mounts in the root for the checks, `magic.self`, `magic.thread-self`, `magic.root`, `magic.cwd`, and `magic.fd`; a directory onto a file, `kind.directory`, and a file onto a directory, `kind.file`; a mapping onto a file that an earlier one shows, `nested.onto`, and one that would create its target inside it, `nested.create`, whether that reached the source, `nested.source`, and their teardown, `nested.teardown`; the runtime's reserved targets, `reserved.run`, `reserved.run-nvx`, `reserved.time`, and `reserved.time-below`, the container root's, `reserved.proc`, `reserved.sys`, `reserved.dev`, `reserved.agent`, `reserved.machine-id`, and `reserved.etc`, and those of a set of the adapter's, with the tree `/srv/adapter` and the path `/srv/checks/exact`, `reserved.adapter-tree`, `reserved.adapter-ancestor`, `reserved.adapter-path`, and `reserved.adapter-below`, and whether a refused one created anything, `reserved.created`; a read-only mapping attached and unmounted 200 times while another thread tries to open its file for writing, `read-only.attaches`, `read-only.flags`, `read-only.writable`, and `read-only.source`; and the entries and mounts of the directory outside the root, `outside.entries` and `outside.mounts`, and the mounts that the checks left, `left` |

A check's outcome is `applied`, or why the mapper refused the mapping: `link`
for a link at the component that the check expects, and otherwise the kind of
the error, such as `reserved` or `target-kind`, with the path that it names, if
any.

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

A failure of the session, a malformed message, a root configuration or an
execution, which only the worker of a VM with a layered root accepts, and a
message that only the guest sends end serving with an error.

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
   `nvx::protocol::root::RootLayout::parse`, and the executions' hostname,
   then bootstraps the guest's cgroups with `nvx::guest::root::bootstrap`,
   which moves the guest's processes into the agent's cgroup, and assembles
   the root with `nvx::guest::root::assemble`. If the configuration names a
   workload identity, it then checks the identity's account in the assembled
   root, in existing-account mode, with `nvx::guest::entry`;
4. answers `RootReady`, or `RootFailed` with the stage that failed, and then
   ends;
5. serves `Ping`, `Echo`, and `Probe`, though it refuses a probe while it runs
   executions, and runs the host's [executions](#the-workers-executions);
6. on `Stop`, drops its executions, which ends their programs, tears the root
   down, reports a teardown that failed with `RootFailed`, confirms the stop
   with `Stopped`, and ends.

Before the root configuration, the worker accepts only the configuration, or a
`Stop`, which ends it without a root. It assembles an EROFS image, or the
layers of a GPT disk, whose first partition holds at most the bound of the
configuration, over an ext4 scratch or a tmpfs in RAM, in the overlay mode of
the configuration. At the layout stage, it refuses a configuration that
configures a scratch for an upper in RAM, or bounds a first partition for an
EROFS image. The executions' hostname is the value of the kernel command
line's first `nvx_hostname=` parameter, which NVX's sandbox plans configure,
and which NVX's init agent reads too, or `nvx-sandbox`,
`nvx::guest::entry::DEFAULT_HOSTNAME`, without one, or with an empty one; a
hostname of more than 64 bytes fails the layout stage. A workload identity
whose account the assembled root lacks fails the identity stage. The worker
ends with 0 once the host stopped it, after a teardown that succeeded, with the
stage's status, from 11 to 16, once a stage failed, and with 1 once its session
failed or the broker closed the tty, each but the first after a diagnostic on
standard error. Once the broker closed the stream at a record boundary, which
may close only the broker's end, the worker drops its executions, and writes
what it queued, as once the host stopped it. Before it exits, it closes what
its probes hold, then unmounts a root that it still holds, as
`nvx::guest::root::Root` does when it drops, ignoring failures. A teardown that
failed leaves the root mounted until the VM ends. The worker runs the root's
operations through `worker::RootOperations`, and enters its executions through
`worker::EntryOperations`, so that tests run it against a fake root.

#### The worker's executions

Once the root is ready, the worker runs several executions at once, each
through NVX's execution engine and its guest binding, with the limits and
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
  the root moved over `/`, the configured hostname, the image's own machine ID,
  and no time status, so that the entry binds nothing into the root, and with
  published listeners, the entry's default, until the adapter tells whether
  its listeners are published. It runs as the identity that exactly one source
  names: the sandbox's workload identity, with no supplementary groups or
  capabilities, or, in a sandbox without one, the identity that its start
  names, with its supplementary groups, its capabilities, and its account
  mode. A start that names an identity in a sandbox with a workload identity,
  or none in a sandbox without one, is refused with `EINVAL`. Before the worker
  prepares the execution's cgroup, the entry checks the identity, and reads its
  account in existing-account mode: it refuses with `EPERM` a capability
  outside its allowlist, any capability of a user ID other than 0, and
  `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` while listeners are published, with
  `ENOENT` an identity without the user ID's passwd entry or the group ID's
  group entry, with the error of an account database or a root that it cannot
  read, and with `EINVAL` otherwise. The program starts in the working
  directory of its start, or in the root's `/`, with exactly the environment
  of its start, none of the worker's own, and with its standard input from the
  host or the null device. It leads a session of its own, without a
  controlling terminal, and the process group that the session creates, and,
  as PID 1 of its PID namespace, its end ends every process of the namespace.
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
  answered: `RootReady`, or the stage of the root that failed;
- `probe` sends a probe, and returns the facts that it found, or the problem.

Besides the session's own buffers, the client holds at most the rest of one of
the guest's records and one partial message. A reset of the session ends the
attachment, and its partial message with it, because the next epoch starts
with a new message, in a new attachment.

A failure of the session, a send that fails or times out, a malformed message,
an answer whose body does not decode, and a message that only the host program
sends end the client, and its later calls fail. A receive that times out
doesn't: it keeps what arrived. A request ends the client on any failure after
the framing accepted it, a timeout included, because an answer that arrived
later would answer the wrong request.

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

### Layered roots

`layered::LayeredLaunch` plans a VM with a layered root with NVX's typed plan
of a cold layered launch, `nvx::host::sandbox::SandboxPlan`, with NVX's default
sandbox settings, and checks it with `SandboxPlan::validated`. The plan names
the images by absolute paths, since OpenVMM runs in the sandbox's store. The
VM's kernel command line is the plan's, without parameters of the caller:
`nvx_sandbox=1`, the image's token, such as
`nvx_layer=distro,0xd0003000,erofs:UUID`, the upper's, such as
`nvx_scratch=0xd0006000,ext4`, and those of the settings, which the worker
ignores. OpenVMM's arguments are those that NVX's own sandbox start passes:
the plan's, which attach the image and the scratch as block devices, then the
managed lifecycle in one process, NVX's default memory, the kernel, the
initramfs, that command line, no virtio console, the control console with the
launch capability on standard input, and the outcome report.

The boot tokens carry only what NVX's boot contract defines. The launch sends
the rest in the [root configuration](#root-configurations), once it attaches:
the defaults, with the runtime directory that the caller names, as it is, so
that the guest checks it, the size of each image in sectors, if it is a whole
number of them, which the guest checks against its block device, and the
overlay mode and the scratch's journal policy that the caller names, if any.

The launch records what it launched in the sandbox's store, beside NVX's
runtime records, in `runtime-adapter.json`, before it spawns the VMM:

```json
{"format": 1, "mode": "layered", "kernelCommandLine": "nvx_sandbox=1 ...",
 "plan": PLAN, "rootConfiguration": "version=1\nruntime=/run/runtime-adapter\n..."}
```

`mode` is `layered`, or `blockless` for a VM without a layered root, whose
`plan` and `rootConfiguration` are `null`. `plan` is the persisted form of the
VM's `SandboxPlan`, and `rootConfiguration` the root configuration that the
launch sends, whether or not the guest accepts it.

### Commands

`runtime-adapter-host` takes NVX's artifacts from the directory that
`NVX_ARTIFACTS_DIR` names. Each command takes the guest agent's build from
`--guest-build BUILD`, the crate's version by default:

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
  `--distro FORMAT:PATH [--distro-identity UUID] --upper (scratch:PATH | ram) [--runtime-dir DIR] [--overlay-mode MODE] [--scratch-journal POLICY] [--first-partition-bound BYTES]`,
  gives the VM a [layered root](#layered-roots). Its image is the file PATH, an
  EROFS file system, `erofs`, or a GPT disk, `gpt`, of which the guest expects
  the identity UUID, if given: the file system's UUID, or the disk's GUID. Its
  upper is the ext4 scratch image PATH, or RAM. The guest assembles the root
  below its runtime directory DIR, `/run/runtime-adapter` by default, in the
  overlay mode MODE, `trusted-lowers` or `untrusted-lowers`, checks a
  scratch's journal against the policy POLICY, `allowed` or `forbidden`, which
  only a scratch takes, and refuses a first partition larger than BYTES, which
  only a GPT disk takes, with the guest's defaults unless given. Once attached,
  the launch sends the root configuration, and exchanges the `Ping` and the
  `Echo` once the guest assembled the root. If the guest reports that a stage
  of the root failed instead, the launch waits for the VM to end, abandons the
  launch, and checks that the VM ended with the stage's status.
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

`reattach`, `stop`, and `probe` find a VM that another runtime identity
launched mismatched, and then signal nothing: the VM keeps running. The
commands print what they find as `KEY=VALUE` lines: `reattached` (`live`,
`mismatched`, `exited`, or `indeterminate`), `differences` (the parts of the
runtime identity in which a mismatched VM differs), `pid` (the VMM's process
ID), `instance` and `epoch` (the identity of the session that they attached
to), `root` (`ready` or `failed`: how the guest answered the root
configuration, or `failed` when a stop finds that the root's teardown failed),
`stage` and `problem` (the stage of the root that failed, such as `assembly`,
and the problem), `PROBE.KEY` (the fact KEY that the probe named PROBE found),
`stopped` (`graceful`, `forced`, or `timed-out`), `outcome` (such as
`success 0`, or `guest-exit 14` once the assembly failed), and `teardown`
(`complete` or `incomplete`: whether the VMM's teardown of the VM completed,
which the outcome report records apart from the root's). Values escape `\`, newlines, and
carriage returns as `\\`, `\n`, and `\r`. The commands exit with 0 once they
did what they were asked, with 3 when they find the VM mismatched, with 4 when
the guest reported that a stage of the root failed, and the VM ended with the
stage's status and a complete teardown, and with 1, after a diagnostic on
standard error, when they fail. Without arguments, the program reports the
contract that it was built against.

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
configuration. They cover each kind's encoding, messages in pieces of any size
and split at every byte, the headers that the framing refuses before their body
arrives, and the root configurations, failures, probes, facts, and messages of
executions that decode, and those that don't. In the guest-only configuration,
the unit tests of `worker::probes` cover what the probes read: the mount table
with its escapes, the kernel's records of a process, `os-release`, and the
guest's memory, and the probes and arguments that the worker refuses. Those of
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
without a layered root answers probes with a failure, and refuses root
configurations and executions.

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
  answers, ready or the stage that failed, probes and their facts, a stop
  that learns that the teardown failed, and answers whose bodies do not
  decode, which end the client.

Through a relay between its test broker and a pseudo-terminal,
`tests/guest_session.rs` also checks that `guest::serve_terminal` and
`runtime-adapter-guest init` serve a tty in raw mode, with an `Echo` of every
byte value, and that `init` fails at once on a path that is no tty.

`tests/guest_session.rs` also runs the worker against a fake root, behind its
test broker: a root that the worker assembles, probes, and tears down on a
stop, across a reset of the session; an image over RAM, in the overlay mode of
the configuration; a GPT disk, with the configuration's bound of the first
partition; the failure of each stage, which the worker reports before it ends
with the stage's status, without the operations of the later stages, a
hostname of more than 64 bytes and a workload identity whose account the root
lacks included; the account check of a sandbox's workload identity, once the
root is assembled, with the hostname of the kernel command line, the default
one without it or with an empty one, and the first of two; a teardown
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
broker, whose host side drives the engine's peers, with frame-counted credit
and one output pool. The fake root prepares each execution's entry, as the
worker describes it, in a directory of the tests' own whose account databases
hold the accounts that the tests name, and then spawns the program without
entering a root, which needs privileges, in the tests' own root and identity.
Unless a test says otherwise, the sandbox has a workload identity, as which
every execution runs. Where the tests can create cgroups, as root in a cgroup
v2 hierarchy with the `pids` controller, the fake root gives the worker a
workload cgroup of the test's own, below which each execution runs in a cgroup
of its own, so that an execution whose program ends on its own settles, and is
reusable. Elsewhere, the fake root names no cgroups, and each program runs in a
process group alone, so that such an execution is fenced. The tests cover:

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
  configured hostname, all of which the worker's entry takes;
- the starts that the worker refuses at stage 0 for their identities: one that
  names an identity in a sandbox with a workload identity, or none in a
  sandbox without one, with `EINVAL`; a capability outside the entry's
  allowlist, one of a user ID other than 0, and `CAP_NET_BIND_SERVICE` and
  `CAP_NET_RAW` while listeners are published, with `EPERM`; an identity
  without a passwd entry, or whose primary group has no group entry, with
  `ENOENT`; and one whose passwd entry has another primary group, or whose
  user ID the kernel reads as "unchanged", with `EINVAL`. None of them takes a
  place, so the worker then runs its limit of executions at once;
- an execution whose start comes in the payload of the root's configuration,
  whose timeout counts from its own start, however long the root's assembly
  takes;
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
Windows, the plan of a layered launch: its block devices and boot tokens, the
OpenVMM arguments of NVX's sandbox start, and the sizes of the images; the
options that it passes on as they are, for the guest to check; the record of
a launch, with and without a layered root; and the command lines that the host
program refuses, each with status 2.

`tests/vm_restart.rs` checks the restart of an orchestrator, on a real
hypervisor, in the host-only configuration:

- `runtime-adapter-host launch` launches a VM, and exits;
- another process's `reattach` holds the VM, which is not its child, and
  attaches a new session, in a later epoch of the same broker instance;
- a third process's `stop` stops the VM: gracefully, with a successful outcome,
  and with no runtime record left.

In another test, a host program that ships another build of the guest agent
finds the VM mismatched in its initramfs, and signals nothing: the VM runs on,
under the same runtime record, until the host program of the right runtime
stops it.

### Layered scenarios

`tests/vm_layered.rs` launches VMs with a layered root on a real hypervisor, in
the host-only configuration, through the host program's commands, and checks
that the guest's worker, outside PID 1, assembles the root, or reports the
stage that failed, and that the VM still ends cleanly. Each scenario is a row
of the table that its `scenarios!` macro declares, and an ignored test of its
own:

| Scenario | Root | Expects |
| --- | --- | --- |
| `erofs_over_scratch` | NVX's Ubuntu EROFS image, with its own identity, over a copy of the scratch template | A root that the worker assembled: the image, read-only, under the overlay, whose upper and work directories are on the scratch, each mounted once below the runtime directory, with the options of the default mode, which trusts its lowers; the merged root's contents, with a file that reaches the upper and not the image; a copy-up of a file's metadata alone, which leaves its data in the image, and the rename of a lower directory; the image's and the scratch's disks; the agent's and the workload's cgroups, with their resource values; a worker that is PID 1's child, and PID 1's unrelated child, untouched; and a teardown that unmounts the scratch cleanly |
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
| `mappings_apply_beneath_the_root` | The image over the scratch, beneath which the `map` and `map-checks` probes map | Through the root: zero mappings that do no work, then a table mounted in order, each mapping with its access, `nosuid`, and `nodev`, whose writes reach the sources of the writable ones only, on targets that the mapper created on the overlay. Through mappers of the adapter's own: a link or a magic link at each component, the wrong kinds, a target that would be created inside another mapping, and the runtime's, the container root's, and the adapter's own reserved targets refused, and a read-only mapping that is never writable. A teardown that unmounts the mappings, then the scratch, cleanly |
| `a_refused_mapping_latches_the_root` | The image over the scratch, in which the `map-refused` probe plants a symbolic link | A mapping through the link that the root refuses, nothing mounted or created, and a root that refuses every later mapping, then tears down cleanly |
| `root_profiles_enter_the_root` | The image over the scratch, which the `entries` probe enters | Executions that start as PID 1 of their namespaces, with the root moved over `/`, only their standard streams, the leaked descriptor closed, no inheritable or ambient capability, and no new privileges, blocking no signal that their caller blocks, at the top of the root unless they name a working directory. A working directory that the execution names is entered, but none through a magic link, such as the leaked descriptor's, its own working directory's or root's, or any descriptor's, nor through a symbolic link to one: each fails the launch before the program runs. User ID 0 with each allowed capability alone holds it alone, and only its own attempt succeeds; with none, nothing does, nor for a numeric identity without an account. No execution writes `/proc/sys`, so none lowers the unprivileged port start, and a nested `chroot` stays in the root. With published listeners, user ID 0 with every other allowed capability can neither bind a privileged source port nor open a raw socket, so it cannot forge a listener announcement in either publication mode; without them, it may. The entry refuses the 51 capabilities outside its allowlist, the binding and raw-socket ones while listeners are published, capabilities for another user ID, and an identity without an account in existing-account mode. The root's own machine ID, then a generated one and the time status that the guest's time daemon publishes, read-only, in which a root workload finds no lock of the daemon's, which the runtime bound in the helper's mount namespace, and which leave the worker's namespace without mounts below the root. A teardown that unmounts the scratch cleanly |

A row names the root, the image, the GPT disk's top-layer variant, the
identity that the launch expects of the image, the upper, the runtime
directory, the overlay mode, the scratch's journal policy, and the bound of the
first partition, and what the scenario expects: a root that the guest
assembles, with a check that probes it, a teardown that fails after a check, or
the stage that fails, with its problem. Each scenario checks the launch's
record, and, over a scratch, whether the VM left the scratch's journal to
recover, which tells whether the guest unmounted the scratch.

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

The VM tests, `tests/vm_restart.rs` and `tests/vm_layered.rs`, share their
sandboxes through `tests/common`. A test that fails ends the VM that it left
running, and keeps the sandbox's directory, whose path it prints, unless it can
confirm that the VM ended. The tests boot VMs, so they are ignored. They read
the hypervisor from `NVX_HYPERVISOR`, through
`nvx::Hypervisor::from_env_or_default`, NVX's artifacts, with their install
metadata and the fixtures, from `NVX_ARTIFACTS_DIR`, and the guest agent from
`RUNTIME_ADAPTER_GUEST`. In a checkout where `python3 scripts/nvx.py download`
installed NVX's artifacts, and with the scratch template, run them in this
directory with:

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
