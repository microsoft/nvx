//! The `nvx-rs` command-line interface, the backend of the runtime steps of `nvx.py`.
//!
//! ```text
//! nvx-rs [-h] [--root PATH] [--prog PROG] artifacts
//! nvx-rs [-h] [--root PATH] [--prog PROG] egress compile PATH
//! nvx-rs [-h] [--root PATH] [--prog PROG] sandbox [run|provision|start|exec|stop|deprovision] [OPTIONS]
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
//! `--prog` names the program in usage and error messages, `nvx-rs` by default: `nvx.py`, which
//! runs `nvx-rs` for its `sandbox` command, passes its own name, so that its users see the
//! messages that it printed before it delegated the command.
//!
//! Errors print `error: MESSAGE` to standard error and exit with status 1, as `nvx.py` reports a
//! `ScriptError`. Usage errors print the usage and `PROG: error: MESSAGE`, and exit with status
//! 2, as `argparse` does. Ctrl-C prints `Interrupted` and exits with status 130. Text ends its
//! lines as Python's `print` ends them: `\r\n` on Windows, and `\n` elsewhere.

mod args;
mod artifacts;
mod deadline;
mod egress;
mod output;
mod sandbox;
mod session;

use std::ffi::OsString;

use self::args::{Parser, Receiver, Spec, Stop, Syntax};
use crate::host::platform;
use crate::host::python::builtins::repr;

/// The name of the program in usage messages unless `--prog` names another.
const PROGRAM: &str = "nvx-rs";
/// The commands, in the order in which usage messages list them.
const COMMANDS: [&str; 3] = ["artifacts", "egress", "sandbox"];
/// The usage and help of the top-level parser.
const SYNTAX: Syntax = Syntax {
    command: "",
    optionals: &["[-h]", "[--root PATH]", "[--prog PROG]"],
    positionals: &["{artifacts,egress,sandbox} ..."],
    help: "\
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
                        artifacts to use (default: the release or checkout
                        that holds this executable)
  --prog PROG           the program name that usage and error messages show,
                        such as nvx.py, which runs nvx-rs for its commands
                        (default: nvx-rs)

environment:
  NVX_OPENVMM           the OpenVMM executable to run instead of the one of the
                        artifact root",
};

/// How a command ends other than by succeeding.
#[derive(Debug)]
pub(crate) enum Exit {
    /// `error: MESSAGE` on standard error, and status 1: what `nvx.py` does for a `ScriptError`,
    /// an `OSError`, a `RuntimeError`, or a `ValueError`.
    Error(String),
    /// OpenVMM closed the control endpoint, as it does when it shuts down: Python's
    /// `ControlEndpointClosed`, which `nvx.py` reports as any other error.
    EndpointClosed,
    /// A usage error of the parser `prog`: its usage, then `PROG: error: MESSAGE`, and status 2.
    Usage {
        /// The parser's usage.
        usage: String,
        /// The parser's name, such as `nvx-rs sandbox`.
        prog: String,
        /// The message.
        message: String,
    },
    /// Help on standard output, and status 0.
    Help(String),
    /// Ctrl-C: `Interrupted` on standard error, and status 130.
    Interrupted,
    /// `SystemExit(status)`: the status of OpenVMM or of a managed workload.
    Status(i32),
}

/// The message of [`Exit::EndpointClosed`], `str(ControlEndpointClosed())`.
pub(crate) const ENDPOINT_CLOSED: &str = "managed control endpoint closed";

impl Exit {
    /// Returns the error that prints `message`.
    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    /// Returns the usage error `message` of the parser of `syntax` for the program `program`.
    pub(crate) fn usage(syntax: &Syntax, program: &str, message: String) -> Self {
        Self::Usage {
            usage: syntax.usage(program),
            prog: syntax.prog(program),
            message,
        }
    }

    /// Returns the end of a parse that `stop` ended, for the parser of `syntax` and the program
    /// `program`.
    pub(crate) fn from_stop(stop: Stop, syntax: &Syntax, program: &str) -> Self {
        match stop {
            Stop::Usage(message) => Self::usage(syntax, program, message),
            Stop::Help => Self::Help(syntax.help(program)),
        }
    }

    /// Returns the message that the end prints after `error: `, if it is an error.
    pub(crate) fn message(&self) -> Option<&str> {
        match self {
            Self::Error(message) => Some(message),
            Self::EndpointClosed => Some(ENDPOINT_CLOSED),
            _ => None,
        }
    }

    /// Reports the end on the standard streams and returns the exit status.
    fn report(self) -> i32 {
        match self {
            Self::Error(_) | Self::EndpointClosed => {
                output::print_stderr(&format!("error: {}", self.message().unwrap_or_default()));
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
            Self::Help(text) => match output::print_stdout(&text) {
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

/// Fails with [`Exit::Interrupted`] if Ctrl-C arrived.
pub(crate) fn check_interrupt() -> Result {
    if platform::interrupted() {
        Err(Exit::Interrupted)
    } else {
        Ok(())
    }
}

/// Runs `nvx-rs` with the process's arguments and returns its exit status.
///
/// The CLI runs on the calling thread, which should be the main thread of the process, and from
/// here on, Ctrl-C ends the command that runs, with status 130, instead of the process.
pub fn main() -> i32 {
    // Like Python, which installs its handler as it starts, run on without one if that fails.
    let _ = platform::watch_interrupts();
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let result = run(&arguments);
    // Python raises KeyboardInterrupt wherever Ctrl-C finds its command, so a command that Ctrl-C
    // reached ends as interrupted even if it finished meanwhile, as a run does whose OpenVMM the
    // same Ctrl-C ended.
    if platform::interrupted() {
        return Exit::Interrupted.report();
    }
    match result {
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
    /// `--prog`, if given.
    prog: Option<String>,
}

impl Receiver for TopLevel {
    fn take(&mut self, name: &'static str, value: Option<&str>) -> std::result::Result<(), String> {
        let value = value.map(str::to_owned);
        match name {
            "--root" => self.root = value,
            _ => self.prog = value,
        }
        Ok(())
    }
}

/// Parses the arguments, reports the unrecognized ones, and runs the command.
fn run(arguments: &[OsString]) -> Result {
    let mut strings = Vec::with_capacity(arguments.len());
    for (index, argument) in arguments.iter().enumerate() {
        let text = argument.to_str().ok_or_else(|| {
            Exit::usage(
                &SYNTAX,
                PROGRAM,
                format!(
                    "argument {} is not valid Unicode: {}",
                    index + 1,
                    repr(&argument.to_string_lossy())
                ),
            )
        })?;
        strings.push(text.to_owned());
    }
    let parser = Parser::new(
        &[Spec::one("--root"), Spec::one("--prog")],
        &[Spec::one("command")],
    );
    let mut top = TopLevel::default();
    let split = parser.split_command(&strings, &mut top);
    // A usage error names the program that the options before it named.
    let program = top.prog.clone().unwrap_or_else(|| PROGRAM.to_owned());
    let usage = |message: String| Exit::usage(&SYNTAX, &program, message);
    let (index, mut extras) = split.map_err(|stop| Exit::from_stop(stop, &SYNTAX, &program))?;
    let rest = &strings[index + 1..];
    let (command, unrecognized) = match strings[index].as_str() {
        "artifacts" => (Command::Artifacts, artifacts::parse(rest, &program)?),
        "egress" => {
            let (path, unrecognized) = egress::parse(rest, &program)?;
            (Command::EgressCompile(path), unrecognized)
        }
        "sandbox" => {
            let (options, unrecognized) = sandbox::parse(rest, &program)?;
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
