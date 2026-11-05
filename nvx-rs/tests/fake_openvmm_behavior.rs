//! Runs the `nvx-fake-openvmm` test double directly, as `nvx-rs run` runs OpenVMM on the
//! terminal, and checks the behaviors of OpenVMM that it reproduces without running a VM.
#![cfg(all(
    feature = "host",
    feature = "testing",
    any(target_os = "linux", windows)
))]

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use nvx::Hypervisor;
use serde_json::{Value, json};

/// Runs the fake in `directory` as `nvx-rs run` runs OpenVMM on the terminal: with two
/// processors and the backend of this host, and then `arguments`.
fn run_microvm(directory: &Path, arguments: &[&str]) -> std::process::Output {
    let hypervisor = Hypervisor::platform_default().expect("tests run on Linux or Windows");
    let mut command = Command::new(env!("CARGO_BIN_EXE_nvx-fake-openvmm"));
    command
        .args([
            "--single-process",
            "--machine",
            "microvm",
            "--processors",
            "2",
        ])
        .args(["--hypervisor", hypervisor.as_str()])
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.spawn().unwrap().wait_with_output().unwrap()
}

fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data).into_owned()
}

#[test]
fn runs_boot_or_restore_a_microvm_on_the_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = |name: &str| root.join(name).to_str().unwrap().to_owned();
    fs::write(root.join("vmlinux"), b"kernel").unwrap();
    fs::write(root.join("initramfs.cpio.gz"), b"initrd").unwrap();
    fs::create_dir(root.join("reports")).unwrap();
    fs::create_dir(root.join("snapshot")).unwrap();
    let (kernel, initrd) = (path("vmlinux"), path("initramfs.cpio.gz"));
    let boot = [
        "--memory",
        "128M",
        "--kernel",
        &kernel,
        "--initrd",
        &initrd,
        "--virtio-console",
        "console",
    ];

    // A cold boot prints that it booted, writes its outcome report, records its command line next
    // to the kernel, and exits with 0.
    let report = root.join("reports").join("boot.json");
    let report_path = report.to_str().unwrap().to_owned();
    let mut arguments = boot.to_vec();
    arguments.extend(["--cpu-profile", "host", "--microvm-report", &report_path]);
    let output = run_microvm(root, &arguments);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "nvx-fake-openvmm: booted the microVM\n"
    );
    assert_eq!(text(&output.stderr), "");
    let document: Value = serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(
        document["outcome"],
        json!({"operation": "run", "category": "success", "status_code": 0})
    );
    assert_eq!(document["network_policy"]["status"], "not-requested");
    let recorded: Vec<String> =
        serde_json::from_str(&fs::read_to_string(root.join("fake-openvmm-reports.json")).unwrap())
            .unwrap();
    assert_eq!(recorded[recorded.len() - 1], report_path);

    // The kernel command line sets the status, also of a restore, which prints that it restored
    // the snapshot and records its command line next to it.
    let mut arguments = boot.to_vec();
    arguments.extend(["--cmdline", "quiet fake_exit_status=5"]);
    let output = run_microvm(root, &arguments);
    assert_eq!(output.status.code(), Some(5), "{}", text(&output.stderr));
    let snapshot = path("snapshot");
    let restore = [
        "--restore-snapshot",
        &snapshot,
        "--restore-processors",
        "1",
        "--cmdline",
        "fake_exit_status=6",
    ];
    fs::remove_file(root.join("fake-openvmm-.json")).unwrap();
    let output = run_microvm(root, &restore);
    assert_eq!(output.status.code(), Some(6), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "nvx-fake-openvmm: restored the microVM\n"
    );
    let recorded: Vec<String> =
        serde_json::from_str(&fs::read_to_string(root.join("fake-openvmm-.json")).unwrap())
            .unwrap();
    assert_eq!(recorded[recorded.len() - restore.len()..], restore);

    // A restore without its snapshot fails as OpenVMM fails it, and OpenVMM refuses a command
    // line that it does not take with status 2.
    let missing = path("missing");
    let output = run_microvm(root, &["--restore-snapshot", &missing]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        format!("nvx-fake-openvmm: cannot restore {missing}: no such snapshot directory\n")
    );
    let output = run_microvm(root, &["--restore-snapshot", &snapshot, "--memory", "128M"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        text(&output.stderr),
        "nvx-fake-openvmm: a restore takes no --memory\n"
    );

    // As OpenVMM does, a restore takes the network profile and policy of a snapshot that has a NIC
    // without `--net`, whose addressing the snapshot saved, and refuses `--net`.
    let networked = [
        "--restore-snapshot",
        &snapshot,
        "--network-profile",
        "portable",
        "--network-egress",
        "deny",
    ];
    let output = run_microvm(root, &networked);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "nvx-fake-openvmm: restored the microVM\n"
    );
    let output = run_microvm(root, &[&networked[..], &["--net", "10.0.0.2/24"]].concat());
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        text(&output.stderr),
        "nvx-fake-openvmm: restore takes microVM network addressing from saved state\n"
    );

    // Only a restore that replaces the policy of a NIC template may publish the guest's
    // listeners.
    let ingress = ["--network-ingress", "allow"];
    let output = run_microvm(root, &[&networked[..], &ingress].concat());
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        text(&output.stderr),
        "nvx-fake-openvmm: microVM snapshots do not support --network-ingress allow, whose \
         published guest listeners are live host attachments, except at a restore with \
         --restore-network-policy\n"
    );
    let output = run_microvm(
        root,
        &[&networked[..], &ingress, &["--restore-network-policy"]].concat(),
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "nvx-fake-openvmm: restored the microVM\n"
    );
}

