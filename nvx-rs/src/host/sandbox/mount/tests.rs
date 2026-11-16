//! The share tests of `scripts/test_nvx_tools.py`, ported.

use std::fs;
use std::path::{Path, PathBuf};

use super::aggregate::plan_mounts_with;
pub(crate) use super::host_paths::validate_mount_host_paths_with;
use super::request::require_mount_owner_supported_on;
pub(crate) use super::request::validate_mount_target;
use super::*;
use crate::ErrorKind;
use crate::error::Result;
use crate::host::filesystem::{Owner, ReservedTargets};
use crate::host::paths;
#[cfg(windows)]
use crate::host::sandbox::tests::host_text;
use crate::host::sandbox::tests::{host, native, policy};

fn parse(value: &str, policy: MountPolicy) -> Result<SandboxMount> {
    SandboxMount::parse(value, policy, MountOwner::Vmm, MountKind::Directory)
}

/// Returns the share of the directory `host_path` at `target`, with `access` and `policy`, whose
/// file operations run as OpenVMM, which [`parse`] returns for a value that names them.
fn share(target: &str, host_path: &str, access: MountAccess, policy: MountPolicy) -> SandboxMount {
    SandboxMount::new(
        target,
        host_path,
        access,
        policy,
        MountOwner::Vmm,
        MountKind::Directory,
    )
    .unwrap()
}

fn message<T: std::fmt::Debug>(result: Result<T>) -> String {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    error.to_string()
}

#[test]
fn mount_parser_defaults_to_read_only_and_accepts_rw() {
    let default = parse("/workspace,host-dir", MountPolicy::default()).unwrap();
    assert_eq!(default.host_path(), Path::new("host-dir"));
    assert_eq!(
        default,
        share(
            "/workspace",
            "host-dir",
            MountAccess::ReadOnly,
            MountPolicy::default()
        )
    );

    let denied = policy(&["logs", "secrets"], &[], &[]);
    let writable = parse("/opt/hostedtoolcache,host-dir,rw", denied.clone()).unwrap();
    assert_eq!(
        writable,
        share(
            "/opt/hostedtoolcache",
            "host-dir",
            MountAccess::ReadWrite,
            denied
        )
    );
}

#[test]
fn mount_parser_rejects_malformed_specifications() {
    let modes = "choose ro or rw (if HOST_PATH contains commas, end the value with ,ro or ,rw)";
    for (value, expected) in [
        (
            "/workspace",
            "--mount must be GUEST_TARGET,HOST_PATH[,ro|rw]".to_owned(),
        ),
        ("/workspace,", "--mount host path is empty".to_owned()),
        ("/workspace,,rw", "--mount host path is empty".to_owned()),
        (
            "/workspace,host,rx",
            format!("unsupported --mount mode \"rx\"; {modes}"),
        ),
        // With more than one comma, the value must end in the mode, which the error suggests for
        // a host path that contains commas.
        (
            "/workspace,host,rw,extra",
            format!("unsupported --mount mode \"extra\"; {modes}"),
        ),
        (
            "/workspace,alpha,beta",
            format!("unsupported --mount mode \"beta\"; {modes}"),
        ),
    ] {
        assert_eq!(message(parse(value, MountPolicy::default())), expected);
    }
    assert_eq!(
        message(SandboxMount::new(
            "/workspace",
            "a\0b",
            MountAccess::ReadOnly,
            MountPolicy::default(),
            MountOwner::Vmm,
            MountKind::Directory,
        )),
        "sandbox mount host path contains a NUL character: \"a\\0b\""
    );
    assert_eq!(
        message(parse("/workspace,link/../share", MountPolicy::default())),
        format!(
            "sandbox mount host path contains a parent component: {}",
            native("link/../share")
        )
    );
}

