//! Test double for the `openvmm` executable.
//!
//! It emulates the part of OpenVMM that the `openvmm` backend relies on, without running a VM:
//! it validates the managed microVM command line, reads the launch capability from standard
//! input, serves the authenticated control console on the requested Unix socket or named pipe,
//! and runs scripted workloads. Integration tests point `OpenVmmConfig::openvmm` at it.
//!
//! Workloads are `/bin/sh -c SCRIPT` or `/bin/echo ARGS...`. A script is a `;`-separated list
//! of commands: `echo TEXT`, `echoerr TEXT`, `sleep MS`, `exit CODE`, `signal NUMBER`,
//! `flood BYTES`, `write KEY VALUE`, `read KEY`, `fail`, and `launchfail`. Values written with
//! `write` live in memory until the VM stops, like files in the guest's RAM root file system.
//! A `CANCEL` request ends a sleeping workload with the cancelled outcome, and a client that
//! disconnects during an exec abandons it, as the real guest agent does. `pwd` prints the
//! working directory that the backend's `cd` prelude selects.
//!
//! Kernel command-line tokens adjust the emulation: `fake_exit_on_start=CODE` fails the launch,
//! `fake_boot_delay_ms=MS` delays the control endpoint, `fake_crash_after_ms=MS` makes the VM
//! die, `fake_ignore_stop=1` ignores stop requests, `fake_ignore_cancel=1` ignores cancellation,
//! and `fake_legacy_guest=1` refuses the
//! features request like a guest agent that predates it. The command line is recorded in
//! `fake-openvmm-<token>.json` next to the kernel.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

const CAPABILITY_LEN: usize = 32;
const OUTER_HEADER_LEN: usize = 44;
const APP_HEADER_LEN: usize = 24;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const OUTPUT_CHUNK_BYTES: usize = 32 * 1024;

const OUTER_HOST_ATTACH: u8 = 2;
const OUTER_DATA: u8 = 5;
const OUTER_READY: u8 = 7;
const OUTER_ERROR: u8 = 8;

const APP_PING: u8 = 1;
const APP_EXEC: u8 = 2;
const APP_STOP: u8 = 3;
const APP_CANCEL: u8 = 4;
const APP_FEATURES: u8 = 5;
const APP_READY: u8 = 0x81;
const APP_STDOUT: u8 = 0x82;
const APP_STDERR: u8 = 0x83;
const APP_EXIT: u8 = 0x84;
const APP_STOPPED: u8 = 0x85;
const APP_ERROR: u8 = 0xff;

/// Control features of the current guest: cancellation, host path mappings, workload accounts,
/// and workload containment.
const GUEST_FEATURES: u32 = 0b1111;

struct Options {
    endpoint: String,
    report: PathBuf,
    hypervisor: String,
    args_dump: PathBuf,
    exit_on_start: Option<u8>,
    boot_delay: Duration,
    crash_after: Option<Duration>,
    ignore_stop: bool,
    ignore_cancel: bool,
    legacy_guest: bool,
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse(&arguments) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("aci-edge-sandboxes-fake-openvmm: {message}");
            return ExitCode::from(2);
        }
    };
    let _ = fs::write(
        &options.args_dump,
        serde_json::to_vec(&arguments).unwrap_or_default(),
    );
    if let Some(code) = options.exit_on_start {
        eprintln!("aci-edge-sandboxes-fake-openvmm: failing the launch as requested");
        return ExitCode::from(code);
    }
    let capability = match read_capability() {
        Ok(capability) => capability,
        Err(message) => {
            eprintln!("aci-edge-sandboxes-fake-openvmm: {message}");
            return ExitCode::from(2);
        }
    };
    thread::sleep(options.boot_delay);
    if let Some(after) = options.crash_after {
        thread::spawn(move || {
            thread::sleep(after);
            std::process::exit(3);
        });
    }
    match serve(&options, &capability) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("aci-edge-sandboxes-fake-openvmm: {error}");
            ExitCode::from(1)
        }
    }
}

