//! The harness of the tests of the `nvx-rs` CLI.
//!
//! A test runs `nvx-rs --root ROOT` in a fresh [`Root`]: a repository checkout with the build
//! provenance that names its runtime identity, a distro layer, a scratch image, and directories and
//! a file to share, whose OpenVMM is the fake OpenVMM, `nvx-fake-openvmm`. The shares belong to a
//! user other than root whoever runs the tests, as `--mount-owner caller` requires of a share.
//!
//! The scenario tests check what callers of `nvx-rs` rely on: how a run ended, as an [`Outcome`]
//! reports it, with its exit status and its errors, by their codes and by stable fragments of their
//! messages, such as option names, values, and paths; the files below the root and their POSIX
//! modes, as a [`Snapshot`] records them, and the records of state directories, parsed; and the
//! arguments that OpenVMM received, which the fake OpenVMM records, or that the `>> ` line of a run
//! shows, split as the host splits a command line.
//!
//! An error reads `error[CODE]: MESSAGE`, and a usage error `PROG: error[CODE]: MESSAGE` after the
//! usage of its parser: callers tell errors apart by their codes, and by stable fragments of their
//! messages, whose wording may change (microsoft/nvx#724).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::Value;

/// The CLI under test.
pub(crate) const NVX_RS: &str = env!("CARGO_BIN_EXE_nvx-rs");
/// The fake OpenVMM, which every root holds as its OpenVMM executable.
pub(crate) const FAKE_OPENVMM: &str = env!("CARGO_BIN_EXE_nvx-fake-openvmm");
/// The UUID of the distro layer of the sandboxes of the tests.
pub(crate) const UUID: &str = "11111111-1111-1111-1111-111111111111";
/// The `--layer` of the sandboxes of the tests: the distro layer of the root.
pub(crate) const LAYER: &str = "distro,{root/distro.erofs},11111111-1111-1111-1111-111111111111";
/// The `--scratch` of the sandboxes of the tests: the scratch image of the root.
pub(crate) const SCRATCH: &str = "{root/scratch.ext4}";
/// The hypervisor that `--hypervisor auto`, the default, selects on this host.
pub(crate) const HYPERVISOR: &str = if cfg!(windows) { "whp" } else { "kvm" };
/// The OpenVMM executable of a root, below it.
pub(crate) const OPENVMM: &str = "openvmm/target/release/openvmm{exe}";
/// How the lines that `nvx-rs` prints end on this host, as Python's `print` ends them.
pub(crate) const LINE_END: &str = if cfg!(windows) { "\r\n" } else { "\n" };

/// The hosts on which a case applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hosts {
    /// Every host.
    Every,
    /// POSIX hosts alone.
    Posix,
    /// Windows hosts alone.
    Windows,
}

impl Hosts {
    /// Returns whether the case applies on this host.
    pub(crate) fn include_this_host(self) -> bool {
        match self {
            Self::Every => true,
            Self::Posix => !cfg!(windows),
            Self::Windows => cfg!(windows),
        }
    }
}

/// The build provenance of a root, in its `build/` directory: each file, the member that names
/// the SHA-256 digest of its artifact, and the digest, which a start records in the runtime
/// identity of its VM.
pub(crate) const PROVENANCE: [(&str, &str, &str); 3] = [
    (
        "openvmm.provenance.json",
        "executable_sha256",
        "1e8c924cdaac227bf74b4a4a9bcdb9e8836a1355fd0408cc81dab5d9aa71eb4d",
    ),
    (
        "vmlinux.provenance.json",
        "kernel_sha256",
        "a0c936696eb7d5ee3192bf53b9d281cecbb40ca9db520de72cb95817ad92ac72",
    ),
    (
        "initramfs.provenance.json",
        "initramfs_sha256",
        "2f77c2c97621e8bf69794e9d12815bb7c46338638f126bea8b18dfe02a684d62",
    ),
];

