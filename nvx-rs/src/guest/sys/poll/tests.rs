//! Checks of polling pipes.

use super::*;
use crate::guest::sys::create_pipe;

#[test]
fn poll_reports_input_in_the_callers_order() {
    let (idle_reader, _idle_writer) = create_pipe().unwrap();
    let (reader, writer) = create_pipe().unwrap();
    writer.write_all(b"x").unwrap();
    let events = poll(
        [Pollable::File(&idle_reader), Pollable::File(&reader)],
        Some(Duration::ZERO),
    )
    .unwrap();
    assert_eq!(
        events,
        [
            PollEvents::default(),
            PollEvents {
                readable: true,
                writable: false,
                error: false
            }

            #[test]
            fn poll_distinguishes_backpressure_from_a_readable_wake() {
                let (reader, writer) = create_pipe().unwrap();
                writer.set_nonblocking().unwrap();
                assert!(
                    poll([Pollable::Writable(&writer)], Some(Duration::ZERO)).unwrap()[0].writable
                );
                let bytes = [0; 4096];
                loop {
                    match writer.write(&bytes) {
                        Ok(count) => assert!(count != 0),
                        Err(Errno::EAGAIN) => break,
                        Err(error) => panic!("cannot fill the owned pipe: {error}"),
                    }
                }
                let (wake, signal) = create_pipe().unwrap();
                signal.write_all(b"x").unwrap();
                let events = poll(
                    [Pollable::Writable(&writer), Pollable::File(&wake)],
                    Some(Duration::ZERO),
                )
                .unwrap();
                assert_eq!(events[0], PollEvents::default());
                assert!(events[1].readable && !events[1].writable);
                reader.read_exact(&mut [0; 4096]).unwrap();
                assert!(
                    poll([Pollable::Writable(&writer)], Some(Duration::ZERO)).unwrap()[0].writable
                );
            }
        ]
    );
}
