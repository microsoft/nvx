//! Parser tests for kernel-format rows, bounded lines, and UDP classification.

use super::*;
use crate::guest::ingress::{FAMILY_IPV4, PROTOCOL_UDP, UDP_CONNECTED, UDP_UNCONNECTED};

/// Minimal socket-table header skipped by every scan.
const HEADER: &[u8] = b"header\n";

/// Returns a dual-stack context with no diagnostic overrides.
fn context() -> AddressContext<'static> {
    AddressContext {
        ipv6_dual_stack: true,
        wildcard_tcp: &[],
    }
}

/// Scans test rows and returns both output collections.
fn scan_rows(
    rows: &[u8],
    protocol: u8,
    ipv6: bool,
    own_port: u16,
) -> (ListenerTable, Vec<UdpSocket>) {
    let mut input = HEADER.to_vec();
    input.extend_from_slice(rows);
    let mut listeners = ListenerTable::default();
    let mut udp = Vec::new();
    scan(
        input.as_slice(),
        protocol,
        ipv6,
        own_port,
        &context(),
        &mut listeners,
        &mut udp,
    );
    (listeners, udp)
}

/// Formats one socket-table row with the fields consumed by ingress.
fn row(local: &str, remote: &str, state: u32, inode: &str) -> Vec<u8> {
    format!(
        "  7: {local} {remote} {state:02X} 00000000:00000000 00:00000000 \
             00000000 0 0 {inode} trailing\n"
    )
    .into_bytes()
}

/// Verifies that any ASCII white space separates columns and that later columns are ignored.
#[test]
fn parses_any_spacing_and_ignores_trailing_columns() {
    let mut input = b"\t7:  00000000:1F90\t00000000:0000 0a a b c 0 0 42 suffix\r\n".to_vec();
    input.extend_from_slice(b" 8: 00000000:1F91 00000000:0000 0A a b c 0 0 43\n");
    let (listeners, udp) = scan_rows(&input, PROTOCOL_TCP, false, 1023);
    assert!(udp.is_empty());
    assert_eq!(
        super::super::encode::encode(listeners),
        b"NVXL\x01\x00\x00\x02\x06\x01\x1f\x90\x06\x01\x1f\x91"
    );
}

/// Verifies rejection of malformed addresses, states, and port bounds.
#[test]
fn rejects_bad_states_zero_and_extreme_ports_and_addresses() {
    let mut input = Vec::new();
    input.extend(row("00000000:0000", "00000000:0000", TCP_LISTEN, "1"));
    input.extend(row("00000000:10000", "00000000:0000", TCP_LISTEN, "2"));
    input.extend(row("0000000G:1F90", "00000000:0000", TCP_LISTEN, "3"));
    input.extend(row("00000000:1F91", "00000000:0000", 1, "4"));
    let (listeners, udp) = scan_rows(&input, PROTOCOL_TCP, false, 1023);
    assert!(udp.is_empty());
    assert_eq!(
        super::super::encode::encode(listeners),
        b"NVXL\x01\x00\x00\x00"
    );
}

/// Verifies unconnected peer requirements, own-port filtering, and connected retention.
#[test]
fn udp_requires_an_unconnected_zero_peer_but_keeps_connected_rows() {
    let mut input = Vec::new();
    input.extend(row("00000000:1C21", "00000000:0000", UDP_UNCONNECTED, "10"));
    input.extend(row("00000000:1C22", "0100007F:0035", UDP_CONNECTED, "11"));
    input.extend(row("00000000:1C23", "00000000:0001", UDP_UNCONNECTED, "12"));
    input.extend(row("00000000:03FF", "00000000:0000", UDP_UNCONNECTED, "13"));
    let (_, udp) = scan_rows(&input, PROTOCOL_UDP, false, 1023);
    assert_eq!(
        udp,
        vec![
            UdpSocket {
                port: 7201,
                families: FAMILY_IPV4,
                connected: false,
                inode: 10,
            },
            UdpSocket {
                port: 7202,
                families: FAMILY_IPV4,
                connected: true,
                inode: 11,
            },
        ]
    );
}

