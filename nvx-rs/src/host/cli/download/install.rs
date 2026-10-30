//! The installation of a checked package into a repository checkout: all of it, or none.
//!
//! The package's OpenVMM executable goes to `openvmm/target/release/`, where its build puts it,
//! and its guest artifacts and their build provenance go to `build/`, as `nvx.py download`
//! installed them, and the known guest artifacts that the package does not declare, such as the
//! Azure Linux guest that source-inclusive packages omit, or the sandbox scratch template that
//! packages built before it shipped lack, go away. `nvx-rs` and its provenance stay as they are:
//! the checkout builds `nvx-rs` itself.
//!
//! The installation first moves each file of the package beside the file that it replaces, under
//! a hidden name. It then moves each file that it replaces or removes aside, under another hidden
//! name, and moves the package's file in. If a step fails, it moves everything back. Only once
//! every file is in place does it delete what it moved aside. An installation of
//! [`Mode::Fresh`] replaces and removes nothing: a file of the checkout that appears where it
//! installs cancels it, and it moves back what it installed. It links each file of the package
//! into place rather than renaming it there, as a link, unlike a rename, fails where a file
//! exists, so that a file that appears as it installs, too late for it to see, cancels it as
//! well, rather than being replaced. When it moves back what it installed, it keeps a file that
//! changed since it put it in place, such as one that a local build rewrote or replaced, which
//! the digest that the package's `SHA256SUMS` lists no longer matches.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::ErrorCode;
use crate::contract::manifest_is_compatible;
use crate::host::artifacts::{
    ArtifactDir, CHECKSUMS_NAME, Guest, INITRAMFS_PROVENANCE_NAME, KERNEL_NAME,
    KERNEL_PROVENANCE_NAME, MANIFEST_NAME, OPENVMM_PROVENANCE_NAME, openvmm_executable,
};
use crate::host::cli::{Exit, Result, io_error, io_message, output};

/// The suffix of the name of the kernel's configuration, which lies beside the kernel.
const KERNEL_CONFIG_SUFFIX: &str = ".config";
/// The directory of a package that holds its executables.
const BIN_DIR: &str = "bin";
/// The directory of a package that holds its guest artifacts.
const GUEST_DIR: &str = "guest";
/// The directory of a package that holds the build provenance of its artifacts.
const PROVENANCE_DIR: &str = "provenance";
/// The build provenance that a package ships for its OpenVMM executable, kernel, and Alpine
/// initramfs, which a checkout keeps beside its guest artifacts.
const PROVENANCE: [&str; 3] = [
    OPENVMM_PROVENANCE_NAME,
    KERNEL_PROVENANCE_NAME,
    INITRAMFS_PROVENANCE_NAME,
];
/// The field of a guest's section of a package's manifest that gives the SHA-256 digest of the
/// guest's sandbox scratch template, and so declares it: packages built before the template
/// shipped have the section without the field.
const SCRATCH_TEMPLATE_FIELD: &str = "scratch_template_sha256";

/// Returns the names of the guest artifacts of the kernel, which the `linux` section of a
/// package's manifest declares: the kernel and its configuration.
fn kernel_names() -> Vec<String> {
    vec![
        KERNEL_NAME.to_owned(),
        format!("{KERNEL_NAME}{KERNEL_CONFIG_SUFFIX}"),
    ]
}

/// Returns the names of the guest artifacts of `guest`, which the section of its name in a
/// package's manifest declares: its initramfs and package manifest, its distro layer and the
/// layer's manifest, if it has one, and, if `scratch_template`, its sandbox scratch template, if
/// it has one.
fn guest_names(guest: Guest, scratch_template: bool) -> Vec<String> {
    let mut names = vec![
        guest.initramfs_name().to_owned(),
        guest.package_manifest_name(),
    ];
    names.extend(guest.distro_layer_name().map(str::to_owned));
    names.extend(guest.distro_manifest_name());
    if scratch_template {
        names.extend(guest.scratch_template_name().map(str::to_owned));
    }
    names
}

