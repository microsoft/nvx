//! The KVM API that `nvx-kvm-probe` drives, from `linux/kvm.h`: the ioctls of `/dev/kvm`, of a
//! VM, and of a vCPU, behind owners of the VM's and the vCPU's descriptors and of the memory that
//! they map.

use std::ffi::{CStr, c_int, c_void};
use std::num::NonZeroUsize;
use std::ptr::NonNull;

use nix::sys::mman::{MapFlags, ProtFlags};

use super::{Errno, FileDescriptor, Mode, OpenFlags, open};

/// `KVM_API_VERSION`, the version of the stable KVM API, the only one that Linux has had.
pub(crate) const KVM_API_VERSION: c_int = 12;
/// `KVM_EXIT_HLT`: a vCPU without an in-kernel interrupt controller executed `HLT`.
pub(crate) const KVM_EXIT_HLT: u32 = 5;

/// `KVMIO`, the type of every KVM ioctl.
const KVMIO: u8 = 0xae;
/// The offset of `exit_reason` in `struct kvm_run`: after `request_interrupt_window`,
/// `immediate_exit`, and six bytes of padding.
const KVM_RUN_EXIT_REASON: usize = 8;

/// `struct kvm_userspace_memory_region`, which `KVM_SET_USER_MEMORY_REGION` reads.
#[repr(C)]
struct UserspaceMemoryRegion {
    /// The slot that the region takes.
    slot: u32,
    /// The `KVM_MEM_*` flags.
    flags: u32,
    /// The guest physical address of the region.
    guest_phys_addr: u64,
    /// The region's length, in bytes.
    memory_size: u64,
    /// The address of the region's memory in this process.
    userspace_addr: u64,
}

// KVM refuses `KVM_GET_API_VERSION`, `KVM_GET_VCPU_MMAP_SIZE`, and `KVM_RUN` with `EINVAL`
// unless their argument is 0, so each passes 0 explicitly: the variadic `ioctl` would otherwise
// pass whatever its third argument's register holds.

nix::ioctl_write_int_bad!(
    /// Returns the KVM API version: `KVM_GET_API_VERSION`, `_IO(KVMIO, 0x00)`, whose argument
    /// must be 0.
    kvm_get_api_version,
    nix::request_code_none!(KVMIO, 0x00)
);

nix::ioctl_write_int_bad!(
    /// Creates a VM of a machine type and returns its descriptor: `KVM_CREATE_VM`,
    /// `_IO(KVMIO, 0x01)`, which takes the type as its integer argument.
    kvm_create_vm,
    nix::request_code_none!(KVMIO, 0x01)
);

nix::ioctl_write_int_bad!(
    /// Returns the size of a vCPU's `struct kvm_run` mapping: `KVM_GET_VCPU_MMAP_SIZE`,
    /// `_IO(KVMIO, 0x04)`, whose argument must be 0.
    kvm_get_vcpu_mmap_size,
    nix::request_code_none!(KVMIO, 0x04)
);

nix::ioctl_write_int_bad!(
    /// Creates a vCPU of an ID and returns its descriptor: `KVM_CREATE_VCPU`, `_IO(KVMIO, 0x41)`,
    /// which takes the ID as its integer argument.
    kvm_create_vcpu,
    nix::request_code_none!(KVMIO, 0x41)
);

nix::ioctl_write_ptr!(
    /// Gives a VM a region of this process's memory: `KVM_SET_USER_MEMORY_REGION`,
    /// `_IOW(KVMIO, 0x46, struct kvm_userspace_memory_region)`.
    kvm_set_user_memory_region,
    KVMIO,
    0x46,
    UserspaceMemoryRegion
);

nix::ioctl_write_int_bad!(
    /// Runs a vCPU until it exits: `KVM_RUN`, `_IO(KVMIO, 0x80)`, whose argument must be 0.
    kvm_run,
    nix::request_code_none!(KVMIO, 0x80)
);

/// One anonymous, private, read-write mapping, which this process owns and unmaps once.
struct AnonymousMapping {
    /// The mapping's base.
    address: NonNull<c_void>,
    /// The number of mapped bytes.
    length: NonZeroUsize,
}

impl AnonymousMapping {
    /// Maps `length` zeroed bytes.
    fn new(length: NonZeroUsize) -> Result<Self, Errno> {
        // SAFETY: no fixed address is requested, `length` is nonzero, and the mapping is owned
        // and unmapped exactly once, by `Drop`.
        let address = unsafe {
            nix::sys::mman::mmap_anonymous(
                None,
                length,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
            )
        }?;
        Ok(Self { address, length })
    }
}

impl Drop for AnonymousMapping {
    fn drop(&mut self) {
        // SAFETY: this type owns the exact mapping, which it unmaps only here. A VM that still
        // maps it keeps nothing of it: KVM drops its translations when the memory is unmapped.
        let _ = unsafe { nix::sys::mman::munmap(self.address, self.length.get()) };
    }
}

/// An open `/dev/kvm`.
pub(crate) struct Kvm(FileDescriptor);

impl Kvm {
    /// Opens the KVM device at `path`, `/dev/kvm` outside the tests, for reading and writing.
    pub(crate) fn open(path: &CStr) -> Result<Self, Errno> {
        open(
            path,
            OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )
        .map(Self)
    }

    /// Returns the KVM API version, which is [`KVM_API_VERSION`] wherever KVM is usable.
    pub(crate) fn api_version(&self) -> Result<c_int, Errno> {
        // SAFETY: `KVM_GET_API_VERSION` reads only its integer argument, 0, and writes no memory.
        unsafe { kvm_get_api_version(self.0.raw(), 0) }
    }

