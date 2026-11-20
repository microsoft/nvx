//! `nvx-exit [CODE]`; see `nvx::guest::exit`.

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn main() -> std::process::ExitCode {
    nvx::guest::exit::main()
}

// Unsupported targets fail rather than pretending to stop a guest.
#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
