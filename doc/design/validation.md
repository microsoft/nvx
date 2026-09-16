# Validation

[Design index](../design.md)

The implementation is exercised at three levels:

- loader, command-line, memory-layout, RTC, PMIO, network-policy, snapshot
  format, and device-private-state unit tests;
- self-contained OpenVMM Petri lifecycle and TTRPC tests using the
  checkout-built [`guest_test_pvh`](../../openvmm/guest_test_pvh); and
- NVX-owned process tests in
  [`scripts/nvx_tools/microvm_tests.py`](../../scripts/nvx_tools/microvm_tests.py)
  using this repository's Linux kernel and Alpine initramfs through the public
  OpenVMM CLI.

The NVX-owned suite boots the same PVH artifacts on the available native
backend and covers IRQ0/RTC behavior, raw portb I/O, shutdown status, exact
snapshot sequencing, repeated immutable restore, coherent downtime, fresh
generation IDs, `getrandom()` output, kernel UUIDs, temporary-file identifiers,
entropy reseed, active console RX/TX, network policy and HTTP traffic, and live
virtio-fs attachment revalidation. Directional network coverage verifies an
egress request and response with ingress denied, denial of a host connection
to an active guest listener, complete egress denial, and pre-boot rejection of
unsupported ingress on KVM, MSHV, and WHP. Sandbox coverage adds deterministic
active block-I/O drain, paired scratch publication, two private restores,
fresh scratch replacement, and pre-entry rejection of missing, corrupt,
mismatched, or wrong-geometry media. The native suite targets KVM, MSHV, and
WHP; a passing run on one backend is not a fresh result for the others.
Coverage also includes
1/2/4/8-vCPU topology, APIC identity,
pinned per-vCPU execution, timer/interrupt progress, reset, cancellation,
count and topology mismatch rejection, and repeated immutable restore.
Restore-time processor coverage captures one capacity-8 template with a
boot-online count of one, restores it at 1/2/4/8 online VPs, schedules work on
every requested CPU, and verifies that the artifact is unchanged.
Restore-time memory coverage captures 512 MiB with a 2-GiB capacity, restores
the same artifact at 512 MiB, 1 GiB, and 2 GiB, validates the added-byte count
and expanded allocation, and verifies artifact immutability. Unit coverage
verifies that only an explicit MSHV processor target selects a runtime prefix,
that the complete saved VP inventory is validated before filtering, that
reduced-prefix saves are rejected, and that dormant VP access fails cleanly.
Lifecycle profiling verifies that MSHV binds exactly the requested prefix while
fixed-capacity comparisons retain equivalent per-prefix binding and
worker-construction costs.
Additional unit coverage exercises the control-console reservation and
attachment inventory, command-line spoofing rejection, management exclusion
at the snapshot boundary, output-drain completion and failures, and backend
TSC repair. Hardware-dependent clock tests still require their native backend.
Platform CI and the benchmark histories in `data/` provide the wider host
matrix.
