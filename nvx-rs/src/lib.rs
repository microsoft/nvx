//! NVX mechanics: the OpenVMM control protocol, and the launch, policy lowering, and supervision
//! of NVX microVMs.
//!
//! An NVX sandbox is a microVM that OpenVMM runs with the managed lifecycle: the guest's init
//! starts a managed agent, which runs workloads that the host sends it over an authenticated
//! control console. This crate holds everything it takes to drive one:
//!
//! - [`protocol`]: the codec of the control console and the `NVXS` session that OpenVMM brokers,
//!   which carries any application protocol, the guest targets that a sandbox's live shares may
//!   not take, and the exit statuses with which a clone's guest fails fast. NVX's own control
//!   protocol, `NVXC`, is private to the crate. It has no dependencies, so the host, the guest,
//!   and test doubles share it.
//! - [`activation`]: the activation record of a layered root, the bounded, versioned, non-secret
//!   record of the settings that its activation applies, which a clone restored from a template
//!   reads in place of its captured configuration, and what Ready reports about the activation.
//!   It needs no features, so the host, the guest, and every adapter's framing share it.
//! - [`contract`]: the control-contract versions that this crate speaks and the compatibility of
//!   NVX releases with them, and the versions of the contracts through which OpenVMM restores a
//!   snapshot.
//! - [`net`]: IP networks in CIDR notation and their subtraction, and the forms of a VM's
//!   hostname and guest network address.
//! - [`gpt`]: GUID Partition Table disks whose first partition configures an adapter and whose
//!   others hold the read-only layers of a root: a strict reader of their protective MBR,
//!   primary header, and partition entries, which reads only through a source that its caller
//!   gives, so that the guest and the host share it.
//! - [`uuid`]: UUIDs, such as the GUIDs of a GPT disk, in their canonical text, the one form that
//!   NVX writes and reads, and in the layout in which GPT stores them. It has no dependencies, so
//!   the host and the guest share it.
//! - [`Hypervisor`]: the hypervisors that run the VMs.
//! - [`host`] (feature `host`): artifact discovery, the OpenVMM command line, the lowering of
//!   network and filesystem policies, the host's NVXS connection, process and platform
//!   primitives, VMs that a caller creates, starts, reattaches to, and stops in private
//!   directories that it owns, and the manifests and compatibility keys of templates.
//! - `guest` (feature `guest`, Linux): the programs that run in NVX guests, such as
//!   `nvx-console-pending`, the guest's NVXS connection, the libraries through which any guest
//!   init or agent opens the terminal on which it serves its session (`guest::terminal`), starts
//!   a program in a new process (`guest::spawn`), has the program enter an execution's
//!   namespaces, view, root, and identity (`guest::entry`), collects the status of each child
//!   that it names (`guest::process`), places, kills, and settles the processes of its
//!   executions in cgroups (`guest::cgroup`), describes host mappings and keeps the account of a
//!   host's mapping table (`guest::mappings`), and drives each program as one execution, with its
//!   launch, bounded output, input, ending, and outcome (`guest::exec`), the typed calls through
//!   which it runs the time ABI's steps (`guest::time`) and reseeds after a restore
//!   (`guest::reseed`) in its own process,
//!   and the capture of a template's guest, which repairs its clones (`guest::snapshot`), and
//!   their activation (`guest::activation`). The crate's `nvx-*` guest binaries run the programs.
//!   NVX's own guest space, which assembles a layered root, reads GPT disks, mounts host mappings,
//!   and applies a clone's activation, is internal reference code: hidden, unstable, and
//!   crate-private once nothing outside NVX uses it.
//!
//! # Example
//!
//! A cold boot through [`host::vm`], in a private directory that the caller owns, and the VM's
//! stop. The `host-workflows` example also captures a VM, restores one, and finds one again after
//! its caller restarts.
//!
//! ```no_run
//! # #[cfg(feature = "host")]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use std::path::Path;
//! use std::time::{Duration, Instant};
//!
//! use nvx::activation::{MachineIdMode, View};
//! use nvx::host::config::{Boot, ConsoleAttachment, RootConfig, VmConfig};
//! use nvx::host::runtime::{LaunchOptions, Runtime};
//! use nvx::host::vm::{self, VmDir};
//! use nvx::{Artifacts, Hypervisor};
//!
//! // Locates OpenVMM, the guest kernel, and the Alpine initramfs, with the identity that the
//! // install metadata of their release or checkout gives them.
//! let artifacts = Artifacts::discover()?;
//! let runtime = Runtime {
//!     identity: artifacts.runtime_identity()?,
//!     openvmm: artifacts.openvmm,
//!     kernel: artifacts.kernel,
//!     initramfs: artifacts.initrd,
//!     hypervisor: Hypervisor::from_env_or_default()?,
//!     cpu_profile: None,
//!     windows_breakaway: false,
//!     options: LaunchOptions::default(),
//! };
//! let config = VmConfig {
//!     processors: 1,
//!     memory_mib: 256,
//!     root: RootConfig::Blockless,
//!     view: View {
//!         hostname: b"example".to_vec(),
//!         machine_id: MachineIdMode::Image,
//!     },
//!     network: None,
//!     filesystem: None,
//!     console: ConsoleAttachment::Disconnected,
//!     boot: Some(Boot {
//!         command_line: "quiet".to_owned(),
//!         memory_capacity_mib: None,
//!         capture: None,
//!     }),
//! };
//! // The caller creates and owns the VM's private directory.
//! let directory = Path::new("example-vm");
//! vm::create_private_dir(directory)?;
//! let dir = VmDir::open(directory)?;
//! // Creation checks the whole launch and creates its log, but starts nothing; the spawn starts
//! // OpenVMM detached, and returns its verified process.
//! let launched = vm::spawn(vm::create(&runtime, &config, None, &dir)?)?;
//! // The caller speaks its own protocol with its guest at `launched.outputs.endpoint`, with the
//! // secret `launched.capability`. To stop the VM, the callback would ask the guest in that
//! // protocol to end it, and return whether it sent the request; this one sends none, so the
//! // stop ends OpenVMM's verified process once the grace period has passed, unless it exited.
//! let grace = Instant::now() + Duration::from_secs(10);
//! let stopped = vm::stop(
//!     &launched.process,
//!     &launched.outputs,
//!     |_| false,
//!     grace,
//!     grace + Duration::from_secs(10),
//! )?;
//! println!("forced: {}, report: {:?}", stopped.forced, stopped.report?);
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "host"))]
//! # fn main() {}
//! ```
//!
//! # Features
//!
//! - `host` (default): the [`host`] module. It enables `serde`.
//! - `testing`: NVX's own `nvx-fake-openvmm` binary, a fake OpenVMM that serves the control
//!   console without running a VM. It adds no API.
//! - `cli`: NVX's own `nvx-rs` binary, the command-line interface that runs NVX's microVMs and
//!   sandboxes. It adds no API, and enables `host`.
//! - `serde`: the serde forms of [`Hypervisor`] and [`net::Ipv4Cidr`].
//! - `guest`: the `guest` module and the guest binaries, which the `guest` profile builds as
//!   static `x86_64-unknown-linux-musl` executables.

pub mod activation;
pub mod contract;
mod error;
pub mod gpt;
mod hypervisor;
pub mod net;
pub mod protocol;
pub mod uuid;

#[cfg(all(feature = "guest", target_os = "linux"))]
pub mod guest;
#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "testing")]
#[doc(hidden)]
pub mod testing;
#[cfg(test)]
mod tests;

pub use error::{Error, ErrorCode, ErrorKind, Result};
#[cfg(feature = "host")]
pub use host::artifacts::Artifacts;
#[cfg(feature = "host")]
pub use host::filesystem::{guest_path, resolve_guest_path};
pub use hypervisor::Hypervisor;
