use super::host_path::display_path;
use super::*;
use crate::ErrorKind;
use crate::host::platform;

/// The lists of a filesystem policy as callers hold them, under the names that messages give
/// them.
#[derive(Debug, Default)]
struct FilesystemPolicy {
    readonly_paths: Vec<PathBuf>,
    readwrite_paths: Vec<PathBuf>,
    denied_paths: Vec<PathBuf>,
}

fn plan(policy: &FilesystemPolicy) -> Result<Option<ExportPlan>> {
    let mappings: Vec<Mapping<'_>> = Mapping::convention(
        PathList {
            name: "filesystem.readonlyPaths",
            paths: &policy.readonly_paths,
        },
        true,
    )
    .chain(Mapping::convention(
        PathList {
            name: "filesystem.readwritePaths",
            paths: &policy.readwrite_paths,
        },
        false,
    ))
    .collect();
    super::plan(&ExportRequest {
        denied: PathList {
            name: "filesystem.deniedPaths",
            paths: &policy.denied_paths,
        },
        ..ExportRequest::new(&mappings, ReservedTargets::BLOCKLESS_ROOT)
    })
}

fn root() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    for name in [
        "work/src/secret",
        "work/out",
        "work/private",
        "projects/app",
        "projects/private",
        "build-output",
        "tools/bin",
    ] {
        fs::create_dir_all(directory.path().join(name)).unwrap();
    }
    fs::write(directory.path().join("work/config.json"), b"{}").unwrap();
    fs::write(directory.path().join("work/run.sh"), b"true").unwrap();
    directory
}

fn canonical(path: &Path) -> PathBuf {
    canonicalize(path).unwrap()
}

fn sources(mapping: &ExportPlan) -> Vec<(String, bool)> {
    mapping
        .binds
        .iter()
        .map(|bind| (bind.guest_source(), bind.read_only))
        .collect()
}

fn option_values(arguments: &[String], option: &str) -> Vec<String> {
    arguments
        .windows(2)
        .filter(|pair| pair[0] == option)
        .map(|pair| pair[1].clone())
        .collect()
}

