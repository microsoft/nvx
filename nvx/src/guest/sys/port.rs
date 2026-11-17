//! OpenVMM's x86 I/O ports, and the guest programs' access to them.
//!
//! The guest programs reach OpenVMM's ports in one of two ways. A program with `CAP_SYS_RAWIO`
//! may ask Linux for permission to execute `in` and `out` on them, which a [`PortAccess`] holds
//! for one transaction on x86-64. Any other access goes through [`PORT_DEVICE`], where the file
//! offset is the port and each byte read or written is one byte-wide port access.

use std::ffi::CStr;

/// The device whose file offsets are I/O ports, and whose bytes are byte-wide port accesses.
pub(crate) const PORT_DEVICE: &CStr = c"/dev/port";

/// OpenVMM's I/O ports that the guest programs use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Port {
    /// Port B's data port, OpenVMM's debug console: the guest writes diagnostic bytes to it, and
    /// reads the time ABI's restore packet and VM generation ID from it four bytes at a time.
    PortbData,
    /// Port B's status port: reading it returns the time ABI's status bits, and writing it
    /// selects what the data port or the window returns next.
    PortbStatus,
    /// Port B's four-byte window, from which the guest reads a time sample.
    PortbWindow,
    /// The control port: a byte written to it stops the VM with that byte as its exit status.
    Control,
    /// The snapshot port, which requests a capture and acknowledges a restore.
    Snapshot,
}

impl Port {
    /// Returns the port's number.
    pub(crate) const fn number(self) -> u16 {
        match self {
            Self::PortbData => 0xe9,
            Self::PortbStatus => 0xea,
            Self::PortbWindow => 0xeb,
            Self::Control => 0x604,
            Self::Snapshot => 0x605,
        }
    }

    /// Returns the port's offset in [`PORT_DEVICE`].
    pub(crate) const fn offset(self) -> u64 {
        self.number() as u64
    }
}

#[cfg(target_arch = "x86_64")]
pub(crate) use access::{PortAccess, PortError, check_released, power_off, probe_port};

/// Permission to execute `in` and `out` on OpenVMM's ports, on x86-64, for one transaction at a
/// time on the thread that takes it.
#[cfg(target_arch = "x86_64")]
mod access {
    use std::arch::asm;
    use std::cell::Cell;
    use std::ffi::c_ulong;
    use std::fmt::{self, Display};
    use std::marker::PhantomData;
    use std::mem::size_of;

    use super::{PORT_DEVICE, Port};
    use crate::guest::sys::{Errno, Mode, OpenFlags, open};

    /// Ranges of ports, as their first port and the last port of an access that starts in the
    /// range, whose permission a [`PortAccess`] holds: port B, whose window a four-byte access
    /// reads, and the control and snapshot ports.
    const GRANTED: [(Port, Port, u16); 2] = [
        (Port::PortbData, Port::PortbWindow, size_of::<u32>() as u16),
        (Port::Control, Port::Snapshot, size_of::<u8>() as u16),
    ];

    /// The kernel's permission for the calling thread to execute `in` and `out` on a range of
    /// ports, and those instructions, which a test replaces with a fake.
    pub(crate) trait PortBackend: Sync {
        /// Grants the calling thread permission for the `count` ports from `first` when
        /// `enable`, and revokes it otherwise.
        fn set_permission(&self, first: u16, count: u16, enable: bool) -> Result<(), Errno>;
        /// Reads one byte from `port`.
        fn read_u8(&self, port: Port) -> u8;
        /// Reads four bytes from `port` with exactly one access.
        fn read_u32(&self, port: Port) -> u32;
        /// Writes one byte to `port`.
        fn write_u8(&self, port: Port, value: u8);
    }

    /// Linux's permission, which `ioperm` changes, and the processor's port instructions, which
    /// only a [`PortAccess`] that holds the permission on the calling thread executes.
    struct Linux;

    /// The one instance of [`Linux`], which this module hands out only inside a [`PortAccess`].
    static LINUX: Linux = Linux;

    impl PortBackend for Linux {
        /// Calls `ioperm` through libc, because `nix` does not wrap it.
        fn set_permission(&self, first: u16, count: u16, enable: bool) -> Result<(), Errno> {
            // SAFETY: `ioperm` reads only its integer arguments, which name ports in the x86 I/O
            // bitmap's range.
            Errno::result(unsafe {
                nix::libc::ioperm(
                    c_ulong::from(first),
                    c_ulong::from(count),
                    i32::from(enable),
                )
            })
            .map(drop)
        }

        /// Reads with one `in al, dx`.
        #[inline]
        fn read_u8(&self, port: Port) -> u8 {
            let value: u8;
            // SAFETY: `in` accesses no memory. Only a `PortAccess` calls this, on the thread to
            // which it holds the permission for `port`.
            unsafe {
                asm!(
                    "in al, dx",
                    in("dx") port.number(),
                    out("al") value,
                    options(nomem, nostack, preserves_flags)
                );
            }
            value
        }

