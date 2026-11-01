//! Checks of OpenVMM's port map, and of the transactions that hold the permission for it.

use super::*;

#[cfg(target_arch = "x86_64")]
pub(crate) use super::access::PortBackend;

#[cfg(target_arch = "x86_64")]
impl<'a> PortAccess<'a> {
    /// Takes the permission through a test's fake `backend`, whose port instructions the
    /// test fakes too.
    pub(crate) fn with_backend(backend: &'a dyn PortBackend) -> Result<Self, PortError> {
        Self::take(backend)
    }
}

#[test]
fn ports_have_openvmm_numbers() {
    let ports = [
        (Port::PortbData, 0xe9),
        (Port::PortbStatus, 0xea),
        (Port::PortbWindow, 0xeb),
        (Port::Control, 0x604),
        (Port::Snapshot, 0x605),
    ];
    for (port, number) in ports {
        assert_eq!(port.number(), number, "{port:?}");
        assert_eq!(port.offset(), u64::from(number), "{port:?}");
    }
}

/// Transactions over a fake permission. Each test runs on a thread of its own, whose
/// bookkeeping starts empty.
#[cfg(target_arch = "x86_64")]
mod transactions {
    use std::sync::Mutex;

    use super::*;
    use crate::guest::sys::Errno;

    /// The grants of port B's six ports, and of the control and snapshot ports, in order, as
    /// `(first, count, enable)`.
    const GRANTS: [(u16, u16, bool); 2] = [(0xe9, 6, true), (0x604, 2, true)];
    /// The revocations of the same ranges, in order.
    const REVOKES: [(u16, u16, bool); 2] = [(0xe9, 6, false), (0x604, 2, false)];

    /// A permission that records its changes and refuses those that a test selects, and
    /// whose port instructions read zero and write nothing.
    #[derive(Default)]
    struct FakePermission {
        /// The changes, as `(first, count, enable)`, in order.
        changes: Mutex<Vec<(u16, u16, bool)>>,
        /// The errors with which grants of port B and of the control ports fail.
        grant_errors: [Option<Errno>; 2],
        /// The error with which every revocation fails.
        revoke_error: Mutex<Option<Errno>>,
    }

    impl FakePermission {
        /// Returns the changes so far.
        fn changes(&self) -> Vec<(u16, u16, bool)> {
            self.changes.lock().unwrap().clone()
        }

        /// Makes every later revocation fail with `error`, or succeed.
        fn fail_revocations(&self, error: Option<Errno>) {
            *self.revoke_error.lock().unwrap() = error;
        }
    }

    impl PortBackend for FakePermission {
        fn set_permission(&self, first: u16, count: u16, enable: bool) -> Result<(), Errno> {
            self.changes.lock().unwrap().push((first, count, enable));
            let error = if enable {
                self.grant_errors[usize::from(first != GRANTS[0].0)]
            } else {
                *self.revoke_error.lock().unwrap()
            };
            error.map_or(Ok(()), Err)
        }

        fn read_u8(&self, _: Port) -> u8 {
            0
        }

        fn read_u32(&self, _: Port) -> u32 {
            0
        }

        fn write_u8(&self, _: Port, _: u8) {}
    }

    #[test]
    fn a_transaction_holds_both_ranges_until_it_is_released() {
        let backend = FakePermission::default();
        let access = PortAccess::with_backend(&backend).unwrap();
        assert_eq!(backend.changes(), GRANTS);
        // Nothing may start while a transaction holds the permission.
        assert_eq!(check_released(), Err(PortError::Busy));
        access.release().unwrap();
        assert_eq!(backend.changes(), [GRANTS, REVOKES].concat());
        assert_eq!(check_released(), Ok(()));
    }

    #[test]
    fn a_refused_or_partial_grant_leaves_nothing_granted() {
        // The kernel refuses port B, or the control ports after it granted port B, whose
        // grant the transaction then rolls back.
        for (refused, attempted) in [(0, 1), (1, 2)] {
            let mut backend = FakePermission::default();
            backend.grant_errors[refused] = Some(Errno::EPERM);
            assert_eq!(
                PortAccess::with_backend(&backend).unwrap_err(),
                PortError::Grant(Errno::EPERM),
                "{refused}"
            );
            assert_eq!(
                backend.changes(),
                [&GRANTS[..attempted], &REVOKES].concat(),
                "{refused}"
            );
            assert_eq!(check_released(), Ok(()), "{refused}");
        }
    }

