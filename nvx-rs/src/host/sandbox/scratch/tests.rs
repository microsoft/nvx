//! Tests of scratch bindings: binds and checks, which read no content, and the claims of checks.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::SystemTime;

use super::*;
use crate::ErrorKind;

/// The content of the staged scratch images.
const CONTENT: &[u8] = b"scratch image\n";

/// The access of a host that lets binds and checks open the image for writing alone, so that no
/// read or mapping of its content can succeed through what they open: on Linux an open for writing
/// alone, and on Windows the access of binds and checks, which never ask to read.
#[cfg(target_os = "linux")]
const WRITE_ONLY: Access<'static> = Access {
    open: &open_write_only,
};
/// The access of a host that lets binds and checks open the image for writing alone, so that no
/// read or mapping of its content can succeed through what they open: on Linux an open for writing
/// alone, and on Windows the access of binds and checks, which never ask to read.
#[cfg(windows)]
const WRITE_ONLY: Access<'static> = Access::WRITABLE;

/// Opens `path` for writing alone, without waiting for a reader of a FIFO.
#[cfg(target_os = "linux")]
fn open_write_only(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
        .open(path)
}

/// A scratch image staged in a temporary directory.
struct Staged {
    /// The temporary directory.
    directory: tempfile::TempDir,
}

impl Staged {
    /// Stages a scratch image.
    fn new() -> Self {
        let staged = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        fs::write(staged.image(), CONTENT).unwrap();
        staged
    }

    /// Returns the path of the image.
    fn image(&self) -> PathBuf {
        self.directory.path().join("scratch.ext4")
    }

    /// Returns the path of another file of the directory.
    fn other(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    /// Returns the path of the image, normalized.
    fn normalized(&self) -> PathBuf {
        paths::normalized(&self.image())
    }

    /// Binds the image through [`WRITE_ONLY`].
    fn bind(&self) -> ScratchBinding {
        bind_with(&self.normalized(), WRITE_ONLY).unwrap()
    }

    /// Checks the image against `binding` through [`WRITE_ONLY`], until `deadline`.
    fn check(&self, binding: &ScratchBinding, deadline: Instant) -> Result<CheckedScratch> {
        binding.check_with(&self.normalized(), deadline, WRITE_ONLY)
    }

    /// Checks the image against `binding` through [`WRITE_ONLY`], and returns it claimed. Like a
    /// launch, it waits [`CLAIM_RELEASE_WAIT`] for a claim that was released: on Linux, a process
    /// that another test spawns holds a copy of the claim until it executes its program.
    fn claim(&self, binding: &ScratchBinding) -> CheckedScratch {
        self.check(binding, Instant::now() + CLAIM_RELEASE_WAIT)
            .unwrap()
    }
}

/// Returns `path` as messages show it.
fn shown(path: &Path) -> String {
    paths::normalized(path).to_str().unwrap().to_owned()
}

/// Returns the kind and the message of the error of `result`.
fn failure<T: fmt::Debug>(result: Result<T>) -> (ErrorKind, String) {
    let error = result.unwrap_err();
    (error.kind(), error.to_string())
}

/// Returns the failure of a check of the image at `path` that another sandbox claims.
fn claimed_failure(path: &Path) -> (ErrorKind, String) {
    (
        ErrorKind::TimedOut,
        format!(
            "another sandbox claims the ext4 scratch image {}",
            shown(path)
        ),
    )
}

/// Returns the failure of a check of the image at `path`, whose `field` changed.
fn changed_failure(path: &Path, field: &str) -> (ErrorKind, String) {
    (
        ErrorKind::Unavailable,
        format!(
            "{} changed after its scratch binding was made: its {field} differs",
            shown(path)
        ),
    )
}

/// Returns whether processes that this one launches inherit `file`.
#[cfg(target_os = "linux")]
fn inheritable(file: &File) -> bool {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};

