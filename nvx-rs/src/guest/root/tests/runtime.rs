//! Tests of [`Root::mount_runtime`] with the fake kernel: the tree's restriction before any
//! mount, the runtime's mounts in their order and with their options, the time status's
//! read-only bind or none, the mount points that it creates or refuses, the undo of a call that
//! fails at any of its steps, the teardown's order, the refusal of a call after a share or a
//! child of an aggregate, and the shares and children beneath `/run` after the runtime.

use super::*;

/// `MOUNT_ATTR_RDONLY`.
const RDONLY: u64 = 0x1;
/// `MOUNT_ATTR_NOSUID`.
const NOSUID: u64 = 0x2;
/// `MOUNT_ATTR_NODEV`.
const NODEV: u64 = 0x4;
/// `MOUNT_ATTR_NOEXEC`.
const NOEXEC: u64 = 0x8;

/// The runtime's mounts, in the order of their mounts.
const RUNTIME: [RuntimeMounts; 7] = [
    RuntimeMounts::Proc,
    RuntimeMounts::Sys,
    RuntimeMounts::Dev,
    RuntimeMounts::DevPts,
    RuntimeMounts::DevShm,
    RuntimeMounts::Run,
    RuntimeMounts::TimeStatus,
];

/// Returns a guest, with its root assembled, and the directory of a time status, `time`, beside
/// the runtime directory.
fn assembled() -> (Guest, Root) {
    let guest = Guest::new();
    let root = guest.assemble(&guest.assembly()).unwrap();
    fs::create_dir(guest.path("time")).unwrap();
    fs::write(guest.path("time/state"), b"synchronized").unwrap();
    (guest, root)
}

/// Returns the path of `mount`'s target in the guest's root.
fn point(guest: &Guest, mount: RuntimeMounts) -> PathBuf {
    guest.path("run/nvx/rootfs").join(&mount.target()[1..])
}

/// Returns the mount points of `mounts` in the guest's root, in reverse order, as the fake
/// kernel records their unmounts.
fn reversed(guest: &Guest, mounts: &[RuntimeMounts]) -> Vec<String> {
    let text = |mount: &RuntimeMounts| point(guest, *mount).display().to_string();
    mounts.iter().rev().map(text).collect()
}

/// Returns the guest's unmounts of the assembly's mounts at a teardown, after those of `first`.
fn then_the_assembly(guest: &Guest, mut first: Vec<String>) -> Vec<String> {
    for assembly in ["rootfs", "layers/distro", "scratch"] {
        first.push(guest.text(&format!("run/nvx/{assembly}")));
    }
    first
}

/// Returns the refusal of a call to [`Root::mount_runtime`] for what is mounted at `path`.
fn occupied(path: &Path) -> Result<(), String> {
    Err(format!(
        "a file system is mounted at {} already",
        path.display()
    ))
}

/// Returns a recorded new mount of `kind` with `parameters` and `attributes`.
fn new_mount(kind: &str, parameters: &[&str], attributes: u64) -> Call {
    Call::NewMount {
        kind: kind.to_owned(),
        parameters: parameters
            .iter()
            .map(|&parameter| parameter.to_owned())
            .collect(),
        attributes,
    }
}

/// Returns a recorded move of a new mount of `kind` onto `mount`'s target in the guest's root.
fn moved(guest: &Guest, kind: &str, mount: RuntimeMounts) -> Call {
    Call::MoveMount {
        source: kind.to_owned(),
        target: point(guest, mount).display().to_string(),
    }
}

