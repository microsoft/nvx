//! `nvx-init TIME_DAEMON [ANNOUNCER]`: NVX's PID 1 for layered roots, which runs ahead of the
//! workloads.
//!
//! A layered root's early boot, which the shell `/init` runs, mounts the kernel's file systems,
//! runs `nvx-time boot`, whose child becomes the time daemon, configures the network, and, when
//! the network publishes the workload's listeners, starts their announcer, `/sbin/nvx-ingress`.
//! To hand PID 1 over to `nvx-init`, the early boot replaces itself with it, so that `nvx-init`
//! inherits both helpers as its children, and passes it their process IDs: `TIME_DAEMON`, and
//! `ANNOUNCER` when the announcer runs. Each is a decimal process ID above 1, and the two differ.
//! As PID 1, `nvx-init`:
//!
//! - reads `SIGCHLD` and `SIGTERM` from a signal descriptor. `SIGCHLD` wakes it to collect the
//!   children that ended, and `SIGTERM` asks it to end the VM, which it does once it collected
//!   the children that ended before: a helper among them ends the VM as a failure. No other
//!   signal reaches it: the kernel delivers to PID 1 only the signals that it handles or blocks;
//! - gives itself and each helper an out-of-memory score adjustment of -1000, which exempts them
//!   from the out-of-memory killer, and a nice value of -10, ahead of the workloads' 0;
//! - registers exactly those helpers, and ends the VM when either ends, whatever its status: the
//!   time daemon disciplines the guest's clock and watches it, and the announcer tells the gateway
//!   the workload's listeners, so the VM cannot go on without them. A helper that ended before
//!   `nvx-init` started, whose status it then collects first, ends the VM too, as does a process ID
//!   that is not one of PID 1's children, whose end PID 1 could not tell;
//! - collects the status of every child that ends, which includes every orphaned process, whose
//!   parent the kernel makes PID 1, from one `waitpid(-1)` loop, the process's only one;
//! - writes each diagnostic to the console, which it opens for each line without making it its
//!   controlling terminal, or to its standard error when the console does not open, and reads
//!   nothing from it;
//! - ends the VM itself, without another guest program: it writes every file system's modified
//!   data to its storage, with `sync`, and then stops the VM through OpenVMM's control port with a
//!   status: 0 after `SIGTERM`, or 125, as `nvx-init-agent`'s failures do, after a failure, which
//!   an `NVX-INIT-ERROR:` line on the console names first.
//!
//! Outside PID 1, `nvx-init` changes nothing: it fails with status 2 and a message.

use std::ffi::{CStr, CString};
use std::process::ExitCode;
use std::time::Duration;

use super::parse::parse_decimal;
use super::status::ExitStatus;
use super::sys::{
    self, ChildStatus, Errno, Mode, OpenFlags, Pollable, ProcessId, Signal, SignalDescriptor,
    kernel,
};

/// The process ID of the init process.
const INIT_PROCESS: i32 = 1;
/// The VM's exit status after a failure of PID 1 or the end of a helper: that of
/// `nvx-init-agent`'s failures.
const FAILURE: u8 = 125;
/// The out-of-memory score adjustment of PID 1 and its helpers, Linux's `OOM_SCORE_ADJ_MIN`,
/// which exempts them from the out-of-memory killer.
const OOM_SCORE_ADJUSTMENT: i32 = -1000;
/// The nice value of PID 1 and its helpers, ahead of the workloads' 0.
const NICE: i32 = -10;
/// The signals that PID 1 reads from its signal descriptor: the end of a child, and the request
/// to end the VM.
const SIGNALS: [Signal; 2] = [Signal::SIGCHLD, Signal::SIGTERM];
/// Pause before PID 1 polls its signal descriptor again after `poll` failed.
const POLL_RETRY: Duration = Duration::from_secs(1);

/// The helpers that the shell `/init` hands over: the only processes whose end ends the VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Helpers {
    /// The time daemon.
    time: ProcessId,
    /// The announcer of the workload's listeners, when the network publishes them.
    announcer: Option<ProcessId>,
}

