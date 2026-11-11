//! Pure decoders, parsers, and arithmetic of the guest time ABI.

use std::fmt::{self, Display, Write as _};
use std::net::IpAddr;
use std::ops::RangeInclusive;

use super::api::RestoreNetwork;
use super::cpu_mask::CpuMask;
use super::events::ViolationCode;
pub(crate) use super::events::match_watch_rule;
pub(crate) use super::sample::{HostClock, frequency_to_ppb, plan_correction};
use super::state::HostClockStatus;
use super::{MAX_CPUS, NSEC_PER_SEC};
use crate::guest::kernel_text::{
    self, ClockEventState, SyslogLine, TickDevice, TickMode, TimerList,
};
use crate::guest::parse::parse_decimal;
use crate::guest::reseed::Seed;
use crate::guest::sys::x86::{CpuidResult, cpuid};
use crate::protocol::cmdline::{self, Parameter};

/// Fixed restore-packet header length.
pub(crate) const PACKET_HEADER_SIZE: usize = 24;
/// Encoded length of one memory range.
pub(crate) const PACKET_RANGE_SIZE: usize = 16;
/// Entropy payload length appended to every restore packet: a restore's [`Seed`].
pub(crate) const PACKET_ENTROPY_SIZE: usize = 64;
// The packet's entropy is decoded straight into a seed.
const _: () = assert!(PACKET_ENTROPY_SIZE == Seed::LENGTH);
/// Fixed host time-sample record length.
pub(crate) const SAMPLE_SIZE: usize = super::sample::SIZE;
/// VM generation identifier length.
pub(crate) const GENERATION_ID_SIZE: usize = 16;