#[test]
fn the_runtime_s_file_systems_are_mounted_in_order_once_the_tree_is_restricted() {
    let (guest, mut root) = assembled();
    let assembled = guest.kernel.calls().len();
    root.mount_runtime(Some(&guest.path("time"))).unwrap();
    // The tree is `nosuid` and `nodev` before the first mount, and each file system takes an
    // execution's options.
    let mut expected = vec![
        Call::RestrictTree(guest.text("run/nvx/rootfs")),
        new_mount("proc", &["source=proc"], NOSUID | NODEV | NOEXEC),
        moved(&guest, "proc", RuntimeMounts::Proc),
        new_mount("sysfs", &["source=sysfs"], RDONLY | NOSUID | NODEV | NOEXEC),
        moved(&guest, "sysfs", RuntimeMounts::Sys),
        new_mount("tmpfs", &["source=tmpfs", "mode=0755"], NOSUID),
        moved(&guest, "tmpfs", RuntimeMounts::Dev),
        Call::CreateDevices(guest.text("run/nvx/rootfs/dev")),
        new_mount(
            "devpts",
            &["source=devpts", "ptmxmode=0666", "mode=0620"],
            NOSUID | NOEXEC,
        ),
        moved(&guest, "devpts", RuntimeMounts::DevPts),
        new_mount("tmpfs", &["source=tmpfs", "mode=1777"], NOSUID | NODEV),
        moved(&guest, "tmpfs", RuntimeMounts::DevShm),
        new_mount("tmpfs", &["source=tmpfs", "mode=0755"], NOSUID | NODEV),
        moved(&guest, "tmpfs", RuntimeMounts::Run),
    ];
    // The time status is bound read-only, `nosuid`, and `nodev`, on the `/run` tmpfs.
    expected.extend(attach_calls(
        &guest.path("time"),
        &point(&guest, RuntimeMounts::TimeStatus),
        Access::ReadOnly,
    ));
    assert_eq!(guest.kernel.calls()[assembled..], expected);
    for mount in RUNTIME {
        let path = point(&guest, mount);
        assert!(guest.kernel.mount_at(&path).is_some(), "{mount:?}");
    }
    // The missing mount points are created, `/run/nvx` and `/run/nvx/time` with mode 0755.
    for relative in ["run/nvx", "run/nvx/time"] {
        let path = guest.path("run/nvx/rootfs").join(relative);
        let mode = fs::metadata(path).unwrap().mode() & 0o7777;
        assert_eq!(mode, 0o755, "{relative}");
    }

    // The teardown unmounts them, the later first, before the assembly's mounts.
    root.teardown().unwrap();
    let mut unmounts = reversed(&guest, &RUNTIME);
    for assembly in ["rootfs", "layers/distro", "scratch"] {
        unmounts.push(guest.text(&format!("run/nvx/{assembly}")));
    }
    assert_eq!(guest.kernel.unmounts(), unmounts);
}

#[test]
fn without_a_time_status_nothing_is_bound_at_run_nvx_time() {
    let (guest, mut root) = assembled();
    root.mount_runtime(None).unwrap();
    let calls = guest.kernel.calls();
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, Call::OpenTree { .. })),
        "{calls:?}"
    );
    let time = point(&guest, RuntimeMounts::TimeStatus);
    assert_eq!(guest.kernel.mount_at(&time), None);
    assert!(!guest.path("run/nvx/rootfs/run/nvx").exists());
    root.teardown().unwrap();
    assert_eq!(
        guest.kernel.unmounts()[..6],
        reversed(&guest, &RUNTIME[..6])
    );
}

#[test]
fn a_failed_mount_unmounts_what_the_call_mounted() {
    for (index, failing) in RUNTIME.into_iter().enumerate() {
        let (guest, mut root) = assembled();
        let target = point(&guest, failing);
        guest
            .kernel
            .fail(Operation::MoveMount, target, Errno::EPERM);
        let unmounted = guest.kernel.unmounts().len();
        let error = root.mount_runtime(Some(&guest.path("time"))).unwrap_err();
        assert!(
            matches!(
                &error,
                Error::Mount { mount: Mount::Runtime(mount), error }
                    if *mount == failing && error.raw_os_error() == Some(Errno::EPERM as i32)
            ),
            "{failing:?}: {error:?}"
        );
        assert_eq!(
            guest.kernel.unmounts()[unmounted..],
            reversed(&guest, &RUNTIME[..index]),
            "{failing:?}"
        );
        for mount in RUNTIME {
            let path = point(&guest, mount);
            assert_eq!(guest.kernel.mount_at(&path), None, "{mount:?}");
        }
        // The teardown has only the assembly's mounts left.
        root.teardown().unwrap();
        assert_eq!(guest.kernel.unmounts().len(), unmounted + index + 3);
    }
}

