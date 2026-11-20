use super::*;
use crate::guest::sys::Errno;
use crate::guest::sys::PortError;
use crate::guest::time::state::StateReport;
use crate::guest::time::system::tests::FakeOps;

#[test]
fn unprivileged_status_reports_legacy_fields_without_touching_binding_provider() {
    let mut ops = FakeOps::new([]);
    ops.trap_privileged = true;
    ops.tsc_hz = Err(Errno::EACCES);
    ops.lapic_hz = Err(Errno::EACCES);
    let runtime = Runtime::new(&ops, false);
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let fixture = b"version=1\nboot_status=ok\nboot_cpus=1\ntsc_hz=2194843000\n\
                    lapic_hz=200000000\nsynchronized=1\n";
    let result = status_report(&runtime, &mut output, &mut errors, || {
        Ok((StateReport::parse(fixture).unwrap(), true))
    });
    assert!(result.is_ok());
    assert!(errors.is_empty());
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "NVX-TIME-ABI: v=1 phase=boot status=ok cpus=1 tsc_hz=2194843000 \
         lapic_hz=200000000 generation=0 elapsed_us=0 cpu_us=0\n\
         NVX-TIME-ABI: v=1 phase=runtime status=synchronized generation=0 \
         discontinuities=0 offset_ns=0 uncertainty_ns=0 rejected_samples=0 \
         last_sample_error=none\n"
    );
}

#[test]
fn malformed_report_is_an_error_not_zero_status_or_hardware_admission() {
    let mut ops = FakeOps::new([]);
    ops.trap_privileged = true;
    let runtime = Runtime::new(&ops, false);
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let result = status_report(&runtime, &mut output, &mut errors, || {
        StateReport::parse(b"version=1\ntsc_hz=invalid\n")
            .map(|report| (report, true))
            .ok_or(Errno::EINVAL)
    });
    assert!(result.is_err());
    assert!(output.is_empty());
    assert!(String::from_utf8(errors).unwrap().contains("unreadable"));
}

#[test]
fn a_failed_preparation_reports_why_the_detectors_stay_unsuppressed() {
    let cases = [
        (
            PreCaptureError::Pending,
            "nvx-time: cannot suppress the stall detectors: a capture is already pending\n"
                .to_owned(),
        ),
        (
            PreCaptureError::Suppression(Errno::EROFS),
            format!(
                "nvx-time: cannot suppress the stall detectors: {}\n",
                Errno::EROFS.desc()
            ),
        ),
        (
            PreCaptureError::Unrestored(Errno::EIO),
            format!(
                "nvx-time: cannot suppress the stall detectors: {}\n",
                Errno::EIO.desc()
            ),
        ),
        // A violation's event is its only report.
        (
            PreCaptureError::BootChecks(TimeError::StateUnavailable),
            String::new(),
        ),
        (
            PreCaptureError::CaptureChecks(TimeError::StateUnavailable),
            String::new(),
        ),
        // `run` reports the permission once, when the command ends.
        (
            PreCaptureError::PortRelease {
                error: PortError::Revoke(Errno::ENOMEM).into(),
                suppressed: true,
            },
            String::new(),
        ),
    ];
    for (error, text) in cases {
        let mut err = Vec::new();
        assert_eq!(
            pre_capture_failure(&mut err, error),
            CommandError::Operation
        );
        assert_eq!(String::from_utf8(err).unwrap(), text);
    }
}

#[test]
fn capture_requests_are_plain_decimal_zero_or_one() {
    for (text, expected) in [
        (b"0".as_slice(), Some(0)),
        (b"1", Some(1)),
        (b"01", Some(1)),
        (b"2", None),
        (b"", None),
        (b"+1", None),
        (b"-0", None),
        (b" 1", None),
        (b"1x", None),
        (b"999999999999999999999999999999999999", None),
    ] {
        assert_eq!(parse_capture_request(text), expected, "{text:?}");
    }
}

#[test]
fn capture_keeps_the_file_mode_and_its_entropy_file() {
    let id = b"000102030405060708090a0b0c0d0e0f";
    let previous = GenerationId::from_bytes(std::array::from_fn(|index| index as u8));
    let path = b"/run/nvx/restore-entropy";
    let file =
        |scratch| CaptureRequest::new(scratch, previous, PathBuf::from("/run/nvx/restore-entropy"));
    assert_eq!(
        capture_request(&[b"0", id, path]),
        Some(file(ScratchPolicy::Fresh))
    );
    assert_eq!(
        capture_request(&[b"1", id, path, b"--finish"]),
        Some(file(ScratchPolicy::Paired).finish())
    );
    for arguments in [
        &[b"0".as_slice(), id, path, b"--fin"][..],
        &[b"2", id, path],
        &[b"0", &id[1..], path],
        &[b"0", id, b"restore\0entropy"],
        &[b"0", id],
    ] {
        assert_eq!(capture_request(arguments), None, "{arguments:?}");
    }
}
