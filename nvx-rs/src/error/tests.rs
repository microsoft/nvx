use super::*;

#[test]
fn errors_keep_their_kind_message_and_cause() {
    let cause = std::io::Error::other("disk full");
    let error = Error::io("cannot write /state/runtime.json").with_source(cause);
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(error.code(), ErrorCode::Io);
    assert_eq!(error.to_string(), "cannot write /state/runtime.json");
    assert_eq!(
        std::error::Error::source(&error).unwrap().to_string(),
        "disk full"
    );
    let (kind, message, source) = error.into_parts();
    assert_eq!(kind, ErrorKind::Io);
    assert_eq!(message, "cannot write /state/runtime.json");
    assert_eq!(source.unwrap().to_string(), "disk full");
    assert_eq!(ErrorKind::TimedOut.to_string(), "timed-out");
}

#[test]
fn codes_determine_kinds() {
    let error = Error::new(ErrorCode::SandboxExited, "sandbox x stopped unexpectedly");
    assert_eq!(error.code(), ErrorCode::SandboxExited);
    assert_eq!(error.kind(), ErrorKind::Exited);
    assert_eq!(error.into_parts().0, ErrorKind::Exited);
    let mismatch = Error::new(ErrorCode::RuntimeMismatch, "another runtime");
    assert_eq!(mismatch.code(), ErrorCode::RuntimeMismatch);
    assert_eq!(mismatch.kind(), ErrorKind::RuntimeMismatch);
    let (kind, message, source) = mismatch.into_parts();
    assert_eq!(kind, ErrorKind::RuntimeMismatch);
    assert_eq!(message, "another runtime");
    assert!(source.is_none());
    assert_eq!(
        Error::new(ErrorKind::TimedOut, "late").code(),
        ErrorCode::TimedOut
    );
    assert_eq!(
        Error::new(ErrorKind::RuntimeMismatch, "elsewhere").code(),
        ErrorCode::RuntimeMismatch
    );
    assert_eq!(Error::unsupported("no").code(), ErrorCode::Unsupported);
    assert_eq!(
        ErrorCode::SandboxNotRunning.to_string(),
        "sandbox-not-running"
    );
}

#[test]
fn every_kind_has_a_code_of_its_own_spelling() {
    for kind in [
        ErrorKind::Unavailable,
        ErrorKind::Unsupported,
        ErrorKind::Exited,
        ErrorKind::StillRunning,
        ErrorKind::Io,
        ErrorKind::NotFound,
        ErrorKind::Protocol,
        ErrorKind::TimedOut,
        ErrorKind::RuntimeMismatch,
        ErrorKind::Other,
    ] {
        let code = ErrorCode::from(kind);
        assert_eq!(code.kind(), kind);
        assert_eq!(code.as_str(), kind.as_str());
    }
}

#[test]
fn codes_are_unique_lowercase_words_joined_by_hyphens() {
    let mut seen = std::collections::HashSet::new();
    for code in ErrorCode::ALL {
        let spelling = code.as_str();
        assert!(seen.insert(spelling), "{spelling} repeats");
        assert!(
            spelling
                .split('-')
                .all(|word| !word.is_empty() && word.bytes().all(|b| b.is_ascii_lowercase())),
            "{spelling}"
        );
        // A finer code is never spelled as a kind, whose code it would shadow.
        if ErrorCode::from(code.kind()) != *code {
            assert_ne!(spelling, code.kind().as_str());
        }
    }
    assert_eq!(seen.len(), ErrorCode::ALL.len());
}

#[test]
fn the_readme_documents_every_code_with_its_kind() {
    // The table of the README's "Error codes" section lists each code and its kind, in the
    // order of `ErrorCode::ALL`.
    let readme = include_str!("../../README.md");
    let documented: Vec<(String, String)> = readme
        .lines()
        .skip_while(|line| *line != "### Error codes")
        .skip(1)
        .take_while(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
            let code = cells.first()?.strip_prefix('`')?.strip_suffix('`')?;
            let kind = cells.get(1)?.strip_prefix('`')?.strip_suffix('`')?;
            Some((code.to_owned(), kind.to_owned()))
        })
        .collect();
    let expected: Vec<(String, String)> = ErrorCode::ALL
        .iter()
        .map(|code| (code.as_str().to_owned(), format!("{:?}", code.kind())))
        .collect();
    assert_eq!(documented, expected);
}
