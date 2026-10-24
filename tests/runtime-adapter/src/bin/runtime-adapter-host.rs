//! The runtime adapter's host program.
//!
//! - `runtime-adapter-host` reports the contract that it was built against and whether `nvx`
//!   supports this host.
//! - `runtime-adapter-host stage STORE --distro FORMAT:PATH --distro-identity UUID
//!   [--upper scratch:PATH]` stages an image and a scratch image for the launches of a VM with a
//!   layered root in the sandbox store STORE, which it creates if needed. The image is the raw
//!   file PATH, an EROFS filesystem, `erofs`, or a GPT disk, `gpt`, of which the guest expects the
//!   identity UUID, and the scratch image the ext4 image PATH, if given. Staging verifies the
//!   image explicitly, which hashes the image only when a manifest is beside it, binds the scratch
//!   image without reading it, and records both bindings in STORE, in place of those staged
//!   earlier; see `runtime_adapter::layered::Staged`. It needs no artifacts, and prints nothing.
//! - `runtime-adapter-host launch STORE --guest-agent PATH [--hypervisor NAME] [LAYERED]` launches
//!   a VM in the sandbox store STORE, which it creates if needed, through NVX's supervisor, with
//!   the guest agent PATH, a static `x86_64-unknown-linux-musl` build of `runtime-adapter-guest`,
//!   in place of NVX's agents; see `runtime_adapter::initramfs`. It records what it launches in
//!   STORE; see `runtime_adapter::layered::LaunchRecord`. It attaches to the VM's control
//!   session, exchanges a `Ping` and the longest `Echo` with the guest agent, commits the launch,
//!   and exits, leaving the VM running. The VM runs on the hypervisor NAME, `kvm`, `mshv`, or
//!   `whp`, or else on the one that `nvx::Hypervisor::from_env_or_default` selects.
//!
//!   LAYERED, `--distro FORMAT:PATH --distro-identity UUID --upper (scratch:PATH | ram)
//!   [--runtime-dir DIR] [--overlay-mode MODE] [--scratch-journal POLICY]
//!   [--first-partition-bound BYTES] [--workload-identity UID:GID] [--network FILE] [SHARES]`,
//!   gives the VM a layered root, which the guest agent's worker assembles; see
//!   `runtime_adapter::layered` and `runtime_adapter::worker`. Its image is the raw file PATH, an
//!   EROFS filesystem, `erofs`, or a GPT disk, `gpt`, of which the guest expects the identity UUID:
//!   the file system's UUID, or the disk's GUID. Its upper is the ext4 scratch image PATH, or RAM.
//!   The guest assembles the root below its runtime directory DIR, `/run/runtime-adapter` by
//!   default, in the overlay mode MODE, `trusted-lowers` or `untrusted-lowers`, checks a scratch's
//!   journal against the policy POLICY, `allowed` or `forbidden`, and refuses a first partition of
//!   a GPT disk larger than BYTES, with the guest's defaults, `trusted-lowers`, `allowed`, and 64
//!   KiB, unless given. A workload identity, the user ID UID and the group ID GID, two numbers from
//!   1 to 4294967294, is the identity as which every execution of the sandbox runs: the guest
//!   checks its account in the assembled root, without which the root's `identity` stage fails. The
//!   VM has a network if FILE names one: an `nvx::host::sandbox::SandboxNetwork`, the guest's
//!   address and the policy that OpenVMM enforces there, in the serde form of a plan's network,
//!   which the launch reads before anything else, so that it refuses a network that the plan
//!   refuses before it needs NVX's artifacts or the store, and puts on the adapter's plan, which
//!   renders it; see `runtime_adapter::layered::read_network`. Before it spawns the VM, the launch
//!   checks the bindings that STORE stages for the image and the scratch image, from their metadata
//!   alone, and fails if STORE stages none for either, or one no longer holds, as for an image that
//!   changed after it was staged. It hands OpenVMM the files that the checks opened, which it holds
//!   until the guest has assembled the root, or until it has torn down a launch that failed, and
//!   records the plan, with its network, and the bindings.
//!
//!   SHARES, `--share HOST:TARGET[:ro]... [--deny PATH]... [--reserved TARGET]...
//!   [--share-root DIR]...`, shares each host directory or regular file HOST with the root, at the
//!   absolute guest path TARGET, read-only with `:ro`, and read-write otherwise. The guest path
//!   starts at the last `:/`. Each HOST must lie inside a host directory DIR, which the adapter
//!   authorizes, once links are resolved: the launch refuses one outside them before it plans the
//!   export, with NVX's `filesystem::plan`. The export hides each host path PATH inside the
//!   shares, and no TARGET may take or cover a guest tree that the container root keeps, or a tree
//!   TARGET that `--reserved` keeps for the adapter. The launch refuses a request that
//!   `filesystem::plan` refuses, such as one with a read-only share inside a read-write one,
//!   before it spawns the VM, and checks the export with `ExportPlan::verify` before it spawns it.
//!   OpenVMM exports the shares; see `runtime_adapter::layered::Shares`.
//!
//!   Once attached, the launch follows the host's order; see `runtime_adapter::host`. It sends the
//!   root configuration, which announces how many mappings the export's table holds, and waits for
//!   the guest agent's `Ready`, then checks that the guest agent is of the build that the host
//!   ships, sends the mapping table, in order, then `Start`, and exchanges the `Ping` and the
//!   `Echo` once the guest started the sandbox. A guest agent of another build gets nothing more,
//!   and the launch fails, which terminates the VM. If the guest reports that a stage of the root
//!   failed instead, the launch waits for the VM to end, abandons it, and checks that the VM ended
//!   with the stage's status. If the guest fails a mapping, the launch sends no more of the table,
//!   nor `Start`, asks the guest agent to stop, which tears down the root, waits for the VM to
//!   end, abandons it, and checks that the VM ended with success and a complete teardown.
//! - `runtime-adapter-host reattach STORE` reattaches to the VM of STORE, which an earlier process
//!   launched and which it holds although it is not its child, attaches a new session to it, in
//!   the session's next epoch, and exchanges a `Ping`, leaving the VM running.
//! - `runtime-adapter-host stop STORE` reattaches and attaches as `reattach` does, then stops the
//!   VM: it asks the guest agent to stop, which first tears down a layered root, waits for the VM
//!   to exit, and checks the VM's outcome report. A VM whose guest agent does not confirm the stop
//!   is terminated.
//! - `runtime-adapter-host probe STORE PROBE...` reattaches and attaches as `reattach` does, then
//!   runs each PROBE in the VM's layered root, and prints the facts that it found. A PROBE is the
//!   text of a probe: its name, then its arguments, each after one space; see
//!   `runtime_adapter::probe` and `runtime_adapter::worker::probes`.
//! - `runtime-adapter-host exec STORE EXECUTION [; EXECUTION]...` reattaches and attaches as
//!   `reattach` does, then runs each EXECUTION in the VM's layered root, all at once, over that one
//!   session, through `runtime_adapter::host::Executions`, and prints their outputs and outcomes
//!   once every one finished. It runs at most 8 executions, as many as the guest runs at once; see
//!   `runtime_adapter::exec::MAX_EXECUTIONS`. An EXECUTION, `[--identity UID:GID [--groups GID,...]
//!   [--capabilities N,...] [--numeric]] [--cwd DIR] [--env NAME=VALUE]... -- PROGRAM [ARG]...`,
//!   runs PROGRAM with its ARGs, up to a lone `;`, as `find -exec` takes them, without standard
//!   input. Without `--identity`, it names no identity, and runs as the sandbox's workload
//!   identity, which the guest otherwise refuses; with it, it runs as the user ID UID and the group
//!   ID GID, with the supplementary groups GIDs and the capabilities numbered N, such as 0 for
//!   `CAP_CHOWN`, if given, and the guest checks its account, unless `--numeric` says that the IDs
//!   have none. It starts in the root's directory DIR, or `/`, with exactly the environment of its
//!   `--env` entries. It runs for at most a minute, with 16 KiB chunks and the credit of 4 of them
//!   for its input and 8 for its output, which its standard output and error share. The command
//!   keeps at most 16 MiB of each stream, and cancels an execution whose stream would exceed them;
//!   see `runtime_adapter::exec::MAX_CAPTURE`. Execution N, from 1, has the key `(B + N, E)`: E is
//!   the epoch of the command's attachment, and B the first 8 bytes of the broker instance's ID, as
//!   a little-endian number. The guest refuses a key that it heard of before, and the epochs of a
//!   broker instance only advance, while a broker restored from a snapshot has a new instance ID,
//!   so no command repeats the key of another's execution.
//!
//! Each command but `stage` takes NVX's artifacts from the directory that `NVX_ARTIFACTS_DIR`
//! names, a repository checkout or an extracted release, and the VM's runtime identity from NVX's
//! identity of them, with the guest agent's build that `--guest-build BUILD` names, this crate's
//! version by default; see `runtime_adapter::runtime`. Reattach finds a VM that another runtime
//! identity launched mismatched, and then signals nothing: the VM keeps running.
//!
//! The commands print what they find as `KEY=VALUE` lines:
//!
//! - `reattached`: what reattach found: `live`, `mismatched`, `exited`, or `indeterminate`;
//! - `differences`: the parts of the runtime identity in which a mismatched VM differs, separated
//!   by commas;
//! - `pid`: the VMM's process ID;
//! - `instance` and `epoch`: the broker instance, in hexadecimal, and the epoch of the session
//!   that the command attached to;
//! - `root`: how the guest answered the root configuration: `ready` or `failed`, or `failed`
//!   once a stop finds that the root's teardown failed;
//! - `build`: the build of the guest agent that assembled the root;
//! - `stage` and `problem`: the stage of the root that failed, such as `assembly`, and the
//!   problem, or the problem of a mapping that failed;
//! - `table`: how the guest took the mapping table: `applied` or `failed`;
//! - `mapped`: how many of the table's mappings the guest applied;
//! - `sandbox`: `started` once the guest started the sandbox;
//! - `PROBE.KEY`: the fact KEY that the probe named PROBE found;
//! - `exec.N.KEY`: what execution N did, with these KEYs:
//!   - `launch`: `started`, `failed STAGE ERRNO`, with the number of the guest's stage that failed
//!     and the error number, or `none` once the guest reported neither;
//!   - `status`: how the program ended: `exit CODE`, `signal NUMBER`, or `none`;
//!   - `cause`: why a side ended the execution, `cancelled`, `dropped`, `timed-out`, `reset`, or
//!     `disconnected`, `other` for a cause that `nvx` adds later, or `none` for an execution that
//!     ended on its own;
//!   - `truncated`: the streams that lost bytes, `stdout`, `stderr`, or both, separated by a
//!     comma, or `none`;
//!   - `settled`: `reusable` once the execution settled, or `fenced`;
//!   - `stdout` and `stderr`: the program's standard output and error;
//! - `stopped`: how the stop ended: `graceful`, `forced`, or `timed-out`;
//! - `outcome`: the category and status code of the VM's outcome report, such as `success 0`;
//! - `teardown`: whether the VM's teardown completed: `complete` or `incomplete`.
//!
//! Values escape `\`, newlines, and carriage returns as `\\`, `\n`, and `\r`, and each byte of a
//! program's output that is not part of UTF-8 text as `\xHH`, with two hexadecimal digits.
//!
//! For tests, `launch` takes a step hook from the variable `RUNTIME_ADAPTER_STEP_HOOK`, which
//! pauses the launch at a named step, for a time that the launch leaves out of its deadline, or
//! exits it there without unwinding, with status 5; see `runtime_adapter::hook`.
//!
//! They exit with 0 once they did what they were asked, `exec` once every execution finished,
//! whatever its outcome, with 3 when reattach finds the VM mismatched, with 4 when the guest
//! reported that a stage of the root failed, and the VM ended with the stage's status and a
//! complete teardown, with 5 when the step hook ends a launch, with 6 when the guest failed a
//! mapping of the table, and the VM stopped gracefully, with success and a complete teardown, and
//! with 1, after a diagnostic on standard error, when they fail. Anything else is a usage error,
//! with status 2, as is a value of `RUNTIME_ADAPTER_STEP_HOOK` that sets no step hook.

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use nvx::exec::{Cause, Key, Limits, Outcome, Status, Stream, Timers, Window};
use nvx::host::artifacts::Artifacts;
use nvx::host::instance::generate_capability;
use nvx::host::launch::LaunchSpec;
use nvx::host::report;
use nvx::host::session;
use nvx::host::store::{IdentityPart, LockedStore, SandboxStore};
use nvx::host::supervisor::{self, Reattached, Starting, StopOutcome, Vmm, VmmCommand};
use nvx::protocol::CAPABILITY_LEN;
use nvx::protocol::root::ImageFormat;
use nvx::uuid::Uuid;
use nvx::{ErrorKind, Hypervisor};
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::exec::{self, AccountMode, HostExecution, Start};
use runtime_adapter::frame::{MAX_BODY, Message};
use runtime_adapter::hook::{Step, StepHook};
use runtime_adapter::host::{self, Client, ClientError, Executions, OrderError};
use runtime_adapter::initramfs;
use runtime_adapter::layered::{LaunchRecord, LayeredLaunch, Share, Shares, Staged, read_network};
use runtime_adapter::probe::{Probe, ProbeReply, escape};
use runtime_adapter::root::{JournalPolicy, OverlayMode, RootFailure};
use runtime_adapter::runtime::Runtime;

