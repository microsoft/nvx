# Command-line usage

`scripts/nvx.py` is the supported command-line entry point for building,
running, benchmarking, and packaging NVX. Run it from the repository root with
Python 3.10 or newer:

```text
python3 scripts/nvx.py COMMAND [OPTIONS]
```

The examples on this page use the POSIX spelling. On Windows, use
`python scripts\nvx.py` instead. Every command accepts `-h` or `--help`,
including nested commands:

```console
python3 scripts/nvx.py --help
python3 scripts/nvx.py run --help
python3 scripts/nvx.py performance gate --help
```

## Commands

| Command | Description |
| --- | --- |
| `init` | Initialize the OpenVMM submodule and its nested submodules. |
| `build-guest` | Build the Linux kernel and selected guest artifacts. |
| `build-kernel` | Build the pinned and patched Linux kernel natively. |
| `build-initramfs` | Build the selected Alpine, Ubuntu, or Azure Linux initramfs. |
| `build-distro-layer` | Build a deterministic Ubuntu EROFS distro layer. |
| `verify-guest-determinism` | Rebuild Ubuntu guest artifacts twice and compare SHA-256 values. |
| `build-openvmm` | Build the OpenVMM release binary. |
| `record-openvmm-provenance` | Bind an existing OpenVMM binary to the pinned source revision. |
| `materialize-kernel-provenance-inputs` | Write kernel provenance inputs from raw run-head blobs. |
| `setup-cross-os-cache` | Install GNU tar and zstd for GitHub Actions cross-OS caches. |
| `check-required-ci` | Validate required GitHub Actions job results. |
| `test-openvmm-unit` | Run the OpenVMM workspace unit and documentation tests. |
| `test-openvmm` | Run self-contained OpenVMM microVM control-plane tests. |
| `test-microvm` | Run NVX Linux and device correctness tests through OpenVMM. |
| `test-aci-edge-sandboxes` | Run the `aci_edge_sandboxes` Rust crate lifecycle test on a real hypervisor. |
| `test-adversarial` | Run a brokered Copilot-driven adversarial campaign. |
| `build` | Build the guest artifacts and OpenVMM. |
| `download` | Download and install the latest matching GitHub release. |
| `run` | Run an OpenVMM microVM. |
| `sandbox` | Run or manage workloads over EROFS layers and private ext4 scratch. |
| `benchmark` | Run the OpenVMM-native benchmark coordinator. |
| `performance` | Collect, gate, and persist CI performance results. |
| `collect-sources` | Materialize verified Linux, Alpine, and Ubuntu release sources. |
| `collect-alpine-sources` | Collect exact Alpine recipes and upstream sources. |
| `collect-ubuntu-sources` | Collect exact Ubuntu source packages. |
| `create-linux-source-archive` | Create a Linux corresponding-source archive. |
| `package` | Stage a binary distribution. |
| `archive-release` | Create a deterministic archive from a staged distribution. |
| `verify` | Verify source and submodule inputs. |

## Initialization and verification

### `init`

```console
python3 scripts/nvx.py init
```

Initializes and recursively updates the OpenVMM Git submodule.

### `verify`

```console
python3 scripts/nvx.py verify
```

Verifies the repository's pinned source inputs and submodule state.

### `setup-cross-os-cache`

```console
python3 scripts/nvx.py setup-cross-os-cache
```

Installs the GNU tar and zstd tools used by GitHub Actions cross-OS caches.

### `check-required-ci`

```text
python3 scripts/nvx.py check-required-ci
    --event-name {pull_request,push}
    --same-repository {false,true}
    --run-tests VALUE
    --run-workloads VALUE
```

Validates the GitHub Actions result values supplied through the
`QUALITY_RESULT`, `CHANGES_RESULT`, and job-specific `*_RESULT` environment
variables. CI supplies `true` or `false` for the two workload flags. The
command expects successful results for jobs enabled by the event, repository,
and workload flags, and `skipped` for jobs that are not enabled; mismatches
are reported as errors and cause a nonzero exit.

See [Setup](setup.md) for host prerequisites.

### `materialize-kernel-provenance-inputs`

```console
python3 scripts/nvx.py materialize-kernel-provenance-inputs
```

Recreates the tracked kernel configuration and patch files from the current
Git run head. It removes stale worktree patches that are absent from that
revision and is used before packaging kernel provenance inputs.

## Build commands

### `build-guest`

```text
python3 scripts/nvx.py build-guest
    [--guest {alpine,ubuntu,azurelinux,all}]
    [--native]
```

