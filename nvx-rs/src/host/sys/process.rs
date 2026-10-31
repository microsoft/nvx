//! Process descriptors, and the hook that detaches a spawned process from its caller.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::ptr;
use std::time::Duration;

use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl, open};
use nix::libc;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{self, Signal};
use nix::sys::stat::Mode;
use nix::unistd::{Pid, setsid};

use super::Errno;

/// The highest descriptor of a standard stream: standard error.
///
/// Standard input, output, and error are descriptors 0, 1, and 2 in every process, and a spawn
/// gives the new process its own standard streams under those numbers. A descriptor of the
/// caller's at or below this one therefore cannot reach the new process under its number, and
/// the descriptors that a detached spawn marks close-on-exec are the ones above it.
pub(crate) const LAST_STANDARD_STREAM: RawFd = libc::STDERR_FILENO;

/// Moves `file` to a descriptor above [`LAST_STANDARD_STREAM`], under which a spawn can pass it to
/// its process, if it is not there already: as when the caller had closed a standard stream, whose
/// descriptor an open then took.
///
/// The new descriptor is a close-on-exec copy of the old one, which is then closed. It shares the
/// file's open file description, and with it the file's offset and `flock` locks.
pub(crate) fn move_above_standard_streams(file: &mut File) -> io::Result<()> {
    if file.as_raw_fd() > LAST_STANDARD_STREAM {
        return Ok(());
    }
    let copy = fcntl(&*file, FcntlArg::F_DUPFD_CLOEXEC(LAST_STANDARD_STREAM + 1))?;
    // SAFETY: fcntl returned a fresh descriptor, which nothing else owns. Replacing the file
    // closes the old descriptor.
    *file = File::from(unsafe { OwnedFd::from_raw_fd(copy) });
    Ok(())
}

/// A pidfd: an owned descriptor that refers to one process. Signals sent through it reach that
/// process or fail, even once another process reuses its ID.
pub(crate) struct PidFd(OwnedFd);

impl PidFd {
    /// Opens a pidfd for the process `pid`.
    ///
    /// Fails with `ESRCH` if no such process exists, with `EINVAL` if `pid` names a thread that
    /// does not lead its process, and with [`io::ErrorKind::InvalidInput`] if `pid` is not a
    /// process ID of Linux. Hosts without pidfds, Linux before 5.3 or seccomp profiles that
    /// reject them, fail with another error.
    pub(crate) fn open(pid: u32) -> io::Result<Self> {
        let pid = libc::pid_t::try_from(pid).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{pid} is not a process ID"),
            )
        })?;
        // SAFETY: pidfd_open takes a process ID and flags and returns a new descriptor or -1.
        let descriptor = Errno::result(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) })?;
        // SAFETY: the kernel returned a fresh descriptor that nothing else owns.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(descriptor as RawFd) }))
    }

    /// Waits up to `timeout` for the process to exit, and returns whether it has.
    ///
    /// The pidfd becomes readable once the whole process has exited: its last thread, not only
    /// its main one. That holds whether or not the caller is the process's parent, and whether
    /// or not the process has been reaped. A signal that interrupts the wait ends it early.
    pub(crate) fn wait_exit(&self, timeout: Duration) -> io::Result<bool> {
        // Round up, so that a wait shorter than a millisecond does not spin.
        let timeout = PollTimeout::try_from(timeout.as_nanos().div_ceil(1_000_000))
            .unwrap_or(PollTimeout::MAX);
        let mut descriptors = [PollFd::new(self.0.as_fd(), PollFlags::POLLIN)];
        match poll(&mut descriptors, timeout) {
            Ok(0) | Err(Errno::EINTR) => Ok(false),
            Ok(_) => {
                let events = descriptors[0].revents().unwrap_or(PollFlags::empty());
                // Since Linux 6.9, a pidfd also reports a hang-up once its process is reaped.
                if events.intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
                    Ok(true)
                } else {
                    Err(io::Error::other(format!(
                        "a pidfd reported unexpected events {events:?}"
                    )))
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Sends `SIGKILL` to the process.
    ///
    /// Fails with `ESRCH` if the process has exited.
    pub(crate) fn kill(&self) -> io::Result<()> {
        // SAFETY: the descriptor is a valid pidfd, and a null siginfo with no flags is allowed.
        Errno::result(unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                Signal::SIGKILL as libc::c_int,
                ptr::null::<libc::siginfo_t>(),
                0,
            )
        })?;
        Ok(())
    }
}

