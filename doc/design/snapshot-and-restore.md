# Snapshot and restore

[Design index](../design.md)

## Tier contract

Sandbox snapshots encode the tier, restore policy, and a consumed
configuration-section bitmask. The validator accepts only these combinations:

| Tier | Restore policy | Scratch | Consumed sections | Intended author and sharing scope |
| --- | --- | --- | --- | --- |
| `platform` | `clone` | Fresh | Invariants only | Trusted platform build; cross-tenant only with the [pre-image-binding guarantees](sandbox-filesystem-and-agent-architecture.md#production-agent-and-launch-sequence-proposed) |
| `workload-start` | `clone` | Paired | Invariants, image binding, sandbox | Warm workload or runtime shim; tenant-scoped |
| `instance-checkpoint` | `resume` | Paired | Invariants, image binding, sandbox | One live instance; one claimed continuation |

Tier metadata requires sandbox blocks; blockless snapshots do not declare it.
The saved host-owned `nvx_snapshot_tier=` token must agree with the manifest.
Platform lower layers have empty, unbound identities, whereas later tiers bind
consumed layers to their hashes. The bitmask establishes when configuration
is considered consumed; it is not an implementation of replaceable payloads
or per-section configuration-region validation.

The platform point removes kernel and agent initialization from subsequent
launches. Workload-start removes runtime initialization as well, but contains
workload memory and often cached image data and secrets. Read-only layer files
remain external attachments; bytes read from them into guest RAM can still
appear in `memory.bin`. Paired scratch is an artifact in the snapshot directory,
not just a reference to the caller's original writable file.

Sharing scope is a deployment requirement, not a tenant authorization feature
of the VMM. The snapshot controller enforces tier combinations, layer binding,
scratch pairing, and the single-use resume claim. Host artifact access control,
template provenance, and a trustworthy pre-capture guest determine whether
cross-tenant publication is permissible.

## Capture boundary

Generic host save and pulse-save/restore RPCs are deliberately unavailable for
the microVM profile. Capture is requested by the guest through PMIO `0x605` and
is coordinated as a bounded transaction. The destination is configured on the
command line or through the management RPC; it must not exist, and OpenVMM
creates file-backed RAM beside it when no memory backing file was supplied.
Capture with sandbox blocks requires
`--snapshot-tier platform|workload-start|instance-checkpoint`; inconsistent
tier, clone/resume, and fresh/paired scratch combinations are rejected. For
paired scratch, the guest snapshot helper first freezes the workload cgroup
with a bounded wait, calls `sync`, and freezes the mounted filesystem. Its
fresh-scratch mode instead requires scratch to be unmounted. A rejected capture
thaws every guest-owned barrier and then releases the stall-detector
suppression ([snapshot agent](time-abi.md#snapshot-agent)); failure to thaw or
release terminates the VM.

1. gate host input and defer completion of the snapshot-port write;
2. stop the vCPU at the I/O boundary while completing the write, so saved state
   starts at the instruction immediately after `out`;
3. preflight the profile, destination, backend, external attachments, and
   snapshot eligibility; a missing destination or rejected preflight releases
   the boundary and lets the guest continue;
4. quiesce the remaining device workers and VM time in dependency order,
   including bounded virtio-blk queue drain;
5. save the exact state-unit inventory, processor state, device-private state,
   the [time contract and CPU profile record](time-abi.md#snapshot-manifest),
   and memory;
6. copy and flush state, memory, and any paired scratch into a unique sibling
   staging directory;
7. publish the directory with one no-replace rename; and
8. terminate the source worker and process after publication.

```mermaid
%%{init: {"theme": "base", "themeVariables": {"background": "#ffffff"}}}%%
sequenceDiagram
   box rgb(255, 255, 255)
   participant Guest
   participant Port as PMIO 0x605
   participant Worker as VM worker
   participant Units as State units
   participant Controller
   participant Storage as Snapshot storage
   end

   rect rgb(255, 255, 255)
   opt Paired mounted scratch
      Guest->>Guest: Freeze workload, sync, and freeze scratch
   end
   Guest->>Port: OUT snapshot request and scratch policy
   Port->>Worker: Bounded asynchronous notification
   Worker->>Units: Gate host input
   Worker->>Worker: Stop vCPU at I/O boundary
   Worker-->>Port: Complete OUT
   Worker-->>Controller: Boundary ready
   Controller->>Controller: Preflight destination and contract
   alt No destination or rejected preflight
      Controller->>Worker: Release boundary
      Worker->>Units: Resume host input
      Worker-->>Guest: Continue execution
   else Capture accepted
      Controller->>Worker: Quiesce for snapshot
      Worker->>Units: Stop and save in dependency order
      Units-->>Controller: State, inventory, time contract, and CPU profile
      Controller->>Storage: Copy state, memory, and optional scratch
      Controller->>Storage: Publish with no-replace rename
      Controller->>Worker: Terminate committed source
   end
   end
```

State-unit transitions follow registered dependencies rather than one global
serial list. In the current worker, the partition depends on the device
prerequisite unit and VM time. Save quiesce stops in reverse dependency order;
restore and rollback start in dependency order. Device prerequisites and VM
time may transition in parallel.

```mermaid
%%{init: {"theme": "base", "themeVariables": {"background": "#ffffff"}}}%%
flowchart LR
   subgraph Capture["Capture and save"]
      direction LR
      Gate["Gate host input"]
      Boundary["Stop vCPU at<br/>PMIO boundary"]
      StopPartition["Stop partition<br/>state unit"]
      StopDevices["Stop device<br/>prerequisite units"]
      StopTime["Stop VM time"]
      Save["Save stopped<br/>state inventory"]

      Gate --> Boundary --> StopPartition
      StopPartition --> StopDevices --> Save
      StopPartition --> StopTime --> Save
   end

   subgraph Restart["Restore or rollback start"]
      direction LR
      RestoreState["Restore stopped state"]
      StartDevices["Start device<br/>prerequisite units"]
      StartTime["Start VM time"]
      StartPartition["Start partition<br/>and vCPU"]
      ResumeInput["Resume host input"]

      RestoreState --> StartDevices --> StartPartition
      RestoreState --> StartTime --> StartPartition
      StartPartition --> ResumeInput
   end

   Save -. committed snapshot .-> RestoreState
```

The directory rename is the commit point. Before it, capture failure exposes no
final snapshot and resumes only when rollback is known to be valid. After it,
the source never resumes. This gives the guest an observable boundary: code
after the snapshot `out` runs once in each restored process and never in the
successfully captured source process. A capture driven through the management
RPC likewise halts the source at the committed boundary and terminates its
process, so clients observe the transport closing.

Establishing the boundary is itself fallible. If host input cannot be gated,
the worker rolls the gate back and releases the write so the guest continues;
if that rollback is uncertain, the worker terminates. Once vCPU stopping has
begun, a boundary failure is terminal: host input stays gated and the VM is
torn down rather than resumed. The post-restore acknowledgment boundary
follows the same rule. Teardown still depends on the affected backends
responding; there is no bounded shutdown deadline.

While the worker holds the snapshot stop guard, its central RPC dispatcher
rejects management operations that could disturb the boundary, including
memory writes, resume/reset, device changes, and state dumps. Rejections are
immediate, not queued until capture finishes; infallible RPC forms report a
channel failure. Snapshot quiesce, rollback, boundary release, and memory reads
remain available. The exclusion applies at the worker boundary shared by every
management frontend.

Guest and host barriers have separate jobs. The workload cgroup is frozen in
captured guest state, the VMM gates external input, and a future multithreaded
agent must park its own RPC/log workers at a capture-safe point. None of these
substitutes for the others. Device workers and guest kernel execution needed
for CPU/RAM repair must run during gated restore; the gate is not a promise
that all interrupts remain disabled. After repair, the helper acknowledges the
VMM gate before thawing scratch and then the workload. The current helper
polls cgroup freeze completion with a bounded loop; an event-driven production
agent and typed capture-failure reporting remain future work.

## Artifact format and publication

A published snapshot contains:

```text
snapshot/
|-- manifest.bin
|-- state.bin
|-- memory.bin
`-- scratch.img    # paired microVM scratch only
```

```mermaid
%%{init: {"theme": "base", "themeVariables": {"background": "#ffffff"}}}%%
flowchart LR
   State["Stopped VM state"]
   Ram["Exact captured base-RAM<br/>file handle"]
   Scratch["Optional exact scratch<br/>file handle"]

   subgraph Staging["Unique private sibling staging directory"]
      direction TB
      StateFile["state.bin<br/>write and flush"]
      MemoryFile["memory.bin<br/>exact captured base RAM<br/>from automatic or supplied backing"]
      ScratchFile["scratch.img<br/>copy, verify, and flush"]
      ManifestFile["manifest.bin<br/>record contract and lengths<br/>write and flush before automatic RAM link"]
   end

   SyncStaging["Flush staging directory"]
   Rename["No-replace same-parent rename<br/>publication commit point"]
   Final["Final snapshot directory"]
   SyncParent["Flush parent directory"]
   Cleanup["Remove owned staging directory"]

   State --> StateFile --> ManifestFile
   Ram --> MemoryFile --> ManifestFile
   Scratch -. paired policy .-> ScratchFile --> ManifestFile
   ManifestFile --> SyncStaging --> Rename --> Final --> SyncParent
   Staging -. failure before commit .-> Cleanup
```

The snapshot publisher implements bounded decoding, exact-length publication,
artifact length checks, restrictive creation, flushing, unique staging paths,
and same-parent no-replace publication. Automatic OpenVMM-owned microVM base
RAM is flushed and then hard-linked into the staging directory from the exact
open handle once its shared mappings, state, and manifest are durable. The
link is reopened relative to the staging directory and must match the source's
file identity (device and inode on Linux, volume and file ID on Windows) and
length. Filesystems that cannot link the handle fall back to an independent
copy, and user-supplied backing always takes that copy path. On Linux the copy
clones extents when the filesystem supports reflinks and otherwise copies only
allocated ranges or nonzero data; on Windows it is a dense copy. Automatically
created RAM backing stays sparse on Linux and is dense on Windows, where sparse
files make copy-on-write faults slow. A pre-existing final destination is never
deleted or replaced.

The source VM is terminal after an automatic RAM link commits. A failure after
creating the staging alias is rollback-safe only after the complete private
staging directory has been synchronously removed and its parent flushed. If
that proof fails, OpenVMM terminates the source rather than resuming it with a
surviving writable alias to the artifact.

The manifest is authoritative for:

- profile and ABI version;
- source hypervisor and architecture;
- captured RAM ranges and file offsets, plus any immutable RAM capacity,
  128-MiB block size, and canonical expansion ranges;
- processor topology, vCPU count, and every APIC identity;
- effective command line and digest;
- exact device order, stable IDs, state-unit names, MMIO/PMIO ranges, IRQs,
  transport, feature masks, and queue limits;
- the time contract (the declared TSC and LAPIC rates, VP 0's TSC paired with
  a host time sample, the host identities, and the generation counter) and the
  CPU profile record (the pinned profile and the effective CPUID);
- required host attachments and their policies;
- block roles, access, geometry, layer identities, and scratch policy;
- snapshot tier, clone/resume policy, and consumed configuration sections;
- exact lengths of `state.bin` and `memory.bin`; and
- the exact length and SHA-256 of paired `scratch.img`.

Snapshot paths and repeated fields are bounded. Restore rejects truncated,
oversized, malformed, wrong-type, path-escaping, symlinked, incompatible,
missing, extra, or reordered state before guest execution. Every snapshot uses
manifest version 6; any other version or format magic is rejected with
`E_SNAPSHOT_VERSION`, which asks for a recapture. The manifest does not store
or validate embedded checksums for `state.bin` or `memory.bin`; a same-length
change to either payload is therefore outside the validation contract. Paired
scratch is checked because it must match captured filesystem state. Snapshot
directories rely on host access control, while
authenticated export or transport belongs outside the default local artifact
format.

Restore opens the snapshot directory once and resolves manifest, state, memory,
paired scratch, and `resume.claim` relative to that directory handle. Windows
opens restore artifacts with read sharing only, rejects reparse points, checks
the file identity and end of file around copy-on-write section creation, and
retains the directory and artifact guards in the VM worker so writes,
truncation, deletion, and rename remain blocked until teardown. Linux retains
the exact directory and regular-file descriptors, opened without following
symbolic links, detects observable metadata changes before worker handoff, and
is therefore immune to pathname replacement; mandatory content immutability
still depends on host access control or an explicit stronger mode such as a
lease, fs-verity, or a verified artifact broker.

## Authoritative restore

Restore proceeds in the opposite direction from capture:

1. open one exact snapshot generation, read its bounded manifest, and derive
   the authoritative machine configuration;
2. resolve console, network, filesystem, policy, and read-only layer
   attachments by stable ID or role;
3. validate state, captured memory, the selected RAM target, block geometry and
   identities, and any paired scratch artifact, and claim a resume-policy
   snapshot, before worker construction;
4. create a writable private copy-on-write mapping from the exact opened
   `memory.bin` handle, add fresh private backing for selected expansion
   ranges, transfer the artifact generation guards to the worker, and use
   either a verified private scratch copy or a caller-supplied fresh scratch
   file;
5. construct the partition and exact device inventory with the selected active
   RAM and the snapshot's immutable capacity;
6. run the [time ABI preflight](time-abi.md#restore-algorithm) (backend, CPU
   profile and generation, rates, and downtime), compare the destination
   topology, device, and queue contract with the saved contract, and check the
   partition's effective CPUID against the snapshot's record;
7. restore VM time, chipset and virtio state, partition state, and vCPU state,
   then set every VP's TSC at one host instant with read-back, advance the
   LAPIC timers, VM time, RTC, and PIT by the downtime, and seal the restore
   packet's time fields;
8. finish reconnecting host resources;
9. start the state units, with external input gated first when tier policy,
   processor activation, or nonempty RAM expansion requires guest repair, and
   publish the optional readiness event;
10. release the restored vCPUs; and
11. for a gated restore, on the agent's `0x605` acknowledgment, stop at the
    exact post-write boundary, release input, and only then let the guest
    continue.

Listener attachments preserve their stable attachment ID, attachment kind,
backend kind, reconnect policy, required flag, length, and timeout. Restore
callers may rebind an eligible listener to a fresh same-kind endpoint. When an
ordinary `recreate-listener` replacement is omitted, OpenVMM reconstructs the
captured pathname; an authenticated control listener still requires an
explicitly approved restore-time attachment with a fresh capability.
Reusable-clone orchestrators must supply fresh private boot-console and
authenticated control-listener paths so independent or concurrent restores do
not collide with the terminated source generation.

An orchestrator may supply a single-use readiness endpoint: an existing Unix
socket on Linux or named pipe on Windows. OpenVMM connects to it before the
worker launches. When the restored VM first starts, the worker writes one
fixed readiness record after every state unit has started and before it
releases a restored vCPU; for a gated restore, host input is still gated at
that point, and guest repair and its acknowledgment follow the event. A
connection, write, or flush failure aborts startup, and the endpoint is never
saved in a snapshot.

The management RPC performs the same authoritative restore for snapshots
without sandbox blocks, a NIC, or a control console. Its request may repeat
only the microVM profile, an exact processor-count assertion, the portb
endpoint, the saved console and filesystem attachments, and guest power
actions. The restored VM stays paused until the client resumes it; that resume
publishes the readiness event, and a publication failure fails the resume and
tears the VM down.

The partition must exist before OpenVMM can read the CPUID it presents and
check it against the effective CPUID. This does not expose a partially
restored guest: these checks and all saved-state validation still complete
before a vCPU runs.

```mermaid
%%{init: {"theme": "base", "themeVariables": {"background": "#ffffff"}}}%%
flowchart LR
   Manifest["Bounded manifest"]
   Config["Authoritative machine config<br/>and host attachments"]
   Verify["Validate state, base RAM, target, and blocks<br/>types, geometry, and identities"]
   Cow["Private base-RAM mapping,<br/>fresh expansion, and scratch"]
   Machine["Construct partition<br/>and exact devices"]
   Validate["Validate destination CPU<br/>and saved device state"]
   Restore["Restore time, devices,<br/>partition, and vCPU"]
   Run["Start vCPU"]

   Manifest --> Config --> Verify --> Cow --> Machine --> Validate --> Restore --> Run
```

The captured memory artifact is mapped private and copy-on-write across
restores, while selected expansion ranges receive fresh private backing.
Paired scratch is copied into a private temporary file for each restore.
Multiple restored VMs may therefore dirty RAM and scratch without changing
reusable clone artifacts. On WHP, a restore whose RAM is entirely the private
mapping of `memory.bin`, with no expansion ranges, prefetch, or VPCI devices,
registers only the first 64 MiB of each mapping with the hypervisor up front.
Later guest accesses register the remainder in 2-MiB chunks, and WHP resolves
the copy-on-write faults itself. Other restores register all guest RAM before
a vCPU runs. An instance checkpoint is single-use: the first
artifact- and configuration-validated restore attempt atomically creates and
flushes `resume.claim`; duplicate and concurrent restores fail before worker
construction, and a later startup failure does not make the checkpoint
reusable. Initial compatibility is same-backend: KVM snapshots restore on
compatible KVM hosts, MSHV on compatible MSHV hosts, and WHP on compatible WHP
hosts, of the CPU generation the snapshot's profile names. Cross-backend
conversion and standalone NVX snapshot import are not supported.

## Template compatibility and placement

A template represents a compatibility class, not an arbitrary fleet image.
The class includes the backend, CPU profile and generation, the
[effective CPUID](time-abi.md#cpu-profiles) that the OpenVMM build computes
for that profile and topology, guest kernel and agent build, effective command
line, device roles and geometry, network identity/policy, and consumed layer
identities. Deployment must rebuild or recertify templates on relevant
rollouts: an OpenVMM build that computes a different effective CPUID, for
example a different cache topology, rejects earlier templates with
`E_CPU_SURFACE`. The VMM's actual compatibility and saved-state checks, not a
promise about every build with the same version string, remain authoritative.

Processor capacity and captured RAM geometry are immutable, but the opt-in
activation contracts below allow one template to serve several online CPU
counts and RAM targets. The template matrix is therefore keyed by capacities,
base state, and supported activation ranges, not necessarily by one artifact
for every final CPU/RAM size. Layer-role presence and device geometry remain
fixed regardless of these activations.

For a platform template, substituting a layer requires the same recorded
logical length and block sizes. The current contract does not promise a
uniform virtual capacity over arbitrary shorter backing files. Builders may
standardize actual raw-file geometry or maintain separate geometry classes;
larger layers need a compatible template. Guest unbind/rebind cannot bypass
the host's pre-entry geometry checks. The bootstrap's buffer invalidation and
UUID check address stale cached bytes and attachment mix-ups, not permission
to alter the saved machine topology.

## Restore-time processor activation

The microVM separates the immutable processor capacity recorded by a snapshot from
the process-local VP set needed by one restore:

- `C` is the manifest VP capacity. Processor topology, APIC identities, ACPI
   and MP tables, and saved VP inventory always contain exactly `C` entries.
- `B` is the boot-online count recorded from an opt-in template's effective
   `maxcpus=` command-line token. It must be one of 1, 2, 4, or 8 and no larger
   than `C`. A snapshot without this opt-in records zero and rejects activation.
- `N` is an explicit `--restore-processors` target. It must be supported and
   satisfy `B <= N <= C`; it is process-local and does not modify the snapshot.

The requested online set is always the contiguous prefix `0..N-1`. This is not
a topology change or a general CPU-hotplug interface: the guest still sees the
capacity-`C` machine contract, and bringing a suffix VP online after readiness
is unsupported.

Runtime VP materialization is backend-specific while the guest-visible
contract remains backend-independent:

| Restore shape | Process-local VP runners and backend binders | Saved VP state |
| --- | --- | --- |
| MSHV with explicit `N` | Instantiate and bind only `0..N-1` | Validate all `C` entries, then apply the prefix |
| MSHV without explicit `N` | Instantiate and bind all `C` | Validate and apply all `C` entries |
| KVM or WHP | Instantiate and bind all `C` | Validate and apply all `C` entries |

MSHV creates application processors lazily while binding their VP runners, so
discarding suffix binders before that boundary avoids creating processors that
this restore will not run. The synchronized TSC set on MSHV therefore covers
only the created VPs; the hypervisor rejects register access to a VP that was
never created. KVM and WHP deliberately retain their existing full-capacity
construction path, including when an explicit online target is sent to the
guest.

Saved state remains authoritative despite the narrower MSHV runtime. Before
filtering, restore requires exactly one VP state entry for every index in
`0..C-1` and rejects missing, duplicate, or out-of-range entries, including
errors in the dormant suffix. Only after that validation may an explicit MSHV
restore apply state for `0..N-1`. A reduced-prefix process cannot later produce
a complete capacity-`C` VP inventory, so any attempt to save it fails as
unsupported; dump or debug access to an uninstantiated suffix VP
also fails explicitly instead of indexing nonexistent runtime state. The
original immutable snapshot remains reusable for independent restores at
other valid targets.

After host-side state restoration, the
[restore packet](time-abi.md#restore-packet) carries `N` to the Alpine agent as
its online VP count, and portb status bit 2 advertises it; a RAM target travels
in the same packet. While external input remains gated, the agent onlines CPUs
from `B` through `N-1`, verifies that the kernel's online-CPU set is exactly the
requested prefix, and acknowledges through PMIO `0x605`. OpenVMM stops at that
post-write boundary, releases host input, and then resumes the guest.

A fixed-capacity `N`-vCPU snapshot and a capacity-`C`, boot-online-`B` template
restored to `N` therefore reach the same online prefix but do not contain the
same guest state. The fixed snapshot captures all `N` CPUs online; the template
captures only `B` online CPUs and performs CPU activation during gated repair.
Performance comparisons should separate VP binding and worker construction
from guest resume-to-readiness. Matching host-side phase costs do not require
the total restore latencies or tail behavior to match.

## Restore-time memory activation

Restore-time memory activation separates the immutable RAM geometry recorded
by a snapshot from the active amount selected for one restored process:

- `M0` is the captured base RAM. `memory.bin`, the Linux direct e820 map, and
  the saved RAM-range inventory contain exactly `M0`.
- `Cmem` is the optional immutable capacity declared by `--memory-capacity` at
  capture. The contract records capability version 1, `Cmem`, the 128-MiB Linux
  memory-block size, and every canonical GPA range in `Cmem - M0`.
- `M` is an explicit `--restore-memory` target. It must be 128-MiB aligned and
  satisfy `M0 <= M <= Cmem`. Omitting it restores exactly `M0`; a snapshot
  without the capability rejects an explicit target.

The management RPC exposes the same capture capacity and per-launch target,
together with the processor target, gate timeout, and readiness endpoint.

The selected expansion is always a prefix of the contract's canonical ranges
in logical RAM order. OpenVMM maps the captured ranges privately from
`memory.bin`, maps only the selected suffix with fresh private backing, and
registers all selected ranges with the hypervisor before any restored vCPU
runs. Expansion bytes are neither read from nor written back to the snapshot,
so independent restores may choose different valid targets without changing
the reusable artifact.

An explicit RAM target sets the restore packet's `MEMORY_TARGET` flag, and the
packet lists the selected expansion ranges as `(u64 gpa_start, u64 length)`
pairs ([layout](time-abi.md#restore-packet)). Portb status bit 3 mirrors the
flag and bit 4 indicates that the selected target contains at least one
expansion range. The same packet may carry a processor target. For a nonempty
expansion, OpenVMM keeps external input gated while the Alpine agent verifies
the 128-MiB block size and each range, probes missing blocks through the Linux
memory-block probe interface, onlines them and verifies their online state, and
then completes entropy and generation-ID repair before acknowledging PMIO
`0x605`. Any malformed range or add/online failure terminates the restore.

An explicit base-size target has a zero range count. Unless the tier or a
processor target gates the restore, the packet's `ACK_REQUIRED` flag is clear,
and the agent emits the deterministic zero-add marker without a restore-gate
transaction. This mechanism is one-shot restore repair, not a general
post-readiness memory-hotplug interface.

## Time and entropy

Every microVM uses [NVX time ABI v1](time-abi.md), which defines the guest's
clocks on every backend:

- The guest sees a minimal Hyper-V identity with the declared TSC and LAPIC
  rates in MSRs, a CPU profile pinned for its CPU generation, an invariant TSC
  as its clocksource, and the LAPIC timer at the backend's fixed rate. The
  command line carries no clock tokens
  ([guest-visible contract](time-abi.md#guest-visible-contract)).
- Capture records the time contract and the CPU profile record in the
  [manifest](time-abi.md#snapshot-manifest).
- Restore requires the same backend and CPU generation, a native TSC rate
  within 250 ppm of the declared rate, and an exactly equal LAPIC rate. It
  measures the downtime with same-boot host monotonic time or the host UTC
  delta, sets every VP's TSC at one host instant with read-back, and advances
  the LAPIC timers, VM time, RTC, and PIT before the first VP runs
  ([restore algorithm](time-abi.md#restore-algorithm)). Any failure rejects
  the restore with a stable code; there is no fallback.
- After restore, the guest's snapshot agent sets the wall clock from the
  restore packet's host UTC sample and re-checks conformance
  ([snapshot agent](time-abi.md#snapshot-agent)).
Replaying a snapshot also replays the guest's in-memory random-number-generator
state. Every restore therefore exposes a fresh one-time
[restore packet](time-abi.md#restore-packet) through the portb status/data
protocol; with the time fields and any targets, it carries 64 bytes of fresh
entropy. Every microVM process receives a fresh 16-byte generation ID before
vCPU entry. The ID is not serialized, and the VMM-owned value is not restored
from device state. On restore, the ID is the packet's first 16 entropy bytes;
on cold boot it is generated independently. This keeps the gated repair path
from adding port reads. The Alpine agent keeps the prior ID in its captured
process state, rejects a restored ID that did not change, and exports the
refreshed value to the runtime hook before releasing the gate.

For clone policy, the Alpine restore path credits the seed, including the
generation ID, to the kernel entropy pool, forces a kernel CRNG reseed,
refreshes machine identity, and requires the workload-start runtime hook to
reset runtime-owned RNG state before accepting work. The workload's
`/etc/machine-id` is a read-only bind of a
runtime-tmpfs file, so the agent can refresh it while scratch remains frozen;
the agent also updates the workload's UTS namespace before acknowledgement. It
then acknowledges the VMM gate before thawing scratch and the workload cgroup.
Resume policy records the fresh generation ID while preserving machine
identity and RNG continuity. Fresh post-restore entropy and the replacement
generation ID are never stored in the reusable snapshot; only the prior ID
remains as the agent's comparison token.
