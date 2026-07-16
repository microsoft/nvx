// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! XDP-for-Windows v1.3.0 AF_XDP L2Bridge backend.
//!
//! This is a dynamic implementation of the published v2 ABI in `xdpapi_v1.h`, `afxdp.h`, and
//! `afxdp_helper.h`. It intentionally loads `xdpapi.dll` and calls `XdpOpenApi(2)` rather than
//! linking an import library.

use ::core::ffi::c_void;
use ::std::fs::{File, OpenOptions};
use ::std::io::{BufReader, ErrorKind, Read, Write};
use ::std::pin::Pin;
use ::std::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};
use ::std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use ::std::sync::{Arc, Mutex};
use ::std::thread::{self, JoinHandle};
use ::std::time::{Duration, Instant};

use ::anyhow::{Context, Result, bail};
use ::serde::Deserialize;
use ::serde_json::json;
use ::windows::Win32::Foundation::{HANDLE as WinHandle, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use ::windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};
use ::windows::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};

use crate::l2bridge::{L2BridgeConfig, MAX_AFXDP_MTU, MAX_EXTERNAL_QUEUES};
use crate::whp::net::{BackendHealth, FrameBackend, FrameCounters};

const XDP_API_VERSION_2: u32 = 2;
const XDP_CREATE_PROGRAM_GENERIC: u32 = 0x1;
const XDP_CREATE_PROGRAM_ALL_QUEUES: u32 = 0x4;
const XSK_BIND_RX_GENERIC: u32 = 0x1 | 0x4;
const XSK_BIND_RX_TX_GENERIC: u32 = 0x1 | 0x2 | 0x4;
const XSK_NOTIFY_POKE_RX: u32 = 0x1;
const XSK_NOTIFY_POKE_TX: u32 = 0x2;
const XSK_NOTIFY_WAIT_RX: u32 = 0x4;
const XSK_NOTIFY_WAIT_TX: u32 = 0x8;
const XSK_RING_FLAG_ERROR: u32 = 0x1;
const XSK_RING_FLAG_NEED_POKE: u32 = 0x2;
const XSK_SOCKOPT_RX_HOOK_ID: u32 = 8;
const XSK_SOCKOPT_RX_ERROR: u32 = 10;
const XSK_SOCKOPT_RX_FILL_ERROR: u32 = 11;
const XSK_SOCKOPT_TX_ERROR: u32 = 12;
const XSK_SOCKOPT_TX_COMPLETION_ERROR: u32 = 13;
const XSK_RING_SIZE: u32 = 128;
const MAX_FRAME_SIZE: usize = 4096;
const RX_FRAME_COUNT: usize = 64;
const TX_FRAME_COUNT: usize = 64;
const RX_CHANNEL_DEPTH: usize = 256;
const MAX_QUEUES_PER_WORKER: usize = 63;
const HEALTH_POLL_INTERVAL_MS: u32 = 250;
const TX_BATCH: usize = 64;
const PIPE_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONTROL_MESSAGE: usize = 4096;
const STATE_READY: u8 = 0;
const STATE_QUIESCING: u8 = 1;
const STATE_STOPPED: u8 = 2;
const STATE_FAILED: u8 = 3;
const HRESULT_IO_PENDING: HResult = 0x8007_03e5_u32 as i32;
const HRESULT_OPERATION_ABORTED: HResult = 0x8007_03e3_u32 as i32;

type Handle = isize;
type HResult = i32;
type XdpOpenApiFn = unsafe extern "system" fn(u32, *mut *const XdpApiTable) -> HResult;
type XdpCloseApiFn = unsafe extern "system" fn(*const XdpApiTable);
type XdpGetRoutineFn = unsafe extern "system" fn(*const u8) -> *const c_void;
type XdpCreateProgramFn = unsafe extern "system" fn(
    u32,
    *const XdpHookId,
    u32,
    u32,
    *const XdpRule,
    u32,
    *mut Handle,
) -> HResult;
type XdpInterfaceOpenFn = unsafe extern "system" fn(u32, *mut Handle) -> HResult;
type XskCreateFn = unsafe extern "system" fn(*mut Handle) -> HResult;
type XskBindFn = unsafe extern "system" fn(Handle, u32, u32, u32) -> HResult;
type XskActivateFn = unsafe extern "system" fn(Handle, u32) -> HResult;
type XskNotifySocketFn = unsafe extern "system" fn(Handle, u32, u32, *mut u32) -> HResult;
type XskNotifyAsyncFn = unsafe extern "system" fn(Handle, u32, *mut OVERLAPPED) -> HResult;
type XskGetNotifyAsyncResultFn = unsafe extern "system" fn(*mut OVERLAPPED, *mut u32) -> HResult;
type XskSetSockoptFn = unsafe extern "system" fn(Handle, u32, *const c_void, u32) -> HResult;
type XskGetSockoptFn = unsafe extern "system" fn(Handle, u32, *mut c_void, *mut u32) -> HResult;
type XskIoctlFn =
    unsafe extern "system" fn(Handle, u32, *const c_void, u32, *mut c_void, *mut u32) -> HResult;
type XdpRssGetCapabilitiesFn =
    unsafe extern "system" fn(Handle, *mut XdpRssCapabilities, *mut u32) -> HResult;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryA(name: *const u8) -> Handle;
    fn GetProcAddress(module: Handle, name: *const u8) -> *const c_void;
    fn FreeLibrary(module: Handle) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
}

