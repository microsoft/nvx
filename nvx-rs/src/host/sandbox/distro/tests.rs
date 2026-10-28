//! Tests of distro bindings: explicit verification, imports, and the staged check, which reads no
//! content.

use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use super::*;
use crate::ErrorKind;
use crate::host::paths;
use crate::host::sandbox::LayerRole;
use crate::host::seal::{open_metadata_only, open_read};

/// The UUID of the staged layers.
const UUID: &str = "11111111-1111-1111-1111-111111111111";

/// The access of a host that lets the binding open files and read their metadata, but fails
/// every read and mapping of their content.
const METADATA_ONLY: Access<'static> = Access {
    open: &open_metadata_only,
    seal: &FileSeal::of,
};

/// A distro layer staged in a temporary directory, with or without its manifest.
struct Staged {
    /// The temporary directory.
    directory: tempfile::TempDir,
}

impl Staged {
    /// Stages a layer, with a manifest that binds it and lists `bash` if `manifest` is true.
    fn new(manifest: bool) -> Self {
        let staged = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        fs::write(staged.image(), b"distro layer\n").unwrap();
        if manifest {
            staged.write_manifest("[{\"name\": \"bash\"}]");
        }
        staged
    }

    /// Returns the path of the layer.
    fn image(&self) -> PathBuf {
        self.directory.path().join("distro.erofs")
    }

    /// Returns the path of the layer's manifest.
    fn manifest(&self) -> PathBuf {
        self.directory.path().join("distro.erofs.manifest.json")
    }

