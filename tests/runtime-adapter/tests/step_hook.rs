//! The host program's step hook, for tests: the values of `RUNTIME_ADAPTER_STEP_HOOK` that set it,
//! and those that the host program refuses, a start's, which it takes at `checked` alone, and a
//! pause, which waits until its file is gone, on Windows until a pending deletion of the file
//! completes, and says how long it paused. The tests need no hypervisor; the VM tests pause
//! launches, starts, and stops with the hook, and end them.
#![cfg(feature = "host")]

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use nvx::host::artifacts::Artifacts;
use runtime_adapter::clone::HOST_TIME_VARIABLE;
use runtime_adapter::hook::{Action, HOOK_EXIT, STEP_HOOK, Step, StepHook};

#[test]
fn a_step_hook_names_a_step_and_what_it_does_there() {
    assert_eq!(
        Step::ALL.map(Step::as_str),
        ["checked", "spawned", "ready", "commit", "found"]
    );
    // A launch reaches each step but that of a command that reattaches, in order.
    assert_eq!(
        Step::LAUNCH.map(Step::as_str),
        ["checked", "spawned", "ready", "commit"]
    );
    // Each step, with each action, reads back as it is written.
    for step in Step::ALL {
        assert_eq!(Step::parse(step.as_str()), Some(step));
        for action in [Action::Exit, Action::Pause(PathBuf::from("/run/pause"))] {
            let hook = StepHook { step, action };
            assert_eq!(StepHook::parse(OsStr::new(&hook.to_string())), Ok(hook));
        }
    }
    assert_eq!(
        StepHook::parse(OsStr::new("ready:exit"))
            .unwrap()
            .to_string(),
        "ready:exit"
    );
    assert_eq!(HOOK_EXIT, 5);

    // The path of a pause may hold colons, as a Windows path does.
    let hook = StepHook::parse(OsStr::new(r"checked:pause:C:\pauses\checked")).unwrap();
    let expected = StepHook {
        step: Step::Checked,
        action: Action::Pause(PathBuf::from(r"C:\pauses\checked")),
    };
    assert_eq!(hook, expected);

    // Values that name no step, or no action.
    let usage = "takes STEP:pause:PATH or STEP:exit, not ";
    for (value, problem) in [
        (
            "",
            "names the step \"\", not one of checked, spawned, ready, commit, found",
        ),
        ("checkd:exit", "names the step \"checkd\""),
        ("Checked:exit", "names the step \"Checked\""),
        ("checked", usage),
        ("checked:", usage),
        ("checked:pause", usage),
        ("checked:pause:", usage),
        ("checked:exit:now", usage),
        ("checked:abort", usage),
    ] {
        let error = StepHook::parse(OsStr::new(value)).unwrap_err();
        assert!(
            error.starts_with(STEP_HOOK) && error.contains(problem),
            "{value:?}: {error}"
        );
    }
}

