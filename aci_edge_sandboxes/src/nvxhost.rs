//! Checked dynamic binding for the privately supplied nvxhost C ABI.

#[cfg(not(target_pointer_width = "64"))]
compile_error!("the nvxhost ABI is supported only on 64-bit hosts");

use std::ffi::{OsString, c_void};
use std::fs;
use std::mem::size_of;
use std::path::Path;
use std::ptr;
use std::slice;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use libloading::Library;
use prost::Message;
use sha2_runtime::{Digest, Sha256};

use crate::error::{Error, Result};

const ABI_VERSION: u32 = 1;
const STATUS_OK: i32 = 0;
const STATUS_TIMEOUT: i32 = 2;
const RESULT_OK: i32 = 0;
const RESULT_ERROR: i32 = 1;
const COMPLETION_CONNECT: u32 = 1;
const COMPLETION_SEND: u32 = 2;
const COMPLETION_RECV: u32 = 3;
const COMPLETION_DISPOSE: u32 = 6;
const ERROR_CONNECT: u32 = 12;
const ERROR_ARGUMENT: u32 = 10;
const ERROR_ARGUMENT_NULL: u32 = 14;
const ERROR_NOT_SUPPORTED: u32 = 15;
const ERROR_HOST_CHANGED: u32 = 18;
const CALL_UNARY: u32 = 0;
const CALL_SERVER_STREAM: u32 = 1;
const FRAME_RESPONSE: u8 = 2;
const FRAME_DATA: u8 = 3;
const FLAG_REMOTE_CLOSED: u8 = 1;
const FLAG_NO_DATA: u8 = 4;
const LAUNCH_GUEST_DEBUG: u32 = 1;
const LAUNCH_HAS_MEMORY: u32 = 4;
const PLAN_VALIDATE_ONLY: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvxStr {
    data: *const u8,
    length: usize,
    present: u32,
    reserved: u32,
}

