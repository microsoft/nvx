//! A blockless capture, kept in the ordinary launch journal until it ends.

use std::fs;
use std::path::Path;
use std::time::Instant;

use super::{Name, Store};
use crate::ErrorCode;
use crate::activation::{MachineIdMode, View};
use crate::host::cli::args::{Parser, Receiver, Spec, Syntax, choice, integer};
use crate::host::cli::deadline::{Deadline, RETRY_INTERVAL};
use crate::host::cli::sandbox::{
    Artifacts, HYPERVISORS, MAX_MEMORY_MIB, error_with_cause, hypervisor,
};
use crate::host::cli::store::{LockedState, StateDir};
use crate::host::cli::{Exit, Result, check_interrupt, io_error, output};
use crate::host::config::{
    BlockGeometry, Boot, Capacity, CaptureConfig, ConsoleAttachment, DiskCapture, Invariants,
    RootConfig, VmConfig,
};
use crate::host::process::{Exited, KILL_TIMEOUT, Process};
use crate::host::runtime::{LaunchOptions, Runtime};
use crate::host::session::{ControlSession, SessionError};
use crate::host::snapshot::{DESCRIPTION_FILE, KEY_FILE, OPENVMM_DIR, finish_capture};
use crate::host::transport;
use crate::host::vm::{self, Prepared, VmDir, VmOutputs};

/// The snapshot subcommand parser.
const SYNTAX: Syntax = Syntax {
    command: "snapshot",
    arguments: "[-h] {capture} ...",
    help: "positional arguments:\n  {capture}  capture a named snapshot\n\noptions:\n  -h, --help  show this help message and exit",
};

/// The capture command parser.
const CAPTURE_SYNTAX: Syntax = Syntax {
    command: "snapshot capture",
    arguments: "[-h] --store-dir PATH [OPTIONS] NAME",
    help: "\
Capture a blockless VM before any workload or layered root exists.
The store's next capture command ends an interrupted attempt; it never resumes it.

positional arguments:
  NAME                  a new snapshot name

options:
  -h, --help            show this help message and exit
  --store-dir PATH      private named snapshot store; its parent must exist
  --hypervisor {auto,whp,kvm,mshv}
  --cpu-profile ID      OpenVMM CPU profile (default: auto)
  --processors COUNT   boot processors: 1, 2, 4 or 8 (default: 1)
  --processor-capacity COUNT
                        processor capacity (default: boot processors)
  --memory-mib MIB      boot RAM (default: 256)
  --memory-capacity-mib MIB
                        RAM capacity, a multiple of 128 (default: boot RAM)
  --image-capacity-mib MIB
                        longest restore image (default: 1024)
  --scratch-mib MIB     dormant scratch length (default: 64)
  --timeout SECONDS    positive whole seconds for launch, capture and exit,
                        and each lock wait (default: 60)",
};

/// Capture's small, fixed option set.
const OPTIONS: [Spec; 10] = [
    Spec::one("--store-dir"),
    Spec::one("--hypervisor"),
    Spec::one("--cpu-profile"),
    Spec::one("--processors"),
    Spec::one("--processor-capacity"),
    Spec::one("--memory-mib"),
    Spec::one("--memory-capacity-mib"),
    Spec::one("--image-capacity-mib"),
    Spec::one("--scratch-mib"),
    Spec::one("--timeout"),
];

/// Inputs of one capture, with no workload, mounts or caller command-line tokens.
pub(in crate::host::cli) struct Options {
    /// The private store.
    store_dir: String,
    /// The required name, filled by the parser.
    name: Option<Name>,
    /// The selected backend.
    hypervisor: &'static str,
    /// The requested CPU profile.
    cpu_profile: Option<String>,
    /// Boot processors and optional capacity.
    processors: u32,
    /// Maximum processors.
    processor_capacity: Option<u32>,
    /// Boot memory.
    memory_mib: u64,
    /// Maximum memory.
    memory_capacity_mib: Option<u64>,
    /// Maximum restore image size.
    image_mib: u64,
    /// Dormant scratch size.
    scratch_mib: u64,
    /// Bounded operation time.
    timeout: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            store_dir: String::new(),
            name: None,
            hypervisor: "auto",
            cpu_profile: None,
            processors: 1,
            processor_capacity: None,
            memory_mib: 256,
            memory_capacity_mib: None,
            image_mib: 1024,
            scratch_mib: 64,
            timeout: 60,
        }
    }
}

