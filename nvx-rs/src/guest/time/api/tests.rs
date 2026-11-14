use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::Ordering;

use super::*;
use crate::guest::sys::{ClockAdjustment, ClockDiscipline, FrequencyHold};
use crate::guest::time::decoders::{HostClock, PACKET_DNS_SIZE};
use crate::guest::time::state::HostClockStatus;
use crate::guest::time::system::PORTB_TIME_WINDOW;
use crate::guest::time::system::tests::{FakeOps, sample_input, sample_tsc};
use crate::protocol::status;

impl Restore {
    /// Returns an unfinished restore to the new VM generation ID `generation_id`, with nothing to
    /// activate, as a test's fake capture returns one.
    pub(crate) fn to(generation_id: GenerationId) -> Self {
        Self {
            flags: 0,
            online_vp_count: 0,
            generation_id,
            memory_ranges: Vec::new(),
            network: None,
            activation: None,
            finished: false,
            seed: None,
        }
    }

    /// Returns the restore with `seed`, as a capture in the in-memory mode returns it.
    pub(crate) fn holding(mut self, seed: Seed) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Returns the restore with the host's input gated until the guest acknowledges it.
    pub(crate) fn gated(mut self) -> Self {
        self.flags |= decoders::PACKET_ACK_REQUIRED;
        self
    }

    /// Returns the restore whose packet ends with the activation record `record`.
    pub(crate) fn with_activation_record(mut self, record: &[u8]) -> Self {
        self.flags |= decoders::PACKET_ACTIVATION;
        self.activation = Some(record.to_vec());
        self
    }

    /// Returns the restore that asks for `count` online processors.
    pub(crate) fn with_processor_target(mut self, count: u8) -> Self {
        self.online_vp_count = count;
        self
    }

    /// Returns the restore with a memory target, whose expansion ranges are `ranges`, each a
    /// start and a length.
    pub(crate) fn with_memory_target(mut self, ranges: &[(u64, u64)]) -> Self {
        self.flags |= decoders::PACKET_MEMORY_TARGET;
        self.with_memory_ranges(ranges)
    }

    /// Returns the restore that lists `ranges`, each a start and a length, whatever its flags
    /// select.
    pub(crate) fn with_memory_ranges(mut self, ranges: &[(u64, u64)]) -> Self {
        self.memory_ranges = ranges
            .iter()
            .map(|&(start, length)| MemoryRange { start, length })
            .collect();
        self
    }

    /// Returns the restore as one that its capture finished.
    pub(crate) fn finished_by_capture(mut self) -> Self {
        self.finished = true;
        self
    }
}

impl Violation {
    /// Returns the violation with which a strict call fails once the time ABI's part of a
    /// restore's repair failed, with `detail`, as the tests of the ABI's callers need one.
    pub(crate) fn of_repair(detail: &str) -> Self {
        Self::new(
            PowerOffStatus::Repair,
            ViolationCode::RepairClock,
            detail.as_bytes(),
        )
    }

    /// Returns the violation with which a strict finish fails without a running time daemon, a
    /// failure of check C10, as the tests of the ABI's callers need one.
    pub(crate) fn of_missing_daemon() -> Self {
        Self::new(
            PowerOffStatus::Conformance,
            ViolationCode::Conformance(CheckId::C10),
            b"the time daemon is not running",
        )
    }
}

/// Returns whether `error` holds the error number `errno`.
fn has_errno(error: &io::Error, errno: Errno) -> bool {
    error.raw_os_error() == Some(errno as i32)
}

#[test]
fn errors_print_the_diagnostics_of_nvx_time() {
    let cases = [
        (
            TimeError::from(PortError::Grant(Errno::EPERM)),
            format!("ioperm: {}", Errno::EPERM.desc()),
        ),
        (
            TimeError::from(PortError::Busy),
            format!("ioperm: {}", Errno::EBUSY.desc()),
        ),
        (
            TimeError::from(PortError::Revoke(Errno::ENOMEM)),
            format!("ioperm revoke: {}", Errno::ENOMEM.desc()),
        ),
        (
            TimeError::PortLock(Errno::EINTR.into()),
            String::from_utf8(system::portb_lock_failure(Errno::EINTR)).unwrap(),
        ),
        (
            TimeError::StateUnavailable,
            "the time state is unavailable".to_owned(),
        ),
        (
            TimeError::EntropyFile {
                path: PathBuf::from("/run/nvx/restore-entropy"),
                source: Errno::EACCES.into(),
            },
            format!("open /run/nvx/restore-entropy: {}", Errno::EACCES.desc()),
        ),
        (
            TimeError::GenerationIdUnavailable,
            "the VM generation ID is unavailable".to_owned(),
        ),
        (
            TimeError::Spawn(Errno::EAGAIN.into()),
            format!("fork: {}", Errno::EAGAIN.desc()),
        ),
        (
            TimeError::Violation(Violation::new(
                PowerOffStatus::Repair,
                ViolationCode::RepairPacket,
                b"restore packet version is 3",
            )),
            "G_REPAIR_PACKET: restore packet version is 3".to_owned(),
        ),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text, "{error:?}");
    }
    let violation = Violation::new(
        PowerOffStatus::Conformance,
        ViolationCode::Conformance(CheckId::C10),
        b"detail",
    );
    assert_eq!(
        (violation.status(), violation.code(), violation.detail()),
        (193, "G_CONFORMANCE_C10", "detail")
    );
}

