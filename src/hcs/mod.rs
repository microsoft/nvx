// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! Host Compute Service backend for Windows.

mod api;
mod compute;
mod console;
#[path = "hcn_endpoint.rs"]
mod network;
mod schema;
mod snapshot;

pub use network::NetConfig;

use ::std::fs::File;
use ::std::path::PathBuf;
use ::std::sync::{Arc, Mutex};
use ::std::time::Duration;

use ::anyhow::{Context, Result, bail};
use ::log::{debug, info, warn};

use crate::console::{Console, TimingMarker};
use crate::windows_terminal::ConsoleGuard;

/// Minimum schema required for `VirtualMachine.Chipset.LinuxKernelDirect`.
const REQUIRED_SCHEMA_MAJOR: u32 = 2;
const REQUIRED_SCHEMA_MINOR: u32 = 2;

/// Configuration for an HCS-managed micro-VM.
pub struct Config {
    pub kernel: Option<PathBuf>,
    pub initrd: Option<PathBuf>,
    pub cmdline: String,
    pub mem_bytes: u64,
    pub quiet: bool,
    pub exit_on_boot: bool,
    pub boot_marker: String,
    pub timing_markers: Vec<TimingMarker>,
    pub defer_stdin_until_boot: bool,
    pub snapshot: Option<PathBuf>,
    pub restore: Option<PathBuf>,
    pub net: Option<NetConfig>,
    pub hcn_endpoint_config: Option<PathBuf>,
}

struct VmPlan {
    vm_id: String,
    kernel: PathBuf,
    initrd: PathBuf,
    cmdline: String,
    memory_mib: u64,
    restore_state: Option<PathBuf>,
    network: Option<network::NetworkConfig>,
}

enum StopReason {
    GuestExit(String),
    Interrupted,
    BootMarker,
    SnapshotRequested,
}

/// Performs the non-destructive Phase 0 HCS capability check.
pub fn preflight() -> Result<()> {
    let document: String = api::service_properties()?;
    let versions: Vec<schema::Version> = schema::supported_versions(&document)?;
    let supported: bool = versions.iter().any(|version| {
        version.major == REQUIRED_SCHEMA_MAJOR && version.minor >= REQUIRED_SCHEMA_MINOR
    });
    if !supported {
        let advertised: String = versions
            .iter()
            .map(|version| format!("{}.{}", version.major, version.minor))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "HCS does not support required schema {}.{} (advertised: {})",
            REQUIRED_SCHEMA_MAJOR,
            REQUIRED_SCHEMA_MINOR,
            advertised
        );
    }

    info!(
        "HCS preflight OK: schema {}.{} is supported (service document: {})",
        REQUIRED_SCHEMA_MAJOR, REQUIRED_SCHEMA_MINOR, document
    );
    Ok(())
}

/// Creates, starts, supervises, and tears down an HCS kernel-direct VM.
pub fn run(cfg: Config) -> Result<()> {
    if cfg.snapshot.is_some() && cfg.restore.is_some() {
        bail!("HCS snapshot capture and restore cannot be combined");
    }
    let host: snapshot::HostVersion = api::host_version()?;
    let restored: Option<snapshot::LoadedSnapshot> = match cfg.restore.as_deref() {
        Some(directory) => Some(snapshot::load(directory, host)?),
        None => None,
    };
    preflight()?;

    let (plan, mut capture): (VmPlan, Option<snapshot::Capture>) = match restored {
        Some(restored) => {
            let network = match cfg.hcn_endpoint_config.as_deref() {
                Some(path) => {
                    let external = network::NetworkConfig::from_external(path, None)?;
                    if restored.manifest.network.as_ref() != Some(&external) {
                        bail!(
                            "external HCN endpoint descriptor does not match the restored snapshot identity"
                        );
                    }
                    Some(external)
                }
                None if restored.manifest.network.is_some() => {
                    bail!("networked HCS restore requires --hcn-endpoint-config")
                }
                None => None,
            };
            (
                VmPlan {
                    vm_id: restored.manifest.vm_id.clone(),
                    kernel: restored.kernel,
                    initrd: restored.initrd,
                    cmdline: restored.manifest.cmdline.clone(),
                    memory_mib: restored.manifest.memory_mib,
                    restore_state: Some(restored.state),
                    network,
                },
                None,
            )
        }
        None => {
            let kernel: PathBuf = canonical_artifact(cfg.kernel.as_ref(), "--kernel")?;
            let initrd: PathBuf = canonical_artifact(cfg.initrd.as_ref(), "--initrd")?;
            let vm_id: String = api::new_guid()?;
            let network = match cfg.hcn_endpoint_config.as_deref() {
                Some(path) => Some(network::NetworkConfig::from_external(path, cfg.net.as_ref())?),
                None if cfg.net.is_some() => {
                    bail!("HCS --net requires --hcn-endpoint-config")
                }
                None => None,
            };
            let capture: Option<snapshot::Capture> = match cfg.snapshot.as_deref() {
                Some(directory) => {
                    let capture: snapshot::Capture = snapshot::Capture::prepare(directory)?;
                    api::create_runtime_state_file(capture.state())?;
                    api::grant_vm_access(&vm_id, capture.state())?;
                    Some(capture)
                }
                None => None,
            };
            (
                VmPlan {
                    vm_id,
                    kernel,
                    initrd,
                    cmdline: cfg.cmdline.clone(),
                    memory_mib: cfg.mem_bytes >> 20,
                    restore_state: None,
                    network,
                },
                capture,
            )
        }
    };
    run_plan(&cfg, &plan, &mut capture, host)
}