/// Verifies that physical lines longer than the C buffer become separate parse chunks.
#[test]
fn an_overlong_physical_line_is_split_like_fgets() {
    let mut input = vec![b'x'; 511];
    input.push(b'\n');
    input.extend_from_slice(&row("00000000:1F90", "00000000:0000", TCP_LISTEN, "1"));
    let (listeners, _) = scan_rows(&input, PROTOCOL_TCP, false, 1023);
    assert_eq!(
        super::super::encode::encode(listeners),
        b"NVXL\x01\x00\x00\x01\x06\x01\x1f\x90"
    );
}

/// Verifies that an internal buffered-reader refill never truncates a short physical line.
#[test]
fn buffered_reader_boundaries_do_not_split_short_lines() {
    let mut input = Vec::new();
    for index in 0..100 {
        input.extend(row(
            &format!("00000000:{:04X}", 20_000 + index),
            "00000000:0000",
            UDP_UNCONNECTED,
            &index.to_string(),
        ));
    }
    assert!(input.len() > 8 * 1024);
    let (_, udp) = scan_rows(&input, PROTOCOL_UDP, false, 1023);
    assert_eq!(udp.len(), 100);
}

/// Verifies a kernel row, and that address columns are read whole up to the 63 bytes that C's
/// `%63s` read.
#[test]
fn rows_parse_the_kernel_format() {
    let row = parse_row(
        b"   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0 \
              0 12345 1 0000000000000000 100 0 0 10 0\n",
    )
    .unwrap();
    assert_eq!(
        row,
        SocketTableRow {
            local: b"00000000:1F90",
            remote: b"00000000:0000",
            state: 0x0a,
            inode: 12345,
        }
    );
    let row = parse_row(b"0: a b 0 c d e 0 0 7").unwrap();
    assert_eq!((row.state, row.inode), (0, 7));
    let column = [b'a'; ADDRESS_COLUMN_LIMIT];
    let mut line = b"0: ".to_vec();
    line.extend(column);
    line.push(b' ');
    line.extend(column);
    line.extend(b" 0A c d e 0 0 1");
    let row = parse_row(&line).unwrap();
    assert_eq!((row.local, row.remote), (&column[..], &column[..]));
}

/// Verifies that numbers must fit their types rather than wrap or saturate as C stored them.
#[test]
fn row_numbers_must_fit_their_types() {
    assert_eq!(
        parse_row(b"0: a b A c d e 0 0 18446744073709551615").map(|row| row.inode),
        Some(u64::MAX)
    );
    for line in [
        &b"0: a b 1000000000A c d e 0 0 1"[..],
        b"0: a b 10000000000000000A c d e 0 0 1",
        b"0: a b A c d e 0 0 18446744073709551616",
    ] {
        assert_eq!(parse_row(line), None, "{}", line.escape_ascii());
    }
}

/// Verifies that rows that C's `sscanf` did not match, and the lenient forms that it did
/// but the kernel never writes, are rejected.
#[test]
fn rows_reject_malformed_and_c_lenient_forms() {
    for line in [
        &b""[..],
        b"\n",
        b"0 : a b 0A c d e 0 0 1",
        b"0: a b 0x c d e 0 0 1",
        b"0: a b 0xg c d e 0 0 1",
        b"0: a b - c d e 0 0 1",
        b"0: a b g c d e 0 0 1",
        b"x: a b 0A c d e 0 0 1",
        b"0: a b 0A c d e 0 x 1",
        b"0: a b 0A c d e 0 0",
        b"0: a b 0A c d e 0 0 +",
        b"+7: a b 0A c d e 0 0 1",
        b"-1: a b 0A c d e 0 0 1",
        b"0:\x0ba b 0A c d e 0 0 1",
        b"0: a b 0x0a c d e 0 0 1",
        b"0: a b +A c d e 0 0 1",
        b"0: a b -0x0a c d e 0 0 1",
        b"0: a b 0A c d e +0 -0 1",
        b"0: a b 0A c d e 0 0 -1",
        b"0: a b 0A c d e 0 0 0x1",
        b"0: a b 0A c d e 0 0 1x",
    ] {
        assert_eq!(parse_row(line), None, "{}", line.escape_ascii());
    }
}