/// The other files of a root, by their paths below it, and their contents: the guest kernel and
/// Alpine initramfs of a checkout, the layers of the sandboxes, of which only the distro layer
/// has a role, their scratch image, and the directories and the file that they share.
const FILES: [(&str, &str); 12] = [
    ("build/vmlinux", "kernel\n"),
    ("build/initramfs.cpio.gz", "initramfs\n"),
    ("distro.erofs", "distro\n"),
    ("runtime.erofs", "runtime\n"),
    ("custom.erofs", "custom\n"),
    ("scratch.ext4", "scratch\n"),
    ("share/file.txt", "file\n"),
    ("share/sub/data.txt", "data\n"),
    ("share/hidden/shown/data.txt", "data\n"),
    ("share/writable/data.txt", "data\n"),
    ("share2/data.txt", "data\n"),
    ("single.txt", "single\n"),
];

/// The empty directories of a root: the one that receives outcome reports.
const DIRECTORIES: [&str; 1] = ["reports"];

/// The directories and the file of a root that the tests share, below it.
#[cfg(unix)]
const SHARES: [&str; 3] = ["share", "share2", "single.txt"];

/// The UID and GID that own the shares of a root when the tests run as root, which owns the rest
/// of the root: `--mount-owner caller` squashes guest root to the owner of a share, so
/// `nvx-rs sandbox` refuses a share that UID 0 or GID 0 owns, and OpenVMM refuses one at launch.
/// It stands for the user that runs the tests unprivileged, who then owns the whole root, and so
/// is neither root nor `nobody` (65534), as whom sandboxes run their workloads by default.
#[cfg(unix)]
const SHARE_OWNER: u32 = 65533;

/// The largest file whose content a [`Snapshot`] digests; it digests the length and modification
/// time of a larger one, such as a copy of the fake OpenVMM.
const DIGESTED_SIZE: u64 = 2 << 20;

/// How many cases of a table [`in_parallel`] checks at once.
const PARALLELISM: usize = 4;

/// Checks each of `cases` with `check`, in [`PARALLELISM`] threads that share them, and fails as
/// a check that fails, if any.
pub(crate) fn in_parallel<T: Sync>(cases: &[T], check: impl Fn(&T) + Sync) {
    let size = cases.len().div_ceil(PARALLELISM).max(1);
    thread::scope(|scope| {
        let handles: Vec<_> = cases
            .chunks(size)
            .map(|chunk| scope.spawn(|| chunk.iter().for_each(&check)))
            .collect();
        for handle in handles {
            if let Err(panic) = handle.join() {
                std::panic::resume_unwind(panic);
            }
        }
    });
}

/// A fresh artifact root in a temporary directory, removed on drop after the fake OpenVMM
/// processes that the runtime records and launch journals below it name end, so that a test
/// that fails leaves none running.
pub(crate) struct Root {
    /// The temporary directory.
    _directory: tempfile::TempDir,
    /// Its canonical path, without the `\\?\` prefix that Windows adds.
    pub(crate) path: PathBuf,
}

impl Root {
    /// Returns a fresh root that holds the build provenance, the files, the directories, and the
    /// OpenVMM of a test root, whose shares belong to a user other than root.
    pub(crate) fn new() -> Self {
        let root = Self::empty();
        for (name, member, digest) in PROVENANCE {
            root.write(
                &format!("build/{name}"),
                format!("{{\n  \"format\": 1,\n  \"{member}\": \"{digest}\"\n}}\n"),
            );
        }
        for (relative, text) in FILES {
            root.write(relative, text);
        }
        for relative in DIRECTORIES {
            fs::create_dir_all(root.below(relative)).unwrap();
        }
        root.install_openvmm(OPENVMM);
        #[cfg(unix)]
        root.give_shares_away();
        root
    }

    /// Gives the shares of the root, with all that they hold, to [`SHARE_OWNER`] when the tests
    /// run as root, so that they belong to a user other than root whoever runs the tests.
    #[cfg(unix)]
    fn give_shares_away(&self) {
        use std::os::unix::fs::lchown;

        if !nix::unistd::geteuid().is_root() {
            return;
        }
        let mut pending: Vec<PathBuf> = SHARES.iter().map(|share| self.below(share)).collect();
        while let Some(path) = pending.pop() {
            if let Err(error) = lchown(&path, Some(SHARE_OWNER), Some(SHARE_OWNER)) {
                panic!(
                    "cannot give {} to UID and GID {SHARE_OWNER}: {error}",
                    path.display()
                );
            }
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                pending.extend(
                    fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
            }
        }
    }

