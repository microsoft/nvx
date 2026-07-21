// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! HCS compute-system lifecycle.

use ::std::thread;
use ::std::time::Duration;

use ::anyhow::{Error, Result};
use ::log::{debug, warn};
use ::windows::Win32::Foundation::{
    ERROR_TIMEOUT, HCN_E_ENDPOINT_ALREADY_ATTACHED, HCS_E_OPERATION_TIMEOUT,
    HCS_E_SYSTEM_ALREADY_EXISTS, WAIT_TIMEOUT,
};
use ::windows::Win32::System::HostComputeSystem::{
    HCS_SYSTEM, HcsCloseComputeSystem, HcsCreateComputeSystem, HcsModifyComputeSystem,
    HcsPauseComputeSystem, HcsSaveComputeSystem, HcsStartComputeSystem, HcsTerminateComputeSystem,
    HcsWaitForComputeSystemExit,
};
use ::windows::core::{HRESULT, HSTRING, PWSTR};

use super::api::{self, Operation};

const CREATE_RELEASE_RETRIES: u32 = 50;
const CREATE_RELEASE_RETRY_DELAY: Duration = Duration::from_millis(100);

/// An HCS compute system with ordered termination and close semantics.
pub struct ComputeSystem {
    handle: HCS_SYSTEM,
    id: String,
    running: bool,
}

