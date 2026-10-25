//! `nvx-time`'s commands, which run the time ABI's typed calls or the program's own steps.

use std::ffi::OsStr;
use std::fmt::{Display, Write as _};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use super::api::{
    self, CaptureOutcome, CaptureRequest, CpuSet, PreCaptureError, RestoreFinish, ScratchPolicy,
    TimeAbi, TimeError,
};
use super::checks::{self, Checks};
use super::cpu_mask::CpuMask;
use super::decoders::{self, GENERATION_ID_SIZE};
use super::events::CheckId;
use super::system::{self, PORTB_TIME_WINDOW, Runtime, clock_ns, cpu_time_ns};
use super::{NSEC_PER_USEC, Phase, USAGE};
use crate::guest::GenerationId;
use crate::guest::parse::parse_decimal;
use crate::guest::status::CommandError;
use crate::guest::sys::{self, Port, PortError};

/// The capture requests that `capture` accepts, and writes to the snapshot port: 0 or 1.
const CAPTURE_REQUESTS: std::ops::RangeInclusive<u64> = 0..=1;

/// Writes one command diagnostic assembled from raw byte slices.
fn report(err: &mut dyn Write, parts: &[&[u8]]) {
    sys::report(err, parts);
}

/// Prints one usage line and fails with the usage status.
fn usage(err: &mut dyn Write, line: &[u8]) -> Result<(), CommandError> {
    report(err, &[line]);
    Err(CommandError::Usage)
}

/// Reports why the thread could not take, or give up, its permission for the time ABI's ports,
/// such as `nvx-time: ioperm: Operation not permitted`, and fails.
fn port_failure(err: &mut dyn Write, error: PortError) -> CommandError {
    report(err, &[format!("nvx-time: {error}\n").as_bytes()]);
    CommandError::Operation
}

/// Reports why a call of the time ABI failed, such as `nvx-time: ioperm: Operation not
/// permitted`, and fails. A violation's event is its only report, and `run` reports a permission
/// that the thread may still hold once, when the command ends.
fn time_failure(err: &mut dyn Write, error: TimeError) -> CommandError {
    match error {
        TimeError::Violation(_) | TimeError::PortRelease(_) => {}
        TimeError::EntropyFile { path, source } => report(
            err,
            &[
                b"nvx-time: open ",
                path.as_os_str().as_bytes(),
                b": ",
                sys::describe(&source).as_bytes(),
                b"\n",
            ],
        ),
        error => report(err, &[format!("nvx-time: {error}\n").as_bytes()]),
    }
    CommandError::Operation
}

/// Runs the cold-boot time step, and starts the time daemon, whose deferred boot checks run in
/// the background.
fn cmd_boot(abi: TimeAbi, runtime: &Runtime<'_>, err: &mut dyn Write) -> Result<(), CommandError> {
    match abi.cold_boot().and_then(|boot| abi.start_daemon(boot)) {
        Ok(_) => Ok(()),
        Err(TimeError::Spawn(error)) => Err(boot_checks_unstarted(runtime, sys::describe(&error))),
        // The step ran, but the thread may still hold port permission, which the daemon would
        // inherit.
        Err(error @ TimeError::PortRelease(_)) => Err(boot_checks_unstarted(runtime, error)),
        Err(error) => Err(time_failure(err, error)),
    }
}

/// Reports that the boot checks could not start for `reason`, a failure of check C10, and fails.
fn boot_checks_unstarted(runtime: &Runtime<'_>, reason: impl Display) -> CommandError {
    let mut checks = Checks::empty(Phase::Boot);
    checks.fail(
        runtime,
        CheckId::C10,
        format!("cannot start the boot checks: {reason}"),
    );
    CommandError::Operation
}

/// Runs explicit capture or restore conformance checks.
fn cmd_check(
    runtime: &Runtime<'_>,
    arguments: &[&[u8]],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    const LINE: &[u8] = b"usage: nvx-time check --phase capture|restore [--new-cpus LIST]\n";
    let start = clock_ns(sys::Clock::Monotonic);
    let cpu_start = cpu_time_ns();
    let mut phase = None;
    let mut new_cpus = CpuMask::EMPTY;
    let mut arguments = arguments.iter().copied();
    while let Some(argument) = arguments.next() {
        match (argument, arguments.next()) {
            (b"--phase", Some(b"capture")) => phase = Some(Phase::Capture),
            (b"--phase", Some(b"restore")) => phase = Some(Phase::Restore),
            (b"--new-cpus", Some(list)) => match CpuMask::parse_new(list) {
                Some(cpus) => new_cpus = cpus,
                None => return usage(err, LINE),
            },
            _ => return usage(err, LINE),
        }
    }
    let Some(phase) = phase else {
        return usage(err, LINE);
    };
    api::load_generation(runtime);
    let mut checks = match Checks::load(phase) {
        Ok(checks) => checks,
        Err(_) => {
            let mut checks = Checks::empty(phase);
            checks.fail(runtime, CheckId::C6, "cannot read the CPU sets");
            return Err(CommandError::Operation);
        }
    };
    if phase == Phase::Capture {
        checks::run_capture_checks(runtime, &mut checks);
    } else {
        checks::run_restore_checks(runtime, &mut checks, new_cpus);
    }
    let _ = checks::record_check(
        runtime,
        &checks,
        clock_ns(sys::Clock::Monotonic).wrapping_sub(start) / NSEC_PER_USEC,
        cpu_time_ns().wrapping_sub(cpu_start) / NSEC_PER_USEC,
        0,
        0,
    );
    if checks.failures == 0 {
        Ok(())
    } else {
        Err(CommandError::Operation)
    }
}

