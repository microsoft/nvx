//! The checklist of the security requirements that the host's documentation states, through the
//! public API alone: the guest paths of an export, the stability of exported objects, the privacy
//! of a VM's directory and outputs, and the strictness of a snapshot's directory. The checks of a
//! staged image and of a scratch image's claim, which only NVX's CLI makes, are unit tests of
//! `host::sandbox::distro` and `host::sandbox::scratch`.
//!
//! Each check is short and runs no VM. The proofs of a saved VM's liveness, of its endpoint's
//! owner and of its recovery from saved values need a process of the test's own, named by its
//! start time, which only the crate's private platform layer reads, so the unit tests of
//! `nvx::host::vm` check them.
#![cfg(all(feature = "host", any(target_os = "linux", windows)))]

use std::fs::{self, File};
#[cfg(windows)]
use std::io;
use std::path::PathBuf;
#[cfg(windows)]
use std::process::{Command, Stdio};

use nvx::activation::{MachineIdMode, View};
use nvx::host::config::{Boot, ConsoleAttachment, RootConfig, VmConfig};
#[cfg(windows)]
use nvx::host::filesystem::Owner;
use nvx::host::filesystem::{self, ExportRequest, Mapping, ReservedTargets, Target};
use nvx::host::runtime::{ArtifactIdentity, LaunchOptions, Runtime, RuntimeIdentity};
use nvx::host::{snapshot, vm};
use nvx::{ErrorKind, Hypervisor};

/// Plans the export of `mappings` into the blockless root.
fn plan(mappings: &[Mapping<'_>]) -> nvx::Result<Option<filesystem::ExportPlan>> {
    filesystem::plan(&ExportRequest::new(
        mappings,
        ReservedTargets::BLOCKLESS_ROOT,
    ))
}

#[test]
fn an_export_refuses_guest_targets_that_are_not_canonical_or_that_a_root_reserves() {
    let directory = tempfile::tempdir().unwrap();
    for target in [
        "",
        "relative",
        "/",
        "/work/",
        "/work//child",
        "/work/./child",
        "/work/../child",
        "/work\0child",
        "/etc",
        "/etc/child",
    ] {
        let mappings = [Mapping {
            name: "filesystem",
            path: directory.path(),
            target: Target::Guest(target),
            read_only: true,
        }];
        let error = plan(&mappings).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{target:?}: {error}");
    }
    let mappings = [Mapping {
        name: "filesystem",
        path: directory.path(),
        target: Target::Guest("/work"),
        read_only: true,
    }];
    let plan = plan(&mappings).unwrap().unwrap();
    plan.check(ReservedTargets::BLOCKLESS_ROOT).unwrap();
    plan.verify().unwrap();
}

#[test]
fn an_export_refuses_a_read_only_path_inside_a_writable_directory() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("input");
    fs::write(&file, b"input").unwrap();
    let mappings = [
        Mapping {
            name: "filesystem",
            path: directory.path(),
            target: Target::Guest("/work"),
            read_only: false,
        },
        Mapping {
            name: "filesystem",
            path: &file,
            target: Target::Guest("/work/input"),
            read_only: true,
        },
    ];
    assert_eq!(plan(&mappings).unwrap_err().kind(), ErrorKind::Unsupported);
}

#[cfg(windows)]
#[test]
fn the_caller_owner_is_refused_on_windows() {
    let mut request = ExportRequest::new(&[], ReservedTargets::BLOCKLESS_ROOT);
    request.owner = Owner::Caller;
    assert_eq!(
        filesystem::plan(&request).unwrap_err().kind(),
        ErrorKind::Unsupported
    );
}

#[test]
fn an_exported_object_that_a_workload_could_move_is_checked_again_before_a_start() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("input");
    fs::write(&file, b"original").unwrap();
    let mappings = [
        Mapping {
            name: "filesystem",
            path: directory.path(),
            target: Target::Guest("/work"),
            read_only: false,
        },
        Mapping {
            name: "filesystem",
            path: &file,
            target: Target::Guest("/work/input"),
            read_only: false,
        },
    ];
    let plan = plan(&mappings).unwrap().unwrap();
    plan.verify().unwrap();
    fs::rename(&file, directory.path().join("kept-original")).unwrap();
    fs::write(&file, b"replacement").unwrap();
    assert_eq!(plan.verify().unwrap_err().kind(), ErrorKind::Other);
    assert_eq!(
        fs::read(directory.path().join("kept-original")).unwrap(),
        b"original"
    );
}

/// Returns a new private directory below a temporary one, which the caller keeps.
fn private_directory() -> (tempfile::TempDir, vm::VmDir) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("private");
    vm::create_private_dir(&path).unwrap();
    let vm_dir = vm::VmDir::open(&path).unwrap();
    (directory, vm_dir)
}