fn run_plan(
    cfg: &Config,
    plan: &VmPlan,
    capture: &mut Option<snapshot::Capture>,
    host: snapshot::HostVersion,
) -> Result<()> {
    let vm_id: &str = &plan.vm_id;
    let console_pipe_name: String = format!(r"\\.\pipe\nvx-{vm_id}-com1");
    let control_enabled: bool = capture.is_some() || plan.restore_state.is_some();
    let control_pipe_name: Option<String> =
        control_enabled.then(|| format!(r"\\.\pipe\nvx-{vm_id}-com2"));

    let mut borrowed_endpoint: Option<network::BorrowedEndpoint> = plan
        .network
        .clone()
        .map(network::BorrowedEndpoint::open)
        .transpose()?;
    let mut effective_cmdline: String = plan.cmdline.clone();
    if let Some(endpoint) = &borrowed_endpoint {
        let mac: &str = endpoint
            .config()
            .mac_address
            .as_deref()
            .context("queried HCN endpoint has no MAC address")?;
        effective_cmdline.push(' ');
        effective_cmdline.push_str(&endpoint.config().cmdline_fragment(mac));
    }

    api::grant_vm_access(vm_id, &plan.kernel)?;
    api::grant_vm_access(vm_id, &plan.initrd)?;
    if let Some(state) = &plan.restore_state {
        api::grant_vm_access(vm_id, state)?;
    }

    let document: String = schema::compute_system_document(
        &plan.kernel.to_string_lossy(),
        &plan.initrd.to_string_lossy(),
        &effective_cmdline,
        plan.memory_mib,
        &console_pipe_name,
        schema::ComputeSystemOptions {
            control_pipe: control_pipe_name.as_deref(),
            restore_state: plan
                .restore_state
                .as_ref()
                .map(|path| path.to_string_lossy())
                .as_deref(),
            network_adapter: borrowed_endpoint.as_ref().map(|endpoint| {
                let attachment = endpoint.attachment();
                (
                    attachment.adapter_id,
                    attachment.endpoint_id,
                    attachment.mac_address,
                )
            }),
        },
    )?;
    debug!("HCS create document for {vm_id}: {document}");

    let mut system: compute::ComputeSystem = compute::ComputeSystem::create(vm_id, &document)?;
    let console_before_start = console::connect(&console_pipe_name, Duration::from_millis(250))?;
    let control_before_start: Option<File> = match control_pipe_name.as_deref() {
        Some(path) => console::connect(path, Duration::from_millis(250))?,
        None => None,
    };

    let shared_console: Arc<Mutex<Console>> = Arc::new(Mutex::new(Console::with_timing_markers(
        cfg.quiet,
        &cfg.boot_marker,
        &cfg.timing_markers,
    )));
    shared_console
        .lock()
        .expect("console poisoned")
        .mark_start();
    let terminal_guard: ConsoleGuard = ConsoleGuard::new();
    let control_guard: console::ControlHandler = console::ControlHandler::install()?;
    system.start()?;
    if plan.restore_state.is_some()
        && let Some(endpoint) = borrowed_endpoint.as_ref()
    {
        let attachment = endpoint.attachment();
        let add = schema::network_adapter_add(
            attachment.adapter_id,
            attachment.endpoint_id,
            attachment.mac_address,
        )?;
        system.modify_network_adapter(
            "HcsModifyComputeSystem(add restored network adapter)",
            &add,
        )?;
    }

    let console_pipe = match console_before_start {
        Some(pipe) => {
            debug!("connected to HCS COM1 pipe before compute-system start");
            pipe
        }
        None => {
            debug!("HCS COM1 pipe was unavailable before start; retrying after start");
            console::connect(&console_pipe_name, Duration::from_secs(5))?.with_context(|| {
                format!("timed out connecting to HCS COM1 pipe {console_pipe_name}")
            })?
        }
    };
    let mut control_pipe: Option<File> = match (control_pipe_name.as_deref(), control_before_start)
    {
        (Some(_), Some(pipe)) => {
            debug!("connected to HCS COM2 pipe before compute-system start");
            Some(pipe)
        }
        (Some(path), None) => {
            debug!("HCS COM2 pipe was unavailable before start; retrying after start");
            Some(
                console::connect(path, Duration::from_secs(5))?
                    .with_context(|| format!("timed out connecting to HCS COM2 pipe {path}"))?,
            )
        }
        (None, _) => None,
    };
    let console_session: console::ConsoleSession = console::ConsoleSession::start(
        console_pipe,
        Arc::clone(&shared_console),
        cfg.defer_stdin_until_boot
            && !terminal_guard.stdin_is_console()
            && plan.restore_state.is_none(),
    )?;

    let snapshot_control: Option<console::SnapshotControlSession> = if capture.is_some() {
        Some(console::SnapshotControlSession::start(
            control_pipe
                .take()
                .context("HCS snapshot capture is missing its COM2 control pipe")?,
        ))
    } else {
        if plan.restore_state.is_some() {
            console::acknowledge_restore(
                control_pipe
                    .as_mut()
                    .context("HCS restore is missing its COM2 control pipe")?,
            )?;
        }
        None
    };

    info!(
        "started HCS guest {vm_id} (mem={} MiB, cmdline={:?})",
        plan.memory_mib, effective_cmdline
    );
    let lifecycle_result: Result<StopReason> = supervise(
        cfg,
        &mut system,
        &shared_console,
        &control_guard,
        snapshot_control.as_ref(),
    );
    let lifecycle_result: Result<()> = lifecycle_result.and_then(|reason| match reason {
        StopReason::SnapshotRequested => {
            let capture = capture
                .as_mut()
                .context("guest requested an HCS snapshot without --snapshot")?;
            save_snapshot(
                &mut system,
                capture,
                plan,
                host,
                borrowed_endpoint
                    .as_ref()
                    .map(network::BorrowedEndpoint::config),
            )
        }
        StopReason::GuestExit(document) => {
            if capture.is_some() {
                bail!("HCS guest exited before requesting the configured snapshot: {document}");
            }
            schema::validate_exit_document(&document)?;
            info!("HCS guest exited: {document}");
            Ok(())
        }
        StopReason::Interrupted => {
            info!("Ctrl-C received; terminating HCS guest");
            Ok(())
        }
        StopReason::BootMarker => {
            if capture.is_some() {
                bail!("--exit-on-boot stopped the HCS guest before it requested a snapshot");
            }
            info!("boot marker seen; stopping HCS guest (--exit-on-boot)");
            Ok(())
        }
    });
    let detach_result: Result<()> = if system.is_running() {
        borrowed_endpoint
            .as_ref()
            .map(|endpoint| {
                let attachment = endpoint.attachment();
                let document = schema::network_adapter_remove(
                    attachment.adapter_id,
                    attachment.endpoint_id,
                    attachment.mac_address,
                )?;
                system.modify_network_adapter(
                    "HcsModifyComputeSystem(remove network adapter)",
                    &document,
                )
            })
            .transpose()
            .map(|_| ())
    } else {
        Ok(())
    };
    let cleanup_result: Result<()> = if system.is_running() {
        system.terminate()
    } else {
        Ok(())
    };
    drop(system);
    let endpoint_result: Result<()> = match borrowed_endpoint.as_mut() {
        Some(endpoint) => endpoint.cleanup(),
        None => Ok(()),
    };
    drop(control_pipe);
    let console_result: Result<()> = console_session.finish();
    let control_result: Result<()> = match snapshot_control {
        Some(session) => session.finish(),
        None => Ok(()),
    };

    if let Err(primary) = lifecycle_result {
        if let Err(error) = detach_result {
            warn!("HCS network-adapter detach after primary failure also failed: {error:#}");
        }
        if let Err(error) = cleanup_result {
            warn!("HCS cleanup after primary failure also failed: {error:#}");
        }
        if let Err(error) = console_result {
            warn!("HCS console cleanup after primary failure also failed: {error:#}");
        }
        if let Err(error) = control_result {
            warn!("HCS control cleanup after primary failure also failed: {error:#}");
        }
        if let Err(error) = endpoint_result {
            warn!("HCN endpoint close after primary failure also failed: {error:#}");
        }
        return Err(primary);
    }
    detach_result?;
    cleanup_result?;
    console_result?;
    control_result?;
    endpoint_result?;
    report_timings(cfg, &shared_console);
    Ok(())
}