/// Preparation steps 1 to 4: waits for the boot checks, which start the daemon that C10
/// requires, runs the capture checks, and suppresses the stall detectors before the snapshot.
/// A preparation that fails leaves the detectors as they were, as far as it can: the snapshot
/// script cancels only a capture whose preparation succeeded.
fn cmd_pre_capture(abi: TimeAbi, err: &mut dyn Write) -> Result<(), CommandError> {
    abi.pre_capture().map_err(|error| {
        if error.holds_suppression() {
            let _ = abi.cancel_capture();
        }
        pre_capture_failure(err, error)
    })
}

/// Reports why the preparation of a capture failed, such as `nvx-time: cannot suppress the stall
/// detectors: a capture is already pending`, and fails. A violation's event is its only report,
/// and `run` reports a permission that the thread may still hold once, when the command ends.
fn pre_capture_failure(err: &mut dyn Write, error: PreCaptureError) -> CommandError {
    let reason = match error {
        PreCaptureError::BootChecks(_)
        | PreCaptureError::CaptureChecks(_)
        | PreCaptureError::PortRelease { .. } => {
            return CommandError::Operation;
        }
        PreCaptureError::Pending => "a capture is already pending",
        PreCaptureError::Suppression(error) | PreCaptureError::Unrestored(error) => error.desc(),
    };
    report(
        err,
        &[
            b"nvx-time: cannot suppress the stall detectors: ",
            reason.as_bytes(),
            b"\n",
        ],
    );
    CommandError::Operation
}

/// Cancels capture by restoring saved stall-detector settings.
fn cmd_cancel_capture(abi: TimeAbi, err: &mut dyn Write) -> Result<(), CommandError> {
    abi.cancel_capture().map_err(|detail| {
        report(err, &[b"nvx-time: ", &detail, b"\n"]);
        CommandError::Operation
    })
}

/// Parses a capture request, 0 or 1, as plain decimal.
fn parse_capture_request(text: &[u8]) -> Option<u8> {
    parse_decimal(text)
        .filter(|request| CAPTURE_REQUESTS.contains(request))
        .map(|request| request as u8)
}

/// Reads the request of `capture 0|1 GENERATION_ID ENTROPY_FILE [--finish]`, in the file mode,
/// in which the restore's entropy goes to `ENTROPY_FILE`, or returns `None` for an invalid
/// command line.
fn capture_request(arguments: &[&[u8]]) -> Option<CaptureRequest> {
    let (request, previous_id, entropy_path, finish) = match arguments {
        [request, id, path] => (request, id, path, false),
        [request, id, path, b"--finish"] => (request, id, path, true),
        _ => return None,
    };
    let scratch = match parse_capture_request(request)? {
        0 => ScratchPolicy::Fresh,
        _ => ScratchPolicy::Paired,
    };
    let previous_id = decoders::parse_hex::<GENERATION_ID_SIZE>(previous_id)?;
    if entropy_path.contains(&0) {
        return None;
    }
    let request = CaptureRequest::new(
        scratch,
        GenerationId::from_bytes(previous_id),
        PathBuf::from(OsStr::from_bytes(entropy_path)),
    );
    Some(if finish { request.finish() } else { request })
}

/// Requests a capture, and repairs a restore, which it prints: `0` when the VM continued, or
/// `1` or `2`, for a restore that it left unfinished or finished, then the restore packet's
/// flags, online VP count, range count, and new generation ID, and each range's start and
/// length.
fn cmd_capture(
    abi: TimeAbi,
    arguments: &[&[u8]],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    const LINE: &[u8] = b"usage: nvx-time capture 0|1 GENERATION_ID ENTROPY_FILE [--finish]\n";
    let Some(request) = capture_request(arguments) else {
        return usage(err, LINE);
    };
    let restore = match abi.capture(request) {
        Ok(CaptureOutcome::Continued) => {
            let _ = out.write_all(b"0\n");
            return Ok(());
        }
        Ok(CaptureOutcome::Restored(restore)) => restore,
        Err(error) => return Err(time_failure(err, error)),
    };
    let mut line = format!(
        "{} {} {} {} {}",
        if restore.finished() { 2 } else { 1 },
        restore.flags(),
        restore.online_vp_count(),
        restore.memory_ranges().len(),
        restore.generation_id()
    );
    for range in restore.memory_ranges() {
        let _ = write!(line, " {} {}", range.start(), range.length());
    }
    line.push('\n');
    let _ = out.write_all(line.as_bytes());
    Ok(())
}

