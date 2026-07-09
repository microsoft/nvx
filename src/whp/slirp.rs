// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # User-mode NAT (slirp-style)
//!
//! The host side of the WHP virt-net NIC. Windows has no unprivileged layer-2 TAP, so instead of
//! bridging Ethernet frames to a host interface the VMM *is* the gateway: it terminates the
//! guest's link and NATs traffic out through ordinary host sockets (`std::net`), needing no driver
//! and no administrator rights.
//!
//! The guest is configured (from the kernel command line) with a static address on a small subnet
//! whose first address is the gateway the VMM answers for. The worker thread:
//!
//! - answers **ARP** for the gateway with a fixed gateway MAC;
//! - answers **ICMP echo** to the gateway (so `ping <gateway>` works);
//! - NATs guest **UDP** (including DNS) out through host `UdpSocket`s and relays replies back;
//! - NATs guest **TCP** out through host `TcpStream`s, speaking TCP to the guest itself.
//!
//! Frames flow over two channels: [`Slirp::send_from_guest`] (guest -> NAT, called on the vCPU
//! thread) and the [`SlirpRx`] receiver (NAT -> guest, drained by the RX pump thread).
//!

use ::std::net::Ipv4Addr;
use ::std::sync::Arc;
use ::std::sync::atomic::{
    AtomicBool,
    Ordering,
};
use ::std::sync::mpsc::{
    Receiver,
    Sender,
    channel,
};
use ::std::thread::{
    self,
    JoinHandle,
};
use ::std::time::Duration;

use crate::whp::net::NetConfig;

mod proto;
mod tcp;
mod udp;

use proto::IpProto;

/// EtherType for ARP.
const ETH_ARP: u16 = 0x0806;
/// EtherType for IPv4.
const ETH_IPV4: u16 = 0x0800;

/// Handle held by the device to push guest frames into the NAT (non-blocking).
pub struct Slirp {
    to_nat: Sender<Vec<u8>>,
}

impl Slirp {
    /// Hands one guest-transmitted Ethernet frame to the NAT worker. Never blocks the vCPU thread.
    pub fn send_from_guest(&self, frame: Vec<u8>) {
        let _ = self.to_nat.send(frame);
    }
}

