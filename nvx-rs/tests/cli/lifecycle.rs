//! The lifecycles of managed sandboxes: what `provision`, `start`, `exec`, `stop`, and
//! `deprovision` leave in the state directory of a sandbox of every configuration, and what
//! OpenVMM and its guest receive; the workloads that `exec` runs; and the refusals of the
//! lifecycle operations, which leave what they refuse as it was.

use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use nvx::contract::{
    BOOT_CONTRACT_VERSION, CONTROL_CONTRACT_REVISION, CONTROL_SESSION_PROTOCOL_VERSION,
};
use nvx::host::process::{Process, ProcessRef};
use nvx::host::sandbox::distro::DistroBinding;
use nvx::host::sandbox::scratch::ScratchBinding;
use nvx::host::sandbox::{LayerRole, SandboxLayer};
use serde_json::{Value, json};

#[cfg(unix)]
use crate::harness::mode;
use crate::harness::{
    Content, HYPERVISOR, Hosts, LAYER, OPENVMM, Outcome, PROVENANCE, Root, SCRATCH, UUID, entries,
    in_parallel, is_hex, json,
};
use crate::openvmm::{
    NO_NETWORK, NetworkPolicy, Options, WORKLOAD, assert_checked_blocks, assert_checked_distro,
    assert_endpoint, assert_opened_distro, assert_opened_images, assert_outcome_report, managed,
};

/// The entries of the state directory of a sandbox that never started: its configuration and
/// the store's lock.
pub(crate) const PROVISIONED: [&str; 2] = ["config.json", "sandbox.lock"];
/// The entries of the state directory of a sandbox that stopped: OpenVMM's log and outcome report
/// stay.
const STOPPED: [&str; 4] = ["config.json", "openvmm.log", "outcome.json", "sandbox.lock"];

/// Returns the entries of the state directory of a running sandbox: its configuration, the store's
/// runtime record, lock, and control capability, the control endpoint on POSIX hosts, and
/// OpenVMM's log.
fn running() -> Vec<&'static str> {
    let mut names = vec!["config.json", "control.capability"];
    if cfg!(unix) {
        names.push("control.sock");
    }
    names.extend(["openvmm.log", "runtime.json", "sandbox.lock"]);
    names
}

/// Returns the arguments of the lifecycle operation `operation` of the sandbox whose state
/// directory is `state`, with `options`.
pub(crate) fn sandbox<S: AsRef<str>>(operation: &str, state: &str, options: &[S]) -> Vec<String> {
    ["sandbox", operation, "--state-dir", state]
        .iter()
        .map(|argument| (*argument).to_owned())
        .chain(options.iter().map(|option| option.as_ref().to_owned()))
        .collect()
}

/// Returns the arguments that provision the sandbox of the distro layer and scratch image of the
/// root in the state directory `state`, with `options`.
pub(crate) fn provision(state: &str, options: &[&str]) -> Vec<String> {
    let mut all = vec!["--layer", LAYER, "--scratch", SCRATCH];
    all.extend(options);
    sandbox("provision", state, &all)
}

/// Returns the configuration that provision records for the minimal sandbox, in format 8, with
/// placeholders, and without the bindings of its images, which [`assert_bindings`] checks.
pub(crate) fn minimal_config() -> Value {
    json!({
        "cmdline": "",
        "export": null,
        "format": 8,
        "hostname": "nvx-sandbox",
        "hypervisor": HYPERVISOR,
        "layers": [{"path": "{root/distro.erofs}", "role": "distro", "uuid": UUID}],
        "memory_max": null,
        "memory_mib": 256,
        "network": null,
        "pids_max": null,
        "scratch": "{root/scratch.ext4}",
        "upper": "scratch",
        "workload_gid": 65534,
        "workload_uid": 65534,
    })
}

/// Returns the configuration of a sandbox with shares, whose export plan is `export`.
pub(crate) fn shared_config(export: Value) -> Value {
    let mut config = minimal_config();
    config["export"] = export;
    config
}

/// Returns the bind of an export plan that binds its child `child` at `target`.
pub(crate) fn bind(child: usize, target: &str, read_only: bool) -> Value {
    json!({"child": child, "readOnly": read_only, "source": "", "target": target})
}

/// Returns the child of an export plan that exports the host directory `root`, read-only unless
/// `writable`, with no policy paths.
pub(crate) fn child(root: &str, writable: bool) -> Value {
    json!({
        "allowed": [],
        "denied": [],
        "deniedIdentities": [],
        "root": root,
        "writable": writable,
        "write": [],
    })
}

/// Asserts that `config`, a configuration that provision recorded, holds the bindings of the
/// distro layer and the scratch image of the root as they are, as explicit verification of the
/// layer and the binding of the image make them, and returns it without them.
pub(crate) fn assert_bindings(root: &Root, mut config: Value) -> Value {
    let members = config.as_object_mut().unwrap();
    let distro = members.remove("distro_binding").expect("a distro binding");
    let scratch = members
        .remove("scratch_binding")
        .expect("a scratch binding");
    let layer = SandboxLayer::new(
        LayerRole::Distro,
        root.below("distro.erofs").to_str().unwrap(),
        UUID,
    );
    assert_eq!(
        distro,
        serde_json::to_value(DistroBinding::verify(&layer).unwrap()).unwrap()
    );
    assert_eq!(
        scratch,
        serde_json::to_value(ScratchBinding::bind(&root.below("scratch.ext4")).unwrap()).unwrap()
    );
    config
}

/// Asserts that `record`, the runtime record of the sandbox in the state directory `state`, below
/// the root, is the store's format 4 for an OpenVMM that runs: with a launch ID, the OpenVMM
/// process by its ID, start time, and the host's boot, the control endpoint, and the runtime
/// identity that the build provenance of the root names, with this build's contracts. Returns the
/// endpoint and the process.
fn assert_runtime_record(root: &Root, state: &str, record: &Value) -> (String, Process) {
    let mut names: Vec<&String> = record.as_object().unwrap().keys().collect();
    names.sort();
    assert_eq!(
        names,
        ["endpoint", "format", "launch", "process", "runtime"],
        "{record}"
    );
    assert_eq!(record["format"], 4, "{record}");
    assert!(
        record["launch"].as_str().is_some_and(|id| is_hex(id, 32)),
        "{record}"
    );
    let reference: ProcessRef = serde_json::from_value(record["process"].clone()).unwrap();
    let process = Process::open(&reference)
        .unwrap()
        .unwrap_or_else(|| panic!("the OpenVMM of the record does not run: {record}"));
    let endpoint = record["endpoint"].as_str().unwrap().to_owned();
    assert_endpoint(root, state, &endpoint);
    let digest = |index: usize| format!("sha256:{}", PROVENANCE[index].2);
    assert_eq!(
        record["runtime"],
        json!({
            "openvmm": digest(0),
            "kernel": digest(1),
            "initramfs": digest(2),
            "bootContract": BOOT_CONTRACT_VERSION,
            "controlContract": CONTROL_CONTRACT_REVISION,
            "controlSession": CONTROL_SESSION_PROTOCOL_VERSION,
        }),
        "{record}"
    );
    (endpoint, process)
}

