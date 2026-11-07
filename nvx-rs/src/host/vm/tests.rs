use std::fs;

use super::*;
use crate::ErrorKind;
use crate::activation::{MachineIdMode, View};
use crate::host::config::{
    Boot, PROCESS_COMMAND_LINE_LIMIT, RootConfig, process_command_line_units,
};
use crate::host::runtime::{ArtifactIdentity, LaunchOptions, RuntimeIdentity};
use crate::hypervisor::Hypervisor;

fn is_fresh_pipe(endpoint: &str, prefix: &str) -> bool {
    endpoint
        .strip_prefix(&format!("//./pipe/{prefix}-"))
        .is_some_and(|digits| {
            digits.len() == 32
                && digits
                    .bytes()
                    .all(|digit| matches!(digit, b'0'..=b'9' | b'a'..=b'f'))
        })
}

fn vm_dir() -> (tempfile::TempDir, VmDir) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("vm");
    create_private_dir(&path).unwrap();
    let vm = VmDir::open(&path).unwrap();
    (directory, vm)
}

#[cfg(target_os = "linux")]
#[test]
fn a_vm_directory_is_created_with_mode_700_for_the_current_user_whatever_the_umask() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::thread;

    let directory = tempfile::tempdir().unwrap();
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    let parent = directory.path().join("setgid");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o2700)).unwrap();
    let path = parent.join("vm");
    thread::scope(|scope| {
        scope
            .spawn(|| {
                nix::sched::unshare(nix::sched::CloneFlags::CLONE_FS).unwrap();
                nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o277));
                create_private_dir(&path)
            })
            .join()
            .unwrap()
    })
    .unwrap();
    assert_eq!(mode(&path), 0o700);
    assert_eq!(
        fs::metadata(&path).unwrap().uid(),
        nix::unistd::Uid::effective().as_raw()
    );
    assert_eq!(VmDir::open(&path).unwrap().path(), path);
    create_private_dir(&path).unwrap();
    assert_eq!(mode(&path), 0o700);
}

#[cfg(target_os = "linux")]
#[test]
fn an_existing_directory_that_is_not_private_is_refused_and_left_unchanged() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use nix::sys::stat::Mode;
    use nix::unistd::Uid;

    use crate::host::platform;

    let directory = tempfile::tempdir().unwrap();
    let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777;
    let set_mode = |path: &Path, mode: u32| {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    };
    let refused = |path: &Path, problem: &str| {
        for error in [
            create_private_dir(path).unwrap_err(),
            VmDir::open(path).unwrap_err(),
        ] {
            assert_eq!(error.kind(), ErrorKind::Io, "{error}");
            assert_eq!(
                error.to_string(),
                format!("the VM directory {} {problem}", path.display())
            );
        }
    };
    let path = directory.path().join("vm");
    fs::create_dir(&path).unwrap();
    for shared in [0o750, 0o705, 0o770] {
        set_mode(&path, shared);
        refused(
            &path,
            &format!("is accessible to other users (mode {shared:03o})"),
        );
        assert_eq!(mode(&path), shared);
    }
    for other in [0o2700, 0o1700, 0o500] {
        set_mode(&path, other);
        refused(
            &path,
            &format!(
                "has mode {other:03o}, not the mode 700 that the directory of a control socket \
                 must have"
            ),
        );
        assert_eq!(mode(&path), other);
    }
    assert_eq!(platform::check_owner_only_dir(&path).unwrap(), None);
    assert_eq!(
        platform::tests::store_dir_problem(Uid::from_raw(1001), Mode::S_IRWXU, Uid::from_raw(1000)),
        Some("belongs to user 1001, not to the current user 1000".to_owned())
    );
    set_mode(&path, 0o700);
    let link = directory.path().join("link");
    symlink(&path, &link).unwrap();
    refused(&link, "is not a plain directory");
    let file = directory.path().join("file");
    fs::write(&file, b"").unwrap();
    refused(&file, "is not a plain directory");
}

