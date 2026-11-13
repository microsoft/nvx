//! The host's side of the library: a VM's runtime and configuration, its launch in a private
//! directory that its caller owns, for a cold boot, a capture, or a restore, its stop and its
//! reattachment, its snapshots and their restores, and verified connections to it.
//!
//! - [`runtime`] holds what every launch shares: the paths of the OpenVMM executable and the
//!   guest kernel and initramfs, and their [`RuntimeIdentity`](runtime::RuntimeIdentity), the
//!   [`Hypervisor`](runtime::Hypervisor), which [`probe_hypervisor`](runtime::probe_hypervisor)
//!   checks, and the [`LaunchOptions`](runtime::LaunchOptions).
//! - [`config`] holds the configuration of a VM as one value, a [`VmConfig`](config::VmConfig),
//!   which it checks without reading the host: the VM's processors and memory, its root, the view
//!   that its workload has of it, its network, its export, its boot console, and what only its
//!   cold boot takes, with what a capture of it adds, a [`CaptureConfig`](config::CaptureConfig),
//!   whose identity names the capture's snapshots.
//! - [`vm`] holds a caller-owned VM directory, private to the current user, the fixed names of
//!   its files and endpoints, and a checked cold, capture or restore [`Prepared`](vm::Prepared)
//!   launch with its opened log and control credential, which the library never writes. A
//!   restore from a [`Snapshot`](snapshot::Snapshot) hard-links the snapshot's OpenVMM files into
//!   the VM directory and binds its restore-ready listener before any spawn. [`vm::spawn`]
//!   starts OpenVMM, detached, [`vm::stop`] ends a VM, and [`vm::verify`] and [`vm::reattach`]
//!   find one again from the [`SavedVm`](vm::SavedVm) that its caller saved.
//! - [`snapshot`] holds the strict snapshot directory, [`Snapshot`](snapshot::Snapshot), the
//!   finish of a capture, [`finish_capture`](snapshot::finish_capture), the compatibility key,
//!   [`SnapshotKey`](snapshot::SnapshotKey), derived from a capture's context and
//!   [`SnapshotManifest`](snapshot::SnapshotManifest), the strict manifest reader, and the
//!   capture's public time and eligibility information.
//! - [`restore`] orders caller-driven activation to Ready without I/O or a clock, and listens for
//!   OpenVMM's restore-ready event from the held VM process.
//! - [`transport`] verifies a VM's endpoint and runs the host's role of the NVXS session for it,
//!   for any application protocol, over a platform transport. [`nvxs`] retains compatibility
//!   paths to it.
//! - [`process`] names processes by verified references: their ID, start time, and host boot,
//!   through which a pidfd or a process handle holds them, so that waits sleep until a process
//!   exits and termination reaches only it, whether or not the caller started it.
//! - [`report`] reads OpenVMM's outcome report.
//! - [`network`] lowers a network posture and its egress rules into OpenVMM's options, and
//!   [`filesystem`] checks and lowers the export of host paths through OpenVMM's aggregate
//!   virtio-fs device.
//!
//! The library keeps no record of VMs, launches, captures, or snapshots, and takes no lock that
//! its caller sees. It starts OpenVMM, in [`vm::spawn`], and no other process, and supervises no
//! VM: it ends a process on its own only when its spawn fails. The caller saves what it needs to
//! find its VMs again, orders its own operations on each VM and each snapshot, and stops each VM
//! that it launched, with [`vm::stop`], or keeps its reference to stop it later. Linux and
//! Windows hosts run VMs; other hosts compile the module but report NVX as unavailable.
//!
//! The children of `host` depend on one another without cycles: each builds only on children
//! beneath it, as [`vm`] does on the private `spawn`, which starts a VMM, detached, passing it
//! only the descriptors or handles that the caller allows, and `spawn` on the private
//! `platform`, which holds the primitives of Linux and Windows hosts. An item that a lower child
//! needs from a higher one moves down, and the higher child re-exports it where it was.
//! `tests/host_modules.rs` reads the crate's sources and fails on a cycle, which it lists with a
//! reference for each of its dependencies.
//!
//! The host modules deny unsafe code. On Unix, the private `sys` module confines the system calls
//! that the standard library lacks behind documented safe functions, which call `nix`. The
//! Windows platform module and the Windows module of the private `seal` still call `windows-sys`
//! directly, and the CLI's identification of the host CPU executes `CPUID` through its
//! `core::arch` intrinsic, which the crate's minimum Rust version still marks unsafe.
#![deny(unsafe_code)]

pub(crate) mod artifacts;
#[cfg(feature = "cli")]
#[doc(hidden)]
pub mod cli;
pub mod config;
pub mod filesystem;
pub mod network;
pub mod nvxs;
pub(crate) mod paths;
pub(crate) mod platform;
pub mod process;
pub mod report;
pub mod restore;
#[cfg(feature = "cli")]
mod restore_driver;
pub mod runtime;
pub(crate) mod sandbox;
pub(crate) mod seal;
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
