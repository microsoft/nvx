//! Test double for the `openvmm` executable.
//!
//! It emulates the part of OpenVMM that NVX hosts rely on, without running a VM: it validates the
//! managed microVM command line, reads the launch capability from standard input, serves the
//! authenticated control console on the requested Unix socket or named pipe, and runs scripted
//! workloads. Tests point the OpenVMM path of their launches at an executable whose `main` calls
//! [`main`], such as `nvx-fake-openvmm`. It speaks the [`protocol`](crate::protocol) of the host
//! and decodes requests with the guest agent's decoders, whose conformance vectors pin the wire
//! format independently.
//!
//! On Unix, as OpenVMM does, the fake refuses a control console unless its socket's directory is a
//! directory, not a link, that belongs to the current user, with exactly the mode 0700, and, if
//! the command line names a snapshot, the canonical directory that holds it: the one that
//! `--restore-snapshot` restores, or else the destination of `--snapshot-destination`.
//!
//! Workloads are `/bin/sh -c SCRIPT`, `/bin/echo ARGS...`, `/bin/pwd`, or a program named `env`
//! or `printenv` in any directory, which prints its environment. A script is a `;`-separated
//! list of commands: `echo TEXT`, `echoerr TEXT`, `printerr TEXT`, which writes `TEXT` to
//! standard error without a line end, `sleep MS`, `exit CODE`, `signal NUMBER`, `flood BYTES`,
//! `write KEY VALUE`, `read KEY`, `mkdir DIRECTORY`, `touch PATH`, which creates the host file
//! `PATH`, as a workload that writes a read-write share does, `env`, `printenv NAME`, `pwd`,
//! `timeout`, `fail`, and `launchfail`. Like `sh`, a script exits with the status of its
//! last command; `printenv NAME` prints nothing and fails with status 1 when `NAME` is unset.
//! Values written with `write` and directories made with `mkdir` live in memory until the VM
//! stops, like files in the guest's RAM root file system. A `CANCEL` request ends a sleeping
//! workload with the cancelled outcome, and a client that disconnects during an exec abandons it,
//! as the real guest agent does. `pwd` prints the working directory of the exec request, and
//! `timeout` its timeout in milliseconds. Like the guest agent, the fake refuses a working
//! directory that does not exist (any but `/`, `/tmp`, `/work`, a mapped directory, or a directory
//! made with `mkdir`), or that is a mapped regular file, with a diagnostic and the `cwd-failed`
//! category.
//!
//! Host paths are mapped as OpenVMM and the guest agent map them: the command line exports
//! numbered `--mount-child` directories, and files with the `file` flag, through
//! `--mount-aggregate`, the kernel command line announces the number of mappings with
//! `nvx_maps=`, and `MAPS` requests deliver the mapping table in order. The fake checks each
//! entry against the exported children, refuses workloads until the table is complete, and
//! records it in `fake-openvmm-<token>-maps.json` next to the kernel, where `<token>` is the name
//! of the directory that holds the outcome report.
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
//! die, `fake_ignore_stop=1` ignores stop requests, `fake_exit_delay_ms=MS` delays the exit after
//! an acknowledged stop, `fake_ignore_cancel=1` ignores cancellation,
//! `fake_guest_features=MASK` advertises the decimal feature mask `MASK` instead of the current
//! guest's, `fake_legacy_guest=1` refuses the features request like a guest agent that
//! predates it, and `fake_refuse_maps=1` fails every `MAPS` request like a guest that cannot
//! mount a mapped path. The command line is recorded in `fake-openvmm-<token>.json` next to the
//! kernel, and each connection to the control endpoint that the fake accepts adds a line to
//! `fake-openvmm-<token>-sessions.log` there, so that a test can tell whether a host connected.
//!
//! The fake also serves the sandbox profile of `nvx-rs sandbox`, which a
//! `--microvm-sandbox-block` option selects: read-only `distro`, `runtime`, and `custom` layers
//! and a `scratch` disk, each an existing file, with `nvx_sandbox=1`, `nvx_entrypoint=`, and
//! `nvx_hostname=` on the kernel command line, or, for a RAM-backed overlay, which one
//! `nvx_overlay_upper=ramfs` there requests, as OpenVMM reads it, the `distro` layer alone. Like
//! OpenVMM, it opens each block device at the
//! path that its option names, a layer read-only and the scratch disk for reading and writing,
//! and holds them until it exits; it reads them whole, and records what each held, with its role
//! and path, in `fake-openvmm-<token>-blocks.json` next to the kernel. The
//! `fake_block_delay_ms=MS` token delays the opens. Its guest advertises the features of the
//! sandbox agent, [`GuestFeatures::SANDBOX_AGENT`]. With `--microvm-lifecycle managed`, it serves
//! the control console as above, and maps the sandbox's shares as above too: from the children of
//! the aggregate export at `/run/nvx/hostfs/root`, as many as `nvx_maps=` announces, which `MAPS`
//! requests deliver; it refuses a `--mount` share and `nvx_share=` tokens. With
//! `--microvm-lifecycle one-shot`, it takes the shares through one `--mount` or the children of
//! an aggregate at `/run/nvx/shares`, which `nvx_share=` tokens place, and whose guest targets of
//! directory shares are directories that a workload can enter; it reads no capability and
//! serves no console: it runs the entrypoint and arguments of the kernel command line with its
//! own standard streams, `/bin/echo` printing its arguments, `/bin/sleep` sleeping for its
//! argument in seconds, `/bin/true` and `/bin/false` exiting with 0 and 1, and any other program
//! not found with 127, then writes an outcome report
//! of operation `run` if `--microvm-report` asks for one, and exits with the workload's status,
//! or with `STATUS` if the kernel command line has `fake_exit_status=STATUS`.
//!
//! The fake also runs the microVMs of `nvx-rs run`, whose command lines have
//! neither a `--microvm-lifecycle` nor a sandbox block device, on the terminal. Such a command
//! line takes `--processors` and at most one `--cpu-profile`, shares through one `--mount` or the
//! children of an aggregate at `/run/nvx/shares`, whose relative policy paths name paths in the
//! only share, as OpenVMM resolves them, and either boots a guest, with `--memory`, an optional
//! `--memory-capacity`, and an existing `--kernel` and `--initrd`, or restores a snapshot, with
//! `--restore-snapshot` and optional `--restore-processors`, `--restore-memory`, and
//! `--restore-ready-path`. It runs no guest: after `fake_boot_delay_ms=MS`, it prints that it
//! booted or restored the microVM on standard output, writes an outcome report of operation `run`
//! if `--microvm-report` asks for one, and exits with 0, or with `STATUS` if the kernel command
//! line, which such a command line may omit, has `fake_exit_status=STATUS`. A restore whose
//! snapshot directory does not exist fails with status 1 and writes no report. A restore records
//! its command line next to its snapshot, since it has no kernel. The outcome reports of sandboxes
//! and of these runs take the form of OpenVMM's.
//!
//! The fake also runs the VMs of templates' captures, whose command lines have no
//! `--microvm-lifecycle`, a `distro` layer and a `scratch` disk on sandbox block devices and no
//! share, `--snapshot-destination`, `--snapshot-tier platform`,
//! `--snapshot-block-identity generation` with a nonzero `--snapshot-generation-id` of 32
//! hexadecimal digits, the options of a cold boot of a run with a `--memory-capacity` that is a
//! multiple of 128 MiB, `nvx_sandbox=1` on the kernel command line, alone or followed by a
//! canonical `maxcpus=` of 1, 2, 4, or 8 of its `--processors`, `--virtio-console none`,
//! the control console with its capability on standard input, and an outcome report. On Unix, the
//! control console's socket must be in the directory of the snapshot's destination, as above. The
//! fake serves no control session of a capture, which the host opens none of. Like OpenVMM, it
//! opens each block device itself, at its path, without following a link, and on Windows sharing
//! only reads and deletion, so that its open fails while another handle may write the file; on
//! Linux, a read lease, which no process may take of a file that a handle holds open for writing,
//! finds such a handle. It records each device's role, path, and length, and whether it found a
//! writer, in the blocks dump. It then backs the RAM with a sparse file beside the destination, and
//! does what `fake-openvmm-capture.json` next to the kernel asks, a JSON object of these fields,
//! each optional but `manifest`:
//!
//! - `manifest`: the manifest to publish, which the fake copies, since it reads none;
//! - `stateLen`: the length of the saved state to publish, 0 by default;
//! - `outcome`: `publish`, the default, which stages the snapshot beside the destination, with the
//!   RAM as a hard link to the file that backed it, renames it to the destination, and reports
//!   the capture's success; `fail`, a guest that fails with `status` before it captures;
//!   `publish-then-fail`, a VMM that publishes the snapshot and then fails to tear the VM down,
//!   which reports `teardown-failure` and exits with 1; `hang` and `publish-then-hang`, a VM that
//!   never ends; and `crash`, a VMM that exits with 3, without a report;
//! - `status`: the guest's status of `fail`, 1 by default;
//! - `active`: a file that the fake creates as it starts, failing if it exists, which holds its
//!   process ID, and which it removes as it ends, unless it is killed, so that a test sees whether
//!   two captures overlapped, and which VMM a capture left;
//! - `delayMs`: how long the guest boots before it captures, 0 by default;
//! - `keepMemory`: whether the fake leaves the file that backed the RAM, as OpenVMM leaves it when
//!   it ends abruptly.
//!
//! On Unix, the snapshot's directory and files are private to the current user, as OpenVMM's are;
//! on Windows, they take the access of the destination's directory, while OpenVMM gives them its
//! own, which the store's unit tests apply.
//!
//! Like OpenVMM, the fake takes `--net` only with `--network-profile portable`, and only to boot a
//! guest: a restore takes the addressing of its NIC from its snapshot, so it refuses `--net`, and
//! takes `--network-profile` alone, which a boot refuses.
//!
//! Every `--mount-deny`, `--mount-allow`, and `--mount-write` path of a launch must exist in a
//! shared directory. Like OpenVMM, the fake attributes each one to the share whose root contains
//! it once it has resolved both, so a root that is relative to the working directory, as
//! `nvx-rs run` passes it, contains the absolute paths below it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use crate::protocol::nvxs::ErrorCode;
use crate::protocol::{
    APP_CANCEL, APP_ERROR, APP_EXEC, APP_EXIT, APP_FEATURES, APP_MAPS, APP_PING, APP_READY,
    APP_STDERR, APP_STDOUT, APP_STOP, APP_STOPPED, BUSY, CAPABILITY_LEN, CWD_FAILED, ExitCategory,
    GuestFeatures, INVALID_REQUEST, LAUNCH_FAILED, MAPPING_FAILED, MAPPINGS_INCOMPLETE, MAPS_TOKEN,
    MAX_HOST_MAPPINGS, MAX_OUTPUT_BYTES, MapsPayload, OUTER_DATA, OUTER_ERROR, OUTER_HEADER_LEN,
    OUTER_HOST_ATTACH, OUTER_READY, OuterHeader, UNSUPPORTED_OPERATION, WORKLOAD_ACCOUNT_TOKEN,
    decode_app, decode_exec_payload, decode_maps_payload, decode_outer_header, encode_app,
    encode_outer,
};

/// Largest workload-output frame emitted by the fake guest.
const OUTPUT_CHUNK_BYTES: usize = 32 * 1024;

/// Control features of the current guest: cancellation, workload accounts, workload
/// containment, per-execution environments, working directories, and host path mapping tables.
/// Bit 1 is retired.
const GUEST_FEATURES: GuestFeatures = GuestFeatures::from_bits(
    GuestFeatures::CANCEL.bits()
        | GuestFeatures::WORKLOAD_ACCOUNT.bits()
        | GuestFeatures::EXEC_CGROUP.bits()
        | GuestFeatures::EXEC_ENVIRONMENT.bits()
        | GuestFeatures::EXEC_CWD.bits()
        | GuestFeatures::HOST_MAPPING_TABLE.bits(),
);

/// The guest directory at which the host mounts the aggregate export.
const GUEST_EXPORT: &str = "/run/nvx/hostfs/root";

/// Bytes in a MiB.
const MIB: u64 = 1024 * 1024;

/// The granularity of the RAM capacity of a capture's VM, which OpenVMM requires.
const MEMORY_CAPACITY_ALIGNMENT: u64 = 128 * MIB;

/// The size of a logical block of a sandbox block device, of which a capture's devices' lengths
/// are multiples.
const LOGICAL_BLOCK_SIZE: u64 = 512;

/// Name of the scenario of a template's capture, next to the kernel.
const CAPTURE_SCENARIO_NAME: &str = "fake-openvmm-capture.json";

/// The refusal of a capture whose kernel command line is not a template's.
const CAPTURE_COMMAND_LINE: &str = "a capture's kernel command line is nvx_sandbox=1, alone or \
                                    with a canonical maxcpus= of 1, 2, 4, or 8 of its processors";

/// Prefix of the name of the file that backs the RAM of a capture's VM, beside the snapshot's
/// destination, which the process ID of the VMM follows.
const MEMORY_FILE_PREFIX: &str = ".openvmm-microvm-memory-";

