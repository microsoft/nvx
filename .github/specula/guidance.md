# NVX OpenVMM snapshot/restore verification scope

Analyze the exact `nanvix/openvmm` revision pinned by the selected NVX release. The target is NVX's guest-requested MicroVM snapshot and restore path, especially the `nvx-snapshot` request, capture boundary, source termination, restore continuation, restore entropy, and reuse of one untiered snapshot by independent restore processes.

Focus modeling and code analysis on the state machines and concurrency boundaries that can violate safety or liveness: request coalescing and acknowledgement, polling versus immediate I/O completion, capture publication ordering, source shutdown, snapshot artifact immutability, restore gates, private-memory reset, entropy injection, timer/clock continuity, and repeated restoration from the same source snapshot.

The runner is Linux x86_64 with KVM. Use OpenVMM's `virt_kvm` feature and `--hypervisor kvm`; do not assume MSHV. Prefer existing OpenVMM MicroVM, snapshot, chipset, and ttrpc tests as implementation evidence. Build and execute real tests when practical, but use explicit finite timeouts and record any environment-limited result rather than fabricating VM evidence.

Keep the model scoped to the NVX-specific snapshot/restore implementation rather than all of OpenVMM. Preserve a reusable harness and model suitable for Specula incremental CI. A completed initialization must include a nonempty `spec/base.tla` and `harness/run.sh`; later runs should update those assets from the cumulative source diff instead of restarting analysis.