/// The NAT -> guest side: the RX pump drains this and scatters each frame into the RX virtqueue.
pub struct SlirpRx {
    /// Frames the NAT has produced for the guest.
    pub to_guest: Receiver<Vec<u8>>,
    /// Worker stop flag and join handle, so the pump can shut the worker down cleanly.
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl SlirpRx {
    /// Signals the worker to stop and joins it.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

impl Drop for SlirpRx {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Starts the NAT worker for `cfg`, returning the device-side handle and the RX side.
pub fn start(cfg: &NetConfig) -> (Slirp, SlirpRx) {
    let (to_nat, from_guest): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = channel();
    let (to_guest_tx, to_guest): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = channel();
    let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let worker_cfg: NetConfig = cfg.clone();
    let worker_stop: Arc<AtomicBool> = Arc::clone(&stop);
    let worker: JoinHandle<()> = thread::Builder::new()
        .name("whp-slirp".into())
        .spawn(move || {
            let mut nat: Nat = Nat::new(worker_cfg, to_guest_tx);
            while !worker_stop.load(Ordering::Acquire) {
                // Drain guest frames without blocking long, then service host sockets, so both
                // directions stay responsive.
                match from_guest.recv_timeout(Duration::from_millis(20)) {
                    Ok(frame) => {
                        nat.on_guest_frame(&frame);
                        // Opportunistically drain any backlog.
                        while let Ok(f) = from_guest.try_recv() {
                            nat.on_guest_frame(&f);
                        }
                    },
                    Err(::std::sync::mpsc::RecvTimeoutError::Timeout) => {},
                    Err(::std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                nat.poll_sockets();
            }
        })
        .expect("spawning slirp worker");

    (
        Slirp { to_nat },
        SlirpRx {
            to_guest,
            stop,
            worker: Some(worker),
        },
    )
}

/// The NAT state machine, owned by the worker thread.
struct Nat {
    cfg: NetConfig,
    /// Sink for frames destined to the guest.
    to_guest: Sender<Vec<u8>>,
    /// Guest MAC, learned from the first frame (falls back to the configured MAC).
    guest_mac: [u8; 6],
    udp: udp::UdpNat,
    tcp: tcp::TcpNat,
}

impl Nat {
    fn new(cfg: NetConfig, to_guest: Sender<Vec<u8>>) -> Self {
        let guest_mac: [u8; 6] = cfg.mac;
        Self {
            cfg,
            to_guest,
            guest_mac,
            udp: udp::UdpNat::new(),
            tcp: tcp::TcpNat::new(),
        }
    }

    /// Sends one fully-formed Ethernet frame to the guest.
    fn emit(&self, frame: Vec<u8>) {
        let _ = self.to_guest.send(frame);
    }

    /// Handles one Ethernet frame transmitted by the guest.
    fn on_guest_frame(&mut self, frame: &[u8]) {
        if frame.len() < 14 {
            return;
        }
        let src_mac: [u8; 6] = frame[6..12].try_into().unwrap();
        self.guest_mac = src_mac;
        let ethertype: u16 = u16::from_be_bytes([frame[12], frame[13]]);
        match ethertype {
            ETH_ARP => self.on_arp(&frame[14..]),
            ETH_IPV4 => self.on_ipv4(&frame[14..]),
            _ => {},
        }
    }

    /// Answers an ARP request for the gateway address.
    fn on_arp(&mut self, arp: &[u8]) {
        if arp.len() < 28 {
            return;
        }
        let oper: u16 = u16::from_be_bytes([arp[6], arp[7]]);
        if oper != 1 {
            return; // only requests
        }
        let sender_mac: [u8; 6] = arp[8..14].try_into().unwrap();
        let sender_ip: Ipv4Addr = Ipv4Addr::new(arp[14], arp[15], arp[16], arp[17]);
        let target_ip: Ipv4Addr = Ipv4Addr::new(arp[24], arp[25], arp[26], arp[27]);
        if target_ip != self.cfg.host_ip {
            return; // we only own the gateway address
        }

        // Build an ARP reply: gateway MAC/IP -> the asking guest.
        let mut reply: Vec<u8> = Vec::with_capacity(42);
        reply.extend_from_slice(&sender_mac); // eth dst
        reply.extend_from_slice(&self.cfg.host_mac); // eth src
        reply.extend_from_slice(&ETH_ARP.to_be_bytes());
        reply.extend_from_slice(&[0, 1]); // htype ethernet
        reply.extend_from_slice(&ETH_IPV4.to_be_bytes()); // ptype ipv4
        reply.push(6); // hlen
        reply.push(4); // plen
        reply.extend_from_slice(&2u16.to_be_bytes()); // oper reply
        reply.extend_from_slice(&self.cfg.host_mac); // sender ha
        reply.extend_from_slice(&self.cfg.host_ip.octets()); // sender pa
        reply.extend_from_slice(&sender_mac); // target ha
        reply.extend_from_slice(&sender_ip.octets()); // target pa
        self.emit(reply);
    }

    /// Handles one IPv4 packet from the guest.
    fn on_ipv4(&mut self, ip: &[u8]) {
        let Some(pkt) = proto::Ipv4Packet::parse(ip) else {
            return;
        };
        match pkt.proto {
            IpProto::Icmp => self.on_icmp(&pkt),
            IpProto::Udp => {
                if let Some(reply) = self.udp.on_guest(&self.cfg, &pkt) {
                    self.emit(self.frame_to_guest(&reply));
                }
            },
            IpProto::Tcp => {
                let out: Vec<Vec<u8>> = self.tcp.on_guest(&self.cfg, &pkt);
                for ip_reply in out {
                    self.emit(self.frame_to_guest(&ip_reply));
                }
            },
            IpProto::Other(_) => {},
        }
    }

    /// Answers ICMP echo requests addressed to the gateway (so `ping <gateway>` works).
    fn on_icmp(&mut self, pkt: &proto::Ipv4Packet<'_>) {
        if pkt.dst != self.cfg.host_ip {
            return; // no raw-socket forwarding of ICMP to third parties (needs admin)
        }
        let icmp: &[u8] = pkt.payload;
        if icmp.len() < 8 || icmp[0] != 8 {
            return; // only echo request (type 8)
        }
        // Echo reply: type 0, same identifier/sequence/data, recomputed checksum.
        let mut reply_icmp: Vec<u8> = icmp.to_vec();
        reply_icmp[0] = 0; // echo reply
        reply_icmp[2] = 0;
        reply_icmp[3] = 0;
        let csum: u16 = proto::checksum(&reply_icmp);
        reply_icmp[2..4].copy_from_slice(&csum.to_be_bytes());

        let ip_reply: Vec<u8> =
            proto::build_ipv4(self.cfg.host_ip, pkt.src, IpProto::Icmp, &reply_icmp);
        self.emit(self.frame_to_guest(&ip_reply));
    }

    /// Wraps an IPv4 packet (whose source is some host/gateway address) in an Ethernet frame
    /// addressed to the guest.
    fn frame_to_guest(&self, ip: &[u8]) -> Vec<u8> {
        let mut f: Vec<u8> = Vec::with_capacity(14 + ip.len());
        f.extend_from_slice(&self.guest_mac);
        f.extend_from_slice(&self.cfg.host_mac);
        f.extend_from_slice(&ETH_IPV4.to_be_bytes());
        f.extend_from_slice(ip);
        f
    }

    /// Services host sockets: relays UDP/TCP data that arrived from the host back to the guest.
    fn poll_sockets(&mut self) {
        let mut out: Vec<Vec<u8>> = Vec::new();
        self.udp.poll(&self.cfg, &mut out);
        self.tcp.poll(&self.cfg, &mut out);
        for ip in out {
            self.emit(self.frame_to_guest(&ip));
        }
    }
}
