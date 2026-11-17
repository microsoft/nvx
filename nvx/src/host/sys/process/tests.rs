use super::*;

fn statement(code: u32, if_equal: u8, otherwise: u8, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        // Every BPF code fits 16 bits.
        code: code as u16,
        jt: if_equal,
        jf: otherwise,
        k,
    }
}

/// Makes `command` refuse only `umask` in its new process, before a later [`detach`] hook.
pub(crate) fn refuse_umask_on_spawn(command: &mut Command, errno: i32) {
    // The filter loads the call's number, the first word of the kernel's `seccomp_data`, and
    // returns `errno` for `umask`, or allows the call.
    let filter = [
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
    filter_on_spawn(command, filter);
}

/// Makes only `getsockopt(SOL_SOCKET, SO_PEERPIDFD)` fail in the new process.
pub(crate) fn refuse_peer_pidfd_on_spawn(command: &mut Command, errno: i32) {
    let arguments = std::mem::offset_of!(libc::seccomp_data, args) as u32;
    let word = if cfg!(target_endian = "big") { 4 } else { 0 };
    let load = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
    let equal = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
    let filter = [
        statement(load, 0, 0, 0),
        statement(equal, 0, 5, libc::SYS_getsockopt as u32),
        statement(load, 0, 0, arguments + 8 + word),
        statement(equal, 0, 3, libc::SOL_SOCKET as u32),
        statement(load, 0, 0, arguments + 16 + word),
        statement(equal, 0, 1, libc::SO_PEERPIDFD as u32),
        statement(
            libc::BPF_RET | libc::BPF_K,
            0,
            0,
            libc::SECCOMP_RET_ERRNO | errno as u32,
        ),
        statement(libc::BPF_RET | libc::BPF_K, 0, 0, libc::SECCOMP_RET_ALLOW),
    ];
    filter_on_spawn(command, filter);
}

fn filter_on_spawn<const N: usize>(command: &mut Command, mut filter: [libc::sock_filter; N]) {
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

/// Makes `command` mark only the descriptors that `/proc/self/fd` lists close-on-exec before it
/// executes, as [`detach`] does on kernels without `CLOSE_RANGE_CLOEXEC`.
pub(crate) fn mark_listed_descriptors_on_spawn(command: &mut Command) {
    // SAFETY: the hook makes only async-signal-safe system calls and allocates nothing, as code
    // that runs between fork and exec must.
    unsafe { command.pre_exec(mark_listed_descriptors_close_on_exec) };
}

/// Clears the close-on-exec flag of `descriptor`, as a descriptor that the caller's parent or
/// native code left inheritable lacks it.
pub(crate) fn clear_close_on_exec(descriptor: impl AsFd) -> io::Result<()> {
    fcntl(descriptor, FcntlArg::F_SETFD(FdFlag::empty()))?;
    Ok(())
}
