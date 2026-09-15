# Remaining production work

[Design index](../design.md)

The following items are **Proposed**, beyond the low-level implementation:

| Area | Remaining design or delivery work |
| --- | --- |
| Launch configuration and control | Implement the replaceable region, consumed-section validation, authenticated broker, bounded framed protocol, reconnect/cancellation semantics, and node-version compatibility policy. |
| Production agent | Complete OCI policy, atomic workload placement, orphan supervision, typed errors, PTY/log handling, and capture-safe worker rendezvous. Measure agent CPU/memory reserves. |
| Image conversion | Build the curated-base converter, metadata allowlist, geometry classes, admission validation, and tests for crafted overlay/security attributes. |
| Snapshot orchestration | Integrate trusted placeholder-based platform builds, per-runtime warm shims, checkpoint requests, private scratch acquisition, and capture failure reporting with the public sandbox workflow. |
| Artifact lifecycle | Define tenant-aware placement, retention, refcounts, quotas, distribution, and GC for layers, RAM, state, and paired scratch; invalidate or recertify templates on rollout. |
| Sharing isolation | Validate platform-template provenance and workload memory/device restrictions, per-tenant memory budgets, host deduplication policy, and ReFS physical-frame separation. |
| Metrics and performance | Define readiness and billing metrics; measure launch-to-workload-runnable latency, conversion/cache misses, scratch copy cost, cold versus warm page caches, and activation costs independently. |
| Networking | Design any external endpoint rebinding or tenant resolver/routing extension without silently changing the current portable profile or saved policy. Live flows must reconnect. |

The sandbox proposal's roughly 10-ms restore goal is a performance target, not
a guarantee established by this document. Report the precise readiness
boundary and backend for every measurement; see [Benchmarks](../benchmarks.md).

## Workload compatibility and volumes (Proposed)

Systemd-as-entrypoint images need an explicit, slower compatibility profile:
appropriate container environment, read-only system views, private devices,
mount propagation, an agent-owned parent cgroup with a delegated subtree, and
the required stop signal. Such images may need `CAP_MKNOD` together with a
cgroup device filter. They are not covered by the current drop-all-capabilities
bootstrap, and its private mount policy must not be described as that profile.

Azure Files, secret/content volumes, and other optional mounts remain undesigned.
Guest-side CIFS, a scratch-backed temporary directory, or an explicit small
content attachment are possible directions, not supported extra device slots
in the fixed ABI. Secure environment delivery through the proposed launch
region is distinct from secret-volume delivery. Scratch cannot currently grow
online; capacity exhaustion and whole-file copy-up remain workload concerns.

## Distribution extensions (Proposed)

P2P distribution can populate the same immutable node blob cache without
changing the guest's filesystem view. Demand fetching missing blob ranges
behind virtio-blk is another possible extension for large images, but it must
define bounded I/O failure, integrity, and snapshot eligibility before replacing
the current cached regular-file backend. Neither facility is implemented.
Adopt them based on measured cache misses and first-run latency, not merely
because a format permits them.

## Alternatives considered

| Alternative | Design tradeoff |
| --- | --- |
| Rootfs over virtio-fs/virtiofsd or 9p | Unnecessary per-operation filesystem transport for fixed cold-plugged layers. The existing optional HostFs export serves a different purpose. |
| Nydus/RAFS and an in-guest fetcher | Lazy distribution may be valuable, but an extra fetcher/daemon is not needed for fully staged EROFS blobs. Host-side distribution can evolve independently. |
| virtio-pmem/DAX or general-purpose phram layers | Requires a new memory/device and sharing contract; direct shared-page mappings and image-sized metadata costs need explicit justification. |
| tmpfs scratch | Competes with workload RAM and turns storage growth into memory pressure instead of an independent storage limit. |
| One merged blob or partitioned disk per image combination | Loses independent caching and reuse of distro/runtime layers; adds layout parsing without a need for arbitrary device discovery. |
| Guest device-mapper snapshots | Adds a second copy-on-write mechanism when overlayfs already supplies the writable filesystem view. |
| SquashFS or uncompressed EROFS | EROFS is the selected compressed read-only and overlay-metadata contract. Another format needs a concrete benefit; uncompressed images are primarily relevant to a future DAX path. |
| A general init system in the outer guest | Adds services and boot work that a single workload's supervisor does not need. A systemd workload inside its own namespace is a separate compatibility question. |
| Another vsock or multiport control device | The reserved single-port control console reuses existing queue and saved-state machinery; authentication and RPC still have to be built. |
| Configuration in the kernel command line | Size and secrecy limits, and no per-restore replacement of the captured effective command line. |
| Configuration baked into layers, scratch, or initramfs | Forces filesystem access or per-launch image writes and can leave stale configuration in captured RAM. The launch region keeps configuration separate from reusable artifacts. |
| Full layer hashing inside the guest | Reads whole images on the launch path. Current integrity checks belong to host restore; UUID checks in the guest detect attachment mistakes only. |
| Sharing one warm-workload memory file across tenants | COW protects artifact contents, not physical-page isolation. Tenant-scoped instantiation and its memory cost need explicit policy. |
| Replacing the agent with the workload via `exec` | Removes supervision, orphan reaping, and control service. The workload must be a child. |
