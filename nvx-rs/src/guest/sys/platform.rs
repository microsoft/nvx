//! Typed access to the ARM development binding's restricted platform-control device.

use std::cell::Cell;
use std::ffi::CStr;
use std::marker::PhantomData;
use std::num::NonZeroUsize;

use super::{Errno, FileDescriptor, MemoryMapping, Mode, OpenFlags, TimeValLike};

pub(crate) mod binding;

/// The only device authority exposed by this backend, never arbitrary physical memory.
const DEVICE: &CStr = c"/dev/nvx-platform";
/// The fixed device aperture and the only accepted mapping length.
const PAGE_SIZE: usize = 4096;
/// The existing process-shared record-window lock, also used by the x86 time implementation.
const RECORD_LOCK: &CStr = c"/run/nvx/portb.lock";

/// Registers of the approved development platform binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Register {
    /// Console data or selected record bytes.
    Data,
    /// Status and record selector.
    Status,
    /// Latched version-1 time-sample bytes.
    Time,
    /// Exact shutdown status.
    Shutdown,
    /// Snapshot/acknowledgement, unavailable during Phase 1.
    Snapshot,
}

impl Register {
    /// The fixed offset in the one-page restricted device.
    pub(crate) const fn offset(self) -> u64 {
        match self {
            Self::Data => 0x00,
            Self::Status => 0x04,
            Self::Time => 0x08,
            Self::Shutdown => 0x0c,
            Self::Snapshot => 0x10,
        }
    }

