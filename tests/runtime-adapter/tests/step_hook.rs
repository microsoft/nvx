//! The host program's step hook, for tests: the values of `RUNTIME_ADAPTER_STEP_HOOK` that set it,
//! and those that the host program refuses, and a pause, which waits until its file is gone. The
//! tests need no hypervisor; the VM tests pause launches with the hook, and end them.
#![cfg(feature = "host")]

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use runtime_adapter::hook::{Action, HOOK_EXIT, STEP_HOOK, Step, StepHook};

#[test]
fn a_step_hook_names_a_step_and_what_it_does_there() {
    assert_eq!(
        Step::ALL.map(Step::as_str),
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
            "names the step \"\", not one of checked, spawned, ready, commit",
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

    // At another step, the hook does nothing.
    hook.at(Step::Checked).unwrap();
    assert!(!path.exists());

    // At its own, it creates its file, which holds the step's name, and returns once the file is
    // gone.
    let paused = thread::spawn({
        let hook = hook.clone();
        move || hook.at(Step::Spawned)
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while fs::read_to_string(&path).unwrap_or_default() != "spawned" {
        assert!(Instant::now() < deadline, "the hook did not pause");
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(100));
    assert!(!paused.is_finished(), "the hook did not wait");
    fs::remove_file(&path).unwrap();
    paused.join().unwrap().unwrap();

    // A pause file that is there already fails the pause.
    fs::write(&path, "").unwrap();
    let error = hook.at(Step::Spawned).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
}

#[test]
fn the_host_program_refuses_a_launch_whose_step_hook_it_cannot_read() {
    for (value, problem) in [
        ("nowhere:exit", "names the step \"nowhere\""),
        ("ready:pause", "takes STEP:pause:PATH or STEP:exit"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"))
            .args(["launch", "store", "--guest-agent", "agent"])
            .env(STEP_HOOK, value)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{value:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(problem) && stderr.contains("usage: runtime-adapter-host"),
            "{value:?}: {stderr}"
        );
    }
}
