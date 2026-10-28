//! Launch-profile contracts and validation order.

use std::fs;
use std::path::PathBuf;

use super::*;
use crate::ErrorKind;
use crate::host::filesystem::{Bind, ExportChild, FileIdentity, Owner};
use crate::host::paths;
use crate::protocol::root::{RootLayout, RootUpper};

const DISTRO: &str = "distro,distro.erofs,11111111-1111-1111-1111-111111111111";

fn distro() -> SandboxLayer {
    SandboxLayer::parse(DISTRO).unwrap()
}

/// Returns the access policy of a share that hides `denied`, exposes `allowed` inside them again,
/// and lets the workload modify only `writable`. The share tests use it too.
pub(super) fn policy(denied: &[&str], allowed: &[&str], writable: &[&str]) -> MountPolicy {
    let owned = |paths: &[&str]| paths.iter().map(|&path| path.to_owned()).collect();
    MountPolicy {
        denied: owned(denied),
        allowed: owned(allowed),
        writable: owned(writable),
    }
}

fn mount(value: &str, denied: &[&str]) -> SandboxMount {
    SandboxMount::parse(
        value,
        policy(denied, &[], &[]),
        MountOwner::Vmm,
        MountKind::Directory,
    )
    .unwrap()
}

fn with_export(export: Option<ExportPlan>) -> Result<SandboxLaunch> {
    SandboxLaunch::new(
        vec![distro()],
        SandboxUpper::scratch("scratch.ext4"),
        SandboxSettings::default(),
    )?
    .with_export(export)
}

/// Returns `path`, an absolute POSIX path, as an absolute host path: on Windows, on drive `C:`.
pub(super) fn host(path: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!("C:{}", path.replace('/', "\\")))
    } else {
        PathBuf::from(path)
    }
}

/// Returns the text of [`host`] of `path`.
pub(super) fn host_text(path: &str) -> String {
    host(path).to_str().unwrap().to_owned()
}

/// Returns the export plan that maps each of `shares`, a guest target, an absolute POSIX host path
/// that [`host`] places, an access mode, and a kind, at the root of a child of its own, in order,
/// without access-policy paths, as [`mount::plan_mounts`] plans shares.
pub(super) fn exported(shares: &[(&str, &str, MountAccess, MountKind)]) -> ExportPlan {
    let children = shares
        .iter()
        .map(|&(_, root, access, kind)| ExportChild {
            root: host(root),
            file: kind == MountKind::File,
            writable: access == MountAccess::ReadWrite,
            denied: Vec::new(),
            denied_identities: Vec::new(),
            allowed: Vec::new(),
            write: Vec::new(),
        })
        .collect();
    let mut binds: Vec<Bind> = shares
        .iter()
        .enumerate()
        .map(|(child, &(target, _, access, _))| Bind {
            child,
            source: String::new(),
            target: target.to_owned(),
            read_only: access == MountAccess::ReadOnly,
            identity: None,
        })
        .collect();
    // Parents bind before children, as the planner orders them.
    binds.sort_by(|left, right| {
        (left.target.matches('/').count(), &left.target)
            .cmp(&(right.target.matches('/').count(), &right.target))
    });
    ExportPlan {
        children,
        binds,
        owner: Owner::Vmm,
    }
}

/// Hides `paths`, absolute POSIX paths that [`host`] places, in the child `index` of `plan`, and
/// pins with made-up identities those that a workload could move, as the planner does: those
/// strictly inside a writable child without write paths, or strictly inside one of its write
/// paths, which callers set first.
pub(super) fn deny(plan: &mut ExportPlan, index: usize, paths: &[&str]) {
    let child = &mut plan.children[index];
    child.denied = paths.iter().map(|path| host(path)).collect();
    child.denied_identities = (1..)
        .zip(&child.denied)
        .map(|(number, path)| {
            let moves = child.writable
                && *path != child.root
                && (child.write.is_empty()
                    || child
                        .write
                        .iter()
                        .any(|write| path != write && path.starts_with(write)));
            moves.then_some(FileIdentity {
                device: 1,
                index: number,
            })
        })
        .collect();
}

fn message<T: std::fmt::Debug>(result: Result<T>) -> String {
    result.unwrap_err().to_string()
}

/// Returns `path` as the launch profile spells it on this host, with its components alone. The
/// share tests use it too.
pub(super) fn native(path: &str) -> String {
    paths::normalized(Path::new(path))
        .to_str()
        .unwrap()
        .to_owned()
}

