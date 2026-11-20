//! `nvx-time [--report-only] COMMAND`, `nvx-time test NAME ARGUMENTS...`; see `nvx::guest::time`.

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn main() -> std::process::ExitCode {
    nvx::guest::time::main()
}

// The supported Linux targets use their own actual time-ABI access and counter.
// Elsewhere this stub fails, so that every target that CI checks with `--all-features` still
// builds.
#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
