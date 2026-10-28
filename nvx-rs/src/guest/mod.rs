//! The programs that run in NVX's Linux guests, which ship as `/sbin/nvx-*`, and the libraries
//! on which any guest init or agent builds: [`nvxs`], the guest's I/O driver of the NVXS session;
//! [`terminal`], the terminal on which it serves that session; [`spawn`], through which it starts
//! a program; [`entry`], through which a program that it starts enters an execution's
//! namespaces, view, root, and identity; [`process`], through which it collects the status of
//! each child that it names, and of no other; [`cgroup`], through which it places, accounts,
//! kills, and settles the processes of its executions; [`mappings`], through which it mounts host
//! mappings beneath the descriptors of the roots that it names; [`exec`], which drives each
//! program as one execution, with its launch, its bounded output and input, its ending, and its
//! outcome, alone or, through its [`binding`](exec::binding), under the
//! [execution engine](crate::exec); [`root`], through which it bootstraps the guest for a layered
//! root, assembles the root, mounts live shares and mappings in it, and tears it down;
//! [`gpt`], through which it reads the partition table of a GPT disk that the kernel attached as
//! a block device; and, on x86-64, `clone`, through which a template's guest prepares for and
//! requests a snapshot's capture before its root's assembly, behind the caller's fence of
//! workload admission, with nothing mounted from the devices that its clones replace, and
//! through which each clone that a restore of the snapshot starts repairs its restore: it
//! activates the restore's memory target, reseeds the kernel's random number generator, and
//! finishes the restore, with its acknowledgement, and then takes its own configuration from the
//! host, in one Activate on a restricted control session, with which it assembles its root and
//! checks its workload identity before it admits any execution.
//!
//! Each other module holds one program, and the crate's binary of the same name runs its `main`.
//! Two of them also offer their steps as typed calls, which any guest init or agent runs in its
//! own process and their programs run on: on x86-64, `time`'s `TimeAbi` runs the time ABI's
//! cold-boot step, capture, and restore finish, and [`reseed`] reseeds the kernel's random number
//! generator with a restore's fresh entropy, which starts with the new [`GenerationId`]. And
//! [`ingress`] starts its program, the announcer of the guest's listeners, from a socket that it
//! binds for it, as a clone's application does. The
//! libraries need none of NVX's control protocol, managed agent, or workload identity, and NVX's
//! managed agent opens its control terminal, applies its host mapping table, and starts, drives,
//! collects, and settles its workloads through them.
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

mod generation;
mod parse;
mod status;
#[allow(unsafe_code)]
mod sys;

#[cfg(target_arch = "x86_64")]
mod kernel_text;

pub use generation::GenerationId;

pub mod cgroup;
#[cfg(target_arch = "x86_64")]
pub mod clone;
pub mod console_pending;
#[cfg(target_arch = "x86_64")]
pub mod device_io;
pub mod entry;
pub mod exec;
#[cfg(target_arch = "x86_64")]
pub mod exit;
pub mod gpt;
pub mod ingress;
pub mod managed_agent;
pub mod mappings;
#[cfg(target_arch = "x86_64")]
pub mod mmio_write;
pub mod nvxs;
pub mod process;
pub mod reseed;
pub mod root;
pub mod spawn;
pub mod terminal;
#[cfg(target_arch = "x86_64")]
pub mod time;
#[cfg(target_arch = "x86_64")]
pub mod time_probe;
