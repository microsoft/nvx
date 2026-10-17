//! The host side of NVX.
//!
//! - [`artifacts`] locates the OpenVMM executable and the guest kernel and initramfs, and every
//!   artifact of the guests that `nvx.py` runs, in a repository checkout or a release.
//! - [`launch`] builds OpenVMM's command line from a [`LaunchSpec`](launch::LaunchSpec): the
//!   managed lifecycle, the workload identity, the kernel command line, and the network and
//!   filesystem options.
//! - [`network`] lowers egress rules, and the egress policy files of `nvx.py`, into OpenVMM's
//!   rules, and [`filesystem`] plans the export of host paths through OpenVMM's aggregate
//!   virtio-fs device.
//! - [`instance`] supervises one VM instance in a directory of its own: it starts OpenVMM and
//!   waits until the guest is ready, reconciles recorded state with the host, runs workloads,
//!   and stops the VM. [`report`] reads OpenVMM's outcome report.
//! - [`session`] speaks the authenticated control protocol, [`process`] launches and terminates
//!   OpenVMM processes, and [`platform`] holds the primitives of Linux and Windows hosts. Other
//!   hosts compile but report NVX as unavailable.
//! - [`sandbox`] is the launch profile of `nvx.py sandbox`: a container over read-only EROFS
//!   layers and a private ext4 scratch disk, with live shares, and the checks of its options.
//!
//! The ports of `nvx.py` code keep its Python semantics, which the private `python` module
//! provides.
//!
//! The host modules deny unsafe code. On Unix, the private `sys` module confines the system calls
//! that the standard library lacks behind documented safe functions, which call `nix`. The
//! Windows platform module, and the module of the Win32 calls that Python's filesystem queries
//! make, still call `windows-sys` directly.
#![deny(unsafe_code)]

pub mod artifacts;
pub mod filesystem;
pub mod instance;
pub mod launch;
pub mod network;
pub mod platform;
pub mod process;
pub(crate) mod python;
pub mod report;
pub mod sandbox;
pub mod session;
#[cfg(unix)]
#[allow(unsafe_code)]
mod sys;
