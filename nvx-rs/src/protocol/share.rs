//! A sandbox's live shares, the host directories and files that OpenVMM exports through one
//! virtio-fs device: the kernel parameters that announce them to the guest, and the guest targets
//! that they may not take.
//!
//! OpenVMM appends `virtfs_dir=`, `virtfs_tag=`, and `virtfs_mode=` parameters for its device,
//! which name the guest directory at which the guest mounts it, its tag, and its mode, `ro` or
//! `rw`, as [`share_tokens`] writes them. When the device exports several host directories, or a
//! file, as the children of an aggregate, OpenVMM appends `virtfs_aggregate=1`, and the guest
//! mounts the aggregate at [`AGGREGATE_MOUNT_TARGET`]. The host then appends a [`CHILD_TOKEN`]
//! parameter for each child, which [`child_token`] writes, to name the target at which the guest
//! binds the child. The guest reads them all with [`Shares::parse`].
//!
//! The container runtime reserves the paths at which it mounts, binds, or creates directories in
//! the container root, and no target may hold certain characters. The host refuses such a target
//! before it attaches a share, and the guest again before it mounts one, with [`reservation`] and
//! [`RESERVED_TARGET_CHARACTERS`].

use super::{ProtocolError, violation};

/// The guest directory at which the guest mounts the aggregate of several shares. The
/// aggregate's root lists every child, so it stays outside the container root.
pub const AGGREGATE_MOUNT_TARGET: &str = "/run/nvx/shares";

/// The prefix of the kernel parameter that names where the guest binds a child of an aggregate,
/// `nvx_share=NAME,TARGET,MODE[,file]`, which [`child_token`] writes and [`Child::parse`] reads.
pub const CHILD_TOKEN: &str = "nvx_share=";

/// The prefixes of the kernel parameters that OpenVMM appends for its virtio-fs device: the guest
/// directory at which the guest mounts it, its tag, and its mode.
const SHARE_TOKENS: [&str; 3] = ["virtfs_dir=", "virtfs_tag=", "virtfs_mode="];
/// The kernel parameter that OpenVMM appends after those of an aggregate.
const AGGREGATE_TOKEN: &str = "virtfs_aggregate=1";
/// The mode of a read-only share.
const READ_ONLY: &str = "ro";
/// The mode of a writable share.
const READ_WRITE: &str = "rw";
/// The end of the value of a child's parameter that marks a child that is a host file.
const FILE_SUFFIX: &str = ",file";

/// The guest trees that the container runtime owns, which a share may neither be in nor contain:
/// it mounts procfs, sysfs, and a private `/dev` over them, stages its tools under
/// `/.nvx-agent`, binds the workload machine ID at `/etc/machine-id`, which it first replaces
/// with a regular file, and binds its time ABI state over `/run/nvx/time`.
pub const RESERVED_MOUNT_TARGETS: [&str; 6] = [
    "/proc",
    "/sys",
    "/dev",
    "/.nvx-agent",
    "/etc/machine-id",
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

/// Returns the mode of a share in its kernel parameters: `ro` if it is read-only, and `rw`
/// otherwise.
pub const fn mode(read_only: bool) -> &'static str {
    if read_only { READ_ONLY } else { READ_WRITE }
}

/// Returns the kernel parameters that OpenVMM appends for its virtio-fs device with the tag
/// `tag`, which the guest mounts at `directory`, read-only if `read_only`: `virtfs_dir=`,
/// `virtfs_tag=`, and `virtfs_mode=`, and then `virtfs_aggregate=1` if the device exports an
/// aggregate.
pub fn share_tokens(directory: &str, tag: &str, read_only: bool, aggregate: bool) -> String {
    let [directory_token, tag_token, mode_token] = SHARE_TOKENS;
    let mut tokens = format!(
        "{directory_token}{directory} {tag_token}{tag} {mode_token}{}",
        mode(read_only)
    );
    if aggregate {
        tokens.push(' ');
        tokens.push_str(AGGREGATE_TOKEN);
    }
    tokens
}

/// Returns the kernel parameter that tells the guest to bind the child `name` of an aggregate at
/// `target`, read-only if `read_only`: `nvx_share=NAME,TARGET,MODE`, and then `,file` if the
/// child is a host file.
pub fn child_token(name: &str, target: &str, read_only: bool, file: bool) -> String {
    let file = if file { FILE_SUFFIX } else { "" };
    format!("{CHILD_TOKEN}{name},{target},{}{file}", mode(read_only))
}

