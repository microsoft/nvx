//! The host program stages the images of a layered root, plans a VM with a layered root with NVX's
//! typed plan for an adapter's own guest, checks the staged bindings of its images and hands
//! OpenVMM the checked files, plans the export of the host paths that it shares with the root
//! inside the directories that the adapter authorizes, attaches it, and verifies it before the
//! launch, composes OpenVMM's arguments without NVX's managed lifecycle, records what it launches,
//! with the bindings that it checked, and refuses options that do not make a command, shares that
//! it cannot export before it touches the store, executions whose starts the guest would not take,
//! and more executions than the guest runs at once. The tests need no hypervisor.
#![cfg(feature = "host")]

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use nvx::host::artifacts::Artifacts;
use nvx::host::filesystem::GUEST_EXPORT;
use nvx::host::sandbox::SandboxPlan;
use nvx::host::sandbox::distro::DistroFormat;
use nvx::host::sandbox::scratch::CLAIM_RELEASE_WAIT;
use nvx::protocol::root::{ImageFormat, RootImage, RootLayout, RootUpper};
use nvx::protocol::{HostMap, MAX_HOST_MAPPINGS};
use nvx::uuid::Uuid;
use nvx::{ErrorCode, ErrorKind, Hypervisor};
use runtime_adapter::exec::MAX_EXECUTIONS;
use runtime_adapter::hook::STEP_HOOK;
use runtime_adapter::layered::{
    Bindings, LaunchRecord, LayeredLaunch, RECORD_NAME, SANDBOX_TOKEN, STAGED_NAME, Share, Shares,
    Staged,
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

/// Returns the launch of the image at `distro`, an EROFS file system of the tests' identity, over
/// the scratch image at `scratch`, or over RAM without one, with the bindings that staging them
/// makes.
fn staged_launch(distro: &Path, scratch: Option<&Path>) -> LayeredLaunch {
    let mut launch =
        LayeredLaunch::new(distro, ImageFormat::Erofs, identity(), scratch, None).unwrap();
    let staged = Staged::stage(distro, ImageFormat::Erofs, identity(), scratch).unwrap();
    launch.bindings = staged.bindings(&launch.plan);
    launch
}

/// Returns how OpenVMM's arguments name `file`, which a check opened at `path`: on Linux, by the
/// descriptor that OpenVMM inherits, and on Windows, by its final path.
fn checked(file: &File, path: &Path) -> String {
    #[cfg(target_os = "linux")]
    {
        let _ = path;
        format!("/proc/self/fd/{}", file.as_raw_fd())
    }
    #[cfg(windows)]
    {
        let _ = file;
        fs::canonicalize(path).unwrap().to_str().unwrap().to_owned()
    }
}

/// Changes the file at `path` in place: writes its first byte, a zero, again, then moves its
/// modification time an hour on, so that the change shows whatever the granularity of the file
/// system's times.
fn change(path: &Path) {
    let mut file = File::options().write(true).open(path).unwrap();
    let modified = file.metadata().unwrap().modified().unwrap();
    file.write_all(&[0]).unwrap();
    file.set_modified(modified + Duration::from_secs(3600))
        .unwrap();
}

/// Replaces the file at `path` with a copy of it, a file of its own, made before the original
/// leaves its path, so that no file system gives it the original's identity.
fn replace(path: &Path) {
    let copy = path.with_extension("copy");
    fs::copy(path, &copy).unwrap();
    fs::rename(&copy, path).unwrap();
}

/// Returns the share of the host path `host` at the guest path `target`.
fn share(host: &Path, target: &str, read_only: bool) -> Share {
    Share {
        host: host.to_owned(),
        target: target.to_owned(),
        read_only,
    }
}

/// Lays out the host sources of the tests' shares in `directory`, and returns the directory that
/// holds them, `sources`: the directory `dir`, with the file `marker`, the file `secret`, and the
/// directory `inner`, the file `file.txt`, and the directory `out`, with the directory `sub`.
fn sources(directory: &Path) -> PathBuf {
    let sources = directory.join("sources");
    fs::create_dir_all(sources.join("dir").join("inner")).unwrap();
    fs::write(sources.join("dir").join("marker"), "marker").unwrap();
    fs::write(sources.join("dir").join("secret"), "secret").unwrap();
    fs::write(sources.join("file.txt"), "file").unwrap();
    fs::create_dir_all(sources.join("out").join("sub")).unwrap();
    sources
}

/// Returns the tests' shares of `sources`, which [`sources`] lays out, below `/srv/maps`: `dir`
/// read-only, `dir/inner` read-write inside it, `file.txt` read-only, and `out` read-write, with
/// `dir/secret` hidden, `/opt/adapter` reserved for the adapter, and `sources` authorized.
fn shares(sources: &Path) -> Shares {
    let dir = sources.join("dir");
    Shares {
        shares: vec![
            share(&dir, "/srv/maps/dir", true),
            share(&dir.join("inner"), "/srv/maps/dir/inner", false),
            share(&sources.join("file.txt"), "/srv/maps/file.txt", true),
            share(&sources.join("out"), "/srv/maps/out", false),
        ],
        denied: vec![dir.join("secret")],
        reserved: vec!["/opt/adapter".to_owned()],
        roots: vec![sources.to_owned()],
    }
}

/// Returns the canonical path of `path`, as an export plan names it: without the verbatim prefix
/// that Windows gives it.
fn canonical(path: &Path) -> PathBuf {
    let canonical = fs::canonicalize(path).unwrap();
    let text = canonical.to_str().unwrap();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text))
}

