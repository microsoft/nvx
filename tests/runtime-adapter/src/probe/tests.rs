use super::*;

#[test]
fn a_probe_is_its_name_then_its_arguments() {
    let probe = Probe::new("hold", ["/run/a", "ä"]).unwrap();
    assert_eq!(probe.name(), "hold");
    assert_eq!(probe.arguments(), ["/run/a", "ä"]);
    assert_eq!(probe.encode(), "hold /run/a ä".as_bytes());
    assert_eq!(Probe::decode(&probe.encode()), Ok(probe.clone()));
    assert_eq!(Probe::parse("hold /run/a ä"), Ok(probe));
    let bare = Probe::parse("mounts").unwrap();
    assert_eq!((bare.name(), bare.arguments()), ("mounts", &[][..]));
    assert_eq!(bare.to_string(), "mounts");

    let longest = format!("p {}", "a".repeat(MAX_TEXT_BODY - 2));
    assert_eq!(
        Probe::parse(&longest).unwrap().encode().len(),
        MAX_TEXT_BODY
    );
    for (text, problem) in [
        ("", "name \"\" is not"),
        ("Mounts", "name \"Mounts\" is not"),
        ("mounts.all", "is not a lowercase ASCII letter, then"),
        ("-mounts", "name \"-mounts\" is not"),
        ("--mounts", "name \"--mounts\" is not"),
        ("9p", "name \"9p\" is not"),
        (" mounts", "name \"\" is not"),
        ("mounts ", "argument \"\" is empty"),
        ("mounts  a", "argument \"\" is empty"),
        ("mounts a\tb", "holds whitespace"),
        ("mounts a\u{7f}", "a control character"),
        ("mounts a\u{a0}b", "holds whitespace"),
        (&format!("{longest}a"), "more than 4096"),
    ] {
        let error = Probe::parse(text).unwrap_err();
        assert!(error.problem().contains(problem), "{text:?}: {error}");
    }
    let error = Probe::decode(b"mounts \xff").unwrap_err();
    assert_eq!(error.problem(), "the probe is not UTF-8 text");
}

#[test]
fn facts_are_one_line_each_with_escaped_values() {
    let mut facts = Facts::new();
    assert!(facts.is_empty());
    facts.push("root.type", "overlay");
    facts.push("os-release.id", "a\\b\nc\rd=e");
    facts.push("empty", "");
    assert_eq!(facts.len(), 3);
    assert_eq!(facts.get("root.type"), Some("overlay"));
    assert_eq!(facts.get("missing"), None);
    assert_eq!(
        facts.iter().collect::<Vec<_>>(),
        [
            ("root.type", "overlay"),
            ("os-release.id", "a\\b\nc\rd=e"),
            ("empty", ""),
        ]
    );
    let encoded = facts.encode();
    assert_eq!(
        encoded,
        b"root.type=overlay\nos-release.id=a\\\\b\\nc\\rd=e\nempty=\n"
    );
    assert_eq!(Facts::decode(&encoded), Ok(facts));
    assert_eq!(Facts::decode(b""), Ok(Facts::new()));

    for (body, problem) in [
        (&b"a=\xff\n"[..], "are not UTF-8 text"),
        (b"a=b", "do not end their last line"),
        (b"a=b\n\n", "\"\" has no value"),
        (b"ab\n", "\"ab\" has no value"),
        (b"A=b\n", "has an invalid key"),
        (b"=b\n", "has an invalid key"),
        (b"a b=c\n", "has an invalid key"),
        (b"a=b\na=c\n", "repeats its key"),
        (b"a=b\\\n", "has an invalid escape"),
        (b"a=b\\t\n", "has an invalid escape"),
    ] {
        let error = Facts::decode(body).unwrap_err();
        assert!(error.problem().contains(problem), "{body:?}: {error}");
    }
}

#[test]
#[should_panic(expected = "the fact a is repeated")]
fn a_probe_names_each_fact_once() {
    let mut facts = Facts::new();
    facts.push("a", "1");
    facts.push("a", "2");
}

#[test]
#[should_panic(expected = "is not valid")]
fn a_probe_names_its_facts_with_valid_keys() {
    Facts::new().push("Key", "value");
}
