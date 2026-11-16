//! The `nvx-rs` command-line interface, which runs NVX's microVMs and sandboxes.
//!
//! ```text
//! nvx-rs [-h] [--root PATH] artifacts
//! nvx-rs [-h] [--root PATH] download [--repository OWNER/REPOSITORY] [--hypervisor HYPERVISOR]
//! nvx-rs [-h] [--root PATH] egress compile PATH
//! nvx-rs [-h] [--root PATH] run [OPTIONS]
//! nvx-rs [-h] [--root PATH] sandbox [run|provision|start|exec|stop|deprovision] [OPTIONS]
//! nvx-rs [-h] [--root PATH] snapshot capture NAME [OPTIONS]
//! nvx-rs [-h] [--root PATH] snapshot restore NAME [OPTIONS]
//! ```
//!
//! - `artifacts` prints the paths of the artifacts of the artifact root as JSON.
//! - `download` installs the release of a repository checkout, built from its `HEAD` or the
//!   nearest released first-parent ancestor, with the options, checks, and outputs of the
//!   `nvx.py download` that it replaced; it leaves `nvx-rs` itself alone.
//! - `egress compile PATH` compiles an egress policy file of `--network-egress-policy-file` and
//!   prints its native rules as JSON.
//! - `run` boots a microVM on the terminal, with the options, checks, outputs, and exit statuses
//!   of the `nvx.py run` that it replaced, or restores a named snapshot once, as
//!   `snapshot restore` restores one.
//! - `sandbox` runs or manages a sandbox, with the options, outputs, exit statuses, outcome
//!   reports, and state directories of the `nvx.py sandbox` that it replaced.
//! - `snapshot capture` captures a blockless VM into an immutable named snapshot, and
//!   `snapshot restore` starts a new managed sandbox from one.
//!
//! The artifact root is `--root`, or else the release or repository checkout that holds the
//! executable, and the `NVX_OPENVMM` environment variable names another OpenVMM executable.
//! `run`, `sandbox run`, and `sandbox start` download the release of a Git checkout that holds
//! none of its artifacts, as `download` does, unless the `NVX_AUTO_DOWNLOAD` environment variable
//! is `0`.
//!
//! Errors print `error[CODE]: MESSAGE` to standard error and exit with status 1, as `nvx.py`
//! reports a `ScriptError`, with the stable `ErrorCode` of the failure. Usage errors print the
//! one-line usage of the parser and `PROG: error[CODE]: MESSAGE`, where `PROG` is the parser's
//! name, such as `nvx-rs run`, with one of the `argument-*` codes, and exit with status 2, as
//! `argparse` does. Help prints the usage and every option of the parser, and exits with status 0.
//! Each error is one line: a line break in a value that its message names, such as a path, prints
//! as `\n` or `\r`. Ctrl-C prints `Interrupted` and exits with status 130. Text ends its lines as
//! Python's `print` ends them: `\r\n` on Windows, and `\n` elsewhere.

mod args;
mod artifacts;
mod cpu_profile;
mod deadline;
mod download;
mod egress;
mod exec_lock;
mod json;
mod network;
mod output;
mod run;
mod sandbox;
mod session;
mod shell;
mod snapshots;
mod store;

use std::borrow::Cow;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use self::args::{Parser, Receiver, Spec, Stop, Syntax};
use crate::ErrorCode;
use crate::host::platform;

/// Returns the name of a temporary file next to `path`: `.NAME.UUID.tmp`, with a fresh UUID4 in
/// hexadecimal.
pub(crate) fn temporary_path(path: &Path) -> Result<PathBuf> {
    let Some(name) = path.file_name() else {
        return Err(Exit::error(
            ErrorCode::Other,
            format!("{} has an empty name", path.display()),
        ));
    };
    let mut temporary = OsString::from(".");
    temporary.push(name);
    temporary.push(format!(".{}.tmp", uuid4_hex()?));
    Ok(path.with_file_name(temporary))
}

/// Returns 32 lowercase hexadecimal digits of 122 random bits, with the version and variant bits
/// of a version-4 UUID.
fn uuid4_hex() -> Result<String> {
    let mut bytes = random_bytes::<16>()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(hex(&bytes))
}

/// Returns `N` random bytes from the operating system.
pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|error| {
        Exit::error(ErrorCode::Io, format!("cannot read random bytes: {error}"))
    })?;
    Ok(bytes)
}

