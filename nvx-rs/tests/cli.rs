//! Replays the scenarios of `tests/fixtures/cli.json` with the `nvx-rs` binary, and checks what
//! `nvx-rs` adds to `nvx.py`.
//!
//! `scripts/nvx_rs_fixtures.py` records the scenarios from `nvx.py sandbox` and from the Python
//! functions behind `nvx-rs egress compile` and `nvx-rs artifacts`, with `nvx-fake-openvmm` as
//! OpenVMM, in a section for POSIX hosts and one for Windows. Each scenario runs here in a fresh
//! root, with `nvx-rs --root ROOT`, and every step must exit with the recorded status, print the
//! recorded output, and leave the recorded files, after the root, process IDs, and random
//! identifiers are replaced with the placeholders that the script uses.
#![cfg(all(
    feature = "cli",
    feature = "testing",
    any(target_os = "linux", windows)
))]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

/// The recorded scenarios.
const FIXTURE: &str = include_str!("fixtures/cli.json");
/// The CLI under test.
const NVX_RS: &str = env!("CARGO_BIN_EXE_nvx-rs");
/// The fake OpenVMM, which the root of every scenario holds as its OpenVMM executable.
const FAKE_OPENVMM: &str = env!("CARGO_BIN_EXE_nvx-fake-openvmm");
/// The section of the fixture for this host.
const PLATFORM: &str = if cfg!(windows) { "windows" } else { "posix" };
/// The files whose permissions the CLI sets, which the fixture records on POSIX hosts.
#[cfg(unix)]
const MODE_NAMES: [&str; 5] = [
    "state",
    "config.json",
    "runtime.json",
    "control.capability",
    "lifecycle.lock",
];
/// How many scenarios run at once.
const PARALLELISM: usize = 4;

/// A fresh scenario root, removed on drop after the fake OpenVMM processes that it records end.
struct Root {
    /// The temporary directory.
    _directory: tempfile::TempDir,
    /// Its canonical path, as Python's `Path.resolve()` gives it.
    path: PathBuf,
}