fn parse(arguments: &[String]) -> Result<Options, String> {
    const VALUED: [&str; 20] = [
        "--mount",
        "--mount-deny",
        "--network-egress-allow",
        "--network-egress-deny",
        "--machine",
        "--microvm-workload-identity",
        "--microvm-lifecycle",
        "--hypervisor",
        "--memory",
        "--kernel",
        "--initrd",
        "--cmdline",
        "--virtio-console",
        "--microvm-control-console",
        "--microvm-report",
        "--net",
        "--network-profile",
        "--network-egress",
        "--network-ingress",
        "--host-loopback",
    ];
    let mut values: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut flags = BTreeSet::new();
    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        let name = argument.as_str();
        if name == "--single-process" || name == "--microvm-control-auth-stdin" {
            flags.insert(name);
        } else if VALUED.contains(&name) {
            let value = iterator
                .next()
                .ok_or_else(|| format!("{name} requires a value"))?;
            values.entry(name).or_default().push(value);
        } else {
            return Err(format!("unsupported argument {name}"));
        }
    }
    let single = |name: &str| -> Result<&str, String> {
        match values.get(name).map(Vec::as_slice) {
            Some([value]) => Ok(value),
            _ => Err(format!("{name} must be given exactly once")),
        }
    };
    let expect = |name: &str, expected: &str| -> Result<(), String> {
        let value = single(name)?;
        if value == expected {
            Ok(())
        } else {
            Err(format!("{name} must be {expected}, not {value}"))
        }
    };
    expect("--machine", "microvm")?;
    expect("--microvm-lifecycle", "managed")?;
    expect("--virtio-console", "none")?;
    for flag in ["--single-process", "--microvm-control-auth-stdin"] {
        if !flags.contains(flag) {
            return Err(format!("{flag} is required"));
        }
    }
    let hypervisor = single("--hypervisor")?;
    if !["kvm", "mshv", "whp"].contains(&hypervisor) {
        return Err(format!("unsupported hypervisor {hypervisor}"));
    }
    if !single("--memory")?.ends_with('M') {
        return Err("--memory must be given in MiB".to_owned());
    }
    let identity = single("--microvm-workload-identity")?;
    let valid_identity = identity.split_once(':').is_some_and(|(uid, gid)| {
        [uid, gid]
            .iter()
            .all(|id| id.parse::<u32>().is_ok_and(|id| id != 0))
    });
    if !valid_identity {
        return Err(format!("invalid workload identity {identity}"));
    }
    let kernel = PathBuf::from(single("--kernel")?);
    for path in [&kernel, &PathBuf::from(single("--initrd")?)] {
        if !path.is_file() {
            return Err(format!("{} does not exist", path.display()));
        }
    }
    let endpoint = single("--microvm-control-console")?
        .strip_prefix("listen=")
        .ok_or("--microvm-control-console must use listen=")?
        .to_owned();
    let report = PathBuf::from(single("--microvm-report")?);
    if report.exists() {
        return Err(format!("report path {} already exists", report.display()));
    }
    let command_line = single("--cmdline")?;
    let tokens: Vec<&str> = command_line.split_whitespace().collect();
    if tokens.iter().any(|token| {
        token.starts_with("nvx_")
            && !token.starts_with("nvx_map=")
            && *token != "nvx_workload_account=create"
    }) {
        return Err("the kernel command line must not select a sandbox".to_owned());
    }
    let maps = tokens
        .iter()
        .filter(|token| token.starts_with("nvx_map="))
        .count();
    match values.get("--mount").map(Vec::as_slice) {
        None if maps == 0 && !values.contains_key("--mount-deny") => {}
        Some([mount]) if maps > 0 => {
            let mut fields = mount.splitn(3, ',');
            let (Some(target), Some(host), Some(mode)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(format!("invalid --mount {mount}"));
            };
            if target != "/run/nvx/hostfs/root"
                || !Path::new(host).is_dir()
                || !["ro", "rw"].contains(&mode)
            {
                return Err(format!("invalid --mount {mount}"));
            }
            for denied in values.get("--mount-deny").into_iter().flatten() {
                let denied = Path::new(denied);
                if !denied.exists() || !denied.starts_with(host) {
                    return Err(format!("invalid --mount-deny {}", denied.display()));
                }
            }
        }
        _ => return Err("--mount, --mount-deny, and nvx_map tokens must agree".to_owned()),
    }
    if (values.contains_key("--network-egress-allow")
        || values.contains_key("--network-egress-deny"))
        && !values.contains_key("--network-egress")
    {
        return Err("--network-egress is required with egress rules".to_owned());
    }
    if !tokens.iter().any(|token| token.starts_with("hostname=")) {
        return Err("the kernel command line must set the hostname".to_owned());
    }
    if values.contains_key("--net") {
        expect("--network-profile", "portable")?;
    }
    let knob = |name: &str| {
        tokens
            .iter()
            .find_map(|token| token.strip_prefix(name)?.strip_prefix('='))
    };
    let millis = |name: &str| {
        knob(name)
            .and_then(|value| value.parse().ok())
            .map(Duration::from_millis)
    };
    let token = report
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Options {
        endpoint,
        args_dump: kernel.with_file_name(format!("fake-openvmm-{token}.json")),
        report,
        hypervisor: hypervisor.to_owned(),
        exit_on_start: knob("fake_exit_on_start").and_then(|value| value.parse().ok()),
        boot_delay: millis("fake_boot_delay_ms").unwrap_or_default(),
        crash_after: millis("fake_crash_after_ms"),
        ignore_stop: knob("fake_ignore_stop") == Some("1"),
        ignore_cancel: knob("fake_ignore_cancel") == Some("1"),
        legacy_guest: knob("fake_legacy_guest") == Some("1"),
    })
}

