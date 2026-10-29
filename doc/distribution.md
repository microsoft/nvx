# Package and source delivery

`package` stages a release directory, `dist/<version>` by default, from the
built OpenVMM binary, `nvx-rs` executable, and guest artifacts. [Build](build.md)
lists the artifacts and provenance it requires and the binary release layout.
Every package is either binary-only or source-inclusive, and you must choose
one.

Every package ships the release build of `nvx-rs`, the Rust command-line
interface that runs NVX's microVMs and sandboxes ([Usage](usage.md#nvx-rs)):
`bin/nvx-rs`, a
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

[`download`](usage.md#download) of `nvx-rs` verifies the package against its
`SHA256SUMS` and installs OpenVMM, the guest artifacts, and their provenance
into the checkout, everything or nothing. It leaves the checkout's `nvx-rs`,
which Cargo builds, alone, and so installs releases from before packages
shipped `nvx-rs` too. It warns when a release, such as that of an ancestor,
declares another microVM ABI or control contract than the `nvx-rs` that
installs it speaks.

CI follows the binary-only path. On a `dev` push that changes more than
documentation, on a ready pull request from a branch of this repository that
changes an input of the platform jobs, and on scheduled or manually
dispatched full runs, the `linux-kvm`, `linux-mshv`,
and `windows-whp` platform jobs each stage
`dist/nvx-<version>-<platform>` with `--binary-only`, then archive it as
`.tar.gz` on Linux and `.zip` on Windows through the
[`package-release`](../.github/actions/package-release/action.yml) action,
with the `nvx-rs` and provenance that the NVX CLI job of their operating
system built and tested. Only `dev` pushes upload the archives, so pull
requests and full runs prove that their packages build without publishing
them. The development release job checks that each package ships OpenVMM and
`nvx-rs` before it publishes them.
[Continuous integration](ci.md) describes the jobs that gate and publish the
resulting development release.

A documentation-only push, in which every changed path is under `doc/` or ends
in `.md`, skips the platform jobs, as
[Change classification](ci.md#change-classification) describes. Such a commit
gets no packages and no development release, so
[`nvx-rs download`](usage.md#download) installs the package of its nearest
released first-parent ancestor.

## The nvx crate

[`nvx-rs/`](../nvx-rs/README.md) is also published on crates.io as the `nvx`
crate, a preview whose versions are pre-releases, `0.1.0-alpha.N`, which Cargo
selects only for a requirement that names a pre-release. The nightly CI run
publishes a new version from `dev` whenever the crate changed, with the next
number, as [crates.io releases](ci.md#cratesio-releases) describes, so a
change reaches crates.io within a day of its merge.

The version that `nvx-rs/Cargo.toml` declares is the lowest that CI
publishes. A pull request changes it to start another series, such as
`0.2.0-alpha.1`. Only `nvx`'s version changes in `nvx-rs/Cargo.lock` and
`tests/runtime-adapter/Cargo.lock`. The `bundled` feature of the published
crate downloads the release that
[`nvx-rs/artifacts.json`](../nvx-rs/artifacts.json) pins and fails unless that
release declares the crate's microVM, time, restore-packet, control-session,
and control contracts, which CI checks before it publishes. When a contract has
changed, publication stays blocked until a pull request repins a compatible release;
see [Bundled artifacts](../nvx-rs/README.md#bundled-artifacts).

When CI cannot publish, as when the newest version on crates.io records no
commit that `dev` holds, a pull request declares the next version as above,
and once it merges, a crate owner publishes that version from a fresh clone of
`dev`, which needs no submodule:

```bash
git clone --branch dev https://github.com/microsoft/nvx.git nvx-crate
cd nvx-crate/nvx-rs
git ls-files --eol . | grep -E 'w/(crlf|mixed)'  # prints nothing
cargo check --locked --features bundled          # stages the pinned release
cargo publish --locked --dry-run
cargo publish --locked
```

`cargo publish` packs the files of the working tree, refuses uncommitted
changes, and records the commit in the package's `.cargo_vcs_info.json`, but
Git does not count line endings as changes. `.gitattributes` checks `nvx-rs/`
out with LF line endings on every platform, whatever `core.autocrlf` says,
since [microsoft/nvx#764](https://github.com/microsoft/nvx/pull/764). A clone
that checked its files out with CRLF line endings before then keeps them until
it checks them out again, and packs them: `0.1.0-alpha.2` came from such a
clone, so its sources differ from the commit that it records. A fresh clone
packs the bytes of its commit, which the `git ls-files` check confirms.
