//! Test double for the `openvmm` executable.
//!
//! It emulates the part of OpenVMM that the `openvmm` backend relies on, without running a VM:
//! it validates the managed microVM command line, reads the launch capability from standard
//! input, serves the authenticated control console on the requested Unix socket or named pipe,
//! and runs scripted workloads. Integration tests point `OpenVmmConfig::openvmm` at it.
//!
//! Workloads are `/bin/sh -c SCRIPT`, `/bin/echo ARGS...`, `/bin/pwd`, or a program named `env`
//! or `printenv` in any directory, which prints its environment. A script is a `;`-separated
//! list of commands: `echo TEXT`, `echoerr TEXT`, `sleep MS`, `exit CODE`, `signal NUMBER`,
//! `flood BYTES`, `write KEY VALUE`, `read KEY`, `mkdir DIRECTORY`, `env`, `printenv NAME`,
//! `pwd`, `timeout`, `fail`, and `launchfail`. Like `sh`, a script exits with the status of its
//! last command; `printenv NAME` prints nothing and fails with status 1 when `NAME` is unset. Values written with `write` and directories made with
//! `mkdir` live in memory until the VM stops, like files in the guest's RAM root file system.
//! A `CANCEL` request ends a sleeping workload with the cancelled outcome, and a client that
//! disconnects during an exec abandons it, as the real guest agent does. `pwd` prints the
//! working directory of the exec request, and `timeout` its timeout in milliseconds. Like the
//! guest agent, the fake refuses a working directory that does not exist (any but `/`, `/tmp`,
//! `/work`, a mapped directory, or a directory made with `mkdir`), or that is a mapped regular
//! file, with a diagnostic and the `cwd-failed` category.
//!
//! Host paths are mapped as OpenVMM and the guest agent map them: the command line exports
//! numbered `--mount-child` directories through `--mount-aggregate`, the kernel command line
//! announces the number of mappings with `nvx_maps=`, and `MAPS` requests deliver the mapping
//! table in order. The fake checks each entry against the exported directories, refuses
//! workloads until the table is complete, and records it in `fake-openvmm-<token>-maps.json`
//! next to the kernel.
//!
//! Each workload gets the environment that its exec request selects, in the order in which the
//! guest agent builds it. The default environment is the documented guest bootstrap environment:
//! `PATH`, `TERM`, and the `HOME`, `USER`, and `LOGNAME` of the workload account. The guest's
//! boot leaves a few more variables behind, and the guest agent points `PWD` at the working
//! directory; the fake omits both. Like BusyBox's `sh`, a script exports `SHLVL`, one higher
//! than the value that it received as an unsigned 32-bit integer that wraps around, and its
//! working directory as `PWD`, replacing entries with these names.
//!
//! Kernel command-line tokens adjust the emulation: `fake_exit_on_start=CODE` fails the launch,
//! `fake_boot_delay_ms=MS` delays the control endpoint, `fake_crash_after_ms=MS` makes the VM
//! die, `fake_ignore_stop=1` ignores stop requests, `fake_ignore_cancel=1` ignores cancellation,
//! `fake_guest_features=MASK` advertises the decimal feature mask `MASK` instead of the current
//! guest's, `fake_legacy_guest=1` refuses the features request like a guest agent that
//! predates it, and `fake_refuse_maps=1` fails every `MAPS` request like a guest that cannot
//! mount a mapped path. The command line is recorded in `fake-openvmm-<token>.json` next to the
//! kernel.

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
const APP_MAPS: u8 = 6;
const APP_READY: u8 = 0x81;
const APP_STDOUT: u8 = 0x82;
const APP_STDERR: u8 = 0x83;
const APP_EXIT: u8 = 0x84;
const APP_STOPPED: u8 = 0x85;
const APP_ERROR: u8 = 0xff;

/// Control features of the current guest: cancellation, workload accounts, workload
/// containment, per-execution environments, working directories, and host path mapping tables.
/// Bit 1 is retired.
const GUEST_FEATURES: u32 = 0b111_1101;

const EXEC_EXTENDED: u16 = 1;
const EXEC_CWD_PRESENT: u16 = 1 << 0;
const EXEC_ENVIRONMENT_PRESENT: u16 = 1 << 1;
const EXEC_INHERIT_DEFAULT_ENV: u16 = 1 << 2;
const MAX_ARGUMENT_BYTES: usize = 4096;
const MAX_ENVIRONMENT: usize = 256;
const MAX_HOST_MAPPINGS: usize = 4096;
const MAP_READ_ONLY: u16 = 1;
/// The guest directory at which the backend mounts the aggregate export.
const GUEST_EXPORT: &str = "/run/nvx/hostfs/root";

