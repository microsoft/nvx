use std::error::Error as _;

use super::*;
use crate::ErrorKind;
use crate::activation::{MachineIdMode, View};
use crate::host::cli::store::{files, record};
use crate::host::config::{Boot, ConsoleAttachment, RootConfig, VmConfig};
use crate::host::process::{BootId, ProcessRef};
use crate::host::runtime::{ArtifactIdentity, LaunchOptions, Runtime, RuntimeIdentity};
use crate::host::vm::{self, Prepared, SavedVm, VmDir};
use crate::hypervisor::Hypervisor;
use crate::protocol::CAPABILITY_LEN;

pub(super) fn prepared(
    state: &StateDir,
    console: ConsoleAttachment,
) -> (Prepared, RuntimeIdentity) {
    let identity = |text| ArtifactIdentity::new(text).unwrap();
    let runtime = Runtime {
        openvmm: "missing-openvmm".into(),
        kernel: "missing-kernel".into(),
        initramfs: "missing-initramfs".into(),
        identity: RuntimeIdentity {
            openvmm: identity("vmm-revision"),
            kernel: identity("kernel-revision"),
            initramfs: identity("initramfs-revision"),
            boot_contract: u64::MAX,
            control_contract: "saved-control-revision".to_owned(),
            control_session: u64::MAX - 1,
        },
        hypervisor: if cfg!(windows) {
            Hypervisor::Whp
        } else {
            Hypervisor::Mshv
        },
        cpu_profile: None,
        windows_breakaway: false,
        options: LaunchOptions::default(),
    };
    let config = VmConfig {
        processors: 1,
        memory_mib: 256,
        root: RootConfig::Blockless,
        view: View {
            hostname: b"journal-test".to_vec(),
            machine_id: MachineIdMode::Image,
        },
        network: None,
        filesystem: None,
        console,
        boot: Some(Boot {
            memory_capacity_mib: None,
            command_line: "nvx_entrypoint=/bin/sh".to_owned(),
            capture: None,
        }),
    };
    let vm_dir = VmDir::open(state.dir()).unwrap();
    (
        vm::create(&runtime, &config, None, &vm_dir).unwrap(),
        runtime.identity,
    )
}

fn saved(state: &StateDir, prepared: &Prepared, runtime: RuntimeIdentity) -> SavedVm {
    SavedVm {
        vm_dir: VmDir::open(state.dir()).unwrap(),
        endpoint: prepared.outputs().endpoint.clone(),
        capability: *prepared.capability().as_bytes(),
        process: ProcessRef {
            pid: u32::MAX,
            start_time: u64::MAX,
            boot: BootId::from_bytes([0xa5; 16]),
        },
        runtime,
    }
}

fn assert_saved(actual: &SavedVm, expected: &SavedVm) {
    assert_eq!(actual.vm_dir, expected.vm_dir);
    assert_eq!(actual.endpoint, expected.endpoint);
    assert_eq!(actual.capability, expected.capability);
    assert_eq!(actual.process, expected.process);
    assert_eq!(actual.runtime, expected.runtime);
}

fn read_file(locked: &locked::LockedState, name: StateFile) -> crate::Result<bool> {
    match name {
        StateFile::Journal => locked.journal().map(|record| record.is_some()),
        StateFile::Runtime => locked.runtime().map(|record| record.is_some()),
        StateFile::Capability => locked.capability().map(|record| record.is_some()),
        _ => unreachable!(),
    }
}

#[test]
fn the_pre_spawn_barrier_and_commit_preserve_every_prepared_and_saved_value() {
    for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
        let (_directory, state) = state();
        let (prepared, runtime) = prepared(&state, console);
        let saved = saved(&state, &prepared, runtime.clone());
        let mut locked = state.lock(Deadline::after(10.0)).unwrap();
        let mut launch = locked.begin_launch(&prepared, &runtime).unwrap();
        let spawn_hook = || {
            let bytes = fs::read(state.path(StateFile::Journal)).unwrap();
            let journal: record::LaunchJournal = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(journal.vm_dir, saved.vm_dir.path());
            assert_eq!(journal.capability, saved.capability);
            assert_eq!(journal.log, prepared.outputs().log);
            assert_eq!(journal.outcome, prepared.outputs().outcome);
            assert_eq!(journal.endpoint, prepared.outputs().endpoint);
            assert_eq!(journal.console, prepared.outputs().console);
            assert_eq!(journal.runtime, runtime);
            assert_eq!(journal.process(), None);
            assert!(!format!("{journal:?}").contains(&format!("{:?}", saved.capability)));
            assert_eq!(
                fs::read(state.path(StateFile::Capability)).unwrap(),
                saved.capability
            );
            for name in [StateFile::Journal, StateFile::Capability] {
                let path = state.path(name);
                let metadata = fs::symlink_metadata(&path).unwrap();
                assert!(metadata.is_file());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
                }
                let file = fs::File::open(path).unwrap();
                assert_eq!(platform::private_file_problem(&file).unwrap(), None);
            }
            assert!(!state.path(StateFile::Runtime).exists());
        };
        spawn_hook();
        launch.record_process(&saved.process).unwrap();
        let journal: record::LaunchJournal =
            serde_json::from_slice(&fs::read(state.path(StateFile::Journal)).unwrap()).unwrap();
        assert_eq!(journal.process(), Some(&saved.process));
        assert!(!state.path(StateFile::Runtime).exists());
        launch.commit(&saved).unwrap();
        assert!(!state.path(StateFile::Journal).exists());
        assert_saved(&locked.runtime().unwrap().unwrap(), &saved);
        assert_eq!(locked.capability().unwrap(), Some(saved.capability));
        drop(locked);
        drop(prepared);
        let reopened = StateDir::open(state.dir())
            .unwrap()
            .lock(Deadline::after(10.0))
            .unwrap();
        assert_saved(&reopened.runtime().unwrap().unwrap(), &saved);
        for name in [StateFile::Runtime, StateFile::Capability] {
            let path = state.path(name);
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(metadata.is_file());
            assert!(!crate::host::filesystem::is_reparse_point(&metadata));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
            }
            let file = fs::File::open(path).unwrap();
            assert_eq!(platform::private_file_problem(&file).unwrap(), None);
        }
    }
}

