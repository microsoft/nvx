use super::parse_ready_event;

#[test]
fn only_the_complete_openvmm_event_is_ready() {
    let event = b"OPENVMM_RESTORE_READY_V1\n";
    parse_ready_event(event).unwrap();
    for length in 0..event.len() {
        assert!(parse_ready_event(&event[..length]).is_err(), "{length}");
    }
    for malformed in [
        &b"OPENVMM_RESTORE_READY_V2\n"[..],
        b"OPENVMM_RESTORE_READY_V1\r\n",
        b"OPENVMM_RESTORE_READY_V1\n\n",
        b"OPENVMM_RESTORE_READY_V1\n\0",
        b" OPENVMM_RESTORE_READY_V1\n",
        b"openvmm_restore_ready_v1\n",
        b"OPENVMM_RESTORE_READY_V1\nOPENVMM_RESTORE_READY_V1\n",
        b"OPENVMM_RESTORE_READY_V1\xff",
    ] {
        assert!(parse_ready_event(malformed).is_err(), "{malformed:?}");
    }
}