#[test]
fn launch_contract_builds_agent_command_line() {
    let launch = SandboxLaunch::new(
        vec![distro()],
        SandboxUpper::scratch("scratch.ext4"),
        SandboxSettings {
            entrypoint: "/bin/workload".to_owned(),
            args: vec!["--serve".to_owned()],
            hostname: "example".to_owned(),
            workload_identity: (1000, 1001),
            memory_max: Some(268_435_456),
            pids_max: Some(64),
        },
    )
    .unwrap();
    let blocks = launch.plan().blocks();
    assert_eq!(
        blocks,
        [
            (BlockRole::Distro, Path::new("distro.erofs")),
            (BlockRole::Scratch, Path::new("scratch.ext4")),
        ]
    );
    assert_eq!(
        launch.plan().layout(),
        RootLayout {
            image: RootImage::erofs(Uuid::parse("11111111-1111-1111-1111-111111111111")),
            upper: RootUpper::Scratch,
        }
    );
    assert_eq!(
        launch.plan().kernel_command_line("quiet").unwrap(),
        "quiet nvx_sandbox=1 \
         nvx_layer=distro,0xd0003000,erofs:11111111-1111-1111-1111-111111111111 \
         nvx_scratch=0xd0006000,ext4 \
         nvx_entrypoint=/bin/workload nvx_hostname=example \
         nvx_arg=--serve nvx_memory_max=268435456 nvx_pids_max=65"
    );
    assert_eq!(
        launch.plan().openvmm_arguments().unwrap(),
        [
            "--machine",
            "microvm",
            "--microvm-sandbox-block",
            "distro:file:distro.erofs,ro",
            "--microvm-sandbox-block",
            "scratch:file:scratch.ext4",
            "--microvm-workload-identity",
            "1000:1001",
        ]
    );
}

