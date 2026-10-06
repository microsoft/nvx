# Integrated implementation status

[Design index](../design.md)

The implementation and remaining proposals separate as follows:

| Area | Current implementation |
| --- | --- |
| Base machine | ACPI-free Linux direct boot with Intel MP tables, allowlisted chipset/PMIO, fixed-role sandbox blocks, eight ABI-2 virtio-mmio slots, optional four-slot ABI-3 image capacity, and 1/2/4/8-vCPU SMP on KVM, MSHV, and WHP, selected explicitly from the command line or the management RPC. Machines without image slots remain ABI 2; slot-declaring machines use ABI 3. Boot layout remains value 2. |
| Snapshot and restore | Guest-requested version-5 publication, same-backend restore, exact saved inventory, private COW RAM, optional fresh RAM expansion, fresh/paired scratch, three sandbox tiers, input gating, single-use resume claims, a single-use restore readiness event, and management exclusion at the capture boundary. The management RPC drives the same capture and restore for blockless machines without a NIC or control console. Older supported manifests remain readable subject to their recorded capabilities. |
| Resource activation | Opt-in CPU-prefix and 128-MiB-aligned RAM targets within immutable capacity. Explicit MSHV targets materialize only that VP prefix and cannot be saved again; KVM, WHP, and untargeted MSHV retain full VP capacity. |
| Console | Boot virtio-console with private RX/TX state and reconnect policies; bounded portb/host-relay drain at process exit, with guest exit status propagated to management-RPC waiters. The separate control console has authenticated Linux and Windows endpoints, bounded framing with receive credits, per-instance epochs, broker saved state, and managed workload RPC. |
| Network | Static identity, fixed transport, the portable in-process Consomme endpoint, directional egress `allow`/`deny`, fixed ingress `deny`, L3/L4 egress rules, host-loopback deny or explicit port publishing, an exact proxy exception, and quiesced restore are implemented. Stateful replies remain available; unrestricted ingress is rejected before launch. Capture drains packet ownership instead of serializing arbitrary pending packets or host flow state. |
| Guest filesystem | Kernel features and shell bootstrap for EROFS over ext4 scratch, overlayfs, namespace/cgroup isolation, capability stripping, supervision, and low-level snapshot hooks. Not a complete OCI agent or public sandbox restore workflow. |
| Host filesystem | Fixed no-DAX HostFs in a slot that command-line and management-RPC cold boots always expose and that stays dormant without an attachment, live attachment revalidation, server-side denied subtrees with alias-identity enforcement, and guest-created symbolic links on read-write exports with exact targets that the host never follows. Provider-backed immutable filesystem generations remain outside the current profile. |
| Workload policy and operations | Host-owned non-root workload identity, one-shot or managed lifecycle selection, bounded local outcome reports, and opt-in lifecycle profiling that does not change ordinary runs. |
| Production services | Replaceable configuration, a Rust guest agent, image conversion/distribution, artifact lifecycle, and tenant-aware snapshot instantiation remain proposals. The current static guest agent and authenticated managed RPC implement only the bounded lifecycle subset. |

The end-to-end tests establish process-boundary behavior for the available
native host backend. They do not make every future extension a portability
guarantee; the versioned code contract and the
[current limits](current-limits.md) remain authoritative.
