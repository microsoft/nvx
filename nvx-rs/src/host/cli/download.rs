//! `nvx-rs download`: the port of the former `nvx.py download`, which installs the NVX release of
//! a repository checkout, and the download that `run` and `sandbox` start themselves in a
//! checkout without artifacts.
//!
//! The release of a checkout is the package of the host's platform, such as `linux-kvm`, that CI
//! built from the checkout's `HEAD`: CI publishes a development release, tagged
//! `v<VERSION>-dev.<first 12 digits of the commit>`, for each `dev` commit whose checks pass.
//! When `HEAD` has none, as for CI's `[skip ci]` baseline commits, commits whose checks failed,
//! and local commits, it is the release of the nearest first-parent ancestor that has one, at
//! most [`SEARCH_COMMITS`] commits back, at one GitHub API request each.
//!
//! `download` finds the release with `git` and the GitHub REST API, downloads its package, and
//! checks the package's size, the names of its members, and its `SHA256SUMS`. It then installs
//! the package's OpenVMM executable, guest artifacts, and their build provenance into the
//! checkout, replacing the artifacts there and removing the guest artifacts that the package does
//! not declare: all of them, or none if a step fails ([`install`]). It leaves `nvx-rs` alone,
//! which the checkout builds with Cargo.
//!
//! `run`, `sandbox run`, and `sandbox start` download the release of a Git checkout themselves
//! when the checkout lacks an artifact that they run and holds none of the files that a release
//! installs, unless [`AUTO_DOWNLOAD_VARIABLE`] is `0` ([`ensure`]). A checkout that holds some of
//! them, such as a local build of OpenVMM, gets no release over them: the command warns, and fails
//! as the artifact is missing. One download at a time installs into a checkout: each holds its
//! [`DownloadLock`] while it searches for, downloads, and installs a release. A command that runs
//! the artifacts of a checkout decides what to do once no download installs into it
//! ([`ensure_under`]), so that it never reads half of an installation, and no download replaces
//! the artifacts while it reads them and launches OpenVMM on them ([`Hold`]).
//!
//! The download runs host tools, as the `bundled` build script does: `git`, which [`git`] runs;
//! `curl`, which [`github`] runs; and `tar`, which [`archive`] runs. On Windows, `curl` and `tar`
//! are those of `%SystemRoot%\System32`, whose `tar` reads the ZIP package of `windows-whp`.

mod archive;
mod git;
mod github;
mod install;
#[cfg(test)]
mod tests;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use self::git::Git;
use self::github::{Failure, GitHub};
use self::install::{Installed, Mode};
use super::args::{Parser, Receiver, Spec, Syntax, choice};
use super::artifacts::{Located, locate};
use super::sandbox::HYPERVISORS;
use super::{Exit, Result, check_interrupt, io_error, output};
use crate::ErrorCode;
use crate::host::artifacts::{ArtifactDir, Guest, Layout, MANIFEST_NAME, REPO_BUILD_DIR};

/// The usage and help of the parser.
pub(super) const SYNTAX: Syntax = Syntax {
    command: "download",
    arguments: "[-h] [--repository OWNER/REPOSITORY] [--hypervisor {auto,whp,kvm,mshv}]",
    help: "\
Download the NVX release package of this host's platform that CI built from
the checkout's HEAD, or else from its nearest first-parent ancestor that has
one, and install its OpenVMM executable, guest artifacts, and their build
provenance into the checkout, replacing the artifacts there.

options:
  -h, --help            show this help message and exit
  --repository OWNER/REPOSITORY
                        the GitHub repository whose releases to download
                        (default: microsoft/nvx)
  --hypervisor {auto,whp,kvm,mshv}
                        the hypervisor of the package: auto selects whp on
                        Windows and kvm on Linux (default: auto)

environment:
  GH_TOKEN, GITHUB_TOKEN
                        a GitHub token that can read the repository, for a
                        private one; a token that GitHub rejects is dropped",
};

/// The options of `nvx-rs download`.
pub(super) const OPTIONS: [Spec; 2] = [Spec::one("--repository"), Spec::one("--hypervisor")];