/// The live shares that a kernel command line announces, which [`Shares::parse`] reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shares<'a> {
    /// No live share.
    None,
    /// One host directory, which the guest mounts at its target.
    Single {
        /// The virtio-fs tag.
        tag: &'a str,
        /// The guest directory at which the guest mounts the share.
        target: &'a [u8],
        /// Whether the share is read-only.
        read_only: bool,
    },
    /// An aggregate of host directories and files, which the guest mounts at
    /// [`AGGREGATE_MOUNT_TARGET`] before it binds each child at its own target.
    Aggregate {
        /// The virtio-fs tag.
        tag: &'a str,
        /// Whether the aggregate is read-only, before its children's own modes.
        read_only: bool,
        /// The children, in the command line's order.
        children: Vec<Child<'a>>,
    },
}

impl<'a> Shares<'a> {
    /// Reads the live shares that a kernel command line announces, such as the guest's
    /// `/proc/cmdline`, so that the guest checks every share before it mounts any. The command
    /// line splits at spaces, tabs, and newlines, without quoting, as the guest's init splits
    /// it, and its parameters may come in any order.
    ///
    /// A command line without share parameters announces no share. Otherwise `virtfs_dir=`,
    /// `virtfs_tag=`, and `virtfs_mode=` must each appear once, with a value; the tag holds only
    /// ASCII letters, digits, `_`, and `-`, and the mode is `ro` or `rw`. Only an aggregate may
    /// have children, and it must have one at least, be mounted at [`AGGREGATE_MOUNT_TARGET`],
    /// and name each child once, as [`Child::parse`] reads it.
    ///
    /// Fails with a [`ProtocolError`] that describes the first violation: an empty share
    /// parameter, as the command line orders them, then a missing or repeated one, and then, for
    /// one share, the children, the tag, and the mode, or, for an aggregate, its directory, its
    /// tag, its mode, its lack of children, and each child in turn.
    pub fn parse(command_line: &'a [u8]) -> Result<Self, ProtocolError> {
        const INCOMPLETE: &str = "live share bootstrap is incomplete";
        let mut fields = [None; 3];
        let mut count = 0;
        let mut aggregate = false;
        let mut children = Vec::new();
        for token in command_line.split(|byte| matches!(byte, b' ' | b'\t' | b'\n')) {
            let field = SHARE_TOKENS
                .iter()
                .enumerate()
                .find_map(|(index, prefix)| Some((index, token.strip_prefix(prefix.as_bytes())?)));
            if let Some((index, value)) = field {
                if value.is_empty() {
                    return Err(violation(INCOMPLETE));
                }
                fields[index] = Some(value);
                count += 1;
            } else if token == AGGREGATE_TOKEN.as_bytes() {
                aggregate = true;
            } else if let Some(child) = token.strip_prefix(CHILD_TOKEN.as_bytes()) {
                children.push(child);
            }
        }
        if count == 0 && !aggregate && children.is_empty() {
            return Ok(Self::None);
        }
        let ([Some(directory), Some(tag), Some(mode)], 3) = (fields, count) else {
            return Err(violation(INCOMPLETE));
        };
        if !aggregate {
            if !children.is_empty() {
                return Err(violation("live share children require an aggregate share"));
            }
            return Ok(Self::Single {
                tag: parse_tag(tag)?,
                target: directory,
                read_only: parse_mode(mode)?,
            });
        }
        if directory != AGGREGATE_MOUNT_TARGET.as_bytes() {
            return Err(violation(format!(
                "the aggregate live share must be mounted at {AGGREGATE_MOUNT_TARGET}"
            )));
        }
        let tag = parse_tag(tag)?;
        let read_only = parse_mode(mode)?;
        if children.is_empty() {
            return Err(violation("the aggregate live share has no children"));
        }
        let mut parsed: Vec<Child<'a>> = Vec::with_capacity(children.len());
        for value in children {
            let child = Child::parse(value)?;
            if parsed.iter().any(|earlier| earlier.name == child.name) {
                return Err(violation(format!(
                    "live share child is not unique: {}",
                    child.name
                )));
            }
            parsed.push(child);
        }
        Ok(Self::Aggregate {
            tag,
            read_only,
            children: parsed,
        })
    }
}

/// A child of an aggregate, as its [`CHILD_TOKEN`] parameter names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Child<'a> {
    /// The child's name in the aggregate: ASCII letters, digits, `.`, `_`, and `-`, and neither
    /// `.` nor `..`.
    pub name: &'a str,
    /// The guest target at which the guest binds the child, which the guest checks before it
    /// binds it.
    pub target: &'a [u8],
    /// Whether the child is read-only.
    pub read_only: bool,
    /// Whether the child is a host file, rather than a directory.
    pub file: bool,
}

