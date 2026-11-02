//! Safe operating-system interface shared by the guest programs.
//!
//! Guest program modules deny unsafe code. This module is their only boundary to the operating
//! system, through `nix` and, for whole files and the MSR devices, the standard library: it owns
//! descriptors and mappings, and its submodules confine the few unsafe `nix` operations, such as
//! ioctls, memory mappings, signal dispositions, and `fork`, behind wrappers that establish their
//! pointer, lifetime, and ownership invariants. Errors are `nix`'s [`Errno`], whose descriptions
//! the programs print. It also builds and decodes the kernel's binary interfaces that `nix` does
//! not model, such as the netlink messages of socket diagnostics, as explicit byte layouts, so
//! that the programs handle only typed values, and erases secrets, such as a restore's seed,
//! with the volatile writes of `zeroize`, which the compiler cannot drop as dead stores.
//!
//! A descriptor that this module opens keeps one owner, its wrapper here, such as
//! `FileDescriptor`, and the programs read and write it only through that wrapper, never through
//! a second owner, such as a `std::fs::File` made from its number. `FileDescriptor` implements
//! `std::io::Read` with one `read` system call per call, as `std::fs::File` does, so that a
//! standard-library adapter such as `BufReader` can hold it and add the only buffer in front of
//! it. Standard output and standard error belong to the process: `Stdout`, `DirectStdout`,
//! `write_standard_error`, and `write_standard_error_without_sigpipe` borrow them from the
//! standard library's handles only to write them directly, so the standard library's line buffer
//! for standard output must stay empty, which the guest module ensures by denying the standard
//! library's print macros. Standard input belongs to the process too, and
//! `discard_standard_input` borrows it only to read it directly.
//!
//! The spawn boundary and the managed agent need a few operations that `nix` cannot provide,
//! which reach libc through `nix`'s re-export: a spawned child, which may neither allocate nor
//! drop an owner, fills its preallocated `PWD` entry with the `getcwd` system call, closes the
//! descriptors that it does not use with the `close_range` system call, and executes and exits
//! through libc; an execution's entry starts its child with `clone3`, and the child changes its
//! credentials, capabilities, hostname, and umask, and mounts its view and enters its working
//! directory with the new mount API and `openat2`, through libc's `syscall`, as the entry's
//! module describes, because libc does not know of that child; the agent's wait-status decoding,
//! the waits for one named child, and an execution's signal to its program's process group by the
//! group's ID, before Linux 6.9, keep the real-time signals that `nix`'s `Signal` cannot represent;
//! a pidfd is opened with the `pidfd_open` system call, and signals are sent through it with
//! `pidfd_send_signal`, which neither `nix` nor musl wraps; a write to an execution's input
//! takes back the `SIGPIPE` that a closed reader raises with `sigtimedwait`, once `sigpending`
//! shows that no such signal was pending before, and `nix` wraps neither; and the agent's
//! execution helper duplicates the descriptor that it inherits by number, which it cannot borrow
//! before it knows that the descriptor is open. So do the time tools, for the calls that `nix`
//! does not wrap: `ioperm`, `adjtimex`, `klogctl`, `getauxval`, `sigqueue`, and `_exit`;
//! `sched_setscheduler`, which musl does not implement for one thread; and the `clock_gettime`
//! and `getpid` system calls, which the latency probe measures without the vDSO's or libc's
//! shortcuts. The guest mappings need `statx`, for the kinds and unique mount IDs of open files,
//! and the mount API's `open_tree`, `mount_setattr`, and `move_mount`, which neither `nix` nor
//! musl wraps. The tests trace a child with libc's `ptrace`, and lower the process's descriptor
//! limit with libc's `setrlimit`, as the crate enables neither for `nix`, and deny a system call
//! to a thread with libc's `prctl`, as `nix` does not wrap seccomp. Each such call says why.
//!
//! The x86 instructions of the time tools are `core::arch` intrinsics, except the port
//! instructions, which only `port` executes, as inline assembly. The constants of the kernel's
//! and the processor's interfaces, such as procfs and sysfs paths, OpenVMM's ports, CPUID
//! leaves, and MSRs, are defined here once, so that the programs name them.

mod clock;
mod discipline;
mod entry;
mod fd;
mod filesystem;
mod ioctl;
pub(crate) mod kernel;
#[cfg(target_arch = "x86_64")]
mod kvm;
mod mapping;
#[cfg(target_arch = "x86_64")]
mod msr;
mod poll;
mod port;
mod process;
mod rooted;
mod scheduler;
mod secret;
mod signal;
mod sock_diag;
mod socket;
mod spawn;
mod terminal;
mod timer;
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86;

use std::ffi::CString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStringExt;

pub(crate) use nix::errno::Errno;
pub(crate) use nix::sys::signal::Signal;
pub(crate) use nix::sys::time::{TimeSpec, TimeValLike};