/// The magic number that starts every restore packet.
const PACKET_MAGIC: [u8; 3] = *b"OVR";
/// The restore-packet version,
/// [`RESTORE_PACKET_VERSION`](crate::contract::RESTORE_PACKET_VERSION), which a template's
/// compatibility key records.
const PACKET_VERSION: u8 = {
    let version = crate::contract::RESTORE_PACKET_VERSION;
    assert!(version <= u8::MAX as u64);
    version as u8
};
/// The bits of the network byte that give the DNS server's address family.
const PACKET_DNS_MASK: u8 = 0x03;
/// The network byte's DNS server family of an IPv4 server.
const PACKET_DNS_IPV4: u8 = 0x01;
/// The network byte's DNS server family of an IPv6 server.
const PACKET_DNS_IPV6: u8 = 0x02;
/// The network byte's bit that asks the guest to announce its listeners.
const PACKET_ANNOUNCE_LISTENERS: u8 = 0x04;
/// Mask of the network byte's bits that the packet defines.
const PACKET_NETWORK_KNOWN: u8 = PACKET_DNS_MASK | PACKET_ANNOUNCE_LISTENERS;
/// Encoded length of the DNS server, which follows the entropy: an IPv6 address, an IPv4 address
/// and twelve zero bytes, or zeros without a DNS server.
pub(crate) const PACKET_DNS_SIZE: usize = 16;
/// Restore flag indicating that the downtime came from the host's UTC clock.
const PACKET_DOWNTIME_UTC: u8 = 0x01;
/// Restore flag indicating that memory ranges follow.
pub(crate) const PACKET_MEMORY_TARGET: u8 = 0x02;
/// Restore flag requiring a snapshot acknowledgement.
pub(crate) const PACKET_ACK_REQUIRED: u8 = 0x04;
/// Restore flag indicating that the network byte and the DNS server carry network settings, as
/// at a restore that replaces the network policy of a snapshot without a tier. Without it, both
/// are zero.
pub(crate) const PACKET_NETWORK: u8 = 0x10;
/// Restore flag indicating that the packet ends with a platform clone's activation record: its
/// length, then its bytes.
pub(crate) const PACKET_ACTIVATION: u8 = 0x20;
/// Mask of restore flags defined by the ABI.
const PACKET_KNOWN_FLAGS: u8 = 0x3f;
/// Encoded length of the activation record's length, a little-endian `u32`, which follows the DNS
/// server in a packet with an [activation record](PACKET_ACTIVATION), and keeps the record's bytes
/// aligned to the guest's four-byte reads.
pub(crate) const PACKET_ACTIVATION_LENGTH_SIZE: usize = 4;
/// The most bytes of a packet's activation record: as many as a record's encoding holds,
/// [`MAX_RECORD_BYTES`](crate::activation::MAX_RECORD_BYTES).
pub(crate) const PACKET_ACTIVATION_MAX: usize = crate::activation::MAX_RECORD_BYTES;
/// The online VP counts that a restore packet may give.
const PACKET_ONLINE_VP_COUNTS: [u8; 5] = [0, 1, 2, 4, 8];
/// Time-sample flag reserved for host test hooks.
const SAMPLE_TEST_HOOKS: u8 = 0x08;
/// Maximum signed restore rate deviation, equivalent to 250 ppm.
const RATE_DEVIATION_LIMIT: i32 = 16_384_000;
/// Hz in a kHz, the unit to which Linux reads the TSC rate from the frequency MSR.
const HZ_PER_KHZ: u64 = 1000;
/// Seconds in a day.
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
/// Maximum accepted downtime, inclusive: 365 days, the fixed duration of 31,536,000 seconds
/// rather than a calendar year, which OpenVMM's time ABI also accepts.
const DOWNTIME_LIMIT_NS: u64 = 365 * SECONDS_PER_DAY * NSEC_PER_SEC as u64;
/// kHz in a MHz, which scale the whole MHz of `/proc/cpuinfo`.
const KHZ_PER_MHZ: u64 = 1000;
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
    /// The version is not [`PACKET_VERSION`].
    Version(u8),
    /// The flags set reserved bits.
    ReservedFlags(u8),
    /// The online VP count is not 0, 1, 2, 4, or 8.
    OnlineVpCount(u8),
    /// Memory ranges follow, but the flags select no memory target.
    RangesWithoutTarget(u8),
    /// The network byte of a packet without network settings is not zero.
    ReservedByte(u8),
    /// The network byte sets reserved bits, or names no DNS server family that the packet
    /// defines.
    NetworkByte(u8),
    /// The generation is zero.
    ZeroGeneration,
    /// The rate deviation exceeds 250 ppm.
    RateDeviation(i32),
    /// The downtime exceeds 365 days.
    Downtime(u64),
    /// The packet ends before the memory ranges, the entropy, and the DNS server that its header
    /// gives.
    IncompleteBody,
    /// The memory range at this index is empty, or ends past the address space.
    Range(usize),
    /// The DNS server is unspecified, pads an IPv4 address with a byte that is not zero, or is
    /// not zero when the network byte names no DNS server or the packet carries no network
    /// settings.
    DnsServer,
    /// The length of the packet's activation record is 0, or more than
    /// [`PACKET_ACTIVATION_MAX`].
    ActivationLength(u32),
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
            Self::ReservedByte(byte) => write!(
                formatter,
                "restore packet network byte is {byte} without network settings"
            ),
            Self::NetworkByte(byte) => write!(
                formatter,
                "restore packet network byte 0x{byte:02x} is invalid"
            ),
            Self::ZeroGeneration => formatter.write_str("restore packet generation is 0"),
            Self::RateDeviation(deviation) => write!(
                formatter,
                "restore packet rate deviation {deviation} exceeds the 250 ppm tolerance"
            ),
            Self::Downtime(downtime) => write!(
                formatter,
                "restore packet downtime {downtime} ns exceeds the 365-day limit"
            ),
            Self::IncompleteBody => formatter.write_str("restore packet body is incomplete"),
            Self::Range(index) => write!(formatter, "restore memory range {index} is invalid"),
            Self::DnsServer => formatter.write_str("restore packet DNS server is invalid"),
            Self::ActivationLength(length) => write!(
                formatter,
                "restore packet activation record length {length} is not 1 to \
                 {PACKET_ACTIVATION_MAX}"
            ),
        }
    }
}

impl std::error::Error for PacketError {}

