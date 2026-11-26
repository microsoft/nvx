use std::fs;
use std::path::Path;

use super::*;
use crate::host::artifacts::Layout;
use crate::host::paths;

/// Makes `link` a link to the directory `target`: a symbolic link on Unix, and on Windows a
/// junction, which any user may create.
fn link_directory(link: &Path, target: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    crate::host::tests::junction(link, target);
}

#[test]
fn executables_find_the_release_or_checkout_that_holds_them() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let executable = |path: &str| root.join(path).join("nvx");
    // Without a manifest, nothing is recognized.
    assert!(default_dir(&executable("bin")).is_none());
    fs::write(root.join(MANIFEST_NAME), "{}").unwrap();
    let release = default_dir(&executable("bin")).unwrap().unwrap();
    assert_eq!(release.layout(), Layout::Release);
    assert_eq!(release.root(), absolute(root).unwrap());
    for build in [
        "nvx/target/release",
        "nvx/target/x86_64-unknown-linux-musl/debug",
    ] {
        let checkout = default_dir(&executable(build)).unwrap().unwrap();
        assert_eq!(checkout.layout(), Layout::Repository, "{build}");
        assert_eq!(checkout.root(), absolute(root).unwrap(), "{build}");
    }
    assert!(default_dir(&executable("target/release")).is_none());
    assert!(default_dir(&executable("elsewhere")).is_none());
}

#[test]
fn executables_find_their_root_in_any_case_only_on_windows() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join(MANIFEST_NAME), "{}").unwrap();
    for (path, layout) in [
        ("BIN", Layout::Release),
        ("Bin", Layout::Release),
        ("NVX/TARGET/release", Layout::Repository),
        (
            "Nvx/Target/x86_64-unknown-linux-musl/debug",
            Layout::Repository,
        ),
    ] {
        let found = default_dir(&root.join(path).join("nvx"));
        if cfg!(windows) {
            let found = found.unwrap().unwrap();
            assert_eq!(found.layout(), layout, "{path}");
            assert_eq!(found.root(), absolute(root).unwrap(), "{path}");
        } else {
            // A checkout that holds the temporary directory may still be found.
            let root = absolute(root).unwrap();
            assert!(
                found.is_none_or(|found| found.unwrap().root() != root),
                "{path}"
            );
        }
    }
}

#[test]
fn executables_started_through_links_find_the_root_that_holds_them() {
    let directory = tempfile::tempdir().unwrap();
    let release = directory.path().join("release");
    let bin = release.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(release.join(MANIFEST_NAME), "{}").unwrap();
    fs::write(bin.join("nvx"), "").unwrap();
    let held = paths::resolve(&release).unwrap();
    // A link to the directory that holds the executable, under another name than `bin`.
    let tools = directory.path().join("tools");
    link_directory(&tools, &bin);
    let found = default_dir(&tools.join("nvx")).unwrap().unwrap();
    assert_eq!(found.layout(), Layout::Release);
    assert_eq!(found.root(), held);
    // A link to the executable itself, where the host lets this account create one.
    let links = directory.path().join("links");
    fs::create_dir(&links).unwrap();
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(bin.join("nvx"), links.join("nvx"));
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(bin.join("nvx"), links.join("nvx"));
    if linked.is_ok() {
        let found = default_dir(&links.join("nvx")).unwrap().unwrap();
        assert_eq!(found.layout(), Layout::Release);
        assert_eq!(found.root(), held);
    }
    // A link whose own path lies in a release keeps that release.
    let other = directory.path().join("other");
    fs::create_dir(&other).unwrap();
    fs::write(other.join(MANIFEST_NAME), "{}").unwrap();
    link_directory(&other.join("bin"), &bin);
    let found = default_dir(&other.join("bin").join("nvx"))
        .unwrap()
        .unwrap();
    assert_eq!(found.root(), absolute(&other).unwrap());
    // A link that leads to no release or checkout finds none.
    let stray = directory.path().join("stray");
    fs::create_dir(&stray).unwrap();
    let elsewhere = directory.path().join("elsewhere");
    link_directory(&elsewhere, &stray);
    assert!(default_dir(&elsewhere.join("nvx")).is_none());
}
