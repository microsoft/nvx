//! The runtime adapter's host program.
//!
//! - `runtime-adapter-host` reports the contract that it was built against and whether `nvx`
//!   supports this host.
//! - `runtime-adapter-host launch STORE --guest-agent PATH [--hypervisor NAME] [LAYERED]` launches
//!   a VM in the sandbox store STORE, which it creates if needed, through NVX's supervisor, with
//!   the guest agent PATH, a static `x86_64-unknown-linux-musl` build of `runtime-adapter-guest`,
//!   in place of NVX's agents; see `runtime_adapter::initramfs`. It records what it launches in
//!   STORE; see `runtime_adapter::layered::LaunchRecord`. It attaches to the VM's control
//!   session, exchanges a `Ping` and the longest `Echo` with the guest agent, commits the launch,
//!   and exits, leaving the VM running. The VM runs on the hypervisor NAME, `kvm`, `mshv`, or
//!   `whp`, or else on the one that `nvx::Hypervisor::from_env_or_default` selects.
//!
//!   LAYERED, `--distro FORMAT:PATH [--distro-identity UUID] --upper (scratch:PATH | ram)
//!   [--runtime-dir DIR] [--overlay-mode MODE] [--scratch-journal POLICY]
//!   [--first-partition-bound BYTES]`, gives the VM a layered root, which the guest agent's worker
//!   assembles; see `runtime_adapter::layered` and `runtime_adapter::worker`. Its image is the raw
//!   file PATH, an EROFS filesystem, `erofs`, or a GPT disk, `gpt`, of which the guest expects the
//!   identity UUID, if given: the file system's UUID, or the disk's GUID. Its upper is the ext4
//!   scratch image PATH, or RAM. The guest assembles the root below its runtime directory DIR,
//!   `/run/runtime-adapter` by default, in the overlay mode MODE, `trusted-lowers` or
//!   `untrusted-lowers`, checks a scratch's journal against the policy POLICY, `allowed` or
//!   `forbidden`, and refuses a first partition of a GPT disk larger than BYTES, with the guest's
//!   defaults, `trusted-lowers`, `allowed`, and 64 KiB, unless given. Once attached, the launch
//!   sends the root configuration, and exchanges the `Ping` and the `Echo` once the guest assembled
//!   the root. If the guest reports that a stage of the root failed instead, the launch waits for
//!   the VM to end, abandons it, and checks that the VM ended with the stage's status.
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
//!
//! Each command takes NVX's artifacts from the directory that `NVX_ARTIFACTS_DIR` names, a
//! repository checkout or an extracted release, and the VM's runtime identity from NVX's identity
//! of them, with the guest agent's build that `--guest-build BUILD` names, this crate's version
//! by default; see `runtime_adapter::runtime`. Reattach finds a VM that another runtime identity
//! launched mismatched, and then signals nothing: the VM keeps running.
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
//! - `stage` and `problem`: the stage of the root that failed, such as `assembly`, and the
//!   problem;
//! - `PROBE.KEY`: the fact KEY that the probe named PROBE found;
//! - `stopped`: how the stop ended: `graceful`, `forced`, or `timed-out`;
//! - `outcome`: the category and status code of the VM's outcome report, such as `success 0`;
//! - `teardown`: whether the VM's teardown completed: `complete` or `incomplete`.
//!
//! Values escape `\`, newlines, and carriage returns as `\\`, `\n`, and `\r`.
//!
//! They exit with 0 once they did what they were asked, with 3 when reattach finds the VM
//! mismatched, with 4 when the guest reported that a stage of the root failed, and the VM ended
//! with the stage's status and a complete teardown, and with 1, after a diagnostic on standard
//! error, when they fail. Anything else is a usage error, with status 2.

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use nvx::Hypervisor;
use nvx::host::artifacts::Artifacts;
use nvx::host::instance::generate_capability;
use nvx::host::launch::LaunchSpec;
use nvx::host::report;
use nvx::host::session;
use nvx::host::store::{IdentityPart, LockedStore, SandboxStore};
use nvx::host::supervisor::{self, Reattached, Starting, StopOutcome, Vmm, VmmCommand};
use nvx::protocol::CAPABILITY_LEN;
use nvx::protocol::root::{ImageFormat, RootImage};
use nvx::uuid::Uuid;
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::frame::MAX_BODY;
use runtime_adapter::host::Client;
use runtime_adapter::initramfs;
use runtime_adapter::layered::{LaunchRecord, LayeredLaunch};
use runtime_adapter::probe::{Probe, ProbeReply, escape};
use runtime_adapter::root::{JournalPolicy, OverlayMode, RootFailure, RootReply};
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
/// The command lines that the program accepts.
const USAGE: &str = concat!(
    "usage: runtime-adapter-host\n",
    "       runtime-adapter-host launch STORE --guest-agent PATH [--hypervisor NAME]",
    " [--guest-build BUILD]\n",
    "           [--distro FORMAT:PATH [--distro-identity UUID]",
    " --upper (scratch:PATH | ram) [--runtime-dir DIR]\n",
    "            [--overlay-mode MODE] [--scratch-journal POLICY]",
    " [--first-partition-bound BYTES]]\n",
    "       runtime-adapter-host (reattach | stop) STORE [--guest-build BUILD]\n",
    "       runtime-adapter-host probe STORE PROBE... [--guest-build BUILD]",
);

