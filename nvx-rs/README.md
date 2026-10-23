# nvx

`nvx` is the Rust crate that holds the NVX mechanics: the OpenVMM
control protocol, and the launch, policy lowering, and supervision of NVX
microVMs. Its package and library are both called `nvx`; the package was
called `nvx-rs`, like the directory that holds it. The
[`aci_edge_sandboxes` crate](../aci_edge_sandboxes/README.md) builds on it and
re-exports it as `aci_edge_sandboxes::nvx`. The crate also holds the programs
that run in NVX's Linux guests, and the `nvx-rs` command-line interface, the
backend to which `nvx.py` delegates its `run` and `sandbox` commands,
egress-policy compilation, and artifact lookup, as the phases of
[microsoft/nvx#356](https://github.com/microsoft/nvx/issues/356) moved them to
Rust.

An NVX sandbox is a microVM that OpenVMM runs with the managed lifecycle: the
guest's init starts a managed agent, which runs the workloads that the host
sends it over an authenticated control console. The crate is published on
crates.io as a preview, whose API can change in any release. It has a
`Cargo.lock` of its own; a workspace at the repository root would interact with
the nested `openvmm/` workspace.

## Layout

```text
nvx-rs/
├── src/
│   ├── protocol/    NVXS session, NVXC frames, EXEC and MAPS payloads, guest features, kernel
│   │                command-line tokens, and the guest targets that live shares may not take;
│   │                no dependencies, shared by the host and the guest
│   ├── contract.rs  control-contract versions and release-manifest compatibility
│   ├── net/         CIDR parsing and subtraction, and hostname and guest-network forms
│   ├── gpt/         strict reading of GPT disks whose first partition configures an adapter and
│   │                whose other partitions hold a root's read-only layers; no dependencies,
│   │                shared by the host and the guest
│   ├── uuid.rs      UUIDs in their canonical text and in GPT's layout; no dependencies, shared
│   │                by the host and the guest
│   ├── hypervisor.rs
│   ├── error.rs     nvx::Error and its ErrorKind
│   ├── exec/        the execution engine: the streams, credit, and outcome of an execution's two
│   │                sides, without I/O, threads, or features, for any framing and host
│   ├── host/        feature `host` (default): artifacts, launch, network and egress policy files,
│   │                filesystem, NVXS connection, session, process, platform, VM instances,
│   │                sandbox stores, outcome reports, file seals, and the sandbox launch profile
│   │                of `nvx.py`, its distro and scratch bindings, and its prepared launches,
│   │                with the Python semantics they keep; with feature `cli`, the `nvx-rs` CLI
│   ├── testing/     feature `testing`: the fake OpenVMM
│   ├── guest/       feature `guest`: the programs that run in Linux guests, the guest's NVXS
│   │                connection, the libraries through which a guest init or agent opens the
│   │                terminal on which it serves its session, starts a program, drives its
│   │                execution, alone or through the execution engine, collects its children's
│   │                statuses, settles its executions' cgroups, mounts host mappings beneath the
│   │                descriptors of the roots that it names, assembles and tears down a layered
│   │                root, and reads a GPT disk's partition table through its block device, and
│   │                the typed calls through which it runs the time ABI's steps and the reseed
│   └── bin/         nvx-rs, nvx-fake-openvmm, and the guest binaries
├── tests/           fake-OpenVMM, CLI, NVXS session, process-supervision, checked-handoff, store,
│                    and guest-binary tests, the store's record fixtures, the golden fixtures that
│                    Python recorded for the ports of its code, in fixtures/, and the conformance
│                    vectors of the wire formats, in vectors/
├── build.rs         bundled-release staging
└── artifacts.json   the NVX release that the `bundled` feature stages
```

| Module | Contents |
| --- | --- |
| `nvx::protocol` | The codec of the control console: `NVXS` outer records, `NVXC` application frames, the `EXEC` payload that the host encodes and the guest agent decodes, the `MAPS` payloads of the host mapping table, `GuestFeatures`, and the `nvx_maps=` and `nvx_workload_account=create` tokens. The [conformance vectors](tests/vectors/README.md) of version 1, `tests/vectors/nvxc-v1.txt`, and OpenVMM's vectors and cases of the `NVXS` session pin every record, frame, and payload independently of the codec, which the host, the guest agent, and the fake OpenVMM share |
| `nvx::protocol::cmdline` | The kernel command line as Linux 6.18 reads it, for the readers that need the kernel's view of it: `parameters` splits it into `Parameter`s as `parse_args` and `next_arg` do, at the bytes that `is_space` accepts, with double quotes, up to a NUL or `--`, and `Parameter::is_named` compares names as `parameq` does, with dashes and underscores alike. `VirtioMmioDevice::parse` reads a `virtio_mmio.device` value, `SIZE@BASE:IRQ[:ID]`, as the virtio-mmio driver reads it, for a device of any type, and `virtio_mmio_devices` reads every such parameter of a command line |
| `nvx::protocol::nvxs` | The NVXS session, independent of `NVXC` and of any other application protocol: `RecordType`, `Record`, `encode_record`, and `RecordParser`, which parses records incrementally across partial reads with buffers bounded by one record; `HostSession` and `GuestSession`, the state machines of the host's and the guest's roles, which validate every record's type, identity, and sequence number, and the guest's receive window and credits, without I/O. An application message may span DATA records, and the application's own framing reassembles it |
| `nvx::protocol::root` | The boot tokens of a layered root, which the host renders and guests parse by the same rules: a `RootLayout` is a `RootImage` on the `distro` block device, an EROFS filesystem or a GPT disk with the `nvx::uuid::Uuid` that the guest expects of it, if any, over a `RootUpper`, the ext4 scratch disk or RAM. `RootLayout::tokens` renders `nvx_layer=distro,0xd0003000,FORMAT[:IDENTITY]` and `nvx_scratch=0xd0006000,ext4` or `nvx_overlay_upper=ramfs`, and `RootLayout::parse` reads them from a kernel command line, and refuses malformed, repeated, and conflicting tokens and the earlier form without a format, with no fallback that probes the devices. It reads the virtio-mmio devices of the command line with `nvx::protocol::cmdline`, to refuse a RAM upper beside one at the scratch base. `tests/fixtures/root-tokens.json` holds its cases, which the tests of the guest's shell init replay too |
| `nvx::protocol::share` | The guest targets that a sandbox's live shares may not take, which the host's `sandbox::mount` and the guest's `root` both refuse: `RESERVED_MOUNT_TARGETS`, the trees where the container runtime mounts or binds, `RESERVED_EXACT_MOUNT_TARGETS`, the directories that it creates, `RESERVED_TARGET_CHARACTERS`, and `reservation`, which tells why a target is reserved |
| `nvx::contract` | The OpenVMM microVM ABI, boot contract, broker protocol, and control-contract revision that the crate speaks, and when each changes, and `manifest_is_compatible` for a release's `SOURCE-MANIFEST.json` |
| `nvx::net` | `Cidr`: IPv4 and IPv6 networks and their subtraction, which `Cidr::parse` reads without host bits and `Cidr::parse_masked` as OpenVMM reads the destination of an egress rule; `is_valid_hostname` and `is_valid_guest_network`, the forms of a VM's hostname and guest address, and `guest_gateway`, the gateway of a guest address |
| `nvx::gpt` | `Disk::read`: the protective MBR, primary header, and primary partition entry array of a GPT disk, read through a `ReadAt` source that the caller gives and checked strictly, as UEFI specifies them and as Linux requires them, without ever reading the backup: 128-byte entries, at most 1 MiB of them, used entries first and without holes, a first partition and 1 to 254 layers, extents in the usable blocks without overlap, and unique GUIDs, with checked arithmetic in the disk's logical blocks. `Expected` adds the disk GUID and the partitions' unique GUIDs and sizes that a caller knows, and `Disk::read_first_partition` returns the first partition's opaque bytes, up to a bound and without trailing NUL bytes and whitespace. GUIDs are `nvx::uuid::Uuid`s. It has no dependencies, so the guest and the host's checks of the images that it stages share it |
| `nvx::uuid` | `Uuid`: a UUID, such as the GUID of a GPT disk or partition or the UUID of a filesystem, in the order of its canonical text, the one text that it displays and parses: 32 lowercase hexadecimal digits in groups of 8, 4, 4, 4, and 12, joined by hyphens. `from_bytes_le` and `to_bytes_le` convert the layout in which GPT and other EFI GUIDs store their first three groups little-endian. It has no dependencies, so the host and the guest share it |
| `nvx::Hypervisor` | KVM, MSHV, or WHP, with its serde form behind the `serde` feature |
| `nvx::exec` | The execution engine: the stream state, credit, ordering, and outcome of one execution, for any framing that carries its messages, without I/O, threads, global state, or features, so that it builds with `--no-default-features`. A `Runner` is the side that runs the workload: it reads the workload's output under the peer's credit, holds the input that the peer sent within its window until the workload takes it, requests the terminations, discards, input close, and settlement that its adapter applies, and returns one terminal `Outcome`, which tells whether the execution is reusable or fenced. A `Peer` is the other side: it takes output into windows that its sink empties, returns credit as the sink takes bytes, sends input within the runner's window, and ends the execution on a cancellation, a timeout, a drop, a reset, or a disconnection. Both take the current `Instant` from the caller, report their next deadline, and never wait, so a blocked stream, sink, window, or transport stops no timer, cancellation, or other execution, and both bound their memory. The adapter chooses one byte window per stream, or frames charged against one pool that stdout and stderr share, and the maximum chunk, in `Limits`; a `Key`'s generation tells stale messages from foreign ones |
| `nvx::host::artifacts` | `Artifacts` (also `nvx::Artifacts`): the OpenVMM executable, guest kernel, and Alpine initramfs, from a release, a repository checkout, the environment, a directory next to the executable, or the bundle. Artifacts located in a release or checkout keep its `ArtifactDir`, and `Artifacts::runtime_identity` reads their `RuntimeIdentity` from its install metadata; artifacts named by path have none. `ArtifactDir` and `Guest`: every artifact of the guests that `nvx.py` runs, the OpenVMM executable, the kernel, the Alpine, Ubuntu, and Azure Linux initramfs images and their package manifests, and the Ubuntu distro layer and its manifest, in a checkout's `build/` and `openvmm/target/release/` or a release's `guest/` and `bin/`, checked in the order and with the messages of `nvx.py`. `ArtifactPaths` lists them all and serializes as JSON. `ArtifactDir::runtime_identity` reads the `RuntimeIdentity` of the OpenVMM executable, kernel, and Alpine initramfs from the install metadata, a release's `SHA256SUMS` or a checkout's build provenance, without hashing an artifact |
| `nvx::host::launch` | `LaunchSpec` and OpenVMM's command line: the managed lifecycle, the `WorkloadIdentity`, the 1024-byte kernel command line, its reserved parameters, the 32,767-character limit of a Windows command line, and `LaunchSpec::check`, which every command line applies to the memory size, the non-root identity, the hostname, the guest network, and the kernel parameters |
| `nvx::host::network` | `NetworkPolicy` and `openvmm_arguments`, the one lowering of a network posture into OpenVMM options, for the CLI and external orchestrators: the egress default and rules, ingress, `HostLoopback` access, denied, `Outbound` from the guest alone, or both ways, and its forwards. A proxy-only policy, `NetworkPolicy::proxy_only`, has one way out, a `NetworkProxy` that listens on a port of host loopback, which the guest reaches at the same port of its gateway, and whose URL as the guest sees it `NetworkProxy::url` gives; the lowering refuses a proxy beside any other way out. Egress rules are lowered exactly into OpenVMM's IPv4 and IPv6 rules by the lowering of egress policy files, with at most 256 rules for each action |
| `nvx::host::network::policy` | Egress policy files: `compile_policy_file`, `compile_policy_bytes`, and `compile_policy` compile one to a `CompiledEgressPolicy` of native rules; see [Egress policy files](#egress-policy-files) |
| `nvx::host::filesystem` | The planner of every NVX caller: `plan` turns an `ExportRequest`, the mapped host paths at explicit guest targets or at those of the host-path convention, their denied, allowed, and write paths, and the owner, into the `ExportPlan` of host paths through OpenVMM's aggregate virtio-fs device, for a root's `ReservedTargets`, the blockless root's or the container root's: its children, its ordered binds, which give the host-mapping table, and the identities that each start checks again; and `guest_path` and `resolve_guest_path` (also at `nvx::`) |
| `nvx::host::store` | `SandboxStore`: the runtime records of one sandbox in a private directory with one lock, which every host that supervises the sandbox shares. The `RuntimeRecord` names the running VM: the `ProcessRef` of its VMM, its control endpoint, and the `RuntimeIdentity` that it was launched with, the identities of its OpenVMM, kernel, and initramfs from install or staging metadata and its boot and control contract versions, which decide whether another build of the host may reattach to it. The `LaunchJournal` of a launch holds a marker written before the spawn, the VMM's `ProcessRef` right after it, and an inherited claim on the VMM's log, which proves that a launch that never published its endpoint has exited, so that recovery ends an interrupted launch and signals only a VMM that it holds through a verified `Process`. Records are JSON in one versioned format, and records in any other format fail rather than being migrated. The caller declares the files that it keeps in the directory, its configuration record, whose presence means that the sandbox is provisioned, and any others, as `CallerFiles`, and takes the lifecycle locks with them: `SandboxStore::lock_for_provision` creates the directory, again if a removal deletes it meanwhile, `SandboxStore::lock_existing` fails with `ErrorKind::NotFound` once it is gone, and a holder that leaves the directory without the configuration record removes its lock file as it releases the lock. `LockedStore::remove` removes a sandbox that no VMM can run in, for a caller that declares its configuration record: the store's files, the caller's, and the directory, unless it holds entries that are neither, which it refuses and keeps, and `Lingering` waits for open files, such as the lock files of other transitions, that keep the emptied directory; both remove the directory only while the path still names it. The store also owns the file in which the VMM stages its outcome report, which a VMM that is killed while it writes the report leaves, and removes it once no process of its launch runs |
| `nvx::host::instance` | `Instance`: a VM in a sandbox store of its own, which `nvx::host::store`, `nvx::host::supervisor`, and `nvx::host::session` drive, and in which the caller keeps the files that it declares as `CallerFiles`. Under the store's lifecycle lock of an existing sandbox, `LockedInstance::find` reattaches to the VM, ends a start that was interrupted, and reports a VM of another runtime identity as `Mismatched` rather than driving it; `LockedInstance::start` launches OpenVMM through the supervisor with a fresh capability, under the caller's `RuntimeIdentity`, waits until the guest is ready and has mounted the mapped paths, and commits the launch; `LockedInstance::stop` stops the VM with a forced fallback; `LockedInstance::terminate` stops a `Mismatched` VM by identity-verified forced termination, without a control session or its outcome report; and `LockedInstance::remove` removes a stopped instance, with the caller's files and its directory, through the store. `Instance::exec` runs workloads without the lock and pumps their output. `write_json` and `read_json` keep the configuration records that callers store beside an instance |
| `nvx::host::nvxs` | `HostConnection`: the host's role of the NVXS session over a platform transport, on which `nvx::host::session` and other host applications run their protocols. It attaches with the launch capability, waiting while the broker asks it to, sends application bytes in as many DATA records as they take, and returns each DATA payload from the guest, with deadlines; a receive that times out keeps a partial record |
| `nvx::host::process` | Verified process references. A `ProcessRef` names one process by its ID, its start time, and the `BootId` of the host boot during which it started. `Process::open` checks a reference and holds its process through a pidfd on Linux or a process handle on Windows, whether or not the caller started it, so that waits sleep until the process exits and termination never reaches a process that reuses its ID. A reference from an earlier boot names an exited process, and a process whose identity this user cannot check, such as another user's, makes `Process::open` fail rather than report an exit. Linux hosts without pidfds poll the reference instead, and cannot terminate the process |
| `nvx::host::supervisor` | The supervisor of a sandbox's VMM, beneath the CLI, `Instance`, and external orchestrators, which keep the configuration, the authorization, and the application protocol. `launch` runs the store's launch steps under the sandbox's lock: it writes the journal's marker, spawns OpenVMM with the claimed log, records its `ProcessRef` right after, and returns a `Starting` launch that the caller commits once the VMM serves its control endpoint, or abandons. A launch that fails never leaves an unrecorded VMM running. `VmmCommand::inherit` lists the descriptors (Linux) or handles (Windows) that the VMM inherits under their numbers, besides its standard streams, and it inherits nothing else of the caller's: on Linux, they must be close-on-exec, a flag that only the VMM's own process clears; on Windows, they join the VMM's explicit handle list. `Vmm::stop` runs one graceful step of the caller's and waits for the VMM's exit event until a deadline, and takes a bounded forced step through the VMM's verified identity only on request. `Vmm::terminate` takes that forced step alone, within the caller's bound, for a VM whose guest the caller must not ask anything, such as one of another runtime identity. `reattach` recovers an interrupted launch, and holds the recorded VMM again after the orchestrator restarts within the same host boot: `Live`, `Mismatched` with another runtime identity, `Exited`, or `Indeterminate` |
| `nvx::host::seal` | `FileSeal`: what the host reports about an open file from its metadata alone, without reading it: the platform scheme, the device or volume serial number, the file ID, the kind, the length, the modification time, and on Linux the change time, with a versioned serde form. A seal detects accidental change, not tampering; see [Distro bindings](#distro-bindings) |
| `nvx::host::session`, `platform`, `report` | The authenticated control session, which carries NVX's control protocol, `NVXC`, one frame per DATA record of a `HostConnection`; Linux and Windows primitives; and OpenVMM's outcome report |
| `nvx::host::sandbox` | The launch profile of `nvx.py sandbox`, ported from the one that `scripts/nvx_tools/sandbox.py` held: `SandboxLaunch`, with the distro `SandboxLayer`, an EROFS layer or a GPT disk, the one layer that a sandbox takes, the ext4 scratch disk, the entrypoint, arguments, hostname, workload identity, and resource limits, and the `SandboxPlan` that it makes of them. `plan`: `SandboxPlan`, the typed plan of a cold layered launch, which is persistable in a versioned serde form: one read-only `distro` block device, an EROFS filesystem or a GPT disk with the identity that the guest expects of it, if any, an upper, the ext4 scratch disk or RAM, the launch settings, the `ExportPlan` of its live shares, and optionally a `SandboxNetwork`, the guest network and its typed `NetworkPolicy`, whose proxy's URL as the guest sees it the plan reports. The plan of an adapter that boots its own guest, `SandboxPlan::adapter`, holds no launch settings and no export plan. A plan knows only the `distro` and `scratch` `BlockRole`s, and gives, for every combination, its `--microvm-sandbox-block` arguments, with `--microvm-workload-identity` when it has launch settings, with no scratch block device for a RAM upper, and those of its network, which `nvx::host::network` lowers, and its kernel command line, with the boot tokens of `nvx::protocol::root`, within the 1024-byte budget that OpenVMM leaves it, refusing one whose root tokens the guest's parser refuses. Both deliver the live shares as the managed lifecycle does: the arguments export them at `nvx::host::filesystem::GUEST_EXPORT`, the command line announces the number of their mappings with `nvx_maps=`, and `mapping_table` gives the ordered table that `start` sends once the guest agent is ready; the CLI's one-shot `run` still renders them as boot tokens. An adapter's plan renders the caller's tokens before those of its root, and refuses tokens that its root or OpenVMM's shares own. `mount`: `SandboxMount` requests of live shares of host directories and files, their guest targets and the reserved ones, their access policies and owner, and the checks of their host paths; `plan_mounts`, which plans them through `nvx::host::filesystem` for the container root, and `boot_shares`, which renders a plan as the OpenVMM arguments and boot tokens of one share or of the aggregate of several, as the one-shot lifecycle delivers them. `options`: the checks that `nvx.py sandbox` and `nvx.py run` apply to their options: the attribution of `--mount-deny`, `--mount-allow`, and `--mount-write` to shares, the systemd entrypoint check and the explicit verification of the distro layer, the targets of `nvx.py run` shares, and the managed exec environment; see [Python parity](#python-parity). `distro`: the `DistroBinding` of a distro layer, an EROFS layer or a GPT disk, which explicit verification makes or a staging caller imports, and its check from metadata alone, which returns the layer open; see [Distro bindings](#distro-bindings). `scratch`: the `ScratchBinding` of a scratch image, its identity, kind, and length, and its check from metadata alone, which claims the image for one sandbox at a time and returns it open. `prepared`: `PreparedLaunch`, the launch of a plan with the checked files of its block devices, which renders the plan's OpenVMM arguments and kernel command line, naming the checked files as `/proc/self/fd/N` on Linux and by their final paths on Windows, and lists the descriptors that the VMM inherits through `VmmCommand::inherit`; see [Checked handoff](#checked-handoff) |
| `nvx::testing::fake_openvmm` | A fake OpenVMM that serves the control console and runs scripted workloads without a VM, for direct launches and for the managed and one-shot sandboxes of `nvx-rs sandbox` and `nvx.py sandbox` |
| `nvx::host::cli` | Feature `cli`: the `nvx-rs` executable; see [The nvx-rs CLI](#the-nvx-rs-cli) |
| `nvx::guest` | The programs that run in Linux guests; see [Guest binaries](#guest-binaries) |
| `nvx::guest::mappings` | `Mapper`: applies host mappings for any guest init or agent beneath the descriptor of a `TargetRoot`, the root of a mount of a whole file system, which `statmount` tells from a bind of a directory below it, such as a layered root's overlay or a guest's `/`, from a `SourceRoot`, such as the guest's mount of the host's export. A `Mapping` names a relative source, an absolute target, its `Access`, and optionally the `Kind` that its source must have. The mapper opens each component with `openat2`, through no symbolic link or magic link, creates what a target lacks only on the root's own mounts, which `statx` tells apart by their unique mount IDs, never inside another mapping, and checks kinds, overlaps, and the targets that the root profile's `Reserved` set holds, with the `/run/nvx/time` tree and its ancestors always. It attaches each mapping as a detached copy of its source's mount, `open_tree`, restricted `nosuid`, `nodev`, and read-only when asked, `mount_setattr`, before `move_mount` puts it at its target, then checks that it is there. Zero mappings do no work; the first that fails is reported with its index as an `ApplyError`, and latches the mapper. `Mapper::teardown` detaches a mapping that a rename moved and that the attach failed to detach, then unmounts the mappings in reverse order, and reports each failure. `bind` applies one runtime bind through the same walk and attach. Linux 6.8 or later. NVX's managed agent applies its host mapping table through it, beneath `/` in direct mode and beneath the container's assembled root in sandbox mode, and a layered `Root` its live shares and mappings |
| `nvx::guest::nvxs` | `GuestConnection`: the guest's role of the NVXS session over a byte stream, such as the control console, on which NVX's managed agent and other guest agents run their sessions. It never reads past the record in progress, so a caller that polls the stream between records sees every record that it has not returned |
| `nvx::guest::terminal` | `Terminal`: the terminal on which a guest agent serves its session, such as the VM's control console, as a byte stream. It opens the terminal, waiting while the kernel sets it up during boot, puts it in raw mode, so that it passes the session's bytes as they are, and drains it, so that it sends what the agent wrote before the agent ends the VM. NVX's managed agent and other guest agents serve their sessions on it |
| `nvx::guest::spawn` | `Command`: starts a program in a new process for any guest init or agent, with its arguments, environment, root and working directories, standard streams, inherited descriptors, cgroup, process group or new session, a `Barrier` at which the program waits until the caller releases it, and an `entry::Prepared` execution to enter, without NVX's control protocol, managed agent, or workload identity. Its `LaunchReport` tells whether the program started, or which `LaunchStage` failed and why. NVX's managed agent is one caller |
| `nvx::guest::entry` | `Entry`: the execution that a program enters, for any guest init or agent, from typed inputs: a root, which is a mount, moved over `/` in new mount, PID, and UTS namespaces, where the program is PID 1 and leads a new session; a hostname; a fresh procfs, a read-only sysfs, and a `/dev` of its own with only the allowlisted `DeviceNode`s, a devpts, and a `/dev/shm`, every other mount `nodev`; the `MachineId`, the image's own or one generated by `MachineId::generate`, and the time status, which `Entry::bind_runtime` binds read-only into the root once, through `mappings::bind`, for every later execution; and the `Identity`: user and group IDs, supplementary groups, and `Capabilities` from an allowlist, which only user ID 0 may hold, without `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` while the execution's listeners are `Publication::Published`, taken with `no_new_privs`. For user ID 0, the risky procfs and sysfs entries are read-only or masked. `Entry::prepare` checks it, reads the existing `Account` of the IDs from the root's account databases unless `AccountMode::Numeric` takes them as they are, and `spawn::Command::enter` runs it, with each step a `LaunchStage`. It reads no kernel command line or boot token and creates no account |
| `nvx::guest::process` | `Child`: the status of one child that the caller names, by its pidfd or its process ID. It waits for that child alone, so another child's status stays for the caller; a caller that collects every status itself, such as a PID 1 that adopts orphans, hands each one to its `Child` with `observe`. Statuses are `std::process::ExitStatus`, real-time signals included |
| `nvx::guest::cgroup` | `Layout`: the agent's cgroup and the workload cgroup, which the caller names, and which may not hold the agent's. `ExecutionCgroup`: the cgroup of one execution, the workload cgroup itself or a child of it, which it prepares with the caller's resource values, places processes in, checks for occupancy through `pids.current`, kills with `cgroup.kill`, and settles, over the execution's cgroup or the whole workload subtree, as the caller's `Scope` chooses. It holds its cgroups' directories open, and the process's removals and preparations of cgroups take turns, so that it never reaches another cgroup that takes one of their paths. Settling calls the caller's status collector between its checks, and collects no status itself |
| `nvx::guest::exec` | `Execution`: drives one program that the caller spawned with the program's ends of its `Pipes`, without blocking, from the caller's own loop or reactor, and reports each step as an `Event`: the launch report, the output of each stream, kill failures, and the `Outcome`. It reads a stream only as far as the caller's `grant` allows, so a stalled sink stalls its own execution alone, writes the program's standard input without `SIGPIPE`, and ends the program on a cancellation, a timeout, or a disconnection, with `SIGKILL` through the program's pidfd to the program and, if the caller asks, its process group, while no collector has taken its status, and, through the opened directory of a cgroup that the caller names, to that cgroup. It closes the program's input once the program has ended. It waits for its program alone, by a pidfd, or takes the status from the caller's collector with `observe`; with `Options::caller_reaps`, that collector is the program's only one. NVX's managed agent is one caller |
| `nvx::guest::exec::binding` | `Binding`: an `nvx::exec::Runner` bound to an `Execution`, the glue that a guest adapter would otherwise write. The runner's read allowances become grants, its discards discards, the input that it accepted writes and then a close, its terminations a cancellation, a disconnection, or a termination, and its settlement a kill of the execution's cgroup and checks until no process occupies it; without a cgroup, nothing proves that the program's descendants are gone, so the execution is fenced. It feeds the execution's events back to the runner, statuses that a caller who reaps hands over with `observe` included, and never blocks: the caller polls its descriptors and its input writer until its deadline, and each call takes a bounded number of steps, so that no execution holds the caller's loop |
| `nvx::guest::time` | x86_64: `TimeAbi`, the steps of NVX time ABI v1 as typed calls in the caller's process, for any guest init or agent: the cold-boot time step and, separately, the start of the time daemon in a child process that the caller supervises, the VM generation ID, a `CaptureRequest` that repairs a `Restore`, and a `RestoreFinish`. A call takes the permission for OpenVMM's ports on the calling thread for each port transaction alone, and fails with `TimeError::PortRelease` if the thread may still hold some of it, which a process that it started would inherit, and from which `nvx::guest::spawn` starts none. No call waits for a child that it did not start. `nvx-time` runs its steps through them |
| `nvx::guest::reseed` | `Seed::read` and `reseed`: a restore's fresh entropy, which starts with its `GenerationId`, and the reseed of the kernel's random number generator with it, in the caller's process. `nvx-reseed` runs them |
| `nvx::guest::root` | The operations of a layered root, for a guest init or agent, whether or not it is PID 1. `bootstrap`: the agent's and the workload's sibling cgroups of a `Layout`, with every process of their parent moved into the agent's, the agent's `memory.low`, and the workload cgroup prepared through `ExecutionCgroup::prepare`, with its `memory.max` and `pids.max`, and user namespaces kept unavailable. `assemble`: the `Assembly`'s block devices, found by their MMIO windows, checked against their roles' access modes and their expected sizes, and flushed, and the `Root` that it mounts, at a runtime directory, a canonical path that no symbolic link leads through, that it holds exclusively until its teardown, where nothing is mounted already, once it has checked everything: the read-only layers of the `Image` that the caller names, which it never probes: an EROFS image, whose expected `nvx::uuid::Uuid` it checks in its superblock, or a GPT disk whose partition table it reads and checks with `nvx::guest::gpt::read_disk`, against the caller's `nvx::gpt::Expected` values, whose first partition it reads, up to the caller's bound, as opaque bytes that it never mounts, and whose later partitions, at least one, hold ext4 layers that it mounts `ro,nosuid,nodev` with `errors=remount-ro`, so that an error that the kernel finds in a layer fails the operation that found it, rather than panic the guest, as the layer's superblock may ask, from the block devices that the kernel created when it found the disk, once their superblocks show that no journal needs a recovery, which their read-only device can't take, partition 2's at the overlay's base and the last partition's at its top, under an overlay mounted `nosuid,nodev` with the one option set of the caller's `OverlayMode`, `metacopy=on,xino=on` or, for lowers that may carry untrusted overlay xattrs, `metacopy=off,redirect_dir=nofollow`, with no fallback, whose upper and work directories are on the `Upper`: an ext4 scratch mounted `nosuid,nodev`, whose superblock it checks for a journal that the caller's `Journal` policy forbids, or a tmpfs mounted `nosuid,nodev` at the kernel's default size. It refuses overlay options longer than `MOUNT_OPTIONS_LIMIT`, the most that a mount passes, which, below `/run/nvx`, fit up to 195 layers of a GPT disk, and an overlay that the kernel's mount table shows read-only or without the mode's options in effect, since the kernel mounts one so, rather than fail, when its upper can't hold its work directory or its extended attributes. The `Root` reports the first partition's bytes, its `Lower` layers, its `UpperKind`, and its `OverlayMode`. `Root::mount_share` and `Root::mount_aggregate`: virtio-fs live shares at targets in the root that it checks, against the targets that `protocol::share` reserves among others, with one aggregate per root, which it attaches through its `mappings::Mapper` beneath the overlay, with NVX's container root's reserved targets: a share's device mounted out of the root, then moved onto its target, and a copy of each child's entry of the aggregate. `Root::map`: a caller's host mappings, through the same mapper. A share, a child, or a mapping that fails latches the root. `Root::teardown`: the shares and mappings in reverse order, then an aggregate's own mount, the overlay, the layers from the top down, and the upper, with every failed unmount reported. Each failure is a typed `Error`. The operations collect no child's status, change neither the caller's root nor its credentials, start no async runtime, and never power the VM off |
| `nvx::guest::gpt` | `read_disk`: the partition table of a GPT disk, read and checked with `nvx::gpt`'s `Disk::read` through the disk's block device, which the caller opened, at the logical block size and size that `BLKSSZGET` and `BLKGETSIZE64` report, with the caller's `Expected` values, such as the disk GUID that a boot parameter names. It only reads the device, at explicit offsets, so it never moves the file's position. It neither compares the partitions that the kernel created with the table nor asks the kernel to reread the table. Each failure is a typed `DeviceError` |

GPT lower layers use `noload,errors=remount-ro`: the kernel neither loads nor
replays their journals, because even a journal whose file system does not
request recovery can cause an unbounded scan. The assembly still refuses any
layer whose superblock requests recovery, before mounting anything. This
policy does not change EROFS layers or writable ext4 scratches.

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
stay in `openvmm_arguments`. `nvx-rs sandbox` reads the rules that a policy
file compiles to, like those of `--network-egress-allow` and
`--network-egress-deny`, back into `EgressRule`s as OpenVMM reads native rules,
and lowers them again with the rest of its policy, which renders compiled rules
unchanged.

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
`CLAIM_RELEASE_WAIT` from now, and then fails with `ErrorKind::TimedOut`. On
Linux, the claim is an exclusive `flock` lock of the checked descriptor's open
file description, which the VMM inherits and holds; a process that another
thread spawns holds a copy of the descriptor from its fork until it executes
its program, which the wait covers. On Windows, the check's handle is a guard
that holds no access to the image's content, shares reading and writing, and
denies deleting and renaming the image, and the check makes sure that no other
process has the image open for writing, as another sandbox's VMM does, then
opens the image for writing, sharing every access, as the claim's handle,
which the VMM inherits: so the VMM holds the claim from its start, even if the
launcher ends before the VMM has opened the image. Claims exclude the launches
of NVX, not a process that writes the image on its own.

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
  OpenVMM's opens share them. A launcher that ends before OpenVMM has opened
  the images, as when it crashes, closes the checked files early, so that a
  replacement at their paths would then reach OpenVMM unchecked, and
  [#780](https://github.com/microsoft/nvx/issues/780) tracks holding the paths
  in that case; `start` never commits such a launch, and the sandbox's next
  transition ends its VMM. The VMM inherits one handle, which no argument
  names: the scratch image's claim, through `VmmCommand::inherit` and
  `PreparedLaunch::inherited`. A final path that OpenVMM could not take as the
  path of a raw image, with a comma or semicolon, or ending in `.vhd`, `.vhdx`,
  `.vmgs`, or `.iso`, which a link, the directory of a relative path, or the
  case of a name on disk can bring, fails the preparation with
  `ErrorKind::Unsupported`.

Images are raw: OpenVMM attaches a file's bytes as they are, and chooses a
container format by the file's extension, which `/proc/self/fd/N` lacks, so a
caller whose images use a container format, such as a fixed VHD, stages a raw
file. The paths in the arguments hold for the launch alone: the plan, which
callers persist, keeps the real paths. The caller adds its own arguments,
starts the VMM, and drops the prepared launch once the guest reports
authenticated Ready, or once it has torn down a launch that failed; the VMM's
own opens then keep the images, and what it inherited keeps the scratch
image's claim. Staging, the checks, and the preparation of a launch read no
content of the images.

`nvx-rs sandbox provision` records the scratch image's binding under
`scratch_binding` in `config.json`, at the image's resolved path, and `start`
checks it after the distro binding, and launches OpenVMM through a prepared
launch, which it drops once the guest is ready or OpenVMM has exited. A
scratch image that changed fails as a distro layer does, such as with
`IMAGE changed after its scratch binding was made: its length differs;
deprovision the sandbox and provision it again`, and one that another sandbox
claims with `another sandbox claims the ext4 scratch image IMAGE`. `run` binds
and claims the scratch image itself, and runs OpenVMM through a prepared
launch until it exits; its command, which it prints, names the checked files,
except in a dry run, which launches nothing.

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
and `cli.json` for `nvx.py sandbox` and `nvx.py run` as commands; see
[Golden scenarios](#golden-scenarios). `scripts/nvx_rs_fixtures.py` recorded
them with CPython 3.14, except the scenarios of `nvx.py run`, and generated
the Unicode tables of `src/host/python/unicode.rs`, until `nvx.py` delegated to
`nvx-rs` and [microsoft/nvx#359](https://github.com/microsoft/nvx/issues/359)
retired the script with the Python code that it recorded;
`scripts/nvx_rs_run_fixtures.py` recorded those of `nvx.py run`, until
[microsoft/nvx#638](https://github.com/microsoft/nvx/issues/638) retired it in
turn. The golden tests compare the ports with these records and need no
Python. The records are now the reference of that behavior: a deliberate change
of behavior updates the records that it changes by hand, in the same change.

## The nvx-rs CLI

The `cli` feature builds the `nvx-rs` executable, a faithful port of the
runtime steps of `nvx.py` that `nvx.py` delegates to it
([microsoft/nvx#359](https://github.com/microsoft/nvx/issues/359) and
[microsoft/nvx#638](https://github.com/microsoft/nvx/issues/638)):

```text
nvx-rs [-h] [--root PATH] [--prog PROG] artifacts
nvx-rs [-h] [--root PATH] [--prog PROG] egress compile PATH
nvx-rs [-h] [--root PATH] [--prog PROG] run [OPTIONS]
nvx-rs [-h] [--root PATH] [--prog PROG] sandbox [run|provision|start|exec|stop|deprovision] [OPTIONS]
```

```bash
cd nvx-rs
cargo build --release --features cli   # target/release/nvx-rs
```

Every NVX release package ships the release build as `bin/nvx-rs`, a static
`x86_64-unknown-linux-musl` executable, in the Linux packages, and as
`bin/nvx-rs.exe`, with a static C runtime, in the Windows package, with its
provenance in `provenance/nvx-rs.provenance.json`
([Package and source delivery](../doc/distribution.md)). The `nvx-rs` of an
extracted release runs as is: without `--root`, it uses the release that holds
it; see [Artifact root](#artifact-root).
[`nvx.py download`](../doc/usage.md#download) installs it in a checkout of the
sources that built it, so that `nvx.py` runs it without Cargo.

`nvx.py` runs the executable that `NVX_RS` names, else
`nvx-rs/target/release/nvx-rs[.exe]` of its checkout, and else builds that one
with Cargo first, with its output on standard error; see
[Usage](../doc/usage.md#nvx-rs). `nvx.py` records the provenance of the builds
that it makes and installs, and `nvx.py record-nvx-rs-provenance` that of a
build made otherwise. It runs `nvx-rs --prog nvx.py --root REPOSITORY run
OPTIONS` and `nvx-rs --prog nvx.py --root REPOSITORY sandbox OPERATION OPTIONS`
with the options that the user gave, in full, and the standard streams and exit
status of the command. For `run`, it adds the `--cold-boot-guidance` of a host
whose CPU no built-in CPU profile serves, which names the OpenVMM that
`nvx-rs --root REPOSITORY artifacts` gives; see [`run`](#run). It also runs
`nvx-rs egress compile PATH` for the egress scenarios of `nvx.py test-microvm`.

Errors print `error: MESSAGE` to standard error and exit with status 1, as
`nvx.py` reports a `ScriptError`. Usage errors print the parser's usage and
`PROG: error: MESSAGE` and exit with status 2, as `argparse` does, and help
exits with status 0. Ctrl-C prints `Interrupted` and exits with status 130.
`run`, `sandbox run`, and `sandbox exec` exit with the status of OpenVMM or of
the workload. Text ends its lines as Python's `print` ends them, with `\r\n` on
Windows and `\n` elsewhere, while workload output passes through unchanged.

`--prog PROG` names the program in usage, help, and error messages, `nvx-rs` by
default. `nvx.py` passes its own name, so that the usage errors of `nvx.py run`
and `nvx.py sandbox` that `nvx-rs` reports, such as an invalid `--processors`
or `--layer`, read `usage: nvx.py run ...` and `nvx.py run: error: ...`, or
those of `nvx.py sandbox`, as they did before `nvx.py` delegated the commands.
The usage wraps for the name as `argparse` wraps it for 80 columns.

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

### `run`

`nvx-rs run` keeps the contract of `nvx.py run`
([Usage](../doc/usage.md#run)): every option of its parser, with the same
names, defaults, and value checks, and the same checks, messages, outputs, and
exit statuses. It prints `>> COMMAND`, OpenVMM's command line, quoted as
`sandbox run` quotes it, and unless `--dry-run` is given, it runs OpenVMM with
the standard streams, so that the guest's console and the terminal settings
that OpenVMM restores behave as they do for `nvx.py run`, and exits with
OpenVMM's status.

- A cold boot runs the kernel and the initramfs of `--guest`, `alpine` by
  default, with `--memory-mib` or else the guest's default, 128 MiB for Alpine
  and 512 MiB for Ubuntu and Azure Linux, which `Guest::default_memory_mib`
  gives. `--restore-snapshot` restores a snapshot instead, without them, and
  with the `--restore-*` options, which need it.
- One directory `--mount` is OpenVMM's `--mount`. Several, or a shared file,
  are the children of an aggregate at `/run/nvx/shares`, whose guest targets
  must be absolute, must not overlap, and must survive the kernel command line,
  and on a cold boot, their `nvx_share=` tokens join `--cmdline` within the
  1024 bytes that OpenVMM leaves to them; a restore adds none, since its guest
  keeps the binds that its boot made. `--mount-deny`, `--mount-allow`, and
  `--mount-write` name host paths, which OpenVMM attributes to the share that
  holds them, and which must be absolute with an aggregate, unlike those of
  `sandbox`, which apply to the `--mount` before them.
- `--cold-boot-guidance=TEXT`, which neither the usage nor the help shows,
  holds what to print to standard error after OpenVMM fails a cold boot: the
  CPU-profile guidance that `nvx.py run` passes for a host whose CPU no
  built-in profile serves, which `nvx.py` derives from the CPU catalog that
  `doctor` and `benchmark` also use. It prints only after OpenVMM exits with a
  status other than 0 from a cold boot, and never after a dry run, a restore,
  an error or usage error of `nvx-rs`, or Ctrl-C, so that it follows only
  OpenVMM's own refusals, whose statuses those of `nvx-rs` share.

### `sandbox`

`nvx-rs sandbox` keeps the contract of `nvx.py sandbox`: every option of its
parser, with the same names, defaults, and value checks, and the same
operations, checks, messages, outputs, exit statuses, and outcome reports,
except as [Other differences from nvx.py](#other-differences-from-nvxpy) lists.
The operation defaults to `run`. Options take their values
as `--option VALUE` or `--option=VALUE`, repeated options accumulate, and
`--mount-deny`, `--mount-allow`, and `--mount-write` apply to the `--mount`
before them.

- `run` prints `>> COMMAND`, OpenVMM's command line, quoted as
  `subprocess.list2cmdline` quotes it on Windows and as `shlex.join` does
  elsewhere. Unless `--dry-run` is given, it then runs OpenVMM with the
  standard streams and exits with its status; the command then names the
  checked distro layer and scratch image as `/proc/self/fd/N` on Linux, which
  OpenVMM inherits, and by their final paths on Windows, as
  [Checked handoff](#checked-handoff) describes.
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
| `config.json` | The provisioned sandbox, in format 1 without shares and in format 7 with the export plan of its shares under `export`, with the binding of its distro layer under `distro_binding`, that of its scratch image under `scratch_binding`, and its network under `network`, `null` for none, written as `json.dumps(value, indent=2, sort_keys=True)` writes it, with mode 0600, through a temporary file |
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
  with each artifact that it builds, and that `nvx.py download` installs with a
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

- Long options cannot be abbreviated; `nvx.py` passes them in full.
- Help and usage are formatted for 80 columns, as `argparse` formats them for a
  pipe, and an invalid choice lists the choices quoted, as Python 3.14.5 and
  later do.
- `nvx.py` checks the choices of its string options, unknown options, missing
  values, and `--help` before `nvx-rs` checks the values, so on a command line
  with two errors, the one that `nvx.py` reports wins, where the Python
  implementation reported the first.
- Where `nvx.py` would end with a Python traceback, as for a configuration
  without a field that `start` reads, `nvx-rs` prints `error: MESSAGE`, with the
  message of the exception, and exits with status 1.
- Text goes to pipes and files in UTF-8, where Python on Windows uses the ANSI
  code page unless `PYTHONIOENCODING` or UTF-8 mode says otherwise.
- `start` refuses a control endpoint that another process serves, which it
  treats as one that has not appeared yet, and, on Windows, starts OpenVMM
  without a console window, as `nvx::host::supervisor` starts it.
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
  of `run`'s standard handles and the scratch image's claim alone.
- `start` and `exec` refuse, and `stop` terminates, a VM that another runtime
  identity started; see [Runtime identity](#runtime-identity).

### Test hooks

`--root` and `NVX_OPENVMM` let tests run the CLI without a VM. `nvx-fake-openvmm`
serves the sandbox profile: its managed lifecycle, with the capability on its
standard input, the control console, and an outcome report at stop, and its
one-shot runs, which run `/bin/echo`, `/bin/sleep`, `/bin/true`, or
`/bin/false`, write an outcome report if asked, and exit with the workload's
status. It also runs the microVMs of `nvx.py run`, without a guest: a cold
boot, or the restore of a snapshot directory, prints that it booted or restored
the microVM, writes an outcome report if asked, and exits with status 0, and a
restore whose snapshot directory is missing fails with status 1. Kernel
command-line tokens make it fail: `fake_exit_on_start=STATUS`,
`fake_exit_status=STATUS`, `fake_guest_features=MASK`, and
`fake_legacy_guest=1`, among others; see `nvx::testing::fake_openvmm`. Like
OpenVMM, it opens the sandbox's block devices at the paths that its options
name and holds them until it exits, after `fake_block_delay_ms=MS` if asked,
and records what each held in `fake-openvmm-TOKEN-blocks.json` next to the
kernel. A managed `start` also reads the root's
[runtime identity](#runtime-identity), so a test root holds build provenance,
as the roots of the golden scenarios do.

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
random identifiers as placeholders, and the devices, file IDs, and times of the
distro and scratch bindings, which `config.json` has recorded since the
recording, and on Linux the descriptor numbers of `/proc/self/fd/N`, with
which launches have named their checked files since then, as well; on
Windows, launches have named them by their final paths, `\\?\` and the path,
since then.
Their kernel command lines name the image format of the distro layer, which
the launch profile has rendered since the recording too, and their network
options render through the typed policy, which `config.json` records under
`network`: the scenarios whose options OpenVMM refused, or that combined a
proxy with other access, now show the planner's refusals or valid policies.
The configurations of sandboxes with shares have since held the shares' export
plan, in format 7, and a policy path outside the host directory of a sandbox's
only share now fails before OpenVMM starts.
Its `posix` and `windows` sections were
recorded on each kind of host, where the script also played every scenario with
`nvx-rs`, and each lifecycle with the Python implementation and `nvx-rs` taking
turns both ways, and found no difference. `tests/cli.rs` replays the section of
its host with `nvx-rs`, without Python; since `nvx.py sandbox` runs `nvx-rs`,
these scenarios are its reference. Their state files have since followed the
sandbox store: its lock file, its runtime record, the log that each start
replaces, and the build provenance in the root from which `start` reads the
runtime identity.

The scenarios whose names start with `run, ` come from the Python
implementation of `nvx.py run`, which `scripts/nvx_rs_run_fixtures.py`
recorded the same way, without the CPU-profile guidance, which depends on the
host's CPU: the usage errors and help of its parser, the refusals of its
checks, dry runs of every guest, of cold boots and restores, of shares, shared
files, and their policy paths and owners, of the network options and egress
policy files, of CPU profiles and outcome reports, and of the 1024-byte budget
of the kernel command line, and runs of the fake OpenVMM, also without the
artifacts that they need. The script recorded the section of each kind of host
there, played its run scenarios with `nvx-rs run`, and found no difference,
before [microsoft/nvx#638](https://github.com/microsoft/nvx/issues/638) retired
it with the Python implementation of `nvx.py run`; since `nvx.py run` runs
`nvx-rs`, these scenarios are its reference too.

## Features

| Feature | Contents |
| --- | --- |
| `host` (default) | The `host` module. It enables `serde`, and `sha2`, which names the children of a sandbox's aggregate share and hashes distro layers in their explicit verification |
| `bundled` | Stages a release at build time; see [Bundled artifacts](#bundled-artifacts) |
| `testing` | The fake OpenVMM and the `nvx-fake-openvmm` binary |
| `cli` | The `nvx-rs` CLI of `host::cli` and its `nvx-rs` binary; it enables `host`. See [The nvx-rs CLI](#the-nvx-rs-cli) |
| `serde` | The serde form of `Hypervisor` |
| `guest` | The `guest` module and the guest binaries; see [Guest binaries](#guest-binaries) |

Without default features, the crate has only `protocol`, `exec`, `contract`,
`net`, `gpt`, `uuid`, `Hypervisor`, and `Error`, and depends on nothing but
`serde` when `serde` is enabled and `nix` and `getrandom` when `guest` is;
`aci_edge_sandboxes_model` uses it that way.

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
as is, an `ErrorKind`, and an optional cause.

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
or `Instance::open` gives it. `aci_edge_sandboxes` maps the kinds onto its error codes; see its
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
`nvx::guest::time` and `nvx::guest::reseed` calls, on which both programs run.
Nor is the `gpt` library a program: it reads the partition table of a GPT
disk's block device.

| Binary | Program |
| --- | --- |
| `nvx-console-pending DEVICE` | Prints the number of bytes waiting to be read from a device, such as a console |
| `nvx-mmio-write ADDRESS VALUE` | Stores a 32-bit value at a physical address through `/dev/mem` (x86_64) |
| `nvx-reseed [--generation-only] SEED_FILE [PREVIOUS]`, `nvx-reseed --sample` | Reseeds the kernel's random number generator from a seed file and checks its VM generation ID, or prints a random sample |
| `nvx-exit [CODE]` | Stops the VM with an 8-bit exit code through OpenVMM's control port 0x604 (x86_64) |
| `nvx-device-io file ...`, `nvx-device-io network ...` | The benchmark helper that measures file and UDP round-trip I/O and prints one JSON line (x86_64) |
| `nvx-managed-agent CONTROL_TTY ROOTFS HOSTNAME UID GID USER HOME` | Serves the managed control session and supervises sequential workloads; `--exec-config-fd FD -- WORKLOAD ...` is the direct agent's internal launch helper |
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
NVX_BUNDLE=skip cargo test --all-features   # unit, conformance-vector, property, parity-fixture, and fake-OpenVMM tests
```

The tests need no hypervisor. The fake-OpenVMM test drives `Instance` end to end
against `nvx-fake-openvmm`, and the CLI test replays the
[golden scenarios](#golden-scenarios) and checks the guest-feature
requirement and, on Linux, Ctrl-C. The
[conformance vectors](tests/vectors/README.md) of the wire formats cover both
sides of every record, frame, and payload: the codec decodes each vector of
`tests/vectors/nvxc-v1.txt` to its values and encodes them to its bytes, it
accepts or refuses OpenVMM's vectors and cases of the `NVXS` session as they
say, and, on Linux, the guest's listener announcements match
`tests/vectors/nvxl-v1.txt`. Property tests check that records round-trip, and
that the `NVXS` record parser and the host's and the guest's `EXEC` and `MAPS`
decoders return, never panic, on arbitrary bytes. The NVXS test drives the
host's and the guest's roles of the session and their connections through a
broker that relays records as OpenVMM's does, with messages of an application
framing of its own that span records. The store test crashes orchestrator
processes at each step of a launch, recovers the VMM stand-ins that they leave
behind, and loads records that another build wrote, in `tests/fixtures/store/`.
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
rustfmt, Clippy with warnings denied for every feature and for the library
without default features, the tests, a build and the tests of the library
without default features, a synthetic-release `bundled` test, rustdoc with
warnings denied, the minimum supported Rust version (1.89), and a macOS check.
On Linux, it also lints, builds, and tests the guest binaries through `nvx.py`.
`python3 scripts/nvx.py test-aci-edge-sandboxes --backend kvm` exercises the
crate on a real hypervisor through `aci_edge_sandboxes`. Other workflows
measure, on Linux and Windows, the lines and branches that each family of golden
tests alone reaches, and run cargo-mutants on the files and with the tests that
a dispatch names; see [nvx-rs coverage](../doc/ci.md#nvx-rs-coverage) and
[nvx-rs mutation testing](../doc/ci.md#nvx-rs-mutation-testing).

### Sandbox specification tests

The sandbox tests specify the documented option grammar and the contracts with
OpenVMM and the guest, rather than Python's messages and input extensions.
Properties cover the restore-stable aggregate child names, reserved targets and
tokens, every generated launch token and its exact 1024-byte budget, policy option
sequences, and paths on the host's own filesystem. The CLI tests check that
`sandbox` records the preceding mount while `run` forwards absolute policy paths
independently of option order, for OpenVMM to attribute by containment.

During the transition tracked by microsoft/nvx#730, all six golden tests remain
unchanged beside these tests. This temporary table maps every semantic section of
the two fixtures to its replacement; generator metadata and the simulated working
directories are not input contracts.

| Fixture | Section | Specification tests |
| --- | --- | --- |
| `sandbox.json` | `identities` | `workload_identity_grammar`, `non_root_workload_identities_round_trip` |
| `sandbox.json` | `layers` | `layer_values_are_role_path_uuid`, `canonical_layer_uuids_reach_the_guest` |
| `sandbox.json` | `mount_values` | `mount_value_grammar` |
| `sandbox.json` | `mount_targets` | `mount_targets_are_canonical_and_byte_bounded`, `reserved_mount_targets_are_refused` |
| `sandbox.json` | `child_names` | `aggregate_child_name_known_answer`, `aggregate_child_names_follow_the_restore_contract` |
| `sandbox.json` | `mounts` | `shares_follow_the_openvmm_and_guest_contracts`, `host_paths_become_absolute`, `windows_drive_relative_and_unc_paths`, and the access-policy tables in `mount::tests` |
| `sandbox.json` | `launches` | `launch_arguments_follow_the_openvmm_contract`, `launch_settings_follow_the_guest_contract`, `kernel_command_line_stays_within_1024_bytes`, `reserved_kernel_tokens_are_always_refused` |
| `sandbox_options.json` | `run_share_targets` | `run_share_targets_are_disjoint`, `run_share_targets_bind_from_an_aggregate` |
| `sandbox_options.json` | `sandbox_mounts` | `policy_paths_attach_to_their_shares`, `policy_paths_need_a_preceding_mount_only_with_several_shares`, `policy_option_sequences_record_the_preceding_mount`, `aggregate_policy_paths_must_be_absolute` |
| `sandbox_options.json` | `environments` | `exec_environment_limits` |
| `sandbox_options.json` | `environment_files` | `environment_files_hold_a_json_array_of_strings` |
| `sandbox_options.json` | `distro_manifests` | `distro_manifest_binds_the_layer`, `systemd_entrypoints_and_images_are_refused_independently` |

The current sandbox policy refuses both systemd entrypoints unconditionally and
refuses a manifest that lists `systemd`, independently of the selected entrypoint.
It does not allow a systemd entrypoint when a manifest lists that package.

`tests/openvmm_vectors.rs` is an ignored test that compares the copies of
OpenVMM's vectors in `tests/vectors/openvmm/` with the files of the OpenVMM
submodule, which the `nvx-rs` job cannot check out. The NVX CLI jobs run it
once they check the submodule out, and so does an OpenVMM pin promotion; see
[OpenVMM's vectors](tests/vectors/README.md#openvmms-vectors).

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
