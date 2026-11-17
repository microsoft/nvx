//! `nvx-time [--report-only] COMMAND`, `nvx-time test NAME ARGUMENTS...`; see `nvx::guest::time`.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() -> std::process::ExitCode {
    nvx::guest::time::main()
}

// The program reads the time ABI's I/O ports and x86 time-stamp counter of NVX's x86_64 guests.
// Elsewhere this stub fails, so that every target that CI checks with `--all-features` still
// builds.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
