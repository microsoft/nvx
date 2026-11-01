use std::fs;

use super::*;
use crate::host::artifacts::Layout;

#[test]
fn executables_find_the_release_or_checkout_that_holds_them() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let executable = |path: &str| root.join(path).join("nvx-rs");
    // Without a manifest, nothing is recognized.
    assert!(default_dir(&executable("bin")).is_none());
    fs::write(root.join(MANIFEST_NAME), "{}").unwrap();
    let release = default_dir(&executable("bin")).unwrap().unwrap();
    assert_eq!(release.layout(), Layout::Release);
    assert_eq!(release.root(), absolute(root).unwrap());
    for build in [
        "nvx-rs/target/release",
        "nvx-rs/target/x86_64-unknown-linux-musl/debug",
    ] {
        let checkout = default_dir(&executable(build)).unwrap().unwrap();
        assert_eq!(checkout.layout(), Layout::Repository, "{build}");
        assert_eq!(checkout.root(), absolute(root).unwrap(), "{build}");
    }
    assert!(default_dir(&executable("target/release")).is_none());
    assert!(default_dir(&executable("elsewhere")).is_none());
}
