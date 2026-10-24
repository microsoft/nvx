# Template manifests

Each directory holds the `manifest.bin` of a template's capture, as OpenVMM
writes it: one `openvmm.snapshot.SnapshotManifest` of version 6 of OpenVMM's
snapshot format, in the protobuf wire format of its `mesh`. The tests of
`nvx::host::template` read them with `TemplateManifest::read`, so that the
reader is checked against bytes that OpenVMM's own code wrote, rather than
against its own idea of them.

| Directory | Capture |
| --- | --- |
| [`platform-kvm`](platform-kvm) | A KVM microVM of 2 processors and 512 MiB of RAM, of a capacity of 2 GiB, with a `distro` of 1 GiB, a fresh scratch of 64 MiB, the dormant filesystem slot, a console, a control console, and no NIC |
| [`platform-mshv-network`](platform-mshv-network) | The same on MSHV, with a fixed NIC at `10.0.0.2/24`, its IPv6 identity, and an allow-list policy |

Both are platform-tier captures for clones, after the invariant configuration
alone, with OpenVMM's test records of the time contract and of the CPU profile
`intel.icelake-sp.v1`.

## How they were made

[`generate.rs`](generate.rs) builds each manifest as a capture does, with
`microvm_machine_contract` of OpenVMM's `openvmm_helpers`, checks it with
OpenVMM's own validation, and encodes it with `mesh`. It wrote these files at
OpenVMM commit `79a501b63fcd2f150e94928d0ceba79cd2fe96c4`.

To write them again, as when a new version of the snapshot format retires these,
append `generate.rs` to `openvmm/openvmm_helpers/src/snapshot/microvm.rs` in a
scratch checkout of the pinned OpenVMM, and run, from that checkout's root:

```bash
cargo xflowey restore-packages --no-compat-igvm
NVX_FIXTURE_DIR=/tmp/fixtures cargo test --locked -p openvmm_helpers --lib \
  nvx_fixture -- --ignored
```

Then copy each `NAME.bin` to `NAME/manifest.bin` here, and record the commit
above.
