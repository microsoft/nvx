//! Tests of file seals and of the opens of sealed files.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;

/// Opens `path` for its metadata alone, so that no read or mapping of its content can succeed
/// through the file: with `O_PATH` on Linux, and on Windows with `FILE_READ_ATTRIBUTES` alone,
/// sharing every access. Linux fails every read and mapping of an `O_PATH` descriptor with
/// `EBADF`.
#[cfg(target_os = "linux")]
pub(crate) fn open_metadata_only(path: &Path) -> io::Result<File> {
    use nix::fcntl::OFlag;
    use nix::sys::stat::Mode;

    // `nix` passes `O_PATH` on, where `OpenOptionsExt::custom_flags` masks out the bits of
    // `O_ACCMODE`, which on musl include `O_PATH`: a musl build would open the file for reading.
    let file = nix::fcntl::open(path, OFlag::O_PATH | OFlag::O_CLOEXEC, Mode::empty())?;
    Ok(File::from(file))
}

/// Opens `path` for its metadata alone, as the Linux variant does. Without `FILE_READ_DATA`,
/// Windows refuses every read of the handle, and every mapping of the file through it.
#[cfg(windows)]
pub(crate) fn open_metadata_only(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, SYNCHRONIZE,
    };

    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// The scheme of this host's seals.
const SCHEME: SealScheme = if cfg!(windows) {
    SealScheme::Windows
} else {
    SealScheme::Linux
};

/// Returns the seal of the file at `path`, opened for reading.
fn seal_of(path: &Path) -> FileSeal {
    FileSeal::of(&File::open(path).unwrap()).unwrap()
}

/// Returns the time of `time` as a seal holds it.
fn seal_time(time: SystemTime) -> SealTime {
    let since = time.duration_since(UNIX_EPOCH).unwrap();
    SealTime {
        seconds: i64::try_from(since.as_secs()).unwrap(),
        nanoseconds: since.subsec_nanos(),
    }
}

#[test]
fn a_seal_holds_the_identity_kind_length_and_times_of_an_open_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("layer.erofs");
    fs::write(&path, b"layer").unwrap();
    let seal = seal_of(&path);
    assert_eq!(seal.scheme(), SCHEME);
    assert_eq!(seal.kind(), FileKind::Regular);
    assert_eq!(seal.length(), 5);
    // The identity is the host's: on Linux, `st_dev` and `st_ino`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(
            (seal.device(), seal.file_id()),
            (metadata.dev(), u128::from(metadata.ino()))
        );
    }
    // A hard link names the same file, which seals alike, and another file has another identity.
    let other = directory.path().join("other.erofs");
    fs::write(&other, b"other").unwrap();
    let link = directory.path().join("link.erofs");
    fs::hard_link(&other, &link).unwrap();
    assert_eq!(seal_of(&link), seal_of(&other));
    assert_ne!(seal_of(&other).file_id(), seal.file_id());
    let metadata = fs::metadata(&path).unwrap();
    assert_eq!(seal.modified(), seal_time(metadata.modified().unwrap()));
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;

        let changed = seal.changed().unwrap();
        assert_eq!(
            (changed.seconds, i64::from(changed.nanoseconds)),
            (metadata.ctime(), metadata.ctime_nsec())
        );
    }
    #[cfg(windows)]
    assert_eq!(seal.changed(), None);
    // Every open of the unchanged file seals it alike, whatever its access.
    assert_eq!(FileSeal::of(&open_guarded(&path).unwrap()).unwrap(), seal);
    assert_eq!(FileSeal::of(&open_read(&path).unwrap()).unwrap(), seal);
    assert_eq!(
        FileSeal::of(&open_metadata_only(&path).unwrap()).unwrap(),
        seal
    );
    let folder = FileSeal::of(&open_guarded(directory.path()).unwrap()).unwrap();
    assert_eq!(folder.kind(), FileKind::Directory);
    assert_eq!(folder.difference(&seal), Some("identity"));
}

#[test]
fn a_seal_needs_no_access_to_the_content() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("layer.erofs");
    fs::write(&path, b"layer").unwrap();
    let mut file = open_metadata_only(&path).unwrap();
    // The file refuses reads, which also shows that it refuses mappings, which need read access
    // on both hosts.
    let mut buffer = [0; 5];
    assert!(file.read(&mut buffer).is_err());
    assert_eq!(FileSeal::of(&file).unwrap(), seal_of(&path));
}

