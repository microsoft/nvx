//! Test doubles for NVX and its consumers.
//!
//! - [`fake_openvmm`]: an executable's [`main`](fake_openvmm::main) that emulates OpenVMM and the
//!   guest agent without running a VM, so that tests can drive VM instances on hosts without a
//!   hypervisor. The `nvx-fake-openvmm` binary runs it; consumers whose tests need the
//!   executable's path from `CARGO_BIN_EXE_*` wrap it in a binary of their own.

pub mod fake_openvmm;