/// The validated header of a restore packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PacketHeader {
    /// Packet behavior flags.
    pub(crate) flags: u8,
    /// Number of online virtual processors reported by the VMM.
    pub(crate) online_vp_count: u8,
    /// Number of following memory ranges.
    pub(crate) range_count: u8,
    /// The network byte of a packet that [carries network settings](PACKET_NETWORK), or `None`
    /// for one that carries none.
    pub(crate) network: Option<u8>,
    /// New VM generation.
    pub(crate) generation: u32,
    /// Host rate deviation in signed scaled parts per million.
    pub(crate) rate_deviation: i32,
    /// Restore downtime in nanoseconds.
    pub(crate) downtime_ns: u64,
}

impl PacketHeader {
    /// Decodes and validates the header at the start of `bytes`.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, PacketError> {
        let mut fields = Fields::new(bytes, PacketError::IncompleteHeader);
        let magic: [u8; 3] = fields.bytes()?;
        let [version, flags, online_vp_count, range_count, network] = fields.bytes()?;
        let header = Self {
            flags,
            online_vp_count,
            range_count,
            network: (flags & PACKET_NETWORK != 0).then_some(network),
            generation: fields.u32()?,
            rate_deviation: fields.i32()?,
            downtime_ns: fields.u64()?,
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
        match header.network {
            None if network != 0 => return Err(PacketError::ReservedByte(network)),
            Some(network)
                if network & !PACKET_NETWORK_KNOWN != 0
                    || network & PACKET_DNS_MASK == PACKET_DNS_MASK =>
            {
                return Err(PacketError::NetworkByte(network));
            }
            _ => {}
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
        Ok(header)
    }

    /// Returns the length of the body that follows the header: the memory ranges, the entropy,
    /// the DNS server, and the length of the packet's activation record, if it has one, whose
    /// bytes follow the body.
    pub(crate) fn body_size(&self) -> usize {
        let activation = if self.activation() {
            PACKET_ACTIVATION_LENGTH_SIZE
        } else {
            0
        };
        usize::from(self.range_count) * PACKET_RANGE_SIZE
            + PACKET_ENTROPY_SIZE
            + PACKET_DNS_SIZE
            + activation
    }

    /// Returns whether the packet ends with a platform clone's activation record.
    pub(crate) const fn activation(&self) -> bool {
        self.flags & PACKET_ACTIVATION != 0
    }

    /// Returns whether the packet asks the guest to apply network settings: a DNS server, or the
    /// announcement of its listeners.
    pub(crate) const fn network_repair(&self) -> bool {
        matches!(self.network, Some(network) if network != 0)
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

/// A validated restore packet.
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
    /// The network settings of a packet that carries them.
    pub(crate) network: Option<RestoreNetwork>,
    /// The activation record that the packet ends with, if any: its bytes, undecoded.
    pub(crate) activation: Option<Vec<u8>>,
}

impl RestorePacket {
    /// Decodes and validates the memory ranges, the entropy, and the DNS server that follow
    /// `header` in `body`, and, for a packet with an activation record, the record's length,
    /// with which `read_record` reads the record's bytes, which follow the body.
    pub(crate) fn parse(
        header: PacketHeader,
        body: &[u8],
        read_record: impl FnOnce(usize) -> Vec<u8>,
    ) -> Result<Self, PacketError> {
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
        let entropy = fields.chunk()?;
        let dns_server: [u8; PACKET_DNS_SIZE] = fields.bytes()?;
        let network = match header.network {
            Some(network) => Some(RestoreNetwork::new(
                parse_dns_server(network, dns_server)?,
                network & PACKET_ANNOUNCE_LISTENERS != 0,
            )),
            None if dns_server == [0; PACKET_DNS_SIZE] => None,
            None => return Err(PacketError::DnsServer),
        };
        let activation = if header.activation() {
            let length = fields.u32()?;
            let size = usize::try_from(length)
                .ok()
                .filter(|size| (1..=PACKET_ACTIVATION_MAX).contains(size))
                .ok_or(PacketError::ActivationLength(length))?;
            Some(read_record(size))
        } else {
            None
        };
        Ok(Self {
            header,
            ranges,
            entropy: Seed::copy_from(entropy),
            network,
            activation,
        })
    }

    /// Returns the new VM generation ID, which leads the entropy.
    pub(crate) fn generation_id(&self) -> &[u8] {
        &self.entropy.bytes()[..GENERATION_ID_SIZE]
    }
}

/// Decodes the DNS server whose family the `network` byte of a restore packet names, from its
/// encoded `address`: none, which must be zero, an IPv4 address and twelve zero bytes, or an IPv6
/// address, neither unspecified.
fn parse_dns_server(
    network: u8,
    address: [u8; PACKET_DNS_SIZE],
) -> Result<Option<IpAddr>, PacketError> {
    let server = match network & PACKET_DNS_MASK {
        PACKET_DNS_IPV4 => match address.split_first_chunk::<4>() {
            Some((ipv4, padding)) if padding.iter().all(|&byte| byte == 0) => {
                Some(IpAddr::from(*ipv4))
            }
            _ => return Err(PacketError::DnsServer),
        },
        PACKET_DNS_IPV6 => Some(IpAddr::from(address)),
        _ if address == [0; PACKET_DNS_SIZE] => None,
        _ => return Err(PacketError::DnsServer),
    };
    if server.is_some_and(|server| server.is_unspecified()) {
        return Err(PacketError::DnsServer);
    }
    Ok(server)
}

/// Why a host time sample is invalid. Its `Display` is the sample's diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeSampleError {
    /// The sample is shorter than its record.
    Incomplete,
    /// The version is not one.
    Version(u8),
    /// The flags set bits other than the test-hook flag, or the reserved byte is not zero.
    Flags {
        /// The sample's flags.
        flags: u8,
        /// The sample's reserved byte.
        reserved: u8,
    },
    /// The host clock status is not one that the sample defines.
    Status(u8),
    /// The UTC time is zero or does not fit the clocks' signed nanoseconds.
    Utc(u64),
    /// The sample's guest TSC lies outside the guest's reads of its TSC around the sample's
    /// latch, by more than the sample's pairing uncertainty and the skew bound allow.
    Tsc {
        /// The sample's guest TSC.
        tsc: u64,
        /// The guest's TSC before the latch.
        before: u64,
        /// The guest's TSC after the latch.
        after: u64,
    },
}

impl Display for TimeSampleError {
    /// Writes the diagnostic, such as `time sample version is 2`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Incomplete => formatter.write_str("time sample is incomplete"),
            Self::Version(version) => write!(formatter, "time sample version is {version}"),
            Self::Flags { flags, reserved } => write!(
                formatter,
                "time sample flags 0x{flags:02x} or reserved 0x{reserved:02x}"
            ),
            Self::Status(status) => write!(formatter, "time sample host status is {status}"),
            Self::Utc(utc) => write!(formatter, "time sample UTC {utc} is invalid"),
            Self::Tsc { tsc, before, after } => write!(
                formatter,
                "time sample TSC {tsc} is outside the guest's reads {before} to {after}"
            ),
        }
    }
}

