use std::collections::VecDeque;
use std::fs;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use std::{panic, thread};

use nvx::host::platform::{self, Transport};
use nvx::host::process::{BootId, ProcessRef};

use super::{CHUNK, connect, open_log, read_marker, read_until};

/// A console that returns what a test scripted, in order: bytes, the end of the console, or an
/// error, and then waits for more for as long as each read allows, with nothing. A read returns
/// as much of the next bytes as its buffer holds, and leaves the rest to the next.
struct Scripted(VecDeque<io::Result<Vec<u8>>>);

impl Scripted {
    /// Returns a console that returns `chunks`, each as one read, then waits.
    fn of(chunks: &[&[u8]]) -> Self {
        Self(chunks.iter().map(|chunk| Ok(chunk.to_vec())).collect())
    }

    /// Returns the console, which then ends.
    fn ending(mut self) -> Self {
        self.0.push_back(Ok(Vec::new()));
        self
    }
}

/// Copies as much of `chunk` as `buffer` holds to it, and returns how many bytes it copied, and
/// the rest of `chunk`, if any.
fn deliver(buffer: &mut [u8], mut chunk: Vec<u8>) -> (usize, Option<Vec<u8>>) {
    let length = chunk.len().min(buffer.len());
    buffer[..length].copy_from_slice(&chunk[..length]);
    let rest = chunk.split_off(length);
    (length, (!rest.is_empty()).then_some(rest))
}

impl Transport for Scripted {
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        match self.0.pop_front() {
            Some(Ok(chunk)) => {
                let (length, rest) = deliver(buffer, chunk);
                if let Some(rest) = rest {
                    self.0.push_front(Ok(rest));
                }
                Ok(length)
            }
            Some(Err(error)) => Err(error),
            None => {
                std::thread::sleep(
                    timeout
                        .unwrap_or(Duration::ZERO)
                        .min(Duration::from_millis(5)),
                );
                Err(io::ErrorKind::TimedOut.into())
            }
        }
    }

    fn write_all(&mut self, _: &[u8], _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
}

/// A console that delivers its chunks in order, as [`Scripted`] does, and counts the reads that
/// delivered bytes, then waits for more for as long as each read allows, with nothing.
struct Counted {
    /// The chunks that the console still delivers.
    chunks: VecDeque<Vec<u8>>,
    /// The reads that delivered bytes.
    reads: Arc<AtomicUsize>,
}

impl Transport for Counted {
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        match self.chunks.pop_front() {
            Some(chunk) => {
                let (length, rest) = deliver(buffer, chunk);
                if let Some(rest) = rest {
                    self.chunks.push_front(rest);
                }
                self.reads.fetch_add(1, Ordering::SeqCst);
                Ok(length)
            }
            None => {
                thread::sleep(
                    timeout
                        .unwrap_or(Duration::ZERO)
                        .min(Duration::from_millis(5)),
                );
                Err(io::ErrorKind::TimedOut.into())
            }
        }
    }

    fn write_all(&mut self, _: &[u8], _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
}

/// A deadline far enough away that no test reaches it.
fn later() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

