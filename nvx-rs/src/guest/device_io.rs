//! `nvx-device-io`: the benchmark helper that measures file and UDP round-trip I/O, and that
//! dirties a file's shared mapping for the microVM scenarios.
//!
//! - `file PATH read|write DURATION SIZE direct|buffered create|existing` reads or writes
//!   page-aligned 4096-byte blocks of PATH, at deterministic pseudo-random offsets within its
//!   first SIZE bytes, until DURATION milliseconds have passed.
//! - `network IPV4 PORT DURATION` sends 64-byte UDP datagrams to an echo server until DURATION
//!   milliseconds have passed, and counts those that come back within 100 ms.
//! - `map PATH TEXT` maps the first bytes of the existing regular file PATH shared and
//!   read-write, closes the file, and writes the nonempty TEXT at its start through the mapping
//!   alone, so that the kernel holds the written pages dirty until it writes them back. It then
//!   holds the mapping until standard input ends, and unmaps it.
//!
//! Each mode prints one JSON line with its operation count, bytes per operation, and elapsed
//! monotonic nanoseconds; `map` prints its line once its write is done, before it holds the
//! mapping. The program exits with 0, 2 for a usage error, 3 when it cannot open, connect, size,
//! map, or time, 4 when a transfer or send is incomplete, a file is shorter than TEXT, or reading
//! standard input or unmapping fails, or 255 when it cannot print the result. It prints nothing
//! else on standard output.
//!
//! A file benchmark also reports its progress on standard error, so that the console of a
//! benchmark that hangs shows where: `nvx-device-io: opened` once it has opened, and when
//! creating, sized its file, `nvx-device-io: transferred N blocks` once its timed loop ends, and,
//! when writing, `nvx-device-io: flushed` or `nvx-device-io: flush failed` once its flush returns.
//! A failure to write a progress line is ignored, and a standard error whose reader is gone raises
//! no `SIGPIPE`, so that only standard output's failures end the program.

use std::ffi::{CStr, CString};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::num::NonZeroUsize;
use std::process::ExitCode;

use super::status::{CommandError, run_main};
use super::sys::{self, Errno, MemoryMapping, Mode, OpenFlags};

/// Size and required alignment of each file transfer.
const BLOCK_SIZE: usize = 4096;
/// Size of the C program's fixed result buffer, which silently truncates a longer line.
const OUTPUT_SIZE: usize = 384;
/// Size of each UDP echo payload.
const PAYLOAD_SIZE: usize = 64;
/// Milliseconds for which each round trip waits for its echo.
const ECHO_TIMEOUT_MS: u16 = 100;
/// Initial state of the C program's block-offset generator.
const LCG_SEED: u64 = 0x9e37_79b9_7f4a_7c15;
/// Multiplier of its 64-bit linear congruential generator.
const LCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;
/// Increment of its 64-bit linear congruential generator.
const LCG_INCREMENT: u64 = 1_442_695_040_888_963_407;

/// One page-aligned file block, as `O_DIRECT` transfers require.
#[repr(C, align(4096))]
struct Block([u8; BLOCK_SIZE]);

/// Parsed `file PATH read|write DURATION SIZE direct|buffered create|existing` benchmark.
struct FileBenchmark<'a> {
    /// Path of the file.
    path: &'a CStr,
    /// Whether transfers write rather than read.
    writing: bool,
    /// Requested duration in milliseconds.
    duration_ms: u64,
    /// Size of the file in bytes, at least one block.
    size_bytes: u64,
    /// Whether to open the file with `O_DIRECT`.
    direct: bool,
    /// Whether to create the file and set its size.
    create: bool,
}

/// Parsed `network IPV4 PORT DURATION` benchmark.
struct NetworkBenchmark {
    /// Address and port of the UDP echo server.
    peer: SocketAddrV4,
    /// Requested duration in milliseconds.
    duration_ms: u64,
}

/// Parsed `map PATH TEXT` write through a shared mapping.
struct MappedWrite<'a> {
    /// Path of the file.
    path: &'a CStr,
    /// Nonempty bytes to write at the start of the file.
    text: &'a [u8],
}

