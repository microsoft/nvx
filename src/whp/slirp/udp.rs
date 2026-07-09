// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! UDP NAT: relays guest UDP datagrams (including DNS) out through host `UdpSocket`s.

use ::std::collections::HashMap;
use ::std::collections::hash_map::Entry;
use ::std::net::{
    Ipv4Addr,
    SocketAddrV4,
    UdpSocket,
};
use ::std::time::{
    Duration,
    Instant,
};

use crate::whp::net::NetConfig;
use crate::whp::slirp::proto::{
    self,
    IpProto,
    Ipv4Packet,
};

/// Idle lifetime of a UDP flow before its host socket is reclaimed.
const FLOW_TTL: Duration = Duration::from_secs(30);
/// Largest UDP payload relayed in either direction.
const MAX_DGRAM: usize = 2048;

/// Identifies a guest UDP flow by its guest source port and intended destination.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FlowKey {
    guest_port: u16,
    dst_ip: Ipv4Addr,
    dst_port: u16,
}

/// One NATed UDP flow: a connected host socket plus its last-activity time.
struct Flow {
    socket: UdpSocket,
    last: Instant,
}

/// Per-flow UDP NAT state.
pub struct UdpNat {
    flows: HashMap<FlowKey, Flow>,
}

impl UdpNat {
    pub fn new() -> Self {
        Self {
            flows: HashMap::new(),
        }
    }

    /// Handles a guest UDP datagram: (re)opens a host socket to the destination and forwards the
    /// payload. Replies arrive asynchronously via [`poll`](Self::poll); returns `None` (UDP never
    /// replies synchronously).
    pub fn on_guest(&mut self, cfg: &NetConfig, pkt: &Ipv4Packet<'_>) -> Option<Vec<u8>> {
        let udp: &[u8] = pkt.payload;
        if udp.len() < 8 {
            return None;
        }
        let guest_port: u16 = u16::from_be_bytes([udp[0], udp[1]]);
        let dst_port: u16 = u16::from_be_bytes([udp[2], udp[3]]);
        let length: usize = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
        let payload: &[u8] = &udp[8..length.clamp(8, udp.len())];

        let key: FlowKey = FlowKey {
            guest_port,
            dst_ip: pkt.dst,
            dst_port,
        };
        // Datagrams to the gateway are served by the host itself (loopback), so the guest can
        // reach host services; everything else goes to the real destination address.
        let host_dst: Ipv4Addr = if pkt.dst == cfg.host_ip {
            Ipv4Addr::LOCALHOST
        } else {
            pkt.dst
        };

        let flow: &mut Flow = match self.flows.entry(key) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let socket: UdpSocket = open_socket(host_dst, dst_port)?;
                e.insert(Flow {
                    socket,
                    last: Instant::now(),
                })
            },
        };
        flow.last = Instant::now();
        let _ = flow.socket.send(payload);
        None
    }

    /// Drains any host replies, building a guest-bound IPv4/UDP packet for each, and expires idle
    /// flows.
    pub fn poll(&mut self, cfg: &NetConfig, out: &mut Vec<Vec<u8>>) {
        let mut buf: [u8; MAX_DGRAM] = [0; MAX_DGRAM];
        let now: Instant = Instant::now();
        self.flows.retain(|key, flow| {
            while let Ok(n) = flow.socket.recv(&mut buf) {
                if n == 0 {
                    break;
                }
                flow.last = now;
                // The reply's apparent source is the address the guest sent to.
                let src: Ipv4Addr = key.dst_ip;
                let udp: Vec<u8> =
                    build_udp(src, cfg.guest_ip, key.dst_port, key.guest_port, &buf[..n]);
                out.push(proto::build_ipv4(src, cfg.guest_ip, IpProto::Udp, &udp));
            }
            now.duration_since(flow.last) < FLOW_TTL
        });
    }
}

/// Opens a non-blocking host UDP socket connected to `dst:port`.
fn open_socket(dst: Ipv4Addr, port: u16) -> Option<UdpSocket> {
    let socket: UdpSocket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(SocketAddrV4::new(dst, port)).ok()?;
    socket.set_nonblocking(true).ok()?;
    Some(socket)
}

/// Builds a UDP datagram (`src_port`/`dst_port`, checksummed over the pseudo-header) for `payload`.
fn build_udp(src: Ipv4Addr, dst: Ipv4Addr, src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
    let len: u16 = 8 + payload.len() as u16;
    let mut dgram: Vec<u8> = Vec::with_capacity(len as usize);
    dgram.extend_from_slice(&src_port.to_be_bytes());
    dgram.extend_from_slice(&dst_port.to_be_bytes());
    dgram.extend_from_slice(&len.to_be_bytes());
    dgram.extend_from_slice(&[0, 0]); // checksum placeholder
    dgram.extend_from_slice(payload);
    let mut csum: u16 = proto::transport_checksum(src, dst, IpProto::Udp, &dgram);
    if csum == 0 {
        csum = 0xffff; // 0 means "no checksum" in UDP, so use the equivalent all-ones form
    }
    dgram[6..8].copy_from_slice(&csum.to_be_bytes());
    dgram
}
