use super::*;
use crate::guest::GenerationId;
use crate::guest::sys::Errno;
use crate::guest::time::{Restore, Violation};
use std::thread;
use std::time::Duration;

/// A capture seam that records ordering and requires each operation to be called once.
struct CaptureSystem {
    /// Calls in execution order.
    calls: Vec<&'static str>,
    /// The caller's deadline until the request context clears it.
    deadline: Option<Instant>,
    /// The original deadline, retained only to force expiry at a chosen test step.
    original_deadline: Instant,
    /// The calling namespace's mount table.
    table: Vec<u8>,
    /// The next preparation's result and suppression ownership.
    preparation: Option<Result<(), (PreCaptureError, bool)>>,
    /// The next generation read.
    generation: Option<Result<GenerationId, TimeError>>,
    /// The next request's outcome.
    outcome: Option<Result<CaptureOutcome, TimeError>>,
    /// Whether detector restoration fails.
    release_fails: bool,
    /// Whether the generation read deliberately completes after the deadline.
    expire_generation: bool,
    /// Whether the unbounded request deliberately completes after the original deadline.
    expire_request: bool,
}

impl CaptureSystem {
    /// Returns a successful template capture with the caller's deadline.
    fn new(deadline: Instant) -> Self {
        Self {
            calls: Vec::new(),
            deadline: Some(deadline),
            original_deadline: deadline,
            table: b"1 1 0:2 / / rw - rootfs rootfs rw\n".to_vec(),
            preparation: Some(Ok(())),
            generation: Some(Ok(GenerationId::from_bytes([7; GenerationId::LENGTH]))),
            outcome: Some(Ok(CaptureOutcome::Continued)),
            release_fails: false,
            expire_generation: false,
            expire_request: false,
        }
    }
}

impl System for CaptureSystem {
    fn mount_table(&mut self) -> io::Result<Vec<u8>> {
        self.calls.push("mounts");
        Ok(self.table.clone())
    }

    fn windows(&mut self, _: u64) -> io::Result<Vec<PathBuf>> {
        self.calls.push("window");
        Ok(Vec::new())
    }

    fn block_device(&mut self, _: DeviceNumber) -> io::Result<PathBuf> {
        panic!("this fixture has no block-backed mounts");
    }

    fn pre_capture(&mut self) -> Result<(), PreCaptureError> {
        panic!("the single-call capture must use the strict bounded preparation");
    }

    fn pre_capture_until(&mut self, deadline: Instant) -> Result<(), (PreCaptureError, bool)> {
        assert_eq!(self.deadline, Some(deadline));
        self.calls.push("prepare");
        self.preparation.take().expect("preparation runs once")
    }

    fn clear_deadline(&mut self) {
        self.calls.push("unbounded");
        self.deadline = None;
    }

    fn cancel_capture(&mut self) -> Result<(), Vec<u8>> {
        self.calls.push("release");
        sys::FileLock::check_deadline(self.deadline)
            .map_err(|error| error.desc().as_bytes().to_vec())?;
        if self.release_fails {
            Err(b"detector restore failed".to_vec())
        } else {
            Ok(())
        }
    }

    fn generation_id(&mut self) -> Result<GenerationId, TimeError> {
        self.calls.push("generation");
        if self.expire_generation {
            expire(self.original_deadline);
        }
        self.generation.take().expect("generation is read once")
    }

    fn capture(&mut self, request: CaptureRequest) -> Result<CaptureOutcome, TimeError> {
        assert_eq!(self.deadline, None);
        assert_eq!(
            request,
            CaptureRequest::in_memory(
                ScratchPolicy::Fresh,
                GenerationId::from_bytes([7; GenerationId::LENGTH]),
            )
        );
        self.calls.push("request");
        if self.expire_request {
            expire(self.original_deadline);
        }
        self.outcome.take().expect("capture is requested once")
    }
}

/// Forces expiry only after the operation under test has started.
fn expire(deadline: Instant) {
    thread::sleep(deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1));
    assert!(Instant::now() >= deadline);
}

