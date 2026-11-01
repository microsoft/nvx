use nvx::activation::TransactionId;
use nvx::protocol::{GuestHostMap, MAP_READ_ONLY, MAX_MAP_PATH_BYTES};

use super::*;
use crate::frame::Message;

/// Returns a configuration with every field.
fn full() -> RootConfig {
    RootConfig {
        distro_sectors: Some(116_960),
        scratch_sectors: Some(131_072),
        first_partition_bound: Some(4096),
        overlay_mode: Some(OverlayMode::UntrustedLowers),
        scratch_journal: Some(JournalPolicy::Forbidden),
        mapping_count: 3,
        workload_identity: Some((1000, 100)),
        listeners: Some(Listeners::Unpublished),
        ..RootConfig::default()
    }
}

#[test]
fn a_configuration_is_one_line_per_field() {
    assert_eq!(
        String::from_utf8(full().encode()).unwrap(),
        "version=1\n\
             runtime=/run/runtime-adapter\n\
             agent-cgroup=/sys/fs/cgroup/agent\n\
             workload-cgroup=/sys/fs/cgroup/workload\n\
             device-timeout-ms=30000\n\
             agent-memory-low=16777216\n\
             workload-memory-max=134217728\n\
             workload-pids-max=128\n\
             distro-sectors=116960\n\
             scratch-sectors=131072\n\
             first-partition-bound=4096\n\
             overlay-mode=untrusted-lowers\n\
             scratch-journal=forbidden\n\
             mapping-count=3\n\
             workload-uid=1000\n\
             workload-gid=100\n\
             listeners=unpublished\n"
    );
    // Without a mapping, the configuration names no count.
    let unmapped = String::from_utf8(RootConfig::default().encode()).unwrap();
    assert!(!unmapped.contains("mapping-count"), "{unmapped}");
    // Each mode, policy, and publication has its own name.
    for mode in OverlayMode::ALL {
        assert_eq!(OverlayMode::from_name(mode.as_str()), Some(mode));
    }
    for policy in JournalPolicy::ALL {
        assert_eq!(JournalPolicy::from_name(policy.as_str()), Some(policy));
    }
    for listeners in Listeners::ALL {
        assert_eq!(Listeners::from_name(listeners.as_str()), Some(listeners));
    }
    let minimal = RootConfig {
        agent_memory_low: None,
        workload_memory_max: None,
        workload_pids_max: None,
        ..RootConfig::default()
    };
    let largest = RootConfig {
        workload_identity: Some((u32::MAX - 1, u32::MAX - 1)),
        ..RootConfig::default()
    };
    let published = RootConfig {
        listeners: Some(Listeners::Published),
        ..RootConfig::default()
    };
    for config in [full(), minimal, largest, published, RootConfig::default()] {
        assert_eq!(RootConfig::decode(&config.encode()), Ok(config));
    }
    // The fields may come in any order.
    let mut lines = String::from_utf8(full().encode())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines.reverse();
    let reversed = lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    assert_eq!(RootConfig::decode(reversed.as_bytes()), Ok(full()));
}

