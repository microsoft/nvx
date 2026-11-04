//! Tests of [`Root::mount_runtime`] and [`Root::switch`] with the kernel itself, which need the
//! privilege to mount and Linux 6.8 or later, for the unique mount IDs. Each runs in a private
//! mount namespace of its own thread, on a tmpfs root below a temporary directory, and they are
//! ignored unless asked for: `cargo test --all-features -- --ignored
//! guest::root::tests::privileged`, as root.
//!
//! They cover what the fake kernel cannot: the runtime's file systems and their options, the
//! device nodes, with their mode whatever the umask, and the links, the time status's read-only
//! bind, the undo of a call that fails part of the way, a second call's refusal, the teardown's
//! unmounts, and the order of the runtime and the mappings beneath `/run`: a call after a mapping
//! is refused and leaves it reachable, and a mapping after the runtime is on the `/run` tmpfs.
//! They also cover a switch into the root, in a thread with root and working directories of its
//! own, after which the root's mounts stay and descriptors opened before it still map.

use std::os::unix::fs::FileTypeExt;

use super::*;
use crate::guest::mappings::Reserved;

/// The options of a mount that the tests check, as the kernel's mount table shows them.
const CHECKED: [&str; 5] = ["ro", "rw", "nosuid", "nodev", "noexec"];

/// A root, a tmpfs below a temporary directory, and the directory of a time status beside it, in
/// a private mount namespace of the calling thread.
struct Scene {
    /// The temporary directory's canonical path.
    base: PathBuf,
    /// The temporary directory, which the scene removes once it has detached its mounts.
    directory: Option<tempfile::TempDir>,
}

impl Scene {
    /// Returns a scene, or `None` where the tests cannot run: without root, or without unique
    /// mount IDs.
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
        let root = scene.path("root");
        fs::create_dir(&root).unwrap();
        let point = sys::path_c_string(&root).unwrap();
        let options = Some(c"mode=0755");
        sys::mount(
            Some(c"tmpfs"),
            &point,
            Some(c"tmpfs"),
            MountFlags::empty(),
            options,
        )
        .unwrap();
        fs::create_dir(scene.path("time")).unwrap();
        fs::write(scene.path("time/state"), b"synchronized").unwrap();
        let flags = OpenFlags::O_PATH | OpenFlags::O_CLOEXEC;
        match sys::file_status(&sys::open(&point, flags, Mode::empty()).unwrap()) {
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

    /// Returns a root of the kernel itself at the scene's tmpfs, which it does not own.
    fn root(&self) -> Root {
        Root {
            kernel: Box::new(SystemKernel),
            runtime: self.base.clone(),
            path: self.path("root"),
            overlay: None,
            lowers: Vec::new(),
            upper: None,
            upper_kind: UpperKind::Tmpfs,
            overlay_mode: OverlayMode::TrustedLowers,
            first_partition: None,
            shares: Vec::new(),
            runtime_stray: None,
            mapper: None,
            mapped: Vec::new(),
            latched: false,
            switched: false,
            reservation: None,
        }
    }
}

impl Drop for Scene {
    /// Detaches the scene's root, with what is mounted below it, then removes the directory.
    fn drop(&mut self) {
        let _ = nix::mount::umount2(&self.path("root"), nix::mount::MntFlags::MNT_DETACH);
        drop(self.directory.take());
    }
}

/// Writes `message` to the standard error, past the test harness's capture.
fn report(message: &str) {
    use std::io::Write;

    let _ = writeln!(std::io::stderr(), "{message}");
}

/// Returns the lines of the thread's mount table, each split into its fields.
fn mount_table() -> Vec<Vec<String>> {
    let table = fs::read_to_string("/proc/thread-self/mountinfo").unwrap();
    let fields = |line: &str| line.split(' ').map(str::to_owned).collect();
    table.lines().map(fields).collect()
}

/// Returns the file system type of the topmost mount at `path`, and those of its options that
/// the tests check, sorted, if anything is mounted there.
fn shown(path: &Path) -> Option<(String, Vec<String>)> {
    let table = mount_table();
    let fields = table
        .iter()
        .rev()
        .find(|fields| Path::new(&fields[4]) == path)?;
    let kind = fields
        .iter()
        .skip_while(|field| *field != "-")
        .nth(1)
        .unwrap();
    let mut options: Vec<String> = fields[5]
        .split(',')
        .filter(|option| CHECKED.contains(option))
        .map(str::to_owned)
        .collect();
    options.sort();
    Some((kind.clone(), options))
}

/// Returns what [`shown`] returns for `path`, where something must be mounted.
fn mounted(path: &Path) -> (String, Vec<String>) {
    shown(path).unwrap_or_else(|| panic!("nothing is mounted at {}", path.display()))
}

/// Returns the root of the mappings' sources beside the scene's root, `source`, with the
/// directories `app` and `state`, each of which holds a file, `data`.
fn sources(scene: &Scene) -> SourceRoot {
    for (name, data) in [("app", "mapped"), ("state", "kept")] {
        let directory = scene.path("source").join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("data"), data).unwrap();
    }
    SourceRoot::open(&scene.path("source")).unwrap()
}