/// Asserts that the state directory at `state` holds the 32-byte control capability of a running
/// OpenVMM, which, like the runtime record, only its owner may read on POSIX hosts.
fn assert_capability(state: &Path) {
    let capability = fs::read(state.join("control.capability")).unwrap();
    assert_eq!(capability.len(), 32);
    assert_ne!(capability, [0; 32]);
    #[cfg(unix)]
    for name in ["control.capability", "runtime.json"] {
        assert_eq!(mode(&state.join(name)), 0o600, "{name}");
    }
}

/// A configuration of a managed sandbox, and what its lifecycle shows of it.
struct Configuration {
    /// The name of the case.
    name: &'static str,
    /// The hosts on which it runs.
    hosts: Hosts,
    /// The `--state-dir` of the sandbox, with placeholders.
    argument: &'static str,
    /// Its state directory below the root.
    state: &'static str,
    /// The options of its provision besides `--state-dir`, `--layer`, and `--scratch`.
    options: Vec<&'static str>,
    /// The files that the case writes below the root first, with their contents.
    files: Vec<(&'static str, Content)>,
    /// The configuration that provision records, with placeholders, without its bindings.
    config: Value,
    /// How the options of the start's OpenVMM differ from those of the minimal sandbox.
    openvmm: fn(Options) -> Options,
    /// The mapping table of its shares that the guest receives, if it has shares.
    maps: Option<Value>,
    /// The options of a workload that `exec` runs while it runs, and the workload's output.
    exec: (Vec<&'static str>, &'static str),
    /// The network policy of the outcome report of its OpenVMM.
    network: NetworkPolicy,
}

/// Returns a configuration of a sandbox in the state directory `state` below the root, which
/// provision records with `options` as `config`.
fn configuration(
    name: &'static str,
    options: Vec<&'static str>,
    config: Value,
    openvmm: fn(Options) -> Options,
) -> Configuration {
    Configuration {
        name,
        hosts: Hosts::Every,
        argument: "{root/state}",
        state: "state",
        options,
        files: Vec::new(),
        config,
        openvmm,
        maps: None,
        exec: (
            vec!["--entrypoint", "/bin/echo", "--arg", "hello"],
            "hello\n",
        ),
        network: NO_NETWORK,
    }
}

/// Returns the mapping table entry that maps the export's child `source` at `target`.
fn mapped(source: &str, target: &str, read_only: bool, directory: bool) -> Value {
    json!({"source": source, "target": target, "read_only": read_only, "directory": directory})
}

/// Returns the options of a start of a sandbox whose aggregate exports `children`, as
/// `--mount-child` values, which the guest maps from `/run/nvx/hostfs/root`.
fn exported(options: Options, children: &[&str]) -> Options {
    let mut tokens: Vec<String> = WORKLOAD.iter().map(|token| (*token).to_owned()).collect();
    tokens.push(format!("nvx_maps={}", children.len()));
    options
        .with("--mount-aggregate", ["/run/nvx/hostfs/root"])
        .with("--mount-child", children.iter().copied())
        .sandbox_kernel("", &tokens)
}

/// Returns the configurations of every kind of sandbox.
fn configurations() -> Vec<Configuration> {
    let workdir = |directory: &'static str| -> (Vec<&'static str>, &'static str) {
        (
            vec!["--entrypoint", "/bin/pwd", "--cwd", directory],
            match directory {
                "/data" => "/data\n",
                _ => "/other\n",
            },
        )
    };
    let mut one_share = configuration(
        "one share",
        vec!["--mount", "/data,{root/share}"],
        shared_config(json!({
            "binds": [bind(0, "/data", true)],
            "children": [child("{root/share}", false)],
        })),
        |options| exported(options, &["0,{root/share},ro"]),
    );
    one_share.maps = Some(json!([mapped("0", "/data", true, true)]));
    one_share.exec = workdir("/data");

    let mut caller = configuration(
        "caller-owned share",
        vec![
            "--mount",
            "/data,{root/share},rw",
            "--mount-owner",
            "caller",
        ],
        shared_config(json!({
            "binds": [bind(0, "/data", false)],
            "children": [child("{root/share}", true)],
            "owner": "caller",
        })),
        |options| exported(options, &["0,{root/share},rw"]).with("--mount-owner", ["caller"]),
    );
    caller.hosts = Hosts::Posix;
    caller.maps = Some(json!([mapped("0", "/data", false, true)]));
    caller.exec = workdir("/data");

    let mut several = configuration(
        "several shares",
        vec![
            "--mount",
            "/data,{root/share}",
            "--mount",
            "/other,{root/share2},rw",
        ],
        shared_config(json!({
            "binds": [bind(0, "/data", true), bind(1, "/other", false)],
            "children": [child("{root/share}", false), child("{root/share2}", true)],
        })),
        |options| exported(options, &["0,{root/share},ro", "1,{root/share2},rw"]),
    );
    several.maps = Some(json!([
        mapped("0", "/data", true, true),
        mapped("1", "/other", false, true),
    ]));
    several.exec = workdir("/other");

    let mut policies = configuration(
        "share policies",
        vec![
            "--mount",
            "/data,{root/share},rw",
            "--mount-deny",
            "{root/share/hidden}",
            "--mount-allow",
            "{root/share/hidden/shown}",
            "--mount-write",
            "{root/share/writable}",
        ],
        shared_config(json!({
            "binds": [bind(0, "/data", false)],
            "children": [{
                "allowed": ["{root/share/hidden/shown}"],
                "denied": ["{root/share/hidden}"],
                "deniedIdentities": [null],
                "root": "{root/share}",
                "writable": true,
                "write": ["{root/share/writable}"],
            }],
        })),
        |options| {
            exported(options, &["0,{root/share},rw"])
                .with("--mount-deny", ["{root/share/hidden}"])
                .with("--mount-allow", ["{root/share/hidden/shown}"])
                .with("--mount-write", ["{root/share/writable}"])
        },
    );
    policies.maps = Some(json!([mapped("0", "/data", false, true)]));
    policies.exec = workdir("/data");

    let mut file = configuration(
        "file share",
        vec!["--mount", "/etc/single.txt,{root/single.txt},ro"],
        shared_config(json!({
            "binds": [bind(0, "/etc/single.txt", true)],
            "children": [{
                "allowed": [],
                "denied": [],
                "deniedIdentities": [],
                "file": true,
                "root": "{root/single.txt}",
                "writable": false,
                "write": [],
            }],
        })),
        |options| exported(options, &["0,{root/single.txt},ro,file"]),
    );
    file.maps = Some(json!([mapped("0", "/etc/single.txt", true, false)]));

