//! The host program stages the images of a layered root, plans a VM with a layered root with NVX's
//! typed plan for an adapter's own guest, composes OpenVMM's arguments without NVX's managed
//! lifecycle, records what it launches, with the bindings that the store stages, and refuses
//! options that do not make a command. The tests need no hypervisor.
#![cfg(feature = "host")]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use nvx::host::sandbox::SandboxPlan;
use nvx::host::sandbox::distro::DistroFormat;
use nvx::protocol::root::{ImageFormat, RootImage, RootLayout, RootUpper};
use nvx::uuid::Uuid;
use nvx::{ErrorKind, Hypervisor};
use runtime_adapter::layered::{
    Bindings, LaunchRecord, LayeredLaunch, RECORD_NAME, SANDBOX_TOKEN, STAGED_NAME, Staged,
};
use runtime_adapter::root::RootConfig;
use serde_json::{Value, json};

/// The SHA-256 digest of 1 KiB of zeros, the content of the tests' images with a manifest.
const ZERO_KIB_SHA256: &str = "5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef";

/// Returns a directory of the test `name`, empty, below Cargo's temporary directory.
fn directory(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    directory
}

/// Writes a file of `length` bytes at `path`, and returns the path.
fn image(path: PathBuf, length: usize) -> PathBuf {
    fs::write(&path, vec![0; length]).unwrap();
    path
}

/// Writes the manifest of the image at `path` beside it, with the format 1, the artifact's name
/// `artifact`, its digest `digest`, and a package.
fn manifest(path: &Path, artifact: &str, digest: &str) {
    let mut name = path.file_name().unwrap().to_owned();
    name.push(".manifest.json");
    let manifest = json!({
        "format": 1,
        "artifact": artifact,
        "artifact_sha256": digest,
        "packages": [{"name": "busybox"}],
    });
    fs::write(path.with_file_name(name), manifest.to_string()).unwrap();
}

/// Returns the identity that the tests expect of their images.
fn identity() -> Uuid {
    Uuid::parse("0a490905-41be-ed5f-984e-63eb7e5e20b1").unwrap()
}

/// Returns the absolute path of `path`.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap()
}

#[test]
fn a_layered_launch_plans_its_blocks_and_boot_tokens_with_the_adapters_plan() {
    let directory = directory("layered-plan");
    let distro = image(directory.join("distro.erofs"), 3 * 512);
    let scratch = image(directory.join("scratch.ext4"), 8 * 512);
    let launch = LayeredLaunch::new(
        &distro,
        ImageFormat::Erofs,
        identity(),
        Some(&scratch),
        None,
    )
    .unwrap();
    let config = RootConfig {
        distro_sectors: Some(3),
        scratch_sectors: Some(8),
        ..RootConfig::default()
    };
    assert_eq!(launch.config, config);
    assert_eq!(launch.bindings, Bindings::default());
    let layout = RootLayout {
        image: RootImage::erofs(Some(identity())),
        upper: RootUpper::Scratch,
    };
    assert_eq!(launch.plan.layout(), layout);
    // The adapter's plan holds none of the settings of NVX's sandbox profile.
    assert_eq!(launch.plan.settings(), None);

    // The kernel command line hands the VM to the init agent, which the guest agent replaces, and
    // carries the boot tokens of the root, which the guest reads back, and nothing of the
    // profile's.
    let command_line = launch.kernel_command_line().unwrap();
    assert_eq!(
        command_line,
        format!("{SANDBOX_TOKEN} {}", layout.tokens().join(" "))
    );
    assert_eq!(
        RootLayout::parse(command_line.as_bytes()).unwrap(),
        Some(layout)
    );

    // OpenVMM's arguments: the plan's, which attach the image read-only and the scratch, with no
    // workload identity, then the launch's, with no managed lifecycle.
    let (kernel, initramfs) = (directory.join("vmlinux"), directory.join("initramfs"));
    let report = directory.join("outcome.json");
    let arguments = launch
        .openvmm_arguments(
            Hypervisor::Kvm,
            &kernel,
            &initramfs,
            "/run/control.sock",
            &report,
        )
        .unwrap();
    let plan = launch.plan.openvmm_arguments().unwrap();
    let blocks = [
        "--machine",
        "microvm",
        "--microvm-sandbox-block",
        &format!("distro:file:{},ro", distro.display()),
        "--microvm-sandbox-block",
        &format!("scratch:file:{}", scratch.display()),
    ];
    assert_eq!(plan, blocks);
    let (planned, rest) = arguments.split_at(plan.len());
    assert_eq!(planned, plan.iter().map(OsString::from).collect::<Vec<_>>());
    let expected: [OsString; 18] = [
        "--single-process".into(),
        "--hypervisor".into(),
        "kvm".into(),
        "--memory".into(),
        "256M".into(),
        "--kernel".into(),
        kernel.into(),
        "--initrd".into(),
        initramfs.into(),
        "--cmdline".into(),
        command_line.into(),
        "--virtio-console".into(),
        "none".into(),
        "--microvm-control-console".into(),
        "listen=/run/control.sock".into(),
        "--microvm-control-auth-stdin".into(),
        "--microvm-report".into(),
        report.into(),
    ];
    assert_eq!(rest, expected);
}

