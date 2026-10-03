//! Helpers shared by the OpenVMM integration tests.

use std::fs;
use std::path::Path;

use aci_edge_sandboxes::SandboxId;

/// Turns the state of a running sandbox into the state left by a caller that died between
/// launching OpenVMM and recording its process identity. Returns the OpenVMM process ID.
pub(crate) fn interrupt_start(state_root: &Path, sandbox_id: &SandboxId) -> u32 {
    let dir = state_root.join(sandbox_id.token());
    let runtime: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("runtime.json")).unwrap()).unwrap();
    let marker = serde_json::json!({
        "format": runtime["format"],
        "endpoint": runtime["endpoint"],
    });
    fs::write(dir.join("launch.json"), marker.to_string()).unwrap();
    fs::remove_file(dir.join("runtime.json")).unwrap();
    u32::try_from(runtime["pid"].as_u64().unwrap()).unwrap()
}

/// Returns whether a process runs. Zombies have exited; only their parent has yet to reap them.
#[cfg(target_os = "linux")]
pub(crate) fn process_running(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .and_then(|(_, fields)| fields.trim_start().chars().next())
            .is_some_and(|state| !matches!(state, 'Z' | 'X'))
    })
}

/// Returns whether a process runs.
#[cfg(windows)]
pub(crate) fn process_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    // SAFETY: OpenProcess has no memory-safety preconditions.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return false;
    }
    // SAFETY: the handle is a live process handle owned by this function.
    let running = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
    // SAFETY: the handle is closed exactly once.
    unsafe { CloseHandle(process) };
    running
}

/// Returns whether a process runs.
#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) fn process_running(_pid: u32) -> bool {
    unreachable!("the OpenVMM backend runs only on Linux and Windows")
}
