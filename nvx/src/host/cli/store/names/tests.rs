use super::{is_random_staging, split_staging};

#[test]
fn random_staging_names_have_six_ascii_letters_or_digits_after_the_prefix() {
    for name in [".tmpAb3xY9", ".tmp000000", ".tmpzzzzzz"] {
        assert!(is_random_staging(name), "{name}");
    }
    for name in [
        ".tmp",
        ".tmpAb3xY",
        ".tmpAb3xY90",
        ".tmpAb3-Y9",
        ".tmpAb3\u{e9}Y",
        "tmpAb3xY9",
        ".TMPAb3xY9",
        ".outcome.json.7.tmp",
    ] {
        assert!(!is_random_staging(name), "{name}");
    }
}

#[test]
fn named_staging_names_split_at_the_last_dot_before_the_suffix() {
    assert_eq!(
        split_staging(".outcome.json.7.tmp"),
        Some(("outcome.json", "7"))
    );
    assert_eq!(split_staging(".a.b..tmp"), Some(("a.b", "")));
    assert_eq!(split_staging("..x.tmp"), Some(("", "x")));
    for name in [
        "outcome.json.7.tmp",
        ".outcome.json.7",
        ".outcome.json.7.TMP",
        ".nodot.tmp",
        ".tmp",
    ] {
        assert_eq!(split_staging(name), None, "{name}");
    }
}
