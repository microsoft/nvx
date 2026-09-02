# Run

Download and install the latest release matching the host before the first run:

```bash
python3 scripts/nvx.py download --transport legacy
python3 scripts/nvx.py run
```

This installs the packaged kernel and initramfs under `build/` and OpenVMM
under `openvmm/target/release/`, so no local build is required. Set `GH_TOKEN`
to a token with contents read access when downloading from a private repository.
On Linux, pass `--hypervisor mshv` to both commands to use the MSHV package.
The explicit legacy selection installs only `build/initramfs.cpio.gz`.

Broker packages are not published by the current development workflow. If a
separately live-gated broker package is delivered, installation requires its
independently authenticated manifest digest:

```bash
python3 scripts/nvx.py download \
  --transport broker-ttrpc \
  --manifest-sha256 <trusted-64-hex-digest>
```

This installs `build/initramfs-agent.cpio.gz`. The installer stages and verifies
the complete profile before touching current files, then transactionally
promotes it and rolls back the previous binary, provenance, and both profile
artifacts on any failure. A shared kernel or VMM therefore cannot be combined
with an initramfs from another release. Build outputs may still coexist before
any release install. A digest shipped only inside the same bundle is not a
trust root.

The CLI chooses WHP on Windows and KVM on Linux:

```bash
python3 scripts/nvx.py run
```

A successful boot prints `ALPINE-MICROVM-BOOT-OK` and opens a root shell.
Exit cleanly from the guest with:

```sh
/sbin/nvx-exit 0
```

Pass extra guest options without changing the generated device ABI:

```bash
python3 scripts/nvx.py run \
  --memory-mib 256 \
  --net 10.0.0.2/24 \
  --network-profile portable \
  --cmdline "quiet loglevel=0"
```

Networking requires the explicit `portable` capability profile. It uses the
same in-process data plane on KVM, MSHV, and WHP; omitting either `--net` or
`--network-profile portable` is rejected before OpenVMM starts.

Select an explicit processor count after building the matching specialized guest kernel:

```bash
python3 scripts/nvx.py run --machine microvm --processors 8
```

The microVM uses fixed device topology, reserves a PVH status page, and uses
shared-status edge-triggered virtio interrupts with 1, 2, 4, or 8 vCPUs.

## Migration from the retired profile

`microvm` is now the only selector and launches the contract previously named
`microvm-v2`. The `microvm-v2` spelling, the former one-vCPU ABI-1 behavior,
ABI-1 device-I/O control, TTRPC numeric value 1, and ABI/PVH-layout-1 snapshot
restore are removed. Snapshot metadata and performance series continue to use
numeric ABI value 2 so existing ABI-2 artifacts are not reinterpreted as ABI 1.
Use NVX commit `cb52bcd454b454cb241096c33ed42a1dcdc65347` with OpenVMM commit
`1b70365613517a10718e00284a62bdffbd80e41c`, or an earlier compatible pair, to
run retired ABI-1 guests or snapshots.

## Snapshot restore readiness

Restore an existing microVM snapshot with an optional host-readiness endpoint:

```bash
python3 scripts/nvx.py run \
  --machine microvm \
  --processors 8 \
  --restore-snapshot /var/lib/nvx/snapshot \
  --restore-processors 4 \
  --restore-memory-mib 1024 \
  --restore-ready-path /run/nvx/restore-ready.sock
```

The endpoint must already be listening. OpenVMM connects to a Unix domain
socket on Linux or a `//./pipe/...` named pipe on Windows and writes exactly
`OPENVMM_RESTORE_READY_V1\n` after snapshot verification, attachment
resolution, worker startup, gated guest repair, and host-input re-enable
complete while the restored vCPU remains stopped. With
`--restore-processors`, the snapshot keeps its immutable eight-vCPU capacity;
the source must have captured a canonical `maxcpus` boot-online prefix, and the
guest onlines exactly the requested prefix before readiness and host input
release. Targets are limited to 1, 2, 4, or 8 and cannot be below the captured
boot-online count or above capacity. Legacy snapshots reject the option. The
peer must accept and read while startup is in progress; Windows flush
completion waits for the named-pipe peer to consume the frame. Failure to
write the complete event aborts and tears down the restore.

