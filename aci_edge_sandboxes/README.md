# aci_edge_sandboxes

`aci_edge_sandboxes` is the Rust interface to the ACI Edge Sandboxes lifecycle.
It exposes provision, start, exec, stop, and deprovision through a pluggable
backend. The default backend drives the `openvmm` binary directly; no daemon or
Python tooling is involved at runtime. The optional `agent` backend uses a
separately supplied native library, `aci_edge_agent`, and a different, image-backed guest.

By default, each sandbox is a microVM that runs the NVX guest's Alpine Linux
userland directly from its initramfs. That backend has no image layers, scratch
disks, or container namespaces; workloads run as a non-root user in the guest
itself, and guest state lives in memory until the sandbox stops. The opt-in
native backend instead runs commands against a caller-supplied, read-only GPT
image with a RAM overlay.

The Cargo package, library, and directory are named `aci_edge_sandboxes`.

## Quick start

```rust,no_run
use aci_edge_sandboxes::nvx::Hypervisor;
use aci_edge_sandboxes::openvmm::OpenVmmConfig;
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
`Cancelled`, and `Failed(reason)`. `Failed(WorkingDirectory)` means that the
workload never ran because it could not enter its working directory. An `Err`
from `Execution::wait` means the outcome could not be determined, for example
because the VM stopped.

Request types serialize with the contract's JSON field names (`readonlyPaths`,
`memoryMib`, `commandLine`, and so on) and reject unknown fields. Every
provision field is optional, so `{}` is a valid provision request. Like MXC's
schema, they accept a `process.timeout` from 0 through 4,294,967,295
milliseconds, about 49.7 days, and refuse a negative or larger one. The wire
envelope (`version`, `phase`, and `containment`) belongs to the caller.

## Architecture

- The NVX mechanics live in the [`nvx` crate](../nvx-rs/README.md): the
  OpenVMM control protocol, artifact discovery, OpenVMM's command
  line, the lowering of network and filesystem policies, process and platform
  primitives, the supervision of VM instances, and the fake OpenVMM. This crate
  re-exports it as `aci_edge_sandboxes::nvx`. It keeps the ACI Edge Sandboxes
  contract: the facade, `Backend`, the MXC model and its validation, the error
  codes, `SandboxId`, the execution and stream plumbing, and the asynchronous
  and mock APIs. Its `openvmm` backend only converts the contract's requests
  into `nvx` calls and maps `nvx` errors onto the contract's error codes; see
  [Error mapping](#error-mapping).
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
    executable through `nvx`.
  - `agent::AgentBackend` (feature `agent`) forwards the lifecycle to a separately
    supplied native library, which drives OpenVMM and an image-backed guest.
  - `testing::MockBackend` (feature `testing`) implements the state machine in
    memory for consumers' unit tests.
- `AsyncAciEdgeSandbox` (feature `async`) wraps `AciEdgeSandbox` for Tokio. Lifecycle calls run on
  the blocking pool, and output arrives as `AsyncRead` streams. A call keeps
  running when its future is dropped. A dropped `exec` requests the
  cancellation of its workload as soon as the workload starts, but only
  backends whose `capabilities().exec.cancel` is true, such as the OpenVMM and
  agent backends, honor the request; with other backends, the workload
  runs until it ends. The other calls complete, so the sandbox's state shows
  their effects.

To add a backend, implement `Backend`. Declare only the capabilities it can
enforce, and report state-machine violations with the codes listed in the
[error mapping](#error-mapping).

## Agent backend (opt-in)

Enable `agent` to use `AciEdgeSandbox::agent(AgentConfig)`. This backend is a thin client of a
separately supplied native library, `aci_edge_agent.dll` (`libaci_edge_agent.so` on Linux),
which owns the whole sandbox lifecycle: the state root and its records, the OpenVMM processes
and their boot consoles, the guest sessions, and the guest images. The default direct-OpenVMM
backend and its Alpine guest are unchanged. The crate does **not** build or publish the library,
and it downloads one only with the `agent-bundled` feature; see
[Bundled agent](#bundled-agent-agent-bundled).

`AgentConfig` names the library, the SHA-256 of the library that the caller's own policy
approved, and a `SetupConfig`, which the library applies once per process:

```jsonc
{
  "stateRoot": "C:\\nvx-edge-state",
  // A release bundle trusted through its manifest's SHA-256, or explicit files:
  // "runtime": { "files": { "openvmm": "…", "kernel": "…", "initrd": "…" } }
  "runtime": { "bundle": { "path": "C:\\nvx\\edge", "manifestSha256": "<64 hex digits>" } },
  "hypervisor": "whp",          // optional; KVM on Linux and WHP on Windows otherwise
  "cpuProfile": "auto",         // or "host"
  "timeouts": { "startMs": 60000, "controlMs": 60000, "stopMs": 30000, "execGraceMs": 30000 },
  "defaults": {                 // what a sandbox inherits unless its spec sets it
    "image": { "path": "C:\\images\\layers.gpt" },
    "resources": { "vcpus": 1, "memoryMib": 256 },
    "guestNetwork": "10.0.0.2/24"
  },
  // How long a library that pulls registry references may take to materialize one.
  "images": { "references": { "pullTimeoutMs": 1800000 } },
  "diagnostics": { "guestDebug": false }
}
```

Every field except `stateRoot` and `runtime` has a default. A bundle directory holds a format-2
`SOURCE-MANIFEST.json` of the `edge` profile, `bin/openvmm[.exe]`, `bin/direct-images[.exe]`,
the library itself (`bin/aci_edge_agent.dll` or `bin/libaci_edge_agent.so`), `guest/vmlinux`,
and `guest/initramfs-edge.cpio.gz`. The manifest pins every file's SHA-256 and the guest's
runtime ABI. The library checks every file but itself: the caller passes the library's SHA-256
(`agent.library_sha256` in the manifest) to `AgentConfig`. Explicit files can be pinned with
`RuntimeFiles::approved_sha256`.

A `SandboxSpec`, passed to `AciEdgeSandbox::provision_with` beside the provision request, sets
one sandbox's image (`ImageSource::Path` of a registered local GPT disk, `ImageSource::Digest` of
a registered image, or `ImageSource::Reference` of a registered registry image where the library
supports it), its resources, its guest network, and its forwarded ports. Every guest has one
virtual processor, so `resources.vcpus` may only be 1, and `resources.memoryMib` sets its memory.
`Capabilities::spec` lists the fields that the library honors; hostnames are not among them yet.
A request may still set `microvm.provision.memoryMib`, but not together
with `spec.resources.memoryMib`.

A library that honors `spec.image.reference` (`Capabilities::spec.image_reference`) registers a
reference with `register_reference`, or as the setup's default image when a backend opens: it
converts the reference into a local disk image with an external tool, once, and keeps the
result by the reference's text. A sandbox boots only a registered reference.
The tool comes from the runtime bundle (`bin/direct-images[.exe]`), so a library given explicit
runtime files does not materialize references. It does not check a tag again, so a moving tag
such as `latest` keeps naming the content that was first pulled: use a version tag or a digest
(`repository@sha256:…`) to control what a sandbox boots. `images.references.pullTimeoutMs`
limits each pull (30 minutes by default).

For private registries, set `images.references.authFile` to a host-side
Docker-compatible JSON credentials file, or use
`ReferenceSettings::with_auth_file`. The setup carries only its path:

```json
{
  "images": {
    "references": {
      "authFile": "C:\\edge-secrets\\registry-auth.json"
    }
  }
}
```

The file's `auths` entries are keyed by the registry host, including its port
when present. Entries support `auth` (base64 of `username:password`),
`username` with `password`, or `registrytoken` for an already issued registry
bearer token. Base64 is **not encryption**; protect the file with host
permissions or ACLs. Credential helpers (`credHelpers` and `credsStore`) and
identity/refresh tokens are not supported and fail explicitly. The host does
not search the user's Docker configuration or other credential files.

The host selects credentials only for the requested registry, and rereads
the file on each actual pull so it can be replaced to rotate credentials.
An unrelated registry is pulled anonymously; a malformed or unreadable
selected file fails the pull. Local images and registered references do not
read it. The existing state root remains the trust boundary of registered
images; credentials do not restrict access to an image already registered
there.

The native host passes credentials through the image tool's existing
password/token command-line options. They are visible to users who can
inspect that process, even though the client receives only the file path.
Do not map the credential file into a guest; the host rejects mappings that
would expose it.

- **Loading.** `AgentBackend::new` checks the library's SHA-256 and sandbox ABI version and
  loads it once per process; it stays loaded until the process exits. It loads the bytes that it
  checked. On Linux it loads them from a sealed in-memory copy, and fails if the kernel refuses
  to create or execute one. On Windows, its open handle keeps writers out of the file and keeps
  the file and its directories from being renamed or deleted until the library is loaded. Other
  hosts cannot load the library. A missing, wrong-version, or wrong-digest library fails rather
  than falling back.
- **Opening.** The first backend that a process opens for a setup verifies the runtime files, or
  checks the seals that an earlier process recorded under the state root, and registers the
  default image. Later backends for the same setup reuse that host without touching a file, so
  a caller may create a backend for every operation; the operations check the files that they
  use. One that finds a runtime file changed makes the next backend verify the files again, and
  one that finds the default image changed or unregistered makes it register the image again.
  Hosts, guest sessions, and boot-console capture stay in the library for the life of the
  process; dropping a backend leaves its sandboxes running.
- **Images.** The library registers images under the state root by their content ID,
  `sha256:<hex>` (`ImageId`), the SHA-256 of the registered file as the caller gave it.
  Registration is the one operation that reads an image: it hashes the image once, unless
  `ImageDigest::Trusted` records a digest that the caller's own policy verified, and
  `ImageDigest::Expect` additionally requires a digest. It then stages the image as a read-only
  copy of the library's own, in raw form (a fixed VHD loses its footer but keeps its ID), which
  `RegisteredImage::path` names and sandboxes attach, so later changes to the registered file do
  not reach them. A later registration, in any process, reuses one that the file still matches
  without reading the file. Provision takes only registered images: by ID, by the path that was
  registered, or by the reference. A path whose file changed since its registration fails until
  it is registered again, and provision never pulls, copies, or hashes an image; it records the
  image ID and the runtime digests. Start compares the seal of each runtime file and of the
  staged copy (volume, file ID, length, and last-write time, plus the change time on Linux) with
  the one taken when it was hashed or staged, instead of hashing again, hands OpenVMM the copy
  that it checked, and fails with `backend_unavailable` if anything changed, so a start costs
  about the guest's boot time. A seal detects replacement or modification, not a writer that
  deliberately restores timestamps. On Windows, start also keeps writers out of the files until
  OpenVMM has opened them. Register an image again with `register_image` to stage it anew:
  unchanged content keeps its ID, so sandboxes that use it start again. `images` lists the
  registrations and whether each staged copy still matches, `verify_image` hashes one again, as a
  diagnostic, and `unregister_image` refuses while a provisioned sandbox uses the image, and
  otherwise removes its staged copy.
- **The guest.** The image is attached read-only; the edge guest validates its GPT and p2+ ext4
  layers and keeps its writable layer in RAM, so it needs no scratch disk. Start sends OpenVMM a
  fresh 32-byte capability through standard input, checks the process that serves the control
  endpoint, and waits until the guest agent is ready.
- **Guest sessions.** OpenVMM accepts one host session per VM at a time. The library keeps one
  per running sandbox for the process that uses it, and that process's operations share it, so
  they never wait for each other's sessions, and its executions run side by side over it.
  Another process waits until the holder releases the session, on stop, deprovision, or exit.
  `guestSession.hold: false` releases a session once it has been idle for `lingerMs`, for
  callers that drive one sandbox from several long-lived processes.
- **Interrupted starts.** Start claims the sandbox's OpenVMM log before launching, and OpenVMM
  inherits the claim. If the caller dies before recording OpenVMM's identity, the next operation
  waits up to the start timeout for that OpenVMM to open its endpoint and then terminates it;
  once nothing holds the log, the sandbox is usable again.
- **Boot console.** The library copies the guest boot console to the console log that
  `AgentBackend::diagnostics` names, from the process that started or last used the sandbox.
  OpenVMM serves one console client at a time and holds guest output while none is connected, so
  a guest that writes enough console output stalls until a listener reconnects: keep that
  process running, or use the sandbox from its successor, whose listener takes the capture over.
- **Metadata.** Start reports `bootMilliseconds`, `guestBuildId`, and `phases` (`spawnMs`,
  `attachMs`, and `readyMs`). Stop reports `forced`, `phases` (`shutdownMs` and `exitMs`), and,
  after a failed graceful shutdown, `gracefulError`. A capture failure never fails stop or
  deprovision: both report it as `consoleError`. `AgentBackend::guest_logs` reads a bounded
  snapshot of the guest's log while it runs.

Executions run shell commands or argv and stream while they run. Output arrives as the command
writes it, with no size limit, `ExecIo::stdin` feeds the command's standard input, and
`ExecControl` cancels it. Each execution's flow control bounds the output that the library
buffers for it, so a caller that stops reading pauses that command, not the others. At most 16
executions run at once per sandbox; one more fails with `backend_error`. A timeout is rounded up
to whole milliseconds, can be up to an hour, and ends the command as `TimedOut`. When it elapses
before the outcome arrives, `wait` closes the output streams, as cancellation does, so that a
stream that the caller holds without reading cannot delay the outcome; an execution that loses
output that way ends as `TimedOut`.
Snapshot/restore and guest-side image pulls are not part of this profile.

[`examples/agent_lifecycle.rs`](examples/agent_lifecycle.rs) runs one lifecycle. It needs
`--host-library`, `--host-sha256`, `--state-root`, either `--image` or `--image-ref`, and either `--openvmm`, `--kernel`,
and `--initrd` or `--bundle` and `--bundle-sha256`, and the command after `--`, unless it is built
with `agent-bundled`, which gives each of them a default; see
[Bundled agent](#bundled-agent-agent-bundled). `--image-sha256`
requires a local image's digest when it is registered. `--image-ref` requires a bundle;
`--registry-auth-file PATH` selects its credentials file and `--pull-timeout-ms` sets its pull
deadline. `--hypervisor`, `--cpu-profile auto|host`,
`--memory-mib`, and `--guest-debug` adjust the setup. The repeatable `--readonly`,
`--readwrite`, and `--denied` options map host paths, and `--egress allow|deny` with the
repeatable `--egress-allow` and `--egress-deny` options, each taking `CIDR` or
`CIDR:tcp|udp:PORT`, attach a network. `--network-proxy URL` routes the guest's traffic through
a host proxy, and `--host-loopback allow|deny` with the repeatable
`--host-loopback-forward tcp|udp:HOST:GUEST` publishes guest ports on host loopback. The
repeatable `--env KEY=VALUE`, layered over the guest's environment, and `--cwd PATH` apply to the
command. The ignored `tests/agent_guest.rs` exercises an actual guest when the corresponding
`EDGE_AGENT_TEST_*` paths and approved library digest are set: under WHP on Windows, or under the
hypervisor that `EDGE_AGENT_TEST_HYPERVISOR` names, such as `mshv` on Linux.
`EDGE_AGENT_TEST_CPU_PROFILE=host` boots its guests on a host CPU profile.

For example, from `aci_edge_sandboxes` on a Windows WHP host, set the following paths to
compatible, separately built artifacts and a caller-prepared GPT disk with p2+ ext4 layers (not
a container image reference). Use the **edge** guest initramfs, not the default Alpine or
standard container guest initramfs:

```powershell
$openvmm = 'C:\path\to\openvmm.exe'
$kernel = 'C:\path\to\vmlinux'
$initrd = 'C:\path\to\nvx-edge-initramfs.cpio.gz'
$image = 'C:\path\to\layers.gpt'
$hostLibrary = 'C:\path\to\aci_edge_agent.dll'
$approvedHostSha256 = '<64 hexadecimal digits from an independent trust policy>'
$stateRoot = 'C:\nvx-edge-state'

