use std::cell::{Cell, RefCell};

use super::*;
use crate::guest::snapshot::{CaptureCause, CaptureStage, Disposition};

impl Capture {
    /// Supplies only a helper registry; no unit check can sweep the host through this fixture.
    pub(crate) fn fixture(fresh: bool) -> Self {
        Self::new(
            Helpers {
                time: pid(42),
                announcer: Some(pid(43)),
            },
            fresh,
        )
    }
}

#[test]
fn capture_preparation_and_acknowledgement_precede_one_synchronous_call() {
    let mut state = Capture::fixture(true);
    assert!(state.queue(17, true));
    assert!(!state.queue(18, true));
    let calls = RefCell::new(Vec::new());
    let deadline = state.pending.unwrap().2;
    let returned = state
        .perform_with(
            |kill| {
                assert!(kill);
                calls.borrow_mut().push("kill-and-sweep");
                Ok(())
            },
            |request_id| {
                assert_eq!(request_id, 17);
                calls.borrow_mut().push("ack");
                Ok(())
            },
            |devices, disks, actual_deadline| {
                assert_eq!(*calls.borrow(), ["kill-and-sweep", "ack"]);
                assert_eq!(devices.distro, DISTRO_MMIO_BASE);
                assert_eq!(devices.scratch, SCRATCH_MMIO_BASE);
                assert_eq!(devices.filesystem_tag, "microvm");
                assert_eq!(disks, Disks::Fresh);
                assert_eq!(actual_deadline, deadline);
                calls.borrow_mut().push("capture");
                Ok(Captured::Continued)
            },
        )
        .unwrap();
    assert!(matches!(
        returned,
        Some(CaptureStep::Returned {
            request_id: 17,
            result: Ok(Captured::Continued)
        })
    ));
    assert_eq!(*calls.borrow(), ["kill-and-sweep", "ack", "capture"]);
    assert!(
        state
            .perform_with(
                |_| panic!("no second preparation"),
                |_| panic!("no second acknowledgement"),
                |_, _, _| panic!("no second snapshot call"),
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn refused_preparation_or_failed_acknowledgement_never_calls_snapshot() {
    for refusal in [Errno::EBUSY, Errno::EIO] {
        let mut state = Capture::fixture(true);
        assert!(state.queue(19, false));
        assert!(matches!(
            state
                .perform_with(
                    |kill| {
                        assert!(!kill);
                        Err(refusal)
                    },
                    |_| panic!("a refused request is not acknowledged"),
                    |_, _, _| panic!("a refused request does not capture"),
                )
                .unwrap(),
            Some(CaptureStep::Refused { request_id: 19, error }) if error == refusal
        ));
    }
    let mut state = Capture::fixture(true);
    assert!(state.queue(20, false));
    assert_eq!(
        state
            .perform_with(
                |_| Ok(()),
                |_| Err(Errno::EPIPE),
                |_, _, _| panic!("a failed acknowledgement does not capture"),
            )
            .err(),
        Some(Errno::EPIPE)
    );
}

#[test]
fn an_old_epoch_drops_its_pending_capture_without_restarting_it() {
    let mut state = Capture::fixture(false);
    assert!(state.queue(21, true));
    state.reset();
    let called = Cell::new(false);
    assert!(
        state
            .perform_with(
                |_| {
                    called.set(true);
                    Ok(())
                },
                |_| panic!("no stale acknowledgement"),
                |_, _, _| panic!("no stale capture"),
            )
            .unwrap()
            .is_none()
    );
    assert!(!called.get());
}

#[test]
fn fresh_capture_selects_the_rootless_control_entry_without_root_tokens() {
    assert_eq!(
        fresh_capture_control(
            b"nvx_sandbox=1 nvx_snapshot_tier=platform nvx_control_tty=ttyS1 quiet"
        )
        .unwrap(),
        Some(PathBuf::from("/dev/ttyS1"))
    );
    assert_eq!(
        fresh_capture_control(b"nvx_sandbox=1 nvx_control_tty=ttyS1").unwrap(),
        None
    );
    for extra in [
        "nvx_layer=distro,0xd0003000,erofs",
        "nvx_scratch=0xd0006000,ext4",
        "nvx_overlay_upper=ramfs",
        "nvx_hostname=626f78",
        "nvx_machine_id=image",
        "nvx_maps=0",
        "nvx_share=0",
        "nvx_identity=1000:1000",
        "nvx_workload_account=create",
        "nvx_entrypoint=/bin/work",
        "nvx_arg=work",
        "nvx_env=HOME=/work",
        "nvx_cwd=/work",
        "virtfs_tag=microvm",
        "virtfs_dir=/work",
    ] {
        let command_line =
            format!("nvx_sandbox=1 nvx_snapshot_tier=platform nvx_control_tty=ttyS1 {extra}");
        assert!(
            fresh_capture_control(command_line.as_bytes()).is_err(),
            "{extra}"
        );
    }
    for command_line in [
        "nvx_snapshot_tier=platform nvx_control_tty=ttyS1",
        "nvx_sandbox=1 nvx_snapshot_tier=paired nvx_control_tty=ttyS1",
        "nvx_sandbox=1 nvx_snapshot_tier=platform nvx_snapshot_tier=platform nvx_control_tty=ttyS1",
        "nvx_sandbox=1 nvx_snapshot_tier=platform",
        "nvx_sandbox=1 nvx_snapshot_tier=platform nvx_control_tty=../ttyS1",
        "nvx_sandbox=1 nvx_snapshot_tier=platform nvx_control_tty=ttyS1 nvx_control_tty=ttyS2",
    ] {
        assert!(
            fresh_capture_control(command_line.as_bytes()).is_err(),
            "{command_line}"
        );
    }
}

#[test]
fn fatal_capture_status_goes_directly_to_pid1_shutdown_without_starting_work() {
    for status in [0, 41, 194] {
        let mut capture = Capture::fixture(true);
        assert!(capture.queue(22, false));
        let returned = capture
            .perform_with(
                |_| Ok(()),
                |_| Ok(()),
                |_, _, _| {
                    Err(CaptureFailure {
                        stage: CaptureStage::Capture,
                        cause: CaptureCause::Io(io::Error::other("capture cannot resume")),
                        disposition: Disposition::MustEnd { status },
                    })
                },
            )
            .unwrap();
        let Some(CaptureStep::Returned {
            result: Err(failure),
            ..
        }) = returned
        else {
            panic!("the fatal disposition is retained");
        };
        let Disposition::MustEnd { status: actual } = failure.disposition else {
            panic!("a fatal capture cannot resume");
        };
        capture.must_end(actual);
        let mut system = Fake::default();
        finish(&mut system, Ok(capture.shutdown_status().unwrap()));
        assert_eq!(system.calls, [Call::ShutDown(status)]);
    }
}
