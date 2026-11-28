use super::*;

#[test]
fn lines_end_as_python_prints_them() {
    if cfg!(windows) {
        assert_eq!(line("a\nb"), "a\r\nb\r\n");
    } else {
        assert_eq!(line("a\nb"), "a\nb\n");
    }
}

#[test]
fn only_output_that_leaves_its_last_line_open_gets_a_line_end() {
    assert_eq!(line_end_after(b"progress"), LINE_SEPARATOR);
    assert_eq!(line_end_after(b"progress\r"), LINE_SEPARATOR);
    for ended in [&b""[..], b"done\n", b"done\r\n"] {
        assert_eq!(line_end_after(ended), "", "{ended:?}");
    }
}

#[test]
fn output_to_a_stream_that_is_not_a_terminal_passes_its_bytes_unchanged() {
    thread_local! {
        static WRITTEN: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    fn capture(data: &[u8]) -> io::Result<()> {
        WRITTEN.with_borrow_mut(|written| written.extend_from_slice(data));
        Ok(())
    }

    let file = tempfile::tempfile().unwrap();
    assert!(!file.is_terminal());
    let mut output = ConsoleOutput::to_stream(capture, &file);
    let chunks = [&b"caf\xc3"[..], b"\xa9 \xff|", b"\xe2\x82"];
    for chunk in chunks {
        output.write_all(chunk).unwrap();
    }
    output.finish().unwrap();
    assert_eq!(WRITTEN.take(), chunks.concat());
}