By default, builds the guest kernel and initramfs with Docker. `--native`
builds the selected artifacts directly on Linux instead. Alpine is the
default. Azure Linux does not support `--native` and always builds through
Docker. `--guest all` also builds the Ubuntu EROFS distro layer.

### `build-kernel`

```console
python3 scripts/nvx.py build-kernel
```

Fetches, verifies, patches, and builds the pinned kernel directly on Linux.

### `build-initramfs`

```console
python3 scripts/nvx.py build-initramfs [--guest {alpine,ubuntu,azurelinux}]
```

Builds the selected initramfs. Alpine and Ubuntu build directly on Linux; Azure
Linux uses Docker. Alpine is the default.

### `build-distro-layer`

```text
python3 scripts/nvx.py build-distro-layer
    --guest ubuntu
    [--output PATH]
    [--replace]
```

Builds the immutable Ubuntu EROFS `distro` layer directly on Linux. The default
output is `build/ubuntu-distro.erofs`. Existing output or manifest files are
rejected unless `--replace` is present.

### `verify-guest-determinism`

```text
python3 scripts/nvx.py verify-guest-determinism
    --guest ubuntu
    [--work-dir PATH]
```

Currently supports only `--guest ubuntu`. It builds the Ubuntu initramfs and
EROFS layer twice from separate roots and compares every artifact SHA-256. A
mismatch reports the first differing normalized rootfs entry when one exists.

### `build-openvmm`

```text
python3 scripts/nvx.py build-openvmm [--skip-restore] [--backend {kvm,mshv,whp}]
```

Builds the `openvmm` release binary. Before building, the command runs
`cargo xflowey restore-packages`; use `--skip-restore` when those packages are
already restored. Without `--backend`, Windows builds the native MSVC target
and Linux builds the native GNU target. On Linux, `--backend kvm` selects GNU
and `--backend mshv` selects musl; Windows accepts `--backend whp`. Build-target
selection does not probe `/dev/kvm` or `/dev/mshv`, so compilation also works
on build-only hosts and hosts exposing both devices. Unsupported OS/backend
combinations are rejected.

### `record-openvmm-provenance`

```console
python3 scripts/nvx.py record-openvmm-provenance
```

Records the checked-out OpenVMM revision, clean-state flag, and executable
SHA-256 for the existing `openvmm/target/release/openvmm[.exe]` binary in
`build/openvmm.provenance.json`. The command takes no options. It requires
the OpenVMM submodule to be checked out/initialized and the release binary to
already exist at `openvmm/target/release/openvmm` on POSIX or
`openvmm/target/release/openvmm.exe` on Windows. The command fails if either
prerequisite is missing.

### `build`

```text
python3 scripts/nvx.py build
    [--guest {alpine,ubuntu,azurelinux,all}]
    [--native]
    [--skip-restore]
    [--backend {kvm,mshv,whp}]
```

Runs `build-guest` followed by `build-openvmm`. The options have the same
meaning as on those individual commands.

See [Build](build.md) for dependencies, outputs, and native build details.

## Test commands

### `test-openvmm-unit`

```console
python3 scripts/nvx.py test-openvmm-unit
```

Runs the OpenVMM workspace's unit-test binaries with cargo-nextest's `agent`
profile and the `ci` feature. Packages that require specialized test harnesses
are excluded, along with all fuzz crates reported by OpenVMM's `xtask`.
Afterward, runs the workspace doctests with Cargo.

### `test-openvmm`

```text
python3 scripts/nvx.py test-openvmm --backend {kvm,mshv,whp}
```

Builds and runs OpenVMM's checkout-owned VMM tests. The Linux-direct microVM
TTRPC test boots NVX's `build/vmlinux` and `build/initramfs.cpio.gz`, so build
the guest first; the remaining test artifacts are produced by OpenVMM itself.

### `test-microvm`

```text
python3 scripts/nvx.py test-microvm
    --backend {kvm,mshv,whp}
    [--guest {alpine,ubuntu,azurelinux}]
    [--scenario SCENARIO]...
    [--processors {1,2,4,8} ...]
    [--memory-mib MIB]
    [--timeout SECONDS]
    [--output-dir PATH]
```

Runs NVX-owned Linux, SMP, virtio, sandbox, and snapshot correctness scenarios
against the public OpenVMM CLI. Repeat `--scenario` to select a subset; without
it, every scenario supported by the selected guest runs. Alpine remains the
default. Ubuntu and Azure Linux cannot act as sandbox control, so they reject
the Alpine-control-only `sandbox-blocks` and `scratch-snapshot` scenarios and
the sandbox-control-dependent `snapshot-tiers` scenario. Ubuntu also rejects
the Alpine-prompt-specific `console-snapshot` scenario. The
command requires `build/vmlinux`, the selected initramfs, and
`openvmm/target/release/openvmm[.exe]`.

