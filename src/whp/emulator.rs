// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # WHP instruction emulator (MMIO)
//!
//! WHP has no in-hypervisor device model, so a guest access to an unmapped guest-physical address
//! (the virtio-mmio window) faults out as a `WHvRunVpExitReasonMemoryAccess`. Unlike a port-I/O
//! exit, the faulting instruction is not pre-decoded, so the VMM drives WHP's built-in instruction
//! emulator ([`WHvEmulatorTryMmioEmulation`]) to decode it and call back for the actual memory
//! access, register reads/writes, and address translation. This wrapper owns the emulator and
//! turns each MMIO access into a call on a caller-supplied [`MmioHandler`].
//!

use ::core::ffi::c_void;
use ::core::ptr::null_mut;

use ::anyhow::{
    Context,
    Result,
    bail,
};
use ::windows::core::HRESULT;
use ::windows::Win32::System::Hypervisor::{
    WHV_EMULATOR_CALLBACKS,
    WHV_EMULATOR_IO_ACCESS_INFO,
    WHV_EMULATOR_MEMORY_ACCESS_INFO,
    WHV_EMULATOR_STATUS,
    WHV_MEMORY_ACCESS_CONTEXT,
    WHV_PARTITION_HANDLE,
    WHV_REGISTER_NAME,
    WHV_REGISTER_VALUE,
    WHV_TRANSLATE_GVA_FLAGS,
    WHV_TRANSLATE_GVA_RESULT,
    WHV_TRANSLATE_GVA_RESULT_CODE,
    WHV_VP_EXIT_CONTEXT,
    WHvEmulatorCreateEmulator,
    WHvEmulatorDestroyEmulator,
    WHvEmulatorTryMmioEmulation,
    WHvGetVirtualProcessorRegisters,
    WHvSetVirtualProcessorRegisters,
    WHvTranslateGva,
};

/// `S_OK`.
const S_OK: HRESULT = HRESULT(0);
/// Index of the single guest virtual processor.
const VP_INDEX: u32 = 0;

/// Services one guest MMIO access decoded by the emulator.
pub trait MmioHandler {
    /// Handles an access at guest-physical `gpa`. For a read (`is_write == false`) the handler
    /// fills `data`; for a write it consumes `data`.
    fn mmio(&mut self, gpa: u64, is_write: bool, data: &mut [u8]);
}

/// Context handed to the C callbacks: the partition (for register/translate helpers) plus a
/// type-erased pointer to the caller's [`MmioHandler`] and a monomorphized shim to invoke it.
#[repr(C)]
struct EmuCtx {
    partition: WHV_PARTITION_HANDLE,
    user: *mut c_void,
    mmio: unsafe fn(*mut c_void, u64, bool, &mut [u8]),
}

/// A reusable WHP instruction emulator.
pub struct Emulator {
    handle: *mut c_void,
}

// SAFETY: the emulator handle is only used on the vCPU thread (single-threaded per the run loop).
unsafe impl Send for Emulator {}

impl Emulator {
    /// Creates the emulator with the fixed callback table.
    pub fn new() -> Result<Self> {
        let callbacks: WHV_EMULATOR_CALLBACKS = WHV_EMULATOR_CALLBACKS {
            Size: size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
            Reserved: 0,
            WHvEmulatorIoPortCallback: Some(io_port_cb),
            WHvEmulatorMemoryCallback: Some(memory_cb),
            WHvEmulatorGetVirtualProcessorRegisters: Some(get_regs_cb),
            WHvEmulatorSetVirtualProcessorRegisters: Some(set_regs_cb),
            WHvEmulatorTranslateGvaPage: Some(translate_cb),
        };
        let mut handle: *mut c_void = null_mut();
        // SAFETY: `callbacks` outlives the call; `handle` receives the created emulator.
        unsafe {
            WHvEmulatorCreateEmulator(&callbacks, &mut handle)
                .context("WHvEmulatorCreateEmulator failed")?;
        }
        Ok(Self { handle })
    }

    /// Emulates the faulting instruction of a memory-access exit, dispatching each MMIO access to
    /// `handler`.
    pub fn emulate<H: MmioHandler>(
        &self,
        partition: WHV_PARTITION_HANDLE,
        vp_context: &WHV_VP_EXIT_CONTEXT,
        mmio_context: &WHV_MEMORY_ACCESS_CONTEXT,
        handler: &mut H,
    ) -> Result<()> {
        unsafe fn shim<H: MmioHandler>(user: *mut c_void, gpa: u64, is_write: bool, data: &mut [u8]) {
            // SAFETY: `user` is the `&mut H` erased in `emulate`, valid for this synchronous call.
            let h: &mut H = unsafe { &mut *(user as *mut H) };
            h.mmio(gpa, is_write, data);
        }
        let mut ctx: EmuCtx = EmuCtx {
            partition,
            user: (handler as *mut H).cast::<c_void>(),
            mmio: shim::<H>,
        };
        // SAFETY: `ctx` outlives the synchronous emulation call; the exit contexts are valid.
        let status: WHV_EMULATOR_STATUS = unsafe {
            WHvEmulatorTryMmioEmulation(
                self.handle,
                (&mut ctx as *mut EmuCtx).cast::<c_void>(),
                vp_context,
                mmio_context,
            )
        }
        .context("WHvEmulatorTryMmioEmulation failed")?;

        // Bit 0 of the status word is EmulationSuccessful.
        // SAFETY: `AsUINT32` is the integer view of the status union, always valid to read.
        let status_word: u32 = unsafe { status.AsUINT32 };
        if status_word & 0x1 == 0 {
            bail!("MMIO instruction emulation did not complete (status={status_word:#x})");
        }
        Ok(())
    }
}