#[test]
fn a_configuration_that_breaks_the_rules_is_refused() {
    let base = String::from_utf8(RootConfig::default().encode()).unwrap();
    let replaced = |name: &str, value: &str| {
        base.lines()
            .map(|line| match line.split_once('=') {
                Some((field, _)) if field == name => format!("{name}={value}\n"),
                _ => format!("{line}\n"),
            })
            .collect::<String>()
    };
    let without = |name: &str| {
        base.lines()
            .filter(|line| !line.starts_with(&format!("{name}=")))
            .map(|line| format!("{line}\n"))
            .collect::<String>()
    };
    let cases = [
        (Vec::from(&b"version=1\xff\n"[..]), "is not UTF-8 text"),
        (
            base.trim_end().as_bytes().to_vec(),
            "does not end its last line",
        ),
        (
            format!("{base}\n").into_bytes(),
            "has a line without a value",
        ),
        (
            format!("{base}color=blue\n").into_bytes(),
            "names the unknown field \"color\"",
        ),
        (
            format!("{base}runtime=/run/other\n").into_bytes(),
            "repeats the field runtime",
        ),
        (Vec::new(), "lacks the field version"),
        (without(RUNTIME).into_bytes(), "lacks the field runtime"),
        (
            without(DEVICE_TIMEOUT).into_bytes(),
            "lacks the field device-timeout-ms",
        ),
        (replaced(VERSION, "2").into_bytes(), "has version \"2\""),
        (
            replaced(RUNTIME, "run/relative").into_bytes(),
            "is not absolute",
        ),
        (
            replaced(RUNTIME, "/").into_bytes(),
            "is not canonical, or is the root",
        ),
        (
            replaced(RUNTIME, "/run//nvx").into_bytes(),
            "is not canonical",
        ),
        (
            replaced(RUNTIME, "/run/./nvx").into_bytes(),
            "is not canonical",
        ),
        (
            replaced(RUNTIME, "/run/../nvx").into_bytes(),
            "is not canonical",
        ),
        (
            replaced(RUNTIME, "/run/nvx/").into_bytes(),
            "is not canonical",
        ),
        (
            replaced(RUNTIME, "/run/a,b").into_bytes(),
            "the overlay's options reserve",
        ),
        (
            replaced(RUNTIME, "/run/a:b").into_bytes(),
            "the overlay's options reserve",
        ),
        (
            replaced(RUNTIME, "/run/a=b").into_bytes(),
            "the overlay's options reserve",
        ),
        (replaced(RUNTIME, "/run/a\0b").into_bytes(), "holds a NUL"),
        (
            replaced(RUNTIME, &format!("/{}", "a".repeat(4095))).into_bytes(),
            "longer than 4095",
        ),
        (
            replaced(AGENT_CGROUP, "agent").into_bytes(),
            "names the agent-cgroup \"agent\", which is not absolute",
        ),
        (
            replaced(WORKLOAD_CGROUP, "/sys/fs/cgroup/agent").into_bytes(),
            "not siblings",
        ),
        (
            replaced(WORKLOAD_CGROUP, "/sys/fs/cgroup/agent/workload").into_bytes(),
            "not siblings",
        ),
        (
            replaced(WORKLOAD_CGROUP, "/sys/fs/workload").into_bytes(),
            "not siblings",
        ),
        (
            replaced(AGENT_CGROUP, "/agent")
                .replace(
                    "workload-cgroup=/sys/fs/cgroup/workload",
                    "workload-cgroup=/workload",
                )
                .into_bytes(),
            "names cgroups in the root directory",
        ),
        (
            replaced(DEVICE_TIMEOUT, "0").into_bytes(),
            "not a positive decimal number",
        ),
        (
            replaced(DEVICE_TIMEOUT, "600001").into_bytes(),
            "longer than 600000 ms",
        ),
        (
            replaced(AGENT_MEMORY_LOW, "-1").into_bytes(),
            "not a positive decimal number",
        ),
        (
            replaced(WORKLOAD_MEMORY_MAX, "0x10").into_bytes(),
            "not a positive decimal number",
        ),
        (
            replaced(WORKLOAD_PIDS_MAX, "010").into_bytes(),
            "not a positive decimal number",
        ),
        (
            replaced(WORKLOAD_PIDS_MAX, "").into_bytes(),
            "not a positive decimal number",
        ),
        (
            format!("{base}distro-sectors=18446744073709551616\n").into_bytes(),
            "not a positive decimal number",
        ),
        (
            format!("{base}scratch-sectors= 1\n").into_bytes(),
            "not a positive decimal number",
        ),
        (
            format!("{base}first-partition-bound=0\n").into_bytes(),
            "has a first-partition-bound that is not a positive decimal number: \"0\"",
        ),
        (
            format!("{base}overlay-mode=TrustedLowers\n").into_bytes(),
            "names the unknown overlay-mode \"TrustedLowers\"",
        ),
        (
            format!("{base}scratch-journal=\n").into_bytes(),
            "names the unknown scratch-journal \"\"",
        ),
        (
            format!("{base}mapping-count=0\n").into_bytes(),
            "has a mapping-count that is not a positive decimal number: \"0\"",
        ),
        (
            format!("{base}mapping-count=03\n").into_bytes(),
            "not a positive decimal number",
        ),
        (
            format!("{base}mapping-count=4097\n").into_bytes(),
            "announces 4097 mappings, more than 4096",
        ),
        (
            format!("{base}mapping-count=18446744073709551615\n").into_bytes(),
            "announces 18446744073709551615 mappings, more than 4096",
        ),
        (
            format!("{base}workload-uid=0\nworkload-gid=1000\n").into_bytes(),
            "has a workload-uid that is not a positive decimal number: \"0\"",
        ),
        (
            format!("{base}workload-uid=1000\nworkload-gid=0\n").into_bytes(),
            "has a workload-gid that is not a positive decimal number: \"0\"",
        ),
        (
            format!("{base}workload-uid=4294967295\nworkload-gid=1000\n").into_bytes(),
            "has a workload-uid above 4294967294, which no identity has: 4294967295",
        ),
        (
            format!("{base}workload-uid=1000\nworkload-gid=4294967296\n").into_bytes(),
            "has a workload-gid above 4294967294",
        ),
        (
            format!("{base}workload-uid=1000\n").into_bytes(),
            "names a workload identity without both workload-uid and workload-gid",
        ),
        (
            format!("{base}workload-gid=1000\n").into_bytes(),
            "names a workload identity without both workload-uid and workload-gid",
        ),
        (
            format!("{base}listeners=Published\n").into_bytes(),
            "names the unknown listeners \"Published\"",
        ),
        (
            format!("{base}listeners=published\nlisteners=unpublished\n").into_bytes(),
            "repeats the field listeners",
        ),
    ];
    for (body, problem) in cases {
        let error = RootConfig::decode(&body).unwrap_err();
        assert!(
            error.problem().starts_with("the root configuration ")
                && error.problem().contains(problem),
            "{:?}: {error}",
            String::from_utf8_lossy(&body)
        );
    }
    // The longest valid path, and the largest numbers.
    let longest = format!("/{}", "a".repeat(4094));
    let large = format!("{base}distro-sectors={}\n", u64::MAX);
    let most_mappings = format!("{base}mapping-count=4096\n");
    assert_eq!(
        RootConfig::decode(replaced(RUNTIME, &longest).as_bytes())
            .unwrap()
            .runtime,
        longest
    );
    assert_eq!(
        RootConfig::decode(large.as_bytes()).unwrap().distro_sectors,
        Some(u64::MAX)
    );
    let config = RootConfig::decode(most_mappings.as_bytes()).unwrap();
    assert_eq!(config.mapping_count, 4096);
    assert_eq!(config.encode(), most_mappings.into_bytes());
}