/// Returns whether a process has the ID `pid`, as signal 0 shows without signaling it.
///
/// Only `ESRCH` means that no process has the ID: another user's process, which refuses the
/// signal with `EPERM`, has it too, and a process that exited keeps it until it is reaped.
pub(crate) fn process_exists(pid: u32) -> io::Result<bool> {
    // Signal 0 to the ID 0, or to a negative one, would test a process group instead.
    let Some(pid) = libc::pid_t::try_from(pid).ok().filter(|&pid| pid > 0) else {
        return Ok(false);
    };
    match signal::kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(Errno::EPERM) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Makes `command` start its process in a new session, with every descriptor of the caller
/// above standard error close-on-exec, except the descriptors `inherit`, which the process
/// inherits under the same numbers, and with the caller's umask without the owner's bits, as
/// [`clear_owner_bits_of_umask`] describes.
///
/// Only the new process clears the flag of a descriptor in `inherit`, between its fork and its
/// exec, so the descriptors stay close-on-exec in the caller, and no process that another thread
/// spawns meanwhile inherits them. The caller keeps them open until the spawn returns. Only the
/// new process changes its umask too, so the caller keeps its own.
pub(crate) fn detach(command: &mut Command, inherit: Vec<RawFd>) {
    restrict_inheritance(command, true, inherit);
}

/// Makes `command` start its process as [`detach`] does, but in the caller's session, as a child
/// that shares the caller's terminal.
#[cfg(feature = "cli")]
pub(crate) fn inherit_only(command: &mut Command, inherit: Vec<RawFd>) {
    restrict_inheritance(command, false, inherit);
}

/// Makes `command` start its process in a new session if `new_session`, with every descriptor of
/// the caller above standard error close-on-exec, except the descriptors `inherit`, and with the
/// caller's umask without the owner's bits, as [`detach`] describes.
fn restrict_inheritance(command: &mut Command, new_session: bool, inherit: Vec<RawFd>) {
    // SAFETY: the hook makes only async-signal-safe system calls and allocates nothing, as code
    // that runs between fork and exec must: `inherit` was allocated before the fork.
    unsafe {
        command.pre_exec(move || {
            if new_session {
                setsid()?;
            }
            clear_owner_bits_of_umask()?;
            mark_descriptors_close_on_exec()?;
            inherit_descriptors(&inherit)
        });
    }
}

/// Clears the owner's bits from the umask that the new process inherited from the caller, and
/// keeps the group's and others'.
///
/// A VMM gives the files that it creates the modes that it intends, and leaves the umask only
/// the group's and others' permissions to narrow: OpenVMM publishes its outcome report and its
/// snapshots private to their owner, reopens files that it created, and creates the files of a
/// share with the modes that the guest asks for. A umask that clears the owner's own
/// permissions, such as 0400, would keep the caller from reading the report, and OpenVMM from
/// using its own files. The usual umasks, such as 022 or 077, clear none of the owner's bits, so
/// this changes nothing for them, and the umask still narrows the group's and others'
/// permissions as the caller's does. In a directory that has a default POSIX ACL, Linux applies
/// that ACL instead of the umask, so the ACL decides the permissions of what is created there,
/// the owner's included.
///
/// The process makes the system calls itself, because `nix`'s `umask` panics when a call fails,
/// as a seccomp policy can make it, and a panic between the fork and the exec would end the
/// process without the error that fails the spawn.
fn clear_owner_bits_of_umask() -> io::Result<()> {
    // Reading the umask replaces it. 077 withholds at least what the final umask withholds, so a
    // file that a signal handler of the caller creates between the two calls gets no permission
    // that the final umask would not give, as it could under 0.
    let inherited = set_umask(0o077)?;
    set_umask(inherited & !libc::S_IRWXU)?;
    Ok(())
}

/// Sets the calling process's umask to `mask`, and returns the umask that it replaces.
fn set_umask(mask: libc::mode_t) -> io::Result<libc::mode_t> {
    // SAFETY: `umask` takes an integer and dereferences nothing.
    let previous = Errno::result(unsafe { libc::syscall(libc::SYS_umask, mask) })?;
    // A umask fits a `mode_t`.
    Ok(previous as libc::mode_t)
}

/// Makes `command` refuse the `umask` system call with the error `errno` in its new process, as
/// a seccomp policy can, through a seccomp filter that allows every other call, before the hooks
/// that a later [`detach`] adds run. The filter needs `no_new_privs`, which the process sets
/// first.
#[cfg(test)]
pub(crate) fn refuse_umask_on_spawn(command: &mut Command, errno: i32) {
    let statement = |code: u32, if_equal: u8, otherwise: u8, k: u32| libc::sock_filter {
        // Every BPF code fits 16 bits.
        code: code as u16,
        jt: if_equal,
        jf: otherwise,
        k,
    };
    // The filter loads the call's number, the first word of the kernel's `seccomp_data`, and
    // returns `errno` for `umask`, or allows the call.
    let mut filter = [
        statement(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0),
        // A system call number fits 32 bits.
        statement(
            libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
            0,
            1,
            libc::SYS_umask as u32,
        ),
        // An error number fits the 16 bits of the filter's data.
        statement(
            libc::BPF_RET | libc::BPF_K,
            0,
            0,
            libc::SECCOMP_RET_ERRNO | errno as u32,
        ),
        statement(libc::BPF_RET | libc::BPF_K, 0, 0, libc::SECCOMP_RET_ALLOW),
    ];
    // SAFETY: the hook makes only async-signal-safe system calls and allocates nothing, as code
    // that runs between fork and exec must: the filter was built before the fork, and the kernel
    // copies it before `prctl` returns.
    unsafe {
        command.pre_exec(move || {
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_mut_ptr(),
            };
            let no_new_privs = libc::prctl(
                libc::PR_SET_NO_NEW_PRIVS,
                1 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            );
            if no_new_privs != 0 {
                return Err(io::Error::last_os_error());
            }
            let installed = libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                &program as *const libc::sock_fprog,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            );
            if installed != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Clears the close-on-exec flag of each descriptor in `inherit`, which the caller of [`detach`]
/// keeps open across the spawn, in the new process between its fork and its exec.
fn inherit_descriptors(inherit: &[RawFd]) -> io::Result<()> {
    for &descriptor in inherit {
        // SAFETY: the caller of `detach` keeps the descriptor open across the spawn, and between
        // fork and exec only this thread runs, which closes no descriptor while the borrow lasts.
        let descriptor = unsafe { BorrowedFd::borrow_raw(descriptor) };
        fcntl(descriptor, FcntlArg::F_SETFD(FdFlag::empty()))?;
    }
    Ok(())
}

/// Makes `command` mark only the descriptors that `/proc/self/fd` lists close-on-exec before it
/// executes, as [`detach`] does on kernels without `CLOSE_RANGE_CLOEXEC`.
#[cfg(test)]
pub(crate) fn mark_listed_descriptors_on_spawn(command: &mut Command) {
    // SAFETY: the hook makes only async-signal-safe system calls and allocates nothing, as code
    // that runs between fork and exec must.
    unsafe { command.pre_exec(mark_listed_descriptors_close_on_exec) };
}

/// Clears the close-on-exec flag of `descriptor`, as a descriptor that the caller's parent or
/// native code left inheritable lacks it.
#[cfg(test)]
pub(crate) fn clear_close_on_exec(descriptor: impl AsFd) -> io::Result<()> {
    fcntl(descriptor, FcntlArg::F_SETFD(FdFlag::empty()))?;
    Ok(())
}

/// Marks every descriptor above standard error close-on-exec.
///
/// Rust opens its own descriptors close-on-exec, but a descriptor that the caller inherited or
/// that native code opened may lack the flag, and a detached VM would hold it until it stops: a
/// pipe writer it holds, for example, keeps the reader from seeing end-of-file. Marking rather
/// than closing keeps the descriptor through which `Command::spawn` reports a failed exec.
fn mark_descriptors_close_on_exec() -> io::Result<()> {
    let first: libc::c_uint = LAST_STANDARD_STREAM.unsigned_abs() + 1;
    // SAFETY: close_range takes two descriptor bounds and flags and changes no memory.
    let marked = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if marked == 0 {
        return Ok(());
    }
    // Linux before 5.11 lacks CLOSE_RANGE_CLOEXEC, and seccomp policies may reject close_range.
    mark_listed_descriptors_close_on_exec()
}

/// Marks every descriptor above standard error that `/proc/self/fd` lists close-on-exec.
///
/// It runs only in a `pre_exec` hook, between fork and exec, where nothing may allocate, so it
/// reads the directory with getdents64 into a stack buffer rather than through `nix`'s `Dir`,
/// whose `fdopendir` allocates.
fn mark_listed_descriptors_close_on_exec() -> io::Result<()> {
    let directory = open(
        c"/proc/self/fd",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    mark_directory_entries(&directory)
}

/// Marks the descriptors enumerated by the open `/proc/self/fd` `directory` close-on-exec.
fn mark_directory_entries(directory: &OwnedFd) -> io::Result<()> {
    #[repr(C, align(8))]
    struct Records([u8; 4096]);

    let malformed = || io::Error::from(Errno::EIO);
    let mut buffer = Records([0; 4096]);
    loop {
        // SAFETY: the kernel writes at most the buffer's length into it.
        let read = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory.as_raw_fd(),
                buffer.0.as_mut_ptr(),
                buffer.0.len(),
            )
        };
        let Ok(read) = usize::try_from(read) else {
            return Err(io::Error::last_os_error());
        };
        if read == 0 {
            return Ok(());
        }
        let mut records = buffer.0.get(..read).ok_or_else(malformed)?;
        while !records.is_empty() {
            // A linux_dirent64 holds d_ino (8 bytes), d_off (8), d_reclen (2), d_type (1), and
            // the NUL-terminated name.
            let length = match records.get(16..18) {
                Some(&[low, high]) => usize::from(u16::from_ne_bytes([low, high])),
                _ => return Err(malformed()),
            };
            let (record, rest) = records.split_at_checked(length).ok_or_else(malformed)?;
            let name = record.get(19..).ok_or_else(malformed)?;
            if let Some(descriptor) = descriptor_number(name)
                && descriptor > LAST_STANDARD_STREAM
                && descriptor != directory.as_raw_fd()
            {
                // SAFETY: the listing shows the descriptor open, and between fork and exec only
                // this thread runs, which closes no descriptor while the borrow lasts.
                let descriptor = unsafe { BorrowedFd::borrow_raw(descriptor) };
                // F_SETFD changes only the flags of the descriptor, so a failure leaves it as is.
                let _ = fcntl(descriptor, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC));
            }
            records = rest;
        }
    }
}

/// Parses a `/proc/self/fd` entry name, which ends at its NUL terminator.
fn descriptor_number(name: &[u8]) -> Option<RawFd> {
    let digits = name.split(|&byte| byte == 0).next()?;
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0, |value: RawFd, &byte| {
        let digit = byte.checked_sub(b'0').filter(|digit| *digit < 10)?;
        value.checked_mul(10)?.checked_add(RawFd::from(digit))
    })
}