#[test]
fn a_reader_refuses_a_vmm_of_another_boot() {
    // The test's process stands for a VMM of an earlier boot that had its ID and start time, as a
    // process of this boot may after a restart on Linux, where the start time counts from the
    // boot. On Linux, it serves the endpoint, so that only the boot tells them apart.
    let pid = std::process::id();
    let start_time = platform::process_start_time(pid).unwrap().unwrap();
    let current = ProcessRef {
        pid,
        start_time,
        boot: BootId::current().unwrap(),
    };
    let mut boot = *current.boot.as_bytes();
    boot[0] ^= 0xff;
    let stale = ProcessRef {
        boot: BootId::from_bytes(boot),
        ..current.clone()
    };
    let directory = std::env::temp_dir().join(format!(
        "runtime-adapter-console-boot-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    #[cfg(unix)]
    let (endpoint, _served) = {
        let socket = directory.join("console.sock");
        let served = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        (socket.display().to_string(), served)
    };
    #[cfg(windows)]
    let endpoint = format!("//./pipe/runtime-adapter-console-boot-{pid}");
    let deadline = Instant::now() + Duration::from_secs(10);
    match connect(&endpoint, &stale, deadline) {
        Err(error) => assert_eq!(error.kind(), io::ErrorKind::NotConnected, "{error}"),
        Ok(_) => panic!("a reader connected to the console of a VMM of another boot"),
    }
    assert!(Instant::now() < deadline);
    // The reference of this boot connects to the endpoint that its process serves.
    #[cfg(unix)]
    assert!(connect(&endpoint, &current, deadline).is_ok());
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn a_reader_reads_until_what_it_read_holds_the_marker() {
    let mut console = Scripted::of(&[b"[    1.0] boot\n", b"[    2.0] mar", b"ker\n", b"after"]);
    let mut read = Vec::new();
    read_until(&mut console, b"marker", later(), &mut read).unwrap();
    // The marker may span reads, and the reader reads nothing past the read that completed it.
    assert_eq!(read, b"[    1.0] boot\n[    2.0] marker\n");
    assert_eq!(console.0.len(), 1);
}

#[test]
fn a_reader_waits_for_the_marker_through_reads_that_time_out() {
    let mut console = Scripted(VecDeque::from([
        Err(io::ErrorKind::TimedOut.into()),
        Ok(b"marker".to_vec()),
    ]));
    let mut read = Vec::new();
    read_until(&mut console, b"marker", later(), &mut read).unwrap();
    assert_eq!(read, b"marker");
}

#[test]
fn a_reader_fails_once_the_console_ends_or_its_deadline_passes_first() {
    // What the console delivered before it failed stays read, such as the last words of a clone
    // whose VMM ended.
    let mut ended = Scripted::of(&[b"boot\n", b"Kernel panic\n"]).ending();
    let mut read = Vec::new();
    let error = read_until(&mut ended, b"marker", later(), &mut read).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    assert_eq!(read, b"boot\nKernel panic\n");

    let mut silent = Scripted::of(&[b"oops\n"]);
    let mut read = Vec::new();
    let error = read_until(&mut silent, b"marker", Instant::now(), &mut read).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(read.is_empty());
    let deadline = Instant::now() + Duration::from_millis(50);
    let error = read_until(&mut silent, b"marker", deadline, &mut read).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(Instant::now() >= deadline);
    assert_eq!(read, b"oops\n");

    let mut failed = Scripted(VecDeque::from([
        Ok(b"oom\n".to_vec()),
        Err(io::ErrorKind::BrokenPipe.into()),
    ]));
    let mut read = Vec::new();
    let error = read_until(&mut failed, b"marker", later(), &mut read).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(read, b"oom\n");
}

#[test]
fn a_reader_finds_a_marker_that_spans_several_reads() {
    // The marker starts three reads before the one that completes it, in bytes that the reader
    // searched already.
    let mut console = Scripted::of(&[b"xxm", b"ar", b"ke", b"r", b"after"]);
    let mut read = Vec::new();
    read_until(&mut console, b"marker", later(), &mut read).unwrap();
    assert_eq!(read, b"xxmarker");
    // A marker that the reader held already needs no read.
    let mut idle = Scripted::of(&[]);
    read_until(&mut idle, b"marker", Instant::now(), &mut read).unwrap();

    // Nor does a marker that spans the reads of a burst of output that one read cannot hold.
    let mut burst = vec![b'x'; CHUNK - 3];
    burst.extend_from_slice(b"marker\n");
    let mut console = Scripted(VecDeque::from([Ok(burst.clone())]));
    let mut read = Vec::new();
    read_until(&mut console, b"marker", later(), &mut read).unwrap();
    assert_eq!(read, burst);
}

#[test]
fn a_reader_reads_what_the_guest_writes_once_it_asked_for_the_marker() {
    let mut console = Scripted::of(&[b"[    9.0] runtime-adapter: marker\n"]);
    let mut asked = false;
    let ((), read, delivered) = read_marker(
        &mut console,
        || asked = true,
        b"marker",
        Duration::from_secs(60),
    );
    assert!(asked);
    assert_eq!(read, b"[    9.0] runtime-adapter: marker\n");
    delivered.unwrap();
}

#[test]
fn a_reader_keeps_what_the_console_delivered_when_the_guest_did_not_answer() {
    // A guest whose kernel panicked once the reader connected answers nothing, and its VMM ends,
    // but its console delivered the panic.
    let mut console = Scripted::of(&[b"Kernel panic - not syncing\n"]).ending();
    let (answered, read, delivered) = read_marker(
        &mut console,
        || Err::<(), _>("the guest agent did not answer: the session was reset"),
        b"marker",
        Duration::from_secs(60),
    );
    assert_eq!(
        answered,
        Err("the guest agent did not answer: the session was reset")
    );
    assert_eq!(read, b"Kernel panic - not syncing\n");
    let error = delivered.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{error}");

    // A guest that refused the probe writes no marker either: the reader keeps what the console
    // delivered until its deadline.
    let mut console = Scripted::of(&[b"oops\n"]);
    let (answered, read, delivered) = read_marker(
        &mut console,
        || Err::<(), _>("the kmsg probe failed"),
        b"marker",
        Duration::from_millis(50),
    );
    assert_eq!(answered, Err("the kmsg probe failed"));
    assert_eq!(read, b"oops\n");
    let error = delivered.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
}

#[test]
fn a_reader_stops_at_once_when_the_ask_panicked() {
    // The reader of a console that never ends must not hold the panic back.
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut console = Scripted::of(&[b"boot\n"]);
        let outcome = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            read_marker(
                &mut console,
                || panic!("the ask panicked"),
                b"marker",
                Duration::from_secs(3600),
            )
        }));
        let _ = sender.send(outcome.is_err());
    });
    assert_eq!(receiver.recv_timeout(Duration::from_secs(30)), Ok(true));
}

