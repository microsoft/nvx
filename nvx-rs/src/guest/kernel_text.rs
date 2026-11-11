//! The kernel's text formats that both time tools parse: the records of `/dev/kmsg` and of
//! `syslog(2)`, CPU lists, and `/proc/timer_list`.
//!
//! The kernel writes its procfs and sysfs files in ASCII, so their parsers read text. A log
//! message may quote any bytes, which the tools print or escape byte for byte, so the log's
//! parsers keep messages as bytes, and [`find`] searches them.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use super::parse::parse_decimal;

/// Returns the position of the first occurrence of `needle` in `haystack`, which may hold any
/// bytes. An empty needle occurs at the start.
pub(crate) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Returns whether `needle` occurs in `haystack`, which may hold any bytes.
pub(crate) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

/// One record of the kernel's structured log, as a read of `/dev/kmsg` returns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KmsgRecord<'a> {
    /// Microseconds since boot at which the kernel logged the record, when its header has them.
    pub(crate) timestamp_us: Option<u64>,
    /// The message's first line.
    pub(crate) message: &'a [u8],
}

impl<'a> KmsgRecord<'a> {
    /// Parses one record: a header of comma-separated fields, the priority, the sequence number,
    /// and the timestamp first, then `;`, the message, and the message's continuation lines.
    /// Returns `None` for a record without a header.
    pub(crate) fn parse(record: &'a [u8]) -> Option<Self> {
        let mut parts = record.splitn(2, |&byte| byte == b';');
        let header = parts.next()?;
        let body = parts.next()?;
        Some(Self {
            timestamp_us: header
                .split(|&byte| byte == b',')
                .nth(2)
                .and_then(parse_decimal),
            message: body.split(|&byte| byte == b'\n').next().unwrap_or_default(),
        })
    }
}

/// One line of the kernel's log as `syslog(2)` returns it: `<priority>`, then `[seconds]` when the
/// kernel stamps its messages, then the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(any(target_arch = "x86_64", test))]
pub(crate) struct SyslogLine<'a> {
    /// Microseconds since boot of the line's timestamp, when it has a well-formed one.
    pub(crate) timestamp_us: Option<u64>,
    /// The message, after the priority, a well-formed timestamp, and one space after it.
    pub(crate) message: &'a [u8],
}

#[cfg(any(target_arch = "x86_64", test))]
impl<'a> SyslogLine<'a> {
    /// Parses one line. A timestamp is spaces, then decimal seconds with a decimal fraction,
    /// between brackets. A line whose brackets hold anything else keeps them in its message.
    pub(crate) fn parse(line: &'a [u8]) -> Self {
        let line = line
            .strip_prefix(b"<")
            .and_then(|rest| {
                let end = rest.iter().position(|&byte| byte == b'>')?;
                Some(&rest[end + 1..])
            })
            .unwrap_or(line);
        let stamped = line.strip_prefix(b"[").and_then(|rest| {
            let end = rest.iter().position(|&byte| byte == b']')?;
            let timestamp_us = microseconds(&rest[..end])?;
            let message = &rest[end + 1..];
            Some(Self {
                timestamp_us: Some(timestamp_us),
                message: message.strip_prefix(b" ").unwrap_or(message),
            })
        });
        stamped.unwrap_or(Self {
            timestamp_us: None,
            message: line,
        })
    }
}

/// Parses a log timestamp, spaces then `seconds.fraction` in decimal, into microseconds.
#[cfg(any(target_arch = "x86_64", test))]
fn microseconds(stamp: &[u8]) -> Option<u64> {
    /// Microseconds in a second.
    const PER_SECOND: u64 = 1_000_000;
    /// Decimal digits of a microsecond fraction of a second.
    const DIGITS: usize = 6;
    let stamp = std::str::from_utf8(stamp).ok()?.trim_start_matches(' ');
    let (seconds, fraction) = stamp.split_once('.')?;
    if !(seconds.bytes().chain(fraction.bytes())).all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let whole = if seconds.is_empty() {
        0
    } else {
        seconds.parse::<u64>().ok()?
    };
    let micros = fraction
        .bytes()
        .chain(std::iter::repeat(b'0'))
        .take(DIGITS)
        .fold(0, |micros, digit| micros * 10 + u64::from(digit - b'0'));
    whole.checked_mul(PER_SECOND)?.checked_add(micros)
}

