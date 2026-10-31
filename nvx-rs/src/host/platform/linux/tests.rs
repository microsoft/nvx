use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::endpoint::UnixTransport;
use super::process::wait_step;
use super::*;
use crate::host::platform::Transport;

#[test]
fn a_wait_sleeps_a_remainder_below_a_millisecond_instead_of_polling_a_millisecond() {
    assert_eq!(wait_step(Duration::ZERO), (Duration::ZERO, Duration::ZERO));
    for remainder in [Duration::from_micros(300), Duration::from_micros(999)] {
        assert_eq!(wait_step(remainder), (Duration::ZERO, remainder));
    }
    assert_eq!(
        wait_step(Duration::from_micros(1_999)),
        (Duration::from_millis(1), Duration::ZERO)
    );
    assert_eq!(
        wait_step(Duration::MAX),
        (Duration::from_millis(u64::MAX), Duration::ZERO)
    );
}

#[test]
fn a_wait_with_less_than_a_millisecond_left_ends_at_its_deadline() {
    let mut child = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let start_time = process_start_time(pid).unwrap().unwrap();
    let process = open_process(pid, start_time).unwrap().unwrap();
    let started = Instant::now();
    let mut waits = Vec::new();
    for _ in 0..20 {
        let deadline = Instant::now() + Duration::from_micros(300);
        waits.push((process.wait(deadline), Instant::now() >= deadline));
    }
    let elapsed = started.elapsed();
    child.kill().unwrap();
    child.wait().unwrap();
    for (wait, ended_after_deadline) in waits {
        assert_eq!(wait.unwrap(), Some(false));
        assert!(ended_after_deadline);
    }
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    assert!(process.wait(Instant::now()).unwrap().unwrap());
}

#[test]
fn a_restore_ready_socket_lets_its_owner_connect_whatever_the_umask() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ready.sock");
    // A umask that clears even the owner's permission to write what it creates, in a thread
    // of the test's own, whose umask no other thread shares.
    let (endpoint, _listener) = thread::scope(|scope| {
        scope
            .spawn(|| {
                nix::sched::unshare(nix::sched::CloneFlags::CLONE_FS).unwrap();
                nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o277));
                listen_ready(&path)
            })
            .join()
            .unwrap()
    })
    .unwrap();
    let mode = fs::symlink_metadata(&path).unwrap().permissions().mode();
    assert_eq!(
        Mode::from_bits_truncate(mode),
        Mode::S_IRUSR | Mode::S_IWUSR,
        "{mode:o}"
    );
    UnixStream::connect(&endpoint).unwrap();
}

#[test]
fn a_restore_ready_client_that_closed_before_its_connection_was_accepted_is_read_to_its_end() {
    let pid = std::process::id();
    let receive_all = |listener: &mut ReadyListener| {
        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while listener
            .receive(pid, &mut received, 64, Duration::from_millis(50))
            .unwrap()
        {
            assert!(Instant::now() < deadline, "the endpoint did not close");
        }
        received
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ready.sock");
    // A client that sent nothing, as a VMM that fails as it starts, and one that sent its
    // event, each closed before the listener accepted their connection: what they sent
    // remains to read, and then the end of the connection.
    for sent in [&b""[..], b"the event\n"] {
        let (endpoint, mut listener) = listen_ready(&path).unwrap();
        let mut client = UnixStream::connect(&endpoint).unwrap();
        std::io::Write::write_all(&mut client, sent).unwrap();
        drop(client);
        assert_eq!(receive_all(&mut listener), sent);
    }
    // A client that closes once the listener accepted its connection.
    let (endpoint, mut listener) = listen_ready(&path).unwrap();
    let client = UnixStream::connect(&endpoint).unwrap();
    let mut received = Vec::new();
    assert!(
        listener
            .receive(pid, &mut received, 64, Duration::from_millis(50))
            .unwrap()
    );
    drop(client);
    assert_eq!(receive_all(&mut listener), b"");
}

#[test]
fn a_unix_transport_reports_what_its_peer_has_not_read() {
    let (stream, mut peer) = UnixStream::pair().unwrap();
    let mut transport = UnixTransport { stream };
    assert_eq!(transport.unread().unwrap(), Some(0));
    let data = b"an attachment";
    transport.write_all(data, None).unwrap();
    // The kernel charges the socket for the write's buffer, more than its bytes, until the
    // peer read it whole: a read of a prefix leaves the charge as it was.
    let queued = transport.unread().unwrap().unwrap();
    assert!(queued >= data.len(), "{queued}");
    let mut received = [0; 6];
    peer.read_exact(&mut received).unwrap();
    assert_eq!(transport.unread().unwrap(), Some(queued));
    let mut rest = vec![0; data.len() - received.len()];
    peer.read_exact(&mut rest).unwrap();
    assert_eq!(transport.unread().unwrap(), Some(0));

    // Writes of a byte each queue a buffer each, which the kernel charges the same for, and
    // which the peer's read of any prefix frees.
    let charges = data
        .chunks(1)
        .map(|byte| {
            transport.write_all(byte, None).unwrap();
            transport.unread().unwrap().unwrap()
        })
        .collect::<Vec<_>>();
    let step = charges[0];
    assert!(step > 0, "{charges:?}");
    assert!(
        charges
            .iter()
            .enumerate()
            .all(|(index, charge)| *charge == (index + 1) * step),
        "{charges:?}"
    );
    peer.read_exact(&mut received[..1]).unwrap();
    let queued = charges[charges.len() - 1];
    assert_eq!(transport.unread().unwrap(), Some(queued - step));
    let mut rest = vec![0; data.len() - 1];
    peer.read_exact(&mut rest).unwrap();
    assert_eq!(transport.unread().unwrap(), Some(0));
}
