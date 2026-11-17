//! `nvx-time-probe COMMAND [OPTIONS]`; see `nvx::guest::time_probe`.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() -> std::process::ExitCode {
    nvx::guest::time_probe::main()
}

// The program measures the x86 time-stamp counter of NVX's x86_64 guests. Elsewhere this stub
// fails, so that every target that CI checks with `--all-features` still builds.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