#[test]
fn mount_parser_accepts_host_paths_with_commas() {
    let alpha = r"C:\projects\alpha,beta";
    let reports = r"C:\reports\2026, final";
    for (value, target, host_path, access) in [
        (
            format!("/work,{alpha},rw"),
            "/work",
            alpha,
            MountAccess::ReadWrite,
        ),
        (
            format!("/reports,{reports},ro"),
            "/reports",
            reports,
            MountAccess::ReadOnly,
        ),
        // A host path that ends in a comma, or in what looks like a mode, keeps it before the
        // mode.
        (
            "/data,/srv/data,rw,ro".to_owned(),
            "/data",
            "/srv/data,rw",
            MountAccess::ReadOnly,
        ),
        (
            "/data,/srv/data,,rw".to_owned(),
            "/data",
            "/srv/data,",
            MountAccess::ReadWrite,
        ),
        // Values without commas in the host path keep their meaning.
        (
            "/data,/srv/data,rw".to_owned(),
            "/data",
            "/srv/data",
            MountAccess::ReadWrite,
        ),
        (
            "/data,/srv/data".to_owned(),
            "/data",
            "/srv/data",
            MountAccess::ReadOnly,
        ),
    ] {
        assert_eq!(
            parse_mount_value(&value).unwrap(),
            (target.to_owned(), host_path.to_owned(), access)
        );
        let mount = parse(&value, policy(&["secret"], &[], &[])).unwrap();
        assert_eq!(mount.host_path(), Path::new(&native(host_path)));
        assert_eq!(mount.access, access);
    }
}

#[test]
fn mount_owner_defaults_to_vmm_and_forwards_caller() {
    let default = parse("/workspace,host-dir,rw", MountPolicy::default()).unwrap();
    assert_eq!(default.owner, MountOwner::Vmm);
    let caller = SandboxMount::parse(
        "/workspace,host-dir,rw",
        policy(&["secrets"], &[], &[]),
        MountOwner::Caller,
        MountKind::Directory,
    )
    .unwrap();
    assert_eq!(
        caller.absolute_with(&paths::current_dir).unwrap().owner,
        MountOwner::Caller
    );
}

#[test]
fn mount_access_policy_is_validated_and_forwarded() {
    let requested = policy(&["mcp-logs"], &["mcp-logs/payloads"], &["agent", "cache"]);
    let mount = parse("/tmp/mcp-runtime,host-dir,rw", requested.clone()).unwrap();
    let absolute = mount.absolute_with(&paths::current_dir).unwrap();
    let host_path = paths::current_dir().unwrap().join("host-dir");
    assert_eq!(
        absolute,
        share(
            "/tmp/mcp-runtime",
            host_path.to_str().unwrap(),
            MountAccess::ReadWrite,
            requested
        )
    );
    let paths: Vec<String> = (0..129).map(|index| format!("path-{index}")).collect();
    let many: Vec<&str> = paths.iter().map(String::as_str).collect();
    for (value, policy, expected) in [
        (
            "/w,host,ro",
            policy(&[], &[], &["out"]),
            "--mount-write narrows the writes of a read-write share; --mount /w is read-only",
        ),
        (
            "/w,host,rw",
            policy(&[], &["logs/payloads"], &[]),
            "--mount-allow exposes a path inside a --mount-deny path of the same share, which \
             has none",
        ),
        (
            "/w,host,rw",
            policy(&["logs"], &["logs/a", "logs/a"], &[]),
            "sandbox mount allowed paths must be unique",
        ),
        (
            "/w,host,rw",
            policy(&[], &[], &[""]),
            "sandbox mount writable paths must be nonempty",
        ),
        (
            "/w,host,rw",
            policy(&[], &[], &["a\0"]),
            "sandbox mount writable paths must be nonempty",
        ),
        (
            "/w,host,rw",
            policy(&[], &[], &many),
            "a sandbox mount permits at most 128 writable paths",
        ),
    ] {
        assert_eq!(message(parse(value, policy)), expected);
    }
}

