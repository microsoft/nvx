//! Mechanisms and value types with which a program runs NVX microVMs on OpenVMM: a VM's
//! configuration and its launch, in a directory that the program owns, for a cold boot, a
//! capture, or a restore; one snapshot type, with a strict directory format; a restore that the
//! program drives with its own I/O; verified connections to a running VM, and attachment to one
//! from the values that the program saved; the contracts that a guest's space reads; and the
//! guest's side of a capture and of a guest restore.
//!
//! The library keeps no durable state, runs no workload, and builds no guest space. On the host,
//! it starts OpenVMM, and no other process, and supervises no VM: the program stops each VM that
//! it launched, or keeps its reference to stop it later. Whatever a deployment needs beyond the
//! mechanisms, the program owns: the runtime's files and the images that it launches, the
//! records with which it finds its VMs and snapshots again, its locks, its protocol with its
//! guest, its deadlines, and its recovery, as the [library guide] lists under
//! [What the caller owns], beside the cold-boot, restore, and reattach workflows. In the guest,
//! each init owns its space and its executions, as the [guest-agent guidelines] describe.
//!
//! - [`protocol`]: the `NVXS` session that OpenVMM brokers between a host and its guest, which
//!   carries any application protocol; the host's mapping table on the wire; the kernel
//!   command-line tokens of a guest's root, view, and export; the guest targets that live shares
//!   may not take; and the exit statuses with which a clone's guest fails fast. It has no
//!   dependencies, so the host and the guest share it.
//! - [`guest_restore`]: the guest-restore record of a layered root, the bounded, versioned,
//!   non-secret record of the settings that its guest restore applies, which a clone restored
//!   from a snapshot reads in place of its captured configuration, and what Ready reports about
//!   the guest restore. It needs no features, so the host, the guest, and every adapter's framing
//!   share it.
//! - [`net`]: IP networks in CIDR notation and their subtraction, and the forms of a VM's
//!   hostname and guest network address.
//! - [`uuid`]: UUIDs, such as the GUIDs of a GPT disk, in their canonical text, the one form that
//!   NVX writes and reads, and in the layout in which GPT stores them. It has no dependencies, so
//!   the host and the guest share it.
//! - [`Error`]: the crate's error, whose [`ErrorKind`]s callers act on.
//! - [`host`] (feature `host`): a VM's runtime, with the
//!   [`Hypervisor`](host::runtime::Hypervisor) that runs it, and its configuration; its launch,
//!   stop, and attachment in a private directory that its caller owns; snapshots, their
//!   capture's finish, and the restores that their caller drives; the lowering of network and
//!   filesystem policies; and a VM's authenticated control connection and boot console,
//!   verified process references, and OpenVMM's outcome reports.
//! - `guest` (feature `guest`, Linux): the guest's side, for any guest init or agent: its NVXS
//!   connection and the terminal on which it serves it (`guest::nvxs`, `guest::terminal`), the
//!   account of a host's mapping table (`guest::mappings`), the time ABI's steps in its own
//!   process (`guest::time`), a capture that repairs each restore of its snapshot
//!   (`guest::snapshot`), a clone's guest restore (`guest::restore`), and its reseed
//!   (`guest::reseed`).
//!
//! NVX's own programs, its `nvx-rs` command-line interface, its guest binaries, and the fake
//! OpenVMM of its tests, build on the library and live in this crate, private to it, with the
//! code that only they use, such as NVX's own control protocol. Each binary calls an entry point
//! that `#[doc(hidden)]` hides, which is no part of the API.
//!
//! GPT parsing is internal to NVX's guest programs, not a public image-checking API. The control
//! plane owns producing, checking, and staging its images; each guest init owns interpreting
//! them. [`uuid`], [`protocol::root::ImageFormat::Gpt`], and [`guest_restore::Image::Gpt`] remain
//! public, independently of the parser.
//!
//! ```compile_fail
//! use nvx::gpt::Disk;
//! ```
//!
//! [library guide]: https://github.com/microsoft/nvx/blob/dev/doc/library.md
//! [What the caller owns]: https://github.com/microsoft/nvx/blob/dev/doc/library.md#what-the-caller-owns
//! [guest-agent guidelines]: https://github.com/microsoft/nvx/blob/dev/doc/guest-agent-guidelines.md
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
//! use nvx::host::config::{Boot, ConsoleAttachment, RootConfig, VmConfig};
//! use nvx::host::runtime::{
//!     ArtifactIdentity, CompatibilityIdentity, Hypervisor, LaunchOptions, Runtime,
//!     RuntimeIdentity,
//! };
//! use nvx::host::vm::{self, VmDir};
//! use nvx::protocol::view::{MachineIdMode, View};
//!
//! // The caller locates OpenVMM, the guest kernel, and the Alpine initramfs, and identifies each
//! // by the SHA-256 digest that its install metadata records, such as the `SHA256SUMS` of an NVX
//! // release: here, from the command line. It names its own contracts with the guest in a
//! // compatibility identity, which it changes whenever it can no longer drive the guests that it
//! // started under the earlier one.
//! let arguments: Vec<String> = std::env::args().skip(1).collect();
//! let [openvmm, kernel, initramfs, openvmm_sha256, kernel_sha256, initramfs_sha256] =
//!     <[String; 6]>::try_from(arguments).map_err(|_| "expected three paths and three digests")?;
//! let runtime = Runtime {
//!     identity: RuntimeIdentity::new(
//!         ArtifactIdentity::sha256(&openvmm_sha256)?,
//!         ArtifactIdentity::sha256(&kernel_sha256)?,
//!         ArtifactIdentity::sha256(&initramfs_sha256)?,
//!         CompatibilityIdentity::new("example-v1")?,
//!     ),
//!     openvmm: openvmm.into(),
//!     kernel: kernel.into(),
//!     initramfs: initramfs.into(),
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
//! - `serde`: the serde form of [`net::Ipv4Cidr`].
//! - `guest`: the `guest` module and the guest binaries, which the `guest` profile builds as
//!   static `x86_64-unknown-linux-musl` executables.

mod error;
#[cfg(any(test, all(feature = "guest", target_os = "linux")))]
mod gpt;
pub mod guest_restore;
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

#[cfg(feature = "host")]
pub(crate) use error::ErrorCode;
pub use error::{Error, ErrorKind, Result};
#[cfg(feature = "host")]
pub use host::filesystem::guest_path;
