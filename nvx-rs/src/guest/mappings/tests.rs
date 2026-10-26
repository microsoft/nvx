//! Tests of the mapper with the root operations' fake kernel, which simulates mounts by their
//! unique IDs, in a temporary directory: target roots of whole file systems, the walk's refusals,
//! creation on the root's own mounts only, with exact modes whatever the umask or a default ACL,
//! and the roots of those that a mapping may cover, the checks of overlaps and reserved targets,
//! the order of an attach, latching, zero mappings, rename races, and the teardown's order and
//! failures.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;
use crate::guest::root::{Call, FakeKernel, Operation, attach_calls};

/// A target root at `root` and a source root at `export`, below a temporary directory, each on a
/// mount of the fake kernel's.
struct Fixture {
    /// The temporary directory, which the fixture holds only so that dropping it removes it.
    _directory: tempfile::TempDir,
    /// The temporary directory's canonical path.
    base: PathBuf,
    /// The fake kernel.
    kernel: FakeKernel,
}

impl Fixture {
    /// Returns a fixture whose export holds the directory `dir`, with the directory `inner`, the
    /// regular file `file.txt`, the FIFO `fifo`, and the symbolic link `link` to `dir`, and whose
    /// root is empty.
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        for path in ["root", "export/dir/inner", "elsewhere"] {
            fs::create_dir_all(base.join(path)).unwrap();
        }
        fs::write(base.join("export/file.txt"), b"file").unwrap();
        std::os::unix::fs::symlink("dir", base.join("export/link")).unwrap();
        let fifo = sys::path_c_string(&base.join("export/fifo")).unwrap();
        sys::create_fifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();
        let kernel = FakeKernel::default();
        kernel.add_mount(base.join("root"));
        kernel.add_mount(base.join("export"));
        Self {
            _directory: directory,
            base,
            kernel,
        }
    }

    /// Returns `relative`'s path below the temporary directory.
    fn path(&self, relative: &str) -> PathBuf {
        self.base.join(relative)
    }

    /// Returns a clone of the fake kernel, which shares what it records.
    fn kernel(&self) -> FakeKernel {
        self.kernel.clone()
    }

    /// Returns the target root at `root`.
    fn root(&self) -> TargetRoot {
        TargetRoot::open_with(&mut self.kernel(), &self.path("root")).unwrap()
    }

    /// Returns a mapper beneath the root, with `reserved`.
    fn mapper(&self, reserved: Reserved) -> Mapper {
        Mapper::new(self.root(), reserved)
    }

    /// Returns the source root at `export`.
    fn source(&self) -> SourceRoot {
        SourceRoot::open(&self.path("export")).unwrap()
    }

    /// Applies `mappings` with `mapper`, from the export.
    fn apply(&self, mapper: &mut Mapper, mappings: &[Mapping]) -> Result<(), ApplyError> {
        mapper.apply_with(&mut self.kernel(), &self.source(), mappings)
    }

    /// Applies the one mapping of `source` at `target` with a new mapper with `reserved`, and
    /// returns its error.
    fn refusal(&self, reserved: Reserved, mapping: Mapping) -> MappingError {
        let mut mapper = self.mapper(reserved);
        let error = self.apply(&mut mapper, &[mapping]).unwrap_err();
        assert_eq!(error.entry(), 0);
        assert!(mapper.is_latched());
        error.error().clone()
    }
}

/// Returns the mapping of `source` at `target` with `access`.
fn mapping(source: &str, target: &str, access: Access) -> Mapping {
    Mapping::new(source, target, access).unwrap()
}

/// Returns the permission bits of `path`.
fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Returns the failure of a system call at `step` with `error`.
fn failed(step: Step, error: Errno) -> MappingError {
    MappingError::System {
        step,
        error: error as i32,
    }
}

#[test]
fn mappings_are_restricted_copies_attached_in_order() {
    let fixture = Fixture::new();
    let mut mapper = fixture.mapper(Reserved::default());
    let mappings = [
        mapping("dir", "/data/ro", Access::ReadOnly),
        mapping("dir/inner", "/data/rw", Access::ReadWrite),
        mapping("file.txt", "/conf/app.conf", Access::ReadOnly),
    ];
    fixture.apply(&mut mapper, &mappings).unwrap();
    // Each mount is copied alone, detached, and restricted before anything can see it.
    let expected: Vec<Call> = [
        ("dir", "data/ro", Access::ReadOnly),
        ("dir/inner", "data/rw", Access::ReadWrite),
        ("file.txt", "conf/app.conf", Access::ReadOnly),
    ]
    .into_iter()
    .flat_map(|(source, target, access)| {
        attach_calls(
            &fixture.path(&format!("export/{source}")),
            &fixture.path(&format!("root/{target}")),
            access,
        )
    })
    .collect();
    assert_eq!(fixture.kernel.calls(), expected);
    // What the targets lacked was created, with its mode, whatever the umask.
    for (path, kind_mode, directory) in [
        ("root/data", 0o755, true),
        ("root/data/ro", 0o755, true),
        ("root/data/rw", 0o755, true),
        ("root/conf", 0o755, true),
        ("root/conf/app.conf", 0o644, false),
    ] {
        let path = fixture.path(path);
        assert_eq!(path.is_dir(), directory, "{}", path.display());
        assert_eq!(mode(&path), kind_mode, "{}", path.display());
    }
    assert_eq!(
        mapper.targets().collect::<Vec<_>>(),
        ["data/ro", "data/rw", "conf/app.conf"].map(Path::new)
    );
    for target in ["root/data/ro", "root/data/rw", "root/conf/app.conf"] {
        assert!(fixture.kernel.mount_at(&fixture.path(target)).is_some());
    }
    assert!(!mapper.is_latched());
}

