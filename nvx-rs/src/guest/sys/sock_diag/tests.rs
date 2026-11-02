//! Checks of the encoded query and the decoder against the kernel's layouts, which the tests
//! spell out byte by byte, and of a live dump.

use super::*;

/// `NLM_F_MULTI`, the flag of each message of a multipart dump, which test dumps carry as the
/// kernel's do, although the decoder ignores it.
const NLM_F_MULTI: u16 = 0x002;

/// Encodes one netlink message of `kind` with `payload`, as the kernel writes a dump's messages.
fn encode_message(kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut message = vec![0_u8; NETLINK_HEADER_LENGTH];
    message[NETLINK_LENGTH_OFFSET..][..size_of::<u32>()]
        .copy_from_slice(&((NETLINK_HEADER_LENGTH + payload.len()) as u32).to_ne_bytes());
    message[NETLINK_TYPE_OFFSET..][..size_of::<u16>()].copy_from_slice(&kind.to_ne_bytes());
    message[NETLINK_FLAGS_OFFSET..][..size_of::<u16>()].copy_from_slice(&NLM_F_MULTI.to_ne_bytes());
    message.extend_from_slice(payload);
    message
}

/// Encodes the dump message of an IPv6 TCP socket in `state` on `address` and `port`, with an
/// `INET_DIAG_SKV6ONLY` attribute of `ipv6_only`, as the kernel writes it.
pub(crate) fn encode_inet_diag_socket(
    state: u8,
    address: Ipv6Addr,
    port: u16,
    ipv6_only: bool,
) -> Vec<u8> {
    let mut payload = vec![0_u8; INET_DIAG_MESSAGE_LENGTH];
    payload[MESSAGE_FAMILY_OFFSET] = AddressFamily::Inet6 as u8;
    payload[MESSAGE_STATE_OFFSET] = state;
    payload[MESSAGE_PORT_OFFSET..][..size_of::<u16>()].copy_from_slice(&port.to_be_bytes());
    payload[MESSAGE_ADDRESS_OFFSET..][..ADDRESS_LENGTH].copy_from_slice(&address.octets());
    let attribute_length = ATTRIBUTE_HEADER_LENGTH + size_of::<u8>();
    payload.extend_from_slice(&(attribute_length as u16).to_ne_bytes());
    payload.extend_from_slice(&INET_DIAG_SKV6ONLY.to_ne_bytes());
    payload.push(u8::from(ipv6_only));
    payload.resize(INET_DIAG_MESSAGE_LENGTH + align(attribute_length), 0);
    encode_message(SOCK_DIAG_BY_FAMILY, &payload)
}

/// Encodes the `NLMSG_DONE` message that ends a dump, whose payload is the dump's 32-bit status.
pub(crate) fn encode_inet_diag_done() -> Vec<u8> {
    encode_message(NLMSG_DONE, &0_i32.to_ne_bytes())
}

/// Linux's `TCP_LISTEN` state number.
const TCP_LISTEN: u8 = 10;

/// Returns the sockets that `dump` describes and whether the dump continues after it.
fn decode(dump: &[u8]) -> (Vec<InetDiagMessage>, bool) {
    let mut messages = Vec::new();
    let continues = decode_inet_diag_dump(dump, |message| messages.push(message));
    (messages, continues)
}

/// Verifies the query's bytes against `linux/netlink.h` and `linux/inet_diag.h`, and the
/// bound of the state mask.
#[test]
fn inet_diag_request_matches_the_linux_layout() {
    let request = inet_diag_request(10).unwrap();
    assert_eq!(request.len(), 72);
    assert_eq!(u32::from_ne_bytes(request[0..4].try_into().unwrap()), 72);
    assert_eq!(u16::from_ne_bytes(request[4..6].try_into().unwrap()), 20);
    assert_eq!(u16::from_ne_bytes(request[6..8].try_into().unwrap()), 0x301);
    // AF_INET6 and IPPROTO_TCP.
    assert_eq!(request[16], 10);
    assert_eq!(request[17], 6);
    assert_eq!(
        u32::from_ne_bytes(request[20..24].try_into().unwrap()),
        1 << 10
    );
    assert!(request[8..16].iter().all(|byte| *byte == 0));
    assert!(request[18..20].iter().all(|byte| *byte == 0));
    assert!(request[24..].iter().all(|byte| *byte == 0));
    assert_eq!(inet_diag_request(31).map(|_| ()), Some(()));
    assert_eq!(inet_diag_request(32), None);
}

