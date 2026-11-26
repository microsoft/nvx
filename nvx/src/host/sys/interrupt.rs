//! Ctrl-C on Unix hosts: a `SIGINT` handler that records the interrupt, which the CLI polls, and
//! writes, and on Linux reads of input files, that the interrupt ends.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::BorrowedFd;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use nix::errno::Errno;
use nix::libc;
use nix::poll::{PollFd, PollFlags, poll};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

/// Whether a `SIGINT` arrived since [`watch_interrupts`] installed its handler.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// The longest, in milliseconds, that [`write_interruptibly`] waits for room in its stream, and
/// that `read_interruptibly` waits for input or for a lease to end, before it checks for a
/// `SIGINT` again.
const WAIT_SLICE_MS: u16 = 250;

/// The `SIGINT` handler, which only records the signal.
extern "C" fn record(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Records each `SIGINT` for [`interrupted`] instead of letting it terminate the process, as
/// Python's handler, which raises `KeyboardInterrupt`, does.
///
/// Like Python, it keeps a `SIGINT` that the process started with ignored: a caller that ignores
/// the signal for its children keeps it from interrupting them.
pub(crate) fn watch_interrupts() -> io::Result<()> {
    let mut current = MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: a null action only reads the current disposition into `current`.
    Errno::result(unsafe { libc::sigaction(libc::SIGINT, ptr::null(), current.as_mut_ptr()) })?;
    // SAFETY: sigaction filled `current` in.
    if unsafe { current.assume_init() }.sa_sigaction == libc::SIG_IGN {
        return Ok(());
    }
    let action = SigAction::new(
        SigHandler::Handler(record),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe { sigaction(Signal::SIGINT, &action) }?;
    Ok(())
}

/// Returns whether a `SIGINT` arrived since [`watch_interrupts`].
pub(crate) fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Writes all of `data` to `stream`, such as standard output, and fails with
/// [`io::ErrorKind::Interrupted`] once a `SIGINT` has arrived, as a write of Python raises
/// `KeyboardInterrupt`, also while the stream waits for a reader that does not read.
///
/// The handler of [`watch_interrupts`] restarts the system calls that a `SIGINT` interrupts, so
/// a write that blocks would resume after it. Instead, this waits for room with `poll`, which a
/// signal ends, in slices of at most 250 ms, and writes at most `PIPE_BUF` bytes at once, which a
/// pipe with room takes without blocking. Like the standard streams of the standard library, it
/// discards what it writes to a descriptor that is not open.
pub(crate) fn write_interruptibly(stream: BorrowedFd<'_>, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        if interrupted() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
        }
        let mut descriptors = [PollFd::new(stream, PollFlags::POLLOUT)];
        match poll(&mut descriptors, WAIT_SLICE_MS) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
        // An error or hang-up that `poll` reports surfaces as the write's error.
        match nix::unistd::write(stream, &data[..data.len().min(libc::PIPE_BUF)]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => data = &data[count..],
            Err(Errno::EINTR | Errno::EAGAIN) => {}
            Err(Errno::EBADF) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Reads at most `limit` bytes of the file at `path`, and fails with
/// [`io::ErrorKind::Interrupted`] once a `SIGINT` has arrived while it waits for the file, as a
/// read of Python raises `KeyboardInterrupt`: for a FIFO's writer, for the input of a terminal,
/// such as `/dev/stdin`, or for the end of another process's lease of the file.
///
/// The handler of [`watch_interrupts`] restarts the system calls that a `SIGINT` interrupts, so
/// an open of a FIFO that waits for a writer, or a read that waits for input, would resume after
/// it. Instead, this opens the file with `O_NONBLOCK`, which does not wait for a FIFO's writer,
/// and tries the open again every 250 ms while another process holds a lease of the file, which
/// a non-blocking open fails on rather than waits for. A regular file, which never waits for
/// input, then reads as after a blocking open. Other files read without blocking, and before each
/// read this waits for input with `poll`, which a signal ends, in slices of at most 250 ms. Linux
/// reports a FIFO that no writer has opened since the open as neither readable nor hung up, so
/// the read waits for a writer, as a blocking open does, and ends once the writers have closed
/// it. The open makes a file description of its own, also of `/dev/stdin`, whose
/// `/proc/self/fd/0` Linux opens anew, so no other description of the file becomes non-blocking.
#[cfg(target_os = "linux")]
pub(crate) fn read_interruptibly(path: &std::path::Path, limit: u64) -> io::Result<Vec<u8>> {
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::thread;
    use std::time::Duration;

    use nix::fcntl::{FcntlArg, OFlag, fcntl};

    /// The most bytes that one read takes.
    const READ_BYTES: usize = 64 * 1024;

    let file = loop {
        if interrupted() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
        }
        match OpenOptions::new()
            .read(true)
            .custom_flags(OFlag::O_NONBLOCK.bits())
            .open(path)
        {
            Ok(file) => break file,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(WAIT_SLICE_MS.into()));
            }
            Err(error) => return Err(error),
        }
    };
    let mut data = Vec::new();
    if file.metadata()?.is_file() {
        let flags = OFlag::from_bits_retain(fcntl(&file, FcntlArg::F_GETFL)?);
        fcntl(
            &file,
            FcntlArg::F_SETFL(flags.difference(OFlag::O_NONBLOCK)),
        )?;
        (&file).take(limit).read_to_end(&mut data)?;
        return Ok(data);
    }
    let mut buffer = vec![0; READ_BYTES];
    let mut left = limit;
    while left > 0 {
        if interrupted() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
        }
        let mut descriptors = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
        match poll(&mut descriptors, WAIT_SLICE_MS) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
        // An error or hang-up that `poll` reports surfaces as the read's error or end of file.
        let room = usize::try_from(left).map_or(READ_BYTES, |left| left.min(READ_BYTES));
        match (&file).read(&mut buffer[..room]) {
            Ok(0) => break,
            Ok(count) => {
                data.extend_from_slice(&buffer[..count]);
                left -= count as u64;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(data)
}
