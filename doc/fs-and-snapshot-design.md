# ACI Sandboxes — Guest Filesystem and Agent Architecture

*High-level architecture and rationale for the single-container-per-VM sandbox: image delivery, filesystem assembly, agent/host control plane, snapshots, and networking.*

**Status:** design proposal
**Scope:** ACI Sandboxes (not classic multi-container ACI container groups). Non-confidential only.
**Related:** `nvx` (VMM), `ACI.Sandbox.GuestAgent` (existing C# guest agent, being superseded), `ACI.SandboxRuntime` (host-side runtime)

---

## 1. Summary

ACI Sandboxes runs **exactly one container per micro-VM**. This single constraint is the source of nearly all the simplification in this design: because the VM's lifetime, resource envelope, and network identity are the container's, we can delete essentially all of the pod/sandbox machinery that conventional VM-based container runtimes (Kata, LCOW/hcsshim) require, and we can pre-compute the guest filesystem entirely off the critical path.

The design has four pillars:

| Pillar | Choice |
|---|---|
| **Image delivery** | An **initramfs carrying the agent**, plus **two or three read-only compressed-EROFS layer blobs** (distro, runtime, and an optional custom layer) and **one scratch blob**, each its own virtio-blk device. No partition tables, no layer discovery. Blobs are built ahead of time by a control-plane conversion service. |
| **Filesystem assembly** | `overlayfs` in the guest: EROFS layers as `lowerdir`, a **virtio-blk scratch VHD** as `upperdir`. Assembled by the agent, not the host. |
| **Control plane** | A **Rust init agent** running as PID 1. Boot-time configuration arrives in a VMM-populated memory region, sectioned by when each part becomes valid; runtime operations arrive over a framed **RPC multiplexed on the VMM-owned stdio channel**. |
| **Startup** | Everything cold-plugged, zero hotplug, and no host round-trips between VM start and `exec` on the cold fast path. Three snapshot tiers (platform / workload start / instance checkpoint) for further latency reduction. |

Deliberately **not** used today, for security reasons: `virtio-pmem` + DAX, which would let N VMs share one host page-cache copy of an image. See §9.

---

## 2. Context and Constraints

### 2.1 What "the VM is the container" buys us

In a conventional VM-based container runtime, one VM hosts a *pod*: containers are created and destroyed dynamically inside a long-lived guest, which forces a shared-filesystem transport (virtio-fs), device hotplug, a pause container to own namespaces, an in-guest init system, and a multi-step host→guest RPC sequence to create each container.

None of that applies here. Because the container is known at VM-create time and never changes:

| Conventional pod machinery | Why it exists | ACI Sandboxes replacement |
|---|---|---|
| `virtio-fs` + a `virtiofsd` daemon per VM | Inject N container rootfs dirs into a running VM | **Deleted.** Layers are cold-plugged block devices. No host daemon, no vhost-user socket, no FUSE round trips. |
| Device hotplug (ACPI/PCI events, guest re-probe) | Containers added after boot | **Deleted.** All devices present before the kernel probes. |
| Pause / sandbox container | Holds pod namespaces across container churn | **Deleted.** The VM is the boundary. |
| Separate guest OS image + systemd | Guest must outlive any container | **Collapsed.** Agent is PID 1 from the initramfs. |
| `CreateSandbox` → `CreateContainer` → `StartContainer` RPCs | Host drives container creation into a live guest | **Removed from the start path.** Configuration is written into a memory region before boot; the agent starts the container unconditionally. |
| Per-container cgroups, veth, network namespace | Divide one VM among N containers | **Deleted.** VM sizing *is* the limit; the container uses the VM's NIC directly. (One cgroup is kept — see §7.4.) |

### 2.2 Explicit non-goals

- **Multi-container container groups.** Out of scope. If a workload needs sidecars, it is not an ACI Sandbox.
- **Confidential computing.** Out of scope. The host is trusted to pull, convert, and store image content in plaintext. (A confidential variant would invalidate the entire host-side conversion pipeline and require guest-side image pull; that is a different design.)
- **Volumes** (Azure Files, `emptyDir`, secrets, gitRepo). Recognized as required *eventually*, but **optional and to be designed** — see §10.

### 2.3 Latency targets

- **Restore is targeted at ~10 ms**, measured from VMM launch to the workload being runnable.
- **Cold start** is dominated by the layer mount and `exec` path (§6); the design keeps host round trips off it entirely.

### 2.4 Guest platform prerequisites

This design depends on guest kernel features and VMM capabilities that **do not exist today**. They are hard prerequisites, not incremental improvements: nothing described below boots until they are in place.

**Guest kernel.** The current `kernel/config-microvm` explicitly disables every filesystem feature this design is built on:

```
# CONFIG_EROFS_FS is not set        ← the layer format (§4.2)
# CONFIG_OVERLAY_FS is not set      ← filesystem assembly (§5)
# CONFIG_MEMCG is not set           ← agent resource guarantees (§7.4)
# CONFIG_BPF_SYSCALL is not set     ← cgroup device filter (§10.1)
CONFIG_VIRTIO_BLK                   ← absent entirely; every blob device (§4)
```

`CONFIG_CGROUP_BPF` is additionally required for the device filter, and `CONFIG_EROFS_FS_ZIP_ZSTD` if layers are built with zstd rather than LZ4. The freezer used by the capture protocol (§8.5) needs nothing new: `cgroup.freeze` is a cgroup v2 core interface file and `CONFIG_CGROUPS=y` is already set.

Enabling these is mechanical. The real work is **measuring what they cost**: each grows the guest kernel, which is loaded into RAM and therefore lands in every `mem.bin` (§8.3), and `MEMCG` in particular carries measurable runtime overhead. Because the kernel ships as a node component (§3.2), this is a node-rollout item.

**VMM.** Three additions to `nvx`:

- **virtio-blk** — content reaches the guest through `phram` today (§4.3). This is a subsystem, not a device: a multi-device virtio-mmio bus, IRQ delivery, queue workers, read-only and short-read semantics, and serialization of negotiated features, queue addresses, ring indices, and in-flight I/O into `state.bin`, which today carries only the legacy devices and virtio-net.
- **Capture as a host transaction** — the capture path must drain block queues, flush backing files, clone the quiesced scratch, validate the restore contract against a host-owned manifest before vCPU entry, and publish artifacts atomically (§8.5). The guest is *not* resumed after capture (§8.1), so this needs no reversible run-state machine — the existing one-shot save-and-exit shape is correct, extended with queue draining, manifest handling, and a status result reported to the host.
- **A restore gate** — device interrupts, network ingress, and host control input must be held until the agent signals that its post-restore work is complete (§8.5).

The network identity `nvx` binds into a snapshot currently hashes routes, DNS servers, and search domains together with the L2/L3 identity, and rejects any change on restore. Since those carry tenant-specific values while the L2/L3 identity does not (§4.1), that binding must narrow to MAC, address, prefix, and MTU.

Host-triggered capture — a VMM-level capture of a guest whose agent is unresponsive — is *degraded mode only* and can be deferred; every snapshot tier is captured by the agent (§8.5).

**Host storage.** The volume holding snapshot artifacts and scratch templates must support **block cloning**, which per-tenant snapshot instantiation (§9.2) and the scratch pool (§5.1) both depend on:

| Platform | Supported | Not supported |
|---|---|---|
| Windows (production) | **ReFS** — `FSCTL_DUPLICATE_EXTENTS_TO_FILE`, Server 2016+ | **NTFS** — has hard links and dedup, neither of which separates page cache |
| Linux (development) | **XFS, btrfs** — `FICLONE` | **ext4** |

Windows **page combining must also be disabled** on these nodes, since it would re-share physically identical pages across sandboxes and undo the separation §9.2 relies on.

---

## 3. Image Preparation (Off the Critical Path)

**Principle: nothing expensive happens while a sandbox is starting.** All image processing is done once, ahead of time, by a control-plane conversion service.

```
customer image reference
        │
        ▼
[ control plane: image known? ]───no──►[ conversion pipeline ]
        │ yes                                   │
        ▼                                       ▼
  blob already staged on node          pull → unpack → flatten to layer trees
        │                              → mkfs.erofs (compressed) per layer
        │                              → publish layer blobs, content-addressed
        └──────────────┬───────────────────────┘
                       ▼
              node-local blob cache (content-addressed)
```

- **Trigger:** an unknown image reference triggers the pipeline; the sandbox start waits on it (a one-time cost per image, not per sandbox). Subsequent starts hit the staged artifact.
- **Where it runs:** a secondary service, **not on the node's start path**. This matters concretely because `mkfs.erofs` is a Linux tool and ACI Sandbox nodes are Windows hosts running the WHP backend of `nvx` — running the builder on a Linux conversion service sidesteps needing an EROFS writer for Windows entirely. (Precedent: `nvx` had to implement a Rust `fatfs` writer for WHP because `mksquashfs` was unavailable on the host; we should not repeat that.)
- **Registry credentials** stay in the control plane / conversion service. The node never needs them, and the guest certainly never does.
- **Addressing and GC:** blobs are content-addressed. Refcount against both running sandboxes **and** snapshots that reference them, so GC cannot delete a layer a snapshot depends on.

### 3.1 Layer model

The guest filesystem is composed from a fixed, platform-defined set of layers, all computed ahead of time. The count is not derived from the source image's layer count:

| Layer | Contents | Required | Shared with | Distribution |
|---|---|---|---|---|
| **initramfs** | The init agent and its minimal userland | Yes | Every sandbox on the fleet | Node deployment |
| **distro** | Base OS userland | Yes | Every sandbox using that distro | Node blob cache |
| **runtime** | Language runtime / framework (e.g. Python + common libraries) | Yes | Every sandbox using that runtime | Node blob cache |
| **custom** | Customer customization | Optional | Every sandbox of that customer image | Node blob cache |
| **scratch** | The container's writable layer | Yes | Nothing — private per sandbox | Node-local pool |

The initramfs is loaded into RAM with the kernel and is the agent's own rootfs; it is not part of the container's filesystem view. The read-only layers below it form the container's `lowerdir` stack, and scratch is its `upperdir`.

The ordering is a deliberate **sharing gradient** — widest-shared at the bottom, narrowest at the top — which maximizes reuse in the node blob cache and the host page cache. It also aligns with the three snapshot tiers in §8.1 (platform / runtime / customer), so a snapshot tier and the layers it depends on are the same cut of the stack.

The custom layer is **optional**: a sandbox running an unmodified distro+runtime combination omits it entirely and mounts a two-layer `lowerdir`. This is the common case for the platform's own curated images, and it means those sandboxes share every read-only byte with each other. The stack is therefore 1–3 read-only layers, which is one of the dimensions a snapshot is keyed on (§8.4).

**Decomposing an arbitrary customer image.** OCI images do not record their ancestry, so the distro/runtime/custom split cannot be inferred from an image's own layer structure. The conversion service determines it by **layer-digest prefix matching against a registry of curated bases**: if a customer image's first *N* layer digests are exactly a known base's layers, it was built `FROM` that base and splits there. The pipeline picks the deepest such match, so an image built on a curated runtime yields all three layers, one built on a bare distro yields two, and one that matches nothing yields a single custom layer over an empty base.

That fallback is correct but unshared — such a sandbox has no layer in common with any other, so it gets no cache reuse, no page-cache sharing, and no benefit from the platform snapshot tier. Customers are therefore encouraged, not required, to build on curated bases. Note the matching is exact: an image rebuilt from identical content but different digests will not match, and falls back to the single-layer case.

Keeping the layers as *separate blobs* rather than pre-merging them is the central point. A merged image would be one blob per `(distro, runtime, custom)` combination, duplicating the widely-shared distro and runtime content across every combination — in the node cache, in the artifact store, and in the host page cache. Separate blobs mean one copy of `distro` on the node no matter how many runtimes and customizations are in play.

Deletions and replacements in the custom layer are materialized at build time as overlayfs whiteouts (`char 0:0`) and opaque-directory markers (`trusted.overlay.opaque="y"`), synthesized by the pipeline rather than copied from the image (§5). EROFS supports the full `trusted.*` and `security.*` xattr namespaces, including compressed long-name prefixes for `trusted.overlay.` (kernel 6.4+), so container images with heavy whiteout usage and SELinux/capability metadata round-trip faithfully.

### 3.2 Two distribution channels

Guest artifacts reach a node by two distinct routes, and the split follows ownership: platform-owned code ships with the node, content-addressed data flows through the cache.

**Node deployment — the guest kernel and the initramfs.** These are **node components**, versioned, packaged, and rolled out as part of the node's own deployment, exactly like the VMM and the host-side runtime. They are present on every node before any sandbox starts. Consequences:

- **They can never miss.** There is no fetch, no cache lookup, and no failure mode where a sandbox cannot start because its agent is not yet on the node.
- **Agent version is a property of the node, not of the image.** A given node runs exactly one agent build for every sandbox it hosts, which makes the host↔agent RPC contract (§7.3) a per-node version rather than a per-workload matrix.
- **Upgrades follow node rollout.** Shipping a new agent means a staged node rollout with the usual safety controls — canary, health gates, rollback — rather than a data-plane push. That is the appropriate cadence for the one component that runs as PID 1 in every guest.
- **The kernel and initramfs travel together** and should be versioned as a unit, since the agent depends on kernel configuration (EROFS, overlayfs, virtio drivers) being present.

**Node blob cache — the layer blobs.** `distro`, `runtime`, and `custom` are content-addressed data produced by the conversion service and pulled on demand into the node's cache, where they are shared across every sandbox using them and refcounted against running sandboxes and snapshots. This is the channel that must handle cache misses, GC, and eventually P2P distribution (§13.1).

**Node-local pool — scratch.** Created on the node itself, never distributed (§5.1).

The practical effect is that the only artifacts on a sandbox's start path that can be *absent* are the layer blobs, so cache-miss handling has exactly one place to live. It also means a snapshot's dependencies split the same way: the snapshot embeds the agent build it was captured with (making agent upgrade a snapshot-invalidation event), while its layer blobs are external references that must remain present in the cache (§8.3).

---

## 4. Blob Layout

Each layer is its own virtio-blk device, cold-plugged at VM creation. There is no partition table, no layer manifest, and no discovery step:

```
(initramfs, loaded with the kernel)   agent rootfs — not part of the container view

/dev/vda   EROFS  ro    distro      ─┐
/dev/vdb   EROFS  ro    runtime      ├─ lowerdir (bottom → top)
/dev/vdc   EROFS  ro    custom      ─┘  (optional; absent for uncustomized images)
/dev/vdd   ext4   rw    scratch         upperdir + workdir
```

**Roles are named in the sandbox configuration** (§4.1), not inferred from device order. A role names the device's **MMIO base address or device serial** — attributes the VMM assigns and therefore knows — and the agent resolves that to a block device through sysfs. It deliberately does not name a `/dev/vdX` path or a `virtioN` bus name: both are derived by Linux (`virtioN` from its own bus enumeration, `vdX` from a global IDA inside `virtblk_probe`), so a host that named them would be predicting guest-kernel behaviour rather than stating a fact it controls. Resolving from an address the VMM chose keeps the agent free of layout parsing, stays correct when the optional custom layer is absent, and survives the unbind/rebind path in §8.4.

One blob per layer, rather than one disk carrying all of them, is what makes the sharing model in §3.1 work: the blob file is the unit of caching, so a single `distro` blob is opened once on the node and its host page cache is shared by every sandbox using it, regardless of which runtime or customization sits above.

### 4.1 Sandbox configuration

**All per-sandbox configuration is delivered in a single VMM-populated memory region.** The agent reads one structure at boot; there is no second source and no per-image config baked into any blob.

The kernel command line carries only two things: the **base and length of that region**, and ordinary kernel parameters (`console=`, `quiet`, and so on).

Three constraints force this split:

1. **Size.** `COMMAND_LINE_SIZE` is 2048–4096 bytes on x86; a realistic config (entrypoint, args, env) exceeds it.
2. **Secrecy.** `/proc/cmdline` is world-readable *inside the container*, so secure environment variables must never appear there.
3. **Mutability across restore.** The command line is parsed once at boot and lives in guest memory thereafter; it comes back from `mem.bin` unchanged and **cannot be rewritten on restore**. Anything that varies per sandbox must therefore live somewhere the VMM can repopulate — which is exactly what the region is for (§8.4).

Constraint 3 is the decisive one: it means the command line is only suitable for values that are *invariant across every restore of a snapshot*. The region's location qualifies (it is a guest-physical address fixed at VM creation); nothing about a particular workload does.

**Contents.** The region holds the sandbox's complete, **fully-resolved** configuration, in two sections distinguished by whether they may change across a restore:

**Contents.** The region holds the sandbox's complete, **fully-resolved** configuration in three sections, separated by when each becomes valid:

**1. Invariants — must match the snapshot they are restored against.** State the captured guest already depends on:

- Guest-visible network identity — MAC, address, prefix, MTU. These are **platform-owned generic values**, not tenant-derived: §11 explains why holding them constant across restores is what makes the design work, and the external endpoint carries the per-launch difference. Tenant-specific routing and resolver settings are deliberately *not* here (see §3 below).
- Device topology and geometry — which MMIO address carries which role, the uniform layer capacity, and the layer count.

The agent reads this section **before** the platform snapshot point and programs the interface from it.

**2. Image binding — supplied per restore.** Which blobs are attached for *this* sandbox:

- The EROFS superblock UUID expected on each layer device.

This is deliberately not an invariant. A platform snapshot is captured before any layer is mounted, so it is image-agnostic and one snapshot serves any image of the same topology; the layer identities are therefore whatever the current restore attached.

**3. Sandbox — supplied per restore.** Everything that distinguishes one sandbox from another:

- OCI runtime config — entrypoint, args, env, workdir, user, capabilities — with image defaults and per-sandbox overrides already merged by the control plane, which holds both.
- Secure environment variables.
- Tenant resolver and routing configuration — DNS servers, search domains, and any non-default routes. These may encode tenant-specific values, so they are applied by the agent after the platform snapshot point rather than baked into a fleet-shared artifact. Writing `/etc/resolv.conf` and installing routes is cheap and stays off the shared-capture path.
- A **launch epoch**, and for a clone, a fresh entropy seed and wall-clock reading (§8.2).

Sections 2 and 3 are read **after** the platform snapshot point, which is what keeps secrets and image bindings out of a fleet-shared snapshot (§8.4).

**What a restore may change depends on the tier.** The section boundaries describe when the *agent* consumes each value, so a snapshot can only redefine what had not yet been consumed when it was captured:

| Tier | Captured before reading | Therefore redefinable on restore |
|---|---|---|
| Platform | §2 and §3 | Image binding, OCI config, secrets, resolver/routing |
| Workload start | nothing — all sections consumed | Work items handed to the shim only (§8.5) |
| Instance checkpoint | nothing — all sections consumed | Work items only |

Supplying a fresh §2 or §3 to a workload-start or instance-checkpoint restore would be silently ineffective: those values were already resolved into mounted filesystems and running process state at capture. The manifest records which sections a given snapshot consumed, and a restore that supplies different values for a consumed section is rejected rather than ignored.

**The host validates the launch contract before vCPU entry.** The manifest carries a canonical digest over the invariants section and the device contract. On every launch `nvx` recomputes that digest from the configuration it is about to write and the devices it is about to attach, compares it against the manifest, and refuses to enter the vCPU on mismatch (§2.4).

The host is where this check belongs because it is the party holding an independent statement of what the sandbox is supposed to be: the placement decision, the blob set, and the device layout all originate there. A snapshot cannot vouch for itself, and a check that runs after vCPU entry runs after devices are already live. The agent re-reads the invariants section on boot and fails with a typed error if it disagrees, which catches VMM-side construction bugs that the digest would not.

Resolving image defaults against overrides on the host rather than in the guest keeps the agent free of OCI-config merge semantics and removes any need for it to read configuration out of a mounted layer before it can act.

**The launch epoch distinguishes a cold boot from a restore.** The VMM writes a monotonically increasing value into the region on every launch. Because the region is never captured (below), the agent compares the epoch it reads against the one recorded in its own memory — which *is* captured — and a mismatch means it is running from a snapshot rather than continuing a cold boot. This is the trigger for the post-restore work in §8.2; the guest has no other way to detect its own restoration, since a capture leaves no observable trace in CPU or memory state.

**Layer identity is a mix-up check, not an integrity control.** The agent compares each device's EROFS superblock UUID after mount. This is a cheap constant-time check that the expected blob was attached — not integrity verification, and it must not be mistaken for one. Full content hashing is done once by the node when a blob is admitted to the cache; re-hashing a multi-gigabyte blob inside the guest would read the entire artifact, defeating both the latency budget and demand paging. The host is trusted in this design (§2.2), so the threat defended against is operational error, not attack.

**Mechanics.** `nvx` carves guest-physical ranges above reported RAM for `phram`, omits them from the PVH usable-memory map, and passes base/length on the kernel command line (`phram.phram=virtfs,<base>,<length>`). The config region uses that machinery: the VMM writes a small region (e.g. 64 KiB) containing a header — magic, version, length, checksum — followed by the payload, and passes its base and length as cmdline tokens. Being plain memory, it costs no I/O and no storage-stack involvement; the VMM writes it before vCPU entry.

> **Requirement: the region must live outside `ram_size`.** `Memory` holds only the low/high RAM regions split at `MMIO_GAP_START`, and `snapshot_ram` walks exactly those, so a region registered as a separate memory slot above reported RAM is absent from `mem.bin`. That is what lets the VMM supply fresh contents on every restore (§8.3). Placing it inside guest RAM would reverse this: restore would overwrite the VMM's configuration with the captured copy.

**Secrecy posture.** The region is guest-physical memory, readable by anything in the guest holding `CAP_SYS_RAWIO` or the backing device — but not by the container, which gets neither. The agent must not expose the region into the container's mount namespace or `/dev`. Zeroing it after use is worthwhile defence in depth, but does not make the secrets unrecoverable: they persist in the agent's own buffers and in the container's environment.

**Updates.** The `StartContainer` RPC can also carry configuration, at the cost of a host→guest round trip. The region is used for initial configuration so the start path stays round-trip-free (§6); the RPC is the mechanism for changing configuration on an already-running or restored sandbox.

### 4.2 Compressed EROFS

Layers are built as **compressed EROFS** (LZ4 or zstd). Benefits, in rough order of importance here:

- **Smaller artifacts** → less to distribute from the conversion service to nodes, faster staging, lower storage cost in the node cache and artifact store.
- **Smaller host page cache footprint** — the host caches *compressed* blocks, so a given amount of host RAM caches proportionally more image content, and that cache is shared across every VM reading the same blob file.
- **Native read-only design** — no journal, no superblock writes, compact metadata, fast mount.
- **In-kernel driver** — decompression happens in the guest kernel. No FUSE daemon inside the guest, and no userspace helper on the host.
- **File-backed mounts (kernel 6.12+)** — EROFS can mount an image file without a loop device, which removes a real kernel-resource ceiling when a node runs many sandboxes.

The cost is guest CPU to decompress, paid once per block as it is first read.

> **Note:** compression and DAX are mutually exclusive — EROFS supports DAX only for *uncompressed, non-inlined* files. Since §9 defers DAX on security grounds, compression is unambiguously the right trade today. If DAX is ever adopted, that decision reverses and images must be built uncompressed with `-E noinline_data`.

### 4.3 phram as an option for small layers

`nvx` can also expose content through **phram → `/dev/mtdblock*`**: the VMM maps a filesystem image into a guest-physical range above reported RAM and the guest's `phram`/`mtdblock` drivers surface it as a block device. This is available as an alternative carrier for **small layers**, where mapping the whole image into guest-physical memory is cheap and avoids the virtio-blk path entirely.

It does **not** scale, and is not the general mechanism. A phram region is guest-physical memory: its cost is proportional to the *entire image size*, not to what is read, and it is charged up front. For large images — a Conda/ML environment can be many gigabytes — this is untenable. virtio-blk is the general-purpose path precisely because its cost scales with what the workload actually touches.

**Selection rule:** phram below a size threshold, virtio-blk above it. Both terminate in the same overlay assembly, so the agent logic is nearly identical.

---

## 5. Guest Filesystem Assembly

The agent performs the assembly. The host does not construct any part of the container's filesystem view.

```
/dev/vda (EROFS, ro, distro)  ──┐
/dev/vdb (EROFS, ro, runtime) ──┼─► lowerdir=[/l/custom:]/l/runtime:/l/distro
/dev/vdc (EROFS, ro, custom)  ──┘   (custom omitted when absent)
                                        overlayfs ──► /rootfs ──► pivot_root
/dev/vdd (ext4, rw, scratch)  ──► upperdir=/s/u, workdir=/s/w
```

Note the `lowerdir` ordering: overlayfs takes lower layers **top-first**, so the customer's layer is listed first and the distro last — the reverse of the device order.

Mount options: `metacopy=on` (a `chmod`/`chown` on a lower-layer file copies only metadata, not data) and `xino=on` (stable `st_ino`/`st_dev` across the overlay, which matters for applications that cache inode identity — note `xino` can fall back to non-unique inode numbers when the underlying high bits overflow, so the chosen EROFS/ext4/kernel combination should be validated rather than assumed).

**`metacopy=on` requires trusted lower layers, which the conversion pipeline must guarantee.** The kernel documentation is explicit: *"Do not use metacopy=on with untrusted upper/lower directories. Otherwise it is possible that an attacker can create a handcrafted file with appropriate REDIRECT and METACOPY xattrs, and gain access to file on lower pointed by REDIRECT."* Setting `trusted.*` xattrs normally requires `CAP_SYS_ADMIN` and so is out of reach of an image author — but the conversion service (§3) runs privileged and writes the layer blobs, so anything it faithfully copies out of a customer image becomes a `trusted.*` xattr on a lower layer.

The pipeline therefore applies a **deny-by-default metadata allow-list** when translating image content into layer blobs. Attributes are dropped unless explicitly permitted, which covers more than the overlay namespace:

- `trusted.overlay.*` and `user.overlay.*` — never copied. Whiteouts and opaque-directory markers are **synthesized** by the pipeline from the image's own deletion semantics, so the only overlay metadata in a layer is metadata the platform wrote.
- `security.capability` — a crafted image could otherwise grant file capabilities to arbitrary binaries. Permitted only where the platform's policy allows it, and normalized rather than passed through.
- `security.selinux`, POSIX ACLs, and any other `security.*` attribute — carried only under an explicit policy decision, since each grants or relaxes access inside the guest.

The allow-list applies to **every** layer, not only the customer's custom layer: distro and runtime blobs are pipeline-built too, and a lower layer is exactly where a crafted redirect would be most useful. Raw `char 0:0` whiteout inodes are not xattrs and are handled by the same synthesis path rather than by copying. With this in place the lower layers are trusted by construction, which is the precondition `metacopy=on` needs.

### 5.1 Scratch: virtio-blk from a pre-formatted VHD pool

The writable layer is a **virtio-blk device backed by an empty VHD** (or equivalent blob format), pre-formatted to a filesystem the guest can mount directly.

Scratch VHDs are produced **off the critical path** by a warm pool — a pattern already implemented host-side in `ScratchProvider`, which maintains a pool of pre-created VHDXs using a `.tempvhdx` → `.vhdx` rename to publish only fully-formed files. Sandbox start takes a ready VHD from the pool (or reflink-clones a template, which is near-instant on a CoW filesystem); it never runs `mkfs` on the start path.

**Why not tmpfs.** tmpfs was considered and rejected:
- tmpfs consumes **guest RAM**, so scratch usage silently competes with the customer's memory allocation and can trigger guest OOM instead of a clean `ENOSPC`.
- Capacity is bounded by VM memory, which is far too small for image-build or data-processing workloads.
- A block-backed scratch can be sized independently of memory, and can be retained or discarded explicitly.

**Copy-up behavior to design around.** overlayfs copy-up is **whole-file and eager**: the first write to a file copies the entire file from the lower layer into scratch. There is no page-granularity CoW. Consequences: large mutable files should not live in the image (mount scratch or a volume over their directories instead), and scratch must be sized with copy-up amplification in mind.

---

## 6. Boot Sequence

Configuration flows in two channels with a clean split of responsibilities:

- **VMM config region** — the sandbox's resolved configuration, in three sections read at the points where each becomes valid (§4.1). The kernel command line carries only the region's base/length plus ordinary kernel parameters.
- **RPC over stdio** — everything at runtime (§7).

This follows the pattern `nvx` uses for `phram`, where the VMM carves a guest-physical range and describes it with cmdline tokens for PID 1 to consume. The region's schema is a **versioned contract**, with the same discipline applied to the version-3 `--net-config` JSON manifest.

The order below is load-bearing rather than incidental: the network is programmed before the platform snapshot point, and nothing image-specific or secret is touched until after it (§8.4).

```
 1. Host: select layer blobs + scratch VHD from pool; write config region; cold-plug all devices
 2. Host: (external networking) DataPlaneReady ──► StartVm handshake before vCPU entry
 3. VMM:  PVH-boot kernel + initramfs
 4. Agent (PID 1): mount /proc, /sys, devtmpfs, cgroup2; create agent + container cgroups
 5. Agent: read config §1 (invariants) → device topology, guest-visible network identity
 6. Agent: configure interface (static address, no DHCP)
    ─────────────────── ◆ platform snapshot point ◆ ───────────────────
 7. Agent: read config §2 + §3 → layer UUIDs, OCI config, secrets, resolver/routing, epoch
 8. Agent: apply resolver and routing configuration
 9. Agent: resolve MMIO addresses → block devices; assert capacities
10. Agent: mount the read-only layer blobs (EROFS) → /l/distro, /l/runtime, [/l/custom];
           verify each layer's EROFS UUID
11. Agent: mount scratch (ext4, rw) → /s
12. Agent: mount overlayfs → /rootfs
13. Agent: if restoring, apply clock and entropy for this launch (§8.2); zero the config region
14. Agent: emit Ready over the control channel
15. Agent: clone3(CLONE_INTO_CGROUP) a child into the container cgroup and
           new mount/PID/UTS namespaces
16. Child: pivot_root into /rootfs, apply OCI config, exec entrypoint
17. Agent (still PID 1): supervise — reap orphans, stream stdio, serve RPC
```

Steps 5–6 read and apply only platform-owned, image-independent values. Everything tenant-specific — image bindings, secrets, resolver and routing settings — is read at step 7, after the snapshot point, which is what allows a platform snapshot to be shared across tenants (§8.4).

**A platform snapshot is captured against placeholder devices.** When building one, step 1 attaches deterministic placeholder blobs of the uniform capacity rather than any tenant's layers. This matters because the kernel's block probe reads the first sectors of each device before PID 1 runs, and those bytes enter the guest page cache and therefore the captured image. Attaching real blobs during capture would embed one tenant's image content in an artifact restored by every other. Restores attach the real blobs, whose probe-time cache is discarded before mount (§8.4).

Two further steps deserve explanation.

**Step 15 does not `exec` the entrypoint.** `exec` replaces the process image, which would destroy the agent and with it supervision, log streaming, and the control channel. The agent stays PID 1 in the initial namespace and clones a child; `pivot_root` happens inside the *child's* mount namespace, so the agent retains its own initramfs view, and the child's PID namespace makes the entrypoint PID 1 from its own perspective (§10.1).

**The child is placed in its cgroup atomically.** `clone3` with `CLONE_INTO_CGROUP` creates the child directly in the container cgroup. Creating it first and moving it afterwards would leave a window in which the child could fork or exec outside the resource limits and outside the freeze that the capture protocol depends on (§8.5).

On a cold boot the whole sequence runs. On a restore the guest resumes inside step 6 and continues from step 7 with a config region the VMM has populated for this sandbox.

Nothing between VM start and `exec` requires a host round trip. Additional latency reductions applied: direct kernel boot (no firmware), minimal kernel config, `quiet`, no serial console noise, and static network configuration.

---

## 7. The Agent

### 7.1 Rust init agent

A single static Rust binary, PID 1, shipped in the initramfs. It is the guest's only platform component, subsuming the role an init system would otherwise fill. Responsibilities: pseudo-filesystem setup, filesystem assembly, network programming, container construction, process supervision (including orphan reaping, which PID 1 must do), log streaming, and RPC service.

**"Container construction" is a real OCI runtime, and should be scoped as one.** The steps are mechanical in the sense that the host has already decided every value — but they are not few. The agent must create the mount, PID, UTS, and cgroup namespaces; `pivot_root`; mount `/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm`; apply masked and read-only paths; set the capability bounding and effective sets, no-new-privs, seccomp profile, rlimits, uid/gid and supplementary groups; write the cgroup limits; and set up the entrypoint's stdio, including PTY allocation when a terminal is requested. What the single-container model removes is *dynamism* — none of this is renegotiated after start — not the surface itself.

Rust is chosen for a static musl binary with no runtime dependencies, memory safety in a PID-1 process where a crash is a guest kernel panic, and small size (initramfs is loaded into RAM and lands in every snapshot).

### 7.2 Control channel: framed RPC over VMM-owned stdio

`nvx` has **no vsock or virtio-serial device**. It does have a console: the VMM forwards guest console output to host `stdout` and host `stdin` into the guest console queue. Critically, **this channel is owned end-to-end by the VMM** — it is not a guest-visible network device, it cannot be reached from outside the host process, and it needs no additional device model, driver, or address space.

We therefore multiplex the control plane over this existing channel rather than adding vsock:

```
┌─────────────┐   stdout  ┌──────────┐  console  ┌──────────────┐
│ SandboxRt   │◄──────────│   nvx    │◄──────────│    agent     │
│  (host)     │──────────►│  (VMM)   │──────────►│   (PID 1)    │
└─────────────┘   stdin   └──────────┘           └──────────────┘
        framed, multiplexed: ctrl / stdout / stderr / stdin
```

Framing carries a stream ID so container stdio and control RPC share one pipe without interleaving corruption. Boot-phase output remains human-readable plain text for diagnosability; the channel switches to framed mode once the agent is up.

**The channel is versioned by launch epoch.** A capture can occur with a partially decoded frame in the agent's buffers and with host-issued requests still outstanding, and on restore the host side is a different process holding a different pipe. The channel therefore carries the launch epoch (§4.1): the agent discards any partial decoder state, drops requests belonging to a previous epoch rather than replying to identifiers whose requester no longer exists, and opens the restored channel with a `RestoreHello` that re-establishes the host's view before any other traffic. Requests that are not safely replayable are failed rather than retried, so a checkpoint taken mid-RPC cannot cause the operation to take effect twice.

Because the capture path parks the agent's worker threads before requesting the snapshot (§8.5), frames are quiesced at a boundary rather than mid-write.

### 7.3 RPC surface

Derived from the operations the existing `GuestAgentService` and the HCS/GCS guest interface already require, reduced to what a single-container sandbox needs:

| Operation | Purpose |
|---|---|
| `Ready` (agent → host) | Boot complete; filesystem assembled; includes guest info |
| `Bootstrap` | Filesystem + network programming, when not fully driven by the config region |
| `ExecuteCommand` | One-shot exec: argv, cwd, env, timeout → exit code + stdout/stderr |
| `InteractiveShell` | Bidirectional streaming shell with PTY, window resize, signals |
| `StreamLogs` | Container stdout/stderr, with follow and since-token |
| `Signal` | Deliver a signal to the container (SIGTERM for graceful stop) |
| `Wait` / `ContainerExited` | Exit code and termination reason (including OOM) |
| `Probe` | Execute a liveness/readiness probe |
| `PrepareSnapshot` / `PostRestore` | Snapshot capture and restore hooks (§8.5) |
| `Checkpoint` | Host-initiated capture of a running sandbox — the instance-checkpoint tier (§8.1) |
| `Shutdown` | Graceful guest shutdown |

**Errors are part of the interface.** Every operation returns a structured error with a stable code, not a log line. A PID 1 that fails by rebooting destroys all diagnostic context; the agent must instead report (for example) "layer blob failed to mount", "entrypoint not found", "scratch full" as typed errors the host can translate into customer-visible ACI failure reasons.

### 7.4 Resource guarantees for the agent

Although the VM's sizing is the container's limit, the agent must remain responsive **even when the container is exhausting the VM's resources** — otherwise the platform loses exactly the ability it needs most: reporting that the container OOM'd, streaming its final logs, and accepting a shutdown.

This is the one place we deliberately keep cgroups. Both the agent and the container are placed in **sibling** cgroups under the root — `memory.min` and `cpu.weight` do not exist on the root cgroup, so leaving the agent at the root cannot protect it:

```
/sys/fs/cgroup/
├── agent/       memory.min = <reserve>, cpu.weight = high
└── container/   memory.max = <customer request>, memory.oom.group = 1
```

- Size the **VM** at the customer's requested memory plus the agent reserve.
- `memory.min` on the agent means guest OOM kills inside the container's cgroup and the agent survives to report it. Without this, the OOM killer may pick the agent and the failure surfaces as an opaque VM death.
- `memory.oom.group=1` on the container makes OOM kill the whole container as a unit rather than one process, so partial-kill states do not have to be reasoned about.
- `cpu.weight` keeps the agent schedulable under container CPU saturation.
- The container cgroup is also the attachment point for a **`BPF_PROG_TYPE_CGROUP_DEVICE`** filter restricting which device nodes the container may create (see §9, §10.1).

Requires `CONFIG_MEMCG` and `CONFIG_BPF_SYSCALL`, both currently disabled (§2.4).

Trade-off to note: the platform reserve is memory the customer pays for the VM but cannot use. It should be small and measured.

---

## 8. Snapshots

`nvx` snapshots are guest-initiated (a write to control port `0x605`), producing `mem.bin` (sparse guest RAM) + `state.bin` (CPU/device state), restored copy-on-write so a single snapshot can be replayed many times.

### 8.1 Three tiers

The tiers differ along three axes that matter more than where they sit in the boot sequence: **who authors them**, **how widely they may be shared**, and **whether a restore is a clone or a resume**.

| Tier | Trigger | Captured at | Sharing scope | Restore semantics |
|---|---|---|---|---|
| **Platform** | Agent, at a fixed point in its boot sequence | Network programmed; before secrets are read or any layer is mounted (§8.4) | **Fleet-wide, cross-tenant** | Clone, 1:N |
| **Workload start** | Guest signal from the customer's own workload | Wherever the customer's code declares a useful starting point | **Single tenant** | Clone, 1:N |
| **Instance checkpoint** | External signal from the customer | Arbitrary point in a live instance | **That one instance** | Resume, 1:1 |

All three are captured by the agent, and in all three the capturing instance ends at the capture point (below).

**Platform** is the only cross-tenant tier, and is safe to share precisely because of *what* it contains: the guest kernel and the agent, with no image content, no customer code, and no secrets (§8.4, §9).

It earns its place on absolute savings rather than proportional ones. Restoring past kernel boot and agent initialization removes a few hundred milliseconds from every cold start on the fleet, and it removes them from *every* sandbox regardless of image, runtime, or customer — it is the one tier that needs no customer participation and no per-runtime work. For the short-lived sandboxes this design targets, a few hundred milliseconds is frequently half or more of total wall time, so the tier is retained even where a warm-workload snapshot would save more in relative terms. The two are complementary: platform removes boot, workload start removes runtime initialization.

**Workload start** exists because customers often know where their workloads begin — a warmed interpreter with modules imported, a loaded model, compiled bytecode in place — and restoring from that point avoids paying startup repeatedly. The typical motivation is skipping **interpreter startup**, which for a Python or Node workload dominates cold start: the difference between a cold start and a restore from a warmed interpreter is measured in seconds, not milliseconds.

Two properties follow from it being customer-authored:

- It is **tenant-scoped by construction**, not by policy. Customer code is running at capture, so the snapshot necessarily contains that customer's code and memory. There is nothing to decide.
- It requires a **runtime-specific warm shim**. "Avoid interpreter startup" means the restored process must then run *this* sandbox's work, so the warm point is an interpreter that has finished initializing and then requests a checkpoint; the agent returns this sandbox's work item as the reply after restore (§8.5). Python, Node, and Java each need their own shim; this is not a generic mechanism.

**Instance checkpoint** is pause and resume of one VM, driven by explicit customer signals relayed to the agent as an RPC.

**Capture terminates the capturing instance.** The VMM writes the artifacts and exits; it does not return the guest to execution. Every restore is therefore a fresh instance continuing from the captured point, and the captured instance never continues past it. This is what makes the tier semantics clean:

- A **pause** is genuine: the instance stops, and the single restore that follows is its one continuation. Identity and RNG continuity are correct because no other continuation exists.
- Had the capturing instance resumed, a later restore would be a *fork* rather than a resume — the original may already have emitted nonces, written to external systems, or advanced its RNG stream, so two instances would share a history that only one of them is entitled to. Terminating at capture removes that hazard by construction rather than by convention.

The consequences are stated plainly: **checkpointing is a build-shaped operation, not something performed mid-request**, and a *failed* capture also ends the instance. Failure is therefore reported by the VMM to the host (§8.5); there is no guest left to report it. For the workload-start tier this fits naturally — a job runs to its declared starting point and produces an artifact — and for the instance-checkpoint tier stopping is the intended behaviour.

**Cross-tenant workload start uses per-tenant instantiation.** A platform-authored warm runtime is more valuable than a per-customer one, but sharing a single `mem.bin` across trust domains creates the cross-VM page sharing §9.1 rules out. Each tenant therefore restores from its **own reflinked copy** of the snapshot: a distinct inode sharing the same disk extents, giving distinct physical pages at near-zero storage cost (§9.2). The platform authors the warm snapshot once and clones it per tenant.

### 8.2 Restore modes: clone and resume

A restore is one of two things, declared by the host in the snapshot manifest rather than by the guest.

**Resume (1:1)** returns a paused instance to where it left off, exactly once. Identity continuity is *correct*: machine-id, hostname, and the RNG stream carry over, and refreshing them would be a bug. This is sound only because the capturing instance was terminated (§8.1), so the restore is the sole continuation; the host enforces single use by claiming the artifact atomically, and a second attempt is rejected rather than silently producing a fork.

**Clone (1:N)** produces many guests from identical captured state. The platform and workload-start tiers are always clones. Three pieces of state are then wrong and must be corrected before the workload runs:

**1. Randomness.** Identical entropy across clones means duplicate UUIDs and, in the worst case, repeated cryptographic material. Correcting it has two halves:

- *Kernel.* Writing the VMM-supplied seed to `/dev/urandom` only mixes bytes into the input pool; it does not reseed the CRNG, so a clone would keep producing the captured stream for some time. The agent therefore uses `RNDADDENTROPY` to credit the seed and `RNDRESEEDCRNG` to force an immediate reseed, both before the container is thawed.
- *Userspace.* Runtime and library DRBG state — OpenSSL, Python's `random`, Java's `SecureRandom` — is already captured inside the workload's memory and cannot be reached from outside the process. Any warm shim (§8.1) must reset its own runtime's RNG state before accepting work. This is a hard limit on customer-authored 1:N checkpoints of arbitrary processes and is stated as such in the replay contract (§8.5).

**2. Wall clock.** A clone resumes with the capture instant's time, breaking TLS certificate validation and token expiry. The emulated RTC returns correct time when read, but Linux reads it once at boot and runs its own timekeeping thereafter, so nothing re-reads it on restore. The agent explicitly re-reads the RTC and sets the system clock. This is distinct from the TSC synchronization `nvx` performs across vCPUs.

**3. Identity.** Hostname, machine-id, and container ID are refreshed from the config region.

All three run in the agent's `PostRestore` path while the container is still frozen (§8.5).

### 8.3 What is and isn't in the snapshot

Image and scratch content are **not** in the snapshot — block-device backing files are external, exactly as the layer blobs and scratch VHD are. The snapshot references them; restore requires them present. This has two consequences:

- **Blob lifetime** must be refcounted against snapshots, not just running sandboxes (§3).
- **The config region is supplied fresh, not restored.** It sits outside guest RAM proper and is therefore not in `mem.bin`; the VMM writes it for every restore, before resume. Because it carries the image binding and the sandbox's entrypoint, environment, and secrets, this is what allows one platform or workload-start snapshot to be fanned out to many differently-configured sandboxes (§4.1). The agent validates the invariants section against what it recorded before the snapshot.
- **Scratch consistency.** Once scratch is mounted, the guest page cache holds dirty ext4 data that *is* captured in the memory snapshot, while the scratch file on the host is at a different point in time. Restoring mismatched pairs corrupts the filesystem. Which tiers this applies to follows directly from where each is captured:
  - **Platform tier** — scratch is attached but never mounted at the snapshot point (§8.4), so there is no dirty state and nothing to pair. Every restore simply takes a fresh VHD from the pool.
  - **Workload start tier** — scratch **is** mounted and written: warming a workload produces bytecode caches, temporary files, and other real filesystem state. Memory and scratch must therefore be captured as an **atomic pair**, and every restore must clone *that* scratch image rather than take a generic pool VHD. The saving grace is volume: the scratch is nearly empty, so the paired image is small and reflink-cloning it per restore is near-instant.
  - **Instance checkpoint tier** — the same pairing requirement, but scratch may hold arbitrary amounts of data, so the clone is proportionally more expensive.

  The quiesce protocol is the same for both tiers and is described in §8.5: the agent freezes the container, syncs and freezes the filesystem, and only then requests the capture. Because paired scratch images are cloned rather than drawn from the pool, they must match the pool's fixed VHD size so device geometry stays invariant across restore (§8.4).

Note that because image content is read through virtio-blk into the guest page cache, a warmed snapshot will contain image bytes in its `mem.bin`. Taking snapshots at well-chosen points (before broad filesystem access) keeps them compact.

### 8.4 The agent-ready snapshot point

The platform tier is captured at a precise place in the agent's boot sequence: **after the network is programmed, but before the sandbox config is read and before any layer or scratch device is mounted.**

```
kernel boot → agent start → pseudo-fs mounts → network programming
    → ◆ SNAPSHOT ◆
    → read per-sandbox config → mount layers → mount scratch → overlay → clone child → exec
```

On restore, `nvx` resumes at the instruction after the snapshot request, so the agent simply continues from `read per-sandbox config` — reading a section the VMM has **repopulated with a different sandbox's configuration** in the interim.

Four properties make this specific point useful:

**1. The snapshot contains no secrets, and no image binding.** The agent has read only the *invariants* section (network identity, device topology — all non-secret and image-independent) before this point; the image-binding and sandbox sections are read *after*. A snapshot taken here is therefore free of tenant data, which is what makes it safe to **share across mutually-untrusting tenants**, and free of any layer identity, which is what makes one snapshot serve any image of the same topology. Any snapshot taken after those sections are read must be treated as tenant-confidential and never fanned out.

**2. Scratch has no dirty state.** The scratch device is attached but not yet mounted, so no ext4 metadata or data is in the guest page cache. The memory/scratch atomicity problem in §8.3 does not arise at all: every restore simply takes a fresh VHD from the pool. This is unique to the platform tier — the workload-start tier snapshots with scratch mounted and must carry a paired scratch image.

**3. `mem.bin` is minimal.** Guest RAM holds the kernel, the initramfs, and the agent — no image content, because no layer has been mounted. This is the smallest useful snapshot the system can produce, which makes it cheap to store and fast to restore.

**4. Network programming is already done, and is safely shared.** This would normally be the objection — network identity is per-sandbox — but §11 resolves it: the guest-visible identity (MAC, IP, MTU, routes, DNS) is deliberately held constant across restores while the external endpoint is rebound per launch. Restored sandboxes therefore share a guest-internal address and differ externally, which is exactly what the L2Bridge/AF_XDP translation is built to do. This is why network identity is an invariant rather than per-sandbox data (§4.1): the captured guest state already depends on it.

#### What a snapshot is keyed on

A snapshot is not universally restorable. Its captured guest state fixes a set of properties that any restore must match:

| Dimension | Why it is fixed |
|---|---|
| vCPU count | vCPU state is serialized per processor |
| RAM size | `mem.bin` is guest RAM in guest-physical order |
| Layer count (1–3) and carrier | Devices were probed at boot; a different count, or a switch between virtio-blk and phram (§4.3), is a different topology |
| Uniform layer capacity | Geometry must be invariant (below) |
| Network profile | Guest-visible identity is baked into guest state |
| Kernel + agent version | Both live in `mem.bin` |
| VMM build and backend | State formats are backend-specific and versioned; a KVM snapshot is not a WHP snapshot |
| CPU feature and XSAVE class | Saved XSAVE/CET state must be restorable on the target processor |
| TSC frequency class | Host TSC frequency is measured again on restore and feeds guest timekeeping |

The last three matter specifically for restoring on a *different node* than the capture: a snapshot is portable only across hosts in the same CPU and TSC class running the same VMM build. Scheduling must treat those as placement constraints.

So the platform tier is **a small matrix of snapshots**, not a single artifact: one per (vCPU, RAM, layer shape) in use per compatibility class, rebuilt whenever the kernel, agent, or VMM is rolled out. The matrix is cheap because these snapshots are small (property 3). Reducing it — by normalizing VM shapes, or by making layer count uniform with a placeholder device — is future work.

#### The configuration swap window

The principle generalizes to any snapshot point: what a restore can still change is whatever the guest has not yet consumed into its own state. **The later the capture, the less remains swappable.**

| Snapshot point | Still swappable on restore | Fan-out scope |
|---|---|---|
| After network, before image and sandbox sections are read | Layer blobs, entrypoint, args, env, secrets | Any tenant |
| After overlay assembly, before `exec` | Entrypoint, args, env, secrets | Tenant-scoped — secrets have been read |
| After `exec` (workload warmed) | Nothing in the OCI config — only work items the shim accepts at runtime | Tenant-scoped |

#### Substituting layer blobs on restore

Restoring against *different layer blobs* than were attached at capture is the one part that needs care. Device naming is not among the concerns: `/dev/vdX` names are assigned only in `virtblk_probe` (an index from a global IDA, formatted by `virtblk_name_format`), and a restore does not re-probe — the guest resumes with its `gendisk` structures already live in guest RAM. Since the VMM attaches the same device set in the same order, names come back exactly as captured.

What *is* stale is device geometry and cached blocks.

**Where the size lives.** Capacity is not part of bus enumeration. It is the first field of the virtio-blk device configuration space:

```c
struct virtio_blk_config {
    /* The capacity (in 512-byte sectors). */
    __virtio64 capacity;      /* offset 0 */
    ...
};
```

The driver reads it once at probe (`virtblk_probe` → `virtblk_update_capacity`) and derives `gendisk`/`bd_inode` state from it. On restore the VMM re-creates the device from the new blob, so **config space is already correct**; what is stale is the guest kernel's derived copy, which returns with `mem.bin`. The second piece of stale state is whatever the probe-time scan pulled into the block device's page cache.

**A uniform reported capacity makes geometry invariant.** The VMM reports the same capacity for every read-only layer device — a single generous value (e.g. 64 GiB) chosen to cover any expected layer — while each **backing file keeps its natural size**, with reads past EOF returning zeros. A device larger than the filesystem on it is an entirely ordinary arrangement: EROFS takes its extent from the superblock and never reads the tail, exactly as a filesystem smaller than its partition behaves. Capacity is therefore a property of *device presentation*, not of the stored artifact — the blob transfers and digests as its real bytes, with no sparse-file handling and no padding.

Because every layer device presents the same geometry, **one platform snapshot serves every image**: capacity never changes across a restore, so there is no config-space update, no config-change interrupt, and no re-probe. (Sizing layer devices in classes instead would work equally well but multiplies snapshots by the number of class combinations across the layer stack, for no gain.)

The fast path per device is then two synchronous, sub-millisecond steps:

```
assert  read(<sysfs>/size) == expected      /* geometry */
ioctl(fd, BLKFLSBUF)                        /* drop probe-time cached blocks */
mount
assert  EROFS superblock UUID == expected   /* identity */
```

**Expected capacity and layer UUID are carried in the config region** (§4.1), so the agent asserts rather than assumes. Both are constant-time checks against values already in memory — deliberately *not* a content hash, which would require reading the whole blob and would break both the latency budget and demand paging.

**Slow path: a layer larger than the uniform capacity.** This is the one legitimate mismatch, and it is handled rather than rejected. The agent unbinds and rebinds only the offending device:

```sh
echo virtioN > /sys/bus/virtio/drivers/virtio_blk/unbind
echo virtioN > /sys/bus/virtio/drivers/virtio_blk/bind
```

Unbind runs `virtblk_remove` → `del_gendisk`, destroying the gendisk *and* its page cache; bind re-probes and re-reads config space, picking up the true capacity. This is synchronous and clears both pieces of stale state at once, at a cost of a few milliseconds — acceptable for an outlier, which is why it is kept off the common path.

Two constraints on the slow path:

- **Rebind one device at a time.** The block-device index comes from a global IDA and is reallocated lowest-free, so rebinding a single device returns the same index and hence the same name, while rebinding several concurrently can permute them.
- **Re-resolve the block device afterwards.** The virtio bus node persists across unbind/rebind — only the `block/*` name underneath is re-derived. This is why roles are named by MMIO address rather than by `/dev` path or bus name (§4).

> **Not used:** `virtio_blk` also supports asynchronous capacity change via a config-change interrupt scheduling `virtblk_config_changed_work` — correct, and the mechanism QEMU uses for online resize, but it completes on a workqueue with no natural completion signal for the agent to wait on. The synchronous paths above avoid needing it.

Scratch VHDs are standardized to a fixed size within each pool, so the writable device raises none of these concerns.

#### What the agent must provide

- A **well-defined snapshot point** in its boot sequence, with nothing image-specific or secret consumed before it.
- **Restore detection** via the launch epoch (§4.1), since a capture leaves no observable trace in CPU or memory state.
- **Idempotent configuration application**, since the same code path runs on both cold boot and every restore.

### 8.5 Capture and restore protocol

Capture and restore raise two ordering problems:

- **Filesystem consistency at capture.** Once scratch is mounted, `mem.bin` holds dirty ext4 pages that have not reached the scratch VHD, while the host-side scratch file is at a different moment. Capturing the pair without quiescing corrupts the filesystem on restore.
- **Execution ordering at restore.** Resuming a guest makes *every* task that was runnable at capture runnable again, and re-arms device interrupts and host-facing I/O. Nothing intrinsically schedules the agent's restore work before any of it.

The first problem is solved at capture time: the agent freezes the container and quiesces the filesystem before requesting the capture, so the artifacts describe a settled instant.

The second is a restore-time problem, and it is broader than the container. Three populations of execution become live again when a snapshot resumes, and each of them can run before the agent has fixed the clock, reseeded the CRNG, and refreshed identity (§8.2):

- **Container tasks**, every one of which was runnable at capture.
- **The agent's own worker threads** — RPC server, log pump — which are captured mid-flight along with everything else.
- **Device and host-facing activity**: interrupts, network ingress, and host control input. Some of this the VMM starts before vCPU entry, so it is not ordered by anything the guest does.

Each population is held by the mechanism that owns it, and each hold is established *at capture* so it is already in force the instant execution resumes:

| Population | Held by | Released by |
|---|---|---|
| Container tasks | `cgroup.freeze`, captured in the frozen state | Agent, last |
| Agent worker threads | Parked at the capture call site, so the agent restores single-threaded | Agent, as it finishes its own work |
| Devices, network, host input | VMM restore gate | Agent, by signalling completion |

They are not substitutes for one another, because they sit at different layers. The cgroup freeze lives in the guest kernel and reaches only tasks in the container cgroup — not the agent's threads, which are in a sibling cgroup (§7.4), and not the VMM's device threads, which are outside the guest entirely. The restore gate lives in the VMM and can hold what enters the guest, but cannot order anything already scheduled inside it. And the resume point orders only one thread: `nvx` resumes at the instruction following the `0x605` write, which puts the agent's capture thread exactly where it needs to be and says nothing about any other vCPU or kernel worker.

The release order in the diagram below follows from this. The agent does its repair work while everything is still held, releases the restore gate so devices and host I/O are live, and thaws the container last — so the workload is the only thing that never observes a partially repaired sandbox. At the platform tier the first row does not apply: no container exists yet (§8.1), and the other two holds are what make that tier's restore safe.

```
              trigger  (agent itself / container request / host RPC)
                 │
   agent  ───────┼─ rendezvous: park own worker threads
                 ├─ freeze container cgroup; wait for cgroup.events: frozen 1
                 ├─ sync(); FIFREEZE scratch
                 └─ write 0x605                       ◆ CAPTURE ◆
                 │
   VMM    ───────┼─ stop vCPUs; drain block device queues; flush backing files
                 ├─ write mem.bin + state.bin into a fresh generation directory
                 ├─ clone the quiesced scratch
                 ├─ fsync artifacts, then publish the manifest atomically
                 ├─ report the result to the host
                 └─ exit  (the capturing instance ends here — §8.1)

        ─────────────────────── on restore ───────────────────────
   VMM    ───────┼─ validate the manifest digest against the launch contract
                 ├─ map mem.bin CoW; attach devices; hold the restore gate
                 └─ enter vCPU
   agent  ───────┼─ read config §1; compare invariants
                 ├─ read config §2 + §3; detect restore via launch epoch
                 ├─ set clock, reseed CRNG, refresh identity  (if clone — §8.2)
                 ├─ release the VMM restore gate
                 ├─ FITHAW; queue the reply; thaw container cgroup
                 └─ continue at boot step 7
```

**Capture is a host transaction.** The VMM drains block queues and flushes backing files before serializing, so `mem.bin`, `state.bin`, and the cloned scratch describe one instant. Artifacts are written into a **fresh generation directory** with checksums and lengths, durably flushed, and then published by atomically installing a single manifest pointer — so a partially written set is never selectable, and a crash mid-capture leaves the previous generation intact. Blob and scratch refcounts are committed in the same transaction as the manifest.

Cloning the scratch is the VMM's job because it happens after the queues are drained; doing it guest-side before the `0x605` write would clone a file the VMM may still have writes queued against.

**Freeze semantics.** `cgroup.freeze` is a cgroup v2 **core** interface file — not a controller, so it needs nothing in `cgroup.subtree_control` — and exists only on non-root cgroups, which the container's sibling cgroup (§7.4) is. Writing `1` freezes the cgroup and all descendants. Three properties matter:

- **It is invisible to the process.** Unlike `SIGSTOP`, no signal is delivered and the task cannot detect it. The workload needs no awareness or handling.
- **It is asynchronous but notified.** The kernel documents that "freezing of the cgroup may take some time"; completion sets `frozen 1` in `cgroup.events` and issues a notification, so the agent waits with `epoll` rather than polling.
- **It can stall.** A task in uninterruptible sleep keeps the cgroup in `FREEZING` until its I/O completes, and such a task does not die promptly on `SIGKILL` either. The agent therefore needs a **freeze timeout** with a defined failure path — thaw, abandon the capture, report a typed error — and teardown of a sandbox stuck this way falls back to host-side VMM termination.

The container cgroup must be **owned by the agent and not delegated**, since a workload able to write its own `cgroup.freeze` or migrate out of the cgroup could escape the barrier. Where a workload legitimately needs a delegated subtree (§10.1), it is delegated a *child* of the agent-owned cgroup, so the freeze applies hierarchically from above.

All of the above is **capture-time** cost. The restore path is unaffected.

#### Triggering a capture

| Tier | Trigger | Barrier for the workload |
|---|---|---|
| Platform | Agent, at its own boot point | No container exists |
| Workload start | Container requests it (below) | Cgroup freeze |
| Instance checkpoint | Host RPC to the agent | Cgroup freeze |

Because the agent performs every capture, one mechanism covers all three tiers, and the VMM needs no way to capture a guest that is not cooperating — except as a degraded-mode fallback for an unresponsive agent (§2.4).

#### The container-facing interface

Most workloads never touch this. For the workload-start tier the platform's warm shim (§8.1) is itself the requester: it checkpoints after the runtime has initialized and before user work begins, so the customer writes ordinary code and sees no API.

For customers who want to choose the point themselves, the agent exposes a **`SOCK_SEQPACKET` UNIX socket bind-mounted into the container** at `/dev/aci/checkpoint` — the agent already owns the container's `/dev` tmpfs, so this costs no copy-up and does not perturb the image. One datagram requests a checkpoint; one datagram comes back.

The socket is a request channel, not a barrier. The requesting thread blocks awaiting its reply, but that alone would leave the process's other threads running, so ordering rests on the three barriers above. Sequenced packets avoid partial-message framing, and per-message `SCM_CREDENTIALS` identify the actual sender rather than whichever process happened to open the socket — the descriptor may have been inherited across `fork` or passed over another socket.

The reply carries this sandbox's work item. After restore the agent has already read the refreshed configuration, so it returns the entrypoint arguments or work item for *this* sandbox. The warm shim's handoff is therefore not a separate protocol — it is this call's return value.

**The shim must reseed before its peers run.** Thawing the cgroup makes every container thread runnable at once, so nothing guarantees the requesting thread processes its reply — and resets its runtime's RNG state (§8.2) — before a background thread consumes cloned randomness or acts on stale state. A workload-start snapshot must therefore be captured **single-threaded**, or the shim must hold its peer threads behind its own barrier until it has completed its restore hooks. This is a requirement on the shim, and on any customer using the interface directly, because the agent cannot reach inside a process to enforce it.

Two policy controls:

- **Opt-in.** The socket is bind-mounted only when the sandbox's configuration enables checkpointing. Otherwise the path does not exist and neither does the capability.
- **Rate-limited.** Each capture writes a `mem.bin` to node storage, so an application looping on the call is a storage denial-of-service. The agent enforces a rate limit and the host caps total artifacts per sandbox.

> **Not exposed: direct port I/O.** Writing `0x605` from the container would require `ioperm`/`iopl` or `/dev/port` access, which grant access to *every* port rather than one. Routing through the agent keeps the port write in the component trusted to make it.

#### The replay contract

Customers choosing their own checkpoint point need a contract, and the useful one is about **replay**, not sensitivity: everything captured happens again on every restore. Live TCP connections resume dead, cached timestamps are stale, and any external side effect already occurred once. Runtime and library RNG state is also captured, and the agent cannot reach inside a process to reset it (§8.2) — so a workload that will be cloned must reseed its own RNG after each restore.

The guidance is therefore: *checkpoint at a point where you hold no live external state, nothing you have cached will be wrong when replayed, and you can reseed your own randomness on resume.* Those are properties a customer can check, unlike "nothing sensitive in flight."

---

## 9. Deferred Optimization: virtio-pmem + DAX

The largest available efficiency win is **not** taken today, deliberately.

**The opportunity.** With `virtio-pmem`, a host file is mapped into guest-physical memory rather than transferred over a block queue. If N VMs' VMMs `mmap` the *same* host file, the host coalesces those mappings onto the same physical page frames; mounting with `-o dax` makes the guest map those pages directly, allocating **no guest page cache at all**. N sandboxes sharing an image would consume ~1× the image's memory instead of ~N×, with true execute-in-place for shared binaries.

**Why we are not doing it.**

1. **Guest-writable shared memory.** The Linux `virtio-pmem` driver does not mark the region read-only from the guest's perspective. **CVE-2026-24834** (CVSS 9.4, Kata Containers) exploits exactly this: a process with `CAP_MKNOD` creates a pmem device node, computes a binary's byte offset, overwrites it, and obtains root in the guest. DAX is the enabler — it removes the hypervisor from the I/O path, so there is nowhere to reject the write. virtio-blk keeps the VMM in the path and the entire class disappears.
2. **Cross-VM cache side channels.** Shared physical pages between mutually-untrusting sandboxes reintroduce **Flush+Reload**: an attacker `clflush`es a line of the shared image and times the reload to observe the victim's execution and data-access pattern at cache-line granularity. This is precisely why KSM and VMware TPS were disabled across tenants in public clouds. Firecracker's own documentation explicitly advises against sharing a pmem backing file across VMs for this reason. Note that **LLC partitioning (Intel CAT) does not mitigate this** — CAT restricts cache *allocation*, not *lookup*, and `clflush` is unaffected; CAT addresses contention channels (Prime+Probe), which are a different family.

**What would make it viable later**, roughly in order of tractability:
- Scope sharing to a **single trust domain** (share a blob only among one customer's sandboxes) — retains most of the density win with a sound boundary.
- Host-side read-only mapping (`readonly=on` / `discard_writes=on` / `read_only: true`) plus a cgroup BPF device filter denying `pmem*` creation — closes the write path while keeping `CAP_MKNOD` available.
- Note the guest-memory cost is not zero: the guest allocates ~64 bytes of `struct page` per 4 KiB of pmem region (~1.6% of image size, per VM, whether touched or not). DAX wins on guest memory only when the touched working set exceeds ~1.6% of the image — true for typical images, but *false* for very large, sparsely-used ones, where virtio-blk is genuinely better.

**Note the interaction with §4.2:** adopting DAX would require rebuilding images *uncompressed*, forfeiting the distribution and host-cache-density benefits of compressed EROFS.

### 9.1 The same question, asked of snapshots

Rejecting DAX on side-channel grounds obliges us to be consistent: `nvx` restores `mem.bin` with `MAP_PRIVATE`/`PAGE_WRITECOPY`, so **every VM restored from one snapshot shares the physical pages it has not yet written**. That is the same precondition — shared physical pages between VMs — and it is why the tier scoping in §8.1 is a security property, not an organizational one.

A clarification worth stating, because it is easy to get wrong: **flushing the guest page cache before capture does not remove this channel.** Flush+Reload targets CPU cache lines backing shared *physical pages*; the guest page cache is a Linux construct. Anonymous memory, kernel text, and agent text are all shared after a COW restore and all LLC-cacheable regardless of what the guest page cache holds. Dropping caches shrinks the shared surface; it does not eliminate it.

**Sharing alone is not sufficient, though — the attacker also needs an address.** `clflush` operates on a virtual address, so a shared physical page is only exploitable if the attacker can map it. That precondition is what separates the tiers:

- **Platform — cross-tenant, and the channel is unlikely to arise.** The shared pages are guest kernel and agent memory. The container can map neither: it has no access to agent address space, no `/dev/mem`, and its own pages are faulted fresh and privatized by the kernel's zeroing write before it ever sees them. With no alias, there is nothing for the container to flush and time. The design still requires that **the agent have no secret-dependent control flow or data-dependent memory access** when handling the sandbox configuration section — cheap to satisfy, since that path is a `memcpy` into a struct — but this is defence in depth against an addressability gap rather than the load-bearing control it might appear to be.
- **Workload start — the alias exists, and is why tenant scoping matters.** Here the container's *own* text and data are in `mem.bin`, and a restored container maps exactly those pages. Two clones of the same snapshot therefore do share attacker-addressable memory. Confined to one tenant this is the same posture as two of that customer's containers sharing page cache on an ordinary host; across tenants it would be the DAX scenario, which is why §9.2 applies.
- **Instance checkpoint — single instance.** A resume shares nothing beyond itself.

Because the precondition rests on the container's inability to map agent and kernel memory, that property should be treated as a security invariant of the agent's namespace and device setup (§7.1) rather than an incidental consequence of it.

The cross-tenant workload-start case is resolved by giving each tenant its own copy of the snapshot — see §9.2.

### 9.2 Breaking page sharing without copying: reflink

Where sharing must be broken, the mechanism is cheap. Page cache is keyed by `(address_space, offset)` and `address_space` belongs to the **inode**, so physical-page identity follows inode identity:

| Reference | New inode? | Separate page cache? | Storage cost |
|---|---|---|---|
| Hard link | No | ❌ shared | 0 |
| Symlink | No | ❌ shared | 0 |
| **Reflink / block clone** | **Yes** | **✅ separate** | **~0 — extents shared** |
| Full copy | Yes | ✅ separate | full |

A reflink is a new inode that shares the original's **disk extents**. Two VMs restoring from reflinked copies of the same snapshot read identical bytes off identical blocks, but the pages land in different physical frames — so there is nothing to `clflush` in common. Hard links are the intuitive answer and the wrong one: the same inode means the same `address_space` and therefore exactly the sharing being avoided.

The inode/`address_space` reasoning above is Linux's. On Windows the analogous property is that two files are distinct stream objects with distinct section objects, which should likewise yield distinct pages — but ReFS block cloning guarantees allocate-on-write *on disk*, not distinct cache page frames, and `nvx` maps guest RAM with `PAGE_WRITECOPY`/`FILE_MAP_COPY` rather than a Linux mapping. **Physical-frame separation between ReFS clones is therefore an assumption to be validated on the production build, not a property to rely on unverified** (§14), and it holds only with page combining disabled (§2.4).

Because `mem.bin` is mapped copy-on-write and never written back, the extents stay shared for the artifact's whole life.

**The cost is RAM, and that is the point.** Storage is unaffected; memory is duplicated per clone, which *is* the isolation rather than a side effect of it. Three separate quantities need modelling, because they scale differently:

- **Clean resident pages — per tenant.** Faulted from that tenant's clone and shared by all of its sandboxes.
- **Dirty pages — per sandbox.** Anything a restored guest writes becomes private to it.
- **Commit charge — per sandbox.** On Windows the copy-on-write mapping is charged for the entire view regardless of how few pages are actually written, so commit tracks sandbox count even where physical pages are shared.

A warmed language-runtime snapshot is a few hundred megabytes, so a node hosting many distinct tenants pays real memory. That is a capacity-planning input, not a correctness question.

**The first restore per tenant is cold.** A freshly cloned artifact has no cached pages, so that tenant's first restore faults its working set in from storage while subsequent restores hit a warm cache. First-restore and steady-state latency should be measured and budgeted separately.

**Do not apply this to the platform tier.** Its sharing is safe (§9.1) and fleet-wide sharing of one copy is the entire value; cloning it would multiply memory for no benefit.

**Not an alternative: pre-faulting.** Force-populating a `MAP_PRIVATE` mapping also privatizes every page, but pays the full copy *in RAM, on the restore path* — the exact latency the design protects. Reflink defers the copy to natural page faults and never pays it for pages nobody touches.

Implementation is a control-plane concern: `nvx` opens whichever path it is given, so nothing in the VMM changes. It does impose a **storage requirement**, and on Windows a **page-combining-disabled requirement**, since page combining would re-share the physically identical pages this separation creates (§2.4).

---

## 10. Known Limitations

### 10.1 Images that run systemd as their entrypoint

A minority of images (RHEL/CentOS UBI `-init`, some test and lift-and-shift images) ship `/sbin/init` as their entrypoint. These are *not* broken by the absence of an init system in the guest — the guest's PID 1 is invisible to the container — but they require the agent to construct a system-shaped environment: `container=` environment variable, **read-only `/sys`** (systemd's test for whether device management is available), read-only `/proc/sys`, a private `/dev` tmpfs with the standard device nodes, `MS_SHARED` mount propagation, a writable delegated cgroup subtree, and `SIGRTMIN+3` as the stop signal.

There is a direct conflict here with §9's mitigations: systemd's container interface explicitly says **not** to drop `CAP_MKNOD` (it is needed for `PrivateDevices=`). The resolution is the cgroup BPF device filter rather than capability removal — which is also systemd's own recommendation.

These images additionally carry a boot-time cost (unit generation, dbus, journald) incompatible with the sub-100ms target, and should be treated as an explicitly slower profile. This limitation is inherent to the container ecosystem, not to this architecture — Docker and Kata face it identically.

### 10.2 Volumes and secrets — to be designed

Azure Files, `emptyDir`, secret, and gitRepo volumes are **not addressed by this design**. They are optional, added only when a workload requires them, and remain an open design item. Sketch of the likely shapes:

- **Azure Files** — guest-side CIFS mount over the sandbox's own network, avoiding any host-side filesystem transport.
- **Secrets / small dynamic content** — a small per-sandbox generated blob attached as an additional block device. This is the case block devices handle worst (heavyweight machinery for kilobytes of data) and deserves explicit attention.
- **`emptyDir`** — a directory on scratch.

### 10.3 Other limitations

- **Whole-file copy-up.** Large files modified in place incur a full copy into scratch (§5.1).
- **TCP sessions do not survive snapshot restore** when the external network endpoint is rebound; applications must reconnect.
- **Scratch exhaustion** surfaces as `ENOSPC` to the workload; the agent reports it as a typed error but cannot grow the device online.
- **Images with no curated ancestor get no sharing.** They fall back to a single custom layer over an empty base (§3.1), so they miss layer reuse, host page-cache sharing, and the platform snapshot tier.
- **Guest-visible network identity is fixed per snapshot profile** (§4.1, §8.4), so sandboxes restored from one snapshot share an internal address and differ only externally.
- **Platform snapshots are a matrix, not one artifact** — one per (vCPU, RAM, layer count) shape, rebuilt on kernel or agent rollout (§8.4).

---

## 11. Networking

Networking is unchanged in structure from the existing `nvx` external-networking path and is summarized here for completeness because it interacts with snapshots.

```
   guest                  VMM                        host
┌──────────┐        ┌────────────┐          ┌──────────────────────┐
│virtio-net│◄──────►│ AF_XDP     │◄────────►│ HCN L2Bridge         │
│  (guest  │ virtio │ queue bind │  XDP     │ host vNIC / endpoint │
│ identity)│  mmio  │ + translate│          │ (external identity)  │
└──────────┘        └────────────┘          └──────────────────────┘
```

- The guest sees a single **virtio-net** device on virtio-mmio, statically configured from the network identity in the config region's invariants section (address, mask, prefix, MAC, MTU, routes, DNS, search domains). No DHCP, therefore no round trip on the start path.
- The host data plane is an externally provisioned **HCN L2Bridge**; `nvx` binds **AF_XDP** queues to it and translates between the two identities. `nvx` does not create or destroy the HCN network, endpoint, or policy.
- A **readiness handshake** gates VM start: `nvx` initializes its queues and sends `DataPlaneReady`; the host replies `StartVm` only when the external network is ready. `nvx` does not enter the vCPU loop before that, which guarantees the container never observes a half-configured network.

**Why this matters for snapshots.** The contract deliberately separates a **stable `guestIdentity`** (MAC, MTU, IPv4, routes, DNS — serialized with the virtio-net snapshot and required to match on restore) from a **per-launch `externalEndpoint`** (HCN endpoint, interface, queues — free to differ on every restore). `nvx` translates ARP and IPv4 between them and rejects guest-spoofed frames.

This is what makes **one snapshot restorable many times onto different external IPs**: the guest's view of its own network never changes (so its in-memory network state stays valid), while the externally routable identity is rebound per restore. Combined with the `ProgramNetwork` / `PostRestore` agent hooks, a restored sandbox can also have guest-visible addressing updated where required. Existing TCP sessions do not survive rebinding.

---

## 12. Alternatives Considered

| Option | Why not |
|---|---|
| **Nydus / RAFS v6** | RAFS v6 *is* the EROFS format plus chunked content-addressed blobs served by a `nydusd` daemon. Its headline benefit — lazy pull — is obtained differently here, by staging complete blobs ahead of time (§3); and its bootstrap+blobs split assumes a fetcher between guest and data, which does not map onto a plain virtio-blk device. Adopting it would add a per-VM daemon, in the guest or host-side. Its P2P distribution layer *is* worth taking, independently of the format (§13.1). |
| **virtio-fs + virtiofsd** (Kata's default) | Requires a host daemon per VM and adds FUSE round trips on every metadata operation. It exists to solve dynamic multi-container injection into a live guest, which a single-container sandbox never does. |
| **virtio-pmem + DAX** | Best memory efficiency by a wide margin, deferred on security grounds (CVE-2026-24834 write path; cross-VM Flush+Reload). Revisitable with trust-domain-scoped sharing. See §9. |
| **phram as the general mechanism** | Cost scales with total image size and is charged up front as guest-physical memory. Untenable for multi-GB images. Retained for small images only (§4.3). |
| **9p / virtio-9p** | Strictly worse than virtio-fs: per-operation protocol round trips, weak POSIX semantics. Legacy. |
| **tmpfs scratch** | Consumes guest RAM (competing with the customer's allocation), capacity-bound by VM memory, converts a clean `ENOSPC` into a guest OOM. |
| **dm-snapshot / dm-thin over a shared read-only device** | Block-layer CoW forces all reads through the guest page cache and cannot preserve any cross-VM sharing; adds device-mapper complexity for no benefit over overlayfs here. |
| **Single disk with one partition per layer** | Makes the *disk* the unit of caching, so every `(distro, runtime, custom)` combination is a distinct blob and the widely-shared distro and runtime content is duplicated across all of them — in the node cache, the artifact store, and the host page cache. Also requires partition-table parsing in the agent and a layout message in the host/guest contract to describe a shape the platform already fixes. Justified only for arbitrary N-layer images discovered at runtime. |
| **Pre-merging the layers into one blob** | The same duplication problem in its most extreme form; and overlayfs is required for the writable layer regardless, so the extra `lowerdir` entries are free. |
| **SquashFS layers** | Compressed like EROFS but with weaker xattr support (needed for overlayfs whiteouts and security labels), no file-backed mount, and no DAX path should one ever be wanted. |
| **Uncompressed EROFS** | Only justified if DAX is adopted; forfeits distribution and host-cache-density benefits today (§4.2). |
| **systemd (or any init) in the guest** | Nothing in the container contract requires it. Its jobs (pseudo-filesystem mounts, orphan reaping, log forwarding, time sync via `kvm-clock`) are a small, well-bounded set the agent absorbs. Kata's initrd mode already ships agent-as-PID-1 in production. |
| **vsock or virtio-serial for the control channel** | Would require implementing a new device in `nvx`. The console stdio channel already exists, is owned end-to-end by the VMM, and is not guest-addressable — better isolation properties for free. |
| **Configuration on the kernel command line** | Exceeds `COMMAND_LINE_SIZE`; `/proc/cmdline` is readable by the container, leaking secure environment variables; and the command line cannot be rewritten on restore, so nothing carried there can vary between the sandboxes a snapshot is fanned out to. |
| **Image config baked into a layer blob** | Would make the agent read configuration out of a mounted filesystem before it could act, require masking that path from the container's view, and split configuration across two sources that the control plane must then keep consistent. The control plane already holds both the image defaults and the per-sandbox overrides, so resolving them there and shipping one structure is strictly less work everywhere. |
| **Sandbox config written into the scratch blob** | Requires the host to attach/mount the VHD or ship an ext4 writer for Windows, turns a cheap pool hand-out into a per-sandbox write, and makes pooled VHDs non-interchangeable. It also bakes config into a blob that a fanned-out snapshot cannot refresh. A VMM-populated memory region achieves the same with no I/O (§4.1). |
| **Sandbox config in the initramfs** | The VMM could append a per-sandbox cpio segment, but initramfs content lives in guest RAM and is therefore captured in the memory snapshot — config would go stale the moment a snapshot is reused for a different sandbox. |
| **Host-side overlay assembly (hcsshim `CombineLayers` model)** | Requires the host to understand and construct the guest's filesystem view, and a host-side transport to project it. The agent can do it with local mounts and no round trips. |
| **Full content-hash verification of layers in the guest** | Hashing a multi-gigabyte blob means reading all of it, which breaks the restore latency budget and defeats demand paging. Verification belongs at node-cache admission; the guest checks a constant-time identity (EROFS superblock UUID) instead (§4.1). |
| **Cross-tenant warm-workload snapshots** | Would share interpreter, libc, and crypto-library pages across mutually-untrusting tenants — the precondition this design rejects for DAX (§9.1). Deferred; a per-tenant instantiation of a platform-authored warm snapshot is the nearer-term compromise. |
| **`exec`ing the entrypoint from the agent process** | `exec` replaces the process image, destroying the agent along with supervision, log streaming, and the control channel. The agent clones a child instead and stays PID 1 (§6). |

---

## 13. Future Work: Distribution and Lazy Loading

Two extensions are explicitly enabled by this design but deliberately not built now. Both address the same pressure — very large images (multi-GB Conda/ML environments) where staging a complete blob to every candidate node is expensive in storage and in first-run latency — and both can be added later without touching the guest.

### 13.1 P2P blob distribution

Blobs are immutable, content-addressed files that arrive in the node cache before a sandbox starts; nothing about how they get there is visible to the VMM, the agent, or the guest. Replacing direct artifact-store pulls with a peer-to-peer fabric (Dragonfly, or equivalent) is therefore a node-side change with a zero-impact blast radius. The layer split helps further: `distro` and `runtime` are byte-identical across large populations of sandboxes, making them ideal high-seed-count P2P artifacts.

### 13.2 Demand-fetched blob backend (lazy loading)

The guest sees only a virtio-blk device and has no knowledge of where the bytes behind it come from. The VMM can therefore be extended to serve reads from a partially-populated local cache and demand-fetch missing ranges from the artifact store in the background, leaving the guest contract, the agent, and the filesystem assembly completely unchanged. This is the shape AWS Lambda uses to hold sub-100 ms cold starts on 10 GB images (512 KiB content-addressed blocks, fetcher in the VMM, multi-tier cache), and keeping the work host-side is what preserves the single-binary agent and daemon-free guest (§12).

**Trigger.** These should be driven by measurement, not anticipation: adopt when node cache-miss rate or the p99 first-run latency for large images crosses an agreed threshold. Note that RAFS v6 is itself built on the EROFS format, so if lazy loading is ever wanted at the *format* level rather than the block level, that is an evolution of the current artifacts rather than a rewrite.

---

## 14. Open Items

1. **Volumes and secrets** (§10.2) — mechanism undesigned.
2. **Agent resource reserve sizing** (§7.4) — how much memory/CPU to withhold from the customer's allocation; needs measurement.
3. **phram/virtio-blk threshold** (§4.3) — the image size at which to switch.
4. **Snapshot artifact lifecycle** (§8.3, §8.5) — how paired scratch images are stored, refcounted, and garbage-collected alongside their snapshots, and the freeze timeout and failure policy when `cgroup.events` never reports `frozen 1`.
5. **Uniform layer capacity** (§8.4) — the value to report, chosen to cover expected layers with margin so the unbind/rebind slow path stays an outlier.
6. **Guest kernel and agent versioning** (§3.2, §2.4) — node rollout cadence, host↔agent RPC skew tolerance during a staged rollout, and how snapshots captured with an older kernel, agent, or VMM build are invalidated or rebuilt.
7. **Metrics and billing accounting** — `GetMetrics` semantics and their mapping to vCPU-second / GB-second billing.
8. **Control and log transport** (§7.2) — the `portb` console is one VM exit per transmitted byte with polled receive and no IRQ, which is adequate for boot diagnostics but poor for streaming container logs. Kernel `printk` can also interleave into framed output. Decide between a purpose-built transport (virtio-serial, vsock, or a shared-memory ring) and an explicit accepted throughput limit, and specify how the transport is quiesced across snapshot so partial frames are not captured.
9. **virtio-blk device state in `state.bin`** (§2.4) — negotiated features, queue addresses, ring indices, and in-flight I/O must serialize and restore, alongside the queue drain and flush the capture transaction depends on (§8.5).
10. **Snapshot artifact distribution and GC** (§3.2) — the workload-start and instance-checkpoint tiers add `mem.bin`, `state.bin`, and paired scratch images. Unlike layer blobs these are tenant-specific and cannot be fleet-cached, so they need their own placement, retention, and GC story, and they constrain where a sandbox can be scheduled.
11. **Warm shim per runtime** (§8.1, §8.5) — where a Python, Node, or Java shim places its checkpoint, how it resets its runtime's RNG state on restore (§8.2), and what happens for a runtime the platform does not ship a shim for.
12. **Per-tenant snapshot memory budget** (§9.2) — how much node memory per-tenant duplication consumes at realistic tenant counts, measured separately for clean resident pages, dirty pages, and commit charge, and how first-restore latency compares with steady state.
13. **Agent side-channel hardening** (§9.1) — establishing the agent's no-secret-dependent-control-flow rule, and confirming as a tested invariant that the container can map neither agent nor kernel memory.
14. **ReFS physical-frame separation** (§9.2) — validating on the production Windows build that two ReFS block clones yield distinct page frames under `PAGE_WRITECOPY`, with page combining disabled. The per-tenant isolation argument depends on it.
15. **Snapshot portability class** (§8.4) — defining the CPU feature, XSAVE, and TSC classes a snapshot may be restored across, and expressing them as placement constraints in scheduling.
16. **Layer metadata allow-list** (§5) — the exact permitted set and the policy for `security.capability`, SELinux labels, and ACLs, with test coverage for images carrying crafted attributes.
17. **Capture failure handling** (§8.1, §8.5) — since capture ends the instance, the host-side reporting path and the retry or abandonment policy for a failed platform or workload-start build.
