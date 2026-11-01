use std::fs;

use super::*;
use crate::guest::status::ExitStatus;
use crate::guest::status::tests::command_line;
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