#[test]
fn a_failed_restriction_mounts_nothing_and_other_failed_steps_undo_the_call() {
    let (guest, mut root) = assembled();
    let rootfs = guest.path("run/nvx/rootfs");
    guest
        .kernel
        .fail(Operation::SetAttributes, rootfs, Errno::EPERM);
    let calls = guest.kernel.calls().len();
    let error = root.mount_runtime(None).unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to make the root's mounts nosuid and nodev: Operation not permitted (os error 1)"
    );
    assert_eq!(
        guest.kernel.calls()[calls..],
        [Call::RestrictTree(guest.text("run/nvx/rootfs"))]
    );

    // The device nodes, once `/dev` is mounted at the image's own directory.
    let (guest, mut root) = assembled();
    let dev = point(&guest, RuntimeMounts::Dev);
    fs::create_dir(&dev).unwrap();
    guest.kernel.fail(Operation::Create, &dev, Errno::EIO);
    let unmounted = guest.kernel.unmounts().len();
    let error = root.mount_runtime(None).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Mount { mount: Mount::Runtime(RuntimeMounts::Dev), error }
                if error.raw_os_error() == Some(Errno::EIO as i32)
        ),
        "{error:?}"
    );
    assert!(
        error
            .to_string()
            .starts_with("failed to mount the runtime's /dev: ")
    );
    let undone = reversed(&guest, &RUNTIME[..3]);
    assert_eq!(guest.kernel.unmounts()[unmounted..], undone);

    // A new mount, devpts's.
    let (guest, mut root) = assembled();
    guest
        .kernel
        .fail(Operation::NewMount, "devpts", Errno::ENODEV);
    let unmounted = guest.kernel.unmounts().len();
    let error = root.mount_runtime(None).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Mount {
                mount: Mount::Runtime(RuntimeMounts::DevPts),
                ..
            }
        ),
        "{error:?}"
    );
    let undone = reversed(&guest, &RUNTIME[..3]);
    assert_eq!(guest.kernel.unmounts()[unmounted..], undone);
}

#[test]
fn a_mount_point_through_a_symbolic_link_or_with_a_mount_already_is_refused() {
    // A link at `/run` is refused once the mounts before it are made, which the call undoes.
    let (guest, mut root) = assembled();
    let run = point(&guest, RuntimeMounts::Run);
    fs::create_dir(guest.path("elsewhere")).unwrap();
    std::os::unix::fs::symlink(guest.path("elsewhere"), &run).unwrap();
    let unmounted = guest.kernel.unmounts().len();
    let error = root.mount_runtime(None).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::MountPoint { path, error }
                if *path == run && error.raw_os_error() == Some(Errno::ELOOP as i32)
        ),
        "{error:?}"
    );
    let undone = reversed(&guest, &RUNTIME[..5]);
    assert_eq!(guest.kernel.unmounts()[unmounted..], undone);
    assert_eq!(fs::read_dir(guest.path("elsewhere")).unwrap().count(), 0);

    // A second call finds the runtime's mounts, and makes no call: it restricts no tree again.
    let (guest, mut root) = assembled();
    root.mount_runtime(None).unwrap();
    let calls = guest.kernel.calls().len();
    let error = root.mount_runtime(None).unwrap_err();
    let proc = point(&guest, RuntimeMounts::Proc);
    assert!(
        matches!(&error, Error::Occupied { path } if *path == proc),
        "{error:?}"
    );
    assert_eq!(guest.kernel.calls().len(), calls);
    root.teardown().unwrap();
    assert_eq!(guest.kernel.unmounts().len(), 6 + 3);
}

#[test]
fn a_mount_that_the_undo_fails_to_unmount_stays_for_the_teardown() {
    let (guest, mut root) = assembled();
    let pts = point(&guest, RuntimeMounts::DevPts);
    let run = point(&guest, RuntimeMounts::Run);
    guest.kernel.fail(Operation::MoveMount, run, Errno::EPERM);
    guest.kernel.fail(Operation::Unmount, &pts, Errno::EBUSY);
    let unmounted = guest.kernel.unmounts().len();
    root.mount_runtime(None).unwrap_err();
    let undone = reversed(&guest, &RUNTIME[..5]);
    assert_eq!(guest.kernel.unmounts()[unmounted..], undone);
    assert!(guest.kernel.mount_at(&pts).is_some());

    // A later call finds it, and makes no call.
    let calls = guest.kernel.calls().len();
    let error = root.mount_runtime(None).unwrap_err();
    assert!(
        matches!(&error, Error::Occupied { path } if *path == pts),
        "{error:?}"
    );
    assert_eq!(guest.kernel.calls().len(), calls);

    // The teardown unmounts it first.
    guest.kernel.restore(Operation::Unmount, &pts);
    let unmounted = guest.kernel.unmounts().len();
    root.teardown().unwrap();
    assert_eq!(
        guest.kernel.unmounts()[unmounted..],
        [
            pts.display().to_string(),
            guest.text("run/nvx/rootfs"),
            guest.text("run/nvx/layers/distro"),
            guest.text("run/nvx/scratch"),
        ]
    );
}

