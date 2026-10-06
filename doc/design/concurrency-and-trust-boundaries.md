# Concurrency and trust boundaries

[Design index](../design.md)

The guest controls PMIO accesses, virtio descriptors, packet data, FUSE
requests, control-session records, and the timing of a snapshot request.
Kernel, initramfs, command line, snapshot artifacts, restore attachments, and
host control-console clients are also untrusted inputs.

The implementation therefore uses checked address arithmetic, bounded buffers
and tables, typed validation errors, rate-limited guest-triggerable logs, and
fallible quiesce/restore operations. Native I/O callbacks only record bounded
state or enqueue notifications; they do not perform blocking snapshot or host
resource work. Malformed saved device state is validated before workers start,
and no vCPU runs after a partial restore failure. A failure after vCPU
stopping begins at a snapshot or post-restore acknowledgment boundary is
terminal: host input stays gated and the VM is torn down instead of resuming
with uncertain state.

A host control client reaches the guest only through the control-console
broker. The broker admits one local peer whose operating-system identity is
the OpenVMM user, requires a launcher-provided capability before any host data
reaches the guest, and bounds every record, queue, and credit window in both
directions; malformed, stale, or out-of-order records fail closed. The
capability never appears in arguments, environment variables, logs, snapshots,
or attachment identities. The state-control endpoint, which pauses and resumes
the VM without reaching the guest, admits the same local peer and requires the
same capability before it answers. It serves one host and one request at a
time; a failed or stalled authentication, a malformed request, or 60 s without
a request closes the connection without a response.

Host-facing outputs are bounded as well. The optional outcome report is written
after teardown to a previously absent local path and records only a schema
version, an opaque instance ID, the backend category, the operation and
outcome category with a numeric status, whether the network policy was
requested and applied with its mode and rule counts, and per-step teardown
flags. It never contains commands, environment values, paths, network
destinations, proxy details, workload output, credentials, or free-form error
text, and OpenVMM does not upload it.
