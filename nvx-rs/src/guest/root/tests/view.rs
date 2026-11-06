use super::*;
use crate::guest::view::{self as vm_view, MACHINE_ID_FILE, RESOLVER_FILE};
use crate::{
    ErrorKind,
    activation::{MachineIdMode, View},
};

/// Returns a root and its view files, with no view bound yet.
fn prepared() -> (Guest, Root, MachineId, PathBuf) {
    let guest = Guest::new();
    let root = guest.assemble(&guest.assembly()).unwrap();
    fs::create_dir_all(root.path().join("etc")).unwrap();
    fs::write(root.path().join("etc/machine-id"), b"image\n").unwrap();
    let machine_id = MachineId::generate(root.runtime().join(MACHINE_ID_FILE)).unwrap();
    let resolver = root.runtime().join(RESOLVER_FILE);
    vm_view::write_resolver_file(&resolver, "10.0.0.1".parse().unwrap()).unwrap();
    (guest, root, machine_id, resolver)
}

#[test]
fn the_root_removes_its_view_binds_in_reverse_order_before_the_overlay() {
    let (guest, mut root, machine_id, resolver) = prepared();
    root.begin_view().unwrap();
    root.bind_view(&machine_id, Some(&resolver)).unwrap();
    for target in ["etc/machine-id", "etc/resolv.conf"] {
        assert!(guest.kernel.mount_at(&root.path().join(target)).is_some());
    }
    root.teardown().unwrap();
    assert_eq!(
        guest.kernel.unmounts(),
        [
            "rootfs/etc/resolv.conf",
            "rootfs/etc/machine-id",
            "rootfs",
            "layers/distro",
            "scratch",
        ]
        .map(|path| guest.text(&format!("run/nvx/{path}")))
    );
}

#[test]
fn image_mode_without_dns_binds_nothing_and_a_second_view_is_refused() {
    let (guest, mut root, _, _) = prepared();
    root.begin_view().unwrap();
    let calls = guest.kernel.calls().len();
    root.bind_view(&MachineId::Image, None).unwrap();
    assert_eq!(guest.kernel.calls().len(), calls);
    assert_eq!(
        root.begin_view().unwrap_err().kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(guest.kernel.calls().len(), calls);
    root.teardown().unwrap();
}

#[test]
fn invalid_hostname_bytes_are_refused_before_any_view_effect() {
    let (guest, mut root, _, _) = prepared();
    for hostname in [Vec::new(), vec![b'h'; 65], b"bad\0name".to_vec()] {
        let view = View {
            hostname,
            machine_id: MachineIdMode::Generated,
        };
        let calls = guest.kernel.calls().len();
        assert_eq!(
            vm_view::apply(&mut root, &view, None).unwrap_err().kind(),
            ErrorKind::Unsupported
        );
        assert!(root.view_binds.is_none());
        assert_eq!(guest.kernel.calls().len(), calls);
    }
    root.teardown().unwrap();
}

#[test]
fn a_later_bind_failure_stays_owned_and_its_teardown_removes_the_earlier_bind() {
    let (guest, mut root, machine_id, resolver) = prepared();
    let target = root.path().join("etc/resolv.conf");
    guest
        .kernel
        .fail(Operation::MoveMount, &target, Errno::EPERM);
    root.begin_view().unwrap();
    assert_eq!(
        root.bind_view(&machine_id, Some(&resolver))
            .unwrap_err()
            .kind(),
        ErrorKind::Io
    );
    assert!(root.is_latched());
    let machine = root.path().join("etc/machine-id");
    assert!(guest.kernel.mount_at(&machine).is_some());
    assert!(guest.kernel.mount_at(&target).is_none());
    root.teardown().unwrap();
    assert!(guest.kernel.mount_at(&machine).is_none());
}

#[test]
fn a_switched_root_leaves_its_view_binds_mounted() {
    for teardown in [false, true] {
        let (guest, mut root, machine_id, resolver) = prepared();
        root.begin_view().unwrap();
        root.bind_view(&machine_id, Some(&resolver)).unwrap();
        let targets = ["etc/machine-id", "etc/resolv.conf"].map(|target| root.path().join(target));
        root.switch().unwrap();
        if teardown {
            assert!(root.teardown().unwrap_err().is_switched());
        } else {
            drop(root);
        }
        assert!(guest.kernel.unmounts().is_empty());
        for target in targets {
            assert!(guest.kernel.mount_at(&target).is_some());
        }
    }
}

#[test]
fn teardown_reports_each_view_bind_that_stays_mounted() {
    let (guest, mut root, machine_id, resolver) = prepared();
    root.begin_view().unwrap();
    root.bind_view(&machine_id, Some(&resolver)).unwrap();
    for target in ["etc/machine-id", "etc/resolv.conf"] {
        guest
            .kernel
            .fail(Operation::Unmount, root.path().join(target), Errno::EBUSY);
    }
    let error = root.teardown().unwrap_err();
    let paths: Vec<_> = error
        .failures()
        .iter()
        .map(|failure| failure.path().to_path_buf())
        .collect();
    assert_eq!(
        paths,
        ["etc/resolv.conf", "etc/machine-id"]
            .map(|target| guest.path("run/nvx/rootfs").join(target))
    );
}

#[test]
fn a_view_bind_that_moves_and_cannot_detach_stays_owned_until_teardown() {
    for persistent in [false, true] {
        let (guest, mut root, _, resolver) = prepared();
        let target = root.path().join("etc/resolv.conf");
        let parent = target.parent().unwrap().to_path_buf();
        let moved_parent = root.path().join("moved");
        let moved = moved_parent.join("resolv.conf");
        guest.kernel.fail(Operation::Detach, &moved, Errno::EBUSY);
        guest.kernel.on_move(move |path| {
            if path == target {
                fs::rename(&parent, &moved_parent).unwrap();
            }
        });
        root.begin_view().unwrap();
        let error = root
            .bind_view(&MachineId::Image, Some(&resolver))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io);
        assert!(root.view_stray.is_some());
        assert!(guest.kernel.mount_at(&moved).is_some());
        if !persistent {
            guest.kernel.restore(Operation::Detach, &moved);
        }
        let torn = root.teardown();
        if persistent {
            let error = torn.unwrap_err();
            assert!(
                error.failures().iter().any(|failure| {
                    failure.path() == guest.path("run/nvx/rootfs/etc/resolv.conf")
                        && failure.error().raw_os_error() == Some(Errno::EBUSY as i32)
                }),
                "{error:?}"
            );
        } else {
            torn.unwrap();
        }
        assert_eq!(guest.kernel.mount_at(&moved).is_some(), persistent);
    }
}
