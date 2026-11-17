//! `nvx-device-io`; see `nvx::guest::device_io`.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() -> std::process::ExitCode {
    nvx::guest::device_io::main()
}

// The benchmark helper runs in NVX's x86_64 guests. Elsewhere this stub fails, so that every
// target that CI checks with `--all-features` still builds.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