#[test]
fn a_vm_directory_is_private_and_never_restricted_in_place() {
    let (_directory, dir) = private_directory();
    vm::VmDir::open(dir.path()).unwrap();
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
    }
    // Every user, the well-known SID S-1-1-0, gets full access, which what it holds inherits.
    #[cfg(windows)]
    let acl = || {
        let output = Command::new("icacls").arg(dir.path()).output().unwrap();
        assert!(output.status.success());
        output.stdout
    };
    #[cfg(windows)]
    let shared = {
        let private = acl();
        assert!(
            Command::new("icacls")
                .arg(dir.path())
                .args(["/grant", "*S-1-1-0:(OI)(CI)(F)"])
                .stdout(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let shared = acl();
        assert_ne!(shared, private);
        shared
    };
    assert_eq!(
        vm::create_private_dir(dir.path()).unwrap_err().kind(),
        ErrorKind::Io
    );
    assert_eq!(
        vm::VmDir::open(dir.path()).unwrap_err().kind(),
        ErrorKind::Io
    );
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o777
        );
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[cfg(windows)]
    assert_eq!(acl(), shared);
}

/// Returns the identity of the file that `file` holds open: its device and inode on Linux.
#[cfg(target_os = "linux")]
fn file_id(file: &File) -> (u64, u128) {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().unwrap();
    (metadata.dev(), u128::from(metadata.ino()))
}

/// Returns the identity of the file that `file` holds open: on Windows, the volume serial number
/// and the 128-bit file ID of its `FILE_ID_INFO`, or, on a volume that has no file ID information
/// or that reports a zero ID, those of its `BY_HANDLE_FILE_INFORMATION`.
#[cfg(windows)]
fn file_id(file: &File) -> (u64, u128) {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx,
    };

    let handle = file.as_raw_handle() as HANDLE;
    let mut id = FILE_ID_INFO::default();
    // SAFETY: the handle is open for as long as `file` is borrowed, and the buffer is a
    // FILE_ID_INFO of the size that is passed.
    let has_id = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&raw mut id).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } != 0;
    let file_id = u128::from_le_bytes(id.FileId.Identifier);
    if has_id && file_id != 0 {
        return (id.VolumeSerialNumber, file_id);
    }
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is open for as long as `file` is borrowed, and the output pointer is
    // valid.
    let read = unsafe { GetFileInformationByHandle(handle, &mut information) };
    assert_ne!(read, 0, "{}", io::Error::last_os_error());
    (
        u64::from(information.dwVolumeSerialNumber),
        (u128::from(information.nFileIndexHigh) << 32) | u128::from(information.nFileIndexLow),
    )
}

/// A cold configuration that creation checks without reading the host.
fn cold() -> VmConfig {
    VmConfig {
        processors: 1,
        memory_mib: 256,
        root: RootConfig::Blockless,
        view: View {
            hostname: b"checklist".to_vec(),
            machine_id: MachineIdMode::Image,
        },
        network: None,
        filesystem: None,
        console: ConsoleAttachment::Disconnected,
        boot: Some(Boot {
            command_line: "quiet".to_owned(),
            memory_capacity_mib: None,
            capture: None,
        }),
    }
}

#[test]
fn a_launch_takes_a_new_private_log_and_keeps_every_entry_that_it_did_not_create() {
    let (_directory, dir) = private_directory();
    let caller = dir.path().join("outcome.json");
    fs::write(&caller, b"caller's").unwrap();
    assert_eq!(
        vm::create(&runtime(), &cold(), None, &dir)
            .unwrap_err()
            .kind(),
        ErrorKind::Io
    );
    assert_eq!(fs::read(&caller).unwrap(), b"caller's");
    fs::remove_file(&caller).unwrap();
    assert!(fs::read_dir(dir.path()).unwrap().next().is_none());

    let prepared = vm::create(&runtime(), &cold(), None, &dir).unwrap();
    let log = prepared.outputs().log.clone();
    assert_eq!(file_id(prepared.log()), file_id(&File::open(&log).unwrap()));
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(prepared);
    fs::write(&log, b"the first launch's").unwrap();
    assert_eq!(
        vm::create(&runtime(), &cold(), None, &dir)
            .unwrap_err()
            .kind(),
        ErrorKind::Io
    );
    assert_eq!(fs::read(&log).unwrap(), b"the first launch's");
}

#[test]
fn a_snapshot_opens_only_from_a_snapshot_directory_and_changes_nothing() {
    let (_directory, dir) = private_directory();
    let caller = dir.path().join("outcome.json");
    fs::write(&caller, b"caller-owned launch state").unwrap();
    assert_eq!(
        snapshot::Snapshot::open(dir.path()).unwrap_err().kind(),
        ErrorKind::Io
    );
    assert_eq!(fs::read(&caller).unwrap(), b"caller-owned launch state");
}

/// A runtime whose files no check reads.
fn runtime() -> Runtime {
    Runtime {
        openvmm: PathBuf::from("openvmm"),
        kernel: PathBuf::from("vmlinux"),
        initramfs: PathBuf::from("initramfs.cpio.gz"),
        identity: RuntimeIdentity::new(
            ArtifactIdentity::new("install:openvmm").unwrap(),
            ArtifactIdentity::new("install:kernel").unwrap(),
            ArtifactIdentity::new("install:initramfs").unwrap(),
        ),
        hypervisor: if cfg!(windows) {
            Hypervisor::Whp
        } else {
            Hypervisor::Kvm
        },
        cpu_profile: None,
        windows_breakaway: false,
        options: LaunchOptions::default(),
    }
}