/// Returns `bytes` in lowercase hexadecimal.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The commands, in the order in which usage messages list them.
const COMMANDS: [&str; 6] = [
    "artifacts",
    "download",
    "egress",
    "run",
    "sandbox",
    "snapshot",
];
/// The options of the top-level parser.
const OPTIONS: [Spec; 1] = [Spec::one("--root")];
/// The usage and help of the top-level parser.
const SYNTAX: Syntax = Syntax {
    command: "",
    arguments: "[-h] [--root PATH] {artifacts,download,egress,run,sandbox,snapshot} ...",
    help: "\
Run NVX microVMs and sandboxes, download and look up NVX artifacts, and compile
egress policies.

positional arguments:
  {artifacts,download,egress,run,sandbox,snapshot}
    artifacts           print the paths of the NVX artifacts as JSON
    download            download and install the NVX release of the checkout
    egress              compile egress policy files
    run                 run an OpenVMM microVM
    sandbox             run or manage workloads over EROFS layers and private
                        ext4 scratch
    snapshot            capture an immutable named snapshot, or restore a
                        managed sandbox from one

options:
  -h, --help            show this help message and exit
  --root PATH           the NVX repository checkout or extracted release whose
                        artifacts to use (default: the release or checkout
                        that holds this executable)

environment:
  NVX_OPENVMM           the OpenVMM executable to run instead of the one of the
                        artifact root
  NVX_AUTO_DOWNLOAD     0 keeps run and sandbox from downloading the release of
                        a Git checkout that holds no NVX artifacts (default: 1)",
};

/// How a command ends other than by succeeding.
#[derive(Debug)]
pub(crate) enum Exit {
    /// `error[CODE]: MESSAGE` on standard error, and status 1: what `nvx.py` does for a
    /// `ScriptError`, an `OSError`, a `RuntimeError`, or a `ValueError`.
    Error {
        /// The code of the failure.
        code: ErrorCode,
        /// The message.
        message: String,
    },
    /// OpenVMM closed the control endpoint, as it does when it shuts down: Python's
    /// `ControlEndpointClosed`, which `nvx.py` reports as any other error, and whose code is
    /// [`ErrorCode::EndpointClosed`].
    EndpointClosed,
    /// A usage error of the parser `prog`: its usage, then `PROG: error[CODE]: MESSAGE`, and
    /// status 2.
    Usage {
        /// The parser's usage.
        usage: String,
        /// The parser's name, such as `nvx-rs sandbox`.
        prog: String,
        /// The code of the usage error, one of the `argument-*` codes.
        code: ErrorCode,
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
    /// Returns the error of `code` that prints `message`.
    pub(crate) fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            code,
            message: message.into(),
        }
    }

    /// Returns the usage error `message` of `code` of the parser of `syntax`.
    pub(crate) fn usage(syntax: &Syntax, code: ErrorCode, message: String) -> Self {
        Self::Usage {
            usage: syntax.usage(),
            prog: syntax.prog(),
            code,
            message,
        }
    }

    /// Returns the end of a parse that `stop` ended, for the parser of `syntax`.
    pub(crate) fn from_stop(stop: Stop, syntax: &Syntax) -> Self {
        match stop {
            Stop::Usage(code, message) => Self::usage(syntax, code, message),
            Stop::Help => Self::Help(syntax.help()),
        }
    }

    /// Returns the error of an I/O error that names no file, which prints the error as Rust
    /// writes it, such as `Broken pipe (os error 32)` for a write to a standard stream whose
    /// reader has gone, with the code [`ErrorCode::Io`].
    pub(crate) fn from_os_error(error: std::io::Error) -> Self {
        Self::error(ErrorCode::Io, error.to_string())
    }

    /// Returns the message that the end prints after its code, if it is an error.
    pub(crate) fn message(&self) -> Option<&str> {
        match self {
            Self::Error { message, .. } | Self::Usage { message, .. } => Some(message),
            Self::EndpointClosed => Some(ENDPOINT_CLOSED),
            _ => None,
        }
    }

    /// Returns the code that the end prints, if it is an error or a usage error.
    pub(crate) fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Error { code, .. } | Self::Usage { code, .. } => Some(*code),
            Self::EndpointClosed => Some(ErrorCode::EndpointClosed),
            _ => None,
        }
    }

    /// Reports the end on the standard streams and returns the exit status.
    fn report(self) -> i32 {
        match self {
            Self::Error { .. } | Self::EndpointClosed => {
                output::print_stderr(&format!(
                    "error[{}]: {}",
                    self.code().unwrap_or(ErrorCode::Other),
                    one_line(self.message().unwrap_or_default())
                ));
                1
            }
            Self::Usage {
                usage,
                prog,
                code,
                message,
            } => {
                output::print_stderr(&format!(
                    "{usage}\n{prog}: error[{code}]: {}",
                    one_line(&message)
                ));
                2
            }
            Self::Help(text) => match output::print_stdout(&text) {
                Ok(()) => 0,
                // Ctrl-C ends a help that waits for its reader, as `KeyboardInterrupt` ends it.
                Err(_) if platform::interrupted() => Self::Interrupted.report(),
                Err(error) => Self::from_os_error(error).report(),
            },
            Self::Interrupted => {
                output::print_stderr("Interrupted");
                130
            }
            Self::Status(status) => status,
        }
    }
}

