//! Persistent per-sandbox state of the OpenVMM backend.
//!
//! ```text
//! <state_root>/
//!   .locks/<token>.lock       serializes lifecycle transitions of one sandbox
//!   <token>/
//!     sandbox.json            provisioned configuration
//!     launch.json             endpoint and available process identity of a start in progress
//!     runtime.json            OpenVMM process identity and endpoint (running only)
//!     control.capability      launch capability (running only)
//!     control.sock            Linux control endpoint (running only)
//!     openvmm.log             OpenVMM output of the latest start
//!     outcome.json            OpenVMM outcome report of the latest run
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::filesystem::HostMapping;
use super::platform;
use super::protocol::CAPABILITY_LEN;
use crate::error::{Error, Result};
use crate::id::SandboxId;
use crate::model::NetworkPolicy;

/// Version of the state files written by this crate.
///
/// Version 1 described sandboxes assembled from image layers and a scratch disk.
pub(crate) const STATE_FORMAT: u32 = 2;
/// Backend key recorded in every sandbox.
pub(crate) const BACKEND_KEY: &str = "openvmm";
/// Linux control socket name.
pub(crate) const SOCKET_NAME: &str = "control.sock";

const RECORD_NAME: &str = "sandbox.json";
const LAUNCH_NAME: &str = "launch.json";
const RUNTIME_NAME: &str = "runtime.json";
const CAPABILITY_NAME: &str = "control.capability";
const LOG_NAME: &str = "openvmm.log";
const OUTCOME_NAME: &str = "outcome.json";
const LOCKS_NAME: &str = ".locks";

/// Provisioned configuration of one sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SandboxRecord {
    pub(crate) format: u32,
    pub(crate) backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) network: Option<NetworkPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) filesystem: Option<HostMapping>,
    pub(crate) memory_mib: u32,
    pub(crate) workload_uid: u32,
    pub(crate) workload_gid: u32,
    /// Whether the guest creates an account for the workload identity if its image lacks a usable
    /// one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) create_workload_account: bool,
    pub(crate) hostname: String,
}

/// Identity of the OpenVMM process of a running sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RuntimeRecord {
    pub(crate) format: u32,
    pub(crate) pid: u32,
    pub(crate) start_time: u64,
    pub(crate) endpoint: String,
}

/// Marker written before OpenVMM is launched and replaced by the runtime record afterwards.
///
/// Process identity is added as soon as the child is identified, before writing runtime state.
/// A marker without identity is never expired: its child may be alive without an endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LaunchRecord {
    pub(crate) format: u32,
    pub(crate) endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) process: Option<ProcessIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: u32,
    pub(crate) start_time: u64,
}

/// Held advisory lock. Dropping it releases the lock.
#[derive(Debug)]
pub(crate) struct LockGuard {
    _file: File,
}

/// Directory tree that holds every sandbox of one backend instance.
#[derive(Debug)]
pub(crate) struct StateStore {
    root: PathBuf,
}

fn io_error(context: String) -> impl FnOnce(io::Error) -> Error {
    move |error| Error::backend_error(context).with_source(error)
}

