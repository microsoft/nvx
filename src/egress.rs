// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! IPv4/CIDR egress policy shared by the host network backends.

use ::std::net::Ipv4Addr;

use ::anyhow::{Result, bail};

const ETH_IPV4: u16 = 0x0800;
const ETH_ARP: u16 = 0x0806;
const ETH_VLAN: u16 = 0x8100;
const ETH_QINQ: u16 = 0x88a8;
const ETH_VLAN_9100: u16 = 0x9100;

/// One normalized IPv4 CIDR range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ipv4Range {
    network: u32,
    mask: u32,
}

impl Ipv4Range {
    fn parse(value: &str) -> Result<Self> {
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) => {
                let prefix: u8 = prefix
                    .parse()
                    .map_err(|_| ::anyhow::anyhow!("invalid IPv4 prefix in {value:?}"))?;
                (address, prefix)
            }
            None => (value, 32),
        };
        if prefix > 32 {
            bail!("IPv4 prefix in {value:?} must be between 0 and 32");
        }
        let address: Ipv4Addr = address
            .parse()
            .map_err(|_| ::anyhow::anyhow!("invalid IPv4 address or CIDR {value:?}"))?;
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        Ok(Self {
            network: u32::from(address) & mask,
            mask,
        })
    }

    fn contains(&self, address: Ipv4Addr) -> bool {
        u32::from(address) & self.mask == self.network
    }
}

/// Host-side egress decision applied before a backend sends guest traffic to the host network.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum EgressFilter {
    /// Permit every destination.
    #[default]
    AllowAll,
    /// Permit only listed IPv4 ranges.
    Allow(Vec<Ipv4Range>),
    /// Permit every IPv4 destination except listed ranges.
    Block(Vec<Ipv4Range>),
}

impl EgressFilter {
    /// Parses mutually exclusive allow/block lists supplied by the host orchestrator.
    pub(crate) fn parse(allow: &[String], block: &[String]) -> Result<Self> {
        if !allow.is_empty() && !block.is_empty() {
            bail!("--allow-host and --block-host are mutually exclusive");
        }
        let parse = |values: &[String]| -> Result<Vec<Ipv4Range>> {
            values.iter().map(|value| Ipv4Range::parse(value)).collect()
        };
        if !allow.is_empty() {
            return Ok(Self::Allow(parse(allow)?));
        }
        if !block.is_empty() {
            return Ok(Self::Block(parse(block)?));
        }
        Ok(Self::AllowAll)
    }

    /// Returns whether an IPv4 `address` may leave the guest.
    pub(crate) fn allows(&self, address: Ipv4Addr) -> bool {
        match self {
            Self::AllowAll => true,
            Self::Allow(ranges) => ranges.iter().any(|range| range.contains(address)),
            Self::Block(ranges) => !ranges.iter().any(|range| range.contains(address)),
        }
    }

    /// Returns whether a raw Ethernet frame may leave the guest.
    ///
    /// ARP remains available so the guest can resolve its gateway. An allow-list rejects
    /// non-IPv4 traffic because it cannot be represented by an IPv4 CIDR; a block-list only
    /// rejects matching IPv4 destinations. Malformed IPv4 and VLAN frames fail closed whenever a
    /// policy is active.
    pub(crate) fn allows_ethernet_frame(&self, frame: &[u8]) -> bool {
        if matches!(self, Self::AllowAll) {
            return true;
        }
        let Some((ether_type, payload_offset)) = ethernet_payload(frame) else {
            return false;
        };
        match ether_type {
            ETH_ARP => true,
            ETH_IPV4 => {
                let Some(header) = frame.get(payload_offset..payload_offset + 20) else {
                    return false;
                };
                if header[0] >> 4 != 4 {
                    return false;
                }
                self.allows(Ipv4Addr::new(
                    header[16], header[17], header[18], header[19],
                ))
            }
            _ => matches!(self, Self::Block(_)),
        }
    }
}

