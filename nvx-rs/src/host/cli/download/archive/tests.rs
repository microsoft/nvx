//! The tests of packages: the names of their members, their extraction with the host's `tar`,
//! and their checksums.

use super::*;

/// The root directory of the packages of the tests.
const ROOT: &str = "nvx-0.1.0-linux-kvm";

/// Writes the files `files`, by their paths below [`ROOT`], with their contents, below
/// `directory/ROOT`, with a `SHA256SUMS` that lists them, and returns the root.
fn package(directory: &Path, files: &[(&str, &str)]) -> PathBuf {
    let root = directory.join(ROOT);
    let mut sums = String::new();
    for (name, text) in files {
        let path = name
            .split('/')
            .fold(root.clone(), |path, part| path.join(part));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        sums.push_str(&format!("{}  {name}\n", sha256_file(&path).unwrap()));
    }
    fs::write(root.join(CHECKSUMS_NAME), sums).unwrap();
    root
}

/// The files of the packages of the tests.
const FILES: [(&str, &str); 4] = [
    ("SOURCE-MANIFEST.json", "{}\n"),
    ("bin/openvmm", "openvmm\n"),
    ("guest/vmlinux", "kernel\n"),
    ("provenance/openvmm.provenance.json", "{}\n"),
];

/// Archives the directory [`ROOT`] of `source` as `archive` with the host's `tar`: as a ZIP
/// archive for a name that ends with `.zip`, and as a gzipped tar archive otherwise.
fn archive(source: &Path, archive: &Path) {
    let mut command = Command::new(system_program("tar"));
    if archive
        .extension()
        .is_some_and(|extension| extension == "zip")
    {
        command.args(["-a", "-cf"]);
    } else {
        command.arg("-czf");
    }
    let status = spawn_command(command.arg(archive).arg(ROOT).current_dir(source))
        .unwrap()
        .wait()
        .unwrap();
    assert!(status.success());
}

/// Packages [`FILES`] as `name` in a staging directory of `directory`, after `change` changes
/// the package, and returns the archive.
fn staged(directory: &Path, name: &str, change: impl FnOnce(&Path)) -> PathBuf {
    let source = directory.join("source");
    let root = package(&source, &FILES);
    change(&root);
    let staging = directory.join("staging");
    fs::create_dir(&staging).unwrap();
    let path = staging.join(name);
    archive(&source, &path);
    path
}

#[test]
fn gzipped_packages_extract_once_their_names_and_checksums_pass() {
    let directory = tempfile::tempdir().unwrap();
    let archive = staged(directory.path(), "nvx-0.1.0-linux-kvm.tar.gz", |_| {});
    let package = extract(&archive).unwrap();
    assert_eq!(package, archive.with_file_name(EXTRACTION_DIR).join(ROOT));
    for (name, text) in FILES {
        let path = name
            .split('/')
            .fold(package.clone(), |path, part| path.join(part));
        assert_eq!(fs::read_to_string(path).unwrap(), text);
    }
}

#[cfg(windows)]
#[test]
fn zip_packages_extract_with_the_tar_of_windows() {
    let directory = tempfile::tempdir().unwrap();
    let archive = staged(directory.path(), "nvx-0.1.0-windows-whp.zip", |_| {});
    let package = extract(&archive).unwrap();
    assert_eq!(
        fs::read_to_string(package.join("guest").join("vmlinux")).unwrap(),
        "kernel\n"
    );
}

#[test]
fn packages_whose_checksums_fail_are_refused() {
    for (change, expected) in [
        (
            (|root: &Path| fs::write(root.join("guest").join("vmlinux"), "other\n").unwrap())
                as fn(&Path),
            "checksum mismatch for guest/vmlinux",
        ),
        (
            |root: &Path| fs::write(root.join("guest").join("extra"), "extra\n").unwrap(),
            "unlisted file in checksummed tree: guest/extra",
        ),
        (
            |root: &Path| fs::remove_file(root.join(CHECKSUMS_NAME)).unwrap(),
            "release package has no SHA256SUMS",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let archive = staged(directory.path(), "nvx-0.1.0-linux-kvm.tar.gz", change);
        let error = extract(&archive).unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::Unavailable));
        assert!(
            error.message().unwrap().contains(expected),
            "{expected}: {error:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn packages_with_symlinks_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    let archive = staged(directory.path(), "nvx-0.1.0-linux-kvm.tar.gz", |root| {
        std::os::unix::fs::symlink("/etc/passwd", root.join("guest").join("link")).unwrap();
    });
    let error = extract(&archive).unwrap_err();
    assert_eq!(
        error.message(),
        Some("release archive extracted a symlink: nvx-0.1.0-linux-kvm/guest/link")
    );
}