/// Checks that `result` failed with [`ErrorKind::Unsupported`] and a description that holds
/// `problem`.
fn assert_refused<T: std::fmt::Debug>(result: nvx::Result<T>, problem: &str) {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    assert!(error.to_string().contains(problem), "{problem}: {error}");
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

    // The plan's OpenVMM arguments attach the image read-only and the scratch, with no workload
    // identity.
    let blocks = [
        "--machine",
        "microvm",
        "--microvm-sandbox-block",
        &format!("distro:file:{},ro", distro.display()),
        "--microvm-sandbox-block",
        &format!("scratch:file:{}", scratch.display()),
    ];
    assert_eq!(launch.plan.openvmm_arguments().unwrap(), blocks);
}

#[test]
fn a_layered_launch_hands_openvmm_the_checked_images() {
    let directory = directory("layered-handoff");
    let distro = image(directory.join("distro.erofs"), 3 * 512);
    let scratch = image(directory.join("scratch.ext4"), 8 * 512);
    let launch = staged_launch(&distro, Some(&scratch));
    let prepared = launch.prepare().unwrap();
    assert_eq!(prepared.plan(), &launch.plan);
    let command_line = launch.kernel_command_line().unwrap();
    assert_eq!(prepared.kernel_command_line(), command_line);

    // OpenVMM's arguments: the plan's, in which each block device names its checked file rather
    // than its path, then the launch's, with no managed lifecycle.
    let distro_file = checked(prepared.distro().file(), &distro);
    let scratch_file = checked(prepared.scratch().unwrap().file(), &scratch);
    let (kernel, initramfs) = (directory.join("vmlinux"), directory.join("initramfs"));
    let report = directory.join("outcome.json");
    let arguments = launch.openvmm_arguments(
        &prepared,
        Hypervisor::Kvm,
        &kernel,
        &initramfs,
        "/run/control.sock",
        &report,
    );
    let expected: [OsString; 24] = [
        "--machine".into(),
        "microvm".into(),
        "--microvm-sandbox-block".into(),
        format!("distro:file:{distro_file},ro").into(),
        "--microvm-sandbox-block".into(),
        format!("scratch:file:{scratch_file}").into(),
        "--single-process".into(),
        "--hypervisor".into(),
        "kvm".into(),
        "--memory".into(),
        "256M".into(),
        "--kernel".into(),
        kernel.clone().into(),
        "--initrd".into(),
        initramfs.clone().into(),
        "--cmdline".into(),
        command_line.into(),
        "--virtio-console".into(),
        "none".into(),
        "--microvm-control-console".into(),
        "listen=/run/control.sock".into(),
        "--microvm-control-auth-stdin".into(),
        "--microvm-report".into(),
        report.clone().into(),
    ];
    assert_eq!(arguments, expected);
    // OpenVMM inherits the checked files on Linux, and on Windows the scratch image's claim, which
    // no argument names.
    assert_eq!(
        prepared.inherited().len(),
        if cfg!(windows) { 1 } else { 2 }
    );

    // An upper in RAM hands OpenVMM the image alone, and claims nothing, while the image's checks
    // exclude no other.
    let ram = staged_launch(&distro, None);
    let prepared = ram.prepare().unwrap();
    assert!(prepared.scratch().is_none());
    let arguments = ram.openvmm_arguments(
        &prepared,
        Hypervisor::Kvm,
        &kernel,
        &initramfs,
        "/run/control.sock",
        &report,
    );
    let distro_file = checked(prepared.distro().file(), &distro);
    let blocks: [OsString; 4] = [
        "--machine".into(),
        "microvm".into(),
        "--microvm-sandbox-block".into(),
        format!("distro:file:{distro_file},ro").into(),
    ];
    assert_eq!(arguments[..4], blocks);
    assert_eq!(arguments[4], "--single-process");
    assert_eq!(
        prepared.inherited().len(),
        if cfg!(windows) { 0 } else { 1 }
    );
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
fn a_launch_refuses_images_that_are_not_staged_or_changed_since() {
    let directory = directory("layered-refusals");
    let distro = image(directory.join("distro.erofs"), 1024);
    let scratch = image(directory.join("scratch.ext4"), 1024);
    let refused = |launch: &LayeredLaunch, problem: &str| {
        let error = launch.prepare().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
        assert!(error.message().contains(problem), "{problem}: {error}");
    };

    // A launch needs the bindings of its images, staged at their paths.
    let mut launch = LayeredLaunch::new(
        &distro,
        ImageFormat::Erofs,
        identity(),
        Some(&scratch),
        None,
    )
    .unwrap();
    let image_path = absolute(&distro).display().to_string();
    let scratch_path = absolute(&scratch).display().to_string();
    refused(
        &launch,
        &format!("the sandbox's store stages no image at {image_path}"),
    );
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), None).unwrap();
    launch.bindings = staged.bindings(&launch.plan);
    refused(
        &launch,
        &format!("the sandbox's store stages no scratch image at {scratch_path}"),
    );

    // The binding of the image names its format and identity, which the launch's must be.
    let other = Uuid::parse("0a490905-41be-ed5f-984e-63eb7e5e20b2").unwrap();
    for (format, identity, problem) in [
        (
            ImageFormat::Gpt,
            identity(),
            format!(
                "names the erofs layer with identity {}, not the gpt layer with identity {}",
                identity(),
                identity()
            ),
        ),
        (
            ImageFormat::Erofs,
            other,
            format!(
                "names the erofs layer with identity {}, not {other}",
                identity()
            ),
        ),
    ] {
        let mut launch = LayeredLaunch::new(&distro, format, identity, None, None).unwrap();
        launch.bindings = staged.bindings(&launch.plan);
        refused(&launch, &problem);
    }

    // An image that changed after it was staged, a scratch image that another file replaced, and
    // an image that is gone fail the checks.
    let launch = staged_launch(&distro, Some(&scratch));
    change(&distro);
    refused(
        &launch,
        &format!(
            "{image_path} changed after its distro binding was made: its modification time differs"
        ),
    );
    let launch = staged_launch(&distro, Some(&scratch));
    replace(&scratch);
    refused(
        &launch,
        &format!("{scratch_path} changed after its scratch binding was made: its identity differs"),
    );
    let launch = staged_launch(&distro, Some(&scratch));
    fs::remove_file(&distro).unwrap();
    refused(&launch, &format!("not found: {image_path}"));
}