#[test]
fn launch_contract_rejects_duplicates_and_reserved_tokens() {
    let error = SandboxLaunch::new(
        vec![distro(), distro()],
        SandboxUpper::scratch("scratch.ext4"),
        SandboxSettings::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert_eq!(error.to_string(), "duplicate sandbox layer role(s): distro");
    let launch = with_export(None).unwrap();
    let plan = launch.plan();
    let renderings: [fn(&SandboxPlan, &str) -> Result<String>; 2] = [
        SandboxPlan::kernel_command_line,
        SandboxPlan::one_shot_kernel_command_line,
    ];
    for render in renderings {
        assert_eq!(
            message(render(plan, "nvx_sandbox=0")),
            "nvx_sandbox is owned by the sandbox profile"
        );
        assert_eq!(
            message(render(plan, r"foo=bar\ nvx_memory_max=max")),
            "nvx_memory_max is owned by the sandbox profile"
        );
        assert_eq!(
            message(render(plan, "a\0b")),
            "kernel command line contains an embedded NUL"
        );
    }
    // Only the one-shot lifecycle puts the shares' targets on the command line.
    let long = "x".repeat(SANDBOX_COMMAND_LINE_MAX_SIZE);
    assert_eq!(
        message(plan.kernel_command_line(&long)),
        "sandbox kernel command line exceeds its 1024-byte x86 budget; use shorter arguments and \
         kernel parameters"
    );
    assert_eq!(
        message(plan.one_shot_kernel_command_line(&long)),
        "sandbox kernel command line exceeds its 1024-byte x86 budget; attach fewer --mount \
         shares or use shorter targets and arguments"
    );
    assert_eq!(
        message(parse_workload_identity("0:0")),
        "--workload-user UID must be between 1 and 4294967294"
    );
}

#[test]
fn launch_settings_are_checked_in_order() {
    let settings = |change: fn(&mut SandboxSettings)| {
        let mut settings = SandboxSettings::default();
        change(&mut settings);
        message(SandboxLaunch::new(
            vec![distro()],
            SandboxUpper::scratch("s.ext4"),
            settings,
        ))
    };
    assert_eq!(
        message(SandboxLaunch::new(
            Vec::new(),
            SandboxUpper::scratch("s.ext4"),
            SandboxSettings::default()
        )),
        "a sandbox requires exactly one read-only distro layer"
    );
    assert_eq!(
        message(SandboxLaunch::new(
            vec![distro(); 4],
            SandboxUpper::scratch("s.ext4"),
            SandboxSettings::default()
        )),
        "duplicate sandbox layer role(s): distro"
    );
    // The entrypoint is checked before the hostname.
    for entrypoint in ["bin/sh", "/bin/s h", "/bin/sh\x1c"] {
        let settings = SandboxSettings {
            entrypoint: entrypoint.to_owned(),
            hostname: "Invalid".to_owned(),
            ..SandboxSettings::default()
        };
        assert_eq!(
            message(SandboxLaunch::new(
                vec![distro()],
                SandboxUpper::scratch("s.ext4"),
                settings
            )),
            "sandbox entrypoint must be an absolute path without whitespace, control characters, or quotes"
        );
    }
    assert_eq!(
        settings(|settings| settings.args = vec![String::new()]),
        "sandbox arguments must be nonempty and contain no whitespace, control characters, or quotes"
    );
    assert_eq!(
        settings(|settings| settings.args = vec!["a\u{3000}b".to_owned()]),
        "sandbox arguments must be nonempty and contain no whitespace, control characters, or quotes"
    );
    for hostname in ["", "-a", "a-", "A", "a.b", "a\n"] {
        let launch = SandboxLaunch::new(
            vec![distro()],
            SandboxUpper::scratch("s.ext4"),
            SandboxSettings {
                hostname: hostname.to_owned(),
                ..SandboxSettings::default()
            },
        );
        assert_eq!(
            message(launch),
            "sandbox hostname must be a lowercase RFC 1123 label up to 63 characters",
            "{hostname:?}"
        );
    }
    assert_eq!(
        settings(|settings| settings.workload_identity = (1000, 0)),
        "sandbox workload identity must be a non-root UID:GID"
    );
    assert_eq!(
        settings(|settings| settings.memory_max = Some(0)),
        "--memory-max must be positive"
    );
    assert_eq!(
        settings(|settings| settings.pids_max = Some(0)),
        "--pids-max must be positive"
    );
}

#[test]
fn launch_rejects_limits_the_guest_cgroup_cannot_store() {
    let settings = |change: fn(&mut SandboxSettings)| {
        let mut settings = SandboxSettings::default();
        change(&mut settings);
        message(SandboxLaunch::new(
            vec![distro()],
            SandboxUpper::scratch("s.ext4"),
            settings,
        ))
    };
    assert_eq!(parse_decimal::<u64>("18446744073709551616"), None);
    assert_eq!(
        settings(|settings| settings.pids_max = Some(4_194_304)),
        "--pids-max must be at most 4194303"
    );
}

#[test]
fn workload_identities_are_ascii_decimal() {
    assert_eq!(parse_workload_identity("1000:1001").unwrap(), (1000, 1001));
    for value in [" +1000:10", "+1000:10", "1_000:10", "1000:\u{ff11}\u{ff10}"] {
        assert!(parse_workload_identity(value).is_err(), "{value:?}");
    }
    assert_eq!(
        parse_workload_identity("4294967294:4294967294").unwrap(),
        (u32::MAX - 1, u32::MAX - 1)
    );
    for (value, expected) in [
        ("1000", "--workload-user must be UID:GID"),
        ("1:2:3", "--workload-user must be UID:GID"),
        ("a:1", "--workload-user must contain numeric UID and GID"),
        ("1:", "--workload-user must contain numeric UID and GID"),
        ("1:0x10", "--workload-user must contain numeric UID and GID"),
        (
            "4294967296:1",
            "--workload-user UID must be between 1 and 4294967294",
        ),
        (
            // `(uid_t)-1` reads as "leave unchanged" to `setresuid`/`setresgid`.
            "4294967295:1",
            "--workload-user UID must be between 1 and 4294967294",
        ),
        (
            "1:4294967295",
            "--workload-user GID must be between 1 and 4294967294",
        ),
        ("-1:1", "--workload-user must contain numeric UID and GID"),
        (
            "1:0",
            "--workload-user GID must be between 1 and 4294967294",
        ),
    ] {
        assert_eq!(
            message(parse_workload_identity(value)),
            expected,
            "{value:?}"
        );
    }
    let digits = format!("{}:1", "1".repeat(4301));
    assert_eq!(
        message(parse_workload_identity(&digits)),
        "--workload-user UID must be between 1 and 4294967294"
    );
}

#[test]
fn launch_contract_attaches_mount_and_reserves_bootstrap_tokens() {
    let mut plan = exported(&[(
        "/workspace",
        "/host/share",
        MountAccess::ReadWrite,
        MountKind::Directory,
    )]);
    deny(&mut plan, 0, &["/host/share/secrets"]);
    let launch = with_export(Some(plan)).unwrap();
    let arguments = launch.plan().one_shot_openvmm_arguments().unwrap();
    let start = arguments
        .iter()
        .position(|argument| argument == "--mount")
        .unwrap();
    assert_eq!(
        arguments[start..],
        [
            "--mount".to_owned(),
            format!("/workspace,{},rw", host_text("/host/share")),
            "--mount-deny".to_owned(),
            host_text("/host/share/secrets"),
        ]
    );
    assert!(
        !launch
            .plan()
            .one_shot_kernel_command_line("quiet")
            .unwrap()
            .contains("virtfs_")
    );
    for (token, name) in [
        ("virtfs_dir=/other", "virtfs_dir"),
        ("virtfs_tag=other", "virtfs_tag"),
        ("virtfs_mode=rw", "virtfs_mode"),
    ] {
        assert_eq!(
            message(launch.plan().one_shot_kernel_command_line(token)),
            format!("{name} is owned by the sandbox --mount option")
        );
    }
    let base = with_export(None)
        .unwrap()
        .plan()
        .one_shot_kernel_command_line("")
        .unwrap();
    let fragment = mount::boot_shares(launch.export().unwrap())
        .unwrap()
        .fragment;
    assert_eq!(
        fragment,
        " virtfs_dir=/workspace virtfs_tag=microvm virtfs_mode=rw"
    );
    let fitting = "x".repeat(SANDBOX_COMMAND_LINE_MAX_SIZE - base.len() - fragment.len() - 2);
    launch
        .plan()
        .one_shot_kernel_command_line(&fitting)
        .unwrap();
    assert!(
        message(
            launch
                .plan()
                .one_shot_kernel_command_line(&format!("{fitting}x"))
        )
        .contains("1024-byte")
    );
}

#[test]
fn launch_contract_attaches_several_shares_as_an_aggregate() {
    let shares = [
        (
            "/workspace",
            "/host/work",
            MountAccess::ReadWrite,
            MountKind::Directory,
        ),
        (
            "/opt/hostedtoolcache",
            "/host/tools",
            MountAccess::ReadOnly,
            MountKind::Directory,
        ),
        (
            "/srv/cache",
            "/host/cache",
            MountAccess::ReadOnly,
            MountKind::Directory,
        ),
    ];
    let mut plan = exported(&shares);
    deny(&mut plan, 0, &["/host/work/secrets"]);
    deny(&mut plan, 1, &["/host/tools/credentials"]);
    let launch = with_export(Some(plan.clone())).unwrap();
    let names: Vec<String> = shares
        .iter()
        .enumerate()
        .map(|(index, &(target, ..))| mount::aggregate_child_name(index, target))
        .collect();
    let arguments = launch.plan().one_shot_openvmm_arguments().unwrap();
    // Several shares are the children of one aggregate, and OpenVMM attributes each denied
    // path, which is absolute, to the child that contains it.
    assert!(!arguments.iter().any(|argument| argument == "--mount"));
    let start = arguments
        .iter()
        .position(|argument| argument == "--mount-aggregate")
        .unwrap();
    assert_eq!(
        arguments[start..],
        [
            "--mount-aggregate".to_owned(),
            "/run/nvx/shares".to_owned(),
            "--mount-child".to_owned(),
            format!("{},{},rw", names[0], host_text("/host/work")),
            "--mount-child".to_owned(),
            format!("{},{},ro", names[1], host_text("/host/tools")),
            "--mount-child".to_owned(),
            format!("{},{},ro", names[2], host_text("/host/cache")),
            "--mount-deny".to_owned(),
            host_text("/host/work/secrets"),
            "--mount-deny".to_owned(),
            host_text("/host/tools/credentials"),
        ]
    );
    let fragment = mount::boot_shares(&plan).unwrap().fragment;
    assert_eq!(
        fragment,
        " virtfs_dir=/run/nvx/shares virtfs_tag=microvm virtfs_mode=rw virtfs_aggregate=1"
    );
    // The guest binds each child at its target, in order.
    let command_line = launch.plan().one_shot_kernel_command_line("").unwrap();
    assert!(!command_line.contains("virtfs_"));
    assert!(
        command_line.ends_with(&format!(
            " nvx_share={},/workspace,rw nvx_share={},/opt/hostedtoolcache,ro \
             nvx_share={},/srv/cache,ro",
            names[0], names[1], names[2]
        )),
        "{command_line}"
    );
    assert_eq!(
        mount::boot_shares(&exported(&shares[1..]))
            .unwrap()
            .fragment,
        " virtfs_dir=/run/nvx/shares virtfs_tag=microvm virtfs_mode=ro virtfs_aggregate=1"
    );
    assert_eq!(
        message(
            launch
                .plan()
                .one_shot_kernel_command_line("nvx_share=0,/etc,rw")
        ),
        "nvx_share is owned by the sandbox profile"
    );
    // Every share's tokens count toward the x86 budget.
    let base = with_export(None)
        .unwrap()
        .plan()
        .one_shot_kernel_command_line("")
        .unwrap();
    let tokens = command_line.len() - base.len();
    let fitting =
        "x".repeat(SANDBOX_COMMAND_LINE_MAX_SIZE - base.len() - tokens - fragment.len() - 2);
    launch
        .plan()
        .one_shot_kernel_command_line(&fitting)
        .unwrap();
    assert!(
        message(
            launch
                .plan()
                .one_shot_kernel_command_line(&format!("{fitting}x"))
        )
        .contains("1024-byte")
    );

    let caller = ExportPlan {
        owner: Owner::Caller,
        ..exported(&shares[1..])
    };
    let arguments = mount::boot_shares(&caller).unwrap().arguments;
    assert_eq!(
        arguments
            .iter()
            .filter(|argument| *argument == "--mount-owner")
            .count(),
        1
    );
    assert_eq!(
        arguments[arguments.len() - 2..],
        ["--mount-owner", "caller"]
    );
}

#[test]
fn launch_bounds_shares_by_the_kernel_command_line() {
    let launch = |count: usize| {
        let shares: Vec<(String, String)> = (0..count)
            .map(|index| (format!("/share/{index:02}"), format!("/host{index}")))
            .collect();
        let shares: Vec<_> = shares
            .iter()
            .map(|(target, root)| {
                (
                    target.as_str(),
                    root.as_str(),
                    MountAccess::ReadOnly,
                    MountKind::Directory,
                )
            })
            .collect();
        with_export(Some(exported(&shares))).unwrap()
    };
    // No fixed number of shares is the limit of the one-shot lifecycle, only the command line,
    // which carries each share's target.
    launch(10).plan().one_shot_kernel_command_line("").unwrap();
    assert!(
        message(launch(20).plan().one_shot_kernel_command_line(""))
            .contains("fewer --mount shares")
    );
    // The managed lifecycle announces only their number, and sends them once the guest is ready.
    assert!(
        launch(20)
            .plan()
            .kernel_command_line("")
            .unwrap()
            .ends_with(" nvx_maps=20")
    );
    assert_eq!(launch(20).plan().mapping_table().len(), 20);
}

#[test]
fn launch_rejects_invalid_share_sets() {
    let owned = |value: &str, owner: MountOwner| {
        SandboxMount::parse(value, MountPolicy::default(), owner, MountKind::Directory).unwrap()
    };
    let vmm = |value: &str| owned(value, MountOwner::Vmm);
    for (mounts, expected) in [
        (
            vec![vmm("/workspace,a,rw"), vmm("/workspace,b")],
            "sandbox mount targets /workspace and /workspace overlap; a share cannot hide another",
        ),
        (
            vec![vmm("/workspace,a,rw"), vmm("/workspace/cache,b")],
            "sandbox mount targets /workspace and /workspace/cache overlap; a share cannot hide \
             another",
        ),
        (
            vec![vmm("/opt/tools/node,a"), vmm("/opt,b,rw")],
            "sandbox mount targets /opt/tools/node and /opt overlap; a share cannot hide another",
        ),
        (
            vec![
                vmm("/workspace,a,rw"),
                owned("/tools,b", MountOwner::Caller),
            ],
            "every sandbox --mount must use the same --mount-owner",
        ),
    ] {
        assert_eq!(message(mount::validate_mounts(&mounts)), expected);
    }
    // A shared prefix that is not a path component is not an overlap.
    mount::validate_mounts(&[vmm("/work,a,rw"), vmm("/workspace,b")]).unwrap();
}

#[test]
fn launch_contract_attaches_a_file_as_an_aggregate_child() {
    let settings = (
        "/etc/app/settings.json",
        "/host/settings.json",
        MountAccess::ReadOnly,
        MountKind::File,
    );
    let launch = with_export(Some(exported(&[settings]))).unwrap();
    let name = mount::aggregate_child_name(0, "/etc/app/settings.json");
    // Even alone, a file is the child of an aggregate, because the guest mounts the device's
    // root, which is a directory.
    let arguments = launch.plan().one_shot_openvmm_arguments().unwrap();
    assert!(!arguments.iter().any(|argument| argument == "--mount"));
    let start = arguments
        .iter()
        .position(|argument| argument == "--mount-aggregate")
        .unwrap();
    assert_eq!(
        arguments[start..],
        [
            "--mount-aggregate".to_owned(),
            "/run/nvx/shares".to_owned(),
            "--mount-child".to_owned(),
            format!("{name},{},ro,file", host_text("/host/settings.json")),
        ]
    );
    assert_eq!(
        mount::boot_shares(launch.export().unwrap())
            .unwrap()
            .fragment,
        " virtfs_dir=/run/nvx/shares virtfs_tag=microvm virtfs_mode=ro virtfs_aggregate=1"
    );
    assert!(
        launch
            .plan()
            .one_shot_kernel_command_line("")
            .unwrap()
            .ends_with(&format!(" nvx_share={name},/etc/app/settings.json,ro,file"))
    );

    // Files and directories share one aggregate, and only a file's token is marked.
    let shares = [
        (
            "/workspace",
            "/host/work",
            MountAccess::ReadWrite,
            MountKind::Directory,
        ),
        settings,
        (
            "/results/output.txt",
            "/host/output.txt",
            MountAccess::ReadWrite,
            MountKind::File,
        ),
    ];
    let mut plan = exported(&shares);
    deny(&mut plan, 0, &["/host/work/secrets"]);
    let mixed = with_export(Some(plan)).unwrap();
    let names: Vec<String> = shares
        .iter()
        .enumerate()
        .map(|(index, &(target, ..))| mount::aggregate_child_name(index, target))
        .collect();
    assert!(
        mixed
            .plan()
            .one_shot_kernel_command_line("")
            .unwrap()
            .ends_with(&format!(
                " nvx_share={},/workspace,rw nvx_share={},/etc/app/settings.json,ro,file \
         nvx_share={},/results/output.txt,rw,file",
                names[0], names[1], names[2]
            ))
    );
    let arguments = mixed.plan().one_shot_openvmm_arguments().unwrap();
    let denied = arguments
        .iter()
        .position(|argument| argument == "--mount-deny")
        .unwrap();
    assert_eq!(arguments[denied + 1], host_text("/host/work/secrets"));

    // A file's mode applies to the whole file, so no policy path names one.
    for policy in [
        policy(&["x"], &[], &[]),
        policy(&[], &["x"], &[]),
        policy(&[], &[], &["x"]),
    ] {
        let error = SandboxMount::new(
            "/results/output.txt",
            "output.txt",
            MountAccess::ReadWrite,
            policy,
            MountOwner::Vmm,
            MountKind::File,
        )
        .unwrap_err();
        assert!(error.to_string().contains("shares a file"), "{error}");
    }
    assert_eq!(
        message(MountKind::from_name("socket")),
        "unsupported sandbox mount kind \"socket\"; choose directory or file"
    );
    // The runtime replaces /etc/machine-id with a file, binds the guest's resolver file over
    // /etc/resolv.conf, and binds its time state over /run/nvx/time, so nothing at or below them
    // may be shared.
    for target in [
        "/etc/machine-id",
        "/etc/machine-id/child",
        "/etc/resolv.conf",
        "/etc/resolv.conf/child",
        "/run/nvx/time",
        "/run/nvx/time/state",
    ] {
        let error = SandboxMount::parse(
            &format!("{target},id"),
            MountPolicy::default(),
            MountOwner::Vmm,
            MountKind::File,
        )
        .unwrap_err();
        assert!(error.to_string().contains("reserved"), "{error}");
    }
    // Only /etc itself is reserved, not what lies below it, and the runtime creates /run and
    // /run/nvx, but nothing else of them (microsoft/nvx#486).
    mount("/etc/app,app", &[]);
    for target in ["/etc", "/run", "/run/nvx"] {
        assert_eq!(
            message(mount::validate_mount_target(target)),
            format!("sandbox mount target {target} is reserved")
        );
    }
    assert_eq!(
        message(mount::validate_mount_target("/run/nvx/time/state")),
        "sandbox mount target /run/nvx/time/state overlaps the reserved /run/nvx/time tree"
    );
    for target in [
        "/run/secrets",
        "/run/nvx/other",
        "/run/nvxtime",
        "/run/nvx/timer",
    ] {
        mount::validate_mount_target(target).unwrap();
    }
}

/// A sandbox's distro layer and scratch image in a temporary directory.
struct Fixture {
    /// The temporary directory.
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("distro.erofs"), b"layer").unwrap();
        fs::write(directory.path().join("scratch.ext4"), b"scratch").unwrap();
        Self { directory }
    }

    /// Returns the path of `name` in the directory, or of the directory.
    fn path(&self, name: &str) -> String {
        let path = paths::normalized(&self.directory.path().join(name));
        path.to_str().unwrap().to_owned()
    }

    fn launch(&self) -> SandboxLaunch {
        SandboxLaunch::new(
            vec![SandboxLayer::new(
                LayerRole::Distro,
                &self.path("distro.erofs"),
                "11111111-1111-1111-1111-111111111111",
            )],
            SandboxUpper::scratch(&self.path("scratch.ext4")),
            SandboxSettings::default(),
        )
        .unwrap()
    }

    /// Returns the launch with the export plan of `mounts`, which [`mount::plan_mounts`] checks
    /// against the host.
    fn planned(&self, mounts: Vec<SandboxMount>) -> Result<SandboxLaunch> {
        self.launch().with_export(mount::plan_mounts(&mounts)?)
    }
}

