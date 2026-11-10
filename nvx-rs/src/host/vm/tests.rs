use std::fs;
#[cfg(any(target_os = "linux", windows))]
use std::process::{Command, Stdio};
#[cfg(any(target_os = "linux", windows))]
use std::time::{Duration, Instant};

use super::*;
use crate::ErrorKind;
use crate::activation::{Journal, MachineIdMode, OverlayMode, View};
use crate::host::config::{
    Boot, LayeredRoot, PROCESS_COMMAND_LINE_LIMIT, RootConfig, UpperConfig,
    process_command_line_units,
};
use crate::host::runtime::{ArtifactIdentity, LaunchOptions, RuntimeIdentity};
use crate::hypervisor::Hypervisor;
use crate::protocol::root::RootImage;

#[cfg(any(target_os = "linux", windows))]
mod reattach;

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
            memory_capacity_mib: None,
            command_line: "nvx_entrypoint=/bin/sh nvx_arg=-c nvx_arg=exit".to_owned(),
            capture: None,
        }),
    }
}

fn cold_runtime() -> Runtime {
    let identity = ArtifactIdentity::new("test-runtime").unwrap();
    Runtime {
        openvmm: absolute(Path::new("missing-openvmm")).unwrap(),
        kernel: absolute(Path::new("missing-kernel")).unwrap(),
        initramfs: absolute(Path::new("missing-initramfs")).unwrap(),
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

#[cfg(target_os = "linux")]
#[test]
fn cold_creation_refuses_listener_delimiters_before_creating_outputs() {
    for delimiter in [',', '='] {
        for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(format!("vm{delimiter}option"));
            create_private_dir(&path).unwrap();
            let vm = VmDir::open(&path).unwrap();
            let input = path.join("caller-input");
            fs::write(&input, b"borrowed input").unwrap();
            let config = VmConfig {
                console,
                ..cold_config()
            };
            let error = create(&cold_runtime(), &config, None, &vm).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert_eq!(fs::read(&input).unwrap(), b"borrowed input");
            assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cold_creation_checks_socket_path_byte_limits_before_creating_outputs() {
    use std::os::unix::net::UnixListener;

    for socket_bytes in [107usize, 108, 109] {
        for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
            let directory = tempfile::tempdir().unwrap();
            let name_bytes = socket_bytes
                .checked_sub(directory.path().as_os_str().len() + CONTROL.len() + 2)
                .expect("a short temporary directory is needed for the socket boundary");
            let prefix = "unicode space ";
            let remaining = name_bytes.checked_sub(prefix.len()).unwrap();
            let mut name = format!("{prefix}{}", "\u{e9}".repeat(remaining / 2));
            name.push_str(&"x".repeat(remaining % 2));
            let path = directory.path().join(name);
            create_private_dir(&path).unwrap();
            let vm = VmDir::open(&path).unwrap();
            let outputs = vm.outputs(console == ConsoleAttachment::Listener).unwrap();
            assert_eq!(outputs.endpoint.len(), socket_bytes);
            assert!(outputs.endpoint.encode_utf16().count() < socket_bytes);
            let input = path.join("caller-input");
            fs::write(&input, b"borrowed input").unwrap();
            let config = VmConfig {
                console,
                ..cold_config()
            };
            let result = create(&cold_runtime(), &config, None, &vm);
            if socket_bytes == 107 {
                let prepared = result.unwrap();
                for endpoint in std::iter::once(&prepared.outputs().endpoint)
                    .chain(prepared.outputs().console.as_ref())
                {
                    let listener = UnixListener::bind(endpoint).unwrap();
                    assert_eq!(
                        listener.local_addr().unwrap().as_pathname(),
                        Some(Path::new(endpoint))
                    );
                }
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
                assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
            }
            assert_eq!(fs::read(&input).unwrap(), b"borrowed input");
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cold_creation_checks_canonical_socket_byte_limits_through_ancestor_symlinks() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    for socket_bytes in [107usize, 108, 109] {
        for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
            let directory = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(directory.path()).unwrap();
            let name_bytes = socket_bytes
                .checked_sub(root.as_os_str().len() + CONTROL.len() + 5)
                .expect("a short temporary directory is needed for the socket boundary");
            let prefix = "unicode space,= ";
            let remaining = name_bytes.checked_sub(prefix.len()).unwrap();
            let mut name = format!("{prefix}{}", "\u{e9}".repeat(remaining / 2));
            name.push_str(&"x".repeat(remaining % 2));
            let target = root.join(name);
            fs::create_dir(&target).unwrap();
            let path = target.join("vm");
            create_private_dir(&path).unwrap();
            let alias = root.join("alias");
            symlink(&target, &alias).unwrap();
            let vm = VmDir::open(&alias.join("vm")).unwrap();
            let outputs = vm.outputs(console == ConsoleAttachment::Listener).unwrap();
            assert!(outputs.endpoint.len() < socket_bytes);
            assert_eq!(path.join(CONTROL).as_os_str().len(), socket_bytes);
            let input = path.join("caller-input");
            fs::write(&input, b"borrowed input").unwrap();
            let config = VmConfig {
                console,
                ..cold_config()
            };
            let result = create(&cold_runtime(), &config, None, &vm);
            if socket_bytes == 107 {
                let prepared = result.unwrap();
                assert_eq!(prepared.outputs(), &outputs);
                for name in std::iter::once(CONTROL)
                    .chain(prepared.outputs().console.as_ref().map(|_| CONSOLE))
                {
                    let resolved = path.join(name);
                    let listener = UnixListener::bind(&resolved).unwrap();
                    assert_eq!(
                        listener.local_addr().unwrap().as_pathname(),
                        Some(resolved.as_path())
                    );
                }
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
                assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
            }
            assert_eq!(fs::read(&input).unwrap(), b"borrowed input");
            assert!(
                fs::symlink_metadata(&alias)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cold_creation_refuses_non_unicode_canonical_sockets_through_ancestor_symlinks() {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
        let directory = tempfile::tempdir().unwrap();
        let target = directory
            .path()
            .join(OsString::from_vec(b"parent-\xff".to_vec()));
        fs::create_dir(&target).unwrap();
        let path = target.join("vm");
        create_private_dir(&path).unwrap();
        let alias = directory.path().join("alias");
        symlink(&target, &alias).unwrap();
        let vm = VmDir::open(&alias.join("vm")).unwrap();
        assert!(vm.path().to_str().is_some());
        assert!(fs::canonicalize(vm.path()).unwrap().to_str().is_none());
        let input = path.join("caller-input");
        fs::write(&input, b"borrowed input").unwrap();
        let config = VmConfig {
            console,
            ..cold_config()
        };
        let error = create(&cold_runtime(), &config, None, &vm).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(error.to_string().contains("Unicode"), "{error}");
        assert_eq!(fs::read(&input).unwrap(), b"borrowed input");
        assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
        assert!(
            fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
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

#[cfg(any(target_os = "linux", windows))]
fn stand_in_command() -> (PathBuf, Vec<OsString>) {
    #[cfg(target_os = "linux")]
    let command = (PathBuf::from("/bin/sleep"), vec![OsString::from("60")]);
    #[cfg(windows)]
    let command = (
        PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32\\PING.EXE"),
        ["-n", "60", "127.0.0.1"]
            .into_iter()
            .map(OsString::from)
            .collect(),
    );
    command
}

#[cfg(any(target_os = "linux", windows))]
fn stand_in(dir: &VmDir) -> Prepared {
    let (program, arguments) = stand_in_command();
    let runtime = Runtime {
        openvmm: program,
        ..cold_runtime()
    };
    let mut prepared = create(&runtime, &cold_config(), None, dir).unwrap();
    prepared.arguments = arguments;
    prepared
}

#[cfg(any(target_os = "linux", windows))]
struct RunningVm(Launched);

#[cfg(any(target_os = "linux", windows))]
impl Drop for RunningVm {
    fn drop(&mut self) {
        let _ = self.0.process.kill();
        let _ = self
            .0
            .process
            .wait(Instant::now() + Duration::from_secs(10));
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn spawn_resolves_relative_runtime_and_block_inputs_outside_the_vm_directory() {
    let cwd = std::env::current_dir().unwrap();
    let inputs = tempfile::tempdir_in(&cwd).unwrap();
    let relative = inputs.path().strip_prefix(&cwd).unwrap();
    let (program, arguments) = stand_in_command();
    let executable = format!("stand-in{}", std::env::consts::EXE_SUFFIX);
    fs::copy(program, inputs.path().join(&executable)).unwrap();
    for name in ["kernel", "initramfs", "image.erofs", "scratch.raw"] {
        fs::write(inputs.path().join(name), name.as_bytes()).unwrap();
    }
    let root = |image, scratch| {
        RootConfig::Layered(LayeredRoot {
            image,
            format: RootImage::erofs(None),
            gpt_first_partition_bound: None,
            upper: UpperConfig::Scratch {
                path: scratch,
                journal: Journal::Forbidden,
            },
            overlay: OverlayMode::TrustedLowers,
        })
    };
    let runtime = Runtime {
        openvmm: relative.join(&executable),
        kernel: relative.join("kernel"),
        initramfs: relative.join("initramfs"),
        ..cold_runtime()
    };
    let config = VmConfig {
        root: root(relative.join("image.erofs"), relative.join("scratch.raw")),
        ..cold_config()
    };
    let (_directory, vm) = vm_dir();
    let mut prepared = create(&runtime, &config, None, &vm).unwrap();
    let resolved_runtime = Runtime {
        openvmm: inputs.path().join(&executable),
        kernel: inputs.path().join("kernel"),
        initramfs: inputs.path().join("initramfs"),
        ..runtime
    };
    let resolved_config = VmConfig {
        root: root(
            inputs.path().join("image.erofs"),
            inputs.path().join("scratch.raw"),
        ),
        ..config
    };
    assert_eq!(prepared.program, resolved_runtime.openvmm);
    assert_eq!(
        prepared.arguments(),
        resolved_config
            .openvmm_arguments(
                &resolved_runtime,
                &prepared.outputs().endpoint,
                &prepared.outputs().outcome,
                prepared.outputs().console.as_deref(),
            )
            .unwrap()
    );
    prepared.arguments = arguments;
    let launched = RunningVm(spawn(prepared).unwrap());
    assert!(!launched.0.process.has_exited().unwrap());
}

#[test]
fn resolving_inputs_preserves_required_path_errors_and_log_rollback() {
    for kernel in [PathBuf::new(), PathBuf::from("bad\0kernel")] {
        let (_directory, vm) = vm_dir();
        let runtime = Runtime {
            kernel,
            ..cold_runtime()
        };
        let error = create(&runtime, &cold_config(), None, &vm).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(error.to_string().contains("Runtime.kernel"), "{error}");
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 0);
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn spawn_returns_the_same_outputs_and_capability_with_a_verified_process() {
    let (_directory, vm) = vm_dir();
    let prepared = stand_in(&vm);
    let outputs = prepared.outputs().clone();
    let capability = *prepared.capability().as_bytes();
    let launched = RunningVm(spawn(prepared).unwrap());
    assert_eq!(launched.0.outputs, outputs);
    assert_eq!(launched.0.capability.as_bytes(), &capability);
    let reference = launched.0.process.reference();
    assert_ne!(reference.pid, std::process::id());
    let reopened = Process::open(reference).unwrap().unwrap();
    assert_eq!(reopened.reference(), reference);
    assert!(!reopened.has_exited().unwrap());
    assert!(!format!("{:?}", launched.0).contains(&format!("{capability:?}")));
    assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 1);
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_failure_after_child_creation_terminates_only_that_child_and_confirms_its_exit() {
    let (_directory, vm) = vm_dir();
    let (_foreign_directory, foreign_vm) = vm_dir();
    let foreign = RunningVm(spawn(stand_in(&foreign_vm)).unwrap());
    let mut process = None;
    let error = spawn_verified(stand_in(&vm), |child| {
        process = Some(child.process().unwrap());
        assert!(!process.as_ref().unwrap().has_exited().unwrap());
        Err(io::Error::other("injected process verification failure"))
    })
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        std::error::Error::source(&error).unwrap().to_string(),
        "injected process verification failure"
    );
    assert!(process.unwrap().has_exited().unwrap());
    assert!(!foreign.0.process.has_exited().unwrap());
    assert!(vm.path().join(LOG).exists());
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn spawn_failure_keeps_caller_owned_files_and_reports_the_host_error() {
    let (_directory, vm) = vm_dir();
    let mut prepared = stand_in(&vm);
    prepared.program = vm.path().join("absent-openvmm");
    let input = vm.path().join("caller-input");
    fs::write(&input, b"caller-owned").unwrap();
    let error = spawn(prepared).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    let source = std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<io::Error>()
        .unwrap();
    assert_eq!(source.kind(), io::ErrorKind::NotFound);
    assert_eq!(fs::read(&input).unwrap(), b"caller-owned");
    assert!(vm.path().join(LOG).exists());
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn spawn_passes_the_opened_log_and_owned_resources_without_reopening_paths() {
    let (_directory, vm) = vm_dir();
    let mut prepared = stand_in(&vm);
    let original_log = vm.path().join("original-log");
    fs::rename(&prepared.outputs.log, &original_log).unwrap();
    fs::write(&prepared.outputs.log, b"replacement-log").unwrap();
    let input = vm.path().join("caller-input");
    let file = File::create(&input).unwrap();
    let retained = file.try_clone().unwrap();
    #[cfg(target_os = "linux")]
    let value = std::os::fd::AsRawFd::as_raw_fd(&file);
    #[cfg(windows)]
    let value = std::os::windows::io::AsRawHandle::as_raw_handle(&file) as usize;
    prepared.inherit(file);
    let original_input = vm.path().join("original-input");
    fs::rename(&input, &original_input).unwrap();
    fs::write(&input, b"replacement-input").unwrap();
    let launched = RunningVm(
        spawn_verified(prepared, |child| {
            #[cfg(target_os = "linux")]
            {
                let info = fs::read_to_string(format!("/proc/self/fdinfo/{value}")).unwrap();
                let flags = info
                    .lines()
                    .find_map(|line| line.strip_prefix("flags:"))
                    .map(|flags| u32::from_str_radix(flags.trim(), 8).unwrap())
                    .unwrap();
                assert_ne!(flags & nix::libc::O_CLOEXEC as u32, 0);
            }
            child.process()
        })
        .unwrap(),
    );
    let pid = launched.0.process.reference().pid;
    #[cfg(target_os = "linux")]
    {
        let object = |descriptor| fs::read_link(format!("/proc/{pid}/fd/{descriptor}")).unwrap();
        assert_eq!(object(value), original_input);
        assert_eq!(object(1), original_log);
        assert_eq!(object(2), original_log);
        drop(retained);
    }
    #[cfg(windows)]
    {
        assert!(platform::tests::inherits_file(pid, value, &retained));
        let deadline = Instant::now() + Duration::from_secs(10);
        while fs::metadata(&original_log).unwrap().len() == 0 {
            assert!(
                Instant::now() < deadline,
                "the opened log received no output"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    assert_eq!(fs::read(&input).unwrap(), b"replacement-input");
    assert_eq!(
        fs::read(&launched.0.outputs.log).unwrap(),
        b"replacement-log"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn library_spawns_preserve_the_callers_umask_and_clear_only_the_childs_owner_bits() {
    let umask_in = |path: &str| {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("Umask:"))
            .map(|mask| u32::from_str_radix(mask.trim(), 8).unwrap())
            .unwrap()
    };
    for mask in [
        0o002, 0o022, 0o027, 0o077, 0o100, 0o200, 0o400, 0o477, 0o777,
    ] {
        let (_directory, vm) = vm_dir();
        let launched = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    nix::sched::unshare(nix::sched::CloneFlags::CLONE_FS).unwrap();
                    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(mask));
                    let launched = RunningVm(spawn(stand_in(&vm)).unwrap());
                    assert_eq!(umask_in("/proc/thread-self/status"), mask);
                    launched
                })
                .join()
                .unwrap()
        });
        assert_eq!(
            umask_in(&format!(
                "/proc/{}/status",
                launched.0.process.reference().pid
            )),
            mask & 0o077
        );
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_library_vm_outlives_the_caller_that_spawned_it_without_retaining_its_output_pipe() {
    let directory = tempfile::tempdir().unwrap();
    let vm = directory.path().join("vm");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "host::vm::tests::spawn_caller_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("NVX_VM_SPAWN_CALLER_DIR", &vm)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reference =
        serde_json::from_slice(&fs::read(vm.join("caller-process.json")).unwrap()).unwrap();
    let process = Process::open(&reference).unwrap().unwrap();
    let launched = RunningVm(Launched {
        process,
        outputs: VmDir::open(&vm).unwrap().outputs(false).unwrap(),
        capability: Capability([0; CAPABILITY_LEN]),
        #[cfg(feature = "cli")]
        child: ForegroundChild(None),
    });
    assert!(!launched.0.process.has_exited().unwrap());
}

#[cfg(all(feature = "cli", any(target_os = "linux", windows)))]
#[test]
fn an_owned_child_status_is_kept_when_its_outcome_is_missing_or_malformed() {
    for malformed in [false, true] {
        let (_directory, dir) = vm_dir();
        let mut prepared = stand_in(&dir).for_foreground();
        #[cfg(target_os = "linux")]
        {
            prepared.program = PathBuf::from("/bin/sh");
            prepared.arguments = ["-c", "exit 37"].map(OsString::from).to_vec();
        }
        #[cfg(windows)]
        {
            prepared.program = PathBuf::from(std::env::var_os("ComSpec").unwrap());
            prepared.arguments = ["/d", "/c", "exit 37"].map(OsString::from).to_vec();
        }
        let mut launched = RunningVm(spawn(prepared).unwrap());
        assert_eq!(
            launched.0.wait_status(Duration::from_secs(10)).unwrap(),
            Some(37)
        );
        if malformed {
            fs::write(&launched.0.outputs.outcome, b"not-json").unwrap();
        }
        let now = Instant::now();
        let stopped = stop(
            &launched.0.process,
            &launched.0.outputs,
            |_| false,
            now,
            now,
        )
        .unwrap();
        assert!(stopped.self_exited);
        assert_eq!(stopped.report.is_err(), malformed);
        assert_eq!(launched.0.wait_status(Duration::ZERO).unwrap(), Some(37));
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
#[ignore = "invoked in a separate caller by the caller-survival test"]
fn spawn_caller_helper() {
    let path = PathBuf::from(std::env::var_os("NVX_VM_SPAWN_CALLER_DIR").unwrap());
    create_private_dir(&path).unwrap();
    let dir = VmDir::open(&path).unwrap();
    let launched = spawn(stand_in(&dir)).unwrap();
    let reference = serde_json::to_vec(launched.process.reference()).unwrap();
    if let Err(error) = fs::write(path.join("caller-process.json"), reference) {
        launched.process.kill().unwrap();
        assert!(
            launched
                .process
                .wait(Instant::now() + Duration::from_secs(10))
                .unwrap()
                .is_some()
        );
        panic!("cannot record the helper's child: {error}");
    }
    drop(launched);
}

#[cfg(windows)]
#[test]
fn library_spawns_refuse_taken_and_untrusted_log_job_names() {
    for trusted in [true, false] {
        let (_directory, vm) = vm_dir();
        let prepared = stand_in(&vm);
        let held = if trusted {
            platform::create_log_job(prepared.log()).unwrap().unwrap()
        } else {
            platform::tests::untrusted_log_job(prepared.log())
        };
        let error =
            spawn_verified(prepared, |_| panic!("a refused job started a child")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io);
        let source = std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(
            source.kind(),
            if trusted {
                io::ErrorKind::AlreadyExists
            } else {
                io::ErrorKind::PermissionDenied
            }
        );
        drop(held);
    }
}

#[cfg(windows)]
#[test]
fn an_unrelated_process_started_in_the_library_spawn_window_gets_no_vm_claim() {
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let (_directory, vm) = vm_dir();
    let prepared = stand_in(&vm);
    let log = prepared.outputs.log.clone();
    let identity = platform::file_identity(&log).unwrap();
    let program = prepared.program.clone();
    let arguments = prepared.arguments.clone();
    let (entered, in_window) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        platform::tests::with_spawn_window(
            move || {
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(10)).unwrap();
            },
            || spawn(prepared),
        )
    });
    in_window.recv_timeout(Duration::from_secs(10)).unwrap();
    let mut other = ChildGuard(
        Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    release.send(()).unwrap();
    let launched = RunningVm(thread.join().unwrap().unwrap());
    assert!(platform::log_claim_held(&log, identity).unwrap());
    launched.0.process.kill().unwrap();
    assert!(
        launched
            .0
            .process
            .wait(Instant::now() + Duration::from_secs(10))
            .unwrap()
            .is_some()
    );
    assert!(
        platform::tests::claim_released_within(&log, identity, Duration::from_secs(10)),
        "an unrelated process retained the VM's job claim"
    );
    assert!(other.0.try_wait().unwrap().is_none());
}

#[cfg(any(target_os = "linux", windows))]
#[test]
#[ignore = "needs exact current OpenVMM, kernel and initramfs paths and an available hypervisor"]
fn current_cold_spawn_boots_a_guest_accepts_shell_input_and_exits_cleanly() {
    use sha2::{Digest, Sha256};

    let path =
        |name| PathBuf::from(std::env::var_os(name).expect("missing explicit VM proof input"));
    let openvmm = path("NVX_VM_SPAWN_OPENVMM");
    let kernel = path("NVX_VM_SPAWN_KERNEL");
    let initramfs = path("NVX_VM_SPAWN_INITRAMFS");
    let identity = |path: &Path| {
        ArtifactIdentity::sha256(&format!("{:x}", Sha256::digest(fs::read(path).unwrap()))).unwrap()
    };
    let runtime = Runtime {
        identity: RuntimeIdentity::new(identity(&openvmm), identity(&kernel), identity(&initramfs)),
        openvmm,
        kernel,
        initramfs,
        hypervisor: Hypervisor::from_env_or_default().unwrap(),
        cpu_profile: Some("auto".to_owned()),
        ..cold_runtime()
    };
    let path = path("NVX_VM_SPAWN_PROOF_DIR");
    assert!(
        !path.exists(),
        "a cold-spawn proof needs a fresh owned directory"
    );
    create_private_dir(&path).unwrap();
    let vm = VmDir::open(&path).unwrap();
    let config = VmConfig {
        boot: Some(Boot {
            memory_capacity_mib: None,
            command_line: "quiet loglevel=0".to_owned(),
            capture: None,
        }),
        ..cold_config()
    };
    let prepared = create(&runtime, &config, None, &vm).unwrap();
    let capability = *prepared.capability().as_bytes();
    let launched = RunningVm(spawn(prepared).unwrap());
    assert_eq!(launched.0.capability.as_bytes(), &capability);
    let reference = launched.0.process.reference();
    let reopened = Process::open(reference).unwrap().unwrap();
    assert_eq!(reopened.reference(), reference);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut console = loop {
        match platform::connect_endpoint(
            launched.0.outputs.console.as_deref().unwrap(),
            reference.pid,
            Duration::from_millis(50),
        ) {
            Ok(console) => break console,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::WouldBlock
                ) && Instant::now() < deadline =>
            {
                assert!(!launched.0.process.has_exited().unwrap());
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("cannot attach the spawned VM's console: {error}"),
        }
    };
    let read_until = |console: &mut dyn platform::Transport, marker: &[u8]| {
        let mut received = Vec::new();
        while !received.windows(marker.len()).any(|part| part == marker) {
            assert!(
                Instant::now() < deadline,
                "guest output: {}",
                String::from_utf8_lossy(&received)
            );
            let mut buffer = [0; 4096];
            match console.read(&mut buffer, Some(Duration::from_millis(100))) {
                Ok(0) => panic!("console ended before {}", String::from_utf8_lossy(marker)),
                Ok(count) => received.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
                Err(error) => panic!("cannot read the guest console: {error}"),
            }
            assert!(received.len() <= 1024 * 1024);
        }
        println!("{}", String::from_utf8_lossy(&received));
    };
    read_until(&mut *console, b"NVX-GUEST-BOOT-OK: alpine");
    console
        .write_all(
            b"printf 'NVX-SPAWN-COLD-%s\\n' OK; /sbin/nvx-exit 0\n",
            Some(Duration::from_secs(5)),
        )
        .unwrap();
    read_until(&mut *console, b"NVX-SPAWN-COLD-OK");
    assert!(launched.0.process.wait(deadline).unwrap().is_some());
    assert!(reopened.has_exited().unwrap());
    let outcome = crate::host::report::read(&launched.0.outputs.outcome)
        .unwrap()
        .unwrap();
    assert_eq!(outcome.category.as_deref(), Some("success"));
    assert_eq!(outcome.status_code, Some(0));
    println!("NVX-LIBRARY-COLD-SPAWN-EXIT-OK: {}", reference.pid);
}

#[cfg(any(target_os = "linux", windows))]
struct StopChild {
    child: std::process::Child,
    process: Process,
}

#[cfg(any(target_os = "linux", windows))]
impl StopChild {
    fn start() -> Self {
        use std::io::{BufRead, BufReader};

        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new(
                PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32\\cmd.exe"),
            );
            command.args(["/d", "/q", "/c", "echo ready & set /p wait="]);
            command
        };
        #[cfg(target_os = "linux")]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "printf 'ready\\n'; read -r wait"]);
            command
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = crate::host::tests::spawn_locked(&mut command).unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim_end(), "ready");
        let reference = crate::host::process::ProcessRef {
            pid: child.id(),
            start_time: platform::process_start_time(child.id()).unwrap().unwrap(),
            boot: crate::host::process::BootId::current().unwrap(),
        };
        let process = Process::open(&reference).unwrap().unwrap();
        Self { child, process }
    }
}

#[cfg(any(target_os = "linux", windows))]
impl Drop for StopChild {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn stop_preserves_report_results_after_a_real_self_exit() {
    for report in ["missing", "valid", "malformed", "unreadable"] {
        let (_directory, vm) = vm_dir();
        let outputs = vm.outputs(false).unwrap();
        match report {
            "missing" => {}
            "valid" => fs::write(
                &outputs.outcome,
                br#"{"outcome":{"category":"success","status_code":0},"teardown":{"workers":true}}"#,
            ).unwrap(),
            "malformed" => fs::write(&outputs.outcome, b"{ truncated").unwrap(),
            "unreadable" => fs::create_dir(&outputs.outcome).unwrap(),
            _ => unreachable!(),
        }
        let mut child = StopChild::start();
        let deadline = Instant::now() + Duration::from_secs(10);
        let stopped = stop(
            &child.process,
            &outputs,
            |supplied| {
                assert_eq!(supplied, deadline);
                drop(child.child.stdin.take());
                false
            },
            deadline,
            deadline,
        )
        .unwrap();
        assert!(stopped.self_exited);
        assert!(!stopped.forced);
        assert!(child.process.wait(Instant::now()).unwrap().is_some());
        assert!(child.child.try_wait().unwrap().is_some());
        let already_exited = stop(
            &child.process,
            &outputs,
            |_| panic!("an exited process needs no graceful request"),
            Instant::now(),
            Instant::now(),
        )
        .unwrap();
        assert!(already_exited.self_exited);
        assert!(!already_exited.forced);
        match report {
            "missing" => assert!(stopped.report.unwrap().is_none()),
            "valid" => assert_eq!(
                stopped.report.unwrap(),
                Some(Report {
                    category: Some("success".to_owned()),
                    status_code: Some(0),
                    teardown_complete: Some(true),
                })
            ),
            _ => {
                let error = stopped.report.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Io);
                assert!(std::error::Error::source(&error).is_some());
            }
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn stop_forces_only_its_verified_process_after_grace() {
    let (_directory, vm) = vm_dir();
    let outputs = vm.outputs(false).unwrap();
    let mut child = StopChild::start();
    let mut other = StopChild::start();
    let grace = Instant::now() + Duration::from_millis(80);
    let deadline = grace + Duration::from_secs(10);
    let stopped = stop(
        &child.process,
        &outputs,
        |supplied| {
            assert_eq!(supplied, grace);
            true
        },
        grace,
        deadline,
    )
    .unwrap();
    assert!(!stopped.self_exited);
    assert!(stopped.forced);
    assert!(stopped.report.unwrap().is_none());
    assert!(child.process.wait(Instant::now()).unwrap().is_some());
    assert!(child.child.try_wait().unwrap().is_some());
    assert!(other.process.wait(Instant::now()).unwrap().is_none());
    assert!(other.child.try_wait().unwrap().is_none());
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn stop_returns_still_running_without_exit_confirmation_at_its_deadline() {
    let (_directory, vm) = vm_dir();
    let outputs = vm.outputs(false).unwrap();
    fs::write(&outputs.outcome, b"{ truncated").unwrap();
    let mut child = StopChild::start();
    #[cfg(target_os = "linux")]
    let handle = platform::ProcessHandle::without_pidfd();
    #[cfg(windows)]
    let handle = platform::ProcessHandle::wait_only(child.child.id());
    let process = Process::held(child.process.reference().clone(), handle);
    let grace = Instant::now();
    let deadline = grace + Duration::from_millis(80);
    let error = stop(&process, &outputs, |_| false, grace, deadline).unwrap_err();
    assert!(Instant::now() >= deadline);
    assert_eq!(error.kind(), ErrorKind::StillRunning);
    assert_eq!(error.code().as_str(), "still-running");
    let cause = std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<io::Error>()
        .unwrap();
    assert_eq!(
        cause.kind(),
        if cfg!(windows) {
            io::ErrorKind::PermissionDenied
        } else {
            io::ErrorKind::Unsupported
        }
    );
    assert!(process.wait(Instant::now()).unwrap().is_none());
    assert!(child.child.try_wait().unwrap().is_none());
}
