# Build

Drive supported workflows directly through `scripts/nvx.py`; no Make wrapper
is required.

The portable workflow downloads the pinned Linux archive, verifies its
SHA-256, applies every patch in `kernel/patches`, and builds Linux plus the
Alpine initramfs in Docker. OpenVMM builds on the host:

```bash
python3 scripts/nvx.py build-guest
python3 scripts/nvx.py build-openvmm
```

On a Linux host, build the guest directly:

```bash
python3 scripts/nvx.py build-guest --native
```

The standard build produces:

```text
build/vmlinux
build/vmlinux.config
build/initramfs.cpio.gz
build/initramfs.cpio.gz.packages.json
openvmm/target/release/openvmm[.exe]
```

The native kernel build caches the verified and patched source under
`.cache/linux`, uses `O=build/linux`, runs `olddefconfig`, exports the exact
generated config as `build/vmlinux.config`, and fails if the Xen PVH note is
absent. Changing an archive hash or patch invalidates both source and object
caches.