#[test]
fn targets_are_created_with_their_modes_whatever_the_umask() {
    // A caller's restrictive umask neither narrows what a mapper creates, which a workload that
    // is not root must be able to enter, nor changes.
    sys::with_thread_umask(0o077, || {
        let fixture = Fixture::new();
        let kept = fixture.path("root/kept");
        fs::create_dir(&kept).unwrap();
        fs::set_permissions(&kept, fs::Permissions::from_mode(0o700)).unwrap();
        let mut mapper = fixture.mapper(Reserved::default());
        let mappings = [
            mapping("dir", "/srv/new/share", Access::ReadOnly),
            mapping("file.txt", "/srv/new/app.conf", Access::ReadOnly),
            mapping("dir/inner", "/kept/new", Access::ReadOnly),
        ];
        fixture.apply(&mut mapper, &mappings).unwrap();
        for (path, expected) in [
            ("root/srv", 0o755),
            ("root/srv/new", 0o755),
            ("root/srv/new/share", 0o755),
            ("root/srv/new/app.conf", 0o644),
            ("root/kept/new", 0o755),
            // A directory that was there keeps its mode.
            ("root/kept", 0o700),
        ] {
            assert_eq!(mode(&fixture.path(path)), expected, "{path}");
        }
        assert_eq!(sys::thread_umask(), 0o077);
    });
}

#[test]
fn targets_are_created_with_their_modes_whatever_a_default_acl() {
    // A default ACL of a target's parent withholds the group's and others' permissions from
    // whatever is created in it, whatever the umask, and new directories inherit it, but what a
    // mapper creates still gets its mode.
    let fixture = Fixture::new();
    let restricted = fixture.path("root/restricted");
    fs::create_dir(&restricted).unwrap();
    if !sys::restrict_new_entries(&restricted) {
        return;
    }
    let mut mapper = fixture.mapper(Reserved::default());
    let mappings = [
        mapping("dir", "/restricted/new/share", Access::ReadOnly),
        mapping("file.txt", "/restricted/new/app.conf", Access::ReadOnly),
    ];
    fixture.apply(&mut mapper, &mappings).unwrap();
    for (path, expected) in [
        ("root/restricted/new", 0o755),
        ("root/restricted/new/share", 0o755),
        ("root/restricted/new/app.conf", 0o644),
    ] {
        assert_eq!(mode(&fixture.path(path)), expected, "{path}");
    }
}

#[test]
fn zero_mappings_do_no_work() {
    let fixture = Fixture::new();
    let mut mapper = fixture.mapper(Reserved::default());
    fixture.apply(&mut mapper, &[]).unwrap();
    // Not even on a latched mapper.
    fixture
        .apply(&mut mapper, &[mapping("missing", "/a", Access::ReadOnly)])
        .unwrap_err();
    fixture.apply(&mut mapper, &[]).unwrap();
    assert!(fixture.kernel.calls().is_empty());
    assert_eq!(fs::read_dir(fixture.path("root")).unwrap().count(), 0);
    assert_eq!(mapper.targets().len(), 0);
}

#[test]
fn links_at_any_component_are_refused() {
    let fixture = Fixture::new();
    let root = fixture.path("root");
    fs::create_dir_all(root.join("a/b/c")).unwrap();
    std::os::unix::fs::symlink("a", root.join("l1")).unwrap();
    std::os::unix::fs::symlink("b", root.join("a/l2")).unwrap();
    std::os::unix::fs::symlink("c", root.join("a/b/l3")).unwrap();
    std::os::unix::fs::symlink("missing", root.join("a/b/dangling")).unwrap();
    std::os::unix::fs::symlink(fixture.path("elsewhere"), root.join("absolute")).unwrap();
    std::os::unix::fs::symlink("..", root.join("up")).unwrap();
    for (target, link) in [
        ("/l1", "/l1"),
        ("/l1/b/c", "/l1"),
        ("/a/l2/c", "/a/l2"),
        ("/a/b/l3", "/a/b/l3"),
        ("/a/b/l3/new", "/a/b/l3"),
        ("/a/b/dangling", "/a/b/dangling"),
        ("/absolute/new", "/absolute"),
        ("/up/new", "/up"),
    ] {
        let error = fixture.refusal(
            Reserved::default(),
            mapping("dir", target, Access::ReadWrite),
        );
        assert_eq!(error, MappingError::Link { path: link.into() }, "{target}");
        assert_eq!(error.raw_os_error(), Errno::ELOOP as i32);
    }
    // Nothing was created through the links, and nothing was copied or attached.
    assert!(!fixture.path("elsewhere/new").exists());
    assert!(!fixture.path("root/a/b/c/new").exists());
    assert!(!fixture.path("new").exists());
    assert!(fixture.kernel.calls().is_empty());

    // A magic link, through procfs, from the system's root.
    let root = TargetRoot::open_with(&mut fixture.kernel(), Path::new("/")).unwrap();
    let mut mapper = Mapper::new(root, Reserved::default());
    let process = std::process::id();
    let error = mapper
        .apply_with(
            &mut fixture.kernel(),
            &fixture.source(),
            &[mapping(
                "dir",
                &format!("/proc/{process}/cwd/new"),
                Access::ReadOnly,
            )],
        )
        .unwrap_err();
    assert_eq!(
        error.error(),
        &MappingError::Link {
            path: format!("/proc/{process}/cwd").into()
        }
    );
}

#[test]
fn a_component_that_is_not_a_directory_is_refused() {
    let fixture = Fixture::new();
    fs::write(fixture.path("root/plain"), b"").unwrap();
    let error = fixture.refusal(
        Reserved::default(),
        mapping("dir", "/plain/x", Access::ReadOnly),
    );
    assert_eq!(
        error,
        MappingError::NotDirectory {
            path: "/plain".into()
        }
    );
    assert_eq!(error.raw_os_error(), Errno::ENOTDIR as i32);
}

