# Validation

[Design index](../design.md)

The implementation is exercised at three levels:

- loader, command-line, memory-layout, RTC, PMIO, network-policy, snapshot
  format, and device-private-state unit tests;
- OpenVMM VMM tests invoked through `nvx.py test-openvmm`: a test-harness
  microVM lifecycle test and a management-RPC lifecycle, SMP, and snapshot
  test that uses NVX's ACPI-free, MP-enabled x86-64 Linux-direct kernel and
  initramfs; and
- NVX-owned process tests using this repository's Linux kernel and selected
  Alpine or Ubuntu initramfs through the public OpenVMM CLI.

The harness lifecycle test covers portb I/O, status shutdown, rejection of
host save and pulse save/restore, and a snapshot request that continues
without a configured destination. With NVX's kernel, the management-RPC test
boots 1, 2, 4, and 8 vCPUs, captures a snapshot with an immutable RAM capacity
and a boot-online prefix through the RPC, restores it twice with readiness
signaling and processor and memory targets, and verifies that the artifacts
are unchanged.

The NVX-owned suite boots the same Linux-direct artifacts on the available native
backend and covers IRQ0/RTC behavior, raw portb I/O, shutdown status, exact
snapshot sequencing, repeated immutable restore, time ABI conformance and
restore downtime, fresh
generation IDs, `getrandom()` output, kernel UUIDs, temporary-file identifiers,
entropy reseed, active console RX/TX, network policy and HTTP traffic, and live
virtio-fs attachment revalidation, including a guest-created symbolic link held
across capture. Directional network coverage verifies an
egress request and response with ingress denied, denial of a host connection
to an active guest listener, complete egress denial, and pre-boot rejection of
unsupported ingress on KVM, MSHV, and WHP. L3/L4 coverage verifies TCP and UDP
port rules, overlapping deny precedence, default-deny behavior, and pre-boot
rejection of malformed or incomplete rules. Protocol coverage verifies that
TCP, UDP, and ICMP rules without a port, `any` with a port, and `any` alone
each match exactly their traffic, both as allow rules and as deny rules that
take precedence over an allow rule, with the other protocols and the next port
as nearby cases. Port-range coverage verifies that TCP, UDP, and `any` ranges
each match their first and last ports and a port inside them, but neither the
port below nor the port above them nor another protocol, both as allow rules
and as deny rules that take precedence over an allow rule; that a deny rule for
one port inside an allowed range blocks that port alone; that each range
reaches OpenVMM as one native range rule; and that OpenVMM and `nvx.py run`
reject reversed ranges and ranges without a first port before launch. IPv6
coverage verifies, with MXC-shaped policy files, the guest's derived IPv6
address and default route, TCP, UDP, and ICMPv6 to the IPv6 gateway, a `deny`
default without rules that blocks both families, port and protocol rules, an
IPv6 port range, deny precedence within `::/0`, a rule without destinations that
admits both families, a `deny` default that IPv4 rules do not widen to IPv6, an
IPv6 deny rule under an `allow` default that leaves IPv4 alone, the DNS server
that each policy gives the guest, the IPv6 path across snapshot restore,
rejection of a restore whose policy differs, and pre-boot rejection of an
invalid IPv6 prefix. The Ubuntu and Azure Linux virtio-net smoke tests also
verify the IPv6 identity.
Host-loopback coverage verifies
general guest-to-host denial, an exact proxy exception, explicit
localhost-to-guest forwarding under allow, and pre-boot rejection when both
directions cannot be enforced. Managed lifecycle coverage authenticates the
dedicated control channel, runs multiple workloads in one warm VM across
reconnects, preserves guest state, reports execution timeout, and stops the VM
cleanly. Public sandbox coverage drives `nvx.py sandbox` itself: request-scoped
execution options, identity refusal, fail-closed lifecycle transitions,
scratch-backed state across requests and restarts, bounded output and outcome
reports, a crashed OpenVMM's stale runtime state, and teardown that leaves no
OpenVMM process, control endpoint, capability, or runtime record while the
supplied layer and scratch survive. Its guest probe verifies the sandbox's
identity, capability, `no_new_privs`, namespace, device, sysfs, cgroup,
resource-limit, image-layer, and scratch invariants. Sandbox coverage adds
deterministic active block-I/O drain, paired
scratch publication, two private restores, fresh scratch replacement, and
pre-entry rejection of missing, corrupt, mismatched, or wrong-geometry media.
Denied-filesystem coverage verifies listing suppression, allowed writes,
direct and parent-relative denial, symlink/junction alias denial, guest-created
links into the denied subtree, a second
virtio-fs mount, and pre-boot rejection of unsafe path policies.
Caller-ownership coverage verifies that files guest root creates belong to the
export owner, that squashed root cannot chown files or create device nodes,
that a foreign guest identity fails with `EPERM` unless OpenVMM holds
`CAP_SETUID` and `CAP_SETGID`, in which case it owns what it creates, and
pre-boot rejection of a root-owned export or, on Windows, of the mode itself.
Concurrent-share coverage attaches a read-write workspace, a read-only tool
cache, and a data directory as the children of one aggregate on the microVM's
single virtio-fs slot, each with its own denied paths. The data directory
hides its root except for one directory and one file, the only writable path
in it. The guest checks that each child is bound at its target with its
mode, writes to the workspace and to that file, cannot write to the tool
cache, also through a symbolic link, and cannot create names in the hidden
root. Through the aggregate's read-write, root-only mount, OpenVMM still
returns `EROFS` for every mutation of the tool cache, keeps denied paths
hidden, and refuses hard links between children. OpenVMM rejects a repeated
`--mount`, `--mount` beside an aggregate, overlapping children, and a
relative denied path before boot. A snapshot round-trips an open workspace
file, and a restore that drops, swaps, or renames a child, or offers a single
`--mount` instead, fails before the guest resumes.
Access-policy coverage narrows a read-write share's writes to a writable
directory and file, and verifies guest writes, renames, and links inside them
and `EROFS` from OpenVMM for every other mutation, including moves into and
out of the writable paths and mutations through a second mount of the tag, as
well as `EXDEV` for a hard link of a read-only file. It denies a directory
except an allowed child, inside which a denied grandchild stays hidden, and
verifies that the denied directory lists and exposes only the allowed child.
It rejects invalid policies before boot and round-trips the policy through a
snapshot whose restore requires the same policy and keeps a writable handle.
The native suite targets KVM, MSHV, and WHP; a passing run on one backend is
not a fresh result for the others.
Coverage also includes
1/2/4/8-vCPU topology, APIC identity,
pinned per-vCPU execution, timer/interrupt progress, reset, cancellation,
count and topology mismatch rejection, and repeated immutable restore.
Restore-time processor coverage captures one capacity-8 template with a
boot-online count of one, restores it at 1/2/4/8 online VPs and without a
target, schedules work on every requested CPU, and verifies that the artifact
is unchanged. Its VP-binding lifecycle records verify that MSHV binds exactly
the requested prefix, while untargeted MSHV restores and all KVM and WHP
restores bind the full capacity. The profiled `snapshot-restore-vcpu`
benchmark reports the same VP-binding and worker-construction phases for
comparison with fixed-capacity restores.
Restore-time memory coverage captures 512 MiB with a 2-GiB capacity, restores
the same artifact at 512 MiB, 1 GiB, and 2 GiB, validates the added-byte count
and expanded allocation, and verifies artifact immutability. Unit coverage
verifies that only an explicit MSHV processor target selects a runtime prefix,
that the complete saved VP inventory is validated before filtering, that
reduced-prefix saves are rejected, that dormant VP access fails cleanly, and
that the MSHV synchronized TSC set targets only created VPs.
Additional unit coverage exercises the control-console slot and attachment
inventory, command-line spoofing rejection, the control-session record
protocol against language-neutral golden vectors and boundary cases, the
broker state machine and its save and restore, peer-identity and capability
admission for local endpoints, workload identity and lifecycle ownership,
the bounded outcome-report schema, egress-policy enforcement in the endpoint
and virtio-net layers, the virtio-fs microVM profile and its access policy,
state-unit quiesce and rollback, management exclusion at the snapshot
boundary, management-RPC guest-exit propagation, output-drain completion and
failures, and the time ABI's rate, downtime, LAPIC, restore-packet, and
manifest rules. Hardware-dependent time ABI tests, such as the synchronized TSC
set, identity routing, and the CPU profile read-back, still require their
native backend; the [time ABI test matrix](time-abi.md#test-matrix) lists the
conformance, restore, soak, and performance coverage. A host-only benchmark
measures snapshot publication, restore preparation, copy-on-write dirtying, and
fresh-process restore preparation without booting a guest. Platform CI and the
benchmark histories in `data/` provide the wider host matrix.

The separate `test-adversarial` harness adaptively selects tracked
deterministic primitives from these same process tests. Copilot has no tools
or direct runtime access; a typed broker records the selection and a
credential-free executor runs the existing scenario on a disposable target.
Independent filesystem/network canaries, resource heartbeats, complete
teardown, and a fresh post-campaign lifecycle boot add containment and
availability oracles. Unit tests inject canary, network, teardown, timeout,
schema, replay, and Copilot-permission faults. Hardware-backed campaigns remain
backend-specific and do not replace deterministic pull-request coverage.
