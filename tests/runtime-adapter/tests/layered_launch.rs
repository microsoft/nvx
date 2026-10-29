//! The host program stages the images of a layered root, plans a VM with a layered root with NVX's
//! typed plan for an adapter's own guest, with the network that it reads, checks the staged
//! bindings of its images and hands OpenVMM the checked files, plans the export of the host paths
//! that it shares with the root inside the directories that the adapter authorizes, attaches it,
//! and verifies it before the launch, composes OpenVMM's arguments without NVX's managed lifecycle,
//! records what it launches, with the bindings that it checked, and refuses options that do not
//! make a command, networks that the plan refuses before it launches anything, shares that it
//! cannot export before it touches the store, executions whose starts the guest would not take, and
//! more executions than the guest runs at once. The root configuration says whether the network
//! publishes the guest's listeners, and executions find the URL of the network's proxy in their
//! environment, from the plan that the record keeps. The tests need no hypervisor.
#![cfg(feature = "host")]

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use nvx::exec::{Key, Limits, Timers, Window};
use nvx::host::artifacts::Artifacts;
use nvx::host::filesystem::GUEST_EXPORT;
use nvx::host::network;
use nvx::host::sandbox::scratch::CLAIM_RELEASE_WAIT;
use nvx::host::sandbox::{SandboxNetwork, SandboxPlan};
use nvx::protocol::root::{ImageFormat, RootImage, RootLayout, RootUpper};
use nvx::protocol::{HostMap, MAX_HOST_MAPPINGS};
use nvx::uuid::Uuid;
use nvx::{ErrorCode, ErrorKind, Hypervisor};
use runtime_adapter::exec::{MAX_EXECUTIONS, Start};
use runtime_adapter::hook::STEP_HOOK;
use runtime_adapter::layered::{
    Bindings, LaunchRecord, LayeredLaunch, PROXY_VARIABLES, RECORD_NAME, SANDBOX_TOKEN,
    read_network, with_proxy,
};
use runtime_adapter::root::{Listeners, RootConfig};
use runtime_adapter::sharing::{HostFsSharing, Share};
use runtime_adapter::staged::Staged;
use serde_json::{Value, json};

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
    Share::new(host, target, read_only)
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

/// Returns the tests' sharing of `sources`, which [`sources`] lays out, below `/srv/maps`: `dir`
/// read-only, `dir/inner` read-write inside it, `file.txt` read-only, and `out` read-write, with
/// `dir/secret` hidden, `/opt/adapter` reserved for the adapter, and `sources` authorized.
fn sharing(sources: &Path) -> HostFsSharing {
    let dir = sources.join("dir");
    let mut sharing = HostFsSharing::default();
    sharing.set_shares(vec![
        share(&dir, "/srv/maps/dir", true),
        share(&dir.join("inner"), "/srv/maps/dir/inner", false),
        share(&sources.join("file.txt"), "/srv/maps/file.txt", true),
        share(&sources.join("out"), "/srv/maps/out", false),
    ]);
    sharing.set_denied(vec![dir.join("secret")]);
    sharing.set_reserved(vec!["/opt/adapter".to_owned()]);
    sharing.set_roots(vec![sources.to_owned()]);
    sharing
}

