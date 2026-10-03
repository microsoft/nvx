# aci_edge_sandboxes

`aci_edge_sandboxes` is the Rust interface to the ACI Edge Sandboxes lifecycle.
It exposes provision, start, exec, stop, and deprovision through a pluggable
backend. The default backend drives the `openvmm` binary directly; no daemon or
Python tooling is involved at runtime.

Each sandbox is a microVM that runs the NVX guest's Alpine Linux userland
directly from its initramfs. There are no image layers, scratch disks, or
container namespaces; workloads run as a non-root user in the guest itself, and
guest state lives in memory until the sandbox stops.

The Cargo package, library, and directory are named `aci_edge_sandboxes`.

## Quick start

```rust,no_run
use aci_edge_sandboxes::openvmm::{Hypervisor, OpenVmmConfig};
use aci_edge_sandboxes::{ExecRequest, NetworkPolicy, AciEdgeSandbox, ProvisionRequest};

fn main() -> aci_edge_sandboxes::Result<()> {
    let config = OpenVmmConfig::from_release_dir(
        "/opt/nvx",              // An extracted NVX release archive.
        Hypervisor::Kvm,         // Hypervisor::Whp on Windows.
        OpenVmmConfig::default_state_root()?,
    )?;
    let client = AciEdgeSandbox::openvmm(config)?; // Or OpenVmmConfig::discover().

    let request = ProvisionRequest::new().with_network(NetworkPolicy::deny_all());
    let sandbox = client.provision(&request)?.sandbox_id;

    client.start(&sandbox)?;
    let output = client
        .exec(&sandbox, &ExecRequest::command_line("echo hello"))?
        .wait_with_output()?;
    println!("{:?}: {}", output.outcome, String::from_utf8_lossy(&output.stdout));
    client.stop(&sandbox)?;
    client.deprovision(&sandbox)?;
    Ok(())
}
```

[`examples/lifecycle.rs`](examples/lifecycle.rs) is a runnable version with
command-line arguments.

## Lifecycle

| Operation | Transition | Result |
| --- | --- | --- |
| `AciEdgeSandbox::provision` | (none) → provisioned | `SandboxId` (`aci-edge-sandboxes:<32 hex>`) and optional metadata |
| `AciEdgeSandbox::start` | provisioned → running | optional metadata |
| `AciEdgeSandbox::exec` | running → running | live stdout and stderr, then an `ExecOutcome` |
| `AciEdgeSandbox::stop` | running → provisioned | optional metadata |
| `AciEdgeSandbox::deprovision` | provisioned → (none) | optional metadata; the ID becomes stale |

`ExecOutcome` distinguishes `Exited(code)`, `Signaled(signal)`, `TimedOut`,
`Cancelled`, and `Failed(reason)`. An `Err` from `Execution::wait` means the
outcome could not be determined, for example because the VM stopped.

Request types serialize with the contract's JSON field names (`readonlyPaths`,
`memoryMib`, `commandLine`, and so on) and reject unknown fields. Every
provision field is optional, so `{}` is a valid provision request. The wire
envelope (`version`, `phase`, and `containment`) belongs to the caller.

## Architecture

- `AciEdgeSandbox` is the entry point. It validates every request in the order the
  contract requires, then calls a backend:
  1. structural checks, reported as `malformed_request` or `malformed_id`;
  2. capability checks against the backend's `Capabilities`, reported as
     `policy_validation`;
  3. deterministic backend policy checks, such as guest argument and egress
     rule limits, through side-effect-free validation hooks;
  4. host-dependent checks, such as artifact or hypervisor availability, only
     when performing the operation.

  A rejected request never runs anything.
