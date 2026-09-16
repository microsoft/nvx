# Package and source delivery

The `simple` profile is the default for packaging, downloading, source
collection, and sandbox commands. A simple shell package is:

```bash
python3 scripts/nvx.py package --binary-only
```

Binary-only mode requires an explicit acknowledgement because the matching
Linux and Alpine source must be published separately.

Without `--destination`, `package` stages the default simple profile under
`dist/VERSION` and the opt-in broker profile under `dist/VERSION-broker`.
These staging defaults are separate from the platform-qualified published
archive names.

Release archives for the default profile keep the established names
`nvx-VERSION-PLATFORM.tar.gz` and `nvx-VERSION-PLATFORM.zip`; they do not add a
`-simple` suffix. Opt-in broker archives use
`nvx-VERSION-PLATFORM-broker.tar.gz` or
`nvx-VERSION-PLATFORM-broker.zip`. The package metadata value remains
`broker-ttrpc`.

A broker package requires explicit `--transport broker-ttrpc`, the staged
agent, and its agent-only initramfs:

```bash
python3 scripts/nvx.py package \
  --transport broker-ttrpc \
  --manifest-digest-output dist/0.1.0-broker.SOURCE-MANIFEST.sha256 \
  --binary-only
```

First materialize the release sources:

```bash
python3 scripts/nvx.py collect-sources --transport broker-ttrpc
```

This produces a patched Linux corresponding-source archive under
`build/sources/linux`. The broker initramfs contains only the static agent and
has no Alpine packages; the collected Alpine recipes/upstream sources apply to
the simple profile. Use
`--transport simple` when packaging only the shell image; source collection
does not require artifacts from the other profile. Then stage the binary
release with the project and Linux source artifacts:

```bash
python3 scripts/nvx.py package --transport broker-ttrpc --include-source
```

The release contains:

```text
guest/initramfs.cpio.gz            # broker package: agent image
guest/initramfs-agent.cpio.gz      # same bytes under the explicit NVX name
guest/nvx-agent                    # exact embedded agent identity
SOURCE-MANIFEST.json               # canonical runtime manifest
source/nvx-project-source-0.1.0.tar.gz
source/nvx-linux-source-6.18.38.tar.gz
```

Simple packages additionally contain `source/nvx-alpine-source-0.1.0.tar.gz`
when `--include-source` is selected.

The broker manifest records explicit `broker-ttrpc` transport, OpenVMM source
and executable identities, kernel/agent-initramfs/agent hashes, guest-agent source
revision, supplied executable digest and size, ELF build ID, protocol schema
v2, microVM ABI v2, control-session protocol v1, control-contract revision, and
the descriptive unversioned agent capability
`runtime_contract: startup-modes-session-operations`. The runtime contract is
not an additional ABI or protocol version. The manifest also records a
canonical fingerprint of those inputs. In a broker package,
`guest/initramfs.cpio.gz` must equal the separately named agent image. Simple
packages contain the unchanged shell `guest/initramfs.cpio.gz` and no active
agent.

The `.SOURCE-MANIFEST.sha256` output is deliberately outside the bundle and is
intended for independently authenticated deployment configuration. It is a
digest, not a signature, and merely generating it beside a package does not
establish trust. The sandbox control plane client must receive the digest over
an independently trusted path before installing the digest-addressed immutable
bundle. The canonical manifest binds the complete initramfs hash; the source
revision alone neither reproduces nor authenticates the image bytes.

The broker package accepts only the supplied executable matching the
`guest_agent` pins in `SOURCE-MANIFEST.json`, including its source revision,
SHA-256, size, build ID, static target, protocol schema, startup modes, and
descriptive runtime contract. Source provenance does not imply byte identity;
a same-source binary with a different digest is rejected. The authenticated
guest-agent contract declares the exact supported startup modes
(`agent-ready`, `image-entrypoint`), and the control plane rejects an
incompatible declaration before launch.

Package creation copies all selected artifacts into a separate staging directory,
then verifies provenance, runtime identities, initramfs contents, and checksums
before publishing. The staged kernel and config must match the hashes admitted
by kernel build provenance before copying, even if the build outputs change
during packaging. `--force` retains the previous release until validation
succeeds. Directory publication and the separate broker manifest digest are
protected by exclusive interprocess locks on both destination paths, acquired in
path order. Destination preflight, backup, promotion, rollback, and cleanup all
run while holding both locks, including the final `--force` check. Publishers
sharing either destination are serialized. The persistent sibling
`.NAME.publish.lock` files must not be deleted to bypass a lock.
Each publication owns uniquely created `.NAME.previous-*` backup directories;
it never restores or removes another publication's backups. The bundle and digest
are rolled back together if either promotion fails, including interruption
immediately after a successful rename. Promotion intent is recorded before each
rename so rollback removes the new output before restoring its own backup.
These locks serialize publishers; readers must not treat two separate path
replacements as one atomic filesystem operation.

Create a release archive through the mode-normalizing writer, not a generic
tar/ZIP extractor:

