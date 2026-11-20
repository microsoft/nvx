//! The fixed launch journal and the caller-saved VM's runtime record.

use std::fmt;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::StateFile;
use super::files;
use super::locked::LockedState;
use crate::host::platform;
use crate::host::process::ProcessRef;
use crate::host::runtime::{GuestArchitecture, RuntimeIdentity};
use crate::host::vm::{Prepared, SavedVm, VmDir, VmOutputs};
use crate::protocol::CAPABILITY_LEN;
use crate::{Error, Result};

#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "LaunchJournalInput")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LaunchJournal {
    pub(super) version: u8,
    pub(super) architecture: GuestArchitecture,
    pub(super) vm_dir: PathBuf,
    pub(super) capability: [u8; CAPABILITY_LEN],
    pub(super) log: PathBuf,
    pub(super) log_identity: (u64, u64),
    pub(super) outcome: PathBuf,
    pub(super) endpoint: String,
    pub(super) console: Option<String>,
    pub(super) runtime: RuntimeIdentity,
    pub(super) process: Option<ProcessRef>,
}

impl fmt::Debug for LaunchJournal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchJournal")
            .field("architecture", &self.architecture)
            .field("vm_dir", &self.vm_dir)
            .field("log", &self.log)
            .field("log_identity", &self.log_identity)
            .field("outcome", &self.outcome)
            .field("endpoint", &self.endpoint)
            .field("console", &self.console)
            .field("runtime", &self.runtime)
            .field("process", &self.process)
            .finish_non_exhaustive()
    }
}

impl LaunchJournal {
    pub(super) fn prepared(
        state: &LockedState,
        prepared: &Prepared,
        runtime: &RuntimeIdentity,
    ) -> Result<Self> {
        let vm_dir = VmDir::open(state.dir())?;
        let VmOutputs {
            log,
            outcome,
            endpoint,
            console,
        } = prepared.outputs();
        if *log != vm_dir.path().join(StateFile::Log.name())
            || *outcome != vm_dir.path().join(StateFile::Outcome.name())
        {
            return Err(Error::other(
                "the prepared launch belongs to another sandbox directory",
            ));
        }
        platform::claim_log(prepared.log())
            .map_err(|error| Error::io("cannot claim the prepared VM log").with_source(error))?;
        let log_identity = platform::opened_file_identity(prepared.log())
            .map_err(|error| Error::io("cannot identify the prepared VM log").with_source(error))?;
        Ok(Self {
            version: 2,
            architecture: prepared.architecture(),
            vm_dir: vm_dir.path().to_path_buf(),
            capability: *prepared.capability().as_bytes(),
            log: log.clone(),
            log_identity,
            outcome: outcome.clone(),
            endpoint: endpoint.clone(),
            console: console.clone(),
            runtime: runtime.clone(),
            process: None,
        })
    }

