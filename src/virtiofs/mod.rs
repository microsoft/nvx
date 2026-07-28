//! Live host-directory sharing through a virtio-fs device.
//!
//! This module owns the guest-visible virtio-mmio transport. The FUSE request server and host
//! filesystem adapter live behind [`RequestHandler`], while KVM and WHP only provide DMA access
//! to guest RAM and deliver the interrupt exposed by [`VirtioFs::irq_asserted`].

use ::std::sync::Arc;
use ::std::sync::atomic::{Ordering, fence};

use ::anyhow::{Context, Result, bail};
use ::log::{debug, warn};

mod fuse;

pub use fuse::PassthroughFs;

/// Guest-physical base of the virtio-fs MMIO window, immediately after virt-net's page.
pub const MMIO_BASE: u64 = 0xd000_1000;
/// Size of the virtio-mmio register window.
pub const MMIO_SIZE: u64 = 0x1000;
/// Legacy PIC line used by virtio-fs. IRQ6 is unused by this device model and fits WHP's
/// deliberately minimal master-PIC implementation.
pub const IRQ: u32 = 6;
/// Stable filesystem tag used by the guest mount command.
pub const TAG: &str = "nvx";

const REG_MAGIC: u64 = 0x000;
const REG_VERSION: u64 = 0x004;
const REG_DEVICE_ID: u64 = 0x008;
const REG_VENDOR_ID: u64 = 0x00c;
const REG_DEVICE_FEATURES: u64 = 0x010;
const REG_DEVICE_FEATURES_SEL: u64 = 0x014;
const REG_DRIVER_FEATURES: u64 = 0x020;
const REG_DRIVER_FEATURES_SEL: u64 = 0x024;
const REG_QUEUE_SEL: u64 = 0x030;
const REG_QUEUE_NUM_MAX: u64 = 0x034;
const REG_QUEUE_NUM: u64 = 0x038;
const REG_QUEUE_READY: u64 = 0x044;
const REG_QUEUE_NOTIFY: u64 = 0x050;
const REG_INTERRUPT_STATUS: u64 = 0x060;
const REG_INTERRUPT_ACK: u64 = 0x064;
const REG_STATUS: u64 = 0x070;
const REG_QUEUE_DESC_LOW: u64 = 0x080;
const REG_QUEUE_DESC_HIGH: u64 = 0x084;
const REG_QUEUE_DRIVER_LOW: u64 = 0x090;
const REG_QUEUE_DRIVER_HIGH: u64 = 0x094;
const REG_QUEUE_DEVICE_LOW: u64 = 0x0a0;
const REG_QUEUE_DEVICE_HIGH: u64 = 0x0a4;
const REG_CONFIG_GENERATION: u64 = 0x0fc;
const REG_CONFIG: u64 = 0x100;

const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
const VIRTIO_MMIO_VERSION: u32 = 2;
const VIRTIO_ID_FS: u32 = 26;
const VIRTIO_VENDOR: u32 = 0x4e56_5800;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;
const VIRTQ_DESC_F_INDIRECT: u16 = 4;
const INT_USED_RING: u32 = 1;
const QUEUE_SIZE_MAX: u16 = 256;
const NUM_QUEUES: usize = 2;
const TAG_LEN: usize = 36;
const MAX_REQUEST_BYTES: usize = 4 << 20;

/// Backend-neutral DMA access used by the shared device model.
pub trait DmaMemory: Send + Sync {
    /// Reads a contiguous guest-physical range.
    fn read(&self, gpa: u64, data: &mut [u8]) -> bool;
    /// Writes a contiguous guest-physical range.
    fn write(&self, gpa: u64, data: &[u8]) -> bool;

    fn read_u16(&self, gpa: u64) -> u16 {
        let mut data = [0u8; 2];
        let _ = self.read(gpa, &mut data);
        u16::from_le_bytes(data)
    }

    fn read_u32(&self, gpa: u64) -> u32 {
        let mut data = [0u8; 4];
        let _ = self.read(gpa, &mut data);
        u32::from_le_bytes(data)
    }

    fn read_u64(&self, gpa: u64) -> u64 {
        let mut data = [0u8; 8];
        let _ = self.read(gpa, &mut data);
        u64::from_le_bytes(data)
    }