#[test]
fn caller_owner_requires_linux_and_a_directory_that_root_does_not_own() {
    assert_eq!(
        message(require_mount_owner_supported_on(MountOwner::Caller, true)),
        "--mount-owner caller requires a Linux host; Windows has no per-request POSIX identity \
         for OpenVMM to switch to"
    );
    require_mount_owner_supported_on(MountOwner::Vmm, true).unwrap();
    require_mount_owner_supported_on(MountOwner::Caller, false).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let temporary = directory.path().to_str().unwrap();
    let caller = |path: &str| {
        SandboxMount::new(
            "/workspace",
            path,
            MountAccess::ReadOnly,
            MountPolicy::default(),
            MountOwner::Caller,
            MountKind::Directory,
        )
        .unwrap()
    };
    #[cfg(windows)]
    assert!(message(caller(temporary).validated()).contains("requires a Linux host"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let owner = std::fs::metadata(directory.path()).unwrap();
        if owner.uid() != 0 && owner.gid() != 0 {
            caller(temporary).validated().unwrap();
        }
        if std::fs::metadata("/").unwrap().uid() == 0 {
            assert_eq!(
                message(caller("/").validated()),
                "--mount-owner caller squashes guest root to the owner of the shared directory, \
                 so / must not be owned by UID 0 or GID 0"
            );
            SandboxMount::new(
                "/workspace",
                "/",
                MountAccess::ReadOnly,
                MountPolicy::default(),
                MountOwner::Vmm,
                MountKind::Directory,
            )
            .unwrap()
            .validated()
            .unwrap();
        }
    }
}

#[test]
fn mount_absolute_path_keeps_symbolic_link_components() {
    let mount = parse("/workspace,link/share,rw", MountPolicy::default()).unwrap();
    let absolute = mount.absolute_with(&paths::current_dir).unwrap();
    let expected = paths::current_dir().unwrap().join("link").join("share");
    assert_eq!(absolute.host_path(), expected.as_path());
    assert!(absolute.host_path.iter().any(|part| part == "link"));
    assert_eq!(
        absolute.absolute_with(&paths::current_dir).unwrap(),
        absolute
    );
    let fixed = host("/fixture/cwd");
    let relative = SandboxMount::parse(
        "/workspace,link/share,rw",
        MountPolicy::default(),
        MountOwner::Vmm,
        MountKind::Directory,
    )
    .unwrap();
    let joined = relative.absolute_with(&|| Ok(fixed.clone())).unwrap();
    assert_eq!(joined.host_path, host("/fixture/cwd/link/share"));
    for (policy, expected) in [
        (
            policy(&["logs", "logs"], &[], &[]),
            "sandbox mount denied paths must be unique",
        ),
        (
            policy(&[""], &[], &[]),
            "sandbox mount denied paths must be nonempty",
        ),
    ] {
        assert_eq!(message(parse("/workspace,host", policy)), expected);
    }
    let paths: Vec<String> = (0..129).map(|index| format!("path-{index}")).collect();
    let many: Vec<&str> = paths.iter().map(String::as_str).collect();
    assert_eq!(
        message(parse("/workspace,host", policy(&many, &[], &[]))),
        "a sandbox mount permits at most 128 denied paths"
    );
}

#[cfg(windows)]
#[test]
fn verbatim_host_paths_keep_their_parent_components() {
    // `PathBuf::push` would resolve the `..` of a path with a verbatim prefix, which would hide it.
    for host_path in [r"\\?\C:\base\link\..\share", r"\\?\C:\base\.."] {
        assert_eq!(
            message(parse(
                &format!("/workspace,{host_path}"),
                MountPolicy::default()
            )),
            format!("sandbox mount host path contains a parent component: {host_path}")
        );
    }
}

#[cfg(windows)]
#[test]
fn drive_relative_policy_paths_are_below_their_share_on_its_drive() {
    // The share's root is on drive `C:`, as is the current directory, which is elsewhere.
    let mount = parse(
        &format!("/workspace,{},rw", host_text("/fixture/share")),
        policy(&["C:secret"], &[], &["c:out\\logs"]),
    )
    .unwrap();
    let paths = mount
        .absolute_policy_paths_with(&|| Ok(host("/fixture/cwd")))
        .unwrap();
    assert_eq!(
        paths,
        [
            (PolicyKind::Denied, vec![host_text("/fixture/share/secret")]),
            (PolicyKind::Allowed, Vec::new()),
            (
                PolicyKind::Writable,
                vec![host_text("/fixture/share/out/logs")]
            ),
        ]
    );
}

#[test]
fn mount_absolute_path_keeps_a_link_on_the_host() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::create_dir_all(root.join("link-target").join("share")).unwrap();
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(root.join("link-target"), root.join("link"));
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(root.join("link-target"), root.join("link"));
    if linked.is_err() {
        // Symbolic links need a privilege that the host may not grant.
        return;
    }
    let mount = parse(
        &format!(
            "/workspace,{},rw",
            root.join("link").join("share").display()
        ),
        MountPolicy::default(),
    )
    .unwrap();
    let absolute = mount.absolute_with(&paths::current_dir).unwrap();
    assert!(!absolute.host_path.iter().any(|part| part == "link-target"));
}