For restore-time memory expansion, capture a fresh snapshot with
`--memory-mib 512 --memory-capacity-mib 2048`, then select a target with
`--restore-memory-mib 512`, `1024`, or `2048`. The snapshot's `memory.bin`
remains exactly 512 MiB; selected expansion ranges receive fresh per-launch
backing and are onlined before restore readiness.

## virtio-fs host mapping

The microVM reserves one mapping slot with a fixed `microvm` tag. On a cold
boot with `--mount`, the initramfs mounts it automatically:

```bash
python3 scripts/nvx.py run --mount "/mnt/host,/absolute/host/share,rw"
```

PowerShell example:

```powershell
python scripts\nvx.py run `
  --mount "/mnt/host,C:\Users\me\microvm-share,rw"
```

Use `ro` for read-only access. The guest target must be an absolute Linux path.
Host paths containing commas are unsupported. To expose multiple directories,
place them under one exported host root. A snapshot captured with a mapping
requires the same canonical host path, target, mode, and filesystem identity.
A snapshot captured without a mapping may restore with a new `--mount`; after
resume, mount it explicitly inside the guest because the initramfs hook has
already completed:

```sh
mkdir -p /mnt/host
mount -t virtiofs microvm /mnt/host
```

## Experimental single-workload sandbox

The `sandbox` command launches the microVM with one to three compressed
EROFS lower layers and one preformatted ext4 scratch image:

```bash
python3 scripts/nvx.py sandbox \
  --transport broker-ttrpc \
  --control-socket /run/nvx/control.sock \
  --boot-console-socket /run/nvx/boot.sock \
  --control-auth-handle 9 \
  --layer distro,/var/lib/nvx/distro.erofs \
  --layer runtime,/var/lib/nvx/runtime.erofs \
  --scratch /var/lib/nvx/scratch.ext4
```

The command selects `initramfs-agent.cpio.gz`, OpenVMM `microvm-v2`, the
ordinary boot console, and a separate authenticated control console. OpenVMM
owns console enumeration and adds the single reserved
`nvx_control_tty=hvc2` token; the package does not hardcode an HVC device.
The authentication handle must be an already-open, inherited one-way pipe
created by the ACI-04 launcher; the numeric descriptor is not capability
material, and this CLI never accepts or logs the capability bytes.
Layer UUID/GPT identities belong to the authenticated ACI-04 Bootstrap request,
not this boot-only CLI. The command validates files before launch, orders roles independently of option order,
attaches layers read-only, and reserves the writable slot for scratch.
Conversion and scratch formatting stay off the start path; prepare those
artifacts on Linux with `mkfs.erofs` and `mkfs.ext4`.

The CLI starts the reviewed PID-1 package but is not an ACI control-plane
client; production Bootstrap and workload lifecycle RPCs come from ACI-04 over
the broker socket. Do not place capabilities, secrets, or workload
configuration on the kernel command line. Broker launch rejects `init`,
`rdinit`, every `nvx_*` parameter (including duplicate `nvx_control_tty` or
`nvx_sandbox` tokens), and console, virtio discovery, virtfs, network, and
other host-owned parameters. The generated command supplies no init override
and relies on the verified `/init -> /sbin/nvx-agent` image layout.

The source-level ttrpc writer priority/FIFO blocker is resolved. Remaining
cross-repository gates are live host-observed stress proving bounded downstream
HVC latency under contention; real privileged MMIO/HVC E2E from broker READY
through authenticated GetGuestInfo self-hash, Bootstrap, WaitReady, Shutdown,
and failure cases; tenant-safe snapshot freeze/restore lifecycle; and external
live-gate proof delivery. Local packaging or initramfs inspection does not
satisfy those gates, and broker release publication remains disabled.