#[test]
fn stages_have_codes_names_and_exit_statuses() {
    let expected = [
        (RootStage::Configuration, 1, "configuration", 11),
        (RootStage::Layout, 2, "layout", 12),
        (RootStage::Bootstrap, 3, "bootstrap", 13),
        (RootStage::Assembly, 4, "assembly", 14),
        (RootStage::Teardown, 5, "teardown", 15),
        (RootStage::Identity, 6, "identity", 16),
    ];
    for (stage, code, name, status) in expected {
        assert_eq!(stage.code(), code);
        assert_eq!(RootStage::from_code(code), Some(stage));
        assert_eq!(stage.as_str(), name);
        assert_eq!(stage.to_string(), name);
        assert_eq!(RootStage::from_name(name), Some(stage));
        assert_eq!(stage.exit_status(), status);
    }
    assert_eq!(RootStage::ALL.map(RootStage::code), [1, 2, 3, 4, 5, 6]);
    assert_eq!(RootStage::from_code(0), None);
    assert_eq!(RootStage::from_code(7), None);
    assert_eq!(RootStage::from_name("mapping"), None);
}

#[test]
fn a_failure_is_its_stage_then_its_problem() {
    let failure = RootFailure::new(RootStage::Assembly, "the UUID differs");
    assert_eq!(failure.encode(), b"\x04the UUID differs");
    assert_eq!(RootFailure::decode(&failure.encode()), Ok(failure.clone()));
    assert_eq!(
        failure.to_string(),
        "the assembly of the root failed: the UUID differs"
    );
    let empty = RootFailure::new(RootStage::Teardown, "");
    assert_eq!(RootFailure::decode(&[5]), Ok(empty));

    // A long problem is cut at a character's boundary, so that the body fits its limit.
    let long = "é".repeat(MAX_TEXT_BODY);
    let failure = RootFailure::new(RootStage::Configuration, long);
    assert_eq!(failure.problem().len(), MAX_PROBLEM_LEN - 1);
    assert!(failure.encode().len() <= MAX_TEXT_BODY);
    assert_eq!(RootFailure::decode(&failure.encode()), Ok(failure));

    for (body, problem) in [
        (&b""[..], "names no stage"),
        (b"\x00problem", "unknown stage 0"),
        (b"\x07problem", "unknown stage 7"),
        (b"\x01\xffproblem", "not UTF-8 text"),
    ] {
        let error = RootFailure::decode(body).unwrap_err();
        assert!(error.problem().contains(problem), "{body:?}: {error}");
    }
}