/// How long a command waits for the sandbox's lock.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a launch waits for the VM to boot, its guest agent to assemble a layered root, and to
/// answer.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long reattach, stop, and probe wait to attach a session and for the guest agent's answers.
const SESSION_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a stop waits for the guest agent's confirmation and for the VM to exit, and how long
/// a launch whose root failed waits for the VM to exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a stop waits for a VM that it terminated to exit.
const FORCED_TIMEOUT: Duration = Duration::from_secs(10);
/// The name of the initramfs that boots the guest agent, which the launch writes into the
/// sandbox's store.
const INITRAMFS_NAME: &str = "runtime-adapter.cpio.gz";
/// The exit status of a command that finds the VM mismatched.
const MISMATCHED: u8 = 3;
/// The exit status of a command whose guest reported that a stage of the root failed, once the VM
/// ended with the stage's status.
const ROOT_FAILED: u8 = 4;
/// The exit status of a launch whose guest failed a mapping of the table, once the VM stopped
/// gracefully, with success and a complete teardown.
const TABLE_FAILED: u8 = 6;
/// How long an execution of `exec` may run before its sides end it.
const EXEC_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the pending output of an execution of `exec` still flows once a side ended it.
const EXEC_GRACE: Duration = Duration::from_secs(1);
/// How long an execution of `exec` may take to settle, once its program or a side ended it.
const EXEC_SETTLEMENT: Duration = Duration::from_secs(5);
/// The most bytes of a chunk of an execution of `exec`.
const EXEC_CHUNK: u32 = 16 * 1024;
/// The chunks of an execution's input that the guest may hold for `exec`.
const EXEC_INPUT_FRAMES: u32 = 4;
/// The chunks of an execution's output, of both streams, that `exec` may hold.
const EXEC_OUTPUT_FRAMES: u32 = 8;
/// The argument that ends an execution's arguments, after which another execution may follow.
const EXEC_END: &str = ";";
/// The command lines that the program accepts.
const USAGE: &str = concat!(
    "usage: runtime-adapter-host\n",
    "       runtime-adapter-host stage STORE --distro FORMAT:PATH --distro-identity UUID",
    " [--upper scratch:PATH]\n",
    "       runtime-adapter-host launch STORE --guest-agent PATH [--hypervisor NAME]",
    " [--guest-build BUILD]\n",
    "           [--distro FORMAT:PATH --distro-identity UUID",
    " --upper (scratch:PATH | ram) [--runtime-dir DIR]\n",
    "            [--overlay-mode MODE] [--scratch-journal POLICY]",
    " [--first-partition-bound BYTES]\n",
    "            [--workload-identity UID:GID] [--network FILE]\n",
    "            [--share HOST:TARGET[:ro]... [--deny PATH]... [--reserved TARGET]...",
    " [--share-root DIR]...]]\n",
    "       runtime-adapter-host (reattach | stop) STORE [--guest-build BUILD]\n",
    "       runtime-adapter-host probe STORE PROBE... [--guest-build BUILD]\n",
    "       runtime-adapter-host exec STORE [--guest-build BUILD] EXECUTION [';' EXECUTION]...\n",
    "           EXECUTION: [--identity UID:GID [--groups GID,...] [--capabilities N,...]",
    " [--numeric]]\n",
    "                      [--cwd DIR] [--env NAME=VALUE]... -- PROGRAM [ARG]...",
);