#[test]
fn an_interruption_keeps_the_journal_and_capability_without_publishing_runtime() {
    for spawned in [false, true] {
        let (_directory, state) = state();
        let (prepared, runtime) = prepared(&state, ConsoleAttachment::Listener);
        let saved = saved(&state, &prepared, runtime.clone());
        let mut locked = state.lock(Deadline::after(10.0)).unwrap();
        {
            let mut launch = locked.begin_launch(&prepared, &runtime).unwrap();
            if spawned {
                launch.record_process(&saved.process).unwrap();
            }
        }
        let before = fs::read(state.path(StateFile::Journal)).unwrap();
        assert!(locked.begin_launch(&prepared, &runtime).is_err());
        assert_eq!(fs::read(state.path(StateFile::Journal)).unwrap(), before);
        drop(locked);
        let journal: record::LaunchJournal =
            serde_json::from_slice(&fs::read(state.path(StateFile::Journal)).unwrap()).unwrap();
        assert_eq!(journal.process(), spawned.then_some(&saved.process));
        assert_eq!(journal.capability, saved.capability);
        assert_eq!(
            fs::read(state.path(StateFile::Capability)).unwrap(),
            saved.capability
        );
        assert!(!state.path(StateFile::Runtime).exists());
    }
}

#[test]
fn a_commit_needs_the_same_saved_values_and_an_already_journaled_process() {
    for mismatch in 0..6 {
        let (_directory, state) = state();
        let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
        let mut saved = saved(&state, &prepared, runtime.clone());
        let mut locked = state.lock(Deadline::after(10.0)).unwrap();
        let mut launch = locked.begin_launch(&prepared, &runtime).unwrap();
        if mismatch != 0 {
            launch.record_process(&saved.process).unwrap();
            assert!(launch.record_process(&saved.process).is_err());
        }
        let original = fs::read(state.path(StateFile::Journal)).unwrap();
        match mismatch {
            0 => {}
            1 => saved.process.start_time -= 1,
            2 => saved.endpoint.push('x'),
            3 => saved.capability[0] ^= 1,
            4 => saved.runtime.control_session -= 1,
            5 => {
                let other = state.dir().join("other-vm");
                vm::create_private_dir(&other).unwrap();
                saved.vm_dir = VmDir::open(&other).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(launch.commit(&saved).is_err());
        assert!(!state.path(StateFile::Runtime).exists());
        assert_eq!(fs::read(state.path(StateFile::Journal)).unwrap(), original);
    }
}

#[test]
fn preparation_for_another_directory_and_failed_record_writes_do_not_publish_runtime() {
    let (_directory, state) = state();
    let (_other_directory, other) = super::state();
    let (other_prepared, runtime) = prepared(&other, ConsoleAttachment::Disconnected);
    let mut locked = state.lock(Deadline::after(10.0)).unwrap();
    assert!(locked.begin_launch(&other_prepared, &runtime).is_err());
    assert!(!state.path(StateFile::Journal).exists());
    assert!(!state.path(StateFile::Capability).exists());
    let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
    let saved = saved(&state, &prepared, runtime.clone());
    let mut launch = locked.begin_launch(&prepared, &runtime).unwrap();
    let journal = state.path(StateFile::Journal);
    let original = journal.with_extension("original");
    fs::rename(&journal, &original).unwrap();
    fs::create_dir(&journal).unwrap();
    let error = launch.record_process(&saved.process).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.source().is_some());
    fs::remove_dir(&journal).unwrap();
    fs::rename(&original, &journal).unwrap();
    assert!(launch.commit(&saved).is_err());
    assert!(!state.path(StateFile::Runtime).exists());
    assert!(locked.journal().unwrap().unwrap().process().is_none());
}

#[test]
fn a_failed_runtime_publication_keeps_the_spawned_journal() {
    let (_directory, state) = state();
    let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
    let saved = saved(&state, &prepared, runtime.clone());
    let mut locked = state.lock(Deadline::after(10.0)).unwrap();
    let mut launch = locked.begin_launch(&prepared, &runtime).unwrap();
    launch.record_process(&saved.process).unwrap();
    let before = fs::read(state.path(StateFile::Journal)).unwrap();
    fs::create_dir(state.path(StateFile::Runtime)).unwrap();
    let error = launch.commit(&saved).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.source().is_some());
    assert_eq!(fs::read(state.path(StateFile::Journal)).unwrap(), before);
    assert_eq!(
        fs::read_dir(state.path(StateFile::Runtime))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn a_failed_journal_removal_reports_that_runtime_already_committed() {
    let (_directory, state) = state();
    let (prepared, runtime) = prepared(&state, ConsoleAttachment::Disconnected);
    let saved = saved(&state, &prepared, runtime.clone());
    let mut locked = state.lock(Deadline::after(10.0)).unwrap();
    let mut launch = locked.begin_launch(&prepared, &runtime).unwrap();
    launch.record_process(&saved.process).unwrap();
    fs::remove_file(state.path(StateFile::Journal)).unwrap();
    fs::create_dir(state.path(StateFile::Journal)).unwrap();
    let error = launch.commit(&saved).unwrap_err();
    assert!(error.message().contains("runtime was committed"), "{error}");
    assert!(error.source().unwrap().source().is_some());
    assert_saved(&locked.runtime().unwrap().unwrap(), &saved);
    assert!(state.path(StateFile::Journal).is_dir());
}

#[test]
fn malformed_records_and_capabilities_keep_their_parse_errors() {
    let (_directory, state) = state();
    let locked = state.lock(Deadline::after(10.0)).unwrap();
    for name in [StateFile::Journal, StateFile::Runtime] {
        files::replace(&locked, name, b"{malformed").unwrap();
        let error = read_file(&locked, name).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io);
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<serde_json::Error>()
                .is_some(),
            "{error}"
        );
    }
    files::replace(&locked, StateFile::Capability, &[0x5a; CAPABILITY_LEN - 1]).unwrap();
    assert_eq!(locked.capability().unwrap_err().kind(), ErrorKind::Io);
}

