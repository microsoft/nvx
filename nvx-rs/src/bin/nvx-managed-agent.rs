//! The managed guest agent. It serves NVX's authenticated control session on the guest's control
//! TTY and runs one workload at a time as the guest's PID 1.
//!
//! `nvx-managed-agent CONTROL_TTY ROOTFS HOSTNAME UID GID USER HOME` starts the agent.
//! See `nvx::guest::managed_agent`.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() -> std::process::ExitCode {
    nvx::guest::managed_agent::main()
}

// The agent runs only in NVX's x86_64 Linux guests. Elsewhere this stub fails, so that every
// target that CI checks with `--all-features` still builds.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
