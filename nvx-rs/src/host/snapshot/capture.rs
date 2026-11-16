//! Post-exit capture validation and the two metadata files beside OpenVMM's snapshot.

use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;
use serde_json::json;

use super::{
    CaptureContext, DESCRIPTION_FILE, KEY_FILE, MANIFEST_NAME, OPENVMM_DIR, SNAPSHOT_ARTIFACTS,
    SnapshotKey, SnapshotManifest, check_openvmm,
};
use crate::host::platform::{self, open_regular, read_at_most, sync_dir, write_private};
use crate::host::process::Exited;
use crate::host::vm_directory::{OUTCOME, VmDir};
use crate::{Error, ErrorKind, Result};

/// Longest outcome report of a capture's VM that the capture reads, in bytes.
const MAX_REPORT_LEN: u64 = 1024 * 1024;

/// The metadata of a finished capture, whose files the caller may now publish.
#[derive(Debug)]
pub struct CapturedSnapshot {
    /// The checked key written to [`KEY_FILE`].
    pub key: SnapshotKey,
}

/// Finishes a capture after its VM has provably exited.
///
/// `exited` must prove the exit of the VM that produced `dir` and `context`; the caller keeps
/// that association. Exit proof alone does not establish success: the outcome report must
/// record a successful capture, and the private OpenVMM snapshot must pass the existing strict
/// manifest, artifact and capture-context checks.
///
/// Writes [`KEY_FILE`] and the informational [`DESCRIPTION_FILE`] beside [`OPENVMM_DIR`].
/// The caller moves only those three entries into its own private snapshot directory and keeps
/// them immutable while using it. No other VM-directory entry is moved, removed or published.
/// A failure may be corrected and retried while the directory and capture context remain intact;
/// either metadata file may already have been replaced.
///
/// Fails with [`ErrorKind::Exited`] for an absent or unsuccessful capture outcome, with
/// [`ErrorKind::Io`] for invalid files or a context/manifest mismatch, and otherwise as
/// [`SnapshotManifest::read`] and the checked context's configuration do.
///
/// A caller cannot finish without exit proof:
///
/// ```compile_fail
/// use nvx::host::{snapshot::{CaptureContext, finish_capture}, vm::VmDir};
///
/// fn without_exit(dir: &VmDir, context: &CaptureContext) {
///     finish_capture(dir, context);
/// }
/// ```
pub fn finish_capture(
    dir: &VmDir,
    context: &CaptureContext,
    _exited: &Exited,
) -> Result<CapturedSnapshot> {
    VmDir::open(dir.path())?;
    check_exit(&dir.path().join(OUTCOME))?;
    let manifest = check_openvmm(&dir.path().join(OPENVMM_DIR))?;
    let key = SnapshotKey::from_capture(context, &manifest)?;
    let time = manifest.time();
    let description = json!({
        "key": key.identity().to_string(),
        "runtime": key.runtime(),
        "hypervisor": key.hypervisor(),
        "cpuProfile": key.cpu_profile(),
        "capture": context.capture,
        "nic": context.nic,
        "console": context.console,
        "createdAt": manifest.created_at(),
        "openvmmVersion": manifest.openvmm_version(),
        "memoryLen": manifest.memory_len(),
        "stateLen": manifest.state_len(),
        "time": {
            "tscFrequencyHz": time.tsc_frequency_hz,
            "tscTolerancePpm": time.tsc_tolerance_ppm,
            "apicFrequencyHz": time.apic_frequency_hz,
            "captureTsc": time.capture_tsc,
            "captureUtcNs": time.capture_utc_ns,
            "captureMonotonicNs": time.capture_monotonic_ns,
            "hostClock": time.host_clock.as_str(),
            "hostId": time.host_id.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
            "bootId": time.boot_id,
            "lineage": time.lineage,
            "cutToAnchorNs": time.cut_to_anchor_ns,
        },
    });
    let description = serde_json::to_vec_pretty(&description).map_err(|error| {
        Error::other("cannot encode the snapshot description").with_source(error)
    })?;
    write_private(&dir.path().join(KEY_FILE), &key.to_bytes())?;
    write_private(&dir.path().join(DESCRIPTION_FILE), &description)?;
    sync_dir(dir.path())?;
    Ok(CapturedSnapshot { key })
}