/// Benchmark, or mapped write, that the command line selects.
enum Benchmark<'a> {
    /// File benchmark.
    File(FileBenchmark<'a>),
    /// UDP echo benchmark.
    Network(NetworkBenchmark),
    /// Write through a shared mapping, which the program holds until its input ends.
    Map(MappedWrite<'a>),
}

/// Operations that a timed loop completed, and the time that it took.
struct Measurement {
    /// Number of completed operations.
    operations: u64,
    /// Elapsed monotonic nanoseconds, wrapping as the C program's arithmetic did.
    elapsed_ns: u64,
}

/// Runs `nvx-device-io` with the process's arguments; see the module documentation.
pub fn main() -> ExitCode {
    run_main(|args| {
        run(
            args,
            &mut sys::DirectStdout,
            &mut sys::write_standard_error_without_sigpipe,
            &mut sys::discard_standard_input,
        )
    })
}

/// Parses and runs one benchmark, and prints its result to `out`. A file benchmark passes each
/// progress line to `progress`. A mapped write calls `hold` after it prints, and holds the mapping
/// until `hold` returns, which `main` makes the end of standard input.
fn run(
    args: &[CString],
    out: &mut dyn Write,
    progress: &mut dyn FnMut(&[u8]),
    hold: &mut dyn FnMut() -> Result<(), Errno>,
) -> Result<(), CommandError> {
    let (device, operation, bytes_per_operation, measurement) =
        match parse(args).ok_or(CommandError::Usage)? {
            Benchmark::File(benchmark) => {
                let operation = if benchmark.writing { "write" } else { "read" };
                (
                    "file",
                    operation,
                    BLOCK_SIZE,
                    measure_file(&benchmark, progress)?,
                )
            }
            Benchmark::Network(benchmark) => (
                "network",
                "roundtrip",
                PAYLOAD_SIZE,
                measure_network(&benchmark)?,
            ),
            Benchmark::Map(write) => return write_mapped(&write, out, hold),
        };
    let line = result_line(
        device,
        operation,
        measurement.operations,
        bytes_per_operation as u64,
        measurement.elapsed_ns,
    );
    out.write_all(&line).map_err(|_| CommandError::Output)
}

/// Parses a complete command line, returning `None` for the usage error.
fn parse(args: &[CString]) -> Option<Benchmark<'_>> {
    let mode = args.get(1)?.as_bytes();
    if mode == b"file" {
        let [_, _, path, operation, duration, size, access, creation] = args else {
            return None;
        };
        let duration_ms = parse_decimal(duration.as_bytes()).filter(|&duration| duration != 0)?;
        let size_bytes =
            parse_decimal(size.as_bytes()).filter(|&size| size >= BLOCK_SIZE as u64)?;
        let writing = match operation.as_bytes() {
            b"write" => true,
            b"read" => false,
            _ => return None,
        };
        let direct = match access.as_bytes() {
            b"direct" => true,
            b"buffered" => false,
            _ => return None,
        };
        let create = match creation.as_bytes() {
            b"create" => true,
            b"existing" => false,
            _ => return None,
        };
        return Some(Benchmark::File(FileBenchmark {
            path,
            writing,
            duration_ms,
            size_bytes,
            direct,
            create,
        }));
    }
    if mode == b"network" {
        let [_, _, address, port, duration] = args else {
            return None;
        };
        let address = parse_ipv4(address.as_bytes())?;
        let port = parse_decimal(port.as_bytes())
            .filter(|&port| port != 0 && port <= u64::from(u16::MAX))?;
        let duration_ms = parse_decimal(duration.as_bytes()).filter(|&duration| duration != 0)?;
        return Some(Benchmark::Network(NetworkBenchmark {
            peer: SocketAddrV4::new(address, port as u16),
            duration_ms,
        }));
    }
    if mode == b"map" {
        let [_, _, path, text] = args else {
            return None;
        };
        let text = text.as_bytes();
        return (!text.is_empty()).then_some(Benchmark::Map(MappedWrite { path, text }));
    }
    None
}

/// Parses at least one decimal digit, wrapping on overflow as the C program's parser did.
///
/// Rust's integer parsers reject an overflow instead of wrapping it, and also accept a leading
/// `+`, so the program keeps its own parser.
fn parse_decimal(text: &[u8]) -> Option<u64> {
    if text.is_empty() {
        return None;
    }
    let mut value = 0_u64;
    for &byte in text {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.wrapping_mul(10).wrapping_add(u64::from(byte - b'0'));
    }
    Some(value)
}