impl Receiver for Options {
    fn take(&mut self, name: &'static str, value: Option<&str>) -> std::result::Result<(), String> {
        let value = value.unwrap_or_default();
        match name {
            "NAME" => {
                self.name =
                    Some(Name::try_from(value.to_owned()).map_err(|error| error.to_string())?)
            }
            "--store-dir" => self.store_dir = value.to_owned(),
            "--hypervisor" => self.hypervisor = choice(value, &HYPERVISORS)?,
            "--cpu-profile" => self.cpu_profile = Some(value.to_owned()),
            "--processors" | "--processor-capacity" => {
                let count = choice(value, &["1", "2", "4", "8"])?
                    .parse()
                    .expect("processor choice");
                if name == "--processors" {
                    self.processors = count;
                } else {
                    self.processor_capacity = Some(count);
                }
            }
            "--memory-mib" => self.memory_mib = integer(value, 1..=MAX_MEMORY_MIB)?,
            "--memory-capacity-mib" => {
                self.memory_capacity_mib = Some(integer(value, 1..=MAX_MEMORY_MIB)?)
            }
            "--image-capacity-mib" => self.image_mib = integer(value, 1..=MAX_MEMORY_MIB)?,
            "--scratch-mib" => self.scratch_mib = integer(value, 1..=MAX_MEMORY_MIB)?,
            "--timeout" => self.timeout = integer(value, 1..=u32::MAX)?,
            _ => unreachable!("the capture parser has a fixed option set"),
        }
        Ok(())
    }
}

/// Parses only the capture operation; restore belongs to later slices.
pub(in crate::host::cli) fn parse(arguments: &[String]) -> Result<(Options, Vec<String>)> {
    let (index, mut extras) = Parser::new(&[], &[Spec::one("snapshot_operation")])
        .split_command(arguments, &mut |_, _| Ok(()))
        .map_err(|stop| Exit::from_stop(stop, &SYNTAX))?;
    choice(&arguments[index], &["capture"])
        .map_err(|message| Exit::usage(&SYNTAX, ErrorCode::ArgumentInvalid, message))?;
    let mut options = Options::default();
    extras.extend(
        Parser::new(&OPTIONS, &[Spec::one("NAME")])
            .parse(&arguments[index + 1..], &mut options)
            .map_err(|stop| Exit::from_stop(stop, &CAPTURE_SYNTAX))?,
    );
    if options.store_dir.is_empty() {
        return Err(Exit::usage(
            &CAPTURE_SYNTAX,
            ErrorCode::ArgumentMissing,
            "--store-dir is required".to_owned(),
        ));
    }
    Ok((options, extras))
}

/// Uses the identified artifacts, never an unlabelled OpenVMM override or a downloaded substitute.
pub(in crate::host::cli) fn run(root: Option<&str>, options: Options) -> Result {
    let artifacts = Artifacts { root };
    let _hold = artifacts.hold()?;
    let [openvmm, kernel, initramfs] = artifacts.require()?;
    let runtime = Runtime {
        openvmm: openvmm.into(),
        kernel: kernel.into(),
        initramfs: initramfs.into(),
        identity: artifacts.launch_identity()?,
        hypervisor: hypervisor(options.hypervisor).parse()?,
        cpu_profile: options.cpu_profile.clone(),
        windows_breakaway: false,
        options: LaunchOptions::default(),
    };
    capture(&options, &runtime, |prepared| {
        Ok(vm::spawn(prepared)?.process)
    })?;
    output::print_stdout("Snapshot captured").map_err(Exit::from_os_error)
}