#[test]
fn a_readiness_is_the_length_of_its_build_then_the_build() {
    let ready = Ready::new("0.1.0").unwrap();
    assert_eq!(ready.build(), "0.1.0");
    assert_eq!(ready.activation(), None);
    assert_eq!(ready.encode(), b"\x050.1.0");
    assert_eq!(Ready::decode(&ready.encode()), Ok(ready));

    // The guest agent reports this crate's version. A build holds any printable ASCII
    // character but a space, up to its limit, which fits the framing's.
    let ours = Ready::new(crate::GUEST_BUILD).unwrap();
    assert_eq!(ours.build(), env!("CARGO_PKG_VERSION"));
    let printable = (b'!'..=b'~').map(char::from).collect::<String>();
    let longest = "b".repeat(MAX_BUILD_LEN);
    for build in [
        crate::GUEST_BUILD,
        "x",
        printable.as_str(),
        longest.as_str(),
    ] {
        let ready = Ready::new(build).unwrap();
        let body = ready.encode();
        assert_eq!(usize::from(body[0]), build.len(), "{build}");
        assert_eq!(&body[1..], build.as_bytes(), "{build}");
        assert!(body.len() <= MAX_TEXT_BODY, "{build}");
        assert_eq!(Ready::decode(&body), Ok(ready), "{build}");
    }
}

#[test]
fn a_clone_s_readiness_reports_its_activation_after_the_build() {
    let clone = ReadyActivation {
        transaction: Some(TransactionId::from_bytes([0x7e; TransactionId::LEN])),
    };
    // A cold boot's activation, which the encoding takes too, though the adapter's cold boots
    // report none.
    let cold = ReadyActivation { transaction: None };
    let longest = "b".repeat(MAX_BUILD_LEN);
    for build in ["0.1.0", longest.as_str()] {
        for activation in [clone, cold] {
            let ready = Ready::new(build).unwrap().with_activation(activation);
            assert_eq!(ready.build(), build);
            assert_eq!(ready.activation(), Some(activation));
            let body = ready.encode();
            let mut expected = vec![u8::try_from(build.len()).unwrap()];
            expected.extend_from_slice(build.as_bytes());
            expected.extend(activation.encode());
            assert_eq!(body, expected, "{build}: {activation:?}");
            assert!(body.len() <= MAX_TEXT_BODY, "{build}");
            assert_eq!(Ready::decode(&body), Ok(ready), "{build}: {activation:?}");
        }
    }
    // The clone's: `1`, and the transaction ID, after the build; a cold boot's: `0`.
    let ready = Ready::new("0.1.0").unwrap().with_activation(clone);
    let mut expected = b"\x050.1.0".to_vec();
    expected.push(1);
    expected.extend([0x7e; 16]);
    assert_eq!(ready.encode(), expected);
    assert_eq!(
        ready.encode().len(),
        6 + ReadyActivation::CLONE_LEN,
        "{ready:?}"
    );
    let ready = Ready::new("0.1.0").unwrap().with_activation(cold);
    assert_eq!(ready.encode(), b"\x050.1.0\x00");
}

