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

## Snapshot restore readiness

Restore an existing microVM snapshot with an optional host-readiness endpoint:

```bash
python3 scripts/nvx.py run \
  --restore-snapshot /var/lib/nvx/snapshot \
  --restore-ready-path /run/nvx/restore-ready.sock
```

The endpoint must already be listening. OpenVMM connects to a Unix domain
socket on Linux or a `//./pipe/...` named pipe on Windows and writes exactly
`OPENVMM_RESTORE_READY_V1\n` after snapshot verification, attachment
resolution, and worker startup complete, but before the restored vCPU can run.
Failure to write the complete event aborts and tears down the restore.

## virtio-fs host mapping

The microVM supports one mapping with a fixed `microvm` tag. The initramfs
mounts it automatically:

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
place them under one exported host root. Snapshot restore requires the same
target, mode, and underlying host directory.

## Experimental single-workload sandbox

The `sandbox` command launches microVM ABI v2 with one to three compressed
EROFS lower layers and one preformatted ext4 scratch image:

```bash
python3 scripts/nvx.py sandbox \
  --layer distro,/var/lib/nvx/distro.erofs,11111111-1111-1111-1111-111111111111 \
  --layer runtime,/var/lib/nvx/runtime.erofs,22222222-2222-2222-2222-222222222222 \
  --scratch /var/lib/nvx/scratch.ext4 \
  --entrypoint /bin/workload \
  --arg=--serve
```

The layer UUID is the EROFS superblock UUID, not a content digest. The command
validates the files before launch, orders roles independently of option order,
attaches layers read-only, and reserves the writable slot for scratch.
Conversion and scratch formatting stay off the start path; prepare those
artifacts on Linux with `mkfs.erofs` and `mkfs.ext4`.

This is the cold-filesystem bootstrap described in
[the sandbox design](fs-and-snapshot-design.md), not the final production
agent. It accepts no environment variables or secrets, does not support block
snapshots, and deliberately omits the configuration region and runtime RPC.
Arguments are individual kernel-command-line tokens and therefore cannot
contain whitespace. The workload enters private mount/PID/UTS namespaces with
a private `/dev`, an agent-owned cgroup, no capabilities, and `no_new_privs`.
The outer agent retains the initramfs root; the capability-stripped child
enters only the assembled root with `chroot`, because Linux cannot
`pivot_root` away from an initramfs `rootfs`.