/// What the program was asked to do.
enum Command {
    /// Report the contract that the program was built against.
    Report,
    /// Stage the images of a layered root.
    Stage {
        /// The sandbox's store.
        store: PathBuf,
        /// The image.
        distro: PathBuf,
        /// The image's format.
        format: ImageFormat,
        /// The identity that the guest expects of the image.
        identity: Uuid,
        /// The ext4 scratch image, if any.
        scratch: Option<PathBuf>,
    },
    /// Launch a VM of the guest agent.
    Launch {
        /// The sandbox's store.
        store: PathBuf,
        /// The guest agent's executable.
        guest_agent: PathBuf,
        /// The hypervisor, unless `nvx::Hypervisor::from_env_or_default` selects it.
        hypervisor: Option<Hypervisor>,
        /// The guest agent's build.
        guest_build: String,
        /// The VM's layered root, if it has one.
        layered: Option<Box<Layered>>,
        /// The step hook, for tests, if `RUNTIME_ADAPTER_STEP_HOOK` sets one.
        hook: Option<StepHook>,
    },
    /// Reattach to the VM, and stop it if asked.
    Reattach {
        /// The sandbox's store.
        store: PathBuf,
        /// The guest agent's build.
        guest_build: String,
        /// Whether to stop the VM.
        stop: bool,
    },
    /// Reattach to the VM, and run probes in its layered root.
    Probe {
        /// The sandbox's store.
        store: PathBuf,
        /// The guest agent's build.
        guest_build: String,
        /// The probes, in order.
        probes: Vec<Probe>,
    },
    /// Reattach to the VM, and run executions in its layered root.
    Exec {
        /// The sandbox's store.
        store: PathBuf,
        /// The guest agent's build.
        guest_build: String,
        /// The executions, in order.
        executions: Vec<Execution>,
    },
}

/// An execution of `exec`, as its options and arguments give it.
#[derive(Default)]
struct Execution {
    /// `--identity`: the user ID and the group ID.
    identity: Option<(u32, u32)>,
    /// `--groups`.
    groups: Option<Vec<u32>>,
    /// `--capabilities`: the set, a bit for each number.
    capabilities: Option<u64>,
    /// `--numeric`.
    numeric: bool,
    /// `--cwd`.
    working_directory: Option<String>,
    /// `--env`, in order.
    environment: Vec<String>,
    /// The program's path, then its arguments.
    arguments: Vec<String>,
}

impl Execution {
    /// Returns the start of the execution, as the execution `key`.
    fn start(&self, key: Key) -> Start {
        let arguments = self.arguments.iter().map(String::as_str);
        let mut start = Start::new(key, exec_limits(), exec_timers(), arguments)
            .with_environment(self.environment.iter().map(String::as_str));
        start.identity = self.identity.map(|(uid, gid)| exec::Identity {
            groups: self.groups.clone().unwrap_or_default(),
            capabilities: self.capabilities.unwrap_or(0),
            account: if self.numeric {
                AccountMode::Numeric
            } else {
                AccountMode::Existing
            },
            ..exec::Identity::new(uid, gid)
        });
        start.working_directory = self.working_directory.clone().map(String::into_bytes);
        start
    }
}

/// Returns the limits of an execution of `exec`: chunks of [`EXEC_CHUNK`] bytes, and frame-counted
/// credit, of [`EXEC_INPUT_FRAMES`] for the input, and of [`EXEC_OUTPUT_FRAMES`] in one pool that
/// standard output and error share.
fn exec_limits() -> Limits {
    Limits::new(
        EXEC_CHUNK,
        Window::frames(EXEC_INPUT_FRAMES),
        Window::frames(EXEC_OUTPUT_FRAMES),
    )
    .expect("the limits of `exec` hold no empty window, and a chunk that the framing carries")
    .shared_output()
}

/// Returns the timers of both sides of an execution of `exec`.
fn exec_timers() -> Timers {
    Timers::new(EXEC_GRACE, EXEC_SETTLEMENT).timeout(EXEC_TIMEOUT)
}

/// The layered root of a launch, as its options give it.
struct Layered {
    /// The image.
    distro: PathBuf,
    /// The image's format.
    format: ImageFormat,
    /// The identity that the guest expects of the image.
    identity: Uuid,
    /// The ext4 scratch image of the upper, or `None` for an upper in RAM.
    scratch: Option<PathBuf>,
    /// The guest's runtime directory, unless the default.
    runtime: Option<String>,
    /// The overlay's mode, unless the guest's default.
    overlay_mode: Option<OverlayMode>,
    /// Whether the scratch may have a journal, unless the guest's default.
    scratch_journal: Option<JournalPolicy>,
    /// The most bytes that the first partition of a GPT disk may hold, unless the guest's default.
    first_partition_bound: Option<u64>,
    /// The workload identity of the whole sandbox, if any: its user ID and its group ID.
    workload_identity: Option<(u32, u32)>,
    /// The host paths that the launch shares with the root.
    shares: Shares,
    /// The file of the VM's network, if it has one.
    network: Option<PathBuf>,
}

/// The options of a command.
#[derive(Default)]
struct Options {
    /// `--guest-agent`.
    guest_agent: Option<PathBuf>,
    /// `--hypervisor`.
    hypervisor: Option<Hypervisor>,
    /// `--guest-build`.
    guest_build: Option<String>,
    /// `--distro`: the image's format and its file.
    distro: Option<(ImageFormat, PathBuf)>,
    /// `--distro-identity`.
    identity: Option<Uuid>,
    /// `--upper`: the scratch image, or `None` for RAM.
    upper: Option<Option<PathBuf>>,
    /// `--runtime-dir`.
    runtime: Option<String>,
    /// `--overlay-mode`.
    overlay_mode: Option<OverlayMode>,
    /// `--scratch-journal`.
    scratch_journal: Option<JournalPolicy>,
    /// `--first-partition-bound`.
    first_partition_bound: Option<u64>,
    /// `--workload-identity`.
    workload_identity: Option<(u32, u32)>,
    /// `--share`, `--deny`, `--reserved`, and `--share-root`, in order.
    shares: Shares,
    /// `--network`.
    network: Option<PathBuf>,
}