impl NvxStr {
    fn present(text: &str) -> Self {
        Self {
            data: text.as_ptr(),
            length: text.len(),
            present: 1,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct NvxLaunchRequest {
    struct_size: u32,
    flags: u32,
    cpus: i32,
    memory_mb: i32,
    kernel_path: NvxStr,
    initrd_path: NvxStr,
    control_socket_path: NvxStr,
    boot_console_socket_path: NvxStr,
    hypervisor: NvxStr,
    vhd_path: NvxStr,
    scratch_vhd_path: NvxStr,
    isolation_profile: NvxStr,
    startup_mode: NvxStr,
}

#[repr(C)]
struct NvxCompletion {
    struct_size: u32,
    kind: u32,
    token: u64,
    result: i32,
    frame_type: u8,
    frame_flags: u8,
    reserved: u16,
    handle: u64,
    value: u64,
    data: *const u8,
    data_length: usize,
    error: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
struct NvxErrorEntry {
    struct_size: u32,
    kind: u32,
    reason: u32,
    has_os_error: u32,
    os_error: i32,
    reserved: u32,
    identity: u64,
    external_token: u64,
    message: *const u8,
    message_length: usize,
    param: *const u8,
    param_length: usize,
    data: *const u8,
    data_length: usize,
}

const _: () = {
    assert!(size_of::<NvxStr>() == 24);
    assert!(size_of::<NvxLaunchRequest>() == 232);
    assert!(size_of::<NvxCompletion>() == 64);
    assert!(size_of::<NvxErrorEntry>() == 88);
    assert!(std::mem::offset_of!(NvxCompletion, error) == 56);
    assert!(std::mem::offset_of!(NvxLaunchRequest, hypervisor) == 112);
};

type GetVersion = unsafe extern "C" fn() -> u32;
type RuntimeCreate = unsafe extern "C" fn(u32, *mut u64, *mut *mut c_void) -> i32;
type RuntimeNext = unsafe extern "C" fn(u64, u32, *mut *mut NvxCompletion) -> i32;
type CompletionRelease = unsafe extern "C" fn(*mut NvxCompletion);
type OperationCancel = unsafe extern "C" fn(u64, u64) -> i32;
type RuntimeFree = unsafe extern "C" fn(u64) -> i32;
type ErrorCount = unsafe extern "C" fn(*const c_void) -> u32;
type ErrorGet = unsafe extern "C" fn(*const c_void, u32, *mut NvxErrorEntry) -> i32;
type ErrorFree = unsafe extern "C" fn(*mut c_void);
type SessionConnectVerified =
    unsafe extern "C" fn(u64, *const u8, usize, *const u8, usize, u32, u64) -> i32;
type SessionDispose = unsafe extern "C" fn(u64, u64) -> i32;
type SessionRelease = unsafe extern "C" fn(u64) -> i32;
type CallOpen =
    unsafe extern "C" fn(u64, *const u8, usize, u32, *mut u64, *mut u32, *mut *mut c_void) -> i32;
type CallSendRequest = unsafe extern "C" fn(u64, *const u8, usize, u8, i64, u32, u64) -> i32;
type CallRecv = unsafe extern "C" fn(u64, u64) -> i32;
type CallTryComplete = unsafe extern "C" fn(u64) -> i32;
type CallRelease = unsafe extern "C" fn(u64) -> i32;
type BuildRamfsArgumentsWithPlan = unsafe extern "C" fn(
    *const NvxLaunchRequest,
    *const u8,
    usize,
    *mut *mut u8,
    *mut usize,
    *mut *mut c_void,
) -> i32;
type PlanSandbox = unsafe extern "C" fn(
    *const u8,
    usize,
    *const u8,
    usize,
    u32,
    *mut *mut u8,
    *mut usize,
    *mut *mut c_void,
) -> i32;
type BufferFree = unsafe extern "C" fn(*mut u8, usize);

struct Exports {
    runtime_create: RuntimeCreate,
    runtime_next: RuntimeNext,
    completion_release: CompletionRelease,
    operation_cancel: OperationCancel,
    runtime_free: RuntimeFree,
    error_count: ErrorCount,
    error_get: ErrorGet,
    error_free: ErrorFree,
    session_connect_verified: SessionConnectVerified,
    session_dispose: SessionDispose,
    session_release: SessionRelease,
    call_open: CallOpen,
    call_send_request: CallSendRequest,
    call_recv: CallRecv,
    call_try_complete: CallTryComplete,
    call_release: CallRelease,
    build_ramfs_arguments_with_plan: BuildRamfsArgumentsWithPlan,
    plan_sandbox: PlanSandbox,
    buffer_free: BufferFree,
}

pub(crate) struct HostLibrary {
    _library: Library,
    exports: Exports,
}

impl std::fmt::Debug for HostLibrary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostLibrary")
            .finish_non_exhaustive()
    }
}

fn resolve<T: Copy>(library: &Library, name: &[u8]) -> Result<T> {
    // Function pointers remain valid because HostLibrary retains the loaded library.
    unsafe { library.get::<T>(name) }
        .map(|symbol| *symbol)
        .map_err(|error| {
            Error::backend_unavailable(format!(
                "nvxhost is missing required export {}",
                String::from_utf8_lossy(name).trim_end_matches('\0')
            ))
            .with_source(error)
        })
}

impl HostLibrary {
    pub(crate) fn load(path: &Path, expected_sha256: &[u8; 32]) -> Result<Arc<Self>> {
        if !path.is_absolute() {
            return Err(Error::backend_unavailable(
                "the nvxhost library path must be absolute",
            ));
        }
        let path = fs::canonicalize(path).map_err(|error| {
            Error::backend_unavailable(format!("cannot locate nvxhost at {}", path.display()))
                .with_source(error)
        })?;
        let bytes = fs::read(&path).map_err(|error| {
            Error::backend_unavailable(format!("cannot verify nvxhost at {}", path.display()))
                .with_source(error)
        })?;
        let actual: [u8; 32] = Sha256::digest(&bytes).into();
        if &actual != expected_sha256 {
            return Err(Error::backend_unavailable(format!(
                "nvxhost at {} does not match the approved SHA-256",
                path.display()
            )));
        }
        let library = unsafe { Library::new(&path) }.map_err(|error| {
            Error::backend_unavailable(format!("cannot load nvxhost from {}", path.display()))
                .with_source(error)
        })?;
        let version: GetVersion = resolve(&library, b"nvx_get_api_version\0")?;
        let actual_version = unsafe { version() };
        if actual_version != ABI_VERSION {
            return Err(Error::backend_unavailable(format!(
                "nvxhost ABI is {actual_version}, expected {ABI_VERSION}"
            )));
        }
        let exports = Exports {
            runtime_create: resolve(&library, b"nvx_runtime_create\0")?,
            runtime_next: resolve(&library, b"nvx_runtime_next_completion\0")?,
            completion_release: resolve(&library, b"nvx_completion_release\0")?,
            operation_cancel: resolve(&library, b"nvx_operation_cancel\0")?,
            runtime_free: resolve(&library, b"nvx_runtime_free\0")?,
            error_count: resolve(&library, b"nvx_error_count\0")?,
            error_get: resolve(&library, b"nvx_error_get\0")?,
            error_free: resolve(&library, b"nvx_error_free\0")?,
            session_connect_verified: resolve(&library, b"nvx_session_connect_verified\0")?,
            session_dispose: resolve(&library, b"nvx_session_dispose\0")?,
            session_release: resolve(&library, b"nvx_session_release\0")?,
            call_open: resolve(&library, b"nvx_call_open\0")?,
            call_send_request: resolve(&library, b"nvx_call_send_request\0")?,
            call_recv: resolve(&library, b"nvx_call_recv\0")?,
            call_try_complete: resolve(&library, b"nvx_call_try_complete\0")?,
            call_release: resolve(&library, b"nvx_call_release\0")?,
            build_ramfs_arguments_with_plan: resolve(
                &library,
                b"nvx_build_ramfs_launch_arguments_with_plan\0",
            )?,
            plan_sandbox: resolve(&library, b"nvx_plan_sandbox\0")?,
            buffer_free: resolve(&library, b"nvx_buffer_free\0")?,
        };
        Ok(Arc::new(Self {
            _library: library,
            exports,
        }))
    }

    /// Plans the host paths and network of `policy`, the JSON of a request's `filesystem` and
    /// `network` sections, for a guest at `guest_network`.
    ///
    /// Returns the plan's JSON, which the caller stores and passes back at launch, or `None`
    /// when `validate_only`, which checks the policy without touching the host. Policies that
    /// the library cannot enforce fail with `policy_validation`.
    pub(crate) fn plan_sandbox(
        &self,
        policy: &str,
        guest_network: &str,
        validate_only: bool,
    ) -> Result<Option<String>> {
        let mut buffer = ptr::null_mut();
        let mut length = 0;
        let mut error = ptr::null_mut();
        let status = unsafe {
            (self.exports.plan_sandbox)(
                policy.as_ptr(),
                policy.len(),
                guest_network.as_ptr(),
                guest_network.len(),
                if validate_only { PLAN_VALIDATE_ONLY } else { 0 },
                &mut buffer,
                &mut length,
                &mut error,
            )
        };
        if status != STATUS_OK || !error.is_null() {
            let failure = self.failure(error);
            if !error.is_null() {
                unsafe { (self.exports.error_free)(error) };
            }
            return Err(
                if matches!(
                    failure.kind,
                    ERROR_ARGUMENT | ERROR_ARGUMENT_NULL | ERROR_NOT_SUPPORTED
                ) {
                    Error::policy_validation(failure.message)
                } else {
                    failure.as_error("planning host paths and network", status)
                },
            );
        }
        if validate_only {
            return Ok(None);
        }
        if buffer.is_null() || length == 0 {
            return Err(Error::backend_error("nvxhost returned no sandbox plan"));
        }
        let plan = unsafe { slice::from_raw_parts(buffer, length).to_vec() };
        unsafe { (self.exports.buffer_free)(buffer, length) };
        String::from_utf8(plan).map(Some).map_err(|error| {
            Error::backend_error("nvxhost returned a non-UTF-8 sandbox plan").with_source(error)
        })
    }

    pub(crate) fn launch_arguments(&self, inputs: &LaunchInputs<'_>) -> Result<Vec<OsString>> {
        fn path_text(path: &Path) -> Result<&str> {
            path.to_str().ok_or_else(|| {
                Error::backend_unavailable(format!(
                    "the nvxhost ABI requires a UTF-8 artifact path: {}",
                    path.display()
                ))
            })
        }
        let kernel = path_text(inputs.kernel)?;
        let initrd = path_text(inputs.initrd)?;
        let image = path_text(inputs.image)?;
        let memory_mb = i32::try_from(inputs.memory_mb)
            .map_err(|_| Error::policy_validation("guest memory exceeds the nvxhost ABI range"))?;
        let request = NvxLaunchRequest {
            struct_size: size_of::<NvxLaunchRequest>() as u32,
            flags: LAUNCH_HAS_MEMORY
                | if inputs.guest_debug {
                    LAUNCH_GUEST_DEBUG
                } else {
                    0
                },
            memory_mb,
            kernel_path: NvxStr::present(kernel),
            initrd_path: NvxStr::present(initrd),
            control_socket_path: NvxStr::present(inputs.control),
            boot_console_socket_path: NvxStr::present(inputs.boot),
            hypervisor: NvxStr::present(inputs.hypervisor),
            vhd_path: NvxStr::present(image),
            ..NvxLaunchRequest::default()
        };
        let mut buffer = ptr::null_mut();
        let mut length = 0;
        let mut error = ptr::null_mut();
        let plan = inputs.plan.unwrap_or_default();
        let status = unsafe {
            (self.exports.build_ramfs_arguments_with_plan)(
                &request,
                plan.as_ptr(),
                plan.len(),
                &mut buffer,
                &mut length,
                &mut error,
            )
        };
        if status != STATUS_OK || !error.is_null() {
            let failure = self.failure(error);
            if !error.is_null() {
                unsafe { (self.exports.error_free)(error) };
            }
            return Err(if failure.kind == ERROR_HOST_CHANGED {
                Error::backend_error(format!(
                    "{}; deprovision the sandbox and provision it again",
                    failure.message
                ))
            } else {
                failure.as_error("building OpenVMM arguments", status)
            });
        }
        if buffer.is_null() || length == 0 {
            return Err(Error::backend_error(
                "nvxhost returned no OpenVMM launch arguments",
            ));
        }
        let encoded = unsafe { slice::from_raw_parts(buffer, length).to_vec() };
        unsafe { (self.exports.buffer_free)(buffer, length) };
        if encoded.last() != Some(&0) {
            return Err(Error::backend_error(
                "nvxhost returned an unterminated argument vector",
            ));
        }
        encoded[..encoded.len() - 1]
            .split(|&byte| byte == 0)
            .map(|bytes| {
                if bytes.is_empty() {
                    return Err(Error::backend_error(
                        "nvxhost returned an empty OpenVMM argument",
                    ));
                }
                String::from_utf8(bytes.to_vec())
                    .map(OsString::from)
                    .map_err(|error| {
                        Error::backend_error("nvxhost returned a non-UTF-8 OpenVMM argument")
                            .with_source(error)
                    })
            })
            .collect()
    }

    fn failure(&self, error: *mut c_void) -> NativeFailure {
        if error.is_null() || unsafe { (self.exports.error_count)(error) } == 0 {
            return NativeFailure {
                kind: 0,
                os_error: None,
                message: "nvxhost returned an error without details".to_owned(),
            };
        }
        let mut entry = NvxErrorEntry::default();
        if unsafe { (self.exports.error_get)(error, 0, &mut entry) } != STATUS_OK
            || entry.struct_size as usize != size_of::<NvxErrorEntry>()
        {
            return NativeFailure {
                kind: 0,
                os_error: None,
                message: "nvxhost returned an incompatible error layout".to_owned(),
            };
        }
        let message = if entry.message_length == 0 {
            "nvxhost returned an empty error message".to_owned()
        } else if entry.message.is_null() {
            "nvxhost returned an invalid error message pointer".to_owned()
        } else {
            String::from_utf8_lossy(unsafe {
                slice::from_raw_parts(entry.message, entry.message_length)
            })
            .into_owned()
        };
        NativeFailure {
            kind: entry.kind,
            os_error: (entry.has_os_error != 0).then_some(entry.os_error),
            message,
        }
    }

    fn check_status(&self, action: &str, status: i32, error: *mut c_void) -> Result<()> {
        if status == STATUS_OK && error.is_null() {
            return Ok(());
        }
        let failure = self.failure(error);
        if !error.is_null() {
            unsafe { (self.exports.error_free)(error) };
        }
        Err(failure.as_error(action, status))
    }
}

pub(crate) struct LaunchInputs<'a> {
    pub(crate) kernel: &'a Path,
    pub(crate) initrd: &'a Path,
    pub(crate) image: &'a Path,
    pub(crate) control: &'a str,
    pub(crate) boot: &'a str,
    pub(crate) hypervisor: &'a str,
    pub(crate) memory_mb: u32,
    pub(crate) guest_debug: bool,
    /// JSON of the sandbox plan from [`HostLibrary::plan_sandbox`], if the sandbox has one.
    pub(crate) plan: Option<&'a str>,
}

#[derive(Debug)]
struct NativeFailure {
    kind: u32,
    os_error: Option<i32>,
    message: String,
}

impl NativeFailure {
    fn as_error(&self, action: &str, status: i32) -> Error {
        Error::backend_error(format!(
            "nvxhost {action} failed (status {status}, kind {}): {}",
            self.kind, self.message
        ))
    }