#[repr(C)]
struct XdpApiTable {
    open_api: Option<XdpOpenApiFn>,
    close_api: Option<XdpCloseApiFn>,
    get_routine: Option<XdpGetRoutineFn>,
    create_program: Option<XdpCreateProgramFn>,
    interface_open: Option<XdpInterfaceOpenFn>,
    xsk_create: Option<XskCreateFn>,
    xsk_bind: Option<XskBindFn>,
    xsk_activate: Option<XskActivateFn>,
    xsk_notify_socket: Option<XskNotifySocketFn>,
    xsk_notify_async: Option<XskNotifyAsyncFn>,
    xsk_get_notify_async_result: Option<XskGetNotifyAsyncResultFn>,
    xsk_set_sockopt: Option<XskSetSockoptFn>,
    xsk_get_sockopt: Option<XskGetSockoptFn>,
    _xsk_ioctl: Option<XskIoctlFn>,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct XdpObjectHeader {
    revision: u32,
    size: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct XdpRssCapabilities {
    header: XdpObjectHeader,
    flags: u32,
    hash_types: u32,
    hash_secret_key_size: u32,
    number_of_receive_queues: u32,
    number_of_indirection_table_entries: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XdpHookId {
    layer: u32,
    direction: u32,
    sublayer: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XdpRedirect {
    target_type: u32,
    _padding: u32,
    target: Handle,
}

#[repr(C)]
union XdpRuleTarget {
    redirect: XdpRedirect,
    _ebpf_target: Handle,
}

#[repr(C)]
union XdpMatchPattern {
    _bytes: [u8; 40],
    // `XDP_IP_PORT_SET` contains pointers, so the published C union is 8-byte aligned.
    _alignment: u64,
}

#[repr(C)]
struct XdpRule {
    match_type: u32,
    pattern: XdpMatchPattern,
    action: u32,
    target: XdpRuleTarget,
}

#[repr(C)]
struct XskUmemReg {
    total_size: u64,
    chunk_size: u32,
    headroom: u32,
    address: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XskRingInfo {
    ring: *mut u8,
    descriptors_offset: u32,
    producer_index_offset: u32,
    consumer_index_offset: u32,
    flags_offset: u32,
    size: u32,
    element_stride: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XskRingInfoSet {
    fill: XskRingInfo,
    completion: XskRingInfo,
    rx: XskRingInfo,
    tx: XskRingInfo,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XskBufferDescriptor {
    address_and_offset: u64,
    length: u32,
    reserved: u32,
}

/// The Agent-hosted control pipe. NVX is always the client.
pub struct ControlPipe {
    reader: Mutex<BufReader<File>>,
    writer: Mutex<File>,
}

impl ControlPipe {
    pub fn connect(path: &str) -> Result<Self> {
        let deadline = Instant::now() + PIPE_CONNECT_TIMEOUT;
        loop {
            match OpenOptions::new().read(true).write(true).open(path) {
                Ok(file) => {
                    let reader = BufReader::new(file.try_clone().context("cloning control pipe")?);
                    return Ok(Self {
                        reader: Mutex::new(reader),
                        writer: Mutex::new(file),
                    });
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::WouldBlock)
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("connecting to control pipe {path:?}"));
                }
            }
        }
    }

    pub fn data_plane_ready(&self, queues: &[u32]) -> Result<()> {
        self.write_message(json!({ "type": "DataPlaneReady", "queues": queues }))
    }

    pub fn data_plane_error(&self, message: &str) {
        let _ = self.write_message(json!({ "type": "DataPlaneError", "message": message }));
    }

    pub fn wait_start_vm(&self) -> Result<()> {
        let mut reader = self.reader.lock().expect("control pipe reader poisoned");
        let mut bytes = Vec::with_capacity(MAX_CONTROL_MESSAGE);
        let mut byte = [0u8; 1];
        loop {
            let read = reader
                .read(&mut byte)
                .context("reading StartVm from control pipe")?;
            if read == 0 {
                bail!("control pipe closed before StartVm");
            }
            if byte[0] == b'\n' {
                break;
            }
            if bytes.len() == MAX_CONTROL_MESSAGE {
                bail!("control-pipe message exceeds {MAX_CONTROL_MESSAGE} bytes");
            }
            bytes.push(byte[0]);
        }
        let line = ::std::str::from_utf8(&bytes).context("control-pipe command is not UTF-8")?;
        let command: StartVm =
            ::serde_json::from_str(line).context("invalid control-pipe command")?;
        if command.kind != "StartVm" {
            bail!("expected StartVm, got {:?}", command.kind);
        }
        Ok(())
    }

    fn write_message(&self, message: ::serde_json::Value) -> Result<()> {
        let mut writer = self.writer.lock().expect("control pipe writer poisoned");
        ::serde_json::to_writer(&mut *writer, &message)?;
        writer.write_all(b"\n")?;
        writer.flush().context("flushing control-pipe message")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartVm {
    #[serde(rename = "type")]
    kind: String,
}

struct XdpApi {
    module: Handle,
    table: *const XdpApiTable,
}

unsafe impl Send for XdpApi {}
unsafe impl Sync for XdpApi {}

impl XdpApi {
    fn load() -> Result<Arc<Self>> {
        // The strings include their NUL terminators as required by LoadLibraryA/GetProcAddress.
        let module = unsafe { LoadLibraryA(c"xdpapi.dll".as_ptr().cast()) };
        if module == 0 {
            bail!("loading xdpapi.dll failed; install signed XDP-for-Windows v1.3.0");
        }
        let proc = unsafe { GetProcAddress(module, c"XdpOpenApi".as_ptr().cast()) };
        if proc.is_null() {
            unsafe {
                FreeLibrary(module);
            }
            bail!("xdpapi.dll does not export XdpOpenApi");
        }
        let open: XdpOpenApiFn = unsafe { ::core::mem::transmute(proc) };
        let mut table = ::core::ptr::null();
        let status = unsafe { open(XDP_API_VERSION_2, &mut table) };
        if failed(status) || table.is_null() {
            unsafe {
                FreeLibrary(module);
            }
            bail!("XdpOpenApi(2) failed with HRESULT {status:#x}; XDP v1.3.0 is required");
        }
        let api = Arc::new(Self { module, table });
        api.required()?; // Verify the v2 table before any object is opened.
        Ok(api)
    }

    fn table(&self) -> &XdpApiTable {
        unsafe { &*self.table }
    }

    fn required(&self) -> Result<()> {
        let t = self.table();
        if t.close_api.is_none()
            || t.get_routine.is_none()
            || t.create_program.is_none()
            || t.interface_open.is_none()
            || t.xsk_create.is_none()
            || t.xsk_bind.is_none()
            || t.xsk_activate.is_none()
            || t.xsk_notify_socket.is_none()
            || t.xsk_notify_async.is_none()
            || t.xsk_get_notify_async_result.is_none()
            || t.xsk_set_sockopt.is_none()
            || t.xsk_get_sockopt.is_none()
        {
            bail!("XdpOpenApi(2) returned an incomplete XDP_API_TABLE");
        }
        Ok(())
    }

    fn automatic_queue_ids(&self, if_index: u32) -> Result<Vec<u32>> {
        let routine = unsafe {
            self.table().get_routine.unwrap()(c"XdpRssGetCapabilitiesExperimental".as_ptr().cast())
        };
        if routine.is_null() {
            bail!("XDP v1.3 does not expose RSS queue capabilities; use explicit queueSelection");
        }
        let get_capabilities: XdpRssGetCapabilitiesFn = unsafe { ::core::mem::transmute(routine) };
        let mut interface = 0;
        check(
            unsafe { self.table().interface_open.unwrap()(if_index, &mut interface) },
            "XdpInterfaceOpen",
        )?;
        let _interface = OwnedHandle(interface);
        let mut capabilities = XdpRssCapabilities {
            header: XdpObjectHeader {
                revision: 1,
                // Revision 1 ends at NumberOfReceiveQueues.
                size: 24,
            },
            ..Default::default()
        };
        let mut size = size_of::<XdpRssCapabilities>() as u32;
        check(
            unsafe { get_capabilities(interface, &mut capabilities, &mut size) },
            "XdpRssGetCapabilitiesExperimental",
        )?;
        automatic_queue_ids_from_count(capabilities.number_of_receive_queues)
    }
}

impl Drop for XdpApi {
    fn drop(&mut self) {
        unsafe {
            if let Some(close) = (*self.table).close_api {
                close(self.table);
            }
            FreeLibrary(self.module);
        }
    }
}

struct OwnedHandle(Handle);

impl OwnedHandle {
    fn win_handle(&self) -> WinHandle {
        WinHandle(self.0 as *mut c_void)
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct Event(OwnedHandle);

impl Event {
    fn new() -> Result<Self> {
        let handle =
            unsafe { CreateEventW(None, true, false, None) }.context("creating AF_XDP event")?;
        Ok(Self(OwnedHandle(handle.0 as Handle)))
    }

    fn handle(&self) -> WinHandle {
        self.0.win_handle()
    }

    fn set(&self) -> Result<()> {
        unsafe { SetEvent(self.handle()) }.context("signalling AF_XDP event")
    }

    fn reset(&self) -> Result<()> {
        unsafe { ResetEvent(self.handle()) }.context("resetting AF_XDP event")
    }
}

struct Ring {
    producer: *mut AtomicU32,
    consumer: *mut AtomicU32,
    flags: *mut AtomicU32,
    elements: *mut u8,
    size: u32,
    mask: u32,
    stride: usize,
    cached_producer: u32,
    cached_consumer: u32,
}

// Each ring is moved to exactly one per-queue worker. The XDP driver owns the other side of the
// shared indices, synchronized by acquire/release operations below.
unsafe impl Send for Ring {}

impl Ring {
    unsafe fn new(info: XskRingInfo) -> Result<Self> {
        if info.ring.is_null()
            || info.size == 0
            || !info.size.is_power_of_two()
            || info.element_stride == 0
        {
            bail!("XDP returned invalid AF_XDP ring metadata");
        }
        Ok(Self {
            producer: unsafe { info.ring.add(info.producer_index_offset as usize).cast() },
            consumer: unsafe { info.ring.add(info.consumer_index_offset as usize).cast() },
            flags: unsafe { info.ring.add(info.flags_offset as usize).cast() },
            elements: unsafe { info.ring.add(info.descriptors_offset as usize) },
            size: info.size,
            mask: info.size - 1,
            stride: info.element_stride as usize,
            cached_producer: unsafe {
                (&*info
                    .ring
                    .add(info.producer_index_offset as usize)
                    .cast::<AtomicU32>())
                    .load(Ordering::Acquire)
            },
            cached_consumer: unsafe {
                (&*info
                    .ring
                    .add(info.consumer_index_offset as usize)
                    .cast::<AtomicU32>())
                    .load(Ordering::Acquire)
            },
        })
    }

    fn element<T>(&self, index: u32) -> *mut T {
        unsafe {
            self.elements
                .add(((index & self.mask) as usize) * self.stride)
                .cast()
        }
    }

    fn consumer_reserve(&mut self, max: u32) -> (u32, u32) {
        let consumer = unsafe { (&*self.consumer).load(Ordering::Relaxed) };
        let mut available = self.cached_producer.wrapping_sub(consumer);
        if available < max {
            self.cached_producer = unsafe { (&*self.producer).load(Ordering::Acquire) };
            available = self.cached_producer.wrapping_sub(consumer);
        }
        (consumer, available.min(max))
    }

    fn consumer_release(&self, count: u32) {
        unsafe {
            (&*self.consumer).store(
                (&*self.consumer)
                    .load(Ordering::Relaxed)
                    .wrapping_add(count),
                Ordering::Release,
            );
        }
    }

    fn producer_reserve(&mut self, max: u32) -> (u32, u32) {
        let producer = unsafe { (&*self.producer).load(Ordering::Relaxed) };
        let mut available = self
            .size
            .wrapping_sub(producer.wrapping_sub(self.cached_consumer));
        if available < max {
            self.cached_consumer = unsafe { (&*self.consumer).load(Ordering::Acquire) };
            available = self
                .size
                .wrapping_sub(producer.wrapping_sub(self.cached_consumer));
        }
        (producer, available.min(max))
    }

    fn producer_submit(&self, count: u32) {
        unsafe {
            (&*self.producer).store(
                (&*self.producer)
                    .load(Ordering::Relaxed)
                    .wrapping_add(count),
                Ordering::Release,
            );
        }
    }

    fn has_error(&self) -> bool {
        unsafe { (&*self.flags).load(Ordering::Acquire) & XSK_RING_FLAG_ERROR != 0 }
    }

    fn needs_poke(&self) -> bool {
        // XDP requires a store-load barrier between publishing producer/consumer progress and
        // observing NEED_POKE, otherwise the driver and application can both go to sleep.
        fence(Ordering::SeqCst);
        unsafe { (&*self.flags).load(Ordering::Acquire) & XSK_RING_FLAG_NEED_POKE != 0 }
    }
}

struct NotifyWait {
    event: Event,
    overlapped: OVERLAPPED,
    pending: bool,
}

// The pinned allocation is moved only by pointer. A worker owns it exclusively, and cancellation
// completes every pending request before the allocation or socket is dropped.
unsafe impl Send for NotifyWait {}

impl NotifyWait {
    fn new() -> Result<Pin<Box<Self>>> {
        let event = Event::new()?;
        let mut wait = Box::pin(Self {
            event,
            overlapped: OVERLAPPED::default(),
            pending: false,
        });
        unsafe {
            Pin::as_mut(&mut wait).get_unchecked_mut().overlapped.hEvent =
                wait.as_ref().event.handle();
        }
        Ok(wait)
    }

    fn event_handle(self: Pin<&Self>) -> WinHandle {
        self.get_ref().event.handle()
    }

    fn arm(self: Pin<&mut Self>, api: &XdpApi, socket: Handle, flags: u32) -> Result<bool> {
        let this = unsafe { self.get_unchecked_mut() };
        if this.pending {
            return Ok(false);
        }
        this.event.reset()?;
        this.overlapped = OVERLAPPED::default();
        this.overlapped.hEvent = this.event.handle();
        let status =
            unsafe { api.table().xsk_notify_async.unwrap()(socket, flags, &mut this.overlapped) };
        if status == HRESULT_IO_PENDING {
            this.pending = true;
            return Ok(false);
        }
        check(status, "XskNotifyAsync")?;
        let mut result = 0;
        check(
            unsafe {
                api.table().xsk_get_notify_async_result.unwrap()(&mut this.overlapped, &mut result)
            },
            "XskGetNotifyAsyncResult",
        )?;
        Ok(true)
    }

    fn complete(self: Pin<&mut Self>, api: &XdpApi) -> Result<()> {
        let this = unsafe { self.get_unchecked_mut() };
        if !this.pending {
            return Ok(());
        }
        let mut result = 0;
        let status = unsafe {
            api.table().xsk_get_notify_async_result.unwrap()(&mut this.overlapped, &mut result)
        };
        this.pending = false;
        check(status, "XskGetNotifyAsyncResult")
    }

    fn cancel(self: Pin<&mut Self>, api: &XdpApi, socket: &OwnedHandle) -> Result<()> {
        let this = unsafe { self.get_unchecked_mut() };
        if !this.pending {
            return Ok(());
        }
        // XskNotifyAsync is an overlapped IOCTL. XDP's cancellation path completes the IRP and
        // signals this event, so waiting here keeps the OVERLAPPED address valid through teardown.
        let _ = unsafe { CancelIoEx(socket.win_handle(), Some(&this.overlapped)) };
        let wait = unsafe { WaitForSingleObject(this.event.handle(), INFINITE) };
        if wait != WAIT_OBJECT_0 {
            bail!(
                "waiting for cancelled AF_XDP notification failed with status {:#x}",
                wait.0
            );
        }
        let mut result = 0;
        let status = unsafe {
            api.table().xsk_get_notify_async_result.unwrap()(&mut this.overlapped, &mut result)
        };
        this.pending = false;
        if failed(status) && status != HRESULT_OPERATION_ABORTED {
            bail!("cancelled XskNotifyAsync failed with HRESULT {status:#x}");
        }
        Ok(())
    }
}

struct Queue {
    _api: Arc<XdpApi>,
    queue_id: u32,
    _rx_program: OwnedHandle,
    socket: OwnedHandle,
    umem: Vec<u8>,
    chunk_size: usize,
    fill: Ring,
    completion: Option<Ring>,
    rx: Ring,
    tx: Option<Ring>,
    tx_free: Vec<u64>,
    notify: Pin<Box<NotifyWait>>,
}

impl Queue {
    fn create(
        api: Arc<XdpApi>,
        if_index: u32,
        queue_id: u32,
        chunk_size: usize,
        tx_enabled: bool,
    ) -> Result<Self> {
        let t = api.table();
        let mut raw = 0;
        check(unsafe { t.xsk_create.unwrap()(&mut raw) }, "XskCreate")?;
        let socket = OwnedHandle(raw);
        let umem_size = queue_umem_size(chunk_size, tx_enabled)?;
        let mut umem = vec![0; umem_size];
        let reg = XskUmemReg {
            total_size: umem_size as u64,
            chunk_size: chunk_size as u32,
            headroom: 0,
            address: umem.as_mut_ptr().cast(),
        };
        check(
            unsafe {
                t.xsk_set_sockopt.unwrap()(
                    socket.0,
                    1,
                    (&reg as *const XskUmemReg).cast(),
                    size_of::<XskUmemReg>() as u32,
                )
            },
            "XSK_SOCKOPT_UMEM_REG",
        )?;
        check(
            unsafe {
                t.xsk_bind.unwrap()(
                    socket.0,
                    if_index,
                    queue_id,
                    if tx_enabled {
                        XSK_BIND_RX_TX_GENERIC
                    } else {
                        XSK_BIND_RX_GENERIC
                    },
                )
            },
            "XskBind",
        )?;
        for option in [2, 3] {
            check(
                unsafe {
                    t.xsk_set_sockopt.unwrap()(
                        socket.0,
                        option,
                        (&XSK_RING_SIZE as *const u32).cast(),
                        size_of::<u32>() as u32,
                    )
                },
                "XskSetSockopt ring size",
            )?;
        }
        if tx_enabled {
            for option in [4, 5] {
                check(
                    unsafe {
                        t.xsk_set_sockopt.unwrap()(
                            socket.0,
                            option,
                            (&XSK_RING_SIZE as *const u32).cast(),
                            size_of::<u32>() as u32,
                        )
                    },
                    "XskSetSockopt TX ring size",
                )?;
            }
        }
        check(
            unsafe { t.xsk_activate.unwrap()(socket.0, 0) },
            "XskActivate",
        )?;
        let mut info = unsafe { ::core::mem::zeroed::<XskRingInfoSet>() };
        let mut len = size_of::<XskRingInfoSet>() as u32;
        check(
            unsafe {
                t.xsk_get_sockopt.unwrap()(
                    socket.0,
                    6,
                    (&mut info as *mut XskRingInfoSet).cast(),
                    &mut len,
                )
            },
            "XSK_SOCKOPT_RING_INFO",
        )?;
        if len as usize != size_of::<XskRingInfoSet>() {
            bail!("unexpected XSK_RING_INFO size {len}");
        }
        let mut hook = XdpHookId {
            layer: 0,
            direction: 0,
            sublayer: 0,
        };
        let mut hook_len = size_of::<XdpHookId>() as u32;
        check(
            unsafe {
                t.xsk_get_sockopt.unwrap()(
                    socket.0,
                    XSK_SOCKOPT_RX_HOOK_ID,
                    (&mut hook as *mut XdpHookId).cast(),
                    &mut hook_len,
                )
            },
            "XSK_SOCKOPT_RX_HOOK_ID",
        )?;
        if hook_len as usize != size_of::<XdpHookId>() {
            bail!("unexpected XSK RX hook size {hook_len}");
        }
        let mut queue = Self {
            _api: Arc::clone(&api),
            queue_id,
            _rx_program: OwnedHandle(create_rx_program(
                &api, if_index, queue_id, socket.0, &hook,
            )?),
            socket,
            umem,
            chunk_size,
            fill: unsafe { Ring::new(info.fill)? },
            completion: if tx_enabled {
                Some(unsafe { Ring::new(info.completion)? })
            } else {
                None
            },
            rx: unsafe { Ring::new(info.rx)? },
            tx: if tx_enabled {
                Some(unsafe { Ring::new(info.tx)? })
            } else {
                None
            },
            tx_free: if tx_enabled {
                (RX_FRAME_COUNT..RX_FRAME_COUNT + TX_FRAME_COUNT)
                    .map(|n| (n * chunk_size) as u64)
                    .collect()
            } else {
                Vec::new()
            },
            notify: NotifyWait::new()?,
        };
        queue.refill_rx(&api)?;
        Ok(queue)
    }

    fn refill_rx(&mut self, api: &XdpApi) -> Result<()> {
        let (start, count) = self.fill.producer_reserve(RX_FRAME_COUNT as u32);
        if count != RX_FRAME_COUNT as u32 {
            bail!("AF_XDP fill ring has insufficient initial capacity");
        }
        for i in 0..count {
            unsafe {
                *self.fill.element::<u64>(start + i) = (i as usize * self.chunk_size) as u64;
            }
        }
        self.fill.producer_submit(count);
        if self.fill.needs_poke() {
            self.notify_socket(api, XSK_NOTIFY_POKE_RX, "POKE_RX")?;
        }
        Ok(())
    }

    fn check_ring_errors(&self, api: &XdpApi) -> Result<()> {
        self.check_ring_error(api, &self.rx, XSK_SOCKOPT_RX_ERROR, "RX")?;
        self.check_ring_error(api, &self.fill, XSK_SOCKOPT_RX_FILL_ERROR, "RX fill")?;
        if let Some(tx) = &self.tx {
            self.check_ring_error(api, tx, XSK_SOCKOPT_TX_ERROR, "TX")?;
        }
        if let Some(completion) = &self.completion {
            self.check_ring_error(
                api,
                completion,
                XSK_SOCKOPT_TX_COMPLETION_ERROR,
                "TX completion",
            )?;
        }
        Ok(())
    }

    fn check_ring_error(&self, api: &XdpApi, ring: &Ring, option: u32, name: &str) -> Result<()> {
        if !ring.has_error() {
            return Ok(());
        }
        let mut code = 0_u32;
        let mut len = size_of::<u32>() as u32;
        check(
            unsafe {
                api.table().xsk_get_sockopt.unwrap()(
                    self.socket.0,
                    option,
                    (&mut code as *mut u32).cast(),
                    &mut len,
                )
            },
            "reading AF_XDP ring error",
        )?;
        if len as usize != size_of::<u32>() {
            bail!(
                "AF_XDP queue {} returned invalid {name} error size {len}",
                self.queue_id
            );
        }
        bail!(
            "AF_XDP queue {} {name} ring entered a terminal state (XSK error {code:#x})",
            self.queue_id
        );
    }

    fn notify_socket(&self, api: &XdpApi, flags: u32, operation: &str) -> Result<()> {
        let mut result = 0;
        check(
            unsafe { api.table().xsk_notify_socket.unwrap()(self.socket.0, flags, 0, &mut result) },
            operation,
        )?;
        if result != 0 {
            bail!("{operation} returned unexpected result flags {result:#x}");
        }
        Ok(())
    }

    fn reclaim_tx(&mut self, inflight: &AtomicUsize) -> Result<bool> {
        let Some(completion) = self.completion.as_mut() else {
            return Ok(false);
        };
        let (start, count) = completion.consumer_reserve(XSK_RING_SIZE);
        for i in 0..count {
            let address = unsafe { *completion.element::<u64>(start + i) } & 0x0000_ffff_ffff_ffff;
            let offset = address as usize;
            let tx_base = RX_FRAME_COUNT * self.chunk_size;
            if offset < tx_base
                || offset >= self.umem.len()
                || !offset.is_multiple_of(self.chunk_size)
            {
                bail!(
                    "AF_XDP queue {} returned invalid TX completion address {address:#x}",
                    self.queue_id
                );
            }
            let previous = inflight
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    value.checked_sub(1)
                })
                .map_err(|_| ::anyhow::anyhow!("AF_XDP TX inflight counter underflow"))?;
            debug_assert!(previous > 0);
            self.tx_free.push(address);
        }
        if count != 0 {
            completion.consumer_release(count);
        }
        Ok(count != 0)
    }

    fn receive(
        &mut self,
        api: &XdpApi,
        out: &SyncSender<Vec<u8>>,
        counters: &Counters,
        quiescing: bool,
    ) -> Result<bool> {
        let (start, count) = self.rx.consumer_reserve(XSK_RING_SIZE);
        if count == 0 {
            return Ok(false);
        }
        let (fill_start, fill_count) = self.fill.producer_reserve(count);
        if fill_count != count {
            bail!(
                "AF_XDP queue {} fill ring cannot recycle {count} RX buffers",
                self.queue_id
            );
        }
        for i in 0..count {
            let descriptor = unsafe { *self.rx.element::<XskBufferDescriptor>(start + i) };
            let base = descriptor.address_and_offset & 0x0000_ffff_ffff_ffff;
            if base as usize >= RX_FRAME_COUNT * self.chunk_size
                || !(base as usize).is_multiple_of(self.chunk_size)
            {
                bail!(
                    "AF_XDP RX queue {} returned invalid buffer address {base:#x}",
                    self.queue_id
                );
            }
            let offset = (base + (descriptor.address_and_offset >> 48)) as usize;
            let end = offset.saturating_add(descriptor.length as usize);
            if end > self.umem.len() {
                bail!(
                    "AF_XDP RX queue {} returned an out-of-range descriptor raw={:#x} length={} umem={}",
                    self.queue_id,
                    descriptor.address_and_offset,
                    descriptor.length,
                    self.umem.len()
                );
            }
            if descriptor.length < 14 {
                log::error!(
                    "virt-net: AF_XDP RX queue {} rejected short frame raw={:#x} offset={} length={}",
                    self.queue_id,
                    descriptor.address_and_offset,
                    offset,
                    descriptor.length,
                );
                counters.rx_dropped.fetch_add(1, Ordering::Relaxed);
            } else if quiescing {
                counters.rx_dropped.fetch_add(1, Ordering::Relaxed);
            } else {
                let frame = &self.umem[offset..end];
                if out.try_send(frame.to_vec()).is_ok() {
                    counters.rx_received.fetch_add(1, Ordering::Relaxed);
                } else {
                    log::error!(
                        "virt-net: AF_XDP RX queue {} dropped a frame because the guest channel is full",
                        self.queue_id
                    );
                    counters.rx_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            unsafe {
                *self.fill.element::<u64>(fill_start + i) = base;
            }
        }
        self.fill.producer_submit(count);
        self.rx.consumer_release(count);
        if self.fill.needs_poke() {
            self.notify_socket(api, XSK_NOTIFY_POKE_RX, "XskNotifySocket POKE_RX")?;
        }
        Ok(true)
    }

    fn transmit(&mut self, frame: &[u8]) -> bool {
        if frame.len() < 14 || frame.len() > self.chunk_size {
            return false;
        }
        let Some(tx) = self.tx.as_mut() else {
            return false;
        };
        let Some(address) = self.tx_free.pop() else {
            return false;
        };
        let (slot, available) = tx.producer_reserve(1);
        if available != 1 {
            self.tx_free.push(address);
            return false;
        }
        self.umem[address as usize..address as usize + frame.len()].copy_from_slice(frame);
        unsafe {
            *tx.element::<XskBufferDescriptor>(slot) = XskBufferDescriptor {
                address_and_offset: address,
                length: frame.len() as u32,
                reserved: 0,
            };
        }
        tx.producer_submit(1);
        true
    }

    fn poke_tx_if_needed(&self, api: &XdpApi) -> Result<()> {
        if self.tx.as_ref().is_some_and(Ring::needs_poke) {
            self.notify_socket(api, XSK_NOTIFY_POKE_TX, "XskNotifySocket POKE_TX")?;
        }
        Ok(())
    }

    fn arm_notify(&mut self, api: &XdpApi) -> Result<bool> {
        let flags = if self.tx.is_some() {
            XSK_NOTIFY_WAIT_RX | XSK_NOTIFY_WAIT_TX
        } else {
            XSK_NOTIFY_WAIT_RX
        };
        self.notify.as_mut().arm(api, self.socket.0, flags)
    }

    fn complete_notify(&mut self, api: &XdpApi) -> Result<()> {
        self.notify.as_mut().complete(api)
    }

    fn cancel_notify(&mut self, api: &XdpApi) -> Result<()> {
        self.notify.as_mut().cancel(api, &self.socket)
    }

    fn notify_handle(&self) -> WinHandle {
        self.notify.as_ref().event_handle()
    }
}

struct Counters {
    tx_accepted: AtomicU64,
    tx_dropped: AtomicU64,
    rx_received: AtomicU64,
    rx_dropped: AtomicU64,
    errors: AtomicU64,
}

struct SharedState {
    stop: AtomicBool,
    state: AtomicU8,
    inflight: AtomicUsize,
    counters: Counters,
    failure: Mutex<Option<String>>,
}

impl SharedState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            state: AtomicU8::new(STATE_READY),
            inflight: AtomicUsize::new(0),
            counters: Counters {
                tx_accepted: AtomicU64::new(0),
                tx_dropped: AtomicU64::new(0),
                rx_received: AtomicU64::new(0),
                rx_dropped: AtomicU64::new(0),
                errors: AtomicU64::new(0),
            },
            failure: Mutex::new(None),
        })
    }

    fn fail(&self, message: impl Into<String>) {
        if self.state.load(Ordering::Acquire) == STATE_STOPPED {
            return;
        }
        let mut failure = self.failure.lock().expect("AF_XDP failure lock poisoned");
        if failure.is_none() {
            let message = message.into();
            log::error!("virt-net: {message}");
            *failure = Some(message);
            self.counters.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.state.store(STATE_FAILED, Ordering::Release);
        self.stop.store(true, Ordering::Release);
    }

    fn failure_message(&self) -> String {
        self.failure
            .lock()
            .expect("AF_XDP failure lock poisoned")
            .clone()
            .unwrap_or_else(|| "AF_XDP backend failed".to_owned())
    }
}

struct Afxdp {
    _api: Arc<XdpApi>,
    tx_guard: Mutex<Option<OwnedHandle>>,
    tx_sender: SyncSender<Vec<u8>>,
    tx_wake: Arc<Event>,
    receiver: Mutex<Receiver<Vec<u8>>>,
    max_frame_size: usize,
    shared: Arc<SharedState>,
    controls: Vec<Arc<Event>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl Afxdp {
    fn new(
        api: Arc<XdpApi>,
        guard: OwnedHandle,
        queues: Vec<Queue>,
        max_frame_size: usize,
    ) -> Result<Arc<Self>> {
        let (out, receiver) = sync_channel(RX_CHANNEL_DEPTH);
        let (tx_sender, tx_receiver) = sync_channel(TX_FRAME_COUNT);
        let shared = SharedState::new();
        let mut tx_receiver = Some(tx_receiver);
        let mut controls: Vec<Arc<Event>> = Vec::new();
        let mut workers: Vec<JoinHandle<()>> = Vec::new();
        let mut queue_iter = queues.into_iter();
        let mut worker_index = 0;
        loop {
            let shard = queue_iter
                .by_ref()
                .take(MAX_QUEUES_PER_WORKER)
                .collect::<Vec<_>>();
            if shard.is_empty() {
                break;
            }
            let control = match Event::new() {
                Ok(event) => Arc::new(event),
                Err(error) => {
                    shared.stop.store(true, Ordering::Release);
                    for control in &controls {
                        let _ = control.set();
                    }
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(error);
                }
            };
            controls.push(Arc::clone(&control));
            let api = Arc::clone(&api);
            let worker_shared = Arc::clone(&shared);
            let out = out.clone();
            let input = if worker_index == 0 {
                tx_receiver.take()
            } else {
                None
            };
            match thread::Builder::new()
                .name(format!("whp-afxdp-{worker_index}"))
                .spawn(move || worker(api, shard, input, out, control, worker_shared))
            {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    shared.stop.store(true, Ordering::Release);
                    for control in &controls {
                        let _ = control.set();
                    }
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(error).context("spawning AF_XDP dispatcher");
                }
            }
            worker_index += 1;
        }
        let tx_wake = Arc::clone(
            controls
                .first()
                .context("AF_XDP backend has no dispatcher")?,
        );
        Ok(Arc::new(Self {
            _api: api,
            tx_guard: Mutex::new(Some(guard)),
            tx_sender,
            tx_wake,
            receiver: Mutex::new(receiver),
            max_frame_size,
            shared,
            controls,
            workers: Mutex::new(workers),
        }))
    }

    fn signal_controls(&self) {
        for control in &self.controls {
            if let Err(error) = control.set() {
                self.shared
                    .fail(format!("failed to wake AF_XDP dispatcher: {error:#}"));
            }
        }
    }

    fn stop_workers(&self) {
        self.shared.stop.store(true, Ordering::Release);
        self.signal_controls();
        for worker in self
            .workers
            .lock()
            .expect("AF_XDP workers poisoned")
            .drain(..)
        {
            if worker.join().is_err() {
                self.shared.fail("AF_XDP dispatcher panicked");
            }
        }
        // RX redirect programs and XSKs are dropped by their workers before this all-queue TX
        // guard is removed. The API table/library is retained until the backend drops.
        self.tx_guard.lock().expect("AF_XDP guard poisoned").take();
    }
}

impl FrameBackend for Afxdp {
    fn try_send(&self, frame: Vec<u8>) -> bool {
        if self.shared.state.load(Ordering::Acquire) != STATE_READY
            || frame.len() < 14
            || frame.len() > self.max_frame_size
        {
            self.shared
                .counters
                .tx_dropped
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.shared.inflight.fetch_add(1, Ordering::AcqRel);
        match self.tx_sender.try_send(frame) {
            Ok(()) => {
                self.shared
                    .counters
                    .tx_accepted
                    .fetch_add(1, Ordering::Relaxed);
                if let Err(error) = self.tx_wake.set() {
                    self.shared
                        .fail(format!("failed to wake AF_XDP TX dispatcher: {error:#}"));
                }
                true
            }
            Err(_) => {
                self.shared.inflight.fetch_sub(1, Ordering::AcqRel);
                self.shared
                    .counters
                    .tx_dropped
                    .fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    fn recv_timeout(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.receiver
            .lock()
            .expect("AF_XDP RX receiver poisoned")
            .recv_timeout(timeout)
            .ok()
    }
    fn health(&self) -> BackendHealth {
        match self.shared.state.load(Ordering::Acquire) {
            STATE_READY => BackendHealth::Ready,
            STATE_QUIESCING => BackendHealth::Quiescing,
            STATE_STOPPED => BackendHealth::Stopped,
            _ => BackendHealth::Failed,
        }
    }
    fn check_health(&self) -> Result<()> {
        if self.shared.state.load(Ordering::Acquire) == STATE_FAILED {
            bail!("{}", self.shared.failure_message());
        }
        Ok(())
    }
    fn counters(&self) -> FrameCounters {
        FrameCounters {
            guest_tx_accepted: self.shared.counters.tx_accepted.load(Ordering::Relaxed),
            guest_tx_dropped: self.shared.counters.tx_dropped.load(Ordering::Relaxed),
            guest_rx_received: self.shared.counters.rx_received.load(Ordering::Relaxed),
            guest_rx_dropped: self.shared.counters.rx_dropped.load(Ordering::Relaxed),
            backend_errors: self.shared.counters.errors.load(Ordering::Relaxed),
        }
    }
    fn quiesce(&self, timeout: Duration) -> Result<()> {
        match self.shared.state.compare_exchange(
            STATE_READY,
            STATE_QUIESCING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(STATE_QUIESCING) => {}
            Err(STATE_FAILED) => return self.check_health(),
            Err(_) => return Ok(()),
        }
        self.signal_controls();
        let deadline = Instant::now() + timeout;
        while self.shared.inflight.load(Ordering::Acquire) != 0 {
            if self.shared.state.load(Ordering::Acquire) == STATE_FAILED {
                self.stop_workers();
                return self.check_health();
            }
            if Instant::now() >= deadline {
                self.shared.fail("timed out draining AF_XDP TX completions");
                self.stop_workers();
                return self.check_health();
            }
            thread::sleep(Duration::from_millis(1));
        }
        self.stop_workers();
        if self.shared.state.load(Ordering::Acquire) != STATE_FAILED {
            self.shared.state.store(STATE_STOPPED, Ordering::Release);
        }
        Ok(())
    }
    fn shutdown(&self) {
        if self.shared.state.load(Ordering::Acquire) != STATE_FAILED {
            self.shared.state.store(STATE_STOPPED, Ordering::Release);
        }
        self.stop_workers();
    }
}

impl Drop for Afxdp {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker(
    api: Arc<XdpApi>,
    mut queues: Vec<Queue>,
    input: Option<Receiver<Vec<u8>>>,
    output: SyncSender<Vec<u8>>,
    control: Arc<Event>,
    shared: Arc<SharedState>,
) {
    if let Err(error) = worker_loop(
        &api,
        &mut queues,
        input.as_ref(),
        &output,
        &control,
        &shared,
    ) && !shared.stop.load(Ordering::Acquire)
    {
        shared.fail(format!("AF_XDP dispatcher failed: {error:#}"));
    }
    for queue in &mut queues {
        if let Err(error) = queue.cancel_notify(&api) {
            log::error!(
                "virt-net: failed to cancel AF_XDP queue {} notification: {error:#}",
                queue.queue_id
            );
        }
    }
}

fn worker_loop(
    api: &XdpApi,
    queues: &mut [Queue],
    input: Option<&Receiver<Vec<u8>>>,
    output: &SyncSender<Vec<u8>>,
    control: &Event,
    shared: &SharedState,
) -> Result<()> {
    let mut pending_tx = None;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }

        let quiescing = shared.state.load(Ordering::Acquire) != STATE_READY;
        for queue in queues.iter_mut() {
            queue.check_ring_errors(api)?;
            if queue.reclaim_tx(&shared.inflight)? {
                queue.poke_tx_if_needed(api)?;
            }
            queue.receive(api, output, &shared.counters, quiescing)?;
        }

        if let Some(input) = input {
            let primary = queues
                .first_mut()
                .context("AF_XDP TX dispatcher has no primary queue")?;
            let mut submitted = 0;
            for _ in 0..TX_BATCH {
                let frame = match pending_tx.take() {
                    Some(frame) => frame,
                    None => match input.try_recv() {
                        Ok(frame) => frame,
                        Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                    },
                };
                if primary.transmit(&frame) {
                    submitted += 1;
                } else {
                    pending_tx = Some(frame);
                    break;
                }
            }
            if submitted != 0 {
                primary.poke_tx_if_needed(api)?;
            }
        }

        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }

        let mut completed_immediately = false;
        for queue in queues.iter_mut() {
            completed_immediately |= queue.arm_notify(api)?;
        }
        if completed_immediately {
            continue;
        }

        let mut handles = Vec::with_capacity(queues.len() + 1);
        // The control event has the lowest wait index so shutdown, quiesce, and guest TX cannot
        // be starved by continuously signalled RX queues.
        handles.push(control.handle());
        handles.extend(queues.iter().map(Queue::notify_handle));
        // Ring-error flags do not necessarily complete an idle XSK notify on interface detach.
        // A low-rate heartbeat bounds fault detection without returning to per-queue polling.
        let wait = unsafe { WaitForMultipleObjects(&handles, false, HEALTH_POLL_INTERVAL_MS) };
        if wait == WAIT_FAILED {
            bail!(
                "WaitForMultipleObjects failed: {}",
                ::std::io::Error::last_os_error()
            );
        }
        if wait == WAIT_TIMEOUT {
            continue;
        }
        let index = wait
            .0
            .checked_sub(WAIT_OBJECT_0.0)
            .context("unexpected AF_XDP wait result")? as usize;
        if index == 0 {
            // Reset before draining the TX channel. Any producer racing after this reset sets the
            // event again, so no enqueue notification can be lost.
            control.reset()?;
        } else {
            let queue = queues
                .get_mut(index - 1)
                .context("AF_XDP wait returned an invalid queue index")?;
            queue.complete_notify(api)?;
        }
    }
}

fn create_rx_program(
    api: &XdpApi,
    if_index: u32,
    queue: u32,
    socket: Handle,
    hook: &XdpHookId,
) -> Result<Handle> {
    let rule = XdpRule {
        match_type: 0,
        pattern: XdpMatchPattern { _bytes: [0; 40] },
        action: 2,
        target: XdpRuleTarget {
            redirect: XdpRedirect {
                target_type: 0,
                _padding: 0,
                target: socket,
            },
        },
    };
    let mut program = 0;
    check(
        unsafe {
            api.table().create_program.unwrap()(
                if_index,
                hook,
                queue,
                XDP_CREATE_PROGRAM_GENERIC,
                &rule,
                1,
                &mut program,
            )
        },
        "XdpCreateProgram RX redirect",
    )?;
    Ok(program)
}

fn create_tx_guard(api: &XdpApi, if_index: u32) -> Result<OwnedHandle> {
    let rule = XdpRule {
        match_type: 0,
        pattern: XdpMatchPattern { _bytes: [0; 40] },
        action: 0,
        target: XdpRuleTarget { _ebpf_target: 0 },
    };
    let hook = XdpHookId {
        layer: 0,
        direction: 1,
        sublayer: 0,
    };
    let mut program = 0;
    // GENERIC | ALL_QUEUES: suppress host-stack TX while AF_XDP injection remains below inspect.
    check(
        unsafe {
            api.table().create_program.unwrap()(
                if_index,
                &hook,
                0,
                XDP_CREATE_PROGRAM_GENERIC | XDP_CREATE_PROGRAM_ALL_QUEUES,
                &rule,
                1,
                &mut program,
            )
        },
        "XdpCreateProgram TX guard",
    )?;
    Ok(OwnedHandle(program))
}

fn failed(status: HResult) -> bool {
    status < 0
}
fn check(status: HResult, operation: &str) -> Result<()> {
    if failed(status) {
        bail!("{operation} failed with HRESULT {status:#x}");
    }
    Ok(())
}

fn frame_layout(mtu: u32) -> Result<(usize, usize)> {
    if mtu > MAX_AFXDP_MTU {
        bail!("device.mtu {mtu} exceeds the AF_XDP {MAX_AFXDP_MTU}-byte frame limit");
    }
    let max_frame_size = mtu as usize + 14;
    let chunk_size = max_frame_size.next_power_of_two();
    if chunk_size > MAX_FRAME_SIZE {
        bail!("AF_XDP frame size {max_frame_size} exceeds the {MAX_FRAME_SIZE}-byte UMEM limit");
    }
    Ok((max_frame_size, chunk_size))
}

fn queue_umem_size(chunk_size: usize, tx_enabled: bool) -> Result<usize> {
    let frame_count = RX_FRAME_COUNT + usize::from(tx_enabled) * TX_FRAME_COUNT;
    chunk_size
        .checked_mul(frame_count)
        .context("AF_XDP UMEM size overflow")
}

fn automatic_queue_ids_from_count(count: u32) -> Result<Vec<u32>> {
    // Non-RSS adapters report zero receive queues, but the generic provider still exposes queue 0.
    let count = count.max(1) as usize;
    if count > MAX_EXTERNAL_QUEUES {
        bail!(
            "AF_XDP automatic queue discovery returned {count} receive queues (supported range 1..={MAX_EXTERNAL_QUEUES})"
        );
    }
    Ok((0..count as u32).collect())
}

fn initialize_queues<T>(
    ids: &[u32],
    mut create: impl FnMut(usize, u32) -> Result<T>,
) -> Result<Vec<T>> {
    let mut queues = Vec::with_capacity(ids.len());
    for (index, id) in ids.iter().copied().enumerate() {
        queues.push(create(index, id).with_context(|| format!("initializing AF_XDP queue {id}"))?);
    }
    Ok(queues)
}

/// Resources returned only after every selected queue and the TX guard are active.
pub struct BackendStart {
    pub backend: Arc<dyn FrameBackend>,
    pub queues: Vec<u32>,
}

pub fn start(config: &L2BridgeConfig) -> Result<BackendStart> {
    let (max_frame_size, chunk_size) = frame_layout(config.device.mtu)?;
    let api = XdpApi::load()?;
    let selected = &config.attachment.queue_selection;
    let ids: Vec<u32> = if selected.mode == "explicit" {
        selected.queues.clone()
    } else {
        api.automatic_queue_ids(config.attachment.interface_index)?
    };
    if ids.is_empty() || ids.len() > MAX_EXTERNAL_QUEUES {
        bail!("AF_XDP queue selection must contain 1..={MAX_EXTERNAL_QUEUES} queues");
    }
    let queues = initialize_queues(&ids, |index, id| {
        Queue::create(
            Arc::clone(&api),
            config.attachment.interface_index,
            id,
            chunk_size,
            index == 0,
        )
    })?;
    let total_umem = queues.iter().map(|queue| queue.umem.len()).sum::<usize>();
    log::info!(
        "virt-net: AF_XDP initialized {} queues with {} dispatchers and {:.1} MiB UMEM",
        ids.len(),
        ids.len().div_ceil(MAX_QUEUES_PER_WORKER),
        total_umem as f64 / (1024.0 * 1024.0),
    );
    let guard = create_tx_guard(&api, config.attachment.interface_index)?;
    let backend: Arc<dyn FrameBackend> = Afxdp::new(api, guard, queues, max_frame_size)?;
    Ok(BackendStart {
        backend,
        queues: ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn v2_abi_layout_matches_published_headers() {
        assert_eq!(size_of::<XdpApiTable>(), 14 * size_of::<usize>());
        assert_eq!(size_of::<XdpHookId>(), 12);
        assert_eq!(size_of::<XdpRssCapabilities>(), 28);
        assert_eq!(size_of::<XskUmemReg>(), 24);
        assert_eq!(size_of::<XskBufferDescriptor>(), 16);
        assert_eq!(size_of::<XskRingInfo>(), 40);
        assert_eq!(size_of::<XskRingInfoSet>(), 160);
        assert_eq!(size_of::<XdpMatchPattern>(), 40);
        assert_eq!(size_of::<XdpRedirect>(), 16);
        assert_eq!(size_of::<XdpRule>(), 72);
        assert_eq!(size_of::<OVERLAPPED>(), 32);
    }
    #[test]
    fn ring_index_wraps_at_power_of_two_size() {
        assert_eq!(17_u32 & (8 - 1), 1);
    }

    #[test]
    fn ring_flags_report_error_and_need_poke() {
        #[repr(C)]
        struct MockRing {
            producer: AtomicU32,
            consumer: AtomicU32,
            flags: AtomicU32,
            descriptors: [u64; 8],
        }

        let mut memory = MockRing {
            producer: AtomicU32::new(0),
            consumer: AtomicU32::new(0),
            flags: AtomicU32::new(0),
            descriptors: [0; 8],
        };
        let base = (&mut memory as *mut MockRing).cast::<u8>();
        let offset = |field: *const u8| unsafe { field.offset_from(base) as u32 };
        let info = XskRingInfo {
            ring: base,
            descriptors_offset: offset(memory.descriptors.as_ptr().cast()),
            producer_index_offset: offset((&memory.producer as *const AtomicU32).cast()),
            consumer_index_offset: offset((&memory.consumer as *const AtomicU32).cast()),
            flags_offset: offset((&memory.flags as *const AtomicU32).cast()),
            size: 8,
            element_stride: size_of::<u64>() as u32,
            reserved: 0,
        };
        let ring = unsafe { Ring::new(info) }.unwrap();
        memory.flags.store(
            XSK_RING_FLAG_ERROR | XSK_RING_FLAG_NEED_POKE,
            Ordering::Release,
        );
        assert!(ring.has_error());
        assert!(ring.needs_poke());
    }

    #[test]
    fn common_mtu_uses_bounded_worker_and_umem_budget() {
        let (max_frame, chunk) = frame_layout(1500).unwrap();
        assert_eq!(max_frame, 1514);
        assert_eq!(chunk, 2048);
        let total =
            queue_umem_size(chunk, true).unwrap() + 63 * queue_umem_size(chunk, false).unwrap();
        assert!(total <= 9 * 1024 * 1024);
        assert_eq!(MAX_EXTERNAL_QUEUES.div_ceil(MAX_QUEUES_PER_WORKER), 2);
    }

    #[test]
    fn automatic_queue_count_is_exact_and_bounded() {
        assert_eq!(automatic_queue_ids_from_count(3).unwrap(), vec![0, 1, 2]);
        assert_eq!(automatic_queue_ids_from_count(0).unwrap(), vec![0]);
        assert!(automatic_queue_ids_from_count(65).is_err());
    }

    #[test]
    fn queue_initialization_is_transactional() {
        struct Tracked(Arc<AtomicUsize>);
        impl Drop for Tracked {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let dropped = Arc::new(AtomicUsize::new(0));
        let result = initialize_queues(&[0, 1, 2], |_, id| {
            if id == 2 {
                bail!("injected queue failure");
            }
            Ok(Tracked(Arc::clone(&dropped)))
        });
        assert!(result.is_err());
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn control_command_requires_exact_schema() {
        let command: StartVm = ::serde_json::from_str(r#"{"type":"StartVm"}"#).unwrap();
        assert_eq!(command.kind, "StartVm");
        assert!(::serde_json::from_str::<StartVm>(r#"{"type":"StartVm","extra":true}"#).is_err());
    }
}
