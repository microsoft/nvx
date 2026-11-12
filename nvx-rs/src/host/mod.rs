//! The host side of NVX.
//!
//! - [`artifacts`] locates the OpenVMM executable and the guest kernel and initramfs, and every
//!   artifact of the guests that `nvx.py` runs, in a repository checkout or a release.
//! - [`runtime`] holds what every launch shares: the paths of those three files and their
//!   [`RuntimeIdentity`](runtime::RuntimeIdentity), the hypervisor, which
//!   [`probe_hypervisor`](runtime::probe_hypervisor) checks, and the
//!   [`LaunchOptions`](runtime::LaunchOptions).
//! - [`config`] holds the configuration of a VM as one value, a [`VmConfig`](config::VmConfig),
//!   which it checks without reading the host: the VM's processors and memory, its root, the view
//!   that its workload has of it, its network, its export, its boot console, and what only its
//!   cold boot takes, with what a capture of it adds, a [`CaptureConfig`](config::CaptureConfig),
//!   whose identity names the capture's snapshots.
//! - [`network`] lowers egress rules, and the egress policy files that `nvx-rs run` and
//!   `nvx-rs sandbox` accept, into OpenVMM's rules, and [`filesystem`] plans the export of host
//!   paths through OpenVMM's aggregate virtio-fs device.
//! - [`report`] reads OpenVMM's outcome report.
//! - [`snapshot`] holds the compatibility key, [`SnapshotKey`](snapshot::SnapshotKey), derived
//!   from a capture's context and [`SnapshotManifest`](snapshot::SnapshotManifest),
//!   the strict manifest reader, and the capture's public time and eligibility information.
//! - [`transport`] verifies a VM's endpoint and runs the host's role of the NVXS session for it,
//!   for any application protocol, over a platform transport. The private `session` (feature
//!   `cli`) speaks NVX's own control protocol, `NVXC`, over it for NVX's programs. [`nvxs`]
//!   retains compatibility paths. [`platform`] holds the primitives of Linux and Windows hosts.
//!   Other hosts compile but report NVX as unavailable.
//! - [`process`] names processes by verified references: their ID, start time, and host boot,
//!   through which a pidfd or a process handle holds them, so that waits sleep until a process
//!   exits and termination reaches only it, whether or not the caller started it.
//! - [`sandbox`] is the launch profile of `nvx-rs sandbox`: a container over read-only EROFS
//!   layers and a private ext4 scratch disk, with live shares, and the checks of its options.
//!   [`sandbox::distro`] binds a distro layer to what explicit verification found, so that a
//!   launch checks the layer through [`seal`], which seals open files from their metadata alone,
//!   and [`sandbox::scratch`] binds and claims a scratch image.
//! - [`vm`] holds a caller-owned VM directory, private to the current user, the fixed names of
//!   its files and endpoints, and a checked cold, capture or restore [`Prepared`](vm::Prepared)
//!   launch with its opened log and control credential, which the library never writes. A
//!   restore from a [`Snapshot`](snapshot::Snapshot) hard-links the snapshot's OpenVMM files into
//!   the VM directory and binds its restore-ready listener before any spawn.
//! - [`restore`] orders caller-driven activation to Ready without I/O or a clock, and listens for
//!   OpenVMM's restore-ready event from the held VM process. The private `restore_driver` (feature
//!   `cli`) drives it over the private `session` for NVX's blocking callers, through Ready, the
//!   mapping table and the workload's start.
//! - `cli` (feature `cli`) is the `nvx-rs` command-line interface, which runs microVMs and
//!   sandboxes, looks up and downloads artifacts, and compiles egress policies.
//!
//! The private `paths` module makes host paths absolute and resolves their links, with
//! `std::path` and `std::fs`, for the sandbox launch profile and the CLI. The private `spawn`
//! module starts a VMM for [`vm`], detached, passing it only the descriptors or handles that the
//! caller allows, and no other descriptor or handle of the caller's.
//!
//! The children of `host` depend on one another without cycles: each builds only on children
//! beneath it, as [`vm`] does on `spawn`, and `spawn` on [`platform`]. An item that
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
#[doc(hidden)]
pub mod cli;
pub mod config;
pub mod filesystem;
pub mod network;
pub mod nvxs;
pub(crate) mod paths;
pub mod platform;
pub mod process;
pub mod report;
pub mod restore;
#[cfg(feature = "cli")]
mod restore_driver;
pub mod runtime;
pub mod sandbox;
pub mod seal;
#[cfg(feature = "cli")]
pub(crate) mod session;
pub mod snapshot;
mod spawn;
#[cfg(unix)]
#[allow(unsafe_code)]
mod sys;
#[cfg(test)]
pub(crate) mod tests;
pub mod transport;
pub mod vm;
mod vm_directory;
