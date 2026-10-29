//! The runtime adapter's guest agent, a static `x86_64-unknown-linux-musl` executable.
//!
//! - `runtime-adapter-guest` reports the contract that it was built against.
//! - `runtime-adapter-guest serve` serves the host program with the adapter's own framing on its
//!   standard input and output, which connect to the guest's end of the control session between
//!   the host and the guest, such as a control console in raw mode. It exits with 0 once the host
//!   stopped it or the broker closed the stream, and with 1, after a diagnostic on standard error,
//!   once serving failed.
//! - `runtime-adapter-guest init TTY` serves the host program on the tty TTY, which it puts in raw
//!   mode itself, in place of NVX's managed agent, whose initramfs runs it on the VM's control tty
//!   as PID 1; see `runtime_adapter::initramfs`. It then ends with 0 once the host stopped it, and
//!   with 1, after a diagnostic on standard error, once serving failed or the broker closed the
//!   tty.
//! - `runtime-adapter-guest init TTY layered` runs PID 1 of a VM with a layered root, in place of
//!   NVX's init agent: it starts an unrelated child, `runtime-adapter-guest idle`, and the worker,
//!   `runtime-adapter-guest worker TTY SIBLING`, collects the status of every child until the
//!   worker ends, and ends with the worker's status; see `runtime_adapter::worker`.
//! - `runtime-adapter-guest worker TTY [SIBLING]` runs the worker on the tty TTY: it assembles the
//!   layered root that the host configures, and serves the host program until the host stops it.
//!   SIBLING is the process ID of PID 1's unrelated child, which its probes check. It exits with 0
//!   once the host stopped it, with 11 to 16 once a stage of the root failed, and with 1 once its
//!   session failed or the broker closed the tty, each but the first after a diagnostic on
//!   standard error. In a template's VM, whose kernel command line names the platform snapshot
//!   tier, the worker captures the VM before it assembles any root instead, on x86_64, as
//!   `runtime_adapter::worker::capture` describes: it exits with 13 once the template's bootstrap
//!   failed, or with 20 to 30. In a clone, which it repairs, then activates and serves, it exits
//!   with 40 to 45, 193, or 195 once the repair failed, with 50 to 58 once the clone's activation
//!   failed, as `nvx::protocol::status` documents, and, once the clone was ready, as the worker of
//!   a VM that boots cold does. It exits with 1 once its session failed. Each comes after a
//!   diagnostic on standard error, unless a committed capture ended the VM; see
//!   `runtime_adapter::template`.
//! - `runtime-adapter-guest idle` sleeps until a signal ends it.
//! - `runtime-adapter-guest entries ROOT CGROUP` runs the executions of the worker's `entries`
//!   probe in the root ROOT, each placed in the cgroup CGROUP, in a mount namespace of its own, and
//!   reports their facts on standard output; see `runtime_adapter::worker::entries`. It exits with
//!   0, or with 1 after a diagnostic on standard error.
//! - `runtime-adapter-guest inside DIRECTORY DESCRIPTOR` runs in each of those executions, from
//!   inside the root: it reports what the execution sees and may do on standard output, with its
//!   case's directory DIRECTORY and the number of the descriptor that its caller leaked. It exits
//!   with 0, or with 1 after a diagnostic on standard error.
//!
//! As PID 1, which must not exit, `init` ends the VM with its status instead, through NVX's
//! `/sbin/nvx-exit`, as NVX's managed agent does. Anything else is a usage error, with status 2.

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    use runtime_adapter::{
        GUEST_ENTRIES_COMMAND, GUEST_IDLE_COMMAND, GUEST_INIT_COMMAND, GUEST_INSIDE_COMMAND,
        GUEST_LAYERED_MODE, GUEST_SERVE_COMMAND, GUEST_WORKER_COMMAND,
    };
    use std::path::Path;

    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => {
            println!("runtime-adapter-guest: {}", runtime_adapter::contract());
            ExitCode::SUCCESS
        }
        [command] if command == GUEST_SERVE_COMMAND => serve(),
        [command, tty] if command == GUEST_INIT_COMMAND => init(Path::new(tty)),
        [command, tty, mode] if command == GUEST_INIT_COMMAND && mode == GUEST_LAYERED_MODE => {
            end(runtime_adapter::worker::supervise(Path::new(tty)))
        }
        [command, tty, sibling @ ..] if command == GUEST_WORKER_COMMAND && sibling.len() < 2 => {
            match sibling
                .first()
                .map(|id| id.to_str().and_then(|id| id.parse().ok()))
            {
                Some(None) => usage(),
                Some(Some(sibling)) => worker(Path::new(tty), Some(sibling)),
                None => worker(Path::new(tty), None),
            }
        }
        [command] if command == GUEST_IDLE_COMMAND => runtime_adapter::worker::idle(),
        [command, root, workload] if command == GUEST_ENTRIES_COMMAND => {
            entries(Path::new(root), Path::new(workload))
        }
        [command, directory, descriptor] if command == GUEST_INSIDE_COMMAND => {
            let descriptor = descriptor.to_str().and_then(|number| number.parse().ok());
            inside(Path::new(directory), descriptor)
        }
        _ => usage(),
    }
}

