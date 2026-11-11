//! `nvx-time`'s commands, which run the time ABI's typed calls or the program's own steps.

#[cfg(target_arch = "x86_64")]
use std::ffi::OsStr;
use std::fmt::Display;
#[cfg(target_arch = "x86_64")]
use std::fmt::Write as _;
use std::io::Write;
#[cfg(target_arch = "x86_64")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_arch = "x86_64")]
use std::path::PathBuf;

#[cfg(target_arch = "x86_64")]
use super::NSEC_PER_USEC;
#[cfg(target_arch = "x86_64")]
use super::api::{
    self, CaptureOutcome, CaptureRequest, CpuSet, PreCaptureError, RestoreFinish, ScratchPolicy,
};
use super::api::{TimeAbi, TimeError};
use super::checks::{self, Checks};
#[cfg(target_arch = "x86_64")]
use super::cpu_mask::CpuMask;
#[cfg(target_arch = "x86_64")]
use super::decoders::{self, GENERATION_ID_SIZE};
use super::events::CheckId;
use super::system::{self, Runtime};
#[cfg(target_arch = "x86_64")]
use super::system::{clock_ns, cpu_time_ns};
use super::{Phase, USAGE};
#[cfg(target_arch = "x86_64")]
use crate::guest::GenerationId;
#[cfg(target_arch = "x86_64")]
use crate::guest::parse::parse_decimal;
use crate::guest::status::CommandError;
use crate::guest::sys;
#[cfg(target_arch = "x86_64")]
use crate::guest::sys::PortError;

/// The capture requests that `capture` accepts, and writes to the snapshot port: 0 or 1.
#[cfg(target_arch = "x86_64")]
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
#[cfg(target_arch = "x86_64")]
fn port_failure(err: &mut dyn Write, error: PortError) -> CommandError {
    report(err, &[format!("nvx-time: {error}\n").as_bytes()]);
    CommandError::Operation
}

