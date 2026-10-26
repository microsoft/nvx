//! Tests of the mapper with the kernel itself, which need the privilege to mount and Linux 6.8 or
//! later, for the unique mount IDs. Each runs in a private mount namespace of its own thread, on
//! tmpfs roots below a temporary directory, and they are ignored unless asked for:
//! `cargo test --all-features -- --ignored guest::mappings::privileged`, as root.
//!
//! They cover what the fake kernel cannot: symbolic links and magic links at every component, a
//! rename race during the walk, kinds, a target inside another mapping, reserved targets, the
//! source root itself, which an empty source maps as a copy of its own mount alone, a mapping
//! that covers the root of one of the root's own mounts until the teardown, what a mapper
//! creates, which a workload that is not root enters whatever the umask or a default ACL, a
//! read-only mapping that is never visible writable, even briefly, a teardown's unmounts, from a
//! thread with a descriptor table of its own too, and the roots of binds of directories below
//! their file systems' roots, which `statmount` tells and no target root may be on.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use nix::sys::statvfs::{FsFlags, statvfs};

use super::*;

/// A target root, a source root, and a directory outside both, each a tmpfs below a temporary
/// directory, in a private mount namespace of the calling thread.
struct Scene {
    /// The temporary directory's canonical path.
    base: PathBuf,
    /// The temporary directory, which the scene removes once it has detached its mounts.
    directory: Option<tempfile::TempDir>,
}

impl Scene {
    /// Returns a scene whose source root holds the directory `dir`, with the directory `inner`
    /// and the file `marker`, and the file `file.txt`, or `None` where the tests cannot run:
    /// without root, or without unique mount IDs.
    fn new() -> Option<Self> {
        if !nix::unistd::geteuid().is_root() {
            report("skipped: the test needs root");
            return None;
        }
        sys::enter_private_mount_namespace().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let scene = Self {
            base,
            directory: Some(directory),
        };
        for name in ["root", "export", "outside"] {
            let path = scene.path(name);
            fs::create_dir(&path).unwrap();
            sys::mount(
                Some(c"tmpfs"),
                &sys::path_c_string(&path).unwrap(),
                Some(c"tmpfs"),
                MountFlags::empty(),
                Some(c"mode=0755"),
            )
            .unwrap();
        }
        fs::create_dir_all(scene.path("export/dir/inner")).unwrap();
        fs::write(scene.path("export/dir/marker"), b"host").unwrap();
        fs::write(scene.path("export/file.txt"), b"file").unwrap();
        match sys::file_status(&open(&scene.path("root"))) {
            Err(Errno::EOPNOTSUPP) => {
                report("skipped: the kernel has no unique mount IDs");
                None
            }
            status => {
                assert!(status.unwrap().mount_root);
                Some(scene)
            }
        }
    }

    /// Returns `relative`'s path below the temporary directory.
    fn path(&self, relative: &str) -> PathBuf {
        self.base.join(relative)
    }

    /// Returns a mapper beneath the target root, with `reserved`.
    fn mapper(&self, reserved: Reserved) -> Mapper {
        Mapper::new(TargetRoot::open(&self.path("root")).unwrap(), reserved)
    }

    /// Returns the source root.
    fn source(&self) -> SourceRoot {
        SourceRoot::open(&self.path("export")).unwrap()
    }

    /// Applies the one mapping of `source` at `target` with a new mapper with `reserved`, and
    /// returns its error.
    fn refusal(&self, reserved: Reserved, source: &str, target: &str) -> MappingError {
        let mut mapper = self.mapper(reserved);
        let mapping = Mapping::new(source, target, Access::ReadWrite).unwrap();
        let error = mapper.apply(&self.source(), &[mapping]).unwrap_err();
        assert!(mapper.is_latched());
        error.error().clone()
    }
}

impl Drop for Scene {
    /// Detaches the scene's mounts, with what is mounted below them, then removes the directory.
    fn drop(&mut self) {
        for name in ["root", "export", "outside"] {
            let _ = nix::mount::umount2(&self.path(name), nix::mount::MntFlags::MNT_DETACH);
        }
        drop(self.directory.take());
    }
}

/// Opens `path` with `O_PATH`.
fn open(path: &Path) -> FileDescriptor {
    let flags = OpenFlags::O_PATH | OpenFlags::O_CLOEXEC;
    sys::open(&sys::path_c_string(path).unwrap(), flags, Mode::empty()).unwrap()
}