cargo run --release --locked --features agent --example agent_lifecycle -- `
    --openvmm $openvmm --kernel $kernel --initrd $initrd --image $image `
    --host-library $hostLibrary --host-sha256 $approvedHostSha256 `
    --state-root $stateRoot -- 'printf READY'
```

To pull a private image through the library instead, use the verified bundle
and replace `--image` with `--image-ref`:

```powershell
cargo run --release --locked --no-default-features --features agent --example agent_lifecycle -- `
    --bundle 'C:\path\to\edge-bundle' --bundle-sha256 '<approved manifest SHA-256>' `
    --image-ref 'registry.example:5443/team/image:1' `
    --registry-auth-file 'C:\edge-secrets\registry-auth.json' --pull-timeout-ms 120000 `
    --host-library $hostLibrary --host-sha256 $approvedHostSha256 `
    --state-root $stateRoot -- 'printf READY'
```

To map `C:\work\src` read-only and `C:\work\out` read-write, and let the guest reach only
`192.0.2.10` on TCP port 443, add
`--readonly C:\work\src --readwrite C:\work\out --egress deny --egress-allow 192.0.2.10:tcp:443`
before `--`; `nvx::resolve_guest_path` gives the guest paths, here `/mnt/c/work/src` and
`/mnt/c/work/out`. To let the guest out only through a proxy that listens on host port 3128, add
`--egress deny --network-proxy http://127.0.0.1:3128` instead; to reach a guest service on port
8080 at host `127.0.0.1:18080`, add
`--egress allow --host-loopback allow --host-loopback-forward tcp:18080:8080`.