#[test]
fn sources_and_targets_must_be_of_one_kind() {
    let fixture = Fixture::new();
    fs::write(fixture.path("root/plain"), b"").unwrap();
    fs::create_dir(fixture.path("root/directory")).unwrap();
    let refusal = |mapping: Mapping| fixture.refusal(Reserved::default(), mapping);
    let read_only = Access::ReadOnly;
    assert_eq!(
        refusal(mapping("dir", "/plain", read_only)),
        MappingError::TargetKind
    );
    assert_eq!(
        refusal(mapping("file.txt", "/directory", read_only)),
        MappingError::TargetKind
    );
    assert_eq!(
        refusal(mapping("fifo", "/fifo", read_only)),
        MappingError::SourceKind
    );
    let file = mapping("dir", "/file", read_only).with_kind(Kind::File);
    assert_eq!(refusal(file), MappingError::SourceKind);
    let directory = mapping("file.txt", "/directory", read_only).with_kind(Kind::Directory);
    assert_eq!(refusal(directory), MappingError::SourceKind);
    // A source is opened through no link, and on its root's mount.
    assert_eq!(
        refusal(mapping("link", "/link", read_only)),
        failed(Step::Source, Errno::ELOOP)
    );
    assert_eq!(
        refusal(mapping("missing", "/missing", read_only)),
        failed(Step::Source, Errno::ENOENT)
    );
    fixture.kernel.add_mount(fixture.path("export/dir"));
    for source in ["dir", "dir/inner"] {
        assert_eq!(
            refusal(mapping(source, "/crossed", read_only)),
            failed(Step::Source, Errno::EXDEV),
            "{source}"
        );
    }
    assert!(fixture.kernel.calls().is_empty());
    for target in ["fifo", "file", "link", "missing", "crossed"] {
        assert!(!fixture.path("root").join(target).exists(), "{target}");
    }
}

#[test]
fn targets_are_created_only_on_the_root_s_own_mounts() {
    // Not on another mount below the root, such as a file system that the system mounted there,
    // or a bind of the root's own file system, which has a mount ID of its own.
    let fixture = Fixture::new();
    let foreign = fixture.path("root/foreign");
    fs::create_dir_all(foreign.join("existing")).unwrap();
    fixture.kernel.add_mount(&foreign);
    let read_write = Access::ReadWrite;
    for (target, path) in [
        ("/foreign/new", "/foreign/new"),
        ("/foreign/new/deeper", "/foreign/new"),
        ("/foreign/existing", "/foreign/existing"),
    ] {
        let error = fixture.refusal(Reserved::default(), mapping("dir", target, read_write));
        assert_eq!(
            error,
            MappingError::Foreign { path: path.into() },
            "{target}"
        );
        assert_eq!(error.raw_os_error(), Errno::EXDEV as i32);
    }
    assert!(!foreign.join("new").exists());
    // Unless the root owns that mount too.
    let mut root = fixture.root();
    root.add_mount_with(&mut fixture.kernel(), &foreign)
        .unwrap();
    let mut mapper = Mapper::new(root, Reserved::default());
    fixture
        .apply(&mut mapper, &[mapping("dir", "/foreign/new", read_write)])
        .unwrap();
    assert!(foreign.join("new").is_dir());

    // Nor inside an earlier mapping, though a mapping may go onto an object that one shows.
    let fixture = Fixture::new();
    let mut mapper = fixture.mapper(Reserved::default());
    // The fake kernel mounts nothing, so the root holds what the mapping of `dir` shows.
    fs::create_dir_all(fixture.path("root/m/inner")).unwrap();
    fixture
        .apply(
            &mut mapper,
            &[
                mapping("dir", "/m", read_write),
                mapping("dir/inner", "/m/inner", Access::ReadOnly),
            ],
        )
        .unwrap();
    let outer = fixture.kernel.mount_at(&fixture.path("root/m")).unwrap();
    let inner = fixture
        .kernel
        .mount_at(&fixture.path("root/m/inner"))
        .unwrap();
    assert_ne!(outer, inner);
    let error = fixture
        .apply(&mut mapper, &[mapping("file.txt", "/m/new", read_write)])
        .unwrap_err();
    assert_eq!(
        error.error(),
        &MappingError::Foreign {
            path: "/m/new".into()
        }
    );
    assert!(!fixture.path("root/m/new").exists());
}

#[test]
fn targets_that_would_shadow_a_mapping_or_a_mount_are_refused() {
    let fixture = Fixture::new();
    let mut mapper = fixture.mapper(Reserved::default());
    fixture
        .apply(&mut mapper, &[mapping("dir", "/data/x", Access::ReadOnly)])
        .unwrap();
    // The checks alone neither open anything nor latch.
    let check = |target: &str| mapper.check(&mapping("dir", target, Access::ReadOnly));
    let overlap = MappingError::Overlap {
        mapped: "/data/x".into(),
    };
    assert_eq!(check("/data/x"), Err(overlap.clone()));
    assert_eq!(check("/data"), Err(overlap.clone()));
    assert_eq!(check("/data/x/y"), Ok(()));
    assert_eq!(check("/data/xy"), Ok(()));
    assert!(!mapper.is_latched());
    let error = fixture
        .apply(&mut mapper, &[mapping("dir", "/data", Access::ReadOnly)])
        .unwrap_err();
    assert_eq!(error.error(), &overlap);
    assert_eq!(error.error().raw_os_error(), Errno::EBUSY as i32);
    assert!(mapper.is_latched());

    // The root of a mount that the root does not own.
    fs::create_dir(fixture.path("root/mounted")).unwrap();
    fixture.kernel.add_mount(fixture.path("root/mounted"));
    let error = fixture.refusal(
        Reserved::default(),
        mapping("dir", "/mounted", Access::ReadOnly),
    );
    assert_eq!(error, MappingError::MountPoint);
    assert_eq!(error.raw_os_error(), Errno::EBUSY as i32);
}

#[test]
fn a_mapping_covers_the_root_of_a_mount_that_the_root_owns() {
    // Such as the tmpfs at a guest's `/tmp`, which a mapping of the host's `/tmp` covers.
    let fixture = Fixture::new();
    let tmp = fixture.path("root/tmp");
    fs::create_dir(&tmp).unwrap();
    let tmpfs = fixture.kernel.add_mount(&tmp);
    let mut root = fixture.root();
    root.add_mount_with(&mut fixture.kernel(), &tmp).unwrap();
    let mut mapper = Mapper::new(root, Reserved::default());
    fixture
        .apply(&mut mapper, &[mapping("dir", "/tmp", Access::ReadWrite)])
        .unwrap();
    assert_eq!(
        fixture.kernel.calls(),
        attach_calls(&fixture.path("export/dir"), &tmp, Access::ReadWrite)
    );
    let covering = fixture.kernel.mount_at(&tmp).unwrap();
    assert_ne!(covering, tmpfs);
    // Nothing is created inside the mapping, though the root owns the mount below it.
    let error = fixture
        .apply(
            &mut mapper,
            &[mapping("file.txt", "/tmp/new", Access::ReadOnly)],
        )
        .unwrap_err();
    assert_eq!(
        error.error(),
        &MappingError::Foreign {
            path: "/tmp/new".into()
        }
    );
    assert!(!tmp.join("new").exists());
    // The teardown uncovers it.
    mapper.teardown_with(&mut fixture.kernel()).unwrap();
    assert_eq!(fixture.kernel.mount_at(&tmp), Some(tmpfs));
}

