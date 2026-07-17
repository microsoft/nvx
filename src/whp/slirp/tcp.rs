// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! TCP NAT: terminates the guest's TCP connections and bridges them to host `TcpStream`s.
//!
//! The VMM speaks TCP to the guest itself (handshake, sequence/acknowledgement numbers, FIN) and
//! forwards the byte stream to a host socket connected to the guest's intended destination. The
//! virtio-net link between guest and NAT is lossless and in-order, so no retransmission is needed;
//! the guest's advertised window is honoured to avoid overrunning its receive queue. Connections
//! to the gateway address are served by the host loopback, so the guest can reach host services.

use ::std::collections::HashMap;
use ::std::io::{
    Read,
    Write,
};
use ::std::net::{
    Ipv4Addr,
    SocketAddrV4,
    TcpStream,
};
use ::std::sync::atomic::{
    AtomicU32,
    Ordering,
};
use ::std::sync::mpsc::{
    Receiver,
    channel,
};
use ::std::thread;
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

// TCP flag bits.
const FIN: u8 = 0x01;
const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const PSH: u8 = 0x08;
const ACK: u8 = 0x10;

/// Idle lifetime of a connection before it is reclaimed.
const CONN_TTL: Duration = Duration::from_secs(120);
/// Window the NAT advertises to the guest.
const OUR_WINDOW: u16 = 64240;
/// Largest chunk read from a host socket per poll.
const READ_CHUNK: usize = 16384;

/// Identifies a guest TCP connection.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    guest_port: u16,
    dst_ip: Ipv4Addr,
    dst_port: u16,
}

/// Connection lifecycle.
#[derive(PartialEq, Eq)]
enum State {
    /// SYN-ACK sent; the host socket is still connecting.
    Connecting,
    /// Data may flow in both directions.
    Established,
    /// One side has sent FIN; drain the other direction, then reclaim.
    Closing,
}

/// One NATed TCP connection.
struct Conn {
    dst_ip: Ipv4Addr,
    dst_port: u16,
    guest_port: u16,
    /// Next sequence number expected from the guest.
    rcv_nxt: u32,
    /// Next sequence number we will send to the guest.
    snd_nxt: u32,
    /// Initial sequence number used for this connection's SYN-ACK.
    isn: u32,
    /// Whether the guest has acknowledged our SYN.
    syn_acked: bool,
    /// Oldest of our sequence numbers the guest has not acknowledged.
    snd_una: u32,
    /// Guest's advertised receive window.
    guest_window: u32,
    /// The connected host socket, once available.
    host: Option<TcpStream>,
    /// Pending background connect result.
    connecting: Option<Receiver<Option<TcpStream>>>,
    /// Guest data buffered until the non-blocking host socket accepts it.
    pending_out: Vec<u8>,
    guest_fin_pending: bool,
    host_write_shutdown: bool,
    state: State,
    /// Whether we have sent our FIN to the guest.
    fin_sent: bool,
    last: Instant,
}

/// Returns true if sequence number `a` is strictly after `b` (mod 2^32).
fn seq_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Per-connection TCP NAT state.
pub struct TcpNat {
    conns: HashMap<Key, Conn>,
}

impl TcpNat {
    pub fn new() -> Self {
        Self {
            conns: HashMap::new(),
        }
    }

