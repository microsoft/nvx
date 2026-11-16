//! Filesystem operations of Linux hosts that the standard library lacks.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use nix::errno::Errno;

/// `renameat2`'s `RENAME_NOREPLACE`, in `linux/fs.h`.
const RENAME_NOREPLACE: nix::libc::c_uint = 1;

/// Renames `from` to `to` unless `to` exists, which it leaves as it is, with `renameat2`'s
/// `RENAME_NOREPLACE`, through the system call itself, which `nix` wraps only for glibc. Fails
/// with [`io::ErrorKind::AlreadyExists`] if `to` exists, and as `rename` fails otherwise; a
/// filesystem that cannot rename without replacing fails it with `EINVAL`.
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let text = |path: &Path| {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path holds a NUL byte"))
    };
    let (from, to) = (text(from)?, text(to)?);
    // SAFETY: `renameat2` reads the two NUL-terminated paths, which outlive the call.
    Errno::result(unsafe {
        nix::libc::syscall(
            nix::libc::SYS_renameat2,
            nix::libc::AT_FDCWD,
            from.as_ptr(),
            nix::libc::AT_FDCWD,
            to.as_ptr(),
            RENAME_NOREPLACE,
        )
    })
    .map(drop)
    .map_err(io::Error::from)
}