### `test-aci-edge-sandboxes`

```text
python3 scripts/nvx.py test-aci-edge-sandboxes
    --backend {kvm,mshv,whp}
    [--output-dir PATH]
    [--cargo CARGO]
```

Runs the ignored `openvmm_e2e` tests of the [`aci_edge_sandboxes` crate](../aci_edge_sandboxes/README.md)
against a real hypervisor. The tests start sandboxes with `build/vmlinux` and
the Alpine initramfs, whose userland the workloads use directly, and run
workloads. They check the workload identity, timeouts, and cancellation, that
guest state lasts until a stop, and that a start terminates the VM of an
interrupted earlier start before deprovisioning. They also map temporary host
directories read-only, read-write, and denied, and check egress defaults and
rules against the network gateway, which needs no Internet access. The command requires
Cargo and `openvmm/target/release/openvmm[.exe]`, and writes the OpenVMM log to
`--output-dir` (default `build/test-results/aci_edge_sandboxes`) when the test fails.
The sandboxes keep their state in a temporary directory, which the command deletes
only once every sandbox has been deprovisioned. If the test is interrupted or fails
to stop or deprovision a sandbox, the command keeps the directory and names each
remaining sandbox with the OpenVMM process ID that may still run.

### `test-adversarial`

```text
python3 scripts/nvx.py test-adversarial
    --backend {kvm,mshv,whp}
    --campaign {workload-isolation,guest-isolation,snapshot-isolation}
    [--budget-seconds SECONDS]
    [--budget-actions COUNT]
    [--budget-ai-credits CREDITS]
    [--seed SEED]
    [--output-dir PATH]
    [--replay ACTIONS.jsonl]
    [--model MODEL]
    [--host-type {baremetal,virtual-machine}]
    [--memory-mib MIB]
    [--phase-timeout SECONDS]
    [--action-timeout SECONDS]
    [--executor-command PATH]
    [--no-minimize]
    [--minimize-attempts COUNT]
```

Runs a bounded adaptive campaign in which an already installed and
authenticated Copilot CLI selects one deterministic primitive at a time.
Copilot has no tools or direct NVX access. The typed broker validates and
records every action, while a credential-free executor and independent
watchdog own VM operation, canaries, teardown checks, and the clean
post-campaign boot.

| Option | Default | Description |
| --- | --- | --- |
| `--backend` | required | Select KVM or MSHV on Linux, or WHP on Windows. |
| `--campaign` | required | Select workload, privileged-guest, or snapshot boundary probes. |
| `--budget-seconds` | `900` | Bound preflight, actions, canary boot, and minimization wall time. |
| `--budget-actions` | `8` | Bound accepted and executed broker actions. |
| `--budget-ai-credits` | `300` | Bound charged Copilot usage; the minimum is 60 credits so authentication preflight and at least one action each retain a 30-credit CLI cap. |
| `--seed` | `0` | Seed deterministic candidate ordering and synthetic canary data. |
| `--output-dir` | backend-specific path under `build/test-results` | Parent for a unique campaign run directory. |
| `--replay` | none | Replay an `actions.jsonl` bound to its sibling `replay-manifest.json`, without invoking Copilot. |
| `--model` | Copilot auto routing | Select the strategist model. |
| `--host-type` | `NVX_HOST_TYPE` or `unspecified` | Record whether the executor is bare metal or a virtual machine. |
| `--memory-mib` | `256` | Set memory for deterministic microVM scenarios. |
| `--phase-timeout` | `60` | Set each underlying deterministic scenario phase timeout. |
| `--action-timeout` | `600` | Set the outer limit for one action or canary boot. |
| `--executor-command` | local child executor | Select one trusted no-argument wrapper for a separate disposable target. |
| `--no-minimize` | off | Disable fresh-target shorter-prefix replay after an anomaly. |
| `--minimize-attempts` | `3` | Bound shorter-prefix attempts within the campaign time budget. |

Missing or unauthenticated Copilot CLI is a preflight failure in adaptive
mode. Replay mode has no Copilot prerequisite. Production and CI campaigns
must use a separate disposable executor through `--executor-command`; local
mode cannot reliably classify a target-host crash. Local target logs are kept
under the short `build/adv` state root to avoid Windows path-length failures;
the run summary records their absolute location. See
[Copilot-driven adversarial testing](design/copilot-adversarial-testing.md)
for the trust boundary, wrapper contract, artifacts, and CI policy.

