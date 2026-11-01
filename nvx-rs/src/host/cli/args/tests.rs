use super::*;

impl Syntax {
    /// Asserts that the help lists `-h, --help` and each of `options`, and no other option: the
    /// options that the help describes on lines of their own, indented by two spaces.
    pub(crate) fn assert_lists(&self, options: &[Spec]) {
        let mut listed: Vec<&str> = self
            .help
            .lines()
            .filter(|line| line.starts_with("  -"))
            .filter_map(|line| line.split_whitespace().find(|word| word.starts_with("--")))
            .collect();
        let mut expected: Vec<&str> = std::iter::once("--help")
            .chain(options.iter().map(|option| option.name))
            .collect();
        listed.sort_unstable();
        expected.sort_unstable();
        assert_eq!(listed, expected, "{}", self.prog());
    }
}

/// Records what the parser hands over, refusing the value `bad`.
#[derive(Default)]
struct Recorder(Vec<(&'static str, Option<String>)>);

impl Receiver for Recorder {
    fn take(&mut self, name: &'static str, value: Option<&str>) -> Result<(), String> {
        if value == Some("bad") {
            return Err("refused \"bad\"".to_owned());
        }
        self.0.push((name, value.map(str::to_owned)));
        Ok(())
    }
}

type Events = Vec<(&'static str, Option<String>)>;

fn parse_with(parser: &Parser, arguments: &[&str]) -> (Result<Vec<String>, Stop>, Events) {
    let arguments: Vec<String> = arguments.iter().map(|&a| a.to_owned()).collect();
    let mut recorder = Recorder::default();
    let result = parser.parse(&arguments, &mut recorder);
    (result, recorder.0)
}

fn parse(arguments: &[&str]) -> (Result<Vec<String>, Stop>, Events) {
    let parser = Parser::new(
        &[Spec::one("--layer"), Spec::flag("--dry-run")],
        &[Spec::optional("operation")],
    );
    parse_with(&parser, arguments)
}

fn taken(pairs: &[(&'static str, Option<&str>)]) -> Events {
    pairs
        .iter()
        .map(|&(name, value)| (name, value.map(str::to_owned)))
        .collect()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|&value| value.to_owned()).collect()
}

fn usage(code: ErrorCode, message: &str) -> Result<Vec<String>, Stop> {
    Err(Stop::Usage(code, message.to_owned()))
}

#[test]
fn options_and_positionals_are_consumed_in_order() {
    let (result, events) = parse(&["--layer", "a", "start", "--dry-run", "--layer=-b"]);
    assert_eq!(result, Ok(Vec::new()));
    assert_eq!(
        events,
        taken(&[
            ("--layer", Some("a")),
            ("operation", Some("start")),
            ("--dry-run", None),
            ("--layer", Some("-b")),
        ])
    );
    let (result, events) = parse(&["--dry-run"]);
    assert_eq!(result, Ok(Vec::new()));
    assert_eq!(events, taken(&[("--dry-run", None), ("operation", None)]));
    let (_, events) = parse(&["--layer=a=b", "--layer="]);
    assert_eq!(
        events[..2],
        taken(&[("--layer", Some("a=b")), ("--layer", Some(""))])
    );
}

#[test]
fn values_that_look_like_options_need_the_attached_form() {
    let expected = usage(
        ErrorCode::ArgumentMissing,
        "argument --layer: expected one argument",
    );
    assert_eq!(parse(&["--layer", "-c"]).0, expected);
    assert_eq!(parse(&["--layer", "--", "x"]).0, expected);
    assert_eq!(parse(&["--layer"]).0, expected);
    assert_eq!(parse(&["--layer", "--dry-run"]).0, expected);
    for value in ["-1abc", "-.5x", "-", "-a b", ""] {
        let (result, events) = parse(&["--layer", value]);
        assert_eq!(result, Ok(Vec::new()), "{value:?}");
        assert_eq!(events[0], ("--layer", Some(value.to_owned())));
    }
    // Only ASCII digits start a negative number: the digits of other scripts, superscripts,
    // and Roman numerals do not, so such a value needs the attached form.
    for value in [
        "-\u{661}",
        "-.\u{665}",
        "-\u{ff15}0",
        "-\u{b2}",
        "-\u{216b}",
        "-.x",
    ] {
        assert_eq!(parse(&["--layer", value]).0, expected, "{value:?}");
        let (result, events) = parse(&[&format!("--layer={value}")]);
        assert_eq!(result, Ok(Vec::new()), "{value:?}");
        assert_eq!(events[0], ("--layer", Some(value.to_owned())));
    }
}

#[test]
fn unknown_arguments_are_returned_for_the_caller() {
    let (result, events) = parse(&["--bogus", "run", "extra", "-x"]);
    assert_eq!(result, Ok(strings(&["--bogus", "extra", "-x"])));
    assert_eq!(events, taken(&[("operation", Some("run"))]));
    // A prefix of an option is unknown: options are not abbreviated.
    assert_eq!(parse(&["--dry"]).0, Ok(strings(&["--dry"])));
    let (result, events) = parse(&["run", "start", "--dry-run"]);
    assert_eq!(result, Ok(strings(&["start"])));
    assert_eq!(
        events,
        taken(&[("operation", Some("run")), ("--dry-run", None)])
    );
}

#[test]
fn errors_end_the_parse_where_they_occur() {
    let invalid = |message| usage(ErrorCode::ArgumentInvalid, message);
    let (result, events) = parse(&["--layer", "a", "--layer", "bad", "--layer", "c"]);
    assert_eq!(result, invalid("argument --layer: refused \"bad\""));
    assert_eq!(events, taken(&[("--layer", Some("a"))]));
    assert_eq!(
        parse(&["--dry-run=1"]).0,
        invalid("argument --dry-run: ignored explicit argument \"1\"")
    );
    assert_eq!(
        parse(&["--help=x"]).0,
        invalid("argument -h/--help: ignored explicit argument \"x\"")
    );
    assert_eq!(
        parse(&["--layer", "a", "-h", "--layer", "bad"]).0,
        Err(Stop::Help)
    );
    assert_eq!(
        parse(&["--layer", "bad", "--help"]).0,
        invalid("argument --layer: refused \"bad\"")
    );
    // An unknown option before a positional leaves the positional to its parser.
    let (result, events) = parse(&["--bogus", "bad"]);
    assert_eq!(result, invalid("argument operation: refused \"bad\""));
    assert!(events.is_empty());
}

#[test]
fn double_dashes_end_the_options() {
    let (result, events) = parse(&["--", "--layer"]);
    assert_eq!(result, Ok(Vec::new()));
    assert_eq!(events, taken(&[("operation", Some("--layer"))]));
    let (result, events) = parse(&["run", "--", "start"]);
    assert_eq!(result, Ok(strings(&["start"])));
    assert_eq!(events, taken(&[("operation", Some("run"))]));
    let (result, events) = parse(&["--layer=1", "--", "a", "--", "b"]);
    assert_eq!(result, Ok(strings(&["--", "b"])));
    assert_eq!(
        events,
        taken(&[("--layer", Some("1")), ("operation", Some("a"))])
    );
    let (result, events) = parse(&["--dry-run", "--"]);
    assert_eq!(result, Ok(Vec::new()));
    assert_eq!(events, taken(&[("--dry-run", None), ("operation", None)]));
    // The first `--` is no argument itself, wherever it is.
    let (result, _) = parse(&["run", "--dry-run", "--", "start"]);
    assert_eq!(result, Ok(strings(&["start"])));
}

#[test]
fn required_positionals_must_be_given() {
    let parser = Parser::new(&[], &[Spec::one("PATH")]);
    let missing = usage(
        ErrorCode::ArgumentMissing,
        "the following arguments are required: PATH",
    );
    assert_eq!(parse_with(&parser, &[]).0, missing);
    let (result, events) = parse_with(&parser, &["policy.json", "more"]);
    assert_eq!(result, Ok(strings(&["more"])));
    assert_eq!(events, taken(&[("PATH", Some("policy.json"))]));
    assert_eq!(parse_with(&parser, &["-x"]).0, missing);
}

#[test]
fn commands_split_after_the_options_before_them() {
    let parser = Parser::new(&[Spec::one("--root")], &[Spec::one("command")]);
    let split = |arguments: &[&str]| {
        let mut recorder = Recorder::default();
        let result = parser.split_command(&strings(arguments), &mut recorder);
        (result, recorder.0)
    };
    let (result, events) = split(&["--root", "/r", "--x", "sandbox", "--root", "y"]);
    assert_eq!(result, Ok((3, strings(&["--x"]))));
    assert_eq!(events, taken(&[("--root", Some("/r"))]));
    assert_eq!(split(&["--root=-r", "--", "-x"]).0, Ok((2, Vec::new())));
    let stop = |code, message: &str| Err(Stop::Usage(code, message.to_owned()));
    let expected_one = stop(
        ErrorCode::ArgumentMissing,
        "argument --root: expected one argument",
    );
    assert_eq!(split(&["--root", "-x", "sandbox"]).0, expected_one);
    assert_eq!(split(&["--root"]).0, expected_one);
    assert_eq!(
        split(&["--help=x", "sandbox"]).0,
        stop(
            ErrorCode::ArgumentInvalid,
            "argument -h/--help: ignored explicit argument \"x\""
        )
    );
    let required = stop(
        ErrorCode::ArgumentMissing,
        "the following arguments are required: command",
    );
    assert_eq!(split(&["--root", "/r"]).0, required);
    assert_eq!(split(&["--"]).0, required);
    assert_eq!(split(&["-h", "sandbox"]).0, Err(Stop::Help));
    assert_eq!(
        split(&["--root", "bad"]).0,
        stop(
            ErrorCode::ArgumentInvalid,
            "argument --root: refused \"bad\""
        )
    );
}

#[test]
fn choices_are_quoted_with_rust_debug_formatting() {
    assert_eq!(
        invalid_choice("x", &["auto", "whp"]),
        "invalid choice: \"x\" (choose from \"auto\", \"whp\")"
    );
    assert_eq!(choice("whp", &["auto", "whp"]), Ok("whp"));
    assert!(choice("WHP", &["auto", "whp"]).is_err());
}

#[test]
fn integers_are_ascii_decimal_digits_in_their_range() {
    assert_eq!(integer::<u32>("8", 1..=8), Ok(8));
    assert_eq!(integer::<u32>("0008", 1..=8), Ok(8));
    assert_eq!(integer::<u64>("0", 0..=u64::MAX), Ok(0));
    assert_eq!(
        integer::<u64>("18446744073709551615", 1..=u64::MAX),
        Ok(u64::MAX)
    );
    for value in [
        "",
        "9",
        "0",
        "+1",
        "-1",
        " 1",
        "1 ",
        "1_0",
        "1.0",
        "1e1",
        "0x1",
        "\u{661}",
        "\u{ff11}",
        "99999999999999999999",
    ] {
        assert_eq!(
            integer::<u32>(value, 1..=8),
            Err(format!(
                "expected an ASCII decimal integer between 1 and 8, not {value:?}"
            )),
            "{value:?}"
        );
    }
}

#[test]
fn usages_name_the_parser_on_one_line() {
    let compile = Syntax {
        command: "egress compile",
        arguments: "[-h] PATH",
        help: "options:\n  -h, --help  show this help message and exit",
    };
    assert_eq!(compile.prog(), "nvx-rs egress compile");
    assert_eq!(compile.usage(), "usage: nvx-rs egress compile [-h] PATH");
    assert_eq!(
        compile.help(),
        "usage: nvx-rs egress compile [-h] PATH\n\noptions:\n  -h, --help  show this help \
             message and exit"
    );
    compile.assert_lists(&[]);
    let top = Syntax {
        command: "",
        arguments: "[OPTIONS]",
        ..compile
    };
    assert_eq!(top.prog(), "nvx-rs");
    assert_eq!(top.usage(), "usage: nvx-rs [OPTIONS]");
}
