# Snapshot sharing and host storage

[Design index](../design.md)

Private COW restore prevents a VM's writes from modifying the reusable
artifact. It does not guarantee that clean physical pages are unshared across
VMs mapping the same file. Guest kernel, agent, and cached workload pages can
all originate in `memory.bin`; dropping the guest page cache before capture
does not by itself eliminate shared host physical pages or cache side channels.

## Trust-domain policy (Proposed)

Production artifact policy must distinguish write isolation from page-sharing
isolation. A workload-start image contains pages addressable by the workload;
sharing those pages across mutually untrusted tenants creates an avoidable
shared-page timing surface. Use tenant-scoped artifacts or independently
instantiated backing files instead of treating COW as tenant isolation.

A fleet-shared platform template requires a narrower, verified argument: it
contains no tenant image bytes, configuration, secrets, or workload state, and
the workload cannot map the captured agent/kernel memory or configuration
region. Namespace/device policy and agent handling of secrets are part of that
argument. It is not enough to assert that PID 1 had not mounted a layer, or
that a template carries the `platform` label. The current VMM does not prove
these guest provenance and addressability properties. Security review must
also consider agent secret-dependent accesses and host memory deduplication.

An instance checkpoint has one claimed continuation. Its single-use rule
prevents accidental forks but does not replace artifact confidentiality,
retention policy, or protection against host-side copying.

## Independent backing files (Proposed)

Per-tenant instantiation can separate file-backed page caches while retaining
shared disk extents on suitable filesystems:

| Reference or copy | File identity | Isolation implication |
| --- | --- | --- |
| Hard link or symlink | Same backing file | Does not separate the file-backed page cache |
| Linux reflink (`FICLONE`) | Distinct inode, initially shared extents | Distinct file page caches, subject to host deduplication policy |
| Windows ReFS block clone | Distinct file streams, initially shared extents | Physical-frame separation under COW mappings requires validation on the deployed Windows build |
| Independent full copy | Distinct backing file | Avoids same-file sharing, subject to host deduplication policy |

XFS/btrfs reflinks and ReFS block clones are optimization options, not mandatory
snapshot storage formats. NTFS and filesystems without cloning can use copies.
The current Windows snapshot fallback is a dense copy, not an implemented
ReFS per-tenant artifact service. KSM/page combining or other memory merging
must not recreate sharing where the deployment relies on separate files;
ReFS on-disk clone semantics alone are not proof of separate physical frames.

Automatic RAM hard-link publication during capture is a different optimization:
it transfers the exact stopped source backing into one immutable generation.
It must not be confused with creating separate tenant backing files. Placement,
instantiation, access control, and garbage collection remain host-service work.

Capacity planning must measure clean resident pages per backing generation,
dirty private pages per restored VM, and Windows COW commit charge separately.
A newly instantiated file may have a cold page cache even when the source
template is warm. Measure first restore and steady-state restore separately;
do not assume a near-zero on-disk clone cost means a free memory working set.

## DAX and other memory-backed layers (Proposed)

The current profile exposes neither virtio-pmem/DAX layers nor `phram` image
carriers. Retaining virtio-blk keeps read-only enforcement in the device I/O
path and avoids mapping shared image pages directly into the guest. DAX would
need host-enforced write protection, a restricted device surface, and an
explicit trust-domain sharing policy. It would also require suitable
uncompressed EROFS images, giving up the chosen compression benefits.

Directly mapping an entire image also has guest-physical-address and kernel
metadata costs even when little of it is used. A future small-image carrier
or DAX profile should be justified by measured working sets and security
analysis, not presented as an existing size-based automatic fallback.
