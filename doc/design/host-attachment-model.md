# Host attachment model

[Design index](../design.md)

Snapshot state contains guest-visible progress, not process-local resources.
Each external resource has a stable ID and a declarative reconstruction policy.

| Resource | Saved | Reconstructed or supplied on restore |
| --- | --- | --- |
| portb | Pending RX/TX bytes | Host serial endpoint, fresh process generation ID, and optional restore packet |
| console | Queue progress, staged RX, partial TX, policy | Listener, client connection, or supplied handle |
| Control console | Distinct console attachment, authenticated broker state, and reconnect epoch | Same-user Unix socket or Windows named pipe plus a fresh capability |
| network | Static identity, queue/packet progress, profile and policy identity | Fresh in-process Consomme endpoint and matching egress policy |
| filesystem | FUSE namespace, handles, cookies, root/object identity, access mode | Fresh host-directory attachment |
| sandbox block | Queue/device state, fixed roles, access, geometry, read-only layer identities, and scratch policy | Matching read-only layers plus a verified private paired scratch copy, or a new same-geometry scratch file |

Attachment resolution happens before vCPU start. Missing privileges, endpoint
binding failures, changed egress policy, replaced filesystem objects, or a
wrong attachment kind fail restore explicitly.