    // A managed sandbox runs the guest agent, whatever entrypoint and arguments provision gets,
    // and records the policy file's rules in its typed network.
    let mut network = configuration(
        "network and launch options",
        vec![
            "--entrypoint",
            "/bin/server",
            "--arg=--serve",
            "--hostname",
            "worker",
            "--workload-user",
            "1000:1000",
            "--memory-max",
            "268435456",
            "--pids-max",
            "64",
            "--memory-mib",
            "384",
            "--hypervisor",
            "auto",
            "--cmdline",
            "quiet",
            "--net",
            "10.0.0.2/24",
            "--network-profile",
            "portable",
            "--network-egress",
            "allow",
            "--network-ingress",
            "deny",
            "--network-egress-policy-file",
            "{root/policy.json}",
            "--host-loopback",
            "allow",
            "--host-loopback-forward",
            "tcp:18080:8080",
            "--host-loopback-forward",
            "udp:19090:9090",
        ],
        {
            let mut config = minimal_config();
            for (name, value) in [
                ("cmdline", json!("quiet")),
                ("hostname", json!("worker")),
                ("memory_max", json!(268_435_456)),
                ("memory_mib", json!(384)),
                ("pids_max", json!(64)),
                ("workload_gid", json!(1000)),
                ("workload_uid", json!(1000)),
                (
                    "network",
                    json!({
                        "allow": {
                            "name": "--network-egress-allow",
                            "rules": [{
                                "ports": [{"endPort": null, "port": 443, "protocol": "tcp"}],
                                "to": [{"cidr": "192.0.2.0/24", "except": []}],
                            }],
                        },
                        "deny": {
                            "name": "--network-egress-deny",
                            "rules": [{
                                "ports": [],
                                "to": [{"cidr": "192.0.2.128/25", "except": []}],
                            }],
                        },
                        "egress": "allow",
                        "guestNetwork": "10.0.0.2/24",
                        "hostLoopback": "allow",
                        "hostLoopbackForwards": [
                            {"guestPort": 8080, "hostPort": 18080, "protocol": "tcp"},
                            {"guestPort": 9090, "hostPort": 19090, "protocol": "udp"},
                        ],
                        "ingress": "deny",
                        "proxy": null,
                    }),
                ),
            ] {
                config[name] = value;
            }
            config
        },
        |options| {
            options
                .with("--microvm-workload-identity", ["1000:1000"])
                .with("--net", ["10.0.0.2/24"])
                .with("--network-profile", ["portable"])
                .with("--network-egress", ["allow"])
                .with("--network-ingress", ["deny"])
                .with("--host-loopback", ["allow"])
                .with("--network-egress-allow", ["192.0.2.0/24:tcp:443"])
                .with("--network-egress-deny", ["192.0.2.128/25"])
                .with(
                    "--host-loopback-forward",
                    ["tcp:18080:8080", "udp:19090:9090"],
                )
                .with("--memory", ["384M"])
                .sandbox_kernel(
                    "quiet",
                    &[
                        WORKLOAD[0],
                        "nvx_hostname=worker",
                        "nvx_memory_max=268435456",
                        "nvx_pids_max=65",
                    ],
                )
        },
    );
    network.files = vec![(
        "policy.json",
        Content::Text(
            r#"{"allow": [{"cidr": "192.0.2.0/24", "protocol": "tcp", "port": 443}], "deny": [{"cidr": "192.0.2.128/25"}]}"#,
        ),
    )];
    network.network = ("applied", "rules", 1, 1, "allow");

    let mut proxy = configuration(
        "proxy-only network",
        vec![
            "--net",
            "10.0.0.2/24",
            "--network-profile",
            "portable",
            "--network-egress",
            "deny",
            "--host-loopback",
            "deny",
            "--network-proxy",
            "10.0.0.1:3128",
        ],
        {
            let mut config = minimal_config();
            config["network"] = json!({
                "allow": {"name": "--network-egress-allow", "rules": []},
                "deny": {"name": "--network-egress-deny", "rules": []},
                "egress": "deny",
                "guestNetwork": "10.0.0.2/24",
                "hostLoopback": "deny",
                "hostLoopbackForwards": [],
                "ingress": "deny",
                "proxy": {"port": 3128, "scheme": null},
            });
            config
        },
        |options| {
            options
                .with("--net", ["10.0.0.2/24"])
                .with("--network-profile", ["portable"])
                .with("--network-egress", ["deny"])
                .with("--network-ingress", ["deny"])
                .with("--host-loopback", ["deny"])
                .with("--network-proxy", ["10.0.0.1:3128"])
        },
    );
    proxy.network = ("applied", "deny-all", 0, 0, "deny");

    // A relative state directory is relative to the working directory, and its missing parents
    // are created.
    let mut nested = configuration(
        "relative and nested state directory",
        vec![],
        minimal_config(),
        |options| options,
    );
    nested.argument = "nested/state";
    nested.state = "nested/state";

    vec![
        configuration("minimal", vec![], minimal_config(), |options| options),
        // The most MiB whose count of bytes fits in 64 bits, as OpenVMM reads a size of RAM.
        configuration(
            "most memory",
            vec!["--memory-mib", "17592186044415"],
            {
                let mut config = minimal_config();
                config["memory_mib"] = json!(17_592_186_044_415_u64);
                config
            },
            |options| options.with("--memory", ["17592186044415M"]),
        ),
        one_share,
        caller,
        several,
        policies,
        file,
        network,
        proxy,
        nested,
    ]
}

/// Provisions, starts, runs a workload in, stops, starts again, and deprovisions the sandbox of
/// `case`, and checks the state that each transition leaves.
fn check_lifecycle(case: &Configuration) {
    let root = Root::new();
    for (relative, content) in &case.files {
        root.write_content(relative, *content);
    }
    let state = root.below(case.state);
    let token = case.state.rsplit('/').next().unwrap();
    root.nvx_rs(&provision(case.argument, &case.options))
        .assert_silent_success();
    assert_eq!(entries(&state), PROVISIONED, "{}", case.name);
    #[cfg(unix)]
    {
        assert_eq!(mode(&state), 0o700, "{}", case.name);
        assert_eq!(mode(&state.join("config.json")), 0o600, "{}", case.name);
    }
    let config = assert_bindings(&root, json(&state.join("config.json")));
    assert_eq!(config, root.expand_json(&case.config), "{}", case.name);
    assert!(
        root.openvmm_records().is_empty(),
        "{}: provision launched OpenVMM",
        case.name
    );
    // A sandbox that stopped starts again.
    for round in 1..=2 {
        let context = format!("{}, start {round}", case.name);
        root.nvx_rs(&sandbox::<&str>("start", case.argument, &[]))
            .assert_silent_success();
        assert_eq!(entries(&state), running(), "{context}");
        let (endpoint, process) =
            assert_runtime_record(&root, case.state, &json(&state.join("runtime.json")));
        assert_capability(&state);
        let mut options =
            Options::parse(&root.openvmm_arguments(&format!("build/fake-openvmm-{token}.json")));
        assert_checked_blocks(&root, &options.take("--microvm-sandbox-block"));
        assert_opened_images(&root, token);
        let mut expected = (case.openvmm)(managed(case.state, &endpoint));
        expected.take("--microvm-sandbox-block");
        expected.assert_matches(&root, &options, &context);
        let maps = root.below(&format!("build/fake-openvmm-{token}-maps.json"));
        match &case.maps {
            Some(table) => assert_eq!(json(&maps), *table, "{context}"),
            None => assert!(!maps.exists(), "{context}"),
        }
        let (options, stdout) = &case.exec;
        let outcome = root.nvx_rs(&sandbox("exec", case.argument, options));
        outcome.assert_success();
        assert_eq!(outcome.stdout(), *stdout, "{context}");
        assert_eq!(entries(&state), running(), "{context}");
        root.nvx_rs(&sandbox::<&str>("stop", case.argument, &[]))
            .assert_silent_success();
        assert!(
            process
                .wait(Instant::now() + Duration::from_secs(10))
                .unwrap(),
            "{context}: OpenVMM runs on"
        );
        assert_eq!(entries(&state), STOPPED, "{context}");
        assert_outcome_report(
            &json(&state.join("outcome.json")),
            HYPERVISOR,
            ("managed", "success", 0),
            case.network,
        );
    }
    root.nvx_rs(&sandbox::<&str>("deprovision", case.argument, &[]))
        .assert_silent_success();
    assert!(!state.exists(), "{}", case.name);
}