impl Root {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("nvx-cli-")
            .tempdir()
            .unwrap();
        let path = canonical(directory.path());
        let text = path.to_str().expect("the temporary directory is Unicode");
        // The recorded commands quote no part of the root, as `shlex.join` or
        // `subprocess.list2cmdline` would quote a path with spaces or other special characters.
        let plain = if cfg!(windows) {
            text.is_ascii() && !text.contains([' ', '\t', '"'])
        } else {
            text.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
        };
        assert!(
            plain,
            "the golden CLI tests need a temporary directory without spaces or special \
             characters, not {text}; set TMPDIR or TEMP"
        );
        Self {
            _directory: directory,
            path,
        }
    }

    fn text(&self) -> &str {
        self.path.to_str().unwrap()
    }

    /// Returns the path of `relative`, with `/` separators and placeholders, below the root.
    fn below(&self, relative: &str) -> PathBuf {
        self.expand(relative)
            .split('/')
            .fold(self.path.clone(), |path, part| path.join(part))
    }

    /// Replaces the `{root}` and `{exe}` placeholders of an input.
    fn expand(&self, text: &str) -> String {
        text.replace("{root}", self.text())
            .replace("{exe}", std::env::consts::EXE_SUFFIX)
    }

    /// Replaces the root and the volatile values of an output with the fixture's placeholders.
    fn normalize(&self, text: &str) -> String {
        let raw = self.text();
        let escaped = python_json_escape(raw);
        let mut text = text.to_owned();
        if escaped != raw {
            text = text.replace(&escaped, "{root_json}");
        }
        text = text.replace(raw, "{root}");
        for prefix in ["\"pid\": ", "\"start_time\": "] {
            text = replace_after(&text, prefix, |c| c.is_ascii_digit(), None, "{int}");
        }
        text = replace_after(&text, "openvmm-microvm-", is_hex, Some(32), "{hex32}");
        for prefix in ["\"operation_id\": \"", "\"instance_id\": \""] {
            text = replace_after(&text, prefix, is_hex, Some(32), "{hex32}");
        }
        text
    }

    /// Creates the tree of the fixture.
    fn populate(&self, tree: &[Value]) {
        for entry in tree {
            let path = self.below(entry["path"].as_str().unwrap());
            if entry.get("openvmm").is_some() {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::copy(FAKE_OPENVMM, &path).unwrap();
            } else if let Some(text) = entry.get("text") {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, text.as_str().unwrap()).unwrap();
            } else {
                fs::create_dir_all(&path).unwrap();
            }
        }
    }

    /// Returns what a step shows of a file or directory below the root, as the script records
    /// it.
    fn snapshot(&self, relative: &str) -> Value {
        let path = self.below(relative);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return Value::Null;
        };
        let name = path.file_name().unwrap().to_str().unwrap();
        let mut entry = serde_json::Map::new();
        if metadata.is_dir() {
            let mut names: Vec<String> = fs::read_dir(&path)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            entry.insert("entries".into(), json!(names));
        } else if name == "control.capability" {
            entry.insert("size".into(), json!(metadata.len()));
        } else {
            let text = String::from_utf8(fs::read(&path).unwrap()).unwrap();
            entry.insert("text".into(), json!(self.normalize(&text)));
        }
        #[cfg(unix)]
        if MODE_NAMES.contains(&name) || name.starts_with("exec") {
            use std::os::unix::fs::PermissionsExt;
            entry.insert("mode".into(), json!(metadata.permissions().mode() & 0o7777));
        }
        Value::Object(entry)
    }

    /// Ends the fake OpenVMM processes that the runtime records below the root name, so that the
    /// root can be removed after a failure.
    fn end_leftovers(&self) {
        let mut pending = vec![self.path.clone()];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if entry.file_name() == "runtime.json"
                    && let Some(pid) = fs::read_to_string(&path)
                        .ok()
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                        .and_then(|record| record["pid"].as_u64())
                {
                    kill(pid);
                }
            }
        }
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        self.end_leftovers();
    }
}

#[cfg(unix)]
fn kill(pid: u64) {
    if let Ok(pid) = i32::try_from(pid) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(windows)]
fn kill(pid: u64) {
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    thread::sleep(Duration::from_millis(200));
}

/// Returns the canonical form of `path`, without the `\\?\` prefix that Windows adds and Python's
/// `Path.resolve()` drops.
fn canonical(path: &Path) -> PathBuf {
    let path = fs::canonicalize(path).unwrap();
    let text = path.to_str().unwrap();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

fn is_hex(c: char) -> bool {
    c.is_ascii_digit() || ('a'..='f').contains(&c)
}

/// Replaces each run of characters of `class` after `prefix`, of exactly `length` characters if
/// given, with `placeholder`.
fn replace_after(
    text: &str,
    prefix: &str,
    class: fn(char) -> bool,
    length: Option<usize>,
    placeholder: &str,
) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find(prefix) {
        let start = position + prefix.len();
        output.push_str(&rest[..start]);
        let tail = &rest[start..];
        let run = tail.find(|c| !class(c)).unwrap_or(tail.len());
        if run > 0 && length.is_none_or(|length| run == length) {
            output.push_str(placeholder);
            rest = &tail[run..];
        } else {
            rest = tail;
        }
    }
    output.push_str(rest);
    output
}

