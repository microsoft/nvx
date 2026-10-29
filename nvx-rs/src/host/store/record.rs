//! The records of the store: the runtime record of a running VM and the journal of a launch, with
//! the identities that they hold, and their reading and writing in format [`FORMAT`].

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::FORMAT;
use super::files::StoreFile;
use super::identity::{IdentityCheck, RuntimeIdentity};
use crate::error::{Error, ErrorCode, Result};
use crate::host::filesystem::FileIdentity;
use crate::host::process::ProcessRef;

/// Longest control endpoint, in bytes.
pub const MAX_ENDPOINT_LEN: usize = 4096;

/// Random bytes in a [`LaunchId`]: 128 bits, so that no two launches draw the same identity.
const LAUNCH_ID_BYTES: usize = 16;

/// Hexadecimal digits of a [`LaunchId`]: two for each of its bytes.
const LAUNCH_ID_LEN: usize = 2 * LAUNCH_ID_BYTES;

/// Returns `bytes` as lowercase hexadecimal digits.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Returns whether `text` is exactly [`LAUNCH_ID_LEN`] lowercase hexadecimal digits.
fn is_hex_id(text: &str) -> bool {
    text.len() == LAUNCH_ID_LEN
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Identity of one launch: 32 lowercase hexadecimal digits, the 16 random bytes that
/// [`generate`](Self::generate) draws for every launch, and which its journal and runtime record
/// share.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LaunchId(String);

impl LaunchId {
    /// Draws a fresh launch identity, which a caller reserves for one launch before it begins it
    /// with [`LockedStore::begin_launch`](super::LockedStore::begin_launch), or
    /// [`supervisor::launch`](crate::host::supervisor::launch), and never uses for another. A
    /// clone's lease reserves one, as
    /// [`template::Store::acquire`](crate::host::template::Store::acquire) draws it.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; LAUNCH_ID_BYTES];
        getrandom::fill(&mut bytes)
            .map_err(|error| Error::other("cannot draw a launch identity").with_source(error))?;
        Ok(Self(hex(&bytes)))
    }

    /// Returns the identity's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LaunchId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for LaunchId {
    type Error = String;

    fn try_from(text: String) -> std::result::Result<Self, String> {
        if is_hex_id(&text) {
            Ok(Self(text))
        } else {
            Err(format!(
                "launch identity {text:?} is not {LAUNCH_ID_LEN} lowercase hexadecimal digits"
            ))
        }
    }
}

impl From<LaunchId> for String {
    fn from(launch: LaunchId) -> Self {
        launch.0
    }
}

/// Explains why the identity of a VMM process is invalid, if it is.
fn process_problem(process: &ProcessRef) -> Option<String> {
    (process.pid == 0).then(|| "its process ID is 0".to_owned())
}

/// The claim of a launch on the log of its VMM: the identity of the log file, which the launch
/// creates afresh, and which the VMM holds as its standard output and error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogClaim {
    /// The device and inode of the log, or its volume serial number and file index.
    pub identity: FileIdentity,
}

/// Explains why a control endpoint is invalid, if it is.
fn endpoint_problem(endpoint: &str) -> Option<String> {
    if endpoint.is_empty() || endpoint.len() > MAX_ENDPOINT_LEN || endpoint.contains('\0') {
        Some(format!(
            "its control endpoint must hold 1 to {MAX_ENDPOINT_LEN} bytes and no NUL"
        ))
    } else {
        None
    }
}

/// Explains why a runtime identity is invalid, if it is.
fn runtime_problem(runtime: &RuntimeIdentity) -> Option<String> {
    runtime
        .check()
        .err()
        .map(|error| error.message().to_owned())
}

/// The runtime record of a sandbox whose VM runs: the verified process reference of its VMM, its
/// control endpoint, and the runtime identity that it was launched with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeRecord {
    /// [`FORMAT`].
    format: u32,
    /// The launch that started the VM.
    launch: LaunchId,
    /// The VMM process.
    process: ProcessRef,
    /// The control endpoint: a Unix socket path on Linux, and a named pipe on Windows.
    endpoint: String,
    /// The runtime that the VM was launched with.
    runtime: RuntimeIdentity,
}

impl RuntimeRecord {
    /// Records the VM that the launch of `journal` started, once it has recorded its process.
    pub(super) fn committed(journal: &LaunchJournal, process: ProcessRef) -> Self {
        Self {
            format: FORMAT,
            launch: journal.launch.clone(),
            process,
            endpoint: journal.endpoint.clone(),
            runtime: journal.runtime.clone(),
        }
    }

    /// Returns the launch that started the VM.
    pub fn launch(&self) -> &LaunchId {
        &self.launch
    }

    /// Returns the reference to the VMM process, which [`Process::open`] verifies before anything
    /// waits for the process or signals it.
    ///
    /// [`Process::open`]: crate::host::process::Process::open
    pub fn process(&self) -> &ProcessRef {
        &self.process
    }

    /// Returns the control endpoint: a Unix socket path on Linux, and a named pipe on Windows.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the runtime identity that the VM was launched with.
    pub fn runtime(&self) -> &RuntimeIdentity {
        &self.runtime
    }

    /// Compares the runtime identity that the VM was launched with with `current`, the identity
    /// of the runtime that the host would now launch.
    ///
    /// A match lets the host reattach to the VM and keep it running, whatever else changed in
    /// the host, such as the build of its own code. A mismatch means that the VM must be stopped,
    /// and only through its verified process identity.
    pub fn check_identity(&self, current: &RuntimeIdentity) -> IdentityCheck {
        let differences = self.runtime.differences(current);
        if differences.is_empty() {
            IdentityCheck::Matches
        } else {
            IdentityCheck::Mismatched(differences)
        }
    }
}