#[test]
fn a_launch_claims_its_scratch_image_until_it_releases_its_checked_files() {
    let directory = directory("layered-claims");
    let distro = image(directory.join("distro.erofs"), 1024);
    let scratch = image(directory.join("scratch.ext4"), 1024);
    let launch = staged_launch(&distro, Some(&scratch));
    let first = launch.prepare().unwrap();

    // Another launch over the scratch image waits for the claim, then fails.
    let started = Instant::now();
    let error = launch.prepare().unwrap_err();
    assert!(started.elapsed() >= CLAIM_RELEASE_WAIT, "{error}");
    assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
    assert_eq!(error.code(), ErrorCode::ScratchClaimed, "{error}");

    // Once the first launch releases its checked files, the next one claims the image.
    drop(first);
    launch.prepare().unwrap();
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
    let launch = staged_launch(&distro, Some(&scratch));
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
    // The bindings that the launch checks, in their serde forms.
    let image_binding = serde_json::to_value(launch.bindings.distro.as_ref().unwrap()).unwrap();
    let scratch_binding = serde_json::to_value(launch.bindings.scratch.as_ref().unwrap()).unwrap();
    let expected = json!({"distro": image_binding, "scratch": scratch_binding});
    assert_eq!(json["bindings"], expected);

    // The record replaces an earlier one in the store.
    fs::write(directory.join(RECORD_NAME), "earlier").unwrap();
    record.write(&directory).unwrap();
    let written = fs::read_to_string(directory.join(RECORD_NAME)).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&written).unwrap(), json);

    // An upper in RAM has no scratch image to bind.
    let ram = staged_launch(&distro, None);
    let record = LaunchRecord {
        kernel_command_line: "nvx_sandbox=1",
        layered: Some(&ram),
    };
    let expected = json!({"distro": image_binding, "scratch": null});
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
fn shares_read_their_host_path_their_target_and_their_access() {
    let shares = [
        ("/data:/srv/data", "/data", "/srv/data", false),
        ("/data:/srv/data:ro", "/data", "/srv/data", true),
        (r"C:\data:/srv/data", r"C:\data", "/srv/data", false),
        ("C:/data:/srv/data:ro", "C:/data", "/srv/data", true),
        // The guest path starts at the last `:/`, and may hold other colons.
        ("/a:/b:/srv/c:d", "/a:/b", "/srv/c:d", false),
        // The planner refuses the guest's root.
        ("/data:/", "/data", "/", false),
    ];
    for (text, host, target, read_only) in shares {
        let expected = share(Path::new(host), target, read_only);
        assert_eq!(Share::parse(text), Some(expected), "{text:?}");
    }
    // Text without a host path, or without a guest path that starts with `/`.
    for text in ["/data", ":/srv/data", ":/srv/data:ro", "/data:srv/data", ""] {
        assert_eq!(Share::parse(text), None, "{text:?}");
    }
}