/// Returns `json.dumps(text)` without its quotes: `ensure_ascii` escapes.
fn python_json_escape(text: &str) -> String {
    let mut output = String::new();
    for c in text.chars() {
        match c {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            ' '..='~' => output.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    output.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    output
}

/// Runs `nvx-rs --root ROOT ARGUMENTS` in the root, without the variables that change its
/// output.
fn nvx_rs(root: &Root, arguments: &[String], environment: &BTreeMap<String, String>) -> Output {
    Command::new(NVX_RS)
        .arg("--root")
        .arg(&root.path)
        .args(arguments)
        .current_dir(&root.path)
        .env_remove("NVX_OPENVMM")
        .env_remove("COLUMNS")
        .env_remove("LINES")
        .envs(environment)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Writes a file of a step: text, `hex` bytes, or `text` then `fill` repeated to `size` bytes.
fn write_file(path: &Path, content: &Value) {
    let data = match content {
        Value::String(text) => text.clone().into_bytes(),
        Value::Object(spec) => match spec.get("hex") {
            Some(hex) => (0..hex.as_str().unwrap().len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex.as_str().unwrap()[i..i + 2], 16).unwrap())
                .collect(),
            None => {
                let mut data = spec
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .as_bytes()
                    .to_vec();
                let fill = spec["fill"].as_str().unwrap().as_bytes();
                let size = usize::try_from(spec["size"].as_u64().unwrap()).unwrap();
                while data.len() < size {
                    data.extend_from_slice(fill);
                }
                data
            }
        },
        other => panic!("invalid file content {other}"),
    };
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

/// Plays a scenario and returns how its steps differ from the recording.
fn play(tree: &[Value], scenario: &Value) -> Vec<String> {
    let name = scenario["name"].as_str().unwrap();
    let root = Root::new();
    root.populate(tree);
    let mut problems = Vec::new();
    for (index, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
        for relative in step
            .get("remove")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            fs::remove_file(root.below(relative.as_str().unwrap())).unwrap();
        }
        for (relative, content) in step
            .get("write")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            write_file(&root.below(relative), content);
        }
        let arguments: Vec<String> = step["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|argument| root.expand(argument.as_str().unwrap()))
            .collect();
        let environment: BTreeMap<String, String> = step
            .get("env")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(name, value)| (name.clone(), root.expand(value.as_str().unwrap())))
            .collect();
        let output = nvx_rs(&root, &arguments, &environment);
        let mut actual = json!({
            "status": output.status.code(),
            "stdout": root.normalize(&String::from_utf8_lossy(&output.stdout)),
            "stderr": root.normalize(&String::from_utf8_lossy(&output.stderr)),
        });
        if let Some(checks) = step.get("check").and_then(Value::as_array) {
            let files: serde_json::Map<String, Value> = checks
                .iter()
                .map(|relative| {
                    let relative = relative.as_str().unwrap();
                    (relative.to_owned(), root.snapshot(relative))
                })
                .collect();
            actual["files"] = Value::Object(files);
        }
        for field in ["status", "stdout", "stderr", "files"] {
            let expected = step.get(field).unwrap_or(&Value::Null);
            let got = actual.get(field).unwrap_or(&Value::Null);
            if expected != got {
                problems.push(format!(
                    "{name}, step {} ({}), {field}:\n  expected: {expected}\n  actual:   {got}",
                    index + 1,
                    arguments.join(" ")
                ));
            }
        }
    }
    problems
}

#[test]
fn nvx_rs_reproduces_the_recorded_scenarios() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let tree = fixture["tree"].as_array().unwrap().clone();
    let scenarios = fixture[PLATFORM].as_array().unwrap().clone();
    assert!(
        !scenarios.is_empty(),
        "tests/fixtures/cli.json has no {PLATFORM} scenarios"
    );
    let chunks: Vec<&[Value]> = scenarios
        .chunks(scenarios.len().div_ceil(PARALLELISM))
        .collect();
    let problems: Vec<String> = thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .iter()
            .map(|chunk| {
                let tree = &tree;
                scope.spawn(move || {
                    chunk
                        .iter()
                        .flat_map(|scenario| play(tree, scenario))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect()
    });
    assert!(
        problems.is_empty(),
        "{} differences from nvx.py:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// Returns the arguments that provision a minimal sandbox in `state` below the root.
fn provision(root: &Root, state: &str, command_line: &str) -> Vec<String> {
    let path = |name: &str| root.path.join(name).to_str().unwrap().to_owned();
    vec![
        "sandbox".into(),
        "provision".into(),
        "--state-dir".into(),
        path(state),
        "--layer".into(),
        format!(
            "distro,{},11111111-1111-1111-1111-111111111111",
            path("distro.erofs")
        ),
        "--scratch".into(),
        path("scratch.ext4"),
        "--cmdline".into(),
        command_line.into(),
    ]
}

/// Returns a root with the fixture's tree.
fn populated_root() -> Root {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let root = Root::new();
    root.populate(fixture["tree"].as_array().unwrap());
    root
}

fn run(root: &Root, arguments: &[String]) -> Output {
    nvx_rs(root, arguments, &BTreeMap::new())
}

fn lifecycle(operation: &str, root: &Root, state: &str) -> Vec<String> {
    vec![
        "sandbox".into(),
        operation.into(),
        "--state-dir".into(),
        root.path.join(state).to_str().unwrap().to_owned(),
    ]
}

fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data).replace("\r\n", "\n")
}

#[test]
fn start_requires_the_guest_features_of_layered_sandboxes() {
    let root = populated_root();
    // A guest that lacks per-execution environments, or that predates feature negotiation, is
    // stopped, and its start fails like any other.
    for (state, command_line) in [
        ("cancel-only", "fake_guest_features=1"),
        ("legacy", "fake_legacy_guest=1"),
    ] {
        let output = run(&root, &provision(&root, state, command_line));
        assert!(output.status.success(), "{}", text(&output.stderr));
        let output = run(&root, &lifecycle("start", &root, state));
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            text(&output.stderr),
            "error: the guest image lacks control features that layered sandboxes need \
             (per-execution environments); use an NVX release or build that includes them\n"
        );
        let mut names: Vec<String> = fs::read_dir(root.path.join(state))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["config.json", "lifecycle.lock", "openvmm.log"]);
        let output = run(&root, &lifecycle("deprovision", &root, state));
        assert!(output.status.success(), "{}", text(&output.stderr));
    }
    // Per-execution environments are all that the CLI needs: neither cancellation nor workload
    // accounts.
    let output = run(
        &root,
        &provision(&root, "environments", "fake_guest_features=16"),
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    for operation in ["start", "stop", "deprovision"] {
        let output = run(&root, &lifecycle(operation, &root, "environments"));
        assert!(
            output.status.success(),
            "{operation}: {}",
            text(&output.stderr)
        );
    }
}

#[test]
fn artifacts_default_to_the_checkout_that_holds_the_executable() {
    let output = Command::new(NVX_RS)
        .arg("artifacts")
        .env_remove("NVX_OPENVMM")
        .output()
        .unwrap();
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let in_checkout = fs::canonicalize(manifest_dir.join("target"))
        .is_ok_and(|target| canonical(Path::new(NVX_RS)).starts_with(canonical(&target)));
    if in_checkout {
        assert!(output.status.success(), "{}", text(&output.stderr));
        let paths: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(paths["layout"], "repository");
        assert_eq!(
            canonical(Path::new(paths["root"].as_str().unwrap())),
            canonical(manifest_dir.parent().unwrap())
        );
    } else {
        // A build elsewhere, as with CARGO_TARGET_DIR, needs --root.
        assert_eq!(output.status.code(), Some(1));
        assert!(text(&output.stderr).starts_with("error: cannot locate the NVX artifacts"));
    }
}

#[test]
fn a_relative_openvmm_override_is_relative_to_the_working_directory() {
    let root = Root::new();
    let output = Command::new(NVX_RS)
        .args(["--root", ".", "artifacts"])
        .current_dir(&root.path)
        .env("NVX_OPENVMM", Path::new("alt").join("openvmm"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    let paths: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(paths["root"], root.text());
    assert_eq!(
        paths["openvmm"],
        root.path.join("alt").join("openvmm").to_str().unwrap()
    );
}

#[test]
fn top_level_usage_errors_exit_with_status_2() {
    let usage = "usage: nvx-rs [-h] [--root PATH] [--prog PROG] {artifacts,egress,sandbox} ...\n";
    for (arguments, message) in [
        (
            &[][..],
            "nvx-rs: error: the following arguments are required: command\n",
        ),
        (
            &["--root"][..],
            "nvx-rs: error: argument --root: expected one argument\n",
        ),
        (
            &["build"][..],
            "nvx-rs: error: argument command: invalid choice: 'build' (choose from \
             'artifacts', 'egress', 'sandbox')\n",
        ),
    ] {
        let output = Command::new(NVX_RS).args(arguments).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert_eq!(
            text(&output.stderr),
            format!("{usage}{message}"),
            "{arguments:?}"
        );
    }
    let output = Command::new(NVX_RS)
        .args(["egress", "compile"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        text(&output.stderr),
        "usage: nvx-rs egress compile [-h] PATH\n\
         nvx-rs egress compile: error: the following arguments are required: PATH\n"
    );
}

/// What `nvx.py sandbox` printed for a refused `--workload-user` before it ran `nvx-rs`, as
/// CPython 3.14 prints it for 80 columns: the usage of its parser, wrapped after its name.
const NVX_PY_SANDBOX_USAGE_ERROR: &str = "\
usage: nvx.py sandbox [-h] [--layer ROLE,PATH,EROFS_UUID] [--scratch SCRATCH]
                      [--state-dir STATE_DIR] [--entrypoint ENTRYPOINT]
                      [--arg SANDBOX_ARG] [--hostname HOSTNAME]
                      [--workload-user UID:GID] [--memory-max MEMORY_MAX]
                      [--pids-max PIDS_MAX] [--memory-mib MEMORY_MIB]
                      [--timeout TIMEOUT] [--exec-timeout-ms EXEC_TIMEOUT_MS]
                      [--cwd CWD] [--environment KEY=VALUE]
                      [--environment-file PATH]
                      [--inherit-default-environment]
                      [--hypervisor {auto,whp,kvm,mshv}]
                      [--mount GUEST_TARGET,HOST_PATH[,ro|rw]]
                      [--mount-deny HOST_PATH] [--mount-allow HOST_PATH]
                      [--mount-write HOST_PATH] [--mount-owner {vmm,caller}]
                      [--net IPV4/PREFIX] [--network-profile {portable}]
                      [--network-egress {allow,deny}]
                      [--network-ingress {allow,deny}]
                      [--network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]]
                      [--network-egress-deny CIDR[:PROTOCOL[:PORT[-PORT]]]]
                      [--network-egress-policy-file PATH]
                      [--host-loopback {allow,deny}]
                      [--network-proxy IPV4:TCP-PORT]
                      [--host-loopback-forward HOST_LOOPBACK_FORWARD]
                      [--outcome-report OUTCOME_REPORT] [--cmdline CMDLINE]
                      [--dry-run]
                      [{run,provision,start,exec,stop,deprovision}]
nvx.py sandbox: error: argument --workload-user: --workload-user UID must be between 1 and \
4294967295
";

#[test]
fn messages_name_the_program_that_prog_names() {
    // nvx.py passes its own name, so that usage errors read as they did before it delegated.
    let output = Command::new(NVX_RS)
        .args(["--prog", "nvx.py", "sandbox", "--workload-user", "0:0"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(text(&output.stderr), NVX_PY_SANDBOX_USAGE_ERROR);
    assert!(output.stdout.is_empty());
    // The usage wraps for the name as argparse wraps it: a long name takes a line of its own.
    let long = "a-program-whose-name-is-long-enough-to-take-its-own-line.py";
    let output = Command::new(NVX_RS)
        .args(["--prog", long, "egress", "compile"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        text(&output.stderr),
        format!(
            "usage: {long} egress compile\n       [-h] PATH\n\
             {long} egress compile: error: the following arguments are required: PATH\n"
        )
    );
    // The top-level parser names it as soon as it has read it, and so does the help.
    let output = Command::new(NVX_RS)
        .args(["--prog=nvx.py", "build"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        text(&output.stderr),
        "usage: nvx.py [-h] [--root PATH] [--prog PROG] {artifacts,egress,sandbox} ...\n\
         nvx.py: error: argument command: invalid choice: 'build' (choose from 'artifacts', \
         'egress', 'sandbox')\n"
    );
    let output = Command::new(NVX_RS)
        .args(["--prog", "nvx.py", "artifacts", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(text(&output.stdout).starts_with("usage: nvx.py artifacts [-h]\n\nPrint the paths"));
}

#[cfg(unix)]
mod interrupts {
    use std::time::Instant;

    use super::*;

    /// Sends SIGINT once OpenVMM has started, to the CLI alone or, as a terminal's Ctrl-C does,
    /// to its whole process group, which a run's OpenVMM shares, and returns the CLI's output.
    fn interrupt(
        root: &Root,
        arguments: &[String],
        started: &Path,
        group: bool,
    ) -> (Output, Duration) {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new(NVX_RS);
        command
            .arg("--root")
            .arg(&root.path)
            .args(arguments)
            .current_dir(&root.path)
            .env_remove("NVX_OPENVMM")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if group {
            command.process_group(0);
        }
        let child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !started.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(200));
        let interrupted = Instant::now();
        let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
        if group {
            nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGINT).unwrap();
        } else {
            nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        (output, interrupted.elapsed())
    }

    /// Returns the arguments of a one-shot run of `/bin/sleep 60` in the fake OpenVMM, and the
    /// file that the fake writes when it starts.
    fn sleeping_run(root: &Root) -> (Vec<String>, PathBuf) {
        let path = |name: &str| root.path.join(name).to_str().unwrap().to_owned();
        let arguments: Vec<String> = [
            "sandbox",
            "run",
            "--layer",
            &format!(
                "distro,{},11111111-1111-1111-1111-111111111111",
                path("distro.erofs")
            ),
            "--scratch",
            &path("scratch.ext4"),
            "--entrypoint",
            "/bin/sleep",
            "--arg",
            "60",
        ]
        .map(str::to_owned)
        .to_vec();
        // The fake records its command line when it starts.
        (
            arguments,
            root.path.join("build").join("fake-openvmm-.json"),
        )
    }

    #[test]
    fn interrupting_a_run_kills_openvmm_and_exits_with_130() {
        let root = populated_root();
        let (arguments, started) = sleeping_run(&root);
        let (output, elapsed) = interrupt(&root, &arguments, &started, false);
        assert_eq!(output.status.code(), Some(130));
        assert_eq!(text(&output.stderr), "Interrupted\n");
        assert!(text(&output.stdout).starts_with(">> "));
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    }

    #[test]
    fn interrupting_a_run_ends_it_as_interrupted_even_if_openvmm_exits_first() {
        // A terminal's Ctrl-C reaches OpenVMM too, which the fake lets end it at once; Python's
        // wait raises KeyboardInterrupt all the same, rather than return OpenVMM's status.
        let root = populated_root();
        let (arguments, started) = sleeping_run(&root);
        let (output, elapsed) = interrupt(&root, &arguments, &started, true);
        assert_eq!(output.status.code(), Some(130));
        assert_eq!(text(&output.stderr), "Interrupted\n");
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    }

    #[test]
    fn interrupting_an_exec_leaves_the_sandbox_running() {
        let root = populated_root();
        let output = run(&root, &provision(&root, "state", ""));
        assert!(output.status.success(), "{}", text(&output.stderr));
        let output = run(&root, &lifecycle("start", &root, "state"));
        assert!(output.status.success(), "{}", text(&output.stderr));
        let mut arguments = lifecycle("exec", &root, "state");
        arguments.extend(
            [
                "--entrypoint",
                "/bin/sh",
                "--arg=-c",
                "--arg",
                "sleep 60000",
            ]
            .map(str::to_owned),
        );
        let started = root.path.join("state").join("runtime.json");
        let (output, elapsed) = interrupt(&root, &arguments, &started, false);
        assert_eq!(output.status.code(), Some(130));
        assert_eq!(text(&output.stderr), "Interrupted\n");
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
        for operation in ["stop", "deprovision"] {
            let output = run(&root, &lifecycle(operation, &root, "state"));
            assert!(
                output.status.success(),
                "{operation}: {}",
                text(&output.stderr)
            );
        }
    }
}