fn main() -> ExitCode {
    let command = match parse(env::args_os().skip(1).collect()) {
        Ok(command) => command,
        Err(problem) => {
            eprintln!("runtime-adapter-host: {problem}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match command {
        Command::Report => {
            println!(
                "runtime-adapter-host: {}; supported host: {}",
                runtime_adapter::contract(),
                nvx::host::platform::SUPPORTED,
            );
            return ExitCode::SUCCESS;
        }
        Command::Stage {
            store,
            distro,
            format,
            identity,
            scratch,
        } => stage(&store, &distro, format, identity, scratch.as_deref()),
        Command::Launch {
            store,
            guest_agent,
            hypervisor,
            guest_build,
            layered,
            hook,
        } => launch(
            &store,
            &guest_agent,
            hypervisor,
            &guest_build,
            layered.as_deref(),
            hook.as_ref(),
        ),
        Command::Reattach {
            store,
            guest_build,
            stop,
        } => reattach(&store, &guest_build, stop),
        Command::Probe {
            store,
            guest_build,
            probes,
        } => probe(&store, &guest_build, &probes),
        Command::Exec {
            store,
            guest_build,
            executions,
        } => exec(&store, &guest_build, &executions),
    };
    result.unwrap_or_else(|problem| {
        eprintln!("runtime-adapter-host: {problem}");
        ExitCode::FAILURE
    })
}

/// Parses the program's arguments, with the step hook that `RUNTIME_ADAPTER_STEP_HOOK` sets for a
/// launch, or says why they are not a command.
fn parse(arguments: Vec<OsString>) -> Result<Command, String> {
    let mut arguments = arguments.into_iter();
    let Some(name) = arguments.next() else {
        return Ok(Command::Report);
    };
    let name = match name.to_str() {
        Some(name @ ("stage" | "launch" | "reattach" | "stop" | "probe" | "exec")) => {
            name.to_owned()
        }
        _ => return Err(format!("unknown command {name:?}")),
    };
    let store = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| format!("{name} needs a sandbox store"))?,
    );
    if name == "exec" {
        return parse_exec(store, arguments);
    }
    let mut options = Options::default();
    let mut probes = Vec::new();
    while let Some(argument) = arguments.next() {
        let Some(option) = argument.to_str().filter(|option| option.starts_with("--")) else {
            if name != "probe" {
                return Err(format!("unexpected argument {argument:?} of {name}"));
            }
            let text = argument
                .to_str()
                .ok_or_else(|| format!("the probe {argument:?} is not Unicode"))?;
            probes.push(Probe::parse(text).map_err(|error| error.problem().to_owned())?);
            continue;
        };
        let value = arguments
            .next()
            .ok_or_else(|| format!("{option:?} needs a value"))?;
        parse_option(&mut options, &name, option, value)?;
    }
    let guest_build = options
        .guest_build
        .unwrap_or_else(|| GUEST_BUILD.to_owned());
    Ok(match name.as_str() {
        "stage" => {
            let (format, distro) = options.distro.ok_or("stage needs --distro")?;
            Command::Stage {
                store,
                distro,
                format,
                identity: options.identity.ok_or("stage needs --distro-identity")?,
                scratch: options.upper.flatten(),
            }
        }
        "launch" => {
            let shares = &options.shares;
            let share_options = [
                ("--deny", !shares.denied.is_empty()),
                ("--reserved", !shares.reserved.is_empty()),
                ("--share-root", !shares.roots.is_empty()),
            ];
            if shares.shares.is_empty()
                && let Some((option, _)) = share_options.iter().find(|(_, given)| *given)
            {
                return Err(format!("{option} needs --share"));
            }
            let layered = match (options.distro, options.identity, options.upper) {
                (Some(_), None, _) => return Err("--distro needs --distro-identity".to_owned()),
                (Some(_), Some(_), None) => return Err("--distro needs --upper".to_owned()),
                (Some(_), Some(_), Some(None)) if options.scratch_journal.is_some() => {
                    return Err("--scratch-journal needs --upper scratch:PATH".to_owned());
                }
                (Some((ImageFormat::Erofs, _)), Some(_), Some(_))
                    if options.first_partition_bound.is_some() =>
                {
                    return Err("--first-partition-bound needs --distro gpt:PATH".to_owned());
                }
                (Some((format, distro)), Some(identity), Some(scratch)) => {
                    Some(Box::new(Layered {
                        distro,
                        format,
                        identity,
                        scratch,
                        runtime: options.runtime,
                        overlay_mode: options.overlay_mode,
                        scratch_journal: options.scratch_journal,
                        first_partition_bound: options.first_partition_bound,
                        workload_identity: options.workload_identity,
                        shares: options.shares,
                        network: options.network,
                    }))
                }
                (None, Some(_), _) => {
                    return Err("--distro-identity needs --distro".to_owned());
                }
                (None, None, Some(_)) => return Err("--upper needs --distro".to_owned()),
                (None, None, None) => {
                    let layered = [
                        ("--runtime-dir", options.runtime.is_some()),
                        ("--overlay-mode", options.overlay_mode.is_some()),
                        ("--scratch-journal", options.scratch_journal.is_some()),
                        (
                            "--first-partition-bound",
                            options.first_partition_bound.is_some(),
                        ),
                        ("--workload-identity", options.workload_identity.is_some()),
                        ("--share", !options.shares.shares.is_empty()),
                        ("--network", options.network.is_some()),
                    ];
                    if let Some((option, _)) = layered.iter().find(|(_, given)| *given) {
                        return Err(format!("{option} needs --distro"));
                    }
                    None
                }
            };
            Command::Launch {
                store,
                guest_agent: options.guest_agent.ok_or("launch needs --guest-agent")?,
                hypervisor: options.hypervisor,
                guest_build,
                layered,
                hook: StepHook::from_env()?,
            }
        }
        "probe" if probes.is_empty() => return Err("probe needs a probe".to_owned()),
        "probe" => Command::Probe {
            store,
            guest_build,
            probes,
        },
        stop => Command::Reattach {
            store,
            guest_build,
            stop: stop == "stop",
        },
    })
}

/// Parses `option` of the command `name`, with `value`, into `options`, or says why it is not one
/// of the command's options, or why its value is invalid.
fn parse_option(
    options: &mut Options,
    name: &str,
    option: &str,
    value: OsString,
) -> Result<(), String> {
    let text = |value: OsString| {
        value
            .into_string()
            .map_err(|value| format!("the value {value:?} of {option} is not Unicode"))
    };
    match (name, option) {
        ("launch", "--guest-agent") if options.guest_agent.is_none() => {
            options.guest_agent = Some(PathBuf::from(value));
        }
        ("launch", "--hypervisor") if options.hypervisor.is_none() => {
            let hypervisor = text(value)?.parse().map_err(|error| describe(&error))?;
            options.hypervisor = Some(hypervisor);
        }
        ("launch" | "stage", "--distro") if options.distro.is_none() => {
            let value = text(value)?;
            let distro = value.split_once(':').and_then(|(format, path)| {
                let format = ImageFormat::from_name(format)?;
                (!path.is_empty()).then(|| (format, PathBuf::from(path)))
            });
            options.distro = Some(distro.ok_or_else(|| {
                format!("--distro takes FORMAT:PATH, with the format erofs or gpt, not {value:?}")
            })?);
        }
        ("launch" | "stage", "--distro-identity") if options.identity.is_none() => {
            let value = text(value)?;
            let identity = Uuid::parse(&value)
                .ok_or_else(|| format!("--distro-identity takes a UUID, not {value:?}"))?;
            options.identity = Some(identity);
        }
        ("launch" | "stage", "--upper") if options.upper.is_none() => {
            let value = text(value)?;
            // Staging binds a scratch image; only a launch puts the upper in RAM.
            let upper = match value.split_once(':') {
                _ if value == "ram" && name == "launch" => Some(None),
                Some(("scratch", path)) if !path.is_empty() => Some(Some(PathBuf::from(path))),
                _ => None,
            };
            options.upper = Some(upper.ok_or_else(|| match name {
                "stage" => format!("--upper of stage takes scratch:PATH, not {value:?}"),
                _ => format!("--upper takes scratch:PATH or ram, not {value:?}"),
            })?);
        }
        ("launch", "--runtime-dir") if options.runtime.is_none() => {
            options.runtime = Some(text(value)?);
        }
        ("launch", "--overlay-mode") if options.overlay_mode.is_none() => {
            let value = text(value)?;
            let mode = OverlayMode::from_name(&value).ok_or_else(|| {
                format!("--overlay-mode takes trusted-lowers or untrusted-lowers, not {value:?}")
            })?;
            options.overlay_mode = Some(mode);
        }
        ("launch", "--scratch-journal") if options.scratch_journal.is_none() => {
            let value = text(value)?;
            let policy = JournalPolicy::from_name(&value).ok_or_else(|| {
                format!("--scratch-journal takes allowed or forbidden, not {value:?}")
            })?;
            options.scratch_journal = Some(policy);
        }
        ("launch", "--first-partition-bound") if options.first_partition_bound.is_none() => {
            let value = text(value)?;
            let bound = value.parse::<u64>().ok().filter(|bound| *bound > 0);
            options.first_partition_bound = Some(bound.ok_or_else(|| {
                format!("--first-partition-bound takes a positive number of bytes, not {value:?}")
            })?);
        }
        ("launch", "--workload-identity") if options.workload_identity.is_none() => {
            let value = text(value)?;
            let identity = ids(&value)
                .filter(|(uid, gid)| [uid, gid].into_iter().all(|id| (1..u32::MAX).contains(id)));
            options.workload_identity = Some(identity.ok_or_else(|| {
                format!(
                    "--workload-identity takes UID:GID, two numbers from 1 to {}, not {value:?}",
                    u32::MAX - 1
                )
            })?);
        }
        ("launch", "--share") => {
            let value = text(value)?;
            let share = Share::parse(&value).ok_or_else(|| {
                format!(
                    "--share takes HOST:TARGET or HOST:TARGET:ro, with an absolute guest path \
                     TARGET, not {value:?}"
                )
            })?;
            options.shares.shares.push(share);
        }
        ("launch", "--deny") => options.shares.denied.push(PathBuf::from(value)),
        ("launch", "--reserved") => options.shares.reserved.push(text(value)?),
        ("launch", "--share-root") => options.shares.roots.push(PathBuf::from(value)),
        ("launch", "--network") if options.network.is_none() => {
            options.network = Some(PathBuf::from(value));
        }
        (_, "--guest-build") if name != "stage" && options.guest_build.is_none() => {
            options.guest_build = Some(text(value)?);
        }
        _ => return Err(format!("unexpected option {option:?} of {name}")),
    }
    Ok(())
}

