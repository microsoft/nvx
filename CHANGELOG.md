# Changelog

This file lists NVX's breaking releases: those of the
[`nvx` crate](nvx-rs/README.md), with the guest artifacts of the same commit.
CI publishes the crate's other versions every night, as
[crates.io releases](doc/ci.md#cratesio-releases) describes.

## 0.2.0-alpha.1

`nvx` becomes a library of mechanisms and value types for microVMs, on which
NVX's own programs and other users build
([microsoft/nvx#1076](https://github.com/microsoft/nvx/issues/1076)). The
release is not backward compatible: users that need `0.1.0-alpha.7` stay on it.
[Migrating from alpha.7](doc/migrate-from-alpha.7.md) maps each removed or
changed item to its replacement, or to what its caller now does.

### Breaking changes

- **No durable state.** The sandbox and template stores, the instance layer and
  the supervisor are gone from `nvx::host`. A caller keeps its own state from the
  values that `nvx::host::vm` returns.
- **One launch.** The old configurations and launch paths, such as
  `LaunchSpec`, `SandboxPlan` and `ClonePlan`, are gone: `nvx::host::vm::create`
  takes a `VmConfig` and, for a restore, a `Snapshot`.
- **No exec.** The exec engine, `nvx::exec`, and the guest's process modules,
  `nvx::guest::{exec, spawn, process, cgroup, entry}`, are gone. A guest's init
  runs its executions itself.
- **`nvx::guest::clone` is `nvx::guest::activation`.** The old name is gone.
- **The guest's space is private.** nvx-init's space, `nvx::guest::{root, gpt}`
  and the mapper of `nvx::guest::mappings`, is private to `nvx`.
  `resolve_guest_path` is gone, and `ExportPlan` holds paths only.
- **NVXC is private.** NVX's own protocol is private to `nvx`, and speaks only
  version 2: version 1 and its `FEATURES` request are gone.
- **Errors.** `ErrorCode` and `Error`'s constructors are private: callers tell
  errors apart by their `ErrorKind`, which gains `RuntimeMismatch` and
  `StillRunning`.
- **NVX's programs are private.** Each binary calls a hidden `main`, which is all
  that the programs add to the public API. Artifact discovery (`nvx::Artifacts`),
  image staging, the control session and the platform layer are private to them.
- **Features.** The `bundled` feature is gone, with its build script.
- **Fresh state.** The CLI reads a sandbox's configuration only in format 10,
  which requires `export_pins`: deprovision each sandbox that an earlier build
  provisioned, and provision it again. Nothing reads alpha.7's records, keys or
  snapshots, so a deploy captures its snapshots again.
- **The guest's `/init`** fails closed on OpenVMM's workload identity: a command
  line with `nvx_workload_uid=` or `nvx_workload_gid=` boots only when
  `nvx_identity=` names the same UID and GID.
- **The guest kernel** no longer sets `CONFIG_MAGIC_SYSRQ`.
