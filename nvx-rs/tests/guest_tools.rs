//! Runs the guest binaries as programs: their command lines, exit statuses, and output.
//!
//! The tests run the binaries that Cargo builds for them, or, when `NVX_GUEST_BINARIES` names a
//! directory, the binaries there, such as the `guest`-profile builds that CI checks.
#![cfg(all(target_os = "linux", feature = "guest"))]

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};

/// Returns the path of the guest binary `name`.
fn binary(name: &str) -> PathBuf {
    if let Some(directory) = std::env::var_os("NVX_GUEST_BINARIES") {
        return Path::new(&directory).join(name);
    }
    PathBuf::from(match name {
        "nvx-console-pending" => env!("CARGO_BIN_EXE_nvx-console-pending"),
        "nvx-mmio-write" => env!("CARGO_BIN_EXE_nvx-mmio-write"),
        "nvx-reseed" => env!("CARGO_BIN_EXE_nvx-reseed"),
        "nvx-managed-agent" => env!("CARGO_BIN_EXE_nvx-managed-agent"),
        "nvx-ingress" => env!("CARGO_BIN_EXE_nvx-ingress"),
        "nvx-time" => env!("CARGO_BIN_EXE_nvx-time"),
        "nvx-time-probe" => env!("CARGO_BIN_EXE_nvx-time-probe"),
        "nvx-exit" => env!("CARGO_BIN_EXE_nvx-exit"),
        "nvx-device-io" => env!("CARGO_BIN_EXE_nvx-device-io"),
        _ => panic!("no guest binary {name}"),
    })
}

/// Held shared by `output_of` while it starts a child, and exclusively by `broken_pipe` while a
/// pipe's read end exists, which a child started meanwhile would keep open until it execs. Tests
/// therefore start every child through `output_of`.
static SPAWNING: RwLock<()> = RwLock::new(());

/// Runs `command`, spawned under `SPAWNING`, with a null stdin and a captured stderr, and
/// returns its output.
fn output_of(command: &mut Command) -> Output {
    let child = {
        let _spawning = SPAWNING.read().unwrap();
        command.stdin(Stdio::null()).stderr(Stdio::piped()).spawn()
    };
    child
        .and_then(Child::wait_with_output)
        .unwrap_or_else(|error| panic!("cannot run {command:?}: {error}"))
}

/// Runs the guest binary `name` with `arguments`, and returns its exit status, stdout, and
/// stderr.
fn run<I, S>(name: &str, arguments: I) -> (Option<i32>, Vec<u8>, Vec<u8>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let Output {
        status,
        stdout,
        stderr,
    } = output_of(
        Command::new(binary(name))
            .args(arguments)
            .stdout(Stdio::piped()),
    );
    (status.code(), stdout, stderr)
}

/// Runs the guest binary `name` with `arguments` as [`run`] does, but kills it and fails the
/// test once `timeout` passes. Its output must fit in the pipes, which nothing reads until it
/// exits.
fn run_within<I, S>(name: &str, arguments: I, timeout: Duration) -> (Option<i32>, Vec<u8>, Vec<u8>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_with_input(name, arguments, Stdio::null(), timeout)
}

/// Runs the guest binary `name` with `arguments` as [`run_within`] does, but with `stdin` as its
/// standard input.
fn run_with_input<I, S>(
    name: &str,
    arguments: I,
    stdin: Stdio,
    timeout: Duration,
) -> (Option<i32>, Vec<u8>, Vec<u8>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut child = {
        let _spawning = SPAWNING.read().unwrap();
        Command::new(binary(name))
            .args(arguments)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("cannot run {name}: {error}"))
    };
    let deadline = Instant::now() + timeout;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{name} did not end within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let Output {
        status,
        stdout,
        stderr,
    } = child.wait_with_output().unwrap();
    (status.code(), stdout, stderr)
}

/// Returns `parts` joined, with the bytes of a path where it occurs.
fn bytes(parts: &[&dyn AsRef<OsStr>]) -> Vec<u8> {
    parts
        .iter()
        .flat_map(|part| part.as_ref().as_bytes().to_vec())
        .collect()
}

/// Returns whether the tests run as root, with which the binaries would reach the host's real
/// devices.
fn is_root() -> bool {
    fs::metadata("/proc/self").map_or(true, |metadata| metadata.uid() == 0)
}

/// Returns the `errno` with which this process fails to open the device at `path`, as an
/// unprivileged guest binary does, or `None` when the tests run as root or the device opens: the
/// binaries would then reach the host's real devices.
fn device_error(path: &str, read: bool, write: bool) -> Option<Errno> {
    if is_root() {
        return None;
    }
    fs::File::options()
        .read(read)
        .write(write)
        .open(path)
        .err()
        .map(|error| Errno::from_raw(error.raw_os_error().expect("an OS error")))
}

#[test]
fn console_pending_usage() {
    for arguments in [&[][..], &["a", "b"]] {
        assert_eq!(
            run("nvx-console-pending", arguments),
            (
                Some(2),
                Vec::new(),
                b"usage: nvx-console-pending DEVICE\n".to_vec()
            )
        );
    }
}

/// Runs the guest binary `name` with `arguments` and its stdout on `stdout`, and returns how it
/// ended and its stderr.
fn run_to<I, S>(
    name: &str,
    arguments: I,
    stdout: impl Into<std::process::Stdio>,
) -> (std::process::ExitStatus, Vec<u8>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = output_of(Command::new(binary(name)).args(arguments).stdout(stdout));
    (output.status, output.stderr)
}

/// Runs a guest binary after a shell sets SIGPIPE to `SIG_IGN`, preserving that disposition
/// across `exec`.
fn run_to_ignoring_sigpipe<I, S>(
    name: &str,
    arguments: I,
    stdout: impl Into<std::process::Stdio>,
) -> (std::process::ExitStatus, Vec<u8>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = output_of(
        Command::new("sh")
            .args(["-c", "trap '' PIPE; exec \"$@\"", "sh"])
            .arg(binary(name))
            .args(arguments)
            .stdout(stdout),
    );
    (output.status, output.stderr)
}

/// Returns a pipe whose reader is closed, so that a write to it raises `SIGPIPE`.
fn broken_pipe() -> io::PipeWriter {
    let _creating = SPAWNING.write().unwrap();
    // The wildcard binds nothing, so the reader closes at the end of the statement.
    let (_, writer) = io::pipe().unwrap();
    writer
}

#[test]
fn console_pending_handles_failed_writes_as_c_stdio() {
    use std::os::unix::process::ExitStatusExt;

    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("console");
    fs::write(&file, b"pending").unwrap();
    // C's stdio buffers a file's stdout until exit, which ignores the failure.
    let (status, stderr) = run_to(
        "nvx-console-pending",
        [&file],
        fs::File::create("/dev/full").unwrap(),
    );
    assert_eq!((status.code(), stderr), (Some(0), Vec::new()));
    // The default SIGPIPE disposition ends the program at a closed pipe.
    let (status, stderr) = run_to("nvx-console-pending", [&file], broken_pipe());
    assert_eq!((status.signal(), stderr), (Some(13), Vec::new()));
    // An ignored disposition survives exec, so the buffered nonterminal failure is dropped.
    let (status, stderr) = run_to_ignoring_sigpipe("nvx-console-pending", [&file], broken_pipe());
    assert_eq!((status.code(), stderr), (Some(0), Vec::new()));
}