    /// Validates the documented exact-width transaction before any device access.
    fn check(self, width: usize, writing: bool) -> Result<(), Errno> {
        if writing {
            if width != 1 || self == Self::Time {
                return Err(Errno::EINVAL);
            }
            if self == Self::Snapshot {
                return Err(Errno::EOPNOTSUPP);
            }
        } else if !matches!(width, 1 | 2 | 4) || (self == Self::Status && width != 1) {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }
}

thread_local! {
    /// A transaction must end before this thread forks, spawns, or executes a workload.
    static HELD: Cell<bool> = const { Cell::new(false) };
    /// Failed unmapping leaves authority that this process must not hand to workload code.
    static RETAINED: Cell<bool> = const { Cell::new(false) };
}

/// Whether the thread has safely relinquished all platform authority.
pub(crate) fn check_released() -> Result<(), Errno> {
    if HELD.get() || RETAINED.get() {
        Err(Errno::EPERM)
    } else {
        Ok(())
    }
}

/// The two restricted access paths, each held by exactly one owner.
enum Backend {
    /// Preferred, non-copyable device-memory mapping.
    Mapped(MemoryMapping),
    /// Exact-width positional I/O, explicitly selected rather than masking a mapping error.
    File(FileDescriptor),
}

/// A thread-local trusted transaction; it exposes no pointer or raw mapping.
pub(crate) struct Access {
    /// Device authority until explicit release or RAII cleanup.
    backend: Option<Backend>,
    /// File-backed authority also must never leave the transaction's thread.
    _thread: PhantomData<*const ()>,
}

impl Access {
    /// Opens and maps exactly one page, then immediately closes its CLOEXEC descriptor.
    pub(crate) fn request() -> Result<Self, Errno> {
        check_released()?;
        if super::page_size() != Some(PAGE_SIZE) {
            return Err(Errno::EINVAL);
        }
        let descriptor = super::open(
            DEVICE,
            OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        let mapping = MemoryMapping::shared_read_write(
            &descriptor,
            NonZeroUsize::new(PAGE_SIZE).expect("one page is nonzero"),
            0,
        )?;
        HELD.set(true);
        let access = Self {
            backend: Some(Backend::Mapped(mapping)),
            _thread: PhantomData,
        };
        descriptor.close()?;
        Ok(access)
    }

    /// Opens the validated positional-I/O path without falling back after a denied mapping.
    pub(crate) fn positional() -> Result<Self, Errno> {
        check_released()?;
        let descriptor = super::open(
            DEVICE,
            OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        HELD.set(true);
        Ok(Self {
            backend: Some(Backend::File(descriptor)),
            _thread: PhantomData,
        })
    }

    /// Reads one byte with one naturally aligned device transaction.
    pub(crate) fn read_u8(&self, register: Register) -> Result<u8, Errno> {
        register.check(1, false)?;
        match self.backend.as_ref().expect("access retains its backend") {
            Backend::Mapped(mapping) => mapping.device_read_u8(register.offset() as usize),
            Backend::File(file) => {
                let mut bytes = [0];
                if file.pread(&mut bytes, register.offset())? != 1 {
                    return Err(Errno::EIO);
                }
                Ok(bytes[0])
            }
        }
    }

    /// Reads the next two record bytes with one ordered device transaction.
    pub(crate) fn read_u16(&self, register: Register) -> Result<u16, Errno> {
        register.check(2, false)?;
        match self.backend.as_ref().expect("access retains its backend") {
            Backend::Mapped(mapping) => mapping.device_read_u16(register.offset() as usize),
            Backend::File(file) => {
                let mut bytes = [0; 2];
                if file.pread(&mut bytes, register.offset())? != 2 {
                    return Err(Errno::EIO);
                }
                Ok(u16::from_le_bytes(bytes))
            }
        }
    }

    /// Reads the next four record bytes with one ordered device transaction.
    pub(crate) fn read_u32(&self, register: Register) -> Result<u32, Errno> {
        register.check(4, false)?;
        match self.backend.as_ref().expect("access retains its backend") {
            Backend::Mapped(mapping) => mapping.device_read_u32(register.offset() as usize),
            Backend::File(file) => {
                let mut bytes = [0; 4];
                if file.pread(&mut bytes, register.offset())? != 4 {
                    return Err(Errno::EIO);
                }
                Ok(u32::from_le_bytes(bytes))
            }
        }
    }

    /// Writes one byte; Phase 1 never requests a snapshot or acknowledges a restore.
    pub(crate) fn write_u8(&self, register: Register, value: u8) -> Result<(), Errno> {
        register.check(1, true)?;
        match self.backend.as_ref().expect("access retains its backend") {
            Backend::Mapped(mapping) => mapping.device_write_u8(register.offset() as usize, value),
            Backend::File(file) => {
                if file.pwrite(&[value], register.offset())? != 1 {
                    return Err(Errno::EIO);
                }
                Ok(())
            }
        }
    }

    /// Relinquishes device authority, surfacing an explicit unmap or close failure.
    pub(crate) fn release(mut self) -> Result<(), Errno> {
        self.end()
    }

    /// Shared cleanup for explicit release and earlier-return RAII.
    fn end(&mut self) -> Result<(), Errno> {
        let Some(backend) = self.backend.take() else {
            return Ok(());
        };
        let result = match backend {
            Backend::Mapped(mapping) => mapping.unmap(),
            Backend::File(file) => file.close(),
        };
        HELD.set(false);
        if result.is_err() {
            RETAINED.set(true);
        }
        result
    }
}

impl Drop for Access {
    fn drop(&mut self) {
        let _ = self.end();
    }
}

/// Runs a record transaction under the existing cross-process lock, relinquishing device
/// authority before unlocking or returning any record to its caller.
fn record_transaction<T>(
    open: fn() -> Result<Access, Errno>,
    read: impl FnOnce(&Access) -> Result<T, Errno>,
) -> Result<T, Errno> {
    for directory in [c"/run/nvx", c"/run/nvx/time"] {
        match super::create_directory(directory, Mode::from_bits_truncate(0o755)) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(error) => return Err(error),
        }
    }
    let _lock = super::lock_file(RECORD_LOCK, Mode::S_IRUSR | Mode::S_IWUSR)?;
    let access = open()?;
    let result = read(&access);
    access.release()?;
    result
}

/// Consumes an exact, word-aligned record from one approved register window.
fn read_window<const N: usize>(access: &Access, register: Register) -> Result<[u8; N], Errno> {
    const { assert!(N.is_multiple_of(4)) };
    let mut bytes = [0; N];
    for word in bytes.chunks_exact_mut(4) {
        word.copy_from_slice(&access.read_u32(register)?.to_le_bytes());
    }
    Ok(bytes)
}

/// Reads the approved identity transport without admitting its CPU profile.
pub(crate) fn time_binding_record() -> Result<[u8; 96], Errno> {
    record_transaction(Access::request, |access| {
        access.write_u8(Register::Status, 0xa8)?;
        let mut bytes = [0; 96];
        bytes[..8].copy_from_slice(&read_window::<8>(access, Register::Data)?);
        bytes[8..10].copy_from_slice(&access.read_u16(Register::Data)?.to_le_bytes());
        bytes[10..12].copy_from_slice(&access.read_u16(Register::Data)?.to_le_bytes());
        bytes[12..].copy_from_slice(&read_window::<84>(access, Register::Data)?);
        Ok(bytes)
    })
}

/// Reads the existing process-generation identity through the normal record selector.
pub(crate) fn generation_id() -> Result<crate::guest::GenerationId, Errno> {
    record_transaction(Access::positional, |access| {
        if access.read_u8(Register::Status)? & 0x20 == 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        access.write_u8(Register::Status, 0xa6)?;
        read_window(access, Register::Data).map(crate::guest::GenerationId::from_bytes)
    })
}

/// Writes an event to the same platform debug channel as x86 without retaining authority.
pub(crate) fn write_console(separator: &[u8], line: &[u8]) -> Result<(), Errno> {
    let access = Access::request()?;
    let result = separator
        .iter()
        .chain(line)
        .try_for_each(|&byte| access.write_u8(Register::Data, byte));
    access.release()?;
    result
}

/// Guest clock reads around one synchronous latch, before any sample bytes are consumed.
pub(crate) struct LatchedTime {
    /// The unchanged 48-byte version-1 sample, including unknown-bound sentinels.
    pub(crate) bytes: [u8; 48],
    /// Counter immediately before the selector write.
    pub(crate) before: u64,
    /// Counter immediately after the selector write.
    pub(crate) after: u64,
    /// Linux realtime read immediately after the latch.
    pub(crate) realtime_ns: i64,
    /// Counter midpoint around that realtime read.
    pub(crate) realtime_counter: u64,
    /// Half the counter interval around the realtime read.
    pub(crate) realtime_half_cycles: u64,
}

/// Latches a real host sample; no frequency, UTC, correction bound or pairing is fabricated.
pub(crate) fn latch_time() -> Result<LatchedTime, Errno> {
    record_transaction(Access::request, |access| {
        if access.read_u8(Register::Status)? & 0x40 == 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        let before = super::aarch64::virtual_counter();
        access.write_u8(Register::Status, 0xa7)?;
        let after = super::aarch64::virtual_counter();
        let realtime_before = super::aarch64::virtual_counter();
        let realtime_ns = super::clock_time(super::Clock::Realtime)?.num_nanoseconds();
        let realtime_after = super::aarch64::virtual_counter();
        if after < before || realtime_after < realtime_before {
            return Err(Errno::EPROTO);
        }
        let half = (realtime_after - realtime_before).div_ceil(2);
        Ok(LatchedTime {
            bytes: read_window(access, Register::Time)?,
            before,
            after,
            realtime_ns,
            realtime_counter: realtime_before + half,
            realtime_half_cycles: half,
        })
    })
}

/// Requests exact-status shutdown and waits only after a successful synchronous device write.
pub(crate) fn power_off(status: u8) -> ! {
    let result = Access::request().and_then(|access| {
        access.write_u8(Register::Shutdown, status)?;
        access.release()
    });
    if let Err(error) = result {
        super::write_standard_error(format!("NVX-PLATFORM-ERROR: shutdown: {error}\n").as_bytes());
        super::exit_immediately(1);
    }
    loop {
        nix::unistd::pause();
    }
}

#[cfg(test)]
mod tests;