fn parse(value: &str, policy: MountPolicy) -> SandboxMount {
    SandboxMount::parse(value, policy, MountOwner::Vmm, MountKind::Directory).unwrap()
}

fn resolve(path: &str) -> PathBuf {
    paths::resolve(Path::new(path)).unwrap()
}

fn path_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[test]
fn launch_validation_rejects_overlapping_or_foreign_host_paths() {
    let fixture = Fixture::new();
    let (work, tools, root) = (
        fixture.path("work"),
        fixture.path("tools"),
        fixture.path(""),
    );
    fs::create_dir_all(fixture.path("work/nested")).unwrap();
    fs::create_dir_all(fixture.path("tools/credentials")).unwrap();
    let nested = fixture.path("work/nested");
    let credentials = fixture.path("tools/credentials");

    let workspace = parse(&format!("/workspace,{work},rw"), MountPolicy::default());
    for host_path in [&nested, &work, &root] {
        let toolcache = parse(
            &format!("/opt/hostedtoolcache,{host_path}"),
            MountPolicy::default(),
        );
        let error = fixture
            .planned(vec![workspace.clone(), toolcache])
            .unwrap_err();
        assert!(error.to_string().contains("overlap"), "{error}");
    }

    // Each denied path must be inside its own share.
    let foreign = parse(
        &format!("/workspace,{work},rw"),
        policy(&[&credentials], &[], &[]),
    );
    let toolcache = parse(
        &format!("/opt/hostedtoolcache,{tools}"),
        MountPolicy::default(),
    );
    let error = fixture
        .planned(vec![foreign, toolcache.clone()])
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "--mount-deny {credentials} is not inside the host directory of --mount /workspace; \
             with several shares, each --mount-deny follows the --mount whose directory it names"
        )
    );
    // A denied path may name a share's root, which OpenVMM then hides behind its allowed paths,
    // but no allowed or writable path may.
    fixture
        .planned(vec![
            parse(
                &format!("/workspace,{work},rw"),
                policy(&["."], &[&nested], &[]),
            ),
            toolcache.clone(),
        ])
        .unwrap();
    for (allowed, writable) in [(&["."][..], &[][..]), (&[], &["."])] {
        let error = fixture
            .planned(vec![
                parse(
                    &format!("/workspace,{work},rw"),
                    policy(&["nested"], allowed, writable),
                ),
                toolcache.clone(),
            ])
            .unwrap_err();
        assert!(error.to_string().contains("not inside"), "{error}");
    }
    let validated = fixture
        .planned(vec![
            parse(
                &format!("/workspace,{work},rw"),
                policy(&["nested"], &[], &[]),
            ),
            parse(
                &format!("/opt/hostedtoolcache,{tools}"),
                policy(&[&credentials], &[], &[]),
            ),
        ])
        .unwrap();
    validated.validated().unwrap();
    assert_eq!(validated.export().unwrap().children.len(), 2);
    // One share's policy paths must lie in it too.
    let error = fixture
        .planned(vec![parse(
            &format!("/workspace,{work},rw"),
            policy(&[&credentials], &[], &[]),
        )])
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "--mount-deny {credentials} is not inside the host directory of --mount /workspace"
        )
    );

    // Resolving a path doesn't see through a bind mount, so one directory reached through two
    // paths, or a share inside a bind mount of the other share's directory, is rejected by its
    // file identities.
    let shares = [workspace.clone(), toolcache.clone()];
    mount::validate_mount_host_paths(&shares).unwrap();
    let same = |_: &Path| -> std::io::Result<(u64, u64)> { Ok((1, 1)) };
    let queries = mount::HostQueries {
        identity: &same,
        ..mount::HostQueries::native()
    };
    let error = mount::validate_mount_host_paths_with(&shares, &queries).unwrap_err();
    assert!(error.to_string().contains("same directory"), "{error}");
    fs::create_dir_all(fixture.path("alias/cache")).unwrap();
    let (work_resolved, alias_resolved) = (resolve(&work), resolve(&fixture.path("alias")));
    let bound = |path: &Path| -> std::io::Result<(u64, u64)> {
        // `alias` is a bind mount of the workspace's directory.
        if *path == work_resolved || *path == alias_resolved {
            return Ok((1, 1));
        }
        Ok((0, path_hash(path.to_str().unwrap())))
    };
    let queries = mount::HostQueries {
        identity: &bound,
        ..mount::HostQueries::native()
    };
    let cache = parse(
        &format!("/opt/hostedtoolcache,{}", fixture.path("alias/cache")),
        MountPolicy::default(),
    );
    mount::validate_mount_host_paths_with(&shares, &queries).unwrap();
    for mounts in [
        [workspace.clone(), cache.clone()],
        [cache.clone(), workspace.clone()],
    ] {
        let error = mount::validate_mount_host_paths_with(&mounts, &queries).unwrap_err();
        assert!(error.to_string().contains("inside the other"), "{error}");
    }
}