    fn write_u16(&self, gpa: u64, value: u16) {
        let _ = self.write(gpa, &value.to_le_bytes());
    }

    fn write_u32(&self, gpa: u64, value: u32) {
        let _ = self.write(gpa, &value.to_le_bytes());
    }
}

/// FUSE request server attached to the transport.
pub trait RequestHandler: Send {
    /// Processes one complete FUSE request. `None` represents an operation with no reply.
    fn handle(&mut self, request: &[u8]) -> Option<Vec<u8>>;

    /// Returns the filesystem-side state needed by snapshot/restore.
    fn save(&self) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }

    /// Restores filesystem-side state into a freshly attached host directory.
    fn load(&mut self, state: &[u8]) -> Result<()> {
        if !state.is_empty() {
            bail!("virtio-fs handler does not support snapshot state");
        }
        Ok(())
    }

    /// Drops protocol state after a transport reset.
    fn reset(&mut self) {}
}

#[derive(Clone, Copy, Default)]
struct Queue {
    ready: bool,
    size: u16,
    desc: u64,
    avail: u64,
    used: u64,
    next_avail: u16,
    next_used: u16,
}

impl Queue {
    fn pop_avail(&mut self, mem: &dyn DmaMemory) -> Option<u16> {
        if !self.ready || self.size == 0 {
            return None;
        }
        let avail_idx = mem.read_u16(self.avail.wrapping_add(2));
        fence(Ordering::Acquire);
        if self.next_avail == avail_idx {
            return None;
        }
        let slot = u64::from(self.next_avail % self.size);
        let head = mem.read_u16(self.avail.wrapping_add(4 + 2 * slot));
        self.next_avail = self.next_avail.wrapping_add(1);
        Some(head)
    }

    fn push_used(&mut self, mem: &dyn DmaMemory, id: u32, len: u32) {
        if self.size == 0 {
            return;
        }
        let slot = u64::from(self.next_used % self.size);
        let elem = self.used.wrapping_add(4 + 8 * slot);
        mem.write_u32(elem, id);
        mem.write_u32(elem.wrapping_add(4), len);
        self.next_used = self.next_used.wrapping_add(1);
        fence(Ordering::Release);
        mem.write_u16(self.used.wrapping_add(2), self.next_used);
    }
}

#[derive(Clone, Copy)]
struct WritableDescriptor {
    addr: u64,
    len: usize,
}

/// A live virtio-fs device on a virtio-mmio transport.
pub struct VirtioFs {
    mem: Arc<dyn DmaMemory>,
    handler: Box<dyn RequestHandler>,
    tag: [u8; TAG_LEN],
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    status: u32,
    queue_sel: u32,
    queues: [Queue; NUM_QUEUES],
    interrupt_status: u32,
}

impl VirtioFs {
    /// Creates a device exporting the filesystem named by `tag`.
    pub fn new(
        mem: Arc<dyn DmaMemory>,
        handler: Box<dyn RequestHandler>,
        tag: &str,
    ) -> Result<Self> {
        if tag.is_empty() || tag.len() > TAG_LEN || !tag.is_ascii() {
            bail!("virtio-fs tag must be 1..={TAG_LEN} ASCII bytes");
        }
        let mut config_tag = [0u8; TAG_LEN];
        config_tag[..tag.len()].copy_from_slice(tag.as_bytes());
        Ok(Self {
            mem,
            handler,
            tag: config_tag,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            status: 0,
            queue_sel: 0,
            queues: [Queue::default(); NUM_QUEUES],
            interrupt_status: 0,
        })
    }

    /// Kernel command-line fragment that discovers and mounts this device.
    pub fn cmdline_fragment(target: &str, writable: bool, tag: &str) -> String {
        let mode = if writable { "rw" } else { "ro" };
        format!(
            "virtio_mmio.device={MMIO_SIZE:#x}@{MMIO_BASE:#x}:{IRQ} \
             virtfs_dir={target} virtfs_tag={tag} virtfs_mode={mode}"
        )
    }

