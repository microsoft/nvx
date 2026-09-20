// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chipset::microvm::MicrovmSnapshotRequest;
use chipset_device::io::IoResult;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

#[test]
fn native_snapshot_request_completed_without_poll_observation() {
    let (send, mut recv) = mesh::channel();
    let mut device = MicrovmSnapshotRequest::new(Some(send), Duration::from_secs(1));
    let mut cx = Context::from_waker(Waker::noop());
    let IoResult::Defer(mut first_write) = device.io_write(0x605, &[0]) else {
        panic!("first request was not deferred");
    };
    let mut first = recv.try_recv().expect("first request was not delivered");
    first.release_write.send(());
    device.poll_device(&mut cx);
    assert!(matches!(first_write.poll_write(&mut cx), Poll::Ready(Ok(()))));
    assert!(matches!(
        Pin::new(&mut first.write_completed).poll(&mut cx),
        Poll::Ready(Ok(()))
    ));
    first.transaction_complete.complete(());

    // Deliberately make the second normal PIO write before the next device poll.
    let result = device.io_write(0x605, &[0]);
    let accepted = matches!(result, IoResult::Defer(_));
    let coalesced = matches!(result, IoResult::Ok);
    let second = recv.try_recv().ok();
    let notified = second.is_some();
    println!(
        "NATIVE-REUSE-COMPLETED-WITHOUT-POLL accepted={accepted} coalesced={coalesced} notified={notified}"
    );
    assert!(accepted ^ coalesced, "unexpected PIO error");
    assert_eq!(accepted, notified, "PIO result and notification disagree");

    // Both revisions must remain usable; the observation above distinguishes
    // their existing behavior without asserting a model-generated expectation.
    if coalesced {
        device.poll_device(&mut cx);
        assert!(matches!(device.io_write(0x605, &[0]), IoResult::Defer(_)));
        assert!(recv.try_recv().is_ok());
    }
}