#[test]
fn a_readiness_that_breaks_the_rules_is_refused() {
    let too_long = "b".repeat(MAX_BUILD_LEN + 1);
    let builds = [
        "",
        "0.1 0",
        "0.1.0\n",
        "0.1.0\0",
        "0.1-é",
        "\u{7f}",
        too_long.as_str(),
    ];
    for build in builds {
        let error = Ready::new(build).unwrap_err();
        assert!(
            error
                .problem()
                .contains("does not hold 1 to 255 printable ASCII characters without a space"),
            "{build:?}: {error}"
        );
    }
    let cases: [(&[u8], &str); 11] = [
        (b"", "names no build"),
        (
            b"\x00",
            "names a build that does not hold 1 to 255 printable ASCII characters",
        ),
        (b"\x060.1.0", "ends early"),
        (b"\xff0.1.0", "ends early"),
        (
            b"\x050.1.0\x01",
            "has 1 byte after its build, which is not an activation",
        ),
        (
            b"\x050.1.0\x02",
            "has 1 byte after its build, which is not an activation",
        ),
        (
            b"\x040.1.0",
            "has 1 byte after its build, which is not an activation",
        ),
        (
            b"\x010.1.0",
            "has 4 bytes after its build, which are not an activation",
        ),
        (b"\x050.1 0", "without a space: \"0.1 0\""),
        (b"\x050.1\xff0", "without a space: \"0.1\u{fffd}0\""),
        (b"\x02\xc3\xa9", "without a space: \"\u{e9}\""),
    ];
    for (body, problem) in cases {
        let error = Ready::decode(body).unwrap_err();
        assert!(
            error.problem().starts_with("the guest's readiness ")
                && error.problem().contains(problem),
            "{body:?}: {error}"
        );
    }
    // Bytes after the build that are neither `0`, nor `1` followed by a transaction ID, which
    // `ReadyActivation::decode` refuses: a cold boot's or a clone's with bytes after it, a
    // clone's whose transaction ID ends early, and another first byte.
    let reported = |rest: &[u8]| {
        let mut body = b"\x050.1.0".to_vec();
        body.extend_from_slice(rest);
        body
    };
    for (body, count) in [
        (reported(&[0, 0]), 2),
        (reported(&[0; 17]), 17),
        (reported(&[1; 18]), 18),
        (reported(&[1; 16]), 16),
        (reported(&[2; 17]), 17),
    ] {
        let error = Ready::decode(&body).unwrap_err();
        assert_eq!(
            error.problem(),
            format!(
                "the guest's readiness has {count} bytes after its build, which are not an \
                     activation: what Ready reports is neither 0, nor 1 followed by a transaction \
                     ID"
            ),
            "{body:?}"
        );
    }
}

/// Returns the host's mapping of `source` at `target`, read-only if `read_only`.
fn mapping(source: &str, target: &str, read_only: bool) -> HostMap {
    HostMap {
        source: source.to_owned(),
        target: target.to_owned(),
        read_only,
    }
}

/// Returns `map` as the guest reads it.
fn read(map: &HostMap) -> GuestHostMap {
    GuestHostMap {
        source: map.source.as_bytes().to_vec(),
        target: map.target.as_bytes().to_vec(),
        read_only: map.read_only,
    }
}

