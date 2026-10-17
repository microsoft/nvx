//! The `nvx-rs` command-line interface, the backend of the runtime steps of `nvx.py`.
//!
//! ```text
//! nvx-rs [-h] [--root PATH] artifacts
//! nvx-rs [-h] [--root PATH] egress compile PATH
//! nvx-rs [-h] [--root PATH] sandbox [run|provision|start|exec|stop|deprovision] [OPTIONS]
//! ```
//!
//! - `artifacts` prints the paths of the artifacts that `nvx.py` runs as JSON.
//! - `egress compile PATH` compiles an egress policy file of `--network-egress-policy-file` and
//!   prints its native rules as JSON.
//! - `sandbox` is `nvx.py sandbox`: the same options, outputs, exit statuses, outcome reports,
//!   and state directories.
//!
//! The artifact root is `--root`, or else the release or repository checkout that holds the
//! executable, and the `NVX_OPENVMM` environment variable names another OpenVMM executable.
//!
//! Errors print `error: MESSAGE` to standard error and exit with status 1, as `nvx.py` reports a
//! `ScriptError`. Usage errors print the usage and `PROG: error: MESSAGE`, and exit with status
//! 2, as `argparse` does. Ctrl-C prints `Interrupted` and exits with status 130. Text ends its
//! lines as Python's `print` ends them: `\r\n` on Windows, and `\n` elsewhere.

mod args;
mod artifacts;
mod egress;
mod output;
mod sandbox;

use std::ffi::OsString;

use self::args::{Parser, Receiver, Spec, Stop};
use crate::host::platform;
use crate::host::python::builtins::repr;

/// The name of the program in usage messages.
const PROGRAM: &str = "nvx-rs";
/// The commands, in the order in which usage messages list them.
const COMMANDS: [&str; 3] = ["artifacts", "egress", "sandbox"];
/// The usage of the top-level parser.
const USAGE: &str = "usage: nvx-rs [-h] [--root PATH] {artifacts,egress,sandbox} ...";
/// The help of the top-level parser.
const HELP: &str = "\
usage: nvx-rs [-h] [--root PATH] {artifacts,egress,sandbox} ...

Run the NVX steps that nvx.py delegates: artifact lookup, egress policy
compilation, and sandboxes.

positional arguments:
  {artifacts,egress,sandbox}
    artifacts           print the paths of the NVX artifacts as JSON
    egress              compile egress policy files
    sandbox             run or manage workloads over EROFS layers and private
                        ext4 scratch

options:
  -h, --help            show this help message and exit
  --root PATH           the NVX repository checkout or extracted release whose
                        artifacts to use (default: the release or checkout that
                        holds this executable)

environment:
  NVX_OPENVMM           the OpenVMM executable to run instead of the one of the
                        artifact root";

/// How a command ends other than by succeeding.
#[derive(Debug)]
pub(crate) enum Exit {
    /// `error: MESSAGE` on standard error, and status 1: what `nvx.py` does for a `ScriptError`,
    /// an `OSError`, a `RuntimeError`, or a `ValueError`.
    Error(String),
    /// A usage error of the parser `prog`: its usage, then `PROG: error: MESSAGE`, and status 2.
    Usage {
        /// The parser's usage line.
        usage: &'static str,
        /// The parser's name, such as `nvx-rs sandbox`.
        prog: &'static str,
        /// The message.
        message: String,
    },
    /// Help on standard output, and status 0.
    Help(&'static str),
    /// Ctrl-C: `Interrupted` on standard error, and status 130.
    Interrupted,
    /// `SystemExit(status)`: the status of OpenVMM or of a managed workload.
    Status(i32),
}

impl Exit {
    /// Returns the error that prints `message`.
    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    /// Returns the end of a parse that `stop` ended, for the parser `prog` with `usage` and
    /// `help`.
    pub(crate) fn from_stop(
        stop: Stop,
        prog: &'static str,
        usage: &'static str,
        help: &'static str,
    ) -> Self {
        match stop {
            Stop::Usage(message) => Self::Usage {
                usage,
                prog,
                message,
            },
            Stop::Help => Self::Help(help),
        }
    }

