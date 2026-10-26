//! The descriptors or handles of a caller that a VMM inherits.

#[cfg(not(any(unix, windows)))]
use std::marker::PhantomData;
#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd};
#[cfg(windows)]
use std::os::windows::io::{AsHandle, BorrowedHandle};
#[cfg(target_os = "linux")]
use std::path::PathBuf;

/// The descriptor of the caller that an [`Inherited`] borrows.
#[cfg(unix)]
type Borrowed<'a> = BorrowedFd<'a>;
/// The handle of the caller that an [`Inherited`] borrows.
#[cfg(windows)]
type Borrowed<'a> = BorrowedHandle<'a>;
/// What an [`Inherited`] would borrow on a host without descriptors or handles, which has none.
#[cfg(not(any(unix, windows)))]
#[derive(Debug, Clone, Copy)]
struct Borrowed<'a>(std::convert::Infallible, PhantomData<&'a ()>);

/// A descriptor (Unix) or handle (Windows) of the caller, such as that of an open
/// [`File`](std::fs::File), that a VMM inherits under the number that it has in the caller, which
/// the caller can therefore name in the VMM's arguments.
///
/// [`spawn`](crate::host::supervisor::spawn) passes the VMM these and its standard streams, and
/// nothing else of the caller's:
///
/// - On Linux, the descriptor must be close-on-exec in the caller, as Rust opens its files, and
///   stays so: only the VMM's own process clears the flag, between its fork and its exec, so no
///   process that another thread spawns meanwhile inherits it. A spawn refuses a descriptor that
///   is not close-on-exec, since any process that the caller spawns would inherit it. The VMM
///   opens it again as `/proc/self/fd/N`, which `proc_path` returns.
/// - On Windows, the handle joins the explicit list of handles that the VMM inherits, and is
///   inheritable only while the VMM is created, after which the spawn restores its earlier
///   inheritability. Spawns that allow handles take that window one at a time, so several may
///   allow the same handle. A process that another thread creates in that window without such a
///   list, as `std::process::Command` does, can inherit it too.
///
/// Other hosts fail a spawn that passes any.
#[derive(Debug, Clone, Copy)]
pub struct Inherited<'a>(Borrowed<'a>);

impl<'a> Inherited<'a> {
    /// Names the descriptor of `object`, such as a [`File`](std::fs::File).
    #[cfg(unix)]
    pub fn new(object: &'a impl AsFd) -> Self {
        Self(object.as_fd())
    }

    /// Names the handle of `object`, such as a [`File`](std::fs::File).
    #[cfg(windows)]
    pub fn new(object: &'a impl AsHandle) -> Self {
        Self(object.as_handle())
    }
}

#[cfg(target_os = "linux")]
impl Inherited<'_> {
    /// Returns the path through which the VMM opens the descriptor again: `/proc/self/fd/N`,
    /// where `N` is its number in the caller and in the VMM.
    pub fn proc_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd;

        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }
}

#[cfg(unix)]
impl<'a> From<BorrowedFd<'a>> for Inherited<'a> {
    fn from(descriptor: BorrowedFd<'a>) -> Self {
        Self(descriptor)
    }
}

#[cfg(windows)]
impl<'a> From<BorrowedHandle<'a>> for Inherited<'a> {
    fn from(handle: BorrowedHandle<'a>) -> Self {
        Self(handle)
    }
}

#[cfg(unix)]
impl AsFd for Inherited<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0
    }
}

#[cfg(windows)]
impl AsHandle for Inherited<'_> {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0
    }
}