/// Parses the arguments of `exec` after its sandbox store `store`: its executions, each ended by
/// a lone `;` or the end of the arguments, with the options before them, or says why they are not
/// the command's.
fn parse_exec(
    store: PathBuf,
    arguments: impl Iterator<Item = OsString>,
) -> Result<Command, String> {
    let mut arguments = arguments.map(|argument| {
        argument
            .into_string()
            .map_err(|argument| format!("the argument {argument:?} of exec is not Unicode"))
    });
    let mut guest_build = None;
    let mut executions = Vec::new();
    loop {
        let mut execution = Execution::default();
        loop {
            let option = arguments
                .next()
                .ok_or("exec needs `-- PROGRAM` after the options of each execution")??;
            match option.as_str() {
                "--" => break,
                "--numeric" if !execution.numeric => {
                    execution.numeric = true;
                    continue;
                }
                _ => {}
            }
            let value = arguments
                .next()
                .ok_or_else(|| format!("{option:?} needs a value"))??;
            match option.as_str() {
                "--guest-build" if guest_build.is_none() => guest_build = Some(value),
                "--identity" if execution.identity.is_none() => {
                    let identity = ids(&value)
                        .ok_or_else(|| format!("--identity takes UID:GID, not {value:?}"))?;
                    execution.identity = Some(identity);
                }
                "--groups" if execution.groups.is_none() => {
                    let groups =
                        numbers(&value, |group| group.parse::<u32>().ok()).ok_or_else(|| {
                            format!("--groups takes GIDs, separated by commas, not {value:?}")
                        })?;
                    execution.groups = Some(groups);
                }
                "--capabilities" if execution.capabilities.is_none() => {
                    let capabilities = numbers(&value, |number| {
                        number.parse::<u8>().ok().filter(|number| *number < 64)
                    })
                    .ok_or_else(|| {
                        format!(
                            "--capabilities takes capability numbers from 0 to 63, separated by \
                             commas, not {value:?}"
                        )
                    })?;
                    let set = capabilities
                        .into_iter()
                        .fold(0, |set, number| set | 1 << number);
                    execution.capabilities = Some(set);
                }
                "--cwd" if execution.working_directory.is_none() => {
                    execution.working_directory = Some(value);
                }
                "--env" => execution.environment.push(value),
                _ => return Err(format!("unexpected option {option:?} of exec")),
            }
        }
        let mut more = false;
        for argument in arguments.by_ref() {
            let argument = argument?;
            if argument == EXEC_END {
                more = true;
                break;
            }
            execution.arguments.push(argument);
        }
        if execution.arguments.is_empty() {
            return Err("exec needs a program after `--`".to_owned());
        }
        if execution.identity.is_none() {
            let qualifiers = [
                ("--groups", execution.groups.is_some()),
                ("--capabilities", execution.capabilities.is_some()),
                ("--numeric", execution.numeric),
            ];
            if let Some((option, _)) = qualifiers.iter().find(|(_, given)| *given) {
                return Err(format!("{option} needs --identity"));
            }
        }
        // The guest ends its session on a start that it cannot decode, so the program refuses
        // one that breaks the rules of the encoding, as a relative working directory or an
        // environment entry without a name does, or that the framing does not carry, before it
        // attaches.
        let encoded = execution.start(Key::new(0, 0)).encode();
        Start::decode(&encoded).map_err(|error| error.problem().to_owned())?;
        Message::ExecStart(encoded)
            .encode()
            .map_err(|error| format!("the start of an execution is too long: {error}"))?;
        executions.push(execution);
        if !more {
            break;
        }
    }
    // The driver runs no more executions at once than the guest does.
    if executions.len() > exec::MAX_EXECUTIONS {
        return Err(format!(
            "exec runs at most {} executions, not {}",
            exec::MAX_EXECUTIONS,
            executions.len()
        ));
    }
    Ok(Command::Exec {
        store,
        guest_build: guest_build.unwrap_or_else(|| GUEST_BUILD.to_owned()),
        executions,
    })
}

/// Returns the user ID and the group ID of `value`, `UID:GID`, or `None` if it is not that.
fn ids(value: &str) -> Option<(u32, u32)> {
    let (uid, gid) = value.split_once(':')?;
    Some((uid.parse().ok()?, gid.parse().ok()?))
}

/// Returns the numbers of `value`, separated by commas, which `parse` reads, or `None` if one is
/// not a number that it accepts, or `value` is empty.
fn numbers<T>(value: &str, parse: impl Fn(&str) -> Option<T>) -> Option<Vec<T>> {
    value.split(',').map(parse).collect()
}

/// Stages the image `distro`, in `format`, of which the guest expects `identity`, and the ext4
/// scratch image `scratch`, if any, in the sandbox store `store`, which it creates if needed.
fn stage(
    store: &Path,
    distro: &Path,
    format: ImageFormat,
    identity: Uuid,
    scratch: Option<&Path>,
) -> Result<ExitCode, String> {
    // Verification may hash the image, so it runs before the sandbox is locked.
    let staged =
        Staged::stage(distro, format, identity, scratch).map_err(|error| describe(&error))?;
    let store = SandboxStore::create(store).map_err(|error| describe(&error))?;
    let _locked = store
        .lock(Some(Instant::now() + LOCK_TIMEOUT))
        .map_err(|error| describe(&error))?;
    staged
        .write(store.dir())
        .map_err(|error| describe(&error))?;
    Ok(ExitCode::SUCCESS)
}

