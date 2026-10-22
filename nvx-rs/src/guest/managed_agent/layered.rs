//! `nvx-managed-agent --layered-root`: the PID 1 of a managed sandbox's layered root.
//!
//! `/init` runs its prelude, then executes this mode for a sandbox in the managed lifecycle. As
//! PID 1, the agent:
//!
//! 1. reads the kernel command line, before it changes anything: the root's boot tokens, which
//!    [`protocol::root`](crate::protocol::root) defines, the cgroups' resource values, the
//!    workload's identity and hostname, the control terminal, and the live shares, which
//!    [`protocol::share`](crate::protocol::share) defines;
//! 2. bootstraps the guest with [`root::bootstrap`]: the `agent` and `container` cgroups, with
//!    every user process of the cgroup root, itself and the prelude's daemons, moved into
//!    `agent`, and user namespaces kept unavailable. It prints `NVX-SANDBOX-CGROUP: agent=PID,...`
//!    with the processes that it moved;
//! 3. assembles the root with [`root::assemble`]: the EROFS image over the ext4 scratch;
//! 4. looks up the workload's account in the root's user and group databases;
//! 5. mounts the live shares in the root, and prints `NVX-SANDBOX-SHARE:` for each;
//! 6. writes the per-boot machine ID, which the container launcher binds over the root's
//!    `/etc/machine-id`;
//! 7. prints `NVX-SANDBOX-READY: managed layers=1`, and serves the control session on
//!    `/dev/hvc2` as the sandbox agent, which launches each workload in the root, until the host
//!    stops it;
//! 8. tears the root down, prints `NVX-SANDBOX-EXIT: status=STATUS`, and ends the VM through
//!    `nvx-exit` with that status: 0 after a stop, 125 if the session failed, or 1 if an unmount
//!    failed, which it reports.
//!
//! A failure before the session prints `NVX-SANDBOX-ERROR: MESSAGE`, tears down what the agent
//! mounted, and ends the VM with status 125. These are the steps, markers, and statuses of NVX's
//! shell init, which runs the one-shot lifecycle.
//!
//! As PID 1, the agent is the parent of the prelude's daemons and of every orphaned process, and
//! collects their statuses as they end: between requests, while it waits for the next one, and
//! while a workload runs, in the exec loop's `waitpid(-1)`, which hands the workload's status to
//! the workload's [`process::Child`](crate::guest::process::Child). The agent never requests I/O
//! port permission, which `/init` never held either, so the workload stages that it spawns hold
//! none.

mod account;
mod boot;
#[cfg(test)]
mod tests;

use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::guest::mappings::Access;
use crate::guest::root::{self, Assembly, Device, Root, ShareChild, ShareKind, TeardownError};
use crate::guest::status::{CommandError, ExitStatus};
use crate::guest::sys::{self, DirectStdout};
use crate::protocol::root::{DISTRO_MMIO_BASE, SCRATCH_MMIO_BASE};
use crate::protocol::share::{self, Shares};

use self::boot::Boot;
use super::diagnostics::write_stderr;
use super::seam::System;
use super::{AgentConfig, cgroup};

/// The runtime directory, below which the root's mounts are.
const RUNTIME: &str = "/run/nvx";
/// The per-boot machine ID, which the container launcher binds over the root's `/etc/machine-id`.
const MACHINE_ID: &str = "/run/nvx/workload-machine-id";
/// The process ID of a one-shot workload, which the teardown removes as the shell init's did.
const CONTAINER_PID: &str = "/run/nvx/container.pid";
/// The kernel's random UUID, a new one at each read.
const RANDOM_UUID: &str = "/proc/sys/kernel/random/uuid";
/// The control terminal of the managed lifecycle.
const CONTROL_TTY: &CStr = c"/dev/hvc2";
/// How long the assembly waits for each block device: as long as the shell init's 600 polls,
/// 50 ms apart.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long PID 1 sleeps at a time once the VM is ending.
const IDLE: Duration = Duration::from_secs(3600);