## Download and run

### `download`

```text
python3 scripts/nvx.py download
    [--repository OWNER/REPOSITORY]
    [--hypervisor {auto,whp,kvm,mshv}]
```

| Option | Default | Description |
| --- | --- | --- |
| `--repository OWNER/REPOSITORY` | `microsoft/nvx` | GitHub repository from which to download the latest release. |
| `--hypervisor {auto,whp,kvm,mshv}` | `auto` | Select the release platform. `auto` chooses WHP on Windows and KVM on Linux. |

Installing a release replaces the packaged guest artifacts under `build/` and
removes any known guest artifact that the release does not declare, such as the
Azure Linux guest that source-inclusive packages omit.

Windows release downloads support WHP. Linux release downloads support KVM
and MSHV. `download` first uses `GH_TOKEN` or `GITHUB_TOKEN` when configured.
If GitHub rejects that token with HTTP 401 or 403, NVX reports the failure and
retries without credentials so public releases remain downloadable. Private
repositories require a token with read access to the repository contents that
is authorized for the organization when it enforces single sign-on.

### `run`

```text
python3 scripts/nvx.py run
    [--guest {alpine,ubuntu,azurelinux}]
    [--hypervisor {auto,whp,kvm,mshv}]
    [--machine {microvm}]
    [--memory-mib MIB]
    [--memory-capacity-mib MIB]
    [--processors {1,2,4,8}]
    [--mount GUEST_TARGET,HOST_PATH[,ro|rw]]
    [--mount-deny HOST_PATH]...
    [--net IPV4/PREFIX]
    [--network-profile {portable}]
    [--network-egress {allow,deny}]
    [--network-ingress {allow,deny}]
    [--network-egress-allow CIDR[:PROTOCOL:PORT]]...
    [--network-egress-deny CIDR[:PROTOCOL:PORT]]...
    [--network-egress-policy-file PATH]
    [--host-loopback {allow,deny}]
    [--network-proxy IPV4:TCP-PORT]
    [--host-loopback-forward PROTOCOL:HOST_PORT:GUEST_PORT]...
    [--outcome-report PATH]
    [--cmdline TEXT]
    [--restore-snapshot PATH]
    [--restore-processors {1,2,4,8}]
    [--restore-memory-mib MIB]
    [--restore-ready-path PATH]
    [--dry-run]
```

| Option | Default | Description |
| --- | --- | --- |
| `--guest {alpine,ubuntu,azurelinux}` | `alpine` | Select Alpine, Ubuntu, or Azure Linux userland with the same NVX kernel. This option is not used for snapshot restore. |
| `--hypervisor {auto,whp,kvm,mshv}` | `auto` | Select the OpenVMM hypervisor. `auto` chooses WHP on Windows and KVM elsewhere. |
| `--machine {microvm}` | `microvm` | Select the fixed-topology microVM with shared-status edge interrupts. |
| `--memory-mib MIB` | guest-specific | Set guest memory in MiB. Defaults to 128 for Alpine, 256 for Ubuntu, and 512 for Azure Linux. |
| `--memory-capacity-mib MIB` | none | Reserve an immutable, 128 MiB-aligned RAM capacity for a fresh microVM snapshot. |
| `--processors {1,2,4,8}` | `1` | Select the microVM processor count. |
| `--mount GUEST_TARGET,HOST_PATH[,ro\|rw]` | none | Expose one host directory to the absolute guest target. An `rw` mapping accepts guest-created symbolic links, which the host never follows. Active snapshot restore requires the same canonical path, target, and mode; a dormant-slot restore may attach a new mapping that the resumed guest mounts explicitly. |
| `--mount-deny HOST_PATH` | none | Hide one existing file or directory inside the mounted host root; repeat to deny multiple paths. |
| `--net IPV4/PREFIX` | none | Enable virtio-net with the static guest IPv4 address and prefix. |
| `--network-profile {portable}` | none | Select the required cross-platform network behavior contract; must be specified with `--net`. |
| `--network-egress {allow,deny}` | `allow` | Set the default guest egress policy. |
| `--network-ingress {allow,deny}` | `deny` | Set the default host ingress policy. The portable profile currently supports only `deny`; `allow` is rejected before launch. |
| `--network-egress-allow CIDR[:PROTOCOL:PORT]` | none | Allow matching guest egress; repeat to add rules. |
| `--network-egress-deny CIDR[:PROTOCOL:PORT]` | none | Deny matching guest egress; repeat to add rules. Deny rules take precedence. |
| `--network-egress-policy-file PATH` | none | Load bounded IPv4 ranges and rule-local CIDR exclusions from JSON. Requires explicit `--network-egress`; cannot be mixed with explicit allow/deny rule flags. |
| `--host-loopback {allow,deny}` | existing mapping | Control guest access to host loopback services. |
| `--network-proxy IPV4:TCP-PORT` | none | Allow one explicit host TCP proxy endpoint. |
| `--host-loopback-forward PROTOCOL:HOST_PORT:GUEST_PORT` | none | Publish one TCP or UDP localhost port to the guest; repeat to add forwards. |
| `--outcome-report PATH` | none | Write a bounded local JSON outcome report. |
| `--cmdline TEXT` | empty | Append kernel parameters; `nvx_*` and `tsc=` tokens are reserved. |
| `--restore-snapshot PATH` | none | Restore the immutable machine contract and saved state from a snapshot directory. |
| `--restore-processors {1,2,4,8}` | none | Bring this contiguous processor prefix online before restore readiness. Requires an opt-in microVM snapshot and cannot exceed `--processors` capacity. |
| `--restore-memory-mib MIB` | none | Select the 128 MiB-aligned RAM target for an expansion-capable snapshot restore. |
| `--restore-ready-path PATH` | none | Publish one restore-readiness event to an existing Unix socket or Windows named pipe. |
| `--dry-run` | off | Print the generated OpenVMM command without running it. |