#[test]
fn archives_that_tar_cannot_read_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("nvx-0.1.0-linux-kvm.tar.gz");
    fs::write(&archive, "not an archive").unwrap();
    let error = extract(&archive).unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unavailable));
    assert!(
        error
            .message()
            .unwrap()
            .starts_with("tar cannot read the release archive: "),
        "{error:?}"
    );
}

#[test]
fn member_names_are_relative_and_canonical() {
    let (root, listed) = members("nvx/\nnvx/bin/\nnvx/bin/openvmm\nnvx/SHA256SUMS\n").unwrap();
    assert_eq!(root, "nvx");
    assert_eq!(
        listed.into_iter().collect::<Vec<_>>(),
        ["nvx", "nvx/SHA256SUMS", "nvx/bin", "nvx/bin/openvmm"]
    );
    for name in [
        "/nvx/x", "nvx/../x", "nvx/./x", "./nvx/x", "nvx//x", "nvx\\x", "C:/nvx/x", "nvx/a:b",
        "nvx/\tx",
    ] {
        let error = members(&format!("nvx/\n{name}\n")).unwrap_err();
        assert_eq!(
            error.message(),
            Some(format!("unsafe path in release archive: {name}").as_str())
        );
    }
}

#[test]
fn packages_have_one_root_and_unique_members() {
    for listing in ["a/x\nb/y\n", "a\nb\n", ""] {
        let error = members(listing).unwrap_err();
        assert_eq!(
            error.message(),
            Some("release archive must contain exactly one package root"),
            "{listing:?}"
        );
    }
    let error = members("nvx/x\nnvx/x/\n").unwrap_err();
    assert_eq!(
        error.message(),
        Some("duplicate path in release archive: nvx/x")
    );
}

#[test]
fn extractions_hold_the_listed_members_and_their_directories_alone() {
    let members: BTreeSet<String> = ["nvx/bin/openvmm", "nvx/SHA256SUMS"]
        .map(str::to_owned)
        .into();
    let mut extracted: BTreeMap<String, bool> = [
        ("nvx", true),
        ("nvx/bin", true),
        ("nvx/bin/openvmm", false),
        ("nvx/SHA256SUMS", false),
    ]
    .map(|(path, directory)| (path.to_owned(), directory))
    .into();
    check_extracted(&members, &extracted).unwrap();
    extracted.insert("nvx/extra".to_owned(), true);
    assert_eq!(
        check_extracted(&members, &extracted).unwrap_err().message(),
        Some("release archive extracted an unlisted path: nvx/extra")
    );
    extracted.remove("nvx/extra");
    extracted.remove("nvx/bin/openvmm");
    assert_eq!(
        check_extracted(&members, &extracted).unwrap_err().message(),
        Some("release archive did not extract nvx/bin/openvmm")
    );
}

#[test]
fn checksum_lists_name_each_file_once_and_nothing_else() {
    let directory = tempfile::tempdir().unwrap();
    let root = package(directory.path(), &FILES);
    let files: BTreeSet<&str> = FILES
        .iter()
        .map(|(name, _)| *name)
        .chain([CHECKSUMS_NAME])
        .collect();
    verify_checksums(&root, &files).unwrap();
    let digest = sha256_file(&root.join("guest").join("vmlinux")).unwrap();
    for (sums, expected) in [
        (
            format!("{digest} guest/vmlinux\n"),
            "malformed checksum line",
        ),
        (
            format!("{}  guest/vmlinux\n", digest.to_uppercase()),
            "malformed checksum line",
        ),
        (
            format!("{digest}  /guest/vmlinux\n"),
            "malformed checksum line",
        ),
        (format!("{digest}  guest/../x\n"), "malformed checksum line"),
        (format!("{digest}  SHA256SUMS\n"), "malformed checksum line"),
        (
            format!("{digest}  guest/vmlinux\n\n"),
            "malformed checksum line",
        ),
        (
            format!("{digest}  guest/vmlinux\n{digest}  guest/vmlinux\n"),
            "duplicate checksum path",
        ),
        (format!("{digest}  guest/other\n"), "invalid checksum path"),
        (
            format!("{digest}  guest/vmlinux\n"),
            "unlisted file in checksummed tree",
        ),
        ("caf\u{e9}\n".to_owned(), "must contain only ASCII text"),
    ] {
        fs::write(root.join(CHECKSUMS_NAME), &sums).unwrap();
        let error = verify_checksums(&root, &files).unwrap_err();
        assert!(
            error.message().unwrap().contains(expected),
            "{sums:?}: {error:?}"
        );
    }
}
