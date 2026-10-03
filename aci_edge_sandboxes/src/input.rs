use std::fmt;
use std::io::{self, PipeReader, Read};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

#[cfg(feature = "async")]
use crate::stream::QueueReader;

/// Interruptible standard input supplied to a backend through [`ExecIo`](crate::ExecIo).
///
/// Backends can clone its [`InputCloser`] to interrupt a pending read. They must finish the
/// input worker and drop this source before reporting a terminal execution outcome.
pub struct InputSource {
    reader: Box<dyn Read + Send>,
    closer: InputCloser,
}

impl InputSource {
    /// Adapts an OS pipe for interruptible reads without changing its writer's raw handle.
    ///
    /// The source must be the pipe's only reader; other readers could consume bytes between
    /// readiness and reading. On Unix the read endpoint is made nonblocking.
    pub fn from_pipe(reader: PipeReader) -> io::Result<Self> {
        native::prepare(&reader)?;
        let state = Arc::new(PipeState::default());
        let closing = Arc::clone(&state);
        Ok(Self::new(
            PipeInput { reader, state },
            InputCloser::new(move || {
                *closing
                    .closed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = true;
                closing.changed.notify_all();
            }),
        ))
    }

    pub(crate) fn new(reader: impl Read + Send + 'static, closer: InputCloser) -> Self {
        Self {
            reader: Box::new(reader),
            closer,
        }
    }

    #[cfg(feature = "async")]
    pub(crate) fn from_queue(reader: QueueReader) -> Self {
        let closer = reader.close_handle();
        Self::new(reader, closer)
    }

    /// Returns a handle that interrupts reads and stops accepting further input.
    pub fn close_handle(&self) -> InputCloser {
        self.closer.clone()
    }
}

impl Read for InputSource {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buffer)
    }
}

impl Drop for InputSource {
    fn drop(&mut self) {
        self.closer.close();
    }
}

impl fmt::Debug for InputSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputSource")
            .finish_non_exhaustive()
    }
}

/// Clonable close signal for a backend's [`InputSource`].
#[derive(Clone)]
pub struct InputCloser(Arc<dyn Fn() + Send + Sync>);

impl InputCloser {
    pub(crate) fn new(close: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(close))
    }

    /// Interrupts a pending read. Repeated calls have no additional effect.
    ///
    /// A pipe reader must finish and be dropped before its caller's writer observes closure.
    pub fn close(&self) {
        (self.0)();
    }
}

impl fmt::Debug for InputCloser {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputCloser")
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct PipeState {
    closed: Mutex<bool>,
    changed: Condvar,
}

struct PipeInput {
    reader: PipeReader,
    state: Arc<PipeState>,
}

impl Read for PipeInput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            if *self
                .state
                .closed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
            {
                return Ok(0);
            }
            match native::read_available(&mut self.reader, buffer) {
                Ok(Some(count)) => return Ok(count),
                Ok(None) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            let closed = self
                .state
                .closed
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let _waited = self
                .state
                .changed
                .wait_timeout_while(closed, Duration::from_millis(10), |closed| !*closed)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

#[cfg(unix)]
mod native {
    use std::os::fd::AsRawFd;

    use super::*;

    pub(super) fn prepare(reader: &PipeReader) -> io::Result<()> {
        // SAFETY: the pipe owns this live fd, and neither fcntl command takes a pointer.
        let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: only the read endpoint is changed; the caller's writer remains blocking.
        if unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn read_available(
        reader: &mut PipeReader,
        buffer: &mut [u8],
    ) -> io::Result<Option<usize>> {
        match reader.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            result => result.map(Some),
        }
    }
}

#[cfg(windows)]
mod native {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED,
    };
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    use super::*;

    pub(super) fn prepare(_reader: &PipeReader) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn read_available(
        reader: &mut PipeReader,
        buffer: &mut [u8],
    ) -> io::Result<Option<usize>> {
        let mut available = 0;
        // SAFETY: the pipe is live and available points to writable storage. No data is consumed.
        if unsafe {
            PeekNamedPipe(
                reader.as_raw_handle().cast(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut available,
                ptr::null_mut(),
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(code)
                    if code == ERROR_BROKEN_PIPE as i32
                        || code == ERROR_NO_DATA as i32
                        || code == ERROR_PIPE_NOT_CONNECTED as i32 =>
                {
                    Ok(Some(0))
                }
                _ => Err(error),
            };
        }
        if available == 0 {
            return Ok(None);
        }
        let length = buffer.len().min(available as usize);
        reader.read(&mut buffer[..length]).map(Some)
    }
}

#[cfg(not(any(unix, windows)))]
mod native {
    use super::*;

    pub(super) fn prepare(_reader: &PipeReader) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interruptible execution input requires Unix or Windows",
        ))
    }

    pub(super) fn read_available(
        _reader: &mut PipeReader,
        _buffer: &mut [u8],
    ) -> io::Result<Option<usize>> {
        prepare(_reader).map(|_| None)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    #[test]
    fn closing_input_finishes_a_blocked_pipe_reader_with_the_writer_retained() {
        let (reader, mut writer) = io::pipe().unwrap();
        let mut source = InputSource::from_pipe(reader).unwrap();
        let closer = source.close_handle();
        let (finished, received) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut buffer = [0u8; 1];
            let result = source.read(&mut buffer);
            drop(source);
            finished.send(result).unwrap();
        });
        closer.close();
        closer.close();
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            0
        );
        worker.join().unwrap();
        assert_eq!(
            writer.write(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn pipe_input_preserves_bytes_empty_reads_and_eof() {
        let (reader, mut writer) = io::pipe().unwrap();
        let mut source = InputSource::from_pipe(reader).unwrap();
        assert_eq!(source.read(&mut []).unwrap(), 0);
        writer.write_all(b"hello").unwrap();
        let mut buffer = [0u8; 5];
        source.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"hello");
        drop(writer);
        assert_eq!(source.read(&mut buffer).unwrap(), 0);
    }
}
