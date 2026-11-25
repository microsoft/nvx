//! Tests of the configuration that a run launches, and of the removal of its VM directory.

use std::fs;
use std::path::PathBuf;

use super::*;
use crate::host::filesystem::PlannedExport;
use crate::host::paths::resolve;
use crate::host::sandbox::distro::DistroBinding;
use crate::host::sandbox::layer::{LayerRole, SandboxLayer};
use crate::host::sandbox::mount::{plan_mounts, validate_mounts};
use crate::host::sandbox::options::sandbox_mounts;
use crate::host::sandbox::{DEFAULT_HOSTNAME, DEFAULT_WORKLOAD_IDENTITY};

/// The starts of restored sandboxes, with a fake OpenVMM and guest in place of OpenVMM's restore,
/// which the tests fake on Linux and Windows.
#[cfg(any(target_os = "linux", windows))]
mod restore;

#[test]
fn run_checks_again_that_its_shares_do_not_overlap() {
    let directory = tempfile::tempdir().unwrap();
    let root = resolve(directory.path()).unwrap();
    let path = |name: &str| root.join(name).to_str().unwrap().to_owned();
    for (name, content) in [
        ("distro.erofs", "distro"),
        ("scratch.ext4", "scratch"),
        ("settings.json", "{}"),
        ("output.log", ""),
    ] {
        fs::write(path(name), content).unwrap();
    }
    let mounts = sandbox_mounts(
        &[
            format!("/etc/settings.json,{}", path("settings.json")),
            format!("/var/log/output.log,{},rw", path("output.log")),
        ],
        &[],
        None,
    )
    .unwrap();
    validate_mounts(&mounts).unwrap();
    let layer = SandboxLayer::new(
        LayerRole::Distro,
        &path("distro.erofs"),
        "11111111-1111-1111-1111-111111111111",
    );
    let scratch = PathBuf::from(path("scratch.ext4"));
    let export = plan_mounts(&mounts).unwrap();
    let binding = DistroBinding::verify(&layer).unwrap();
    let distro = RecordedDistro::new(&layer, binding).unwrap();
    let config = ProvisionConfig {
        hypervisor: "kvm",
        cpu_profile: None,
        memory_mib: 256,
        cmdline: "",
        ram_upper_percent: None,
        hostname: DEFAULT_HOSTNAME,
        workload_identity: DEFAULT_WORKLOAD_IDENTITY,
        export: export.as_ref(),
        network: None,
    };
    record(&distro, Some(&scratch), &config).unwrap();

    // A hard link that replaced the read-only file with the writable one while the run verified
    // its distro layer would export one file under both access modes; the export plan's own
    // checks pass, since a file share's root is not pinned.
    fs::remove_file(path("settings.json")).unwrap();
    fs::hard_link(path("output.log"), path("settings.json")).unwrap();
    export.as_ref().unwrap().verify().unwrap();
    let Err(error) = record(&distro, Some(&scratch), &config) else {
        panic!("the run launched shares that came to overlap");
    };
    assert_eq!(
        error.code(),
        Some(crate::ErrorCode::Unsupported),
        "{error:?}"
    );
    let message = error.message().unwrap_or_default();
    assert!(
        message.contains("name the same directory or file"),
        "{message}"
    );
}

