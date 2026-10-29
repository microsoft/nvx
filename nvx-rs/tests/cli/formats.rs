//! The configurations of every format that a build of `nvx-rs` or `nvx.py` wrote, from the
//! hand-written `config.json` files of `tests/fixtures/state/`: `start` reads format 7, which
//! earlier builds wrote, with shares or without, and format 1, which they wrote for a sandbox
//! without shares, and refuses the others, and the configurations that `nvx.py` wrote, without
//! bindings, before it launches OpenVMM. The lifecycle tests cover format 8, which this build
//! writes.
//!
//! A fixture names the paths of a root as `{root/RELATIVE}`, the hypervisor that a provision
//! records on this host as `{hypervisor}`, and the bindings of the root's distro layer and scratch
//! image, which hold the identities of the files of each root, as `{distro_binding}` and
//! `{scratch_binding}`.

use nvx::host::sandbox::distro::DistroBinding;
use nvx::host::sandbox::scratch::ScratchBinding;
use nvx::host::sandbox::{LayerRole, SandboxLayer};
use serde_json::{Value, json};

use crate::harness::{HYPERVISOR, Root, UUID, entries, in_parallel, json};
use crate::lifecycle::{PROVISIONED, provision, sandbox};
use crate::openvmm::{Options, WORKLOAD, assert_checked_blocks, assert_opened_images, managed};

/// What a start does with a configuration of a format.
enum Start {
    /// It starts the sandbox, whose OpenVMM options differ as the function says from those of the
    /// minimal sandbox, and whose guest maps the table, if any. A workload of the options runs
    /// and writes the output.
    Runs {
        /// How the options of OpenVMM differ from those of the minimal sandbox.
        openvmm: fn(Options) -> Options,
        /// The mapping table of the shares that the guest receives, if any.
        maps: Option<Value>,
        /// The options of a workload that `exec` runs, and its output.
        exec: (&'static [&'static str], &'static str),
    },
    /// It refuses the configuration as one of a format that this build does not read, with an
    /// error with the code `format-unsupported` that names the fragments.
    Refused(&'static [&'static str]),
}

/// Returns the options with which a start launches OpenVMM for the sandbox of `format-1.json` and
/// `format-7-without-shares.json`, a reader without shares, from those of the minimal sandbox.
fn reader(options: Options) -> Options {
    options
        .with("--microvm-workload-identity", ["2000:2000"])
        .with("--memory", ["320M"])
        .sandbox_kernel(
            "quiet",
            &[
                WORKLOAD[0],
                "nvx_hostname=reader",
                "nvx_memory_max=134217728",
                "nvx_pids_max=33",
            ],
        )
}

/// The configurations of every format, as fixtures, and what a start does with each.
fn formats() -> [(&'static str, &'static str, Start); 10] {
    let unsupported = &[
        "sandbox configuration has an unsupported format",
        "{root/state/config.json}",
    ][..];
    [
        (
            "format 1",
            include_str!("../fixtures/state/format-1.json"),
            Start::Runs {
                openvmm: reader,
                maps: None,
                exec: (&["--entrypoint", "/bin/echo", "--arg", "one"], "one\n"),
            },
        ),
        (
            "format 1 with a network",
            include_str!("../fixtures/state/format-1-network.json"),
            Start::Runs {
                openvmm: |options| {
                    options
                        .with("--net", ["10.0.0.2/24"])
                        .with("--network-profile", ["portable"])
                        .with("--network-egress", ["deny"])
                        .with("--network-ingress", ["allow"])
                        .with("--host-loopback", ["allow"])
                        .with("--network-egress-allow", ["192.0.2.0/24:tcp:8080-8443"])
                        .with("--network-egress-deny", ["198.51.100.0/24"])
                        .with("--host-loopback-forward", ["tcp:18080:8080"])
                },
                maps: None,
                exec: (
                    &["--entrypoint", "/bin/echo", "--arg", "network"],
                    "network\n",
                ),
            },
        ),
        (
            "format 7",
            include_str!("../fixtures/state/format-7.json"),
            Start::Runs {
                openvmm: |options| {
                    options
                        .with("--mount-aggregate", ["/run/nvx/hostfs/root"])
                        .with(
                            "--mount-child",
                            ["0,{root/share},rw", "1,{root/single.txt},ro,file"],
                        )
                        .with("--mount-deny", ["{root/share/hidden}"])
                        .with("--mount-allow", ["{root/share/hidden/shown}"])
                        .with("--mount-write", ["{root/share/writable}"])
                        .sandbox_kernel("", &[WORKLOAD[0], WORKLOAD[1], "nvx_maps=2"])
                },
                maps: Some(json!([
                    {"source": "0", "target": "/data", "read_only": false, "directory": true},
                    {
                        "source": "1",
                        "target": "/etc/single.txt",
                        "read_only": true,
                        "directory": false,
                    },
                ])),
                exec: (&["--entrypoint", "/bin/pwd", "--cwd", "/data"], "/data\n"),
            },
        ),
        (
            "format 7 without shares",
            include_str!("../fixtures/state/format-7-without-shares.json"),
            Start::Runs {
                openvmm: reader,
                maps: None,
                exec: (&["--entrypoint", "/bin/echo", "--arg", "seven"], "seven\n"),
            },
        ),
        // Formats 2 to 6 recorded shares as they were requested, which no start reads any more:
        // such a sandbox is provisioned again.
        (
            "format 2",
            include_str!("../fixtures/state/format-2.json"),
            Start::Refused(unsupported),
        ),
        (
            "format 3",
            include_str!("../fixtures/state/format-3.json"),
            Start::Refused(unsupported),
        ),
        (
            "format 4",
            include_str!("../fixtures/state/format-4.json"),
            Start::Refused(unsupported),
        ),
        (
            "format 5",
            include_str!("../fixtures/state/format-5.json"),
            Start::Refused(unsupported),
        ),
        (
            "format 6",
            include_str!("../fixtures/state/format-6.json"),
            Start::Refused(unsupported),
        ),
        // `nvx.py` recorded no binding of the images, and its network options rather than the
        // typed network.
        (
            "format 1 of nvx.py",
            include_str!("../fixtures/state/format-1-nvx-py.json"),
            Start::Refused(&[
                "sandbox configuration has no distro binding",
                "provision it again",
            ]),
        ),
    ]
}