#[test]
fn reserved_targets_and_their_ancestors_are_refused() {
    let fixture = Fixture::new();
    let check = |reserved: Reserved, target: &str| {
        Mapper::new(fixture.root(), reserved).check(&mapping("dir", target, Access::ReadOnly))
    };
    // Every mapper reserves the runtime's time tree, and the ancestors that would shadow it.
    for target in ["/run", "/run/nvx", "/run/nvx/time", "/run/nvx/time/status"] {
        assert_eq!(
            check(Reserved::default(), target),
            Err(MappingError::Reserved),
            "{target}"
        );
    }
    for target in ["/run/other", "/runs", "/run/nvx/timer", "/run/nvx/state"] {
        assert_eq!(check(Reserved::default(), target), Ok(()), "{target}");
    }
    // NVX's container root reserves its runtime's trees, and the paths above them.
    for target in [
        "/proc",
        "/proc/self",
        "/sys/fs/cgroup",
        "/dev",
        "/dev/shm",
        "/.nvx-agent/tool",
        "/etc",
        "/etc/machine-id",
        "/etc/resolv.conf",
        "/run",
        "/run/nvx",
        "/run/nvx/time/x",
    ] {
        assert_eq!(
            check(Reserved::container_root(), target),
            Err(MappingError::Reserved),
            "{target}"
        );
    }
    for target in [
        "/etc/app",
        "/etc/machine-ids",
        "/etc/resolvconf",
        "/procs",
        "/run/app",
        "/workspace",
    ] {
        assert_eq!(
            check(Reserved::container_root(), target),
            Ok(()),
            "{target}"
        );
    }
    // A caller's profile.
    let profile = || Reserved::new(["/srv/private"], ["/srv"]).unwrap();
    for target in ["/srv", "/srv/private", "/srv/private/x", "/run/nvx"] {
        assert_eq!(
            check(profile(), target),
            Err(MappingError::Reserved),
            "{target}"
        );
    }
    assert_eq!(check(profile(), "/srv/public"), Ok(()));
    for (trees, paths) in [
        (["relative"], ["/a"]),
        (["/a"], ["/a/../b"]),
        (["/"], ["/a"]),
    ] {
        assert_eq!(
            Reserved::new(trees, paths),
            Err(InvalidMapping::Target),
            "{trees:?} {paths:?}"
        );
    }
    // A refused target is not created.
    let error = fixture.refusal(
        Reserved::default(),
        mapping("dir", "/run/nvx/time", Access::ReadOnly),
    );
    assert_eq!(error, MappingError::Reserved);
    assert_eq!(error.raw_os_error(), Errno::EPERM as i32);
    assert!(!fixture.path("root/run").exists());
    assert!(fixture.kernel.calls().is_empty());
}

#[test]
fn the_first_failure_latches_the_mapper() {
    let fixture = Fixture::new();
    let mut mapper = fixture.mapper(Reserved::default());
    let error = fixture
        .apply(
            &mut mapper,
            &[
                mapping("dir", "/one", Access::ReadOnly),
                mapping("missing", "/two", Access::ReadOnly),
                mapping("file.txt", "/three", Access::ReadOnly),
            ],
        )
        .unwrap_err();
    assert_eq!(error.entry(), 1);
    assert_eq!(error.error(), &failed(Step::Source, Errno::ENOENT));
    assert_eq!(
        error.to_string(),
        format!(
            "mapping 1 failed: failed to open the source: {}",
            io::Error::from_raw_os_error(Errno::ENOENT as i32)
        )
    );
    // The mapping before it stays; the one after it is not attempted.
    assert!(fixture.kernel.mount_at(&fixture.path("root/one")).is_some());
    assert!(!fixture.path("root/two").exists());
    assert!(!fixture.path("root/three").exists());
    assert!(mapper.is_latched());
    let calls = fixture.kernel.calls();
    let error = fixture
        .apply(
            &mut mapper,
            &[mapping("file.txt", "/three", Access::ReadOnly)],
        )
        .unwrap_err();
    assert_eq!((error.entry(), error.error()), (0, &MappingError::Latched));
    assert_eq!(error.error().raw_os_error(), Errno::ECANCELED as i32);
    assert_eq!(fixture.kernel.calls(), calls);
    assert_eq!(mapper.targets().collect::<Vec<_>>(), [Path::new("one")]);
}

#[test]
fn a_failure_at_any_step_leaves_nothing_mounted() {
    let directory = fs::create_dir;
    for (operation, path, step, error) in [
        (Operation::Open, "export/dir", Step::Source, Errno::EACCES),
        (Operation::Open, "root/a", Step::Walk, Errno::EACCES),
        (Operation::Create, "root/new", Step::Create, Errno::ENOSPC),
        (
            Operation::ChangeMode,
            "root/new",
            Step::Create,
            Errno::EPERM,
        ),
        (Operation::Status, "root/a", Step::Status, Errno::EIO),
        (Operation::OpenTree, "export/dir", Step::Clone, Errno::EPERM),
        (
            Operation::SetAttributes,
            "export/dir",
            Step::Restrict,
            Errno::EPERM,
        ),
        (Operation::MoveMount, "root/a", Step::Attach, Errno::EBUSY),
    ] {
        let fixture = Fixture::new();
        directory(fixture.path("root/a")).unwrap();
        fixture.kernel.fail(operation, fixture.path(path), error);
        let target = if step == Step::Create { "/new" } else { "/a" };
        let refused = fixture.refusal(
            Reserved::default(),
            mapping("dir", target, Access::ReadOnly),
        );
        assert_eq!(refused, failed(step, error), "{operation:?}");
        assert_eq!(refused.raw_os_error(), error as i32);
        let mounted = fixture
            .kernel
            .mount_at(&fixture.path(&format!("root{target}")));
        assert_eq!(mounted, None, "{operation:?}");
        // Nothing is attached without its restrictions.
        let calls = fixture.kernel.calls();
        let restricted = calls
            .iter()
            .position(|call| matches!(call, Call::SetAttributes { .. }));
        let attached = calls
            .iter()
            .position(|call| matches!(call, Call::MoveMount { .. }));
        assert!(attached.is_none() || restricted < attached, "{operation:?}");
        assert_eq!(
            attached.is_some(),
            operation == Operation::MoveMount,
            "{operation:?}"
        );
        if operation == Operation::ChangeMode {
            // The directory that the mapper created has no permissions still, so that no
            // workload that is not root enters it; the temporary directory's removal needs some.
            assert_eq!(mode(&fixture.path("root/new")), 0o000);
            let permissions = fs::Permissions::from_mode(0o700);
            fs::set_permissions(fixture.path("root/new"), permissions).unwrap();
        }
    }
}