#[test]
fn lifecycles_of_every_configuration() {
    let cases: Vec<Configuration> = configurations()
        .into_iter()
        .filter(|case| case.hosts.include_this_host())
        .collect();
    in_parallel(&cases, check_lifecycle);
}

/// A sandbox whose upper is in RAM needs no scratch image: provision records `ram` and neither a
/// scratch image nor its binding, and each start attaches the distro layer alone, which OpenVMM
/// opens, and asks the guest for the RAM upper.
#[test]
fn lifecycle_of_a_sandbox_whose_upper_is_in_ram() {
    let root = Root::new();
    root.remove("scratch.ext4");
    let state = root.below("state");
    operate(
        &root,
        "provision",
        "state",
        &["--layer", LAYER, "--upper", "ram"],
    )
    .assert_silent_success();
    assert_eq!(entries(&state), PROVISIONED);
    let mut config = json(&state.join("config.json"));
    let distro = config
        .as_object_mut()
        .unwrap()
        .remove("distro_binding")
        .expect("a distro binding");
    let layer = SandboxLayer::new(
        LayerRole::Distro,
        root.below("distro.erofs").to_str().unwrap(),
        UUID,
    );
    assert_eq!(
        distro,
        serde_json::to_value(DistroBinding::verify(&layer).unwrap()).unwrap()
    );
    let mut expected = minimal_config();
    for (name, value) in [
        ("upper", json!("ram")),
        ("scratch", Value::Null),
        ("scratch_binding", Value::Null),
    ] {
        expected[name] = value;
    }
    assert_eq!(config, root.expand_json(&expected));
    // The upper of each start is a new one.
    for round in 1..=2 {
        let context = format!("start {round}");
        operate(&root, "start", "state", &[]).assert_silent_success();
        assert_eq!(entries(&state), running(), "{context}");
        let (endpoint, process) =
            assert_runtime_record(&root, "state", &json(&state.join("runtime.json")));
        let mut options = Options::parse(&root.openvmm_arguments("build/fake-openvmm-state.json"));
        assert_checked_distro(&root, &options.take("--microvm-sandbox-block"));
        assert_opened_distro(&root, "state");
        let mut expected = managed("state", &endpoint).in_ram();
        expected.take("--microvm-sandbox-block");
        expected.assert_matches(&root, &options, &context);
        let outcome = operate(
            &root,
            "exec",
            "state",
            &["--entrypoint", "/bin/echo", "--arg", "hello"],
        );
        outcome.assert_success();
        assert_eq!(outcome.stdout(), "hello\n", "{context}");
        operate(&root, "stop", "state", &[]).assert_silent_success();
        assert!(
            process
                .wait(Instant::now() + Duration::from_secs(10))
                .unwrap(),
            "{context}: OpenVMM runs on"
        );
        assert_eq!(entries(&state), STOPPED, "{context}");
        assert_outcome_report(
            &json(&state.join("outcome.json")),
            HYPERVISOR,
            ("managed", "success", 0),
            NO_NETWORK,
        );
    }
    operate(&root, "deprovision", "state", &[]).assert_silent_success();
    assert!(!state.exists());
}