fn save_snapshot(
    system: &mut compute::ComputeSystem,
    capture: &mut snapshot::Capture,
    plan: &VmPlan,
    host: snapshot::HostVersion,
    network: Option<&network::NetworkConfig>,
) -> Result<()> {
    let pause_options: String = schema::pause_options()?;
    info!("pausing HCS guest {} for snapshot", plan.vm_id);
    system.pause(&pause_options)?;
    let save_options: String = schema::save_options(&capture.state().to_string_lossy())?;
    info!("saving HCS guest {} to {:?}", plan.vm_id, capture.state());
    system.save(&save_options)?;
    info!("HCS save operation completed for {}", plan.vm_id);

    let manifest: snapshot::Manifest = snapshot::Manifest::capture(
        plan.vm_id.clone(),
        host,
        plan.memory_mib,
        plan.cmdline.clone(),
        &plan.kernel,
        &plan.initrd,
        network.cloned(),
    )?;
    capture.commit(&manifest)?;
    info!("saved HCS snapshot to {:?}", capture.directory());
    Ok(())
}

fn supervise(
    cfg: &Config,
    system: &mut compute::ComputeSystem,
    console: &Arc<Mutex<Console>>,
    control: &console::ControlHandler,
    snapshot_control: Option<&console::SnapshotControlSession>,
) -> Result<StopReason> {
    loop {
        if let Some(exit_document) = system.wait_for_exit(20)? {
            return Ok(StopReason::GuestExit(exit_document));
        }
        if let Some(snapshot_control) = snapshot_control
            && snapshot_control.requested()?
        {
            return Ok(StopReason::SnapshotRequested);
        }
        if control.interrupted() {
            return Ok(StopReason::Interrupted);
        }
        if cfg.exit_on_boot && console.lock().expect("console poisoned").booted() {
            return Ok(StopReason::BootMarker);
        }
    }
}

fn canonical_artifact(path: Option<&PathBuf>, option: &str) -> Result<PathBuf> {
    let path = path.with_context(|| format!("{option} is required by the HCS backend"))?;
    path.canonicalize()
        .with_context(|| format!("resolving {option} artifact {path:?}"))
}

fn report_timings(cfg: &Config, console: &Arc<Mutex<Console>>) {
    let console = console.lock().expect("console poisoned");
    for (label, elapsed) in console.timings() {
        eprintln!(
            "timing-marker: {label} {:.1} ms",
            elapsed.as_secs_f64() * 1000.0
        );
    }
    if cfg.exit_on_boot
        && let Some(elapsed) = console.cold_start()
    {
        let label: &str = if cfg.restore.is_some() {
            "restore"
        } else {
            "cold-start"
        };
        eprintln!(
            "{label}: {:.1} ms to marker ({} console bytes emitted)",
            elapsed.as_secs_f64() * 1000.0,
            console.bytes_out()
        );
    }
}