#[test]
fn mount_target_validation_rejects_unsafe_and_reserved_targets() {
    let long = format!("/{}", "x".repeat(4096));
    for target in [
        "",
        "/",
        "workspace",
        "/workspace/",
        "/work//space",
        "/work/./space",
        "/work/../space",
        "/work space",
        "/work=space",
        "/work\\space",
        "/work,space",
        "/work\"space",
        "/work\0space",
        "/work\u{85}space",
        "/work\x1cspace",
        &long,
    ] {
        assert_eq!(
            message(validate_mount_target(target)),
            format!(
                "invalid sandbox mount target {target:?}: expected an absolute non-root path without \
                 empty, dot, or parent components, whitespace, control characters, quotes, '\\', '=', or ','",
            )
        );
    }
    // Every way of making a share checks its target, so none holds a `"`, which would hide the
    // rest of the kernel command line from Linux.
    let expected = message(validate_mount_target("/data\"x"));
    assert_eq!(
        message(parse("/data\"x,host-dir", MountPolicy::default())),
        expected
    );
    assert_eq!(
        message(SandboxMount::new(
            "/data\"x",
            "host-dir",
            MountAccess::ReadOnly,
            MountPolicy::default(),
            MountOwner::Vmm,
            MountKind::Directory,
        )),
        expected
    );
    for (target, reserved) in [
        ("/proc", "/proc"),
        ("/proc/self", "/proc"),
        ("/sys", "/sys"),
        ("/sys/fs", "/sys"),
        ("/dev", "/dev"),
        ("/dev/shm", "/dev"),
        ("/.nvx-agent", "/.nvx-agent"),
        ("/.nvx-agent/tools", "/.nvx-agent"),
        ("/etc/machine-id", "/etc/machine-id"),
        ("/etc/resolv.conf", "/etc/resolv.conf"),
        ("/etc/resolv.conf/x", "/etc/resolv.conf"),
    ] {
        assert_eq!(
            message(validate_mount_target(target)),
            format!("sandbox mount target {target} overlaps the reserved {reserved} tree")
        );
    }
    assert_eq!(
        message(validate_mount_target("/etc")),
        "sandbox mount target /etc is reserved"
    );
    for target in ["/run", "/run/nvx"] {
        assert_eq!(
            message(validate_mount_target(target)),
            format!("sandbox mount target {target} is reserved")
        );
    }
    for (target, reserved) in [
        ("/run/nvx/time", "/run/nvx/time"),
        ("/run/nvx/time/state", "/run/nvx/time"),
    ] {
        assert_eq!(
            message(validate_mount_target(target)),
            format!("sandbox mount target {target} overlaps the reserved {reserved} tree")
        );
    }
    for target in [
        "/workspace",
        "/procfs",
        "/etc/app",
        "/etc/resolvconf",
        "/opt/hostedtoolcache",
        "/run/secrets",
        "/run/nvx/other",
    ] {
        validate_mount_target(target).unwrap();
    }
    // The longest target fits.
    validate_mount_target(&format!("/{}", "x".repeat(4095))).unwrap();
}

/// Returns the resolved form of `path`, without Windows' verbatim prefix, as the planner records
/// host paths.
fn resolved(path: &Path) -> PathBuf {
    let canonical = fs::canonicalize(path).unwrap();
    canonical
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        .map_or(canonical.clone(), PathBuf::from)
}

