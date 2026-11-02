//! Checks of the parsers against the kernel's formats.

use super::*;

/// An excerpt of a Linux 6 `/proc/timer_list` with one CPU.
const TIMER_LIST: &str = "\
Timer List Version: v0.10
HRTIMER_MAX_CLOCK_BASES: 8
now at 1234567890 nsecs

cpu: 0
 clock 0:
  .base:       00000000a1b2c3d4
  .index:      0
active timers:
 #0: <00000000deadbeef>, tick_nohz_highres_handler, S:01
  .expires_next   : 1235000000 nsecs
  .hres_active    : 1
  .nr_events      : 12345
  .nohz           : 1
  .highres        : 1

jiffies: 4294967296

Tick Device: mode:     1
Broadcast device
Clock Event Device: <NULL>
tick_broadcast_mask: 00
tick_broadcast_oneshot_mask: 00

Tick Device: mode:     1
Per CPU device: 0
Clock Event Device: lapic
 max_delta_ns:   1374389534634
 mode:           3
 next_event:     1235000000 nsecs
 oneshot stopped: lapic_timer_shutdown
 event_handler:  hrtimer_interrupt
 retries:        0
";

#[test]
fn byte_search_finds_any_bytes() {
    assert_eq!(find(b"ab\xffcd", b"\xffc"), Some(2));
    assert_eq!(find(b"abc", b""), Some(0));
    assert_eq!(find(b"ab", b"abc"), None);
    assert!(contains(b"x TSC unstable", b"TSC"));
    assert!(!contains(b"tsc", b"TSC"));
}

#[test]
fn kmsg_records_give_their_timestamp_and_first_line() {
    let record = KmsgRecord::parse(b"6,339,5140900,-;NET: Registered\n SUBSYSTEM=net\n");
    assert_eq!(
        record,
        Some(KmsgRecord {
            timestamp_us: Some(5_140_900),
            message: b"NET: Registered",
        })
    );
    let record = KmsgRecord::parse(b"6,1;\xff").unwrap();
    assert_eq!((record.timestamp_us, record.message), (None, &b"\xff"[..]));
    assert_eq!(KmsgRecord::parse(b"no header"), None);
}

#[test]
fn syslog_lines_strip_only_well_formed_timestamps() {
    let line = SyslogLine::parse(b"<6>[    3.000001] tsc: Refined");
    assert_eq!(line.timestamp_us, Some(3_000_001));
    assert_eq!(line.message, b"tsc: Refined");
    let line = SyslogLine::parse(b"<4>[12.5]x");
    assert_eq!(
        (line.timestamp_us, line.message),
        (Some(12_500_000), &b"x"[..])
    );
    let line = SyslogLine::parse(b"<6>[abc] message");
    assert_eq!(
        (line.timestamp_us, line.message),
        (None, &b"[abc] message"[..])
    );
    let line = SyslogLine::parse(b"plain");
    assert_eq!((line.timestamp_us, line.message), (None, &b"plain"[..]));
}

#[test]
fn cpu_lists_are_plain_ranges() {
    assert_eq!(parse_cpu_list(b"0-3,6"), Some(vec![0..=3, 6..=6]));
    assert_eq!(parse_cpu_list(b""), Some(Vec::new()));
    for list in [
        &b"3-1"[..],
        b"0,",
        b",0",
        b"-1",
        b"+1",
        b" 1",
        b"1-",
        b"0x1",
        b"1\n",
    ] {
        assert_eq!(parse_cpu_list(list), None, "{}", list.escape_ascii());
    }
}

#[test]
fn timer_lists_give_tick_devices_and_timer_bases() {
    let list = TimerList::parse(TIMER_LIST.as_bytes());
    let broadcast = list.broadcast.unwrap();
    assert_eq!(broadcast.device.as_deref(), Some("<NULL>"));
    assert_eq!(broadcast.tick_mode(), Some(TickMode::Oneshot));
    let cpu = &list.tick_devices[&0];
    assert_eq!(cpu.device.as_deref(), Some("lapic"));
    assert_eq!(cpu.handler.as_deref(), Some("hrtimer_interrupt"));
    assert_eq!(cpu.clock_event_state(), Some(ClockEventState::Oneshot));
    assert_eq!(cpu.state, Some(3));
    assert_eq!(
        list.timer_bases[&0],
        TimerBase {
            hres_active: Some(1),
            nohz: Some(1),
        }
    );
    let older = TimerList::parse(b"cpu: 2\n  .nohz_mode      : 2\n");
    assert_eq!(older.timer_bases[&2].nohz, Some(2));
    assert_eq!(TickMode::from_number(7), None);
    assert_eq!(
        ClockEventState::from_number(4).map(ClockEventState::name),
        Some("oneshot_stopped")
    );
}