The command requires the OpenVMM release binary, `build/vmlinux`, and the
selected `build/initramfs*.cpio.gz`. Ubuntu selection never falls back to
Alpine. See [Run](run.md) for host setup, guest shutdown, networking, and
virtio-fs examples.

### `sandbox`

```text
python3 scripts/nvx.py sandbox
    [{run,provision,start,exec,stop,deprovision}]
    [--layer ROLE,PATH,EROFS_UUID]...
    [--scratch PATH]
    [--state-dir PATH]
    [--entrypoint PATH]
    [--arg VALUE]...
    [--hostname NAME]
    [--workload-user UID:GID]
    [--memory-max BYTES]
    [--pids-max COUNT]
    [--memory-mib MIB]
    [--timeout SECONDS]
    [--exec-timeout-ms MILLISECONDS]
    [--cwd GUEST_PATH]
    [--environment KEY=VALUE]...
    [--environment-file PATH]
    [--hypervisor {auto,whp,kvm,mshv}]
    [--mount GUEST_TARGET,HOST_PATH[,ro|rw]]
    [--mount-deny HOST_PATH]...
    [--net IPV4/PREFIX]
    [--network-profile {portable}]
    [--network-egress {allow,deny}]
    [--network-ingress {allow,deny}]
    [--network-egress-allow CIDR[:PROTOCOL:PORT]]...
    [--network-egress-deny CIDR[:PROTOCOL:PORT]]...
    [--network-egress-policy-file PATH]
    [--host-loopback {allow,deny}]
    [--network-proxy IPV4:TCP-PORT]
    [--host-loopback-forward PROTOCOL:HOST_PORT:GUEST_PORT]...
    [--outcome-report PATH]
    [--cmdline TEXT]
    [--dry-run]
```

Network policy and live-share options configure only the `run` and `provision`
launches.