/// Runs a single launch. The spawn callback consumes exactly the journaled preparation.
fn capture(
    options: &Options,
    runtime: &Runtime,
    spawn: impl FnOnce(Prepared) -> crate::Result<Process>,
) -> Result {
    let store = Store::create(Path::new(&options.store_dir))?;
    let deadline = Deadline::after(f64::from(options.timeout));
    // This persistent outer lock protects removal of the inner VM directory and its lock file.
    let _capture_lock = store.root.lock(deadline)?;
    remove_capture(&store, deadline)?;
    let name = options
        .name
        .as_ref()
        .expect("the parser requires a snapshot name");
    let named = store.lock(name, deadline)?;
    match fs::symlink_metadata(&named.dir) {
        Ok(_) => {
            return Err(Exit::error(
                ErrorCode::Io,
                "the named snapshot already exists",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(ErrorCode::Io, &error, &named.dir)),
    }
    let state = StateDir::create(&store.root.dir().join("capture"))?;
    let mut locked = state.lock(deadline)?;
    let dir = VmDir::open(state.dir())?;
    let mut geometry = disk_geometry(&dir)?;
    geometry.length = options.image_mib << 20;
    let config = VmConfig {
        processors: options.processors,
        memory_mib: options.memory_mib,
        root: RootConfig::Blockless,
        view: View {
            hostname: b"nvx-capture".to_vec(),
            machine_id: MachineIdMode::Image,
        },
        network: None,
        filesystem: None,
        console: ConsoleAttachment::Disconnected,
        boot: Some(Boot {
            memory_capacity_mib: None,
            command_line: "nvx_sandbox=1".to_owned(),
            capture: Some(CaptureConfig {
                disks: DiskCapture::Fresh {
                    image: geometry,
                    scratch: BlockGeometry {
                        length: options.scratch_mib << 20,
                        ..geometry
                    },
                },
                processors: Capacity {
                    boot: u64::from(options.processors),
                    capacity: u64::from(options.processor_capacity.unwrap_or(options.processors)),
                },
                memory: Capacity {
                    boot: options.memory_mib,
                    capacity: options.memory_capacity_mib.unwrap_or(options.memory_mib),
                },
                invariants: Invariants::new(),
            }),
        }),
    };
    let prepared = vm::create(runtime, &config, None, &dir)?;
    let context = prepared
        .capture_context()
        .cloned()
        .expect("boot.capture was checked");
    let capability = *prepared.capability().as_bytes();
    let outputs = prepared.outputs().clone();
    let mut launch = locked.begin_launch(&prepared, &runtime.identity)?;
    check_interrupt()?;
    let process = spawn(prepared).map_err(error_with_cause)?;
    launch.record_process(process.reference())?;
    drop(launch);
    let exited = match request_and_wait(&process, &outputs, &capability, deadline) {
        Ok(exited) => exited,
        Err(error)
            if !matches!(error, Exit::Interrupted)
                && (error.code() == Some(ErrorCode::TimedOut) || deadline.passed()) =>
        {
            let now = Instant::now();
            let stopped = vm::stop(&process, &outputs, |_| false, now, now + KILL_TIMEOUT)
                .map_err(|cleanup| {
                    Exit::error(
                        cleanup.code(),
                        format!(
                            "capture timed out; stopping its VM failed: {}",
                            error_with_cause(cleanup).message().unwrap_or_default()
                        ),
                    )
                })?;
            stopped.report.map_err(error_with_cause)?;
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    check_interrupt()?;
    finish_capture(&dir, &context, &exited).map_err(error_with_cause)?;
    check_interrupt()?;
    let staged = named.stage()?;
    let publication: Result = (|| {
        for entry in [KEY_FILE, DESCRIPTION_FILE, OPENVMM_DIR] {
            fs::rename(dir.path().join(entry), staged.dir().join(entry))
                .map_err(|error| io_error(ErrorCode::Io, &error, &dir.path().join(entry)))?;
        }
        named.publish(staged.clone())?;
        Ok(())
    })();
    if let Err(error) = publication {
        return Err(match fs::remove_dir_all(staged.dir()) {
            Ok(()) => error,
            Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => error,
            Err(cleanup) => Exit::error(
                ErrorCode::Io,
                format!(
                    "{}; cannot remove capture staging {}: {cleanup}",
                    error.message().unwrap_or_default(),
                    staged.dir().display()
                ),
            ),
        });
    }
    drop(locked);
    drop(process);
    remove_capture(&store, Deadline::after(KILL_TIMEOUT.as_secs_f64())).map_err(|error| {
        Exit::error(
            error.code().unwrap_or(ErrorCode::Io),
            format!(
                "snapshot was published; capture VM cleanup failed: {}",
                error.message().unwrap_or_default()
            ),
        )
    })
}

/// Reads the geometry through the existing block-device helper on this VM's filesystem.
fn disk_geometry(dir: &VmDir) -> Result<BlockGeometry> {
    let path = dir.path().join("geometry.raw");
    let file = crate::host::store::files::create_new(&path)?;
    file.set_len(512)
        .map_err(|error| io_error(ErrorCode::Io, &error, &path))?;
    let geometry = crate::host::sandbox::block_geometry(&file, &path, "capture probe")?;
    drop(file);
    fs::remove_file(&path).map_err(|error| io_error(ErrorCode::Io, &error, &path))?;
    Ok(geometry)
}

/// Normal launch recovery must prove exit before the reserved capture directory can be removed.
fn remove_capture(store: &Store, deadline: Deadline) -> Result {
    let path = store.root.dir().join("capture");
    let state = match StateDir::open(&path) {
        Ok(state) => state,
        Err(error) if error.code() == Some(ErrorCode::NotFound) => return Ok(()),
        Err(error) => return Err(error),
    };
    let locked: LockedState = state.lock(deadline)?;
    if locked.runtime()?.is_some() {
        return Err(Exit::error(
            ErrorCode::Io,
            "a capture directory cannot hold a committed VM",
        ));
    }
    drop(locked);
    fs::remove_dir_all(&path).map_err(|error| io_error(ErrorCode::Io, &error, &path))
}

/// Uses authentication and the actual held process, never the capture acknowledgement as exit.
fn request_and_wait(
    process: &Process,
    outputs: &VmOutputs,
    capability: &[u8; crate::protocol::CAPABILITY_LEN],
    deadline: Deadline,
) -> Result<Exited> {
    let until = deadline.instant();
    let transport = transport::connect_verified(&outputs.endpoint, process.reference(), until);
    check_interrupt()?;
    let attached = ControlSession::attach(transport.map_err(error_with_cause)?, capability, until);
    check_interrupt()?;
    let requested = attached.map_err(control_error)?.capture(false, until);
    check_interrupt()?;
    requested.map_err(control_error)?;
    loop {
        check_interrupt()?;
        let now = Instant::now();
        if deadline.passed() {
            return Err(Exit::error(
                ErrorCode::TimedOut,
                "capture VM did not exit before its deadline",
            ));
        }
        let exited = process
            .wait(until.min(now + RETRY_INTERVAL))
            .map_err(Exit::from_os_error)?;
        if let Some(exited) = exited {
            if deadline.passed() {
                return Err(Exit::error(
                    ErrorCode::TimedOut,
                    "capture VM exit was not confirmed before its deadline",
                ));
            }
            return Ok(exited);
        }
    }
}

/// Keeps control failures explicit, including the distinction between timeout and I/O.
fn control_error(error: SessionError) -> Exit {
    let code = match error {
        SessionError::TimedOut => ErrorCode::TimedOut,
        SessionError::Io(_) | SessionError::Refused(_) => ErrorCode::Io,
        SessionError::Closed | SessionError::ProcessExited => ErrorCode::Exited,
        _ => ErrorCode::Protocol,
    };
    Exit::error(code, format!("capture control failed: {error}"))
}

#[cfg(test)]
mod tests;
