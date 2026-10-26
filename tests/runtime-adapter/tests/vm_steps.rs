//! The host program's step hook in VMs, on a real hypervisor, through public APIs only: a launch
//! paused after its checks while the test replaces its staged images at their paths, which never
//! reach the VM; a launch that the hook ends after its checks, as a crash would; and a launch
//! paused at each of its steps, which then goes on.
//!
//! The tests boot VMs, so they are ignored. They read what the `common` module lists, and boot its
//! fixtures: NVX's Ubuntu EROFS image over a copy of the scratch template.
//!
//! ```text
//! cargo test --locked --features host --test vm_steps -- --ignored
//! ```
#![cfg(feature = "host")]

mod common;

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::time::Instant;

use nvx::ErrorCode;
use nvx::host::process::Process;
use nvx::host::store::{LaunchState, SandboxStore};
use runtime_adapter::hook::{Action, HOOK_EXIT, STEP_HOOK, Step, StepHook};
use runtime_adapter::staged::Staged;

use common::{Fixtures, Printed, Sandbox};

/// How many bytes at the start of a replacement the tests check: the superblocks that a guest that
/// mounted it would have written, and more.
const CHECKED_BYTES: usize = 1 << 20;

/// Returns the options that stage and launch the EROFS image at `distro`, of the identity
/// `identity`, over the scratch image at `scratch`.
fn images(distro: &Path, identity: &str, scratch: &Path) -> Vec<OsString> {
    let mut image = OsString::from("erofs:");
    image.push(distro);
    let mut upper = OsString::from("scratch:");
    upper.push(scratch);
    vec![
        "--distro".into(),
        image,
        "--distro-identity".into(),
        identity.into(),
        "--upper".into(),
        upper,
    ]
}

/// Returns the value of the step hook's variable that sets the hook to do `action` at `step`.
fn hook(step: Step, action: Action) -> OsString {
    StepHook { step, action }.to_string().into()
}