/// Returns the mount points below `path`, not `path` itself, in the thread's mount namespace.
fn mounts_below(path: &Path) -> Vec<PathBuf> {
    let points = mount_table()
        .into_iter()
        .map(|fields| PathBuf::from(&fields[4]));
    points
        .filter(|point| point.starts_with(path) && point != path)
        .collect()
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn the_runtime_s_file_systems_have_an_execution_s_options_nodes_and_links() {
    let Some(scene) = Scene::new() else {
        return;
    };
    let base = scene.path("root");
    let path = |relative: &str| base.join(relative);
    let mut root = scene.root();
    // The umask is the thread's alone in its mount namespace.
    let umask = nix::sys::stat::umask(Mode::from_bits_truncate(0o077));
    let made = root.mount_runtime(Some(&scene.path("time")));
    nix::sys::stat::umask(umask);
    made.unwrap();

    // The root's own mount is `nosuid` and `nodev` now, and each file system has an execution's
    // options.
    assert_eq!(mounted(&base).1, ["nodev", "nosuid", "rw"]);
    for (relative, kind, options) in [
        ("proc", "proc", &["nodev", "noexec", "nosuid", "rw"][..]),
        ("sys", "sysfs", &["nodev", "noexec", "nosuid", "ro"]),
        ("dev", "tmpfs", &["nosuid", "rw"]),
        ("dev/pts", "devpts", &["noexec", "nosuid", "rw"]),
        ("dev/shm", "tmpfs", &["nodev", "nosuid", "rw"]),
        ("run", "tmpfs", &["nodev", "nosuid", "rw"]),
    ] {
        let (found, found_options) = mounted(&path(relative));
        assert_eq!(found, kind, "{relative}");
        assert_eq!(found_options, options, "{relative}");
    }
    for (relative, mode) in [
        ("dev", 0o755),
        ("dev/shm", 0o1777),
        ("run", 0o755),
        ("run/nvx", 0o755),
    ] {
        let found = fs::metadata(path(relative)).unwrap().mode() & 0o7777;
        assert_eq!(found, mode, "{relative}");
    }

    // The device nodes have mode 0666, whatever the umask, and open on the `/dev` tmpfs.
    for (name, major, minor) in [
        ("null", 1, 3),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
        ("tty", 5, 0),
    ] {
        let node = fs::symlink_metadata(path(&format!("dev/{name}"))).unwrap();
        assert!(node.file_type().is_char_device(), "{name}");
        assert_eq!(node.mode() & 0o7777, 0o666, "{name}");
        assert_eq!(node.rdev(), nix::sys::stat::makedev(major, minor), "{name}");
    }
    fs::write(path("dev/null"), b"discarded").unwrap();
    for (name, target) in [
        ("ptmx", "pts/ptmx"),
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        let link = fs::read_link(path(&format!("dev/{name}"))).unwrap();
        assert_eq!(link, Path::new(target), "{name}");
    }

    // The time status is bound read-only, `nosuid`, and `nodev`.
    let time = path("run/nvx/time");
    assert_eq!(mounted(&time).1, ["nodev", "nosuid", "ro"]);
    assert_eq!(fs::read(time.join("state")).unwrap(), b"synchronized");
    let written = fs::write(time.join("written"), b"");
    assert_eq!(
        written.unwrap_err().raw_os_error(),
        Some(Errno::EROFS as i32)
    );

    // The teardown unmounts them all.
    root.teardown().unwrap();
    assert_eq!(mounts_below(&base), Vec::<PathBuf>::new());
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn a_call_that_fails_part_of_the_way_unmounts_what_it_mounted() {
    let Some(scene) = Scene::new() else {
        return;
    };
    let base = scene.path("root");
    // `/run` is a file, which no mount point may be, so its mount fails after `/dev/shm`'s.
    fs::write(base.join("run"), b"").unwrap();
    let mut root = scene.root();
    let error = root.mount_runtime(None).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::MountPoint { path, error }
                if *path == base.join("run")
                    && error.raw_os_error() == Some(Errno::ENOTDIR as i32)
        ),
        "{error:?}"
    );
    assert_eq!(mounts_below(&base), Vec::<PathBuf>::new());

    // Once it is a directory, a call mounts them all, and a second changes nothing: the mounts
    // keep their options, and the device nodes still open.
    fs::remove_file(base.join("run")).unwrap();
    root.mount_runtime(None).unwrap();
    let mounts = mounts_below(&base);
    assert_eq!(mounts.len(), 6, "{mounts:?}");
    let options: Vec<_> = mounts.iter().map(|mount| mounted(mount)).collect();
    let error = root.mount_runtime(None).unwrap_err();
    assert!(
        matches!(&error, Error::Occupied { path } if *path == base.join("proc")),
        "{error:?}"
    );
    assert_eq!(mounts_below(&base), mounts);
    let kept: Vec<_> = mounts.iter().map(|mount| mounted(mount)).collect();
    let written = fs::write(base.join("dev/null"), b"discarded").map_err(|error| error.kind());
    assert_eq!((kept, written), (options, Ok(())));
    root.teardown().unwrap();
    assert_eq!(mounts_below(&base), Vec::<PathBuf>::new());
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn a_call_after_a_mapping_is_refused_and_leaves_it_reachable() {
    let Some(scene) = Scene::new() else {
        return;
    };
    let base = scene.path("root");
    let app = base.join("run/app");
    let mut root = scene.root();
    let mapping = Mapping::new("app", "/run/app", Access::ReadOnly).unwrap();
    root.map(&sources(&scene), &[mapping]).unwrap();
    // The `/run` tmpfs would hide the mapping, which the teardown could then not unmount. Every
    // step runs before the one check, and the scene detaches what a failed one leaves.
    let first = root.mount_runtime(Some(&scene.path("time")));
    let read = fs::read(app.join("data")).map_err(|error| error.kind());
    let mounts = mounts_below(&base);
    let second = root.mount_runtime(None);
    let torn = root.teardown().map_err(|error| error.to_string());
    let refused = Err(format!(
        "a file system is mounted at {} already",
        app.display()
    ));
    assert_eq!(
        (
            first.map_err(|error| error.to_string()),
            read,
            mounts,
            second.map_err(|error| error.to_string()),
            torn,
            mounts_below(&base),
        ),
        (
            refused.clone(),
            Ok(b"mapped".to_vec()),
            vec![app.clone()],
            refused,
            Ok(()),
            Vec::new(),
        )
    );
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn mappings_beneath_run_after_the_runtime_are_on_its_tmpfs_and_go_with_it() {
    let Some(scene) = Scene::new() else {
        return;
    };
    let base = scene.path("root");
    let path = |relative: &str| base.join(relative);
    let mut root = scene.root();
    root.mount_runtime(Some(&scene.path("time"))).unwrap();
    let mappings = [
        Mapping::new("app", "/run/app", Access::ReadOnly).unwrap(),
        Mapping::new("state", "/run/nvx/state", Access::ReadOnly).unwrap(),
    ];
    // Every step runs before the one check, and the scene detaches what a failed one leaves.
    let mapped = root.map(&sources(&scene), &mappings);
    let options = ["run/app", "run/nvx/state"]
        .map(|relative| shown(&path(relative)).map(|(_, options)| options));
    let read = ["run/app/data", "run/nvx/state/data", "run/nvx/time/state"]
        .map(|relative| fs::read(path(relative)).map_err(|error| error.kind()));
    let mounts = mounts_below(&base);
    let second = root.mount_runtime(None);
    let kept = mounts_below(&base) == mounts;
    let torn = root.teardown().map_err(|error| error.to_string());
    // The mount points that the mapper created went with the `/run` tmpfs.
    let created = path("run/app").exists();
    let restricted = Some(["nodev", "nosuid", "ro"].map(str::to_owned).to_vec());
    assert_eq!(
        (
            mapped.map_err(|error| error.to_string()),
            options,
            read,
            second.map_err(|error| error.to_string()),
            kept,
            torn,
            mounts_below(&base),
            created,
        ),
        (
            Ok(()),
            [restricted.clone(), restricted],
            [
                Ok(b"mapped".to_vec()),
                Ok(b"kept".to_vec()),
                Ok(b"synchronized".to_vec()),
            ],
            Err(format!(
                "a file system is mounted at {} already",
                path("proc").display()
            )),
            true,
            Ok(()),
            Vec::new(),
            false,
        )
    );
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn a_switched_root_keeps_its_mounts_and_maps_through_earlier_descriptors() {
    let Some(scene) = Scene::new() else {
        return;
    };
    let base = scene.path("root");
    fs::write(base.join("marker"), b"the root").unwrap();
    let source = scene.path("source");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("early")).unwrap();
    fs::write(source.join("early/data"), b"early").unwrap();
    fs::write(source.join("late"), b"late").unwrap();
    let mut root = scene.root();
    root.mount_runtime(None).unwrap();
    // The root's own mapper, a target root, and a source root are opened before the switch.
    let sources = SourceRoot::open(&source).unwrap();
    let early = Mapping::new("early", "/srv/early", Access::ReadOnly).unwrap();
    root.map(&sources, &[early]).unwrap();
    let target = TargetRoot::open(&base).unwrap();

    // The thread that switches has root and working directories of its own, so the test's
    // thread still reaches the temporary directory afterwards.
    std::thread::scope(|scope| {
        scope.spawn(move || {
            nix::sched::unshare(nix::sched::CloneFlags::CLONE_FS).unwrap();
            root.switch().unwrap();
            assert_eq!(std::env::current_dir().unwrap(), Path::new("/"));
            assert_eq!(fs::read("/marker").unwrap(), b"the root");
            assert!(!source.exists());
            assert_eq!(fs::read("/srv/early/data").unwrap(), b"early");

            // A mapper maps beneath the target root, from the source root, which no path
            // reaches any more.
            let mut mapper = Mapper::new(target, Reserved::container_root());
            let late = Mapping::new("late", "/srv/late", Access::ReadOnly).unwrap();
            mapper.apply(&sources, &[late]).unwrap();
            assert_eq!(fs::read("/srv/late").unwrap(), b"late");

            // A second switch and the teardown are refused, and the drop that follows unmounts
            // nothing: the runtime's mounts and the root's mapping stay.
            let mounts = mounts_below(Path::new("/"));
            for point in ["/proc", "/dev/pts", "/run", "/srv/early", "/srv/late"] {
                assert!(
                    mounts.contains(&PathBuf::from(point)),
                    "{point}: {mounts:?}"
                );
            }
            let error = root.switch().unwrap_err();
            assert!(matches!(error, SwitchError::Switched), "{error:?}");
            assert!(root.teardown().unwrap_err().is_switched());
            assert_eq!(mounts_below(Path::new("/")), mounts);
            assert_eq!(fs::read("/srv/early/data").unwrap(), b"early");
        });
    });
}
