//! The package of a release: its members, which `tar` lists and extracts, and its `SHA256SUMS`.
//!
//! A package holds one root directory, whose `SHA256SUMS` lists the SHA-256 digest of each other
//! file of the package. Before `tar` extracts anything, the name of each member must be relative
//! and canonical, without `.` or `..` components, backslashes, colons, or control characters, and
//! below that one root. The extraction must then hold regular files and directories alone,
//! exactly the members and the directories that hold them, and each file must have the digest
//! that `SHA256SUMS` lists for it, as `verify_sha256_sums` of `nvx.py` checked a package.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use sha2::{Digest, Sha256};

use super::system_program;
use crate::ErrorCode;
use crate::host::artifacts::CHECKSUMS_NAME;
use crate::host::cli::{Exit, Result, check_interrupt, io_error};
use crate::host::supervisor::spawn_command;

/// The directory beside the package into which `tar` extracts it.
const EXTRACTION_DIR: &str = "package";

/// Extracts the package `archive` into a new directory beside it once the names of its members
/// pass, and returns the package's root directory there once its files pass their checksums.
pub(super) fn extract(archive: &Path) -> Result<PathBuf> {
    let (Some(directory), Some(name)) = (archive.parent(), archive.file_name()) else {
        return Err(invalid(format!(
            "release archive has no name: {}",
            archive.display()
        )));
    };
    let name = name.to_str().ok_or_else(|| {
        invalid(format!(
            "release archive name is not Unicode: {}",
            archive.display()
        ))
    })?;
    // Without the gzip flag, `tar` reads a ZIP archive, as the libarchive `tar` of Windows does.
    let gzip = name.ends_with(".tar.gz");
    let listed = tar(directory, &[if gzip { "-tzf" } else { "-tf" }, name])?;
    let (root, members) = members(&listed)?;
    let destination = directory.join(EXTRACTION_DIR);
    fs::create_dir(&destination).map_err(|error| io_error(ErrorCode::Io, &error, &destination))?;
    tar(
        directory,
        &[
            if gzip { "-xzf" } else { "-xf" },
            name,
            "-C",
            EXTRACTION_DIR,
        ],
    )?;
    let extracted = walk(&destination)?;
    check_extracted(&members, &extracted)?;
    if extracted.get(&root) != Some(&true) {
        return Err(invalid(
            "release archive package root must be a directory".to_owned(),
        ));
    }
    let prefix = format!("{root}/");
    let files: BTreeSet<&str> = extracted
        .iter()
        .filter(|(_, directory)| !**directory)
        .filter_map(|(path, _)| path.strip_prefix(&prefix))
        .collect();
    let package = destination.join(&root);
    verify_checksums(&package, &files)?;
    Ok(package)
}