/// The GitHub repository whose releases a download installs, unless `--repository` names another.
pub(crate) const DEFAULT_REPOSITORY: &str = "microsoft/nvx";
/// How many commits of the first-parent history of `HEAD` the search for a release reads at most.
pub(crate) const SEARCH_COMMITS: usize = 20;
/// The environment variable that keeps `run` and `sandbox` from downloading a release when it is
/// `0`.
pub(crate) const AUTO_DOWNLOAD_VARIABLE: &str = "NVX_AUTO_DOWNLOAD";
/// The environment variables that name a GitHub token, in the order in which they are read.
const TOKEN_VARIABLES: [&str; 2] = ["GH_TOKEN", "GITHUB_TOKEN"];
/// The entry whose presence at the root of a checkout makes it a Git checkout: a directory, or a
/// file in a worktree.
const GIT_ENTRY: &str = ".git";

/// The parsed options of `nvx-rs download`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Options {
    /// `--repository`, [`DEFAULT_REPOSITORY`] by default.
    pub(crate) repository: String,
    /// `--hypervisor`, `auto` by default.
    pub(crate) hypervisor: &'static str,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            repository: DEFAULT_REPOSITORY.to_owned(),
            hypervisor: "auto",
        }
    }
}

impl Receiver for Options {
    fn take(&mut self, name: &'static str, value: Option<&str>) -> std::result::Result<(), String> {
        let value = value.unwrap_or_default();
        if name == "--repository" {
            if github::repository_parts(value).is_none() {
                return Err(format!("expected OWNER/REPOSITORY, not {value:?}"));
            }
            self.repository = value.to_owned();
        } else {
            self.hypervisor = choice(value, &HYPERVISORS)?;
        }
        Ok(())
    }
}

/// Parses the arguments of `download`, and returns the options and the arguments that the parser
/// does not recognize.
pub(super) fn parse(arguments: &[String]) -> Result<(Options, Vec<String>)> {
    let mut options = Options::default();
    let unrecognized = Parser::new(&OPTIONS, &[])
        .parse(arguments, &mut options)
        .map_err(|stop| Exit::from_stop(stop, &SYNTAX))?;
    Ok((options, unrecognized))
}

/// Installs the release of the checkout of `--root`, or of the one that holds the executable.
pub(super) fn run(root: Option<&str>, options: &Options) -> Result {
    let Located { dir, .. } = locate(root)?;
    if dir.layout() == Layout::Release {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            format!(
                "download installs a release into a repository checkout, and {} is an \
                 extracted release",
                dir.root().display()
            ),
        ));
    }
    let platform = platform(options.hypervisor)?;
    let (history, commits) = read_history(&dir)?;
    let _lock = DownloadLock::acquire(&dir, Report::Stdout)?;
    install_release(
        &dir,
        &history,
        &commits,
        &options.repository,
        platform,
        Report::Stdout,
        Mode::Replace,
    )
    .map(drop)
}

/// Where a download reports its progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Report {
    /// Standard output: the output of `download`.
    Stdout,
    /// Standard error, which leaves the output of the command that downloads alone.
    Stderr,
}

impl Report {
    /// Prints `text` and a line end where the progress goes.
    fn print(self, text: &str) -> Result {
        match self {
            Self::Stdout => output::print_stdout(text).map_err(Exit::from_os_error),
            Self::Stderr => {
                output::print_stderr(text);
                Ok(())
            }
        }
    }
}