/// Returns the names of the guest artifacts of the kernel and of every guest, with each guest's
/// sandbox scratch template if `scratch_templates`.
fn guest_artifact_names(scratch_templates: bool) -> Vec<String> {
    let mut names = kernel_names();
    for guest in Guest::ALL {
        names.extend(guest_names(guest, scratch_templates));
    }
    names
}

/// Returns the names of every guest artifact that a package may ship, as
/// `ReleaseBuildConstants.GUEST_ARTIFACT_NAMES` of `nvx.py` names them.
fn all_guest_names() -> Vec<String> {
    guest_artifact_names(true)
}

/// Returns each file of the checkout `dir` that installing a release writes or removes: the
/// OpenVMM executable, every guest artifact, and the build provenance of the artifacts.
pub(super) fn destinations(dir: &ArtifactDir) -> Vec<PathBuf> {
    let mut paths = vec![dir.openvmm()];
    paths.extend(all_guest_names().iter().map(|name| dir.artifact(name)));
    paths.extend(PROVENANCE.iter().map(|name| dir.artifact(name)));
    paths
}

/// Returns whether a file, directory, or link, broken or not, exists at `path`: not if its
/// metadata is not found, or a component of `path` is no directory, and fails if it cannot be
/// read otherwise, as when access is denied.
pub(super) fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// How an installation treats the files of the checkout that a release installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Replace them, and remove the guest artifacts that the package does not declare, as
    /// `nvx-rs download` does.
    Replace,
    /// Install the package where the checkout holds none of them, as `run` and `sandbox` do: one
    /// that appears meanwhile, such as from a local build, cancels the installation.
    Fresh,
}

/// How an installation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Installed {
    /// Every file of the package is in place.
    All,
    /// A file of the checkout appeared at this path, so an installation of [`Mode::Fresh`]
    /// installed nothing.
    Cancelled(PathBuf),
}

/// A file of a package and the file of the checkout that it replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    /// The package's file.
    source: PathBuf,
    /// The checkout's file.
    destination: PathBuf,
    /// Whether it is an executable, which gets mode 0755 on Unix.
    executable: bool,
    /// The SHA-256 digest of the package's file, in lowercase hexadecimal, which the package's
    /// `SHA256SUMS` lists.
    digest: String,
}

/// How to install a package into a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Plan {
    /// The files of the package that the installation moves in.
    items: Vec<Item>,
    /// The guest artifacts of the checkout that the package does not declare.
    removals: Vec<PathBuf>,
    /// Whether the package's manifest declares the contracts that this crate speaks.
    compatible: bool,
}

impl Plan {
    /// Returns whether the package's manifest declares the microVM ABI, control session, and
    /// control contract that this crate speaks.
    pub(super) fn compatible(&self) -> bool {
        self.compatible
    }
}

/// Returns how to install the package at `package` into the checkout `dir`, as
/// `_install_release_archive` of `nvx.py` did, and fails for a file that the package lacks: its
/// OpenVMM executable, each guest artifact that its `SOURCE-MANIFEST.json` declares, or the build
/// provenance of its artifacts, or that its `SHA256SUMS` does not list.
pub(super) fn plan(package: &Path, dir: &ArtifactDir) -> Result<Plan> {
    let manifest = read_manifest(&package.join(MANIFEST_NAME))?;
    let names = match &manifest {
        Some(manifest) => declared_names(manifest)?,
        // A package without a manifest predates the sandbox scratch template.
        None => guest_artifact_names(false),
    };
    let digests = read_digests(package)?;
    let item = |directory: &str, name: &str, destination: PathBuf, description: &str| {
        let source = required(package.join(directory).join(name), description)?;
        let relative = format!("{directory}/{name}");
        let digest = digests.get(&relative).cloned().ok_or_else(|| {
            Exit::error(
                ErrorCode::Unavailable,
                format!("packaged {CHECKSUMS_NAME} lists no {relative}"),
            )
        })?;
        Ok::<_, Exit>(Item {
            source,
            destination,
            executable: directory == BIN_DIR,
            digest,
        })
    };
    let mut items = vec![item(
        BIN_DIR,
        openvmm_executable(),
        dir.openvmm(),
        "packaged OpenVMM binary",
    )?];
    for name in &names {
        items.push(item(
            GUEST_DIR,
            name,
            dir.artifact(name),
            &format!("packaged guest artifact {name}"),
        )?);
    }
    for name in PROVENANCE {
        items.push(item(
            PROVENANCE_DIR,
            name,
            dir.artifact(name),
            &format!("packaged provenance artifact {name}"),
        )?);
    }
    let removals = all_guest_names()
        .into_iter()
        .filter(|name| !names.contains(name))
        .map(|name| dir.artifact(&name))
        .collect();
    Ok(Plan {
        items,
        removals,
        compatible: manifest.as_ref().is_some_and(manifest_is_compatible),
    })
}

