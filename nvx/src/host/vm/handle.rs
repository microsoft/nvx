//! An owned descriptor or handle that a VMM inherits, and its platform conversions.

use std::fs::File;

/// An owned descriptor on Unix.
#[cfg(unix)]
type Owned = std::os::fd::OwnedFd;
/// An owned handle on Windows.
#[cfg(windows)]
type Owned = std::os::windows::io::OwnedHandle;
/// An owned file on hosts without descriptors or handles.
#[cfg(not(any(unix, windows)))]
type Owned = File;

/// An already opened resource that a VMM inherits, owned until dropped or transferred.
///
/// Conversions move the same descriptor or handle; they neither duplicate nor reopen it.
/// Borrowed raw descriptors and handles cannot transfer ownership into this type.
#[derive(Debug)]
pub struct InheritedHandle {
    /// The owned platform resource.
    handle: Owned,
}

impl From<File> for InheritedHandle {
    fn from(file: File) -> Self {
        Self {
            handle: file.into(),
        }
    }
}

#[cfg(unix)]
impl From<std::os::fd::OwnedFd> for InheritedHandle {
    fn from(handle: std::os::fd::OwnedFd) -> Self {
        Self { handle }
    }
}

#[cfg(unix)]
impl From<InheritedHandle> for std::os::fd::OwnedFd {
    fn from(resource: InheritedHandle) -> Self {
        resource.handle
    }
}

#[cfg(unix)]
impl std::os::fd::AsFd for InheritedHandle {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.handle)
    }
}

#[cfg(windows)]
impl From<std::os::windows::io::OwnedHandle> for InheritedHandle {
    fn from(handle: std::os::windows::io::OwnedHandle) -> Self {
        Self { handle }
    }
}

#[cfg(windows)]
impl From<InheritedHandle> for std::os::windows::io::OwnedHandle {
    fn from(resource: InheritedHandle) -> Self {
        resource.handle
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsHandle for InheritedHandle {
    fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        std::os::windows::io::AsHandle::as_handle(&self.handle)
    }
}

#[cfg(test)]
pub(super) mod tests;
