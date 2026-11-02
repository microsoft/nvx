//! `--nested-virt`, which `run`, and `sandbox` for `run` and `provision`, take: the checks that
//! refuse it before anything launches where a microVM cannot have nested virtualization.
//!
//! Nested virtualization gives a microVM's guest VMX, with which the guest runs VMs of its own
//! through `/dev/kvm`; see the architecture guide's
//! [nested virtualization](../../../../doc/design/2.2-cpu-and-topology.md). OpenVMM refuses it with a
//! snapshot, and on MSHV, and checks before the guest runs that the CPU profile is Intel's and that
//! the backend offers it. These checks name those refusals for NVX's own options first:
//!
//! - [`check_request`], whatever the host: a restore refuses it, since it takes its machine from a
//!   snapshot, which never has it, and so does MSHV, which offers it only to a partition with the
//!   Hyper-V guest interface that the NVX time ABI excludes;
//! - [`check_host`], which a dry run skips: a CPU other than Intel's refuses it, since a microVM
//!   exposes VMX and not AMD's SVM, and on Linux, KVM must offer nested VMX, as the
//!   `nested` parameter of `kvm_intel`, [`KVM_INTEL_NESTED`], says. On Windows, OpenVMM itself
//!   finds whether WHP offers it, before the guest runs.

use std::io;

use super::cpu_profile::HostCpu;
use super::{Exit, Result};
use crate::ErrorCode;

/// The `nested` parameter of the `kvm_intel` module, `Y` or `1` where KVM offers its guests VMX.
pub(super) const KVM_INTEL_NESTED: &str = "/sys/module/kvm_intel/parameters/nested";

/// The CPUID vendor string of Intel's CPUs, whose VMX nested virtualization exposes.
const INTEL: &str = "GenuineIntel";

/// Refuses `--nested-virt` where no host can honor it: for a restore, and for `hypervisor`, the
/// resolved backend, MSHV.
pub(super) fn check_request(hypervisor: &str, restore: bool) -> Result {
    if restore {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            "--nested-virt is not accepted for restore; a snapshot never has nested \
             virtualization",
        ));
    }
    if hypervisor == "mshv" {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            "--nested-virt requires the KVM or WHP hypervisor; MSHV offers nested virtualization \
             only with the Hyper-V guest interface, which the NVX time ABI excludes",
        ));
    }
    Ok(())
}

/// Refuses `--nested-virt` on a host that cannot give the microVM of `hypervisor`, the resolved
/// backend, nested virtualization: one with a CPU other than Intel's, or, for KVM, one whose
/// `kvm_intel` does not offer nested VMX.
pub(super) fn check_host(hypervisor: &str) -> Result {
    let cpu = HostCpu::current();
    check_host_with(hypervisor, cpu.as_ref().map(HostCpu::vendor), || {
        std::fs::read_to_string(KVM_INTEL_NESTED)
    })
}

/// [`check_host`] for a CPU of `vendor`, if known, on a host whose `kvm_intel` reports its
/// `nested` parameter through `nested`.
fn check_host_with(
    hypervisor: &str,
    vendor: Option<&str>,
    nested: impl FnOnce() -> io::Result<String>,
) -> Result {
    if let Some(vendor) = vendor
        && vendor != INTEL
    {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            format!(
                "--nested-virt requires an Intel CPU, not {vendor}; a microVM exposes VMX, and no \
                 CPU profile describes AMD's SVM"
            ),
        ));
    }
    if hypervisor != "kvm" {
        return Ok(());
    }
    let unavailable = |reason: String| {
        Err(Exit::error(
            ErrorCode::Unavailable,
            format!(
                "--nested-virt requires KVM's nested virtualization, which this host does not \
                 offer: {reason}; load kvm_intel with nested=1"
            ),
        ))
    };
    match nested() {
        Ok(value) if matches!(value.trim_end(), "Y" | "1") => Ok(()),
        Ok(value) => unavailable(format!("{KVM_INTEL_NESTED} is {}", value.trim_end())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => unavailable(format!(
            "kvm_intel is not loaded, as {KVM_INTEL_NESTED} does not exist"
        )),
        Err(error) => unavailable(format!("cannot read {KVM_INTEL_NESTED}: {error}")),
    }
}

#[cfg(test)]
mod tests;