impl<'a> Child<'a> {
    /// Reads the value of a child's [`CHILD_TOKEN`] parameter, `NAME,TARGET,MODE[,file]`: the
    /// name before the first comma, and, before a trailing `,file`, which marks a file, the mode
    /// after the last comma, with the target between them.
    ///
    /// Fails with a [`ProtocolError`] if a field is missing, if the name is invalid, or if the
    /// mode is neither `ro` nor `rw`.
    pub fn parse(value: &'a [u8]) -> Result<Self, ProtocolError> {
        let invalid = || {
            violation(format!(
                "live share child is invalid: {}",
                String::from_utf8_lossy(value)
            ))
        };
        let comma = value
            .iter()
            .position(|&byte| byte == b',')
            .ok_or_else(invalid)?;
        let mut rest = &value[comma + 1..];
        let file = match rest.strip_suffix(FILE_SUFFIX.as_bytes()) {
            Some(directory) => {
                rest = directory;
                true
            }
            None => false,
        };
        let last = rest
            .iter()
            .rposition(|&byte| byte == b',')
            .ok_or_else(invalid)?;
        let name = match std::str::from_utf8(&value[..comma]) {
            Ok(name)
                if !matches!(name, "" | "." | "..")
                    && name.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                    }) =>
            {
                name
            }
            _ => return Err(invalid()),
        };
        Ok(Self {
            name,
            target: &rest[..last],
            read_only: parse_mode(&rest[last + 1..])?,
            file,
        })
    }
}

/// Reads a share's virtio-fs tag: one or more ASCII letters, digits, `_`, and `-`.
fn parse_tag(tag: &[u8]) -> Result<&str, ProtocolError> {
    match std::str::from_utf8(tag) {
        Ok(tag)
            if !tag.is_empty()
                && tag
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) =>
        {
            Ok(tag)
        }
        _ => Err(violation("live share tag is invalid")),
    }
}