#[test]
fn a_launch_shares_host_paths_inside_the_authorized_directories() {
    let directory = directory("layered-shares");
    let sources = sources(&directory);
    let export = shares(&sources).plan().unwrap().unwrap();

    // The guest maps each share at its target, each before the mappings inside its target, from
    // the export's directories and files, the outermost shares, in order.
    let mapping = |source: &str, target: &str, read_only| HostMap {
        source: source.to_owned(),
        target: target.to_owned(),
        read_only,
    };
    let table = [
        mapping("0", "/srv/maps/dir", true),
        mapping("1", "/srv/maps/file.txt", true),
        mapping("2", "/srv/maps/out", false),
        mapping("0/inner", "/srv/maps/dir/inner", false),
    ];
    assert_eq!(export.guest_table(), table);
    let dir = sources.join("dir");
    let children = export
        .children
        .iter()
        .map(|child| (child.root.clone(), child.file, child.writable))
        .collect::<Vec<_>>();
    let expected = [
        (canonical(&dir), false, true),
        (canonical(&sources.join("file.txt")), true, false),
        (canonical(&sources.join("out")), false, true),
    ];
    assert_eq!(children, expected);
    // OpenVMM hides the denied path, and lets the guest write only `dir/inner` in `dir`.
    assert_eq!(export.children[0].denied, [canonical(&dir.join("secret"))]);
    assert_eq!(export.children[0].write, [canonical(&dir.join("inner"))]);

    // A target may lie below a path that the container root creates, though not at it.
    let mut below = shares(&sources);
    below.shares[2].target = "/etc/app/file.txt".to_owned();
    assert!(below.plan().unwrap().is_some());

    // The launch announces the table in its root configuration, and attaches the export after
    // the image.
    let distro = image(directory.join("distro.erofs"), 3 * 512);
    let mut launch = staged_launch(&distro, None);
    launch.share(export.clone()).unwrap();
    assert_eq!(launch.export.as_ref(), Some(&export));
    assert_eq!(launch.config.mapping_count, 4);
    assert_eq!(launch.table(), table);
    let config = String::from_utf8(launch.config.encode()).unwrap();
    assert!(config.contains("\nmapping-count=4\n"), "{config}");
    let prepared = launch.prepare().unwrap();
    let arguments = launch.openvmm_arguments(
        &prepared,
        Hypervisor::Kvm,
        &directory.join("vmlinux"),
        &directory.join("initramfs"),
        "/run/control.sock",
        &directory.join("outcome.json"),
    );
    let exported = export
        .openvmm_arguments()
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    assert_eq!(exported[..2], ["--mount-aggregate", GUEST_EXPORT]);
    assert_eq!(arguments[4..4 + exported.len()], exported[..]);
    assert_eq!(arguments[4 + exported.len()], "--single-process");
    drop(prepared);

    // A launch that shares nothing announces no table, and attaches no export.
    let nothing = Shares {
        roots: vec![sources.clone()],
        ..Shares::default()
    };
    assert_eq!(nothing.plan().unwrap(), None);
    let plain = staged_launch(&distro, None);
    assert!(plain.table().is_empty());
    assert_eq!(plain.config.mapping_count, 0);

    // The launch verifies the export before it prepares anything: a directory that replaced a
    // shared file would grant all of itself.
    fs::remove_file(sources.join("file.txt")).unwrap();
    fs::create_dir(sources.join("file.txt")).unwrap();
    let error = launch.prepare().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other, "{error}");
    assert!(
        error
            .to_string()
            .contains("no longer names the host object"),
        "{error}"
    );

    // A table that the guest would refuse fails before the launch announces it.
    let mut launch = staged_launch(&distro, None);
    let mut too_many = export;
    too_many.binds = vec![too_many.binds[0].clone(); MAX_HOST_MAPPINGS + 1];
    assert_refused(launch.share(too_many), "the mapping table does not encode");
    assert_eq!(launch.export, None);
    assert_eq!(launch.config.mapping_count, 0);
}