impl Drop for Emulator {
    fn drop(&mut self) {
        // SAFETY: `handle` was produced by `WHvEmulatorCreateEmulator` and is dropped once.
        unsafe {
            let _ = WHvEmulatorDestroyEmulator(self.handle);
        }
    }
}

/// Emulator callback: an I/O-port access encountered mid-instruction. The virtio-mmio driver does
/// not mix port I/O into an MMIO instruction, so reads float to zero and writes are dropped.
unsafe extern "system" fn io_port_cb(
    _context: *const c_void,
    io_access: *mut WHV_EMULATOR_IO_ACCESS_INFO,
) -> HRESULT {
    // SAFETY: the emulator passes a valid, writable access descriptor.
    if let Some(io) = unsafe { io_access.as_mut() }
        && io.Direction == 0
    {
        io.Data = 0;
    }
    S_OK
}

/// Emulator callback: the MMIO memory access itself — dispatched to the caller's handler.
unsafe extern "system" fn memory_cb(
    context: *const c_void,
    access: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
) -> HRESULT {
    // SAFETY: `context` is the `EmuCtx` from `emulate`; `access` is a valid descriptor.
    let ctx: &EmuCtx = unsafe { &*(context as *const EmuCtx) };
    let acc: &mut WHV_EMULATOR_MEMORY_ACCESS_INFO = match unsafe { access.as_mut() } {
        Some(a) => a,
        None => return S_OK,
    };
    let size: usize = (acc.AccessSize as usize).min(8);
    let mut buf: [u8; 8] = acc.Data;
    let is_write: bool = acc.Direction != 0;
    // SAFETY: `ctx.mmio`/`ctx.user` form the monomorphized handler installed in `emulate`.
    unsafe {
        (ctx.mmio)(ctx.user, acc.GpaAddress, is_write, &mut buf[..size]);
    }
    if !is_write {
        acc.Data = buf;
    }
    S_OK
}

/// Emulator callback: read guest registers.
unsafe extern "system" fn get_regs_cb(
    context: *const c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *mut WHV_REGISTER_VALUE,
) -> HRESULT {
    // SAFETY: `context` is the `EmuCtx`; `names`/`values` are `count`-long arrays from the emulator.
    let ctx: &EmuCtx = unsafe { &*(context as *const EmuCtx) };
    match unsafe { WHvGetVirtualProcessorRegisters(ctx.partition, VP_INDEX, names, count, values) } {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

/// Emulator callback: write guest registers (the emulator uses this to advance RIP, etc.).
unsafe extern "system" fn set_regs_cb(
    context: *const c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *const WHV_REGISTER_VALUE,
) -> HRESULT {
    // SAFETY: `context` is the `EmuCtx`; `names`/`values` are `count`-long arrays from the emulator.
    let ctx: &EmuCtx = unsafe { &*(context as *const EmuCtx) };
    match unsafe { WHvSetVirtualProcessorRegisters(ctx.partition, VP_INDEX, names, count, values) } {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

/// Emulator callback: translate a guest virtual page to a guest physical page.
unsafe extern "system" fn translate_cb(
    context: *const c_void,
    gva: u64,
    flags: WHV_TRANSLATE_GVA_FLAGS,
    result_code: *mut WHV_TRANSLATE_GVA_RESULT_CODE,
    gpa: *mut u64,
) -> HRESULT {
    // SAFETY: `context` is the `EmuCtx`; `result_code`/`gpa` are valid out-pointers.
    let ctx: &EmuCtx = unsafe { &*(context as *const EmuCtx) };
    let mut result: WHV_TRANSLATE_GVA_RESULT = WHV_TRANSLATE_GVA_RESULT::default();
    let mut out_gpa: u64 = 0;
    match unsafe {
        WHvTranslateGva(ctx.partition, VP_INDEX, gva, flags, &mut result, &mut out_gpa)
    } {
        Ok(()) => {
            // SAFETY: valid out-pointers supplied by the emulator.
            unsafe {
                *result_code = result.ResultCode;
                *gpa = out_gpa;
            }
            S_OK
        },
        Err(e) => e.code(),
    }
}
