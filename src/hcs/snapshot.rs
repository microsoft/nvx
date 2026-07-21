// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! Versioned metadata around HCS's opaque runtime-state file.

use ::std::fs::{self, File};
use ::std::io::{Read, Write};
use ::std::path::{Path, PathBuf};

use ::anyhow::{Context, Result, bail};
use ::serde::{Deserialize, Serialize};
use ::sha2::{Digest, Sha256};
use ::windows::core::GUID;

use super::network::NetworkConfig;

const FORMAT: &str = "NVXHCSS1";
const CURRENT_VERSION: u32 = 3;
const MINIMUM_VERSION: u32 = 1;
const NON_NETWORK_VERSION: u32 = 2;
const BACKEND: &str = "hcs";
pub const MANIFEST_FILE: &str = "manifest.json";
pub const STATE_FILE: &str = "runtime.vmrs";

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct HostVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Artifact {
    path: PathBuf,
    sha256: String,
}

impl Artifact {
    fn capture(path: &Path) -> Result<Self> {
        let path: PathBuf = path
            .canonicalize()
            .with_context(|| format!("resolving snapshot artifact {path:?}"))?;
        Ok(Self {
            sha256: sha256(&path)?,
            path,
        })
    }

    fn validate(&self, name: &str) -> Result<PathBuf> {
        let path: PathBuf = self
            .path
            .canonicalize()
            .with_context(|| format!("resolving HCS snapshot {name} {:?}", self.path))?;
        let actual: String = sha256(&path)?;
        if actual != self.sha256 {
            bail!(
                "HCS snapshot {name} hash mismatch for {path:?}: expected {}, found {actual}",
                self.sha256
            );
        }
        Ok(path)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    format: String,
    version: u32,
    backend: String,
    pub vm_id: String,
    pub host: HostVersion,
    pub memory_mib: u64,
    pub cmdline: String,
    kernel: Artifact,
    initrd: Artifact,
    state_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,
}

pub struct LoadedSnapshot {
    pub manifest: Manifest,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub state: PathBuf,
}

/// A newly created snapshot directory removed on failure until its manifest is committed.
pub struct Capture {
    directory: PathBuf,
    state: PathBuf,
    committed: bool,
}

impl Capture {
    pub fn prepare(directory: &Path) -> Result<Self> {
        if directory.exists() {
            bail!("HCS snapshot directory already exists: {directory:?}");
        }
        if let Some(parent) = directory
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating HCS snapshot parent directory {parent:?}"))?;
        }
        fs::create_dir(directory)
            .with_context(|| format!("creating HCS snapshot directory {directory:?}"))?;
        let directory: PathBuf = directory
            .canonicalize()
            .with_context(|| format!("resolving HCS snapshot directory {directory:?}"))?;
        let state: PathBuf = directory.join(STATE_FILE);
        Ok(Self {
            directory,
            state,
            committed: false,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn state(&self) -> &Path {
        &self.state
    }

    pub fn commit(&mut self, manifest: &Manifest) -> Result<()> {
        let metadata = fs::metadata(&self.state)
            .with_context(|| format!("reading saved HCS state metadata {:?}", self.state))?;
        if metadata.len() == 0 {
            bail!("HCS produced an empty runtime-state file: {:?}", self.state);
        }
        manifest.write(&self.directory)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

impl Manifest {
    pub fn capture(
        vm_id: String,
        host: HostVersion,
        memory_mib: u64,
        cmdline: String,
        kernel: &Path,
        initrd: &Path,
        network: Option<NetworkConfig>,
    ) -> Result<Self> {
        let version = if network.is_some() {
            CURRENT_VERSION
        } else {
            NON_NETWORK_VERSION
        };
        Ok(Self {
            format: FORMAT.to_string(),
            version,
            backend: BACKEND.to_string(),
            vm_id,
            host,
            memory_mib,
            cmdline,
            kernel: Artifact::capture(kernel)?,
            initrd: Artifact::capture(initrd)?,
            state_file: STATE_FILE.to_string(),
            network,
        })
    }

    pub fn write(&self, directory: &Path) -> Result<()> {
        let document: Vec<u8> =
            ::serde_json::to_vec_pretty(self).context("serializing HCS snapshot manifest")?;
        let temporary: PathBuf = directory.join(format!("{MANIFEST_FILE}.tmp"));
        let final_path: PathBuf = directory.join(MANIFEST_FILE);
        let mut file: File = File::create(&temporary)
            .with_context(|| format!("creating HCS snapshot manifest {temporary:?}"))?;
        file.write_all(&document)
            .with_context(|| format!("writing HCS snapshot manifest {temporary:?}"))?;
        file.sync_all()
            .with_context(|| format!("flushing HCS snapshot manifest {temporary:?}"))?;
        fs::rename(&temporary, &final_path).with_context(|| {
            format!("committing HCS snapshot manifest {temporary:?} to {final_path:?}")
        })
    }

    fn validate_header(&self, host: HostVersion) -> Result<()> {
        if self.format != FORMAT
            || !(MINIMUM_VERSION..=CURRENT_VERSION).contains(&self.version)
            || self.backend != BACKEND
        {
            bail!(
                "unsupported HCS snapshot format: format={:?}, version={}, backend={:?}",
                self.format,
                self.version,
                self.backend
            );
        }
        if self.host != host {
            bail!(
                "HCS snapshot host mismatch: captured on {}.{}.{} but current host is {}.{}.{}",
                self.host.major,
                self.host.minor,
                self.host.build,
                host.major,
                host.minor,
                host.build
            );
        }
        if self.state_file != STATE_FILE {
            bail!("invalid HCS snapshot state-file name {:?}", self.state_file);
        }
        if GUID::try_from(self.vm_id.as_str()).is_err() {
            bail!("invalid HCS snapshot VM ID {:?}", self.vm_id);
        }
        if self.memory_mib == 0 {
            bail!("invalid HCS snapshot memory size 0 MiB");
        }
        if self.version < CURRENT_VERSION && self.network.is_some() {
            bail!(
                "networked HCS snapshot version {} predates externally managed HCN endpoints; recapture it with version {CURRENT_VERSION}",
                self.version
            );
        }
        if let Some(network) = &self.network {
            network.validate()?;
            if network.mac_address.is_none() {
                bail!("networked HCS snapshot has no canonical MAC address");
            }
        }
        Ok(())
    }
}

pub fn load(directory: &Path, host: HostVersion) -> Result<LoadedSnapshot> {
    let directory: PathBuf = directory
        .canonicalize()
        .with_context(|| format!("resolving HCS snapshot directory {directory:?}"))?;
    let manifest_path: PathBuf = directory.join(MANIFEST_FILE);
    let document: Vec<u8> = fs::read(&manifest_path)
        .with_context(|| format!("reading HCS snapshot manifest {manifest_path:?}"))?;
    let manifest: Manifest = ::serde_json::from_slice(&document)
        .with_context(|| format!("parsing HCS snapshot manifest {manifest_path:?}"))?;
    manifest.validate_header(host)?;
    let state_path: PathBuf = directory.join(STATE_FILE);
    let state: PathBuf = state_path
        .canonicalize()
        .with_context(|| format!("resolving HCS snapshot state file {state_path:?}"))?;
    if state.parent() != Some(directory.as_path()) {
        bail!("HCS snapshot state file escapes its directory: {state:?}");
    }
    let state_metadata = fs::metadata(&state)
        .with_context(|| format!("reading HCS snapshot state metadata {state:?}"))?;
    if !state_metadata.is_file() || state_metadata.len() == 0 {
        bail!("HCS snapshot state file is missing or empty: {state:?}");
    }
    let kernel: PathBuf = manifest.kernel.validate("kernel")?;
    let initrd: PathBuf = manifest.initrd.validate("initrd")?;
    Ok(LoadedSnapshot {
        manifest,
        kernel,
        initrd,
        state,
    })
}

fn sha256(path: &Path) -> Result<String> {
    let mut file: File = File::open(path)
        .with_context(|| format!("opening snapshot artifact for hashing {path:?}"))?;
    let mut hasher: Sha256 = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count: usize = file
            .read(&mut buffer)
            .with_context(|| format!("hashing snapshot artifact {path:?}"))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use ::std::net::Ipv4Addr;
    use ::std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    const TEST_VM_ID: &str = "11111111-2222-4333-8444-555555555555";

    fn fixture() -> (PathBuf, PathBuf, PathBuf, HostVersion) {
        let serial: u64 = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory: PathBuf = ::std::env::temp_dir().join(format!(
            "nvx-hcs-snapshot-test-{}-{serial}",
            ::std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let kernel: PathBuf = directory.join("kernel");
        let initrd: PathBuf = directory.join("initrd");
        fs::write(&kernel, b"kernel fixture").unwrap();
        fs::write(&initrd, b"initrd fixture").unwrap();
        fs::write(directory.join(STATE_FILE), b"opaque state").unwrap();
        (
            directory,
            kernel,
            initrd,
            HostVersion {
                major: 10,
                minor: 0,
                build: 26100,
            },
        )
    }

    fn network_config() -> NetworkConfig {
        NetworkConfig {
            network_id: "11111111-2222-4333-8444-555555555555".to_string(),
            endpoint_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".to_string(),
            adapter_id: "01234567-89ab-4cde-8f01-23456789abcd".to_string(),
            guest_ip: Ipv4Addr::new(10, 0, 0, 2),
            prefix: 24,
            network: Ipv4Addr::new(10, 0, 0, 0),
            gateway: Ipv4Addr::new(10, 0, 0, 1),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            dns_servers: vec![Ipv4Addr::new(1, 1, 1, 1)],
            mac_address: Some("00-15-5D-52-C0-10".to_string()),
        }
    }

    #[test]
    fn manifest_round_trips_and_validates_artifacts() {
        let (directory, kernel, initrd, host) = fixture();
        let manifest = Manifest::capture(
            TEST_VM_ID.to_string(),
            host,
            512,
            "console=ttyS0".to_string(),
            &kernel,
            &initrd,
            None,
        )
        .unwrap();
        manifest.write(&directory).unwrap();
        let loaded: LoadedSnapshot = load(&directory, host).unwrap();
        assert_eq!(loaded.manifest.vm_id, TEST_VM_ID);
        assert_eq!(loaded.manifest.memory_mib, 512);
        assert_eq!(loaded.kernel, kernel.canonicalize().unwrap());
        assert_eq!(loaded.initrd, initrd.canonicalize().unwrap());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn restore_rejects_changed_artifact() {
        let (directory, kernel, initrd, host) = fixture();
        let manifest = Manifest::capture(
            TEST_VM_ID.to_string(),
            host,
            512,
            "console=ttyS0".to_string(),
            &kernel,
            &initrd,
            None,
        )
        .unwrap();
        manifest.write(&directory).unwrap();
        fs::write(&kernel, b"changed kernel").unwrap();
        let error = load(&directory, host).err().unwrap();
        assert!(error.to_string().contains("kernel hash mismatch"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn restore_rejects_host_and_state_file_mismatches() {
        let (directory, kernel, initrd, host) = fixture();
        let mut manifest = Manifest::capture(
            TEST_VM_ID.to_string(),
            host,
            512,
            "console=ttyS0".to_string(),
            &kernel,
            &initrd,
            None,
        )
        .unwrap();
        manifest.write(&directory).unwrap();
        let other_host = HostVersion {
            build: 26200,
            ..host
        };
        assert!(
            load(&directory, other_host)
                .err()
                .unwrap()
                .to_string()
                .contains("host mismatch")
        );

        manifest.state_file = "..\\outside.vmrs".to_string();
        manifest.write(&directory).unwrap();
        assert!(
            load(&directory, host)
                .err()
                .unwrap()
                .to_string()
                .contains("state-file name")
        );
        manifest.state_file = STATE_FILE.to_string();
        manifest.vm_id = "not-a-guid".to_string();
        manifest.write(&directory).unwrap();
        assert!(
            load(&directory, host)
                .err()
                .unwrap()
                .to_string()
                .contains("VM ID")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn incomplete_capture_is_removed_and_existing_target_is_rejected() {
        let serial: u64 = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory: PathBuf = ::std::env::temp_dir().join(format!(
            "nvx-hcs-capture-test-{}-{serial}",
            ::std::process::id()
        ));
        {
            let capture: Capture = Capture::prepare(&directory).unwrap();
            assert!(capture.directory().is_dir());
            assert!(Capture::prepare(&directory).is_err());
        }
        assert!(!directory.exists());
    }

    #[test]
    fn capture_commit_publishes_manifest_and_empty_state_rolls_back() {
        let serial: u64 = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let parent: PathBuf = ::std::env::temp_dir().join(format!(
            "nvx-hcs-commit-test-{}-{serial}",
            ::std::process::id()
        ));
        fs::create_dir(&parent).unwrap();
        let kernel: PathBuf = parent.join("kernel");
        let initrd: PathBuf = parent.join("initrd");
        fs::write(&kernel, b"kernel fixture").unwrap();
        fs::write(&initrd, b"initrd fixture").unwrap();
        let host = HostVersion {
            major: 10,
            minor: 0,
            build: 26100,
        };
        let manifest = Manifest::capture(
            TEST_VM_ID.to_string(),
            host,
            512,
            "console=ttyS0".to_string(),
            &kernel,
            &initrd,
            None,
        )
        .unwrap();

        let committed_directory: PathBuf = parent.join("committed");
        {
            let mut capture: Capture = Capture::prepare(&committed_directory).unwrap();
            fs::write(capture.state(), b"opaque state").unwrap();
            capture.commit(&manifest).unwrap();
        }
        assert!(load(&committed_directory, host).is_ok());

        let empty_directory: PathBuf = parent.join("empty");
        {
            let mut capture: Capture = Capture::prepare(&empty_directory).unwrap();
            fs::write(capture.state(), b"").unwrap();
            assert!(capture.commit(&manifest).is_err());
        }
        assert!(!empty_directory.exists());
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn network_identity_round_trips_and_requires_manifest_v3() {
        let (directory, kernel, initrd, host) = fixture();
        let network = network_config();
        let mut manifest = Manifest::capture(
            TEST_VM_ID.to_string(),
            host,
            512,
            "console=ttyS0".to_string(),
            &kernel,
            &initrd,
            Some(network.clone()),
        )
        .unwrap();
        manifest.write(&directory).unwrap();
        let loaded = load(&directory, host).unwrap();
        assert_eq!(loaded.manifest.network, Some(network));
        assert_eq!(loaded.manifest.version, 3);

        manifest.version = 2;
        manifest.write(&directory).unwrap();
        assert!(
            load(&directory, host)
                .err()
                .unwrap()
                .to_string()
                .contains("predates externally managed HCN endpoints")
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
