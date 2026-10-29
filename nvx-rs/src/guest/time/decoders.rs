//! Pure decoders, parsers, and arithmetic of the guest time ABI.

use std::fmt::{self, Display, Write as _};
use std::ops::RangeInclusive;

use super::cpu_mask::CpuMask;
use super::events::ViolationCode;
use super::{MAX_CPUS, NSEC_PER_SEC, NSEC_PER_USEC};
use crate::guest::kernel_text::{
    self, ClockEventState, SyslogLine, TickDevice, TickMode, TimerList,
};
use crate::guest::parse::parse_decimal;
use crate::guest::reseed::Seed;
use crate::guest::sys::ClockAdjustment;
use crate::guest::sys::x86::{CpuidResult, cpuid};
use crate::protocol::cmdline::{self, Parameter};

/// Fixed restore-packet header length.
pub(crate) const PACKET_HEADER_SIZE: usize = 32;
/// Encoded length of one memory range.
pub(crate) const PACKET_RANGE_SIZE: usize = 16;
/// Entropy payload length appended to every restore packet: a restore's [`Seed`].
pub(crate) const PACKET_ENTROPY_SIZE: usize = 64;
// The packet's entropy is decoded straight into a seed.
const _: () = assert!(PACKET_ENTROPY_SIZE == Seed::LENGTH);
/// Fixed host time-sample record length.
pub(crate) const SAMPLE_SIZE: usize = 16;
/// VM generation identifier length.
pub(crate) const GENERATION_ID_SIZE: usize = 16;

/// The magic number that starts every restore packet.
const PACKET_MAGIC: [u8; 3] = *b"OVR";
/// The restore-packet version of time ABI version one,
/// [`RESTORE_PACKET_VERSION`](crate::contract::RESTORE_PACKET_VERSION), which a template's
/// compatibility key records.
const PACKET_VERSION: u8 = {
    let version = crate::contract::RESTORE_PACKET_VERSION;
    assert!(version <= u8::MAX as u64);
    version as u8
};
/// Restore flag indicating that the downtime came from the host's UTC clock.
const PACKET_DOWNTIME_UTC: u8 = 0x01;
/// Restore flag indicating that memory ranges follow.
pub(crate) const PACKET_MEMORY_TARGET: u8 = 0x02;
/// Restore flag requiring a snapshot acknowledgement.
pub(crate) const PACKET_ACK_REQUIRED: u8 = 0x04;
/// Mask of restore flags defined by ABI version one.
const PACKET_KNOWN_FLAGS: u8 = 0x0f;
/// The online VP counts that a restore packet may give.
const PACKET_ONLINE_VP_COUNTS: [u8; 5] = [0, 1, 2, 4, 8];
/// The time-sample version of time ABI version one.
const SAMPLE_VERSION: u8 = 1;
/// Time-sample flag reserved for host test hooks.
const SAMPLE_TEST_HOOKS: u8 = 0x08;
/// Maximum signed restore rate deviation, equivalent to 250 ppm.
const RATE_DEVIATION_LIMIT: i32 = 16_384_000;
/// One part per million in Linux's frequency correction, which `adjtimex` scales by 2^16.
const SCALED_PPM: i64 = 1 << 16;
/// Parts per billion in a part per million.
const PPB_PER_PPM: i64 = 1000;
/// Linux's limit of the frequency correction, 500 ppm.
const FREQUENCY_LIMIT: i64 = 500 * SCALED_PPM;
/// Seconds in a day.
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
/// Maximum accepted downtime of thirty days.
const DOWNTIME_LIMIT_NS: u64 = 30 * SECONDS_PER_DAY * NSEC_PER_SEC as u64;
/// kHz in a MHz, which scale the whole MHz of `/proc/cpuinfo`.
const KHZ_PER_MHZ: u64 = 1000;
/// Offset magnitude at or above which correction steps the clock.
const STEP_THRESHOLD_NS: u64 = 128_000_000;
/// Time constant of the phase-locked loop while the discipline converges.
const FAST_TIME_CONSTANT: i64 = 4;
/// Time constant of the phase-locked loop once the discipline has converged.
const SLOW_TIME_CONSTANT: i64 = 6;
/// Bytes of a kernel message that a boot-log diagnostic quotes.
const QUOTED_MESSAGE_MAX: usize = 300;
/// Bytes of a kernel command-line token that a diagnostic quotes.
const QUOTED_TOKEN_MAX: usize = 120;
/// The prefix of the time tools' own kernel-log records, which the watcher ignores.
const OWN_RECORD_PREFIX: &[u8] = b"NVX-TIME-";

/// Reads the little-endian fields of a binary record in order, and fails with `incomplete` when
/// the record ends before a field.
struct Fields<'a, E> {
    /// The bytes after the fields read so far.
    rest: &'a [u8],
    /// The error of a record that ends early.
    incomplete: E,
}

impl<'a, E: Copy> Fields<'a, E> {
    /// Starts reading `record`.
    const fn new(record: &'a [u8], incomplete: E) -> Self {
        Self {
            rest: record,
            incomplete,
        }
    }

    /// Reads the next `N` bytes.
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], E> {
        self.chunk().copied()
    }

    /// Reads the next `N` bytes, which it borrows from the record rather than copies.
    fn chunk<const N: usize>(&mut self) -> Result<&'a [u8; N], E> {
        let (field, rest) = self.rest.split_first_chunk().ok_or(self.incomplete)?;
        self.rest = rest;
        Ok(field)
    }

    /// Reads the next 16-bit field.
    fn u16(&mut self) -> Result<u16, E> {
        self.bytes().map(u16::from_le_bytes)
    }

    /// Reads the next 32-bit field.
    fn u32(&mut self) -> Result<u32, E> {
        self.bytes().map(u32::from_le_bytes)
    }

    /// Reads the next signed 32-bit field.
    fn i32(&mut self) -> Result<i32, E> {
        self.bytes().map(i32::from_le_bytes)
    }

    /// Reads the next 64-bit field.
    fn u64(&mut self) -> Result<u64, E> {
        self.bytes().map(u64::from_le_bytes)
    }
}

/// Why a restore packet is invalid. Its `Display` is the diagnostic that repair reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PacketError {
    /// The packet ends within its header.
    IncompleteHeader,
    /// The magic number is not `OVR`.
    Magic([u8; 3]),
    /// The version is not version one's.
    Version(u8),
    /// The flags set reserved bits.
    ReservedFlags(u8),
    /// The online VP count is not 0, 1, 2, 4, or 8.
    OnlineVpCount(u8),
    /// Memory ranges follow, but the flags select no memory target.
    RangesWithoutTarget(u8),
    /// The reserved byte is not zero.
    ReservedByte(u8),
    /// The generation is zero.
    ZeroGeneration,
    /// The rate deviation exceeds 250 ppm.
    RateDeviation(i32),
    /// The downtime exceeds 30 days.
    Downtime(u64),
    /// The UTC time is zero or does not fit the clocks' signed nanoseconds.
    Utc(u64),
    /// The packet ends before the memory ranges and the entropy that its header gives.
    IncompleteBody,
    /// The memory range at this index is empty, or ends past the address space.
    Range(usize),
}

impl Display for PacketError {
    /// Writes the diagnostic, such as `restore packet version is 3`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::IncompleteHeader => formatter.write_str("restore packet header is incomplete"),
            Self::Magic([first, second, third]) => write!(
                formatter,
                "restore packet magic is {first:02x}{second:02x}{third:02x}"
            ),
            Self::Version(version) => write!(formatter, "restore packet version is {version}"),
            Self::ReservedFlags(flags) => write!(
                formatter,
                "restore packet flags 0x{flags:02x} set reserved bits"
            ),
            Self::OnlineVpCount(count) => {
                write!(formatter, "restore packet online VP count is {count}")
            }
            Self::RangesWithoutTarget(count) => write!(
                formatter,
                "restore packet has {count} memory ranges without a memory target"
            ),
            Self::ReservedByte(byte) => write!(formatter, "restore packet reserved byte is {byte}"),
            Self::ZeroGeneration => formatter.write_str("restore packet generation is 0"),
            Self::RateDeviation(deviation) => write!(
                formatter,
                "restore packet rate deviation {deviation} exceeds the 250 ppm tolerance"
            ),
            Self::Downtime(downtime) => write!(
                formatter,
                "restore packet downtime {downtime} ns exceeds 30 days"
            ),
            Self::Utc(utc) => write!(formatter, "restore packet UTC {utc} is invalid"),
            Self::IncompleteBody => formatter.write_str("restore packet body is incomplete"),
            Self::Range(index) => write!(formatter, "restore memory range {index} is invalid"),
        }
    }
}

