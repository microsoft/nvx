#[cfg(feature = "cli")]
use std::fs;

use super::*;

/// Returns `path` with `/` as the separator of the host.
fn native(path: &str) -> PathBuf {
    PathBuf::from(path.replace('/', std::path::MAIN_SEPARATOR_STR))
}

#[test]
fn paths_are_spelled_with_their_components() {
    for (path, expected) in [
        ("a//b", "a/b"),
        ("a/./b/.", "a/b"),
        ("a/b/", "a/b"),
        ("./a", "./a"),
        ("../a", "../a"),
        ("a/../b", "a/../b"),
        ("/a//b", "/a/b"),
    ] {
        let spelled = normalized(Path::new(path));
        assert_eq!(
            spelled.as_os_str(),
            native(expected).as_os_str(),
            "{path:?}"
        );
    }
    #[cfg(windows)]
    for (path, expected) in [
        ("C:/a//b", r"C:\a\b"),
        (r"\\server\share", r"\\server\share\"),
        (r"\\?\C:\a\work.", r"\\?\C:\a\work."),
        ("D:relative", "D:relative"),
        // `PathBuf::push` would remove these components of a path with a verbatim prefix.
        (r"\\?\C:\base\link\..\share", r"\\?\C:\base\link\..\share"),
        (r"\\?\C:\base\.\share", r"\\?\C:\base\.\share"),
    ] {
        assert_eq!(
            normalized(Path::new(path)).as_os_str(),
            expected,
            "{path:?}"
        );
    }
}

#[test]
fn relative_paths_join_the_directory_without_resolving_anything() {
    let directory = native("/fixture/cwd");
    let directory = absolute(&directory).unwrap();
    for (path, expected) in [
        ("link/share", directory.join("link").join("share")),
        (
            "link/../share",
            directory.join("link").join("..").join("share"),
        ),
    ] {
        assert_eq!(
            absolute_in(&directory, &native(path)).unwrap(),
            expected,
            "{path}"
        );
    }
    let absolute_path = directory.join("elsewhere");
    assert_eq!(
        absolute_in(&native("/other"), &absolute_path).unwrap(),
        absolute_path
    );
    assert_eq!(
        absolute(Path::new("relative")).unwrap(),
        std::env::current_dir().unwrap().join("relative")
    );
}

#[cfg(windows)]
#[test]
fn drive_relative_paths_are_below_the_directory_on_its_drive() {
    let current = std::env::current_dir().unwrap();
    let Some(letter) = drive(&current).map(char::from) else {
        // A current directory on a share has no drive of its own to compare.
        return;
    };
    // A directory on the current directory's drive that is not the current directory, as a
    // share's root is.
    let root = PathBuf::from(format!(r"{letter}:\fixture\share"));
    let lower = letter.to_ascii_lowercase();
    for (path, expected) in [
        (format!("{letter}:secret"), root.join("secret")),
        (
            format!(r"{lower}:secret\deeper"),
            root.join("secret").join("deeper"),
        ),
        (format!("{letter}:"), root.clone()),
    ] {
        assert_eq!(
            absolute_in(&root, Path::new(&path)).unwrap(),
            expected,
            "{path}"
        );
    }
    let verbatim = PathBuf::from(format!(r"\\?\{letter}:\fixture\share"));
    assert_eq!(
        absolute_in(&verbatim, Path::new(&format!("{letter}:secret"))).unwrap(),
        verbatim.join("secret")
    );
    // Another drive's path is below the current directory of that drive.
    let other = if letter == 'Q' { 'R' } else { 'Q' };
    let elsewhere = format!("{other}:secret");
    assert_eq!(
        absolute_in(&root, Path::new(&elsewhere)).unwrap(),
        std::path::absolute(&elsewhere).unwrap()
    );
    // The current directory is below its own drive's.
    assert_eq!(
        absolute(Path::new(&format!("{letter}:relative"))).unwrap(),
        current.join("relative")
    );
}

#[cfg(windows)]
#[test]
fn verbatim_directories_keep_the_components_after_them() {
    let directory = Path::new(r"\\?\C:\base");
    for (path, expected) in [
        (r"link\..\share", r"\\?\C:\base\link\..\share"),
        (r".\share", r"\\?\C:\base\.\share"),
        (r"C:link\..\share", r"\\?\C:\base\link\..\share"),
        (r"\rooted\..\x", r"\\?\C:\rooted\..\x"),
    ] {
        let joined = absolute_in(directory, Path::new(path)).unwrap();
        assert_eq!(joined.as_os_str(), expected, "{path}");
    }
}

#[cfg(feature = "cli")]
#[test]
fn resolution_follows_links_and_keeps_missing_components() {
    let directory = tempfile::tempdir().unwrap();
    let root = resolve(directory.path()).unwrap();
    assert!(root.is_absolute());
    let target = root.join("target");
    fs::create_dir(&target).unwrap();
    assert_eq!(resolve(&target).unwrap(), target);
    assert_eq!(
        resolve(&root.join("missing").join("deeper")).unwrap(),
        root.join("missing").join("deeper")
    );
    assert_eq!(
        resolve(&root.join("target").join("..").join("target")).unwrap(),
        target
    );
    let link = root.join("link");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&target, &link);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(&target, &link);
    if linked.is_ok() {
        assert_eq!(resolve(&link).unwrap(), target);
        assert_eq!(resolve(&link.join("child")).unwrap(), target.join("child"));
    }
}

#[cfg(all(feature = "cli", windows))]
#[test]
fn resolution_keeps_the_verbatim_path_that_alone_names_the_object() {
    let directory = tempfile::tempdir().unwrap();
    let verbatim = fs::canonicalize(directory.path()).unwrap();
    assert!(verbatim.to_str().unwrap().starts_with(r"\\?\"));
    let root = resolve(directory.path()).unwrap();
    assert!(!root.to_str().unwrap().starts_with(r"\\?\"), "{root:?}");
    // Windows drops the trailing period of a name in a path without the verbatim prefix.
    let dotted = PathBuf::from(format!(r"{}\work.", verbatim.display()));
    fs::create_dir(&dotted).unwrap();
    assert_eq!(resolve(&dotted).unwrap(), dotted);
}