/// Returns the release platform of `hypervisor` on this host, as `_release_platform` of `nvx.py`
/// did: Windows takes `whp`, and Linux `kvm` and `mshv`, and `auto` selects the first of them.
fn platform(hypervisor: &str) -> Result<&'static str> {
    let (host, platforms): (&str, &[(&str, &'static str)]) = if cfg!(windows) {
        ("windows", &[("whp", "windows-whp")])
    } else if cfg!(target_os = "linux") {
        ("linux", &[("kvm", "linux-kvm"), ("mshv", "linux-mshv")])
    } else {
        return Err(Exit::error(
            ErrorCode::Unavailable,
            format!(
                "release downloads are unsupported on {}",
                std::env::consts::OS
            ),
        ));
    };
    let selected = match hypervisor {
        "auto" => platforms[0].0,
        other => other,
    };
    platforms
        .iter()
        .find(|(name, _)| *name == selected)
        .map(|(_, platform)| *platform)
        .ok_or_else(|| {
            Exit::error(
                ErrorCode::Unsupported,
                format!("{selected} is not supported on {host}"),
            )
        })
}

/// The package of a release, as the GitHub REST API describes its asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Asset {
    /// The tag of the release.
    pub(super) tag: String,
    /// The package's file name, such as `nvx-0.1.0-linux-kvm.tar.gz`.
    pub(super) name: String,
    /// The REST API URL of the asset, which serves the package.
    pub(super) url: String,
    /// The package's size in bytes.
    pub(super) size: u64,
}

impl Asset {
    /// Returns the package of `platform` that `asset`, an asset of the release tagged `tag`,
    /// describes, if it describes one, with its URL and size.
    fn of_platform(tag: &str, asset: &Value, platform: &str) -> Option<Self> {
        let name = asset.get("name")?.as_str()?;
        let url = asset.get("url")?.as_str()?;
        let size = asset.get("size")?.as_u64()?;
        is_package(name, platform).then(|| Self {
            tag: tag.to_owned(),
            name: name.to_owned(),
            url: url.to_owned(),
            size,
        })
    }
}

/// Returns whether `name` names a package of `platform`, `nvx-VERSION-PLATFORM.tar.gz`, or
/// `.zip` for a Windows platform, as `nvx.py` matched it, with a `VERSION` that holds no path
/// separator, colon, or control character.
fn is_package(name: &str, platform: &str) -> bool {
    let extension = if platform.starts_with("windows-") {
        ".zip"
    } else {
        ".tar.gz"
    };
    name.strip_prefix("nvx-")
        .and_then(|rest| rest.strip_suffix(extension))
        .and_then(|rest| rest.strip_suffix(platform))
        .and_then(|rest| rest.strip_suffix('-'))
        .is_some_and(|version| {
            !version.is_empty()
                && !version.contains(['/', '\\', ':'])
                && !version.chars().any(char::is_control)
        })
}

/// Returns the tag of the development release that CI publishes for `commit` at `version`, as
/// `development_release_tag` of `nvx.py` names it.
fn development_tag(version: &str, commit: &str) -> String {
    format!("v{version}-dev.{}", commit.get(..12).unwrap_or(commit))
}

/// The history of the `HEAD` of a checkout, which [`Git`] reads.
trait History {
    /// Returns up to `limit` commits of the first-parent history of `HEAD`, from `HEAD` on.
    fn first_parent_commits(&self, limit: usize) -> Result<Vec<String>>;
    /// Returns the version that the `VERSION` file of `commit` holds, or `None` if it has none.
    fn version_at(&self, commit: &str) -> Result<Option<String>>;
}

/// The releases of a GitHub repository, which [`GitHub`] queries.
trait Releases {
    /// Returns the repository, `OWNER/REPOSITORY`.
    fn repository(&self) -> &str;
    /// Returns the release tagged `tag`, or `None` if the repository has none.
    fn release(&self, tag: &str) -> std::result::Result<Option<Value>, Failure>;
    /// Checks that the repository can be read, to report one that cannot, whose releases GitHub
    /// reports missing as it reports missing releases.
    fn check_repository(&self) -> std::result::Result<(), Failure>;
}

