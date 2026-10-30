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
│   ├── protocol/    NVXS session, NVXC frames, EXEC and MAPS payloads, guest features, kernel
│   │                command-line tokens, and the guest targets that live shares may not take;
│   │                no dependencies, shared by the host and the guest
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
│   ├── exec/        the execution engine: the streams, credit, and outcome of an execution's two
│   │                sides, without I/O, threads, or features, for any framing and host
│   ├── host/        feature `host` (default): artifacts, launch, network and egress policy files,
│   │                filesystem, NVXS connection, session, process, platform, VM instances,
│   │                sandbox stores, templates' manifests, compatibility keys, store, and leases,
│   │                outcome reports, file seals, and the sandbox launch profile of
│   │                `nvx-rs sandbox`, its distro and scratch bindings, and its prepared launches;
│   │                with feature `cli`, the `nvx-rs` CLI
│   ├── testing/     feature `testing`: the fake OpenVMM
│   ├── guest/       feature `guest`: the programs that run in Linux guests, the guest's NVXS
│   │                connection, the libraries through which a guest init or agent opens the
│   │                terminal on which it serves its session, starts a program, drives its
│   │                execution, alone or through the execution engine, collects its children's
│   │                statuses, settles its executions' cgroups, mounts host mappings beneath the
│   │                descriptors of the roots that it names, assembles and tears down a layered
│   │                root, reads a GPT disk's partition table through its block device, and
│   │                prepares a template's guest for a capture before its root's assembly, and
│   │                the typed calls through which it runs the time ABI's steps and the reseed
│   └── bin/         nvx-rs, nvx-fake-openvmm, and the guest binaries
├── tests/           fake-OpenVMM, CLI, NVXS session, process-supervision, checked-handoff, store,
│                    host-module layering, template-capture, and guest-binary tests, the CLI's
│                    scenario tests and their harness, in cli/, the store's record fixtures, the
│                    hand-written sandbox configurations of every format, the cases of the export
│                    and root boot tokens, and the template manifests that OpenVMM wrote, in
│                    fixtures/, and the conformance vectors of the wire formats and of a template's
│                    key, in vectors/
├── build.rs         bundled-release staging
└── artifacts.json   the NVX release that the `bundled` feature stages
```

The [module diagrams](../doc/modules/README.md) show the crate's modules, the
modules that each contains, and the dependencies between its top-level modules,
on Linux and Windows.

The [filesystem module](src/host/filesystem/mod.rs) keeps its request types and
planner together, with private siblings for guest and host paths, access policies,
OpenVMM limits, and recorded-plan checks and rendering. The
[instance module](src/host/instance/mod.rs) separates private-file helpers,
lock-held lifecycle transitions, and workload execution. The CLI's
[managed sandbox lifecycle](src/host/cli/sandbox/lifecycle/mod.rs) separates state
directories, outcome reports, start and exec, and stop and deprovision. Their
`mod.rs` files retain the existing entry points through re-exports; their
`tests.rs` files keep the unit tests at the same module paths.

The table below says in one line what each module is for, and links to the
module's section after the table, which holds the module's details. A change to
a module's details goes in its section; its row, at most 200 characters long,
changes only when the module's purpose does.

| Module | Purpose |
| --- | --- |
| [`nvx::protocol`](#nvxprotocol) | The codec of the control console's `NVXS` records, `NVXC` frames, and payloads, which the host, the guest agent, and the fake OpenVMM share |
| [`nvx::protocol::cmdline`](#nvxprotocolcmdline) | The kernel command line as Linux and the guest's init read it, with the virtio-mmio devices and the guest's DNS server that it names |
| [`nvx::protocol::export`](#nvxprotocolexport) | The boot tokens of the host export, the virtio-fs device from which a guest maps host paths, and the guest's parser of them |
| [`nvx::protocol::nvxs`](#nvxprotocolnvxs) | The NVXS session, independent of any application protocol: its records and the state machines of its host and guest roles |
| [`nvx::protocol::root`](#nvxprotocolroot) | The boot tokens that tell the guest which image its layered root assembles and where the root keeps its upper |
| [`nvx::protocol::share`](#nvxprotocolshare) | The guest targets that a sandbox's live shares may not take, which the host and the guest both refuse |
| [`nvx::protocol::status`](#nvxprotocolstatus) | The exit statuses with which a clone's guest ends its VM once it fails fast, and what each means |
| [`nvx::activation`](#nvxactivation) | The activation record of a layered root, which a clone reads in place of its captured configuration, and what Ready reports |
| [`nvx::contract`](#nvxcontract) | The versions of the contracts that the crate speaks with OpenVMM and the guest, and the compatibility of releases with them |
| [`nvx::net`](#nvxnet) | Network primitives that do not depend on the host: CIDR networks, hostnames, guest addresses, and gateways |
| [`nvx::gpt`](#nvxgpt) | Strict reading of GPT disks whose first partition configures an adapter and whose others hold a root's read-only layers |
| [`nvx::uuid`](#nvxuuid) | UUIDs, such as those of GPT disks and filesystems, in their canonical text and in GPT's layout |
| [`nvx::Hypervisor`](#nvxhypervisor) | KVM, MSHV, or WHP, with its serde form behind the `serde` feature |
| [`nvx::exec`](#nvxexec) | The execution engine: the streams, credit, and outcome of an execution's two sides, without I/O, for any framing |
| [`nvx::host::artifacts`](#nvxhostartifacts) | Locating the OpenVMM executable and the guest artifacts that a VM runs, and their runtime identity |
| [`nvx::host::launch`](#nvxhostlaunch) | `LaunchSpec`: the OpenVMM command line of a managed microVM, and the checks that every command line applies |
| [`nvx::host::network`](#nvxhostnetwork) | The one lowering of a network posture into OpenVMM options, for the CLI and external orchestrators |
| [`nvx::host::network::policy`](#nvxhostnetworkpolicy) | Egress policy files and their compilation into OpenVMM's native rules |
| [`nvx::host::filesystem`](#nvxhostfilesystem) | The planner of the host paths that every NVX caller exports to the guest through OpenVMM's virtio-fs device |
| [`nvx::host::store`](#nvxhoststore) | `SandboxStore`: the runtime records of one sandbox, which every host that supervises it shares under one lock |
| [`nvx::host::instance`](#nvxhostinstance) | `Instance`: a VM in a sandbox store of its own, which the store, the supervisor, and the control session drive |
| [`nvx::host::nvxs`](#nvxhostnvxs) | `HostConnection`: the host's role of the NVXS session over a platform transport |
| [`nvx::host::process`](#nvxhostprocess) | Verified references to OpenVMM processes, through which callers wait for them and end them |
| [`nvx::host::supervisor`](#nvxhostsupervisor) | The supervisor of a sandbox's VMM, beneath the CLI, `Instance`, and external orchestrators |
| [`nvx::host::template`](#nvxhosttemplate) | Templates, captures of a microVM from which clones are restored: their plans, manifests, keys, and store |
| [`nvx::host::seal`](#nvxhostseal) | `FileSeal`: what the host reports about an open file from its metadata alone, without reading it |
| [`nvx::host::session`, `platform`, `report`](#nvxhostsession-platform-report) | The authenticated control session, Linux and Windows primitives, and OpenVMM's outcome report |
| [`nvx::host::sandbox`](#nvxhostsandbox) | The launch profile of `nvx-rs sandbox`: its plans, live shares, option checks, bindings, and prepared launches |
| [`nvx::testing::fake_openvmm`](#nvxtestingfake_openvmm) | A fake OpenVMM that serves the control console and runs scripted workloads without a VM |
| [`nvx::host::cli`](#nvxhostcli) | Feature `cli`: the `nvx-rs` executable; see [The nvx-rs CLI](#the-nvx-rs-cli) |
| [`nvx::guest`](#nvxguest) | The programs that run in Linux guests; see [Guest binaries](#guest-binaries) |
| [`nvx::guest::mappings`](#nvxguestmappings) | `Mapper`: applies host mappings beneath the descriptors of their roots, for any guest init or agent |
| [`nvx::guest::nvxs`](#nvxguestnvxs) | `GuestConnection`: the guest's role of the NVXS session over a byte stream, such as the control console |
| [`nvx::guest::terminal`](#nvxguestterminal) | `Terminal`: the terminal on which a guest agent serves its session, such as the VM's control console |
| [`nvx::guest::spawn`](#nvxguestspawn) | `Command`: starts a program in a new process, for any guest init or agent |
| [`nvx::guest::entry`](#nvxguestentry) | `Entry`: the namespaces, view, root, and identity in which a program runs, for any guest init or agent |
| [`nvx::guest::process`](#nvxguestprocess) | `Child`: the status of one child that the caller names, and of no other |
| [`nvx::guest::cgroup`](#nvxguestcgroup) | Cgroup v2 handles through which a guest init or agent places, accounts, kills, and settles its executions |
| [`nvx::guest::exec`](#nvxguestexec) | `Execution`: drives one program as an execution, for any guest init or agent |
| [`nvx::guest::exec::binding`](#nvxguestexecbinding) | `Binding`: an `nvx::exec::Runner` bound to an `Execution`, the glue that a guest adapter would otherwise write |
| [`nvx::guest::time`](#nvxguesttime) | x86_64: the steps of NVX time ABI v1 as typed calls in the caller's process, for any guest init or agent |
| [`nvx::guest::clone`](#nvxguestclone) | x86_64: the preparation of a template's guest for a capture, and the repair and activation of each clone |
| [`nvx::guest::reseed`](#nvxguestreseed) | A restore's fresh entropy, and the reseed of the kernel's random number generator with it |
| [`nvx::guest::root`](#nvxguestroot) | The operations of a layered root: its bootstrap, assembly, shares, export, mappings, and teardown |
| [`nvx::guest::gpt`](#nvxguestgpt) | `read_disk`: the partition table of a GPT disk, read and checked through its block device |

GPT lower layers use `noload,errors=remount-ro`: the kernel neither loads nor
replays their journals, because even a journal whose file system does not
request recovery can cause an unbounded scan. The assembly still refuses any
layer whose superblock requests recovery, before mounting anything. This
policy does not change EROFS layers or writable ext4 scratches.

Lowering functions take the names by which messages refer to their inputs, such
as `EgressRules::name` and `PathList::name`, so callers' messages name their own
fields.

### `nvx::protocol`

The codec of the control console: `NVXS` outer records, `NVXC` application
frames, the `EXEC` payload that the host encodes and the guest agent decodes,
the `MAPS` payloads of the host mapping table, `GuestFeatures`, and the
`nvx_maps=` and `nvx_workload_account=create` tokens.

The [conformance vectors](tests/vectors/README.md) of version 1,
`tests/vectors/nvxc-v1.txt`, and OpenVMM's vectors and cases of the `NVXS`
session pin every record, frame, and payload independently of the codec, which
the host, the guest agent, and the fake OpenVMM share.

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
`HostExport`, the device's tag and mode, which
`nvx::guest::root::Root::mount_export` mounts, or `None`.
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

### `nvx::protocol::root`

The boot tokens of a layered root, which the host renders and guests parse by
the same rules: a `RootLayout` is a `RootImage` on the `distro` block device, an
EROFS filesystem or a GPT disk with the `nvx::uuid::Uuid` that the guest expects
of it, if any, over a `RootUpper`, the ext4 scratch disk or RAM.

`RootLayout::tokens` renders `nvx_layer=distro,0xd0003000,FORMAT[:IDENTITY]` and
`nvx_scratch=0xd0006000,ext4` or `nvx_overlay_upper=ramfs`, and
`RootLayout::parse` reads them from a kernel command line, and refuses
malformed, repeated, and conflicting tokens and the earlier form without a
format, with no fallback that probes the devices. It reads the virtio-mmio
devices of the command line with `nvx::protocol::cmdline`, to refuse a RAM upper
beside one at the scratch base. `tests/fixtures/root-tokens.json` holds its
cases, which the tests of the guest's shell init replay too.

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

### `nvx::activation`

The activation record of a layered root: one bounded, versioned, non-secret
record of the settings that its activation applies, which a clone restored from
a template reads in place of its captured configuration.

An `ActivationRecord` holds the schema's `VERSION`, a clone's `RestoreSection`,
the restore's `TransactionId` and the `TemplateReference` of its template, the
template's `StoreGeneration`, a nonzero publication ID that its store assigns in
order, and the `Identity` of its key, and the `Configuration`: the `Image`,
EROFS or GPT, with the `Identity` of its binding and the UUID or disk GUID that
the guest expects; the `Upper`, a tmpfs or the scratch with its binding's
`Identity` and its `Journal` policy; the `OverlayMode`; an `Export`, a directory
or an aggregate with its plan's `Identity`, and its 1 to 4,096 mappings; the
`Network`, its policy's `Identity`, a DNS address, the listeners' `Publication`,
and a proxy endpoint without credentials; the `WorkloadView`, its hostname,
`MachineIdMode`, and `WorkloadIdentity`, whose IDs are neither 0 nor `u32::MAX`;
and the `ExpectedResources`. The other inputs of the root's assembly are
invariants of the template.

`ActivationRecord::new` refuses a configuration that `Configuration::check`
finds past a public limit, `MAX_MAPPINGS`, `MAX_HOSTNAME_BYTES`, or
`MAX_PROXY_ENDPOINT_BYTES`, which keep every record within `MAX_RECORD_BYTES`,
or with an export without mappings, mappings without an export, a UID or GID of
0 or `u32::MAX`, a proxy endpoint with credentials, or a zero size.

`ActivationRecord::encode` writes a record's canonical encoding, its version,
then a tagged, length-prefixed field for each part in the order of their tags,
602 bytes at most, and `ActivationRecord::decode` reads exactly those bytes back
and refuses any others, naming the `Field` at fault.

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

### `nvx::exec`

The execution engine: the stream state, credit, ordering, and outcome of one
execution, for any framing that carries its messages, without I/O, threads,
global state, or features, so that it builds with `--no-default-features`.

A `Runner` is the side that runs the workload: it reads the workload's output
under the peer's credit, holds the input that the peer sent within its window
until the workload takes it, requests the terminations, discards, input close,
and settlement that its adapter applies, and returns one terminal `Outcome`,
which tells whether the execution is reusable or fenced.

A `Peer` is the other side: it takes output into windows that its sink empties,
returns credit as the sink takes bytes, sends input within the runner's window,
and ends the execution on a cancellation, a timeout, a drop, a reset, or a
disconnection.

Both take the current `Instant` from the caller, report their next deadline, and
never wait, so a blocked stream, sink, window, or transport stops no timer,
cancellation, or other execution, and both bound their memory.

The adapter chooses one byte window per stream, or frames charged against one
pool that stdout and stderr share, and the maximum chunk, in `Limits`; a `Key`'s
generation tells stale messages from foreign ones.

### `nvx::host::artifacts`

`Artifacts` (also `nvx::Artifacts`): the OpenVMM executable, guest kernel, and
Alpine initramfs, from a release, a repository checkout, the environment, a
directory next to the executable, or the bundle. Artifacts located in a release
or checkout keep its `ArtifactDir`, and `Artifacts::runtime_identity` reads
their `RuntimeIdentity` from its install metadata; artifacts named by path have
none.

`ArtifactDir` and `Guest`: every artifact of the guests that `nvx.py` runs, the
OpenVMM executable, the kernel, the Alpine, Ubuntu, and Azure Linux initramfs
images and their package manifests, and the Ubuntu distro layer and its
manifest, in a checkout's `build/` and `openvmm/target/release/` or a release's
`guest/` and `bin/`, checked in the order and with the messages of `nvx.py`.
`ArtifactPaths` lists them all and serializes as JSON.

`ArtifactDir::runtime_identity` reads the `RuntimeIdentity` of the OpenVMM
executable, kernel, and Alpine initramfs from the install metadata, a release's
`SHA256SUMS` or a checkout's build provenance, without hashing an artifact.

### `nvx::host::launch`

`LaunchSpec` and OpenVMM's command line: the managed lifecycle, the
`WorkloadIdentity`, the 1024-byte kernel command line, its reserved parameters,
the 32,767-character limit of a Windows command line, and `LaunchSpec::check`,
which every command line applies to the memory size, the non-root identity, the
hostname, the guest network, and the kernel parameters.

`kernel_parameters_problem` checks the extra kernel parameters as the guest's
kernel reads them, with `nvx::protocol::cmdline`: trimmed, split at every byte
that the kernel takes for whitespace, including the 0xa0 that ends the UTF-8
encoding of characters such as `à`, with names that match whether they spell a
dash or an underscore. It refuses NUL characters, quotes, and the `--`
delimiter, after which the kernel would leave the `hostname=` parameter that
follows the extra parameters to init, and the reserved parameters: those named
`tsc` or `hostname`, with or without a value, and those whose names start with
`nvx_` or `nvx-`.

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

### `nvx::host::store`

`SandboxStore`: the runtime records of one sandbox in a private directory with
one lock, which every host that supervises the sandbox shares.

The `RuntimeRecord` names the running VM: the `ProcessRef` of its VMM, its
control endpoint, and the `RuntimeIdentity` that it was launched with, the
identities of its OpenVMM, kernel, and initramfs from install or staging
metadata and its boot and control contract versions, which decide whether
another build of the host may reattach to it.

The `LaunchJournal` of a launch holds a marker written before the spawn, the
VMM's `ProcessRef` right after it, and an inherited claim on the VMM's log,
which proves that a launch that never published its endpoint has exited, so that
recovery ends an interrupted launch and signals only a VMM that it holds through
a verified `Process`.

Records are JSON in one versioned format, and records in any other format fail
rather than being migrated.

The caller declares the files that it keeps in the directory, its configuration
record, whose presence means that the sandbox is provisioned, and any others, as
`CallerFiles`, and takes the lifecycle locks with them:
`SandboxStore::lock_for_provision` creates the directory, again if a removal
deletes it meanwhile, `SandboxStore::lock_existing` fails with
`ErrorKind::NotFound` once it is gone, and a holder that leaves the directory
without the configuration record removes its lock file as it releases the lock.

`LockedStore::remove` removes a sandbox that no VMM can run in, for a caller
that declares its configuration record: the store's files, the caller's, and the
directory, unless it holds entries that are neither, which it refuses and keeps,
and `Lingering` waits for open files, such as the lock files of other
transitions, that keep the emptied directory; both remove the directory only
while the path still names it.

The store also owns the file in which the VMM stages its outcome report, which a
VMM that is killed while it writes the report leaves, and removes it once no
process of its launch runs.

### `nvx::host::instance`

`Instance`: a VM in a sandbox store of its own, which `nvx::host::store`,
`nvx::host::supervisor`, and `nvx::host::session` drive, and in which the caller
keeps the files that it declares as `CallerFiles`.

Under the store's lifecycle lock of an existing sandbox, `LockedInstance::find`
reattaches to the VM, ends a start that was interrupted, and reports a VM of
another runtime identity as `Mismatched` rather than driving it;
`LockedInstance::start` launches OpenVMM through the supervisor with a fresh
capability, under the caller's `RuntimeIdentity`, waits until the guest is ready
and has mounted the mapped paths, and commits the launch; `LockedInstance::stop`
stops the VM with a forced fallback; `LockedInstance::terminate` stops a
`Mismatched` VM by identity-verified forced termination, without a control
session or its outcome report; and `LockedInstance::remove` removes a stopped
instance, with the caller's files and its directory, through the store.

`Instance::exec` runs workloads without the lock and pumps their output.

`write_json` and `read_json` keep the configuration records that callers store
beside an instance.

### `nvx::host::nvxs`

`HostConnection`: the host's role of the NVXS session over a platform transport,
on which `nvx::host::session` and other host applications run their protocols.
It attaches with the launch capability, waiting while the broker asks it to,
sends application bytes in as many DATA records as they take, and returns each
DATA payload from the guest, with deadlines; a receive that times out keeps a
partial record.

### `nvx::host::process`

Verified process references. A `ProcessRef` names one process by its ID, its
start time, and the `BootId` of the host boot during which it started.

`Process::open` checks a reference and holds its process through a pidfd on
Linux or a process handle on Windows, whether or not the caller started it, so
that waits sleep until the process exits and termination never reaches a process
that reuses its ID. A reference from an earlier boot names an exited process,
and a process whose identity this user cannot check, such as another user's,
makes `Process::open` fail rather than report an exit. Linux hosts without
pidfds poll the reference instead, and cannot terminate the process.

### `nvx::host::supervisor`

The supervisor of a sandbox's VMM, beneath the CLI, `Instance`, and external
orchestrators, which keep the configuration, the authorization, and the
application protocol.

`launch` runs the store's launch steps under the sandbox's lock, as a launch
whose `LaunchId` the caller reserved beforehand, a fresh one for each launch, or
that a clone's lease reserves: it writes the journal's marker, which names the
launch, spawns OpenVMM with the claimed log, records its `ProcessRef` right
after, and returns a `Starting` launch that the caller commits once the VMM
serves its control endpoint, or abandons. A launch that fails never leaves an
unrecorded VMM running.

`VmmCommand::inherit` lists the descriptors (Linux) or handles (Windows) that
the VMM inherits under their numbers, besides its standard streams, and it
inherits nothing else of the caller's: on Linux, they must be close-on-exec, a
flag that only the VMM's own process clears; on Windows, they join the VMM's
explicit handle list.

On Windows, the VMM's standard handles and its allowed handles are inheritable
while it is created, and a process that another thread creates meanwhile
without a handle list, as `std::process::Command` creates one, inherits them
and holds them until it exits: the read end of the VMM's capability pipe, its
log, and the handles that it allows. The VMM runs in a job object of its own,
named after its log, whose processes are the launch's claim while such a
process holds the log, so a recovery still ends the launch once the VMM exits.
Each spawn of a VMM holds a `SpawnLock` throughout that window, so a caller that
launches VMMs spawns its other processes with `spawn_command`, which spawns a
`std::process::Command` under one, or under one that `lock_spawns` returns. On
other hosts, the lock holds nothing.

`Vmm::stop` runs one graceful step of the caller's and waits for the VMM's exit
event until a deadline, and takes a bounded forced step through the VMM's
verified identity only on request.

`Vmm::stop_unless_exited` stops the VM the same way only if its VMM still runs
as the stop begins, and otherwise stops nothing and keeps the runtime record of
its launch, so that `reattach` finds that the VM exited without its holder.

`Vmm::terminate` takes that forced step alone, within the caller's bound, for a
VM whose guest the caller must not ask anything, such as one of another runtime
identity.

`reattach` recovers an interrupted launch, and holds the recorded VMM again
after the orchestrator restarts within the same host boot: `Live`, `Mismatched`
with another runtime identity, `Exited`, or `Indeterminate`.

A clone's launch takes its locks in one order, the template store's, then the
sandbox's, which guards its clone binding and launch journal, then the scratch
image's claim, and holds none for the VM's lifetime; its lease lasts through
crashes and reattaches, until a release ends it once a stop, a termination, or a
reattach has verified the VM's exit.

### `nvx::host::template`

The metadata of templates, captures of a microVM from which independent clones
are restored.

`TemplatePlan::new` lowers an adapter's `SandboxPlan` into the plan of a
template's capture, without I/O: the placeholder `distro`, which its
`DistroBinding` binds, and whose length is the capacity of every clone's image,
over a dormant scratch device, the sparse file `SCRATCH_NAME` in the capture's
transaction directory, with the dormant filesystem slot, and no NIC or one at
the guest's address of the adapter's network, under the complete deny-all
policy, whatever the network's own, which each clone's restore replaces with the
clone's; `TemplateResources`, with the processors, 1, 2, 4, or 8, and those
online at boot, as many or fewer, and RAM and a `--memory-capacity` in blocks of
`MEMORY_BLOCK_SIZE`; the kernel command line `nvx_sandbox=1`, with `maxcpus=`
and the processors online at boot when they are fewer, so that the capture's
whole command line holds tokens of OpenVMM's platform allowlist alone, and no
tenant data, which the tests check with a copy of the allowlist's source; and
the capture's options, `--snapshot-tier platform`, a `--snapshot-destination`,
`SNAPSHOT_NAME` in the transaction directory, and
`--snapshot-block-identity generation` with the capture's bound-block
`StorageGeneration`, so that the capture hashes no block device.

`TemplateManifest::read` reads the `manifest.bin` that OpenVMM wrote for a
capture, at most `MAX_MANIFEST_LEN` bytes of a regular file that is not a link
or other reparse point, and refuses another snapshot format version, an unknown,
mistyped, or repeated field, truncated bytes, and a capture that is not a
template's: a platform-tier capture for clones, with a read-only `distro` whose
identity is unbound, a fresh scratch, and the dormant filesystem slot.

`CaptureClass` holds the invariant inputs of a capture, known before it: the
runtime, the guest's build, the backend, the requested CPU profile, the
processors, those online at boot, and RAM, the geometries of the disks, the
network, of which it takes what the template captures alone, its NIC at the
guest's address under the complete deny-all policy, and the adapter's other
`Invariants`.

`TemplateKey::new` checks that the capture made the machine that its class
requested, and derives the template's compatibility key, from metadata alone:
the runtime and contract versions through which its clones are restored, the
backend, CPU profile and effective CPUID, topology, rates, resources, kernel
command line, disks, and devices of its machine, with the NIC's addresses and
the identity of the complete deny-all policy under which the capture attached
it, which each clone's restore replaces with the clone's own, so that a template
with a NIC serves the clones of every network policy at its guest's address. A
capture's own metadata, such as its time anchor and host, stays out of the key.

A key and a class each have one canonical, versioned encoding, whose
domain-separated SHA-256 digest is its identity, a `KeyIdentity`, which converts
to and from the `nvx::activation::Identity` by which a clone's activation record
names its template's key, or a `ClassIdentity`; the
[conformance vectors](tests/vectors/README.md) of version 1,
`tests/vectors/template-key-v1.txt`, pin both encodings and identities. The
key's encoding, version 1, leaves the network policy out, since templates
capture their NIC under the complete deny-all policy, and has not been released,
so a template store that an earlier build created is not supported: delete it
and create a new one. There is no migration.

`Store` keeps the generations of templates that a host published, apart from
every sandbox's store, at a location that the adapter chooses: a private
directory with an identity of its own, a `StoreId`, the immutable payload of
each generation, its key's canonical encoding beside the snapshot that OpenVMM
published, and the versioned `Index` of them, which lists each generation with
the identities of its key and capture class and its own metadata, such as its
time contract.

`Store::create` creates a store, or opens the one that its directory holds,
crash-safely and once between concurrent creations, and refuses a directory with
foreign entries; `Store::open` opens one.

Its records are JSON in one format, replaced atomically, and records of another
format fail rather than being migrated. Its directories must be private to the
current user, and it refuses links and, on Windows, other reparse points among
its directories and files.

Four namespaces stay apart: the `nvx::activation::StoreGeneration`, the host's
publication ID, which the index assigns in the order of publication and never
twice, and by which a clone's activation record names its template; the
bound-block `StorageGeneration` of a capture, which belongs to the storage
owner; the time lineage counter of the time contract; and the VM generation ID,
which the store never records.

`TimeContract::eligibility` decides, apart from compatibility, whether a clone
may restore a capture at a host's time, a `HostTime`, which `HostTime::now`
samples as OpenVMM does: within the boot of the capture, by the host's monotonic
clock, and otherwise by UTC, the downtime must not be negative, and the interval
by which a restore advances the clone's VM time at most `MAX_DOWNTIME_NS`, 30
days.

`Store::select` selects the latest eligible generation of a key that no
tombstone quarantines.

`Store::acquire` leases a generation to a clone in one transaction, under the
store's lock and then the sandbox's: it checks the generation's eligibility,
writes the lease's record in the store and the clone binding, `clone.json`,
beside the sandbox's launch journal, and returns the lease with the sandbox's
lock, the only one under which the caller launches the clone, once, as the
launch that the lease reserves, `Lease::launch`.

A lease never expires: `Store::release` ends it once the sandbox records no VM
and no launch.

`ClonePlan::new` plans a clone under the lock that acquired its lease, before
the launch that the lease reserves, from metadata alone: the leased generation,
its key, which must speak the current contracts, the boot and control contracts
of the runtime that captured it included, and its snapshot; a network that
attaches a NIC if and only if the template has one, at the guest's address of
the template's NIC, which OpenVMM restores from the snapshot, so that a template
with a NIC serves the clones of every network policy at its guest's address; a
processor target of 1, 2, 4, or 8 from the template's processors online at boot
to its capacity; a memory
target in whole blocks
of `MEMORY_BLOCK_SIZE` from the template's RAM to its capacity; the clone's
image, which the check of its `DistroBinding` opens; its upper, either the
caller's scratch image, which the check of its `ScratchBinding` claims, and
which nothing formats, copies, or reads, or RAM, whose restore declares the
template's scratch absent, so that OpenVMM attaches it with no file and the plan
creates none;
its export, if it has one, an `ExportPlan` that `filesystem::plan` made for the
container root, an aggregate of up to 256 host directories and files, as a cold
launch exports, none of which may overlap the template store, by the paths that
the plan and the store's directory resolve to, let the guest write in a
directory through which the store's path leads, links included, since the host
reaches the store, and OpenVMM its snapshots, by that path, or be, hold, or for
a file lie in the directory of the clone's sandbox, in which the host writes the
clone's records, checks of paths that leave two rules to whoever names the
export, as they leave the directories above the store: the export must not
reach the store through an alias of it that an administrator configures, such as
a bind mount or a mount point of the store, or of a directory above or inside
it, a loopback share, or a hard link of a payload, nor lie inside a directory
that a writable export of another sandbox or clone holds, since OpenVMM resolves
an export's paths again at each launch and restore, so that the directories
above the export's directories and files must be ones that no guest may change,
as those above a clone's block devices must; and a fresh capability.

It builds the clone's activation record, `ClonePlan::activation`, which the
clone's guest applies in place of the configuration that the template captured,
from the request's `WorkloadView`, overlay mode, journal policy, and bound of a
GPT disk's first partition, and from the checked inputs: a restore section with
a fresh transaction ID of 16 random bytes and the lease's `TemplateReference`;
the image, an EROFS layer or a GPT disk, as its binding names it, with its
binding's identity and the filesystem UUID or disk GUID that the adapter's plan
expects of it, and for a GPT disk the bound of its first partition, whose
partition table the guest checks with `nvx::gpt` as at a cold launch; a tmpfs,
or the scratch image's binding identity with the journal policy; the export, an
aggregate, with its plan's identity, and its mappings, the plan's binds, from 1
to 4,096, or no export and no mappings, which leaves the slot dormant; the
clone's network, with its identity, the DNS server that OpenVMM serves the guest
at a gateway under its policy, if any, whether the guest publishes its
listeners, as under a policy that allows ingress or host-loopback access both
ways, and the guest's proxy URL; and the processor and memory targets. The
identity of a binding, of an export plan, or of a network is the SHA-256 digest
of a domain and of its serde form, never of a file's content.

The image must present the logical and physical block sizes of the template's
placeholder and be no longer than it, since a restore presents every clone's
image at the placeholder's length, its capacity, and reads zeros past the end of
a shorter image, while a scratch image must present the `BlockGeometry` of the
template's scratch, its length included, which a restore compares; both must be,
at the paths by which OpenVMM opens them, the regular files that the checks
found, which are not links or other reparse points, and which no one else may
change. Neither the image nor a scratch image may be a file that the sandbox's
store owns, such as its log or its records, by any path, since the store's
transitions replace or remove them, and a link or a directory under one of the
store's names is refused.

`ClonePlan::launch` then launches the clone as the launch that its lease
reserves, once it has checked that the sandbox's clone binding records the
lease, that the sandbox records no failure of the clone, which fails once under
its lease, and, with `ExportPlan::verify`, that the objects of its export, if it
has one, are still those that its plan found, and returns a `Restoring`: it
writes the clone's activation record in its canonical encoding,
`clone-activation.bin`, from which a caller resends it unchanged; it listens for
the VMM's restore-ready event, at the socket `restore.sock` on Linux, and at a
named pipe of a random name on Windows; and it runs `supervisor::launch` with
OpenVMM's restore of the clone's devices, with `--restore-absent-scratch` for a
clone in RAM, and of its network, whose policy `--restore-network-policy`
installs on the template's NIC in place of the complete deny-all policy, its
export bound to the dormant slot as a cold launch attaches it,
`--mount-aggregate` with a `--mount-child` of each of its directories and files,
their hidden, re-exposed, and writable paths, and its owner,
`--restore-snapshot` with the generation's snapshot in place, which no clone
copies or changes, so that clones of one generation restore it at once,
`--restore-memory` with the memory target, `--restore-processors` with the
processor target, for a template whose processors were not all online at boot,
and `--restore-ready-path`, under the
template's backend, CPU profile, and processors, with a boot console that
discards its output, as cold boots have, and the control console, but no kernel,
initramfs, command line, or guest address, which the snapshot fixes.

On Linux, where OpenVMM requires a restore's control socket in the snapshot's
parent, the clone's VMM listens in the generation's payload, at `<launch>.sock`,
named after the launch that the lease reserves, whose path must be shorter than
108 bytes with or without the payload's links resolved; on Windows, it listens
at a named pipe, as every launch does. The VMM inherits the claim of a scratch
image, on Windows through the image's guard, once the launch has closed the
claim's handle, which OpenVMM's open of the image would refuse.

A payload holds the sockets of its clones only while they lease it: the VMM
removes its socket as it exits, a stop or a recovery removes the one that an
abrupt exit leaves, and the release makes the socket's absence durable before
the lease ends, and removes the record and `restore.sock`.

`Restoring::activate` then runs the clone's restore sequence through the
adapter's framing of its control session, a `CloneControl`, by a
restore-to-Ready deadline that runs from the launch, `RESTORE_TO_READY_TIMEOUT`,
60 seconds by default: it reads the restore-ready event as soon as it comes,
refusing another process's connection and bytes that differ from the event, and
reading nothing past it, since OpenVMM on Windows waits until the host has read
it, and then closes its end; only then connects to the clone's control endpoint,
whose console bounds the host's authentication from the connection on; attaches
with the launch's capability; sends the record, read again from
`clone-activation.bin`, which must still be byte for byte the launch's, in the
adapter's Activate, as the session's first request; and checks that Ready
reports the record's transaction ID. It replaces a session that the adapter
lost, as `CloneControl::lost` tells, with a new one that sends the same record,
in at most `MAX_ACTIVATION_SESSIONS` sessions by the same deadline, and retries
nothing else.

`Activated::start` then sends the clone's mapping table, if it has an export,
through `CloneControl::map`, in order, as a cold launch's guest receives it,
each path by its child's index, and starts the sandbox once the guest applied
every mapping, both by a Ready-to-Start deadline from the check of Ready,
`READY_TO_START_TIMEOUT`, 30 seconds by default, never sending either again,
and `Started::commit` commits the launch, which writes its runtime record. A
failure, a partial or failed mapping table included, ends the launch as
`Starting::abandon` does; a crash before the commit leaves the launch's
journal, whose recovery ends the VMM, while the lease lasts until the release,
and a crash after it leaves a VM that reattaches, and keeps its lease while it
runs.

A failure of a clone is a `CloneFailure`, whose `Evidence` attributes it to an
`Outcome`, at a `Stage`, once the clone got as far as its `Progress`: the
request, for a denied authorization, a stale binding, invalid targets, format,
settings, Activate, or mappings, such as an export that no clone takes, or whose
objects changed since its plan, or a mapping table that the guest refused, as
`CloneControl::refused` tells, or an image, a scratch, or a root that the
template cannot serve, and for a supplied file of the wrong identity, length, or
geometry, which the host's staging takes instead once the caller proves it,
`CloneFailure::with_staging_proven`; the host, for an endpoint that cannot be
bound, resource exhaustion, a host that cannot restore the template, another
runtime, or a failure of its own operations; unknown, for a lost status, a
timeout, a reset, a forced kill, or a broken protocol; the template, for a
malformed generation, or a defect of its capture or repair that a diagnosis
attributes to it, the only outcome that quarantines the generation, through its
tombstone, `CloneFailure::quarantine`; and the repair stage, for a status of the
clone's repair alone.

OpenVMM's outcome report gives the status with which the guest ended the clone's
VM, which `status_evidence` attributes, and which prevails over what the host or
the caller saw, but not over evidence of the template, which shows the
provenance that a status alone leaves to establish: a status of the repair at
any point, and one of the activation before the start was sent.

A VMM that exits before its restore-ready event, as OpenVMM does once it cannot
restore the snapshot, after it closed the event's endpoint, which the sequence
then gives ten seconds to exit, leaves the line with which OpenVMM reported its
fatal error in its log, which the failure's message keeps, and whose time ABI
code, if it says that the host cannot present the template's CPU profile, TSC,
or LAPIC timer, attributes the failure to the host.

From the launch on, a failure ends the clone's VMM through its verified
identity, and the sandbox keeps the clone's progress log, `clone-progress.log`,
which records each step from the spawn to the commit, after a first line that
names the launch and the boot of the host, a few lines that it appends without a
sync, and the record of its failure, `clone-failure.json`, JSON in format
`FAILURE_FORMAT`, which it writes before the teardown, with the teardown
pending, so that a crash of the caller during the teardown leaves it for a
reattach to finish, and again once the teardown ended, with the stage, the
evidence and the status, the original outcome, the transaction, the generation,
the progress, and the teardown: no VM, an exit or a termination that the host
verified, or an unresolved one, which keeps the launch's journal or runtime
record, and the lease; a release removes both, and a sandbox that boots cold
writes neither. A failure that cannot be recorded before its teardown keeps the
launch's journal or runtime record, and the lease, so that a reattach still
finds that the clone failed.

A step or a check of the caller's own that fails once the clone is ready, or
once its sandbox started, fails the clone with the caller's evidence through
`Activated::fail` or `Started::fail`, which end the launch and record the
failure as the sequence's own failures do.

`reattach_clone` takes a clone over after the caller of its launch restarted: it
reattaches by the identity of its VMM's process, checks a committed clone in a
new session, in which the clone answers the byte-equal Activate of
`clone-activation.bin` with the Ready that it answered once, and ends a clone
that another runtime identity launched, or that fails the check, through its
verified identity; it records the failure of a launch that a crash interrupted,
but not the journal that a commit may leave beside its runtime record,
classifies a VM that exited by its outcome report, which is the clone's VM's
once its launch began, whether or not a crash left a record of its spawn,
records a failure before it ends a VMM or clears the runtime record of one that
exited, and fails, leaving them as they were, if it cannot, finishes a failure
that a crash left pending as its teardown would have recorded it, keeping its
transaction, resolves the teardown of an earlier failure with what the VMM left
as it ended, its outcome report and OpenVMM's fatal error, attributes a status
of the guest's as one that a workload may have given once the clone's progress
log is lost, damaged, or was written during another boot of the host, whose
crash may have lost its last lines, and reads nothing of the template store, so
that a tombstone or a retirement of the generation leaves the clone its lease.

`CloneRecords::read` reads what the sandbox keeps of a clone, for the operations
that list, inspect, or remove clones.

`Store::tombstone` quarantines a generation, which takes no new lease while its
leases keep working, and `Store::retire` deletes it once its leases have
drained, crash-safely, removing the records of leases that no clone binding
names any more.

`Store::create_template` serves a `Capture`, an adapter's plan for a capture
class with the placeholder's binding and the runtime's artifacts, with a
generation of its class that the store holds and that is eligible at the host's
time, which it reads once it holds the class's slot, `Created::Existing`, or
captures one, `Created::Captured`, while `Store::refresh_template` always
captures, and leaves older generations and their leases untouched.

A capture runs in its class's slot under `captures/`, a sandbox store whose lock
serializes the class's captures, which comes before the store's lock, and which
keeps the log and outcome report of the VMM of the class's latest capture: in a
private transaction on the store's volume, which is also the sandbox store of
its VM, since OpenVMM requires the control socket of a VM that captures a
snapshot, on Linux, beside the snapshot's destination, it creates the dormant
scratch, checks the placeholder through its binding, and both block devices at
their paths, regular files that are not links or other reparse points, that
belong to the user or an administrator, and that no one else may change, and
launches the template's VM through `supervisor::launch` from the transaction,
holding no handle that could write either file, since OpenVMM opens them itself
by path, as it does a restore's. Once the VMM has exited, it checks the outcome
report, the snapshot's manifest and artifacts, and the key, and publishes the
transaction in two commits, a rename into `generations/` that replaces nothing,
then the index.

A capture that fails publishes nothing, and `Store::recover_captures` ends the
captures that a crash interrupted, publishing those that were checked, and
retiring the others.

### `nvx::host::seal`

`FileSeal`: what the host reports about an open file from its metadata alone,
without reading it: the platform scheme, the device or volume serial number, the
file ID, the kind, the length, the modification time, and on Linux the change
time, with a versioned serde form. A seal detects accidental change, not
tampering; see [Distro bindings](#distro-bindings).

### `nvx::host::session`, `platform`, `report`

The authenticated control session, which carries NVX's control protocol, `NVXC`,
one frame per DATA record of a `HostConnection`; Linux and Windows primitives;
and OpenVMM's outcome report.

### `nvx::host::sandbox`

The launch profile of `nvx-rs sandbox`, with native input validation:
`SandboxLaunch`, with the distro `SandboxLayer`, an EROFS layer or a GPT disk,
the one layer that a sandbox takes, its `SandboxUpper`, the ext4 scratch disk
or RAM, the entrypoint, arguments, hostname, workload identity, and
`SandboxSettings` resource limits (`Option<u64>` for memory and `Option<u32>`
for processes), and the `SandboxPlan` that it makes of them.

`plan`: `SandboxPlan`, the typed plan of a cold layered launch, which is
persistable in a versioned serde form: one read-only `distro` block device, an
EROFS filesystem or a GPT disk with the identity that the guest expects of it,
if any, an upper, the ext4 scratch disk or RAM, the launch settings, the
`ExportPlan` of its live shares, and optionally a `SandboxNetwork`, the guest
network and its typed `NetworkPolicy`, whose proxy's URL as the guest sees it
the plan reports. The plan of an adapter that boots its own guest,
`SandboxPlan::adapter`, holds no launch settings and no export plan.

A plan knows only the `distro` and `scratch` `BlockRole`s, and gives, for every
combination, its `--microvm-sandbox-block` arguments, with
`--microvm-workload-identity` when it has launch settings, with no scratch block
device for a RAM upper, and those of its network, which `nvx::host::network`
lowers, and its kernel command line, with the boot tokens of
`nvx::protocol::root`, within the 1024-byte budget that OpenVMM leaves it,
refusing one whose root tokens the guest's parser refuses. Both deliver the live
shares as the managed lifecycle does: the arguments export them at
`nvx::host::filesystem::GUEST_EXPORT`, the command line announces the number of
their mappings with `nvx_maps=`, and `mapping_table` gives the ordered table
that `start` sends once the guest agent is ready; the CLI's one-shot `run` still
renders them as boot tokens.

An adapter's plan renders the caller's tokens before those of its root, and
refuses tokens that its root or OpenVMM's shares own.

`mount`: `SandboxMount` requests of live shares of host directories and files,
their guest targets and the reserved ones, their access policies and owner, and
the checks of their host paths; `plan_mounts`, which plans them through
`nvx::host::filesystem` for the container root, and `boot_shares`, which renders
a plan as the OpenVMM arguments and boot tokens of one share or of the aggregate
of several, as the one-shot lifecycle delivers them.

`options`: the checks that `nvx-rs sandbox` and `nvx-rs run` apply to their
options: the attribution of `--mount-deny`, `--mount-allow`, and `--mount-write`
to shares, the systemd entrypoint check and the explicit verification of the
distro layer, the targets of `nvx-rs run` shares, and the managed exec
environment; see [Sandbox input contract](#sandbox-input-contract).

`distro`: the `DistroBinding` of a distro layer, an EROFS layer or a GPT disk,
which explicit verification makes or a staging caller imports, and its check
from metadata alone, which returns the layer open; see
[Distro bindings](#distro-bindings).

`scratch`: the `ScratchBinding` of a scratch image, its identity, kind, and
length, and its check from metadata alone, which claims the image for one
sandbox at a time and returns it open.

`CheckedDistro::geometry` and `CheckedScratch::geometry` read, through the
checked files, the `BlockGeometry` that OpenVMM presents for them: their length,
a nonzero multiple of 512 bytes, logical blocks of 512 bytes, and physical
blocks of the I/O block size of their filesystem on Linux, and of 4096 bytes on
Windows.

`prepared`: `PreparedLaunch`, the launch of a plan with the checked files of its
block devices, which renders the plan's OpenVMM arguments and kernel command
line, naming the checked files as `/proc/self/fd/N` on Linux and by their final
paths on Windows, lists the descriptors or handles that the VMM inherits through
`VmmCommand::inherit`, and releases the checked files, and on Windows the VMM's
copies of them, at authenticated Ready; see [Checked handoff](#checked-handoff).

### `nvx::testing::fake_openvmm`

A fake OpenVMM that serves the control console and runs scripted workloads
without a VM, for direct launches and for the managed and one-shot sandboxes of
`nvx-rs sandbox`, and runs the VMs of templates' captures, which publish a
snapshot, or fail, as a scenario asks.

### `nvx::host::cli`

Feature `cli`: the `nvx-rs` executable; see [The nvx-rs CLI](#the-nvx-rs-cli).

### `nvx::guest`

The programs that run in Linux guests; see [Guest binaries](#guest-binaries).

### `nvx::guest::mappings`

`Mapper`: applies host mappings for any guest init or agent beneath the
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

`Mapper::teardown` detaches a mapping that a rename moved and that the attach
failed to detach, then unmounts the mappings in reverse order, and reports each
failure.

`bind` applies one runtime bind through the same walk and attach.

Linux 6.8 or later. NVX's managed agent applies its host mapping table through
it, beneath `/` in direct mode and beneath the container's assembled root in
sandbox mode, and refuses an empty source, since its export is an aggregate,
whose root lists its children; NVX's hosts, whose exports are all aggregates,
send none; a layered `Root` applies its live shares and mappings through it.

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

### `nvx::guest::spawn`

`Command`: starts a program in a new process for any guest init or agent, with
its arguments, environment, a `PATH` in which it looks for a program named
without a `/`, root and working directories, standard streams, inherited
descriptors, cgroup, process group or new session, a `Barrier` at which the
program waits until the caller releases it, a reset that gives every signal its
default disposition and blocks none, and an `entry::Prepared` execution to
enter, without NVX's control protocol, managed agent, or workload identity.

Its `LaunchReport` tells whether the program started, or which `LaunchStage`
failed and why. Each stage keeps its number, `LaunchStage::code`, which
`LaunchStage::from_code` turns back into the stage, and a new stage takes a
number of its own.

NVX's managed agent is one caller.

### `nvx::guest::entry`

`Entry`: the execution that a program enters, for any guest init or agent, from
typed inputs: a root, which is a mount, moved over `/` in new mount, PID, and
UTS namespaces, where the program is PID 1 and leads a new session; a hostname;
a fresh procfs, a read-only sysfs, and a `/dev` of its own with only the
allowlisted `DeviceNode`s, a devpts, and a `/dev/shm`, every other mount
`nodev`; the `MachineId`, the image's own or one generated by
`MachineId::generate`, the time status, and the resolver file that
`Entry::resolver` names, if any, which `Entry::bind_runtime` binds read-only
into the root once, through `mappings::bind`'s walk and attach, for every later
execution, and returns as `RuntimeBinds`, which `RuntimeBinds::remove` unmounts
in reverse order before the root's teardown, and of which a failed bind removes
those made before it, with an `Error::Rollback` that names any that stays; and
the `Identity`: user and group IDs, supplementary groups, and `Capabilities`
from an allowlist, which only user ID 0 may hold, without `CAP_NET_BIND_SERVICE`
and `CAP_NET_RAW` while the execution's listeners are `Publication::Published`,
taken with `no_new_privs`.

Its program starts with every signal's default disposition and none blocked. For
user ID 0, the risky procfs and sysfs entries are read-only or masked.

`Entry::prepare` checks it, reads the existing `Account` of the IDs from the
root's account databases unless `AccountMode::Numeric` takes them as they are,
and `spawn::Command::enter` runs it, with each step a `LaunchStage`. It reads no
kernel command line or boot token and creates no account.

### `nvx::guest::process`

`Child`: the status of one child that the caller names, by its pidfd or its
process ID. It waits for that child alone, so another child's status stays for
the caller; a caller that collects every status itself, such as a PID 1 that
adopts orphans, hands each one to its `Child` with `observe`. Statuses are
`std::process::ExitStatus`, real-time signals included.

### `nvx::guest::cgroup`

`Layout`: the agent's cgroup and the workload cgroup, which the caller names,
and which may not hold the agent's.

`ExecutionCgroup`: the cgroup of one execution, the workload cgroup itself or a
child of it, which it prepares with the caller's resource values, places
processes in, checks for occupancy through `pids.current`, kills with
`cgroup.kill`, and settles, over the execution's cgroup or the whole workload
subtree, as the caller's `Scope` chooses. It holds its cgroups' directories
open, and the process's removals and preparations of cgroups take turns, so that
it never reaches another cgroup that takes one of their paths. Settling calls
the caller's status collector between its checks, and collects no status itself.

### `nvx::guest::exec`

`Execution`: drives one program that the caller spawned with the program's ends
of its `Pipes`, or that the caller started itself and adopts with its own ends
of the program's pipes and a `Child` that names it, without blocking, from the
caller's own loop or reactor, and reports each step as an `Event`: the launch
report, which an adopted program has at once, the output of each stream, kill
failures, and the `Outcome`.

It reads a stream only as far as the caller's `grant` allows, so a stalled sink
stalls its own execution alone, writes the program's standard input without
`SIGPIPE`, and ends the program on a cancellation, a timeout, or a
disconnection, with `SIGKILL` through the program's pidfd to the program and, if
the caller asks, its process group, while no collector has taken its status,
and, through the opened directory of a cgroup that the caller names, to that
cgroup.

`signal` sends the program a signal that the caller chooses, such as `SIGTERM`
for a graceful stop, as container runtimes stop their workloads, to the same
processes but the cgroup's, each of them once unless the program leaves its
group meanwhile, while no collector has taken its status. It ends neither the
program nor the execution: it records no cause, so a program that exits after it
ends with its own status, and the caller ends one that outlives the caller's own
deadline with `terminate` or `cancel`. It refuses `SIGKILL`, which only the
execution's causes send.

It closes the program's input once the program has ended. It waits for its
program alone, by a pidfd, or takes the status from the caller's collector with
`observe`; with `Options::caller_reaps`, that collector is the program's only
one.

NVX's managed agent is one caller.

### `nvx::guest::exec::binding`

`Binding`: an `nvx::exec::Runner` bound to an `Execution`, the glue that a guest
adapter would otherwise write.

The runner's read allowances become grants, its discards discards, the input
that it accepted writes and then a close, its terminations a cancellation, a
disconnection, or a termination, and its settlement a kill of the execution's
cgroup and checks until no process occupies it; without a cgroup, nothing proves
that the program's descendants are gone, so the execution is fenced.

It feeds the execution's events back to the runner, statuses that a caller who
reaps hands over with `observe` included, and never blocks: the caller polls its
descriptors and its input writer until its deadline, and each call takes a
bounded number of steps, so that no execution holds the caller's loop.

The caller's own `signal` passes through to the execution, which it does not
end: the runner sees only the program's end, once the program exits.

A launch that failed reaches the runner with the number of the `LaunchStage`
that failed, `LaunchStage::code`, or with 0, which numbers no stage, if the
binding could not read the launch report.

### `nvx::guest::time`

x86_64: `TimeAbi`, the steps of NVX time ABI v1 as typed calls in the caller's
process, for any guest init or agent: the cold-boot time step and, separately,
the start of the time daemon in a child process that the caller supervises, the
VM generation ID, a `CaptureRequest` that repairs a `Restore`, and a
`RestoreFinish`.

The request's mode selects how a restore hands over its fresh entropy: the file
mode, which `nvx-time` keeps, writes it to a file, and the in-memory mode
returns it as the `Restore`'s owned `Seed`, and writes no file.

A violation of the ABI stops the VM in enforcing mode, with the time ABI's
status, as `nvx-time` does; in strict mode, `TimeAbi::strict`, which a caller
that owns the VM's exit status and shutdown selects, the call fails with the
`Violation`, whose `status` is the one with which enforcing mode stops the VM,
and the time daemon and a capture's preparation enforce the ABI all the same.

A call takes the permission for OpenVMM's ports on the calling thread for each
port transaction alone, and fails with `TimeError::PortRelease` if the thread
may still hold some of it, which a process that it started would inherit, and
from which `nvx::guest::spawn` starts none. No call waits for a child that it
did not start.

`nvx-time` runs its steps through them.

### `nvx::guest::clone`

x86_64: the preparation of a template's guest for a capture before its root's
assembly, and the repair of each clone that a restore of its snapshot starts,
for a guest init or agent, whether or not it is PID 1.

`CapturePreparation`, of the caller's `AdmissionFence` and the `Devices` that
the template's clones replace, the `distro` and scratch block devices, which
their MMIO windows name, and the host filesystem device's virtio-fs tag:
`prepare` closes the fence, so that no execution starts, refuses with a
`Mounted` the first mount from those devices, a distro partition included, or
from an overlay, bind mounts included, in a bounded read of
`/proc/self/mountinfo` of at most `MOUNT_TABLE_LIMIT` bytes, then waits for the
time ABI's boot checks, runs its capture checks, and suppresses the stall
detectors, as `nvx-time pre-capture` does, and returns an owned
`CaptureBarrier`.

`CaptureBarrier::capture` requests the capture with the platform tier's request,
in the in-memory mode, through `TimeAbi::capture`, and returns
`Captured::Continued` once it opened the fence and restored the detectors'
settings, or, in a clone, `Captured::Restored`, with the fence still closed,
since nothing else may run before repair acknowledges the restore, and the
detectors suppressed until the time daemon restores their settings:
`Restored::into_parts` hands the `Restore` that repair takes next, which holds
the restore's `Seed`, and the closed fence to the caller, which keeps it closed
through the repair and opens it once the clone may admit executions, such as
once it is activated.

`CaptureBarrier::cancel`, a dropped barrier, and a failure to read the VM
generation ID open the fence and restore the detectors' settings; a capture
request that fails restores the settings but leaves the fence closed, since it
may fail in a clone whose restore it could not repair. Each failure is a
`CaptureError`, with its `CaptureStage` and `CaptureCause`.

`repair`, or a `Repair` that the caller may retry, repairs the clone of a
`Restore` next: it checks that the online CPUs are a prefix that the restore's
processor target does not undercut, brings the target's other CPUs online
through sysfs, and verifies them; with a memory target, it checks, before it
adds any memory block, that the kernel's block size is 128 MiB and that the
restore's ranges continue the captured RAM in logical order, with no gap,
overlap, or range in the MMIO aperture, while the host bounds the target by the
snapshot's capacity, then adds and onlines the blocks through sysfs, as
`nvx-snapshot` does, and verifies them; it reseeds the kernel's random number
generator with the `Seed`, having drawn no randomness before, and drops it; and
it finishes the restore through `TimeAbi::restore_finish`, with the CPUs that
the restore brought online and the acknowledgement when the host gates its input
until then, which hands the deferred time work to the time daemon that the
caller supervises.

It returns the `Repaired` clone, with its generation ID, its online CPUs and
memory, and when it acknowledged the restore, and never touches the fence. It
adds no memory block twice, and reseeds and finishes at most once, so a failure
at or after the reseed is final.

Each failure is a `RepairError`, with its `RepairStage`, its `RepairCause`, and
the repair's `RepairProgress`, and `RepairError::status` selects the status, of
`nvx::protocol::status`, with which the caller ends the VM.

The capture and the repair take the time ABI in strict mode, so that a violation
of it fails them, with its status, rather than stopping the VM, and only the
capture's preparation and the time daemon enforce it.

An `Activation` of the `Repaired` clone, or, with `Activation::with_resources`,
of the resources and the instant of the acknowledgement that a repair reports,
as in an adapter's tests, and of the caller's closed `AdmissionFence` then takes
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
with `apply`, as at a cold boot: it assembles the root as `root::assemble` does,
with the record's image, an EROFS image whose UUID it checks if the record
expects one, or a GPT disk whose GUID it checks if the record expects one, its
upper, a tmpfs or the scratch with the record's journal policy, and its overlay
mode, and with the `RootTemplate` that the guest's own configuration holds,
which its template captured: the runtime directory, the block devices, the
timeout, the host filesystem device's virtio-fs tag, and the gateway of the
template's NIC, if it has one, but waits for no device past the activation's
deadline, which the adapter passes, so that the application ends by then.

For a GPT disk, it first asks the kernel to reread the disk's partition table
with `BLKRRPART`, since the kernel created the block devices of partitions for
the template's placeholder. Whatever the image, it requires a fresh upper, whose
`upper` and `work` directories are missing or empty, in a bounded check that
reads no file, while a cold sandbox's scratch keeps its upper across boots. With
the record's workload identity, it checks its account in the assembled root
before Ready, as NVX's init checks it at boot: `entry::Identity::new(uid, gid)`,
without supplementary groups or capabilities, in existing-account mode, as which
every execution runs, while without one each execution names its own identity,
which `Entry::check` checks. With the record's generated machine ID, it then
generates one, once, with `MachineId::generate`, after the repair's reseed, so
that no two clones share it, to `MACHINE_ID_FILE` in the template's runtime
directory. With the record's export, it mounts the host export through that
filesystem device with `Root::mount_export`, read-write, at `hostfs/root` in the
runtime directory, outside the root, since the record names no mode and OpenVMM
limits each child to its own access; with its network's DNS server, it writes a
resolver file that names it, mode 0444, to `RESOLVER_FILE` in the runtime
directory; and with a network that publishes the workload's listeners, it starts
their announcer, `/sbin/nvx-ingress inherit GATEWAY_IPV4`, which announces from
its standard input, a UDP socket that the application bound, as root, to a
privileged source port. It then binds the runtime's files for every execution
with one `Entry::bind_runtime` in the caller's mount namespace: the machine ID's
file read-only over the root's `/etc/machine-id` and the resolver file over its
`/etc/resolv.conf`, or, with neither, nothing; the entry checks the record's
hostname, which every execution's entry takes with `Entry::hostname`. It returns
the `Applied` root and workload, from which `take_export` hands the adapter the
export's `SourceRoot`, to map the host's mapping table from once the clone is
ready, `proxy` the network's proxy endpoint, which the adapter tells its
workloads as at a cold boot, and `announcer` the announcer, which the adapter
watches through its pidfd, and must end the clone once it ended, since the
gateway then keeps publishing the listeners that it last announced until the VM
ends, and whose `teardown` stops the announcer, closes the export's root, and
removes the binds before it tears the root down, as a drop does, or an
`ApplyError`, `Export` or `Network` among them, after which nothing that it
mounted or bound stays mounted unless the error reports the unmounts that
failed, and no announcer that it started runs.

Once the adapter passed the activation what the session received while the
application ran, which came before Ready, `Activation::complete` opens the fence
and returns the `ReadyActivation` that Ready reports beside the adapter's own
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

### `nvx::guest::root`

The operations of a layered root, for a guest init or agent, whether or not it
is PID 1.

`bootstrap`: the agent's and the workload's sibling cgroups of a `Layout`, with
every process of their parent moved into the agent's, the agent's `memory.low`,
and the workload cgroup prepared through `ExecutionCgroup::prepare`, with its
`memory.max` and `pids.max`, and user namespaces kept unavailable.

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
the caller's `Journal` policy forbids, or a tmpfs mounted `nosuid,nodev` at the
kernel's default size. It refuses overlay options longer than
`MOUNT_OPTIONS_LIMIT`, the most that a mount passes, which, below `/run/nvx`,
fit up to 195 layers of a GPT disk, and an overlay that the kernel's mount table
shows read-only or without the mode's options in effect, since the kernel mounts
one so, rather than fail, when its upper can't hold its work directory or its
extended attributes.

The `Root` reports the first partition's bytes, its `Lower` layers, its
`UpperKind`, and its `OverlayMode`.

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
the caller's mappings.

`Root::map`: a caller's host mappings, through the same mapper. A share, a
child, the export, or a mapping that fails latches the root.

`Root::teardown`: the shares and mappings in reverse order, then the mounts of
an aggregate and of the export, the later first, the overlay, the layers from
the top down, and the upper, with every failed sync or unmount reported. An
ext4 scratch's file system is synced before its unmount, since a mount namespace
that copied the root's mounts keeps them, unsynced, after this one unmounts
them, and a scratch whose sync fails stays mounted.

Each failure is a typed `Error`. The operations collect no child's status,
change neither the caller's root nor its credentials, start no async runtime,
and never power the VM off.

### `nvx::guest::gpt`

`read_disk`: the partition table of a GPT disk, read and checked with
`nvx::gpt`'s `Disk::read` through the disk's block device, which the caller
opened, at the logical block size and size that `BLKSSZGET` and `BLKGETSIZE64`
report, with the caller's `Expected` values, such as the disk GUID that a boot
parameter names. It only reads the device, at explicit offsets, so it never
moves the file's position. It neither compares the partitions that the kernel
created with the table nor asks the kernel to reread the table. Each failure is
a typed `DeviceError`.

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
verification reads it:

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
verification, or from `DistroBinding::import`, which a staging caller that
verified the layer itself calls to bind it without hashing it again. `check`
fails for a binding of another version, layer, format, or policy revision, a verdict
that rejects the layer, a layer whose identity, kind, length, or times changed,
and a manifest that appeared, disappeared, or changed. A binding of a layer
without a manifest holds only while nothing is at the manifest's path: unlike
verification, which finds a manifest as `Path.exists` does, `check` and
`import` refuse a path whose lookup fails for another reason than a missing
file. A missing or stale binding fails: no runtime operation registers,
verifies, or hashes a layer on its own. Hashing belongs where layers are built,
installed, or staged, and to diagnostics.

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
that the check accepts there. `start` checks the binding before it launches
OpenVMM, and hands OpenVMM the checked layer; see
[Checked handoff](#checked-handoff). `run` and `start`
plan the launch with the image format and the identity that the binding
records, which name an EROFS layer, and launch it over a scratch disk, the one
layered root that the guest's shell init assembles. A configuration
without a binding, as `nvx.py` wrote, fails with `sandbox configuration has no
distro binding`, and a stale binding with what changed, such as `LAYER changed
after its distro binding was made: its length differs`; both add `deprovision
the sandbox and provision it again`, since a new provision verifies the layer
again.

## Checked handoff

A launch hands the VMM exactly the objects that its checks found, never a path
that something could replace between the check and the VMM's open of it.

`SandboxPlan::validated` rejects disk image paths containing commas, semicolons,
or improperly nested square brackets with `ErrorKind::Unsupported`, including
the resolved paths that `provision` records. Balanced brackets, as in
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
handle. The VMM inherits the claim's handle and the guard: so it holds the
claim, and the guard the image's path, from its start, even if the launcher ends
before the VMM has opened the image. Claims exclude the launches of NVX, not a
process that writes the image on its own.

`PreparedLaunch::new` takes a `SandboxPlan`, the `CheckedDistro` that
`DistroBinding::check` returned for its image, and, for an upper on scratch,
the `CheckedScratch` that `ScratchBinding::check` returned, and refuses files
that are not those of the plan. It renders the plan's OpenVMM arguments and
kernel command line, holds the checked files, and lists the descriptors that
the VMM inherits:

- On Linux, the VMM inherits the checked descriptors through
  `VmmCommand::inherit`, `PreparedLaunch::inherited`, and the arguments name
  each `/proc/self/fd/N`, `distro:file:/proc/self/fd/N,ro` and
  `scratch:file:/proc/self/fd/M`, which OpenVMM opens: the link reaches the
  checked file whatever later takes its path. Every other descriptor stays
  close-on-exec, as the supervisor guarantees. A checked descriptor that took a
  standard stream's number, as when the caller had closed that stream, first
  moves above the standard streams, with its open file description and its
  claim.
- On Windows, the arguments name each checked file by its final path, the
  absolute `\\?\` path that its handle gives, which resolves every link that
  the plan's path went through, and does not depend on the VMM's directory.
  The checked files hold that path in place: the distro layer's file denies
  writing and deleting the layer, the scratch image's guard denies deleting and
  renaming it, and NTFS renames no directory while a file below it is open;
  OpenVMM's opens share them. The VMM also inherits the handles of the checked
  files and the scratch image's claim, though no argument names a handle,
  through `VmmCommand::inherit` and `PreparedLaunch::inherited`, so that its
  copies hold the paths and the claim from its start: a launcher that ends
  before OpenVMM has opened the images, as when it crashes, closes only its own
  checked files, and a replacement at their paths still fails until OpenVMM
  exits; `start` never commits such a launch, and the sandbox's next transition
  ends its VMM. At authenticated Ready, `PreparedLaunch::release` closes
  OpenVMM's copies of the checked files with `DuplicateHandle` and
  `DUPLICATE_CLOSE_SOURCE`, through the VMM's process handle that its launch
  holds, once a copy of each is found to hold the checked file's object, and
  then the checked files themselves, so that the guards are released while
  OpenVMM keeps the claim. A final path that OpenVMM could not take as the
  path of a raw image, with a comma, semicolon, or unbalanced brackets, or
  ending in `.vhd`, `.vhdx`, `.vmgs`, or `.iso`, which a link, the directory of
  a relative path, or the case of a name on disk can bring, fails the preparation
  with `ErrorKind::Unsupported`.

Images are raw: OpenVMM attaches a file's bytes as they are, and chooses a
container format by the file's extension, which `/proc/self/fd/N` lacks, so a
caller whose images use a container format, such as a fixed VHD, stages a raw
file. The paths in the arguments hold for the launch alone: the plan, which
callers persist, keeps the real paths. The caller adds its own arguments,
starts the VMM, and releases the prepared launch with `PreparedLaunch::release`
once the guest reports authenticated Ready, or drops it once it has torn down a
launch that failed; the VMM's own opens then keep the images, and what it
inherited keeps the scratch image's claim. Staging, the checks, and the
preparation of a launch read no content of the images.

The clone of a template takes its block devices by path instead, as the
template's capture does, since OpenVMM opens the files of a restore itself:
`ClonePlan::new` checks the clone's image and scratch image through their
bindings, then checks each at its path, and requires the geometry that the
template recorded for its own, which `CheckedDistro::geometry` and
`CheckedScratch::geometry` read through the checked files.

`nvx-rs sandbox provision` records the scratch image's binding under
`scratch_binding` in `config.json`, at the image's resolved path, and `start`
checks it after the distro binding, and launches OpenVMM through a prepared
launch, which it releases once the guest is ready, or drops once OpenVMM has
exited. A scratch image that changed fails as a distro layer does, such as with
`IMAGE changed after its scratch binding was made: its length differs;
deprovision the sandbox and provision it again`, and one that another sandbox
claims with `another sandbox claims the ext4 scratch image IMAGE`. `run` binds
and claims the scratch image itself, and runs OpenVMM through a prepared
launch until it exits; its command, which it prints, names the checked files,
except in a dry run, which launches nothing. A sandbox whose upper is in RAM,
`--upper ram`, has no scratch image: `provision` records `null` for the image
and its binding, and neither `start` nor `run` binds or claims one.

## Sandbox input contract

The sandbox launch profile specifies its own inputs rather than emulating Python.
UID/GID and workload resource limits contain only ASCII decimal digits, without
signs, whitespace, underscores, or Unicode digits. Each UID/GID is in
`1..4294967294`, memory in `1..18446744073709551615` bytes, and the process
limit in `1..4194303`. A layer UUID is canonical lowercase hyphenated text;
braces, a prefix, uppercase, and unhyphenated forms are refused.

API change: `nvx::host::sandbox::{Int, IntError}` are no longer exported.
`SandboxSettings::memory_max` is `Option<u64>` and `pids_max` is `Option<u32>`;
`None` leaves the respective limit unset, and zero is refused. Persisted plans
still encode these limits as canonical positive decimal strings, and CLI sandbox
configuration records use JSON integers, so valid existing records keep their
wire format. See [Specification tests](#specification-tests).

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
UTF-8 byte that Linux treats as whitespace (including `0xa0`). Caller-supplied
sandbox command lines use Linux's parameter splitting, including quoted values
and hyphen/underscore name equivalence. The exact trimmed text is validated
before rendering, and the guest bootstrap's whitespace-only token view is
checked too. Reserved `nvx_*` and `virtfs_*` names cannot be hidden inside
quoted values or behind leading Unicode whitespace; a `--` or unclosed quote
that hides the appended root tokens is refused. Adapter plans reserve only
their root and OpenVMM share parameters, as before.

Host paths are the host's own, which `std::path` holds: spelled with their
components alone, as `Path::components` reads them, with backslashes on
Windows, and resolved by `std::fs::canonicalize`, with the components that do
not exist yet appended. Paths that are not valid Unicode are refused. The
filesystem queries are those of `std::fs`, and the identities of files those
of `nvx::host::platform`: the device and inode on Unix, and the volume serial
number and the 64-bit file index on Windows.

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

It runs host tools, as the [bundled artifacts](#bundled-artifacts) build script
does: `git` reads the history, from the root alone; `curl` queries the GitHub
REST API and downloads the package, speaking HTTPS alone and following no
redirect itself, so that the token of `GH_TOKEN` or `GITHUB_TOKEN`, which it
reads on its standard input, reaches only the API; and `tar` lists the
package's members, whose names must be relative and canonical below one root,
and extracts it into a private directory below `build/`. Every extracted file
must be a regular file that `SHA256SUMS` lists with its digest. On Windows,
`curl` and `tar` are those of `%SystemRoot%\System32`, whose `tar` reads the
ZIP package of `windows-whp`. `download` reads the status and the redirect of
each response from the headers that `curl` dumps, rather than from its
`--write-out`, with which the `curl` 8.8.0 of some Windows releases fails every
HTTPS request.

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
and `sandbox start` until its VM is ready. It thus never sees part of an
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
[Other differences from nvx.py](#other-differences-from-nvxpy) lists. It
prints `>> COMMAND`, OpenVMM's command line, quoted as `sandbox run` quotes
it, and unless `--dry-run` is given, it runs OpenVMM with the standard
streams, so that the guest's console and the terminal settings that OpenVMM
restores behave as they do for OpenVMM itself, and exits with OpenVMM's
status.

- A cold boot runs the kernel and the initramfs of `--guest`, `alpine` by
  default, with `--memory-mib` or else the guest's default, 128 MiB for Alpine
  and 512 MiB for Ubuntu and Azure Linux, which `Guest::default_memory_mib`
  gives. `--restore-snapshot` restores a snapshot instead, without them, and
  with the `--restore-*` options, which need it.
- A cold boot takes `--net` and `--network-profile` together. A restore takes
  the addressing of its NIC from the snapshot, as OpenVMM does, so it refuses
  `--net`, and passes `--network-profile` alone, which OpenVMM requires, with
  the network policy that the snapshot was captured with, to restore a
  snapshot that has a NIC.
- One directory `--mount` is OpenVMM's `--mount`. Several, or a shared file,
  are the children of an aggregate at `/run/nvx/shares`, whose guest targets
  must be absolute, must not overlap, and must survive the kernel command line,
  and on a cold boot, their `nvx_share=` tokens join `--cmdline` within the
  1024 bytes that OpenVMM leaves to them; a restore adds none, since its guest
  keeps the binds that its boot made. `--mount-deny`, `--mount-allow`, and
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

- `run` prints `>> COMMAND`, OpenVMM's command line, quoted as the Microsoft C
  runtime splits a command line on Windows and as a POSIX shell splits one
  elsewhere, as `nvx.py` quoted it with `subprocess.list2cmdline` and
  `shlex.join`. Unless `--dry-run` is given, it then runs OpenVMM with the
  standard streams and exits with its status; the command then names the
  checked distro layer, and the scratch image of an upper on scratch, as
  `/proc/self/fd/N` on Linux, which OpenVMM inherits, and by their final paths
  on Windows, as [Checked handoff](#checked-handoff) describes.
  `--outcome-report` asks OpenVMM for its outcome report.
- `provision` records the sandbox in its state directory. `start` launches
  OpenVMM detached with the managed lifecycle, passes it a fresh capability
  through its standard input, and waits until the guest answers. `exec` runs a
  workload, prints its standard output and then its standard error, and exits
  with its status; `--outcome-report` writes
  `{"operation_id": HEX, "outcome": {"category": CATEGORY, "operation":
  "exec", "status_code": STATUS}, "schema_version": 1}`. `stop` shuts the VM
  down and prints nothing, but for a warning when it terminates a VM of another
  [runtime identity](#runtime-identity). `deprovision` removes the state
  directory.

The state directory is the directory of the sandbox's
[store](src/host/store/mod.rs), the runtime records that every host of a
sandbox shares, and `config.json` is the CLI's own record beside them. It also
records the [distro binding](#distro-bindings) and the
[scratch binding](#checked-handoff) of the sandbox, and its network as the
typed `SandboxNetwork`, in the serde form of the plan's network, in place of
the network options. `nvx-rs` refuses to start a sandbox that `nvx.py` or an
earlier `nvx-rs` provisioned, which has no bindings or no typed network:

| File | Contents |
| --- | --- |
| `config.json` | The provisioned sandbox, in format 8, with the export plan of its shares under `export`, `null` for none, its upper under `upper`, `scratch` or `ram`, the binding of its distro layer under `distro_binding`, its scratch image under `scratch` and that image's binding under `scratch_binding`, both `null` for an upper in RAM, and its network under `network`, `null` for none, written by `serde_json`'s pretty printer with the members of every object sorted by name, with mode 0600, through a temporary file. `start` reads it as strict JSON, as egress policy files are read, into the typed fields of format 8, and also reads the format 7 that earlier builds wrote, which records no `upper` and keeps the upper on scratch, and the format 1 that they wrote for a sandbox without shares, with `mount` `null` and no `export` |
| `sandbox.lock` | The store's lock, which serializes the lifecycle transitions |
| `launch.json` | The store's journal of a start in progress, or of one that was interrupted |
| `runtime.json` | The store's runtime record of the running VM, in its format 4: the OpenVMM process, by its ID, its start time, and the host's boot, its control endpoint, `STATE/control.sock` or `//./pipe/openvmm-microvm-HEX` on Windows, and the [runtime identity](#runtime-identity) of its start |
| `control.capability` | The 32-byte capability of the running OpenVMM, with mode 0600 |
| `control.sock` | The control endpoint on POSIX hosts |
| `openvmm.log` | OpenVMM's output, which each start replaces |
| `outcome.json` | OpenVMM's outcome report of the last start |