    /// Reports the end on the standard streams and returns the exit status.
    fn report(self) -> i32 {
        match self {
            Self::Error(message) => {
                output::print_stderr(&format!("error: {message}"));
                1
            }
            Self::Usage {
                usage,
                prog,
                message,
            } => {
                output::print_stderr(&format!("{usage}\n{prog}: error: {message}"));
                2
            }
            Self::Help(text) => match output::print_stdout(text) {
                Ok(()) => 0,
                Err(error) => Self::Error(output::stream_error(&error)).report(),
            },
            Self::Interrupted => {
                output::print_stderr("Interrupted");
                130
            }
            Self::Status(status) => status,
        }
    }
}

impl From<crate::Error> for Exit {
    fn from(error: crate::Error) -> Self {
        Self::Error(error.message().to_owned())
    }
}

/// The result of a command.
pub(crate) type Result<T = ()> = std::result::Result<T, Exit>;

/// Runs `nvx-rs` with the process's arguments and returns its exit status.
///
/// The CLI runs on the calling thread, which should be the main thread of the process, and from
/// here on, Ctrl-C ends the command that runs, with status 130, instead of the process.
pub fn main() -> i32 {
    // Like Python, which installs its handler as it starts, run on without one if that fails.
    let _ = platform::watch_interrupts();
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    match run(&arguments) {
        Ok(()) => 0,
        Err(exit) => exit.report(),
    }
}

/// A parsed command.
enum Command {
    /// `artifacts`.
    Artifacts,
    /// `egress compile PATH`.
    EgressCompile(String),
    /// `sandbox ...`.
    Sandbox(Box<sandbox::Options>),
}

/// The top-level options.
#[derive(Default)]
struct TopLevel {
    /// `--root`, if given.
    root: Option<String>,
}

impl Receiver for TopLevel {
    fn take(
        &mut self,
        _name: &'static str,
        value: Option<&str>,
    ) -> std::result::Result<(), String> {
        self.root = value.map(str::to_owned);
        Ok(())
    }
}

/// Parses the arguments, reports the unrecognized ones, and runs the command.
fn run(arguments: &[OsString]) -> Result {
    let usage = |message: String| Exit::Usage {
        usage: USAGE,
        prog: PROGRAM,
        message,
    };
    let mut strings = Vec::with_capacity(arguments.len());
    for (index, argument) in arguments.iter().enumerate() {
        let text = argument.to_str().ok_or_else(|| {
            usage(format!(
                "argument {} is not valid Unicode: {}",
                index + 1,
                repr(&argument.to_string_lossy())
            ))
        })?;
        strings.push(text.to_owned());
    }
    let parser = Parser::new(&[Spec::one("--root")], &[Spec::one("command")]);
    let mut top = TopLevel::default();
    let (index, mut extras) = parser
        .split_command(&strings, &mut top)
        .map_err(|stop| Exit::from_stop(stop, PROGRAM, USAGE, HELP))?;
    let rest = &strings[index + 1..];
    let (command, unrecognized) = match strings[index].as_str() {
        "artifacts" => (Command::Artifacts, artifacts::parse(rest)?),
        "egress" => {
            let (path, unrecognized) = egress::parse(rest)?;
            (Command::EgressCompile(path), unrecognized)
        }
        "sandbox" => {
            let (options, unrecognized) = sandbox::parse(rest)?;
            (Command::Sandbox(Box::new(options)), unrecognized)
        }
        other => {
            return Err(usage(format!(
                "argument command: {}",
                args::invalid_choice(other, &COMMANDS)
            )));
        }
    };
    extras.extend(unrecognized);
    if !extras.is_empty() {
        return Err(usage(format!(
            "unrecognized arguments: {}",
            extras.join(" ")
        )));
    }
    let root = top.root.as_deref();
    match command {
        Command::Artifacts => artifacts::run(root),
        Command::EgressCompile(path) => egress::run(&path),
        Command::Sandbox(options) => sandbox::run(root, *options),
    }
}
