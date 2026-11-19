use std::fs;

use super::*;
use crate::host::artifacts::Layout;

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