/// Returns the innermost EtherType and payload offset, unwrapping stacked VLAN headers.
fn ethernet_payload(frame: &[u8]) -> Option<(u16, usize)> {
    let ethernet = frame.get(..14)?;
    let mut ether_type = u16::from_be_bytes([ethernet[12], ethernet[13]]);
    let mut payload_offset = 14;
    while matches!(ether_type, ETH_VLAN | ETH_QINQ | ETH_VLAN_9100) {
        let vlan = frame.get(payload_offset..payload_offset + 4)?;
        ether_type = u16::from_be_bytes([vlan[2], vlan[3]]);
        payload_offset += 4;
    }
    Some((ether_type, payload_offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn ipv4_frame(destination: Ipv4Addr) -> Vec<u8> {
        let mut frame = vec![0_u8; 34];
        frame[12..14].copy_from_slice(&ETH_IPV4.to_be_bytes());
        frame[14] = 0x45;
        frame[30..34].copy_from_slice(&destination.octets());
        frame
    }

    #[test]
    fn parses_allow_and_block_cidrs() {
        let allow = EgressFilter::parse(&strings(&["10.0.0.0/8"]), &[]).unwrap();
        assert!(allow.allows("10.2.3.4".parse().unwrap()));
        assert!(!allow.allows("192.168.1.1".parse().unwrap()));

        let block = EgressFilter::parse(&[], &strings(&["192.168.0.0/16"])).unwrap();
        assert!(!block.allows("192.168.1.1".parse().unwrap()));
        assert!(block.allows("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn rejects_ambiguous_or_invalid_filters() {
        assert!(EgressFilter::parse(&strings(&["1.1.1.1"]), &strings(&["2.2.2.2"])).is_err());
        assert!(EgressFilter::parse(&strings(&["not-an-ip"]), &[]).is_err());
        assert!(EgressFilter::parse(&strings(&["10.0.0.0/33"]), &[]).is_err());
    }

    #[test]
    fn allow_list_filters_raw_ipv4_but_preserves_arp() {
        let filter = EgressFilter::parse(&strings(&["10.0.0.0/8"]), &[]).unwrap();
        assert!(filter.allows_ethernet_frame(&ipv4_frame("10.2.3.4".parse().unwrap())));
        assert!(!filter.allows_ethernet_frame(&ipv4_frame("192.168.1.1".parse().unwrap())));

        let mut arp = vec![0_u8; 42];
        arp[12..14].copy_from_slice(&ETH_ARP.to_be_bytes());
        assert!(filter.allows_ethernet_frame(&arp));

        let mut ipv6 = vec![0_u8; 54];
        ipv6[12..14].copy_from_slice(&0x86dd_u16.to_be_bytes());
        assert!(!filter.allows_ethernet_frame(&ipv6));
    }

    #[test]
    fn raw_filter_unwraps_vlan_headers() {
        let filter = EgressFilter::parse(&strings(&["10.0.0.0/8"]), &[]).unwrap();
        let mut frame = ipv4_frame("10.2.3.4".parse().unwrap());
        frame[12..14].copy_from_slice(&ETH_QINQ.to_be_bytes());
        frame.splice(14..14, [0, 1, 0x81, 0x00, 0, 2, 0x08, 0x00]);
        assert!(filter.allows_ethernet_frame(&frame));

        frame[38..42].copy_from_slice(&Ipv4Addr::new(192, 168, 1, 1).octets());
        assert!(!filter.allows_ethernet_frame(&frame));
    }

    #[test]
    fn block_list_filters_ipv4_and_fails_closed_on_malformed_frames() {
        let filter = EgressFilter::parse(&[], &strings(&["192.168.0.0/16"])).unwrap();
        assert!(!filter.allows_ethernet_frame(&ipv4_frame("192.168.1.1".parse().unwrap())));
        assert!(filter.allows_ethernet_frame(&ipv4_frame("1.1.1.1".parse().unwrap())));

        let mut ipv6 = vec![0_u8; 54];
        ipv6[12..14].copy_from_slice(&0x86dd_u16.to_be_bytes());
        assert!(filter.allows_ethernet_frame(&ipv6));

        let mut malformed_ipv4 = vec![0_u8; 14];
        malformed_ipv4[12..14].copy_from_slice(&ETH_IPV4.to_be_bytes());
        assert!(!filter.allows_ethernet_frame(&malformed_ipv4));
    }
}