    /// Services a read from the virtio-mmio register window.
    pub fn mmio_read(&mut self, offset: u64, data: &mut [u8]) {
        if offset >= REG_CONFIG {
            self.read_config((offset - REG_CONFIG) as usize, data);
            return;
        }

        let value = match offset {
            REG_MAGIC => VIRTIO_MMIO_MAGIC,
            REG_VERSION => VIRTIO_MMIO_VERSION,
            REG_DEVICE_ID => VIRTIO_ID_FS,
            REG_VENDOR_ID => VIRTIO_VENDOR,
            REG_DEVICE_FEATURES => {
                if self.device_features_sel == 1 {
                    (VIRTIO_F_VERSION_1 >> 32) as u32
                } else {
                    VIRTIO_F_VERSION_1 as u32
                }
            }
            REG_QUEUE_NUM_MAX => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |_| u32::from(QUEUE_SIZE_MAX)),
            REG_QUEUE_READY => u32::from(
                self.queues
                    .get(self.queue_sel as usize)
                    .is_some_and(|queue| queue.ready),
            ),
            REG_INTERRUPT_STATUS => self.interrupt_status,
            REG_STATUS => self.status,
            REG_CONFIG_GENERATION => 0,
            _ => 0,
        };
        copy_prefix(data, &value.to_le_bytes());
    }

    /// Services a write to the virtio-mmio register window.
    pub fn mmio_write(&mut self, offset: u64, data: &[u8]) {
        let mut raw = [0u8; 4];
        copy_prefix(&mut raw, data);
        let value = u32::from_le_bytes(raw);
        let selected = self.queue_sel as usize;

        match offset {
            REG_DEVICE_FEATURES_SEL => self.device_features_sel = value,
            REG_DRIVER_FEATURES => {
                if self.driver_features_sel == 1 {
                    self.driver_features =
                        (self.driver_features & 0xffff_ffff) | (u64::from(value) << 32);
                } else {
                    self.driver_features = (self.driver_features & !0xffff_ffff) | u64::from(value);
                }
            }
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            REG_QUEUE_SEL => self.queue_sel = value,
            REG_QUEUE_NUM => {
                if let Some(queue) = self.queues.get_mut(selected) {
                    queue.size = (value as u16).min(QUEUE_SIZE_MAX);
                }
            }
            REG_QUEUE_READY => self.set_queue_ready(selected, value == 1),
            REG_QUEUE_NOTIFY => self.process_queue(value as usize),
            REG_INTERRUPT_ACK => self.interrupt_status &= !value,
            REG_STATUS => {
                self.status = value;
                if value == 0 {
                    self.reset();
                }
            }
            REG_QUEUE_DESC_LOW => {
                self.set_queue_addr(selected, |queue| &mut queue.desc, value, false)
            }
            REG_QUEUE_DESC_HIGH => {
                self.set_queue_addr(selected, |queue| &mut queue.desc, value, true)
            }
            REG_QUEUE_DRIVER_LOW => {
                self.set_queue_addr(selected, |queue| &mut queue.avail, value, false)
            }
            REG_QUEUE_DRIVER_HIGH => {
                self.set_queue_addr(selected, |queue| &mut queue.avail, value, true)
            }
            REG_QUEUE_DEVICE_LOW => {
                self.set_queue_addr(selected, |queue| &mut queue.used, value, false)
            }
            REG_QUEUE_DEVICE_HIGH => {
                self.set_queue_addr(selected, |queue| &mut queue.used, value, true)
            }
            _ => {}
        }
    }

    /// Whether the device has an unacknowledged used-ring interrupt.
    pub fn irq_asserted(&self) -> bool {
        self.interrupt_status != 0
    }

    /// Serializes transport and filesystem protocol state for a VM snapshot.
    pub fn save(&self) -> Result<Vec<u8>> {
        let handler_state = self.handler.save()?;
        let mut state = Vec::new();
        state.extend(self.device_features_sel.to_le_bytes());
        state.extend(self.driver_features_sel.to_le_bytes());
        state.extend(self.driver_features.to_le_bytes());
        state.extend(self.status.to_le_bytes());
        state.extend(self.queue_sel.to_le_bytes());
        state.extend(self.interrupt_status.to_le_bytes());
        for queue in &self.queues {
            state.push(u8::from(queue.ready));
            state.extend(queue.size.to_le_bytes());
            state.extend(queue.desc.to_le_bytes());
            state.extend(queue.avail.to_le_bytes());
            state.extend(queue.used.to_le_bytes());
            state.extend(queue.next_avail.to_le_bytes());
            state.extend(queue.next_used.to_le_bytes());
        }
        state.extend((handler_state.len() as u32).to_le_bytes());
        state.extend(handler_state);
        Ok(state)
    }

    /// Restores transport and filesystem protocol state into a freshly attached device.
    pub fn load(&mut self, state: &[u8]) -> Result<()> {
        let mut cursor = StateCursor::new(state);
        self.device_features_sel = cursor.u32()?;
        self.driver_features_sel = cursor.u32()?;
        self.driver_features = cursor.u64()?;
        self.status = cursor.u32()?;
        self.queue_sel = cursor.u32()?;
        self.interrupt_status = cursor.u32()?;
        for queue in &mut self.queues {
            queue.ready = cursor.u8()? != 0;
            queue.size = cursor.u16()?;
            queue.desc = cursor.u64()?;
            queue.avail = cursor.u64()?;
            queue.used = cursor.u64()?;
            queue.next_avail = cursor.u16()?;
            queue.next_used = cursor.u16()?;
        }
        let handler_len = cursor.u32()? as usize;
        self.handler.load(cursor.take(handler_len)?)?;
        if !cursor.is_empty() {
            bail!("virtio-fs snapshot has trailing bytes");
        }
        Ok(())
    }

    /// Replays pending queue work and interrupts after restore.
    pub fn resume(&mut self) {
        for index in 0..NUM_QUEUES {
            self.process_queue(index);
        }
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let mut config = [0u8; TAG_LEN + 4];
        config[..TAG_LEN].copy_from_slice(&self.tag);
        config[TAG_LEN..].copy_from_slice(&1u32.to_le_bytes());
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = config.get(offset + index).copied().unwrap_or(0);
        }
    }

    fn set_queue_ready(&mut self, selected: usize, ready: bool) {
        if let Some(queue) = self.queues.get_mut(selected) {
            queue.ready = ready;
            if ready {
                queue.next_avail = 0;
                queue.next_used = 0;
                self.mem.write_u16(queue.used, 0);
                self.mem.write_u16(queue.used.wrapping_add(2), 0);
            }
        }
    }

    fn set_queue_addr(
        &mut self,
        selected: usize,
        field: impl FnOnce(&mut Queue) -> &mut u64,
        value: u32,
        high: bool,
    ) {
        if let Some(queue) = self.queues.get_mut(selected) {
            let address = field(queue);
            if high {
                *address = (*address & 0xffff_ffff) | (u64::from(value) << 32);
            } else {
                *address = (*address & !0xffff_ffff) | u64::from(value);
            }
        }
    }

    fn process_queue(&mut self, index: usize) {
        if index >= NUM_QUEUES || !self.queues[index].ready {
            return;
        }
        let mem = Arc::clone(&self.mem);
        let mut completed = false;
        while let Some(head) = self.queues[index].pop_avail(mem.as_ref()) {
            let (request, writable) = match self.read_chain(index, head) {
                Ok(chain) => chain,
                Err(error) => {
                    warn!("virtio-fs: invalid descriptor chain: {error:#}");
                    self.queues[index].push_used(mem.as_ref(), u32::from(head), 0);
                    completed = true;
                    continue;
                }
            };
            let response = self.handler.handle(&request);
            let written = response
                .as_deref()
                .map(|bytes| scatter_write(mem.as_ref(), &writable, bytes))
                .unwrap_or(0);
            if response.as_ref().is_some_and(|bytes| written < bytes.len()) {
                warn!(
                    "virtio-fs: response truncated by guest buffers ({} < {})",
                    written,
                    response.as_ref().map_or(0, Vec::len)
                );
            }
            self.queues[index].push_used(mem.as_ref(), u32::from(head), written as u32);
            completed = true;
        }
        if completed {
            self.interrupt_status |= INT_USED_RING;
        }
    }

    fn read_chain(
        &self,
        queue_index: usize,
        head: u16,
    ) -> Result<(Vec<u8>, Vec<WritableDescriptor>)> {
        let queue = self.queues[queue_index];
        let mut request = Vec::new();
        let mut writable = Vec::new();
        let mut index = head;
        let mut hops = 0u16;
        let mut saw_writable = false;
        loop {
            if index >= queue.size || hops >= queue.size {
                bail!("descriptor chain is cyclic or out of range");
            }
            let (addr, len, flags, next) = read_descriptor(self.mem.as_ref(), queue.desc, index);
            if flags & VIRTQ_DESC_F_INDIRECT != 0 {
                bail!("indirect descriptors were not negotiated");
            }
            if flags & VIRTQ_DESC_F_WRITE != 0 {
                saw_writable = true;
                writable.push(WritableDescriptor {
                    addr,
                    len: len as usize,
                });
            } else {
                if saw_writable {
                    bail!("readable descriptor follows writable response buffer");
                }
                let old_len = request.len();
                let new_len = old_len
                    .checked_add(len as usize)
                    .context("request length overflow")?;
                if new_len > MAX_REQUEST_BYTES {
                    bail!("request exceeds {MAX_REQUEST_BYTES} bytes");
                }
                request.resize(new_len, 0);
                if !self.mem.read(addr, &mut request[old_len..]) {
                    bail!("request descriptor points outside guest RAM");
                }
            }
            hops += 1;
            if flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            index = next;
        }
        if request.is_empty() {
            bail!("request chain has no readable input");
        }
        Ok((request, writable))
    }

    fn reset(&mut self) {
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.status = 0;
        self.queue_sel = 0;
        self.queues = [Queue::default(); NUM_QUEUES];
        self.interrupt_status = 0;
        self.handler.reset();
        debug!("virtio-fs: transport reset");
    }
}

