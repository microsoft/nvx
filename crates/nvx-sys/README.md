# nvx-sys

`nvx-sys` is the OS layer that NVX's crates share, to which the OS modules of
the [`nvx`](../../nvx-rs/README.md) crate move. It holds the error type of
NVX's operations in its `error` module, which `nvx` re-exports as
`nvx::Error`, `nvx::ErrorCode`, `nvx::ErrorKind`, and `nvx::Result`. The
nightly CI run publishes it on crates.io before `nvx`, which requires it at
exactly their shared version; see
[crates.io releases](../../doc/ci.md#cratesio-releases). It makes no stability
guarantee: its API can change in any release.

## Errors

Every fallible NVX operation returns an `Error`, which `nvx` re-exports as
`nvx::Error`: a message that callers can show as is, an `ErrorCode`, which
determines the error's `ErrorKind`, and an optional cause.

| `ErrorKind` | Meaning |
| --- | --- |
| `Unavailable` | A missing or unusable artifact, hypervisor, or release, a guest image without a required control feature, or an unsupported host |
| `Unsupported` | A well-formed request that NVX cannot represent or enforce, such as a policy beyond a limit of OpenVMM or the guest agent |
| `Exited` | The VM's OpenVMM process exited |
| `Io` | A host I/O failure, or state that NVX recorded on the host and cannot read |
| `NotFound` | State that an operation names and that does not exist on the host, such as the store of a sandbox that was never created or that a removal deleted |
| `Protocol` | A control session that failed, or a request that the guest agent refused |
| `TimedOut` | A deadline that passed |
| `Other` | Any other failure |

### Error codes

An `ErrorCode` is finer than an `ErrorKind`: callers tell errors apart by their
codes, and by stable fragments of their messages, such as the names of options,
fields, files, and processes, since the wording of a message may change. The
[CLI](../../nvx-rs/README.md#the-nvx-rs-cli) prints the code of each error that
it reports.

`ErrorCode::as_str` spells a code in lowercase ASCII words joined by hyphens. A
failure that a caller can act on has a code of its own, which names the subject
of the failure and then its condition, such as `sandbox-not-running`. Any other
failure has the code of its kind, which is spelled as the kind, such as `io`,
and which an `ErrorKind` converts into. A code keeps its spelling, its kind, and
its meaning, and is never reused for another failure: a failure that callers
come to act on gets a new code, so a caller treats a code that it does not know
as a failure of the operation. The `argument-*` codes are those of the CLI's
usage errors, which no library error has.

| Code | `ErrorKind` | Meaning |
| --- | --- | --- |
| `unavailable` | `Unavailable` | A dependency is missing or unusable, other than as a finer code says: install metadata, the hypervisor, a release, a distro layer or scratch image that the request names, or a host that NVX does not support |
| `unsupported` | `Unsupported` | NVX refuses the request: options that it cannot combine, a value that it does not accept, or a policy that it cannot represent or enforce. The message names the option or field |
| `exited` | `Exited` | OpenVMM exited, as when a start fails, whose message names the category and status of OpenVMM's outcome report when it has one |
| `io` | `Io` | A host I/O operation failed, or state that NVX recorded on the host is unreadable or malformed |
| `not-found` | `NotFound` | Other state that the operation names does not exist on the host |
| `protocol` | `Protocol` | The control session failed otherwise: the peer violated the protocol, reset the session, or failed authentication |
| `timed-out` | `TimedOut` | Another deadline passed, such as that of a stop or of a control request |
| `other` | `Other` | Any other failure |
| `argument-unrecognized` | `Unsupported` | A usage error: the command line holds an argument that the command does not take, such as an unknown option |
| `argument-missing` | `Unsupported` | A usage error: a required argument, or the value of an option, is missing |
| `argument-invalid` | `Unsupported` | A usage error: an argument has an invalid value, such as one that is not among its choices, not a number, not of its form, or not valid Unicode |
| `artifact-missing` | `Unavailable` | An artifact that the operation runs, such as OpenVMM, the guest kernel, or an initramfs, does not exist, or no artifact root holds them |
| `guest-features-missing` | `Unavailable` | The guest image lacks control features that the operation needs, which the message names |
| `sandbox-not-provisioned` | `NotFound` | No sandbox is provisioned at its state directory, which does not exist, is not a plain directory, or holds no configuration |
| `sandbox-provisioned` | `Other` | The sandbox is provisioned already, or was provisioned again while a removal deleted it |
| `sandbox-not-running` | `Other` | The operation needs the sandbox's VM, which is not running |
| `sandbox-running` | `Other` | The operation needs a stopped sandbox, but its VM, or a launch of it, may still run |
| `sandbox-exited` | `Exited` | The sandbox's VM exited without a stop: the transition that finds it clears its runtime record, and a start launches it again |
| `sandbox-busy` | `TimedOut` | Another lifecycle transition of the sandbox held its lock until the deadline passed |
| `runtime-mismatch` | `Other` | The sandbox's VM runs under another runtime identity than the artifacts', whose differing parts the message names, so only a stop, which terminates it, applies |
| `vmm-indeterminate` | `Other` | Whether the OpenVMM process of the sandbox runs cannot be determined, so its records remain for a later attempt |
| `format-unsupported` | `Io` | A record or report has a format or schema version that this build does not read, such as the state of a sandbox that another build provisioned or started |
| `foreign-files` | `Other` | A directory of NVX's state, such as a sandbox's state directory or a template store, holds files that NVX does not own, which the message names and NVX keeps |
| `outcome-report-exists` | `Other` | A file exists at the path of the outcome report to write |
| `scratch-claimed` | `TimedOut` | Another sandbox claims the scratch image |
| `exec-refused` | `Protocol` | The guest agent refused the workload, as for a working directory that it cannot use |
| `endpoint-closed` | `Protocol` | OpenVMM closed the control endpoint, as it does when the VM shuts down |
| `template-ineligible` | `Unavailable` | The restore window of a template's generation, which the time contract of its capture sets, excludes the host's time, so no clone may restore it now: its capture is too old, or the host's clock is behind the capture's |
| `template-tombstoned` | `Unavailable` | A tombstone quarantines the template's generation, which takes no new clones, while the clones that lease it keep their leases |
| `sandbox-leased` | `Other` | The sandbox holds a lease of a template's generation, which allows no other lease, and one launch of its clone, until a release ends it once its VM has exited |
