//! `nvx-kvm-probe`: checks that the guest can run its own VMs, through the KVM of a guest whose
//! microVM has nested virtualization.
//!
//! It opens `/dev/kvm`, requires the stable KVM API, version 12, and creates a VM with one page
//! of memory at `0xfffff000`, which holds `HLT` at the x86 reset vector, `0xfffffff0`, and one
//! vCPU, which starts there in its reset state. It runs the vCPU once, which must exit with
//! `KVM_EXIT_HLT`: with no in-kernel interrupt controller, KVM returns a halt to the process. It
//! then prints `nvx-kvm-probe: api=12 vm=created vcpu=created exit=hlt` and exits with 0.
//!
//! Any other result is a failure, which it reports on standard error, naming the step, such as
//! `KVM_CREATE_VM: Operation not supported` or `KVM_RUN exited with reason 8, not
//! KVM_EXIT_HLT (5)`, and exits with 1. A command line with an argument is a usage error, which
//! exits with 2.

use std::ffi::{CStr, CString};
use std::io::{self, Write};
use std::process::ExitCode;

use super::status::{CommandError, run_main};
use super::sys::{self, Errno, KVM_API_VERSION, KVM_EXIT_HLT, Kvm};

/// The guest physical address of the probe VM's page, the last page below 4 GiB.
const PAGE_ADDRESS: u64 = 0xffff_f000;
/// The offset of the x86 reset vector, `0xfffffff0`, in that page.
const RESET_VECTOR_OFFSET: usize = 0xff0;
/// `HLT`.
const HLT: u8 = 0xf4;
/// The length of the probe VM's page.
const PAGE_LENGTH: usize = 0x1000;

/// Runs `nvx-kvm-probe`; see the module documentation.
pub fn main() -> ExitCode {
    run_main(|args| {
        run(
            args,
            c"/dev/kvm",
            &mut sys::Stdout::new(),
            &mut io::stderr(),
        )
    })
}

/// Runs the probe with `args` through the KVM device at `device`, reporting on `out` and `err`.
fn run(
    args: &[CString],
    device: &CStr,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    if args.len() > 1 {
        sys::report(err, &[b"usage: nvx-kvm-probe\n"]);
        return Err(CommandError::Usage);
    }
    let kvm = Kvm::open(device).map_err(|errno| {
        let mut step = b"open ".to_vec();
        step.extend_from_slice(device.to_bytes());
        failed(err, &step, errno)
    })?;
    let version = kvm
        .api_version()
        .map_err(|errno| failed(err, b"KVM_GET_API_VERSION", errno))?;
    if version != KVM_API_VERSION {
        sys::report(
            err,
            &[format!("KVM API version {version}, not {KVM_API_VERSION}\n").as_bytes()],
        );
        return Err(CommandError::Operation);
    }
    let mut vm = kvm
        .create_vm()
        .map_err(|errno| failed(err, b"KVM_CREATE_VM", errno))?;
    let mut page = [0; PAGE_LENGTH];
    page[RESET_VECTOR_OFFSET] = HLT;
    vm.add_memory(0, PAGE_ADDRESS, &page)
        .map_err(|errno| failed(err, b"KVM_SET_USER_MEMORY_REGION", errno))?;
    let mut vcpu = vm
        .create_vcpu(0)
        .map_err(|errno| failed(err, b"KVM_CREATE_VCPU", errno))?;
    let reason = vcpu.run().map_err(|errno| failed(err, b"KVM_RUN", errno))?;
    if reason != KVM_EXIT_HLT {
        sys::report(
            err,
            &[
                format!("KVM_RUN exited with reason {reason}, not KVM_EXIT_HLT ({KVM_EXIT_HLT})\n")
                    .as_bytes(),
            ],
        );
        return Err(CommandError::Operation);
    }
    sys::print(
        out,
        format!("nvx-kvm-probe: api={version} vm=created vcpu=created exit=hlt\n").as_bytes(),
    )
    .map_err(|errno| failed(err, b"write", errno))
}

/// Reports that `step` failed with `errno`, as C's `perror(step)` does, with `nix`'s description,
/// and returns the operation failure.
fn failed(err: &mut dyn Write, step: &[u8], errno: Errno) -> CommandError {
    sys::report(err, &[step, b": ", errno.desc().as_bytes(), b"\n"]);
    CommandError::Operation
}

#[cfg(test)]
mod tests;
