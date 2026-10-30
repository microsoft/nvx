# Command-line usage

NVX has two command-line interfaces, which run from the repository root:

- [`nvx-rs`](#nvx-rs) runs NVX microVMs and sandboxes: [`run`](#run) boots or
  restores a microVM on the terminal, [`sandbox`](#sandbox) runs or manages
  workloads over EROFS layers and a private ext4 scratch or tmpfs upper, and
  [`download`](#download) installs the release of the checkout, which `run`
  and `sandbox` also download into a checkout that holds no artifacts.
- `scripts/nvx.py`, with Python 3.10 or newer, builds, tests, benchmarks, and
  packages NVX:

  ```text
  python3 scripts/nvx.py COMMAND [OPTIONS]
  ```

The examples on this page use the POSIX spelling. On Windows, use
`nvx-rs\target\release\nvx-rs.exe` and `python scripts\nvx.py` instead.
Every command accepts `-h` or `--help`, including nested commands:

```console
nvx-rs/target/release/nvx-rs run --help
python3 scripts/nvx.py --help
python3 scripts/nvx.py performance gate --help
```

## nvx-rs

[`nvx-rs`](../nvx-rs/README.md#the-nvx-rs-cli) is the Rust command-line
interface that runs NVX's microVMs and sandboxes. Cargo builds it from the
repository root at `nvx-rs/target/release/nvx-rs`, or `nvx-rs.exe` on Windows,
with the Rust release that `rust-toolchain.toml` pins;
[rustup](https://rustup.rs) installs Cargo. `--target-dir` keeps the executable
there even where `CARGO_TARGET_DIR` names another directory. Build it again
after changing `nvx-rs`:

```console
cargo build --release --locked --features cli --manifest-path nvx-rs/Cargo.toml --target-dir nvx-rs/target
```

Without `--root`, `nvx-rs` uses the artifacts of the checkout that holds it:
the guest artifacts in `build/` and OpenVMM in `openvmm/target/release/`.
[`download`](#download) installs those of a release there, and `run` and
`sandbox` download them first into a Git checkout that holds none of them.
The `nvx-rs` of an extracted release package, `bin/nvx-rs`, uses the release's
artifacts; see [Artifact root](../nvx-rs/README.md#artifact-root).

The [`test-microvm`](#test-microvm) scenarios run `nvx-rs` too. They run, in
this order:

1. the executable that the `NVX_RS` environment variable names;
2. `nvx-rs/target/release/nvx-rs[.exe]` in the checkout; or else
3. that executable, after they build it with the command above, with Cargo's
   output on standard error, so that it never mixes with their output.

Without `NVX_RS`, a built executable, or Cargo, they fail and name these
choices. They rebuild nothing once the executable exists. The
`managed-lifecycle` scenario also needs Cargo, which runs its `nvx-rs` test.

The provenance of a checkout's `nvx-rs`, `build/nvx-rs.provenance.json`, binds
the SHA-256 of its executable to a digest of the `nvx-rs` sources that built
it, the crate's manifest, lock file, build script, and `src/` tree and
`rust-toolchain.toml`, and to the Rust release, target, and Cargo features of
the build. `nvx.py` records it when it builds `nvx-rs` for the scenarios, and
[`record-nvx-rs-provenance`](#record-nvx-rs-provenance) records it for a build
made otherwise, such as with the command above. [`package`](#package) and the
[adversarial executor](adversarial-testing.md#nvx-rs-on-the-target) refuse an
`nvx-rs` that it does not describe as built from the checkout's sources, so a
stale build never ships or runs unnoticed.

### Error codes

`nvx-rs` reports each error on standard error as `error[CODE]: MESSAGE`, and
exits with status 1, and each usage error as the usage of its parser and then
`PROG: error[CODE]: MESSAGE`, such as `nvx-rs run: error[argument-invalid]:
...`, and exits with status 2. Each error is one line: a line break in a value
that its message names, such as a path, prints as `\n` or `\r`. `CODE` is a
stable code, which tells errors apart where their messages may change: callers
match codes, and stable fragments of messages, such as the names of options,
fields, and files, never whole messages. A code keeps its meaning and is never
reused, and a later release may add codes, which callers treat as failures of
the command. The usage errors that `nvx.py` itself reports, as `argparse` does,
carry no code, and neither does an interruption by SIGINT or, outside
Windows, SIGTERM, which prints `Interrupted` and exits with status 130. The
[`nvx-rs` README](../nvx-rs/README.md#error-codes) gives the `ErrorKind` of
each code for callers of the `nvx` library.

| Code | Status | Meaning |
| --- | --- | --- |
| `argument-unrecognized` | 2 | The command line holds an argument that the command does not take, such as an unknown option. |
| `argument-missing` | 2 | A required argument, or the value of an option, is missing. |
| `argument-invalid` | 2 | An argument has an invalid value, such as one that is not among its choices, not a number, not of its form, or not valid Unicode. |
| `unsupported` | 1 | NVX refuses the request: options that it cannot combine, a value that it does not accept, or a policy that it cannot represent or enforce. The message names the option or field. |
| `unavailable` | 1 | A dependency is missing or unusable, other than as a finer code says: install metadata, the hypervisor, a release, a distro layer or scratch image that the request names, or a host that NVX does not support. |
| `artifact-missing` | 1 | An artifact that the command runs, such as OpenVMM, the guest kernel, or an initramfs, does not exist, or no artifact root holds them. |
| `guest-features-missing` | 1 | The guest image lacks control features that the sandbox needs, which the message names. |
| `sandbox-not-provisioned` | 1 | No sandbox is provisioned at `--state-dir`, which does not exist, is not a plain directory, or holds no configuration. |
| `sandbox-provisioned` | 1 | `provision` found the sandbox provisioned already, or it was provisioned again while `deprovision` removed it. |
| `sandbox-not-running` | 1 | `exec` or `stop` found the VM of a provisioned sandbox not running. |
| `sandbox-running` | 1 | `start` or `deprovision` found the sandbox's VM, or a launch of it, running. |
| `sandbox-exited` | 1 | The sandbox's OpenVMM exited without a `stop`: the transition that finds it reports it and clears its runtime record, and `start` launches the sandbox again. |
| `sandbox-busy` | 1 | Another lifecycle transition held the sandbox's lock until `--timeout` passed. |
| `runtime-mismatch` | 1 | The sandbox's VM runs under another runtime identity than the artifacts', whose differing parts the message names, so only `stop`, which terminates it, applies. |
| `vmm-indeterminate` | 1 | Whether the sandbox's OpenVMM runs cannot be determined, so its records remain for a later attempt. |
| `format-unsupported` | 1 | A record or report has a format that this build does not read, such as the state of a sandbox that another build provisioned or started. |
| `foreign-files` | 1 | `deprovision` found files that NVX does not own in the state directory, which the message names, and kept them with the directory. |
| `outcome-report-exists` | 1 | A file exists at the `--outcome-report` path. |
| `scratch-claimed` | 1 | Another sandbox claims the scratch image. |
| `sandbox-leased` | 1 | The sandbox holds a lease of a template's generation, which allows no other lease, and one launch of its clone, until a release ends it once its VM has exited. |
| `template-ineligible` | 1 | The restore window of a template's generation, which the time contract of its capture sets, excludes the host's time, so no clone may restore it now. |
| `template-tombstoned` | 1 | A tombstone quarantines the template's generation, which takes no new clones. |
| `exited` | 1 | OpenVMM exited, as when a start fails, whose message names the category and status of OpenVMM's outcome report when it has one. |
| `exec-refused` | 1 | The guest agent refused the workload, as for a working directory that it cannot use. |
| `endpoint-closed` | 1 | OpenVMM closed the control endpoint, as it does when the VM shuts down. |
| `protocol` | 1 | The control session failed otherwise, as when the guest breaks the protocol or the session is reset. |
| `timed-out` | 1 | Another deadline passed, such as that of a `stop` or of a control request. |
| `io` | 1 | A host I/O operation failed, or state that NVX recorded is unreadable or malformed. |
| `not-found` | 1 | Other state that the command names does not exist on the host. |
| `other` | 1 | Any other failure. |

### `run`

```text
nvx-rs/target/release/nvx-rs run
    [--guest {alpine,ubuntu,azurelinux}]
    [--hypervisor {auto,whp,kvm,mshv}]
    [--machine {microvm}]
    [--memory-mib MIB]
    [--memory-capacity-mib MIB]
    [--processors {1,2,4,8}]
    [--cpu-profile ID]
    [--mount GUEST_TARGET,HOST_PATH[,ro|rw]]...
    [--mount-deny HOST_PATH]...
    [--mount-allow HOST_PATH]...
    [--mount-write HOST_PATH]...
    [--mount-owner {vmm,caller}]
    [--net IPV4/PREFIX]
    [--network-profile {portable}]
    [--network-egress {allow,deny}]
    [--network-ingress {allow,deny}]
    [--network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]]...
    [--network-egress-deny CIDR[:PROTOCOL[:PORT[-PORT]]]]...
    [--network-egress-policy-file PATH]
    [--host-loopback {allow,deny}]
    [--network-proxy IPV4:TCP-PORT]
    [--host-loopback-forward PROTOCOL:HOST_PORT:GUEST_PORT]...
    [--outcome-report PATH]
    [--cmdline TEXT]
    [--restore-snapshot PATH]
    [--restore-processors {1,2,4,8}]
    [--restore-memory-mib MIB]
    [--restore-ready-path PATH]
    [--dry-run]
```

| Option | Default | Description |
| --- | --- | --- |
| `--guest {alpine,ubuntu,azurelinux}` | `alpine` | Select Alpine, Ubuntu, or Azure Linux userland with the same NVX kernel. This option is not used for snapshot restore. |
| `--hypervisor {auto,whp,kvm,mshv}` | `auto` | Select the OpenVMM hypervisor. `auto` chooses WHP on Windows and KVM elsewhere. |
| `--machine {microvm}` | `microvm` | Select the fixed-topology microVM with shared-status edge interrupts. |
| `--memory-mib MIB` | guest-specific | Set guest memory in MiB, an ASCII decimal integer from 1 to 17592186044415, without signs, whitespace, or underscores. Defaults to 128 for Alpine, 512 for Ubuntu, and 512 for Azure Linux. |
| `--memory-capacity-mib MIB` | none | Reserve an immutable, 128 MiB-aligned RAM capacity for a fresh microVM snapshot, in the range of `--memory-mib`. |
| `--processors {1,2,4,8}` | `1` | Select the microVM processor count, written as one of the choices. |
| `--cpu-profile ID` | `auto` | Select the guest's [CPU profile](#cpu-profiles): `auto` for the built-in profile of the host's CPU, which falls back with a warning to a development profile derived from this host on an Intel or AMD CPU that no built-in profile serves, a built-in profile ID, or `host` for that development profile. A restore uses the snapshot's profile, which `auto` and, for a host profile, `host` also name. |
| `--mount GUEST_TARGET,HOST_PATH[,ro\|rw]` | none | Expose a host directory, or a single regular host file without its directory, at the guest target: an absolute, canonical path other than `/`, without whitespace, control characters, double quotes, backslashes, `=`, or any UTF-8 byte that Linux treats as whitespace, such as the `0xa0` in `à`. The mode follows the last comma, so `HOST_PATH` may contain commas when the mode is given. Repeat to expose more directories or files, each with its own target and mode, as the children of an aggregate on the microVM's one virtio-fs device, which the guest binds at their targets; a shared file always uses the aggregate. Targets and host paths must not overlap, and the shares' kernel command-line tokens must fit its 1024-byte budget. An `rw` mapping accepts guest-created symbolic links, which the host never follows. Active snapshot restore requires the same mappings in the same order, with the same canonical paths, kinds, targets, modes, and ownership mode; a dormant-slot restore may attach one new directory mapping that the resumed guest mounts explicitly. |
| `--mount-deny HOST_PATH` | none | Hide one existing file or directory inside a mounted host root; repeat to deny multiple paths. It may name the root itself when `--mount-allow` paths inside it remain. With several mappings, or a shared file, the path must be absolute; a shared file takes none. |
| `--mount-allow HOST_PATH` | none | Expose one existing file or directory inside a `--mount-deny` path again; the hidden directories on the way list only the entries that lead to it. Repeat to allow multiple paths. With several mappings, or a shared file, the path must be absolute; a shared file takes none. See [Access policy](run.md#access-policy). |
| `--mount-write HOST_PATH` | none | Make one existing file or directory one of the only writable parts of an `rw` mapping; the guest's writes anywhere else in it fail with `EROFS`. Repeat to declare multiple writable paths. With several mappings, or a shared file, the path must be absolute; a shared file takes none. Active snapshot restore requires the same denied, allowed, and writable paths. |
| `--mount-owner {vmm,caller}` | `vmm` | Select the host identity of the guest's operations on every `--mount` directory or file. `caller` performs each one as the guest caller's UID and GID and squashes guest root to the owner of the shared directory or file; it requires a Linux host. See [Run](run.md#file-ownership). |
| `--net IPV4/PREFIX` | none | Enable virtio-net with the static guest IPv4 address and prefix. The guest also gets the IPv6 address that embeds it in `fd00::/96`, such as `fd00::a00:2/120` for `10.0.0.2/24`. A restore takes the addresses from the snapshot and rejects this option. |
| `--network-profile {portable}` | none | Select the required cross-platform network behavior contract; must be specified with `--net` on a fresh boot, and without it to restore a snapshot captured with `--net`. |
| `--network-egress {allow,deny}` | `allow` | Set the default guest egress policy. |
| `--network-ingress {allow,deny}` | `deny` | Set the default host ingress policy. `allow` publishes every TCP and UDP port on which the guest listens to clients on the host's private networks, and to the host itself only with `--host-loopback allow`; see [Private-network ingress](run.md#private-network-ingress). Snapshot capture and restore reject `allow`. |
| `--network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]` | none | Allow matching guest egress; repeat to add rules. `CIDR` is an IPv4 or IPv6 address or network, and a rule matches only its own family. `PROTOCOL` is `tcp`, `udp`, or `icmp` and matches every port when `PORT` is omitted; only `tcp` and `udp` take a `PORT` or an inclusive `PORT-PORT` range, such as `8000-8010`. |
| `--network-egress-deny CIDR[:PROTOCOL[:PORT[-PORT]]]` | none | Deny matching guest egress; repeat to add rules. Deny rules take precedence. `PROTOCOL` and `PORT` select traffic as for `--network-egress-allow`. |
| `--network-egress-policy-file PATH` | none | Load bounded IPv4 and IPv6 ranges and rule-local CIDR exclusions from JSON. Requires explicit `--network-egress`; cannot be mixed with explicit allow/deny rule flags. |
| `--host-loopback {allow,deny}` | existing mapping | Control access between the guest and host loopback services. Explicit `allow` is bidirectional: the guest reaches host loopback through its gateway, and every TCP and UDP port on which the guest listens is published on host loopback for the host alone, or with `--network-ingress allow` admits the host as well; it needs a `--net` prefix of `/29` or shorter, which OpenVMM checks; see [Host-loopback access](run.md#host-loopback-access). Snapshot capture and restore reject `allow`. |
| `--network-proxy IPV4:TCP-PORT` | none | Allow one explicit host TCP proxy endpoint. |
| `--host-loopback-forward PROTOCOL:HOST_PORT:GUEST_PORT` | none | Publish one TCP or UDP localhost port to the guest; repeat to add forwards. Snapshot capture and restore reject forwards. |
| `--outcome-report PATH` | none | Write a bounded local JSON outcome report. |
| `--cmdline TEXT` | empty | Append kernel parameters; `nvx_*` and `tsc=` tokens are reserved. |
| `--restore-snapshot PATH` | none | Restore the immutable machine contract and saved state from a snapshot directory. A snapshot captured with `--net` needs `--network-profile portable` and the network policy options of its capture, which the restore cannot change; see [Networked snapshots](run.md#networked-snapshots). |
| `--restore-processors {1,2,4,8}` | none | Bring this contiguous processor prefix online before restore readiness. Requires an opt-in microVM snapshot and cannot exceed `--processors` capacity. |
| `--restore-memory-mib MIB` | none | Select the 128 MiB-aligned RAM target for an expansion-capable snapshot restore, in the range of `--memory-mib`. |
| `--restore-ready-path PATH` | none | Publish one restore-readiness event to an existing Unix socket or Windows named pipe. |
| `--dry-run` | off | Print the generated OpenVMM command without running it. |

`nvx-rs run` checks its options, prints the OpenVMM command after `>> `, and
unless `--dry-run` is given, runs OpenVMM with the standard streams. It exits
with status 1 for an error, 2 for a usage error, and 130 after Ctrl-C, and
otherwise with the status that OpenVMM returns. Its errors and usage errors
carry their [error codes](#error-codes), as `error[CODE]: MESSAGE` and
`nvx-rs run: error[CODE]: MESSAGE`. A cold boot requires the OpenVMM release
binary, `build/vmlinux`, and the selected `build/initramfs*.cpio.gz`, and a
restore only OpenVMM. When one of them is missing from a Git checkout that
holds none of the files that a release installs, `run` first installs the
release of the checkout, as [`download`](#download) does, reporting on standard
error; a checkout that holds some of them gets a warning instead, and the
missing artifact fails the run. An option takes its value from the next
argument, or after `=` in its own, which a value that starts with `-` needs
unless it starts like a negative number in ASCII digits, such as `-1`, and its
name cannot be abbreviated. Ubuntu selection never falls back to Alpine. See
[Run](run.md) for host setup, guest shutdown, networking, and virtio-fs
examples.

If no built-in CPU profile serves an Intel or AMD host's CPU, OpenVMM falls
back to a host profile and warns with `NVX-CPU-PROFILE-FALLBACK:` at every
cold boot, as [CPU profiles](#cpu-profiles) describes. After a failed cold
boot on such a CPU, `run` names the CPU, lists the CPUs that the built-in
profiles cover, says that `auto` falls back on it, and asks for the CPU's
fingerprint through a [CPU profile request](#cpu-profiles), with the OpenVMM
command that writes the fingerprint. OpenVMM's
own error, above the guidance, names the cause of the failure, and says so
where the host profile cannot boot. On a CPU of another vendor that no
built-in profile serves, OpenVMM exits with `E_PROFILE_HOST_UNKNOWN` before it
creates the VM, and `run` explains that host profiles cannot boot there
either. A host whose hypervisor does not support its built-in profile fails
with `E_PROFILE_UNSUPPORTED` instead, and on an Intel or AMD CPU OpenVMM's
error itself names `--cpu-profile host`. `nvx-rs run` identifies the CPU with
CPUID, as OpenVMM does, derives the guidance from its copy of the CPUs that the
built-in profiles serve, and prints it only after OpenVMM fails a cold boot,
never after a dry run, a restore, an option that `nvx-rs` refuses, or Ctrl-C.

### `sandbox`

```text
nvx-rs/target/release/nvx-rs sandbox
    [{run,provision,start,exec,stop,deprovision}]
    [--layer ROLE,PATH,EROFS_UUID]...
    [--scratch PATH]
    [--upper {scratch,ram}]
    [--state-dir PATH]
    [--entrypoint PATH]
    [--arg VALUE]...
    [--hostname NAME]
    [--workload-user UID:GID]
    [--memory-max BYTES]
    [--pids-max COUNT]
    [--memory-mib MIB]
    [--timeout SECONDS]
    [--exec-timeout-ms MILLISECONDS]
    [--cwd GUEST_PATH]
    [--environment KEY=VALUE]...
    [--environment-file PATH]
    [--inherit-default-environment]
    [--hypervisor {auto,whp,kvm,mshv}]
    [--mount GUEST_TARGET,HOST_PATH[,ro|rw]]...
    [--mount-deny HOST_PATH]...
    [--mount-allow HOST_PATH]...
    [--mount-write HOST_PATH]...
    [--mount-owner {vmm,caller}]
    [--net IPV4/PREFIX]
    [--network-profile {portable}]
    [--network-egress {allow,deny}]
    [--network-ingress {allow,deny}]
    [--network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]]...
    [--network-egress-deny CIDR[:PROTOCOL[:PORT[-PORT]]]]...
    [--network-egress-policy-file PATH]
    [--host-loopback {allow,deny}]
    [--network-proxy IPV4:TCP-PORT]
    [--host-loopback-forward PROTOCOL:HOST_PORT:GUEST_PORT]...
    [--outcome-report PATH]
    [--cmdline TEXT]
    [--dry-run]
```

Network policy and live-share options configure only the `run` and `provision`
launches, which check the network options as one typed policy before anything
launches or is recorded; see [Sandbox networking](run.md#sandbox-networking).

`nvx-rs sandbox` runs the operation, `run` by default, with the options
given. It exits with status 1 for an error, 2 for a usage error, and 130 after
Ctrl-C, and otherwise with the status that the workload or OpenVMM returns. Its
errors and usage errors carry their [error codes](#error-codes), as
`error[CODE]: MESSAGE` and `nvx-rs sandbox: error[CODE]: MESSAGE`; an `exec`
prints its workload's standard output and error before any error of its own,
which starts a new line. Options take their values as those of [`run`](#run)
do, and their names cannot be abbreviated either.

| Option | Default | Description |
| --- | --- | --- |
| `{run,provision,start,exec,stop,deprovision}` | `run` | Select a one-shot run or a managed lifecycle operation. |
| `--layer ROLE,PATH,EROFS_UUID` | required for `run` and `provision` | Attach the `distro` EROFS layer. Only `distro` is accepted. The UUID must be lowercase hexadecimal in canonical hyphenated form; braces, prefixes, uppercase, and unhyphenated forms are refused. |
| `--scratch PATH` | required for `run` and `provision` with a scratch upper | Attach a preformatted ext4 scratch image as the overlay's writable upper. `--upper ram` refuses it. |
| `--upper {scratch,ram}` | `scratch` | Select the overlay's writable upper for `run` or `provision`: `scratch`, the image of `--scratch`, or `ram`, a tmpfs in guest memory with no scratch image, which loses the workload's writes when the VM stops. A RAM upper holds at most half of the guest's RAM, and its pages count against `--memory-max`; see [Overlay upper](run.md#overlay-upper). |
| `--state-dir PATH` | required for managed operations | Select persistent sandbox state. One-shot `run` rejects this option. |
| `--entrypoint PATH` | `/bin/sh` | Select an absolute workload entrypoint without whitespace, control characters, double quotes, or any UTF-8 byte that Linux treats as whitespace. |
| `--arg VALUE` | none | Append one nonempty entrypoint argument without whitespace, control characters, double quotes, or any UTF-8 byte that Linux treats as whitespace. Repeat to pass multiple arguments. |
| `--hostname NAME` | `nvx-sandbox` | Set the workload UTS hostname. |
| `--workload-user UID:GID` | `65534:65534` | Select the fixed non-root workload identity for `run` or `provision`. Both IDs must be ASCII decimal integers from 1 to 4294967294, without signs, whitespace, or underscores. |
| `--memory-max BYTES` | none | Set the workload cgroup memory limit to an ASCII decimal integer from 1 to 18446744073709551615, without signs, whitespace, or underscores. |
| `--pids-max COUNT` | none | Set the workload cgroup process limit to an ASCII decimal integer from 1 to 4194303, without signs, whitespace, or underscores. The guest adds one process for init. |
| `--memory-mib MIB` | `256` | Set guest memory in MiB, an ASCII decimal integer from 1 to 17592186044415, without signs, whitespace, or underscores. |
| `--timeout SECONDS` | `60` | Set the timeout for each managed control request and its response in `start`, `exec`, and `stop`, and the longest time that `provision`, `start`, `stop`, and `deprovision` wait for another lifecycle transition of the sandbox. It is a decimal number, with an optional fraction and exponent, that must be finite and greater than 0. |
| `--exec-timeout-ms MILLISECONDS` | `0` | Set the managed `exec` guest workload timeout to an ASCII decimal integer from 0 to 4294967295, without signs, whitespace, or underscores; zero disables the workload deadline. This is separate from the finite host `--timeout` control deadline. |
| `--cwd GUEST_PATH` | `/` | Set an absolute working directory inside the workload root for managed `exec`. It is resolved after the workload identity and root are applied; missing, inaccessible, or non-directory paths fail the workload launch. Default and layered environments point `PWD` at this directory; an exact replacement environment controls `PWD` itself. |
| `--environment KEY=VALUE` | omitted | Set the exact managed `exec` environment. Repeat for multiple entries. Empty values, spaces, additional equals signs, and UTF-8 are preserved. Inline values are visible in the invoking host process arguments; use `--environment-file` for sensitive values. |
| `--environment-file PATH` | omitted | Read the exact managed `exec` environment from a strict UTF-8 JSON array of `KEY=VALUE` strings, limited to 1 MiB of input. Invalid JSON, non-finite numbers, and unpaired surrogate escapes are refused. An empty array requests an empty environment. This option is mutually exclusive with `--environment`; omitting both preserves guest defaults. |
| `--inherit-default-environment` | off | Layer the managed `exec` environment from `--environment` or `--environment-file` over the guest default environment instead of replacing it; an entry replaces the default variable of the same name. Without either option the workload already receives the defaults, so the flag has no effect. |
| `--hypervisor {auto,whp,kvm,mshv}` | `auto` | Select the host hypervisor. |
| `--mount GUEST_TARGET,HOST_PATH[,ro\|rw]` | none | Live-share a host directory, or a single regular host file without its directory, at the absolute target inside the container rootfs for `run` or `provision`; defaults to `ro`. The target cannot contain whitespace, control characters, double quotes, backslashes, `=`, or any UTF-8 byte that Linux treats as whitespace, such as the `0xa0` in `à`, so that it stays one kernel command-line token in the shares of a one-shot `run`. The mode follows the last comma, so `HOST_PATH` may contain commas when the mode is given. Repeat to attach more shares, each with its own target and mode, for example a read-write workspace and a read-only tool cache, as the children of an aggregate that the guest binds at their targets; a shared file always uses the aggregate, and so does every share of a managed sandbox. Targets and host paths must not overlap. A one-shot `run` describes its shares in kernel command-line tokens that count toward the sandbox's 1024-byte budget; a managed sandbox announces only their number and sends their mapping table once its guest agent is ready, for at most 256 shares. An `rw` share accepts guest-created symbolic links, which the host never follows. `/`, `/etc`, `/run`, and `/run/nvx`, and the `/etc/machine-id`, `/etc/resolv.conf`, `/proc`, `/sys`, `/dev`, `/.nvx-agent`, and `/run/nvx/time` trees, are reserved. |
| `--mount-deny HOST_PATH` | none | Hide one existing file or directory inside a `--mount` host directory; relative paths are resolved inside it. It may name the directory itself when `--mount-allow` paths inside it remain. With several shares, it applies to the `--mount` before it, which must share a directory. Repeat to deny multiple paths. |
| `--mount-allow HOST_PATH` | none | Expose one existing file or directory inside a `--mount-deny` path of the same share again, for example a readable `mcp-payloads` directory inside hidden logs. The hidden directories on the way list only the entries that lead to it, and the workload cannot modify them. Relative paths are resolved inside the share; with several shares, it applies to the `--mount` before it. Repeat to allow multiple paths. See [Access policy](run.md#access-policy). |
| `--mount-write HOST_PATH` | none | Make one existing file or directory one of the only writable parts of an `rw` share; the workload's writes anywhere else in the share fail with `EROFS`. Relative paths are resolved inside the share; with several shares, it applies to the `--mount` before it, which must be `rw`. Repeat to declare multiple writable paths. `provision` persists the denied, allowed, and writable paths. |
| `--mount-owner {vmm,caller}` | `vmm` | Select the host identity of the shares' file operations for `run` or `provision`. `vmm` performs them as OpenVMM; `caller` performs them as the workload's UID and GID, squashes guest root to the owner of each shared host directory or file, and fails them with `EPERM` when OpenVMM cannot assume that identity. `caller` requires a Linux host and is persisted by `provision`. See [File ownership](run.md#file-ownership). |
| `--net IPV4/PREFIX` | none | Enable virtio-net with a static guest address. The guest also gets the IPv6 address that embeds it in `fd00::/96`. The other network options need it. A policy with `--network-egress deny` and `--host-loopback deny`, and no allow rules, `--network-ingress allow`, or `--network-proxy`, attaches no network device, whatever its deny rules; a policy file's allow rules are those that it compiles to. Any other policy attaches one, even one that lets nothing through; see [Sandbox networking](run.md#sandbox-networking). |
| `--network-profile {portable}` | none | Select the required cross-platform network behavior contract; must be specified with `--net`. |
| `--network-egress {allow,deny}` | `allow` | Set the default guest egress policy for `run` or `provision`. |
| `--network-ingress {allow,deny}` | `deny` | Set the host ingress policy for `run` or `provision`. `allow` publishes every TCP and UDP port on which the workload listens to clients on the host's private networks, and to the host itself only with `--host-loopback allow`; see [Private-network ingress](run.md#private-network-ingress). |
| `--network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]` | none | Allow matching guest egress; repeat to add rules. Requires explicit `--network-egress`. `CIDR` is an IPv4 or IPv6 address or network, and a rule matches only its own family. `PROTOCOL` is `tcp`, `udp`, or `icmp` and matches every port when `PORT` is omitted; only `tcp` and `udp` take a `PORT` or an inclusive `PORT-PORT` range, such as `8000-8010`. |
| `--network-egress-deny CIDR[:PROTOCOL[:PORT[-PORT]]]` | none | Deny matching guest egress; repeat to add rules. Requires explicit `--network-egress`; deny rules take precedence. `PROTOCOL` and `PORT` select traffic as for `--network-egress-allow`. |
| `--network-egress-policy-file PATH` | none | Load bounded IPv4 and IPv6 ranges and rule-local CIDR exclusions for `run` or `provision`. Managed provision persists the compiled rules in its network policy, not this path. Requires explicit `--network-egress`; cannot be mixed with explicit allow/deny rule flags. |
| `--host-loopback {allow,deny}` | guest to host | Control access between the workload and host loopback services for `run` or `provision`. Without the option, the workload reaches host loopback through its gateway, subject to the egress policy, and no port is published; `deny` closes both directions. Explicit `allow` is bidirectional: the workload reaches host loopback through its gateway, and every TCP and UDP port on which the workload listens is published on host loopback for the host alone, or with `--network-ingress allow` admits the host as well; see [Host-loopback access](run.md#host-loopback-access). |
| `--network-proxy IPV4:TCP-PORT` | none | Make the network proxy-only: the workload reaches the HTTP or HTTPS proxy that listens on host `127.0.0.1:TCP-PORT` at `IPV4:TCP-PORT`, where `IPV4` must be the guest gateway, and nothing else. `TCP-PORT` can't be 53, where OpenVMM answers DNS at the gateway. Requires `--network-egress deny` and `--host-loopback deny`, and refuses egress rules, `--network-ingress allow`, and forwards. NVX gives the workload no proxy settings; see [Proxy-only networking](run.md#proxy-only-networking). |
| `--host-loopback-forward PROTOCOL:HOST_PORT:GUEST_PORT` | none | Publish one TCP or UDP localhost port to the guest; repeat to add forwards. Requires `--host-loopback allow`. A UDP forward also requires egress allowed by default and no deny rule that covers the guest network, which carries its replies; OpenVMM lets the replies on a forwarded TCP connection through whatever the egress policy. |
| `--outcome-report PATH` | none | Write a bounded local JSON outcome report for one-shot `run` or managed `exec`. |
| `--cmdline TEXT` | empty | Append non-sandbox kernel parameters, which `nvx-rs` reads as Linux does: split at whitespace, which includes the `0xa0` in `à`, outside double quotes, with names that match whether they spell a dash or an underscore. Quoted values, such as `a="b c"`, keep their whitespace. Parameters whose names start with `nvx_` or `virtfs_` are reserved and refused, even inside a quoted value, and so is a `--` or unclosed quote that hides the sandbox's own tokens; see [Experimental single-workload sandbox](run.md#experimental-single-workload-sandbox). The guest powers off if its kernel applies a clock parameter that the time ABI forbids, such as `tsc=unstable`. |
| `--dry-run` | off | For one-shot `run` only, print the generated OpenVMM microVM command without running it. Managed operations reject this option. |

`run` and `start` install the release of a Git checkout that holds no
artifacts before they launch, as `nvx-rs run` does.

See [Run](run.md) for artifact preparation, the security boundary, and current
snapshot/configuration limitations.

### `download`

```text
nvx-rs/target/release/nvx-rs download
    [--repository OWNER/REPOSITORY]
    [--hypervisor {auto,whp,kvm,mshv}]
```

| Option | Default | Description |
| --- | --- | --- |
| `--repository OWNER/REPOSITORY` | `microsoft/nvx` | GitHub repository from which to download the release. |
| `--hypervisor {auto,whp,kvm,mshv}` | `auto` | Select the release platform. `auto` chooses WHP on Windows and KVM on Linux. |

`download` installs a release into the repository checkout of the artifact
root, which it refuses to be an extracted release. It first looks for the
selected platform's package in the release that CI built from the checked-out
commit. When that package exists, the packaged artifacts match the checkout.
CI publishes a development release tagged
`v<VERSION>-dev.<first 12 characters of the commit>` for each `dev` commit whose
checks pass. When `HEAD` has no release, as with CI's `[skip ci]` baseline
commits, commits whose checks failed, and local commits, or its release has no
package for the selected platform, `download` uses the nearest first-parent
ancestor that has one. Those fallback artifacts match that ancestor, not
`HEAD`. It checks at most 20 commits, at one GitHub API request each, and reads
the history with `git`. Update the checkout to install a newer release.

`download` checks the size of the package, the names of its members, and every
file against the package's `SHA256SUMS`, and then installs it: it replaces the
packaged guest artifacts under `build/`, among them the sandbox scratch
template, `build/ubuntu-smoke-scratch.ext4`, that the sandbox scenarios of
[`test-microvm`](#test-microvm) and the runtime adapter copy, removes any known
guest artifact that the release does not declare, such as the Azure Linux
guest that source-inclusive packages omit, or the template that releases built
before it shipped lack, and installs the packaged OpenVMM at
`openvmm/target/release/openvmm[.exe]`, executable, with the build provenance
of the artifacts in `build/`. It installs everything or, if a step fails,
nothing, and fails on a directory where the release has a file rather than
delete what the directory holds. It leaves the checkout's `nvx-rs` alone, which
Cargo builds; see [nvx-rs](#nvx-rs). It warns when the release declares another
microVM ABI or control contract than the `nvx-rs` that installs it speaks, as a
fallback release may.

`run`, `sandbox run`, and `sandbox start` install the release of the checkout
themselves, before they launch, when an artifact that they run is missing from
a Git checkout that holds none of the files that a release installs: OpenVMM,
the guest artifacts, and their build provenance. A command whose options they
refuse downloads nothing. They report the download on standard error, so their
output stays their own. They install no release over a checkout that holds
some of those files, which may be local builds: they warn instead, and the
missing artifact fails them. A file of a release that appears while they
download, as from a local build, cancels their installation with a warning, so
that none of the release is installed, and any file that it already installed
and the build has since rewritten or replaced stays the build's. Set
`NVX_AUTO_DOWNLOAD=0` to turn this off, as CI does where it tests the artifacts
that it built.

One download at a time installs into a checkout: it holds the lock of
`build/.nvx-download.lock` while it searches for, downloads, and installs a
release, and another waits for it. `run`, `sandbox run`, and `sandbox start`
share that lock, creating its file if needed, from the moment they check the
artifacts of a checkout, or `sandbox run` reads its distro layer, which may be
the release's, until they have launched OpenVMM on them, and `sandbox start`
until its VM is ready. They thus never see part of a release, and no download
replaces the files that they read. `sandbox provision` shares the lock while it
reads its distro layer and records the sandbox, and needs no artifact root
otherwise. `sandbox exec`, `stop`, and `deprovision` share the lock too while
they read the runtime identity of the artifacts of a sandbox that started, and
download nothing. A run that waited for a download downloads nothing if the
artifacts that it runs are then in place. With `NVX_AUTO_DOWNLOAD=0`, commands
take no lock.

Windows release downloads support WHP. Linux release downloads support KVM
and MSHV. `download` runs the host's `curl` and `tar`, those of
`%SystemRoot%\System32` on Windows 10 and later, and `curl` honors the usual
proxy environment variables. `download` first uses `GH_TOKEN` or
`GITHUB_TOKEN` when configured, which `curl` receives on its standard input and
sends to the GitHub API alone, never to the host that serves the package. If
GitHub rejects that token with HTTP 401 or 403, NVX reports the failure and
retries without credentials so public releases remain downloadable. Private
repositories require a token with read access to the repository contents that
is authorized for the organization when it enforces single sign-on.

## Commands

| Command | Description |
| --- | --- |
| `init` | Initialize the OpenVMM submodule and its nested submodules. |
| `build-guest` | Build the Linux kernel and selected guest artifacts. |
| `check-guest-binaries` | Lint and test the Rust guest executables for musl. |
| `build-guest-binaries` | Build and digest the pinned Rust executables for Linux guests. |
| `test-guest-binaries` | Run the musl integration tests against prebuilt Rust guest executables. |
| `build-kernel` | Build the pinned and patched Linux kernel natively. |
| `build-initramfs` | Build the selected Alpine, Ubuntu, or Azure Linux initramfs. |
| `build-distro-layer` | Build a deterministic Ubuntu EROFS distro layer. |
| `build-scratch-template` | Format the reproducible ext4 sandbox scratch template. |
| `verify-guest-determinism` | Rebuild Ubuntu guest artifacts twice and compare SHA-256 values. |
| `build-openvmm` | Build the OpenVMM release binary. |
| `record-openvmm-provenance` | Bind an existing OpenVMM binary to the pinned source revision. |
| `record-nvx-rs-provenance` | Bind an existing `nvx-rs` executable to its sources, Rust release, target, and features. |
| `materialize-kernel-provenance-inputs` | Write kernel provenance inputs from raw run-head blobs. |
| `setup-cross-os-cache` | Install GNU tar and zstd for GitHub Actions cross-OS caches. |
| `classify-ci-changes` | Select the CI suites that a list of changed files runs. |
| `kernel-patch-identity` | Print the digest of the kernel patch paths and contents that keys the kernel cache. |
| `guest-binary-source-identity` | Print the digest of the Rust guest binary source paths and contents that keys the guest image caches. |
| `record-guest-artifacts` | Record the cache keys and the SHA-256 of each file of guest artifact components, which a merge group or a `dev` push checks before it reuses them. |
| `reuse-guest-artifacts` | Install guest artifact components that missed the cache from the latest same-repository pull request run for the merged head. |
| `check-required-ci` | Validate required GitHub Actions job results. |
| `report-nightly-ci` | Open, update, or close the report of failed nightly CI runs. |
| `cancel-superseded-merge-groups` | Cancel the CI runs of the merge groups that the merge queue replaced. |
| `check-static-elf` | Check that executables are static ELF files without an interpreter or an executable stack. |
| `test-openvmm-unit` | Run the OpenVMM workspace unit and documentation tests. |
| `build-openvmm-tests` | Build OpenVMM Petri VMM tests without running them. |
| `openvmm-test-cache-key` | Print the key under which CI caches the OpenVMM VMM tests that `build-openvmm-tests` builds. |
| `check-openvmm-tests` | Check that a directory holds the OpenVMM VMM tests that `build-openvmm-tests` builds from this checkout. |
| `reuse-openvmm-tests` | Keep OpenVMM VMM tests that a cache restored if `build-openvmm-tests` builds them from this checkout, and remove them otherwise. |
| `test-openvmm` | Run OpenVMM Petri VMM tests. |
| `plan-openvmm-test-shards` | Split the OpenVMM VMM tests into shards that nextest runs in balanced times for CI. |
| `test-microvm` | Run NVX Linux and device correctness tests through OpenVMM. |
| `plan-microvm-shards` | Split the default microVM suite into shards of balanced scenario durations for CI. |
| `doctor` | Qualify this host for the NVX time ABI. |
| `test-runtime-adapter` | Run the runtime adapter's consolidated suite by name on a real hypervisor. |
| `test-adversarial` | Run a brokered Copilot-driven adversarial campaign. |
| `build` | Build the guest artifacts and OpenVMM. |
| `benchmark` | Run the OpenVMM-native benchmark coordinator. |
| `performance` | Collect, gate, and persist CI performance results, and check benchmark hosts. |
| `collect-sources` | Materialize verified Linux, Alpine, and Ubuntu release sources. |
| `collect-alpine-sources` | Collect exact Alpine recipes and upstream sources. |
| `collect-ubuntu-sources` | Collect exact Ubuntu source packages. |
| `create-linux-source-archive` | Create a Linux corresponding-source archive. |
| `package` | Stage a binary distribution. |
| `archive-release` | Create a deterministic archive from a staged distribution. |
| `verify` | Verify source and submodule inputs. |

Ctrl-C, that is SIGINT, interrupts a command, and so does SIGTERM outside
Windows, which is how CI ends a cancelled step (see
[Cancellation](ci.md#cancellation)). The command then runs its cleanup, as
`test-microvm` and `benchmark` stop their OpenVMM processes and remove their
scratch directories, prints `Interrupted`, and exits with status 130. On
Linux, the OpenVMM processes that they start, and the process trees that they
contain, end with `nvx.py` even when something kills it outright.

## Initialization and verification

### `init`

```console
python3 scripts/nvx.py init
```

Initializes and recursively updates the OpenVMM Git submodule.

### `verify`

```console
python3 scripts/nvx.py verify
```

Verifies the repository's pinned source inputs and submodule state.

### `setup-cross-os-cache`

```console
python3 scripts/nvx.py setup-cross-os-cache
```

Installs the GNU tar and zstd tools used by GitHub Actions cross-OS caches.

### `kernel-patch-identity`

```text
python3 scripts/nvx.py kernel-patch-identity [--root PATH]
```

Prints a digest of each file under `kernel/patches/` by relative path, in path
order, together with its content. The guest build appends it to both kernel
cache keys, because `hashFiles` hashes contents only: a renamed patch changes
the key even when its bytes do not.

### `guest-binary-source-identity`

```text
python3 scripts/nvx.py guest-binary-source-identity [--root PATH]
```

Prints the source digest that guest package manifests record for the Rust guest
binaries: a digest of the relative path and content of every file that their
build reads. Those are the crate's manifest, lockfile, and build script, its
Cargo configuration under `nvx-rs/.cargo/`, every file under `nvx-rs/src/`
except the host side and the test modules, `rust-toolchain.toml`, and the
`build_constants.py` and `guest_binaries.py` build modules. The host side is
the `host` and `testing` modules and the `nvx-rs` and `nvx-fake-openvmm`
binaries; the test modules are the `tests.rs` files, the `tests` directories,
and the managed agent's `loopback` harness. `GuestRustBuildConstants` in
`scripts/nvx_tools/build_constants.py` defines the set, and
[`build-guest-binaries`](#build-guest-binaries) fails when Cargo's dep-info
lists a file outside it. The Alpine, Ubuntu, and Azure Linux cache keys end
with the digest and hash no other file of `nvx-rs`, so a change to the
crate's host side or tests rebuilds no guest image. Because the digest covers
paths, and `hashFiles` hashes contents only, moving a module into its own
`mod.rs` changes the keys even when no bytes change.

### `record-guest-artifacts`

```text
python3 scripts/nvx.py record-guest-artifacts --components COMPONENTS
    --keys JSON [--directory PATH]
```

Writes the guest artifact record, `guest-artifacts.json`, into the directory
of the guest artifacts, `build/` by default. `--components` lists, separated
by spaces, some of the `kernel`, `alpine`, `ubuntu`, and `azurelinux`
components of the CI guest build, whose `ubuntu` component also holds the
sandbox scratch template; the CI debug kernel is in no record. The record
holds the exact cache keys of those components' cache entries, from the JSON
object of keys by name that `--keys` gives, and the SHA-256 of each of their
files. It fails if a key or a file is missing. Each CI job that builds guest
images records them, and uploads the record beside the components' own
artifacts, so that the merge group and the `dev` push that merge the pull
request can check them before they reuse them; see
[Guest artifact reuse](ci.md#guest-artifact-reuse).

### `reuse-guest-artifacts`

```text
python3 scripts/nvx.py reuse-guest-artifacts
    --repository OWNER/REPOSITORY
    --workflow-ref REF
    --record-artifact NAME
    --components COMPONENTS
    --keys JSON
    (--commit SHA | --head-sha SHA)
    [--dry-run]
    [--destination PATH]
```

When the destination, `build/` by default, lacks a file of a component that
`--components` lists, as after a cache miss, replaces every file of those
components in it with the one from the artifacts of the latest `pull_request`
run of the workflow that `--workflow-ref`, a `GITHUB_WORKFLOW_REF` value,
names, for the pull request head: the second parent of the merge commit
`--commit`, or `--head-sha`. The run's head must be in `--repository` itself,
never a fork.
The command first downloads the run's `--record-artifact` artifact, which
holds the record that [`record-guest-artifacts`](#record-guest-artifacts)
wrote, and goes on only if the record gives exactly the cache keys of the
components' cache entries in `--keys`, a JSON object of keys by name. It then
downloads the artifacts in which CI hands over the components' files,
`guest-kernel`, `guest-alpine`, `guest-ubuntu`, `guest-ubuntu-distro`, or
`guest-azurelinux`, and installs the files only when no artifact has expired,
each matches the SHA-256 digest that GitHub recorded for it, every file
matches its SHA-256 in the record, and the kernel and initramfs provenance and
the Ubuntu and Azure Linux manifests match the files and this checkout's
sources. A failed installation puts back the files that it replaced. With
`--dry-run`, it checks the record and the artifacts in the same way, even when
the destination lacks no file, but installs nothing.

It reads the GitHub token from `GH_TOKEN` or `GITHUB_TOKEN` and gives up on
the artifacts after two minutes, which a timer enforces on Linux until the
installation starts. It never fails for a reason not to reuse, so the guest
build proceeds: it prints the reason, as a GitHub notice when reuse is not
possible, such as for other keys or an expired artifact, and as a warning for
a fault, such as a failed request or a mismatched file, sets the `reused` step
output to `true` or `false`, and adds the outcome to the job summary; see
[Guest artifact reuse](ci.md#guest-artifact-reuse).

### `classify-ci-changes`

```text
python3 scripts/nvx.py classify-ci-changes
    --event-name {pull_request,merge_group,push}
    --changed-files PATH
    [--null]
    [--summary PATH]
```

Reads one changed repository path per line by default, or NUL-delimited paths
with `--null`, and prints
the change detector's
`run-guest-artifacts`, `run-openvmm-binaries`, `run-openvmm-unit-tests`,
`run-openvmm-vmm-tests`, `run-microvm-tests`, and `run-benchmarks` outputs as
`name=true` or `name=false` lines. Documentation selects no suite. Pull
requests, merge groups, and pushes run each test suite only when one of its
own inputs changed. A pull request or a merge group runs the platform
benchmarks only when one of their inputs changed, but a push runs them for
every change but documentation, because it publishes the development release
and records the performance history. The guest artifacts and the OpenVMM
binaries are built exactly when a job that uses them runs. The summary file
receives a Markdown
explanation of each decision. CI runs the full matrix instead when the command
fails or omits an output; see
[Change classification](ci.md#change-classification).

### `check-required-ci`

```text
python3 scripts/nvx.py check-required-ci
    --event-name {pull_request,merge_group,push,schedule,workflow_dispatch}
    --same-repository {false,true}
    --run-guest-artifacts VALUE
    --run-openvmm-binaries VALUE
    --run-openvmm-unit-tests VALUE
    --run-openvmm-vmm-tests VALUE
    --run-microvm-tests VALUE
    --run-benchmarks VALUE
```

Validates the GitHub Actions result values supplied through the
`QUALITY_RESULT`, `CHANGES_RESULT`, and job-specific `*_RESULT` environment
variables. CI supplies the `true` or `false` suite flags that
`classify-ci-changes` reports. The command expects successful results for jobs
enabled by the event, repository, and suite flags, and `skipped` for jobs that
are not enabled; mismatches are reported as errors and cause a nonzero exit.
As in CI, a job that uses the guest artifacts or the OpenVMM binaries is
enabled only when the flags enable the jobs that build them too. A merge group
expects the jobs of a same-repository pull request, whatever
`--same-repository` says, but the rehearsal of the guest artifact reuse, which
its pull request's run made; see [Merge queue](ci.md#merge-queue).

### `report-nightly-ci`

```text
python3 scripts/nvx.py report-nightly-ci
    --repository OWNER/REPOSITORY
    --run-id ID
    [--run-attempt N]
    [--assignee LOGIN]...
    [--label NAME]...
    [--summary PATH]
    [--dry-run]
```

Reports a completed scheduled run of the CI workflow, by default its latest
attempt, in the repository's nightly failure report: a failing run opens or
updates the report, and a passing run closes it, as
[Nightly failure reports](ci.md#nightly-failure-reports) describes. The command
prints what it did. It reads a token from `GH_TOKEN` or `GITHUB_TOKEN`, which
must be able to read the run's jobs, logs, and check annotations, and to read
and write issues. A new report is assigned to every `--assignee` and names
them as its triage owners, and carries every `--label` that exists in the
repository; the command never creates a label. `--summary` appends the report
to a Markdown file, such as `$GITHUB_STEP_SUMMARY`. `--dry-run` prints the
changes that the command would make, with their Markdown, and makes none; it
accepts a run of any event, so that a past failure can preview its report,
and, for a public repository, needs a token only for the job logs.

### `cancel-superseded-merge-groups`

```text
python3 scripts/nvx.py cancel-superseded-merge-groups
    --repository OWNER/REPOSITORY
    --workflow-ref REF
    --run-id ID
    --ref REF
    [--summary PATH]
```

Cancels the unfinished runs, whether they run or wait, of the workflow that
`--workflow-ref`, a `GITHUB_WORKFLOW_REF` value, names, for the earlier merge
groups of the pull request of the merge group that `--ref` names, a
`refs/heads/gh-readonly-queue/BASE/pr-NUMBER-SHA` ref, as
[Superseded merge groups](ci.md#superseded-merge-groups) describes. It cancels
only the runs of the groups of the same base branch and pull request whose
branches GitHub has deleted or given another head, as when the pull request
returns to the queue on the same base, so that it never cancels the run of the
pull request's live group. It holds the run `--run-id`, its caller, to the same
test, but cancels it last: GitHub can start a replaced group's run late, after
the run of the group that replaced it looked. It reads a token from `GH_TOKEN`
or `GITHUB_TOKEN`, which must be able to read and cancel the repository's
workflow runs and read its branches, and prints each run that it cancelled,
that had completed meanwhile, or that it could not cancel, which doesn't stop
the others. `--summary` appends the same lines to a Markdown file, such as
`$GITHUB_STEP_SUMMARY`. A failed request warns, as a GitHub warning, but never
fails the command, which only frees runners; a missing token or a `--ref` that
names no merge group does.

### `check-static-elf`

```text
python3 scripts/nvx.py check-static-elf PATH [PATH ...]
```

Prints each executable's ELF type, size, and linkage, and fails unless every
one is a 64-bit little-endian ELF executable without a program interpreter
(`PT_INTERP`) or needed libraries (`DT_NEEDED`), so that it runs in a guest
without a dynamic loader. A static position-independent executable passes. It
also fails for an executable stack: a `PT_GNU_STACK` program header with
`PF_X`. The `nvx-rs` CI job checks the
[guest binaries](../nvx-rs/README.md#guest-binaries) with it.

### `check-guest-binaries`

```text
python3 scripts/nvx.py check-guest-binaries [--toolchain TOOLCHAIN]
```

The Linux-only check command adds the `x86_64-unknown-linux-musl` target to the
Rust toolchain that `--toolchain` names, by default the exact
`rust-toolchain.toml` pin. It then runs Clippy with warnings denied on every
target of the `guest` feature, and the feature's musl tests, whose static test
executables run on the host. It ignores an inherited `NVX_GUEST_BINARIES`, so
that the integration tests run the binaries that Cargo builds from the
checkout. The `nvx-rs` CI job runs it with the toolchain that its action
selects, before it builds the binaries.

### `build-guest-binaries`

```text
python3 scripts/nvx.py build-guest-binaries --output DIRECTORY
```

The Linux-only build command checks the exact `rust-toolchain.toml` pin and
builds the nine Rust guest executables for `x86_64-unknown-linux-musl` with one
`cargo build` of the `guest` profile, a fresh target directory, Cargo's locked
mode, incremental compilation disabled, inherited Cargo profile overrides
removed, remapped source paths, and no linker build IDs. It then fails unless
the source identity, which
[`guest-binary-source-identity`](#guest-binary-source-identity) prints and the
guest image cache keys end with, covers every file that Cargo's dep-info lists
for the executables: each source file that rustc read, including the targets of
`include_str!` and `include_bytes!`, and each path that the build script
watches. Files that the build generates in its own target directory are exempt.
It runs the static-ELF check and writes `manifest.json` with the source and
executable SHA-256 digests.
It updates the output of an earlier build in place, adding `nvx-managed-agent`,
`nvx-ingress`, `nvx-time`, and `nvx-time-probe` when that build's manifest does
not record them and removing the retired `nvx-port-io` executable when that
build's manifest records it, and refuses any other existing `DIRECTORY`.

### `test-guest-binaries`

```text
python3 scripts/nvx.py test-guest-binaries --binaries DIRECTORY
```

The Linux-only test command validates the nine binaries and build manifest,
sets `NVX_GUEST_BINARIES`, and runs the musl `guest_tools` integration test
against that exact directory.

See [Setup](setup.md) for host prerequisites.

### `materialize-kernel-provenance-inputs`

```console
python3 scripts/nvx.py materialize-kernel-provenance-inputs
```

Recreates the tracked kernel configuration and patch files from the current
Git run head. It removes stale worktree patches that are absent from that
revision and is used before packaging kernel provenance inputs.

## Build commands

### `build-guest`

```text
python3 scripts/nvx.py build-guest
    [--guest {alpine,ubuntu,azurelinux,all}]
    [--native]
    [--debug-kernel]
```

By default, builds the guest kernel and initramfs with Docker. `--native`
builds the selected artifacts directly on Linux instead. Alpine is the
default. Azure Linux does not support `--native` and always builds through
Docker. `--guest all` also builds the Ubuntu EROFS distro layer and the
sandbox scratch template; see [`build-scratch-template`](#build-scratch-template).
`--debug-kernel` also builds the CI debug kernel, `build/vmlinux-debug`; see
[Build](build.md#ci-debug-kernel).

### `build-kernel`

```console
python3 scripts/nvx.py build-kernel [--debug]
```

Fetches, verifies, patches, and builds the pinned kernel directly on Linux.
`--debug` builds the CI debug variant instead of the production kernel.

### `build-initramfs`

```console
python3 scripts/nvx.py build-initramfs [--guest {alpine,ubuntu,azurelinux}]
```

Builds the selected initramfs. Alpine and Ubuntu build directly on Linux; Azure
Linux uses Docker. Alpine is the default.

### `build-distro-layer`

```text
python3 scripts/nvx.py build-distro-layer
    --guest ubuntu
    [--output PATH]
    [--replace]
```

Builds the immutable Ubuntu EROFS `distro` layer directly on Linux. The default
output is `build/ubuntu-distro.erofs`. Existing output or manifest files are
rejected unless `--replace` is present.

### `build-scratch-template`

```text
python3 scripts/nvx.py build-scratch-template
    [--output PATH]
    [--replace]
```

Formats the sandbox scratch template directly on Linux, with `mkfs.ext4` from
e2fsprogs, which it finds on `PATH` or else in `/usr/sbin` or `/sbin`, where
Debian keeps it out of an unprivileged user's `PATH`: an empty 64 MiB ext4
file system, of which the sandbox scenarios boot a copy as their scratch. The
default output is `build/ubuntu-smoke-scratch.ext4`. An existing output is
rejected unless `--replace` is present. `UbuntuBuildConstants` in
[`build_constants.py`](../scripts/nvx_tools/build_constants.py) defines the
template once: its name, size, UUID, block size, inode size and count, and
features, which the command passes to `mkfs.ext4` rather than leave them to
`mke2fs.conf` and the defaults that distributions patch. The command also
fixes the clock of `mkfs.ext4` with `E2FSPROGS_FAKE_TIME` and its directory
hash seed, and ignores the caller's other `E2FSPROGS_*` and `MKE2FS_*`
variables, so e2fsprogs 1.46.5 and 1.47.0 of Debian 12, Ubuntu 22.04 and
24.04, and Azure Linux 3 format the same bytes, whose SHA-256
`UbuntuBuildConstants.SCRATCH_TEMPLATE_SHA256` pins. It warns when
`mkfs.ext4` formats other bytes. The guest Docker build formats the template
with the Ubuntu distro layer, in the pinned Debian image, for
[`build-guest`](#build-guest) and for CI, which caches, records, and hands it
over with the layer. On other systems than Linux, the command fails and names
that build of every guest and [`download`](#download), which installs the
template.

### `verify-guest-determinism`

```text
python3 scripts/nvx.py verify-guest-determinism
    --guest ubuntu
    [--work-dir PATH]
```

Currently supports only `--guest ubuntu`. It builds the Ubuntu initramfs,
EROFS layer, and sandbox scratch template twice from separate roots and
compares every artifact SHA-256, and fails unless the template's is the one
that `UbuntuBuildConstants.SCRATCH_TEMPLATE_SHA256` pins. A mismatch reports
the first differing normalized rootfs entry when one exists.

### `build-openvmm`

```text
python3 scripts/nvx.py build-openvmm [--skip-restore] [--backend {kvm,mshv,whp}]
```

Builds the `openvmm` release binary. Before building, the command runs
`cargo xflowey restore-packages`; use `--skip-restore` when those packages are
already restored. Without `--backend`, Windows builds the native MSVC target
and Linux builds the native GNU target. On Linux, `--backend kvm` selects GNU
and `--backend mshv` selects musl; Windows accepts `--backend whp`. Build-target
selection does not probe `/dev/kvm` or `/dev/mshv`, so compilation also works
on build-only hosts and hosts exposing both devices. Unsupported OS/backend
combinations are rejected.

### `record-openvmm-provenance`

```console
python3 scripts/nvx.py record-openvmm-provenance
```

Records the checked-out OpenVMM revision, clean-state flag, and executable
SHA-256 for the existing `openvmm/target/release/openvmm[.exe]` binary in
`build/openvmm.provenance.json`. The command takes no options. It requires
the OpenVMM submodule to be checked out/initialized and the release binary to
already exist at `openvmm/target/release/openvmm` on POSIX or
`openvmm/target/release/openvmm.exe` on Windows. The command fails if either
prerequisite is missing.

### `record-nvx-rs-provenance`

```text
python3 scripts/nvx.py record-nvx-rs-provenance [--target TARGET]
```

| Option | Default | Description |
| --- | --- | --- |
| `--target TARGET` | the host target that `rustc -vV` reports | Rust target that built `nvx-rs`. |

Records `build/nvx-rs.provenance.json` for the `nvx-rs` that the checkout
runs without building it, the one that `NVX_RS` names or else
`nvx-rs/target/release/nvx-rs[.exe]`; see [nvx-rs](#nvx-rs). The record binds
the executable's SHA-256 to the digest of this checkout's `nvx-rs` sources,
the Rust release that `rust-toolchain.toml` pins, the target, and the `cli`
feature, so record it right after building `nvx-rs` from this checkout. For
`x86_64-unknown-linux-musl`, the target of the Linux release packages, the
executable must be static. CI's NVX CLI jobs record the builds that they hand
to the packages, and `nvx.py` records the builds that it makes itself.

### `build`

```text
python3 scripts/nvx.py build
    [--guest {alpine,ubuntu,azurelinux,all}]
    [--native]
    [--debug-kernel]
    [--skip-restore]
    [--backend {kvm,mshv,whp}]
```

Runs `build-guest` followed by `build-openvmm`. The options have the same
meaning as on those individual commands.

See [Build](build.md) for dependencies, outputs, and native build details.

## Test commands

### `test-openvmm-unit`

```console
python3 scripts/nvx.py test-openvmm-unit
```

Runs the OpenVMM workspace's unit-test binaries with cargo-nextest's `agent`
profile and the `ci` feature. Packages that require specialized test harnesses
are excluded, along with all fuzz crates reported by OpenVMM's `xtask`.
Afterward, runs the workspace doctests with Cargo. On Windows, it appends
`/Brepro` to `_LINK_`, so that MSVC links proc macros reproducibly and sccache
can reuse the crates that use them; see
[Compilation cache](ci.md#compilation-cache).

### `build-openvmm-tests`

```text
python3 scripts/nvx.py build-openvmm-tests
    --backend {kvm,mshv,whp} [--backend {kvm,mshv,whp} ...]
    --output PATH
```

Builds the VMM tests that [`test-openvmm`](#test-openvmm) runs for each
`--backend`, without running them, into the new or empty directory that
`--output` names, so that another host can run them without building OpenVMM.
The backends must run on this host's operating system: Linux builds the KVM
and MSHV tests, and Windows the WHP tests. The command runs OpenVMM's
`vmm-tests-run` in build-only mode on the union of the backends' filters, for
the `linux-x64` or `windows-x64` target, since NVX supports only x86-64 hosts,
as a release build without optional features whose run script uses the
nextest CI profile, which puts the tests' nextest archive, OpenVMM, the guest
agents, the test firmware and kernels, and a `vmm-tests-run-target` runner into
the directory. It then reads the runner's options from the run script that the
build writes, and records them in the directory's `nvx-openvmm-tests.json`
with the OpenVMM revision and whether its checkout was clean, the Rust
release, the target, the `vmm-tests-run` build options, each backend's filter,
and every file's size, SHA-256, and executable bit. The tests boot NVX's kernel
and initramfs only when they run, so the build needs no guest artifacts. On
Windows, it appends `/Brepro` to `_LINK_`, as `test-openvmm` does.

### `openvmm-test-cache-key`

```text
python3 scripts/nvx.py openvmm-test-cache-key
    --backend {kvm,mshv,whp} [--backend {kvm,mshv,whp} ...]
```

Prints the key under which CI caches the test content that
[`build-openvmm-tests`](#build-openvmm-tests) builds for the backends from this
checkout (see [OpenVMM VMM test cache](ci.md#openvmm-vmm-test-cache)):
`openvmm-vmm-tests-vVERSION-TARGET-REVISION-DIGEST`. VERSION is the
cache's own version, which changes when what a build makes changes in a way
that nothing else in the key captures. TARGET is the build's target, REVISION
the OpenVMM revision that the checkout's index pins, and DIGEST the SHA-256 of
the fields of the manifest of a clean build of that revision that describe
what built it: the manifest's format, the Rust release, the target, the build
options, and each backend's filter and their union. The backends must run on
this host's operating system, as for `build-openvmm-tests`, and the command
needs neither the OpenVMM submodule's files nor Cargo. In GitHub Actions, it
also gives its step the `key` output.

### `check-openvmm-tests`

```text
python3 scripts/nvx.py check-openvmm-tests
    --backend {kvm,mshv,whp} [--backend {kvm,mshv,whp} ...]
    --content PATH
```

Fails unless the directory that `--content` names holds exactly the test
content of the backends' cache key: a manifest that records every field that
[`openvmm-test-cache-key`](#openvmm-test-cache-key) digests, for a clean
OpenVMM checkout, and the runner and options that the content's run script
runs, and exactly the files that the manifest lists, with their sizes,
SHA-256, and executable bits. CI checks each build this way before it caches
it.

### `reuse-openvmm-tests`

```text
python3 scripts/nvx.py reuse-openvmm-tests
    --backend {kvm,mshv,whp} [--backend {kvm,mshv,whp} ...]
    --content PATH
```

Keeps the directory that `--content` names, into which a cache restored test
content, if it passes the checks of
[`check-openvmm-tests`](#check-openvmm-tests), and otherwise removes it with a
warning that gives the reason, so that a build can take its place. A
directory that doesn't exist, as after a cache miss, needs no removal. The
command fails only when it cannot compute the key or remove the directory,
never because of the content. In GitHub Actions, it gives its step the
`reused` output, `true` or `false`.

### `test-openvmm`

```text
python3 scripts/nvx.py test-openvmm
    --backend {kvm,mshv,whp}
    [--content PATH]
    [--shard INDEX/COUNT]
```

Builds and runs OpenVMM's checkout-owned VMM tests. The Linux-direct microVM
TTRPC test boots NVX's `build/vmlinux` and `build/initramfs.cpio.gz`, so build
the guest first; the remaining test artifacts are produced by OpenVMM itself.
On Windows, it appends `/Brepro` to `_LINK_`, as `test-openvmm-unit` does.

`--shard INDEX/COUNT`, such as `1/2`, runs shard INDEX of the COUNT shards of
the backend's tests, as
[`openvmm_test_shards.json`](../scripts/nvx_tools/openvmm_test_shards.json)
splits them: the tests of the backend's filter that the shard lists, which the
command runs with the backend's filter and the shard's tests both as its
filter. CI runs each shard of the WHP tests in a job of its own (see
[OpenVMM VMM test shards](ci.md#openvmm-vmm-test-shards)). Only the WHP filter
names each of its tests, so only the WHP tests have shards. The command fails
before it runs a test unless that file puts each test of every sharded filter
in exactly one shard, with its duration, and splits the backend's tests into
COUNT shards.

With `--content`, the command builds nothing: it runs the backend's tests from
a directory that [`build-openvmm-tests`](#build-openvmm-tests) built for that
backend, so the host needs neither the OpenVMM checkout nor its Rust targets.
It refuses a directory built from another OpenVMM revision than the checkout
pins or for another filter than the backend's, and one that lacks a file of
the build or holds a truncated one, and it restores the executable bits that
an artifact download drops. It still needs the guest artifacts above, and
Petri writes the test results to `test_results` in the directory, where the
command also writes the [test duration report](#test-duration-report). On
Linux, it runs the tests with the directory's `vmm-tests-run-target` runner and
the options that its manifest records.

On Windows, the pinned OpenVMM's runner checks for Hyper-V whatever tests it
runs: the account must be able to run `Get-VM`, and the
`AllowFirmwareLoadFromFile` and `EnableAdditionalComPorts` values must be set
under `HKLM\Software\Microsoft\Windows NT\CurrentVersion\Virtualization`. No WHP
test needs Hyper-V, so the command takes the runner's remaining steps itself.
It downloads the images that the recorded options name into `images` in the
directory, or into the directory that `VMM_TEST_IMAGES` names, unless that
already holds an image at the size that OpenVMM declares. It then sets the
environment that Petri reads and runs the backend's filter, or the shard's,
from the directory's nextest archive with `cargo-nextest`, which must be on
`PATH`, the directory's nextest configuration, the profile that the options
select, and the directory as the workspace. It refuses the options that only
the runner carries out, such as prep steps, the test IGVM agent, hardware
isolation, and repeated runs, and an image that it cannot download.

#### Test duration report

A run of a test content directory records how long each test that nextest ran
took in `openvmm-test-durations.json` in the directory's `test_results`, as
the run ends, whether its tests passed or not, and [CI](ci.md) uploads the
reports of its VMM-test jobs. The command reads the durations from the JUnit
report that OpenVMM's nextest configuration writes for the `ci` profile, which
the content's runs use, under the workspace that a run remaps to the
directory: `target/nextest/ci/junit.xml`. It removes both reports of an
earlier run before it runs the tests, so that a run whose nextest writes no
JUnit report leaves no test duration report either. A run that is
interrupted, as when CI cancels its job, stops nextest before it writes the
JUnit report, which it writes only as its run ends, and so usually leaves no
test duration report. A report that it cannot write only warns, so that it
never hides the run's own outcome. The report is a JSON object:

| Field | Value |
| --- | --- |
| `schema`, `schema_version` | `nvx-openvmm-test-durations` and `1`. The version changes only when a reader of the previous version would misread a report, not when a field is added. |
| `backend` | The `--backend` |
| `shard` | The `--shard`, as `{"index": INDEX, "count": COUNT}`, or `null` for a run of all of the backend's tests |
| `filter` | The nextest filter that the run ran |
| `logical_processors` | The host's logical processors, one for each of nextest's default test threads, or `null` if the OS doesn't tell |
| `github` | In GitHub Actions, the job's `repository`, `workflow`, `run_id`, `run_attempt`, `job`, `event_name`, `ref`, `sha`, `runner_name`, and `runner_os`, as strings from the `GITHUB_` and `RUNNER_` variables of those names; otherwise `null` |
| `tests` | Each test case of the JUnit report, in its order: its `binary` and `test` names, the `seconds` that it ran, to the millisecond, and its `outcome`: `passed`, `failed`, `flaky` for a test that passed only when nextest retried it, or `skipped` |

### `plan-openvmm-test-shards`

```text
python3 scripts/nvx.py plan-openvmm-test-shards
    [--report PATH]...
    [--seconds TEST=SECONDS]...
    [--shards BACKEND=COUNT]...
    [--test-threads BACKEND=COUNT]...
    [--partition PATH]
```

Plans the shards of the OpenVMM VMM tests, which CI runs in parallel (see
[OpenVMM VMM test shards](ci.md#openvmm-vmm-test-shards)), and writes them to
[`openvmm_test_shards.json`](../scripts/nvx_tools/openvmm_test_shards.json),
or to `--partition`. The file holds, for each sharded backend, the test
threads with which nextest runs its tests, the median duration of each of the
tests that its filter selects, the CI runs whose reports gave them, and each
shard's tests with the time that nextest takes to run them. Only a backend
whose filter names each of its tests, which only the WHP filter does, can have
shards.

`--report` reads [test duration reports](#test-duration-report), or every
report in a directory, such as the one that `gh run download` fills with a
run's `openvmm-vmm-test-durations-*` artifacts. Each backend's duration of a
test is the median, to a tenth of a second, of the reports' times of the test
when it passed, in runs of a shard or of all of the backend's tests. A test
that no report measures keeps its duration in the file. `--seconds` sets a
test's duration on every backend instead, such as an estimate of a new test
that no report measures yet; the command fails when a test of a sharded filter
has no duration.

nextest runs a shard's tests in parallel, so a shard does not take the sum of
their durations. The command models how nextest runs them: in name order, each
as soon as it has the test threads that it requires, and never one before an
earlier one. A test requires the threads that OpenVMM's nextest configuration
gives it, 2, or 12 for a `heavy` test and 32 for a `very_heavy` one, and
nextest gives a test that requires more than it runs all of its threads, so
that the test runs alone and the tests after it wait for it. A backend's test
threads are those that the file records, which `--test-threads` changes, one
for each logical processor of its runners; the command notes the reports whose
runners have another number.

Each backend keeps its number of shards unless `--shards` changes it, and a
new backend needs both `--shards` and `--test-threads`. Each test starts in its
shard of the file, and a test that has none goes, longest first, to the shard
that nextest takes the least time to run so far. A shard that is still empty,
as one that `--shards` adds can be, takes the longest test of the longest
shard that has more than one. The command then moves a test to another shard,
or swaps two tests of different shards, while that shortens the longest shard
by at least a second, comparing the next longest where the longest stay alike,
and takes the best such change each time; no change empties a shard. It prints
each backend's shards with the times that nextest takes to run them and all of
its tests, and the tests that changed shard.

### `test-microvm`

```text
python3 scripts/nvx.py test-microvm
    --backend {kvm,mshv,whp}
    [--guest {alpine,ubuntu,azurelinux}]
    [--scenario SCENARIO]...
    [--debug-kernel]
    [--shard INDEX/COUNT]
    [--processors {1,2,4,8} ...]
    [--memory-mib MIB]
    [--timeout SECONDS]
    [--output-dir PATH]
```

Runs NVX-owned Linux, SMP, virtio, sandbox, and snapshot correctness scenarios
against the public OpenVMM CLI. Repeat `--scenario` to select a subset; without
it, every scenario supported by the selected guest runs, except `smp-lapic`,
which runs only when named: it repeats `smp` and also asserts that every CPU
uses the one-shot counting LAPIC. `managed-exec-config`, `sandbox-lifecycle`,
`sandbox-proxy-only`, and `sandbox-ram-upper`, the public sandbox scenarios,
drive the public `nvx-rs sandbox` lifecycle commands (see
[Run](run.md#managed-lifecycle)) and also read the Ubuntu distro layer, its
manifest, and the sandbox scratch template, which
[`download`](#download) installs and the guest-artifact build makes; a run
that selects one of them fails before its first scenario when one of those
files is missing. Alpine remains the
default. Ubuntu and Azure Linux cannot act as sandbox control, so they reject
the Alpine-control-only `sandbox-blocks` and `scratch-snapshot` scenarios and
the sandbox-control-dependent `snapshot-tiers`, `platform-restore-processors`,
`platform-zero-fill`, `platform-filesystem`, `platform-aggregate-export`,
`platform-network-policy`, `managed-exec-config`, `sandbox-lifecycle`,
`sandbox-proxy-only`, and `sandbox-ram-upper` scenarios. Ubuntu also rejects
the Alpine-prompt-specific `console-snapshot` scenario. `--debug-kernel` boots
the CI debug kernel, `build/vmlinux-debug`, whose soft-lockup and hung-task
detectors the guest's time ABI watcher reports; without `--scenario`, it runs
only the same-host restore scenarios `smp`, `smp-snapshot`,
`restore-processors`, `restore-downtime`, and `snapshot-tiers`.
`--shard INDEX/COUNT`, such as `1/2`, runs shard INDEX of the COUNT shards of
the default Alpine suite on the default kernel, the scenarios of a run without
`--scenario` but the public sandbox scenarios, which CI runs in its guest and
sandbox jobs, as
[`microvm_shards.json`](../scripts/nvx_tools/microvm_shards.json) splits it for
the backend; CI runs each shard in a job of its own (see
[MicroVM test shards](ci.md#microvm-test-shards)). It takes no `--scenario`,
`--debug-kernel`, or other `--guest`, and fails before it runs a scenario
unless that file puts each default scenario in exactly one shard of every
backend and splits the backend's suite into COUNT shards. The
`cpu-profile-fallback` scenario cold boots with OpenVMM's `host-cpu-unknown`
test hook, which makes OpenVMM's default CPU profile selection, `auto`, fall
back to a host profile on a host that a built-in profile serves, checks the
fallback's warning, captures the guest, and restores it on the snapshot's host
profile. The `managed-lifecycle` scenario boots a guest whose init hands
the boot to the managed agent, and runs the ignored `nvx-rs` test
`tests/managed_lifecycle.rs` with Cargo, which starts OpenVMM with the
scenario's command line and drives the guest's control session; the scenario
then checks OpenVMM's outcome report and the guest console. The scenarios that
run microVMs or sandboxes as the public commands do, or that compile egress
policies, run [`nvx-rs`](#nvx-rs). The
command requires `build/vmlinux` (with `--debug-kernel`, `build/vmlinux-debug`
and its `build/vmlinux-debug.config`), the selected initramfs, and
`openvmm/target/release/openvmm[.exe]`.

#### Scenario duration report

`test-microvm` prints each scenario's progress line as the scenario starts,
so that a log's timestamps follow the scenarios. It also records how long each
scenario takes in `scenario-durations.jsonl` in `--output-dir`, replacing the
previous run's report, and [CI](ci.md) uploads the reports of its microVM
jobs. The report is JSON Lines: one JSON object per line, whose `record` field
names its kind. The first line, `run`, describes the run:

| Field | Value |
| --- | --- |
| `schema`, `schema_version` | `nvx-microvm-durations` and `1`. The version changes only when a reader of the previous version would misread a report, not when a field is added. |
| `started_at` | When the run and its `setup` started, in UTC, such as `2026-10-21T13:50:00.123Z` |
| `guest`, `backend` | The `--guest` and the `--backend` |
| `kernel` | `debug` with `--debug-kernel`, otherwise `default` |
| `processors` | The `--processors` counts, without repeats |
| `scenarios` | The selected scenarios |
| `github` | In GitHub Actions, the job's `repository`, `workflow`, `run_id`, `run_attempt`, `job`, `event_name`, `ref`, `sha`, `runner_name`, and `runner_os`, as strings from the `GITHUB_` and `RUNNER_` variables of those names; otherwise `null` |

Each later line records a span of the run as soon as the span ends, so that a
run that fails or is interrupted keeps every span that it completed. The spans
follow one another without gaps, so they add up to the run's time:

- `setup`, from the start of the Python process, or from the report's creation
  where the OS doesn't report that start, until the first scenario starts;
- a `scenario` for each scenario execution, until the next one starts.
  `console-exit`, `smp`, and `smp-lapic` execute once per processor count;
- `teardown`, which sums up the time ABI evidence and records the run's
  outcome.

Every span has its `started_at`, its `seconds`, rounded to the millisecond, its
`outcome`, `passed`, `failed`, or `interrupted`, for example by `SIGINT` or
`SIGTERM`, and
an `error`: for a failure, the exception's type and message, cut to 1,000
characters, and otherwise `null`. A `scenario` record also has the
`scenario`, the `guest`, `backend`, and `kernel`, and its `processors`: the
`--processors` counts that the execution covers, such as `[4]` for `smp` on 4
vCPUs, the largest count for `time-abi-conformance`, all of them for
`smp-snapshot` and `restore-processors`, those up to 4 for
`platform-restore-processors`, or `null` for a scenario that doesn't
take them. A failed run ends with the failed scenario's record and the
`teardown`; a run that is killed keeps every record that it completed and has
no `teardown`.

### `plan-microvm-shards`

```text
python3 scripts/nvx.py plan-microvm-shards
    [--report PATH]...
    [--seconds SCENARIO=SECONDS]...
    [--shards BACKEND=COUNT]...
    [--partition PATH]
```

Plans the shards of the default Alpine suite on the default kernel, which CI
runs in parallel (see [MicroVM test shards](ci.md#microvm-test-shards)), and
writes them to [`microvm_shards.json`](../scripts/nvx_tools/microvm_shards.json),
or to `--partition`. The file holds each backend's median duration of every
default scenario, the CI runs whose reports gave them, and each shard's
scenarios with their total duration.

`--report` reads [scenario duration reports](#scenario-duration-report), or
every report in a directory, such as the one that `gh run download` fills with
a run's `nvx-microvm-durations-*` artifacts. Each backend's duration of a
scenario is the median, to a tenth of a second, of the reports of the default
suite, sharded or not: those of the Alpine guest on the default kernel with
the default processor counts, of runs that ended, passed or not. A report
counts for a scenario only when every execution of the scenario passed, such
as the four of `console-exit`. A scenario that no report measures keeps its
duration in the file. `--seconds` sets a scenario's duration on every backend
instead, such as an estimate of a new scenario that no report measures yet;
the command fails when a default scenario has no duration.

Each backend keeps its number of shards unless `--shards` changes it. Each
scenario starts in its shard of the file, and a scenario that has none goes,
longest first, to the shard that takes the least time so far. A shard that is
still empty, as one that `--shards` adds can be, takes the longest scenario of
the longest shard that has more than one. The command then moves a scenario
to another shard, or swaps two scenarios of different shards, while that
shortens the longest shard by at least a second, comparing the next longest
where the longest stay alike, and takes the best such change each time. It
prints each backend's shards with their durations, and the scenarios that
changed shard.

### `doctor`

```text
python3 scripts/nvx.py doctor
    --backend {kvm,mshv,whp}
    [--checks ID ...]
    [--openvmm PATH]
    [--kernel PATH]
    [--initrd PATH]
    [--cpu-fingerprint PATH | --no-openvmm]
    [--ci-schedule]
    [--probe-dir PATH]
    [--summary PATH]
    [--timeout SECONDS]
```

Qualifies this host for the [time ABI](design/3.2-clock-and-time.md).
It runs the selected checks in order, prints one
`NVX-DOCTOR: check=<id> status=<pass|fail> detail="..."` line per check, and
exits with status 1 if any check fails. A check that passes with a warning
starts its detail with `warning:`. A check fails, and never passes, when
it can't read a fact it gates on. [CI](ci.md#host-qualification) describes each
check and the subset that CI runs.

| Option | Default | Description |
| --- | --- | --- |
| `--backend {kvm,mshv,whp}` | required | Select the backend to qualify. |
| `--checks ID ...` | `H1` to `H7` | Run only these checks, from `H1` to `H7`, in order. |
| `--openvmm PATH` | `openvmm/target/release/openvmm[.exe]` | Select the OpenVMM binary for `H2`, `H3`, and `H6`. |
| `--kernel PATH` | `build/vmlinux` | Select the guest kernel for `H3` and `H6`. |
| `--initrd PATH` | `build/initramfs.cpio.gz` | Select the guest initramfs for `H3` and `H6`. |
| `--cpu-fingerprint PATH` | `nvx-cpu-fingerprint-<backend>.json` in the probe directory | Select where `H2` writes OpenVMM's CPU fingerprint. |
| `--no-openvmm` | off | Qualify without an OpenVMM binary. `H2` then checks the CPU identity and generation but not the CPU profile, and `--checks` must exclude `H3` and `H6`, which boot OpenVMM. |
| `--ci-schedule` | off | Run `H4` and `H6` on CI's short schedules: 3 rate samples 1 s apart instead of 13 samples 10 s apart, and two warp-probe runs instead of five. |
| `--probe-dir PATH` | `$RUNNER_TOOL_CACHE/nvx-host-time-probe`, or `build/host-time-probe` | Select the cache directory for the host probe, which the doctor builds with `rustc`. |
| `--summary PATH` | none | Append a Markdown summary, for example to `$GITHUB_STEP_SUMMARY`. |
| `--timeout SECONDS` | `120` | Set the seconds allowed for each probe or guest. `H4` gets its sampling window on top. |

### `test-runtime-adapter`

```text
python3 scripts/nvx.py test-runtime-adapter
    --backend {kvm,mshv,whp}
    [--guest-agent PATH]
    [--check-only]
    [--cargo CARGO]
```

Runs the consolidated suite of the [runtime adapter](../tests/runtime-adapter/README.md),
the [stage scenarios](../tests/runtime-adapter/README.md#stage-scenarios) that
`tests/runtime-adapter/scenarios.txt` names, against a real hypervisor. Each
line of that file names one of the adapter's test targets, then one of its
tests. The command first checks that the lines name exactly the tests of their
targets, and that neither the adapter's library nor another target has an
ignored test, as the adapter's VM scenarios are, so that a scenario that is
renamed, added, or gone fails it rather than drop out of the suite. It then
runs each target's scenarios by
name, in the adapter's host-only configuration, the ignored ones with the
others, with the backend in `NVX_HYPERVISOR`, NVX's artifacts from the
directory that `NVX_ARTIFACTS_DIR` names, from the current directory if it is
relative, or this checkout unless it names one, and the adapter's guest agent
from `--guest-agent` (default
`tests/runtime-adapter/target/x86_64-unknown-linux-musl/release/runtime-adapter-guest`),
and fails unless each of them ran and passed. `--check-only` checks the names
alone, and needs neither a hypervisor nor the guest agent.

### `test-adversarial`

```text
python3 scripts/nvx.py test-adversarial
    --backend {kvm,mshv,whp}
    --campaign {workload-isolation,guest-isolation,snapshot-isolation}
    [--budget-seconds SECONDS]
    [--budget-actions COUNT]
    [--budget-ai-credits CREDITS]
    [--seed SEED]
    [--output-dir PATH]
    [--replay ACTIONS.jsonl]
    [--model MODEL]
    [--host-type {baremetal,virtual-machine}]
    [--memory-mib MIB]
    [--phase-timeout SECONDS]
    [--action-timeout SECONDS]
    [--executor-command PATH]
    [--no-minimize]
    [--minimize-attempts COUNT]
```

Runs a bounded adaptive campaign in which an already installed and
authenticated Copilot CLI selects one deterministic primitive at a time.
Copilot has no tools or direct NVX access. The typed broker validates and
records every action, while a credential-free executor and independent
watchdog own VM operation, canaries, teardown checks, and the clean
post-campaign boot.

| Option | Default | Description |
| --- | --- | --- |
| `--backend` | required | Select KVM or MSHV on Linux, or WHP on Windows. |
| `--campaign` | required | Select workload, privileged-guest, or snapshot boundary probes. |
| `--budget-seconds` | `900` | Bound preflight, actions, canary boot, and minimization wall time. |
| `--budget-actions` | `8` | Bound accepted and executed broker actions. |
| `--budget-ai-credits` | `300` | Bound charged Copilot usage; the minimum is 60 credits so authentication preflight and at least one action each retain a 30-credit CLI cap. |
| `--seed` | `0` | Seed deterministic candidate ordering and synthetic canary data. |
| `--output-dir` | backend-specific path under `build/test-results` | Parent for a unique campaign run directory. |
| `--replay` | none | Replay an `actions.jsonl` bound to its sibling `replay-manifest.json`, without invoking Copilot. |
| `--model` | Copilot auto routing | Select the strategist model. |
| `--host-type` | `NVX_HOST_TYPE` or `unspecified` | Record whether the executor is bare metal or a virtual machine. |
| `--memory-mib` | `256` | Set memory for deterministic microVM scenarios. |
| `--phase-timeout` | `60` | Set each underlying deterministic scenario phase timeout. |
| `--action-timeout` | `600` | Set the outer limit for one action or canary boot. |
| `--executor-command` | local child executor | Select one trusted no-argument wrapper for a separate disposable target. |
| `--no-minimize` | off | Disable fresh-target shorter-prefix replay after an anomaly. |
| `--minimize-attempts` | `3` | Bound shorter-prefix attempts within the campaign time budget. |

Missing or unauthenticated Copilot CLI is a preflight failure in adaptive
mode. Replay mode has no Copilot prerequisite. Production and CI campaigns
must use a separate disposable executor through `--executor-command`; local
mode cannot reliably classify a target-host crash. Local target logs are kept
under the short `build/adv` state root to avoid Windows path-length failures;
the run summary records their absolute location. See
[Copilot-driven adversarial testing](adversarial-testing.md)
for the trust boundary, wrapper contract, artifacts, and CI policy.

## CPU profiles

Every microVM presents a CPU profile to its guest: the complete CPUID surface
of one CPU generation, which OpenVMM pins and every backend shares (see the
[CPU profile design](design/2.2-cpu-and-topology.md#221-cpu-profiles)). By default, OpenVMM selects the
latest revision of the built-in profile of the host's CPU generation:

| Profile | Generation | CPUs (display family/model) |
| --- | --- | --- |
| `intel.skylake-sp.v1` | `skylake-sp` | Intel Xeon Scalable, first generation (6/85, steppings 0 to 4) |
| `intel.icelake-sp.v1` | `icelake-sp` | Intel Xeon Scalable, third generation (6/106) |
| `intel.emeraldrapids.v1` | `emeraldrapids` | Intel Xeon Scalable, fifth generation (6/207) |
| `intel.alderlake.v1` | `alderlake` | Intel Core, twelfth generation (6/151 and 6/154) |
| `amd.milan.v1` | `milan` | AMD EPYC, third generation (25/1, Milan-X included) |
| `amd.genoa.v2` | `genoa` | AMD EPYC, fourth generation (25/17, Genoa-X included) |
| `amd.turin.v1` | `turin` | AMD EPYC, fifth generation (26/2) |

OpenVMM keeps every earlier revision pinned, so that its snapshots still
restore, and `--cpu-profile` still selects it by its ID: `amd.genoa.v1` on
Genoa.

Any other CPU, including Cascade Lake, Sapphire Rapids, Granite Rapids, Tiger
Lake, Raptor Lake, and AMD's Ryzen CPUs, has no built-in profile: on an Intel
or AMD CPU, `--cpu-profile auto` falls back to a host profile, described
below, and on another vendor's CPU, it fails with `E_PROFILE_HOST_UNKNOWN`.
A host of a listed generation can still fail with `E_PROFILE_UNSUPPORTED` if
its SKU or hypervisor lacks a feature of the profile, and on an Intel or AMD
CPU OpenVMM's error then names `--cpu-profile host`. `intel.alderlake.v1`
derives from one Core i9-12900H on WHP, `amd.milan.v1` from one EPYC 7763
Azure VM on WHP, `amd.genoa.v1` and `amd.turin.v1` from KVM on one
GitHub-hosted Actions runner each, an Azure VM with an EPYC 9V74 or 9V45, and
`amd.genoa.v2` from that EPYC 9V74 runner's KVM and the nested MSHV of an
Azure VM with the same CPU. GitHub's runners with an EPYC 7763, 9V74, or 9V45
pass their profiles' full `test-microvm` suite on KVM, the EPYC 9V74 runner
on `amd.genoa.v1`, of which `amd.genoa.v2` presents a subset. Alder Lake
hosts on KVM and MSHV, Milan, Genoa, and Turin hosts on MSHV, Genoa and Turin
hosts on WHP, bare-metal AMD hosts, and SKUs without the profiles' features
are unverified. The AMD profiles leave
out what KVM cannot present, `BTC_NO` and PSFD without a `SPEC_CTRL` control,
and the enumerations of Intel's speculation controls that KVM adds on AMD
hosts. They present none of AMD's speculation controls, `IBRS`, `STIBP`, and
`SSBD`, because the Azure VMs' hypervisors withhold them; `amd.milan.v1`'s
Linux guests therefore use retpolines and report SSB, SRSO, and TSA as
vulnerable on every host. `amd.genoa.v1` pins what Azure presents to KVM on
Genoa: the basic leaves up to `0x1c`, and `LFENCE` serialization,
NoSmmCtlMsr, and the TSA immunities in `0x80000021`. The Microsoft hypervisor
of the Azure Genoa VMs fingerprinted so far, under MSHV and WHP, presents none
of them, and a bare-metal Genoa host's KVM does not report the TSA
immunities. `amd.genoa.v2` leaves them out, so its Linux guests report TSA as
vulnerable, as `amd.milan.v1`'s do.

On an Intel or AMD development host that no built-in profile serves,
`--cpu-profile auto` falls back to a host profile, `intel.host.v1` or
`amd.host.v1`, which `nvx-rs run --cpu-profile host` also selects, for
example where the hypervisor does not support the built-in profile. OpenVMM
fingerprints the
hypervisor on this host and applies the built-in profiles' derivation policy
to it, so the guest sees the same kind of filtered CPU surface, and verifies
it as it verifies a built-in profile: a cold boot still fails with
`E_PROFILE_UNSUPPORTED` if the hypervisor lacks a CPU feature that the time
ABI requires. Where `auto` fell back, the error also says so; with an explicit
`--cpu-profile host`, it reports the failure alone. A host profile is for
development only:

- It is not pinned: a microcode, firmware, hypervisor, or OS update can change
  it.
- Each cold boot fingerprints the hypervisor first, which
  [#394](https://github.com/microsoft/nvx/pull/394) measured at 2.6 to 5.5 ms
  on WHP, 11 to 23 ms on KVM, and 52 ms on MSHV. The cost varies with the
  host, and the fallback's warning reports what each cold boot's fingerprint
  took.
- A snapshot records its host profile, and restores only on a host with the
  same CPU model and stepping whose hypervisor supports the profile.
- `doctor` never qualifies it, so CI hosts need a built-in profile, and
  `benchmark` refuses a CPU that no built-in profile serves.

Every cold boot that falls back logs one OpenVMM warning, at the default log
level, that starts with `NVX-CPU-PROFILE-FALLBACK:` and names the CPU, the
host profile with its digest, and these limits, with what that cold boot's
fingerprint took. It also asks you to help add
a built-in profile for the CPU: run
`openvmm --hypervisor <backend> --cpu-fingerprint fingerprint.json`, which
reports `E_PROFILE_HOST_UNKNOWN` on such a CPU but still writes the
fingerprint, and attach `fingerprint.json` to a
[CPU profile request](https://github.com/microsoft/nvx/issues/new?template=cpu-profile.yml),
whose link in the warning prefills the CPU. The fingerprint holds the CPU's
identity and microcode, the OS kind and build, the hypervisor's interface, and
the CPUID, XSAVE, MSR, and time surface that the backend supports; it holds no
hostname, user name, or serial number. Fingerprints of the generation from
other backends help too, because a profile is the intersection of its
backends' fingerprints and released profiles are immutable. An explicit
`--cpu-profile host` warns too, with the same request where no built-in
profile serves the CPU.

[#408](https://github.com/microsoft/nvx/issues/408) tracks built-in profiles
for more Intel CPUs, such as Tiger Lake and Meteor Lake, and
[#409](https://github.com/microsoft/nvx/issues/409) for more AMD CPUs, and
maintainers link each CPU profile request to one of them; host profiles serve
no CPU of another vendor than Intel and AMD. A profile derives from
fingerprints of its generation's hosts on every backend that it serves; see
`vmm_core/cpu_profile` in the OpenVMM submodule.

## Benchmarking

### `benchmark`

```text
python3 scripts/nvx.py benchmark [OPTIONS]
```

| Option | Default | Description |
| --- | --- | --- |
| `--suite {boot,snapshot,restore,e2e,phase2,snapshot-profile,all,cold-start,device-io,device-restore-profile,network-snapshot,performance,platform-snapshot-restore-vcpu,shell-snapshot,shell-snapshot-restore,snapshot-restore-memory,snapshot-restore-vcpu,virtfs}` | `boot` | Select an acceptance, diagnostic, or workload suite. |
| `--backend {whp,kvm,mshv,both}` | `both` on Windows; `kvm` elsewhere | Select the hypervisor backend. |
| `--platform NAME` | inferred OS/backend | Record the host-typed performance series. |
| `--openvmm-dir PATH` | `openvmm/` | Select the OpenVMM repository. |
| `--nvx-dir PATH` | repository root | Select the NVX repository containing guest artifacts. |
| `--warmups N` | `5` for `device-io`; `1` for `device-restore-profile`; `3` otherwise | Set the number of excluded warmup attempts; zero is allowed. |
| `--runs N` | `30` for `device-io`; `5` for `device-restore-profile`; `11` otherwise | Set the number of retained attempts. |
| `--memory-mib MIB` | `128` | Set guest memory for the general suites. |
| `--processors {1,2,4,8}` | `1` | Run every cold, capture, restore, and workload launch with this microVM count. |
| `--virtfs-runs N` | `3` | Set the number of virtio-fs workload samples. |
| `--virtfs-memory-mib MIB` | `512` | Set guest memory for the virtio-fs workload. |
| `--payload-mib MIB` | `64` | Set the virtio-fs sequential I/O payload size. |
| `--shell-memories MIB [MIB ...]` | `128 256 512` (`128 256 512 1024` for `snapshot-profile`) | Set the guest memory sizes for shell snapshot measurements. |
| `--network-memory-mib MIB` | `256` | Set guest memory for the network snapshot workload. |
| `--restore-devices {console,net,virtiofs} [...]` | all three devices | Select devices for the `device-restore-profile` suite. |
| `--restore-modes {active,deferred} [...]` | both modes | Select activation modes for the `device-restore-profile` suite. |
| `--device-io-duration-seconds SECONDS` | `10` | Set each storage-operation or UDP round-trip measurement window. |
| `--device-io-size-mib MIB` | `512` | Set the virtio-blk and virtio-fs backing-object size. |
| `--device-io-port PORT` | `5201` | Set the same-host UDP echo port. |
| `--net IPV4/PREFIX` | none | Enable virtio-net with a static guest address. |
| `--network-profile {portable}` | none | Required with `--net`; selects the portable KVM/MSHV/WHP contract. |
| `--cpus CPUSET` | one logical CPU per physical core | Set process affinity in `taskset` syntax. |
| `--host-cpu-reserve N` | `2` | Require this many affinity CPUs beyond the guest vCPU count for VMM/device work. |
| `--timeout SECONDS` | `10` | Set the time allowed for each boot marker. |
| `--teardown-mode {guest-exit,host-terminate,host-sigterm}` | `guest-exit` | Select how to stop a measured VM; `host-sigterm` is a deprecated alias. |
| `--skip-build` | off | Reuse existing release binaries. |
| `--snapshot-profile` | off | Retain OpenVMM lifecycle phase samples and host counters, and on Windows sample storage, CPU, writeback, and Defender telemetry during each capture; implied by the `snapshot-profile` suite. |
| `--cache-state {warm,cold,both}` | `both` | Select artifact cache states for the `snapshot-profile` suite. |
| `--output PATH` | none | Write the benchmark result as JSON. |
| `--output-dir PATH` | none | Write canonical workload logs to a directory. |
| `--scratch-dir PATH` | system temporary directory | Select an existing directory for temporary snapshots, guest RAM backing, and workload files. |
| `--keep-kvm-stage` | off | Keep temporary staged KVM benchmark binaries. |

Measured counts must be at least 1; warmups may be zero, and timeouts must be greater than zero.
Every suite but `phase2`, which boots no microVM, refuses a host before it
builds or measures anything if no built-in [CPU profile](#cpu-profiles) of the
`--openvmm-dir` checkout serves its CPU, or if it cannot identify the CPU or
read that checkout's profiles and so cannot confirm that one does.
On such an Intel or AMD CPU, OpenVMM's default CPU profile selection, `auto`,
falls back to a host profile, whose every cold boot fingerprints the
hypervisor first and whose results no baseline shares, and the refusal names
the CPU and links a CPU profile request. On another vendor's CPU, every cold
boot fails with `E_PROFILE_HOST_UNKNOWN`, as it does on a CPU that profiles of
more than one of the checkout's generations cover, which `benchmark` refuses
too: `auto` rejects such a catalog without falling back.
The `e2e` suite uses the general memory size and measures cold start, snapshot
generation, snapshot restore, guest-exit teardown, and peak RSS against the
shell-ready markers. CI uses the default 128 MiB baseline.
JSON results and workload metadata record the benchmark host's provenance under
`host`, without credentials or Azure resource identifiers.
See [Benchmark](benchmarks.md) for suite semantics, platform support, metric
definitions, and complete examples.

### `performance`

`performance` processes benchmark outputs for CI. It requires one nested
command.

#### `performance collect`

```text
python3 scripts/nvx.py performance collect
    --platform PLATFORM
    --commit COMMIT
    --input-dir PATH
    --output-dir PATH
    [--require-network]
    [--require-shell-snapshot]
    [--require-shared-suite]
    [--require-shell-snapshot-restore-512]
    [--lifecycle-input PATH]
    [--lifecycle-attempt-log PATH]
    [--summary PATH]
```

Parses canonical benchmark logs into p50 CSV files. Each row records `--commit`
as a full commit ID, resolving a revision such as `HEAD` in the NVX checkout.
The `--require-*` flags reject incomplete inputs for their respective workload
sets.
`--require-shell-snapshot-restore-512` accepts only the canonical 512 MiB
restore metric from a 2-, 4-, or 8-vCPU run.
`--lifecycle-input` validates and merges a 128 MiB, guest-exit `e2e` JSON
result, producing the 29-metric microVM CI result. A directory whose metadata
selects `device-io` is collected as five additional ABI-2, one-vCPU `ops/s`
metrics; CI merges them into a 34-metric one-vCPU result.
`--lifecycle-attempt-log` reads the attempt log that `validate-openvmm`
records. The `--lifecycle-input` file must be a byte-identical copy of exactly
one valid logged attempt, or collection fails before it writes a CSV; the
command names that attempt.
On MSHV and WHP, collection also fails before it writes a CSV when a
shell-snapshot restore series fails the check of `validate-workload`.
`--summary` writes the p50 table plus lifecycle min/max/sample-count and RSS
diagnostics, the benchmark host provenance, and any snapshot-generation storage
telemetry. With `--lifecycle-attempt-log`, it also lists every attempt, marks
the published one, and reports the telemetry of rejected attempts.
When the inputs record the benchmark host, the command also writes it to
`--output-dir` as `<platform>-host.json`, the format of an
[expected benchmark host](benchmarks.md#expected-benchmark-hosts). It fails if
only some inputs record a host, if the inputs, or a host file already in
`--output-dir`, record another host, and if `--output-dir` would mix results
that record a host with results that record none.

#### `performance validate-openvmm`

```text
python3 scripts/nvx.py performance validate-openvmm
    --platform PLATFORM
    --input PATH
    [--attempt-log PATH]
```

Validates a complete 128 MiB, guest-exit OpenVMM `e2e` result without
writing a CSV. Snapshot-generation instability exits with status 75 so
callers can remeasure the temporary host condition selectively; other
malformed or incomplete inputs exit with status 2. `--attempt-log` appends
the input's path, SHA-256 digest, outcome (`valid`, `unstable`, or
`invalid`), exit status, and error to a JSON attempt log, creating it on the
first attempt.

#### `performance validate-workload`

```text
python3 scripts/nvx.py performance validate-workload
    --platform PLATFORM
    --input-dir PATH
```

Checks the shell-snapshot restore series in the `shell-snapshot.log` and
`shell-snapshot-restore.log` of a workload output directory without writing a
CSV, so that callers can remeasure a series that a transient host stall split
before they collect it. On MSHV and WHP, a series of at least ten samples must
list them in its log, and it fails the check when its p50 is more than 25% above
p25 or a gap of more than 25% separates its sorted samples with at least two on
each side; the command reports every such series. KVM restore series are not
checked, as [Shell snapshot](benchmarks.md#shell-snapshot) explains. A failed
check exits with status 75; a missing log, a malformed or inconsistent sample
list, or an unsupported platform exits with status 2.

#### `performance collect-openvmm`

```text
python3 scripts/nvx.py performance collect-openvmm
    --platform PLATFORM
    --commit COMMIT
    --input PATH
    --output-dir PATH
    [--summary PATH]
```

Converts a complete 128 MiB, guest-exit OpenVMM `e2e` result into an
eight-metric lifecycle p50 CSV. Like `performance collect`, it records `--commit`
as a full commit ID and writes the recorded benchmark host to `--output-dir` as
`<platform>-host.json`.

#### `performance gate`

```text
python3 scripts/nvx.py performance gate
    --baseline-dir PATH
    --target-dir PATH
    [--window N]
    [--minimum-history N]
    [--threshold PERCENT]
    [--absolute-tolerance-ms MILLISECONDS]
    [--history-reset-dir PATH]
    [--summary PATH]
```

Checks target p50 values for regressions against rolling baseline histories.
`--window` defaults to `10`, `--minimum-history` to `10`, `--threshold` to
`40`, and `--absolute-tolerance-ms` to `5`. The gate uses the median of the
available window and treats metrics with insufficient history as warmups.
`--summary` writes a Markdown summary.
`--history-reset-dir` names tracked candidate histories; metrics removed from
an existing history file restart baseline warmup. When a candidate expected
host, `<platform>-host.json` in `--history-reset-dir`, differs from the one in
`--baseline-dir` in a required property, the gate fails unless the candidate
history files keep no row of that series, of any metric.

#### `performance persist`

```text
python3 scripts/nvx.py performance persist
    --source-dir PATH
    --history-dir PATH
    [--exclude-metric NAME]...
    [--require-host]
```

Appends current p50 values to branch history. Repeat `--exclude-metric` to omit
more than one metric. When both directories contain a platform's host file,
`<platform>-host.json`, the command refuses to append that platform's results
if their recorded host differs from the history's in a required property, and
warns about advisory differences. It also refuses results without a host file
for a history that has one. `--require-host` requires both files for every
platform. When `--history-dir` lies in a git checkout, even before the
directory exists, the command refuses results whose commit is not a full
commit ID or not an ancestor of `HEAD`, and skips results measured before a
later commit removed rows of their metric from the history file, as a reset
does; see [CI collection](benchmarks.md#ci-collection).

#### `performance check-host`

```text
python3 scripts/nvx.py performance check-host
    --platform PLATFORM
    --history-dir PATH
    [--scratch-dir PATH]
    [--output PATH]
    [--summary PATH]
```

Compares this host with the expected host of the `PLATFORM` series,
`<platform>-host.json` in `--history-dir`, as CI does before it measures the
series; see [Expected benchmark hosts](benchmarks.md#expected-benchmark-hosts).
The command exits with status 1 when a required property, such as the CPU, the
VM size, the OS build, or a benchmark disk or volume, differs, and with status
2 when the expected host is missing or malformed. An advisory difference, such
as the microcode revision or a Windows cumulative update, only warns. The
command prints every compared property, and reports the required and the
advisory differences as one GitHub Actions error and one warning annotation.
`--scratch-dir` names the benchmark scratch directory whose volume the
benchmarks will use; it defaults to the system temporary directory. `--output`
writes this host in the host-file format, ready to replace the expected host,
and `--summary` appends a Markdown comparison that includes it.

#### `performance storage-events`

```text
python scripts\nvx.py performance storage-events
    --since TIME
    --output-dir PATH
    [--scratch-dir PATH]
```

Saves the Windows storage events from `--since`, an ISO 8601 time with a UTC
offset or `Z`, such as PowerShell's round-trip format, until now, as a failed
Windows platform job does; see [CI collection](benchmarks.md#ci-collection).
It writes the `Microsoft-Windows-Storage-Storport/Operational` events and the
System log's storage events as rendered XML, and `storage-events.txt`, which
summarizes them and lists every event, to `--output-dir`, and prints the
summary. I/Os that took longer than Storport's threshold raise a GitHub Actions
warning annotation, and so does a log that can't be read, which doesn't fail the
command. `--scratch-dir` names the benchmark scratch directory, whose volume,
with the workspace, temporary, and system volumes, the summary names for each
Storport device; it defaults to the system temporary directory. A stalled
volume can stall what reads it, so the command abandons a query after 120
seconds, and probes the volumes only once it has saved the events, for at most
30 seconds, after which the summary says why it names none. It also abandons
creating `--output-dir` or writing a file there after 30 seconds, as the
directory can share that volume, and the summary and a warning then name each
file that it couldn't save. Reading the
Storport log takes a service account, such as the runners' Network Service, an
elevated prompt, or membership in Event Log Readers. On other hosts, the command
exits with status 2.

## Source and package commands

### `collect-sources`

```console
python3 scripts/nvx.py collect-sources
```

Materializes the verified Linux, Alpine, and Ubuntu source artifacts needed
for a source-inclusive release. Azure Linux corresponding source is not
collected, so source-inclusive packages omit the Azure Linux guest.

### `collect-alpine-sources`

```text
python3 scripts/nvx.py collect-alpine-sources MANIFEST [MANIFEST ...]
    [--output PATH]
    [--cache PATH]
    [--skip-upstream]
```

| Option | Default | Description |
| --- | --- | --- |
| `MANIFEST` | required | One or more Alpine package manifests to collect. |
| `--output PATH` | `build/sources/alpine` | Select the output directory. |
| `--cache PATH` | `.cache/aports` | Select the aports cache directory. |
| `--skip-upstream` | off | Collect exact aports recipes without running `abuild fetch`. |

Each run replaces the generated `recipes`, `upstream`, `manifest.json`, and
`SHA256SUMS` entries. The collector rejects an output path that is a symlink or
a Windows junction, and an output directory that contains anything else.
Release checksum manifests reject symlinks, so a recipe symlink is stored as a
regular copy of its target. Links that leave the recipe, hard links, and
special files are rejected.

### `collect-ubuntu-sources`

```text
python3 scripts/nvx.py collect-ubuntu-sources MANIFEST [MANIFEST ...]
    [--output PATH]
    [--cache PATH]
```

| Option | Default | Description |
| --- | --- | --- |
| `MANIFEST` | required | One or more Ubuntu package or EROFS manifests to collect. |
| `--output PATH` | `build/sources/ubuntu` | Select the output directory. |
| `--cache PATH` | `.cache/ubuntu-source-indexes` | Select the downloaded source-index cache. |

The collector requires `gpgv`, deduplicates source package name/version pairs,
authenticates live or historical Ubuntu source indexes through signed
`InRelease` metadata and the pinned Ubuntu archive keyring, validates each
`.dsc`, and downloads every referenced source member.

### `create-linux-source-archive`

```text
python3 scripts/nvx.py create-linux-source-archive
    --config PATH
    --output PATH
```

Creates the deterministic Linux corresponding-source archive from the pinned
kernel inputs and the generated kernel configuration. `--config` must name an
existing generated kernel configuration, and `--output` names the archive to
create.

### `package`

```text
python3 scripts/nvx.py package
    [--version VERSION]
    [--destination PATH]
    (--include-source | --binary-only)
    [--force]
```

| Option | Description |
| --- | --- |
| `--version VERSION` | Override the packaged version. |
| `--destination PATH` | Override the staging destination. |
| `--include-source` | Include the corresponding source artifacts in the package and omit the Azure Linux guest artifacts. |
| `--binary-only` | Stage binaries only; publish corresponding source separately. |
| `--force` | Replace an existing staging destination. |

Exactly one of `--include-source` and `--binary-only` is required. Every
package ships `nvx-rs` with its provenance, which must describe
`nvx-rs/target/release/nvx-rs[.exe]` as built from the checkout's sources
for the release target of the host's operating system:
`x86_64-unknown-linux-musl`, a static executable, on Linux, and
`x86_64-pc-windows-msvc` on Windows; see
[`record-nvx-rs-provenance`](#record-nvx-rs-provenance). See
[Package and source delivery](distribution.md) for release procedures and
source-publication requirements.

### `archive-release`

```text
python3 scripts/nvx.py archive-release
    --source PATH
    --destination PATH
```

Validates the staged distribution against its `SHA256SUMS`, snapshots the
accepted inventory, and creates a deterministic archive at the destination.
The destination must be outside the source directory and end in `.tar.gz` or
`.zip`.
