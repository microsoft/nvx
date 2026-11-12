//! A caller-owned launch: `vm::create` prepares a cold boot or a restore, and the caller's own
//! launcher starts the VMM of the `vm::VmmCommand` that it hands over.
//!
//! Run the compiled example with a backend, OpenVMM, kernel, initramfs, new VM directory, and
//! the three artifact SHA-256 digests from their installation metadata, in that order.
//! Append SNAPSHOT-DIR IMAGE.erofs for a restore-ready demonstration: it verifies the ready
//! event, then intentionally stops and verifies the VM's exit without driving guest activation.
//! The snapshot must have captured a disconnected boot console, as `nvx-rs snapshot capture`
//! does.
//!
//! Its launcher, `spawn`, is a minimal one on `std::process`, enough for these two VMs: it writes
//! the capability into a pipe that it closes to writers before the spawn, gives the VMM that pipe
//! as its standard input and the log as its standard output and error, holds the VMM through a
//! `process::ProcessRef` whose start time it reads itself, as `ProcessRef` defines it, and ends
//! and reaps the VMM if anything fails once it runs. It leaves out the rest of the launch contract
//! that
//! `vm::VmmCommand` states, which a production launcher should add, as `vm::spawn` does:
//!
//! - It passes the VMM no descriptor or handle besides its standard streams, and refuses a
//!   command that allows any. These two VMs need none.
//! - On Linux, it starts the VMM in a process group of its own, but not in a new session, and
//!   leaves the VMM the caller's umask rather than clearing the owner's bits from it.
//! - On Windows, it starts the VMM without a console, but lets it inherit every inheritable handle
//!   of the caller rather than an explicit list, and starts it in no job named after its log.
//!
//! The example spawns nothing else while the VMM starts, so it takes no lock around its spawn.

use std::error::Error;
#[cfg(target_os = "linux")]
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use nvx::Hypervisor;
use nvx::activation::{MachineIdMode, View};
use nvx::host::config::{
    Boot, ConsoleAttachment, LayeredRoot, NetworkConfig, RootConfig, UpperConfig, VmConfig,
};
use nvx::host::network::{Access, NetworkPolicy};
use nvx::host::process::{BootId, Opened, Process, ProcessRef};
use nvx::host::runtime::{ArtifactIdentity, LaunchOptions, Runtime, RuntimeIdentity};
use nvx::host::transport::{self, Transport};
use nvx::host::{report, snapshot, vm};
use nvx::protocol::root::RootImage;