/// Reports a usage error.
#[cfg(target_os = "linux")]
fn usage() -> ExitCode {
    eprintln!(
        "usage: runtime-adapter-guest [{} | {} TTY [{}] | {} TTY [SIBLING] | {} | {} ROOT CGROUP | \
         {} DIRECTORY DESCRIPTOR]",
        runtime_adapter::GUEST_SERVE_COMMAND,
        runtime_adapter::GUEST_INIT_COMMAND,
        runtime_adapter::GUEST_LAYERED_MODE,
        runtime_adapter::GUEST_WORKER_COMMAND,
        runtime_adapter::GUEST_IDLE_COMMAND,
        runtime_adapter::GUEST_ENTRIES_COMMAND,
        runtime_adapter::GUEST_INSIDE_COMMAND,
    );
    ExitCode::from(2)
}

/// Serves the session on standard input and output.
#[cfg(target_os = "linux")]
fn serve() -> ExitCode {
    let stdio = Stdio {
        input: std::io::stdin().lock(),
        output: std::io::stdout().lock(),
    };
    match runtime_adapter::guest::serve(stdio) {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("runtime-adapter-guest: {error}");
            ExitCode::FAILURE
        }
    }
}

/// NVX's program that ends the VM with an exit status, which its initramfs provides.
#[cfg(target_os = "linux")]
const NVX_EXIT: &str = "/sbin/nvx-exit";

/// Serves the session on the tty `tty` in place of NVX's managed agent, then ends with the status
/// of serving.
#[cfg(target_os = "linux")]
fn init(tty: &std::path::Path) -> ExitCode {
    use runtime_adapter::guest::{self, Served};

    let status: u8 = match guest::serve_terminal(tty) {
        Ok(Served::Stopped) => 0,
        Ok(Served::Closed) => {
            eprintln!("runtime-adapter-guest: the broker closed {}", tty.display());
            1
        }
        Err(error) => {
            eprintln!("runtime-adapter-guest: {error}");
            1
        }
    };
    end(status)
}

/// Runs the worker on the tty `tty`, with the process ID of PID 1's unrelated child, if any, and
/// ends with its status. In a template's VM, the worker captures the VM instead; see
/// `capture_template`.
#[cfg(target_os = "linux")]
fn worker(tty: &std::path::Path, sibling: Option<u32>) -> ExitCode {
    use runtime_adapter::worker::{self, LibraryRoot, Worked};

    let mut operations = LibraryRoot::new(sibling);
    #[cfg(target_arch = "x86_64")]
    if let Some(status) = capture_template(tty, &mut operations) {
        return ExitCode::from(status);
    }
    let worked = worker::work_on_terminal(tty, &mut operations);
    let status = match worked {
        Ok(worked) => {
            match &worked {
                Worked::Stopped => {}
                Worked::Failed(failure) => eprintln!("runtime-adapter-guest: {failure}"),
                Worked::Closed | Worked::Abandoned => {
                    eprintln!("runtime-adapter-guest: the broker closed {}", tty.display());
                }
                // Only a clone's worker activates, in `capture_template`.
                Worked::Unactivated { problem, .. } => {
                    eprintln!("runtime-adapter-guest: {problem}");
                }
            }
            worked.status()
        }
        Err(error) => {
            eprintln!("runtime-adapter-guest: {error}");
            worker::SESSION_STATUS
        }
    };
    // Closes what the probes held, then unmounts a root that the worker still holds, ignoring
    // failures, before the worker exits.
    drop(operations);
    ExitCode::from(status)
}