    /// Handles a guest TCP segment, returning any IPv4 packets to send back to the guest now.
    pub fn on_guest(&mut self, cfg: &NetConfig, pkt: &Ipv4Packet<'_>) -> Vec<Vec<u8>> {
        let seg: &[u8] = pkt.payload;
        if seg.len() < 20 {
            return Vec::new();
        }
        let src_port: u16 = u16::from_be_bytes([seg[0], seg[1]]);
        let dst_port: u16 = u16::from_be_bytes([seg[2], seg[3]]);
        let seq: u32 = u32::from_be_bytes([seg[4], seg[5], seg[6], seg[7]]);
        let ack: u32 = u32::from_be_bytes([seg[8], seg[9], seg[10], seg[11]]);
        let data_off: usize = usize::from(seg[12] >> 4) * 4;
        let flags: u8 = seg[13];
        let window: u32 = u32::from(u16::from_be_bytes([seg[14], seg[15]]));
        let payload: &[u8] = seg.get(data_off..).unwrap_or(&[]);

        let key: Key = Key {
            guest_port: src_port,
            dst_ip: pkt.dst,
            dst_port,
        };
        let mut out: Vec<Vec<u8>> = Vec::new();

        if flags & RST != 0 {
            self.conns.remove(&key);
            return out;
        }

        if flags & SYN != 0 {
            if let Some(conn) = self.conns.get(&key) {
                if !conn.syn_acked {
                    out.push(build_segment(
                        cfg,
                        conn,
                        conn.isn,
                        seq.wrapping_add(1),
                        SYN | ACK,
                        &[],
                    ));
                }
                return out;
            }

            let host_dst: Ipv4Addr = if pkt.dst == cfg.host_ip {
                Ipv4Addr::LOCALHOST
            } else {
                pkt.dst
            };
            let isn: u32 = next_isn();
            let conn: Conn = Conn {
                dst_ip: pkt.dst,
                dst_port,
                guest_port: src_port,
                rcv_nxt: seq.wrapping_add(1),
                snd_nxt: isn.wrapping_add(1),
                isn,
                syn_acked: false,
                snd_una: isn,
                guest_window: window.max(1),
                host: None,
                connecting: Some(spawn_connect(host_dst, dst_port)),
                pending_out: Vec::new(),
                guest_fin_pending: false,
                host_write_shutdown: false,
                state: State::Connecting,
                fin_sent: false,
                last: Instant::now(),
            };
            // SYN-ACK.
            out.push(build_segment(cfg, &conn, isn, seq.wrapping_add(1), SYN | ACK, &[]));
            self.conns.insert(key, conn);
            return out;
        }

        let conn: &mut Conn = match self.conns.get_mut(&key) {
            Some(c) => c,
            None => return out,
        };
        conn.last = Instant::now();
        conn.guest_window = window.max(1);
        if flags & ACK != 0 && seq_gt(ack, conn.isn) {
            conn.syn_acked = true;
        }
        if flags & ACK != 0 && seq_gt(ack, conn.snd_una) {
            conn.snd_una = ack;
        }

        // In-order guest data: forward to the host (or buffer until connected), then acknowledge.
        if !payload.is_empty() && seq == conn.rcv_nxt {
            let capacity = usize::from(OUR_WINDOW).saturating_sub(conn.pending_out.len());
            let accepted = payload.len().min(capacity);
            conn.pending_out.extend_from_slice(&payload[..accepted]);
            conn.rcv_nxt = conn.rcv_nxt.wrapping_add(accepted as u32);
        }

        // Guest FIN: acknowledge it only after all payload in this segment was accepted. The
        // host write side is closed later, after the buffered payload has drained.
        if flags & FIN != 0 && seq.wrapping_add(payload.len() as u32) == conn.rcv_nxt {
            conn.rcv_nxt = conn.rcv_nxt.wrapping_add(1);
            conn.guest_fin_pending = true;
            conn.state = State::Closing;
        }

        // Acknowledge only when the guest delivered data or a FIN (a bare ACK needs no reply).
        let need_ack: bool = !payload.is_empty() || flags & FIN != 0;
        if need_ack {
            let conn: &Conn = self.conns.get(&key).unwrap();
            out.push(build_segment(cfg, conn, conn.snd_nxt, conn.rcv_nxt, ACK, &[]));
        }
        out
    }