    /// Returns a fresh, empty root.
    fn empty() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("nvx-cli-")
            .tempdir()
            .unwrap();
        let path = canonical(directory.path());
        let text = path.to_str().expect("the temporary directory is Unicode");
        // `--layer` and `--mount` separate their fields with commas.
        assert!(
            !text.contains(','),
            "the CLI tests need a temporary directory without commas, not {text}; set TMPDIR or \
             TEMP"
        );
        Self {
            _directory: directory,
            path,
        }
    }

    /// Returns the root as text.
    pub(crate) fn text(&self) -> &str {
        self.path.to_str().unwrap()
    }

    /// Returns the path of `relative`, with `/` separators and `{exe}` for the suffix of
    /// executables, below the root.
    pub(crate) fn below(&self, relative: &str) -> PathBuf {
        relative
            .replace("{exe}", std::env::consts::EXE_SUFFIX)
            .split('/')
            .filter(|part| !part.is_empty())
            .fold(self.path.clone(), |path, part| path.join(part))
    }

    /// Replaces the placeholders of `text`: `{exe}` with the suffix of executables, `{root}` with
    /// the root, and `{root/RELATIVE}` with the path of `RELATIVE` below it, with the separators
    /// of the host. `{root:?}` and `{root/RELATIVE:?}` quote that path as Rust's `Debug` quotes
    /// it, which escapes the backslashes of Windows paths, as the messages of `nvx-rs` that name
    /// a path in double quotes quote it.
    pub(crate) fn expand(&self, text: &str) -> String {
        let text = text.replace("{exe}", std::env::consts::EXE_SUFFIX);
        let mut output = String::new();
        let mut rest = text.as_str();
        while let Some(start) = rest.find("{root") {
            output.push_str(&rest[..start]);
            let tail = &rest[start + "{root".len()..];
            let end = tail.find('}').expect("a placeholder ends with }");
            let (placeholder, quoted) = match tail[..end].strip_suffix(":?") {
                Some(placeholder) => (placeholder, true),
                None => (&tail[..end], false),
            };
            let path = match placeholder {
                "" => self.path.clone(),
                relative => self.below(
                    relative
                        .strip_prefix('/')
                        .unwrap_or_else(|| panic!("invalid placeholder in {text}")),
                ),
            };
            if quoted {
                output.push_str(&format!("{path:?}"));
            } else {
                output.push_str(path.to_str().unwrap());
            }
            rest = &tail[end + 1..];
        }
        output.push_str(rest);
        output
    }

    /// Returns `value` with the placeholders of each of its strings replaced, as
    /// [`expand`](Self::expand) replaces them.
    pub(crate) fn expand_json(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.expand(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.expand_json(item)).collect())
            }
            Value::Object(members) => Value::Object(
                members
                    .iter()
                    .map(|(name, member)| (name.clone(), self.expand_json(member)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// Writes `data` to the file `relative` below the root, with its missing parents.
    pub(crate) fn write(&self, relative: &str, data: impl AsRef<[u8]>) {
        let path = self.below(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    /// Writes `content` to the file `relative` below the root, with its missing parents.
    pub(crate) fn write_content(&self, relative: &str, content: Content) {
        match content {
            Content::Text(text) => self.write(relative, text),
            Content::Bytes(bytes) => self.write(relative, bytes),
            Content::Padded(text, length) => {
                let mut data = text.as_bytes().to_vec();
                data.resize(length, b' ');
                self.write(relative, data);
            }
        }
    }

    /// Removes the file `relative` below the root.
    pub(crate) fn remove(&self, relative: &str) {
        fs::remove_file(self.below(relative)).unwrap();
    }

    /// Makes the file `relative` below the root the fake OpenVMM: a link to it on POSIX hosts,
    /// and a copy on Windows, which runs no link to an executable.
    pub(crate) fn install_openvmm(&self, relative: &str) {
        let path = self.below(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Concurrent forks must not inherit a writable executable that a test wrote.
        #[cfg(unix)]
        std::os::unix::fs::symlink(FAKE_OPENVMM, &path).unwrap();
        #[cfg(windows)]
        fs::copy(FAKE_OPENVMM, &path).unwrap();
    }

    /// Returns the command that runs `nvx-rs --root ROOT ARGUMENTS` in the root, without the
    /// variables that change its output, its OpenVMM, or whether it downloads a release, and with
    /// no standard input.
    pub(crate) fn command<S: AsRef<OsStr>>(&self, arguments: &[S]) -> Command {
        let mut command = Command::new(NVX_RS);
        command
            .arg("--root")
            .arg(&self.path)
            .args(arguments)
            .current_dir(&self.path)
            .env_remove("NVX_OPENVMM")
            .env_remove("NVX_AUTO_DOWNLOAD")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("COLUMNS")
            .env_remove("LINES")
            .stdin(Stdio::null());
        command
    }

    /// Runs `nvx-rs --root ROOT ARGUMENTS` in the root, with the placeholders of each argument
    /// replaced, and returns how it ended.
    pub(crate) fn nvx_rs<S: AsRef<str>>(&self, arguments: &[S]) -> Outcome {
        self.nvx_rs_with(arguments, &[])
    }

    /// Runs `nvx-rs --root ROOT ARGUMENTS` in the root, with the variables of `environment`, and
    /// with the placeholders of each argument and value replaced, and returns how it ended.
    pub(crate) fn nvx_rs_with<S: AsRef<str>>(
        &self,
        arguments: &[S],
        environment: &[(&str, &str)],
    ) -> Outcome {
        let arguments: Vec<String> = arguments
            .iter()
            .map(|argument| self.expand(argument.as_ref()))
            .collect();
        let output = self
            .command(&arguments)
            .envs(
                environment
                    .iter()
                    .map(|(name, value)| (*name, self.expand(value))),
            )
            .output()
            .unwrap();
        Outcome::new(arguments, output)
    }

    /// Returns what the tree below the root holds.
    pub(crate) fn snapshot(&self) -> Snapshot {
        let mut snapshot = Snapshot::new();
        let mut pending = vec![(self.path.clone(), String::new())];
        while let Some((directory, prefix)) = pending.pop() {
            for entry in fs::read_dir(&directory).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                let relative = format!("{prefix}{name}");
                let node = Node::of(&entry.path());
                if matches!(node, Node::Directory { .. }) {
                    pending.push((entry.path(), format!("{relative}/")));
                }
                snapshot.insert(relative, node);
            }
        }
        snapshot
    }

    /// Returns the records that the fake OpenVMM keeps below the root, which it writes once it
    /// has accepted its command line: the files whose names start with `fake-openvmm-`, by their
    /// paths below the root.
    pub(crate) fn openvmm_records(&self) -> Vec<String> {
        self.snapshot()
            .into_keys()
            .filter(|relative| {
                relative
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with("fake-openvmm-"))
            })
            .collect()
    }

    /// Returns the arguments that the fake OpenVMM received, as it records them in the file
    /// `relative` below the root once it has accepted them: `fake-openvmm-TOKEN.json` next to the
    /// kernel, or next to the snapshot of a restore, where `TOKEN` is the name of the directory of
    /// the launch's outcome report, if it has one.
    pub(crate) fn openvmm_arguments(&self, relative: &str) -> Vec<String> {
        serde_json::from_value(self.json(relative)).unwrap()
    }

    /// Returns the JSON document of the file `relative` below the root.
    pub(crate) fn json(&self, relative: &str) -> Value {
        json(&self.below(relative))
    }

    /// Ends the fake OpenVMM processes that the runtime records and launch journals below the
    /// root name.
    fn end_leftovers(&self) {
        let mut pending = vec![self.path.clone()];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if (entry.file_name() == "runtime.json"
                    || entry.file_name() == "launch.json")
                    && let Some(pid) = fs::read_to_string(&path)
                        .ok()
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                        .and_then(|record| record["process"]["pid"].as_u64())
                {
                    kill(pid);
                }
            }
        }
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        self.end_leftovers();
    }
}

/// Kills process `pid`, and gives it time to exit.
#[cfg(unix)]
pub(crate) fn kill(pid: u64) {
    if let Ok(pid) = i32::try_from(pid) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        thread::sleep(Duration::from_millis(200));
    }
}