/// Name of the manifest of a snapshot.
const SNAPSHOT_MANIFEST: &str = "manifest.bin";
/// Name of the RAM of a snapshot.
const SNAPSHOT_MEMORY: &str = "memory.bin";
/// Name of the saved state of a snapshot.
const SNAPSHOT_STATE: &str = "state.bin";

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

/// The lifecycle that the command line selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    /// `--microvm-lifecycle managed`: the guest serves the control console until a stop.
    Managed,
    /// `--microvm-lifecycle one-shot`: the guest runs its entrypoint once, and OpenVMM exits with
    /// its status. Only the sandbox profile uses it.
    OneShot,
    /// Neither a lifecycle nor a sandbox block device: the microVM that `nvx-rs run`
    /// boots, or restores from a snapshot, on the terminal, which OpenVMM runs until
    /// the guest shuts down.
    Run,
    /// No lifecycle, sandbox block devices, and a snapshot destination: the VM of a template's
    /// capture, which its guest captures, after which OpenVMM exits.
    Capture,
}

/// The options of a template's capture.
#[derive(Debug, Clone)]
struct CaptureOptions {
    /// The snapshot's destination, which must not exist.
    destination: PathBuf,
    /// The RAM, in bytes.
    memory: u64,
    /// The scenario of the capture, `fake-openvmm-capture.json` next to the kernel.
    scenario: PathBuf,
}

/// Parsed OpenVMM-compatible command line and fake-specific test controls.
struct Options {
    /// Name of the executable, which prefixes the fake's diagnostics.
    program: String,
    /// The lifecycle.
    lifecycle: Lifecycle,
    /// Whether the command line launches the sandbox profile of `nvx-rs sandbox`: a container
    /// over EROFS layers and an ext4 scratch disk.
    sandbox: bool,
    /// The snapshot directory that a run restores, or `None` for a cold boot.
    restore: Option<PathBuf>,
    /// Control endpoint at which the fake accepts host sessions; empty for a one-shot run.
    endpoint: String,
    /// Path to the synthetic OpenVMM outcome report, which a one-shot run may omit.
    report: Option<PathBuf>,
    /// Backend name recorded in the synthetic report.
    hypervisor: String,
    /// Path that receives the parsed command-line dump.
    args_dump: PathBuf,
    /// Path that receives the mounted-host-path dump.
    maps_dump: PathBuf,
    /// Path that receives what the sandbox block devices held.
    blocks_dump: PathBuf,
    /// The sandbox block devices: their roles and paths, and whether each is read-only.
    blocks: Vec<SandboxBlock>,
    /// Artificial delay before the fake opens the sandbox block devices.
    block_delay: Duration,
    /// Path that receives a line for each control connection that the fake accepts.
    sessions_log: PathBuf,
    /// Status with which to stop immediately after launch, when requested.
    exit_on_start: Option<u8>,
    /// Status with which a one-shot workload ends instead of its own, when requested.
    exit_status: Option<i32>,
    /// Artificial delay before the fake guest becomes ready.
    boot_delay: Duration,
    /// Artificial delay before the fake OpenVMM crashes, when requested.
    crash_after: Option<Duration>,
    /// Whether stop requests are deliberately left unanswered.
    ignore_stop: bool,
    /// Artificial delay between the acknowledgement of a stop and the exit.
    exit_delay: Duration,
    /// Whether cancellation requests are deliberately ignored.
    ignore_cancel: bool,
    /// Whether the guest omits feature negotiation like an older image.
    legacy_guest: bool,
    /// Whether host-mapping requests are deliberately refused.
    refuse_maps: bool,
    /// UID whose account and environment the fake guest models.
    workload_uid: u32,
    /// Feature mask returned during guest feature negotiation.
    guest_features: GuestFeatures,
    /// Host directories of the aggregate export, in child order.
    children: Vec<PathBuf>,
    /// Number of host mappings that the kernel command line announces.
    maps: usize,
    /// The guest directories at which a sandbox's directory shares appear.
    share_targets: Vec<String>,
    /// The entrypoint and arguments of a sandbox, from its kernel command line.
    workload: Vec<String>,
    /// The network policy of the command line, as OpenVMM summarizes it in its report.
    network: NetworkSummary,
    /// The options of a template's capture, for [`Lifecycle::Capture`].
    capture: Option<CaptureOptions>,
}

/// The network policy of a command line, as the `network_policy` section of OpenVMM's outcome
/// report summarizes it.
#[derive(Debug, Clone, Default)]
struct NetworkSummary {
    /// Whether any network option was given.
    requested: bool,
    /// `rules`, `deny-all`, `allow-all`, or `none`.
    mode: &'static str,
    /// The number of `--network-egress-allow` rules.
    allow: usize,
    /// The number of `--network-egress-deny` rules.
    deny: usize,
    /// `allow`, `deny`, or `none`.
    host_loopback: &'static str,
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

/// A sandbox block device of the command line.
#[derive(Debug, Clone)]
struct SandboxBlock {
    /// Its role: `distro`, `runtime`, `custom`, or `scratch`.
    role: String,
    /// The path of its file, as the option names it.
    path: PathBuf,
    /// Whether the guest sees it read-only, as a layer.
    read_only: bool,
}

/// What a sandbox block device held when the fake opened it.
#[derive(serde::Serialize)]
struct OpenedBlock {
    /// Its role.
    role: String,
    /// The path at which the fake opened it.
    path: String,
    /// Its content, with invalid UTF-8 replaced.
    content: String,
}

/// Runs the fake OpenVMM with the process's arguments and standard input, as an executable's
/// `main` does, and returns its exit status.
pub fn main() -> ExitCode {
    let program = program_name();
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse(&program, &arguments) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{program}: {message}");
            return ExitCode::from(2);
        }
    };
    let _ = fs::write(
        &options.args_dump,
        serde_json::to_vec(&arguments).unwrap_or_default(),
    );
    if let Some(code) = options.exit_on_start {
        eprintln!("{program}: failing the launch as requested");
        return ExitCode::from(code);
    }
    thread::sleep(options.block_delay);
    // OpenVMM opens its disks as it builds the VM, and holds them until it exits.
    let opened = if options.lifecycle == Lifecycle::Capture {
        open_capture_blocks(&options)
    } else {
        open_blocks(&options)
    };
    let _blocks = match opened {
        Ok(blocks) => blocks,
        Err(message) => {
            eprintln!("{program}: {message}");
            return ExitCode::from(2);
        }
    };
    let ended = match options.lifecycle {
        Lifecycle::OneShot => Some(run_one_shot(&options)),
        Lifecycle::Run => Some(run_microvm(&options)),
        Lifecycle::Capture => Some(run_capture(&options)),
        Lifecycle::Managed => None,
    };
    if let Some(status) = ended {
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
        return exit_code(status);
    }
    let capability = match read_capability() {
        Ok(capability) => capability,
        Err(message) => {
            eprintln!("{program}: {message}");
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
            eprintln!("{program}: {error}");
            ExitCode::from(1)
        }
    }
}

/// Returns `status` for [`main`] to return, or exits with it if `ExitCode` cannot hold it.
///
/// Returning from `main`, rather than calling `std::process::exit`, lets the C runtime run its exit
/// handlers, which `std::process::exit` skips on Windows: among them is the one with which a
/// coverage build writes its profile.
fn exit_code(status: i32) -> ExitCode {
    match u8::try_from(status) {
        Ok(status) => ExitCode::from(status),
        Err(_) => std::process::exit(status),
    }
}

/// Opens the sandbox block devices of `options` as OpenVMM opens them, a layer read-only and the
/// scratch disk for reading and writing, reads each whole, and records what each held in the
/// blocks dump. Returns the open files, which the caller holds until it exits, as OpenVMM holds
/// its disks.
fn open_blocks(options: &Options) -> Result<Vec<fs::File>, String> {
    let mut files = Vec::new();
    let mut opened = Vec::new();
    for block in &options.blocks {
        let failed = |error: io::Error| {
            format!(
                "cannot open the {} block device {}: {error}",
                block.role,
                block.path.display()
            )
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(!block.read_only)
            .open(&block.path)
            .map_err(failed)?;
        let mut content = Vec::new();
        file.read_to_end(&mut content).map_err(failed)?;
        opened.push(OpenedBlock {
            role: block.role.clone(),
            path: block.path.display().to_string(),
            content: String::from_utf8_lossy(&content).into_owned(),
        });
        files.push(file);
    }
    if !opened.is_empty() {
        let _ = fs::write(
            &options.blocks_dump,
            serde_json::to_vec(&opened).unwrap_or_default(),
        );
    }
    Ok(files)
}

/// What the fake found of a block device of a template's capture as it opened it.
#[derive(serde::Serialize)]
struct CheckedBlock {
    /// Its role: `distro` or `scratch`.
    role: String,
    /// The path at which the fake opened it.
    path: String,
    /// Its length, in bytes.
    length: u64,
    /// Whether a handle through which a process could write the file was open as the fake opened
    /// it, or `null` if the fake could not tell.
    writers: Option<bool>,
}

/// Opens the block devices of a template's capture as OpenVMM opens them, at their paths: the
/// `distro` layer read-only and the scratch disk for reading and writing, each without following
/// a link, and on Windows sharing only reads and deletion. Checks that each is a regular file
/// whose length is a nonzero multiple of 512 bytes, without reading it, and records in the blocks
/// dump each one's role, path, and length, and whether a handle through which a process could
/// write it was open. Returns the open files, which the caller holds until it exits.
fn open_capture_blocks(options: &Options) -> Result<Vec<fs::File>, String> {
    let mut files = Vec::new();
    let mut checked = Vec::new();
    for block in &options.blocks {
        let failed = |error: io::Error| {
            format!(
                "cannot open the {} block device {}: {error}",
                block.role,
                block.path.display()
            )
        };
        let (file, writers) = open_unshared(&block.path, !block.read_only).map_err(failed)?;
        let metadata = file.metadata().map_err(failed)?;
        let length = metadata.len();
        if !metadata.is_file() || length == 0 || !length.is_multiple_of(LOGICAL_BLOCK_SIZE) {
            return Err(format!(
                "the {} block device {} is not a regular file whose length is a nonzero multiple \
                 of {LOGICAL_BLOCK_SIZE} bytes",
                block.role,
                block.path.display()
            ));
        }
        checked.push(CheckedBlock {
            role: block.role.clone(),
            path: block.path.display().to_string(),
            length,
            writers,
        });
        files.push(file);
    }
    let _ = fs::write(
        &options.blocks_dump,
        serde_json::to_vec(&checked).unwrap_or_default(),
    );
    Ok(files)
}

/// Opens the file `path` without following a link, for writing too if `write` is set, and returns
/// it, with whether a handle through which a process could write it was open, as
/// [`writers_of`] finds it.
#[cfg(unix)]
fn open_unshared(path: &Path, write: bool) -> io::Result<(fs::File, Option<bool>)> {
    use std::os::unix::fs::OpenOptionsExt;

    let open = |write: bool| {
        OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)
    };
    let reader = open(false)?;
    let writers = writers_of(&reader);
    let file = if write { open(true)? } else { reader };
    Ok((file, writers))
}

/// Opens the file `path` as OpenVMM opens a snapshot-capable block device on Windows: the file
/// itself rather than the target of a reparse point, for writing too if `write` is set, sharing
/// only reads and deletion. The open fails while another handle may write the file, so none was
/// open once it succeeds, and none can be opened while the fake holds the file.
#[cfg(windows)]
fn open_unshared(path: &Path, write: bool) -> io::Result<(fs::File, Option<bool>)> {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
    };

    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    Ok((file, Some(false)))
}