    /// Services host sockets: completes pending connects, relays host data to the guest, and
    /// reclaims closed/idle connections.
    pub fn poll(&mut self, cfg: &NetConfig, out: &mut Vec<Vec<u8>>) {
        let now: Instant = Instant::now();
        let mut dead: Vec<Key> = Vec::new();

        for (key, conn) in self.conns.iter_mut() {
            // Complete a pending host connect.
            if conn.host.is_none()
                && let Some(rx) = &conn.connecting
            {
                match rx.try_recv() {
                        Ok(Some(stream)) => {
                            if stream.set_nonblocking(true).is_err() {
                                out.push(build_segment(
                                    cfg, conn, conn.snd_nxt, conn.rcv_nxt, RST | ACK, &[],
                                ));
                                dead.push(*key);
                                continue;
                            }
                            conn.host = Some(stream);
                            conn.connecting = None;
                            if conn.state == State::Connecting {
                                conn.state = State::Established;
                            }
                        },
                        Ok(None) => {
                            // Connect failed: reset the guest's connection.
                            out.push(build_segment(
                                cfg, conn, conn.snd_nxt, conn.rcv_nxt, RST | ACK, &[],
                            ));
                            dead.push(*key);
                            continue;
                        },
                        Err(::std::sync::mpsc::TryRecvError::Empty) => {},
                        Err(::std::sync::mpsc::TryRecvError::Disconnected) => {
                            dead.push(*key);
                            continue;
                        },
                    }
            }

            let window_before = advertised_window(conn);
            let mut host_write_failed = false;
            if let Some(stream) = &mut conn.host {
                while !conn.pending_out.is_empty() {
                    match stream.write(&conn.pending_out) {
                        Ok(0) => break,
                        Ok(written) => {
                            conn.pending_out.drain(..written);
                        },
                        Err(ref error)
                            if error.kind() == ::std::io::ErrorKind::WouldBlock =>
                        {
                            break;
                        },
                        Err(_) => {
                            host_write_failed = true;
                            break;
                        },
                    }
                }
                if conn.pending_out.is_empty()
                    && conn.guest_fin_pending
                    && !conn.host_write_shutdown
                {
                    let _ = stream.shutdown(::std::net::Shutdown::Write);
                    conn.host_write_shutdown = true;
                }
            }
            if host_write_failed {
                out.push(build_segment(
                    cfg, conn, conn.snd_nxt, conn.rcv_nxt, RST | ACK, &[],
                ));
                dead.push(*key);
                continue;
            }
            if advertised_window(conn) > window_before {
                out.push(build_segment(
                    cfg, conn, conn.snd_nxt, conn.rcv_nxt, ACK, &[],
                ));
            }

            // Relay host -> guest data, honouring the guest's advertised window.
            if let Some(stream) = &conn.host {
                let mut stream: &TcpStream = stream;
                loop {
                    let in_flight: u32 = conn.snd_nxt.wrapping_sub(conn.snd_una);
                    if in_flight >= conn.guest_window {
                        break; // window full; back off (host TCP backpressures)
                    }
                    let room: usize =
                        (conn.guest_window - in_flight).min(READ_CHUNK as u32) as usize;
                    let mut buf: Vec<u8> = vec![0; room];
                    match stream.read(&mut buf) {
                        Ok(0) => {
                            // Host closed: send our FIN once, then wind the connection down.
                            if !conn.fin_sent {
                                out.push(build_segment(
                                    cfg, conn, conn.snd_nxt, conn.rcv_nxt, FIN | ACK, &[],
                                ));
                                conn.snd_nxt = conn.snd_nxt.wrapping_add(1);
                                conn.fin_sent = true;
                                conn.state = State::Closing;
                            }
                            break;
                        },
                        Ok(n) => {
                            out.push(build_segment(
                                cfg, conn, conn.snd_nxt, conn.rcv_nxt, ACK | PSH, &buf[..n],
                            ));
                            conn.snd_nxt = conn.snd_nxt.wrapping_add(n as u32);
                        },
                        Err(ref e) if e.kind() == ::std::io::ErrorKind::WouldBlock => break,
                        Err(_) => {
                            out.push(build_segment(
                                cfg, conn, conn.snd_nxt, conn.rcv_nxt, RST | ACK, &[],
                            ));
                            dead.push(*key);
                            break;
                        },
                    }
                }
            }

            // Reclaim a fully-closed connection (our FIN acknowledged after the guest's FIN).
            if can_reclaim(conn) {
                dead.push(*key);
            }
            if now.duration_since(conn.last) > CONN_TTL {
                dead.push(*key);
            }
        }

        for key in dead {
            self.conns.remove(&key);
        }
    }
}