/// Runs the host's `tar` with `arguments` in `directory`, and returns what it printed on its
/// standard output.
fn tar(directory: &Path, arguments: &[&str]) -> Result<String> {
    let program = system_program("tar");
    let mut command = Command::new(&program);
    command
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = spawn_command(&mut command)
        .and_then(Child::wait_with_output)
        .map_err(|error| {
            Exit::error(
                ErrorCode::Unavailable,
                format!(
                    "download needs tar, which did not run: {}: {error}",
                    program.display()
                ),
            )
        })?;
    check_interrupt()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = match stderr.trim() {
            "" => format!("tar exited with {}", output.status),
            text => text.to_owned(),
        };
        return Err(invalid(format!(
            "tar cannot read the release archive: {detail}"
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| invalid("release archive member names are not UTF-8".to_owned()))
}

/// Returns the one root directory of the members that `listing` names, one per line, and the
/// canonical name of each member: without the `/` that ends the name of a directory.
fn members(listing: &str) -> Result<(String, BTreeSet<String>)> {
    let mut members = BTreeSet::new();
    for name in listing.lines().filter(|line| !line.is_empty()) {
        let canonical = name.strip_suffix('/').unwrap_or(name);
        if !is_canonical(canonical) {
            return Err(invalid(format!("unsafe path in release archive: {name}")));
        }
        if !members.insert(canonical.to_owned()) {
            return Err(invalid(format!(
                "duplicate path in release archive: {canonical}"
            )));
        }
    }
    let roots: BTreeSet<&str> = members
        .iter()
        .filter_map(|member| member.split('/').next())
        .collect();
    let root = match roots.into_iter().collect::<Vec<_>>()[..] {
        [root] => root.to_owned(),
        _ => {
            return Err(invalid(
                "release archive must contain exactly one package root".to_owned(),
            ));
        }
    };
    Ok((root, members))
}

/// Returns whether `path` is relative and canonical: components that `/` separates, none of them
/// empty, `.`, or `..`, without backslashes, colons, or control characters.
fn is_canonical(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Returns each entry below `directory`, by its path relative to it with `/` separators, and
/// whether it is a directory, and fails for one that is neither a directory nor a regular file,
/// such as a symbolic link.
fn walk(directory: &Path) -> Result<BTreeMap<String, bool>> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![(directory.to_path_buf(), String::new())];
    while let Some((path, relative)) = pending.pop() {
        let failed = |error: io::Error| io_error(ErrorCode::Io, &error, &path);
        for entry in fs::read_dir(&path).map_err(failed)? {
            let entry = entry.map_err(failed)?;
            let name = entry.file_name().into_string().map_err(|name| {
                invalid(format!(
                    "release archive extracted a name that is not Unicode: {name:?}"
                ))
            })?;
            let child = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            let file_type = entry.file_type().map_err(failed)?;
            if file_type.is_symlink() {
                return Err(invalid(format!(
                    "release archive extracted a symlink: {child}"
                )));
            } else if file_type.is_dir() {
                entries.insert(child.clone(), true);
                pending.push((entry.path(), child));
            } else if file_type.is_file() {
                entries.insert(child, false);
            } else {
                return Err(invalid(format!(
                    "release archive extracted a special file: {child}"
                )));
            }
        }
    }
    Ok(entries)
}

/// Checks that `extracted` holds each of the `members` that the archive lists, and nothing else
/// but the directories that hold them.
fn check_extracted(members: &BTreeSet<String>, extracted: &BTreeMap<String, bool>) -> Result {
    if let Some(missing) = members
        .iter()
        .find(|member| !extracted.contains_key(*member))
    {
        return Err(invalid(format!(
            "release archive did not extract {missing}"
        )));
    }
    let parents: BTreeSet<&str> = members
        .iter()
        .flat_map(|member| member.match_indices('/').map(|(index, _)| &member[..index]))
        .collect();
    if let Some((unlisted, _)) = extracted.iter().find(|(path, directory)| {
        !(members.contains(*path) || (**directory && parents.contains(path.as_str())))
    }) {
        return Err(invalid(format!(
            "release archive extracted an unlisted path: {unlisted}"
        )));
    }
    Ok(())
}

/// Checks the files of the package at `package`, `files` by their paths in it, against its
/// `SHA256SUMS`, as `verify_sha256_sums` of `nvx.py` did: the list names each file but itself once,
/// with its digest, and nothing else.
fn verify_checksums(package: &Path, files: &BTreeSet<&str>) -> Result {
    let path = package.join(CHECKSUMS_NAME);
    if !files.contains(CHECKSUMS_NAME) {
        return Err(invalid(format!(
            "release package has no {CHECKSUMS_NAME}: {}",
            path.display()
        )));
    }
    let bytes = fs::read(&path).map_err(|error| io_error(ErrorCode::Io, &error, &path))?;
    let text = std::str::from_utf8(&bytes)
        .ok()
        .filter(|text| text.is_ascii())
        .ok_or_else(|| {
            invalid(format!(
                "release checksums must contain only ASCII text: {}",
                path.display()
            ))
        })?;
    let mut listed = BTreeSet::new();
    for line in text.lines() {
        check_interrupt()?;
        let malformed = || {
            invalid(format!(
                "malformed checksum line in {}: {line}",
                path.display()
            ))
        };
        let (expected, relative) = line.split_once("  ").ok_or_else(malformed)?;
        if !is_digest(expected) || !is_canonical(relative) || relative == CHECKSUMS_NAME {
            return Err(malformed());
        }
        if !listed.insert(relative) {
            return Err(invalid(format!(
                "duplicate checksum path in {}: {relative}",
                path.display()
            )));
        }
        if !files.contains(relative) {
            return Err(invalid(format!(
                "invalid checksum path in {}: {relative}",
                path.display()
            )));
        }
        let file = relative
            .split('/')
            .fold(package.to_path_buf(), |path, part| path.join(part));
        let actual = sha256_file(&file)?;
        if actual != expected {
            return Err(invalid(format!(
                "checksum mismatch for {relative}: {actual}, expected {expected}"
            )));
        }
    }
    if let Some(unlisted) = files
        .iter()
        .find(|file| **file != CHECKSUMS_NAME && !listed.contains(**file))
    {
        return Err(invalid(format!(
            "unlisted file in checksummed tree: {unlisted}"
        )));
    }
    Ok(())
}

/// Returns whether `text` is a SHA-256 digest as `SHA256SUMS` lists it: 64 lowercase hexadecimal
/// digits.
fn is_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Returns the SHA-256 digest of the file at `path` in lowercase hexadecimal.
pub(super) fn sha256_file(path: &Path) -> Result<String> {
    let failed = |error: io::Error| io_error(ErrorCode::Io, &error, path);
    let mut file = fs::File::open(path).map_err(failed)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).map_err(failed)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Returns the error of a release archive that `message` describes.
fn invalid(message: String) -> Exit {
    Exit::error(ErrorCode::Unavailable, message)
}

#[cfg(test)]
mod tests;