impl std::error::Error for PacketError {}

/// The validated header of a version-one restore packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PacketHeader {
    /// Packet behavior flags.
    pub(crate) flags: u8,
    /// Number of online virtual processors reported by the VMM.
    pub(crate) online_vp_count: u8,
    /// Number of following memory ranges.
    pub(crate) range_count: u8,
    /// New VM generation.
    pub(crate) generation: u32,
    /// Host rate deviation in signed scaled parts per million.
    pub(crate) rate_deviation: i32,
    /// Restore downtime in nanoseconds.
    pub(crate) downtime_ns: u64,
    /// Host UTC at packet creation in nanoseconds.
    pub(crate) utc_ns: u64,
}

impl PacketHeader {
    /// Decodes and validates the header at the start of `bytes`.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, PacketError> {
        let mut fields = Fields::new(bytes, PacketError::IncompleteHeader);
        let magic: [u8; 3] = fields.bytes()?;
        let [version, flags, online_vp_count, range_count, reserved] = fields.bytes()?;
        let header = Self {
            flags,
            online_vp_count,
            range_count,
            generation: fields.u32()?,
            rate_deviation: fields.i32()?,
            downtime_ns: fields.u64()?,
            utc_ns: fields.u64()?,
        };
        if magic != PACKET_MAGIC {
            return Err(PacketError::Magic(magic));
        }
        if version != PACKET_VERSION {
            return Err(PacketError::Version(version));
        }
        if flags & !PACKET_KNOWN_FLAGS != 0 {
            return Err(PacketError::ReservedFlags(flags));
        }
        if !PACKET_ONLINE_VP_COUNTS.contains(&online_vp_count) {
            return Err(PacketError::OnlineVpCount(online_vp_count));
        }
        if !header.memory_target() && range_count != 0 {
            return Err(PacketError::RangesWithoutTarget(range_count));
        }
        if reserved != 0 {
            return Err(PacketError::ReservedByte(reserved));
        }
        if header.generation == 0 {
            return Err(PacketError::ZeroGeneration);
        }
        if !(-RATE_DEVIATION_LIMIT..=RATE_DEVIATION_LIMIT).contains(&header.rate_deviation) {
            return Err(PacketError::RateDeviation(header.rate_deviation));
        }
        if header.downtime_ns > DOWNTIME_LIMIT_NS {
            return Err(PacketError::Downtime(header.downtime_ns));
        }
        if header.utc_ns == 0 || i64::try_from(header.utc_ns).is_err() {
            return Err(PacketError::Utc(header.utc_ns));
        }
        Ok(header)
    }

    /// Returns the length of the body that follows the header.
    pub(crate) fn body_size(&self) -> usize {
        usize::from(self.range_count) * PACKET_RANGE_SIZE + PACKET_ENTROPY_SIZE
    }

    /// Returns whether the host measured the downtime with its UTC clock, rather than with its
    /// monotonic clock.
    pub(crate) const fn downtime_from_utc(&self) -> bool {
        self.flags & PACKET_DOWNTIME_UTC != 0
    }

    /// Returns whether the packet selects a memory target, whose memory ranges follow.
    pub(crate) const fn memory_target(&self) -> bool {
        self.flags & PACKET_MEMORY_TARGET != 0
    }

    /// Returns whether the host waits for the guest to acknowledge the restore.
    pub(crate) const fn acknowledgement_required(&self) -> bool {
        self.flags & PACKET_ACK_REQUIRED != 0
    }
}

/// A validated version-one restore packet.
///
/// It is neither `Clone` nor comparable, since its entropy is a [`Seed`], which has one owner.
#[derive(Debug)]
pub(crate) struct RestorePacket {
    /// The packet's header.
    pub(crate) header: PacketHeader,
    /// Memory ranges as `(start, length)` pairs.
    pub(crate) ranges: Vec<(u64, u64)>,
    /// Fresh entropy, which starts with the new VM generation ID, decoded straight into the
    /// seed's allocation.
    pub(crate) entropy: Seed,
}

impl RestorePacket {
    /// Decodes and validates the memory ranges and entropy that follow `header` in `body`.
    pub(crate) fn parse(header: PacketHeader, body: &[u8]) -> Result<Self, PacketError> {
        let mut fields = Fields::new(body, PacketError::IncompleteBody);
        let ranges = (0..usize::from(header.range_count))
            .map(|index| {
                let (start, length) = (fields.u64()?, fields.u64()?);
                if length == 0 || start.checked_add(length).is_none() {
                    return Err(PacketError::Range(index));
                }
                Ok((start, length))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            header,
            ranges,
            entropy: Seed::copy_from(fields.chunk()?),
        })
    }

    /// Returns the new VM generation ID, which leads the entropy.
    pub(crate) fn generation_id(&self) -> &[u8] {
        &self.entropy.bytes()[..GENERATION_ID_SIZE]
    }
}

/// Why a host time sample is invalid. Its `Display` is the sample's diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeSampleError {
    /// The sample is shorter than its record.
    Incomplete,
    /// The version is not version one's.
    Version(u8),
    /// The flags set bits other than the test-hook flag, or the reserved field is not zero.
    Flags {
        /// The sample's flags.
        flags: u8,
        /// The sample's reserved field.
        reserved: u16,
    },
    /// The UTC time is zero or does not fit the clocks' signed nanoseconds.
    Utc(u64),
}

impl Display for TimeSampleError {
    /// Writes the diagnostic, such as `time sample version is 2`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Incomplete => formatter.write_str("time sample is incomplete"),
            Self::Version(version) => write!(formatter, "time sample version is {version}"),
            Self::Flags { flags, reserved } => write!(
                formatter,
                "time sample flags 0x{flags:02x} or reserved 0x{reserved:04x}"
            ),
            Self::Utc(utc) => write!(formatter, "time sample UTC {utc} is invalid"),
        }
    }
}

impl std::error::Error for TimeSampleError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// One host clock sample paired with guest clock brackets.
pub(crate) struct TimeSample {
    /// Sample feature/test flags.
    pub(crate) flags: u8,
    /// VM generation associated with the sample.
    pub(crate) generation: u32,
    /// Host UTC sample in nanoseconds.
    pub(crate) utc_ns: u64,
    /// Guest realtime immediately before requesting the sample.
    pub(crate) t0: i64,
    /// Guest realtime immediately after requesting the sample.
    pub(crate) t1: i64,
}

impl TimeSample {
    /// Decodes and validates the sample record at the start of `bytes`, without the guest's
    /// brackets.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, TimeSampleError> {
        let mut fields = Fields::new(bytes, TimeSampleError::Incomplete);
        let [version, flags] = fields.bytes()?;
        let reserved = fields.u16()?;
        let generation = fields.u32()?;
        let utc_ns = fields.u64()?;
        if version != SAMPLE_VERSION {
            return Err(TimeSampleError::Version(version));
        }
        if flags & !SAMPLE_TEST_HOOKS != 0 || reserved != 0 {
            return Err(TimeSampleError::Flags { flags, reserved });
        }
        if utc_ns == 0 || i64::try_from(utc_ns).is_err() {
            return Err(TimeSampleError::Utc(utc_ns));
        }
        Ok(Self {
            flags,
            generation,
            utc_ns,
            t0: 0,
            t1: 0,
        })
    }
}

/// Encodes bytes as lowercase hexadecimal without separators.
pub(crate) fn format_hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Decodes exactly `N` bytes from `2 * N` hexadecimal digits, in either case.
pub(crate) fn parse_hex<const N: usize>(text: &[u8]) -> Option<[u8; N]> {
    if text.len() != 2 * N {
        return None;
    }
    let digit = |byte: u8| char::from(byte).to_digit(16);
    let mut bytes = [0; N];
    for (byte, pair) in bytes.iter_mut().zip(text.chunks_exact(2)) {
        *byte = u8::try_from(digit(pair[0])? << 4 | digit(pair[1])?).ok()?;
    }
    Some(bytes)
}