#[test]
fn a_layered_launch_passes_what_the_guest_checks_as_it_is() {
    let directory = directory("layered-options");
    // An image that is not a whole number of sectors has no size, an upper in RAM no scratch, and
    // a runtime directory replaces the default as it is, so that the guest checks it.
    let distro = image(directory.join("distro.img"), 1000);
    let launch = LayeredLaunch::new(
        &distro,
        ImageFormat::Gpt,
        identity(),
        None,
        Some("run/relative"),
    )
    .unwrap();
    let config = RootConfig {
        runtime: "run/relative".to_owned(),
        ..RootConfig::default()
    };
    assert_eq!(launch.config, config);
    assert_eq!(launch.plan.layout().image, RootImage::gpt(Some(identity())));
    assert_eq!(launch.plan.layout().upper, RootUpper::Ram);
    let arguments = launch.plan.openvmm_arguments().unwrap();
    assert!(
        !arguments
            .iter()
            .any(|argument| argument.starts_with("scratch:")),
        "{arguments:?}"
    );

    // A relative image is planned by its absolute path, since OpenVMM runs in the sandbox's store.
    let relative = distro.strip_prefix(std::env::current_dir().unwrap());
    if let Ok(relative) = relative {
        let launch =
            LayeredLaunch::new(relative, ImageFormat::Gpt, identity(), None, None).unwrap();
        assert_eq!(launch.plan.distro().path(), distro);
    }

    // An image that does not exist is refused.
    let missing = directory.join("missing.erofs");
    let error =
        LayeredLaunch::new(&missing, ImageFormat::Erofs, identity(), None, None).unwrap_err();
    assert!(
        error.to_string().contains("distro EROFS layer not found"),
        "{error}"
    );
}