/// Returns the digest of each file of the package at `package`, by its path in the package, as the
/// package's `SHA256SUMS` lists them, which the package's extraction checked.
fn read_digests(package: &Path) -> Result<BTreeMap<String, String>> {
    let path = package.join(CHECKSUMS_NAME);
    let text = fs::read_to_string(&path).map_err(|error| io_error(ErrorCode::Io, &error, &path))?;
    Ok(text
        .lines()
        .filter_map(|line| line.split_once("  "))
        .map(|(digest, file)| (file.to_owned(), digest.to_owned()))
        .collect())
}

/// Returns the package's manifest at `path`, or `None` if it has none.
fn read_manifest(path: &Path) -> Result<Option<Value>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(ErrorCode::Io, &error, path)),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(manifest) if manifest.is_object() => Ok(Some(manifest)),
        _ => Err(Exit::error(
            ErrorCode::Unavailable,
            format!(
                "packaged source manifest is not a JSON object: {}",
                path.display()
            ),
        )),
    }
}

/// Returns the names of the guest artifacts that `manifest` declares, as
/// `_packaged_guest_artifact_names` of `nvx.py` read them: those of the kernel for its `linux`
/// section, and those of each guest for the section of the guest's name, with the guest's sandbox
/// scratch template where that section gives its digest.
fn declared_names(manifest: &Value) -> Result<Vec<String>> {
    let section = |name: &str| manifest.get(name).filter(|section| section.is_object());
    let mut names = Vec::new();
    if section("linux").is_some() {
        names.extend(kernel_names());
    }
    for guest in Guest::ALL {
        if let Some(section) = section(guest.name()) {
            names.extend(guest_names(
                guest,
                section.get(SCRATCH_TEMPLATE_FIELD).is_some(),
            ));
        }
    }
    if names.is_empty() {
        return Err(Exit::error(
            ErrorCode::Unavailable,
            "packaged source manifest does not declare guest artifacts",
        ));
    }
    Ok(names)
}

/// Returns `path` if it is a regular file, and fails with `DESCRIPTION not found: PATH` if not.
fn required(path: PathBuf, description: &str) -> Result<PathBuf> {
    if path.is_file() {
        Ok(path)
    } else {
        Err(Exit::error(
            ErrorCode::Unavailable,
            format!("{description} not found: {}", path.display()),
        ))
    }
}

/// Installs `plan` as `mode` says, under hidden names that hold `token` while it moves files.
pub(super) fn commit(plan: &Plan, token: &str, mode: Mode) -> Result<Installed> {
    commit_with(
        plan,
        token,
        mode,
        &mut |from, to| fs::rename(from, to),
        &mut |from, to| fs::hard_link(from, to),
    )
}

/// Why the installation stopped moving files into the checkout.
#[derive(Debug)]
enum Stop {
    /// A step failed with this error.
    Failed(Exit),
    /// A file of the checkout appeared at this path, which an installation of [`Mode::Fresh`]
    /// does not replace.
    Appeared(PathBuf),
}