/// A start and a run check the kernel command line that they launch: the user's part, after the
/// boot tokens of the root and of the workload, and the bootstrap tokens that OpenVMM appends for
/// the shares fill the 1024-byte budget to the byte, whatever the upper and the shares, and the
/// guest must read the root's boot tokens from it.
#[test]
fn starts_check_the_kernel_command_line_that_they_launch() {
    let directory = tempfile::tempdir().unwrap();
    let root = resolve(directory.path()).unwrap();
    let path = |name: &str| root.join(name).to_str().unwrap().to_owned();
    for (name, content) in [
        ("distro.erofs", "distro"),
        ("scratch.ext4", "scratch"),
        ("settings.json", "{}"),
        ("output.log", ""),
    ] {
        fs::write(path(name), content).unwrap();
    }
    let layer = SandboxLayer::new(
        LayerRole::Distro,
        &path("distro.erofs"),
        "11111111-1111-1111-1111-111111111111",
    );
    let distro = RecordedDistro::new(&layer, DistroBinding::verify(&layer).unwrap()).unwrap();
    let scratch = PathBuf::from(path("scratch.ext4"));
    let settings = format!("/etc/settings.json,{}", path("settings.json"));
    let output = format!("/var/log/output.log,{},rw", path("output.log"));
    let export = |shares: &[String]| plan_mounts(&sandbox_mounts(shares, &[], None).unwrap());
    let read_only = export(std::slice::from_ref(&settings)).unwrap();
    let writable = export(&[settings, output]).unwrap();
    let tokens = |export: &PlannedExport| export_tokens(&export.description);
    assert_eq!(
        tokens(read_only.as_ref().unwrap()),
        " virtfs_dir=/run/nvx/hostfs/root virtfs_tag=microvm virtfs_mode=ro virtfs_aggregate=1"
    );
    let configure = |cmdline: &str, scratch: Option<&Path>, export: Option<&PlannedExport>| {
        let config = ProvisionConfig {
            hypervisor: "kvm",
            cpu_profile: None,
            memory_mib: 256,
            cmdline,
            ram_upper_percent: None,
            hostname: DEFAULT_HOSTNAME,
            workload_identity: DEFAULT_WORKLOAD_IDENTITY,
            export,
            network: None,
        };
        let config = record(&distro, scratch, &config).unwrap();
        let vm = config.vm_config().unwrap();
        (config, vm)
    };
    let refusal = |cmdline: &str, scratch: Option<&Path>, export: Option<&PlannedExport>| {
        let (config, vm) = configure(cmdline, scratch, export);
        let error = check_command_line(&config, &vm).unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::Unsupported), "{error:?}");
        error.message().unwrap_or_default().to_owned()
    };
    for upper in [None, Some(scratch.as_path())] {
        for export in [None, read_only.as_ref(), writable.as_ref()] {
            let (_, vm) = configure("", upper, export);
            let fragment = export.map(tokens).unwrap_or_default();
            let room =
                COMMAND_LINE_BUDGET - vm.kernel_command_line().unwrap().len() - fragment.len() - 2;
            let (config, vm) = configure(&"x".repeat(room), upper, export);
            check_command_line(&config, &vm).unwrap();
            assert_eq!(
                vm.kernel_command_line().unwrap().len() + fragment.len() + 1,
                COMMAND_LINE_BUDGET
            );
            assert_eq!(
                refusal(&"x".repeat(room + 1), upper, export),
                "sandbox kernel command line exceeds its 1024-byte x86 budget; use shorter \
                 arguments and kernel parameters"
            );
        }
    }
    assert_eq!(
        refusal("quiet\0", None, None),
        "kernel command line contains an embedded NUL"
    );
    // A `--` as Linux reads it, whose whitespace includes the 0xa0 that ends `à` and a vertical
    // tab, and an unclosed quote hide the tokens that OpenVMM appends after the user's part, also
    // after a parameter of any name.
    for user in [
        "quiet -- debug",
        "quiet \u{e0}--",
        "quiet\u{b}--",
        "x --",
        "x\u{b}--",
        "a=\"b c",
    ] {
        assert_eq!(
            refusal(user, None, None),
            "sandbox kernel command line hides the tokens that OpenVMM appends to it",
            "{user:?}"
        );
    }
    // The user's part may add a virtio-mmio device, which the guest reads as the kernel does: one
    // at the scratch disk's base conflicts with a RAM upper.
    assert_eq!(
        refusal("virtio_mmio.device=0x1000@0xd0006000:22", None, None),
        "sandbox kernel command line conflicts with the boot tokens of its root: a RAM upper \
         takes no scratch block device, but the command line adds a virtio-mmio device at \
         0xd0006000"
    );
}

/// A run that Ctrl-C interrupts while it forces its VM down still waits for the VM's exit, for
/// [`KILL_TIMEOUT`], without heeding Ctrl-C, as after Ctrl-C during the workload, and says that a
/// VM that has not exited by then is stuck exiting. Ctrl-C reaches every process of a console, so
/// a helper of a console of its own raises it.
#[cfg(windows)]
#[test]
fn a_run_that_ctrl_c_interrupts_while_it_forces_its_vm_down_waits_for_the_vm_all_the_same() {
    let output = platform::tests::in_private_console(
        "host::cli::sandbox::lifecycle::start::tests::interrupted_force_down",
    );
    assert!(output.status.success(), "{output:?}");
}