#[test]
fn records_and_capabilities_refuse_non_regular_entries_and_links_before_read_or_replace() {
    for name in [
        StateFile::Journal,
        StateFile::Runtime,
        StateFile::Capability,
    ] {
        let (directory, state) = state();
        let locked = state.lock(Deadline::after(10.0)).unwrap();
        let path = state.path(name);
        fs::create_dir(&path).unwrap();
        assert!(read_file(&locked, name).is_err());
        assert!(files::replace(&locked, name, b"replacement").is_err());
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        fs::remove_dir(&path).unwrap();
        let target = directory.path().join("target");
        fs::write(&target, b"preserved target").unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&target, &path);
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&target, &path);
        match linked {
            Ok(()) => {
                assert!(read_file(&locked, name).is_err());
                assert!(files::replace(&locked, name, b"replacement").is_err());
                assert_eq!(fs::read(&target).unwrap(), b"preserved target");
                fs::remove_file(&path).unwrap();
            }
            #[cfg(windows)]
            Err(error) if error.raw_os_error() == Some(1314) => {
                eprintln!("skipped the symbolic link without Developer Mode or privilege: {error}");
            }
            Err(error) => panic!("cannot create the test symbolic link: {error}"),
        }
        #[cfg(target_os = "linux")]
        {
            use nix::sys::stat::Mode;
            nix::unistd::mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
            assert!(read_file(&locked, name).is_err());
            assert!(files::replace(&locked, name, b"replacement").is_err());
        }
        #[cfg(windows)]
        {
            let target = directory.path().join("junction-target");
            fs::create_dir(&target).unwrap();
            junction(&path, &target);
            assert!(read_file(&locked, name).is_err());
            assert!(files::replace(&locked, name, b"replacement").is_err());
            assert_eq!(fs::read_dir(target).unwrap().count(), 0);
        }
    }
}

#[cfg(windows)]
#[test]
fn records_and_capabilities_that_other_users_may_access_are_refused_without_replacement() {
    let (_directory, state) = state();
    let locked = state.lock(Deadline::after(10.0)).unwrap();
    for name in [
        StateFile::Journal,
        StateFile::Runtime,
        StateFile::Capability,
    ] {
        files::replace(&locked, name, b"preserved shared contents").unwrap();
        let path = state.path(name);
        allow_everyone(&path, "");
        let before = fs::read(&path).unwrap();
        for error in [
            read_file(&locked, name).unwrap_err(),
            files::replace(&locked, name, b"replacement").unwrap_err(),
        ] {
            assert_eq!(error.kind(), ErrorKind::Io);
            assert!(error.message().contains("allows other users"), "{error}");
        }
        assert_eq!(fs::read(path).unwrap(), before);
    }
}