    fn endpoint_unavailable(&self) -> bool {
        self.kind == ERROR_CONNECT
            && match self.os_error {
                #[cfg(windows)]
                Some(2 | 231) => true,
                #[cfg(target_os = "linux")]
                Some(2 | 111) => true,
                _ => false,
            }
    }
}

struct Completed {
    kind: u32,
    token: u64,
    result: i32,
    frame_type: u8,
    frame_flags: u8,
    handle: u64,
    data: Vec<u8>,
    error: Option<NativeFailure>,
}

#[derive(Clone, PartialEq, Message)]
struct TtrpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
}

#[derive(Clone, PartialEq, Message)]
struct TtrpcResponse {
    #[prost(message, optional, tag = "1")]
    status: Option<TtrpcStatus>,
    #[prost(bytes, tag = "2")]
    payload: Vec<u8>,
}

fn response_payload(method: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    let response = TtrpcResponse::decode(bytes).map_err(|error| {
        Error::backend_error(format!("{method} returned an invalid ttrpc response"))
            .with_source(error)
    })?;
    if let Some(status) = response.status
        && status.code != 0
    {
        return Err(Error::backend_error(format!(
            "{method} failed with guest status {}: {}",
            status.code, status.message
        )));
    }
    Ok(response.payload)
}

