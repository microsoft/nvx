// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The idea of exposing host networking to the guest -- here a point-to-point link the guest
// kernel drives itself -- mirrors the Nanvix Micro-VM (uservm) `-allow-host-networking` option,
// Copyright(c) The Maintainers of Nanvix, MIT License.

//!
//! # Virtual Network (virt-net)
//!
//! Gives the guest a real NIC that the stock Linux kernel drives, exposing a host network
//! endpoint to the guest. The device is a **virtio-net** function on a **virtio-mmio** (version 2,
//! VIRTIO 1.0) transport, backed by a host **TAP** interface.
//!
//! This mirrors the project's virt-fs approach (and, in spirit, the Nanvix micro-VM's
//! `-allow-host-networking`): the VMM places the device in a fixed guest-physical window and
//! points the guest at it through the kernel command line, so the guest side needs no custom
//! driver -- only the stock `virtio_mmio`/`virtio_net` options and a `virtio_mmio.device=` token:
//!
//! ```text
//! virtio_mmio.device=0x1000@0xd0000000:10 virtnet_ip=<ip>/<prefix> virtnet_gw=<host>
//! ```
//!
//! `--net <ip>/<prefix>` is the guest endpoint; the host side of the TAP link takes the first
//! address of the same subnet (e.g. `--net 10.0.0.2/24` gives the guest `10.0.0.2` and makes the
//! host `10.0.0.1` its gateway). The VMM creates and configures the TAP (via `ip`, using `sudo`
//! when not already privileged), owns it for the VM's lifetime, and tears it down on exit.
//!
//! ## Data path
//!
//! - **TX** (guest -> host): the guest adds a header+frame chain to the transmit virtqueue and
//!   writes `QueueNotify`; the vCPU thread drains the queue, strips the 12-byte `virtio_net_hdr`,
//!   and `write`s the Ethernet frame to the TAP.
//! - **RX** (host -> guest): a dedicated thread `poll`s the TAP; each frame is prefixed with a
//!   zeroed 12-byte header and scattered into the next receive-queue buffer, then the buffer is
//!   returned on the used ring and the NIC's IRQ (line 10, through the in-kernel 8259 PIC via an
//!   `irqfd`) is raised.
//!

use ::std::net::Ipv4Addr;
use ::std::os::fd::{
    AsRawFd,
    FromRawFd,
    OwnedFd,
    RawFd,
};
use ::std::process::Command;
use ::std::sync::atomic::{
    AtomicBool,
    Ordering,
    fence,
};
use ::std::sync::{
    Arc,
    Mutex,
};
use ::std::thread::JoinHandle;

use ::anyhow::{
    Context,
    Result,
    bail,
};
use ::kvm_ioctls::VmFd;
use ::log::{
    debug,
    info,
    warn,
};
use ::vmm_sys_util::eventfd::EventFd;

use crate::memory::GuestRam;

/// Guest-physical base of the virtio-mmio device window. It lives in the MMIO gap
/// (`0xC000_0000..4 GiB`), which is never reported to the guest as RAM, so accesses there trap
/// out to the VMM instead of hitting a memory slot.
pub const NET_MMIO_BASE: u64 = 0xd000_0000;
/// Size of the virtio-mmio device window (one page is ample for the registers + config space).
pub const NET_MMIO_SIZE: u64 = 0x1000;
/// Legacy IRQ line the NIC raises. With no ACPI/MPS the guest runs the in-kernel 8259 PIC, so a
/// line in `0..=15` is delivered through it (via an `irqfd` on the matching GSI).
pub const NET_IRQ: u32 = 10;

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

/// Length of the `virtio_net_hdr_v1` prepended to every frame. When `VIRTIO_F_VERSION_1` is
/// negotiated the Linux driver always uses the 12-byte (`num_buffers`-carrying) header, even
/// without mergeable RX buffers.
const NET_HDR_LEN: usize = 12;

/// Maximum virtqueue size the device advertises.
const QUEUE_SIZE_MAX: u16 = 256;

/// Number of virtqueues (receive + transmit).
const NUM_QUEUES: usize = 2;
/// Receive virtqueue index (device writes guest buffers).
const RX_QUEUE: usize = 0;
/// Transmit virtqueue index (device reads guest buffers).
const TX_QUEUE: usize = 1;

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
    /// Host TAP interface name.
    pub tap: String,
}