/// Returns whether a handle, of any process, holds the file that `reader` holds open for reading
/// only open for writing: `Some(false)` once the fake has taken a read lease of the file, which
/// Linux grants no process while a handle may write the file, and has given it up; and `None` if
/// the file's file system grants no lease. A process that another started holds copies of the
/// other's handles until it runs its program, so the fake tries again for a second.
#[cfg(target_os = "linux")]
fn writers_of(reader: &fs::File) -> Option<bool> {
    use std::os::fd::AsRawFd;

    use nix::libc::{EAGAIN, F_RDLCK, F_SETLEASE, F_UNLCK, fcntl};

    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        // SAFETY: the descriptor is open for as long as `reader` is borrowed, and F_SETLEASE
        // takes an integer.
        if unsafe { fcntl(reader.as_raw_fd(), F_SETLEASE, F_RDLCK) } == 0 {
            // SAFETY: as above.
            unsafe { fcntl(reader.as_raw_fd(), F_SETLEASE, F_UNLCK) };
            return Some(false);
        }
        if io::Error::last_os_error().raw_os_error() != Some(EAGAIN) {
            return None;
        }
        if Instant::now() >= deadline {
            return Some(true);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Returns `None`: the fake cannot tell whether a handle of another process may write a file on
/// Unix hosts other than Linux.
#[cfg(all(unix, not(target_os = "linux")))]
fn writers_of(_reader: &fs::File) -> Option<bool> {
    None
}

/// Runs the entrypoint of a one-shot sandbox with its output on the fake's own, as OpenVMM runs
/// the guest on its console, writes the outcome report, and returns the status with which
/// OpenVMM exits: the workload's.
///
/// The entrypoint is `/bin/echo`, which prints its arguments, `/bin/sleep`, which sleeps for its
/// argument in seconds, `/bin/true` or `/bin/false`, or any other program, which is not found;
/// the `fake_exit_status=STATUS` token replaces the status.
fn run_one_shot(options: &Options) -> i32 {
    let (program, arguments) = options
        .workload
        .split_first()
        .map_or(("", &[][..]), |(program, arguments)| {
            (program.as_str(), arguments)
        });
    let status = match program {
        "/bin/echo" => {
            println!("{}", arguments.join(" "));
            0
        }
        "/bin/sleep" => {
            let seconds = arguments
                .first()
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|seconds| seconds.is_finite())
                .unwrap_or(0.0);
            thread::sleep(Duration::from_secs_f64(seconds.clamp(0.0, 3600.0)));
            0
        }
        "/bin/true" => 0,
        "/bin/false" => 1,
        _ => {
            eprintln!("{}: {program}: not found", options.program);
            127
        }
    };
    let status = options.exit_status.unwrap_or(status);
    let (category, status_code) = if status == 0 {
        ("success", 0)
    } else {
        ("guest-exit", status)
    };
    write_report(options, "run", category, status_code);
    status
}

/// Boots or restores the microVM of a run of `nvx-rs run`, as OpenVMM runs it on
/// the terminal, and returns the status with which OpenVMM exits.
///
/// No guest runs. After the `fake_boot_delay_ms` delay, a cold boot prints `PROGRAM: booted the
/// microVM`, and the restore of an existing snapshot directory `PROGRAM: restored the microVM`,
/// on standard output; then the run writes its outcome report and ends with 0, or with the status
/// of the `fake_exit_status=STATUS` token. A restore without its snapshot directory fails with 1,
/// as OpenVMM fails a restore that it cannot read, and writes no report.
fn run_microvm(options: &Options) -> i32 {
    thread::sleep(options.boot_delay);
    match &options.restore {
        Some(snapshot) if !snapshot.is_dir() => {
            eprintln!(
                "{}: cannot restore {}: no such snapshot directory",
                options.program,
                snapshot.display()
            );
            return 1;
        }
        Some(_) => println!("{}: restored the microVM", options.program),
        None => println!("{}: booted the microVM", options.program),
    }
    let status = options.exit_status.unwrap_or(0);
    let (category, status_code) = if status == 0 {
        ("success", 0)
    } else {
        ("guest-exit", status)
    };
    write_report(options, "run", category, status_code);
    status
}

/// The scenario of a template's capture, `fake-openvmm-capture.json` next to the kernel, as the
/// module's documentation describes it.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Scenario {
    /// The manifest that the snapshot publishes.
    manifest: PathBuf,
    /// The length of the saved state that the snapshot publishes.
    #[serde(default)]
    state_len: u64,
    /// What the capture does.
    #[serde(default)]
    outcome: CaptureOutcome,
    /// The guest's status of [`CaptureOutcome::Fail`].
    #[serde(default = "Scenario::default_status")]
    status: i32,
    /// The file that marks the capture active while it runs.
    #[serde(default)]
    active: Option<PathBuf>,
    /// How long the guest boots before it captures, in milliseconds.
    #[serde(default)]
    delay_ms: u64,
    /// Whether the fake leaves the file that backed the RAM.
    #[serde(default)]
    keep_memory: bool,
}

impl Scenario {
    /// Returns the guest's status of a failure that the scenario does not set.
    fn default_status() -> i32 {
        1
    }

    /// Reads the scenario at `path`.
    fn read(path: &Path) -> Result<Self, String> {
        let bytes = fs::read(path)
            .map_err(|error| format!("cannot read the scenario {}: {error}", path.display()))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("the scenario {} is malformed: {error}", path.display()))
    }
}

/// What the VM of a template's capture does, as its scenario's `outcome` asks.
#[derive(serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum CaptureOutcome {
    /// It publishes the snapshot, and reports the capture's success.
    #[default]
    Publish,
    /// Its guest fails before it captures.
    Fail,
    /// It publishes the snapshot, and then fails to tear the VM down.
    PublishThenFail,
    /// It never ends.
    Hang,
    /// It publishes the snapshot, and then never ends.
    PublishThenHang,
    /// It crashes, without a report.
    Crash,
}

/// The marker of a capture that runs, which its scenario's `active` names: created as the capture
/// starts, which fails if the marker exists, and removed as the capture ends.
struct Active(PathBuf);

