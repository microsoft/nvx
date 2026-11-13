//! An independent adapter of the NVX runtime, with a host program and a guest agent. NVX's CI
//! builds it against `nvx` the way an adapter outside the repository would: as a path dependency
//! with default features off, with a lockfile of its own, and through the public API alone.
//!
//! Each configuration enables one side of `nvx`:
//!
//! - protocol-only, without features: the control protocol, the contract versions, and the other
//!   items that `nvx` has without features, with the adapter's own [`frame`]s, the bodies of its
//!   messages about a layered [`root`], its [`probe`]s, and [`exec`]utions, and the kernel command
//!   line and statuses of its [`template`] mode;
//! - host-only, with the `host` feature: the host API too, and the host program, in the `host`,
//!   `runtime`, `initramfs`, `staged`, `sharing`, `layered`, `capture`, `clone`, `generations`, and
//!   `hook` modules and the `runtime-adapter-host` binary;
//! - guest-only, with the `guest` feature: the guest API too, and the guest agent, in the `guest`
//!   and `worker` modules and the `runtime-adapter-guest` binary, a static
//!   `x86_64-unknown-linux-musl` executable.
//!
//! The adapter brings its own framing to the control session between the host and the guest, in
//! place of NVX's `NVXC` frames. The guest agent serves requests in that framing, and
//! `runtime-adapter-guest serve` runs it on its standard input and output. The host program's
//! client attaches to the session with the launch capability and sends those requests.
//!
//! The host program runs the guest agent in VMs of NVX's artifacts, in place of NVX's managed
//! agent, through NVX's supervisor: `runtime-adapter-host launch` starts a VM, and later
//! processes reattach to it and stop it with `runtime-adapter-host reattach` and `stop`, as an
//! orchestrator does after it restarts. The `runtime` module takes the VMs' runtime identity from
//! NVX's identity of its artifacts, with the guest agent's build in it, and the `initramfs` module
//! writes the initramfs that puts the guest agent in place of the managed agent, where
//! `runtime-adapter-guest init` serves the VM's control tty.
//!
//! The host program also launches VMs with a layered root, of an image over an upper, which it
//! stages in the `staged` module, plans with NVX's typed plan for an adapter's own guest, which
//! also renders the VM's network, if any, and hands OpenVMM as the files that its checks of the
//! staged bindings opened, in the `layered` module, with the host directories and files that it
//! shares with the root, whose export the `sharing` module plans with NVX's `filesystem::plan`. Its
//! step hook, in the `hook` module, lets tests pause a launch, or end it as a crash would, at a
//! named step. In those VMs, the guest agent's PID 1 starts a worker, in the `worker` module, which
//! assembles the root through `nvx::guest::root` outside PID 1, as the host's root configuration of
//! the [`root`] module asks, applies the host's mapping table from the host export, and runs the
//! [`probe`]s with which the host checks the root. The host runs that order, in the `host` module:
//! it checks that the guest agent that assembled the root is of the build that it ships, sends the
//! table, then starts the sandbox, and tears down a VM whose order failed. Once the host started
//! the sandbox, the worker also runs several [`exec`]utions at once through NVX's execution engine
//! and its guest binding, from one loop that never blocks. The executions enter the root with the
//! publication of listeners that the root configuration names, and the host gives them the URL of
//! the network's proxy, if any, in their environment.
//!
//! A VM whose kernel command line names the platform snapshot tier is a template's, which the
//! worker captures before it assembles any root, so that every clone of the snapshot attaches a
//! root of its own, in [`template`] mode. The host program's `runtime-adapter-host create`
//! captures such a VM through NVX's template store, which publishes the snapshot as a generation
//! of a template, in the `capture` module. In each clone, the worker repairs the restore, then
//! activates the clone with the host's `Activate`, whose record the guest library applies, answers
//! `Ready` with the activation, and serves the host's order as after a cold boot. On the host, the
//! `host` module's clone order is the framing through which the restore sequence of
//! `nvx::host::template` activates a clone and starts its sandbox, and the host program's
//! `runtime-adapter-host start` starts a clone of a generation in a sandbox of its own, which
//! leases the generation, in the `clone` module. Its `run` runs one execution in such a clone,
//! then tears the clone down. The host program's `refresh` captures a new generation, `list` and
//! `inspect` report the store's generations, and `remove` retires one, in the `generations`
//! module, which also decides which generations the host program evicts.

#[cfg(feature = "host")]
pub mod capture;
#[cfg(feature = "host")]
pub mod clone;
pub mod exec;
pub mod frame;
#[cfg(feature = "host")]
pub mod generations;
#[cfg(all(feature = "guest", target_os = "linux"))]
pub mod guest;
#[cfg(feature = "host")]
pub mod hook;
#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "host")]
pub mod initramfs;
#[cfg(feature = "host")]
pub mod layered;
pub mod probe;
pub mod root;
#[cfg(feature = "host")]
pub mod runtime;
#[cfg(feature = "host")]
pub mod sharing;
#[cfg(feature = "host")]
pub mod staged;
pub mod template;
#[cfg(all(feature = "guest", target_os = "linux"))]
pub mod worker;

/// Command that starts the guest agent on the runtime adapter's standard streams.
pub const GUEST_SERVE_COMMAND: &str = "serve";

/// Command that starts the guest agent on the tty that follows it, in place of NVX's managed agent,
/// or, with [`GUEST_LAYERED_MODE`] after the tty, PID 1 of a VM with a layered root.
pub const GUEST_INIT_COMMAND: &str = "init";

/// The mode of [`GUEST_INIT_COMMAND`] that runs PID 1 of a VM with a layered root, which starts the
/// worker.
pub const GUEST_LAYERED_MODE: &str = "layered";

/// Command that starts the worker of a VM with a layered root on the tty that follows it, with the
/// process ID of PID 1's unrelated child after the tty.
pub const GUEST_WORKER_COMMAND: &str = "worker";

/// Command that starts PID 1's unrelated child in a VM with a layered root, which sleeps until a
/// signal ends it.
pub const GUEST_IDLE_COMMAND: &str = "idle";

/// Command that runs the executions of the worker's `entries` probe in a mount namespace of its
/// own, with the root and the workload cgroup that follow it; see `worker::entries`.
pub const GUEST_ENTRIES_COMMAND: &str = "entries";

/// Command that an execution of the worker's `entries` probe runs inside the root, with its case's
/// directory and the number of the descriptor that its caller leaked to it, and that reports what
/// the execution sees and may do; see `worker::entries`.
pub const GUEST_INSIDE_COMMAND: &str = "inside";

/// The guest agent's build: this crate's version. The guest agent reports it once it assembled a
/// layered root, in its [`Ready`](root::Ready), and a host program ships it unless the host names
/// another build.
pub const GUEST_BUILD: &str = env!("CARGO_PKG_VERSION");

/// Describes the runtime contract that this build speaks: the control contract revision, and the
/// versions of the microVM ABI, the NVXS records, and the NVXC frames.
pub fn contract() -> String {
    format!(
        "control contract {} (microVM ABI {}, NVXS v{}, NVXC v{})",
        nvx::contract::CONTROL_CONTRACT_REVISION,
        nvx::contract::MICROVM_ABI_VERSION,
        nvx::protocol::OUTER_VERSION,
        nvx::protocol::APP_VERSION,
    )
}