#[test]
fn seals_differ_when_their_file_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("layer.erofs");
    fs::write(&path, b"layer").unwrap();
    let original = seal_of(&path);

    // A file that replaces it has another identity, even with the same content and times.
    let replacement = directory.path().join("replacement.erofs");
    fs::copy(&path, &replacement).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    OpenOptions::new()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    fs::rename(&replacement, &path).unwrap();
    let replaced = seal_of(&path);
    assert_eq!(original.difference(&replaced), Some("identity"));
    assert_ne!(original, replaced);

    // An append changes its length, and a new modification time its modification time.
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"!").unwrap();
    file.set_modified(modified).unwrap();
    drop(file);
    let appended = seal_of(&path);
    assert_eq!(replaced.difference(&appended), Some("length"));
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified + Duration::from_secs(1))
        .unwrap();
    let touched = seal_of(&path);
    assert_eq!(appended.difference(&touched), Some("modification time"));
    assert_eq!(
        touched.modified(),
        seal_time(modified + Duration::from_secs(1))
    );
    assert_eq!(touched.difference(&touched.clone()), None);

    // On Linux, a change of its metadata alone moves its change time, which nothing sets back.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;

        // File times come from a clock that advances with the scheduler's tick.
        std::thread::sleep(Duration::from_millis(50));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        let changed = seal_of(&path);
        assert_eq!(touched.difference(&changed), Some("change time"));
    }
}

#[test]
fn seals_round_trip_through_their_serde_form() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("layer.erofs");
    fs::write(&path, b"layer").unwrap();
    let seal = seal_of(&path);
    let value = serde_json::to_value(&seal).unwrap();
    let mut fields: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "changed", "device", "fileId", "kind", "length", "modified", "scheme", "version"
        ]
    );
    assert_eq!(value["version"], 1);
    assert_eq!(value["kind"], "regular");
    assert_eq!(value["length"], 5);
    assert_eq!(value["fileId"], format!("{:032x}", seal.file_id()).as_str());
    assert_eq!(
        value["scheme"],
        if cfg!(windows) { "windows" } else { "linux" }
    );
    assert_eq!(value["changed"].is_null(), cfg!(windows));
    assert_eq!(
        serde_json::from_value::<FileSeal>(value.clone()).unwrap(),
        seal
    );

    // Every field is required, `changed` too when it is null, and no other is accepted.
    for field in fields {
        let mut partial = value.clone();
        partial.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<FileSeal>(partial).is_err(),
            "{field}"
        );
    }
    let mut extended = value.clone();
    extended["inode"] = 1.into();
    assert!(serde_json::from_value::<FileSeal>(extended).is_err());
    for file_id in [
        "1",
        "0x0000000000000000000000000000000",
        "g".repeat(32).as_str(),
    ] {
        let mut invalid = value.clone();
        invalid["fileId"] = file_id.into();
        assert!(
            serde_json::from_value::<FileSeal>(invalid).is_err(),
            "{file_id}"
        );
    }
    // A seal of another version differs from every seal of this one.
    let mut other = value;
    other["version"] = 2.into();
    let other: FileSeal = serde_json::from_value(other).unwrap();
    assert_eq!(seal.difference(&other), Some("seal version"));
}

#[cfg(windows)]
#[test]
fn a_guarded_file_denies_writers_and_deleters_until_it_is_closed() {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
    };

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("layer.erofs");
    let other = directory.path().join("other.erofs");
    fs::write(&path, b"layer").unwrap();
    fs::write(&other, b"other").unwrap();
    let guard = open_guarded(&path).unwrap();
    // Readers share it: OpenVMM opens a read-only disk with the default sharing of the standard
    // library, and a disk that it may snapshot with read and delete sharing.
    File::open(&path).unwrap();
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)
        .unwrap();
    let error = OpenOptions::new().write(true).open(&path).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
    assert!(fs::remove_file(&path).is_err());
    assert!(fs::rename(&path, directory.path().join("renamed.erofs")).is_err());
    assert!(fs::rename(&other, &path).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"layer");
    drop(guard);
    fs::rename(&other, &path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"other");
}
