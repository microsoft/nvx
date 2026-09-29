# Host attachment model

[Design index](../design.md)

Snapshot state contains guest-visible progress, not process-local resources.
Each external resource has a stable ID and a declarative reconstruction policy.

| Resource | Saved | Reconstructed or supplied on restore |
| --- | --- | --- |
| portb | Pending RX/TX bytes | Host serial endpoint, fresh process generation ID, and optional restore packet |
| console | Queue progress, staged RX, partial TX, endpoint identity, and reconnect policy | Eligible listener recreated at its captured or an approved fresh same-kind endpoint, required client connection, or supplied inherited terminal |
| Control console | Distinct attachment identity and broker policy, plus bounded broker state without queued records, host output, or receive credit; never the capability or peer identity | An explicitly approved same-user Unix socket or Windows named pipe, which may use a fresh endpoint identity, plus a fresh launcher-provided capability; the broker restarts with a fresh instance and epoch, keeping only its guest-side parser, any partially written guest record, and counters |
| network | Static identity, queue/packet progress, profile, and policy digest | Fresh in-process Consomme endpoint and matching egress policy; live host-loopback port forwards are rejected |
| filesystem | FUSE namespace, handles, cookies, root/object identity, access mode, and denied, allowed, and writable paths, or explicit dormant state | Fresh host-directory attachment with the same canonical path, target, mode, and denied, allowed, and writable paths; a dormant slot may stay unattached or bind a new attachment |
| sandbox block | Queue/device state, fixed roles, access, geometry, SHA-256 or immutable-generation identities, scratch policy, and paired-scratch materialization | Matching read-only layers plus private-copy or reflink COW scratch for clones, direct scratch after an exclusive resume claim, or a new same-geometry scratch file |
| Readiness endpoint | Nothing | Optional single-use Unix socket or named pipe supplied for one restore |

Attachment resolution happens before vCPU start. A supplied attachment must
reproduce the saved identity except that an eligible listener may replace only
its endpoint identity while preserving its stable ID, attachment and backend
kinds, reconnect policy, required flag, length, and timeout. Client, inherited,
disconnected, network, filesystem, and block attachments retain their stricter
saved contracts. Missing privileges, endpoint binding failures, changed egress
policy, replaced filesystem objects, or a wrong attachment kind fail restore
explicitly.