/// Parses four decimal octets separated by dots, with any number of leading zeros.
///
/// Rust's `Ipv4Addr` parser rejects leading zeros, so the program keeps the C program's parser.
fn parse_ipv4(text: &[u8]) -> Option<Ipv4Addr> {
    let mut octets = [0_u8; 4];
    let mut index = 0;
    let mut digits = 0_usize;
    for &byte in text {
        match byte {
            b'.' => {
                if digits == 0 || index == 3 {
                    return None;
                }
                index += 1;
                digits = 0;
            }
            b'0'..=b'9' => {
                let octet = u16::from(octets[index]) * 10 + u16::from(byte - b'0');
                octets[index] = u8::try_from(octet).ok()?;
                digits += 1;
            }
            _ => return None,
        }
    }
    (index == 3 && digits != 0).then_some(Ipv4Addr::from(octets))
}

/// Deterministic pseudo-random block offsets of the file benchmark.
struct Offsets {
    /// Current state of the linear congruential generator.
    state: u64,
    /// Number of whole blocks in the file, at least one.
    blocks: u64,
}

impl Offsets {
    /// Creates the offsets of a file with `blocks` whole blocks.
    fn new(blocks: u64) -> Self {
        Self {
            state: LCG_SEED,
            blocks,
        }
    }

    /// Advances the generator and returns the next byte offset.
    fn next_offset(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(LCG_MULTIPLIER)
            .wrapping_add(LCG_INCREMENT);
        (self.state % self.blocks) * BLOCK_SIZE as u64
    }
}

/// Fills a file block with byte `i * 31 + 17`, modulo 256.
fn fill_pattern(block: &mut [u8; BLOCK_SIZE]) {
    for (index, byte) in block.iter_mut().enumerate() {
        *byte = (index * 31 + 17) as u8;
    }
}

/// Fills a UDP payload with the bytes 1 through 64.
fn fill_payload(payload: &mut [u8; PAYLOAD_SIZE]) {
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte = index as u8 + 1;
    }
}

/// Returns the monotonic clock in nanoseconds, or 0 when it cannot be read, as in the C program.
fn now() -> u64 {
    sys::monotonic_ns().unwrap_or(0)
}

/// Reads the clock that starts a timed loop, or fails the setup when it cannot be read.
fn start() -> Result<u64, CommandError> {
    match now() {
        0 => Err(CommandError::Setup),
        started => Ok(started),
    }
}

/// Transfers blocks of the benchmark's file until its duration has passed, and passes its progress
/// lines to `progress`.
///
/// The file closes when this returns, before the caller prints the result, and a close error is
/// ignored, as in the C program.
fn measure_file(
    benchmark: &FileBenchmark<'_>,
    progress: &mut dyn FnMut(&[u8]),
) -> Result<Measurement, CommandError> {
    let mut flags = if benchmark.writing {
        OpenFlags::O_RDWR
    } else {
        OpenFlags::O_RDONLY
    } | OpenFlags::O_CLOEXEC;
    if benchmark.direct {
        flags |= OpenFlags::O_DIRECT;
    }
    if benchmark.create {
        flags |= OpenFlags::O_CREAT;
    }
    let file = sys::open(benchmark.path, flags, Mode::S_IRUSR | Mode::S_IWUSR)
        .map_err(|_| CommandError::Setup)?;
    if benchmark.create {
        file.truncate(benchmark.size_bytes)
            .map_err(|_| CommandError::Setup)?;
    }
    progress(b"nvx-device-io: opened\n");
    let mut block = Block([0; BLOCK_SIZE]);
    fill_pattern(&mut block.0);
    let mut offsets = Offsets::new(benchmark.size_bytes / BLOCK_SIZE as u64);
    let started = start()?;
    let duration_ns = benchmark.duration_ms.wrapping_mul(1_000_000);
    let mut ended = started;
    let mut operations = 0_u64;
    while ended.wrapping_sub(started) < duration_ns {
        let offset = offsets.next_offset();
        let transferred = if benchmark.writing {
            file.pwrite(&block.0, offset)
        } else {
            file.pread(&mut block.0, offset)
        };
        if transferred != Ok(BLOCK_SIZE) {
            return Err(CommandError::Transfer);
        }
        operations = operations.wrapping_add(1);
        if operations & 63 == 0 {
            ended = now();
        }
    }
    ended = now();
    progress(format!("nvx-device-io: transferred {operations} blocks\n").as_bytes());
    if benchmark.writing {
        // The flush starts after the timing ends. As in the C program, its failure fails nothing,
        // but the progress line reports it.
        let flushed: &[u8] = match file.sync() {
            Ok(_) => b"nvx-device-io: flushed\n",
            Err(_) => b"nvx-device-io: flush failed\n",
        };
        progress(flushed);
    }
    Ok(Measurement {
        operations,
        elapsed_ns: ended.wrapping_sub(started),
    })
}

