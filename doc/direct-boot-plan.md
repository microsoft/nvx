# ACPI-free microVM Linux direct boot plan

**Status: Implemented on September 17, 2026.** This document records the
migration from the removed PVH path to the current OpenVMM microVM cold-boot
contract.

## Objective

Boot `MachineProfile::Microvm` through OpenVMM's existing x86-64 Linux direct
boot protocol while:

- keeping ACPI disabled in the guest;
- exposing no ACPI or SMBIOS data;
- using Intel MP 1.4 tables for processor and IOAPIC discovery;
- retaining fixed command-line virtio-mmio discovery and the allowlisted
  microVM device model; and
- preserving the current microVM command-line and shared-status contracts; and
- removing the Xen PVH entry path and all of its supporting OpenVMM code,
  configuration, APIs, tests, artifacts, and snapshot compatibility.

This approach deliberately retains the Linux 64-bit direct-boot ABI. OpenVMM
must therefore continue to construct `struct boot_params`, a bootstrap GDT, and
identity page tables, enter the kernel in long mode with paging enabled, and
pass the `boot_params` address in `RSI`. Removing those responsibilities would
require a different kernel entry protocol.

## Proposed low-memory layout

| Guest physical address | Contents |
| ---: | --- |
| `0x0000..0x000f` | Intel MP floating pointer |
| `0x0400...` | Intel MP configuration table |
| `0x1000` | Linux-direct bootstrap GDT |
| `0x2000` | Linux `boot_params` zero page |
| `0x4000..0x17fff` | Linux-direct identity page tables |
| `0x20000..0x2ffff` | NUL-terminated microVM command line |
| `0x30000..0x30fff` | Reserved virtio shared-status page |
| `0x100000` and above | Kernel, initramfs, and ordinary RAM |

The standard ACPI Linux-direct layout remains unchanged. In particular, its
command line stays at `0x3000` with the existing 4-KiB slot. The MP-table mode
uses `0x20000..0x2ffff`, which supports the microVM limit of 64 KiB including
the trailing NUL and ends immediately before the shared-status page.

## 1. Add an explicit Linux-direct mode

Extend
[`LinuxDirectBootMode`](../openvmm/openvmm/openvmm_defs/src/config.rs) with an
`MpTable` variant. Add it after the existing variants so existing serialized
variant values do not move.

The initial contract for `MpTable` is:

- x86-64 only;
- valid only with `MachineProfile::Microvm`;
- no ACPI, SMBIOS, device tree, or firmware tables;
- Intel MP 1.4 processor, ISA bus, IOAPIC, and IRQ entries;
- Linux 64-bit direct entry through `RSI=boot_params`; and
- no isolation, matching the current microVM restriction.

Do not reuse `LinuxDirectBootMode::Acpi` with empty tables. An explicit variant
makes invalid combinations rejectable before resources are opened and keeps
the guest-visible discovery contract unambiguous.

## 2. Extract shared MP-table construction

Move MP-table construction from
[`vm/loader/src/pvh.rs`](../openvmm/vm/loader/src/pvh.rs) into a protocol-neutral
module such as `vm/loader/src/mptable.rs`.

The shared input should contain only data represented by the MP tables:

```rust
pub struct MpTableConfig<'a> {
    pub apic_ids: &'a [u32],
    pub level_triggered_irqs: &'a [u32],
}
```

Preserve validation for:

- at least one processor;
- contiguous microVM APIC IDs starting at zero;
- APIC IDs representable by MP 1.4 entries;
- ISA IRQ numbers below 16;
- checked table-size and entry-count arithmetic; and
- floating-pointer and configuration-table checksums.

The shared builder should return table bytes without assuming a GDT address.
During migration, the PVH loader validates against its GDT at `0x800` and the
Linux-direct loader validates against its GDT at `0x1000`. After Linux-direct
coverage passes, delete the PVH caller and leave the shared module owned by the
Linux-direct path.

## 3. Add an MP-table Linux loader entry point

Refactor [`vm/loader/src/linux.rs`](../openvmm/vm/loader/src/linux.rs) around an
internal x86 platform description, for example:

```rust
enum X86Platform<'a> {
    Acpi {
        acpi: &'a AcpiTables,
        smbios: Option<&'a crate::smbios::BuiltSmbios>,
    },
    MpTable {
        config: MpTableConfig<'a>,
        reserved_memory_ranges: &'a [MemoryRange],
    },
}
```

Keep the existing public `load_x86()` and `load_config_x86()` behavior intact.
They are also used by OpenHCL and IGVM generation. Add an additive entry point,
such as `load_x86_mptable()`, or a new lower-level configured function used by
both wrappers.

The MP-table path must:

1. Load the uncompressed ELF kernel and optional initramfs through the existing
   kernel-loading code.
2. Import the MP floating pointer and configuration table at `0x0` and `0x400`.
3. Import the existing Linux-direct GDT, zero page, and identity page tables.
4. Place the command line at `0x20000` and enforce a 64-KiB region including
   its trailing NUL.
5. Leave all ACPI fields in `boot_params` zero.
6. Import no RSDP, ACPI, SMBIOS entry point, or SMBIOS structure table.
7. Set the existing Linux-direct initial registers, including `RSI=0x2000`,
   `CR3=0x4000`, paging and long mode enabled, and `RIP` set to the ELF entry.

The microVM should continue to require an uncompressed ELF even though the
general Linux-direct loader supports `bzImage`. Enforce that through the
dedicated entry point or an explicit kernel-format policy so the profile does
not silently acquire decompressor startup work.

## 4. Generalize zero-page and e820 construction

The existing zero-page builder assumes mandatory ACPI placement. Refactor it
so the boot header fields and e820 construction are separable from ACPI and
SMBIOS placement.

For MP-table boot, construct the e820 map from `MemoryLayout` plus typed
reservations. It must include:

- all active low and high RAM ranges from the microVM memory layout;
- `0x30000..0x31000` as `E820_RESERVED`;
- the x86 ISA hole at `0xa0000..0x100000` as reserved; and
- the fixed 3-GiB to 4-GiB MMIO gap as non-RAM.

The implementation must validate that caller-provided reservations are
nonempty, ordered, page-aligned, nonoverlapping, and wholly contained in RAM.
Split RAM entries around reservations, merge adjacent entries with the same
type, emit entries in ascending GPA order, reject arithmetic overflow, and
enforce the 128-entry zero-page capacity.

Boot metadata below 1 MiB may remain described as RAM where Linux already
reserves it during early boot. The shared-status page must be explicitly
reserved because it remains live after boot and is owned by the virtio
interrupt contract.

Preserve the current ACPI e820 output exactly for existing callers. This keeps
the change additive and avoids unrelated Linux-direct regressions.

## 5. Integrate the worker path

In
[`openvmm_core/src/worker/dispatch.rs`](../openvmm/openvmm/openvmm_core/src/worker/dispatch.rs),
split x86 Linux loading by `LinuxDirectBootMode`:

- `DeviceTree`: continue rejecting it on x86-64.
- `Acpi`: retain the existing ACPI and SMBIOS path unchanged.
- `MpTable`: require the microVM profile and call the new loader entry point.

For `MpTable`, the worker must collect APIC IDs from the full processor
topology, pass `MICROVM_LEVEL_TRIGGERED_IRQS`, and reserve
`MICROVM_SHARED_STATUS_PAGE_GPA..+MICROVM_SHARED_STATUS_PAGE_SIZE`. It must not
invoke `AcpiTablesBuilder` or build SMBIOS data.

Update
[`openvmm_core/src/worker/vm_loaders/linux.rs`](../openvmm/openvmm/openvmm_core/src/worker/vm_loaders/linux.rs)
with a dedicated wrapper or protocol-neutral platform parameter. Do not add a
boolean `is_microvm` to the low-level loader API; the explicit platform mode
should carry the behavior.

The TSC and LAPIC frequency command-line helpers currently live in the PVH
worker module even though they are microVM policy. Move them to a
protocol-neutral microVM helper, switch their caller to Linux `MpTable`, and
delete the PVH worker module after the transition.

## 6. Update configuration validation