/// The canonical MMIO identities, independent of the legacy root Device type.
fn capture_devices() -> Devices {
    Devices {
        distro: 0xd000_3000,
        scratch: 0xd000_6000,
        filesystem_tag: "microvm".into(),
    }
}

#[test]
fn fresh_capture_orders_the_existing_steps_and_does_not_deadline_the_request() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut system = CaptureSystem::new(deadline);
    system.expire_request = true;
    assert_eq!(
        capture_with(&mut system, &capture_devices(), deadline).unwrap(),
        Captured::Continued
    );
    assert!(Instant::now() > deadline);
    assert_eq!(
        system.calls,
        [
            "mounts",
            "window",
            "window",
            "prepare",
            "generation",
            "unbounded",
            "request",
            "release",
        ]
    );
}

#[test]
fn a_pre_request_failure_resumes_only_after_its_detector_restoration_succeeds() {
    for release_fails in [false, true] {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut system = CaptureSystem::new(deadline);
        system.generation = Some(Err(TimeError::GenerationIdUnavailable));
        system.release_fails = release_fails;
        let failure = capture_with(&mut system, &capture_devices(), deadline).unwrap_err();
        if release_fails {
            assert_eq!(failure.stage, CaptureStage::Release);
            assert_eq!(
                failure.disposition,
                Disposition::MustEnd {
                    status: status::TIME_REPAIR
                }
            );
        } else {
            assert_eq!(failure.stage, CaptureStage::GenerationId);
            assert_eq!(failure.disposition, Disposition::Resumed);
        }
        assert_eq!(system.calls.last(), Some(&"release"));
        assert!(!system.calls.contains(&"request"));
    }
}

#[test]
fn strict_time_status_is_retained_when_detector_rollback_also_fails() {
    for violation in [
        Violation::of_missing_daemon(),
        Violation::of_repair("repair failed"),
    ] {
        let status = violation.status();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut system = CaptureSystem::new(deadline);
        system.preparation = Some(Err((
            PreCaptureError::CaptureChecks(TimeError::Violation(violation)),
            true,
        )));
        system.release_fails = true;
        let failure = capture_with(&mut system, &capture_devices(), deadline).unwrap_err();
        assert_eq!(failure.disposition, Disposition::MustEnd { status });
        assert_eq!(failure.stage, CaptureStage::Release);
        assert!(!system.calls.contains(&"generation"));
    }
}

#[test]
fn port_release_is_fatal_and_keeps_its_cause_at_each_time_stage_despite_rollback_failure() {
    for stage in [
        CaptureStage::CaptureChecks,
        CaptureStage::GenerationId,
        CaptureStage::Capture,
    ] {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut system = CaptureSystem::new(deadline);
        let port = || TimeError::PortRelease(Errno::ENOMEM.into());
        match stage {
            CaptureStage::CaptureChecks => {
                system.preparation = Some(Err((
                    PreCaptureError::PortRelease {
                        error: port(),
                        suppressed: true,
                    },
                    true,
                )));
            }
            CaptureStage::GenerationId => system.generation = Some(Err(port())),
            CaptureStage::Capture => system.outcome = Some(Err(port())),
            _ => unreachable!(),
        }
        system.release_fails = true;
        let failure = capture_with(&mut system, &capture_devices(), deadline).unwrap_err();
        assert_eq!(failure.stage, stage);
        assert!(
            matches!(failure.cause, CaptureCause::Time(TimeError::PortRelease(ref error))
            if error.raw_os_error() == Some(Errno::ENOMEM as i32))
        );
        assert_eq!(
            failure.disposition,
            Disposition::MustEnd {
                status: status::TIME_REPAIR
            }
        );
        assert_eq!(system.calls.last(), Some(&"release"));
    }
}