/// Returns a part of a mapping table, whose first mapping has the index `first` in the table,
/// of `count` mappings, of which it holds `entries`, each its flags, source, and target.
fn part(first: u32, count: u32, entries: &[(u16, &[u8], &[u8])]) -> Vec<u8> {
    let mut body = [first.to_le_bytes(), count.to_le_bytes()].concat();
    for (flags, source, target) in entries {
        let length = |path: &[u8]| u16::try_from(path.len()).unwrap().to_le_bytes();
        body.extend_from_slice(&flags.to_le_bytes());
        body.extend_from_slice(&length(source));
        body.extend_from_slice(&length(target));
        body.extend_from_slice(source);
        body.extend_from_slice(target);
    }
    body
}

/// Checks that `parts` carry `table` in order, each within the framing's limit, and returns
/// their number.
fn check_parts(parts: &[Vec<u8>], table: &[HostMap]) -> usize {
    let mut read_back = Vec::new();
    for body in parts {
        assert!(body.len() <= MAX_BODY, "{}", body.len());
        assert!(Message::Maps(body.clone()).encode().is_ok());
        let part = decode_maps(body).unwrap();
        assert_eq!(part.first as usize, read_back.len());
        read_back.extend(part.maps);
    }
    assert_eq!(read_back, table.iter().map(read).collect::<Vec<_>>());
    parts.len()
}

#[test]
fn a_mapping_table_travels_in_parts_that_the_guest_reads_in_order() {
    // An empty table needs no part.
    assert_eq!(encode_maps(&[]), Ok(Vec::new()));

    // A part holds the index of its first mapping and their number, then each mapping's
    // flags, the lengths of its source and target, and both.
    let table = [
        mapping("dir", "/srv/dir", true),
        mapping("dir/inner", "/srv/dir/inner", false),
    ];
    let parts = encode_maps(&table).unwrap();
    let expected = part(
        0,
        2,
        &[
            (MAP_READ_ONLY, b"dir", b"/srv/dir"),
            (0, b"dir/inner", b"/srv/dir/inner"),
        ],
    );
    assert_eq!(parts, [expected]);
    assert_eq!(check_parts(&parts, &table), 1);

    // A table of long paths travels in several parts, which continue the table in order.
    let long = "p".repeat(MAX_MAP_PATH_BYTES - 6);
    let table = (0..100)
        .map(|index| {
            let source = format!("{index:03}/{long}");
            mapping(&source, &format!("/{source}"), index % 3 == 0)
        })
        .collect::<Vec<_>>();
    let parts = encode_maps(&table).unwrap();
    assert!(check_parts(&parts, &table) > 1, "{}", parts.len());

    // So does the largest table.
    let largest = (0..MAX_HOST_MAPPINGS)
        .map(|index| mapping(&format!("s{index}"), &format!("/t/{index}"), false))
        .collect::<Vec<_>>();
    check_parts(&encode_maps(&largest).unwrap(), &largest);
}

