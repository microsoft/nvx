# Package and source delivery

Package selection is explicit. A legacy shell package is:

```bash
python3 scripts/nvx.py package --transport legacy --binary-only
```

Binary-only mode requires an explicit acknowledgement because the matching
Linux and Alpine source must be published separately.

A broker package requires the staged agent and its agent-only initramfs:

```bash
python3 scripts/nvx.py package \
  --transport broker-ttrpc \
  --manifest-digest-output dist/nvx-broker.SOURCE-MANIFEST.sha256 \
  --binary-only
```

First materialize the release sources:

```bash
python3 scripts/nvx.py collect-sources --transport broker-ttrpc
```

This produces a patched Linux corresponding-source archive under
`build/sources/linux`. The broker initramfs contains only the static agent and
has no Alpine packages; the collected Alpine recipes/upstream sources apply to
the legacy profile. Use
`--transport legacy` when packaging only the shell image; source collection
does not require artifacts from the other profile. Then stage the binary
release with three separate source artifacts:

```bash
python3 scripts/nvx.py package --transport broker-ttrpc --include-source
```

The release contains:

```text
guest/initramfs.cpio.gz            # broker package: agent image for ACI-04
guest/initramfs-agent.cpio.gz      # same bytes under the explicit NVX name
guest/nvx-agent                    # exact embedded agent identity
SOURCE-MANIFEST.json               # canonical runtime manifest
source/nvx-project-source-0.1.0.tar.gz
source/nvx-linux-source-6.18.38.tar.gz
source/nvx-alpine-source-0.1.0.tar.gz
```

The broker manifest records explicit `broker-ttrpc` transport, OpenVMM source
and executable identities, kernel/agent-initramfs/agent hashes, ACI-04 source
revision, external agent input digest and size, ELF build ID, protocol schema
v1, microVM ABI v2, broker protocol v1, and control-contract revision. It also
records a canonical fingerprint of those inputs. `guest/initramfs.cpio.gz` is
the ACI-04 installer name; its bytes must equal the separately named agent
image. Legacy packages contain the unchanged shell `guest/initramfs.cpio.gz`
and no active agent.

The `.SOURCE-MANIFEST.sha256` output is deliberately outside the bundle and is
intended for independently authenticated deployment configuration. It is a
digest, not a signature, and merely generating it beside a package does not
establish trust. ACI-04 must receive the digest over an independently trusted
path before installing the digest-addressed immutable bundle. The canonical
manifest binds the complete initramfs hash; the source revision alone neither
reproduces nor authenticates the image bytes.

The broker package accepts only the reviewed ACI-03 input:
source `865984883584ae5569b1936981921fbe59c1f6e8`, SHA-256
`be0083fc1b7d77df7f14db705bf74ab52d173f029863643c6d3535307aa6e971`,
size `1,901,440` bytes, and build ID
`d7dd52b0b0cd298dbe69268f4347d026b117fb48`. Source provenance does not
imply byte identity; a same-source binary with a different digest is rejected.

Create a release archive through the mode-normalizing writer, not a generic
tar/ZIP extractor:

```bash
python3 scripts/nvx.py archive-release \
  --bundle dist/nvx-0.1.0-linux-kvm-broker-ttrpc \
  --output dist/nvx-0.1.0-linux-kvm-broker-ttrpc.tar.gz
```

The archive records directories as `0755`, OpenVMM and `nvx-agent` as `0755`,
and kernels, initramfs images, manifests, configs, notices, and checksums as
`0644`. Before extraction, one package validator rejects excessive archive/member
sizes, counts, paths, compression ratios, duplicate or case-colliding names,
links, devices, special entries, and unexpected modes. The same validator is
used for archive creation, installation, and the publication gate. It checks
every required alias, config, package manifest, trust sidecar, checksum
relationship, and the selected initramfs structure. Installation stages and
verifies the complete selected profile before promotion and rolls back every
replaced or removed file on any `BaseException`. A persistent transaction
journal and backups are deleted only after commit or successful rollback; a
later install recovers any interrupted rollback before proceeding.

## Broker publication live gate

Development CI does not boot the broker artifact, so it publishes only legacy
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
installed APK and runs `abuild fetch` plus `abuild verify`. If Python
initramfs variants are present, `collect-sources` includes their package
manifests automatically.

OpenVMM is MIT licensed: retain its notice, but its source does not have to be
published merely because it is aggregated with Linux. See
`THIRD_PARTY_NOTICES.md`.
