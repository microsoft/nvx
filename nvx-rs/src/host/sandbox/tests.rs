//! The checks of the sandbox profile: workload identities, share sets, and the host paths of
//! shares.

use std::fs;
use std::path::PathBuf;

use super::*;
use crate::ErrorKind;
use crate::host::filesystem::ExportPlan;
use crate::host::paths;

mod specification;

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

/// Returns `path`, an absolute POSIX path, as an absolute host path: on Windows, on drive `C:`.
pub(super) fn host(path: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!("C:{}", path.replace('/', "\\")))
    } else {
        PathBuf::from(path)
    }
}

/// Returns the text of [`host`] of `path`.
#[cfg(windows)]
pub(super) fn host_text(path: &str) -> String {
    host(path).to_str().unwrap().to_owned()
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
fn file_shares_take_no_policy_paths_or_reserved_targets() {
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

/// The host directories and files of shares in a temporary directory.
struct Fixture {
    /// The temporary directory.
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        Self { directory }
    }

    /// Returns the path of `name` in the directory, or of the directory.
    fn path(&self, name: &str) -> String {
        let path = paths::normalized(&self.directory.path().join(name));
        path.to_str().unwrap().to_owned()
    }

    /// Returns the export plan of `mounts`, which [`mount::plan_mounts`] checks against the host.
    fn planned(&self, mounts: Vec<SandboxMount>) -> Result<ExportPlan> {
        Ok(mount::plan_mounts(&mounts)?.expect("shares have an export plan"))
    }
}

/// Checks `export` against the host again, as a start of a provisioned sandbox does: its
/// directories and files must still not overlap, and it must pass [`ExportPlan::verify`].
fn validated(export: &ExportPlan) -> Result<()> {
    validated_with(export, &mount::HostQueries::native())
}

/// Implements [`validated`] with the given host queries.
fn validated_with(export: &ExportPlan, host: &mount::HostQueries<'_>) -> Result<()> {
    mount::check_distinct_export_roots(export, host)?;
    export.verify()
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
    let export = fixture
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
    validated(&export).unwrap();
    assert_eq!(export.children.len(), 2);
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
    let error = mount::tests::validate_mount_host_paths_with(&shares, &queries).unwrap_err();
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
    mount::tests::validate_mount_host_paths_with(&shares, &queries).unwrap();
    for mounts in [
        [workspace.clone(), cache.clone()],
        [cache.clone(), workspace.clone()],
    ] {
        let error = mount::tests::validate_mount_host_paths_with(&mounts, &queries).unwrap_err();
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
    let export = fixture
        .planned(vec![
            file("/srv/settings.json", "settings.json"),
            file("/srv/token", "token"),
        ])
        .unwrap();
    validated(&export).unwrap();
    // A hard link that replaced one shared file with the other after provisioning would export
    // one file twice.
    fs::remove_file(fixture.path("token")).unwrap();
    fs::hard_link(fixture.path("settings.json"), fixture.path("token")).unwrap();
    let error = validated(&export).unwrap_err();
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
    let export = fixture
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
    validated(&export).unwrap();
    let roots: Vec<PathBuf> = export
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
    let error = validated_with(&export, &queries).unwrap_err();
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
    let export = launch().unwrap();
    validated(&export).unwrap();
    let canonical = fs::canonicalize(&share).unwrap();
    // The planner leaves out Windows' verbatim prefix.
    let canonical = canonical
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        .map_or(canonical.clone(), PathBuf::from);
    assert_eq!(export.children[0].root, canonical);
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
fn missing_files_are_named_with_their_components_alone() {
    let error = require_file(Path::new("missing//file"), "OpenVMM release binary").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(
        error.to_string(),
        format!(
            "OpenVMM release binary not found: {}",
            native("missing/file")
        )
    );
}