/// Reads a share's mode, and returns whether it is read-only.
fn parse_mode(mode: &[u8]) -> Result<bool, ProtocolError> {
    if mode == READ_ONLY.as_bytes() {
        Ok(true)
    } else if mode == READ_WRITE.as_bytes() {
        Ok(false)
    } else {
        Err(violation(format!(
            "live share mode is unsupported: {}",
            String::from_utf8_lossy(mode)
        )))
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
            "/run/app",
            "/run/nvx/state",
            "/run/nvx/timer",
            "/",
            "",
        ] {
            assert_eq!(reservation(target.as_bytes()), None, "{target}");
        }
    }

    #[test]
    fn the_share_parameters_are_pinned() {
        assert_eq!(AGGREGATE_MOUNT_TARGET, "/run/nvx/shares");
        assert_eq!(CHILD_TOKEN, "nvx_share=");
        assert_eq!((mode(true), mode(false)), ("ro", "rw"));
        assert_eq!(
            share_tokens("/workspace", "microvm", false, false),
            "virtfs_dir=/workspace virtfs_tag=microvm virtfs_mode=rw"
        );
        assert_eq!(
            share_tokens(AGGREGATE_MOUNT_TARGET, "microvm", true, true),
            "virtfs_dir=/run/nvx/shares virtfs_tag=microvm virtfs_mode=ro virtfs_aggregate=1"
        );
        assert_eq!(
            child_token("0-a", "/workspace", false, false),
            "nvx_share=0-a,/workspace,rw"
        );
        assert_eq!(
            child_token("1-b", "/etc/app/settings.json", true, true),
            "nvx_share=1-b,/etc/app/settings.json,ro,file"
        );
    }

    #[test]
    fn the_guest_reads_the_shares_that_the_host_and_openvmm_announce() {
        assert_eq!(
            Shares::parse(b"console=hvc0 quiet nvx_sandbox=1\n"),
            Ok(Shares::None)
        );
        let single = share_tokens("/workspace", "microvm", false, false);
        assert_eq!(
            Shares::parse(format!("console=hvc0 {single}\n").as_bytes()),
            Ok(Shares::Single {
                tag: "microvm",
                target: b"/workspace",
                read_only: false,
            })
        );

        // The parameters may come in any order, apart at spaces, tabs, or newlines.
        let children = [
            ("0-a", "/workspace", false, false),
            ("1-b", "/srv/cache", true, false),
            ("2-c", "/etc/app/settings.json", true, true),
            ("3-d", "/results", false, true),
        ];
        let mut tokens: Vec<String> = children
            .iter()
            .map(|&(name, target, read_only, file)| child_token(name, target, read_only, file))
            .collect();
        let aggregate = share_tokens(AGGREGATE_MOUNT_TARGET, "microvm", false, true);
        tokens.insert(2, aggregate.replace(' ', "\t"));
        assert_eq!(
            Shares::parse(tokens.join("\n").as_bytes()),
            Ok(Shares::Aggregate {
                tag: "microvm",
                read_only: false,
                children: children
                    .iter()
                    .map(|&(name, target, read_only, file)| Child {
                        name,
                        target: target.as_bytes(),
                        read_only,
                        file,
                    })
                    .collect(),
            })
        );
    }

    #[test]
    fn malformed_share_parameters_are_refused_in_order() {
        const SINGLE: [&str; 3] = [
            "virtfs_dir=/workspace",
            "virtfs_tag=microvm",
            "virtfs_mode=rw",
        ];
        const AGGREGATE: [&str; 4] = [
            "virtfs_dir=/run/nvx/shares",
            "virtfs_tag=microvm",
            "virtfs_mode=rw",
            "virtfs_aggregate=1",
        ];
        let incomplete = "live share bootstrap is incomplete";
        for (tokens, message) in [
            (vec!["virtfs_dir="], incomplete),
            (vec!["virtfs_tag=a.b", "virtfs_dir="], incomplete),
            (
                vec!["virtfs_dir=/workspace", "virtfs_tag=microvm"],
                incomplete,
            ),
            ([&SINGLE[..], &["virtfs_dir=/again"]].concat(), incomplete),
            (vec!["virtfs_aggregate=1"], incomplete),
            (vec!["nvx_share=a,/b,rw"], incomplete),
            (
                [&SINGLE[..], &["nvx_share=a,/b,rw"]].concat(),
                "live share children require an aggregate share",
            ),
            (
                vec!["virtfs_dir=/w", "virtfs_tag=a.b", "virtfs_mode=rx"],
                "live share tag is invalid",
            ),
            (
                vec!["virtfs_dir=/w", "virtfs_tag=t", "virtfs_mode=rx"],
                "live share mode is unsupported: rx",
            ),
            (
                vec![
                    "virtfs_dir=/workspace",
                    "virtfs_tag=a.b",
                    "virtfs_mode=rw",
                    "virtfs_aggregate=1",
                    "nvx_share=a,/b,rw",
                ],
                "the aggregate live share must be mounted at /run/nvx/shares",
            ),
            (
                vec![
                    "virtfs_dir=/run/nvx/shares",
                    "virtfs_tag=a.b",
                    "virtfs_mode=rx",
                    "virtfs_aggregate=1",
                ],
                "live share tag is invalid",
            ),
            (
                vec![
                    "virtfs_dir=/run/nvx/shares",
                    "virtfs_tag=microvm",
                    "virtfs_mode=rx",
                    "virtfs_aggregate=1",
                ],
                "live share mode is unsupported: rx",
            ),
            (
                AGGREGATE.to_vec(),
                "the aggregate live share has no children",
            ),
            (
                [&AGGREGATE[..], &["nvx_share=nocomma"]].concat(),
                "live share child is invalid: nocomma",
            ),
            (
                [&AGGREGATE[..], &["nvx_share=a,/b"]].concat(),
                "live share child is invalid: a,/b",
            ),
            (
                [&AGGREGATE[..], &["nvx_share=a,/b,file"]].concat(),
                "live share child is invalid: a,/b,file",
            ),
            (
                [&AGGREGATE[..], &["nvx_share=..,/b,rw"]].concat(),
                "live share child is invalid: ..,/b,rw",
            ),
            (
                [&AGGREGATE[..], &["nvx_share=a+b,/b,rw"]].concat(),
                "live share child is invalid: a+b,/b,rw",
            ),
            (
                [&AGGREGATE[..], &["nvx_share=a,/b,rx"]].concat(),
                "live share mode is unsupported: rx",
            ),
            (
                [&AGGREGATE[..], &["nvx_share=a,/b,rw", "nvx_share=a,/c,ro"]].concat(),
                "live share child is not unique: a",
            ),
        ] {
            let command_line = tokens.join(" ");
            assert_eq!(
                Shares::parse(command_line.as_bytes())
                    .unwrap_err()
                    .message(),
                message,
                "{tokens:?}"
            );
        }
    }

    #[test]
    fn a_child_s_target_lies_between_its_name_and_its_mode() {
        // The guest checks the target itself, and refuses one that holds a comma.
        assert_eq!(
            Child::parse(b"a,/b,c,rw,file"),
            Ok(Child {
                name: "a",
                target: b"/b,c",
                read_only: false,
                file: true,
            })
        );
        // Each child that the host names reads back as it was written.
        for (name, target, read_only, file) in [
            ("0-a.b_c", "/workspace", true, false),
            ("1-0123abcd", "/etc/app/settings.json", false, true),
        ] {
            let token = child_token(name, target, read_only, file);
            let value = token.strip_prefix(CHILD_TOKEN).unwrap();
            assert_eq!(
                Child::parse(value.as_bytes()),
                Ok(Child {
                    name,
                    target: target.as_bytes(),
                    read_only,
                    file,
                })
            );
        }
    }
}
