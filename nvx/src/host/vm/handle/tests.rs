use super::*;

pub(in crate::host::vm) fn inherited_file(resource: InheritedHandle) -> File {
    #[cfg(any(unix, windows))]
    return File::from(Owned::from(resource));
    #[cfg(not(any(unix, windows)))]
    resource.handle
}

#[cfg(unix)]
#[test]
fn an_owned_descriptor_moves_into_and_out_of_an_inherited_handle() {
    use std::io::Write;
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};

    let file = tempfile::tempfile().unwrap();
    let number = file.as_raw_fd();
    let descriptor: OwnedFd = file.into();
    let resource: InheritedHandle = descriptor.into();
    assert_eq!(resource.as_fd().as_raw_fd(), number);
    let descriptor: OwnedFd = resource.into();
    assert_eq!(descriptor.as_raw_fd(), number);
    File::from(descriptor).write_all(b"still open").unwrap();
}

#[cfg(windows)]
#[test]
fn an_owned_windows_handle_moves_into_and_out_of_an_inherited_handle() {
    use std::io::Write;
    use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};

    let file = tempfile::tempfile().unwrap();
    let number = file.as_raw_handle();
    let handle: OwnedHandle = file.into();
    let resource: InheritedHandle = handle.into();
    assert_eq!(resource.as_handle().as_raw_handle(), number);
    let handle: OwnedHandle = resource.into();
    assert_eq!(handle.as_raw_handle(), number);
    File::from(handle).write_all(b"still open").unwrap();
}