/// Launches a VM of the guest agent `guest_agent` in the sandbox store `store`, on `hypervisor`
/// or else the one that `nvx::Hypervisor::from_env_or_default` selects, with the guest agent's
/// build `guest_build`, and with the layered root `layered`, if any. The step hook `hook`, if any,
/// acts at its step of the launch.
fn launch(
    store: &Path,
    guest_agent: &Path,
    hypervisor: Option<Hypervisor>,
    guest_build: &str,
    layered: Option<&Layered>,
    hook: Option<&StepHook>,
) -> Result<ExitCode, String> {
    // A network that the plan refuses fails the launch before it needs NVX's artifacts or the
    // store.
    let network = layered
        .and_then(|layered| layered.network.as_deref())
        .map(read_network)
        .transpose()
        .map_err(|error| describe(&error))?;
    let hypervisor = match hypervisor {
        Some(hypervisor) => hypervisor,
        None => Hypervisor::from_env_or_default().map_err(|error| describe(&error))?,
    };
    let runtime = runtime(guest_build)?;
    runtime
        .artifacts
        .probe()
        .map_err(|error| describe(&error))?;
    let mut layered = layered
        .map(|layered| -> nvx::Result<LayeredLaunch> {
            let mut launch = LayeredLaunch::new(
                &layered.distro,
                layered.format,
                layered.identity,
                layered.scratch.as_deref(),
                layered.runtime.as_deref(),
            )?;
            launch.config.overlay_mode = layered.overlay_mode;
            launch.config.scratch_journal = layered.scratch_journal;
            launch.config.first_partition_bound = layered.first_partition_bound;
            launch.config.workload_identity = layered.workload_identity;
            // The launch refuses shares that it cannot export before it touches the store.
            if let Some(export) = layered.shares.plan()? {
                launch.share(export)?;
            }
            launch.plan = launch.plan.with_network(network);
            Ok(launch)
        })
        .transpose()
        .map_err(|error| describe(&error))?;
    let store = SandboxStore::create(store).map_err(|error| describe(&error))?;
    let mut locked = store
        .lock(Some(Instant::now() + LOCK_TIMEOUT))
        .map_err(|error| describe(&error))?;
    // The launch checks the bindings that the store stages for its images before it spawns
    // anything. The prepared launch holds the files that the checks opened until the guest has
    // assembled the root, or until a launch that failed is torn down, which ends its VM first:
    // the launch, declared after it, drops first.
    let mut prepared = None;
    if let Some(layered) = &mut layered {
        if let Some(staged) = Staged::read(store.dir()).map_err(|error| describe(&error))? {
            layered.bindings = staged.bindings(&layered.plan);
        }
        prepared = Some(layered.prepare().map_err(|error| binding_error(&error))?);
    }
    step(hook, Step::Checked)?;
    let capability = generate_capability().map_err(|error| describe(&error))?;
    let initramfs = store.dir().join(INITRAMFS_NAME);
    let spec = LaunchSpec::new(
        Artifacts::new(
            &runtime.artifacts.openvmm,
            &runtime.artifacts.kernel,
            &initramfs,
        ),
        hypervisor,
    );
    let mut deadline = Instant::now() + START_TIMEOUT;
    let starting = supervisor::launch(&mut locked, &runtime.identity, &capability, |endpoint| {
        // The launch has checked that no VM of the sandbox runs, which could still read the
        // initramfs of an earlier launch.
        initramfs::write(&runtime.artifacts.initrd, guest_agent, &initramfs).map_err(|error| {
            nvx::Error::io("cannot write the initramfs of the guest agent").with_source(error)
        })?;
        let outcome = store.outcome_path();
        let (arguments, kernel_command_line) = match layered.as_ref().zip(prepared.as_ref()) {
            Some((layered, prepared)) => (
                layered.openvmm_arguments(
                    prepared,
                    hypervisor,
                    &spec.kernel,
                    &spec.initrd,
                    endpoint,
                    &outcome,
                ),
                prepared.kernel_command_line().to_owned(),
            ),
            None => (
                spec.openvmm_arguments(endpoint, &outcome)?,
                spec.kernel_command_line()?,
            ),
        };
        let record = LaunchRecord {
            kernel_command_line: &kernel_command_line,
            layered: layered.as_ref(),
        };
        record.write(store.dir())?;
        let mut command = VmmCommand::new(&spec.openvmm, arguments, store.dir());
        // OpenVMM inherits the checked files that the prepared launch passes, and nothing else.
        if let Some(prepared) = &prepared {
            command.inherit = prepared.inherited();
        }
        Ok(command)
    })
    .map_err(|error| describe(&error))?;
    let process = starting.process().reference().clone();
    println!("pid={}", process.pid);
    // A launch that fails from here on ends its VM, once `starting` drops. Its deadline leaves out
    // the time that the step hook pauses it.
    deadline += step(hook, Step::Spawned)?;
    let failed = |problem: String| {
        format!(
            "{problem}; see the VMM's log {}",
            store.log_path().display()
        )
    };
    let transport = session::connect(
        starting.endpoint(),
        process.pid,
        process.start_time,
        deadline,
    )
    .map_err(|error| failed(format!("cannot connect to the VM: {}", describe(&error))))?;
    let mut client = attach(transport, &capability, deadline).map_err(failed)?;
    if let Some(layered) = &layered {
        match host::ready(&mut client, &layered.config, guest_build, deadline) {
            Ok(ready) => {
                println!("root=ready");
                println!("build={}", escape(ready.build()));
                // The guest has assembled the root, so OpenVMM holds the images: the checked files
                // can go, which releases their guards, while OpenVMM keeps the scratch image's
                // claim.
                drop(prepared);
                deadline += step(hook, Step::Ready)?;
            }
            Err(OrderError::Root(failure)) => {
                print_failure(&failure);
                drop(client);
                return launch_failed(starting, &store, &failure).map_err(failed);
            }
            // The host sends a guest agent of another build nothing more, and the VM ends once
            // `starting` drops.
            Err(OrderError::Build { reported, expected }) => {
                println!("root=ready");
                println!("build={}", escape(&reported));
                return Err(failed(format!(
                    "the guest agent is of build {reported}, not of the host's build {expected}"
                )));
            }
            Err(error) => {
                return Err(failed(format!(
                    "the guest agent did not answer the root configuration: {}",
                    describe(&error)
                )));
            }
        }
        let table = layered.table();
        match host::start_sandbox(&mut client, &table, deadline) {
            Ok(()) => {
                println!("table=applied");
                println!("mapped={}", table.len());
                println!("sandbox=started");
            }
            Err(OrderError::Mapping { failure, stop }) => {
                println!("table=failed");
                println!("mapped={}", failure.applied());
                println!("problem={}", escape(failure.problem()));
                drop(client);
                return table_failed(starting, &store, stop).map_err(failed);
            }
            Err(error) => {
                return Err(failed(format!(
                    "the guest agent did not start the sandbox: {}",
                    describe(&error)
                )));
            }
        }
    }
    client
        .ping(deadline)
        .and_then(|()| client.echo(&echo_body(), deadline))
        .map_err(|error| {
            failed(format!(
                "the guest agent did not answer: {}",
                describe(&error)
            ))
        })?;
    step(hook, Step::Commit)?;
    starting.commit().map_err(|error| describe(&error))?;
    Ok(ExitCode::SUCCESS)
}

/// Runs the step hook `hook`, if any, at the launch's `step`, and returns how long it paused the
/// launch there, which the launch leaves out of its deadline.
fn step(hook: Option<&StepHook>, step: Step) -> Result<Duration, String> {
    match hook {
        Some(hook) => hook
            .at(step)
            .map_err(|error| format!("the step hook cannot pause the launch at {step}: {error}")),
        None => Ok(Duration::ZERO),
    }
}

