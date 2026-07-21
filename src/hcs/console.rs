// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! COM1 named-pipe transport for an HCS-managed VM.

use ::std::fs::{File, OpenOptions};
use ::std::io::{self, Read, Write};
use ::std::sync::atomic::{AtomicBool, Ordering};
use ::std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use ::std::sync::{Arc, Mutex};
use ::std::thread;
use ::std::time::{Duration, Instant};

use ::anyhow::{Context, Result, anyhow};
use ::windows::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED};
use ::windows::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler,
};
use ::windows::core::BOOL;

use crate::console::Console;

const ERROR_FILE_NOT_FOUND_CODE: i32 = 2;
const ERROR_PIPE_BUSY_CODE: i32 = 231;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const SNAPSHOT_REQUEST: &[u8] = b"NVX-HCS-SNAPSHOT/1 REQUEST\n";
const SNAPSHOT_RESTORE_ACK: &[u8] = b"NVX-HCS-SNAPSHOT/1 RESTORED\n";

/// Installed console-control handler, removed on drop.
pub struct ControlHandler;

impl ControlHandler {
    pub fn install() -> Result<Self> {
        INTERRUPTED.store(false, Ordering::SeqCst);
        // SAFETY: `handle_control` has the required static system ABI and remains valid forever.
        unsafe { SetConsoleCtrlHandler(Some(handle_control), true) }
            .context("installing HCS Ctrl-C handler")?;
        Ok(Self)
    }

    pub fn interrupted(&self) -> bool {
        INTERRUPTED.load(Ordering::SeqCst)
    }
}

impl Drop for ControlHandler {
    fn drop(&mut self) {
        // SAFETY: Removes the same static handler installed in `install`.
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(handle_control), false);
        }
    }
}

unsafe extern "system" fn handle_control(control_type: u32) -> BOOL {
    if matches!(
        control_type,
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT
    ) {
        INTERRUPTED.store(true, Ordering::SeqCst);
        true.into()
    } else {
        false.into()
    }
}

/// Tries documented transient named-pipe failures until `timeout` expires.
pub fn connect(path: &str, timeout: Duration) -> Result<Option<File>> {
    let deadline: Instant = Instant::now() + timeout;
    loop {
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(pipe) => return Ok(Some(pipe)),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(ERROR_FILE_NOT_FOUND_CODE | ERROR_PIPE_BUSY_CODE)
                ) =>
            {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("connecting to HCS COM pipe {path}"));
            }
        }
    }
}

/// Active output and input pumps for one COM pipe.
pub struct ConsoleSession {
    console: Arc<Mutex<Console>>,
    stop: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<io::Result<()>>>,
    writer: Option<thread::JoinHandle<io::Result<()>>>,
}

/// Dedicated COM2 reader that reports a versioned guest snapshot request.
pub struct SnapshotControlSession {
    events: Receiver<SnapshotControlEvent>,
    reader: thread::JoinHandle<io::Result<()>>,
}

enum SnapshotControlEvent {
    Request,
    Error(String),
}

impl SnapshotControlSession {
    pub fn start(mut pipe: File) -> Self {
        let (sender, events) = mpsc::sync_channel(2);
        let reader = thread::spawn(move || {
            let mut matcher: RequestMatcher = RequestMatcher::new();
            let mut buffer = [0u8; 256];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => return Ok(()),
                    Ok(count) => {
                        for byte in &buffer[..count] {
                            if matcher.scan(*byte) {
                                let _ = sender.try_send(SnapshotControlEvent::Request);
                            }
                        }
                    }
                    Err(error) if is_closed_pipe(&error) => return Ok(()),
                    Err(error) => {
                        let _ = sender.try_send(SnapshotControlEvent::Error(error.to_string()));
                        return Err(error);
                    }
                }
            }
        });
        Self { events, reader }
    }

    pub fn requested(&self) -> Result<bool> {
        match self.events.try_recv() {
            Ok(SnapshotControlEvent::Request) => Ok(true),
            Ok(SnapshotControlEvent::Error(error)) => {
                Err(anyhow!("reading HCS COM2 snapshot control: {error}"))
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => Ok(false),
        }
    }

    pub fn finish(self) -> Result<()> {
        self.reader
            .join()
            .map_err(|_| anyhow!("HCS snapshot control reader thread panicked"))?
            .context("reading HCS COM2 snapshot control")
    }
}

/// Releases the guest task blocked immediately after the captured request.
pub fn acknowledge_restore(pipe: &mut File) -> Result<()> {
    pipe.write_all(SNAPSHOT_RESTORE_ACK)
        .context("writing HCS COM2 restore acknowledgment")?;
    pipe.flush()
        .context("flushing HCS COM2 restore acknowledgment")
}

struct RequestMatcher {
    position: usize,
}