/// Returns the package of `platform` of the release that CI built from the first of `commits`
/// that has one, which `history` read from `HEAD` on, among the releases of `releases`, as
/// `_checkout_release_asset` of `nvx.py` did. Draft releases do not count.
fn search(
    commits: &[String],
    history: &dyn History,
    releases: &dyn Releases,
    platform: &str,
    report: Report,
) -> std::result::Result<Asset, Failure> {
    for (distance, commit) in commits.iter().enumerate() {
        check_interrupt()?;
        let Some(version) = history.version_at(commit)? else {
            continue;
        };
        let tag = development_tag(&version, commit);
        let Some(release) = releases.release(&tag)? else {
            continue;
        };
        let release = release.as_object().ok_or_else(|| {
            Exit::error(
                ErrorCode::Unavailable,
                "GitHub release query returned an invalid response",
            )
        })?;
        if release.get("draft").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(assets) = release.get("assets").and_then(Value::as_array) else {
            continue;
        };
        if let Some(asset) = assets
            .iter()
            .find_map(|asset| Asset::of_platform(&tag, asset, platform))
        {
            if distance > 0 {
                let plural = if distance > 1 { "s" } else { "" };
                report.print(&format!(
                    ">> no {platform} release for HEAD; using {tag}, {distance} first-parent \
                     commit{plural} earlier"
                ))?;
            }
            return Ok(asset);
        }
    }
    // GitHub also answers 404 for a repository that the caller cannot read, so query it to report
    // that case with its hint.
    releases.check_repository()?;
    Err(Exit::error(
        ErrorCode::Unavailable,
        format!(
            "no {} release with an NVX package for {platform} was built from HEAD or its \
             first-parent ancestors ({} commits searched); check out a released commit or build \
             the artifacts",
            releases.repository(),
            commits.len()
        ),
    )
    .into())
}

/// Finds the package as [`search`] does, among the releases that `connect` returns for `token`,
/// and returns it with those releases. If GitHub rejects the token with HTTP 401 or 403, it
/// reports that, and searches again without the token, as
/// `_checkout_release_asset_with_fallback` of `nvx.py` did; if GitHub fails that search too, it
/// fails as GitHub failed the first.
fn find<R: Releases>(
    commits: &[String],
    history: &dyn History,
    platform: &str,
    token: Option<String>,
    connect: impl Fn(Option<String>) -> Result<R>,
    report: Report,
) -> Result<(Asset, R)> {
    let authenticated = token.is_some();
    let releases = connect(token)?;
    let rejected = match search(commits, history, &releases, platform, report) {
        Ok(asset) => return Ok((asset, releases)),
        Err(Failure::Http(401 | 403, error)) if authenticated => error,
        Err(failure) => return Err(failure.into()),
    };
    output::print_stderr(&format!(
        ">> {}; retrying without credentials",
        rejected.message().unwrap_or_default()
    ));
    let public = connect(None)?;
    match search(commits, history, &public, platform, report) {
        Ok(asset) => Ok((asset, public)),
        Err(Failure::Http(..)) => Err(rejected),
        Err(Failure::Other(error)) => Err(error),
    }
}

/// Returns the history of the checkout `dir`, which must be an NVX repository checkout, and up to
/// [`SEARCH_COMMITS`] commits of the first-parent history of its `HEAD`.
fn read_history(dir: &ArtifactDir) -> Result<(Git, Vec<String>)> {
    let root = dir.root();
    if !root.join(MANIFEST_NAME).is_file() {
        return Err(Exit::error(
            ErrorCode::Unavailable,
            format!(
                "{} is not an NVX repository checkout: it has no {MANIFEST_NAME}",
                root.display()
            ),
        ));
    }
    let history = Git::new(root);
    let commits = history.first_parent_commits(SEARCH_COMMITS)?;
    Ok((history, commits))
}

/// Downloads the release of the checkout `dir`, whose first-parent `commits` `history` read,
/// from `repository` for `platform`, checks it, and installs it as `mode` says, reporting its
/// progress as `report` says. The caller holds the [`DownloadLock`] of the checkout.
fn install_release(
    dir: &ArtifactDir,
    history: &dyn History,
    commits: &[String],
    repository: &str,
    platform: &str,
    report: Report,
    mode: Mode,
) -> Result<Installed> {
    let staging = Staging::new(&dir.root().join(REPO_BUILD_DIR))?;
    let (asset, github) = find(
        commits,
        history,
        platform,
        token(),
        |token| GitHub::new(repository, token, staging.path()),
        report,
    )?;
    report.print(&format!(
        ">> downloading {} ({}) from {}",
        asset.name,
        size_text(asset.size),
        asset.tag
    ))?;
    let archive = staging.path().join(&asset.name);
    github.download(&asset, &archive)?;
    let package = archive::extract(&archive)?;
    let plan = install::plan(&package, dir)?;
    if !plan.compatible() {
        output::print_stderr(&format!(
            "warning: the artifacts of {} declare other OpenVMM contracts than this nvx-rs \
             speaks ({}), so its sandboxes may fail on them: check out {0}, or build the \
             artifacts",
            asset.tag,
            plan.mismatches().join("; ")
        ));
    }
    let installed = install::commit(&plan, staging.token(), mode)?;
    if installed == Installed::All {
        report.print(&format!(">> installed {} for {platform}", asset.tag))?;
    }
    Ok(installed)
}

