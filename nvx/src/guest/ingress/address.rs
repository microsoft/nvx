//! Parses the kernel's hexadecimal socket addresses and classifies their reachability.

use std::net::{Ipv4Addr, Ipv6Addr};

use super::diag::WildcardListener;
use super::{FAMILY_IPV4, FAMILY_IPV6, PROTOCOL_TCP};
use crate::guest::parse;

/// Number of hexadecimal digits in which the kernel's socket tables render each 32-bit word of an
/// address, with `%08X`: an IPv4 address is one word, and an IPv6 address four.
const WORD_DIGITS: usize = 8;

/// Address-family policy used while scanning one set of socket tables.
pub(super) struct AddressContext<'a> {
    /// Whether an unspecified IPv6 UDP socket accepts IPv4 by default.
    pub(super) ipv6_dual_stack: bool,
    /// Per-port IPv6 TCP socket diagnostics.
    pub(super) wildcard_tcp: &'a [WildcardListener],
}

impl AddressContext<'_> {
    /// Returns the families of an unspecified IPv6 socket.
    fn wildcard_families(&self, protocol: u8, port: u16) -> u8 {
        if protocol == PROTOCOL_TCP
            && let Some(listener) = self
                .wildcard_tcp
                .iter()
                .find(|listener| listener.port == port)
        {
            return if listener.dual_stack {
                FAMILY_IPV4 | FAMILY_IPV6
            } else {
                FAMILY_IPV6
            };
        }
        if self.ipv6_dual_stack {
            FAMILY_IPV4 | FAMILY_IPV6
        } else {
            FAMILY_IPV6
        }
    }

    /// Returns the families through which the kernel-rendered address is reachable.
    ///
    /// Loopback addresses, IPv6 link-local ones, and IPv4 loopback addresses mapped into IPv6 are
    /// unreachable through the gateway, as the C program's `IN6_IS_ADDR_*` checks found them.
    pub(super) fn reachable_families(
        &self,
        address: &[u8],
        ipv6: bool,
        protocol: u8,
        port: u16,
    ) -> u8 {
        if !ipv6 {
            return match decode_kernel_address::<4>(address).map(Ipv4Addr::from) {
                Some(address) if !address.is_loopback() => FAMILY_IPV4,
                _ => 0,
            };
        }

        let Some(address) = decode_kernel_address::<16>(address).map(Ipv6Addr::from) else {
            return 0;
        };
        if address.is_unspecified() {
            return self.wildcard_families(protocol, port);
        }
        if address.is_loopback() || address.is_unicast_link_local() {
            return 0;
        }
        match address.to_ipv4_mapped() {
            Some(mapped) if mapped.is_loopback() => 0,
            Some(_) => FAMILY_IPV4,
            None => FAMILY_IPV6,
        }
    }
}

/// Decodes the kernel's rendering of an address of `N` bytes, each 32-bit word in `WORD_DIGITS`
/// hexadecimal digits of its value as it lies in memory, into the address's bytes, or returns
/// `None` for any other text.
fn decode_kernel_address<const N: usize>(text: &[u8]) -> Option<[u8; N]> {
    if text.len() != N / size_of::<u32>() * WORD_DIGITS {
        return None;
    }
    let mut bytes = [0_u8; N];
    for (digits, word) in text
        .chunks_exact(WORD_DIGITS)
        .zip(bytes.chunks_exact_mut(size_of::<u32>()))
    {
        let value = u32::try_from(parse::parse_hexadecimal(digits)?).ok()?;
        word.copy_from_slice(&value.to_ne_bytes());
    }
    Some(bytes)
}

#[cfg(test)]
mod tests;
