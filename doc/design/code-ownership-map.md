# Code ownership map

[Design index](../design.md)

| Area | Primary implementation |
| --- | --- |
| Public profile, ABI constants, validation, command line | [`openvmm_defs/src/config.rs`](../../openvmm/openvmm/openvmm_defs/src/config.rs) |
| CLI and host attachment construction | [`openvmm_entry/src`](../../openvmm/openvmm/openvmm_entry/src) |
| Worker composition and fixed virtio placement | [`openvmm_core/src/worker`](../../openvmm/openvmm/openvmm_core/src/worker) |
| Restore-time VP materialization and saved-state filtering | [`openvmm_core/src/worker/dispatch.rs`](../../openvmm/openvmm/openvmm_core/src/worker/dispatch.rs) and [`vmm_core/src/partition_unit/vp_set.rs`](../../openvmm/vmm_core/src/partition_unit/vp_set.rs) |
| Restore-time RAM capacity, range selection, and split backing | [`openvmm_helpers/src/snapshot.rs`](../../openvmm/openvmm/openvmm_helpers/src/snapshot.rs), [`openvmm_core/src/worker/memory_layout.rs`](../../openvmm/openvmm/openvmm_core/src/worker/memory_layout.rs), and [`openvmm_core/src/worker/dispatch.rs`](../../openvmm/openvmm/openvmm_core/src/worker/dispatch.rs) |
| Xen PVH loading | [`vm/loader/src/pvh.rs`](../../openvmm/vm/loader/src/pvh.rs) |
| Minimal PVH ACPI construction | [`vmm_core/src/acpi_builder.rs`](../../openvmm/vmm_core/src/acpi_builder.rs) |
| Base-chipset allowlist and memory-layout defaults | [`vmm_core/vm_manifest_builder`](../../openvmm/vmm_core/vm_manifest_builder) |
| portb, shutdown, and snapshot PMIO | [`vm/devices/chipset/src/microvm.rs`](../../openvmm/vm/devices/chipset/src/microvm.rs) |
| RTC normalization | [`vm/devices/chipset/src/cmos_rtc.rs`](../../openvmm/vm/devices/chipset/src/cmos_rtc.rs) |
| Virtio device-private saved state | [`vm/devices/virtio`](../../openvmm/vm/devices/virtio) |
| Snapshot format, machine contract, publication, validation | [`openvmm_helpers/src/snapshot.rs`](../../openvmm/openvmm/openvmm_helpers/src/snapshot.rs) |
| Capture orchestration | [`openvmm_entry/src/vm_controller.rs`](../../openvmm/openvmm/openvmm_entry/src/vm_controller.rs) |
| Snapshot management exclusion | [`openvmm_core/src/worker/dispatch/snapshot_rpc.rs`](../../openvmm/openvmm/openvmm_core/src/worker/dispatch/snapshot_rpc.rs) |
| Portb process-exit output drain | [`openvmm_entry/src/microvm_output.rs`](../../openvmm/openvmm/openvmm_entry/src/microvm_output.rs) |
| Backend snapshot clocks | [`virt_kvm`](../../openvmm/vmm_core/virt_kvm), [`virt_mshv`](../../openvmm/vmm_core/virt_mshv), and [`virt_whp`](../../openvmm/vmm_core/virt_whp) |
| Sandbox launch and kernel features | [`scripts/nvx_tools/sandbox.py`](../../scripts/nvx_tools/sandbox.py) and [`kernel/config-microvm`](../../kernel/config-microvm) |
| Workload namespace and root construction | [`guest/alpine/nvx-container-launch`](../../guest/alpine/nvx-container-launch) and [`guest/alpine/nvx-container-enter`](../../guest/alpine/nvx-container-enter) |
| Guest workload, scratch quiesce, and post-restore CPU/RAM repair | [`guest/common/nvx-snapshot`](../../guest/common/nvx-snapshot) and [`guest/common/nvx-init-agent`](../../guest/common/nvx-init-agent) |
| Self-contained OpenVMM control-plane tests | [`guest_test_pvh`](../../openvmm/guest_test_pvh), [`vmm_tests/tests/tests/x86_64/microvm.rs`](../../openvmm/vmm_tests/vmm_tests/tests/tests/x86_64/microvm.rs), and [`vmm_tests/tests/tests/ttrpc.rs`](../../openvmm/vmm_tests/vmm_tests/tests/tests/ttrpc.rs) |
| NVX Linux and device integration tests | [`scripts/nvx_tools/microvm_tests.py`](../../scripts/nvx_tools/microvm_tests.py) and [`scripts/nvx_tools/microvm_test_scripts`](../../scripts/nvx_tools/microvm_test_scripts) |