| Option | Default | Description |
| --- | --- | --- |
| `{run,provision,start,exec,stop,deprovision}` | `run` | Select a one-shot run or a managed lifecycle operation. |
| `--layer ROLE,PATH,EROFS_UUID` | required for `run` and `provision` | Attach a `distro`, `runtime`, or `custom` EROFS layer. Repeat once per distinct role. |
| `--scratch PATH` | required for `run` and `provision` | Attach a preformatted ext4 scratch image as the writable overlay. |
| `--state-dir PATH` | required for managed operations | Select persistent sandbox state. One-shot `run` rejects this option. |
| `--entrypoint PATH` | `/bin/sh` | Select an absolute workload entrypoint without whitespace. |
| `--arg VALUE` | none | Append one whitespace-free entrypoint argument. Repeat to pass multiple arguments. |
| `--hostname NAME` | `nvx-sandbox` | Set the workload UTS hostname. |
| `--workload-user UID:GID` | `65534:65534` | Select the fixed non-root workload identity for `run` or `provision`. |
| `--memory-max BYTES` | none | Set the workload cgroup memory limit. |
| `--pids-max COUNT` | none | Set the workload cgroup process limit. |
| `--memory-mib MIB` | `256` | Set guest memory in MiB. |
| `--timeout SECONDS` | `60` | Set the control response timeout for managed `start`, `exec`, and `stop`. |
| `--exec-timeout-ms MILLISECONDS` | `0` | Set the managed `exec` guest workload timeout in the unsigned 32-bit range `0..4294967295`; zero disables the workload deadline. This is separate from the finite host `--timeout` response deadline. |
| `--cwd GUEST_PATH` | `/` | Set an absolute working directory inside the workload root for managed `exec`. It is resolved after the workload identity and root are applied; missing, inaccessible, or non-directory paths fail the workload launch. |
| `--environment KEY=VALUE` | omitted | Set the exact managed `exec` environment. Repeat for multiple entries. Empty values, spaces, additional equals signs, and UTF-8 are preserved. Inline values are visible in the invoking host process arguments; use `--environment-file` for sensitive values. |
| `--environment-file PATH` | omitted | Read the exact managed `exec` environment from a UTF-8 JSON array of `KEY=VALUE` strings, limited to 1 MiB of input. An empty array requests an empty environment. This option is mutually exclusive with `--environment`; omitting both preserves guest defaults. |
| `--hypervisor {auto,whp,kvm,mshv}` | `auto` | Select the host hypervisor. |
| `--mount GUEST_TARGET,HOST_PATH[,ro\|rw]` | none | Live-share one host directory at the absolute target inside the container rootfs for `run` or `provision`; defaults to `ro`. An `rw` share accepts guest-created symbolic links, which the host never follows. `/`, `/etc`, and the `/proc`, `/sys`, `/dev`, and `/.nvx-agent` trees are reserved. |
| `--mount-deny HOST_PATH` | none | Hide one existing file or directory inside the `--mount` host directory; relative paths are resolved inside it. Repeat to deny multiple paths. |
| `--net IPV4/PREFIX` | none | Enable virtio-net with a static guest address. |
| `--network-profile {portable}` | none | Select the required cross-platform network behavior contract; must be specified with `--net`. |
| `--network-egress {allow,deny}` | `allow` | Set the default guest egress policy for `run` or `provision`. |
| `--network-ingress {allow,deny}` | `deny` | Set the host ingress policy for `run` or `provision`. The portable profile supports only `deny`. |
| `--network-egress-allow CIDR[:PROTOCOL:PORT]` | none | Allow matching guest egress; repeat to add rules. Requires explicit `--network-egress`. |
| `--network-egress-deny CIDR[:PROTOCOL:PORT]` | none | Deny matching guest egress; repeat to add rules. Requires explicit `--network-egress`; deny rules take precedence. |
| `--network-egress-policy-file PATH` | none | Load bounded IPv4 ranges and rule-local CIDR exclusions for `run` or `provision`. Managed provision persists lowered rules, not this path. Requires explicit `--network-egress`; cannot be mixed with explicit allow/deny rule flags. |
| `--host-loopback {allow,deny}` | existing mapping | Control guest access to host loopback services for `run` or `provision`. |
| `--network-proxy IPV4:TCP-PORT` | none | Allow one explicit host TCP proxy endpoint; the IPv4 address must match the guest gateway. |
| `--host-loopback-forward PROTOCOL:HOST_PORT:GUEST_PORT` | none | Publish one TCP or UDP localhost port to the guest; repeat to add forwards and set `--host-loopback allow`. |
| `--outcome-report PATH` | none | Write a bounded local JSON outcome report for one-shot `run` or managed `exec`. |
| `--cmdline TEXT` | empty | Append non-sandbox kernel parameters; `nvx_*`, `virtfs_*`, and `tsc=` tokens are reserved. |
| `--dry-run` | off | Print the generated OpenVMM microVM command without running it. |

See [Run](run.md) for artifact preparation, the security boundary, and current
snapshot/configuration limitations.

## Benchmarking

### `benchmark`

```text
python3 scripts/nvx.py benchmark [OPTIONS]
```

