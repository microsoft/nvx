//! Reads the kernel's TCP and UDP socket tables, row by row as the C program did, and parses each
//! row strictly, in the form in which the kernel prints it.

use std::io::{BufRead, BufReader, Read};

use super::address::AddressContext;
use super::encode::ListenerTable;
use super::{MAX_TRACKED, PROTOCOL_TCP, TCP_LISTEN, UDP_CONNECTED, UDP_UNCONNECTED, UdpSocket};
use crate::guest::parse;

/// Most bytes of a line that one row takes, as the C program's `fgets` read at most 511 bytes
/// into its `char line[512]`.
///
/// Every row that the kernel writes, at most about 200 bytes long for an IPv6 socket, fits, so
/// the limit only bounds the memory that a malformed snapshot's longer line takes; the rest of
/// that line is read as another row, as C read it.
const ROW_LENGTH_LIMIT: u64 = 511;

/// Text that ends a row's first column, the row's number, which the kernel prints with `"%4d:"`.
const ROW_NUMBER_END: &[u8] = b":";

/// Byte that separates an address from its port in a row's `ADDRESS:PORT` columns, which the
/// kernel prints with `"%08X:%04X"`, or with four address words for IPv6.
const PORT_SEPARATOR: u8 = b':';

/// Most bytes of a row's local or remote `ADDRESS:PORT` column, as the C program's `%63s` read at
/// most 63 bytes of one into its `char local[64]` or `char remote[64]`.
///
/// The kernel's longest such column, an IPv6 address and its port, takes 37 bytes. C read a
/// longer column as two, which shifted the columns after it, so a row with one is rejected rather
/// than read as C never read it.
const ADDRESS_COLUMN_LIMIT: usize = 63;

/// Bytes that the kernel never writes in its socket tables and that C's `sscanf` read differently
/// from the program's split at ASCII white space: a NUL byte, which ended C's string, and a
/// vertical tab, which C, unlike `u8::is_ascii_whitespace`, took for white space. A row that holds
/// one is rejected.
const REJECTED_BYTES: &[u8] = b"\0\x0b";

/// Number of columns between the state and the user ID that the program skips without reading
/// them, as C skipped them with `%*s`: `tx_queue:rx_queue`, `tr:tm->when`, and `retrnsmt`, none
/// of which bears on whether a socket listens.
const SKIPPED_COLUMNS: usize = 3;

/// Number of decimal columns before the inode that the program checks but does not use: `uid`
/// and `timeout`, which C read with `%*u`, so that a row that C rejected stays rejected.
const UNUSED_DECIMAL_COLUMNS: usize = 2;

/// The columns of a socket-table row that the program reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SocketTableRow<'a> {
    /// The local address and port, as the kernel renders them.
    local: &'a [u8],
    /// The remote address and port.
    remote: &'a [u8],
    /// The socket's TCP or UDP state number.
    state: u32,
    /// The socket's inode number.
    inode: u64,
}

/// Parses one row of a socket table, `"%4d: LOCAL REMOTE %02X ... %lu ..."`, whose columns any
/// ASCII white space separates, and returns the columns that the program reads, or `None` if a
/// column is missing or is not in the form in which the kernel prints it.
///
/// A row that holds any of `REJECTED_BYTES`, or an address column longer than
/// `ADDRESS_COLUMN_LIMIT`, whose columns C's `sscanf` read differently, is also rejected, so that
/// C read every row that the program reads alike. Columns after the inode, which kernels have
/// added over time, are ignored, as C ignored them.
fn parse_row(line: &[u8]) -> Option<SocketTableRow<'_>> {
    if line.iter().any(|byte| REJECTED_BYTES.contains(byte)) {
        return None;
    }
    let mut columns = line
        .split(u8::is_ascii_whitespace)
        .filter(|column| !column.is_empty());
    parse::parse_decimal(columns.next()?.strip_suffix(ROW_NUMBER_END)?)?;
    let local = columns
        .next()
        .filter(|column| column.len() <= ADDRESS_COLUMN_LIMIT)?;
    let remote = columns
        .next()
        .filter(|column| column.len() <= ADDRESS_COLUMN_LIMIT)?;
    let state = u32::try_from(parse::parse_hexadecimal(columns.next()?)?).ok()?;
    let mut columns = columns.skip(SKIPPED_COLUMNS);
    for _ in 0..UNUSED_DECIMAL_COLUMNS {
        parse::parse_decimal(columns.next()?)?;
    }
    let inode = parse::parse_decimal(columns.next()?)?;
    Some(SocketTableRow {
        local,
        remote,
        state,
        inode,
    })
}

/// Splits an `ADDRESS:PORT` column at its last separator, as the C program's `strrchr` did, into
/// the address's text and the port's.
fn split_port(column: &[u8]) -> Option<(&[u8], &[u8])> {
    let separator = column.iter().rposition(|byte| *byte == PORT_SEPARATOR)?;
    Some((&column[..separator], &column[separator + 1..]))
}

/// Reads the next row of a table into `line`, at most `ROW_LENGTH_LIMIT` bytes of it, and returns
/// whether it read one: false at the end of the table or after a read error, which also ended the
/// C program's `fgets` loop.
fn read_row(reader: &mut impl BufRead, line: &mut Vec<u8>) -> bool {
    line.clear();
    reader
        .take(ROW_LENGTH_LIMIT)
        .read_until(b'\n', line)
        .is_ok_and(|length| length != 0)
}

/// Scans one kernel table and appends reachable listener or UDP-socket records.
pub(super) fn scan(
    reader: impl Read,
    protocol: u8,
    ipv6: bool,
    own_port: u16,
    addresses: &AddressContext<'_>,
    listeners: &mut ListenerTable,
    udp_sockets: &mut Vec<UdpSocket>,
) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    // The first line is the table's header.
    if !read_row(&mut reader, &mut line) {
        return;
    }
    while read_row(&mut reader, &mut line) {
        let Some(row) = parse_row(&line) else {
            continue;
        };
        let (Some((local_address, local_port)), Some((remote_address, remote_port))) =
            (split_port(row.local), split_port(row.remote))
        else {
            continue;
        };
        let Some(port) = parse::parse_hexadecimal(local_port)
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
        else {
            continue;
        };

        let connected = if protocol == PROTOCOL_TCP {
            if row.state != TCP_LISTEN {
                continue;
            }
            false
        } else if row.state == UDP_CONNECTED {
            true
        } else {
            if row.state != UDP_UNCONNECTED
                || parse::parse_hexadecimal(remote_port) != Some(0)
                || remote_address.iter().any(|byte| *byte != b'0')
                || port == own_port
            {
                continue;
            }
            false
        };
        let families = addresses.reachable_families(local_address, ipv6, protocol, port);
        if families == 0 {
            continue;
        }
        if protocol == PROTOCOL_TCP {
            listeners.add(protocol, port, families);
        } else if udp_sockets.len() < MAX_TRACKED {
            udp_sockets.push(UdpSocket {
                port,
                families,
                connected,
                inode: row.inode,
            });
        }
    }
}

#[cfg(test)]
mod tests;
