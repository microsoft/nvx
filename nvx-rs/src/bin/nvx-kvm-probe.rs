//! `nvx-kvm-probe`; see `nvx::guest::kvm_probe`.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() -> std::process::ExitCode {
    nvx::guest::kvm_probe::main()
}

// The program runs an x86 VM through the KVM of NVX's x86_64 guests. Elsewhere this stub fails,
// so that every target that CI checks with `--all-features` still builds.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