    /// Creates a VM of the default machine type, 0, without memory or vCPUs.
    pub(crate) fn create_vm(&self) -> Result<KvmVm, Errno> {
        // SAFETY: `KVM_CREATE_VM` reads only its integer argument, and the descriptor that it
        // returns is new and owned by the result alone.
        let descriptor = unsafe { kvm_create_vm(self.0.raw(), 0) }?;
        // SAFETY: the kernel just opened the descriptor, which nothing else owns.
        let vm = unsafe { FileDescriptor::from_raw(descriptor) };
        let vcpu_mmap_size = self.vcpu_mmap_size()?;
        Ok(KvmVm {
            descriptor: vm,
            vcpu_mmap_size,
            memory: Vec::new(),
        })
    }

    /// Returns the size of a vCPU's `struct kvm_run` mapping.
    fn vcpu_mmap_size(&self) -> Result<NonZeroUsize, Errno> {
        // SAFETY: `KVM_GET_VCPU_MMAP_SIZE` reads only its integer argument, 0, and writes no
        // memory.
        let size = unsafe { kvm_get_vcpu_mmap_size(self.0.raw(), 0) }?;
        usize::try_from(size)
            .ok()
            .and_then(NonZeroUsize::new)
            .filter(|size| size.get() > KVM_RUN_EXIT_REASON + size_of::<u32>())
            .ok_or(Errno::EINVAL)
    }
}

/// A VM, with the memory that it maps.
pub(crate) struct KvmVm {
    /// The VM's descriptor.
    descriptor: FileDescriptor,
    /// The size of a vCPU's `struct kvm_run` mapping.
    vcpu_mmap_size: NonZeroUsize,
    /// The memory of each region that the VM maps, which lives as long as the VM.
    memory: Vec<AnonymousMapping>,
}

impl KvmVm {
    /// Gives the VM, in `slot`, a region of `contents.len()` bytes, a multiple of the page size,
    /// at the guest physical address `address`, holding `contents`.
    pub(crate) fn add_memory(
        &mut self,
        slot: u32,
        address: u64,
        contents: &[u8],
    ) -> Result<(), Errno> {
        let length = NonZeroUsize::new(contents.len()).ok_or(Errno::EINVAL)?;
        let mapping = AnonymousMapping::new(length)?;
        // SAFETY: the mapping is writable for `length` bytes, the length of `contents`, which
        // cannot overlap it, since nothing else refers to the new mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(
                contents.as_ptr(),
                mapping.address.as_ptr().cast::<u8>(),
                contents.len(),
            );
        }
        let region = UserspaceMemoryRegion {
            slot,
            flags: 0,
            guest_phys_addr: address,
            memory_size: length.get() as u64,
            userspace_addr: mapping.address.as_ptr() as u64,
        };
        // SAFETY: `region` has the kernel's layout and outlives the synchronous call, and the
        // memory that it names stays mapped as long as the VM, which owns its mapping.
        unsafe { kvm_set_user_memory_region(self.descriptor.raw(), &raw const region) }?;
        self.memory.push(mapping);
        Ok(())
    }

    /// Creates the vCPU of `id`, in the reset state of an x86 processor, with its `kvm_run`
    /// mapping.
    pub(crate) fn create_vcpu(&self, id: c_int) -> Result<KvmVcpu, Errno> {
        // SAFETY: `KVM_CREATE_VCPU` reads only its integer argument, and the descriptor that it
        // returns is new and owned by the result alone.
        let descriptor = unsafe { kvm_create_vcpu(self.descriptor.raw(), id) }?;
        // SAFETY: the kernel just opened the descriptor, which nothing else owns.
        let descriptor = unsafe { FileDescriptor::from_raw(descriptor) };
        // SAFETY: the descriptor is open, the length is the size that KVM gave, no fixed address
        // is requested, and the mapping is owned and unmapped exactly once, by `KvmVcpu`.
        let run = unsafe {
            nix::sys::mman::mmap(
                None,
                self.vcpu_mmap_size,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                descriptor.borrowed(),
                0,
            )
        }?;
        Ok(KvmVcpu {
            descriptor,
            run,
            run_length: self.vcpu_mmap_size,
        })
    }
}

/// A vCPU, with its `struct kvm_run` mapping.
pub(crate) struct KvmVcpu {
    /// The vCPU's descriptor.
    descriptor: FileDescriptor,
    /// The vCPU's `struct kvm_run`.
    run: NonNull<c_void>,
    /// The length of the `struct kvm_run` mapping.
    run_length: NonZeroUsize,
}

impl KvmVcpu {
    /// Runs the vCPU until it exits to this process, and returns the exit's `KVM_EXIT_*` reason.
    pub(crate) fn run(&mut self) -> Result<u32, Errno> {
        // SAFETY: `KVM_RUN` reads only its integer argument, 0; it writes the vCPU's
        // `struct kvm_run`, which this vCPU maps.
        unsafe { kvm_run(self.descriptor.raw(), 0) }?;
        // SAFETY: the mapping, longer than the offset and the field by construction, holds the
        // `struct kvm_run` that the kernel wrote before `KVM_RUN` returned, whose `exit_reason`
        // is a naturally aligned `u32` at this offset; the volatile read takes the kernel's
        // value.
        Ok(unsafe {
            self.run
                .as_ptr()
                .cast::<u8>()
                .add(KVM_RUN_EXIT_REASON)
                .cast::<u32>()
                .read_volatile()
        })
    }
}

impl Drop for KvmVcpu {
    fn drop(&mut self) {
        // SAFETY: this type owns the exact `struct kvm_run` mapping, which it unmaps only here.
        let _ = unsafe { nix::sys::mman::munmap(self.run, self.run_length.get()) };
    }
}

#[cfg(test)]
mod tests;