Update
[`openvmm_defs/src/config.rs`](../openvmm/openvmm/openvmm_defs/src/config.rs) so
that:

- the standard profile rejects `LinuxDirectBootMode::MpTable`;
- fresh microVM configuration requires `LoadMode::Linux` with `MpTable`;
- legacy `LoadMode::Pvh` is accepted only when PVH compatibility is retained;
- the microVM command-line validator extracts the command line from either
  supported load variant during migration;
- MP-table microVM loading requires `LinuxIsolationConfig::None`;
- emulated serial remains disabled because the profile uses portb and optional
  virtio-console; and
- SMBIOS overrides are rejected rather than silently ignored.

Keep all existing processor topology, device inventory, MMIO layout, IRQ,
memory capacity, and hypervisor restrictions unchanged.

## 7. Change CLI and TTRPC construction

For the CLI, change the microVM branch in
[`openvmm_entry/src/lib.rs`](../openvmm/openvmm/openvmm_entry/src/lib.rs) to
construct:

```rust
LoadMode::Linux {
    kernel,
    initrd,
    cmdline,
    enable_serial: false,
    isolation: LinuxIsolationConfig::None,
    boot_mode: LinuxDirectBootMode::MpTable,
    smbios: Box::default(),
}
```

Continue using the profile-owned effective command-line builder. Generalize
all later command-line mutation and extraction sites that currently match only
- every microVM configuration requires `LoadMode::Linux` with `MpTable`;
- `LoadMode::Pvh` is removed from the configuration enum;
- the microVM command-line validator reads the `LoadMode::Linux` command line;
wire field is required when Linux-direct replaces PVH as the default. Update
[`openvmm_entry/src/ttrpc/mod.rs`](../openvmm/openvmm/openvmm_entry/src/ttrpc/mod.rs)
to stop rejecting that combination and to apply the microVM command-line
builder.

Remove the `PvhBoot` message and its `pvh_boot` oneof member from
[`vmservice.proto`](../openvmm/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto).
Reserve both field number 14 and the name `pvh_boot` in `VMConfig` so neither
can be reused accidentally. Remove all generated Rust match arms and update
the TTRPC tests to submit `DirectBoot` for a microVM.

## 8. Update Petri construction

In
[`petri/src/vm/openvmm/construct.rs`](../openvmm/petri/src/vm/openvmm/construct.rs),
stop rewriting a microVM `Firmware::LinuxDirect` artifact into
`LoadMode::Pvh`. Retain `LoadMode::Linux`, replace its command line with the
profile-owned value, and select `LinuxDirectBootMode::MpTable`.

The source-built `guest_test_pvh` binary is not a Linux boot-protocol image and
cannot validate this path. Replace its coverage with the existing x86
Linux-direct kernel and initrd artifacts, then remove the PVH-specific Petri
firmware variant and artifact declarations.

## 9. Version the snapshot boot layout

The snapshot contract currently calls mesh field 16 `pvh_layout_version`.
Generalize it to `boot_layout_version` while retaining mesh field number 16 so
the binary wire layout remains readable.

Define distinct values, for example:

| Boot layout version | Meaning |
| ---: | --- |
| `2` | Removed SMP PVH layout; unsupported by the final implementation |
| `3` | Linux-direct MP-table layout from this plan |

Update
[`openvmm_helpers/src/snapshot.rs`](../openvmm/openvmm/openvmm_helpers/src/snapshot.rs)
so `microvm_machine_contract()` accepts the boot layout identity and new
captures write version 3. Shape validation and exact contract comparison must
check the selected version.

The canonical snapshot RAM ranges do not need to change: the status page still
has RAM backing and only its guest e820 type changes. The layout version must
change because low-memory boot contents and initial entry state differ.

Reject boot-layout version 2 with a clear unsupported-layout diagnostic before
opening restore resources or starting a VP. Restore reconstructs an inert
Linux `MpTable` load mode from a version-3 contract; firmware loading remains
skipped during restore. No PVH loader or `LoadMode::Pvh` compatibility holder
remains in the final implementation.