    let flags = fcntl(file, FcntlArg::F_GETFD).unwrap();
    !FdFlag::from_bits_truncate(flags).contains(FdFlag::FD_CLOEXEC)
}

/// Returns whether processes that this one launches inherit `file`.
#[cfg(windows)]
#[allow(unsafe_code)]
fn inheritable(file: &File) -> bool {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};

    let mut flags = 0;
    // SAFETY: the handle is open for as long as `file` is borrowed, and the output pointer is
    // valid.
    assert_ne!(
        unsafe { GetHandleInformation(file.as_raw_handle() as HANDLE, &mut flags) },
        0
    );
    flags & HANDLE_FLAG_INHERIT != 0
}

#[test]
fn binds_record_the_image_without_reading_it() {
    let staged = Staged::new();
    let binding = staged.bind();
    let seal = FileSeal::of(&File::open(staged.image()).unwrap()).unwrap();
    assert_eq!(binding.scheme(), seal.scheme());
    assert_eq!(
        (binding.device(), binding.file_id()),
        (seal.device(), seal.file_id())
    );
    assert_eq!(binding.kind(), FileKind::Regular);
    assert_eq!(binding.length(), CONTENT.len() as u64);
    assert_eq!(ScratchBinding::bind(&staged.image()).unwrap(), binding);
    // The positive control: what the access of the test opens refuses every read.
    let mut byte = [0];
    let mut opened = (WRITE_ONLY.open)(&staged.image()).unwrap();
    assert!(opened.read(&mut byte).is_err());
}

#[test]
fn bindings_round_trip_through_their_serde_form() {
    let staged = Staged::new();
    let binding = staged.bind();
    let value = serde_json::to_value(&binding).unwrap();
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["device", "fileId", "kind", "length", "scheme", "version"]
    );
    assert_eq!(value["version"], SCRATCH_BINDING_VERSION);
    assert_eq!(
        value["scheme"],
        if cfg!(windows) { "windows" } else { "linux" }
    );
    assert_eq!(value["fileId"], format!("{:032x}", binding.file_id()));
    assert_eq!(value["kind"], "regular");
    assert_eq!(value["length"], CONTENT.len());
    assert_eq!(
        serde_json::from_value::<ScratchBinding>(value.clone()).unwrap(),
        binding
    );
    // Every field is required, and no other is accepted, such as a time, which the VMM changes.
    for key in &keys {
        let mut partial = value.clone();
        partial.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<ScratchBinding>(partial).is_err(),
            "{key}"
        );
    }
    let mut extended = value.clone();
    extended["modified"] = serde_json::json!({"seconds": 0, "nanoseconds": 0});
    assert!(serde_json::from_value::<ScratchBinding>(extended).is_err());
}

#[test]
fn binds_refuse_what_is_not_a_scratch_image() {
    let staged = Staged::new();
    let not_found = |path: &Path| {
        (
            ErrorKind::Unavailable,
            format!("ext4 scratch image not found: {}", shown(path)),
        )
    };
    let missing = staged.other("missing.ext4");
    assert_eq!(failure(ScratchBinding::bind(&missing)), not_found(&missing));
    let directory = staged.other("directory.ext4");
    fs::create_dir(&directory).unwrap();
    assert_eq!(
        failure(ScratchBinding::bind(&directory)),
        not_found(&directory)
    );
    // No plan names a path that is not Unicode.
    #[cfg(target_os = "linux")]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = staged
            .other("")
            .join(OsStr::from_bytes(b"scratch-\xff.ext4"));
        for (kind, message) in [
            failure(ScratchBinding::bind(&path)),
            failure(staged.bind().check(&path, Instant::now())),
        ] {
            assert_eq!(kind, ErrorKind::Unsupported);
            assert!(
                message.starts_with("ext4 scratch image path is not valid Unicode: "),
                "{message}"
            );
        }
    }
}

