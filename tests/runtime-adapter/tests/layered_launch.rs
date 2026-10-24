//! The host program stages the images of a layered root, plans a VM with a layered root with NVX's
//! typed plan for an adapter's own guest, with the network that it reads, composes OpenVMM's
//! arguments without NVX's managed lifecycle, records what it launches, with the bindings that the
//! store stages, and refuses options that do not make a command, and networks that the plan
//! refuses, before it launches anything. The tests need no hypervisor.
#![cfg(feature = "host")]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nvx::host::artifacts::Artifacts;
use nvx::host::network;
use nvx::host::sandbox::SandboxPlan;
use nvx::host::sandbox::distro::DistroFormat;
use nvx::protocol::root::{ImageFormat, RootImage, RootLayout, RootUpper};
use nvx::uuid::Uuid;
use nvx::{ErrorKind, Hypervisor};
use runtime_adapter::layered::{
    Bindings, LaunchRecord, LayeredLaunch, RECORD_NAME, SANDBOX_TOKEN, STAGED_NAME, Staged,
    read_network,
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
fn a_launch_renders_each_element_of_its_network_through_the_plan() {
    let directory = directory("layered-network");
    let distro = image(directory.join("distro.erofs"), 512);
    let scratch = image(directory.join("scratch.ext4"), 512);
    let (kernel, initramfs) = (directory.join("vmlinux"), directory.join("initramfs"));
    let report = directory.join("outcome.json");
    let without = LayeredLaunch::new(
        &distro,
        ImageFormat::Erofs,
        identity(),
        Some(&scratch),
        None,
    )
    .unwrap();
    let arguments = |launch: &LayeredLaunch| {
        launch
            .openvmm_arguments(
                Hypervisor::Kvm,
                &kernel,
                &initramfs,
                "/run/control.sock",
                &report,
            )
            .unwrap()
    };
    let blocks = without.plan.openvmm_arguments().unwrap();
    let options = arguments(&without).split_off(blocks.len());
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
        let mut launch = without.clone();
        launch.plan = launch.plan.with_network(Some(network.clone()));
        // NVX's lowering renders the network, as it does for NVX's own sandbox profile, and the
        // plan puts it after the block devices, before the launch's own options.
        assert_eq!(
            network::openvmm_arguments(Some(network.policy()), network.guest_network()).unwrap(),
            rendered,
            "{name}"
        );
        let expected = blocks
            .iter()
            .map(String::as_str)
            .chain(rendered.iter().copied())
            .map(OsString::from)
            .chain(options.iter().cloned())
            .collect::<Vec<_>>();
        assert_eq!(arguments(&launch), expected, "{name}");
        assert_eq!(launch.plan.proxy_url().as_deref(), proxy_url, "{name}");
        // The network adds no boot token: OpenVMM appends the guest's network settings itself.
        assert_eq!(
            launch.kernel_command_line().unwrap(),
            without.kernel_command_line().unwrap()
        );

        // The record keeps the network in the plan, as the file gave it.
        let record = LaunchRecord {
            kernel_command_line: "nvx_sandbox=1",
            layered: Some(&launch),
        };
        let json = record.to_json().unwrap();
        assert_eq!(json["plan"]["network"], form, "{name}");
        let plan: SandboxPlan = serde_json::from_value(json["plan"].clone()).unwrap();
        assert_eq!(plan, launch.plan, "{name}");
        assert_eq!(plan.proxy_url().as_deref(), proxy_url, "{name}");
    }
    // Without a network, the record says so.
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
                "guest network '{guest_network}' must be an IPv4 address with a /1 to /30 prefix"
            ),
            &|network| network["guestNetwork"] = json!(guest_network),
        );
    }
    change(
        "egress.allow[0].to[0].cidr is not a valid IPv4 or IPv6 CIDR: 192.0.2.0/33",
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
    let cases: [(&[&str], &str); 27] = [
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
    ];
    let mut commands = cases
        .iter()
        .map(|(options, problem)| ([&launch[..], options].concat(), *problem))
        .collect::<Vec<_>>();
    let stage = ["stage", "store", "--distro", "erofs:image"];
    let stage_cases: [(&[&str], &str); 8] = [
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
            vec!["stop", "store", "--network", "network.json"],
            "unexpected option \"--network\" of stop",
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
