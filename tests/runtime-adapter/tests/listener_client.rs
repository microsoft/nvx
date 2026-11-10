//! Real TCP regressions for the hardware scenarios' listener client.
#![cfg(feature = "host")]

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[test]
fn a_published_listener_exchanges_its_greeting() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    let (finished, completion) = mpsc::channel();
    thread::scope(|scope| {
        let stop_ref = &stop;
        scope.spawn(move || {
            if completion.recv_timeout(Duration::from_secs(5)).is_err() {
                stop_ref.store(true, Ordering::SeqCst);
            }
        });
        let server = scope.spawn(|| {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"guest\n").unwrap();
            let mut answer = Vec::new();
            stream.read_to_end(&mut answer).unwrap();
            assert_eq!(answer, b"host\n");
        });
        let reached = common::reach_guest(address, &stop, "guest", "host");
        finished
            .send(())
            .expect("the listener exchange must finish before its deadline");
        assert!(reached);
        server.join().unwrap();
    });
}

#[test]
#[cfg(target_os = "linux")]
fn a_connection_reset_before_inspection_is_retried() {
    use std::net::TcpStream;

    use nix::sys::socket::{setsockopt, sockopt};

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    let (server, _) = listener.accept().unwrap();
    setsockopt(
        &server,
        sockopt::Linger,
        &nix::libc::linger {
            l_onoff: 1,
            l_linger: 0,
        },
    )
    .unwrap();
    drop(server);
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(
        stream.read(&mut [0]).unwrap_err().kind(),
        std::io::ErrorKind::ConnectionReset
    );

    let mut supplied = Some(stream);
    let mut retried = false;
    let stop = AtomicBool::new(false);
    assert!(!common::reach_guest_with(
        address,
        &stop,
        "guest",
        "host",
        |_| {
            if let Some(stream) = supplied.take() {
                Ok(stream)
            } else {
                retried = true;
                stop.store(true, Ordering::SeqCst);
                Err(std::io::ErrorKind::ConnectionRefused.into())
            }
        }
    ));
    assert!(
        retried,
        "a reset connection must retry instead of panicking"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn a_self_connection_does_not_hold_the_publication_port() {
    use std::net::TcpStream;
    use std::os::fd::AsRawFd;

    use nix::sys::socket::{
        AddressFamily, SockFlag, SockProtocol, SockType, SockaddrIn, bind, connect, getsockname,
        setsockopt, socket, sockopt,
    };

    let client = socket(
        AddressFamily::Inet,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        SockProtocol::Tcp,
    )
    .unwrap();
    bind(client.as_raw_fd(), &SockaddrIn::new(127, 0, 0, 1, 0)).unwrap();
    let address: SockaddrIn = getsockname(client.as_raw_fd()).unwrap();
    connect(client.as_raw_fd(), &address).unwrap();
    let stream = TcpStream::from(client);
    assert_eq!(stream.local_addr().unwrap(), stream.peer_addr().unwrap());
    let peer = stream.peer_addr().unwrap();
    let mut supplied = Some(stream);
    let stop = AtomicBool::new(false);
    assert!(!common::reach_guest_with(
        peer,
        &stop,
        "guest",
        "host",
        |_| {
            if let Some(stream) = supplied.take() {
                Ok(stream)
            } else {
                stop.store(true, Ordering::SeqCst);
                Err(std::io::ErrorKind::ConnectionRefused.into())
            }
        }
    ));

    let publisher = socket(
        AddressFamily::Inet,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        SockProtocol::Tcp,
    )
    .unwrap();
    setsockopt(&publisher, sockopt::ReuseAddr, &true).unwrap();
    bind(publisher.as_raw_fd(), &address)
        .expect("the client must not leave the guest's publication port in TIME_WAIT");
}