/// The name of the lock file of the downloads into a checkout, in its `build/`.
const LOCK_NAME: &str = ".nvx-download.lock";
/// How long a download, or a command that waits for one, waits between its attempts to take the
/// lock.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Returns the path of the lock file of the downloads into the checkout `dir`.
fn lock_path(dir: &ArtifactDir) -> PathBuf {
    dir.root().join(REPO_BUILD_DIR).join(LOCK_NAME)
}

/// The lock of the downloads into a checkout, on its `build/.nvx-download.lock`. One download at
/// a time holds it exclusively while it searches for, downloads, and installs a release, and a
/// command that runs the artifacts of the checkout holds it shared while it decides whether to
/// download one, and then while it reads the artifacts and launches OpenVMM on them, as [`Hold`]
/// says. The operating system releases it when its holder exits, however it exits, and the lock
/// file stays.
struct DownloadLock {
    /// The lock file, whose lock the holder holds until it drops the file.
    _file: fs::File,
}

impl DownloadLock {
    /// Takes the lock of the downloads into the checkout `dir` exclusively, creating its file,
    /// and waits while another download, or a command that reads the checkout, holds it, as
    /// [`Self::wait`] says.
    fn acquire(dir: &ArtifactDir, report: Report) -> Result<Self> {
        let path = lock_path(dir);
        let file =
            create_lock_file(dir).map_err(|(error, at)| io_error(ErrorCode::Io, &error, &at))?;
        Self::wait(
            dir,
            &path,
            file,
            report,
            "another nvx-rs command that uses the artifacts of",
            fs::File::try_lock,
        )
    }

    /// Takes the lock of the downloads into the checkout `dir` shared, which other commands that
    /// read the checkout share, creating its file as a download does, and waits while a download
    /// holds it, as [`Self::wait`] says. Returns `None` for a checkout that has no lock file and
    /// may not create one, such as a read-only one, into which no download installs either.
    fn shared(dir: &ArtifactDir, report: Report) -> Result<Option<Self>> {
        let path = lock_path(dir);
        let file = match create_lock_file(dir) {
            Ok(file) => file,
            Err((error, _))
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
                ) =>
            {
                match fs::File::open(&path) {
                    Ok(file) => file,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ) =>
                    {
                        return Ok(None);
                    }
                    Err(error) => return Err(io_error(ErrorCode::Io, &error, &path)),
                }
            }
            Err((error, at)) => return Err(io_error(ErrorCode::Io, &error, &at)),
        };
        Self::wait(
            dir,
            &path,
            file,
            report,
            "another download into",
            fs::File::try_lock_shared,
        )
        .map(Some)
    }

    /// Takes the lock of the lock `file` at `path` of the checkout `dir` with `try_lock`, waiting
    /// while another holder keeps it. A wait that outlasts a retry interval is reported once as
    /// `report` says, as waiting for `holder` the checkout: a command that only decides holds the
    /// lock for far less. Ctrl-C ends the wait.
    fn wait(
        dir: &ArtifactDir,
        path: &Path,
        file: fs::File,
        report: Report,
        holder: &str,
        try_lock: fn(&fs::File) -> std::result::Result<(), fs::TryLockError>,
    ) -> Result<Self> {
        let mut attempts = 0_u32;
        loop {
            match try_lock(&file) {
                Ok(()) => return Ok(Self { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {
                    if attempts == 1 {
                        report
                            .print(&format!(">> waiting for {holder} {}", dir.root().display()))?;
                    }
                    attempts = attempts.saturating_add(1);
                    check_interrupt()?;
                    std::thread::sleep(LOCK_RETRY_INTERVAL);
                }
                Err(fs::TryLockError::Error(error)) => {
                    return Err(io_error(ErrorCode::Io, &error, path));
                }
            }
        }
    }
}

/// Opens the lock file of the downloads into the checkout `dir` for writing, creating it and the
/// checkout's `build/` if they do not exist, or returns the error and the path that it concerns.
fn create_lock_file(dir: &ArtifactDir) -> std::result::Result<fs::File, (io::Error, PathBuf)> {
    let build = dir.root().join(REPO_BUILD_DIR);
    fs::create_dir_all(&build).map_err(|error| (error, build.clone()))?;
    let path = lock_path(dir);
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| (error, path))
}