impl NetConfig {
    ///
    /// # Description
    ///
    /// Parses a `--net` value of the form `<ipv4>/<prefix>` (e.g. `10.0.0.2/24`) into the guest
    /// and host endpoints of a point-to-point TAP link. The host/gateway address is the first
    /// usable address of the guest's subnet (network + 1).
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
        if !(1..=30).contains(&prefix) {
            bail!("--net: prefix /{prefix} out of range (use 1..=30)");
        }

        // Host/gateway = network address + 1.
        let mask: u32 = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
        let guest_u32: u32 = u32::from(guest_ip);
        let network: u32 = guest_u32 & mask;
        let host_ip: Ipv4Addr = Ipv4Addr::from(network + 1);
        if host_ip == guest_ip {
            bail!(
                "--net: guest IP {guest_ip} collides with the derived host/gateway {host_ip}; \
                 pick a different guest address (e.g. the .2 of the subnet)"
            );
        }

        // Locally administered MAC 52:54:00 + the low 3 octets of the guest IP.
        let o: [u8; 4] = guest_ip.octets();
        let mac: [u8; 6] = [0x52, 0x54, 0x00, o[1], o[2], o[3]];

        // A TAP name unique to this process (kept within IFNAMSIZ = 15).
        let tap: String = format!("llx{}", ::std::process::id());

        Ok(Self { guest_ip, prefix, netmask: Ipv4Addr::from(mask), host_ip, mac, tap })
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
}

/// A host TAP interface owned by the VMM for the VM's lifetime.
///
/// The TAP is created (persistent, owned by the current user), given the host/gateway address,
/// and brought up; the character device is then opened and attached with `TUNSETIFF`. On drop the
/// interface is deleted.
pub struct HostTap {
    name: String,
    fd: Option<OwnedFd>,
}

impl HostTap {
    /// Creates and configures the host TAP for `cfg`, then opens and attaches to it.
    pub fn create(cfg: &NetConfig) -> Result<Self> {
        let uid: u32 = unsafe { ::libc::getuid() };
        let host_cidr: String = format!("{}/{}", cfg.host_ip, cfg.prefix);

        // Bring up the host side. These need CAP_NET_ADMIN, so run them privileged.
        ip_priv(&["tuntap", "add", "dev", &cfg.tap, "mode", "tap", "user", &uid.to_string()])
            .with_context(|| format!("creating TAP interface {}", cfg.tap))?;
        // From here on any failure must still tear the interface down.
        let configured = (|| -> Result<()> {
            ip_priv(&["addr", "add", &host_cidr, "dev", &cfg.tap])
                .with_context(|| format!("assigning {host_cidr} to {}", cfg.tap))?;
            ip_priv(&["link", "set", "dev", &cfg.tap, "up"])
                .with_context(|| format!("bringing up {}", cfg.tap))?;
            Ok(())
        })();
        if let Err(e) = configured {
            let _ = ip_priv(&["tuntap", "del", "dev", &cfg.tap, "mode", "tap"]);
            return Err(e);
        }

        // Attach to the now-existing, user-owned TAP. Because it is owned by our uid this
        // succeeds without CAP_NET_ADMIN.
        let fd: OwnedFd = match open_tap(&cfg.tap) {
            Ok(fd) => fd,
            Err(e) => {
                let _ = ip_priv(&["tuntap", "del", "dev", &cfg.tap, "mode", "tap"]);
                return Err(e);
            },
        };

        info!(
            "virt-net: host TAP {} up ({}), guest {}/{} via gateway {}",
            cfg.tap, host_cidr, cfg.guest_ip, cfg.prefix, cfg.host_ip
        );
        Ok(Self { name: cfg.tap.clone(), fd: Some(fd) })
    }

    /// Returns the raw file descriptor for the TAP (valid while this `HostTap` is alive).
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_ref().map(|f| f.as_raw_fd()).unwrap_or(-1)
    }
}

impl Drop for HostTap {
    fn drop(&mut self) {
        // Close our attachment first: `ip tuntap del` re-attaches to remove the interface and
        // would get EBUSY while our fd still holds it.
        self.fd = None;
        // Best-effort teardown of the interface we created.
        if let Err(e) = ip_priv(&["tuntap", "del", "dev", &self.name, "mode", "tap"]) {
            warn!("virt-net: failed to delete TAP {}: {e:#}", self.name);
        } else {
            debug!("virt-net: deleted TAP {}", self.name);
        }
    }
}

