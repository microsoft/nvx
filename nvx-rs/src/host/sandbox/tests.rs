//! The launch-profile tests of `scripts/test_nvx_tools.py`, ported.

use std::fs;

use super::*;
use crate::ErrorKind;
use crate::host::python::fs::cwd;

const DISTRO: &str = "distro,distro.erofs,11111111-1111-1111-1111-111111111111";

fn distro() -> SandboxLayer {
    SandboxLayer::parse(DISTRO).unwrap()
}

fn policy(denied: &[&str], allowed: &[&str], writable: &[&str]) -> MountPolicy {
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

fn with_mounts(mounts: Vec<SandboxMount>) -> Result<SandboxLaunch> {
    SandboxLaunch::new(
        vec![distro()],
        "scratch.ext4",
        SandboxSettings {
            mounts,
            ..SandboxSettings::default()
        },
    )
}

fn message<T: std::fmt::Debug>(result: Result<T>) -> String {
    result.unwrap_err().to_string()
}

fn native(path: &str) -> String {
    PurePath::native(path).as_str().to_owned()
}

#[test]
fn launch_contract_orders_roles_and_builds_agent_command_line() {
    let custom =
        SandboxLayer::parse("custom,custom.erofs,22222222-2222-2222-2222-222222222222").unwrap();
    let launch = SandboxLaunch::new(
        vec![custom, distro()],
        "scratch.ext4",
        SandboxSettings {
            entrypoint: "/bin/workload".to_owned(),
            args: vec!["--serve".to_owned()],
            hostname: "example".to_owned(),
            workload_identity: (1000, 1001),
            memory_max: Some(Int::from(268_435_456_u32)),
            pids_max: Some(Int::from(64_u32)),
            mounts: Vec::new(),
        },
    )
    .unwrap();
    let roles: Vec<LayerRole> = launch
        .ordered_layers()
        .iter()
        .map(|layer| layer.role())
        .collect();
    assert_eq!(roles, [LayerRole::Distro, LayerRole::Custom]);
    assert_eq!(
        launch.kernel_command_line("quiet").unwrap(),
        "quiet nvx_sandbox=1 \
         nvx_layer=distro,0xd0003000,11111111-1111-1111-1111-111111111111 \
         nvx_layer=custom,0xd0005000,22222222-2222-2222-2222-222222222222 \
         nvx_scratch=0xd0006000,ext4 \
         nvx_entrypoint=/bin/workload nvx_hostname=example \
         nvx_arg=--serve nvx_memory_max=268435456 nvx_pids_max=65"
    );
    assert_eq!(
        launch.openvmm_arguments().unwrap(),
        [
            "--machine",
            "microvm",
            "--microvm-sandbox-block",
            "distro:file:distro.erofs,ro",
            "--microvm-sandbox-block",
            "custom:file:custom.erofs,ro",
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
        "scratch.ext4",
        SandboxSettings::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert_eq!(error.to_string(), "duplicate sandbox layer role(s): distro");
    let launch = with_mounts(Vec::new()).unwrap();
    assert_eq!(
        message(launch.kernel_command_line("nvx_sandbox=0")),
        "nvx_sandbox is owned by the sandbox profile"
    );
    assert_eq!(
        message(launch.kernel_command_line(r"foo=bar\ nvx_memory_max=max")),
        "nvx_memory_max is owned by the sandbox profile"
    );
    assert_eq!(
        message(launch.kernel_command_line(&"x".repeat(SANDBOX_COMMAND_LINE_MAX_SIZE))),
        "sandbox kernel command line exceeds its 1024-byte x86 budget; attach fewer --mount \
         shares or use shorter targets and arguments"
    );
    assert_eq!(
        message(launch.kernel_command_line("a\0b")),
        "kernel command line contains an embedded NUL"
    );
    assert_eq!(
        message(parse_workload_identity("0:0")),
        "--workload-user UID must be between 1 and 4294967295"
    );
}

#[test]
fn launch_settings_are_checked_in_python_order() {
    let settings = |change: fn(&mut SandboxSettings)| {
        let mut settings = SandboxSettings::default();
        change(&mut settings);
        message(SandboxLaunch::new(vec![distro()], "s.ext4", settings))
    };
    assert_eq!(
        message(SandboxLaunch::new(
            Vec::new(),
            "s.ext4",
            SandboxSettings::default()
        )),
        "a sandbox requires one to three read-only layers"
    );
    assert_eq!(
        message(SandboxLaunch::new(
            vec![distro(); 4],
            "s.ext4",
            SandboxSettings::default()
        )),
        "a sandbox requires one to three read-only layers"
    );
    let custom = SandboxLayer::parse("custom,c.erofs,11111111111111111111111111111111").unwrap();
    assert_eq!(
        message(SandboxLaunch::new(
            vec![custom.clone(), distro(), custom],
            "s.ext4",
            SandboxSettings::default()
        )),
        "duplicate sandbox layer role(s): custom"
    );
    // The entrypoint is checked before the hostname.
    for entrypoint in ["bin/sh", "/bin/s h", "/bin/sh\x1c"] {
        let settings = SandboxSettings {
            entrypoint: entrypoint.to_owned(),
            hostname: "Invalid".to_owned(),
            ..SandboxSettings::default()
        };
        assert_eq!(
            message(SandboxLaunch::new(vec![distro()], "s.ext4", settings)),
            "sandbox entrypoint must be an absolute path without spaces"
        );
    }
    assert_eq!(
        settings(|settings| settings.args = vec![String::new()]),
        "sandbox arguments must be nonempty and contain no spaces"
    );
    assert_eq!(
        settings(|settings| settings.args = vec!["a\u{3000}b".to_owned()]),
        "sandbox arguments must be nonempty and contain no spaces"
    );
    for hostname in ["", "-a", "a-", "A", "a.b", "a\n"] {
        let launch = SandboxLaunch::new(
            vec![distro()],
            "s.ext4",
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
        settings(|settings| settings.memory_max = Some(Int::zero())),
        "--memory-max must be positive"
    );
    assert_eq!(
        settings(|settings| settings.pids_max = Some(Int::from(-1_i64))),
        "--pids-max must be positive"
    );
}

#[test]
fn workload_identities_parse_as_python_int() {
    assert_eq!(parse_workload_identity("1000:1001").unwrap(), (1000, 1001));
    assert_eq!(
        parse_workload_identity(" +1_000 :\u{ff11}\u{ff10}").unwrap(),
        (1000, 10)
    );
    assert_eq!(
        parse_workload_identity("4294967295:1").unwrap(),
        (u32::MAX, 1)
    );
    for (value, expected) in [
        ("1000", "--workload-user must be UID:GID"),
        ("1:2:3", "--workload-user must be UID:GID"),
        ("a:1", "--workload-user must contain numeric UID and GID"),
        ("1:", "--workload-user must contain numeric UID and GID"),
        ("1:0x10", "--workload-user must contain numeric UID and GID"),
        (
            "4294967296:1",
            "--workload-user UID must be between 1 and 4294967295",
        ),
        (
            "-1:1",
            "--workload-user UID must be between 1 and 4294967295",
        ),
        (
            "1:0",
            "--workload-user GID must be between 1 and 4294967295",
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
        "--workload-user must contain numeric UID and GID"
    );
}

#[test]
fn launch_contract_attaches_mount_and_reserves_bootstrap_tokens() {
    let launch = with_mounts(vec![mount("/workspace,share,rw", &["secrets"])]).unwrap();
    let arguments = launch.openvmm_arguments().unwrap();
    let start = arguments
        .iter()
        .position(|argument| argument == "--mount")
        .unwrap();
    assert_eq!(
        arguments[start..],
        ["--mount", "/workspace,share,rw", "--mount-deny", "secrets"]
    );
    assert!(
        !launch
            .kernel_command_line("quiet")
            .unwrap()
            .contains("virtfs_")
    );
    for (token, name) in [
        ("virtfs_dir=/other", "virtfs_dir"),
        ("virtfs_tag=other", "virtfs_tag"),
        ("virtfs_mode=rw", "virtfs_mode"),
    ] {
        assert_eq!(
            message(launch.kernel_command_line(token)),
            format!("{name} is owned by the sandbox --mount option")
        );
    }
    let base = with_mounts(Vec::new())
        .unwrap()
        .kernel_command_line("")
        .unwrap();
    let fragment = mount::mounts_command_line_fragment(launch.mounts());
    assert_eq!(
        fragment,
        " virtfs_dir=/workspace virtfs_tag=microvm virtfs_mode=rw"
    );
    let fitting = "x".repeat(SANDBOX_COMMAND_LINE_MAX_SIZE - base.len() - fragment.len() - 2);
    launch.kernel_command_line(&fitting).unwrap();
    assert!(message(launch.kernel_command_line(&format!("{fitting}x"))).contains("1024-byte"));
}

#[test]
fn launch_contract_attaches_several_shares_as_an_aggregate() {
    let launch = with_mounts(vec![
        mount("/workspace,work,rw", &["secrets"]),
        mount("/opt/hostedtoolcache,tools,ro", &["credentials"]),
        mount("/srv/cache,cache,ro", &[]),
    ])
    .unwrap();
    let names: Vec<String> = launch
        .mounts()
        .iter()
        .enumerate()
        .map(|(index, mount)| mount::aggregate_child_name(index, mount.guest_target()))
        .collect();
    let arguments = launch.openvmm_arguments().unwrap();
    // Several shares are the children of one aggregate, and OpenVMM attributes each denied
    // path to the child that contains it, so relative paths are joined to their share.
    assert!(!arguments.iter().any(|argument| argument == "--mount"));
    let start = arguments
        .iter()
        .position(|argument| argument == "--mount-aggregate")
        .unwrap();
    let directory = cwd().unwrap();
    assert_eq!(
        arguments[start..],
        [
            "--mount-aggregate".to_owned(),
            "/run/nvx/shares".to_owned(),
            "--mount-child".to_owned(),
            format!("{},work,rw", names[0]),
            "--mount-child".to_owned(),
            format!("{},tools,ro", names[1]),
            "--mount-child".to_owned(),
            format!("{},cache,ro", names[2]),
            "--mount-deny".to_owned(),
            directory.join("work").join("secrets").as_str().to_owned(),
            "--mount-deny".to_owned(),
            directory
                .join("tools")
                .join("credentials")
                .as_str()
                .to_owned(),
        ]
    );
    let fragment = mount::mounts_command_line_fragment(launch.mounts());
    assert_eq!(
        fragment,
        " virtfs_dir=/run/nvx/shares virtfs_tag=microvm virtfs_mode=rw virtfs_aggregate=1"
    );
    // The guest binds each child at its target, in order.
    let command_line = launch.kernel_command_line("").unwrap();
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
        mount::mounts_command_line_fragment(&launch.mounts()[1..]),
        " virtfs_dir=/run/nvx/shares virtfs_tag=microvm virtfs_mode=ro virtfs_aggregate=1"
    );
    assert_eq!(
        message(launch.kernel_command_line("nvx_share=0,/etc,rw")),
        "nvx_share is owned by the sandbox profile"
    );
    // Every share's tokens count toward the x86 budget.
    let base = with_mounts(Vec::new())
        .unwrap()
        .kernel_command_line("")
        .unwrap();
    let shares = command_line.len() - base.len();
    let fitting =
        "x".repeat(SANDBOX_COMMAND_LINE_MAX_SIZE - base.len() - shares - fragment.len() - 2);
    launch.kernel_command_line(&fitting).unwrap();
    assert!(message(launch.kernel_command_line(&format!("{fitting}x"))).contains("1024-byte"));

    let caller: Vec<SandboxMount> = [("/workspace", "work"), ("/tools", "tools")]
        .iter()
        .map(|(target, path)| {
            SandboxMount::new(
                target,
                path,
                MountAccess::ReadOnly,
                MountPolicy::default(),
                MountOwner::Caller,
                MountKind::Directory,
            )
            .unwrap()
        })
        .collect();
    let arguments = mount::mounts_openvmm_arguments(&caller).unwrap();
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
        with_mounts(
            (0..count)
                .map(|index| mount(&format!("/share/{index:02},host{index}"), &[]))
                .collect(),
        )
        .unwrap()
    };
    // No fixed number of shares is the limit, only the command line.
    launch(10).kernel_command_line("").unwrap();
    assert!(message(launch(20).kernel_command_line("")).contains("fewer --mount shares"));
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
        assert_eq!(message(with_mounts(mounts)), expected);
    }
    // A shared prefix that is not a path component is not an overlap.
    with_mounts(vec![vmm("/work,a,rw"), vmm("/workspace,b")]).unwrap();
}

#[test]
fn launch_contract_attaches_a_file_as_an_aggregate_child() {
    let file = |value: &str| {
        SandboxMount::parse(
            value,
            MountPolicy::default(),
            MountOwner::Vmm,
            MountKind::File,
        )
        .unwrap()
    };
    let settings = file("/etc/app/settings.json,settings.json,ro");
    let launch = with_mounts(vec![settings.clone()]).unwrap();
    let name = mount::aggregate_child_name(0, "/etc/app/settings.json");
    // Even alone, a file is the child of an aggregate, because the guest mounts the device's
    // root, which is a directory.
    assert!(mount::mounts_use_aggregate(launch.mounts()));
    let arguments = launch.openvmm_arguments().unwrap();
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
            format!("{name},settings.json,ro,file"),
        ]
    );
    assert_eq!(
        mount::mounts_command_line_fragment(launch.mounts()),
        " virtfs_dir=/run/nvx/shares virtfs_tag=microvm virtfs_mode=ro virtfs_aggregate=1"
    );
    assert!(
        launch
            .kernel_command_line("")
            .unwrap()
            .ends_with(&format!(" nvx_share={name},/etc/app/settings.json,ro,file"))
    );

    // Files and directories share one aggregate, and only a file's token is marked.
    let workspace = mount("/workspace,work,rw", &["secrets"]);
    let output = file("/results/output.txt,output.txt,rw");
    let mixed = with_mounts(vec![workspace, settings, output]).unwrap();
    let names: Vec<String> = mixed
        .mounts()
        .iter()
        .enumerate()
        .map(|(index, mount)| mount::aggregate_child_name(index, mount.guest_target()))
        .collect();
    assert!(mixed.kernel_command_line("").unwrap().ends_with(&format!(
        " nvx_share={},/workspace,rw nvx_share={},/etc/app/settings.json,ro,file \
         nvx_share={},/results/output.txt,rw,file",
        names[0], names[1], names[2]
    )));
    let arguments = mixed.openvmm_arguments().unwrap();
    let denied = arguments
        .iter()
        .position(|argument| argument == "--mount-deny")
        .unwrap();
    assert_eq!(
        arguments[denied + 1],
        cwd().unwrap().join("work").join("secrets").as_str()
    );

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
        "unsupported sandbox mount kind 'socket'; choose directory or file"
    );
    // The runtime replaces /etc/machine-id with a file, and binds its time state over
    // /run/nvx/time, so nothing at or below them may be shared.
    for target in [
        "/etc/machine-id",
        "/etc/machine-id/child",
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
        let root = PurePath::native(self.directory.path().to_str().unwrap());
        if name.is_empty() {
            return root.as_str().to_owned();
        }
        root.join(name).as_str().to_owned()
    }

    fn launch(&self, mounts: Vec<SandboxMount>) -> SandboxLaunch {
        SandboxLaunch::new(
            vec![SandboxLayer::new(
                LayerRole::Distro,
                &self.path("distro.erofs"),
                "11111111-1111-1111-1111-111111111111",
            )],
            &self.path("scratch.ext4"),
            SandboxSettings {
                mounts,
                ..SandboxSettings::default()
            },
        )
        .unwrap()
    }
}