/// Ends the launch `starting` of the sandbox `store`, whose guest reported `failure` and ends: it
/// waits for the VM to exit, abandons the launch, which terminates a VM that still runs, and
/// checks that the VM ended with the failed stage's status and a complete teardown.
fn launch_failed(
    starting: Starting<'_>,
    store: &SandboxStore,
    failure: &RootFailure,
) -> Result<ExitCode, String> {
    let exited = starting
        .process()
        .wait(Instant::now() + STOP_TIMEOUT)
        .map_err(|error| format!("cannot wait for the VM to exit: {error}"))?;
    starting.abandon().map_err(|error| describe(&error))?;
    println!("stopped={}", if exited { "graceful" } else { "forced" });
    if !exited {
        return Err("the VM did not exit once its guest reported the failure".to_owned());
    }
    check_outcome(store, "guest-exit", failure.stage().exit_status().into())?;
    Ok(ExitCode::from(ROOT_FAILED))
}

/// Ends the launch `starting` of the sandbox `store`, whose guest failed a mapping of the table,
/// and which the host asked to stop, as `stop` tells how the stop ended: once the guest agent
/// confirmed the stop, it waits for the VM to exit, then abandons the launch, which terminates a
/// VM that still runs, and checks that the VM stopped gracefully, with success and a complete
/// teardown. A guest agent that did not confirm the stop, or reported that the root's teardown
/// failed, fails the launch.
fn table_failed(
    starting: Starting<'_>,
    store: &SandboxStore,
    stop: Result<Option<RootFailure>, ClientError>,
) -> Result<ExitCode, String> {
    let exited = match &stop {
        Ok(_) => starting
            .process()
            .wait(Instant::now() + STOP_TIMEOUT)
            .map_err(|error| format!("cannot wait for the VM to exit: {error}"))?,
        Err(_) => false,
    };
    starting.abandon().map_err(|error| describe(&error))?;
    println!("stopped={}", if exited { "graceful" } else { "forced" });
    let failure = stop.map_err(|error| {
        format!(
            "the guest agent did not confirm the stop: {}",
            describe(&error)
        )
    })?;
    if !exited {
        return Err("the VM did not exit once the guest agent confirmed the stop".to_owned());
    }
    if let Some(failure) = failure {
        return Err(format!("once the mapping table failed, {failure}"));
    }
    check_outcome(store, "success", 0)?;
    Ok(ExitCode::from(TABLE_FAILED))
}

/// Prints the root's `failure`.
fn print_failure(failure: &RootFailure) {
    println!("root=failed");
    println!("stage={}", failure.stage());
    println!("problem={}", escape(failure.problem()));
}

/// Describes `error`, of a launch that could not check the bindings of its images or prepare their
/// handoff: a binding that the store does not stage, or that no longer holds, needs the images
/// staged again, while a scratch image that another sandbox claims, and a failure of the host, do
/// not.
fn binding_error(error: &nvx::Error) -> String {
    match error.kind() {
        ErrorKind::Unavailable => format!("{}; stage the images first", describe(error)),
        _ => describe(error),
    }
}

/// Returns the body of the launch's `Echo`: the longest that the framing allows, of every byte
/// value, which spans several of the session's records, each way.
fn echo_body() -> Vec<u8> {
    (0..=u8::MAX).cycle().take(MAX_BODY).collect()
}

