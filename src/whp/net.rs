// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The idea of exposing host networking to the guest -- here a point-to-point link the guest
// kernel drives itself -- mirrors the Nanvix Micro-VM (uservm) `-allow-host-networking` option,
// Copyright(c) The Maintainers of Nanvix, MIT License. The device model follows the KVM backend's
// `src/net.rs`; the host side is the user-mode NAT in `whp/slirp.rs` rather than a TAP.

//!
//! # Virtual Network (virt-net, WHP)
//!
//! Gives the guest a real NIC that the stock Linux kernel drives: a **virtio-net** function on a
//! **virtio-mmio** (version 2, VIRTIO 1.0) transport. It is identical in guest-visible behaviour
//! to the KVM backend's virt-net, but two things differ because WHP has no in-hypervisor device
//! model:
//!
//! - MMIO to the device window is serviced through WHP's instruction emulator (see
//!   [`crate::whp::emulator`]) instead of a KVM MMIO exit.
//! - The NIC interrupt is delivered as a **master-PIC line (IRQ5)** injected as a local-APIC
//!   vector (the same mechanism as the PIT/IRQ0 timer), rather than through an `irqfd`. A master
//!   line avoids the slave-PIC cascade entirely.
//!
//! The host side is a user-mode NAT ([`crate::whp::slirp`]): the guest is given a static address
//! from the kernel command line and the VMM plays the gateway, so no TAP driver or admin rights
//! are needed.
//!

use ::std::net::Ipv4Addr;
use ::std::sync::Arc;
use ::std::sync::atomic::{Ordering, fence};

use ::anyhow::{Context, Result, bail};

use crate::l2bridge::ExternalIdentity;
use crate::whp::memory::GuestMemory;

/// Guest-physical base of the virtio-mmio device window. It lives in the MMIO gap
/// (`0xC000_0000..4 GiB`), which is never reported to the guest as RAM, so accesses there fault
/// out to the VMM (a `WHvRunVpExitReasonMemoryAccess`) instead of hitting mapped RAM.
pub const NET_MMIO_BASE: u64 = 0xd000_0000;
/// Size of the virtio-mmio device window (one page covers the registers + config space).
pub const NET_MMIO_SIZE: u64 = 0x1000;
/// Master-PIC IRQ line the NIC raises. A line in `0..=7` is delivered through the emulated 8259
/// exactly like the timer's IRQ0, avoiding the slave-PIC cascade.
pub const NET_IRQ: u32 = 5;

// virtio-mmio register offsets (VIRTIO 1.x, MMIO transport, version 2).
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
/// Start of the device-specific configuration space (for virtio-net: the 6-byte MAC).
const REG_CONFIG: u64 = 0x100;

const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976; // "virt"
const VIRTIO_MMIO_VERSION: u32 = 2;
const VIRTIO_ID_NET: u32 = 1;
const VIRTIO_VENDOR: u32 = 0x4c4c_5800; // "LLX\0"

// virtio feature bits.
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// virtqueue descriptor flags.
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// virtio-mmio interrupt-status bits.
const INT_USED_RING: u32 = 1;

/// Length of the `virtio_net_hdr_v1` prepended to every frame (the 12-byte, `num_buffers`-carrying
/// header the Linux driver always uses once `VIRTIO_F_VERSION_1` is negotiated).
const NET_HDR_LEN: usize = 12;

/// Maximum virtqueue size the device advertises.
const QUEUE_SIZE_MAX: u16 = 256;

/// Number of virtqueues (receive + transmit).
const NUM_QUEUES: usize = 2;
/// Receive virtqueue index (device writes guest buffers).
const RX_QUEUE: usize = 0;
/// Transmit virtqueue index (device reads guest buffers).
const TX_QUEUE: usize = 1;
const EXTERNAL_SNAPSHOT_MAGIC: &[u8; 8] = b"NXL2NET1";

/// Snapshot of backend-side packet accounting. Counters make bounded-queue backpressure visible
/// rather than silently growing memory while the guest is faster than the external data plane.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameCounters {
    pub guest_tx_accepted: u64,
    pub guest_tx_dropped: u64,
    pub guest_rx_received: u64,
    pub guest_rx_dropped: u64,
    pub backend_errors: u64,
    pub driver_rx_dropped: u64,
    pub driver_rx_truncated: u64,
    pub driver_rx_invalid_descriptors: u64,
    pub driver_tx_invalid_descriptors: u64,
}

