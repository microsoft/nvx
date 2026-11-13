//! `nvx-exit [CODE]`; see `nvx::guest::exit`.

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn main() -> std::process::ExitCode {
    nvx::guest::exit::main()
}

// Unsupported targets retain the CI stub; Linux x86-64 and ARM64 use the guest implementation.
#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