pub(crate) struct Session {
    host: Arc<HostLibrary>,
    runtime: u64,
    session: u64,
    next_token: u64,
}

fn disposal_deadline(operation: Option<Instant>, now: Instant) -> Instant {
    let maximum = now + Duration::from_secs(5);
    operation.map_or(maximum, |deadline| deadline.min(maximum))
}

impl Session {
    pub(crate) fn connect_verified(
        host: Arc<HostLibrary>,
        endpoint: &str,
        capability: &[u8; 32],
        expected_pid: u32,
        deadline: Instant,
        is_running: impl Fn() -> Result<bool>,
    ) -> Result<Self> {
        let mut runtime = 0;
        let mut error = ptr::null_mut();
        let status = unsafe { (host.exports.runtime_create)(2, &mut runtime, &mut error) };
        host.check_status("starting a native runtime", status, error)?;
        if runtime == 0 {
            return Err(Error::backend_error(
                "nvxhost returned an invalid runtime handle",
            ));
        }
        let mut client = Self {
            host,
            runtime,
            session: 0,
            next_token: 1,
        };
        loop {
            let token = client.token()?;
            let status = unsafe {
                (client.host.exports.session_connect_verified)(
                    client.runtime,
                    endpoint.as_ptr(),
                    endpoint.len(),
                    capability.as_ptr(),
                    capability.len(),
                    expected_pid,
                    token,
                )
            };
            client
                .host
                .check_status("connecting to OpenVMM", status, ptr::null_mut())?;
            let completed = client.next(COMPLETION_CONNECT, token, Some(deadline))?;
            if completed.result == RESULT_OK {
                if completed.handle == 0 || completed.data.len() != 16 {
                    if completed.handle != 0 {
                        unsafe { (client.host.exports.session_release)(completed.handle) };
                    }
                    return Err(Error::backend_error(
                        "nvxhost returned an invalid broker session identity",
                    ));
                }
                client.session = completed.handle;
                return Ok(client);
            }
            if completed.result != RESULT_ERROR {
                return Err(Error::backend_error(format!(
                    "nvxhost returned unexpected connection result {}",
                    completed.result
                )));
            }
            let failure = completed.error.ok_or_else(|| {
                Error::backend_error("nvxhost connection failed without error details")
            })?;
            if failure.endpoint_unavailable() && Instant::now() < deadline && is_running()? {
                thread::sleep(Duration::from_millis(25));
                continue;
            }
            return Err(failure.as_error("connecting to OpenVMM", completed.result));
        }
    }