impl Active {
    /// Marks the capture active at `path`, with the fake's process ID.
    fn begin(path: &Path) -> Result<Self, String> {
        fs::File::create_new(path)
            .and_then(|mut marker| write!(marker, "{}", std::process::id()))
            .map_err(|error| {
                format!(
                    "cannot mark the capture active at {}: {error}",
                    path.display()
                )
            })?;
        Ok(Self(path.to_owned()))
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Runs the VM of a template's capture as OpenVMM runs it, and returns the status with which
/// OpenVMM exits.
///
/// The fake reads the capability, backs the RAM with a sparse file beside the snapshot's
/// destination, and, once the guest has booted for the scenario's delay, does what the scenario
/// asks: it publishes the snapshot as [`publish_snapshot`] does, removes the file that backed the
/// RAM, unless the scenario keeps it, and writes the outcome report of the capture. A capture whose
/// guest fails reports `guest-exit` and exits with its status; a VMM that cannot publish the
/// snapshot reports `vmm-failure`, and one that fails once it has published it,
/// `teardown-failure`, both with 1; and a crash exits with 3, without a report, leaving the file
/// that backed the RAM.
fn run_capture(options: &Options) -> i32 {
    let fail = |message: String| {
        eprintln!("{}: {message}", options.program);
        2
    };
    let Some(capture) = &options.capture else {
        return fail("a capture has no capture options".to_owned());
    };
    if let Err(message) = read_capability() {
        return fail(message);
    }
    let scenario = match Scenario::read(&capture.scenario) {
        Ok(scenario) => scenario,
        Err(message) => return fail(message),
    };
    let _active = match scenario.active.as_deref().map(Active::begin).transpose() {
        Ok(active) => active,
        Err(message) => return fail(message),
    };
    let directory = capture.destination.parent().unwrap_or(Path::new("."));
    let memory = directory.join(format!("{MEMORY_FILE_PREFIX}{}", std::process::id()));
    if let Err(error) = create_sparse(&memory, capture.memory) {
        return fail(format!(
            "cannot back the RAM with {}: {error}",
            memory.display()
        ));
    }
    thread::sleep(Duration::from_millis(scenario.delay_ms));
    let (category, status) = match scenario.outcome {
        CaptureOutcome::Crash => return 3,
        CaptureOutcome::Hang => hang(),
        CaptureOutcome::Fail => ("guest-exit", scenario.status),
        publishing => {
            if let Err(error) = publish_snapshot(capture, &scenario, &memory) {
                eprintln!(
                    "{}: cannot publish the snapshot {}: {error}",
                    options.program,
                    capture.destination.display()
                );
                ("vmm-failure", 1)
            } else if publishing == CaptureOutcome::PublishThenHang {
                hang()
            } else if publishing == CaptureOutcome::PublishThenFail {
                ("teardown-failure", 1)
            } else {
                ("success", 0)
            }
        }
    };
    if !scenario.keep_memory {
        let _ = fs::remove_file(&memory);
    }
    write_report(options, "capture", category, status);
    status
}

/// Never returns, as a VM that never ends.
fn hang() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

/// Publishes the snapshot of a capture as OpenVMM publishes it: stages a copy of the scenario's
/// manifest, a sparse saved state of the scenario's length, and the RAM, a hard link to the file
/// `memory` that backed it, in a directory private to the current user beside the destination, and
/// renames it to the destination, which must not exist.
fn publish_snapshot(
    capture: &CaptureOptions,
    scenario: &Scenario,
    memory: &Path,
) -> io::Result<()> {
    let destination = &capture.destination;
    let name = destination
        .file_name()
        .ok_or_else(|| io::Error::other("the destination has no name"))?;
    let staging = destination.with_file_name(format!(
        ".{}.staging-{}-0-0",
        name.to_string_lossy(),
        std::process::id()
    ));
    create_private_dir(&staging)?;
    let manifest = fs::read(&scenario.manifest)?;
    create_private_file(&staging.join(SNAPSHOT_MANIFEST))?.write_all(&manifest)?;
    create_sparse(&staging.join(SNAPSHOT_STATE), scenario.state_len)?;
    fs::hard_link(memory, staging.join(SNAPSHOT_MEMORY))?;
    if fs::symlink_metadata(destination).is_ok() {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    fs::rename(&staging, destination)
}

/// Creates the directory `path`, private to the current user on Unix; on Windows, it takes the
/// access of its parent.
fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    fs::create_dir(path)
}

/// Creates the file `path`, which must not exist, for writing, private to the current user on
/// Unix; on Windows, it takes the access of its directory.
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// Creates the file `path`, which must not exist, as [`create_private_file`] does, as a sparse
/// file of `length` bytes.
fn create_sparse(path: &Path, length: u64) -> io::Result<()> {
    let file = create_private_file(path)?;
    #[cfg(windows)]
    set_sparse(&file)?;
    file.set_len(length)
}

/// Marks the file that `file` holds open for writing sparse, which NTFS otherwise allocates whole
/// as the file is extended.
#[cfg(windows)]
fn set_sparse(file: &fs::File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    /// `FSCTL_SET_SPARSE`, in `winioctl.h`, which marks a file sparse without a buffer.
    const FSCTL_SET_SPARSE: u32 = 0x0009_00c4;
    let mut returned = 0;
    // SAFETY: the handle is open for as long as `file` is borrowed, the control takes no input or
    // output buffer, and the count's pointer is valid.
    let set = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Returns the name of the running executable, such as `nvx-fake-openvmm`.
fn program_name() -> String {
    std::env::args_os()
        .next()
        .and_then(|path| {
            Path::new(&path)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "nvx-fake-openvmm".to_owned())
}

/// Parses the OpenVMM command line plus fake controls, rejecting unsupported combinations.
fn parse(program: &str, arguments: &[String]) -> Result<Options, String> {
    const VALUED: [&str; 28] = [
        "--mount-aggregate",
        "--mount-child",
        "--mount-deny",
        "--mount-allow",
        "--mount-write",
        "--mount",
        "--mount-owner",
        "--network-egress-allow",
        "--network-egress-deny",
        "--machine",
        "--microvm-workload-identity",
        "--microvm-lifecycle",
        "--microvm-sandbox-block",
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
        "--network-proxy",
        "--host-loopback",
        "--host-loopback-forward",
    ];
    // The options that only the microVMs of `nvx-rs run` and of captures take.
    const RUN_VALUED: [&str; 7] = [
        "--processors",
        "--cpu-profile",
        "--memory-capacity",
        "--restore-snapshot",
        "--restore-processors",
        "--restore-memory",
        "--restore-ready-path",
    ];
    // The options that only the VMs of templates' captures take.
    const CAPTURE_VALUED: [&str; 4] = [
        "--snapshot-destination",
        "--snapshot-tier",
        "--snapshot-block-identity",
        "--snapshot-generation-id",
    ];
    // `nvx-rs run` boots or restores its microVM without a lifecycle, and is no sandbox; a
    // capture boots its VM without a lifecycle, with sandbox block devices.
    let mut option_arguments = arguments.iter();
    let mut run = true;
    let mut capture = false;
    while let Some(argument) = option_arguments.next() {
        let name = argument.as_str();
        if name == "--microvm-lifecycle" || name == "--microvm-sandbox-block" {
            run = false;
        }
        capture |= name == "--snapshot-destination";
        if VALUED.contains(&name) || RUN_VALUED.contains(&name) || CAPTURE_VALUED.contains(&name) {
            let _ = option_arguments.next();
        }
    }
    let mut values: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut flags = BTreeSet::new();
    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        let name = argument.as_str();
        if name == "--single-process" || name == "--microvm-control-auth-stdin" {
            flags.insert(name);
        } else if VALUED.contains(&name)
            || ((run || capture) && RUN_VALUED.contains(&name))
            || (capture && CAPTURE_VALUED.contains(&name))
        {
            let value = iterator
                .next()
                .ok_or_else(|| format!("{name} requires a value"))?;
            values.entry(name).or_default().push(value);
        } else {
            return Err(format!("unsupported argument {name}"));
        }
    }
    let single = |name: &str| exactly_once(&values, name);
    let expect = |name: &str, expected: &str| -> Result<(), String> {
        let value = single(name)?;
        if value == expected {
            Ok(())
        } else {
            Err(format!("{name} must be {expected}, not {value}"))
        }
    };
    expect("--machine", "microvm")?;
    // The sandbox profile of `nvx-rs sandbox` puts its layers and scratch disk on sandbox block
    // devices, and may run one-shot.
    let sandbox = values.contains_key("--microvm-sandbox-block");
    let lifecycle = if capture {
        if values.contains_key("--microvm-lifecycle") {
            return Err("a capture takes no --microvm-lifecycle".to_owned());
        }
        Lifecycle::Capture
    } else if run {
        Lifecycle::Run
    } else {
        match single("--microvm-lifecycle")? {
            "managed" => Lifecycle::Managed,
            "one-shot" if sandbox => Lifecycle::OneShot,
            other => return Err(format!("unsupported lifecycle {other}")),
        }
    };
    if !flags.contains("--single-process") {
        return Err("--single-process is required".to_owned());
    }
    let (endpoint, report) = match lifecycle {
        Lifecycle::Managed | Lifecycle::Capture => {
            expect("--virtio-console", "none")?;
            if !flags.contains("--microvm-control-auth-stdin") {
                return Err("--microvm-control-auth-stdin is required".to_owned());
            }
            let endpoint = single("--microvm-control-console")?
                .strip_prefix("listen=")
                .ok_or("--microvm-control-console must use listen=")?
                .to_owned();
            if endpoint.is_empty() {
                return Err(
                    "--microvm-control-console must name its endpoint after listen=".to_owned(),
                );
            }
            (endpoint, Some(PathBuf::from(single("--microvm-report")?)))
        }
        Lifecycle::OneShot | Lifecycle::Run => {
            let kind = if run {
                "a microVM run"
            } else {
                "a one-shot run"
            };
            for option in ["--virtio-console", "--microvm-control-console"] {
                if values.contains_key(option) {
                    return Err(format!("{kind} takes no {option}"));
                }
            }
            if flags.contains("--microvm-control-auth-stdin") {
                return Err(format!("{kind} takes no --microvm-control-auth-stdin"));
            }
            let report = match values.get("--microvm-report").map(Vec::as_slice) {
                None => None,
                Some([report]) => Some(PathBuf::from(report)),
                Some(_) => return Err("--microvm-report must be given at most once".to_owned()),
            };
            (String::new(), report)
        }
    };
    let hypervisor = single("--hypervisor")?;
    if !["kvm", "mshv", "whp"].contains(&hypervisor) {
        return Err(format!("unsupported hypervisor {hypervisor}"));
    }
    // A run, or a capture, uses the default workload identity of the guest, and records its
    // command line next to the kernel of a cold boot or to the snapshot of a restore.
    let (workload_uid, anchor, restore) = if run || capture {
        if values.contains_key("--microvm-workload-identity") {
            return Err(format!(
                "{} takes no --microvm-workload-identity",
                if capture {
                    "a capture"
                } else {
                    "a microVM run"
                }
            ));
        }
        let (anchor, restore) = check_run_boot(&values)?;
        if capture && restore.is_some() {
            return Err("a capture boots its VM cold".to_owned());
        }
        (65534, anchor, restore)
    } else {
        let (workload_uid, kernel) = check_boot(&values)?;
        (workload_uid, kernel, None)
    };
    let capture_options = if capture {
        if !sandbox {
            return Err("a capture takes sandbox block devices".to_owned());
        }
        Some(check_capture(&values, &anchor)?)
    } else {
        None
    };
    // OpenVMM checks where a control console listens before it starts the VM, beside the snapshot
    // that the VM restores, or else captures, if any.
    #[cfg(unix)]
    if matches!(lifecycle, Lifecycle::Managed | Lifecycle::Capture) {
        let snapshot = ["--restore-snapshot", "--snapshot-destination"]
            .into_iter()
            .find_map(|option| values.get(option).and_then(|given| given.first()));
        check_control_console(&endpoint, snapshot.map(|snapshot| Path::new(*snapshot)))?;
    }
    if let Some(report) = &report
        && report.exists()
    {
        return Err(format!("report path {} already exists", report.display()));
    }
    // A run passes the kernel command line that its user gives, if any.
    let command_line = match values.get("--cmdline").map(Vec::as_slice) {
        None if run => "",
        Some([command_line]) if run => command_line,
        _ => single("--cmdline")?,
    };
    let tokens: Vec<&str> = command_line.split_whitespace().collect();
    let token_values = |name: &str| -> Vec<&str> {
        tokens
            .iter()
            .filter_map(|token| token.strip_prefix(name))
            .collect()
    };
    let (maps, children, share_targets, workload, blocks) = if sandbox {
        if !tokens.contains(&"nvx_sandbox=1") {
            return Err("a sandbox block device requires nvx_sandbox=1".to_owned());
        }
        let blocks = check_sandbox_blocks(
            values.get("--microvm-sandbox-block").into_iter().flatten(),
            ram_overlay_requested(&tokens)?,
        )?;
        if capture {
            // A template's VM boots the invariant configuration alone, without a tenant, with at
            // most the limit of its processors online at boot, which OpenVMM records in the
            // canonical form alone.
            let processors = values
                .get("--processors")
                .and_then(|given| given.first())
                .and_then(|given| given.parse::<u32>().ok());
            let invariant = match tokens.as_slice() {
                ["nvx_sandbox=1"] => true,
                ["nvx_sandbox=1", limit] => limit
                    .strip_prefix("maxcpus=")
                    .and_then(|count| {
                        count
                            .parse::<u32>()
                            .ok()
                            .filter(|parsed| parsed.to_string() == count)
                    })
                    .is_some_and(|count| {
                        [1, 2, 4, 8].contains(&count) && processors.is_some_and(|all| count <= all)
                    }),
                _ => false,
            };
            if !invariant {
                return Err(CAPTURE_COMMAND_LINE.to_owned());
            }
            let roles: BTreeSet<&str> = blocks.iter().map(|block| block.role.as_str()).collect();
            if roles != BTreeSet::from(["distro", "scratch"]) {
                return Err("a capture takes a distro layer and a scratch disk".to_owned());
            }
            for option in [
                "--mount",
                "--mount-aggregate",
                "--mount-child",
                "--mount-owner",
            ] {
                if values.contains_key(option) {
                    return Err(format!("a capture takes no {option}"));
                }
            }
            (0, Vec::new(), Vec::new(), Vec::new(), blocks)
        } else {
            let mut workload: Vec<String> = match token_values("nvx_entrypoint=").as_slice() {
                [entrypoint] => vec![(*entrypoint).to_owned()],
                _ => return Err("the kernel command line must set one nvx_entrypoint".to_owned()),
            };
            workload.extend(token_values("nvx_arg=").into_iter().map(str::to_owned));
            if token_values("nvx_hostname=").len() != 1 {
                return Err("the kernel command line must set one nvx_hostname".to_owned());
            }
            let (maps, children, share_targets) = match lifecycle {
                // A managed sandbox's guest agent maps its shares from the export, as the mapping
                // table of the host's `MAPS` requests orders.
                Lifecycle::Managed => {
                    if values.contains_key("--mount") {
                        return Err("a managed sandbox takes no --mount".to_owned());
                    }
                    if !token_values("nvx_share=").is_empty() {
                        return Err("a managed sandbox takes no nvx_share=".to_owned());
                    }
                    let maps = mapping_count(&token_values(MAPS_TOKEN))?;
                    (maps, mapped_children(&values, maps)?, Vec::new())
                }
                // A one-shot sandbox's shell binds them at boot, as their tokens place them.
                Lifecycle::OneShot => {
                    if !token_values(MAPS_TOKEN).is_empty() {
                        return Err(format!("a one-shot sandbox takes no {MAPS_TOKEN}"));
                    }
                    let (children, mut share_targets) = sandbox_shares(&values)?;
                    // The guest binds each directory child of an aggregate at the target of its
                    // `nvx_share=NAME,TARGET,ACCESS[,file]` token.
                    for share in token_values("nvx_share=") {
                        match share.split(',').collect::<Vec<_>>().as_slice() {
                            [_, target, _] => share_targets.push((*target).to_owned()),
                            [_, _, _, "file"] => {}
                            _ => return Err(format!("invalid nvx_share={share}")),
                        }
                    }
                    (0, children, share_targets)
                }
                // A sandbox block device makes a command line no run, and a capture was handled above.
                Lifecycle::Run | Lifecycle::Capture => {
                    return Err("a microVM run takes no sandbox block device".to_owned());
                }
            };
            (maps, children, share_targets, workload, blocks)
        }
    } else if run {
        // A run shares as a sandbox does, and its kernel command line is its user's.
        let (children, _) = sandbox_shares(&values)?;
        (0, children, Vec::new(), Vec::new(), Vec::new())
    } else {
        for option in ["--mount", "--mount-owner"] {
            if values.contains_key(option) {
                return Err(format!("unsupported argument {option}"));
            }
        }
        if tokens.iter().any(|token| {
            token.starts_with("nvx_")
                && !token.starts_with(MAPS_TOKEN)
                && *token != WORKLOAD_ACCOUNT_TOKEN
        }) {
            return Err("the kernel command line must not select a sandbox".to_owned());
        }
        if !tokens.iter().any(|token| token.starts_with("hostname=")) {
            return Err("the kernel command line must set the hostname".to_owned());
        }
        let maps = mapping_count(&token_values(MAPS_TOKEN))?;
        let children = mapped_children(&values, maps)?;
        (maps, children, Vec::new(), Vec::new(), Vec::new())
    };
    // OpenVMM resolves a run's relative policy path in its only share.
    let only_share = match children.as_slice() {
        [only] if run && values.contains_key("--mount") => Some(only),
        _ => None,
    };
    // OpenVMM attributes each policy path to the share whose root contains it once it has
    // resolved both, so a root that is relative to the working directory contains the absolute
    // paths below it. No policy path names a file child, which contains nothing.
    let roots: Vec<PathBuf> = children
        .iter()
        .filter(|root| root.is_dir())
        .filter_map(|root| fs::canonicalize(root).ok())
        .collect();
    for option in ["--mount-deny", "--mount-allow", "--mount-write"] {
        for given in values.get(option).into_iter().flatten() {
            let path = match only_share {
                Some(share) if Path::new(given).is_relative() => share.join(given),
                _ => PathBuf::from(given),
            };
            let shared = fs::canonicalize(&path)
                .is_ok_and(|path| roots.iter().any(|root| path.starts_with(root)));
            if !shared {
                return Err(format!("invalid {option} {}", Path::new(given).display()));
            }
        }
    }
    if (values.contains_key("--network-egress-allow")
        || values.contains_key("--network-egress-deny"))
        && !values.contains_key("--network-egress")
    {
        return Err("--network-egress is required with egress rules".to_owned());
    }
    if values.contains_key("--host-loopback-forward")
        && values.get("--host-loopback").map(Vec::as_slice) != Some(&["allow"][..])
    {
        return Err("--host-loopback-forward requires explicit --host-loopback allow".to_owned());
    }
    // OpenVMM attaches a NIC at boot with `--net`, and restores one with the addressing that its
    // snapshot saved, which `--network-profile` selects without `--net`.
    let net = values.contains_key("--net");
    if net || values.contains_key("--network-profile") {
        expect("--network-profile", "portable")?;
    }
    if net && restore.is_some() {
        return Err("restore takes microVM network addressing from saved state".to_owned());
    }
    if !net && restore.is_none() && values.contains_key("--network-profile") {
        return Err("--network-profile portable requires --net or --restore-snapshot".to_owned());
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
        .as_ref()
        .and_then(|report| report.parent())
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let default_features = if sandbox {
        GuestFeatures::SANDBOX_AGENT
    } else {
        GUEST_FEATURES
    };
    Ok(Options {
        program: program.to_owned(),
        lifecycle,
        sandbox,
        restore,
        endpoint,
        args_dump: anchor.with_file_name(format!("fake-openvmm-{token}.json")),
        maps_dump: anchor.with_file_name(format!("fake-openvmm-{token}-maps.json")),
        blocks_dump: anchor.with_file_name(format!("fake-openvmm-{token}-blocks.json")),
        blocks,
        block_delay: millis("fake_block_delay_ms").unwrap_or_default(),
        sessions_log: anchor.with_file_name(format!("fake-openvmm-{token}-sessions.log")),
        report,
        hypervisor: hypervisor.to_owned(),
        exit_on_start: knob("fake_exit_on_start").and_then(|value| value.parse().ok()),
        exit_status: knob("fake_exit_status").and_then(|value| value.parse().ok()),
        boot_delay: millis("fake_boot_delay_ms").unwrap_or_default(),
        crash_after: millis("fake_crash_after_ms"),
        ignore_stop: knob("fake_ignore_stop") == Some("1"),
        exit_delay: millis("fake_exit_delay_ms").unwrap_or_default(),
        ignore_cancel: knob("fake_ignore_cancel") == Some("1"),
        legacy_guest: knob("fake_legacy_guest") == Some("1"),
        refuse_maps: knob("fake_refuse_maps") == Some("1"),
        workload_uid,
        guest_features: knob("fake_guest_features")
            .and_then(|value| value.parse().ok())
            .map_or(default_features, GuestFeatures::from_bits),
        children,
        maps,
        share_targets,
        workload,
        network: network_summary(&values),
        capture: capture_options,
    })
}

/// Returns the one value of `name`, or fails unless it was given exactly once.
fn exactly_once<'a>(values: &BTreeMap<&str, Vec<&'a str>>, name: &str) -> Result<&'a str, String> {
    match values.get(name).map(Vec::as_slice) {
        Some([value]) => Ok(value),
        _ => Err(format!("{name} must be given exactly once")),
    }
}

/// Returns the value of `name` if it was given, or fails if it was given more than once.
fn at_most_once<'a>(
    values: &BTreeMap<&str, Vec<&'a str>>,
    name: &str,
) -> Result<Option<&'a str>, String> {
    match values.get(name).map(Vec::as_slice) {
        None => Ok(None),
        Some([value]) => Ok(Some(value)),
        Some(_) => Err(format!("{name} must be given at most once")),
    }
}