#[cfg(target_os = "linux")]
#[test]
fn trailing_separators_and_current_directory_suffixes_do_not_hide_a_symbolic_link() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let (directory, vm) = vm_dir();
    let link = directory.path().join("link");
    symlink(vm.path(), &link).unwrap();
    for suffix in ["/", "/.", "/./", "//.//"] {
        let suffixed = |path: &Path| {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            PathBuf::from(name)
        };
        let path = suffixed(&link);
        for error in [
            create_private_dir(&path).unwrap_err(),
            VmDir::open(&path).unwrap_err(),
        ] {
            assert_eq!(error.kind(), ErrorKind::Io, "{error}");
            assert_eq!(
                error.to_string(),
                format!(
                    "the VM directory {} is not a plain directory",
                    link.display()
                )
            );
        }
        let private = suffixed(vm.path());
        create_private_dir(&private).unwrap();
        assert_eq!(VmDir::open(&private).unwrap().path(), vm.path());
    }
    assert_eq!(
        fs::metadata(vm.path()).unwrap().permissions().mode() & 0o7777,
        0o700
    );
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 0);
}

#[cfg(windows)]
#[test]
fn a_vm_directory_is_created_owner_only_and_protected_under_a_permissive_parent() {
    use crate::host::platform;

    let directory = tempfile::tempdir().unwrap();
    platform::tests::set_access(directory.path(), "D:(A;OICI;FA;;;WD)").unwrap();
    let path = directory.path().join("vm");
    create_private_dir(&path).unwrap();
    let sid = platform::tests::user_sid();
    let private = format!("O:{sid}D:P(A;OICI;FA;;;{sid})");
    assert_eq!(platform::tests::access(&path).unwrap(), private);
    assert_eq!(VmDir::open(&path).unwrap().path(), path);
    create_private_dir(&path).unwrap();
    assert_eq!(platform::tests::access(&path).unwrap(), private);
}

#[cfg(windows)]
#[test]
fn existing_vm_directories_that_grant_others_or_inherit_are_refused_and_left_unchanged() {
    use crate::host::platform;

    let directory = tempfile::tempdir().unwrap();
    let sid = platform::tests::user_sid();
    let refused = |path: &Path, problem: &str| {
        let access = platform::tests::access(path).unwrap();
        for error in [
            create_private_dir(path).unwrap_err(),
            VmDir::open(path).unwrap_err(),
        ] {
            assert_eq!(error.kind(), ErrorKind::Io, "{error}");
            assert_eq!(
                error.to_string(),
                format!("the VM directory {} {problem}", path.display())
            );
        }
        assert_eq!(platform::tests::access(path).unwrap(), access);
    };
    let path = directory.path().join("vm");
    create_private_dir(&path).unwrap();
    let full = format!("(A;OICI;FA;;;{sid})");
    for other in ["SY", "BA", "OW"] {
        platform::tests::set_access(&path, &format!("D:P{full}(A;OICI;FA;;;{other})")).unwrap();
        assert_eq!(platform::check_store_dir(&path).unwrap(), None, "{other}");
        refused(&path, "allows others than the current user");
    }
    platform::tests::set_access(&path, "D:P(A;OICI;FA;;;WD)").unwrap();
    refused(&path, "allows others than the current user");
    for list in [
        format!("D:P(A;OICI;FR;;;{sid})"),
        format!("D:P(A;;FA;;;{sid})"),
        format!("D:P(A;OICINP;FA;;;{sid})"),
    ] {
        platform::tests::set_access(&path, &list).unwrap();
        refused(
            &path,
            "does not allow the current user full access that the files and directories \
             created in it inherit",
        );
    }
    platform::tests::set_access(&path, &format!("D:P(D;;WO;;;{sid}){full}")).unwrap();
    refused(
        &path,
        "has an access control entry of type 1, not one that allows access",
    );
    platform::tests::set_access(&path, &format!("D:P{full}")).unwrap();
    let inheriting = path.join("inheriting");
    fs::create_dir(&inheriting).unwrap();
    assert_eq!(platform::check_store_dir(&inheriting).unwrap(), None);
    let owned = platform::tests::access(&inheriting)
        .unwrap()
        .starts_with(&format!("O:{sid}D:"));
    refused(
        &inheriting,
        if owned {
            "inherits access control entries from its parent"
        } else {
            "belongs to another user"
        },
    );
    let file = directory.path().join("file");
    fs::write(&file, b"").unwrap();
    refused(&file, "is not a plain directory");
}

