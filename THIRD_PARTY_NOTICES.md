# Third-party notices

This distribution combines independently licensed components. It is an
aggregate; the Linux kernel license does not change the license of OpenVMM or
the distribution tooling.

| Component | Location | License and source |
| --- | --- | --- |
| Linux 6.18.38 | Generated under `.cache/linux` | GPL-2.0-only; the upstream archive and patch series are pinned in `SOURCE-MANIFEST.json` and `kernel/patches`. Each build exports its exact config, and `collect-sources` emits the complete patched tree. |
| OpenVMM | `openvmm` | MIT; see `openvmm/LICENSE`. Source is a private Git submodule and is available only to authorized users. |
| NVX guest/build sources | `guest`, `ubuntu`, `kernel`, `scripts/nvx_tools` | MIT; see `LICENSE`. |
| Alpine Linux packages | generated initramfs images | Per-package licenses. Every build emits `*.packages.json`; retain that manifest and collect the corresponding aports recipes and upstream sources before distributing an image. |
| Ubuntu Base 26.04.1 LTS and supplemental packages | generated Ubuntu initramfs and EROFS images | Per-package licenses. The base archive and every supplemental `.deb` are SHA-256 pinned. Retain the generated Ubuntu manifests and publish the matching `.dsc` plus all referenced source members before distributing an image. Ubuntu artifacts contain Ubuntu userland with the NVX kernel, not the Ubuntu kernel. |

The source manifest records pinned versions, commits, and the upstream Linux
archive digest. A binary release made with `scripts/nvx.py package
--include-source` includes separate project, patched Linux, Alpine, and Ubuntu source
archives. OpenVMM source is intentionally excluded; its MIT license notice is
included in the binary release.

Run `scripts/nvx.py collect-sources` before creating a source-inclusive
release. It saves the patched Linux source archive under `build/sources/linux`
and exact aports recipes plus `abuild verify`-checked upstream source files
under `build/sources/alpine`. Ubuntu source packages are stored under
`build/sources/ubuntu` with their verified source-index and `.dsc` metadata.
