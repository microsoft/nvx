//! The tests of `nvx-rs download` and of the download that `run` and `sandbox` start themselves:
//! its options, the search for a release, with its fallback without a token, and what a command
//! does before it runs the artifacts of a checkout.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use serde_json::json;

use super::*;

/// The commits of a first-parent history, from `HEAD` on.
const COMMITS: [&str; 4] = [
    "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
    "b1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
    "c1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
    "d1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
];

/// A first-parent history of `HEAD`.
struct FakeHistory {
    /// Its commits, from `HEAD` on.
    commits: Vec<String>,
    /// The version of each commit that has one.
    versions: HashMap<String, String>,
}

impl FakeHistory {
    /// Returns the history of `commits`, each with its version, if it has one.
    fn new(commits: &[(&str, Option<&str>)]) -> Self {
        Self {
            commits: commits
                .iter()
                .map(|(commit, _)| (*commit).to_owned())
                .collect(),
            versions: commits
                .iter()
                .filter_map(|(commit, version)| {
                    Some(((*commit).to_owned(), (*version)?.to_owned()))
                })
                .collect(),
        }
    }
}

impl History for FakeHistory {
    fn first_parent_commits(&self, limit: usize) -> Result<Vec<String>> {
        Ok(self.commits.iter().take(limit).cloned().collect())
    }

    fn version_at(&self, commit: &str) -> Result<Option<String>> {
        Ok(self.versions.get(commit).cloned())
    }
}

/// The releases of `example/nvx`.
#[derive(Debug, Default)]
struct FakeReleases {
    /// The release of each tag that has one.
    releases: HashMap<String, Value>,
    /// The HTTP status with which GitHub fails each query of a release, if it does.
    failing: Option<u16>,
    /// The HTTP status with which GitHub fails the query of the repository, if it does.
    unreadable: Option<u16>,
    /// The tags of the releases that the search queried, in order.
    queried: RefCell<Vec<String>>,
}

impl FakeReleases {
    /// Returns the releases of `releases`, each with its tag.
    fn new(releases: &[(&str, Value)]) -> Self {
        Self {
            releases: releases
                .iter()
                .map(|(tag, release)| ((*tag).to_owned(), release.clone()))
                .collect(),
            ..Self::default()
        }
    }

    /// Returns releases of which GitHub fails every query with `status`.
    fn failing(status: u16) -> Self {
        Self {
            failing: Some(status),
            unreadable: Some(status),
            ..Self::default()
        }
    }

    /// Returns the tags that the search queried.
    fn queried(&self) -> Vec<String> {
        self.queried.borrow().clone()
    }
}

/// Returns the failure of a query that GitHub answered with `status`.
fn http(status: u16) -> Failure {
    Failure::Http(
        status,
        Exit::error(
            ErrorCode::Unavailable,
            format!("GitHub release query failed with HTTP {status}"),
        ),
    )
}

impl Releases for FakeReleases {
    fn repository(&self) -> &str {
        "example/nvx"
    }

    fn release(&self, tag: &str) -> std::result::Result<Option<Value>, Failure> {
        self.queried.borrow_mut().push(tag.to_owned());
        match self.failing {
            Some(status) => Err(http(status)),
            None => Ok(self.releases.get(tag).cloned()),
        }
    }

    fn check_repository(&self) -> std::result::Result<(), Failure> {
        self.unreadable.map_or(Ok(()), |status| Err(http(status)))
    }
}

/// Returns a published release whose assets have the names `assets`, each of 7 bytes.
fn release(assets: &[&str]) -> Value {
    let assets: Vec<Value> = assets
        .iter()
        .enumerate()
        .map(|(index, name)| {
            json!({
                "name": name,
                "url": format!("https://api.github.com/repos/example/nvx/releases/assets/{index}"),
                "size": 7,
            })
        })
        .collect();
    json!({"draft": false, "prerelease": true, "assets": assets})
}

/// Returns the tag of the development release of the commit `index` of [`COMMITS`] at `version`.
fn tag(version: &str, index: usize) -> String {
    development_tag(version, COMMITS[index])
}