`provision`, `start`, `stop`, and `deprovision` hold the store's lock for their
whole transition, and `exec` only while it finds the running VM, so that a long
workload cannot delay a `stop`. The CLI takes the store's
[lifecycle locks](src/host/store/mod.rs) with `config.json` as its
configuration record: a transition that leaves the state directory without it,
such as one of a sandbox that was never provisioned, removes the lock file as
it releases the lock, and `deprovision` removes the sandbox through the store,
which removes the store's files and `config.json`, refuses files that are not
NVX's, and removes the lock file, while it still holds the lock, before the
directory. Every transition but `provision` finds the
sandbox's VM with `nvx::host::supervisor::reattach`, as an orchestrator does
after it restarts within the same boot of the host: it ends a start that was
interrupted, holds an OpenVMM that still runs through its verified process
identity, and clears the runtime record of one that exited without a `stop`.
The first `exec` or `stop` to find such a record fails because the runtime
state is stale, while `start` launches OpenVMM again and `deprovision` removes
the sandbox. `start` launches OpenVMM with `nvx::host::supervisor::launch`, and
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

#### Guest features

After the guest answers, `start` asks it for its control features and
requires `GuestFeatures::LAYERED_SANDBOX`, the features that its launches use:
per-execution environments (bit 4), since `exec` sends a working directory and
an environment, and host path mapping tables (bit 6), since `start` then sends
the mapping table of the sandbox's shares, which the guest agent mounts before
it runs any workload; a mapping that it refuses fails the start, after the
guest shuts down as `stop` has it do. It needs neither cancellation (bit 0),
since the CLI never cancels a workload, and Ctrl-C ends the session, nor
workload account creation (bit 2), since a layered sandbox runs its workloads
as an account of the image. A guest image that lacks a required feature, or
that predates the request, is stopped as a failed start is, and the start
fails with
`the guest image lacks control features that layered sandboxes need (...)`,
which names the missing features. `nvx.py` does not ask; this, the checks of
the [distro binding](#distro-bindings) and of the
[scratch binding](#checked-handoff), and the claim of the scratch image are the
intended changes of behavior. The
guest agents of NVX advertise bits 0, 4, and 6 in sandbox mode.

#### Other differences from nvx.py

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
  of `--memory-mib`, `--memory-capacity-mib`, and `--restore-memory-mib` from 1
  to 17592186044415, the most whose bytes OpenVMM counts in 64 bits, and
  `--exec-timeout-ms` from 0 to 4294967295. `--processors` and
  `--restore-processors` take one of their choices as written, and `--timeout`
  a number as Rust reads an `f64`. `nvx.py` read them with Python's `int` and
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
  without a console window, as `nvx::host::supervisor` starts it.
- `start` stops waiting for the control endpoint once the OpenVMM that it
  launched exits, as `nvx::host::session::connect` does, and fails at once
  with `exited`, where `nvx.py` waited until `--timeout` passed before it
  reported the exit.
- `exec` refuses more than 1 MiB of workload output, which the guest agent
  never forwards.
- The state directory holds the store's files: `sandbox.lock` replaces the
  `lifecycle.lock` of `nvx.py`, `runtime.json` has the store's format, and each
  start replaces `openvmm.log`, which `nvx.py` appended to. `exec` holds the
  lock while it finds the running VM, which `nvx.py` did not. A runtime record
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
- `run` and `start` hand OpenVMM the files that their checks opened: its
  command line names them `/proc/self/fd/N` on Linux, and by their final
  paths, `\\?\` paths, on Windows, and on every host, a sandbox's scratch image
  is claimed while it runs, so a second sandbox, or a `run`, on the same image
  fails; see [Checked handoff](#checked-handoff). OpenVMM inherits no other
  descriptor or handle of `run` beyond its standard streams, as `subprocess.run`
  passes none with `close_fds`: on Windows, OpenVMM's handle list holds copies
  of `run`'s standard handles, its checked files, and the scratch image's claim
  alone.
- `start` and `exec` refuse, and `stop` terminates, a VM that another runtime
  identity started; see [Runtime identity](#runtime-identity).
- `run --restore-snapshot` refuses `--net`, whose addressing a restored NIC
  takes from the snapshot, and passes `--network-profile` without it, as
  OpenVMM requires to restore a snapshot that has a NIC. `nvx.py run` took the
  two only together, so it could not restore such a snapshot: OpenVMM refused
  its `--net`, or required the `--network-profile` that it omitted.

### Test hooks

`--root` and `NVX_OPENVMM` let tests run the CLI without a VM. `nvx-fake-openvmm`
serves the sandbox profile: its managed lifecycle, with the capability on its
standard input, the control console, and an outcome report at stop, and its
one-shot runs, which run `/bin/echo`, `/bin/sleep`, `/bin/true`, or
`/bin/false`, write an outcome report if asked, and exit with the workload's
status. It also runs the microVMs of `nvx-rs run`, without a guest: a cold
boot, or the restore of a snapshot directory, prints that it booted or restored
the microVM, writes an outcome report if asked, and exits with status 0, and a
restore whose snapshot directory is missing fails with status 1. Like OpenVMM,
it refuses with status 2 a restore with `--net`, whose NIC keeps the addressing
that its snapshot saved, and a cold boot with `--network-profile` alone. Kernel
command-line tokens make it fail: `fake_exit_on_start=STATUS`,
`fake_exit_status=STATUS`, `fake_guest_features=MASK`, and
`fake_legacy_guest=1`, among others; see `nvx::testing::fake_openvmm`. Like
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
checks that refuse a command line before anything changes, dry runs, one-shot
runs, the lifecycles of sandboxes of every configuration, their executions, the
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
| `bundled` | Stages a release at build time; see [Bundled artifacts](#bundled-artifacts) |
| `testing` | The fake OpenVMM and the `nvx-fake-openvmm` binary |
| `cli` | The `nvx-rs` CLI of `host::cli` and its `nvx-rs` binary; it enables `host`. See [The nvx-rs CLI](#the-nvx-rs-cli) |
| `serde` | The serde form of `Hypervisor` |
| `guest` | The `guest` module and the guest binaries; see [Guest binaries](#guest-binaries) |

Without default features, the crate has only `protocol`, `activation`, `exec`,
`contract`, `net`, `gpt`, `uuid`, `Hypervisor`, and `Error`, and depends on
nothing but `serde` when `serde` is enabled and `nix` and `getrandom` when
`guest` is; the protocol-only configuration of the
[runtime adapter](../tests/runtime-adapter/README.md) uses it that way.

## Example

```rust,no_run
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use nvx::host::instance::{Found, Instance};
use nvx::host::launch::LaunchSpec;
use nvx::host::store::CallerFiles;
use nvx::protocol::{Workload, WorkloadEnvironment};
use nvx::{Artifacts, Hypervisor};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifacts = Artifacts::discover()?;
    let runtime = artifacts.runtime_identity()?;
    let spec = LaunchSpec::new(artifacts, Hypervisor::from_env_or_default()?);
    // The example keeps no files of its own beside the instance's store.
    let directory = std::env::temp_dir().join("nvx-example");
    let instance = Instance::create(directory, "example", CallerFiles::none())?;
    // Lifecycle transitions hold the instance's lock; executions need none.
    let mut locked = instance.lock(None)?;
    let Found::Stopped = locked.find(|| Ok(runtime.clone()))? else {
        return Err("the example's VM runs already".into());
    };
    locked.start(&spec, &runtime, Duration::from_secs(60))?;
    let Found::Running(running) = locked.find(|| Ok(runtime.clone()))? else {
        return Err("the example's VM stopped".into());
    };

    let workload = Workload {
        argv: vec!["/bin/echo".to_owned(), "hello".to_owned()],
        timeout_ms: 0,
        cwd: None,
        environment: WorkloadEnvironment::Default,
    };
    let timeout = Duration::from_secs(60);
    let execution = instance.exec(&running, &workload, timeout, timeout)?;
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
    locked.stop(running, Duration::from_secs(30))?;
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
| `Unsupported` | A well-formed request that NVX cannot represent or enforce, such as a policy beyond a limit of OpenVMM or the guest agent |
| `Exited` | The VM's OpenVMM process exited |
| `Io` | A host I/O failure, or state that NVX recorded on the host and cannot read |
| `NotFound` | State that an operation names and that does not exist on the host, such as the store of a sandbox that was never created or that a removal deleted |
| `Protocol` | A control session that failed, or a request that the guest agent refused |
| `TimedOut` | A deadline that passed |
| `Other` | Any other failure |

Messages call an instance `sandbox NAME`, after the name that `Instance::create`
or `Instance::open` gives it.

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
of `nvx::contract`. The build script extracts and verifies only the files that
the bundle holds, so a release that ships `bin/nvx-rs[.exe]` stages as the
older release that `artifacts.json` pins, which doesn't. The build scripts of
dependents receive the staged
directory as `DEP_NVX_ARTIFACTS_DIR` and can ship it next to their executables
as `nvx/`.

## Guest binaries

The `guest` feature adds the programs that run in NVX's Linux guests: the
`nvx::guest` module and one binary per program. The Alpine, Ubuntu, and Azure
Linux images ship all nine at their established `/sbin/nvx-*` paths. The
module's libraries are not programs: `terminal` opens the terminal on which an
agent serves its session, `spawn` starts a program in a new process, `entry`
has the program enter an execution's namespaces, view, root, and identity,
`process` collects the status of a child that the caller names, `cgroup`
places, kills, and settles the processes of an execution, `exec` drives one
program as an execution, `mappings` mounts host mappings beneath the
descriptors of the roots that its caller names, and `root` bootstraps the
guest for a layered root through `cgroup`, assembles the root, mounts live
shares and mappings in it through `mappings`, and tears it down.
`nvx-managed-agent` opens its control terminal, and launches, drives, collects,
and settles its workloads, through them, has each sandbox workload enter its
execution through `entry`, and applies its host mapping table through
`mappings`, from its mount of the host's export, beneath `/` in direct mode and
beneath the container's assembled root in sandbox mode, where it unmounts the
table before it returns to the init agent, and any other guest init or agent
can use them too. Any guest init or agent can also run the steps of `nvx-time`
and `nvx-reseed` in its own process, through the
`nvx::guest::time` and `nvx::guest::reseed` calls, on which both programs run,
and a template's guest prepares for a capture before its root's assembly through
`nvx::guest::clone`, which runs the steps of `nvx-time pre-capture` behind the
caller's fence of workload admission, and through which each clone repairs its
restore: it activates the restore's memory target, reseeds the kernel's random
number generator, and finishes the restore, as `nvx-snapshot` does.
Nor is the `gpt` library a program: it reads the partition table of a GPT
disk's block device.

| Binary | Program |
| --- | --- |
| `nvx-console-pending DEVICE` | Prints the number of bytes waiting to be read from a device, such as a console |
| `nvx-mmio-write ADDRESS VALUE` | Stores a 32-bit value at a physical address through `/dev/mem` (x86_64) |
| `nvx-reseed [--generation-only] SEED_FILE [PREVIOUS]`, `nvx-reseed --sample` | Reseeds the kernel's random number generator from a seed file and checks its VM generation ID, or prints a random sample |
| `nvx-exit [CODE]` | Stops the VM with an 8-bit exit code through OpenVMM's control port 0x604 (x86_64) |
| `nvx-device-io file ...`, `nvx-device-io network ...`, `nvx-device-io map PATH TEXT` | The benchmark helper that measures file and UDP round-trip I/O and prints one JSON line, and reports a file benchmark's progress on stderr; `map` writes TEXT at the start of a file through a shared mapping and holds the dirty mapping until its standard input ends, for the microVM scenarios (x86_64) |
| `nvx-managed-agent CONTROL_TTY ROOTFS HOSTNAME UID GID USER HOME` | Serves the managed control session and supervises sequential workloads; `--exec-config-fd FD -- WORKLOAD ...` is the direct agent's internal launch helper |
| `nvx-ingress [inherit] GATEWAY_IPV4` | Announces reachable TCP listeners and settled UDP sockets to the portable-network gateway, from a privileged source port that it binds, or, with `inherit`, from its standard input, a socket that a clone's application bound to one; `test DIRECTORY` runs synthetic procfs snapshots |
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

Of this crate, the guest images depend only on the files that this build reads:
`Cargo.toml`, `Cargo.lock`, `build.rs`, any Cargo configuration in `.cargo/`,
and every file under `src/` but the host side and the test modules. The host
side is `src/host/`, `src/testing/`, and the `nvx-rs` and `nvx-fake-openvmm`
binaries; the test modules are the `tests.rs` files, the `tests` directories,
and the managed agent's `loopback` harness. CI keys the guest images on those
files and runs the OpenVMM VMM tests, which boot the Alpine image, only when
one of them changes, while every change to the crate runs the NVX microVM
tests; see [Continuous integration](../doc/ci.md#change-classification). Keep
host-only code under `src/host/` and test-only modules in `tests.rs` files or
`tests` directories. The build fails when Cargo's dep-info lists another file,
which `GuestRustBuildConstants` in
[`build_constants.py`](../scripts/nvx_tools/build_constants.py) then has to
select.

## Testing

```bash
cd nvx-rs
NVX_BUNDLE=skip cargo test --all-features   # unit, conformance-vector, property, and fake-OpenVMM tests
```

The tests need no hypervisor. The fake-OpenVMM test drives `Instance` end to end
against `nvx-fake-openvmm`, and the template-capture test runs templates'
captures through the supervisor against it, up to a crash of the adapter whose
VMM a recovery ends. The CLI test runs the
[scenario tests](#scenario-tests) of `nvx-rs`, and checks the guest-feature
requirement and, on Linux, Ctrl-C. The
[conformance vectors](tests/vectors/README.md) of the wire formats cover both
sides of every record, frame, and payload: the codec decodes each vector of
`tests/vectors/nvxc-v1.txt` to its values and encodes them to its bytes, it
accepts or refuses OpenVMM's vectors and cases of the `NVXS` session as they
say, and, on Linux, the guest's listener announcements match
`tests/vectors/nvxl-v1.txt`. The activation codecs decode each record and
Ready activation of `tests/vectors/activation-v1.txt` to its values, encode
them to its bytes, and refuse its invalid bytes. Property tests check that
records and activation records round-trip, that the activation record and
Ready activation decoders accept only canonical encodings, and that the
`NVXS` record parser, those decoders, and the host's and the guest's `EXEC`
and `MAPS` decoders return, never panic, on arbitrary bytes; those of the
egress policy compiler compare it with a reference model of the format
([Egress policy files](#egress-policy-files)). The NVXS test drives the
host's and the guest's roles of the session and their connections through a
broker that relays records as OpenVMM's does, with messages of an application
framing of its own that span records. The store test crashes orchestrator
processes at each step of a launch, recovers the VMM stand-ins that they leave
behind, and loads records that another build wrote, in `tests/fixtures/store/`.
The host-module test reads the crate's sources and fails if the direct children
of `nvx::host` depend on one another in a cycle through the paths of their
production code, outside comments and test-only code. It lists the shortest
cycle through each dependency on one, with a reference for each of its
dependencies: break a cycle by moving the item that a lower child takes from a
higher one down, and re-exporting it at its old path. So the children stack in
layers, each above those that it depends on, which the test prints with
`--nocapture`.
The process-supervision test crashes orchestrator processes at each step of a
launch through the supervisor, reattaches to the VMM stand-ins that they leave
behind and stops them, and checks that a VMM inherits only its standard streams
and the descriptors or handles that it is allowed while another thread spawns
processes. The tests of `nvx::exec` drive a runner and a peer through a
simulated workload, transport, and sink on a clock of their own, and, on Linux,
the tests of `nvx::guest::exec::binding` drive real programs through them, two
at once in one loop, one of them with a stalled sink; run as root, one of them
also settles a real cgroup.
On Linux, a loopback test drives
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
rustfmt, Clippy with warnings denied for all features and for the library
with and without default features, the tests, a build and the tests of the library
without default features, a synthetic-release `bundled` test, rustdoc with
warnings denied, the minimum supported Rust version (1.89), and a macOS check.
On Linux, it also lints, builds, and tests the guest binaries through `nvx.py`.
Another workflow runs cargo-mutants on the files and with the tests that a
dispatch names; see
[nvx-rs mutation testing](../doc/ci.md#nvx-rs-mutation-testing).

The guest's mount-ID tests also run from a thread with its own descriptor
table. An ignored test of `nvx::guest::root` checks mount points and mount
options from a thread with both its own descriptor table and a private mount
namespace. It mounts a temporary tmpfs and needs root, but no hypervisor;
run it as root with:

```bash
NVX_BUNDLE=skip cargo test --locked --all-features --lib -- \
  --ignored --exact guest::root::tests::a_thread_with_private_tables_finds_its_mount_points_and_options
```

`tests/openvmm_vectors.rs` is an ignored test that compares the copies of
OpenVMM's vectors in `tests/vectors/openvmm/` with the files of the OpenVMM
submodule, which the `nvx-rs` job cannot check out, and the versions of
OpenVMM's snapshot format, saved-state schema, time ABI, and restore packet in
`nvx::contract` with the submodule's. The NVX CLI jobs run it once they check
the submodule out, and so does an OpenVMM pin promotion; see
[OpenVMM's vectors](tests/vectors/README.md#openvmms-vectors).
`tests/openvmm_cpu_profiles.rs` is an ignored test that compares
`src/host/cli/cpu_generations.json`, the identities of the built-in CPU
profiles from which [`run`](#run) guides a failed cold boot, with the profiles
that the submodule pins, and runs where that one does.

`tests/managed_lifecycle.rs` is an ignored test of the managed lifecycle of a
guest whose init hands the boot to the managed agent, on a real hypervisor:
authenticated sessions and reconnects, request-scoped working directories and
environments, the workload identity, timeouts, refused working directories,
and stop, which `nvx::host::session` drives. `nvx.py test-microvm --scenario
managed-lifecycle` builds OpenVMM's command line for its guest and backend,
runs the test with Cargo, and checks the outcome report and the guest console
that it leaves:

```bash
python3 ../scripts/nvx.py test-microvm --backend kvm --scenario managed-lifecycle
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
documented option grammar and the contracts with OpenVMM and the guest.
Properties cover the restore-stable aggregate child names, reserved targets
and tokens, every generated launch token and its exact 1024-byte budget,
policy option sequences, and paths on the host's own filesystem. They check
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
properties check that the kernel reads no reserved parameter in the extra
parameters that a `LaunchSpec` accepts, whatever separates it, and reads the
`hostname=` parameter after them; that the time ABI's check of the guest's
command line fails exactly when the kernel applies a clock parameter that the
ABI forbids; and that the kernel reads every token of the command lines of a
sandbox whose entrypoint, arguments, and share targets a launch accepts as one
parameter, whole.