impl std::error::Error for TimeSampleError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// One host time sample: host UTC paired with the guest TSC, and the discipline of host UTC.
pub(crate) struct TimeSample {
    /// Sample feature/test flags.
    pub(crate) flags: u8,
    /// VM generation associated with the sample.
    pub(crate) generation: u32,
    /// Host UTC at the latch, in nanoseconds since the Unix epoch.
    pub(crate) utc_ns: u64,
    /// The guest TSC, of the first processor, at the instant of `utc_ns`.
    pub(crate) tsc: u64,
    /// The most the instants of `tsc` and `utc_ns` can differ, in nanoseconds.
    pub(crate) pairing_ns: u32,
    /// The discipline of host UTC.
    pub(crate) host: HostClock,
}

impl TimeSample {
    /// Decodes and validates the sample record at the start of `bytes`.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, TimeSampleError> {
        use super::sample::{Error, Sample};
        let sample = Sample::parse(bytes).map_err(|error| match error {
            Error::Incomplete => TimeSampleError::Incomplete,
            Error::Version(version) => TimeSampleError::Version(version),
            Error::Flags { flags, reserved } => TimeSampleError::Flags { flags, reserved },
            Error::Status(status) => TimeSampleError::Status(status),
            Error::Utc(utc) => TimeSampleError::Utc(utc),
        })?;
        Ok(Self {
            flags: sample.flags,
            generation: sample.generation,
            utc_ns: sample.utc_ns,
            tsc: sample.counter,
            pairing_ns: sample.pairing_ns,
            host: HostClock {
                status: HostClockStatus::from_byte(sample.host_status)
                    .expect("validated common wire status"),
                disruption: sample.disruption,
                correction: sample.correction,
                correction_bound: sample.correction_bound,
                max_error_us: sample.max_error_us,
            },
        })
    }

    /// Returns whether the sample carries the test-hook flag, which mirrors the restore packet's:
    /// host overrides, which only tests make, of the time that it publishes are active.
    pub(crate) const fn test_hooks(&self) -> bool {
        self.flags & SAMPLE_TEST_HOOKS != 0
    }
}

