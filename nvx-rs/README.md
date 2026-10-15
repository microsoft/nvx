# nvx-rs

`nvx-rs` is the Rust crate that holds the NVX mechanics: the OpenVMM
control protocol, and the launch, policy lowering, and supervision of NVX
microVMs. Its library is called `nvx`. The
[`aci_edge_sandboxes` crate](../aci_edge_sandboxes/README.md) builds on it and
re-exports it as `aci_edge_sandboxes::nvx`, and later phases of
[microsoft/nvx#356](https://github.com/microsoft/nvx/issues/356) add the guest
programs and a host CLI to it.

An NVX sandbox is a microVM that OpenVMM runs with the managed lifecycle: the
guest's init starts a managed agent, which runs the workloads that the host
sends it over an authenticated control console. The crate is not published to
a registry, and has a `Cargo.lock` of its own; a workspace at the repository
root would interact with the nested `openvmm/` workspace.

## Layout

```text
nvx-rs/
├── src/
│   ├── protocol/    NVXS records, NVXC frames, EXEC and MAPS payloads, guest features, and
│   │                kernel command-line tokens; no dependencies, shared by the host and the guest
│   ├── contract.rs  control-contract versions and release-manifest compatibility
│   ├── net/         CIDR parsing and subtraction, and hostname and guest-network forms
│   ├── hypervisor.rs
│   ├── error.rs     nvx::Error and its ErrorKind
│   ├── host/        feature `host` (default): artifacts, launch, network, filesystem, session,
│   │                process, platform, VM instances, and outcome reports
│   ├── testing/     feature `testing`: the fake OpenVMM
│   ├── guest/       feature `guest`: the programs that run in Linux guests
│   └── bin/         nvx-fake-openvmm and the guest binaries
├── build.rs         bundled-release staging
└── artifacts.json   the NVX release that the `bundled` feature stages
```

| Module | Contents |
| --- | --- |
| `nvx::protocol` | The codec of the control console: `NVXS` outer records, `NVXC` application frames, the `EXEC` payload that the host encodes and the guest agent decodes, the `MAPS` payloads of the host mapping table, `GuestFeatures`, and the `nvx_maps=` and `nvx_workload_account=create` tokens. Golden byte fixtures pin every record independently of the codec, which the host and the fake OpenVMM share |
| `nvx::contract` | The OpenVMM microVM ABI, broker protocol, and control-contract revision that the crate speaks, and `manifest_is_compatible` for a release's `SOURCE-MANIFEST.json` |
| `nvx::net` | `Cidr`: IPv4 and IPv6 networks and their subtraction; `is_valid_hostname` and `is_valid_guest_network`, the forms of a VM's hostname and guest address |
| `nvx::Hypervisor` | KVM, MSHV, or WHP, with its serde form behind the `serde` feature |
| `nvx::host::artifacts` | `Artifacts` (also `nvx::Artifacts`): the OpenVMM executable, guest kernel, and Alpine initramfs, from a release, a repository checkout, the environment, a directory next to the executable, or the bundle |
| `nvx::host::launch` | `LaunchSpec` and OpenVMM's command line: the managed lifecycle, the `WorkloadIdentity`, the 1024-byte kernel command line, its reserved parameters, the 32,767-character limit of a Windows command line, and `LaunchSpec::check`, which every command line applies to the memory size, the non-root identity, the hostname, the guest network, and the kernel parameters |
| `nvx::host::network` | Egress rules and their exact expansion into OpenVMM's IPv4 and IPv6 rules: exceptions, protocol-only rules, and port ranges, with at most 256 rules for each action |
| `nvx::host::filesystem` | The `ExportPlan` of host paths through OpenVMM's aggregate virtio-fs device, the host-mapping table, the reserved guest targets, and `guest_path` and `resolve_guest_path` (also at `nvx::`) |
| `nvx::host::instance` | `Instance`: a VM in a directory of its own. It starts OpenVMM with a fresh capability and waits until the guest is ready and has mounted the mapped paths, reconciles recorded state, recovers interrupted launches, runs workloads and pumps their output, and stops the VM with a forced fallback |
| `nvx::host::session`, `process`, `platform`, `report` | The authenticated control session, OpenVMM processes, Linux and Windows primitives, and OpenVMM's outcome report |
| `nvx::testing::fake_openvmm` | A fake OpenVMM that serves the control console and runs scripted workloads without a VM |
| `nvx::guest` | The programs that run in Linux guests; see [Guest binaries](#guest-binaries) |

Lowering functions take the names by which messages refer to their inputs, such
as `EgressRules::name` and `PathList::name`, so callers' messages name their own
fields.

## Features

| Feature | Contents |
| --- | --- |
| `host` (default) | The `host` module. It enables `serde` |
| `bundled` | Stages a release at build time; see [Bundled artifacts](#bundled-artifacts) |
| `testing` | The fake OpenVMM and the `nvx-fake-openvmm` binary |
| `serde` | The serde form of `Hypervisor` |
| `guest` | The `guest` module and the guest binaries; see [Guest binaries](#guest-binaries) |

Without default features, the crate has only `protocol`, `contract`, `net`,
`Hypervisor`, and `Error`, and depends on nothing but `serde` when `serde` is
enabled and `nix` and `getrandom` when `guest` is; `aci_edge_sandboxes_model`
uses it that way.

## Example

```rust,no_run
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use nvx::host::instance::{self, Instance};
use nvx::host::launch::LaunchSpec;
use nvx::protocol::{Workload, WorkloadEnvironment};
use nvx::{Artifacts, Hypervisor};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let spec = LaunchSpec::new(Artifacts::discover()?, Hypervisor::from_env_or_default()?);
    let directory = std::env::temp_dir().join("nvx-example");
    instance::create_private_dir(&directory)?;
    let instance = Instance::new(&directory, "example");
    let started = instance.start(&spec, Duration::from_secs(60))?;

    let workload = Workload {
        argv: vec!["/bin/echo".to_owned(), "hello".to_owned()],
        timeout_ms: 0,
        cwd: None,
        environment: WorkloadEnvironment::Default,
    };
    let timeout = Duration::from_secs(60);
    let capability = instance.read_capability()?;
    let execution = instance.exec(&started.runtime, &capability, &workload, timeout, timeout)?;
    let mut stdout = Vec::new();
    let outcome = execution.pump(
        &mut |chunk: &[u8]| {
            stdout.extend_from_slice(chunk);
            Ok(())
        },
        &mut |_: &[u8]| Ok(()),
        &AtomicBool::new(false),
    )?;
    println!("{outcome:?}: {}", String::from_utf8_lossy(&stdout));
    instance.stop(&started.runtime, Duration::from_secs(30))?;
    Ok(())
}
```

## Errors

Every fallible operation returns `nvx::Error`: a message that callers can show
as is, an `ErrorKind`, and an optional cause.

| `ErrorKind` | Meaning |
| --- | --- |
| `Unavailable` | A missing or unusable artifact, hypervisor, or release, a guest image without a required control feature, or an unsupported host |
| `Unsupported` | A well-formed request that NVX cannot represent or enforce, such as a policy beyond a limit of OpenVMM or the guest agent |
| `Exited` | The VM's OpenVMM process exited |
| `Io` | A host I/O failure, or state that NVX recorded on the host and cannot read |
| `Protocol` | A control session that failed, or a request that the guest agent refused |
| `TimedOut` | A deadline that passed |
| `Other` | Any other failure |

Messages call an instance `sandbox NAME`, after the name that `Instance::new`
gives it. `aci_edge_sandboxes` maps the kinds onto its error codes; see its
[error mapping](../aci_edge_sandboxes/README.md#error-mapping).

## Bundled artifacts

With the `bundled` feature, the build script stages the OpenVMM executable,
guest kernel, and Alpine initramfs of a release, with its
`SOURCE-MANIFEST.json` and `SHA256SUMS`, in its output directory, and
`Artifacts::bundled` returns them. `Artifacts::discover` uses the bundle when no
other source is present.

| Build variable | Effect |
| --- | --- |
| `NVX_BUNDLE_DIR` | Stages the extracted release in this directory, for offline builds or local artifacts |
| `NVX_BUNDLE_HYPERVISOR` | `kvm` (default) or `mshv`: the Linux package to download |
| `NVX_BUNDLE=skip` | Stages nothing, as when the feature is disabled; CI sets it for `--all-features` builds |

Without `NVX_BUNDLE_DIR`, the build script downloads the release package that
[`artifacts.json`](artifacts.json) pins with `curl` and extracts it with `tar`;
Windows packages are ZIP archives, which need bsdtar (Windows' own `tar`) or
`unzip`. Every staged file must match the source's `SHA256SUMS`, a download
must match its pinned digest, and the release must declare the control contract
of `nvx::contract`. The build scripts of dependents receive the staged
directory as `DEP_NVX_ARTIFACTS_DIR` and can ship it next to their executables
as `nvx/`.

## Guest binaries

The `guest` feature adds the programs that run in NVX's Linux guests: the
`nvx::guest` module and one binary per program. The Alpine, Ubuntu, and Azure
Linux images ship all nine at their established `/sbin/nvx-*` paths.

| Binary | Program |
| --- | --- |
| `nvx-console-pending DEVICE` | Prints the number of bytes waiting to be read from a device, such as a console |
| `nvx-mmio-write ADDRESS VALUE` | Stores a 32-bit value at a physical address through `/dev/mem` (x86_64) |
| `nvx-reseed [--generation-only] SEED_FILE [PREVIOUS]`, `nvx-reseed --sample` | Reseeds the kernel's random number generator from a seed file and checks its VM generation ID, or prints a random sample |
| `nvx-exit [CODE]` | Stops the VM with an 8-bit exit code through OpenVMM's control port 0x604 (x86_64) |
| `nvx-device-io file ...`, `nvx-device-io network ...` | The benchmark helper that measures file and UDP round-trip I/O and prints one JSON line (x86_64) |
| `nvx-managed-agent CONTROL_TTY ROOTFS HOSTNAME UID GID USER HOME` | Serves the managed control session and supervises sequential workloads; `--exec-config-fd FD -- WORKLOAD ...` is its internal launch helper |
| `nvx-ingress GATEWAY_IPV4` | Announces reachable TCP listeners and settled UDP sockets to the portable-network gateway; `test DIRECTORY` runs synthetic procfs snapshots |
| `nvx-time [--report-only] COMMAND` | Enforces NVX time ABI v1: runs its conformance checks, repairs the wall clock after a restore, publishes the time state, and runs the discipline and violation-watcher daemon; `test NAME ...` runs its decoder and process test hooks (x86_64) |
| `nvx-time-probe latency\|warp\|clock ...` | Diagnoses clock-read latency, cross-CPU TSC warp and offset, the clocks, CPUID, clock events, kernel messages, and optionally MSRs (x86_64) |

Each binary's command line, exit statuses, and byte output are compatibility
contracts; the [`nvx::guest`](src/guest/mod.rs) documentation describes each
program. Guests run them as static `x86_64-unknown-linux-musl` executables. On
Linux, with the musl target and a C compiler driver, `cc`, `nvx.py` builds them:

```bash
rustup target add x86_64-unknown-linux-musl
python3 scripts/nvx.py build-guest-binaries \
  --output build/nvx-rs-guest-binaries
```

Native guest builds, the digest-pinned Docker Rust stage, and CI all call this
routine. Docker builds the set once and installs identical bytes in all three
guest images; Ubuntu determinism checks rebuild from fresh targets.

## Testing

```bash
cd nvx-rs
NVX_BUNDLE=skip cargo test --all-features   # unit, golden-fixture, and fake-OpenVMM tests
```

The tests need no hypervisor. The fake-OpenVMM test drives `Instance` end to end
against `nvx-fake-openvmm`, and shared protocol definitions plus golden byte
fixtures cover both sides of every record. On Linux, a loopback test drives the
host `ControlSession` through a broker shim into the real managed agent's
session, dispatch, and execution engine, and `nvx.py` also lints and tests the
guest binaries, and runs their integration tests against a build of them:

```bash
python3 ../scripts/nvx.py check-guest-binaries
python3 ../scripts/nvx.py test-guest-binaries \
  --binaries ../build/nvx-rs-guest-binaries
```

The
[`check-nvx-rs`](../.github/actions/check-nvx-rs/action.yml) action runs
rustfmt, Clippy with warnings denied for every feature and for the library
without default features, the tests, a synthetic-release `bundled` test,
rustdoc with warnings denied, the minimum supported Rust version (1.89), and a
macOS check. On Linux, it also lints, builds, and tests the guest binaries
through `nvx.py`. `python3 scripts/nvx.py test-aci-edge-sandboxes --backend kvm`
exercises the crate on a real hypervisor through `aci_edge_sandboxes`.
