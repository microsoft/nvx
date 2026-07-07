// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # Guest Console
//!
//! A single output sink for the guest console: the byte stream transmitted by the portb
//! console device (`outb` to `0xE9`). It:
//!
//! - renders guest output to the host terminal, or discards it in `quiet` mode;
//! - buffers output and flushes per line (and on demand) instead of per byte, which is the
//!   dominant cost of a "loud" boot;
//! - counts emitted bytes; and
//! - watches the byte stream for a boot-completion marker and records the cold-start time
//!   (measured from [`mark_start`](Console::mark_start), i.e. the first guest instruction).
//!

use ::std::io::{
    self,
    Write,
};
use ::std::time::{
    Duration,
    Instant,
};

/// Shared guest console sink with boot-time instrumentation.
pub struct Console {
    /// Where guest output goes (buffered stdout, or a sink in quiet mode).
    sink: Box<dyn Write + Send>,
    /// Boot-completion marker to watch for in the output stream.
    marker: Vec<u8>,
    /// Current match position within `marker`.
    match_pos: usize,
    /// Instant the guest started executing.
    start: Option<Instant>,
    /// Elapsed time from `start` to the boot marker.
    cold_start: Option<Duration>,
    /// Total bytes emitted by the guest.
    bytes_out: u64,
}

impl Console {
    /// Creates a console. When `quiet` is set, output is discarded (only counted and
    /// scanned for the boot marker).
    pub fn new(quiet: bool, marker: &str) -> Self {
        let sink: Box<dyn Write + Send> = if quiet {
            Box::new(io::sink())
        } else {
            Box::new(io::BufWriter::new(io::stdout()))
        };
        Self {
            sink,
            marker: marker.as_bytes().to_vec(),
            match_pos: 0,
            start: None,
            cold_start: None,
            bytes_out: 0,
        }
    }

    /// Marks the instant the guest begins executing (the cold-start reference point).
    pub fn mark_start(&mut self) {
        self.start = Some(Instant::now());
    }

    /// Emits one byte of guest console output.
    pub fn write_byte(&mut self, byte: u8) {
        self.bytes_out += 1;
        let _ = self.sink.write_all(&[byte]);
        if byte == b'\n' {
            let _ = self.sink.flush();
        }
        self.scan_marker(byte);
    }

    /// Advances the boot-marker matcher and records the cold-start time on completion.
    fn scan_marker(&mut self, byte: u8) {
        if self.cold_start.is_some() || self.marker.is_empty() {
            return;
        }
        if byte == self.marker[self.match_pos] {
            self.match_pos += 1;
            if self.match_pos == self.marker.len() {
                self.cold_start = self.start.map(|start| start.elapsed());
            }
        } else {
            // Restart the match, allowing the current byte to begin a new one.
            self.match_pos = usize::from(byte == self.marker[0]);
        }
    }

    /// Flushes buffered output to the host terminal.
    pub fn flush(&mut self) {
        let _ = self.sink.flush();
    }

    /// Returns `true` once the boot marker has been observed.
    pub fn booted(&self) -> bool {
        self.cold_start.is_some()
    }

    /// Returns the measured cold-start duration, if boot has completed.
    pub fn cold_start(&self) -> Option<Duration> {
        self.cold_start
    }

    /// Returns the number of console bytes emitted so far.
    pub fn bytes_out(&self) -> u64 {
        self.bytes_out
    }
}