/// Sends payloads to the benchmark's echo server, and counts their echoes, until its duration has
/// passed.
///
/// The socket closes when this returns, before the caller prints the result, as in the C
/// program.
fn measure_network(benchmark: &NetworkBenchmark) -> Result<Measurement, CommandError> {
    let socket = sys::connect_datagram(benchmark.peer).map_err(|_| CommandError::Setup)?;
    let mut payload = [0; PAYLOAD_SIZE];
    fill_payload(&mut payload);
    let started = start()?;
    let duration_ns = benchmark.duration_ms.wrapping_mul(1_000_000);
    let mut ended = started;
    let mut operations = 0_u64;
    while ended.wrapping_sub(started) < duration_ns {
        if socket.write(&payload) != Ok(PAYLOAD_SIZE) {
            return Err(CommandError::Transfer);
        }
        // A failed or empty wait or receive only leaves this round trip uncounted, and the echo
        // replaces the payload, as in the C program.
        if socket.wait_readable(ECHO_TIMEOUT_MS) == Ok(true)
            && socket.read(&mut payload) == Ok(PAYLOAD_SIZE)
        {
            operations = operations.wrapping_add(1);
        }
        ended = now();
    }
    Ok(Measurement {
        operations,
        elapsed_ns: ended.wrapping_sub(started),
    })
}

/// Writes the text at the start of the file through a shared mapping, prints the result line,
/// and holds the mapping until `hold` returns, then unmaps it.
///
/// Only the mapping holds the file while the write dirties its pages: the kernel writes a FUSE
/// file's dirty pages back when a descriptor of it closes, as it does when its mapping goes, so
/// the descriptor closes before the write. A failed print or hold drops the mapping.
fn write_mapped(
    write: &MappedWrite<'_>,
    out: &mut dyn Write,
    hold: &mut dyn FnMut() -> Result<(), Errno>,
) -> Result<(), CommandError> {
    let length = NonZeroUsize::new(write.text.len()).ok_or(CommandError::Usage)?;
    let mut mapping = map_file(write.path, length)?;
    let started = start()?;
    if !mapping.write_bytes(0, write.text) {
        return Err(CommandError::Transfer);
    }
    let elapsed_ns = now().wrapping_sub(started);
    let line = result_line("file", "map", 1, length.get() as u64, elapsed_ns);
    out.write_all(&line).map_err(|_| CommandError::Output)?;
    hold().map_err(|_| CommandError::Transfer)?;
    mapping.unmap().map_err(|_| CommandError::Transfer)
}

/// Maps the first `length` bytes of the regular file at `path` shared and read-write. The file's
/// descriptor closes when this returns, and the mapping keeps the file open.
fn map_file(path: &CStr, length: NonZeroUsize) -> Result<MemoryMapping, CommandError> {
    let file = sys::open(
        path,
        OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| CommandError::Setup)?;
    let (regular, size) = sys::file_size(&file).map_err(|_| CommandError::Setup)?;
    if !regular {
        return Err(CommandError::Setup);
    }
    // Touching a mapped page past the end of the file would raise SIGBUS.
    if size < length.get() as u64 {
        return Err(CommandError::Transfer);
    }
    MemoryMapping::shared_read_write(&file, length, 0).map_err(|_| CommandError::Setup)
}

