use super::*;
use crate::ErrorKind;

/// Returns the message with which declaring `config`, and then `others`, fails.
fn refusal(config: &str, others: &[&str]) -> String {
    let declared = CallerFiles::new(config).and_then(|files| {
        others
            .iter()
            .try_fold(files, |files, &name| files.with(name))
    });
    let error = declared.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    error.message().to_owned()
}

#[test]
fn declarations_keep_their_names() {
    let files = CallerFiles::new("sandbox.json")
        .unwrap()
        .with("boot.sock")
        .unwrap()
        .with(".config.json.1.tmp")
        .unwrap();
    assert_eq!(files.config(), Some("sandbox.json"));
    assert_eq!(files.others(), ["boot.sock", ".config.json.1.tmp"]);
    assert_eq!(
        files.names().collect::<Vec<_>>(),
        ["boot.sock", ".config.json.1.tmp", "sandbox.json"]
    );
    let none = CallerFiles::none().with("boot.sock").unwrap();
    assert_eq!(none.config(), None);
    assert_eq!(none.names().collect::<Vec<_>>(), ["boot.sock"]);
    assert_eq!(CallerFiles::none(), CallerFiles::default());
}

#[test]
fn names_that_leave_the_directory_are_refused_on_every_host() {
    for (name, problem) in [
        ("", "is empty"),
        (".", "names a directory rather than a file in it"),
        ("..", "names a directory rather than a file in it"),
        ("/etc/passwd", "has a path separator"),
        ("../config.json", "has a path separator"),
        ("state/config.json", "has a path separator"),
        ("config.json/", "has a path separator"),
        ("C:\\config.json", "has a path separator"),
        ("..\\config.json", "has a path separator"),
        ("\\\\?\\C:\\config.json", "has a path separator"),
        ("\\", "has a path separator"),
        ("C:", "has a character that Windows refuses in a file name"),
        (
            "C:config.json",
            "has a character that Windows refuses in a file name",
        ),
        (
            "config.json:stream",
            "has a character that Windows refuses in a file name",
        ),
        (
            "config\0.json",
            "has a character that Windows refuses in a file name",
        ),
        (
            "config\n.json",
            "has a character that Windows refuses in a file name",
        ),
        (
            "config?.json",
            "has a character that Windows refuses in a file name",
        ),
        (
            "config.json.",
            "ends with a dot or a space, which Windows drops",
        ),
        (
            "config.json ",
            "ends with a dot or a space, which Windows drops",
        ),
        ("NUL", "is a name that Windows reserves for a device"),
        ("nul.json", "is a name that Windows reserves for a device"),
        ("Com1.json", "is a name that Windows reserves for a device"),
        ("lpt\u{b9}", "is a name that Windows reserves for a device"),
        ("CON .json", "is a name that Windows reserves for a device"),
        ("conout$", "is a name that Windows reserves for a device"),
    ] {
        let expected = format!("the caller's file name {name:?} {problem}");
        assert_eq!(refusal(name, &[]), expected);
        assert_eq!(refusal("config.json", &[name]), expected);
    }
    // Names that only resemble those are plain file names.
    for name in [
        "...json",
        "com10",
        "console.json",
        "nul-file",
        "lpt",
        "a_b.json",
    ] {
        CallerFiles::new(name).unwrap();
    }
}

#[test]
fn names_that_a_file_system_could_take_for_others_are_refused() {
    let problem = "has a character other than ASCII letters and digits, `.`, `_`, and `-`";
    for name in [
        // The uppercase forms of `ı` and `ſ` are `I` and `S`, which `to_lowercase` does not
        // tell, so a file system that compares the uppercase forms of names could take these
        // for the store's files.
        "runt\u{131}me.json",
        "control.\u{17f}ock",
        // macOS's file systems take a composed letter for the same letter followed by a
        // combining mark.
        "caf\u{e9}.json",
        "cafe\u{301}.json",
        // Windows names `runtime.json` by this short name too on volumes with short names.
        "RUNTIM~1.JSO",
        "a b.json",
    ] {
        let expected = format!("the caller's file name {name:?} {problem}");
        assert_eq!(refusal(name, &[]), expected);
        assert_eq!(refusal("config.json", &[name]), expected);
    }
}

#[test]
fn names_that_collide_are_refused_whatever_their_case() {
    for (name, problem) in [
        ("sandbox.lock", "is the name of one of the store's files"),
        ("Runtime.JSON", "is the name of one of the store's files"),
        ("outcome.json", "is the name of one of the store's files"),
        (
            ".runtime.json.12345.tmp",
            "has the form of a temporary file that the store owns",
        ),
        (
            ".SANDBOX.LOCK.1.TMP",
            "has the form of a temporary file that the store owns",
        ),
        (
            ".tmpAbC123",
            "has the form of a temporary file that the store owns",
        ),
        (
            ".TMP000000",
            "has the form of a temporary file that the store owns",
        ),
        (
            ".nfs000000001",
            "has the form of a removed file that a removal waits for",
        ),
        (
            ".NFS1",
            "has the form of a removed file that a removal waits for",
        ),
    ] {
        let expected = format!("the caller's file name {name:?} {problem}");
        assert_eq!(refusal(name, &[]), expected);
        assert_eq!(refusal("config.json", &[name]), expected);
    }
    let twice = |name: &str| format!("the caller's file name {name:?} is declared already");
    assert_eq!(
        refusal("config.json", &["config.json"]),
        twice("config.json")
    );
    assert_eq!(
        refusal("config.json", &["CONFIG.json"]),
        twice("CONFIG.json")
    );
    assert_eq!(
        refusal("config.json", &["boot.sock", "Boot.Sock"]),
        twice("Boot.Sock")
    );
    let none = CallerFiles::none().with("boot.sock").unwrap();
    assert_eq!(
        none.with("BOOT.sock").unwrap_err().message(),
        twice("BOOT.sock")
    );
}

#[test]
fn store_temporaries_name_one_of_the_stores_files_and_a_number() {
    for name in [
        ".runtime.json.0.tmp",
        ".sandbox.lock.18446744073709551615.tmp",
    ] {
        assert!(is_store_temporary(name), "{name}");
    }
    for name in [
        ".config.json.1.tmp",
        ".runtime.json..tmp",
        ".runtime.json.1a.tmp",
        "runtime.json.1.tmp",
        ".runtime.json.1.tmp.tmp",
        ".tmpAbC123",
        ".Runtime.json.1.tmp",
    ] {
        assert!(!is_store_temporary(name), "{name}");
    }
}

#[test]
fn report_staging_files_have_the_names_that_tempfile_gives_them() {
    for name in [".tmpAbC123", ".tmp000000", ".tmpzzzzzz"] {
        assert!(is_report_staging(name), "{name}");
    }
    // Other names are not the VMM's, whatever they resemble.
    for name in [
        ".tmpAbC12",
        ".tmpAbC1234",
        ".tmpAb-123",
        ".tmpAb.123",
        ".tmpAbC12\u{e9}",
        ".TMPAbC123",
        "tmpAbC123",
        ".tmp",
        ".runtime.json.1.tmp",
    ] {
        assert!(!is_report_staging(name), "{name}");
    }
}