/// Reports why a call of the time ABI failed, such as `nvx-time: ioperm: Operation not
/// permitted`, and fails. A violation's event is its only report, and `run` reports a permission
/// that the thread may still hold once, when the command ends.
fn time_failure(err: &mut dyn Write, error: TimeError) -> CommandError {
    match error {
        TimeError::Violation(_) => {}
        #[cfg(target_arch = "x86_64")]
        TimeError::PortRelease(_) => {}
        #[cfg(target_arch = "aarch64")]
        TimeError::PlatformRelease(_) => {}
        #[cfg(target_arch = "x86_64")]
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
        #[cfg(target_arch = "x86_64")]
        Err(error @ TimeError::PortRelease(_)) => Err(boot_checks_unstarted(runtime, error)),
        #[cfg(target_arch = "aarch64")]
        Err(error @ TimeError::PlatformRelease(_)) => Err(boot_checks_unstarted(runtime, error)),
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
#[cfg(target_arch = "x86_64")]
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
#[cfg(target_arch = "x86_64")]
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
#[cfg(target_arch = "x86_64")]
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
#[cfg(target_arch = "x86_64")]
fn cmd_cancel_capture(abi: TimeAbi, err: &mut dyn Write) -> Result<(), CommandError> {
    abi.cancel_capture().map_err(|detail| {
        report(err, &[b"nvx-time: ", &detail, b"\n"]);
        CommandError::Operation
    })
}

/// Parses a capture request, 0 or 1, as plain decimal.
#[cfg(target_arch = "x86_64")]
fn parse_capture_request(text: &[u8]) -> Option<u8> {
    parse_decimal(text)
        .filter(|request| CAPTURE_REQUESTS.contains(request))
        .map(|request| request as u8)
}

/// Reads the request of `capture 0|1 GENERATION_ID ENTROPY_FILE [--finish]`, in the file mode,
/// in which the restore's entropy goes to `ENTROPY_FILE`, or returns `None` for an invalid
/// command line.
#[cfg(target_arch = "x86_64")]
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
/// flags, online VP count, range count, and new generation ID, the DNS server of its network
/// settings, or `-` for none, `1` if they ask the guest to announce its listeners, or else `0`,
/// and each range's start and length.
#[cfg(target_arch = "x86_64")]
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
    let network = restore.network();
    let dns_server = network
        .and_then(|network| network.dns_server())
        .map_or_else(|| "-".to_owned(), |server| server.to_string());
    let mut line = format!(
        "{} {} {} {} {} {} {}",
        if restore.finished() { 2 } else { 1 },
        restore.flags(),
        restore.online_vp_count(),
        restore.memory_ranges().len(),
        restore.generation_id(),
        dns_server,
        u8::from(network.is_some_and(|network| network.announces_listeners())),
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
#[cfg(target_arch = "x86_64")]
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
fn cmd_status(
    runtime: &Runtime<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let (state, completed) = checks::wait_for_checks(false).map_err(|error| {
        report(
            err,
            &[format!("nvx-time: read time state: {error}\n").as_bytes()],
        );
        CommandError::Operation
    })?;
    let _ = out.write_all(state.status_markers(runtime.report_only()).as_bytes());
    if completed && state.checks_ok() {
        Ok(())
    } else {
        Err(CommandError::Operation)
    }
}

/// Prints one raw host time sample, its pairing with the guest's clocks, and the frequency
/// correction that its host clock correction asks for.
fn cmd_sample(
    runtime: &Runtime<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let sample = system::diagnostic_sample(runtime).map_err(|detail| {
        report(err, &[b"nvx-time: ", &detail, b"\n"]);
        CommandError::Operation
    })?;
    let (line, pairing) = sample.format();
    let _ = out.write_all(line.as_bytes());
    pairing.map_err(|error| {
        report(
            err,
            &[format!(
                "nvx-time: sample counter {} outside latch reads {}..{}\n",
                error.counter, error.before, error.after
            )
            .as_bytes()],
        );
        CommandError::Operation
    })
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
    let command_line = sys::read_file(sys::kernel::COMMAND_LINE).map_err(|error| {
        report(
            err,
            &[format!("nvx-time: read kernel command line: {error}\n").as_bytes()],
        );
        CommandError::Operation
    })?;
    let abi = TimeAbi::with_report_only(super::sample::report_only(&command_line, report_only));
    let runtime = abi.runtime();
    sys::reset_child_signal();
    let Some((&command, rest)) = arguments.split_first() else {
        return usage(err, USAGE);
    };
    let result = match (command, rest) {
        (b"boot", []) => cmd_boot(abi, &runtime, err),
        #[cfg(target_arch = "x86_64")]
        (b"check", arguments) => cmd_check(&runtime, arguments, err),
        #[cfg(target_arch = "x86_64")]
        (b"pre-capture", []) => cmd_pre_capture(abi, err),
        #[cfg(target_arch = "x86_64")]
        (b"capture", arguments) => cmd_capture(abi, arguments, out, err),
        #[cfg(target_arch = "x86_64")]
        (b"cancel-capture", []) => cmd_cancel_capture(abi, err),
        #[cfg(target_arch = "x86_64")]
        (b"restore-finish", arguments) => cmd_restore_finish(abi, arguments, err),
        (b"status", []) => cmd_status(&runtime, out, err),
        (b"sample", []) => cmd_sample(&runtime, out, err),
        (b"generation-id", []) => cmd_generation_id(abi, out, err),
        #[cfg(target_arch = "x86_64")]
        (b"exhaustive", []) => checks::exhaustive(&runtime, out),
        _ => usage(err, USAGE),
    };
    match sys::check_released() {
        Ok(()) => result,
        #[cfg(target_arch = "x86_64")]
        Err(error) => Err(port_failure(err, error)),
        #[cfg(target_arch = "aarch64")]
        Err(error) => {
            report(
                err,
                &[format!("nvx-time: platform release: {error}\n").as_bytes()],
            );
            Err(CommandError::Operation)
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests;
