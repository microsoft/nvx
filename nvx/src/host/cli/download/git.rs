//! The history of a checkout, which `git` reads.
//!
//! `git` looks for the repository at the checkout's root alone, never in a directory above it,
//! so a checkout whose `.git` is not a repository fails instead of reading the history of another
//! repository that holds it. `git` runs in the root's physical location, with links and `..`
//! components resolved, and its search stops at that location's parent: `git` measures the bound
//! against its physical working directory, so the parent of a root written through a link or with
//! `..` would let it climb above the checkout.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use super::History;
use crate::ErrorCode;
use crate::host::cli::{Exit, Result, failure_detail, io_error};
use crate::host::paths;

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
            && let Ok(ceiling) = std::env::join_paths([parent])
        {
            command.env("GIT_CEILING_DIRECTORIES", ceiling);
        }
        command.spawn()?.wait_with_output()
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
        let output = self
            .run(&[
                "rev-list",
                "--first-parent",
                &format!("--max-count={limit}"),
                "HEAD",
            ])
            .map_err(|error| unavailable(&format!("cannot run git: {error}")))?;
        if !output.status.success() {
            return Err(unavailable(&failure_detail("git rev-list", &output)));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(str::to_owned)
            .collect())
    }

    fn version_at(&self, commit: &str) -> Result<Option<String>> {
        let output = self
            .run(&["show", &format!("{commit}:VERSION")])
            .map_err(|error| {
                Exit::error(
                    ErrorCode::Unavailable,
                    format!("download failed to read VERSION at {commit}: {error}"),
                )
            })?;
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