/// Parses a CPU list in the kernel's syntax, such as `0-3,6`: comma-separated CPU numbers and
/// inclusive ranges of them, in plain decimal. An empty list has no ranges.
///
/// Returns `None` for any other form, including a range that ends before it starts.
pub(crate) fn parse_cpu_list(text: &[u8]) -> Option<Vec<RangeInclusive<usize>>> {
    /// Parses one CPU number.
    fn cpu(text: &[u8]) -> Option<usize> {
        parse_decimal(text).and_then(|cpu| usize::try_from(cpu).ok())
    }
    if text.is_empty() {
        return Some(Vec::new());
    }
    text.split(|&byte| byte == b',')
        .map(|item| {
            let mut bounds = item.splitn(2, |&byte| byte == b'-');
            let first = cpu(bounds.next()?)?;
            let last = bounds.next().map_or(Some(first), cpu)?;
            (first <= last).then_some(first..=last)
        })
        .collect()
}

/// The modes of a tick device, `enum tick_device_mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TickMode {
    /// The device ticks periodically.
    Periodic,
    /// The device is programmed for each next event.
    Oneshot,
}

impl TickMode {
    /// Returns the mode that the kernel numbers `number`.
    pub(crate) const fn from_number(number: i32) -> Option<Self> {
        match number {
            0 => Some(Self::Periodic),
            1 => Some(Self::Oneshot),
            _ => None,
        }
    }

    /// Returns the mode's name, as the time probe prints it.
    #[cfg(target_arch = "x86_64")]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Periodic => "periodic",
            Self::Oneshot => "oneshot",
        }
    }
}

/// The states of a clock-event device, `enum clock_event_state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClockEventState {
    /// The device is not in use.
    Detached,
    /// The device is powered down.
    Shutdown,
    /// The device fires periodically.
    Periodic,
    /// The device fires once for each programmed event.
    Oneshot,
    /// The device is in one-shot mode with no event programmed.
    OneshotStopped,
}

impl ClockEventState {
    /// Returns the state that the kernel numbers `number`.
    pub(crate) const fn from_number(number: i32) -> Option<Self> {
        match number {
            0 => Some(Self::Detached),
            1 => Some(Self::Shutdown),
            2 => Some(Self::Periodic),
            3 => Some(Self::Oneshot),
            4 => Some(Self::OneshotStopped),
            _ => None,
        }
    }

    /// Returns the state's name, as the time probe prints it.
    #[cfg(any(target_arch = "x86_64", test))]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Detached => "detached",
            Self::Shutdown => "shutdown",
            Self::Periodic => "periodic",
            Self::Oneshot => "oneshot",
            Self::OneshotStopped => "oneshot_stopped",
        }
    }
}

/// One tick device's section of `/proc/timer_list`. The numbers are the kernel's, so that the
/// diagnostics can print values that no variant of [`TickMode`] or [`ClockEventState`] names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TickDevice {
    /// The tick device's mode, from the `Tick Device: mode:` line that starts the section.
    pub(crate) mode: Option<i32>,
    /// The clock-event device's name, `<NULL>` for none.
    pub(crate) device: Option<String>,
    /// The clock-event device's state.
    pub(crate) state: Option<i32>,
    /// The clock-event device's event handler.
    pub(crate) handler: Option<String>,
}

impl TickDevice {
    /// Returns the tick device's mode, when the kernel reported a known one.
    pub(crate) fn tick_mode(&self) -> Option<TickMode> {
        self.mode.and_then(TickMode::from_number)
    }

    /// Returns the clock-event device's state, when the kernel reported a known one.
    pub(crate) fn clock_event_state(&self) -> Option<ClockEventState> {
        self.state.and_then(ClockEventState::from_number)
    }
}

