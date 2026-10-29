//! The host program's template operations without a hypervisor: `list`, `inspect`, and `remove`
//! of template stores that hold no generation, or of directories that hold no template store,
//! `refresh`, which refuses a template before it creates anything, as `create` does, and the
//! command lines of the operations that the host program refuses. The VM tests of
//! `tests/vm_template.rs` create, refresh, list, inspect, and remove generations on a real
//! hypervisor.
#![cfg(feature = "host")]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nvx::host::artifacts::Artifacts;
use nvx::host::store::SandboxStore;
use nvx::host::template::Store;
use serde_json::{Value, json};

/// The exit status of a command that failed, after a diagnostic.
const FAILED: i32 = 1;
/// The exit status of a usage error.
const USAGE: i32 = 2;

/// Returns a directory of the test `name`, empty, below Cargo's temporary directory.
fn directory(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    directory
}

/// Runs the host program's `command` on the template store `store`, with `arguments` after it,
/// without NVX's artifacts, and returns its output.
fn run(command: &str, store: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"))
        .arg(command)
        .arg(store)
        .args(arguments)
        .env_remove(Artifacts::ENV_DIR)
        .output()
        .unwrap()
}

/// Checks that `output` is of a command that exited with `status`, and returns its standard
/// error.
fn exited(output: &Output, status: i32) -> String {
    assert_eq!(output.status.code(), Some(status), "{output:?}");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Returns the serde form of a network with egress allowed, and ingress and host-loopback access
/// denied, which a template takes.
fn egress_network() -> Value {
    json!({
        "guestNetwork": "192.0.2.2/24",
        "egress": "allow",
        "allow": {"name": "egress.allow", "rules": []},
        "deny": {"name": "egress.deny", "rules": []},
        "ingress": "deny",
        "hostLoopback": "deny",
        "hostLoopbackForwards": [],
        "proxy": null,
    })
}

#[test]
fn the_operations_open_a_template_store_and_create_none() {
    let directory = directory("template-operations-open");
    // A directory that does not exist, and a private one that holds no template store, as a
    // sandbox's store does not, fail every operation that opens a store, which creates nothing.
    let missing = directory.join("missing");
    let sandbox = directory.join("sandbox");
    SandboxStore::create(&sandbox).unwrap();
    let operations: [(&str, &[&str]); 3] = [("list", &[]), ("inspect", &["1"]), ("remove", &["1"])];
    for (command, arguments) in operations {
        let stderr = exited(&run(command, &missing, arguments), FAILED);
        assert!(
            stderr.contains(&format!(
                "cannot open the template store {}",
                missing.display()
            )),
            "{command}: {stderr}"
        );
        assert!(!missing.exists(), "{command}");

        let stderr = exited(&run(command, &sandbox, arguments), FAILED);
        assert!(
            stderr.contains(&format!(
                "the directory {} holds no template store",
                sandbox.display()
            )),
            "{command}: {stderr}"
        );
        assert!(Store::open(&sandbox).is_err(), "{command}");
    }
}

#[test]
fn an_empty_template_store_lists_no_generation() {
    let directory = directory("template-operations-empty");
    let store = Store::create(directory.join("store")).unwrap();

    // The store, and none of its generations.
    let output = run("list", store.dir(), &[]);
    exited(&output, 0);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("store={}\ngenerations=none\n", store.id())
    );

    // No generation to inspect or remove: the store never published one.
    let stderr = exited(&run("inspect", store.dir(), &["1"]), FAILED);
    assert!(
        stderr.contains(&format!(
            "the template store {} never published a generation 1",
            store.dir().display()
        )),
        "{stderr}"
    );
    let stderr = exited(&run("remove", store.dir(), &["1"]), FAILED);
    assert!(
        stderr.contains(&format!(
            "the template store {} holds no generation 1",
            store.dir().display()
        )),
        "{stderr}"
    );

    // A failed removal tombstones nothing.
    assert!(store.index().unwrap().generations().is_empty());
    assert!(store.tombstones().unwrap().is_empty());
}

