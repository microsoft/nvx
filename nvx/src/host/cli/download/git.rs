//! The history of a checkout, which `git` reads.
//!
//! `git` reads the history of the repository at the checkout's root alone, never of one in a
//! directory above it, so a checkout whose `.git` is not a repository fails instead of reading the
//! history of another repository that holds it. `git` runs in the root's physical location, with
//! links and `..` components resolved, and `GIT_CEILING_DIRECTORIES` stops its search at that
//! location's parent: `git` measures the bound against its physical working directory, so the
//! parent of a root written through a link or with `..` would let it climb above the checkout.
//!
//! Git splits `GIT_CEILING_DIRECTORIES` at every [`LIST_SEPARATOR`], `:`, or `;` on Windows,
//! without quoting, so the variable cannot name a parent whose path holds one, and `git` may climb
//! above a root below such a parent. There, `git rev-parse` must find the root at the top of a
//! work tree before the reader reads any history.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use super::History;
use crate::ErrorCode;
use crate::host::cli::{Exit, Result, failure_detail, io_error};
use crate::host::paths;

/// The byte at which Git splits `GIT_CEILING_DIRECTORIES`, its separator of paths in a list.
const LIST_SEPARATOR: u8 = if cfg!(windows) { b';' } else { b':' };

/// The `git` of a checkout.
pub(super) struct Git {
    /// The physical location of the root of the checkout, with links and `..` components resolved.
    root: PathBuf,
}

impl Git {
    /// Returns the `git` of the checkout at `root`, an existing directory, at the physical
    /// location of `root`.
    pub(super) fn new(root: &Path) -> Result<Self> {
        let root = paths::resolve(root).map_err(|error| io_error(ErrorCode::Io, &error, root))?;
        Ok(Self { root })
    }

    /// Runs `git ARGUMENTS` in the checkout, and returns its output.
    fn run(&self, arguments: &[&str]) -> std::io::Result<Output> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.root)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE");
        if let Some(parent) = self.root.parent()
            && !self.unbounded()
        {
            command.env("GIT_CEILING_DIRECTORIES", parent);
        }
        command.spawn()?.wait_with_output()
    }

    /// Returns whether `GIT_CEILING_DIRECTORIES` cannot stop `git`'s search at the root's parent,
    /// whose path holds a [`LIST_SEPARATOR`].
    fn unbounded(&self) -> bool {
        self.root.parent().is_some_and(|parent| {
            parent
                .as_os_str()
                .as_encoded_bytes()
                .contains(&LIST_SEPARATOR)
        })
    }

    /// Returns why the reader must not read the history that `git` finds, if its search is
    /// [unbounded](Self::unbounded): `git rev-parse --is-inside-work-tree --show-prefix` fails, as
    /// for a root that is no repository and has none above it, or does not find the root at the
    /// top of a work tree, as when the root lies in the work tree of a repository above it.
    fn foreign_repository(&self) -> std::io::Result<Option<String>> {
        if !self.unbounded() {
            return Ok(None);
        }
        let output = self.run(&["rev-parse", "--is-inside-work-tree", "--show-prefix"])?;
        if !output.status.success() {
            return Ok(Some(failure_detail("git rev-parse", &output)));
        }
        // At the top of a work tree, `git` prints `true` and an empty prefix, each on a line of its
        // own, which compares no path, so a name that ends in a line end cannot confuse it.
        Ok((output.stdout != b"true\n\n").then(|| {
            format!(
                "{} is not the root of a Git repository, and git would read the history of the \
                 repository above it",
                self.root.display()
            )
        }))
    }
}

impl History for Git {
    fn first_parent_commits(&self, limit: usize) -> Result<Vec<String>> {
        let unavailable = |detail: &str| {
            Exit::error(
                ErrorCode::Unavailable,
                format!("download needs a Git checkout to find its release: {detail}"),
            )
        };
        let cannot_run = |error: std::io::Error| unavailable(&format!("cannot run git: {error}"));
        if let Some(problem) = self.foreign_repository().map_err(cannot_run)? {
            return Err(unavailable(&problem));
        }
        let output = self
            .run(&[
                "rev-list",
                "--first-parent",
                &format!("--max-count={limit}"),
                "HEAD",
            ])
            .map_err(cannot_run)?;
        if !output.status.success() {
            return Err(unavailable(&failure_detail("git rev-list", &output)));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(str::to_owned)
            .collect())
    }

    fn version_at(&self, commit: &str) -> Result<Option<String>> {
        let unreadable = |error: std::io::Error| {
            Exit::error(
                ErrorCode::Unavailable,
                format!("download failed to read VERSION at {commit}: {error}"),
            )
        };
        if self.foreign_repository().map_err(unreadable)?.is_some() {
            return Ok(None);
        }
        let output = self
            .run(&["show", &format!("{commit}:VERSION")])
            .map_err(unreadable)?;
        if !output.status.success() {
            return Ok(None);
        }
        let version = String::from_utf8(output.stdout).map_err(|_| {
            Exit::error(
                ErrorCode::Unavailable,
                format!("the VERSION of {commit} is not UTF-8"),
            )
        })?;
        let version = version.trim();
        Ok((!version.is_empty()).then(|| version.to_owned()))
    }
}

#[cfg(test)]
mod tests;