/// Computes clock offset and half-round-trip uncertainty with wrapping C arithmetic.
pub(crate) fn pair_clocks(t0: i64, t1: i64, utc_ns: u64) -> (i64, i64) {
    let epsilon = t1.wrapping_sub(t0) / 2;
    (
        (utc_ns as i64).wrapping_sub(t0.wrapping_add(epsilon)),
        epsilon,
    )
}

/// Converts host rate deviation into a clamped Linux frequency correction.
pub(crate) fn restore_frequency(rate_deviation: i32) -> i64 {
    (-i64::from(rate_deviation)).clamp(-FREQUENCY_LIMIT, FREQUENCY_LIMIT)
}

/// Converts Linux scaled-ppm frequency into rounded parts per billion.
pub(crate) fn frequency_to_ppb(frequency: i64) -> i64 {
    let scaled = frequency * PPB_PER_PPM;
    let half = SCALED_PPM / 2;
    (scaled + if scaled >= 0 { half } else { -half }) / SCALED_PPM
}

/// Splits a signed nanosecond offset into whole seconds, rounded toward negative infinity, and
/// the nonnegative nanoseconds that complete it, as a step of the wall clock takes it.
pub(crate) fn split_offset(offset_ns: i64) -> (i64, i64) {
    (
        offset_ns.div_euclid(NSEC_PER_SEC),
        offset_ns.rem_euclid(NSEC_PER_SEC),
    )
}

/// Selects and parameterizes a step or slew of the wall clock for the offset `theta` and the
/// uncertainty `epsilon`, both in nanoseconds: a step at or above the step threshold, and
/// otherwise a slew with the fast or slow time constant.
pub(crate) fn plan_correction(theta: i64, epsilon: i64, fast: bool) -> ClockAdjustment {
    let magnitude = theta.unsigned_abs();
    if magnitude >= STEP_THRESHOLD_NS {
        let (seconds, nanoseconds) = split_offset(theta);
        return ClockAdjustment::Step {
            seconds,
            nanoseconds,
        };
    }
    let ceil_us = |nanoseconds: i64| nanoseconds.saturating_add(NSEC_PER_USEC - 1) / NSEC_PER_USEC;
    ClockAdjustment::Slew {
        offset_ns: theta,
        time_constant: if fast {
            FAST_TIME_CONSTANT
        } else {
            SLOW_TIME_CONSTANT
        },
        // Below the step threshold, the magnitude fits an `i64`.
        maximum_error_us: ceil_us((magnitude as i64).saturating_add(epsilon)),
        estimated_error_us: ceil_us(epsilon),
    }
}

/// Kernel-message signature associated with one violation code.
struct WatchRule {
    /// Stable ABI violation code.
    code: ViolationCode,
    /// Substrings that must all occur.
    all: &'static [&'static str],
    /// Alternative substrings of which at least one must occur.
    any: &'static [&'static str],
}

/// Ordered kernel-message rules used by the runtime watcher.
const WATCH_RULES: &[WatchRule] = &[
    WatchRule {
        code: ViolationCode::TscUnstable,
        all: &["Marking TSC unstable due to "],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::ClocksourceUnstable,
        all: &[
            "timekeeping watchdog on CPU",
            "as unstable because the skew is too large",
        ],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::ClocksourceSwitch,
        all: &["clocksource: Switched to clocksource "],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::ClocksourceSkew,
        all: &["clocksource: "],
        any: &[" ahead of CPU ", " behind CPU "],
    },
    WatchRule {
        code: ViolationCode::TscWarp,
        all: &[],
        any: &[
            "TSC synchronization [CPU#",
            "cycles TSC warp between CPUs",
            "TSC warped randomly between CPUs",
        ],
    },
    WatchRule {
        code: ViolationCode::TscAdjust,
        all: &["TSC ADJUST"],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::RcuStall,
        all: &["rcu: INFO: "],
        any: &[
            " detected stalls on CPUs/tasks",
            " self-detected stall on CPU",
            " detected expedited stalls",
        ],
    },
    WatchRule {
        code: ViolationCode::RcuStarved,
        all: &["rcu: "],
        any: &[
            " kthread starved for ",
            " kthread timer wakeup didn't happen for ",
        ],
    },
    WatchRule {
        code: ViolationCode::SoftLockup,
        all: &["watchdog: BUG: soft lockup - CPU#"],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::HardLockup,
        all: &["Watchdog detected hard LOCKUP"],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::HungTask,
        all: &["INFO: task ", " blocked for more than "],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::UncheckedMsr,
        all: &["unchecked MSR access error"],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::ApicIdMismatch,
        all: &["APIC ID mismatch"],
        any: &[],
    },
];

/// Returns the first violation code matched by a kernel message.
pub(crate) fn match_watch_rule(message: &[u8]) -> Option<ViolationCode> {
    if message.starts_with(OWN_RECORD_PREFIX) {
        return None;
    }
    let contains = |part: &&str| kernel_text::contains(message, part.as_bytes());
    WATCH_RULES
        .iter()
        .find(|rule| {
            rule.all.iter().all(contains) && (rule.any.is_empty() || rule.any.iter().any(contains))
        })
        .map(|rule| rule.code)
}

/// Accumulated boot-log evidence for check C4, and the boot log's first W+X record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BootLog {
    /// Whether the Microsoft Hyper-V hypervisor line was observed.
    hypervisor: bool,
    /// Whether the required Hyper-V privilege flags were observed.
    privileges: bool,
    /// Whether the expected LAPIC frequency line was observed.
    lapic: bool,
    /// The clocksource that the last clocksource switch selected.
    last_switch: Option<Vec<u8>>,
    /// The diagnostic of the first forbidden or violation-bearing message.
    problem: Option<Vec<u8>>,
    /// The first message that reports an insecure writable and executable mapping.
    pub(crate) wx: Option<Vec<u8>>,
}

impl BootLog {
    /// Incorporates one kernel message into the accumulated verdict.
    pub(crate) fn add(&mut self, message: &[u8]) {
        const FORBIDDEN: [&[u8]; 4] = [
            b"Fast TSC calibration",
            b"Refined TSC clocksource calibration",
            b"kvm-clock",
            b"APIC timer: using supplied frequency",
        ];
        const LAPIC_RATES: [&[u8]; 2] = [
            b"Hyper-V: LAPIC Timer Frequency: 0x989680",
            b"Hyper-V: LAPIC Timer Frequency: 0x1e8480",
        ];
        const SWITCHED: &[u8] = b"clocksource: Switched to clocksource ";
        let own = message.starts_with(OWN_RECORD_PREFIX);
        let quoted = &message[..message.len().min(QUOTED_MESSAGE_MAX)];
        if self.wx.is_none()
            && !own
            && kernel_text::contains(message, b"Found insecure W+X mapping")
        {
            self.wx = Some(quoted.to_vec());
        }
        self.hypervisor |=
            kernel_text::contains(message, b"Hypervisor detected: Microsoft Hyper-V");
        self.privileges |= kernel_text::contains(message, b"Hyper-V: privilege flags low 0x8860,");
        self.lapic |= LAPIC_RATES.iter().any(|record| message.ends_with(record));
        if let Some(start) = kernel_text::find(message, SWITCHED) {
            let name = &message[start + SWITCHED.len()..];
            let name = name.split(|&byte| matches!(byte, b' ' | b'\n')).next();
            self.last_switch = name.map(<[u8]>::to_vec);
        }
        if self.problem.is_some() {
            return;
        }
        if let Some(code) = match_watch_rule(message)
            && code != ViolationCode::ClocksourceSwitch
        {
            self.problem = Some([format!("{code}: ").as_bytes(), quoted].concat());
        } else if !own
            && FORBIDDEN
                .iter()
                .any(|record| kernel_text::contains(message, record))
        {
            self.problem = Some([b"forbidden record: ".as_slice(), quoted].concat());
        }
    }