#[test]
fn an_expired_pre_request_deadline_prevents_capture_and_does_not_extend_rollback() {
    let deadline = Instant::now();
    let mut system = CaptureSystem::new(deadline);
    let failure = capture_with(&mut system, &capture_devices(), deadline).unwrap_err();
    assert_eq!(failure.stage, CaptureStage::Mounts);
    assert_eq!(failure.disposition, Disposition::Resumed);
    assert!(system.calls.is_empty());

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut system = CaptureSystem::new(deadline);
    system.expire_generation = true;
    let failure = capture_with(&mut system, &capture_devices(), deadline).unwrap_err();
    assert_eq!(failure.stage, CaptureStage::Release);
    assert_eq!(
        failure.disposition,
        Disposition::MustEnd {
            status: status::TIME_REPAIR
        }
    );
    assert!(!system.calls.contains(&"request"));
    assert_eq!(system.calls.last(), Some(&"release"));
}

#[test]
fn a_fresh_restore_refuses_even_an_otherwise_empty_network_policy() {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut system = CaptureSystem::new(deadline);
    system.outcome = Some(Ok(CaptureOutcome::Restored(
        Restore::to(GenerationId::from_bytes([8; GenerationId::LENGTH])).with_network(None, false),
    )));
    let failure = capture_with(&mut system, &capture_devices(), deadline).unwrap_err();
    assert_eq!(failure.stage, CaptureStage::Capture);
    assert!(matches!(failure.cause, CaptureCause::Io(ref error)
        if error.kind() == io::ErrorKind::Unsupported && error.to_string().contains("network")));
    assert_eq!(
        failure.disposition,
        Disposition::MustEnd {
            status: status::TIME_REPAIR
        }
    );
    assert_eq!(system.calls.last(), Some(&"release"));
}

#[test]
fn a_pending_capture_is_not_released_and_a_restore_without_network_is_not_repaired() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut system = CaptureSystem::new(deadline);
    system.preparation = Some(Err((PreCaptureError::Pending, false)));
    let failure = capture_with(&mut system, &capture_devices(), deadline).unwrap_err();
    assert!(matches!(failure.cause, CaptureCause::Pending));
    assert_eq!(failure.disposition, Disposition::Resumed);
    assert!(!system.calls.contains(&"release"));
    assert!(!system.calls.contains(&"request"));

    let mut system = CaptureSystem::new(deadline);
    system.outcome = Some(Ok(CaptureOutcome::Restored(Restore::to(
        GenerationId::from_bytes([8; GenerationId::LENGTH]),
    ))));
    let failure = capture_with(&mut system, &capture_devices(), deadline).unwrap_err();
    assert_eq!(failure.stage, CaptureStage::Capture);
    assert_eq!(
        failure.disposition,
        Disposition::MustEnd {
            status: status::TIME_REPAIR
        }
    );
    assert!(matches!(failure.cause, CaptureCause::Io(ref error)
        if error.to_string().contains("does not repair")));
}

#[test]
fn request_and_release_failures_never_report_that_the_guest_resumed() {
    for release_fails in [false, true] {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut system = CaptureSystem::new(deadline);
        system.release_fails = release_fails;
        if !release_fails {
            system.outcome = Some(Err(TimeError::StateUnavailable));
        }
        let failure = capture_with(&mut system, &capture_devices(), deadline).unwrap_err();
        assert_eq!(
            failure.stage,
            if release_fails {
                CaptureStage::Release
            } else {
                CaptureStage::Capture
            }
        );
        assert_eq!(
            failure.disposition,
            Disposition::MustEnd {
                status: status::TIME_REPAIR
            }
        );
    }
}

#[test]
fn a_mount_refusal_never_prepares_or_requests_capture() {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut system = CaptureSystem::new(deadline);
    system.table = b"1 1 0:2 / /root rw - overlay overlay rw\n".to_vec();
    let failure = capture_with(&mut system, &capture_devices(), deadline).unwrap_err();
    assert_eq!(failure.stage, CaptureStage::Mounts);
    assert!(
        matches!(failure.cause, CaptureCause::Mounted(ref mounted) if mounted.origin() == Origin::Overlay)
    );
    assert_eq!(failure.disposition, Disposition::Resumed);
    assert!(!system.calls.contains(&"prepare"));
    assert!(!system.calls.contains(&"release"));
}

