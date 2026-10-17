# Package and source delivery

`package` stages a release directory, `dist/<version>` by default, from the
built OpenVMM binary, `nvx-rs` executable, and guest artifacts. [Build](build.md)
lists the artifacts and provenance it requires and the binary release layout.
Every package is either binary-only or source-inclusive, and you must choose
one.

Every package ships the release build of `nvx-rs`, the Rust command-line
interface that `nvx.py` runs ([Usage](usage.md#nvx-rs)): `bin/nvx-rs`, a
static `x86_64-unknown-linux-musl` executable, in the Linux packages, and
`bin/nvx-rs.exe`, an `x86_64-pc-windows-msvc` executable with a static C
runtime, in the Windows package. `provenance/nvx-rs.provenance.json` binds it
to the digest of the `nvx-rs` sources that built it, the Rust release, the
target, and the `cli` feature, and the packaged `SOURCE-MANIFEST.json` records
the same in its `nvx_rs` section with the executable's SHA-256. `package`
requires `nvx-rs/target/release/nvx-rs[.exe]` and its provenance in `build/`,
which must describe it as such a build of the checkout's sources; see
[`record-nvx-rs-provenance`](usage.md#record-nvx-rs-provenance).

## Binary-only packages

Stage a binary release and SHA-256 manifest:

```bash
python3 scripts/nvx.py package --binary-only
```

Binary-only mode requires an explicit acknowledgement because the matching
Linux, Alpine, Ubuntu, and Azure Linux source must be published separately.

## Source-inclusive packages

First materialize the release sources:

```bash
python3 scripts/nvx.py collect-sources
```

`collect-sources` reads the package manifests of the built guest artifacts, so
run `build-guest --guest all` first. It requires the kernel, Alpine, and Ubuntu
artifacts, but not the Azure Linux ones. It also needs Docker with the Linux
engine, which fetches and verifies the Alpine upstream sources in an Alpine
container and builds the Linux source archive, and `gpgv`, which authenticates
the Ubuntu source indexes.

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

`collect-sources` does not collect Azure Linux corresponding source, so
`--include-source` omits `initramfs-azurelinux.cpio.gz` and its package
manifest from the staged release and from the packaged `SOURCE-MANIFEST.json`.
Distribute the Azure Linux guest only from a binary-only package while its
corresponding source is published separately.

Linux is GPL-2.0-only, so a distributor of `vmlinux` must make its complete
corresponding source available. Alpine packages retain their individual
licenses. The collector uses the exact aports commit embedded in every
installed APK and runs `abuild fetch` plus `abuild verify`. Its manifest lists
the executable recipe files, and the Alpine source archive takes its file modes
from that list rather than from the collecting host, which may not store them.

Ubuntu artifacts use Ubuntu userland with the NVX kernel. The collector
deduplicates exact source package name/version pairs from both Ubuntu
manifests, downloads the matching `.dsc` and source members from Canonical's
archive, verifies each source index through its signed `InRelease` file and the
pinned Ubuntu archive keyring, and then verifies the indexed SHA-256 metadata
before packaging. Source collection and release staging also require each
Ubuntu artifact to match the artifact name and SHA-256 embedded in its companion
manifest. Exact versions that have left the live suite indexes are located
through Canonical's Launchpad publishing history and resolved from a signed
historical `snapshot.ubuntu.com` index. The signed release metadata, keyring,
and raw Launchpad responses are retained with their URLs and SHA-256 digests.
Newer source versions are never substituted.

OpenVMM is MIT licensed: retain its notice, but its source does not have to be
published merely because it is aggregated with Linux. See
`THIRD_PARTY_NOTICES.md`.

The `nvx-rs` executables are MIT licensed NVX code that statically links the
Rust standard library and permissively licensed crates, with musl and LLVM's
`libunwind` on Linux and the Microsoft C runtime on Windows.
`THIRD_PARTY_NOTICES.md` lists each with its license; `nvx-rs/Cargo.lock`
pins every crate by checksum.

## Release archives

Turn a staged release directory into one deterministic archive:

```bash
python3 scripts/nvx.py archive-release \
  --source dist/0.1.0 \
  --destination dist/nvx-0.1.0.tar.gz
```

`archive-release` checks the staged files against their `SHA256SUMS` before
it writes a `.tar.gz` or `.zip` archive outside the staged directory; see
[`archive-release`](usage.md#archive-release). `SHA256SUMS` lists every
packaged file, `bin/nvx-rs[.exe]` and its provenance included, and every
archive marks the executables in `bin/` executable.

CI follows the binary-only path. On a `dev` push, or a ready pull request from
a branch of this repository, that changes more than documentation, the
`linux-kvm`, `linux-mshv`, and `windows-whp` platform jobs each stage
`dist/nvx-<version>-<platform>` with `--binary-only`, then archive it as
`.tar.gz` on Linux and `.zip` on Windows through the
[`package-release`](../.github/actions/package-release/action.yml) action,
with the `nvx-rs` and provenance that the NVX CLI job of their operating
system built and tested. Only `dev` pushes upload the archives, so a pull
request proves that its packages build without publishing them. The
development release job checks that each package ships OpenVMM and `nvx-rs`
before it publishes them.
[Continuous integration](ci.md) describes the jobs that gate and publish the
resulting development release.

A documentation-only push, in which every changed path is under `doc/` or ends
in `.md`, skips the platform jobs, as
[Change classification](ci.md#change-classification) describes. Such a commit
gets no packages and no development release, so
[`download`](usage.md#download) installs the package of its nearest released
first-parent ancestor.
