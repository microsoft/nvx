use super::super::*;
use crate::guest::counter::{ClockContext, CounterScale};
use crate::guest::sys::{ClockAdjustment, ClockDiscipline, Errno, FileLock, TimeAccess};
use crate::guest::time::state::StateReport;
use crate::guest::time::system::{PowerOffStatus, PrivilegedOps, SampleError};
use std::time::Instant;

struct TrapOps;

#[test]
fn unsupported_arm_capture_command_cannot_truncate_or_create_entropy_output() {
    let directory = tempfile::tempdir().unwrap();
    let existing = directory.path().join("existing");
    let absent = directory.path().join("absent");
    std::fs::write(&existing, b"preserve caller data").unwrap();
    for abi in [
        TimeAbi::report_only(),
        TimeAbi::new(),
        TimeAbi::new().strict(),
    ] {
        for path in [&existing, &absent] {
            let mut output = Vec::new();
            let mut errors = Vec::new();
            let arguments = [
                b"0".as_slice(),
                b"01010101010101010101010101010101",
                path.as_os_str().as_bytes(),
            ];
            assert!(matches!(
                cmd_capture(abi, &arguments, &mut output, &mut errors),
                Err(CommandError::Operation)
            ));
            assert!(output.is_empty());
            assert_eq!(errors, b"nvx-time: ARM capture is unsupported\n");
        }
    }
    assert_eq!(std::fs::read(&existing).unwrap(), b"preserve caller data");
    assert!(!absent.exists());
}

#[test]
fn unsupported_arm_preparation_restore_and_cancel_commands_report_the_refusal() {
    let mut errors = Vec::new();
    let abi = TimeAbi::report_only();
    assert!(matches!(
        cmd_pre_capture(abi, &mut errors),
        Err(CommandError::Operation)
    ));
    assert!(matches!(
        cmd_cancel_capture(abi, &mut errors),
        Err(CommandError::Operation)
    ));
    assert!(matches!(
        cmd_restore_finish(abi, &[], &mut errors),
        Err(CommandError::Operation)
    ));
    assert_eq!(
        errors,
        b"nvx-time: ARM capture is unsupported\n\
          nvx-time: ARM capture is unsupported\n\
          nvx-time: ARM restore is unsupported\n"
    );
}

impl PrivilegedOps for TrapOps {
    fn read_counter(&self) -> u64 {
        panic!("status read a hardware counter")
    }
    fn counter_scale(&self, _: Option<&FileLock>) -> Result<CounterScale, SampleError> {
        panic!("status admitted a hardware rate")
    }
    fn clock_context(&self, _: Option<&FileLock>) -> Result<ClockContext, SampleError> {
        panic!("status requested a hardware binding")
    }
    fn realtime_ns(&self) -> i64 {
        panic!("unexpected realtime read")
    }
    fn open_time_window(&self) -> Result<TimeAccess<'_>, Errno> {
        panic!("status opened MMIO")
    }
    fn adjust_clock(&self, _: ClockAdjustment) -> Result<ClockDiscipline, Errno> {
        panic!("status changed the clock")
    }
    fn power_off(&self, _: PowerOffStatus) {
        panic!("status stopped the VM")
    }
    fn write_kernel_log(&self, _: &[u8], _: Option<Instant>) {
        panic!("unexpected kernel log")
    }
    fn write_console(&self, _: &[u8], _: &[u8], _: Option<Instant>) {
        panic!("unexpected console")
    }
}

#[test]
fn arm_unprivileged_status_keeps_x86_wire_output_opaque_without_any_device_or_binding_call() {
    let runtime = Runtime::new(&TrapOps, false);
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
fn arm_malformed_status_refuses_without_fabricating_zero_or_calling_a_provider() {
    let runtime = Runtime::new(&TrapOps, false);
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
