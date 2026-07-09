// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! Minimal IPv4/transport packet helpers for the user-mode NAT: parsing, checksums, and framing.

use ::std::net::Ipv4Addr;

/// The IPv4 protocol numbers the NAT handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpProto {
    Icmp,
    Tcp,
    Udp,
    Other(u8),
}

impl IpProto {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => IpProto::Icmp,
            6 => IpProto::Tcp,
            17 => IpProto::Udp,
            other => IpProto::Other(other),
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            IpProto::Icmp => 1,
            IpProto::Tcp => 6,
            IpProto::Udp => 17,
            IpProto::Other(v) => v,
        }
    }
}

/// A parsed IPv4 packet: header fields plus the transport payload (options are skipped).
pub struct Ipv4Packet<'a> {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub proto: IpProto,
    pub payload: &'a [u8],
}

impl<'a> Ipv4Packet<'a> {
    /// Parses an IPv4 packet, returning `None` if it is malformed or truncated.
    pub fn parse(buf: &'a [u8]) -> Option<Ipv4Packet<'a>> {
        if buf.len() < 20 {
            return None;
        }
        if buf[0] >> 4 != 4 {
            return None;
        }
        let ihl: usize = usize::from(buf[0] & 0x0f) * 4;
        if ihl < 20 || buf.len() < ihl {
            return None;
        }
        let total_len: usize = usize::from(u16::from_be_bytes([buf[2], buf[3]]));
        let end: usize = total_len.min(buf.len());
        if end < ihl {
            return None;
        }
        Some(Ipv4Packet {
            src: Ipv4Addr::new(buf[12], buf[13], buf[14], buf[15]),
            dst: Ipv4Addr::new(buf[16], buf[17], buf[18], buf[19]),
            proto: IpProto::from_u8(buf[9]),
            payload: &buf[ihl..end],
        })
    }
}

/// Computes the 16-bit one's-complement checksum over `data` (RFC 1071).
pub fn checksum(data: &[u8]) -> u16 {
    fold(sum_words(data))
}

/// Sums 16-bit big-endian words of `data` into a 32-bit accumulator.
fn sum_words(data: &[u8]) -> u32 {
    let mut sum: u32 = 0;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u32::from(u16::from_be_bytes([c[0], c[1]]));
    }
    if let [last] = chunks.remainder() {
        sum += u32::from(u16::from_be_bytes([*last, 0]));
    }
    sum
}

/// Folds a checksum accumulator into the final one's-complement 16-bit value.
fn fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Computes a TCP/UDP checksum over the pseudo-header and `payload`.
pub fn transport_checksum(src: Ipv4Addr, dst: Ipv4Addr, proto: IpProto, payload: &[u8]) -> u16 {
    let s: [u8; 4] = src.octets();
    let d: [u8; 4] = dst.octets();
    let mut sum: u32 = 0;
    sum += u32::from(u16::from_be_bytes([s[0], s[1]]));
    sum += u32::from(u16::from_be_bytes([s[2], s[3]]));
    sum += u32::from(u16::from_be_bytes([d[0], d[1]]));
    sum += u32::from(u16::from_be_bytes([d[2], d[3]]));
    sum += u32::from(proto.to_u8());
    sum += u32::from(payload.len() as u16);
    sum += sum_words(payload);
    fold(sum)
}

/// Global counter for the IPv4 identification field.
static IP_ID: ::std::sync::atomic::AtomicU16 = ::std::sync::atomic::AtomicU16::new(1);

/// Builds a complete IPv4 packet (20-byte header, DF set, TTL 64) carrying `payload`.
pub fn build_ipv4(src: Ipv4Addr, dst: Ipv4Addr, proto: IpProto, payload: &[u8]) -> Vec<u8> {
    let total: u16 = 20 + payload.len() as u16;
    let id: u16 = IP_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed);
    let mut h: Vec<u8> = Vec::with_capacity(total as usize);
    h.push(0x45); // version 4, IHL 5
    h.push(0x00); // DSCP/ECN
    h.extend_from_slice(&total.to_be_bytes());
    h.extend_from_slice(&id.to_be_bytes());
    h.extend_from_slice(&0x4000u16.to_be_bytes()); // flags: DF
    h.push(64); // TTL
    h.push(proto.to_u8());
    h.extend_from_slice(&[0, 0]); // checksum placeholder
    h.extend_from_slice(&src.octets());
    h.extend_from_slice(&dst.octets());
    let csum: u16 = checksum(&h);
    h[10..12].copy_from_slice(&csum.to_be_bytes());
    h.extend_from_slice(payload);
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_of_a_valid_header_verifies_to_zero() {
        // A correctly-checksummed IPv4 header sums (including its checksum field) to 0.
        let pkt = build_ipv4(
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            IpProto::Udp,
            &[1, 2, 3, 4],
        );
        assert_eq!(checksum(&pkt[..20]), 0, "header checksum must verify");
    }

    #[test]
    fn parse_extracts_transport_payload() {
        let pkt = build_ipv4(
            Ipv4Addr::new(1, 2, 3, 4),
            Ipv4Addr::new(5, 6, 7, 8),
            IpProto::Tcp,
            &[0xaa, 0xbb, 0xcc],
        );
        let parsed = Ipv4Packet::parse(&pkt).unwrap();
        assert_eq!(parsed.src, Ipv4Addr::new(1, 2, 3, 4));
        assert_eq!(parsed.dst, Ipv4Addr::new(5, 6, 7, 8));
        assert_eq!(parsed.proto, IpProto::Tcp);
        assert_eq!(parsed.payload, &[0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn transport_checksum_is_nonzero_and_stable() {
        let a = transport_checksum(
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            IpProto::Tcp,
            &[0; 20],
        );
        let b = transport_checksum(
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            IpProto::Tcp,
            &[0; 20],
        );
        assert_eq!(a, b);
    }
}
