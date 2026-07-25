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
//! Frames flow over bounded channels: [`FrameBackend::try_send`] (guest -> NAT, called on the
//! vCPU thread) and [`FrameBackend::recv_timeout`] (NAT -> guest, drained by the RX pump thread).
//!

use ::std::net::Ipv4Addr;
use ::std::sync::Arc;
use ::std::sync::Mutex;
use ::std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ::std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use ::std::thread::{self, JoinHandle};
use ::std::time::Duration;
use ::std::time::Instant;

use crate::egress::EgressFilter;
use crate::whp::net::{BackendHealth, FrameBackend, FrameCounters, FrameSend, NetConfig};

const FRAME_QUEUE_DEPTH: usize = 256;

mod proto;
mod tcp;
mod udp;

use proto::IpProto;

/// EtherType for ARP.
const ETH_ARP: u16 = 0x0806;
/// EtherType for IPv4.
const ETH_IPV4: u16 = 0x0800;

/// The standalone SLIRP implementation of the generic raw-frame backend.
pub struct Slirp {
    to_nat: SyncSender<Vec<u8>>,
    to_guest: Mutex<Receiver<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
    tx_accepted: AtomicU64,
    tx_dropped: AtomicU64,
    rx_received: AtomicU64,
}

impl Slirp {
    fn shutdown_worker(&self, timeout: Duration) -> ::anyhow::Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + timeout;
        loop {
            let finished = self
                .worker
                .lock()
                .expect("slirp worker lock poisoned")
                .as_ref()
                .is_none_or(JoinHandle::is_finished);
            if finished {
                if let Some(worker) = self
                    .worker
                    .lock()
                    .expect("slirp worker lock poisoned")
                    .take()
                {
                    worker
                        .join()
                        .map_err(|_| ::anyhow::anyhow!("SLIRP worker panicked"))?;
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                ::anyhow::bail!("timed out stopping SLIRP worker");
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn stop_bounded(&self) {
        if let Err(error) = self.shutdown_worker(Duration::from_secs(2)) {
            log::error!("virt-net: {error:#}");
            self.worker
                .lock()
                .expect("slirp worker lock poisoned")
                .take();
        }
    }
}

impl Drop for Slirp {
    fn drop(&mut self) {
        self.stop_bounded();
    }
}

/// Starts the NAT worker for `cfg`.
pub fn start(cfg: &NetConfig, egress_filter: &EgressFilter) -> Arc<Slirp> {
    let (to_nat, from_guest): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) =
        sync_channel(FRAME_QUEUE_DEPTH);
    let (to_guest_tx, to_guest): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) =
        sync_channel(FRAME_QUEUE_DEPTH);
    let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let worker_cfg: NetConfig = cfg.clone();
    let worker_filter = egress_filter.clone();
    let worker_stop: Arc<AtomicBool> = Arc::clone(&stop);
    let worker: JoinHandle<()> = thread::Builder::new()
        .name("whp-slirp".into())
        .spawn(move || {
            let mut nat: Nat = Nat::new(
                worker_cfg,
                worker_filter,
                to_guest_tx,
                Arc::clone(&worker_stop),
            );
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
                    }
                    Err(::std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(::std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                nat.poll_sockets();
            }
        })
        .expect("spawning slirp worker");

    Arc::new(Slirp {
        to_nat,
        to_guest: Mutex::new(to_guest),
        stop,
        worker: Mutex::new(Some(worker)),
        tx_accepted: AtomicU64::new(0),
        tx_dropped: AtomicU64::new(0),
        rx_received: AtomicU64::new(0),
    })
}

impl FrameBackend for Slirp {
    fn try_send(&self, frame: Vec<u8>) -> FrameSend {
        match self.to_nat.try_send(frame) {
            Ok(()) => {
                self.tx_accepted.fetch_add(1, Ordering::Relaxed);
                FrameSend::Accepted
            }
            Err(TrySendError::Full(_)) => FrameSend::Backpressure,
            Err(TrySendError::Disconnected(_)) => {
                self.tx_dropped.fetch_add(1, Ordering::Relaxed);
                FrameSend::Dropped
            }
        }
    }

    fn recv_timeout(&self, timeout: Duration) -> Option<Vec<u8>> {
        match self
            .to_guest
            .lock()
            .expect("slirp RX lock poisoned")
            .recv_timeout(timeout)
        {
            Ok(frame) => {
                self.rx_received.fetch_add(1, Ordering::Relaxed);
                Some(frame)
            }
            Err(_) => None,
        }
    }

    fn health(&self) -> BackendHealth {
        if self.stop.load(Ordering::Acquire) {
            BackendHealth::Stopped
        } else {
            BackendHealth::Ready
        }
    }

    fn check_health(&self) -> ::anyhow::Result<()> {
        Ok(())
    }

    fn counters(&self) -> FrameCounters {
        FrameCounters {
            guest_tx_accepted: self.tx_accepted.load(Ordering::Relaxed),
            guest_tx_dropped: self.tx_dropped.load(Ordering::Relaxed),
            guest_rx_received: self.rx_received.load(Ordering::Relaxed),
            ..Default::default()
        }
    }

    fn quiesce(&self, timeout: Duration) -> ::anyhow::Result<()> {
        self.shutdown_worker(timeout)
    }