/// Stops the VM that `launched` printed, and checks that it stopped gracefully, with a successful
/// outcome and a complete teardown.
fn stop(sandbox: &mut Sandbox, launched: &Printed) {
    let stopped = sandbox.run("stop", &[], 0);
    assert_eq!(stopped["pid"], launched["pid"]);
    assert_eq!(stopped["stopped"], "graceful");
    assert_eq!(stopped["outcome"], "success 0");
    assert_eq!(stopped["teardown"], "complete");
    assert_eq!(
        sandbox.record(),
        None,
        "the stop cleared the runtime record"
    );
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn images_replaced_after_the_checks_never_reach_the_vm() {
    let fixtures = Fixtures::find();
    let mut sandbox = Sandbox::new("replaced");
    // The test's own copies of the image and of the scratch template, which it replaces.
    let distro = sandbox.root.join("distro.erofs");
    let scratch = sandbox.root.join("scratch.ext4");
    fs::copy(&fixtures.distro, &distro).unwrap();
    fs::copy(&fixtures.scratch, &scratch).unwrap();
    let images = images(&distro, &fixtures.identity, &scratch);
    let images = images.iter().map(OsString::as_os_str).collect::<Vec<_>>();
    sandbox.run("stage", &images, 0);

    let pause = sandbox.root.join("checked.pause");
    let hook = hook(Step::Checked, Action::Pause(pause.clone()));
    let mut launch = sandbox.start_launch(&images, &[(STEP_HOOK, &hook)]);
    assert_eq!(sandbox.paused(&mut launch, &pause), "checked");

    // Files of zeros, of the images' lengths, which the guest could neither assemble as the image
    // nor mount as the scratch, try to take the images' paths.
    let replaced = [&distro, &scratch].map(|path| {
        let replacement = path.with_extension("replacement");
        File::create(&replacement)
            .unwrap()
            .set_len(fs::metadata(path).unwrap().len())
            .unwrap();
        (path, fs::rename(&replacement, path))
    });
    if cfg!(windows) {
        // The checked files hold the paths until the launch releases them: no other file takes
        // either path, and neither image can be deleted, nor the image opened for writing.
        for (path, replaced) in replaced {
            assert!(replaced.is_err(), "{} was replaced", path.display());
            assert!(
                fs::remove_file(path).is_err(),
                "{} was deleted",
                path.display()
            );
        }
        assert!(
            File::options().write(true).open(&distro).is_err(),
            "the image was opened for writing"
        );
    } else {
        // The replacements take the paths, but the launch hands OpenVMM the checked files.
        for (path, replaced) in replaced {
            replaced.unwrap_or_else(|error| panic!("cannot replace {}: {error}", path.display()));
        }
    }

    // The guest assembles the checked originals: it could assemble no replacement.
    fs::remove_file(&pause).unwrap();
    let launched = sandbox.finish_launch(launch, 0);
    assert_eq!(launched["root"], "ready");
    if cfg!(windows) {
        // Once the guest is ready, the launch has released the checked files: the images can be
        // renamed again, as OpenVMM's own opens of them allow.
        for path in [&distro, &scratch] {
            let moved = path.with_extension("moved");
            fs::rename(path, &moved).unwrap();
            fs::rename(&moved, path).unwrap();
        }
    }
    stop(&mut sandbox, &launched);
    if !cfg!(windows) {
        // The VM wrote nothing to the replacements that took the paths.
        for path in [&distro, &scratch] {
            let mut start = vec![0; CHECKED_BYTES];
            File::open(path).unwrap().read_exact(&mut start).unwrap();
            assert!(
                start.iter().all(|byte| *byte == 0),
                "{} was written",
                path.display()
            );
        }
    }
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn a_launch_that_the_hook_ends_after_its_checks_leaves_no_launch() {
    let fixtures = Fixtures::find();
    let mut sandbox = Sandbox::new("ended");
    let scratch = sandbox.root.join("scratch.ext4");
    fs::copy(&fixtures.scratch, &scratch).unwrap();
    let images = images(&fixtures.distro, &fixtures.identity, &scratch);
    let images = images.iter().map(OsString::as_os_str).collect::<Vec<_>>();
    sandbox.run("stage", &images, 0);

    // The hook exits the launch without unwinding once it holds the checked files.
    let hook = hook(Step::Checked, Action::Exit);
    let launch = sandbox.start_launch(&images, &[(STEP_HOOK, &hook)]);
    let ended = sandbox.finish_launch(launch, HOOK_EXIT.into());
    assert!(ended.is_empty(), "{ended:?}");
    sandbox.assert_unlaunched();

    // The system closed the checked files when the launch exited, which released the scratch
    // image's claim: the next launch claims it, and the guest assembles the root.
    let launched = sandbox.launch(&images, 0);
    assert_eq!(launched["root"], "ready");
    stop(&mut sandbox, &launched);
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn the_hook_pauses_a_launch_at_each_of_its_steps() {
    let fixtures = Fixtures::find();
    let mut sandbox = Sandbox::new("steps");
    let scratch = sandbox.root.join("scratch.ext4");
    fs::copy(&fixtures.scratch, &scratch).unwrap();
    let images = images(&fixtures.distro, &fixtures.identity, &scratch);
    let images = images.iter().map(OsString::as_os_str).collect::<Vec<_>>();
    sandbox.run("stage", &images, 0);
    let staged = Staged::read(&sandbox.store).unwrap().unwrap();
    let scratch_binding = staged.scratch.unwrap().binding;

    for step in Step::LAUNCH {
        let pause = sandbox.root.join(format!("{step}.pause"));
        let hook = hook(step, Action::Pause(pause.clone()));
        let mut launch = sandbox.start_launch(&images, &[(STEP_HOOK, &hook)]);
        assert_eq!(sandbox.paused(&mut launch, &pause), step.as_str());
        let store = SandboxStore::open(&sandbox.store).unwrap();
        if step == Step::Checked {
            // The launch holds the checked files, and with them the scratch image's claim, which
            // a check that makes one attempt finds, but has begun nothing in the store.
            sandbox.assert_unlaunched();
            let error = scratch_binding.check(&scratch, Instant::now()).unwrap_err();
            assert_eq!(error.code(), ErrorCode::ScratchClaimed, "{error}");
        } else {
            // The VMM runs, as the launch's journal records it, which no runtime record has
            // replaced yet.
            let journal = store
                .journal()
                .unwrap()
                .expect("the launch has its journal");
            assert_eq!(journal.state(), LaunchState::Spawned, "at {step}");
            let vmm = Process::open(journal.process().unwrap())
                .unwrap()
                .expect("the VMM runs");
            assert!(!vmm.has_exited().unwrap(), "at {step}");
            assert_eq!(store.runtime().unwrap(), None, "at {step}");
        }
        assert_eq!(
            fs::read_to_string(&pause).unwrap(),
            step.as_str(),
            "the pause file names its step"
        );

        // Once the pause file is gone, the launch goes on.
        fs::remove_file(&pause).unwrap();
        let launched = sandbox.finish_launch(launch, 0);
        assert_eq!(launched["root"], "ready", "after a pause at {step}");
        stop(&mut sandbox, &launched);
    }
}
