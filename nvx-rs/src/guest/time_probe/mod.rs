//! `nvx-time-probe`, the guest-side diagnostics for the NVX time ABI.
//!
//! It keeps the commands, output, and exit statuses of the C program that it replaced: 0 on
//! success, 1 for a warp check that fails, 2 for an invalid command line, and 3 for a
//! measurement that could not run. Its numeric options are plain decimal, or `0x`-prefixed
//! hexadecimal for the integers, and its rate options decimal floating point, as the other guest
//! programs read their arguments. It reads the kernel's numbers, such as `/proc/cpuinfo`'s
//! rates, only in the form in which the kernel writes them.

mod clock;
mod format;
mod latency;
mod support;
mod warp;

use std::ffi::CString;
use std::io::{self, Write};
use std::process::ExitCode;

use super::status::{CommandError, ExitStatus};
use super::sys;

/// Stable diagnostic output version.
pub(super) const VERSION: &str = "1.1";
/// Maximum CPU number accepted by probe command-line masks, the size of Linux's CPU sets.
pub(super) const MAX_CPUS: usize = 1024;

/// Byte-exact command help and exit-status contract.
const USAGE: &[u8] = br#"usage: nvx-time-probe COMMAND [OPTIONS]

Commands:
  latency [--iterations N] [--batch B] [--cpu C] [--tsc-hz HZ]
      Per-operation cost (p50/p99/min/mean ns) of rdtsc, rdtscp,
      lfence;rdtsc, clock_gettime(MONOTONIC/REALTIME) through the vDSO,
      and syscall controls. Default: 2000 timed batches of 50 operations
      on the current CPU.
  warp [--duration-ms N] [--cpus LIST] [--bound-ns N] [--tsc-hz HZ]
      Pairwise cross-CPU TSC checks: a check_tsc_warp-style spinlock test
      and a ping-pong offset estimate, each running N ms per pair
      (default 100). LIST looks like 0-3,6 (default: every CPU in the
      affinity mask). The bound defaults to 1000 ns.
  clock [--measure-ms N] [--tsc-hz HZ] [--msr]
      Clocks, clocksources, TSC flags, CPUID time leaves, hypervisor and
      time kernel messages, and the /proc/timer_list clock event summary.
      --msr also reads the TSC, Hyper-V time, and kvmclock MSRs on CPU 0
      through /dev/cpu/0/msr (a #GP is reported, not raised).
  version

The TSC rate comes from --tsc-hz, sysfs tsc_freq_khz, the kernel log, or
/proc/cpuinfo, in that order, and is measured as a last resort.
Exit status: 0 success, 1 warp FAIL, 2 usage error, 3 runtime error.
"#;

/// Writes one diagnostic assembled from byte slices.
pub(super) fn report(stream: &mut dyn Write, parts: &[&[u8]]) {
    sys::report(stream, parts);
}

/// Runs `nvx-time-probe` with the process's arguments.
pub fn main() -> ExitCode {
    sys::restore_sigpipe();
    ExitStatus::from(run(
        &sys::args(),
        &mut sys::Stdout::new(),
        &mut io::stderr(),
    ))
    .into()
}

/// Dispatches one argument vector, program name first, to a probe command.
fn run(
    arguments: &[CString],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let arguments = arguments
        .iter()
        .skip(1)
        .map(CString::as_bytes)
        .collect::<Vec<_>>();
    let Some((&command, rest)) = arguments.split_first() else {
        report(err, &[USAGE]);
        return Err(CommandError::Usage);
    };
    match command {
        b"latency" => latency::run(rest, out, err),
        b"warp" => warp::run(rest, out, err),
        b"clock" => clock::run(rest, out, err),
        b"version" | b"--version" => {
            let _ = writeln!(out, "nvx-time-probe {VERSION}");
            Ok(())
        }
        b"help" | b"--help" | b"-h" => {
            let _ = out.write_all(USAGE);
            Ok(())
        }
        _ => {
            report(err, &[USAGE]);
            Err(CommandError::Usage)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&[u8]]) -> Vec<CString> {
        std::iter::once(CString::new("nvx-time-probe").unwrap())
            .chain(values.iter().map(|value| CString::new(*value).unwrap()))
            .collect()
    }

    #[test]
    fn version_help_and_usage_are_byte_exact() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(run(&args(&[b"version"]), &mut out, &mut err), Ok(()));
        assert_eq!(out, b"nvx-time-probe 1.1\n");
        assert!(err.is_empty());

        out.clear();
        assert_eq!(run(&args(&[b"--help"]), &mut out, &mut err), Ok(()));
        assert_eq!(out, USAGE);

        err.clear();
        assert_eq!(
            run(&args(&[]), &mut out, &mut err),
            Err(CommandError::Usage)
        );
        assert_eq!(err, USAGE);
    }

    #[test]
    fn the_shared_statuses_keep_the_probes_contract() {
        let cases = [
            (Ok(()), 0),
            (Err(CommandError::Operation), 1),
            (Err(CommandError::Usage), 2),
            (Err(CommandError::Setup), 3),
        ];
        for (result, status) in cases {
            assert_eq!(ExitStatus::from(result) as u8, status, "{result:?}");
        }
    }
}