fn parse(value: &str, policy: MountPolicy) -> SandboxMount {
    SandboxMount::parse(value, policy, MountOwner::Vmm, MountKind::Directory).unwrap()
}

fn resolve(path: &str) -> PurePath {
    crate::host::python::fs::resolve(&PurePath::native(path)).unwrap()
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
            .launch(vec![workspace.clone(), toolcache])
            .validated()
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
        .launch(vec![foreign, toolcache.clone()])
        .validated()
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
        .launch(vec![
            parse(
                &format!("/workspace,{work},rw"),
                policy(&["."], &[&nested], &[]),
            ),
            toolcache.clone(),
        ])
        .validated()
        .unwrap();
    for (allowed, writable) in [(&["."][..], &[][..]), (&[], &["."])] {
        let error = fixture
            .launch(vec![
                parse(
                    &format!("/workspace,{work},rw"),
                    policy(&["nested"], allowed, writable),
                ),
                toolcache.clone(),
            ])
            .validated()
            .unwrap_err();
        assert!(error.to_string().contains("not inside"), "{error}");
    }
    let validated = fixture.launch(vec![
        parse(
            &format!("/workspace,{work},rw"),
            policy(&["nested"], &[], &[]),
        ),
        parse(
            &format!("/opt/hostedtoolcache,{tools}"),
            policy(&[&credentials], &[], &[]),
        ),
    ]);
    validated.validated().unwrap();
    assert_eq!(validated.mounts().len(), 2);

    // Resolving a path doesn't see through a bind mount, so one directory reached through two
    // paths, or a share inside a bind mount of the other share's directory, is rejected by its
    // file identities.
    let shares = [workspace.clone(), toolcache.clone()];
    mount::validate_mount_host_paths(&shares).unwrap();
    let same = |_: &PurePath| -> std::io::Result<(u64, u128)> { Ok((1, 1)) };
    let queries = mount::HostQueries {
        identity: &same,
        ..mount::HostQueries::native()
    };
    let error = mount::validate_mount_host_paths_with(&shares, &queries).unwrap_err();
    assert!(error.to_string().contains("same directory"), "{error}");
    fs::create_dir_all(fixture.path("alias/cache")).unwrap();
    let (work_resolved, alias_resolved) = (resolve(&work), resolve(&fixture.path("alias")));
    let bound = |path: &PurePath| -> std::io::Result<(u64, u128)> {
        // `alias` is a bind mount of the workspace's directory.
        if *path == work_resolved || *path == alias_resolved {
            return Ok((1, 1));
        }
        Ok((0, u128::from(path_hash(path.as_str()))))
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

#[test]
fn launch_validation_requires_plain_mount_directory() {
    let fixture = Fixture::new();
    let share = fixture.path("share");
    let launch = || {
        fixture.launch(vec![
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
    assert_eq!(launch().validated().unwrap_err().to_string(), expected);
    fs::write(&share, b"file").unwrap();
    assert_eq!(launch().validated().unwrap_err().to_string(), expected);
    fs::remove_file(&share).unwrap();
    fs::create_dir(&share).unwrap();
    let validated = launch();
    validated.validated().unwrap();
    assert_eq!(validated.mounts()[0].host_path(), Path::new(&share));
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
fn launch_contract_rejects_disk_option_delimiters() {
    let fixture = Fixture::new();
    let layer = fixture.path("layer;create=1G");
    fs::write(&layer, b"").unwrap();
    let launch = SandboxLaunch::new(
        vec![SandboxLayer::new(
            LayerRole::Distro,
            &layer,
            "11111111-1111-1111-1111-111111111111",
        )],
        &fixture.path("scratch.ext4"),
        SandboxSettings::default(),
    )
    .unwrap();
    assert_eq!(
        launch.validated().unwrap_err().to_string(),
        format!("disk paths containing commas or semicolons are unsupported: {layer}")
    );
}

#[test]
fn launch_validation_requires_the_layers_and_the_scratch_image() {
    let fixture = Fixture::new();
    let missing = SandboxLaunch::new(
        vec![SandboxLayer::new(
            LayerRole::Runtime,
            &fixture.path("missing.erofs"),
            "11111111-1111-1111-1111-111111111111",
        )],
        &fixture.path("scratch.ext4"),
        SandboxSettings::default(),
    )
    .unwrap();
    let error = missing.validated().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(
        error.to_string(),
        format!(
            "runtime EROFS layer not found: {}",
            fixture.path("missing.erofs")
        )
    );
    let scratch = fixture.launch(Vec::new());
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