#[cfg(windows)]
#[test]
fn shares_that_only_a_verbatim_path_names_are_refused() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path("work")).unwrap();
    // Without the verbatim prefix, Windows drops the trailing period of `work.`, so the share
    // would export `work` instead, read-write.
    let verbatim = fs::canonicalize(fixture.path("")).unwrap();
    let host = format!(r"{}\work.", verbatim.display());
    fs::create_dir(&host).unwrap();
    let error = fixture
        .planned(vec![parse(
            &format!("/workspace,{host},rw"),
            MountPolicy::default(),
        )])
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(error.to_string().contains("is not accessible"), "{error}");
}

#[test]
fn launch_validation_rejects_shares_that_came_to_overlap() {
    let fixture = Fixture::new();
    fs::write(fixture.path("settings.json"), b"{}").unwrap();
    fs::write(fixture.path("token"), b"token").unwrap();
    let file = |target: &str, name: &str| {
        SandboxMount::parse(
            &format!("{target},{}", fixture.path(name)),
            MountPolicy::default(),
            MountOwner::Vmm,
            MountKind::File,
        )
        .unwrap()
    };
    let launch = fixture
        .planned(vec![
            file("/srv/settings.json", "settings.json"),
            file("/srv/token", "token"),
        ])
        .unwrap();
    launch.validated().unwrap();
    // A hard link that replaced one shared file with the other after provisioning would export
    // one file twice.
    fs::remove_file(fixture.path("token")).unwrap();
    fs::hard_link(fixture.path("settings.json"), fixture.path("token")).unwrap();
    let error = launch.validated().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(
        error
            .to_string()
            .contains("name the same directory or file"),
        "{error}"
    );

    // A bind mount that made one shared directory a view of the directory that holds the other
    // after provisioning would let one share reach the other's files under its access mode.
    fs::create_dir_all(fixture.path("tools")).unwrap();
    fs::create_dir_all(fixture.path("work")).unwrap();
    let launch = fixture
        .planned(vec![
            parse(
                &format!("/workspace,{},rw", fixture.path("work")),
                MountPolicy::default(),
            ),
            parse(
                &format!("/opt/tools,{}", fixture.path("tools")),
                MountPolicy::default(),
            ),
        ])
        .unwrap();
    launch.validated().unwrap();
    let roots: Vec<PathBuf> = launch
        .export()
        .unwrap()
        .children
        .iter()
        .map(|child| child.root.clone())
        .collect();
    let (view, holder) = (roots[0].clone(), roots[1].parent().unwrap().to_path_buf());
    let bound = move |path: &Path| -> std::io::Result<(u64, u64)> {
        if *path == view || *path == holder {
            return Ok((1, 1));
        }
        Ok((0, path_hash(path.to_str().unwrap())))
    };
    let queries = mount::HostQueries {
        identity: &bound,
        ..mount::HostQueries::native()
    };
    let error = launch.plan().validated_with(&queries).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(
        error
            .to_string()
            .contains("inside the other through a bind mount"),
        "{error}"
    );
}