    /// Validates all accumulated boot-log requirements.
    pub(crate) fn verdict(&self) -> Result<(), Vec<u8>> {
        if let Some(problem) = &self.problem {
            Err(problem.clone())
        } else if !self.hypervisor {
            Err(b"no 'Hypervisor detected: Microsoft Hyper-V'".to_vec())
        } else if !self.privileges {
            Err(b"no 'Hyper-V: privilege flags low 0x8860,'".to_vec())
        } else if !self.lapic {
            Err(b"no 'Hyper-V: LAPIC Timer Frequency' of 1 GHz or 200 MHz".to_vec())
        } else if self.last_switch.as_deref() != Some(b"tsc") {
            Err([
                b"last clocksource switch is to '".as_slice(),
                self.last_switch.as_deref().unwrap_or_default(),
                b"', not tsc",
            ]
            .concat())
        } else {
            Ok(())
        }
    }
}

/// Scans every line of a boot log into one accumulated verdict. A `syslog(2)` log's lines start
/// with their priorities and timestamps.
pub(crate) fn scan_boot_log(text: &[u8], syslog: bool) -> BootLog {
    let mut log = BootLog::default();
    for line in text
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        log.add(if syslog {
            SyslogLine::parse(line).message
        } else {
            line
        });
    }
    log
}

/// Hyper-V CPUID leaves that the time ABI requires to read as zero: the leaves that follow the
/// identity leaves, through 0x4000000F, and the nested-virtualization leaves.
pub(crate) const ZERO_HYPERV_LEAVES: [RangeInclusive<u32>; 2] =
    [0x4000_0006..=0x4000_000f, 0x4000_0080..=0x4000_0082];

/// Returns whether the ABI explicitly requires `leaf` to read as zero.
pub(crate) fn explicit_zero_leaf(leaf: u32) -> bool {
    ZERO_HYPERV_LEAVES
        .iter()
        .any(|leaves| leaves.contains(&leaf))
}

/// Validates one CPUID leaf in an exhaustive sparse-range scan.
pub(crate) fn exhaustive_leaf_ok(
    leaf: u32,
    value: CpuidResult,
    out_of_range: CpuidResult,
) -> Result<(), String> {
    let explicit = explicit_zero_leaf(leaf);
    if value == cpuid::ZERO || (!explicit && value == out_of_range) {
        return Ok(());
    }
    Err(format!(
        "leaf 0x{leaf:08x} is {} (signature \"{}\"), expected zero{}",
        cpuid::hex_registers(value, " "),
        cpuid::signature(value),
        if explicit {
            ""
        } else {
            " or the highest basic leaf"
        }
    ))
}

/// Validates the broadcast device and the selected CPUs' tick devices of `/proc/timer_list`:
/// the broadcast device, if any, has no clock-event device, and each selected CPU ticks with a
/// one-shot LAPIC timer through the high-resolution timers.
pub(crate) fn timer_list_verdict(text: &[u8], selected: CpuMask) -> Result<(), String> {
    let list = TimerList::parse(text);
    if let Some(broadcast) = &list.broadcast
        && broadcast.device.as_deref() != Some("<NULL>")
    {
        return Err(format!(
            "broadcast clock event device is {}",
            broadcast.device.as_deref().unwrap_or_default()
        ));
    }
    for cpu in selected.iter() {
        let tick = list.tick_devices.get(&cpu);
        if tick.is_some_and(one_shot_lapic) {
            continue;
        }
        let missing = TickDevice::default();
        let details = tick.unwrap_or(&missing);
        return Err(format!(
            "cpu {cpu} tick device {} handler {} tick mode {} state {}",
            match tick {
                Some(tick) => tick.device.as_deref().unwrap_or_default(),
                None => "missing",
            },
            details.handler.as_deref().unwrap_or("none"),
            details.mode.unwrap_or(-1),
            details.state.unwrap_or(-1),
        ));
    }
    Ok(())
}

/// Returns whether a tick device is the LAPIC timer, in one-shot mode, with the
/// high-resolution timers' event handler.
fn one_shot_lapic(tick: &TickDevice) -> bool {
    tick.device.as_deref() == Some("lapic")
        && tick.handler.as_deref() == Some("hrtimer_interrupt")
        && tick.tick_mode() == Some(TickMode::Oneshot)
        && matches!(
            tick.clock_event_state(),
            Some(ClockEventState::Oneshot | ClockEventState::OneshotStopped)
        )
}

/// The CPU rates of `/proc/cpuinfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpuInfo {
    /// Each CPU's `cpu MHz`, in kHz rounded down, when the file gives it.
    pub(crate) khz: [Option<u64>; MAX_CPUS],
}

impl Default for CpuInfo {
    fn default() -> Self {
        Self {
            khz: [None; MAX_CPUS],
        }
    }
}

/// Parses a `cpu MHz` value, such as `2194.843`, into kHz, rounded down.
fn parse_khz(value: &str) -> Option<u64> {
    /// The decimals of a MHz value that make whole kHz.
    const DECIMALS: usize = 3;
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let fraction = fraction
        .bytes()
        .chain(std::iter::repeat(b'0'))
        .take(DECIMALS)
        .fold(0, |khz, digit| khz * 10 + u64::from(digit - b'0'));
    parse_decimal(whole.as_bytes())?
        .checked_mul(KHZ_PER_MHZ)?
        .checked_add(fraction)
}

/// Records every CPU's rate from `/proc/cpuinfo`, and, with `flags`, validates the selected
/// CPUs' feature flags and their presence. The first failure ends the scan.
pub(crate) fn check_cpuinfo(
    text: &[u8],
    selected: CpuMask,
    flags: bool,
) -> (CpuInfo, Result<(), String>) {
    const REQUIRED: [&str; 8] = [
        "tsc",
        "constant_tsc",
        "nonstop_tsc",
        "tsc_known_freq",
        "tsc_reliable",
        "rdtscp",
        "hypervisor",
        "arat",
    ];
    const FORBIDDEN: [&str; 2] = ["tsc_deadline_timer", "tsc_adjust"];
    let text = String::from_utf8_lossy(text);
    let mut info = CpuInfo::default();
    let mut seen = CpuMask::EMPTY;
    let mut cpu = None;
    for (key, value) in text.lines().filter_map(|line| line.split_once(':')) {
        match key.trim_end() {
            "processor" => {
                cpu = value
                    .trim()
                    .parse()
                    .ok()
                    .filter(|&cpu: &usize| cpu < MAX_CPUS);
                seen = cpu.map_or(seen, |cpu| seen.with(cpu));
            }
            "cpu MHz" => {
                if let Some(cpu) = cpu {
                    info.khz[cpu] = parse_khz(value.trim());
                }
            }
            "flags" if flags => {
                let Some(cpu) = cpu.filter(|&cpu| selected.contains(cpu)) else {
                    continue;
                };
                let has = |flag: &str| value.split_ascii_whitespace().any(|word| word == flag);
                if let Some(flag) = REQUIRED.into_iter().find(|&flag| !has(flag)) {
                    return (info, Err(format!("cpu {cpu} lacks the {flag} flag")));
                }
                if let Some(flag) = FORBIDDEN.into_iter().find(|&flag| has(flag)) {
                    return (info, Err(format!("cpu {cpu} has the {flag} flag")));
                }
            }
            _ => {}
        }
    }
    let missing = selected.iter().find(|&cpu| !seen.contains(cpu));
    match missing.filter(|_| flags) {
        Some(cpu) => (info, Err(format!("cpu {cpu} is not in /proc/cpuinfo"))),
        None => (info, Ok(())),
    }
}