/// Runs the layered root's PID 1, which ends the VM and never returns, or fails with status 125
/// in a process that is not PID 1, before it changes anything.
pub(super) fn run(system: &mut impl System) -> Result<(), CommandError> {
    if sys::current_process().raw() != 1 {
        write_stderr(b"nvx-managed-agent: --layered-root runs only as PID 1\n");
        return Err(CommandError::Agent);
    }
    let status = match start(system) {
        Ok((root, config)) => {
            print(b"NVX-SANDBOX-READY: managed layers=1\n");
            let result = super::serve(system, &CString::from(CONTROL_TTY), &config);
            tear_down(root, ExitStatus::from(result) as u8)
        }
        Err(failure) => {
            print_error(failure.message.as_bytes());
            if let Some(root) = failure.root
                && let Err(error) = root.teardown()
            {
                print_failures(&error);
            }
            ExitStatus::AgentFailure as u8
        }
    };
    end_vm(system, status)
}

/// A failure before the session: its message, and the root that the agent assembled, if any,
/// which the agent tears down.
struct Failure {
    /// The message.
    message: String,
    /// The root, if the agent assembled it.
    root: Option<Box<Root>>,
}

impl Failure {
    /// Returns a failure with `message`, before the agent assembled the root.
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            root: None,
        }
    }

    /// Returns a failure with `message`, after the agent assembled `root`.
    fn with_root(message: impl Into<String>, root: Root) -> Self {
        Self {
            message: message.into(),
            root: Some(Box::new(root)),
        }
    }
}

/// Runs the steps before the session, and returns the root and the configuration of the session
/// in it.
fn start(system: &impl System) -> Result<(Root, AgentConfig), Failure> {
    let paths = system.paths();
    let command_line = fs::read(&paths.proc_cmdline).map_err(|error| {
        Failure::new(format!("failed to read the kernel command line: {error}"))
    })?;
    let boot = Boot::parse(&command_line).map_err(Failure::new)?;
    let layout = cgroup::layout(paths, false).map_err(|error| {
        Failure::new(format!(
            "failed to name the agent's cgroups: {}",
            error.desc()
        ))
    })?;
    let bootstrap = root::bootstrap(&layout, &boot.resources)
        .map_err(|error| Failure::new(error.to_string()))?;
    print(&cgroup_line(bootstrap.moved()));
    let mut root = root::assemble(&Assembly {
        runtime: RUNTIME.into(),
        distro: Device {
            mmio: DISTRO_MMIO_BASE,
            sectors: boot.layer_sectors,
        },
        distro_uuid: boot.image_uuid,
        scratch: Device {
            mmio: SCRATCH_MMIO_BASE,
            sectors: None,
        },
        timeout: DEVICE_TIMEOUT,
    })
    .map_err(|error| Failure::new(error.to_string()))?;
    let account = match account::lookup(root.path(), &boot.uid, &boot.gid) {
        Ok(account) => account,
        Err(message) => return Err(Failure::with_root(message, root)),
    };
    let config = match session_config(&root, &boot, account) {
        Ok(config) => config,
        Err(message) => return Err(Failure::with_root(message, root)),
    };
    if let Err(error) = mount_shares(&mut root, &boot.shares) {
        return Err(Failure::with_root(error.to_string(), root));
    }
    if let Err(error) = write_machine_id() {
        return Err(Failure::with_root(
            format!("failed to write the workload machine ID: {error}"),
            root,
        ));
    }
    Ok((root, config))
}

/// Returns the configuration of the sandbox agent's session in `root`, as the workload's
/// `account`.
fn session_config(
    root: &Root,
    boot: &Boot,
    account: account::Account,
) -> Result<AgentConfig, &'static str> {
    let text = |bytes: &[u8]| CString::new(bytes).map_err(|_| "the workload's account holds a NUL");
    Ok(AgentConfig {
        rootfs: text(root.path().as_os_str().as_bytes())?,
        hostname: boot.hostname.clone(),
        uid: text(&boot.uid)?,
        gid: text(&boot.gid)?,
        user: text(&account.name)?,
        home: text(&account.home)?,
        direct: false,
    })
}

/// Returns the line that reports the processes that the bootstrap moved into the agent's cgroup.
fn cgroup_line(moved: &[u32]) -> Vec<u8> {
    let processes: Vec<String> = moved.iter().map(u32::to_string).collect();
    format!("NVX-SANDBOX-CGROUP: agent={}\n", processes.join(",")).into_bytes()
}