| Option | Default | Description |
| --- | --- | --- |
| `--suite {boot,snapshot,restore,e2e,phase2,snapshot-profile,all,cold-start,device-io,device-restore-profile,network-snapshot,performance,shell-snapshot,shell-snapshot-restore,snapshot-restore-memory,snapshot-restore-vcpu,virtfs}` | `boot` | Select an acceptance, diagnostic, or workload suite. |
| `--backend {whp,kvm,mshv,both}` | `both` on Windows; `kvm` elsewhere | Select the hypervisor backend. |
| `--platform NAME` | inferred OS/backend | Record the host-typed performance series. |
| `--openvmm-dir PATH` | `openvmm/` | Select the OpenVMM repository. |
| `--nvx-dir PATH` | repository root | Select the NVX repository containing guest artifacts. |
| `--warmups N` | `5` for `device-io`; `1` for `device-restore-profile`; `3` otherwise | Set the number of excluded warmup attempts; zero is allowed. |
| `--runs N` | `30` for `device-io`; `5` for `device-restore-profile`; `11` otherwise | Set the number of retained attempts. |
| `--memory-mib MIB` | `128` | Set guest memory for the general suites. |
| `--processors {1,2,4,8}` | `1` | Run every cold, capture, restore, and workload launch with this microVM count. |
| `--virtfs-runs N` | `3` | Set the number of virtio-fs workload samples. |
| `--virtfs-memory-mib MIB` | `512` | Set guest memory for the virtio-fs workload. |
| `--payload-mib MIB` | `64` | Set the virtio-fs sequential I/O payload size. |
| `--shell-memories MIB [MIB ...]` | `128 256 512` (`128 256 512 1024` for `snapshot-profile`) | Set the guest memory sizes for shell snapshot measurements. |
| `--network-memory-mib MIB` | `256` | Set guest memory for the network snapshot workload. |
| `--restore-devices {console,net,virtiofs} [...]` | all three devices | Select devices for the `device-restore-profile` suite. |
| `--restore-modes {active,deferred} [...]` | both modes | Select activation modes for the `device-restore-profile` suite. |
| `--device-io-duration-seconds SECONDS` | `10` | Set each storage-operation or UDP round-trip measurement window. |
| `--device-io-size-mib MIB` | `512` | Set the virtio-blk and virtio-fs backing-object size. |
| `--device-io-port PORT` | `5201` | Set the same-host UDP echo port. |
| `--net IPV4/PREFIX` | none | Enable virtio-net with a static guest address. |
| `--network-profile {portable}` | none | Required with `--net`; selects the portable KVM/MSHV/WHP contract. |
| `--cpus CPUSET` | one logical CPU per physical core | Set process affinity in `taskset` syntax. |
| `--host-cpu-reserve N` | `2` | Require this many affinity CPUs beyond the guest vCPU count for VMM/device work. |
| `--timeout SECONDS` | `10` | Set the time allowed for each boot marker. |
| `--teardown-mode {guest-exit,host-terminate,host-sigterm}` | `guest-exit` | Select how to stop a measured VM; `host-sigterm` is a deprecated alias. |
| `--skip-build` | off | Reuse existing release binaries. |
| `--snapshot-profile` | off | Retain OpenVMM lifecycle phase samples and host counters; implied by the `snapshot-profile` suite. |
| `--cache-state {warm,cold,both}` | `both` | Select artifact cache states for the `snapshot-profile` suite. |
| `--output PATH` | none | Write the benchmark result as JSON. |
| `--output-dir PATH` | none | Write canonical workload logs to a directory. |
| `--scratch-dir PATH` | system temporary directory | Select an existing directory for temporary snapshots, guest RAM backing, and workload files. |
| `--keep-kvm-stage` | off | Keep temporary staged KVM benchmark binaries. |

Measured counts must be at least 1; warmups may be zero, and timeouts must be greater than zero.
The `e2e` suite uses the general memory size and measures cold start, snapshot
generation, snapshot restore, guest-exit teardown, and peak RSS against the
shell-ready markers. CI uses the default 128 MiB baseline.
See [Benchmark](benchmarks.md) for suite semantics, platform support, metric
definitions, and complete examples.

### `performance`

`performance` processes benchmark outputs for CI. It requires one nested
command.

#### `performance collect`

```text
python3 scripts/nvx.py performance collect
    --platform PLATFORM
    --commit COMMIT
    --input-dir PATH
    --output-dir PATH
    [--require-network]
    [--require-shell-snapshot]
    [--require-shared-suite]
    [--require-shell-snapshot-restore-512]
    [--lifecycle-input PATH]
    [--summary PATH]
```

Parses canonical benchmark logs into p50 CSV files. The `--require-*` flags
reject incomplete inputs for their respective workload sets.
`--require-shell-snapshot-restore-512` accepts only the canonical 512 MiB
restore metric from a 2-, 4-, or 8-vCPU run.
`--lifecycle-input` validates and merges a 128 MiB, guest-exit `e2e` JSON
result, producing the 31-metric microVM CI result. A directory whose metadata
selects `device-io` is collected as five additional ABI-2, one-vCPU `ops/s`
metrics; CI merges them into a 36-metric one-vCPU result.
`--summary` writes the p50 table plus lifecycle min/max/sample-count and RSS diagnostics.

#### `performance validate-openvmm`

```text
python3 scripts/nvx.py performance validate-openvmm
    --platform PLATFORM
    --input PATH
```