/// Data-plane state exposed to the device and lifecycle code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendHealth {
    Ready,
    Quiescing,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameSend {
    Accepted,
    Backpressure,
    Dropped,
}

/// Raw-Ethernet frame boundary between virtio-net and its host data plane.
///
/// Implementations must be bounded and nonblocking for [`try_send`](Self::try_send): it is called
/// while the vCPU holds the `VirtioNet` mutex. The receive side is owned by the pump thread.
pub trait FrameBackend: Send + Sync {
    fn try_send(&self, frame: Vec<u8>) -> FrameSend;
    fn recv_timeout(&self, timeout: ::std::time::Duration) -> Option<Vec<u8>>;
    fn health(&self) -> BackendHealth;
    fn check_health(&self) -> Result<()>;
    fn counters(&self) -> FrameCounters;
    fn quiesce(&self, timeout: ::std::time::Duration) -> Result<()>;
    fn shutdown(&self);
}

/// The guest and host endpoints of the virt-net link, parsed from `--net`.
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// Guest IPv4 address.
    pub guest_ip: Ipv4Addr,
    /// Subnet prefix length (bits).
    pub prefix: u8,
    /// Subnet mask in dotted form (handed to the guest for `ifconfig`).
    pub netmask: Ipv4Addr,
    /// Host (gateway) IPv4 address: the first address of the guest's subnet.
    pub host_ip: Ipv4Addr,
    /// Guest MAC address (locally administered, derived from the guest IP).
    pub mac: [u8; 6],
    /// Host (gateway) MAC the NAT answers ARP with; fixed so the guest's ARP cache stays valid
    /// across snapshot/restore.
    pub host_mac: [u8; 6],
}

impl NetConfig {
    ///
    /// # Description
    ///
    /// Parses a `--net` value of the form `<ipv4>/<prefix>` (e.g. `10.0.0.2/24`) into the guest
    /// and host endpoints of a point-to-point link. The host/gateway address is the first usable
    /// address of the guest's subnet (network + 1).
    ///
    pub fn parse(spec: &str) -> Result<Self> {
        let (ip_str, prefix_str) = spec
            .split_once('/')
            .with_context(|| format!("--net '{spec}' must be <ipv4>/<prefix>, e.g. 10.0.0.2/24"))?;
        let guest_ip: Ipv4Addr = ip_str
            .parse()
            .with_context(|| format!("--net: invalid IPv4 address '{ip_str}'"))?;
        let prefix: u8 = prefix_str
            .parse()
            .with_context(|| format!("--net: invalid prefix '{prefix_str}'"))?;
        Self::build(guest_ip, prefix)
    }

