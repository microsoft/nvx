//! The share tests of `scripts/test_nvx_tools.py`, ported.

use std::fs;

use super::*;
use crate::ErrorKind;
use crate::host::python::fs::cwd;

fn policy(denied: &[&str], allowed: &[&str], writable: &[&str]) -> MountPolicy {
    let owned = |paths: &[&str]| paths.iter().map(|&path| path.to_owned()).collect();
    MountPolicy {
        denied: owned(denied),
        allowed: owned(allowed),
        writable: owned(writable),
    }
}

fn parse(value: &str, policy: MountPolicy) -> Result<SandboxMount> {
    SandboxMount::parse(value, policy, MountOwner::Vmm, MountKind::Directory)
}

fn message<T: std::fmt::Debug>(result: Result<T>) -> String {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    error.to_string()
}

fn native(path: &str) -> String {
    PurePath::native(path).as_str().to_owned()
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
    assert_eq!(
        mounts_openvmm_arguments(std::slice::from_ref(&writable)).unwrap(),
        [
            "--mount",
            "/opt/hostedtoolcache,host-dir,rw",
            "--mount-deny",
            "logs",
            "--mount-deny",
            "secrets",
        ]
    );
    assert_eq!(
        writable.command_line_fragment(),
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
            format!("unsupported --mount mode 'rx'; {modes}"),
        ),
        // With more than one comma, the value must end in the mode, which the error suggests for
        // a host path that contains commas.
        (
            "/workspace,host,rw,extra",
            format!("unsupported --mount mode 'extra'; {modes}"),
        ),
        (
            "/workspace,alpha,beta",
            format!("unsupported --mount mode 'beta'; {modes}"),
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
        "sandbox mount host path contains a NUL character: 'a\\x00b'"
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
        assert_eq!(
            mounts_openvmm_arguments(&[mount]).unwrap(),
            [
                "--mount".to_owned(),
                format!("{target},{},{access}", native(host_path)),
                "--mount-deny".to_owned(),
                "secret".to_owned(),
            ]
        );
    }
    let alpha_mount = parse(&format!("/work,{alpha},rw"), MountPolicy::default()).unwrap();
    let reports_mount = parse(&format!("/reports,{reports},ro"), MountPolicy::default()).unwrap();
    let arguments = mounts_openvmm_arguments(&[alpha_mount, reports_mount]).unwrap();
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
    assert!(
        !mounts_openvmm_arguments(std::slice::from_ref(&default))
            .unwrap()
            .contains(&"--mount-owner".to_owned())
    );
    let caller = SandboxMount::parse(
        "/workspace,host-dir,rw",
        policy(&["secrets"], &[], &[]),
        MountOwner::Caller,
        MountKind::Directory,
    )
    .unwrap();
    assert_eq!(
        mounts_openvmm_arguments(std::slice::from_ref(&caller)).unwrap(),
        [
            "--mount",
            "/workspace,host-dir,rw",
            "--mount-deny",
            "secrets",
            "--mount-owner",
            "caller",
        ]
    );
    assert_eq!(caller.absolute().unwrap().owner(), MountOwner::Caller);
    assert_eq!(
        caller.command_line_fragment(),
        default.command_line_fragment()
    );
    assert_eq!(
        message(MountOwner::from_name("root")),
        "unsupported sandbox mount owner 'root'; choose vmm or caller"
    );
    assert_eq!(
        message(MountAccess::from_name("rx")),
        "unsupported sandbox mount mode 'rx'; choose ro or rw"
    );
}

#[test]
fn mount_access_policy_is_validated_and_forwarded() {
    let mount = parse(
        "/tmp/mcp-runtime,host-dir,rw",
        policy(&["mcp-logs"], &["mcp-logs/payloads"], &["agent", "cache"]),
    )
    .unwrap();
    assert_eq!(
        mounts_openvmm_arguments(std::slice::from_ref(&mount)).unwrap(),
        [
            "--mount",
            "/tmp/mcp-runtime,host-dir,rw",
            "--mount-deny",
            "mcp-logs",
            "--mount-allow",
            "mcp-logs/payloads",
            "--mount-write",
            "agent",
            "--mount-write",
            "cache",
        ]
    );
    let absolute = mount.absolute().unwrap();
    assert_eq!(absolute.policy().allowed, ["mcp-logs/payloads"]);
    assert_eq!(absolute.policy().writable, ["agent", "cache"]);
    // OpenVMM enforces the policy on the host, so the guest's bootstrap tokens are those of the
    // plain share.
    assert_eq!(
        mount.command_line_fragment(),
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
                "invalid sandbox mount target {}: expected an absolute non-root path without \
                 empty, dot, or parent components, whitespace, '\\', '=', or ','",
                repr(target)
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
    for target in ["/workspace", "/procfs", "/etc/app", "/opt/hostedtoolcache"] {
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
