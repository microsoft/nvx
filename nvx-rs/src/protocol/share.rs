//! The guest targets that a sandbox's live shares may not take: the paths at which the guest's
//! container runtime mounts, binds, or creates directories in the container root, and the
//! characters that no target may hold.
//!
//! The host refuses such a target before it attaches a share, and the guest again before it
//! mounts one, with [`reservation`] and [`RESERVED_TARGET_CHARACTERS`].

/// The guest trees that the container runtime owns, which a share may neither be in nor contain:
/// it mounts procfs, sysfs, and a private `/dev` over them, stages its tools under
/// `/.nvx-agent`, binds the workload machine ID at `/etc/machine-id`, which it first replaces
/// with a regular file, binds the guest's resolver file at `/etc/resolv.conf`, which it first
/// replaces with a regular file if it is a symbolic link, and binds its time ABI state over
/// `/run/nvx/time`.
pub const RESERVED_MOUNT_TARGETS: [&str; 7] = [
    "/proc",
    "/sys",
    "/dev",
    "/.nvx-agent",
    "/etc/machine-id",
    "/etc/resolv.conf",
    "/run/nvx/time",
];

/// The guest paths at which a share may not be, though it may be below them: `/etc`, and `/run`
/// and `/run/nvx`, which the container runtime creates below the container root.
pub const RESERVED_EXACT_MOUNT_TARGETS: [&str; 3] = ["/etc", "/run", "/run/nvx"];

/// The characters that a share's guest target may not hold: NUL, `\`, `=`, and `,`.
pub const RESERVED_TARGET_CHARACTERS: &str = "\0\\=,";

/// Why the container runtime reserves a guest target, as [`reservation`] tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reservation {
    /// The target is in the tree of this one of the [`RESERVED_MOUNT_TARGETS`]: it is that path,
    /// or is below it.
    Tree(&'static str),
    /// The target is one of the [`RESERVED_EXACT_MOUNT_TARGETS`].
    Exact,
}

/// Returns why the container runtime reserves the guest target `target`, if it does: because it
/// is in one of the [`RESERVED_MOUNT_TARGETS`] trees, or is one of the
/// [`RESERVED_EXACT_MOUNT_TARGETS`].
///
/// The target is compared as written, so the caller checks first that it is absolute and
/// canonical: `/proc/` and `/run/./nvx` are not reserved here.
pub fn reservation(target: &[u8]) -> Option<Reservation> {
    let tree = RESERVED_MOUNT_TARGETS.into_iter().find(|tree| {
        target
            .strip_prefix(tree.as_bytes())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/"))
    });
    match tree {
        Some(tree) => Some(Reservation::Tree(tree)),
        None => RESERVED_EXACT_MOUNT_TARGETS
            .iter()
            .any(|exact| target == exact.as_bytes())
            .then_some(Reservation::Exact),
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(RESERVED_TARGET_CHARACTERS, "\0\\=,");
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
}