/// Kills process `pid`, and gives it time to exit.
#[cfg(windows)]
pub(crate) fn kill(pid: u64) {
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    thread::sleep(Duration::from_millis(200));
}

/// Returns the canonical form of `path`, without the `\\?\` prefix that Windows adds and Python's
/// `Path.resolve()` drops.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    let path = fs::canonicalize(path).unwrap();
    let text = path.to_str().unwrap();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

/// Returns the names in the directory at `path`, sorted.
pub(crate) fn entries(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Returns the JSON document of the file at `path`.
pub(crate) fn json(path: &Path) -> Value {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{}: {error}: {text}", path.display()))
}

/// Returns the permission bits of the file at `path`, which it does not follow.
#[cfg(unix)]
pub(crate) fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Returns whether `text` is `length` lowercase hexadecimal digits.
pub(crate) fn is_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Returns the relative path `path`, with `/` separators, with the separators of this host.
pub(crate) fn native(path: &str) -> String {
    path.replace('/', std::path::MAIN_SEPARATOR_STR)
}

/// The content of a file that a case writes below a root.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Content {
    /// Text.
    Text(&'static str),
    /// Bytes.
    Bytes(&'static [u8]),
    /// Text, then spaces up to a length in bytes.
    Padded(&'static str, usize),
}

/// What the tree below a root holds: each path below it, with `/` separators, and what is there.
pub(crate) type Snapshot = BTreeMap<String, Node>;

/// What a path below a root holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    /// A directory.
    Directory {
        /// Its POSIX permission bits, or 0 on Windows.
        mode: u32,
    },
    /// A regular file.
    File {
        /// Its length.
        length: u64,
        /// A digest of its content, or of its length and modification time if it is larger than
        /// [`DIGESTED_SIZE`].
        digest: u64,
        /// Its POSIX permission bits, or 0 on Windows.
        mode: u32,
    },
    /// A symbolic link, and its target.
    Link(PathBuf),
    /// Anything else, such as a socket.
    Other,
}

impl Node {
    /// Returns what `path` holds, without following a link.
    fn of(path: &Path) -> Self {
        let metadata = fs::symlink_metadata(path).unwrap();
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;

            metadata.permissions().mode() & 0o7777
        };
        #[cfg(not(unix))]
        let mode = 0;
        if metadata.is_symlink() {
            Self::Link(fs::read_link(path).unwrap())
        } else if metadata.is_dir() {
            Self::Directory { mode }
        } else if metadata.is_file() {
            let mut hasher = DefaultHasher::new();
            if metadata.len() > DIGESTED_SIZE {
                hasher.write_u64(metadata.len());
                hasher.write(format!("{:?}", metadata.modified().unwrap()).as_bytes());
            } else {
                hasher.write(&fs::read(path).unwrap());
            }
            Self::File {
                length: metadata.len(),
                digest: hasher.finish(),
                mode,
            }
        } else {
            Self::Other
        }
    }
}

/// How a run of `nvx-rs` ended.
#[derive(Debug)]
pub(crate) struct Outcome {
    /// Its arguments after `--root ROOT`, which the messages of failed checks show.
    pub(crate) arguments: Vec<String>,
    /// Its exit status, if it exited.
    pub(crate) status: Option<i32>,
    /// What it wrote to standard output.
    pub(crate) stdout: Vec<u8>,
    /// What it wrote to standard error.
    pub(crate) stderr: Vec<u8>,
}

impl Outcome {
    /// Returns how the run of `nvx-rs` with `arguments` that produced `output` ended.
    pub(crate) fn new(arguments: Vec<String>, output: Output) -> Self {
        Self {
            arguments,
            status: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }

    /// Returns standard output as text.
    pub(crate) fn stdout(&self) -> String {
        String::from_utf8(self.stdout.clone()).expect("standard output is UTF-8")
    }

    /// Returns standard error as text.
    pub(crate) fn stderr(&self) -> String {
        String::from_utf8(self.stderr.clone()).expect("standard error is UTF-8")
    }

    /// Returns a description of the run for the message of a failed check.
    pub(crate) fn describe(&self) -> String {
        format!(
            "nvx-rs {:?} exited with {:?}\nstdout: {}\nstderr: {}",
            self.arguments,
            self.status,
            String::from_utf8_lossy(&self.stdout),
            String::from_utf8_lossy(&self.stderr)
        )
    }

    /// Asserts that the run exited with status 0 and wrote nothing to standard error.
    pub(crate) fn assert_success(&self) {
        assert!(
            self.status == Some(0) && self.stderr.is_empty(),
            "{}",
            self.describe()
        );
    }

    /// Asserts that the run exited with status 0 and wrote nothing at all.
    pub(crate) fn assert_silent_success(&self) {
        self.assert_success();
        assert!(self.stdout.is_empty(), "{}", self.describe());
    }

    /// Asserts that the run exited with status 1, wrote nothing to standard output, and wrote
    /// one error to standard error, `error[CODE]: MESSAGE`, with the code `code` and a message that
    /// holds each of `fragments`, and returns the message.
    pub(crate) fn assert_error(&self, code: &str, fragments: &[&str]) -> String {
        assert_eq!(self.status, Some(1), "{}", self.describe());
        assert!(self.stdout.is_empty(), "{}", self.describe());
        let stderr = self.stderr();
        let line = stderr
            .strip_suffix(LINE_END)
            .filter(|line| !line.contains('\n'))
            .unwrap_or_else(|| panic!("not one line: {}", self.describe()));
        let message = self.reported(line, "", code);
        self.assert_fragments(message, fragments);
        message.to_owned()
    }

    /// Asserts that the run exited with status 2, wrote nothing to standard output, and wrote to
    /// standard error the usage of a parser of `nvx-rs`, then one usage error,
    /// `PROG: error[CODE]: MESSAGE`, with the code `code` and a message that holds each of
    /// `fragments`, and returns the message.
    pub(crate) fn assert_usage_error(&self, code: &str, fragments: &[&str]) -> String {
        assert_eq!(self.status, Some(2), "{}", self.describe());
        assert!(self.stdout.is_empty(), "{}", self.describe());
        let stderr = self.stderr();
        assert!(stderr.starts_with("usage: nvx-rs"), "{}", self.describe());
        let line = stderr
            .strip_suffix(LINE_END)
            .and_then(|text| text.rsplit(LINE_END).next())
            .unwrap_or_else(|| panic!("no usage error: {}", self.describe()));
        let (program, _) = line
            .split_once(": error[")
            .unwrap_or_else(|| panic!("no usage error: {}", self.describe()));
        assert!(program.starts_with("nvx-rs"), "{}", self.describe());
        let message = self.reported(line, &format!("{program}: "), code);
        self.assert_fragments(message, fragments);
        message.to_owned()
    }

    /// Asserts that `line`, which the run wrote, reports an error with the code `code` after
    /// `prefix`, and returns its message.
    fn reported<'a>(&self, line: &'a str, prefix: &str, code: &str) -> &'a str {
        let (reported, message) = reported_error(line, prefix)
            .unwrap_or_else(|| panic!("not an error: {}", self.describe()));
        assert_eq!(reported, code, "{}", self.describe());
        message
    }

    /// Asserts that `message`, an error of the run, holds each of `fragments`.
    fn assert_fragments(&self, message: &str, fragments: &[&str]) {
        for fragment in fragments {
            assert!(
                message.contains(fragment),
                "the error lacks {fragment:?}: {}",
                self.describe()
            );
        }
    }

    /// Returns the OpenVMM command that standard output starts with, as `>> COMMAND`, split into
    /// its arguments as this host splits a command line, and the output that follows its line.
    pub(crate) fn command(&self) -> (Vec<String>, &[u8]) {
        let rest = self
            .stdout
            .strip_prefix(b">> ")
            .unwrap_or_else(|| panic!("no >> line: {}", self.describe()));
        split_command(rest)
    }
}