/// Verifies the decoding of a message that the test lays out as `linux/inet_diag.h` does,
/// independently of the module's offsets.
#[test]
fn messages_decode_from_the_linux_layout() {
    let mut dump = Vec::new();
    dump.extend_from_slice(&(16_u32 + 72 + 8 + 8).to_ne_bytes());
    dump.extend_from_slice(&20_u16.to_ne_bytes());
    dump.extend_from_slice(&[0; 10]);
    let mut message = [0_u8; 72];
    message[0] = 10;
    message[1] = 10;
    message[4..6].copy_from_slice(&8080_u16.to_be_bytes());
    message[8..24].copy_from_slice(&"fd00::a00:2".parse::<Ipv6Addr>().unwrap().octets());
    dump.extend_from_slice(&message);
    // An unrelated attribute, then `INET_DIAG_SKV6ONLY` with its value and padding.
    dump.extend_from_slice(&8_u16.to_ne_bytes());
    dump.extend_from_slice(&1_u16.to_ne_bytes());
    dump.extend_from_slice(&[7; 4]);
    dump.extend_from_slice(&5_u16.to_ne_bytes());
    dump.extend_from_slice(&11_u16.to_ne_bytes());
    dump.extend_from_slice(&[1, 0, 0, 0]);
    assert_eq!(
        decode(&dump),
        (
            vec![InetDiagMessage {
                ipv6: true,
                state: TCP_LISTEN,
                port: 8080,
                address: "fd00::a00:2".parse().unwrap(),
                ipv6_only: Some(true),
            }],
            true
        )
    );
    message[0] = 2;
    let (messages, _) = decode(&[&dump[..16], &message, &dump[16 + 72..]].concat());
    assert!(!messages[0].ipv6);
}

/// Verifies that `NLMSG_DONE`, `NLMSG_ERROR`, and malformed framing end a dump.
#[test]
fn dumps_end_at_done_error_and_malformed_framing() {
    let socket = encode_inet_diag_socket(TCP_LISTEN, Ipv6Addr::UNSPECIFIED, 7001, true);
    let mut dump = socket.clone();
    dump.extend(encode_inet_diag_done());
    dump.extend(&socket);
    let (messages, continues) = decode(&dump);
    assert_eq!((messages.len(), continues), (1, false));

    let error = encode_message(NLMSG_ERROR, &(-(Errno::EINVAL as i32)).to_ne_bytes());
    let (messages, continues) = decode(&[&socket[..], &error, &socket].concat());
    assert_eq!((messages.len(), continues), (1, false));

    for length in [8_u32, 4096] {
        let mut malformed = socket.clone();
        malformed[..4].copy_from_slice(&length.to_ne_bytes());
        assert_eq!(decode(&malformed), (Vec::new(), false), "{length}");
    }
    assert_eq!(decode(&socket[..40]), (Vec::new(), false));
    assert_eq!(decode(&[0; 37]), (Vec::new(), false));
    let (messages, continues) = decode(&[&socket[..], &socket].concat());
    assert_eq!((messages.len(), continues), (2, true));
}

/// Verifies that short messages are skipped and malformed attributes leave `IPV6_V6ONLY`
/// unknown, while a valueless one is passed over.
#[test]
fn short_messages_and_malformed_attributes_are_contained() {
    let socket = encode_inet_diag_socket(TCP_LISTEN, Ipv6Addr::UNSPECIFIED, 7101, true);
    let attribute = NETLINK_HEADER_LENGTH + INET_DIAG_MESSAGE_LENGTH;
    for length in [2_u16, 200] {
        let mut malformed = socket.clone();
        malformed[attribute..attribute + 2].copy_from_slice(&length.to_ne_bytes());
        let (messages, continues) = decode(&malformed);
        assert_eq!(messages[0].ipv6_only, None, "{length}");
        assert!(continues);
    }

    let short = encode_message(SOCK_DIAG_BY_FAMILY, &[0; 71]);
    assert_eq!(decode(&short), (Vec::new(), true));

    let mut valueless = socket[..attribute].to_vec();
    valueless.extend_from_slice(&4_u16.to_ne_bytes());
    valueless.extend_from_slice(&INET_DIAG_SKV6ONLY.to_ne_bytes());
    valueless.extend_from_slice(&socket[attribute..]);
    let length = valueless.len() as u32;
    valueless[..4].copy_from_slice(&length.to_ne_bytes());
    let (messages, _) = decode(&valueless);
    assert_eq!(messages[0].ipv6_only, Some(true));
}

/// Verifies a live dump against a listener on the unspecified IPv6 address, where the host
/// has IPv6.
#[test]
fn the_kernel_reports_an_ipv6_wildcard_listener() {
    let Ok(listener) = std::net::TcpListener::bind("[::]:0") else {
        return;
    };
    let port = listener.local_addr().unwrap().port();
    let mut messages = Vec::new();
    dump_ipv6_tcp_sockets(u32::from(TCP_LISTEN), Duration::from_secs(5), |message| {
        messages.push(message);
    })
    .unwrap();
    let message = messages
        .iter()
        .find(|message| message.port == port)
        .expect("the dump reports the listener");
    assert!(message.ipv6);
    assert_eq!(message.state, TCP_LISTEN);
    assert!(message.address.is_unspecified());
    assert!(message.ipv6_only.is_some());
}