/// One byte past OpenVMM's 25-byte restore-ready event, so that a longer event fails to parse.
const READY_LIMIT: usize = 26;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 9 && args.len() != 11 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: external-launch BACKEND OPENVMM KERNEL INITRAMFS VM-DIR \
             OPENVMM-SHA256 KERNEL-SHA256 INITRAMFS-SHA256 [SNAPSHOT-DIR IMAGE.erofs]",
        )
        .into());
    }
    let hypervisor = match args[1].to_str() {
        Some("kvm") => Hypervisor::Kvm,
        Some("mshv") => Hypervisor::Mshv,
        Some("whp") => Hypervisor::Whp,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid backend").into()),
    };
    nvx::host::runtime::probe_hypervisor(hypervisor)?;
    let identity = |index: usize| {
        ArtifactIdentity::sha256(args[index].to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "artifact digest is not Unicode",
            )
        })?)
        .map_err(Box::<dyn Error>::from)
    };
    let runtime = Runtime {
        openvmm: std::path::absolute(Path::new(&args[2]))?,
        kernel: std::path::absolute(Path::new(&args[3]))?,
        initramfs: std::path::absolute(Path::new(&args[4]))?,
        identity: RuntimeIdentity::new(identity(6)?, identity(7)?, identity(8)?),
        hypervisor,
        cpu_profile: Some("auto".to_owned()),
        windows_breakaway: false,
        options: LaunchOptions::default(),
    };
    let snapshot_dir = (args.len() == 11).then(|| Path::new(&args[9]));
    // Every caller of this snapshot must take this same lock before it opens the snapshot, and
    // hold it through the restore-ready event or the VM's exit, and the view's removal.
    #[cfg(target_os = "linux")]
    let snapshot_lock = snapshot_dir
        .map(|dir| File::open(dir.join(snapshot::KEY_FILE)))
        .transpose()?;
    #[cfg(target_os = "linux")]
    if let Some(lock) = &snapshot_lock {
        lock.lock()?;
    }
    let snapshot = snapshot_dir.map(snapshot::Snapshot::open).transpose()?;
    let mut config = VmConfig {
        processors: 1,
        memory_mib: 256,
        root: RootConfig::Blockless,
        view: View {
            hostname: b"external".to_vec(),
            machine_id: MachineIdMode::Image,
        },
        network: None,
        filesystem: None,
        console: ConsoleAttachment::Listener,
        boot: Some(Boot {
            command_line: "quiet loglevel=0".to_owned(),
            memory_capacity_mib: None,
            capture: None,
        }),
    };
    if let Some(snapshot) = &snapshot {
        let key = snapshot.key();
        config.processors = match key.boot_processors() {
            0 => key.processors(),
            boot => boot,
        };
        config.memory_mib = key.memory() / (1 << 20);
        config.root = RootConfig::Layered(LayeredRoot {
            image: std::path::absolute(Path::new(&args[10]))?,
            format: RootImage::erofs(None),
            gpt_first_partition_bound: None,
            upper: UpperConfig::Ram {
                max_memory_percent: 50,
            },
            overlay: nvx::activation::OverlayMode::TrustedLowers,
        });
        config.network = snapshot.nic().map(|_| NetworkConfig {
            guest_network: None,
            policy: NetworkPolicy::new(Access::Deny),
        });
        config.console = ConsoleAttachment::Disconnected;
        config.boot = None;
    }
    vm::create_private_dir(Path::new(&args[5]))?;
    let dir = vm::VmDir::open(Path::new(&args[5]))?;
    let command = vm::create(&runtime, &config, snapshot.as_ref(), &dir)?.into_command();
    let mut child = match spawn(&command) {
        Ok(child) => child,
        Err(error) => {
            if snapshot.is_some()
                && let Err(cleanup) = remove_view(dir.path())
            {
                return Err(io::Error::other(format!(
                    "{error}; cannot remove restore view after spawn failure: {cleanup}"
                ))
                .into());
            }
            return Err(error.into());
        }
    };
    let vm::VmmCommand {
        outputs, mut ready, ..
    } = command;
    let result = (|| -> Result<(), Box<dyn Error>> {
        let process = held(&child)?;
        let deadline = Instant::now() + Duration::from_secs(60);
        if let Some(listener) = &mut ready {
            let mut event = Vec::new();
            let mut open = true;
            while open && event.len() < READY_LIMIT {
                let timeout = deadline.saturating_duration_since(Instant::now());
                if timeout.is_zero() {
                    return Err(
                        io::Error::new(io::ErrorKind::TimedOut, "restore-ready timed out").into(),
                    );
                }
                open = listener.receive(&process, &mut event, READY_LIMIT, timeout)?;
            }
            nvx::protocol::restore_ready::parse_ready_event(&event)?;
            process.kill()?;
            if process.wait(deadline)?.is_none() || child.try_wait()?.is_none() {
                return Err(io::Error::other("restored VM exit is unconfirmed").into());
            }
            println!(
                "external restore-ready peer verified; demonstration VM intentionally stopped"
            );
            return Ok(());
        }
        let console = outputs.console.as_deref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "no prepared boot console")
        })?;
        let mut console = transport::connect_verified(console, process.reference(), deadline)?;
        read_until(console.as_mut(), b"NVX-GUEST-BOOT-OK:", deadline)?;
        console.write_all(
            b"printf 'NVX-EXTERNAL-LAUNCH-OK\\n'; /sbin/nvx-exit 0\n",
            Some(deadline.saturating_duration_since(Instant::now())),
        )?;
        read_until(console.as_mut(), b"\nNVX-EXTERNAL-LAUNCH-OK", deadline)?;
        if process.wait(deadline)?.is_none() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "VM did not exit").into());
        }
        match child.try_wait()? {
            Some(status) if status.success() => {}
            Some(status) => {
                return Err(io::Error::other(format!("VMM exited with {status}")).into());
            }
            None => return Err(io::Error::other("VMM exit status is unavailable").into()),
        }
        let outcome = report::read(&outputs.outcome)?
            .ok_or_else(|| io::Error::other("missing or invalid VM outcome"))?;
        if outcome.category.as_deref() != Some("success")
            || outcome.status_code != Some(0)
            || outcome.teardown_complete != Some(true)
        {
            return Err(
                io::Error::other(format!("VM did not shut down cleanly: {outcome:?}")).into(),
            );
        }
        Ok(())
    })();
    if let Err(error) = result {
        // `std::process` signals the child only until it reaps it, so the kill reaches the VMM
        // alone.
        let _ = child.kill();
        if let Err(cleanup) = child.wait() {
            return Err(io::Error::other(format!(
                "{error}; could not confirm exit of the launched VM: {cleanup}"
            ))
            .into());
        }
        if snapshot.is_some()
            && let Err(cleanup) = remove_view(dir.path())
        {
            return Err(io::Error::other(format!(
                "{error}; cannot remove restore view: {cleanup}"
            ))
            .into());
        }
        return Err(error);
    }
    if snapshot.is_some() {
        remove_view(dir.path())?;
    }
    println!(
        "external launch completed; outcome={}",
        outputs.outcome.display()
    );
    Ok(())
}

