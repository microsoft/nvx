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
                error: false
            }
        ]
    );
}
