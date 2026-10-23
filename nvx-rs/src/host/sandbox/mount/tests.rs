//! The share tests of `scripts/test_nvx_tools.py`, ported.

use std::fs;

use super::*;
use crate::ErrorKind;
use crate::host::filesystem::Bind;
use crate::host::python::fs::cwd;
use crate::host::sandbox::tests::{deny, exported, host, host_text, native, policy};

fn parse(value: &str, policy: MountPolicy) -> Result<SandboxMount> {
    SandboxMount::parse(value, policy, MountOwner::Vmm, MountKind::Directory)
}

fn message<T: std::fmt::Debug>(result: Result<T>) -> String {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    error.to_string()
}

#[test]
fn mount_parser_defaults_to_read_only_and_accepts_rw() {
    let default = parse("/workspace,host-dir", MountPolicy::default()).unwrap();
    assert_eq!(default.guest_target(), "/workspace");
    assert_eq!(default.host_path(), Path::new("host-dir"));
    assert_eq!(default.access(), MountAccess::ReadOnly);
    assert!(default.policy().denied.is_empty());

    let writable = parse(
        "/opt/hostedtoolcache,host-dir,rw",
        policy(&["logs", "secrets"], &[], &[]),
    )
    .unwrap();
    assert_eq!(writable.access(), MountAccess::ReadWrite);
    assert_eq!(writable.policy().denied, ["logs", "secrets"]);
    // One directory uses the device directly, with the absolute denied paths of its plan.
    let mut plan = exported(&[(
        "/opt/hostedtoolcache",
        "/host/dir",
        MountAccess::ReadWrite,
        MountKind::Directory,
    )]);
    deny(&mut plan, 0, &["/host/dir/logs", "/host/dir/secrets"]);
    let boot = boot_shares(&plan).unwrap();
    assert_eq!(
        boot.arguments,
        [
            "--mount".to_owned(),
            format!("/opt/hostedtoolcache,{},rw", host_text("/host/dir")),
            "--mount-deny".to_owned(),
            host_text("/host/dir/logs"),
            "--mount-deny".to_owned(),
            host_text("/host/dir/secrets"),
        ]
    );
    assert!(boot.tokens.is_empty());
    assert_eq!(
        boot.fragment,
        " virtfs_dir=/opt/hostedtoolcache virtfs_tag=microvm virtfs_mode=rw"
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
        assert_eq!(mount.access(), access);
        // OpenVMM finds the end of the host path before the mode, which NVX therefore always
        // gives.
        let mut plan = exported(&[(target, "/unused", access, MountKind::Directory)]);
        plan.children[0].root = PathBuf::from(native(host_path));
        assert_eq!(
            boot_shares(&plan).unwrap().arguments,
            [
                "--mount".to_owned(),
                format!("{target},{},{access}", native(host_path)),
            ]
        );
    }
    let mut plan = exported(&[
        (
            "/work",
            "/unused/a",
            MountAccess::ReadWrite,
            MountKind::Directory,
        ),
        (
            "/reports",
            "/unused/b",
            MountAccess::ReadOnly,
            MountKind::Directory,
        ),
    ]);
    plan.children[0].root = PathBuf::from(native(alpha));
    plan.children[1].root = PathBuf::from(native(reports));
    let arguments = boot_shares(&plan).unwrap().arguments;
    let children: Vec<&String> = arguments
        .iter()
        .zip(arguments.iter().skip(1))
        .filter(|(option, _)| *option == "--mount-child")
        .map(|(_, value)| value)
        .collect();
    assert_eq!(
        children,
        [
            &format!("{},{},rw", aggregate_child_name(0, "/work"), native(alpha)),
            &format!(
                "{},{},ro",
                aggregate_child_name(1, "/reports"),
                native(reports)
            ),
        ]
    );
}

#[test]
fn mount_owner_defaults_to_vmm_and_forwards_caller() {
    let default = parse("/workspace,host-dir,rw", MountPolicy::default()).unwrap();
    assert_eq!(default.owner(), MountOwner::Vmm);
    let mut plan = exported(&[(
        "/workspace",
        "/host/dir",
        MountAccess::ReadWrite,
        MountKind::Directory,
    )]);
    assert!(
        !boot_shares(&plan)
            .unwrap()
            .arguments
            .contains(&"--mount-owner".to_owned())
    );
    let caller = SandboxMount::parse(
        "/workspace,host-dir,rw",
        policy(&["secrets"], &[], &[]),
        MountOwner::Caller,
        MountKind::Directory,
    )
    .unwrap();
    deny(&mut plan, 0, &["/host/dir/secrets"]);
    let vmm = boot_shares(&plan).unwrap();
    plan.owner = Owner::Caller;
    let boot = boot_shares(&plan).unwrap();
    assert_eq!(
        boot.arguments,
        [
            "--mount".to_owned(),
            format!("/workspace,{},rw", host_text("/host/dir")),
            "--mount-deny".to_owned(),
            host_text("/host/dir/secrets"),
            "--mount-owner".to_owned(),
            "caller".to_owned(),
        ]
    );
    assert_eq!(caller.absolute().unwrap().owner(), MountOwner::Caller);
    assert_eq!(boot.fragment, vmm.fragment);
    assert_eq!(
        message(MountOwner::from_name("root")),
        "unsupported sandbox mount owner \"root\"; choose vmm or caller"
    );
    assert_eq!(
        message(MountAccess::from_name("rx")),
        "unsupported sandbox mount mode \"rx\"; choose ro or rw"
    );
}