/// The step that a launch has reached, as its journal records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchState {
    /// The launch claimed its log and may have spawned its VMM, whose identity it has not
    /// recorded.
    Prepared,
    /// The launch spawned its VMM and recorded its process identity, and has not committed its
    /// runtime record yet.
    Spawned,
}

/// The journal of a launch in progress, or of one that its caller did not finish.
///
/// A marker written before the VMM is spawned holds the launch's identity, control endpoint,
/// runtime identity, and claim on its log. The VMM's process identity follows right after the
/// spawn. Committing the launch replaces the journal with the runtime record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchJournal {
    /// [`FORMAT`].
    format: u32,
    /// The launch.
    launch: LaunchId,
    /// The control endpoint that the launch chose.
    endpoint: String,
    /// The runtime that the launch starts.
    runtime: RuntimeIdentity,
    /// The launch's claim on its log.
    log: LogClaim,
    /// The VMM process, once spawned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process: Option<ProcessRef>,
}

impl LaunchJournal {
    /// Returns the marker of a launch that is about to spawn its VMM.
    pub(super) fn prepared(
        launch: LaunchId,
        endpoint: String,
        runtime: RuntimeIdentity,
        log: LogClaim,
    ) -> Self {
        Self {
            format: FORMAT,
            launch,
            endpoint,
            runtime,
            log,
            process: None,
        }
    }

    /// Records the VMM process that the launch spawned.
    pub(super) fn spawned(&self, process: ProcessRef) -> Self {
        Self {
            process: Some(process),
            ..self.clone()
        }
    }

    /// Returns the launch.
    pub fn launch(&self) -> &LaunchId {
        &self.launch
    }

    /// Returns the control endpoint that the launch chose.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the runtime identity that the launch starts.
    pub fn runtime(&self) -> &RuntimeIdentity {
        &self.runtime
    }

    /// Returns the launch's claim on its log.
    pub fn log(&self) -> &LogClaim {
        &self.log
    }

    /// Returns the reference to the VMM process, once the launch has recorded it.
    pub fn process(&self) -> Option<&ProcessRef> {
        self.process.as_ref()
    }

    /// Returns the step that the launch has reached.
    pub fn state(&self) -> LaunchState {
        match self.process {
            None => LaunchState::Prepared,
            Some(_) => LaunchState::Spawned,
        }
    }
}

/// A record of the store, in format [`FORMAT`].
pub(super) trait Record: Serialize + DeserializeOwned {
    /// What messages call the record, and the [`StoreFile`] that holds it.
    const DESCRIPTION: &'static str;

    /// Explains why the record, which parsed, is invalid, if it is.
    fn problem(&self) -> Option<String>;
}

impl Record for RuntimeRecord {
    const DESCRIPTION: &'static str = "sandbox runtime record";

    fn problem(&self) -> Option<String> {
        process_problem(&self.process)
            .or_else(|| endpoint_problem(&self.endpoint))
            .or_else(|| runtime_problem(&self.runtime))
    }
}

impl Record for LaunchJournal {
    const DESCRIPTION: &'static str = "sandbox launch journal";

    fn problem(&self) -> Option<String> {
        self.process
            .as_ref()
            .and_then(process_problem)
            .or_else(|| endpoint_problem(&self.endpoint))
            .or_else(|| runtime_problem(&self.runtime))
    }
}

/// Reads the record that `file` holds, or returns `None` if the file does not exist.
///
/// A record of another format fails before its fields are read, with a message that names its
/// format, and the code [`ErrorCode::FormatUnsupported`]: the store reads format [`FORMAT`] only,
/// and migrates no other. A record file that is not private to the current user fails as well,
/// since others could have written it, and so does one that is not a regular file, without waiting
/// on it, as [`StoreFile::read`] reads it. Every failure to read or check the record is an
/// [`ErrorKind::Io`](crate::ErrorKind::Io) error.
pub(super) fn read<T: Record>(file: &StoreFile) -> Result<Option<T>> {
    let (path, description) = (file.path(), file.description());
    let Some(text) = file.read()? else {
        return Ok(None);
    };
    let malformed = || format!("the {description} {} is malformed", path.display());
    let value: serde_json::Value =
        serde_json::from_slice(&text).map_err(|error| Error::io(malformed()).with_source(error))?;
    match value.get("format") {
        Some(format) if format.as_u64() == Some(FORMAT.into()) => {}
        Some(format) => {
            return Err(Error::new(
                ErrorCode::FormatUnsupported,
                format!(
                    "the {description} {} has the unsupported format {format}; this build reads \
                     format {FORMAT} only and migrates no other, so stop the sandbox with the \
                     build that started it",
                    path.display()
                ),
            ));
        }
        None => {
            return Err(Error::io(format!("{}: it has no format", malformed())));
        }
    }
    let record: T =
        serde_json::from_value(value).map_err(|error| Error::io(malformed()).with_source(error))?;
    match record.problem() {
        Some(problem) => Err(Error::io(format!("{}: {problem}", malformed()))),
        None => Ok(Some(record)),
    }
}

/// Atomically replaces the record that `file` holds with `record`, as pretty JSON and a newline.
pub(super) fn write<T: Record>(file: &StoreFile, record: &T) -> Result<()> {
    let mut text = serde_json::to_vec_pretty(record).map_err(|error| {
        Error::other(format!("cannot encode the {}", file.description())).with_source(error)
    })?;
    text.push(b'\n');
    file.replace(&text)
}
