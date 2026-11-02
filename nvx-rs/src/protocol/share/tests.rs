use super::*;

#[test]
fn the_reserved_targets_and_characters_are_pinned() {
    assert_eq!(
        RESERVED_MOUNT_TARGETS,
        [
            "/proc",
            "/sys",
            "/dev",
            "/.nvx-agent",
            "/etc/machine-id",
            "/etc/resolv.conf",
            "/run/nvx/time"
        ]
    );
    assert_eq!(RESERVED_EXACT_MOUNT_TARGETS, ["/etc", "/run", "/run/nvx"]);
    assert_eq!(RESERVED_TARGET_CHARACTERS, "\0\\=,\"");
}

#[test]
fn a_target_is_reserved_in_a_tree_or_at_an_exact_path() {
    for (target, tree) in [
        ("/proc", "/proc"),
        ("/proc/self", "/proc"),
        ("/sys/fs/cgroup", "/sys"),
        ("/dev/shm", "/dev"),
        ("/.nvx-agent/tool", "/.nvx-agent"),
        ("/etc/machine-id", "/etc/machine-id"),
        ("/etc/machine-id/x", "/etc/machine-id"),
        ("/etc/resolv.conf", "/etc/resolv.conf"),
        ("/etc/resolv.conf/x", "/etc/resolv.conf"),
        ("/run/nvx/time", "/run/nvx/time"),
        ("/run/nvx/time/state", "/run/nvx/time"),
    ] {
        assert_eq!(
            reservation(target.as_bytes()),
            Some(Reservation::Tree(tree)),
            "{target}"
        );
    }
    for target in ["/etc", "/run", "/run/nvx"] {
        assert_eq!(
            reservation(target.as_bytes()),
            Some(Reservation::Exact),
            "{target}"
        );
    }
    for target in [
        "/workspace",
        "/procs",
        "/devices",
        "/.nvx-agents",
        "/etc/app",
        "/etc/machine-ids",
        "/etc/resolv.conf.d",
        "/etc/resolvconf",
        "/run/app",
        "/run/nvx/state",
        "/run/nvx/timer",
        "/",
        "",
    ] {
        assert_eq!(reservation(target.as_bytes()), None, "{target}");
    }
}
