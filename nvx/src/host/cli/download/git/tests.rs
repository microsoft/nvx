//! The tests of the history that `git` reads: from the checkout's root alone, however a path
//! names that root.
//!
//! Each test holds the tests' spawn lock while it runs `git`, so that on Windows no `git` inherits
//! the handles of a stand-in VM that a concurrent test starts, such as its log.

use std::fs;
use std::process::{Command, Stdio};

use super::*;
use crate::host::tests::{hold_spawns, spawn_locked};

/// Runs `git ARGUMENTS` in `directory`, without the user's or the system's Git configuration, and
/// checks that it succeeds.
fn git(directory: &Path, arguments: &[&str]) {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        .args([
            "-c",
            "user.name=nvx",
            "-c",
            "user.email=nvx@example.invalid",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", directory.join("missing-gitconfig"))
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = spawn_locked(&mut command).unwrap().wait().unwrap();
    assert!(
        status.success(),
        "git {arguments:?} failed in {}",
        directory.display()
    );
}

/// Makes `directory` a Git repository whose one commit has a `VERSION` of `version`.
fn repository(directory: &Path, version: &str) {
    fs::create_dir_all(directory).unwrap();
    git(directory, &["init", "--quiet"]);
    fs::write(directory.join("VERSION"), format!("{version}\n")).unwrap();
    git(directory, &["add", "VERSION"]);
    git(directory, &["commit", "--quiet", "-m", version]);
}

/// Makes `link` a link to the directory `target`: a symbolic link, or on Windows a junction,
/// which any user may create.
fn link(link: &Path, target: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    crate::host::tests::junction(link, target);
}

#[test]
fn a_checkout_that_is_no_repository_reads_none_above_it_however_named() {
    let _spawns = hold_spawns();
    let directory = tempfile::tempdir().unwrap();
    let outer = directory.path().join("outer");
    repository(&outer, "9.9.9");
    // A `.git` that is not a repository, as the checkout's own.
    let checkout = outer.join("checkout");
    fs::create_dir_all(checkout.join(".git")).unwrap();
    fs::create_dir(checkout.join("below")).unwrap();
    let linked = directory.path().join("linked");
    link(&linked, &checkout);
    for root in [&checkout, &linked, &checkout.join("below").join("..")] {
        let history = Git::new(root).unwrap();
        assert!(
            history.first_parent_commits(5).is_err(),
            "{} read the history of the repository above it",
            root.display()
        );
        assert_eq!(history.version_at("HEAD").unwrap(), None);
    }
}

#[test]
fn a_checkout_reads_none_above_a_parent_named_with_the_path_list_separator() {
    let _spawns = hold_spawns();
    let directory = tempfile::tempdir().unwrap();
    // `GIT_CEILING_DIRECTORIES` separates its directories with `;` on Windows and `:` elsewhere.
    let separator = if cfg!(windows) { ";" } else { ":" };
    let outer = directory.path().join(format!("outer{separator}repository"));
    repository(&outer, "9.9.9");
    let checkout = outer.join("checkout");
    fs::create_dir_all(checkout.join(".git")).unwrap();
    let history = Git::new(&checkout).unwrap();
    assert!(
        history.first_parent_commits(5).is_err(),
        "{} read the history of the repository above it",
        checkout.display()
    );
    assert_eq!(history.version_at("HEAD").unwrap(), None);
}

#[test]
fn a_checkout_below_a_parent_named_with_the_path_list_separator_reads_its_own_history() {
    let _spawns = hold_spawns();
    let directory = tempfile::tempdir().unwrap();
    let separator = if cfg!(windows) { ";" } else { ":" };
    let outer = directory.path().join(format!("outer{separator}repository"));
    repository(&outer, "9.9.9");
    let checkout = outer.join("checkout");
    repository(&checkout, "1.2.3");
    let history = Git::new(&checkout).unwrap();
    let commits = history.first_parent_commits(5).unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(
        history.version_at(&commits[0]).unwrap().as_deref(),
        Some("1.2.3")
    );
}

#[test]
fn a_checkout_named_through_a_link_reads_its_own_history() {
    let _spawns = hold_spawns();
    let directory = tempfile::tempdir().unwrap();
    let checkout = directory.path().join("checkout");
    repository(&checkout, "1.2.3");
    let linked = directory.path().join("linked");
    link(&linked, &checkout);
    let history = Git::new(&linked).unwrap();
    let commits = history.first_parent_commits(5).unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(
        history.version_at(&commits[0]).unwrap().as_deref(),
        Some("1.2.3")
    );
}
