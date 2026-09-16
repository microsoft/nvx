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
| `build-guest` | Build the Linux kernel and Alpine initramfs. |
| `build-kernel` | Build the pinned and patched Linux kernel natively. |
| `build-initramfs` | Build the Alpine initramfs natively. |
| `build-agent-initramfs` | Build the explicit broker-ttrpc PID-1 initramfs. |
| `stage-agent` | Verify and stage a pinned static NVX agent input. |
| `verify-agent-initramfs` | Recheck a broker image, full-rootfs metadata, and embedded agent identity. |
| `build-openvmm` | Build the OpenVMM release binary. |
| `setup-cross-os-cache` | Install GNU tar and zstd for GitHub Actions cross-OS caches. |
| `test-openvmm` | Run self-contained OpenVMM microVM control-plane tests. |
| `test-microvm` | Run NVX Linux and device correctness tests through OpenVMM. |
| `build` | Build the guest artifacts and OpenVMM. |
| `download` | Download and install the latest matching GitHub release. |
| `run` | Run an OpenVMM microVM. |
| `sandbox` | Run one workload from EROFS layers over private ext4 scratch. |
| `benchmark` | Run the OpenVMM-native benchmark coordinator. |
| `performance` | Collect, gate, and persist CI performance results. |
| `collect-sources` | Materialize verified Linux and Alpine release sources. |
| `collect-alpine-sources` | Collect exact Alpine recipes and upstream sources. |
| `create-linux-source-archive` | Create a Linux corresponding-source archive. |
| `package` | Stage a binary distribution. |
| `archive-release` | Create a release archive with canonical safe modes. |
| `verify-broker-live-gate` | Verify an externally authenticated live broker proof against a package. |
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

See [Setup](setup.md) for host prerequisites.

## Build commands

### `build-guest`

```text
python3 scripts/nvx.py build-guest [--native] [--with-agent]
```

By default, builds the guest kernel and initramfs with Docker. `--native`
builds both simple-profile artifacts directly on Linux. `--with-agent` additionally
requires the staged agent and produces the separate agent initramfs.

### `build-kernel`

```console
python3 scripts/nvx.py build-kernel
```

Fetches, verifies, patches, and builds the pinned kernel directly on Linux.

### `build-initramfs`

```console
python3 scripts/nvx.py build-initramfs
```

Builds the Alpine initramfs directly on Linux.

### `build-agent-initramfs`

```text
python3 scripts/nvx.py build-agent-initramfs [--native]
```

Builds `build/initramfs-agent.cpio.gz` through Docker, or directly on Linux
with `--native`. The command requires a previously verified `stage-agent`
input. Native construction uses `~/.cache/nvx/native-work/broker-ttrpc` by
default; set `NVX_NATIVE_WORK_DIR` to another native Linux filesystem. DrvFS
and other mode-losing work roots are rejected before extraction.

### `build-openvmm`

```text
python3 scripts/nvx.py build-openvmm [--skip-restore]
```

Builds the `openvmm` release binary. Before building, the command runs
`cargo xflowey restore-packages`; use `--skip-restore` when those packages are
already restored.

### `build`

```text
python3 scripts/nvx.py build [--native] [--with-agent] [--skip-restore]
```

Runs `build-guest` followed by `build-openvmm`. The two options have the same
meaning as on those individual commands.

See [Build](build.md) for dependencies, outputs, and native build details.

## Test commands

### `test-openvmm`

```text
python3 scripts/nvx.py test-openvmm --backend {kvm,mshv,whp}
```

Builds and runs OpenVMM's checkout-owned microVM tests. The test artifacts are
produced by OpenVMM itself; NVX's kernel and initramfs are not required.

### `test-microvm`

```text
python3 scripts/nvx.py test-microvm
    --backend {kvm,mshv,whp}
    [--scenario SCENARIO]...
    [--processors {1,2,4,8} ...]
    [--memory-mib MIB]
    [--timeout SECONDS]
    [--output-dir PATH]
```

Runs NVX-owned Linux, SMP, virtio, sandbox, and snapshot correctness scenarios
against the public OpenVMM CLI. Repeat `--scenario` to select a subset; without
it, every scenario runs. The command requires `build/vmlinux`,
`build/initramfs.cpio.gz`, and `openvmm/target/release/openvmm[.exe]`.

## Download and run

### `download`

```text
python3 scripts/nvx.py download
    [--repository OWNER/REPOSITORY]
    [--hypervisor {auto,whp,kvm,mshv}]
    --transport {simple,broker-ttrpc}
    [--manifest-sha256 SHA256]
```

| Option | Default | Description |
| --- | --- | --- |
| `--repository OWNER/REPOSITORY` | `nanvix/nvx` | GitHub repository from which to download the latest release. |
| `--hypervisor {auto,whp,kvm,mshv}` | `auto` | Select the release platform. `auto` chooses WHP on Windows and KVM on Linux. |
| `--transport {simple,broker-ttrpc}` | required | Select an explicit package profile. |
| `--manifest-sha256 SHA256` | none | Independently trusted runtime-manifest digest; required for broker-ttrpc. |