/// Checks the boot of a managed or one-shot launch, `--memory` in MiB, a non-root
/// `--microvm-workload-identity UID:GID`, and an existing `--kernel` and `--initrd`, and returns
/// the workload's UID and the kernel.
fn check_boot(values: &BTreeMap<&str, Vec<&str>>) -> Result<(u32, PathBuf), String> {
    if !exactly_once(values, "--memory")?.ends_with('M') {
        return Err("--memory must be given in MiB".to_owned());
    }
    let identity = exactly_once(values, "--microvm-workload-identity")?;
    let workload_uid = identity
        .split_once(':')
        .and_then(|(uid, gid)| Some((uid.parse::<u32>().ok()?, gid.parse::<u32>().ok()?)))
        .filter(|&(uid, gid)| uid != 0 && gid != 0)
        .map(|(uid, _)| uid)
        .ok_or_else(|| format!("invalid workload identity {identity}"))?;
    let kernel = PathBuf::from(exactly_once(values, "--kernel")?);
    for path in [&kernel, &PathBuf::from(exactly_once(values, "--initrd")?)] {
        if !path.is_file() {
            return Err(format!("{} does not exist", path.display()));
        }
    }
    Ok((workload_uid, kernel))
}

/// Checks how a run of `nvx-rs run` starts its microVM, and returns the path next to which the
/// fake records the command line, and the snapshot directory of a restore.
///
/// A run takes `--processors`, a positive count, and at most one `--cpu-profile`. A cold boot
/// takes `--memory` and an optional `--memory-capacity`, in MiB, and an existing `--kernel` and
/// `--initrd`, and records its command line next to the kernel. A restore takes
/// `--restore-snapshot`, next to which it records its command line, at most one each of
/// `--restore-processors`, which cannot exceed `--processors`, `--restore-memory` in MiB, and
/// `--restore-ready-path`, and none of the options of a cold boot.
fn check_run_boot(
    values: &BTreeMap<&str, Vec<&str>>,
) -> Result<(PathBuf, Option<PathBuf>), String> {
    let count = |name: &str, value: &str| {
        value
            .parse::<u32>()
            .ok()
            .filter(|count| *count > 0)
            .ok_or_else(|| format!("invalid {name} {value}"))
    };
    let mib = |name: &str| match at_most_once(values, name)? {
        Some(value) if !value.ends_with('M') => Err(format!("{name} must be given in MiB")),
        _ => Ok(()),
    };
    let processors = count("--processors", exactly_once(values, "--processors")?)?;
    at_most_once(values, "--cpu-profile")?;
    if let Some(snapshot) = at_most_once(values, "--restore-snapshot")? {
        for option in ["--memory", "--memory-capacity", "--kernel", "--initrd"] {
            if values.contains_key(option) {
                return Err(format!("a restore takes no {option}"));
            }
        }
        if let Some(restored) = at_most_once(values, "--restore-processors")?
            && count("--restore-processors", restored)? > processors
        {
            return Err("--restore-processors cannot exceed --processors".to_owned());
        }
        mib("--restore-memory")?;
        at_most_once(values, "--restore-ready-path")?;
        let snapshot = PathBuf::from(snapshot);
        return Ok((snapshot.clone(), Some(snapshot)));
    }
    for option in [
        "--restore-processors",
        "--restore-memory",
        "--restore-ready-path",
    ] {
        if values.contains_key(option) {
            return Err(format!("{option} requires --restore-snapshot"));
        }
    }
    exactly_once(values, "--memory")?;
    mib("--memory")?;
    mib("--memory-capacity")?;
    let kernel = PathBuf::from(exactly_once(values, "--kernel")?);
    for path in [&kernel, &PathBuf::from(exactly_once(values, "--initrd")?)] {
        if !path.is_file() {
            return Err(format!("{} does not exist", path.display()));
        }
    }
    Ok((kernel, None))
}

/// Checks the options of a template's capture, besides those of its boot, and returns them: RAM
/// and a RAM capacity in MiB, the capacity a multiple of 128 MiB no smaller than the RAM, a
/// `--snapshot-destination` that does not exist, in an existing directory, `--snapshot-tier
/// platform`, `--snapshot-block-identity generation`, and a `--snapshot-generation-id` of 32
/// hexadecimal digits that are not all zero. The scenario of the capture is next to `kernel`.
fn check_capture(
    values: &BTreeMap<&str, Vec<&str>>,
    kernel: &Path,
) -> Result<CaptureOptions, String> {
    let bytes = |name: &str| {
        let value = exactly_once(values, name)?;
        value
            .strip_suffix('M')
            .and_then(|mebibytes| mebibytes.parse::<u64>().ok())
            .and_then(|mebibytes| mebibytes.checked_mul(MIB))
            .filter(|bytes| *bytes != 0)
            .ok_or_else(|| format!("invalid {name} {value}"))
    };
    let memory = bytes("--memory")?;
    let capacity = bytes("--memory-capacity")?;
    if !capacity.is_multiple_of(MEMORY_CAPACITY_ALIGNMENT) || capacity < memory {
        return Err(
            "--memory-capacity must be a multiple of 128 MiB no smaller than --memory".to_owned(),
        );
    }
    for (name, expected) in [
        ("--snapshot-tier", "platform"),
        ("--snapshot-block-identity", "generation"),
    ] {
        let value = exactly_once(values, name)?;
        if value != expected {
            return Err(format!("{name} must be {expected}, not {value}"));
        }
    }
    let generation = exactly_once(values, "--snapshot-generation-id")?;
    if generation.len() != 32
        || !generation.bytes().all(|digit| digit.is_ascii_hexdigit())
        || generation.bytes().all(|digit| digit == b'0')
    {
        return Err(format!("invalid --snapshot-generation-id {generation}"));
    }
    let destination = PathBuf::from(exactly_once(values, "--snapshot-destination")?);
    if !destination.parent().is_some_and(Path::is_dir) {
        return Err(format!(
            "the directory of the snapshot destination {} does not exist",
            destination.display()
        ));
    }
    if fs::symlink_metadata(&destination).is_ok() {
        return Err(format!(
            "the snapshot destination {} exists",
            destination.display()
        ));
    }
    Ok(CaptureOptions {
        destination,
        memory,
        scenario: kernel.with_file_name(CAPTURE_SCENARIO_NAME),
    })
}

/// Checks the socket `endpoint` of a control console as OpenVMM checks it on Unix before it
/// starts the VM: its directory, once its path is absolute, must be a directory, not a link, that
/// belongs to the effective user, with exactly the mode 0700, and, if the VM restores or captures
/// the snapshot directory `snapshot`, the canonical directory that holds it, in which a restore of
/// the snapshot listens again.
#[cfg(unix)]
fn check_control_console(endpoint: &str, snapshot: Option<&Path>) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let endpoint = Path::new(endpoint);
    let absolute = if endpoint.is_absolute() {
        endpoint.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                format!("failed to resolve current directory for control endpoint: {error}")
            })?
            .join(endpoint)
    };
    let directory = absolute
        .parent()
        .ok_or("microVM control endpoint has no parent directory")?;
    let metadata = fs::symlink_metadata(directory).map_err(|error| {
        format!(
            "failed to inspect microVM control endpoint parent {}: {error}",
            directory.display()
        )
    })?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.permissions().mode() & 0o7777 != 0o700
    {
        return Err(
            "microVM control endpoint parent must be a non-symlink directory owned by \
                    OpenVMM with mode 0700"
                .to_owned(),
        );
    }
    let Some(snapshot) = snapshot else {
        return Ok(());
    };
    // OpenVMM finds the socket's identity in the canonical directory of its path.
    let directory = fs::canonicalize(directory).map_err(|error| {
        format!(
            "failed to canonicalize microVM console endpoint parent {}: {error}",
            directory.display()
        )
    })?;
    let parent = snapshot
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let namespace = fs::canonicalize(parent).map_err(|error| {
        format!(
            "failed to canonicalize snapshot parent {}: {error}",
            parent.display()
        )
    })?;
    if directory != namespace {
        return Err(format!(
            "microVM console socket must be inside the snapshot parent namespace {}",
            namespace.display()
        ));
    }
    Ok(())
}

/// Checks the sandbox block devices of a sandbox: each layer, `ROLE:file:PATH,ro` with a role of
/// `distro`, `runtime`, or `custom`, at most once each, and one scratch disk, `scratch:file:PATH`,
/// each an existing regular file, and returns them in order. A RAM-backed overlay, `ram_overlay`,
/// takes the `distro` layer alone instead, as OpenVMM admits it.
fn check_sandbox_blocks<'a>(
    blocks: impl Iterator<Item = &'a &'a str>,
    ram_overlay: bool,
) -> Result<Vec<SandboxBlock>, String> {
    let mut roles = BTreeSet::new();
    let mut checked = Vec::new();
    for block in blocks {
        let invalid = || format!("invalid --microvm-sandbox-block {block}");
        let (role, rest) = block.split_once(':').ok_or_else(invalid)?;
        let path = rest.strip_prefix("file:").ok_or_else(invalid)?;
        let (path, read_only) = match role {
            "distro" | "runtime" | "custom" => {
                (path.strip_suffix(",ro").ok_or_else(invalid)?, true)
            }
            "scratch" => (path, false),
            _ => return Err(invalid()),
        };
        if !roles.insert(role) || !Path::new(path).is_file() {
            return Err(invalid());
        }
        checked.push(SandboxBlock {
            role: role.to_owned(),
            path: PathBuf::from(path),
            read_only,
        });
    }
    if ram_overlay {
        if roles != BTreeSet::from(["distro"]) {
            return Err(
                "microVM RAM-backed overlay requires exactly one read-only distro block".to_owned(),
            );
        }
    } else if !roles.contains("scratch") || roles.len() < 2 {
        return Err("a sandbox needs a layer and a scratch disk".to_owned());
    }
    Ok(checked)
}

/// Returns whether the kernel command line of `tokens` requests a RAM-backed overlay upper, with
/// one `nvx_overlay_upper=ramfs`, as OpenVMM reads it: a token of that name with another value,
/// none, or a second one fails.
fn ram_overlay_requested(tokens: &[&str]) -> Result<bool, String> {
    let values: Vec<&str> = tokens
        .iter()
        .filter_map(|token| {
            let (name, value) = token.split_once('=').unwrap_or((token, ""));
            (name == "nvx_overlay_upper").then_some(value)
        })
        .collect();
    match values[..] {
        [] => Ok(false),
        ["ramfs"] => Ok(true),
        _ => Err("microVM nvx_overlay_upper must appear once and equal ramfs".to_owned()),
    }
}

/// Returns the number of host mappings that the `nvx_maps=` tokens of a kernel command line,
/// whose values are `counts`, announce: none without a token, and otherwise from 1 through
/// [`MAX_HOST_MAPPINGS`], given once.
fn mapping_count(counts: &[&str]) -> Result<usize, String> {
    match counts {
        [] => Ok(0),
        [count] => count
            .parse::<usize>()
            .ok()
            .filter(|count| (1..=MAX_HOST_MAPPINGS).contains(count))
            .ok_or_else(|| format!("invalid {MAPS_TOKEN}{count}")),
        _ => Err(format!("{MAPS_TOKEN} must be given at most once")),
    }
}

/// Returns the host directories of the shares of a one-shot sandbox or a run, for the checks of
/// their policy paths, and the guest targets of their directory shares: one
/// `--mount TARGET,HOST,MODE` of a directory, or the `--mount-child NAME,HOST,MODE[,file]`
/// children of an aggregate at `/run/nvx/shares`.
fn sandbox_shares(
    values: &BTreeMap<&str, Vec<&str>>,
) -> Result<(Vec<PathBuf>, Vec<String>), String> {
    let mut roots = Vec::new();
    let mut targets = Vec::new();
    for share in values.get("--mount").into_iter().flatten() {
        let invalid = || format!("invalid --mount {share}");
        let (target, rest) = share.split_once(',').ok_or_else(invalid)?;
        let (host, mode) = rest.rsplit_once(',').ok_or_else(invalid)?;
        if !Path::new(host).is_dir() || !["ro", "rw"].contains(&mode) {
            return Err(invalid());
        }
        roots.push(PathBuf::from(host));
        targets.push(target.to_owned());
    }
    match values.get("--mount-aggregate").map(Vec::as_slice) {
        None => {}
        Some(["/run/nvx/shares"]) if roots.is_empty() => {
            for child in values.get("--mount-child").into_iter().flatten() {
                roots.push(mount_child_host(child, None)?);
            }
        }
        Some(_) => return Err("invalid --mount-aggregate".to_owned()),
    }
    if values.contains_key("--mount-child") && !values.contains_key("--mount-aggregate") {
        return Err("--mount-child requires --mount-aggregate".to_owned());
    }
    Ok((roots, targets))
}

