//! `nvx-exit [CODE]`; see `nvx::guest::exit`.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() -> std::process::ExitCode {
    nvx::guest::exit::main()
}

// The program writes to an I/O port of NVX's x86_64 guests. Elsewhere this stub fails, so that
// every target that CI checks with `--all-features` still builds.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
