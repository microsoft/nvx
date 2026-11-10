//! A caller-owned cold launch, using the strict low-level spawn primitive without a store.
//!
//! Run the compiled example with a backend, OpenVMM, kernel, initramfs, new VM directory, and
//! the three artifact SHA-256 digests from their installation metadata, in that order.

use std::error::Error;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use nvx::Hypervisor;
use nvx::activation::{MachineIdMode, View};
use nvx::host::config::{Boot, ConsoleAttachment, RootConfig, VmConfig};
use nvx::host::platform::Transport;
use nvx::host::runtime::{ArtifactIdentity, LaunchOptions, Runtime, RuntimeIdentity};
use nvx::host::supervisor::{self, Inherited};
use nvx::host::{artifacts, report, session, vm};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 9 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: external-launch BACKEND OPENVMM KERNEL INITRAMFS VM-DIR \
             OPENVMM-SHA256 KERNEL-SHA256 INITRAMFS-SHA256",
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
        openvmm: artifacts::absolute(Path::new(&args[2]))?,
        kernel: artifacts::absolute(Path::new(&args[3]))?,
        initramfs: artifacts::absolute(Path::new(&args[4]))?,
        identity: RuntimeIdentity::new(identity(6)?, identity(7)?, identity(8)?),
        hypervisor,
        cpu_profile: Some("auto".to_owned()),
        windows_breakaway: false,
        options: LaunchOptions::default(),
    };
    let config = VmConfig {
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
            capture: None,
        }),
    };
    vm::create_private_dir(Path::new(&args[5]))?;
    let dir = vm::VmDir::open(Path::new(&args[5]))?;
    let vm::VmmCommand {
        program,
        arguments,
        working_dir,
        capability,
        outputs,
        inherit,
        log,
        breakaway_from_job,
    } = vm::create(&runtime, &config, None, &dir)?.into_command();
    let command = supervisor::VmmCommand {
        program,
        arguments,
        working_dir,
        breakaway_from_job,
        inherit: inherit.iter().map(Inherited::new).collect(),
    };
    let mut launched = supervisor::spawn_vm(&command, capability.as_bytes(), log)?;
    let result = (|| -> Result<(), Box<dyn Error>> {
        let process = launched.process()?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let console = outputs.console.as_deref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "no prepared boot console")
        })?;
        let mut console = session::connect(
            console,
            process.reference().pid,
            process.reference().start_time,
            deadline,
        )?;
        read_until(console.as_mut(), b"NVX-GUEST-BOOT-OK:", deadline)?;
        console.write_all(
            b"printf 'NVX-EXTERNAL-LAUNCH-OK\\n'; /sbin/nvx-exit 0\n",
            Some(deadline.saturating_duration_since(Instant::now())),
        )?;
        read_until(console.as_mut(), b"\nNVX-EXTERNAL-LAUNCH-OK", deadline)?;
        if process.wait(deadline)?.is_none() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "VM did not exit").into());
        }
        match launched.wait_timeout(Duration::ZERO)? {
            Some(0) => {}
            Some(status) => {
                return Err(io::Error::other(format!("VMM exited with status {status}")).into());
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
        if !launched.kill_and_wait() {
            return Err(
                io::Error::other(format!("{error}; could not reap the launched VM")).into(),
            );
        }
        return Err(error);
    }
    launched.detach();
    println!(
        "external cold launch completed; outcome={}",
        outputs.outcome.display()
    );
    Ok(())
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
