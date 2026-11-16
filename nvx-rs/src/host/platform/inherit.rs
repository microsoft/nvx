//! The descriptors (Unix) or handles (Windows) of a caller that a VMM inherits when the crate's
//! spawn of a VMM, which [`vm::spawn`](crate::host::vm::spawn) makes, starts it, and the lock that
//! keeps the caller's other spawns out of the window in which they are inheritable.

#[cfg(windows)]
use std::cell::RefCell;
use std::marker::PhantomData;
#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd};
#[cfg(windows)]
use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::sync::MutexGuard;
#[cfg(windows)]
use std::sync::{Mutex, PoisonError};

/// The lock of the window in which a spawn of a VMM makes handles of its caller inheritable, from
/// before it makes the first inheritable until it has closed or restored them all, which a
/// [`SpawnLock`] holds.
///
/// It also serializes those spawns. Two spawns that allow the same handle would otherwise take
/// each other's temporary inheritability for the handle's own: one could find the handle no
/// longer inheritable when it creates its process, and fail, and then leave the handle
/// inheritable for good.
#[cfg(windows)]
static INHERITANCE: Mutex<()> = Mutex::new(());

#[cfg(windows)]
thread_local! {
    /// How many [`SpawnLock`]s this thread holds, and its hold on [`INHERITANCE`] while it holds
    /// any.
    static HELD: RefCell<(usize, Option<MutexGuard<'static, ()>>)> =
        const { RefCell::new((0, None)) };
}

/// A hold on this process's spawns of VMMs, during which no spawn of a VMM by another thread
/// makes a handle of this process inheritable, so that a process that the holder spawns
/// meanwhile, through any API, inherits no handle of a VMM.
///
/// On Windows, a spawn of a VMM makes the VMM's standard handles, the
/// read end of the pipe that carries its capability and two handles of its log, and the handles
/// that it allows, inheritable while it creates the VMM. A process that another thread creates in
/// that window with inheritance but without a list of handles, as `std::process::Command` creates
/// one, inherits every handle of this process that is inheritable then, the VMM's among them, and
/// holds them until it exits. It could read the VMM's capability before the VMM does, and it
/// holds the VMM's log, and the handles that the VMM is allowed, such as the claim of a scratch
/// image, after the VMM has exited. It holds no claim of the VMM's launch on the log, which the
/// VMM's job then decides, so a recovery that finds the VMM gone still ends the launch. Each spawn
/// of a VMM holds this lock throughout that window, so a process that the caller spawns under it
/// inherits none of those handles, and one that the caller spawns otherwise still can.
///
/// On other hosts it holds nothing. On Linux, a process that another thread spawns holds a copy
/// of a VMM's log only from its fork until it executes its program, which closes the copy, and
/// recovery checks a held claim again for long enough to see the claim released.
///
/// The locks of one thread share one hold, which lasts until the last of them is dropped: a
/// thread that holds one takes another at once, and may spawn a VMM, whose window then opens and
/// closes under its hold. A spawn of a VMM by another thread waits for the hold, so hold it only
/// while spawning.
#[must_use = "the lock is released when it is dropped"]
#[derive(Debug)]
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "outside the tests, which take it on every host, only the spawns of VMMs on Windows take it"
    )
)]
pub(crate) struct SpawnLock(PhantomData<MutexGuard<'static, ()>>);

/// Waits until no spawn of a VMM of this process has handles inheritable for it, and returns a
/// [`SpawnLock`], which keeps the spawns of VMMs of other threads from making any inheritable
/// until it is dropped, as it describes.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "outside the tests, which take it on every host, only the spawns of VMMs on Windows take it"
    )
)]
pub(crate) fn lock_spawns() -> SpawnLock {
    #[cfg(windows)]
    HELD.with_borrow_mut(|(locks, held)| {
        if *locks == 0 {
            *held = Some(INHERITANCE.lock().unwrap_or_else(PoisonError::into_inner));
        }
        *locks += 1;
    });
    SpawnLock(PhantomData)
}

#[cfg(windows)]
impl Drop for SpawnLock {
    fn drop(&mut self) {
        // A thread whose storage is being destroyed released its hold with it.
        let _ = HELD.try_with(|held| {
            let (locks, held) = &mut *held.borrow_mut();
            *locks -= 1;
            if *locks == 0 {
                *held = None;
            }
        });
    }
}

/// The descriptor of the caller that an [`Inherited`] borrows.
#[cfg(unix)]
type Borrowed<'a> = BorrowedFd<'a>;
/// The handle of the caller that an [`Inherited`] borrows.
#[cfg(windows)]
type Borrowed<'a> = BorrowedHandle<'a>;
/// What an [`Inherited`] would borrow on a host without descriptors or handles, which has none.
#[cfg(not(any(unix, windows)))]
#[derive(Debug, Clone, Copy)]
struct Borrowed<'a>(std::convert::Infallible, PhantomData<&'a ()>);

/// A descriptor (Unix) or handle (Windows) of the caller, such as that of an open
/// [`File`](std::fs::File), that a VMM inherits under the number that it has in the caller, which
/// the caller can therefore name in the VMM's arguments.
///
/// A spawn of a VMM passes the VMM these and its standard streams, and nothing else of the
/// caller's:
///
/// - On Linux, the descriptor must be close-on-exec in the caller, as Rust opens its files, and
///   stays so: only the VMM's own process clears the flag, between its fork and its exec, so no
///   process that another thread spawns meanwhile inherits it. A spawn refuses a descriptor that
///   is not close-on-exec, since any process that the caller spawns would inherit it.
/// - On Windows, the handle joins the explicit list of handles that the VMM inherits, and is
///   inheritable only while the VMM is created, after which the spawn restores its earlier
///   inheritability. Spawns of VMMs take that window one at a time, under the crate's spawn lock,
///   so several may allow the same handle. A process that another thread creates in that window
///   without such a list, as `std::process::Command` does, can inherit it too, and holds it until
///   it exits, unless the caller spawns that process under the crate's spawn lock.
///
/// Other hosts fail a spawn that passes any.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Inherited<'a>(Borrowed<'a>);

impl<'a> Inherited<'a> {
    /// Names the descriptor of `object`, such as a [`File`](std::fs::File).
    #[cfg(unix)]
    pub(crate) fn new(object: &'a impl AsFd) -> Self {
        Self(object.as_fd())
    }

    /// Names the handle of `object`, such as a [`File`](std::fs::File).
    #[cfg(windows)]
    pub(crate) fn new(object: &'a impl AsHandle) -> Self {
        Self(object.as_handle())
    }
}

#[cfg(unix)]
impl<'a> From<BorrowedFd<'a>> for Inherited<'a> {
    fn from(descriptor: BorrowedFd<'a>) -> Self {
        Self(descriptor)
    }
}

#[cfg(windows)]
impl<'a> From<BorrowedHandle<'a>> for Inherited<'a> {
    fn from(handle: BorrowedHandle<'a>) -> Self {
        Self(handle)
    }
}

#[cfg(unix)]
impl AsFd for Inherited<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0
    }
}

#[cfg(windows)]
impl AsHandle for Inherited<'_> {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0
    }
}