Build the native library and the static edge initramfs separately from their matching sources;
this example neither fetches nor builds them unless it is built with `agent-bundled`. Use the
pinned OpenVMM, which includes the
scratchless RAM-overlay topology, and the NVX kernel that its time ABI requires, such as a
release's `guest/vmlinux`. OpenVMM selects a [CPU profile](../doc/usage.md#cpu-profiles) at
every cold boot, so start fails with `backend_error` on a host that none of its built-in
profiles serves, or whose hypervisor does not support its profile; the OpenVMM log that the
error names gives the reason. On such a development host, `cpuProfile: "host"` (the example's
`--cpu-profile host`) boots the guests on a host profile, which OpenVMM derives from the host's
hypervisor at every cold boot and does not pin. On Linux, supply a matching
`libaci_edge_agent.so` and OpenVMM build and select `--hypervisor mshv`.

### Bundled agent (`agent-bundled`)

The `agent-bundled` feature enables `agent` and makes the build script download, verify, and
stage a complete edge agent bundle: the agent library, OpenVMM, the edge guest kernel and
initramfs, the image tool, their manifests, and `SHA256SUMS`. A program built with it needs no
paths or digests:

```rust
use aci_edge_sandboxes::AciEdgeSandbox;
use aci_edge_sandboxes::agent::AgentConfig;

let client = AciEdgeSandbox::agent(AgentConfig::bundled("C:\\nvx-edge-state")?)?;
```

`BundledAgent::discover` returns the bundle's directory, runtime, library, and digests, for a
setup of one's own: `bundle.config(bundle.setup(state_root).with_cpu_profile(...))`.

By default the build script downloads the `aci-edge-sandboxes.zip` asset of the release of the
commit being built, `v<VERSION>-dev.<first 12 hex digits>`, where `<VERSION>` is what that
commit's `VERSION` file holds. Releases do not publish the bundle for every commit, so when that
release has none, the build uses the release of the nearest first-parent ancestor that does,
named with the ancestor's own `VERSION`, searching at most 32 commits, and warns which one it
used. It fails if none has one or if the commit cannot be told (no git checkout or no `git` on
`PATH`).
Releases publish no bundle for Linux, so a Linux build needs one of the variables below. Other
targets, where the agent library cannot run, stage nothing, as `bundled` does. The build script
runs again when `HEAD` moves to another commit or a variable below changes; run
`cargo clean -p aci_edge_sandboxes` to pick up a bundle published later for the same commit. It
also runs again when the copy beside the executables (see below) is replaced, changed, or
removed, for example by a build of another feature configuration that uses a different bundle,
and restores the bundle that the configuration being built pins.

| Build variable | Effect |
| --- | --- |
| `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL` | Downloads this bundle ZIP instead, from any URL that `curl` reads, such as a mirror; a `file://` URL names a ZIP on the build host |
| `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR` | Stages this extracted bundle instead, for offline builds or locally built bundles |
| `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_SHA256` | Requires the ZIP to have this SHA-256 |
| `ACI_EDGE_SANDBOXES_AGENT_BUNDLE=skip` | Stages nothing, as when the feature is disabled; `BundledAgent::discover` then fails with `backend_unavailable`. CI sets it for `--all-features` builds |