#[test]
fn options_default_to_the_integration_repository_and_the_host_hypervisor() {
    assert_eq!(
        parse(&[]).unwrap(),
        (
            Options {
                repository: "microsoft/nvx".to_owned(),
                hypervisor: "auto",
            },
            Vec::new()
        )
    );
    let arguments = ["--repository", "octo/nvx", "--hypervisor=mshv", "extra"].map(str::to_owned);
    let (options, unrecognized) = parse(&arguments).unwrap();
    assert_eq!(options.repository, "octo/nvx");
    assert_eq!(options.hypervisor, "mshv");
    assert_eq!(unrecognized, ["extra"]);
}

#[test]
fn repositories_are_owner_and_name() {
    for repository in ["nvx", "/nvx", "microsoft/", "microsoft/nvx/dev", ""] {
        let arguments = ["--repository".to_owned(), repository.to_owned()];
        let exit = parse(&arguments).unwrap_err();
        assert_eq!(
            exit.code(),
            Some(ErrorCode::ArgumentInvalid),
            "{repository}"
        );
        let message = exit.message().unwrap();
        assert!(
            message.contains("--repository") && message.contains("OWNER/REPOSITORY"),
            "{message}"
        );
    }
}

#[test]
fn platforms_are_selected_by_os_and_backend_not_downloader_isa() {
    if cfg!(windows) {
        assert_eq!(platform("auto").unwrap(), "windows-whp");
        assert_eq!(platform("whp").unwrap(), "windows-whp");
        let exit = platform("kvm").unwrap_err();
        assert_eq!(exit.code(), Some(ErrorCode::Unsupported));
        assert_eq!(exit.message(), Some("kvm is not supported on windows"));
    } else if cfg!(target_os = "linux") {
        assert_eq!(platform("auto").unwrap(), "linux-kvm");
        assert_eq!(platform("kvm").unwrap(), "linux-kvm");
        assert_eq!(platform("mshv").unwrap(), "linux-mshv");
        let exit = platform("whp").unwrap_err();
        assert_eq!(exit.message(), Some("whp is not supported on linux"));
    } else {
        assert_eq!(
            platform("auto").unwrap_err().code(),
            Some(ErrorCode::Unavailable)
        );
    }
}

#[test]
fn packages_are_named_for_their_platform() {
    for (name, platform, expected) in [
        ("nvx-0.1.0-linux-kvm.tar.gz", "linux-kvm", true),
        ("nvx-0.1.0-linux-kvm.tar.gz", "linux-mshv", false),
        ("nvx-0.1.0-linux-mshv.tar.gz", "linux-mshv", true),
        ("nvx-0.1.0-windows-whp.zip", "windows-whp", true),
        ("nvx-0.1.0-windows-whp.tar.gz", "windows-whp", false),
        ("nvx-0.1.0-linux-kvm.zip", "linux-kvm", false),
        ("nvx-0.1.0-linux-kvm.tar.gz.sha256", "linux-kvm", false),
        ("nvx--linux-kvm.tar.gz", "linux-kvm", false),
        ("nvx-linux-kvm.tar.gz", "linux-kvm", false),
        ("other-0.1.0-linux-kvm.tar.gz", "linux-kvm", false),
        ("nvx-0.1.0-source.tar.gz", "linux-kvm", false),
        ("nvx-0.1/0-linux-kvm.tar.gz", "linux-kvm", false),
        ("nvx-0.1\\0-linux-kvm.tar.gz", "linux-kvm", false),
        ("nvx-C:0-linux-kvm.tar.gz", "linux-kvm", false),
        ("nvx-0.1\n0-linux-kvm.tar.gz", "linux-kvm", false),
    ] {
        assert_eq!(is_package(name, platform), expected, "{name:?} {platform}");
    }
}

#[test]
fn development_tags_name_twelve_digits_of_their_commit() {
    assert_eq!(
        development_tag("0.1.0", "f95dabbdfd3bbcfab289559428db43da905d08f5"),
        "v0.1.0-dev.f95dabbdfd3b"
    );
    assert_eq!(development_tag("1.2.3", "abc"), "v1.2.3-dev.abc");
}