/// Reads the kernel command line through `operations`, before anything else, and captures a
/// template's VM on the tty `tty`, with `operations`, as `runtime_adapter::worker::capture`
/// describes, then repairs a clone that a restore of its snapshot started, and serves it through
/// its activation. Returns the status with which the worker ends then, after a diagnostic on
/// standard error, or `None` in a VM that boots cold, whose worker serves the host.
///
/// A command line that the worker cannot read, or that `runtime_adapter::template::boot` refuses,
/// ends the worker with `COMMAND_LINE_STATUS`.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn capture_template(
    tty: &std::path::Path,
    operations: &mut runtime_adapter::worker::LibraryRoot,
) -> Option<u8> {
    use runtime_adapter::template::{self, Boot, COMMAND_LINE_STATUS};
    use runtime_adapter::worker::capture;
    use runtime_adapter::worker::{RootOperations, SESSION_STATUS};

    let boot = operations
        .command_line()
        .and_then(|command_line| template::boot(&command_line).map_err(|error| error.to_string()));
    let gateway = match boot {
        Ok(Boot::Cold) => return None,
        Ok(Boot::Template { gateway }) => gateway,
        Err(problem) => {
            eprintln!("runtime-adapter-guest: {problem}");
            return Some(COMMAND_LINE_STATUS);
        }
    };
    Some(
        match capture::capture_on_terminal(tty, operations, gateway) {
            Ok(templated) => {
                eprintln!("runtime-adapter-guest: {templated}");
                templated.status()
            }
            Err(error) => {
                eprintln!("runtime-adapter-guest: {error}");
                SESSION_STATUS
            }
        },
    )
}

/// Runs the executions of the worker's `entries` probe in the root `root`, each placed in the cgroup
/// `workload`, and reports their facts.
#[cfg(target_os = "linux")]
fn entries(root: &std::path::Path, workload: &std::path::Path) -> ExitCode {
    use runtime_adapter::worker::entries;

    match entries::helper(root, workload).and_then(|facts| entries::report(&facts)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("runtime-adapter-guest: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Reports, from inside an execution of the worker's `entries` probe, what it sees and may do, with
/// its case's `directory` and the number of the descriptor that its caller leaked, if any.
#[cfg(target_os = "linux")]
fn inside(directory: &std::path::Path, descriptor: Option<u32>) -> ExitCode {
    use runtime_adapter::worker::entries;

    match entries::report(&entries::inside(directory, descriptor)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("runtime-adapter-guest: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Ends with `status`: as PID 1, by ending the VM through [`NVX_EXIT`].
#[cfg(target_os = "linux")]
fn end(status: u8) -> ExitCode {
    if std::process::id() != 1 {
        return ExitCode::from(status);
    }
    // PID 1 must not exit, or the kernel panics: `nvx-exit` replaces it and ends the VM. Should
    // it fail to start, PID 1 waits, and the host stops the VM.
    let error = std::os::unix::process::CommandExt::exec(
        std::process::Command::new(NVX_EXIT).arg(status.to_string()),
    );
    eprintln!("runtime-adapter-guest: cannot run {NVX_EXIT}: {error}");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Standard input and output as one stream, which carries nothing but the session's records.
#[cfg(target_os = "linux")]
struct Stdio {
    /// Standard input, from the broker.
    input: std::io::StdinLock<'static>,
    /// Standard output, to the broker.
    output: std::io::StdoutLock<'static>,
}

#[cfg(target_os = "linux")]
impl std::io::Read for Stdio {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buffer)
    }
}

#[cfg(target_os = "linux")]
impl std::io::Write for Stdio {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.output.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

// The guest agent runs only in Linux guests. Elsewhere this stub fails, so that every target still
// builds with every feature.
#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("runtime-adapter-guest runs only in Linux guests");
    ExitCode::FAILURE
}
