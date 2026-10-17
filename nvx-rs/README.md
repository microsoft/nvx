# nvx-rs

`nvx-rs` is the Rust crate that holds the NVX mechanics: the OpenVMM
control protocol, and the launch, policy lowering, and supervision of NVX
microVMs. Its library is called `nvx`. The
[`aci_edge_sandboxes` crate](../aci_edge_sandboxes/README.md) builds on it and
re-exports it as `aci_edge_sandboxes::nvx`. The crate also holds the programs
that run in NVX's Linux guests, and the `nvx-rs` command-line interface, the
backend to which `nvx.py` delegates its sandbox, egress-policy, and artifact
steps, as phases of
[microsoft/nvx#356](https://github.com/microsoft/nvx/issues/356) move them to
Rust.

An NVX sandbox is a microVM that OpenVMM runs with the managed lifecycle: the
guest's init starts a managed agent, which runs the workloads that the host
sends it over an authenticated control console. The crate is not published to
a registry, and has a `Cargo.lock` of its own; a workspace at the repository
root would interact with the nested `openvmm/` workspace.

## Layout

```text
nvx-rs/
├── src/
│   ├── protocol/    NVXS records, NVXC frames, EXEC and MAPS payloads, guest features, and
│   │                kernel command-line tokens; no dependencies, shared by the host and the guest
│   ├── contract.rs  control-contract versions and release-manifest compatibility
│   ├── net/         CIDR parsing and subtraction, and hostname and guest-network forms
│   ├── hypervisor.rs
│   ├── error.rs     nvx::Error and its ErrorKind
│   ├── host/        feature `host` (default): artifacts, launch, network and egress policy files,
│   │                filesystem, session, process, platform, VM instances, outcome reports, and
│   │                the sandbox launch profile of `nvx.py`, with the Python semantics they keep;
│   │                with feature `cli`, the `nvx-rs` CLI
│   ├── testing/     feature `testing`: the fake OpenVMM
│   ├── guest/       feature `guest`: the programs that run in Linux guests
│   └── bin/         nvx-rs, nvx-fake-openvmm, and the guest binaries
├── tests/           fake-OpenVMM, CLI, and guest-binary tests, and the golden fixtures that Python
│                    recorded for the ports of its code, in fixtures/
├── build.rs         bundled-release staging
└── artifacts.json   the NVX release that the `bundled` feature stages
```

| Module | Contents |
| --- | --- |
| `nvx::protocol` | The codec of the control console: `NVXS` outer records, `NVXC` application frames, the `EXEC` payload that the host encodes and the guest agent decodes, the `MAPS` payloads of the host mapping table, `GuestFeatures`, and the `nvx_maps=` and `nvx_workload_account=create` tokens. Golden byte fixtures pin every record independently of the codec, which the host and the fake OpenVMM share |
| `nvx::contract` | The OpenVMM microVM ABI, broker protocol, and control-contract revision that the crate speaks, and `manifest_is_compatible` for a release's `SOURCE-MANIFEST.json` |
| `nvx::net` | `Cidr`: IPv4 and IPv6 networks and their subtraction; `is_valid_hostname` and `is_valid_guest_network`, the forms of a VM's hostname and guest address |
| `nvx::Hypervisor` | KVM, MSHV, or WHP, with its serde form behind the `serde` feature |
| `nvx::host::artifacts` | `Artifacts` (also `nvx::Artifacts`): the OpenVMM executable, guest kernel, and Alpine initramfs, from a release, a repository checkout, the environment, a directory next to the executable, or the bundle. `ArtifactDir` and `Guest`: every artifact of the guests that `nvx.py` runs, the OpenVMM executable, the kernel, the Alpine, Ubuntu, and Azure Linux initramfs images and their package manifests, and the Ubuntu distro layer and its manifest, in a checkout's `build/` and `openvmm/target/release/` or a release's `guest/` and `bin/`, checked in the order and with the messages of `nvx.py`. `ArtifactPaths` lists them all and serializes as JSON |
| `nvx::host::launch` | `LaunchSpec` and OpenVMM's command line: the managed lifecycle, the `WorkloadIdentity`, the 1024-byte kernel command line, its reserved parameters, the 32,767-character limit of a Windows command line, and `LaunchSpec::check`, which every command line applies to the memory size, the non-root identity, the hostname, the guest network, and the kernel parameters |
| `nvx::host::network` | `NetworkPolicy` and `openvmm_arguments`: the egress default and rules, ingress, host-loopback access and forwards as OpenVMM options. Egress rules are lowered exactly into OpenVMM's IPv4 and IPv6 rules by the lowering of egress policy files, with at most 256 rules for each action |
| `nvx::host::network::policy` | Egress policy files: `compile_policy_file`, `compile_policy_bytes`, and `compile_policy` compile one to a `CompiledEgressPolicy` of native rules; see [Egress policy files](#egress-policy-files) |
| `nvx::host::filesystem` | The `ExportPlan` of host paths through OpenVMM's aggregate virtio-fs device, the host-mapping table, the reserved guest targets, and `guest_path` and `resolve_guest_path` (also at `nvx::`) |
| `nvx::host::instance` | `Instance`: a VM in a directory of its own. It starts OpenVMM with a fresh capability and waits until the guest is ready and has mounted the mapped paths, reconciles recorded state, recovers interrupted launches, runs workloads and pumps their output, and stops the VM with a forced fallback |
| `nvx::host::session`, `process`, `platform`, `report` | The authenticated control session, OpenVMM processes, Linux and Windows primitives, and OpenVMM's outcome report |
| `nvx::host::sandbox` | The launch profile of `nvx.py sandbox`, ported from the one that `scripts/nvx_tools/sandbox.py` held: `SandboxLaunch`, with one to three EROFS `SandboxLayer`s in role order, the ext4 scratch disk, the entrypoint, arguments, hostname, workload identity, and resource limits, and its `--microvm-sandbox-block` and `--microvm-workload-identity` arguments and kernel command line, within the 1024-byte budget that OpenVMM leaves it. `mount`: `SandboxMount` live shares of host directories and files, their guest targets and the reserved ones, their access policies and owner, the aggregate of several shares, and the checks of their host paths. `options`: the checks that `nvx.py sandbox` and `nvx.py run` apply to their options: the attribution of `--mount-deny`, `--mount-allow`, and `--mount-write` to shares, the systemd policy, the targets of `nvx.py run` shares, and the managed exec environment; see [Python parity](#python-parity) |
| `nvx::testing::fake_openvmm` | A fake OpenVMM that serves the control console and runs scripted workloads without a VM, for direct launches and for the managed and one-shot sandboxes of `nvx-rs sandbox` and `nvx.py sandbox` |
| `nvx::host::cli` | Feature `cli`: the `nvx-rs` executable; see [The nvx-rs CLI](#the-nvx-rs-cli) |
| `nvx::guest` | The programs that run in Linux guests; see [Guest binaries](#guest-binaries) |

Lowering functions take the names by which messages refer to their inputs, such
as `EgressRules::name` and `PathList::name`, so callers' messages name their own
fields.

## Egress policy files

`nvx::host::network::policy` compiles the egress policy files that
`nvx.py run` and `nvx.py sandbox` take with `--network-egress-policy-file`
([format](../doc/run.md#run-openvmm-directly)). It is a faithful port of the
former `scripts/nvx_tools/egress_policy.py`, which it replaced when `nvx.py`
delegated to `nvx-rs`: it accepts the same files, compiles them to the same
rules, and fails with the same messages.

- A policy is at most 1 MiB of UTF-8 JSON whose root has only `allow` and
  `deny` arrays. JSON is read as Python's `json.loads` reads it, so `NaN` and
  `Infinity` are numbers, while a repeated field and an integer of more than 64
  digits are errors.
- Rules take the MXC form, with optional `to` destinations (`cidr` and
  `except`) and `ports` selectors (`protocol`, `port`, and `endPort`), or the
  legacy flat form (`cidr`, `except`, `protocol`, `port`, and `endPort`).
  Protocols are `tcp`, `udp`, `icmp`, and `any`, and TCP and UDP take one port
  or a range. A CIDR is read as Python's `ipaddress.ip_interface` reads it:
  host bits are cleared, IPv4 netmasks such as `/255.255.0.0` are accepted, and
  IPv6 scopes are refused.
- Each action, `allow` or `deny`, lowers to at most 256 native rules:
  exceptions are subtracted, destinations merge into the fewest networks, rules
  for every port of a protocol subsume the narrower rules that they cover, and
  a sweep over the port boundaries merges overlapping and adjacent ranges. The
  rules sort by family, address, prefix length, protocol, and ports, and read
  `CIDR`, `CIDR:PROTOCOL`, `CIDR:PROTOCOL:PORT`, or `CIDR:PROTOCOL:FIRST-LAST`.

`openvmm_arguments` lowers `NetworkPolicy` rules, and so `aci_edge_sandboxes`
policies, the same way: an `EgressRule` is a policy file's MXC rule, its
messages name `EgressRules::name` where a file's name `allow` or `deny`, such
as `network.egress.allow[0].to[1].cidr is not a valid IPv4 or IPv6 CIDR: x`,
and the checks that only `NetworkPolicy` has, such as the host-loopback ones,
stay in `openvmm_arguments`.

Messages match Python's byte for byte, but two cases have no Rust equivalent. A
string that Python would hold with an unpaired surrogate, such as one decoded
from a `\ud800` escape or a path that is not Unicode, shows the surrogate as
`\udXXX`, which is how Python prints it, and paths show as
`str(pathlib.Path(path))` does. Python's decoder raises `RecursionError` for a
document nested more deeply than its stack allows; this reader has no such
limit, so those documents fail as invalid policies instead.

[`tests/fixtures/egress-policy.json`](tests/fixtures/egress-policy.json) holds
the Python compiler's outcomes for every policy that its tests compiled,
hand-written edge cases, and a seeded random corpus, which the tests replay
without Python; `NVX_EGRESS_POLICY_FIXTURE` points the tests at another corpus
in the same format. See [Python parity](#python-parity).

## Python parity

`nvx::host::network::policy`, `nvx::host::sandbox`, and the guest lookups of
`nvx::host::artifacts` replaced Python code that `nvx.py` ran, so they keep its
contract byte for byte: its limits, option semantics, outputs, and messages.
They check values as Python does: whitespace is what `str.isspace` accepts,
integers are what `int` reads, UUIDs what `uuid.UUID` reads, and JSON what
`json.loads` reads, failing with its messages; host paths are normalized as
`pathlib` normalizes them, with backslashes on Windows, and resolved as
`Path.resolve` resolves them; file identities are those of `os.stat`, which on
Windows are the volume serial number and the 128-bit file ID; and messages
quote values as `repr` does. The private `python` module implements these
semantics after CPython 3.14, with the Unicode 16.0.0 tables of Python 3.14.
Python decodes operating-system strings that are not valid Unicode into
surrogates: paths print as Python prints them, and the filesystem queries
refuse them.

`tests/fixtures` holds what the Python code did with fixed inputs and seeded
random corpora: `python.json` for the Python semantics, `sandbox.json` and
`sandbox_options.json` for the launch profile and the sandbox options, for
POSIX and Windows paths alike, `egress-policy.json` for the policy compiler,
and `cli.json` for `nvx.py sandbox` as a command; see
[Golden scenarios](#golden-scenarios). `scripts/nvx_rs_fixtures.py` recorded
them with CPython 3.14, and generated the Unicode tables of
`src/host/python/unicode.rs`, until `nvx.py` delegated to `nvx-rs` and
[microsoft/nvx#359](https://github.com/microsoft/nvx/issues/359) retired the
script with the Python code that it recorded. The golden tests compare the
ports with these records and need no Python. The records are now the
reference of that behavior: a deliberate change of behavior updates the
records that it changes by hand, in the same change.

## The nvx-rs CLI

The `cli` feature builds the `nvx-rs` executable, a faithful port of the
runtime steps of `nvx.py` that `nvx.py` delegates to it
([microsoft/nvx#359](https://github.com/microsoft/nvx/issues/359)):

```text
nvx-rs [-h] [--root PATH] [--prog PROG] artifacts
nvx-rs [-h] [--root PATH] [--prog PROG] egress compile PATH
nvx-rs [-h] [--root PATH] [--prog PROG] sandbox [run|provision|start|exec|stop|deprovision] [OPTIONS]
```

```bash
cd nvx-rs
cargo build --release --features cli   # target/release/nvx-rs
```

Errors print `error: MESSAGE` to standard error and exit with status 1, as
`nvx.py` reports a `ScriptError`. Usage errors print the parser's usage and
`PROG: error: MESSAGE` and exit with status 2, as `argparse` does, and help
exits with status 0. Ctrl-C prints `Interrupted` and exits with status 130.
`sandbox run` and `sandbox exec` exit with the status of OpenVMM or of the
workload. Text ends its lines as Python's `print` ends them, with `\r\n` on
Windows and `\n` elsewhere, while workload output passes through unchanged.

`--prog PROG` names the program in usage, help, and error messages, `nvx-rs` by
default. `nvx.py` passes its own name, so that the usage errors of
`nvx.py sandbox` that `nvx-rs` reports, such as an invalid `--layer`, read
`usage: nvx.py sandbox ...` and `nvx.py sandbox: error: ...` as they did before
`nvx.py` delegated the command. The usage wraps for the name as `argparse` wraps
it for 80 columns.

### Artifact root

Every command uses the artifacts of one root: a repository checkout, with the
guest artifacts in `build/` and OpenVMM in `openvmm/target/release/`, or an
extracted release, with them in `guest/` and `bin/`.

- `--root PATH` names the root, as `nvx.py`, which knows its repository root,
  passes it. A directory that holds `bin/openvmm[.exe]`, `guest/vmlinux`, or
  `guest/initramfs.cpio.gz` is a release, and any other is a checkout.
- Without `--root`, the root is the release whose `bin/` holds the executable,
  as for a release's `bin/nvx-rs[.exe]`, or the checkout whose `nvx-rs/target/`
  holds it at any depth, as after `cargo build`. Either must have its
  `SOURCE-MANIFEST.json`; otherwise the command fails and asks for `--root`.
- The `NVX_OPENVMM` environment variable names an OpenVMM executable to run
  instead of the root's, such as `nvx-fake-openvmm` in tests. A relative path is
  relative to the working directory.

### `artifacts`

Prints the paths of the artifacts as JSON, as `json.dumps(value, indent=2,
sort_keys=True)` writes it. Paths are absolute and not checked:

```json
{
  "guests": [
    {
      "distribution": "Alpine Linux",
      "distro_layer": null,
      "distro_manifest": null,
      "initramfs": "/nvx/build/initramfs.cpio.gz",
      "name": "alpine",
      "package_manifest": "/nvx/build/initramfs.cpio.gz.packages.json"
    },
    {
      "distribution": "Ubuntu Base",
      "distro_layer": "/nvx/build/ubuntu-distro.erofs",
      "distro_manifest": "/nvx/build/ubuntu-distro.erofs.manifest.json",
      "initramfs": "/nvx/build/initramfs-ubuntu.cpio.gz",
      "name": "ubuntu",
      "package_manifest": "/nvx/build/initramfs-ubuntu.cpio.gz.packages.json"
    },
    {
      "distribution": "Azure Linux",
      "distro_layer": null,
      "distro_manifest": null,
      "initramfs": "/nvx/build/initramfs-azurelinux.cpio.gz",
      "name": "azurelinux",
      "package_manifest": "/nvx/build/initramfs-azurelinux.cpio.gz.packages.json"
    }
  ],
  "kernel": "/nvx/build/vmlinux",
  "layout": "repository",
  "openvmm": "/nvx/openvmm/target/release/openvmm",
  "root": "/nvx"
}
```

`layout` is `repository` or `release`, the guests are listed in the order of
`nvx.py`, a guest without a distro layer has `null` for it and its manifest,
and `openvmm` is `NVX_OPENVMM` when that is set.

### `egress compile PATH`

Compiles an egress policy file, as `--network-egress-policy-file` does, and
prints its native OpenVMM rules in the same JSON form:
`{"allow": [RULE, ...], "deny": [RULE, ...]}`, each list in the order in which
OpenVMM receives the rules, which are `--network-egress-allow` and
`--network-egress-deny` values. A file that `nvx.py` refuses fails with its
message.

### `sandbox`

`nvx-rs sandbox` keeps the contract of `nvx.py sandbox`: every option of its
parser, with the same names, defaults, and value checks, and the same
operations, checks, messages, outputs, exit statuses, outcome reports, and
state directories. The operation defaults to `run`. Options take their values
as `--option VALUE` or `--option=VALUE`, repeated options accumulate, and
`--mount-deny`, `--mount-allow`, and `--mount-write` apply to the `--mount`
before them.

- `run` prints `>> COMMAND`, OpenVMM's command line, quoted as
  `subprocess.list2cmdline` quotes it on Windows and as `shlex.join` does
  elsewhere. Unless `--dry-run` is given, it then runs OpenVMM with the
  standard streams and exits with its status. `--outcome-report` asks OpenVMM
  for its outcome report.
- `provision` records the sandbox in its state directory. `start` launches
  OpenVMM detached with the managed lifecycle, passes it a fresh capability
  through its standard input, and waits until the guest answers. `exec` runs a
  workload, prints its standard output and then its standard error, and exits
  with its status; `--outcome-report` writes
  `{"operation_id": HEX, "outcome": {"category": CATEGORY, "operation":
  "exec", "status_code": STATUS}, "schema_version": 1}`. `stop` shuts the VM
  down and prints nothing. `deprovision` removes the state directory.

The state directory and its files are those that `nvx.py` wrote before it
delegated to `nvx-rs`, so `nvx-rs` and an older `nvx.py` can each operate a
sandbox that the other provisioned or started:

| File | Contents |
| --- | --- |
| `config.json` | The provisioned sandbox, in the lowest of formats 1 to 6 that holds it, written as `json.dumps(value, indent=2, sort_keys=True)` writes it, with mode 0600, through a temporary file |
| `runtime.json` | Format 1: the `pid` and `start_time` of the running OpenVMM, as Python identifies a process, and its `control_endpoint`: `STATE/control.sock`, or `//./pipe/openvmm-microvm-HEX` on Windows |
| `control.capability` | The 32-byte capability of the running OpenVMM, with mode 0600 |
| `control.sock` | The control endpoint on POSIX hosts |
| `openvmm.log` | OpenVMM's output, appended by each start |
| `outcome.json` | OpenVMM's outcome report of the last start |
| `lifecycle.lock` | The lock that serializes lifecycle transitions, an exclusive `flock` on POSIX and a lock of its first byte on Windows, as Python takes it; deprovision leaves `deprovisioned` in it while it removes the directory |

#### Guest features

After the guest answers, `start` asks it for its control features and
requires `GuestFeatures::LAYERED_SANDBOX`, the features that its launches use:
per-execution environments (bit 4), since `exec` sends a working directory and
an environment. It needs neither cancellation (bit 0), since the CLI never
cancels a workload, and Ctrl-C ends the session, nor workload accounts
(bit 2, [#566](https://github.com/microsoft/nvx/issues/566)). A guest image
that lacks a required feature, or that predates the request, is stopped as a
failed start is, and the start fails with
`the guest image lacks control features that layered sandboxes need (...)`,
which names the missing features. `nvx.py` does not ask; this is the one
intended change of behavior. The guest agents of NVX advertise bits 0, 2, and 4
in sandbox mode.

#### Other differences from nvx.py

- Long options cannot be abbreviated; `nvx.py` passes them in full.
- Help and usage are formatted for 80 columns, as `argparse` formats them for a
  pipe, and an invalid choice lists the choices quoted, as Python 3.14.5 and
  later do.
- Where `nvx.py` would end with a Python traceback, as for a configuration
  without a field that `start` reads, `nvx-rs` prints `error: MESSAGE`, with the
  message of the exception, and exits with status 1.
- Text goes to pipes and files in UTF-8, where Python on Windows uses the ANSI
  code page unless `PYTHONIOENCODING` or UTF-8 mode says otherwise.
- `start` refuses a control endpoint that another process serves, which it
  treats as one that has not appeared yet, and, on Windows, starts OpenVMM
  without a console window, as `nvx::host::process` starts it.
- `exec` refuses more than 1 MiB of workload output, which the guest agent
  never forwards.

### Test hooks

`--root` and `NVX_OPENVMM` let tests run the CLI without a VM. `nvx-fake-openvmm`
serves the sandbox profile: its managed lifecycle, with the capability on its
standard input, the control console, and an outcome report at stop, and its
one-shot runs, which run `/bin/echo`, `/bin/sleep`, `/bin/true`, or
`/bin/false`, write an outcome report if asked, and exit with the workload's
status. Kernel command-line tokens make it fail: `fake_exit_on_start=STATUS`,
`fake_exit_status=STATUS`, `fake_guest_features=MASK`, and
`fake_legacy_guest=1`, among others; see `nvx::testing::fake_openvmm`.

```bash
cd nvx-rs
cargo build --features cli,testing --bins
NVX_OPENVMM=target/debug/nvx-fake-openvmm \
  target/debug/nvx-rs --root .. sandbox run --layer distro,LAYER,UUID \
  --scratch SCRATCH --entrypoint /bin/echo --arg hello
```

### Golden scenarios

`tests/fixtures/cli.json` holds scenarios that the fixture script recorded from
the Python implementation of `nvx.py sandbox`, run as a command with the root's
OpenVMM a copy of `nvx-fake-openvmm`, and from the Python functions behind
`egress compile` and `artifacts`: the option and usage errors, dry runs,
one-shot runs, the lifecycles of sandboxes in every configuration format, and
the refusals of the lifecycle operations. Each step records the exit status,
standard output and error, and the state files, with the root, process IDs, and
random identifiers as placeholders. Its `posix` and `windows` sections were
recorded on each kind of host, where the script also played every scenario with
`nvx-rs`, and each lifecycle with the Python implementation and `nvx-rs` taking
turns both ways, and found no difference. `tests/cli.rs` replays the section of
its host with `nvx-rs`, without Python; since `nvx.py sandbox` runs `nvx-rs`,
these scenarios are its reference.

## Features

| Feature | Contents |
| --- | --- |
| `host` (default) | The `host` module. It enables `serde`, and `sha2`, which names the children of a sandbox's aggregate share |
| `bundled` | Stages a release at build time; see [Bundled artifacts](#bundled-artifacts) |
| `testing` | The fake OpenVMM and the `nvx-fake-openvmm` binary |
| `cli` | The `nvx-rs` CLI of `host::cli` and its `nvx-rs` binary; it enables `host`. See [The nvx-rs CLI](#the-nvx-rs-cli) |
| `serde` | The serde form of `Hypervisor` |
| `guest` | The `guest` module and the guest binaries; see [Guest binaries](#guest-binaries) |

Without default features, the crate has only `protocol`, `contract`, `net`,
`Hypervisor`, and `Error`, and depends on nothing but `serde` when `serde` is
enabled and `nix` and `getrandom` when `guest` is; `aci_edge_sandboxes_model`
uses it that way.

## Example

```rust,no_run
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use nvx::host::instance::{self, Instance};
use nvx::host::launch::LaunchSpec;
use nvx::protocol::{Workload, WorkloadEnvironment};
use nvx::{Artifacts, Hypervisor};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let spec = LaunchSpec::new(Artifacts::discover()?, Hypervisor::from_env_or_default()?);
    let directory = std::env::temp_dir().join("nvx-example");
    instance::create_private_dir(&directory)?;
    let instance = Instance::new(&directory, "example");
    let started = instance.start(&spec, Duration::from_secs(60))?;

    let workload = Workload {
        argv: vec!["/bin/echo".to_owned(), "hello".to_owned()],
        timeout_ms: 0,
        cwd: None,
        environment: WorkloadEnvironment::Default,
    };
    let timeout = Duration::from_secs(60);
    let capability = instance.read_capability()?;
    let execution = instance.exec(&started.runtime, &capability, &workload, timeout, timeout)?;
    let mut stdout = Vec::new();
    let outcome = execution.pump(
        &mut |chunk: &[u8]| {
            stdout.extend_from_slice(chunk);
            Ok(())
        },
        &mut |_: &[u8]| Ok(()),
        &AtomicBool::new(false),
    )?;
    println!("{outcome:?}: {}", String::from_utf8_lossy(&stdout));
    instance.stop(&started.runtime, Duration::from_secs(30))?;
    Ok(())
}
```

## Errors

Every fallible operation returns `nvx::Error`: a message that callers can show
as is, an `ErrorKind`, and an optional cause.

| `ErrorKind` | Meaning |
| --- | --- |
| `Unavailable` | A missing or unusable artifact, hypervisor, or release, a guest image without a required control feature, or an unsupported host |
| `Unsupported` | A well-formed request that NVX cannot represent or enforce, such as a policy beyond a limit of OpenVMM or the guest agent |
| `Exited` | The VM's OpenVMM process exited |
| `Io` | A host I/O failure, or state that NVX recorded on the host and cannot read |
| `Protocol` | A control session that failed, or a request that the guest agent refused |
| `TimedOut` | A deadline that passed |
| `Other` | Any other failure |

Messages call an instance `sandbox NAME`, after the name that `Instance::new`
gives it. `aci_edge_sandboxes` maps the kinds onto its error codes; see its
[error mapping](../aci_edge_sandboxes/README.md#error-mapping).

## Bundled artifacts

With the `bundled` feature, the build script stages the OpenVMM executable,
guest kernel, and Alpine initramfs of a release, with its
`SOURCE-MANIFEST.json` and `SHA256SUMS`, in its output directory, and
`Artifacts::bundled` returns them. `Artifacts::discover` uses the bundle when no
other source is present.

| Build variable | Effect |
| --- | --- |
| `NVX_BUNDLE_DIR` | Stages the extracted release in this directory, for offline builds or local artifacts |
| `NVX_BUNDLE_HYPERVISOR` | `kvm` (default) or `mshv`: the Linux package to download |
| `NVX_BUNDLE=skip` | Stages nothing, as when the feature is disabled; CI sets it for `--all-features` builds |

Without `NVX_BUNDLE_DIR`, the build script downloads the release package that
[`artifacts.json`](artifacts.json) pins with `curl` and extracts it with `tar`;
Windows packages are ZIP archives, which need bsdtar (Windows' own `tar`) or
`unzip`. Every staged file must match the source's `SHA256SUMS`, a download
must match its pinned digest, and the release must declare the control contract
of `nvx::contract`. The build scripts of dependents receive the staged
directory as `DEP_NVX_ARTIFACTS_DIR` and can ship it next to their executables
as `nvx/`.

## Guest binaries

The `guest` feature adds the programs that run in NVX's Linux guests: the
`nvx::guest` module and one binary per program. The Alpine, Ubuntu, and Azure
Linux images ship all nine at their established `/sbin/nvx-*` paths.

| Binary | Program |
| --- | --- |
| `nvx-console-pending DEVICE` | Prints the number of bytes waiting to be read from a device, such as a console |
| `nvx-mmio-write ADDRESS VALUE` | Stores a 32-bit value at a physical address through `/dev/mem` (x86_64) |
| `nvx-reseed [--generation-only] SEED_FILE [PREVIOUS]`, `nvx-reseed --sample` | Reseeds the kernel's random number generator from a seed file and checks its VM generation ID, or prints a random sample |
| `nvx-exit [CODE]` | Stops the VM with an 8-bit exit code through OpenVMM's control port 0x604 (x86_64) |
| `nvx-device-io file ...`, `nvx-device-io network ...` | The benchmark helper that measures file and UDP round-trip I/O and prints one JSON line (x86_64) |
| `nvx-managed-agent CONTROL_TTY ROOTFS HOSTNAME UID GID USER HOME` | Serves the managed control session and supervises sequential workloads; `--exec-config-fd FD -- WORKLOAD ...` is its internal launch helper |
| `nvx-ingress GATEWAY_IPV4` | Announces reachable TCP listeners and settled UDP sockets to the portable-network gateway; `test DIRECTORY` runs synthetic procfs snapshots |
| `nvx-time [--report-only] COMMAND` | Enforces NVX time ABI v1: runs its conformance checks, repairs the wall clock after a restore, publishes the time state, and runs the discipline and violation-watcher daemon; `test NAME ...` runs its decoder and process test hooks (x86_64) |
| `nvx-time-probe latency\|warp\|clock ...` | Diagnoses clock-read latency, cross-CPU TSC warp and offset, the clocks, CPUID, clock events, kernel messages, and optionally MSRs (x86_64) |

Each binary's command line, exit statuses, and byte output are compatibility
contracts; the [`nvx::guest`](src/guest/mod.rs) documentation describes each
program. Guests run them as static `x86_64-unknown-linux-musl` executables. On
Linux, with the musl target and a C compiler driver, `cc`, `nvx.py` builds them:

```bash
rustup target add x86_64-unknown-linux-musl
python3 scripts/nvx.py build-guest-binaries \
  --output build/nvx-rs-guest-binaries
```

Native guest builds, the digest-pinned Docker Rust stage, and CI all call this
routine. Docker builds the set once and installs identical bytes in all three
guest images; Ubuntu determinism checks rebuild from fresh targets.

## Testing

```bash
cd nvx-rs
NVX_BUNDLE=skip cargo test --all-features   # unit, golden-fixture, parity-fixture, and fake-OpenVMM tests
```

The tests need no hypervisor. The fake-OpenVMM test drives `Instance` end to end
against `nvx-fake-openvmm`, the CLI test replays the
[golden scenarios](#golden-scenarios) and checks the guest-feature
requirement and, on Linux, Ctrl-C, and shared protocol definitions plus golden
byte fixtures cover both sides of every record. On Linux, a loopback test drives
the host `ControlSession` through a broker shim into the real managed agent's
session, dispatch, and execution engine, and `nvx.py` also lints and tests the
guest binaries, and runs their integration tests against a build of them:

```bash
python3 ../scripts/nvx.py check-guest-binaries
python3 ../scripts/nvx.py test-guest-binaries \
  --binaries ../build/nvx-rs-guest-binaries
```

The
[`check-nvx-rs`](../.github/actions/check-nvx-rs/action.yml) action runs
rustfmt, Clippy with warnings denied for every feature and for the library
without default features, the tests, a synthetic-release `bundled` test,
rustdoc with warnings denied, the minimum supported Rust version (1.89), and a
macOS check. On Linux, it also lints, builds, and tests the guest binaries
through `nvx.py`. `python3 scripts/nvx.py test-aci-edge-sandboxes --backend kvm`
exercises the crate on a real hypervisor through `aci_edge_sandboxes`.