    /// Serializes only the guest-visible external NIC identity. Interface indices, selected queues,
    /// XSK/UMEM state and all XDP handles are deliberately excluded.
    pub fn save_external_header(identity: &ExternalIdentity) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + 6 + 4 + 32 + 4 + identity.bootstrap.len());
        bytes.extend_from_slice(EXTERNAL_SNAPSHOT_MAGIC);
        bytes.extend_from_slice(&identity.mac);
        bytes.extend_from_slice(&identity.mtu.to_le_bytes());
        bytes.extend_from_slice(&identity.bootstrap_digest);
        bytes.extend_from_slice(&(identity.bootstrap.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&identity.bootstrap);
        bytes
    }

    /// Reads an external snapshot header and returns the identity plus the byte count it consumed.
    pub fn load_external_header(data: &[u8]) -> Result<(ExternalIdentity, usize)> {
        const FIXED: usize = 8 + 6 + 4 + 32 + 4;
        if data.len() < FIXED || &data[..8] != EXTERNAL_SNAPSHOT_MAGIC {
            bail!("invalid external L2Bridge snapshot header");
        }
        let mut mac = [0; 6];
        mac.copy_from_slice(&data[8..14]);
        let mtu = u32::from_le_bytes(data[14..18].try_into().unwrap());
        let mut digest = [0; 32];
        digest.copy_from_slice(&data[18..50]);
        let bootstrap_len = u32::from_le_bytes(data[50..54].try_into().unwrap()) as usize;
        let end = FIXED
            .checked_add(bootstrap_len)
            .context("external snapshot header overflow")?;
        let bootstrap = data
            .get(FIXED..end)
            .context("external snapshot bootstrap truncated")?
            .to_vec();
        Ok((
            ExternalIdentity {
                mac,
                mtu,
                bootstrap,
                bootstrap_digest: digest,
            },
            end,
        ))
    }

    /// Whether this net-state blob belongs to the external L2Bridge backend.
    pub fn is_external_snapshot(data: &[u8]) -> bool {
        data.starts_with(EXTERNAL_SNAPSHOT_MAGIC)
    }

    /// Derives the full endpoint configuration (gateway, netmask, MACs) from a guest address and
    /// prefix. Shared by [`parse`](Self::parse) and [`from_header`](Self::from_header).
    fn build(guest_ip: Ipv4Addr, prefix: u8) -> Result<Self> {
        if !(1..=30).contains(&prefix) {
            bail!("--net: prefix /{prefix} out of range (use 1..=30)");
        }
        let mask: u32 = u32::MAX << (32 - prefix);
        let network: u32 = u32::from(guest_ip) & mask;
        let host_ip: Ipv4Addr = Ipv4Addr::from(network + 1);
        if host_ip == guest_ip {
            bail!(
                "--net: guest IP {guest_ip} collides with the derived host/gateway {host_ip}; \
                 pick a different guest address (e.g. the .2 of the subnet)"
            );
        }
        // Locally administered MACs 52:54:00 + the low 3 octets of each IP.
        let g: [u8; 4] = guest_ip.octets();
        let h: [u8; 4] = host_ip.octets();
        let mac: [u8; 6] = [0x52, 0x54, 0x00, g[1], g[2], g[3]];
        let host_mac: [u8; 6] = [0x52, 0x54, 0x00, h[1], h[2], h[3]];
        Ok(Self {
            guest_ip,
            prefix,
            netmask: Ipv4Addr::from(mask),
            host_ip,
            mac,
            host_mac,
        })
    }

    /// Builds the kernel command-line fragment that points the guest at the NIC and tells its
    /// `init` how to address the link.
    pub fn cmdline_fragment(&self) -> String {
        format!(
            "virtio_mmio.device={:#x}@{:#x}:{} \
             virtnet_ip={} virtnet_mask={} virtnet_gw={}",
            NET_MMIO_SIZE, NET_MMIO_BASE, NET_IRQ, self.guest_ip, self.netmask, self.host_ip
        )
    }

    /// Serializes the guest endpoint (address + prefix) into a snapshot so a restore can rebuild
    /// the identical link without `--net` being given again.
    pub fn save_header(&self) -> Vec<u8> {
        let mut b: Vec<u8> = Vec::with_capacity(5);
        b.extend(self.guest_ip.octets());
        b.push(self.prefix);
        b
    }

    /// Reconstructs the endpoints from a header written by [`save_header`](Self::save_header),
    /// returning the config and the number of header bytes consumed.
    pub fn from_header(data: &[u8]) -> Result<(Self, usize)> {
        if data.len() < 5 {
            bail!("virt-net snapshot header truncated");
        }
        let guest_ip: Ipv4Addr = Ipv4Addr::new(data[0], data[1], data[2], data[3]);
        Ok((Self::build(guest_ip, data[4])?, 5))
    }
}