#[test]
fn a_refresh_refuses_a_template_before_it_creates_anything() {
    let directory = directory("template-operations-refresh");
    let distro = directory.join("placeholder.erofs");
    fs::write(&distro, vec![0; 4096]).unwrap();
    let store = directory.join("templates");
    // Without NVX's artifacts, a refresh fails once it has read its network, as a creation does.
    let refresh = |network: Option<&Path>| -> String {
        let mut image = std::ffi::OsString::from("erofs:");
        image.push(&distro);
        let mut arguments = vec![
            OsStr::new("--guest-agent"),
            OsStr::new("agent"),
            OsStr::new("--hypervisor"),
            OsStr::new("kvm"),
            OsStr::new("--distro"),
            &image,
            OsStr::new("--distro-identity"),
            OsStr::new("0a490905-41be-ed5f-984e-63eb7e5e20b1"),
        ];
        if let Some(network) = network {
            arguments.extend([OsStr::new("--network"), network.as_os_str()]);
        }
        let output = Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"))
            .arg("refresh")
            .arg(&store)
            .args(arguments)
            .env_remove(Artifacts::ENV_DIR)
            .output()
            .unwrap();
        let stderr = exited(&output, FAILED);
        // Nothing was created: not even the store.
        assert!(!store.exists(), "{output:?}");
        stderr
    };

    // A network that the plan refuses fails the refresh first.
    let network = directory.join("network.json");
    let mut proxy = egress_network();
    proxy["proxy"] = json!({"port": 3128, "scheme": "http"});
    fs::write(&network, proxy.to_string()).unwrap();
    let stderr = refresh(Some(&network));
    let refused = format!(
        "the network {} is refused: a network proxy needs egress denied by default",
        network.display()
    );
    assert!(stderr.contains(&refused), "{stderr}");

    // A network that the plan takes passes, as does no network, and the refresh goes on to need
    // NVX's artifacts.
    fs::write(&network, egress_network().to_string()).unwrap();
    for network in [Some(network.as_path()), None] {
        let stderr = refresh(network);
        assert!(
            stderr.contains(&format!("set {} to NVX's artifacts", Artifacts::ENV_DIR)),
            "{stderr}"
        );
    }
}

#[test]
fn the_host_program_refuses_command_lines_of_the_operations() {
    let refresh = [
        "refresh",
        "templates",
        "--guest-agent",
        "agent",
        "--distro",
        "erofs:image",
        "--distro-identity",
        "0a490905-41be-ed5f-984e-63eb7e5e20b1",
    ];
    let with = |options: &[&'static str]| [&refresh[..], options].concat();
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["refresh"], "refresh needs a template store"),
        (vec!["refresh", "templates"], "refresh needs --guest-agent"),
        (refresh[..4].to_vec(), "refresh needs --distro"),
        (refresh[..6].to_vec(), "refresh needs --distro-identity"),
        (
            with(&["--processors", "0"]),
            "--processors takes a positive number, not \"0\"",
        ),
        (
            with(&["--boot-processors", "one"]),
            "--boot-processors takes a positive number, not \"one\"",
        ),
        (
            with(&["--memory", "256M"]),
            "--memory takes a positive number of MiB, not \"256M\"",
        ),
        (
            with(&["--cpu-profile", "auto", "--cpu-profile", "host"]),
            "unexpected option \"--cpu-profile\" of refresh",
        ),
        (
            with(&["--upper", "ram"]),
            "unexpected option \"--upper\" of refresh",
        ),
        (
            with(&["--workload-identity", "1000:1000"]),
            "unexpected option \"--workload-identity\" of refresh",
        ),
        (with(&["image"]), "unexpected argument \"image\" of refresh"),
        (vec!["list"], "list needs a template store"),
        (
            vec!["list", "templates", "1"],
            "unexpected argument \"1\" of list",
        ),
        (
            vec!["list", "templates", "--guest-build", "build"],
            "unexpected option \"--guest-build\" of list",
        ),
        (vec!["inspect"], "inspect needs a template store"),
        (vec!["inspect", "templates"], "inspect needs a generation"),
        (
            vec!["inspect", "templates", "0"],
            "inspect takes a generation, a positive number, not \"0\"",
        ),
        (
            vec!["inspect", "templates", "first"],
            "inspect takes a generation, a positive number, not \"first\"",
        ),
        (
            vec!["inspect", "templates", "-1"],
            "inspect takes a generation, a positive number, not \"-1\"",
        ),
        (
            vec!["inspect", "templates", "1", "2"],
            "unexpected argument \"2\" of inspect",
        ),
        (
            vec!["inspect", "templates", "--all"],
            "unexpected option \"--all\" of inspect",
        ),
        (vec!["remove"], "remove needs a template store"),
        (vec!["remove", "templates"], "remove needs a generation"),
        // A store generation is a nonzero 64-bit number.
        (
            vec!["remove", "templates", "18446744073709551616"],
            "remove takes a generation, a positive number, not \"18446744073709551616\"",
        ),
        (
            vec!["remove", "templates", "1", "--force"],
            "unexpected option \"--force\" of remove",
        ),
    ];
    for (arguments, problem) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"))
            .args(&arguments)
            .output()
            .unwrap();
        let stderr = exited(&output, USAGE);
        assert!(
            stderr.contains(problem) && stderr.contains("usage: runtime-adapter-host"),
            "{arguments:?}: {stderr}"
        );
    }
}