/// Reattaches to the VM of the sandbox store `store`, with the guest agent's build `guest_build`,
/// attaches a session to it, and exchanges a `Ping`, then stops the VM if `stop` asks.
fn reattach(store: &Path, guest_build: &str, stop: bool) -> Result<ExitCode, String> {
    let runtime = runtime(guest_build)?;
    let store = SandboxStore::open(store).map_err(|error| describe(&error))?;
    let mut locked = store
        .lock(Some(Instant::now() + LOCK_TIMEOUT))
        .map_err(|error| describe(&error))?;
    let vmm = match hold(&mut locked, &runtime)? {
        Ok(vmm) => vmm,
        Err(status) => return Ok(status),
    };
    let deadline = Instant::now() + SESSION_TIMEOUT;
    let attached = store
        .read_capability()
        .map_err(|error| describe(&error))
        .and_then(|capability| connect(&vmm, &capability, deadline))
        .and_then(|mut client| {
            client
                .ping(deadline)
                .map_err(|error| format!("the guest agent did not answer: {}", describe(&error)))?;
            Ok(client)
        });
    if !stop {
        return attached.map(|_| ExitCode::SUCCESS);
    }
    // A VM whose guest agent cannot confirm the stop is terminated.
    let mut failure = None;
    let stopped = vmm
        .stop(
            &mut locked,
            Instant::now() + STOP_TIMEOUT,
            Some(FORCED_TIMEOUT),
            |_, deadline| {
                failure = attached?.stop(deadline).map_err(|error| {
                    format!(
                        "the guest agent did not confirm the stop: {}",
                        describe(&error)
                    )
                })?;
                Ok(())
            },
        )
        .map_err(|error| describe(&error))?;
    println!(
        "stopped={}",
        match stopped.outcome {
            StopOutcome::Graceful => "graceful",
            StopOutcome::Forced => "forced",
            StopOutcome::TimedOut => "timed-out",
        }
    );
    if let Some(problem) = stopped.graceful_error {
        return Err(problem);
    }
    if stopped.outcome != StopOutcome::Graceful {
        return Err("the VM did not exit once the guest agent confirmed the stop".to_owned());
    }
    match failure {
        None => {
            check_outcome(&store, "success", 0)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(failure) => {
            print_failure(&failure);
            check_outcome(&store, "guest-exit", failure.stage().exit_status().into())?;
            Ok(ExitCode::from(ROOT_FAILED))
        }
    }
}

/// Reattaches to the VM of the sandbox store `store`, with the guest agent's build `guest_build`,
/// attaches a session to it, and runs `probes` in the VM's layered root, printing the facts that
/// each found.
fn probe(store: &Path, guest_build: &str, probes: &[Probe]) -> Result<ExitCode, String> {
    let runtime = runtime(guest_build)?;
    let store = SandboxStore::open(store).map_err(|error| describe(&error))?;
    let mut locked = store
        .lock(Some(Instant::now() + LOCK_TIMEOUT))
        .map_err(|error| describe(&error))?;
    let vmm = match hold(&mut locked, &runtime)? {
        Ok(vmm) => vmm,
        Err(status) => return Ok(status),
    };
    let deadline = Instant::now() + SESSION_TIMEOUT;
    let capability = store.read_capability().map_err(|error| describe(&error))?;
    let mut client = connect(&vmm, &capability, deadline)?;
    for probe in probes {
        let reply = client
            .probe(probe, deadline)
            .map_err(|error| format!("the guest agent did not answer: {}", describe(&error)))?;
        match reply {
            ProbeReply::Report(facts) => {
                for (key, value) in facts.iter() {
                    println!("{}.{key}={}", probe.name(), escape(value));
                }
            }
            ProbeReply::Failed(problem) => return Err(problem),
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Reattaches to the VM of the sandbox store `store`, with the guest agent's build `guest_build`,
/// attaches a session to it, runs `executions` at once in the VM's layered root, and prints what
/// each did once every one finished.
fn exec(store: &Path, guest_build: &str, executions: &[Execution]) -> Result<ExitCode, String> {
    let runtime = runtime(guest_build)?;
    let store = SandboxStore::open(store).map_err(|error| describe(&error))?;
    let mut locked = store
        .lock(Some(Instant::now() + LOCK_TIMEOUT))
        .map_err(|error| describe(&error))?;
    let vmm = match hold(&mut locked, &runtime)? {
        Ok(vmm) => vmm,
        Err(status) => return Ok(status),
    };
    let deadline = Instant::now() + SESSION_TIMEOUT;
    let capability = store.read_capability().map_err(|error| describe(&error))?;
    let mut client = connect(&vmm, &capability, deadline)?;
    // The keys of the attachment's executions are new to the guest: see the module documentation.
    let identity = client.identity();
    let (base, _) = identity.instance_id.split_at(8);
    let base = u64::from_le_bytes(base.try_into().expect("an instance ID has 16 bytes"));
    let mut running = Executions::new();
    let mut ids = Vec::new();
    for (number, execution) in (1..).zip(executions) {
        let key = Key::new(base.wrapping_add(number), identity.epoch);
        running
            .start(&mut client, execution.start(key), exec_timers(), deadline)
            .map_err(|error| format!("cannot start execution {number}: {}", describe(&error)))?;
        ids.push(key.id());
    }
    // Every execution finishes once it ended, which its timeout bounds, and its output's grace and
    // its settlement passed, or once the guest's last messages about it arrived.
    let deadline = Instant::now() + EXEC_TIMEOUT + EXEC_GRACE + EXEC_SETTLEMENT + SESSION_TIMEOUT;
    running
        .run(&mut client, deadline)
        .map_err(|error| format!("the executions failed: {}", describe(&error)))?;
    for (number, id) in (1..).zip(ids) {
        let execution = running.get(id).expect("the driver started every execution");
        print_execution(number, execution);
    }
    Ok(ExitCode::SUCCESS)
}

/// Prints what the execution numbered `number`, `execution`, which finished, did.
fn print_execution(number: u64, execution: &HostExecution) {
    let Some(outcome) = execution.outcome() else {
        unreachable!("every execution finished");
    };
    let launch = match outcome.launch_failure() {
        Some(failure) => format!("failed {} {}", failure.stage(), failure.error()),
        None if execution.peer().has_started() => "started".to_owned(),
        None => "none".to_owned(),
    };
    let status = match outcome.status() {
        Some(Status::Exited(code)) => format!("exit {code}"),
        Some(Status::Signaled(signal)) => format!("signal {signal}"),
        None => "none".to_owned(),
    };
    let truncated = Stream::ALL
        .into_iter()
        .filter(|stream| outcome.is_truncated(*stream))
        .map(stream_name)
        .collect::<Vec<_>>();
    let settled = if outcome.is_reusable() {
        "reusable"
    } else {
        "fenced"
    };
    println!("exec.{number}.launch={launch}");
    println!("exec.{number}.status={status}");
    println!("exec.{number}.cause={}", cause_name(&outcome));
    println!(
        "exec.{number}.truncated={}",
        if truncated.is_empty() {
            "none".to_owned()
        } else {
            truncated.join(",")
        }
    );
    println!("exec.{number}.settled={settled}");
    for stream in Stream::ALL {
        let output = escape_bytes(execution.output(stream));
        println!("exec.{number}.{}={output}", stream_name(stream));
    }
}

/// Returns the name of why a side ended the execution whose outcome is `outcome`, or `none`.
fn cause_name(outcome: &Outcome) -> &'static str {
    match outcome.cause() {
        None => "none",
        Some(Cause::Cancelled) => "cancelled",
        Some(Cause::Dropped) => "dropped",
        Some(Cause::TimedOut) => "timed-out",
        Some(Cause::Reset) => "reset",
        Some(Cause::Disconnected) => "disconnected",
        Some(_) => "other",
    }
}

/// Returns the name of `stream`: `stdout` or `stderr`.
fn stream_name(stream: Stream) -> &'static str {
    match stream {
        Stream::Stdout => "stdout",
        Stream::Stderr => "stderr",
    }
}

/// Returns `bytes` as a value: the UTF-8 text that they hold, escaped as [`escape`] escapes it, and
/// each other byte as `\xHH`, with two hexadecimal digits.
fn escape_bytes(bytes: &[u8]) -> String {
    let mut escaped = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        escaped.push_str(&escape(chunk.valid()));
        for byte in chunk.invalid() {
            escaped.push_str(&format!("\\x{byte:02x}"));
        }
    }
    escaped
}

/// Reattaches to the VM of `locked`, with the runtime identity of `runtime`, and prints what it
/// found. Returns the VM, or the status with which the command ends once it finds the VM
/// mismatched, which it leaves running.
fn hold(locked: &mut LockedStore, runtime: &Runtime) -> Result<Result<Vmm, ExitCode>, String> {
    let reattached =
        supervisor::reattach(locked, &runtime.identity).map_err(|error| describe(&error))?;
    let vmm = match reattached {
        Reattached::Live(vmm) => {
            println!("reattached=live");
            vmm
        }
        Reattached::Mismatched { vmm, differences } => {
            println!("reattached=mismatched");
            println!("differences={}", names(&differences));
            println!("pid={}", vmm.record().process().pid);
            return Ok(Err(ExitCode::from(MISMATCHED)));
        }
        Reattached::Exited { .. } => {
            println!("reattached=exited");
            return Err("no VM of the sandbox runs".to_owned());
        }
        Reattached::Indeterminate(reason) => {
            println!("reattached=indeterminate");
            return Err(format!(
                "whether the VM of the sandbox runs cannot be determined: {reason}"
            ));
        }
    };
    println!("pid={}", vmm.record().process().pid);
    Ok(Ok(vmm))
}

/// Reads the outcome report of the VM of `store`, prints it, and checks that the VM ended with
/// the category `category`, the status code `status`, and a complete teardown.
fn check_outcome(store: &SandboxStore, category: &str, status: i64) -> Result<(), String> {
    let outcome = report::read(&store.outcome_path()).ok_or_else(|| {
        format!(
            "the VM left no outcome report {}",
            store.outcome_path().display()
        )
    })?;
    let found = outcome.category.as_deref().unwrap_or("none");
    let code = outcome
        .status_code
        .map_or("none".to_owned(), |code| code.to_string());
    println!("outcome={found} {code}");
    println!(
        "teardown={}",
        match outcome.teardown_complete {
            Some(true) => "complete",
            Some(false) | None => "incomplete",
        }
    );
    if (found, outcome.status_code, outcome.teardown_complete)
        != (category, Some(status), Some(true))
    {
        return Err(format!(
            "the VM's outcome is not {category} {status} with a complete teardown"
        ));
    }
    Ok(())
}

/// Returns the runtime of NVX's artifacts in the directory that `NVX_ARTIFACTS_DIR` names, with
/// the guest agent's build `guest_build`.
fn runtime(guest_build: &str) -> Result<Runtime, String> {
    let directory = env::var_os(Artifacts::ENV_DIR)
        .filter(|directory| !directory.is_empty())
        .ok_or_else(|| {
            format!(
                "set {} to NVX's artifacts: a repository checkout or an extracted release",
                Artifacts::ENV_DIR
            )
        })?;
    Runtime::from_dir(directory, guest_build).map_err(|error| describe(&error))
}

/// Connects to the control endpoint of the VM `vmm`, and attaches a session with `capability`,
/// by `deadline`.
fn connect(
    vmm: &Vmm,
    capability: &[u8; CAPABILITY_LEN],
    deadline: Instant,
) -> Result<Client, String> {
    let process = vmm.record().process();
    let transport = session::connect(vmm.endpoint(), process.pid, process.start_time, deadline)
        .map_err(|error| format!("cannot connect to the VM: {}", describe(&error)))?;
    attach(transport, capability, deadline)
}

/// Attaches a session over `transport` with `capability` by `deadline`, and prints its identity.
fn attach(
    transport: Box<dyn nvx::host::platform::Transport>,
    capability: &[u8; CAPABILITY_LEN],
    deadline: Instant,
) -> Result<Client, String> {
    let client = Client::attach(transport, capability, deadline)
        .map_err(|error| format!("cannot attach to the VM: {}", describe(&error)))?;
    let identity = client.identity();
    let instance: String = identity
        .instance_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!("instance={instance}");
    println!("epoch={}", identity.epoch);
    Ok(client)
}

/// Returns the names of `parts`, separated by commas.
fn names(parts: &[IdentityPart]) -> String {
    parts
        .iter()
        .map(|part| part.name())
        .collect::<Vec<_>>()
        .join(",")
}

/// Describes `error` with the causes that its description does not end with already.
fn describe(error: &dyn Error) -> String {
    let mut description = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_description = cause.to_string();
        if !description.ends_with(&cause_description) {
            description = format!("{description}: {cause_description}");
        }
        source = cause.source();
    }
    description
}