The build fails unless every file that `SHA256SUMS` lists matches it, the bundle holds no links,
`SOURCE-MANIFEST.json` is a format-2 manifest of the `edge` profile whose pins match
`bin/openvmm[.exe]`, `guest/vmlinux`, `guest/initramfs-edge.cpio.gz`,
`bin/direct-images[.exe]`, and the library, `bundle-manifest.json` describes a complete bundle
whose consumer section names that manifest and an agent library for the target platform with
the digests found, and the library speaks this crate's sandbox ABI. It warns when the bundle
records an NVX commit other than the one it should come from. These checks prove that the bundle
is intact, not that it is authorized: the trust comes from the download source, which
`ACI_EDGE_SANDBOXES_AGENT_BUNDLE_SHA256` can pin.
Extracting the ZIP needs bsdtar (Windows' own `tar`) or `unzip` on the build host.

The build script stages the bundle in its output directory, which the build scripts of
dependents read as `DEP_ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR`, and copies it to an
`aci-edge-sandboxes` directory in Cargo's profile directory, such as `target/release`, beside
the executables that Cargo builds. The crate compiles in only the SHA-256 digests of the
bundle's `SOURCE-MANIFEST.json` and library. `BundledAgent::discover` uses, in this order:

1. The directory that `ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR` names, when it is set and not
   empty. The directory must then hold a copy of the bundle; discovery does not fall back.
2. An `aci-edge-sandboxes/` directory next to the current executable, if it holds the bundle.
3. One next to the executable's directory, which serves Cargo's `examples/` and `deps/`.

An executable therefore keeps working when it is copied elsewhere together with the
`aci-edge-sandboxes` directory, as long as their relative positions stay the same, and the
host refuses a bundle whose files differ from the digests that were compiled in. When Cargo's
`build-dir` is not the target directory, the copy goes there and serves only test executables;
for the others, copy the bundle from `DEP_ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR` next to the
executable or set `ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR`.

Built with `agent-bundled`, [`examples/agent_lifecycle.rs`](examples/agent_lifecycle.rs) runs
without any option:

```powershell
cargo run --release --locked --features agent-bundled --example agent_lifecycle
```

It uses the staged bundle and library, keeps its state in `aci-edge-sandboxes-state/` beside
the executable, boots `mcr.microsoft.com/azurelinux/base/core:3.0`, and runs
`printf READY; echo; head -n 2 /etc/os-release`. Every option still overrides its default, and
`--bundle`, `--openvmm`, or `--host-library` options replace the staged runtime or library.

### Native host paths and network

The native backend accepts the same `filesystem` and `network` policies as the direct
backend: it maps host paths to the same guest paths and expands network rules the same way;
see [Host paths](#host-paths) and [Network rules](#network-rules). Unlike the direct backend,
it exports one directory for all mapped paths, under the rules below. It also
refuses mappings that would show workloads its state root, described below. Its workloads run
as the guest's root. The library plans the policies at provision: it resolves the mapped and
denied paths, chooses the export, pins the objects inside read-write mappings, and expands the
network rules, and the sandbox's record keeps the resulting plan. Every start checks the plan
again against every planning rule that needs no host access, and checks the pinned objects: if
one was replaced, start fails with `backend_error`, and the sandbox must be deprovisioned and
provisioned again to accept the change.

- The state root holds the record, and so the plan, of every sandbox, which decides what the
  next start exports, which egress it allows, and which host loopback ports the guest reaches
  or publishes. Start's checks catch a damaged plan, but not one changed into another plan that
  planning could have produced, so only the caller may write the state root. Provision
  therefore refuses, with `policy_validation`, a mapped path inside the state root, and one that
  contains it unless a denied path inside the mapping hides it.
- OpenVMM exports the deepest directory that contains every mapped path through one
  virtio-fs device, read-only unless a path is read-write, and hides the denied paths. The
  guest mounts the export where workloads cannot reach it and bind-mounts each mapped path
  into the RAM overlay at its guest path, read-only where requested. The bind mounts travel on
  the guest's 1024-byte kernel command line, which leaves room for roughly a dozen typical
  paths; provision rejects a policy whose mounts do not fit with `policy_validation`.
- Denied paths stay hidden through their aliases: `..`, host symbolic links and junctions,
  links that workloads create, and other names of a denied object, such as a host hard link to
  a denied file. OpenVMM never follows a link on the host, so the guest resolves every link
  itself, and each lookup of a denied path fails with `EACCES`; an absolute Windows symbolic
  link or a junction cannot be followed at all (`EPERM`). OpenVMM recognizes only the denied
  objects themselves: a host hard link that already joins a file inside a denied directory to a
  name outside it stays readable through that name. A directory that holds a host hard link to
  a denied file may list the link's name, and a listing that also looks its entries up, as the
  guest's first read of a directory does, fails with `EACCES`.
- Workloads run as the guest's root with only the default container capabilities (`CHOWN`,
  `DAC_OVERRIDE`, `FOWNER`, `FSETID`, `KILL`, `SETGID`, `SETUID`, `NET_BIND_SERVICE`, and
  `AUDIT_WRITE`), with `no_new_privs`, and without user namespaces, so they cannot remount or
  unmount the mapped paths or mount the export again. Host-side changes through read-write
  mappings, including modes and ownership, happen with the credentials of the account that
  runs OpenVMM, so run OpenVMM unprivileged. A host hard link that already joins a file in a
  read-write mapping to one in a read-only mapping stays writable through the read-write
  path.
- A policy that denies egress without allow rules or a proxy attaches no network device.
  Otherwise the guest gets its spec's `guestNetwork`, or the setup's default (`10.0.0.2/24`
  unless set), behind OpenVMM's NAT gateway, the network's first address. OpenVMM also gives
  the guest the IPv6 network that embeds this one in `fd00::/96`, such as `fd00::a00:2/120`,
  behind the gateway's counterpart, such as `fd00::a00:1`. The guest names the gateway as its
  DNS server in `/etc/resolv.conf` when the policy allows TCP or UDP port 53 to it, and
  otherwise the gateway's IPv6 address when the policy allows DNS to that one. Choose a guest
  network that contains no address the guest must reach. Unlike with the
  [OpenVMM backend](#network-rules), ingress must be `deny`; host-loopback access may be allowed
  independently, as described next.

### Native proxy and host loopback

The proxy keeps general host-loopback access denied. Generic listener publication and explicit
forwards need host-loopback access allowed.

- `runtimeConfig.networkProxy` (`ProvisionRequest::with_network_proxy`) names an HTTP or HTTPS
  proxy that listens on host IPv4 loopback, such as `http://127.0.0.1:3128` or
  `http://localhost:3128`: an explicit port, and at most a trailing `/` after it. As in MXC's
  schema, the proxy is the guest's only way out, so the network policy must deny egress without
  allow or deny rules; provision rejects other policies, and IPv6 or remote proxies, with
  `policy_validation`. The guest's only reachable destination is TCP to its gateway at the
  proxy's port, which OpenVMM connects to the proxy; it cannot reach a DNS server, so the proxy
  resolves names. Every workload gets `HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`, and
  `https_proxy` naming the proxy at the gateway, such as `http://10.0.0.1:3128`, and `NO_PROXY`
  and `no_proxy` set to `localhost,127.0.0.1`; an HTTPS proxy's certificate must therefore be
  valid for the gateway's address.
- `network.ingress.hostLoopback: allow` lets the guest reach host-loopback services through its
  gateway, and publishes each TCP or UDP listener that the guest binds to a non-loopback address
  on the same port of host IPv4 or IPv6 loopback, without a port list. A listener bound only to
  guest `127.0.0.1` or `::1` stays private to the sandbox, OpenVMM never publishes a listener on
  a LAN or public interface or displaces a host listener that holds the port, and replies to the
  host pass whatever the egress policy. It needs a guest network of `/29` or larger: OpenVMM
  presents host peers to the guest at an address of its network, which a `/30` cannot spare.
- `hostLoopbackForwards` of the sandbox spec (`SandboxSpec::with_host_loopback_forward`, passed
  to `AciEdgeSandbox::provision_with`) publishes up to 64 further guest ports on host loopback:
  OpenVMM listens on `127.0.0.1:hostPort` and relays each TCP connection or UDP datagram to the
  guest's `guestPort`. Ports are nonzero, and a host port and a guest port each appear at most
  once per protocol, because OpenVMM keeps one forward listener per guest port.
  Forwarded ports need `network.ingress.hostLoopback: allow`. The guest answers forwarded
  traffic at addresses that OpenVMM assigns inside the guest network, so the egress default
  must be `allow`, which leaves the guest's other egress open, and no deny rule may cover the
  guest network.

Only the guest sets the proxy variables: an exec whose `process.env` names one of them, in any
case, fails with `policy_validation`, with or without a proxy.

### Native working directories and environments

`process.cwd` must be an absolute guest path without `..` of at most 4095 bytes, as with the
direct backend; `nvx::resolve_guest_path` gives the guest path of a mapped host path. A
directory that the workload cannot enter ends the command with exit code 126 and an
`NVX-EDGE-STAGE-ERROR` line on standard error. The guest's default environment holds only
`PATH` (`/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`) and the proxy
variables. `process.env` can only be layered over it, with `inheritDefaultEnv: true`, as at
most 256 entries of at most 32 KiB in total; replacing it fails with `policy_validation`. A
later entry for a name replaces an earlier one, and an entry replaces the default of the same
name.

The caller must independently approve and protect the native asset. Checking a caller-supplied
digest does not make a writable path or a self-declared digest trustworthy; use this profile
only with an immutable, externally authorized library installation.

## OpenVMM backend

### Configuration

`OpenVmmConfig` names the artifacts, the hypervisor, and a state root:

| Constructor | Artifacts |
| --- | --- |
| `OpenVmmConfig::discover` | located by `nvx::Artifacts::discover`; the hypervisor comes from `NVX_HYPERVISOR` or the platform default, and the state root is the default one |
| `OpenVmmConfig::from_artifacts` | an `nvx::Artifacts` value |
| `OpenVmmConfig::from_release_dir` | `bin/openvmm[.exe]`, `guest/vmlinux`, and `guest/initramfs.cpio.gz` of an extracted release |
| `OpenVmmConfig::from_repo_layout` | `openvmm/target/release/openvmm[.exe]`, `build/vmlinux`, and `build/initramfs.cpio.gz` of a checkout |
| `OpenVmmConfig::new` | Explicit paths |

The release and repository constructors require the `SOURCE-MANIFEST.json`
control contract `nvx-microvm-v2-control-v2`. The initramfs is the Alpine
image: its userland is the workloads' environment, and its managed agent
serves the control console.

### Artifacts

`nvx::Artifacts` holds the OpenVMM executable, the guest kernel, and the Alpine
initramfs.

`nvx::Artifacts::discover` takes the first source that is present:

1. `NVX_OPENVMM`, `NVX_KERNEL`, and `NVX_INITRD`, which must be set together.
2. `NVX_ARTIFACTS_DIR`, naming an extracted release or a repository checkout.
   A directory that contains a release's `bin/openvmm[.exe]`, `guest/vmlinux`,
   or `guest/initramfs.cpio.gz` is a release; any other directory is a
   checkout, whose `guest/` directory holds sources only.
3. An `nvx/` directory in the release layout next to the current executable.
4. The bundle staged at build time by the `bundled` feature.

The `bundled` feature enables the `bundled` feature of `nvx`, whose build
script stages `bin/`, `guest/`, `SOURCE-MANIFEST.json`, and `SHA256SUMS` in its
output directory, and `nvx::Artifacts::bundled` returns them. The bundle comes
from one of two sources:

- `NVX_BUNDLE_DIR` names an extracted release, for offline builds or locally
  built artifacts.
- Otherwise, the build script downloads the release package pinned in
  [`nvx-rs/artifacts.json`](../nvx-rs/artifacts.json) with `curl` and extracts
  it with `tar`. The Windows package is a ZIP archive, which GNU tar cannot
  read, so building for Windows needs bsdtar (Windows' own `tar`) or `unzip` on
  the build host. `NVX_BUNDLE_HYPERVISOR=mshv` selects the Linux MSHV package
  instead of KVM.

Every staged file must match the source's `SHA256SUMS`, and a download must
match its pinned digest. The staged directory is in Cargo's build output, so it
serves development and tests on the build machine. To deploy, copy it next to
your executable as `nvx/`. The build scripts of crates that depend on this one
receive its path as `DEP_ACI_EDGE_SANDBOXES_ARTIFACTS_DIR`, which this crate's
build script passes on from `nvx`; crates that depend on `nvx` directly
receive it as `DEP_NVX_ARTIFACTS_DIR`. `NVX_BUNDLE=skip` turns staging off,
which CI uses for `--all-features` builds.

> The OpenVMM backend requires a compatible NVX guest image. `start` rejects an
> incompatible image with `backend_unavailable` rather than running without the
> requested policy guarantees.
>
> The build script stages only a release whose `SOURCE-MANIFEST.json` declares
> the control contract of `nvx-rs`. After a contract change, `bundled` builds
> that download the pinned release fail until
> [`nvx-rs/artifacts.json`](../nvx-rs/artifacts.json) names a release with the
> new contract, so repin it as soon as one is published. Until then, stage a
> current build with `NVX_BUNDLE_DIR`.

Defaults:

| Field | Default |
| --- | --- |
| `memory_mib` | 256 (a provision request can override it with `microvm.provision.memoryMib`) |
| `workload_uid`, `workload_gid` | 65534, Alpine's `nobody` |
| `hostname` | `nvx-sandbox`, set through the kernel's `hostname=` parameter |
| `guest_network` | `10.0.0.2/24` |
| `start_timeout`, `control_timeout` | 60 s |
| `stop_timeout`, `exec_response_grace` | 30 s |
| `runtime_identity` | none: the install metadata of the artifacts' release or checkout names it; see [Runtime identity](#runtime-identity) |

Each timeout must be at most 30 days (`OpenVmmConfig::MAX_TIMEOUT`), and all
but `exec_response_grace` must be positive. `guest_network` must be an address
that OpenVMM accepts: a /1 to /30 prefix, and neither the network's own address,
its broadcast address, nor its first address, which is the gateway.

Choose a `state_root` that only the current user can access.
`OpenVmmConfig::default_state_root` returns `%LOCALAPPDATA%\nvx\sandboxes` on
Windows and `$XDG_STATE_HOME/nvx/sandboxes` (default
`~/.local/state/nvx/sandboxes`) elsewhere.

### Runtime identity

Each sandbox's state directory is the `nvx::host::store` of its VM, which
records the runtime identity of the VM's start: the identities of its
OpenVMM, kernel, and initramfs, and the boot contract, control contract, and
control session versions of the build that started it, which `nvx::contract`
defines. Every operation finds the sandbox's VM under the
backend's runtime identity, so a backend never drives a VM that another NVX
build, or other artifacts, started.

- Artifacts located in a release or a repository checkout, by
  `OpenVmmConfig::discover` from `NVX_ARTIFACTS_DIR`, an `nvx/` directory, or
  the bundle, by `from_release_dir` or `from_repo_layout`, or by
  `from_artifacts` with artifacts that `nvx::Artifacts` located, take their
  identity from the directory's install metadata: the digests that a
  release's `SHA256SUMS` lists, or the build provenance that `nvx.py` records
  beside a checkout's guest artifacts. Nothing reads or hashes an artifact.
  Pointing `openvmm`, `kernel`, or `initrd` of such a configuration at other
  files leaves it without an identity.
- Artifacts named by path, by `OpenVmmConfig::new` or by `discover` from
  `NVX_OPENVMM`, `NVX_KERNEL`, and `NVX_INITRD`, have no install metadata. Set
  `OpenVmmConfig::runtime_identity` to the identity that the deployment
  records for them; it also replaces the identity of located artifacts.

Without a runtime identity, `probe`, `provision`, and `start` fail with
`backend_unavailable`, as does any operation on a sandbox whose store records a
VM. A sandbox whose VM runs under another runtime identity, such as one that the
build before an upgrade started, is never driven, since its guest might speak
other contracts. `stop` terminates its OpenVMM through its verified process
identity, without a control session, and reports `forced`, but no summary of
OpenVMM's outcome report, which follows that runtime's contracts. Every other
operation fails with `backend_error`, naming the parts that differ and
OpenVMM's process ID, and leaves the VM running.

### How it runs OpenVMM

- **Provision** validates the request and records it in
  `<state_root>/<token>/sandbox.json`. No VM runs.
- **Start** launches OpenVMM with the managed lifecycle (`--microvm-lifecycle
  managed`), the workload identity, and an authenticated control console, but
  no sandbox blocks. The guest init therefore stays in its Alpine root file
  system and starts the managed agent directly.
  - A fresh 32-byte capability travels through OpenVMM's standard-input pipe,
    which is closed before OpenVMM starts.
  - Before launching OpenVMM, start records the control endpoint and
    OpenVMM's log in the store's launch journal, `launch.json`, and then
    OpenVMM's process identity. If the caller dies before the VM is ready, the
    next operation on the sandbox terminates the VM, through its process
    identity, or through the endpoint's server if that process holds the log,
    so no VM is left untracked.
  - OpenVMM runs detached from the caller: in a new session on Linux, and
    without the caller's console on Windows. It inherits no handle of the
    caller except its standard input and log, so a caller whose own output is
    a pipe, such as an MXC phase process, still reaches end-of-file when it
    exits.
  - Start returns once the guest agent answers on the control console,
    advertises the control features this backend needs, and has mounted every
    mapped host path. A guest that lacks any of the features is terminated,
    and start fails with `backend_unavailable`.
- **Exec** connects to the control console, authenticates, and streams the
  workload's output live. The agent runs the workload through `setpriv` as the
  workload identity, with no capabilities and `no_new_privs`, in a cgroup of
  its own. Each execution gets its own [working directory](#working-directories)
  and [environment](#environment). When the workload's first process exits, the
  agent kills whatever it left behind, including processes in other sessions,
  so no workload process outlives its exec.
- **Stop** asks the guest to shut down. If the guest does not finish within
  `stop_timeout`, OpenVMM is terminated. The OpenVMM of a VM of another
  [runtime identity](#runtime-identity) is terminated without a shutdown
  request, and the stop waits up to `stop_timeout` for it to exit. Everything
  the workloads wrote is discarded, because the root file system lives in guest
  memory.
- **Deprovision** deletes the state directory. It refuses to remove a directory
  that contains unexpected files.

Before every operation, the backend reattaches to the VM that the sandbox's
store records, as NVX's supervisor does after a restart: it checks that
OpenVMM's process still runs, verifying the process rather than trusting its
ID, under the backend's [runtime identity](#runtime-identity). A sandbox whose
VM died is provisioned again. Operations can therefore run safely from
different caller processes, one after another or after a caller restarts,
without forgetting a live VM. On Windows, set
`breakaway_from_job` if the caller runs inside a job object that would otherwise
terminate the VM when the caller exits.

### Policy honor matrix

| Field | provision | exec |
| --- | --- | --- |
| `filesystem.readonlyPaths`, `readwritePaths` | mapped into the guest; see [Host paths](#host-paths) | n/a |
| `filesystem.deniedPaths` | hidden inside mapped paths | n/a |
| `network.egress.default` | applied (`allow` or `deny`) | n/a |
| `network.egress.allow`, `deny` | applied; see [Network rules](#network-rules) | n/a |
| `network.ingress.default` | applied (`allow` or `deny`); see [Network rules](#network-rules) | n/a |
| `network.ingress.hostLoopback` | applied (`allow` or `deny`, `deny` when omitted); see [Network rules](#network-rules) | n/a |
| `spec.hostLoopbackForwards` (ACI Edge Sandboxes extension) | applied; see [Network rules](#network-rules) | n/a |
| `microvm.provision.memoryMib` | applied | n/a |
| `process.commandLine` | n/a | run as `/bin/sh -c <commandLine>`; at most 4096 bytes |
| `process.argv` (ACI Edge Sandboxes extension) | n/a | applied; absolute program, up to 64 arguments of 4096 bytes |
| `process.cwd` | n/a | applied; an absolute guest path of at most 4095 bytes, `/` when omitted; see [Working directories](#working-directories) |
| `process.timeout` | n/a | applied, up to 4,294,967,295 ms (about 49.7 days), the most that MXC allows; `0` disables it |
| `process.env`, `inheritDefaultEnv` | n/a | applied per execution; see [Environment](#environment) |
| Piped standard input | n/a | rejected; the workload reads end-of-file |

Workloads run as the configured non-root identity and may write at most 1 MiB
of combined output. Larger output ends with `Failed(OutputLimitExceeded)`.

### Working directories

Each execution starts in the working directory that it requests, and nothing
carries over from an earlier execution.

- Without `process.cwd`, the workload starts in the guest's root directory,
  `/`.
- `process.cwd` must be an absolute guest path of at most 4095 bytes. A
  relative path is rejected with `policy_validation` before anything runs;
  `nvx::resolve_guest_path` turns a mapped host path into a guest path.
- The guest agent enters the directory with the workload's own identity,
  before the workload starts, and, unless `process.env` replaces the
  environment, points `PWD` at it. For a path with empty, `.`, or `..`
  components, `PWD` names the directory's physical path instead.
- A directory that does not exist, is not a directory, or that the workload
  cannot search fails the launch, and the backend never falls back to another
  directory. Nothing runs, the workload's standard error receives a diagnostic
  such as `nvx-managed-agent: cannot enter working directory /work: No such
  file or directory`, and the execution ends with `Failed(WorkingDirectory)`.
- Start refuses a guest image whose agent cannot refuse an unusable working
  directory this way with `backend_unavailable`.

### Environment

Each execution starts from its own environment, so nothing carries over from an
earlier one. `process.env` and `process.inheritDefaultEnv` follow MXC's schema:

| `process.env` | `inheritDefaultEnv` | The workload gets |
| --- | --- | --- |
| omitted | ignored | the default environment |
| `[]` | `false` (default) | an empty environment |
| `[]` | `true` | the default environment |
| `["FOO=bar", "EMPTY="]` | `false` (default) | exactly `FOO=bar` and `EMPTY=` |
| `["FOO=bar"]` | `true` | the default environment plus `FOO`; an entry replaces the default of the same name |

The default environment is the guest's own and never holds host variables:
`PATH=/usr/sbin:/usr/bin:/sbin:/bin`, `TERM=linux`, the `HOME`, `USER`, and
`LOGNAME` of the workload identity, and `PWD`, which names the working
directory, plus a few variables that the guest's boot leaves behind (`SHLVL`
and kernel parameters such as `nvx_lifecycle`), which workloads should not
rely on.

In Rust, `ProcessSpec::env` is `None` when `process.env` is omitted and `Some`
of an empty list for `[]`; `"env": null` does not deserialize, because MXC's
schema allows only an array. `ExecRequest::with_env` adds an entry,
`ExecRequest::with_envs` sets the list, and
`ExecRequest::with_inherit_default_env` sets the flag.

- Entries are `KEY=VALUE` strings. A value may hold anything but NUL, such as
  spaces, quotes, newlines, `=`, or nothing at all, and the workload receives it
  as written, without shell interpretation. An entry without `=` or with an
  empty name is `malformed_request`.
- The exec request carries the entries in a field of their own, next to the
  arguments and the working directory, so they leave the argument limits
  untouched. It takes at most 256 entries with unique names, each at most 4096
  bytes, in a request of at most 64 KiB. A repeated name, or more or longer
  entries, is `policy_validation`, before anything runs.
- The guest agent enters the working directory with the workload's identity
  and applies the entries after it has dropped the workload's privileges, just
  before it starts the program. The workload therefore keeps its identity, no
  capabilities, and `no_new_privs`, and a `process.argv` program receives
  exactly the requested environment, also in a working directory. Run
  `/usr/bin/env` through `process.argv` to see it.
- A `process.commandLine` runs in `/bin/sh`, and that shell is the workload, so
  it exports variables of its own: BusyBox's `sh` sets `SHLVL` and `PWD`,
  replacing entries with those names, as it does for any script.

### Host paths

Mapped host paths appear in the guest the way MXC's WSLc backend maps them: a
Windows path `C:\work\src` appears at `/mnt/c/work/src`, and an absolute Linux
path appears at the same path. Host paths may contain commas, as MXC's
`readonlyPaths` and `readwritePaths` allow, so `C:\reports\2026, final` appears
at `/mnt/c/reports/2026, final`. Guest paths derive from the resolved host path,
so links, letter case, and Windows short names do not create second guest
paths for one object. `nvx::resolve_guest_path` performs the same
translation, for example to turn a host working directory into a
`process.cwd`; `nvx::guest_path` translates a path as written.

- OpenVMM offers one virtio-fs device, which the backend attaches as an
  aggregate: a synthetic, read-only root that lists one exported host
  directory or file per child, mounted at `/run/nvx/hostfs/root`, a guest
  directory that only the guest's root can enter. The children are the
  outermost mapped directories and the mapped files outside them, so mapped
  paths may lie anywhere, on any volume. The guest agent then bind-mounts
  each mapped path at its guest path, read-only or read-write, so workloads
  see only the mapped paths.
- The host enforces each mapping's access. A child is read-write only if it
  holds a read-write mapping, and OpenVMM then limits writes to its
  read-write mappings. OpenVMM cannot keep a path read-only inside a writable
  one, so a read-only path inside a read-write directory is refused rather
  than left to the guest's bind mount. OpenVMM exports a mapped file alone:
  the guest reaches neither its siblings nor its directory, which it cannot
  list, inspect, or change, so a read-write file can be rewritten in place
  but not replaced, renamed, or removed. OpenVMM follows the file's name, so
  the guest sees the file that a host program, such as an editor, saves over
  it, and the file's accesses fail with `EACCES` while anything but a
  regular file holds the name. Start refuses to run a sandbox whose mapped
  file a directory replaced, or whose mapped directory a file replaced, and
  OpenVMM refuses such a child too, so a file grant never becomes a
  directory grant. Mapping a whole volume (`/` or `C:\`) is refused; map
  directories inside it.
- Denied paths inside an exported directory are hidden by OpenVMM itself: they
  are absent from listings and inaccessible through any name, except a host
  hard link to a file inside a denied directory, which OpenVMM cannot tell
  from any other file. Below its exported directory, a path that OpenVMM hides
  or singles out, such as a read-write path inside a read-only one, must not
  contain colons, backslashes, links, or whitespace other than spaces, and no
  name in it may begin or end with a space.
- Mapped paths must exist and be directories or regular files. On Windows, a
  mapped or denied path that only a verbatim `\\?\` path names, such as that
  of a directory whose name ends in a period or a space, is refused, since the
  path without the prefix would name another object, or none. A path listed
  both read-only and read-write is mapped read-only. A read-only path,
  directory or file, must not resolve to a path inside a read-write
  directory, but a read-write path may lie inside a read-only directory.
  Start refuses a sandbox that an earlier version provisioned with such a
  path. A denied path must not contain a mapped path, and a denied path that
  does not exist yet must not lie inside a mapped path, because nothing could
  hide it once a workload creates it. Mapping over the guest's own system
  directories (`/usr`, `/etc`, and so on) is refused. Violations are reported
  as `policy_validation`.
- A sandbox maps at most 4096 paths through at most 256 children. A policy
  whose mapped directories, and mapped files outside them, would need more
  children is refused rather than served through the directories that hold
  its files, which would grant more than the files; map directories that
  contain the files instead. OpenVMM hides at most 128 denied paths, and
  singles out at most 128 read-write paths inside read-only ones, in each
  exported directory.
- OpenVMM's command line must fit the 32,767 characters of a Windows process
  command line, with 1,024 held in reserve, on every host, which typically
  leaves room for several hundred paths. Provision rejects a policy beyond
  these limits with `policy_validation`, as it does one whose children would
  export one object twice, such as two mapped host hard links to one file.
  The reserve absorbs configured paths that grow after provision; start
  refuses with `policy_validation` a command line that no longer fits at all.
- Provision records the identity of every mapped and denied object inside a
  read-write mapping, and start refuses to run if one changed, so a workload
  cannot rename a denied or mapped object and leave a decoy at its path for
  the next start. Objects outside read-write mappings are not pinned, so host
  edits that replace them do not block a restart.
- Start hands the guest agent the mapping table over the control channel, in
  requests that each fit one 64 KiB control record; the kernel command line
  carries only the number of entries. The agent refuses workloads until it has
  mounted every entry, and start fails if it cannot mount one. Guest images
  that predate the mapping table do not advertise its control feature, so
  start refuses them.
- On Windows hosts, mapped files appear owned by root with mode `0777`, so the
  workload identity can read and, for read-write paths, write them. On Linux
  hosts virtio-fs shows host owners and modes unchanged; with
  `map_host_identity` (the default), sandboxes that map host paths run their
  workloads under the calling user's IDs, and the guest creates an account for
  them.

To keep part of a directory read-only while workloads write the rest, map the
directory read-only and the paths in it that they write read-write: OpenVMM
then limits writes to those paths.

### Network rules

OpenVMM's portable network profile enforces an egress default plus IPv4 and
IPv6 allow and deny rules; deny rules take precedence, and a rule matches only
destinations of its networks' family, so a denied default blocks the other
family. Each rule lists destination networks (`to`, each a CIDR with optional
`except` sub-networks) and destination `ports` (protocol, `port`, optional
`endPort`). An empty `to` matches every destination of both families, such as
`0.0.0.0/0` and `::/0` together, and an empty `ports` matches every protocol
and port. A `tcp` or `udp` entry without a `port` matches every port of that
protocol, one with a `port` and an `endPort` matches every port from `port`
through `endPort`, an `icmp` entry matches ICMP alone, or ICMPv6 for IPv6
networks, and `any` matches every protocol or, with a port or a port range,
TCP and UDP on those ports.

Rules are lowered into OpenVMM rules exactly, by the lowering that compiles
NVX's egress policy files
([`nvx::host::network::policy`](../nvx-rs/src/host/network/policy/mod.rs)):
exceptions are subtracted from their networks, protocol `any` with a port or a
port range becomes a TCP and a UDP rule, the destinations of each action merge
into the fewest networks that cover them, a rule for every port of a protocol,
or for every protocol, subsumes the narrower rules that it covers, and
overlapping and adjacent port ranges of a network merge into one range rule.
The rules of each action are passed in order of family, address, prefix length,
protocol, and ports, and an action that still needs more than 256 OpenVMM rules
is rejected. Networks are read as policy files read them, so host bits are
cleared and IPv4 netmasks such as `/255.255.0.0` are accepted, but request
validation still refuses both before the backend sees them. Egress
denied without allow rules, with ingress and host loopback denied, attaches no
network device;
otherwise the guest gets `10.0.0.2/24` behind the profile's NAT gateway
`10.0.0.1`, which also serves DNS, and `fd00::a00:2/120` behind the gateway's
IPv6 address `fd00::a00:1`.

With `network.ingress.default: allow`, OpenVMM listens on every host address at
each TCP and UDP port on which the workload listens, within about a second of
the workload starting to listen, and relays clients on the host's private
networks, whose addresses are not globally reachable, to the guest. It refuses
other clients, clients inside the guest network, which the guest would take for
its neighbors, IPv6 link-local clients, and the host itself unless
`network.ingress.hostLoopback` is `allow`. Replies to admitted clients pass
whatever the egress policy. A port that a host process already holds is not
published, and clients on other hosts need the host's firewall to admit them to
OpenVMM; see [Private-network ingress](../doc/run.md#private-network-ingress).

With `network.ingress.hostLoopback: allow`, access between the workload and
host loopback is bidirectional: the workload reaches host loopback services
through its gateway, such as `10.0.0.1:8080` for a host service on
`127.0.0.1:8080`, subject to the egress policy, and the host reaches the
workload. Without ingress allowed, OpenVMM listens on host IPv4 and IPv6
loopback alone at each TCP and UDP port on which the workload listens on a
non-loopback address, admits only the host, and lets replies to it pass
whatever the egress policy; a workload listener on guest loopback stays
private, and a port that a host process already holds is not published.
Host-loopback access needs a `guest_network` of `/29` or larger, since OpenVMM
presents host peers to the workload at an address of that network, and a
policy that allows it for a `/30` fails with `policy_validation`. The
sandbox spec's `hostLoopbackForwards` (`SandboxSpec::with_host_loopback_forward`,
passed to `AciEdgeSandbox::provision_with`) publish up to 64 further host
loopback ports at guest ports of their choice, one forward per protocol and
guest port. They need host-loopback access allowed. OpenVMM lets the
workload's replies on a forwarded TCP connection through whatever the egress
policy, but filters its UDP replies, which go to addresses that OpenVMM assigns
inside the guest network, so UDP forwards also need an egress default of
`allow` and no deny rule that covers the guest network; see
[Host-loopback access](../doc/run.md#host-loopback-access).

### Idempotence and concurrency

- Every provision creates a new sandbox.
- `start` on a running sandbox fails with `already_started`, and `stop` on a
  stopped sandbox fails with `already_stopped`.
- Deprovisioning a running sandbox fails with `already_started`; stop it first.
  After deprovision, every call fails with `stale_id` without writing any
  state, as do calls with IDs that were never provisioned.
- Deprovision removes the sandbox's files, its record last, and then its state
  directory. Files in that directory that are not the backend's stay: the
  deprovision fails with `backend_error`, naming them, and the ID is stale
  from then on.
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
| Invalid request shape, relative or non-UTF-8 path, zero `memoryMib`, `process.timeout` above 4,294,967,295 ms | `MalformedRequest` | `malformed_request` |
| ID without the `aci-edge-sandboxes:` prefix or a 32-digit lowercase hexadecimal token | `MalformedId` | `malformed_id` |
| No state directory for the ID (never provisioned, or deprovisioned) | `StaleId` | `stale_id` |
| `exec` on a stopped sandbox, or a VM that died | `NotStarted` | `not_started` |
| `start` or `deprovision` on a running sandbox | `AlreadyStarted` | `already_started` |
| `stop` on a stopped sandbox | `AlreadyStopped` | `already_stopped` |
| Unsupported policy or exec feature, oversized command, relative or oversized `process.cwd` | `PolicyValidation` | `policy_validation` |
| Missing OpenVMM artifacts, inaccessible hypervisor, incompatible release or guest image, no [runtime identity](#runtime-identity), unsupported host | `BackendUnavailable` | `backend_unavailable` |
| OpenVMM launch failure, boot timeout, control-session failure, a VM of another runtime identity, I/O errors | `BackendError` | `backend_error` |

Start failures name the OpenVMM log, `OpenVmmBackend::log_path`.

The backend keeps the messages of `nvx` errors and maps their kinds onto these
codes: `Unavailable` becomes `backend_unavailable`, `Unsupported` becomes
`policy_validation`, `Exited` becomes `not_started` when an exec finds the
sandbox's VM gone, and every other kind becomes `backend_error`. The same
conversion, without the exec case, turns any `nvx::Error` into an `Error`.
`Error::nvx_code` keeps the error's
[`nvx::ErrorCode`](../nvx-rs/README.md#error-codes), which tells apart the
failures that share an `ErrorCode`, such as a runtime record of another format
(`format-unsupported`) and a VM of another runtime identity
(`runtime-mismatch`). Callers match these codes rather than messages, whose
wording may change. The wire body carries the `ErrorCode` alone.

### Metadata

| Operation | Fields |
| --- | --- |
| provision | none |
| start | `bootMilliseconds`: time from launch to guest readiness |
| stop | `forced`: whether OpenVMM had to be terminated; `vmOutcome`, `vmStatusCode`, `teardownComplete`: summary of OpenVMM's outcome report, for a VM of the backend's runtime identity |
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
python3 scripts/nvx.py test-agent-bundling   # agent bundle staging (repository root)
python3 scripts/nvx.py test-aci-edge-sandboxes --backend kvm   # real VM (repository root)
```

Set `NVX_BUNDLE=skip` and `ACI_EDGE_SANDBOXES_AGENT_BUNDLE=skip` for `--all-features` builds
that should not download the pinned release or the edge agent bundle.
`nvx.py test-agent-bundling`, which the
[`check-aci-edge-sandboxes`](../.github/actions/check-aci-edge-sandboxes/action.yml) action
runs, stages a synthetic agent bundle through `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR`, which the
bundled-agent tests must find, and checks that the build rejects broken copies of it; it needs
neither a hypervisor nor an agent library that runs. The NVX
mechanics are tested in [`nvx-rs`](../nvx-rs/README.md)
as well, with their own unit tests, protocol conformance vectors, and a
fake-OpenVMM lifecycle test.

- `testing::MockBackend` scripts executions for consumers' unit tests.
- The `aci-edge-sandboxes-fake-openvmm` binary (feature `testing`) emulates the OpenVMM
  control console. It runs the fake OpenVMM of `nvx::testing::fake_openvmm`, which
  `nvx` also builds as `nvx-fake-openvmm`; the binary stays in this package
  because `CARGO_BIN_EXE_*` names only binaries of the package under test. It lets the real OpenVMM backend run end to end on hosts
  without a hypervisor: detached launch, reconnection from a new process,
  crash recovery, interrupted-start recovery, forced stop, cancellation,
  working directories, host path mapping tables, refusal of guest images that
  lack required features, and failure injection.
- `cargo run --example lifecycle -- <command>` runs one command with
  discovered artifacts.
- `nvx.py test-aci-edge-sandboxes` runs the ignored `openvmm_e2e` tests against a real
  hypervisor with the repository's kernel and Alpine initramfs: the lifecycle,
  reattachment and the termination of a VM of another runtime identity,
  working directories, host path mapping, network rules, private-network
  ingress, bidirectional host-loopback access, and exec environments.
  CI runs them on
  Linux/KVM, Linux/MSHV, and Windows/WHP.

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
| Backend construction | `nvx::Artifacts::discover` and `OpenVmmConfig::from_artifacts` |
| `policy.readonly_paths`, `readwrite_paths`, `denied_paths` | `FilesystemPolicy` with the same host paths |
| `policy.network_egress` rules | `EgressPolicy` rules, field for field |
| `working_directory` | `ExecRequest::with_cwd(nvx::guest_path(...))`; a directory that the workload cannot enter ends with `Failed(WorkingDirectory)` |
| `process.env`, `process.inheritDefaultEnv` | `ProcessSpec::env`, `None` when omitted, and `inherit_default_env`, or `ExecRequest::with_envs` and `with_inherit_default_env`; see [Environment](#environment) |
| `runtimeConfig.networkProxy` | `ProvisionRequest::with_network_proxy` with the `agent` backend; see [Native proxy and host loopback](#native-proxy-and-host-loopback) |

A proof-of-concept MXC adapter, `nvx_backend`, implements this mapping. It
runs each phase in its own process against a real VM and consumes exec pipes
through MXC's `ExecSandboxProcess`. It is not yet wired into MXC's wire
contract or `ContainmentBackend` routing.

The crate is not published to a registry. A Cargo git dependency on
`microsoft/nvx` also fetches the `openvmm` submodule, which requires access to
`microsoft/openvmm`; vendor the crate if that access is unavailable. The crate and
its data model depend on `nvx` through relative paths, so vendor `nvx-rs/`
beside `aci_edge_sandboxes/`.

## Breaking changes

The NVX mechanics moved to the [`nvx` crate](../nvx-rs/README.md). Requests, results,
wire formats, error codes and messages, and on-disk sandbox state are
unchanged; sandboxes provisioned before the move keep working. These changes
affect Rust callers and builds:

- `Artifacts`, `Hypervisor`, `guest_path`, and `resolve_guest_path` moved from
  `aci_edge_sandboxes::openvmm` to `nvx`, which this crate re-exports as
  `aci_edge_sandboxes::nvx`: use `aci_edge_sandboxes::nvx::Hypervisor` and so
  on. `OpenVmmConfig` and `OpenVmmBackend` stay in `aci_edge_sandboxes::openvmm`.
- `Hypervisor`'s `FromStr` implementation and `Hypervisor::from_env_or_default`
  fail with `nvx::Error` instead of `aci_edge_sandboxes::Error`. Both are
  `ErrorKind::Unavailable` errors with the same messages, and `?` or `Into`
  converts them into `backend_unavailable` errors.
- `aci_edge_sandboxes_model` depends on `nvx`, without its host side, for
  `Cidr`, `Hypervisor`, and the hostname and guest-network rules, so a consumer
  that vendors this crate or its model also vendors `nvx-rs/`.
  `aci_edge_sandboxes_model::cidr`, `aci_edge_sandboxes_model::setup::Hypervisor`,
  and `aci_edge_sandboxes_model::spec::{is_valid_hostname, is_valid_guest_network}`
  keep their paths, and `SetupConfig` keeps its JSON.
- The package of the `nvx` crate, which was called `nvx-rs`, now has its
  library's name; it stays in `nvx-rs/`. A crate that depends on it directly
  declares the dependency as `nvx`, enables its features as `nvx/FEATURE`, and
  keeps its `nvx::` paths.
- The build variables `ACI_EDGE_SANDBOXES_BUNDLE`,
  `ACI_EDGE_SANDBOXES_BUNDLE_DIR`, and `ACI_EDGE_SANDBOXES_BUNDLE_HYPERVISOR`
  became `NVX_BUNDLE`, `NVX_BUNDLE_DIR`, and `NVX_BUNDLE_HYPERVISOR`, and the
  release that `bundled` downloads is pinned in `nvx-rs/artifacts.json`.
  Dependents still receive the staged directory as
  `DEP_ACI_EDGE_SANDBOXES_ARTIFACTS_DIR`.
- A relative `process.cwd` is still rejected with `policy_validation`, and the
  message now points at `nvx::guest_path`.

The OpenVMM backend lowers egress rules with the lowering that compiles NVX's
egress policy files
([`nvx::host::network::policy`](../nvx-rs/src/host/network/policy/mod.rs)),
instead of expanding each rule on its own. What the rules match is unchanged,
and so are the error codes, but these change:

- **Rule order.** The `--network-egress-allow` and `--network-egress-deny`
  values follow family, address, prefix length, protocol, and ports, instead of
  string order: `::/0` precedes `2001:db8::/32`, and `tcp:80` precedes
  `tcp:443`. OpenVMM's matching does not depend on the order.
- **Rule count.** Each action's destinations merge into the fewest networks, a
  rule for every port of a protocol, or for every protocol, subsumes the
  narrower rules that it covers, and overlapping and adjacent port ranges of a
  network merge. Policies usually need fewer OpenVMM rules, so some that
  exceeded the limit of 256 are now accepted: 256 adjacent `/24` destinations
  become one `/16` rule. Port ranges that overlap only partly across networks
  can need a rule for their union, so a policy near the limit can also need
  more rules than before.
- **Messages.** The backend's own rule checks, which `Backend` callers reach
  without the request validation of `AciEdgeSandbox`, report the policy
  compiler's messages, which name the field:
  - `network.egress.allow[0].ports[0]: ICMP has no ports` became
    `network.egress.allow[0].ports[0].port is not supported with icmp`;
  - `....endPort must not be below port` became `....endPort 80 cannot be
    below port 90`, and ports outside 1 through 65535 report `....port must be
    between 1 and 65535`;
  - `...to[0]: the destination is not a network` and `...to[0].except: an
    exception is not a network within the destination` became
    `...to[0].cidr is not a valid IPv4 or IPv6 CIDR: "VALUE"`,
    `...to[0].cidr must not name an IPv6 scope: "VALUE"`, and
    `...to[0].except[1] must be contained in NETWORK`; and
  - `network.egress.allow expands to 300 OpenVMM rules; the openvmm backend
    accepts at most 256`, and the rule-level `expands to more than 256 OpenVMM
    rules` and `covers more than 256 networks after removing its exceptions`,
    became `network.egress.allow emits at most 256 native rules`. This is the
    one message that requests through `AciEdgeSandbox` can also reach.
- **Accepted rules.** Without request validation, the backend now reads a
  network as policy files do: host bits are cleared and IPv4 netmasks are
  accepted, so `10.0.0.7/24` is `10.0.0.0/24`, while port 0, which the backend
  passed to OpenVMM before, is refused. Request validation still refuses such
  networks with `malformed_request` first. A deny rule that covers the guest
  network of host-loopback forwards is detected after the same normalization.

## Limitations

Live standard input requires guest control-protocol work and is rejected today.
Guest state does not survive a stop.