/// What a command holds while it reads the artifacts of its root and launches OpenVMM on them, so
/// that no download replaces them meanwhile: in a Git checkout that may download, the lock of the
/// checkout's downloads, shared, or exclusive after a download of the command's own. Dropping it
/// lets downloads replace the artifacts again.
#[must_use]
pub(crate) struct Hold {
    /// The lock, if the command holds one.
    _lock: Option<DownloadLock>,
}

impl Hold {
    /// Returns a hold of no lock, for a command that downloads nothing, and whose reads of the
    /// artifacts downloads need not wait for.
    pub(crate) fn none() -> Self {
        Self { _lock: None }
    }
}

/// Returns the GitHub token that `GH_TOKEN`, or else `GITHUB_TOKEN`, names, if either does.
fn token() -> Option<String> {
    TOKEN_VARIABLES
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|token| !token.is_empty()))
}

/// Returns `size` bytes as `format_size` of `nvx.py` wrote them, such as `258.4 MiB`.
fn size_text(size: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    if size < 1024 {
        return format!("{size} B");
    }
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// A private directory of the checkout's `build/`, `.nvx-download-TOKEN`, which holds the
/// responses of GitHub and the package while the download checks them, and which is removed with
/// all that it holds when dropped. Its random `TOKEN` also names the files that the installation
/// stages beside the artifacts that it replaces.
struct Staging {
    /// The directory.
    path: PathBuf,
    /// The random token in its name.
    token: String,
}

impl Staging {
    /// Creates a staging directory in `parent`, and `parent` if it does not exist.
    fn new(parent: &Path) -> Result<Self> {
        fs::create_dir_all(parent).map_err(|error| io_error(ErrorCode::Io, &error, parent))?;
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(|error| {
            Exit::error(ErrorCode::Io, format!("cannot read random bytes: {error}"))
        })?;
        let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = parent.join(format!(".nvx-download-{token}"));
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;

            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder
        };
        #[cfg(not(unix))]
        let builder = fs::DirBuilder::new();
        builder
            .create(&path)
            .map_err(|error| io_error(ErrorCode::Io, &error, &path))?;
        Ok(Self { path, token })
    }

    /// Returns the directory.
    fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the random token in its name.
    fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// What a command does before it runs the artifacts of its root.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// Nothing: the artifacts that it runs exist, or no download may provide them.
    Proceed,
    /// Warn that the first path is missing while the checkout holds the second, a file that a
    /// release installs, and download nothing.
    Warn(PathBuf, PathBuf),
    /// Download and install the release of the checkout.
    Download,
}

/// Returns whether `dir` is a Git checkout of NVX, into which `run` and `sandbox` may download a
/// release: a repository root with its `.git`, a directory or, in a worktree, a file, and its
/// `SOURCE-MANIFEST.json`.
fn is_checkout(dir: &ArtifactDir) -> bool {
    let root = dir.root();
    dir.layout() == Layout::Repository
        && fs::symlink_metadata(root.join(GIT_ENTRY)).is_ok()
        && root.join(MANIFEST_NAME).is_file()
}

