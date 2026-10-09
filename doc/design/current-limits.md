# Current limits

[Design index](../design.md)

The current ABI family intentionally does not provide:

- processor counts other than 1/2/4/8, SMT, multiple NUMA nodes, non-x86 guests, or nested virtualization;
- firmware boot, caller-defined ACPI, SMBIOS, PCI, VPCI, VMBus, arbitrary
   post-readiness CPU or memory hotplug, or arbitrary devices;
- cross-hypervisor snapshot restore;
- capture-and-continue, live migration, or saving a reduced-prefix MSHV
   restore;
- restore-time RAM shrinking, targets beyond the captured capacity,
   non-128-MiB targets, or expansion from snapshots without an opt-in capacity;
- ABI value 1 or boot-layout value 1 snapshots;
- snapshot block media other than cached regular raw files;
- copy-on-write paired-scratch restore on non-Linux hosts or filesystems
   without reflink support;
- direct-claimed paired-scratch restore on filesystems without exact-file
   hard-link support;
- more than four image slots, writable image slots, binding an inactive slot,
   rebinding or ejecting media, or activating a slot after restore readiness;
- sandbox-block, network, or control-console construction through the
   management RPC, or its restore of snapshots that contain them;
- host networking other than the in-process portable endpoint, unrestricted
   ingress, generic bidirectional host-loopback connectivity, or live
   host-loopback port forwards in snapshots;
- a production control-session RPC protocol beyond the managed lifecycle
   subset;
- replaceable sandbox configuration, a production OCI agent, or a public sandbox snapshot workflow;
- serialization of live network flows or native host handles;
- snapshotting of the contents of a live virtio-fs export; or
- compatibility with standalone NVX `MVMSNAP*` or `WHPSNAP*` files.

The tier contract and post-restore gate are currently low-level OpenVMM and
guest-agent primitives exercised by the native lifecycle suite. The public
`nvx sandbox` command does not yet orchestrate platform builds, warm-shim
handoff, checkpoints, or sandbox restore. That integration depends on the
replaceable configuration region, production agent, and versioned control
protocol tracked by issues #158, #159, and #160. In particular, a platform
snapshot must be produced only by a trusted pre-image-binding workflow: the
current host validation rejects tenant command-line configuration and records
layer identities as unbound, but cannot prove that arbitrary guest code did
not read an attached layer before requesting capture.

The authenticated control console, its bounded record framing and broker saved
state, the snapshot RPC exclusion, and the managed lifecycle subset are the
transport foundation for that protocol, not completion of it. The shell
bootstrap does not implement a fleet-safe platform build point, warm-runtime
handoff, arbitrary OCI policy, or production agent resource guarantees.

Image slots extend ABI 2 in place: a machine that does not declare them keeps
the original device topology, and development snapshots from before this
extension are not guaranteed to remain compatible. Otherwise, changing a
guest-visible address, IRQ, command-line token, feature mask, queue
shape, time policy, or device behavior requires a new microVM ABI version. A
backend-specific difference is valid only when it is explicitly part of that
versioned contract, such as the virtio-net IRQ.
