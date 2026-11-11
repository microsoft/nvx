//! The checklist of the security requirements that the host's documentation states, through the
//! public API alone: the guest paths of an export, the checks of a staged image and of a scratch
//! image's claim, the stability of exported objects, the privacy of a VM's directory and outputs,
//! and the proofs of a VM's liveness, of its endpoint's owner and of its recovery from saved
//! values, and the strictness of a snapshot's directory.
//!
//! Each check is short and runs no VM: a process of the test's own, an idle copy of the test
//! binary, stands in for a VM's process where one is needed.
#![cfg(all(feature = "host", any(target_os = "linux", windows)))]

use std::error::Error as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nvx::activation::{MachineIdMode, View};
use nvx::host::config::{Boot, ConsoleAttachment, RootConfig, VmConfig};
#[cfg(windows)]
use nvx::host::filesystem::Owner;
use nvx::host::filesystem::{self, ExportRequest, Mapping, ReservedTargets, Target};
use nvx::host::process::{BootId, ProcessRef};
use nvx::host::runtime::{ArtifactIdentity, LaunchOptions, Runtime, RuntimeIdentity};
use nvx::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding, PolicyVerdict};
use nvx::host::sandbox::layer::{LayerRole, SandboxLayer};
use nvx::host::sandbox::scratch::{CLAIM_RELEASE_WAIT, ScratchBinding};
use nvx::host::seal::FileSeal;
use nvx::host::{platform, snapshot, transport, vm};
use nvx::{ErrorCode, ErrorKind, Hypervisor};

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
fn a_staged_image_that_was_replaced_fails_its_check() {
    // Callers stage images in a directory that only their own account can write.
    let (_directory, images) = private_directory();
    let path = images.path().join("distro.erofs");
    fs::write(&path, b"layer").unwrap();
    let layer = SandboxLayer::new(
        LayerRole::Distro,
        path.to_str().unwrap(),
        "01234567-89ab-cdef-0123-456789abcdef",
    );
    let binding = DistroBinding::import(&layer, PolicyVerdict::ACCEPTED).unwrap();
    drop(binding.check(&layer, DISTRO_POLICY_REVISION).unwrap());
    fs::rename(&path, images.path().join("original.erofs")).unwrap();
    fs::write(&path, b"layer").unwrap();
    assert_eq!(
        binding
            .check(&layer, DISTRO_POLICY_REVISION)
            .unwrap_err()
            .kind(),
        ErrorKind::Unavailable
    );
}