/// Sets the guest path of the share at `index` of `sharing` to `target`.
fn retarget(sharing: &mut HostFsSharing, index: usize, target: &str) {
    let mut shares = sharing.shares().to_vec();
    shares[index].set_target(target);
    sharing.set_shares(shares);
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

/// Returns the serde form of an open network of a guest at `10.0.0.2/24`, from which the network
/// cases change one element at a time: egress allowed, without rules, ingress and host-loopback
/// access denied, no forwards, and no proxy.
fn open_network() -> Value {
    json!({
        "guestNetwork": "10.0.0.2/24",
        "egress": "allow",
        "allow": {"name": "egress.allow", "rules": []},
        "deny": {"name": "egress.deny", "rules": []},
        "ingress": "deny",
        "hostLoopback": "deny",
        "hostLoopbackForwards": [],
        "proxy": null,
    })
}

/// Returns the serde form of the open network, changed by `edit`.
fn network_with(edit: impl FnOnce(&mut Value)) -> Value {
    let mut network = open_network();
    edit(&mut network);
    network
}

/// Returns the OpenVMM arguments of a network of a guest at `guest`: the portable profile, with
/// the defaults `egress` and `ingress`, the host-loopback access `loopback`, or none for OpenVMM's
/// own, from the guest alone, then `more`.
fn portable<'a>(
    guest: &'a str,
    egress: &'a str,
    ingress: &'a str,
    loopback: Option<&'a str>,
    more: &[&'a str],
) -> Vec<&'a str> {
    let mut arguments = vec![
        "--net",
        guest,
        "--network-profile",
        "portable",
        "--network-egress",
        egress,
        "--network-ingress",
        ingress,
    ];
    if let Some(loopback) = loopback {
        arguments.extend(["--host-loopback", loopback]);
    }
    arguments.extend_from_slice(more);
    arguments
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
        listeners: Some(Listeners::Unpublished),
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
    // OpenVMM inherits the checked files, and on Windows the scratch image's claim too, which no
    // argument names.
    assert_eq!(
        prepared.inherited().len(),
        if cfg!(windows) { 3 } else { 2 }
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
    assert_eq!(prepared.inherited().len(), 1);
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
        listeners: Some(Listeners::Unpublished),
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
    assert_eq!(json["format"], 1);
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
        "format": 1,
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
        let share = Share::parse(text).unwrap_or_else(|| panic!("{text:?}"));
        assert_eq!(share.host(), Path::new(host), "{text:?}");
        assert_eq!(share.target(), target, "{text:?}");
        assert_eq!(share.is_read_only(), read_only, "{text:?}");
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
    let export = sharing(&sources).plan().unwrap().unwrap();

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

    // It shows an allowed path inside a hidden one again: here `out/sub` in `out`, which it hides.
    let out = sources.join("out");
    let mut shown = sharing(&sources);
    shown.add_denied(out.clone());
    shown.add_allowed(out.join("sub"));
    let shown = shown.plan().unwrap().unwrap();
    assert_eq!(shown.children[2].denied, [canonical(&out)]);
    assert_eq!(shown.children[2].allowed, [canonical(&out.join("sub"))]);
    assert!(export.children.iter().all(|child| child.allowed.is_empty()));

    // A target may lie below a path that the container root creates, though not at it.
    let mut below = sharing(&sources);
    retarget(&mut below, 2, "/etc/app/file.txt");
    assert!(below.plan().unwrap().is_some());

    // The launch announces the table in its root configuration, and attaches the export after
    // the image.
    let distro = image(directory.join("distro.erofs"), 3 * 512);
    let mut launch = staged_launch(&distro, None);
    launch.share(export.clone()).unwrap();
    assert_eq!(launch.export(), Some(&export));
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
    let mut nothing = HostFsSharing::default();
    nothing.add_root(sources.clone());
    assert_eq!(nothing.plan().unwrap(), None);
    let plain = staged_launch(&distro, None);
    assert!(plain.table().is_empty());
    assert_eq!(plain.export(), None);
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
    assert_eq!(launch.export(), None);
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
    let refused = |change: &dyn Fn(&mut HostFsSharing), problem: &str| {
        let mut request = sharing(&sources);
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
        &|request| request.add_share(share(&outside, "/srv/outside", false)),
        &unauthorized(&outside),
    );
    refused(
        &|request| request.set_roots(vec![dir.clone()]),
        &unauthorized(&file),
    );
    refused(
        &|request| request.set_roots(Vec::new()),
        &unauthorized(&dir),
    );
    let missing = directory.join("missing");
    refused(
        &|request| request.add_root(missing.clone()),
        &format!("--share-root entry {} is not accessible", missing.display()),
    );
    refused(
        &|request| request.set_roots(vec![file.clone()]),
        &format!("--share-root entry {} is not a directory", file.display()),
    );
    refused(
        &|request| request.add_share(share(&missing, "/srv/missing", true)),
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
            &|request| request.add_reserved(reserved),
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
            &|request| retarget(request, 0, target),
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
        &|request| request.add_share(share(&sub, "/srv/maps/out/sub", true)),
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
        &|request| request.add_denied(unborn.clone()),
        &format!("--deny entry {} does not exist", unborn.display()),
    );
    // An allowed path that no hidden path holds, which has nothing to show again.
    let marker = dir.join("marker");
    refused(
        &|request| request.add_allowed(marker.clone()),
        &format!(
            "--allow entry {} lies inside no --deny entry, so it has nothing to show again",
            marker.display()
        ),
    );

    // A link inside an authorized directory to one outside it.
    #[cfg(unix)]
    {
        let link = sources.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        refused(
            &|request| request.add_share(share(&link, "/srv/link", false)),
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
    let cases: [(Vec<OsString>, String); 5] = [
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
                "--allow".into(),
                dir.join("marker").into(),
                "--share-root".into(),
                root.clone(),
            ],
            "lies inside no --deny entry".to_owned(),
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
fn a_launch_renders_each_element_of_its_network_through_the_plan() {
    let directory = directory("layered-network");
    let distro = image(directory.join("distro.erofs"), 512);
    let scratch = image(directory.join("scratch.ext4"), 512);
    let (kernel, initramfs) = (directory.join("vmlinux"), directory.join("initramfs"));
    let report = directory.join("outcome.json");
    let without = staged_launch(&distro, Some(&scratch));
    // OpenVMM's arguments for `launch`, prepared anew, after the plan's block devices, which name
    // the checked files and come first.
    let arguments = |launch: &LayeredLaunch| {
        let prepared = launch.prepare().unwrap();
        let mut arguments = launch.openvmm_arguments(
            &prepared,
            Hypervisor::Kvm,
            &kernel,
            &initramfs,
            "/run/control.sock",
            &report,
        );
        let distro_file = checked(prepared.distro().file(), &distro);
        let scratch_file = checked(prepared.scratch().unwrap().file(), &scratch);
        let blocks: [OsString; 6] = [
            "--machine".into(),
            "microvm".into(),
            "--microvm-sandbox-block".into(),
            format!("distro:file:{distro_file},ro").into(),
            "--microvm-sandbox-block".into(),
            format!("scratch:file:{scratch_file}").into(),
        ];
        assert_eq!(arguments[..blocks.len()], blocks);
        arguments.split_off(blocks.len())
    };
    let options = arguments(&without);
    let cases: [(&str, Value, Vec<&str>, Option<&str>); 13] = [
        (
            "open",
            open_network(),
            portable("10.0.0.2/24", "allow", "deny", Some("deny"), &[]),
            None,
        ),
        (
            "guest-network",
            network_with(|network| network["guestNetwork"] = json!("192.168.7.6/30")),
            portable("192.168.7.6/30", "allow", "deny", Some("deny"), &[]),
            None,
        ),
        // A policy that lets nothing through attaches no network device.
        (
            "closed",
            network_with(|network| network["egress"] = json!("deny")),
            Vec::new(),
            None,
        ),
        (
            "allow",
            network_with(|network| {
                network["egress"] = json!("deny");
                network["allow"]["rules"] = json!([{
                    "to": [{"cidr": "192.0.2.0/24", "except": ["192.0.2.128/25"]}],
                    "ports": [{"protocol": "tcp", "port": 443, "endPort": null}],
                }]);
            }),
            portable(
                "10.0.0.2/24",
                "deny",
                "deny",
                Some("deny"),
                &["--network-egress-allow", "192.0.2.0/25:tcp:443"],
            ),
            None,
        ),
        (
            "allow-everywhere",
            network_with(|network| {
                network["egress"] = json!("deny");
                network["allow"]["rules"] = json!([{
                    "to": [],
                    "ports": [{"protocol": "any", "port": 8000, "endPort": 8010}],
                }]);
            }),
            portable(
                "10.0.0.2/24",
                "deny",
                "deny",
                Some("deny"),
                &[
                    "--network-egress-allow",
                    "0.0.0.0/0:tcp:8000-8010",
                    "--network-egress-allow",
                    "0.0.0.0/0:udp:8000-8010",
                    "--network-egress-allow",
                    "::/0:tcp:8000-8010",
                    "--network-egress-allow",
                    "::/0:udp:8000-8010",
                ],
            ),
            None,
        ),
        (
            "deny",
            network_with(|network| {
                network["deny"]["rules"] = json!([
                    {
                        "to": [{"cidr": "198.51.100.0/24", "except": []}],
                        "ports": [{"protocol": "udp", "port": 53, "endPort": null}],
                    },
                    {
                        "to": [{"cidr": "2001:db8::/32", "except": []}],
                        "ports": [{"protocol": "icmp", "port": null, "endPort": null}],
                    },
                ]);
            }),
            portable(
                "10.0.0.2/24",
                "allow",
                "deny",
                Some("deny"),
                &[
                    "--network-egress-deny",
                    "198.51.100.0/24:udp:53",
                    "--network-egress-deny",
                    "2001:db8::/32:icmp",
                ],
            ),
            None,
        ),
        (
            "ingress",
            network_with(|network| {
                network["egress"] = json!("deny");
                network["ingress"] = json!("allow");
            }),
            portable("10.0.0.2/24", "deny", "allow", Some("deny"), &[]),
            None,
        ),
        (
            "host-loopback-outbound",
            network_with(|network| network["hostLoopback"] = json!("outbound")),
            portable("10.0.0.2/24", "allow", "deny", None, &[]),
            None,
        ),
        (
            "host-loopback-allowed",
            network_with(|network| {
                network["egress"] = json!("deny");
                network["hostLoopback"] = json!("allow");
            }),
            portable("10.0.0.2/24", "deny", "deny", Some("allow"), &[]),
            None,
        ),
        (
            "forwards",
            network_with(|network| {
                network["hostLoopback"] = json!("allow");
                network["hostLoopbackForwards"] = json!([
                    {"protocol": "tcp", "hostPort": 18080, "guestPort": 8080},
                    {"protocol": "udp", "hostPort": 18081, "guestPort": 8081},
                ]);
            }),
            portable(
                "10.0.0.2/24",
                "allow",
                "deny",
                Some("allow"),
                &[
                    "--host-loopback-forward",
                    "tcp:18080:8080",
                    "--host-loopback-forward",
                    "udp:18081:8081",
                ],
            ),
            None,
        ),
        // A proxy-only network reaches its proxy at the gateway, which its URL names.
        (
            "proxy",
            network_with(|network| {
                network["egress"] = json!("deny");
                network["proxy"] = json!({"port": 3128, "scheme": null});
            }),
            portable(
                "10.0.0.2/24",
                "deny",
                "deny",
                Some("deny"),
                &["--network-proxy", "10.0.0.1:3128"],
            ),
            Some("10.0.0.1:3128"),
        ),
        (
            "proxy-http",
            network_with(|network| {
                network["egress"] = json!("deny");
                network["proxy"] = json!({"port": 3128, "scheme": "http"});
            }),
            portable(
                "10.0.0.2/24",
                "deny",
                "deny",
                Some("deny"),
                &["--network-proxy", "10.0.0.1:3128"],
            ),
            Some("http://10.0.0.1:3128"),
        ),
        (
            "proxy-https",
            network_with(|network| {
                network["guestNetwork"] = json!("192.168.7.6/30");
                network["egress"] = json!("deny");
                network["proxy"] = json!({"port": 8443, "scheme": "https"});
            }),
            portable(
                "192.168.7.6/30",
                "deny",
                "deny",
                Some("deny"),
                &["--network-proxy", "192.168.7.5:8443"],
            ),
            Some("https://192.168.7.5:8443"),
        ),
    ];
    for (name, form, rendered, proxy_url) in cases {
        let path = directory.join(format!("{name}.json"));
        fs::write(&path, form.to_string()).unwrap();
        let network = read_network(&path).unwrap();
        let launch = without.clone().with_network(Some(network.clone()));
        // NVX's lowering renders the network, as it does for NVX's own sandbox profile, and the
        // plan puts it after the block devices, before the launch's own options.
        assert_eq!(
            network::openvmm_arguments(Some(network.policy()), network.guest_network()).unwrap(),
            rendered,
            "{name}"
        );
        let expected = rendered
            .iter()
            .copied()
            .map(OsString::from)
            .chain(options.iter().cloned())
            .collect::<Vec<_>>();
        assert_eq!(arguments(&launch), expected, "{name}");
        assert_eq!(launch.plan.network(), Some(&network), "{name}");
        assert_eq!(launch.plan.proxy_url().as_deref(), proxy_url, "{name}");
        // The network adds no boot token: OpenVMM appends the guest's network settings itself.
        assert_eq!(
            launch.kernel_command_line().unwrap(),
            without.kernel_command_line().unwrap()
        );
        // The root configuration says whether OpenVMM publishes the guest's listeners: with
        // ingress allowed, or with host-loopback access allowed, which forwards need, but not
        // with outbound host-loopback access alone, nor through a proxy.
        let published = ["ingress", "host-loopback-allowed", "forwards"].contains(&name);
        let listeners = if published {
            Listeners::Published
        } else {
            Listeners::Unpublished
        };
        let config = RootConfig {
            listeners: Some(listeners),
            ..without.config.clone()
        };
        assert_eq!(launch.config, config, "{name}");

        // The record keeps the network in the plan, as the file gave it, and the root
        // configuration that says whether the listeners are published.
        let record = LaunchRecord {
            kernel_command_line: "nvx_sandbox=1",
            layered: Some(&launch),
        };
        let json = record.to_json().unwrap();
        assert_eq!(json["plan"]["network"], form, "{name}");
        let plan: SandboxPlan = serde_json::from_value(json["plan"].clone()).unwrap();
        assert_eq!(plan, launch.plan, "{name}");
        assert_eq!(plan.proxy_url().as_deref(), proxy_url, "{name}");
        let line = format!("\nlisteners={}\n", listeners.as_str());
        let encoded = json["rootConfiguration"].as_str().unwrap();
        assert!(encoded.ends_with(&line), "{name}: {encoded:?}");

        // Taking the network away leaves the launch as it was without one.
        assert_eq!(launch.with_network(None), without, "{name}");
    }
    // Without a network, the record says so, and nothing publishes the guest's listeners.
    assert_eq!(without.config.listeners, Some(Listeners::Unpublished));
    let record = LaunchRecord {
        kernel_command_line: "nvx_sandbox=1",
        layered: Some(&without),
    };
    assert_eq!(record.to_json().unwrap()["plan"]["network"], Value::Null);
}

#[test]
fn a_launch_reads_only_the_networks_that_the_plan_takes() {
    let directory = directory("layered-network-refused");
    let missing = directory.join("missing.json");
    let error = read_network(&missing).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert_eq!(
        error.message(),
        format!("cannot read the network {}", missing.display())
    );

    // Each refusal names its reason in its cause: the forms that the plan does not read, then the
    // networks that it refuses.
    let mut cases = vec![
        ("{".to_owned(), "EOF while parsing".to_owned()),
        ("null".to_owned(), "invalid type: null".to_owned()),
    ];
    let mut change = |problem: &str, edit: &dyn Fn(&mut Value)| {
        cases.push((network_with(edit).to_string(), problem.to_owned()));
    };
    change("unknown field `extra`", &|network| {
        network["extra"] = json!(null);
    });
    change("missing field `proxy`", &|network| {
        network.as_object_mut().unwrap().remove("proxy");
    });
    change("unknown variant `open`", &|network| {
        network["egress"] = json!("open");
    });
    change("unknown variant `both`", &|network| {
        network["hostLoopback"] = json!("both");
    });
    change("invalid value: integer `65536`", &|network| {
        network["hostLoopback"] = json!("allow");
        network["hostLoopbackForwards"] =
            json!([{"protocol": "tcp", "hostPort": 65536, "guestPort": 8080}]);
    });
    for guest_network in ["10.0.0.1/24", "10.0.0.0/24", "10.0.0.2/31", "10.0.0.2"] {
        change(
            &format!(
                "guest network {guest_network:?} must be an IPv4 address with a /1 to /30 prefix"
            ),
            &|network| network["guestNetwork"] = json!(guest_network),
        );
    }
    change(
        "egress.allow[0].to[0].cidr is not a valid IPv4 or IPv6 CIDR: \"192.0.2.0/33\"",
        &|network| {
            network["allow"]["rules"] =
                json!([{"to": [{"cidr": "192.0.2.0/33", "except": []}], "ports": []}]);
        },
    );
    change(
        "egress.deny[0].to[0].except[0] must be contained in 198.51.100.0/24",
        &|network| {
            network["deny"]["rules"] = json!([{
                "to": [{"cidr": "198.51.100.0/24", "except": ["192.0.2.0/25"]}],
                "ports": [],
            }]);
        },
    );
    change(
        "egress.allow[0].ports[0].endPort 80 cannot be below port 443",
        &|network| {
            network["allow"]["rules"] =
                json!([{"to": [], "ports": [{"protocol": "tcp", "port": 443, "endPort": 80}]}]);
        },
    );
    change(
        "egress.deny[0].ports[0].port is not supported with icmp",
        &|network| {
            network["deny"]["rules"] =
                json!([{"to": [], "ports": [{"protocol": "icmp", "port": 8, "endPort": null}]}]);
        },
    );
    // Hosts that never merge, one more than OpenVMM takes of one action.
    change("egress.allow emits at most 256 native rules", &|network| {
        let hosts = (0..=network::MAX_RULES)
            .map(|index| {
                let host = 2 * index + 1;
                json!({"cidr": format!("10.1.{}.{}", host / 256, host % 256), "except": []})
            })
            .collect::<Vec<_>>();
        network["egress"] = json!("deny");
        network["allow"]["rules"] = json!([{"to": hosts, "ports": []}]);
    });
    change(
        "host-loopback forwards need host-loopback access allowed",
        &|network| {
            network["hostLoopbackForwards"] =
                json!([{"protocol": "tcp", "hostPort": 18080, "guestPort": 8080}]);
        },
    );
    change(
        "host-loopback forwards bind tcp host port 18080 more than once",
        &|network| {
            network["hostLoopback"] = json!("allow");
            network["hostLoopbackForwards"] = json!([
                {"protocol": "tcp", "hostPort": 18080, "guestPort": 8080},
                {"protocol": "tcp", "hostPort": 18080, "guestPort": 8081},
            ]);
        },
    );
    change(
        "host-loopback UDP forwards need egress allowed by default for their replies",
        &|network| {
            network["egress"] = json!("deny");
            network["hostLoopback"] = json!("allow");
            network["hostLoopbackForwards"] =
                json!([{"protocol": "udp", "hostPort": 18081, "guestPort": 8081}]);
        },
    );
    change(
        "host-loopback access needs a guest network prefix of /29 or shorter",
        &|network| {
            network["guestNetwork"] = json!("10.0.0.2/30");
            network["hostLoopback"] = json!("allow");
        },
    );
    // A proxy is the guest's one way out.
    let proxy = |network: &mut Value| {
        network["egress"] = json!("deny");
        network["proxy"] = json!({"port": 3128, "scheme": "http"});
    };
    change(
        "a network proxy needs egress denied by default",
        &|network| {
            proxy(network);
            network["egress"] = json!("allow");
        },
    );
    change("a network proxy takes no egress.allow rules", &|network| {
        proxy(network);
        network["allow"]["rules"] = json!([{"to": [], "ports": []}]);
    });
    change("a network proxy needs ingress denied", &|network| {
        proxy(network);
        network["ingress"] = json!("allow");
    });
    change(
        "a network proxy needs host-loopback access denied",
        &|network| {
            proxy(network);
            network["hostLoopback"] = json!("outbound");
        },
    );
    change(
        "a network proxy takes no host-loopback forwards",
        &|network| {
            proxy(network);
            network["hostLoopbackForwards"] =
                json!([{"protocol": "tcp", "hostPort": 18080, "guestPort": 8080}]);
        },
    );
    change("a network proxy needs a port other than 53", &|network| {
        proxy(network);
        network["proxy"]["port"] = json!(53);
    });
    change(
        "a network proxy needs a port between 1 and 65535",
        &|network| {
            proxy(network);
            network["proxy"]["port"] = json!(0);
        },
    );
    change(
        "network proxy scheme \"ftp\" must be http or https",
        &|network| {
            proxy(network);
            network["proxy"]["scheme"] = json!("ftp");
        },
    );
    let path = directory.join("network.json");
    for (contents, problem) in cases {
        fs::write(&path, &contents).unwrap();
        let error = read_network(&path).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{problem}: {error}");
        assert_eq!(
            error.message(),
            format!("the network {} is refused", path.display())
        );
        let cause = std::error::Error::source(&error).unwrap().to_string();
        assert!(cause.contains(&problem), "{problem}: {cause}");
    }
}

/// Returns the start of an execution of `/usr/bin/env` with the environment of the `NAME=VALUE`
/// entries `environment`.
fn printing<E: Into<Vec<u8>>>(environment: impl IntoIterator<Item = E>) -> Start {
    let limits = Limits::new(16 * 1024, Window::frames(4), Window::frames(8)).unwrap();
    let timers = Timers::new(Duration::from_secs(1), Duration::from_secs(5));
    Start::new(Key::new(1, 2), limits, timers, ["/usr/bin/env"]).with_environment(environment)
}

/// Returns the network of a guest at `10.0.0.2/24` whose one way out is a proxy at port 3128 of
/// host loopback, with the scheme `scheme`, which the test `directory` reads from a file.
fn proxy_only(directory: &Path, scheme: Value) -> SandboxNetwork {
    let form = network_with(|network| {
        network["egress"] = json!("deny");
        network["proxy"] = json!({"port": 3128, "scheme": scheme});
    });
    let path = directory.join("proxy.json");
    fs::write(&path, form.to_string()).unwrap();
    read_network(&path).unwrap()
}

#[test]
fn an_execution_finds_the_url_of_its_network_s_proxy_in_its_environment() {
    let directory = directory("layered-proxy");
    let distro = image(directory.join("distro.erofs"), 512);
    let without = LayeredLaunch::new(&distro, ImageFormat::Erofs, identity(), None, None).unwrap();
    let own = ["PATH=/usr/bin:/bin", "EMPTY=", "TWICE=a=b"];

    // Each variable holds the URL of the proxy as the guest sees it, at its gateway, after the
    // start's own entries, which the start carries to the guest as its own.
    for (scheme, url) in [
        (Value::Null, "10.0.0.1:3128"),
        (json!("http"), "http://10.0.0.1:3128"),
        (json!("https"), "https://10.0.0.1:3128"),
    ] {
        let launch = without
            .clone()
            .with_network(Some(proxy_only(&directory, scheme)));
        let given = with_proxy(printing(own), &launch.plan);
        let proxies = PROXY_VARIABLES.map(|variable| format!("{variable}={url}"));
        let expected = own.into_iter().map(str::to_owned).chain(proxies);
        assert_eq!(given, printing(expected), "{url}");
        assert_eq!(Start::decode(&given.encode()), Ok(given), "{url}");
    }
    assert_eq!(
        PROXY_VARIABLES,
        ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"]
    );

    // A start whose caller chose its own proxy settings keeps them, even if they name only one of
    // the variables, since a client that reads another would not find the caller's.
    let launch = without
        .clone()
        .with_network(Some(proxy_only(&directory, json!("http"))));
    for variable in PROXY_VARIABLES {
        let chosen = [
            format!("{variable}=http://proxy.example:8080"),
            "A=1".to_owned(),
        ];
        let start = printing(chosen.clone());
        assert_eq!(
            with_proxy(start, &launch.plan),
            printing(chosen),
            "{variable}"
        );
    }
    // A name that only starts or ends like one of them is not one.
    let similar = ["http_proxy_port=1", "no_proxy=*", "XHTTP_PROXY=x"];
    let given = with_proxy(printing(similar), &launch.plan);
    assert_eq!(
        given.environment.len(),
        similar.len() + PROXY_VARIABLES.len()
    );

    // Without a proxy, an execution's environment is its start's, whatever the network.
    let path = directory.join("open.json");
    fs::write(&path, open_network().to_string()).unwrap();
    let open = without
        .clone()
        .with_network(Some(read_network(&path).unwrap()));
    for launch in [&without, &open] {
        assert_eq!(with_proxy(printing(own), &launch.plan), printing(own));
    }
}

#[test]
fn the_record_of_a_launch_gives_back_its_plan() {
    let directory = directory("layered-record-plan");
    let distro = image(directory.join("distro.erofs"), 512);
    let scratch = image(directory.join("scratch.ext4"), 512);
    let empty = directory.join("store");
    fs::create_dir_all(&empty).unwrap();
    let error = LaunchRecord::read_plan(&empty).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");

    // The plan of a layered root, with its network, from which the host gives executions the
    // URL of the network's proxy.
    let launch = LayeredLaunch::new(
        &distro,
        ImageFormat::Erofs,
        identity(),
        Some(&scratch),
        None,
    )
    .unwrap()
    .with_network(Some(proxy_only(&directory, json!("http"))));
    let record = LaunchRecord {
        kernel_command_line: "nvx_sandbox=1",
        layered: Some(&launch),
    };
    record.write(&directory).unwrap();
    let plan = LaunchRecord::read_plan(&directory).unwrap().unwrap();
    assert_eq!(plan, launch.plan);
    let given = with_proxy(printing(["A=1"]), &plan);
    assert_eq!(
        given.environment.last(),
        Some(&b"HTTPS_PROXY=http://10.0.0.1:3128".to_vec())
    );
    // A VM without a layered root has no plan.
    let blockless = LaunchRecord {
        kernel_command_line: "hostname=nvx-sandbox",
        layered: None,
    };
    blockless.write(&directory).unwrap();
    assert_eq!(LaunchRecord::read_plan(&directory).unwrap(), None);

    // A record that is not one of this format's, or whose plan is not a plan, is unavailable.
    let mut json = record.to_json().unwrap();
    let mut cases = vec![("{".to_owned(), "EOF while parsing")];
    json["format"] = json!(2);
    cases.push((json.to_string(), "the record is not in format 1"));
    json["format"] = json!(1);
    json.as_object_mut().unwrap().remove("plan");
    cases.push((json.to_string(), "the record has no plan"));
    json["plan"] = json!({"distro": 1});
    cases.push((json.to_string(), ""));
    cases.push(("[1]".to_owned(), "the record is not in format 1"));
    for (contents, cause) in cases {
        fs::write(directory.join(RECORD_NAME), &contents).unwrap();
        let error = LaunchRecord::read_plan(&directory).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable, "{contents}: {error}");
        let path = directory.join(RECORD_NAME);
        assert_eq!(
            error.message(),
            format!(
                "cannot read the plan of the launch record {}",
                path.display()
            )
        );
        let source = std::error::Error::source(&error).unwrap().to_string();
        assert!(source.contains(cause), "{contents}: {source}");
    }
}

#[test]
fn the_host_program_refuses_a_network_before_it_launches() {
    let directory = directory("layered-network-command");
    let distro = image(directory.join("distro.erofs"), 1024);
    let store = directory.join("store");
    // Without NVX's artifacts, a launch fails once it has read its network.
    let launch = |network: &Path| -> Output {
        let mut image = OsString::from("erofs:");
        image.push(&distro);
        Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"))
            .arg("launch")
            .arg(&store)
            .args(["--guest-agent", "agent", "--hypervisor", "kvm", "--distro"])
            .arg(image)
            .args(["--distro-identity", &identity().to_string()])
            .args(["--upper", "ram", "--network"])
            .arg(network)
            .env_remove(Artifacts::ENV_DIR)
            .output()
            .unwrap()
    };
    let failed = |output: Output| {
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        // Nothing was launched: the store was never created.
        assert!(!store.exists(), "{output:?}");
        String::from_utf8_lossy(&output.stderr).into_owned()
    };

    // A network that the plan refuses, and one that cannot be read, fail the launch first.
    let network = directory.join("network.json");
    let proxy = network_with(|network| network["proxy"] = json!({"port": 3128, "scheme": "http"}));
    fs::write(&network, proxy.to_string()).unwrap();
    let stderr = failed(launch(&network));
    let refused = format!(
        "the network {} is refused: a network proxy needs egress denied by default",
        network.display()
    );
    assert!(stderr.contains(&refused), "{stderr}");
    let missing = directory.join("missing.json");
    let stderr = failed(launch(&missing));
    let unreadable = format!("cannot read the network {}", missing.display());
    assert!(stderr.contains(&unreadable), "{stderr}");

    // A network that the plan takes passes, and the launch goes on to need NVX's artifacts.
    fs::write(&network, open_network().to_string()).unwrap();
    let stderr = failed(launch(&network));
    assert!(
        stderr.contains(&format!("set {} to NVX's artifacts", Artifacts::ENV_DIR)),
        "{stderr}"
    );
}

#[test]
fn the_host_program_refuses_options_that_do_not_make_a_command() {
    let launch = ["launch", "store", "--guest-agent", "agent"];
    let uuid = "0a490905-41be-ed5f-984e-63eb7e5e20b1";
    let cases: [(&[&str], &str); 38] = [
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
        (&["--network", "network.json"], "--network needs --distro"),
        (
            &[
                "--distro",
                "erofs:image",
                "--distro-identity",
                uuid,
                "--upper",
                "ram",
                "--network",
                "network.json",
                "--network",
                "other.json",
            ],
            "unexpected option \"--network\"",
        ),
        (&["--network"], "\"--network\" needs a value"),
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
        (&["--allow", "secret/shown"], "--allow needs --share"),
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
    let stage_cases: [(&[&str], &str); 10] = [
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
        (
            &["--network", "network.json"],
            "unexpected option \"--network\" of stage",
        ),
        (&["image"], "unexpected argument \"image\" of stage"),
    ];
    commands.extend(
        stage_cases
            .iter()
            .map(|(options, problem)| ([&stage[..], options].concat(), *problem)),
    );
    let create = [
        "create",
        "templates",
        "--guest-agent",
        "agent",
        "--distro",
        "erofs:image",
        "--distro-identity",
        uuid,
    ];
    let create_cases: [(&[&str], &str); 16] = [
        (
            &["--processors", "0"],
            "--processors takes a positive number, not \"0\"",
        ),
        (
            &["--processors", "two"],
            "--processors takes a positive number, not \"two\"",
        ),
        (
            &["--memory", "0"],
            "--memory takes a positive number of MiB, not \"0\"",
        ),
        (
            &["--memory", "256M"],
            "--memory takes a positive number of MiB, not \"256M\"",
        ),
        // A number of mebibytes whose bytes do not fit 64 bits.
        (
            &["--memory-capacity", "17592186044416"],
            "--memory-capacity takes a positive number of MiB, not \"17592186044416\"",
        ),
        (
            &["--scratch-length", "0"],
            "--scratch-length takes a positive number of bytes, not \"0\"",
        ),
        (
            &["--scratch-length", "64M"],
            "--scratch-length takes a positive number of bytes, not \"64M\"",
        ),
        (
            &["--cpu-profile", "auto", "--cpu-profile", "host"],
            "unexpected option \"--cpu-profile\" of create",
        ),
        (
            &["--upper", "ram"],
            "unexpected option \"--upper\" of create",
        ),
        (
            &["--runtime-dir", "/run/a"],
            "unexpected option \"--runtime-dir\" of create",
        ),
        (
            &["--workload-identity", "1000:1000"],
            "unexpected option \"--workload-identity\" of create",
        ),
        (
            &["--share", "a:/b"],
            "unexpected option \"--share\" of create",
        ),
        (
            &["--first-partition-bound", "4096"],
            "unexpected option \"--first-partition-bound\" of create",
        ),
        (
            &["--distro", "gpt:image"],
            "unexpected option \"--distro\" of create",
        ),
        (&["--network"], "\"--network\" needs a value"),
        (&["image"], "unexpected argument \"image\" of create"),
    ];
    commands.extend(
        create_cases
            .iter()
            .map(|(options, problem)| ([&create[..], options].concat(), *problem)),
    );
    commands.extend([
        (vec!["create"], "create needs a template store"),
        (vec!["create", "templates"], "create needs --guest-agent"),
        (
            vec!["create", "templates", "--guest-agent", "agent"],
            "create needs --distro",
        ),
        (
            vec![
                "create",
                "templates",
                "--guest-agent",
                "agent",
                "--distro",
                "erofs:image",
            ],
            "create needs --distro-identity",
        ),
        (
            vec![
                "launch",
                "store",
                "--guest-agent",
                "agent",
                "--cpu-profile",
                "host",
            ],
            "unexpected option \"--cpu-profile\" of launch",
        ),
        (
            vec![
                "launch",
                "store",
                "--guest-agent",
                "agent",
                "--processors",
                "2",
            ],
            "unexpected option \"--processors\" of launch",
        ),
        (
            vec!["stage", "store", "--memory", "256"],
            "unexpected option \"--memory\" of stage",
        ),
    ]);
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
        (
            vec!["stop", "store", "--network", "network.json"],
            "unexpected option \"--network\" of stop",
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
