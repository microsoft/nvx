//! The tests of the lock of the downloads into a checkout.

use std::sync::atomic::{AtomicBool, Ordering};

use super::super::tests::checkout;
use super::*;

#[test]
fn one_download_at_a_time_installs_into_a_checkout() {
    let (_directory, dir) = checkout(true);
    let first = DownloadLock::acquire(&dir, Report::Stderr).unwrap();
    let acquired = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let lock = DownloadLock::acquire(&dir, Report::Stderr).unwrap();
            acquired.store(true, Ordering::SeqCst);
            drop(lock);
        });
        std::thread::sleep(Duration::from_millis(500));
        assert!(!acquired.load(Ordering::SeqCst));
        drop(first);
        waiter.join().unwrap();
    });
    assert!(acquired.load(Ordering::SeqCst));
    // The lock file stays, and the next download takes its lock at once.
    assert!(dir.root().join("build").join(LOCK_NAME).is_file());
    drop(DownloadLock::acquire(&dir, Report::Stderr).unwrap());
}
