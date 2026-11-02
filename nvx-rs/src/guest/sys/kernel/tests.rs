//! Checks of the kernel queries on the build host.

use super::*;

#[test]
fn the_process_has_a_vdso() {
    assert!(has_vdso());
}

#[test]
fn the_kernel_log_reads_into_any_buffer() {
    // Containers and unprivileged users may not read the log.
    if let Ok(size) = kernel_log_size() {
        assert!(size > 0);
        let mut buffer = vec![0; size];
        if let Ok(count) = read_kernel_log(&mut buffer) {
            assert!(count <= size);
        }
        assert!(matches!(read_kernel_log(&mut []), Ok(0) | Err(_)));
    }
}