/// Mounts `shares` in `root`, and prints a line for each share or child.
fn mount_shares(root: &mut Root, shares: &Shares<'_>) -> Result<(), root::Error> {
    let access = |read_only| {
        if read_only {
            Access::ReadOnly
        } else {
            Access::ReadWrite
        }
    };
    match shares {
        Shares::None => {}
        Shares::Single {
            tag,
            target,
            read_only,
        } => {
            root.mount_share(
                tag,
                Path::new(OsStr::from_bytes(target)),
                access(*read_only),
            )?;
            print(&share_line(target, *read_only, "tag", tag));
        }
        Shares::Aggregate {
            tag,
            read_only,
            children,
        } => {
            let binds: Vec<ShareChild> = children
                .iter()
                .map(|child| ShareChild {
                    name: child.name.to_owned(),
                    target: PathBuf::from(OsStr::from_bytes(child.target)),
                    access: access(child.read_only),
                    kind: if child.file {
                        ShareKind::File
                    } else {
                        ShareKind::Directory
                    },
                })
                .collect();
            root.mount_aggregate(tag, access(*read_only), &binds)?;
            for child in children {
                print(&share_line(
                    child.target,
                    child.read_only,
                    "child",
                    child.name,
                ));
            }
        }
    }
    Ok(())
}

/// Returns the line that reports a mounted share or child at `target`, read-only if
/// `read_only`, and its `key`, `tag` or `child`, and `name`.
fn share_line(target: &[u8], read_only: bool, key: &str, name: &str) -> Vec<u8> {
    [
        b"NVX-SANDBOX-SHARE: target=".as_slice(),
        target,
        format!(" mode={} {key}={name}\n", share::mode(read_only)).as_bytes(),
    ]
    .concat()
}

/// Writes a new per-boot machine ID, from the kernel's random UUID, readable by the workload.
fn write_machine_id() -> std::io::Result<()> {
    let id = machine_id(&fs::read(RANDOM_UUID)?);
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(MACHINE_ID)?
        .write_all(&id)
}

/// Returns the machine ID of the UUID text `uuid`: its hexadecimal digits and a newline.
fn machine_id(uuid: &[u8]) -> Vec<u8> {
    let mut id: Vec<u8> = uuid
        .iter()
        .copied()
        .filter(|&byte| byte != b'-' && byte != b'\n')
        .collect();
    id.push(b'\n');
    id
}

/// Tears `root` down after the session ended with `status`, prints the exit line, and returns
/// the VM's status: `status`, or 1 if an unmount failed.
fn tear_down(root: Root, status: u8) -> u8 {
    let _ = fs::remove_file(CONTAINER_PID);
    let _ = fs::remove_file(MACHINE_ID);
    let status = match root.teardown() {
        Ok(()) => status,
        Err(error) => {
            print_failures(&error);
            ExitStatus::Failure as u8
        }
    };
    print(format!("NVX-SANDBOX-EXIT: status={status}\n").as_bytes());
    status
}

/// Prints each failed unmount of a teardown.
fn print_failures(error: &TeardownError) {
    for failure in error.failures() {
        print_error(failure.to_string().as_bytes());
    }
}

/// Ends the VM with `status` through `nvx-exit`, which takes PID 1's place. If it cannot run,
/// PID 1 waits for the host to stop the VM, since the kernel panics when PID 1 exits.
fn end_vm(system: &mut impl System, status: u8) -> ! {
    if let Err(error) = super::exit_vm(system, status) {
        print_error(format!("failed to run nvx-exit: {}", error.desc()).as_bytes());
    }
    loop {
        std::thread::sleep(IDLE);
    }
}

/// Prints `line` to standard output, the console, ignoring a failure.
fn print(line: &[u8]) {
    sys::report(&mut DirectStdout, &[line]);
}

/// Prints `NVX-SANDBOX-ERROR: MESSAGE` to standard error, the console.
fn print_error(message: &[u8]) {
    write_stderr(&[b"NVX-SANDBOX-ERROR: ".as_slice(), message, b"\n"].concat());
}