#[test]
fn mapping_tables_and_parts_that_break_the_rules_are_refused() {
    // The tables that the host cannot send.
    let too_many = vec![mapping("a", "/a", false); MAX_HOST_MAPPINGS + 1];
    let tables = [
        too_many,
        vec![mapping("/a", "/a", false)],
        vec![mapping("a", "a", false)],
        vec![mapping("a/../b", "/a", false)],
        vec![mapping("a", "/a/./b", false)],
        vec![mapping("a//b", "/a", false)],
        vec![mapping("a", "/a\0b", false)],
        vec![mapping(
            "a",
            &format!("/{}", "a".repeat(MAX_MAP_PATH_BYTES)),
            false,
        )],
    ];
    for table in tables {
        let error = encode_maps(&table).unwrap_err();
        assert!(
            error
                .problem()
                .starts_with("the mapping table does not encode: a host mapping"),
            "{error}"
        );
    }

    // The parts that the guest refuses.
    let entry: (u16, &[u8], &[u8]) = (0, b"a", b"/a");
    let cases = [
        (
            Vec::new(),
            "does not decode: the host mapping request is truncated",
        ),
        (vec![0; 7], "is truncated"),
        (part(0, 1, &[]), "is truncated"),
        (
            part(0, 2, &[entry])[..15].to_vec(),
            "does not decode: the host mapping request is truncated",
        ),
        (
            part(0, 1, &[(2, b"a", b"/a")]),
            "does not decode: a host mapping has unknown flags",
        ),
        (part(0, 1, &[(0, b"/a", b"/a")]), "the guest agent refuses"),
        (part(0, 1, &[(0, b"a", b"a")]), "the guest agent refuses"),
        (
            part(0, 1, &[(0, b"a/./b", b"/a")]),
            "the guest agent refuses",
        ),
        (
            part(0, 1, &[(0, b"a", b"/a/..")]),
            "the guest agent refuses",
        ),
        (part(0, 1, &[(0, b"a\0", b"/a")]), "the guest agent refuses"),
        (
            [part(0, 1, &[entry]), vec![0]].concat(),
            "does not decode: the host mapping request has trailing bytes",
        ),
        (
            part(0, 4097, &[]),
            "does not decode: the host mapping request has too many entries",
        ),
        (part(0, 0, &[]), "holds no mapping"),
        (
            part(4096, 1, &[entry]),
            "ends the table at 4097 mappings, more than 4096",
        ),
        (
            part(u32::MAX, 1, &[entry]),
            "ends the table at 4294967296 mappings, more than 4096",
        ),
    ];
    for (body, problem) in cases {
        let error = decode_maps(&body).unwrap_err();
        assert!(
            error.problem().starts_with("a part of the mapping table ")
                && error.problem().contains(problem),
            "{body:?}: {error}"
        );
    }

    // The last mapping of the largest table, whose paths need not be UTF-8 text, as Linux's
    // need not.
    let last = decode_maps(&part(4095, 1, &[(MAP_READ_ONLY, b"\xff", b"/\xfe")])).unwrap();
    assert_eq!(last.first, 4095);
    let expected = GuestHostMap {
        source: vec![0xff],
        target: b"/\xfe".to_vec(),
        read_only: true,
    };
    assert_eq!(last.maps, [expected]);
}

#[test]
fn a_failure_of_the_mapping_table_is_its_count_then_its_problem() {
    let problem = "failed to mount the mapping at /srv/out: the runtime reserves the target";
    let failure = MapFailure::new(2, problem);
    assert_eq!(failure.applied(), 2);
    assert_eq!(failure.problem(), problem);
    let expected = [2u32.to_le_bytes().as_slice(), problem.as_bytes()].concat();
    assert_eq!(failure.encode(), expected);
    assert_eq!(MapFailure::decode(&failure.encode()), Ok(failure.clone()));
    assert_eq!(
        failure.to_string(),
        format!("the mapping table failed after 2 mappings: {problem}")
    );
    assert_eq!(
        MapFailure::new(1, "refused").to_string(),
        "the mapping table failed after 1 mapping: refused"
    );
    assert_eq!(MapFailure::decode(&[0; 4]), Ok(MapFailure::new(0, "")));
    let last = MapFailure::new(u32::MAX, "x");
    assert_eq!(last.encode(), b"\xff\xff\xff\xffx");
    assert_eq!(MapFailure::decode(&last.encode()), Ok(last));

    // A long problem is cut at a character's boundary, so that the body fits its limit.
    let long = format!("x{}", "é".repeat(MAX_TEXT_BODY));
    let failure = MapFailure::new(4096, long);
    assert_eq!(failure.problem().len(), MAX_MAP_PROBLEM_LEN - 1);
    assert_eq!(failure.encode().len(), MAX_TEXT_BODY - 1);
    assert!(Message::MapFailed(failure.encode()).encode().is_ok());
    assert_eq!(MapFailure::decode(&failure.encode()), Ok(failure));

    for (body, problem) in [
        (&b""[..], "ends early"),
        (b"\x01\x00\x00", "ends early"),
        (b"\x01\x00\x00\x00\xffproblem", "is not UTF-8 text"),
    ] {
        let error = MapFailure::decode(body).unwrap_err();
        assert!(
            error
                .problem()
                .starts_with("a failure of the mapping table ")
                && error.problem().contains(problem),
            "{body:?}: {error}"
        );
    }
}
