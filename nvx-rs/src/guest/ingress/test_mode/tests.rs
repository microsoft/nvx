//! Snapshot path compatibility tests.

use std::os::unix::ffi::OsStrExt;

use super::*;

/// Verifies both generated paths use the original byte-capacity limits.
#[test]
fn snapshot_paths_enforce_the_c_buffers_in_bytes() {
    assert!(snapshot_root(&vec![b'a'; 4093], 0).is_some());
    assert!(snapshot_root(&vec![b'a'; 4094], 0).is_none());
    let root = snapshot_root(b"directory", 12).unwrap();
    assert_eq!(root.as_os_str().as_bytes(), b"directory/12");
}