fn read_capability() -> Result<[u8; CAPABILITY_LEN], String> {
    let mut stdin = io::stdin().lock();
    let mut capability = [0u8; CAPABILITY_LEN];
    stdin
        .read_exact(&mut capability)
        .map_err(|error| format!("cannot read the capability from stdin: {error}"))?;
    let mut extra = [0u8; 1];
    if stdin.read(&mut extra).map_err(|error| error.to_string())? != 0 {
        return Err("standard input carried more than the capability".to_owned());
    }
    if capability == [0; CAPABILITY_LEN] {
        return Err("the capability is all zero".to_owned());
    }
    Ok(capability)
}

/// Guest state that lives in memory until the VM stops.
#[derive(Default)]
struct Guest {
    values: BTreeMap<String, String>,
}

enum Flow {
    Continue,
    Exit,
}

/// Outcome of control traffic observed while a workload runs.
enum Interrupt {
    None,
    Cancel,
    Disconnected,
}

/// A control stream that can report whether input is waiting without consuming it.
trait Pending {
    /// Returns whether a read would make progress, including at end of stream.
    fn pending(&mut self) -> io::Result<bool>;
}

#[cfg(unix)]
impl Pending for std::os::unix::net::UnixStream {
    fn pending(&mut self) -> io::Result<bool> {
        use std::os::fd::AsRawFd;

        let mut descriptor = libc::pollfd {
            fd: self.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: the descriptor array has one valid entry, and a zero timeout only polls.
        match unsafe { libc::poll(&mut descriptor, 1, 0) } {
            -1 => Err(io::Error::last_os_error()),
            ready => Ok(ready > 0),
        }
    }
}

#[cfg(windows)]
impl Pending for fs::File {
    fn pending(&mut self) -> io::Result<bool> {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;

        let mut available = 0u32;
        // SAFETY: the handle is a connected pipe, and only the byte count is requested.
        let peeked = unsafe {
            PeekNamedPipe(
                self.as_raw_handle() as HANDLE,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        // A failed peek means the client is gone; the next read reports end of stream.
        Ok(peeked == 0 || available > 0)
    }
}

fn outer(
    record_type: u8,
    instance: &[u8; 16],
    epoch: u64,
    sequence: u64,
    payload: &[u8],
) -> Vec<u8> {
    let mut record = Vec::with_capacity(OUTER_HEADER_LEN + payload.len());
    record.extend_from_slice(b"NVXS");
    record.extend_from_slice(&1u16.to_le_bytes());
    record.extend_from_slice(&[record_type, 0]);
    record.extend_from_slice(instance);
    record.extend_from_slice(&epoch.to_le_bytes());
    record.extend_from_slice(&sequence.to_le_bytes());
    record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    record.extend_from_slice(payload);
    record
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

type Record = ([u8; OUTER_HEADER_LEN], Vec<u8>);

/// Reads one outer record, returning `None` at end of stream.
fn read_record<S: Read>(stream: &mut S) -> io::Result<Option<Record>> {
    let mut header = [0u8; OUTER_HEADER_LEN];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    if &header[..4] != b"NVXS" || header[4..6] != 1u16.to_le_bytes() || header[7] != 0 {
        return Err(io::Error::other("invalid outer record"));
    }
    let length = u32_at(&header, 40) as usize;
    if length > 65_536 {
        return Err(io::Error::other("oversized outer record"));
    }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload)?;
    Ok(Some((header, payload)))
}

/// One authenticated host session over a connected stream.
struct Session<'a, S> {
    stream: &'a mut S,
    instance: [u8; 16],
    epoch: u64,
    guest_sequence: u64,
    host_sequence: u64,
    ignore_cancel: bool,
}

impl<S: Read + Write + Pending> Session<'_, S> {
    fn send(&mut self, kind: u8, request_id: u64, status: i32, payload: &[u8]) -> io::Result<()> {
        let mut frame = Vec::with_capacity(APP_HEADER_LEN + payload.len());
        frame.extend_from_slice(b"NVXC");
        frame.extend_from_slice(&[1, kind, 0, 0]);
        frame.extend_from_slice(&request_id.to_le_bytes());
        frame.extend_from_slice(&status.to_le_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(payload);
        let record = outer(
            OUTER_DATA,
            &self.instance,
            self.epoch,
            self.guest_sequence,
            &frame,
        );
        self.guest_sequence += 1;
        self.stream.write_all(&record)
    }

    /// Reads the next host request, returning `None` at end of stream.
    fn request(&mut self) -> io::Result<Option<(u8, u64, Vec<u8>)>> {
        let Some((header, frame)) = read_record(self.stream)? else {
            return Ok(None);
        };
        let valid = header[6] == OUTER_DATA
            && header[8..24] == self.instance
            && u64_at(&header, 24) == self.epoch
            && u64_at(&header, 32) == self.host_sequence
            && frame.len() >= APP_HEADER_LEN
            && &frame[..4] == b"NVXC"
            && frame[4] == 1
            && frame[6..8] == [0, 0]
            && u32_at(&frame, 20) as usize == frame.len() - APP_HEADER_LEN;
        if !valid {
            return Err(io::Error::other("invalid data record"));
        }
        self.host_sequence += 1;
        Ok(Some((
            frame[5],
            u64_at(&frame, 8),
            frame[APP_HEADER_LEN..].to_vec(),
        )))
    }

    fn run(&mut self, options: &Options, guest: &mut Guest) -> io::Result<Flow> {
        loop {
            let Some((kind, request_id, payload)) = self.request()? else {
                return Ok(Flow::Continue);
            };
            match kind {
                APP_PING => self.send(APP_READY, request_id, 0, &[])?,
                APP_FEATURES if !options.legacy_guest => {
                    self.send(APP_READY, request_id, 0, &GUEST_FEATURES.to_le_bytes())?;
                }
                APP_EXEC => {
                    if !self.exec(request_id, &payload, guest)? {
                        return Ok(Flow::Continue);
                    }
                }
                // The targeted workload already finished.
                APP_CANCEL => {}
                APP_STOP if options.ignore_stop => {}
                APP_STOP => {
                    let _ = self.send(APP_STOPPED, request_id, 0, &[]);
                    return Ok(Flow::Exit);
                }
                _ => self.send(APP_ERROR, request_id, 95, b"unsupported-operation")?,
            }
        }
    }

    /// Handles control traffic that arrives while workload `request_id` runs.
    fn interrupt(&mut self, request_id: u64) -> io::Result<Interrupt> {
        while self.stream.pending()? {
            match self.request()? {
                None => return Ok(Interrupt::Disconnected),
                Some((APP_CANCEL, id, _)) if id == request_id && !self.ignore_cancel => {
                    return Ok(Interrupt::Cancel);
                }
                Some((APP_CANCEL, _, _)) => {}
                Some((_, id, _)) => self.send(APP_ERROR, id, 16, b"busy")?,
            }
        }
        Ok(Interrupt::None)
    }

    /// Runs one workload. Returns `false` if the client disconnected and abandoned it.
    fn exec(&mut self, request_id: u64, payload: &[u8], guest: &mut Guest) -> io::Result<bool> {
        self.exec_script(request_id, payload, guest)
            .map(|outcome| outcome.is_some())
    }

    fn exec_script(
        &mut self,
        request_id: u64,
        payload: &[u8],
        guest: &mut Guest,
    ) -> io::Result<Option<()>> {
        let Some(argv) = decode_exec(payload) else {
            return self.send_some(APP_ERROR, request_id, 22, b"invalid-request");
        };
        let timeout_ms = u32_at(payload, 0);
        const PRELUDE: &str = "cd -- \"$1\" || exit 125\nshift\n";
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        let (words, cwd) = match words.as_slice() {
            ["/bin/sh", "-c", script, "/bin/sh", cwd, rest @ ..] if script.starts_with(PRELUDE) => {
                let script = &script[PRELUDE.len()..];
                let mut words = if script == "exec \"$@\"" {
                    rest.to_vec()
                } else {
                    vec!["/bin/sh", "-c", script]
                };
                if words.is_empty() {
                    words.push("/bin/true");
                }
                (words, (*cwd).to_owned())
            }
            _ => (words, "/".to_owned()),
        };
        let script = match words.as_slice() {
            ["/bin/sh", "-c", script] => (*script).to_owned(),
            ["/bin/echo", words @ ..] => format!("echo {}", words.join(" ")),
            [program, ..] => {
                let message = format!("aci-edge-sandboxes-fake-openvmm: {program}: not found\n");
                self.send(APP_STDERR, request_id, 0, message.as_bytes())?;
                return self.send_some(APP_EXIT, request_id, 127, b"exit");
            }
            [] => return self.send_some(APP_ERROR, request_id, 22, b"invalid-request"),
        };
        let started = Instant::now();
        let limit = (timeout_ms > 0).then(|| started + Duration::from_millis(timeout_ms.into()));
        let mut output = 0usize;
        for command in script
            .split(';')
            .map(str::trim)
            .filter(|command| !command.is_empty())
        {
            let (name, argument) = command
                .split_once(' ')
                .map_or((command, ""), |(name, argument)| (name, argument.trim()));
            match name {
                "echo" | "echoerr" => {
                    let line = format!("{argument}\n");
                    if output + line.len() > MAX_OUTPUT_BYTES {
                        return self.send_some(APP_EXIT, request_id, 125, b"output-limit");
                    }
                    output += line.len();
                    let kind = if name == "echo" {
                        APP_STDOUT
                    } else {
                        APP_STDERR
                    };
                    self.send(kind, request_id, 0, line.as_bytes())?;
                }
                "sleep" => {
                    let wake =
                        Instant::now() + Duration::from_millis(argument.parse().unwrap_or(0));
                    let end = limit.map_or(wake, |limit| wake.min(limit));
                    while Instant::now() < end {
                        match self.interrupt(request_id)? {
                            Interrupt::None => thread::sleep(
                                end.saturating_duration_since(Instant::now())
                                    .min(Duration::from_millis(5)),
                            ),
                            Interrupt::Cancel => {
                                return self.send_some(APP_EXIT, request_id, 137, b"cancelled");
                            }
                            Interrupt::Disconnected => return Ok(None),
                        }
                    }
                    if wake > end {
                        return self.send_some(APP_EXIT, request_id, 124, b"timeout");
                    }
                }
                "exit" => {
                    let status = argument.parse().unwrap_or(1);
                    return self.send_some(APP_EXIT, request_id, status, b"exit");
                }
                "signal" => {
                    let signal: i32 = argument.parse().unwrap_or(9);
                    return self.send_some(APP_EXIT, request_id, 128 + signal, b"signal");
                }
                "flood" => {
                    let mut remaining: usize = argument.parse().unwrap_or(0);
                    while remaining > 0 {
                        let chunk = remaining.min(OUTPUT_CHUNK_BYTES);
                        if output + chunk > MAX_OUTPUT_BYTES {
                            return self.send_some(APP_EXIT, request_id, 125, b"output-limit");
                        }
                        output += chunk;
                        remaining -= chunk;
                        self.send(APP_STDOUT, request_id, 0, &vec![b'x'; chunk])?;
                    }
                }
                "write" => {
                    let (key, value) = argument.split_once(' ').unwrap_or((argument, ""));
                    guest.values.insert(key.to_owned(), value.to_owned());
                }
                "read" => {
                    if let Some(value) = guest.values.get(argument).cloned() {
                        self.send(APP_STDOUT, request_id, 0, value.as_bytes())?;
                    }
                }
                "pwd" => {
                    let line = format!("{cwd}\n");
                    self.send(APP_STDOUT, request_id, 0, line.as_bytes())?;
                }
                "fail" => return self.send_some(APP_EXIT, request_id, 125, b"failed"),
                "launchfail" => {
                    return self.send_some(APP_ERROR, request_id, 125, b"launch-failed");
                }
                _ => {
                    let message =
                        format!("aci-edge-sandboxes-fake-openvmm: unknown command {name}\n");
                    self.send(APP_STDERR, request_id, 0, message.as_bytes())?;
                    return self.send_some(APP_EXIT, request_id, 127, b"exit");
                }
            }
        }
        self.send_some(APP_EXIT, request_id, 0, b"exit")
    }

    fn send_some(
        &mut self,
        kind: u8,
        request_id: u64,
        status: i32,
        payload: &[u8],
    ) -> io::Result<Option<()>> {
        self.send(kind, request_id, status, payload).map(Some)
    }
}

fn decode_exec(payload: &[u8]) -> Option<Vec<String>> {
    if payload.len() < 8 || payload[6..8] != [0, 0] {
        return None;
    }
    let count = usize::from(u16::from_le_bytes([payload[4], payload[5]]));
    let mut offset = 8;
    let mut argv = Vec::with_capacity(count);
    for _ in 0..count {
        let length = u32_at(payload.get(offset..offset + 4)?, 0) as usize;
        offset += 4;
        argv.push(String::from_utf8(payload.get(offset..offset + length)?.to_vec()).ok()?);
        offset += length;
    }
    (offset == payload.len() && !argv.is_empty()).then_some(argv)
}

/// Authenticates one client and serves it until it disconnects or stops the VM.
fn serve_client<S: Read + Write + Pending>(
    stream: &mut S,
    options: &Options,
    capability: &[u8; CAPABILITY_LEN],
    epoch: u64,
    guest: &mut Guest,
) -> Flow {
    let Ok(Some((header, payload))) = read_record(stream) else {
        return Flow::Continue;
    };
    if header[6] != OUTER_HOST_ATTACH || payload != capability {
        let _ = stream.write_all(&outer(OUTER_ERROR, &[0; 16], 0, 0, &[]));
        return Flow::Continue;
    }
    let mut instance = [0u8; 16];
    while instance == [0; 16] {
        if getrandom::fill(&mut instance).is_err() {
            return Flow::Continue;
        }
    }
    if stream
        .write_all(&outer(OUTER_READY, &instance, epoch, 0, &[]))
        .is_err()
    {
        return Flow::Continue;
    }
    let mut session = Session {
        stream,
        instance,
        epoch,
        guest_sequence: 1,
        host_sequence: 0,
        ignore_cancel: options.ignore_cancel,
    };
    match session.run(options, guest) {
        Ok(Flow::Exit) => {
            write_report(options);
            Flow::Exit
        }
        _ => Flow::Continue,
    }
}

fn write_report(options: &Options) {
    let report = serde_json::json!({
        "schema_version": 1,
        "backend": options.hypervisor,
        "outcome": { "operation": "managed", "category": "success", "status_code": 0 },
        "network_policy": { "status": "not-requested" },
        "teardown": { "workers": true, "memory": true },
    });
    if let Ok(mut file) = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&options.report)
    {
        let _ = file.write_all(report.to_string().as_bytes());
    }
}

#[cfg(unix)]
fn serve(options: &Options, capability: &[u8; CAPABILITY_LEN]) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    let listener = UnixListener::bind(&options.endpoint)?;
    fs::set_permissions(&options.endpoint, fs::Permissions::from_mode(0o600))?;
    let mut guest = Guest::default();
    let mut epoch = 0;
    for stream in listener.incoming() {
        let mut stream = stream?;
        epoch += 1;
        if let Flow::Exit = serve_client(&mut stream, options, capability, epoch, &mut guest) {
            let _ = fs::remove_file(&options.endpoint);
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn serve(options: &Options, capability: &[u8; CAPABILITY_LEN]) -> io::Result<()> {
    use std::fs::File;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

    use windows_sys::Win32::Foundation::{
        ERROR_NO_DATA, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    let name: Vec<u16> = std::ffi::OsStr::new(&options.endpoint.replace('/', "\\"))
        .encode_wide()
        .chain([0])
        .collect();
    // Like OpenVMM, serve every client on one pipe instance, so the endpoint exists for as long
    // as the process runs.
    // SAFETY: the name is NUL-terminated and default security is allowed.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            65_536,
            65_536,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateNamedPipeW returned a fresh handle that nothing else owns.
    let mut file = File::from(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) });
    let pipe = file.as_raw_handle() as HANDLE;
    let mut guest = Guest::default();
    let mut epoch = 0;
    loop {
        // SAFETY: the handle is a pipe server instance; a null OVERLAPPED waits synchronously.
        if unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) } == 0 {
            // SAFETY: GetLastError has no preconditions.
            match unsafe { GetLastError() } {
                ERROR_PIPE_CONNECTED => {}
                // The client left before the connection was accepted.
                ERROR_NO_DATA => {
                    // SAFETY: the handle is a pipe server instance.
                    unsafe { DisconnectNamedPipe(pipe) };
                    continue;
                }
                error => return Err(io::Error::from_raw_os_error(error as i32)),
            }
        }
        epoch += 1;
        let flow = serve_client(&mut file, options, capability, epoch, &mut guest);
        // Wait until the client has read every reply: disconnecting discards unread data.
        let _ = file.sync_all();
        // SAFETY: the handle is a connected pipe server instance.
        unsafe { DisconnectNamedPipe(pipe) };
        if let Flow::Exit = flow {
            return Ok(());
        }
    }
}