#[test]
fn a_mapping_that_a_rename_moved_is_detached_and_refused() {
    for (replaced, detach) in [(false, Ok(())), (true, Ok(())), (false, Err(Errno::EBUSY))] {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.path("root/data/x")).unwrap();
        let base = fixture.base.clone();
        // A rename between the walk and the attach moves the target's directory.
        fixture.kernel.on_move(move |_| {
            fs::rename(base.join("root/data"), base.join("root/moved")).unwrap();
            if replaced {
                fs::create_dir_all(base.join("root/data/x")).unwrap();
            }
        });
        let moved = fixture.path("root/moved/x");
        if let Err(error) = detach {
            fixture.kernel.fail(Operation::Detach, &moved, error);
        }
        let mut mapper = fixture.mapper(Reserved::default());
        let error = fixture
            .apply(&mut mapper, &[mapping("dir", "/data/x", Access::ReadOnly)])
            .unwrap_err();
        assert_eq!(error.entry(), 0);
        assert!(mapper.is_latched());
        assert_eq!(
            error.error(),
            &MappingError::Moved {
                mounted: detach.is_err()
            }
        );
        assert_eq!(error.error().raw_os_error(), Errno::EAGAIN as i32);
        let calls = fixture.kernel.calls();
        assert_eq!(
            calls[calls.len() - 2..],
            [
                Call::MoveMount {
                    source: fixture.path("export/dir").display().to_string(),
                    target: moved.display().to_string(),
                },
                Call::Detach(moved.display().to_string()),
            ]
        );
        assert_eq!(fixture.kernel.mount_at(&moved).is_some(), detach.is_err());
        assert_eq!(fixture.kernel.mount_at(&fixture.path("root/data/x")), None);
        // The mapper tracks it as no mapping, but its teardown detaches one that stayed, wherever
        // it is.
        assert_eq!(mapper.targets().len(), 0);
        fixture.kernel.restore(Operation::Detach, &moved);
        mapper.teardown_with(&mut fixture.kernel()).unwrap();
        assert_eq!(fixture.kernel.mount_at(&moved), None);
        let detaches = fixture
            .kernel
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::Detach(_)))
            .count();
        assert_eq!(detaches, if detach.is_err() { 2 } else { 1 });
    }

    // A detach that fails again at the teardown is reported, first, with the index after the
    // mappings'.
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path("root/data/x")).unwrap();
    let mut mapper = fixture.mapper(Reserved::default());
    fixture
        .apply(&mut mapper, &[mapping("dir", "/kept", Access::ReadOnly)])
        .unwrap();
    let base = fixture.base.clone();
    fixture.kernel.on_move(move |_| {
        fs::rename(base.join("root/data"), base.join("root/moved")).unwrap();
    });
    let moved = fixture.path("root/moved/x");
    fixture.kernel.fail(Operation::Detach, &moved, Errno::EBUSY);
    let error = fixture
        .apply(&mut mapper, &[mapping("dir", "/data/x", Access::ReadOnly)])
        .unwrap_err();
    assert_eq!(error.error(), &MappingError::Moved { mounted: true });
    let calls = fixture.kernel.calls().len();
    let failures = mapper.unmount_with(&mut fixture.kernel());
    assert_eq!(
        failures,
        [(
            1,
            UnmountFailure {
                target: "/data/x".into(),
                error: failed(Step::Unmount, Errno::EBUSY),
            }
        )]
    );
    assert_eq!(
        fixture.kernel.calls()[calls..],
        [
            Call::Detach(moved.display().to_string()),
            Call::Unmount(fixture.path("root/kept").display().to_string()),
        ]
    );
    assert!(fixture.kernel.mount_at(&moved).is_some());
    // The teardown forgets it, as it forgets a mapping that it failed to unmount.
    assert!(mapper.unmount_with(&mut fixture.kernel()).is_empty());
    assert_eq!(fixture.kernel.calls().len(), calls + 2);
}