#[test]
fn strict_mode_keeps_report_only_mode_and_spares_the_daemon_and_the_preparation() {
    let enforcing = TimeAbi {
        report_only: false,
        strict: false,
    };
    assert!(!enforcing.is_strict() && !enforcing.runtime().is_strict());
    let strict = enforcing.strict();
    assert!(strict.is_strict() && !strict.is_report_only());
    assert!(strict.runtime().is_strict());
    // The daemon and the preparation of a capture enforce the ABI.
    let spared = strict.enforcing_runtime();
    assert!(!spared.is_strict() && !spared.report_only());
    // Report-only mode stays report-only.
    let report_only = TimeAbi::report_only().strict();
    assert!(report_only.is_report_only() && !report_only.is_strict());
    assert!(report_only.runtime().report_only() && !report_only.runtime().is_strict());
    assert_eq!(TimeAbi::report_only(), report_only);
}

#[test]
fn a_strict_call_fails_with_the_violation_that_stops_an_enforcing_one() {
    for (report_only, strict, power) in [
        (false, false, status::TIME_REPAIR),
        (false, true, 0),
        (true, false, 0),
        (true, true, 0),
    ] {
        let ops = FakeOps::new([]);
        let runtime = Runtime::new(&ops, report_only);
        let runtime = if strict { runtime.strict() } else { runtime };
        for read in [
            PacketRead::InvalidHeader(PacketError::Version(3)),
            PacketRead::Generation(3),
        ] {
            let target = RepairTarget {
                previous_id: GenerationId::from_bytes([1; GenerationId::LENGTH]),
                entropy: EntropySink::Seed,
                finish: false,
                daemon_pid: None,
            };
            let error = settled(&runtime, repair_restore(&runtime, read, target, 0));
            let Err(TimeError::Violation(violation)) = error else {
                panic!("{error:?}");
            };
            assert_eq!(violation.status(), status::TIME_REPAIR, "{violation}");
            assert!(violation.code().starts_with("G_REPAIR_"), "{violation}");
            // The call keeps no violation for the next one.
            assert_eq!(runtime.take_violation(), None);
        }
        assert_eq!(ops.power.load(Ordering::Relaxed), usize::from(power));
        // Each violation has its event, in its mode's form.
        let console = String::from_utf8(ops.console.lock().unwrap().clone()).unwrap();
        let prefix = if report_only {
            "NVX-TIME-REPORT-VIOLATION: "
        } else {
            "NVX-TIME-ABI-VIOLATION: "
        };
        assert_eq!(console.matches(prefix).count(), 2, "{console}");
    }
}

#[test]
fn a_call_holds_the_port_permission_for_its_transaction_alone() {
    // Port B offers no VM generation ID.
    let ops = FakeOps::new([0]);
    let runtime = Runtime::new(&ops, true);
    let error = released(read_generation_id(&runtime)).unwrap_err();
    assert!(
        matches!(error, TimeError::GenerationIdUnavailable),
        "{error:?}"
    );
    assert_eq!(ops.transactions(), 1);
    assert_eq!(sys::check_released(), Ok(()));
}

#[test]
fn a_refused_grant_fails_a_call_before_its_port_input_and_output() {
    let mut ops = FakeOps::new([u32::from(PORTB_GENERATION_ID_AVAILABLE)]);
    ops.grant_error = Some(Errno::EPERM);
    let runtime = Runtime::new(&ops, true);
    let error = released(read_generation_id(&runtime)).unwrap_err();
    assert!(
        matches!(&error, TimeError::PortAccess(source) if has_errno(source, Errno::EPERM)),
        "{error:?}"
    );
    assert_eq!(ops.input.lock().unwrap().len(), 1);
    assert!(ops.output.lock().unwrap().is_empty());
    assert_eq!(sys::check_released(), Ok(()));
}