/// Returns the line that the kernel writes for a mount of `filesystem` from `source` at
/// `mount_point`, on the device `device`, with the optional fields `optional`.
fn line(device: &str, mount_point: &str, optional: &str, filesystem: &str, source: &str) -> String {
    format!("36 25 {device} / {mount_point} rw,relatime{optional} - {filesystem} {source} rw")
}

#[test]
fn a_line_tells_the_device_mount_point_type_and_source_of_its_mount() {
    let text = line(
        "254:2",
        "/run/nvx/layers/p2",
        " shared:7 master:1",
        "ext4",
        "/dev/vda2",
    );
    assert_eq!(
        Line::parse(text.as_bytes()),
        Some(Line {
            device: DeviceNumber {
                major: 254,
                minor: 2
            },
            mount_point: PathBuf::from("/run/nvx/layers/p2"),
            filesystem: b"ext4".to_vec(),
            source: b"/dev/vda2".to_vec(),
        })
    );
    // Without optional fields or the file system's options.
    let parsed = Line::parse(b"1 1 0:2 / / rw - rootfs rootfs").unwrap();
    assert_eq!(parsed.device, DeviceNumber { major: 0, minor: 2 });
    assert_eq!(parsed.filesystem, b"rootfs");
    assert_eq!(parsed.source, b"rootfs");
    // A source that the mounter left empty, and file system options of any form.
    let parsed = Line::parse(b"36 25 0:50 / /mnt rw - tmpfs  rw  x").unwrap();
    assert_eq!(parsed.filesystem, b"tmpfs");
    assert_eq!(parsed.source, b"");
    assert_eq!(
        DeviceNumber {
            major: 259,
            minor: 0
        }
        .to_string(),
        "259:0"
    );
}

#[test]
fn escaped_fields_are_unescaped() {
    let text = line(
        "0:31",
        "/mnt/a\\040b\\011c\\012d\\134e",
        "",
        "virt\\134iofs",
        "tag\\040x",
    );
    let parsed = Line::parse(text.as_bytes()).unwrap();
    assert_eq!(parsed.mount_point, Path::new("/mnt/a b\tc\nd\\e"));
    assert_eq!(parsed.filesystem, b"virt\\iofs");
    assert_eq!(parsed.source, b"tag x");
    // A backslash that starts no escape stays, and so does one before a byte above 0o377.
    assert_eq!(unescape(b"a\\b\\04\\400\\377"), b"a\\b\\04\\400\xff");
}

#[test]
fn lines_of_another_form_are_refused() {
    for text in [
        "",
        "36 25 254:2 / /mnt rw - ext4",
        "36 25 254:2 / /mnt rw shared:1 ext4 /dev/vda rw",
        "36 25 254:2 / /mnt rw",
        "x 25 254:2 / /mnt rw - ext4 /dev/vda rw",
        "36 -1 254:2 / /mnt rw - ext4 /dev/vda rw",
        "36 25 254 / /mnt rw - ext4 /dev/vda rw",
        "36 25 254:x / /mnt rw - ext4 /dev/vda rw",
        "36 25 4294967296:0 / /mnt rw - ext4 /dev/vda rw",
        "36 25 +254:0 / /mnt rw - ext4 /dev/vda rw",
        "36 25  254:0 / /mnt rw - ext4 /dev/vda rw",
        "36 25 254:0 /  /mnt rw - ext4 /dev/vda rw",
        "36 25 254:0 / /mnt rw  shared:1 - ext4 /dev/vda rw",
        "36 25 254:0 / /mnt rw -  ext4 /dev/vda rw",
    ] {
        assert_eq!(Line::parse(text.as_bytes()), None, "{text:?}");
    }
}
