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
use ::kvm_bindings::kvm_pit_config;
use ::kvm_ioctls::VmFd;

/// ISA interrupt line used by the primary 16550 UART (`ttyS0`).
pub const SERIAL_IRQ: u32 = 4;

/// Creates the in-kernel PIC + IOAPIC and the PIT.
pub fn setup(vm_fd: &VmFd) -> Result<()> {
    vm_fd
        .create_irq_chip()
        .context("KVM_CREATE_IRQCHIP failed")?;
    let pit_config: kvm_pit_config = kvm_pit_config::default();
    vm_fd
        .create_pit2(pit_config)
        .context("KVM_CREATE_PIT2 failed")?;
    Ok(())
}