#[test]
fn a_runtime_mount_that_moves_is_detached_before_it_is_recorded() {
    for runtime in &RUNTIME[..6] {
        let (guest, mut root) = assembled();
        let target = point(&guest, *runtime);
        let moved = target.with_file_name("moved");
        let renamed = moved.clone();
        guest.kernel.on_move(move |path| {
            if path == target {
                fs::rename(&target, &renamed).unwrap();
                fs::create_dir(&target).unwrap();
            }
        });

        let mounted = root.mount_runtime(None);
        let refused = matches!(
            &mounted,
            Err(Error::Mount {
                mount: Mount::Runtime(found),
                ..
            }) if found == runtime
        );
        let after_call = guest.kernel.mount_at(&moved);
        let torn = root.teardown().map_err(|error| error.to_string());
        let after_teardown = guest.kernel.mount_at(&moved);
        assert_eq!(
            (refused, after_call, torn.is_ok(), after_teardown),
            (true, None, true, None),
            "{runtime:?}: {mounted:?}, {torn:?}"
        );
    }
}

#[test]
fn a_runtime_mount_whose_detach_fails_is_owned_until_teardown() {
    for runtime in [
        RuntimeMounts::Dev,
        RuntimeMounts::DevPts,
        RuntimeMounts::Run,
    ] {
        for persistent in [false, true] {
            let (guest, mut root) = assembled();
            let target = point(&guest, runtime);
            let original = target.clone();
            let moved = target.with_file_name("moved");
            let renamed = moved.clone();
            guest.kernel.fail(Operation::Detach, &moved, Errno::EBUSY);
            guest.kernel.on_move(move |path| {
                if path == target {
                    fs::rename(&target, &renamed).unwrap();
                    fs::create_dir(&target).unwrap();
                }
            });

            let error = root.mount_runtime(None).unwrap_err();
            assert!(
                matches!(
                    &error,
                    Error::Mount {
                        mount: Mount::Runtime(found),
                        ..
                    } if *found == runtime
                ),
                "{runtime:?}: {error:?}"
            );
            assert!(guest.kernel.mount_at(&moved).is_some());
            let calls = guest.kernel.calls().len();
            let error = root.mount_runtime(None).unwrap_err();
            assert!(
                matches!(&error, Error::Occupied { path } if *path == original),
                "{runtime:?}: {error:?}"
            );
            assert_eq!(guest.kernel.calls().len(), calls);
            if !persistent {
                guest.kernel.restore(Operation::Detach, &moved);
            }
            let torn = root.teardown();
            if persistent {
                let error = torn.unwrap_err();
                assert!(
                    error.failures.iter().any(|failure| {
                        failure.mount == Mount::Runtime(runtime)
                            && failure.error.raw_os_error() == Some(Errno::EBUSY as i32)
                    }),
                    "{runtime:?}: {error:?}"
                );
            } else {
                torn.unwrap();
            }
            assert_eq!(guest.kernel.mount_at(&moved).is_some(), persistent);
            let detaches = guest
                .kernel
                .calls()
                .into_iter()
                .filter(|call| matches!(call, Call::Detach(_)))
                .count();
            assert_eq!(detaches, 3, "{runtime:?}");
        }
    }
}

