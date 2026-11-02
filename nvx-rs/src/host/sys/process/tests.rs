use super::*;

/// Makes `command` refuse the `umask` system call with the error `errno` in its new process, as
/// a seccomp policy can, through a seccomp filter that allows every other call, before the hooks
/// that a later [`detach`] adds run. The filter needs `no_new_privs`, which the process sets
/// first.
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
