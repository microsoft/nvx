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
  --cmdline "quiet loglevel=0"
```

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