#[test]
fn sizes_read_as_nvx_py_wrote_them() {
    assert_eq!(size_text(0), "0 B");
    assert_eq!(size_text(1023), "1023 B");
    assert_eq!(size_text(1536), "1.5 KiB");
    assert_eq!(size_text(270_951_628), "258.4 MiB");
    assert_eq!(size_text(5 << 40), "5120.0 GiB");
}

#[test]
fn the_release_built_from_head_wins() {
    let history = FakeHistory::new(&[(COMMITS[0], Some("0.1.0")), (COMMITS[1], Some("0.1.0"))]);
    let releases = FakeReleases::new(&[(
        &tag("0.1.0", 0),
        release(&[
            "nvx-0.1.0-linux-mshv.tar.gz",
            "nvx-0.1.0-linux-kvm.tar.gz",
            "nvx-0.1.0-windows-whp.zip",
        ]),
    )]);
    let asset = search(
        &history.commits,
        &history,
        &releases,
        "linux-kvm",
        Report::Stderr,
    )
    .unwrap();
    assert_eq!(
        asset,
        Asset {
            tag: "v0.1.0-dev.a1b2c3d4e5f6".to_owned(),
            name: "nvx-0.1.0-linux-kvm.tar.gz".to_owned(),
            url: "https://api.github.com/repos/example/nvx/releases/assets/1".to_owned(),
            size: 7,
        }
    );
    assert_eq!(releases.queried(), [tag("0.1.0", 0)]);
}

#[test]
fn the_search_walks_the_first_parent_history_to_the_nearest_package() {
    // HEAD's release is a draft, its parent has no VERSION, and the grandparent's release has no
    // package for the platform, so the package is the great-grandparent's.
    let history = FakeHistory::new(&[
        (COMMITS[0], Some("0.1.0")),
        (COMMITS[1], None),
        (COMMITS[2], Some("0.1.0")),
        (COMMITS[3], Some("0.0.9")),
    ]);
    let mut draft = release(&["nvx-0.1.0-windows-whp.zip"]);
    draft["draft"] = Value::Bool(true);
    let releases = FakeReleases::new(&[
        (&tag("0.1.0", 0), draft),
        (&tag("0.1.0", 2), release(&["nvx-0.1.0-linux-kvm.tar.gz"])),
        (&tag("0.0.9", 3), release(&["nvx-0.0.9-windows-whp.zip"])),
    ]);
    let asset = search(
        &history.commits,
        &history,
        &releases,
        "windows-whp",
        Report::Stderr,
    )
    .unwrap();
    assert_eq!(asset.tag, tag("0.0.9", 3));
    assert_eq!(asset.name, "nvx-0.0.9-windows-whp.zip");
    assert_eq!(
        releases.queried(),
        [tag("0.1.0", 0), tag("0.1.0", 2), tag("0.0.9", 3)]
    );
}

#[test]
fn a_history_without_a_package_reports_the_commits_that_it_searched() {
    let history = FakeHistory::new(&[(COMMITS[0], Some("0.1.0")), (COMMITS[1], Some("0.1.0"))]);
    let releases = FakeReleases::new(&[(&tag("0.1.0", 1), release(&["nvx-0.1.0-linux-kvm.zip"]))]);
    let exit = Exit::from(
        search(
            &history.commits,
            &history,
            &releases,
            "linux-kvm",
            Report::Stderr,
        )
        .unwrap_err(),
    );
    assert_eq!(exit.code(), Some(ErrorCode::Unavailable));
    assert_eq!(
        exit.message(),
        Some(
            "no example/nvx release with an NVX package for linux-kvm was built from HEAD or its \
             first-parent ancestors (2 commits searched); check out a released commit or build \
             the artifacts"
        )
    );
}

#[test]
fn an_unreadable_repository_fails_as_github_answers() {
    let history = FakeHistory::new(&[(COMMITS[0], Some("0.1.0"))]);
    let releases = FakeReleases {
        unreadable: Some(404),
        ..FakeReleases::default()
    };
    let failure = search(
        &history.commits,
        &history,
        &releases,
        "linux-kvm",
        Report::Stderr,
    )
    .unwrap_err();
    assert!(matches!(failure, Failure::Http(404, _)), "{failure:?}");
}