/// A workload that `exec` runs in a running sandbox, and how it ends.
struct Execution {
    /// The name of the case.
    name: &'static str,
    /// The files that the case writes below the root first, with their contents.
    files: Vec<(&'static str, Content)>,
    /// The options of `exec` after `--state-dir`, with placeholders.
    options: Vec<String>,
    /// The status with which `exec` exits.
    status: i32,
    /// What the workload wrote to standard output.
    stdout: String,
    /// What the workload wrote to standard error.
    stderr: &'static str,
    /// The code of the error with which `exec` refuses the workload, if it does, and what the
    /// error names.
    refusal: Option<(&'static str, Vec<&'static str>)>,
    /// The outcome report that `exec` writes: its path below the root, its category, and its
    /// status code.
    report: Option<(&'static str, &'static str, i64)>,
}

/// Returns the execution `name` of the workload of `options`, which exits with `status` and
/// writes `stdout`.
fn execution(name: &'static str, options: &[&str], status: i32, stdout: &str) -> Execution {
    Execution {
        name,
        files: Vec::new(),
        options: options.iter().map(|option| (*option).to_owned()).collect(),
        status,
        stdout: stdout.to_owned(),
        stderr: "",
        refusal: None,
        report: None,
    }
}

/// Returns the executions that `exec` runs, or refuses.
fn executions() -> Vec<Execution> {
    let environment_file = |name, text| {
        let mut case = execution(
            name,
            &[
                "--entrypoint",
                "/usr/bin/env",
                "--environment-file",
                "{root/env.json}",
            ],
            0,
            "",
        );
        case.files.push(("env.json", Content::Text(text)));
        case
    };
    // A refused workload would leave a value in the guest's memory, where the next one would
    // find it.
    let refused = |name, options: &[&str], code, fragments: Vec<&'static str>| {
        let mut all = vec![
            "--entrypoint",
            "/bin/sh",
            "--arg=-c",
            "--arg",
            "write ran yes",
        ];
        all.extend(options);
        let mut case = execution(name, &all, 1, "");
        case.refusal = Some((code, fragments));
        case
    };
    let mut output = execution(
        "exit status with output",
        &[
            "--entrypoint",
            "/bin/sh",
            "--arg=-c",
            "--arg",
            "echo out; echoerr err; exit 3",
            "--outcome-report",
            "{root/reports/exec.json}",
        ],
        3,
        "out\n",
    );
    output.stderr = "err\n";
    output.report = Some(("reports/exec.json", "exit", 3));
    let inherited = execution(
        "inherited environment",
        &[
            "--entrypoint",
            "/usr/bin/printenv",
            "--arg",
            "PATH",
            "--inherit-default-environment",
            "--environment",
            "A=1",
        ],
        0,
        "/usr/sbin:/usr/bin:/sbin:/bin\n",
    );
    let mut words = environment_file("environment file", r#"["X=1", "Y=two words"]"#);
    words.stdout = "X=1\nY=two words\n".to_owned();
    let mut timeout = execution(
        "timeout",
        &[
            "--entrypoint",
            "/bin/sh",
            "--arg=-c",
            "--arg",
            "sleep 5000",
            "--exec-timeout-ms",
            "50",
            "--outcome-report",
            "{root/reports/exec-timeout.json}",
        ],
        124,
        "",
    );
    timeout.report = Some(("reports/exec-timeout.json", "timeout", 124));
    let mut missing = execution(
        "missing program",
        &["--entrypoint", "/bin/missing"],
        127,
        "",
    );
    missing.stderr = "openvmm: /bin/missing: not found\n";
    let mut existing = refused(
        "existing outcome report",
        &["--outcome-report", "{root/reports/existing.json}"],
        "outcome-report-exists",
        vec![
            "outcome report already exists",
            "{root/reports/existing.json}",
        ],
    );
    existing
        .files
        .push(("reports/existing.json", Content::Text("{}\n")));
    vec![
        execution(
            "echo",
            &[
                "--entrypoint",
                "/bin/echo",
                "--arg",
                "hello",
                "--arg",
                "world",
            ],
            0,
            "hello world\n",
        ),
        execution(
            "working directory",
            &["--entrypoint", "/bin/pwd", "--cwd", "/tmp"],
            0,
            "/tmp\n",
        ),
        // A shell in the working directory exports it as `PWD`, whatever `PWD` it inherits.
        execution(
            "working directory of a shell",
            &[
                "--entrypoint",
                "/bin/sh",
                "--arg=-c",
                "--arg",
                "printenv PWD",
                "--cwd",
                "/tmp",
            ],
            0,
            "/tmp\n",
        ),
        output,
        execution(
            "exact environment",
            &[
                "--entrypoint",
                "/usr/bin/env",
                "--environment",
                "A=1",
                "--environment",
                "B=two",
            ],
            0,
            "A=1\nB=two\n",
        ),
        inherited,
        words,
        environment_file("empty environment file", "[]"),
        timeout,
        // A workload that writes while its timeout passes ends with it, and its output so far
        // follows.
        execution(
            "timeout of a writing workload",
            &[
                "--entrypoint",
                "/bin/sh",
                "--arg=-c",
                "--arg",
                "flood 65536; sleep 60000",
                "--exec-timeout-ms",
                "500",
            ],
            124,
            &"x".repeat(65536),
        ),
        execution(
            "signal",
            &["--entrypoint", "/bin/sh", "--arg=-c", "--arg", "signal 9"],
            137,
            "",
        ),
        missing,
        refused(
            "missing working directory",
            &["--cwd", "/missing"],
            "exec-refused",
            vec!["cwd-failed"],
        ),
        refused(
            "relative working directory",
            &["--cwd", "relative"],
            "unsupported",
            vec!["working directory", "absolute"],
        ),
        existing,
        refused(
            "outcome report in a missing directory",
            &["--outcome-report", "{root/missing/exec.json}"],
            "unsupported",
            vec!["outcome report", "{root/missing}"],
        ),
    ]
}

/// Asserts that the file at `path` is the outcome report of an execution, as `exec` writes it:
/// its category and status code, under a random operation ID, in schema version 1, which only its
/// owner may read on POSIX hosts.
fn assert_exec_report(path: &Path, category: &str, status: i64) {
    let report = json(path);
    let mut names: Vec<&String> = report.as_object().unwrap().keys().collect();
    names.sort();
    assert_eq!(
        names,
        ["operation_id", "outcome", "schema_version"],
        "{report}"
    );
    assert!(
        report["operation_id"]
            .as_str()
            .is_some_and(|id| is_hex(id, 32)),
        "{report}"
    );
    assert_eq!(
        report["outcome"],
        json!({"category": category, "operation": "exec", "status_code": status}),
        "{report}"
    );
    assert_eq!(report["schema_version"], 1, "{report}");
    #[cfg(unix)]
    assert_eq!(mode(path), 0o600);
}

#[test]
fn managed_execs_exit_with_the_workload_status() {
    let root = Root::new();
    root.nvx_rs(&provision("{root/state}", &[]))
        .assert_silent_success();
    root.nvx_rs(&sandbox::<&str>("start", "{root/state}", &[]))
        .assert_silent_success();
    let state = root.below("state");
    for case in executions() {
        for (relative, content) in &case.files {
            root.write_content(relative, *content);
        }
        let before = root.snapshot();
        let started = Instant::now();
        let outcome = root.nvx_rs(&sandbox("exec", "{root/state}", &case.options));
        eprintln!("case: {}", case.name);
        match &case.refusal {
            Some((code, fragments)) => {
                let fragments: Vec<String> = fragments
                    .iter()
                    .map(|fragment| root.expand(fragment))
                    .collect();
                let fragments: Vec<&str> = fragments.iter().map(String::as_str).collect();
                outcome.assert_error(code, &fragments);
                // A refusal changes nothing, but the fake OpenVMM's log of the connections to its
                // control endpoint, and the workload did not run.
                let log = "build/fake-openvmm-state-sessions.log".to_owned();
                let mut after = root.snapshot();
                after.insert(log.clone(), before[&log].clone());
                assert_eq!(after, before, "{}", case.name);
                let marker = root.nvx_rs(&sandbox(
                    "exec",
                    "{root/state}",
                    &["--entrypoint", "/bin/sh", "--arg=-c", "--arg", "read ran"],
                ));
                marker.assert_success();
                assert_eq!(marker.stdout(), "", "{}: the workload ran", case.name);
            }
            None => {
                assert_eq!(outcome.status, Some(case.status), "{}", outcome.describe());
                assert_eq!(outcome.stdout(), case.stdout, "{}", case.name);
                assert_eq!(outcome.stderr(), case.stderr, "{}", case.name);
            }
        }
        if let Some((path, category, status)) = case.report {
            assert_exec_report(&root.below(path), category, status);
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{}: the workload outlived its timeout",
            case.name
        );
        assert_eq!(entries(&state), running(), "{}", case.name);
    }
    root.nvx_rs(&sandbox::<&str>("stop", "{root/state}", &[]))
        .assert_silent_success();
    root.nvx_rs(&sandbox::<&str>("deprovision", "{root/state}", &[]))
        .assert_silent_success();
}

/// Runs the lifecycle operation `operation` of the sandbox in `{root/STATE}` with `options`, and
/// returns how it ended.
fn operate(root: &Root, operation: &str, state: &str, options: &[&str]) -> Outcome {
    root.nvx_rs(&sandbox(operation, &format!("{{root/{state}}}"), options))
}

/// Asserts that `outcome` failed with an error with the code `code` that names each of `fragments`,
/// with placeholders.
fn assert_refused(root: &Root, outcome: &Outcome, code: &str, fragments: &[&str]) {
    let fragments: Vec<String> = fragments
        .iter()
        .map(|fragment| root.expand(fragment))
        .collect();
    let fragments: Vec<&str> = fragments.iter().map(String::as_str).collect();
    outcome.assert_error(code, &fragments);
}

/// Every operation but provision fails for a state directory that does not exist, and creates
/// none.
fn operations_without_a_sandbox(root: &Root) {
    let before = root.snapshot();
    for operation in ["start", "exec", "stop", "deprovision"] {
        let outcome = operate(root, operation, "state", &[]);
        assert_refused(
            root,
            &outcome,
            "sandbox-not-provisioned",
            &["not a plain directory", "{root/state}"],
        );
        assert_eq!(root.snapshot(), before, "{operation}");
    }
}

/// Provision refuses a provisioned sandbox, start a running one, deprovision one that runs, and
/// stop and exec one that does not run, and each leaves the sandbox as it was.
fn refusals_of_a_running_sandbox(root: &Root) {
    let state = root.below("state");
    root.nvx_rs(&provision("{root/state}", &[]))
        .assert_silent_success();
    let outcome = root.nvx_rs(&provision("{root/state}", &[]));
    assert_refused(
        root,
        &outcome,
        "sandbox-provisioned",
        &["already provisioned"],
    );
    assert_eq!(entries(&state), PROVISIONED);
    operate(root, "start", "state", &[]).assert_silent_success();
    let record = json(&state.join("runtime.json"));
    let outcome = operate(root, "start", "state", &[]);
    assert_refused(root, &outcome, "sandbox-running", &["already running"]);
    let outcome = operate(root, "deprovision", "state", &[]);
    assert_refused(
        root,
        &outcome,
        "sandbox-running",
        &["must be stopped before deprovision"],
    );
    assert_eq!(entries(&state), running());
    assert_eq!(json(&state.join("runtime.json")), record);
    operate(root, "stop", "state", &[]).assert_silent_success();
    for operation in ["stop", "exec"] {
        let outcome = operate(root, operation, "state", &[]);
        assert_refused(root, &outcome, "sandbox-not-running", &["not running"]);
        assert_eq!(entries(&state), STOPPED, "{operation}");
    }
    operate(root, "deprovision", "state", &[]).assert_silent_success();
}

/// A start whose OpenVMM exits during the start fails with OpenVMM's status as soon as OpenVMM
/// exits, rather than at its timeout, keeps OpenVMM's log, and leaves no runtime state, so that
/// the next start launches it again.
fn failed_start(root: &Root) {
    let state = root.below("state");
    root.nvx_rs(&provision(
        "{root/state}",
        &["--cmdline", "fake_exit_on_start=5"],
    ))
    .assert_silent_success();
    for _ in 0..2 {
        let started = Instant::now();
        let outcome = operate(root, "start", "state", &["--timeout", "60"]);
        let elapsed = started.elapsed();
        assert_refused(
            root,
            &outcome,
            "exited",
            &["OpenVMM exited during startup", "status 5"],
        );
        assert!(
            elapsed < Duration::from_secs(60),
            "the start waited out its timeout: {elapsed:?}"
        );
        assert_eq!(
            entries(&state),
            ["config.json", "openvmm.log", "sandbox.lock"]
        );
    }
    operate(root, "deprovision", "state", &[]).assert_silent_success();
}

/// A start without the OpenVMM of the root fails before it changes anything, and deprovision,
/// which needs no OpenVMM, removes the sandbox.
fn start_without_openvmm(root: &Root) {
    root.nvx_rs(&provision("{root/state}", &[]))
        .assert_silent_success();
    root.remove(OPENVMM);
    let before = root.snapshot();
    let outcome = operate(root, "start", "state", &[]);
    assert_refused(
        root,
        &outcome,
        "artifact-missing",
        &["OpenVMM", "{root/openvmm/target/release/openvmm{exe}}"],
    );
    assert_eq!(root.snapshot(), before);
    operate(root, "deprovision", "state", &[]).assert_silent_success();
}

/// A start refuses a configuration of a format that it does not read, one that is not an object,
/// one that is not strict JSON, one without the fields of its format or with fields of other
/// types or ranges, one whose format is not that of its shares, and one whose upper is not as its
/// format records it, before it launches OpenVMM.
fn malformed_configuration(root: &Root) {
    root.nvx_rs(&provision("{root/state}", &[]))
        .assert_silent_success();
    // Format 8 records the shares under `export`, `null` for none, and the upper under `upper`;
    // format 7 records no `upper`, and format 1 records no `export` either.
    let config = json(&root.below("state/config.json"));
    let mut without_export = config.clone();
    without_export.as_object_mut().unwrap().remove("export");
    let mut memory_text = config.clone();
    memory_text["memory_mib"] = json!("256");
    let mut uid_beyond_32_bits = config.clone();
    uid_beyond_32_bits["workload_uid"] = json!(4_294_967_296_u64);
    let mut without_upper = config.clone();
    without_upper.as_object_mut().unwrap().remove("upper");
    let mut format_7_with_upper = config.clone();
    format_7_with_upper["format"] = json!(7);
    let mut other_upper = config.clone();
    other_upper["upper"] = json!("tmpfs");
    let mut scratch_without_image = config.clone();
    scratch_without_image["scratch"] = Value::Null;
    // An upper in RAM has no scratch image, nor its binding.
    let mut ram_with_scratch = config.clone();
    ram_with_scratch["upper"] = json!("ram");
    let mut ram_with_binding = ram_with_scratch.clone();
    ram_with_binding["scratch"] = Value::Null;
    let mut format_1_with_export = without_upper.clone();
    format_1_with_export["format"] = json!(1);
    let unsupported = &["unsupported format", "{root/state/config.json}"][..];
    let mismatched = &["format does not match its mounts"][..];
    let malformed = &["sandbox configuration is malformed"][..];
    for (text, code, fragments) in [
        (
            r#"{"format": 9}"#.to_owned(),
            "format-unsupported",
            unsupported,
        ),
        ("[]".to_owned(), "format-unsupported", unsupported),
        // Only the integers 8, 7, and 1 are formats, and the configuration is strict JSON, in
        // which no object repeats a field.
        (
            r#"{"format": 8.0}"#.to_owned(),
            "format-unsupported",
            unsupported,
        ),
        (
            r#"{"format": NaN}"#.to_owned(),
            "io",
            &[
                "failed to read sandbox configuration",
                "{root/state/config.json}",
            ][..],
        ),
        (
            r#"{"format": 8, "format": 8}"#.to_owned(),
            "io",
            &[
                "failed to read sandbox configuration",
                "{root/state/config.json}",
                "duplicate field \"format\"",
            ][..],
        ),
        (r#"{"format": 1}"#.to_owned(), "io", malformed),
        (r#"{"format": 8}"#.to_owned(), "io", malformed),
        (memory_text.to_string(), "io", malformed),
        (uid_beyond_32_bits.to_string(), "io", malformed),
        (without_upper.to_string(), "io", malformed),
        (format_7_with_upper.to_string(), "io", malformed),
        (other_upper.to_string(), "io", malformed),
        (scratch_without_image.to_string(), "io", malformed),
        (ram_with_scratch.to_string(), "io", malformed),
        (ram_with_binding.to_string(), "io", malformed),
        (without_export.to_string(), "format-unsupported", mismatched),
        (
            format_1_with_export.to_string(),
            "format-unsupported",
            mismatched,
        ),
    ] {
        root.write("state/config.json", &text);
        let before = root.snapshot();
        let outcome = operate(root, "start", "state", &[]);
        assert_refused(root, &outcome, code, fragments);
        assert_eq!(root.snapshot(), before, "{text}");
    }
    operate(root, "deprovision", "state", &[]).assert_silent_success();
}

/// A start refuses a configuration that records guest RAM outside the range of `--memory-mib`,
/// as `nvx.py` and earlier releases recorded for any integer that it gave, and OpenVMM refused at
/// every start. It names the RAM and advises provisioning the sandbox again before it launches
/// OpenVMM, and deprovision then removes the sandbox.
fn unusable_guest_ram(root: &Root) {
    root.nvx_rs(&provision("{root/state}", &[]))
        .assert_silent_success();
    let mut config = json(&root.below("state/config.json"));
    config["memory_mib"] = json!("MEMORY");
    let template = config.to_string();
    // The error names each value as the configuration records it, but an integer beyond 64 bits,
    // which `start` reads as a double.
    for (recorded, named) in [
        ("0", true),
        ("-1", true),
        // The least MiB whose count of bytes, 2^64, overflows 64 bits.
        ("17592186044416", true),
        ("99999999999999", true),
        ("18446744073709551616", false),
        ("256.5", true),
    ] {
        root.write(
            "state/config.json",
            template.replace("\"MEMORY\"", recorded),
        );
        let before = root.snapshot();
        let outcome = operate(root, "start", "state", &[]);
        let value = format!("sandbox configuration records {recorded} MiB of guest RAM");
        let mut fragments = vec![
            "MiB of guest RAM",
            "between 1 and 17592186044415",
            "deprovision the sandbox and provision it again",
        ];
        if named {
            fragments.push(&value);
        }
        assert_refused(root, &outcome, "format-unsupported", &fragments);
        assert_eq!(root.snapshot(), before, "{recorded}");
    }
    operate(root, "deprovision", "state", &[]).assert_silent_success();
    assert!(!root.below("state").exists());
}

/// A state directory must be a directory: a file there fails every operation, and provision
/// leaves it as it is.
fn not_a_directory(root: &Root) {
    let before = root.snapshot();
    let outcome = operate(root, "start", "single.txt", &[]);
    assert_refused(
        root,
        &outcome,
        "sandbox-not-provisioned",
        &["not a plain directory", "{root/single.txt}"],
    );
    let outcome = root.nvx_rs(&provision("{root/single.txt}", &[]));
    assert_refused(root, &outcome, "io", &["single.txt"]);
    let outcome = operate(root, "deprovision", "single.txt", &[]);
    assert_refused(
        root,
        &outcome,
        "sandbox-not-provisioned",
        &["not a plain directory", "{root/single.txt}"],
    );
    assert_eq!(root.snapshot(), before);
}

/// Deprovision removes the files of NVX, but keeps a state directory that holds another, which it
/// names.
fn foreign_file(root: &Root) {
    root.nvx_rs(&provision("{root/state}", &[]))
        .assert_silent_success();
    root.write("state/foreign.txt", "x");
    let outcome = operate(root, "deprovision", "state", &[]);
    assert_refused(
        root,
        &outcome,
        "foreign-files",
        &["not owned by NVX", "foreign.txt"],
    );
    assert_eq!(entries(&root.below("state")), ["foreign.txt"]);
}

/// `--dry-run` fails every lifecycle operation, and changes nothing of a running sandbox
/// (microsoft/nvx#373).
fn dry_runs_of_lifecycle_operations(root: &Root) {
    root.nvx_rs(&provision("{root/state}", &[]))
        .assert_silent_success();
    operate(root, "start", "state", &[]).assert_silent_success();
    let before = root.snapshot();
    for arguments in [
        provision("{root/state}", &["--dry-run"]),
        sandbox("start", "{root/state}", &["--dry-run"]),
        sandbox(
            "exec",
            "{root/state}",
            &["--entrypoint", "/bin/true", "--dry-run"],
        ),
        sandbox("stop", "{root/state}", &["--dry-run"]),
        sandbox("deprovision", "{root/state}", &["--dry-run"]),
    ] {
        let outcome = root.nvx_rs(&arguments);
        assert_refused(root, &outcome, "unsupported", &["--dry-run", "sandbox run"]);
        assert_eq!(root.snapshot(), before, "{arguments:?}");
    }
    operate(root, "stop", "state", &[]).assert_silent_success();
    operate(root, "deprovision", "state", &[]).assert_silent_success();
}

/// A process that no runtime record names, as an OpenVMM whose record a check cleared: it kills
/// the process and waits for it when it drops, also when the check fails, since a [`Root`] ends
/// only the processes that the records below it name.
struct Unrecorded(Process);

impl std::ops::Deref for Unrecorded {
    type Target = Process;

    fn deref(&self) -> &Process {
        &self.0
    }
}

impl Drop for Unrecorded {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait(Instant::now() + Duration::from_secs(10));
    }
}

/// A runtime record that names the ID of its OpenVMM with another start time, as after OpenVMM
/// exited and another process took its ID, names no process that runs: exec and stop report it
/// as stale and clear it, and signal no process (microsoft/nvx#369).
fn recycled_process_id(root: &Root) {
    root.nvx_rs(&provision("{root/state}", &[]))
        .assert_silent_success();
    operate(root, "start", "state", &[]).assert_silent_success();
    let path = root.below("state/runtime.json");
    let mut record = json(&path);
    let reference: ProcessRef = serde_json::from_value(record["process"].clone()).unwrap();
    let openvmm = Unrecorded(Process::open(&reference).unwrap().expect("OpenVMM runs"));
    let start_time = record["process"]["startTime"].as_u64().unwrap();
    record["process"]["startTime"] = json!(start_time + 1);
    fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    let outcome = operate(root, "exec", "state", &["--entrypoint", "/bin/true"]);
    assert_refused(root, &outcome, "sandbox-exited", &["stale", "not running"]);
    assert!(!path.exists());
    assert!(
        !openvmm
            .wait(Instant::now() + Duration::from_millis(200))
            .unwrap(),
        "the stale record's process was signaled"
    );
    openvmm.kill().unwrap();
    assert!(
        openvmm
            .wait(Instant::now() + Duration::from_secs(10))
            .unwrap()
    );
    let outcome = operate(root, "stop", "state", &[]);
    assert_refused(root, &outcome, "sandbox-not-running", &["not running"]);
    operate(root, "start", "state", &[]).assert_silent_success();
    operate(root, "stop", "state", &[]).assert_silent_success();
    operate(root, "deprovision", "state", &[]).assert_silent_success();
}

/// Two starts at once launch one OpenVMM: the lifecycle lock serializes them, so that the second
/// finds the sandbox that the first started running (microsoft/nvx#464).
fn concurrent_starts(root: &Root) {
    root.nvx_rs(&provision("{root/state}", &[]))
        .assert_silent_success();
    let arguments: Vec<String> = sandbox::<&str>("start", "{root/state}", &[])
        .iter()
        .map(|argument| root.expand(argument))
        .collect();
    let starts: Vec<_> = (0..2)
        .map(|_| {
            root.command(&arguments)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut outcomes: Vec<Outcome> = starts
        .into_iter()
        .map(|start| Outcome::new(arguments.clone(), start.wait_with_output().unwrap()))
        .collect();
    outcomes.sort_by_key(|outcome| outcome.status);
    outcomes[0].assert_silent_success();
    assert_refused(root, &outcomes[1], "sandbox-running", &["already running"]);
    let state = root.below("state");
    assert_eq!(entries(&state), running());
    assert_runtime_record(root, "state", &json(&state.join("runtime.json")));
    operate(root, "stop", "state", &[]).assert_silent_success();
    operate(root, "deprovision", "state", &[]).assert_silent_success();
}

/// A refusal of a lifecycle operation: its name, and the steps that lead to it and check it.
type Refusal = (&'static str, fn(&Root));

/// The refusals of the lifecycle operations.
const REFUSALS: [Refusal; 11] = [
    ("operations without a sandbox", operations_without_a_sandbox),
    (
        "refusals of a running sandbox",
        refusals_of_a_running_sandbox,
    ),
    ("failed start", failed_start),
    ("start without OpenVMM", start_without_openvmm),
    ("malformed configuration", malformed_configuration),
    ("unusable guest RAM", unusable_guest_ram),
    ("not a directory", not_a_directory),
    ("foreign file", foreign_file),
    (
        "dry runs of lifecycle operations",
        dry_runs_of_lifecycle_operations,
    ),
    ("recycled process ID", recycled_process_id),
    ("concurrent starts", concurrent_starts),
];

#[test]
fn refusals_of_lifecycle_operations() {
    in_parallel(&REFUSALS, |(name, refusal)| {
        eprintln!("case: {name}");
        refusal(&Root::new());
    });
}

/// The lock files that Windows refuses to open: every lifecycle operation fails at once for one
/// that refuses access for good, and provision waits for a removed one, which a file system
/// without POSIX deletion keeps under its name until its last handle closes (microsoft/nvx#464).
#[cfg(windows)]
mod windows {
    use std::fs::{self, File};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::thread;
    use std::time::Duration;

    use nvx::host::store::SandboxStore;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileDispositionInfo, SetFileInformationByHandle,
    };

    use super::{PROVISIONED, assert_refused, operate, provision};
    use crate::harness::{Outcome, Root, entries};

    /// Creates the state directory `STATE` below the root, private to the current user and
    /// without a sandbox, as an interrupted deprovision leaves it, and returns the path of its
    /// lock file.
    fn unprovisioned(root: &Root, state: &str) -> PathBuf {
        let dir = root.below(state);
        SandboxStore::create(&dir).unwrap();
        dir.join("sandbox.lock")
    }

    /// Sets or clears the read-only attribute of the file `path`.
    fn set_read_only(path: &Path, read_only: bool) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(read_only);
        fs::set_permissions(path, permissions).unwrap();
    }

    /// Deletes the file `path` as a file system without POSIX deletion deletes a file that other
    /// handles hold: its name stays, and opening it fails, until the last handle closes, of which
    /// the returned one is the last unless others hold the file.
    fn delete_pending(path: &Path) -> File {
        let file = fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
            .unwrap();
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: the handle is open, and the information is a FILE_DISPOSITION_INFO of its size.
        let set = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle() as HANDLE,
                FileDispositionInfo,
                (&raw const disposition).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
        assert_ne!(set, 0, "{}", std::io::Error::last_os_error());
        file
    }

    #[test]
    fn operations_fail_at_once_for_a_lock_file_that_refuses_access_for_good() {
        let root = Root::new();
        // A read-only lock file, and a directory at its path, refuse an open for writing as a
        // removed lock file does, but for good.
        for (state, read_only) in [("read-only", true), ("directory", false)] {
            let lock = unprovisioned(&root, state);
            if read_only {
                fs::write(&lock, b"").unwrap();
                set_read_only(&lock, true);
            } else {
                fs::create_dir(&lock).unwrap();
            }
            let before = root.snapshot();
            let path = format!("{{root/{state}}}");
            // A provision that waited for the lock file would fail with `sandbox-busy` once its
            // timeout passed.
            let mut outcomes = vec![root.nvx_rs(&provision(&path, &["--timeout", "5"]))];
            for operation in ["start", "exec", "stop", "deprovision"] {
                outcomes.push(operate(&root, operation, state, &[]));
            }
            let named = format!("{{root/{state}/sandbox.lock}}");
            let fragments = ["cannot lock", named.as_str(), "(os error 5)"];
            for outcome in &outcomes {
                assert_refused(&root, outcome, "io", &fragments);
            }
            assert_eq!(root.snapshot(), before, "{state}");
            if read_only {
                set_read_only(&lock, false);
            }
        }
    }

    #[test]
    fn provision_waits_for_a_removed_lock_file_that_a_handle_keeps() {
        let root = Root::new();
        let lock = unprovisioned(&root, "state");
        fs::write(&lock, b"").unwrap();
        let pending = delete_pending(&lock);
        let arguments: Vec<String> = provision("{root/state}", &["--timeout", "30"])
            .iter()
            .map(|argument| root.expand(argument))
            .collect();
        let mut child = root
            .command(&arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(500));
        if child.try_wait().unwrap().is_some() {
            let outcome = Outcome::new(arguments, child.wait_with_output().unwrap());
            panic!(
                "provision did not wait for the removed lock file: {}",
                outcome.describe()
            );
        }
        drop(pending);
        let outcome = Outcome::new(arguments, child.wait_with_output().unwrap());
        outcome.assert_silent_success();
        assert_eq!(entries(&root.below("state")), PROVISIONED);
        operate(&root, "deprovision", "state", &[]).assert_silent_success();
    }
}