/// Keeps the code and the message of a library error.
impl From<crate::Error> for Exit {
    fn from(error: crate::Error) -> Self {
        Self::error(error.code(), error.message())
    }
}

/// Returns `message` as one line: a line break in it, which only a value that it names holds, such
/// as a path, prints as `\r` or `\n`, so that each error is the one line that callers look for,
/// and no line of a message reads as another error.
fn one_line(message: &str) -> Cow<'_, str> {
    if message.contains(['\r', '\n']) {
        Cow::Owned(message.replace('\r', "\\r").replace('\n', "\\n"))
    } else {
        Cow::Borrowed(message)
    }
}

/// The result of a command.
pub(crate) type Result<T = ()> = std::result::Result<T, Exit>;

/// Returns the message of the I/O error that an operation on `path` met, as the sandbox launch
/// profile writes its own: the path, quoted with Rust's debug formatting, and the error as Rust
/// writes it, such as `"/state/config.json": Permission denied (os error 13)`.
pub(crate) fn io_message(error: &std::io::Error, path: &Path) -> String {
    format!("{path:?}: {error}")
}

/// Returns the error of `code` for the I/O error that an operation on `path` met, which prints
/// [`io_message`].
pub(crate) fn io_error(code: ErrorCode, error: &std::io::Error, path: &Path) -> Exit {
    Exit::error(code, io_message(error, path))
}

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
    /// `download ...`.
    Download(download::Options),
    /// `egress compile PATH`.
    EgressCompile(String),
    /// `run ...`.
    Run(Box<run::Options>),
    /// `sandbox ...`.
    Sandbox(Box<sandbox::Options>),
    /// `snapshot capture ...` or `snapshot restore ...`.
    Snapshot(snapshots::Command),
}

/// The top-level options.
#[derive(Default)]
struct TopLevel {
    /// `--root`, if given.
    root: Option<String>,
}

impl Receiver for TopLevel {
    fn take(&mut self, _: &'static str, value: Option<&str>) -> std::result::Result<(), String> {
        // `--root` is the only option.
        self.root = value.map(str::to_owned);
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
                ErrorCode::ArgumentInvalid,
                format!("argument {} is not valid Unicode: {argument:?}", index + 1),
            )
        })?;
        strings.push(text.to_owned());
    }
    let parser = Parser::new(&OPTIONS, &[Spec::one("command")]);
    let mut top = TopLevel::default();
    let usage = |code, message: String| Exit::usage(&SYNTAX, code, message);
    let (index, mut extras) = parser
        .split_command(&strings, &mut top)
        .map_err(|stop| Exit::from_stop(stop, &SYNTAX))?;
    let rest = &strings[index + 1..];
    let (command, unrecognized) = match strings[index].as_str() {
        "artifacts" => (Command::Artifacts, artifacts::parse(rest)?),
        "download" => {
            let (options, unrecognized) = download::parse(rest)?;
            (Command::Download(options), unrecognized)
        }
        "egress" => {
            let (path, unrecognized) = egress::parse(rest)?;
            (Command::EgressCompile(path), unrecognized)
        }
        "run" => {
            let (options, unrecognized) = run::parse(rest)?;
            (Command::Run(Box::new(options)), unrecognized)
        }
        "sandbox" => {
            let (options, unrecognized) = sandbox::parse(rest)?;
            (Command::Sandbox(Box::new(options)), unrecognized)
        }
        "snapshot" => {
            let (command, unrecognized) = snapshots::parse(rest)?;
            (Command::Snapshot(command), unrecognized)
        }
        other => {
            return Err(usage(
                ErrorCode::ArgumentInvalid,
                format!(
                    "argument command: {}",
                    args::invalid_choice(other, &COMMANDS)
                ),
            ));
        }
    };
    extras.extend(unrecognized);
    if !extras.is_empty() {
        return Err(usage(
            ErrorCode::ArgumentUnrecognized,
            format!("unrecognized arguments: {}", extras.join(" ")),
        ));
    }
    let root = top.root.as_deref();
    match command {
        Command::Artifacts => artifacts::run(root),
        Command::Download(options) => download::run(root, &options),
        Command::EgressCompile(path) => egress::run(&path),
        Command::Run(options) => run::run(root, *options),
        Command::Sandbox(options) => sandbox::run(root, *options),
        Command::Snapshot(command) => snapshots::run(root, command),
    }
}

#[cfg(test)]
mod tests;
