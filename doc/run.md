# Run

Download and install the latest release matching the host before the first run:

```bash
python3 scripts/nvx.py download
python3 scripts/nvx.py run
```

This installs the packaged kernel and initramfs under `build/` and OpenVMM
under `openvmm/target/release/`, so no local build is required. Set `GH_TOKEN`
to a token with contents read access when downloading from a private repository.
On Linux, pass `--hypervisor mshv` to both commands to use the MSHV package.

The CLI chooses WHP on Windows and KVM on Linux:

```bash
python3 scripts/nvx.py run
```

A successful Alpine boot prints both `ALPINE-MICROVM-BOOT-OK` and
`NVX-GUEST-BOOT-OK: alpine`. Boot Ubuntu userland with the same NVX kernel:

```bash
python3 scripts/nvx.py run --guest ubuntu
```

Ubuntu defaults to 256 MiB and prints `NVX-GUEST-BOOT-OK: ubuntu`. It is
Ubuntu userland with the NVX kernel, not a stock Ubuntu kernel or systemd VM.
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

The microVM uses fixed device topology, reserves a shared interrupt-status page, and uses
shared-status edge-triggered virtio interrupts with 1, 2, 4, or 8 vCPUs.

## Run OpenVMM directly

The platform release archives are self-contained; `scripts/nvx.py` is a
convenience wrapper and is not required at runtime. Extract the archive that
matches the host and run the following command from its
`nvx-VERSION-PLATFORM` directory. For Linux/KVM:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --processors 1 \
  --hypervisor kvm \
  --memory 128M \
  --kernel guest/vmlinux \
  --initrd guest/initramfs.cpio.gz
```

For Windows/WHP in PowerShell:

```powershell
.\bin\openvmm.exe `
  --single-process `
  --machine microvm `
  --processors 1 `
  --hypervisor whp `
  --memory 128M `
  --kernel guest\vmlinux `
  --initrd guest\initramfs.cpio.gz
```

For Linux/MSHV, use the `linux-mshv` archive and replace `kvm` with `mshv`.
If the artifacts are already installed in the repository layout, use
`openvmm/target/release/openvmm[.exe]`, `build/vmlinux`, and
`build/initramfs.cpio.gz` instead of the paths above.

For Ubuntu userland, use 256 MiB initially and select
`guest/initramfs-ubuntu.cpio.gz` or
`build/initramfs-ubuntu.cpio.gz` as the initrd. The kernel path remains
unchanged.

Direct OpenVMM launches accept generic directional network defaults:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --hypervisor kvm \
  --memory 128M \
  --kernel guest/vmlinux \
  --initrd guest/initramfs.cpio.gz \
  --net 10.0.0.2/24 \
  --network-profile portable \
  --network-egress allow \
  --network-ingress deny
```

Egress defaults to `allow` and ingress defaults to `deny`. With ingress denied,
responses to guest-initiated connections remain available, while new inbound
connections do not. The portable profile supports egress `allow` or `deny` but
rejects ingress `allow` before the workload starts.

For destination and port rules, select an explicit default and repeat generic
allow/deny options:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --hypervisor kvm \
  --memory 128M \
  --kernel guest/vmlinux \
  --initrd guest/initramfs.cpio.gz \
  --net 10.0.0.2/24 \
  --network-profile portable \
  --network-egress deny \
  --network-egress-allow 140.82.112.0/20:tcp:443 \
  --network-egress-deny 140.82.114.0/24:tcp:443
```

Rules match IPv4 addresses or CIDRs and may add one TCP or UDP destination
port. Deny matches take precedence over allow matches.

Host-loopback denial and deliberate localhost port publishing are separately
controlled from ordinary egress:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --hypervisor kvm \
  --memory 128M \
  --kernel guest/vmlinux \
  --initrd guest/initramfs.cpio.gz \
  --net 10.0.0.2/24 \
  --network-profile portable \
  --host-loopback deny \
  --network-proxy 10.0.0.1:3128
```

With `deny`, general guest-to-host loopback and every host-to-guest forward are
blocked, while the exact TCP proxy endpoint remains available. UDP on that
same port and other host service ports remain blocked even with ordinary
egress allowed.

The portable profile does **not** support generic bidirectional host-loopback
allow. Explicit `--host-loopback allow` without any forward is rejected before
VM resources are opened. For deliberate port publishing, repeat
`--host-loopback-forward tcp:HOST_PORT:GUEST_PORT` or its UDP form to expose
only selected localhost ports toward the guest, with explicit
`--host-loopback allow`. These forwards do not satisfy a generic allow policy
that provides no port list. Omitting `--host-loopback` preserves existing
guest-to-host mapping without publishing guest ports. Guest-originated traffic
remains subject to egress policy.

Most `nvx.py run` options pass through unchanged: `--machine`, `--processors`,
`--mount`, `--net`, `--network-profile`, `--cmdline`, `--restore-snapshot`,
`--restore-processors`, and `--restore-ready-path`. The wrapper performs these
translations and additions:

| `nvx.py run` | Direct OpenVMM option |
| --- | --- |
| `--guest alpine` | `--initrd .../initramfs.cpio.gz` on a fresh boot |
| `--guest ubuntu` | `--initrd .../initramfs-ubuntu.cpio.gz` and a 256 MiB default on a fresh boot |
| `--hypervisor auto` | `--hypervisor kvm` on Linux or `--hypervisor whp` on Windows |
| `--memory-mib N` | `--memory NM` |
| `--memory-capacity-mib N` | `--memory-capacity NM` on a fresh boot |
| `--restore-memory-mib N` | `--restore-memory NM` |
| `--dry-run` | No equivalent; this only prints the generated command |

Always include `--single-process`. When restoring, omit `--memory`, `--kernel`,
and `--initrd`, and add `--restore-entropy`; the wrapper adds this option
automatically. Do not pass `--guest ubuntu` during restore; the captured RAM
and machine contract already identify the restored guest. For example:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --processors 8 \
  --hypervisor kvm \
  --restore-snapshot /var/lib/nvx/snapshot \
  --restore-entropy \
  --restore-processors 4 \
  --restore-memory 1024M \
  --restore-ready-path /run/nvx/restore-ready.sock
```