#[test]
fn a_vm_directory_that_cannot_be_opened_or_created_fails_with_the_hosts_error() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    let relative = Path::new("missing-vm-directory");
    for (error, action, path) in [
        (VmDir::open(&missing).unwrap_err(), "open", missing.clone()),
        (
            create_private_dir(&missing.join("vm")).unwrap_err(),
            "create",
            missing.join("vm"),
        ),
        (
            VmDir::open(relative).unwrap_err(),
            "open",
            std::env::current_dir().unwrap().join(relative),
        ),
    ] {
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert_eq!(
            error.to_string(),
            format!("cannot {action} the VM directory {}", path.display())
        );
        let source = std::error::Error::source(&error).unwrap();
        let source = source.downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.kind(), io::ErrorKind::NotFound, "{source}");
    }
}

#[test]
fn outputs_name_the_vm_directorys_files_and_endpoints() {
    let (_directory, vm) = vm_dir();
    let outputs = vm.outputs(false).unwrap();
    assert_eq!(outputs.log, vm.path().join("openvmm.log"));
    assert_eq!(outputs.outcome, vm.path().join("outcome.json"));
    assert_eq!(outputs.console, None);
    let with_console = vm.outputs(true).unwrap();
    assert_eq!(
        (&with_console.log, &with_console.outcome),
        (&outputs.log, &outputs.outcome)
    );
    let console = with_console.console.as_deref().unwrap();
    if cfg!(windows) {
        for endpoint in [outputs.endpoint.as_str(), &with_console.endpoint, console] {
            assert!(is_fresh_pipe(endpoint, "openvmm-microvm"), "{endpoint}");
        }
        assert_ne!(outputs.endpoint, with_console.endpoint);
        assert_ne!(with_console.endpoint, console);
    } else {
        let socket = |name: &str| vm.path().join(name).into_os_string().into_string().unwrap();
        assert_eq!(outputs.endpoint, socket("control.sock"));
        assert_eq!(with_console.endpoint, outputs.endpoint);
        assert_eq!(console, socket("console.sock"));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn the_endpoints_of_a_vm_directory_that_is_not_unicode_are_refused() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(OsStr::from_bytes(b"vm-\xff"));
    create_private_dir(&path).unwrap();
    let vm = VmDir::open(&path).unwrap();
    for error in [
        vm.outputs(false).unwrap_err(),
        vm.restore_ready_endpoint().unwrap_err(),
    ] {
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert_eq!(
            error.to_string(),
            format!(
                "the VM directory {} is not valid Unicode, which OpenVMM's endpoints must be",
                path.display()
            )
        );
    }
}

#[test]
fn a_restore_ready_endpoint_is_the_reserved_socket_or_a_fresh_pipe() {
    let (_directory, vm) = vm_dir();
    let first = vm.restore_ready_endpoint().unwrap();
    let second = vm.restore_ready_endpoint().unwrap();
    if cfg!(windows) {
        for endpoint in [&first, &second] {
            assert!(is_fresh_pipe(endpoint, "nvx-restore-ready"), "{endpoint}");
        }
        assert_ne!(first, second);
        let error = fs::File::open(first.replace('/', "\\")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
    } else {
        let socket = vm.path().join("restore-ready.sock");
        assert_eq!(first, socket.into_os_string().into_string().unwrap());
        assert_eq!(second, first);
    }
}

#[test]
fn the_reserved_names_are_the_documented_ones_and_leave_the_openvmm_prefix_to_openvmm() {
    let names = [
        LOG,
        OUTCOME,
        CONTROL,
        CONSOLE,
        RESTORE_READY,
        OPENVMM,
        KEY,
        DESCRIPTION,
        CAPTURE_IMAGE,
        CAPTURE_SCRATCH,
    ];
    assert_eq!(
        names,
        [
            "openvmm.log",
            "outcome.json",
            "control.sock",
            "console.sock",
            "restore-ready.sock",
            "openvmm",
            "key.bin",
            "snapshot.json",
            "capture-image.raw",
            "capture-scratch.raw",
        ]
    );
    for name in names {
        assert!(!name.starts_with(".openvmm"), "{name}");
    }
}

#[test]
fn a_capability_gives_its_bytes_and_its_debug_hides_them() {
    let bytes: [u8; CAPABILITY_LEN] = std::array::from_fn(|index| u8::try_from(index + 1).unwrap());
    let capability = Capability(bytes);
    assert_eq!(capability.as_bytes(), &bytes);
    assert_eq!(format!("{capability:?}"), "Capability { .. }");
    assert_eq!(format!("{capability:#?}"), "Capability { .. }");
}

#[test]
fn creating_and_opening_a_vm_directory_writes_no_capability_file_or_output() {
    let (directory, vm) = vm_dir();
    create_private_dir(vm.path()).unwrap();
    VmDir::open(vm.path()).unwrap();
    vm.outputs(true).unwrap();
    vm.restore_ready_endpoint().unwrap();
    assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 0);
    let entries: Vec<_> = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["vm"]);
}

