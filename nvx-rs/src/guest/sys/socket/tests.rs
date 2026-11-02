use std::os::fd::AsFd;

use super::*;

#[test]
fn unconnected_datagrams_bind_and_send_to_a_peer() {
    let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let std::net::SocketAddr::V4(peer) = server.local_addr().unwrap() else {
        panic!("an IPv4 server");
    };
    let client = UdpSocket::ipv4().unwrap();
    assert_eq!(client.bind_any(0), Ok(()));
    assert_eq!(client.bind_any(0), Err(Errno::EINVAL));
    assert_eq!(client.send_to(peer, b"announce"), Ok(8));
    let mut buffer = [0; 16];
    let (length, _) = server.recv_from(&mut buffer).unwrap();
    assert_eq!(&buffer[..length], b"announce");
}

#[test]
fn an_inherited_descriptor_is_taken_only_as_an_ipv4_datagram_socket() {
    let duplicate = |descriptor: &dyn AsFd| {
        FileDescriptor::from_owned(descriptor.as_fd().try_clone_to_owned().unwrap())
    };
    // A bound socket keeps its port, and one that is bound to none has port 0.
    let bound = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let port = bound.local_addr().unwrap().port();
    let inherited = UdpSocket::inherited(duplicate(&bound)).unwrap();
    assert_eq!(inherited.port(), Ok(port));
    let unbound = UdpSocket::ipv4().unwrap();
    let inherited = UdpSocket::inherited(duplicate(&unbound.0)).unwrap();
    assert_eq!(inherited.port(), Ok(0));
    // A file, a stream socket, and a datagram socket of IPv6 are refused.
    let file = tempfile::tempfile().unwrap();
    let stream = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let ipv6 = nix::sys::socket::socket(
        AddressFamily::Inet6,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .unwrap();
    for (descriptor, expected) in [
        (&file as &dyn AsFd, Errno::ENOTSOCK),
        (&stream, Errno::EPROTOTYPE),
        (&ipv6, Errno::EAFNOSUPPORT),
    ] {
        assert_eq!(
            UdpSocket::inherited(duplicate(descriptor)).unwrap_err(),
            expected
        );
    }
}

#[test]
fn a_used_port_is_eaddrinuse() {
    let used = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let port = used.local_addr().unwrap().port();
    assert_eq!(
        UdpSocket::ipv4().unwrap().bind_any(port),
        Err(Errno::EADDRINUSE)
    );
}