#[test]
fn a_failed_revocation_fails_the_call_and_every_later_one_until_a_revocation_succeeds() {
    let mut ops = FakeOps::new([0, 0]);
    ops.revoke_error = Some(Errno::ENOMEM);
    let error = released(read_generation_id(&Runtime::new(&ops, true))).unwrap_err();
    assert!(
        matches!(&error, TimeError::PortRelease(source) if has_errno(source, Errno::ENOMEM)),
        "{error:?}"
    );
    assert!(matches!(released(Ok(())), Err(TimeError::PortRelease(_))));

    ops.revoke_error = None;
    let error = released(read_generation_id(&Runtime::new(&ops, true))).unwrap_err();
    assert!(
        matches!(error, TimeError::GenerationIdUnavailable),
        "{error:?}"
    );
    assert_eq!(sys::check_released(), Ok(()));
}

#[test]
fn a_preparation_that_may_leave_port_permission_fails_with_its_suppression() {
    let revoke = Err(PortError::Revoke(Errno::ENOMEM));
    // A preparation that suppressed the detectors holds their suppression.
    let error = released_preparation(Ok(()), revoke).unwrap_err();
    assert!(
        matches!(
            &error,
            PreCaptureError::PortRelease {
                error: TimeError::PortRelease(source),
                suppressed: true,
            } if has_errno(source, Errno::ENOMEM)
        ),
        "{error:?}"
    );
    assert!(error.holds_suppression());
    // One that kept the record of the settings that it could not restore holds it too.
    let kept = Err(PreCaptureError::Unrestored(Errno::EIO));
    let error = released_preparation(kept, revoke).unwrap_err();
    assert!(error.holds_suppression(), "{error:?}");
    // One that left the settings as they were holds nothing.
    for failed in [
        PreCaptureError::Pending,
        PreCaptureError::Suppression(Errno::EIO),
    ] {
        let error = released_preparation(Err(failed), revoke).unwrap_err();
        assert!(
            matches!(
                error,
                PreCaptureError::PortRelease {
                    suppressed: false,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(!error.holds_suppression());
    }
    // Without port permission, the preparation's result stands.
    released_preparation(Ok(()), Ok(())).unwrap();
    let error = released_preparation(Err(PreCaptureError::Pending), Ok(())).unwrap_err();
    assert!(matches!(error, PreCaptureError::Pending), "{error:?}");
    assert!(!error.holds_suppression());
    let kept = Err(PreCaptureError::Unrestored(Errno::EIO));
    assert!(
        released_preparation(kept, Ok(()))
            .unwrap_err()
            .holds_suppression()
    );
}

/// Returns the outcome of a cold boot that accepted no sample.
fn cold_boot() -> ColdBoot {
    ColdBoot {
        start_ns: 0,
        cpu_ns: 0,
        accepted_ns: None,
        failures: 1,
    }
}

#[test]
fn the_daemon_starts_only_in_a_process_without_other_threads() {
    assert!(!cold_boot().synchronized());
    // The test harness runs each test on a thread of its own.
    let error = TimeAbi::report_only()
        .start_daemon(cold_boot())
        .unwrap_err();
    assert!(
        matches!(&error, TimeError::Spawn(source) if has_errno(source, Errno::EBUSY)),
        "{error:?}"
    );
    assert_eq!(error.to_string(), format!("fork: {}", Errno::EBUSY.desc()));
}

#[test]
fn the_daemon_does_not_start_while_the_thread_may_hold_port_permission() {
    let mut ops = FakeOps::new([0]);
    ops.revoke_error = Some(Errno::ENOMEM);
    let _ = read_generation_id(&Runtime::new(&ops, true));
    let error = TimeAbi::report_only()
        .start_daemon(cold_boot())
        .unwrap_err();
    assert!(
        matches!(&error, TimeError::PortRelease(source) if has_errno(source, Errno::ENOMEM)),
        "{error:?}"
    );
}

#[test]
fn a_capture_refuses_an_entropy_path_with_a_nul_byte_before_its_transaction() {
    let ops = FakeOps::new([]);
    let runtime = Runtime::new(&ops, true);
    let request = CaptureRequest::new(
        ScratchPolicy::Fresh,
        GenerationId::from_bytes([1; GenerationId::LENGTH]),
        PathBuf::from(OsStr::from_bytes(b"restore\0entropy")),
    );
    let error = capture(&runtime, request).unwrap_err();
    assert!(
        matches!(&error, TimeError::EntropyFile { source, .. } if source.kind() == io::ErrorKind::InvalidInput),
        "{error:?}"
    );
    assert_eq!(ops.transactions(), 0);
}

#[test]
fn requests_and_restores_carry_the_abi_values() {
    assert_eq!(ScratchPolicy::Fresh.request(), 0);
    assert_eq!(ScratchPolicy::Paired.request(), 1);
    let range = MemoryRange {
        start: 1 << 32,
        length: 128 << 20,
    };
    let restore = Restore {
        flags: decoders::PACKET_ACK_REQUIRED | decoders::PACKET_MEMORY_TARGET,
        online_vp_count: 4,
        generation_id: GenerationId::from_bytes([7; GenerationId::LENGTH]),
        memory_ranges: vec![range],
        network: None,
        activation: None,
        finished: false,
        seed: None,
    };
    assert!(restore.acknowledgement_required() && restore.memory_target());
    assert!(!restore.finished());
    assert_eq!(restore.processor_target(), Some(4));
    assert_eq!(restore.memory_ranges(), [range]);
    assert_eq!(restore.network(), None);
    assert_eq!((range.start(), range.length()), (1 << 32, 128 << 20));
    assert_eq!(
        restore.generation_id(),
        GenerationId::from_bytes([7; GenerationId::LENGTH])
    );
    let dns_server = IpAddr::from([10, 0, 0, 1]);
    let networked = Restore {
        network: Some(RestoreNetwork::new(Some(dns_server), true)),
        ..restore
    };
    let network = networked.network().unwrap();
    assert_eq!(network.dns_server(), Some(dns_server));
    assert!(network.announces_listeners());
    let plain = Restore {
        flags: 0,
        online_vp_count: 0,
        memory_ranges: Vec::new(),
        network: None,
        finished: true,
        ..networked
    };
    assert!(!plain.acknowledgement_required() && !plain.memory_target());
    assert!(plain.finished());
    assert_eq!(plain.processor_target(), None);
    assert_eq!(plain.activation_record(), None);
    let activated = plain.with_activation_record(&[0xa1, 0xa2, 0xa3]);
    assert_eq!(activated.activation_record(), Some(&[0xa1, 0xa2, 0xa3][..]));
    assert_eq!(activated.flags(), decoders::PACKET_ACTIVATION);
}

#[test]
fn cpu_sets_hold_the_cpus_that_the_abi_covers() {
    let set = [1, 2, 3, 6]
        .into_iter()
        .try_fold(CpuSet::new(), CpuSet::with)
        .unwrap();
    assert_eq!(set.to_string(), "1-3,6");
    assert_eq!(set.iter().collect::<Vec<_>>(), [1, 2, 3, 6]);
    assert_eq!(
        (set.len(), set.contains(6), set.contains(4)),
        (4, true, false)
    );
    assert_eq!(
        CpuSet::new().with(63).map(|set| set.to_string()),
        Some("63".to_owned())
    );
    assert_eq!(CpuSet::new().with(64), None);
    assert!(CpuSet::new().is_empty());
    assert_eq!(CpuSet::new().to_string(), "none");
    assert_eq!(CpuSet::from(CpuMask::parse(b"1-3,6").unwrap()), set);

    let finish = RestoreFinish::new().acknowledge().new_cpus(set);
    assert!(finish.acknowledge);
    assert_eq!(finish.new_cpus, set);
    assert_eq!(RestoreFinish::new(), RestoreFinish::default());
    assert!(TimeAbi::report_only().is_report_only());
}

#[test]
fn capture_requests_select_how_a_restore_hands_over_its_entropy() {
    let id = GenerationId::from_bytes([1; GenerationId::LENGTH]);
    let path = PathBuf::from("/run/nvx/restore-entropy");
    let file = CaptureRequest::new(ScratchPolicy::Paired, id, path.clone());
    assert_eq!(file.entropy, EntropyMode::File(path));
    let in_memory = CaptureRequest::in_memory(ScratchPolicy::Paired, id);
    assert_eq!(
        (in_memory.scratch, in_memory.generation_id, in_memory.finish),
        (ScratchPolicy::Paired, id, false)
    );
    assert_eq!(in_memory.entropy, EntropyMode::InMemory);
    let finishing = in_memory.clone().finish();
    assert_eq!(
        (finishing.entropy, finishing.finish),
        (EntropyMode::InMemory, true)
    );
    assert_ne!(file, in_memory);
}

#[test]
fn a_restore_hands_over_its_seed_once_and_formats_none_of_it() {
    let seed = Seed::from_bytes([7; Seed::LENGTH]);
    let mut restore = Restore {
        flags: 0,
        online_vp_count: 0,
        generation_id: seed.generation_id(),
        memory_ranges: Vec::new(),
        network: None,
        activation: None,
        finished: false,
        seed: Some(seed),
    };
    let text = format!("{restore:?}");
    assert!(text.ends_with(", seed: Some(Seed { .. }) }"), "{text}");
    let seed = restore.take_seed().unwrap();
    assert_eq!(seed.generation_id(), restore.generation_id());
    assert_eq!(*seed.bytes(), [7; Seed::LENGTH]);
    assert!(restore.take_seed().is_none());
    assert!(format!("{restore:?}").ends_with(", seed: None }"));
}

#[test]
fn the_file_mode_writes_the_entropy_and_the_in_memory_mode_returns_it() {
    let entropy: [u8; Seed::LENGTH] = std::array::from_fn(|index| index as u8 ^ 0x5a);
    let directory = tempfile::tempdir().unwrap();
    let file_path = directory.path().join("restore-entropy");
    let path = sys::path_c_string(&file_path).unwrap();
    let open = || sys::open(&path, ENTROPY_FLAGS, PRIVATE_FILE_MODE).unwrap();
    let sink = EntropySink::File {
        path: path.clone(),
        file: open(),
    };
    assert!(sink.deliver(Seed::from_bytes(entropy)).unwrap().is_none());
    assert_eq!(fs::read(&file_path).unwrap(), entropy);
    let mode = fs::metadata(&file_path).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o600);
    // The in-memory mode returns the seed, and writes nothing.
    let seed = EntropySink::Seed.deliver(Seed::from_bytes(entropy));
    assert_eq!(seed.unwrap().map(|seed| *seed.bytes()), Some(entropy));
    EntropySink::Seed.discard();
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    // A capture that continues removes the file mode's file.
    let file = open();
    EntropySink::File { path, file }.discard();
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

/// Returns the port input with which a restore answers a capture: port B's status, then a
/// packet of `generation`, which carries `entropy` and nothing to activate, in four-byte
/// reads, and then port B's status, with the time window, and a time sample of the
/// generation, within the bound.
fn restore_input(generation: u32, entropy: &[u8; Seed::LENGTH]) -> Vec<u32> {
    restore_input_with(generation, entropy, None)
}

/// Returns the port input of [`restore_input`], whose packet ends with the activation record
/// `record`, if any.
fn restore_input_with(
    generation: u32,
    entropy: &[u8; Seed::LENGTH],
    record: Option<&[u8]>,
) -> Vec<u32> {
    let mut packet = vec![0_u8; PACKET_HEADER_SIZE + Seed::LENGTH + PACKET_DNS_SIZE];
    packet[..4].copy_from_slice(b"OVR\x01");
    packet[8..12].copy_from_slice(&generation.to_le_bytes());
    packet[PACKET_HEADER_SIZE..][..Seed::LENGTH].copy_from_slice(entropy);
    if let Some(record) = record {
        packet[4] |= decoders::PACKET_ACTIVATION;
        packet.extend_from_slice(&u32::try_from(record.len()).unwrap().to_le_bytes());
        packet.extend_from_slice(record);
        // The device's last read of the packet returns bytes past it, which the guest ignores.
        packet.resize(packet.len().next_multiple_of(size_of::<u32>()), 0xee);
    }
    let words = packet
        .chunks(size_of::<u32>())
        .map(|word| u32::from_ne_bytes(word.try_into().unwrap()));
    let sample = sample_input(
        generation,
        FakeOps::REALTIME_NS as u64,
        sample_tsc(0),
        400,
        HostClock::default(),
    );
    std::iter::once(u32::from(PORTB_PACKET_AVAILABLE))
        .chain(words)
        .chain(std::iter::once(u32::from(PORTB_TIME_WINDOW)))
        .chain(sample)
        .collect()
}

#[test]
fn a_packet_read_takes_its_activation_record_before_the_time_sample() {
    let entropy: [u8; Seed::LENGTH] = std::array::from_fn(|index| index as u8 + 2);
    // Records whose last read is whole, and partial.
    for record in [&[0xa1, 0xa2, 0xa3, 0xa4][..], &[0xb1; 7][..]] {
        // Without port B's status, which the capture reads before the packet.
        let input = restore_input_with(1, &entropy, Some(record))[1..].to_vec();
        let ops = FakeOps::new(input);
        let runtime = Runtime::new(&ops, true);
        let ports = runtime.open_ports().unwrap();
        let read = read_packet(&runtime, &ports, 0);
        ports.release().unwrap();
        // The time sample after the record decodes only if the packet's reads took the record
        // whole, and nothing past it.
        let PacketRead::Packet(Ok((packet, Ok(_)))) = read else {
            panic!("the packet with a record of {} bytes failed", record.len());
        };
        assert_eq!(packet.activation.as_deref(), Some(record));
        assert!(ops.input.lock().unwrap().is_empty());
    }
}

/// Returns whether a file below `directory` holds `secret`, as bytes or in hexadecimal.
fn holds(directory: &Path, secret: &[u8]) -> bool {
    let hex = decoders::format_hex(secret);
    fs::read_dir(directory).unwrap().any(|entry| {
        let path = entry.unwrap().path();
        if path.is_dir() {
            return holds(&path, secret);
        }
        let contents = fs::read(&path).unwrap();
        contents.windows(secret.len()).any(|bytes| bytes == secret)
            || contents
                .windows(hex.len())
                .any(|text| text == hex.as_bytes())
    })
}

/// Captures through the fakes, in the in-memory mode and then in the file mode, a restore,
/// then a VM that continues, and then a hold that the kernel refuses, on a tmpfs at `/run` in a
/// mount namespace of the process's own.
fn capture_in_each_mode() {
    sys::enter_private_mount_namespace().unwrap();
    let _run = sys::TmpfsMount::mount(c"nvx-test", c"/run", c"mode=0755").unwrap();
    system::make_runtime_directories();
    state_store(&TimeState::default()).unwrap();
    let previous = GenerationId::from_bytes([1; GenerationId::LENGTH]);
    let entropy: [u8; Seed::LENGTH] = std::array::from_fn(|index| index as u8 + 2);
    let secret = &entropy[GenerationId::LENGTH..];
    let path = Path::new("/run/nvx/restore-entropy");
    // The discipline that the capture finds: the frequency correction that the host's asks for,
    // and the status and error estimates of a synchronized clock.
    let found = ClockDiscipline {
        scaled_ppm: -983_040,
        maximum_error_us: 40,
        estimated_error_us: 2,
        status: 0x2081,
    };
    // Returns the capture's outcome, what it wrote to the kernel log and the console, and its
    // adjustments of the clock.
    let capture_with = |input: Vec<u32>, request| {
        let ops = FakeOps::new(input);
        *ops.discipline.lock().unwrap() = found;
        let outcome = capture(&Runtime::new(&ops, true), request).unwrap();
        let mut logged = ops.kernel_log.lock().unwrap().clone();
        logged.extend_from_slice(&ops.console.lock().unwrap());
        let adjustments = ops.adjustments.lock().unwrap().clone();
        (outcome, logged, adjustments)
    };
    // Every request first holds the clock at the declared rate, which the fake's 2 GHz is the
    // kernel's own, without the correction that it found, and keeps the status and the error
    // estimates that it found.
    let held = FrequencyHold {
        scaled_ppm: 0,
        maximum_error_us: 40,
        estimated_error_us: 2,
    };
    let pin = ClockAdjustment::Pin {
        hold: held,
        status: 0x2081,
    };

    // The in-memory mode returns the entropy as the restore's seed, and writes no file.
    let request = CaptureRequest::in_memory(ScratchPolicy::Fresh, previous);
    let (CaptureOutcome::Restored(mut restore), logged, adjustments) =
        capture_with(restore_input(1, &entropy), request)
    else {
        panic!("the in-memory capture restored nothing");
    };
    assert_eq!(logged, b"");
    // The repair of the restore reads the clock's frequency, which it pairs its time sample at,
    // and holds the host's correction after its step, as it did before.
    assert_eq!(adjustments[..2], [ClockAdjustment::Query, pin]);
    assert!(
        matches!(
            adjustments[2..],
            [
                ClockAdjustment::Query,
                ClockAdjustment::Step { .. },
                ClockAdjustment::Frequency(_)
            ]
        ),
        "{adjustments:?}"
    );
    let id = restore.generation_id().to_bytes();
    assert_eq!(id[..], entropy[..GenerationId::LENGTH]);
    assert_eq!(restore.take_seed().map(|seed| *seed.bytes()), Some(entropy));
    assert!(!path.exists() && !holds(Path::new("/run"), secret));
    let request = CaptureRequest::in_memory(ScratchPolicy::Fresh, previous);
    let (outcome, _, adjustments) = capture_with(vec![0], request);
    assert!(matches!(outcome, CaptureOutcome::Continued), "{outcome:?}");
    // A VM that continues resumes the frequency correction that it found.
    assert_eq!(
        adjustments,
        [
            ClockAdjustment::Query,
            pin,
            ClockAdjustment::SetFrequency(found.scaled_ppm),
        ]
    );

    // The file mode, which the tools keep, writes the entropy to a file that only its owner
    // may read and write, and the restore holds no seed.
    let request = CaptureRequest::new(ScratchPolicy::Paired, previous, path.to_owned());
    let (CaptureOutcome::Restored(mut restore), logged, _) =
        capture_with(restore_input(2, &entropy), request)
    else {
        panic!("the file mode's capture restored nothing");
    };
    assert_eq!(logged, b"");
    assert!(restore.take_seed().is_none());
    assert_eq!(fs::read(path).unwrap(), entropy);
    let mode = fs::metadata(path).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o600);
    // A VM that continues has the file removed.
    let request = CaptureRequest::new(ScratchPolicy::Fresh, previous, path.to_owned());
    let (outcome, _, _) = capture_with(vec![0], request);
    assert!(matches!(outcome, CaptureOutcome::Continued), "{outcome:?}");
    assert!(!path.exists());

    // A kernel that refuses the hold fails the request before it writes the snapshot port, and
    // the failure's event reaches the debug console in a port transaction of its own.
    let mut ops = FakeOps::new([]);
    ops.adjust_error = Some(Errno::EPERM);
    let runtime = Runtime::new(&ops, true);
    let request = CaptureRequest::new(ScratchPolicy::Fresh, previous, path.to_owned());
    let error = capture(&runtime, request).unwrap_err();
    let TimeError::Violation(violation) = &error else {
        panic!("{error:?}");
    };
    assert_eq!(
        (violation.status(), violation.code()),
        (status::TIME_CONFORMANCE, "G_CONFORMANCE_C10")
    );
    assert_eq!(
        violation.detail(),
        format!(
            "cannot hold the clock at the declared rate: adjtimex: {}",
            Errno::EPERM.desc()
        )
    );
    let output = ops.output.lock().unwrap();
    assert!(
        output.iter().all(|&(port, _)| port == Port::PortbData),
        "{output:?}"
    );
    let event = String::from_utf8(output.iter().map(|&(_, byte)| byte).collect()).unwrap();
    assert!(event.contains("G_CONFORMANCE_C10"), "{event}");
    assert!(!path.exists());
}

#[test]
fn privileged_captures_hand_over_the_entropy_in_their_mode() {
    if crate::guest::managed_agent::tests::isolate_process_test(
        "NVX_TIME_CAPTURE_PRIVILEGED_TEST_CHILD",
        "guest::time::api::tests::privileged_captures_hand_over_the_entropy_in_their_mode",
    ) {
        return;
    }
    if !sys::tests::holds_capability(sys::tests::CAP_SYS_ADMIN).unwrap() {
        return;
    }
    // A capture keeps its state, its locks, and the file mode's entropy file in `/run/nvx`,
    // so a child of the harness's process makes the calls alone in a mount namespace of its
    // own, which only a process with one thread may enter.
    match sys::tests::fork_isolated_test().unwrap() {
        ForkResult::Child => {
            let outcome = std::panic::catch_unwind(capture_in_each_mode);
            sys::exit_immediately(u8::from(outcome.is_err()));
        }
        ForkResult::Parent(child) => {
            assert_eq!(sys::wait(child), Some(sys::ChildStatus::Exited(0)));
        }
    }
}

/// Runs, through the fakes, cold boots whose initial sample fails check C12, in strict mode
/// and then in report-only mode, on a tmpfs at `/run` in a mount namespace of the process's
/// own.
fn cold_boot_in_strict_and_report_only_mode() {
    sys::enter_private_mount_namespace().unwrap();
    let _run = sys::TmpfsMount::mount(c"nvx-test", c"/run", c"mode=0755").unwrap();
    // Port B offers no time-sample window, so the initial sample fails check C12.
    let ops = FakeOps::new([0]);
    let runtime = Runtime::new(&ops, false).strict();
    let error = super::cold_boot(&runtime);
    let Err(TimeError::Violation(violation)) = error else {
        panic!("{error:?}");
    };
    assert_eq!(
        (violation.status(), violation.code()),
        (status::TIME_CONFORMANCE, "G_CONFORMANCE_C12")
    );
    // A strict cold boot stops where enforcing mode stops the VM, without stopping it: it
    // publishes no time state.
    assert_eq!(ops.power.load(Ordering::Relaxed), 0);
    assert!(state_load().is_err());
    assert_eq!(runtime.take_violation(), None);
    let console = String::from_utf8(ops.console.lock().unwrap().clone()).unwrap();
    assert!(
        console.starts_with(
            "NVX-TIME-ABI-VIOLATION: v=1 code=G_CONFORMANCE_C12 source=conformance phase=boot "
        ),
        "{console}"
    );
    // A report-only cold boot goes on, and leaves the wall clock unsynchronized.
    let ops = FakeOps::new([0]);
    let boot = super::cold_boot(&Runtime::new(&ops, true)).unwrap();
    assert!(!boot.synchronized());
    assert_eq!(boot.failures, 1);
    assert!(!state_load().unwrap().synchronized);
    // A sample beyond the bound fails check C12 too.
    let host = HostClock {
        status: HostClockStatus::FreeRunning,
        disruption: 0,
        correction: 13_107_200,
        correction_bound: None,
        max_error_us: Some(16_000_000),
    };
    let utc_ns = FakeOps::REALTIME_NS as u64 + 1_000;
    let window = |pairing_ns| {
        std::iter::once(u32::from(PORTB_TIME_WINDOW))
            .chain(sample_input(0, utc_ns, sample_tsc(0), pairing_ns, host))
            .collect::<Vec<_>>()
    };
    let ops = FakeOps::new(window(150_000));
    let boot = super::cold_boot(&Runtime::new(&ops, true)).unwrap();
    assert_eq!((boot.synchronized(), boot.failures), (false, 1));
    let console = String::from_utf8(ops.console.lock().unwrap().clone()).unwrap();
    assert!(
        console.contains("time sample uncertainty 151026 ns exceeds 100000 ns"),
        "{console}"
    );
    // A sample within the bound steps the wall clock to host UTC, holds the frequency that
    // the host's correction asks for, and publishes the host's discipline. A free-running
    // host's error does not count.
    let ops = FakeOps::new(window(400));
    let boot = super::cold_boot(&Runtime::new(&ops, false).strict()).unwrap();
    assert_eq!((boot.synchronized(), boot.failures), (true, 0));
    assert_eq!(
        ops.adjustments.lock().unwrap().as_slice(),
        [
            ClockAdjustment::Query,
            ClockAdjustment::Step {
                seconds: 0,
                nanoseconds: 1_050,
            },
            ClockAdjustment::Frequency(FrequencyHold {
                scaled_ppm: 13_107_200,
                maximum_error_us: 2,
                estimated_error_us: 2,
            }),
        ]
    );
    let state = state_load().unwrap();
    assert!(state.synchronized);
    assert_eq!(
        (state.samples, state.offset_ns, state.uncertainty_ns),
        (1, 1_050, 1_426)
    );
    assert_eq!(
        (state.frequency_ppb, state.frequency_limited),
        (200_000, false)
    );
    assert_eq!(
        (
            state.host_status,
            state.host_correction_ppb,
            state.host_correction_bound_ppb,
            state.host_max_error_us
        ),
        (
            HostClockStatus::FreeRunning,
            200_000,
            None,
            Some(16_000_000)
        )
    );
    assert!(!state.host_test_hooks);
    // The state says when host test hooks altered the initial sample.
    let mut hooked = window(400);
    // The sample's first word, after port B's status, holds its version, flags, status, and
    // reserved bytes.
    hooked[1] |= u32::from_ne_bytes([0, 0x08, 0, 0]);
    let ops = FakeOps::new(hooked);
    let boot = super::cold_boot(&Runtime::new(&ops, false).strict()).unwrap();
    assert!(boot.synchronized());
    assert!(state_load().unwrap().host_test_hooks);
}

#[test]
fn a_privileged_strict_cold_boot_stops_at_its_violation() {
    if crate::guest::managed_agent::tests::isolate_process_test(
        "NVX_TIME_COLD_BOOT_PRIVILEGED_TEST_CHILD",
        "guest::time::api::tests::a_privileged_strict_cold_boot_stops_at_its_violation",
    ) {
        return;
    }
    if !sys::tests::holds_capability(sys::tests::CAP_SYS_ADMIN).unwrap() {
        return;
    }
    // A cold boot keeps its state and its locks in `/run/nvx`, which a child of the
    // harness's process holds alone in a mount namespace of its own.
    match sys::tests::fork_isolated_test().unwrap() {
        ForkResult::Child => {
            let outcome = std::panic::catch_unwind(cold_boot_in_strict_and_report_only_mode);
            sys::exit_immediately(u8::from(outcome.is_err()));
        }
        ForkResult::Parent(child) => {
            assert_eq!(sys::wait(child), Some(sys::ChildStatus::Exited(0)));
        }
    }
}