#[test]
fn checks_claim_the_image_without_reading_it() {
    let staged = Staged::new();
    let binding = staged.bind();
    let checked = staged.check(&binding, Instant::now()).unwrap();
    assert_eq!(checked.path().to_str().unwrap(), shown(&staged.image()));
    let seal = FileSeal::of(checked.file()).unwrap();
    assert_eq!(
        (seal.device(), seal.file_id()),
        (binding.device(), binding.file_id())
    );
    // What the check holds reads nothing, and processes that this one launches do not inherit
    // it.
    let mut byte = [0];
    assert!(checked.file().read(&mut byte).is_err());
    assert!(!inheritable(checked.file()));
    #[cfg(windows)]
    {
        assert!(checked.claim().read(&mut byte).is_err());
        assert!(!inheritable(checked.claim()));
    }

    // A second check finds the image claimed until its deadline...
    assert_eq!(
        failure(staged.check(&binding, Instant::now())),
        claimed_failure(&staged.image())
    );
    // ...and claims it once the first check's claim is released before then.
    let started = Instant::now();
    let release = Duration::from_millis(200);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(release);
        drop(checked);
    });
    let second = staged
        .check(&binding, started + Duration::from_secs(30))
        .unwrap();
    assert!(started.elapsed() >= release);
    releaser.join().unwrap();
    assert_eq!(
        failure(staged.check(&binding, Instant::now())),
        claimed_failure(&staged.image())
    );
    drop(second);
    // The access of launches claims as the test's does.
    let checked = binding
        .check(&staged.image(), Instant::now() + CLAIM_RELEASE_WAIT)
        .unwrap();
    assert_eq!(
        failure(staged.check(&binding, Instant::now())),
        claimed_failure(&staged.image())
    );
    drop(checked);
    drop(staged.claim(&binding));
}

/// Returns the physical block size that OpenVMM presents for the file at `path`: the I/O block
/// size of its filesystem on Linux, and 4096 bytes on Windows.
fn physical_block_size(path: &Path) -> u32 {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;

        u32::try_from(fs::metadata(path).unwrap().blksize()).unwrap()
    }
    #[cfg(windows)]
    {
        let _ = path;
        4096
    }
}

#[test]
fn checked_images_present_the_geometry_that_openvmm_reads_through_them() {
    let staged = Staged::new();
    let resize = |length: u64| {
        OpenOptions::new()
            .write(true)
            .open(staged.image())
            .unwrap()
            .set_len(length)
            .unwrap();
    };
    resize(1 << 20);
    let binding = staged.bind();
    let checked = staged.claim(&binding);
    // It reads the metadata of what the check holds, which reads nothing, while the claim lasts.
    assert_eq!(
        checked.geometry().unwrap(),
        BlockGeometry {
            length: 1 << 20,
            logical_block_size: 512,
            physical_block_size: physical_block_size(&staged.image()),
        }
    );
    assert_eq!(
        failure(staged.check(&binding, Instant::now())),
        claimed_failure(&staged.image())
    );
    drop(checked);
    // OpenVMM attaches block devices of nonzero multiples of 512 bytes alone.
    for length in [0, CONTENT.len() as u64, (1 << 20) + 1] {
        resize(length);
        let binding = staged.bind();
        assert_eq!(
            failure(staged.claim(&binding).geometry()),
            (
                ErrorKind::Unsupported,
                format!(
                    "the ext4 scratch image {} of {length} bytes cannot be attached: OpenVMM \
                     attaches block devices of nonzero multiples of 512 bytes",
                    shown(&staged.image())
                )
            )
        );
    }
}