#[test]
fn what_a_rename_puts_in_the_place_of_a_created_component_keeps_its_mode() {
    let fixture = Fixture::new();
    // A directory in the same parent, as only one there can take the place of a directory that
    // has no permissions: moving a directory to another parent takes the permission to write it.
    let victim = fixture.path("root/victim");
    fs::create_dir(&victim).unwrap();
    fs::set_permissions(&victim, fs::Permissions::from_mode(0o700)).unwrap();
    let swapped = victim.clone();
    // A rename between the creation of `/data` and the walk's open of it puts another directory
    // there.
    fixture.kernel.on_create(move |created| {
        if created.ends_with("root/data") {
            let (created, swapped) = (
                sys::path_c_string(created).unwrap(),
                sys::path_c_string(&swapped).unwrap(),
            );
            sys::exchange_paths(&created, &swapped).unwrap();
        }
    });
    let mut mapper = fixture.mapper(Reserved::default());
    fixture
        .apply(&mut mapper, &[mapping("dir", "/data/x", Access::ReadOnly)])
        .unwrap();
    // The walk went on through that directory, as through an existing one, and changed nothing:
    // neither its mode, nor that of the directory that the mapper created, which the rename took
    // away before the mapper gave it its mode, and which has no permissions still.
    assert_eq!(mode(&fixture.path("root/data")), 0o700);
    assert!(fixture.path("root/data/x").is_dir());
    assert_eq!(mode(&victim), 0o000);
    fs::set_permissions(&victim, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(fs::read_dir(&victim).unwrap().next().is_none());
}

#[test]
fn a_teardown_unmounts_in_reverse_order_and_reports_each_failure() {
    let fixture = Fixture::new();
    let mut mapper = fixture.mapper(Reserved::default());
    fs::create_dir_all(fixture.path("root/a/inner")).unwrap();
    fixture
        .apply(
            &mut mapper,
            &[
                mapping("dir", "/a", Access::ReadOnly),
                mapping("dir/inner", "/a/inner", Access::ReadWrite),
                mapping("file.txt", "/b/f", Access::ReadOnly),
                mapping("dir", "/c", Access::ReadOnly),
            ],
        )
        .unwrap();
    fixture
        .kernel
        .fail(Operation::Unmount, fixture.path("root/b/f"), Errno::EBUSY);
    let failures = mapper.unmount_with(&mut fixture.kernel());
    assert_eq!(
        failures,
        [(
            2,
            UnmountFailure {
                target: "/b/f".into(),
                error: failed(Step::Unmount, Errno::EBUSY),
            }
        )]
    );
    // Each is attempted, by its name in its parent, after its own mappings' children.
    assert_eq!(
        fixture.kernel.unmounts(),
        ["root/c", "root/b/f", "root/a/inner", "root/a"]
            .map(|path| fixture.path(path).display().to_string())
    );
    assert!(fixture.kernel.mount_at(&fixture.path("root/b/f")).is_some());
    assert_eq!(fixture.kernel.mount_at(&fixture.path("root/a")), None);
    assert_eq!(mapper.targets().len(), 0);

    // A mapping that is not the topmost mount at its target, or that is missing, stays.
    let fixture = Fixture::new();
    let mut mapper = fixture.mapper(Reserved::default());
    fixture
        .apply(
            &mut mapper,
            &[
                mapping("dir", "/covered", Access::ReadOnly),
                mapping("dir", "/renamed", Access::ReadOnly),
                mapping("dir", "/kept", Access::ReadOnly),
            ],
        )
        .unwrap();
    fixture.kernel.add_mount(fixture.path("root/covered"));
    fs::rename(fixture.path("root/renamed"), fixture.path("root/elsewhere")).unwrap();
    let error = mapper.teardown_with(&mut fixture.kernel()).unwrap_err();
    assert_eq!(
        error.failures(),
        [
            UnmountFailure {
                target: "/renamed".into(),
                error: failed(Step::Locate, Errno::ENOENT),
            },
            UnmountFailure {
                target: "/covered".into(),
                error: MappingError::Moved { mounted: true },
            },
        ]
    );
    assert_eq!(
        error.to_string(),
        format!(
            "failed to unmount the mapping at /renamed: failed to find the mapping at its target: \
             {}; failed to unmount the mapping at /covered: the mapping is not at its target, and \
             it stays mounted",
            io::Error::from_raw_os_error(Errno::ENOENT as i32)
        )
    );
    assert_eq!(
        fixture.kernel.unmounts(),
        [fixture.path("root/kept").display().to_string()]
    );
}

#[test]
fn a_runtime_bind_skips_the_checks_of_a_host_mapping_but_not_the_walk() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path("root/etc")).unwrap();
    fs::write(fixture.path("root/etc/machine-id"), b"image\n").unwrap();
    let root = fixture.root();
    let source = fixture.source();
    let bind = |mapping: Mapping| bind_with(&mut fixture.kernel(), &root, &source, &mapping);
    // Onto targets that a container root's mapper refuses, creating what they lack.
    bind(mapping("file.txt", "/etc/machine-id", Access::ReadOnly)).unwrap();
    bind(mapping("dir", "/run/nvx/time", Access::ReadOnly)).unwrap();
    let expected: Vec<Call> = [("file.txt", "etc/machine-id"), ("dir", "run/nvx/time")]
        .into_iter()
        .flat_map(|(source, target)| {
            attach_calls(
                &fixture.path(&format!("export/{source}")),
                &fixture.path(&format!("root/{target}")),
                Access::ReadOnly,
            )
        })
        .collect();
    assert_eq!(fixture.kernel.calls(), expected);
    assert_eq!(mode(&fixture.path("root/run/nvx")), 0o755);
    // Through no link, and onto a target of its source's kind.
    std::os::unix::fs::symlink("etc", fixture.path("root/link")).unwrap();
    assert_eq!(
        bind(mapping("file.txt", "/link/machine-id", Access::ReadOnly)),
        Err(MappingError::Link {
            path: "/link".into()
        })
    );
    assert_eq!(
        bind(mapping("dir", "/etc/machine-id", Access::ReadOnly)),
        Err(MappingError::TargetKind)
    );
    // A bind is not a mapping that a mapper tracks.
    let mapper = Mapper::new(fixture.root(), Reserved::container_root());
    assert_eq!(mapper.targets().len(), 0);
}

