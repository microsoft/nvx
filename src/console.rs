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
use ::std::str::FromStr;
use ::std::time::{
    Duration,
    Instant,
};

/// A named console substring whose first appearance should be timed from guest start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimingMarker {
    label: String,
    text: String,
}

impl TimingMarker {
    pub fn label(&self) -> &str {
        &self.label
    }

    fn text(&self) -> &str {
        &self.text
    }
}

impl FromStr for TimingMarker {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (label, text) = value
            .split_once('=')
            .ok_or_else(|| "timing marker must have the form LABEL=TEXT".to_string())?;
        if label.is_empty()
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err("timing marker LABEL must use letters, digits, '.', '_' or '-'".into());
        }
        if text.is_empty() {
            return Err("timing marker TEXT must not be empty".into());
        }
        Ok(Self {
            label: label.to_string(),
            text: text.to_string(),
        })
    }
}

/// Runtime matcher for one named timing marker.
struct TrackedMarker {
    spec: TimingMarker,
    match_pos: usize,
    elapsed: Option<Duration>,
}

impl TrackedMarker {
    fn new(spec: TimingMarker) -> Self {
        Self {
            spec,
            match_pos: 0,
            elapsed: None,
        }
    }

    fn scan(&mut self, byte: u8, start: Option<Instant>) {
        if self.elapsed.is_some() {
            return;
        }
        let marker = self.spec.text().as_bytes();
        if byte == marker[self.match_pos] {
            self.match_pos += 1;
            if self.match_pos == marker.len() {
                self.elapsed = start.map(|instant| instant.elapsed());
            }
        } else {
            self.match_pos = usize::from(byte == marker[0]);
        }
    }
}

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
    /// Additional named markers measured from the same guest-start instant.
    timing_markers: Vec<TrackedMarker>,
    /// Total bytes emitted by the guest.
    bytes_out: u64,
}

impl Console {
    /// Creates a console with additional named timing markers.
    pub fn with_timing_markers(
        quiet: bool,
        marker: &str,
        timing_markers: &[TimingMarker],
    ) -> Self {
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
            timing_markers: timing_markers.iter().cloned().map(TrackedMarker::new).collect(),
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
        for marker in &mut self.timing_markers {
            marker.scan(byte, self.start);
        }
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

    /// Returns every additional timing marker observed so far, in CLI order.
    pub fn timings(&self) -> impl Iterator<Item = (&str, Duration)> {
        self.timing_markers
            .iter()
            .filter_map(|marker| marker.elapsed.map(|elapsed| (marker.spec.label(), elapsed)))
    }

    /// Returns the number of console bytes emitted so far.
    pub fn bytes_out(&self) -> u64 {
        self.bytes_out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_multiple_markers_from_one_start_in_cli_order() {
        let markers = ["first=PHASE-ONE".parse().unwrap(), "second=PHASE-TWO".parse().unwrap()];
        let mut console = Console::with_timing_markers(true, "DONE", &markers);
        console.mark_start();
        for byte in b"PHASE-ONE PHASE-TWO DONE" {
            console.write_byte(*byte);
        }
        let observed: Vec<&str> = console.timings().map(|(label, _)| label).collect();
        assert_eq!(observed, ["first", "second"]);
        assert!(console.booted());
    }

    #[test]
    fn timing_marker_parser_rejects_ambiguous_values() {
        assert!("missing-separator".parse::<TimingMarker>().is_err());
        assert!("=text".parse::<TimingMarker>().is_err());
        assert!("label=".parse::<TimingMarker>().is_err());
        assert!("bad label=text".parse::<TimingMarker>().is_err());
    }
}
