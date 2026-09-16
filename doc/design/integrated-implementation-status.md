# Integrated implementation status

[Design index](../design.md)

The implementation and remaining proposals separate as follows:

| Area | Current implementation |
| --- | --- |
| Base machine | PVH, MP/minimal ACPI, allowlisted chipset/PMIO, fixed-role sandbox blocks, eight reserved virtio-mmio slots, and 1/2/4/8-vCPU SMP on KVM, MSHV, and WHP. Persisted ABI and PVH layout remain value 2. |
| Snapshot and restore | Guest-requested version-5 publication, same-backend restore, exact saved inventory, private COW RAM, optional fresh RAM expansion, fresh/paired scratch, three sandbox tiers, input gating, single-use resume claims, and management exclusion at the capture boundary. Older supported manifests remain readable subject to their recorded capabilities. |
| Resource activation | Opt-in CPU-prefix and 128-MiB-aligned RAM targets within immutable capacity. Explicit MSHV targets materialize only that VP prefix and cannot be saved again; KVM, WHP, and untargeted MSHV retain full VP capacity. |
| Console | Boot virtio-console with private RX/TX state and reconnect policies; bounded portb/host-relay drain at process exit. The separate control console has reservation and snapshot lifecycle support but no public authenticated activation. |
| Network | Static identity, fixed transport, the portable in-process Consomme endpoint, directional egress `allow`/`deny`, fixed ingress `deny`, L3/L4 egress rules, bidirectional host-loopback policy, an exact proxy exception, and quiesced restore are implemented. Stateful replies remain available; unrestricted ingress is rejected before launch. Capture drains packet ownership instead of serializing arbitrary pending packets or host flow state. |
| Guest filesystem | Kernel features and shell bootstrap for EROFS over ext4 scratch, overlayfs, namespace/cgroup isolation, capability stripping, supervision, and low-level snapshot hooks. Not a complete OCI agent or public sandbox restore workflow. |
| Host filesystem | Fixed no-DAX HostFs and live attachment revalidation. Provider-backed immutable filesystem generations remain outside the current profile. |
| Production services | Replaceable configuration, Rust agent, authenticated RPC, image conversion/distribution, artifact lifecycle, and tenant-aware snapshot instantiation remain proposals, not completed VMM features. |

The end-to-end tests establish process-boundary behavior for the available
native host backend. They do not make every future extension a portability
guarantee; the versioned code contract and the
[current limits](current-limits.md) remain authoritative.