/// `NAME=VALUE` variables in the order in which a workload's `environ` lists them.
type Environment = Vec<(String, String)>;

/// Sets `name` like `putenv`: in place when it exists, and at the end otherwise.
fn set_variable(environment: &mut Environment, name: &str, value: &str) {
    match environment
        .iter_mut()
        .find(|(existing, _)| existing == name)
    {
        Some(entry) => value.clone_into(&mut entry.1),
        None => environment.push((name.to_owned(), value.to_owned())),
    }
}

/// What BusyBox's `sh` does to the environment of everything that it starts: it raises `SHLVL`
/// and exports its working directory as `PWD`, replacing the values of the same names that it
/// received. It keeps every other variable.
fn start_shell(environment: &mut Environment, cwd: &str) {
    let level = environment
        .iter()
        .find(|(name, _)| name == "SHLVL")
        .map_or(0, |(_, value)| atoi(value));
    // BusyBox exports `atoi(SHLVL) + 1` as an unsigned integer, so `4294967295` and `-1`
    // become `0`, and `-3` becomes `4294967294`.
    set_variable(
        environment,
        "SHLVL",
        &level.wrapping_add(1).cast_unsigned().to_string(),
    );
    set_variable(environment, "PWD", cwd);
}

/// musl's `atoi`, which the guest's BusyBox uses: it skips leading whitespace, takes an
/// optional sign and the digits that follow, wraps around on overflow, and stops at the first
/// other character, so `7x` is 7 and a value without digits is 0.
fn atoi(value: &str) -> i32 {
    let value = value.trim_start_matches([' ', '\t', '\n', '\u{b}', '\u{c}', '\r']);
    let (negative, digits) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let magnitude = digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0i32, |n, digit| {
            n.wrapping_mul(10).wrapping_add(i32::from(digit - b'0'))
        });
    if negative {
        magnitude.wrapping_neg()
    } else {
        magnitude
    }
}

struct Options {
    endpoint: String,
    report: PathBuf,
    hypervisor: String,
    args_dump: PathBuf,
    maps_dump: PathBuf,
    exit_on_start: Option<u8>,
    boot_delay: Duration,
    crash_after: Option<Duration>,
    ignore_stop: bool,
    ignore_cancel: bool,
    legacy_guest: bool,
    refuse_maps: bool,
    workload_uid: u32,
    guest_features: u32,
    /// Host directories of the aggregate export, in child order.
    children: Vec<PathBuf>,
    /// Number of host mappings that the kernel command line announces.
    maps: usize,
}