/// The outcome report of a VM, as OpenVMM writes it, of which a capture reads the outcome.
#[derive(Deserialize)]
struct OutcomeReport {
    /// The outcome of the VM.
    outcome: Outcome,
}

/// The outcome of a VM, as its outcome report records it.
#[derive(Deserialize)]
struct Outcome {
    /// The operation of the VM: `capture` for a VM that OpenVMM captures.
    operation: String,
    /// The category of the outcome: `success` for a capture that OpenVMM published.
    category: String,
    /// The status code of the outcome: 0 for a capture that OpenVMM published.
    status_code: i64,
}

/// Checks that the outcome report records a capture that OpenVMM published successfully.
pub(in crate::host) fn check_exit(path: &Path) -> Result<()> {
    let description = format!("the outcome report {}", path.display());
    let file = match open_regular(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(Error::exited(format!(
                "the VM of the capture ended without {description}"
            )));
        }
        Err(error) => {
            return Err(Error::io(format!("cannot read {description}")).with_source(error));
        }
    };
    let bytes = read_at_most(file, MAX_REPORT_LEN, "an outcome report")
        .map_err(|error| Error::io(format!("cannot read {description}")).with_source(error))?;
    let report: OutcomeReport = serde_json::from_slice(&bytes)
        .map_err(|error| Error::io(format!("{description} is malformed")).with_source(error))?;
    let Outcome {
        operation,
        category,
        status_code,
    } = report.outcome;
    if operation != "capture" || category != "success" || status_code != 0 {
        return Err(Error::exited(format!(
            "the VM of the capture ended without a capture: {description} records the outcome \
             {category} with status {status_code} of the operation {operation}"
        )));
    }
    Ok(())
}

/// Preserves the legacy capture's directory, strict manifest, regular artifact and length checks.
pub(in crate::host) fn check_snapshot(snapshot: &Path) -> Result<SnapshotManifest> {
    let what = "the snapshot of the capture";
    let problem = platform::check_snapshot_dir(snapshot).map_err(|error| {
        Error::io(format!("cannot open {what} {}", snapshot.display())).with_source(error)
    })?;
    if let Some(problem) = problem {
        return Err(Error::io(format!(
            "{what} {} {problem}",
            snapshot.display()
        )));
    }
    let manifest = SnapshotManifest::read(snapshot).map_err(|error| {
        if error.kind() != ErrorKind::NotFound {
            return error;
        }
        let (_, message, source) = error.into_parts();
        let error = Error::io(message);
        match source {
            Some(source) => error.with_source(source),
            None => error,
        }
    })?;
    let mut names = fs::read_dir(snapshot)
        .and_then(|entries| {
            entries
                .map(|entry| {
                    entry?.file_name().into_string().map_err(|name| {
                        io::Error::other(format!("the snapshot entry {name:?} is not Unicode"))
                    })
                })
                .collect::<io::Result<Vec<_>>>()
        })
        .map_err(|error| super::entry_error(snapshot, error))?;
    names.sort();
    if names != SNAPSHOT_ARTIFACTS {
        return Err(Error::io(format!(
            "{what} {} holds {}, not exactly the manifest, RAM, and saved state of a platform-tier \
             snapshot",
            snapshot.display(),
            names.join(", ")
        )));
    }
    for (name, length) in [
        (MANIFEST_NAME, None),
        ("memory.bin", Some(manifest.memory_len())),
        ("state.bin", Some(manifest.state_len())),
    ] {
        let path = snapshot.join(name);
        let refused = |problem: String| {
            Error::io(format!(
                "the artifact {} of {what} {problem}",
                path.display()
            ))
        };
        let file =
            open_regular(&path).map_err(|error| refused(format!("cannot be used: {error}")))?;
        if let Some(problem) = platform::private_file_problem(&file)
            .map_err(|error| refused(format!("cannot be checked: {error}")))?
        {
            return Err(refused(problem));
        }
        let found = file
            .metadata()
            .map_err(|error| refused(format!("cannot be examined: {error}")))?
            .len();
        if let Some(length) = length
            && found != length
        {
            return Err(refused(format!(
                "is {found} bytes long, not the {length} bytes that its manifest records"
            )));
        }
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests;
