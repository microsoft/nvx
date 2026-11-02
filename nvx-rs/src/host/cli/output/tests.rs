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