/// Returns the code and the message of the error that `line` reports after `prefix`,
/// `PREFIXerror[CODE]: MESSAGE`, whose code is lowercase ASCII words joined by hyphens.
fn reported_error<'a>(line: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let rest = line.strip_prefix(prefix)?.strip_prefix("error[")?;
    let (code, message) = rest.split_once("]: ")?;
    let words = code
        .split('-')
        .all(|word| !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_lowercase()));
    words.then_some((code, message))
}

/// Splits the command line that `text` starts with as a POSIX shell splits it into words, up to
/// the newline that ends it outside quotes, and returns the words and what follows its line.
///
/// Spaces and tabs separate words. A single-quoted part is literal. In a double-quoted part, a
/// backslash escapes `$`, `` ` ``, `"`, and `\`, removes a newline, and is literal before any
/// other character; outside quotes, it escapes any character and removes a newline.
#[cfg(not(windows))]
pub(crate) fn split_command(text: &[u8]) -> (Vec<String>, &[u8]) {
    let mut words = Vec::new();
    let mut word: Option<Vec<u8>> = None;
    let mut index = 0;
    let next = |index: &mut usize| -> u8 {
        let byte = *text
            .get(*index)
            .unwrap_or_else(|| panic!("the command line does not end: {text:?}"));
        *index += 1;
        byte
    };
    loop {
        match next(&mut index) {
            b'\n' => break,
            b' ' | b'\t' => words.extend(word.take()),
            b'\'' => {
                let word = word.get_or_insert_with(Vec::new);
                loop {
                    match next(&mut index) {
                        b'\'' => break,
                        byte => word.push(byte),
                    }
                }
            }
            b'"' => {
                let word = word.get_or_insert_with(Vec::new);
                loop {
                    match next(&mut index) {
                        b'"' => break,
                        b'\\' => match next(&mut index) {
                            b'\n' => {}
                            byte @ (b'$' | b'`' | b'"' | b'\\') => word.push(byte),
                            byte => word.extend([b'\\', byte]),
                        },
                        byte => word.push(byte),
                    }
                }
            }
            b'\\' => match next(&mut index) {
                b'\n' => {}
                byte => word.get_or_insert_with(Vec::new).push(byte),
            },
            byte => word.get_or_insert_with(Vec::new).push(byte),
        }
    }
    words.extend(word);
    let words = words
        .into_iter()
        .map(|word| String::from_utf8(word).expect("the command line is UTF-8"))
        .collect();
    (words, &text[index..])
}