#[test]
fn a_rejected_token_falls_back_to_the_public_releases() {
    let history = FakeHistory::new(&[(COMMITS[0], Some("0.1.0"))]);
    for status in [401, 403] {
        let tokens = RefCell::new(Vec::new());
        let (asset, releases) = find(
            &history.commits,
            &history,
            "linux-kvm",
            Some("secret".to_owned()),
            |token| {
                tokens.borrow_mut().push(token.clone());
                Ok(match token {
                    Some(_) => FakeReleases::failing(status),
                    None => FakeReleases::new(&[(
                        &tag("0.1.0", 0),
                        release(&["nvx-0.1.0-linux-kvm.tar.gz"]),
                    )]),
                })
            },
            Report::Stderr,
        )
        .unwrap();
        assert_eq!(asset.tag, tag("0.1.0", 0));
        assert_eq!(releases.failing, None);
        assert_eq!(*tokens.borrow(), [Some("secret".to_owned()), None]);
    }
}

#[test]
fn a_failed_public_fallback_fails_as_the_token_did() {
    let history = FakeHistory::new(&[(COMMITS[0], Some("0.1.0"))]);
    let exit = find(
        &history.commits,
        &history,
        "linux-kvm",
        Some("secret".to_owned()),
        |token| {
            Ok(FakeReleases::failing(match token {
                Some(_) => 403,
                None => 404,
            }))
        },
        Report::Stderr,
    )
    .unwrap_err();
    assert_eq!(
        exit.message(),
        Some("GitHub release query failed with HTTP 403")
    );
    // A public search that fails otherwise reports its own failure.
    let exit = find(
        &history.commits,
        &history,
        "linux-kvm",
        Some("secret".to_owned()),
        |token| {
            Ok(match token {
                Some(_) => FakeReleases::failing(401),
                None => FakeReleases::default(),
            })
        },
        Report::Stderr,
    )
    .unwrap_err();
    assert!(
        exit.message()
            .unwrap()
            .starts_with("no example/nvx release with an NVX package"),
        "{exit:?}"
    );
}

#[test]
fn only_a_rejected_token_falls_back() {
    let history = FakeHistory::new(&[(COMMITS[0], Some("0.1.0"))]);
    for (token, status) in [
        (None, 401),
        (None, 403),
        (Some("secret"), 404),
        (Some("secret"), 500),
    ] {
        let connections = RefCell::new(0);
        let exit = find(
            &history.commits,
            &history,
            "linux-kvm",
            token.map(str::to_owned),
            |_| {
                *connections.borrow_mut() += 1;
                Ok(FakeReleases::failing(status))
            },
            Report::Stderr,
        )
        .unwrap_err();
        assert_eq!(*connections.borrow(), 1, "{token:?} {status}");
        assert_eq!(
            exit.message(),
            Some(format!("GitHub release query failed with HTTP {status}").as_str())
        );
    }
}

/// A temporary repository checkout with its `SOURCE-MANIFEST.json`, and its `.git` if `git`.
fn checkout(git: bool) -> (tempfile::TempDir, ArtifactDir) {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join(MANIFEST_NAME), "{}").unwrap();
    if git {
        fs::create_dir(directory.path().join(GIT_ENTRY)).unwrap();
    }
    let dir = ArtifactDir::repository(directory.path()).unwrap();
    (directory, dir)
}

/// Returns the artifacts that a cold boot of the Alpine guest runs in `dir`.
fn needed(dir: &ArtifactDir) -> Vec<PathBuf> {
    vec![dir.openvmm(), dir.kernel(), dir.initramfs(Guest::Alpine)]
}

/// Writes a file at `path`, with its missing parents.
fn write(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "artifact").unwrap();
}

#[test]
fn a_git_checkout_without_artifacts_downloads_its_release() {
    let (_directory, dir) = checkout(true);
    assert_eq!(
        action(&dir, &needed(&dir), || Ok(true)).unwrap(),
        Action::Download
    );
    // A worktree's .git is a file.
    let (directory, dir) = checkout(false);
    fs::write(directory.path().join(GIT_ENTRY), "gitdir: elsewhere\n").unwrap();
    assert_eq!(
        action(&dir, &needed(&dir), || Ok(true)).unwrap(),
        Action::Download
    );
}