/// Opens `/dev/net/tun` and attaches it to the existing TAP `name` (non-blocking, no packet info).
fn open_tap(name: &str) -> Result<OwnedFd> {
    const TUNSETIFF: ::libc::c_ulong = 0x4004_54ca;
    const IFF_TAP: u16 = 0x0002;
    const IFF_NO_PI: u16 = 0x1000;

    if name.len() >= 16 {
        bail!("TAP name {name:?} exceeds IFNAMSIZ");
    }

    // SAFETY: opening a character device with a constant path and flags.
    let raw: ::libc::c_int =
        unsafe { ::libc::open(c"/dev/net/tun".as_ptr(), ::libc::O_RDWR | ::libc::O_NONBLOCK) };
    if raw < 0 {
        return Err(::std::io::Error::last_os_error()).context("opening /dev/net/tun");
    }
    // SAFETY: `raw` is a freshly opened, owned descriptor.
    let owned: OwnedFd = unsafe { OwnedFd::from_raw_fd(raw) };

    // struct ifreq is 40 bytes: char ifr_name[16] followed by a 24-byte union; TUNSETIFF only
    // reads ifr_name and the ifr_flags short at offset 16.
    let mut ifr: [u8; 40] = [0; 40];
    ifr[..name.len()].copy_from_slice(name.as_bytes());
    ifr[16..18].copy_from_slice(&(IFF_TAP | IFF_NO_PI).to_le_bytes());

    // SAFETY: `owned` is a valid tun fd and `ifr` is a correctly sized `struct ifreq`.
    let rc: ::libc::c_int =
        unsafe { ::libc::ioctl(owned.as_raw_fd(), TUNSETIFF, ifr.as_mut_ptr()) };
    if rc < 0 {
        return Err(::std::io::Error::last_os_error())
            .with_context(|| format!("TUNSETIFF on {name}"));
    }
    Ok(owned)
}

/// Runs `ip <args>`, escalating with `sudo -n` when the process is not already privileged.
fn ip_priv(args: &[&str]) -> Result<()> {
    let root: bool = unsafe { ::libc::geteuid() } == 0;
    let mut cmd: Command = if root {
        Command::new("ip")
    } else {
        let mut c: Command = Command::new("sudo");
        c.arg("-n").arg("ip");
        c
    };
    let output = cmd
        .args(args)
        .output()
        .context("failed to run `ip` (is iproute2 installed?)")?;
    if !output.status.success() {
        let stderr: String = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let how: &str = if root { "ip" } else { "sudo -n ip" };
        bail!(
            "`{how} {}` failed: {stderr}\n(--net needs privileges to manage the host TAP; run the \
             VMM as root or enable passwordless sudo for `ip`)",
            args.join(" ")
        );
    }
    Ok(())
}

/// One split virtqueue's device-visible state.
#[derive(Clone, Copy, Default)]
struct Queue {
    /// Whether the driver has marked the queue ready for use.
    ready: bool,
    /// Negotiated queue size (number of descriptors).
    size: u16,
    /// Guest-physical address of the descriptor table.
    desc: u64,
    /// Guest-physical address of the available ring (driver area).
    avail: u64,
    /// Guest-physical address of the used ring (device area).
    used: u64,
    /// Next available-ring index the device will consume.
    next_avail: u16,
    /// Next used-ring index the device will publish.
    next_used: u16,
}

impl Queue {
    /// Pops the head descriptor index of the next available chain, or `None` if none is pending.
    fn pop_avail(&mut self, mem: &GuestRam) -> Option<u16> {
        if !self.ready || self.size == 0 {
            return None;
        }
        let avail_idx: u16 = mem.read_u16(self.avail.wrapping_add(2));
        // Ensure the ring contents are observed after the index that advertises them.
        fence(Ordering::Acquire);
        if self.next_avail == avail_idx {
            return None;
        }
        let slot: u64 = u64::from(self.next_avail % self.size);
        let head: u16 = mem.read_u16(self.avail.wrapping_add(4 + 2 * slot));
        self.next_avail = self.next_avail.wrapping_add(1);
        Some(head)
    }