impl RequestMatcher {
    fn new() -> Self {
        Self { position: 0 }
    }

    fn scan(&mut self, byte: u8) -> bool {
        if byte == SNAPSHOT_REQUEST[self.position] {
            self.position += 1;
            if self.position == SNAPSHOT_REQUEST.len() {
                self.position = 0;
                return true;
            }
        } else {
            self.position = usize::from(byte == SNAPSHOT_REQUEST[0]);
        }
        false
    }
}

impl ConsoleSession {
    pub fn start(
        pipe: File,
        console: Arc<Mutex<Console>>,
        defer_stdin_until_boot: bool,
    ) -> Result<Self> {
        let mut reader_pipe: File = pipe
            .try_clone()
            .context("cloning HCS COM pipe for console output")?;
        let output_console: Arc<Mutex<Console>> = Arc::clone(&console);
        let reader = thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match reader_pipe.read(&mut buffer) {
                    Ok(0) => return Ok(()),
                    Ok(count) => {
                        let mut console = output_console.lock().expect("console poisoned");
                        for byte in &buffer[..count] {
                            console.write_byte(*byte);
                        }
                    }
                    Err(error) if is_closed_pipe(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
        });

        let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let input: Receiver<Vec<u8>> = spawn_stdin_reader();
        let writer = spawn_pipe_writer(
            pipe,
            Arc::clone(&console),
            Arc::clone(&stop),
            defer_stdin_until_boot,
            input,
        );
        Ok(Self {
            console,
            stop,
            reader: Some(reader),
            writer: Some(writer),
        })
    }

    pub fn finish(mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        let writer_result = self
            .writer
            .take()
            .expect("HCS console writer already joined")
            .join()
            .map_err(|_| anyhow!("HCS console writer thread panicked"))?;
        let reader_result = self
            .reader
            .take()
            .expect("HCS console reader already joined")
            .join()
            .map_err(|_| anyhow!("HCS console reader thread panicked"))?;
        writer_result.context("writing HCS COM1 input")?;
        reader_result.context("reading HCS COM1 output")?;
        self.console.lock().expect("console poisoned").flush();
        Ok(())
    }
}

impl Drop for ConsoleSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn is_closed_pipe(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
    ) || matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_BROKEN_PIPE.0 as i32
                || code == ERROR_NO_DATA.0 as i32
                || code == ERROR_PIPE_NOT_CONNECTED.0 as i32
    )
}

fn spawn_stdin_reader() -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buffer = [0u8; 256];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) if sender.send(buffer[..count].to_vec()).is_err() => break,
                Ok(_) => {}
            }
        }
    });
    receiver
}

fn spawn_pipe_writer(
    mut pipe: File,
    console: Arc<Mutex<Console>>,
    stop: Arc<AtomicBool>,
    defer_until_boot: bool,
    input: Receiver<Vec<u8>>,
) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        if defer_until_boot {
            while !stop.load(Ordering::SeqCst)
                && !console.lock().expect("console poisoned").booted()
            {
                thread::sleep(Duration::from_millis(1));
            }
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }

        while !stop.load(Ordering::SeqCst) {
            match input.recv_timeout(Duration::from_millis(20)) {
                Ok(bytes) => match pipe.write_all(&bytes) {
                    Ok(()) => {}
                    Err(error) if is_closed_pipe(&error) => break,
                    Err(error) => return Err(error),
                },
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_request_matcher_handles_noise_and_fragmentation() {
        let mut matcher: RequestMatcher = RequestMatcher::new();
        let mut matches: usize = 0;
        for chunk in [
            b"noise NVX-HCS-SNAP".as_slice(),
            b"SHOT/1 REQUEST\ntrailing".as_slice(),
            SNAPSHOT_REQUEST,
        ] {
            for byte in chunk {
                matches += usize::from(matcher.scan(*byte));
            }
        }
        assert_eq!(matches, 2);
    }

    #[test]
    fn snapshot_request_matcher_rejects_other_versions_and_commands() {
        let mut matcher: RequestMatcher = RequestMatcher::new();
        for byte in b"NVX-HCS-SNAPSHOT/2 REQUEST\nNVX-HCS-SNAPSHOT/1 SAVE\n" {
            assert!(!matcher.scan(*byte));
        }
    }

    #[test]
    fn recognizes_win32_pipe_disconnect_errors() {
        for code in [
            ERROR_BROKEN_PIPE.0,
            ERROR_NO_DATA.0,
            ERROR_PIPE_NOT_CONNECTED.0,
        ] {
            assert!(is_closed_pipe(&io::Error::from_raw_os_error(code as i32)));
        }
        assert_eq!(ERROR_PIPE_NOT_CONNECTED.0, 233);
        assert!(!is_closed_pipe(&io::Error::from_raw_os_error(5)));
    }
}