#[test]
fn staging_verifies_the_image_and_binds_the_scratch() {
    let directory = directory("layered-stage");
    let distro = image(directory.join("distro.erofs"), 1024);
    let scratch = image(directory.join("scratch.ext4"), 4096);

    // An image without a manifest passes without being read, and the scratch image is bound
    // without being read either. Each is staged at its absolute path, as a plan names it.
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), Some(&scratch)).unwrap();
    assert_eq!(staged.distro.path, absolute(&distro));
    let binding = &staged.distro.binding;
    assert_eq!(binding.format(), DistroFormat::Erofs);
    assert_eq!(binding.identity(), identity().to_string());
    assert_eq!(binding.manifest(), None);
    let staged_scratch = staged.scratch.as_ref().unwrap();
    assert_eq!(staged_scratch.path, absolute(&scratch));
    assert_eq!(staged_scratch.binding.length(), 4096);

    // A manifest beside the image must name it and its digest, which verification compares with a
    // hash of the image.
    manifest(&distro, "distro.erofs", ZERO_KIB_SHA256);
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), None).unwrap();
    assert!(staged.distro.binding.manifest().is_some());
    assert_eq!(staged.scratch, None);
    for (artifact, digest) in [
        ("distro.erofs", &"0".repeat(64)[..]),
        ("other.erofs", ZERO_KIB_SHA256),
    ] {
        manifest(&distro, artifact, digest);
        let error = Staged::stage(&distro, ImageFormat::Erofs, identity(), None).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert_eq!(
            error.message(),
            "sandbox distro manifest does not match its artifact"
        );
    }

    // A GPT disk over RAM.
    let disk = image(directory.join("distro.img"), 1024);
    let staged = Staged::stage(&disk, ImageFormat::Gpt, identity(), None).unwrap();
    assert_eq!(staged.distro.binding.format(), DistroFormat::Gpt);
    assert_eq!(staged.scratch, None);

    // Staging refuses what no launch could attach: an image that does not exist, and one that
    // OpenVMM would not attach as a raw image.
    let missing = directory.join("missing.erofs");
    let error = Staged::stage(&missing, ImageFormat::Erofs, identity(), None).unwrap_err();
    assert!(
        error.message().contains("distro EROFS layer not found"),
        "{error}"
    );
    let container = image(directory.join("distro.vhd"), 1024);
    let error = Staged::stage(&container, ImageFormat::Gpt, identity(), None).unwrap_err();
    assert!(
        error.message().contains("disk paths ending in .vhd"),
        "{error}"
    );
    let error = Staged::stage(&disk, ImageFormat::Gpt, identity(), Some(&missing)).unwrap_err();
    assert!(
        error.message().contains("ext4 scratch image not found"),
        "{error}"
    );
}

#[test]
fn a_launch_takes_the_bindings_that_the_store_stages_for_its_images() {
    let directory = directory("layered-bindings");
    let distro = image(directory.join("distro.erofs"), 1024);
    let scratch = image(directory.join("scratch.ext4"), 1024);
    let other = image(directory.join("other.ext4"), 1024);
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), Some(&scratch)).unwrap();
    let scratch_binding = staged.scratch.as_ref().unwrap().binding.clone();
    let bindings = |scratch: Option<&Path>| {
        let launch =
            LayeredLaunch::new(&distro, ImageFormat::Erofs, identity(), scratch, None).unwrap();
        staged.bindings(&launch.plan)
    };
    assert_eq!(
        bindings(Some(&scratch)),
        Bindings {
            distro: Some(staged.distro.binding.clone()),
            scratch: Some(scratch_binding),
        }
    );
    // An upper in RAM takes no scratch image, and another scratch image is not staged.
    for scratch in [None, Some(other.as_path())] {
        assert_eq!(
            bindings(scratch),
            Bindings {
                distro: Some(staged.distro.binding.clone()),
                scratch: None,
            }
        );
    }
    // Another image is not staged either.
    let launch = LayeredLaunch::new(&other, ImageFormat::Erofs, identity(), None, None).unwrap();
    assert_eq!(staged.bindings(&launch.plan), Bindings::default());
}