#[test]
fn console_pending_reports_the_unread_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("console");
    fs::write(&file, b"pending").unwrap();
    assert_eq!(
        run("nvx-console-pending", [&file]),
        (Some(0), b"7\n".to_vec(), Vec::new())
    );

    let missing = directory.path().join("missing");
    let message = bytes(&[
        &"nvx-console-pending: open ",
        &missing,
        &format!(": {}\n", Errno::ENOENT.desc()),
    ]);
    assert_eq!(
        run("nvx-console-pending", [&missing]),
        (Some(1), Vec::new(), message)
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn mmio_write_usage_and_page_boundary() {
    let usage = b"usage: nvx-mmio-write ADDRESS VALUE\n".to_vec();
    for arguments in [
        &[][..],
        &["0x1000"],
        &["0x1000", "1", "2"],
        &["0x", "1"],
        &["0x1000", "4294967296"],
        &["0x1000", "-1"],
    ] {
        assert_eq!(
            run("nvx-mmio-write", arguments),
            (Some(2), Vec::new(), usage.clone()),
            "{arguments:?}"
        );
    }
    // Guests have 4 KiB pages.
    assert_eq!(
        run("nvx-mmio-write", ["0xffd", "1"]),
        (
            Some(1),
            Vec::new(),
            b"MMIO write crosses a page boundary\n".to_vec()
        )
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn mmio_write_reports_an_inaccessible_dev_mem() {
    let Some(errno) = device_error("/dev/mem", true, true) else {
        return;
    };
    assert_eq!(
        run("nvx-mmio-write", ["0x1000", "1"]),
        (
            Some(1),
            Vec::new(),
            format!("open /dev/mem: {}\n", errno.desc()).into_bytes()
        )
    );
}

#[test]
fn reseed_usage_and_sample() {
    let usage = b"usage: nvx-reseed --sample | [--generation-only] SEED_FILE \
[PREVIOUS_GENERATION_ID]\n";
    for arguments in [&[][..], &["a", "b", "c"]] {
        assert_eq!(
            run("nvx-reseed", arguments),
            (Some(2), Vec::new(), usage.to_vec())
        );
    }
    let (status, sample, err) = run("nvx-reseed", ["--sample"]);
    assert_eq!((status, err), (Some(0), Vec::new()));
    let (digits, newline) = sample.split_at(64);
    assert!(digits.iter().all(u8::is_ascii_hexdigit), "{sample:?}");
    assert_eq!(newline, b"\n");
    // C's stdio buffers a file's stdout until exit, which ignores the failure.
    let (status, stderr) = run_to(
        "nvx-reseed",
        ["--sample"],
        fs::File::create("/dev/full").unwrap(),
    );
    assert_eq!((status.code(), stderr), (Some(0), Vec::new()));
}

#[test]
fn reseed_checks_the_generation_id() {
    let directory = tempfile::tempdir().unwrap();
    let seed = directory.path().join("seed");
    fs::write(&seed, [0x5a; 64]).unwrap();
    let previous = "00000000000000000000000000000000";
    assert_eq!(
        run(
            "nvx-reseed",
            [
                OsStr::new("--generation-only"),
                seed.as_os_str(),
                OsStr::new(previous)
            ]
        ),
        (
            Some(0),
            format!("{}\n", "5a".repeat(16)).into_bytes(),
            Vec::new()
        )
    );
    assert_eq!(
        run(
            "nvx-reseed",
            [seed.as_os_str(), OsStr::new(&"5A".repeat(16))]
        ),
        (
            Some(1),
            Vec::new(),
            b"nvx-reseed: VM generation ID did not change\n".to_vec()
        )
    );
}

#[test]
fn reseed_needs_privileges_to_reseed() {
    if is_root() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let seed = directory.path().join("seed");
    fs::write(&seed, [0x5a; 64]).unwrap();
    // Anyone may open /dev/random for writing, but RNDADDENTROPY takes CAP_SYS_ADMIN.
    assert_eq!(
        run("nvx-reseed", [&seed]),
        (
            Some(1),
            Vec::new(),
            format!("nvx-reseed: random ioctl: {}\n", Errno::EPERM.desc()).into_bytes()
        )
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn exit_fails_silently_without_dev_port() {
    if device_error("/dev/port", false, true).is_none() {
        return;
    }
    for arguments in [&["0"][..], &[], &["256", "extra"]] {
        assert_eq!(
            run("nvx-exit", arguments),
            (Some(1), Vec::new(), Vec::new()),
            "{arguments:?}"
        );
    }
}

/// Runs `nvx-device-io` with `arguments`, checks that it exits with `expected` and writes nothing
/// to stderr, and returns its result line, which must be the only output.
#[cfg(target_arch = "x86_64")]
fn device_io<I, S>(arguments: I, expected: i32) -> Option<serde_json::Value>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (result, progress) = device_io_with_progress(arguments, expected);
    assert!(progress.is_empty(), "expected no stderr, got {progress:?}");
    result
}

/// Runs `nvx-device-io` with `arguments`, checks that it exits with `expected`, and returns its
/// result line, which must be its only standard output, and the progress lines of its stderr.
#[cfg(target_arch = "x86_64")]
fn device_io_with_progress<I, S>(
    arguments: I,
    expected: i32,
) -> (Option<serde_json::Value>, Vec<String>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let Output {
        status,
        stdout,
        stderr,
    } = output_of(
        Command::new(binary("nvx-device-io"))
            .args(arguments)
            .stdout(Stdio::piped()),
    );
    let progress = String::from_utf8(stderr).unwrap();
    assert!(
        status.code() == Some(expected),
        "expected exit status {expected}, got {status}, stdout {:?}, stderr {progress:?}",
        String::from_utf8_lossy(&stdout),
    );
    let progress = progress.lines().map(str::to_owned).collect();
    if stdout.is_empty() {
        return (None, progress);
    }
    let line = String::from_utf8(stdout).unwrap();
    let keys = [
        r#"{"device":""#,
        r#"","operation":""#,
        r#"","operations":"#,
        r#","bytes_per_operation":"#,
        r#","elapsed_ns":"#,
        "}\n",
    ];
    let mut rest = line.as_str();
    for key in keys {
        let position = rest.find(key).unwrap_or_else(|| panic!("{key} in {line}"));
        rest = &rest[position + key.len()..];
    }
    assert_eq!(rest, "", "{line}");
    (Some(serde_json::from_str(&line).unwrap()), progress)
}

#[test]
#[cfg(target_arch = "x86_64")]
fn device_io_usage() {
    for arguments in [
        &[][..],
        &["file"],
        &["disk", "x"],
        &["file", "f", "read", "0", "4096", "buffered", "create"],
        &["file", "f", "read", "1", "4095", "buffered", "create"],
        &["file", "f", "copy", "1", "4096", "buffered", "create"],
        &["file", "f", "read", "1", "4096", "cached", "create"],
        &[
            "file", "f", "read", "1", "4096", "buffered", "create", "extra",
        ],
        &["network", "127.0.0.1", "0", "1"],
        &["network", "127.0.0.1", "65536", "1"],
        &["network", "127.0.0.256", "1", "1"],
        &["network", "127.0.0.1", "1", "0"],
        &["map"],
        &["map", "f"],
        &["map", "f", ""],
        &["map", "f", "NVX", "extra"],
    ] {
        assert_eq!(device_io(arguments, 2), None, "{arguments:?}");
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn device_io_measures_a_file() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("disk");
    let arguments = |operation: &str, creation: &str| {
        let path = file.to_str().unwrap();
        ["file", path, operation, "50", "65536", "buffered", creation].map(str::to_owned)
    };
    for (operation, creation) in [("write", "create"), ("read", "existing")] {
        let (result, progress) = device_io_with_progress(arguments(operation, creation), 0);
        let result = result.expect("a result line");
        assert_eq!(result["device"], "file");
        assert_eq!(result["operation"], operation);
        assert_eq!(result["bytes_per_operation"], 4096);
        assert!(result["operations"].as_u64().unwrap() > 0, "{result}");
        assert!(
            result["elapsed_ns"].as_u64().unwrap() >= 50_000_000,
            "{result}"
        );
        // The progress lines name each phase, and only a write flushes.
        let mut expected = vec![
            "nvx-device-io: opened".to_owned(),
            format!("nvx-device-io: transferred {} blocks", result["operations"]),
        ];
        if operation == "write" {
            expected.push("nvx-device-io: flushed".to_owned());
        }
        assert_eq!(progress, expected);
    }
    assert_eq!(fs::metadata(&file).unwrap().len(), 65536);
    let contents = fs::read(&file).unwrap();
    let pattern: Vec<u8> = (0..4096_usize)
        .map(|index| (index * 31 + 17) as u8)
        .collect();
    // The first offset of a 64 KiB file is its first block.
    assert_eq!(&contents[..4096], pattern);

    // A missing file cannot open, and an empty one opens but cannot transfer a block.
    let missing = directory.path().join("missing");
    let mut arguments = arguments("read", "existing");
    arguments[1] = missing.to_str().unwrap().to_owned();
    assert_eq!(device_io(&arguments, 3), None);
    fs::write(&missing, b"").unwrap();
    arguments[4] = "4096".to_owned();
    assert_eq!(
        device_io_with_progress(&arguments, 4),
        (None, vec!["nvx-device-io: opened".to_owned()])
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn device_io_holds_a_mapped_write_until_its_input_ends() {
    use std::io::BufRead;

    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("mapped");
    fs::write(&file, vec![b'.'; 4096]).unwrap();
    let mut child = {
        let _spawning = SPAWNING.read().unwrap();
        Command::new(binary("nvx-device-io"))
            .args(["map", file.to_str().unwrap(), "NVX-MAPPED"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut line = String::new();
    io::BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(result["device"], "file", "{line}");
    assert_eq!(result["operation"], "map", "{line}");
    assert_eq!(result["operations"], 1, "{line}");
    assert_eq!(result["bytes_per_operation"], 10, "{line}");
    // The program prints once it has written, and then holds the mapping until its input ends.
    assert!(child.try_wait().unwrap().is_none());
    assert!(fs::read(&file).unwrap().starts_with(b"NVX-MAPPED."));
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        (output.status.code(), output.stdout, output.stderr),
        (Some(0), Vec::new(), Vec::new())
    );
    let contents = fs::read(&file).unwrap();
    assert_eq!(contents.len(), 4096);
    assert!(contents.starts_with(b"NVX-MAPPED."));

    // A missing file cannot map, and a file shorter than the text cannot take it.
    let missing = directory.path().join("missing");
    assert_eq!(
        device_io(["map", missing.to_str().unwrap(), "NVX"], 3),
        None
    );
    fs::write(&missing, b"NV").unwrap();
    assert_eq!(
        device_io(["map", missing.to_str().unwrap(), "NVX"], 4),
        None
    );
    assert_eq!(fs::read(&missing).unwrap(), b"NV");
}

#[test]
#[cfg(target_arch = "x86_64")]
fn device_io_measures_a_network_round_trip() {
    use std::net::UdpSocket;
    use std::thread;

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    // Echoes datagrams until the test sends an empty one after the benchmark exits, and returns
    // the first.
    let echo = thread::spawn(move || {
        let mut buffer = [0; 128];
        let mut first = None;
        loop {
            let (length, peer) = match socket.recv_from(&mut buffer) {
                Ok(received) => received,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("the echo cannot receive: {error}"),
            };
            if length == 0 {
                return first;
            }
            if first.is_none() {
                first = Some(buffer[..length].to_vec());
            }
            socket.send_to(&buffer[..length], peer).unwrap();
        }
    });
    // On a heavily loaded host, the echo thread can wait longer for a processor than a shorter
    // benchmark runs.
    let port = address.port().to_string();
    let result = device_io(["network", "127.0.0.1", &port, "1000"], 0).expect("a result line");
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .send_to(&[], address)
        .unwrap();
    let first = echo.join().unwrap();
    assert_eq!(result["device"], "network");
    assert_eq!(result["operation"], "roundtrip");
    assert_eq!(result["bytes_per_operation"], 64);
    assert!(result["operations"].as_u64().unwrap() > 0, "{result}");
    assert!(
        result["elapsed_ns"].as_u64().unwrap() >= 1_000_000_000,
        "{result}"
    );
    let payload: Vec<u8> = (1..=64).collect();
    assert_eq!(first, Some(payload));
}

#[test]
#[cfg(target_arch = "x86_64")]
fn device_io_exits_255_when_it_cannot_print() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("disk");
    let output = output_of(
        Command::new(binary("nvx-device-io"))
            .args([
                "file",
                file.to_str().unwrap(),
                "write",
                "1",
                "4096",
                "buffered",
            ])
            .arg("create")
            .stdout(fs::File::create("/dev/full").unwrap()),
    );
    // The progress lines precede the result that cannot print.
    assert_eq!(output.status.code(), Some(255));
    assert_progress(&output.stderr, true);
    // A closed pipe raises SIGPIPE, whose default disposition the program keeps.
    use std::os::unix::process::ExitStatusExt;
    let path = file.to_str().unwrap();
    let arguments = ["file", path, "read", "1", "4096", "buffered", "existing"];
    let (status, stderr) = run_to("nvx-device-io", arguments, broken_pipe());
    assert_eq!(status.signal(), Some(13));
    assert_progress(&stderr, false);
    // With inherited SIG_IGN, write reports EPIPE and the tool returns its output-failure status.
    let (status, stderr) = run_to_ignoring_sigpipe("nvx-device-io", arguments, broken_pipe());
    assert_eq!(status.code(), Some(255));
    assert_progress(&stderr, false);
}

#[test]
#[cfg(target_arch = "x86_64")]
fn device_io_ignores_a_closed_stderr() {
    // The progress lines are diagnostics: a stderr whose reader is gone raises no SIGPIPE, even
    // under the default disposition, which ends the program at a closed stdout.
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("disk");
    let path = file.to_str().unwrap();
    let stderr = broken_pipe();
    let child = {
        let _spawning = SPAWNING.read().unwrap();
        Command::new(binary("nvx-device-io"))
            .args(["file", path, "write", "1", "4096", "buffered", "create"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap()
    };
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.status);
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["device"], "file");
    assert_eq!(result["operation"], "write");
    assert!(result["operations"].as_u64().unwrap() > 0, "{result}");
}

/// Checks that `stderr` holds the progress lines of a completed file benchmark, with a flush when
/// it `wrote`.
#[cfg(target_arch = "x86_64")]
fn assert_progress(stderr: &[u8], wrote: bool) {
    let progress = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = progress.lines().collect();
    assert_eq!(lines.len(), if wrote { 3 } else { 2 }, "{progress:?}");
    assert_eq!(lines[0], "nvx-device-io: opened", "{progress:?}");
    let blocks = lines[1]
        .strip_prefix("nvx-device-io: transferred ")
        .and_then(|rest| rest.strip_suffix(" blocks"))
        .unwrap_or_else(|| panic!("{progress:?}"));
    assert!(blocks.parse::<u64>().unwrap() > 0, "{progress:?}");
    if wrote {
        assert_eq!(lines[2], "nvx-device-io: flushed", "{progress:?}");
    }
}

/// Lays out the direct agent's private helper configuration: the little-endian flags and entry
/// count, then each environment entry after its little-endian length.
fn helper_config(flags: u16, environment: &[&[u8]]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.extend_from_slice(&(environment.len() as u16).to_le_bytes());
    for entry in environment {
        bytes.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        bytes.extend_from_slice(entry);
    }
    bytes
}
fn run_agent_helper(
    config: &[u8],
    workload: &[&OsStr],
    directory: Option<&Path>,
    environment: &[(&str, &str)],
) -> Output {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(config).unwrap();
    file.rewind().unwrap();
    let fd = file.as_raw_fd();
    // The helper inherits the descriptor, and closes it before it executes the workload.
    fcntl(&file, FcntlArg::F_SETFD(FdFlag::empty())).unwrap();
    let mut command = Command::new(binary("nvx-managed-agent"));
    command
        .arg("--exec-config-fd")
        .arg(fd.to_string())
        .arg("--")
        .args(workload)
        .env_clear();
    for (name, value) in environment {
        command.env(name, value);
    }
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    output_of(command.stdout(Stdio::piped()))
}

/// Returns `stdout`, the environment that `/usr/bin/env` printed, without the variable that the
/// LLVM profiling runtime of an instrumented build sets in the environment of its own process when
/// it starts, such as the helper's, whose workload then inherits it.
fn without_profiling_runtime(stdout: Vec<u8>) -> Vec<u8> {
    stdout
        .split_inclusive(|&byte| byte == b'\n')
        .filter(|line| !line.starts_with(b"__LLVM_PROFILE_RT_INIT_ONCE="))
        .flatten()
        .copied()
        .collect()
}

#[test]
fn managed_agent_helper_applies_inherited_exact_and_layered_environments() {
    const ENVIRONMENT_PRESENT: u16 = 2;
    const INHERIT_DEFAULT_ENV: u16 = 4;
    let env = OsStr::new("/usr/bin/env");

    let output = run_agent_helper(
        &helper_config(0, &[]),
        &[env],
        None,
        &[("BASE", "inherited")],
    );
    assert_eq!(
        (
            output.status.code(),
            without_profiling_runtime(output.stdout),
            output.stderr
        ),
        (Some(0), b"BASE=inherited\n".to_vec(), Vec::new())
    );

    let output = run_agent_helper(
        &helper_config(ENVIRONMENT_PRESENT, &[]),
        &[env],
        None,
        &[("BASE", "inherited")],
    );
    assert_eq!(
        (output.status.code(), output.stdout, output.stderr),
        (Some(0), Vec::new(), Vec::new())
    );

    let output = run_agent_helper(
        &helper_config(
            ENVIRONMENT_PRESENT | INHERIT_DEFAULT_ENV,
            &[b"VALUE=layered", b"BASE=replaced"],
        ),
        &[env],
        None,
        &[("BASE", "inherited")],
    );
    assert_eq!(
        (
            output.status.code(),
            without_profiling_runtime(output.stdout),
            output.stderr
        ),
        (
            Some(0),
            b"BASE=replaced\nVALUE=layered\n".to_vec(),
            Vec::new()
        )
    );

    let output = run_agent_helper(
        &helper_config(ENVIRONMENT_PRESENT, &[b"PWD=/caller"]),
        &[env],
        None,
        &[("PWD", "/inherited")],
    );
    assert_eq!(
        (output.status.code(), output.stdout, output.stderr),
        (Some(0), b"PWD=/caller\n".to_vec(), Vec::new())
    );
}

#[test]
fn managed_agent_helper_accepts_large_exact_environment() {
    const ENVIRONMENT_PRESENT: u16 = 2;
    let entries: Vec<Vec<u8>> = (0..12)
        .map(|index| {
            let mut entry = format!("KEY{index}=").into_bytes();
            entry.extend(std::iter::repeat_n(b'x', 4000));
            entry
        })
        .collect();
    let references: Vec<&[u8]> = entries.iter().map(Vec::as_slice).collect();
    let output = run_agent_helper(
        &helper_config(ENVIRONMENT_PRESENT, &references),
        &[OsStr::new("/usr/bin/env")],
        None,
        &[],
    );
    assert_eq!((output.status.code(), output.stderr), (Some(0), Vec::new()));
    let expected: Vec<u8> = entries
        .iter()
        .flat_map(|entry| entry.iter().copied().chain(std::iter::once(b'\n')))
        .collect();
    assert_eq!(output.stdout, expected);
}

#[test]
fn managed_agent_helper_defensively_rejects_bad_configuration() {
    const ENVIRONMENT_PRESENT: u16 = 2;
    const INHERIT_DEFAULT_ENV: u16 = 4;
    // The helper enters no working directory, so the former flags of one are unknown.
    const RETIRED_CWD_PRESENT: u16 = 1;
    const RETIRED_CWD_ENTERED: u16 = 0x8000;
    let workload = [OsStr::new("/bin/pwd")];
    for config in [
        helper_config(RETIRED_CWD_PRESENT, &[]),
        helper_config(RETIRED_CWD_ENTERED, &[]),
        helper_config(INHERIT_DEFAULT_ENV, &[]),
        helper_config(ENVIRONMENT_PRESENT, &[b"NO_EQUALS"]),
        helper_config(ENVIRONMENT_PRESENT, &[b"=value"]),
        helper_config(ENVIRONMENT_PRESENT, &[b"KEY=embedded\0nul"]),
    ] {
        let output = run_agent_helper(&config, &workload, None, &[]);
        assert_eq!(output.status.code(), Some(125), "{config:?}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn managed_agent_helper_runs_the_workload_in_its_own_directory() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_agent_helper(
        &helper_config(0, &[]),
        &[OsStr::new("/bin/pwd")],
        Some(directory.path()),
        &[("BASE", "inherited")],
    );
    let expected = [directory.path().as_os_str().as_bytes(), b"\n"].concat();
    assert_eq!(
        (output.status.code(), output.stdout, output.stderr),
        (Some(0), expected, Vec::new())
    );
}

#[test]
fn managed_agent_helper_closes_its_internal_descriptor() {
    let script = OsStr::new(r#"test ! -e "/proc/self/fd/$1""#);
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&helper_config(0, &[])).unwrap();
    file.rewind().unwrap();
    let fd = file.as_raw_fd();
    // The helper deliberately inherits this descriptor.
    fcntl(&file, FcntlArg::F_SETFD(FdFlag::empty())).unwrap();
    let output = output_of(
        Command::new(binary("nvx-managed-agent"))
            .args([
                OsStr::new("--exec-config-fd"),
                OsStr::new(&fd.to_string()),
                OsStr::new("--"),
                OsStr::new("/bin/sh"),
                OsStr::new("-c"),
                script,
                OsStr::new("sh"),
                OsStr::new(&fd.to_string()),
            ])
            .stdout(Stdio::piped()),
    );
    assert_eq!(
        (output.status.code(), output.stdout, output.stderr),
        (Some(0), Vec::new(), Vec::new())
    );
}
#[test]
fn managed_agent_bad_helper_syntax_exits_125() {
    for arguments in [
        &[][..],
        &["--exec-config-fd"],
        &["--exec-config-fd", "-1", "--", "/bin/true"],
        &["--exec-config-fd", "0", "--", "bin/true"],
    ] {
        assert_eq!(
            run("nvx-managed-agent", arguments).0,
            Some(125),
            "{arguments:?}"
        );
    }
}

/// The header line of the kernel's socket tables, which `nvx-ingress` skips.
const INGRESS_TABLE_HEADER: &str =
    "  sl  local_address rem_address st tx rx tr tm retr uid timeout inode\n";

/// The conformance vectors of the listener announcement, whose line format
/// `tests/vectors/README.md` describes.
const NVXL_V1: &str = include_str!("vectors/nvxl-v1.txt");

/// Returns the line that `nvx-ingress test` prints for a scan at `time` whose announcement is the
/// conformance vector `name`: the time and the vector's bytes in hexadecimal.
fn ingress_line(time: u64, name: &str) -> Vec<u8> {
    let bytes = NVXL_V1
        .lines()
        .filter_map(|line| {
            line.strip_prefix("ANNOUNCEMENT|")?
                .strip_prefix(name)?
                .strip_prefix('|')
        })
        .find_map(|rest| rest.split('|').next())
        .unwrap_or_else(|| panic!("nvxl-v1.txt has no announcement named {name}"));
    format!("{time} {bytes}\n").into_bytes()
}

/// Renders `address` as the kernel's socket tables do: each 32-bit word in hexadecimal, as it
/// lies in memory.
fn ingress_kernel_address(address: std::net::IpAddr) -> String {
    let octets = match address {
        std::net::IpAddr::V4(address) => address.octets().to_vec(),
        std::net::IpAddr::V6(address) => address.octets().to_vec(),
    };
    octets
        .chunks_exact(4)
        .map(|word| format!("{:08X}", u32::from_ne_bytes(word.try_into().unwrap())))
        .collect()
}

/// Returns row `index` of a socket table for an unconnected socket bound to `address` and `port`,
/// in `state`, with `inode`.
fn ingress_row(index: usize, address: &str, port: u16, state: u8, inode: u64) -> String {
    let address: std::net::IpAddr = address.parse().unwrap();
    let remote: std::net::IpAddr = match address {
        std::net::IpAddr::V4(_) => "0.0.0.0".parse().unwrap(),
        std::net::IpAddr::V6(_) => "::".parse().unwrap(),
    };
    format!(
        "{index:4}: {}:{port:04X} {}:0000 {state:02X} \
         00000000:00000000 00:00000000 00000000 0 0 {inode} \
         2 0000000000000000\n",
        ingress_kernel_address(address),
        ingress_kernel_address(remote),
    )
}

/// Writes snapshot `index` under `directory` for `nvx-ingress test`: its scan `time`, and the
/// four socket tables, each with the `rows` named for it.
fn write_ingress_snapshot(directory: &Path, index: u32, time: &[u8], rows: &[(&str, String)]) {
    let root = directory.join(index.to_string());
    fs::create_dir_all(root.join("net")).unwrap();
    fs::write(root.join("time"), time).unwrap();
    for table in ["tcp", "tcp6", "udp", "udp6"] {
        let mut contents = INGRESS_TABLE_HEADER.to_owned();
        for (_, row) in rows.iter().filter(|(name, _)| *name == table) {
            contents.push_str(row);
        }
        fs::write(root.join("net").join(table), contents).unwrap();
    }
}

#[test]
fn ingress_test_mode_prints_the_conformance_vector_from_prebuilt_binaries() {
    let directory = tempfile::tempdir().unwrap();
    write_ingress_snapshot(
        directory.path(),
        0,
        b"0\n",
        &[
            ("tcp", ingress_row(0, "0.0.0.0", 8080, 0x0a, 1)),
            ("tcp6", ingress_row(1, "fd00::a00:2", 8443, 0x0a, 2)),
        ],
    );
    assert_eq!(
        run(
            "nvx-ingress",
            [OsStr::new("test"), directory.path().as_os_str()]
        ),
        (Some(0), ingress_line(0, "TWO_LISTENERS"), Vec::new())
    );
}

#[test]
fn ingress_test_mode_preserves_udp_settling_between_snapshots() {
    let directory = tempfile::tempdir().unwrap();
    let rows = [("udp", ingress_row(0, "0.0.0.0", 7201, 0x07, 10))];
    write_ingress_snapshot(directory.path(), 0, b"0\n", &rows);
    write_ingress_snapshot(directory.path(), 1, b"1000\n", &rows);
    assert_eq!(
        run(
            "nvx-ingress",
            [OsStr::new("test"), directory.path().as_os_str()]
        ),
        (
            Some(0),
            [ingress_line(0, "NONE"), ingress_line(1000, "UDP")].concat(),
            Vec::new(),
        )
    );
}

#[test]
fn ingress_rejects_bad_invocations_and_test_snapshots() {
    let usage = b"usage: nvx-ingress [inherit] GATEWAY_IPV4\n".to_vec();
    for arguments in [
        &[][..],
        &["not-an-ipv4-address"],
        &["inherit"],
        &["inherit", "not-an-ipv4-address"],
        &["inherit", "127.0.0.1", "extra"],
    ] {
        assert_eq!(
            run("nvx-ingress", arguments),
            (Some(2), Vec::new(), usage.clone()),
            "{arguments:?}"
        );
    }

    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        run(
            "nvx-ingress",
            [OsStr::new("test"), directory.path().as_os_str()]
        ),
        (Some(1), Vec::new(), Vec::new())
    );
    write_ingress_snapshot(directory.path(), 0, b"bad\n", &[]);
    assert_eq!(
        run(
            "nvx-ingress",
            [OsStr::new("test"), directory.path().as_os_str()]
        ),
        (Some(1), Vec::new(), Vec::new())
    );
    write_ingress_snapshot(directory.path(), 0, b"7\n", &[]);
    write_ingress_snapshot(directory.path(), 1, b"bad\n", &[]);
    assert_eq!(
        run(
            "nvx-ingress",
            [OsStr::new("test"), directory.path().as_os_str()]
        ),
        (Some(1), ingress_line(7, "NONE"), Vec::new())
    );

    let overlong = std::ffi::OsString::from_vec(vec![b'a'; 4094]);
    assert_eq!(
        run("nvx-ingress", [OsStr::new("test"), &overlong]),
        (Some(1), Vec::new(), Vec::new())
    );
}

#[test]
fn ingress_inherits_only_an_ipv4_datagram_socket_bound_to_a_privileged_source_port() {
    // The announcer that a clone starts announces from the socket that it inherits as its
    // standard input, which only root can bind to a privileged source port, from which alone the
    // gateway takes announcements: it refuses any other at once, before it scans anything.
    let refused = |stdin: Stdio, expected: String| {
        assert_eq!(
            run_with_input(
                "nvx-ingress",
                ["inherit", "127.0.0.1"],
                stdin,
                Duration::from_secs(10)
            ),
            (Some(1), Vec::new(), expected.into_bytes())
        );
    };
    let unprivileged = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = unprivileged.local_addr().unwrap().port();
    assert!(port >= 1024, "{port}");
    refused(
        Stdio::from(std::os::fd::OwnedFd::from(unprivileged)),
        format!(
            "nvx-ingress: the inherited socket's port {port} is not a privileged source port\n"
        ),
    );
    let unbound = nix::sys::socket::socket(
        nix::sys::socket::AddressFamily::Inet,
        nix::sys::socket::SockType::Datagram,
        nix::sys::socket::SockFlag::empty(),
        None,
    )
    .unwrap();
    refused(
        Stdio::from(unbound),
        "nvx-ingress: the inherited socket's port 0 is not a privileged source port\n".to_owned(),
    );
    let stream = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    refused(
        Stdio::from(std::os::fd::OwnedFd::from(stream)),
        format!(
            "nvx-ingress: inherited socket: {}\n",
            Errno::EPROTOTYPE.desc()
        ),
    );
    refused(
        Stdio::null(),
        format!(
            "nvx-ingress: inherited socket: {}\n",
            Errno::ENOTSOCK.desc()
        ),
    );
}

#[cfg(target_arch = "x86_64")]
/// Returns the first CPU that this process may run on.
fn first_allowed_cpu() -> usize {
    let affinity = nix::sched::sched_getaffinity(nix::unistd::Pid::from_raw(0)).unwrap();
    (0..nix::sched::CpuSet::count())
        .find(|&cpu| affinity.is_set(cpu).unwrap())
        .unwrap()
}

#[cfg(target_arch = "x86_64")]
/// Returns the CPUs in the time ABI's mask that this process may run on.
fn allowed_time_cpus() -> Vec<usize> {
    let affinity = nix::sched::sched_getaffinity(nix::unistd::Pid::from_raw(0)).unwrap();
    (0..64)
        .filter(|&cpu| affinity.is_set(cpu).unwrap())
        .collect()
}

#[cfg(target_arch = "x86_64")]
/// Returns the CPUs that this process may not run on, of the 64 that the time ABI covers.
fn unusable_cpus() -> Vec<usize> {
    let affinity = nix::sched::sched_getaffinity(nix::unistd::Pid::from_raw(0)).unwrap();
    (0..64)
        .filter(|&cpu| !affinity.is_set(cpu).unwrap())
        .collect()
}

#[cfg(target_arch = "x86_64")]
/// Returns whether this process may switch itself from `SCHED_IDLE` to `SCHED_OTHER`.
fn can_leave_idle_priority() -> bool {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let capabilities = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
        .unwrap_or(0);
    if capabilities & (1 << 23) != 0 {
        return true;
    }
    let limits = fs::read_to_string("/proc/self/limits").unwrap();
    let nice = limits.lines().find_map(|line| {
        line.strip_prefix("Max nice priority")?
            .split_ascii_whitespace()
            .next()
    });
    nice.is_some_and(|value| value == "unlimited" || value.parse::<u64>().is_ok_and(|n| n >= 20))
}

#[cfg(target_arch = "x86_64")]
const TIME_UTC_NS: u64 = 1_790_841_600_123_456_789;

#[cfg(target_arch = "x86_64")]
fn time_packet(
    flags: u8,
    online: u8,
    ranges: &[(u64, u64)],
    generation: u32,
    rate_deviation: i32,
    downtime_ns: u64,
) -> Vec<u8> {
    let mut packet = Vec::with_capacity(32 + ranges.len() * 16 + 64);
    packet.extend_from_slice(b"OVR");
    packet.extend_from_slice(&[4, flags, online, ranges.len() as u8, 0]);
    packet.extend_from_slice(&generation.to_le_bytes());
    packet.extend_from_slice(&rate_deviation.to_le_bytes());
    packet.extend_from_slice(&downtime_ns.to_le_bytes());
    packet.extend_from_slice(&TIME_UTC_NS.to_le_bytes());
    for &(start, length) in ranges {
        packet.extend_from_slice(&start.to_le_bytes());
        packet.extend_from_slice(&length.to_le_bytes());
    }
    packet.extend(0_u8..64);
    packet
}

#[cfg(target_arch = "x86_64")]
fn time_fixture(directory: &Path, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, contents).unwrap();
    path
}

/// Runs the `nvx-time` test hook `hook` on the fixture at `fixture`, with `arguments` after it,
/// as [`run`] does.
#[cfg(target_arch = "x86_64")]
fn time_hook(hook: &str, fixture: &Path, arguments: &[&str]) -> (Option<i32>, Vec<u8>, Vec<u8>) {
    let hook = [OsStr::new("test"), OsStr::new(hook), fixture.as_os_str()];
    run(
        "nvx-time",
        hook.into_iter().chain(arguments.iter().map(OsStr::new)),
    )
}

#[test]
#[cfg(target_arch = "x86_64")]
fn time_test_mode_decodes_packets_samples_and_arithmetic() {
    let directory = tempfile::tempdir().unwrap();
    let packet = time_fixture(
        directory.path(),
        "packet",
        time_packet(0x0d, 4, &[], 3, -13_107_200, 30_000_000_000),
    );
    assert_eq!(
        time_hook(
            "packet",
            &packet,
            &["2", "ffffffffffffffffffffffffffffffff"]
        ),
        (
            Some(0),
            concat!(
                "ok flags=13 online=4 ranges=0 generation=3 ",
                "rate_deviation=-13107200 frequency=13107200 ",
                "downtime_ns=30000000000 utc_ns=1790841600123456789 ",
                "generation_id=000102030405060708090a0b0c0d0e0f\n"
            )
            .as_bytes()
            .to_vec(),
            Vec::new(),
        )
    );

    let ranges = time_fixture(
        directory.path(),
        "ranges",
        time_packet(
            0x02,
            0,
            &[(0x8000_0000, 0x0800_0000), (0x1_0000_0000, 0x1000_0000)],
            1,
            0,
            0,
        ),
    );
    let output = time_hook(
        "packet",
        &ranges,
        &["0", "ffffffffffffffffffffffffffffffff"],
    );
    assert_eq!(output.0, Some(0));
    assert!(
        output
            .1
            .ends_with(b" range=2147483648:134217728 range=4294967296:268435456\n")
    );
    let no_ranges = time_fixture(
        directory.path(),
        "no-ranges",
        time_packet(0x02, 0, &[], 1, 0, 0),
    );
    let no_ranges = time_hook(
        "packet",
        &no_ranges,
        &["0", "ffffffffffffffffffffffffffffffff"],
    );
    assert_eq!(no_ranges.0, Some(0));
    assert!(
        no_ranges
            .1
            .windows(b" ranges=0 ".len())
            .any(|window| window == b" ranges=0 ")
    );

    let mut sample = Vec::new();
    sample.extend_from_slice(&[1, 8]);
    sample.extend_from_slice(&0_u16.to_le_bytes());
    sample.extend_from_slice(&4_u32.to_le_bytes());
    sample.extend_from_slice(&TIME_UTC_NS.to_le_bytes());
    let sample = time_fixture(directory.path(), "sample", sample);
    assert_eq!(
        time_hook("sample", &sample, &["4"]),
        (
            Some(0),
            b"ok flags=8 generation=4 utc_ns=1790841600123456789\n".to_vec(),
            Vec::new(),
        )
    );
    assert_eq!(
        run("nvx-time", ["test", "pair", "1000", "3000", "5000"]),
        (
            Some(0),
            b"theta_ns=3000 epsilon_ns=1000\n".to_vec(),
            Vec::new()
        )
    );
    assert_eq!(
        run("nvx-time", ["test", "frequency", "-40000000"]),
        (
            Some(0),
            b"frequency=32768000 ppb=500000\n".to_vec(),
            Vec::new()
        )
    );
    assert_eq!(
        run(
            "nvx-time",
            ["test", "correction", "-1128000000", "10", "slow"]
        ),
        (
            Some(0),
            b"step modes=0x2100 sec=-2 nsec=872000000\n".to_vec(),
            Vec::new()
        )
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn time_test_mode_checks_logs_timer_state_and_status() {
    let directory = tempfile::tempdir().unwrap();
    let boot = time_fixture(
        directory.path(),
        "boot",
        concat!(
            "Hypervisor detected: Microsoft Hyper-V\n",
            "Hyper-V: privilege flags low 0x8860, high 0x0\n",
            "Hyper-V: LAPIC Timer Frequency: 0x1e8480\n",
            "clocksource: Switched to clocksource tsc-early\n",
            "clocksource: Switched to clocksource tsc\n"
        ),
    );
    assert_eq!(
        time_hook("boot-log", &boot, &[]),
        (Some(0), b"ok\n".to_vec(), Vec::new())
    );
    let kmsg = time_fixture(
        directory.path(),
        "kmsg",
        concat!(
            "tsc: Marking TSC unstable due to watchdog\n",
            "clocksource: tsc: mask: 0xffff\n"
        ),
    );
    assert_eq!(
        time_hook("kmsg", &kmsg, &[]),
        (Some(0), b"G_TSC_UNSTABLE\nnone\n".to_vec(), Vec::new())
    );

    let timer = time_fixture(
        directory.path(),
        "timer",
        concat!(
            "Tick Device: mode:     1\n",
            "Broadcast device\n",
            "Clock Event Device: <NULL>\n",
            "Tick Device: mode:     1\n",
            "Per CPU device: 0\n",
            "Clock Event Device: lapic\n",
            " mode:           3\n",
            " event_handler:  hrtimer_interrupt\n"
        ),
    );
    assert_eq!(
        time_hook("timer-list", &timer, &["0"]),
        (Some(0), b"ok\n".to_vec(), Vec::new())
    );

    let state_text = concat!(
        "version=1\n",
        "generation=2\n",
        "boot_status=ok\n",
        "boot_cpus=8\n",
        "boot_elapsed_us=2400\n",
        "boot_cpu_us=1900\n",
        "tsc_hz=2194843000\n",
        "lapic_hz=200000000\n",
        "synchronized=1\n",
        "offset_ns=-42\n",
        "uncertainty_ns=1800\n",
        "last_sample_error=none\n"
    );
    let state = time_fixture(directory.path(), "state", state_text);
    assert_eq!(
        time_hook("state", &state, &[]).1,
        concat!(
            "version=1\ngeneration=2\nboot_status=ok\nboot_cpus=8\n",
            "boot_elapsed_us=2400\nboot_cpu_us=1900\n",
            "tsc_hz=2194843000\nlapic_hz=200000000\n",
            "discontinuities=0\nlast_discontinuity=none\nlast_step_ns=0\n",
            "last_step_realtime_ns=0\nlast_downtime_ns=0\n",
            "last_downtime_source=none\nsynchronized=1\noffset_ns=-42\n",
            "uncertainty_ns=1800\nfrequency_ppb=0\nsamples=0\n",
            "rejected_samples=0\nlast_sample_error=none\nviolations=0\n"
        )
        .as_bytes()
    );
    let malformed_state = time_fixture(
        directory.path(),
        "malformed-state",
        b"unknown=x\nsamples=bad\n",
    );
    assert_eq!(
        time_hook("state", &malformed_state, &[]),
        (Some(0), b"error\n".to_vec(), Vec::new())
    );
    let status = time_hook("status", &state, &["enforcing"]);
    assert_eq!(status.0, Some(0));
    assert!(
        status
            .1
            .starts_with(b"NVX-TIME-ABI: v=1 phase=boot status=ok cpus=8 tsc_hz=2194843000 ")
    );
    assert!(status.1.ends_with(b"last_sample_error=none\nok=1\n"));
}

#[test]
#[cfg(target_arch = "x86_64")]
fn time_test_mode_preserves_boot_log_and_syslog_results() {
    let directory = tempfile::tempdir().unwrap();
    let run_log = |name: &str, mode: &str, text: &str| {
        let fixture = time_fixture(directory.path(), name, text);
        time_hook(mode, &fixture, &[])
    };
    let boot = concat!(
        "Hypervisor detected: Microsoft Hyper-V\n",
        "Hyper-V: privilege flags low 0x8860, high 0x0, ext 0x0, hints 0x0, misc 0x100\n",
        "Hyper-V: LAPIC Timer Frequency: 0x1e8480\n",
        "tsc: Detected 2194.843 MHz processor\n",
        "clocksource: Switched to clocksource tsc-early\n",
        "clocksource: Switched to clocksource tsc\n"
    );
    assert_eq!(
        run_log(
            "boot-kvm-rate",
            "boot-log",
            &boot.replace("0x1e8480", "0x989680")
        ),
        (Some(0), b"ok\n".to_vec(), Vec::new())
    );

    let mut syslog = boot
        .lines()
        .enumerate()
        .map(|(index, line)| format!("<6>[{index:5}.{:06}] {line}", index * 7919 % 1_000_000))
        .collect::<Vec<_>>();
    syslog.insert(
        1,
        "<6>[    0.123456] [drm] a bracketed message is kept".to_owned(),
    );
    let syslog = syslog.join("\n") + "\n";
    assert_eq!(
        run_log("syslog-good", "syslog", &syslog),
        (Some(0), b"ok\n".to_vec(), Vec::new())
    );
    let own_c4 = concat!(
        "<2>[    3.000000] NVX-TIME-REPORT-VIOLATION: v=1 code=G_CONFORMANCE_C4 ",
        "detail=\"forbidden record: kvm-clock: Using msrs\"\n"
    );
    assert_eq!(
        run_log("syslog-own-c4", "syslog", &format!("{syslog}{own_c4}")),
        (Some(0), b"ok\n".to_vec(), Vec::new())
    );
    let kvm = run_log(
        "syslog-kvm",
        "syslog",
        &format!("{syslog}<6>[    4.000000] kvm-clock: Using msrs\n"),
    );
    assert_eq!(kvm.0, Some(0));
    assert!(kvm.2.is_empty());
    assert!(
        kvm.1.starts_with(b"fail forbidden record: kvm-clock"),
        "{}",
        String::from_utf8_lossy(&kvm.1)
    );
    let missing = syslog.lines().skip(2).collect::<Vec<_>>().join("\n") + "\n";
    let missing = run_log("syslog-missing", "syslog", &missing);
    assert_eq!(missing.0, Some(0));
    assert!(missing.2.is_empty());
    assert!(
        missing.1.starts_with(b"fail no 'Hypervisor"),
        "{}",
        String::from_utf8_lossy(&missing.1)
    );

    let lines = boot
        .lines()
        .map(|line| format!("<6>[    0.100000] {line}"))
        .collect::<Vec<_>>();
    let warning = [
        "<4>[    0.412345] ------------[ cut here ]------------",
        "<4>[    0.412346] x86/mm: Found insecure W+X mapping at address \
         0xffffffffc0000000",
        "<4>[    0.412400] WARNING: CPU: 0 PID: 1 at \
         arch/x86/mm/dump_pagetables.c:246 note_wx+0x5a/0x70",
        "<6>[    0.412500] x86/mm: Checked W+X mappings: FAILED, 4 W+X pages found.",
    ];
    let with_warning = lines[..2]
        .iter()
        .map(String::as_str)
        .chain(warning.iter().copied())
        .chain(lines[2..].iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_eq!(
        run_log("syslog-wx", "syslog", &with_warning),
        (
            Some(0),
            concat!(
                "ok\n",
                "wx x86/mm: Found insecure W+X mapping at address 0xffffffffc0000000\n"
            )
            .as_bytes()
            .to_vec(),
            Vec::new()
        )
    );
    let passed = lines
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(
            "<6>[    0.412500] x86/mm: Checked W+X mappings: passed, no W+X pages found.",
        ))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_eq!(
        run_log("syslog-wx-passed", "syslog", &passed),
        (Some(0), b"ok\n".to_vec(), Vec::new())
    );
    let missing_with_warning = warning
        .iter()
        .copied()
        .chain(lines[1..].iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_eq!(
        run_log("syslog-wx-missing", "syslog", &missing_with_warning),
        (
            Some(0),
            concat!(
                "fail no 'Hypervisor detected: Microsoft Hyper-V'\n",
                "wx x86/mm: Found insecure W+X mapping at address 0xffffffffc0000000\n"
            )
            .as_bytes()
            .to_vec(),
            Vec::new()
        )
    );
    let own_wx = concat!(
        "<2>[    3.000000] NVX-TIME-REPORT-VIOLATION: v=1 code=G_KERNEL_WX ",
        "detail=\"x86/mm: Found insecure W+X mapping at address 0x0\"\n"
    );
    assert_eq!(
        run_log(
            "syslog-own-wx",
            "syslog",
            &(lines.join("\n") + "\n" + own_wx)
        ),
        (Some(0), b"ok\n".to_vec(), Vec::new())
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn time_test_mode_preserves_parser_and_usage_failures() {
    let usage = concat!(
        "usage: nvx-time [--report-only] COMMAND\n",
        "  boot\n",
        "  check --phase capture|restore [--new-cpus LIST]\n",
        "  pre-capture\n",
        "  capture 0|1 GENERATION_ID ENTROPY_FILE [--finish]\n",
        "  cancel-capture\n",
        "  restore-finish [--ack] [--new-cpus LIST]\n",
        "  status\n",
        "  sample\n",
        "  generation-id\n",
        "  exhaustive\n",
        "  test NAME ARGUMENTS...\n"
    )
    .as_bytes()
    .to_vec();
    assert_eq!(
        run("nvx-time", std::iter::empty::<&str>()),
        (Some(2), Vec::new(), usage)
    );
    assert_eq!(
        run("nvx-time", ["test", "deferral"]),
        (
            Some(0),
            b"deferred_start_ms=150 idle_bound_ms=100\n".to_vec(),
            Vec::new()
        )
    );
    assert_eq!(
        run(
            "nvx-time",
            [
                "test",
                "exhaustive-leaf",
                "0x40000081",
                "1",
                "2",
                "3",
                "4",
                "1",
                "2",
                "3",
                "4"
            ]
        )
        .1,
        concat!(
            "fail leaf 0x40000081 is 00000001 00000002 00000003 00000004 ",
            "(signature \"............\"), expected zero\n"
        )
        .as_bytes()
    );
}

#[cfg(target_arch = "x86_64")]
fn run_time_packet(
    directory: &Path,
    name: &str,
    packet: &[u8],
    recorded: &str,
    previous: &str,
) -> (Option<i32>, Vec<u8>, Vec<u8>) {
    let path = time_fixture(directory, name, packet);
    time_hook("packet", &path, &[recorded, previous])
}

#[test]
#[cfg(target_arch = "x86_64")]
fn time_test_mode_checks_packet_and_sample_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let previous = "ffffffffffffffffffffffffffffffff";
    let base = time_packet(0, 0, &[], 1, 0, 0);
    let mut rejected = Vec::new();
    let mutate = |offset: usize, bytes: &[u8]| {
        let mut packet = base.clone();
        packet[offset..offset + bytes.len()].copy_from_slice(bytes);
        packet
    };
    rejected.push(mutate(0, b"OVX"));
    rejected.push(mutate(3, &[3]));
    rejected.push(mutate(4, &[0x10]));
    rejected.push(mutate(5, &[3]));
    rejected.push(mutate(6, &[1]));
    rejected.push(mutate(7, &[1]));
    rejected.push(mutate(8, &0_u32.to_le_bytes()));
    rejected.push(mutate(12, &16_384_001_i32.to_le_bytes()));
    rejected.push(mutate(12, &(-16_384_001_i32).to_le_bytes()));
    rejected.push(mutate(
        16,
        &(2_592_000_u64 * 1_000_000_000 + 1).to_le_bytes(),
    ));
    rejected.push(mutate(24, &0_u64.to_le_bytes()));
    rejected.push(time_packet(0x02, 0, &[(0, 0)], 1, 0, 0));
    rejected.push(time_packet(0x02, 0, &[(u64::MAX - 4095, 8192)], 1, 0, 0));
    for (index, packet) in rejected.iter().enumerate() {
        let output = run_time_packet(
            directory.path(),
            &format!("rejected-{index}"),
            packet,
            "0",
            previous,
        );
        assert_eq!(output.0, Some(0));
        assert!(
            output.1.starts_with(b"error G_REPAIR_PACKET "),
            "case {index}: {:?}",
            String::from_utf8_lossy(&output.1)
        );
    }

    for (index, packet) in [
        time_packet(0, 8, &[], 1, 16_384_000, 2_592_000 * 1_000_000_000),
        time_packet(0, 0, &[], 1, -16_384_000, 0),
    ]
    .iter()
    .enumerate()
    {
        assert!(
            run_time_packet(
                directory.path(),
                &format!("accepted-{index}"),
                packet,
                "0",
                previous
            )
            .1
            .starts_with(b"ok ")
        );
    }
    assert_eq!(
        run_time_packet(directory.path(), "stale", &base, "1", previous).1,
        b"stale\n"
    );
    assert!(
        run_time_packet(directory.path(), "skipped", &base, "2", previous)
            .1
            .starts_with(b"error G_REPAIR_GENERATION ")
    );
    let forward_gap = time_packet(0, 0, &[], 7, 0, 0);
    assert!(
        run_time_packet(directory.path(), "forward-gap", &forward_gap, "5", previous)
            .1
            .starts_with(b"error G_REPAIR_GENERATION ")
    );
    assert!(
        run_time_packet(
            directory.path(),
            "unchanged",
            &base,
            "0",
            "000102030405060708090a0b0c0d0e0f"
        )
        .1
        .starts_with(b"error G_REPAIR_GENERATION ")
    );

    let sample_path =
        |name: &str, version: u8, flags: u8, reserved: u16, generation: u32, utc: u64| {
            let mut sample = vec![version, flags];
            sample.extend_from_slice(&reserved.to_le_bytes());
            sample.extend_from_slice(&generation.to_le_bytes());
            sample.extend_from_slice(&utc.to_le_bytes());
            time_fixture(directory.path(), name, sample)
        };
    let good = sample_path("sample-good", 1, 8, 0, 4, TIME_UTC_NS);
    assert_eq!(
        time_hook("sample", &good, &["4"]).1,
        b"ok flags=8 generation=4 utc_ns=1790841600123456789\n"
    );
    for (index, path) in [
        sample_path("sample-version", 2, 0, 0, 0, TIME_UTC_NS),
        sample_path("sample-flags", 1, 1, 0, 0, TIME_UTC_NS),
        sample_path("sample-reserved", 1, 0, 1, 0, TIME_UTC_NS),
        sample_path("sample-utc", 1, 0, 0, 0, 0),
        sample_path("sample-generation", 1, 0, 0, 3, TIME_UTC_NS),
    ]
    .iter()
    .enumerate()
    {
        let output = time_hook("sample", path, &["0"]);
        assert!(
            output.1.starts_with(b"error "),
            "sample case {index}: {:?}",
            String::from_utf8_lossy(&output.1)
        );
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn time_test_mode_covers_checks_events_records_and_workers() {
    let directory = tempfile::tempdir().unwrap();
    let good_flags = concat!(
        "tsc msr pae constant_tsc rdtscp nonstop_tsc tsc_known_freq ",
        "tsc_reliable hypervisor arat x2apic"
    );
    let cpuinfo = format!(
        "processor : 0\ncpu MHz : 2194.843\nflags : {good_flags}\n\n\
         processor : 1\ncpu MHz : 2194.843\nflags : {good_flags}\n"
    );
    let cpuinfo = time_fixture(directory.path(), "cpuinfo", cpuinfo);
    assert_eq!(
        time_hook("cpuinfo", &cpuinfo, &["0-1"]).1,
        b"ok cpu0_khz=2194843 cpu1_khz=2194843\n"
    );
    let cpuinfo_text = |flags: &str, cpus: usize| {
        (0..cpus)
            .map(|cpu| format!("processor : {cpu}\ncpu MHz : 2194.843\nflags : {flags}\n\n"))
            .collect::<String>()
    };
    for (name, flags, cpus) in [
        (
            "cpuinfo-no-reliable",
            good_flags.replace("tsc_reliable ", ""),
            2,
        ),
        (
            "cpuinfo-deadline",
            format!("{good_flags} tsc_deadline_timer"),
            2,
        ),
        ("cpuinfo-adjust", format!("{good_flags} tsc_adjust"), 2),
        ("cpuinfo-missing", good_flags.to_owned(), 1),
    ] {
        let fixture = time_fixture(directory.path(), name, cpuinfo_text(&flags, cpus));
        let output = time_hook("cpuinfo", &fixture, &["0-1"]);
        assert_eq!(output.0, Some(0));
        assert!(output.2.is_empty());
        assert!(
            output
                .1
                .windows(b"code=G_CONFORMANCE_C5".len())
                .any(|window| window == b"code=G_CONFORMANCE_C5"),
            "{name}: {}",
            String::from_utf8_lossy(&output.1)
        );
    }
    for token in [
        "tsc_early_khz=2194843",
        "lapic_timer_hz=200000000",
        "notsc",
        "nolapic",
        "nolapic_timer",
        "tsc=unstable",
        "hpet=force",
        "clocksource=kvm-clock",
    ] {
        let cmdline = format!("earlycon=xe9 clocksource=tsc {token}");
        assert!(
            run("nvx-time", ["test", "cmdline", &cmdline])
                .1
                .windows(b"code=G_CONFORMANCE_C9".len())
                .any(|window| window == b"code=G_CONFORMANCE_C9")
        );
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn time_probe_cli_and_small_diagnostics_run() {
    let usage = concat!(
        "usage: nvx-time-probe COMMAND [OPTIONS]\n",
        "\n",
        "Commands:\n",
        "  latency [--iterations N] [--batch B] [--cpu C] [--tsc-hz HZ]\n",
        "      Per-operation cost (p50/p99/min/mean ns) of rdtsc, rdtscp,\n",
        "      lfence;rdtsc, clock_gettime(MONOTONIC/REALTIME) through the vDSO,\n",
        "      and syscall controls. Default: 2000 timed batches of 50 operations\n",
        "      on the current CPU.\n",
        "  warp [--duration-ms N] [--cpus LIST] [--bound-ns N] [--tsc-hz HZ]\n",
        "      Pairwise cross-CPU TSC checks: a check_tsc_warp-style spinlock test\n",
        "      and a ping-pong offset estimate, each running N ms per pair\n",
        "      (default 100). LIST looks like 0-3,6 (default: every CPU in the\n",
        "      affinity mask). The bound defaults to 1000 ns.\n",
        "  clock [--measure-ms N] [--tsc-hz HZ] [--msr]\n",
        "      Clocks, clocksources, TSC flags, CPUID time leaves, hypervisor and\n",
        "      time kernel messages, and the /proc/timer_list clock event summary.\n",
        "      --msr also reads the TSC, Hyper-V time, and kvmclock MSRs on CPU 0\n",
        "      through /dev/cpu/0/msr (a #GP is reported, not raised).\n",
        "  version\n",
        "\n",
        "The TSC rate comes from --tsc-hz, sysfs tsc_freq_khz, the kernel log, or\n",
        "/proc/cpuinfo, in that order, and is measured as a last resort.\n",
        "Exit status: 0 success, 1 warp FAIL, 2 usage error, 3 runtime error.\n"
    )
    .as_bytes()
    .to_vec();
    assert_eq!(
        run("nvx-time-probe", std::iter::empty::<&str>()),
        (Some(2), Vec::new(), usage.clone())
    );
    assert_eq!(
        run("nvx-time-probe", ["version"]),
        (Some(0), b"nvx-time-probe 1.1\n".to_vec(), Vec::new())
    );
    assert_eq!(
        run("nvx-time-probe", ["--help"]),
        (Some(0), usage, Vec::new())
    );
    assert_eq!(
        run("nvx-time-probe", ["latency", "--iterations=0"]),
        (
            Some(2),
            Vec::new(),
            b"nvx-time-probe latency: invalid option or value: --iterations=0\n".to_vec()
        )
    );

    let cpu = first_allowed_cpu().to_string();
    let latency = run(
        "nvx-time-probe",
        [
            "latency",
            "--iterations",
            "16",
            "--batch",
            "2",
            "--cpu",
            &cpu,
            "--tsc-hz",
            "2500000000",
        ],
    );
    assert_eq!(latency.0, Some(0));
    assert!(latency.2.is_empty());
    assert!(
        latency
            .1
            .starts_with(format!("nvx-time-probe 1.1 latency: cpu={cpu} ").as_bytes())
    );
    assert_eq!(
        latency
            .1
            .windows(b"NVX-TIME-PROBE latency method=".len())
            .filter(|window| *window == b"NVX-TIME-PROBE latency method=")
            .count(),
        8
    );

    let warp = run(
        "nvx-time-probe",
        [
            "warp",
            "--duration-ms",
            "1",
            "--cpus",
            &cpu,
            "--bound-ns",
            "1000",
            "--tsc-hz",
            "2500000000",
        ],
    );
    assert_eq!(warp.0, Some(0));
    assert!(warp.2.is_empty());
    assert!(
        warp.1.ends_with(
            format!(
                "NVX-TIME-PROBE warp-detail max_abs_offset_cycles=0.0 \
                 max_uncertainty_ns=0 max_skew_bound_ns=0 total_warps=0 \
                 inconsistent_pairs=0 stalled_pairs=0 cpus={cpu} duration_ms=1 \
                 tsc_hz=2500000000 tsc_hz_source=option conclusive=1\n"
            )
            .as_bytes()
        )
    );

    let clock = run(
        "nvx-time-probe",
        ["clock", "--measure-ms", "0", "--tsc-hz", "2500000000"],
    );
    assert_eq!(clock.0, Some(0));
    assert!(clock.2.is_empty());
    assert!(
        clock
            .1
            .starts_with(b"nvx-time-probe 1.1 clock\n== clocks\n")
    );
    assert!(
        clock
            .1
            .windows(b"\nNVX-TIME-PROBE clock ".len())
            .any(|window| window == b"\nNVX-TIME-PROBE clock ")
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn time_probe_warp_runs_a_cpu_pair() {
    let affinity = nix::sched::sched_getaffinity(nix::unistd::Pid::from_raw(0)).unwrap();
    let cpus = (0..nix::sched::CpuSet::count())
        .filter(|&cpu| affinity.is_set(cpu).unwrap())
        .take(2)
        .collect::<Vec<_>>();
    let [first, second] = cpus[..] else {
        // A single CPU has no pair to run.
        return;
    };
    let (status, stdout, stderr) = run_within(
        "nvx-time-probe",
        [
            "warp",
            "--duration-ms",
            "1",
            "--cpus",
            &format!("{first},{second}"),
            "--tsc-hz",
            "2500000000",
        ],
        Duration::from_secs(60),
    );
    let stdout = String::from_utf8(stdout).unwrap();
    // The verdict depends on the host's TSCs, so either verdict's status ends a run.
    assert!(matches!(status, Some(0 | 1)), "{status:?}: {stdout}");
    assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(&stderr));
    assert!(stdout.contains(" pairs=1 verdict="), "{stdout}");
    let prefix = format!("pair {first}-{second}: ");
    let pair = stdout
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no result for the pair: {stdout}"));
    let field = |name: &str| -> u64 {
        pair.split(' ')
            .find_map(|field| field.strip_prefix(name)?.strip_prefix('=')?.parse().ok())
            .unwrap_or_else(|| panic!("no {name}: {pair}"))
    };
    // Each worker of the spinlock test counts its iterations, and the ping-pong test completes
    // round trips rather than stalling.
    assert!(field("warp_iterations") >= 2, "{pair}");
    assert!(field("rounds") >= 1, "{pair}");
}

#[test]
#[cfg(target_arch = "x86_64")]
fn time_test_mode_covers_remaining_events_records_and_workers() {
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        run(
            "nvx-time",
            [
                "test",
                "clocksource",
                "tsc\n",
                "tsc refined-jiffies jiffies\n"
            ]
        )
        .1,
        b"ok\n"
    );
    for (current, available) in [
        ("tsc-early\n", "tsc-early \n"),
        ("tsc\n", "tsc kvm-clock \n"),
        ("refined-jiffies\n", "refined-jiffies \n"),
    ] {
        assert!(
            run("nvx-time", ["test", "clocksource", current, available])
                .1
                .windows(b"code=G_CONFORMANCE_C6".len())
                .any(|window| window == b"code=G_CONFORMANCE_C6")
        );
    }

    assert_eq!(
        run(
            "nvx-time",
            [
                "test",
                "event",
                "G_UNCHECKED_MSR",
                "watcher",
                "runtime",
                "3",
                "123456789",
                "unchecked \"MSR\"\\ access\u{1}",
            ]
        )
        .1,
        concat!(
            "NVX-TIME-ABI-VIOLATION: v=1 code=G_UNCHECKED_MSR source=watcher ",
            "phase=runtime generation=3 boottime_ns=123456789 ",
            "detail=\"unchecked \\\"MSR\\\"\\\\ access\\x01\"\n"
        )
        .as_bytes()
    );
    let long_event = run(
        "nvx-time",
        [
            OsStr::new("test"),
            OsStr::new("event"),
            OsStr::new("G_HUNG_TASK"),
            OsStr::new("watcher"),
            OsStr::new("runtime"),
            OsStr::new("0"),
            OsStr::new("1"),
            &std::ffi::OsString::from_vec(vec![2; 400]),
        ],
    )
    .1;
    assert!(long_event.len() <= 512 && long_event.ends_with(b"\\x02\"\n"));

    for (name, record, expected) in [
        (
            "restore-cpus",
            "generation=3\nnew_cpus=4-6,9\n",
            "generation=3 new_cpus=4,5,6,9 count=4\n",
        ),
        (
            "restore-none",
            "generation=3\nnew_cpus=none\n",
            "generation=3 new_cpus=none count=0\n",
        ),
        (
            "restore-absent",
            "generation=3\n",
            "generation=3 new_cpus=none count=0\n",
        ),
    ] {
        let record = time_fixture(directory.path(), name, record);
        assert_eq!(
            time_hook("restore-record", &record, &[]).1,
            expected.as_bytes()
        );
    }
    for (index, record) in [
        "",
        "new_cpus=4\ngeneration=3\n",
        "generation=3\nnew_cpus=7-4\n",
        "generation=3\nnew_cpus=64\n",
    ]
    .into_iter()
    .enumerate()
    {
        let record = time_fixture(directory.path(), &format!("restore-error-{index}"), record);
        assert_eq!(
            time_hook("restore-record", &record, &[]),
            (Some(0), b"error\n".to_vec(), Vec::new()),
            "case {index}"
        );
    }

    // These exercise the same thread promotion, affinity merge, and at-fork CPU accounting
    // paths as the production deferred boot and restore workers.
    let cpu = first_allowed_cpu();
    if cpu < 64 {
        let output = run("nvx-time", ["test", "parallel", &cpu.to_string()]);
        assert_eq!(output.0, Some(0));
        assert!(output.1.ends_with(b"failures=7\n"));
    }
    let allowed = allowed_time_cpus();
    if let Some(&cpu) = allowed.first() {
        let mut sizes = vec![1, allowed.len().min(2), allowed.len().min(4)];
        sizes.sort_unstable();
        sizes.dedup();
        for size in sizes {
            let selected = allowed[..size]
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let (status, output, stderr) = run("nvx-time", ["test", "parallel", &selected]);
            assert_eq!(status, Some(0));
            assert!(stderr.is_empty());
            let output = String::from_utf8(output).unwrap();
            assert!(output.ends_with("failures=7\n"), "{output}");
            assert_eq!(
                output.matches("code=G_CONFORMANCE_C4").count(),
                6,
                "{output}"
            );
            assert_eq!(
                output.matches("code=G_CONFORMANCE_C9").count(),
                1,
                "{output}"
            );
        }

        // Each CPU job that cannot run on its CPU fails C1, and every job's failures count.
        if let Some(&unusable) = unusable_cpus().first() {
            let (status, output, stderr) = run(
                "nvx-time",
                ["test", "parallel", &format!("{cpu},{unusable}")],
            );
            assert_eq!(status, Some(0));
            assert!(stderr.is_empty());
            let output = String::from_utf8(output).unwrap();
            assert!(output.ends_with("failures=8\n"), "{output}");
            assert!(
                output.contains(&format!("cannot run on cpu {unusable}")),
                "{output}"
            );
        }
        if let [first, second, ..] = unusable_cpus()[..] {
            let (status, output, _) = run(
                "nvx-time",
                ["test", "parallel", &format!("{cpu},{first},{second}")],
            );
            assert_eq!(status, Some(0));
            let output = String::from_utf8(output).unwrap();
            assert!(output.ends_with("failures=9\n"), "{output}");
            for cpu in [first, second] {
                let failure = format!("cannot run on cpu {cpu}: ");
                assert!(output.contains(&failure), "{output}");
            }
        }
    }
    let normal_policy = if can_leave_idle_priority() { 0 } else { 5 };
    let promoted = run("nvx-time", ["test", "promote"]);
    assert_eq!(promoted.0, Some(0));
    assert!(promoted.1.starts_with(b"promoted caller="));
    assert!(
        promoted
            .1
            .windows(b"\nstopped caller=".len())
            .any(|window| window == b"\nstopped caller=")
    );
    assert!(promoted.2.is_empty());
    let promoted = String::from_utf8(promoted.1).unwrap();
    let mut promoted_lines = promoted.lines();
    assert_eq!(
        promoted_lines.next(),
        Some(format!("promoted caller={normal_policy} thread={normal_policy}").as_str())
    );
    let stopped = promoted_lines.next().unwrap();
    let stop_ms = stopped
        .strip_prefix("stopped caller=5 stop_ms=")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_else(|| panic!("unexpected stop line: {stopped}"));
    assert!(stop_ms < 1000, "{stopped}");
    assert_eq!(promoted_lines.next(), None);

    let idle = run("nvx-time", ["test", "idle-priority"]);
    assert_eq!(idle.0, Some(0));
    assert!(idle.1.starts_with(b"idle=5 normal="));
    assert_eq!(
        idle,
        (
            Some(0),
            format!("idle=5 normal={normal_policy}\n").into_bytes(),
            Vec::new()
        )
    );
    let cpu = run(
        "nvx-time",
        ["test", "deferred-restore-cpu", "20", "30", "10"],
    );
    assert_eq!(cpu.0, Some(0));
    let fields = String::from_utf8(cpu.1).unwrap();
    let fields = fields
        .split_whitespace()
        .filter_map(|field| field.split_once('='))
        .map(|(name, value)| (name, value.parse::<i64>().unwrap()))
        .collect::<Vec<_>>();
    // The preparation and the fork's share precede the worker's own CPU time in its total.
    assert!(
        matches!(fields[..], [("prior_ms", 20..30), ("total_ms", 60..)]),
        "{fields:?}"
    );
    assert!(
        matches!(fields[..], [("prior_ms", 20..30), ("total_ms", 60..90)]),
        "{fields:?}"
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn time_reads_a_plain_capture_request() {
    let previous = "ffffffffffffffffffffffffffffffff";
    let usage = b"usage: nvx-time capture 0|1 GENERATION_ID ENTROPY_FILE [--finish]\n".to_vec();
    for request in ["", "+1", "-0", " 1", "2", "0x1"] {
        assert_eq!(
            run("nvx-time", ["capture", request, previous, "/nonexistent"]),
            (Some(2), Vec::new(), usage.clone()),
            "{request:?}"
        );
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn time_probe_reads_plain_numbers_from_its_options() {
    for (command, option) in [
        ("latency", "--iterations=010"),
        ("latency", "--batch=+5"),
        ("latency", "--tsc-hz=inf"),
        ("latency", "--tsc-hz=0x1p30"),
        ("warp", "--cpus= 1"),
        ("warp", "--cpus=0,0"),
        ("warp", "--duration-ms=-1"),
        ("clock", "--measure-ms=60001"),
        ("clock", "--tsc-hz=1e400"),
    ] {
        assert_eq!(
            run("nvx-time-probe", [command, option]),
            (
                Some(2),
                Vec::new(),
                format!("nvx-time-probe {command}: invalid option or value: {option}\n")
                    .into_bytes()
            ),
        );
    }
}
