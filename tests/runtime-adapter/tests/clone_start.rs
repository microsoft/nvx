//! The host program's `start` refuses options that do not make a start, and a clock fixture that is
//! not a host's time, as usage errors, and a generation that the template store does not hold, or
//! a template store that does not exist, before it needs NVX's artifacts or creates the sandbox's
//! store. A clone's record names its template store and its generation, which a stop reads back,
//! beside the plan that the other commands read, and a host's time keeps its text form, the form
//! of the clock fixture. The tests need no hypervisor.
#![cfg(feature = "host")]

use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nvx::activation::StoreGeneration;
use nvx::host::artifacts::Artifacts;
use nvx::host::template::{HostClock, HostTime, Store};
use nvx::protocol::root::ImageFormat;
use nvx::uuid::Uuid;
use runtime_adapter::clone::{
    CLONE_MODE, CloneRecord, HOST_TIME_VARIABLE, Request, host_time_text, parse_host_time,
};
use runtime_adapter::layered::{Bindings, LaunchRecord, RECORD_FORMAT, RECORD_NAME, Staged};
use serde_json::{Value, json};

/// The identity that the tests' images have.
const IDENTITY: &str = "0a490905-41be-ed5f-984e-63eb7e5e20b1";

/// Returns a directory of the test `name`, empty, below Cargo's temporary directory.
fn directory(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    directory
}

/// Returns the identity that the tests' images have.
fn identity() -> Uuid {
    Uuid::parse(IDENTITY).unwrap()
}

/// Writes a file of `length` zeros at `path`, and returns the path.
fn image(path: PathBuf, length: usize) -> PathBuf {
    fs::write(&path, vec![0; length]).unwrap();
    path
}

/// Runs the host program with `arguments`, with the clock fixture `time`, if any, and without
/// NVX's artifacts.
fn host(arguments: &[&str], time: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"));
    command
        .args(arguments)
        .env_remove(Artifacts::ENV_DIR)
        .env_remove(HOST_TIME_VARIABLE);
    if let Some(time) = time {
        command.env(HOST_TIME_VARIABLE, time);
    }
    command.output().unwrap()
}

/// Returns a host's time of the tests, by `clock`.
fn host_time(clock: HostClock) -> HostTime {
    HostTime {
        host_id: [0x0f, 0xa1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x5c, 0xff],
        boot_id: "00112233445566778899aabbccddeeff".parse().unwrap(),
        clock,
        monotonic_ns: 5_000_000_123,
        utc_ns: 1_790_841_600_000_000_000,
    }
}