#[test]
fn the_store_records_the_staged_images() {
    let directory = directory("layered-staged");
    let distro = image(directory.join("distro.erofs"), 1024);
    let scratch = image(directory.join("scratch.ext4"), 1024);
    assert_eq!(Staged::read(&directory).unwrap(), None);
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), Some(&scratch)).unwrap();
    let json = staged.to_json().unwrap();
    let expected = json!({
        "format": 1,
        "distro": {
            "path": absolute(&distro).to_str().unwrap(),
            "binding": serde_json::to_value(&staged.distro.binding).unwrap(),
        },
        "scratch": {
            "path": absolute(&scratch).to_str().unwrap(),
            "binding": serde_json::to_value(&staged.scratch.as_ref().unwrap().binding).unwrap(),
        },
    });
    assert_eq!(json, expected);

    // The record replaces an earlier one in the store, and reads back as it was written.
    fs::write(directory.join(STAGED_NAME), "earlier").unwrap();
    staged.write(&directory).unwrap();
    let written = fs::read_to_string(directory.join(STAGED_NAME)).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&written).unwrap(), json);
    assert_eq!(Staged::read(&directory).unwrap(), Some(staged.clone()));
    let ram = Staged::stage(&distro, ImageFormat::Erofs, identity(), None).unwrap();
    let mut without_scratch = json.clone();
    without_scratch["scratch"] = Value::Null;
    assert_eq!(ram.to_json().unwrap(), without_scratch);
    assert_eq!(Staged::from_json(&without_scratch).unwrap(), ram);

    // Other forms are refused.
    let mut cases = Vec::new();
    let mut change = |problem: &'static str, edit: &dyn Fn(&mut Value)| {
        let mut record = json.clone();
        edit(&mut record);
        cases.push((record, problem));
    };
    change(
        "has the format 2, which this build does not read",
        &|record| {
            record["format"] = json!(2);
        },
    );
    change("is not an object of a format", &|record| {
        record["extra"] = json!(null);
    });
    change("is not an object of a format", &|record| {
        record.as_object_mut().unwrap().remove("scratch");
    });
    change(
        "does not name its image by a path and a binding",
        &|record| {
            record["distro"]["extra"] = json!(null);
        },
    );
    change(
        "names its scratch image by a path that is not a string",
        &|record| {
            record["scratch"]["path"] = json!(1);
        },
    );
    change(
        "holds a binding of the image that does not decode",
        &|record| {
            record["distro"]["binding"]["extra"] = json!(null);
        },
    );
    change(
        "holds a binding of the scratch image that does not decode",
        &|record| record["scratch"]["binding"]["version"] = json!("1"),
    );
    cases.push((json!([1]), "is not an object of a format"));
    for (record, problem) in cases {
        let error = Staged::from_json(&record).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
        assert!(error.message().contains(problem), "{problem}: {error}");
    }
    fs::write(directory.join(STAGED_NAME), "{").unwrap();
    let error = Staged::read(&directory).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(
        error
            .message()
            .starts_with("cannot read the record of the staged images"),
        "{error}"
    );
}

#[test]
fn a_launch_records_what_it_launched() {
    let directory = directory("layered-record");
    let distro = image(directory.join("distro.erofs"), 512);
    let scratch = image(directory.join("scratch.ext4"), 512);
    let mut launch = LayeredLaunch::new(
        &distro,
        ImageFormat::Erofs,
        identity(),
        Some(&scratch),
        None,
    )
    .unwrap();
    let record = LaunchRecord {
        kernel_command_line: "nvx_sandbox=1",
        layered: Some(&launch),
    };
    let json = record.to_json().unwrap();
    assert_eq!(json["format"], 2);
    assert_eq!(json["mode"], "layered");
    assert_eq!(json["kernelCommandLine"], "nvx_sandbox=1");
    let config = String::from_utf8(launch.config.encode()).unwrap();
    assert_eq!(json["rootConfiguration"], config.as_str());
    let plan: SandboxPlan = serde_json::from_value(json["plan"].clone()).unwrap();
    assert_eq!(plan, launch.plan);
    // Without staged images, the launch has no bindings to record.
    assert_eq!(json["bindings"], json!({"distro": null, "scratch": null}));

    // The record replaces an earlier one in the store.
    fs::write(directory.join(RECORD_NAME), "earlier").unwrap();
    record.write(&directory).unwrap();
    let written = fs::read_to_string(directory.join(RECORD_NAME)).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&written).unwrap(), json);

    // The bindings that the store stages for the launch's images, in their serde forms.
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), Some(&scratch)).unwrap();
    launch.bindings = staged.bindings(&launch.plan);
    let record = LaunchRecord {
        kernel_command_line: "nvx_sandbox=1",
        layered: Some(&launch),
    };
    let expected = json!({
        "distro": serde_json::to_value(&staged.distro.binding).unwrap(),
        "scratch": serde_json::to_value(&staged.scratch.as_ref().unwrap().binding).unwrap(),
    });
    assert_eq!(record.to_json().unwrap()["bindings"], expected);

    let blockless = LaunchRecord {
        kernel_command_line: "hostname=nvx-sandbox",
        layered: None,
    };
    let expected = json!({
        "format": 2,
        "mode": "blockless",
        "kernelCommandLine": "hostname=nvx-sandbox",
        "plan": null,
        "bindings": null,
        "rootConfiguration": null,
    });
    assert_eq!(blockless.to_json().unwrap(), expected);
}