#[test]
fn runs_find_absolute_policy_paths_in_relative_share_roots() {
    // nvx-rs run passes the host path of each share as given, which may be relative to the
    // working directory, and with several shares, absolute policy paths, which OpenVMM
    // attributes to the share whose root contains them once it has resolved both.
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join("vmlinux"), b"kernel").unwrap();
    fs::write(root.join("initramfs.cpio.gz"), b"initrd").unwrap();
    for path in ["share/hidden/shown", "share/writable", "other", "outside"] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    let absolute = |relative: &str| root.join(relative).to_str().unwrap().to_owned();
    let (kernel, initrd) = (absolute("vmlinux"), absolute("initramfs.cpio.gz"));
    let boot = [
        "--memory",
        "128M",
        "--kernel",
        &kernel,
        "--initrd",
        &initrd,
        "--virtio-console",
        "console",
    ];
    // The aggregate's children that the guest binds at /data and /other, named by their index
    // and the first 32 hexadecimal digits of the SHA-256 digest of their target.
    let share = "0-bd47413b5c03dfc6e4b5b8143fe205ea,share,rw";
    let other = "1-bf2faee2596026620b5cbc997c2d7d45,other,ro";
    let aggregate = [
        "--mount-aggregate",
        "/run/nvx/shares",
        "--mount-child",
        share,
        "--mount-child",
        other,
    ];
    let (hidden, shown, writable) = (
        absolute("share/hidden"),
        absolute("share/hidden/shown"),
        absolute("share/writable"),
    );
    let policy = [
        "--mount-deny",
        &hidden,
        "--mount-allow",
        &shown,
        "--mount-write",
        &writable,
    ];
    let mut arguments = boot.to_vec();
    arguments.extend(aggregate);
    arguments.extend(policy);
    let output = run_microvm(root, &arguments);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "nvx-fake-openvmm: booted the microVM\n"
    );

    // One share with a relative root takes absolute policy paths too, beside the relative ones
    // that OpenVMM resolves in it.
    let mut arguments = boot.to_vec();
    arguments.extend([
        "--mount",
        "/data,share,rw",
        "--mount-deny",
        &hidden,
        "--mount-allow",
        "hidden/shown",
    ]);
    let output = run_microvm(root, &arguments);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));

    // A path outside every share stays refused, as one that does not exist does.
    for path in [absolute("outside"), absolute("share/missing")] {
        let mut arguments = boot.to_vec();
        arguments.extend(aggregate);
        arguments.extend(["--mount-deny", &path]);
        let output = run_microvm(root, &arguments);
        assert_eq!(output.status.code(), Some(2), "{path}");
        assert_eq!(
            text(&output.stderr),
            format!("nvx-fake-openvmm: invalid --mount-deny {path}\n")
        );
    }
}
