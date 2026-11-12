//! The programs that run in NVX's Linux guests, which ship as `/sbin/nvx-*`, and the libraries
//! on which any guest init or agent builds: [`nvxs`], the guest's I/O driver of the NVXS session;
//! [`terminal`], the terminal on which it serves that session; [`spawn`], through which it starts
//! a program; [`entry`], through which a program that it starts enters an execution's
//! namespaces, view, root, and identity; [`process`], through which it collects the status of
//! each child that it names, and of no other; [`cgroup`], through which it places, accounts,
//! kills, and settles the processes of its executions; [`mappings`], the host mappings that it
//! mounts, and its account of a host's mapping table; [`exec`], which drives each program as one
//! execution, with its launch, its bounded output and input, its ending, and its outcome; and,
//! on x86-64, [`snapshot`], which composes a strictly checked Fresh capture by one caller deadline,
//! without execution control, with nothing mounted from the devices that a template's clones
//! replace, and which repairs each clone that a restore of the snapshot starts before it returns:
//! it activates the restore's memory target, reseeds the kernel's random number generator, and
//! finishes the restore, with its acknowledgement; and `activation`, through which each repaired
//! clone then takes its own configuration from the host, in one Activate on a restricted control
//! session.
//!
//! The [guest-agent guidelines] give an init agent's contract for its space, its executions, a
//! capture, and a restore, with nvx-init as the worked example; the library enforces none of it.
//!
//! [guest-agent guidelines]: https://github.com/microsoft/nvx/blob/dev/doc/guest-agent-guidelines.md
//!
//! The guest's space, the root that an init assembles and what it mounts in it, is each init's
//! own. NVX's own space is internal reference code, which NVX's guest programs use: it is hidden
//! from this documentation and unstable, and it becomes crate-private once nothing outside NVX
//! uses it. It is `root`, which bootstraps the guest for a layered root, assembles the root,
//! mounts live shares and mappings in it, and tears it down; `view`, which sets the VM's hostname,
//! machine ID, and resolver file, whose binds the root owns; `gpt`, which reads the partition
//! table of a GPT disk that the kernel attached as a block device; the mapper of `mappings`, which
//! mounts host mappings beneath the descriptors of the roots that it names; and the activation's
//! application, `activation::apply` and its types, with which a clone assembles its root and
//! applies its VM-wide view without starting a process. The contracts that every space reads
//! stay public: the mapping values and [`MappingTable`](mappings::MappingTable), and the tokens
//! and records of [`protocol`](crate::protocol) and [`activation`](crate::activation).
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
//! Each other module holds one program, and the crate's binary of the same name runs its `main`.
//! Two of them also offer their steps as typed calls, which any guest init or agent runs in its
//! own process and their programs run on: on x86-64, `time`'s `TimeAbi` runs the time ABI's
//! cold-boot step and starts its daemon, while [`snapshot`] runs its capture and restore finish,
//! and [`reseed`] reseeds the kernel's random number generator with a restore's fresh entropy,
//! which starts with the new [`GenerationId`]. The
//! program of the hidden `ingress` module, the announcer of the guest's listeners, offers no call:
//! activation reports the publication and gateway to its caller, which runs the program, or has
//! `nvx-ingress start` run it from a socket that it binds for it. The
//! libraries need none of NVX's control protocol, managed agent, or workload identity, and NVX's
//! managed agent opens its control terminal, applies its host mapping table, and supervises its
//! workloads through them. On x86-64, the hidden `init` module holds
//! `nvx-init`, a PID 1 for layered roots, which takes PID 1 over from their early boot with the
//! guest's helpers, the time daemon and the announcer: it runs with them ahead of the workloads,
//! collects every child's status, and ends the VM after `SIGTERM` or once either helper ends.
//!
//! The programs keep the command lines, exit statuses, and output of the C programs that they
//! replace, except that numeric arguments are plain decimal, with a `-` sign where `nvx-time`'s
//! test hooks take negative numbers, or `0x`-prefixed hexadecimal where the C program also read
//! hexadecimal, floating-point arguments plain decimal with an optional exponent, error messages
//! describe `errno` values with `nix`'s text, and the programs read the kernel's text, such as
//! its socket tables, sysctls, rates, CPU lists, and timer list, their own records, and their
//! test fixtures only in the form in which they are written, rejecting the signed, prefixed, and
//! other lenient forms that C's `strtoul`, `strtod`, and `scanf` also read. Nor do they keep
//! what only the C programs' fixed buffers and string handling did to oversized or malformed
//! input, such as a length limit on a valid CPU list, an unknown option that a later valid one
//! masked, or a NUL byte that ended a diagnostic.
//!
//! # Conventions
//!
//! - Program and library modules deny unsafe code. Only the private `sys` module may use it,
//!   behind documented safe operations. It makes every system call through `nix`, with the
//!   enabled, default-free feature set, or through the standard library, which reads and writes
//!   whole files and the MSR devices, except the few spawn, managed-agent, and time-tool
//!   operations that neither provides, which `sys` documents, and the programs take random bytes
//!   from the `getrandom` crate. `sys` also names the kernel's and the processor's interfaces
//!   once: the procfs, sysfs, and device paths, OpenVMM's I/O ports, CPUID leaves and MSRs, and
//!   the kernel's flags, so that the programs refer to them by name.
//! - Descriptors and mappings have owners that release them when dropped. Where a C program
//!   reported a cleanup failure, a consuming `close` or `unmap` returns it instead. Raw
//!   descriptor integers never leave `sys`, and no resource is dropped by hand: where a C
//!   program released one before a later step, its scope ends there, for example in a helper
//!   function that returns what the later step needs. State that a program changes for a while,
//!   such as a lock, a signal mask, a thread's CPU affinity or scheduling policy, a mount, a
//!   created file, or a kernel setting, likewise has an owner that restores it when dropped.
//! - A descriptor stays with the library that opened it: one that `sys` opens through `nix` is
//!   read and written only through `sys`, and one that the standard library opens only through
//!   it, except that a test may hand one to `FileDescriptor::from_owned`, which takes it over
//!   whole. No descriptor thus has two owners or two buffers; a standard-library adapter such as
//!   `BufReader` may add the one buffer of a `sys` descriptor through the `std::io::Read` that
//!   `FileDescriptor` implements, and a caller that takes `std::io::Write`, such as [`nvxs`],
//!   writes one through the `std::io::Write` that it implements, which buffers nothing.
//!   `sys::Stdout` and `sys::DirectStdout` write standard output directly, past the standard
//!   library's line buffer, so the programs never print with the standard library's macros:
//!   `print!` would reorder its buffered output with theirs, and `eprint!` would bypass the error
//!   streams that the programs' tests capture. Clippy denies both here.
//! - Every program fails with the shared `CommandError`, whose kind selects the program's exit
//!   status from the shared `ExitStatus`. Every module, type, field, variant, constant, static,
//!   and function is documented, which CI enforces with `clippy::missing_docs_in_private_items`.
//! - The programs use `nix`, `getrandom`, or the standard library rather than reimplement a
//!   facility, and keep a parser of their own only for forms that no library parses.
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
pub mod cgroup;
/// Compatibility name for the guest's activation.
#[cfg(target_arch = "x86_64")]
pub use activation as clone;
#[doc(hidden)]
pub mod console_pending;
#[cfg(target_arch = "x86_64")]
#[doc(hidden)]
pub mod device_io;
pub mod entry;
pub mod exec;
#[cfg(target_arch = "x86_64")]
#[doc(hidden)]
pub mod exit;
#[doc(hidden)]
pub mod gpt;
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
pub mod process;
pub mod reseed;
#[doc(hidden)]
pub mod root;
#[cfg(target_arch = "x86_64")]
pub mod snapshot;
pub mod spawn;
pub mod terminal;
#[cfg(target_arch = "x86_64")]
pub mod time;
#[cfg(target_arch = "x86_64")]
#[doc(hidden)]
pub mod time_probe;
#[doc(hidden)]
pub mod view;
