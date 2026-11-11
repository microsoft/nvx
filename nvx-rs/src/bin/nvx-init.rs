//! `nvx-init TIME_DAEMON [ANNOUNCER]`; see `nvx::guest::init`.

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn main() -> std::process::ExitCode {
    nvx::guest::init::main()
}

// Unsupported targets retain the CI stub; Linux x86-64 and ARM64 use the Rust PID 1.
#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