    fn token(&mut self) -> Result<u64> {
        let token = self.next_token;
        self.next_token = token
            .checked_add(1)
            .ok_or_else(|| Error::backend_error("nvxhost completion tokens have been exhausted"))?;
        Ok(token)
    }

    fn next(&self, kind: u32, token: u64, deadline: Option<Instant>) -> Result<Completed> {
        let mut completion = ptr::null_mut();
        let status = loop {
            let timeout_ms = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        unsafe { (self.host.exports.operation_cancel)(self.runtime, token) };
                        return Err(Error::backend_error("nvxhost operation timed out"));
                    }
                    u32::try_from(remaining.as_millis())
                        .unwrap_or(u32::MAX)
                        .max(1)
                }
                None => 1_000,
            };
            let status = unsafe {
                (self.host.exports.runtime_next)(self.runtime, timeout_ms, &mut completion)
            };
            if status == STATUS_TIMEOUT && deadline.is_none() {
                continue;
            }
            break status;
        };
        if status == STATUS_TIMEOUT {
            unsafe { (self.host.exports.operation_cancel)(self.runtime, token) };
            return Err(Error::backend_error("nvxhost operation timed out"));
        }
        self.host
            .check_status("waiting for a native completion", status, ptr::null_mut())?;
        if completion.is_null() {
            return Err(Error::backend_error(
                "nvxhost returned a null completion pointer",
            ));
        }
        let result = unsafe {
            let frame = &*completion;
            if frame.struct_size as usize != size_of::<NvxCompletion>() {
                Err(Error::backend_error(
                    "nvxhost returned an incompatible completion layout",
                ))
            } else if frame.data_length != 0 && frame.data.is_null() {
                Err(Error::backend_error(
                    "nvxhost returned an invalid completion payload pointer",
                ))
            } else {
                Ok(Completed {
                    kind: frame.kind,
                    token: frame.token,
                    result: frame.result,
                    frame_type: frame.frame_type,
                    frame_flags: frame.frame_flags,
                    handle: frame.handle,
                    data: if frame.data_length == 0 {
                        Vec::new()
                    } else {
                        slice::from_raw_parts(frame.data, frame.data_length).to_vec()
                    },
                    error: (!frame.error.is_null()).then(|| self.host.failure(frame.error)),
                })
            }
        };
        unsafe { (self.host.exports.completion_release)(completion) };
        let result = result?;
        if result.kind != kind || result.token != token {
            if result.kind == COMPLETION_CONNECT && result.handle != 0 {
                unsafe { (self.host.exports.session_release)(result.handle) };
            }
            return Err(Error::backend_error(
                "nvxhost returned a completion for a different operation",
            ));
        }
        Ok(result)
    }

    fn open_call(&self, method: &str, kind: u32) -> Result<Call> {
        let mut call = 0;
        let mut stream_id = 0;
        let mut error = ptr::null_mut();
        let status = unsafe {
            (self.host.exports.call_open)(
                self.session,
                method.as_ptr(),
                method.len(),
                kind,
                &mut call,
                &mut stream_id,
                &mut error,
            )
        };
        self.host
            .check_status("opening a guest RPC", status, error)?;
        if call == 0 || stream_id == 0 || stream_id & 1 == 0 {
            if call != 0 {
                unsafe { (self.host.exports.call_release)(call) };
            }
            return Err(Error::backend_error(
                "nvxhost returned an invalid RPC stream",
            ));
        }
        Ok(Call {
            host: Arc::clone(&self.host),
            handle: call,
        })
    }

    fn send(&mut self, call: &Call, payload: &[u8], deadline: Option<Instant>) -> Result<()> {
        let timeout_nanos = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(Error::backend_error(
                        "guest RPC deadline expired before dispatch",
                    ));
                }
                i64::try_from(remaining.as_nanos())
                    .map_err(|_| Error::backend_error("guest RPC deadline is too long"))?
            }
            None => 0,
        };
        let token = self.token()?;
        let status = unsafe {
            (self.host.exports.call_send_request)(
                call.handle,
                payload.as_ptr(),
                payload.len(),
                FLAG_REMOTE_CLOSED,
                timeout_nanos,
                0,
                token,
            )
        };
        self.host
            .check_status("sending a guest RPC", status, ptr::null_mut())?;
        let sent = self.next(COMPLETION_SEND, token, deadline)?;
        self.completed_ok("sending a guest RPC", &sent)
    }

    fn recv(&mut self, call: &Call, deadline: Option<Instant>) -> Result<Completed> {
        let token = self.token()?;
        let status = unsafe { (self.host.exports.call_recv)(call.handle, token) };
        self.host
            .check_status("receiving a guest RPC", status, ptr::null_mut())?;
        let frame = self.next(COMPLETION_RECV, token, deadline)?;
        self.completed_ok("receiving a guest RPC", &frame)?;
        Ok(frame)
    }

    fn completed_ok(&self, action: &str, completed: &Completed) -> Result<()> {
        if completed.result == RESULT_OK {
            return Ok(());
        }
        if completed.result != RESULT_ERROR {
            return Err(Error::backend_error(format!(
                "nvxhost {action} returned unexpected result {}",
                completed.result
            )));
        }
        Err(completed.error.as_ref().map_or_else(
            || Error::backend_error(format!("nvxhost {action} returned {}", completed.result)),
            |failure| failure.as_error(action, completed.result),
        ))
    }

    pub(crate) fn unary(
        &mut self,
        method: &str,
        payload: &[u8],
        timeout: Option<Duration>,
    ) -> Result<Vec<u8>> {
        let deadline = timeout
            .map(|timeout| {
                Instant::now()
                    .checked_add(timeout)
                    .ok_or_else(|| Error::backend_error("guest RPC timeout is too long"))
            })
            .transpose()?;
        let call = self.open_call(method, CALL_UNARY)?;
        self.send(&call, payload, deadline)?;
        let response = self.recv(&call, deadline)?;
        if response.frame_type != FRAME_RESPONSE {
            return Err(Error::backend_error(format!(
                "{method} returned unexpected ttrpc frame type {}",
                response.frame_type
            )));
        }
        if unsafe { (self.host.exports.call_try_complete)(call.handle) } != 1 {
            return Err(Error::backend_error(format!(
                "{method} did not complete its unary stream"
            )));
        }
        response_payload(method, &response.data)
    }

    pub(crate) fn server_stream(
        &mut self,
        method: &str,
        payload: &[u8],
        timeout: Duration,
        mut on_message: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Error::backend_error("guest RPC timeout is too long"))?;
        let call = self.open_call(method, CALL_SERVER_STREAM)?;
        self.send(&call, payload, Some(deadline))?;
        loop {
            let frame = self.recv(&call, Some(deadline))?;
            match frame.frame_type {
                FRAME_DATA => {
                    if frame.frame_flags & FLAG_NO_DATA == 0 {
                        on_message(&frame.data)?;
                    }
                    if frame.frame_flags & FLAG_REMOTE_CLOSED != 0 {
                        break;
                    }
                }
                FRAME_RESPONSE => {
                    let final_payload = response_payload(method, &frame.data)?;
                    if !final_payload.is_empty() {
                        on_message(&final_payload)?;
                    }
                    break;
                }
                other => {
                    return Err(Error::backend_error(format!(
                        "{method} returned unexpected ttrpc frame type {other}"
                    )));
                }
            }
        }
        if unsafe { (self.host.exports.call_try_complete)(call.handle) } != 1 {
            return Err(Error::backend_error(format!(
                "{method} did not complete its server stream"
            )));
        }
        Ok(())
    }

    pub(crate) fn close(mut self, deadline: Option<Instant>) -> Result<()> {
        let now = Instant::now();
        let deadline = disposal_deadline(deadline, now);
        if now >= deadline {
            return Err(Error::backend_error(
                "guest session disposal exceeded the operation deadline",
            ));
        }
        let token = self.token()?;
        let status = unsafe { (self.host.exports.session_dispose)(self.session, token) };
        self.host
            .check_status("closing a guest session", status, ptr::null_mut())?;
        let disposed = self.next(COMPLETION_DISPOSE, token, Some(deadline))?;
        self.completed_ok("closing a guest session", &disposed)?;
        let status = unsafe { (self.host.exports.session_release)(self.session) };
        self.host
            .check_status("releasing a guest session", status, ptr::null_mut())?;
        self.session = 0;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.session != 0 {
            let status = unsafe { (self.host.exports.session_release)(self.session) };
            if status != STATUS_OK {
                eprintln!("nvxhost failed to release guest session: {status}");
            }
        }
        if self.runtime != 0 {
            let status = unsafe { (self.host.exports.runtime_free)(self.runtime) };
            if status != STATUS_OK {
                eprintln!("nvxhost failed to release native runtime: {status}");
            }
        }
    }
}