#[test]
fn the_host_program_refuses_options_that_do_not_make_a_start() {
    let start = [
        "start",
        "store",
        "--template",
        "templates",
        "--generation",
        "1",
        "--distro",
        "erofs:image",
        "--distro-identity",
        IDENTITY,
    ];
    let with = |options: &[&'static str]| [&start[..], options].concat();
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["start"], "start needs a sandbox store"),
        (vec!["start", "store"], "start needs --template"),
        (start[..4].to_vec(), "start needs --generation"),
        (start[..6].to_vec(), "start needs --distro"),
        (start[..8].to_vec(), "start needs --distro-identity"),
        (start.to_vec(), "start needs --upper"),
        (
            vec!["start", "store", "--generation", "0"],
            "--generation takes a positive number, not \"0\"",
        ),
        (
            vec!["start", "store", "--generation", "+1"],
            "--generation takes a positive number, not \"+1\"",
        ),
        (
            vec!["start", "store", "--generation", "18446744073709551616"],
            "--generation takes a positive number",
        ),
        (
            with(&["--upper", "ram", "--scratch-journal", "forbidden"]),
            "--scratch-journal needs --upper scratch:PATH",
        ),
        (
            with(&["--upper", "scratch:"]),
            "--upper takes scratch:PATH or ram, not \"scratch:\"",
        ),
        (
            with(&["--upper", "ram", "--memory", "0"]),
            "--memory takes a positive number of MiB, not \"0\"",
        ),
        (
            with(&["--upper", "ram", "--workload-identity", "0:1000"]),
            "--workload-identity takes UID:GID, two numbers from 1 to 4294967294",
        ),
        (
            with(&["--upper", "ram", "--overlay-mode", "lowers"]),
            "--overlay-mode takes trusted-lowers or untrusted-lowers",
        ),
        (
            with(&["--upper", "ram", "--template", "other"]),
            "unexpected option \"--template\" of start",
        ),
        (
            with(&["--upper", "ram", "extra"]),
            "unexpected argument \"extra\" of start",
        ),
        (
            vec!["launch", "store", "--template", "templates"],
            "unexpected option \"--template\" of launch",
        ),
        (
            vec!["probe", "store", "memory", "--template", "templates"],
            "unexpected option \"--template\" of probe",
        ),
        (
            vec!["create", "store", "--generation", "1"],
            "unexpected option \"--generation\" of create",
        ),
        (
            vec!["stage", "store", "--upper", "ram"],
            "--upper of stage takes scratch:PATH, not \"ram\"",
        ),
        (
            vec!["stop", "store", "--memory", "512"],
            "unexpected option \"--memory\" of stop",
        ),
    ];
    // Each option that a start does not take.
    let others: [(&str, &str); 8] = [
        ("--guest-agent", "agent"),
        ("--hypervisor", "whp"),
        ("--runtime-dir", "/run/a"),
        ("--first-partition-bound", "4096"),
        ("--share", "a:/b"),
        ("--network", "network.json"),
        ("--cpu-profile", "auto"),
        ("--memory-capacity", "512"),
    ];
    let others = others.map(|(option, value)| {
        let arguments = with(&["--upper", "ram", option, value]);
        (
            arguments,
            format!("unexpected option \"{option}\" of start"),
        )
    });
    let cases = cases
        .into_iter()
        .map(|(arguments, problem)| (arguments, problem.to_owned()))
        .chain(others);
    for (arguments, problem) in cases {
        let output = host(&arguments, None);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&problem) && stderr.contains("usage: runtime-adapter-host"),
            "{arguments:?}: {stderr}"
        );
    }

    // A clock fixture that is not a host's time is a usage error too, of a start that the options
    // make.
    let arguments = with(&["--upper", "ram"]);
    for time in [
        "now",
        "host=0,boot=0,clock=linux-boottime,monotonic=0,utc=0",
    ] {
        let output = host(&arguments, Some(time));
        assert_eq!(output.status.code(), Some(2), "{time:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("{HOST_TIME_VARIABLE} takes host=HOST,boot=BOOT")),
            "{time:?}: {stderr}"
        );
    }
}

#[test]
fn a_start_refuses_a_generation_that_the_template_store_does_not_hold_before_anything_else() {
    // The start reads the template store's index before it needs NVX's artifacts, which the
    // commands do not name, and before it creates the sandbox's store.
    let directory = directory("start-unindexed");
    let distro = image(directory.join("distro.erofs"), 4096);
    let distro = format!("erofs:{}", distro.display());
    let templates = directory.join("templates");
    let store = directory.join("store");
    let start = |templates: &Path, generation: &str| {
        let arguments = [
            "start",
            store.to_str().unwrap(),
            "--template",
            templates.to_str().unwrap(),
            "--generation",
            generation,
            "--distro",
            &distro,
            "--distro-identity",
            IDENTITY,
            "--upper",
            "ram",
        ];
        host(&arguments, None)
    };
    let failed = |output: &Output, problem: &str| {
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(problem), "{stderr}");
        assert!(!stderr.contains("usage:"), "{stderr}");
        assert!(!store.exists(), "the start created the sandbox's store");
    };

    // No template store.
    let output = start(&templates, "1");
    failed(&output, "cannot open the template store");

    // A template store that holds no generation.
    Store::create(&templates).unwrap();
    for generation in ["1", "2"] {
        let output = start(&templates, generation);
        let problem = format!(
            "the template store {} holds no generation {generation}",
            Store::open(&templates).unwrap().dir().display()
        );
        failed(&output, &problem);
    }

    // An image that the plan refuses fails the start before the store is read.
    let missing = format!("erofs:{}", directory.join("missing.erofs").display());
    let none = directory.join("none");
    let arguments = [
        "start",
        store.to_str().unwrap(),
        "--template",
        none.to_str().unwrap(),
        "--generation",
        "1",
        "--distro",
        &missing,
        "--distro-identity",
        IDENTITY,
        "--upper",
        "ram",
    ];
    let output = host(&arguments, None);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("template store"), "{stderr}");
    assert!(!store.exists());
}

