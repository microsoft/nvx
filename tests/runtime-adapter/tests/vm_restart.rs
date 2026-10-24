//! The host program stages an image and launches a VM of the guest agent with a layered root
//! through NVX's supervisor, and later processes of it reattach to the VM and stop it, as an
//! orchestrator does after it restarts, on a real hypervisor, through public APIs only. A launch
//! that finds the guest agent of another build than the one that the host program ships tears the
//! VM down.
//!
//! The tests boot VMs, so they are ignored. They read what the `common` module lists, and boot its
//! fixtures: NVX's Ubuntu EROFS image over a copy of the scratch template.
//!
//! ```text
//! cargo test --locked --features host -- --ignored
//! ```
#![cfg(feature = "host")]

mod common;

use std::ffi::{OsStr, OsString};
use std::fs;

use nvx::host::process::Process;
use runtime_adapter::GUEST_BUILD;

use common::{Fixtures, Printed, Sandbox, epoch};

/// Stages the fixtures' image over a copy of the scratch template in `sandbox`, and returns the
/// options that launch a VM of them.
fn stage(sandbox: &mut Sandbox) -> Vec<OsString> {
    let fixtures = Fixtures::find();
    let scratch = sandbox.root.join("scratch.ext4");
    fs::copy(&fixtures.scratch, &scratch).unwrap();
    let mut distro = OsString::from("erofs:");
    distro.push(&fixtures.distro);
    let mut upper = OsString::from("scratch:");
    upper.push(&scratch);
    let images = vec![
        OsString::from("--distro"),
        distro,
        OsString::from("--distro-identity"),
        OsString::from(&fixtures.identity),
        OsString::from("--upper"),
        upper,
    ];
    sandbox.run("stage", &options(&images), 0);
    images
}

/// Returns `images` as the options of a command.
fn options(images: &[OsString]) -> Vec<&OsStr> {
    images.iter().map(OsString::as_os_str).collect()
}

/// Stages the fixtures' image over a copy of the scratch template in `sandbox`, launches a VM of
/// them, whose guest agent assembles the root, and starts the sandbox, and returns what the launch
/// printed.
fn launch(sandbox: &mut Sandbox) -> Printed {
    let images = stage(sandbox);
    let launched = sandbox.launch(&options(&images), 0);
    assert_started(&launched);
    launched
}

/// Checks that a launch printed that the guest agent of the build that the host program ships
/// assembled the root, and that the guest started the sandbox, without mappings.
fn assert_started(launched: &Printed) {
    assert_eq!(launched["root"], "ready");
    assert_eq!(launched["build"], GUEST_BUILD);
    assert_eq!(launched["table"], "applied");
    assert_eq!(launched["mapped"], "0");
    assert_eq!(launched["sandbox"], "started");
}

/// Checks that a command stopped the VM of `vmm` that `launched` printed, with the guest agent's
/// confirmation, after it attached a session in an epoch after `earlier`'s: the VM exited with
/// a successful outcome, and no runtime record names it.
fn assert_stopped(
    sandbox: &Sandbox,
    stopped: &Printed,
    launched: &Printed,
    earlier: &Printed,
    vmm: &Process,
) {
    assert_eq!(stopped["reattached"], "live");
    assert_eq!(stopped["pid"], launched["pid"]);
    assert_eq!(stopped["instance"], launched["instance"]);
    assert!(epoch(stopped) > epoch(earlier), "{stopped:?} {earlier:?}");
    assert_eq!(stopped["stopped"], "graceful");
    assert_eq!(stopped["outcome"], "success 0");
    assert_eq!(stopped["teardown"], "complete");
    assert!(vmm.has_exited().unwrap());
    assert_eq!(
        sandbox.record(),
        None,
        "the stop cleared the runtime record"
    );
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn a_restarted_host_program_reattaches_to_the_vm_and_stops_it() {
    let mut sandbox = Sandbox::new("restart");
    // The launching process has exited, and the VM runs on.
    let launched = launch(&mut sandbox);
    let vmm = sandbox
        .vmm()
        .expect("the VM runs after its launcher exited");

    // Another process holds the VM, which is not its child, and attaches a new session, in the
    // session's next epoch.
    let reattached = sandbox.run("reattach", &[], 0);
    assert_eq!(reattached["reattached"], "live");
    assert_eq!(reattached["pid"], launched["pid"]);
    assert_eq!(reattached["instance"], launched["instance"]);
    assert!(epoch(&reattached) > epoch(&launched), "{reattached:?}");
    assert!(!vmm.has_exited().unwrap());

    // A third one stops it.
    let stopped = sandbox.run("stop", &[], 0);
    assert_stopped(&sandbox, &stopped, &launched, &reattached, &vmm);
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn a_host_program_of_another_runtime_finds_the_vm_mismatched_and_signals_nothing() {
    let mut sandbox = Sandbox::new("identity");
    let launched = launch(&mut sandbox);
    let record = sandbox.record();
    let vmm = sandbox
        .vmm()
        .expect("the VM runs after its launcher exited");

    // A host program that ships another build of the guest agent runs another runtime.
    let options = [OsStr::new("--guest-build"), OsStr::new("another-build")];
    let mismatched = sandbox.run("reattach", &options, 3);
    assert_eq!(mismatched["reattached"], "mismatched");
    assert_eq!(mismatched["differences"], "initramfs");
    assert_eq!(mismatched["pid"], launched["pid"]);
    assert!(!mismatched.contains_key("epoch"), "{mismatched:?}");

    // It signaled nothing: the VM runs on, under the same runtime record, and its guest agent
    // serves the host program of the right runtime, which stops it.
    assert!(!vmm.has_exited().unwrap());
    assert_eq!(sandbox.record(), record);
    let stopped = sandbox.run("stop", &[], 0);
    assert_stopped(&sandbox, &stopped, &launched, &launched, &vmm);
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn a_launch_tears_down_a_vm_whose_guest_agent_is_of_another_build() {
    let mut sandbox = Sandbox::new("build");
    let images = stage(&mut sandbox);

    // The host program ships another build of the guest agent than the one that it boots, which
    // assembles the root, and reports its own build.
    let mut another = options(&images);
    another.extend([OsStr::new("--guest-build"), OsStr::new("another-build")]);
    let launched = sandbox.launch(&another, 1);
    assert_eq!(launched["root"], "ready");
    assert_eq!(launched["build"], GUEST_BUILD);
    let stderr = sandbox.stderr();
    let problem =
        format!("the guest agent is of build {GUEST_BUILD}, not of the host's build another-build");
    assert!(stderr.contains(&problem), "{stderr}");
    // The launch sent it neither the mapping table nor `Start`, and ended the VM: no runtime
    // record names it.
    for key in ["table", "mapped", "sandbox"] {
        assert!(!launched.contains_key(key), "{launched:?}");
    }
    assert_eq!(sandbox.record(), None);

    // The VM released the scratch image as it ended, so the next launch, of the build that the
    // host program ships, claims it, and starts the sandbox.
    let launched = sandbox.launch(&options(&images), 0);
    assert_started(&launched);
    let vmm = sandbox
        .vmm()
        .expect("the VM runs after its launcher exited");
    let stopped = sandbox.run("stop", &[], 0);
    assert_stopped(&sandbox, &stopped, &launched, &launched, &vmm);
}