/// Returns the newline-terminated JSON result, truncated to the C program's 384-byte buffer.
fn result_line(
    device: &str,
    operation: &str,
    operations: u64,
    bytes_per_operation: u64,
    elapsed_ns: u64,
) -> Vec<u8> {
    let mut line = format!(
        "{{\"device\":\"{device}\",\"operation\":\"{operation}\",\"operations\":{operations},\
         \"bytes_per_operation\":{bytes_per_operation},\"elapsed_ns\":{elapsed_ns}}}\n"
    )
    .into_bytes();
    line.truncate(OUTPUT_SIZE);
    line
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::guest::status::{ExitStatus, command_line};
    use crate::guest::sys::tests::Failing;

    /// Returns the command line of `nvx-device-io` with `arguments`.
    fn args(arguments: &[&str]) -> Vec<CString> {
        command_line("nvx-device-io", arguments)
    }

    /// Stands for the input that ends a mapped write's hold, which a benchmark never waits for.
    fn never_holds() -> Result<(), Errno> {
        panic!("only a mapped write holds its mapping")
    }

    /// Stands for the progress of a file benchmark, which a mapped write never reports.
    fn no_progress(line: &[u8]) {
        panic!("only a file benchmark reports progress: {line:?}")
    }

    /// Returns the progress lines of a file benchmark that `report` received.
    fn progress_lines(report: &[u8]) -> Vec<String> {
        String::from_utf8(report.to_vec())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn parses_decimals_that_wrap_around() {
        assert_eq!(parse_decimal(b""), None);
        assert_eq!(parse_decimal(b"0"), Some(0));
        assert_eq!(parse_decimal(b"0042"), Some(42));
        assert_eq!(parse_decimal(b"18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_decimal(b"18446744073709551616"), Some(0));
        assert_eq!(parse_decimal(b"18446744073709551617"), Some(1));
        for invalid in ["+1", "-1", " 1", "1 ", "0x10", "1a"] {
            assert_eq!(parse_decimal(invalid.as_bytes()), None, "{invalid}");
        }
    }

    #[test]
    fn parses_dotted_quads() {
        assert_eq!(parse_ipv4(b"127.0.0.1"), Some(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(parse_ipv4(b"10.0.2.2"), Some(Ipv4Addr::new(10, 0, 2, 2)));
        assert_eq!(parse_ipv4(b"255.255.255.255"), Some(Ipv4Addr::BROADCAST));
        assert_eq!(
            parse_ipv4(b"010.0.0.0001"),
            Some(Ipv4Addr::new(10, 0, 0, 1))
        );
        for invalid in [
            "",
            "1",
            "1.2.3",
            "1.2.3.4.5",
            "1..2.3",
            ".1.2.3",
            "1.2.3.",
            "256.0.0.1",
            "1.2.3.4a",
            "1.2.3.-4",
            " 1.2.3.4",
        ] {
            assert_eq!(parse_ipv4(invalid.as_bytes()), None, "{invalid}");
        }
    }

    #[test]
    fn parses_file_benchmarks() {
        let file = args(&["file", "/f", "write", "250", "8192", "direct", "create"]);
        let Some(Benchmark::File(parsed)) = parse(&file) else {
            panic!("not a file benchmark");
        };
        assert_eq!(parsed.path, c"/f");
        assert_eq!(
            (
                parsed.writing,
                parsed.duration_ms,
                parsed.size_bytes,
                parsed.direct,
                parsed.create
            ),
            (true, 250, 8192, true, true)
        );
        let read = ["file", "/f", "read", "1", "4096", "buffered", "existing"];
        let read_args = args(&read);
        let Some(Benchmark::File(parsed)) = parse(&read_args) else {
            panic!("not a file benchmark");
        };
        assert_eq!(
            (
                parsed.writing,
                parsed.duration_ms,
                parsed.size_bytes,
                parsed.direct,
                parsed.create
            ),
            (false, 1, 4096, false, false)
        );
        for (position, invalid) in [
            (3, "0"),
            (3, "-1"),
            (3, ""),
            (4, "4095"),
            (2, "readwrite"),
            (2, "Read"),
            (5, "cached"),
            (6, "new"),
            (0, "x"),
        ] {
            let mut arguments = read;
            arguments[position] = invalid;
            assert!(parse(&args(&arguments)).is_none(), "{arguments:?}");
        }
        assert!(parse(&args(&read[..6])).is_none());
        assert!(parse(&args(&[&read[..], &["extra"]].concat())).is_none());
    }

    #[test]
    fn parses_network_benchmarks() {
        let network = ["network", "10.0.2.2", "18446744073709551617", "50"];
        let Some(Benchmark::Network(parsed)) = parse(&args(&network)) else {
            panic!("not a network benchmark");
        };
        assert_eq!(
            (parsed.peer, parsed.duration_ms),
            (SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 2), 1), 50)
        );
        for (position, invalid) in [(1, "10.0.2"), (2, "0"), (2, "65536"), (3, "0"), (0, "net")] {
            let mut arguments = network;
            arguments[position] = invalid;
            assert!(parse(&args(&arguments)).is_none(), "{arguments:?}");
        }
        assert!(parse(&args(&[])).is_none());
        assert!(parse(&args(&network[..3])).is_none());
    }

    #[test]
    fn parses_mapped_writes() {
        let map = args(&["map", "/f", "NVX-TEXT"]);
        let Some(Benchmark::Map(parsed)) = parse(&map) else {
            panic!("not a mapped write");
        };
        assert_eq!((parsed.path, parsed.text), (c"/f", &b"NVX-TEXT"[..]));
        assert!(parse(&args(&["map", "/f", ""])).is_none());
        assert!(parse(&args(&["map", "/f"])).is_none());
        assert!(parse(&args(&["map", "/f", "NVX-TEXT", "extra"])).is_none());
        assert!(parse(&args(&["Map", "/f", "NVX-TEXT"])).is_none());
    }

    #[test]
    fn offsets_follow_the_c_generator() {
        let mut offsets = Offsets::new(256);
        let first: Vec<u64> = (0..4).map(|_| offsets.next_offset()).collect();
        assert_eq!(first, [0, 79 * 4096, 50 * 4096, 25 * 4096]);
        let mut offsets = Offsets::new(1);
        assert!((0..100).all(|_| offsets.next_offset() == 0));
    }

    #[test]
    fn fills_the_patterns() {
        let mut block = Block([0; BLOCK_SIZE]);
        fill_pattern(&mut block.0);
        assert_eq!(&block.0[..4], &[17, 48, 79, 110]);
        assert_eq!(block.0[4095], (4095 * 31 + 17) as u8);
        assert_eq!(align_of::<Block>(), BLOCK_SIZE);
        let mut payload = [0; PAYLOAD_SIZE];
        fill_payload(&mut payload);
        assert_eq!((payload[0], payload[63]), (1, 64));
    }

    #[test]
    fn formats_and_truncates_the_result_line() {
        assert_eq!(
            result_line("file", "read", 12, 4096, 1_000_000_007),
            b"{\"device\":\"file\",\"operation\":\"read\",\"operations\":12,\
              \"bytes_per_operation\":4096,\"elapsed_ns\":1000000007}\n"
        );
        assert_eq!(
            result_line("network", "roundtrip", 0, 64, u64::MAX),
            b"{\"device\":\"network\",\"operation\":\"roundtrip\",\"operations\":0,\
              \"bytes_per_operation\":64,\"elapsed_ns\":18446744073709551615}\n"
        );
        let long = "x".repeat(OUTPUT_SIZE);
        let line = result_line("file", &long, 1, 2, 3);
        assert_eq!(line.len(), OUTPUT_SIZE);
        assert!(line.starts_with(b"{\"device\":\"file\",\"operation\":\"xxx"));
    }

    #[test]
    fn measures_a_file_and_reports_each_failure() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("disk");
        let path = file.to_str().unwrap();
        let mut out = Vec::new();
        let mut report = Vec::new();
        let write = ["file", path, "write", "1", "8192", "buffered", "create"];
        let status = ExitStatus::from(run(
            &args(&write),
            &mut out,
            &mut |line| report.extend_from_slice(line),
            &mut never_holds,
        ));
        assert_eq!(status, ExitStatus::Success);
        let line = String::from_utf8(out).unwrap();
        assert!(
            line.starts_with("{\"device\":\"file\",\"operation\":\"write\",\"operations\":"),
            "{line}"
        );
        assert!(
            line.contains(",\"bytes_per_operation\":4096,\"elapsed_ns\":") && line.ends_with("}\n"),
            "{line}"
        );
        assert_eq!(fs::metadata(&file).unwrap().len(), 8192);
        let operations = line
            .split_once("\"operations\":")
            .and_then(|(_, rest)| rest.split_once(','))
            .map(|(count, _)| count)
            .unwrap();
        assert_eq!(
            progress_lines(&report),
            [
                "nvx-device-io: opened".to_owned(),
                format!("nvx-device-io: transferred {operations} blocks"),
                "nvx-device-io: flushed".to_owned(),
            ]
        );

        // A read reports no flush, and a failed print still reports the progress before it.
        let read = ["file", path, "read", "1", "8192", "buffered", "existing"];
        let mut report = Vec::new();
        let status = ExitStatus::from(run(
            &args(&read),
            &mut Failing(sys::Errno::ENOSPC),
            &mut |line| report.extend_from_slice(line),
            &mut never_holds,
        ));
        assert_eq!(status, ExitStatus::OutputFailure);
        let lines = progress_lines(&report);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], "nvx-device-io: opened");
        assert!(
            lines[1].starts_with("nvx-device-io: transferred ") && lines[1].ends_with(" blocks"),
            "{lines:?}"
        );

        // A file that cannot open reports no progress.
        let missing = directory.path().join("missing");
        let mut arguments = read;
        arguments[1] = missing.to_str().unwrap();
        let mut report = Vec::new();
        let status = ExitStatus::from(run(
            &args(&arguments),
            &mut Vec::new(),
            &mut |line| report.extend_from_slice(line),
            &mut never_holds,
        ));
        assert_eq!((status, report), (ExitStatus::SetupFailure, Vec::new()));

        // An empty file opens but cannot transfer a block.
        fs::write(&missing, b"").unwrap();
        arguments[4] = "4096";
        let mut out = Vec::new();
        let mut report = Vec::new();
        let status = ExitStatus::from(run(
            &args(&arguments),
            &mut out,
            &mut |line| report.extend_from_slice(line),
            &mut never_holds,
        ));
        assert_eq!(status, ExitStatus::TransferFailure);
        assert!(out.is_empty());
        assert_eq!(progress_lines(&report), ["nvx-device-io: opened"]);
    }

    #[test]
    fn writes_through_a_held_mapping_and_reports_each_failure() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("mapped");
        fs::write(&file, vec![b'.'; 2 * BLOCK_SIZE]).unwrap();
        let path = file.to_str().unwrap();
        let mut out = Vec::new();
        let mut held = false;
        let status = ExitStatus::from(run(
            &args(&["map", path, "NVX-MAPPED"]),
            &mut out,
            &mut no_progress,
            &mut || {
                // The page cache already holds the write while the mapping is held.
                assert!(fs::read(&file).unwrap().starts_with(b"NVX-MAPPED."));
                held = true;
                Ok(())
            },
        ));
        assert_eq!(status, ExitStatus::Success);
        assert!(held);
        let line = String::from_utf8(out).unwrap();
        assert!(
            line.starts_with(
                "{\"device\":\"file\",\"operation\":\"map\",\"operations\":1,\
                 \"bytes_per_operation\":10,\"elapsed_ns\":"
            ) && line.ends_with("}\n"),
            "{line}"
        );
        let contents = fs::read(&file).unwrap();
        assert_eq!(contents.len(), 2 * BLOCK_SIZE);
        assert!(contents.starts_with(b"NVX-MAPPED."));

        // A mapped write that cannot print its result never holds the mapping, and one whose
        // hold fails reports an incomplete transfer.
        let status = ExitStatus::from(run(
            &args(&["map", path, "NVX"]),
            &mut Failing(Errno::ENOSPC),
            &mut no_progress,
            &mut never_holds,
        ));
        assert_eq!(status, ExitStatus::OutputFailure);
        let status = ExitStatus::from(run(
            &args(&["map", path, "NVX"]),
            &mut Vec::new(),
            &mut no_progress,
            &mut || Err(Errno::EIO),
        ));
        assert_eq!(status, ExitStatus::TransferFailure);

        // A missing file and a directory cannot map, and a file shorter than the text cannot
        // take it.
        for (target, expected) in [
            (directory.path().join("missing"), ExitStatus::SetupFailure),
            (directory.path().to_path_buf(), ExitStatus::SetupFailure),
        ] {
            let arguments = ["map", target.to_str().unwrap(), "NVX"];
            let mut out = Vec::new();
            let status = ExitStatus::from(run(
                &args(&arguments),
                &mut out,
                &mut no_progress,
                &mut never_holds,
            ));
            assert_eq!((status, out), (expected, Vec::new()), "{target:?}");
        }
        let short = directory.path().join("short");
        fs::write(&short, b"NVX").unwrap();
        let arguments = ["map", short.to_str().unwrap(), "NVX-MAPPED"];
        let mut out = Vec::new();
        let status = ExitStatus::from(run(
            &args(&arguments),
            &mut out,
            &mut no_progress,
            &mut never_holds,
        ));
        assert_eq!((status, out), (ExitStatus::TransferFailure, Vec::new()));
        assert_eq!(fs::read(&short).unwrap(), b"NVX");
    }
}