A full microVM ABI version bump is not required if the device, memory, and
interrupt contracts remain unchanged; the dedicated boot-layout version
records the boot-state incompatibility. Existing PVH snapshots are
intentionally not restorable after full removal.

## 10. Test coverage

### Loader unit tests

Extend the tests in
[`vm/loader/src/linux.rs`](../openvmm/vm/loader/src/linux.rs) to cover:

- valid MP floating-pointer and configuration-table checksums;
- exact MP page placement and no overlap with the GDT;
- one, two, four, and eight contiguous APIC IDs;
- invalid APIC IDs, IRQs, entry counts, and oversized tables;
- no ACPI or SMBIOS page imports in MP-table mode;
- `boot_params.acpi_rsdp_addr == 0`;
- exact command-line placement at `0x20000`;
- a 65,535-byte command line succeeding with its NUL terminator;
- a 65,536-byte command line failing before any import;
- status-page and ISA-hole e820 reservations;
- split low/high RAM and e820-capacity failures; and
- the existing Linux initial registers, especially `RSI`, `CR3`, `CR0`,
  `CR4`, `EFER`, and `RIP`.

Retain all existing ACPI, SMBIOS, SNP, and command-line tests unchanged.

### Configuration tests

Add a validation matrix for:

- `Microvm + Linux/MpTable` accepted;
- `Standard + Linux/MpTable` rejected;
- `Microvm + Linux/Acpi` rejected;
- `Microvm + Linux/DeviceTree` rejected;
- MP-table boot with isolation rejected;
- SMBIOS overrides rejected; and
- malformed processor or device topology still rejected.

### VMM integration tests

Add an x86-64 MicroVM Linux-direct test that verifies:

- boot without an ACPI RSDP;
- Linux discovers all configured processors through MP tables;
- IOAPIC and legacy IRQ routing work;
- 1, 2, 4, and 8 vCPUs reach guest readiness;
- fixed virtio-mmio devices initialize from command-line discovery;
- the shared-status interrupt path works; and
- capture and restore preserve boot-layout version 3.

Add a negative restore test proving that removed boot-layout version 2 is
rejected before resource opening or VP startup.

## 11. NVX guest and tooling follow-up

The NVX kernel must retain:

- `CONFIG_ACPI=n`;
- `CONFIG_X86_MPPARSE=y`;
- `CONFIG_X86_LOCAL_APIC=y`;
- `CONFIG_X86_IO_APIC=y`;
- `CONFIG_VIRTIO_MMIO=y`; and
- `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y`.

Disable `CONFIG_PVH` to remove the unused entry code and Xen ELF note. Replace
the PVH note checks in
[`scripts/nvx_tools/build.py`](../scripts/nvx_tools/build.py) and
[`scripts/nvx_tools/release.py`](../scripts/nvx_tools/release.py) with checks
for the MP-table and virtio-mmio kernel requirements. Rename PVH-specific
artifact descriptions, tests, benchmark labels, and documentation.

## 12. Remove the PVH feature from OpenVMM

Full removal is part of this plan's definition of done, not an optional cleanup.
Perform it only after the Linux-direct MP-table path has equivalent cold-boot,
SMP, device, and snapshot coverage.

### Loader and register plumbing

- Delete [`vm/loader/src/pvh.rs`](../openvmm/vm/loader/src/pvh.rs) after moving
  its MP-table builder.
- Remove `pub mod pvh` from `vm/loader/src/lib.rs`.
- Remove the loader-only `X86Register::Rbx` variant and its IGVM conversion
  handling from `vm/loader/src/importer.rs`.
- Remove the corresponding `X86Register::Rbx` match arms from the SNP and TDX
  IGVM context builders, the OpenVMM IGVM register allowlist, and
  `vmm_core/vm_loader/src/initial_regs.rs`.
- Retain ordinary architectural RBX fields in VP state, VMSA, emulation, and
  hypervisor state code; only the loader register variant introduced for PVH
  handoff is removed.

The final loader must contain no Xen ELF-note parser, `hvm_start_info`, PVH
module list, PVH memory-map structures, PVH entry register state, or
PVH-specific placement constants.

### Worker and configuration