/// Installs `plan` as [`commit`] does, moving files with `rename` and linking them with `link`,
/// which tests make fail, or race with a local build.
fn commit_with(
    plan: &Plan,
    token: &str,
    mode: Mode,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
    link: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> Result<Installed> {
    let mut staged = Vec::new();
    for item in &plan.items {
        match stage(item, token, rename) {
            Ok(path) => staged.push(path),
            Err(error) => {
                discard(&staged);
                return Err(error);
            }
        }
    }
    let mut moves = Vec::new();
    if let Err(stop) = swap(plan, &staged, token, mode, rename, link, &mut moves) {
        let failures = roll_back(&moves);
        discard(&staged);
        return match (stop, failures.is_empty()) {
            (Stop::Appeared(path), true) => Ok(Installed::Cancelled(path)),
            (Stop::Failed(error), true) => Err(error),
            (Stop::Appeared(path), false) => Err(Exit::error(
                ErrorCode::Io,
                format!(
                    "{} appeared during the installation, and restoring the checkout failed: {}",
                    path.display(),
                    failures.join("; ")
                ),
            )),
            (Stop::Failed(error), false) => Err(Exit::error(
                error.code().unwrap_or(ErrorCode::Io),
                format!(
                    "{}; restoring the checkout failed too: {}",
                    error.message().unwrap_or_default(),
                    failures.join("; ")
                ),
            )),
        };
    }
    // The files of the checkout that went aside go now, and so do the staged names of the files
    // that a fresh installation linked into place, which their places keep.
    let asides = moves.iter().filter_map(|step| step.aside.as_ref());
    let linked = staged.iter().filter(|_| mode == Mode::Fresh);
    for path in asides.chain(linked) {
        if let Err(error) = remove(path) {
            output::print_stderr(&format!(
                "warning: cannot remove {}",
                io_message(&error, path)
            ));
        }
    }
    Ok(Installed::All)
}

/// A file of the checkout that the installation changed.
#[derive(Debug)]
struct Move {
    /// The file of the checkout.
    destination: PathBuf,
    /// Where the file that it held went, if it held one.
    aside: Option<PathBuf>,
    /// Whether the package's file took its place.
    installed: bool,
    /// The digest of the package's file that a fresh installation put in its place, which the
    /// rollback checks before it removes the file there.
    digest: Option<String>,
}

/// Moves the package's file of `item` beside its destination, or copies it there if it cannot
/// move it, as across file systems, and returns where.
fn stage(
    item: &Item,
    token: &str,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> Result<PathBuf> {
    if let Some(parent) = item.destination.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(ErrorCode::Io, &error, parent))?;
    }
    let staged = sibling(&item.destination, token, "new");
    if rename(&item.source, &staged).is_err()
        && let Err(error) = fs::copy(&item.source, &staged)
    {
        let _ = fs::remove_file(&staged);
        return Err(io_error(ErrorCode::Io, &error, &staged));
    }
    if item.executable
        && let Err(error) = make_executable(&staged)
    {
        // The caller discards only the files that it received.
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    Ok(staged)
}

/// Moves the files of the checkout that `plan` replaces or removes aside, and the package's files,
/// which `staged` holds in the order of the plan's items, into their places, as `mode` says,
/// recording each change in `moves` as it makes it.
fn swap(
    plan: &Plan,
    staged: &[PathBuf],
    token: &str,
    mode: Mode,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
    link: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
    moves: &mut Vec<Move>,
) -> std::result::Result<(), Stop> {
    for (item, staged) in plan.items.iter().zip(staged) {
        let aside = set_aside(&item.destination, token, mode, rename)?;
        moves.push(Move {
            destination: item.destination.clone(),
            aside,
            installed: false,
            digest: (mode == Mode::Fresh).then(|| item.digest.clone()),
        });
        put(staged, &item.destination, mode, rename, link)?;
        if let Some(step) = moves.last_mut() {
            step.installed = true;
        }
    }
    for removal in &plan.removals {
        if let Some(aside) = set_aside(removal, token, mode, rename)? {
            moves.push(Move {
                destination: removal.clone(),
                aside: Some(aside),
                installed: false,
                digest: None,
            });
        }
    }
    Ok(())
}

/// Puts the package's file `staged` in its place, `destination`: [`Mode::Replace`] renames it
/// there, over any file. [`Mode::Fresh`] links it there instead, and leaves `staged` for the
/// caller to remove, as a link fails where a file exists: one that appeared since [`set_aside`]
/// found none stops the installation, where a rename would replace it with nothing to restore.
fn put(
    staged: &Path,
    destination: &Path,
    mode: Mode,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
    link: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> std::result::Result<(), Stop> {
    let failed = |error: io::Error| Stop::Failed(io_error(ErrorCode::Io, &error, destination));
    match mode {
        Mode::Replace => rename(staged, destination).map_err(failed),
        Mode::Fresh => link(staged, destination).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                Stop::Appeared(destination.to_path_buf())
            } else {
                failed(error)
            }
        }),
    }
}

