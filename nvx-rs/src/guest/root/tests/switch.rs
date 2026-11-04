//! Tests of [`Root::switch`] with the fake kernel: the move of the root's mount onto `/` before
//! the entry into the root, the refusal of a second switch, the teardown of a switched root,
//! which refuses, and its drop, neither of which unmounts anything, a move that fails, after
//! which the root has not switched, and an entry that fails, after which it has.

use super::*;

/// Returns the calls with which the fake kernel records the switch into `guest`'s root: the move
/// of the root's mount onto `/`, then the entry into the root.
fn switch_calls(guest: &Guest) -> [Call; 2] {
    let root = guest.text("run/nvx/rootfs");
    [
        Call::Mount {
            source: Some(root.clone()),
            target: "/".to_owned(),
            filesystem: None,
            flags: MountFlags::MS_MOVE,
            data: None,
        },
        Call::EnterRoot(root),
    ]
}

/// Returns the mount points of the assembly's mounts, in the order of their unmounts.
fn assembly_unmounts(guest: &Guest) -> Vec<String> {
    ["rootfs", "layers/distro", "scratch"]
        .iter()
        .map(|mount| guest.text(&format!("run/nvx/{mount}")))
        .collect()
}

#[test]
fn a_root_switches_once_and_its_teardown_then_unmounts_nothing() {
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    root.mount_runtime(None).unwrap();
    let before = guest.kernel.calls().len();
    root.switch().unwrap();
    assert_eq!(guest.kernel.calls()[before..], switch_calls(&guest));

    // A second switch is refused before any call.
    let error = root.switch().unwrap_err();
    assert!(matches!(error, SwitchError::Switched), "{error:?}");
    assert_eq!(error.to_string(), "the root has switched already");

    // The teardown refuses, and neither it nor the drop that follows unmounts anything.
    let error = root.teardown().unwrap_err();
    assert!(error.is_switched(), "{error:?}");
    assert!(error.failures().is_empty(), "{error:?}");
    assert_eq!(
        error.to_string(),
        "the root has switched, so its mounts stay"
    );
    assert_eq!(guest.kernel.calls().len(), before + 2);
    assert_eq!(guest.kernel.unmounts(), Vec::<String>::new());
}

#[test]
fn a_switched_root_that_is_dropped_unmounts_nothing() {
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    root.switch().unwrap();
    drop(root);
    assert_eq!(guest.kernel.unmounts(), Vec::<String>::new());
}

#[test]
fn a_move_that_fails_leaves_the_root_unswitched() {
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    guest.kernel.fail(Operation::Mount, "/", Errno::EINVAL);
    let before = guest.kernel.calls().len();
    let error = root.switch().unwrap_err();
    assert!(
        matches!(
            &error,
            SwitchError::Move { error } if error.raw_os_error() == Some(Errno::EINVAL as i32)
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "failed to move the root onto /: Invalid argument (os error 22)"
    );
    // Nothing entered the root, and its teardown unmounts the assembly's mounts.
    assert_eq!(guest.kernel.calls()[before..], switch_calls(&guest)[..1]);
    root.teardown().unwrap();
    assert_eq!(guest.kernel.unmounts(), assembly_unmounts(&guest));
}

#[test]
fn a_root_whose_entry_fails_has_switched() {
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    let path = guest.path("run/nvx/rootfs");
    guest.kernel.fail(Operation::EnterRoot, path, Errno::EACCES);
    let error = root.switch().unwrap_err();
    assert!(
        matches!(
            &error,
            SwitchError::Enter { error } if error.raw_os_error() == Some(Errno::EACCES as i32)
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "failed to enter the root, whose mount is on / already: Permission denied (os error 13)"
    );
    // The root's mount is on `/`, so the root refuses another switch and its teardown.
    let error = root.switch().unwrap_err();
    assert!(matches!(error, SwitchError::Switched), "{error:?}");
    assert!(root.teardown().unwrap_err().is_switched());
    assert_eq!(guest.kernel.unmounts(), Vec::<String>::new());
}