/// Validates the kernel command line required by time ABI version one: the kernel must apply none
/// of the clock parameters that the ABI forbids among the parameters that it reads from the
/// command line, as [`cmdline::parameters`] reads them, in quotes, after any of its whitespace,
/// and up to a `--`.
///
/// The ABI forbids `tsc_early_khz`, `nolapic`, and `nolapic_timer`, whose `early_param` handlers
/// the kernel matches by their names, and `lapic_timer_hz`, the clock token that OpenVMM refuses
/// with `tsc_early_khz`, by its name too; `notsc`, whose `__setup` handler the kernel matches with
/// every parameter that starts with it; and `tsc=unstable`, an `hpet=` list with an option that
/// starts with `force`, as `hpet_setup` reads it, and a `clocksource=` other than `tsc`, whose
/// `__setup` handlers take the values. Names match whether they spell a dash or an underscore.
/// The diagnostic quotes the parameter as the kernel reads it, `NAME=VALUE` without its quotes.
///
/// `text` is what `/proc/cmdline` holds: the command line that the kernel read, and the newline
/// that procfs writes after it, which the check drops, so that a quote that the command line
/// leaves open holds the value that the kernel read.
pub(crate) fn check_cmdline(text: &[u8]) -> Result<(), Vec<u8>> {
    /// The forbidden parameters that the kernel, or OpenVMM for `lapic_timer_hz`, matches by name.
    const NAMES: [&str; 4] = [
        "tsc_early_khz",
        "lapic_timer_hz",
        "nolapic",
        "nolapic_timer",
    ];
    let forbidden = |parameter: &Parameter<'_>| {
        let value = |name: &str| parameter.value.filter(|_| parameter.is_named(name));
        NAMES.iter().any(|name| parameter.is_named(name))
            || parameter.name.starts_with(b"notsc")
            || value("tsc") == Some(b"unstable".as_slice())
            || value("hpet").is_some_and(|options| {
                options
                    .split(|&byte| byte == b',')
                    .any(|option| option.starts_with(b"force"))
            })
            || value("clocksource").is_some_and(|source| source != b"tsc")
    };
    let command_line = text.strip_suffix(b"\n").unwrap_or(text);
    match cmdline::parameters(command_line).find(forbidden) {
        Some(parameter) => {
            let mut quoted = parameter.name.to_vec();
            if let Some(value) = parameter.value {
                quoted.push(b'=');
                quoted.extend_from_slice(value);
            }
            quoted.truncate(QUOTED_TOKEN_MAX);
            Err([b"command line contains ".as_slice(), &quoted].concat())
        }
        None => Ok(()),
    }
}

/// Validates the current clocksource, the first line of `current`, and the available ones.
pub(crate) fn check_clocksource(current: &[u8], available: &[u8]) -> Result<(), Vec<u8>> {
    let current = current
        .split(|&byte| byte == b'\n')
        .next()
        .unwrap_or_default();
    if current != b"tsc" {
        return Err([b"current clocksource is ".as_slice(), current].concat());
    }
    let mut tsc = false;
    for name in available
        .split(|&byte| matches!(byte, b' ' | b'\n'))
        .filter(|name| !name.is_empty())
    {
        match name {
            b"tsc" => tsc = true,
            b"refined-jiffies" | b"jiffies" => {}
            _ => return Err([b"clocksource ".as_slice(), name, b" is available"].concat()),
        }
    }
    if tsc {
        Ok(())
    } else {
        Err(b"tsc is not an available clocksource".to_vec())
    }
}