#[test]
fn a_checkout_with_some_artifacts_of_a_release_warns_instead() {
    let (_directory, dir) = checkout(true);
    for present in [
        dir.openvmm(),
        dir.artifact("vmlinux.config"),
        dir.artifact("openvmm.provenance.json"),
        dir.artifact("ubuntu-distro.erofs"),
        dir.artifact("initramfs-azurelinux.cpio.gz.packages.json"),
    ] {
        write(&present);
        let missing = if present == dir.openvmm() {
            dir.kernel()
        } else {
            dir.openvmm()
        };
        assert_eq!(
            action(&dir, &needed(&dir), || Ok(true)).unwrap(),
            Action::Warn(missing, present.clone()),
            "{}",
            present.display()
        );
        fs::remove_file(&present).unwrap();
    }
    // Files that no release installs do not count.
    write(&dir.artifact("nvx-rs.provenance.json"));
    write(&dir.root().join("openvmm/target/release/other"));
    assert_eq!(
        action(&dir, &needed(&dir), || Ok(true)).unwrap(),
        Action::Download
    );
}

#[test]
fn present_artifacts_need_no_download_and_no_variable() {
    let unasked = || -> Result<bool> { panic!("the variable was read") };
    let (_directory, dir) = checkout(true);
    for path in needed(&dir) {
        write(&path);
    }
    assert_eq!(
        action(&dir, &needed(&dir), unasked).unwrap(),
        Action::Proceed
    );
    // A restore with another OpenVMM runs nothing of the root.
    let (_directory, dir) = checkout(true);
    assert_eq!(action(&dir, &[], unasked).unwrap(), Action::Proceed);
}

#[test]
fn local_arm_artifacts_proceed_but_missing_ones_never_download_x86() {
    let (_directory, dir) = checkout(true);
    let dir = dir.with_architecture(Architecture::Aarch64).unwrap();
    let exit = action(&dir, &needed(&dir), || Ok(true)).unwrap_err();
    assert_eq!(exit.code(), Some(ErrorCode::Unsupported));
    assert!(exit.message().unwrap().contains("ARM64 release downloads"));
    assert_eq!(
        action(&dir, &needed(&dir), || Ok(false)).unwrap(),
        Action::Proceed
    );
    for path in needed(&dir) {
        write(&path);
    }
    assert_eq!(
        action(&dir, &needed(&dir), || panic!(
            "local artifacts need no download setting"
        ))
        .unwrap(),
        Action::Proceed
    );
}

#[test]
fn only_git_checkouts_of_nvx_download_and_only_when_allowed() {
    let (_directory, dir) = checkout(false);
    assert_eq!(
        action(&dir, &needed(&dir), || Ok(true)).unwrap(),
        Action::Proceed
    );
    let (directory, dir) = checkout(true);
    fs::remove_file(directory.path().join(MANIFEST_NAME)).unwrap();
    assert_eq!(
        action(&dir, &needed(&dir), || Ok(true)).unwrap(),
        Action::Proceed
    );
    let (directory, _) = checkout(true);
    let release = ArtifactDir::release(directory.path()).unwrap();
    assert_eq!(
        action(&release, &needed(&release), || Ok(true)).unwrap(),
        Action::Proceed
    );
    let (_directory, dir) = checkout(true);
    assert_eq!(
        action(&dir, &needed(&dir), || Ok(false)).unwrap(),
        Action::Proceed
    );
    let failure = action(&dir, &needed(&dir), || {
        Err(Exit::error(ErrorCode::Unsupported, "invalid"))
    });
    assert_eq!(failure.unwrap_err().message(), Some("invalid"));
}

#[test]
fn the_variable_turns_downloads_off_with_0_alone() {
    for value in [None, Some(""), Some("1")] {
        assert!(
            auto_download_allowed(value.map(OsStr::new)).unwrap(),
            "{value:?}"
        );
    }
    assert!(!auto_download_allowed(Some(OsStr::new("0"))).unwrap());
    for value in ["false", "no", "00", " 0"] {
        let exit = auto_download_allowed(Some(OsStr::new(value))).unwrap_err();
        assert_eq!(exit.code(), Some(ErrorCode::Unsupported), "{value}");
        assert_eq!(
            exit.message(),
            Some(format!("NVX_AUTO_DOWNLOAD must be 0 or 1, not {value:?}").as_str())
        );
    }
}