#[test]
fn a_host_s_time_keeps_its_text_form() {
    let linux = host_time(HostClock::LinuxBoottime);
    let text = host_time_text(&linux);
    assert_eq!(
        text,
        "host=0fa10000000000000000000000005cff,boot=00112233445566778899aabbccddeeff,\
         clock=linux-boottime,monotonic=5000000123,utc=1790841600000000000"
    );
    assert_eq!(parse_host_time(&text), Ok(linux));
    let windows = host_time(HostClock::WindowsInterruptTime);
    let text = host_time_text(&windows);
    assert!(text.contains(",clock=windows-interrupt-time,"), "{text}");
    assert_eq!(parse_host_time(&text), Ok(windows));
    let extremes = HostTime {
        monotonic_ns: 0,
        utc_ns: u64::MAX,
        ..linux
    };
    assert_eq!(parse_host_time(&host_time_text(&extremes)), Ok(extremes));

    // Anything else is no host's time: each field in its place, its identities in lowercase
    // hexadecimal, a clock that a manifest names, and readings of digits alone, in 64 bits.
    let text = host_time_text(&linux);
    let refused = [
        String::new(),
        text.replace("host=0fa1", "host=0FA1"),
        text.replace("host=0fa1", "host=0fa"),
        text.replace("boot=0011", "boot=001g"),
        text.replace("clock=linux-boottime", "clock=boottime"),
        text.replace("monotonic=5000000123", "monotonic=+5000000123"),
        text.replace("monotonic=5000000123", "monotonic="),
        text.replace("utc=1790841600000000000", "utc=18446744073709551616"),
        text.replace("utc=", "UTC="),
        text.replace(",clock=linux-boottime", ""),
        format!("{text},extra=1"),
        format!("{text},"),
        format!(" {text}"),
        {
            let (host, rest) = text.split_once(',').unwrap();
            format!("{rest},{host}")
        },
    ];
    for text in refused {
        let problem = parse_host_time(&text).unwrap_err();
        assert!(
            problem.starts_with(&format!("{HOST_TIME_VARIABLE} takes host=HOST")),
            "{text:?}: {problem}"
        );
    }
}

#[test]
fn a_clone_s_record_names_its_template_store_and_generation_beside_its_plan() {
    let directory = directory("clone-record");
    let distro = image(directory.join("distro.erofs"), 4096);
    let scratch = image(directory.join("scratch.ext4"), 8192);
    let request = Request {
        distro: distro.clone(),
        format: ImageFormat::Erofs,
        identity: identity(),
        scratch: Some(scratch.clone()),
        memory: None,
        workload_identity: Some((1000, 1000)),
        overlay_mode: None,
        scratch_journal: None,
    };
    let plan = request.plan().unwrap();
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), Some(&scratch)).unwrap();
    let bindings = staged.bindings(&plan);
    let templates = directory.join("templates");
    let generation = StoreGeneration::new(NonZeroU64::new(7).unwrap());
    let record = CloneRecord {
        template: &templates,
        generation,
        kernel_command_line: "nvx_sandbox=1 nvx_snapshot_tier=platform",
        plan: &plan,
        bindings: &bindings,
    };
    let sandbox = directory.join("store");
    fs::create_dir(&sandbox).unwrap();
    assert_eq!(CloneRecord::read_template(&sandbox).unwrap(), None);
    record.write(&sandbox).unwrap();

    let written: Value =
        serde_json::from_str(&fs::read_to_string(sandbox.join(RECORD_NAME)).unwrap()).unwrap();
    assert_eq!(written["format"], json!(RECORD_FORMAT));
    assert_eq!(written["mode"], json!(CLONE_MODE));
    assert_eq!(
        written["kernelCommandLine"],
        json!("nvx_sandbox=1 nvx_snapshot_tier=platform")
    );
    assert_eq!(written["plan"], serde_json::to_value(&plan).unwrap());
    assert_eq!(written["bindings"], bindings.to_json().unwrap());
    assert_eq!(written["rootConfiguration"], Value::Null);
    assert_eq!(
        written["template"],
        json!({"store": templates.to_str().unwrap(), "generation": 7})
    );
    assert_eq!(
        CloneRecord::read_template(&sandbox).unwrap(),
        Some((templates.clone(), generation))
    );
    assert_eq!(
        LaunchRecord::read_plan(&sandbox).unwrap(),
        Some(plan.clone())
    );

    // A record of a launch that is no clone names no template store; one of a clone must name
    // both, by a path and a positive number.
    let launched = LaunchRecord {
        kernel_command_line: "nvx_sandbox=1",
        layered: None,
    };
    launched.write(&sandbox).unwrap();
    assert_eq!(CloneRecord::read_template(&sandbox).unwrap(), None);
    for template in [
        json!(null),
        json!({"store": "/templates"}),
        json!({"store": 7, "generation": 1}),
        json!({"store": "/templates", "generation": 0}),
        json!({"store": "/templates", "generation": "1"}),
    ] {
        let mut broken = written.clone();
        broken["template"] = template.clone();
        fs::write(sandbox.join(RECORD_NAME), broken.to_string()).unwrap();
        let error = CloneRecord::read_template(&sandbox).unwrap_err();
        assert_eq!(error.kind(), nvx::ErrorKind::Unavailable, "{template}");
    }
    let mut other = written.clone();
    other["format"] = json!(RECORD_FORMAT + 1);
    fs::write(sandbox.join(RECORD_NAME), other.to_string()).unwrap();
    let error = CloneRecord::read_template(&sandbox).unwrap_err();
    assert_eq!(error.kind(), nvx::ErrorKind::Unavailable);
    fs::write(sandbox.join(RECORD_NAME), "{").unwrap();
    let error = CloneRecord::read_template(&sandbox).unwrap_err();
    assert_eq!(error.kind(), nvx::ErrorKind::Unavailable);
}

