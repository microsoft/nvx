//! Regressions for the names that the VM tests give their sandboxes.
#![cfg(feature = "host")]

mod common;

use common::Sandbox;

/// Windows refuses to create a directory under the name of one that another test of the process is
/// still removing, so a sandbox never takes the name of one that the process dropped, even for a
/// test of the same name.
#[test]
fn a_process_never_reuses_the_name_of_a_dropped_sandbox() {
    let dropped = Sandbox::new("names");
    let name = dropped.root.clone();
    drop(dropped);
    assert!(
        !name.exists(),
        "the dropped sandbox kept {}",
        name.display()
    );
    let next = Sandbox::new("names");
    assert_ne!(next.root, name);
}