#[test]
fn a_claim_lasts_while_the_vmm_holds_the_image() {
    let staged = Staged::new();
    let binding = staged.bind();
    let checked = binding
        .check(&staged.image(), Instant::now() + CLAIM_RELEASE_WAIT)
        .unwrap();
    // The VMM's hold of the image: on Linux the descriptor that it inherits, a copy of the
    // checked one, and on Windows its own open for reading and writing, which the guard shares.
    #[cfg(target_os = "linux")]
    let mut vmm = checked.file().try_clone().unwrap();
    #[cfg(windows)]
    let mut vmm = OpenOptions::new()
        .read(true)
        .write(true)
        .open(staged.image())
        .unwrap();
    vmm.write_all(b"S").unwrap();
    // The caller releases what it holds once the VMM is ready: the VMM keeps the claim.
    drop(checked);
    assert_eq!(
        failure(staged.check(&binding, Instant::now())),
        claimed_failure(&staged.image())
    );
    drop(vmm);
    drop(staged.claim(&binding));
}

#[cfg(windows)]
#[test]
fn the_claims_handle_keeps_the_claim_once_the_guard_is_closed() {
    let staged = Staged::new();
    let binding = staged.bind();
    let CheckedScratch {
        file: guard, claim, ..
    } = staged.check(&binding, Instant::now()).unwrap();
    // The caller ends before the VMM, which inherited the claim's handle, has opened the image,
    // which closes the guard: the VMM's copy of the handle keeps the claim.
    drop(guard);
    assert_eq!(
        failure(staged.check(&binding, Instant::now())),
        claimed_failure(&staged.image())
    );
    // Without the guard, nothing keeps the image from being renamed, as after authenticated Ready.
    let moved = staged.other("moved.ext4");
    fs::rename(staged.image(), &moved).unwrap();
    fs::rename(&moved, staged.image()).unwrap();
    drop(claim);
    drop(staged.claim(&binding));
}

#[test]
fn a_check_that_waited_for_a_claim_compares_the_image_again() {
    // Another launch holds the image: on Linux the descriptor of its check, which holds the claim,
    // and on Windows, as its VMM does once its guard is gone, an open for reading and writing.
    let staged = Staged::new();
    let binding = staged.bind();
    #[cfg(target_os = "linux")]
    let holder = staged.check(&binding, Instant::now()).unwrap().into_file();
    #[cfg(windows)]
    let holder = OpenOptions::new()
        .read(true)
        .write(true)
        .open(staged.image())
        .unwrap();
    let waiting = {
        let (binding, path) = (binding.clone(), staged.normalized());
        std::thread::spawn(move || {
            binding.check_with(&path, Instant::now() + Duration::from_secs(30), WRITE_ONLY)
        })
    };
    // Once the check has compared the image and waits, the holder resizes the image and lets it
    // go: the check fails rather than return the image that changed.
    std::thread::sleep(Duration::from_millis(300));
    holder.set_len(CONTENT.len() as u64 + 1).unwrap();
    drop(holder);
    assert_eq!(
        failure(waiting.join().unwrap()),
        changed_failure(&staged.image(), "length")
    );
    // The failed check holds no claim.
    drop(staged.claim(&staged.bind()));
}

#[test]
fn the_checked_image_is_the_object_that_the_check_compared() {
    let staged = Staged::new();
    let binding = staged.bind();
    let checked = staged.check(&binding, Instant::now()).unwrap();
    let replacement = staged.other("replacement.ext4");
    fs::write(&replacement, CONTENT).unwrap();
    // On Windows, the guard keeps anything from removing, renaming, or replacing the image while
    // it is held, and shares OpenVMM's open of the image for reading and writing.
    #[cfg(windows)]
    {
        assert!(fs::remove_file(staged.image()).is_err());
        assert!(fs::rename(staged.image(), staged.other("moved.ext4")).is_err());
        assert!(fs::rename(&replacement, staged.image()).is_err());
        let mut vmm = OpenOptions::new()
            .read(true)
            .write(true)
            .open(staged.image())
            .unwrap();
        let mut content = Vec::new();
        vmm.read_to_end(&mut content).unwrap();
        assert_eq!(content, CONTENT);
        vmm.write_all(b"!").unwrap();
    }
    #[cfg(target_os = "linux")]
    fs::rename(&replacement, staged.image()).unwrap();

    // What the caller holds is still the object that the check compared, whatever has its path
    // now.
    let seal = FileSeal::of(checked.file()).unwrap();
    assert_eq!(
        (seal.device(), seal.file_id(), seal.kind()),
        (binding.device(), binding.file_id(), FileKind::Regular)
    );
    let file = checked.into_file();
    assert!(!inheritable(&file));
    drop(file);

    #[cfg(windows)]
    fs::rename(&replacement, staged.image()).unwrap();
    assert_eq!(
        failure(staged.check(&binding, Instant::now())),
        changed_failure(&staged.image(), "identity")
    );
}

