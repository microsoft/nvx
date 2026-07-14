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
use ::std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use ::std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use ::std::sync::{Arc, Mutex};
use ::std::thread::{self, JoinHandle};
use ::std::time::{Duration, Instant};

use ::anyhow::{Context, Result, bail};
use ::serde::Deserialize;
use ::serde_json::json;

use crate::l2bridge::L2BridgeConfig;
use crate::whp::net::{BackendHealth, FrameBackend, FrameCounters};

const XDP_API_VERSION_2: u32 = 2;
const XDP_CREATE_PROGRAM_GENERIC: u32 = 0x1;
const XDP_CREATE_PROGRAM_ALL_QUEUES: u32 = 0x4;
const XSK_BIND_RX_TX_GENERIC: u32 = 0x1 | 0x2 | 0x4;
const XSK_RING_SIZE: u32 = 128;
const FRAME_SIZE: usize = 4096;
const RX_FRAME_COUNT: usize = XSK_RING_SIZE as usize;
const TX_FRAME_COUNT: usize = XSK_RING_SIZE as usize;
const UMEM_SIZE: usize = FRAME_SIZE * (RX_FRAME_COUNT + TX_FRAME_COUNT);
/// The supported automatic topology is at most 64 queues. Queue ID 64 is an overflow probe.
const MAX_AUTO_QUEUES: u32 = 64;
const TX_BATCH: usize = 64;
const PIPE_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONTROL_MESSAGE: usize = 4096;

type Handle = isize;
type HResult = i32;
type XdpOpenApiFn = unsafe extern "system" fn(u32, *mut *const XdpApiTable) -> HResult;
type XdpCloseApiFn = unsafe extern "system" fn(*const XdpApiTable);
type XdpCreateProgramFn = unsafe extern "system" fn(
    u32,
    *const XdpHookId,
    u32,
    u32,
    *const XdpRule,
    u32,
    *mut Handle,
) -> HResult;
type XskCreateFn = unsafe extern "system" fn(*mut Handle) -> HResult;
type XskBindFn = unsafe extern "system" fn(Handle, u32, u32, u32) -> HResult;
type XskActivateFn = unsafe extern "system" fn(Handle, u32) -> HResult;
type XskNotifySocketFn = unsafe extern "system" fn(Handle, u32, u32, *mut u32) -> HResult;
type XskSetSockoptFn = unsafe extern "system" fn(Handle, u32, *const c_void, u32) -> HResult;
type XskGetSockoptFn = unsafe extern "system" fn(Handle, u32, *mut c_void, *mut u32) -> HResult;
type XskIoctlFn =
    unsafe extern "system" fn(Handle, u32, *const c_void, u32, *mut c_void, *mut u32) -> HResult;

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
    _get_routine: *const c_void,
    create_program: Option<XdpCreateProgramFn>,
    _interface_open: *const c_void,
    xsk_create: Option<XskCreateFn>,
    xsk_bind: Option<XskBindFn>,
    xsk_activate: Option<XskActivateFn>,
    xsk_notify_socket: Option<XskNotifySocketFn>,
    _xsk_notify_async: *const c_void,
    _xsk_get_notify_async_result: *const c_void,
    xsk_set_sockopt: Option<XskSetSockoptFn>,
    xsk_get_sockopt: Option<XskGetSockoptFn>,
    _xsk_ioctl: Option<XskIoctlFn>,
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
            || t.create_program.is_none()
            || t.xsk_create.is_none()
            || t.xsk_bind.is_none()
            || t.xsk_activate.is_none()
            || t.xsk_notify_socket.is_none()
            || t.xsk_set_sockopt.is_none()
            || t.xsk_get_sockopt.is_none()
        {
            bail!("XdpOpenApi(2) returned an incomplete XDP_API_TABLE");
        }
        Ok(())
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
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct Ring {
    producer: *mut AtomicU32,
    consumer: *mut AtomicU32,
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
}

struct Queue {
    _api: Arc<XdpApi>,
    queue_id: u32,
    _rx_program: OwnedHandle,
    socket: OwnedHandle,
    umem: Vec<u8>,
    fill: Ring,
    completion: Ring,
    rx: Ring,
    tx: Ring,
    tx_free: Vec<u64>,
}