- `Backend` is an object-safe trait with one method per operation plus
  `capabilities`, `probe`, and provision/exec validation hooks. Implement those
  hooks with the same deterministic policy checks used by the operations;
  validation must not inspect host files, create state, or launch processes.
  Backends push workload output into
  `OutputSink`s and return an `ExecControl` for waiting and cancellation. The
  facade decides how output reaches the caller: operating-system pipes in the
  synchronous API, and in-process streams in the asynchronous API. The facade's
  sinks return an `OutputCloser` from `OutputSink::close_handle`; close it on
  cancellation or timeout when a write could wait for a caller that stopped
  reading.
  `ExecIo::stdin` supplies an interruptible `InputSource`. Clone its
  `InputCloser` to wake pending reads on cancellation or timeout, and finish
  input workers before reporting completion. Synchronous callers still receive
  the original OS-pipe writer; writes fail once the backend has closed input.
- Backends in this crate:
  - `openvmm::OpenVmmBackend` (feature `openvmm`, default) drives the `openvmm`
    executable.
  - `testing::MockBackend` (feature `testing`) implements the state machine in
    memory for consumers' unit tests.
- `AsyncAciEdgeSandbox` (feature `async`) wraps `AciEdgeSandbox` for Tokio. Lifecycle calls run on
  the blocking pool, and output arrives as `AsyncRead` streams.

