//! The guest's side of the library, for Linux guests: the calls on which any guest init or agent
//! builds, in its own process.
//!
//! - [`nvxs`]: the guest's I/O driver of the NVXS session, and [`terminal`]: the terminal on
//!   which an agent serves that session, such as the VM's control console.
//! - [`mappings`]: host mappings, and the account of a host's mapping table, which hands each of
//!   the table's parts to the caller's own mapper.
//! - [`reseed`]: a restore's fresh entropy, which starts with the VM's new [`GenerationId`], and
//!   the reseed of the kernel's random number generator with it.
//! - On x86-64, `time`: the time ABI's cold-boot step and the start of its daemon, a process that
//!   the caller supervises, as typed calls.
//! - On x86-64, [`snapshot`]: a strictly checked Fresh capture by one caller deadline, without
//!   execution control, with nothing mounted from the devices that a snapshot's clones replace,
//!   which repairs each clone that a restore of the snapshot starts before it returns: it
//!   activates the restore's memory target, reseeds the kernel's random number generator, and
//!   finishes the restore, with its acknowledgement.
//! - On x86-64, `activation`: the activation through which each repaired clone then takes its own
//!   configuration from the host, in one Activate on a restricted control session.
//!
//! The library builds no space and runs no execution: the guest's space, the root that an init
//! assembles and what it mounts in it, and the executions that it runs are each init's own. The
//! [guest-agent guidelines] give an init agent's contract for its space, its executions, a
//! capture, and a restore, with nvx-init, NVX's PID 1 for layered roots, as the worked example;
//! the library enforces none of it. The contracts that every space reads stay public: the mapping
//! values and [`MappingTable`](mappings::MappingTable), and the tokens and records of
//! [`protocol`](crate::protocol) and [`activation`](crate::activation).
//!
//! [guest-agent guidelines]: https://github.com/microsoft/nvx/blob/dev/doc/guest-agent-guidelines.md
//!
//! The guidelines quote this excerpt, in which an init applies one part of the host's mapping
//! table through its own mapper, `mount`, and admits workloads only once the table is complete:
//!
//! ```no_run
//! use nvx::guest::mappings::{Mapping, MappingTable, TableError};
//!
//! fn apply_part<E>(
//!     table: &mut MappingTable,
//!     part: &[u8],
//!     mount: impl FnOnce(&[Mapping]) -> Result<(), (usize, E)>,
//! ) -> Result<bool, TableError<E>> {
//!     table.apply(part, mount)?;
//!     Ok(table.is_complete())
//! }
//! ```
//!
//! NVX's own guest programs, which ship as `/sbin/nvx-*`, among them nvx-init and NVX's managed
//! agent, live in this module, private to the crate, with nvx-init's space, its internal
//! reference code: each binary calls the hidden `main` of its program's module. The calls above
//! need none of NVX's control protocol, its managed agent, or its workload identity, and NVX's
//! programs run on them too, such as `nvx-time` on `time` and `nvx-reseed` on [`reseed`].
#![deny(unsafe_code)]
#![deny(clippy::print_stdout, clippy::print_stderr)]

mod account;
mod generation;
mod parse;
mod status;
#[allow(unsafe_code)]
mod sys;

#[cfg(target_arch = "x86_64")]
mod kernel_text;

pub use generation::GenerationId;

#[cfg(target_arch = "x86_64")]
pub mod activation;
/// Compatibility name for the guest's activation.
#[cfg(target_arch = "x86_64")]
pub use activation as clone;
#[doc(hidden)]
pub mod console_pending;
#[cfg(target_arch = "x86_64")]
#[doc(hidden)]
pub mod device_io;
#[cfg(target_arch = "x86_64")]
#[doc(hidden)]
pub mod exit;
pub(crate) mod gpt;
#[doc(hidden)]
pub mod ingress;
#[cfg(target_arch = "x86_64")]
#[doc(hidden)]
pub mod init;
#[cfg(target_arch = "x86_64")]
#[doc(hidden)]
pub mod managed_agent;
pub mod mappings;
#[cfg(target_arch = "x86_64")]
#[doc(hidden)]
pub mod mmio_write;
pub mod nvxs;
pub mod reseed;
pub(crate) mod root;
#[cfg(target_arch = "x86_64")]
pub mod snapshot;
pub mod terminal;
#[cfg(target_arch = "x86_64")]
pub mod time;
#[cfg(target_arch = "x86_64")]
#[doc(hidden)]
pub mod time_probe;
pub(crate) mod view;