/// The helper of the test above, which runs in a console of its own.
#[cfg(windows)]
#[test]
#[ignore = "run in a console of its own by the test that raises Ctrl-C there"]
fn interrupted_force_down() {
    use std::time::Duration;

    use crate::host::process::{BootId, Opened, ProcessRef};

    platform::tests::watch_private_interrupts();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("vm");
    vm::create_private_dir(&path).unwrap();
    let outputs = VmDir::open(&path).unwrap().outputs(false).unwrap();
    let open = |pid| {
        let reference = ProcessRef {
            pid,
            start_time: platform::process_start_time(pid).unwrap().unwrap(),
            boot: BootId::current().unwrap(),
        };
        let Opened::Running(process) = Process::open(&reference).unwrap() else {
            panic!("the debugged process runs");
        };
        process
    };
    // Ctrl-C comes while the run waits for the exit of the VM that it terminated, which the
    // debugger holds until shortly after.
    let mut held = platform::tests::HeldExit::start();
    let process = open(held.pid());
    let stopped = thread::scope(|scope| {
        let held = &mut held;
        scope.spawn(move || {
            let until = Instant::now() + Duration::from_secs(30);
            while !held.exiting() {
                assert!(Instant::now() < until, "the run did not terminate the VM");
                thread::sleep(Duration::from_millis(5));
            }
            platform::tests::interrupt_private_console();
            thread::sleep(Duration::from_millis(500));
            held.release();
        });
        force_down(&process, &outputs, Deadline::after(60.0))
    });
    let exited = process.wait(Instant::now()).unwrap();
    let (self_exited, _) = stopped.unwrap();
    assert!(!self_exited);
    assert!(exited.is_some(), "the run returned before the VM exited");
    // Ctrl-C has come before the run forces another VM down, whose exit the debugger holds past
    // the wait.
    let mut held = platform::tests::HeldExit::start();
    let pid = held.pid();
    let process = open(pid);
    let began = Instant::now();
    let stopped = force_down(&process, &outputs, Deadline::after(60.0));
    let waited = began.elapsed();
    let unconfirmed = process.wait(Instant::now()).unwrap();
    held.release();
    let error = stopped.unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::StillRunning), "{error:?}");
    assert_eq!(
        error.message(),
        Some(
            format!(
                "cannot confirm exit of VM process {pid}: it is stuck exiting after its forced \
                 termination"
            )
            .as_str()
        )
    );
    assert!(waited >= KILL_TIMEOUT, "{waited:?}");
    assert!(unconfirmed.is_none(), "the held exit showed");
}

/// A run removes its VM directory with the staging file of an outcome report that OpenVMM did
/// not finish writing, as one that the run forced down can leave, and keeps a directory that
/// holds a file of another name.
#[test]
fn a_run_removes_its_vm_directory_with_a_report_s_staging_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("vm");
    for (staging, removed) in [(".tmpA1b2C3", true), ("other", false)] {
        vm::create_private_dir(&path).unwrap();
        for name in [StateFile::Log.name(), StateFile::Outcome.name(), staging] {
            fs::write(path.join(name), "").unwrap();
        }
        let result = remove_vm_directory(&VmDir::open(&path).unwrap());
        assert_eq!(result.is_ok(), removed, "{staging}: {result:?}");
        assert_eq!(path.exists(), !removed, "{staging}");
        if !removed {
            assert!(path.join(staging).exists(), "{staging}");
            fs::remove_dir_all(&path).unwrap();
        }
    }
}

/// A run removes the VM directories of runs that ended without removing them, whose logs no
/// process holds any longer, and leaves every other directory as it is: one whose log a process
/// holds, as a run that still runs or its OpenVMM does, one without a log, one that holds a file
/// of another name, one that is not private, and one of another name.
#[cfg(windows)]
#[test]
fn a_run_removes_only_the_vm_directories_that_runs_abandoned() {
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path();
    let digits = |digit: char| digit.to_string().repeat(32);
    let private = |name: &str, files: &[&str]| {
        let path = parent.join(name);
        vm::create_private_dir(&path).unwrap();
        for file in files {
            fs::write(path.join(file), "").unwrap();
        }
        path
    };
    let log = StateFile::Log.name();
    let outcome = StateFile::Outcome.name();
    let abandoned = private(
        &format!("{RUN_DIRECTORY}{}", digits('a')),
        &[log, outcome, ".tmpA1b2C3"],
    );
    let running = private(&format!("{RUN_DIRECTORY}{}", digits('b')), &[log]);
    let held = fs::File::open(running.join(log)).unwrap();
    let starting = private(&format!("{RUN_DIRECTORY}{}", digits('c')), &[]);
    let foreign = private(&format!("{RUN_DIRECTORY}{}", digits('d')), &[log, "other"]);
    let shared = parent.join(format!("{RUN_DIRECTORY}{}", digits('e')));
    fs::create_dir(&shared).unwrap();
    fs::write(shared.join(log), "").unwrap();
    let uppercase = private(&format!("{RUN_DIRECTORY}{}", digits('F')), &[log]);
    let short = private(&format!("{RUN_DIRECTORY}{}", "0".repeat(31)), &[log]);
    let other = private(&format!("nvx-run-{}", digits('0')), &[log]);
    remove_abandoned_vm_directories(parent, RUN_DIRECTORY);
    drop(held);
    assert!(!abandoned.exists(), "an abandoned directory was kept");
    for kept in [
        &running, &starting, &foreign, &shared, &uppercase, &short, &other,
    ] {
        assert!(kept.exists(), "{}", kept.display());
    }
    for kept in [&running, &foreign, &shared, &uppercase, &short, &other] {
        assert!(kept.join(log).exists(), "{}", kept.display());
    }
    assert!(foreign.join("other").exists());
}