/// A host path mapped into the guest.
#[derive(serde::Serialize)]
struct Mapped {
    /// Path relative to the guest's mount of the export.
    source: String,
    /// Guest path at which the host path appears.
    target: String,
    /// Whether the guest mounts it read-only.
    read_only: bool,
    /// Whether the host path is a directory rather than a regular file.
    directory: bool,
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
    const VALUED: [&str; 23] = [
        "--mount-aggregate",
        "--mount-child",
        "--mount-deny",
        "--mount-allow",
        "--mount-write",
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
    let workload_uid = identity
        .split_once(':')
        .and_then(|(uid, gid)| Some((uid.parse::<u32>().ok()?, gid.parse::<u32>().ok()?)))
        .filter(|&(uid, gid)| uid != 0 && gid != 0)
        .map(|(uid, _)| uid)
        .ok_or_else(|| format!("invalid workload identity {identity}"))?;
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
            && !token.starts_with("nvx_maps=")
            && *token != "nvx_workload_account=create"
    }) {
        return Err("the kernel command line must not select a sandbox".to_owned());
    }
    let maps = match tokens
        .iter()
        .filter_map(|token| token.strip_prefix("nvx_maps="))
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => 0,
        [count] => count
            .parse::<usize>()
            .ok()
            .filter(|count| (1..=MAX_HOST_MAPPINGS).contains(count))
            .ok_or_else(|| format!("invalid nvx_maps={count}"))?,
        _ => return Err("nvx_maps= must be given at most once".to_owned()),
    };
    let children = match values.get("--mount-aggregate").map(Vec::as_slice) {
        None if maps == 0 => Vec::new(),
        Some([GUEST_EXPORT]) if maps > 0 => {
            let mut children = Vec::new();
            for (index, child) in values
                .get("--mount-child")
                .into_iter()
                .flatten()
                .enumerate()
            {
                let invalid = || format!("invalid --mount-child {child}");
                let (name, rest) = child.split_once(',').ok_or_else(invalid)?;
                let (host, mode) = rest.rsplit_once(',').ok_or_else(invalid)?;
                if name != index.to_string()
                    || !Path::new(host).is_dir()
                    || !["ro", "rw"].contains(&mode)
                {
                    return Err(invalid());
                }
                children.push(PathBuf::from(host));
            }
            if children.is_empty() {
                return Err("--mount-aggregate requires --mount-child".to_owned());
            }
            children
        }
        _ => return Err("--mount-aggregate and nvx_maps= must agree".to_owned()),
    };
    if children.is_empty() && values.contains_key("--mount-child") {
        return Err("--mount-child requires --mount-aggregate".to_owned());
    }
    for option in ["--mount-deny", "--mount-allow", "--mount-write"] {
        for path in values.get(option).into_iter().flatten() {
            let path = Path::new(path);
            if !path.exists() || !children.iter().any(|root| path.starts_with(root)) {
                return Err(format!("invalid {option} {}", path.display()));
            }
        }
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
        maps_dump: kernel.with_file_name(format!("fake-openvmm-{token}-maps.json")),
        report,
        hypervisor: hypervisor.to_owned(),
        exit_on_start: knob("fake_exit_on_start").and_then(|value| value.parse().ok()),
        boot_delay: millis("fake_boot_delay_ms").unwrap_or_default(),
        crash_after: millis("fake_crash_after_ms"),
        ignore_stop: knob("fake_ignore_stop") == Some("1"),
        ignore_cancel: knob("fake_ignore_cancel") == Some("1"),
        legacy_guest: knob("fake_legacy_guest") == Some("1"),
        refuse_maps: knob("fake_refuse_maps") == Some("1"),
        workload_uid,
        guest_features: knob("fake_guest_features")
            .and_then(|value| value.parse().ok())
            .unwrap_or(GUEST_FEATURES),
        children,
        maps,
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
struct Guest {
    values: BTreeMap<String, String>,
    /// Environment that workloads inherit unless they replace it.
    default_environment: Environment,
    /// Directories a workload can enter.
    directories: BTreeSet<String>,
    /// Mapped regular files, which a workload cannot enter either.
    files: BTreeSet<String>,
    /// Entries of the host mapping table mounted so far.
    mapped: Vec<Mapped>,
}

impl Guest {
    fn new(options: &Options) -> Self {
        // The Alpine image resolves the default identity to `nobody`; for any other host-selected
        // identity, the managed init creates the `nvx` account homed in /tmp.
        let (user, home) = if options.workload_uid == 65534 {
            ("nobody", "/")
        } else {
            ("nvx", "/tmp")
        };
        let default_environment = [
            ("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"),
            ("TERM", "linux"),
            ("HOME", home),
            ("USER", user),
            ("LOGNAME", user),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
        Self {
            values: BTreeMap::new(),
            default_environment,
            directories: ["/", "/tmp", "/work"].map(str::to_owned).into(),
            files: BTreeSet::new(),
            mapped: Vec::new(),
        }
    }

    /// Mounts the entries of a `MAPS` request, like the guest agent: in order, and only as many
    /// as the kernel command line announced. Returns the error number and category of a refusal.
    fn map(&mut self, options: &Options, payload: &[u8]) -> Result<(), (i32, &'static str)> {
        const INVALID: (i32, &str) = (22, "invalid-request");
        let header = |offset: usize| {
            payload
                .get(offset..offset + 4)
                .map(|bytes| u32_at(bytes, 0))
        };
        let (Some(first), Some(count)) = (header(0), header(4)) else {
            return Err(INVALID);
        };
        let (first, count) = (first as usize, count as usize);
        if options.maps == 0
            || first != self.mapped.len()
            || count == 0
            || count > options.maps - first
        {
            return Err(INVALID);
        }
        let field = |offset: usize| {
            payload
                .get(offset..offset + 2)
                .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        };
        let mut entries = Vec::with_capacity(count);
        let mut offset = 8;
        for _ in 0..count {
            let (Some(flags), Some(source_len), Some(target_len)) =
                (field(offset), field(offset + 2), field(offset + 4))
            else {
                return Err(INVALID);
            };
            offset += 6;
            let text = |offset: usize, length: u16| {
                payload
                    .get(offset..offset + usize::from(length))
                    .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
            };
            let (Some(source), Some(target)) = (
                text(offset, source_len),
                text(offset + usize::from(source_len), target_len),
            ) else {
                return Err(INVALID);
            };
            offset += usize::from(source_len) + usize::from(target_len);
            if flags & !MAP_READ_ONLY != 0 || !target.starts_with('/') || target.contains('\0') {
                return Err(INVALID);
            }
            entries.push((source, target, flags & MAP_READ_ONLY != 0));
        }
        if offset != payload.len() {
            return Err(INVALID);
        }
        if options.refuse_maps {
            return Err((13, "mapping-failed"));
        }
        for (source, target, read_only) in entries {
            let (child, relative) = source.split_once('/').unwrap_or((&source, ""));
            let Some(root) = child
                .parse::<usize>()
                .ok()
                .and_then(|child| options.children.get(child))
            else {
                return Err(INVALID);
            };
            let host = if relative.is_empty() {
                root.clone()
            } else {
                root.join(relative)
            };
            let directory = if host.is_dir() {
                self.directories.insert(target.clone());
                true
            } else if host.is_file() {
                self.files.insert(target.clone());
                false
            } else {
                return Err((2, "mapping-failed"));
            };
            self.mapped.push(Mapped {
                source,
                target,
                read_only,
                directory,
            });
        }
        let _ = fs::write(
            &options.maps_dump,
            serde_json::to_vec(&self.mapped).unwrap_or_default(),
        );
        Ok(())
    }
}

struct Exec {
    argv: Vec<String>,
    cwd: Option<String>,
    environment: Option<Vec<String>>,
    inherit_default_env: bool,
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
                    self.send(
                        APP_READY,
                        request_id,
                        0,
                        &options.guest_features.to_le_bytes(),
                    )?;
                }
                APP_EXEC if guest.mapped.len() < options.maps => {
                    self.send(APP_ERROR, request_id, 11, b"mappings-incomplete")?;
                }
                APP_EXEC => {
                    if !self.exec(request_id, &payload, guest)? {
                        return Ok(Flow::Continue);
                    }
                }
                APP_MAPS => match guest.map(options, &payload) {
                    Ok(()) => self.send(APP_READY, request_id, 0, &[])?,
                    Err((status, category)) => {
                        self.send(APP_ERROR, request_id, status, category.as_bytes())?;
                    }
                },
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
        let Some(exec) = decode_exec(payload) else {
            return self.send_some(APP_ERROR, request_id, 22, b"invalid-request");
        };
        let mut environment = if exec.inherit_default_env {
            guest.default_environment.clone()
        } else {
            Environment::new()
        };
        for entry in exec.environment.iter().flatten() {
            let (name, value) = entry.split_once('=').unwrap();
            set_variable(&mut environment, name, value);
        }
        let cwd = exec.cwd.as_deref().unwrap_or("/");
        if !guest.directories.contains(cwd) {
            // The guest agent's refusal: a diagnostic, then the error number, before anything
            // runs.
            let (error, reason) = if guest.files.contains(cwd) {
                (20, "Not a directory") // ENOTDIR
            } else {
                (2, "No such file or directory") // ENOENT
            };
            let message =
                format!("nvx-managed-agent: cannot enter working directory {cwd}: {reason}\n");
            self.send(APP_STDERR, request_id, 0, message.as_bytes())?;
            return self.send_some(APP_ERROR, request_id, error, b"cwd-failed");
        }
        let timeout_ms = u32_at(payload, 0);
        let program_name =
            |program: &str| program.rsplit('/').next().unwrap_or_default().to_owned();
        let script = match exec.argv.as_slice() {
            [shell, option, script] if shell == "/bin/sh" && option == "-c" => {
                start_shell(&mut environment, cwd);
                script.clone()
            }
            [program, words @ ..] if program == "/bin/echo" => format!("echo {}", words.join(" ")),
            [program] if program == "/bin/pwd" => "pwd".to_owned(),
            [program] if ["env", "printenv"].contains(&program_name(program).as_str()) => {
                "env".to_owned()
            }
            [program, name] if program_name(program) == "printenv" => format!("printenv {name}"),
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
        let mut status = 0;
        for command in script
            .split(';')
            .map(str::trim)
            .filter(|command| !command.is_empty())
        {
            let (name, argument) = command
                .split_once(' ')
                .map_or((command, ""), |(name, argument)| (name, argument.trim()));
            status = 0;
            match name {
                "echo" | "echoerr" => {
                    let line = format!("{argument}\n");
                    let kind = if name == "echo" {
                        APP_STDOUT
                    } else {
                        APP_STDERR
                    };
                    if !self.forward(kind, request_id, &mut output, line.as_bytes())? {
                        return self.send_some(APP_EXIT, request_id, 125, b"output-limit");
                    }
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
                        if !self.forward(APP_STDOUT, request_id, &mut output, &vec![b'x'; chunk])? {
                            return self.send_some(APP_EXIT, request_id, 125, b"output-limit");
                        }
                        remaining -= chunk;
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
                "mkdir" => {
                    guest.directories.insert(argument.to_owned());
                }
                "env" => {
                    for (name, value) in &environment {
                        let line = format!("{name}={value}\n");
                        if !self.forward(APP_STDOUT, request_id, &mut output, line.as_bytes())? {
                            return self.send_some(APP_EXIT, request_id, 125, b"output-limit");
                        }
                    }
                }
                "printenv" => {
                    let value = environment
                        .iter()
                        .find(|(variable, _)| variable == argument)
                        .map(|(_, value)| format!("{value}\n"));
                    match value {
                        Some(line) => {
                            if !self.forward(
                                APP_STDOUT,
                                request_id,
                                &mut output,
                                line.as_bytes(),
                            )? {
                                return self.send_some(APP_EXIT, request_id, 125, b"output-limit");
                            }
                        }
                        None => status = 1,
                    }
                }
                "pwd" => {
                    let line = format!("{cwd}\n");
                    self.send(APP_STDOUT, request_id, 0, line.as_bytes())?;
                }
                "timeout" => {
                    let line = format!("{timeout_ms}\n");
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
        self.send_some(APP_EXIT, request_id, status, b"exit")
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

    /// Forwards workload output unless it would exceed the guest agent's output limit, and
    /// returns whether it did. A workload whose output was refused ends with `output-limit`.
    fn forward(
        &mut self,
        kind: u8,
        request_id: u64,
        forwarded: &mut usize,
        output: &[u8],
    ) -> io::Result<bool> {
        if *forwarded + output.len() > MAX_OUTPUT_BYTES {
            return Ok(false);
        }
        *forwarded += output.len();
        self.send(kind, request_id, 0, output)?;
        Ok(true)
    }
}

/// Reads the `length` bytes at `offset` as text. Like the guest agent, refuses a NUL byte, which
/// it rejects in every argument, working directory, and environment entry.
fn text_at(payload: &[u8], offset: usize, length: usize) -> Option<String> {
    let bytes = payload.get(offset..offset + length)?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes.to_vec()).ok()
}

fn decode_exec(payload: &[u8]) -> Option<Exec> {
    if payload.len() < 8 {
        return None;
    }
    let extension = u16::from_le_bytes(payload[6..8].try_into().ok()?);
    let count = usize::from(u16::from_le_bytes([payload[4], payload[5]]));
    // Like the guest agent, accept only flags that it knows, in combinations that make sense.
    let (flags, environment_count, cwd_len, mut offset) = match extension {
        0 => (0, 0, 0, 8),
        EXEC_EXTENDED if payload.len() >= 16 => {
            let flags = u16::from_le_bytes(payload[8..10].try_into().ok()?);
            let environment_count =
                usize::from(u16::from_le_bytes(payload[10..12].try_into().ok()?));
            let cwd_len = u32_at(payload, 12) as usize;
            let known = EXEC_CWD_PRESENT | EXEC_ENVIRONMENT_PRESENT | EXEC_INHERIT_DEFAULT_ENV;
            let cwd_valid = if flags & EXEC_CWD_PRESENT == 0 {
                cwd_len == 0
            } else {
                (1..=MAX_ARGUMENT_BYTES).contains(&cwd_len)
            };
            let environment_valid = flags & EXEC_ENVIRONMENT_PRESENT != 0
                || (environment_count == 0 && flags & EXEC_INHERIT_DEFAULT_ENV == 0);
            if flags & !known != 0
                || !cwd_valid
                || !environment_valid
                || environment_count > MAX_ENVIRONMENT
            {
                return None;
            }
            (flags, environment_count, cwd_len, 16)
        }
        _ => return None,
    };
    let mut argv = Vec::with_capacity(count);
    for _ in 0..count {
        let length = u32_at(payload.get(offset..offset + 4)?, 0) as usize;
        offset += 4;
        if length == 0 || length > MAX_ARGUMENT_BYTES {
            return None;
        }
        argv.push(text_at(payload, offset, length)?);
        offset += length;
    }
    let cwd = if flags & EXEC_CWD_PRESENT == 0 {
        None
    } else {
        let cwd = text_at(payload, offset, cwd_len)?;
        offset += cwd_len;
        if !cwd.starts_with('/') {
            return None;
        }
        Some(cwd)
    };
    let mut environment = Vec::with_capacity(environment_count);
    let mut names = BTreeSet::new();
    for _ in 0..environment_count {
        let length = u32_at(payload.get(offset..offset + 4)?, 0) as usize;
        offset += 4;
        if length == 0 || length > MAX_ARGUMENT_BYTES {
            return None;
        }
        let entry = text_at(payload, offset, length)?;
        offset += length;
        // Like the guest agent, refuse entries without a name and repeated names.
        let (name, _) = entry.split_once('=').filter(|(name, _)| !name.is_empty())?;
        if !names.insert(name.to_owned()) {
            return None;
        }
        environment.push(entry);
    }
    (offset == payload.len() && !argv.is_empty()).then_some(Exec {
        argv,
        cwd,
        environment: (flags & EXEC_ENVIRONMENT_PRESENT != 0).then_some(environment),
        inherit_default_env: flags & EXEC_ENVIRONMENT_PRESENT == 0
            || flags & EXEC_INHERIT_DEFAULT_ENV != 0,
    })
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
    let mut guest = Guest::new(options);
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
    let mut guest = Guest::new(options);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes an extended exec request with an explicit environment.
    fn extended(argv: &[&str], cwd: Option<&str>, environment: &[&str]) -> Vec<u8> {
        let length = |text: &str| u32::try_from(text.len()).unwrap().to_le_bytes();
        let mut flags = EXEC_ENVIRONMENT_PRESENT;
        if cwd.is_some() {
            flags |= EXEC_CWD_PRESENT;
        }
        let mut payload = 0u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&u16::try_from(argv.len()).unwrap().to_le_bytes());
        payload.extend_from_slice(&EXEC_EXTENDED.to_le_bytes());
        payload.extend_from_slice(&flags.to_le_bytes());
        payload.extend_from_slice(&u16::try_from(environment.len()).unwrap().to_le_bytes());
        payload.extend_from_slice(&length(cwd.unwrap_or_default()));
        for argument in argv {
            payload.extend_from_slice(&length(argument));
            payload.extend_from_slice(argument.as_bytes());
        }
        payload.extend_from_slice(cwd.unwrap_or_default().as_bytes());
        for entry in environment {
            payload.extend_from_slice(&length(entry));
            payload.extend_from_slice(entry.as_bytes());
        }
        payload
    }

    #[test]
    fn exec_requests_are_refused_where_the_guest_agent_refuses_them() {
        let exec = decode_exec(&extended(
            &["/usr/bin/env", "-0"],
            Some("/tmp"),
            &["FOO=bar", "EMPTY="],
        ))
        .unwrap();
        assert_eq!(exec.argv, ["/usr/bin/env", "-0"]);
        assert_eq!(exec.cwd.as_deref(), Some("/tmp"));
        assert_eq!(
            exec.environment,
            Some(vec!["FOO=bar".to_owned(), "EMPTY=".to_owned()])
        );
        assert!(!exec.inherit_default_env);
        for payload in [
            extended(&["/usr/bin/env"], None, &["FOO=b\0r"]),
            extended(&["/usr/bin/env"], None, &["F\0O=bar"]),
            extended(&["/usr/bin/e\0nv"], None, &[]),
            extended(&["/usr/bin/env", "a\0b"], None, &[]),
            extended(&["/usr/bin/env"], Some("/t\0mp"), &[]),
            extended(&["/usr/bin/env"], Some("tmp"), &[]),
            extended(&["/usr/bin/env"], None, &["FOO=1", "FOO=2"]),
            extended(&["/usr/bin/env"], None, &["NOVALUE"]),
            extended(&["/usr/bin/env"], None, &["=value"]),
        ] {
            assert!(decode_exec(&payload).is_none(), "{payload:?}");
        }
    }
}