/// Starts the VMM of `command` with `std::process`, as the minimal launcher that the module
/// describes: with the capability on its standard input, through a pipe that is closed to writers
/// before the spawn, and the log as its standard output and error, in a process group of its own,
/// and on Windows without a console, outside the caller's job if the command asks for it.
///
/// Fails with [`io::ErrorKind::Unsupported`] for a command that allows the VMM a descriptor or
/// handle, which this launcher does not pass.
fn spawn(command: &vm::VmmCommand) -> io::Result<Child> {
    if !command.inherit.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this launcher passes the VMM no descriptor or handle",
        ));
    }
    let (stdin, mut writer) = io::pipe()?;
    writer.write_all(command.capability.as_bytes())?;
    drop(writer);
    let mut child = Command::new(&command.program);
    child
        .args(&command.arguments)
        .current_dir(&command.working_dir)
        .stdin(stdin)
        .stdout(command.log.try_clone()?)
        .stderr(command.log.try_clone()?);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut child, 0);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
        };

        let breakaway = if command.breakaway_from_job {
            CREATE_BREAKAWAY_FROM_JOB
        } else {
            0
        };
        child.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | breakaway);
    }
    child.spawn()
}

/// Holds the VMM that `child` runs through a handle of its own: until the child is reaped, its ID
/// and start time name it alone.
fn held(child: &Child) -> io::Result<Process> {
    let reference = ProcessRef {
        pid: child.id(),
        start_time: start_time(child)?,
        boot: BootId::current()?,
    };
    match Process::open(&reference)? {
        Opened::Running(process) => Ok(process),
        Opened::Exited(_) => Err(io::Error::other("the VMM exited before it could be held")),
    }
}

/// Returns the start time of the process that `child` runs, as `ProcessRef` defines it: on Linux,
/// the `starttime` field of `/proc/<pid>/stat`, in clock ticks since boot, which the process keeps
/// until its parent reaps it.
#[cfg(target_os = "linux")]
fn start_time(child: &Child) -> io::Result<u64> {
    let path = format!("/proc/{}/stat", child.id());
    let stat = std::fs::read(&path)?;
    // The command name may hold spaces and parentheses, so the fields that follow it start after
    // the last closing parenthesis, with the state, field 3; the start time is field 22.
    stat.iter()
        .rposition(|&byte| byte == b')')
        .and_then(|end| std::str::from_utf8(&stat[end + 1..]).ok())
        .and_then(|fields| fields.split_whitespace().nth(19))
        .and_then(|field| field.parse().ok())
        .ok_or_else(|| io::Error::other(format!("{path} has an unexpected format")))
}

/// Returns the start time of the process that `child` runs, as `ProcessRef` defines it: on
/// Windows, the creation time of the process as a 64-bit `FILETIME`.
#[cfg(windows)]
fn start_time(child: &Child) -> io::Result<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;

    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the handle of `child` has every access to its process, which it keeps until the
    // child drops, and each output points to a `FILETIME` that the call may write.
    let succeeded = unsafe {
        GetProcessTimes(
            child.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } != 0;
    if !succeeded {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// Fails on the hosts other than Linux and Windows, where NVX runs no VMs.
#[cfg(not(any(target_os = "linux", windows)))]
fn start_time(_child: &Child) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "NVX runs VMs on Linux and Windows hosts only",
    ))
}

/// Removes only the successful fixed restore view, while the caller still holds its lock.
fn remove_view(directory: &Path) -> io::Result<()> {
    let view = directory.join(snapshot::OPENVMM_DIR);
    for name in [snapshot::MANIFEST_NAME, "memory.bin", "state.bin"] {
        std::fs::remove_file(view.join(name))?;
    }
    std::fs::remove_dir(view)
}

/// Prints console output through a marker, retaining only enough bytes to match across reads.
fn read_until(console: &mut dyn Transport, marker: &[u8], deadline: Instant) -> io::Result<()> {
    let mut pending = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "console marker not received",
            ));
        }
        let count = console.read(&mut buffer, Some(remaining))?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "boot console closed",
            ));
        }
        io::stdout().write_all(&buffer[..count])?;
        pending.extend_from_slice(&buffer[..count]);
        if pending.windows(marker.len()).any(|bytes| bytes == marker) {
            return Ok(());
        }
        if pending.len() > marker.len() {
            pending.drain(..pending.len() - marker.len());
        }
    }
}