#[test]
fn a_reader_reads_the_console_while_the_guest_is_asked() {
    // A guest whose console holds output that its reader left unread waits to write more, so it
    // answers only once the reader drained the console.
    let reads = Arc::new(AtomicUsize::new(0));
    let mut console = Counted {
        chunks: VecDeque::from([
            b"burst 1\n".to_vec(),
            b"burst 2\n".to_vec(),
            b"marker\n".to_vec(),
        ]),
        reads: Arc::clone(&reads),
    };
    let drained = || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while reads.load(Ordering::SeqCst) < 2 {
            if Instant::now() >= deadline {
                return Err("the reader read nothing while the guest was asked".to_owned());
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    };
    let (answered, read, delivered) =
        read_marker(&mut console, drained, b"marker", Duration::from_secs(60));
    answered.unwrap();
    delivered.unwrap();
    assert_eq!(read, b"burst 1\nburst 2\nmarker\n");
}

#[test]
fn a_reader_drains_the_console_until_the_guest_answered() {
    // A guest may write more once it wrote the marker, before it answers, so the reader reads on
    // until it answered.
    let reads = Arc::new(AtomicUsize::new(0));
    let mut console = Counted {
        chunks: VecDeque::from([
            b"marker\n".to_vec(),
            b"after 1\n".to_vec(),
            b"after 2\n".to_vec(),
        ]),
        reads: Arc::clone(&reads),
    };
    let drained = || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while reads.load(Ordering::SeqCst) < 3 {
            if Instant::now() >= deadline {
                return Err("the reader stopped once the console delivered the marker");
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    };
    let (answered, read, delivered) =
        read_marker(&mut console, drained, b"marker", Duration::from_secs(60));
    answered.unwrap();
    delivered.unwrap();
    assert_eq!(read, b"marker\nafter 1\nafter 2\n");
}

#[test]
fn a_log_is_a_regular_file_of_the_store() {
    let directory = std::env::temp_dir().join(format!(
        "runtime-adapter-console-regular-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    // A directory at the log's path is no log.
    let path = directory.join("console.log");
    fs::create_dir(&path).unwrap();
    assert!(open_log(&path).is_err());
    fs::remove_dir(&path).unwrap();

    // Nor is a link, whose target the log's writes would reach: an existing file, or one that
    // does not exist yet, which the open must not create.
    let outside = directory.join("outside");
    fs::write(&outside, b"kept").unwrap();
    let missing = directory.join("missing");
    for target in [&outside, &missing] {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &path).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_file(target, &path) {
            // Windows creates symbolic links only with a privilege that a test may lack.
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
            break;
        }
        assert!(open_log(&path).is_err(), "{}", target.display());
        fs::remove_file(&path).unwrap();
    }
    assert_eq!(fs::read(&outside).unwrap(), b"kept");
    assert!(!missing.exists());
    fs::remove_dir_all(&directory).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn a_log_is_no_fifo_whose_open_would_wait_for_a_reader() {
    use std::os::unix::fs::OpenOptionsExt;

    use nix::sys::stat::Mode;

    let directory = std::env::temp_dir().join(format!(
        "runtime-adapter-console-fifo-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("console.log");
    nix::unistd::mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    let (sender, receiver) = mpsc::channel();
    let opened = path.clone();
    thread::spawn(move || {
        let _ = sender.send(open_log(&opened).map(drop));
    });
    let refused = receiver.recv_timeout(Duration::from_secs(30));
    if refused.is_err() {
        // A reader releases the open that waits for one, before the test fails.
        let _ = fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(&path);
    }
    let error = refused
        .expect("the open of the log waited for a reader of the FIFO")
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
    fs::remove_dir_all(&directory).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn a_new_log_is_private_to_its_user_whatever_the_umask() {
    use std::os::unix::fs::PermissionsExt;

    use nix::sys::stat::Mode;

    let directory = std::env::temp_dir().join(format!(
        "runtime-adapter-console-umask-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("console.log");
    // A thread of its own, whose umask, which withholds even its user's permission to write what
    // it creates, no other thread shares, creates the log.
    thread::scope(|scope| {
        scope
            .spawn(|| {
                nix::sched::unshare(nix::sched::CloneFlags::CLONE_FS).unwrap();
                nix::sys::stat::umask(Mode::from_bits_truncate(0o277));
                open_log(&path).unwrap().write_all(b"first\n").unwrap();
            })
            .join()
            .unwrap();
    });
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    // So later reads append to it.
    open_log(&path).unwrap().write_all(b"second\n").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"first\nsecond\n");
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn a_log_keeps_what_each_read_appends_private_to_its_user() {
    let directory = std::env::temp_dir().join(format!(
        "runtime-adapter-console-log-{}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("console.log");
    for read in [&b"[    1.0] first\r\n"[..], b"[    2.0] second\r\n"] {
        open_log(&path).unwrap().write_all(read).unwrap();
    }
    assert_eq!(
        fs::read(&path).unwrap(),
        b"[    1.0] first\r\n[    2.0] second\r\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    fs::remove_dir_all(&directory).unwrap();
}