impl StateStore {
    /// Opens `root`, creating it and restricting it to the current user if needed.
    pub(crate) fn open(root: &Path) -> io::Result<Self> {
        if let Some(parent) = root.parent() {
            fs::create_dir_all(parent)?;
        }
        platform::create_private_dir(root)?;
        platform::create_private_dir(&root.join(LOCKS_NAME))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    pub(crate) fn dir(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.root.join(sandbox_id.token())
    }

    pub(crate) fn log_path(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.dir(sandbox_id).join(LOG_NAME)
    }

    pub(crate) fn outcome_path(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.dir(sandbox_id).join(OUTCOME_NAME)
    }

    pub(crate) fn socket_path(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.dir(sandbox_id).join(SOCKET_NAME)
    }

    fn lock_path(&self, sandbox_id: &SandboxId) -> PathBuf {
        self.root
            .join(LOCKS_NAME)
            .join(format!("{}.lock", sandbox_id.token()))
    }

    /// Takes the exclusive lifecycle lock of a sandbox.
    pub(crate) fn lock(&self, sandbox_id: &SandboxId) -> Result<LockGuard> {
        lock(&self.lock_path(sandbox_id))
    }

    /// Removes the lock file of a deprovisioned sandbox.
    pub(crate) fn remove_lock(&self, sandbox_id: &SandboxId) {
        let _ = fs::remove_file(self.lock_path(sandbox_id));
    }

    /// Creates the state directory of a new sandbox.
    pub(crate) fn create(&self, sandbox_id: &SandboxId, record: &SandboxRecord) -> Result<()> {
        let dir = self.dir(sandbox_id);
        fs::create_dir(&dir).map_err(io_error(format!(
            "cannot create sandbox state {}",
            dir.display()
        )))?;
        let created = platform::create_private_dir(&dir)
            .map_err(io_error(format!(
                "cannot restrict sandbox state {}",
                dir.display()
            )))
            .and_then(|()| write_json(&dir.join(RECORD_NAME), record));
        if created.is_err() {
            let _ = fs::remove_dir_all(&dir);
        }
        created
    }

    /// Loads the configuration of a provisioned sandbox.
    pub(crate) fn load(&self, sandbox_id: &SandboxId) -> Result<SandboxRecord> {
        let path = self.dir(sandbox_id).join(RECORD_NAME);
        let record: SandboxRecord = match read_json(&path)? {
            Some(record) => record,
            None => {
                return Err(Error::stale_id(format!(
                    "sandbox {sandbox_id} is not provisioned"
                )));
            }
        };
        if record.format != STATE_FORMAT || record.backend != BACKEND_KEY {
            return Err(Error::backend_error(format!(
                "sandbox state {} has an unsupported format",
                path.display()
            )));
        }
        Ok(record)
    }

    pub(crate) fn runtime(&self, sandbox_id: &SandboxId) -> Result<Option<RuntimeRecord>> {
        let path = self.dir(sandbox_id).join(RUNTIME_NAME);
        let runtime: Option<RuntimeRecord> = read_json(&path)?;
        match runtime {
            Some(runtime) if runtime.format != STATE_FORMAT => Err(Error::backend_error(format!(
                "sandbox runtime state {} has an unsupported format",
                path.display()
            ))),
            runtime => Ok(runtime),
        }
    }

    pub(crate) fn write_runtime(
        &self,
        sandbox_id: &SandboxId,
        runtime: &RuntimeRecord,
    ) -> Result<()> {
        write_json(&self.dir(sandbox_id).join(RUNTIME_NAME), runtime)
    }

    pub(crate) fn write_launch(&self, sandbox_id: &SandboxId, launch: &LaunchRecord) -> Result<()> {
        write_json(&self.dir(sandbox_id).join(LAUNCH_NAME), launch)
    }

    pub(crate) fn launch(&self, sandbox_id: &SandboxId) -> Result<Option<LaunchRecord>> {
        let path = self.dir(sandbox_id).join(LAUNCH_NAME);
        let Some(launch) = read_json::<LaunchRecord>(&path)? else {
            return Ok(None);
        };
        if launch.format != STATE_FORMAT {
            return Err(Error::backend_error(format!(
                "sandbox launch marker {} has an unsupported format",
                path.display()
            )));
        }
        Ok(Some(launch))
    }

    /// Removes the launch marker once the runtime record replaces it.
    pub(crate) fn remove_launch(&self, sandbox_id: &SandboxId) {
        let _ = fs::remove_file(self.dir(sandbox_id).join(LAUNCH_NAME));
    }

    pub(crate) fn write_capability(
        &self,
        sandbox_id: &SandboxId,
        capability: &[u8; CAPABILITY_LEN],
    ) -> Result<()> {
        write_private(&self.dir(sandbox_id).join(CAPABILITY_NAME), capability)
    }

    pub(crate) fn read_capability(&self, sandbox_id: &SandboxId) -> Result<[u8; CAPABILITY_LEN]> {
        let path = self.dir(sandbox_id).join(CAPABILITY_NAME);
        let bytes = fs::read(&path).map_err(io_error(format!(
            "cannot read the sandbox control capability {}",
            path.display()
        )))?;
        match <[u8; CAPABILITY_LEN]>::try_from(bytes.as_slice()) {
            Ok(capability) if capability != [0; CAPABILITY_LEN] => Ok(capability),
            _ => Err(Error::backend_error(format!(
                "sandbox control capability {} is invalid",
                path.display()
            ))),
        }
    }

    /// Removes the files that exist only while the sandbox starts or runs.
    pub(crate) fn clear_runtime(&self, sandbox_id: &SandboxId) -> Result<()> {
        let dir = self.dir(sandbox_id);
        for name in [RUNTIME_NAME, LAUNCH_NAME, CAPABILITY_NAME, SOCKET_NAME] {
            remove_if_present(&dir.join(name))?;
        }
        Ok(())
    }

    /// Opens a fresh OpenVMM log, replacing the previous one.
    pub(crate) fn create_log(&self, sandbox_id: &SandboxId) -> Result<File> {
        let path = self.log_path(sandbox_id);
        private_options()
            .truncate(true)
            .open(&path)
            .map_err(io_error(format!("cannot create {}", path.display())))
    }

    /// Deletes the state of a stopped sandbox.
    ///
    /// Unknown files abort the removal before anything is deleted. The configuration is removed
    /// last, so an interrupted removal leaves a sandbox that can be deprovisioned again.
    pub(crate) fn remove(&self, sandbox_id: &SandboxId) -> Result<()> {
        let dir = self.dir(sandbox_id);
        let entries = fs::read_dir(&dir).map_err(io_error(format!(
            "cannot list sandbox state {}",
            dir.display()
        )))?;
        let known = [
            RUNTIME_NAME,
            LAUNCH_NAME,
            CAPABILITY_NAME,
            SOCKET_NAME,
            OUTCOME_NAME,
            LOG_NAME,
        ];
        let mut owned = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_error(format!(
                "cannot list sandbox state {}",
                dir.display()
            )))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let temporary = name.starts_with('.') && name.ends_with(".tmp");
            if known.contains(&name.as_ref()) || temporary {
                owned.push(entry.path());
            } else if name != RECORD_NAME {
                return Err(Error::backend_error(format!(
                    "sandbox state {} contains the unexpected entry {name:?}; remove it and retry",
                    dir.display()
                )));
            }
        }
        for path in owned {
            remove_if_present(&path)?;
        }
        remove_if_present(&dir.join(RECORD_NAME))?;
        fs::remove_dir(&dir).map_err(io_error(format!(
            "cannot remove sandbox state {}",
            dir.display()
        )))
    }
}