fn read_descriptor(mem: &dyn DmaMemory, table: u64, index: u16) -> (u64, u32, u16, u16) {
    let address = table.wrapping_add(u64::from(index) * 16);
    (
        mem.read_u64(address),
        mem.read_u32(address.wrapping_add(8)),
        mem.read_u16(address.wrapping_add(12)),
        mem.read_u16(address.wrapping_add(14)),
    )
}

fn scatter_write(mem: &dyn DmaMemory, descriptors: &[WritableDescriptor], data: &[u8]) -> usize {
    let mut written = 0usize;
    for descriptor in descriptors {
        let count = descriptor.len.min(data.len().saturating_sub(written));
        if count == 0 || !mem.write(descriptor.addr, &data[written..written + count]) {
            break;
        }
        written += count;
        if written == data.len() {
            break;
        }
    }
    written
}

fn copy_prefix(destination: &mut [u8], source: &[u8]) {
    let count = destination.len().min(source.len());
    destination[..count].copy_from_slice(&source[..count]);
}

struct StateCursor<'a> {
    state: &'a [u8],
    offset: usize,
}

impl<'a> StateCursor<'a> {
    fn new(state: &'a [u8]) -> Self {
        Self { state, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .context("virtio-fs snapshot overflow")?;
        let bytes = self
            .state
            .get(self.offset..end)
            .context("virtio-fs snapshot truncated")?;
        self.offset = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(
            bytes.try_into().expect("four-byte slice"),
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(
            bytes.try_into().expect("eight-byte slice"),
        ))
    }

    fn is_empty(&self) -> bool {
        self.offset == self.state.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::std::sync::Mutex;
    use ::std::sync::atomic::AtomicBool;

    struct TestMemory(Mutex<Vec<u8>>);

    impl TestMemory {
        fn new(size: usize) -> Self {
            Self(Mutex::new(vec![0; size]))
        }
    }

    impl DmaMemory for TestMemory {
        fn read(&self, gpa: u64, data: &mut [u8]) -> bool {
            let memory = self.0.lock().expect("test memory poisoned");
            let start = gpa as usize;
            let Some(source) = memory.get(start..start.saturating_add(data.len())) else {
                return false;
            };
            data.copy_from_slice(source);
            true
        }

        fn write(&self, gpa: u64, data: &[u8]) -> bool {
            let mut memory = self.0.lock().expect("test memory poisoned");
            let start = gpa as usize;
            let Some(destination) = memory.get_mut(start..start.saturating_add(data.len())) else {
                return false;
            };
            destination.copy_from_slice(data);
            true
        }
    }

    struct ReverseHandler;

    impl RequestHandler for ReverseHandler {
        fn handle(&mut self, request: &[u8]) -> Option<Vec<u8>> {
            Some(request.iter().rev().copied().collect())
        }
    }

    struct NoReplyHandler(Arc<AtomicBool>);

    impl RequestHandler for NoReplyHandler {
        fn handle(&mut self, _request: &[u8]) -> Option<Vec<u8>> {
            self.0.store(true, Ordering::SeqCst);
            None
        }
    }

    #[test]
    fn command_line_describes_live_device() {
        assert_eq!(
            VirtioFs::cmdline_fragment("/mnt/host", true, "nvx"),
            "virtio_mmio.device=0x1000@0xd0001000:6 virtfs_dir=/mnt/host \
             virtfs_tag=nvx virtfs_mode=rw"
        );
    }

    #[test]
    fn request_crosses_split_queue() {
        let memory = Arc::new(TestMemory::new(0x8000));
        let dma: Arc<dyn DmaMemory> = memory.clone();
        let mut device = VirtioFs::new(dma, Box::new(ReverseHandler), "nvx").unwrap();

        let descriptor_table = 0x1000u64;
        let available_ring = 0x2000u64;
        let used_ring = 0x3000u64;
        let request = 0x4000u64;
        let response = 0x5000u64;
        assert!(memory.write(request, b"live"));

        assert!(memory.write(descriptor_table, &request.to_le_bytes()));
        assert!(memory.write(descriptor_table + 8, &4u32.to_le_bytes()));
        assert!(memory.write(descriptor_table + 12, &VIRTQ_DESC_F_NEXT.to_le_bytes()));
        assert!(memory.write(descriptor_table + 14, &1u16.to_le_bytes()));
        assert!(memory.write(descriptor_table + 16, &response.to_le_bytes()));
        assert!(memory.write(descriptor_table + 24, &4u32.to_le_bytes()));
        assert!(memory.write(descriptor_table + 28, &VIRTQ_DESC_F_WRITE.to_le_bytes()));
        memory.write_u16(available_ring + 2, 1);
        memory.write_u16(available_ring + 4, 0);

        device.queue_sel = 1;
        device.queues[1] = Queue {
            ready: true,
            size: 8,
            desc: descriptor_table,
            avail: available_ring,
            used: used_ring,
            ..Queue::default()
        };
        device.mmio_write(REG_QUEUE_NOTIFY, &1u32.to_le_bytes());

        let mut output = [0u8; 4];
        assert!(memory.read(response, &mut output));
        assert_eq!(&output, b"evil");
        assert_eq!(memory.read_u16(used_ring + 2), 1);
        assert_eq!(memory.read_u32(used_ring + 8), 4);
        assert!(device.irq_asserted());
    }

    #[test]
    fn no_reply_request_needs_no_writable_descriptor() {
        let memory = Arc::new(TestMemory::new(0x6000));
        let dma: Arc<dyn DmaMemory> = memory.clone();
        let handled = Arc::new(AtomicBool::new(false));
        let mut device =
            VirtioFs::new(dma, Box::new(NoReplyHandler(Arc::clone(&handled))), "nvx").unwrap();

        let descriptor_table = 0x1000u64;
        let available_ring = 0x2000u64;
        let used_ring = 0x3000u64;
        let request = 0x4000u64;
        assert!(memory.write(request, b"forget"));
        assert!(memory.write(descriptor_table, &request.to_le_bytes()));
        assert!(memory.write(descriptor_table + 8, &6u32.to_le_bytes()));
        memory.write_u16(available_ring + 2, 1);
        memory.write_u16(available_ring + 4, 0);
        device.queues[1] = Queue {
            ready: true,
            size: 8,
            desc: descriptor_table,
            avail: available_ring,
            used: used_ring,
            ..Queue::default()
        };

        device.mmio_write(REG_QUEUE_NOTIFY, &1u32.to_le_bytes());

        assert!(handled.load(Ordering::SeqCst));
        assert_eq!(memory.read_u16(used_ring + 2), 1);
        assert_eq!(memory.read_u32(used_ring + 8), 0);
    }
}

#[cfg(target_os = "linux")]
impl DmaMemory for crate::memory::GuestRam {
    fn read(&self, gpa: u64, data: &mut [u8]) -> bool {
        crate::memory::GuestRam::read(self, gpa, data)
    }

    fn write(&self, gpa: u64, data: &[u8]) -> bool {
        crate::memory::GuestRam::write(self, gpa, data)
    }
}