#[test]
fn a_clone_s_request_takes_the_staged_bindings_and_the_guest_s_defaults() {
    let directory = directory("clone-request");
    let distro = image(directory.join("distro.erofs"), 4096);
    let scratch = image(directory.join("scratch.ext4"), 8192);
    let request = Request {
        distro: distro.clone(),
        format: ImageFormat::Erofs,
        identity: identity(),
        scratch: Some(scratch.clone()),
        memory: None,
        workload_identity: None,
        overlay_mode: None,
        scratch_journal: None,
    };
    let plan = request.plan().unwrap();
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), Some(&scratch)).unwrap();
    let bindings = staged.bindings(&plan);
    let memory = 512 << 20;
    let clone = request
        .clone_request(plan.clone(), &bindings, memory)
        .unwrap();
    assert_eq!(clone.adapter, plan);
    assert_eq!(clone.binding, staged.distro.binding);
    assert_eq!(clone.scratch, Some(staged.scratch.unwrap().binding));
    assert_eq!(clone.memory, memory);
    assert_eq!(
        clone.policy_revision,
        nvx::host::sandbox::distro::DISTRO_POLICY_REVISION
    );
    assert_eq!(clone.workload.hostname, b"runtime-adapter");
    assert_eq!(
        clone.workload.machine_id,
        nvx::activation::MachineIdMode::Image
    );
    assert_eq!(clone.workload.identity, None);
    assert_eq!(clone.overlay, nvx::activation::OverlayMode::TrustedLowers);
    assert_eq!(clone.journal, nvx::activation::Journal::Allowed);

    // A request that names its settings.
    let named = Request {
        workload_identity: Some((65534, 65533)),
        overlay_mode: Some(runtime_adapter::root::OverlayMode::UntrustedLowers),
        scratch_journal: Some(runtime_adapter::root::JournalPolicy::Forbidden),
        ..request.clone()
    };
    let clone = named
        .clone_request(plan.clone(), &bindings, memory)
        .unwrap();
    assert_eq!(
        clone.workload.identity,
        Some(nvx::activation::WorkloadIdentity {
            uid: 65534,
            gid: 65533
        })
    );
    assert_eq!(clone.overlay, nvx::activation::OverlayMode::UntrustedLowers);
    assert_eq!(clone.journal, nvx::activation::Journal::Forbidden);

    // An image, or a scratch image, that the store does not stage.
    for (bindings, what) in [
        (Bindings::default(), "image"),
        (
            Bindings {
                scratch: None,
                ..bindings.clone()
            },
            "scratch image",
        ),
    ] {
        let error = request
            .clone_request(plan.clone(), &bindings, memory)
            .unwrap_err();
        assert_eq!(error.kind(), nvx::ErrorKind::Unavailable, "{what}");
        assert!(
            error
                .to_string()
                .starts_with(&format!("the sandbox's store stages no {what} at ")),
            "{error}"
        );
    }

    // A clone in RAM takes no scratch's binding, but the image's still.
    let in_ram = Request {
        scratch: None,
        ..request
    };
    let plan = in_ram.plan().unwrap();
    let error = in_ram
        .clone_request(plan.clone(), &Bindings::default(), memory)
        .unwrap_err();
    assert_eq!(error.kind(), nvx::ErrorKind::Unavailable);
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), None).unwrap();
    let clone = in_ram
        .clone_request(plan.clone(), &staged.bindings(&plan), memory)
        .unwrap();
    assert_eq!(clone.scratch, None);
    assert_eq!(clone.adapter, plan);
}