#[test]
fn a_launch_refuses_shares_that_it_cannot_export() {
    let directory = directory("layered-share-refusals");
    let sources = sources(&directory);
    let outside = directory.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let (dir, file, out) = (
        sources.join("dir"),
        sources.join("file.txt"),
        sources.join("out"),
    );
    let refused = |change: &dyn Fn(&mut Shares), problem: &str| {
        let mut request = shares(&sources);
        change(&mut request);
        assert_refused(request.plan(), problem);
    };
    let unauthorized = |path: &Path| {
        format!(
            "--share entry {} lies outside the host directories that --share-root authorizes",
            path.display()
        )
    };

    // Each share must lie inside a directory that the adapter authorizes.
    refused(
        &|request| request.shares.push(share(&outside, "/srv/outside", false)),
        &unauthorized(&outside),
    );
    refused(
        &|request| request.roots = vec![dir.clone()],
        &unauthorized(&file),
    );
    refused(&|request| request.roots.clear(), &unauthorized(&dir));
    let missing = directory.join("missing");
    refused(
        &|request| request.roots.push(missing.clone()),
        &format!("--share-root entry {} is not accessible", missing.display()),
    );
    refused(
        &|request| request.roots = vec![file.clone()],
        &format!("--share-root entry {} is not a directory", file.display()),
    );
    refused(
        &|request| request.shares.push(share(&missing, "/srv/missing", true)),
        &format!("--share entry {} is not accessible", missing.display()),
    );

    // The adapter's reserved trees are absolute and canonical guest paths, which no target takes
    // or covers, as no target takes or covers those of the container root.
    for (reserved, problem) in [
        ("opt", "is not absolute"),
        ("/", "is not canonical, or is the root"),
        ("/opt/", "is not canonical, or is the root"),
    ] {
        refused(
            &|request| request.reserved.push(reserved.to_owned()),
            &format!("--reserved entry {reserved:?} {problem}"),
        );
    }
    for (target, reserved) in [
        ("/opt/adapter/dir", "/opt/adapter"),
        ("/opt", "/opt/adapter"),
        ("/proc/dir", "/proc"),
        ("/etc", "/etc"),
        ("/run/nvx", "/run/nvx"),
    ] {
        refused(
            &|request| request.shares[0].target = target.to_owned(),
            &format!(
                "--share entry {} has the guest target {target}, which would cover the guest's \
                 {reserved}",
                dir.display()
            ),
        );
    }

    // A read-only share inside a read-write one, which the host could not keep read-only (#552).
    let sub = out.join("sub");
    refused(
        &|request| request.shares.push(share(&sub, "/srv/maps/out/sub", true)),
        &format!(
            "--share entry {} lies inside the --share entry {}, where the host cannot keep it \
             read-only",
            sub.display(),
            out.display()
        ),
    );
    // A denied path that a workload could create inside a share, which nothing would hide.
    let unborn = dir.join("unborn");
    refused(
        &|request| request.denied.push(unborn.clone()),
        &format!("--deny entry {} does not exist", unborn.display()),
    );

    // A link inside an authorized directory to one outside it.
    #[cfg(unix)]
    {
        let link = sources.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        refused(
            &|request| request.shares.push(share(&link, "/srv/link", false)),
            &unauthorized(&link),
        );
    }
}