#[test]
fn staging_directories_are_private_and_go_when_dropped() {
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path().join("build");
    let first = Staging::new(&parent).unwrap();
    let second = Staging::new(&parent).unwrap();
    assert_ne!(first.path(), second.path());
    for staging in [&first, &second] {
        assert!(staging.path().is_dir());
        assert_eq!(staging.token().len(), 16);
        assert!(staging.token().bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            staging.path().file_name().unwrap(),
            format!(".nvx-download-{}", staging.token()).as_str()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(first.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    write(&first.path().join("package/nvx/SHA256SUMS"));
    let path = first.path().to_path_buf();
    drop(first);
    assert!(!path.exists());
    assert!(second.path().is_dir());
}

#[test]
fn one_download_at_a_time_installs_into_a_checkout() {
    let (_directory, dir) = checkout(true);
    let first = DownloadLock::acquire(&dir, Report::Stderr).unwrap();
    let acquired = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let lock = DownloadLock::acquire(&dir, Report::Stderr).unwrap();
            acquired.store(true, Ordering::SeqCst);
            drop(lock);
        });
        std::thread::sleep(Duration::from_millis(500));
        assert!(!acquired.load(Ordering::SeqCst));
        drop(first);
        waiter.join().unwrap();
    });
    assert!(acquired.load(Ordering::SeqCst));
    // The lock file stays, and the next download takes its lock at once.
    assert!(dir.root().join("build").join(LOCK_NAME).is_file());
    drop(DownloadLock::acquire(&dir, Report::Stderr).unwrap());
}

#[test]
fn commands_decide_once_the_installation_of_a_download_is_complete() {
    // A download that holds the lock has put some files in place: OpenVMM alone, on which a
    // command would warn of the missing kernel, or every artifact that the command runs but no
    // provenance, which a managed start reads.
    for installed in [1, 3] {
        let (_directory, dir) = checkout(true);
        let installing = DownloadLock::acquire(&dir, Report::Stderr).unwrap();
        for path in &needed(&dir)[..installed] {
            write(path);
        }
        std::thread::scope(|scope| {
            let (sender, receiver) = mpsc::channel();
            let dir = &dir;
            scope.spawn(move || {
                let reading = DownloadLock::shared(dir, Report::Stderr).unwrap();
                let decided = action(dir, &needed(dir), || Ok(true)).unwrap();
                drop(reading);
                sender.send(decided).unwrap();
            });
            assert!(
                receiver.recv_timeout(Duration::from_millis(500)).is_err(),
                "{installed}"
            );
            for path in needed(dir) {
                write(&path);
            }
            write(&dir.artifact("openvmm.provenance.json"));
            drop(installing);
            assert_eq!(
                receiver.recv_timeout(Duration::from_secs(60)).unwrap(),
                Action::Proceed,
                "{installed}"
            );
        });
    }
}

#[test]
fn commands_hold_the_lock_that_a_download_takes_alone_while_they_read_the_checkout() {
    // A command creates the lock file in a checkout into which nothing downloaded.
    let (_directory, dir) = checkout(true);
    for path in needed(&dir) {
        write(&path);
    }
    let reading = DownloadLock::shared(&dir, Report::Stderr).unwrap().unwrap();
    assert_eq!(
        action(&dir, &needed(&dir), || Ok(true)).unwrap(),
        Action::Proceed
    );
    assert!(dir.root().join("build").join(LOCK_NAME).is_file());
    // Another command that reads the checkout meanwhile does not wait.
    let other = fs::File::open(dir.root().join("build").join(LOCK_NAME)).unwrap();
    other.try_lock_shared().unwrap();
    drop(other);
    let acquired = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let download = scope.spawn(|| {
            let lock = DownloadLock::acquire(&dir, Report::Stderr).unwrap();
            acquired.store(true, Ordering::SeqCst);
            drop(lock);
        });
        std::thread::sleep(Duration::from_millis(500));
        assert!(!acquired.load(Ordering::SeqCst));
        drop(reading);
        download.join().unwrap();
    });
    assert!(acquired.load(Ordering::SeqCst));
}