#[test]
fn the_host_program_stages_images_in_the_store() {
    let directory = directory("layered-stage-command");
    let distro = image(directory.join("distro.erofs"), 1024);
    let scratch = image(directory.join("scratch.ext4"), 1024);
    let store = directory.join("store");
    let stage = |options: &[&Path]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"));
        let mut image = OsString::from("erofs:");
        image.push(&distro);
        command.arg("stage").arg(&store).arg("--distro").arg(image);
        command.arg("--distro-identity").arg(identity().to_string());
        for scratch in options {
            let mut upper = OsString::from("scratch:");
            upper.push(scratch);
            command.arg("--upper").arg(upper);
        }
        command.output().unwrap()
    };

    // Staging creates the store, records both bindings in it, and prints nothing.
    let output = stage(&[scratch.as_path()]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let expected = Staged::stage(&distro, ImageFormat::Erofs, identity(), Some(&scratch)).unwrap();
    assert_eq!(Staged::read(&store).unwrap(), Some(expected.clone()));

    // A manifest that does not match the image fails the staging, which leaves the earlier one.
    manifest(&distro, "distro.erofs", &"0".repeat(64));
    let output = stage(&[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("sandbox distro manifest does not match its artifact"),
        "{stderr}"
    );
    assert_eq!(Staged::read(&store).unwrap(), Some(expected));

    // Staging again replaces the staged images: an image whose manifest matches, without a scratch
    // image.
    manifest(&distro, "distro.erofs", ZERO_KIB_SHA256);
    let output = stage(&[]);
    assert!(output.status.success(), "{output:?}");
    let staged = Staged::read(&store).unwrap().unwrap();
    assert!(staged.distro.binding.manifest().is_some());
    assert_eq!(staged.scratch, None);
}

#[test]
fn the_host_program_refuses_options_that_do_not_make_a_command() {
    let launch = ["launch", "store", "--guest-agent", "agent"];
    let uuid = "0a490905-41be-ed5f-984e-63eb7e5e20b1";
    let cases: [(&[&str], &str); 24] = [
        (
            &["--distro", "erofs:image", "--upper", "ram"],
            "--distro needs --distro-identity",
        ),
        (
            &["--distro", "erofs:image", "--distro-identity", uuid],
            "--distro needs --upper",
        ),
        (&["--upper", "ram"], "--upper needs --distro"),
        (
            &["--distro-identity", uuid],
            "--distro-identity needs --distro",
        ),
        (&["--runtime-dir", "/run/a"], "--runtime-dir needs --distro"),
        (
            &["--overlay-mode", "untrusted-lowers"],
            "--overlay-mode needs --distro",
        ),
        (
            &["--scratch-journal", "forbidden"],
            "--scratch-journal needs --distro",
        ),
        (
            &[
                "--distro",
                "erofs:image",
                "--distro-identity",
                uuid,
                "--upper",
                "ram",
                "--scratch-journal",
                "allowed",
            ],
            "--scratch-journal needs --upper scratch:PATH",
        ),
        (
            &["--first-partition-bound", "4096"],
            "--first-partition-bound needs --distro",
        ),
        (
            &[
                "--distro",
                "erofs:image",
                "--distro-identity",
                uuid,
                "--upper",
                "ram",
                "--first-partition-bound",
                "4096",
            ],
            "--first-partition-bound needs --distro gpt:PATH",
        ),
        (
            &["--first-partition-bound", "0"],
            "--first-partition-bound takes a positive number of bytes, not \"0\"",
        ),
        (
            &["--first-partition-bound", "64K"],
            "--first-partition-bound takes a positive number of bytes, not \"64K\"",
        ),
        (
            &["--overlay-mode", "trusted"],
            "--overlay-mode takes trusted-lowers or untrusted-lowers",
        ),
        (
            &["--scratch-journal", "none"],
            "--scratch-journal takes allowed or forbidden",
        ),
        (
            &["--distro", "squashfs:image"],
            "--distro takes FORMAT:PATH",
        ),
        (&["--distro", "erofs:"], "--distro takes FORMAT:PATH"),
        (&["--distro", "image"], "--distro takes FORMAT:PATH"),
        (&["--upper", "tmpfs"], "--upper takes scratch:PATH or ram"),
        (
            &["--upper", "scratch:"],
            "--upper takes scratch:PATH or ram",
        ),
        (
            &["--distro-identity", "identity"],
            "--distro-identity takes a UUID",
        ),
        (
            &["--upper", "ram", "--upper", "ram"],
            "unexpected option \"--upper\"",
        ),
        (&["mounts"], "unexpected argument \"mounts\" of launch"),
        (&["--hypervisor", "qemu"], "qemu"),
        (&["--distro"], "\"--distro\" needs a value"),
    ];
    let mut commands = cases
        .iter()
        .map(|(options, problem)| ([&launch[..], options].concat(), *problem))
        .collect::<Vec<_>>();
    let stage = ["stage", "store", "--distro", "erofs:image"];
    let stage_cases: [(&[&str], &str); 7] = [
        (&[], "stage needs --distro-identity"),
        (
            &["--distro-identity", uuid, "--upper", "ram"],
            "--upper of stage takes scratch:PATH, not \"ram\"",
        ),
        (
            &["--upper", "scratch:"],
            "--upper of stage takes scratch:PATH, not \"scratch:\"",
        ),
        (
            &["--guest-agent", "agent"],
            "unexpected option \"--guest-agent\" of stage",
        ),
        (
            &["--runtime-dir", "/run/a"],
            "unexpected option \"--runtime-dir\" of stage",
        ),
        (
            &["--guest-build", "build"],
            "unexpected option \"--guest-build\" of stage",
        ),
        (&["image"], "unexpected argument \"image\" of stage"),
    ];
    commands.extend(
        stage_cases
            .iter()
            .map(|(options, problem)| ([&stage[..], options].concat(), *problem)),
    );
    commands.extend([
        (vec!["stage"], "stage needs a sandbox store"),
        (vec!["stage", "store"], "stage needs --distro"),
        (vec!["probe", "store"], "probe needs a probe"),
        (
            vec!["probe", "store", "Mounts"],
            "the probe's name \"Mounts\"",
        ),
        (vec!["probe", "store", "mounts "], "is empty"),
        (
            vec!["probe", "store", "mounts", "--guest-agent", "agent"],
            "unexpected option \"--guest-agent\" of probe",
        ),
        (
            vec!["stop", "store", "mounts"],
            "unexpected argument \"mounts\" of stop",
        ),
        (
            vec!["reattach", "store", "--distro", "erofs:image"],
            "unexpected option \"--distro\" of reattach",
        ),
    ]);
    for (arguments, problem) in commands {
        let output = Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"))
            .args(&arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(problem) && stderr.contains("usage: runtime-adapter-host"),
            "{arguments:?}: {stderr}"
        );
    }
}