#[test]
fn launch_validation_requires_plain_mount_directory() {
    let fixture = Fixture::new();
    let share = fixture.path("share");
    let launch = || {
        fixture.planned(vec![
            SandboxMount::new(
                "/workspace",
                &share,
                MountAccess::ReadOnly,
                MountPolicy::default(),
                MountOwner::Vmm,
                MountKind::Directory,
            )
            .unwrap(),
        ])
    };
    let expected = format!("sandbox mount host path is not a plain directory: {share}");
    assert_eq!(launch().unwrap_err().to_string(), expected);
    fs::write(&share, b"file").unwrap();
    assert_eq!(launch().unwrap_err().to_string(), expected);
    fs::remove_file(&share).unwrap();
    fs::create_dir(&share).unwrap();
    let validated = launch().unwrap();
    validated.validated().unwrap();
    let canonical = fs::canonicalize(&share).unwrap();
    // The planner leaves out Windows' verbatim prefix.
    let canonical = canonical
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        .map_or(canonical.clone(), PathBuf::from);
    assert_eq!(validated.export().unwrap().children[0].root, canonical);
}

#[test]
fn launch_validation_requires_the_kind_of_each_share() {
    let fixture = Fixture::new();
    let config = fixture.path("config");
    fs::create_dir(&config).unwrap();
    let settings = fixture.path("config/settings.json");
    fs::write(&settings, b"{}").unwrap();
    assert_eq!(mount::mount_kind(Path::new(&settings)), MountKind::File);
    assert_eq!(mount::mount_kind(Path::new(&config)), MountKind::Directory);
    assert_eq!(
        mount::mount_kind(Path::new(&fixture.path("missing"))),
        MountKind::Directory
    );
    let share = |target: &str, path: &str, kind: MountKind| {
        SandboxMount::new(
            target,
            path,
            MountAccess::ReadOnly,
            MountPolicy::default(),
            MountOwner::Vmm,
            kind,
        )
        .unwrap()
    };
    let file = share("/etc/app/settings.json", &settings, MountKind::File);
    file.validated().unwrap();
    // A directory at the file's path is not shared in its place.
    fs::remove_file(&settings).unwrap();
    fs::create_dir(&settings).unwrap();
    let expected = format!("sandbox mount host path is not a regular file: {settings}");
    assert_eq!(file.validated().unwrap_err().to_string(), expected);
    fs::remove_dir(&settings).unwrap();
    assert_eq!(file.validated().unwrap_err().to_string(), expected);

    // A file overlaps a shared directory that contains it, and another share of the same file,
    // such as through a hard link, but not a file beside it.
    fs::write(&settings, b"{}").unwrap();
    let other = fixture.path("config/other.json");
    fs::write(&other, b"{}").unwrap();
    let link = fixture.path("link.json");
    fs::hard_link(&settings, &link).unwrap();
    mount::validate_mount_host_paths(&[
        share("/a.json", &settings, MountKind::File),
        share("/b.json", &other, MountKind::File),
    ])
    .unwrap();
    let error = mount::validate_mount_host_paths(&[
        share("/config", &config, MountKind::Directory),
        share("/a.json", &settings, MountKind::File),
    ])
    .unwrap_err();
    assert!(error.to_string().contains("overlap"), "{error}");
    let error = mount::validate_mount_host_paths(&[
        share("/a.json", &settings, MountKind::File),
        share("/b.json", &link, MountKind::File),
    ])
    .unwrap_err();
    assert!(
        error.to_string().contains("same directory or file"),
        "{error}"
    );
}

