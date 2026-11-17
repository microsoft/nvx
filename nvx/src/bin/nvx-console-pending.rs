//! `nvx-console-pending DEVICE`; see `nvx::guest::console_pending`.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    nvx::guest::console_pending::main()
}

// The guest programs run only in Linux guests. Elsewhere this stub fails, so that every target
// that CI checks with `--all-features` still builds.
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
