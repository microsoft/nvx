// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! Minimal ownership-safe wrappers around the flat HCS API.

use ::core::ffi::c_void;
use ::std::path::Path;

use ::anyhow::{Context, Result, anyhow, bail};
use ::windows::Wdk::System::SystemServices::RtlGetVersion;
use ::windows::Win32::Foundation::{
    ERROR_TIMEOUT, HCS_E_OPERATION_TIMEOUT, HLOCAL, LocalFree, WAIT_TIMEOUT,
};
use ::windows::Win32::System::HostComputeSystem::{
    HCS_OPERATION, HcsCancelOperation, HcsCloseOperation, HcsCreateEmptyRuntimeStateFile,
    HcsCreateOperation, HcsGetServiceProperties, HcsGrantVmAccess, HcsWaitForOperationResult,
};
use ::windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use ::windows::core::{Error as WindowsError, GUID, HRESULT, HSTRING, PWSTR};

use super::snapshot::HostVersion;

const OPERATION_TIMEOUT_MS: u32 = 120_000;

/// A wide string returned by HCS and owned by the process-local allocator.
struct LocalWideString(PWSTR);

impl LocalWideString {
    fn new(value: PWSTR) -> Self {
        Self(value)
    }

    fn to_string(&self) -> Result<String> {
        if self.0.is_null() {
            return Ok(String::new());
        }
        // SAFETY: HCS returned a valid, NUL-terminated string that remains owned by `self`.
        unsafe { self.0.to_string() }.context("decoding HCS result document as UTF-16")
    }
}

/// One in-flight HCS operation, closed on every path.
pub struct Operation(HCS_OPERATION);

impl Operation {
    pub fn new() -> Result<Self> {
        // SAFETY: A null context and callback request a waitable operation with no notifications.
        let handle: HCS_OPERATION = unsafe { HcsCreateOperation(None, None) };
        if handle.is_invalid() {
            bail!("HcsCreateOperation returned an invalid handle");
        }
        Ok(Self(handle))
    }

    pub fn handle(&self) -> HCS_OPERATION {
        self.0
    }

    pub fn wait(&self, operation: &str, resource_id: &str) -> Result<String> {
        let mut result: PWSTR = PWSTR::null();
        // SAFETY: The operation handle is live and `result` is a valid out-pointer.
        let completion =
            unsafe { HcsWaitForOperationResult(self.0, OPERATION_TIMEOUT_MS, Some(&mut result)) };
        let document: String = local_string(result)
            .unwrap_or_else(|error| format!("<failed to decode HCS result: {error:#}>"));
        match completion {
            Ok(()) => Ok(document),
            Err(error) if is_timeout(error.code()) => {
                let cancel = unsafe { HcsCancelOperation(self.0) };
                let cancel_detail = cancel
                    .err()
                    .map(|error| format!("; cancel failed: {error}"))
                    .unwrap_or_default();
                bail!(
                    "{operation} timed out after {} ms for compute system {resource_id}{}; result: {}",
                    OPERATION_TIMEOUT_MS,
                    cancel_detail,
                    if document.is_empty() {
                        "<none>"
                    } else {
                        &document
                    }
                )
            }
            Err(error) => Err(hcs_error(operation, resource_id, error, &document)),
        }
    }
}

fn is_timeout(code: HRESULT) -> bool {
    code == HCS_E_OPERATION_TIMEOUT
        || code == HRESULT::from_win32(WAIT_TIMEOUT.0)
        || code == HRESULT::from_win32(ERROR_TIMEOUT.0)
}

impl Drop for Operation {
    fn drop(&mut self) {
        // SAFETY: `self.0` is valid and this non-cloneable wrapper closes it exactly once.
        unsafe { HcsCloseOperation(self.0) };
    }
}

impl Drop for LocalWideString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: HCS documents are allocated with the process-local allocator and this
            // wrapper releases the pointer exactly once.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.0.cast::<c_void>())));
            }
        }
    }
}

/// Queries basic HCS service properties, including supported schema versions.
pub fn service_properties() -> Result<String> {
    let query: HSTRING = HSTRING::from(r#"{"PropertyTypes":["Basic"]}"#);
    // SAFETY: `query` is a valid, NUL-terminated Windows string. The returned buffer is wrapped
    // immediately and released with `LocalFree`.
    let result: PWSTR = unsafe { HcsGetServiceProperties(&query) }
        .context("HcsGetServiceProperties(Basic) failed")?;
    LocalWideString::new(result).to_string()
}

/// Generates a lower-case GUID suitable for HCS and HCN identifiers.
pub fn new_guid() -> Result<String> {
    let guid: GUID = GUID::new().context("generating HCS VM identifier")?;
    Ok(format!("{guid:?}").to_ascii_lowercase())
}

/// Grants one VM identity access to a host artifact.
pub fn grant_vm_access(vm_id: &str, path: &Path) -> Result<()> {
    let vm_id: HSTRING = HSTRING::from(vm_id);
    let path_text: String = path.to_string_lossy().into_owned();
    let path_wide: HSTRING = HSTRING::from(&path_text);
    // SAFETY: Both arguments are valid, NUL-terminated Windows strings.
    unsafe { HcsGrantVmAccess(&vm_id, &path_wide) }
        .with_context(|| format!("HcsGrantVmAccess failed for {path_text}"))
}

/// Creates the empty VMRS file required by `HcsSaveComputeSystem`.
pub fn create_runtime_state_file(path: &Path) -> Result<()> {
    let path_text: String = path.to_string_lossy().into_owned();
    let path_wide: HSTRING = HSTRING::from(&path_text);
    // SAFETY: `path_wide` is a valid, NUL-terminated Windows path.
    unsafe { HcsCreateEmptyRuntimeStateFile(&path_wide) }
        .with_context(|| format!("HcsCreateEmptyRuntimeStateFile failed for {path_text}"))
}

/// Returns the real Windows version rather than the application-manifest-adjusted value.
pub fn host_version() -> Result<HostVersion> {
    let mut version: OSVERSIONINFOW = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `version` has the required size field and is a valid writable structure.
    let status = unsafe { RtlGetVersion(&mut version) };
    if status.0 < 0 {
        bail!(
            "RtlGetVersion failed with NTSTATUS 0x{:08X}",
            status.0 as u32
        );
    }
    Ok(HostVersion {
        major: version.dwMajorVersion,
        minor: version.dwMinorVersion,
        build: version.dwBuildNumber,
    })
}

/// Takes ownership of an HCS-allocated result string and decodes it.
pub fn local_string(value: PWSTR) -> Result<String> {
    LocalWideString::new(value).to_string()
}

/// Builds an error retaining both the HRESULT and the HCS result document.
pub fn hcs_error(
    operation: &str,
    resource_id: &str,
    error: WindowsError,
    document: &str,
) -> ::anyhow::Error {
    let code: u32 = error.code().0 as u32;
    let result: &str = if document.is_empty() {
        "<none>"
    } else {
        document
    };
    anyhow!(
        "{operation} failed for compute system {resource_id}: {error} \
         (HRESULT 0x{code:08X}); result: {result}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_operation_timeout_forms() {
        assert!(is_timeout(HCS_E_OPERATION_TIMEOUT));
        assert!(is_timeout(HRESULT::from_win32(WAIT_TIMEOUT.0)));
        assert!(is_timeout(HRESULT::from_win32(ERROR_TIMEOUT.0)));
        assert!(!is_timeout(HRESULT(0x8000_4005u32 as i32)));
    }
}
