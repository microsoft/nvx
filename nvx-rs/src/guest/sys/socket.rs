//! Datagram sockets: connected and unconnected IPv4 UDP sockets.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::{AsRawFd, OwnedFd};

use nix::sys::socket::{
    AddressFamily, MsgFlags, SockFlag, SockType, SockaddrIn, SockaddrLike, SockaddrStorage, sockopt,
};

use super::{Errno, FileDescriptor};

/// Creates a close-on-exec IPv4 UDP socket connected to `peer`.
///
/// Connecting fixes the destination of every `write` and the only source from which `read`
/// receives, as the `socket` and `connect` calls of `nvx-device-io`'s C program did.
pub(crate) fn connect_datagram(peer: SocketAddrV4) -> Result<FileDescriptor, Errno> {
    let UdpSocket(socket) = UdpSocket::ipv4()?;
    nix::sys::socket::connect(socket.as_raw_fd(), &SockaddrIn::from(peer))?;
    Ok(FileDescriptor::from_owned(socket))
}

/// An unconnected, close-on-exec IPv4 UDP socket, from which `nvx-ingress` sends its
/// announcements, and which a clone's application binds for the `nvx-ingress` that it starts.
///
/// Dropping it closes the socket, which the C program left to its exit.
#[derive(Debug)]
pub(crate) struct UdpSocket(OwnedFd);

impl UdpSocket {
    /// Creates the socket.
    pub(crate) fn ipv4() -> Result<Self, Errno> {
        nix::sys::socket::socket(
            AddressFamily::Inet,
            SockType::Datagram,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .map(Self)
    }

    /// Takes over `descriptor`, which this process inherited, such as the duplicate of its
    /// standard input that `take_inherited_descriptor` returns, as an IPv4 datagram socket: a
    /// descriptor that is not a socket is `ENOTSOCK`, a socket of another type `EPROTOTYPE`, and
    /// a datagram socket of another address family `EAFNOSUPPORT`.
    pub(crate) fn inherited(descriptor: FileDescriptor) -> Result<Self, Errno> {
        let socket = OwnedFd::from(descriptor);
        if nix::sys::socket::getsockopt(&socket, sockopt::SockType)? != SockType::Datagram {
            return Err(Errno::EPROTOTYPE);
        }
        let address: SockaddrStorage = nix::sys::socket::getsockname(socket.as_raw_fd())?;
        if address.family() != Some(AddressFamily::Inet) {
            return Err(Errno::EAFNOSUPPORT);
        }
        Ok(Self(socket))
    }

    /// Returns the port to which the socket is bound, or 0 if it is bound to none.
    pub(crate) fn port(&self) -> Result<u16, Errno> {
        nix::sys::socket::getsockname::<SockaddrIn>(self.0.as_raw_fd())
            .map(|address| address.port())
    }

    /// Binds the socket to `port` on every local IPv4 address.
    pub(crate) fn bind_any(&self, port: u16) -> Result<(), Errno> {
        nix::sys::socket::bind(
            self.0.as_raw_fd(),
            &SockaddrIn::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)),
        )
    }

    /// Sends `payload` to `peer` in one datagram, and returns the number of bytes sent.
    pub(crate) fn send_to(&self, peer: SocketAddrV4, payload: &[u8]) -> Result<usize, Errno> {
        nix::sys::socket::sendto(
            self.0.as_raw_fd(),
            payload,
            &SockaddrIn::from(peer),
            MsgFlags::empty(),
        )
    }
}

/// Gives up the socket to its new owner, which `crate::guest::spawn` hands to a child as one of
/// its standard streams.
impl From<UdpSocket> for OwnedFd {
    fn from(socket: UdpSocket) -> Self {
        socket.0
    }
}

#[cfg(test)]
mod tests {
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
}