    fn shutdown(&self) {
        self.stop_bounded();
    }
}

/// The NAT state machine, owned by the worker thread.
struct Nat {
    cfg: NetConfig,
    egress_filter: EgressFilter,
    /// Sink for frames destined to the guest.
    to_guest: SyncSender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    /// Guest MAC, learned from the first frame (falls back to the configured MAC).
    guest_mac: [u8; 6],
    udp: udp::UdpNat,
    tcp: tcp::TcpNat,
}

impl Nat {
    fn new(
        cfg: NetConfig,
        egress_filter: EgressFilter,
        to_guest: SyncSender<Vec<u8>>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        let guest_mac: [u8; 6] = cfg.mac;
        Self {
            cfg,
            egress_filter,
            to_guest,
            stop,
            guest_mac,
            udp: udp::UdpNat::new(),
            tcp: tcp::TcpNat::new(),
        }
    }

    /// Sends one fully-formed Ethernet frame to the guest. Bounded-channel pressure pauses the
    /// NAT state machine instead of dropping committed TCP sequence state. Shutdown cancels the
    /// wait so snapshot quiescing cannot deadlock behind a full guest RX ring.
    fn emit(&self, frame: Vec<u8>) -> bool {
        let mut frame = frame;
        loop {
            match self.to_guest.try_send(frame) {
                Ok(()) => return true,
                Err(TrySendError::Full(pending)) => {
                    if self.stop.load(Ordering::Acquire) {
                        return false;
                    }
                    frame = pending;
                    thread::sleep(Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
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
            _ => {}
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
        let _ = self.emit(reply);
    }

    /// Handles one IPv4 packet from the guest.
    fn on_ipv4(&mut self, ip: &[u8]) {
        let Some(pkt) = proto::Ipv4Packet::parse(ip) else {
            return;
        };
        if !self.egress_filter.allows(pkt.dst) {
            self.reject_ipv4(ip);
            return;
        }
        match pkt.proto {
            IpProto::Icmp => self.on_icmp(&pkt),
            IpProto::Udp => {
                if let Some(reply) = self.udp.on_guest(&self.cfg, &pkt) {
                    let _ = self.emit(self.frame_to_guest(&reply));
                }
            }
            IpProto::Tcp => {
                let out: Vec<Vec<u8>> = self.tcp.on_guest(&self.cfg, &pkt);
                for ip_reply in out {
                    if !self.emit(self.frame_to_guest(&ip_reply)) {
                        break;
                    }
                }
            }
            IpProto::Other(_) => {}
        }
    }

    /// Reports a policy denial as ICMP destination-unreachable / administratively prohibited.
    fn reject_ipv4(&self, original: &[u8]) {
        let quoted = &original[..original.len().min(28)];
        let mut icmp = Vec::with_capacity(8 + quoted.len());
        icmp.extend_from_slice(&[3, 13, 0, 0]); // destination unreachable, admin prohibited
        icmp.extend_from_slice(&[0, 0, 0, 0]);
        icmp.extend_from_slice(quoted);
        let checksum = proto::checksum(&icmp);
        icmp[2..4].copy_from_slice(&checksum.to_be_bytes());
        let reply = proto::build_ipv4(self.cfg.host_ip, self.cfg.guest_ip, IpProto::Icmp, &icmp);
        let _ = self.emit(self.frame_to_guest(&reply));
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
        let _ = self.emit(self.frame_to_guest(&ip_reply));
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
            if !self.emit(self.frame_to_guest(&ip)) {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::std::sync::mpsc::{TryRecvError, channel};

    #[test]
    fn guest_output_backpressure_waits_instead_of_dropping() {
        let cfg = NetConfig::parse("10.0.0.2/24").unwrap();
        let (frames, receiver) = sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let nat = Nat::new(cfg, EgressFilter::AllowAll, frames, Arc::clone(&stop));
        assert!(nat.emit(vec![1]));

        let (completed, completion) = channel();
        let sender = thread::spawn(move || {
            completed.send(nat.emit(vec![2])).unwrap();
        });
        thread::sleep(Duration::from_millis(20));
        assert_eq!(completion.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(receiver.recv().unwrap(), vec![1]);
        assert!(completion.recv_timeout(Duration::from_secs(1)).unwrap());
        assert_eq!(receiver.recv().unwrap(), vec![2]);
        sender.join().unwrap();
    }

    #[test]
    fn unlisted_dns_destination_returns_admin_prohibited() {
        let cfg = NetConfig::parse("10.0.0.2/24").unwrap();
        let filter = EgressFilter::parse(&["10.0.0.0/8".to_string()], &[]).unwrap();
        let (frames, receiver) = sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let mut nat = Nat::new(cfg.clone(), filter, frames, stop);
        let mut tcp = vec![0_u8; 20];
        tcp[2..4].copy_from_slice(&53_u16.to_be_bytes());
        let packet = proto::build_ipv4(
            cfg.guest_ip,
            "192.168.1.10".parse().unwrap(),
            IpProto::Tcp,
            &tcp,
        );

        nat.on_ipv4(&packet);

        let frame = receiver.recv().unwrap();
        assert_eq!(&frame[34..36], &[3, 13]);
    }
}
