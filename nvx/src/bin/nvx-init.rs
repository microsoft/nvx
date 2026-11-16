//! `nvx-init TIME_DAEMON [ANNOUNCER]`; see `nvx::guest::init`.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() -> std::process::ExitCode {
    nvx::guest::init::main()
}

// PID 1 stops NVX's x86_64 guests through an I/O port. Elsewhere this stub fails, so that every
// target that CI checks with `--all-features` still builds.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