/// One CPU's high-resolution timer base in `/proc/timer_list`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimerBase {
    /// Whether high-resolution timers are active, as the kernel numbers it.
    pub(crate) hres_active: Option<i32>,
    /// The tickless mode, `.nohz_mode` in older kernels and `.nohz` in newer ones.
    pub(crate) nohz: Option<i32>,
}

/// The tick devices and timer bases that `/proc/timer_list` describes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimerList {
    /// The broadcast tick device's section, when the list has one.
    pub(crate) broadcast: Option<TickDevice>,
    /// Each CPU's tick device, by CPU number.
    pub(crate) tick_devices: BTreeMap<usize, TickDevice>,
    /// Each CPU's timer base, by CPU number.
    pub(crate) timer_bases: BTreeMap<usize, TimerBase>,
}

/// The section of `/proc/timer_list` that its lines describe.
#[derive(Clone, Copy)]
enum Section {
    /// No section, or one whose lines the tools ignore.
    None,
    /// The broadcast tick device.
    Broadcast,
    /// One CPU's tick device.
    TickDevice(usize),
    /// One CPU's timer base.
    TimerBase(usize),
}

impl TimerList {
    /// Parses the list's `key: value` lines, whose keys the kernel pads with whitespace.
    pub(crate) fn parse(text: &[u8]) -> Self {
        /// Returns the number that starts `value`.
        fn number(value: &str) -> Option<i32> {
            value.split_whitespace().next()?.parse().ok()
        }
        /// Returns the CPU number that starts `value`.
        fn cpu(value: &str) -> Option<usize> {
            number(value).and_then(|cpu| usize::try_from(cpu).ok())
        }
        /// Returns the word that starts `value`.
        fn word(value: &str) -> Option<String> {
            value.split_whitespace().next().map(str::to_owned)
        }
        let text: Cow<'_, str> = String::from_utf8_lossy(text);
        let mut list = Self::default();
        let mut section = Section::None;
        let mut mode = None;
        for line in text.lines() {
            if line.starts_with("Broadcast device") {
                list.broadcast = Some(TickDevice {
                    mode,
                    ..TickDevice::default()
                });
                section = Section::Broadcast;
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            match key.trim() {
                "cpu" => section = cpu(value).map_or(Section::None, Section::TimerBase),
                "Tick Device" => {
                    mode = value.trim().strip_prefix("mode:").and_then(number);
                    section = Section::None;
                }
                "Per CPU device" => {
                    section = cpu(value).map_or(Section::None, |cpu| {
                        let device = TickDevice {
                            mode,
                            ..TickDevice::default()
                        };
                        list.tick_devices.insert(cpu, device);
                        Section::TickDevice(cpu)
                    });
                }
                "Clock Event Device" => {
                    if let Some(device) = list.tick_device(section) {
                        device.device = word(value);
                    }
                }
                "mode" => {
                    if let Some(device) = list.tick_device(section) {
                        device.state = number(value);
                    }
                }
                "event_handler" => {
                    if let Some(device) = list.tick_device(section) {
                        device.handler = word(value);
                    }
                }
                ".hres_active" => {
                    if let Some(base) = list.timer_base(section) {
                        base.hres_active = number(value);
                    }
                }
                ".nohz_mode" | ".nohz" => {
                    if let Some(base) = list.timer_base(section) {
                        base.nohz = number(value);
                    }
                }
                _ => {}
            }
        }
        list
    }

    /// Returns the tick device that `section` describes, if it describes one.
    fn tick_device(&mut self, section: Section) -> Option<&mut TickDevice> {
        match section {
            Section::Broadcast => self.broadcast.as_mut(),
            Section::TickDevice(cpu) => self.tick_devices.get_mut(&cpu),
            Section::None | Section::TimerBase(_) => None,
        }
    }

    /// Returns the timer base that `section` describes, if it describes one.
    fn timer_base(&mut self, section: Section) -> Option<&mut TimerBase> {
        match section {
            Section::TimerBase(cpu) => Some(self.timer_bases.entry(cpu).or_default()),
            Section::None | Section::Broadcast | Section::TickDevice(_) => None,
        }
    }
}

#[cfg(test)]
mod tests;