/// Writes `message` to the standard error, past the test harness's capture, as the guest's
/// programs write theirs.
fn report(message: &str) {
    use std::io::Write;

    let _ = writeln!(std::io::stderr(), "{message}");
}

/// Returns the mount points below `path`, not `path` itself, in the thread's mount namespace.
fn mounts_below(path: &Path) -> Vec<PathBuf> {
    let table = fs::read_to_string("/proc/thread-self/mountinfo").unwrap();
    table
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(PathBuf::from)
        .filter(|point| point.starts_with(path) && point != path)
        .collect()
}

/// Returns the unique ID of the mount at `path`, and whether `path` is its root.
fn mount_of(path: &Path) -> (u64, bool) {
    let status = sys::file_status(&open(path)).unwrap();
    (status.mount, status.mount_root)
}

/// Binds `source` at `target`.
fn bind_mount(source: &Path, target: &Path) {
    let source = sys::path_c_string(source).unwrap();
    sys::mount(
        Some(source.as_c_str()),
        &sys::path_c_string(target).unwrap(),
        None,
        MountFlags::MS_BIND,
        None,
    )
    .unwrap();
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn a_target_root_shows_its_whole_file_system() {
    let Some(scene) = Scene::new() else {
        return;
    };
    let root = scene.path("root");
    fs::create_dir_all(root.join("sub/inner")).unwrap();
    let (bound, whole) = (scene.path("bound"), scene.path("whole"));
    fs::create_dir(&bound).unwrap();
    fs::create_dir(&whole).unwrap();
    bind_mount(&root.join("sub"), &bound);
    bind_mount(&scene.path("export"), &whole);
    assert_eq!(sys::is_whole_file_system(mount_of(&bound).0), Ok(false));
    assert_eq!(sys::is_whole_file_system(mount_of(&whole).0), Ok(true));
    // A bind of a directory below its file system's root, whose files a rename through another
    // mount of that file system could move out of it, is neither a root nor one of its mounts.
    let error = TargetRoot::open(&bound).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    let mut target = TargetRoot::open(&root).unwrap();
    let error = target.add_mount(&bound.join("inner")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    // A bind of a file system's root shows the whole file system.
    TargetRoot::open(&whole).unwrap();
    target.add_mount(&whole).unwrap();
    for path in [&bound, &whole] {
        nix::mount::umount2(path, nix::mount::MntFlags::MNT_DETACH).unwrap();
    }
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn links_and_magic_links_at_every_component_are_refused() {
    let Some(scene) = Scene::new() else {
        return;
    };
    let root = scene.path("root");
    let outside = scene.path("outside");
    // Each component of `/tN/a/b/new` in turn is a link to the directory outside the root.
    let components = ["a", "b", "new"];
    for (index, link) in components.iter().enumerate() {
        let mut path = root.join(format!("t{index}"));
        for component in &components[..index] {
            path.push(component);
        }
        fs::create_dir_all(&path).unwrap();
        std::os::unix::fs::symlink(&outside, path.join(link)).unwrap();
        let target = format!("/t{index}/{}", components.join("/"));
        let error = scene.refusal(Reserved::default(), "dir", &target);
        let mut linked = format!("/t{index}");
        for component in &components[..=index] {
            linked.push('/');
            linked.push_str(component);
        }
        assert_eq!(
            error,
            MappingError::Link {
                path: linked.into()
            },
            "{target}"
        );
    }
    // A relative link that leads above the root, and one at the root's first component.
    std::os::unix::fs::symlink("..", root.join("up")).unwrap();
    assert_eq!(
        scene.refusal(Reserved::default(), "dir", "/up/new"),
        MappingError::Link { path: "/up".into() }
    );
    // Magic links of a procfs mounted in the root.
    let proc = root.join("proc");
    fs::create_dir(&proc).unwrap();
    sys::mount(
        Some(c"proc"),
        &sys::path_c_string(&proc).unwrap(),
        Some(c"proc"),
        MountFlags::empty(),
        None,
    )
    .unwrap();
    let process = std::process::id();
    let held = open(&outside);
    let descriptor = std::os::fd::AsRawFd::as_raw_fd(&held.borrowed());
    for (target, link) in [
        ("/proc/self/new".to_owned(), "/proc/self".to_owned()),
        ("/proc/thread-self/new".into(), "/proc/thread-self".into()),
        (
            format!("/proc/{process}/root/new"),
            format!("/proc/{process}/root"),
        ),
        (
            format!("/proc/{process}/cwd/new"),
            format!("/proc/{process}/cwd"),
        ),
        (
            format!("/proc/{process}/fd/{descriptor}/new"),
            format!("/proc/{process}/fd/{descriptor}"),
        ),
    ] {
        assert_eq!(
            scene.refusal(Reserved::default(), "dir", &target),
            MappingError::Link { path: link.into() },
            "{target}"
        );
    }
    // Nothing was created or mounted outside the root, nor mounted in it but procfs.
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    assert!(mounts_below(&outside).is_empty());
    assert_eq!(mounts_below(&root), [proc]);
    assert!(!scene.base.join("new").exists());
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn a_rename_race_never_mounts_outside_the_target() {
    let Some(scene) = Scene::new() else {
        return;
    };
    let root = scene.path("root");
    let outside = scene.path("outside");
    fs::create_dir(root.join("a")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    // Another thread, in the same mount namespace, keeps exchanging the target's directory with
    // another directory and with a link out of the root.
    let stop = Arc::new(AtomicBool::new(false));
    let racer = {
        let (stop, root) = (Arc::clone(&stop), root.clone());
        std::thread::spawn(move || {
            let path = |name: &str| sys::path_c_string(&root.join(name)).unwrap();
            let (target, other, link) = (path("a"), path("b"), path("link"));
            while !stop.load(Ordering::Relaxed) {
                sys::exchange_paths(&target, &other).unwrap();
                sys::exchange_paths(&target, &link).unwrap();
            }
        })
    };
    let source = scene.source();
    let mut attached = 0;
    let (mut linked, mut moved) = (0, 0);
    for attempt in 0..2000 {
        let mut mapper = scene.mapper(Reserved::default());
        let mapping = Mapping::new("dir", format!("/a/x{attempt}"), Access::ReadOnly).unwrap();
        match mapper.apply(&source, &[mapping]) {
            Ok(()) => attached += 1,
            Err(error) => match error.error() {
                MappingError::Link { .. } => linked += 1,
                MappingError::Moved { mounted: false } => moved += 1,
                other => panic!("attempt {attempt}: {other}"),
            },
        }
    }
    stop.store(true, Ordering::Relaxed);
    racer.join().unwrap();
    report(&format!(
        "{attached} attached; refused: {linked} at a link, {moved} moved and detached"
    ));
    // Nothing went outside the root, and each mapping that a rename moved was detached: what
    // stays is exactly what the mapper reported attached.
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    assert!(mounts_below(&outside).is_empty());
    assert_eq!(mounts_below(&root).len(), attached);
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn kinds_nested_targets_and_reserved_targets_are_checked() {
    let Some(scene) = Scene::new() else {
        return;
    };
    let root = scene.path("root");
    fs::write(root.join("plain"), b"").unwrap();
    fs::create_dir(root.join("directory")).unwrap();
    assert_eq!(
        scene.refusal(Reserved::default(), "dir", "/plain"),
        MappingError::TargetKind
    );
    assert_eq!(
        scene.refusal(Reserved::default(), "file.txt", "/directory"),
        MappingError::TargetKind
    );
    // A mapping may go onto what an earlier one shows, but creates nothing inside it, which would
    // write to the host's directory.
    let mut mapper = scene.mapper(Reserved::default());
    let mappings = [
        Mapping::new("dir", "/m", Access::ReadWrite).unwrap(),
        Mapping::new("file.txt", "/m/marker", Access::ReadOnly).unwrap(),
    ];
    mapper.apply(&scene.source(), &mappings).unwrap();
    let (outer, outer_root) = mount_of(&root.join("m"));
    let (inner, inner_root) = mount_of(&root.join("m/marker"));
    assert!(outer_root && inner_root && outer != inner);
    assert_eq!(fs::read(root.join("m/marker")).unwrap(), b"file");
    let nested = Mapping::new("dir/inner", "/m/new", Access::ReadOnly).unwrap();
    let error = mapper.apply(&scene.source(), &[nested]).unwrap_err();
    assert_eq!(
        error.error(),
        &MappingError::Foreign {
            path: "/m/new".into()
        }
    );
    assert!(!scene.path("export/dir/new").exists());
    // The runtime's tree and its ancestors, and a profile's trees.
    for target in ["/run", "/run/nvx", "/run/nvx/time", "/run/nvx/time/x"] {
        assert_eq!(
            scene.refusal(Reserved::default(), "dir", target),
            MappingError::Reserved,
            "{target}"
        );
    }
    for target in [
        "/etc",
        "/etc/machine-id",
        "/etc/resolv.conf",
        "/proc/x",
        "/dev",
    ] {
        assert_eq!(
            scene.refusal(Reserved::container_root(), "dir", target),
            MappingError::Reserved,
            "{target}"
        );
    }
    assert!(!root.join("run").exists() && !root.join("etc").exists());
    mapper.teardown().unwrap();
    assert!(mounts_below(&root).is_empty());
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn an_empty_source_maps_a_copy_of_the_source_root_s_own_mount() {
    let Some(scene) = Scene::new() else {
        return;
    };
    let (root, export) = (scene.path("root"), scene.path("export"));
    // A file system mounted below the source root, and a link in it.
    let inner = export.join("dir/inner");
    sys::mount(
        Some(c"tmpfs"),
        &sys::path_c_string(&inner).unwrap(),
        Some(c"tmpfs"),
        MountFlags::empty(),
        Some(c"mode=0755"),
    )
    .unwrap();
    fs::write(inner.join("below"), b"below").unwrap();
    std::os::unix::fs::symlink("dir", export.join("link")).unwrap();
    let mut mapper = scene.mapper(Reserved::default());
    let whole = Mapping::new("", "/whole", Access::ReadOnly).unwrap();
    mapper.apply(&scene.source(), &[whole]).unwrap();
    // The target is the root of a copy of the source root's own mount, read-only, `nosuid`, and
    // `nodev`, which shows the whole export, but not the file system mounted below it.
    let mapped = root.join("whole");
    let (mount, mount_root) = mount_of(&mapped);
    assert!(mount_root);
    assert_ne!(mount, mount_of(&export).0);
    assert_eq!(fs::read(mapped.join("file.txt")).unwrap(), b"file");
    assert_eq!(fs::read(mapped.join("dir/marker")).unwrap(), b"host");
    assert_eq!(fs::read_dir(mapped.join("dir/inner")).unwrap().count(), 0);
    let flags = statvfs(&mapped).unwrap().flags();
    assert!(flags.contains(FsFlags::ST_RDONLY | FsFlags::ST_NOSUID | FsFlags::ST_NODEV));
    let written = fs::write(mapped.join("new"), b"guest").unwrap_err();
    assert_eq!(written.raw_os_error(), Some(Errno::EROFS as i32));
    assert_eq!(mounts_below(&root), [mapped]);
    // Every other source still opens through no link, and on the source root's own mount.
    let refused = |error: Errno| MappingError::System {
        step: Step::Source,
        error: error as i32,
    };
    assert_eq!(
        scene.refusal(Reserved::default(), "link", "/link"),
        refused(Errno::ELOOP)
    );
    assert_eq!(
        scene.refusal(Reserved::default(), "dir/inner", "/inner"),
        refused(Errno::EXDEV)
    );
    mapper.teardown().unwrap();
    assert!(mounts_below(&root).is_empty());
    assert!(!export.join("new").exists());
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn a_mapping_covers_the_root_of_a_mount_only_if_the_root_owns_it() {
    let Some(scene) = Scene::new() else {
        return;
    };
    // A tmpfs at the root's `/tmp`, as a guest's init mounts one.
    let root = scene.path("root");
    let tmp = root.join("tmp");
    fs::create_dir(&tmp).unwrap();
    sys::mount(
        Some(c"tmpfs"),
        &sys::path_c_string(&tmp).unwrap(),
        Some(c"tmpfs"),
        MountFlags::empty(),
        Some(c"mode=1777"),
    )
    .unwrap();
    fs::write(tmp.join("own"), b"tmpfs").unwrap();
    let (tmpfs, _) = mount_of(&tmp);
    assert_eq!(
        scene.refusal(Reserved::default(), "dir", "/tmp"),
        MappingError::MountPoint
    );
    let mut target = TargetRoot::open(&root).unwrap();
    target.add_mount(&tmp).unwrap();
    let mut mapper = Mapper::new(target, Reserved::default());
    let mapping = Mapping::new("dir", "/tmp", Access::ReadWrite).unwrap();
    mapper.apply(&scene.source(), &[mapping]).unwrap();
    let (covering, covering_root) = mount_of(&tmp);
    assert!(covering_root && covering != tmpfs);
    assert_eq!(fs::read(tmp.join("marker")).unwrap(), b"host");
    assert!(!tmp.join("own").exists());
    // The teardown uncovers it.
    mapper.teardown().unwrap();
    assert_eq!(mount_of(&tmp), (tmpfs, true));
    assert_eq!(fs::read(tmp.join("own")).unwrap(), b"tmpfs");
    assert_eq!(mounts_below(&root), [tmp]);
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn a_workload_that_is_not_root_enters_what_a_mapper_created_whatever_the_umask_or_a_default_acl() {
    // A thread of the test's own, whose umask, once it unshares its mount namespace, is its own.
    std::thread::spawn(|| {
        let Some(scene) = Scene::new() else {
            return;
        };
        nix::sys::stat::umask(Mode::from_bits_truncate(0o077));
        // A workload enters the temporary directory as it enters a guest's `/`.
        fs::set_permissions(&scene.base, fs::Permissions::from_mode(0o755)).unwrap();
        let root = scene.path("root");
        // A directory whose default ACL withholds the group's and others' permissions from what
        // is created in it, in the umask's stead.
        let restricted = root.join("restricted");
        fs::create_dir(&restricted).unwrap();
        fs::set_permissions(&restricted, fs::Permissions::from_mode(0o755)).unwrap();
        let parents: &[&str] = if sys::restrict_new_entries(&restricted) {
            &["srv/new", "restricted/new"]
        } else {
            &["srv/new"]
        };
        let mut mapper = scene.mapper(Reserved::default());
        let mut mappings = Vec::new();
        for parent in parents {
            let (share, file) = (format!("/{parent}/share"), format!("/{parent}/app.conf"));
            mappings.push(Mapping::new("dir", share, Access::ReadOnly).unwrap());
            mappings.push(Mapping::new("file.txt", file, Access::ReadOnly).unwrap());
        }
        mapper.apply(&scene.source(), &mappings).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        for directory in ["srv"].iter().chain(parents) {
            assert_eq!(mode(&root.join(directory)), 0o755, "{directory}");
        }
        // A thread with the file system identity of a workload that is not root, which takes
        // the root's file system capabilities from it, reads every mapping.
        let expected: Vec<(PathBuf, &[u8])> = parents
            .iter()
            .flat_map(|parent| {
                [
                    (root.join(parent).join("share/marker"), &b"host"[..]),
                    (root.join(parent).join("app.conf"), &b"file"[..]),
                ]
            })
            .collect();
        let paths: Vec<PathBuf> = expected.iter().map(|(path, _)| path.clone()).collect();
        let read = std::thread::spawn(move || {
            let nobody = 65534;
            nix::unistd::setfsgid(nix::unistd::Gid::from_raw(nobody));
            nix::unistd::setfsuid(nix::unistd::Uid::from_raw(nobody));
            paths.iter().map(fs::read).collect::<Vec<_>>()
        })
        .join()
        .unwrap();
        for ((path, contents), read) in expected.iter().zip(read) {
            assert_eq!(read.unwrap(), *contents, "{}", path.display());
        }
        mapper.teardown().unwrap();
    })
    .join()
    .unwrap();
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn a_read_only_mapping_is_never_visible_writable() {
    let Some(scene) = Scene::new() else {
        return;
    };
    let marker = scene.path("root/ro/marker");
    // Another thread keeps trying to open the mapping's file for writing, which exists only once
    // the mapping is attached; the lock keeps it out of the teardowns.
    let stop = Arc::new(AtomicBool::new(false));
    let written = Arc::new(AtomicBool::new(false));
    let attempts = Arc::new(RwLock::new(()));
    let racer = {
        let (stop, written, attempts) = (
            Arc::clone(&stop),
            Arc::clone(&written),
            Arc::clone(&attempts),
        );
        let marker = marker.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _attempt = attempts.read().unwrap();
                if fs::OpenOptions::new().write(true).open(&marker).is_ok() {
                    written.store(true, Ordering::Relaxed);
                }
            }
        })
    };
    let source = scene.source();
    for _ in 0..300 {
        let mut mapper = scene.mapper(Reserved::default());
        let mapping = Mapping::new("dir", "/ro", Access::ReadOnly).unwrap();
        mapper.apply(&source, &[mapping]).unwrap();
        let flags = statvfs(&scene.path("root/ro")).unwrap().flags();
        assert!(flags.contains(FsFlags::ST_RDONLY | FsFlags::ST_NOSUID | FsFlags::ST_NODEV));
        let _teardown = attempts.write().unwrap();
        mapper.teardown().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    racer.join().unwrap();
    assert!(!written.load(Ordering::Relaxed));
    assert_eq!(fs::read(scene.path("export/dir/marker")).unwrap(), b"host");
    // A writable mapping is writable, and still `nosuid` and `nodev`.
    let mut mapper = scene.mapper(Reserved::default());
    let mapping = Mapping::new("dir", "/rw", Access::ReadWrite).unwrap();
    mapper.apply(&source, &[mapping]).unwrap();
    let flags = statvfs(&scene.path("root/rw")).unwrap().flags();
    assert!(flags.contains(FsFlags::ST_NOSUID | FsFlags::ST_NODEV));
    assert!(!flags.contains(FsFlags::ST_RDONLY));
    fs::write(scene.path("root/rw/marker"), b"guest").unwrap();
    assert_eq!(fs::read(scene.path("export/dir/marker")).unwrap(), b"guest");
    mapper.teardown().unwrap();
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn a_teardown_unmounts_each_mapping_and_reports_a_busy_one() {
    let Some(scene) = Scene::new() else {
        return;
    };
    let root = scene.path("root");
    let mut mapper = scene.mapper(Reserved::default());
    let mappings = [
        Mapping::new("dir", "/busy", Access::ReadWrite).unwrap(),
        Mapping::new("dir/inner", "/busy/inner", Access::ReadOnly).unwrap(),
        Mapping::new("file.txt", "/deep/file", Access::ReadOnly).unwrap(),
    ];
    mapper.apply(&scene.source(), &mappings).unwrap();
    assert_eq!(mounts_below(&root).len(), 3);
    // An open file keeps its mapping busy, which the teardown reports and leaves.
    let held = fs::File::open(root.join("busy/marker")).unwrap();
    let error = mapper.teardown().unwrap_err();
    assert_eq!(
        error
            .failures()
            .iter()
            .map(UnmountFailure::target)
            .collect::<Vec<_>>(),
        [Path::new("/busy")]
    );
    assert_eq!(
        error.failures()[0].error(),
        &MappingError::System {
            step: Step::Unmount,
            error: Errno::EBUSY as i32
        }
    );
    assert_eq!(mounts_below(&root), [root.join("busy")]);
    drop(held);
    nix::mount::umount(&root.join("busy")).unwrap();
    // Without one, every mapping goes.
    let mut mapper = scene.mapper(Reserved::default());
    mapper.apply(&scene.source(), &mappings).unwrap();
    mapper.teardown().unwrap();
    assert!(mounts_below(&root).is_empty());
    assert!(root.join("deep/file").is_file());
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn a_thread_with_a_descriptor_table_of_its_own_tears_its_mappings_down() {
    // A thread that the test spawns, never the process's first one, whose descriptor table
    // `/proc/self/fd` would show instead of the thread's own.
    std::thread::spawn(|| {
        // Before the thread owns a descriptor, so that each of the mapper's is in the thread's
        // own table, which copies none of the other tests'.
        sys::unshare_standard_descriptors().unwrap();
        let Some(scene) = Scene::new() else {
            return;
        };
        let root = scene.path("root");
        let mut mapper = scene.mapper(Reserved::default());
        let mappings = [
            Mapping::new("dir", "/a/b", Access::ReadOnly).unwrap(),
            Mapping::new("file.txt", "/c", Access::ReadOnly).unwrap(),
        ];
        mapper.apply(&scene.source(), &mappings).unwrap();
        assert_eq!(mounts_below(&root).len(), 2);
        mapper.teardown().unwrap();
        assert!(mounts_below(&root).is_empty());
    })
    .join()
    .unwrap();
}