struct Call {
    host: Arc<HostLibrary>,
    handle: u64,
}

impl Drop for Call {
    fn drop(&mut self) {
        let status = unsafe { (self.host.exports.call_release)(self.handle) };
        if status != STATUS_OK {
            eprintln!("nvxhost failed to release guest RPC: {status}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposal_never_extends_a_short_operation_deadline() {
        let now = Instant::now();
        let short = now + Duration::from_millis(100);
        assert_eq!(disposal_deadline(Some(short), now), short);
        let maximum = now + Duration::from_secs(5);
        assert_eq!(
            disposal_deadline(Some(now + Duration::from_secs(60)), now),
            maximum
        );
        assert_eq!(disposal_deadline(None, now), maximum);
        let expired = now - Duration::from_millis(1);
        assert_eq!(disposal_deadline(Some(expired), now), expired);
    }

    #[test]
    fn ttrpc_response_decodes_the_existing_wire_vector() {
        let bytes = [0x0a, 0x00, 0x12, 0x06, 0x7a, 0x04, b'r', b'u', b's', b't'];
        assert_eq!(
            response_payload("GetGuestInfo", &bytes).unwrap(),
            b"z\x04rust"
        );
    }

    #[test]
    fn missing_library_fails_explicitly() {
        let path = std::env::temp_dir().join("nvxhost-missing-file.dll");
        let error = HostLibrary::load(&path, &[0; 32]).unwrap_err();
        assert!(error.message().contains("cannot locate nvxhost"));
    }

    #[test]
    #[ignore = "set NVXHOST_TEST_LIBRARY to a separately built nvxhost DLL or shared library"]
    fn privately_built_library_exposes_the_ramfs_launch_abi() {
        let path = std::env::var_os("NVXHOST_TEST_LIBRARY")
            .map(std::path::PathBuf::from)
            .expect("NVXHOST_TEST_LIBRARY must name a private nvxhost build");
        let bytes = fs::read(&path).unwrap();
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        let host = HostLibrary::load(&path, &digest).unwrap();
        let error = HostLibrary::load(&path, &[0; 32]).unwrap_err();
        assert!(
            error
                .message()
                .contains("does not match the approved SHA-256")
        );
        let inputs = |plan| LaunchInputs {
            kernel: Path::new(r"C:\test\vmlinux"),
            initrd: Path::new(r"C:\test\edge-initramfs.cpio.gz"),
            image: Path::new(r"C:\test\image.gpt"),
            control: "//./pipe/openvmm-microvm-edge-control",
            boot: "//./pipe/openvmm-microvm-edge-boot",
            hypervisor: "whp",
            memory_mb: 256,
            guest_debug: false,
            plan,
        };
        let args = host.launch_arguments(&inputs(None)).unwrap();
        let args: Vec<_> = args
            .iter()
            .map(|argument| argument.to_str().unwrap())
            .collect();
        assert!(args.contains(&"--microvm-control-auth-stdin"));
        assert!(args.contains(&"distro:file:C:\\test\\image.gpt,ro"));
        assert_eq!(
            args.iter()
                .filter(|&&argument| argument == "--microvm-sandbox-block")
                .count(),
            1
        );
        assert!(!args.contains(&"--mount") && !args.contains(&"--net"));

        // Validation consults nothing on the host; unenforceable policies are policy errors.
        assert_eq!(host.plan_sandbox("{}", "10.0.0.2/24", true).unwrap(), None);
        let ipv6 = r#"{"network":{"egress":{"default":"allow","deny":[{"to":[{"cidr":"::/0"}]}]},"ingress":{"default":"deny"}}}"#;
        assert_eq!(
            host.plan_sandbox(ipv6, "10.0.0.2/24", true)
                .unwrap_err()
                .code(),
            crate::ErrorCode::PolicyValidation
        );
        // A plan maps a host directory and attaches a network device at launch.
        let directory = tempfile::tempdir().unwrap();
        let policy = serde_json::json!({
            "filesystem": { "readonlyPaths": [directory.path()] },
            "network": { "egress": { "default": "allow" }, "ingress": { "default": "deny" } },
        })
        .to_string();
        let plan = host
            .plan_sandbox(&policy, "10.0.0.2/24", false)
            .unwrap()
            .unwrap();
        let args = host.launch_arguments(&inputs(Some(&plan))).unwrap();
        let args: Vec<_> = args
            .iter()
            .map(|argument| argument.to_str().unwrap())
            .collect();
        assert!(args.contains(&"--mount") && args.contains(&"--net"));
        assert!(
            args.iter()
                .any(|argument| argument.contains("nvx_overlay_upper=ramfs nvx_map=.,"))
        );
    }
}