/// Moves the file at `path` aside, if there is one, and returns where it went. An installation of
/// [`Mode::Fresh`] stops at it instead, and one of [`Mode::Replace`] at a directory, which a release
/// replaces with no file: it holds files of its own, which the cleanup would delete with it.
fn set_aside(
    path: &Path,
    token: &str,
    mode: Mode,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> std::result::Result<Option<PathBuf>, Stop> {
    let failed = |error: io::Error| Stop::Failed(io_error(ErrorCode::Io, &error, path));
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(failed(error)),
    };
    if mode == Mode::Fresh {
        return Err(Stop::Appeared(path.to_path_buf()));
    }
    if metadata.is_dir() {
        return Err(Stop::Failed(Exit::error(
            ErrorCode::Unsupported,
            format!(
                "{} is a directory, which a release does not replace",
                path.display()
            ),
        )));
    }
    let aside = sibling(path, token, "old");
    rename(path, &aside).map_err(failed)?;
    Ok(Some(aside))
}

/// Undoes `moves`, the last first, and returns the messages of the steps that failed.
fn roll_back(moves: &[Move]) -> Vec<String> {
    let mut failures = Vec::new();
    for step in moves.iter().rev() {
        if step.installed
            && let Some(failure) = remove_installed(step)
        {
            failures.push(failure);
        }
        if let Some(aside) = &step.aside
            && let Err(error) = fs::rename(aside, &step.destination)
        {
            failures.push(io_message(&error, &step.destination));
        }
    }
    failures
}

/// Removes the package's file that the installation put at the destination of `step`, and
/// returns the message of a failure. A fresh installation, which removes nothing of the
/// checkout's, keeps a file there that no longer holds the package's file, which the step's digest
/// identifies, as when a local build rewrote or replaced it after the installation put it there.
fn remove_installed(step: &Move) -> Option<String> {
    let path = &step.destination;
    if let Some(digest) = &step.digest {
        match exists(path) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(error) => return Some(io_message(&error, path)),
        }
        match super::archive::sha256_file(path) {
            Ok(actual) if actual != *digest => return None,
            Ok(_) => {}
            Err(error) => return Some(error.message().unwrap_or_default().to_owned()),
        }
    }
    fs::remove_file(path)
        .err()
        .map(|error| io_message(&error, path))
}

/// Removes the staged files of the package that are still staged.
fn discard(staged: &[PathBuf]) {
    for path in staged {
        let _ = fs::remove_file(path);
    }
}

/// Removes the file at `path`, or the directory if it is empty, but never what a directory holds.
fn remove(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// Returns the hidden sibling of `path` that the installation that `token` names uses for
/// `purpose`: `.NAME.TOKEN.new` for the package's file, and `.NAME.TOKEN.old` for the file that
/// it replaces.
fn sibling(path: &Path, token: &str, purpose: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{token}.{purpose}"))
}

/// Makes the file at `path` executable, with mode 0755.
#[cfg(unix)]
fn make_executable(path: &Path) -> Result {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|error| io_error(ErrorCode::Io, &error, path))
}

/// Makes the file at `path` executable, which every file is on this host.
#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result {
    Ok(())
}

#[cfg(test)]
mod tests;