## Migration from the retired profile

`microvm` is now the only selector and launches the contract previously named
`microvm-v2`. The `microvm-v2` spelling, the former one-vCPU ABI-1 behavior,
ABI-1 device-I/O control, TTRPC numeric value 1, ABI-1 snapshot restore, and
boot-layout-1 snapshot restore are removed. Snapshot metadata and performance
series continue to use numeric ABI value 2 and boot-layout value 2.
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
A repeatable `--mount-deny HOST_PATH` hides an existing file or directory
inside that root. Denied names are omitted from directory listings and remain
inaccessible through `..`, a symlink/junction, or another mount of the same
virtio-fs device. Unsafe, external, duplicate, overlapping, and nested-mount
rules are rejected before boot.
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
python3 scripts/nvx.py build-distro-layer \
  --guest ubuntu \
  --output build/ubuntu-distro.erofs
```

Read the deterministic UUID from
`build/ubuntu-distro.erofs.manifest.json`, create scratch independently with
`mkfs.ext4`, then launch the Ubuntu workload through the existing Alpine
control initramfs:

```bash
python3 scripts/nvx.py sandbox \
  --layer distro,build/ubuntu-distro.erofs,11111111-1111-1111-1111-111111111111 \
  --scratch /var/lib/nvx/scratch.ext4 \
  --entrypoint /bin/sh \
  --workload-user 65534:65534 \
  --memory-mib 256
```

CI uses `/sbin/nvx-sandbox-smoke` as the entrypoint to verify Ubuntu identity,
the fixed non-root account, and a scratch-backed `/tmp` write before clean
guest exit.

The layer UUID is the EROFS superblock UUID, not a content digest. The command
validates the files before launch, orders roles independently of option order,
attaches layers read-only, and reserves the writable slot for scratch.
Conversion and scratch formatting stay off the start path. The Ubuntu
converter verifies immutable inputs, applies the deny-by-default metadata
policy, creates `/nonexistent` for UID/GID 65534, and invokes `mkfs.erofs`.
Systemd entrypoints are explicitly unsupported and do not relax the non-root,
drop-all-capabilities sandbox policy.

This is the cold-filesystem bootstrap described in
[the sandbox design](design/sandbox-filesystem-and-agent-architecture.md#implemented-filesystem-bootstrap), not the final
production agent. It accepts no environment variables or secrets and does not
expose sandbox snapshot capture or restore, the configuration region, or runtime RPC.
Arguments are individual kernel-command-line tokens and therefore cannot
contain whitespace. The workload enters private mount/PID/UTS namespaces with
a private `/dev`, an agent-owned cgroup, no capabilities, and `no_new_privs`.
It always runs as the fixed non-root `UID:GID` selected at VM creation
(`65534:65534` by default). The guest verifies that exactly one matching user,
its primary group, and its absolute home directory exist in the assembled
root; otherwise the workload is never started.
The outer agent retains the initramfs root; the capability-stripped child
enters only the assembled root with `chroot`, because Linux cannot
`pivot_root` away from an initramfs `rootfs`.

For a state-aware sandbox, provision configuration without starting a VM,
start it once, run multiple workloads in the same warm guest, stop it while
retaining configuration, and finally deprovision it:

```bash
python3 scripts/nvx.py sandbox provision \
  --state-dir /run/user/1000/nvx-example \
  --layer distro,/var/lib/nvx/distro.erofs,11111111-1111-1111-1111-111111111111 \
  --scratch /var/lib/nvx/scratch.ext4
python3 scripts/nvx.py sandbox start \
  --state-dir /run/user/1000/nvx-example
python3 scripts/nvx.py sandbox exec \
  --state-dir /run/user/1000/nvx-example \
  --entrypoint /usr/bin/python3 --arg=/work/agent.py \
  --outcome-report /run/user/1000/nvx-example-exec.json
python3 scripts/nvx.py sandbox exec \
  --state-dir /run/user/1000/nvx-example \
  --entrypoint /bin/sh --arg=-c --arg='cat /tmp/previous-result'
python3 scripts/nvx.py sandbox stop \
  --state-dir /run/user/1000/nvx-example
python3 scripts/nvx.py sandbox deprovision \
  --state-dir /run/user/1000/nvx-example
```

Lifecycle transitions fail closed: `start` rejects an already-running or stale
runtime record, `exec` and `stop` require a live OpenVMM process, and
`deprovision` refuses to remove a running sandbox or unknown files. Managed
workload arguments use the bounded control protocol rather than the kernel
command line and may contain whitespace. The legacy operation-less `sandbox`
form is `sandbox run`; it remains one-shot and rejects `--state-dir` or any
request to retain VM state.

`run --outcome-report PATH` and one-shot `sandbox run --outcome-report PATH`
forward OpenVMM's bounded local JSON report. Managed `sandbox exec` writes only
the operation, bounded result category, numeric status, and an opaque operation
ID to its requested report; stdout, stderr, arguments, environment values, and
credentials remain excluded. `sandbox stop` waits for OpenVMM teardown and
retains the latest VM-level report as `outcome.json` in the state directory.
Neither OpenVMM nor NVX uploads these files.