To add a backend, implement `Backend`. Declare only the capabilities it can
enforce, and report state-machine violations with the codes listed in the
[error mapping](#error-mapping).

## OpenVMM backend

### Configuration

`OpenVmmConfig` names the artifacts, the hypervisor, and a state root:

| Constructor | Artifacts |
| --- | --- |
| `OpenVmmConfig::discover` | located by `Artifacts::discover`; the hypervisor comes from `NVX_HYPERVISOR` or the platform default, and the state root is the default one |
| `OpenVmmConfig::from_artifacts` | an `Artifacts` value |
| `OpenVmmConfig::from_release_dir` | `bin/openvmm[.exe]`, `guest/vmlinux`, and `guest/initramfs.cpio.gz` of an extracted release |
| `OpenVmmConfig::from_repo_layout` | `openvmm/target/release/openvmm[.exe]`, `build/vmlinux`, and `build/initramfs.cpio.gz` of a checkout |
| `OpenVmmConfig::new` | Explicit paths |

The release and repository constructors require the `SOURCE-MANIFEST.json`
control contract `nvx-microvm-v2-control-v1`. The initramfs is the Alpine
image: its userland is the workloads' environment, and its managed agent
serves the control console.

### Artifacts

`Artifacts` holds the OpenVMM executable, the guest kernel, and the Alpine
initramfs.

`Artifacts::discover` takes the first source that is present:

1. `NVX_OPENVMM`, `NVX_KERNEL`, and `NVX_INITRD`, which must be set together.
2. `NVX_ARTIFACTS_DIR`, naming an extracted release or a repository checkout.
   A directory that contains a release's `bin/openvmm[.exe]`, `guest/vmlinux`,
   or `guest/initramfs.cpio.gz` is a release; any other directory is a
   checkout, whose `guest/` directory holds sources only.
3. An `nvx/` directory in the release layout next to the current executable.
4. The bundle staged at build time by the `bundled` feature.

With the `bundled` feature, the build script stages `bin/`, `guest/`,
`SOURCE-MANIFEST.json`, and `SHA256SUMS` in its output directory, and
`Artifacts::bundled` returns them. The bundle comes from one of two sources:

- `ACI_EDGE_SANDBOXES_BUNDLE_DIR` names an extracted release, for offline builds or locally
  built artifacts.
- Otherwise, the build script downloads the release package pinned in
  [`artifacts.json`](artifacts.json) with `curl` and extracts it with `tar`.
  The Windows package is a ZIP archive, which GNU tar cannot read, so building
  for Windows needs bsdtar (Windows' own `tar`) or `unzip` on the build host.
  `ACI_EDGE_SANDBOXES_BUNDLE_HYPERVISOR=mshv` selects the Linux MSHV package instead of KVM.

Every staged file must match the source's `SHA256SUMS`, and a download must
match its pinned digest. The staged directory is in Cargo's build output, so it
serves development and tests on the build machine. To deploy, copy it next to
your executable as `nvx/`. Dependent build scripts receive its path as
`DEP_ACI_EDGE_SANDBOXES_ARTIFACTS_DIR`. `ACI_EDGE_SANDBOXES_BUNDLE=skip` turns staging off, which CI uses for
`--all-features` builds.

> The OpenVMM backend requires a compatible NVX guest image. `start` rejects an
> incompatible image with `backend_unavailable` rather than running without the
> requested policy guarantees.
>
> The release pinned in [`artifacts.json`](artifacts.json) predates these
> features, so a `bundled` build cannot start sandboxes until the pin names the
> first release that includes them. Until then, stage a current build with
> `ACI_EDGE_SANDBOXES_BUNDLE_DIR`, or point `NVX_ARTIFACTS_DIR` at one.

Defaults:

| Field | Default |
| --- | --- |
| `memory_mib` | 256 (a provision request can override it with `microvm.provision.memoryMib`) |
| `workload_uid`, `workload_gid` | 65534, Alpine's `nobody` |
| `hostname` | `nvx-sandbox`, set through the kernel's `hostname=` parameter |
| `guest_network` | `10.0.0.2/24` |
| `start_timeout`, `control_timeout` | 60 s |
| `stop_timeout`, `exec_response_grace` | 30 s |

Each timeout must be at most 30 days (`OpenVmmConfig::MAX_TIMEOUT`), and all
but `exec_response_grace` must be positive.

Choose a `state_root` that only the current user can access.
`OpenVmmConfig::default_state_root` returns `%LOCALAPPDATA%\nvx\sandboxes` on
Windows and `$XDG_STATE_HOME/nvx/sandboxes` (default
`~/.local/state/nvx/sandboxes`) elsewhere.

### How it runs OpenVMM

- **Provision** validates the request and records it in
  `<state_root>/<token>/sandbox.json`. No VM runs.
- **Start** launches OpenVMM with the managed lifecycle (`--microvm-lifecycle
  managed`), the workload identity, and an authenticated control console, but
  no sandbox blocks. The guest init therefore stays in its Alpine root file
  system and starts the managed agent directly.
  - A fresh 32-byte capability travels through OpenVMM's standard-input pipe,
    which is closed before OpenVMM starts.
  - Before launching OpenVMM, start records the control endpoint in
    `launch.json`. If the caller dies before OpenVMM's process identity is
    recorded, the next operation on the sandbox finds the VM through that
    endpoint and terminates it, so no VM is left untracked.
  - OpenVMM runs detached from the caller: in a new session on Linux, and
    without the caller's console on Windows. It inherits no handle of the
    caller except its standard input and log, so a caller whose own output is
    a pipe, such as an MXC phase process, still reaches end-of-file when it
    exits.
  - Start returns once the guest agent answers on the control console and
    advertises the control features this backend needs. A guest that lacks any
    of them is terminated, and start fails with `backend_unavailable`.
- **Exec** connects to the control console, authenticates, and streams the
  workload's output live. The agent runs the workload through `setpriv` as the
  workload identity, with no capabilities and `no_new_privs`, in a cgroup of
  its own. When the workload's first process exits, the agent kills whatever
  it left behind, including processes in other sessions, so no workload
  process outlives its exec.
- **Stop** asks the guest to shut down. If the guest does not finish within
  `stop_timeout`, OpenVMM is terminated. Everything the workloads wrote is
  discarded, because the root file system lives in guest memory.
- **Deprovision** deletes the state directory. It refuses to remove a directory
  that contains unexpected files.

The backend stores durable lifecycle state and reconciles it with the OpenVMM
process before every operation. Operations can therefore run safely from
different caller processes without forgetting a live VM. On Windows, set
`breakaway_from_job` if the caller runs inside a job object that would otherwise
terminate the VM when the caller exits.

### Policy honor matrix

| Field | provision | exec |
| --- | --- | --- |
| `filesystem.readonlyPaths`, `readwritePaths` | mapped into the guest; see [Host paths](#host-paths) | n/a |
| `filesystem.deniedPaths` | hidden inside mapped paths | n/a |
| `network.egress.default` | applied (`allow` or `deny`) | n/a |
| `network.egress.allow`, `deny` | applied; see [Network rules](#network-rules) | n/a |
| `network.ingress.default` | `deny` applied; `allow` rejected | n/a |
| `network.ingress.hostLoopback` | `deny` applied (also when omitted); `allow` rejected | n/a |
| `microvm.provision.memoryMib` | applied | n/a |
| `process.commandLine` | n/a | run as `/bin/sh -c <commandLine>`; at most 4096 bytes |
| `process.argv` (ACI Edge Sandboxes extension) | n/a | applied; absolute program, up to 64 arguments of 4096 bytes |
| `process.cwd` | n/a | an absolute guest path; a missing directory ends the workload with status 125 |
| `process.timeout` | n/a | applied, up to 3,600,000 ms |
| `process.env`, `inheritDefaultEnv: false` | n/a | rejected |
| Piped standard input | n/a | rejected; the workload reads end-of-file |

Workloads run as the configured non-root identity and may write at most 1 MiB
of combined output. Larger output ends with `Failed(OutputLimitExceeded)`.

### Host paths

Mapped host paths appear in the guest the way MXC's WSLc backend maps them: a
Windows path `C:\work\src` appears at `/mnt/c/work/src`, and an absolute Linux
path appears at the same path. Guest paths derive from the resolved host path,
so links, letter case, and Windows short names do not create second guest
paths for one object. `openvmm::resolve_guest_path` performs the same
translation, for example to turn a host working directory into a
`process.cwd`; `openvmm::guest_path` translates a path as written.

- OpenVMM offers one virtio-fs export. The backend exports the deepest
  directory that contains every mapped path to `/run/nvx/hostfs/root`, a guest
  directory that only the guest's root can enter. The guest agent then
  bind-mounts each mapped path at its guest path, read-only or read-write, so
  workloads see only the mapped paths. The export is writable when any path is
  read-write, and read-only mounts are then enforced by the guest kernel.
  Exporting a whole volume (`/` or `C:\`) is refused, so mapped paths must
  share a directory below it.
- Denied paths inside the export are hidden by OpenVMM itself: they are absent
  from listings and inaccessible through any name. OpenVMM requires hidden
  paths below the export to contain no whitespace, colons, or backslashes and
  no links, and accepts at most 128 of them.
- Mapped paths must exist, be directories or regular files, and share one
  volume. A path listed both read-only and read-write is mapped read-only. A
  denied path must not contain a mapped path, and a denied path that does not
  exist yet must not lie inside a mapped path, because nothing could hide it
  once a workload creates it. On Windows hosts, which open files
  case-insensitively, a read-only file inside a read-write directory is
  refused. Mapping over the guest's own system directories (`/usr`, `/etc`,
  and so on) is refused. Violations are reported as `policy_validation`.
- Provision records the identity of every mapped and denied object inside a
  read-write mapping, and start refuses to run if one changed, so a workload
  cannot rename a denied or read-only object and leave a decoy at its path for
  the next start. Objects outside read-write mappings are not pinned, so host
  edits that replace them do not block a restart.
- The bind mounts travel on the kernel command line, which leaves room for
  roughly a dozen typical paths.
- On Windows hosts, mapped files appear owned by root with mode `0777`, so the
  workload identity can read and, for read-write paths, write them. On Linux
  hosts virtio-fs shows host owners and modes unchanged; with
  `map_host_identity` (the default), sandboxes that map host paths run their
  workloads under the calling user's IDs, and the guest creates an account for
  them.

The guest kernel, not only the workload, separates the mapped paths from the
rest of the export, so place mapped paths under one directory when possible.

### Network rules

OpenVMM's portable network profile enforces an egress default plus IPv4 allow
and deny rules; deny rules take precedence. Each rule lists destination
networks (`to`, each a CIDR with optional `except` sub-networks) and destination
`ports` (protocol, `port`, optional `endPort`). An empty `to` matches every
destination, and an empty `ports` matches every protocol and port.

Rules are expanded into OpenVMM rules exactly: exceptions are subtracted from
their networks, port ranges become one rule per port, and protocol `any` with a
port becomes a TCP and a UDP rule. IPv6 networks, ICMP-only selectors, TCP or
UDP selectors without a port, and rules that would expand to more than 256
OpenVMM rules are rejected. Egress denied without allow rules, with ingress
denied, attaches no network device; otherwise the guest gets `10.0.0.2/24`
behind the profile's NAT gateway `10.0.0.1`, which also serves DNS.

### Idempotence and concurrency

- Every provision creates a new sandbox.
- `start` on a running sandbox fails with `already_started`, and `stop` on a
  stopped sandbox fails with `already_stopped`.
- Deprovisioning a running sandbox fails with `already_started`; stop it first.
  After deprovision, every call fails with `stale_id`.
- Lifecycle transitions of one sandbox are serialized by a lock file.
- Executions run one at a time: a concurrent exec waits up to
  `control_timeout` for the running one to finish.
- `stop` waits for a running exec until `stop_timeout`, then terminates the VM;
  that exec then fails.
- `Canceller::cancel` sends `CANCEL` to the guest within 20 ms. The agent kills
  every process in the workload's cgroup, and the execution ends with
  `Cancelled`. Cancelling an execution that already finished does nothing.
  Once cancellation is sent, a missing response fails with `backend_error`
  within `control_timeout`, or an earlier workload-response deadline, even
  when the workload has no timeout.
### Error mapping

| Condition | `ErrorCode` | Wire code |
| --- | --- | --- |
| Invalid request shape, relative or non-UTF-8 path, zero `memoryMib` | `MalformedRequest` | `malformed_request` |
| ID without the `aci-edge-sandboxes:` prefix or a 32-digit lowercase hexadecimal token | `MalformedId` | `malformed_id` |
| No state directory for the ID (never provisioned, or deprovisioned) | `StaleId` | `stale_id` |
| `exec` on a stopped sandbox, or a VM that died | `NotStarted` | `not_started` |
| `start` or `deprovision` on a running sandbox | `AlreadyStarted` | `already_started` |
| `stop` on a stopped sandbox | `AlreadyStopped` | `already_stopped` |
| Unsupported policy or exec feature, oversized command | `PolicyValidation` | `policy_validation` |
| Missing OpenVMM artifacts, inaccessible hypervisor, incompatible release or guest image, unsupported host | `BackendUnavailable` | `backend_unavailable` |
| OpenVMM launch failure, boot timeout, control-session failure, I/O errors | `BackendError` | `backend_error` |

Start failures name the OpenVMM log, `OpenVmmBackend::log_path`.

### Metadata

| Operation | Fields |
| --- | --- |
| provision | none |
| start | `bootMilliseconds`: time from launch to guest readiness |
| stop | `forced`: whether OpenVMM had to be terminated; `vmOutcome`, `vmStatusCode`, `teardownComplete`: summary of OpenVMM's outcome report |
| deprovision | none |

### Security notes

- The capability never appears in arguments, environment variables, logs, or
  endpoint names. It is stored in an owner-only file only while the VM starts
  or runs.
- The client only talks to the OpenVMM process it launched:
  - On Linux, the Unix socket lives in the owner-only state directory, and its
    peer must be that process (`SO_PEERCRED`).
  - On Windows, the named-pipe server must be that process
    (`GetNamedPipeServerProcessId`), and the pipe is opened with SQOS
    identification so the server cannot impersonate the client.
- The client validates every guest record and enforces the output limit itself.
- Termination never signals a reused process ID: Linux pins the process with a
  pidfd (Linux 5.3 or later is required to force a stop), and Windows holds a
  process handle.
- `WinHvPlatform.dll` is loaded dynamically, so the crate loads on hosts
  without the Windows Hypervisor Platform.

## Testing

```bash
cargo test --all-features   # unit, mock, and fake-OpenVMM tests; no hypervisor needed
python3 scripts/nvx.py test-aci-edge-sandboxes --backend kvm   # real VM (repository root)
```

- `testing::MockBackend` scripts executions for consumers' unit tests.
- The `aci-edge-sandboxes-fake-openvmm` binary (feature `testing`) emulates the OpenVMM
  control console. It lets the real OpenVMM backend run end to end on hosts
  without a hypervisor: detached launch, reconnection from a new process,
  crash recovery, interrupted-start recovery, forced stop, cancellation,
  refusal of guest images that lack required features, and failure injection.
- `cargo run --example lifecycle -- <command>` runs one command with
  discovered artifacts.
- `nvx.py test-aci-edge-sandboxes` runs the ignored `openvmm_e2e` tests against a real
  hypervisor with the repository's kernel and Alpine initramfs: the lifecycle,
  host path mapping, and network rules. CI runs them on Linux/KVM, Linux/MSHV,
  and Windows/WHP.

## MXC integration

An MXC `StatefulSandboxBackend` adapter maps onto this crate as follows:

| MXC | `aci_edge_sandboxes` |
| --- | --- |
| `ID_PREFIX` | `SandboxId::PREFIX` (`aci-edge-sandboxes`); pass sandbox IDs through unchanged |
| `provision`, `start`, `stop`, `deprovision` | the matching `AciEdgeSandbox` methods; `metadata` is a JSON object |
| `exec` with `ExecStdio::Piped` | `AciEdgeSandbox::exec`; hand out raw handles from `take_stdout` and `take_stderr` (`AsRawFd` or `AsRawHandle`) and keep the readers alive |
| `exec` with `ExecStdio::Relayed` | the same pipes, relayed by the dispatcher; stdin is not exposed |
| `ExecHandle::waiter` | `Execution::wait`: `Exited(c)` → `Exited(c)`, `Signaled(s)` → `Exited(128 + s)`, `Cancelled` → `Exited(137)`, `TimedOut` → `TimedOut` (`backend_error` under `Relayed`, which cannot represent it), and other outcomes → `backend_error` |
| `ExecHandle::terminator` | `Execution::canceller` |
| `validate_provision`, `validate_exec` hooks | `AciEdgeSandbox::validate_provision` and `AciEdgeSandbox::validate_exec`, which run the same checks as the operations without running anything; errors carry `ErrorCode::wire_code` |
| Backend construction | `Artifacts::discover` and `OpenVmmConfig::from_artifacts` |
| `policy.readonly_paths`, `readwrite_paths`, `denied_paths` | `FilesystemPolicy` with the same host paths |
| `policy.network_egress` rules | `EgressPolicy` rules, field for field |
| `working_directory` | `ExecRequest::with_cwd(guest_path(...))` |

A proof-of-concept MXC adapter, `nvx_backend`, implements this mapping. It
runs each phase in its own process against a real VM and consumes exec pipes
through MXC's `ExecSandboxProcess`. It is not yet wired into MXC's wire
contract or `ContainmentBackend` routing.

The crate is not published to a registry. A Cargo git dependency on
`microsoft/nvx` also fetches the `openvmm` submodule, which requires access to
`nanvix/openvmm`; vendor the crate if that access is unavailable.

## Limitations

The following require guest control-protocol work and are rejected today:
live standard input, `env`, and a cleared default environment. Guest state does
not survive a stop.