/// What the program was asked to do.
enum Command {
    /// Report the contract that the program was built against.
    Report,
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
        layered: Option<Layered>,
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
}

/// The layered root of a launch, as its options give it.
struct Layered {
    /// The image.
    distro: PathBuf,
    /// The image's format, and the identity that the guest expects of it, if any.
    image: RootImage,
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
        Command::Launch {
            store,
            guest_agent,
            hypervisor,
            guest_build,
            layered,
        } => launch(
            &store,
            &guest_agent,
            hypervisor,
            &guest_build,
            layered.as_ref(),
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
    };
    result.unwrap_or_else(|problem| {
        eprintln!("runtime-adapter-host: {problem}");
        ExitCode::FAILURE
    })
}

/// Parses the program's arguments, or says why they are not a command.
fn parse(arguments: Vec<OsString>) -> Result<Command, String> {
    let mut arguments = arguments.into_iter();
    let Some(name) = arguments.next() else {
        return Ok(Command::Report);
    };
    let name = match name.to_str() {
        Some(name @ ("launch" | "reattach" | "stop" | "probe")) => name.to_owned(),
        _ => return Err(format!("unknown command {name:?}")),
    };
    let store = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| format!("{name} needs a sandbox store"))?,
    );
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
        "launch" => {
            let layered = match (options.distro, options.upper) {
                (Some(_), Some(None)) if options.scratch_journal.is_some() => {
                    return Err("--scratch-journal needs --upper scratch:PATH".to_owned());
                }
                (Some((ImageFormat::Erofs, _)), Some(_))
                    if options.first_partition_bound.is_some() =>
                {
                    return Err("--first-partition-bound needs --distro gpt:PATH".to_owned());
                }
                (Some((format, distro)), Some(scratch)) => Some(Layered {
                    distro,
                    image: RootImage {
                        format,
                        identity: options.identity,
                    },
                    scratch,
                    runtime: options.runtime,
                    overlay_mode: options.overlay_mode,
                    scratch_journal: options.scratch_journal,
                    first_partition_bound: options.first_partition_bound,
                }),
                (Some(_), None) => return Err("--distro needs --upper".to_owned()),
                (None, _) if options.identity.is_some() => {
                    return Err("--distro-identity needs --distro".to_owned());
                }
                (None, Some(_)) => return Err("--upper needs --distro".to_owned()),
                (None, None) => {
                    let layered = [
                        ("--runtime-dir", options.runtime.is_some()),
                        ("--overlay-mode", options.overlay_mode.is_some()),
                        ("--scratch-journal", options.scratch_journal.is_some()),
                        (
                            "--first-partition-bound",
                            options.first_partition_bound.is_some(),
                        ),
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
        ("launch", "--distro") if options.distro.is_none() => {
            let value = text(value)?;
            let distro = value.split_once(':').and_then(|(format, path)| {
                let format = ImageFormat::from_name(format)?;
                (!path.is_empty()).then(|| (format, PathBuf::from(path)))
            });
            options.distro = Some(distro.ok_or_else(|| {
                format!("--distro takes FORMAT:PATH, with the format erofs or gpt, not {value:?}")
            })?);
        }
        ("launch", "--distro-identity") if options.identity.is_none() => {
            let value = text(value)?;
            let identity = Uuid::parse(&value)
                .ok_or_else(|| format!("--distro-identity takes a UUID, not {value:?}"))?;
            options.identity = Some(identity);
        }
        ("launch", "--upper") if options.upper.is_none() => {
            let value = text(value)?;
            let upper = match value.split_once(':') {
                _ if value == "ram" => Some(None),
                Some(("scratch", path)) if !path.is_empty() => Some(Some(PathBuf::from(path))),
                _ => None,
            };
            options.upper = Some(
                upper.ok_or_else(|| format!("--upper takes scratch:PATH or ram, not {value:?}"))?,
            );
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
        (_, "--guest-build") if options.guest_build.is_none() => {
            options.guest_build = Some(text(value)?);
        }
        _ => return Err(format!("unexpected option {option:?} of {name}")),
    }
    Ok(())
}

/// Launches a VM of the guest agent `guest_agent` in the sandbox store `store`, on `hypervisor`
/// or else the one that `nvx::Hypervisor::from_env_or_default` selects, with the guest agent's
/// build `guest_build`, and with the layered root `layered`, if any.
fn launch(
    store: &Path,
    guest_agent: &Path,
    hypervisor: Option<Hypervisor>,
    guest_build: &str,
    layered: Option<&Layered>,
) -> Result<ExitCode, String> {
    let hypervisor = match hypervisor {
        Some(hypervisor) => hypervisor,
        None => Hypervisor::from_env_or_default().map_err(|error| describe(&error))?,
    };
    let runtime = runtime(guest_build)?;
    runtime
        .artifacts
        .probe()
        .map_err(|error| describe(&error))?;
    let layered = layered
        .map(|layered| -> nvx::Result<LayeredLaunch> {
            let mut launch = LayeredLaunch::new(
                &layered.distro,
                layered.image,
                layered.scratch.as_deref(),
                layered.runtime.as_deref(),
            )?;
            launch.config.overlay_mode = layered.overlay_mode;
            launch.config.scratch_journal = layered.scratch_journal;
            launch.config.first_partition_bound = layered.first_partition_bound;
            Ok(launch)
        })
        .transpose()
        .map_err(|error| describe(&error))?;
    let store = SandboxStore::create(store).map_err(|error| describe(&error))?;
    let mut locked = store
        .lock(Some(Instant::now() + LOCK_TIMEOUT))
        .map_err(|error| describe(&error))?;
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
    let deadline = Instant::now() + START_TIMEOUT;
    let starting = supervisor::launch(&mut locked, &runtime.identity, &capability, |endpoint| {
        // The launch has checked that no VM of the sandbox runs, which could still read the
        // initramfs of an earlier launch.
        initramfs::write(&runtime.artifacts.initrd, guest_agent, &initramfs).map_err(|error| {
            nvx::Error::io("cannot write the initramfs of the guest agent").with_source(error)
        })?;
        let outcome = store.outcome_path();
        let (arguments, kernel_command_line) = match &layered {
            Some(layered) => (
                layered.openvmm_arguments(
                    hypervisor,
                    &spec.kernel,
                    &spec.initrd,
                    endpoint,
                    &outcome,
                )?,
                layered.kernel_command_line()?,
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
        Ok(VmmCommand::new(&spec.openvmm, arguments, store.dir()))
    })
    .map_err(|error| describe(&error))?;
    let process = starting.process().reference().clone();
    println!("pid={}", process.pid);
    // A launch that fails from here on ends its VM, once `starting` drops.
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
        let reply = client
            .configure_root(&layered.config, deadline)
            .map_err(|error| {
                failed(format!(
                    "the guest agent did not answer the root configuration: {}",
                    describe(&error)
                ))
            })?;
        match reply {
            RootReply::Ready(_) => println!("root=ready"),
            RootReply::Failed(failure) => {
                print_failure(&failure);
                drop(client);
                return launch_failed(starting, &store, &failure).map_err(failed);
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
    starting.commit().map_err(|error| describe(&error))?;
    Ok(ExitCode::SUCCESS)
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

/// Prints the root's `failure`.
fn print_failure(failure: &RootFailure) {
    println!("root=failed");
    println!("stage={}", failure.stage());
    println!("problem={}", escape(failure.problem()));
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