#[test]
fn mount_access_policy_is_validated_and_forwarded() {
    let mount = parse(
        "/tmp/mcp-runtime,host-dir,rw",
        policy(&["mcp-logs"], &["mcp-logs/payloads"], &["agent", "cache"]),
    )
    .unwrap();
    let mut plan = exported(&[(
        "/tmp/mcp-runtime",
        "/host/dir",
        MountAccess::ReadWrite,
        MountKind::Directory,
    )]);
    plan.children[0].write = vec![host("/host/dir/agent"), host("/host/dir/cache")];
    deny(&mut plan, 0, &["/host/dir/mcp-logs"]);
    plan.children[0].allowed = vec![host("/host/dir/mcp-logs/payloads")];
    let boot = boot_shares(&plan).unwrap();
    assert_eq!(
        boot.arguments,
        [
            "--mount".to_owned(),
            format!("/tmp/mcp-runtime,{},rw", host_text("/host/dir")),
            "--mount-deny".to_owned(),
            host_text("/host/dir/mcp-logs"),
            "--mount-allow".to_owned(),
            host_text("/host/dir/mcp-logs/payloads"),
            "--mount-write".to_owned(),
            host_text("/host/dir/agent"),
            "--mount-write".to_owned(),
            host_text("/host/dir/cache"),
        ]
    );
    let absolute = mount.absolute().unwrap();
    assert_eq!(absolute.policy().allowed, ["mcp-logs/payloads"]);
    assert_eq!(absolute.policy().writable, ["agent", "cache"]);
    // OpenVMM enforces the policy on the host, so the guest's bootstrap tokens are those of the
    // plain share.
    assert_eq!(
        boot.fragment,
        " virtfs_dir=/tmp/mcp-runtime virtfs_tag=microvm virtfs_mode=rw"
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
    if cfg!(windows) {
        assert!(message(caller(temporary).validated()).contains("requires a Linux host"));
        return;
    }
    let owner = crate::host::python::fs::stat(directory.path()).unwrap();
    if owner.uid != 0 && owner.gid != 0 {
        caller(temporary).validated().unwrap();
    }
    if crate::host::python::fs::stat(Path::new("/")).unwrap().uid == 0 {
        assert_eq!(
            message(caller("/").validated()),
            "--mount-owner caller squashes guest root to the owner of the shared directory, so / \
             must not be owned by UID 0 or GID 0"
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

#[test]
fn mount_absolute_path_keeps_symbolic_link_components() {
    let mount = parse("/workspace,link/share,rw", MountPolicy::default()).unwrap();
    let absolute = mount.absolute().unwrap();
    let expected = cwd().unwrap().join("link").join("share");
    assert_eq!(absolute.host_path(), expected.as_path());
    assert!(absolute.host_path.parts().iter().any(|part| part == "link"));
    assert_eq!(absolute.absolute().unwrap(), absolute);
    let fixed = PurePath::new(Flavor::Posix, "/fixture/cwd");
    let relative = SandboxMount::parse_in(
        Flavor::Posix,
        "/workspace,link/share,rw",
        MountPolicy::default(),
        MountOwner::Vmm,
        MountKind::Directory,
    )
    .unwrap();
    let joined = relative.absolute_with(&|| Ok(fixed.clone())).unwrap();
    assert_eq!(joined.host_path.as_str(), "/fixture/cwd/link/share");
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

#[test]
fn mount_absolute_path_keeps_a_link_on_the_host() {
    let directory = tempfile::tempdir().unwrap();
    let root = PurePath::native(directory.path().to_str().unwrap());
    fs::create_dir_all(root.join("link-target/share").as_path()).unwrap();
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(
        root.join("link-target").as_path(),
        root.join("link").as_path(),
    );
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(
        root.join("link-target").as_path(),
        root.join("link").as_path(),
    );
    if linked.is_err() {
        // Symbolic links need a privilege that the host may not grant.
        return;
    }
    let mount = parse(
        &format!("/workspace,{},rw", root.join("link/share")),
        MountPolicy::default(),
    )
    .unwrap();
    let absolute = mount.absolute().unwrap();
    assert!(
        !absolute
            .host_path
            .parts()
            .iter()
            .any(|part| part == "link-target")
    );
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
        "/work\0space",
        "/work\u{85}space",
        "/work\x1cspace",
        &long,
    ] {
        assert_eq!(
            message(validate_mount_target(target)),
            format!(
                "invalid sandbox mount target {target:?}: expected an absolute non-root path without \
                 empty, dot, or parent components, whitespace, '\\', '=', or ','",
            )
        );
    }
    for (target, reserved) in [
        ("/proc", "/proc"),
        ("/proc/self", "/proc"),
        ("/sys", "/sys"),
        ("/sys/fs", "/sys"),
        ("/dev", "/dev"),
        ("/dev/shm", "/dev"),
        ("/.nvx-agent", "/.nvx-agent"),
        ("/.nvx-agent/tools", "/.nvx-agent"),
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
        "/opt/hostedtoolcache",
        "/run/secrets",
        "/run/nvx/other",
    ] {
        validate_mount_target(target).unwrap();
    }
    // The longest target fits.
    validate_mount_target(&format!("/{}", "x".repeat(4095))).unwrap();
}

#[test]
fn aggregate_child_names_pin_their_targets() {
    let name = aggregate_child_name(0, "/workspace");
    assert_eq!(name, "0-c52ddf65534b7b46035084358ab7902b");
    // Equal targets give equal names, so a restore names its children as the boot did, and
    // another target or position gives another name.
    assert_eq!(aggregate_child_name(0, "/workspace"), name);
    assert_ne!(aggregate_child_name(0, "/workspace2"), name);
    assert_ne!(aggregate_child_name(1, "/workspace"), name);
    // Every name follows OpenVMM's rule for the name of a child.
    for (index, target) in [
        (0, "/w".to_owned()),
        (999, format!("/{}", "x".repeat(1000))),
    ] {
        let name = aggregate_child_name(index, &target);
        let (number, digest) = name.split_once('-').unwrap();
        assert!(number.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(digest.len(), 32);
        assert!(digest.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    }
    assert!(aggregate_child_name(999, "/w").len() <= 64);
    assert_eq!(
        aggregate_child_argument("n", "/h,x", MountAccess::ReadWrite, MountKind::File),
        "n,/h,x,rw,file"
    );
    assert_eq!(
        mount_argument("/t", "/h", MountAccess::ReadOnly),
        "/t,/h,ro"
    );
    assert_eq!(
        aggregate_command_line_fragment(&[MountAccess::ReadOnly, MountAccess::ReadOnly]),
        " virtfs_dir=/run/nvx/shares virtfs_tag=microvm virtfs_mode=ro virtfs_aggregate=1"
    );
}

/// Returns the resolved form of `path`, without Windows' verbatim prefix, as the planner records
/// host paths.
fn resolved(path: &PurePath) -> PathBuf {
    let canonical = fs::canonicalize(path.as_path()).unwrap();
    canonical
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        .map_or(canonical.clone(), PathBuf::from)
}

#[test]
fn shares_are_planned_through_the_export_planner() {
    let directory = tempfile::tempdir().unwrap();
    let base = PurePath::native(directory.path().to_str().unwrap());
    for name in ["work/secrets/public", "work/out", "tools"] {
        fs::create_dir_all(base.join(name).as_path()).unwrap();
    }
    fs::write(base.join("settings.json").as_path(), b"{}").unwrap();
    let share = |value: String, policy: MountPolicy, owner: MountOwner, kind: MountKind| {
        SandboxMount::parse(&value, policy, owner, kind).unwrap()
    };
    let mounts = |owner: MountOwner| {
        [
            share(
                format!("/workspace,{},rw", base.join("work")),
                policy(&["secrets"], &["secrets/public"], &["out"]),
                owner,
                MountKind::Directory,
            ),
            share(
                format!("/opt/tools,{}", base.join("tools")),
                MountPolicy::default(),
                owner,
                MountKind::Directory,
            ),
            share(
                format!("/etc/app/settings.json,{}", base.join("settings.json")),
                MountPolicy::default(),
                owner,
                MountKind::File,
            ),
        ]
    };
    assert_eq!(plan_mounts(&[]).unwrap(), None);
    let plan = plan_mounts(&mounts(MountOwner::Vmm)).unwrap().unwrap();
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
    assert_eq!(workspace.denied_identities, [None]);
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
    plan.verify().unwrap();
    plan.check(ReservedTargets::CONTAINER_ROOT).unwrap();

    // The guest's shell binds each child from its token, in child order.
    let names = [
        aggregate_child_name(0, "/etc/app/settings.json"),
        aggregate_child_name(1, "/opt/tools"),
        aggregate_child_name(2, "/workspace"),
    ];
    let boot = boot_shares(&plan).unwrap();
    assert_eq!(
        boot.tokens,
        [
            format!("nvx_share={},/etc/app/settings.json,ro,file", names[0]),
            format!("nvx_share={},/opt/tools,ro", names[1]),
            format!("nvx_share={},/workspace,rw", names[2]),
        ]
    );
    let text = |path: &Path| path.to_str().unwrap().to_owned();
    assert_eq!(
        boot.arguments[boot.arguments.len() - 6..],
        [
            "--mount-deny".to_owned(),
            text(&work.join("secrets")),
            "--mount-allow".to_owned(),
            text(&work.join("secrets").join("public")),
            "--mount-write".to_owned(),
            text(&work.join("out")),
        ]
    );

    // The planner refuses what OpenVMM would: here, a writable path that a denied path hides.
    let error = plan_mounts(&[share(
        format!("/workspace,{},rw", base.join("work")),
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
        let owned = crate::host::python::fs::stat(directory.path()).unwrap();
        if owned.uid != 0 && owned.gid != 0 {
            let plan = plan_mounts(&mounts(MountOwner::Caller)).unwrap().unwrap();
            assert_eq!(plan.owner, Owner::Caller);
            assert_eq!(
                boot_shares(&plan).unwrap().arguments[boot.arguments.len()..],
                ["--mount-owner", "caller"]
            );
        }
    }
}

#[test]
fn planned_shares_keep_their_owner_and_kind() {
    let directory = tempfile::tempdir().unwrap();
    let base = PurePath::native(directory.path().to_str().unwrap());
    fs::create_dir_all(base.join("work").as_path()).unwrap();
    fs::write(base.join("settings.json").as_path(), b"{}").unwrap();
    let share = |target: &str, name: &str, owner: MountOwner, kind: MountKind| {
        SandboxMount::new(
            target,
            base.join(name).as_str(),
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
        let error =
            plan_mounts_with(&[share("/data", name, MountOwner::Vmm, kind)], &cwd).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.to_string().contains(expected), "{error}");
    }
    // A host path that leads into another share's directory leaves the share no root of its own.
    fs::create_dir_all(base.join("work/nested").as_path()).unwrap();
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
        &cwd,
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

#[test]
fn boot_tokens_refuse_what_the_shell_cannot_bind() {
    let plan = exported(&[(
        "/workspace",
        "/host/work",
        MountAccess::ReadWrite,
        MountKind::Directory,
    )]);
    // A path below a share's root.
    let mut below = plan.clone();
    below.binds[0].source = "sub".to_owned();
    assert_eq!(
        message(boot_shares(&below)),
        format!(
            "the boot tokens of a share cannot map {}, a path below the share's root {}",
            host("/host/work").join("sub").display(),
            host_text("/host/work")
        )
    );
    // One child at two targets.
    let mut twice = plan.clone();
    twice.binds.push(Bind {
        target: "/workspace2".to_owned(),
        ..twice.binds[0].clone()
    });
    assert_eq!(
        message(boot_shares(&twice)),
        format!(
            "the boot tokens of a share map {} at one target, not 2",
            host_text("/host/work")
        )
    );
    // A bind of no child.
    let mut unknown = plan.clone();
    unknown.binds.push(Bind {
        child: 1,
        target: "/other".to_owned(),
        ..unknown.binds[0].clone()
    });
    assert_eq!(
        message(boot_shares(&unknown)),
        "the boot tokens of a share cannot map a path of an unknown host directory"
    );
    // A read-only bind of a directory whose writes OpenVMM narrows to a path inside it, which the
    // tokens would mount read-write, and a read-write bind of a read-only directory.
    let mut narrowed = plan.clone();
    narrowed.children[0].write = vec![host("/host/work/out")];
    narrowed.binds[0].read_only = true;
    assert_eq!(
        message(boot_shares(&narrowed)),
        format!(
            "the boot tokens of a share cannot map {} read-only while OpenVMM exports it \
             read-write",
            host_text("/host/work")
        )
    );
    assert!(narrowed.check(ReservedTargets::CONTAINER_ROOT).is_err());
    let mut read_only = plan.clone();
    read_only.children[0].writable = false;
    assert_eq!(
        message(boot_shares(&read_only)),
        format!(
            "the boot tokens of a share cannot map {} read-write while OpenVMM exports it \
             read-only",
            host_text("/host/work")
        )
    );
    assert!(read_only.check(ReservedTargets::CONTAINER_ROOT).is_err());
    // A target that a token cannot carry.
    let mut comma = plan;
    comma.binds[0].target = "/work,space".to_owned();
    assert!(message(boot_shares(&comma)).contains("invalid sandbox mount target"));
}