/// Verifies that rows whose columns C's `sscanf` read differently, which the kernel never
/// writes, are rejected: an address column longer than C's `%63s`, which C split, and a NUL
/// byte, which ended C's string, or a vertical tab, which C took for white space, anywhere in
/// the row, even in a column that the program skips.
#[test]
fn rows_reject_columns_that_c_read_differently() {
    let column = [b'a'; ADDRESS_COLUMN_LIMIT + 1];
    let mut long_local = b"0: ".to_vec();
    long_local.extend(column);
    long_local.extend(b" b 0A c d e 0 0 1");
    let mut long_remote = b"0: a ".to_vec();
    long_remote.extend(column);
    long_remote.extend(b" 0A c d e 0 0 1");
    for line in [
        long_local.as_slice(),
        long_remote.as_slice(),
        b"0: a b 0A \0 d e 0 0 1",
        b"0: a b 0A c\x0bd e f 0 0 1",
        b"0: a b 0A c d e 0 0 1 \0",
        b"0: a b 0A c d e 0 0 1 \x0b",
    ] {
        assert_eq!(parse_row(line), None, "{}", line.escape_ascii());
    }
}

/// Verifies that rows that C split or cut short, and so skipped, announce nothing, though the
/// program's split at ASCII white space alone would find a listener or UDP socket in each.
#[test]
fn rows_that_c_skipped_announce_nothing() {
    // A zero-padded port makes the local column 70 bytes long.
    let mut input = row(
        &format!("00000000:{:0>61}", "1F90"),
        "00000000:0000",
        TCP_LISTEN,
        "1",
    );
    input.extend_from_slice(b"  8: 00000000:1F91 00000000:0000 0A \0 b c 0 0 2\n");
    input.extend_from_slice(b"  9: 00000000:1F92 00000000:0000 0A a\x0bb c d 0 0 3\n");
    let (listeners, _) = scan_rows(&input, PROTOCOL_TCP, false, 1023);
    assert_eq!(
        super::super::encode::encode(listeners),
        b"NVXL\x01\x00\x00\x00"
    );
    let input = row(
        "00000000:1F90",
        &format!("{:0>64}:0000", ""),
        UDP_UNCONNECTED,
        "1",
    );
    let (_, udp) = scan_rows(&input, PROTOCOL_UDP, false, 1023);
    assert!(udp.is_empty());
}

/// Verifies that addresses and ports in forms that C's `strtoul` read but the kernel never
/// writes announce nothing, as TCP listeners or UDP sockets, while their canonical form does.
#[test]
fn c_lenient_addresses_and_ports_announce_nothing() {
    let canonical = row("00000000:1F90", "00000000:0000", TCP_LISTEN, "1");
    let (listeners, _) = scan_rows(&canonical, PROTOCOL_TCP, false, 1023);
    assert_eq!(
        super::super::encode::encode(listeners),
        b"NVXL\x01\x00\x00\x01\x06\x01\x1f\x90"
    );
    let canonical = row("00000000:1F90", "00000000:0000", UDP_UNCONNECTED, "1");
    let (_, udp) = scan_rows(&canonical, PROTOCOL_UDP, false, 1023);
    assert_eq!(udp.len(), 1);
    for local in [
        "-0000001:1F90",
        "+0000001:1F90",
        "0000000\0:1F90",
        "00000000:+1F90",
        "00000000:0x1F90",
        "00000000:1F90x",
    ] {
        let input = row(local, "00000000:0000", TCP_LISTEN, "1");
        let (listeners, _) = scan_rows(&input, PROTOCOL_TCP, false, 1023);
        assert_eq!(
            super::super::encode::encode(listeners),
            b"NVXL\x01\x00\x00\x00",
            "{}",
            local.escape_debug()
        );
        let input = row(local, "00000000:0000", UDP_UNCONNECTED, "1");
        let (_, udp) = scan_rows(&input, PROTOCOL_UDP, false, 1023);
        assert!(udp.is_empty(), "{}", local.escape_debug());
    }
    // Only an unconnected UDP socket's remote port must be zero.
    for remote in ["00000000:+0", "00000000:0x0", "00000000:x"] {
        let input = row("00000000:1F90", remote, UDP_UNCONNECTED, "1");
        let (_, udp) = scan_rows(&input, PROTOCOL_UDP, false, 1023);
        assert!(udp.is_empty(), "{remote}");
    }
}
