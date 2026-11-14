# Migrate from alpha.7

`nvx` 0.2.0-alpha.1 breaks `nvx` 0.1.0-alpha.7, the version that
crates.io published from commit
[`e42b1795`](https://github.com/microsoft/nvx/commit/e42b17959360c1738a99cb3036137dfbf3d5bf96).
It makes `nvx` a library of mechanisms and value types for the microVMs that
OpenVMM runs: a VM's configuration and launch, its snapshots and restores, its
control connection, and the guest's capture and guest restore. The stores, the
supervisor, the templates and clones, the exec engine, and the guest's process
modules are gone, and NVX's own programs, its control protocol, NVXC, the
guest space that nvx-init builds, and the code that only they use are private
to the crate, but for a hidden entry point per binary. The release reads none
of alpha.7's records, keys, or snapshots, and converts none of them: a program
that needs alpha.7 stays on it.

This guide says what breaks and where each part of alpha.7 went. The
[library guide](library.md) gives the release's workflows and what their
caller owns, the crate's [README](../nvx-rs/README.md) its features and
modules, and its rustdoc the contract of each item.

## Upgrade

1. With alpha.7's `nvx-rs`, stop and deprovision each sandbox, as
   [Upgrades](run.md#upgrades) describes: the release refuses alpha.7's
   sandbox records.
2. [Pin](#install-pins) the release exactly, with the guest tools that you
   install from it.
3. Drop the `bundled` feature, and give each launch the runtime's files
   yourself: see [Features and entry points](#features-and-entry-points).
4. Port each call through the [API crosswalk](#api-crosswalk).
5. Take over what the library no longer does:
   [What the caller owns](#what-the-caller-owns).
6. Start from [fresh state](#incompatible-state): capture the snapshots again,
   and provision the sandboxes again.
7. Update the guests, and the launchers that start NVX's guests on OpenVMM
   themselves: see [Guests](#guests).

## Install pins

The release starts a new series of pre-releases, `0.2.0-alpha`, so no
requirement of alpha.7's, such as `nvx = "0.1.0-alpha.7"`, selects it. Cargo
selects a pre-release only for a requirement that names one, and a caret
requirement such as `nvx = "0.2.0-alpha.1"` also accepts every later
pre-release of `0.2.0`, such as `0.2.0-alpha.2`, which `cargo update` takes.
So require the release exactly, with `=`, as the crate's
[README](../nvx-rs/README.md) advises. A host program takes the default
feature, `host`:

```toml
[dependencies]
nvx = "=0.2.0-alpha.1"
```

A guest init or agent takes the `guest` feature alone:

```toml
[dependencies]
nvx = { version = "=0.2.0-alpha.1", default-features = false, features = ["guest"] }
```

A guest image that NVX does not build installs NVX's guest programs from the
same version, as static executables built with the crate's `guest` profile, as
[The guest tools](distribution.md#the-guest-tools) describes. `cargo install`
reads a `--version` without an operator as that exact version. Repeat `--bin`
for each program, or pass `--bins` for all ten:

```bash
rustup target add x86_64-unknown-linux-musl
cargo install nvx --version 0.2.0-alpha.1 --locked \
  --no-default-features --features guest --profile guest \
  --target x86_64-unknown-linux-musl --bin nvx-ingress
```

The crate no longer pins NVX's runtime, the OpenVMM executable, guest kernel,
and initramfs that alpha.7's `bundled` feature downloaded from the release
that its `artifacts.json` named. A program runs those of an NVX release whose
guests speak the contracts that it speaks with them, labels each file with the
digest that the release's `SHA256SUMS` lists, and names those contracts in a
`CompatibilityIdentity` of its own, as [The library](library.md#the-library)
describes. The release's guests speak other versions of their contracts than
alpha.7's, so the runtime that alpha.7 bundled does not match it.

## Features and entry points

### Features

| Feature | alpha.7 | The release |
| --- | --- | --- |
| `host` (default) | The `host` module: artifact discovery, launch, policy lowering, control sessions, process and platform primitives, the supervision of VM instances, the sandbox launch profile, and the sandbox and template stores. It enables `serde` | The `host` module: VMs that the caller creates, spawns, attaches to, and stops in directories that it owns, one snapshot type, restores that the caller drives, authenticated control connections, and the lowering of network and filesystem policies. It enables `serde` |
| `bundled` | Staged a release's OpenVMM, kernel, and initramfs at build time for `Artifacts::bundled`, through the crate's build script, `links = "nvx"` with `DEP_NVX_ARTIFACTS_DIR`, `artifacts.json`, and the `NVX_BUNDLE`, `NVX_BUNDLE_DIR`, and `NVX_BUNDLE_HYPERVISOR` variables | Removed, with the build script and `links`: the caller locates the runtime's files, as [Install pins](#install-pins) says |
| `cli` | `host::cli`, whose public `main` the `nvx-rs` binary ran | The `nvx-rs` binary, which adds only its hidden `main` to the API |
| `testing` | `testing::fake_openvmm`, whose public `main` the `nvx-fake-openvmm` binary ran | The `nvx-fake-openvmm` binary, which adds only its hidden `main` to the API |
| `serde` | The serde form of `Hypervisor` | The serde form of `net::Ipv4Cidr`. `Hypervisor`, now `host::runtime::Hypervisor`, has its serde form with the `host` feature |
| `guest` | The `guest` module, with the guest's space, executions, and processes, and nine guest binaries | The `guest` module, without the guest's space and executions, and ten guest binaries: `nvx-init` is new |

Without default features, the crate holds what the host and the guest share:
`protocol`, `guest_restore`, `net`, `uuid`, and the errors.
alpha.7's also held `exec` and `contract`, which are gone, and `Hypervisor`,
which moved to `host::runtime`.

### Hidden program entry points

In alpha.7, the `main` of each of NVX's programs was a public, documented
function. In the release, each binary calls a `main` that `#[doc(hidden)]`
hides, which is no interface to call: run the binaries instead, or install the
guest's from the crate, as [Install pins](#install-pins) shows. These entry
points are all that NVX's programs add to the public API, with the modules
that hold them, which are hidden too but for `guest::reseed` and
`guest::time`, whose calls stay public.

| Binary | Feature | Hidden entry point |
| --- | --- | --- |
| `nvx-rs` | `cli` | `nvx::host::cli::main` |
| `nvx-fake-openvmm` | `testing` | `nvx::testing::fake_openvmm::main` |
| `nvx-console-pending` | `guest` | `nvx::guest::console_pending::main` |
| `nvx-device-io` | `guest` | `nvx::guest::device_io::main` |
| `nvx-exit` | `guest` | `nvx::guest::exit::main` |
| `nvx-ingress` | `guest` | `nvx::guest::ingress::main` |
| `nvx-init` | `guest` | `nvx::guest::init::main`, new |
| `nvx-managed-agent` | `guest` | `nvx::guest::managed_agent::main` |
| `nvx-mmio-write` | `guest` | `nvx::guest::mmio_write::main` |
| `nvx-reseed` | `guest` | `nvx::guest::reseed::main` |
| `nvx-time` | `guest` | `nvx::guest::time::main` |
| `nvx-time-probe` | `guest` | `nvx::guest::time_probe::main` |

## API crosswalk

CI checks the crate's public API against its snapshots,
[`linux.txt`](../nvx-rs/public-api/linux.txt) and
[`windows.txt`](../nvx-rs/public-api/windows.txt), which list each item of the
release, hidden items marked. Each row below names alpha.7's items by their
alpha.7 paths, and their replacement in the release, or what the caller does
in their place. *No longer public* means that an item left the public API:
the release removed it, or keeps it private to the crate for NVX's own
programs, and no caller can name it. An item of alpha.7 that no row names
keeps its path and its signature.

### Errors

| alpha.7 | The release |
| --- | --- |
| `nvx::ErrorCode`, `Error::code` | No longer public: act on `Error::kind`, an `ErrorKind`. The codes that `nvx-rs` prints are the CLI's own, as [Error codes](usage.md#error-codes) lists |
| `Error::new`, `Error::exited`, `Error::io`, `Error::not_found`, `Error::other`, `Error::protocol`, `Error::timed_out`, `Error::unavailable`, `Error::unsupported`, `Error::with_source` | No longer public: only `nvx` makes its errors. A caller reads them with `Error::kind`, `Error::message`, `Error::into_parts`, and their source, and makes errors of its own types |
| `ErrorKind` | Two new kinds: `RuntimeMismatch`, for a VM that another runtime launched, which alpha.7 reported as `Other`, and `StillRunning`, for a process whose exit a call could not confirm |

### The runtime's artifacts

| alpha.7 | The release |
| --- | --- |
| `nvx::Artifacts`, the same type as `host::artifacts::Artifacts`, with `new`, `discover`, `from_dir`, `from_release_dir`, `from_repo_layout`, `artifact_dir`, `ENV_OPENVMM`, `ENV_KERNEL`, `ENV_INITRD`, `ENV_DIR`, and `BESIDE_EXE_DIR` | No longer public: the caller locates OpenVMM, the guest kernel, and the initramfs, and gives their paths in a `host::runtime::Runtime`. NVX's CLI keeps a lookup of its own, which [`nvx-rs artifacts`](../nvx-rs/README.md#artifacts) prints |
| `Artifacts::bundled` | Removed, with the `bundled` feature |
| `Artifacts::runtime_identity`, and `host::store`'s `RuntimeIdentity`, `ArtifactIdentity`, `IdentityPart`, and `MAX_IDENTITY_LEN` | `host::runtime`'s `RuntimeIdentity`, `ArtifactIdentity`, `IdentityPart`, and `MAX_IDENTITY_LEN`: `RuntimeIdentity::new` takes three labels that the caller reads from install metadata, such as `ArtifactIdentity::sha256` of the digests that a release's `SHA256SUMS` lists, and a `CompatibilityIdentity` that names the caller's contracts with its guests |
| `RuntimeIdentity`'s `boot_contract`, `control_contract`, and `control_session`, `RuntimeIdentity::check`, and `IdentityPart`'s `BootContract`, `ControlContract`, and `ControlSession` | `RuntimeIdentity::compatibility` and `IdentityPart::Compatibility`: a `CompatibilityIdentity`, an opaque token of 1 to `MAX_IDENTITY_LEN` printable ASCII characters without spaces. The caller composes it from the contracts that it drives its guests through, such as the versions of its boot tokens and of its protocol with its guest agent, and `nvx` compares it and never interprets it. `CompatibilityIdentity::new` checks the token, so `check` is gone, and `RuntimeIdentity::differences` reports `Compatibility` when the tokens differ |
| `nvx::Hypervisor` | `host::runtime::Hypervisor`, with the `host` feature: the same variants, OpenVMM spellings, and calls, such as `from_env_or_default`, which reads `NVX_HYPERVISOR`. The crate root no longer exports it |
| `Artifacts::probe`, `host::platform::probe_hypervisor`, `host::platform::SUPPORTED` | `host::runtime::probe_hypervisor` checks a hypervisor, and `host::runtime::Hypervisor::supported_on_host` whether the host's operating system can have it. The caller checks that its files exist |
| `host::artifacts::check_manifest` | No longer public: the caller decides which releases its guests speak with, for example from the contract versions of a release's `SOURCE-MANIFEST.json`, and names its contracts with them in the `CompatibilityIdentity` of its `RuntimeIdentity` |
| `nvx::contract`: `MICROVM_ABI_VERSION`, `BOOT_CONTRACT_VERSION`, `CONTROL_SESSION_PROTOCOL_VERSION`, `CONTROL_CONTRACT_REVISION`, `SNAPSHOT_FORMAT_VERSION`, `DEVICE_STATE_SCHEMA_VERSION`, `TIME_ABI_VERSION`, `RESTORE_PACKET_VERSION`, and `manifest_is_compatible` | No longer public, with the module: the caller names the contracts that it drives its guests through in a `CompatibilityIdentity`, and the versions of OpenVMM's formats that the crate reads are private to it |
| The rest of `host::artifacts`: `ArtifactDir`, `ArtifactPaths`, `GuestPaths`, `Guest`, `Layout`, `absolute`, `openvmm_executable`, and the names of a release's files, such as `KERNEL_NAME`, `CHECKSUMS_NAME`, and `MANIFEST_NAME` | No longer public: the caller knows the layout of the releases that it runs, which [Release packages](build.md#release-packages) gives |

### Launches and VMs

| alpha.7 | The release |
| --- | --- |
| `host::launch::LaunchSpec`, and `host::sandbox`'s `SandboxPlan`, `SandboxLaunch`, `SandboxSettings`, `SandboxNetwork`, `SandboxUpper`, `SandboxDistro`, and `PreparedLaunch`, with `host::sandbox::plan` and `host::sandbox::prepared` | One `host::config::VmConfig` for cold boots, captures, and restores: a `RootConfig`, blockless or a `LayeredRoot` with an `UpperConfig`, a `protocol::view::View`, a `NetworkConfig`, an `ExportPlan`, a `ConsoleAttachment`, and, for a cold boot, a `Boot`. Every launch also takes a `host::runtime::Runtime`; see [The library](library.md#the-library) |
| `host::supervisor::launch`, `host::supervisor::spawn` | `host::vm::create(&runtime, &config, from, &dir)`, which checks a launch and starts nothing, then `host::vm::spawn`, in a directory that the caller creates with `vm::create_private_dir` and opens with `vm::VmDir::open` |
| `host::supervisor::VmmCommand`, `host::supervisor::spawn_command`, `host::supervisor::Inherited` | `vm::Prepared::into_command`, whose `vm::VmmCommand` and `vm::InheritedHandle`s a launcher of the caller's own starts, as `VmmCommand` documents and the [`external-launch`](../nvx-rs/examples/external-launch.rs) example does |
| `host::supervisor::Launched`, `host::supervisor::Vmm`, `host::supervisor::Starting` | `host::vm::Launched`: the VM's verified `host::process::Process`, its `VmOutputs`, its `Capability`, and, for a restore, its `ReadyListener` |
| `host::supervisor::reattach`, `host::supervisor::Reattached` | `host::vm::attach` and `host::vm::verify`, of a `vm::SavedVm` that the caller saved, and `vm::Attached` |
| `host::supervisor::StopOutcome`, `host::supervisor::Stopped` | `host::vm::stop`, whose callback asks the guest to end the VM in the caller's protocol, and `vm::Stopped` |
| `host::supervisor::SpawnLock`, `host::supervisor::lock_spawns` | No longer public: the spawn's lock is internal to `vm::spawn`. On Windows, the caller keeps its other process starts out of a VM's spawn |
| `host::instance`'s `Instance`, `LockedInstance`, `Running`, `Started`, `Stopped`, `Found`, and `Mismatched` | `host::vm`'s `create`, `spawn`, `verify`, `attach`, and `stop`, over records of the caller's own. A VM that another runtime launched fails with `ErrorKind::RuntimeMismatch` |
| `host::instance::generate_capability`, `host::instance::check_launch` | `vm::create` generates the control capability, which `Prepared::capability` and `Launched::capability` hold, and checks the whole launch. `VmConfig::check` and `VmConfig::check_command_line` check a configuration without the host |
| `host::instance`'s `read_json`, `write_json`, `write_private`, `remove_if_present`, and `remove_interrupted_writes` | No longer public: the caller does its own file I/O |
| `host::launch`'s `COMMAND_LINE_BUDGET`, `DEFAULT_GUEST_NETWORK`, and `PROCESS_COMMAND_LINE_LIMIT` | The same names in `host::config`, with `fit_process_command_line` and `process_command_line_units` |
| `host::launch::base_command_line`, `host::launch::kernel_parameters_problem` | `VmConfig::kernel_command_line` and `VmConfig::check` |
| `host::launch`'s `DEFAULT_HOSTNAME`, `DEFAULT_MEMORY_MIB`, `MAX_TIMEOUT`, and `timeout_problem`, and `host::sandbox`'s `DEFAULT_HOSTNAME`, `DEFAULT_ENTRYPOINT`, `KERNEL_COMMAND_LINE_MAX_SIZE`, `SANDBOX_COMMAND_LINE_MAX_SIZE`, and `OPENVMM_COMMAND_LINE_RESERVE` | No longer public: the caller picks its defaults, and `VmConfig::check` and `vm::create` check what OpenVMM and the guest accept |
| `host::launch::WorkloadIdentity`, `host::sandbox::parse_workload_identity`, `host::sandbox::DEFAULT_WORKLOAD_IDENTITY` | No longer public: the workload's identity is the caller's to give, in its own boot tokens or protocol; see [Guests](#guests) |
| `host::platform::create_private_dir`, `host::platform::Transport` | `host::vm::create_private_dir`. `Transport` is no longer public: the caller speaks with its guest over a `host::vm::Connection`, and reads and writes the boot console through a `host::vm::Console` |
| `host::platform::file_identity`, `host::platform::process_start_time` | No longer public. `host::process::ProcessRef` names a process by its ID, its start time, and the host's boot |

### State

| alpha.7 | The release |
| --- | --- |
| `host::store`'s `SandboxStore`, `LockedStore`, `Launch`, `LaunchId`, `LaunchJournal`, `LaunchState`, `Lingering`, `LogClaim`, `Recovery`, `RemovalError`, `RuntimeRecord`, `CallerFiles`, `IdentityCheck`, `FORMAT`, and `MAX_ENDPOINT_LEN` | No longer public: the library keeps no state, and NVX's CLI keeps a store of its own, in formats of its own. The caller saves a launch's `Prepared::capability` and `Prepared::outputs` before its spawn, and its `vm::SavedVm` after it, and finds the VM again with `vm::verify` or `vm::attach` |
| `host::store`'s file names, `LOCK_NAME`, `JOURNAL_NAME`, `RUNTIME_NAME`, `CAPABILITY_NAME`, `SOCKET_NAME`, `LOG_NAME`, `CONSOLE_NAME`, `OUTCOME_NAME`, `RESTORE_READY_NAME`, the `CLONE_*_NAME` constants, and `FILE_NAMES` | No longer public: a VM directory has fixed names, which `vm::VmDir` and `vm::VmOutputs` give, and the capability has no file. The caller names its own records |

### Templates, clones, and snapshots

| alpha.7 | The release |
| --- | --- |
| `host::template::TemplateKey`, `host::template::TemplateManifest` | `host::snapshot::SnapshotKey` and `host::snapshot::SnapshotManifest`, whose key has an encoding of its own; see [Incompatible state](#incompatible-state) |
| `host::template`'s `KeyIdentity`, `TimeContract`, `Eligibility`, `Downtime`, `DowntimeSource`, `HostClock`, `HostTime`, `BlockGeometry`, `Invariants`, `MANIFEST_NAME`, `MAX_MANIFEST_LEN`, and `MAX_DOWNTIME_NS` | The same names in `host::snapshot`. `BlockGeometry` and `Invariants` are also `host::config`'s |
| `host::template::KEY_NAME`, `host::template::SNAPSHOT_NAME` | A snapshot is a directory of exactly `host::snapshot::KEY_FILE`, `DESCRIPTION_FILE`, and `OPENVMM_DIR`, which `host::snapshot::Snapshot::open` checks |
| `host::template::MEMORY_BLOCK_SIZE` | `SnapshotKey::memory_block_size` of a snapshot |
| `host::template`'s `CaptureClass`, `ClassIdentity`, `TemplatePlan`, and `TemplateResources` | A `VmConfig` whose `Boot::capture` holds a `host::config::CaptureConfig`, whose `CaptureConfig::identity` is a `CaptureIdentity` |
| `host::template::Capture`, `host::template::Created`, and the captures of a template store, `CAPTURES_NAME`, `CAPTURE_RECORD_NAME`, and `TRANSACTION_NAME` | `vm::create` of a capture, whose `Prepared::capture_context` the caller keeps, then `host::snapshot::finish_capture` once the VM exited, which checks the capture and writes `key.bin` and `snapshot.json` beside the VM directory's `openvmm/`, and returns the checked key in a `CapturedSnapshot`. The caller moves only those three entries into a private directory of its own, and keeps them immutable |
| `host::template`'s `Store`, `StoreId`, `Index`, `Generation`, `StorageGeneration`, `Lease`, `LeaseId`, `Acquired`, `Retirement`, and `Teardown`, and a store's `INDEX_NAME`, `INDEX_LOCK_NAME`, `LEASES_NAME`, `TOMBSTONES_NAME`, `GENERATIONS_NAME`, `STORE_RECORD_NAME`, `STORE_FORMAT`, `MAX_GENERATIONS`, and `SCRATCH_NAME` | Removed: the caller names, keeps, and locks its snapshots, as NVX's CLI does in a snapshot store of its own |
| `host::template`'s `CloneRequest`, `ClonePlan`, and `CloneUpper` | `vm::create(&runtime, &config, Some(&snapshot), &dir)`, of a `Snapshot` that `Snapshot::open` opened, for a `VmConfig` without a `Boot`, and `Prepared::guest_restore_record` |
| `host::template`'s `CloneControl`, `Restoring`, `Activated`, `Started`, `RestoreDeadlines`, `READY_TO_START_TIMEOUT`, `RESTORE_TO_READY_TIMEOUT`, and `MAX_ACTIVATION_SESSIONS` | `host::restore::RestoreMachine`, which the caller drives with its own I/O, clock, and retries, through `RestoreInput` and `RestoreAction` |
| `host::template::RESTORE_READY_EVENT` | `host::restore::ReadyListener`, which `vm::Launched::ready` holds, and `protocol::restore_ready::parse_ready_event` |
| `host::template`'s `Evidence`, `Outcome`, `Progress`, `Stage`, `status_evidence`, `CloneFailure`, `FailureRecord`, `FAILURE_FORMAT`, `CloneRecords`, `reattach_clone`, and `ReattachedClone` | A failed restore's `host::restore::RestoreError`, with its `RestorePhase`, `RestoreEvidence`, and `Attribution`. The caller records what it keeps of a failure, and reattaches with `vm::attach` |

### Exports, policies, and the sandbox profile

| alpha.7 | The release |
| --- | --- |
| `host::filesystem::plan`, `host::filesystem::ExportRequest`, `host::filesystem::Mapping`, `host::filesystem::Target`, `host::filesystem::PathList` | No longer public: NVX's CLI keeps the planner. The caller builds a `host::filesystem::ExportPlan` of its own, checks it with `ExportPlan::check`, and gives it in `VmConfig::filesystem` |
| `nvx::resolve_guest_path`, `host::filesystem::resolve_guest_path` | Removed: the caller resolves the host path, such as with `std::fs::canonicalize`, then calls `nvx::guest_path` |
| `host::filesystem::FileIdentity`, `Bind::identity`, `ExportChild::denied_identities` | No longer public: a plan holds paths only, and the caller keeps them naming the objects that it means, as [`host::filesystem`](../nvx-rs/src/host/filesystem/mod.rs) details. NVX's CLI keeps the identities that it pins beside its plans |
| `ExportPlan::verify`, `ExportPlan::kernel_token`, `ExportPlan::openvmm_arguments` | No longer public: `vm::create` lowers the plan of `VmConfig::filesystem`, and `VmConfig::mapping_table` and `ExportPlan::guest_table` give the guest's mapping table |
| `host::sandbox::mount`, with `SandboxMount`, `MountPolicy`, `MountAccess`, `MountKind`, `MountOwner`, `PolicyKind`, `BootShares`, `plan_mounts`, `parse_mount_value`, and its other items, and its re-exports in `host::sandbox` | No longer public: NVX's CLI parses its own `--mount` syntax. The guest targets that a share may not take are `protocol::share`'s `RESERVED_MOUNT_TARGETS`, `RESERVED_EXACT_MOUNT_TARGETS`, and `reservation`, and `host::filesystem::ReservedTargets` |
| `host::sandbox::options`, `host::sandbox::layer`, `host::sandbox::distro`, `host::sandbox::scratch`, `host::seal`, and `host::sandbox`'s `LayerRole`, `LAYER_ROLES`, `BlockRole`, `PLAN_VERSION`, `require_file`, and `SandboxLayer` | No longer public: the caller checks its own options and images |
| `host::network::policy`, with `compile_policy`, `compile_policy_bytes`, `compile_policy_file`, `CompiledEgressPolicy`, `MAX_JSON_DEPTH`, `MAX_POLICY_FILE_SIZE`, and `MAX_RULES_PER_ACTION` | No longer public: NVX's CLI still reads [policy files](../nvx-rs/README.md#egress-policy-files). The caller builds a `host::network::NetworkPolicy` of `EgressRule`s, `Destination`s, and `PortSelector`s, which `NetworkPolicy::native_egress_rules` lowers |

### Transport and protocols

| alpha.7 | The release |
| --- | --- |
| `host::session`'s `connect`, `ControlSession`, `ExecEvent`, and `SessionError` | No longer public: they speak NVXC. The caller connects with `host::vm::Launched::connect` or `host::vm::attach`, and speaks a protocol of its own over the `host::vm::Connection` that they open |
| `host::nvxs::HostConnection`, `host::nvxs::HostConnectionError` | No longer public, nor is `host::nvxs`. `host::vm::Connection` carries an ordered byte stream, where `HostConnection` carried NVXS records, so the caller frames its own messages. Its `send` and `receive` fail with a `host::vm::ConnectionError`, whose `ConnectionErrorKind` tells the failure, where they failed with a `HostConnectionError` |
| NVXC in `protocol`: `AppFrame`, `encode_app`, `decode_app`, the `APP_*` constants, `ExecPayload`, `encode_exec_payload`, `decode_exec_payload`, `GuestExecPayload`, `decode_guest_exec_payload`, `Workload`, `WorkloadEnvironment`, `ExitCategory`, `GuestFeatures`, the `EXEC_*` flags, the limits `MAX_ARGUMENTS`, `MAX_ARGUMENT_BYTES`, `MAX_CWD_BYTES`, `MAX_ENVIRONMENT`, `MAX_GUEST_CWD_BYTES`, `MAX_OUTPUT_BYTES`, and `MAX_TIMEOUT_MS`, and the refusals `BUSY`, `CWD_FAILED`, `INVALID_REQUEST`, `LAUNCH_FAILED`, `MAPPINGS_INCOMPLETE`, `MAPPING_FAILED`, and `UNSUPPORTED_OPERATION` | No longer public: NVXC v1 and its FEATURES exchange are gone, and NVX's programs speak NVXC v2 among themselves. A caller speaks a protocol of its own over a `host::vm::Connection` |
| `protocol::WORKLOAD_ACCOUNT_TOKEN` | No longer public: NVX's guests still read `nvx_workload_account=create`, which the caller writes itself |
| `protocol::status`'s `ACTIVATION_DEADLINE`, `ACTIVATION_PROTOCOL`, `ACTIVATION_RECORD`, `ACTIVATION_RESOURCES`, `ACTIVATION_ROOT`, `ACTIVATION_WORKLOAD`, `ACTIVATION_EXPORT`, `ACTIVATION_NETWORK`, `ACTIVATION_ORDER`, and `ACTIVATION_STATUSES` | `GUEST_RESTORE_DEADLINE`, `GUEST_RESTORE_PROTOCOL`, `GUEST_RESTORE_RECORD`, `GUEST_RESTORE_RESOURCES`, `GUEST_RESTORE_ROOT`, `GUEST_RESTORE_WORKLOAD`, `GUEST_RESTORE_EXPORT`, `GUEST_RESTORE_NETWORK`, `GUEST_RESTORE_ORDER`, and `GUEST_RESTORE_STATUSES`, with the same values, 50 to 58 |
| `protocol::root::RootLayout`, `RootLayout::tokens` | `RootLayout` also holds `overlay`, `journal`, `gpt_first_partition_bound`, and `max_memory_percent`, and `tokens` returns a `Vec<String>`, where it returned two tokens |

### Processes and reports

| alpha.7 | The release |
| --- | --- |
| `host::process::Process::wait(deadline)`, which returned `Result<bool>` | Returns `Result<Option<Exited>>`, an `Exited` that proves the exit |
| `host::process::Process::open(&reference)`, which returned `Result<Option<Process>>` | Returns `Result<Opened>`: `Opened::Running`, with the process, or `Opened::Exited`, when the reference proves that it exited |
| `host::report::read(path)`, which returned `Option<Report>` | Returns `Result<Option<Report>>`: `Ok(None)` for a missing report, and an error for one that cannot be read or parsed |

### The guest-restore record

| alpha.7 | The release |
| --- | --- |
| `activation`, with `ActivationRecord`, `ActivationError`, and `ReadyActivation` | `guest_restore`, with `GuestRestoreRecord`, `GuestRestoreError`, and `GuestRestoreReady`. The module's other items keep their names, but for those that the rows below name |
| `activation::VERSION`, 1 | 2: the record's decoder refuses version 1, and alpha.7 cannot read version 2 |
| `activation::WorkloadView`, `Configuration::workload` | `protocol::view::View`, which holds no identity, and `Configuration::view` |
| `activation::MachineIdMode`, `activation::MAX_HOSTNAME_BYTES` | `protocol::view::MachineIdMode` and `protocol::view::MAX_HOSTNAME_BYTES`, beside `View`, which cold boots share |
| `activation::OverlayMode`, `activation::Journal` | `protocol::root::OverlayMode` and `protocol::root::Journal`, beside `RootLayout`, which cold boots share |
| `activation::WorkloadIdentity`, `ActivationError::WorkloadIdentity`, `Field::WorkloadIdentity` | Removed: the record carries no workload identity, which the caller gives in its own protocol. `Field::ALL` lists the 13 other fields |
| `activation::Identity`, `Image::binding`, and the `binding` of `Image::Erofs`, `Image::Gpt`, and `Upper::Scratch` | Removed: the caller checks its images before it launches them |
| `activation::StoreGeneration`, `activation::TemplateReference`, `RestoreSection::template` | Removed: the record names no store or template |
| `Network::policy`, `Export::plan` | Removed: the record carries the restore's DNS, proxy, and publication, and the export's kind, but no identity of a policy or a plan |
| `Upper::Tmpfs` | `Upper::Tmpfs { max_memory_percent }`, the cap of a RAM upper, from 1 to 90 percent, which `GuestRestoreError::TmpfsMemoryPercent` refuses otherwise |

### Executions

| alpha.7 | The release |
| --- | --- |
| `nvx::exec`, with `exec::peer` and `exec::runner`: `Runner`, `Peer`, `Key`, `Limits`, `Window`, `Timers`, `Pool`, `Unit`, `Outcome`, `Receipt`, `Settlement`, `Status`, `Stream`, `Cause`, `Violation`, and `LaunchFailure` | No longer public: the library runs no executions. A caller runs them through its own guest agent and protocol, and NVX's programs run theirs over NVXC v2. The [guest-agent guidelines](guest-agent-guidelines.md#run-the-executions) say how an init runs them |
| `host::instance::Execution`, `host::instance::OutputSink`, `host::instance::WorkloadOutcome` | No longer public: the caller starts a workload, and receives its output and outcome, in its own protocol |
| `guest::exec`, `guest::exec::binding`, `guest::spawn`, `guest::process`, `guest::cgroup`, `guest::entry` | No longer public: the library runs no executions, and the caller's init isolates its executions as it chooses. nvx-init runs each as a plain process in its own process group, in the VM's one space, without a cgroup, a namespace, or limits of its own |

### The guest

| alpha.7 | The release |
| --- | --- |
| `guest::clone` | `guest::restore`: the old name is gone. Its `CaptureCause` and `CaptureStage` have a new `Repair` variant |
| `guest::clone`'s `CapturePreparation`, `CaptureBarrier`, `AdmissionFence`, `CaptureError`, `Captured`, `Devices`, `Repair`, `Restored`, and `repair` | `guest::snapshot::capture(&devices, disks, deadline)`, one call that checks the devices, captures, and, in a VM that a restore started, repairs the restore before it returns `Captured::Restored`. A `CaptureFailure` says whether the guest resumed or must end. The init stops its own executions first, as [Coordinate a capture](guest-agent-guidelines.md#coordinate-a-capture) describes |
| `guest::clone`'s `Activation`, `ActivationInput`, `ActivationAction`, `ActivationState`, `ActivationCause`, and `ActivationFailure` | `guest::restore`'s `GuestRestore`, `GuestRestoreInput`, `GuestRestoreAction`, `GuestRestoreState`, `GuestRestoreCause`, and `GuestRestoreFailure` |
| `guest::clone::Activation::new`, `guest::clone::Activation::with_resources` | Take no admission fence: `guest::restore::GuestRestore::new(&repaired, timeout, now)` |
| `guest::clone`'s `apply`, `Applied`, `ApplyError`, `AppliedTeardownError`, `NetworkError`, and `RootTemplate` | No longer public: the init applies a restore's guest-restore record in its own space, as [Complete a restore before Ready](guest-agent-guidelines.md#complete-a-restore-before-ready) describes |
| `guest::root` | No longer public: nvx-init's own space. Each init builds its own from the tokens of `protocol::root`, `protocol::view`, `protocol::export`, and `protocol::share`, and the records of `guest_restore`, as [Build the space](guest-agent-guidelines.md#build-the-space) describes |
| `guest::mappings`'s `Mapper`, `bind`, `ApplyError`, `MappingError`, `Reserved`, `SourceRoot`, `TargetRoot`, `Step`, `TeardownError`, and `UnmountFailure` | No longer public: the init mounts the mappings itself. `guest::mappings::MappingTable` keeps the account of the host's mapping table, beside `Mapping`, `Access`, `Kind`, and `InvalidMapping` |
| `nvx::gpt`, `guest::gpt::read_disk`, `guest::gpt::DeviceError` | No longer public: GPT parsing is internal to NVX's guest programs. The control plane checks and stages its own images, and each init interprets them. `nvx::uuid`, `protocol::root::ImageFormat::Gpt`, and `guest_restore::Image::Gpt` remain public |
| `guest::ingress`'s `Announcer`, `PROGRAM`, and `StartError` | Removed: the guest restore reports the publication and the gateway, and the init runs `/sbin/nvx-ingress` itself, or has `nvx-ingress start` run it |
| `guest::time`'s `CaptureRequest`, `CaptureOutcome`, `Restore`, `RestoreFinish`, `CpuSet`, `MemoryRange`, and `ScratchPolicy`, and `TimeAbi::capture` and `TimeAbi::restore_finish` | No longer public: `guest::snapshot::capture` runs the time ABI's capture and restore finish. `TimeError` has a new `Deadline` variant |

## What the caller owns

The library keeps no state, takes no lock that its caller sees, and speaks no
protocol of the caller's with its guest. What alpha.7 did for its caller is
the caller's own now, as
[What the caller owns](library.md#what-the-caller-owns) details:

| Obligation | What alpha.7 did | What the caller does |
| --- | --- | --- |
| Durable state | The sandbox store of `host::store`, with its launch journals and runtime records, and the template store of `host::template`, with its leases, capture transactions, clone and failure records, and quarantine | Saves, durably and privately, a launch's capability and outputs before its spawn and its `vm::SavedVm` after it, a capture's `CaptureContext`, and its snapshots, which it names and keeps |
| Paths and images | `Artifacts`' discovery, `resolve_guest_path`, the export planner, `host::seal`, and the distro and scratch bindings of `host::sandbox` | Locates and labels the runtime's files, checks its images and keeps their paths naming what it checked, and makes its `ExportPlan`, whose paths it keeps naming the objects that it means |
| Locking | The stores' locks, the template store's leases, and `lock_spawns` | Takes its own lock of each snapshot on Linux, from `vm::create` through a peer-verified restore-ready event or the VM's verified exit, and again around the removal of the VM directory's `openvmm/`, keeps its other process starts out of `vm::spawn` on Windows, and orders its own operations on each VM |
| I/O | The exec engine, the NVXC session of `host::session`, and the clone driver's `CloneControl` | Speaks its own protocol with its guest over a `host::vm::Connection`, and does the I/O of each `RestoreAction` |
| Timing | The library's own bounds, such as `MAX_TIMEOUT`, `RestoreDeadlines`, and the clone driver's timeouts | Chooses the deadline of each wait and the budget of each restore, and how often a lost session reconnects |
| Recovery | The store's recovery of interrupted launches, `supervisor::reattach`, and `template::reattach_clone` | Finds its VMs again from what it saved, with `vm::verify` or `vm::attach`, and decides what becomes of a VM after a `RestoreError` or a `StillRunning` error, since neither stops it |
| The guest's space and executions | `exec`, the guest's `exec`, `spawn`, `process`, `cgroup`, and `entry`, `guest::root`, and the mapper of `guest::mappings` | Its init builds the VM's one space and runs the executions, as the [guest-agent guidelines](guest-agent-guidelines.md#ownership) describe |

## Incompatible state

The release reads nothing that alpha.7 wrote, and converts nothing: an upgrade
starts from fresh state.

- **The library's records.** alpha.7's sandbox stores, with their launch
  journals and runtime records, and its template stores, with their indexes,
  leases, tombstones, generations, and captures, and the records of clones and
  of their failures: no code of the release reads them. Remove them once
  nothing of alpha.7 uses them.
- **Snapshots and their keys.** A snapshot of the release is a directory of
  exactly `key.bin`, `snapshot.json`, and `openvmm/`, whose key is a
  `SnapshotKey` of an encoding that no earlier build wrote, and
  `Snapshot::open` accepts nothing else. alpha.7's template generations, their
  keys, and raw OpenVMM snapshot directories do not restore. Capture each
  snapshot again, with the release and the runtime that it runs: a snapshot
  restores only under the runtime identity that captured it; see
  [Snapshot compatibility](design/1.5-compatibility-and-versioning-model.md#152-snapshot-compatibility)
  and
  [Runtime identity](design/1.5-compatibility-and-versioning-model.md#154-runtime-identity).
- **Guest-restore records**, alpha.7's activation records. The release reads
  and writes version 2 alone.
- **NVXC.** NVXC v1 and its FEATURES exchange are gone, and NVX's hosts and
  guests speak NVXC v2 alone, which keeps no compatibility with v1: run NVX's
  guest images and guest programs of the same release as its host.
- **The CLI's sandboxes.** The release's `nvx-rs` reads only the sandbox
  configuration of format 10, with the pins of its export, `export_pins`. It
  refuses alpha.7's, of formats 1, 7, and 8, with `format-unsupported` and the
  advice to deprovision the sandbox and provision it again, and fails on the
  runtime records of alpha.7's store, which it cannot parse. Stop and
  deprovision each sandbox with alpha.7 before the upgrade, as
  [Upgrades](run.md#upgrades) describes, and provision it again with the
  release.
- **Restored sandboxes of earlier builds.** `nvx-rs sandbox deprovision`
  refuses a restored sandbox, which `nvx-rs snapshot restore` creates, that a
  build before the release provisioned, since it reads the sandbox's
  configuration to find the snapshot whose view the sandbox holds. Stop such a
  sandbox, then remove its state directory while no restore of its snapshot
  runs.

## Guests

- **Boot tokens.** The shell `/init` of NVX's initramfs takes the workload's
  identity from `nvx_identity=UID:GID`, and its lifecycle from
  `nvx_mode=one-shot` or `nvx_mode=managed`, NVX's own tokens. It ignores the
  `nvx_lifecycle=` token that OpenVMM's `--microvm-lifecycle` appends, and
  fails closed on the `nvx_workload_uid=` and `nvx_workload_gid=` tokens that
  OpenVMM's `--microvm-workload-identity` appends: a boot that has them stops
  with `NVX-WORKLOAD-ERROR` and status 125 before it starts a workload or a
  guest agent, unless `nvx_identity=` names the same UID and GID. A launcher
  that starts OpenVMM itself with that option also passes `nvx_identity=`.
  `nvx` passes neither option; see
  [Effective command line](design/5.1-boot-protocol.md#512-effective-command-line).
- **Layered roots.** A boot with `nvx_sandbox=1` runs `/sbin/nvx-init`, NVX's
  PID 1 for layered roots, which builds one space for the VM, as
  [Guest agents and workload model](design/4.2-guest-agents-and-workload-model.md)
  describes. `/sbin/nvx-init-agent` and the Alpine and Azure Linux container
  helpers are gone.
- **Executions.** In NVX's guests, each execution is a plain process in its
  own process group, with no cgroup, namespace, or limit of its own: the VM's
  processors and memory are its only limits.
- **The guest kernel.** NVX's guest kernel no longer has magic SysRq,
  `CONFIG_MAGIC_SYSRQ`. Its configuration now comes from a feature inventory,
  which leaves off what no NVX path needs, such as `/dev/kvm` in the guest; see
  [What the inventory removed](build.md#what-the-inventory-removed).

## The nvx-rs CLI

[Command-line usage](usage.md#nvx-rs) is the reference of the release's
commands, and [Run](run.md) describes their workflows. What changes for a
script that ran alpha.7's:

- `run --restore-snapshot` takes the name of a snapshot of the store that
  `--store-dir` names, which [`snapshot capture`](usage.md#snapshot-capture)
  published, and refuses a raw OpenVMM snapshot directory.
  `--restore-processors`, `--restore-memory-mib`, and `--restore-ready-path`
  are gone. `snapshot capture` and
  [`snapshot restore`](usage.md#snapshot-restore) are new.
- `sandbox provision` and `sandbox run` refuse `--memory-max` and
  `--pids-max`: the VM's resources are the only limit.
- `sandbox exec` runs one execution at a time in a sandbox: another waits for
  it up to `--wait`, 60 seconds by default, then fails with `sandbox-busy`.
  `sandbox exec`, which refused more than 1 MiB of a workload's output, and
  `sandbox run` stream the workload's output as it arrives, without a limit
  on its size.
- The error codes `guest-features-missing`, `sandbox-leased`,
  `template-ineligible`, and `template-tombstoned` are gone, and
  `still-running` is new; see [Error codes](usage.md#error-codes).
