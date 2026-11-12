//! The transitional public path of the low-level spawn of a VMM, which code that stays still uses
//! until SL6.3 (#1122): [`spawn`] and [`spawn_vm`], which start a VMM detached, the [`VmmCommand`]
//! that they start and the [`Launched`] VMM that they return, [`Inherited`], which allows the VMM
//! a descriptor (Linux) or handle (Windows) of the caller's, and [`spawn_command`], with which a
//! caller that spawns VMMs spawns its other processes.
//!
//! The module has no code of its own, and supervises nothing: it re-exports these items from the
//! crate's private spawn and platform modules. [`host::vm`](crate::host::vm) creates, spawns,
//! reattaches to, and stops a VM in a private directory that its caller owns. SL6.3 deletes
//! `spawn_command` and the checked-file handoff of
//! [`PreparedLaunch`](crate::host::sandbox::PreparedLaunch), and settles the launcher of the
//! `external-launch` example: the last users of this path, besides the tests of the spawn in
//! `tests/supervisor.rs`.
//!
//! # Inheritance
//!
//! The VMM receives its capability on its standard input, its log as its standard output and
//! error, and the descriptors (Linux) or handles (Windows) that the caller names in
//! [`VmmCommand::inherit`], under the numbers that they have in the caller, and nothing else of
//! the caller's. On Linux, every other descriptor stays close-on-exec, and only the VMM's own
//! process clears the flag of the allowed ones, so a process that another thread spawns meanwhile
//! inherits none of them. On Windows, the allowed handles join the explicit list of handles that
//! the VMM inherits, and stay inheritable only while it is created; see [`Inherited`].
//!
//! On Windows, the VMM's standard handles are inheritable while it is created too, and a process
//! that another thread of the caller creates in that window without a list of handles, as
//! `std::process::Command` creates one, inherits them all, and holds them until it exits: it could
//! read the VMM's capability before the VMM does, and it holds the VMM's log, and the handles that
//! the VMM is allowed. Each spawn of a VMM holds the crate's spawn lock throughout that window, so
//! a caller that launches VMMs spawns its other processes with [`spawn_command`], which takes the
//! same lock, and keeps them out of it. On Linux, the lock holds nothing.

pub use super::platform::inherit::{Inherited, spawn_command};
pub use super::spawn::{Launched, VmmCommand, spawn, spawn_vm};