/// Returns the host directories that the aggregate export of the host mapping table shares, in
/// child order, which `--mount-aggregate` and the `nvx_maps=` count of the kernel command line
/// must agree on.
fn mapped_children(
    values: &BTreeMap<&str, Vec<&str>>,
    maps: usize,
) -> Result<Vec<PathBuf>, String> {
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
                children.push(mount_child_host(child, Some(index))?);
            }
            if children.is_empty() {
                return Err("--mount-aggregate requires --mount-child".to_owned());
            }
            children
        }
        _ => {
            return Err(format!("--mount-aggregate and {MAPS_TOKEN} must agree"));
        }
    };
    if children.is_empty() && values.contains_key("--mount-child") {
        return Err("--mount-child requires --mount-aggregate".to_owned());
    }
    Ok(children)
}

/// Returns the host path of a `--mount-child NAME,HOST,MODE[,file]` value, or fails with
/// `invalid --mount-child VALUE` unless its mode is `ro` or `rw` and, if `index` is given, its
/// name is that index.
///
/// Like OpenVMM, a child exposes a directory or, with the `file` flag, a regular file, and refuses
/// a host path of the other kind.
fn mount_child_host(child: &str, index: Option<usize>) -> Result<PathBuf, String> {
    let invalid = || format!("invalid --mount-child {child}");
    let (name, rest) = child.split_once(',').ok_or_else(invalid)?;
    let (rest, file) = match rest.strip_suffix(",file") {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    let (host, mode) = rest.rsplit_once(',').ok_or_else(invalid)?;
    let kind_matches = if file {
        Path::new(host).is_file()
    } else {
        Path::new(host).is_dir()
    };
    if index.is_some_and(|index| name != index.to_string())
        || !kind_matches
        || !["ro", "rw"].contains(&mode)
    {
        return Err(invalid());
    }
    Ok(PathBuf::from(host))
}

/// Summarizes the network policy of a command line as OpenVMM's outcome report does.
fn network_summary(values: &BTreeMap<&str, Vec<&str>>) -> NetworkSummary {
    let count = |name: &str| values.get(name).map_or(0, Vec::len);
    let requested = [
        "--net",
        "--network-profile",
        "--network-egress",
        "--network-ingress",
        "--network-egress-allow",
        "--network-egress-deny",
        "--host-loopback",
        "--network-proxy",
        "--host-loopback-forward",
    ]
    .iter()
    .any(|name| values.contains_key(name));
    let egress = values
        .get("--network-egress")
        .and_then(|values| values.last());
    let mode = if count("--network-egress-allow") + count("--network-egress-deny") > 0 {
        "rules"
    } else if egress == Some(&"deny") {
        "deny-all"
    } else if requested {
        "allow-all"
    } else {
        "none"
    };
    let host_loopback = if !requested {
        "none"
    } else if values
        .get("--host-loopback")
        .and_then(|values| values.last())
        == Some(&"deny")
    {
        "deny"
    } else {
        "allow"
    };
    NetworkSummary {
        requested,
        mode,
        allow: count("--network-egress-allow"),
        deny: count("--network-egress-deny"),
        host_loopback,
    }
}

/// Reads exactly one nonzero control capability from standard input.
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
    /// Values exposed by the fake's scripted `get` and `set` commands.
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
    /// Constructs the initial guest environment and filesystem model from `options`.
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
            directories: ["/", "/tmp", "/work"]
                .into_iter()
                .map(str::to_owned)
                .chain(options.share_targets.iter().cloned())
                .collect(),
            files: BTreeSet::new(),
            mapped: Vec::new(),
        }
    }

    /// Mounts the entries of a `MAPS` request, like the guest agent: in order, and only as many
    /// as the kernel command line announced. Returns the error number and category of a refusal.
    fn map(&mut self, options: &Options, payload: &[u8]) -> Result<(), (i32, &'static str)> {
        const INVALID: (i32, &str) = (22, INVALID_REQUEST);
        let Ok(MapsPayload { first, maps }) = decode_maps_payload(payload) else {
            return Err(INVALID);
        };
        let (first, count) = (first as usize, maps.len());
        if options.maps == 0
            || first != self.mapped.len()
            || count == 0
            || count > options.maps - first
        {
            return Err(INVALID);
        }
        if options.refuse_maps {
            return Err((13, MAPPING_FAILED));
        }
        for map in maps {
            let (child, relative) = map.source.split_once('/').unwrap_or((&map.source, ""));
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
                self.directories.insert(map.target.clone());
                true
            } else if host.is_file() {
                self.files.insert(map.target.clone());
                false
            } else {
                return Err((2, MAPPING_FAILED));
            };
            self.mapped.push(Mapped {
                source: map.source,
                target: map.target,
                read_only: map.read_only,
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

/// Whether the fake should accept another control session or terminate.
enum Flow {
    /// Keep the fake VM running after the current session.
    Continue,
    /// Stop the fake VM after a successful stop request.
    Exit,
}

/// Outcome of control traffic observed while a workload runs.
enum Interrupt {
    /// No control request is waiting.
    None,
    /// The workload's matching cancellation request arrived.
    Cancel,
    /// The host disconnected and abandoned the workload.
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
        use std::os::fd::AsFd;

        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

        let mut descriptors = [PollFd::new(self.as_fd(), PollFlags::POLLIN)];
        Ok(poll(&mut descriptors, PollTimeout::ZERO)? > 0)
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

/// Encodes the broker's answer to an attachment, its first record to the host: READY, which
/// carries the session's identity, or ERROR, whose payload is the refusal's code.
fn control_record(record_type: u8, instance: &[u8; 16], epoch: u64, payload: &[u8]) -> Vec<u8> {
    encode_outer(record_type, instance, epoch, 0, payload).expect("an answer fits a record")
}

/// Decoded outer header paired with its owned payload.
type Record = (OuterHeader, Vec<u8>);

/// Reads one outer record, returning `None` at end of stream.
fn read_record<S: Read>(stream: &mut S) -> io::Result<Option<Record>> {
    let mut header = [0u8; OUTER_HEADER_LEN];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let header = decode_outer_header(&header).map_err(io::Error::other)?;
    let mut payload = vec![0u8; header.length];
    stream.read_exact(&mut payload)?;
    Ok(Some((header, payload)))
}

/// One authenticated host session over a connected stream.
struct Session<'a, S> {
    /// Connected transport to the host.
    stream: &'a mut S,
    /// Executable name used to prefix diagnostics.
    program: &'a str,
    /// Random identity assigned to this attachment.
    instance: [u8; 16],
    /// Monotonic attachment epoch.
    epoch: u64,
    /// Sequence number of the next guest record.
    guest_sequence: u64,
    /// Sequence number expected from the next host record.
    host_sequence: u64,
    /// Whether matching cancellation requests are ignored for fault injection.
    ignore_cancel: bool,
}

impl<S: Read + Write + Pending> Session<'_, S> {
    /// Encodes and sends one sequenced guest application frame.
    fn send(&mut self, kind: u8, request_id: u64, status: i32, payload: &[u8]) -> io::Result<()> {
        let frame = encode_app(kind, request_id, status, payload);
        let record = encode_outer(
            OUTER_DATA,
            &self.instance,
            self.epoch,
            self.guest_sequence,
            &frame,
        )
        .map_err(io::Error::other)?;
        self.guest_sequence += 1;
        self.stream.write_all(&record)
    }

    /// Reads the next host request, returning `None` at end of stream.
    fn request(&mut self) -> io::Result<Option<(u8, u64, Vec<u8>)>> {
        let Some((header, frame)) = read_record(self.stream)? else {
            return Ok(None);
        };
        let valid = header.record_type == OUTER_DATA
            && header.instance_id == self.instance
            && header.epoch == self.epoch
            && header.sequence == self.host_sequence;
        let frame = decode_app(&frame)
            .ok()
            .filter(|_| valid)
            .ok_or_else(|| io::Error::other("invalid data record"))?;
        self.host_sequence += 1;
        Ok(Some((frame.kind, frame.request_id, frame.payload.to_vec())))
    }

    /// Serves requests until the host disconnects or asks the VM to stop.
    fn run(&mut self, options: &Options, guest: &mut Guest) -> io::Result<Flow> {
        loop {
            let Some((kind, request_id, payload)) = self.request()? else {
                return Ok(Flow::Continue);
            };
            match kind {
                APP_PING => self.send(APP_READY, request_id, 0, &[])?,
                APP_FEATURES if !options.legacy_guest => {
                    self.send(APP_READY, request_id, 0, &options.guest_features.encode())?;
                }
                APP_EXEC if guest.mapped.len() < options.maps => {
                    self.send(APP_ERROR, request_id, 11, MAPPINGS_INCOMPLETE.as_bytes())?;
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
                _ => self.send(APP_ERROR, request_id, 95, UNSUPPORTED_OPERATION)?,
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
                Some((_, id, _)) => self.send(APP_ERROR, id, 16, BUSY.as_bytes())?,
            }
        }
        Ok(Interrupt::None)
    }

    /// Runs one workload. Returns `false` if the client disconnected and abandoned it.
    fn exec(&mut self, request_id: u64, payload: &[u8], guest: &mut Guest) -> io::Result<bool> {
        self.exec_script(request_id, payload, guest)
            .map(|outcome| outcome.is_some())
    }

    /// Interprets one fake-workload script and emits the same records a guest agent would.
    fn exec_script(
        &mut self,
        request_id: u64,
        payload: &[u8],
        guest: &mut Guest,
    ) -> io::Result<Option<()>> {
        let Ok(exec) = decode_exec_payload(payload) else {
            return self.send_some(APP_ERROR, request_id, 22, INVALID_REQUEST.as_bytes());
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
            return self.send_some(APP_ERROR, request_id, error, CWD_FAILED.as_bytes());
        }
        let timeout_ms = exec.timeout_ms;
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
                let message = format!("{}: {program}: not found\n", self.program);
                self.send(APP_STDERR, request_id, 0, message.as_bytes())?;
                return self.exit(request_id, 127, ExitCategory::Exit);
            }
            [] => return self.send_some(APP_ERROR, request_id, 22, INVALID_REQUEST.as_bytes()),
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
                        return self.exit(request_id, 125, ExitCategory::OutputLimit);
                    }
                }
                "printerr" => {
                    if !self.forward(APP_STDERR, request_id, &mut output, argument.as_bytes())? {
                        return self.exit(request_id, 125, ExitCategory::OutputLimit);
                    }
                }
                "touch" => {
                    // A workload that writes a read-write share creates host files.
                    if let Err(error) = OpenOptions::new().create(true).append(true).open(argument)
                    {
                        let message = format!("{}: touch: {argument}: {error}\n", self.program);
                        self.send(APP_STDERR, request_id, 0, message.as_bytes())?;
                        status = 1;
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
                                return self.exit(request_id, 137, ExitCategory::Cancelled);
                            }
                            Interrupt::Disconnected => return Ok(None),
                        }
                    }
                    if wake > end {
                        return self.exit(request_id, 124, ExitCategory::Timeout);
                    }
                }
                "exit" => {
                    let status = argument.parse().unwrap_or(1);
                    return self.exit(request_id, status, ExitCategory::Exit);
                }
                "signal" => {
                    let signal: i32 = argument.parse().unwrap_or(9);
                    return self.exit(request_id, 128 + signal, ExitCategory::Signal);
                }
                "flood" => {
                    let mut remaining: usize = argument.parse().unwrap_or(0);
                    while remaining > 0 {
                        let chunk = remaining.min(OUTPUT_CHUNK_BYTES);
                        if !self.forward(APP_STDOUT, request_id, &mut output, &vec![b'x'; chunk])? {
                            return self.exit(request_id, 125, ExitCategory::OutputLimit);
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
                            return self.exit(request_id, 125, ExitCategory::OutputLimit);
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
                                return self.exit(request_id, 125, ExitCategory::OutputLimit);
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
                "fail" => return self.exit(request_id, 125, ExitCategory::Failed),
                "launchfail" => {
                    return self.send_some(APP_ERROR, request_id, 125, LAUNCH_FAILED.as_bytes());
                }
                _ => {
                    let message = format!("{}: unknown command {name}\n", self.program);
                    self.send(APP_STDERR, request_id, 0, message.as_bytes())?;
                    return self.exit(request_id, 127, ExitCategory::Exit);
                }
            }
        }
        self.exit(request_id, status, ExitCategory::Exit)
    }

    /// Sends one frame and adapts success to the optional value expected by script execution.
    fn send_some(
        &mut self,
        kind: u8,
        request_id: u64,
        status: i32,
        payload: &[u8],
    ) -> io::Result<Option<()>> {
        self.send(kind, request_id, status, payload).map(Some)
    }

    /// Ends workload `request_id` with `status` and `category`.
    fn exit(
        &mut self,
        request_id: u64,
        status: i32,
        category: ExitCategory,
    ) -> io::Result<Option<()>> {
        self.send_some(APP_EXIT, request_id, status, category.as_str().as_bytes())
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
    if header.record_type != OUTER_HOST_ATTACH || payload != capability {
        let code = ErrorCode::Authentication.code().to_le_bytes();
        let _ = stream.write_all(&control_record(OUTER_ERROR, &[0; 16], 0, &code));
        return Flow::Continue;
    }
    let mut instance = [0u8; 16];
    while instance == [0; 16] {
        if getrandom::fill(&mut instance).is_err() {
            return Flow::Continue;
        }
    }
    if stream
        .write_all(&control_record(OUTER_READY, &instance, epoch, &[]))
        .is_err()
    {
        return Flow::Continue;
    }
    let mut session = Session {
        stream,
        program: &options.program,
        instance,
        epoch,
        guest_sequence: 1,
        host_sequence: 0,
        ignore_cancel: options.ignore_cancel,
    };
    match session.run(options, guest) {
        Ok(Flow::Exit) => {
            write_report(options, "managed", "success", 0);
            thread::sleep(options.exit_delay);
            Flow::Exit
        }
        _ => Flow::Continue,
    }
}