Windows release downloads support WHP. Linux release downloads support KVM
and MSHV. Set `GH_TOKEN` when the selected repository requires authentication.
Current development releases publish only the simple profile; broker download
is available only after a separately live-gated package is published.

### `run`

```text
python3 scripts/nvx.py run
    [--hypervisor {auto,whp,kvm,mshv}]
    [--machine {microvm}]
    [--memory-mib MIB]
    [--memory-capacity-mib MIB]
    [--processors {1,2,4,8}]
    [--mount GUEST_TARGET,HOST_PATH[,ro|rw]]
    [--net IPV4/PREFIX]
    [--network-profile {portable}]
    [--cmdline TEXT]
    [--restore-snapshot PATH]
    [--restore-processors {1,2,4,8}]
    [--restore-memory-mib MIB]
    [--restore-ready-path PATH]
    [--dry-run]
```

| Option | Default | Description |
| --- | --- | --- |
| `--hypervisor {auto,whp,kvm,mshv}` | `auto` | Select the OpenVMM hypervisor. `auto` chooses WHP on Windows and KVM elsewhere. |
| `--machine {microvm}` | `microvm` | Select the fixed-topology microVM with shared-status edge interrupts. |
| `--memory-mib MIB` | `128` | Set guest memory in MiB. |
| `--memory-capacity-mib MIB` | none | Reserve an immutable, 128 MiB-aligned RAM capacity for a fresh microVM snapshot. |
| `--processors {1,2,4,8}` | `1` | Select the microVM processor count. |
| `--mount GUEST_TARGET,HOST_PATH[,ro\|rw]` | none | Expose one host directory to the absolute guest target. Active snapshot restore requires the same canonical path, target, and mode; a dormant-slot restore may attach a new mapping that the resumed guest mounts explicitly. |
| `--net IPV4/PREFIX` | none | Enable virtio-net with the static guest IPv4 address and prefix. |
| `--network-profile {portable}` | none | Select the required cross-platform network behavior contract; must be specified with `--net`. |
| `--cmdline TEXT` | empty | Append kernel parameters; `nvx_*` and `tsc=` tokens are reserved. |
| `--restore-snapshot PATH` | none | Restore the immutable machine contract and saved state from a snapshot directory. |
| `--restore-processors {1,2,4,8}` | none | Bring this contiguous processor prefix online before restore readiness. Requires an opt-in microVM snapshot and cannot exceed `--processors` capacity. |
| `--restore-memory-mib MIB` | none | Select the 128 MiB-aligned RAM target for an expansion-capable snapshot restore. |
| `--restore-ready-path PATH` | none | Publish one restore-readiness event to an existing Unix socket or Windows named pipe. |
| `--dry-run` | off | Print the generated OpenVMM command without running it. |

The command requires the OpenVMM release binary, `build/vmlinux`, and
`build/initramfs.cpio.gz`. See [Run](run.md) for host setup, guest shutdown,
networking, and virtio-fs examples.

### `sandbox`

```text
python3 scripts/nvx.py sandbox
    --layer ROLE,PATH [--layer ...]
    --scratch PATH
    --transport broker-ttrpc
    --control-socket PATH
    --boot-console-socket PATH
    --control-auth-handle FD
    [--memory-mib MIB]
    [--hypervisor {auto,whp,kvm,mshv}]
    [--net IPV4/PREFIX]
    [--network-profile {portable}]
    [--cmdline TEXT]
    [--dry-run]
```

| Option | Default | Description |
| --- | --- | --- |
| `--layer ROLE,PATH` | required | Attach a `distro`, `runtime`, or `custom` EROFS layer. Authenticated UUID/GPT identity is supplied later through ACI-04 Bootstrap. |
| `--scratch PATH` | required | Attach a preformatted ext4 scratch image as the writable overlay. |
| `--transport broker-ttrpc` | required | Select the authenticated broker transport explicitly. |
| `--control-socket PATH` | required | Set the protected broker control-console listener. |
| `--boot-console-socket PATH` | required | Set the separate boot-diagnostic console listener. |
| `--control-auth-handle FD` | required | Redirect the readable inherited ACI-04 capability pipe to OpenVMM stdin with `--microvm-control-auth-stdin`; never expose its bytes in arguments, environment variables, or logs. |
| `--memory-mib MIB` | `256` | Set guest memory in MiB. |
| `--hypervisor {auto,whp,kvm,mshv}` | `auto` | Select the host hypervisor. |
| `--net IPV4/PREFIX` | none | Enable virtio-net with a static guest address. |
| `--network-profile {portable}` | none | Select the required cross-platform network behavior contract; must be specified with `--net`. |
| `--cmdline TEXT` | empty | Append non-sandbox kernel parameters; `nvx_*` and `tsc=` tokens are reserved. |
| `--dry-run` | off | Print the generated OpenVMM microVM command without running it. |