/// Parses the private restore record that `restore-finish` writes: a `generation=N` line, and
/// optionally a `new_cpus=` line with the CPUs that the restore brought online, as
/// `--new-cpus` gives them.
pub(crate) fn parse_restore_record(text: &[u8]) -> Option<(u64, CpuMask)> {
    let mut lines = std::str::from_utf8(text).ok()?.lines();
    let generation = parse_decimal(lines.next()?.strip_prefix("generation=")?.as_bytes())?;
    let new_cpus = match lines.next() {
        Some(line) => CpuMask::parse_new(line.strip_prefix("new_cpus=")?.as_bytes())?,
        None => CpuMask::EMPTY,
    };
    lines.next().is_none().then_some((generation, new_cpus))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::protocol::cmdline::model;

    /// A valid packet header without memory ranges, of generation 1.
    fn valid_header() -> [u8; PACKET_HEADER_SIZE] {
        let mut bytes = [0; PACKET_HEADER_SIZE];
        bytes[..4].copy_from_slice(b"OVR\x04");
        bytes[8..12].copy_from_slice(&1_u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&1_u64.to_le_bytes());
        bytes
    }

    #[test]
    fn packets_decode_their_header_then_their_body() {
        let header = PacketHeader::parse(&valid_header()).unwrap();
        assert_eq!((header.generation, header.utc_ns), (1, 1));
        assert_eq!(header.body_size(), PACKET_ENTROPY_SIZE);
        let entropy: Vec<u8> = (0..64).collect();
        let packet = RestorePacket::parse(header, &entropy).unwrap();
        assert_eq!(packet.entropy.bytes().as_slice(), entropy);
        assert_eq!(packet.generation_id(), &entropy[..GENERATION_ID_SIZE]);
        assert_eq!(
            RestorePacket::parse(header, &entropy[1..]).unwrap_err(),
            PacketError::IncompleteBody
        );
        assert_eq!(
            PacketHeader::parse(&valid_header()[..31]),
            Err(PacketError::IncompleteHeader)
        );
    }

    #[test]
    fn packet_errors_name_the_invalid_field() {
        let mut bytes = valid_header();
        bytes[2] = b'X';
        let error = PacketHeader::parse(&bytes).unwrap_err();
        assert_eq!(error.to_string(), "restore packet magic is 4f5658");
        let mut bytes = valid_header();
        bytes[4] = 0x02;
        bytes[6] = 1;
        let header = PacketHeader::parse(&bytes).unwrap();
        let mut body = [0; PACKET_RANGE_SIZE + PACKET_ENTROPY_SIZE];
        body[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        body[8..16].copy_from_slice(&1_u64.to_le_bytes());
        let error = RestorePacket::parse(header, &body).unwrap_err();
        assert_eq!(error.to_string(), "restore memory range 0 is invalid");
    }

    #[test]
    fn samples_check_their_version_flags_and_time() {
        let mut sample = [0_u8; SAMPLE_SIZE];
        sample[0] = 1;
        sample[8] = 1;
        assert_eq!(TimeSample::parse(&sample).unwrap().utc_ns, 1);
        assert_eq!(
            TimeSample::parse(&sample[..15]),
            Err(TimeSampleError::Incomplete)
        );
        sample[2] = 1;
        assert_eq!(
            TimeSample::parse(&sample).unwrap_err().to_string(),
            "time sample flags 0x00 or reserved 0x0001"
        );
    }

    #[test]
    fn generation_ids_are_exact_hexadecimal() {
        assert_eq!(parse_hex::<2>(b"0aFf"), Some([0x0a, 0xff]));
        assert_eq!(parse_hex::<2>(b"0aF"), None);
        assert_eq!(parse_hex::<2>(b"+aff"), None);
        assert_eq!(parse_hex::<2>(b"0agf"), None);
        assert_eq!(format_hex(&[0x0a, 0xff]), "0aff");
    }

    #[test]
    fn correction_boundary_matches_the_abi() {
        assert!(matches!(
            plan_correction(128_000_000, 10, true),
            ClockAdjustment::Step { .. }
        ));
        assert!(matches!(
            plan_correction(127_999_999, 10, true),
            ClockAdjustment::Slew { .. }
        ));
        assert_eq!(
            plan_correction(-1_128_000_000, 0, false),
            ClockAdjustment::Step {
                seconds: -2,
                nanoseconds: 872_000_000,
            }
        );
    }

    #[test]
    fn offsets_split_toward_negative_infinity() {
        assert_eq!(split_offset(-1), (-1, 999_999_999));
        assert_eq!(split_offset(1_500_000_000), (1, 500_000_000));
        assert_eq!(split_offset(i64::MIN), (-9_223_372_037, 145_224_192));
    }

    #[test]
    fn extreme_clock_arithmetic_neither_wraps_nor_panics() {
        assert_eq!(pair_clocks(i64::MIN, i64::MAX, 0), (i64::MIN, 0));
        assert_eq!(
            plan_correction(i64::MIN, 0, true),
            ClockAdjustment::Step {
                seconds: -9_223_372_037,
                nanoseconds: 145_224_192,
            }
        );
        let slew = plan_correction(-5, i64::MAX, true);
        assert_eq!(
            slew,
            ClockAdjustment::Slew {
                offset_ns: -5,
                time_constant: 4,
                maximum_error_us: i64::MAX / 1000,
                estimated_error_us: i64::MAX / 1000,
            }
        );
        assert_eq!((slew.modes(), slew.status()), (0x203d, 0x2001));
    }

    #[test]
    fn watcher_ignores_its_own_event() {
        assert_eq!(
            match_watch_rule(b"unchecked MSR access error"),
            Some(ViolationCode::UncheckedMsr)
        );
        assert_eq!(
            match_watch_rule(b"NVX-TIME-ABI-VIOLATION: unchecked MSR access error"),
            None
        );
    }

    #[test]
    fn watcher_rules_match_linux_6_18_messages() {
        let messages = [
            (
                "tsc: Marking TSC unstable due to clocksource watchdog",
                Some(ViolationCode::TscUnstable),
            ),
            (
                "clocksource: timekeeping watchdog on CPU0: Marking clocksource \
                 'tsc-early' as unstable because the skew is too large:",
                Some(ViolationCode::ClocksourceUnstable),
            ),
            (
                "clocksource: Switched to clocksource refined-jiffies",
                Some(ViolationCode::ClocksourceSwitch),
            ),
            (
                "clocksource:         CPUs 1 ahead of CPU 0 for clocksource tsc.",
                Some(ViolationCode::ClocksourceSkew),
            ),
            (
                "clocksource:         CPUs 2-3 behind CPU 0 for clocksource tsc.",
                Some(ViolationCode::ClocksourceSkew),
            ),
            (
                "TSC synchronization [CPU#0 -> CPU#1]:",
                Some(ViolationCode::TscWarp),
            ),
            (
                "Measured 45 cycles TSC warp between CPUs, turning off TSC clock.",
                Some(ViolationCode::TscWarp),
            ),
            (
                "TSC warped randomly between CPUs",
                Some(ViolationCode::TscWarp),
            ),
            (
                "[Firmware Bug]: TSC ADJUST differs: CPU1 0 --> 100. Restoring",
                Some(ViolationCode::TscAdjust),
            ),
            (
                "rcu: INFO: rcu_preempt detected stalls on CPUs/tasks:",
                Some(ViolationCode::RcuStall),
            ),
            (
                "rcu: INFO: rcu_preempt self-detected stall on CPU",
                Some(ViolationCode::RcuStall),
            ),
            (
                "rcu: INFO: rcu_preempt detected expedited stalls on CPUs/tasks: { 1-... }",
                Some(ViolationCode::RcuStall),
            ),
            (
                "rcu: rcu_preempt kthread starved for 2603 jiffies! g345 f0x0",
                Some(ViolationCode::RcuStarved),
            ),
            (
                "rcu: rcu_preempt kthread timer wakeup didn't happen for 2602 jiffies! \
                 g345 f0x0",
                Some(ViolationCode::RcuStarved),
            ),
            (
                "watchdog: BUG: soft lockup - CPU#0 stuck for 22s! [sh:123]",
                Some(ViolationCode::SoftLockup),
            ),
            (
                "watchdog: Watchdog detected hard LOCKUP on cpu 1",
                Some(ViolationCode::HardLockup),
            ),
            (
                "INFO: task kworker/0:1:12 blocked for more than 120 seconds.",
                Some(ViolationCode::HungTask),
            ),
            (
                "unchecked MSR access error: RDMSR from 0x1ad at rIP: \
                 0xffffffff816f11ee (__rdmsr_on_cpu+0x1e/0x30)",
                Some(ViolationCode::UncheckedMsr),
            ),
            (
                "[Firmware Bug]: CPU   1: APIC ID mismatch. CPUID: 0x0000 APIC: 0x0001",
                Some(ViolationCode::ApicIdMismatch),
            ),
            (
                "clocksource: tsc: mask: 0xffffffffffffffff max_cycles: 0x1fa3",
                None,
            ),
            ("Hyper-V: LAPIC Timer Frequency: 0x1e8480", None),
            ("rcu: Hierarchical RCU implementation.", None),
            (
                "NVX-TIME-ABI-VIOLATION: v=1 code=G_UNCHECKED_MSR \
                 detail=\"unchecked MSR access error\"",
                None,
            ),
        ];
        for (message, expected) in messages {
            assert_eq!(match_watch_rule(message.as_bytes()), expected, "{message}");
        }
    }

    #[test]
    fn boot_log_reads_syslog_lines() {
        let log = scan_boot_log(
            b"<6>[    0.000000] Hypervisor detected: Microsoft Hyper-V\n\
              <6>[    0.000000] Hyper-V: privilege flags low 0x8860, high 0x0\n\
              <6>[    0.000000] Hyper-V: LAPIC Timer Frequency: 0x1e8480\n\
              <6>[    1.000000] clocksource: Switched to clocksource tsc\n\
              <4>[    1.500000] x86/mm: Found insecure W+X mapping at address 0x1\n",
            true,
        );
        assert_eq!(log.verdict(), Ok(()));
        assert_eq!(
            log.wx.as_deref(),
            Some(b"x86/mm: Found insecure W+X mapping at address 0x1".as_slice())
        );
        let log = scan_boot_log(b"clocksource: Switched to clocksource hpet\n", false);
        assert_eq!(
            log.verdict(),
            Err(b"no 'Hypervisor detected: Microsoft Hyper-V'".to_vec())
        );
    }

    #[test]
    fn boot_log_requires_the_identity_and_a_final_switch_to_tsc() {
        const BOOT_LOG: &str = "Hypervisor detected: Microsoft Hyper-V\n\
            Hyper-V: privilege flags low 0x8860, high 0x0, ext 0x0, hints 0x0, misc 0x100\n\
            Hyper-V: LAPIC Timer Frequency: 0x1e8480\n\
            tsc: Detected 2194.843 MHz processor\n\
            clocksource: Switched to clocksource tsc-early\n\
            clocksource: Switched to clocksource tsc\n";
        assert_eq!(scan_boot_log(BOOT_LOG.as_bytes(), false).verdict(), Ok(()));
        assert_eq!(
            scan_boot_log(BOOT_LOG.replace("0x1e8480", "0x989680").as_bytes(), false).verdict(),
            Ok(())
        );
        for (name, log) in [
            (
                "missing identity",
                BOOT_LOG.lines().skip(1).collect::<Vec<_>>().join("\n"),
            ),
            ("wrong LAPIC rate", BOOT_LOG.replace("0x1e8480", "0x1e8481")),
            (
                "last switch",
                format!("{BOOT_LOG}clocksource: Switched to clocksource refined-jiffies\n"),
            ),
            (
                "kvm-clock",
                format!("{BOOT_LOG}kvm-clock: Using msrs 4b564d01\n"),
            ),
            (
                "refinement",
                format!("{BOOT_LOG}tsc: Refined TSC clocksource calibration: 2194.843 MHz\n"),
            ),
            (
                "lapic token",
                format!("{BOOT_LOG}APIC timer: using supplied frequency 200000000 Hz\n"),
            ),
            (
                "watcher record",
                format!("{BOOT_LOG}tsc: Marking TSC unstable due to clocksource watchdog\n"),
            ),
        ] {
            assert!(
                scan_boot_log(log.as_bytes(), false).verdict().is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn boot_log_reads_syslog_read_all_lines() {
        let lines = concat!(
            "<6>[    0.000000] Hypervisor detected: Microsoft Hyper-V\n",
            "<6>[    0.123456] [drm] a bracketed message is kept\n",
            "<6>[    1.007919] Hyper-V: privilege flags low 0x8860, high 0x0, ",
            "ext 0x0, hints 0x0, misc 0x100\n",
            "<6>[    2.015838] Hyper-V: LAPIC Timer Frequency: 0x1e8480\n",
            "<6>[    3.023757] tsc: Detected 2194.843 MHz processor\n",
            "<6>[    4.031676] clocksource: Switched to clocksource tsc-early\n",
            "<6>[    5.039595] clocksource: Switched to clocksource tsc\n"
        );
        assert_eq!(scan_boot_log(lines.as_bytes(), true).verdict(), Ok(()));
        let own = concat!(
            "<2>[    3.000000] NVX-TIME-REPORT-VIOLATION: v=1 ",
            "code=G_CONFORMANCE_C4 detail=\"forbidden record: kvm-clock: Using msrs\"\n"
        );
        assert_eq!(
            scan_boot_log(format!("{lines}{own}").as_bytes(), true).verdict(),
            Ok(())
        );
        assert!(
            scan_boot_log(
                format!("{lines}<6>[    4.000000] kvm-clock: Using msrs\n").as_bytes(),
                true
            )
            .verdict()
            .unwrap_err()
            .starts_with(b"forbidden record: kvm-clock")
        );
        assert!(
            scan_boot_log(
                lines
                    .lines()
                    .skip(2)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .as_bytes(),
                true
            )
            .verdict()
            .unwrap_err()
            .starts_with(b"no 'Hypervisor")
        );
    }

    #[test]
    fn boot_log_reports_a_kernel_wx_mapping_apart_from_c4() {
        const BOOT_LOG: &str = "<6>[    0.100000] Hypervisor detected: Microsoft Hyper-V\n\
            <6>[    0.100000] Hyper-V: privilege flags low 0x8860, high 0x0, ext 0x0\n\
            <6>[    0.100000] Hyper-V: LAPIC Timer Frequency: 0x1e8480\n\
            <6>[    0.100000] clocksource: Switched to clocksource tsc\n";
        const WX: &str = "<4>[    0.412345] ------------[ cut here ]------------\n\
            <4>[    0.412346] x86/mm: Found insecure W+X mapping at address 0xffffffffc0000000\n\
            <4>[    0.412400] WARNING: CPU: 0 PID: 1 at \
            arch/x86/mm/dump_pagetables.c:246 note_wx+0x5a/0x70\n\
            <6>[    0.412500] x86/mm: Checked W+X mappings: FAILED, 4 W+X pages found.\n";
        let log = scan_boot_log(format!("{WX}{BOOT_LOG}").as_bytes(), true);
        assert_eq!(log.verdict(), Ok(()));
        assert_eq!(
            log.wx.as_deref(),
            Some(b"x86/mm: Found insecure W+X mapping at address 0xffffffffc0000000".as_slice())
        );
        let passed = scan_boot_log(
            format!(
                "{BOOT_LOG}<6>[    0.412500] x86/mm: Checked W+X mappings: passed, \
                 no W+X pages found.\n"
            )
            .as_bytes(),
            true,
        );
        assert_eq!((passed.verdict(), passed.wx), (Ok(()), None));
        let missing = scan_boot_log(
            format!(
                "{WX}<6>[    0.100000] Hyper-V: privilege flags low 0x8860, high 0x0\n\
                 <6>[    0.100000] Hyper-V: LAPIC Timer Frequency: 0x1e8480\n\
                 <6>[    0.100000] clocksource: Switched to clocksource tsc\n"
            )
            .as_bytes(),
            true,
        );
        assert!(
            missing
                .verdict()
                .unwrap_err()
                .starts_with(b"no 'Hypervisor")
        );
        assert!(missing.wx.is_some());
        let own = concat!(
            "<2>[    3.000000] NVX-TIME-REPORT-VIOLATION: v=1 code=G_KERNEL_WX ",
            "detail=\"x86/mm: Found insecure W+X mapping at address 0x0\"\n"
        );
        let ignored = scan_boot_log(format!("{BOOT_LOG}{own}").as_bytes(), true);
        assert_eq!((ignored.verdict(), ignored.wx), (Ok(()), None));
    }

    #[test]
    fn timer_list_requires_one_shot_lapic_ticks() {
        let list = b"Tick Device: mode:     1\n\
                     Broadcast device\n\
                     Clock Event Device: <NULL>\n\
                     \n\
                     Tick Device: mode:     1\n\
                     Per CPU device: 0\n\
                     Clock Event Device: lapic\n \
                     mode:           3\n \
                     event_handler:  hrtimer_interrupt\n";
        assert_eq!(timer_list_verdict(list, CpuMask::EMPTY.with(0)), Ok(()));
        assert_eq!(
            timer_list_verdict(list, CpuMask::EMPTY.with(1)),
            Err("cpu 1 tick device missing handler none tick mode -1 state -1".to_owned())
        );
    }

    #[test]
    fn timer_list_covers_two_cpu_linux_6_18_variants() {
        fn cpu(cpu: usize, device: &str, state: i32, handler: &str, mode: i32) -> String {
            format!(
                "Tick Device: mode:     {mode}\n\
                 Per CPU device: {cpu}\n\
                 Clock Event Device: {device}\n\
                  max_delta_ns:   1374389534634\n\
                  min_delta_ns:   1000\n\
                  mult:           6710886\n\
                  shift:          26\n\
                  mode:           {state}\n\
                  next_event:     1342000000 nsecs\n\
                  set_next_event: lapic_next_event\n\
                  shutdown:       lapic_timer_shutdown\n\
                  periodic:       lapic_timer_set_periodic\n\
                  oneshot:        lapic_timer_set_oneshot\n\
                  oneshot stopped: lapic_timer_shutdown\n\
                  event_handler:  {handler}\n\
                  retries:        0\n\n"
            )
        }
        fn list(broadcast: &str, second: Option<(&str, i32, &str, i32)>) -> String {
            let mut text = format!(
                "Tick Device: mode:     1\n\
                 Broadcast device\n\
                 Clock Event Device: {broadcast}\n\
                 tick_broadcast_mask: 00\n\
                 tick_broadcast_oneshot_mask: 00\n\n"
            );
            text.push_str(&cpu(0, "lapic", 3, "hrtimer_interrupt", 1));
            if let Some((device, state, handler, mode)) = second {
                text.push_str(&cpu(1, device, state, handler, mode));
            }
            text
        }
        let selected = CpuMask::EMPTY.with(0).with(1);
        assert_eq!(
            timer_list_verdict(
                list("<NULL>", Some(("lapic", 4, "hrtimer_interrupt", 1))).as_bytes(),
                selected
            ),
            Ok(())
        );
        for (name, text) in [
            (
                "deadline",
                list(
                    "<NULL>",
                    Some(("lapic-deadline", 3, "hrtimer_interrupt", 1)),
                ),
            ),
            (
                "periodic tick",
                list("<NULL>", Some(("lapic", 3, "hrtimer_interrupt", 0))),
            ),
            (
                "periodic state",
                list("<NULL>", Some(("lapic", 2, "hrtimer_interrupt", 1))),
            ),
            (
                "handler",
                list("<NULL>", Some(("lapic", 3, "tick_handle_periodic", 1))),
            ),
            ("missing cpu", list("<NULL>", None)),
            (
                "broadcast",
                list("pit", Some(("lapic", 4, "hrtimer_interrupt", 1))),
            ),
        ] {
            assert!(
                timer_list_verdict(text.as_bytes(), selected).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn cpuinfo_rates_round_down_to_khz() {
        assert_eq!(parse_khz("2194.8439"), Some(2_194_843));
        assert_eq!(parse_khz("2194.8"), Some(2_194_800));
        assert_eq!(parse_khz("2194"), Some(2_194_000));
        assert_eq!(parse_khz("2194.8x"), None);
        let (info, verdict) = check_cpuinfo(
            b"processor\t: 1\ncpu MHz\t\t: 2194.843\n",
            CpuMask::EMPTY.with(0),
            true,
        );
        assert_eq!((info.khz[0], info.khz[1]), (None, Some(2_194_843)));
        assert_eq!(verdict, Err("cpu 0 is not in /proc/cpuinfo".to_owned()));

        const FLAGS: &str = "tsc msr pae constant_tsc rdtscp nonstop_tsc tsc_known_freq \
                             tsc_reliable hypervisor arat x2apic";
        let fixture = |flags: &str, cpus: usize| {
            (0..cpus)
                .map(|cpu| {
                    format!(
                        "processor\t: {cpu}\n\
                         vendor_id\t: GenuineIntel\n\
                         cpu family\t: 6\n\
                         model\t\t: 85\n\
                         cpu MHz\t\t: 2194.843\n\
                         flags\t\t: fpu vme {flags}\n\
                         bugs\t\t: spectre_v1\n\n"
                    )
                })
                .collect::<String>()
        };
        let selected = CpuMask::EMPTY.with(0).with(1);
        let (info, verdict) = check_cpuinfo(fixture(FLAGS, 2).as_bytes(), selected, true);
        assert_eq!(verdict, Ok(()));
        assert_eq!(
            (info.khz[0], info.khz[1]),
            (Some(2_194_843), Some(2_194_843))
        );
        for (name, flags) in [
            ("tsc_reliable", FLAGS.replace("tsc_reliable ", "")),
            ("deadline", format!("{FLAGS} tsc_deadline_timer")),
            ("adjust", format!("{FLAGS} tsc_adjust")),
        ] {
            assert!(
                check_cpuinfo(fixture(&flags, 2).as_bytes(), selected, true)
                    .1
                    .is_err(),
                "{name}"
            );
        }
        assert!(
            check_cpuinfo(fixture(FLAGS, 1).as_bytes(), selected, true)
                .1
                .is_err()
        );
    }

    #[test]
    fn restore_record_has_a_generation_and_optional_new_cpus() {
        let cpus = CpuMask::parse(b"4-6,9").unwrap();
        assert_eq!(
            parse_restore_record(b"generation=3\nnew_cpus=4-6,9\n"),
            Some((3, cpus))
        );
        assert_eq!(
            parse_restore_record(b"generation=18446744073709551615\nnew_cpus=none\n"),
            Some((u64::MAX, CpuMask::EMPTY))
        );
        assert_eq!(
            parse_restore_record(b"generation=3\n"),
            Some((3, CpuMask::EMPTY))
        );
        for record in [
            &b""[..],
            &b"new_cpus=4\ngeneration=3\n"[..],
            b"generation=3 new_cpus=1",
            b"generation=3\nnew_cpus=7-4\n",
            b"generation=3\nnew_cpus= 1-2\n",
            b"generation=3\nnew_cpus=64\n",
            b"generation=3\nnew_cpus=\ntrailing=value\n",
        ] {
            assert_eq!(parse_restore_record(record), None, "{record:?}");
        }
    }

    #[test]
    fn the_command_line_is_checked_as_the_kernel_reads_it() {
        // The kernel applies these parameters however the command line separates, quotes, or
        // spells them: `à` is C3 A0 in UTF-8, and the kernel takes the byte 0xa0, as it does the
        // vertical tab, for whitespace (microsoft/nvx#723).
        let cases: [(&[u8], &[u8]); 21] = [
            (b"quiet \xc3\xa0tsc=unstable", b"tsc=unstable"),
            (b"quiet\xc3\xa0tsc=unstable", b"tsc=unstable"),
            (b"quiet\x0btsc_early_khz=2100000", b"tsc_early_khz=2100000"),
            (b"quiet\xa0notsc", b"notsc"),
            (b"\"tsc=unstable\"", b"tsc=unstable"),
            (b"tsc=\"unstable\"", b"tsc=unstable"),
            (b"a \"clocksource=hpet\" b", b"clocksource=hpet"),
            (b"tsc-early-khz=2100000", b"tsc-early-khz=2100000"),
            (b"lapic-timer-hz=200000000", b"lapic-timer-hz=200000000"),
            (b"nolapic-timer", b"nolapic-timer"),
            (b"nolapic=1", b"nolapic=1"),
            (b"tsc_early_khz", b"tsc_early_khz"),
            // `__setup("notsc", ...)` takes every parameter that starts with it, and
            // `hpet_setup` every option of its list that starts with `force`.
            (b"notsc=1", b"notsc=1"),
            (b"notscx", b"notscx"),
            (b"hpet=verbose,force", b"hpet=verbose,force"),
            (b"hpet=forced", b"hpet=forced"),
            (b"clocksource=tsc clocksource=hpet", b"clocksource=hpet"),
            (b"clocksource=", b"clocksource="),
            (b"nolapic_timer\n", b"nolapic_timer"),
            (b"hpet=force\0", b"hpet=force"),
            // The newline that procfs writes after the command line is not in a quote that the
            // command line leaves open.
            (b"quiet tsc=\"unstable\n", b"tsc=unstable"),
        ];
        for (command_line, quoted) in cases {
            assert_eq!(
                check_cmdline(command_line).unwrap_err(),
                [b"command line contains ".as_slice(), quoted].concat(),
                "{}",
                command_line.escape_ascii()
            );
        }
        // The kernel leaves what follows a `--` to init, reads text in quotes as a value of
        // another parameter, and applies these parameters to no clock.
        for command_line in [
            &b"earlycon=xe9 console=hvc0 quiet clocksource=tsc tsc=reliable\n"[..],
            b"quiet -- tsc=unstable notsc",
            b"quiet\xa0--\xa0tsc=unstable",
            b"a=\"tsc=unstable notsc\" b",
            b"xnotsc tscx=unstable tsc=unstable2 hpet=disable,verbose clocksource nolapicx",
            b"quiet\xe2\x80\x83tsc=unstable",
            b"quiet\0 tsc=unstable",
            b"quiet clocksource=\"tsc\n",
            b"",
        ] {
            assert_eq!(
                check_cmdline(command_line),
                Ok(()),
                "{}",
                command_line.escape_ascii()
            );
        }
        let long = [b"notsc".as_slice(), &[b'x'; QUOTED_TOKEN_MAX]].concat();
        assert_eq!(
            check_cmdline(&long).unwrap_err(),
            [
                b"command line contains ".as_slice(),
                &long[..QUOTED_TOKEN_MAX]
            ]
            .concat()
        );
    }

    /// Returns whether the kernel applies, in the parameter `name` with `value`, a clock
    /// parameter that time ABI v1 forbids, as `init/main.c` matches parameters with their
    /// handlers: an `early_param` by `parameq` of its name, which NVX applies to
    /// `lapic_timer_hz` too, and a `__setup` string by `parameqn` of the parameter's text over the
    /// string's length, which passes the rest of the text to the handler.
    fn applies_forbidden_clock(name: &[u8], value: Option<&[u8]>) -> bool {
        let line = model::line(name, value);
        let early = |string: &str| model::parameq(name, string.as_bytes());
        let setup = |string: &str| {
            model::parameqn(&line, string.as_bytes(), string.len()).then(|| &line[string.len()..])
        };
        [
            "tsc_early_khz",
            "lapic_timer_hz",
            "nolapic",
            "nolapic_timer",
        ]
        .into_iter()
        .any(early)
            || setup("notsc").is_some()
            || setup("tsc=") == Some(b"unstable".as_slice())
            || setup("hpet=").is_some_and(|argument| {
                argument
                    .split(|&byte| byte == b',')
                    .any(|option| option.starts_with(b"force"))
            })
            || setup("clocksource=").is_some_and(|argument| argument != b"tsc")
    }

    /// Returns a strategy for command lines of the clock parameters, others, their values,
    /// quotes, the `--` delimiter, and the kernel's whitespace.
    fn clock_command_line() -> impl Strategy<Value = Vec<u8>> {
        /// The pieces of the command lines.
        const PIECES: [&[u8]; 29] = [
            b"tsc",
            b"notsc",
            b"nolapic",
            b"_timer",
            b"-timer",
            b"tsc_early_khz",
            b"tsc-early",
            b"_khz",
            b"lapic_timer_hz",
            b"hpet",
            b"clocksource",
            b"x",
            b"=",
            b"unstable",
            b"reliable",
            b"force",
            b",",
            b"verbose",
            b"1",
            b"\"",
            b"--",
            b"-",
            b"_",
            b" ",
            b"\t",
            b"\n",
            b"\x0b",
            b"\xa0",
            b"\xc3\xa0",
        ];
        prop::collection::vec(prop::sample::select(PIECES.to_vec()), 0..16)
            .prop_map(|pieces| pieces.concat())
    }

    proptest! {
        /// The check of what `/proc/cmdline` holds, a command line and the newline that procfs
        /// writes after it, fails exactly when the kernel applies, in the command line, a clock
        /// parameter that the ABI forbids.
        #[test]
        fn the_check_fails_when_the_kernel_applies_a_forbidden_clock(
            command_line in clock_command_line(),
        ) {
            let applied = model::read(&command_line)
                .parameters
                .iter()
                .any(|(name, value)| applies_forbidden_clock(name, value.as_deref()));
            prop_assert_eq!(
                check_cmdline(&[command_line.as_slice(), b"\n"].concat()).is_err(),
                applied,
                "{}",
                command_line.escape_ascii()
            );
        }
    }
}
