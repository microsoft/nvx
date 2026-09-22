# Package and source delivery

Stage a binary release and SHA-256 manifest:

```bash
python3 scripts/nvx.py package --binary-only
```

Binary-only mode requires an explicit acknowledgement because the matching
Linux, Alpine, and Ubuntu source must be published separately.

First materialize the release sources:

```bash
python3 scripts/nvx.py collect-sources
```

This produces a patched Linux corresponding-source archive under
`build/sources/linux`, exact Alpine recipes/upstream sources under
`build/sources/alpine`, and exact Ubuntu `.dsc` plus referenced source members
under `build/sources/ubuntu`. Then stage the binary release with four separate
source artifacts:

```bash
python3 scripts/nvx.py package --include-source
```

The release contains:

```text
source/nvx-project-source-0.1.0.tar.gz
source/nvx-linux-source-6.18.38.tar.gz
source/nvx-alpine-source-0.1.0.tar.gz
source/nvx-ubuntu-source-0.1.0.tar.gz
```

Linux is GPL-2.0-only, so a distributor of `vmlinux` must make its complete
corresponding source available. Alpine packages retain their individual
licenses. The collector uses the exact aports commit embedded in every
installed APK and runs `abuild fetch` plus `abuild verify`. If Python
initramfs variants are present, `collect-sources` includes their package
manifests automatically.

Ubuntu artifacts use Ubuntu userland with the NVX kernel. The collector
deduplicates exact source package name/version pairs from both Ubuntu
manifests, downloads the matching `.dsc` and source members from Canonical's
archive, and verifies the SHA-256 metadata before packaging. Exact versions
that have left the live suite indexes are resolved through Canonical's
Launchpad publishing history; the raw publishing and source-file URL responses
are retained with their URLs and SHA-256 digests. Newer source versions are
never substituted.

OpenVMM is MIT licensed: retain its notice, but its source does not have to be
published merely because it is aggregated with Linux. See
`THIRD_PARTY_NOTICES.md`.