/// Builds a guest-bound IPv4/TCP segment (source = the address the guest connected to).
fn build_segment(
    cfg: &NetConfig,
    conn: &Conn,
    seq: u32,
    ack: u32,
    flags: u8,
    payload: &[u8],
) -> Vec<u8> {
    let mut tcp: Vec<u8> = Vec::with_capacity(20 + payload.len());
    tcp.extend_from_slice(&conn.dst_port.to_be_bytes()); // src port = the port the guest dialled
    tcp.extend_from_slice(&conn.guest_port.to_be_bytes());
    tcp.extend_from_slice(&seq.to_be_bytes());
    tcp.extend_from_slice(&ack.to_be_bytes());
    tcp.push(5 << 4); // data offset = 5 words, no options
    tcp.push(flags);
    tcp.extend_from_slice(&advertised_window(conn).to_be_bytes());
    tcp.extend_from_slice(&[0, 0]); // checksum placeholder
    tcp.extend_from_slice(&[0, 0]); // urgent pointer
    tcp.extend_from_slice(payload);
    let csum: u16 = proto::transport_checksum(conn.dst_ip, cfg.guest_ip, IpProto::Tcp, &tcp);
    tcp[16..18].copy_from_slice(&csum.to_be_bytes());
    proto::build_ipv4(conn.dst_ip, cfg.guest_ip, IpProto::Tcp, &tcp)
}

fn advertised_window(conn: &Conn) -> u16 {
    OUR_WINDOW.saturating_sub(conn.pending_out.len().min(usize::from(OUR_WINDOW)) as u16)
}

fn can_reclaim(conn: &Conn) -> bool {
    conn.state == State::Closing
        && conn.fin_sent
        && conn.guest_fin_pending
        && !seq_gt(conn.snd_nxt, conn.snd_una)
}

/// A monotonically-increasing source of initial sequence numbers.
fn next_isn() -> u32 {
    static ISN: AtomicU32 = AtomicU32::new(0x1000_0000);
    ISN.fetch_add(0x0004_0000, Ordering::Relaxed)
}

/// Connects to `dst:port` on a background thread, delivering the stream (or `None` on failure).
fn spawn_connect(dst: Ipv4Addr, port: u16) -> Receiver<Option<TcpStream>> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let stream: Option<TcpStream> =
            TcpStream::connect_timeout(&SocketAddrV4::new(dst, port).into(), Duration::from_secs(5))
                .ok();
        let _ = tx.send(stream);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> Conn {
        Conn {
            dst_ip: Ipv4Addr::LOCALHOST,
            dst_port: 80,
            guest_port: 1000,
            rcv_nxt: 0,
            snd_nxt: 1,
            isn: 0,
            syn_acked: true,
            snd_una: 1,
            guest_window: 1024,
            host: None,
            connecting: None,
            pending_out: Vec::new(),
            guest_fin_pending: false,
            host_write_shutdown: false,
            state: State::Closing,
            fin_sent: true,
            last: Instant::now(),
        }
    }

    fn syn_packet<'a>(tcp: &'a [u8], cfg: &NetConfig) -> Ipv4Packet<'a> {
        Ipv4Packet {
            src: cfg.guest_ip,
            dst: cfg.host_ip,
            proto: IpProto::Tcp,
            payload: tcp,
        }
    }

    #[test]
    fn duplicate_syn_retransmits_the_same_syn_ack() {
        let cfg = NetConfig::parse("10.0.0.2/24").unwrap();
        let mut tcp = [0u8; 20];
        tcp[0..2].copy_from_slice(&49152u16.to_be_bytes());
        tcp[2..4].copy_from_slice(&9u16.to_be_bytes());
        tcp[4..8].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        tcp[12] = 5 << 4;
        tcp[13] = SYN;
        tcp[14..16].copy_from_slice(&4096u16.to_be_bytes());

        let mut nat = TcpNat::new();
        let first = nat.on_guest(&cfg, &syn_packet(&tcp, &cfg));
        let retry = nat.on_guest(&cfg, &syn_packet(&tcp, &cfg));

        assert_eq!(first.len(), 1);
        assert_eq!(retry.len(), 1);
        assert_eq!(&first[0][20..], &retry[0][20..]);
    }

    #[test]
    fn pending_host_output_closes_and_reopens_advertised_window() {
        let mut conn = connection();
        assert_eq!(advertised_window(&conn), OUR_WINDOW);
        conn.pending_out.resize(usize::from(OUR_WINDOW), 0);
        assert_eq!(advertised_window(&conn), 0);
        conn.pending_out.clear();
        assert_eq!(advertised_window(&conn), OUR_WINDOW);
    }

    #[test]
    fn host_fin_waits_for_guest_fin_before_reclaim() {
        let mut conn = connection();
        assert!(!can_reclaim(&conn));
        conn.guest_fin_pending = true;
        assert!(can_reclaim(&conn));
    }
}