    /// Returns the path of another file of the directory.
    fn other(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    /// Returns the distro layer, whose filesystem has `uuid`.
    fn layer_with(&self, uuid: &str) -> SandboxLayer {
        SandboxLayer::new(LayerRole::Distro, self.image().to_str().unwrap(), uuid)
    }

    /// Returns the distro layer.
    fn layer(&self) -> SandboxLayer {
        self.layer_with(UUID)
    }

    /// Writes a manifest that binds the layer as it is now and lists `packages`.
    fn write_manifest(&self, packages: &str) {
        let digest: String = Sha256::digest(fs::read(self.image()).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        fs::write(
            self.manifest(),
            format!(
                "{{\"format\": 1, \"artifact\": \"distro.erofs\", \"artifact_sha256\": \
                 \"{digest}\", \"packages\": {packages}}}"
            ),
        )
        .unwrap();
    }
}

/// Returns `path` as messages show it.
fn shown(path: &Path) -> String {
    paths::normalized(path).to_str().unwrap().to_owned()
}

/// Returns the seal of the file at `path`.
fn seal_of(path: &Path) -> FileSeal {
    FileSeal::of(&File::open(path).unwrap()).unwrap()
}

/// Returns the kind and the message of the error of `result`.
fn failure<T: fmt::Debug>(result: Result<T>) -> (ErrorKind, String) {
    let error = result.unwrap_err();
    (error.kind(), error.to_string())
}

/// Replaces the file at `path` with a copy that has the same content and modification time, as a
/// file that something rewrote by rename would be.
fn replace_with_copy(path: &Path, replacement: &Path) {
    fs::copy(path, replacement).unwrap();
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    OpenOptions::new()
        .write(true)
        .open(replacement)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    fs::rename(replacement, path).unwrap();
}

/// Moves the modification time of the file at `path` by a second.
fn touch(path: &Path) {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified + Duration::from_secs(1))
        .unwrap();
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
fn verification_binds_the_layer_its_manifest_and_the_policy() {
    for with_manifest in [true, false] {
        let staged = Staged::new(with_manifest);
        let binding = DistroBinding::verify(&staged.layer()).unwrap();
        assert_eq!(binding.format(), DistroFormat::Erofs);
        assert_eq!(binding.identity(), UUID);
        assert_eq!(
            binding.policy(),
            PolicyVerdict {
                revision: DISTRO_POLICY_REVISION,
                result: PolicyResult::Accepted,
            }
        );
        assert_eq!(binding.image(), &seal_of(&staged.image()));
        let manifest = with_manifest.then(|| seal_of(&staged.manifest()));
        assert_eq!(binding.manifest(), manifest.as_ref());
        binding
            .check(&staged.layer(), DISTRO_POLICY_REVISION)
            .unwrap();
    }
}

#[test]
fn bindings_round_trip_through_their_serde_form() {
    let staged = Staged::new(false);
    let binding = DistroBinding::verify(&staged.layer()).unwrap();
    let value = serde_json::to_value(&binding).unwrap();
    assert_eq!(value["version"], BINDING_VERSION);
    assert_eq!(value["format"], "erofs");
    assert_eq!(value["identity"], UUID);
    assert_eq!(
        value["image"],
        serde_json::to_value(binding.image()).unwrap()
    );
    // The absence of a manifest is explicit.
    assert_eq!(value.get("manifest"), Some(&serde_json::Value::Null));
    assert_eq!(
        value["policy"],
        serde_json::json!({"revision": DISTRO_POLICY_REVISION, "result": "accepted"})
    );
    assert_eq!(
        serde_json::from_value::<DistroBinding>(value.clone()).unwrap(),
        binding
    );
    for field in [
        "version", "format", "identity", "image", "manifest", "policy",
    ] {
        let mut partial = value.clone();
        partial.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<DistroBinding>(partial).is_err(),
            "{field}"
        );
    }
    let mut extended = value.clone();
    extended["path"] = "distro.erofs".into();
    assert!(serde_json::from_value::<DistroBinding>(extended).is_err());
    let mut rejected = value;
    rejected["policy"]["result"] = "rejected".into();
    assert_eq!(
        serde_json::from_value::<DistroBinding>(rejected)
            .unwrap()
            .policy()
            .result,
        PolicyResult::Rejected
    );
}

#[test]
fn the_staged_check_reads_no_content() {
    let staged = Staged::new(true);
    let layer = staged.layer();
    let binding = DistroBinding::verify(&layer).unwrap();
    assert!(binding.manifest().is_some());

    // On a host that lets the check open the layer and its manifest and read their metadata, but
    // fails every read and mapping of their content, the check passes, and returns the layer.
    let checked = binding
        .check_with(&layer, DISTRO_POLICY_REVISION, METADATA_ONLY)
        .unwrap();
    assert_eq!(&FileSeal::of(checked.file()).unwrap(), binding.image());
    let mut byte = [0];
    assert!(checked.into_file().read(&mut byte).is_err());

    // Verification, the positive control, reads both files there: the manifest first, which it
    // fails to read...
    let (kind, message) = failure(verify_with(&layer, METADATA_ONLY));
    assert_eq!(kind, ErrorKind::Unsupported);
    assert!(
        message.starts_with(&format!(
            "invalid sandbox distro manifest: {:?}: ",
            staged.manifest()
        )),
        "{message}"
    );
    // ...and then the whole layer, which it fails to hash.
    let manifest = staged.manifest();
    let open = |path: &Path| {
        if path == manifest {
            open_read(path)
        } else {
            open_metadata_only(path)
        }
    };
    let access = Access {
        open: &open,
        seal: &FileSeal::of,
    };
    let (kind, message) = failure(verify_with(&layer, access));
    assert_eq!(kind, ErrorKind::Io);
    assert!(
        message.starts_with(&format!("{:?}: ", shown(&staged.image()))),
        "{message}"
    );
    // With reads allowed, it hashes the layer and binds it, as the check found it.
    assert_eq!(verify_with(&layer, Access::READ).unwrap(), binding);
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
fn checked_layers_present_the_geometry_that_openvmm_reads_through_them() {
    let staged = Staged::new(false);
    let image = staged.image();
    let resize = |length: u64| {
        OpenOptions::new()
            .write(true)
            .open(&image)
            .unwrap()
            .set_len(length)
            .unwrap();
    };
    let check = |layer: &SandboxLayer| {
        import_with(layer, PolicyVerdict::ACCEPTED, METADATA_ONLY)
            .unwrap()
            .check_with(layer, DISTRO_POLICY_REVISION, METADATA_ONLY)
            .unwrap()
    };
    resize(8192);
    let checked = check(&staged.layer());
    let geometry = BlockGeometry {
        length: 8192,
        logical_block_size: 512,
        physical_block_size: physical_block_size(&image),
    };
    // It reads the metadata of the checked file alone, which reads nothing.
    assert_eq!(checked.geometry().unwrap(), geometry);
    // The geometry is the checked file's, whatever later takes its path.
    #[cfg(target_os = "linux")]
    {
        let replacement = staged.other("replacement.erofs");
        fs::write(&replacement, [0; 4096]).unwrap();
        fs::rename(&replacement, &image).unwrap();
        assert_eq!(checked.geometry().unwrap(), geometry);
    }
    drop(checked);
    // OpenVMM attaches block devices of nonzero multiples of 512 bytes alone.
    let gpt = SandboxLayer::gpt(LayerRole::Distro, image.to_str().unwrap(), UUID);
    for (layer, description) in [
        (staged.layer(), "distro EROFS layer"),
        (gpt, "distro GPT image"),
    ] {
        for length in [0, 8191] {
            resize(length);
            assert_eq!(
                failure(check(&layer).geometry()),
                (
                    ErrorKind::Unsupported,
                    format!(
                        "the {description} {} of {length} bytes cannot be attached: OpenVMM \
                         attaches block devices of nonzero multiples of 512 bytes",
                        shown(&image)
                    )
                )
            );
        }
    }
}

/// A layer on hugetlbfs, whose I/O block size is a huge page, presents physical blocks of 2 MiB.
#[cfg(target_os = "linux")]
#[test]
fn layers_present_the_block_size_of_their_filesystem() {
    use nix::sys::memfd::{MFdFlags, memfd_create};

    const HUGE_PAGE: u64 = 2 * 1024 * 1024;
    let flags = MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_HUGETLB | MFdFlags::MFD_HUGE_2MB;
    let huge = match memfd_create("nvx-geometry-test", flags) {
        Ok(descriptor) => File::from(descriptor),
        Err(error) => {
            eprintln!("this host offers no file on hugetlbfs: {error}");
            return;
        }
    };
    if let Err(error) = huge.set_len(HUGE_PAGE) {
        eprintln!("this host cannot size a file on hugetlbfs: {error}");
        return;
    }
    let staged = Staged::new(false);
    let image = staged.image();
    let open = |path: &Path| {
        if path == image {
            huge.try_clone()
        } else {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }
    };
    let access = Access {
        open: &open,
        seal: &FileSeal::of,
    };
    let checked = import_with(&staged.layer(), PolicyVerdict::ACCEPTED, access)
        .unwrap()
        .check_with(&staged.layer(), DISTRO_POLICY_REVISION, access)
        .unwrap();
    assert_eq!(
        checked.geometry().unwrap(),
        BlockGeometry {
            length: HUGE_PAGE,
            logical_block_size: 512,
            physical_block_size: HUGE_PAGE as u32,
        }
    );
}

/// Stages a layer, with a manifest if `manifest` is true, binds it by verification, applies
/// `change`, and returns the failure of the check on a host where reads of content fail, with the
/// staged layer.
fn stale_after(manifest: bool, change: impl FnOnce(&Staged)) -> (ErrorKind, String, Staged) {
    let staged = Staged::new(manifest);
    let binding = DistroBinding::verify(&staged.layer()).unwrap();
    change(&staged);
    let (kind, message) =
        failure(binding.check_with(&staged.layer(), DISTRO_POLICY_REVISION, METADATA_ONLY));
    (kind, message, staged)
}

#[test]
fn the_staged_check_fails_without_reading_when_the_layer_changes() {
    let changed = |staged: &Staged, field: &str| {
        (
            ErrorKind::Unavailable,
            format!(
                "{} changed after its distro binding was made: its {field} differs",
                shown(&staged.image())
            ),
        )
    };

    // Another file in its place, even with the same content and modification time, has another
    // identity.
    let (kind, message, staged) = stale_after(true, |staged| {
        replace_with_copy(&staged.image(), &staged.other("copy.erofs"));
    });
    assert_eq!((kind, message), changed(&staged, "identity"));

    // So has a directory in its place, whose kind differs too.
    let (kind, message, staged) = stale_after(false, |staged| {
        let directory = staged.other("directory");
        fs::create_dir(&directory).unwrap();
        fs::remove_file(staged.image()).unwrap();
        fs::rename(&directory, staged.image()).unwrap();
    });
    assert_eq!((kind, message), changed(&staged, "identity"));
    let staged = Staged::new(false);
    let mut value = serde_json::to_value(DistroBinding::verify(&staged.layer()).unwrap()).unwrap();
    value["image"]["kind"] = "directory".into();
    let binding: DistroBinding = serde_json::from_value(value).unwrap();
    let (kind, message) =
        failure(binding.check_with(&staged.layer(), DISTRO_POLICY_REVISION, METADATA_ONLY));
    assert_eq!((kind, message), changed(&staged, "kind"));

    // An append changes its length, even with its modification time set back.
    let (kind, message, staged) = stale_after(true, |staged| {
        let modified = fs::metadata(staged.image()).unwrap().modified().unwrap();
        let mut file = OpenOptions::new()
            .append(true)
            .open(staged.image())
            .unwrap();
        file.write_all(b"!").unwrap();
        file.set_modified(modified).unwrap();
    });
    assert_eq!((kind, message), changed(&staged, "length"));

    let (kind, message, staged) = stale_after(true, |staged| touch(&staged.image()));
    assert_eq!((kind, message), changed(&staged, "modification time"));

    // On Linux, a change of its metadata alone moves its change time, which nothing sets back.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;

        let (kind, message, staged) = stale_after(true, |staged| {
            // File times come from a clock that advances with the scheduler's tick.
            std::thread::sleep(Duration::from_millis(50));
            fs::set_permissions(staged.image(), fs::Permissions::from_mode(0o400)).unwrap();
        });
        assert_eq!((kind, message), changed(&staged, "change time"));
    }

    // A missing layer is unavailable, as a launch's own check of its layers finds it.
    let (kind, message, staged) =
        stale_after(false, |staged| fs::remove_file(staged.image()).unwrap());
    assert_eq!(
        (kind, message),
        (
            ErrorKind::Unavailable,
            format!("distro EROFS layer not found: {}", shown(&staged.image()))
        )
    );
}

#[test]
fn the_staged_check_fails_without_reading_when_the_manifest_changes() {
    let (kind, message, staged) = stale_after(false, |staged| staged.write_manifest("[]"));
    assert_eq!(
        (kind, message),
        (
            ErrorKind::Unavailable,
            format!(
                "{} appeared after the distro binding of {} was made",
                shown(&staged.manifest()),
                shown(&staged.image())
            )
        )
    );

    let (kind, message, staged) =
        stale_after(true, |staged| fs::remove_file(staged.manifest()).unwrap());
    assert_eq!(
        (kind, message),
        (
            ErrorKind::Unavailable,
            format!(
                "{} disappeared after the distro binding of {} was made",
                shown(&staged.manifest()),
                shown(&staged.image())
            )
        )
    );

    let changed = |staged: &Staged, field: &str| {
        (
            ErrorKind::Unavailable,
            format!(
                "{} changed after the distro binding of {} was made: its {field} differs",
                shown(&staged.manifest()),
                shown(&staged.image())
            ),
        )
    };
    let (kind, message, staged) = stale_after(true, |staged| {
        replace_with_copy(&staged.manifest(), &staged.other("copy.json"));
    });
    assert_eq!((kind, message), changed(&staged, "identity"));
    let (kind, message, staged) = stale_after(true, |staged| touch(&staged.manifest()));
    assert_eq!((kind, message), changed(&staged, "modification time"));
}

#[test]
fn the_staged_check_fails_without_reading_for_another_policy_or_layer() {
    let staged = Staged::new(true);
    let layer = staged.layer();
    let image = shown(&staged.image());
    let binding = DistroBinding::verify(&layer).unwrap();
    binding
        .check_with(&layer, DISTRO_POLICY_REVISION, METADATA_ONLY)
        .unwrap();

    // A new revision of the policy needs a new verification.
    assert_eq!(
        failure(binding.check_with(&layer, DISTRO_POLICY_REVISION + 1, METADATA_ONLY)),
        (
            ErrorKind::Unavailable,
            format!(
                "the distro binding of {image} holds a verdict of distro policy revision {}, not \
                 {}",
                DISTRO_POLICY_REVISION,
                DISTRO_POLICY_REVISION + 1
            )
        )
    );

    // A verdict that rejects the layer passes no check.
    let rejected = import_with(
        &layer,
        PolicyVerdict {
            revision: DISTRO_POLICY_REVISION,
            result: PolicyResult::Rejected,
        },
        METADATA_ONLY,
    )
    .unwrap();
    assert_eq!(
        failure(rejected.check_with(&layer, DISTRO_POLICY_REVISION, METADATA_ONLY)),
        (
            ErrorKind::Unsupported,
            format!("the distro binding of {image} holds a verdict that rejects the layer")
        )
    );

    // A launch that expects another filesystem.
    let other = "22222222-2222-2222-2222-222222222222";
    assert_eq!(
        failure(binding.check_with(
            &staged.layer_with(other),
            DISTRO_POLICY_REVISION,
            METADATA_ONLY
        )),
        (
            ErrorKind::Unavailable,
            format!(
                "the distro binding of {image} names the erofs layer with identity {UUID}, not \
                 {other}"
            )
        )
    );

    // A binding of another version.
    let mut value = serde_json::to_value(&binding).unwrap();
    value["version"] = 2.into();
    let newer: DistroBinding = serde_json::from_value(value).unwrap();
    assert_eq!(
        failure(newer.check_with(&layer, DISTRO_POLICY_REVISION, METADATA_ONLY)),
        (
            ErrorKind::Unavailable,
            format!("the distro binding of {image} has version 2, which this build does not check")
        )
    );
}

#[test]
fn verification_fails_if_a_file_changes_while_it_is_read() {
    // Verification seals the manifest before and after it reads it, and then the layer before and
    // after it hashes it. A change right after the first seal of a file is what a writer racing
    // with verification would make.
    for (file, seals) in [("manifest", 1), ("layer", 3)] {
        let staged = Staged::new(true);
        let path = if file == "layer" {
            staged.image()
        } else {
            staged.manifest()
        };
        let taken = Cell::new(0);
        let seal = |opened: &File| {
            let sealed = FileSeal::of(opened);
            taken.set(taken.get() + 1);
            if taken.get() == seals {
                let mut writer = OpenOptions::new().append(true).open(&path).unwrap();
                writer.write_all(b" ").unwrap();
            }
            sealed
        };
        let access = Access {
            open: &open_read,
            seal: &seal,
        };
        assert_eq!(
            failure(verify_with(&staged.layer(), access)),
            (
                ErrorKind::Io,
                format!(
                    "{} changed while it was verified: its length differs",
                    shown(&path)
                )
            ),
            "{file}"
        );
    }
}

#[test]
fn the_checked_file_is_the_object_that_the_check_compared() {
    let staged = Staged::new(true);
    let layer = staged.layer();
    let binding = DistroBinding::verify(&layer).unwrap();
    let checked = binding.check(&layer, DISTRO_POLICY_REVISION).unwrap();
    let replacement = staged.other("replacement.erofs");
    fs::write(&replacement, b"another layer\n").unwrap();
    // On Windows, the check's handle keeps anything from modifying, removing, or replacing the
    // layer while it is held.
    #[cfg(windows)]
    {
        assert!(
            OpenOptions::new()
                .append(true)
                .open(staged.image())
                .is_err()
        );
        assert!(fs::remove_file(staged.image()).is_err());
        assert!(fs::rename(&replacement, staged.image()).is_err());
    }
    #[cfg(target_os = "linux")]
    fs::rename(&replacement, staged.image()).unwrap();

    // The caller owns the file, which is still the object that the check compared, with its
    // content, whatever has its path now, and which processes that the caller launches do not
    // inherit. On Linux, the replacement unlinked the object, which may move its change time.
    let mut file = checked.into_file();
    let difference = binding.image().difference(&FileSeal::of(&file).unwrap());
    assert!(
        difference.is_none() || (cfg!(target_os = "linux") && difference == Some("change time")),
        "{difference:?}"
    );
    let mut content = Vec::new();
    file.read_to_end(&mut content).unwrap();
    assert_eq!(content, b"distro layer\n");
    assert!(!inheritable(&file));
    drop(file);

    #[cfg(windows)]
    fs::rename(&replacement, staged.image()).unwrap();
    let (kind, message) =
        failure(binding.check_with(&layer, DISTRO_POLICY_REVISION, METADATA_ONLY));
    assert_eq!(kind, ErrorKind::Unavailable);
    assert!(
        message.ends_with("changed after its distro binding was made: its identity differs"),
        "{message}"
    );
}

#[test]
fn imports_bind_a_staged_layer_without_reading_it() {
    let staged = Staged::new(true);
    let layer = staged.layer();
    // The staging caller vouches for the layer and its manifest, which the import does not read:
    // even a manifest that verification refuses.
    fs::write(staged.manifest(), b"not JSON").unwrap();
    let binding = import_with(&layer, PolicyVerdict::ACCEPTED, METADATA_ONLY).unwrap();
    assert_eq!(binding.image(), &seal_of(&staged.image()));
    assert_eq!(binding.manifest(), Some(&seal_of(&staged.manifest())));
    assert_eq!(binding.policy(), PolicyVerdict::ACCEPTED);
    assert_eq!(binding.identity(), UUID);
    binding.check(&layer, DISTRO_POLICY_REVISION).unwrap();

    // A verdict of the caller's own policy passes the checks of that policy's revision alone.
    let own = PolicyVerdict {
        revision: 7,
        result: PolicyResult::Accepted,
    };
    let binding = DistroBinding::import(&layer, own).unwrap();
    binding.check(&layer, 7).unwrap();
    assert_eq!(
        failure(binding.check(&layer, DISTRO_POLICY_REVISION)).0,
        ErrorKind::Unavailable
    );

    // Without a manifest, the import binds its absence.
    fs::remove_file(staged.manifest()).unwrap();
    let binding = DistroBinding::import(&layer, PolicyVerdict::ACCEPTED).unwrap();
    assert_eq!(binding.manifest(), None);
    binding.check(&layer, DISTRO_POLICY_REVISION).unwrap();

    // It binds regular files alone.
    fs::remove_file(staged.image()).unwrap();
    fs::create_dir(staged.image()).unwrap();
    assert_eq!(
        failure(DistroBinding::import(&layer, PolicyVerdict::ACCEPTED)),
        (
            ErrorKind::Unavailable,
            format!("distro EROFS layer not found: {}", shown(&staged.image()))
        )
    );
}

#[test]
fn gpt_disks_bind_by_their_disk_guid() {
    let staged = Staged::new(false);
    let image = shown(&staged.image());
    let gpt = SandboxLayer::gpt(LayerRole::Distro, staged.image().to_str().unwrap(), UUID);

    // Verification of a disk without a manifest reads nothing, and binds what an import binds.
    let binding = verify_with(&gpt, METADATA_ONLY).unwrap();
    assert_eq!(
        (binding.format(), binding.identity()),
        (DistroFormat::Gpt, UUID)
    );
    assert_eq!(
        import_with(&gpt, PolicyVerdict::ACCEPTED, METADATA_ONLY).unwrap(),
        binding
    );
    let value = serde_json::to_value(&binding).unwrap();
    assert_eq!(value["format"], "gpt");
    assert_eq!(
        serde_json::from_value::<DistroBinding>(value).unwrap(),
        binding
    );

    // The check returns the disk, with the format and disk GUID that the binding names.
    let checked = binding
        .check_with(&gpt, DISTRO_POLICY_REVISION, METADATA_ONLY)
        .unwrap();
    assert_eq!(checked.bound(), (DistroFormat::Gpt, UUID));
    assert_eq!(&FileSeal::of(checked.file()).unwrap(), binding.image());

    // A binding of one format checks no image of the other, even at its path and identity.
    let erofs = staged.layer();
    let erofs_binding = DistroBinding::verify(&erofs).unwrap();
    for (bound, launched) in [(&binding, &erofs), (&erofs_binding, &gpt)] {
        assert_eq!(
            failure(bound.check_with(launched, DISTRO_POLICY_REVISION, METADATA_ONLY)),
            (
                ErrorKind::Unavailable,
                format!(
                    "the distro binding of {image} names the {} layer with identity {UUID}, not \
                     the {} layer with identity {UUID}",
                    bound.format(),
                    launched.format()
                )
            )
        );
    }

    // Messages name a missing disk a GPT image.
    fs::remove_file(staged.image()).unwrap();
    assert_eq!(
        failure(DistroBinding::import(&gpt, PolicyVerdict::ACCEPTED)),
        (
            ErrorKind::Unavailable,
            format!("distro GPT image not found: {image}")
        )
    );
}

/// Makes `path` a symbolic link to itself, whose lookup fails for another reason than a missing
/// file, and returns whether the host allowed it: Windows lets only Developer Mode or a privilege
/// create one.
fn link_to_itself(path: &Path) -> bool {
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(path, path);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(path, path);
    linked.is_ok()
}

#[test]
fn a_manifest_path_whose_lookup_fails_is_no_absent_manifest_for_a_binding() {
    // Verification finds a manifest as `Path.exists` does, which counts a path whose lookup fails
    // as none. The binding of the manifest's absence holds only while nothing is at its path, so
    // the staged check refuses such a path, without reading anything, and so does an import.
    let staged = Staged::new(false);
    let layer = staged.layer();
    let binding = verify_with(&layer, METADATA_ONLY).unwrap();
    assert_eq!(binding.manifest(), None);
    if !link_to_itself(&staged.manifest()) {
        eprintln!("skipped: this host does not let the test create a symbolic link");
        return;
    }
    assert_eq!(verify_with(&layer, METADATA_ONLY).unwrap(), binding);
    for (kind, message) in [
        failure(binding.check_with(&layer, DISTRO_POLICY_REVISION, METADATA_ONLY)),
        failure(import_with(&layer, PolicyVerdict::ACCEPTED, METADATA_ONLY)),
    ] {
        assert_eq!(kind, ErrorKind::Io, "{message}");
        assert!(
            message.starts_with(&format!("{:?}: ", shown(&staged.manifest()))),
            "{message}"
        );
    }
    // Once nothing is at the path, the binding holds again.
    fs::remove_file(staged.manifest()).unwrap();
    binding
        .check_with(&layer, DISTRO_POLICY_REVISION, METADATA_ONLY)
        .unwrap();
}