#[test]
fn guest_paths_follow_the_mxc_convention() {
    if cfg!(windows) {
        assert_eq!(
            guest_path(Path::new(r"C:\Work\src")).as_deref(),
            Some("/mnt/c/Work/src")
        );
        assert_eq!(guest_path(Path::new(r"D:\")).as_deref(), Some("/mnt/d"));
        assert_eq!(
            guest_path(Path::new(r"\\?\C:\x\.\y")).as_deref(),
            Some("/mnt/c/x/y")
        );
        assert_eq!(guest_path(Path::new(r"\\server\share\x")), None);
        assert_eq!(guest_path(Path::new(r"\x")), None);
    } else {
        assert_eq!(
            guest_path(Path::new("/home/me/src")).as_deref(),
            Some("/home/me/src")
        );
    }
    assert_eq!(guest_path(Path::new("relative")), None);
    assert_eq!(guest_path(Path::new("..")), None);
}

/// Verifies that a guest path is within itself and its ancestors only, comparing whole
/// components, as the reserved targets here and the sandbox's share checks require.
#[test]
fn guest_paths_are_within_themselves_and_their_ancestors() {
    for (path, directory) in [
        ("/run", "/run"),
        ("/run/nvx", "/run"),
        ("/run/nvx/t", "/run"),
        ("/", "/"),
        ("/run", "/"),
        ("/run/nvx", "/"),
    ] {
        assert!(within(path, directory), "{path} {directory}");
    }
    for (path, directory) in [
        ("/runner", "/run"),
        ("/run", "/run/nvx"),
        ("/srv/run", "/run"),
        ("/", "/run"),
    ] {
        assert!(!within(path, directory), "{path} {directory}");
    }
}

/// The layout of microsoft/nvx#281: directories that share no directory but the volume's
/// root, each with its own access.
#[test]
fn unrelated_directories_are_exported_with_their_own_access() {
    let directory = root();
    let base = directory.path();
    let policy = FilesystemPolicy {
        readonly_paths: vec![base.join("tools")],
        readwrite_paths: vec![base.join("projects/app"), base.join("build-output")],
        ..FilesystemPolicy::default()
    };
    let mapping = plan(&policy).unwrap().unwrap();
    let children: Vec<(PathBuf, bool)> = mapping
        .children
        .iter()
        .map(|child| (child.root.clone(), child.writable))
        .collect();
    assert_eq!(
        children,
        [
            (canonical(&base.join("build-output")), true),
            (canonical(&base.join("projects/app")), true),
            (canonical(&base.join("tools")), false),
        ]
    );
    assert!(mapping.children.iter().all(|child| child.write.is_empty()
        && child.allowed.is_empty()
        && child.denied.is_empty()));
    // Parents mount before children, so the shallower targets come first.
    assert_eq!(
        sources(&mapping),
        [
            ("0".to_owned(), false),
            ("2".to_owned(), true),
            ("1".to_owned(), false)
        ]
    );

    let arguments = mapping.openvmm_arguments();
    assert_eq!(arguments[..2], ["--mount-aggregate", GUEST_EXPORT]);
    assert_eq!(
        option_values(&arguments, "--mount-child"),
        [
            format!(
                "0,{},rw",
                display_path(&canonical(&base.join("build-output")))
            ),
            format!(
                "1,{},rw",
                display_path(&canonical(&base.join("projects/app")))
            ),
            format!("2,{},ro", display_path(&canonical(&base.join("tools")))),
        ]
    );
    assert_eq!(arguments.len(), 8);
    assert_eq!(mapping.kernel_token(), "nvx_maps=3");
    let table = mapping.guest_table();
    assert_eq!(table.len(), 3);
    assert_eq!(
        table[1].target,
        guest_path(&canonical(&base.join("tools"))).unwrap()
    );
    assert!(table[1].read_only);
}

/// The layout of microsoft/nvx#283: directories whose names hold commas, each with a denied
/// subdirectory.
#[test]
fn host_paths_may_contain_commas() {
    let directory = root();
    let base = directory.path();
    let alpha = base.join("projects/alpha,beta");
    let reports = base.join("reports/2026, final");
    for path in [&alpha, &reports] {
        fs::create_dir_all(path.join("secret")).unwrap();
    }
    let policy = FilesystemPolicy {
        readonly_paths: vec![reports.clone()],
        readwrite_paths: vec![alpha.clone()],
        denied_paths: vec![alpha.join("secret"), reports.join("secret")],
    };
    let mapping = plan(&policy).unwrap().unwrap();
    let children: Vec<(PathBuf, bool, Vec<PathBuf>)> = mapping
        .children
        .iter()
        .map(|child| (child.root.clone(), child.writable, child.denied.clone()))
        .collect();
    assert_eq!(
        children,
        [
            (
                canonical(&alpha),
                true,
                vec![canonical(&alpha.join("secret"))]
            ),
            (
                canonical(&reports),
                false,
                vec![canonical(&reports.join("secret"))]
            ),
        ]
    );
    // OpenVMM reads each child's mode after the last comma, so the host paths keep theirs.
    let arguments = mapping.openvmm_arguments();
    assert_eq!(
        option_values(&arguments, "--mount-child"),
        [
            format!("0,{},rw", display_path(&canonical(&alpha))),
            format!("1,{},ro", display_path(&canonical(&reports))),
        ]
    );
    assert_eq!(
        option_values(&arguments, "--mount-deny"),
        [
            display_path(&canonical(&alpha.join("secret"))),
            display_path(&canonical(&reports.join("secret"))),
        ]
    );
    // So do the guest paths, which the control channel carries with their lengths.
    let mut table: Vec<(String, bool)> = mapping
        .guest_table()
        .into_iter()
        .map(|map| (map.target, map.read_only))
        .collect();
    table.sort();
    assert_eq!(
        table,
        [
            (guest_path(&canonical(&alpha)).unwrap(), false),
            (guest_path(&canonical(&reports)).unwrap(), true),
        ]
    );
    assert!(table[0].0.ends_with("/projects/alpha,beta"), "{table:?}");
    assert!(table[1].0.ends_with("/reports/2026, final"), "{table:?}");
}

#[test]
fn mapped_files_are_exported_alone() {
    let directory = root();
    let base = directory.path();
    let policy = FilesystemPolicy {
        readonly_paths: vec![base.join("work/config.json"), base.join("work/src")],
        readwrite_paths: vec![base.join("work/run.sh"), base.join("work/out")],
        denied_paths: vec![
            // Not exported at all: no child holds it.
            base.join("work/private"),
            base.join("work/src/secret"),
        ],
    };
    let mapping = plan(&policy).unwrap().unwrap();
    let work = canonical(&base.join("work"));
    // Each file is a child of its own, without its directory, beside the directories.
    let children: Vec<(PathBuf, bool, bool)> = mapping
        .children
        .iter()
        .map(|child| (child.root.clone(), child.file, child.writable))
        .collect();
    assert_eq!(
        children,
        [
            (work.join("config.json"), true, false),
            (work.join("out"), false, true),
            (work.join("run.sh"), true, true),
            (work.join("src"), false, false),
        ]
    );
    assert!(
        mapping
            .children
            .iter()
            .all(|child| child.allowed.is_empty() && child.write.is_empty())
    );
    assert_eq!(
        mapping.children[3].denied,
        [work.join("src").join("secret")]
    );
    assert_eq!(
        sources(&mapping),
        [
            ("0".to_owned(), true),
            ("1".to_owned(), false),
            ("2".to_owned(), false),
            ("3".to_owned(), true),
        ]
    );

    // OpenVMM exposes each file as the child itself, so nothing hides its directory, and the
    // `file` flag makes it refuse a directory in the file's place.
    let arguments = mapping.openvmm_arguments();
    assert_eq!(
        option_values(&arguments, "--mount-child")[0],
        format!("0,{},ro,file", display_path(&work.join("config.json")))
    );
    assert_eq!(
        option_values(&arguments, "--mount-deny"),
        [display_path(&work.join("src").join("secret"))]
    );
    assert!(option_values(&arguments, "--mount-allow").is_empty());
    assert!(option_values(&arguments, "--mount-write").is_empty());

    // A record without file children leaves the field out, and one with them names it.
    let record = serde_json::to_value(&mapping).unwrap();
    assert_eq!(record["children"][0]["file"], true);
    assert!(record["children"][1].get("file").is_none());
    let restored: ExportPlan = serde_json::from_value(record).unwrap();
    assert_eq!(restored, mapping);

    // A file inside a mapped directory belongs to that directory's child, whose writes
    // OpenVMM limits to the file when the directory is read-only.
    let nested = FilesystemPolicy {
        readonly_paths: vec![base.join("work")],
        readwrite_paths: vec![base.join("work/config.json")],
        ..FilesystemPolicy::default()
    };
    let mapping = plan(&nested).unwrap().unwrap();
    assert_eq!(mapping.children.len(), 1);
    assert!(!mapping.children[0].file && mapping.children[0].writable);
    assert_eq!(mapping.children[0].write, [work.join("config.json")]);
    assert_eq!(
        sources(&mapping),
        [("0".to_owned(), true), ("0/config.json".to_owned(), false)]
    );

    // One file reached through two names is one object, which is mapped once.
    let link = base.join("work/config-link.json");
    fs::hard_link(base.join("work/config.json"), &link).unwrap();
    let aliased = FilesystemPolicy {
        readonly_paths: vec![base.join("work/config.json"), link],
        ..FilesystemPolicy::default()
    };
    let error = plan(&aliased).unwrap_err();
    assert!(
        error.message().contains("same host directory or file"),
        "{error}"
    );
}

#[test]
fn policies_beyond_the_children_of_an_aggregate_are_refused() {
    let directory = root();
    let base = directory.path();
    let files = |parent: &str, count: usize| -> Vec<PathBuf> {
        (0..count)
            .map(|index| {
                let path = base.join(parent).join(format!("file-{index:03}"));
                fs::write(&path, b"x").unwrap();
                path
            })
            .collect()
    };
    // Up to 256 files are children of their own, however many share a directory.
    let policy = FilesystemPolicy {
        readonly_paths: files("tools", 256),
        ..FilesystemPolicy::default()
    };
    let mapping = plan(&policy).unwrap().unwrap();
    assert_eq!(mapping.children.len(), 256);
    assert!(
        mapping
            .children
            .iter()
            .all(|child| child.file && child.allowed.is_empty())
    );

    // More are refused rather than exported through their directory, which would grant
    // more than the files.
    let mut readonly_paths = files("tools", 256);
    readonly_paths.extend(files("build-output", 1));
    let error = plan(&FilesystemPolicy {
        readonly_paths,
        ..FilesystemPolicy::default()
    })
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(error.message().contains("at most 256"), "{error}");
    assert!(
        error
            .message()
            .contains("map directories that contain the files instead"),
        "{error}"
    );

    // A policy that grants the directory instead needs one child.
    let mut readonly_paths = vec![base.join("tools")];
    readonly_paths.extend(files("build-output", 1));
    let mapping = plan(&FilesystemPolicy {
        readonly_paths,
        ..FilesystemPolicy::default()
    })
    .unwrap()
    .unwrap();
    assert_eq!(mapping.children.len(), 2);
}

#[test]
fn nested_mappings_merge_into_the_outermost_directory() {
    let directory = root();
    let base = directory.path();
    fs::create_dir_all(base.join("work/out/cache")).unwrap();
    let policy = FilesystemPolicy {
        readonly_paths: vec![base.join("work"), base.join("work/src")],
        readwrite_paths: vec![base.join("work/out"), base.join("work/out/cache")],
        ..FilesystemPolicy::default()
    };
    let mapping = plan(&policy).unwrap().unwrap();
    assert_eq!(mapping.children.len(), 1);
    let child = &mapping.children[0];
    let work = canonical(&base.join("work"));
    assert!(child.writable);
    // The read-only root is read-only on the host, except the outermost read-write mapping
    // inside it, to which OpenVMM limits writes.
    assert_eq!(child.write, [work.join("out")]);
    assert_eq!(
        sources(&mapping),
        [
            ("0".to_owned(), true),
            ("0/out".to_owned(), false),
            ("0/src".to_owned(), true),
            ("0/out/cache".to_owned(), false),
        ]
    );

    // A read-write root covers everything, so OpenVMM need not limit writes.
    let policy = FilesystemPolicy {
        readwrite_paths: vec![base.join("work"), base.join("work/src")],
        ..FilesystemPolicy::default()
    };
    let mapping = plan(&policy).unwrap().unwrap();
    assert!(mapping.children[0].writable && mapping.children[0].write.is_empty());
    assert_eq!(
        sources(&mapping),
        [("0".to_owned(), false), ("0/src".to_owned(), false)]
    );

    // The read-write duplicate of a read-only path is tightened to read-only, so a
    // read-write path inside it is the only writable one.
    let policy = FilesystemPolicy {
        readonly_paths: vec![base.join("work")],
        readwrite_paths: vec![base.join("work"), base.join("work/src")],
        ..FilesystemPolicy::default()
    };
    let mapping = plan(&policy).unwrap().unwrap();
    assert!(mapping.children[0].writable);
    assert_eq!(mapping.children[0].write, [work.join("src")]);
    assert_eq!(
        sources(&mapping),
        [("0".to_owned(), true), ("0/src".to_owned(), false)]
    );
}

/// Only the guest's bind mount could keep a read-only path inside a read-write directory
/// read-only, so such a path is refused on every host, under any of its names, while the
/// inverse nesting and denied paths inside a read-write directory stay valid.
#[test]
fn read_only_paths_inside_read_write_directories_are_refused() {
    let directory = root();
    let base = directory.path();
    fs::create_dir(base.join("work/output")).unwrap();
    let policy = |readonly_paths: &[PathBuf], readwrite_paths: &[PathBuf]| FilesystemPolicy {
        readonly_paths: readonly_paths.to_vec(),
        readwrite_paths: readwrite_paths.to_vec(),
        ..FilesystemPolicy::default()
    };
    let refusal = |inner: &Path, outer: &Path| {
        format!(
            "filesystem.readonlyPaths entry {} lies inside the filesystem.readwritePaths \
                 entry {},",
            inner.display(),
            outer.display()
        )
    };
    let work = base.join("work");
    let (src, config) = (work.join("src"), work.join("config.json"));
    let secret = src.join("secret");
    // A directory, a file, a path deeper inside, a path that is also listed read-write, and a
    // path inside a read-write directory that itself lies inside a read-only one.
    for (readonly, readwrite, inner, outer) in [
        (vec![src.clone()], vec![work.clone()], &src, &work),
        (vec![config.clone()], vec![work.clone()], &config, &work),
        (vec![secret.clone()], vec![work.clone()], &secret, &work),
        (
            vec![src.clone()],
            vec![work.clone(), src.clone()],
            &src,
            &work,
        ),
        (
            vec![work.clone(), secret.clone()],
            vec![src.clone()],
            &secret,
            &src,
        ),
    ] {
        let error = plan(&policy(&readonly, &readwrite)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(
            error.message().starts_with(&refusal(inner, outer)),
            "{error}"
        );
    }
    // Resolved paths decide, so another name of a nested path is refused too: a spelling in
    // other letter case on Windows, and one through a symbolic link elsewhere.
    #[cfg(windows)]
    let alias = base.join("WORK").join("CONFIG.JSON");
    #[cfg(unix)]
    let alias = {
        std::os::unix::fs::symlink(&work, base.join("work-link")).unwrap();
        base.join("work-link").join("config.json")
    };
    let error = plan(&policy(
        std::slice::from_ref(&alias),
        std::slice::from_ref(&work),
    ))
    .unwrap_err();
    assert!(
        error.message().starts_with(&refusal(&alias, &work)),
        "{error}"
    );

    // The inverse nesting is valid: OpenVMM limits the read-only directory's writes to the
    // read-write path inside it.
    let mapping = plan(&policy(std::slice::from_ref(&work), &[work.join("out")]))
        .unwrap()
        .unwrap();
    assert_eq!(mapping.children[0].write, [canonical(&work.join("out"))]);
    // Containment compares whole components, so work/output lies beside work/out.
    let mapping = plan(&policy(&[work.join("output")], &[work.join("out")]))
        .unwrap()
        .unwrap();
    assert_eq!(mapping.children.len(), 2);
    let denied = FilesystemPolicy {
        readwrite_paths: vec![work.clone()],
        denied_paths: vec![work.join("private")],
        ..FilesystemPolicy::default()
    };
    assert_eq!(plan(&denied).unwrap().unwrap().children[0].denied.len(), 1);
}

#[test]
fn denied_paths_belong_to_the_directory_that_contains_them() {
    let directory = root();
    let base = directory.path();
    let policy = FilesystemPolicy {
        readonly_paths: vec![base.join("work/src"), base.join("projects")],
        readwrite_paths: vec![base.join("build-output")],
        denied_paths: vec![
            base.join("work/src/secret"),
            base.join("projects/private"),
            base.join("work/out"),
            base.join("elsewhere"),
        ],
    };
    let mapping = plan(&policy).unwrap().unwrap();
    let denied: Vec<Vec<PathBuf>> = mapping
        .children
        .iter()
        .map(|child| child.denied.clone())
        .collect();
    assert_eq!(
        denied,
        [
            Vec::new(),
            vec![canonical(&base.join("projects/private"))],
            vec![canonical(&base.join("work/src/secret"))],
        ]
    );
    // Nothing a workload could move is pinned without a read-write parent.
    assert!(
        mapping
            .children
            .iter()
            .flat_map(|child| &child.denied_identities)
            .all(Option::is_none)
    );
    assert_eq!(
        option_values(&mapping.openvmm_arguments(), "--mount-deny").len(),
        2
    );
}

/// Checks the order, duplicates, and containment by whole components that the roots, denied
/// paths, and writable paths share, which needs no host paths.
#[test]
fn outermost_paths_are_sorted_once_without_the_paths_inside_others() {
    let paths = ["/b/c", "/a/bc", "/b", "/a/b", "/b/c/d", "/a/b"].map(PathBuf::from);
    assert_eq!(
        outermost(paths.to_vec()),
        ["/a/b", "/a/bc", "/b"].map(PathBuf::from)
    );
}

#[test]
fn unenforceable_policies_are_rejected() {
    let directory = root();
    let base = directory.path();
    let cases = [
        FilesystemPolicy {
            readonly_paths: vec![base.join("missing")],
            ..FilesystemPolicy::default()
        },
        FilesystemPolicy {
            readwrite_paths: vec![base.join("work/out")],
            denied_paths: vec![base.join("work")],
            ..FilesystemPolicy::default()
        },
        FilesystemPolicy {
            readwrite_paths: vec![base.join("work/out")],
            denied_paths: vec![base.join("work/out/not-yet")],
            ..FilesystemPolicy::default()
        },
        FilesystemPolicy {
            readonly_paths: vec![base.join("work/config.json")],
            denied_paths: vec![base.join("work")],
            ..FilesystemPolicy::default()
        },
        // A whole volume is never exported.
        FilesystemPolicy {
            readonly_paths: vec![PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" })],
            ..FilesystemPolicy::default()
        },
    ];
    for policy in cases {
        assert_eq!(
            plan(&policy).unwrap_err().kind(),
            ErrorKind::Unsupported,
            "{policy:?}"
        );
    }
    if !cfg!(windows) {
        let system = FilesystemPolicy {
            readonly_paths: vec![PathBuf::from("/usr")],
            ..FilesystemPolicy::default()
        };
        assert_eq!(plan(&system).unwrap_err().kind(), ErrorKind::Unsupported);
    }
    let empty = FilesystemPolicy {
        denied_paths: vec![base.join("tools")],
        ..FilesystemPolicy::default()
    };
    assert_eq!(plan(&empty).unwrap(), None);
}

#[test]
fn whole_volumes_are_refused() {
    // The paths are only inspected, so nothing is created at a volume root. A file directly
    // in a volume's root is exported alone, so only a whole volume is refused.
    let (volumes, directories): (&[&str], &[&str]) = if cfg!(windows) {
        (&[r"C:\", r"D:\"], &[r"C:\work", r"D:\a\b"])
    } else {
        (&["/"], &["/work", "/srv/a/b"])
    };
    for volume in volumes {
        let reason = volume_root_refusal(Path::new(volume)).unwrap();
        assert!(reason.contains("whole volume"), "{volume}: {reason}");
    }
    for directory in directories {
        assert_eq!(volume_root_refusal(Path::new(directory)), None);
    }
}

#[test]
fn spellings_of_one_object_share_its_guest_path() {
    let directory = root();
    let base = directory.path();
    let upper = if cfg!(windows) {
        base.join("WORK").join("SRC")
    } else {
        base.join("work").join(".").join("src")
    };
    let policy = FilesystemPolicy {
        readonly_paths: vec![upper],
        readwrite_paths: vec![base.join("work").join("src")],
        ..FilesystemPolicy::default()
    };
    let mapping = plan(&policy).unwrap().unwrap();
    assert_eq!(mapping.binds.len(), 1);
    assert!(mapping.binds[0].read_only);
    assert!(!mapping.children[0].writable);
    assert_eq!(
        mapping.binds[0].target,
        guest_path(&canonical(&base.join("work").join("src"))).unwrap()
    );
}

#[cfg(windows)]
#[test]
fn resolved_guest_paths_canonicalize_temp_directory_case_aliases() {
    let directory = root();
    let path = directory.path().join("work").join("out");
    let upper = PathBuf::from(path.to_str().unwrap().to_uppercase());
    let expected = guest_path(&canonical(&path)).unwrap();
    assert_ne!(guest_path(&upper), Some(expected.clone()));
    assert_eq!(resolve_guest_path(&upper), Some(expected));
}

#[cfg(windows)]
#[test]
fn paths_that_only_a_verbatim_path_names_are_refused() {
    let directory = root();
    let base = directory.path();
    // A verbatim path keeps a trailing period or space of a name, which Windows drops from a
    // path without the prefix: there, `work.` and `work ` would name `work`, and `solo.`
    // nothing.
    let verbatim = fs::canonicalize(base).unwrap();
    let named = |name: &str| PathBuf::from(format!(r"{}\{name}", verbatim.display()));
    for name in ["work.", "work ", "solo.", r"work\src\secret."] {
        fs::create_dir(named(name)).unwrap();
    }
    for name in ["work.", "work ", "solo."] {
        let path = named(name);
        let error = canonicalize(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported, "{error}");
        let policy = FilesystemPolicy {
            readwrite_paths: vec![path],
            ..FilesystemPolicy::default()
        };
        let error = plan(&policy).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.to_string().contains("is not accessible"), "{error}");
    }
    // A denied path that only a verbatim path names would hide another path instead.
    let policy = FilesystemPolicy {
        readwrite_paths: vec![base.join("work")],
        denied_paths: vec![named(r"work\src\secret.")],
        ..FilesystemPolicy::default()
    };
    let error = plan(&policy).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(error.to_string().contains("cannot be resolved"), "{error}");
    // A verbatim path that names what the path without its prefix names is planned as that
    // path.
    let policy = FilesystemPolicy {
        readwrite_paths: vec![named("work")],
        ..FilesystemPolicy::default()
    };
    let exported = plan(&policy).unwrap().unwrap();
    assert_eq!(exported.children[0].root, canonical(&base.join("work")));
}

#[test]
fn policy_paths_follow_openvmm_rules() {
    let directory = root();
    let base = directory.path();
    for name in ["work/out/My Secrets", "work/out/ leading", "my files"] {
        fs::create_dir_all(base.join(name)).unwrap();
    }
    fs::write(base.join("my files/notes one.txt"), b"notes").unwrap();
    // Names may contain spaces, in a file that a child exposes alone and in a policy path.
    let spaced = FilesystemPolicy {
        readonly_paths: vec![base.join("my files/notes one.txt")],
        readwrite_paths: vec![base.join("work/out")],
        denied_paths: vec![base.join("work/out/My Secrets")],
    };
    let mapping = plan(&spaced).unwrap().unwrap();
    let arguments = mapping.openvmm_arguments();
    assert!(
        option_values(&arguments, "--mount-child")[0].ends_with("notes one.txt,ro,file"),
        "{arguments:?}"
    );
    assert!(option_values(&arguments, "--mount-deny")[0].ends_with("My Secrets"));
    // But they must not begin or end with one.
    let leading = FilesystemPolicy {
        readwrite_paths: vec![base.join("work/out")],
        denied_paths: vec![base.join("work/out/ leading")],
        ..FilesystemPolicy::default()
    };
    assert_eq!(plan(&leading).unwrap_err().kind(), ErrorKind::Unsupported);
    if !cfg!(windows) {
        fs::create_dir_all(base.join("work/out/tab\there")).unwrap();
        fs::create_dir_all(base.join("work/out/colon:here")).unwrap();
        for name in ["tab\there", "colon:here"] {
            let policy = FilesystemPolicy {
                readwrite_paths: vec![base.join("work/out")],
                denied_paths: vec![base.join("work/out").join(name)],
                ..FilesystemPolicy::default()
            };
            assert_eq!(
                plan(&policy).unwrap_err().kind(),
                ErrorKind::Unsupported,
                "{name}"
            );
        }
    }
}

#[test]
fn openvmm_limits_are_checked_per_directory() {
    let directory = root();
    let base = directory.path();
    let files = |count: usize| -> Vec<PathBuf> {
        (0..count)
            .map(|index| {
                let path = base.join("tools").join(format!("tool-{index:03}"));
                fs::write(&path, b"x").unwrap();
                path
            })
            .collect()
    };
    // One exported directory hides at most 128 denied paths, and singles out at most 128
    // read-write paths inside a read-only one.
    for policy in [
        FilesystemPolicy {
            readonly_paths: vec![base.join("tools")],
            denied_paths: files(129),
            ..FilesystemPolicy::default()
        },
        FilesystemPolicy {
            readonly_paths: vec![base.join("tools")],
            readwrite_paths: files(129),
            ..FilesystemPolicy::default()
        },
    ] {
        let error = plan(&policy).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.message().contains("at most 128"), "{error}");
    }
}

#[test]
fn swapped_objects_fail_verification() {
    let directory = root();
    let base = directory.path();
    // Everything below the read-write work directory is pinned.
    let policy = FilesystemPolicy {
        readwrite_paths: vec![base.join("work"), base.join("work/src")],
        denied_paths: vec![base.join("work/src/secret")],
        ..FilesystemPolicy::default()
    };
    let mapping = plan(&policy).unwrap().unwrap();
    assert!(
        mapping.children[0]
            .denied_identities
            .iter()
            .all(Option::is_some)
    );
    mapping.verify().unwrap();
    fs::rename(base.join("work/src/secret"), base.join("work/src/moved")).unwrap();
    fs::create_dir(base.join("work/src/secret")).unwrap();
    assert_eq!(mapping.verify().unwrap_err().kind(), ErrorKind::Other);
    fs::remove_dir(base.join("work/src/secret")).unwrap();
    fs::rename(base.join("work/src/moved"), base.join("work/src/secret")).unwrap();
    mapping.verify().unwrap();
    fs::rename(base.join("work/src"), base.join("work/src-moved")).unwrap();
    fs::create_dir_all(base.join("work/src/secret")).unwrap();
    assert_eq!(mapping.verify().unwrap_err().kind(), ErrorKind::Other);

    // Without a read-write parent, nothing a workload could move is pinned, so host edits
    // that replace objects do not block the next start.
    let read_only = FilesystemPolicy {
        readonly_paths: vec![base.join("tools")],
        ..FilesystemPolicy::default()
    };
    let mapping = plan(&read_only).unwrap().unwrap();
    fs::remove_dir_all(base.join("tools")).unwrap();
    fs::create_dir(base.join("tools")).unwrap();
    mapping.verify().unwrap();

    // Every child keeps its kind: a directory in place of a file child would grant all of it,
    // and a directory child must remain a directory.
    let settings = base.join("settings.json");
    fs::write(&settings, b"{}").unwrap();
    let with_file = FilesystemPolicy {
        readonly_paths: vec![settings.clone(), base.join("tools")],
        ..FilesystemPolicy::default()
    };
    let mapping = plan(&with_file).unwrap().unwrap();
    mapping.verify().unwrap();
    fs::remove_file(&settings).unwrap();
    fs::create_dir(&settings).unwrap();
    assert_eq!(mapping.verify().unwrap_err().kind(), ErrorKind::Other);
    fs::remove_dir(&settings).unwrap();
    fs::write(&settings, b"{}").unwrap();
    mapping.verify().unwrap();
    fs::remove_dir(base.join("tools")).unwrap();
    fs::write(base.join("tools"), b"").unwrap();
    assert_eq!(mapping.verify().unwrap_err().kind(), ErrorKind::Other);
}

/// A path that no longer reaches its object names it no longer, but any other failure to
/// inspect an object shows nothing of it, so it is the host's own, with its cause.
#[test]
fn objects_that_cannot_be_inspected_fail_verification_by_what_the_failure_shows() {
    let directory = root();
    let base = directory.path();
    let policy = FilesystemPolicy {
        readwrite_paths: vec![base.join("work"), base.join("work/src")],
        denied_paths: vec![base.join("work/src/secret")],
        ..FilesystemPolicy::default()
    };
    let mapping = plan(&policy).unwrap().unwrap();
    mapping.verify().unwrap();
    // A pinned object at a name that the host's file system cannot hold.
    let mut unnamed = mapping.clone();
    let pinned = unnamed
        .binds
        .iter()
        .position(|bind| bind.identity.is_some())
        .unwrap();
    unnamed.binds[pinned].source = "n".repeat(300);
    let bind = &unnamed.binds[pinned];
    let path = unnamed.children[bind.child].root.join(&bind.source);
    let error = unnamed.verify().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert_eq!(
        error.message(),
        format!("cannot inspect {}", path.display())
    );
    let source = std::error::Error::source(&error).unwrap();
    assert!(
        source.downcast_ref::<std::io::Error>().is_some(),
        "{source}"
    );
    // A pinned object that went.
    fs::remove_dir(base.join("work/src/secret")).unwrap();
    let error = mapping.verify().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other, "{error}");
    assert!(error.message().contains("no longer names"), "{error}");
    // A link in its place that names itself, which a workload that may write the directory could
    // put there, and through which the host reaches no object.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("secret", base.join("work/src/secret")).unwrap();
        let error = mapping.verify().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Other, "{error}");
        assert!(error.message().contains("no longer names"), "{error}");
        let source = std::error::Error::source(&error).unwrap();
        assert_eq!(
            source
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(nix::errno::Errno::ELOOP as i32)
        );
    }
    // How the host failed to inspect an object tells whether its path still reaches it.
    let path = base.join("work");
    for (kind, expected) in [
        (std::io::ErrorKind::NotFound, ErrorKind::Other),
        (std::io::ErrorKind::NotADirectory, ErrorKind::Other),
        (std::io::ErrorKind::PermissionDenied, ErrorKind::Other),
        (std::io::ErrorKind::StorageFull, ErrorKind::Io),
        (std::io::ErrorKind::OutOfMemory, ErrorKind::Io),
        (std::io::ErrorKind::InvalidFilename, ErrorKind::Io),
        (std::io::ErrorKind::Other, ErrorKind::Io),
    ] {
        let error = uninspected(&path, kind.into());
        assert_eq!(error.kind(), expected, "{kind:?}");
        let source = std::error::Error::source(&error).unwrap();
        assert_eq!(
            source.downcast_ref::<std::io::Error>().unwrap().kind(),
            kind
        );
    }
    // A path whose links loop, which the host reports with a code of its own.
    #[cfg(unix)]
    let looped = nix::errno::Errno::ELOOP as i32;
    #[cfg(windows)]
    let looped = windows_sys::Win32::Foundation::ERROR_CANT_RESOLVE_FILENAME as i32;
    #[cfg(any(unix, windows))]
    {
        let error = uninspected(&path, std::io::Error::from_raw_os_error(looped));
        assert_eq!(error.kind(), ErrorKind::Other, "{error}");
        let source = std::error::Error::source(&error).unwrap();
        assert_eq!(
            source
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(looped)
        );
    }
}

/// A recorded plan with a read-only bind in a part of a child that the guest may write, which
/// only the guest's bind mount would keep read-only, is refused before a guest runs.
#[test]
fn read_only_binds_that_the_host_cannot_enforce_fail_verification() {
    let directory = root();
    let base = directory.path();
    fs::create_dir_all(base.join("work/out/cache")).unwrap();
    let refused = |mut mapping: ExportPlan, source: &str| {
        let bind = mapping
            .binds
            .iter_mut()
            .find(|bind| bind.source == source)
            .unwrap();
        bind.read_only = true;
        let error = mapping.verify().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(
            error.message().contains("read-only inside a part of"),
            "{error}"
        );
    };
    // A read-only path inside a read-write root, all of which the guest may write.
    let writable_root = plan(&FilesystemPolicy {
        readwrite_paths: vec![base.join("work"), base.join("work/src")],
        ..FilesystemPolicy::default()
    })
    .unwrap()
    .unwrap();
    writable_root.verify().unwrap();
    refused(writable_root, "src");
    // A read-only path inside a writable path of a read-only root, whose other read-only
    // paths stay read-only on the host.
    let writable_path = plan(&FilesystemPolicy {
        readonly_paths: vec![base.join("work"), base.join("work/src")],
        readwrite_paths: vec![base.join("work/out"), base.join("work/out/cache")],
        ..FilesystemPolicy::default()
    })
    .unwrap()
    .unwrap();
    writable_path.verify().unwrap();
    refused(writable_path, "out/cache");
}

/// A directory on another volume than the temporary directory, if the host has one: the one
/// that `NVX_TEST_SECOND_VOLUME` names, or the runner's temporary directory
/// on GitHub's Windows runners. Tests create their files below it. Linux's `/dev/shm` does
/// not qualify, because it lies in the guest's reserved `/dev`.
fn second_volume() -> Option<PathBuf> {
    let candidate = std::env::var_os("NVX_TEST_SECOND_VOLUME")
        .or_else(|| std::env::var_os("RUNNER_TEMP"))
        .map(PathBuf::from)?;
    let temporary = canonical(&std::env::temp_dir());
    let candidate = canonicalize(&candidate).ok()?;
    let volume = |path: &Path| platform::file_identity(path).ok().map(|(device, _)| device);
    (candidate.is_dir() && volume(&candidate) != volume(&temporary)).then_some(candidate)
}

#[test]
fn directories_on_other_volumes_are_exported_too() {
    let Some(volume) = second_volume() else {
        eprintln!("skipped: the host has no second volume");
        return;
    };
    let directory = root();
    let other = tempfile::tempdir_in(volume).unwrap();
    fs::create_dir(other.path().join("cache")).unwrap();
    let policy = FilesystemPolicy {
        readonly_paths: vec![directory.path().join("tools")],
        readwrite_paths: vec![other.path().join("cache")],
        ..FilesystemPolicy::default()
    };
    let mapping = plan(&policy).unwrap().unwrap();
    assert_eq!(mapping.children.len(), 2);
    let modes: Vec<bool> = mapping
        .children
        .iter()
        .map(|child| child.writable)
        .collect();
    assert!(modes.contains(&true) && modes.contains(&false));
}
/// Returns a mapping of `path` at the guest path `target`.
fn at<'a>(path: &'a Path, target: &'a str, read_only: bool) -> Mapping<'a> {
    Mapping {
        name: "mappings",
        path,
        target: Target::Guest(target),
        read_only,
    }
}

#[test]
fn explicit_targets_place_mappings_at_any_free_guest_path() {
    let directory = root();
    let base = directory.path();
    let (work, tools) = (base.join("work"), base.join("tools"));
    let config = work.join("config.json");
    let mappings = [
        at(&work, "/workspace", false),
        at(&tools, "/opt/tools", true),
        // A target inside another shows what the other shows there.
        at(&config, "/workspace/config.json", false),
        // One object may appear at several targets.
        at(&work, "/mirror", false),
    ];
    let request = ExportRequest::new(&mappings, ReservedTargets::CONTAINER_ROOT);
    let mapping = super::plan(&request).unwrap().unwrap();
    let roots: Vec<&Path> = mapping
        .children
        .iter()
        .map(|child| child.root.as_path())
        .collect();
    assert_eq!(roots, [canonical(&tools), canonical(&work)]);
    let binds: Vec<(&str, String, bool)> = mapping
        .binds
        .iter()
        .map(|bind| (bind.target.as_str(), bind.guest_source(), bind.read_only))
        .collect();
    assert_eq!(
        binds,
        [
            ("/mirror", "1".to_owned(), false),
            ("/workspace", "1".to_owned(), false),
            ("/opt/tools", "0".to_owned(), true),
            ("/workspace/config.json", "1/config.json".to_owned(), false),
        ]
    );
    mapping.check(ReservedTargets::CONTAINER_ROOT).unwrap();
    // A root that keeps its whole tree refuses every mapping, planned or recorded.
    let everything = ReservedTargets {
        trees: &["/"],
        exact: &[],
    };
    for error in [
        super::plan(&ExportRequest::new(&mappings, everything)).unwrap_err(),
        mapping.check(everything).unwrap_err(),
    ] {
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(
            error.message().ends_with("would cover the guest's /"),
            "{error}"
        );
    }
    // The convention's plans pass the check of their root too.
    let convention = plan(&FilesystemPolicy {
        readonly_paths: vec![tools.clone()],
        readwrite_paths: vec![work.clone(), config.clone()],
        ..FilesystemPolicy::default()
    })
    .unwrap()
    .unwrap();
    convention.check(ReservedTargets::BLOCKLESS_ROOT).unwrap();

    let refused = |mappings: &[Mapping<'_>], expected: &str| {
        let request = ExportRequest::new(mappings, ReservedTargets::CONTAINER_ROOT);
        let error = super::plan(&request).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(
            error.message().contains(expected),
            "{error} lacks {expected}"
        );
    };
    refused(
        &[at(&work, "/data", true), at(&tools, "/data", true)],
        "have the same guest target /data",
    );
    refused(
        &[at(&work, "/data", true), at(&tools, "/data/tools", true)],
        "at /data/tools would hide part of the mappings entry",
    );
    // A file shows nothing below it.
    refused(
        &[at(&config, "/data", true), at(&tools, "/data/tools", true)],
        "would hide part of",
    );
    let long = format!("/{}", "x".repeat(4095));
    for (target, problem) in [
        ("data", "is not absolute"),
        ("/", "is the guest's root"),
        ("/a//b", "has an empty, `.`, or `..` component"),
        ("/a/./b", "has an empty, `.`, or `..` component"),
        ("/a/../b", "has an empty, `.`, or `..` component"),
        ("/a/", "has an empty, `.`, or `..` component"),
        ("/a\0b", "contains a NUL character"),
        (long.as_str(), "is longer than a guest path may be"),
    ] {
        refused(&[at(&work, target, true)], problem);
    }
    refused(
        &[at(&work, "/etc", true)],
        "has the guest target /etc, which would cover the guest's /etc",
    );
    refused(
        &[at(&work, "/proc/self", true)],
        "would cover the guest's /proc",
    );
    refused(
        &[at(&config, "/etc/resolv.conf", true)],
        "has the guest target /etc/resolv.conf, which would cover the guest's /etc/resolv.conf",
    );
    // The same object read-only and read-write at two targets would let the guest write it.
    refused(
        &[at(&work, "/a", false), at(&work, "/b", true)],
        "names the object of the mappings entry",
    );
    // A target inside another must spell, name for name, the host path below the other's
    // object, so a backslash in a guest name, which a Windows host reads as a separator, and a
    // guest name that a Windows host reads as a root of its own show nothing there.
    let host = work.join("src").join("host");
    fs::create_dir_all(&host).unwrap();
    refused(
        &[
            at(&work, "/workspace", true),
            at(&host, r"/workspace/src\host", true),
        ],
        "would hide part of",
    );
    let resolved = canonical(&work);
    let text = resolved.to_str().unwrap();
    let rooted = text
        .split_once(':')
        .map_or(text, |(_, rest)| rest)
        .replace('/', r"\");
    let inside = format!("/opt/tools/{rooted}");
    refused(
        &[at(&tools, "/opt/tools", true), at(&work, &inside, true)],
        "would hide part of",
    );
}

#[test]
fn reserved_targets_depend_on_the_root() {
    let container = ReservedTargets::CONTAINER_ROOT;
    for (target, reserved) in [
        ("/", "/"),
        ("/proc", "/proc"),
        ("/proc/self", "/proc"),
        ("/sys/fs", "/sys"),
        ("/dev/shm", "/dev"),
        ("/.nvx-agent/tools", "/.nvx-agent"),
        ("/etc", "/etc"),
        ("/etc/machine-id", "/etc/machine-id"),
        ("/etc/resolv.conf", "/etc/resolv.conf"),
        ("/run", "/run"),
        // The ancestors of the runtime's trees are reserved, as exact paths here.
        ("/run/nvx", "/run/nvx"),
        ("/run/nvx/time/state", "/run/nvx/time"),
    ] {
        assert_eq!(container.reservation(target), Some(reserved), "{target}");
    }
    for target in [
        "/etc/app",
        "/etc/resolvconf",
        "/run/secrets",
        "/run/nvx/other",
        "/usr/local",
        "/workspace",
        "/procfs",
    ] {
        assert_eq!(container.reservation(target), None, "{target}");
    }
    let blockless = ReservedTargets::BLOCKLESS_ROOT;
    for (target, reserved) in [
        ("/", "/"),
        ("/usr/local", "/usr"),
        ("/run", "/run"),
        ("/etc/app", "/etc"),
        ("/init", "/init"),
    ] {
        assert_eq!(blockless.reservation(target), Some(reserved), "{target}");
    }
    for target in ["/workspace", "/mnt/c/work", "/home/me", "/initrd", "/opt"] {
        assert_eq!(blockless.reservation(target), None, "{target}");
    }
    // A mapping at an ancestor of a reserved tree would shadow it.
    let nested = ReservedTargets {
        trees: &["/srv/runtime/state"],
        exact: &[],
    };
    assert_eq!(nested.reservation("/srv"), Some("/srv/runtime/state"));
    assert_eq!(
        nested.reservation("/srv/runtime"),
        Some("/srv/runtime/state")
    );
    assert_eq!(nested.reservation("/srv/other"), None);
    // A root that keeps its whole tree takes no mapping.
    let everything = ReservedTargets {
        trees: &["/"],
        exact: &[],
    };
    for target in ["/", "/workspace", "/srv/runtime/state"] {
        assert_eq!(everything.reservation(target), Some("/"), "{target}");
    }
}

#[test]
fn policy_paths_hide_show_and_narrow_writes() {
    let directory = root();
    let base = directory.path();
    fs::create_dir_all(base.join("work/src/secret/public")).unwrap();
    fs::create_dir_all(base.join("work/out/cache")).unwrap();
    let work = base.join("work");
    let paths =
        |names: &[&str]| -> Vec<PathBuf> { names.iter().map(|name| work.join(name)).collect() };
    let request =
        |mappings: &[Mapping<'_>], denied: &[PathBuf], allowed: &[PathBuf], write: &[PathBuf]| {
            let request = ExportRequest {
                denied: PathList {
                    name: "denied",
                    paths: denied,
                },
                allowed: PathList {
                    name: "allowed",
                    paths: allowed,
                },
                write: PathList {
                    name: "write",
                    paths: write,
                },
                ..ExportRequest::new(mappings, ReservedTargets::CONTAINER_ROOT)
            };
            super::plan(&request)
        };
    let whole = [at(&work, "/workspace", false)];
    let (denied, allowed, write) = (
        paths(&["src/secret"]),
        paths(&["src/secret/public"]),
        paths(&["out"]),
    );
    let mapping = request(&whole, &denied, &allowed, &write).unwrap().unwrap();
    let child = &mapping.children[0];
    let resolved = canonical(&work);
    assert!(child.writable);
    assert_eq!(child.denied, [resolved.join("src").join("secret")]);
    assert_eq!(
        child.allowed,
        [resolved.join("src").join("secret").join("public")]
    );
    assert_eq!(child.write, [resolved.join("out")]);
    // Writes are narrowed to `out`, so nothing outside it can move.
    assert_eq!(child.denied_identities, [None]);
    let arguments = mapping.openvmm_arguments();
    let text = |path: PathBuf| vec![display_path(&path)];
    assert_eq!(
        option_values(&arguments, "--mount-deny"),
        text(resolved.join("src").join("secret"))
    );
    assert_eq!(
        option_values(&arguments, "--mount-allow"),
        text(resolved.join("src").join("secret").join("public"))
    );
    assert_eq!(
        option_values(&arguments, "--mount-write"),
        text(resolved.join("out"))
    );
    assert!(!arguments.contains(&"--mount-owner".to_owned()));
    mapping.check(ReservedTargets::CONTAINER_ROOT).unwrap();

    // A denied root shows only its allowed paths.
    let mapping = request(&whole, std::slice::from_ref(&work), &paths(&["src"]), &[])
        .unwrap()
        .unwrap();
    assert_eq!(mapping.children[0].denied, std::slice::from_ref(&resolved));
    assert_eq!(mapping.children[0].allowed, [resolved.join("src")]);
    let error = request(&whole, std::slice::from_ref(&work), &[], &[]).unwrap_err();
    assert!(
        error.message().contains("contains the mapped path"),
        "{error}"
    );

    // A read-only path may lie in the part of a narrowed directory that stays read-only, and
    // the directory may be mapped read-only elsewhere too.
    let src = work.join("src");
    let parts = [
        at(&work, "/workspace", false),
        at(&src, "/src", true),
        at(&work, "/view", true),
    ];
    request(&parts, &[], &[], &write).unwrap().unwrap();

    let refused = |mappings: &[Mapping<'_>],
                   denied: &[PathBuf],
                   allowed: &[PathBuf],
                   write: &[PathBuf],
                   expected: &str| {
        let error = request(mappings, denied, allowed, write).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(
            error.message().contains(expected),
            "{error} lacks {expected}"
        );
    };
    refused(
        &whole,
        &[],
        &paths(&["src"]),
        &[],
        "lies inside no denied entry",
    );
    refused(
        &whole,
        std::slice::from_ref(&work),
        std::slice::from_ref(&work),
        &[],
        "is an exported path itself",
    );
    refused(
        &whole,
        &paths(&["src"]),
        &paths(&["src/secret", "src/secret/public"]),
        &[],
        "which shows it already",
    );
    refused(
        &whole,
        &paths(&["src"]),
        &paths(&["src"]),
        &[],
        "is also a denied entry",
    );
    refused(
        &[at(&work, "/workspace", true)],
        &[],
        &[],
        &write,
        "lies in no directory that is mapped read-write as a whole",
    );
    refused(
        &whole,
        &[],
        &[],
        std::slice::from_ref(&work),
        "lies in no directory that is mapped read-write as a whole",
    );
    refused(
        &whole,
        &paths(&["src"]),
        &[],
        &paths(&["src/secret"]),
        "which hides it",
    );
    let cache = work.join("out/cache");
    refused(
        &[at(&work, "/workspace", false), at(&cache, "/cache", true)],
        &[],
        &[],
        &write,
        "lies inside the write entry",
    );
    refused(
        &[at(&work, "/workspace", false), at(&work, "/view", true)],
        &[],
        &[],
        &[],
        "names the object of the mappings entry",
    );
}

#[test]
fn narrowed_writes_pin_only_what_lies_in_writable_paths() {
    let directory = root();
    let base = directory.path();
    fs::create_dir_all(base.join("work/out/cache")).unwrap();
    fs::create_dir_all(base.join("work/out/private")).unwrap();
    let work = base.join("work");
    let (cache, src) = (work.join("out/cache"), work.join("src"));
    let mappings = [
        at(&work, "/workspace", false),
        at(&cache, "/cache", false),
        at(&src, "/src", true),
    ];
    let denied = [work.join("out/private"), work.join("src/secret")];
    let write = [work.join("out")];
    let request = ExportRequest {
        denied: PathList {
            name: "denied",
            paths: &denied,
        },
        write: PathList {
            name: "write",
            paths: &write,
        },
        ..ExportRequest::new(&mappings, ReservedTargets::CONTAINER_ROOT)
    };
    let mapping = super::plan(&request).unwrap().unwrap();
    let child = &mapping.children[0];
    assert_eq!(child.write, [canonical(&work).join("out")]);
    let pinned: Vec<bool> = child
        .denied_identities
        .iter()
        .map(Option::is_some)
        .collect();
    assert_eq!(pinned, [true, false]);
    let pinned: Vec<(&str, bool)> = mapping
        .binds
        .iter()
        .map(|bind| (bind.target.as_str(), bind.identity.is_some()))
        .collect();
    assert_eq!(
        pinned,
        [("/cache", true), ("/src", false), ("/workspace", false)]
    );
    // A record keeps every pin of the plan, and narrows writes only to paths that keep its
    // read-write mappings writable.
    mapping.check(ReservedTargets::CONTAINER_ROOT).unwrap();
    let refused = |change: fn(&mut ExportPlan), expected: &str| {
        let mut changed = mapping.clone();
        change(&mut changed);
        let error = changed.check(ReservedTargets::CONTAINER_ROOT).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(error.message().contains(expected), "{error}");
    };
    refused(
        |changed| changed.children[0].denied_identities[0] = None,
        "does not pin the denied path",
    );
    refused(
        |changed| changed.binds[0].identity = None,
        "does not pin the object that it maps at /cache",
    );
    refused(
        |changed| changed.children[0].write = vec![changed.children[0].root.join("other")],
        "which it maps read-write, stays read-only",
    );
    // Without write paths, the guest could move `src/secret` too.
    refused(
        |changed| changed.children[0].write.clear(),
        "does not pin the denied path",
    );
    mapping.verify().unwrap();
    fs::rename(&cache, work.join("out/moved")).unwrap();
    fs::create_dir(&cache).unwrap();
    assert_eq!(mapping.verify().unwrap_err().kind(), ErrorKind::Other);
}

#[test]
fn the_caller_owner_reaches_openvmm_on_linux_hosts() {
    let directory = root();
    let work = directory.path().join("work");
    let mappings = [at(&work, "/workspace", false)];
    let request = ExportRequest {
        owner: Owner::Caller,
        ..ExportRequest::new(&mappings, ReservedTargets::CONTAINER_ROOT)
    };
    if cfg!(windows) {
        let error = super::plan(&request).unwrap_err();
        assert!(error.message().contains("requires a Linux host"), "{error}");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let owned = fs::metadata(&work).unwrap();
        if owned.uid() != 0 && owned.gid() != 0 {
            let mapping = super::plan(&request).unwrap().unwrap();
            assert_eq!(mapping.owner, Owner::Caller);
            let arguments = mapping.openvmm_arguments();
            assert_eq!(
                arguments[arguments.len() - 2..],
                ["--mount-owner", "caller"]
            );
            mapping.verify().unwrap();
            let record = serde_json::to_value(&mapping).unwrap();
            assert_eq!(record["owner"], "caller");
            assert_eq!(
                serde_json::from_value::<ExportPlan>(record).unwrap(),
                mapping
            );
        }
        // Guest root is squashed to the owner, which must not be root.
        let system = Path::new("/usr");
        if fs::metadata(system).is_ok_and(|metadata| metadata.uid() == 0) {
            let mappings = [at(system, "/srv/system", true)];
            let request = ExportRequest {
                owner: Owner::Caller,
                ..ExportRequest::new(&mappings, ReservedTargets::CONTAINER_ROOT)
            };
            let error = super::plan(&request).unwrap_err();
            assert!(
                error
                    .message()
                    .contains("must not be owned by UID 0 or GID 0"),
                "{error}"
            );
        }
    }
    // The default owner is left out of the record.
    let vmm = super::plan(&ExportRequest::new(
        &mappings,
        ReservedTargets::CONTAINER_ROOT,
    ))
    .unwrap()
    .unwrap();
    assert!(serde_json::to_value(&vmm).unwrap().get("owner").is_none());
}

#[test]
fn read_write_mappings_to_which_writes_are_narrowed_stay_visible() {
    let directory = root();
    let base = directory.path();
    fs::create_dir_all(base.join("tree/private/public")).unwrap();
    fs::create_dir_all(base.join("tree/out")).unwrap();
    let (tree, private) = (base.join("tree"), base.join("tree/private"));
    let denied = [private.clone()];
    let allowed = [private.join("public")];
    let request = |mappings: &[Mapping<'_>], write: &[PathBuf]| {
        super::plan(&ExportRequest {
            denied: PathList {
                name: "denied",
                paths: &denied,
            },
            allowed: PathList {
                name: "allowed",
                paths: &allowed,
            },
            write: PathList {
                name: "write",
                paths: write,
            },
            ..ExportRequest::new(mappings, ReservedTargets::CONTAINER_ROOT)
        })
    };
    // OpenVMM narrows the writes of the tree to the read-write private, which it cannot hide:
    // in a read-only tree, and in a read-write tree whose writes are narrowed.
    let out = [tree.join("out")];
    for (mappings, write) in [
        (
            [at(&tree, "/tree", true), at(&private, "/private", false)],
            &[][..],
        ),
        (
            [at(&tree, "/tree", false), at(&private, "/private", false)],
            &out[..],
        ),
    ] {
        let error = request(&mappings, write).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(
            error
                .message()
                .contains("OpenVMM cannot make a hidden path writable"),
            "{error}"
        );
    }
    // In a tree that is writable as a whole, the denied private shows its allowed paths.
    let mapping = request(
        &[at(&tree, "/tree", false), at(&private, "/private", false)],
        &[],
    )
    .unwrap()
    .unwrap();
    let child = &mapping.children[0];
    assert!(child.writable && child.write.is_empty());
    assert_eq!(child.denied, [canonical(&private)]);
    assert_eq!(child.allowed, [canonical(&private).join("public")]);
}

#[test]
fn denied_mapped_directories_inside_denied_ones_show_their_allowed_paths() {
    let directory = root();
    let base = directory.path();
    fs::create_dir_all(base.join("tree/private/public")).unwrap();
    fs::create_dir_all(base.join("tree/other")).unwrap();
    let (tree, private) = (base.join("tree"), base.join("tree/private"));
    let denied = [tree.clone(), private.clone()];
    let request = |mappings: &[Mapping<'_>], allowed: &[PathBuf]| {
        super::plan(&ExportRequest {
            denied: PathList {
                name: "denied",
                paths: &denied,
            },
            allowed: PathList {
                name: "allowed",
                paths: allowed,
            },
            ..ExportRequest::new(mappings, ReservedTargets::CONTAINER_ROOT)
        })
    };
    let both = [at(&tree, "/tree", false), at(&private, "/private", false)];
    // Each mapped directory is denied itself and shows the allowed path inside it; OpenVMM
    // needs only the outer denial, which hides the inner one already.
    let mapping = request(&both, &[private.join("public")]).unwrap().unwrap();
    let child = &mapping.children[0];
    assert_eq!(child.denied, [canonical(&tree)]);
    assert_eq!(child.allowed, [canonical(&private).join("public")]);
    mapping.check(ReservedTargets::CONTAINER_ROOT).unwrap();
    // OpenVMM receives each exported directory's policy alone, so one that a denied directory
    // outside it contains keeps its own denial.
    let mapping = request(
        &[at(&private, "/private", false)],
        &[private.join("public")],
    )
    .unwrap()
    .unwrap();
    let child = &mapping.children[0];
    assert_eq!(child.root, canonical(&private));
    assert_eq!(child.denied, [canonical(&private)]);
    assert_eq!(child.allowed, [canonical(&private).join("public")]);
    mapping.check(ReservedTargets::CONTAINER_ROOT).unwrap();
    // A mapped directory whose denial shows nothing again inside it stays hidden.
    let error = request(&both, &[tree.join("other")]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    assert!(
        error.message().contains("contains the mapped path"),
        "{error}"
    );
}

#[test]
fn recorded_plans_hold_no_more_policy_than_one_aggregate_accepts() {
    // Each child's paths of each kind stay within OpenVMM's limits for one directory.
    let child = |index: usize| {
        let root = std::env::temp_dir().join(format!("nvx-policy-{index}"));
        let long = |kind: &str, path: usize| format!("{kind}-{path}-{}", "x".repeat(3990));
        let paths = |kind: &str| -> Vec<PathBuf> {
            (0..4).map(|path| root.join(long(kind, path))).collect()
        };
        // OpenVMM shows paths again only inside a path that it hides.
        let hidden = root.join(long("denied", 0));
        ExportChild {
            root: root.clone(),
            file: false,
            writable: true,
            denied: paths("denied"),
            denied_identities: vec![None; 4],
            allowed: (0..4)
                .map(|path| hidden.join(format!("allowed-{path}")))
                .collect(),
            write: paths("write"),
        }
    };
    let recorded = |count: usize| ExportPlan {
        children: (0..count).map(child).collect(),
        binds: (0..count)
            .map(|index| Bind {
                child: index,
                source: String::new(),
                target: format!("/c{index}"),
                read_only: false,
                identity: None,
            })
            .collect(),
        owner: Owner::Vmm,
    };
    recorded(2).check(ReservedTargets::CONTAINER_ROOT).unwrap();
    let error = recorded(3)
        .check(ReservedTargets::CONTAINER_ROOT)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    assert!(
        error.message().contains("exceed its 131072-byte limit"),
        "{error}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn mapped_paths_resolve_to_utf8_paths() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let directory = root();
    let base = directory.path();
    let share = base.join(OsStr::from_bytes(b"share-\xff"));
    fs::create_dir(&share).unwrap();
    // A link of a valid name resolves to the same path, which OpenVMM's options could not
    // name.
    let link = base.join("link");
    std::os::unix::fs::symlink(&share, &link).unwrap();
    for path in [&share, &link] {
        let mappings = [at(path, "/workspace", false)];
        let error = super::plan(&ExportRequest::new(
            &mappings,
            ReservedTargets::CONTAINER_ROOT,
        ))
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(
            error
                .message()
                .contains("resolves to a path that is not valid UTF-8"),
            "{error}"
        );
    }
}