#[test]
fn a_time_status_bind_that_moves_and_cannot_detach_stays_owned_until_teardown() {
    let (guest, mut root) = assembled();
    let time = point(&guest, RuntimeMounts::TimeStatus);
    let parent = time.parent().unwrap().to_path_buf();
    let moved_parent = guest.path("run/nvx/rootfs/run/moved");
    let moved = moved_parent.join("time");
    guest.kernel.fail(Operation::Detach, &moved, Errno::EBUSY);
    guest.kernel.on_move(move |target| {
        if target == time {
            fs::rename(&parent, &moved_parent).unwrap();
        }
    });

    let error = root.mount_runtime(Some(&guest.path("time"))).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Mount {
                mount: Mount::Runtime(RuntimeMounts::TimeStatus),
                ..
            }
        ),
        "{error:?}"
    );
    assert!(guest.kernel.mount_at(&moved).is_some());
    let calls = guest.kernel.calls().len();
    assert!(matches!(
        root.mount_runtime(None),
        Err(Error::Occupied { .. })
    ));
    assert_eq!(guest.kernel.calls().len(), calls);
    guest.kernel.restore(Operation::Detach, &moved);
    root.teardown().unwrap();
    assert_eq!(guest.kernel.mount_at(&moved), None);
    let detaches = guest
        .kernel
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Detach(_)))
        .count();
    assert_eq!(detaches, 3);
}

#[test]
fn a_time_status_bind_whose_lookup_and_detach_fail_reports_its_teardown_failure() {
    let (guest, mut root) = assembled();
    let time = point(&guest, RuntimeMounts::TimeStatus);
    guest.kernel.fail(Operation::Detach, &time, Errno::EBUSY);
    let kernel = guest.kernel.clone();
    let target = time.clone();
    guest.kernel.on_move(move |path| {
        if path == target {
            kernel.fail(Operation::Status, &target, Errno::EIO);
        }
    });

    root.mount_runtime(Some(&guest.path("time"))).unwrap_err();
    let error = root.teardown().unwrap_err();
    assert!(
        error.failures.iter().any(|failure| {
            failure.mount == Mount::Runtime(RuntimeMounts::TimeStatus)
                && failure.error.raw_os_error() == Some(Errno::EBUSY as i32)
        }),
        "{error:?}"
    );
    assert!(guest.kernel.mount_at(&time).is_some());
}

#[test]
fn a_call_after_a_share_or_a_child_is_refused_before_it_changes_anything() {
    let rw = Access::ReadWrite;
    // The `/run` tmpfs would hide one beneath `/run` from the teardown, and the runtime comes
    // first whatever the target.
    for (kind, target) in [("share", "/run/app"), ("child", "/run/app")] {
        let (guest, mut root) = assembled();
        let path = guest.path("run/nvx/rootfs").join(&target[1..]);
        let mut unmounts = vec![path.display().to_string()];
        match kind {
            "share" => root.mount_share("app", Path::new(target), rw).unwrap(),
            _ => {
                export(&guest, &[("app", ShareKind::Directory)]);
                let children = [child("app", target, rw, ShareKind::Directory)];
                root.mount_aggregate("all", rw, &children).unwrap();
                unmounts.push(guest.text("run/nvx/shares"));
            }
        }
        let calls = guest.kernel.calls().len();
        let unmounted = guest.kernel.unmounts().len();
        let first = root.mount_runtime(Some(&guest.path("time")));
        let second = root.mount_runtime(None);
        let made = guest.kernel.calls().len() - calls;
        // What the call was refused for is still the topmost mount at its target.
        let shown = guest.kernel.mount_at(&path).is_some();
        let torn = root.teardown().map_err(|error| error.to_string());
        let observed = (
            first.map_err(|error| error.to_string()),
            second.map_err(|error| error.to_string()),
            made,
            shown,
            torn,
            guest.kernel.unmounts()[unmounted..].to_vec(),
        );
        let expected = (
            occupied(&path),
            occupied(&path),
            0,
            true,
            Ok(()),
            then_the_assembly(&guest, unmounts),
        );
        assert_eq!(observed, expected, "{kind}");
    }
}