See [Run](run.md) for artifact preparation, the security boundary, and current
snapshot/configuration limitations. Live broker launch is Linux-only; `--dry-run`
is available on other hosts.

## Benchmarking

### `benchmark`

```text
python3 scripts/nvx.py benchmark [OPTIONS]
```

| Option | Default | Description |
| --- | --- | --- |
| `--suite {boot,snapshot,restore,e2e,phase2,snapshot-profile,all,cold-start,device-io,network-snapshot,performance,shell-snapshot,shell-snapshot-restore,snapshot-restore-vcpu,virtfs}` | `boot` | Select an acceptance, diagnostic, or workload suite. |
| `--backend {whp,kvm,mshv,both}` | `both` on Windows; `kvm` elsewhere | Select the hypervisor backend. |
| `--platform NAME` | inferred OS/backend | Record the host-typed performance series. |
| `--openvmm-dir PATH` | `openvmm/` | Select the OpenVMM repository. |
| `--nvx-dir PATH` | repository root | Select the NVX repository containing guest artifacts. |
| `--warmups N` | `5` for `device-io`; `3` otherwise | Set the number of excluded warmup attempts; zero is allowed. |
| `--runs N` | `30` for `device-io`; `11` otherwise | Set the number of retained attempts. |
| `--memory-mib MIB` | `128` | Set guest memory for the general suites. |
| `--processors {1,2,4,8}` | `1` | Run every cold, capture, restore, and workload launch with this microVM count. |
| `--virtfs-runs N` | `3` | Set the number of virtio-fs workload samples. |
| `--virtfs-memory-mib MIB` | `512` | Set guest memory for the virtio-fs workload. |
| `--payload-mib MIB` | `64` | Set the virtio-fs sequential I/O payload size. |
| `--shell-memories MIB [MIB ...]` | `64 128 256 512` | Set the guest memory sizes for shell snapshot measurements. |
| `--network-memory-mib MIB` | `256` | Set guest memory for the network snapshot workload. |
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
| `--output PATH` | none | Write the benchmark result as JSON. |
| `--output-dir PATH` | none | Write canonical workload logs to a directory. |
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
    [--summary PATH]
```

Checks target p50 values for regressions against rolling baseline histories.
`--window` defaults to `10`, `--minimum-history` to `10`, `--threshold` to
`40`, and `--absolute-tolerance-ms` to `5`. The gate uses the median of the
available window and treats metrics with insufficient history as warmups.
`--summary` writes a Markdown summary.

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

```text
python3 scripts/nvx.py collect-sources --transport {simple,broker-ttrpc}
```

Materializes the verified Linux and Alpine source artifacts needed for a
source-inclusive release for the selected profile.

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

### `create-linux-source-archive`

```text
python3 scripts/nvx.py create-linux-source-archive
    --config PATH
    --output PATH
```

Creates the Linux corresponding-source archive using the required kernel
configuration and output paths.

### `package`

```text
python3 scripts/nvx.py package
    [--version VERSION]
    [--destination PATH]
    --transport {simple,broker-ttrpc}
    (--include-source | --binary-only)
    [--manifest-digest-output PATH]
    [--force]
```

| Option | Description |
| --- | --- |
| `--version VERSION` | Override the packaged version. |
| `--destination PATH` | Override the staging destination. |
| `--transport {simple,broker-ttrpc}` | Select the explicit release profile. |
| `--include-source` | Include the corresponding source artifacts in the package. |
| `--binary-only` | Stage binaries only; publish corresponding source separately. |
| `--manifest-digest-output PATH` | Write the broker manifest digest outside the bundle for independently trusted delivery. |
| `--force` | Replace an existing staging destination. |

Exactly one of `--include-source` and `--binary-only` is required. See
[Package and source delivery](distribution.md) for release procedures and
source-publication requirements.

### `archive-release`

```text
python3 scripts/nvx.py archive-release --bundle PATH --output PATH
```

Creates a `.tar.gz` or `.zip` with one package root and canonical `0755`
executable/directory or `0644` data modes. It rejects special files and
verifies `SHA256SUMS` before writing.

### `verify-broker-live-gate`

```text
python3 scripts/nvx.py verify-broker-live-gate
    --archive PATH
    --archive-sha256 SHA256
    --manifest-digest PATH
    --proof PATH
    --proof-sha256 SHA256
    --platform PLATFORM
```

Authenticates the external proof by its separately trusted digest, checks the
publisher-computed exact archive digest, validates the bounded archive and
complete package, and requires successful control authentication, GetGuestInfo
self-hash, Bootstrap, WaitReady, and Shutdown results.