Validates a complete 128 MiB, guest-exit OpenVMM `e2e` result without
writing a CSV. Snapshot-generation instability exits with status 75 so
callers can remeasure the temporary host condition selectively; other
malformed or incomplete inputs exit with status 2.

#### `performance collect-openvmm`

```text
python3 scripts/nvx.py performance collect-openvmm
    --platform PLATFORM
    --commit COMMIT
    --input PATH
    --output-dir PATH
    [--summary PATH]
```

Converts a complete 128 MiB, guest-exit OpenVMM `e2e` result into an
eight-metric lifecycle p50 CSV.

#### `performance gate`

```text
python3 scripts/nvx.py performance gate
    --baseline-dir PATH
    --target-dir PATH
    [--window N]
    [--minimum-history N]
    [--threshold PERCENT]
    [--absolute-tolerance-ms MILLISECONDS]
    [--history-reset-dir PATH]
    [--summary PATH]
```

Checks target p50 values for regressions against rolling baseline histories.
`--window` defaults to `10`, `--minimum-history` to `10`, `--threshold` to
`40`, and `--absolute-tolerance-ms` to `5`. The gate uses the median of the
available window and treats metrics with insufficient history as warmups.
`--summary` writes a Markdown summary.
`--history-reset-dir` names tracked candidate histories; metrics removed from
an existing history file restart baseline warmup.

#### `performance persist`

```text
python3 scripts/nvx.py performance persist
    --source-dir PATH
    --history-dir PATH
    [--exclude-metric NAME]...
```

Appends current p50 values to branch history. Repeat `--exclude-metric` to omit
more than one metric.

## Source and package commands

### `collect-sources`

```console
python3 scripts/nvx.py collect-sources
```

Materializes the verified Linux, Alpine, and Ubuntu source artifacts needed
for a source-inclusive release. Azure Linux corresponding source is not
collected, so source-inclusive packages omit the Azure Linux guest.

### `collect-alpine-sources`

```text
python3 scripts/nvx.py collect-alpine-sources MANIFEST [MANIFEST ...]
    [--output PATH]
    [--cache PATH]
    [--skip-upstream]
```

| Option | Default | Description |
| --- | --- | --- |
| `MANIFEST` | required | One or more Alpine package manifests to collect. |
| `--output PATH` | `build/sources/alpine` | Select the output directory. |
| `--cache PATH` | `.cache/aports` | Select the aports cache directory. |
| `--skip-upstream` | off | Collect exact aports recipes without running `abuild fetch`. |

### `collect-ubuntu-sources`

```text
python3 scripts/nvx.py collect-ubuntu-sources MANIFEST [MANIFEST ...]
    [--output PATH]
    [--cache PATH]
```

| Option | Default | Description |
| --- | --- | --- |
| `MANIFEST` | required | One or more Ubuntu package or EROFS manifests to collect. |
| `--output PATH` | `build/sources/ubuntu` | Select the output directory. |
| `--cache PATH` | `.cache/ubuntu-source-indexes` | Select the downloaded source-index cache. |

The collector requires `gpgv`, deduplicates source package name/version pairs,
authenticates live or historical Ubuntu source indexes through signed
`InRelease` metadata and the pinned Ubuntu archive keyring, validates each
`.dsc`, and downloads every referenced source member.

### `create-linux-source-archive`

```text
python3 scripts/nvx.py create-linux-source-archive
    --config PATH
    --output PATH
```

Creates the deterministic Linux corresponding-source archive from the pinned
kernel inputs and the generated kernel configuration. `--config` must name an
existing generated kernel configuration, and `--output` names the archive to
create.

### `package`

```text
python3 scripts/nvx.py package
    [--version VERSION]
    [--destination PATH]
    (--include-source | --binary-only)
    [--force]
```

| Option | Description |
| --- | --- |
| `--version VERSION` | Override the packaged version. |
| `--destination PATH` | Override the staging destination. |
| `--include-source` | Include the corresponding source artifacts in the package and omit the Azure Linux guest artifacts. |
| `--binary-only` | Stage binaries only; publish corresponding source separately. |
| `--force` | Replace an existing staging destination. |

Exactly one of `--include-source` and `--binary-only` is required. See
[Package and source delivery](distribution.md) for release procedures and
source-publication requirements.

### `archive-release`

```text
python3 scripts/nvx.py archive-release
    --source PATH
    --destination PATH
```

Validates the staged distribution against its `SHA256SUMS`, snapshots the
accepted inventory, and creates a deterministic archive at the destination.
The destination must be outside the source directory and end in `.tar.gz` or
`.zip`.