/// The guest's reads around the latch of one time sample, with which it pairs the sample's
/// guest TSC with its realtime clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GuestReads {
    /// The TSC right before the selector write that latched the sample.
    pub(crate) before: u64,
    /// The TSC right after the selector write.
    pub(crate) after: u64,
    /// The realtime clock, in nanoseconds, read right after the selector write.
    pub(crate) realtime_ns: i64,
    /// The TSC at the realtime read: the midpoint of the TSC reads around it.
    pub(crate) realtime_tsc: u64,
    /// Half the TSC cycles between the TSC reads around the realtime read.
    pub(crate) realtime_half_cycles: u64,
}

/// The rates with which the guest's realtime clock counts its TSC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClockRates {
    /// The declared TSC rate `F`, in Hz, which the frequency MSR reports.
    pub(crate) tsc_hz: u64,
    /// The kernel's frequency correction, in parts per million scaled by 2^16.
    pub(crate) frequency: i64,
}

/// Returns the rate at which the kernel counts the TSC for the declared rate `tsc_hz`: Linux
/// reads the frequency MSR to the kHz.
pub(crate) const fn kernel_tsc_hz(tsc_hz: u64) -> u64 {
    tsc_hz / HZ_PER_KHZ * HZ_PER_KHZ
}

/// Pairs `sample` with the guest's realtime clock through `reads`, taken at `rates`, and returns
/// the offset of host UTC from the guest's realtime clock at the sample's guest TSC, `theta`,
/// and its uncertainty, `epsilon`, both in nanoseconds.
///
/// The sample's guest TSC must lie between the reads around the latch, give or take the sample's
/// pairing uncertainty and the skew bound, since the host reads its UTC while it handles the
/// selector write. The guest's realtime clock at that TSC extrapolates its read after the latch
/// at the kernel's rate, and `epsilon` adds to the host's pairing uncertainty the cross-CPU skew
/// bound, half the reads around the realtime read, and what a slew can change the rate by over
/// the extrapolation.
pub(crate) fn pair_sample(
    sample: &TimeSample,
    reads: &GuestReads,
    rates: ClockRates,
) -> Result<(i64, i64), TimeSampleError> {
    super::sample::pair(
        sample.utc_ns,
        sample.tsc,
        sample.pairing_ns,
        &super::sample::Reads {
            before: reads.before,
            after: reads.after,
            realtime_ns: reads.realtime_ns,
            realtime_counter: reads.realtime_tsc,
            realtime_half_cycles: reads.realtime_half_cycles,
        },
        super::sample::Rates {
            counter_hz: rates.tsc_hz,
            kernel_hz: kernel_tsc_hz(rates.tsc_hz).max(HZ_PER_KHZ),
            frequency: rates.frequency,
        },
    )
    .map_err(|error| TimeSampleError::Tsc {
        tsc: error.counter,
        before: error.before,
        after: error.after,
    })
}

/// Returns the frequency correction, in parts per million scaled by 2^16, that runs the wall
/// clock at host UTC's rate when host UTC runs `correction` faster than the TSC counted at the
/// declared rate `tsc_hz`, and whether Linux's ±500 ppm limit clamped it.
///
/// The kernel counts the TSC at `F_k`, the declared rate `F` to the kHz, so the correction is
/// `(1 + correction) * F_k / F - 1`, rounded to the nearest.
pub(crate) fn guest_frequency(correction: i64, tsc_hz: u64) -> (i64, bool) {
    super::sample::guest_frequency(correction, tsc_hz, kernel_tsc_hz(tsc_hz))
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

/// Validates the kernel command line required by the time ABI: the kernel must apply none
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
mod tests;