pub(crate) fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(io_error(format!("cannot remove {}", path.display()))(error))
        }
        _ => Ok(()),
    }
}

fn lock(path: &Path) -> Result<LockGuard> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(io_error(format!("cannot open lock {}", path.display())))?;
    file.lock()
        .map_err(io_error(format!("cannot lock {}", path.display())))?;
    Ok(LockGuard { _file: file })
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// Atomically replaces `path` with `contents`, readable only by the current user.
fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix)
        .map_err(|error| Error::backend_error("cannot name a temporary file").with_source(error))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", u64::from_le_bytes(suffix)));
    let written = (|| -> io::Result<()> {
        let mut file = private_options().create_new(true).open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written.map_err(io_error(format!("cannot write {}", path.display())))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut text = serde_json::to_vec_pretty(value)
        .map_err(|error| Error::backend_error("cannot encode sandbox state").with_source(error))?;
    text.push(b'\n');
    write_private(path, &text)
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let text = match fs::read(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(format!("cannot read {}", path.display()))(error)),
    };
    serde_json::from_slice(&text).map(Some).map_err(|error| {
        Error::backend_error(format!("sandbox state {} is malformed", path.display()))
            .with_source(error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

    fn record() -> SandboxRecord {
        SandboxRecord {
            format: STATE_FORMAT,
            backend: BACKEND_KEY.to_owned(),
            network: None,
            filesystem: None,
            memory_mib: 256,
            workload_uid: 65534,
            workload_gid: 65534,
            create_workload_account: false,
            hostname: "nvx-sandbox".to_owned(),
        }
    }

    #[test]
    fn records_round_trip_and_unknown_ids_are_stale() {
        let root = tempfile::tempdir().unwrap();
        let store = StateStore::open(&root.path().join("sandboxes")).unwrap();
        let id = SandboxId::generate().unwrap();
        assert_eq!(store.load(&id).unwrap_err().code(), ErrorCode::StaleId);
        store.create(&id, &record()).unwrap();
        assert_eq!(store.load(&id).unwrap(), record());
        assert!(store.runtime(&id).unwrap().is_none());
    }

    #[test]
    fn runtime_files_are_cleared_and_state_removed() {
        let root = tempfile::tempdir().unwrap();
        let store = StateStore::open(root.path()).unwrap();
        let id = SandboxId::generate().unwrap();
        store.create(&id, &record()).unwrap();
        assert!(store.launch(&id).unwrap().is_none());
        let launch = LaunchRecord {
            format: STATE_FORMAT,
            endpoint: "endpoint".to_owned(),
            process: None,
        };
        store.write_launch(&id, &launch).unwrap();
        assert_eq!(store.launch(&id).unwrap(), Some(launch.clone()));
        let identified = LaunchRecord {
            process: Some(ProcessIdentity {
                pid: 1,
                start_time: 2,
            }),
            ..launch.clone()
        };
        store.write_launch(&id, &identified).unwrap();
        assert_eq!(store.launch(&id).unwrap(), Some(identified));
        store.remove_launch(&id);
        assert!(store.launch(&id).unwrap().is_none());
        store.write_launch(&id, &launch).unwrap();
        let runtime = RuntimeRecord {
            format: STATE_FORMAT,
            pid: 1,
            start_time: 2,
            endpoint: "endpoint".to_owned(),
        };
        store.write_runtime(&id, &runtime).unwrap();
        store.write_capability(&id, &[9; CAPABILITY_LEN]).unwrap();
        assert_eq!(store.runtime(&id).unwrap(), Some(runtime));
        assert_eq!(store.read_capability(&id).unwrap(), [9; CAPABILITY_LEN]);
        store.clear_runtime(&id).unwrap();
        assert!(store.runtime(&id).unwrap().is_none());
        assert!(store.launch(&id).unwrap().is_none());
        assert!(store.read_capability(&id).is_err());
        drop(store.create_log(&id).unwrap());
        store.remove(&id).unwrap();
        assert!(!store.dir(&id).exists());
    }

    #[test]
    fn removal_preserves_unknown_files_and_the_sandbox() {
        let root = tempfile::tempdir().unwrap();
        let store = StateStore::open(root.path()).unwrap();
        let id = SandboxId::generate().unwrap();
        store.create(&id, &record()).unwrap();
        fs::write(store.dir(&id).join("keep.txt"), b"user data").unwrap();
        assert!(store.remove(&id).is_err());
        assert!(store.dir(&id).join("keep.txt").exists());
        assert_eq!(store.load(&id).unwrap(), record());
    }

    #[test]
    fn records_of_other_formats_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let store = StateStore::open(root.path()).unwrap();
        let id = SandboxId::generate().unwrap();
        store.create(&id, &record()).unwrap();
        fs::write(store.dir(&id).join(RECORD_NAME), b"{ truncated").unwrap();
        assert_eq!(store.load(&id).unwrap_err().code(), ErrorCode::BackendError);
        let mut legacy = serde_json::to_value(record()).unwrap();
        legacy["format"] = 1.into();
        fs::write(
            store.dir(&id).join(RECORD_NAME),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        assert_eq!(store.load(&id).unwrap_err().code(), ErrorCode::BackendError);
    }

    #[test]
    fn locks_are_exclusive_per_sandbox() {
        let root = tempfile::tempdir().unwrap();
        let store = StateStore::open(root.path()).unwrap();
        let id = SandboxId::generate().unwrap();
        let guard = store.lock(&id).unwrap();
        let path = store.lock_path(&id);
        let contender = OpenOptions::new().write(true).open(&path).unwrap();
        assert!(contender.try_lock().is_err());
        drop(guard);
        contender.try_lock().unwrap();
    }
}