/// Returns what a command that runs the artifacts `needed` of `dir` does first: it downloads the
/// release when one of them is missing, `dir` is a Git checkout, as [`is_checkout`] says,
/// `enabled` allows it, and the checkout holds none of the files that a release installs; it
/// warns when the checkout holds one of them.
fn action(
    dir: &ArtifactDir,
    needed: &[PathBuf],
    enabled: impl FnOnce() -> Result<bool>,
) -> Result<Action> {
    let Some(missing) = needed.iter().find(|path| !path.is_file()) else {
        return Ok(Action::Proceed);
    };
    if !is_checkout(dir) || !enabled()? {
        return Ok(Action::Proceed);
    }
    for path in install::destinations(dir) {
        if install::exists(&path).map_err(|error| io_error(ErrorCode::Io, &error, &path))? {
            return Ok(Action::Warn(missing.clone(), path));
        }
    }
    Ok(Action::Download)
}

/// Returns the artifacts of the root of `located` that a command runs: its OpenVMM executable,
/// unless `NVX_OPENVMM` names another, which is the caller's to provide, and for `guest`, the
/// kernel and that guest's initramfs.
fn needed(located: &Located, guest: Option<Guest>) -> Vec<PathBuf> {
    let dir = &located.dir;
    let mut needed = Vec::new();
    if located.openvmm == dir.openvmm() {
        needed.push(dir.openvmm());
    }
    if let Some(guest) = guest {
        needed.push(dir.kernel());
        needed.push(dir.initramfs(guest));
    }
    needed
}

/// Downloads and installs the release of the checkout of `located` before a command runs its
/// OpenVMM executable and, for `guest`, its kernel and that guest's initramfs, if one of them is
/// missing, as [`ensure_under`] does once the command holds what [`hold`] returns, and returns the
/// [`Hold`] that the command keeps while it reads them and launches OpenVMM on them. A command
/// that runs nothing of the root holds nothing.
pub(crate) fn ensure(located: &Located, guest: Option<Guest>, hypervisor: &str) -> Result<Hold> {
    if needed(located, guest).is_empty() {
        return Ok(Hold::none());
    }
    ensure_under(hold(located)?, located, guest, hypervisor)
}

/// Downloads and installs the release of the checkout of `located` before a command runs its
/// OpenVMM executable and, for `guest`, its kernel and that guest's initramfs, if one of them is
/// missing, as [`action`] decides, unless [`AUTO_DOWNLOAD_VARIABLE`] is `0`, while the command
/// holds `held`, which [`hold`] returned before the command read anything of the root, and returns
/// the [`Hold`] that the command keeps while it reads the artifacts and launches OpenVMM on them.
/// The download reports on standard error and installs the package of `hypervisor`.
///
/// In a Git checkout, the command decides under the shared lock of `held`, once no download
/// installs into the checkout, so that it neither warns of an artifact that a download has yet to
/// install nor runs artifacts whose provenance a download has yet to install, and no download
/// replaces the artifacts until it drops the hold. A checkout that holds some files of a release,
/// which may be local builds, gets a warning instead, and the command then fails as the artifact
/// is missing. A command that downloads drops `held` first, and then decides again once it holds
/// the [`DownloadLock`] of the checkout exclusively, after the downloads and the commands that
/// held it, and installs as [`Mode::Fresh`] says, so a file that appears meanwhile, such as from a
/// local build, cancels it with a warning. A command that may not download, as `0` says, holds
/// nothing, as does one of another root.
pub(crate) fn ensure_under(
    held: Hold,
    located: &Located,
    guest: Option<Guest>,
    hypervisor: &str,
) -> Result<Hold> {
    let dir = &located.dir;
    let needed = needed(located, guest);
    // Only a Git checkout downloads, and only where it may. The variable's other values are
    // checked once the command would download.
    if needed.is_empty() || !is_checkout(dir) || auto_download_disabled() {
        return Ok(held);
    }
    match action(dir, &needed, auto_download_enabled)? {
        Action::Proceed => return Ok(held),
        Action::Warn(missing, present) => {
            warn_partial(dir, &missing, &present);
            return Ok(held);
        }
        Action::Download => drop(held),
    }
    let platform = platform(hypervisor)?;
    output::print_stderr(&format!(
        ">> {} holds no NVX artifacts, so nvx-rs downloads its release; set \
         {AUTO_DOWNLOAD_VARIABLE}=0 to skip that",
        dir.root().display()
    ));
    let (history, commits) = read_history(dir)?;
    let lock = DownloadLock::acquire(dir, Report::Stderr)?;
    // Another download, or a build, may have provided artifacts while this one waited.
    match action(dir, &needed, || Ok(true))? {
        Action::Proceed => {}
        Action::Warn(missing, present) => warn_partial(dir, &missing, &present),
        Action::Download => {
            let installed = install_release(
                dir,
                &history,
                &commits,
                DEFAULT_REPOSITORY,
                platform,
                Report::Stderr,
                Mode::Fresh,
            )?;
            if let Installed::Cancelled(path) = installed {
                output::print_stderr(&format!(
                    "warning: {} appeared while nvx-rs downloaded the release of {}, so it \
                     installed none of it: build the missing artifacts, or replace them all with \
                     those of the release with `nvx-rs download`",
                    path.display(),
                    dir.root().display()
                ));
            }
        }
    }
    Ok(Hold { _lock: Some(lock) })
}