/// Writes the synthetic OpenVMM outcome report of `operation` if a report was requested and its
/// destination is new.
///
/// The report of a sandbox or a run takes the form of OpenVMM's, with the instance, the network
/// policy that the command line requested, and every step of the teardown; a direct launch's
/// keeps a shorter form.
fn write_report(options: &Options, operation: &str, category: &str, status_code: i32) {
    let Some(path) = &options.report else {
        return;
    };
    let report = if options.sandbox || options.lifecycle == Lifecycle::Run {
        let mut instance = [0u8; 16];
        let _ = getrandom::fill(&mut instance);
        let instance: String = instance.iter().map(|byte| format!("{byte:02x}")).collect();
        let network = &options.network;
        let teardown = serde_json::json!({
            "guest_workload_stopped": true,
            "vm_stopped": true,
            "openvmm_process_terminated": true,
            "virtiofs_released": true,
            "network_released": true,
            "temporary_storage_removed": true,
            "control_channels_closed": true,
        });
        let text = serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 1,
            "instance_id": instance,
            "backend": options.hypervisor,
            "outcome": { "operation": operation, "category": category, "status_code": status_code },
            "network_policy": {
                "status": if network.requested { "applied" } else { "not-requested" },
                "status_code": 0,
                "mode": network.mode,
                "allow_rule_count": network.allow,
                "deny_rule_count": network.deny,
                "host_loopback": network.host_loopback,
            },
            "teardown": teardown,
        }))
        .unwrap_or_default();
        format!("{text}\n")
    } else {
        serde_json::json!({
            "schema_version": 1,
            "backend": options.hypervisor,
            "outcome": { "operation": operation, "category": category, "status_code": status_code },
            "network_policy": { "status": "not-requested" },
            "teardown": { "workers": true, "memory": true },
        })
        .to_string()
    };
    if let Ok(mut file) = OpenOptions::new().write(true).create_new(true).open(path) {
        let _ = file.write_all(report.as_bytes());
    }
}

/// Records a connection to the control endpoint that the fake accepted, before it reads anything
/// of it, as a line of the session log.
fn record_session(options: &Options) {
    if let Ok(mut log) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&options.sessions_log)
    {
        let _ = log.write_all(b"connection\n");
    }
}