#[test]
fn a_scratch_image_is_claimed_by_one_launch_and_checked_again_after_a_release() {
    let (_directory, images) = private_directory();
    let path = images.path().join("scratch.ext4");
    File::create(&path).unwrap().set_len(4096).unwrap();
    let binding = ScratchBinding::bind(&path).unwrap();
    let held = binding
        .check(&path, Instant::now() + CLAIM_RELEASE_WAIT)
        .unwrap();
    let error = binding
        .check(&path, Instant::now() + Duration::from_millis(50))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    assert_eq!(error.code(), ErrorCode::ScratchClaimed);
    drop(held);
    drop(
        binding
            .check(&path, Instant::now() + CLAIM_RELEASE_WAIT)
            .unwrap(),
    );
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(8192)
        .unwrap();
    assert_eq!(
        binding
            .check(&path, Instant::now() + CLAIM_RELEASE_WAIT)
            .unwrap_err()
            .kind(),
        ErrorKind::Unavailable,
    );
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
    let opened = FileSeal::of(prepared.log()).unwrap();
    let named = FileSeal::of(&File::open(&log).unwrap()).unwrap();
    assert_eq!(
        (opened.device(), opened.file_id()),
        (named.device(), named.file_id())
    );
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

/// An idle copy of the test binary, which stands in for a VM's process, and is killed and
/// reaped when dropped.
struct IdleChild(Child);

impl IdleChild {
    fn start() -> Self {
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "idle_child", "--ignored", "--test-threads=1"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn reference(&self) -> ProcessRef {
        ProcessRef {
            pid: self.0.id(),
            start_time: platform::process_start_time(self.0.id()).unwrap().unwrap(),
            boot: BootId::current().unwrap(),
        }
    }
}

impl Drop for IdleChild {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}

#[test]
#[ignore = "the idle stand-in for a VM's process, which only the checklist starts"]
fn idle_child() {
    io::stdin().read_to_end(&mut Vec::new()).unwrap();
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

/// Returns what a caller saves of a VM whose process is `process`, with an endpoint that no
/// process serves.
fn saved(directory: &vm::VmDir, process: ProcessRef) -> vm::SavedVm {
    #[cfg(target_os = "linux")]
    let endpoint = directory
        .path()
        .join("control.sock")
        .to_str()
        .unwrap()
        .to_owned();
    #[cfg(windows)]
    let endpoint = {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).unwrap();
        let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        format!(r"\\.\pipe\openvmm-microvm-{suffix}")
    };
    vm::SavedVm {
        vm_dir: directory.clone(),
        endpoint,
        capability: [0xa5; nvx::protocol::CAPABILITY_LEN],
        process,
        runtime: runtime().identity,
    }
}

#[test]
fn a_saved_vm_runs_until_its_process_exits_and_another_runtime_is_refused_without_a_signal() {
    let (_directory, dir) = private_directory();
    let mut child = IdleChild::start();
    let saved = saved(&dir, child.reference());
    let held = vm::verify(&saved, &saved.runtime).unwrap();
    assert!(held.wait(Instant::now()).unwrap().is_none());
    let mut other = saved.runtime.clone();
    other.control_session += 1;
    assert_eq!(
        vm::verify(&saved, &other).unwrap_err().kind(),
        ErrorKind::RuntimeMismatch
    );
    assert!(!held.has_exited().unwrap());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(
        held.wait(Instant::now() + Duration::from_secs(10))
            .unwrap()
            .is_some()
    );
    let error = vm::verify(&saved, &saved.runtime).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Exited);
    assert_eq!(
        error
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap()
            .kind(),
        io::ErrorKind::NotFound
    );
}

/// Serves `endpoint` from the test's own process, which is not the VM's.
#[cfg(target_os = "linux")]
fn endpoint_server(endpoint: &str) -> std::os::unix::net::UnixListener {
    std::os::unix::net::UnixListener::bind(endpoint).unwrap()
}

/// Serves `endpoint` from the test's own process, which is not the VM's.
#[cfg(windows)]
fn endpoint_server(endpoint: &str) -> File {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    let name: Vec<u16> = endpoint.encode_utf16().chain(Some(0)).collect();
    // SAFETY: the name is a NUL-terminated wide string that outlives the call, and the test owns
    // the new pipe, which the returned file closes.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    assert_ne!(
        handle,
        INVALID_HANDLE_VALUE,
        "{}",
        io::Error::last_os_error()
    );
    // SAFETY: the successful call returned a new handle that nothing else owns.
    File::from(unsafe { OwnedHandle::from_raw_handle(handle) })
}

#[test]
fn reattaching_refuses_an_endpoint_that_another_process_serves_and_keeps_the_vm_and_its_values() {
    let (_directory, dir) = private_directory();
    let child = IdleChild::start();
    let saved = saved(&dir, child.reference());
    let before = saved.clone();
    let _server = endpoint_server(&saved.endpoint);
    let deadline = Instant::now() + Duration::from_secs(10);
    let error = vm::reattach(&saved, &runtime(), deadline).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::Protocol);
    assert_eq!(
        error
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(
        !vm::verify(&saved, &saved.runtime)
            .unwrap()
            .has_exited()
            .unwrap()
    );
    assert_eq!(saved.endpoint, before.endpoint);
    assert_eq!(saved.capability, before.capability);
    assert_eq!(saved.process, before.process);
    assert_eq!(saved.runtime, before.runtime);
    assert_eq!(saved.vm_dir.path(), before.vm_dir.path());

    let mut invalid = saved.clone();
    invalid.process.pid = 0;
    let error = transport::connect_verified(&invalid.endpoint, &invalid.process, deadline)
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        error
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(
        !vm::verify(&saved, &saved.runtime)
            .unwrap()
            .has_exited()
            .unwrap()
    );
}