/// Returns what a command holds while it reads the artifacts of the root of `located`, such as the
/// runtime identity that their install metadata names, or a release's distro layer, so that no
/// download replaces them meanwhile: the shared [`DownloadLock`] of a Git checkout that may
/// download, which it takes after any download that holds it, without deciding to download
/// anything. A command that runs the artifacts then decides under it, with [`ensure_under`].
pub(crate) fn hold(located: &Located) -> Result<Hold> {
    let dir = &located.dir;
    if !is_checkout(dir) || auto_download_disabled() {
        return Ok(Hold::none());
    }
    Ok(Hold {
        _lock: DownloadLock::shared(dir, Report::Stderr)?,
    })
}

/// Returns whether [`AUTO_DOWNLOAD_VARIABLE`] is `0`, which keeps a command from downloading,
/// and so from taking the lock of downloads that it would never make.
fn auto_download_disabled() -> bool {
    std::env::var_os(AUTO_DOWNLOAD_VARIABLE).is_some_and(|value| value == "0")
}

/// Warns that the artifact `missing` of the checkout `dir` does not exist, while the checkout
/// holds `present`, a file that a release installs, so that no release is downloaded over it.
fn warn_partial(dir: &ArtifactDir, missing: &Path, present: &Path) {
    output::print_stderr(&format!(
        "warning: {} does not exist, but {} holds other NVX artifacts, such as {}, so nvx-rs \
         downloads no release over them: build the missing artifacts, or replace them all with \
         those of the release with `nvx-rs download`",
        missing.display(),
        dir.root().display(),
        present.display()
    ));
}

/// Returns whether `run` and `sandbox` may download a release, as [`auto_download_allowed`] reads
/// [`AUTO_DOWNLOAD_VARIABLE`].
fn auto_download_enabled() -> Result<bool> {
    auto_download_allowed(std::env::var_os(AUTO_DOWNLOAD_VARIABLE).as_deref())
}

/// Returns whether `value`, that of [`AUTO_DOWNLOAD_VARIABLE`] if it is set, lets `run` and
/// `sandbox` download a release: unless it is `0`. It may also be empty or `1`, and any other
/// value fails.
fn auto_download_allowed(value: Option<&std::ffi::OsStr>) -> Result<bool> {
    match value {
        None => Ok(true),
        Some(value) if value.is_empty() || value == "1" => Ok(true),
        Some(value) if value == "0" => Ok(false),
        Some(value) => Err(Exit::error(
            ErrorCode::Unsupported,
            format!("{AUTO_DOWNLOAD_VARIABLE} must be 0 or 1, not {value:?}"),
        )),
    }
}

/// Returns the program `name` of Windows's `System32`, if it has one, and else `name`, which
/// `PATH` resolves: on Windows, the tools of Windows itself, rather than others of the same name
/// on `PATH`, such as those of Git for Windows.
fn system_program(name: &str) -> PathBuf {
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        let path = Path::new(&root)
            .join("System32")
            .join(format!("{name}.exe"));
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from(name)
}

/// Removes the file at `path` if there is one.
fn remove_stale(path: &Path) -> Result {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(io_error(ErrorCode::Io, &error, path))
        }
        _ => Ok(()),
    }
}