impl Queue {
    fn create(api: Arc<XdpApi>, if_index: u32, queue_id: u32) -> Result<Self> {
        let t = api.table();
        let mut raw = 0;
        check(unsafe { t.xsk_create.unwrap()(&mut raw) }, "XskCreate")?;
        let socket = OwnedHandle(raw);
        let mut umem = vec![0; UMEM_SIZE];
        let reg = XskUmemReg {
            total_size: UMEM_SIZE as u64,
            chunk_size: FRAME_SIZE as u32,
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
            unsafe { t.xsk_bind.unwrap()(socket.0, if_index, queue_id, XSK_BIND_RX_TX_GENERIC) },
            "XskBind",
        )?;
        for option in [2, 3, 4, 5] {
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
        let mut queue = Self {
            _api: Arc::clone(&api),
            queue_id,
            _rx_program: OwnedHandle(create_rx_program(&api, if_index, queue_id, socket.0)?),
            socket,
            umem,
            fill: unsafe { Ring::new(info.fill)? },
            completion: unsafe { Ring::new(info.completion)? },
            rx: unsafe { Ring::new(info.rx)? },
            tx: unsafe { Ring::new(info.tx)? },
            tx_free: (RX_FRAME_COUNT..RX_FRAME_COUNT + TX_FRAME_COUNT)
                .map(|n| (n * FRAME_SIZE) as u64)
                .collect(),
        };
        queue.refill_rx()?;
        let mut notify_result = 0;
        check(
            unsafe { t.xsk_notify_socket.unwrap()(queue.socket.0, 0x1, 0, &mut notify_result) },
            "XskNotifySocket POKE_RX",
        )?;
        Ok(queue)
    }

    fn refill_rx(&mut self) -> Result<()> {
        let (start, count) = self.fill.producer_reserve(RX_FRAME_COUNT as u32);
        if count != RX_FRAME_COUNT as u32 {
            bail!("AF_XDP fill ring has insufficient initial capacity");
        }
        for i in 0..count {
            unsafe {
                *self.fill.element::<u64>(start + i) = (i as usize * FRAME_SIZE) as u64;
            }
        }
        self.fill.producer_submit(count);
        Ok(())
    }

    fn reclaim_tx(&mut self, inflight: &AtomicUsize) {
        let (start, count) = self.completion.consumer_reserve(XSK_RING_SIZE);
        for i in 0..count {
            let address = unsafe { *self.completion.element::<u64>(start + i) };
            self.tx_free.push(address & 0x0000_ffff_ffff_ffff);
            inflight.fetch_sub(1, Ordering::AcqRel);
        }
        if count != 0 {
            self.completion.consumer_release(count);
        }
    }

    fn receive(&mut self, out: &SyncSender<Vec<u8>>, counters: &Counters, quiescing: bool) {
        let (start, count) = self.rx.consumer_reserve(XSK_RING_SIZE);
        for i in 0..count {
            let descriptor = unsafe { *self.rx.element::<XskBufferDescriptor>(start + i) };
            let offset = ((descriptor.address_and_offset & 0x0000_ffff_ffff_ffff)
                + (descriptor.address_and_offset >> 48)) as usize;
            let end = offset.saturating_add(descriptor.length as usize);
            if descriptor.length < 14 || end > self.umem.len() || quiescing {
                log::error!(
                    "virt-net: AF_XDP RX queue {} rejected descriptor raw={:#x} offset={} length={} end={} umem={} quiescing={}",
                    self.queue_id,
                    descriptor.address_and_offset,
                    offset,
                    descriptor.length,
                    end,
                    self.umem.len(),
                    quiescing
                );
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
            let (fill, available) = self.fill.producer_reserve(1);
            if available == 1 {
                unsafe {
                    *self.fill.element::<u64>(fill) =
                        descriptor.address_and_offset & 0x0000_ffff_ffff_ffff;
                }
                self.fill.producer_submit(1);
            } else {
                counters.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        if count != 0 {
            self.rx.consumer_release(count);
        }
    }

    fn transmit(&mut self, frame: &[u8]) -> bool {
        if frame.len() < 14 || frame.len() > FRAME_SIZE {
            return false;
        }
        let Some(address) = self.tx_free.pop() else {
            return false;
        };
        let (slot, available) = self.tx.producer_reserve(1);
        if available != 1 {
            self.tx_free.push(address);
            return false;
        }
        self.umem[address as usize..address as usize + frame.len()].copy_from_slice(&frame);
        unsafe {
            *self.tx.element::<XskBufferDescriptor>(slot) = XskBufferDescriptor {
                address_and_offset: address,
                length: frame.len() as u32,
                reserved: 0,
            };
        }
        self.tx.producer_submit(1);
        true
    }
}

struct Counters {
    tx_accepted: AtomicU64,
    tx_dropped: AtomicU64,
    rx_received: AtomicU64,
    rx_dropped: AtomicU64,
    errors: AtomicU64,
}

struct Afxdp {
    _api: Arc<XdpApi>,
    tx_guard: Mutex<Option<OwnedHandle>>,
    senders: Vec<SyncSender<Vec<u8>>>,
    receiver: Mutex<Receiver<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    inflight: Arc<AtomicUsize>,
    counters: Arc<Counters>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl Afxdp {
    fn new(api: Arc<XdpApi>, guard: OwnedHandle, queues: Vec<Queue>) -> Arc<Self> {
        let (out, receiver) = sync_channel((queues.len() * XSK_RING_SIZE as usize).max(1));
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(AtomicU8::new(0));
        let inflight = Arc::new(AtomicUsize::new(0));
        let counters = Arc::new(Counters {
            tx_accepted: AtomicU64::new(0),
            tx_dropped: AtomicU64::new(0),
            rx_received: AtomicU64::new(0),
            rx_dropped: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        });
        let mut senders = Vec::new();
        let mut workers = Vec::new();
        for mut queue in queues {
            let (tx, rx) = sync_channel(XSK_RING_SIZE as usize);
            senders.push(tx);
            let stop = Arc::clone(&stop);
            let state = Arc::clone(&state);
            let inflight = Arc::clone(&inflight);
            let counters = Arc::clone(&counters);
            let api = Arc::clone(&api);
            let out = out.clone();
            workers.push(
                thread::Builder::new()
                    .name("whp-afxdp".into())
                    .spawn(move || {
                        worker(&api, &mut queue, rx, out, stop, state, inflight, counters);
                    })
                    .expect("spawning AF_XDP worker"),
            );
        }
        Arc::new(Self {
            _api: api,
            tx_guard: Mutex::new(Some(guard)),
            senders,
            receiver: Mutex::new(receiver),
            stop,
            state,
            inflight,
            counters,
            workers: Mutex::new(workers),
        })
    }
}

impl FrameBackend for Afxdp {
    fn try_send(&self, frame: Vec<u8>) -> bool {
        if self.state.load(Ordering::Acquire) != 0 || frame.len() < 14 || frame.len() > FRAME_SIZE {
            self.counters.tx_dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // The guest exposes one ordered virtio TX queue. Keep it on the first validated XSK;
        // additional XSKs exist to receive RSS-distributed traffic from every adapter queue.
        let index = 0;
        self.inflight.fetch_add(1, Ordering::AcqRel);
        match self.senders[index].try_send(frame) {
            Ok(()) => {
                self.counters.tx_accepted.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => {
                self.inflight.fetch_sub(1, Ordering::AcqRel);
                self.counters.tx_dropped.fetch_add(1, Ordering::Relaxed);
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
        match self.state.load(Ordering::Acquire) {
            0 => BackendHealth::Ready,
            1 => BackendHealth::Quiescing,
            2 => BackendHealth::Stopped,
            _ => BackendHealth::Failed,
        }
    }
    fn counters(&self) -> FrameCounters {
        FrameCounters {
            guest_tx_accepted: self.counters.tx_accepted.load(Ordering::Relaxed),
            guest_tx_dropped: self.counters.tx_dropped.load(Ordering::Relaxed),
            guest_rx_received: self.counters.rx_received.load(Ordering::Relaxed),
            guest_rx_dropped: self.counters.rx_dropped.load(Ordering::Relaxed),
            backend_errors: self.counters.errors.load(Ordering::Relaxed),
        }
    }
    fn quiesce(&self, timeout: Duration) -> Result<()> {
        if self.state.load(Ordering::Acquire) == 3 {
            bail!("AF_XDP backend is already failed");
        }
        self.state.store(1, Ordering::Release);
        let deadline = Instant::now() + timeout;
        while self.inflight.load(Ordering::Acquire) != 0 {
            if self.state.load(Ordering::Acquire) == 3 {
                bail!("AF_XDP backend failed while draining TX");
            }
            if Instant::now() >= deadline {
                self.state.store(3, Ordering::Release);
                self.shutdown();
                bail!("timed out draining AF_XDP TX completions");
            }
            thread::sleep(Duration::from_millis(1));
        }
        self.shutdown();
        Ok(())
    }
    fn shutdown(&self) {
        let failed = self.state.load(Ordering::Acquire) == 3;
        if !self.stop.swap(true, Ordering::AcqRel) {
            if !failed {
                self.state.store(2, Ordering::Release);
            }
        }
        for worker in self
            .workers
            .lock()
            .expect("AF_XDP workers poisoned")
            .drain(..)
        {
            let _ = worker.join();
        }
        // RX redirect programs and XSKs are dropped by their workers before this all-queue TX
        // guard is removed. The API table/library is retained until the backend drops.
        self.tx_guard.lock().expect("AF_XDP guard poisoned").take();
    }
}

impl Drop for Afxdp {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker(
    api: &XdpApi,
    queue: &mut Queue,
    input: Receiver<Vec<u8>>,
    output: SyncSender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    inflight: Arc<AtomicUsize>,
    counters: Arc<Counters>,
) {
    let mut pending = None;
    while !stop.load(Ordering::Acquire) {
        queue.reclaim_tx(&inflight);
        queue.receive(&output, &counters, state.load(Ordering::Acquire) != 0);

        let mut submitted = 0;
        for _ in 0..TX_BATCH {
            let frame = match pending.take() {
                Some(frame) => frame,
                None => match input.try_recv() {
                    Ok(frame) => frame,
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                },
            };
            if queue.transmit(&frame) {
                submitted += 1;
            } else {
                pending = Some(frame);
                break;
            }
        }

        if submitted != 0 {
            let mut result = 0;
            if failed(unsafe {
                api.table().xsk_notify_socket.unwrap()(queue.socket.0, 0x2, 0, &mut result)
            }) {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                state.store(3, Ordering::Release);
                stop.store(true, Ordering::Release);
            }
            // Drain further immediately available work without paying a receive wait per frame.
            continue;
        }

        let mut result = 0;
        let wait_flag = if pending.is_some() { 0x8 } else { 0x4 };
        let _ = unsafe {
            api.table().xsk_notify_socket.unwrap()(queue.socket.0, wait_flag, 1, &mut result)
        };
    }
}

fn create_rx_program(api: &XdpApi, if_index: u32, queue: u32, socket: Handle) -> Result<Handle> {
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
    let hook = XdpHookId {
        layer: 0,
        direction: 0,
        sublayer: 0,
    };
    let mut program = 0;
    check(
        unsafe {
            api.table().create_program.unwrap()(
                if_index,
                &hook,
                queue,
                0,
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

/// Resources returned only after every selected queue and the TX guard are active.
pub struct BackendStart {
    pub backend: Arc<dyn FrameBackend>,
    pub queues: Vec<u32>,
}

pub fn start(config: &L2BridgeConfig) -> Result<BackendStart> {
    if config.device.mtu as usize + 14 > FRAME_SIZE {
        bail!(
            "device.mtu {} exceeds the AF_XDP {}-byte frame limit",
            config.device.mtu,
            FRAME_SIZE - 14
        );
    }
    let api = XdpApi::load()?;
    let selected = &config.attachment.queue_selection;
    let mut queues = Vec::new();
    let ids: Vec<u32> = if selected.mode == "explicit" {
        selected.queues.clone()
    } else {
        (0..=MAX_AUTO_QUEUES).collect()
    };
    for id in ids {
        match Queue::create(Arc::clone(&api), config.attachment.interface_index, id) {
            Ok(queue) => queues.push((id, queue)),
            Err(_error) if selected.mode == "auto" && id != 0 => break,
            Err(error) => return Err(error).with_context(|| format!("binding AF_XDP queue {id}")),
        }
    }
    if queues.is_empty() {
        bail!("AF_XDP queue 0 did not bind");
    }
    if selected.mode == "auto" && queues.len() > MAX_AUTO_QUEUES as usize {
        bail!("AF_XDP queue discovery found more than {MAX_AUTO_QUEUES} queues");
    }
    let queue_ids = queues.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let guard = create_tx_guard(&api, config.attachment.interface_index)?;
    let backend: Arc<dyn FrameBackend> =
        Afxdp::new(api, guard, queues.into_iter().map(|(_, q)| q).collect());
    Ok(BackendStart {
        backend,
        queues: queue_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn v2_abi_layout_matches_published_headers() {
        assert_eq!(size_of::<XdpApiTable>(), 14 * size_of::<usize>());
        assert_eq!(size_of::<XdpHookId>(), 12);
        assert_eq!(size_of::<XskUmemReg>(), 24);
        assert_eq!(size_of::<XskBufferDescriptor>(), 16);
        assert_eq!(size_of::<XskRingInfo>(), 40);
        assert_eq!(size_of::<XskRingInfoSet>(), 160);
        assert_eq!(size_of::<XdpMatchPattern>(), 40);
        assert_eq!(size_of::<XdpRedirect>(), 16);
        assert_eq!(size_of::<XdpRule>(), 72);
    }
    #[test]
    fn ring_index_wraps_at_power_of_two_size() {
        assert_eq!(17_u32 & (8 - 1), 1);
    }

    #[test]
    fn control_command_requires_exact_schema() {
        let command: StartVm = ::serde_json::from_str(r#"{"type":"StartVm"}"#).unwrap();
        assert_eq!(command.kind, "StartVm");
        assert!(::serde_json::from_str::<StartVm>(r#"{"type":"StartVm","extra":true}"#).is_err());
    }
}