/// Records new CPUs, acknowledges the restore when asked, and hands repair step 7 to the daemon
/// or runs it.
fn cmd_restore_finish(
    abi: TimeAbi,
    arguments: &[&[u8]],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    const LINE: &[u8] = b"usage: nvx-time restore-finish [--ack] [--new-cpus LIST]\n";
    let mut request = RestoreFinish::new();
    let mut arguments = arguments.iter().copied();
    while let Some(argument) = arguments.next() {
        match argument {
            b"--ack" => request = request.acknowledge(),
            b"--new-cpus" => match arguments.next().and_then(CpuMask::parse_new) {
                Some(cpus) => request = request.new_cpus(CpuSet::from(cpus)),
                None => return usage(err, LINE),
            },
            _ => return usage(err, LINE),
        }
    }
    abi.restore_finish(request)
        .map_err(|error| time_failure(err, error))
}

/// Prints persisted status after waiting for pending checks.
fn cmd_status(runtime: &Runtime<'_>, out: &mut dyn Write) -> Result<(), CommandError> {
    let (state, completed) = checks::wait_for_checks(false);
    let _ = out.write_all(state.status_markers(runtime.report_only()).as_bytes());
    if completed && state.checks_ok() {
        Ok(())
    } else {
        Err(CommandError::Operation)
    }
}

/// Prints one raw host time sample and its derived pairing.
fn cmd_sample(
    runtime: &Runtime<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let sample = {
        let ports = runtime
            .open_ports()
            .map_err(|error| port_failure(err, error))?;
        let status = ports.read_u8(Port::PortbStatus);
        if status & PORTB_TIME_WINDOW == 0 {
            report(
                err,
                &[
                    format!("nvx-time: portb status 0x{status:02x} has no time window\n")
                        .as_bytes(),
                ],
            );
            return Err(CommandError::Operation);
        }
        let _lock = system::lock_portb().map_err(|error| {
            report(
                err,
                &[b"nvx-time: ", &system::portb_lock_failure(error), b"\n"],
            );
            CommandError::Operation
        })?;
        system::take_sample(&ports)
    };
    let sample = sample.map_err(|error| {
        report(err, &[format!("nvx-time: {error}\n").as_bytes()]);
        CommandError::Operation
    })?;
    let (theta, epsilon) = decoders::pair_clocks(sample.t0, sample.t1, sample.utc_ns);
    let line = format!(
        "flags=0x{:02x} generation={} utc_ns={} t0={} t1={} theta_ns={} epsilon_ns={}\n",
        sample.flags, sample.generation, sample.utc_ns, sample.t0, sample.t1, theta, epsilon
    );
    let _ = out.write_all(line.as_bytes());
    Ok(())
}

/// Reads and prints the VM generation ID.
fn cmd_generation_id(
    abi: TimeAbi,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let id = abi
        .generation_id()
        .map_err(|error| time_failure(err, error))?;
    let _ = out.write_all(format!("{id}\n").as_bytes());
    Ok(())
}

/// Dispatches a production `nvx-time` command.
///
/// A command takes the port permission only for each of its port transactions. When the kernel
/// refused to revoke it, the thread starts nothing more, and the command fails once its work
/// ended, because the thread may still hold some of the permission.
pub(super) fn run(
    arguments: &[&[u8]],
    report_only: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let abi = if report_only {
        TimeAbi::report_only()
    } else {
        TimeAbi::new()
    };
    let runtime = abi.runtime();
    sys::reset_child_signal();
    let Some((&command, rest)) = arguments.split_first() else {
        return usage(err, USAGE);
    };
    let result = match (command, rest) {
        (b"boot", []) => cmd_boot(abi, &runtime, err),
        (b"check", arguments) => cmd_check(&runtime, arguments, err),
        (b"pre-capture", []) => cmd_pre_capture(abi, err),
        (b"capture", arguments) => cmd_capture(abi, arguments, out, err),
        (b"cancel-capture", []) => cmd_cancel_capture(abi, err),
        (b"restore-finish", arguments) => cmd_restore_finish(abi, arguments, err),
        (b"status", []) => cmd_status(&runtime, out),
        (b"sample", []) => cmd_sample(&runtime, out, err),
        (b"generation-id", []) => cmd_generation_id(abi, out, err),
        (b"exhaustive", []) => checks::exhaustive(&runtime, out),
        _ => usage(err, USAGE),
    };
    match sys::check_released() {
        Ok(()) => result,
        Err(error) => Err(port_failure(err, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest::sys::Errno;

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
        let file = |scratch| {
            CaptureRequest::new(scratch, previous, PathBuf::from("/run/nvx/restore-entropy"))
        };
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
}