impl ComputeSystem {
    pub fn create(id: &str, document: &str) -> Result<Self> {
        for retry in 0..=CREATE_RELEASE_RETRIES {
            match Self::create_once(id, document) {
                Ok(system) => return Ok(system),
                Err(error) if retry < CREATE_RELEASE_RETRIES && is_release_pending(&error) => {
                    debug!(
                        "HCS resources for {id} are still being released; retrying create ({}/{})",
                        retry + 1,
                        CREATE_RELEASE_RETRIES,
                    );
                    thread::sleep(CREATE_RELEASE_RETRY_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded HCS create loop always returns")
    }

    fn create_once(id: &str, document: &str) -> Result<Self> {
        let operation: Operation = Operation::new()?;
        let id_string: HSTRING = HSTRING::from(id);
        let document: HSTRING = HSTRING::from(document);
        // SAFETY: The strings and operation handle remain valid for the call; the reserved
        // security descriptor is null as required by HCS.
        let handle: HCS_SYSTEM =
            unsafe { HcsCreateComputeSystem(&id_string, &document, operation.handle(), None) }
                .map_err(|error| {
                    api::hcs_error("HcsCreateComputeSystem (immediate)", id, error, "")
                })?;
        let system = Self {
            handle,
            id: id.to_string(),
            running: false,
        };
        operation.wait("HcsCreateComputeSystem", id)?;
        Ok(system)
    }

    pub fn start(&mut self) -> Result<()> {
        let operation: Operation = Operation::new()?;
        let options: HSTRING = HSTRING::from("{}");
        // SAFETY: The compute system and operation handles are live for this call.
        unsafe { HcsStartComputeSystem(self.handle, operation.handle(), &options) }.map_err(
            |error| api::hcs_error("HcsStartComputeSystem (immediate)", &self.id, error, ""),
        )?;
        operation.wait("HcsStartComputeSystem", &self.id)?;
        self.running = true;
        Ok(())
    }

    pub fn pause(&mut self, options: &str) -> Result<()> {
        let operation: Operation = Operation::new()?;
        let options: HSTRING = HSTRING::from(options);
        // SAFETY: The compute system and operation handles are live for this call.
        unsafe { HcsPauseComputeSystem(self.handle, operation.handle(), &options) }.map_err(
            |error| api::hcs_error("HcsPauseComputeSystem (immediate)", &self.id, error, ""),
        )?;
        operation.wait("HcsPauseComputeSystem", &self.id)?;
        Ok(())
    }

    pub fn save(&mut self, options: &str) -> Result<()> {
        let operation: Operation = Operation::new()?;
        let options: HSTRING = HSTRING::from(options);
        // SAFETY: The compute system is paused and both handles are live for this call.
        unsafe { HcsSaveComputeSystem(self.handle, operation.handle(), &options) }.map_err(
            |error| api::hcs_error("HcsSaveComputeSystem (immediate)", &self.id, error, ""),
        )?;
        operation.wait("HcsSaveComputeSystem", &self.id)?;
        self.running = false;
        Ok(())
    }

    pub fn remove_network_adapter(&mut self, document: &str) -> Result<()> {
        let operation: Operation = Operation::new()?;
        let document: HSTRING = HSTRING::from(document);
        // SAFETY: The compute system and operation handles are live for this call; no caller
        // identity is required for removing a device owned by this compute system.
        unsafe { HcsModifyComputeSystem(self.handle, operation.handle(), &document, None) }
            .map_err(|error| {
                api::hcs_error(
                    "HcsModifyComputeSystem(remove network adapter) (immediate)",
                    &self.id,
                    error,
                    "",
                )
            })?;
        operation.wait("HcsModifyComputeSystem(remove network adapter)", &self.id)?;
        Ok(())
    }

    pub fn wait_for_exit(&mut self, timeout_ms: u32) -> Result<Option<String>> {
        let mut result: PWSTR = PWSTR::null();
        // SAFETY: The compute-system handle is live and `result` is a valid out-pointer.
        let completion =
            unsafe { HcsWaitForComputeSystemExit(self.handle, timeout_ms, Some(&mut result)) };
        let document: String = api::local_string(result)?;
        match completion {
            Ok(()) => {
                self.running = false;
                Ok(Some(document))
            }
            Err(error) if is_wait_timeout(error.code()) => Ok(None),
            Err(error) => Err(api::hcs_error(
                "HcsWaitForComputeSystemExit",
                &self.id,
                error,
                &document,
            )),
        }
    }

    pub fn terminate(&mut self) -> Result<()> {
        if !self.running {
            return Ok(());
        }
        let operation: Operation = Operation::new()?;
        let options: HSTRING = HSTRING::from("{}");
        // SAFETY: The compute system and operation handles are live for this call.
        unsafe { HcsTerminateComputeSystem(self.handle, operation.handle(), &options) }.map_err(
            |error| api::hcs_error("HcsTerminateComputeSystem (immediate)", &self.id, error, ""),
        )?;
        operation.wait("HcsTerminateComputeSystem", &self.id)?;
        self.running = false;
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.running
    }
}

fn is_release_pending(error: &Error) -> bool {
    is_release_pending_code(api::error_code(error))
}

fn is_release_pending_code(code: Option<HRESULT>) -> bool {
    code == Some(HCN_E_ENDPOINT_ALREADY_ATTACHED) || code == Some(HCS_E_SYSTEM_ALREADY_EXISTS)
}

fn is_wait_timeout(code: HRESULT) -> bool {
    code == HCS_E_OPERATION_TIMEOUT
        || code == HRESULT::from_win32(WAIT_TIMEOUT.0)
        || code == HRESULT::from_win32(ERROR_TIMEOUT.0)
}

impl Drop for ComputeSystem {
    fn drop(&mut self) {
        if self.running
            && let Err(error) = self.terminate()
        {
            warn!(
                "last-resort termination of HCS system {} failed: {error:#}",
                self.id
            );
        }
        // SAFETY: This non-cloneable wrapper owns the valid compute-system handle.
        unsafe { HcsCloseComputeSystem(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_hcs_and_win32_wait_timeouts() {
        let wait_timeout: HRESULT = HRESULT::from_win32(WAIT_TIMEOUT.0);
        assert_eq!(wait_timeout.0 as u32, 0x8007_0102);
        assert!(is_wait_timeout(wait_timeout));
        assert!(is_wait_timeout(HCS_E_OPERATION_TIMEOUT));
        assert!(is_wait_timeout(HRESULT::from_win32(ERROR_TIMEOUT.0)));
        assert!(!is_wait_timeout(HRESULT(0x8000_4005u32 as i32)));
    }

    #[test]
    fn retries_only_create_errors_caused_by_pending_release() {
        for code in [HCN_E_ENDPOINT_ALREADY_ATTACHED, HCS_E_SYSTEM_ALREADY_EXISTS] {
            assert!(is_release_pending_code(Some(code)));
        }
        assert!(!is_release_pending_code(Some(
            ::windows::Win32::Foundation::HCS_E_ACCESS_DENIED
        )));
        assert!(!is_release_pending_code(None));
    }
}