#[test]
fn a_pause_waits_until_its_file_is_gone() {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join("step-hook-pause");
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("pause");
    let hook = StepHook {
        step: Step::Spawned,
        action: Action::Pause(path.clone()),
    };

    // At another step, the hook does nothing, and pauses for no time.
    assert_eq!(hook.at(Step::Checked).unwrap(), Duration::ZERO);
    assert!(!path.exists());

    // At its own, it creates its file, which holds the step's name, and returns once the file is
    // gone, with how long it paused, which is at least as long as the file was there.
    let paused = thread::spawn({
        let hook = hook.clone();
        move || hook.at(Step::Spawned)
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while fs::read_to_string(&path).unwrap_or_default() != "spawned" {
        assert!(Instant::now() < deadline, "the hook did not pause");
        thread::sleep(Duration::from_millis(10));
    }
    let held = Duration::from_millis(100);
    thread::sleep(held);
    assert!(!paused.is_finished(), "the hook did not wait");
    fs::remove_file(&path).unwrap();
    let paused = paused.join().unwrap().unwrap();
    assert!(paused >= held, "the hook paused for {paused:?}");

    // A pause file that is there already fails the pause.
    fs::write(&path, "").unwrap();
    let error = hook.at(Step::Spawned).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
}

/// On Windows, a file whose deletion has begun stays in its directory, and refuses to be opened,
/// until the deletion completes: a pause waits until then, as a test that deletes the pause file
/// while the hook checks it would have it.
#[cfg(windows)]
#[test]
fn a_pause_waits_until_the_deletion_of_its_file_completes() {
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;

    /// The access right that deleting a file takes.
    const DELETE: u32 = 0x0001_0000;
    /// The flag that deletes a file once the handle that it opens closes.
    const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;

    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join("step-hook-pending");
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("pause");
    let hook = StepHook {
        step: Step::Commit,
        action: Action::Pause(path.clone()),
    };
    let paused = thread::spawn({
        let hook = hook.clone();
        move || hook.at(Step::Commit)
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while fs::read_to_string(&path).unwrap_or_default() != "commit" {
        assert!(Instant::now() < deadline, "the hook did not pause");
        thread::sleep(Duration::from_millis(10));
    }

    // The deletion begins once the handle that deletes the file on close closes, and completes
    // once the test closes the last handle to the file, which it holds meanwhile.
    let last = File::open(&path).unwrap();
    drop(
        OpenOptions::new()
            .access_mode(DELETE)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .open(&path)
            .unwrap(),
    );
    let error = File::open(&path).unwrap_err();
    assert_eq!(
        error.kind(),
        io::ErrorKind::PermissionDenied,
        "the deletion of the pause file is not pending: {error}"
    );
    let held = Duration::from_millis(100);
    thread::sleep(held);
    if paused.is_finished() {
        panic!(
            "the hook ended while the deletion of its file was pending: {:?}",
            paused.join().unwrap()
        );
    }
    drop(last);
    let paused = paused.join().unwrap().unwrap();
    assert!(paused >= held, "the hook paused for {paused:?}");
    assert!(!path.exists(), "the pause file is still there");
}

#[test]
fn the_host_program_refuses_a_command_whose_step_hook_it_cannot_read() {
    let commands: [&[&str]; 3] = [
        &["launch", "store", "--guest-agent", "agent"],
        &["reattach", "store"],
        &["stop", "store"],
    ];
    for command in commands {
        for (value, problem) in [
            ("nowhere:exit", "names the step \"nowhere\""),
            ("ready:pause", "takes STEP:pause:PATH or STEP:exit"),
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"))
                .args(command)
                .env(STEP_HOOK, value)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(2),
                "{command:?} {value:?}: {output:?}"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(problem) && stderr.contains("usage: runtime-adapter-host"),
                "{command:?} {value:?}: {stderr}"
            );
        }
    }
}

#[test]
fn a_start_takes_a_step_hook_at_checked_alone() {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join("step-hook-start");
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let image = directory.join("distro.erofs");
    fs::write(&image, vec![0; 4096]).unwrap();
    let store = directory.join("store");
    let start = |value: &str| {
        Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"))
            .arg("start")
            .arg(&store)
            .arg("--template")
            .arg(directory.join("templates"))
            .args(["--generation", "1", "--distro"])
            .arg(format!("erofs:{}", image.display()))
            .args([
                "--distro-identity",
                "0a490905-41be-ed5f-984e-63eb7e5e20b1",
                "--upper",
                "ram",
            ])
            .env(STEP_HOOK, value)
            .env_remove(Artifacts::ENV_DIR)
            .env_remove(HOST_TIME_VARIABLE)
            .output()
            .unwrap()
    };

    // A hook at a step that a start does not stop at is a usage error, as one that names no step.
    for (value, problem) in [
        (
            "spawned:exit",
            "names the step spawned, but a start takes the hook at checked alone",
        ),
        ("ready:exit", "names the step ready"),
        ("commit:pause:pause", "names the step commit"),
        ("found:exit", "names the step found"),
        ("nowhere:exit", "names the step \"nowhere\""),
    ] {
        let output = start(value);
        assert_eq!(output.status.code(), Some(2), "{value:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(problem) && stderr.contains("usage: runtime-adapter-host"),
            "{value:?}: {stderr}"
        );
    }

    // One at `checked` makes a start, whose selection the template store, which does not exist,
    // refuses before the start plans the clone.
    let output = start("checked:exit");
    assert_eq!(output.status.code(), Some(10), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("failure.stage=selection\n"), "{stdout}");
    assert!(!store.exists(), "the start created the sandbox's store");
}