#[test]
fn runtime_binds_unbind_in_reverse_order_and_report_each_failure() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let source = fixture.source();
    let bind_all = || {
        [
            ("file.txt", "/etc/machine-id"),
            ("dir", "/run/nvx/time"),
            ("file.txt", "/etc/resolv.conf"),
        ]
        .map(|(source_path, target)| {
            let mapping = mapping(source_path, target, Access::ReadOnly);
            bind_with(&mut fixture.kernel(), &root, &source, &mapping).unwrap()
        })
    };
    let points = [
        "root/etc/resolv.conf",
        "root/run/nvx/time",
        "root/etc/machine-id",
    ]
    .map(|point| fixture.path(point));
    // Each bind is the mount at its target, which the unbind takes off, last first.
    let binds = bind_all();
    assert_eq!(
        binds.iter().map(Mapped::target).collect::<Vec<_>>(),
        ["etc/machine-id", "run/nvx/time", "etc/resolv.conf"].map(Path::new)
    );
    for (bind, point) in binds.iter().rev().zip(&points) {
        assert_eq!(fixture.kernel.mount_at(point), Some(bind.mount));
    }
    unbind_with(&mut fixture.kernel(), &root, &binds).unwrap();
    assert_eq!(
        fixture.kernel.unmounts(),
        points.clone().map(|point| point.display().to_string())
    );
    assert!(
        points
            .iter()
            .all(|point| fixture.kernel.mount_at(point).is_none())
    );
    // Each is attempted, whatever failed before, and one that is busy, or that another mount
    // covers, stays and is reported.
    let binds = bind_all();
    fixture
        .kernel
        .fail(Operation::Unmount, &points[0], Errno::EBUSY);
    let covering = fixture.kernel.add_mount(&points[1]);
    let unmounts = fixture.kernel.unmounts().len();
    let error = unbind_with(&mut fixture.kernel(), &root, &binds).unwrap_err();
    assert_eq!(
        error.failures(),
        [
            UnmountFailure {
                target: "/etc/resolv.conf".into(),
                error: failed(Step::Unmount, Errno::EBUSY),
            },
            UnmountFailure {
                target: "/run/nvx/time".into(),
                error: MappingError::Moved { mounted: true },
            },
        ]
    );
    assert_eq!(
        fixture.kernel.unmounts()[unmounts..],
        [&points[0], &points[2]].map(|point| point.display().to_string())
    );
    assert_eq!(fixture.kernel.mount_at(&points[0]), Some(binds[2].mount));
    assert_eq!(fixture.kernel.mount_at(&points[1]), Some(covering));
    assert_eq!(fixture.kernel.mount_at(&points[2]), None);
    // No binds, nothing to unbind.
    let calls = fixture.kernel.calls().len();
    unbind_with(&mut fixture.kernel(), &root, &[]).unwrap();
    assert_eq!(fixture.kernel.calls().len(), calls);
}

#[test]
fn a_staged_mount_moves_onto_its_target_with_its_restrictions() {
    let fixture = Fixture::new();
    let staging = fixture.path("staging");
    fs::create_dir(&staging).unwrap();
    let staged = fixture.kernel.add_mount(&staging);
    let open = |path: &Path| {
        let flags = OpenFlags::O_PATH | OpenFlags::O_DIRECTORY | OpenFlags::O_CLOEXEC;
        sys::open(&sys::path_c_string(path).unwrap(), flags, Mode::empty()).unwrap()
    };
    let mut mapper = fixture.mapper(Reserved::container_root());
    mapper
        .move_mount_with(
            &mut fixture.kernel(),
            &open(&staging),
            Path::new("srv/share"),
            Access::ReadOnly,
        )
        .unwrap();
    let source = staging.display().to_string();
    assert_eq!(
        fixture.kernel.calls(),
        [
            Call::OpenTree {
                source: source.clone(),
                clone: false,
            },
            Call::SetAttributes {
                source: source.clone(),
                set: MountAttributes::READ_ONLY
                    | MountAttributes::NO_SET_ID
                    | MountAttributes::NO_DEVICES,
                clear: MountAttributes::empty(),
            },
            Call::MoveMount {
                source,
                target: fixture.path("root/srv/share").display().to_string(),
            },
        ]
    );
    assert_eq!(fixture.kernel.mount_at(&staging), None);
    assert_eq!(
        fixture.kernel.mount_at(&fixture.path("root/srv/share")),
        Some(staged)
    );
    assert_eq!(
        mapper.targets().collect::<Vec<_>>(),
        [Path::new("srv/share")]
    );
    // A descriptor of a directory that is not a mount's root is no staged mount.
    let mut mapper = fixture.mapper(Reserved::container_root());
    let moved = mapper.move_mount_with(
        &mut fixture.kernel(),
        &open(&fixture.path("elsewhere")),
        Path::new("srv/other"),
        Access::ReadOnly,
    );
    assert_eq!(moved, Err(MappingError::SourceKind));
    // Nor is one that the profile's checks refuse.
    let mut mapper = fixture.mapper(Reserved::container_root());
    let moved = mapper.move_mount_with(
        &mut fixture.kernel(),
        &open(&fixture.path("export")),
        Path::new("etc"),
        Access::ReadOnly,
    );
    assert_eq!(moved, Err(MappingError::Reserved));
}

#[test]
fn a_target_root_is_the_root_of_a_mount_of_a_whole_file_system() {
    let fixture = Fixture::new();
    let error =
        TargetRoot::open_with(&mut fixture.kernel(), &fixture.path("elsewhere")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        format!(
            "{} is not the root of a mount",
            fixture.path("elsewhere").display()
        )
    );
    std::os::unix::fs::symlink(fixture.path("root"), fixture.path("link")).unwrap();
    assert!(TargetRoot::open_with(&mut fixture.kernel(), &fixture.path("link")).is_err());
    TargetRoot::open_with(&mut fixture.kernel(), &fixture.path("root")).unwrap();

    // Not of a bind of a directory below its file system's root, whose files a rename through
    // another mount could move out of it, neither as the root nor as one of its own mounts.
    let bound = fixture.path("bound");
    fs::create_dir_all(bound.join("inner")).unwrap();
    fixture.kernel.add_subtree_mount(&bound);
    let refused =
        "is on a mount of a directory below its file system's root, not of the whole file system";
    let error = TargetRoot::open_with(&mut fixture.kernel(), &bound).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(error.to_string(), format!("{} {refused}", bound.display()));
    let mut root = fixture.root();
    let error = root
        .add_mount_with(&mut fixture.kernel(), &bound.join("inner"))
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        format!("{} {refused}", bound.join("inner").display())
    );
    // Any directory on a whole file system's mount adds it.
    root.add_mount_with(&mut fixture.kernel(), &fixture.path("elsewhere"))
        .unwrap();
    // A failure to read a mount's root fails the open.
    fixture
        .kernel
        .fail(Operation::MountRoot, fixture.path("root"), Errno::ENOSYS);
    let error = TargetRoot::open_with(&mut fixture.kernel(), &fixture.path("root")).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(Errno::ENOSYS as i32));
}

