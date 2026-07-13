// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// Derived from the KVM backend of the Nanvix Micro-VM (uservm),
// Copyright(c) The Maintainers of Nanvix, MIT License.

//!
//! # Interrupt Controller
//!
//! Creates the in-kernel interrupt controller (PIC + IOAPIC) and the emulated PIT. Using
//! the in-kernel irqchip lets KVM handle `HLT`, the local APIC, and timer interrupts
//! without exiting to user space.
//!

use ::anyhow::{
    Context,
    Result,
};
use ::kvm_bindings::{
    KVM_PIT_SPEAKER_DUMMY,
    kvm_pit_config,
};
use ::kvm_ioctls::VmFd;

/// Configures KVM to handle the PIT channel-2 speaker port in-kernel.
fn pit_config() -> kvm_pit_config {
    kvm_pit_config {
        // Linux polls port 0x61 while calibrating the TSC. This VMM has no userspace speaker
        // device, so KVM must provide the channel-2 gate and output bits itself.
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    }
}

/// Creates the in-kernel PIC + IOAPIC and the PIT.
pub fn setup(vm_fd: &VmFd) -> Result<()> {
    vm_fd
        .create_irq_chip()
        .context("KVM_CREATE_IRQCHIP failed")?;
    vm_fd
        .create_pit2(pit_config())
        .context("KVM_CREATE_PIT2 failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pit_handles_speaker_port_in_kernel() {
        assert_eq!(pit_config().flags, KVM_PIT_SPEAKER_DUMMY);
    }
}