#[test]
fn shares_are_planned_through_the_export_planner() {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path();
    let joined = |name: &str| paths::normalized(&base.join(name));
    for name in ["work/secrets/public", "work/out", "tools"] {
        fs::create_dir_all(joined(name)).unwrap();
    }
    fs::write(joined("settings.json"), b"{}").unwrap();
    let share = |value: String, policy: MountPolicy, owner: MountOwner, kind: MountKind| {
        SandboxMount::parse(&value, policy, owner, kind).unwrap()
    };
    let mounts = |owner: MountOwner| {
        [
            share(
                format!("/workspace,{},rw", joined("work").display()),
                policy(&["secrets"], &["secrets/public"], &["out"]),
                owner,
                MountKind::Directory,
            ),
            share(
                format!("/opt/tools,{}", joined("tools").display()),
                MountPolicy::default(),
                owner,
                MountKind::Directory,
            ),
            share(
                format!(
                    "/etc/app/settings.json,{}",
                    joined("settings.json").display()
                ),
                MountPolicy::default(),
                owner,
                MountKind::File,
            ),
        ]
    };
    assert_eq!(plan_mounts(&[]).unwrap(), None);
    let planned = plan_mounts(&mounts(MountOwner::Vmm)).unwrap().unwrap();
    let plan = &planned.description;
    let work = resolved(&base.join("work"));
    // The children follow their resolved host paths, and each share maps its root at its target,
    // parents first, with its policy paths resolved.
    let children: Vec<(PathBuf, bool, bool)> = plan
        .children
        .iter()
        .map(|child| (child.root.clone(), child.file, child.writable))
        .collect();
    assert_eq!(
        children,
        [
            (resolved(&base.join("settings.json")), true, false),
            (resolved(&base.join("tools")), false, false),
            (work.clone(), false, true),
        ]
    );
    let workspace = &plan.children[2];
    assert_eq!(workspace.denied, [work.join("secrets")]);
    assert_eq!(workspace.allowed, [work.join("secrets").join("public")]);
    assert_eq!(workspace.write, [work.join("out")]);
    // Writes are narrowed to `out`, so nothing that the plan names can move.
    assert_eq!(planned.pins.denied[2], [None]);
    let binds: Vec<(usize, &str, &str, bool)> = plan
        .binds
        .iter()
        .map(|bind| {
            (
                bind.child,
                bind.source.as_str(),
                bind.target.as_str(),
                bind.read_only,
            )
        })
        .collect();
    assert_eq!(
        binds,
        [
            (2, "", "/workspace", false),
            (1, "", "/opt/tools", true),
            (0, "", "/etc/app/settings.json", true),
        ]
    );
    assert_eq!(plan.owner, Owner::Vmm);
    planned.verify().unwrap();
    planned.check(ReservedTargets::CONTAINER_ROOT).unwrap();

    // The planner refuses what OpenVMM would: here, a writable path that a denied path hides.
    let error = plan_mounts(&[share(
        format!("/workspace,{},rw", joined("work").display()),
        policy(&["secrets"], &[], &["secrets/public"]),
        MountOwner::Vmm,
        MountKind::Directory,
    )])
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(error.to_string().contains("which hides it"), "{error}");

    // The caller owner, on a Linux host, becomes the export's.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let owned = std::fs::metadata(directory.path()).unwrap();
        if owned.uid() != 0 && owned.gid() != 0 {
            let plan = plan_mounts(&mounts(MountOwner::Caller)).unwrap().unwrap();
            assert_eq!(plan.description.owner, Owner::Caller);
        }
    }
}

#[test]
fn planned_shares_keep_their_owner_and_kind() {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path();
    let joined = |name: &str| paths::normalized(&base.join(name));
    fs::create_dir_all(joined("work")).unwrap();
    fs::write(joined("settings.json"), b"{}").unwrap();
    let share = |target: &str, name: &str, owner: MountOwner, kind: MountKind| {
        SandboxMount::new(
            target,
            joined(name).to_str().unwrap(),
            MountAccess::ReadOnly,
            MountPolicy::default(),
            owner,
            kind,
        )
        .unwrap()
    };
    // One owner performs the operations of every share.
    let error = plan_mounts(&[
        share("/workspace", "work", MountOwner::Vmm, MountKind::Directory),
        share(
            "/etc/app/settings.json",
            "settings.json",
            MountOwner::Caller,
            MountKind::File,
        ),
    ])
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(
        error.to_string().contains("the same --mount-owner"),
        "{error}"
    );
    // A share keeps the kind with which it was requested, whatever the planner finds at its host
    // path later.
    for (name, kind, expected) in [
        ("work", MountKind::File, "is not a regular file"),
        (
            "settings.json",
            MountKind::Directory,
            "is not a plain directory",
        ),
    ] {
        let error = plan_mounts_with(
            &[share("/data", name, MountOwner::Vmm, kind)],
            &paths::current_dir,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.to_string().contains(expected), "{error}");
    }
    // A host path that leads into another share's directory leaves the share no root of its own.
    fs::create_dir_all(joined("work/nested")).unwrap();
    let error = plan_mounts_with(
        &[
            share("/workspace", "work", MountOwner::Vmm, MountKind::Directory),
            share(
                "/nested",
                "work/nested",
                MountOwner::Vmm,
                MountKind::Directory,
            ),
        ],
        &paths::current_dir,
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(
        error
            .to_string()
            .contains("lies inside the host directory of another share"),
        "{error}"
    );
}