/// A little-endian byte cursor for parsing serialized virt-net device state.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end: usize = self
            .pos
            .checked_add(n)
            .context("virt-net snapshot overflow")?;
        let s: &[u8] = self
            .data
            .get(self.pos..end)
            .context("virt-net snapshot truncated")?;
        self.pos = end;
        Ok(s)
    }

    fn u16(&mut self) -> Result<u16> {
        let b: &[u8] = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b: &[u8] = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let b: &[u8] = self.take(8)?;
        let mut a: [u8; 8] = [0; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
}

/// One split virtqueue's device-visible state.
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
    fn peek_avail(&self, mem: &GuestMemory) -> Option<u16> {
        if !self.ready || self.size == 0 {
            return None;
        }
        let avail_idx: u16 = mem.read_u16(self.avail.wrapping_add(2));
        fence(Ordering::Acquire);
        if self.next_avail == avail_idx {
            return None;
        }
        let slot: u64 = u64::from(self.next_avail % self.size);
        Some(mem.read_u16(self.avail.wrapping_add(4 + 2 * slot)))
    }

    fn consume_avail(&mut self) {
        self.next_avail = self.next_avail.wrapping_add(1);
    }

    /// Pops the head descriptor index of the next available chain, or `None` if none is pending.
    fn pop_avail(&mut self, mem: &GuestMemory) -> Option<u16> {
        let head = self.peek_avail(mem)?;
        self.consume_avail();
        Some(head)
    }

    /// Publishes `id` (a chain head) on the used ring with `len` bytes written by the device.
    fn push_used(&mut self, mem: &GuestMemory, id: u32, len: u32) {
        if self.size == 0 {
            return;
        }
        let slot: u64 = u64::from(self.next_used % self.size);
        let elem: u64 = self.used.wrapping_add(4 + 8 * slot);
        mem.write_u32(elem, id);
        mem.write_u32(elem.wrapping_add(4), len);
        self.next_used = self.next_used.wrapping_add(1);
        fence(Ordering::Release);
        mem.write_u16(self.used.wrapping_add(2), self.next_used);
    }
}

/// Reads a 16-byte virtqueue descriptor: `(addr, len, flags, next)`.
fn read_desc(mem: &GuestMemory, table: u64, idx: u16) -> (u64, u32, u16, u16) {
    let base: u64 = table.wrapping_add(u64::from(idx) * 16);
    (
        mem.read_u64(base),
        mem.read_u32(base.wrapping_add(8)),
        mem.read_u16(base.wrapping_add(12)),
        mem.read_u16(base.wrapping_add(14)),
    )
}

/// The virtio-net-mmio device: MMIO register state plus the two virtqueues and a raw-frame backend.
pub struct VirtioNet {
    /// Guest RAM accessor for virtqueue DMA (shared with the RX pump thread).
    mem: Arc<GuestMemory>,
    /// Host data plane the guest's Ethernet frames are sent to and received from.
    backend: Arc<dyn FrameBackend>,
    /// Guest MAC address (exposed through config space).
    mac: [u8; 6],
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    status: u32,
    queue_sel: u32,
    queues: [Queue; NUM_QUEUES],
    interrupt_status: u32,
}

impl VirtioNet {
    /// Creates the device backed by `backend`, presenting `mac` to the guest.
    pub fn new(mem: Arc<GuestMemory>, backend: Arc<dyn FrameBackend>, mac: [u8; 6]) -> Self {
        Self {
            mem,
            backend,
            mac,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            status: 0,
            queue_sel: 0,
            queues: [Queue::default(); NUM_QUEUES],
            interrupt_status: 0,
        }
    }