fn cold_config() -> VmConfig {
    VmConfig {
        processors: 1,
        memory_mib: 256,
        root: RootConfig::Blockless,
        view: View {
            hostname: b"box".to_vec(),
            machine_id: MachineIdMode::Image,
        },
        network: None,
        filesystem: None,
        console: ConsoleAttachment::Listener,
        boot: Some(Boot {
            command_line: "nvx_entrypoint=/bin/sh nvx_arg=-c nvx_arg=exit".to_owned(),
        }),
    }
}

fn cold_runtime() -> Runtime {
    let identity = ArtifactIdentity::new("test-runtime").unwrap();
    Runtime {
        openvmm: PathBuf::from("missing-openvmm"),
        kernel: PathBuf::from("missing-kernel"),
        initramfs: PathBuf::from("missing-initramfs"),
        identity: RuntimeIdentity::new(identity.clone(), identity.clone(), identity),
        hypervisor: if cfg!(windows) {
            Hypervisor::Whp
        } else {
            Hypervisor::Mshv
        },
        cpu_profile: None,
        windows_breakaway: false,
        options: LaunchOptions::default(),
    }
}

#[test]
fn cold_creation_is_inspectable_without_a_process_listener_or_capability_file() {
    for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
        let (_directory, vm) = vm_dir();
        let runtime = cold_runtime();
        let config = VmConfig {
            console,
            ..cold_config()
        };
        let prepared = create(&runtime, &config, None, &vm).unwrap();
        let outputs = prepared.outputs();
        assert_eq!(outputs.log, vm.path().join("openvmm.log"));
        assert_eq!(
            outputs.console.is_some(),
            console == ConsoleAttachment::Listener
        );
        assert!(!outputs.outcome.exists());
        assert_eq!(prepared.capability().as_bytes().len(), CAPABILITY_LEN);
        assert_ne!(prepared.capability().as_bytes(), &[0; CAPABILITY_LEN]);
        assert_eq!(
            prepared.arguments(),
            config
                .openvmm_arguments(
                    &runtime,
                    &outputs.endpoint,
                    &outputs.outcome,
                    outputs.console.as_deref()
                )
                .unwrap()
        );
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 1);
        drop(prepared);
        assert!(vm.path().join(LOG).exists());
    }
}

