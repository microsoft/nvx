//! The CLI's own store of a sandbox: its private state directory, the fixed paths of the files
//! that the sandbox commands keep in it, and the lock that serializes their lifecycle
//! transitions.
//!
//! ```text
//! STATE/                  one sandbox, private to the current user
//!   config.json           the configuration of the provisioned sandbox
//!   sandbox.lock          the lock that serializes its lifecycle transitions
//!   launch.json           the journal of a start in progress, or of one that was interrupted
//!   runtime.json          the runtime record of its running VM
//!   control.capability    the control capability of its VM
//!   control.sock          the control endpoint of its VM, on Linux
//!   openvmm.log           OpenVMM's output
//!   outcome.json          OpenVMM's outcome report of the latest start
//! ```
//!
//! The files have the names under which state directories have always kept them, and the lock
//! is the lock of the same file that `nvx`'s sandbox store locks, so that a transition that takes
//! one excludes a transition that takes the other.
//!
//! The directory is private to the current user, since its records decide which process a later
//! command signals. On Linux, it belongs to the user with mode 0700, which OpenVMM also requires
//! of the directory of its control socket, and no one else may reach the files in it. On Windows,
//! a directory that the store creates has a protected access control list that allows the user
//! alone, which the files created in it inherit; one that it did not create may also allow SYSTEM
//! and the Administrators group, as one under the user's profile does. An existing directory that
//! others may access is refused rather than restricted, since they could have placed files in it,
//! or still hold them open. The store creates its files readable only by the current user.
//!
//! A locked launch writes its prepared capability and outputs before spawn, records the spawned
//! process durably, and publishes the caller's exact saved VM only at commit. Dropping an
//! uncommitted launch keeps its journal; these transitions do not recover or signal a process.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "SL3.5b switches the sandbox lifecycle commands to the CLI's own store"
    )
)]

mod files;
mod launch;
mod locked;
mod names;
mod record;

use std::io;
use std::path::{Path, PathBuf};

pub(crate) use self::locked::LockedState;
pub(crate) use self::names::StateFile;
use crate::ErrorCode;
use crate::host::cli::{Exit, Result};
use crate::host::platform;

/// Builds an error mapper for a failure to `context`, such as `cannot lock PATH`, that keeps the
/// I/O error that caused it: with the code [`ErrorCode::NotFound`] when a path does not exist, and
/// [`ErrorCode::Io`] otherwise.
fn failed(context: String) -> impl FnOnce(io::Error) -> Exit {
    move |error| {
        let code = match error.kind() {
            io::ErrorKind::NotFound => ErrorCode::NotFound,
            _ => ErrorCode::Io,
        };
        Exit::error(code, format!("{context}: {error}"))
    }
}

/// A sandbox's state directory, private to the current user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StateDir {
    /// The directory, as the command resolved it.
    dir: PathBuf,
}

impl StateDir {
    /// Creates the directory `dir`, whose parent must exist, private to the current user, or
    /// opens it if it exists already, which then must be private, as [`open`](Self::open)
    /// requires.
    pub(crate) fn create(dir: &Path) -> Result<Self> {
        let problem = platform::create_store_dir(dir).map_err(failed(format!(
            "cannot create the sandbox store {}",
            dir.display()
        )))?;
        Self::private(dir, problem)
    }

    /// Opens the existing directory `dir`, which must be private to the current user, as
    /// [`create`](Self::create) makes it. Fails with [`ErrorCode::NotFound`] if it does not exist.
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        let problem = platform::check_store_dir(dir).map_err(failed(format!(
            "cannot open the sandbox store {}",
            dir.display()
        )))?;
        Self::private(dir, problem)
    }

    /// Returns the state directory `dir`, unless `problem` says why it is not private to the
    /// current user.
    fn private(dir: &Path, problem: Option<String>) -> Result<Self> {
        match problem {
            None => Ok(Self {
                dir: dir.to_path_buf(),
            }),
            Some(problem) => Err(Exit::error(
                ErrorCode::Io,
                format!("the sandbox store {} {problem}", dir.display()),
            )),
        }
    }

    /// Returns the directory.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the path of `file` in the directory.
    pub(crate) fn path(&self, file: StateFile) -> PathBuf {
        self.dir.join(file.name())
    }
}

#[cfg(test)]
mod tests;