    /// Publishes `id` (a chain head) on the used ring with `len` bytes written by the device.
    fn push_used(&mut self, mem: &GuestRam, id: u32, len: u32) {
        if self.size == 0 {
            return;
        }
        let slot: u64 = u64::from(self.next_used % self.size);
        let elem: u64 = self.used.wrapping_add(4 + 8 * slot);
        mem.write_u32(elem, id);
        mem.write_u32(elem.wrapping_add(4), len);
        self.next_used = self.next_used.wrapping_add(1);
        // Make the used element visible before bumping the index the driver polls.
        fence(Ordering::Release);
        mem.write_u16(self.used.wrapping_add(2), self.next_used);
    }
}

/// Reads a 16-byte virtqueue descriptor: `(addr, len, flags, next)`.
fn read_desc(mem: &GuestRam, table: u64, idx: u16) -> (u64, u32, u16, u16) {
    let base: u64 = table.wrapping_add(u64::from(idx) * 16);
    (
        mem.read_u64(base),
        mem.read_u32(base.wrapping_add(8)),
        mem.read_u16(base.wrapping_add(12)),
        mem.read_u16(base.wrapping_add(14)),
    )
}

/// The virtio-net-mmio device: MMIO register state plus the two virtqueues and the TAP backend.
pub struct VirtioNet {
    /// Guest RAM accessor for virtqueue DMA.
    mem: GuestRam,
    /// Host TAP descriptor (owned by [`HostTap`]; valid for the VM's lifetime).
    tap: RawFd,
    /// IRQ line, raised by signalling this eventfd (registered as an `irqfd`).
    irq: Arc<EventFd>,
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
    /// Creates the device backed by TAP `tap`, raising `irq`, presenting `mac` to the guest.
    pub fn new(mem: GuestRam, tap: RawFd, irq: Arc<EventFd>, mac: [u8; 6]) -> Self {
        Self {
            mem,
            tap,
            irq,
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
        // Device-specific configuration space (the MAC, read byte-wise by the driver).
        if offset >= REG_CONFIG {
            let base: usize = (offset - REG_CONFIG) as usize;
            for (i, b) in data.iter_mut().enumerate() {
                *b = self.mac.get(base + i).copied().unwrap_or(0);
            }
            return;
        }

        let sel: usize = self.queue_sel as usize;
        let val: u32 = match offset {
            REG_MAGIC => VIRTIO_MMIO_MAGIC,
            REG_VERSION => VIRTIO_MMIO_VERSION,
            REG_DEVICE_ID => VIRTIO_ID_NET,
            REG_VENDOR_ID => VIRTIO_VENDOR,
            REG_DEVICE_FEATURES => {
                let feat: u64 = Self::device_features();
                if self.device_features_sel == 1 {
                    (feat >> 32) as u32
                } else {
                    feat as u32
                }
            },
            REG_QUEUE_NUM_MAX => u32::from(QUEUE_SIZE_MAX),
            REG_QUEUE_READY => {
                u32::from(self.queues.get(sel).map(|q| q.ready).unwrap_or(false))
            },
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

    /// Services a guest MMIO write of `data` at `offset` within the device window. A write to
    /// `QueueNotify` for the transmit queue drains it to the TAP.
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
                    self.driver_features =
                        (self.driver_features & !0xffff_ffff) | u64::from(val);
                }
            },
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = val,
            REG_QUEUE_SEL => self.queue_sel = val,
            REG_QUEUE_NUM => {
                if let Some(q) = self.queues.get_mut(sel) {
                    q.size = val as u16;
                }
            },
            REG_QUEUE_READY => {
                if let Some(q) = self.queues.get_mut(sel) {
                    q.ready = val == 1;
                    if q.ready {
                        q.next_avail = 0;
                        q.next_used = 0;
                        // Initialise the used-ring header the device owns.
                        let used: u64 = q.used;
                        self.mem.write_u16(used, 0);
                        self.mem.write_u16(used.wrapping_add(2), 0);
                    }
                }
            },
            REG_QUEUE_NOTIFY => {
                if val as usize == TX_QUEUE {
                    self.process_tx();
                }
            },
            REG_INTERRUPT_ACK => self.interrupt_status &= !val,
            REG_STATUS => {
                self.status = val;
                if val == 0 {
                    self.reset();
                }
            },
            REG_QUEUE_DESC_LOW => self.set_queue_addr(sel, |q| &mut q.desc, val, false),
            REG_QUEUE_DESC_HIGH => self.set_queue_addr(sel, |q| &mut q.desc, val, true),
            REG_QUEUE_DRIVER_LOW => self.set_queue_addr(sel, |q| &mut q.avail, val, false),
            REG_QUEUE_DRIVER_HIGH => self.set_queue_addr(sel, |q| &mut q.avail, val, true),
            REG_QUEUE_DEVICE_LOW => self.set_queue_addr(sel, |q| &mut q.used, val, false),
            REG_QUEUE_DEVICE_HIGH => self.set_queue_addr(sel, |q| &mut q.used, val, true),
            _ => {},
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

    /// Drains the transmit queue, writing each guest frame to the TAP.
    fn process_tx(&mut self) {
        let mem: GuestRam = self.mem.clone();
        if !self.queues[TX_QUEUE].ready {
            return;
        }
        let mut raised: bool = false;
        while let Some(head) = self.queues[TX_QUEUE].pop_avail(&mem) {
            let table: u64 = self.queues[TX_QUEUE].desc;
            let size: u16 = self.queues[TX_QUEUE].size;

            // Gather the readable (driver -> device) descriptor chain into one frame buffer.
            let mut frame: Vec<u8> = Vec::new();
            let mut idx: u16 = head;
            let mut hops: u16 = 0;
            loop {
                let (addr, len, flags, next) = read_desc(&mem, table, idx);
                if flags & VIRTQ_DESC_F_WRITE == 0 && len > 0 {
                    let start: usize = frame.len();
                    frame.resize(start + len as usize, 0);
                    let _ = mem.read(addr, &mut frame[start..]);
                }
                hops += 1;
                if flags & VIRTQ_DESC_F_NEXT != 0 && hops <= size {
                    idx = next;
                } else {
                    break;
                }
            }

            // Strip the virtio_net_hdr and hand the raw Ethernet frame to the TAP.
            if frame.len() > NET_HDR_LEN {
                tap_write(self.tap, &frame[NET_HDR_LEN..]);
            }
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
        let mem: GuestRam = self.mem.clone();
        let head: u16 = match self.queues[RX_QUEUE].pop_avail(&mem) {
            Some(h) => h,
            None => return false,
        };
        let table: u64 = self.queues[RX_QUEUE].desc;
        let size: u16 = self.queues[RX_QUEUE].size;

        // Payload delivered to the guest: 12-byte header (num_buffers = 1) followed by the frame.
        let mut payload: Vec<u8> = Vec::with_capacity(NET_HDR_LEN + frame.len());
        payload.extend_from_slice(&[0u8; NET_HDR_LEN]);
        payload[10] = 1; // virtio_net_hdr_v1.num_buffers = 1
        payload.extend_from_slice(frame);

        // Scatter across the writable (device -> driver) descriptor chain.
        let mut written: usize = 0;
        let mut idx: u16 = head;
        let mut hops: u16 = 0;
        loop {
            let (addr, len, flags, next) = read_desc(&mem, table, idx);
            if flags & VIRTQ_DESC_F_WRITE != 0 && written < payload.len() {
                let take: usize = (len as usize).min(payload.len() - written);
                if take > 0 {
                    let _ = mem.write(addr, &payload[written..written + take]);
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
            warn!(
                "virt-net: RX buffer too small ({written} < {}); frame truncated",
                payload.len()
            );
        }
        self.queues[RX_QUEUE].push_used(&mem, u32::from(head), written as u32);
        self.raise_irq();
        true
    }

    /// Raises the used-buffer interrupt: latch the status bit, then pulse the IRQ line.
    fn raise_irq(&mut self) {
        self.interrupt_status |= INT_USED_RING;
        if let Err(e) = self.irq.write(1) {
            warn!("virt-net: failed to signal IRQ: {e}");
        }
    }
}

/// Writes one Ethernet frame to the TAP, tolerating a transiently full queue.
fn tap_write(fd: RawFd, frame: &[u8]) {
    // SAFETY: `fd` is a live TAP descriptor; `frame` is a valid readable slice.
    let n: ::libc::ssize_t =
        unsafe { ::libc::write(fd, frame.as_ptr().cast::<::libc::c_void>(), frame.len()) };
    if n < 0 {
        let err = ::std::io::Error::last_os_error();
        if err.raw_os_error() != Some(::libc::EAGAIN) {
            debug!("virt-net: TAP write failed: {err}");
        }
    }
}

///
/// # Description
///
/// Spawns the receive thread: it `poll`s the TAP and feeds each frame to [`VirtioNet::process_rx`]
/// until `stop` is set. Returns the thread handle so the caller can join it on shutdown (before
/// the guest memory the device DMAs into is torn down).
///
pub fn spawn_rx_thread(
    dev: Arc<Mutex<VirtioNet>>,
    tap: RawFd,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    ::std::thread::spawn(move || {
        let mut buf: Vec<u8> = vec![0u8; 65536];
        while !stop.load(Ordering::Relaxed) {
            // Wait up to 200 ms for a frame, re-checking `stop` between waits.
            let mut pfd: ::libc::pollfd = ::libc::pollfd {
                fd: tap,
                events: ::libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `pfd` is a single valid pollfd for the live TAP descriptor.
            let rc: ::libc::c_int = unsafe { ::libc::poll(&mut pfd, 1, 200) };
            if rc <= 0 || pfd.revents & ::libc::POLLIN == 0 {
                continue;
            }
            // Drain all frames currently queued on the TAP.
            loop {
                // SAFETY: `buf` is a valid, sufficiently large writable buffer.
                let n: ::libc::ssize_t = unsafe {
                    ::libc::read(tap, buf.as_mut_ptr().cast::<::libc::c_void>(), buf.len())
                };
                if n <= 0 {
                    break;
                }
                let frame: &[u8] = &buf[..n as usize];
                let mut guard = dev.lock().expect("virt-net device poisoned");
                let _ = guard.process_rx(frame);
                drop(guard);
            }
        }
        debug!("virt-net: RX thread stopped");
    })
}

/// Registers the NIC's `irqfd` on [`NET_IRQ`] so the device thread can inject its interrupt
/// without touching the VM fd, and returns the eventfd used to raise it.
pub fn register_irq(vm_fd: &VmFd) -> Result<Arc<EventFd>> {
    let evt: EventFd =
        EventFd::new(::libc::EFD_NONBLOCK).context("creating virt-net IRQ eventfd")?;
    vm_fd
        .register_irqfd(&evt, NET_IRQ)
        .context("registering virt-net irqfd")?;
    Ok(Arc::new(evt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_derives_gateway_mac_and_prefix() {
        let cfg = NetConfig::parse("10.0.0.2/24").unwrap();
        assert_eq!(cfg.guest_ip, Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(cfg.prefix, 24);
        assert_eq!(cfg.netmask, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(cfg.host_ip, Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(cfg.mac, [0x52, 0x54, 0x00, 0, 0, 2]);
    }

    #[test]
    fn parse_gateway_is_network_plus_one() {
        // 192.168.5.37/28 -> network 192.168.5.32, gateway 192.168.5.33, mask 255.255.255.240.
        let cfg = NetConfig::parse("192.168.5.37/28").unwrap();
        assert_eq!(cfg.host_ip, Ipv4Addr::new(192, 168, 5, 33));
        assert_eq!(cfg.netmask, Ipv4Addr::new(255, 255, 255, 240));
    }

    #[test]
    fn parse_rejects_gateway_collision() {
        // The .1 host of a /24 collides with the derived gateway.
        assert!(NetConfig::parse("10.0.0.1/24").is_err());
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert!(NetConfig::parse("10.0.0.2").is_err());
        assert!(NetConfig::parse("not-an-ip/24").is_err());
        assert!(NetConfig::parse("10.0.0.2/40").is_err());
    }

    #[test]
    fn cmdline_fragment_has_device_and_addresses() {
        let cfg = NetConfig::parse("10.0.0.2/24").unwrap();
        let frag = cfg.cmdline_fragment();
        assert!(frag.contains("virtio_mmio.device=0x1000@0xd0000000:10"));
        assert!(frag.contains("virtnet_ip=10.0.0.2"));
        assert!(frag.contains("virtnet_mask=255.255.255.0"));
        assert!(frag.contains("virtnet_gw=10.0.0.1"));
    }
}