- Delete
  [`openvmm_core/src/worker/vm_loaders/pvh.rs`](../openvmm/openvmm/openvmm_core/src/worker/vm_loaders/pvh.rs)
  after moving the frequency helpers.
- Remove `pub mod pvh` from the worker loader module.
- Delete the PVH dispatch arm and its ACPI, memory-map, initramfs, and register
  setup from `openvmm_core/src/worker/dispatch.rs`.
- Remove `LoadMode::Pvh` from `openvmm_defs/src/config.rs` and update every
  exhaustive match.
- Remove PVH-only validation errors, command-line extraction branches, and
  user-facing diagnostics from `openvmm_defs`, `openvmm_core`, and
  `openvmm_entry`.
- Update the ACPI builder test that imports the MP builder through
  `loader::pvh` to use the new shared MP-table module.

### Management APIs and snapshots

- Remove `PvhBoot` and reserve protobuf field 14 and name `pvh_boot`.
- Remove all CLI, TTRPC, and controller branches that construct or inspect
  `LoadMode::Pvh`.
- Rename `pvh_layout_version` and `MICROVM_PVH_LAYOUT_VERSION` to protocol-neutral
  boot-layout names while retaining mesh field number 16.
- Remove success paths for snapshot boot-layout version 2 and retain only a
  deterministic unsupported-version error test.
- Replace PVH wording in snapshot errors, comments, manifest descriptions,
  and management documentation.

### Test guest, Petri, and Flowey

- Delete the `guest_test_pvh/` crate and remove it from the workspace and lock
  file.
- Remove `GUEST_TEST_PVH_X64`, `MicrovmTestPvh`, `microvm_test_pvh()`, known-path
  resolver support, and PVH-specific VMM test macros.
- Convert the microVM lifecycle and TTRPC snapshot tests to Linux-direct
  artifacts and rename their test functions and markers.
- Delete `flowey_lib_hvlite/src/build_guest_test_pvh.rs` and remove its module,
  request types, artifact variables, download wiring, content-directory copy,
  clippy package entry, and pipeline publication steps.
- Regenerate checked-in Flowey YAML and GitHub workflows with the repository's
  generator. Do not edit generated workflow files manually.

### Documentation and final audit

Update the OpenVMM Guide pages for Linux direct boot, CLI, TTRPC, run, and
snapshots so they describe only the ACPI-free Linux-direct MP-table microVM.
Remove PVH-specific terminology from code comments and test names unless it
appears in an intentional incompatibility or historical migration note.

As the final source audit, search tracked OpenVMM source, configuration, tests,
Flowey definitions, and Guide content for `PVH`, `Pvh`, `pvh`, `PvhBoot`,
`LoadMode::Pvh`, `guest_test_pvh`, and `X86Register::Rbx`. Remaining hits must
be either unrelated architectural RBX state or explicit release-history notes.

## Performance acceptance gate

PVH removal is blocked until Linux-direct MP-table boot demonstrates the same
or better cold-boot performance and process memory footprint as the PVH entry
implementation it replaces.

Capture the PVH baseline before deleting either path. The A/B comparison must
use the same commit, release build profile, uncompressed kernel, initramfs,
effective command line, device inventory, guest RAM, vCPU count, hypervisor,
host, host configuration, and affinity policy. Build the comparison kernel with
both entry points enabled so the boot protocol is the only intentional variable.
Interleave PVH and Linux-direct samples to reduce drift from host temperature,
scheduler load, and background activity.

For every required KVM, MSHV, and WHP platform and supported 1, 2, 4, and 8
vCPU configuration:

- run at least one warmup and ten measured samples per boot path;
- require the Linux-direct `openvmm_cold_start` p50 to be less than or equal to
  the matched PVH p50;
- require the Linux-direct `openvmm_cold_start_peak_rss` p50 to be less than or
  equal to the matched PVH p50;
- retain raw samples, host identity, build identifiers, and aggregate results
  as review artifacts; and
- treat a noisy or statistically inconclusive comparison as not passing and
  collect additional interleaved samples instead of widening the threshold.

