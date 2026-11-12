# nvx

`nvx` is the Rust crate that holds the NVX mechanics: the OpenVMM
control protocol, and the launch, policy lowering, and supervision of NVX
microVMs. Its package and library are both called `nvx`; the package was
called `nvx-rs`, like the directory that holds it. The crate also holds the
programs that run in NVX's Linux guests, and the `nvx-rs` command-line
interface, which runs NVX's microVMs and sandboxes, compiles egress policies,
looks up artifacts, and downloads releases, the runtime steps that the phases of
[microsoft/nvx#356](https://github.com/microsoft/nvx/issues/356) moved from
`nvx.py` to Rust.

An NVX sandbox is a microVM that OpenVMM runs with the managed lifecycle: the
guest's init starts a managed agent, which runs the workloads that the host
sends it over an authenticated control console. The crate is published on
crates.io as a preview, whose API can change in any release: the nightly CI
run publishes `dev` as the next pre-release, such as `0.1.0-alpha.4`, whenever
the crate changed; see [crates.io releases](../doc/ci.md#cratesio-releases).
Cargo resolves a requirement such as `nvx = "0.1.0-alpha.4"` to the newest
pre-release of `0.1.0`, so require an exact version, such as
`nvx = "=0.1.0-alpha.4"`, to keep one. It has a
`Cargo.lock` of its own; a workspace at the repository root would interact with
the nested `openvmm/` workspace.

## Layout

```text
nvx-rs/
├── src/
│   ├── protocol/    NVXS session, NVXC frames, EXEC and MAPS payloads, kernel command-line
│   │                tokens, and the guest targets that live shares may not take; no
│   │                dependencies, shared by the host and the guest
│   ├── activation/  the activation record of a layered root, its canonical encoding, and what
│   │                Ready reports about it; no dependencies, shared by the host, the guest, and
│   │                every adapter's framing
│   ├── contract.rs  control-contract versions, release-manifest compatibility, and the versions
│   │                of the contracts through which OpenVMM restores a snapshot
│   ├── net/         CIDR parsing and subtraction, and hostname and guest-network forms
│   ├── gpt/         strict reading of GPT disks whose first partition configures an adapter and
│   │                whose other partitions hold a root's read-only layers; no dependencies,
│   │                shared by the host and the guest
│   ├── uuid.rs      UUIDs in their canonical text and in GPT's layout; no dependencies, shared
│   │                by the host and the guest
│   ├── hypervisor.rs
│   ├── error.rs     nvx::Error, its ErrorKind, and its stable ErrorCode
│   ├── host/        feature `host` (default): the runtime that every launch shares, VM
│   │                directories and configurations, artifacts, network and egress policy files,
│   │                filesystem, NVXS connection, session, process, platform, templates'
│   │                manifests and compatibility keys, outcome reports, file seals, and the
│   │                sandbox launch profile of `nvx-rs sandbox` and its distro and scratch
│   │                bindings; with feature `cli`, the `nvx-rs` CLI
│   ├── testing/     feature `testing`: the fake OpenVMM
│   ├── guest/       feature `guest`: the programs that run in Linux guests, the guest's NVXS
│   │                connection, the libraries through which a guest init or agent opens the
│   │                terminal on which it serves its session, mounts host mappings beneath the
│   │                descriptors of the roots that it names, assembles and tears down a layered
│   │                root, reads a GPT disk's partition table through its block device, and
│   │                prepares a template's guest for a capture before its root's assembly, and
│   │                the typed calls through which it runs the time ABI's steps and the reseed
│   └── bin/         nvx-rs, nvx-fake-openvmm, and the guest binaries
├── tests/           fake-OpenVMM, CLI, NVXS session, process and spawn, checked-handoff,
│                    host-module layering, and guest-binary tests, the CLI's scenario tests and
│                    their harness, in cli/, the
│                    hand-written sandbox configurations of every format, the cases of the export
│                    and root boot tokens, and the template manifests that OpenVMM wrote, in
│                    fixtures/, and the conformance vectors of the wire formats and of a template's
│                    key, in vectors/
└── public-api/      the crate's public API on Linux and on Windows, which CI checks
```

The [module diagrams](../doc/modules/README.md) show the crate's modules, the
modules that each contains, and the dependencies between its top-level modules,
on Linux and Windows.

The [filesystem module](src/host/filesystem/mod.rs) keeps its request types and
planner together, with private siblings for guest and host paths, access policies,
OpenVMM limits, and recorded-plan checks and rendering. The CLI's
[managed sandbox lifecycle](src/host/cli/sandbox/lifecycle/mod.rs) separates state
directories, outcome reports, start and exec, and stop and deprovision. The
CLI's own [store](src/host/cli/store/mod.rs) keeps the layout of a sandbox's
state directory and its documentation, with private siblings for the names of
its files and its lock. The
[Linux](src/host/platform/linux/mod.rs) and
[Windows](src/host/platform/windows/mod.rs) platform modules each separate
processes, spawns, private directories and files, file identities, the claim of a
launch's log, and OpenVMM's endpoints, and on Windows, the CLI's Ctrl-C.
Their `mod.rs` files retain the existing entry points through re-exports; their
`tests.rs` files keep the unit tests at the same module paths.

The table below says in one line what each module is for, and links to the
module's section after the table, which holds the module's details. A change to
a module's details goes in its section; its row, at most 200 characters long,
changes only when the module's purpose does.

| Module | Purpose |
| --- | --- |
| [`nvx::protocol`](#nvxprotocol) | The codec of the control console's `NVXS` records and the payloads that the host, the guest agent, and the fake OpenVMM share |
| [`nvx::protocol::cmdline`](#nvxprotocolcmdline) | The kernel command line as Linux and the guest's init read it, with the virtio-mmio devices and the guest's DNS server that it names |
| [`nvx::protocol::export`](#nvxprotocolexport) | The boot tokens of the host export, the virtio-fs device from which a guest maps host paths, and the guest's parser of them |
| [`nvx::protocol::nvxs`](#nvxprotocolnvxs) | The NVXS session, independent of any application protocol: its records and the state machines of its host and guest roles |
| [`nvx::protocol::restore_ready`](#nvxprotocolrestore_ready) | The exact parser of OpenVMM's restore-ready event, including its terminating newline |
| [`nvx::protocol::root`](#nvxprotocolroot) | The boot tokens that tell the guest which image its layered root assembles, where the root keeps its upper, and how the guest assembles them |
| [`nvx::protocol::share`](#nvxprotocolshare) | The guest targets that a sandbox's live shares may not take, which the host and the guest both refuse |
| [`nvx::protocol::status`](#nvxprotocolstatus) | The exit statuses with which a clone's guest ends its VM once it fails fast, and what each means |
| [`nvx::protocol::view`](#nvxprotocolview) | A cold boot's hostname bytes and machine-ID mode, with their renderer and strict boot-token parser |
| [`nvx::activation`](#nvxactivation) | The activation record of a layered root, which a clone reads in place of its captured configuration, and what Ready reports |
| [`nvx::contract`](#nvxcontract) | The versions of the contracts that the crate speaks with OpenVMM and the guest, and the compatibility of releases with them |
| [`nvx::net`](#nvxnet) | Network primitives that do not depend on the host: CIDR networks, hostnames, guest addresses, and gateways |
| [`nvx::gpt`](#nvxgpt) | Strict reading of GPT disks whose first partition configures an adapter and whose others hold a root's read-only layers |
| [`nvx::uuid`](#nvxuuid) | UUIDs, such as those of GPT disks and filesystems, in their canonical text and in GPT's layout |
| [`nvx::Hypervisor`](#nvxhypervisor) | KVM, MSHV, or WHP, with its serde form behind the `serde` feature |
| [`nvx::host::runtime`](#nvxhostruntime) | `Runtime`: what every launch shares, the runtime's files and their identity, the hypervisor and its probe, and the launch options |
| [`nvx::host::vm`](#nvxhostvm) | A private VM directory, cold, capture and restore preparation, owned launch commands, verified detached launch, confirmed stop, and caller-saved reattachment |
| [`nvx::host::snapshot`](#nvxhostsnapshot) | Strict private snapshots, post-exit capture finishing, verified metadata, VM compatibility, captured NIC addresses, and time eligibility |
| [`nvx::host::config`](#nvxhostconfig) | `VmConfig` and `CaptureConfig`: pure configuration, cold-boot kernel/OpenVMM arguments, portable length checks, and capture identity |
| [`nvx::host::restore`](#nvxhostrestore) | A caller-driven restore to Ready without I/O or a clock, and its peer-verified restore-ready listener |
| [`nvx::host::network`](#nvxhostnetwork) | The one lowering of a network posture into OpenVMM options, for the CLI and external orchestrators |
| [`nvx::host::network::policy`](#nvxhostnetworkpolicy) | Egress policy files and their compilation into OpenVMM's native rules |
| [`nvx::host::filesystem`](#nvxhostfilesystem) | The planner of the host paths that every NVX caller exports to the guest through OpenVMM's virtio-fs device |
| [`nvx::host::nvxs`](#nvxhostnvxs) | Compatibility paths for the host NVXS connection and its retained cause details |
| [`nvx::host::transport`](#nvxhosttransport) | A verified VM endpoint connection and the blocking host NVXS driver, with the existing timed transport |
| [`nvx::host::process`](#nvxhostprocess) | Verified references to OpenVMM processes, through which callers wait for them and end them |
| [`nvx::host::report`](#nvxhostreport) | OpenVMM's outcome report |
| [`nvx::host::sandbox`](#nvxhostsandbox) | The launch profile of `nvx-rs sandbox`: its live shares and option checks; its layer and bindings are private to the CLI |
| [`nvx::guest`](#nvxguest) | The programs that run in Linux guests; see [Guest binaries](#guest-binaries) |
| [`nvx::guest::mappings`](#nvxguestmappings) | `Mapping` and `MappingTable`: host mappings, and the account of a host's mapping table, for any guest init or agent |
| [`nvx::guest::nvxs`](#nvxguestnvxs) | `GuestConnection`: the guest's role of the NVXS session over a byte stream, such as the control console |
| [`nvx::guest::terminal`](#nvxguestterminal) | `Terminal`: the terminal on which a guest agent serves its session, such as the VM's control console |
| [`nvx::guest::time`](#nvxguesttime) | x86_64: the boot steps of NVX time ABI v1 as typed calls in the caller's process, for any guest init or agent |
| [`nvx::guest::snapshot`](#nvxguestsnapshot) | x86_64: bounded Fresh capture with inline restore repair and staged failure dispositions, without execution control |
| [`nvx::guest::activation`](#nvxguestactivation) | x86_64: fence-free activation of each clone that its capture repaired |
| [`nvx::guest::reseed`](#nvxguestreseed) | A restore's fresh entropy, and the reseed of the kernel's random number generator with it |

GPT lower layers use `noload,errors=remount-ro`: the kernel neither loads nor
replays their journals, because even a journal whose file system does not
request recovery can cause an unbounded scan. The assembly still refuses any
layer whose superblock requests recovery, before mounting anything. This
policy does not change EROFS layers or writable ext4 scratches.

Lowering functions take the names by which messages refer to their inputs, such
as `EgressRules::name` and `PathList::name`, so callers' messages name their own
fields.

### `nvx::protocol`

The codec of the control console: `NVXS` outer records, the `MAPS` payloads of
the host mapping table, and the `nvx_maps=` and `nvx_workload_account=create`
tokens. NVX's own control protocol, `NVXC`, is private to the crate, gated by
the features of its users, `cli`, `guest` and `testing`: its application
frames, and the `EXEC` payload that the host encodes and the guest agent
decodes. `protocol::app::v2` holds the codec of version 2's frames, NVXC's only
version, which carry every control path, with version 1's payloads, and stream
an execution's standard input, output and error. NVX's host, the guest loop
that nvx-init and the managed agent share, and the fake OpenVMM speak version 2
alone, and refuse a frame of another version, such as version 1, whose frames
and `FEATURES` request are gone.

The [conformance vectors](tests/vectors/README.md) of version 1,
`tests/vectors/nvxc-v1.txt`, and OpenVMM's vectors and cases of the `NVXS`
session pin the records and the `EXEC` and `MAPS` payloads independently of the
codec, which the host, the guest agent, and the fake OpenVMM share; version 2
refuses the frames of version 1 that they hold, their features payloads have
no codec any longer, and version 2's tests pin its own frames.

### `nvx::protocol::cmdline`

The kernel command line as Linux 6.18 reads it, for the readers that need the
kernel's view of it: `parameters` splits it into `Parameter`s as `parse_args`
and `next_arg` do, at the bytes that `is_space` accepts, with double quotes, up
to a NUL or `--`, and `Parameter::is_named` compares names as `parameq` does,
with dashes and underscores alike.

`VirtioMmioDevice::parse` reads a `virtio_mmio.device` value,
`SIZE@BASE:IRQ[:ID]`, as the virtio-mmio driver reads it, for a device of any
type, and `virtio_mmio_devices` reads every such parameter of a command line.

`init_tokens` splits a command line into `Token`s as NVX's guest init splits
it, for the readers of the boot tokens that the init consumes: at spaces, tabs,
and newlines alone, without quoting, and past a NUL or `--`, with each token's
name up to its first `=`, which the init compares byte for byte.
`HostExport::parse`, `RootLayout::parse`, `parse_host_mapping_count`, and the
time ABI's check of report-only mode read the command line with it, and so does
the sandbox plans' check of the user's kernel parameters, which also reads the
kernel's view and compares names as the kernel does in both.

`dns_server` reads the guest's DNS server from OpenVMM's `virtnet_dns=` token,
`DNS_TOKEN`, strictly, as OpenVMM renders and reads it: in a command line split
at ASCII whitespace, at most one token, with one IPv4 or IPv6 address in the
form that OpenVMM writes, and `write_resolver` writes the resolver file that
names it, `nameserver ADDRESS`, as NVX's init writes the initramfs's
`/etc/resolv.conf`. `gateway` reads the IPv4 gateway of the guest's NIC from
OpenVMM's `virtnet_gw=` token, `GATEWAY_TOKEN`, as strictly: the address to
which the announcer of a clone's listeners sends its announcements.

### `nvx::protocol::export`

The boot tokens of the host export, the aggregate virtio-fs device from which a
guest maps host paths, which OpenVMM appends to the kernel command line:
`virtfs_dir=` the export's guest directory, `EXPORT_DIRECTORY`,
`/run/nvx/hostfs/root`, where the host's `nvx::host::filesystem` plan exports
it, `virtfs_tag=` the device's tag, `virtfs_mode=` `ro` or `rw`, an
`ExportMode`, and `virtfs_aggregate=1`.

`HostExport::parse` reads them as the guest's init splits the command line: all
four tokens or none, each once and in its form, with the checks of NVX's shell
init, and no `nvx_share=` token, which binds a live share. It returns the
`HostExport`, the device's tag and mode, which nvx-init's space mounts, or
`None`.
`tests/fixtures/export-tokens.json` holds its cases.

### `nvx::protocol::nvxs`

The NVXS session, independent of `NVXC` and of any other application protocol:
`RecordType`, `Record`, `encode_record`, and `RecordParser`, which parses
records incrementally across partial reads with buffers bounded by one record;
`HostSession` and `GuestSession`, the state machines of the host's and the
guest's roles, which validate every record's type, identity, and sequence
number, and the guest's receive window and credits, without I/O.

An application message may span DATA records, and the application's own framing
reassembles it.

### `nvx::protocol::restore_ready`

`parse_ready_event` accepts only `OPENVMM_RESTORE_READY_V1\n`, byte for byte,
including the newline. Truncated events, another version, CRLF endings, and
trailing bytes fail with `ProtocolError`.

### `nvx::protocol::root`

The boot tokens of a layered root, which the host renders and guests parse by
the same rules: a `RootLayout` is a `RootImage` on the `distro` block device, an
EROFS filesystem or a GPT disk with the `nvx::uuid::Uuid` that the guest expects
of it, if any, over a `RootUpper`, the ext4 scratch disk or RAM, with the
settings of the root's assembly that it names: the overlay's `OverlayMode` and
a scratch's `Journal` policy, both of `nvx::activation`, a GPT disk's
first-partition bound, and a RAM upper's cap, from 1 to 90 percent of the VM's
memory. The guest takes its own default for a setting that the layout leaves
out.

`RootLayout::tokens` renders `nvx_layer=distro,0xd0003000,FORMAT[:IDENTITY]` and
`nvx_scratch=0xd0006000,ext4` or `nvx_overlay_upper=ramfs`, then
`nvx_overlay_mode=trusted-lowers` or `untrusted-lowers`,
`nvx_scratch_journal=allowed` or `forbidden`, `nvx_first_partition_bound=BYTES`,
and `nvx_ram_upper_percent=PERCENT` for the settings that it names, and
`RootLayout::parse` reads them from a kernel command line, and refuses
malformed, repeated, and conflicting tokens, a setting of an upper or a format
that the layout lacks, and the earlier form without a format, with no fallback
that probes the devices. It reads the virtio-mmio devices of the command line
with `nvx::protocol::cmdline`, to refuse a RAM upper beside one at the scratch
base. `tests/fixtures/root-tokens.json` holds its cases.

### `nvx::protocol::share`

The guest targets that a sandbox's live shares may not take, which the host's
`sandbox::mount` and the guest's `root` both refuse: `RESERVED_MOUNT_TARGETS`,
the trees where the container runtime mounts or binds,
`RESERVED_EXACT_MOUNT_TARGETS`, the directories that it creates,
`RESERVED_TARGET_CHARACTERS`, and `reservation`, which tells why a target is
reserved.

### `nvx::protocol::status`

The exit statuses with which a clone's guest ends its VM once it fails fast,
from which the host classifies a clone's end, with the diagnostic that precedes
each: the time ABI's own, `TIME_CONFORMANCE`, 193, `TIME_VIOLATION`, 194, and
`TIME_REPAIR`, 195, which keep their meanings; one for each other class of a
clone's repair's failures, `REPAIR_RESTORE` to `REPAIR_FINISH`, 40 to 45, which
`nvx::guest::clone::RepairError::status` selects; and one reserved for each
class of a clone's activation's failures, `ACTIVATION_DEADLINE` to
`ACTIVATION_ORDER`, 50 to 58.

`describe` tells what a status means.

### `nvx::protocol::view`

`tokens` renders a cold boot's `activation::View`: `nvx_hostname=HEX`, always
lowercase hexadecimal with two digits per hostname byte, then
`nvx_machine_id=image` or `nvx_machine_id=generated`. The hostname keeps the
view's 1 to 64 non-NUL bytes, including whitespace and non-UTF-8 bytes, without
quoting. For example, `box` becomes `nvx_hostname=626f78`, not the legacy
sandbox's raw `nvx_hostname=box`; consumers use the decoded bytes.

`parse` reads the exact names through `cmdline::init_tokens`, in either order,
and returns a `View`, or `None` when neither token is present. A partial pair,
duplicate, missing value, malformed hexadecimal, overlong hostname, decoded
NUL, or unknown machine-ID mode fails with `ProtocolError`. Restores receive
their view through the activation record instead.

### `nvx::activation`

The activation record of a layered root: one bounded, versioned, non-secret
record of the settings that its activation applies, which a clone restored from
a template reads in place of its captured configuration.

An `ActivationRecord` holds the schema's `VERSION`, 2, a clone's
`RestoreSection`, only the restore's `TransactionId`, and the `Configuration`:
the `Image`, EROFS or GPT, with the UUID or disk GUID that the guest expects;
the `Upper`, a tmpfs with its cap in percent of the VM's memory or the scratch
with its `Journal` policy; the `OverlayMode`; an `Export`, a directory or an
aggregate, and its 1 to 4,096 mappings; the `Network`, a DNS address, the
listeners' `Publication`, and a proxy endpoint without credentials; the `View`,
its hostname and `MachineIdMode`; and the `ExpectedResources`. Store, binding,
plan, policy and execution identities never enter a record. The other inputs
of the root's assembly are invariants of the template.

`View` is the view that a VM's configuration, a [`VmConfig`](#nvxhostconfig),
gives its workload: its hostname and `MachineIdMode`, without an identity, which
each execution names.

`ActivationRecord::new` refuses a configuration that `Configuration::check`
finds past a public limit, `MAX_MAPPINGS`, `MAX_HOSTNAME_BYTES`, or
`MAX_PROXY_ENDPOINT_BYTES`, which keep every record within `MAX_RECORD_BYTES`,
or with an export without mappings, mappings without an export, a tmpfs cap
outside 1 to 90 percent, a proxy endpoint with credentials, or a zero size.

`ActivationRecord::encode` writes a record's canonical encoding, its version,
then a tagged, length-prefixed field for each part in the order of their tags,
424 bytes at most, and `ActivationRecord::decode` reads exactly those bytes back
and refuses any others, naming the `Field` at fault. Version 1 is refused.

`ReadyActivation` is what Ready reports once the activation applied: a clone's
transaction ID, which `ReadyActivation::of` takes from a record, and nothing
more for a cold boot, in a canonical encoding of `COLD_LEN` or `CLONE_LEN`
bytes, 1 or 17, that `ReadyActivation::encode` writes and
`ReadyActivation::decode` reads.

The records and both encodings need no features.

### `nvx::contract`

The OpenVMM microVM ABI, boot contract, broker protocol, and control-contract
revision that the crate speaks, and when each changes, and
`manifest_is_compatible` for a release's `SOURCE-MANIFEST.json`; and the
versions of OpenVMM's snapshot format, saved device state, time ABI, and restore
packet, which a template's key holds and which an ignored test of
`tests/openvmm_vectors.rs` checks against the OpenVMM submodule.

### `nvx::net`

`Cidr`: IPv4 and IPv6 networks and their subtraction, which `Cidr::parse` reads
without host bits and `Cidr::parse_masked` as OpenVMM reads the destination of
an egress rule; `is_valid_hostname` and `is_valid_guest_network`, the forms of a
VM's hostname and guest address, and `guest_gateway`, the gateway of a guest
address.

`Ipv4Cidr`: a guest address, checked when it is built, which `Ipv4Cidr::new` and
`Ipv4Cidr::parse` refuse unless OpenVMM accepts it, as `is_valid_guest_network`
does; it displays and parses as `ADDRESS/PREFIX`, such as `10.0.0.2/24`, and
with the `serde` feature it serializes as that text.

### `nvx::gpt`

`Disk::read`: the protective MBR, primary header, and primary partition entry
array of a GPT disk, read through a `ReadAt` source that the caller gives and
checked strictly, as UEFI specifies them and as Linux requires them, without
ever reading the backup: 128-byte entries, at most 1 MiB of them, used entries
first and without holes, a first partition and 1 to 254 layers, extents in the
usable blocks without overlap, and unique GUIDs, with checked arithmetic in the
disk's logical blocks.

`Expected` adds the disk GUID and the partitions' unique GUIDs and sizes that a
caller knows, and `Disk::read_first_partition` returns the first partition's
opaque bytes, up to a bound and without trailing NUL bytes and whitespace.

GUIDs are `nvx::uuid::Uuid`s. It has no dependencies, so the guest and the
host's checks of the images that it stages share it.

### `nvx::uuid`

`Uuid`: a UUID, such as the GUID of a GPT disk or partition or the UUID of a
filesystem, in the order of its canonical text, the one text that it displays
and parses: 32 lowercase hexadecimal digits in groups of 8, 4, 4, 4, and 12,
joined by hyphens. `from_bytes_le` and `to_bytes_le` convert the layout in which
GPT and other EFI GUIDs store their first three groups little-endian. It has no
dependencies, so the host and the guest share it.

### `nvx::Hypervisor`

KVM, MSHV, or WHP, with its serde form behind the `serde` feature.

### `nvx::host::runtime`

`Runtime`: what every launch shares, the paths of the OpenVMM executable, guest
kernel, and guest initramfs, their `RuntimeIdentity`, the hypervisor, an
optional CPU profile, whether OpenVMM breaks away from the caller's Windows job
object, and the `LaunchOptions`. Those are what OpenVMM does on a plain guest
power-off or hibernate event, a `GuestPowerAction`, by default `Exit`, or
`Halt` or `Reset`, and how long OpenVMM waits for the host to authenticate on
the control console, in whole milliseconds from 1 to 60000. A power-off that
reports an exit status always exits OpenVMM with that status, regardless of
the selected action. The boot console is not among the options: each VM
attaches its own.

`RuntimeIdentity` labels the three files with the `ArtifactIdentity` tokens that
their install or staging metadata gives, such as `sha256:` and a digest, which
NVX compares and never computes, and adds the versions of the contracts that the
crate speaks; `differences` names the `IdentityPart`s in which two identities
differ. A caller saves it with each VM, in a `SavedVm`, which `vm::verify`
checks against the current one.

`probe_hypervisor` checks that the host has the hypervisor: its device on Linux,
and the Windows Hypervisor Platform on Windows.

### `nvx::host::vm`

The directory of a VM, which its caller creates and owns, one for each VM.
`create_private_dir` creates it private to the current user, and `VmDir::open`
opens an existing one only if it is private: on Linux, it belongs to the user
with mode 0700, which OpenVMM requires of the directory of its control socket;
on Windows, it belongs to the user, and its protected access control list holds
only entries that allow the user access, one of which allows full access, which
the files and directories created in it inherit at every depth. Neither changes
an existing directory: one that others may access fails with `ErrorKind::Io`
rather than being restricted, since they could still hold its files open.
Symbolic links and other reparse points are refused, including paths with
trailing separators or `.` components.

The library reserves these names in the directory, and every name that starts
with `.openvmm`, which OpenVMM gives its temporary files, such as
`.openvmm.staging-*`. The caller may keep files of its own under other names.

| Name | What it holds |
| --- | --- |
| `openvmm.log` | OpenVMM's log of a launch |
| `outcome.json` | OpenVMM's outcome report of a launch |
| `control.sock` | The control endpoint at which OpenVMM listens, on Linux |
| `console.sock` | The guest's boot console, on Linux, for a launch that listens on it |
| `restore-ready.sock` | The endpoint at which a restore awaits OpenVMM's restore-ready event, on Linux |
| `openvmm/` | OpenVMM's snapshot files: a capture's destination, and a restore's view of its snapshot |
| `key.bin`, `snapshot.json` | A finished capture's key and description, which form its snapshot with `openvmm/` |
| `capture-image.raw`, `capture-scratch.raw` | The sparse placeholder image and the dormant scratch that a capture VM attaches |

On Windows, where OpenVMM listens on named pipes, the control and console
endpoints are fresh pipes, `//./pipe/openvmm-microvm-` and 32 random
hexadecimal digits, the prefix that OpenVMM requires of the pipe of a console,
and a restore awaits its event at a fresh `//./pipe/nvx-restore-ready-` pipe.

`VmOutputs` holds the paths of OpenVMM's log and outcome report, its control
endpoint, and the optional boot-console endpoint. `Capability` holds the
credential that the host presents at a VM's control endpoint: `as_bytes`
returns it, and its `Debug` form hides it. The library never writes it to a
file; a caller that reconnects after a restart keeps it.

`SavedVm` bundles the caller's `VmDir`, the original control endpoint,
capability bytes, `ProcessRef` and `RuntimeIdentity`. The caller owns saving
and reloading these values; the library neither serializes nor locates them.
`vm::verify(&saved, &identity)` compares the exact runtime identity, returning
`RuntimeMismatch` naming the differing parts before opening the process if it
differs, then returns the verified held `Process` without opening an endpoint
or reading runtime files.
`vm::reattach(&saved, &runtime, deadline)` verifies, connects through
`transport::connect_verified`, and attaches with the saved capability. Its
`Reattached` holds both the process and authenticated connection. Failures
retain their original kinds and sources and leave the saved values unchanged.

`stop(process, outputs, graceful, grace, deadline)` waits through the verified
process, first after the caller's graceful request, then after forced termination
if needed. It returns `Stopped` only after confirmed exit. `self_exited` records
confirmation before a forced attempt, and `forced` records that attempt, not
proof from kill success. An unconfirmed exit is `StillRunning`, and the caller
keeps the process. The callback receives the bounded grace deadline and must
respect it; its boolean result is never exit proof.

`Stopped::report` retains the outcome-report read's result separately, so report
failure never hides confirmed exit.

`vm::create(&runtime, &config, None, &dir)` creates an inspectable cold
`Prepared` launch without starting a process or binding a listener. It requires
`config.boot`, rechecks the directory's privacy, and
creates a new private log. Existing log, outcome and Linux endpoint entries
are refused rather than replaced. An ordinary cold boot creates only the log
before a spawn; OpenVMM creates its remaining outputs when it runs. A failed preparation
closes and removes only its new files, preserving all pre-existing entries and
caller-owned inputs, and reports a cleanup failure with the original error.

With `Some(snapshot)`, creation refuses `boot`, checks `Snapshot::accepts`
at the real `HostTime::now`, and hard-links only the existing OpenVMM files
into a new `<vm_dir>/openvmm/`. A different filesystem fails; there is no
copy or snapshot-parent fallback. The restore's arguments follow the clone
launch's rules: the devices at the snapshot's processor capacity,
`--restore-network-policy` with the restore's own policy in place of `--net`
and the guest's address, which OpenVMM takes from the snapshot,
`--restore-absent-scratch` for an upper in RAM, `--restore-processors` only
when the snapshot records its boot processors, `--restore-memory`, the
console attachment that the snapshot allows, the snapshot's backend and CPU
profile, and the launch options, with no kernel, initramfs, command line or
`--memory`. Creation binds the restore-ready listener before any spawn and
retains that same owned listener through `Launched::ready` or
`VmmCommand::ready`, never in the child's inheritance allowlist.

On Linux, the caller holds its per-snapshot lock from creation, including
adding links, through a peer-verified restore-ready event or verified VM
exit, and around view removal. NVX takes no lock; Windows needs none.
A restore whose caller dies before OpenVMM connects the restore-ready
endpoint cannot complete: OpenVMM checks the snapshot files before that one
connection, which it does not retry, and the dead caller's endpoint no longer
listens. After the connection OpenVMM no longer checks those files, so no
durable in-flight restore record is needed; the caller keeps the VM's process
as for any launch. A successful view remains caller-owned.

With `boot.capture`, creation requires a blockless root and refuses every
filesystem export. It creates private sparse `capture-image.raw` and
`capture-scratch.raw` files, verifies their actual complete geometries, and
targets `<vm_dir>/openvmm`. A configured NIC keeps its resolved address but
uses the complete deny-all policy, whatever its configured policy.
`Prepared::capture_context()` exposes a cloneable, serializable
`snapshot::CaptureContext` with the checked capture configuration and disk
geometries, NIC and console devices, runtime identity, hypervisor and CPU
profile. The caller chooses whether and how to persist it. Creation writes
no context file, `key.bin` or `snapshot.json`, and does not finish or publish
a capture.

On Linux, the generated control and boot-console socket names must be valid
Unicode and shorter than 108 bytes both as supplied and with the directory's
ancestor links resolved. Supplied names must hold no comma or equals sign,
which OpenVMM's listener options cannot represent literally. Creation refuses
these names before creating any output, including the log.

`Prepared::capability()` and `outputs()` return what the caller may journal
before a spawn. `log()` returns the same opened log, without reopening its
path. `inherit(resource)` moves an already opened resource into the preparation's
inheritance allowlist and keeps it open. `arguments()` returns the whole
configuration's OpenVMM arguments. Creation makes caller-relative runtime
and block-image paths absolute without resolving links or `..` components,
so spawning in the VM directory does not change which inputs they name.
Creation checks the complete Windows
command line with the actual runtime and VM output paths and no reserve,
including quoting and the terminating NUL, on every host. Callers keep the
directory and runtime inputs stable through creation and a later spawn.
Dropping a successful preparation closes its resources but leaves its output
paths owned by the caller.

`Prepared::nic()` returns the effective NIC, including NVX's default cold
address when the caller leaves it unset, or none when no NIC is attached.
The preparation owns its checked configuration and NIC; later changes to the
caller's configuration do not affect them. A restore's resolved NIC retains
its captured presence and address, independently of its new network policy.

`Prepared::activation_record(tx)` builds record v2 from the owned, resolved
settings and preserves the supplied transaction. It carries the layered root,
image UUID or disk GUID and bound, upper and RAM cap or journal policy, view,
export mapping count and expected CPU and memory resources. The effective NIC
selects the network's presence and IPv4 and derived IPv6 gateways, while the
launch's policy supplies DNS, publication and proxy. An omitted limit takes the
record's maximum bound. It reads no host path and opens no snapshot. A
blockless root or memory that cannot fit the record's byte count fails with
`ErrorKind::Unsupported`. A restore resolves these settings against the
captured NIC and its own requested policy.

`Prepared::into_command()` moves the complete launch into `VmmCommand`: its
program, arguments, working directory, capability, outputs, inherited resources,
opened log, Windows breakaway setting, and any already-bound ready listener.
It neither reopens nor duplicates resources, renders again nor rediscovers
paths. `InheritedHandle` owns its
resource, converts from and back to `OwnedFd` on Unix or `OwnedHandle` on Windows,
and also accepts an opened `File`. The command documents the caller's spawn
contract, including standard streams, allowlisted inheritance, Linux detachment
and umask, Windows log job and breakaway, and cleanup of a failed child.
Read the resolved NIC or capture context, or build an activation record,
before consuming `Prepared` into the command.

The compiled [external-launch example](examples/external-launch.rs) consumes
this command, starts it with a minimal launcher of its own on `std::process`,
checks the cold guest's boot marker and shell response, then asks it to exit
cleanly. Its launcher passes the VMM no descriptor or handle besides its
standard streams, and starts it neither in a new session with the owner's bits
cleared from its umask on Linux nor in its log's job on Windows: a production
launcher adds what the command's contract asks, as `vm::spawn` does, and the
example's docs list what it leaves out. It holds the VMM through a `ProcessRef`
whose start time it reads as `ProcessRef` defines it: the `starttime` field of
`/proc/<pid>/stat` on Linux, and the process's creation time on Windows.
It requires the VMM's actual exit status 0 and a successful
outcome with complete teardown. Build with
`cargo build --locked --example external-launch` and supply the backend, OpenVMM,
kernel, initramfs, new VM directory, and the three artifact SHA-256 digests from
their installation metadata, in that order. Artifact paths may be relative to
the caller's directory.
Append a snapshot directory and EROFS image to run its restore-ready
demonstration instead, using a snapshot captured with a disconnected boot
console, as `nvx-rs snapshot capture` captures it. It uses the public
peer-verified receiver, then intentionally stops and verifies the VM's exit
without driving guest activation. Linux callers of the example coordinate on
the snapshot's `key.bin` lock, taken before opening the snapshot and retained
through view removal.

`vm::spawn(prepared)` consumes that preparation and returns `Launched` with
its verified `process`, the same `outputs`, the same `capability`, and a
restore's same bound `ready` listener. The log
and inherited resources are passed as already opened objects, never reopened
by name. The capability reaches OpenVMM only on standard input, then
end-of-file, not in arguments, environment variables, or a file. A failure
after child creation terminates that child and confirms its exit; a cleanup
failure is reported with the original error. Spawn removes no output path.
Dropping `Launched` closes the process handle but does not stop the VM.

On Linux the detached child gets the caller's umask with the owner's bits
cleared; the caller's own umask is unchanged. The umask still narrows the
group's and others' permissions to the files that OpenVMM creates, such as its
outcome report, its snapshots, and the files that a guest creates in a share,
but never the owner's, which a umask such as 0400 would clear, so that the
caller could not read the report, nor OpenVMM use its own files. In a directory
that has a default POSIX ACL, Linux narrows those permissions by the ACL
instead of the umask. On Windows it starts without a
console in its log-named job, refusing a taken or untrusted job name, and
leaves the caller's job when the runtime requests breakaway. The inheritance
lock is internal to spawn. Windows callers coordinate their own other spawns,
for example with their own lock around VM and other process starts: an
unrelated process started during the inheritance window may inherit handles,
but never the VM's job claim. Windows' process-creation API limits the VM
directory, in which OpenVMM starts, to 258 UTF-16 code units in its absolute
path, including any verbatim prefix and trailing separator: the spawn refuses
a longer one before it creates a process.

The [VM tests](src/host/vm/tests.rs) cover cleanup after child creation,
ownership, caller survival, the Linux umask, and Windows job claims, and the
[spawn tests](src/host/spawn/tests.rs) what the VMM inherits while other
threads spawn, the working-directory limit, the survival of a VMM whose
launcher crashed, and the waits and cleanup of its child. Their
ignored cold-spawn smoke test runs the current library against explicit
`NVX_VM_SPAWN_OPENVMM`, `NVX_VM_SPAWN_KERNEL`, and `NVX_VM_SPAWN_INITRAMFS`
paths, using a fresh `NVX_VM_SPAWN_PROOF_DIR` and `NVX_HYPERVISOR` or the
platform's default hypervisor. It checks a guest boot marker,
responsive shell input, and a verified clean exit; it downloads no artifact.

### `nvx::host::snapshot`

`SnapshotKey` and `SnapshotManifest` own the compatibility key and strict
OpenVMM manifest reader. `SnapshotManifest::time` and `SnapshotKey::time` expose
the capture's `TimeContract`, whose `eligibility` checks its restore window at a `HostTime`;
`HostTime::now` samples the host's clock and identities.

`SnapshotManifest::read` reads the `manifest.bin` that OpenVMM wrote for a
capture, at most `MAX_MANIFEST_LEN` bytes of a regular file that is not a link
or other reparse point, and refuses another snapshot format version, an unknown,
mistyped, or repeated field, truncated bytes, and a capture that is not a
template's: a platform-tier capture for clones, with a read-only `distro` whose
identity is unbound, a fresh scratch, and the dormant filesystem slot.

`SnapshotKey` checks that a capture made the machine that its context, the
`CaptureContext` with which `vm::create` prepared it, requested, and derives
the template's compatibility key from metadata alone: the runtime and contract
versions through which its clones are restored, the backend, CPU profile and
effective CPUID, topology, rates, resources, kernel command line, disks, and
devices of its machine, with the NIC's addresses and the identity of the
complete deny-all policy under which the capture attached it, which each
clone's restore replaces with the clone's own, so that a template with a NIC
serves the clones of every network policy at its guest's address.

A key has one canonical, versioned encoding, whose domain-separated SHA-256
digest is its identity, a `KeyIdentity`; the
[conformance vectors](tests/vectors/README.md) of version 1,
`tests/vectors/snapshot-key-v1.txt`, pin its encoding and identity. The key's
encoding, version 1, leaves the network policy out, since templates capture
their NIC under the complete deny-all policy.

The key's canonical encoding and identity use the snapshot names and omit
`guest_build`. Version 1 changes in place: no snapshot outlives a deployment,
and no reader or migration of the prior encoding is supported.

The persisted key includes the complete per-snapshot time contract and must
agree with the manifest's time. Its compatibility identity excludes capture
anchors, host and boot identities, lineage and cut-to-anchor time: captures
at different times can have different key bytes and the same `KeyIdentity`.
The declared rates and time ABI remain compatibility inputs.

`TimeContract::eligibility` decides, apart from compatibility, whether a clone
may restore a capture at a host's time, a `HostTime`, which `HostTime::now`
samples as OpenVMM does: within the boot of the capture, by the host's monotonic
clock, and otherwise by UTC, the downtime must not be negative, and the interval
by which a restore advances the clone's VM time at most `MAX_DOWNTIME_NS`, 30
days.

`Snapshot::open` checks exactly `key.bin`, `snapshot.json` and
`openvmm/{manifest.bin,memory.bin,state.bin}`. Directories are private, files
are regular and private to their owner, artifact lengths equal the strict
manifest's, and the canonical key is consistent with that manifest.
`KEY_FILE`, `DESCRIPTION_FILE` and `OPENVMM_DIR` name the root entries.
The description must exist, but its contents are never read, parsed or
trusted. `Snapshot::key` and `manifest` expose the verified metadata.
Opening also checks that a captured NIC address fits `NicConfig`, refusing
its network, gateway and broadcast addresses with `Io`, and caches that
checked value for the infallible `nic` accessor.

`Snapshot::accepts(&config, &runtime, &now)` checks a layered restore against
the captured boot resources and capacity. Processor counts are 1, 2, 4 or 8;
an encoded boot count of zero means all capacity processors are already
online. Memory is between boot and capacity, in multiples of 128 MiB, with
checked conversion from MiB to bytes. The image fits the placeholder and
keeps its logical and physical block sizes; a scratch upper matches the
dormant scratch's complete geometry. These checks read the requested disks'
real metadata, not the snapshot entries again.

NIC presence must match, and an explicitly named address equals the captured
one; an unnamed address inherits it. `Snapshot::nic` returns only the captured
presence and address as an `Option<NicConfig>`, never a network policy.
Each restore supplies its own policy. The boot console retains its captured
attachment policy: `recreate-listener` for a listener, or
`discard-while-disconnected` for a disconnected console. A mismatched,
missing or unknown policy is refused. These facts come from the same
validated manifest parse; the control listener and console device do not
prove that attachment. The runtime uses the same backend and captured CPU
profile, and has the same runtime identity. An omitted profile or `auto`
names the snapshot's profile; `host` also names a captured host-derived
profile. Requested disks must be regular files; on Linux, the geometry
check refuses a FIFO without waiting for its writer.
Compatibility refusals are `Unsupported`; an ineligible time contract at
the supplied `HostTime` is `Unavailable`.

The caller keeps the directory and every entry private and immutable while
using the snapshot; opening checks them once and retains no file handles
or lock. `vm::create(..., Some(snapshot), ...)` checks compatibility and
current-time eligibility, then creates the fixed hard-linked view and
binds the caller-owned ready listener described above.

`CaptureContext` records a prepared capture's checked configuration and
effective devices independently of runtime paths or network policy.
`vm::Prepared::capture_context` returns it for the caller to clone and
serialize; NVX owns no capture-context store or persistence format.

`finish_capture(&VmDir, &CaptureContext, &Exited)` requires the capture VM's
proved exit and successful outcome, checks the OpenVMM snapshot against the
context, and atomically writes `key.bin` and informational `snapshot.json`
beside `openvmm/`. It returns `CapturedSnapshot { key }`. The caller moves
only those three entries into its own private, immutable snapshot directory;
logs, outcomes and other VM files remain where they are. A failure can be
corrected and retried while the VM directory and capture context stay intact,
including when one metadata file was already written.

### `nvx::host::config`

`VmConfig`: the configuration of a VM as one value, its processors, its memory
in MiB, its `RootConfig`, the `View` that its workload has of it, its
`NetworkConfig` if it has a NIC, the `ExportPlan` of the host paths that it
exports, if any, the `ConsoleAttachment` of its boot console, a `Listener` for a
client or `Disconnected`, whose device exists either way, and a `Boot`, what
only a cold boot takes.

A root is `Blockless`, the guest's own system from its initramfs, or `Layered`,
a `LayeredRoot`: the path of an image and its `RootImage`, EROFS or a GPT disk,
with the identity that the guest expects, an optional bound of a GPT disk's
first partition, the `OverlayMode`, and the `UpperConfig` of the overlay's
upper, a scratch image with its `Journal` policy, or RAM up to a cap of 1 to 90
percent of the VM's memory, 50 when the caller has no other
(`UpperConfig::DEFAULT_MAX_MEMORY_PERCENT`).

A `NetworkConfig` holds the `NetworkPolicy` that OpenVMM enforces and the NIC's
address, an `nvx::net::Ipv4Cidr`, if the caller names one: without one, a cold
boot takes `DEFAULT_GUEST_NETWORK`, `10.0.0.2/24`, and a restore the
snapshot's. A `NicConfig` is the NIC that a VM
settled on, with its address. A `Boot` holds the caller's own kernel
command-line tokens, to which NVX adds those of what it lowers: a VM that boots
needs one, and a VM that a snapshot restores takes none.

`VmConfig::kernel_command_line` checks the configuration and requires `Boot`,
then renders the root's tokens and settings, the view's tokens, and `nvx_maps`
when an export exists, followed by accepted `Boot.command_line` text unchanged.
It reads nothing on the host. A restore has no kernel command line from this
method and fails with kind `Unsupported`.

`VmConfig::check` checks the whole value without reading the host: it opens no
path and checks no support of the host. It fails with an `nvx::Error` of kind
`Unsupported` that names the first refusal: no processor or no memory; a layered
root without an image or a scratch path, with a bound of an EROFS image or of 0,
or with a RAM upper's cap outside 1 to 90 percent; a hostname that an activation
record refuses; an export that `ExportPlan::check` refuses for the guest targets
that the root reserves; a boot's capture that `CaptureConfig::check` refuses;
or caller boot tokens that use a reserved name. The network's policy is checked
where it is lowered, with the guest's address.

The reserved boot names are the root's `nvx_layer`, `nvx_scratch`,
`nvx_overlay_upper`, `nvx_overlay_mode`, `nvx_scratch_journal`,
`nvx_first_partition_bound` and `nvx_ram_upper_percent`; the view's
`nvx_hostname` and `nvx_machine_id`; `nvx_maps`; and every name beginning
`virtfs_`. The check reads tokens as both Linux and the guest's init read them,
with dashes and underscores matching as Linux matches parameter names.
Reserved names stay reserved after `--` and when init's splitting exposes them
inside quoted text. Every other token stays caller-owned, including
`nvx_entrypoint`, `nvx_arg` and `nvx_share`.

`VmConfig::mapping_table` returns exactly the configured
`ExportPlan::guest_table`, in its order, without reading host paths or
revalidating the plan. Without an export it returns an empty table and the
kernel command line omits `nvx_maps`; with an export the token counts those
entries. The table's `HostMap` and its wire codecs are accessible through
`nvx::protocol::maps`, and remain re-exported from `nvx::protocol`.

`COMMAND_LINE_BUDGET`, in `nvx::host::config`, is the 1,024-byte x86 kernel
command-line budget, including the terminating NUL. The CLI checks the command
lines that it launches against it.

The internal whole-config renderer used by `vm::create` renders a cold boot's
OpenVMM arguments from `Runtime` and `VmConfig`. Its borrowed control endpoint,
report path, and optional boot-console endpoint are already chosen output names, not additional
launch settings. It opens no path, allocates no endpoint, and starts no process.
It renders processors, memory without narrowing it, kernel and initramfs paths,
the kernel command line, layered-root block devices by path, network and export
arguments, console attachment, the CPU profile, and every `LaunchOptions` value.
A configured NIC remains attached even under deny-all policy. A listener needs
its supplied console name; a disconnected attachment retains the device and
discards its output. Listener names are nonempty and contain no NUL, comma, or
equals sign, which OpenVMM's serial options cannot hold literally.
The renderer emits neither `--microvm-lifecycle` nor
`--microvm-workload-identity`. Callers inspect the complete arguments through
`Prepared::arguments()`.

For a capture, `CaptureConfig` supplies the processor capacity, the boot-online
processor limit, and the boot memory and its capacity; the ordinary VM's cold
resource counts need not match. Boot memory must be aligned to 128 MiB.
The kernel text omits root, view, mapping and workload tokens and admits only
OpenVMM's named platform allowlist. Caller-supplied tier, processor, console,
device and network tokens are refused by name even when allowlisted: the
builder owns them. OpenVMM adds exactly one `nvx_snapshot_tier=platform` from
`--snapshot-tier platform`; callers may not supply or duplicate it.
Non-Unicode export child roots are still refused rather than lossily rendered.

`VmConfig::check_command_line()` checks the configuration's known arguments on
every host, before any host change, with 2048 UTF-16 code units reserved for the
runtime's files, launch options, and VM output names. The reserve foresees file
and endpoint names of up to 260 code units, not arbitrary runtime paths or
CPU-profile strings. The whole renderer checks the complete actual command
line without a reserve too. `process_command_line_units`, in
`nvx::host::config`, counts the Windows command line that runs a program with
its arguments, in UTF-16 code units, as Windows quotes them, with the
terminating NUL, and `fit_process_command_line` checks it against
`PROCESS_COMMAND_LINE_LIMIT`, on every host, with the reserve that its caller
keeps.

A `Boot` may also hold a `CaptureConfig`, what a capture of the VM adds to the
VM's configuration, which keeps the rest, such as the NIC, the console, and the
export: a `DiskCapture`, so far only `Fresh`, the `BlockGeometry`, length and
block sizes, of the placeholder of the image and of the dormant scratch that a
fresh capture of a blockless VM attaches, which each restore replaces with its
own; its processors and its memory in MiB, each a `Capacity`, what the VM boots
with and what a restore may grow it to; and the caller's `Invariants`, at most
64 settings that NVX keeps without interpreting them, each named by 1 to 64
lowercase ASCII letters, digits, `.`, `-`, and `_` from a letter, with a value
of at most 256 printable ASCII characters. `Invariants` and `BlockGeometry`
belong here.

`CaptureConfig::check` checks a capture without reading the host. It fails with
an `nvx::Error` of kind `Unsupported` for processors at boot or a capacity other
than 1, 2, 4, or 8, more processors at boot than the capacity, no memory at
boot, a memory capacity below it or not a multiple of 128 MiB, or a disk whose
geometry the template's manifest would refuse: a logical block size that is not
a power of two of at least 512 bytes, a physical one that is not a power of two
of at least the logical one, or a length that is not a nonzero multiple of the
logical one.

`CaptureConfig::identity` returns the `CaptureIdentity` that names the
snapshots of a capture of a VM of a `VmConfig` on a `Runtime`: the SHA-256
digest, after the domain `nvx capture` and a NUL, of a fixed encoding of what
tells captures apart, which displays and serializes as 64 lowercase hexadecimal
digits. It identifies the captures that NVX supports, fresh captures of a
blockless VM, from the values alone: it reads nothing on the host and refuses
no configuration, which creating the capture checks. It digests the runtime's
identity, the versions of this crate's contracts, the hypervisor, and the
requested CPU profile; the capture's disks, processors, memory, and invariants;
and the VM's NIC's address, `DEFAULT_GUEST_NETWORK` when the configuration
names none, its console attachment, whether it exports host paths, and its
boot's command line. No path, launch option, view, network policy, or anything
of the VM's root is part of it, nor the VM's own processors and memory, for
which the capture's count.

### `nvx::host::restore`

`RestoreMachine::new(&ActivationRecord)` latches one canonical record and its
transaction. After `Spawned`, its actions await the restore-ready event,
connect a verified control session, and send the same activation bytes on
every session. `ActivateSent` marks a completed write before waiting for
Ready; a caller that combines the write and Ready read may supply `GuestReady`
directly. `GuestReady` must echo the transaction. A lost session offers
`Connect` again, with no built-in retry count, delay or deadline; the caller
may instead feed `Expired` or `Cancelled`.

The caller drives every I/O operation and peer check and supplies verified
`Exited` events. `OperationFailed` keeps its explicit `RestorePhase`,
`RestoreEvidence` and original error source in `RestoreError`; expiry,
cancellation and exit fail every pending phase. Failure stops no process and
writes no record. A matching Ready ends at `Done`, without sending mappings or
Start. Terminal results are offered once and absorb later inputs.

NVX's own binaries drive the machine with a blocking driver, private to the
crate, beside the control session. It checks that the restore-ready client and
the control server are the VM's held process, sends the same activate message,
the record and the workload's identity, in at most three sessions, and allows
60 seconds to Ready, then 30 more for the mapping table and the workload's
Start. The caller's cancellation and the VM's exit end any pending accept, read
or write.

`ReadyListener` owns a restore-ready endpoint: a mode-0600 Unix socket in the
private VM directory on Linux, or a fresh first-instance named pipe with an
owner-only protected access control list on Windows. Its public `receive`
checks the connecting client's process ID against the held VM process. On
Linux it also checks the socket's original peer through `SO_PEERPIDFD`, so
a reaped client's queued event cannot impersonate a VM that reuses its ID.
This requires Linux 6.5 or a backport of that option; if the host cannot
provide it, receive fails with `ErrorKind::Unavailable`, without a numeric-PID
fallback. An exited held VM is refused too.

`into_std()` transfers the same nonblocking `UnixListener` on Linux, or the
same overlapped first pipe instance as an `OwnedHandle` on Windows, without
reopening or duplicating it. Transfer precedes acceptance; transferring after
the receiver accepted a client fails with `ErrorKind::Unsupported`. A caller
that drives the native object owns its I/O. On Linux it checks the original
socket peer through `SO_PEERPIDFD`, refuses an exited peer, and matches its
credentials against the held VM process: opening a pidfd from the cached
numeric PID is not authentication. On Windows it checks the named-pipe client
process ID. After matching the peer, it checks `Process::has_exited` and
refuses an exited VM. Only `vm::create` constructs a listener, for a restore,
and hands it over through `VmmCommand::ready` or `Launched::ready`.

The Linux PID-reuse regression runs a separate helper in private PID and mount
namespaces with Bash as its reaper. If the host refuses unprivileged user
namespaces, it uses passwordless `sudo` only for that isolated helper, not the
test suite or build. If neither route works, the test fails; it never changes
host namespace policy or skips the identity assertions.

### `nvx::host::network`

`NetworkPolicy` and `openvmm_arguments`, the one lowering of a network posture
into OpenVMM options, for the CLI and external orchestrators: the egress default
and rules, ingress, `HostLoopback` access, denied, `Outbound` from the guest
alone, or both ways, and its forwards.

A proxy-only policy, `NetworkPolicy::proxy_only`, has one way out, a
`NetworkProxy` that listens on a port of host loopback, which the guest reaches
at the same port of its gateway, and whose URL as the guest sees it
`NetworkProxy::url` gives; the lowering refuses a proxy beside any other way
out.

Egress rules are lowered exactly into OpenVMM's IPv4 and IPv6 rules by the
lowering of egress policy files, with at most 256 rules for each action.
`NetworkPolicy::native_egress_rules` returns a policy's lowered rules, a
`NativeEgressRules` of its allow and deny lists, without reading anything on
the host, and fails with an `Unsupported` error for a malformed rule or for an
action that needs more than 256 rules.

### `nvx::host::network::policy`

Egress policy files: `compile_policy_file`, `compile_policy_bytes`, and
`compile_policy` compile one to a `CompiledEgressPolicy` of native rules; see
[Egress policy files](#egress-policy-files).

### `nvx::host::filesystem`

The planner of every NVX caller: `plan` turns an `ExportRequest`, the mapped
host paths at explicit guest targets or at those of the host-path convention,
their denied, allowed, and write paths, and the owner, into the `ExportPlan` of
host paths through OpenVMM's aggregate virtio-fs device, for a root's
`ReservedTargets`, the blockless root's or the container root's: its children,
its ordered binds, which give the host-mapping table, and the identities that
each start checks again; and `guest_path` and `resolve_guest_path` (also at
`nvx::`).

### `nvx::host::nvxs`

Compatibility paths for `HostConnection` and its retained cause details, now
owned by [`nvx::host::transport`](#nvxhosttransport).

### `nvx::host::transport`

`connect_verified` opens and holds the recorded process by its boot, ID and
start time, retries while the endpoint is unavailable until the caller's
deadline, and checks its server before authentication: Unix peer credentials
and the original socket peer's live pidfd on Linux, and the named-pipe server
process ID on Windows. Linux requires `SO_PEERPIDFD` (Linux 6.5 or a backport)
and refuses unverifiable peers rather than falling back to a numeric PID.
An indeterminate process check remains an error, never exit proof.

`HostConnection`: the host's role of the NVXS session over a platform transport,
on which NVX's own control session and other host applications run their
protocols.
It attaches with the launch capability, waiting while the broker asks it to,
sends application bytes in as many DATA records as they take, and returns each
DATA payload from the guest, with deadlines; a receive that times out keeps a
partial record.

These APIs return the crate's `Result`: a deadline is `TimedOut`, an exited
recorded VM `Exited`, a refused, closed, reset or broken session `Protocol`,
and another transport failure `Io`. Original I/O and protocol causes are
retained directly; attachment refusal, reset and ended-connection details
remain available as sources.

`Transport` and `TransportHandle` re-export the types of the crate's private
platform layer, with one definition beneath the OS adapters. Timed reads and
writes, native-handle duplication and `unread` are unchanged: Linux reports
Unix `SIOCOUTQ` charges, and Windows reports `None`. The old
`session::connect`, private to the crate like the session, remains a
compatibility wrapper for the CLI.

A `Transport` to an OpenVMM control endpoint reads and writes with deadlines,
and tells, through `Transport::unread`, what the endpoint has not read yet of
what it wrote, where the platform tells: on Linux, what `SIOCOUTQ` reports of
its Unix socket, the kernel's charge for the buffers of each write until
OpenVMM read each whole, which a read of part of a buffer leaves as it was, and
which is zero once OpenVMM read everything. A named pipe tells its writer
nothing of the kind, so on Windows, and for a transport that does not tell, it
is `None`.

On Linux and Windows, `Transport::try_clone_handle` optionally returns an
owned native handle to the same peer-verified connection, not a reconnect:
an `OwnedFd` for the Unix socket or an `OwnedHandle` for the overlapped pipe.
An adapter must drop the blocking transport before using or registering the
duplicate with an I/O runtime, or changing the shared blocking mode. The
default is `Ok(None)`, so transports without native handles need not implement
it, and NVX does not depend on an asynchronous I/O runtime.

### `nvx::host::process`

Verified process references. A `ProcessRef` names one process by its ID, its
start time, and the `BootId` of the host boot during which it started.

`Process::open` returns `Opened::Running(Process)` only after checking the
recorded boot, ID, and start time. It holds the process through a pidfd on
Linux or a process handle on Windows, whether or not the caller started it,
so that waits and termination never reach a process that reuses its ID.
`Opened::Exited(Exited)` proves that the recorded boot ended or no running
process matches both its ID and start time. A process whose identity this
user cannot check makes `Process::open` fail rather than report an exit.
Linux hosts without pidfds poll the reference instead, and cannot terminate
the process.

On Linux and Windows, `Process::exit_handle()` returns
`Result<Option<ExitHandle>>`: an independently owned duplicate of the pidfd
or process handle, or `Ok(None)` only on a polling-only Linux host. Callers
register the standard owned descriptor or handle with their own runtime;
dropping the original `Process` or another duplicate leaves it valid.
On Windows, the duplicate retains the original handle's access rights.
Duplication failures have kind `Io` and retain their original I/O source.
No watcher thread, future or callback is created by the library.

`Process::wait(deadline)` returns `Some(Exited)` only after verified exit, or
`None` when the deadline passes without confirmation. `Exited` proves exit,
not an exit status, and cannot be made by a caller.

`Process::bytes_read` reports how many bytes the process has read through the
operating system's read calls, whether the host's cache served them or not:
`rchar` of `/proc/<pid>/io` on Linux, which it reads only while the process
still runs with its reference's identity, and the `ReadTransferCount` of
`GetProcessIoCounters` on Windows. `own_bytes_read` reports the calling
process's. Neither counts what a process maps from a file, as OpenVMM maps the
memory of a snapshot that it restores, nor, on Linux, what it reads through
io_uring, as OpenVMM reads a sandbox's block devices. On Linux, the count also
holds what the process's children read once it reaped them, as the kernel
accounts them to their parent, while Windows counts the process's own reads
alone.

### `nvx::host::report`

OpenVMM's outcome report. The Linux and Windows primitives are private to the
crate, in `host::platform`, and so is the authenticated control session, which
carries NVX's own control protocol, `NVXC`, one frame per DATA record of a
`HostConnection`: the CLI and its restore driver use it, with the `cli`
feature. The session speaks version 2 alone, and requires the guest's `READY`
to name that version. It streams an execution's standard input, with an
explicit end, and keeps at most 64 KiB of it that the guest has not
acknowledged writing, and receives the execution's output without a total
limit; a `STOP` ends an execution that runs.

`report::read` returns `Ok(None)` for a missing report, and an `Io` error for an
unreadable or malformed report, retaining the underlying error. It keeps today's
summary of the outcome category, status code and teardown steps, not free-form
fields.

### `nvx::host::sandbox`

The launch profile of `nvx-rs sandbox`, with native input validation: the
workload identity that `--workload-user` names, the live shares, and the checks
of the options. The image staging of the profile is private to the crate, for
the CLI (feature `cli`): `layer`'s `SandboxLayer`, the distro layer of a
sandbox, an EROFS layer or a GPT disk, the one layer that a sandbox takes;
`distro`'s `DistroBinding` of the layer, which explicit verification makes, and
its check from metadata alone; `scratch`'s `ScratchBinding` of a scratch image,
its identity, kind, and length, and its check from metadata alone, which claims
the image for one sandbox at a time; and the seals of `nvx::host`'s private
`seal`, which they record. See [Distro bindings](#distro-bindings) and
[Images by path](#images-by-path).

`mount`: `SandboxMount` requests of live shares of host directories and files,
their guest targets and the reserved ones, their access policies and owner, and
the checks of their host paths; and `plan_mounts`, which plans them through
`nvx::host::filesystem` for the container root.

`options`: the checks that `nvx-rs sandbox` and `nvx-rs run` apply to their
options: the attribution of `--mount-deny`, `--mount-allow`, and `--mount-write`
to shares, the systemd entrypoint check and the explicit verification of the
distro layer, the targets of `nvx-rs run` shares, and the managed exec
environment; see [Sandbox input contract](#sandbox-input-contract).

### `nvx::guest`

The programs that run in Linux guests; see [Guest binaries](#guest-binaries).

### `nvx::guest::mappings`

The mapping values, `Mapping`, `Access`, `Kind`, `InvalidMapping`, and
`MAX_PATH_BYTES`, and `MappingTable` are public. The mapper, in the `mapper`
module, is part of the guest's space, nvx-init's internal reference code,
private to the crate, which re-exports the items that NVX's programs use here.

`Mapper`: applies host mappings beneath the
descriptor of a `TargetRoot`, the root of a mount of a whole file system, which
`statmount` tells from a bind of a directory below it, such as a layered root's
overlay or a guest's `/`, from a `SourceRoot`, such as the guest's mount of the
host's export.

A `Mapping` names a relative source, or an empty one for the source root itself,
such as a directory export's root, which the mapper opens as `.` beneath it with
the same flags, an absolute target, its `Access`, and optionally the `Kind` that
its source must have.

The mapper opens each component with `openat2`, through no symbolic link or
magic link, creates what a target lacks only on the root's own mounts, which
`statx` tells apart by their unique mount IDs, never inside another mapping, and
checks kinds, overlaps, and the targets that the root profile's `Reserved` set
holds, with the `/run/nvx/time` tree and its ancestors always.

It attaches each mapping as a detached copy of its source's mount, `open_tree`,
restricted `nosuid`, `nodev`, and read-only when asked, `mount_setattr`, before
`move_mount` puts it at its target, then checks that it is there. Zero mappings
do no work; the first that fails is reported with its index as an `ApplyError`,
and latches the mapper.

A layered root's teardown, which unmounts its mapper's mappings, first detaches
a mapping that a rename moved and that the attach failed to detach, then
unmounts the mappings in reverse order, and reports each failure.

A runtime bind, such as the view's machine ID, goes through the same walk and
attach.

`MappingTable` keeps a guest agent's account of a host mapping table: the
mappings that the host hands the agent in `MAPS` parts once it is ready, after
it announced how many, with the `nvx_maps=` token of the kernel command line,
which `MappingTable::announced` reads, or as an adapter's own protocol has it,
which `MappingTable::new` takes. `MappingTable::apply` checks each part against
the announcement and the entries applied in order, and refuses one that does not
decode, holds no entry, does not continue the table, ends past the
announcement, or maps the export's root, which an empty source names, as a
`TableError::Refused` at the entry that it expects next, which changes nothing.
It hands the part's mappings to the caller, which applies them beneath the root
of its choice, such as through its own mapper; the first that
fails latches the table, which reports it as a `TableError::Failed` with its
entry, and refuses every later part. The table is complete once every announced
mapping is applied, and none failed, and the caller's teardown tears it down.

Linux 6.8 or later. NVX's managed agent applies its host mapping table through
a `MappingTable` and a mapper, beneath `/` in direct mode; the table refuses an
empty source, since the export is an aggregate, whose root lists its children,
and NVX's hosts, whose exports are all aggregates, send none. nvx-init's
layered root mounts its live shares through a mapper.

### `nvx::guest::nvxs`

`GuestConnection`: the guest's role of the NVXS session over a byte stream, such
as the control console, on which NVX's managed agent and other guest agents run
their sessions. It never reads past the record in progress, so a caller that
polls the stream between records sees every record that it has not returned.

### `nvx::guest::terminal`

`Terminal`: the terminal on which a guest agent serves its session, such as the
VM's control console, as a byte stream. It opens the terminal, waiting while the
kernel sets it up during boot, puts it in raw mode, so that it passes the
session's bytes as they are, and drains it, so that it sends what the agent
wrote before the agent ends the VM. NVX's managed agent and other guest agents
serve their sessions on it.

### `nvx::guest::time`

x86_64: `TimeAbi`, the boot steps of NVX time ABI v1 as typed calls in the
caller's process, for any guest init or agent: the cold-boot time step and,
separately, the start of the time daemon in a child process that the caller
supervises, and the VM generation ID.

The ABI's capture request, which repairs the time of a restore, and the
restore's finish are private: `nvx::guest::snapshot::capture` runs them, and
hands the restore's fresh entropy to its repair in memory, as an owned `Seed`,
while `nvx-time capture` writes it to a file.

A violation of the ABI stops the VM in enforcing mode, with the time ABI's
status, as `nvx-time` does; in strict mode, `TimeAbi::strict`, which a caller
that owns the VM's exit status and shutdown selects, the call fails with the
`Violation`, whose `status` is the one with which enforcing mode stops the VM,
and the time daemon enforces the ABI all the same, while the single-call
snapshot capture below prepares strictly.

A call takes the permission for OpenVMM's ports on the calling thread for each
port transaction alone, and fails with `TimeError::PortRelease` if the thread
may still hold some of it, which a process that it started would inherit, and
from which NVX's guest programs start none. No call waits for a child that it
did not start.

`nvx-time` runs its steps through them.

### `nvx::guest::snapshot`

x86_64: `capture(&Devices, Disks::Fresh, deadline)` composes the platform
template's mount checks, strict time checks, stall-detector suppression,
generation ID and capture request, without inspecting or freezing the
caller's executions. `Devices` names the distro and scratch devices by their
MMIO base values and the host filesystem by its virtio-fs tag. `Mounted`,
`Origin` and `MOUNT_TABLE_LIMIT` describe a mount refusal.

Every step and wait before the request, including runtime-lock acquisition
and rollback, shares the caller's absolute deadline. The request itself has
no deadline while the host saves stopped processors. If no snapshot was
taken, `Captured::Continued` reports that the detectors were restored.

`CaptureFailure` reports its `stage`, `cause` and `disposition`: `Resumed`
only when no suppression owned by this call remains, or `MustEnd { status }`.
A strict time violation preserves its actual status. A `PortRelease` remains
the cause even if detector rollback also fails. Request and release failures
are final.

After the time core's existing clock correction, `capture` runs the clone's
existing repair inline. `Captured::Restored(Repaired)` returns only once its
processor and memory targets, reseed and finish completed. `Repaired` reports
the generation ID, CPU count, memory size and acknowledgement instant; every
repair failure is `MustEnd` with the existing `RepairError`'s status and detail.
Fresh platform restore packets must carry no network settings; even an
otherwise empty network policy is explicitly refused.

The caller owns its transport. After answering the capture request, send
nothing until `capture` returns. After `Continued` or a `Resumed` failure,
report on the same session. After `Restored`, take the session's RESET, then
call `GuestConnection::acknowledge(window)` on that session before receiving
the activation record.

### `nvx::guest::activation`

x86_64: the activation of each clone that a restore of a template's snapshot
starts, once `nvx::guest::snapshot::capture` repaired it, for a guest init or
agent, whether or not it is PID 1.

`nvx::guest::clone` remains a compatibility alias. Activation itself holds no
admission fence and starts no process.

In a clone, the capture repairs the restore before it returns: it checks that
the online CPUs are a prefix that the restore's processor target does not
undercut, brings the target's other CPUs online through sysfs, and verifies
them; with a memory target, it checks, before it adds any memory block, that
the kernel's block size is 128 MiB and that the restore's ranges continue the
captured RAM in logical order, with no gap, overlap, or range in the MMIO
aperture, while the host bounds the target by the snapshot's capacity, then
adds and onlines the blocks through sysfs, as `nvx-snapshot` does, and verifies
them; it reseeds the kernel's random number generator with the `Seed`, having
drawn no randomness before, and drops it; and it finishes the restore through
the time ABI, with the CPUs that the restore brought online and the
acknowledgement when the host gates its input until then, which hands the
deferred time work to the time daemon that the caller supervises.

It returns the `Repaired` clone, with its generation ID, its online CPUs and
memory, and when it acknowledged the restore. It adds no memory block twice,
and reseeds and finishes at most once.

Each failure is a `RepairError`, with its `RepairStage`, its `RepairCause`, and
the repair's `RepairProgress`, and `RepairError::status` selects the status, of
`nvx::protocol::status`, with which the caller ends the VM.

The capture and the repair take the time ABI in strict mode, so that a violation
of it fails them, with its status, rather than stopping the VM, and only the
time daemon enforces it.

An `Activation` of the `Repaired` clone, or, with `Activation::with_resources`,
of the resources and the instant of the acknowledgement that a repair reports,
as in an adapter's tests, then takes
the clone's own configuration from the host, in one Activate, on a restricted
control session: a state machine without I/O, independent of any framing, to
which the adapter passes what its session receives, as an `ActivationInput`, and
which returns the `ActivationAction` that the adapter takes.

From its transport bootstrap, which takes the reset that the restore queued for
the NVXS session, the restricted session takes only flow control and one
Activate, and anything else, a request or another reset, fails it; the
activation decodes the Activate's record with `ActivationRecord::decode`, and
accepts only a clone's, with a restore section, that expects the CPU count and
the memory size that the repair reports, before any side effect, while an
Activate whose record the adapter's framing refused unread, as longer than a
record can be, fails as a record that long does; the adapter applies the record
in its own space, by the activation's deadline.

nvx-init applies it with its own space, NVX's internal reference code, private
to the crate, as at a cold boot: it assembles the root with the record's image,
an EROFS image whose UUID it checks if the record expects one, or a GPT disk
whose GUID it checks if the record expects one, its upper, a tmpfs or the
scratch with the record's journal policy, and its overlay mode, and with the
inputs that the guest's own configuration holds, which its template captured:
the runtime directory, the block devices, the timeout, the host filesystem
device's virtio-fs tag, and the gateway of the template's NIC, if it has one,
but waits for no device past the activation's deadline, so that the
application ends by then.

For a GPT disk, it first asks the kernel to reread the disk's partition table
with `BLKRRPART`, since the kernel created the block devices of partitions for
the template's placeholder. Whatever the image, it requires a fresh upper, whose
`upper` and `work` directories are missing or empty, in a bounded check that
reads no file, while a cold sandbox's scratch keeps its upper across boots. It
uses no execution identity from the record, which carries none: each execution
names its own identity, as in a sandbox without a workload identity at a cold
boot. With the record's export, it mounts the host export through that
filesystem device, read-write, at `hostfs/root` in the runtime directory,
outside the root, since the record names no mode and OpenVMM limits each child
to its own access. It applies the record's VM-wide view, with the record's own
DNS server. The view sets the hostname and, when requested, generates a machine
ID after repair's reseed. The root owns the machine-ID and resolver binds,
including those made before a later failure. Application starts no announcer
and no other process. nvx-init keeps the root and workload that it activated:
the export's root, from which it maps the host's mapping table once the clone
is ready, the network's proxy endpoint, which it tells its workloads as at a
cold boot, and the publication and gateway, from which it starts and supervises
ingress. Their teardown closes the export's root, then tears the root down with
its view binds. A switched root unmounts nothing. After a failure, of the
export or the network among others, nothing that the application mounted or
bound stays mounted unless the failure reports the unmounts that failed.

Once the adapter passed the activation what the session received while the
application ran, which came before Ready, `Activation::complete` returns the
`ReadyActivation` that Ready reports beside the adapter's own
identities, that the activation applied, with the record's transaction ID, after
which the host's mapping table and start follow in the adapter's order, as after
a cold boot, and `Activation::start` records the start.

A retry with the same transaction ID and a byte-equal record joins the running
activation, or gets its cached result, also in a later session once the clone is
ready; a changed record, another transaction, another reset before Ready, or a
failed activation is final.

Its deadline, which the caller chooses, and which neither input nor retries
extend, starts at the restore's acknowledgement, and the adapter ticks it
however its input trickles and its output blocks. Each failure is an
`ActivationFailure`, with its `ActivationState` and `ActivationCause`, and never
powers the VM off: the adapter ends it.

### `nvx::guest::reseed`

`Seed`, `Seed::read`, and `reseed`: a restore's fresh entropy, which starts with
its `GenerationId`, and the reseed of the kernel's random number generator with
it, in the caller's process. A `Seed` keeps its bytes in one allocation, which
no move copies, zeroes them with volatile writes when it is dropped, can't be
cloned, and formats none of them. `nvx-reseed` runs them.

## Egress policy files

`nvx::host::network::policy` compiles the egress policy files that
`nvx-rs run` and `nvx-rs sandbox` take with `--network-egress-policy-file`
([format](../doc/run.md#run-openvmm-directly)). It replaced the former
`scripts/nvx_tools/egress_policy.py` when `nvx.py` delegated to `nvx-rs`, and
compiles the policies that it compiled to the same rules, but reads policy
files as strict JSON
([microsoft/nvx#729](https://github.com/microsoft/nvx/issues/729)).

- A policy is at most 1 MiB of UTF-8 JSON, as RFC 8259 defines it, whose root
  has only `allow` and `deny` arrays: `NaN`, `Infinity`, numbers beyond the
  range of a double, a repeated field, and arrays and objects nested more than
  32 deep are errors. A port is an integer, written without a fraction or an
  exponent.
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

`openvmm_arguments` lowers `NetworkPolicy` rules the same way: an
`EgressRule` is a policy file's MXC rule, its messages name
`EgressRules::name` where a file's name `allow` or `deny`, such
as `network.egress.allow[0].to[1].cidr is not a valid IPv4 or IPv6 CIDR: "x"`,
and the checks that only `NetworkPolicy` has, such as the host-loopback ones,
stay in `openvmm_arguments`. `nvx-rs sandbox` reads the rules that a policy
file compiles to, like those of `--network-egress-allow` and
`--network-egress-deny`, back into `EgressRule`s as OpenVMM reads native rules,
and lowers them again with the rest of its policy, which renders compiled rules
unchanged.

A message names the field that breaks the format by its path, or the file, and
says why, quoting values as Rust does, as in
`allow[0].ports[0] has duplicate field "port"` or
`egress policy file policy.json is not valid JSON: expected value at line 1
column 1`. Messages name files as they were given.

The tests take their expectations from the format, rather than from what the
Python compiler did, which a fixture recorded until
[microsoft/nvx#729](https://github.com/microsoft/nvx/issues/729) retired it. A
reference model of policies, written from the format that
[`doc/run.md`](../doc/run.md#run-openvmm-directly) documents, decides which
packets the rules of an action match, and which packets the native rules that
it lowers to match. Properties then hold for the policies that a `proptest` port
of the generator of the fixture's random corpus draws, and for networks at the
ends of each family's addresses: each action lowers to canonical native rules,
none redundant, that match exactly what the model matches, at one point of every
cell that the boundaries of the networks and port ranges delimit; an action is
refused exactly when it needs more than 256 native rules; the order of the
rules, repeated rules, and networks split into their halves change no lowering;
`allow` and `deny` lower independently; and policy files, their values, and
`NetworkPolicy` rules compile alike and deterministically, or fail with an
error. A test of each rule of the format's grammar checks the field path, or the
file, that each refusal names, a fragment of its reason, and its error code.

## Distro bindings

Three separate checks guard the distro layer of a sandbox, and only explicit
verification reads it. The bindings and the seals that they record are private
to the crate, for the CLI:

- `options::validate_sandbox_entrypoint` refuses the entrypoints of systemd,
  which the sandbox security profile does not run. Every launch applies it,
  whether or not the layer has a manifest, and it reads nothing.
- `DistroBinding::verify` verifies the layer explicitly. With a manifest,
  `LAYER.manifest.json`, beside the layer, it reads the manifest, hashes the
  whole layer, compares the digest with the manifest's `artifact_sha256`, and
  then refuses a `systemd` package, in the order and with the messages of
  `nvx.py`. A layer without a manifest passes unread. It reads each file
  through the file that it opened, and fails if the file's metadata changed
  meanwhile.
- `DistroBinding::check` compares the layer and its manifest with a binding
  from their metadata alone, and returns the layer, open, as a
  `CheckedDistro`. It never reads, maps, or hashes either file.

A `DistroBinding` ties the `FileSeal` of the layer to the seal of its
manifest, or to the manifest's absence, to the revision and result of the
distro policy, `DISTRO_POLICY_REVISION`, and to the layer's format and the
identity that the guest expects of it: the UUID of an EROFS layer, or the disk
GUID of a GPT disk, which `SandboxLayer::gpt` names. It comes from explicit
verification. `check` fails for a binding of another version, layer, format,
or policy revision, a verdict that rejects the layer, a layer whose identity,
kind, length, or times changed, and a manifest that appeared, disappeared, or
changed. A binding of a layer without a manifest holds only while nothing is at
the manifest's path: unlike verification, which finds a manifest as
`Path.exists` does, `check` refuses a path whose lookup fails for another
reason than a missing file. A missing or stale binding fails: no runtime
operation registers, verifies, or hashes a layer on its own. Hashing belongs
where layers are built, installed, or staged, and to diagnostics.

On Linux, a seal holds `st_dev`, `st_ino`, the file type, `st_size`, and
`st_mtime` and `st_ctime` with their nanoseconds; on Windows, the volume serial
number, the 128-bit file ID, the kind, the length, and the last write time. A
seal detects accidental change, not deliberate tampering by an account that can
write the sealed files. Callers therefore stage layers and their manifests
read-only, in a directory that only their own account can write, never rewrite
them in place, and bind a replacement anew. The checked layer is a plain owned
`File`, which no child process inherits: an owned descriptor on Linux, and on
Windows an owned handle that denies other opens for writing or deleting the
layer, so that nothing modifies, renames, or replaces it while it is held.
Read-only opens, such as OpenVMM's open of a sandbox's layer, share it.

`nvx-rs sandbox run` and `provision` verify the distro layer in their
preflight, at the path given. `provision` records the layer at its resolved
path, as it records every path, with the binding of the layer there, under
`distro_binding` in `config.json`. When the path given is a link to a layer of
another name, `start` checks the manifest beside the link's target, so
`provision` verifies the layer there as well, and it records only a binding
that the check accepts there. `start` checks the binding just before it
launches OpenVMM, which opens the layer at its path; see
[Images by path](#images-by-path). `run` and `start`
launch the image of the format and the identity that the binding records, an
EROFS layer or a GPT disk, over a scratch or a RAM upper, the layered root that
nvx-init assembles. A configuration
without a binding, as `nvx.py` wrote, fails with `sandbox configuration has no
distro binding`, and a stale binding with what changed, such as `LAYER changed
after its distro binding was made: its length differs`; both add `deprovision
the sandbox and provision it again`, since a new provision verifies the layer
again.

## Images by path

A launch names its images by path: OpenVMM opens the distro layer and the
scratch image at the paths that the VM's configuration holds. Their checks,
`DistroBinding::check` and `ScratchBinding::check`, find a staged image
unchanged from its metadata alone and return it open, but what takes an image's
path after its check is what OpenVMM opens. Callers therefore stage their
images in a directory that only their own account can write, as
[Distro bindings](#distro-bindings) and the docs of the crate's private
`host::sandbox::distro`, `host::sandbox::scratch`, and `host::seal` require, so
that no other account can replace a checked image. On Windows, a checked distro layer also
denies other opens for writing or deleting the layer while it is held, and the
scratch image's guard denies deleting and renaming the image. `nvx-rs sandbox
start` and `run` check the images just before they create the VM, and hold the
checked files until the guest is ready, or, for `run`, until its VM has exited.

`nvx-rs sandbox` rejects disk image paths containing commas, semicolons, or
improperly nested square brackets with `ErrorKind::Unsupported`, including the
resolved paths that `provision` records. Balanced brackets, as in
`layer[1].erofs` or `layer[[1][2]].erofs`, are supported; an unclosed `[`, an
unmatched `]`, or reversed `][` is not. `run` and `provision` apply these checks
before launching OpenVMM or recording a sandbox, and `start` checks recorded
paths again.

The scratch image, which the VMM writes, has a binding of its own, a
`ScratchBinding`: its platform scheme, its device or volume and file ID, its
kind, a regular file, and its length, which fixes the guest's block device, in
a versioned serde form without times. `ScratchBinding::bind` makes it without
reading the image, and `ScratchBinding::check` opens the image, compares it with
the binding from its metadata alone, and claims it for one sandbox at a time,
until the sandbox's VMM exits, then compares it again, since the launch whose
claim it waited for may have changed it. On Linux, the file ID is the inode
number, which a file that is created once the image is removed can take again,
so a replacement made that way, with the image's length, passes the check. A
check that finds the image claimed waits until its deadline, such as
`CLAIM_RELEASE_WAIT` from now, and then fails with `ErrorKind::TimedOut` and the
code `scratch-claimed`. On Linux, the claim is an exclusive `flock` lock of the
checked descriptor's open file description, which the VMM inherits and holds; a
process that another thread spawns holds a copy of the descriptor from its fork
until it executes its program, which the wait covers. On Windows, the check's
handle is a guard that holds no access to the image's content, shares reading
and writing, and denies deleting and renaming the image, and the check makes
sure that no other process has the image open for writing, as another sandbox's
VMM does, then opens the image for writing, sharing every access, as the claim's
handle. The VMM inherits the claim's handle, or for a restore, whose scratch
image OpenVMM opens only while no other handle may write it, the guard, which
then holds the claim: so it holds the claim from its start, even if the
launcher ends before the VMM has opened the image. Claims exclude the launches
of NVX, not a process that writes the image on its own.

Images are raw: OpenVMM attaches a file's bytes as they are, and would choose a
container format by a file's extension, which the checks of a launch refuse, so
a caller whose images use a container format, such as a fixed VHD, stages a raw
file. Staging and the checks read no content of the images.

`nvx-rs sandbox provision` records the scratch image's binding under
`scratch_binding` in `config.json`, at the image's resolved path, and `start`
checks it after the distro binding, and launches OpenVMM, which inherits the
image's claim. A scratch image that changed fails as a distro layer does, such
as with `IMAGE changed after its scratch binding was made: its length differs;
deprovision the sandbox and provision it again`, and one that another sandbox
claims with `another sandbox claims the ext4 scratch image IMAGE`. `run` binds
the scratch image itself, checks the binding and claims the image as `start`
does, and holds the claim until its VM exits. A sandbox whose upper is in RAM,
`--upper ram`, has no scratch image: `provision` records `null` for the image
and its binding, and neither `start` nor `run` binds or claims one.
## Sandbox input contract

The sandbox launch profile specifies its own inputs rather than emulating Python.
A UID or GID contains only ASCII decimal digits, without signs, whitespace,
underscores, or Unicode digits, and is in `1..4294967294`. A layer UUID is
canonical lowercase hyphenated text; braces, a prefix, uppercase, and
unhyphenated forms are refused.

API change: `nvx::host::sandbox::{Int, IntError}` are no longer exported. The
CLI's sandbox configuration records hold no resource limits: the VM is the
limit of a sandbox's workload. See [Specification tests](#specification-tests).

Managed exec environment files and distro manifests are strict UTF-8 JSON,
read with `serde_json`; non-finite numbers, numbers beyond the range of a
double, and unpaired surrogate escapes are refused, including in otherwise
unused manifest fields. Distro manifests are read as egress policy files are,
so that what they accept does not depend on the features of `serde_json`, such
as `arbitrary_precision`, which another crate of a build may enable: no object
repeats a field, and arrays and objects nest at most 32 deep. The manifest
`format` is the integer `1`, not `true`, `1.0`, or an object such as
`{"$serde_json::private::Number": "1"}`. Environment files contain an array of
strings, at most 1 MiB before parsing; each `KEY=VALUE` entry still obeys the
256-entry and 4096-byte limits. Diagnostics quote values with Rust debug
formatting and retain native JSON/I/O error details rather than Python's
`repr`, codec errors, or `json.loads` messages.

Entrypoints, workload arguments, and guest mount targets must each remain one
unquoted kernel token: no whitespace, control characters, double quotes, or
UTF-8 byte that Linux treats as whitespace (including `0xa0`). `nvx-rs sandbox
start` and `run` check the kernel command line that they launch: the caller's
`--cmdline` uses Linux's parameter splitting, including quoted values and
hyphen/underscore name equivalence, and is checked as the launched line holds
it, untrimmed, and through the guest bootstrap's whitespace-only token view
too. Reserved `nvx_*` and `virtfs_*` names cannot be hidden inside quoted
values or behind leading Unicode whitespace; a `--` or unclosed quote that
hides the tokens that OpenVMM appends is refused.

Host paths are the host's own, which `std::path` holds: spelled with their
components alone, as `Path::components` reads them, with backslashes on
Windows, and resolved by `std::fs::canonicalize`, with the components that do
not exist yet appended. Paths that are not valid Unicode are refused. The
filesystem queries are those of `std::fs`, and the identities of files those
of the crate's private platform layer: the device and inode on Unix, and the
volume serial number and the 64-bit file index on Windows.

Every host path becomes absolute through one function, which joins a relative
path to the current directory without consulting the filesystem, so it
resolves neither symbolic links nor `..` components. On Windows, share roots
and policy paths such as `D:relative` use native per-drive resolution when the
drive differs from the current directory's, and UNC paths keep their share
root. A policy path must still lie in its share when the export planner applies
containment checks.

## The nvx-rs CLI

The `cli` feature builds the `nvx-rs` executable, which runs NVX's microVMs
and sandboxes, a faithful port of the former runtime steps of `nvx.py`
([microsoft/nvx#359](https://github.com/microsoft/nvx/issues/359),
[microsoft/nvx#638](https://github.com/microsoft/nvx/issues/638), and
[microsoft/nvx#725](https://github.com/microsoft/nvx/issues/725)):

```text
nvx-rs [-h] [--root PATH] artifacts
nvx-rs [-h] [--root PATH] download [--repository OWNER/REPOSITORY] [--hypervisor HYPERVISOR]
nvx-rs [-h] [--root PATH] egress compile PATH
nvx-rs [-h] [--root PATH] run [OPTIONS]
nvx-rs [-h] [--root PATH] sandbox [run|provision|start|exec|stop|deprovision] [OPTIONS]
nvx-rs [-h] [--root PATH] snapshot capture NAME [OPTIONS]
nvx-rs [-h] [--root PATH] snapshot restore NAME [OPTIONS]
```

```bash
cd nvx-rs
cargo build --release --locked --features cli   # target/release/nvx-rs
target/release/nvx-rs run                       # the checkout's artifacts
```

Every NVX release package ships the release build as `bin/nvx-rs`, a static
`x86_64-unknown-linux-musl` executable, in the Linux packages, and as
`bin/nvx-rs.exe`, with a static C runtime, in the Windows package, with its
provenance in `provenance/nvx-rs.provenance.json`
([Package and source delivery](../doc/distribution.md)). The `nvx-rs` of an
extracted release runs as is: without `--root`, it uses the release that holds
it; see [Artifact root](#artifact-root). A checkout builds its own with Cargo,
and [`download`](#download) installs the other artifacts of a release into it,
as its first `run` does; see [Usage](../doc/usage.md#nvx-rs).

The scenarios of `nvx.py test-microvm` run the executable that `NVX_RS` names,
else `nvx-rs/target/release/nvx-rs[.exe]` of their checkout, and else build
that one with Cargo first, with its output on standard error; see
[Usage](../doc/usage.md#nvx-rs). `nvx.py` records the provenance of the builds
that it makes and installs, and `nvx.py record-nvx-rs-provenance` that of a
build made otherwise. The scenarios run their microVMs and sandboxes with
`nvx-rs --root ROOT run OPTIONS` and `nvx-rs --root ROOT sandbox OPERATION
OPTIONS`, and compile their egress policies with `nvx-rs egress compile PATH`.

Errors print `error[CODE]: MESSAGE` to standard error and exit with status 1,
as `nvx.py` reports a `ScriptError`, where `CODE` is the stable
[error code](#error-codes) of the failure. Usage errors print the parser's
usage and `PROG: error[CODE]: MESSAGE`, with one of the `argument-*` codes, and
exit with status 2, as `argparse` does, and help exits with status 0. Callers
tell errors apart by their codes and by stable fragments of their messages,
such as the names of options, fields, and files, since the wording may change.
Ctrl-C prints `Interrupted` and exits with status 130, without a code, since it
reports no error. `run`, `sandbox run`, and `sandbox exec` exit with the status
of OpenVMM or of the workload, which is no error of `nvx-rs` either. Text ends
its lines as Python's `print` ends them, with `\r\n` on Windows and `\n`
elsewhere, while workload output passes through unchanged; an error that
`sandbox exec` reports after the workload's output still starts a line of its
own, even when the workload left its standard error without a line end. Each
error is one line: a line break in a value that its message names, such as a
path, prints as `\n` or `\r`.

Usage errors and help start with the usage of the parser, on one line, such as
`usage: nvx-rs egress compile [-h] PATH`. The usages of `run` and `sandbox`,
which take many options, name them `[OPTIONS]`, as in
`usage: nvx-rs run [OPTIONS]`, and their help lists every option. A usage error
names the parser after the usage, as in `nvx-rs run: error[CODE]: MESSAGE`.

`snapshot capture NAME --store-dir PATH` captures a blockless VM before any
layered root or workload exists. It journals the launch before spawning,
requests capture through authenticated NVXC, and requires that VM's timely
exit before finishing and publishing exactly the strict snapshot's three
entries. With `--net IPV4/PREFIX`, the VM has a NIC at that address under the
complete deny-all policy, which the snapshot keeps, without the policy, so that
each restore installs a policy of its own on it. The next capture using that
store ends an interrupted attempt through ordinary launch recovery, never
resuming it. See [`snapshot capture`](../doc/usage.md#snapshot-capture) for its
resource options and recovery behavior.

`snapshot restore NAME --store-dir PATH --state-dir PATH` provisions a managed
sandbox, with the options of `sandbox provision` that a restore takes, whose
configuration also records the snapshot, its store, and the sandbox's workload,
then starts it: that start, and each later one, creates the VM with
`host::vm::create` from the snapshot, through the store's launch journal, and
drives its restore with the blocking restore driver, which activates the guest
with the start's own network policy, maps the sandbox's shares, and runs the
workload, whose output the start forwards until it exits. The restore's NIC
comes from the snapshot, which `snapshot capture --net` gives one, so the
restore takes no `--net`: the network options of a snapshot with a NIC, which
need `--network-profile`, are the restore's own policy at the NIC's address, as
for `run --restore-snapshot`, and a snapshot without a NIC refuses them before
anything is provisioned. A restore's guest accepts only a fresh upper, so a
later start refuses a scratch image once an earlier start has created its view,
as each start does before its guest can use the image. Each start holds the
snapshot's exclusive lock from before it links the snapshot's files into the
sandbox's `openvmm/` view until the start ends or the VM has exited, and
`deprovision` holds it while it removes the view; a start whose command dies
leaves a journal whose recovery ends the VM. No index of a
snapshot's sandboxes or record of a restore in progress exists. See
[`snapshot restore`](../doc/usage.md#snapshot-restore).

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
  relative to the working directory. A managed `start` refuses it, since the
  root's install metadata identifies the root's own OpenVMM alone; see
  [Runtime identity](#runtime-identity). On Windows, it cannot name a batch
  file, which Windows would run through `cmd.exe`, reading OpenVMM's arguments
  as shell syntax: `run` refuses it.
- `run`, `sandbox run`, and `sandbox start` install the release of a root that
  is a Git checkout and holds none of its artifacts before they run them,
  unless the `NVX_AUTO_DOWNLOAD` environment variable is `0`; see
  [`download`](#download).

### `artifacts`

Prints the paths of the artifacts as JSON, as `serde_json`'s pretty printer
writes it, with the members of every object sorted by name. Paths are absolute
and not checked:

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

### `download`

`nvx-rs download` ([Usage](../doc/usage.md#download)) is the port of the
former `nvx.py download`, with its options, search, checks, and messages. It
installs the package of the host's platform from the release that CI built
from the checkout's `HEAD`, or else from the nearest of up to 20 first-parent
ancestors that has one, into the repository checkout of the root: OpenVMM, the
guest artifacts, and their build provenance, replacing those of the checkout,
and removing the guest artifacts that the package does not declare. A package
declares the sandbox scratch template, `ubuntu-smoke-scratch.ext4`, that the
sandbox scenarios of `nvx.py` copy, with the `scratch_template_sha256` of the
`ubuntu` section of its `SOURCE-MANIFEST.json`, which packages built before it
shipped lack, so their installation removes a checkout's template. It
installs all of them or, if a step fails, none, and never installs `nvx-rs`,
which the checkout builds. A directory where the release has a file fails the
installation, which deletes no directory. An extracted release root is refused.

It runs host tools: `git` reads the history, from the root alone; `curl`
queries the GitHub REST API and downloads the package, speaking HTTPS alone
and following no redirect itself, so that the token of `GH_TOKEN` or
`GITHUB_TOKEN`, which it reads on its standard input, reaches only the API; and
`tar` lists the package's members, whose names must be relative and canonical
below one root, and extracts it into a private directory below `build/`. Every
extracted file must be a regular file that `SHA256SUMS` lists with its digest.
On Windows, `curl` and `tar` are those of `%SystemRoot%\System32`, whose `tar`
reads the ZIP package of `windows-whp`. `download` reads the status and the
redirect of each response from the headers that `curl` dumps, rather than from
its `--write-out`, with which the `curl` 8.8.0 of some Windows releases fails
every HTTPS request.

`run`, `sandbox run`, and `sandbox start` download the release themselves,
before they launch, when an artifact that they run is missing from a root that
is a Git checkout, with its `.git` and `SOURCE-MANIFEST.json`, that holds none
of the files that a release installs, unless `NVX_AUTO_DOWNLOAD` is `0`. They
report on standard error. A root that holds some of those files, which may be
local builds, gets a warning instead, and the missing artifact fails the
command as before; so does one in which such a file appears during the
download, which then installs none of the release: it links each file into
place, which fails where a file exists, rather than renaming it over one, and
when it moves back what it installed, it keeps a file that changed since, which
the digest that `SHA256SUMS` lists no longer matches, such as a local build's.
`run` and `sandbox run` download nothing for options that they refuse, though
they report a missing artifact before them, as they always have. One download
at a time installs into a checkout, holding the lock of
`build/.nvx-download.lock` exclusively; the operating system releases it when
its holder exits. A command shares that lock, creating its file if needed and
if it may, while it decides, reads the artifacts, and launches OpenVMM on them,
`sandbox run` from before it reads its distro layer, which may be a release's,
and `sandbox start`, until their VM is ready. It thus never sees part of an
installation, no download replaces the files that it reads, and a download
that it waited for has it decide again. `sandbox provision` shares the lock
where it can take it while it reads its distro layer and records the sandbox,
and needs no artifact root otherwise. `sandbox exec`, `stop`, and
`deprovision` share the lock while they read the runtime identity of the
artifacts of a sandbox that started, and download nothing. With
`NVX_AUTO_DOWNLOAD` at `0`, a command takes no lock. The roots of the
[scenario tests](#scenario-tests) are no Git checkouts, or hold a `.git` that
is no repository, so their commands reach no network.

### `egress compile PATH`

Compiles an egress policy file, as `--network-egress-policy-file` does, and
prints its native OpenVMM rules in the same JSON form:
`{"allow": [RULE, ...], "deny": [RULE, ...]}`, each list in the order in which
OpenVMM receives the rules, which are `--network-egress-allow` and
`--network-egress-deny` values. A file that `nvx.py` refuses fails with its
message.

### `run`

`nvx-rs run` ([Usage](../doc/usage.md#run)) has the options, with their names,
defaults, and value checks, and the checks, messages, outputs, and exit
statuses of the `nvx.py run` that it replaced, except as
[Other differences from nvx.py](#other-differences-from-nvxpy) lists. A cold
boot prints `>> COMMAND`, OpenVMM's command line, quoted as `sandbox run`
quotes it, and unless `--dry-run` is given, it runs OpenVMM with the standard
streams, so that the guest's console and the terminal settings that OpenVMM
restores behave as they do for OpenVMM itself, and exits with OpenVMM's
status; a restore does none of these, as the second item says.

- A cold boot runs the kernel and the initramfs of `--guest`, `alpine` by
  default, with `--memory-mib` or else the guest's default, 128 MiB for Alpine
  and 512 MiB for Ubuntu and Azure Linux, which `Guest::default_memory_mib`
  gives.
- `--restore-snapshot NAME` restores the named snapshot of `--store-dir` in
  place of a cold boot, once, through the path of `snapshot restore`: it
  provisions a managed sandbox in `--state-dir`, a new directory outside the
  store, on its filesystem, with the image, upper, workload, and timeout of the
  options of `snapshot restore` that `sandbox` defines and parses,
  `--state-dir`, `--layer`, `--layer-format`, `--scratch`, `--upper`,
  `--ram-upper-percent`, `--entrypoint`, `--arg`, `--hostname`,
  `--workload-user`, and `--timeout`, starts it from the snapshot, which
  forwards the workload's output, stops the VM once the workload has exited
  with status 0, removes the sandbox and its directory in any case, and exits
  with the workload's status. It refuses a raw OpenVMM snapshot before it
  reads anything, a `--state-dir` that exists or lies in the store, a workload
  that `snapshot restore` refuses, and the options of a cold boot that the
  snapshot or its sandbox fixes; a cold boot refuses `--store-dir`,
  `--state-dir`, and the restore options. A `run` that dies before it removes
  the sandbox leaves it with its VM running, which `sandbox deprovision` ends,
  after `sandbox stop` once the workload has exited with status 0; see
  [`run`](../doc/usage.md#run).
- A cold boot attaches the guest's console, the boot console `hvc1`, to the
  terminal with `--virtio-console console`, so that the terminal's input
  reaches the guest through it, not through the byte channel, `hvc0`, whose
  output, such as the kernel's early messages, goes to standard error.
- A cold boot takes `--net` and `--network-profile` together. A restore takes
  its NIC, with its addressing, from the snapshot, which
  `snapshot capture --net` gives one, so it refuses `--net`. Its other network
  options are the restore's own policy, at the NIC's address, which `sandbox`
  lowers, with its DNS server, egress, forwards, and published listeners; a
  snapshot with a NIC needs `--network-profile` with them, and one without
  refuses them.
- One directory `--mount` is OpenVMM's `--mount`. Several, or a shared file,
  are the children of an aggregate at `/run/nvx/shares`, whose guest targets
  must be absolute, must not overlap, and must survive the kernel command line,
  and on a cold boot, their `nvx_share=` tokens join `--cmdline` within the
  1024 bytes that OpenVMM leaves to them. `--mount-deny`, `--mount-allow`, and
  `--mount-write` name host paths, which OpenVMM attributes to the share that
  holds them, and which must be absolute with an aggregate, unlike those of
  `sandbox`, which apply to the `--mount` before them.
- After OpenVMM exits with a status other than 0 from a cold boot, on a host
  whose CPU no built-in CPU profile serves, it prints guidance to standard
  error: what `--cpu-profile auto` does on that CPU, and, on an Intel or AMD
  CPU, how to request a built-in profile for it with the fingerprint that the
  OpenVMM of the run writes. It identifies the CPU with CPUID, as OpenVMM does,
  and reads the CPUs that the built-in profiles serve from
  `src/host/cli/cpu_generations.json`, a copy of the identities of the profiles
  that OpenVMM pins. It prints none for `--cpu-profile` with a profile ID, nor
  for `host` on an Intel or AMD CPU, whose failures OpenVMM explains, and never
  after a dry run, a restore, an error or usage error of `nvx-rs`, or Ctrl-C,
  so that it follows only OpenVMM's own refusals, whose statuses those of
  `nvx-rs` share.

### `sandbox`

`nvx-rs sandbox` ([Usage](../doc/usage.md#sandbox)) has the options, with
their names, defaults, and value checks, and the operations, checks,
messages, outputs, exit statuses, and outcome reports of the `nvx.py sandbox`
that it replaced, except as
[Other differences from nvx.py](#other-differences-from-nvxpy) lists.
The operation defaults to `run`. Options take their values
as `--option VALUE` or `--option=VALUE`, repeated options accumulate, and
`--mount-deny`, `--mount-allow`, and `--mount-write` apply to the `--mount`
before them.

- `run` runs one workload in a sandbox of its own, which keeps no state
  directory: it launches the VM that a `provision` and a `start` of its
  options would launch, in a fresh temporary VM directory, and prints
  `>> COMMAND`, OpenVMM's command line, quoted as the Microsoft C runtime
  splits a command line on Windows and as a POSIX shell splits one elsewhere,
  as `nvx.py` quoted it with `subprocess.list2cmdline` and `shlex.join`.
  Unless `--dry-run` is given, it then executes the workload once through the
  control session, as `exec` does, with its working directory and environment
  options, streaming its standard input, output and error, asks the guest to
  shut the VM down, waits up to `--timeout` seconds for OpenVMM to exit before
  it terminates it, removes the VM directory, and exits with the workload's
  status. `--outcome-report` forwards OpenVMM's outcome report of the VM's
  stop.
- `provision` records the sandbox in its state directory. `start` launches
  OpenVMM detached with the managed lifecycle, passes it a fresh capability
  through its standard input, and waits until the guest answers. `exec` runs a
  workload, streams its own standard input to it, until that ends, and the
  workload's standard output and error back as they arrive, and exits with the
  workload's status; Ctrl-C cancels the workload. `--outcome-report` writes
  `{"operation_id": HEX, "outcome": {"category": CATEGORY, "operation":
  "exec", "status_code": STATUS}, "schema_version": 1}`. `stop` shuts the VM
  down and prints nothing, but for a warning when it terminates a VM of another
  [runtime identity](#runtime-identity). `deprovision` removes the state
  directory.

The state directory is the CLI's own [store](src/host/cli/store/mod.rs) of the
sandbox, which keeps the runtime records of its VM, and `config.json` is the
CLI's record of the sandbox beside them. It also
records the [distro binding](#distro-bindings) and the
[scratch binding](#images-by-path) of the sandbox, and its network as the
network options from which each `start` builds the policy again, as `provision`
built it. The typed configuration names one EROFS or GPT image by path and
identity, and a scratch upper by path or a RAM upper with its cap from 1 through
90 percent, 50 by default. `nvx-rs` refuses earlier configuration formats, such
as format 9, rather than migrating them, and says to deprovision the sandbox and
provision it again:

| File | Contents |
| --- | --- |
| `config.json` | The provisioned sandbox, in format 10 only: `image` holds its format, path and identity; `upper` is tagged as scratch with its path or RAM with `max_memory_percent`. `distro_binding` binds the image, and `scratch_binding` binds scratch or is `null` for RAM. The export is typed, `null` when absent. `network`, `null` without `--net`, holds `--net`, `--network-egress`, `--network-ingress`, `--host-loopback`, `--network-proxy`, each `null` when not given, and the `--host-loopback-forward` options, as given, with the egress rules in OpenVMM's form, from a policy file's compile if one was given. A sandbox that `snapshot restore` provisioned also records its snapshot's name under `source_snapshot`, the absolute path of the snapshot's store under `source_store`, and the program and arguments of the workload that each restore runs under `workload`; one that boots cold records none of them. An atomic pretty-JSON write uses mode 0600, and start reads the same strict typed fields without a legacy reader. |
| `sandbox.lock` | The store's lock, which serializes the lifecycle transitions |
| `exec.lock` | The exec lock, which an `exec` holds from before it finds the running VM until its workload ends, so that one `exec` at a time runs in the sandbox; it stays from the first `exec` until `deprovision` |
| `launch.json` | The store's journal of a start in progress, or of one that was interrupted |
| `runtime.json` | The store's runtime record of the running VM, in its format 4: the OpenVMM process, by its ID, its start time, and the host's boot, its control endpoint, `STATE/control.sock` or `//./pipe/openvmm-microvm-HEX` on Windows, and the [runtime identity](#runtime-identity) of its start |
| `control.capability` | The 32-byte capability of the running OpenVMM, with mode 0600 |
| `control.sock` | The control endpoint on POSIX hosts |
| `openvmm.log` | OpenVMM's output, which each start replaces |
| `outcome.json` | OpenVMM's outcome report of the last start |
| `openvmm/` | A restored sandbox's view of its snapshot, hard links to the files of the snapshot's `openvmm/`, which a start creates and `deprovision` removes, each under the snapshot's lock |
| `restore-ready.sock` | On Linux, the endpoint of a restore's restore-ready event, which the next start of a restored sandbox, or its `deprovision`, removes under the snapshot's lock |

`provision`, `start`, `stop`, and `deprovision` hold the store's lock for their
whole transition, and `exec` only while it finds the running VM, so that a long
workload cannot delay a `stop`. An `exec` that finds another holding
`exec.lock` releases the store's lock and tries again until `--wait` seconds,
60 by default, have passed, and then fails with `sandbox-busy`; `stop` takes no
exec lock. The CLI takes the lifecycle locks of its own
[store](src/host/cli/store/mod.rs) with `config.json` as its
configuration record: a transition that leaves the state directory without it,
such as one of a sandbox that was never provisioned, removes the lock file as
it releases the lock, and `deprovision` removes the sandbox through the store,
which removes the store's files and `config.json`, refuses files that are not
NVX's, and removes the lock file, while it still holds the lock, before the
directory. Taking the lock ends a start that was interrupted. Every transition
but `provision` then finds the sandbox's VM in the store's runtime record, as an
orchestrator does after it restarts within the same boot of the host: it holds
an OpenVMM that still runs through its verified process identity, and clears
the runtime record of one that exited without a `stop`.
The first `exec` or `stop` to find such a record fails because the runtime
state is stale, while `start` launches OpenVMM again and `deprovision` removes
the sandbox. `start` launches OpenVMM with `nvx::host::vm::create` and
`nvx::host::vm::spawn`, through the store's launch journal, and
`stop` asks the guest to shut down through the control session, waits up to
`--timeout` seconds for OpenVMM to exit, and reports one that does not, which
keeps running: it never ends OpenVMM otherwise, except one of another
[runtime identity](#runtime-identity).

#### Runtime identity

`start` records the runtime identity of its artifacts in the runtime record:
the SHA-256 digests of OpenVMM, the kernel, and the Alpine initramfs, and the
boot contract, control contract, and control session versions of this build,
which `nvx::contract` defines with the changes that make each one change. The
boot contract covers the microVM ABI, the kernel command-line tokens that
configure the guest, such as those of `nvx::protocol::root`, and the root that
the guest assembles from them. The digests come from the root's install
metadata, and no artifact is hashed:

- A release lists them in its `SHA256SUMS`, under `bin/openvmm[.exe]`,
  `guest/vmlinux`, and `guest/initramfs.cpio.gz`.
- A checkout has them in the build provenance that `nvx.py` writes to `build/`
  with each artifact that it builds, and that `nvx-rs download` installs with a
  release's artifacts: the `executable_sha256` of `openvmm.provenance.json`, the
  `kernel_sha256` of `vmlinux.provenance.json`, and the `initramfs_sha256` of
  `initramfs.provenance.json`, each of format 1.

`start` needs that metadata, and so does every other transition but
`provision` while the store records a start of the sandbox: each fails when
the metadata is missing or names no valid digest, and signals nothing. `start`
runs the root's own OpenVMM, which the metadata identifies, and refuses another
one that `NVX_OPENVMM` names. The transitions drive only a VM that the same
identity started. One that other artifacts started, as before they were rebuilt
or replaced, may speak other contracts, so no transition opens a control
session to it: `start` and `exec` refuse it, naming the parts that differ,
`deprovision` refuses it as running, and `stop` terminates its OpenVMM through
its verified process identity, waits up to `--timeout` seconds for it to exit,
and warns on standard error that it did. The guest does not shut down, so
OpenVMM publishes no outcome report. An OpenVMM that does not exit in time, or
whose identity the stop cannot determine, keeps the runtime record, as does a
stop whose wait Ctrl-C ends, and the next `stop` retries.

The store reads runtime records of its format alone. A sandbox that a build
with another format started, such as one whose `runtime.json` has format 1,
fails every transition with a message that names that format: stop every
running sandbox with the build that started it before you switch builds. An
OpenVMM that a start of such a build left behind when it was interrupted has no
record, and that build asked for it to be ended by hand: end it before the
switch too, since this build finds no VM in that sandbox.

#### Guest protocol

After the guest answers, `start`, and `sandbox run`, speak version 2 of `NVXC`
to it, whose `READY` names the version that the guest agent speaks, and require
that version: a guest image whose agent answers in another version, as one
that predates version 2 does, is stopped as a failed start is, and the start
fails. They ask for no control features, which only version 1's `FEATURES`
request reported, and which `start` required before: per-execution environments
and host path mapping tables, which every guest agent of version 2 provides.
`exec` and `sandbox run` send a working directory and an environment, and the
start then sends the mapping table of the sandbox's shares, which the guest
agent mounts before it runs any workload; a mapping that it refuses fails the
start, after the guest shuts down as `stop` has it do. `nvx.py` does not check
the version; this, the streams of `exec` and `sandbox run`, the checks of the
[distro binding](#distro-bindings) and of the
[scratch binding](#images-by-path), and the claim of the scratch image are the
intended changes of behavior.

#### Other differences from nvx.py

- The cold boots of `run` attach the guest's console to the terminal through
  the boot console, with `--virtio-console console`, where
  `nvx.py` left it on the byte channel, `hvc0`, which took the terminal's
  input. The byte channel's output goes to standard error.
- Long options cannot be abbreviated, where `argparse` accepted any prefix that
  named one option.
- A value that starts with `-` follows its option as a separate argument only
  if it starts like a negative number in ASCII digits, such as `-1` or `-.5`;
  any other, such as `-١`, takes the `--option=VALUE` form, where
  `argparse` also took the digits of other scripts. The first `--` ends the
  options without being an argument itself, so `nvx-rs run -- extra` reports
  `unrecognized arguments: extra`, where `argparse` reported `-- extra`.
- The usage is one line, which names `[OPTIONS]` for the options of `run` and
  `sandbox`, and the program is always `nvx-rs`, where `argparse` listed every
  option and wrapped the usage for 80 columns.
- Messages quote values with Rust's debug formatting, as in
  `invalid choice: "3" (choose from "1", "2", "4", "8")`, where `nvx.py` quoted
  them as Python's `repr` does, as in `'3'`.
- I/O errors print as Rust writes them, after the path that they concern, as in
  `"/state/config.json": Permission denied (os error 13)`, where `nvx.py`
  printed Python's `[Errno 13] Permission denied: '/state/config.json'`.
- Integer options take ASCII decimal digits, in a range of their own: the MiB
  of `--memory-mib` and `--memory-capacity-mib` from 1 to 17592186044415, the
  most whose bytes OpenVMM counts in 64 bits, and `--exec-timeout-ms` from 0 to
  4294967295. `--processors` takes one of its choices as written, and
  `--timeout` a number as Rust reads an `f64`. `nvx.py` read them with Python's `int` and
  `float`, which also accept signs, underscores, surrounding whitespace, and the
  digits of other scripts, and left OpenVMM to refuse a negative size, and
  `exec` to refuse a timeout beyond 32 bits, where `nvx-rs` refuses them as
  usage errors. `provision` in `nvx.py` and in earlier releases of `nvx-rs`
  recorded any integer that `--memory-mib` gave, and OpenVMM then refused that
  RAM at every `start`. `start` now refuses a configuration that records guest
  RAM outside the range of `--memory-mib`, before it launches OpenVMM, with
  `sandbox configuration records N MiB of guest RAM`, the range, and
  `deprovision the sandbox and provision it again`.
- `start` reads `config.json`, and `start` and `stop` read OpenVMM's outcome
  report, as strict JSON, as egress policy files are read: without `NaN`,
  `Infinity`, or a field that an object repeats, and with each field of the
  type that the format gives it, where `nvx.py` read them with `json.loads` and
  converted their values with `str` and `int`. The files that `nvx-rs` writes,
  `config.json`, the outcome report of `exec`, and the output of `artifacts`
  and `egress compile`, hold characters beyond ASCII as they are, in UTF-8,
  where `json.dumps` escaped them as `\uXXXX`.
- Where `nvx.py` would end with a Python traceback, as for a configuration
  without a field that `start` reads, `nvx-rs` prints `error[CODE]: MESSAGE`,
  such as `sandbox configuration is malformed`, and exits with status 1.
- Paths print with their components alone, as `std::path::Path::components`
  reads them, so a leading `.` stays, as in `./snapshot`, where `nvx.py`
  printed `snapshot`, as `pathlib` normalizes it. A path resolves as the host
  resolves it: on Linux, a `..` that follows a directory that does not exist
  fails, where `Path.resolve` dropped both.
- On Windows, a junction counts as a link, as `std::path::Path::is_symlink`
  tells it, so a share's host path, or the directory of an outcome report, that
  is a junction is refused as a link is, where Python's `os.path.islink` counted
  only symbolic links. A state directory is resolved first, as before, so a
  junction there names its target.
- Errors and usage errors carry their [error codes](#error-codes), as
  `error[CODE]: MESSAGE` and `PROG: error[CODE]: MESSAGE`, where `nvx.py`
  printed `error: MESSAGE` and `PROG: error: MESSAGE`.
- `exec` and `stop` report a state directory without the configuration of a
  sandbox, which holds no provisioned one, as `start` does, with
  `sandbox-not-provisioned`, where `nvx.py` reported a sandbox that is not
  running.
- Text goes to pipes and files in UTF-8, where Python on Windows uses the ANSI
  code page unless `PYTHONIOENCODING` or UTF-8 mode says otherwise.
- `start` refuses a control endpoint that another process serves, which it
  treats as one that has not appeared yet, and, on Windows, starts OpenVMM
  without a console window, as `nvx::host::vm::spawn` starts it.
- `start` stops waiting for the control endpoint once the OpenVMM that it
  launched exits, as `nvx::host::transport::connect_verified` does, and fails
  at once with `exited`, where `nvx.py` waited until `--timeout` passed before
  it reported the exit.
- `exec` and `sandbox run` stream the workload's standard input, output and
  error while it runs, where `nvx.py` gave it no input and printed its output
  once it ended. So a guest's diagnostic of a request that it refuses, such as
  a working directory that the workload cannot enter, precedes the error.
  Ctrl-C cancels the workload, where `nvx.py` closed the session.
- `provision`, and `start` again, refuse a share whose resolved host path
  equals, contains, or lies inside the sandbox's state directory, image, or
  scratch image, or the OpenVMM, kernel, or initramfs of the artifact root, as
  [Live host shares](../doc/run.md#live-host-shares) describes, where `nvx.py`
  shared them.
- The state directory holds the store's files: `sandbox.lock` replaces the
  `lifecycle.lock` of `nvx.py`, `runtime.json` has the store's format, and each
  start replaces `openvmm.log`, which `nvx.py` appended to. `exec` holds the
  lock while it finds the running VM, which `nvx.py` did not, and holds
  `exec.lock` until its workload ends, so that one `exec` at a time runs in a
  sandbox, where `nvx.py` ran them at once and had no `--wait`. A runtime record
  of an OpenVMM that exited is cleared by the first transition that finds it,
  so that `start` launches the sandbox again, where `nvx.py` refused to.
- `provision` refuses an existing state directory that is not private to the
  current user, as the store refuses it, rather than make it private.
- A managed `start` refuses an OpenVMM that `NVX_OPENVMM` names instead of the
  root's; see [Runtime identity](#runtime-identity).
- The `nvx_layer` token of the kernel command line names the image format of
  the distro layer, as in `nvx_layer=distro,0xd0003000,erofs:UUID`, and the
  guest refuses the form without a format that `nvx.py` wrote; see
  `nvx::protocol::root`.
- `run` and `provision` read the network options into one typed
  `NetworkPolicy`, which the plan holds and `nvx::host::network` lowers, where
  `nvx.py` passed them to OpenVMM. Invalid values and combinations fail before
  OpenVMM runs and before `provision` records anything, with status 1, where
  OpenVMM refused them, or never checked them, as for a proxy beside any other
  way out. The command spells out OpenVMM's defaults and the rules in their
  canonical form, with the network options after the plan's other options; see
  [Sandbox networking](../doc/run.md#sandbox-networking).
- A sandbox's scratch image is claimed while its VM runs, so a second
  sandbox, or a `run`, on the same image fails; see
  [Images by path](#images-by-path).
- On Linux, OpenVMM starts with the caller's umask without the owner's bits,
  where `nvx.py` passed the umask unchanged, so that a umask that clears the
  owner's own permissions, such as 0400, no longer keeps the caller, or `start`
  and `stop`, from reading OpenVMM's outcome report; see
  [`nvx::host::vm`](#nvxhostvm).
- `start` and `exec` refuse, and `stop` terminates, a VM that another runtime
  identity started; see [Runtime identity](#runtime-identity).
- `run --restore-snapshot` names a snapshot of `--store-dir` that
  `snapshot capture` published, and restores it once through the path of
  `snapshot restore`, where `nvx.py run` handed OpenVMM the directory of a raw
  snapshot, which `nvx-rs run` refuses before it reads anything. It refuses
  `--net`: a restored NIC, with its addressing, comes from the snapshot, which
  `snapshot capture --net` gives one. It takes none of the options of a raw
  restore, `--restore-processors`, `--restore-memory-mib`, and
  `--restore-ready-path`, whose values the snapshot and the restore fix.

### Test hooks

`--root` and `NVX_OPENVMM` let tests run the CLI without a VM. `nvx-fake-openvmm`
serves the sandbox profile: its managed lifecycle, with the capability on its
standard input, the control console, and an outcome report at stop, and its
one-shot runs, which run `/bin/echo`, `/bin/sleep`, `/bin/true`, or
`/bin/false`, write an outcome report if asked, and exit with the workload's
status. It also runs microVMs without a guest, a cold boot of `nvx-rs run` or
the restore of a snapshot directory: it prints that it booted or restored
the microVM, writes an outcome report if asked, and exits with status 0, and a
restore whose snapshot directory is missing fails with status 1. Like OpenVMM,
a restore checks its `--virtio-console` against the attachment of the boot
console that its snapshot's manifest records, which the fake decodes on its
own, and fails with status 1 where OpenVMM would: without
`--virtio-console console` for a boot console that was on the terminal, and
with it for any other. Like OpenVMM, it refuses with status 2 a restore with
`--net`, whose NIC keeps the addressing that its snapshot saved, a cold boot
with `--network-profile` alone or with `--restore-network-policy`, and a
restore with `--network-ingress allow` or explicit `--host-loopback allow`
without `--restore-network-policy`. Kernel
command-line tokens make it fail: `fake_exit_on_start=STATUS` and
`fake_exit_status=STATUS`, among others; see `src/testing/fake_openvmm.rs`. Like
OpenVMM, it opens the sandbox's block devices at the paths that its options
name and holds them until it exits, after `fake_block_delay_ms=MS` if asked,
and records what each held in `fake-openvmm-TOKEN-blocks.json` next to the
kernel; it takes a scratch disk, or, for `nvx_overlay_upper=ramfs`, the
`distro` layer alone. It also runs the VMs of templates' captures, whose block
devices it opens as OpenVMM does, without following a link and, on Windows,
sharing only reads and deletion, recording in that file whether a handle could
write them; it then publishes the snapshot of the manifest that
`fake-openvmm-capture.json` next to the kernel names, or fails, never ends, or
crashes, as that scenario asks. A managed `start` also reads the root's
[runtime identity](#runtime-identity), so a test root holds build provenance,
as the roots of the CLI tests do.

```bash
cd nvx-rs
cargo build --features cli,testing --bins
NVX_OPENVMM=target/debug/nvx-fake-openvmm \
  target/debug/nvx-rs --root .. sandbox run --layer distro,LAYER,UUID \
  --scratch SCRATCH --entrypoint /bin/echo --arg hello
```

### Scenario tests

The scenario tests of `tests/cli/` run `nvx-rs` with `nvx-fake-openvmm` as
OpenVMM, in a fresh artifact root for each case, and check from tables of cases
what callers rely on: exit statuses; errors, by their [codes](#error-codes) and
by stable fragments of their messages, such as option names, values, and paths;
the entries of state directories, their records, parsed, and their POSIX modes;
and the arguments that OpenVMM receives, which the fake OpenVMM records, and
into which the `>> ` line of a run splits back, with `CommandLineToArgvW` on
Windows and a shell-word splitter on POSIX, as a property test checks for
arbitrary kernel command lines too. The tables cover usage errors and help, the
checks that refuse a command line before anything changes, dry runs, the runs
of `nvx-rs run` and of `nvx-rs sandbox run`, the lifecycles of sandboxes of
every configuration, their executions, the
refusals of lifecycle operations, configurations of every format, from the
hand-written `config.json` files of `tests/fixtures/state/`, `egress compile`,
`artifacts`, and `download`, with the downloads of `run` and `sandbox`, which
fail before they reach a network.

They replaced the scenarios that `scripts/nvx_rs_fixtures.py` and
`scripts/nvx_rs_run_fixtures.py` recorded from the Python implementations of
`nvx.py sandbox` and `nvx.py run`, which the CLI test replayed byte for byte
until [microsoft/nvx#728](https://github.com/microsoft/nvx/issues/728) retired
them, once every recorded scenario, and every regression that
[microsoft/nvx#722](https://github.com/microsoft/nvx/issues/722) traces, mapped
to the scenario tests, which reached every line and branch of production code
that the replay reached, and caught every mutant of the CLI that the replay
caught.

The scenario tests check the same outcomes whoever runs them, unprivileged or
as root. `--mount-owner caller` squashes guest root to the owner of a share, so
`nvx-rs sandbox` refuses a share that UID 0 or GID 0 owns, and OpenVMM refuses
one at launch. The shares of each root therefore belong to a user other than
root: the user that runs the tests, or UID and GID 65533 when they run as root.
Whoever runs the tests, the checks that refuse a command line also check that
`sandbox run` and `provision` refuse to share `/`, which root owns, with the
caller owner.

## Features

| Feature | Contents |
| --- | --- |
| `host` (default) | The `host` module. It enables `serde`, and `sha2`, which names the children of a sandbox's aggregate share and hashes distro layers in their explicit verification |
| `testing` | NVX's own `nvx-fake-openvmm` binary, a fake OpenVMM for tests; see [Test hooks](#test-hooks). It adds no API |
| `cli` | NVX's own `nvx-rs` binary; it adds no API, and enables `host`. See [The nvx-rs CLI](#the-nvx-rs-cli) |
| `serde` | The serde forms of `Hypervisor` and `net::Ipv4Cidr` |
| `guest` | The `guest` module and the guest binaries; see [Guest binaries](#guest-binaries) |

Without default features, the crate has only `protocol`, `activation`, `exec`,
`contract`, `net`, `gpt`, `uuid`, `Hypervisor`, and `Error`, and depends on
nothing but `serde` when `serde` is enabled and `nix` and `getrandom` when
`guest` is.

## Example

A cold boot through `nvx::host::vm`, in a private directory that the caller
owns, and the VM's stop.
[`examples/host-workflows.rs`](examples/host-workflows.rs) also captures a VM,
restores one, and finds one again after its caller restarts.

```rust,no_run
use std::path::Path;
use std::time::{Duration, Instant};

use nvx::Hypervisor;
use nvx::activation::{MachineIdMode, View};
use nvx::host::config::{Boot, ConsoleAttachment, RootConfig, VmConfig};
use nvx::host::runtime::{ArtifactIdentity, LaunchOptions, Runtime, RuntimeIdentity};
use nvx::host::vm::{self, VmDir};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The caller locates OpenVMM, the guest kernel, and the Alpine initramfs, and identifies each
    // by the SHA-256 digest that its install metadata records, such as the `SHA256SUMS` of an NVX
    // release: here, from the command line.
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [openvmm, kernel, initramfs, openvmm_sha256, kernel_sha256, initramfs_sha256] =
        <[String; 6]>::try_from(arguments).map_err(|_| "expected three paths and three digests")?;
    let runtime = Runtime {
        identity: RuntimeIdentity::new(
            ArtifactIdentity::sha256(&openvmm_sha256)?,
            ArtifactIdentity::sha256(&kernel_sha256)?,
            ArtifactIdentity::sha256(&initramfs_sha256)?,
        ),
        openvmm: openvmm.into(),
        kernel: kernel.into(),
        initramfs: initramfs.into(),
        hypervisor: Hypervisor::from_env_or_default()?,
        cpu_profile: None,
        windows_breakaway: false,
        options: LaunchOptions::default(),
    };
    let config = VmConfig {
        processors: 1,
        memory_mib: 256,
        root: RootConfig::Blockless,
        view: View {
            hostname: b"example".to_vec(),
            machine_id: MachineIdMode::Image,
        },
        network: None,
        filesystem: None,
        console: ConsoleAttachment::Disconnected,
        boot: Some(Boot {
            command_line: "quiet".to_owned(),
            memory_capacity_mib: None,
            capture: None,
        }),
    };
    // The caller creates and owns the VM's private directory.
    let directory = Path::new("example-vm");
    vm::create_private_dir(directory)?;
    let dir = VmDir::open(directory)?;
    // Creation checks the whole launch and creates its log, but starts nothing; the spawn starts
    // OpenVMM detached, and returns its verified process.
    let launched = vm::spawn(vm::create(&runtime, &config, None, &dir)?)?;
    // The caller speaks its own protocol with its guest at `launched.outputs.endpoint`, with the
    // secret `launched.capability`. To stop the VM, the callback would ask the guest in that
    // protocol to end it, and return whether it sent the request; this one sends none, so the
    // stop ends OpenVMM's verified process once the grace period has passed, unless it exited.
    let grace = Instant::now() + Duration::from_secs(10);
    let stopped = vm::stop(
        &launched.process,
        &launched.outputs,
        |_| false,
        grace,
        grace + Duration::from_secs(10),
    )?;
    println!("forced: {}, report: {:?}", stopped.forced, stopped.report?);
    Ok(())
}
```

## Errors

Every fallible operation returns `nvx::Error`: a message that callers can show
as is, an `ErrorCode`, which determines the error's `ErrorKind`, and an
optional cause.

| `ErrorKind` | Meaning |
| --- | --- |
| `Unavailable` | A missing or unusable artifact, hypervisor, or release, a guest image without a required control feature, or an unsupported host |
| `Unsupported` | A request that NVX refuses: a value that it does not accept, such as a malformed or out-of-range one, options that it cannot combine, or a request that it cannot represent or enforce, such as a policy beyond a limit of OpenVMM or the guest agent |
| `Exited` | The VM's OpenVMM process exited |
| `StillRunning` | A stop could not confirm the VM process's exit, so the caller keeps its verified process |
| `Io` | A host I/O failure, or state that NVX recorded on the host and cannot read |
| `NotFound` | State that an operation names and that does not exist on the host, such as the store of a sandbox that was never created or that a removal deleted |
| `Protocol` | A control session that failed, or a request that the guest agent refused |
| `TimedOut` | A deadline that passed |
| `RuntimeMismatch` | A VM that runs under another runtime identity than the artifacts', which NVX does not drive: only a stop, which terminates it, applies |
| `Other` | Any other failure |

### Error codes

An `ErrorCode` is finer than an `ErrorKind`: callers tell errors apart by their
codes, and by stable fragments of their messages, such as the names of options,
fields, files, and processes, since the wording of a message may change. The
[CLI](#the-nvx-rs-cli) prints the code of each error that it reports.

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
| `still-running` | `StillRunning` | A stop could not confirm the VM process's exit, so the caller keeps its verified process |
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
| `sandbox-busy` | `TimedOut` | Another lifecycle transition, or `exec`, of the sandbox held its lock until the deadline passed |
| `runtime-mismatch` | `RuntimeMismatch` | The sandbox's VM runs under another runtime identity than the artifacts', whose differing parts the message names, so only a stop, which terminates it, applies |
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

## Guest binaries

The `guest` feature adds the programs that run in NVX's Linux guests: the
`nvx::guest` module and one binary per program. The Alpine, Ubuntu, and Azure
Linux images ship all ten at their established `/sbin/nvx-*` paths. The
module's libraries are not programs: `terminal` opens the terminal on which an
agent serves its session, and `mappings` describes host mappings and keeps the
account of a host's mapping table. NVX's own guest space is nvx-init's internal
reference code, private to the crate, as [nvx-init's space](#nvx-inits-space)
tells: the mapper of `mappings` mounts host mappings beneath the descriptors of
the roots that its caller names, and `root` assembles a layered root, mounts
live shares in it through the mapper, and tears it down.
`nvx-managed-agent` opens its control terminal and runs direct workloads through
nvx-init's plain-process core: each child resets its priority, drops its
credentials and capabilities, and enters its working directory before exec.
Each execution ends its process group, including on normal completion, without
cgroups, namespaces, limits, or a privilege-dropping helper. It applies its
host mapping table through the table of `mappings` and nvx-init's mapper, from
its mount of the host's export, beneath `/` in direct mode, and any other guest
init or agent can use the table and the mapping values too.
Any guest init or agent can also run the boot steps of `nvx-time` and the steps
of `nvx-reseed` in its own process, through the
`nvx::guest::time` and `nvx::guest::reseed` calls, on which both programs run,
and a template's guest captures before its root's assembly through
`nvx::guest::snapshot::capture`, which runs the steps of `nvx-time pre-capture`
and, in each clone, repairs its restore: it activates the restore's memory
target, reseeds the kernel's random number generator, and finishes the restore,
as `nvx-snapshot` does.
Nor is the private `gpt` module a program: it reads the partition table of a GPT
disk's block device.

| Binary | Program |
| --- | --- |
| `nvx-console-pending DEVICE` | Prints the number of bytes waiting to be read from a device, such as a console |
| `nvx-mmio-write ADDRESS VALUE` | Stores a 32-bit value at a physical address through `/dev/mem` (x86_64) |
| `nvx-reseed [--generation-only] SEED_FILE [PREVIOUS]`, `nvx-reseed --sample` | Reseeds the kernel's random number generator from a seed file and checks its VM generation ID, or prints a random sample |
| `nvx-exit [CODE]` | Stops the VM with an 8-bit exit code through OpenVMM's control port 0x604 (x86_64) |
| `nvx-device-io file ...`, `nvx-device-io network ...`, `nvx-device-io map PATH TEXT` | The benchmark helper that measures file and UDP round-trip I/O and prints one JSON line, and reports a file benchmark's progress on stderr; `map` writes TEXT at the start of a file through a shared mapping and holds the dirty mapping until its standard input ends, for the microVM scenarios (x86_64) |
| `nvx-managed-agent CONTROL_TTY ROOTFS HOSTNAME UID GID USER HOME` | Serves the managed control session and supervises sequential workloads (x86_64) |
| `nvx-ingress [inherit\|start] GATEWAY_IPV4` | Announces reachable TCP listeners and settled UDP sockets to the portable-network gateway, from a privileged source port that it binds, or, with `inherit`, from its standard input, a socket that the caller bound to one after the clone's process-free application; `start` binds such a socket and starts `inherit` on it in a session of its own, then exits once its program runs, or with status 1 if it does not start, for a guest agent that cannot hold the announcer; `test DIRECTORY` runs synthetic procfs snapshots |
| `nvx-time [--report-only] COMMAND` | Enforces NVX time ABI v1: runs its conformance checks, repairs the wall clock after a restore, publishes the time state, and runs the discipline and violation-watcher daemon; `test NAME ...` runs its decoder and process test hooks (x86_64) |
| `nvx-time-probe latency\|warp\|clock ...` | Diagnoses clock-read latency, cross-CPU TSC warp and offset, the clocks, CPUID, clock events, kernel messages, and optionally MSRs (x86_64) |
| `nvx-init TIME_DAEMON [ANNOUNCER]` | Takes PID 1 over from a layered root's early boot, with its time daemon and optional announcer: gives itself and them an out-of-memory score adjustment of -1000 and a nice value of -10, collects every child's status, and, after `sync`, ends the VM with status 125 when a helper ends, 0 on `SIGTERM`, or the status of a failed restored activation's class; elsewhere it exits with 2 (x86_64) |

Each binary's command line, exit statuses, and byte output are compatibility
contracts; the [`nvx::guest`](src/guest/mod.rs) documentation describes each
program. After a restored platform capture, `nvx-init` keeps one activation
across control-session replacements, with its deadline anchored to the repair
acknowledgement. It applies the restore's root and network, mounts the runtime,
starts and retains an announcer only for published listeners, switches once,
then completes and reports Ready. The host supplies the complete mapping table
before the first valid EXEC starts the restored workload; later executions and
byte-identical activation retries, also those that arrive while a workload
runs, reuse the retained state, and an activation with other bytes ends the VM
as a failure. A failed activation, which a deadline that passes before Ready
and a refused activate request also are, ends the VM with the status of the
failure's class, which [`nvx::protocol::status`](#nvxprotocolstatus) defines,
such as 54, `ACTIVATION_ROOT`, for a root that does not assemble over its
upper, or 51, `ACTIVATION_PROTOCOL`, for an activation with other bytes.
The accepted proxy replaces the four HTTP(S) proxy defaults, or clears them when
the restore has no proxy. Per-EXEC layered entries still win, and exact
environments still replace every default.

Guests run them as static `x86_64-unknown-linux-musl` executables. On
Linux, with the musl target and a C compiler driver, `cc`, `nvx.py` builds them:

```bash
rustup target add x86_64-unknown-linux-musl
python3 scripts/nvx.py build-guest-binaries \
  --output build/nvx-rs-guest-binaries
```

Native guest builds, the digest-pinned Docker Rust stage, and CI all call this
routine. Docker builds the set once and installs identical bytes in all three
guest images; Ubuntu determinism checks rebuild from fresh targets.

Of this crate, the guest images depend only on the files that this build reads:
`Cargo.toml`, `Cargo.lock`, any Cargo configuration in `.cargo/`,
and every file under `src/` but the host side and the test modules. The host
side is `src/host/`, `src/testing/`, and the `nvx-rs` and `nvx-fake-openvmm`
binaries; the test modules are the `tests.rs` files and the `tests`
directories. CI keys the guest images on those
files, so only a change to one of them rebuilds the images, while every change
to the crate runs the NVX microVM tests; see
[Continuous integration](../doc/ci.md#change-classification). Keep
host-only code under `src/host/` and test-only modules in `tests.rs` files or
`tests` directories. The build fails when Cargo's dep-info lists another file,
which `GuestRustBuildConstants` in
[`build_constants.py`](../scripts/nvx_tools/build_constants.py) then has to
select.

### nvx-init's space

nvx-init's space, the root that it assembles and what it mounts in it, is NVX's
internal reference code, private to the crate: `nvx::guest::root`,
`nvx::guest::view`, `nvx::guest::gpt`, the mapper of `nvx::guest::mappings`, and
the application of `nvx::guest::activation`, which nvx-init and NVX's managed
agent use. The
[guest-agent guidelines](../doc/guest-agent-guidelines.md#build-the-space) give
the rules that it keeps, which another init's space may adapt.

#### `nvx::guest::root`

The operations of a layered root, whether or not their caller is PID 1.

`assemble`: the `Assembly`'s block devices, found by their MMIO windows, checked
against their roles' access modes and their expected sizes, and flushed, and the
`Root` that it mounts, at a runtime directory, a canonical path that no symbolic
link leads through, that it holds exclusively until its teardown, where nothing
is mounted already, once it has checked everything: the read-only layers of the
`Image` that the caller names, which it never probes: an EROFS image, whose
expected `nvx::uuid::Uuid` it checks in its superblock, or a GPT disk whose
partition table it reads and checks with `nvx::guest::gpt::read_disk`, against
the caller's `nvx::gpt::Expected` values, whose first partition it reads, up to
the caller's bound, as opaque bytes that it never mounts, and whose later
partitions, at least one, hold ext4 layers that it mounts `ro,nosuid,nodev` with
`errors=remount-ro`, so that an error that the kernel finds in a layer fails the
operation that found it, rather than panic the guest, as the layer's superblock
may ask, from the block devices that the kernel created when it found the disk,
once their superblocks show that no journal needs a recovery, which their
read-only device can't take, partition 2's at the overlay's base and the last
partition's at its top, under an overlay mounted `nosuid,nodev` with the one
option set of the caller's `OverlayMode`, `metacopy=on,xino=on` or, for lowers
that may carry untrusted overlay xattrs, `metacopy=off,redirect_dir=nofollow`,
with no fallback, whose upper and work directories are on the `Upper`: an ext4
scratch mounted `nosuid,nodev`, whose superblock it checks for a journal that
the caller's `Journal` policy forbids, or a tmpfs mounted `nosuid,nodev` with
`size=N%` for the caller's cap of N percent of the guest's memory, which it
refuses outside 1 to 90 before anything changes, or else at the kernel's
default size. It refuses overlay options longer than
`MOUNT_OPTIONS_LIMIT`, the most that a mount passes, which, below `/run/nvx`,
fit up to 195 layers of a GPT disk, and an overlay that the kernel's mount table
shows read-only or without the mode's options in effect, since the kernel mounts
one so, rather than fail, when its upper can't hold its work directory or its
extended attributes.

`Root::mount_share` and `Root::mount_aggregate`: virtio-fs live shares at
targets in the root that it checks, against the targets that `protocol::share`
reserves among others, with one aggregate per root, which it attaches through
its `mappings::Mapper` beneath the overlay, with NVX's container root's reserved
targets: a share's device mounted out of the root, then moved onto its target,
and a copy of each child's entry of the aggregate.

`Root::mount_export`: the host export, the aggregate device of
`protocol::export`, one per root, mounted `nosuid,nodev` with the access of its
`ExportMode` at `hostfs/root` in the runtime directory, out of the root, below a
directory that only root may enter, whose root it returns as the `SourceRoot` of
the caller's mappings, which the caller's own mapper applies. A share, a child,
or the export that fails latches the root.

`Root::mount_runtime`: the runtime's file systems, which an init mounts once
for its VM, before the root's first live share, after it makes every
mount of the root's tree `nosuid,nodev`: procfs at `/proc`, sysfs at `/sys`,
read-only, a tmpfs at `/dev` with an execution's device nodes and links, devpts
at `/dev/pts`, and tmpfs at `/dev/shm` and `/run`, with an execution's options,
then the time status's directory, when the caller names one, bound read-only at
`/run/nvx/time`. It refuses a mount point that leads through a symbolic link or
where something is mounted already, and, before it changes anything, a call
while the root holds the runtime's mounts, whose `/dev` and `/dev/pts` a second
restriction would make `nodev`, or a live share, since the `/run`
tmpfs would hide one beneath `/run` from the teardown. A call that fails
unmounts what it mounted. A later share beneath `/run` is on that
tmpfs, which the root's mapper owns, as a guest's mapper owns its `/tmp`.
Each attachment is checked at its named target before it is recorded. A runtime
mount whose attachment fails and which cannot be detached keeps its mount
descriptor, so rollback and teardown can retry cleanup without trusting its
original path.

`Root::switch`: the switch into the root, which an init makes once for its VM:
it moves the root's mount onto `/`, then makes the root the caller's root
directory with `chroot`, and its working directory with `chdir`. Once the mount
is on `/`, the root has switched for good: a second switch and the teardown are
refused, and dropping the root unmounts nothing, while descriptors opened before
the switch, such as a `TargetRoot` and a `SourceRoot`, still map. A move that
fails leaves the root where it was.

`Root::teardown`: the view's binds, then the shares in reverse order, a runtime mount whose
attachment failed and which could not be detached, then the mounts of an
aggregate, of the export, and of the runtime, the later first, the overlay,
the layers from the top down, and the upper, with every failed sync or unmount
reported. An
ext4 scratch's file system is synced before its unmount, since a mount namespace
that copied the root's mounts keeps them, unsynced, after this one unmounts
them, and a scratch whose sync fails stays mounted.

Each failure is a typed `Error`, or a `SwitchError` for the switch. The
operations collect no child's status, change the caller's root only by the
switch and never its credentials, start no async runtime, and never power the VM
off.

#### `nvx::guest::view`

`apply(&mut Root, &activation::View, dns)` sets the current UTS namespace's
hostname from the view's bytes. Image mode leaves `/etc/machine-id` untouched;
generated mode writes a new UUID at `MACHINE_ID_FILE` in the runtime directory
and binds it read-only over the image's ID. Optional DNS writes one server in
`RESOLVER_FILE` and binds it read-only over `/etc/resolv.conf`.
`nvx::guest::activation` re-exports both file constants.

The root owns every bind, including an attachment left by a failed call.
Teardown before the switch removes them; after the switch teardown refuses
and drop unmounts nothing. A view is attempted once per root, before its
switch. Refusals return the crate's `Error` with kind `Unsupported`, and
system failures with kind `Io`; an error does not restore the old hostname
or remove generated files. The activation record's hostname limit and
machine-ID modes are unchanged.

#### `nvx::guest::gpt`

`read_disk`, through which `nvx::guest::root` reads its disks: the partition
table of a GPT disk, read and checked with
`nvx::gpt`'s `Disk::read` through the disk's block device, which the caller
opened, at the logical block size and size that `BLKSSZGET` and `BLKGETSIZE64`
report, with the caller's `Expected` values, such as the disk GUID that a boot
parameter names. It only reads the device, at explicit offsets, so it never
moves the file's position. It neither compares the partitions that the kernel
created with the table nor asks the kernel to reread the table. Each failure is
a typed `DeviceError`.

## Testing

```bash
cd nvx-rs
cargo test --all-features   # unit, conformance-vector, property, and fake-OpenVMM tests
```

A module's unit tests belong in its `tests` module, out of line, in the
module's `tests.rs` file or in the files of its `tests` directory, which only
tests compile, with its fixtures and test helpers. Production code holds no
tests, only the test hooks that
[`test-layout-allowlist.json`](../scripts/test-layout-allowlist.json) allows, as
[Test layout](../doc/contribute.md#test-layout) describes.

The tests need no hypervisor. The fake-OpenVMM test runs `nvx-fake-openvmm`
directly, as `nvx-rs run` runs OpenVMM on the terminal. The CLI test runs the
[scenario tests](#scenario-tests) of `nvx-rs`, and checks that `exec` and
`sandbox run` stream, and, on Linux, Ctrl-C, which also cancels an `exec`. The
[conformance vectors](tests/vectors/README.md) of the wire formats cover both
sides of every record and payload: the codecs decode each record and each
`EXEC` and `MAPS` payload of `tests/vectors/nvxc-v1.txt` to its values and
encode them to its bytes, version 2's codec refuses its frames, the codecs
accept or refuse OpenVMM's vectors and cases of the `NVXS` session as they
say, and, on Linux, the guest's listener announcements match
`tests/vectors/nvxl-v1.txt`. The activation codecs decode each record and
Ready activation of `tests/vectors/activation-v2.txt` to its values, encode
them to its bytes, and refuse its invalid bytes. Property tests check that
records and activation records round-trip, that the activation record and
Ready activation decoders accept only canonical encodings, and that the
`NVXS` record parser, those decoders, and the host's and the guest's `EXEC`
and `MAPS` decoders return, never panic, on arbitrary bytes; those of the
egress policy compiler compare it with a reference model of the format
([Egress policy files](#egress-policy-files)). The NVXS test drives the
host's and the guest's roles of the session and their connections through a
broker that relays records as OpenVMM's does, with messages of an application
framing of its own that span records. The host-module test reads the crate's
sources and fails if the direct children
of `nvx::host` depend on one another in a cycle through the paths of their
production code, outside comments and test-only code. It lists the shortest
cycle through each dependency on one, with a reference for each of its
dependencies: break a cycle by moving the item that a lower child takes from a
higher one down, and re-exporting it at its old path. So the children stack in
layers, each above those that it depends on, which the test prints with
`--nocapture`.
The process tests, unit tests of `nvx::host::process`, start processes of
their own as an orchestrator does, name them by the start time that the
crate's private platform layer reads, wait for them through their verified
references, and find one again after the process that started it exited; the
spawn's unit tests check that a VMM inherits only its standard streams and the
descriptors or handles that it is allowed while another thread spawns
processes.
On Linux, with the `cli` feature, a loopback test drives
the host's private `ControlSession` through a broker shim into the real managed
agent's session, dispatch, and execution engine, and `nvx.py` also lints and
tests the guest binaries, and runs their integration tests against a build of
them:

```bash
python3 ../scripts/nvx.py check-guest-binaries
python3 ../scripts/nvx.py test-guest-binaries \
  --binaries ../build/nvx-rs-guest-binaries
```

The [security checklist](tests/security_checklist.rs) checks, through the
public API alone and without a VM, each security requirement that the host's
documentation states: the guest targets of an export and a read-only path
inside a writable one, the exported objects that each start checks again, a
VM's private directory and its new private log, and a snapshot's strict
directory. The exit proof of a saved VM, the owner of its endpoint, and its
recovery from the saved values need a process that the test names by its start
time, which only the crate's private platform layer reads, so the unit tests of
`nvx::host::vm` check them, with an idle copy of their test binary as the VM's
process. The checks that NVX makes of a restore need a captured snapshot,
so the unit tests of `nvx::host::vm`, `nvx::host::snapshot`, and
`nvx::host::restore` check them. The checks of a staged image and the claim of
a scratch image, which the CLI keeps in directories that only its own account
can write, are private to the crate, so the unit tests of its distro and
scratch bindings check them.

Two examples compile the documented workflows through the public API alone,
and nothing runs them: [`host-workflows`](examples/host-workflows.rs), a cold
boot, a capture into a snapshot, a restore from it, finding a VM again after its
caller restarts, and a stop; and [`guest-workflows`](examples/guest-workflows.rs),
for Linux guests, an init's time daemon, control session, capture, activation
of a restore, and mapping table. The guest example leaves the guest's space to
its caller: the steps of NVX's reference space, as nvx-init takes them, compile
in `guest::init::boot::tests::the_reference_space_steps_compile`. Clippy
compiles the examples with the rest of the crate's targets, and
[`external-launch`](examples/external-launch.rs) runs a cold boot end to end,
or a restore up to its restore-ready event.

The [case matrix](../doc/acceptance-case-matrix.md) gives each test of the
runtime adapter, of CI, and of the docs, and each microVM scenario, a fate:
kept, rewritten, or retired.

The
[`check-nvx-rs`](../.github/actions/check-nvx-rs/action.yml) action runs
rustfmt, Clippy with warnings denied for all features, for the library
with and without default features, and for the `serde`, `cli` and `testing`
features alone, the tests, a build and the tests of the library
without default features, rustdoc with
warnings denied, the minimum supported Rust version (1.89), and a macOS check.
On Linux, it also lints, builds, and tests the guest binaries through `nvx.py`,
and checks the crate's public API against its snapshots,
[`public-api/linux.txt`](public-api/linux.txt) and
[`public-api/windows.txt`](public-api/windows.txt), which list each public item
of the crate by kind and path, hidden items marked. A change to the public API
updates them, as [Rust crates](../doc/ci.md#rust-crates) describes:

```bash
python3 ../scripts/public_api.py --update
```

Another workflow runs cargo-mutants on the files and with the tests that a
dispatch names; see
[nvx-rs mutation testing](../doc/ci.md#nvx-rs-mutation-testing).

The guest's mount-ID tests also run from a thread with its own descriptor
table. An ignored test of `nvx::guest::root` checks mount points and mount
options from a thread with both its own descriptor table and a private mount
namespace. It mounts a temporary tmpfs and needs root, but no hypervisor;
run it as root with:

```bash
cargo test --locked --all-features --lib -- \
  --ignored --exact guest::root::tests::a_thread_with_private_tables_finds_its_mount_points_and_options
```

Ignored tests of `Root::mount_runtime` and `Root::switch` mount the runtime's
file systems on a temporary tmpfs root, in a thread's private mount namespace,
and check their options, device nodes, and time status, a call's undo, the
teardown, and the order of the runtime and the mappings beneath `/run`. They
also switch into the root from a thread with root and working directories of
its own, and check the mounts that stay and a mapping through descriptors opened
before the switch. They need root and Linux 6.8 or later; run them as root with:

```bash
cargo test --locked --all-features --lib -- \
  --ignored guest::root::tests::privileged
```

`tests/openvmm_vectors.rs` is an ignored test that compares the copies of
OpenVMM's vectors in `tests/vectors/openvmm/` with the files of the OpenVMM
submodule, which the `nvx-rs` job cannot check out, and the versions of
OpenVMM's snapshot format, saved-state schema, time ABI, and restore packets in
`nvx::contract` with the submodule's. The NVX CLI jobs run it once they check
the submodule out, and so does an OpenVMM pin promotion; see
[OpenVMM's vectors](tests/vectors/README.md#openvmms-vectors).
`tests/openvmm_cpu_profiles.rs` is an ignored test that compares
`src/host/cli/cpu_generations.json`, the identities of the built-in CPU
profiles from which [`run`](#run) guides a failed cold boot, with the profiles
that the submodule pins, and runs where that one does.

`src/host/session/tests/managed_lifecycle.rs` is an ignored unit test of the
host's private control session, which the `cli` feature builds: the managed
lifecycle of a guest whose init hands the boot to the managed agent, on a real
hypervisor: authenticated sessions and reconnects, request-scoped working
directories and environments, the workload identity, timeouts, refused working
directories, and stop, which the session drives. `nvx.py test-microvm
--scenario managed-lifecycle` builds OpenVMM's command line for its guest and
backend, runs the test with Cargo, and checks the outcome report and the guest
console that it leaves:

```bash
python3 ../scripts/nvx.py test-microvm --backend kvm --scenario managed-lifecycle
```

`tests/vm_lifecycle.rs` is an ignored test of a cold VM's lifecycle through
the public API alone, on a real hypervisor: `host::vm::create`, `spawn`,
`reattach` from the values that the caller saved, once the launch is gone,
and `stop`, through a request to the guest, nvx-init, over the Ubuntu layer
and a scratch upper. `nvx.py test-microvm --scenario vm-lifecycle` gives it
its inputs, runs it with Cargo, and checks the outcome report that it leaves:

```bash
python3 ../scripts/nvx.py test-microvm --backend kvm --scenario vm-lifecycle
```

### Specification tests

The tests take their expectations from what this README and the documents
that it links specify, never from output that an implementation recorded; see
[Test expectations](../doc/contribute.md#test-expectations). They replaced the
golden tests that compared the crate with records of what Python did.
[microsoft/nvx#722](https://github.com/microsoft/nvx/issues/722) retired those
one family at a time, with their fixtures:

| Golden tests | Fixtures | Replaced by |
| --- | --- | --- |
| Python semantics | `python.json` | The tests of the inputs, below ([microsoft/nvx#731](https://github.com/microsoft/nvx/issues/731)) |
| Launch profile and options | `sandbox.json` and `sandbox_options.json` | The sandbox specification tests, below ([microsoft/nvx#730](https://github.com/microsoft/nvx/issues/730)) |
| Egress policies | `egress-policy.json` | A reference model of the format, and properties; see [Egress policy files](#egress-policy-files) ([microsoft/nvx#729](https://github.com/microsoft/nvx/issues/729)) |
| CLI scenarios | `cli.json` | The [scenario tests](#scenario-tests) ([microsoft/nvx#728](https://github.com/microsoft/nvx/issues/728)) |

The golden bytes of the wire formats stay, as the
[conformance vectors](tests/vectors/README.md), the only check against peers
that share no code with the crate.

The tests of the inputs check what the
[sandbox input contract](#sandbox-input-contract) and
[the nvx-rs CLI](#the-nvx-rs-cli) specify, where the golden tests checked
`nvx::host::python`, the emulation of CPython that the ports used until Rust's
own facilities replaced it:

- Those of `nvx::uuid` check that only the canonical text of a UUID parses.
- Those of `host::cli::args` check that the CLI's integers are ASCII decimal
  digits in each option's range, and that messages quote values with Rust's
  debug formatting.
- Those of `host::cli::json` check that the CLI reads its JSON files strictly,
  with their versions, and writes them with sorted members, and those of the
  sandbox's options that environment files and distro manifests are strict
  JSON.
- Those of `host::paths` check that host paths are spelled with their
  components, made absolute without consulting the filesystem, and resolved
  through their links, on Linux and Windows, and those of the scratch image
  and of the export planner that paths that are not valid Unicode are refused.

The sandbox specification tests, those of `nvx::host::sandbox`, specify the
documented option grammar. Properties cover canonical layer UUIDs, reserved
targets, policy option sequences, and paths on the host's own filesystem. They check
that a launch refuses both systemd entrypoints whatever the distro layer's
manifest lists, and that verification refuses a manifest that lists `systemd`
whatever the entrypoint, as [Distro bindings](#distro-bindings) describes. The
CLI tests check that `sandbox` records the preceding mount while `run`
forwards absolute policy paths independently of option order, for OpenVMM to
attribute by containment.

The tests of the kernel command line take their expectations from the C code
with which Linux 6.18 reads it
([microsoft/nvx#723](https://github.com/microsoft/nvx/issues/723)), which
`protocol::cmdline::model` transcribes for the tests: `isspace`, `next_arg`,
and the loop of `parse_args`, which split the command line into parameters,
and `parameq`, `parameqn`, and `repair_env_string`, with which the kernel
matches parameters with their handlers. `nvx::protocol::cmdline::parameters`
must read as the model does every command line of up to six bytes among a
letter and those that `next_arg` treats apart, and random ones. With the model,
properties check that the time ABI's check of the guest's command line fails
exactly when the kernel applies a clock parameter that the ABI forbids, and
that the share targets that a sandbox accepts hold nothing at which the kernel
would end a parameter.