        /// Reads with exactly one `in eax, dx`, never a string or a narrower access, as the time
        /// ABI's window requires.
        #[inline]
        fn read_u32(&self, port: Port) -> u32 {
            let value: u32;
            // SAFETY: `in` accesses no memory. Only a `PortAccess` calls this, on the thread to
            // which it holds the permission for `port` and the three ports after it.
            unsafe {
                asm!(
                    "in eax, dx",
                    in("dx") port.number(),
                    out("eax") value,
                    options(nomem, nostack, preserves_flags)
                );
            }
            value
        }

        /// Writes with one `out dx, al`.
        #[inline]
        fn write_u8(&self, port: Port, value: u8) {
            // SAFETY: `out` accesses no memory. Only a `PortAccess` calls this, on the thread to
            // which it holds the permission for `port`.
            unsafe {
                asm!(
                    "out dx, al",
                    in("dx") port.number(),
                    in("al") value,
                    options(nomem, nostack, preserves_flags)
                );
            }
        }
    }

    /// Why the calling thread could not take, or give up, its permission for OpenVMM's ports.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum PortError {
        /// Another transaction on the calling thread holds the permission.
        Busy,
        /// The kernel refused the permission. The thread holds none of it: a partial grant was
        /// revoked.
        Grant(Errno),
        /// The kernel refused to revoke the permission, so the thread may still hold some of it.
        Revoke(Errno),
    }

    impl Display for PortError {
        /// Writes the diagnostic, such as `ioperm: Operation not permitted`.
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match *self {
                Self::Busy => {
                    formatter.write_str("ioperm: another transaction holds the port permission")
                }
                Self::Grant(error) => write!(formatter, "ioperm: {}", error.desc()),
                Self::Revoke(error) => write!(formatter, "ioperm revoke: {}", error.desc()),
            }
        }
    }

    impl std::error::Error for PortError {}

    /// The calling thread's port permission, as this module changed it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct ThreadPorts {
        /// Whether a [`PortAccess`] holds the permission for a transaction.
        open: bool,
        /// The error with which the last revocation failed, while the thread may still hold some
        /// of the permission.
        retained: Option<Errno>,
    }

    thread_local! {
        /// The calling thread's [`ThreadPorts`]. A new thread holds none of the permission,
        /// because no thread starts one while it holds some, which [`check_released`] checks.
        static THREAD_PORTS: Cell<ThreadPorts> = const {
            Cell::new(ThreadPorts {
                open: false,
                retained: None,
            })
        };
    }

    /// Changes the calling thread's permission for one range of `GRANTED` to `enable`.
    fn set_range(
        backend: &dyn PortBackend,
        (first, last, width): (Port, Port, u16),
        enable: bool,
    ) -> Result<(), Errno> {
        let count = last.number() + width - first.number();
        backend.set_permission(first.number(), count, enable)
    }

    /// Grants the calling thread the permission for every range of `GRANTED`, for one
    /// transaction. When the kernel refuses a range, every range is revoked again, so that a
    /// partial grant leaves nothing behind.
    fn grant(backend: &dyn PortBackend) -> Result<(), PortError> {
        if THREAD_PORTS.get().open {
            return Err(PortError::Busy);
        }
        for range in GRANTED {
            if let Err(error) = set_range(backend, range, true) {
                revoke(backend)?;
                return Err(PortError::Grant(error));
            }
        }
        THREAD_PORTS.set(ThreadPorts {
            open: true,
            retained: None,
        });
        Ok(())
    }

    /// Revokes the calling thread's permission for every range of `GRANTED`, and goes on after
    /// a failure, so that it revokes what it can. Until a later revocation succeeds, a failure
    /// keeps [`check_released`] from letting the thread start anything.
    fn revoke(backend: &dyn PortBackend) -> Result<(), PortError> {
        let mut failure = None;
        for range in GRANTED {
            if let Err(error) = set_range(backend, range, false) {
                failure.get_or_insert(error);
            }
        }
        THREAD_PORTS.set(ThreadPorts {
            open: false,
            retained: failure,
        });
        failure.map_or(Ok(()), |error| Err(PortError::Revoke(error)))
    }

    /// Checks that the calling thread holds none of the permission that a [`PortAccess`] took,
    /// as it must before it starts a thread or a process, which would inherit the permission.
    pub(crate) fn check_released() -> Result<(), PortError> {
        let ports = THREAD_PORTS.get();
        if ports.open {
            Err(PortError::Busy)
        } else {
            ports
                .retained
                .map_or(Ok(()), |error| Err(PortError::Revoke(error)))
        }
    }

    /// Permission for the calling thread to execute `in` and `out` on OpenVMM's ports, for one
    /// transaction, which the time tools' port-B transactions and VM power-off need.
    ///
    /// Linux keeps the permission in each thread's I/O bitmap, which the threads and processes
    /// that the thread starts inherit, and which a process keeps across `exec`, so a workload
    /// that a holder started could use the ports. A transaction therefore takes the permission
    /// on the thread that uses the ports, when no other transaction there holds it, and rolls
    /// back a partial grant; the owner never leaves that thread; and
    /// [`release`](Self::release) revokes the permission and reports a failure, after which
    /// [`check_released`] keeps the thread from starting anything until a later transaction
    /// revokes it. Dropping the owner revokes the permission too, and records a failure without
    /// reporting it.
    pub(crate) struct PortAccess<'a> {
        /// The permission and the port instructions, until the owner gives them up.
        backend: Option<&'a dyn PortBackend>,
        /// Keeps the owner on the thread whose permission it holds: a raw pointer is neither
        /// `Send` nor `Sync`.
        _thread: PhantomData<*const ()>,
    }

    impl PortAccess<'static> {
        /// Takes the calling thread's permission for OpenVMM's ports, which needs
        /// `CAP_SYS_RAWIO`.
        pub(crate) fn request() -> Result<Self, PortError> {
            Self::take(&LINUX)
        }
    }

    impl<'a> PortAccess<'a> {
        /// Takes the calling thread's permission through `backend`.
        pub(super) fn take(backend: &'a dyn PortBackend) -> Result<Self, PortError> {
            grant(backend)?;
            Ok(Self {
                backend: Some(backend),
                _thread: PhantomData,
            })
        }

        /// Returns the backend, which the owner holds until it gives it up.
        fn backend(&self) -> &'a dyn PortBackend {
            self.backend
                .expect("a port access holds its backend until it ends")
        }

        /// Reads one byte from `port` with one `in al, dx`.
        #[inline]
        pub(crate) fn read_u8(&self, port: Port) -> u8 {
            self.backend().read_u8(port)
        }

        /// Reads four bytes from `port` with exactly one `in eax, dx`, never a string or a
        /// narrower access, as the time ABI's window requires.
        #[inline]
        pub(crate) fn read_u32(&self, port: Port) -> u32 {
            self.backend().read_u32(port)
        }

        /// Writes one byte to `port` with one `out dx, al`.
        #[inline]
        pub(crate) fn write_u8(&self, port: Port, value: u8) {
            self.backend().write_u8(port, value);
        }

        /// Revokes the permission, and reports a failure, after which the thread may still hold
        /// some of it.
        pub(crate) fn release(mut self) -> Result<(), PortError> {
            self.backend.take().map_or(Ok(()), revoke)
        }
    }

    impl fmt::Debug for PortAccess<'_> {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_struct("PortAccess")
                .field("held", &self.backend.is_some())
                .finish()
        }
    }

    impl Drop for PortAccess<'_> {
        /// Revokes the permission unless [`release`](PortAccess::release) did, and records a
        /// failure as it does, without reporting it.
        fn drop(&mut self) {
            if let Some(backend) = self.backend.take() {
                let _ = revoke(backend);
            }
        }
    }

    /// Stops the VM with `status` through the control port, and waits for OpenVMM to end the
    /// process.
    ///
    /// The port is written directly when the process may access it, and otherwise through
    /// [`PORT_DEVICE`], as the C program did. Neither write returns an error that the caller
    /// could act on, so the process waits for signals until the VM stops, holding no port
    /// permission.
    pub(crate) fn power_off(status: u8) -> ! {
        match PortAccess::request() {
            Ok(access) => {
                access.write_u8(Port::Control, status);
                let _ = access.release();
            }
            Err(_) => {
                let flags = OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC;
                if let Ok(device) = open(PORT_DEVICE, flags, Mode::empty()) {
                    let _ = device.pwrite(&[status], Port::Control.offset());
                }
            }
        }
        loop {
            nix::unistd::pause();
        }
    }

    /// Reads one byte from `port` without taking the permission, as a workload that probes the
    /// ports does, and returns it.
    ///
    /// Unless the calling thread holds the permission for `port`, such as one that its process
    /// inherited from the thread that started it, the processor refuses the instruction, and
    /// Linux ends the process with `SIGSEGV`. `nvx-time`'s port-access test hook relies on it.
    pub(crate) fn probe_port(port: Port) -> u8 {
        let value: u8;
        // SAFETY: `in` accesses no memory. Without the permission it faults, and Linux sends
        // `SIGSEGV`, whose default action ends the process: the standard library's handler of
        // the signal handles only a stack overflow, and otherwise restores that action.
        unsafe {
            asm!(
                "in al, dx",
                in("dx") port.number(),
                out("al") value,
                options(nomem, nostack, preserves_flags)
            );
        }
        value
    }
}

#[cfg(test)]
pub(super) mod tests;
