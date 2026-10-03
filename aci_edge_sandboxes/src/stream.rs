//! In-process byte queues that connect backend output pumps to caller-facing streams.
//!
//! A queue decouples the backend from the consumer: backends push chunks without waiting for a
//! reader unless the queue holds more than [`QUEUE_LIMIT`] bytes, and readers drain it either
//! synchronously or through a waker.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::thread;

use crate::backend::{OutputCloser, OutputSink};
#[cfg(feature = "async")]
use crate::input::InputCloser;

/// Largest number of unread bytes a queue holds before writers wait for the reader.
pub(crate) const QUEUE_LIMIT: usize = 16 * 1024 * 1024;
/// Largest chunk stored at once. Larger writes are split, so a queue never exceeds
/// [`QUEUE_LIMIT`] by more than one segment.
const SEGMENT: usize = 64 * 1024;

#[derive(Default)]
struct State {
    chunks: VecDeque<Vec<u8>>,
    /// Bytes of the front chunk that the reader has already taken. Partial reads advance it
    /// instead of shifting the chunk's remaining bytes.
    consumed: usize,
    /// Unread bytes across all chunks.
    bytes: usize,
    writer_closed: bool,
    reader_closed: bool,
    reader_waker: Option<Waker>,
    writer_waker: Option<Waker>,
}

#[derive(Default)]
struct Queue {
    state: Mutex<State>,
    readable: Condvar,
    writable: Condvar,
}

impl Queue {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wake_reader(&self, state: &mut State) {
        self.readable.notify_all();
        if let Some(waker) = state.reader_waker.take() {
            waker.wake();
        }
    }

    fn wake_writer(&self, state: &mut State) {
        self.writable.notify_all();
        if let Some(waker) = state.writer_waker.take() {
            waker.wake();
        }
    }

    /// Ends the stream: the reader drains the buffered bytes and then reads end-of-file, while
    /// pending and later writes fail.
    fn close_writer(&self) {
        let mut state = self.lock();
        state.writer_closed = true;
        self.wake_reader(&mut state);
        self.wake_writer(&mut state);
    }

    /// Abandons the stream: buffered bytes are discarded, and pending and later writes fail.
    fn close_reader(&self) {
        let mut state = self.lock();
        state.reader_closed = true;
        state.chunks.clear();
        state.consumed = 0;
        state.bytes = 0;
        self.wake_reader(&mut state);
        self.wake_writer(&mut state);
    }
}

/// Creates a connected writer/reader pair.
pub(crate) fn queue() -> (QueueWriter, QueueReader) {
    let queue = Arc::new(Queue::default());
    (QueueWriter(Arc::clone(&queue)), QueueReader(queue))
}

/// Producer half of a queue.
pub(crate) struct QueueWriter(Arc<Queue>);

impl QueueWriter {
    fn has_room(state: &State, length: usize) -> bool {
        state.bytes == 0 || state.bytes.saturating_add(length) <= QUEUE_LIMIT
    }

    fn rejects_writes(state: &State) -> bool {
        state.reader_closed || state.writer_closed
    }

    fn push(&self, state: &mut State, chunk: &[u8]) {
        state.chunks.push_back(chunk.to_vec());
        state.bytes += chunk.len();
        self.0.wake_reader(state);
    }