Snapshot generation, snapshot restore, guest-exit teardown, and device-rate
metrics remain regression gates under the normal performance suite, but the
two cold-start metrics above are the protocol-replacement acceptance gates.
Any required platform that regresses blocks PVH removal until the regression is
fixed and the matched comparison is rerun.

After disabling `CONFIG_PVH` and deleting the OpenVMM PVH implementation, rerun
the Linux-direct measurements with the final artifacts. The final
`openvmm_cold_start` and `openvmm_cold_start_peak_rss` p50 values must still be
no worse than the archived matched PVH baseline.

## Implementation sequence

1. Extract the shared MP builder while keeping behavior unchanged.
2. Add the Linux MP-table loader entry point and exhaustive loader tests.
3. Add `LinuxDirectBootMode::MpTable` and worker dispatch support.
4. Generalize microVM frequency propagation and command-line extraction.
5. Switch CLI, TTRPC, and Petri fresh launches to Linux direct boot.
6. Generalize the snapshot field, write layout version 3, and reject version 2.
7. Add Linux MP-table VMM tests, including SMP and snapshot restore.
8. Run and pass the matched performance acceptance gate while both paths are
  temporarily available.
9. Remove the PVH loader, worker path, RBX loader plumbing, APIs, and snapshot
   compatibility.
10. Remove the PVH test crate, Petri artifacts, Flowey wiring, and generated
    workflow jobs.
11. Switch NVX kernel validation and runtime suites to the new path and disable
    `CONFIG_PVH`.
12. Update documentation, regenerate workflows, and run the final no-PVH audit.

Keep MP extraction, Linux loader enablement, profile switching, and full PVH
removal in separate pull requests. The final pull request in the series must
leave Linux direct boot as the only microVM cold-boot path.

## Validation

For each modified OpenVMM package, run the nested repository's required checks:

```text
cargo check -p <package>
cargo clippy --all-targets -p <package>
cargo doc --no-deps -p <package>
cargo nextest run --profile agent -p <package>
cargo xtask fmt --fix
```

Expected affected packages include `loader`, `openvmm_defs`, `openvmm_core`,
`openvmm_entry`, `openvmm_helpers`, `petri`, the VMM-test support crates,
`vmm_tests`, and the affected Flowey crates. Run focused VMM tests with
`cargo xflowey vmm-tests-run --filter "test(<test-name>)"`, then regenerate
Flowey outputs and verify that regeneration is clean.

For the NVX integration, first run `python scripts\nvx.py verify` on Windows or
`python3 scripts/nvx.py verify` on Linux, followed by the commands from
`.github/actions/check-quality/action.yml` and
`.github/actions/validate-nvx/action.yml`. Run the MicroVM SMP and snapshot
suites on each available KVM, MSHV, and WHP backend. Hardware-dependent lanes
that are unavailable locally remain CI requirements and must not be reported
as locally passed.

## Acceptance criteria

The implementation is complete when:

- a fresh microVM boots an uncompressed ELF through `LoadMode::Linux` and
  `LinuxDirectBootMode::MpTable`;
- no ACPI or SMBIOS data is imported;
- Linux discovers 1, 2, 4, and 8 vCPUs and the IOAPIC from MP tables;
- the existing fixed device, command-line, interrupt, and memory contracts pass;
- snapshot capture and restore reject mismatched boot-layout versions before
  starting a VP;
- existing standard ACPI Linux-direct behavior is unchanged; and
- measured results separately report protocol overhead from kernel and device
  profile savings;
- boot-layout version 2 snapshots fail with the documented unsupported error;
- OpenVMM exposes no PVH load mode or `PvhBoot` management API;
- the PVH loader, worker module, test guest, Petri artifacts, and Flowey jobs
  are absent; and
- the final source audit finds no PVH implementation references or
  loader-specific `X86Register::Rbx` plumbing;
- Linux-direct `openvmm_cold_start` p50 is no greater than the matched PVH p50
  on every required backend and vCPU configuration; and
- Linux-direct `openvmm_cold_start_peak_rss` p50 is no greater than the matched
  PVH p50 on every required backend and vCPU configuration, both before and
  after final PVH removal.