    pub(crate) fn process(&self) -> Option<&ProcessRef> {
        self.process.as_ref()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(try_from = "RuntimeRecordInput")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RuntimeRecord {
    version: u8,
    architecture: GuestArchitecture,
    vm_dir: PathBuf,
    endpoint: String,
    capability: [u8; CAPABILITY_LEN],
    process: ProcessRef,
    runtime: RuntimeIdentity,
}

impl From<&SavedVm> for RuntimeRecord {
    fn from(saved: &SavedVm) -> Self {
        Self {
            version: 2,
            architecture: saved.architecture,
            vm_dir: saved.vm_dir.path().to_path_buf(),
            endpoint: saved.endpoint.clone(),
            capability: saved.capability,
            process: saved.process.clone(),
            runtime: saved.runtime.clone(),
        }
    }
}

impl LockedState {
    pub(crate) fn journal(&self) -> Result<Option<LaunchJournal>> {
        self.read_record(StateFile::Journal)
    }

    pub(crate) fn runtime(&self) -> Result<Option<SavedVm>> {
        let Some(record) = self.read_record::<RuntimeRecord>(StateFile::Runtime)? else {
            return Ok(None);
        };
        let vm_dir = VmDir::open(self.dir())?;
        if record.vm_dir != vm_dir.path() {
            return Err(Error::io(
                "the runtime record belongs to another sandbox directory",
            ));
        }
        Ok(Some(SavedVm {
            architecture: record.architecture,
            vm_dir,
            endpoint: record.endpoint,
            capability: record.capability,
            process: record.process,
            runtime: record.runtime,
        }))
    }
}

/// Decodes only the old CLI's exact unversioned schema and producer contract, or the new
/// explicitly versioned form. A new-format record never gets an implicit architecture.
fn recorded_architecture(
    version: Option<u8>,
    architecture: Option<GuestArchitecture>,
    runtime: &RuntimeIdentity,
) -> std::result::Result<GuestArchitecture, String> {
    match (version, architecture) {
        (Some(2), Some(architecture)) => Ok(architecture),
        (None, None)
            if runtime.compatibility.as_str()
                == "nvx-boot-v5+nvxs-v1+nvx-microvm-v2-control-v2" =>
        {
            // This CLI record schema predates guest-ISA support and its v5 producers were x86-only.
            Ok(GuestArchitecture::X86_64)
        }
        _ => Err(
            "unrecognized VM launch metadata: version 2 requires an explicit guest architecture; \
                 only the legacy x86 CLI v5 record can omit both"
                .to_owned(),
        ),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuntimeRecordInput {
    #[serde(default, deserialize_with = "present_metadata")]
    version: Option<u8>,
    #[serde(default, deserialize_with = "present_metadata")]
    architecture: Option<GuestArchitecture>,
    vm_dir: PathBuf,
    endpoint: String,
    capability: [u8; CAPABILITY_LEN],
    process: ProcessRef,
    runtime: RuntimeIdentity,
}

impl TryFrom<RuntimeRecordInput> for RuntimeRecord {
    type Error = String;

    fn try_from(record: RuntimeRecordInput) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            version: 2,
            architecture: recorded_architecture(
                record.version,
                record.architecture,
                &record.runtime,
            )?,
            vm_dir: record.vm_dir,
            endpoint: record.endpoint,
            capability: record.capability,
            process: record.process,
            runtime: record.runtime,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LaunchJournalInput {
    #[serde(default, deserialize_with = "present_metadata")]
    version: Option<u8>,
    #[serde(default, deserialize_with = "present_metadata")]
    architecture: Option<GuestArchitecture>,
    vm_dir: PathBuf,
    capability: [u8; CAPABILITY_LEN],
    log: PathBuf,
    log_identity: (u64, u64),
    outcome: PathBuf,
    endpoint: String,
    console: Option<String>,
    runtime: RuntimeIdentity,
    process: Option<ProcessRef>,
}

/// An absent legacy field differs from a present null, which is invalid new metadata.
fn present_metadata<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

impl TryFrom<LaunchJournalInput> for LaunchJournal {
    type Error = String;

    fn try_from(record: LaunchJournalInput) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            version: 2,
            architecture: recorded_architecture(
                record.version,
                record.architecture,
                &record.runtime,
            )?,
            vm_dir: record.vm_dir,
            capability: record.capability,
            log: record.log,
            log_identity: record.log_identity,
            outcome: record.outcome,
            endpoint: record.endpoint,
            console: record.console,
            runtime: record.runtime,
            process: record.process,
        })
    }
}

#[cfg(test)]
mod tests;

impl LockedState {
    pub(crate) fn capability(&self) -> Result<Option<[u8; CAPABILITY_LEN]>> {
        files::read(self, StateFile::Capability)?
            .map(|bytes| {
                bytes.try_into().map_err(|_| {
                    Error::io(format!(
                        "the sandbox capability must hold {CAPABILITY_LEN} bytes"
                    ))
                })
            })
            .transpose()
    }

    fn read_record<T: DeserializeOwned>(&self, name: StateFile) -> Result<Option<T>> {
        files::read(self, name)?
            .map(|bytes| {
                serde_json::from_slice(&bytes).map_err(|error| {
                    Error::io(format!("cannot parse {}", self.path(name).display()))
                        .with_source(error)
                })
            })
            .transpose()
    }

    pub(super) fn write_record(&self, name: StateFile, record: &impl Serialize) -> Result<()> {
        let bytes = serde_json::to_vec(record).map_err(|error| {
            Error::io(format!("cannot serialize {}", self.path(name).display())).with_source(error)
        })?;
        files::replace(self, name, &bytes)
    }
}
