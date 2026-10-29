//! The host side of NVX.
//!
//! - [`artifacts`] locates the OpenVMM executable and the guest kernel and initramfs, and every
//!   artifact of the guests that `nvx.py` runs, in a repository checkout or a release.
//! - [`launch`] builds OpenVMM's command line from a [`LaunchSpec`](launch::LaunchSpec): the
//!   managed lifecycle, the workload identity, the kernel command line, and the network and
//!   filesystem options.
//! - [`network`] lowers egress rules, and the egress policy files that `nvx-rs run` and
//!   `nvx-rs sandbox` accept, into OpenVMM's rules, and [`filesystem`] plans the export of host
//!   paths through OpenVMM's aggregate virtio-fs device.
//! - [`instance`] drives one VM instance in a directory of its own, the [`store`] of its VM,
//!   through the [`supervisor`] and the [`session`]: it starts OpenVMM and waits until the guest
//!   is ready, reattaches to the VM after a restart, runs workloads, and stops the VM.
//!   [`report`] reads OpenVMM's outcome report.
//! - [`store`] keeps the runtime records of a sandbox in a private directory with one lock: the
//!   runtime record of its VM, with the VM's runtime identity, and the journal of its launch,
//!   through which an interrupted launch is recovered without signaling an unverified process.
//! - [`supervisor`] launches a sandbox's VMM through the store's journal, passing it only the
//!   descriptors or handles that the caller allows, stops it through the caller's graceful step,
//!   with a bounded forced step on request, and reattaches to it after the orchestrator restarts
//!   within the same host boot. The private `spawn` module, whose items `supervisor` re-exports,
//!   starts the VMM detached, with no other descriptor or handle of the caller's.
//! - [`template`] holds the metadata of templates, captures from which clones are restored: the
//!   compatibility key of a template, [`TemplateKey`](template::TemplateKey), which it derives
//!   from the [`CaptureClass`](template::CaptureClass) of the capture and the capture's
//!   [`TemplateManifest`](template::TemplateManifest), the [`Store`](template::Store) of the
//!   generations of templates that a host published, which leases them to clones, and the plan
//!   of a template's capture, [`TemplatePlan`](template::TemplatePlan), which it lowers from an
//!   adapter's plan.
//! - [`session`] speaks NVX's control protocol, `NVXC`, over [`nvxs`], which runs the host's role
//!   of the NVXS session for it and for any other application protocol, over a platform
//!   transport. [`platform`] holds the primitives of Linux and Windows hosts. Other hosts compile
//!   but report NVX as unavailable.
//! - [`process`] names processes by verified references: their ID, start time, and host boot,
//!   through which a pidfd or a process handle holds them, so that waits sleep until a process
//!   exits and termination reaches only it, whether or not the caller started it.
//! - [`sandbox`] is the launch profile of `nvx-rs sandbox`: a container over read-only EROFS
//!   layers and a private ext4 scratch disk, with live shares, and the checks of its options.
//!   [`sandbox::distro`] binds a distro layer to what explicit verification found, so that a
//!   launch checks the layer through [`seal`], which seals open files from their metadata alone,
//!   and [`sandbox::scratch`] binds and claims a scratch image. A
//!   [`PreparedLaunch`](sandbox::PreparedLaunch) hands the VMM the files that those checks
//!   opened, which it inherits through the [`supervisor`]'s allowlist.
//! - `cli` (feature `cli`) is the `nvx-rs` command-line interface, which runs microVMs and
//!   sandboxes, looks up and downloads artifacts, and compiles egress policies.
//!
//! The private `paths` module makes host paths absolute and resolves their links, with
//! `std::path` and `std::fs`, for the sandbox launch profile and the CLI.
//!
//! The children of `host` depend on one another without cycles: each builds only on children
//! beneath it, as the [`supervisor`] does on the [`store`], and the store on `spawn`. An item that
//! a lower child needs from a higher one moves down, and the higher child re-exports it where it
//! was. `tests/host_modules.rs` reads the crate's sources and fails on a cycle, which it lists
//! with a reference for each of its dependencies.
//!
//! The host modules deny unsafe code. On Unix, the private `sys` module confines the system calls
//! that the standard library lacks behind documented safe functions, which call `nix`. The
//! Windows platform module and the Windows module of [`seal`] still call `windows-sys` directly,
//! and the CLI's identification of the host CPU executes `CPUID` through its `core::arch`
//! intrinsic, which the crate's minimum Rust version still marks unsafe.
#![deny(unsafe_code)]

pub mod artifacts;
#[cfg(feature = "cli")]
pub mod cli;
pub mod filesystem;
pub mod instance;
pub mod launch;
pub mod network;
pub mod nvxs;
pub(crate) mod paths;
pub mod platform;
pub mod process;
pub mod report;
pub mod sandbox;
pub mod seal;
pub mod session;
mod spawn;
pub mod store;
pub mod supervisor;
#[cfg(unix)]
#[allow(unsafe_code)]
mod sys;
pub mod template;
#[cfg(test)]
mod test_support;
