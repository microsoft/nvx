# Package and source delivery

Stage a binary release and SHA-256 manifest:

```bash
python3 scripts/nvx.py package --binary-only
```

Binary-only mode requires an explicit acknowledgement because the matching
Linux and Alpine source must be published separately.

First materialize the release sources:

```bash
python3 scripts/nvx.py collect-sources
```

This produces a patched Linux corresponding-source archive under
`build/sources/linux` and exact Alpine recipes/upstream sources under
`build/sources/alpine`. Then stage the binary release with three separate
source artifacts:

```bash
python3 scripts/nvx.py package --include-source
```

The release contains:

```text
guest/nvx-agent                    # only when staged
source/nvx-project-source-0.1.0.tar.gz
source/nvx-linux-source-6.18.38.tar.gz
source/nvx-alpine-source-0.1.0.tar.gz
```

The packaged `SOURCE-MANIFEST.json` records SHA-256 identities for OpenVMM,
the kernel, the initramfs, and the optional agent. The agent remains a staged
input and is not installed in the initramfs until the explicit cutover.

Linux is GPL-2.0-only, so a distributor of `vmlinux` must make its complete
corresponding source available. Alpine packages retain their individual
licenses. The collector uses the exact aports commit embedded in every
installed APK and runs `abuild fetch` plus `abuild verify`. If Python
initramfs variants are present, `collect-sources` includes their package
manifests automatically.

OpenVMM is MIT licensed: retain its notice, but its source does not have to be
published merely because it is aggregated with Linux. See
`THIRD_PARTY_NOTICES.md`.
