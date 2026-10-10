# Sandbox filesystem and agent architecture

[Design index](../design.md)

The sandbox specialization runs exactly one workload container per microVM.
Its lifetime, resource envelope, and network identity belong to that workload,
so it does not need pod infrastructure, a pause container, dynamic rootfs
injection, or a sequence of host RPCs to create additional containers. The
agent remains outside the workload's namespaces and supervises it from the
initramfs. This specialization is for non-confidential, single-workload
sandboxes, not multi-container groups; the host is trusted with image content
and guest memory.

The implemented foundation is a cold-filesystem bootstrap, low-level snapshot
primitives, and an authenticated control channel with a bounded managed
lifecycle. The production conversion service, replaceable launch
configuration, Rust agent, and production runtime protocol described below
are **Proposed**. They must not be inferred from the presence of block devices
or snapshot-tier metadata alone.

Two host components launch sandboxes on this foundation. The `nvx sandbox`
command attaches image layers and scratch, from which the guest assembles the
[layered root](#implemented-filesystem-bootstrap). The
[`aci_edge_sandboxes` crate](../../aci_edge_sandboxes/README.md) exposes a
provision, start, exec, stop, and deprovision API. Its default backend launches
OpenVMM directly, without NVX tooling, with the managed lifecycle and no
sandbox blocks, so each workload runs in the Alpine initramfs root, as
[Control protocol and checkpoint handoff](#control-protocol-and-checkpoint-handoff)
describes. Its optional agent backend delegates the lifecycle to a separately
supplied native library and an image-backed edge guest that NVX does not build.

## Implemented filesystem bootstrap

The public `nvx sandbox` command accepts one to three role-bearing EROFS lower
images, a preformatted ext4 scratch image, any number of
[live host shares](#live-host-shares) that its kernel command line holds, an
absolute entrypoint, and
individual argument tokens. It supplies non-secret kernel-command-line
configuration for one-shot runs. Managed execution carries bounded arguments
and a per-execution environment over the authenticated control channel;
one-shot environment variables, secrets, and sandbox snapshot orchestration
are not supported by this command. Lower-level OpenVMM capture and restore do
support sandbox blocks. See [Run](../run.md#experimental-single-workload-sandbox).

The required kernel facilities are already enabled in the NVX microVM kernel
configuration: virtio-blk, virtio-fs, EROFS with compression and xattrs,
overlayfs, cgroup v2, memory and process controllers, namespaces, BPF system
calls, and cgroup BPF programs. Kernel support for a device filter does not
mean the current agent installs one.

The guest init agent performs the assembly:

1. mount runtime tmpfs and cgroup2, create sibling `agent` and `container`
   cgroups, and move the supervisor into the agent cgroup;
2. resolve each role's MMIO address through the platform resources in
   `/proc/iomem` and the virtio block-device sysfs tree, rather than assuming
   a `/dev/vdX` name or `virtioN` enumeration order;
3. check an optional expected sector count, flush cached block-device buffers,
   mount each lower layer read-only as EROFS, and check its supplied UUID;
4. resolve and flush scratch, mount it as ext4 with `rw,nosuid,nodev`, and
   create its `upper` and `work` directories;
5. mount overlayfs with top-first lower layers and `metacopy=on,xino=on`;
6. verify the workload identity in the assembled root, mount each live share
   at its target inside that root, and write the workload machine ID to a
   runtime-tmpfs file;
7. start a child behind a FIFO barrier and place that child in the workload
   cgroup before releasing it;
8. wait for the child, unmount the shares in reverse order and then the
   overlay, layers, and scratch, and return its status through the guest exit
   helper.

In a managed sandbox, the managed agent repeats step 7 for each requested
workload, and the teardown in step 8 follows `stop`.

The assembled view is:

```text
/run/nvx/layers/custom   (optional EROFS) --+
/run/nvx/layers/runtime  (optional EROFS) --+--> lowerdir, top first
/run/nvx/layers/distro   (optional EROFS) --+
/run/nvx/scratch/upper   (ext4) -----------> upperdir
/run/nvx/scratch/work    (same ext4) ------> workdir
                                          overlay --> /run/nvx/rootfs
virtio-fs tag microvm    (optional share) ---> /run/nvx/rootfs/TARGET
  or an aggregate        ---> /run/nvx/shares, each child bound at
                              /run/nvx/rootfs/TARGET
```

At least one lower role is required by the bootstrap; distro plus runtime is
the intended curated-image shape, not a requirement that all three lower
slots be populated. The initramfs remains the supervisor's root and is not
another container lower layer.

The container launch helper releases the barrier into private mount, PID, and
UTS namespaces. The container entry helper makes mounts
private, creates private proc, read-only sysfs, `/dev`, devpts, and shared-memory
mounts, binds the workload machine ID read-only, and enters the overlay with
`chroot`. It clears supplementary groups and all capability sets and enables
`no_new_privs`. It does not `pivot_root` away from the initramfs `rootfs`, and
the outer supervisor does not replace itself with the workload.

The current agent sets `memory.low` to 16 MiB by default and accepts optional
workload `memory.max` and `pids.max`. This is not the stronger production
resource-reservation contract below. FIFO-gated cgroup placement is also not
`clone3(CLONE_INTO_CGROUP)`. The shell supervisor and textual errors remain an
experimental bootstrap, not a complete OCI runtime, typed RPC service, or
systemd-container profile. Workloads do run under one host-owned non-root
numeric identity fixed at initial boot. The agent requires an exact user and
primary-group match in the assembled root, clears supplementary groups and all
capability sets, and rejects an unavailable identity before starting the
workload.

### Live host shares

A live share exposes a host directory, such as a source checkout or a tool
cache, or a single host file, such as a settings file, to the workload without
copying it into an image: edits are visible in both directions without staging
or copy-back.

The microVM has one virtio-fs slot, tag `microvm`. One `--mount` of a directory
attaches a HostFs server to it. Several, or a shared file, attach an aggregate
instead, whose children, named
by their index and a digest of their guest target, are independent HostFs
volumes, so the shares keep independent `ro` or `rw` modes and access
policies. A file child serves only its file, never the file's directory or
siblings, because the guest mounts the device's root, which is a directory.
On every request, whichever guest
mount or link reaches the share, the host enforces the share's mode and its
denied, allowed, and writable paths, and accesses host files as the identity
that `--mount-owner` selects. Guest mount flags are therefore not a security
boundary, and the access policy adds no guest configuration. Before OpenVMM
starts, NVX rejects policy paths outside their share or for a shared file, and
guest targets or host paths that equal or contain one another, because one
share could otherwise hide another or reach its files under a different policy,
and it rejects shares whose kernel command-line tokens exceed the sandbox's
budget.
[Machine and device ABI](machine-and-device-abi.md#filesystem) defines the
device contract.

OpenVMM appends one `virtfs_dir=`, `virtfs_tag=`, `virtfs_mode=` triplet for
its share, and `virtfs_aggregate=1` for an aggregate, which it mounts
read-write when any child is. NVX then adds one
`nvx_share=NAME,TARGET,MODE[,file]` token per child, where `file` marks a
shared file. Because a child's name holds a digest of its target, a
snapshot pins each target, and a restore that requests another target fails
before the guest runs. The init agent parses every token, and creates every
target, before it mounts anything, so a malformed bootstrap mounts nothing. It
creates each target inside the container root one component at a time and
refuses a path that crosses a symbolic link, so an image layer cannot redirect
a share outside that root. The last component of a shared file's target is a
regular file, which the agent creates when the container root lacks it, and of
any other target a directory. It also refuses a repeated child, overlapping
targets, and targets that the container entry helper later mounts or binds
over, such as `/etc/machine-id`, where the runtime would hide the share or
write into it. A single share
is mounted at its target with its mode and `nosuid,nodev`. An aggregate is
mounted at `/run/nvx/shares`, outside the container root, where its root lists
the children and only root can enter, and each child is bound at its target
with its mode and `nosuid,nodev`. The workload's private mount namespace
inherits the mounts, and teardown unmounts the binds in reverse order before
the aggregate. Any
refusal or mount failure aborts the sandbox with status 125 instead of
starting the workload without its shares. A managed sandbox records its
shares, including whether each is a file, when it is provisioned, and
reattaches them on every start.
[Live host shares](../run.md#live-host-shares) describes
the options and their rules.

## Image preparation and distribution (Proposed)

Image conversion belongs off the sandbox start path, in a Linux control-plane
service that can run `mkfs.erofs`. Registry credentials stay in that service,
not on the node's launch path or inside the guest. The host resolves image
defaults and sandbox overrides once; the agent should not implement OCI image
configuration merge semantics or read configuration from a mounted layer.

The intended artifact ownership is:

| Artifact | Contents | Delivery and lifetime |
| --- | --- | --- |
| Kernel and initramfs | Platform kernel, agent, and minimal userland | Versioned node deployment, present before sandbox creation |
| `distro` | Curated base OS | Immutable content-addressed node blob cache |
| `runtime` | Language runtime and common libraries | Immutable cache, shared across images using that runtime |
| `custom` | Customer-specific additions, replacements, and deletions | Optional immutable cache entry, scoped to its image |
| Scratch | Writable ext4 upper and work directories | Private node-local file, drawn from a prepared pool or template |

Curated bases can be recognized by exact OCI layer-digest prefix matching.
The deepest matching base determines the reusable distro/runtime split. An
unrecognized image can be flattened into one custom lower layer; it loses
curated-layer sharing but can still use a compatible image-independent
platform template. Identical file contents with different layer digests do not
establish a prefix match.

Separate blobs preserve reuse of the common distro and runtime across many
customer images. Pre-merging all combinations, or packaging every combination
as one partitioned disk, would make each combination a separate cache object.
Compressed EROFS reduces distribution size and the host's cached image bytes;
decompression is performed by the guest kernel as blocks are read. A FUSE
daemon and guest-side lazy-pull agent are not required for these rootfs layers.
The optional HostFs devices remain useful for
[live host shares](#live-host-shares) and are a separate feature, not the
container image-delivery path.

The converter must synthesize OCI deletions as overlay whiteouts and opaque
directory markers. It must apply a deny-by-default metadata policy to every
layer: never copy arbitrary `trusted.overlay.*` or `user.overlay.*` attributes,
and allow and normalize file capabilities, SELinux labels, and ACLs only under
an explicit policy. Device inodes, including whiteouts, need the same treatment.
The current bootstrap mounts supplied EROFS images; it is not this sanitizing
conversion service.

This trust requirement matters because `metacopy=on` interprets overlay
metadata from lower layers. Metadata-only changes can avoid copying file
contents, but a data write can still copy an entire lower file into scratch.
Scratch capacity must account for that amplification. `xino=on` improves inode
identity but can fall back when the underlying inode encoding overflows; the
chosen filesystem and kernel combination needs validation.

Scratch should be prepared off the launch path and acquired without `mkfs`
during startup. For the current snapshot-capable microVM it must be a regular
raw file containing ext4, not a VHD/VHDX attachment. A pool or a filesystem
clone can accelerate acquisition, but the VMM also supports independent copies;
block-clone support is not a prerequisite for running NVX. Scratch consumes
host storage rather than guest RAM and can fail with `ENOSPC` independently of
guest memory pressure. Runtime tmpfs is still appropriate for small agent
state, not the workload's general writable layer.

Blob garbage collection must retain references from both live sandboxes and
snapshots. Paired scratch, state, and RAM need snapshot-scoped retention and
placement. Kernel/agent rollout must rebuild or explicitly recertify templates:
restoring guest RAM restores the captured agent build, not the node's newly
installed initramfs. These cache, pool, and rollout services are host
orchestration responsibilities, not implemented VMM artifact management.

## Replaceable configuration region (Proposed)

The production launch contract should use one bounded, versioned,
VMM-populated memory region. Only its location and ordinary invariant kernel
parameters belong on the command line. The command line is size-limited,
readable through `/proc/cmdline`, and captured in RAM; it cannot safely carry
secrets or values that change between clones.

The configuration is fully resolved by the host and separated by consumption:

| Section | Contents | First consumed |
| --- | --- | --- |
| Invariants | Role/MMIO mapping, layer geometry, and platform-owned guest network identity such as MAC, address, prefix, and MTU | Before the platform snapshot point |
| Image binding | Expected EROFS UUID for each attached lower role | After the platform snapshot point, before workload launch |
| Sandbox | Entrypoint, arguments, environment and secrets, workdir, identity, capability policy, and tenant resolver/routing settings | After the platform snapshot point |

Only unconsumed configuration may be replaced. Platform clones may receive
new image binding and sandbox values; workload-start and instance-checkpoint
restores have already consumed them and must not pretend that rewriting a
region changes mounted filesystems or process state. Those later tiers accept
new work through a runtime protocol, not replacement OCI configuration.

The host must validate the independent launch configuration against the saved
contract before entering a vCPU, including canonical digests for consumed
sections. The agent must validate the header, version, bounds, and invariants
again before using the payload. An EROFS UUID is only an attachment mix-up
check, not a content-integrity proof. Current OpenVMM restore verifies SHA-256
for consumed read-only layers; a future cache-admission optimization needs an
explicit trusted artifact contract rather than replacing that check with UUIDs.

The region must be separate from both captured RAM and restore-time expansion
backing, omitted from the Linux direct e820 RAM map and `memory.bin`, and populated
before every launch. Its placement must not overlap any capacity reservation
or device. Restoring RAM must never overwrite newly supplied configuration.
A header with magic, version, bounded length, and corruption detection does not
by itself provide authenticity or secrecy.

The current platform command-line validator recognizes the exact prospective
token `nvx_config=0xd0010000,65536`; that allowlist entry does not allocate a
region or implement the payload protocol. The current bootstrap still reads
non-secret command-line tokens. The former standalone `phram` mechanism is not
an alternative configuration or layer carrier in this microVM profile.

Launch metadata must remain fresh even when all configuration sections have
been consumed. A future channel epoch, entropy seed, and clock sample therefore
cannot be frozen by an OCI-section digest. Today restore detection and entropy
repair use the process generation ID and
[portb restore packet](snapshot-and-restore.md#time-and-entropy),
not an implemented configuration-region epoch. Zeroing a region after use is
defense in depth: it does not erase copies in agent buffers or workload memory.
The region and raw-memory devices must never be exposed in the workload's
mount namespace or `/dev`.

The proposed network split also needs an explicit compatibility contract.
Current portable networking saves the guest identity, bootstrap command line,
and egress policy; it does not implement per-launch tenant DNS/routing swaps or
the old HCN L2Bridge/AF_XDP endpoint translation and readiness handshake.

## Production agent and launch sequence (Proposed)

The intended production agent is a small static Rust binary running as PID 1
from the initramfs. It owns pseudo-filesystem setup, filesystem assembly,
network programming, workload construction, orphan reaping, stdio, and runtime
control. It remains alive after starting the workload; `exec` of the workload
from the supervisor would destroy those responsibilities.

The cold launch should require no configuration RPC round trip:

1. the host stages layers and private scratch, constructs all devices, prepares
   the network backend, and writes the launch configuration;
2. the VMM boots the kernel and initramfs through Linux direct MP-table mode;
3. the agent sets up pseudo-filesystems and sibling cgroups, consumes only
   invariants, and configures the platform-owned guest network;
4. a trusted platform-template build captures here, before image binding,
   tenant configuration, or scratch mounts;
5. a cold launch or restored template validates the applicable configuration,
   performs required restore repair, discards stale block buffers, and mounts
   EROFS, ext4, and the overlay;
6. the agent creates the workload directly in its cgroup and private namespaces,
   applies its resolved runtime policy, and starts it under supervision.

The platform build must attach deterministic non-tenant placeholder layers.
Linux can probe block contents before PID 1 runs, so merely avoiding mounts
does not prove a snapshot contains no image bytes. Restored layers must have
the captured geometry, and cached placeholder blocks must be invalidated
before mounting replacements. Current host validation records platform layers
as unbound but cannot prove which bytes arbitrary guest code read before capture.

A complete runtime must implement more than the current bootstrap: atomic
`clone3(CLONE_INTO_CGROUP)` placement, the required namespace and mount policy,
masked/read-only paths, user/group and supplementary-group handling, capability
sets, seccomp, rlimits, terminal allocation, signals, and reliable child/orphan
supervision. The root transition must preserve the outer agent's initramfs
view; it cannot assume `pivot_root` works directly from initramfs `rootfs`.

Production resource policy should give the agent a measured memory reserve
and CPU weight in a sibling cgroup, with workload `memory.max`,
`memory.oom.group=1`, and appropriate process limits. `memory.min` protection
and workload limits must be sized together with VM RAM; the current
`memory.low` setting alone is not a hard survival guarantee. A cgroup device
filter is needed for profiles that retain `CAP_MKNOD`. The workload must not
be able to escape the agent-owned cgroup or undo its freeze; any delegated
subtree must remain below that boundary.

## Control protocol and checkpoint handoff

The implemented managed runtime uses the dedicated control virtio-console and
its authenticated broker. Boot diagnostics and kernel `printk` stay on the
existing consoles; framed control traffic does not share an unstructured byte
stream with them. OpenVMM owns the bounded outer framing, same-user local
endpoint authorization, capability authentication, reconnect epochs, and
receive-credit backpressure. The current guest protocol provides readiness,
sequential command execution with bounded arguments, per-execution
environments and output, separate stdout/stderr, timeout and exit categories,
cancellation, and graceful VM shutdown. The control device is never exposed
inside the workload namespaces.

While a workload runs, the agent keeps reading the control console. A `CANCEL`
request that carries the workload's request ID kills the workload, and the exit
report then uses the `cancelled` category with status 137. Without sandbox
layers, the agent runs each workload directly in the guest's root file system,
inside the `nvx-exec` cgroup; termination writes that cgroup's `cgroup.kill`,
which also reaches processes in other sessions or process groups. When the
workload's first process exits, the agent kills what remains and reaps the
orphans it inherits as PID 1, so no workload process outlives its exec.
It reports the outcome only after the cgroup's `pids.current` reads zero, that
is, once every process of the workload has exited and been reaped. The agent
enables the `pids` controller for this check, because the controller charges a
process until the process is reaped. `cgroup.events` would not do: it stops
counting a killed process as populated before the process becomes a zombie, so
the zombie could remain in `/proc` for the next workload to find. A failed
kill, an unreadable or malformed `pids.current`, or a settlement timeout reports
`containment-failed` instead. Every subsequent direct execution also reaps what
an earlier workload left and then requires verified emptiness, so a
control-session reset cannot bypass a failed containment check.

An `EXEC` payload holds a 32-bit timeout, a 16-bit argument count, a reserved
16-bit field, and the length-prefixed arguments. A reserved field of 1
announces an extended header: a 16-bit flag field, a 16-bit environment entry
count, and a 32-bit working-directory length. With flag bit 0, an absolute
working directory of at most 4096 bytes follows the arguments. With flag bit 1,
up to 256 environment entries come last, each a 32-bit length and a
`KEY=VALUE` string of at most 4096 bytes with a distinct, non-empty key, and
replace the workload's environment; flag bit 2, which requires bit 1, layers
them over the default environment instead, each entry replacing the default
variable of the same name. Without bit 1, the entry count must be zero. The
agent passes these fields through a sealed anonymous file to a launch
helper, a copy of the agent that runs once `setpriv` has applied the workload's
identity, inside the container root with sandbox layers. The helper enters the
working directory, or `/` without one, points `PWD` at it unless the request
replaces the environment, applies any environment, and executes the workload;
if it cannot enter the directory or set `PWD`, it writes a diagnostic to the
workload's standard error and exits with status 125. Without sandbox layers,
the workload's child process enters the directory itself, under the workload's
user and group IDs with no supplementary groups or effective capabilities,
before `setpriv` makes that identity permanent. `setpriv` and the helper keep
that directory instead of looking its path up again, so the workload starts in
the directory that was checked even if the path is renamed, replaced, or
retargeted in the meantime. Unless the request replaces the environment, which
controls `PWD` itself, the child also points `PWD` at the directory, and the
agent refuses the request as `launch-failed` if it cannot. If the workload
cannot enter the directory, the agent writes a diagnostic to the workload's
standard error and refuses the request with the `cwd-failed` category and the
error number as status, so nothing runs in another directory.

Without sandbox layers, the agent maps host directories for workloads from
one aggregate virtio-fs export. The host exports the outermost mapped
directories and the parents of the outermost mapped files as numbered children
of the export, which the init mounts at `/run/nvx/hostfs/root`, and enforces
each mapping's access itself: a child is read-write only if it holds a
read-write mapping, OpenVMM limits writes to its read-write mappings, and the
parent of mapped files hides everything else. Neither a whole volume nor a file
directly in a volume's root, whose parent would be that root, is mapped. Before
it accepts control
traffic, the agent makes `/run/nvx/hostfs` root-only. The kernel command line
announces the number of mappings with an `nvx_maps=COUNT` token, and the host
then sends the mapping table in `MAPS` requests. Each request carries the index
of its first entry and its number of entries, and each entry its flags (bit 0
selects read-only), the lengths of its source and target, the source, a path
relative to the export such as `0/src`, and the absolute target. The agent
accepts entries only in order and only up to the announced count, checks a
whole request before it mounts anything, bind-mounts each source at its
target, remounting the bind `nosuid,nodev` and, for a read-only entry,
read-only, and answers `READY`, or `ERROR` with the `mapping-failed` category
and the error number. Until it has mounted every announced entry, it refuses
`EXEC` with the `mappings-incomplete` category, and a failed mount keeps it
refusing. Without an `nvx_maps=` token, it refuses `MAPS` as
`invalid-request`, and it ignores the `nvx_map=` tokens of older hosts.
OpenVMM's deny list hides denied paths. A
`nvx_workload_account=create` token lets the managed init create an account for
a host-selected non-root identity that the image lacks, which Linux hosts use
to run workloads under the host user's IDs. An image account that already uses
the UID but names another primary group or lacks a usable home is replaced.
A `CANCEL` for a workload that already finished is ignored. Any other request
during an execution is refused as `busy`. A reset means the host session that
could observe the workload is gone: the agent kills the workload, sends nothing
more for it, and acknowledges the new epoch, so the next session does not wait
for an abandoned workload.

A host cannot tell from a successful readiness probe whether the image enforces
the behaviors it depends on: an older guest boots and answers, but ignores the
mapping table and `CANCEL` requests. A `FEATURES` request, which carries no
payload, therefore asks which control behaviors the image provides. The agent
answers `READY` with a four-byte little-endian bit mask: `CANCEL` (bit 0),
`WORKLOAD_ACCOUNT` (bit 2, provided by the managed init and reported by the
agent, because both ship in one initramfs), `EXEC_CGROUP` (bit 3),
`EXEC_ENVIRONMENT` (bit 4, which applies an explicit environment to each
execution, either replacing or layering over the bootstrap environment),
`EXEC_CWD` (bit 5, the `cwd-failed` refusal of a working directory that the
workload cannot enter), and `HOST_MAPPING_TABLE` (bit 6, the `MAPS` requests
above). Bit 1 announced the bind mounts that `nvx_map=` kernel tokens listed in
earlier images; it stays unset, so hosts that still send those tokens refuse
current images. Without sandbox layers all six features are provided; with
them, only `CANCEL`, `WORKLOAD_ACCOUNT`, and `EXEC_ENVIRONMENT`.
An agent that predates the request refuses it as `unsupported-operation`, which
a host reads as no features, so a host terminates a guest that lacks a feature
it needs instead of running workloads without the policy it asked for. During
an execution the request is refused as `busy`. A feature bit is added together
with the behavior it names; hosts ignore bits they do not know and trailing
bytes of the answer.

The remaining production operation families are:

| Operation | Purpose |
| --- | --- |
| `Ready`, `RestoreHello` | Establish readiness, protocol version, and the current launch identity |
| `Bootstrap` | Explicit configuration when a workflow cannot use the launch region |
| `InteractiveShell` | PTY-backed interactive sessions, including cancellation and resize |
| `Signal`, `ContainerExited` | Out-of-band signal control and OOM reporting |
| `Probe` | Execute health checks |
| `PrepareSnapshot`, `PostRestore`, `Checkpoint` | Agent-coordinated capture and restore hooks |
| `Shutdown` | Graceful workload and VM termination |

Messages need bounded framing, stream and request identifiers, stable typed
errors, backpressure, and launch-scoped identity. On restore the agent must
discard partial decoder state and requests from the old host connection,
reestablish the session with `RestoreHello`, and fail non-replayable operations
rather than silently executing them twice. Protocol versioning is tied to the
node-deployed agent; transport compatibility alone is not RPC compatibility.

The proposed workload-facing checkpoint interface is an opt-in, bind-mounted
`SOCK_SEQPACKET` socket, such as `/dev/nvx/checkpoint`, with per-message
`SCM_CREDENTIALS`. The agent owns the PMIO write; the workload does not receive
`CAP_SYS_RAWIO`, `/dev/port`, or unrestricted port-I/O access. Capture requests
and retained artifacts need agent and host rate limits.

A warm shim requests capture after runtime initialization and receives its
next work item after restore. Python, Node, and Java need runtime-specific
hooks; kernel CRNG reseeding cannot reset their userspace RNGs, caches, or
external connections. Blocking the requesting thread on a socket is not a
barrier for its peers. A cloneable warm point must be single-threaded or hold
all peers behind a runtime-owned barrier until repair is complete. The current
workload-start helper requires a runtime post-restore hook, but there is no
production shim or work-item handoff protocol yet.

The replay contract is explicit: do not capture live external connections or
state that cannot be safely reused, do not repeat irreversible side effects,
and repair runtime randomness before accepting work. An instance checkpoint
is a single continuation; a workload-start snapshot is a cloneable starting
point, not transparent checkpointing of arbitrary requests.
