# Sandbox filesystem and agent architecture

[Design index](../design.md)

The sandbox specialization runs exactly one workload container per microVM.
Its lifetime, resource envelope, and network identity belong to that workload,
so it does not need pod infrastructure, a pause container, dynamic rootfs
injection, or a sequence of host RPCs to create additional containers. The
agent remains outside the workload's namespaces and supervises it from the
initramfs. This specialization is for non-confidential ACI Sandboxes, not
multi-container ACI container groups; the host is trusted with image content
and guest memory.

The implemented foundation is a cold-filesystem bootstrap and low-level
snapshot primitives. The production conversion service, replaceable launch
configuration, Rust agent, and authenticated runtime protocol described below
are **Proposed**. They must not be inferred from the presence of block devices
or snapshot-tier metadata alone.

## Implemented filesystem bootstrap

The public `nvx sandbox` command accepts one to three role-bearing EROFS lower
images, a preformatted ext4 scratch image, an absolute entrypoint, and
individual argument tokens. It supplies non-secret kernel-command-line
configuration; environment variables, secrets, arguments containing
whitespace, and sandbox snapshot orchestration are not supported by this
command. Lower-level OpenVMM capture and restore do support sandbox blocks.
See [Run](../run.md#experimental-single-workload-sandbox) and
[`scripts/nvx_tools/sandbox.py`](../../scripts/nvx_tools/sandbox.py).

The required kernel facilities are already enabled in
[`kernel/config-microvm`](../../kernel/config-microvm): virtio-blk, EROFS with
compression and xattrs, overlayfs, cgroup v2, memory and process controllers,
namespaces, `CONFIG_BPF_SYSCALL`, and `CONFIG_CGROUP_BPF`. Kernel support for a
device filter does not mean the current agent installs one.

[`alpine/nvx-init-agent`](../../alpine/nvx-init-agent) performs the assembly:

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
6. start a child behind a FIFO barrier, place that child in the workload
   cgroup before releasing it, and retain a runtime-tmpfs machine-ID file;
7. wait for the child, unmount the overlay, layers, and scratch, and return its
   status through the guest exit helper.

The assembled view is:

```text
/run/nvx/layers/custom   (optional EROFS) --+
/run/nvx/layers/runtime  (optional EROFS) --+--> lowerdir, top first
/run/nvx/layers/distro   (optional EROFS) --+
/run/nvx/scratch/upper   (ext4) -----------> upperdir
/run/nvx/scratch/work    (same ext4) ------> workdir
                                          overlay --> /run/nvx/rootfs
```

At least one lower role is required by the bootstrap; distro plus runtime is
the intended curated-image shape, not a requirement that all three lower
slots be populated. The initramfs remains the supervisor's root and is not
another container lower layer.

[`alpine/nvx-container-launch`](../../alpine/nvx-container-launch) releases the
barrier into private mount, PID, and UTS namespaces.
[`alpine/nvx-container-enter`](../../alpine/nvx-container-enter) makes mounts
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
The optional HostFs device remains useful for a live host export and is a
separate feature, not the container image-delivery path.

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
backing, omitted from the PVH usable-memory map and `memory.bin`, and populated
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
2. the VMM PVH-boots the kernel and initramfs;
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

## Control protocol and checkpoint handoff (Proposed)

The runtime protocol should use the dedicated control virtio-console once its
authenticated broker is implemented. Boot diagnostics and kernel `printk`
stay on the existing consoles; framed control traffic must not share an
unstructured byte stream with them. The reservation reuses a proven transport
but does not implement framing, authorization, guest RPC, or host-side broker
isolation. A VMM-owned endpoint still needs an authenticated, access-controlled
host attachment and must not be exposed to the workload.

The proposed operation families are:

| Operation | Purpose |
| --- | --- |
| `Ready`, `RestoreHello` | Establish readiness, protocol version, and the current launch identity |
| `Bootstrap` | Explicit configuration when a workflow cannot use the launch region |
| `ExecuteCommand`, `InteractiveShell` | One-shot execution or PTY-backed interactive sessions, including cancellation and resize |
| `StreamLogs` | Bounded stdout/stderr streaming with continuation semantics |
| `Signal`, `Wait`, `ContainerExited` | Lifecycle control, exit status, and OOM reporting |
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
`SOCK_SEQPACKET` socket, such as `/dev/aci/checkpoint`, with per-message
`SCM_CREDENTIALS`. The agent owns the PMIO write; the workload does not receive
`CAP_SYS_RAWIO`, `/dev/port`, or unrestricted port-I/O access. Capture requests
and retained artifacts need agent and host rate limits.

A warm shim requests capture after runtime initialization and receives its
next work item after restore. Python, Node, and Java need runtime-specific
hooks; kernel CRNG reseeding cannot reset their userspace RNGs, caches, or
external connections. Blocking the requesting thread on a socket is not a
barrier for its peers. A cloneable warm point must be single-threaded or hold
all peers behind a runtime-owned barrier until repair is complete. The current
workload-start helper requires a `runtime-post-restore` hook, but there is no
production shim or work-item handoff protocol yet.

The replay contract is explicit: do not capture live external connections or
state that cannot be safely reused, do not repeat irreversible side effects,
and repair runtime randomness before accepting work. An instance checkpoint
is a single continuation; a workload-start snapshot is a cloneable starting
point, not transparent checkpointing of arbitrary requests.