impl Helpers {
    /// Parses `TIME_DAEMON [ANNOUNCER]`, the arguments after the program's name, or returns
    /// `None` for anything else.
    fn parse(arguments: &[CString]) -> Option<Self> {
        let process = |argument: &CString| {
            parse_decimal(argument.as_bytes())
                .and_then(|id| i64::try_from(id).ok())
                .and_then(ProcessId::from_positive)
                .filter(|process| process.raw() != INIT_PROCESS)
        };
        let helpers = match arguments {
            [time] => Self {
                time: process(time)?,
                announcer: None,
            },
            [time, announcer] => Self {
                time: process(time)?,
                announcer: Some(process(announcer)?),
            },
            _ => return None,
        };
        (helpers.announcer != Some(helpers.time)).then_some(helpers)
    }

    /// Returns each helper's process ID, with the helper's name.
    fn named(self) -> impl Iterator<Item = (&'static str, ProcessId)> {
        std::iter::once(("the time daemon", self.time))
            .chain(self.announcer.map(|announcer| ("the announcer", announcer)))
    }

    /// Returns the name of the helper `process`, or `None` for another process.
    fn name(self, process: ProcessId) -> Option<&'static str> {
        self.named()
            .find(|&(_, helper)| helper == process)
            .map(|(name, _)| name)
    }
}

/// The operations of PID 1 on the system, which tests replace.
trait System {
    /// Blocks [`SIGNALS`], with the default disposition of `SIGCHLD`, and opens the descriptor
    /// that reads them.
    fn listen(&mut self) -> Result<(), Errno>;
    /// Gives `process`, or PID 1 for `None`, [`OOM_SCORE_ADJUSTMENT`] and [`NICE`].
    fn raise_priority(&mut self, process: Option<ProcessId>) -> Result<(), Errno>;
    /// Collects the status of the child `process` if it ended, returns `None` while it runs, and
    /// fails with `ECHILD` if `process` is not a child.
    fn adopt(&mut self, process: ProcessId) -> Result<Option<ChildStatus>, Errno>;
    /// Collects the status of one child that ended, without waiting, or returns `None` when none
    /// did.
    fn reap(&mut self) -> Option<(ProcessId, ChildStatus)>;
    /// Waits for one of [`SIGNALS`], and returns it.
    fn wait_signal(&mut self) -> Option<Signal>;
    /// Writes the diagnostic `line` to the console.
    fn report(&mut self, line: &[u8]);
    /// Writes every file system's modified data to its storage, and then stops the VM with
    /// `status`. In production it does not return.
    fn shut_down(&mut self, status: u8);
}

/// The production operations.
struct RealSystem {
    /// The descriptor that reads [`SIGNALS`], once [`listen`](System::listen) opened it.
    signals: Option<SignalDescriptor>,
}

impl System for RealSystem {
    fn listen(&mut self) -> Result<(), Errno> {
        // An inherited `SIG_IGN` would have the kernel collect each child's status itself.
        sys::reset_child_signal();
        self.signals = Some(SignalDescriptor::new(&SIGNALS)?);
        Ok(())
    }

    fn raise_priority(&mut self, process: Option<ProcessId>) -> Result<(), Errno> {
        let path = match process {
            None => kernel::OOM_SCORE_ADJUSTMENT.to_owned(),
            Some(process) => {
                sys::c_string(format!("/proc/{}/oom_score_adj", process.raw()).as_bytes())?
            }
        };
        sys::write_number(&path, OOM_SCORE_ADJUSTMENT)?;
        kernel::set_nice(process, NICE)
    }

    fn adopt(&mut self, process: ProcessId) -> Result<Option<ChildStatus>, Errno> {
        sys::try_wait(process)
    }

    fn reap(&mut self) -> Option<(ProcessId, ChildStatus)> {
        sys::reap_any_nonblocking().map(|child| (child.process, child.status))
    }