#[test]
fn a_missing_boot_and_a_snapshot_are_refused_before_any_output_is_created() {
    let (_directory, vm) = vm_dir();
    let runtime = cold_runtime();
    let mut config = cold_config();
    config.boot = None;
    let snapshot = Snapshot { _private: () };
    for (from, config, expected) in [
        (None, &config, "Boot"),
        (Some(&snapshot), &cold_config(), "Snapshot"),
    ] {
        let error = create(&runtime, config, from, &vm).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 0);
    }
}

#[test]
fn creation_rolls_back_its_log_and_never_removes_preexisting_outputs_or_inputs() {
    let mut names = vec![LOG, OUTCOME];
    if !cfg!(windows) {
        names.extend([CONTROL, CONSOLE]);
    }
    for name in names {
        let (_directory, vm) = vm_dir();
        let existing = vm.path().join(name);
        let input = vm.path().join("caller-input");
        fs::write(&existing, b"preexisting output").unwrap();
        fs::write(&input, b"borrowed input").unwrap();
        let error = create(&cold_runtime(), &cold_config(), None, &vm).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert_eq!(fs::read(&existing).unwrap(), b"preexisting output");
        assert_eq!(fs::read(&input).unwrap(), b"borrowed input");
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 2);
    }
}

#[test]
fn the_log_and_inherited_resources_stay_open_without_reopening_their_paths() {
    use std::io::Write;
    let (_directory, vm) = vm_dir();
    let mut prepared = create(&cold_runtime(), &cold_config(), None, &vm).unwrap();
    let log = prepared.outputs().log.clone();
    let moved_log = vm.path().join("original-log");
    fs::rename(&log, &moved_log).unwrap();
    fs::write(&log, b"replacement log").unwrap();
    prepared
        .log()
        .try_clone()
        .unwrap()
        .write_all(b"original log")
        .unwrap();
    assert_eq!(fs::read(&moved_log).unwrap(), b"original log");
    assert_eq!(fs::read(&log).unwrap(), b"replacement log");
    let input = vm.path().join("caller-input");
    let moved_input = vm.path().join("original-input");
    let file = File::create(&input).unwrap();
    prepared.inherit(file);
    fs::rename(&input, &moved_input).unwrap();
    fs::write(&input, b"replacement input").unwrap();
    prepared.inherit[0].write_all(b"original input").unwrap();
    assert_eq!(fs::read(&moved_input).unwrap(), b"original input");
    assert_eq!(fs::read(&input).unwrap(), b"replacement input");
}

#[test]
fn creation_checks_the_complete_actual_command_line_at_the_windows_boundary() {
    let (_directory, vm) = vm_dir();
    let config = cold_config();
    config.check_command_line().unwrap();
    let mut runtime = cold_runtime();
    let outputs = vm.outputs(true).unwrap();
    let arguments = config
        .openvmm_arguments(
            &runtime,
            &outputs.endpoint,
            &outputs.outcome,
            outputs.console.as_deref(),
        )
        .unwrap();
    let length = process_command_line_units(runtime.openvmm.as_os_str(), &arguments);
    runtime
        .openvmm
        .as_mut_os_string()
        .push("x".repeat(PROCESS_COMMAND_LINE_LIMIT - length));
    let prepared = create(&runtime, &config, None, &vm).unwrap();
    assert_eq!(
        process_command_line_units(runtime.openvmm.as_os_str(), prepared.arguments()),
        PROCESS_COMMAND_LINE_LIMIT
    );
    let log = prepared.outputs().log.clone();
    drop(prepared);
    fs::remove_file(&log).unwrap();
    let input = vm.path().join("caller-input");
    fs::write(&input, b"borrowed input").unwrap();
    runtime.openvmm.as_mut_os_string().push("x");
    let error = create(&runtime, &config, None, &vm).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    assert!(error.to_string().contains("32768"), "{error}");
    assert!(!log.exists());
    assert_eq!(fs::read(&input).unwrap(), b"borrowed input");
    assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 1);
}