    #[test]
    fn a_failed_rollback_keeps_the_thread_from_starting_anything() {
        let mut backend = FakePermission::default();
        backend.grant_errors[1] = Some(Errno::EPERM);
        backend.fail_revocations(Some(Errno::ENOMEM));
        assert_eq!(
            PortAccess::with_backend(&backend).unwrap_err(),
            PortError::Revoke(Errno::ENOMEM)
        );
        assert_eq!(backend.changes(), [GRANTS, REVOKES].concat());
        assert_eq!(check_released(), Err(PortError::Revoke(Errno::ENOMEM)));
    }

    #[test]
    fn a_failed_revocation_keeps_the_thread_from_starting_anything_until_one_succeeds() {
        let backend = FakePermission::default();
        let access = PortAccess::with_backend(&backend).unwrap();
        backend.fail_revocations(Some(Errno::ENOMEM));
        assert_eq!(access.release(), Err(PortError::Revoke(Errno::ENOMEM)));
        // The revocation went on past the first refusal.
        assert_eq!(backend.changes(), [GRANTS, REVOKES].concat());
        assert_eq!(check_released(), Err(PortError::Revoke(Errno::ENOMEM)));
        backend.fail_revocations(None);
        PortAccess::with_backend(&backend)
            .unwrap()
            .release()
            .unwrap();
        assert_eq!(check_released(), Ok(()));
    }

    #[test]
    fn a_thread_that_may_hold_the_permission_spawns_no_command() {
        use crate::guest::spawn::Command;
        use crate::guest::sys::{self, ChildStatus};

        let command = || Command::new(c"/bin/true".to_owned(), vec![c"true".to_owned()]);
        let refusal = |command: Command| command.spawn().unwrap_err().raw_os_error();
        let backend = FakePermission::default();
        let access = PortAccess::with_backend(&backend).unwrap();
        assert_eq!(refusal(command()), Some(Errno::EPERM as i32));
        backend.fail_revocations(Some(Errno::ENOMEM));
        assert_eq!(access.release(), Err(PortError::Revoke(Errno::ENOMEM)));
        assert_eq!(refusal(command()), Some(Errno::EPERM as i32));
        // A later transaction revokes the permission, and the thread starts a command again.
        backend.fail_revocations(None);
        PortAccess::with_backend(&backend)
            .unwrap()
            .release()
            .unwrap();
        let spawned = command().spawn().unwrap();
        let process = spawned.process();
        assert_eq!(spawned.into_report().wait().unwrap(), None);
        assert_eq!(sys::wait(process), Some(ChildStatus::Exited(0)));
    }

    #[test]
    fn dropping_a_transaction_revokes_it_and_records_a_failure() {
        let backend = FakePermission::default();
        drop(PortAccess::with_backend(&backend).unwrap());
        assert_eq!(backend.changes(), [GRANTS, REVOKES].concat());
        assert_eq!(check_released(), Ok(()));
        let access = PortAccess::with_backend(&backend).unwrap();
        backend.fail_revocations(Some(Errno::EIO));
        drop(access);
        assert_eq!(check_released(), Err(PortError::Revoke(Errno::EIO)));
    }

    #[test]
    fn a_thread_holds_one_transaction_at_a_time() {
        let backend = FakePermission::default();
        let access = PortAccess::with_backend(&backend).unwrap();
        assert_eq!(
            PortAccess::with_backend(&backend).unwrap_err(),
            PortError::Busy
        );
        // The bookkeeping, like the kernel's permission, is per thread, and the fake grants
        // nothing that the other thread could inherit.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    assert_eq!(check_released(), Ok(()));
                    PortAccess::with_backend(&backend)
                        .unwrap()
                        .release()
                        .unwrap();
                })
                .join()
                .unwrap();
        });
        access.release().unwrap();
        assert_eq!(
            backend.changes(),
            [GRANTS, GRANTS, REVOKES, REVOKES].concat()
        );
    }

    #[test]
    fn a_transaction_stays_on_the_thread_that_took_it() {
        /// Implemented twice for a `Send` type, which makes `check` ambiguous for it.
        trait AmbiguousIfSend<A> {
            /// Compiles only when one implementation applies.
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
        /// Implemented twice for a `Sync` type, which makes `check` ambiguous for it.
        trait AmbiguousIfSync<A> {
            /// Compiles only when one implementation applies.
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfSync<()> for T {}
        impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}
        <PortAccess<'static> as AmbiguousIfSend<_>>::check();
        <PortAccess<'static> as AmbiguousIfSync<_>>::check();
    }
}