#[test]
fn shares_and_children_beneath_run_after_the_runtime_are_on_its_tmpfs_and_unmounted_first() {
    let (guest, mut root) = assembled();
    root.mount_runtime(Some(&guest.path("time"))).unwrap();
    let unmounted = guest.kernel.unmounts().len();
    // The mapper creates what the targets lack on the `/run` tmpfs, one of its own mounts.
    let shared = root.mount_share("app", Path::new("/run/app"), Access::ReadWrite);
    export(&guest, &[("state", ShareKind::Directory)]);
    let children = [child(
        "state",
        "/run/nvx/state",
        Access::ReadOnly,
        ShareKind::Directory,
    )];
    let bound = root.mount_aggregate("all", Access::ReadOnly, &children);
    let tracked = targets(&root);
    // A second call is still refused for the runtime's own mounts first.
    let again = root.mount_runtime(None);
    let torn = root.teardown().map_err(|error| error.to_string());
    let observed = (
        shared.map_err(|error| error.to_string()),
        bound.map_err(|error| error.to_string()),
        tracked,
        again.map_err(|error| error.to_string()),
        torn,
        guest.kernel.unmounts()[unmounted..].to_vec(),
    );
    // The teardown unmounts the mapper's mounts, the later first, then the aggregate, before the
    // runtime's.
    let mut unmounts = vec![
        guest.text("run/nvx/rootfs/run/nvx/state"),
        guest.text("run/nvx/rootfs/run/app"),
        guest.text("run/nvx/shares"),
    ];
    unmounts.extend(reversed(&guest, &RUNTIME));
    let expected = (
        Ok(()),
        Ok(()),
        vec!["/run/app".to_owned(), "/run/nvx/state".to_owned()],
        occupied(&point(&guest, RuntimeMounts::Proc)),
        Ok(()),
        then_the_assembly(&guest, unmounts),
    );
    assert_eq!(observed, expected);
}

#[test]
fn a_run_that_the_mapper_fails_to_add_latches_the_root_and_one_that_an_undo_left_is_its_own() {
    // A `/run` whose status fails stops the mapper's opening: nothing is mounted for the share,
    // and the root is latched.
    let (guest, mut root) = assembled();
    root.mount_runtime(None).unwrap();
    let run = point(&guest, RuntimeMounts::Run);
    guest.kernel.fail(Operation::Status, &run, Errno::EIO);
    let calls = guest.kernel.calls().len();
    let unmounted = guest.kernel.unmounts().len();
    let error = root
        .mount_share("app", Path::new("/run/app"), Access::ReadWrite)
        .unwrap_err();
    let later = root
        .mount_share("t", Path::new("/srv"), Access::ReadWrite)
        .unwrap_err();
    let observed = (
        matches!(&error, Error::MountPoint { path, error }
            if *path == run && error.raw_os_error() == Some(Errno::EIO as i32)),
        matches!(
            later,
            Error::Mapping {
                error: MappingError::Latched,
                ..
            }
        ),
        root.mapper.is_some(),
        guest.kernel.calls().len() - calls,
    );
    assert_eq!(observed, (true, true, false, 0), "{error:?}");
    root.teardown().unwrap();
    assert_eq!(
        guest.kernel.unmounts()[unmounted..],
        then_the_assembly(&guest, reversed(&guest, &RUNTIME[..6]))
    );

    // A `/run` that a failed undo left is the mapper's: a share beneath it is mounted there, and
    // the teardown unmounts the share first.
    let (guest, mut root) = assembled();
    let run = point(&guest, RuntimeMounts::Run);
    let time = point(&guest, RuntimeMounts::TimeStatus);
    guest.kernel.fail(Operation::MoveMount, &time, Errno::EPERM);
    guest.kernel.fail(Operation::Unmount, &run, Errno::EBUSY);
    root.mount_runtime(Some(&guest.path("time"))).unwrap_err();
    assert!(guest.kernel.mount_at(&run).is_some());
    guest.kernel.restore(Operation::Unmount, &run);
    let unmounted = guest.kernel.unmounts().len();
    let shared = root.mount_share("app", Path::new("/run/app"), Access::ReadWrite);
    // A later call is refused for that `/run`, the runtime's mount, and makes no call.
    let calls = guest.kernel.calls().len();
    let again = root.mount_runtime(None);
    let made = guest.kernel.calls().len() - calls;
    let torn = root.teardown().map_err(|error| error.to_string());
    let observed = (
        shared.map_err(|error| error.to_string()),
        again.map_err(|error| error.to_string()),
        made,
        torn,
        guest.kernel.unmounts()[unmounted..].to_vec(),
    );
    let unmounts = vec![
        guest.text("run/nvx/rootfs/run/app"),
        run.display().to_string(),
    ];
    let expected = (
        Ok(()),
        occupied(&run),
        0,
        Ok(()),
        then_the_assembly(&guest, unmounts),
    );
    assert_eq!(observed, expected);
}