/// Stages a scratch image, binds it, applies `change`, and returns the failure of the check on a
/// host where reads of content fail, with the staged image.
fn stale_after(change: impl FnOnce(&Staged)) -> ((ErrorKind, String), Staged) {
    let staged = Staged::new();
    let binding = staged.bind();
    change(&staged);
    (failure(staged.check(&binding, Instant::now())), staged)
}

#[test]
fn checks_fail_without_reading_when_the_image_changes() {
    // Another file in its place, even with the same content, has another identity.
    let (failed, staged) = stale_after(|staged| {
        let copy = staged.other("copy.ext4");
        fs::copy(staged.image(), &copy).unwrap();
        fs::rename(&copy, staged.image()).unwrap();
    });
    assert_eq!(failed, changed_failure(&staged.image(), "identity"));

    // So has a directory in its place, whose kind differs too. The image stays, under another
    // name, so that the directory cannot take its inode number, which is its file ID on Linux.
    let (failed, staged) = stale_after(|staged| {
        fs::rename(staged.image(), staged.other("moved.ext4")).unwrap();
        fs::create_dir(staged.image()).unwrap();
    });
    assert_eq!(failed, changed_failure(&staged.image(), "identity"));

    // A change of length changes the guest's block device.
    let (failed, staged) = stale_after(|staged| {
        let mut file = OpenOptions::new()
            .append(true)
            .open(staged.image())
            .unwrap();
        file.write_all(b"!").unwrap();
    });
    assert_eq!(failed, changed_failure(&staged.image(), "length"));

    // A binding of another kind, scheme, or version fails, as a binding that names another image
    // would.
    let staged = Staged::new();
    let value = serde_json::to_value(staged.bind()).unwrap();
    let edited = |field: &str, replacement: serde_json::Value| -> ScratchBinding {
        let mut value = value.clone();
        value[field] = replacement;
        serde_json::from_value(value).unwrap()
    };
    assert_eq!(
        failure(staged.check(&edited("kind", "directory".into()), Instant::now())),
        changed_failure(&staged.image(), "kind")
    );
    let other_scheme = if cfg!(windows) { "linux" } else { "windows" };
    assert_eq!(
        failure(staged.check(&edited("scheme", other_scheme.into()), Instant::now())),
        changed_failure(&staged.image(), "seal scheme")
    );
    assert_eq!(
        failure(staged.check(&edited("version", 2.into()), Instant::now())),
        (
            ErrorKind::Unavailable,
            format!(
                "the scratch binding of {} has version 2, which this build does not check",
                shown(&staged.image())
            )
        )
    );

    // A missing image is unavailable, as a launch's own check of its images finds it.
    let (failed, staged) = stale_after(|staged| fs::remove_file(staged.image()).unwrap());
    assert_eq!(
        failed,
        (
            ErrorKind::Unavailable,
            format!("ext4 scratch image not found: {}", shown(&staged.image()))
        )
    );
}

#[test]
fn checks_pass_after_writes_that_keep_the_length() {
    // The VMM writes the image and moves its times, which the binding does not record.
    let staged = Staged::new();
    let binding = staged.bind();
    let mut file = OpenOptions::new().write(true).open(staged.image()).unwrap();
    file.write_all(b"S").unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(60))
        .unwrap();
    drop(file);
    drop(staged.check(&binding, Instant::now()).unwrap());
}