    fn wait_signal(&mut self) -> Option<Signal> {
        let signals = self.signals.as_ref()?;
        loop {
            if let Some(event) = signals.read() {
                return event.signal;
            }
            match sys::poll([Pollable::Signals(signals)], None) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(_) => std::thread::sleep(POLL_RETRY),
            }
        }
    }

    fn report(&mut self, line: &[u8]) {
        write_console(kernel::CONSOLE_DEVICE, line);
    }

    fn shut_down(&mut self, status: u8) {
        kernel::sync();
        sys::power_off(status)
    }
}

/// Runs `nvx-init` with the process's arguments. As PID 1, it serves until the VM must end, and
/// then ends it, so it does not return; elsewhere it fails with status 2.
#[doc(hidden)]
pub fn main() -> ExitCode {
    sys::restore_sigpipe();
    if sys::current_process().raw() != INIT_PROCESS {
        sys::write_standard_error(b"nvx-init: runs only as PID 1\n");
        return ExitStatus::Usage.into();
    }
    let arguments = sys::args();
    init(
        &mut RealSystem { signals: None },
        arguments.get(1..).unwrap_or_default(),
    );
    ExitStatus::Failure.into()
}

/// Serves as PID 1 with the helpers that `arguments` name until the VM must end, and then ends
/// it: with 0 after `SIGTERM`, or with [`FAILURE`] after the diagnostic of a failure.
fn init(system: &mut impl System, arguments: &[CString]) {
    let status = match serve(system, arguments) {
        Ok(()) => 0,
        Err(failure) => {
            system.report(format!("NVX-INIT-ERROR: {failure}\n").as_bytes());
            FAILURE
        }
    };
    system.shut_down(status);
}

/// Takes over as PID 1, and collects the status of every child that ends, until `SIGTERM` asks
/// it to end the VM and no helper ended before, or fails with what ends the VM otherwise.
fn serve(system: &mut impl System, arguments: &[CString]) -> Result<(), String> {
    system
        .listen()
        .map_err(|error| format!("cannot read signals: {}", error.desc()))?;
    system
        .raise_priority(None)
        .map_err(|error| format!("cannot raise the priority of PID 1: {}", error.desc()))?;
    let helpers = Helpers::parse(arguments).ok_or("usage: nvx-init TIME_DAEMON [ANNOUNCER]")?;
    for (name, process) in helpers.named() {
        let pid = process.raw();
        match system.adopt(process) {
            Ok(None) => {}
            Ok(Some(status)) => return Err(ended(name, process, status)),
            Err(error) => {
                return Err(format!(
                    "{name}, process {pid}, is not a child of PID 1: {}",
                    error.desc()
                ));
            }
        }
        system.raise_priority(Some(process)).map_err(|error| {
            format!(
                "cannot raise the priority of {name}, process {pid}: {}",
                error.desc()
            )
        })?;
    }
    let mut terminating = false;
    loop {
        while let Some((process, status)) = system.reap() {
            if let Some(name) = helpers.name(process) {
                return Err(ended(name, process, status));
            }
        }
        if terminating {
            return Ok(());
        }
        // The descriptor returns the lower of two pending signals first, `SIGTERM` before
        // `SIGCHLD`: a helper may have ended just before `SIGTERM` came, so PID 1 collects once
        // more before it accepts `SIGTERM`.
        terminating = system.wait_signal() == Some(Signal::SIGTERM);
    }
}

/// Describes the end of the helper `name`, process `process`, with `status`.
fn ended(name: &str, process: ProcessId, status: ChildStatus) -> String {
    let how = match status {
        ChildStatus::Exited(code) => format!("exited with status {code}"),
        ChildStatus::Signaled(signal) => format!("was killed by signal {signal}"),
        ChildStatus::Other => "ended".to_owned(),
    };
    format!("{name}, process {}, {how}", process.raw())
}

/// Writes `line` to the console device `console`, which it opens for the line without making it
/// the process's controlling terminal, or to standard error if it cannot.
fn write_console(console: &CStr, line: &[u8]) {
    let flags = OpenFlags::O_WRONLY | OpenFlags::O_NOCTTY | OpenFlags::O_CLOEXEC;
    let written =
        sys::open(console, flags, Mode::empty()).and_then(|console| console.write_all(line));
    if written.is_err() {
        sys::write_standard_error(line);
    }
}

#[cfg(test)]
mod tests;