#[test]
fn the_host_program_refuses_shares_before_it_touches_the_store() {
    let directory = directory("layered-share-command");
    // NVX's artifacts in a release's layout, which the launch takes, but which boot nothing.
    let artifacts = directory.join("artifacts");
    let openvmm = if cfg!(windows) {
        "bin/openvmm.exe"
    } else {
        "bin/openvmm"
    };
    let mut sums = String::new();
    for name in [openvmm, "guest/vmlinux", "guest/initramfs.cpio.gz"] {
        let path = artifacts.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, name).unwrap();
        sums.push_str(&format!("{}  {name}\n", "0".repeat(64)));
    }
    fs::write(artifacts.join("SHA256SUMS"), sums).unwrap();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../SOURCE-MANIFEST.json");
    fs::copy(manifest, artifacts.join("SOURCE-MANIFEST.json")).unwrap();

    let sources = sources(&directory);
    let outside = directory.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let distro = image(directory.join("distro.erofs"), 1024);
    let store = directory.join("store");
    let shared = |path: &Path, access: &str| {
        let mut value = path.as_os_str().to_owned();
        value.push(access);
        value
    };
    let root = sources.as_os_str().to_owned();
    let (dir, out) = (sources.join("dir"), sources.join("out"));
    let cases: [(Vec<OsString>, String); 4] = [
        (
            vec![
                "--share".into(),
                shared(&outside, ":/srv/outside"),
                "--share-root".into(),
                root.clone(),
            ],
            format!(
                "--share entry {} lies outside the host directories that --share-root authorizes",
                outside.display()
            ),
        ),
        (
            vec![
                "--share".into(),
                shared(&dir, ":/opt/adapter/dir:ro"),
                "--reserved".into(),
                "/opt/adapter".into(),
                "--share-root".into(),
                root.clone(),
            ],
            "which would cover the guest's /opt/adapter".to_owned(),
        ),
        (
            vec![
                "--share".into(),
                shared(&out, ":/srv/out"),
                "--share".into(),
                shared(&out.join("sub"), ":/srv/out/sub:ro"),
                "--share-root".into(),
                root.clone(),
            ],
            "where the host cannot keep it read-only".to_owned(),
        ),
        (
            vec![
                "--share".into(),
                shared(&dir, ":/srv/dir"),
                "--reserved".into(),
                "opt".into(),
                "--share-root".into(),
                root,
            ],
            "--reserved entry \"opt\" is not absolute".to_owned(),
        ),
    ];
    for (options, problem) in cases {
        let mut image = OsString::from("erofs:");
        image.push(&distro);
        let output = Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"))
            .arg("launch")
            .arg(&store)
            .arg("--guest-agent")
            .arg(directory.join("agent"))
            .args(["--hypervisor", if cfg!(windows) { "whp" } else { "kvm" }])
            .arg("--distro")
            .arg(image)
            .args([
                "--distro-identity",
                &identity().to_string(),
                "--upper",
                "ram",
            ])
            .args(&options)
            .env(Artifacts::ENV_DIR, &artifacts)
            .env_remove(STEP_HOOK)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{options:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{options:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&problem), "{options:?}: {stderr}");
        assert!(!store.exists(), "{options:?}");
    }
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
    let cases: [(&[&str], &str); 34] = [
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
            &["--workload-identity", "1000:1000"],
            "--workload-identity needs --distro",
        ),
        (
            &["--workload-identity", "0:1000"],
            "--workload-identity takes UID:GID, two numbers from 1 to 4294967294, not \"0:1000\"",
        ),
        (
            &["--workload-identity", "1000:4294967295"],
            "--workload-identity takes UID:GID",
        ),
        (
            &["--workload-identity", "1000"],
            "--workload-identity takes UID:GID",
        ),
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
        (&["--share", "a:/b"], "--share needs --distro"),
        (&["--deny", "secret"], "--deny needs --share"),
        (&["--reserved", "/opt/adapter"], "--reserved needs --share"),
        (&["--share-root", "sources"], "--share-root needs --share"),
        (
            &["--share", "a"],
            "--share takes HOST:TARGET or HOST:TARGET:ro, with an absolute guest path TARGET, \
             not \"a\"",
        ),
        (
            &["--share", ":/b:ro"],
            "--share takes HOST:TARGET or HOST:TARGET:ro",
        ),
    ];
    let mut commands = cases
        .iter()
        .map(|(options, problem)| ([&launch[..], options].concat(), *problem))
        .collect::<Vec<_>>();
    let stage = ["stage", "store", "--distro", "erofs:image"];
    let stage_cases: [(&[&str], &str); 9] = [
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
        (
            &["--share", "a:/b"],
            "unexpected option \"--share\" of stage",
        ),
        (
            &["--share-root", "sources"],
            "unexpected option \"--share-root\" of stage",
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
        (
            vec!["reattach", "store", "--share", "a:/b"],
            "unexpected option \"--share\" of reattach",
        ),
    ]);
    // Each execution of `exec` has options of its own, before `--` and its program, and its
    // arguments end at a lone `;`. The program refuses a start that the guest would not decode,
    // on which the guest would end its session, and more executions than the guest runs at once,
    // before it attaches.
    let program = ["--", "/bin/true"];
    let exec = |options: &[&'static str]| [&["exec", "store"], options, &program[..]].concat();
    commands.extend([
        (
            vec!["exec", "store"],
            "exec needs `-- PROGRAM` after the options of each execution",
        ),
        (
            vec!["exec", "store", "--"],
            "exec needs a program after `--`",
        ),
        (
            exec(&[]).into_iter().chain([";"]).collect(),
            "exec needs `-- PROGRAM` after the options of each execution",
        ),
        (exec(&["--groups", "1"]), "--groups needs --identity"),
        (
            exec(&["--capabilities", "0"]),
            "--capabilities needs --identity",
        ),
        (exec(&["--numeric"]), "--numeric needs --identity"),
        (
            exec(&["--identity", "1000"]),
            "--identity takes UID:GID, not \"1000\"",
        ),
        (
            exec(&["--identity", "0:0", "--groups", "1,,2"]),
            "--groups takes GIDs, separated by commas, not \"1,,2\"",
        ),
        (
            exec(&["--identity", "0:0", "--capabilities", "64"]),
            "--capabilities takes capability numbers from 0 to 63",
        ),
        (
            exec(&["--cwd", "tmp"]),
            "names a working directory that is not absolute",
        ),
        (
            exec(&["--env", "=value"]),
            "has an environment entry that is not NAME=VALUE",
        ),
        (
            exec(&["--cwd", "/a", "--cwd", "/b"]),
            "unexpected option \"--cwd\" of exec",
        ),
        (
            exec(&["--distro", "erofs:image"]),
            "unexpected option \"--distro\" of exec",
        ),
        (vec!["exec", "store", "--env"], "\"--env\" needs a value"),
        (
            [
                exec(&["--identity", "0:0", "--groups", "1"]),
                vec![";"],
                exec(&["--groups", "1"])[2..].to_vec(),
            ]
            .concat(),
            "--groups needs --identity",
        ),
        (
            [
                vec!["exec", "store"],
                vec![program.to_vec(); MAX_EXECUTIONS + 1].join(&";"),
            ]
            .concat(),
            "exec runs at most",
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