#[test]
fn mappings_name_paths_below_their_roots() {
    let longest = "a".repeat(MAX_PATH_BYTES);
    for source in ["0", "0/workspace", "a/.../b", ".hidden", longest.as_str()] {
        Mapping::new(source, "/target", Access::ReadOnly).unwrap();
    }
    Mapping::new(
        std::ffi::OsStr::from_bytes(b"0/\xff"),
        "/t",
        Access::ReadOnly,
    )
    .unwrap();
    // An empty source names the source root itself.
    let root = Mapping::new("", "/target", Access::ReadOnly).unwrap();
    assert_eq!(root.source(), Path::new(""));
    let too_long = format!("{longest}a");
    for source in [
        "/",
        "/0",
        ".",
        "..",
        "./",
        "a/../b",
        "a/./b",
        "a//b",
        "a/",
        "a\0b",
        "\0",
        too_long.as_str(),
    ] {
        assert_eq!(
            Mapping::new(source, "/target", Access::ReadOnly),
            Err(InvalidMapping::Source),
            "{source:?}"
        );
    }
    let longest = format!("/{}", "a".repeat(MAX_PATH_BYTES - 1));
    let mapping = Mapping::new("0", "/work/space", Access::ReadWrite).unwrap();
    assert_eq!(
        (
            mapping.source(),
            mapping.target(),
            mapping.access(),
            mapping.kind()
        ),
        (
            Path::new("0"),
            Path::new("work/space"),
            Access::ReadWrite,
            None
        )
    );
    assert_eq!(mapping.with_kind(Kind::File).kind(), Some(Kind::File));
    Mapping::new("0", &longest, Access::ReadOnly).unwrap();
    let too_long = format!("{longest}a");
    for target in [
        "",
        "relative",
        "/",
        "//a",
        "/a/",
        "/a//b",
        "/a/./b",
        "/a/..",
        "/a\0",
        too_long.as_str(),
    ] {
        assert_eq!(
            Mapping::new("0", target, Access::ReadOnly),
            Err(InvalidMapping::Target),
            "{target:?}"
        );
    }
    assert_eq!(
        InvalidMapping::Target.to_string(),
        format!(
            "a mapping target must be an absolute path of at most {MAX_PATH_BYTES} bytes without \
             NUL bytes or empty, `.`, or `..` components"
        )
    );
    assert_eq!(
        InvalidMapping::Source.to_string(),
        format!(
            "a mapping source must be empty, for its root, or a relative path of at most \
             {MAX_PATH_BYTES} bytes without NUL bytes or empty, `.`, or `..` components"
        )
    );
}

#[test]
fn an_empty_source_maps_the_source_root_itself_from_beneath_it() {
    let fixture = Fixture::new();
    fs::write(fixture.path("root/plain"), b"").unwrap();
    // The source root itself, which the mapper opens as `.` beneath it, is copied alone and
    // restricted before it is attached, as any directory below it is.
    let mut mapper = fixture.mapper(Reserved::default());
    let whole = mapping("", "/whole", Access::ReadOnly);
    fixture.apply(&mut mapper, &[whole]).unwrap();
    assert_eq!(
        fixture.kernel.calls(),
        attach_calls(
            &fixture.path("export"),
            &fixture.path("root/whole"),
            Access::ReadOnly
        )
    );
    assert!(fixture.path("root/whole").is_dir());
    mapper.teardown_with(&mut fixture.kernel()).unwrap();
    // It is a directory, which goes only onto a directory, and only where a mapping of a
    // directory may go.
    let calls = fixture.kernel.calls();
    let refusal = |mapping: Mapping| fixture.refusal(Reserved::default(), mapping);
    let read_only = Access::ReadOnly;
    assert_eq!(
        refusal(mapping("", "/plain", read_only)),
        MappingError::TargetKind
    );
    let file = mapping("", "/file", read_only).with_kind(Kind::File);
    assert_eq!(refusal(file), MappingError::SourceKind);
    assert_eq!(
        refusal(mapping("", "/run/nvx", read_only)),
        MappingError::Reserved
    );
    // Every other source is still opened through no link, and on the root's mount, once a mount
    // below the root shows up, beneath which the root itself still opens.
    fixture.kernel.add_mount(fixture.path("export/dir"));
    assert_eq!(
        refusal(mapping("link", "/link", read_only)),
        failed(Step::Source, Errno::ELOOP)
    );
    assert_eq!(
        refusal(mapping("dir", "/crossed", read_only)),
        failed(Step::Source, Errno::EXDEV)
    );
    assert_eq!(fixture.kernel.calls(), calls);
    let mut mapper = fixture.mapper(Reserved::default());
    let again = mapping("", "/again", Access::ReadWrite);
    fixture.apply(&mut mapper, &[again]).unwrap();
    let attached: Vec<Call> = calls
        .into_iter()
        .chain(attach_calls(
            &fixture.path("export"),
            &fixture.path("root/again"),
            Access::ReadWrite,
        ))
        .collect();
    assert_eq!(fixture.kernel.calls(), attached);
}

#[test]
fn errors_name_what_failed() {
    let texts = [
        (MappingError::Latched, "an earlier mapping failed"),
        (MappingError::Reserved, "the runtime reserves the target"),
        (
            MappingError::Overlap {
                mapped: "/a".into(),
            },
            "the target would shadow the mapping at /a",
        ),
        (
            MappingError::SourceKind,
            "the source is not a directory or a regular file of the mapping's kind",
        ),
        (
            MappingError::Link { path: "/l".into() },
            "/l is a symbolic link or a magic link",
        ),
        (
            MappingError::NotDirectory { path: "/f".into() },
            "/f is not a directory",
        ),
        (
            MappingError::TargetKind,
            "the target exists as another kind than its source",
        ),
        (
            MappingError::Foreign { path: "/m".into() },
            "/m is not on one of the root's own mounts or inside an earlier mapping",
        ),
        (
            MappingError::MountPoint,
            "the target is the root of a mount that the root does not own",
        ),
        (
            MappingError::Moved { mounted: false },
            "the mapping is not at its target",
        ),
    ];
    for (error, text) in texts {
        assert_eq!(error.to_string(), text);
    }
    assert_eq!(
        failed(Step::Restrict, Errno::EPERM).to_string(),
        format!(
            "failed to restrict the mount: {}",
            io::Error::from_raw_os_error(Errno::EPERM as i32)
        )
    );
}