    /// Appends `chunk`, blocking while the queue is full. Fails once either end closes the stream.
    pub(crate) fn write_blocking(&self, chunk: &[u8]) -> io::Result<()> {
        for segment in chunk.chunks(SEGMENT) {
            let mut state = self.0.lock();
            loop {
                if Self::rejects_writes(&state) {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                if Self::has_room(&state, segment.len()) {
                    break;
                }
                state = self
                    .0
                    .writable
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            self.push(&mut state, segment);
        }
        Ok(())
    }

    /// Appends up to one segment of `chunk` without blocking, registering the task's waker while
    /// the queue is full. Returns the number of bytes accepted.
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn poll_write(
        &self,
        context: &mut Context<'_>,
        chunk: &[u8],
    ) -> Poll<io::Result<usize>> {
        let segment = &chunk[..chunk.len().min(SEGMENT)];
        if segment.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let mut state = self.0.lock();
        if Self::rejects_writes(&state) {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if !Self::has_room(&state, segment.len()) {
            state.writer_waker = Some(context.waker().clone());
            return Poll::Pending;
        }
        self.push(&mut state, segment);
        Poll::Ready(Ok(segment.len()))
    }

    /// Marks the end of the stream. Dropping the writer has the same effect.
    pub(crate) fn close(&self) {
        self.0.close_writer();
    }
}

impl Drop for QueueWriter {
    fn drop(&mut self) {
        self.close();
    }
}

impl OutputSink for QueueWriter {
    fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.write_blocking(chunk)
    }

    fn close_handle(&self) -> Option<OutputCloser> {
        let queue = Arc::clone(&self.0);
        Some(OutputCloser::new(move || queue.close_writer()))
    }
}

/// Consumer half of a queue.
pub(crate) struct QueueReader(Arc<Queue>);

impl QueueReader {
    #[cfg(feature = "async")]
    pub(crate) fn close_handle(&self) -> InputCloser {
        let queue = Arc::clone(&self.0);
        InputCloser::new(move || queue.close_reader())
    }

    fn take(&self, state: &mut State, buffer: &mut [u8]) -> usize {
        let Some(front) = state.chunks.front() else {
            return 0;
        };
        let unread = &front[state.consumed..];
        let count = unread.len().min(buffer.len());
        buffer[..count].copy_from_slice(&unread[..count]);
        if count == unread.len() {
            state.chunks.pop_front();
            state.consumed = 0;
        } else {
            state.consumed += count;
        }
        state.bytes -= count;
        self.0.wake_writer(state);
        count
    }

    /// Reads available bytes, blocking until data arrives or the writer closes the stream.
    pub(crate) fn read_blocking(&self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut state = self.0.lock();
        loop {
            if !state.chunks.is_empty() {
                return Ok(self.take(&mut state, buffer));
            }
            if state.writer_closed || state.reader_closed {
                return Ok(0);
            }
            state = self
                .0
                .readable
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Reads available bytes without blocking, registering the task's waker while empty.
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn poll_read(&self, context: &mut Context<'_>, buffer: &mut [u8]) -> Poll<usize> {
        if buffer.is_empty() {
            return Poll::Ready(0);
        }
        let mut state = self.0.lock();
        if !state.chunks.is_empty() {
            return Poll::Ready(self.take(&mut state, buffer));
        }
        if state.writer_closed || state.reader_closed {
            return Poll::Ready(0);
        }
        state.reader_waker = Some(context.waker().clone());
        Poll::Pending
    }

    /// Reads the remaining stream to its end.
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn read_to_end_blocking(&self) -> Vec<u8> {
        let mut output = Vec::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match self.read_blocking(&mut buffer) {
                Ok(0) | Err(_) => return output,
                Ok(count) => output.extend_from_slice(&buffer[..count]),
            }
        }
    }
}

impl Drop for QueueReader {
    fn drop(&mut self) {
        self.0.close_reader();
    }
}

impl Read for QueueReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_blocking(buffer)
    }
}

/// Copies a queue into an operating-system pipe on a dedicated thread.
///
/// The thread ends when the queue's writer closes or when the pipe's reader goes away.
pub(crate) fn forward_to_pipe(
    reader: QueueReader,
    mut pipe: io::PipeWriter,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("aci-edge-sandboxes-exec-stream".to_owned())
        .spawn(move || {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                match reader.read_blocking(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if pipe.write_all(&buffer[..count]).is_err() {
                            break;
                        }
                    }
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "async")]
    #[test]
    fn closing_queue_input_wakes_a_blocked_reader_and_rejects_writes() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (writer, reader) = queue();
        let closer = reader.close_handle();
        let (finished, received) = mpsc::channel();
        let worker = thread::spawn(move || {
            finished.send(reader.read_blocking(&mut [0u8; 1])).unwrap();
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
            writer.write_blocking(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn delivers_bytes_in_order_until_close() {
        let (writer, reader) = queue();
        writer.write_blocking(b"hello ").unwrap();
        writer.write_blocking(b"world").unwrap();
        drop(writer);
        assert_eq!(reader.read_to_end_blocking(), b"hello world");
    }

    #[test]
    fn partial_reads_preserve_remaining_bytes() {
        let (writer, reader) = queue();
        writer.write_blocking(b"abcdef").unwrap();
        let mut buffer = [0u8; 4];
        assert_eq!(reader.read_blocking(&mut buffer).unwrap(), 4);
        assert_eq!(&buffer, b"abcd");
        drop(writer);
        assert_eq!(reader.read_to_end_blocking(), b"ef");
    }

    #[test]
    fn partial_reads_advance_through_a_segment_without_moving_it() {
        let (writer, reader) = queue();
        let data: Vec<u8> = (0..SEGMENT).map(|value| value as u8).collect();
        writer.write_blocking(&data).unwrap();
        let mut output = Vec::new();
        let mut byte = [0u8; 1];
        for _ in 0..3 {
            assert_eq!(reader.read_blocking(&mut byte).unwrap(), 1);
            output.push(byte[0]);
        }
        {
            let state = reader.0.lock();
            assert_eq!(state.chunks.front().map(Vec::len), Some(SEGMENT));
            assert_eq!((state.consumed, state.bytes), (3, SEGMENT - 3));
        }
        drop(writer);
        output.extend(reader.read_to_end_blocking());
        assert_eq!(output, data);
        let state = reader.0.lock();
        assert_eq!((state.chunks.len(), state.consumed, state.bytes), (0, 0, 0));
    }

    #[test]
    fn writes_fail_after_the_reader_is_dropped() {
        let (writer, reader) = queue();
        drop(reader);
        assert_eq!(
            writer.write_blocking(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn closing_output_fails_a_write_waiting_for_room_and_keeps_buffered_bytes() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (writer, reader) = queue();
        writer.write_blocking(&vec![0u8; QUEUE_LIMIT]).unwrap();
        let closer = writer.close_handle().unwrap();
        let (finished, received) = mpsc::channel();
        let blocked = thread::spawn(move || {
            finished.send(writer.write_blocking(b"x")).unwrap();
        });
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
        closer.close();
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        blocked.join().unwrap();
        assert_eq!(reader.read_to_end_blocking().len(), QUEUE_LIMIT);
    }

    #[test]
    fn large_writes_are_stored_in_bounded_segments() {
        let (writer, reader) = queue();
        let data: Vec<u8> = (0..200_000u32).map(|value| value as u8).collect();
        writer.write_blocking(&data).unwrap();
        {
            let state = writer.0.lock();
            assert_eq!(state.chunks.len(), 4);
            assert!(state.chunks.iter().all(|chunk| chunk.len() <= SEGMENT));
        }
        drop(writer);
        assert_eq!(reader.read_to_end_blocking(), data);
    }

    #[test]
    fn pipe_forwarding_reaches_end_of_file() {
        let (writer, reader) = queue();
        let (mut pipe_reader, pipe_writer) = io::pipe().unwrap();
        let forwarder = forward_to_pipe(reader, pipe_writer).unwrap();
        writer.write_blocking(b"through a pipe").unwrap();
        drop(writer);
        let mut output = Vec::new();
        pipe_reader.read_to_end(&mut output).unwrap();
        forwarder.join().unwrap();
        assert_eq!(output, b"through a pipe");
    }
}
