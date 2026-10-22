//! The managed guest agent. It serves NVX's authenticated control session on the guest's control
//! TTY and runs one workload at a time, as the guest's PID 1: in the guest's own root, or in a
//! managed sandbox's layered root, which it assembles and tears down.
//!
//! `nvx-managed-agent CONTROL_TTY - HOSTNAME UID GID USER HOME` starts the agent in the guest's
//! own root, `nvx-managed-agent --layered-root` starts it as a managed sandbox's PID 1, and
//! `nvx-managed-agent --exec-config-fd FD -- WORKLOAD...` is its internal execution helper. See
//! `nvx::guest::managed_agent`.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    nvx::guest::managed_agent::main()
}

// The agent runs only in Linux guests. Elsewhere this stub fails, so that every target that CI
// checks with `--all-features` still builds.
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