    /// The 64-bit feature set the device offers.
    fn device_features() -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC
    }

    /// Services a guest MMIO read at `offset` within the device window, filling `data`.
    pub fn mmio_read(&mut self, offset: u64, data: &mut [u8]) {
        if offset >= REG_CONFIG {
            let base: usize = (offset - REG_CONFIG) as usize;
            for (i, b) in data.iter_mut().enumerate() {
                *b = self.mac.get(base + i).copied().unwrap_or(0);
            }
            return;
        }
        let val: u32 = match offset {
            REG_MAGIC => VIRTIO_MMIO_MAGIC,
            REG_VERSION => VIRTIO_MMIO_VERSION,
            REG_DEVICE_ID => VIRTIO_ID_NET,
            REG_VENDOR_ID => VIRTIO_VENDOR,
            REG_DEVICE_FEATURES => {
                let f: u64 = Self::device_features();
                if self.device_features_sel == 1 {
                    (f >> 32) as u32
                } else {
                    f as u32
                }
            }
            REG_QUEUE_NUM_MAX => u32::from(QUEUE_SIZE_MAX),
            REG_QUEUE_READY => u32::from(
                self.queues
                    .get(self.queue_sel as usize)
                    .is_some_and(|q| q.ready),
            ),
            REG_INTERRUPT_STATUS => self.interrupt_status,
            REG_STATUS => self.status,
            REG_CONFIG_GENERATION => 0,
            _ => 0,
        };
        let bytes: [u8; 4] = val.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            *b = bytes.get(i).copied().unwrap_or(0);
        }
    }

    /// Services a guest MMIO write at `offset` within the device window.
    pub fn mmio_write(&mut self, offset: u64, data: &[u8]) {
        let mut raw: [u8; 4] = [0; 4];
        for (i, b) in data.iter().take(4).enumerate() {
            raw[i] = *b;
        }
        let val: u32 = u32::from_le_bytes(raw);
        let sel: usize = self.queue_sel as usize;

        match offset {
            REG_DEVICE_FEATURES_SEL => self.device_features_sel = val,
            REG_DRIVER_FEATURES => {
                if self.driver_features_sel == 1 {
                    self.driver_features =
                        (self.driver_features & 0xffff_ffff) | (u64::from(val) << 32);
                } else {
                    self.driver_features = (self.driver_features & !0xffff_ffff) | u64::from(val);
                }
            }
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = val,
            REG_QUEUE_SEL => self.queue_sel = val,
            REG_QUEUE_NUM => {
                if let Some(q) = self.queues.get_mut(sel) {
                    q.size = val as u16;
                }
            }
            REG_QUEUE_READY => {
                if let Some(q) = self.queues.get_mut(sel) {
                    q.ready = val == 1;
                    if q.ready {
                        q.next_avail = 0;
                        q.next_used = 0;
                        let used: u64 = q.used;
                        self.mem.write_u16(used, 0);
                        self.mem.write_u16(used.wrapping_add(2), 0);
                    }
                }
            }
            REG_QUEUE_NOTIFY if val as usize == TX_QUEUE => self.process_tx(),
            REG_INTERRUPT_ACK => self.interrupt_status &= !val,
            REG_STATUS => {
                self.status = val;
                if val == 0 {
                    self.reset();
                }
            }
            REG_QUEUE_DESC_LOW => self.set_queue_addr(sel, |q| &mut q.desc, val, false),
            REG_QUEUE_DESC_HIGH => self.set_queue_addr(sel, |q| &mut q.desc, val, true),
            REG_QUEUE_DRIVER_LOW => self.set_queue_addr(sel, |q| &mut q.avail, val, false),
            REG_QUEUE_DRIVER_HIGH => self.set_queue_addr(sel, |q| &mut q.avail, val, true),
            REG_QUEUE_DEVICE_LOW => self.set_queue_addr(sel, |q| &mut q.used, val, false),
            REG_QUEUE_DEVICE_HIGH => self.set_queue_addr(sel, |q| &mut q.used, val, true),
            _ => {}
        }
    }

    /// Sets the low or high half of a 64-bit queue address selected by `field`.
    fn set_queue_addr(
        &mut self,
        sel: usize,
        field: impl FnOnce(&mut Queue) -> &mut u64,
        val: u32,
        high: bool,
    ) {
        if let Some(q) = self.queues.get_mut(sel) {
            let addr: &mut u64 = field(q);
            if high {
                *addr = (*addr & 0xffff_ffff) | (u64::from(val) << 32);
            } else {
                *addr = (*addr & !0xffff_ffff) | u64::from(val);
            }
        }
    }

    /// Resets the device to its post-power-on state (driver wrote Status = 0).
    fn reset(&mut self) {
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.queues = [Queue::default(); NUM_QUEUES];
        self.interrupt_status = 0;
        self.status = 0;
    }

    /// Drains the transmit queue, handing each guest Ethernet frame to the bounded backend.
    pub fn process_tx(&mut self) {
        if !self.queues[TX_QUEUE].ready {
            return;
        }
        let mem: Arc<GuestMemory> = Arc::clone(&self.mem);
        let mut raised: bool = false;
        while let Some(head) = self.queues[TX_QUEUE].peek_avail(&mem) {
            let table: u64 = self.queues[TX_QUEUE].desc;
            let size: u16 = self.queues[TX_QUEUE].size;

            let mut frame: Vec<u8> = Vec::new();
            let mut idx: u16 = head;
            let mut hops: u16 = 0;
            loop {
                let (addr, len, flags, next) = read_desc(&mem, table, idx);
                if flags & VIRTQ_DESC_F_WRITE == 0 && len > 0 {
                    let start: usize = frame.len();
                    frame.resize(start + len as usize, 0);
                    let _ = mem.read_slice(addr, &mut frame[start..]);
                }
                hops += 1;
                if flags & VIRTQ_DESC_F_NEXT != 0 && hops <= size {
                    idx = next;
                } else {
                    break;
                }
            }

            // Strip the virtio_net_hdr and hand the raw Ethernet frame to the backend. A full
            // bounded queue completes the descriptor and is observable through backend counters;
            // it must never stall a vCPU while this device mutex is held.
            let outcome = if frame.len() > NET_HDR_LEN {
                self.backend.try_send(frame[NET_HDR_LEN..].to_vec())
            } else {
                FrameSend::Dropped
            };
            if outcome == FrameSend::Backpressure {
                break;
            }
            self.queues[TX_QUEUE].consume_avail();
            self.queues[TX_QUEUE].push_used(&mem, u32::from(head), 0);
            raised = true;
        }
        if raised {
            self.raise_irq();
        }
    }

    ///
    /// # Description
    ///
    /// Delivers one received Ethernet `frame` to the guest: prepends a zeroed 12-byte
    /// `virtio_net_hdr` and scatters it into the next receive-queue buffer chain. Returns `false`
    /// if the queue is not ready or has no free buffer (the frame is then dropped).
    ///
    pub fn process_rx(&mut self, frame: &[u8]) -> bool {
        let mem: Arc<GuestMemory> = Arc::clone(&self.mem);
        let head: u16 = match self.queues[RX_QUEUE].pop_avail(&mem) {
            Some(h) => h,
            None => return false,
        };
        let table: u64 = self.queues[RX_QUEUE].desc;
        let size: u16 = self.queues[RX_QUEUE].size;

        let mut payload: Vec<u8> = Vec::with_capacity(NET_HDR_LEN + frame.len());
        payload.extend_from_slice(&[0u8; NET_HDR_LEN]);
        payload[10] = 1; // virtio_net_hdr_v1.num_buffers = 1
        payload.extend_from_slice(frame);

        let mut written: usize = 0;
        let mut idx: u16 = head;
        let mut hops: u16 = 0;
        loop {
            let (addr, len, flags, next) = read_desc(&mem, table, idx);
            if flags & VIRTQ_DESC_F_WRITE != 0 && written < payload.len() {
                let take: usize = (len as usize).min(payload.len() - written);
                if take > 0 {
                    if let Err(e) = mem.write_slice(addr, &payload[written..written + take]) {
                        log::error!(
                            "virt-net: RX descriptor write failed at {addr:#x} for {take} bytes: {e:#}"
                        );
                        break;
                    }
                    written += take;
                }
            }
            hops += 1;
            if written >= payload.len() {
                break;
            }
            if flags & VIRTQ_DESC_F_NEXT != 0 && hops <= size {
                idx = next;
            } else {
                break;
            }
        }

        if written < payload.len() {
            log::warn!(
                "virt-net: RX buffer too small ({written} < {}); frame truncated",
                payload.len()
            );
        }
        self.queues[RX_QUEUE].push_used(&mem, u32::from(head), written as u32);
        self.raise_irq();
        true
    }

    /// Flags the used-ring interrupt. The vCPU loop re-injects the NIC's IRQ each iteration while
    /// this remains set (level-triggered), so a masked guest never loses the notification; the
    /// guest clears it by writing `InterruptACK`.
    fn raise_irq(&mut self) {
        self.interrupt_status |= INT_USED_RING;
    }

    /// Whether the device currently has an unacknowledged interrupt asserted.
    pub fn irq_asserted(&self) -> bool {
        self.interrupt_status != 0
    }

    /// Serializes the device's transport state for a snapshot.
    pub fn save(&self) -> Vec<u8> {
        let mut b: Vec<u8> = Vec::new();
        b.extend(self.device_features_sel.to_le_bytes());
        b.extend(self.driver_features_sel.to_le_bytes());
        b.extend(self.driver_features.to_le_bytes());
        b.extend(self.status.to_le_bytes());
        b.extend(self.queue_sel.to_le_bytes());
        b.extend(self.interrupt_status.to_le_bytes());
        for q in &self.queues {
            b.push(u8::from(q.ready));
            b.extend(q.size.to_le_bytes());
            b.extend(q.desc.to_le_bytes());
            b.extend(q.avail.to_le_bytes());
            b.extend(q.used.to_le_bytes());
            b.extend(q.next_avail.to_le_bytes());
            b.extend(q.next_used.to_le_bytes());
        }
        b
    }

    /// Restores transport state produced by [`save`](Self::save).
    pub fn load(&mut self, data: &[u8]) -> Result<()> {
        let mut c: Cursor<'_> = Cursor { data, pos: 0 };
        self.device_features_sel = c.u32()?;
        self.driver_features_sel = c.u32()?;
        self.driver_features = c.u64()?;
        self.status = c.u32()?;
        self.queue_sel = c.u32()?;
        self.interrupt_status = c.u32()?;
        for q in &mut self.queues {
            q.ready = c.take(1)?[0] != 0;
            q.size = c.u16()?;
            q.desc = c.u64()?;
            q.avail = c.u64()?;
            q.used = c.u64()?;
            q.next_avail = c.u16()?;
            q.next_used = c.u16()?;
        }
        Ok(())
    }

    /// After a restore, drains anything the guest queued for transmit before the snapshot.
    pub fn resume(&mut self) {
        if self.queues[TX_QUEUE].ready {
            self.process_tx();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_derives_gateway_and_macs() {
        let cfg = NetConfig::parse("10.0.0.2/24").unwrap();
        assert_eq!(cfg.guest_ip, Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(cfg.prefix, 24);
        assert_eq!(cfg.host_ip, Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(cfg.netmask, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(cfg.mac, [0x52, 0x54, 0x00, 0, 0, 2]);
        assert_eq!(cfg.host_mac, [0x52, 0x54, 0x00, 0, 0, 1]);
    }

    #[test]
    fn config_derives_gateway_from_subnet() {
        let cfg = NetConfig::parse("192.168.5.37/28").unwrap();
        assert_eq!(cfg.host_ip, Ipv4Addr::new(192, 168, 5, 33));
        assert_eq!(cfg.netmask, Ipv4Addr::new(255, 255, 255, 240));
    }

    #[test]
    fn config_rejects_gateway_collision() {
        assert!(NetConfig::parse("10.0.0.1/24").is_err());
    }

    #[test]
    fn config_rejects_bad_input() {
        assert!(NetConfig::parse("10.0.0.2").is_err());
        assert!(NetConfig::parse("not-an-ip/24").is_err());
        assert!(NetConfig::parse("10.0.0.2/40").is_err());
    }

    #[test]
    fn config_header_round_trips() {
        let cfg = NetConfig::parse("192.168.5.10/24").unwrap();
        let header = cfg.save_header();
        let (back, used) = NetConfig::from_header(&header).unwrap();
        assert_eq!(used, header.len());
        assert_eq!(back.guest_ip, cfg.guest_ip);
        assert_eq!(back.prefix, cfg.prefix);
        assert_eq!(back.host_ip, cfg.host_ip);
        assert_eq!(back.mac, cfg.mac);
        assert_eq!(back.host_mac, cfg.host_mac);
    }

    #[test]
    fn config_header_rejects_short_input() {
        assert!(NetConfig::from_header(&[10, 0, 0]).is_err());
    }

    #[test]
    fn cmdline_fragment_points_at_the_nic() {
        let cfg = NetConfig::parse("10.0.0.2/24").unwrap();
        let frag = cfg.cmdline_fragment();
        assert!(frag.contains(&format!("{NET_MMIO_SIZE:#x}@{NET_MMIO_BASE:#x}:{NET_IRQ}")));
        assert!(frag.contains("virtnet_ip=10.0.0.2"));
        assert!(frag.contains("virtnet_gw=10.0.0.1"));
    }

    #[test]
    fn external_snapshot_header_has_guest_identity_only() {
        let identity = ExternalIdentity {
            mac: [0, 0x15, 0x5d, 1, 2, 3],
            mtu: 1500,
            bootstrap: br#"{"version":1,"ipv4":{"address":"192.168.0.12"}}"#.to_vec(),
            bootstrap_digest: [0xa5; 32],
        };
        let header = NetConfig::save_external_header(&identity);
        let (loaded, consumed) = NetConfig::load_external_header(&header).unwrap();
        assert_eq!(loaded, identity);
        assert_eq!(consumed, header.len());
        assert!(NetConfig::is_external_snapshot(&header));
    }
}