/// Splits the command line that `text` starts with as `CommandLineToArgvW` splits it, up to the
/// `\r\n` that ends its line, and returns the words and what follows its line.
#[cfg(windows)]
pub(crate) fn split_command(text: &[u8]) -> (Vec<String>, &[u8]) {
    let end = text
        .windows(2)
        .position(|pair| pair == b"\r\n")
        .unwrap_or_else(|| panic!("the command line does not end: {text:?}"));
    let line = std::str::from_utf8(&text[..end]).expect("the command line is UTF-8");
    (command_line_to_argv(line), &text[end + 2..])
}

/// Splits `line` into its arguments with `CommandLineToArgvW`, as the C runtime of a program
/// splits the command line that starts it.
#[cfg(windows)]
pub(crate) fn command_line_to_argv(line: &str) -> Vec<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    let wide: Vec<u16> = line.encode_utf16().chain([0]).collect();
    let mut count = 0;
    // SAFETY: the line is a NUL-terminated wide string, and the count is a valid output.
    let arguments = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut count) };
    assert!(!arguments.is_null(), "{}", std::io::Error::last_os_error());
    let words = (0..usize::try_from(count).unwrap())
        .map(|index| {
            // SAFETY: CommandLineToArgvW returned an array of `count` NUL-terminated strings.
            let units = unsafe {
                let argument = *arguments.add(index);
                let length = (0..).take_while(|&unit| *argument.add(unit) != 0).count();
                std::slice::from_raw_parts(argument, length)
            };
            String::from_utf16(units).unwrap()
        })
        .collect();
    // SAFETY: the array is CommandLineToArgvW's, and nothing refers to it any more.
    unsafe { LocalFree(arguments.cast()) };
    words
}