```bash
python3 scripts/nvx.py archive-release \
  --bundle dist/nvx-0.1.0-linux-kvm-broker \
  --output dist/nvx-0.1.0-linux-kvm-broker.tar.gz
```

The archive records directories as `0755`, OpenVMM and `nvx-agent` as `0755`,
and kernels, initramfs images, manifests, configs, notices, and checksums as
`0644`. Before extraction, one package validator rejects excessive archive/member
sizes, counts, paths, compression ratios, duplicate or case-colliding names,
links, devices, special entries, and unexpected modes. ZIP and fixed-layout ZIP64
central directories are limited to 1 MiB and 256 members before `ZipFile` reads
them; the raw scan counts headers rather than trusting the end record.
Only stored and deflate ZIP members are accepted. Before `ZipExtFile` opens any
member, a bounded scan checks actual output length, stream completion, compressed
data bounds, and CRC against the headers; LZMA, BZIP2, and other methods are rejected.
The same validator is used for archive creation, installation, and the publication
gate. It checks every required alias, config, package manifest, trust sidecar,
checksum relationship, and the selected initramfs structure. Newc verification
requires every numeric header field to be exactly eight ASCII hexadecimal digits,
matching the kernel parser. Archive creation requires the output to be outside
the bundle and all of its descendants, including paths that become descendants
after alias resolution. It validates a temporary `.zip` or `.tar.gz` completely
before atomically replacing the output, so validation failure preserves any
previous archive. Downloaded manifests must
declare the unchanged microVM ABI 2 and control-session protocol 1 with strict
integer types and match the local control contract before any runtime mutation.
Installation holds an exclusive per-runtime interprocess lock across recovery,
staging, promotion, commit, and rollback. A contending installer waits up to
60 seconds before reporting a lock timeout without touching transaction state.
The persistent `build/.install.lock` file must not be deleted to bypass an active
lock; the operating system releases the lock when its process exits.
Installation verifies the complete selected profile before promotion and attempts
rollback on any uncommitted `BaseException`, including failure to persist the
commit journal. Before undoing any file, it persists a `rolling-back` phase, even
if the commit write published successfully before raising. If that rollback
state cannot be persisted, no files are undone and the journal and backups remain.
A later install resumes active or rolling-back transactions; committed
transactions retain the installed runtime and only need backup cleanup.
Journals and backups are deleted only after commit or successful rollback.
Staged file contents and modes are flushed with `fsync` before promotion. Each
backup, promotion, and restore flushes the destination parent followed by the
source parent; directory creation also flushes the ancestor links. Every journal
transition flushes its temporary file, atomically replaces the journal, then
flushes the journal directory. Backup cleanup follows durable promoted
destinations and a durable commit, or durable restoration and a `rolled-back`
journal. Recovery retries interrupted directory flushes even when a restore or
unlink has already completed, and flushes a visible terminal journal before
cleanup.

Directory flushing is filesystem-dependent. Linux uses directory `fsync` where
supported. The Windows standard library flushes file contents but its CRT cannot
open directory handles for `fsync`, so Windows does not provide the same
power-loss guarantee for directory entries. Only explicit unsupported-directory
errors are tolerated; other flush failures stop installation or preserve recovery
state rather than reporting success.

## Broker publication live gate

Development CI does not boot the broker artifact, so it publishes only simple
assets. `publish-development-release` rejects broker archives unless its
`broker-live-gate-contracts` input supplies one independently authenticated
record for each archive:

```text
PLATFORM|ARCHIVE|MANIFEST_DIGEST|PROOF|TRUSTED_PROOF_SHA256
```

`PROOF` is canonical JSON with `format: 1`, `result: "passed"`, the exact
archive SHA-256, raw and canonical bundle-manifest digests,
platform/backend/transport, the complete `SHA256SUMS` artifact map, and exact
OpenVMM, kernel, kernel-config, initramfs package-manifest, complete initramfs,
and agent SHA-256 values. Its `checks` object must contain exactly successful
`control_auth`, `get_guest_info`, `bootstrap`, `wait_ready`, and `shutdown`
booleans plus `get_guest_info_self_sha256` equal to the packaged agent. The
publish action independently computes the archive digest and requires the
proof to match it before upload. `TRUSTED_PROOF_SHA256` must arrive from the
separately authenticated live smoke/E2E job; the proof is never trusted from
inside the archive. Inspection-only output is not a proof. No such proof or
job exists in the current workflow, so broker publishing remains disabled.

Linux is GPL-2.0-only, so a distributor of `vmlinux` must make its complete
corresponding source available. Alpine packages retain their individual
licenses. The collector uses the exact aports commit embedded in every
installed APK and runs `abuild fetch` plus `abuild verify`. For additional
initramfs variants, pass their package manifests explicitly to
`collect-alpine-sources`.

OpenVMM is MIT licensed: retain its notice, but its source does not have to be
published merely because it is aggregated with Linux. See
`THIRD_PARTY_NOTICES.md`.
