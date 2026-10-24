//! The host program's step hook, for tests: at a named [`Step`] of a launch, it pauses the
//! program, so that a test can act between two steps, as when it replaces a staged image once the
//! launch has checked it, or it exits the program without unwinding, as a crash would end it there.
//!
//! The variable [`STEP_HOOK`] names the step, and what the hook does at it, its [`Action`]:
//!
//! - `STEP:pause:PATH`: the program creates the file PATH, which holds the step's name, and waits
//!   until the file is gone, then goes on, and leaves the time that it waited out of its deadlines,
//!   so that a test may pause it for as long as it needs;
//! - `STEP:exit`: the program exits at once with the status [`HOOK_EXIT`], without unwinding: it
//!   ends nothing that it started, such as the VMM, and nothing that it holds is released but what
//!   the system releases when a process exits, such as its descriptors or handles, and their
//!   locks.
//!
//! Without the variable, the hook does nothing. `runtime-adapter-host launch` reads it, and refuses
//! a value that names no step or action, as it refuses a command line that is not a command.

use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process;
use std::thread;
use std::time::{Duration, Instant};

/// The variable that sets the step hook.
pub const STEP_HOOK: &str = "RUNTIME_ADAPTER_STEP_HOOK";
/// The status with which the step hook exits the program.
pub const HOOK_EXIT: u8 = 5;
/// How often a paused program checks whether its pause file is gone.
const PAUSE_POLL: Duration = Duration::from_millis(10);

/// A named step of a launch, at which the step hook acts: right after what the launch has done by
/// then, and before what it does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// `checked`: the launch has checked the bindings of its images, and holds the checked files,
    /// which claim the scratch image, but has begun nothing in the sandbox's store, and has spawned
    /// nothing. A launch without a layered root, which checks nothing, reaches it too.
    Checked,
    /// `spawned`: the VMM runs, and the launch's journal records it, but the launch has not
    /// connected to the VM.
    Spawned,
    /// `ready`: the guest has assembled the layered root, and the launch has released the checked
    /// files. A launch without a layered root never reaches it.
    Ready,
    /// `commit`: the guest agent has answered, and the launch is about to commit, so its journal
    /// still records the VMM, which no runtime record names yet.
    Commit,
}

impl Step {
    /// The steps, in the order in which a launch reaches them.
    pub const ALL: [Self; 4] = [Self::Checked, Self::Spawned, Self::Ready, Self::Commit];

    /// Returns the step's name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Checked => "checked",
            Self::Spawned => "spawned",
            Self::Ready => "ready",
            Self::Commit => "commit",
        }
    }

    /// Returns the step named `name`, if any.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|step| step.as_str() == name)
    }
}

impl fmt::Display for Step {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What the step hook does at its step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Create the file at this path, with the step's name, and wait until it is gone.
    Pause(PathBuf),
    /// Exit at once with [`HOOK_EXIT`], without unwinding.
    Exit,
}

/// The step hook: the step at which it acts, and what it does there.
///
/// Its [`Display`](fmt::Display) form is the value of [`STEP_HOOK`] that sets it, which
/// [`parse`](Self::parse) reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepHook {
    /// The step at which the hook acts.
    pub step: Step,
    /// What the hook does at its step.
    pub action: Action,
}

impl StepHook {
    /// Returns the hook that `value`, a value of [`STEP_HOOK`], sets: `STEP:pause:PATH` or
    /// `STEP:exit`, where STEP is the name of a [`Step`], and PATH, which may hold colons, is not
    /// empty. Otherwise, says why it sets none.
    pub fn parse(value: &OsStr) -> Result<Self, String> {
        let text = value
            .to_str()
            .ok_or_else(|| format!("{STEP_HOOK} is not Unicode text: {value:?}"))?;
        let mut parts = text.splitn(3, ':');
        let name = parts.next().unwrap_or_default();
        let step = Step::parse(name).ok_or_else(|| {
            let names = Step::ALL.map(Step::as_str).join(", ");
            format!("{STEP_HOOK} names the step {name:?}, not one of {names}")
        })?;
        let action = match (parts.next(), parts.next()) {
            (Some("pause"), Some(path)) if !path.is_empty() => Action::Pause(PathBuf::from(path)),
            (Some("exit"), None) => Action::Exit,
            _ => {
                return Err(format!(
                    "{STEP_HOOK} takes STEP:pause:PATH or STEP:exit, not {text:?}"
                ));
            }
        };
        Ok(Self { step, action })
    }

    /// Returns the hook that [`STEP_HOOK`] sets, if it is set, or says why its value sets none.
    pub fn from_env() -> Result<Option<Self>, String> {
        env::var_os(STEP_HOOK)
            .map(|value| Self::parse(&value))
            .transpose()
    }

    /// Acts at `step`, if it is the hook's: exits the program, or creates the pause file, with the
    /// step's name, and waits until it is gone. Does nothing at another step.
    ///
    /// Returns how long the hook paused the program, which is zero at another step, so that the
    /// program can leave the pause out of its deadlines.
    ///
    /// Fails if the pause file cannot be created, as when it exists already, or written, or if
    /// whether it is gone cannot be told.
    pub fn at(&self, step: Step) -> io::Result<Duration> {
        if step != self.step {
            return Ok(Duration::ZERO);
        }
        match &self.action {
            Action::Exit => process::exit(HOOK_EXIT.into()),
            Action::Pause(path) => {
                let paused = Instant::now();
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?
                    .write_all(step.as_str().as_bytes())?;
                while path.try_exists()? {
                    thread::sleep(PAUSE_POLL);
                }
                Ok(paused.elapsed())
            }
        }
    }
}

impl fmt::Display for StepHook {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.action {
            Action::Pause(path) => write!(formatter, "{}:pause:{}", self.step, path.display()),
            Action::Exit => write!(formatter, "{}:exit", self.step),
        }
    }
}