#[test]
fn launch_contract_rejects_disk_option_delimiters_and_unbalanced_brackets() {
    let fixture = Fixture::new();
    for name in [
        "layer;create=1G",
        "layer[1.erofs",
        "layer]1.erofs",
        "layer][1.erofs",
    ] {
        let layer = fixture.path(name);
        fs::write(&layer, b"").unwrap();
        let launch = SandboxLaunch::new(
            vec![SandboxLayer::new(
                LayerRole::Distro,
                &layer,
                "11111111-1111-1111-1111-111111111111",
            )],
            SandboxUpper::scratch(&fixture.path("scratch.ext4")),
            SandboxSettings::default(),
        )
        .unwrap();
        let error = launch.validated().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert_eq!(
            error.to_string(),
            format!(
                "disk paths containing commas, semicolons, or unbalanced brackets are unsupported: \
                 {layer}"
            )
        );
    }
}

#[test]
fn launch_validation_requires_the_layers_and_the_scratch_image() {
    let fixture = Fixture::new();
    let missing = SandboxLaunch::new(
        vec![SandboxLayer::new(
            LayerRole::Distro,
            &fixture.path("missing.erofs"),
            "11111111-1111-1111-1111-111111111111",
        )],
        SandboxUpper::scratch(&fixture.path("scratch.ext4")),
        SandboxSettings::default(),
    )
    .unwrap();
    let error = missing.validated().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(
        error.to_string(),
        format!(
            "distro EROFS layer not found: {}",
            fixture.path("missing.erofs")
        )
    );
    let scratch = fixture.launch();
    fs::remove_file(fixture.path("scratch.ext4")).unwrap();
    assert_eq!(
        scratch.validated().unwrap_err().to_string(),
        format!(
            "ext4 scratch image not found: {}",
            fixture.path("scratch.ext4")
        )
    );
    assert_eq!(
        require_file(Path::new("missing//file"), "OpenVMM release binary")
            .unwrap_err()
            .to_string(),
        format!(
            "OpenVMM release binary not found: {}",
            native("missing/file")
        )
    );
}
