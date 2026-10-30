//! The history of a checkout, which `git` reads.
//!
//! `git` looks for the repository at the checkout's root alone, never in a directory above it,
//! so a checkout whose `.git` is not a repository fails instead of reading the history of another
//! repository that holds it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use super::History;
use crate::ErrorCode;
use crate::host::cli::{Exit, Result};
use crate::host::supervisor::spawn_command;

/// The `git` of a checkout.
pub(super) struct Git {
    /// The root of the checkout.
    root: PathBuf,
}

impl Git {
    /// Returns the `git` of the checkout at `root`.
    pub(super) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
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
        spawn_command(&mut command)?.wait_with_output()
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
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(match stderr.trim() {
                "" => unavailable(&format!("git rev-list exited with {}", output.status)),
                detail => unavailable(detail),
            });
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
