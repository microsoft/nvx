# Time ABI

[Design index](../design.md)

**Proposed.** This document specifies NVX time ABI v1, the single
guest-visible time, timer, and clock contract of the microVM profile on KVM,
MSHV, and WHP. NVX and its OpenVMM fork (`nanvix/openvmm`) implement it on
their `time-abi-v1` integration branches, and it becomes part of the current
machine contract when they pass validation and merge. It replaces the earlier
time rules of [Snapshot and restore](snapshot-and-restore.md#time-and-entropy)
and the clock tokens of [Cold boot](cold-boot.md#effective-command-line).

Notation:

| Symbol | Meaning |
| --- | --- |
| `F` | Declared guest TSC rate in Hz, returned by MSR `0x40000022` |
| `F_s` | Declared rate recorded in a snapshot |
| `F_d` | Native TSC rate of the destination partition in Hz |
| `L` | LAPIC timer rate in Hz, returned by MSR `0x40000023` |
| `C` | VP capacity of the machine (1, 2, 4, or 8) |
| `D` | Snapshot downtime in nanoseconds |
| `T_c` | Guest TSC of VP 0 at the capture anchor |
| `g` | Restore generation counter of a VM process |

"Reject" means fail with the named error code and never fall back to another
behavior. Every check in this document is a hard check.

## Scope, goals, and non-goals

Goals:

- One time ABI on every backend and for every microVM, whether launched from
  the command line or through OpenVMM's management endpoint: the same CPUID
  identity, synthetic MSRs, CPU time bits, clocksource, tick, timers, and
  restore semantics.
- No guest kernel patches and no clock command-line tokens.
- No implicit fallbacks. A host or snapshot that cannot meet the contract is
  rejected with a stable error code.
- Restore within one backend across hosts of the same CPU generation and CPU
  profile, including restores after a host reboot.
- Guest monotonic time advances by the snapshot downtime. Wall clock is set
  on every restore and disciplined afterwards.
- RCU-stall, soft-lockup, hung-task, clocksource, and TSC-warp detectors never
  trip across capture and restore. A detected violation fails fast.
- No performance regression beyond the gate in
  [Performance expectations and acceptance gate](#performance-expectations-and-acceptance-gate).

Non-goals:

- Restore across backends or across CPU generations. Both are rejected.
- Restore by an OpenVMM build that computes a different
  [effective CPUID](#cpu-profiles) for the snapshot's profile and topology.
  It is rejected with `E_CPU_SURFACE`.
- TSC scaling and RDTSC trapping.
- Paravirtual clocks: kvmclock, the Hyper-V reference TSC page and reference
  counter, synthetic timers, VMGenID, and VMClock.
- A virtual PMU. AMD profiles are deferred until an AMD host is registered.
- Live migration, standard-machine guests, and guests other than the NVX
  Linux 6.18 kernel.
- Snapshots older than manifest version 6. They are rejected with a recapture
  error.

The time ABI is versioned independently of the microVM machine ABI:
`time_abi_version` is 1 in the manifest and in CPUID leaf `0x40000002`, while
the microVM ABI version stays 2. Performance history keys its baselines by
microVM ABI version, so the time ABI does not create a new baseline series.

## Guest-visible contract

### Hypervisor identity

Every VP sees a minimal Hyper-V frequency and invariant-TSC identity. OpenVMM
configures exactly these six leaves on every backend:

| Leaf | EAX | EBX | ECX | EDX |
| --- | --- | --- | --- | --- |
| `0x40000000` | `0x40000005` (maximum leaf) | `0x7263694d` (`"Micr"`) | `0x666f736f` (`"osof"`) | `0x76482074` (`"t Hv"`) |
| `0x40000001` | `0x31237648` (`"Hv#1"`) | 0 | 0 | 0 |
| `0x40000002` | `0x0058564e` (`"NVX"`, build) | `0x00010000` (time ABI 1.0) | 0 | 0 |
| `0x40000003` | `0x00008860` | 0 | 0 | `0x00000100` |
| `0x40000004` | 0 | `0xffffffff` | 0 | 0 |
| `0x40000005` | `C` | `C` | 0 | 0 |

`0x40000003` EAX grants exactly `AccessHypercallMsrs` (bit 5) and
`AccessVpIndex` (bit 6), which Linux requires to detect the platform, plus
`AccessFrequencyRegs` (bit 11) and `AccessTscInvariantControls` (bit 15). EDX
sets only `FrequencyRegsAvailable` (bit 8). `0x40000004` recommends nothing
and sets the spinlock retry count to "never notify".

OpenVMM also programs explicit zero results for `0x40000006..=0x4000000f`
and `0x40000080..=0x40000082`, so they read zero on every backend. The
contract promises zeros only for these leaves: every other leaf in
`0x40000006..=0x400000ff` returns either all zeros or the vendor's
architectural out-of-range result (on Intel, the result of the highest basic
leaf for the same subleaf). KVM returns that result for every leaf missing
from its CPUID table, whose 256 entries cannot list the whole range.

Every leaf beyond the identity leaves that Linux 6.18 reads with this
identity is an explicit zero. Built as NVX builds it (`CONFIG_HYPERVISOR_GUEST`
without `CONFIG_HYPERV`, `CONFIG_KVM_GUEST`, or Xen, ACRN, Jailhouse, and
bhyve guest support), Linux probes only VMware and Hyper-V, and these are all
its reads in the range:

| Leaf | Reader | Condition |
| --- | --- | --- |
| `0x40000000` | `vmware_platform`, `ms_hyperv_platform`, `ms_hyperv_init_platform` | Always |
| `0x40000003` to `0x40000005` | `ms_hyperv_platform`, `ms_hyperv_init_platform` | Always |
| `0x40000081` | `ms_hyperv_msi_ext_dest_id` | x2APIC available without interrupt remapping |
| `0x40000082` | `ms_hyperv_msi_ext_dest_id` | `0x40000081` EAX is `VS#1`: never |
| `0x4000000a` | `ms_hyperv_init_platform` | Maximum leaf at least `0x4000000a`: never |
| `0x4000000c` | `ms_hyperv_init_platform` | `HV_ISOLATION` in `0x40000003` EBX: never |
| `0x40000010` | `vmware_select_hypercall` | VMware signature at `0x40000000`: never |

The other explicit zeros serve Hyper-V-aware software that probes further.
No leaf returns Hyper-V feature data, a `VS#1` interface signature at
`0x40000081`, or any hypervisor signature. No base `0x40000100..=0x4000ff00`
(step `0x100`) carries a KVM, Xen, VMware, or other hypervisor signature, so
Linux selects only the Hyper-V platform.

### MSRs

OpenVMM serves the identity MSR range `0x40000000..=0x400001ff` identically
on every backend:

| MSR | Name | Read | Write |
| --- | --- | --- | --- |
| `0x40000002` | `HV_X64_MSR_VP_INDEX` | VP index | #GP |
| `0x40000022` | `HV_X64_MSR_TSC_FREQUENCY` | `F` | #GP |
| `0x40000023` | `HV_X64_MSR_APIC_FREQUENCY` | `L` | #GP |
| `0x40000118` | `HV_X64_MSR_TSC_INVARIANT_CONTROL` | Last value written, 0 after reset | 0 or 1 accepted; any other value #GP |
| Any other MSR in the range | | #GP | #GP |

`HV_X64_MSR_TSC_INVARIANT_CONTROL` is one partition-wide value. It is guest
state: it is saved with the VM and restored with it. Linux writes 1 once at
boot, on the BSP, with an unchecked `wrmsrq`, so this write must never fault
on any backend. Clearing it after setting it is accepted.

The CPU time bits hide two architectural timer MSRs, and every backend makes
them raise #GP: `IA32_TSC_ADJUST` (`0x3b`) and `IA32_TSC_DEADLINE`
(`0x6e0`).

The legacy P6 L2-cache MSRs (`0x88..=0x8a`, `0x116`, `0x118..=0x11b`, and
`0x11e`) also raise #GP on every backend, as MSHV's hypervisor already
makes them do; no profile pins them. KVM and WHP stub them only for Windows
booting from the PCAT BIOS, and standard-machine partitions keep those
stubs. Linux 6.18 never accesses them. A fault on an MSR that the guest
kernel accesses unchecked logs `unchecked MSR access error`, which the boot
check (`C4`) and the violation watcher (`G_UNCHECKED_MSR`) reject.

### CPU time bits

Every CPU profile fixes these bits. A profile that violates them is rejected.

| Leaf | Register and bits | Value |
| --- | --- | --- |
| `0x1` | ECX[31] hypervisor present | 1 |
| `0x1` | ECX[24] TSC-deadline timer | 0 |
| `0x1` | ECX[15] PDCM | 0 |
| `0x1` | EDX[4] TSC | 1 |
| `0x6` | EAX, EBX, ECX, EDX | `0x00000004` (ARAT only), 0, 0, 0 |
| `0x7.0` | EBX[1] `IA32_TSC_ADJUST` | 0 |
| `0xa` | EAX, EBX, ECX, EDX | 0 (no PMU) |
| `0x15`, `0x16` | EAX, EBX, ECX, EDX | 0, when within the maximum basic leaf |
| `0x80000001` | EDX[27] RDTSCP | 1 |
| `0x80000007` | EAX, EBX, ECX, EDX | 0, 0, 0, `0x00000100` (invariant TSC only) |

Leaves `0x15` and `0x16` are zeroed rather than synthesized. Linux takes both
rates from the frequency MSRs, and zero leaves keep the profile's CPUID
independent of any host's rate.

Linux trusts the TSC because of `AccessTscInvariantControls`, not because of
CPUID `0x80000007` EDX[8]; that bit only adds the `nonstop_tsc` flag and
spares each AP a delay-loop calibration of about 150 ms (on Azure MSHV, an
8-vCPU cold boot takes 1,385 ms without the bit and 299 ms with it). The time
bits override the fingerprint: the bit is policy, not a fingerprint feature.
Profiles set it even where the backend cannot offer it to guests through its
feature banks, as on Azure's WHP and nested MSHV, whose host OS sees an
invariant TSC. Every backend exposes it (MSHV and WHP through a CPUID
override), and host qualification backs it with measured invariance (see
[Host qualification](#host-qualification)).

### Rates

- **TSC.** The guest TSC runs at the destination's native rate `F_d` and is
  never scaled. The declared rate `F` is fixed for the life of a snapshot
  lineage: a cold boot declares its native rate, and every restored process
  keeps `F = F_s`. OpenVMM does not capture a restored process
  (`--snapshot-destination` and `--restore-snapshot` are exclusive, and so
  are the management endpoint's capture and restore paths), so a lineage is
  one capture and its restores; if re-capture is added, the new snapshot
  records `F_s` and the restored process's `g`. The kernel computes
  `tsc_khz = floor(F / 1000)` once and never recalibrates. The
  [rate policy](#tsc-rate-policy-and-lapic-rate-rule) bounds
  `|F_d - F| / F`.
- **LAPIC.** `L` is a per-backend constant: 1,000,000,000 Hz on KVM and
  200,000,000 Hz on MSHV and WHP. Linux sets `lapic_timer_period = L / HZ`
  (`0x989680` on KVM, `0x1e8480` on MSHV and WHP, with HZ=100) and never
  calibrates the LAPIC.

### Clocksource, tick, and PIT

With this identity, Linux 6.18 built without `CONFIG_HYPERV`:

- reads `F` from MSR `0x40000022`, sets `X86_FEATURE_TSC_KNOWN_FREQ`, and
  registers clocksource `tsc` at `device_initcall` with no refinement;
- writes 1 to MSR `0x40000118` and sets `X86_FEATURE_TSC_RELIABLE`, which
  disables the clocksource watchdog for `tsc` and the CPU-online TSC warp
  test;
- presets `lapic_timer_period` from MSR `0x40000023`, sets `no_timer_check`,
  and disables the hard-lockup detector; and
- disables the PIT at boot because the TSC rate is known, ARAT is present,
  and the LAPIC period is preset.

The guest must then be in this state:

- `/proc/cpuinfo` flags on every CPU include `tsc`, `constant_tsc`,
  `nonstop_tsc`, `tsc_known_freq`, `tsc_reliable`, `rdtscp`, `hypervisor`,
  and `arat`, and exclude `tsc_deadline_timer` and `tsc_adjust`.
- `current_clocksource` is `tsc`, and `available_clocksource` is `tsc`
  alone: Linux lists `refined-jiffies` and `jiffies` only while the reading
  CPU's tick is periodic, which ends at that CPU's first tick.
- Ticks are tickless-idle with high-resolution timers. Every online CPU's
  tick device is `lapic` in one-shot mode from its first tick onward. No
  `pit` or `hpet` clock event device and no broadcast device exist.
- The LAPIC timer is never periodic after boot and never in TSC-deadline mode.
- PIT channel 0 never counts after boot, and IRQ 0 has no timer handler.

Because the kernel no longer checks cross-vCPU TSC consistency, the
[skew bound](#cross-vcpu-skew-bound) is enforced outside the guest.

### Deviations from the Hyper-V TLFS

- `0x40000003` advertises `AccessHypercallMsrs` only for platform detection.
  `HV_X64_MSR_GUEST_OS_ID` (`0x40000000`) and `HV_X64_MSR_HYPERCALL`
  (`0x40000001`) raise #GP, and there is no hypercall page. The NVX kernel is
  built without `CONFIG_HYPERV` and never touches them. `VMCALL` behavior is
  outside the contract; the guest never executes it.
- `HV_X64_MSR_TSC_FREQUENCY` returns the declared rate `F`, which differs
  from the physical rate `F_d` by up to the rate tolerance after a restore.
- `HV_X64_MSR_TSC_INVARIANT_CONTROL` can be cleared after it is set; Hyper-V
  and KVM raise #GP instead.
- On MSHV and WHP, CPUID `0x80000007` EDX[8] is set by the CPU profile from
  boot; it does not wait for a write to `HV_X64_MSR_TSC_INVARIANT_CONTROL`.
  KVM 6.3 and newer follow the TLFS and hide the bit while KVM's own copy of
  the control is 0, so the KVM backend mirrors the guest-visible value into
  KVM (see [Backend obligations](#backend-obligations)). Linux writes 1 in
  `ms_hyperv_init_platform`, before `identify_boot_cpu` and the APs read the
  bit, so the booted guest is identical on every backend.
- `0x40000002` carries an NVX signature and the time ABI version, not a
  Hyper-V build number.
- `0x40000005` reports the VP capacity in both EAX and EBX.

Side effects on Linux are limited to selecting the Hyper-V platform: an
unknown-NMI handler is registered, `pv_info.name` is `Hyper-V`, the
hard-lockup detector is off, and `no_timer_check` is set. No KVM
paravirtual feature (kvmclock, PV EOI, PV IPIs, PV TLB flush, PV spinlocks,
halt polling, steal time, or async page faults) exists on any backend;
`sched_clock` and the vDSO use the raw TSC.

On KVM, the identity therefore forgoes the paravirtual IPIs, TLB flushes,
halt polling, and steal time that a KVM signature would give a multi-vCPU
guest. That may be why the guest's side of an 8-vCPU cold boot on nested
Azure KVM measured 8.2 [0.4, 15.9] ms slower than the `dev` base's; the
difference is unattributed, and the VMM's gain offsets it (see
[Performance expectations](#performance-expectations-and-acceptance-gate)).
Hyper-V's own enlightenments could restore paravirtual IPIs and TLB flushes
under the identity, but they need the guest's `CONFIG_HYPERV` and hypercall
support in OpenVMM, so they are left for after v1.

## Backend obligations

Each backend implements these mechanisms behind one OpenVMM interface. The
guest-visible result is identical; only the mechanism differs. The spikes
(`p1-spike-kvm`, `p1-spike-mshv`, `p1-spike-whp-tsc`) demonstrated every
settled cell on the registered hosts.

| Obligation | KVM | MSHV | WHP |
| --- | --- | --- | --- |
| Identity CPUID (exact leaves, out-of-range rule) | `KVM_SET_CPUID2` with the identity and explicit zero leaves; every KVM `0x4xxxxxxx` entry removed. Other leaves in the range return KVM's Intel out-of-range result | No synthetic processor features, so the hypervisor reports no `0x400000xx` leaves; CPUID intercept results (`always_override`) for the identity leaves, while the explicit zero leaves already read zero in VP 0's view and get none (see the next row); preflight reads every entry back and requires VP 0 to read zero at six sentinel leaves (`0x40000006`, `0x40000081`, `0x400000ff`, `0x40000100`, `0x40000200`, and `0x4000ff00`) | CPUID exits for the six identity leaves, served from OpenVMM's table; with synthetic features off, WHP returns zero natively for `0x40000006..=0x400000ff` and the bases from `0x40000100`, which preflight asserts |
| Profile CPUID and time bits | `KVM_SET_CPUID2` with the effective CPUID. KVM 6.3 and newer hide invariant TSC while KVM's own `HV_X64_MSR_TSC_INVARIANT_CONTROL` is 0, so the backend mirrors the guest-visible value, which core saves with the VM, into it with a host-initiated `KVM_SET_MSRS` after every accepted guest write and before a VP runs after a restore or reset; without it the guest loses `constant_tsc` and `nonstop_tsc` and boots about 160 ms slower | Processor feature banks derived from the profile (`hv_banks`), plus a CPUID intercept result (`always_override`, subleaf-specific where the entry has a subleaf) for every partition-wide entry of the effective CPUID that VP 0's own view does not already present under its mask. Once VP 0 exists, one bulk `HvCallGetVpCpuidValues` reads every partition-wide entry before any result is registered, and a failed bulk read registers every entry. That leaves 11 of 61 entries on the bare-metal host (the hypervisor bit, one leaf-2 descriptor byte, ARAT, the six identity leaves, and two brand leaves) and 12 of 65 on the nested Azure host (the same plus invariant TSC), at every vCPU count, because VP 0's own view already presents leaf 4's shared L3; registering them, the read included, takes 63 to 71 µs and about 90 µs, against 200 and about 300 µs for every entry. Profile masks never cover runtime-owned or VM-owned bits, so a pinned value that VP 0 presents at reset it presents always. Reserved entries pass through from the hypervisor's guest view, which reads zero at every one with no result registered, and verification requires every candidate to read zero (`E_CPU_UNLISTED`). Per-VP fields cannot be left to the hypervisor: it does not implement `0xB` for these partitions and reads `EDX` as 0 on every VP, so Linux would log `APIC ID mismatch` (`G_APIC_ID_MISMATCH`). The `0xB` and `0x1F` entries are therefore per-VP intercept results with the VP's x2APIC ID in `EDX`, registered as each VP is created: the hypervisor refuses a per-VP result for a VP that does not exist yet (`InvalidVpIndex`). Leaf 1's initial APIC ID is the hypervisor's own, which is correct | Processor feature banks derived from the profile (`hv_banks`), and every effective-CPUID entry outside the hypervisor range as a `CpuidResultList2` result (the banks cannot express the hypervisor bit, ARAT, or, on Azure, invariant TSC); reserved entries pass through from WHP's guest view, which verification requires to be zero (`E_CPU_UNLISTED`). CPUID exits only for the identity leaves and the leaves with per-VP APIC fields (`1`, `0xB`, and, where listed, `0x1F` and `0x8000001E`), 8 exits for every v1 profile: each exit-list entry adds about 25 µs to partition setup, and the legacy 269-entry list cost about 7 ms per restore. The exit handler presents the effective CPUID's `0xB` terminator, with the VP's x2APIC ID, instead of zeroing subleaves from 2 |
| Identity MSRs routed to OpenVMM | `KVM_CAP_X86_USER_SPACE_MSR` (`UNKNOWN`, `FILTER`) and `KVM_X86_SET_MSR_FILTER` denying reads and writes of `0x40000000..=0x400001ff`; the filter takes precedence over KVM's in-kernel Hyper-V MSRs. A Linux boot takes four MSR exits, all on the BSP, at any vCPU count | MSR-index intercepts (`HV_INTERCEPT_TYPE_X64_MSR_INDEX`, `READ_WRITE`) for `0x40000002`, `0x40000022`, `0x40000023`, and `0x40000118`. The hypervisor itself raises #GP for writes to the three read-only MSRs and for every other MSR in the range. Native synthetic MSRs are forbidden: they pre-empt the intercepts, and the native `HV_X64_MSR_TSC_FREQUENCY` returns the destination's rate instead of `F`. Verified on hypervisor builds 26100.30000 (bare metal) and 26100.9444 (Azure) | `X64MsrExitBitmap` with `UnhandledMsrs` (capability `0x3f` on every host) and the offloaded APIC, no synthetic features and no `hv1_emulator`: the identity MSRs exit to OpenVMM, which raises #GP for every other MSR in the range |
| Native rate `F_d` | `KVM_GET_TSC_KHZ` × 1000 on VP 0 (1 kHz granularity) | `ProcessorClockFrequency` partition property | `WHvCapabilityCodeProcessorClockFrequency` |
| No TSC scaling | Never `KVM_SET_TSC_KHZ`; every vCPU reports the host rate | No frequency override | No `ProcessorClockFrequency` partition property; the 1 GHz request is removed |
| LAPIC rate `L` | In-kernel LAPIC at 1 GHz; `KVM_CAP_X86_APIC_BUS_CYCLES_NS` never set | The hypervisor's LAPIC at 200 MHz | Offloaded APIC at its fixed 200 MHz, verified at preflight (setting `InterruptClockFrequency` is not supported); the emulated APIC is not used |
| Pending LAPIC vector after a LAPIC state write | Nothing more: `KVM_SET_LAPIC` raises `KVM_REQ_EVENT`, which wakes a halted vCPU for a deliverable vector | Writing the LAPIC state does not wake a halted VP, so the backend asserts the highest pending edge-triggered vector (16 and up) to the VP's own APIC through the hypervisor's interrupt path after the write, and again when the VP next runs; level-triggered vectors stay with the IOAPIC | Writing the offloaded LAPIC state does not reliably wake a halted VP, so the backend asserts the highest pending edge-triggered vector with `WHvRequestInterrupt` to the VP's own APIC ID after the write, and again before the VP next runs |
| TSC-deadline and `TSC_ADJUST` hidden; both MSRs raise #GP | CPUID bits cleared; the MSR filter also denies `IA32_TSC_ADJUST` (`0x3b`) and `IA32_TSC_DEADLINE` (`0x6e0`), which KVM would otherwise serve (reading 0 and ignoring writes to `0x6e0`), and OpenVMM raises #GP | Feature-bank bits `tsc_deadline_tmr_support`, `tsc_adjust_support`, and `a_count_m_count_support` cleared, and CPUID bits cleared; the hypervisor then raises #GP for reads and writes of both MSRs on every CPU | Feature-bank bits `TscDeadlineTmr`, `TscAdjust`, and `ACountMCount` cleared, and CPUID bits cleared; both MSRs exit to OpenVMM, which raises #GP on every CPU |
| Invariant TSC exposed | CPUID bit. KVM hides it from a guest with `"Hv#1"` until KVM's own `HV_X64_MSR_TSC_INVARIANT_CONTROL` is set, so OpenVMM writes 1 to it host-side at vCPU creation | Forced by the profile through a CPUID intercept result: Azure's nested MSHV cannot offer the bit to its guests, although the host OS sees an invariant TSC, and without it each AP pays about 150 ms of calibration. Qualification measures the property instead (`H4`, `H6`) | Set by the CPUID override on every host: on Azure, WHP cannot offer it through the feature banks (bank 1 lacks `TscInvariant`), although the host OS sees an invariant TSC. There the guest TSCs stayed within 60 ns of each other at 8 vCPUs over 60 s and matched the declared rate against host QPC to 0.000 ppm (residual at most 0.07 µs) over 118 s; `H4` and `H6` measure both on every host |
| No paravirtual or synthetic features | No KVM leaves; `KVM_CAP_ENFORCE_PV_FEATURE_CPUID`, so KVM's paravirtual MSRs raise #GP; KVM's in-kernel Hyper-V MSRs are unreachable behind the filter | No synthetic processor features | `--hv` stays rejected for the microVM |
| Capture anchor: VP 0 TSC paired with a host time sample within 100 µs, from at most 64 samples | Host `rdtsc` plus VP 0's `KVM_VCPU_TSC_OFFSET`, bracketed by two host `rdtsc` reads around the host clock reads; up to 16 attempts (0.06 to 0.4 µs) | The tightest of up to 64 brackets `[sample, HvCallGetVpRegisters(VP 0 TSC), sample]`, paired at the bracket midpoint (p50 3.5 µs on bare metal, 6.2 µs on Azure) | The tightest of up to 64 bracketed reads of VP 0's TSC register, paired at the bracket midpoint (2.6 to 6.4 µs on bare metal, 5.4 to 9.7 µs on 8370C runners, 5.8 to 11 µs on 8573C runners) |
| Synchronized TSC set at one host instant | One `KVM_VCPU_TSC_OFFSET` value for every vCPU, `target(t) - (h0 + h1) / 2` from a host clock read `t` bracketed by host `rdtsc` reads `h0` and `h1` (Linux 5.16 or newer); no `IA32_TSC` writes, which Linux 6.6 can discard | Freeze partition time (already frozen since preflight), write the target to every created VP, read back, and clear `TimeFreeze` right after a successful read-back, inside the set rather than at the first VP run (56 to 312 µs for 1 to 8 VPs; equal on 20 of 20 restores) | Suspend partition time, write the target to every VP, read back, and resume with `WHvResumePartitionTime` right after a successful read-back, inside the set. Writing while time runs would skew the VPs by the write latency, about 11 µs per write on bare metal and 25 µs nested; `TscVirtualOffset` is unusable (writes fail) |
| Read-back before any VP runs | Every vCPU's `KVM_VCPU_TSC_OFFSET` equals the written value, and a host `rdtsc` bracket around VP 0's `IA32_TSC` shows no scaling | Every created VP's TSC equals the target while time is frozen | Every VP's TSC equals the target while time is suspended; live reads cannot verify 1 µs (a register read takes 9.5 to 21 µs) |
| Live cross-vCPU skew after release at most 1 µs | Equal offsets: skew is the host's TSC skew, bounded by qualification. Measured at most 63 ns | Measured 0 warps; offsets within 516 ns on dual-socket bare metal and 195 ns on Azure, both bounded by the probe's round trip | Measured at most 70 ns over 60 s on the bare-metal host and on 8370C and 8573C runners |
| VP instantiation | All `C` VPs exist before the set | Every instantiated VP is bound, which creates it, before the set; no VP is created after it | All `C` VPs exist before the set |
| Partition capabilities, so that the identity leaves never enable the Hyper-V emulator or its saved-state elements | Derived from CPUID with the hypervisor range masked: `hv1` and `kvm_clock` are false | Same | Same |
| Unknown MSRs | #GP, including the legacy L2-cache MSR stubs (see [MSRs](#msrs)) | #GP from the hypervisor | #GP, including the legacy L2-cache MSR stubs |
| Removed | `KVM_GET_CLOCK`/`KVM_SET_CLOCK` in the microVM downtime path, kvmclock MSR state, leaf `0x15` synthesis, `KVM_SET_TSC_KHZ`, restore-time `IA32_TSC` writes | BSP-copy TSC alignment, exact-rate equality, leaf `0x15` synthesis | 1 GHz request and fallback, `RestoredTsc` and its RDTSC, RDTSCP, and `IA32_TSC` exits, leaf `0x15` synthesis |

**KVM common offset.** Restoring each VP's `IA32_TSC` is unreliable on KVM.
Linux 6.6 KVM, which the Azure KVM runners run, treats a host `IA32_TSC`
write within about 1 s of the TSC timeline started at vCPU creation as a
synchronization attempt and discards the written value, the first write of a
restore included; every CI shell snapshot is taken within a second of boot.
With the legacy restore path on nested Azure KVM, a restored guest's monotonic
clock advanced 54.7 ms across a 512.5 ms host interval, losing 458 ms. Linux
6.7 applies the heuristic only after a first user-space write
(`user_set_tsc`). `KVM_VCPU_TSC_OFFSET` sets the offset exactly on every
kernel from 5.16, so the backend never writes `IA32_TSC` and core omits the
saved per-VP TSC values from the VP restore.

**WHP decision gate: met.** Suspending partition time, writing the target,
verifying the frozen values, and resuming kept every pair of vCPUs within
70 ns over 60 s on bare metal and on both Azure runner generations, so WHP
never traps RDTSC. The emulated clock it replaces cost about 45 µs per guest
timestamp read on bare metal and 66 to 70 µs on Azure (p50), on every vCPU of
an SMP-restored VM for its lifetime.

## CPU profiles

A CPU profile is the complete guest-visible CPU surface of one vendor and CPU
generation, shared by every backend. In every governed entry the guest sees
only profile values, never host passthrough, except for a fixed, code-defined
set of VMM-owned fields and the fields the time ABI owns; the other entries
are reserved, and verification keeps host features out of them (see
**Effective CPUID**). Profiles are data: derived mechanically by
intersecting host fingerprints (`openvmm --cpu-fingerprint`, the Firecracker
`cpu-template-helper` workflow) per register, first within each backend and
then across backends, reviewed, checked in at
`vmm_core/cpu_profile/profiles/<id>.json`, embedded in the OpenVMM binary, and
immutable once released. A normative change creates a new revision; released
revisions are never deleted, so old snapshots stay restorable. One surface
serves all three backends: MSHV and WHP derive their processor feature banks
from the profile's CPUID (`cpu_profile::hv_banks`) instead of from the host's,
and each backend verifies at partition creation that it supports the profile.
Every backend programs the whole surface explicitly and never relies on
hypervisor defaults; WHP's default processor features, for example, omit
SPEC_CTRL, IBPB, STIBP, and SSBD on Skylake and PSFD on Ice Lake. With
explicit banks, Linux in a bare-metal Skylake-SP WHP guest enables IBRS,
IBPB, SSBD, and CPU buffer clearing. On Azure, the L0 hypervisor withholds
the SPEC_CTRL family and MD_CLEAR from every backend on both generations
(each reports `CPUID.7.0:EDX` = `0x20000010`), and the Ice Lake-SP and
Emerald Rapids profiles pin that value.
A shared profile gives the same guest behavior on every backend; it does not
make snapshots portable, because cross-backend restore is rejected
(`E_BACKEND_MISMATCH`). The `IA32_ARCH_CAPABILITIES` bits outside the pinned
mask are the backend's, because MSHV and WHP can neither set nor read them
back. Measured, they differ only in `PSCHANGE_MC_NO` (bit 6) on Skylake-SP,
where KVM and WHP present 1 and MSHV presents 0 on the same hardware. Linux
uses that bit only for `X86_BUG_ITLB_MULTIHIT`, which matters only to KVM
inside the guest, so it changes no mitigation of an NVX guest, which has no
VMX. Only the sysfs `itlb_multihit` line differs.

**Format.** Schema `openvmm-cpu-profile/v1`. The canonical encoding is
compact canonical JSON (object keys sorted by their UTF-8 bytes, no
whitespace, numbers as hexadecimal strings); the profile digest is the
SHA-256 of that encoding, and decoding accepts only canonical bytes, so a
profile has exactly one encoding and one digest. Pinned files use the same
document in pretty form.

| Field | Content |
| --- | --- |
| `schema` | `openvmm-cpu-profile/v1` |
| `id` | `<vendor>.<generation>.v<revision>`, each component `[a-z0-9-]+`, for example `intel.icelake-sp.v1` |
| `description` | Free text |
| `vendor` | The 12-byte CPUID vendor string |
| `generation` | The generation `name` (`skylake-sp`, `icelake-sp`, or `emeraldrapids`) and its `cpus`: `(family, model, stepping range)` display signatures; stepping ranges separate model 85's Skylake-SP (0 to 4), Cascade Lake, and Cooper Lake |
| `cpuid` | A dense table of every leaf and subleaf in `[0, max basic]` and `[0x80000000, max extended]` with a value and a mask per register; mask bit 1 pins the value, mask bit 0 marks a VMM-owned or runtime-owned bit |
| `xcr0`, `xss`, `xsave_components` | The XSAVE features the guest may enable, and the size, offset, and flags of every enabled component |
| `physical_address_width` | The guest physical address width |
| `msrs` | Pinned MSR values with masks; v1 pins `IA32_ARCH_CAPABILITIES` only, under the mask of the bits that Hyper-V's feature banks derive plus `ITS_NO`. KVM presents it as a feature MSR; MSHV and WHP present it through their banks' `*_NO` bits, because neither has a register to set or read it back |
| `provenance` | The derivation method and the source fingerprints' backends, host counts, and surface digests (informative) |

VMM-owned bits are a code table, not profile data: `CPUID.1:EBX[31:16]`
(logical count and initial APIC ID), x2APIC (`CPUID.1:ECX[21]`), the core
and cache-sharing counts in `CPUID.4:EAX[31:14]`, and the topology leaves
`0xB` and `0x1F`, which profiles omit. The machine has one socket and one
core per vCPU ([configuration boundary](configuration-boundary.md)), so in
`CPUID.4` every cache reports the vCPU count less one as the cores per
socket (`EAX[31:26]`), and the L3 cache reports it as the logical processors
sharing the cache (`EAX[25:14]`) too; the L1 and L2 caches report 0 there,
one vCPU each. Runtime-owned bits mirror control state: `OSXSAVE`, `OSPKE`,
and the XSAVE sizes for the current XCR0 and XSS.
The time ABI owns `0x40000000..=0x4fffffff`, `0x15`, and `0x16`, and every
profile pins the [CPU time bits](#cpu-time-bits). Invariant TSC and ARAT are
pinned set and, with the hypervisor bit, exempt from host-support
verification, because a backend's fingerprint can lack them: WHP's feature
banks cannot express ARAT, and on Azure, WHP and nested MSHV cannot offer
invariant TSC to guests through their feature banks although the host OS
sees it. Host qualification measures them instead. Profiles also pin policy zeros
(VMX and SVM, SGX, PT, RDT, PCONFIG, and the other features listed by the
profiles' derivation policy). The effective guest CPUID is a pure function of
the profile, the VM topology, and the time ABI's identity leaves.

**Effective CPUID.** The effective CPUID
lists exactly the governed leaves: every leaf and subleaf of the profile's
`cpuid` table; the topology leaves `0xB` and `0x1F`, when they are within
the maximum basic leaf, at the subleaves the VM's topology defines and the
subleaf that terminates them (an invalid level: `EAX` and `EBX` zero,
`ECX[7:0]` its own number, `ECX[15:8]` zero, and the x2APIC ID in `EDX`),
which Linux reads to end its enumeration; and the identity leaves
`0x40000000..=0x40000005` with the explicit zero leaves. Each
entry holds the four registers and their masks; the per-VP fields hold VP 0's
APIC identity, and the runtime-owned bits have mask 0. OpenVMM computes it,
records it as the manifest's packed binary record, and recomputes it on
restore, so the record never depends on how a backend enumerates or caches
CPUID, and one profile and topology give the same record on every backend.
The record also fixes OpenVMM's own leaves and VMM-owned bits, so changing
how OpenVMM computes them breaks snapshot compatibility: snapshots that
earlier builds captured fail restore with `E_CPU_SURFACE` and must be
recaptured, and the release notes say so.
Its canonical JSON form (`openvmm-effective-cpuid/v1`) serves tools, never
the start paths. At preflight, before any VP runs, each backend reports
VP 0's CPUID for the governed leaves (KVM from the `KVM_SET_CPUID2` table it
programmed, MSHV by reading VP 0, WHP from VP 0's register view), reading an
entry without a subleaf at subleaf 0, and OpenVMM compares the report with
the effective CPUID under its masks. MSHV and WHP also report VP 0's view at
the reserved entries the host's CPUID enumerates (verification step 6). MSHV
reads every reported entry from the hypervisor, the explicit zero leaves of
the identity range included, and preflight also requires VP 0 to read zero
at six sentinel leaves of the range (`0x40000006`, `0x40000081`,
`0x400000ff`, `0x40000100`, `0x40000200`, and `0x4000ff00`;
`E_IDENTITY_ROUTING`). It batches the reads into rep
`HvCallGetVpCpuidValues` calls of up to 128 entries, with the VP's XFEM and
XSS and the registered results applied, and reads one entry per call, with a
warning, if a batched call fails.

The effective CPUID is the single source of the guest's CPUID. Core passes it
to the backend as `TimeAbiConfig::cpuid`, with the per-VP fields unmasked,
and the backend presents every entry with each VP's own APIC identity (see
[Backend obligations](#backend-obligations)). Banks and pinned MSRs come
from the profile itself, which the backend looks up by
`TimeAbiConfig::cpu_profile`. The governed entries are every entry an
architectural enumeration reaches.

The other entries, which only probing past an advertised maximum reaches, are
reserved: their values are implementation-defined, and guests must not rely
on them. They are:

- a leaf above the maximum basic or extended leaf;
- a subleaf past its leaf's reported maximum or terminator;
- a `0xD` subleaf of a component that is not enabled.

KVM answers them from the effective CPUID: zero, Intel's out-of-range result
(the highest basic leaf's) above the maximum basic leaf, or, past the last
topology level, an invalid level with the x2APIC ID. MSHV and WHP pass them
through from their own guest views. Both read zero at every reserved entry
their sweeps reach (below), so neither programs zero results there, which
would cost about 5 µs per entry at partition setup: about 35 µs on WHP for
the 6 or 7 candidates of a fleet host, and 49 to 63 µs on MSHV for its former
13 to 15. Verification keeps host features out of every entry passed
through: profile verification fails a host whose hypervisor presents a
non-zero entry outside the profile's tables (`E_CPU_UNLISTED`, verification
step 6), checked on VP 0's view at every start. Under the time ABI, the MSHV
and WHP sweeps of VP 0 read zero there, including Intel PT's `0x14.1`, which
the Skylake-SP roots show as non-zero:

- WHP's hardware sweep covers subleaves 0 to 63 of every indexed leaf and four
  leaves past each maximum, on all three host types.
- MSHV's sweep, run with no result registered for any reserved entry, covers
  the host-enumerated reserved entries (15 on the bare-metal host and 13 on
  the nested Azure one) plus 2,755 and 3,261 probes: the same near range as
  WHP's, and far probes where the hypervisor answers by itself (subleaves 64
  to 255 of every indexed leaf, the leaves past the near range up to `0xff`
  and `0x800000ff`, the hypervisor range up to `0x400001ff`, and nine distant
  leaves). The hypervisor builds were 10.0.26100.30000 and, nested,
  10.0.26100.9444.

The fleet fingerprints cover only the entries that a guest's own enumeration
reaches. The `E_CPU_SURFACE` comparison and the snapshot record cover only
the governed entries.

Informational fields:

- The brand string (`0x80000002..=0x80000004`) is generic per generation:
  `Intel(R) Xeon(R) Processor (<name>)` with the generation's display name
  (`Skylake-SP`, `Ice Lake-SP`, or `Emerald Rapids`), zero-padded, without a
  frequency. Every host of a generation presents it whatever its SKU, and
  derivation needs no common brand across the source hosts.
- The cache leaves (`0x2` and `0x4`, except their VMM-owned counts) come from
  the reference hosts; other SKUs of the generation differ, and verification
  does not compare them. Address widths are checked as limits.
- `IA32_UCODE_REV` is not pinned: Linux disables its microcode loader and
  skips its microcode checks under a hypervisor, so the value only reaches
  `/proc/cpuinfo`, and it is outside the effective-CPUID record and every
  restore check.

Mitigation-relevant bits follow the hardware. A profile sets `ITS_NO`
(`IA32_ARCH_CAPABILITIES` bit 62) or `BHI_CTRL` (`CPUID.7.2:EDX[4]`) only if
every CPU of its generation is immune or has the control and every backend
can present the value; MSHV cannot intercept `IA32_ARCH_CAPABILITIES`. The
v1 profiles clear both: MSHV and WHP cannot present `ITS_NO`, and no host
available to the fleet exposes `BHI_CTRL`. MSHV cannot present `BHI_CTRL` at
all: even on 8573C runners its feature banks have neither `bhi_dis` nor
`bhi_no`, and leaf 7 reports a maximum subleaf of 0. Without `ITS_NO`, the
hypervisor bit makes Linux enable its ITS mitigation. NVX keeps every such
mitigation enabled, and the kernel must not leave its thunk pages writable
and executable (boot check `K1`).

**Selection.** A microVM always has a profile. A cold boot uses
`--cpu-profile <id>`, or `auto` (the default), which selects the highest
revision of the single profile whose generation covers the host's vendor,
family, model, and stepping as the VMM's host OS sees them (the L1 view on
Azure). No match, or matches in more than one generation, is
`E_PROFILE_HOST_UNKNOWN`, naming the host's signature and the available IDs;
host CPUID passthrough does not exist. A restore always uses the profile
recorded in the snapshot; an explicit `--cpu-profile` must name the same
profile (`E_PROFILE_UNKNOWN`).

**Verification.** At partition creation, for cold boot and restore, OpenVMM
reports every violation at once, naming the leaf, subleaf, register, and bit:

1. The profile is valid: schema, canonical encoding, density, the VMM-owned
   and runtime-owned mask tables, and the [CPU time bits](#cpu-time-bits)
   (`E_PROFILE_TIME_BITS`; a catalog profile that fails is a build defect
   caught by unit tests).
2. The host's vendor, family, model, and stepping are in the profile's
   generation (`E_CPU_GENERATION`).
3. The backend supports the profile (`E_PROFILE_UNSUPPORTED`): every set
   feature bit is a supported bit, every limit (maximum leaves, address
   widths) is within the backend's, every enabled XSAVE component has the
   same size, offset, and flags, XCR0 and XSS are subsets, and every pinned
   MSR value can be presented (an `ARCH_CAPABILITIES` immunity the host lacks
   is a violation). The time policy bits are exempt. Each backend derives
   its supported surface from capability queries and the host's CPUID,
   without a probe partition, which would add 2.6 to 5.5 ms per cold boot
   and restore on WHP; the full fingerprint stays in `--cpu-fingerprint`.
4. On restore only: this OpenVMM pins a profile with the same ID and digest,
   and the recorded document is its canonical encoding
   (`E_PROFILE_UNKNOWN`, `E_PROFILE_DIGEST`), and the recomputed effective
   CPUID equals the recorded one (`E_CPU_SURFACE`). Both are comparisons
   with precomputed or recomputed values; nothing is hashed or decoded. The
   effective-CPUID record replaces the exact-equality CPU contract.
5. The backend presents the effective CPUID: VP 0's CPUID for every governed
   leaf equals it under its masks (`E_CPU_SURFACE`).
6. On MSHV and WHP, which pass reserved entries through, the hypervisor
   presents no non-zero CPUID entry outside the profile's tables, apart from
   the identity range and the topology leaves (`E_CPU_UNLISTED`).
   - `--cpu-fingerprint` checks the probe partition's guest view, at the
     entries its own enumeration reaches.
   - At every cold boot and restore, step 5's check covers it on VP 0's
     view. The candidates are the entries that the host's CPUID enumerates
     outside the profile's tables (`cpu_profile::unlisted_cpuid_candidates`),
     each read at subleaf 0 if subleaf-independent. The backend reads them
     with the governed leaves: 7 more entries on the Skylake-SP hosts
     (`0xF.1`, `0x10.1` to `0x10.3`, `0x12.1`, `0x12.2`, and `0x14.1`) and 6
     on the Azure 8370C and 8573C runners, whose roots report no Intel PT.
     The supported surface of step 3 cannot serve: it derives from the
     root's CPUID, which shows values that a guest does not see there, so
     it would fail sound hosts.
     On the Skylake-SP hosts, the MSHV and WHP roots read Intel PT's
     `0x14.1` as non-zero. A guest's own enumeration stops at `0x14.0`,
     which reads zero, so only the host-derived candidates reach `0x14.1`.
   - KVM answers reserved entries from the effective CPUID itself, so it
     needs no check.

The catalog has three profiles, derived from the fingerprints of three
bare-metal hosts (one per backend) and fifteen Azure hosts:

| ID | Generation | Hosts | Source backends |
| --- | --- | --- | --- |
| `intel.skylake-sp.v1` | 6/85, steppings 0 to 4 | The bare-metal hosts | KVM, MSHV, WHP |
| `intel.icelake-sp.v1` | 6/106 | Xeon Platinum 8370C runners | KVM, MSHV, WHP |
| `intel.emeraldrapids.v1` | 6/207 | Xeon Platinum 8573C runners | MSHV, WHP (no KVM host exists) |

Sharing costs nothing on Ice Lake-SP and Emerald Rapids, where every backend
offers the same features. On Skylake-SP, used only by the bare-metal
development hosts, the shared profile drops what one backend alone offers:
PKU, UMIP, and FDP_EXCPTN_ONLY (KVM only), FLUSH_L1D (not on WHP), and the
AMD-alias speculation bits in `0x80000008` EBX (KVM only; Intel guests use
the `7.0` EDX equivalents, which every backend presents). Without FLUSH_L1D,
and with `FB_CLEAR` clear because KVM's feature MSR does not offer it on these
hosts (KVM would accept it in a host-initiated write, but v1 pins only what
every backend's surface offers),
Linux cannot confirm the microcode that makes VERW clear the fill buffers.
It therefore reports MMIO Stale Data as "Vulnerable: Clear CPU buffers
attempted, no microcode" on every backend. Before the profiles, KVM
reported "Mitigation: Clear CPU buffers" through FLUSH_L1D, and MSHV
through `FB_CLEAR`. The guest still clears the CPU buffers with VERW at the
same points (`MD_CLEAR`), as its MDS and TAA status, "Mitigation: Clear CPU
buffers", shows, and the host has the microcode, so only the reported status
differs.

No profile pins the legacy P6 L2-cache MSRs; they raise #GP on every
backend (see [MSRs](#msrs)).

## TSC rate policy and LAPIC rate rule

**TSC rate.** At restore, with integer arithmetic in at least 128 bits:

```text
accept  iff  |F_d - F_s| * 1_000_000  <=  250 * F_s
```

The boundary is accepted. Beyond it, restore is rejected
(`E_TSC_RATE_TOLERANCE`), reporting both rates and the deviation. The
tolerance is recorded in the manifest as `tsc_tolerance_ppm = 250`; any other
recorded value is rejected. Within the tolerance:

- the TSC is never scaled, and RDTSC is never trapped;
- MSR `0x40000022` keeps returning `F_s`, so the guest's `tsc_khz` is
  unchanged;
- guest clocks run fast or slow by at most the deviation (measured hosts
  differ by at most 1 ppm), and the restore packet reports the deviation so
  the guest pre-compensates it (see
  [Wall-clock discipline](#wall-clock-discipline)).

Every rate is also checked for plausibility, `500 MHz <= F <= 10 GHz`
(`E_TSC_RATE_IMPLAUSIBLE`). A backend that cannot report its native rate is
rejected at cold boot, capture, and restore alike (`E_TSC_RATE_UNAVAILABLE`);
there is no command-line or calibration fallback.

**Rate deviation.** The restore packet carries the deviation in the unit of
`adjtimex` frequency, ppm scaled by 2^16, rounded to nearest:

```text
rate_deviation = round((F_d - F_s) * 1_000_000 * 65_536 / F_s)
```

Within the tolerance its magnitude is at most 16,384,000.

**LAPIC rate.** `L` must equal the backend constant (1 GHz on KVM, 200 MHz on
MSHV and WHP), and on restore `L_d` must equal `L_s` exactly
(`E_LAPIC_RATE_MISMATCH`). A counting-mode one-shot timer advances over a
downtime `D` by exactly

```text
ticks = floor(D * L / 1_000_000_000 / divide)
```

where `divide` is the divide-configuration value (1 to 128). If `ticks` is at
least the current count, the count becomes 0 and the timer interrupt is queued
in the restored LAPIC state unless the LVT is masked; otherwise the current
count decreases by `ticks`. A vector queued this way, or one pending in the
restored state, must reach its VP when the VP is released, even if the VP is
halted. MSHV and WHP do not wake a halted VP for an IRR bit written with its
LAPIC state, so their backends assert the vector again (see
[Backend obligations](#backend-obligations)). Without that, on busy 8-vCPU
guests, 29 of 60 time ABI restores on bare-metal MSHV and 5 of 12 on
bare-metal WHP left a CPU halted with its timer vector pending until another
interrupt woke it, about 2 s later or never; with it, 0 of 300 and 0 of 12.
A periodic or TSC-deadline LAPIC timer is never valid: capture and restore
reject one that is armed (`E_LAPIC_PERIODIC`, `E_LAPIC_TSC_DEADLINE`).

## Cross-vCPU skew bound

At every guest-observable instant, the TSCs of any two online VPs differ by
at most 1 µs, that is `F_s / 1,000,000` cycles. Linux no longer checks this
(`TSC_RELIABLE`), so it is enforced at four points:

1. **VMM synchronized set.** Restore writes one target to every instantiated
   VP at one host instant and verifies it by read-back before any VP runs
   (`E_TSC_SYNC_READBACK`). The VMM introduces no skew of its own.
2. **Host qualification.** `nvx.py doctor` runs a host TSC skew probe (`H5`)
   and the guest warp probe on an idle-inducing schedule (`H6`); a host above
   1 µs is not qualified.
3. **CI warp probe.** Every CI microVM boot and restore scenario, and the
   conformance, restore-matrix, and soak runs, execute the guest warp probe
   on the CI schedule (see [Warp schedules](#warp-schedules-h6-and-ci))
   after boot and after every restore and fail above 1 µs.
4. **Backend live skew.** Each backend shows that live skew stays within the
   bound after release. The spikes measured at most 63 ns of ping-pong
   offset and 7.2 ns of backward step on KVM; on MSHV, zero warps and an
   equal read-back on 20 of 20 restores, with offsets of at most 516 ns on
   dual-socket bare metal (bounded by the probe's round trip) and 195 ns on
   Azure; and at most 70 ns on WHP.

The guest warp probe (`nvx-time-probe warp`, built from
`guest/common/nvx-time-probe.c` and installed as `/sbin/nvx-time-probe`)
runs two tests on every pair of online CPUs, each for at least 100 ms per
pair:

- `max_backward_ns`: the largest backward TSC step observed when the two CPUs
  alternately read the TSC under a shared spinlock (the Linux
  `check_tsc_warp` method); a lower bound on skew; and
- `max_abs_offset_ns`: the largest pairwise offset estimated by ping-pong
  rounds, `t2 - (t1 + t3) / 2` at the round with the minimum round trip.

Both must be at most 1,000 ns, and no pair may stall for more than 2 s.
Cycle values convert to nanoseconds with `F`. Each offset estimate is
uncertain by half its pair's minimum round trip, which the probe reports
(`max_uncertainty_ns`); a verdict counts only if every pair's uncertainty is
below the bound (`conclusive=1`), and CI fails an inconclusive one. Round
trips are about 0.25 µs within a socket and 1 µs across sockets, so 100 ms
per pair gives more than 10,000 rounds and an uncertainty of at most about
0.5 µs on dual-socket hosts.

## Downtime semantics and source selection

Guest monotonic time advances by the downtime `D`, the host time elapsed
between the capture anchor and the restore anchor, plus at most the restore
latency `R` defined below. The guest observes no other discontinuity between
the snapshot `out` and the first restored instruction.

Capture records, at the capture anchor:

- `T_c`, the guest TSC of VP 0;
- host UTC in nanoseconds since the Unix epoch;
- host monotonic time in nanoseconds: `CLOCK_BOOTTIME` on Linux, or
  `QueryInterruptTimePrecise` × 100 on Windows (both include host suspend);
- the host identity: `/etc/machine-id` on Linux, or the `MachineGuid`
  registry value on Windows, as 16 bytes; and
- the host boot identity: `/proc/sys/kernel/random/boot_id` on Linux, or the
  `PrefetchParameters\BootId` registry value on Windows (zero-extended), as
  16 bytes.

A host that cannot provide all of these rejects capture and restore
(`E_HOST_IDENTITY`). Restore takes the same sample at the restore anchor and
selects exactly one source:

| Condition | Source | `D` |
| --- | --- | --- |
| Same host identity, same boot identity, and same host clock kind | Host monotonic | `monotonic_now - monotonic_capture` |
| Anything else | UTC | `utc_now - utc_capture` |

`D` must satisfy `0 <= D <= 30 days` (2,592,000 s); otherwise restore is
rejected (`E_DOWNTIME_NEGATIVE`, `E_DOWNTIME_EXCESSIVE`). On the monotonic
path, a UTC delta that differs from `D` by more than 1 s is logged as a host
wall-clock step and does not change `D`. The packet reports the source, so
the guest and tests can tell the two paths apart.

The restore anchor is the instant of the synchronized TSC set, and the guest
TSC runs from that instant on every backend: KVM never stops it, and MSHV and
WHP resume partition time right after a successful read-back, inside the
synchronized set (see step 13 of the
[restore algorithm](#restore-algorithm)). No backend defers the resume to the
first VP run. The guest monotonic advance across a restore therefore lies in
`[D, D + R]`, up
to the two anchors' pairing errors (each at most 100 µs), where `R` is the
time from the restore anchor to the first restored instruction; it falls
short of `D + R` only by the frozen write (at most about 0.5 ms). Wall-clock
repair absorbs `R` and the pairing errors for `CLOCK_REALTIME`. The KVM spike
kept guest `CLOCK_MONOTONIC` within −0.09 to +2.8 ms of the host interval
across restores with 0 s and 30 s of downtime at 1 to 8 vCPUs; its positive
part came from a host sample taken before the per-VP TSC save, which the
paired capture anchor removes. MSHV measured −0.05 to −0.55 ms with the
resume at the read-back, at parity with KVM, against −0.9 to −1.6 ms when the
first VP run thawed time; warps and restore latency were unchanged. The
legacy path, which stopped guest time during restore setup, left guest
`CLOCK_MONOTONIC` about 400 ms behind the host on every restore on
nested Azure KVM with 512 MiB (7 to 12 ms on bare metal); the time ABI stays
within 1.5 ms there.

**Anchor pairing bound.** An anchor that pairs a TSC read with a host time
sample (the capture anchor on every backend, and KVM's restore anchor)
brackets one with the other, and its pairing error is half the bracket. Every
backend uses one bound, 100 µs (`MAX_ANCHOR_PAIRING_NS` in
`virt::time_abi`): it keeps the tightest of at most 64 samples, never accepts
a looser pair, and otherwise fails with `E_TSC_ANCHOR`. A nested hypercall
round trip on Azure takes about 12 µs, so nested MSHV pairs at a median of
6.2 µs and WHP on 8573C runners at up to 11 µs; a 10 µs bound would leave
about 1.5× margin over the median and fail restores at random. Downtime needs
only millisecond accuracy, so a 100 µs pairing error is negligible.

## Restore algorithm

Restore runs these steps in order. Any failure rejects the restore before a
restored VP runs, with the named code.

Before worker construction:

1. Read the bounded manifest. Its version must be 6 with format magic
   `OPENVMM_SNAPSHOT_V6\0` (`E_SNAPSHOT_VERSION`, which asks for a
   recapture).
2. Validate the time contract and the CPU profile record: `time_abi_version`
   is 1, `tsc_tolerance_ppm` is 250, `F_s` is plausible, `L_s` is a backend
   constant, identities are 16 bytes, `capture_generation < 2^32 - 1`, the
   profile digest is 32 bytes, and the effective CPUID record is whole,
   well-formed entries (`E_MANIFEST_TIME`). Validation computes no digest.
3. Require the destination backend to equal `source_hypervisor`
   (`E_BACKEND_MISMATCH`).
4. Require a pinned profile with the recorded ID and digest, whose canonical
   encoding is the embedded document byte for byte (`E_PROFILE_UNKNOWN`,
   `E_PROFILE_DIGEST`), both compared with the build's precomputed record,
   and the host's CPU generation to be in it (`E_CPU_GENERATION`).
5. Preflight the downtime: sample the host clocks, select the source, and
   check the bounds (`E_DOWNTIME_*`, `E_HOST_IDENTITY`). The authoritative
   `D` is measured again in step 12.
6. Validate the rest of the machine contract as today (topology, devices,
   attachments, command line, blocks). The command line must not contain
   `tsc_early_khz=` or `lapic_timer_hz=` (`E_CMDLINE_CLOCK_TOKEN`).
7. Generate the restore entropy and generation ID, and set
   `g = capture_generation + 1`.

In the worker, with every VP stopped:

8. Create the partition with the profile CPUID, the identity leaves, and the
   time platform declaring `F = F_s` and `L = L_s`. Preflight the backend
   before any VP runs and before any VP state is restored: profile support
   and effective CPUID (`E_PROFILE_UNSUPPORTED`, `E_CPU_SURFACE`,
   `E_CPU_UNLISTED`), identity routing (`E_IDENTITY_ROUTING`), the
   synchronized-set primitive (`E_TSC_SYNC_UNSUPPORTED`), and no scaling
   (`E_TSC_SCALING_ACTIVE`). MSHV's preflight freezes partition time, which
   probes the primitive, and leaves it frozen, as a reset does: steps 9 to 12
   run with it frozen, and at cold boot the first VP run resumes it. It also
   reads VP 0's CPUID at its reset state.
9. Read `F_d` and `L_d` and apply the
   [rate policy](#tsc-rate-policy-and-lapic-rate-rule)
   (`E_TSC_RATE_UNAVAILABLE`, `E_TSC_RATE_IMPLAUSIBLE`,
   `E_TSC_RATE_TOLERANCE`, `E_LAPIC_RATE_UNAVAILABLE`,
   `E_LAPIC_RATE_MISMATCH`).
10. Restore every state unit while stopped: VM time, chipset and virtio
    devices, the time platform (`HV_X64_MSR_TSC_INVARIANT_CONTROL`), the
    partition, and every VP.
11. Freeze the instantiated VP set. Every instantiated VP was bound before
    step 10 restored its state, and MSHV creates a VP only when it is bound,
    so the set is complete; creating a VP after this point is an internal
    error (`E_VP_LATE_CREATION`). The PIT's restore in step 10 already
    rejected a channel 0 counting in a periodic mode (`E_PIT_ACTIVE`).
12. Synchronized TSC set. The backend takes the restore anchor (one host
    instant) and passes its host sample to the orchestrator, which selects
    the downtime source, computes `D` and checks its bounds, and returns

    ```text
    TSC_target = T_c + floor(D * F_s / 1_000_000_000)
    ```

    (`E_TSC_TARGET_OVERFLOW` if it exceeds 64 bits). The backend writes
    `TSC_target` to every instantiated VP at that instant. Per-VP TSC values
    from saved VP state are omitted from the VP restore and never applied. A
    restore anchor that pairs a TSC read with host time (KVM's common offset)
    takes at most 64 samples and fails with `E_TSC_ANCHOR` if no pair is
    within 100 µs; a pairing error shifts every VP's TSC alike, by at most
    that amount. A frozen write needs no such pairing. Core never calls the
    legacy clock entry points (`set_tsc_frequency_hz`,
    `advance_snapshot_time`) on a time ABI partition; KVM rejects both there.
13. Read back: every instantiated VP holds the synchronized value as defined
    by its backend (`E_TSC_SYNC_READBACK`). A backend that froze partition
    time for the set resumes it right after a successful read-back, before
    the synchronized set returns: MSHV clears `TimeFreeze`, and WHP calls
    `WHvResumePartitionTime`; KVM's TSC never stops. Guest time therefore
    runs from the restore anchor on every backend. A failed read-back leaves
    time frozen and fails the restore.
14. Advance every VP's counting-mode LAPIC timer by `D` at `L`, and set every
    VP's LAPIC state again, always after step 12 and even when no timer is
    armed: KVM derives its timer deadline from the guest TSC when the LAPIC
    state is set. The same pass rejects an armed periodic or TSC-deadline
    timer (`E_LAPIC_PERIODIC`, `E_LAPIC_TSC_DEADLINE`) before any VP runs,
    so restore takes no separate pass over the VPs to check them.
15. Advance VM time by `D` and the RTC's UTC by `D` (milliseconds). The PIT
    catches up from its saved VM-time cursor; it is idle. Steps 14 and 15
    touch disjoint state, because no time ABI LAPIC reads VM time (see the
    LAPIC rate row of [Backend obligations](#backend-obligations)), so
    OpenVMM runs them concurrently and waits for both before step 16.
16. Seal the time fields of the restore packet: `D`, its source, the rate
    deviation, `g`, and the test-hook flag.
17. Start the state units, with host input gated when the restore requires an
    acknowledgement; publish the readiness event; release the VPs. Partition
    time has run since step 13, so releasing the VPs does not start it, and
    no backend resumes time at the first VP run.

The guest then repairs its clocks (see
[Snapshot agent](#snapshot-agent)) before acknowledging a gated restore.

**Cold boot.** A cold boot runs the partition steps without a snapshot, in
this order:

1. Select the CPU profile (see [CPU profiles](#cpu-profiles)) and reject
   `tsc_early_khz=` and `lapic_timer_hz=` in the command line
   (`E_CMDLINE_CLOCK_TOKEN`); OpenVMM injects neither.
2. Create the partition and preflight the backend as in step 8, before any
   VP runs.
3. Read `F_d` and `L_d`, check them (`E_TSC_RATE_UNAVAILABLE`,
   `E_TSC_RATE_IMPLAUSIBLE`, `E_LAPIC_RATE_UNAVAILABLE`,
   `E_LAPIC_RATE_MISMATCH`), and declare `F = F_d` and `L = L_d`.
4. Set `g = 0`, start the state units, and release the VPs.

A cold boot writes no TSC: every backend starts the VPs' TSCs together (the
spikes measured at most 63 ns of skew), and skew enforcement points 2 to 4
apply.

## Snapshot manifest

New snapshots use manifest version 6 with format magic
`OPENVMM_SNAPSHOT_V6\0`. Restore accepts exactly version 6; versions 2
through 5 and every other value are rejected with `E_SNAPSHOT_VERSION`, whose
message tells the operator to recapture the snapshot. The legacy version
branches and their format constants are deleted. Version 6 is the format's
only version, so standard-machine disk snapshots use it too; they carry no
machine contract and no time records. The manifest's fields 9 and 10, the
version 2 artifact digests, are retired and never reused.

**Capture.** After every VP has stopped at the snapshot boundary and the
state units are quiesced, capture:

1. asserts that no LAPIC timer is periodic or in TSC-deadline mode and that
   PIT channel 0 is not counting periodically (`E_LAPIC_PERIODIC`,
   `E_LAPIC_TSC_DEADLINE`, `E_PIT_ACTIVE`);
2. takes the capture anchor: VP 0's TSC paired with a host time sample taken
   within 100 µs of it, keeping the tightest of at most 64 samples
   (`E_TSC_ANCHOR`), plus the host identities (`E_HOST_IDENTITY`); and
3. records the declared rates, the CPU profile, the effective CPUID, and the
   process's generation counter. The CPU profile record is the pinned
   profile's precomputed digest and canonical encoding, copied, and the
   effective CPUID is a packed binary record: capture encodes no document and
   computes no digest.

Each of these failures is a rollback-safe capture failure.

No backend stops guest time for a capture: the guest TSC keeps running while
the VPs are stopped. So a capture that rolls back leaves the guest's clocks
continuous, and the guest sees the quiesce as elapsed time, as it would a long
preemption. On the bare-metal WHP host, the guest's `CLOCK_MONOTONIC` covered
quiesce windows of 0.39 to 0.65 s within 2.2 ms of the host interval, and
`CLOCK_REALTIME` minus `CLOCK_MONOTONIC` moved by at most 0.14 µs. On the
bare-metal KVM host, `CLOCK_MONOTONIC` stayed within −0.38 to +0.40 ms of the
host over requests of 0.2 to 1.1 s.

**Machine contract changes.** `SnapshotMachineContract` retires protobuf
fields 11 (`capture_wall_clock`), 12 (`tsc_frequency_hz`), 13
(`tsc_tolerance_ppm`), 14 (`cpu_contract`), 15 (`cpu_contract_sha256`), 17
(`clock_policy`), and 20 (`apic_frequency_hz`); their numbers are never
reused. It adds two required fields:

| Field | Number | Type |
| --- | --- | --- |
| `time` | 31 | `SnapshotTimeContract` |
| `cpu_profile` | 32 | `SnapshotCpuProfile` |

`SnapshotTimeContract` (package `openvmm.snapshot`):

| Number | Field | Type | Content |
| --- | --- | --- | --- |
| 1 | `time_abi_version` | `u32` | 1 |
| 2 | `tsc_frequency_hz` | `u64` | `F_s`, the declared rate |
| 3 | `tsc_tolerance_ppm` | `u32` | 250 |
| 4 | `apic_frequency_hz` | `u64` | `L_s` |
| 5 | `capture_tsc` | `u64` | `T_c` |
| 6 | `capture_utc_ns` | `u64` | Host UTC at the capture anchor, nanoseconds since the Unix epoch |
| 7 | `capture_monotonic_ns` | `u64` | Host monotonic time at the capture anchor |
| 8 | `host_clock` | `string` | `linux-boottime` or `windows-interrupt-time` |
| 9 | `host_id` | `bytes` | 16-byte host identity |
| 10 | `host_boot_id` | `bytes` | 16-byte host boot identity |
| 11 | `capture_generation` | `u32` | `g` of the captured process |

`SnapshotCpuProfile` (package `openvmm.snapshot`):

| Number | Field | Type | Content |
| --- | --- | --- | --- |
| 1 | `id` | `string` | Profile ID |
| 2 | `sha256` | `bytes` | Profile digest, 32 bytes: the pinned profile's precomputed digest |
| 3 | `profile` | `bytes` | The pinned profile's canonical encoding, at most 1 MiB |
| 4 | `effective_cpuid` | `bytes` | The [effective CPUID](#cpu-profiles), 1 to 1,024 entries in its order (by leaf, then subleaf). Each entry is 44 bytes: eleven little-endian `u32` values, namely the leaf, then 1 and the subleaf (or 0 and 0 for an entry that applies to every subleaf), then the four registers `EAX` to `EDX`, then their four masks |
| 6 | `capture_cpu_signature` | `u32` | CPUID.1:EAX of the capture host, for diagnostics |

Field 5, a digest of field 4's earlier canonical JSON form, is retired and
never reused. Restore checks the record by comparison: the digest and the
document against the build's pinned record, and the effective CPUID against
its recomputation, encoded the same way.

Other rules:

- The effective command line contains no `tsc_early_khz=` or
  `lapic_timer_hz=` token, and platform-tier validation no longer requires
  one (`E_CMDLINE_CLOCK_TOKEN`).
- The state-unit inventory gains the time platform unit, `time-abi`, which
  saves `HV_X64_MSR_TSC_INVARIANT_CONTROL`.
- `source_hypervisor` keeps its meaning and must match exactly.
- Nothing in the manifest depends on the capture host's native TSC rate
  except `F_s`, which is the lineage's declared rate.

## Restore packet v4 and the time-sample selector

Every byte the guest reads from portb is a port exit, so the time ABI keeps
records small and lets the guest read four bytes per exit.

### Portb registers

| Port | Access | Time ABI v1 behavior |
| ---: | --- | --- |
| `0xe9` | Read | With the restore packet or generation ID selected, a 1-, 2-, or 4-byte read returns that many bytes of the selected record, little-endian, zero-filled past its end. Console input reads are unchanged: one byte per read. |
| `0xea` | Read | Status bits 0 to 5 are unchanged. Bit 6 (`0x40`) is always set: the time-sample window exists. Bit 7 is zero. |
| `0xea` | Write | `0xa5` selects the restore packet and `0xa6` the generation ID, as today. `0xa7` latches a fresh time sample into the window at `0xeb` and does not change the `0xe9` selection. |
| `0xeb` | Read | A 1-, 2-, or 4-byte read returns the next bytes of the latched time sample, zero-filled past its end and when no sample is latched. |

Port `0xeb` belongs to the portb device, whose range becomes
`0xe9..=0xeb`. The time window is separate from `0xe9` because the console
driver polls `0xea` and reads `0xe9` from a timer thread; a sample on the
console stream could interleave with input bytes. The window is not saved
state: restore empties it. Guest users of selectors `0xa5`, `0xa6`, and
`0xa7` serialize their transactions with an exclusive `flock` on
`/run/nvx/portb.lock`. The snapshot agent holds it from its capture request
through repair step 8, and a discipline poll holds it from its first sample
to the published state, so a capture may wait for an in-flight poll, which
is bounded by three samples and two `adjtimex` calls.

### Restore packet

OpenVMM exposes a restore packet on every microVM restore, for every tier and
for untiered snapshots; status bit 1 advertises it. Version 4 replaces
versions 1 through 3, which are no longer produced or accepted.

```text
offset size field
     0    3 magic "OVR"
     3    1 version = 4
     4    1 flags
     5    1 online_vp_count      0 = no processor target, else 1, 2, 4, or 8
     6    1 memory_range_count   0 unless MEMORY_TARGET
     7    1 reserved = 0
     8    4 generation           u32, little-endian, g >= 1
    12    4 rate_deviation       i32, little-endian, ppm scaled by 2^16
    16    8 downtime_ns          u64, little-endian, D
    24    8 utc_ns               u64, little-endian, host UTC at selection
    32 16*n ranges               n x { u64 gpa_start, u64 length }, LE
 32+16n  64 entropy
```

Flags:

| Bit | Name | Meaning |
| ---: | --- | --- |
| 0 | `DOWNTIME_UTC` | `D` came from the UTC delta; clear means same-boot host monotonic time |
| 1 | `MEMORY_TARGET` | An explicit RAM target was requested; `memory_range_count` is valid and may be 0 |
| 2 | `ACK_REQUIRED` | Host input is gated; the guest must acknowledge through `0x605` after repair |
| 3 | `TEST_HOOKS` | A test hook altered the downtime source, `D`, UTC, or the rate deviation |
| 4–7 | | Zero; the guest rejects a packet with any of them set |

`utc_ns` is latched when the guest first writes selector `0xa5` after the
restore; every other field is sealed before the first restored VP runs (step
16 of the [restore algorithm](#restore-algorithm)). A packet without ranges is
96 bytes, 24 four-byte reads. Status bits 2 to 4 remain and agree with the
packet; the packet is authoritative. `ACK_REQUIRED` replaces the guest's
inference of gating from the tier and targets, so an untiered guest never
writes `0x605` after an ungated restore.

A VM process exposes at most one packet, and reading its last byte consumes
it: status bits 1 to 4 clear and later `0xa5` writes select nothing. A guest
that is captured again in the same process, including after a rejected
capture, therefore never sees the earlier packet. It also ignores a packet
whose `g` equals its recorded `g`.

### Time sample

Writing `0xa7` to `0xea` latches a 16-byte sample into the window at `0xeb`:

```text
offset size field
     0    1 version = 1
     1    1 flags                bit 3 TEST_HOOKS; other bits zero
     2    2 reserved = 0
     4    4 generation           u32, little-endian, g of this VM process
     8    8 utc_ns               u64, little-endian, host UTC at the latch
```

The guest pairs `utc_ns` with its own clock: it reads `CLOCK_REALTIME` as
`t0` immediately before the selector write and as `t1` immediately after it.
The host instant lies in `[t0, t1]`, so the offset is

```text
theta = utc_ns - (t0 + t1) / 2      uncertainty epsilon = (t1 - t0) / 2
```

The same bracket applies to `utc_ns` in the restore packet, around the first
`0xa5` write. A sample costs one write and four reads. OpenVMM cannot observe
a running VP's TSC on every backend, so the guest forms the UTC and TSC pair
itself through this bracket; its `CLOCK_REALTIME` is derived from the TSC.

### Uncertainty bounds

The guest accepts a pairing only if `epsilon` is within a bound, retries
otherwise, and then fails with a stable code:

| Use | Bound | Attempts | When every attempt exceeds the bound |
| --- | --- | --- | --- |
| Restore repair (packet bracket, then time samples) | 1 ms | 3 | `G_REPAIR_SAMPLE`: event and power-off with status 195 |
| Initial synchronization at boot, before shell-ready (time samples) | 1 ms | 3 | `G_CONFORMANCE_C12`: event and power-off with status 193 |
| Discipline poll (time samples) | 50 µs | 3 | `G_SAMPLE_UNCERTAIN`: the poll is skipped and the code is recorded in the state file; never fatal |

`epsilon` is half the round trip of one PMIO write exit plus two
`CLOCK_REALTIME` reads, so it stays far below both bounds on every backend.
Measured from a guest with a release build of OpenVMM (`ea570db7e`) and a
probe that brackets each selector write with `CLOCK_REALTIME` reads, two runs
of 2,000 samples per boot at 1, 2, and 4 or 8 vCPUs, which made no
difference:

| Backend | Host | `epsilon` p50 / p99 | Worst sample |
| --- | --- | --- | --- |
| KVM | Bare metal (Linux 7.0) | 4.0 to 4.1 µs / 4 to 15 µs | 33 µs |
| KVM | Nested Azure 8370C (Linux 6.6) | 3.8 to 4.1 µs / 4 to 9 µs | 75 µs |
| MSHV | Bare metal | 6.3 to 6.4 µs / 6.8 to 7.1 µs | 37 µs |
| MSHV | Nested Azure 8573C | 12.7 to 13.0 µs / 15 to 25 µs | 102 µs |
| WHP | Bare metal | 20.7 to 21.3 µs / 29.4 to 30.5 µs | 52 µs |
| WHP | Nested Azure 8370C | 24.8 to 25.2 µs / 36.5 to 41.9 µs | 794 µs |
| WHP | Nested Azure 8573C | 30.5 to 30.8 µs / 46.0 to 46.4 µs | 251 µs |

The second run of each boot overlaps the console's drain of the first run's
output, and it has the higher KVM p99s. The WHP rows come from the WHP
agent's probe (two runs of 2,000 samples). A sample above the 50 µs
discipline bound is rare except on WHP, whose p99 approaches it; a poll then
retries, and a poll whose three attempts all fail is skipped without harm
(`G_SAMPLE_UNCERTAIN`).

### Generation counter and generation ID

- The generation counter `g` is 0 in a cold-booted process and
  `capture_generation + 1` in a restored one. It orders restores within a
  snapshot lineage; clones of one snapshot share it. The packet and every
  time sample carry it, so a guest observes a restore between two samples as
  a change of `g`.
- The generation ID is unchanged: 16 random bytes, unique to each VM process,
  selected with `0xa6`, and equal to the first 16 entropy bytes when a
  restore packet exists. It distinguishes clones.
- After a restore, the guest requires `g` in the packet to equal its
  recorded `g` plus one, and the generation ID to differ from the recorded
  one. Either mismatch fails repair.

## Guest obligations

The guest's time component (`nvx-time` in `guest/common`) performs the
conformance checks, runs the violation watcher and the wall-clock discipline
as one daemon, executes the snapshot agent's time steps, and reports status
on demand. Every guest mode starts it: init's interactive and test paths,
`nvx_exec`, the sandbox agent, and the managed agent.

Production paths keep the console quiet: the time component prints no
marker or status line, and only a [violation event](#violation-watcher)
reaches the console. Each console byte costs one or two port exits, about 10
to 12 µs on bare-metal KVM and an estimated 20 to 45 µs nested on Azure, so a
110-byte boot marker and a 117-byte restore marker would cost 1 to 5 ms on
the measured paths. State goes to the [state file](#wall-clock-discipline),
and `nvx-time status` prints it on demand.

### Conformance checks and the `NVX-TIME-ABI` marker

Before shell-ready, init does only what boot correctness needs. Right after
mounting `/proc`, `/sys`, and `/dev`, it takes a time sample within the
[uncertainty bound](#uncertainty-bounds) and steps the clock to host UTC
(`C12`), so the workload starts on host time; this step is not counted as a
discontinuity. Init then reports shell-ready (or starts the workload, in
modes without a shell) and runs every other boot check asynchronously,
starting 150 ms after its time ABI boot step (`nvx-time boot`) at
`SCHED_IDLE`, and at normal priority from 100 ms after they start (see
[Snapshot agent](#snapshot-agent)). The boot step is the anchor because
every mode reaches it, whereas a mode that execs an agent reports readiness
later, on the agent's own protocol. In shell mode the boot step comes 15 to
44 ms before shell-ready on Alpine on bare metal, up to 96 ms before it on
the nested Azure WHP runners, and 26 to 84 ms before it on Ubuntu at 1 to
8 vCPUs, so the checks start about 54 to 135 ms after it. Because they start
at `SCHED_IDLE`, a start closer to shell-ready costs the boot path nothing.
The checks start the daemon when they pass. Every check stays
fail-fast: a failure powers the guest off with status 193, even if the
workload is running. A guest that powers off before its checks finish skips them, which
is why CI takes its evidence from `nvx-time status`, which waits for them.

CPUID is executed on every online CPU (the checker pins itself to each CPU in
turn, or runs one thread pinned to each CPU), and MSRs are read through
`/dev/cpu/<n>/msr`. A CPU's VP index is its Linux CPU number, because CPUs
come online as a prefix in APIC-ID order. CPUID tables are per VP on KVM, so
`C1` and `C2` run on every CPU; the MSRs other than the VP index are
partition-wide on every backend, so `C3` reads them on CPU 0 only.

| ID | Check | Phases |
| --- | --- | --- |
| `C1` | Identity leaves `0x40000000..=0x40000005` equal the [identity table](#hypervisor-identity), with `C` equal to the number of possible CPUs; the explicit zero leaves `0x40000006..=0x4000000f` and `0x40000080..=0x40000082` are zero | boot, restore (new CPUs) |
| `C2` | The [CPU time bits](#cpu-time-bits) | boot, restore (new CPUs) |
| `C3` | MSR `0x40000002` equals the CPU's VP index on every CPU; on CPU 0, `0x40000022` equals `F` with `floor(F / 1000)` equal to the kernel's `cpu MHz` in kHz, `0x40000023` equals 1,000,000,000 or 200,000,000, `0x40000118` equals 1, and reads of `0x40000000`, `0x40000001`, and `0x40000020` fail with `EIO` | boot; restore (CPU 0 `0x40000022` only) |
| `C4` | The kernel log contains `Hypervisor detected: Microsoft Hyper-V`, `Hyper-V: privilege flags low 0x8860,`, `Hyper-V: LAPIC Timer Frequency: 0x989680` or `0x1e8480`, and `clocksource: Switched to clocksource tsc` as its last clocksource switch; it contains no record matching a watcher pattern other than `G_CLOCKSOURCE_SWITCH`, and none containing `Fast TSC calibration`, `Refined TSC clocksource calibration`, `kvm-clock`, or `APIC timer: using supplied frequency` | boot |
| `C5` | `/proc/cpuinfo` flags as listed in [Clocksource, tick, and PIT](#clocksource-tick-and-pit) on every CPU | boot, restore (new CPUs) |
| `C6` | `current_clocksource` is `tsc`; `available_clocksource` lists `tsc` and no other clocksource except `refined-jiffies` and `jiffies`, which Linux lists only before the reading CPU's first tick (after boot it is `tsc` alone) | boot, capture, restore |
| `C7` | `/proc/timer_list`: every online CPU's tick device is `lapic` with `hrtimer_interrupt` in one-shot mode; no `pit` or `hpet` device; no broadcast device | boot, capture, restore (deferred) |
| `C8` | `/sys/bus/vmbus` and `/sys/devices/system/cpu/cpufreq/policy0` are absent; `rcu_cpu_stall_suppress` is 0 and `rcu_cpu_stall_timeout` is 21 | boot |
| `C9` | `/proc/cmdline` contains none of `tsc_early_khz=`, `lapic_timer_hz=`, `notsc`, `nolapic`, `nolapic_timer`, `tsc=unstable`, `hpet=force`, or `clocksource=` with a value other than `tsc` | boot |
| `C10` | The time daemon is running and has recorded no violation; `/sys/kernel/rcu_stall_count` is 0; at boot, the state file is published and the daemon started | boot, capture, restore |
| `C11` | Debug kernel only: `/proc/sys/kernel/soft_watchdog` is 1 and `/proc/sys/kernel/hung_task_timeout_secs` is nonzero | boot |
| `C12` | Before shell-ready: a time sample within the [uncertainty bound](#uncertainty-bounds) is obtained, and the clock is stepped to host UTC | boot |
| `K1` | Kernel integrity, not a clock property: the kernel's boot-time W+X audit logged no `Found insecure W+X mapping` record | boot |

Capture and restore checks never wait. The boot `C7` check and the deferred
restore `C7` check may poll for at most 200 ms, because a CPU switches to
one-shot mode at its first tick.

**Exhaustive check (CI only).** The CI conformance suite also runs
`nvx-time exhaustive`, provided by the guest. It is a test tool: it reports
and exits instead of powering off, and production boots never run it. Every
check runs on every online CPU:

| ID | Check |
| --- | --- |
| `X1` | The explicit zero leaves are zero, and every other leaf in `0x40000006..=0x400000ff` returns all zeros or the Intel out-of-range result (the highest basic leaf's result for the same subleaf), as the [identity rules](#hypervisor-identity) require |
| `X2` | No base `0x40000100..=0x4000ff00` (step `0x100`) carries `KVMKVMKVM` or another hypervisor signature; the NVX kernel has no KVM guest support, so the boot check leaves this static backend property to CI |
| `X3` | Every `C3` MSR |
| `X4` | `0x40000118` accepts writes of 0 and 1, each read back, and rejects 2; the check leaves it at 1 |
| `X5` | Writes to every read-only identity MSR fail |
| `X6` | Reads of `IA32_TSC_ADJUST` and `IA32_TSC_DEADLINE` fail |

It prints one line per check and CPU, then a summary, and exits with status
0 if every check passed and 1 otherwise:

```text
NVX-TIME-ABI-EXHAUSTIVE: v=1 check=<ID> cpu=<n> status=<pass|fail> detail="<escaped text>"
NVX-TIME-ABI-EXHAUSTIVE: v=1 status=<ok|fail> cpus=<online> failures=<n>
```

**Status command.** Checks print nothing on success; they record their
result in the [state file](#wall-clock-discipline). `/sbin/nvx-time status`
prints it on demand: CI scenarios and the matrix driver run it over the
console when they need evidence, outside measured paths, and production
paths never do. It waits until no recorded check is pending, for at most
30 s. A missing state file or boot record counts as pending, because every
boot runs the boot check, and step 8 of the [snapshot agent](#snapshot-agent)
records the restore check as pending before the workload resumes, so a
`status` run after a restore always waits for that restore's deferred work.
It then prints one line per recorded phase, in the order boot, capture, and
restore, then the runtime line:

```text
NVX-TIME-ABI: v=1 phase=<boot|capture|restore> status=<ok|pending> cpus=<n> tsc_hz=<F> lapic_hz=<L> generation=<g> elapsed_us=<duration> cpu_us=<duration>
NVX-TIME-ABI: v=1 phase=runtime status=<synchronized|unsynchronized> generation=<g> discontinuities=<n> offset_ns=<theta> uncertainty_ns=<epsilon> rejected_samples=<n> last_sample_error=<none|G_SAMPLE_UNCERTAIN>
```

Each phase line reports its own check:

- `cpus` is the number of online CPUs the check covered.
- `generation` is `g` when the check ran: 0 for boot, the capture's `g` for
  capture, and at least 1 for a restore.
- The capture line comes from the checks before the capture request (step
  1), which the snapshot carries, so a restored guest still reports it.
- `status` reports the latest capture, which in a restored guest is the
  capture that produced its snapshot, and the restore of this process: a
  restored process never captures, and every restore is a new process.
- The runtime line describes the guest at the time `status` runs.

No other line uses the `NVX-TIME-ABI:` prefix, so a harness can parse every
line by its `phase`.

`status` exits with status 0 when every phase line reports `ok`. If a check
is still pending after 30 s, its line reports `status=pending`, without
`elapsed_us` or `cpu_us`, and the exit status is 1.

At boot, the checks' work is the checks and the daemon start, but not the
150 ms delay after the boot step; at capture,
the step 1 checks of the [snapshot agent](#snapshot-agent). At restore, it
is steps 6 to 12 and the step 11 checks, but not step 13's 150 ms delay, the
grace period wait, or the deferred `C7`. `elapsed_us` is that work's wall
time, including any wait for a CPU at `SCHED_IDLE`; it bounds the fail-fast
latency and is not budgeted. `cpu_us` is the CPU time that the `nvx-time`
processes spend on it (`CLOCK_PROCESS_CPUTIME_ID`, summed over their
threads); other processes, such as the shell's children in steps 9 and 10,
are not counted. At restore, `cpu_us` counts only step 13's part: the
daemon's preparation, including its side of the fork that runs the checks,
and the step 11 checks. Steps 6 to 12 are the readiness
path, whose cost the restore latency gate already measures, so `cpu_us` is
the background cost to the workload. It includes interrupts that arrive
while the checks run, and it scales with the host processor's clock: a host
idling at a low frequency runs the checks more slowly, and because the
Hyper-V identity gives the guest no steal-time accounting, host preemption is
charged to the checks too. The fleet matrix driver checks every `cpu_us`
sample against the backend's budget for the phase in
[Performance expectations](#performance-expectations-and-acceptance-gate)
during validation; CI reports it without gating on it.

A failed check prints no marker: it emits the violation event with code
`G_CONFORMANCE_<ID>` (`G_KERNEL_WX` for `K1`) and powers off with status 193,
so `status` never reports a failure.

**Report-only mode (test only).** The kernel token `nvx_time_abi=report-only`
makes the checks and the watcher report instead of powering off, so the guest
can be evaluated under an OpenVMM that does not implement the time ABI.
Violations print `NVX-TIME-REPORT-VIOLATION` lines with the event's fields.
`nvx-time status` prints its phase lines with the `NVX-TIME-REPORT` prefix,
`status=fail` for a failed check, and ` failures=<n>` appended, and it exits
with status 1 if any check failed. The state file carries them: in this mode
only, a phase's `_status` key may be `fail`, and `boot_failures`,
`capture_failures`, and `restore_failures` count each check's failures.
These prefixes and keys are reserved for this mode; production images never
set the token, and harnesses never accept a report-only boot as conformant.

### Violation watcher

The boot check opens `/dev/kmsg`, seeks it to its end, and only then reads
the whole kernel log once with `syslog(SYSLOG_ACTION_READ_ALL)` for `C4`. The
daemon inherits that descriptor and follows it, so no record is missed;
records logged in between are seen twice, and identical lines are one event.
A log buffer that wrapped before the boot check fails `C4`, because its
required records are missing. The daemon sets `oom_score_adj` to -1000. A
record matches when its message text contains every substring of a row:

| Code | Substrings |
| --- | --- |
| `G_TSC_UNSTABLE` | `Marking TSC unstable due to ` |
| `G_CLOCKSOURCE_UNSTABLE` | `timekeeping watchdog on CPU` and `as unstable because the skew is too large` |
| `G_CLOCKSOURCE_SWITCH` | `clocksource: Switched to clocksource ` (any switch after the boot check) |
| `G_CLOCKSOURCE_SKEW` | `clocksource: ` and ` ahead of CPU `, or `clocksource: ` and ` behind CPU ` |
| `G_TSC_WARP` | `TSC synchronization [CPU#`, `cycles TSC warp between CPUs`, or `TSC warped randomly between CPUs` |
| `G_TSC_ADJUST` | `TSC ADJUST` |
| `G_RCU_STALL` | `rcu: INFO: ` and one of ` detected stalls on CPUs/tasks`, ` self-detected stall on CPU`, or ` detected expedited stalls` |
| `G_RCU_STARVED` | `rcu: ` and one of ` kthread starved for ` or ` kthread timer wakeup didn't happen for ` |
| `G_SOFT_LOCKUP` | `watchdog: BUG: soft lockup - CPU#` |
| `G_HARD_LOCKUP` | `Watchdog detected hard LOCKUP` |
| `G_HUNG_TASK` | `INFO: task ` and ` blocked for more than ` |
| `G_UNCHECKED_MSR` | `unchecked MSR access error` |
| `G_APIC_ID_MISMATCH` | `APIC ID mismatch` (Linux found a CPUID APIC identity that differs from the CPU's APIC: a backend did not present a per-VP field of the [effective CPUID](#cpu-profiles)) |
| `G_KMSG_OVERRUN` | A read of `/dev/kmsg` fails with `EPIPE` (records were lost) |

Before the boot check passes, a match is reported by `C4`. Afterwards it is
a runtime violation: the daemon emits the event and powers off with status
194. The daemon also reads `/sys/kernel/rcu_stall_count` at every discipline
poll and reports `G_RCU_STALL` if it is nonzero, so a stall is caught even if
its log record is lost.

A violation event is one line of at most 512 bytes:

```text
NVX-TIME-ABI-VIOLATION: v=1 code=<code> source=<conformance|watcher|repair> phase=<boot|capture|restore|runtime> generation=<g> boottime_ns=<CLOCK_BOOTTIME> detail="<escaped text>"
```

The guest writes it to `/dev/kmsg` at priority 2, to the portb data port
`0xe9` (through `/dev/port` or `outb`), and to the console, on every path:
violations are rare, and a diagnosable failure is worth the console bytes.
Consumers treat identical lines as one event. The guest then writes the
status to the shutdown port `0x604` (directly, or through `nvx-exit`); its
first byte becomes the OpenVMM process exit status.

### Snapshot agent

`nvx-snapshot` performs these steps for every tier and for untiered
snapshots. Steps marked "debug" apply only to the CI debug kernel, which
builds the soft-lockup and hung-task detectors.

Before the capture request:

1. Wait until no boot check is pending, for at most 30 s, as
   `nvx-time status` does: a capture right after shell-ready may find the
   asynchronous boot checks still running, and the daemon, which `C10`
   requires, starts only when they pass. A timeout fails the capture with
   `G_CONFORMANCE_C10` (193). Then run the capture checks (`C6`, `C7`,
   `C10`), which do not wait and print nothing; they record
   `capture_status=ok` in the state file, which the snapshot carries.
2. Save `/sys/module/rcupdate/parameters/rcu_cpu_stall_suppress` and write 1.
   Values that an earlier request saved and never restored refuse the
   capture.
3. Debug: save and zero `/proc/sys/kernel/soft_watchdog` and
   `/proc/sys/kernel/hung_task_timeout_secs`.
4. Apply the existing freezer and scratch barriers.
5. Write the capture request to `0x605`.

A committed capture terminates the source, so the write returns in the
source VM only when the capture had no destination or failed before its
commit point and OpenVMM resumed the guest. Step 6's status read tells the
cases apart: bit 1 is clear in the source, which has no restore packet and
keeps its generation, and set in a restored process, whose packet carries
the next `g`. Reads of `0x605` return `0xff`; a capture reports no status.
In the source, the agent removes the barriers and then restores the values
saved in steps 2 and 3 (`nvx-time cancel-capture`); if any of these fails,
it powers the guest off with status 1. A restored process runs these steps.
Steps 6 to 10 and 12 are the readiness path: nothing else runs before the
acknowledgement.

6. Read the status from `0xea`. Bit 1 is always set after a restore.
7. Read `CLOCK_REALTIME` as `t0`, write `0xa5` to `0xea`, read it as `t1`,
   and read the packet with four-byte `inl` reads. (`rep insl` would take a
   single exit on KVM, but OpenVMM's instruction emulator, which serves
   string port I/O on MSHV and WHP, raises #GP for it at CPL 3 although the
   TSS I/O permission bitmap grants the port.) Validate the magic, version,
   reserved bits, counts, `g`, and the generation ID (`G_REPAIR_PACKET`,
   `G_REPAIR_GENERATION`).
8. Set the wall clock. Compute `theta` and `epsilon` from the bracket. If
   `epsilon` exceeds 1 ms, take up to two time samples and keep the one with
   the smallest uncertainty; if none is within 1 ms, fail
   (`G_REPAIR_SAMPLE`). Apply `adjtimex` with `ADJ_SETOFFSET | ADJ_NANO` and
   `time = theta`, then `ADJ_FREQUENCY | ADJ_STATUS` with
   `freq = -rate_deviation` (clamped to ±500 ppm) and
   `status = STA_PLL | STA_NANO` (`G_REPAIR_CLOCK`). Record the restore
   discontinuity and the new `g`, with `restore_status=pending` and
   `restore_generation=g` in the same state-file update, so that
   `nvx-time status` waits for this restore.
9. Run the existing processor and memory activation.
10. Run the existing entropy and identity repair for the tier. The RTC-based
    wall-clock refresh is removed; `instance-checkpoint` restores also get
    step 8.
11. Defer the restore checks to step 13: `C1`, `C2`, and `C5` on newly
    onlined CPUs; `C3` for CPU 0; `C6`; and `C10`. They do not wait, and a
    failure still emits its violation event and powers off with status 193.
12. If `ACK_REQUIRED` is set, write the acknowledgement (2) to `0x605`.
13. After the acknowledgement, or after step 10 when none is required, signal
    the daemon, which finishes the restore asynchronously, starting 150 ms
    later at `SCHED_IDLE`:
    - it runs the step 11 checks;
    - it waits for the [grace period release](#rcu-grace-period-release);
    - it restores the values saved in steps 2 and 3
      (`G_REPAIR_SUPPRESSION`);
    - it runs the deferred `C7` check and records the restore check in the
      state file;
    - it restarts the discipline at the fast cadence.

    It prints nothing unless a check fails.

No restore check runs before the acknowledgement. The post-acknowledgement
checks are `C1`, `C2`, `C3`, `C5`, `C6`, `C7`, and `C10`.

Steps 6, 7, and 8 run in one helper process. For an untiered restore with no
processor or memory to activate, that helper runs every step through 12 and
signals the daemon, so no other process starts on the readiness path. Each
extra process costs 2 to 4 ms after a restore (fork and exec under demand
faulting, measured on KVM).

Step 13 starts 150 ms after the acknowledgement (or after step 10, when none
is required), and the asynchronous boot checks start 150 ms after the boot
step. Starting them earlier slows the guest: their checks contend
with the readiness path on the other vCPUs, even at `SCHED_IDLE`. On WHP
(bare metal, 512 MiB, measurement-only builds), checks right after the
acknowledgement added 6 to 12 ms to 2-vCPU restores, whereas a 100 ms
delay left them 0.8 and 1.2 ms over the release guest at 1 and 2 vCPUs. On
bare-metal MSHV, boot checks right after shell-ready added about 3 ms
to 2-vCPU cold boots. KVM showed no clear effect either way. The delay is
150 ms rather than 100 ms because a workload's own activity right after
readiness must not meet the checks either: the guest-exit window of
`network_snapshot_restore_wall` falls 110 to 130 ms after readiness on WHP
and 87 to 97 ms on KVM, which checks starting at 100 ms could overlap.

Step 13 and the asynchronous boot checks run at `SCHED_IDLE`, so on few
vCPUs they never take a CPU from the restored or starting workload. Work
that has not finished 100 ms after it starts continues at normal priority.
Every check stays fail-fast: a failing check powers the guest off about
150 ms after the acknowledgement or the boot step on an idle guest, and at
most about 250 ms plus the checks' own run time after it on a CPU-bound
one, where `SCHED_IDLE` alone would allow about 0.5 s or more. The `elapsed_us`
and `cpu_us` of both exclude the 150 ms delay. `nvx-time status` waits for
this deferred work, within its 30 s bound. The watcher and the discipline
always run at normal priority, and stall suppression stays set until the
release. Repair failures emit a violation event and power off with status
195.

### RCU grace period release

Stall suppression is required: on KVM, restores after 30 s of downtime
without it report `rcu_preempt self-detected stall` at 1 and 8 vCPUs, because
jiffies jump by `D`; with it, no stall appears at 1, 2, 4, or 8 vCPUs.

Stall suppression may be released only after the grace period that was in
flight at capture has ended, or the restored guest reports a false stall.
`membarrier(MEMBARRIER_CMD_GLOBAL)` does not guarantee this: Linux 6.18 skips
`synchronize_rcu()` when one CPU is online, and with `rcupdate.rcu_expedited=1`
it runs an expedited grace period, which does not end the in-flight normal
one. The daemon therefore:

1. saves `/sys/kernel/rcu_normal` and writes 1, so every synchronous grace
   period, expedited or not, waits for a normal grace period;
2. mounts a private `tmpfs` at `/run/nvx/rcu-sync` and unmounts it (on every
   unmount, Linux 6.18 `namespace_unlock()` waits in
   `synchronize_rcu_expedited()`, which now waits for a full normal grace
   period started after the call, on any number of CPUs);
3. restores `/sys/kernel/rcu_normal`; and
4. releases the suppression.

If step 2 has not returned after `rcu_cpu_stall_timeout` seconds, the daemon
releases the suppression anyway: the grace period is genuinely stuck, and the
kernel then reports the real stall.

### Wall-clock discipline

The daemon keeps `CLOCK_REALTIME` on host UTC through the kernel's PLL:

- **Cadence.** A poll every 16 s for the first four polls after the boot
  check or a restore, then every 64 s.
- **Sample.** Up to three time samples per poll; keep the first with
  `epsilon <= 50 µs`. Otherwise skip the poll, count a rejected sample, and
  set `last_sample_error=G_SAMPLE_UNCERTAIN` in the state file; nothing is
  printed. The next accepted sample resets `last_sample_error` to `none`.
  A sample whose `g` differs from the recorded `g` is discarded: a restore
  happened, and restore repair resets the discipline.
- **Step.** If `|theta| >= 128 ms`, apply `ADJ_SETOFFSET | ADJ_NANO` with
  `time = theta`, count a discontinuity, and reapply the frequency and
  status.
- **Slew.** Otherwise apply `ADJ_OFFSET | ADJ_STATUS | ADJ_NANO |
  ADJ_TIMECONST | ADJ_MAXERROR | ADJ_ESTERROR` with `offset = theta`
  nanoseconds, `status = STA_PLL | STA_NANO` (clearing `STA_UNSYNC`),
  `constant = 4` at the 16 s cadence or 6 at the 64 s cadence,
  `maxerror = ceil((|theta| + epsilon) / 1000)` µs, and
  `esterror = ceil(epsilon / 1000)` µs. A clear `STA_UNSYNC` reports the
  clock to applications as synchronized, with these error bounds. It also
  makes the kernel write the CMOS RTC within a second and then every
  11 minutes, which v1 accepts (see
  [Performance expectations](#performance-expectations-and-acceptance-gate)).
- **Bounds.** The kernel limits the frequency to ±500 ppm; the declared rate
  tolerance uses at most 250 ppm of it.
- **Convergence.** The boot check and restore repair step the clock, and
  restore repair corrects the TSC's rate deviation. Neither corrects the rate
  at which the host's own UTC clock runs against its TSC, which host time
  synchronization slews by a few ppm. The PLL's frequency integrator absorbs
  that rate over about an hour at the 64 s cadence. Until then the offset
  settles near 256 s times the rate, behind host UTC when the host's UTC runs
  fast and ahead when it runs slow: 1.76 ms behind at the +6.88 ppm of an
  Azure 8370C WHP runner, about 3.2 ms ahead at the −12.89 ppm of an 8573C
  one, and 4.06 ms behind at the +16.07 ppm of the bare-metal WHP host,
  against 4.11 ms predicted. The bare-metal MSHV host, at +14.82 ppm, was 3.50
  ms behind after 12 minutes and still approaching its 3.79 ms plateau. Where
  the host's own UTC is being slewed, as on a nested Azure MSHV host, the
  offset follows the changing rate. A skipped poll (`G_SAMPLE_UNCERTAIN`) lets
  the offset grow at the remaining rate for another 64 s, and the nested WHP
  runners, whose sample uncertainty approaches the 50 µs bound, skip some: the
  8573C runner reached 4.1 ms after two skipped polls in a row. All of these
  are far below the step threshold.

The discipline never powers off the guest: a host wall-clock step is
followed, not reported as a violation.

**State file.** The time component publishes `/run/nvx/time/state`
from the start of the boot checks, replaced atomically with `rename(2)` after
every change, and `nvx-time status` reads it. The sandbox agent bind-mounts
`/run/nvx/time` read-only into the container at the same path. The file holds
`key=value` lines:

| Key | Content |
| --- | --- |
| `version` | 1 |
| `generation` | `g` |
| `boot_status`, `capture_status`, `restore_status` | `pending` while the phase's check runs, then `ok`; absent until the phase first runs (a failed check powers the guest off). The capture keys hold the latest capture, and the restore keys this process's restore |
| `boot_cpus`, `capture_cpus`, `restore_cpus` | Online CPUs the check covered |
| `boot_elapsed_us`, `capture_elapsed_us`, `restore_elapsed_us` | Its wall time, as `elapsed_us` in the [status command](#conformance-checks-and-the-nvx-time-abi-marker) |
| `boot_cpu_us`, `capture_cpu_us`, `restore_cpu_us` | Its CPU time, as `cpu_us` in the [status command](#conformance-checks-and-the-nvx-time-abi-marker) |
| `capture_generation`, `restore_generation` | `g` when the check ran (the boot check's is 0) |
| `boot_failures`, `capture_failures`, `restore_failures` | [Report-only mode](#conformance-checks-and-the-nvx-time-abi-marker) only: the check's failure count |
| `tsc_hz`, `lapic_hz` | `F` and `L` |
| `discontinuities` | Wall-clock discontinuities since cold boot: restores plus discipline steps |
| `last_discontinuity` | `none`, `restore`, or `step` |
| `last_step_ns` | Signed size of the last step |
| `last_step_realtime_ns` | `CLOCK_REALTIME` right after the last step |
| `last_downtime_ns` | `D` of the last restore |
| `last_downtime_source` | `monotonic` or `utc` |
| `synchronized` | 1 when the last accepted sample is at most 128 s old |
| `offset_ns` | Last measured `theta`. Until the first discipline poll, the boot check's or restore repair's `theta` from before its step |
| `uncertainty_ns` | Last accepted `epsilon` |
| `frequency_ppb` | Current kernel frequency correction |
| `samples`, `rejected_samples` | Sample counters |
| `last_sample_error` | `none` or `G_SAMPLE_UNCERTAIN` |
| `violations` | 0; a violation powers the guest off |

Workloads that need prompt notice of a step can also arm a `timerfd` with
`TFD_TIMER_CANCEL_ON_SET`, which the kernel cancels on every step.

## Failure codes

Codes are stable. OpenVMM errors put the code in brackets at the start of the
message, for example `[E_TSC_RATE_TOLERANCE] destination TSC rate ...`, and
the code survives every error-wrapping layer, including the worker boundary:
the process's fatal-error line leads with the first code in the chain,
`fatal error: [E_TSC_RATE_TOLERANCE] <outermost context>`, followed by the
full cause chain. A rejected cold boot or restore exits the OpenVMM process
with status 1, the existing fatal-error status; tests and orchestrators match
the bracketed code at the start of that line. A capture that a time ABI
check rejects is rollback-safe: the guest continues, and OpenVMM logs the
code. A capture that fails for another reason before its commit point, such
as a failed publication, also rolls back, and its log carries no time ABI
code.

| VMM code | Condition | Detected at |
| --- | --- | --- |
| `E_SNAPSHOT_VERSION` | Manifest version is not 6; the snapshot must be recaptured | Restore |
| `E_MANIFEST_TIME` | Time contract missing or malformed, `time_abi_version` not 1, or tolerance not 250 | Restore |
| `E_BACKEND_MISMATCH` | Snapshot taken on another backend | Restore |
| `E_PROFILE_UNKNOWN` | Profile ID not pinned in this OpenVMM, or a restore's explicit `--cpu-profile` names another profile than the snapshot's | Cold boot, restore |
| `E_PROFILE_DIGEST` | The recorded profile digest or embedded document is not the pinned profile's of the same ID | Restore |
| `E_PROFILE_HOST_UNKNOWN` | `--cpu-profile auto` maps the host to no profile, or to profiles of more than one generation | Cold boot |
| `E_PROFILE_TIME_BITS` | The profile is invalid or violates the CPU time bits | Cold boot, restore |
| `E_CPU_GENERATION` | Host CPU vendor, family, model, or stepping not in the profile | Cold boot, restore |
| `E_PROFILE_UNSUPPORTED` | Backend lacks a feature, limit, XSAVE layout, MSR value, or feature-bank bit of the profile, or the host is not qualified | Cold boot, restore |
| `E_CPU_SURFACE` | Recomputed effective CPUID differs from the recorded one, or VP 0's CPUID differs from the effective CPUID under its masks | Cold boot, restore |
| `E_CPU_UNLISTED` | On MSHV or WHP, the hypervisor presents a non-zero CPUID entry outside the profile's tables, a reserved entry that would expose a host feature | Cold boot, restore, `--cpu-fingerprint` |
| `E_IDENTITY_ROUTING` | Backend cannot deliver the identity CPUID or MSRs | Cold boot, restore |
| `E_TSC_SYNC_UNSUPPORTED` | Backend lacks the synchronized TSC set | Cold boot, restore |
| `E_TSC_SCALING_ACTIVE` | The guest TSC would be scaled | Cold boot, restore |
| `E_TSC_RATE_UNAVAILABLE` | Backend cannot report its native TSC rate | Cold boot, capture, restore |
| `E_TSC_RATE_IMPLAUSIBLE` | A rate is outside 500 MHz to 10 GHz | Cold boot, capture, restore |
| `E_TSC_RATE_TOLERANCE` | `abs(F_d - F_s)` exceeds 250 ppm of `F_s` | Restore |
| `E_LAPIC_RATE_UNAVAILABLE` | Backend cannot report its LAPIC rate | Cold boot, capture, restore |
| `E_LAPIC_RATE_MISMATCH` | `L` is not the backend constant, or `L_d` differs from `L_s` | Cold boot, restore |
| `E_HOST_IDENTITY` | Host identity, boot identity, or clocks unavailable | Capture, restore |
| `E_DOWNTIME_NEGATIVE` | Downtime below zero | Restore |
| `E_DOWNTIME_EXCESSIVE` | Downtime above 30 days | Restore |
| `E_TSC_ANCHOR` | An anchor is unavailable, or none of at most 64 samples pairs within 100 µs | Capture, restore |
| `E_TSC_TARGET_OVERFLOW` | `TSC_target` exceeds 64 bits | Restore |
| `E_TSC_SYNC_READBACK` | A VP does not hold the synchronized value | Restore |
| `E_VP_LATE_CREATION` | A VP was instantiated after the synchronized set | Restore |
| `E_LAPIC_PERIODIC` | A periodic LAPIC timer is armed | Capture, restore |
| `E_LAPIC_TSC_DEADLINE` | TSC-deadline mode or state is present | Capture, restore |
| `E_PIT_ACTIVE` | PIT channel 0 is counting in a periodic mode | Capture, restore |
| `E_GENERATION_EXHAUSTED` | `capture_generation + 1` overflows `u32` | Restore |
| `E_CMDLINE_CLOCK_TOKEN` | `tsc_early_khz=` or `lapic_timer_hz=` in a supplied or saved command line | Cold boot, restore |
| `E_TEST_HOOK` | A malformed or unknown test hook | Cold boot, restore |

Guest failures power off through `0x604` with a status distinct from the
statuses the guest already uses (0, 1, 37, 125, 126, 127, 128 to 192 for
signals, and 255):

| Status | Class | Guest codes |
| ---: | --- | --- |
| 193 | Conformance | `G_CONFORMANCE_C1` to `G_CONFORMANCE_C12`, `G_KERNEL_WX` |
| 194 | Runtime violation | `G_TSC_UNSTABLE`, `G_CLOCKSOURCE_UNSTABLE`, `G_CLOCKSOURCE_SWITCH`, `G_CLOCKSOURCE_SKEW`, `G_TSC_WARP`, `G_TSC_ADJUST`, `G_RCU_STALL`, `G_RCU_STARVED`, `G_SOFT_LOCKUP`, `G_HARD_LOCKUP`, `G_HUNG_TASK`, `G_UNCHECKED_MSR`, `G_APIC_ID_MISMATCH`, `G_KMSG_OVERRUN` |
| 195 | Restore repair | `G_REPAIR_PACKET`, `G_REPAIR_GENERATION`, `G_REPAIR_SAMPLE`, `G_REPAIR_CLOCK`, `G_REPAIR_SUPPRESSION` |

`G_SAMPLE_UNCERTAIN` is the only non-fatal guest code: the discipline records
it and continues (see [Uncertainty bounds](#uncertainty-bounds)). Every fatal
guest code prints its violation event before the power-off, on production
paths too. No other time ABI line reaches the console unless a test runs
`nvx-time status` or `nvx-time exhaustive`. A boot or restore check that
fails after shell-ready or after the acknowledgement still powers off with
193, while the workload may be running.

A workload can exit with any 8-bit status, so a harness classifies a time
failure by the status together with its `NVX-TIME-ABI-VIOLATION` event.

## Host qualification

`nvx.py doctor --backend <backend>` qualifies a host by running every check,
with `H4` and `H6` on their long schedules. The `validate-runner` action runs
`H1`, `H2`, and the short `H4` before every CI job and `H3` in microVM jobs,
and every CI microVM boot and restore scenario runs the CI warp schedule;
`H5` and `H7` run in `doctor` only. Both tools print one
`NVX-DOCTOR: check=<id> status=<pass|fail> detail=...` line per check and
fail if any check fails. The time checks replace the `nonstop_tsc` check.

| ID | Check |
| --- | --- |
| `H1` | The backend device or API is present and usable |
| `H2` | CPU fingerprint: vendor, family, model, stepping, microcode, host kernel or OS build, the generation name, and the profile that `auto` selects; an unmapped generation fails (`E_PROFILE_HOST_UNKNOWN`). `openvmm --hypervisor <backend> --cpu-fingerprint <path>` writes the fingerprint (`openvmm-cpu-fingerprint/v1`), checks it against the generation's profile, and prints one `NVX-CPU-PROFILE:` line (`E_PROFILE_HOST_UNKNOWN`, `E_PROFILE_UNSUPPORTED`, `E_CPU_UNLISTED`). The host's invariant TSC (`constant_tsc` and `nonstop_tsc` on Linux, the CPUID bit on Windows) is reported as evidence only |
| `H3` | OpenVMM preflight in verification mode: profile support, identity routing, synchronized TSC set, no scaling, and both rates, without booting a guest |
| `H4` | TSC rate stability against the host's clocks: interval agreement against an undisciplined clock (`CLOCK_MONOTONIC_RAW` or `QueryPerformanceCounter`) and the whole-window rate against `F_d` on the disciplined one (see [Rate stability](#rate-stability-h4)). On a Linux host the detail also reports the host clocksource, as evidence only |
| `H5` | Host cross-CPU TSC skew: a pinned-thread probe over all host CPU pairs, `max_abs_offset_ns <= 1000` |
| `H6` | Guest warp probe on the qualification schedule (see [Warp schedules](#warp-schedules-h6-and-ci)) |
| `H7` | Host UTC is synchronized: no `STA_UNSYNC` on Linux; a synchronized `w32tm` source on Windows |

### Rate stability (`H4`)

A host probe compares the TSC with two host clocks:

- **Interval agreement and conclusiveness** use a clock that time
  synchronization never steers: `CLOCK_MONOTONIC_RAW` on Linux and
  `QueryPerformanceCounter` on Windows.
- **The whole-window rate `r`** and its deviation from `F_d` use the
  disciplined `CLOCK_MONOTONIC` on Linux, which is never stepped and whose
  long-run rate is the true one, and `QueryPerformanceCounter` on Windows.

A sample reads the TSC between two reads of a clock and keeps the tightest of
up to 64 such brackets. Its uncertainty is half the bracket plus half the
clock's resolution. The probe reads each clock 100,000 times back to back.
If two consecutive reads ever return the same value, the clock is coarser
than one read, and its resolution is the larger of the reported resolution
and the smallest nonzero step between consecutive reads: 100 ns for Hyper-V's
reference TSC page clock (`hyperv_clocksource_tsc_page`), the clocksource of
MSHV roots and the Azure MSHV runners, which `clock_getres` reports as 1 ns,
and one 10 MHz tick (100 ns) for `QueryPerformanceCounter`. A clock that
advances on every read keeps its reported resolution, because its smallest
step is then only the time one read takes (for example
`CLOCK_MONOTONIC_RAW` on a host whose clocksource is `tsc`).

The probe sleeps between samples, so the host's CPUs can idle: 3 samples 1 s
apart in `validate-runner`, and 13 samples 10 s apart (120 s) in `doctor`.
For each interval between consecutive samples, it computes against the
undisciplined clock the rate `r_i`, in TSC cycles per second, and its
relative uncertainty `u_i`, the two samples' uncertainties divided by the
interval. `r` is the rate over the whole window against the disciplined
clock. The check passes if:

- every `u_i` is at most 0.25 ppm; otherwise the measurement is
  inconclusive and fails;
- the interval rates agree within 1 ppm (largest minus smallest, relative to
  their whole-window rate against the same clock); and
- where `F_d` is known (in `doctor`, and in the `validate-runner` jobs that
  run `H3`), `|r - F_d|` is at most 100 ppm of `F_d`.

The probe's output names both clocks, each with its role (rate or
stability), reported resolution, and observed step. The detail reports `r`,
its deviation from `F_d` in ppm, the agreement, the largest `u_i`, both
clocks and the undisciplined clock's measured resolution, and, on Linux, the
host clocksource, which never gates.

Interval agreement uses the undisciplined clock because chrony's frequency
updates move `CLOCK_MONOTONIC`'s rate between seconds, on Azure through the
Hyper-V PTP clock:

- Against `CLOCK_MONOTONIC`, the short schedule failed 3 of 9 runs on the
  Azure MSHV runners: their interval rates differed by up to 12.2 ppm. Over
  30 1-s intervals they spread by up to 2.7 ppm, against at most 0.07 ppm
  versus `CLOCK_MONOTONIC_RAW`, while chrony's frequency moved by up to
  2.7 ppm.
- Against `CLOCK_MONOTONIC_RAW` and `QueryPerformanceCounter`, 35
  short-schedule runs on the Azure KVM, MSHV, and WHP runners agree within
  0.000 to 0.050 ppm, with the largest `u_i` between 0.04 and 0.106 ppm. The
  long schedule agrees within 0.002 ppm on the bare-metal MSHV host, where
  `CLOCK_MONOTONIC` gave 0.402 ppm.
- The WHP hosts show 0.000 ppm with a residual of at most 0.07 µs over 118 s.

A TSC that stops, slows, or is rescaled across idle misses the bound by
orders of magnitude where the host's clocksource does not follow the TSC's
steps: the Hyper-V reference page and `QueryPerformanceCounter` in a VM,
which the hypervisor computes from the TSC but keeps continuous when it
corrects TSC offsets, or the HPET, which has its own oscillator.
Where the host clocksource is `tsc` itself (the Azure KVM runners and the
bare-metal KVM host), every kernel clock derives from the TSC, so no host
clock can catch a TSC step; the guest warp probe (`H6` and the CI schedule)
is the detector there.

The 100 ppm bound keeps the guest's wall-clock discipline, which steers at
most 500 ppm, clear of the TSC's error plus a restore's rate deviation of up
to 250 ppm. On Windows the clock derives from the same TSC, so the deviation
compares `F_d` with the host's own calibration.

### Warp schedules (`H6` and CI)

Each run of the guest warp probe executes
`/sbin/nvx-time-probe warp --bound-ns 1000` over every online vCPU and must
report `max_backward_ns` and `max_abs_offset_ns` at most 1,000, no stall, and
a conclusive verdict. Between runs the guest only sleeps, so every vCPU halts
and the host applies any idle-triggered TSC correction it makes, the source
of the warps in #265:

- `H6`: a microVM with the host's largest supported vCPU count up to 8 boots,
  passes the boot check, and runs the probe five times, with idle gaps of
  0.1, 1, 5, and 1 s; a 1-vCPU microVM then boots and runs it once.
- CI: every microVM boot and restore scenario runs the probe twice, with a
  1 s idle gap, after boot and after every restore.

### Qualification gates

Qualification gates alike on every backend, on measured properties only: the
CPU profile (`H2`, `H3`), rate stability (`H4`), and the idle-scheduled warp
probe (`H6`, and the CI schedule in every microVM job). The host OS's
invariant-TSC bit and the host clocksource are recorded as evidence and never
gate, on any backend:

- On Azure, WHP and nested MSHV cannot offer the invariant-TSC bit to their
  guests through their feature banks, although the host OS sees an invariant
  TSC on every runner except one 8370C MSHV runner. The profile exposes the
  bit through CPUID anyway, so `H4` and `H6` measure what it promises.
- MSHV roots and Azure KVM hosts run `hyperv_clocksource_tsc_page`, not
  `tsc`. The guest has no kvmclock, so no guest clock derives from the host
  clocksource, and the downtime comes from host monotonic time and UTC, whose
  accuracy does not depend on it.
- KVM rewrites a vCPU's TSC offset whenever it loads the vCPU on a host that
  marked its TSC unstable, unless the host clocksource is
  `hyperv_clocksource_tsc_page`. `H6` measures the warps that this would
  cause.

That 8370C MSHV runner, whose host OS lacks the bit and which showed the
#265 warps, is out of rotation and unqualified because our account cannot run
guests there, not because of this rule; its host-level warp probe saw
backward steps of at most 2.1 ns.

### Generations and runner placement

Generation names used in logs, reports, and job summaries are `skylake-sp`
(family 6, model 85), `icelake-sp` (6/106), and `emeraldrapids` (6/207).
`validate-runner` and `nvx.py doctor` detect the generation at run time and
report it, together with the selected profile, `F_d`, `L`, the `H4` rate
metrics, and the skew metrics, in their log and the job summary; an unknown
generation fails qualification explicitly. Runner labels are not used and
runners are not re-registered: per-PR CI captures and restores on the same
runner, and same-generation cross-VM restore is validated by the fleet
restore matrix (`p6-restore-matrix`) on the hosts our account can use.
Routing CI jobs by generation labels is optional future work.

## Performance expectations and acceptance gate

**Gate.** Per backend and vCPU count, no p50 is worse than the base-branch
median by more than `max(5%, 2 ms)`. The gate covers 37 cells: all 34
one-vCPU metrics and `shell_snapshot_restore_512_mib` at 2, 4, and 8 vCPUs.
It is measured on the CI Azure platforms; bare-metal results are
informational. The bare-metal KVM host idles at 800 MHz (`intel_pstate`
powersave without HWP), which inflates its absolute restore latency
(`restored_ms` p50 at one vCPU 39 ms, against 23 ms at full clock) and its
`cpu_us`; the arms of an A/B share the effect.

Expected effects:

| Change | Expected effect |
| --- | --- |
| WHP restored SMP without RDTSC emulation | Removes the 1-to-2 vCPU restore jump (120.7 ms to 165.9 ms p50): SMP restores measured 8 to 18 ms faster in the spike and 14 to 31 ms faster on 8370C runners at 2 to 8 vCPUs. A guest timestamp read after an SMP restore drops from 41 to 73 µs to 10 to 16 ns |
| Packet v4 with four-byte reads instead of 83 or more byte reads | Fewer restore exits than reading packets v1 to v3 a byte at a time. The gate's base reads no packet in its untiered restores, so there the time ABI adds 25 port exits per restore (59 against 34 on KVM): the selector write and 24 four-byte reads |
| Wall clock from the packet instead of RTC polling | Removes at least 32 CMOS port exits and the update wait per tiered restore; the gate's base skips the RTC reads in its untiered restores |
| The kernel's RTC write | Because the discipline clears `STA_UNSYNC`, the guest kernel writes the CMOS RTC within a second of each clock step at boot and restore and then every 11 minutes: 26 port exits per write, about 0.1 ms on KVM and more where port exits cost more. Keeping `STA_UNSYNC` set would avoid it but report `TIME_ERROR` to applications |
| No capture-time clocksource waits | Removes the harness's wait for `tsc-early` to become `tsc`: 0.73 to 0.92 s per capture on MSHV and 0.47 to 0.67 s on WHP; outside the gated metrics |
| `no_timer_check` from the Hyper-V identity, no LAPIC calibration, and no `tsc-early` window at cold boot | About 43 to 52 ms (9 to 15%) faster `cold_start_base` and other quiet cold boots on MSHV and WHP (WHP measured 31 to 55 ms on bare metal, and 61 to 71 ms at one vCPU on Azure); KVM unchanged |
| Invariant TSC from the profile where the backend cannot offer it to guests (Azure MSHV and WHP) | Removes each AP's delay calibration of about 150 ms: an 8-vCPU cold boot takes 299 ms instead of 1,385 ms on Azure MSHV, and cold boots are 0.2 to 1.1 s faster at 2 to 8 vCPUs on Azure WHP |
| Fixed restore work: the restore clock (0.7 to 1.1 ms), restore verification, and the backend preflight | A one-vCPU WHP restore is at parity on Azure 8370C runners (p50 −0.6 ms) and about 2 ms slower on bare metal, where no emulation cost is recovered; the counting LAPIC accounts for at most 0.75 ms of it |
| MSHV root-driver costs | Host characteristics, which the time ABI and the legacy path pay alike, not ABI obligations. On the bare-metal MSHV host (hypervisor 26100.30000) three root-driver costs make up 50 of the 70 ms of a one-vCPU, 512 MiB restore and 130 of 150 ms at eight vCPUs: registering guest RAM, which the driver maps no-access 4 KiB at a time (28 ms; 9.5 ms on Azure); creating each application processor after the first: the second takes about 6 ms and each later one 14 to 15 ms, because `MSHV_CREATE_VP` pre-deposits 90 pages, a VP needs about 172 there, and each failed creation deposits one more page (21 and 78 ms at 4 and 8 vCPUs, and the same at cold boot; 1.9 ms at 8 vCPUs on Azure); and the first touch of each 2 MiB chunk after resume (about 22 ms, see below). The first two serialize on the driver's partition mutex. Prefaulting the snapshot's resident chunks before registration (`MADV_POPULATE_WRITE`) cuts the third to 4 to 10 ms but costs about 45 ms itself, so the remedies are driver changes (larger deposits, and read-only mappings for read faults or a smaller fault granule for private file mappings) or guest RAM filled from the snapshot in parallel. The frozen synchronized TSC set adds 40 to 170 µs for 1 to 4 VPs and no per-VP serialized work |
| Boot check and daemon start | Off the cold-boot path. Before shell-ready only the initial time sample and clock step remain (`C12`: one port write and four reads). The other checks and the daemon start run after shell-ready, and `nvx-time status` reports their wall time (`elapsed_us`, not budgeted) and CPU time (`cpu_us`), which each backend budgets per phase (see `cpu_us` budgets below). Wiring v7 measured medians of 1.6 to 3.7 ms at boot and 0.4 to 1.0 ms at restore on MSHV, 3.5 to 5.1 ms and 2.8 to 5.3 ms on KVM (the bare-metal KVM host idles at 800 MHz, which slows the delayed checks), and 2.6 to 4.8 ms and 7.7 to 11.5 ms on WHP at 1 to 8 vCPUs, with captures under 3 ms everywhere |
| Counting LAPIC instead of TSC-deadline on KVM | Different timer-programming exits; covered by the gate |
| v1 profiles without `ITS_NO` | Linux's ITS mitigation at boot, about 6 ms of a one-vCPU cold boot where the host's KVM advertises `ITS_NO` (the bare-metal KVM host: +6.3 [5.5, 7.3] ms against the same build booted with `indirect_target_selection=off`). The gated Azure KVM, MSHV, and WHP guests were already mitigated and are unchanged; restore is unaffected |
| The profile's CPU view at 2 or more vCPUs on KVM | None at the integration head. Before the L3 fix, multi-vCPU cold boots were slower: +9.2 [5.0, 13.7] ms at 2 vCPUs on the bare-metal KVM host against the pre-profile head, and about +25 ms on nested Azure KVM. OpenVMM's `CPUID.4` reported a private L3 cache per vCPU, which made Linux's cache-info initialization wait a 10 ms tick for CPU 1 on many boots. With the L3 shared by the socket, the bare-metal host's cold boots match the pre-profile head: −0.3 [−3.2, +2.7], −0.0 [−2.1, +2.0], and −1.0 [−3.6, +1.7] ms at 2, 4, and 8 vCPUs. The gate's cold boots are one-vCPU; a boot-only comparison against the `dev` base on nested Azure KVM found no significant difference: +1.7 [−2.5, +6.9] ms at 2 vCPUs, +3.7 [−5.4, +13.6] at 4, and +3.3 [−7.0, +11.5] at 8, which excludes the earlier +25 ms. At 8 vCPUs, the time ABI's VMM boots 9.5 [4.6, 16.4] ms faster and its guest 8.2 [0.4, 15.9] ms slower, which cancel; the guest's part may be the KVM paravirtual features that the identity forgoes (see [Hypervisor identity](#hypervisor-identity)). The msr driver's per-CPU hotplug callback (`msr_init`) waits one or two ticks on multi-vCPU boots, as it did before the profiles; it competes for CPU with the asynchronous initramfs unpack and is off the critical path |
| Restore work before the acknowledgement | Only the packet read, the clock set, CPU and memory activation, and entropy and identity repair, in one helper process. The restore checks, the RCU release, and the deferred `C7` start at `SCHED_IDLE` 150 ms after the acknowledgement, after the readiness path |
| Monotonic time that includes the downtime | Right after resume, the guest kernel runs the timer work that came due during the downtime (kworkers, softirqs, and RCU) and first-touches cold pages doing so: at one vCPU about 1.4 ms on MSHV, 2.2 ms on KVM, and 17 ms on WHP (26 ms at two vCPUs). On WHP each restored 4 KiB page that the guest first touches costs about 50 µs of wall time on bare metal and about 70 to 95 µs on the Azure runners: a nested page fault, about 11 µs of it in the hypervisor on bare metal and the rest resolved by the root, which the VMM never sees. WHP's lazily registered 2 MiB chunks add about 90 µs each, all before readiness, and on bare metal a WHP restore's latency after the VPs are released is roughly the pages it touches times 53 µs. It falls inside the gated restore metric whatever the helper's priority. On MSHV the cost is per 2 MiB chunk: the root driver resolves the first touch of each chunk with 512 copy-on-write copies of the private snapshot mapping, about 1.1 ms. A one-vCPU restore touches about 24 chunks, 25 ms of its restore on both the time ABI and the legacy path, and the time ABI's catch-up adds about one chunk |
| No time ABI console lines in production | Each console byte costs one or two port exits, about 10 to 12 µs on bare-metal KVM and an estimated 20 to 45 µs nested on Azure. Without the boot marker (about 110 bytes) and the restore marker (about 117 bytes), boots and restores save 1 to 5 ms, enough to fail the gate on nested Azure KVM; violation events still print |

**The first second after a restore.** Not all post-resume work finishes before
readiness. At resume the guest runs every timer that came due during the
downtime: the kernel's, those that came due during the VMM's restore setup
(the legacy path stopped guest time during that setup, so its guest ran them
later), and the discipline's poll when the downtime outlasts the poll period.
Restore repair also steps the clock, which the kernel follows with its RTC
write, and the restore checks start 150 ms after the acknowledgement.
Readiness doesn't wait for this work, but a request made during it does, most
where first touches of restored RAM are expensive. On the Azure WHP runners,
an exit requested right at readiness takes 7 to 9 ms longer than on the legacy
path (`openvmm_snapshot_restore_guest_exit_teardown`: +31% and +37%, and +24%
on bare metal). Requested 0.3 s after readiness it takes 3.5 ms longer, and
from 1 s on the difference is within noise. End to end, from process launch to
exit, a WHP restore under the time ABI is at most about 2 ms slower than on
the legacy path, and up to 6 ms faster on bare metal. On nested Azure KVM, a
network restore that follows the shell-snapshot benchmarks' captures and
restores is about 12 ms (8.5%) slower and is no slower on its own, which fits
the same work faulting in restored RAM from a colder host page cache.

Expected wins are tracked separately and do not relax the gate.

**`cpu_us` budgets.** Each backend budgets the CPU time of each phase's
checks (boot, capture, and restore) at a + b·(n − 1) ms for n online vCPUs,
per sample. A budget is the larger of the bare-metal and Azure maxima of
wiring v7m on the production kernel (`vmlinux-lockstep`), with at least
20% headroom:

| Backend | Boot | Capture | Restore | Measured on |
| --- | --- | --- | --- | --- |
| KVM | 6 + 2 | 1 + 0.4 | 6.5 + 1.5 | Bare metal and nested Azure 8370C, 128 MiB |
| MSHV | 3 + 0.75 | 1 + 0.4 | 2.5 + 0.5 | Bare metal and nested Azure 8573C, 128 and 512 MiB |
| WHP | 20 + 1.5 | 1 + 0.4 | 35 + 6 | Bare metal and the Azure 8370C and 8573C runners, 512 MiB |

Fleet validation records every sample's `cpu_us` and checks each one
against these budgets. CI reports each phase against its budget
(`<phase>_cpu_over_budget`) but does not gate on it, because the A/B gate
above covers latency. The checks' CPU time also counts the host stalls of
their first touches of restored RAM:

- WHP's restore checks first-touch restored copy-on-write RAM page by page,
  about 50 µs each on bare metal and 70 to 95 µs on Azure, which the workload
  would otherwise do. Their cost grows with guest memory: medians are 1.3 to
  2.4 times higher at 512 MiB than at 128 MiB at 4 and 8 vCPUs on Azure. So
  WHP's budgets come from 512 MiB guests, which bound smaller ones. Its Azure
  maxima are 17.03 ms at boot with 2 vCPUs and 42.20 ms at restore with 4.
- On MSHV, each first touch of a 2 MiB chunk of restored RAM stalls the
  toucher about 1.1 ms on bare metal (see MSHV root-driver costs). The
  restore budget leaves room for one more stall above every maximum.
- On nested Azure KVM, KVM's restore checks fault in the file-backed restored
  RAM 4 KiB at a time: medians of 3.4 to 7.6 ms at 1 to 8 vCPUs, and maxima
  of 5.15 ms at 1 vCPU and 13.14 ms at 8, over 72 restores.

KVM's budgets come from 128 MiB guests, the size that CI and the fleet
matrices run. MSHV's hold at 128 and 512 MiB alike: MSHV registers all
guest RAM before the restore, and the checks pay only for the chunks they
touch, which doesn't depend on the guest's memory size.

**Start-up budget.** The time ABI and CPU profile work of a cold boot or
restore costs less than 0.5 ms over the pre-profile head (`e7ec0ca6c`),
measured on the bare-metal KVM host with an attribution harness; the
flip requires it. At the integration head (`e06ed4d4a`), up to the backend
preflight at one vCPU, a restore starts 0.32 ms and a cold boot 0.30 ms
faster than `e7ec0ca6c`. The only added work is the worker's time ABI CPU
checks, about 56 µs over the pre-profile path, and every 90% upper bound is
within +0.31 ms. Process start moves each binary's milestones by about
0.25 ms from run to run, so only differences within one run count.
Neither path encodes, decodes, or hashes a profile document
or an effective CPUID:

- the pinned profiles are compile-time constants with precomputed canonical
  encodings and digests (`cpu_profile::pinned_record`), so selecting one
  copies constants;
- capture copies the pinned record and writes the effective CPUID as a packed
  binary record;
- restore compares the recorded digest and document with the pinned record,
  and the recomputed effective CPUID with the binary record, about 5 µs;
- `verify_support` costs 2 to 10 µs of CPU time, plus the backend's surface.

On bare-metal KVM, selection, the pinned-record and generation checks, the
effective CPUID merge, `verify_support`, and the unlisted-entry check cost
about 26 µs together (cold medians over 30 fresh processes). The codecs and
digests (`CpuProfile::{encode, decode, digest, digest_string,
to_pretty_json, from_pretty_json}` and `EffectiveCpuid::{encode, decode,
decode_verified, digest}`), `cpu_profile::pinned_profiles`, and the
fingerprint and derive paths are offline-only: tools, tests, and
`--cpu-fingerprint` use them, and no start path may.

The backends add their own CPUID work:

- MSHV and WHP enumerate the host CPUID once per process, for the supported
  surface and the unlisted-entry candidates, without the hypervisor range,
  which neither uses (`cpu_profile::cpuid::enumerate_basic_and_extended`).
  Every CPUID instruction exits in a Hyper-V root, so a full enumeration
  costs 70 to 100 µs on the bare-metal roots and 0.23 to 0.31 ms on Azure's
  nested roots. Skipping the range took MSHV's from 96 to 49 µs on
  bare metal and from 305 to 220 µs nested, more than its share of the
  queries suggests. KVM takes its surface from `KVM_GET_SUPPORTED_CPUID` and
  does not enumerate. MSHV enumerates on a thread that partition creation
  starts, so the read leaves the start path; starting the thread costs about
  50 µs in a root partition, and keeping its channel until the partition
  drops avoids freeing memory there, which costs 12 to 18 µs.
- MSHV reads VP 0's report in one rep `HvCallGetVpCpuidValues`: 73 µs p50 on
  bare metal and 51 µs nested, against 0.51 and 0.82 ms with
  one call per entry. WHP has no batched read: its report takes 57 to 60
  native reads, one call each, which with its one host enumeration cost
  0.81 ms on bare metal and 1.48 to 1.86 ms on Azure (medians). Reading
  them on a few threads before any VP runs is the allowed lever if a gate
  needs it.

**Attribution.** With `OPENVMM_STARTUP_PROFILE` set, OpenVMM's lifecycle
profile records three exclusive time ABI restore phases:
`restore.time_abi_clock` (restore steps 12 to 16, whose phases OpenVMM also
logs as `time ABI restore clock`: `tsc_set_us`, then `lapic_us` and
`vm_time_us`, which overlap and are each measured from the end of the TSC
set, and `total_us`), `restore.guest_resume`
(from the VP release to the guest's first selection of the restore packet),
and, for a restore with `ACK_REQUIRED`, `restore.guest_repair` (from that
selection to the arrival of the `0x605` acknowledgement). The
`restore.guest_repair_gate` milestone still spans from the VP release to the
release of the acknowledgement boundary. The guest's restore checks run
after the acknowledgement, outside these phases. `nvx-time status` reports
`elapsed_us` and `cpu_us`, which cover the guest's readiness path and those
checks. On
every cold boot and restore, the worker logs `time ABI CPU checks passed`
with `recorded_cpuid_us`, `presented_cpuid_us`, and `profile_support_us`.
Their sum per restore at one vCPU is about 0.15 ms on bare-metal MSHV and
0.12 ms nested, and 0.87 ms on bare-metal WHP and 1.6 to 1.9 ms nested, about
95% of it WHP's per-leaf read-back. OpenVMM's comparison and unlisted-entry
check take 40 to 66 µs of each.

## Test matrix

**Unit tests (OpenVMM).** Rate boundary (250 ppm accepted, one hertz beyond
rejected) in both directions; downtime source selection and bounds, including
equal identities with a different clock kind; `TSC_target` arithmetic and
overflow; LAPIC tick arithmetic for every divide value, expiry, masking, and
periodic rejection; identity CPUID and MSR tables, including every #GP case
and `0x40000118` save and restore; capabilities derivation (`hv1` and
`kvm_clock` false); manifest version 6 validation and rejection of versions
2 through 5; packet v4 and time-sample golden vectors and their guest-side
parser; four-byte portb reads; and the generation counter.

**Conformance.** The boot and restore checks pass at 1, 2, 4, and 8 vCPUs on
all 18 registered hosts, plus the exhaustive CI check and the warp probe on
every backend. Production paths print no marker, so CI scenarios and the
matrix driver run `/sbin/nvx-time status` over the console after shell-ready
and after every restore. They require exit status 0 and, after a restore, a
`phase=restore` line with `status=ok`, the restored `generation`, and `cpus`
equal to the online CPUs; after a cold boot, a `phase=boot` line with
`generation=0`. During fleet validation (`p6`), the matrix driver records
each phase's `cpu_us`, and every recorded sample must be within the
backend's budget for that phase in
[Performance expectations](#performance-expectations-and-acceptance-gate);
CI reports it without gating on it. The fleet runs them on
the hosts our SSH account can use,
which for KVM and for MSHV are a bare-metal Skylake-SP host and a nested
Azure VM (8370C for KVM, 8573C for MSHV): the account cannot open `/dev/kvm`
or `/dev/mshv` on
the other KVM and MSHV runners, which only CI jobs exercise. On the Azure WHP
runners, a cold boot with an empty command line, which logs to the console,
keeps `tsc` as its clocksource (#292: 6 of 6 runs with the time ABI, against
0 of 6 before it, which fell back from `tsc-early` to `refined-jiffies`).

**Restore matrix.** Every case runs with zero violations; rejected cases must
fail with the listed code. A restored case's evidence is the
`nvx-time status` output after the restore, which waits for the deferred
checks. Per-PR CI runs the same-host cases on one runner;
the full matrix, including the cross-VM cases, runs on the hosts our account
can use as the fleet restore matrix.

| Case | Hosts | Expected |
| --- | --- | --- |
| Same host, immediate | All | Restored; `DOWNTIME_UTC` clear |
| Same host, downtime of 30 s (over the 21 s RCU stall timeout), at 1 and 8 vCPUs, with and without `rcupdate.rcu_expedited=1` | All | Restored; no RCU stall; `rcu_stall_count` stays 0 |
| Same host under DVFS load | The bare-metal KVM host | Restored |
| Simulated host reboot: hooks `force-utc-downtime`, `boot-id-mismatch`, and `dest-rate-offset-ppm=+200`, then `-200` | One host per backend | Restored; `DOWNTIME_UTC` and `TEST_HOOKS` set; rate deviation reported |
| Simulated rate beyond tolerance: `dest-rate-offset-ppm=+251` | One host per backend | `E_TSC_RATE_TOLERANCE` |
| Downtime bounds: `downtime-add-s=2592001`; `force-utc-downtime` with `utc-offset-ms=-<n>`, `n` above the elapsed time | One host per backend | `E_DOWNTIME_EXCESSIVE`; `E_DOWNTIME_NEGATIVE` |
| Sample uncertainty: `sample-delay-us=200`; then `sample-delay-us=3000` on restore and on cold boot | One host per backend | Restored and running, with `last_sample_error=G_SAMPLE_UNCERTAIN` in `nvx-time status`; `G_REPAIR_SAMPLE` (195); `G_CONFORMANCE_C12` (193) |
| Wall-clock convergence, for 12 minutes after a restore and after a cold boot, with the host's UTC rate sampled every 10 s alongside (Linux: `CLOCK_REALTIME` against `CLOCK_MONOTONIC_RAW`; Windows: UTC against QPC) | One host per backend | No step after the initial one; `synchronized=1`; `frequency_ppb` moving toward the window's mean rate; and, at every accepted discipline poll, `abs(theta)` at most 512 s × `abs(r)` + 1 ms, where `r` is the host's mean UTC rate over the 512 s before that poll (before then, over the window so far, at least 60 s), which leaves room for four skipped polls in a row. The checks start at the first discipline poll: the boot check's and restore repair's own samples are taken before their steps (at cold boot, the RTC's whole-second error). Skipped polls are reported with their count and longest run, and fail the case only through that bound. The spread of the host's per-minute rate is reported too: where the host's own UTC is being slewed, a minute's rate can swing through zero, so no single minute sets the bound |
| Across VMs of one generation | Between two 8370C Azure WHP runners, and between two 8573C ones. KVM and MSHV have no usable pair of one generation, so the simulated host reboot covers their cross-host path | Restored |
| Across generations | An 8370C Azure WHP runner to an 8573C one; bare-metal Skylake-SP KVM to nested Azure 8370C KVM; bare-metal Skylake-SP MSHV to nested Azure 8573C MSHV | `E_CPU_GENERATION` |
| Backend that cannot offer invariant TSC to its guests, on a host OS that sees it | The Azure WHP runners and nested Azure MSHV | Restored; the guest has `constant_tsc` and `nonstop_tsc`; `H4` and `H6` pass |
| Across backends | Bare-metal KVM to bare-metal MSHV | `E_BACKEND_MISMATCH` |
| Pre-v1 snapshot | Any | `E_SNAPSHOT_VERSION` |
| Failed capture: in one VM, a request whose destination's parent the host made unwritable to the OpenVMM process after launch, so that creating the staging directory fails after quiesce and OpenVMM rolls back (root's `CAP_DAC_OVERRIDE` and an administrator's backup and restore privileges bypass file permissions, so drop them first and probe with the same credentials, or use an immutable or read-only mount); then, with the parent writable again and after 30 s idle, a second request. Companion: a request whose destination already exists, which preflight rejects before quiesce. Scratch variant: the failed request in a VM with a paired scratch device mounted at `/run/nvx/scratch` and a workload in the `container` cgroup, so that the snapshot agent's freezer and scratch barriers engage | Each bare-metal host at 1 and 8 vCPUs, 3 times each, and the scratch variant on one bare-metal host at 1 vCPU, 3 times; [CI's `snapshot-core`](../ci.md) exercises a request without a destination, which OpenVMM releases before preflight, and asserts that the guest continues once, that `nvx-time status` passes at `generation=0` with no restore, and that `rcu_cpu_stall_suppress` reads 0 | OpenVMM logs the rollback (`microVM snapshot failed before commit; attempting rollback`, then `microVM snapshot rollback succeeded; guest resumed`), with no time ABI code, and each failed request returns in the same VM: status bit 1 clear, `nvx-time status` passing at `generation=0` with `discontinuities` unchanged (from the capture's checks; `cancel-capture` records nothing), the saved values back at their values before the request (`rcu_cpu_stall_suppress`, and on the debug kernel `soft_watchdog` and `hung_task_timeout_secs`) with no saved-values file left, and no 193, 194, or 195 and no clock message; the second request captures, which step 2 of the snapshot agent refuses while the first request's saved values remain, and its snapshot restores. The companion logs `microVM snapshot preflight failed; guest continues` with no capture anchor and no rollback, and its guest side is the same. In the scratch variant, the failed request also leaves the workload cgroup thawed (`cgroup.freeze` reads 0), the scratch filesystem taking a write with `fsync` within 1 s, and the workload running, and the real capture and restore with the scratch device pass |
| Processor activation from one boot-online CPU to 2, 4, and 8 | All backends | Restored; warp probe passes |
| Every tier and an untiered snapshot, restored more than once | All backends | Restored; each restore of a snapshot captured at `g = 0` carries `g = 1` and a new generation ID. OpenVMM cannot capture a restored process, so unit tests cover the generation arithmetic beyond one restore, including `E_GENERATION_EXHAUSTED` |

No test reboots a host. The orchestrator asks the user before any real
reboot.

**Reliability.** Repeated snapshot scenarios at 1, 2, 4, and 8 vCPUs on the
hosts the fleet can use (see Conformance), with the warp probe and the
watcher active, report no RCU, soft-lockup, hung-task, clocksource, or warp
message. The CI debug-kernel variant runs the same-host cases with the
soft-lockup and hung-task detectors enabled.

**Performance.** The gate above, computed by the performance agent from the
CI benchmark matrix.

Test hooks are hidden OpenVMM options (`--x-time-abi-test-hook <hook>`,
repeatable):

| Hook | Effect |
| --- | --- |
| `force-utc-downtime` | Select the UTC source even on the same host boot |
| `boot-id-mismatch` | Treat the destination boot identity as different |
| `dest-rate-offset-ppm=<n>` | Perturb the measured `F_d` used by the rate policy and the reported deviation; the TSC is never scaled |
| `downtime-add-s=<n>` | Add `n` seconds to the measured `D` before the bounds check |
| `utc-offset-ms=<n>` | Add `n` milliseconds to every destination UTC reading: the downtime sample, the packet, and time samples |
| `sample-delay-us=<n>` | Delay the handling of the first `0xa5` write and of every `0xa7` write by `n` µs, which widens the guest's bracket |

Every active hook is logged at warning level and sets `TEST_HOOKS` in the
packet and in every time sample.

## Removed mechanisms and migration

Removed from OpenVMM's microVM paths; standard-machine VMs keep their
behavior:

- Cold-boot clock tokens: `prepare_cold_boot_command_line` no longer injects
  `tsc_early_khz=` or `lapic_timer_hz=`, and their propagation, parsing, and
  platform-tier validation are deleted.
- The exact-equality CPU contract (`CpuCompatibilityContract`, including its
  `tsc_deadline` and `kvm_clock` fields) and host-derived CPUID, replaced by
  CPU profiles.
- Leaf `0x15` synthesis on every backend. `tsc_frequency_cpuid_leaves` and
  WHP's `add_frequency_leaves` stay for standard-machine VMs, such as WHP VMs
  with `--hv`.
- Per-backend downtime paths (`advance_snapshot_time`), per-VP TSC
  advancement (`advance_tsc`), TSC-deadline advancement, periodic LAPIC
  advancement, wall-clock-only downtime (`calculate_snapshot_downtime`), and
  the per-VP TSC writes of saved VP state on restore.
- KVM: the microVM downtime use of `KVM_GET_CLOCK`/`KVM_SET_CLOCK` (the
  Hyper-V reference time source keeps them; it requires `hv1`, which the time
  ABI never enables), kvmclock MSR state, KVM CPUID leaves, `KVM_SET_TSC_KHZ`,
  and restore-time `IA32_TSC` writes.
- MSHV: BSP-copy TSC alignment and exact rate equality.
- WHP: the 1 GHz rate request and its silent fallback, and `RestoredTsc`
  with its RDTSC, RDTSCP, and `IA32_TSC` exits.
- Restore packets v1 to v3, and manifest versions 2 to 5 for every snapshot:
  standard-machine disk snapshots also move to version 6, so earlier ones
  must be recaptured too.
- The effect of `--restore-entropy`: every restore exposes packet v4, which
  carries fresh entropy. OpenVMM still accepts the option without effect,
  and the harness stops passing it.

Removed from NVX:

- Kernel: patch 0004 (`lapic_timer_hz`); `CONFIG_CPU_FREQ`,
  `CONFIG_X86_INTEL_PSTATE`, and `CONFIG_SCHED_MC_PRIO` (which selects both
  and needs ACPI CPPC data the microVM lacks); and `CONFIG_KVM_GUEST` with
  `CONFIG_PARAVIRT_CLOCK` and `CONFIG_HALTPOLL_CPUIDLE`, which are dormant
  without a KVM signature. `CONFIG_HYPERVISOR_GUEST` and `CONFIG_PARAVIRT`
  stay on and `CONFIG_HYPERV` stays off. `CONFIG_IRQ_TIME_ACCOUNTING` stays
  off: it slows WHP cold boots by 15 to 24 ms on bare metal, because the guest
  then first-touches about 400 more pages, each a memory-access exit on WHP,
  and it changes no `cpu_us` measurably. The CI
  debug kernel adds `CONFIG_DEBUG_KERNEL` with the soft-lockup and hung-task
  detectors only.
- Guest: RTC polling in `nvx-reseed`, and the restore packet v1 to v3 parser
  in `nvx-port-io`.
- Harness: `tsc=reliable` and `no_timer_check` in `BASE_TUNING`; per-backend
  `clocksource=` tokens; `stable_clocksource_wait_script` and the
  capture-time clocksource waits; the KVM and WHP branches of
  `snapshot-core`; the `smp-lapic` scenarios (`lapic=notscdeadline`), which
  duplicate `smp`; `restore-tsc-sync`, `clearcpuid=tsc_adjust`, and the
  fresh-boot TSC control, which the warp probe replaces; and the dmesg greps
  in `restore-processors.sh`, which the watcher replaces. The
  `cold_start_clocksource` benchmark scenario passes `clocksource=tsc` on
  every backend.
- CI: the `nonstop_tsc` runner check, replaced by host qualification.

Migration impact:

- Every snapshot from a release before the time ABI (manifest version 5 or
  older) is rejected with `E_SNAPSHOT_VERSION`; templates must be
  recaptured.
- OpenVMM, the kernel, and the initramfs must be upgraded together. A new
  guest on an old OpenVMM fails the boot check (status 193); an old guest's
  snapshot agent cannot parse packet v4 and fails closed.
- The guest identifies as Hyper-V on every backend, including KVM.
- CPUID becomes profile-defined, so guests can lose host features that no
  profile of their generation pins.
- Hosts that fail qualification cannot run microVMs until replaced.
  One 8370C MSHV runner stays out of rotation and unqualified because our
  account cannot run guests there.
- Per-PR CI captures and restores on the same runner. Same-generation
  cross-VM restore is validated by the fleet restore matrix on WHP only; no
  usable KVM or MSHV pair of one generation exists, so the simulated host
  reboot covers those backends.
- The `cold_start_clocksource` metric on KVM changes meaning (from
  `kvm-clock` to `tsc`). The change is accepted without an exemption; a gate
  failure on it is investigated as a regression.
- Documentation updates: this document, [Snapshot and
  restore](snapshot-and-restore.md), [Machine and device
  ABI](machine-and-device-abi.md), [Cold boot](cold-boot.md), the
  benchmarks and CI guides, and the OpenVMM Guide (see the appendix).

## Appendix: OpenVMM Guide

The OpenVMM Guide documents the user-facing contract in
`Guide/src/user_guide/openvmm/snapshots.md`:

- **Overview:** the time contract and CPU profile in microVM manifests, and
  manifest version 6 as the only accepted version.
- **Restoring a snapshot:** restore packet v4, the time sample at `0xeb`, the
  generation ID, and the synchronized TSC set with its read-back.
- **Time and CPU compatibility:** the identity, the CPU profile and
  `--cpu-profile`, the 250 ppm rate rule, the exact LAPIC rule, the downtime
  sources and bounds, and the error codes.
- **Limitations:** the same backend, CPU generation, and profile, and the
  recapture of older snapshots.

The CLI reference documents `--cpu-profile` and `--x-time-abi-verify`. The
test hooks stay undocumented in the Guide; this document describes them.