pub(crate) use clock::{Clock, clock_time, monotonic_ms, sleep_once, sleep_until_monotonic};
#[cfg(target_arch = "x86_64")]
pub(crate) use clock::{monotonic_ns, syscall_monotonic_nanoseconds};
pub(crate) use discipline::{ClockAdjustment, ClockDiscipline, FrequencyHold, adjust_clock};
pub(crate) use entry::{EntryChild, file_size};
#[cfg(target_arch = "x86_64")]
pub(crate) use fd::close_inherited_descriptors;
pub(crate) use fd::{
    CreatedFile, FileDescriptor, FileLock, Mode, OpenFlags, create_directory, create_memfd,
    create_pipe, create_socket_pair, lock_file, open, open_at, page_size, try_lock_directory,
    unlink,
};
pub(crate) use filesystem::{
    MountFlags, PATH_MAX, ScratchDirectory, TmpfsMount, check_exists, chmod, chown,
    enter_private_mount_namespace, mount, mount_id, path_c_string, path_exists, read_file,
    read_line, read_number, unmount, write_file, write_number,
};
pub(crate) use ioctl::{
    block_device_size, console_pending, flush_block_buffers, logical_block_size,
    reread_partition_table, reseed_random,
};
#[cfg(target_arch = "x86_64")]
pub(crate) use kvm::{KVM_API_VERSION, KVM_EXIT_HLT, Kvm};
pub(crate) use mapping::MemoryMapping;
#[cfg(target_arch = "x86_64")]
pub(crate) use msr::{MsrAccess, MsrFile};
pub(crate) use poll::{Pollable, hung_up, poll, readable_or_hung_up};
pub(crate) use port::{PORT_DEVICE, Port};
#[cfg(target_arch = "x86_64")]
pub(crate) use port::{PortAccess, PortError, check_released, power_off, probe_port};
pub(crate) use process::set_command_name;
pub(crate) use process::{
    ChildStatus, DirectStdout, ForkResult, ProcessId, Stdout, WaitTarget, current_process, detach,
    exec, exit_immediately, fork, ignore_interrupt_and_hangup, is_unreaped, kill_process,
    kill_process_group, kill_through_pidfd, open_pidfd, process_exists, process_group_of,
    process_name, reap_any_nonblocking, restore_sigpipe, signal_process_group,
    signal_through_pidfd, take_inherited_descriptor, try_wait, wait, wait_named,
    write_port_diagnostic, write_standard_error,
};
#[cfg(target_arch = "x86_64")]
pub(crate) use process::{discard_standard_input, syscall_process_id};
pub(crate) use rooted::{
    FileKind, FileStatus, MountAttributes, change_mode, create_directory_at, create_file_at,
    detach_mount, file_status, is_whole_file_system, move_mount, open_beneath, open_tree,
    set_mount_attributes, unmount_beneath,
};
pub(crate) use scheduler::{
    AffinityGuard, IdlePriority, SchedulingPolicy, current_affinity, current_cpu, current_thread,
    pin_current_thread, promote_all_threads, scheduling_policy, set_scheduling_policy,
    thread_count,
};
pub(crate) use secret::zeroize;
#[cfg(target_arch = "x86_64")]
pub(crate) use signal::write_standard_error_without_sigpipe;
pub(crate) use signal::{
    ChildSignalWait, SignalDescriptor, ignore_signals, queue_user_signal, reset_child_signal,
    write_without_sigpipe,
};
pub(crate) use sock_diag::{
    INET_DIAG_BUFFER_LENGTH, InetDiagMessage, decode_inet_diag_dump, dump_ipv6_tcp_sockets,
};
pub(crate) use socket::{UdpSocket, connect_datagram};
pub(crate) use spawn::{ChildSpec, PreparedChild, REPORT_LENGTH};
pub(crate) use terminal::{configure_raw, drain};
pub(crate) use timer::Timer;

/// Returns the process arguments, program name first, as the C strings that `main` received.
///
/// Rust exposes Unix arguments as arbitrary bytes. Converting them back to `CString` is
/// infallible for real process arguments because the kernel terminates, rather than embeds,
/// their NUL bytes.
pub(crate) fn args() -> Vec<CString> {
    std::env::args_os()
        .map(|argument| CString::new(argument.into_vec()).expect("arguments are C strings"))
        .collect()
}

/// Converts arbitrary bytes into a C string, or `EINVAL` if they contain a NUL.
pub(crate) fn c_string(bytes: &[u8]) -> Result<CString, Errno> {
    CString::new(bytes).map_err(|_| Errno::EINVAL)
}

/// Writes the concatenation of `parts` to `stream` in one write.
///
/// The C tools wrote diagnostics through unbuffered `stderr` and ignored a diagnostic write
/// failure. This helper deliberately preserves both behaviors.
pub(crate) fn report(stream: &mut dyn Write, parts: &[&[u8]]) {
    let _ = stream.write_all(&parts.concat());
}

/// Writes `bytes` to `stream`, flushes it, and returns the corresponding `errno` on failure.
pub(crate) fn print(stream: &mut dyn Write, bytes: &[u8]) -> Result<(), Errno> {
    stream
        .write_all(bytes)
        .and_then(|()| stream.flush())
        .map_err(|error| errno_of(&error))
}

/// Returns the `errno` of an I/O error, or `EIO` for an error without one.
///
/// A write that made no progress has no OS error, so it maps to `EIO`, matching the guest tools'
/// generic I/O-failure treatment.
pub(crate) fn errno_of(error: &io::Error) -> Errno {
    error.raw_os_error().map_or(Errno::EIO, Errno::from_raw)
}

/// Describes an I/O error as the guest programs describe an `errno`, with `nix`'s text and
/// without the standard library's `(os error N)`, or with the error's own text when it has no
/// `errno`.
pub(crate) fn describe(error: &io::Error) -> String {
    error.raw_os_error().map_or_else(
        || error.to_string(),
        |code| Errno::from_raw(code).desc().to_owned(),
    )
}

#[cfg(test)]
pub(crate) mod tests;