#[cfg(unix)]
/// Serves the fake control endpoint as a Unix-domain socket until a stop request.
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
        record_session(options);
        if let Flow::Exit = serve_client(&mut stream, options, capability, epoch, &mut guest) {
            let _ = fs::remove_file(&options.endpoint);
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(windows)]
/// Serves the fake control endpoint as a Windows named pipe until a stop request.
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
        record_session(options);
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

    #[test]
    fn shells_raise_shlvl_like_busybox() {
        let level = |value: &str| {
            let mut environment = vec![("SHLVL".to_owned(), value.to_owned())];
            start_shell(&mut environment, "/work");
            assert_eq!(environment[1], ("PWD".to_owned(), "/work".to_owned()));
            environment.swap_remove(0).1
        };
        assert_eq!(level("1"), "2");
        assert_eq!(level("7x"), "8");
        assert_eq!(level(" +3"), "4");
        assert_eq!(level("nothing"), "1");
        assert_eq!(level("4294967295"), "0");
        assert_eq!(level("-1"), "0");
        assert_eq!(level("-3"), "4294967294");
        let mut fresh = Environment::new();
        start_shell(&mut fresh, "/");
        assert_eq!(fresh[0], ("SHLVL".to_owned(), "1".to_owned()));
    }

    #[test]
    fn the_current_guest_advertises_every_required_feature() {
        assert_eq!(GUEST_FEATURES, GuestFeatures::REQUIRED);
        assert!(GUEST_FEATURES.missing(GuestFeatures::REQUIRED).is_empty());
    }

    #[test]
    fn mount_children_resolve_to_hosts_of_their_kind() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().to_str().unwrap().to_owned();
        let file = temporary.path().join("file").to_str().unwrap().to_owned();
        fs::write(&file, b"").unwrap();
        for (value, index, host) in [
            (format!("0,{directory},rw"), Some(0), &directory),
            (format!("1,{file},ro,file"), Some(1), &file),
            // Without an index, any name goes, as for the children of a sandbox's aggregate.
            (format!("digest,{directory},ro"), None, &directory),
        ] {
            assert_eq!(mount_child_host(&value, index), Ok(PathBuf::from(host)));
        }
        for (value, index) in [
            (format!("1,{directory},rw"), Some(0)),
            (format!("0,{file},rw"), None),
            (format!("0,{directory},ro,file"), None),
            (format!("0,{directory}-missing,rw"), None),
            (format!("0,{directory},wo"), None),
            (format!("0,{directory}"), None),
            ("0".to_owned(), None),
        ] {
            assert_eq!(
                mount_child_host(&value, index),
                Err(format!("invalid --mount-child {value}"))
            );
        }
    }

    /// Returns a directory with a kernel, an initramfs, a share with a `hidden` directory, and a
    /// snapshot directory, and the parse of the command line that `arguments` gives with the
    /// paths of that directory in place of `{dir}`.
    fn parse_run(arguments: &[&str]) -> (tempfile::TempDir, Result<Options, String>) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("vmlinux"), b"kernel").unwrap();
        fs::write(root.join("initramfs.cpio.gz"), b"initrd").unwrap();
        fs::create_dir_all(root.join("share").join("hidden")).unwrap();
        fs::create_dir(root.join("snapshot")).unwrap();
        let directory = root.to_str().unwrap();
        let arguments: Vec<String> = arguments
            .iter()
            .map(|argument| argument.replace("{dir}", directory))
            .collect();
        let options = parse("openvmm", &arguments);
        (temporary, options)
    }

    /// The options of every run, before those of its boot or restore.
    const RUN: [&str; 7] = [
        "--single-process",
        "--machine",
        "microvm",
        "--processors",
        "4",
        "--hypervisor",
        "kvm",
    ];

    fn run_with<'a>(extra: &[&'a str]) -> Vec<&'a str> {
        RUN.iter().chain(extra).copied().collect()
    }

    #[test]
    fn runs_boot_or_restore_without_a_lifecycle() {
        let boot = run_with(&[
            "--memory",
            "128M",
            "--memory-capacity",
            "1024M",
            "--kernel",
            "{dir}/vmlinux",
            "--initrd",
            "{dir}/initramfs.cpio.gz",
            "--cpu-profile",
            "host",
            "--mount",
            "/data,{dir}/share,rw",
            "--mount-deny",
            "hidden",
        ]);
        let (temporary, options) = parse_run(&boot);
        let options = options.unwrap();
        assert_eq!(options.lifecycle, Lifecycle::Run);
        assert!(!options.sandbox);
        assert_eq!(options.restore, None);
        assert_eq!(options.exit_status, None);
        assert_eq!(
            options.args_dump,
            temporary.path().join("fake-openvmm-.json")
        );
        let restore = run_with(&[
            "--restore-snapshot",
            "{dir}/snapshot",
            "--restore-processors",
            "2",
            "--restore-memory",
            "512M",
            "--restore-ready-path",
            "{dir}/ready.sock",
            "--cmdline",
            "fake_exit_status=3",
            "--microvm-report",
            "{dir}/share/report.json",
        ]);
        let (temporary, options) = parse_run(&restore);
        let options = options.unwrap();
        let snapshot = temporary.path().join("snapshot");
        assert_eq!(options.restore.as_ref(), Some(&snapshot));
        assert_eq!(options.exit_status, Some(3));
        // A restore has no kernel, so the command line goes next to the snapshot.
        assert_eq!(
            options.args_dump,
            temporary.path().join("fake-openvmm-share.json")
        );
        // A restored NIC keeps the addressing that its snapshot saved, so a restore takes the
        // network profile and policy without `--net`.
        let networked = run_with(&[
            "--restore-snapshot",
            "{dir}/snapshot",
            "--network-profile",
            "portable",
            "--network-egress",
            "deny",
            "--network-egress-allow",
            "10.0.0.1/32:tcp:443",
            "--host-loopback",
            "deny",
        ]);
        let (temporary, options) = parse_run(&networked);
        let options = options.unwrap();
        assert_eq!(options.restore, Some(temporary.path().join("snapshot")));
        assert!(options.network.requested);
        assert_eq!(options.network.mode, "rules");
        assert_eq!(options.network.allow, 1);
        assert_eq!(options.network.host_loopback, "deny");
    }

    #[test]
    fn runs_refuse_what_their_boot_or_restore_does_not_take() {
        const KERNEL: [&str; 4] = [
            "--kernel",
            "{dir}/vmlinux",
            "--initrd",
            "{dir}/initramfs.cpio.gz",
        ];
        fn boot(extra: &[&'static str]) -> Vec<&'static str> {
            let mut arguments = vec!["--memory", "128M"];
            arguments.extend(KERNEL);
            arguments.extend(extra);
            run_with(&arguments)
        }
        fn restore(extra: &[&'static str]) -> Vec<&'static str> {
            let mut arguments = vec!["--restore-snapshot", "{dir}/snapshot"];
            arguments.extend(extra);
            run_with(&arguments)
        }
        let kernel = KERNEL;
        let cases: Vec<(Vec<&str>, &str)> = vec![
            (
                restore(&["--memory", "128M"]),
                "a restore takes no --memory",
            ),
            (restore(&kernel), "a restore takes no --kernel"),
            (
                restore(&["--restore-processors", "8"]),
                "--restore-processors cannot exceed --processors",
            ),
            (
                restore(&["--restore-memory", "1G"]),
                "--restore-memory must be given in MiB",
            ),
            (
                restore(&["--net", "10.0.0.2/24", "--network-profile", "portable"]),
                "restore takes microVM network addressing from saved state",
            ),
            (
                restore(&["--network-profile", "open"]),
                "--network-profile must be portable, not open",
            ),
            (
                boot(&["--network-profile", "portable"]),
                "--network-profile portable requires --net or --restore-snapshot",
            ),
            (
                boot(&["--restore-processors", "2"]),
                "--restore-processors requires --restore-snapshot",
            ),
            (
                boot(&["--memory-capacity", "2"]),
                "--memory-capacity must be given in MiB",
            ),
            (
                boot(&["--cpu-profile", "host", "--cpu-profile", "auto"]),
                "--cpu-profile must be given at most once",
            ),
            (
                boot(&["--microvm-workload-identity", "1000:1000"]),
                "a microVM run takes no --microvm-workload-identity",
            ),
            (
                boot(&["--virtio-console", "none"]),
                "a microVM run takes no --virtio-console",
            ),
            (
                boot(&["--mount", "/data,{dir}/share,ro", "--mount-deny", "missing"]),
                "invalid --mount-deny missing",
            ),
            (
                boot(&["--mount-allow", "hidden"]),
                "invalid --mount-allow hidden",
            ),
        ];
        for (arguments, message) in cases {
            let (_temporary, options) = parse_run(&arguments);
            assert_eq!(options.err().as_deref(), Some(message), "{arguments:?}");
        }
        let mut missing = RUN.to_vec();
        missing.truncate(3);
        missing.extend(["--hypervisor", "kvm", "--memory", "128M"]);
        let (_temporary, options) = parse_run(&missing);
        assert_eq!(
            options.err().as_deref(),
            Some("--processors must be given exactly once")
        );
    }

    #[test]
    fn launches_with_a_lifecycle_take_no_options_of_a_run() {
        for option in ["--processors", "--cpu-profile", "--restore-snapshot"] {
            let arguments = [
                "--single-process",
                "--microvm-lifecycle",
                "managed",
                option,
                "1",
            ];
            let (_temporary, options) = parse_run(&arguments);
            assert_eq!(
                options.err(),
                Some(format!("unsupported argument {option}")),
                "{option}"
            );
        }
    }

    /// The command line of a template's capture, with `{dir}` in place of the directory of
    /// [`parse_capture`].
    const CAPTURE: &[&str] = &[
        "--machine",
        "microvm",
        "--microvm-sandbox-block",
        "distro:file:{dir}/placeholder,ro",
        "--microvm-sandbox-block",
        "scratch:file:{dir}/transaction/scratch.raw",
        "--processors",
        "2",
        "--memory",
        "512M",
        "--memory-capacity",
        "2048M",
        "--snapshot-destination",
        "{dir}/transaction/snapshot",
        "--snapshot-tier",
        "platform",
        "--snapshot-block-identity",
        "generation",
        "--snapshot-generation-id",
        "0123456789abcdef0123456789ABCDEF",
        "--single-process",
        "--hypervisor",
        "kvm",
        "--cpu-profile",
        "auto",
        "--kernel",
        "{dir}/vmlinux",
        "--initrd",
        "{dir}/initramfs.cpio.gz",
        "--cmdline",
        "nvx_sandbox=1",
        "--virtio-console",
        "none",
        "--microvm-control-console",
        "listen={dir}/transaction/control.sock",
        "--microvm-control-auth-stdin",
        "--microvm-report",
        "{dir}/transaction/outcome.json",
    ];

    /// Returns a directory with a kernel, an initramfs, a placeholder, and a transaction with a
    /// scratch disk, the directory and the transaction private to the current user, as the
    /// directory of a control console's socket must be, and the parse of the command line that
    /// `arguments` gives with the path of that directory in place of `{dir}`.
    fn parse_capture(arguments: &[&str]) -> (tempfile::TempDir, Result<Options, String>) {
        parse_capture_in(arguments, |_| {})
    }

    /// Returns the directory of [`parse_capture`], and the parse of `arguments` there once
    /// `prepare` has changed the directory.
    fn parse_capture_in(
        arguments: &[&str],
        prepare: impl FnOnce(&Path),
    ) -> (tempfile::TempDir, Result<Options, String>) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("vmlinux"), b"kernel").unwrap();
        fs::write(root.join("initramfs.cpio.gz"), b"initrd").unwrap();
        fs::write(root.join("placeholder"), [0; 512]).unwrap();
        fs::create_dir(root.join("transaction")).unwrap();
        fs::write(root.join("transaction").join("scratch.raw"), [0; 512]).unwrap();
        #[cfg(unix)]
        for directory in [root.to_owned(), root.join("transaction")] {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        prepare(root);
        let directory = root.to_str().unwrap();
        let arguments: Vec<String> = arguments
            .iter()
            .map(|argument| argument.replace("{dir}", directory))
            .collect();
        let options = parse("openvmm", &arguments);
        (temporary, options)
    }

    /// Returns the command line of [`CAPTURE`] with `value` in place of the value of `option`.
    fn capture_with(option: &str, value: &'static str) -> Vec<&'static str> {
        let mut arguments = CAPTURE.to_vec();
        let at = arguments.iter().position(|given| *given == option).unwrap();
        arguments[at + 1] = value;
        arguments
    }

    #[test]
    fn captures_boot_their_template_cold_on_their_block_devices() {
        let (temporary, options) = parse_capture(CAPTURE);
        let options = options.unwrap();
        let root = temporary.path();
        assert_eq!(options.lifecycle, Lifecycle::Capture);
        assert!(options.sandbox);
        assert_eq!(options.restore, None);
        let blocks: Vec<_> = options
            .blocks
            .iter()
            .map(|block| (block.role.as_str(), block.path.clone(), block.read_only))
            .collect();
        assert_eq!(
            blocks,
            [
                ("distro", root.join("placeholder"), true),
                (
                    "scratch",
                    root.join("transaction").join("scratch.raw"),
                    false
                ),
            ]
        );
        let capture = options.capture.as_ref().unwrap();
        assert_eq!(
            capture.destination,
            root.join("transaction").join("snapshot")
        );
        assert_eq!(capture.memory, 512 * MIB);
        assert_eq!(capture.scenario, root.join(CAPTURE_SCENARIO_NAME));
        assert_eq!(
            options.endpoint,
            format!("{}/transaction/control.sock", root.to_str().unwrap())
        );
        assert_eq!(
            options.blocks_dump,
            root.join("fake-openvmm-transaction-blocks.json")
        );
        assert!(options.workload.is_empty());
        assert!(options.children.is_empty());
    }

    #[test]
    fn captures_may_limit_the_processors_online_at_boot() {
        // The capture of `CAPTURE` has 2 processors.
        for command_line in ["nvx_sandbox=1 maxcpus=1", "nvx_sandbox=1 maxcpus=2"] {
            let (_temporary, options) = parse_capture(&capture_with("--cmdline", command_line));
            assert_eq!(options.err(), None, "{command_line}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn captures_listen_in_the_directory_of_their_snapshot_destination() {
        let (temporary, options) = parse_capture(&capture_with(
            "--microvm-control-console",
            "listen={dir}/control.sock",
        ));
        let namespace = fs::canonicalize(temporary.path().join("transaction")).unwrap();
        assert_eq!(
            options.err(),
            Some(format!(
                "microVM console socket must be inside the snapshot parent namespace {}",
                namespace.display()
            ))
        );
    }

    #[cfg(unix)]
    #[test]
    fn control_consoles_listen_in_a_private_directory_of_their_user() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let refusal = "microVM control endpoint parent must be a non-symlink directory owned by \
                       OpenVMM with mode 0700";
        // OpenVMM takes exactly the mode 0700.
        for mode in [0o750, 0o701, 0o1700] {
            let (_temporary, options) = parse_capture_in(CAPTURE, |root| {
                let transaction = root.join("transaction");
                fs::set_permissions(transaction, fs::Permissions::from_mode(mode)).unwrap();
            });
            assert_eq!(options.err().as_deref(), Some(refusal), "{mode:o}");
        }
        // It follows no link to the socket's directory, even to a private one.
        let (_temporary, options) = parse_capture_in(
            &capture_with(
                "--microvm-control-console",
                "listen={dir}/link/control.sock",
            ),
            |root| symlink(root.join("transaction"), root.join("link")).unwrap(),
        );
        assert_eq!(options.err().as_deref(), Some(refusal));
        let (temporary, options) = parse_capture(&capture_with(
            "--microvm-control-console",
            "listen={dir}/missing/control.sock",
        ));
        let missing = format!(
            "failed to inspect microVM control endpoint parent {}: ",
            temporary.path().join("missing").display()
        );
        let error = options.err().unwrap();
        assert!(error.starts_with(&missing), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_restore_listens_in_the_directory_of_the_snapshot_that_it_restores() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let (clone, generation) = (
            temporary.path().join("clone"),
            temporary.path().join("generation"),
        );
        for directory in [&clone, &generation] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let endpoint = clone.join("control.sock");
        let endpoint = endpoint.to_str().unwrap();
        assert_eq!(
            check_control_console(endpoint, Some(&clone.join("snapshot"))),
            Ok(())
        );
        // A VM that restores a snapshot elsewhere cannot listen in its own directory.
        assert_eq!(
            check_control_console(endpoint, Some(&generation.join("snapshot"))),
            Err(format!(
                "microVM console socket must be inside the snapshot parent namespace {}",
                fs::canonicalize(&generation).unwrap().display()
            ))
        );
        // Without a snapshot, the socket's directory alone counts.
        assert_eq!(check_control_console(endpoint, None), Ok(()));
    }

    #[test]
    fn captures_refuse_what_a_template_does_not_take() {
        let mut lifecycle = CAPTURE.to_vec();
        lifecycle.extend(["--microvm-lifecycle", "managed"]);
        let mut runtime = CAPTURE.to_vec();
        runtime.extend([
            "--microvm-sandbox-block",
            "runtime:file:{dir}/placeholder,ro",
        ]);
        let mut aggregate = CAPTURE.to_vec();
        aggregate.extend(["--mount-aggregate", "/run/nvx/shares"]);
        let mut identity = CAPTURE.to_vec();
        identity.extend(["--microvm-workload-identity", "1000:1000"]);
        let mut unbounded = CAPTURE.to_vec();
        let at = unbounded
            .iter()
            .position(|given| *given == "--memory-capacity")
            .unwrap();
        unbounded.drain(at..at + 2);
        let mut without_blocks = CAPTURE.to_vec();
        without_blocks.drain(2..6);
        let invalid_generation = "invalid --snapshot-generation-id";
        let capacity = "--memory-capacity must be a multiple of 128 MiB no smaller than --memory";
        let cases: Vec<(Vec<&str>, String)> =
            vec![
            (
                lifecycle,
                "a capture takes no --microvm-lifecycle".to_owned(),
            ),
            (
                runtime,
                "a capture takes a distro layer and a scratch disk".to_owned(),
            ),
            (aggregate, "a capture takes no --mount-aggregate".to_owned()),
            (
                identity,
                "a capture takes no --microvm-workload-identity".to_owned(),
            ),
            (
                unbounded,
                "--memory-capacity must be given exactly once".to_owned(),
            ),
            (
                without_blocks,
                "a capture takes sandbox block devices".to_owned(),
            ),
            (
                capture_with("--memory-capacity", "2000M"),
                capacity.to_owned(),
            ),
            (capture_with("--memory-capacity", "256M"), capacity.to_owned()),
            (
                capture_with("--snapshot-tier", "process"),
                "--snapshot-tier must be platform, not process".to_owned(),
            ),
            (
                capture_with("--snapshot-block-identity", "path"),
                "--snapshot-block-identity must be generation, not path".to_owned(),
            ),
            (
                capture_with("--snapshot-generation-id", "00000000000000000000000000000000"),
                format!("{invalid_generation} 00000000000000000000000000000000"),
            ),
            (
                capture_with("--snapshot-generation-id", "0123456789abcdef0123456789abcde"),
                format!("{invalid_generation} 0123456789abcdef0123456789abcde"),
            ),
            (
                capture_with("--snapshot-generation-id", "0123456789abcdef0123456789abcdeg"),
                format!("{invalid_generation} 0123456789abcdef0123456789abcdeg"),
            ),
            (
                capture_with("--cmdline", "nvx_sandbox=1 nvx_hostname=tenant"),
                CAPTURE_COMMAND_LINE.to_owned(),
            ),
            // OpenVMM records a limit of the processors online at boot in its canonical form
            // alone, of a count that a microVM supports, and at most its processors.
            (
                capture_with("--cmdline", "nvx_sandbox=1 maxcpus=01"),
                CAPTURE_COMMAND_LINE.to_owned(),
            ),
            (
                capture_with("--cmdline", "nvx_sandbox=1 maxcpus=3"),
                CAPTURE_COMMAND_LINE.to_owned(),
            ),
            (
                capture_with("--cmdline", "nvx_sandbox=1 maxcpus=4"),
                CAPTURE_COMMAND_LINE.to_owned(),
            ),
            (
                capture_with("--cmdline", "nvx_sandbox=1 maxcpus=1 maxcpus=1"),
                CAPTURE_COMMAND_LINE.to_owned(),
            ),
            (
                capture_with("--snapshot-destination", "{dir}/transaction"),
                "the snapshot destination {dir}/transaction exists".to_owned(),
            ),
            (
                capture_with("--snapshot-destination", "{dir}/missing/snapshot"),
                "the directory of the snapshot destination {dir}/missing/snapshot does not exist"
                    .to_owned(),
            ),
            (
                capture_with("--virtio-console", "stdio"),
                "--virtio-console must be none, not stdio".to_owned(),
            ),
            (
                capture_with("--microvm-control-console", "listen="),
                "--microvm-control-console must name its endpoint after listen=".to_owned(),
            ),
        ];
        for (arguments, message) in cases {
            let (temporary, options) = parse_capture(&arguments);
            let message = message.replace("{dir}", temporary.path().to_str().unwrap());
            assert_eq!(options.err(), Some(message), "{arguments:?}");
        }
    }

    #[test]
    fn capture_scenarios_take_defaults_and_refuse_unknown_fields() {
        let scenario: Scenario = serde_json::from_str(r#"{"manifest": "manifest.bin"}"#).unwrap();
        assert_eq!(scenario.manifest, Path::new("manifest.bin"));
        assert_eq!(scenario.state_len, 0);
        assert_eq!(scenario.outcome, CaptureOutcome::Publish);
        assert_eq!(scenario.status, 1);
        assert_eq!(scenario.active, None);
        assert_eq!(scenario.delay_ms, 0);
        assert!(!scenario.keep_memory);
        let scenario: Scenario = serde_json::from_str(
            r#"{"manifest": "m", "stateLen": 512, "outcome": "publish-then-hang", "status": 3,
                "active": "a", "delayMs": 5, "keepMemory": true}"#,
        )
        .unwrap();
        assert_eq!(
            (
                scenario.state_len,
                scenario.outcome,
                scenario.status,
                scenario.delay_ms,
                scenario.keep_memory
            ),
            (512, CaptureOutcome::PublishThenHang, 3, 5, true)
        );
        assert_eq!(scenario.active.as_deref(), Some(Path::new("a")));
        for refused in [
            r#"{}"#,
            r#"{"manifest": "m", "outcome": "succeed"}"#,
            r#"{"manifest": "m", "memory": 1}"#,
        ] {
            assert!(
                serde_json::from_str::<Scenario>(refused).is_err(),
                "{refused}"
            );
        }
    }
}