/// Returns the configuration of the fixture `text` for `root`, with its placeholders replaced.
fn render(root: &Root, text: &str) -> Value {
    let layer = SandboxLayer::new(
        LayerRole::Distro,
        root.below("distro.erofs").to_str().unwrap(),
        UUID,
    );
    let distro = serde_json::to_value(DistroBinding::verify(&layer).unwrap()).unwrap();
    let scratch =
        serde_json::to_value(ScratchBinding::bind(&root.below("scratch.ext4")).unwrap()).unwrap();
    let mut config = root.expand_json(&serde_json::from_str(text).unwrap());
    for member in config.as_object_mut().unwrap().values_mut() {
        match member.as_str() {
            Some("{distro_binding}") => *member = distro.clone(),
            Some("{scratch_binding}") => *member = scratch.clone(),
            Some("{hypervisor}") => *member = json!(HYPERVISOR),
            _ => {}
        }
    }
    config
}

#[test]
fn configurations_of_every_format_start_and_exec() {
    in_parallel(&formats(), |(name, text, start)| {
        eprintln!("case: {name}");
        let root = Root::new();
        let state = root.below("state");
        // The state directory is the one that a provision makes, private to its owner.
        root.nvx_rs(&provision("{root/state}", &[]))
            .assert_silent_success();
        let config = serde_json::to_string_pretty(&render(&root, text)).unwrap();
        root.write("state/config.json", format!("{config}\n"));
        match start {
            Start::Runs {
                openvmm,
                maps,
                exec,
            } => {
                root.nvx_rs(&sandbox::<&str>("start", "{root/state}", &[]))
                    .assert_silent_success();
                let endpoint = json(&state.join("runtime.json"))["endpoint"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let mut options =
                    Options::parse(&root.openvmm_arguments("build/fake-openvmm-state.json"));
                assert_checked_blocks(&root, &options.take("--microvm-sandbox-block"));
                assert_opened_images(&root, "state");
                let mut expected = openvmm(managed("state", &endpoint));
                expected.take("--microvm-sandbox-block");
                expected.assert_matches(&root, &options, name);
                let mapped = root.below("build/fake-openvmm-state-maps.json");
                match maps {
                    Some(table) => assert_eq!(json(&mapped), *table, "{name}"),
                    None => assert!(!mapped.exists(), "{name}"),
                }
                let outcome = root.nvx_rs(&sandbox("exec", "{root/state}", exec.0));
                outcome.assert_success();
                assert_eq!(outcome.stdout(), exec.1, "{name}");
                root.nvx_rs(&sandbox::<&str>("stop", "{root/state}", &[]))
                    .assert_silent_success();
            }
            Start::Refused(fragments) => {
                let before = root.snapshot();
                let outcome = root.nvx_rs(&sandbox::<&str>("start", "{root/state}", &[]));
                let fragments: Vec<String> = fragments
                    .iter()
                    .map(|fragment| root.expand(fragment))
                    .collect();
                let fragments: Vec<&str> = fragments.iter().map(String::as_str).collect();
                outcome.assert_error("format-unsupported", &fragments);
                assert_eq!(root.snapshot(), before, "{name}");
                assert_eq!(entries(&state), PROVISIONED, "{name}");
            }
        }
        root.nvx_rs(&sandbox::<&str>("deprovision", "{root/state}", &[]))
            .assert_silent_success();
        assert!(!state.exists(), "{name}");
    });
}
