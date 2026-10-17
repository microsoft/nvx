//! A command-line parser with the semantics of Python's `argparse` for the parsers of `nvx.py`
//! that `nvx-rs` replaces: long options that take one value or none, and positionals that take
//! exactly one value or at most one.
//!
//! It follows `ArgumentParser.parse_args` step by step:
//!
//! - Each argument is classified as `argparse` classifies it: the first `--` ends the options,
//!   and every argument after it is a positional; a known option, or one with `=VALUE` attached,
//!   is an option; an argument that does not start with `-`, a lone `-`, an argument that starts
//!   like a negative number, such as `-1` or `-.5`, and one that contains a space are
//!   positionals; and any other argument that starts with `-` is an unknown option.
//! - Options and positionals are consumed in `argparse`'s alternation: the positionals before
//!   each option, then the option with its value, which must be the next argument and a
//!   positional by that classification, or the value after `=`. A value that starts with `-`
//!   therefore needs the `--option=VALUE` form. A positional takes the `--` next to its value,
//!   and an optional positional that would take nothing before an option waits for the
//!   arguments after it.
//! - Each value is handed to the caller as it is consumed, so conversion and choice errors
//!   surface in command-line order, as `argument NAME: MESSAGE`, and end the parse.
//! - Unknown options and positionals beyond those that the parser takes are returned for the
//!   caller to report last, as `unrecognized arguments: ...`, since `argparse` reports them from
//!   the top-level parser.
//! - `-h` and `--help` stop the parse for the help as soon as they are consumed.
//!
//! Unlike `argparse`, it does not expand unambiguous prefixes of option names: `nvx.py` passes
//! canonical options, so `--dry` is an unknown option rather than `--dry-run`.
//!
//! [`Syntax`] holds what a parser's usage and help show, and formats its usage for any program
//! name as `argparse` wraps it for 80 columns.

use crate::host::python::builtins::repr;

/// The width of usage and help: 80 columns, less the margin of 2 that `argparse` keeps, as for a
/// pipe, whose `shutil.get_terminal_size()` is 80 columns.
const WIDTH: usize = 78;
/// What usage messages start with.
const USAGE_PREFIX: &str = "usage: ";

/// What the usage and help of a parser show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Syntax {
    /// The command of the parser after the program's name, such as `sandbox`, or nothing for the
    /// top-level parser.
    pub(crate) command: &'static str,
    /// The usage parts of the options, such as `[-h]` and `[--root PATH]`, in order.
    pub(crate) optionals: &'static [&'static str],
    /// The usage parts of the positionals, such as `PATH`, in order.
    pub(crate) positionals: &'static [&'static str],
    /// The help after the usage and its blank line: the description and the sections of the
    /// arguments, without a final line end.
    pub(crate) help: &'static str,
}

impl Syntax {
    /// Returns the parser's name for the program `program`, such as `nvx-rs sandbox`: what
    /// `argparse` calls the `prog` of a subparser.
    pub(crate) fn prog(&self, program: &str) -> String {
        if self.command.is_empty() {
            program.to_owned()
        } else {
            format!("{program} {}", self.command)
        }
    }

    /// Returns the usage for the program `program`, `usage: PROG ...`, without a line end.
    pub(crate) fn usage(&self, program: &str) -> String {
        format_usage(&self.prog(program), self.optionals, self.positionals)
    }

    /// Returns the help for the program `program`: the usage, a blank line, and the rest.
    pub(crate) fn help(&self, program: &str) -> String {
        format!("{}\n\n{}", self.usage(program), self.help)
    }
}

/// Returns the usage of the parser `prog` with the usage parts `optionals` and `positionals`, as
/// `HelpFormatter._format_usage` of CPython 3.14 formats it for [`WIDTH`]: on one line if it
/// fits, and otherwise wrapped at the parts, after the name if the name is short and on the lines
/// after it if not.
fn format_usage(prog: &str, optionals: &[&str], positionals: &[&str]) -> String {
    let length = |text: &str| text.chars().count();
    let parts: Vec<&str> = optionals.iter().chain(positionals).copied().collect();
    let full = std::iter::once(prog)
        .chain(parts.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    if USAGE_PREFIX.len() + length(&full) <= WIDTH {
        return format!("{USAGE_PREFIX}{full}");
    }
    // Wraps `parts` into lines after `indent`, the first of which continues the prefix if given.
    let lines_of = |parts: &[&str], indent: &str, prefix: Option<&str>| -> Vec<String> {
        let mut lines = Vec::new();
        let mut line: Vec<&str> = Vec::new();
        let indent_length = length(indent);
        let mut line_length = prefix.map_or(indent_length, length) - 1;
        for part in parts {
            if line_length + 1 + length(part) > WIDTH && !line.is_empty() {
                lines.push(format!("{indent}{}", line.join(" ")));
                line.clear();
                line_length = indent_length - 1;
            }
            line.push(part);
            line_length += length(part) + 1;
        }
        if !line.is_empty() {
            lines.push(format!("{indent}{}", line.join(" ")));
        }
        if prefix.is_some()
            && let Some(first) = lines.first_mut()
        {
            *first = first[indent.len()..].to_owned();
        }
        lines
    };
    let prog_length = length(prog);
    // A name of at most three quarters of the width is followed by the parts.
    let lines = if 4 * (USAGE_PREFIX.len() + prog_length) <= 3 * WIDTH {
        let indent = " ".repeat(USAGE_PREFIX.len() + prog_length + 1);
        if optionals.is_empty() {
            let leading: Vec<&str> = std::iter::once(prog)
                .chain(positionals.iter().copied())
                .collect();
            lines_of(&leading, &indent, Some(USAGE_PREFIX))
        } else {
            let leading: Vec<&str> = std::iter::once(prog)
                .chain(optionals.iter().copied())
                .collect();
            let mut lines = lines_of(&leading, &indent, Some(USAGE_PREFIX));
            lines.extend(lines_of(positionals, &indent, None));
            lines
        }
    } else {
        let indent = " ".repeat(USAGE_PREFIX.len());
        let mut lines = lines_of(&parts, &indent, None);
        if lines.len() > 1 {
            lines = lines_of(optionals, &indent, None);
            lines.extend(lines_of(positionals, &indent, None));
        }
        lines.insert(0, prog.to_owned());
        lines
    };
    format!("{USAGE_PREFIX}{}", lines.join("\n"))
}

/// How a parse ends early: with a usage error, or with the help that `-h` asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stop {
    /// `PROG: error: MESSAGE` after the parser's usage, with status 2.
    Usage(String),
    /// The help text, on standard output, with status 0.
    Help,
}

/// How many values an option or positional takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arity {
    /// No value: a flag, such as `store_true`.
    Flag,
    /// Exactly one value.
    One,
    /// At most one value: an optional positional, `nargs='?'`.
    Optional,
}

/// An argument that the parser recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Spec {
    /// The option string, such as `--layer`, or the name with which messages refer to a
    /// positional, such as `sandbox_operation`.
    pub(crate) name: &'static str,
    /// How many values it takes.
    pub(crate) arity: Arity,
}

impl Spec {
    /// An option that takes no value.
    pub(crate) const fn flag(name: &'static str) -> Self {
        Self {
            name,
            arity: Arity::Flag,
        }
    }

    /// An option or positional that takes exactly one value.
    pub(crate) const fn one(name: &'static str) -> Self {
        Self {
            name,
            arity: Arity::One,
        }
    }

    /// A positional that takes at most one value.
    pub(crate) const fn optional(name: &'static str) -> Self {
        Self {
            name,
            arity: Arity::Optional,
        }
    }
}

/// Receives each argument as the parser consumes it, in `argparse`'s order.
pub(crate) trait Receiver {
    /// Takes `value` for the option or positional `name`: `None` for a flag, and for an optional
    /// positional that received no value, which then takes its default. An error message ends
    /// the parse as `argument NAME: MESSAGE`.
    fn take(&mut self, name: &'static str, value: Option<&str>) -> Result<(), String>;
}

impl<F: FnMut(&'static str, Option<&str>) -> Result<(), String>> Receiver for F {
    fn take(&mut self, name: &'static str, value: Option<&str>) -> Result<(), String> {
        self(name, value)
    }
}

/// The classification of one argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An option that the parser knows, with the byte offset of the value attached after `=`,
    /// if any.
    Option(usize, Option<usize>),
    /// An option-like argument that the parser does not know.
    Unknown,
    /// A positional value.
    Positional,
    /// The `--` that ends the options.
    End,
}

/// A parser of options and positionals.
pub(crate) struct Parser {
    /// The options, such as `--layer`, and `-h` and `--help`, which every parser has.
    options: Vec<Spec>,
    /// The positionals, in order.
    positionals: Vec<Spec>,
}

/// What an argument is to a parser, as [`Parser::token`] classifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Token<'a> {
    /// A known option, with the value attached after `=`, if any.
    Option(&'static str, Option<&'a str>),
    /// An option-like argument that the parser does not know.
    Unknown,
    /// A positional value.
    Positional,
    /// The `--` that ends the options.
    End,
}

impl Parser {
    /// Returns the parser of `options` and `positionals`, which also knows `-h` and `--help`.
    pub(crate) fn new(options: &[Spec], positionals: &[Spec]) -> Self {
        let mut all = vec![Spec::flag("-h"), Spec::flag("--help")];
        all.extend_from_slice(options);
        Self {
            options: all,
            positionals: positionals.to_vec(),
        }
    }

    /// Classifies one argument before the options end, for a caller that walks the arguments of
    /// a parser whose last positional takes all the arguments after it, such as a command.
    pub(crate) fn token<'a>(&self, argument: &'a str) -> Token<'a> {
        if argument == "--" {
            return Token::End;
        }
        match self.classify(argument) {
            Kind::Option(option, attached) => Token::Option(
                self.options[option].name,
                attached.map(|offset| &argument[offset..]),
            ),
            Kind::Unknown => Token::Unknown,
            Kind::Positional | Kind::End => Token::Positional,
        }
    }

    /// Returns the option whose string is `name`.
    fn option(&self, name: &str) -> Option<usize> {
        self.options.iter().position(|spec| spec.name == name)
    }

    /// Classifies `argument` as `ArgumentParser._parse_optional` does, without abbreviations.
    fn classify(&self, argument: &str) -> Kind {
        if !argument.starts_with('-') {
            return Kind::Positional;
        }
        if let Some(option) = self.option(argument) {
            return Kind::Option(option, None);
        }
        if argument.len() == 1 {
            return Kind::Positional;
        }
        if let Some((name, _)) = argument.split_once('=')
            && let Some(option) = self.option(name)
        {
            return Kind::Option(option, Some(name.len() + 1));
        }
        if looks_like_negative_number(argument) || argument.contains(' ') {
            return Kind::Positional;
        }
        Kind::Unknown
    }

    /// Walks `arguments` up to the command that the parser's last positional names, as
    /// `argparse` walks the arguments of a parser with subparsers: the options before the
    /// command go to `receiver`, unknown ones are collected, and the arguments from the command
    /// on are the command's. Returns the index of the command, which `--` may precede, and the
    /// unknown arguments.
    pub(crate) fn split_command(
        &self,
        arguments: &[String],
        receiver: &mut dyn Receiver,
    ) -> Result<(usize, Vec<String>), Stop> {
        let command = self.positionals.last().map_or("command", |spec| spec.name);
        let missing = || Stop::Usage(format!("the following arguments are required: {command}"));
        let mut extras = Vec::new();
        let mut index = 0;
        loop {
            let argument = arguments.get(index).ok_or_else(missing)?;
            match self.token(argument) {
                Token::End => {
                    return if index + 1 < arguments.len() {
                        Ok((index + 1, extras))
                    } else {
                        Err(missing())
                    };
                }
                Token::Positional => return Ok((index, extras)),
                Token::Unknown => {
                    extras.push(argument.clone());
                    index += 1;
                }
                Token::Option(name, attached) => {
                    let spec = self.options[self.option(name).unwrap_or_default()];
                    let help = name == "-h" || name == "--help";
                    let label = if help { "-h/--help" } else { name };
                    let take = |receiver: &mut dyn Receiver, value: Option<&str>| {
                        receiver
                            .take(name, value)
                            .map_err(|message| Stop::Usage(format!("argument {label}: {message}")))
                    };
                    match (spec.arity, attached) {
                        (Arity::Flag, Some(value)) => {
                            return Err(Stop::Usage(format!(
                                "argument {label}: ignored explicit argument {}",
                                repr(value)
                            )));
                        }
                        (Arity::Flag, None) if help => return Err(Stop::Help),
                        (Arity::Flag, None) => {
                            take(receiver, None)?;
                            index += 1;
                        }
                        (_, Some(value)) => {
                            take(receiver, Some(value))?;
                            index += 1;
                        }
                        (_, None) => match arguments.get(index + 1) {
                            Some(value) if self.token(value) == Token::Positional => {
                                take(receiver, Some(value))?;
                                index += 2;
                            }
                            _ => {
                                return Err(Stop::Usage(format!(
                                    "argument {label}: expected one argument"
                                )));
                            }
                        },
                    }
                }
            }
        }
    }

    /// Parses `arguments`, handing each value to `receiver`, and returns the arguments that the
    /// parser does not recognize, for the caller to report.
    pub(crate) fn parse(
        &self,
        arguments: &[String],
        receiver: &mut dyn Receiver,
    ) -> Result<Vec<String>, Stop> {
        let mut kinds = Vec::with_capacity(arguments.len());
        let mut ended = false;
        for argument in arguments {
            kinds.push(if ended {
                Kind::Positional
            } else if argument == "--" {
                ended = true;
                Kind::End
            } else {
                self.classify(argument)
            });
        }
        let mut state = State {
            parser: self,
            arguments,
            kinds: &kinds,
            positionals: 0,
            extras: Vec::new(),
            receiver,
        };
        let options: Vec<usize> = (0..kinds.len())
            .filter(|&index| matches!(kinds[index], Kind::Option(..) | Kind::Unknown))
            .collect();
        let mut start = 0;
        while let Some(&next) = options.iter().find(|&&index| index >= start) {
            if start != next {
                let end = state.consume_positionals(start)?;
                if end > start {
                    start = end;
                    continue;
                }
                state.extras.extend_from_slice(&arguments[start..next]);
                start = next;
            }
            start = state.consume_option(start)?;
        }
        let stop = state.consume_positionals(start)?;
        state.extras.extend_from_slice(&arguments[stop..]);
        let missing: Vec<&str> = self.positionals[state.positionals..]
            .iter()
            .filter(|spec| spec.arity == Arity::One)
            .map(|spec| spec.name)
            .collect();
        if !missing.is_empty() {
            return Err(Stop::Usage(format!(
                "the following arguments are required: {}",
                missing.join(", ")
            )));
        }
        Ok(state.extras)
    }
}

/// Returns whether `argument` starts like a negative number, as `argparse`'s
/// `_negative_number_matcher`, `-\.?\d`, finds it.
fn looks_like_negative_number(argument: &str) -> bool {
    let rest = &argument.as_bytes()[1..];
    let rest = rest.strip_prefix(b".").unwrap_or(rest);
    rest.first().is_some_and(u8::is_ascii_digit)
}

/// The progress of one parse.
struct State<'a> {
    /// The parser.
    parser: &'a Parser,
    /// The arguments.
    arguments: &'a [String],
    /// Their classifications.
    kinds: &'a [Kind],
    /// How many positionals have been consumed.
    positionals: usize,
    /// The arguments that the parser does not recognize.
    extras: Vec<String>,
    /// The receiver of the values.
    receiver: &'a mut dyn Receiver,
}

impl State<'_> {
    /// Hands `value` to the receiver, turning its error into a usage error for `name`.
    fn take(&mut self, name: &'static str, value: Option<&str>) -> Result<(), Stop> {
        self.receiver
            .take(name, value)
            .map_err(|message| Stop::Usage(format!("argument {name}: {message}")))
    }

    /// Consumes the option at `start` and its value, as `consume_optional` does, and returns the
    /// index after them.
    fn consume_option(&mut self, start: usize) -> Result<usize, Stop> {
        let Kind::Option(option, attached) = self.kinds[start] else {
            self.extras.push(self.arguments[start].clone());
            return Ok(start + 1);
        };
        let spec = self.parser.options[option];
        let attached = attached.map(|offset| &self.arguments[start][offset..]);
        let help = spec.name == "-h" || spec.name == "--help";
        let name = if help { "-h/--help" } else { spec.name };
        match (spec.arity, attached) {
            (Arity::Flag, Some(value)) => Err(Stop::Usage(format!(
                "argument {name}: ignored explicit argument {}",
                repr(value)
            ))),
            (Arity::Flag, None) if help => Err(Stop::Help),
            (Arity::Flag, None) => {
                self.take(spec.name, None)?;
                Ok(start + 1)
            }
            (_, Some(value)) => {
                self.take(spec.name, Some(value))?;
                Ok(start + 1)
            }
            (_, None) => match self.kinds.get(start + 1) {
                Some(Kind::Positional) => {
                    self.take(spec.name, Some(&self.arguments[start + 1]))?;
                    Ok(start + 2)
                }
                _ => Err(Stop::Usage(format!(
                    "argument {name}: expected one argument"
                ))),
            },
        }
    }

    /// Consumes as many positionals as the arguments from `start` provide, as
    /// `consume_positionals` does, and returns the index after them.
    fn consume_positionals(&mut self, mut start: usize) -> Result<usize, Stop> {
        while let Some(&spec) = self.parser.positionals.get(self.positionals) {
            // A positional takes the `--` before and after its value, which `argparse` drops.
            let mut end = start;
            if self.kinds.get(end) == Some(&Kind::End) {
                end += 1;
            }
            let value = match self.kinds.get(end) {
                Some(Kind::Positional) => {
                    end += 1;
                    Some(self.arguments[end - 1].as_str())
                }
                _ if spec.arity == Arity::One => return Ok(start),
                _ => None,
            };
            if self.kinds.get(end) == Some(&Kind::End) {
                end += 1;
            }
            // An optional positional that would take nothing before an option waits for the
            // arguments after the option.
            if end == start && matches!(self.kinds.get(end), Some(Kind::Option(..) | Kind::Unknown))
            {
                return Ok(start);
            }
            self.positionals += 1;
            self.take(spec.name, value)?;
            start = end;
        }
        Ok(start)
    }
}

/// Returns `argparse`'s message for `value` that is not one of `choices`.
pub(crate) fn invalid_choice(value: &str, choices: &[&str]) -> String {
    let listed: Vec<String> = choices.iter().map(|choice| repr(choice)).collect();
    format!(
        "invalid choice: {} (choose from {})",
        repr(value),
        listed.join(", ")
    )
}

/// Returns `argparse`'s message for a value that a `type` function refused with `ValueError`,
/// such as `invalid int value: 'x'`.
pub(crate) fn invalid_value(type_name: &str, value: &str) -> String {
    format!("invalid {type_name} value: {}", repr(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what the parser hands over, refusing the value `bad`.
    #[derive(Default)]
    struct Recorder(Vec<(&'static str, Option<String>)>);

    impl Receiver for Recorder {
        fn take(&mut self, name: &'static str, value: Option<&str>) -> Result<(), String> {
            if value == Some("bad") {
                return Err(invalid_value("int", "bad"));
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

    fn usage(message: &str) -> Result<Vec<String>, Stop> {
        Err(Stop::Usage(message.to_owned()))
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
        let expected = usage("argument --layer: expected one argument");
        assert_eq!(parse(&["--layer", "-c"]).0, expected);
        assert_eq!(parse(&["--layer", "--", "x"]).0, expected);
        assert_eq!(parse(&["--layer"]).0, expected);
        assert_eq!(parse(&["--layer", "--dry-run"]).0, expected);
        for value in ["-1abc", "-.5x", "-", "-a b", ""] {
            let (result, events) = parse(&["--layer", value]);
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
        let (result, events) = parse(&["--layer", "a", "--layer", "bad", "--layer", "c"]);
        assert_eq!(result, usage("argument --layer: invalid int value: 'bad'"));
        assert_eq!(events, taken(&[("--layer", Some("a"))]));
        assert_eq!(
            parse(&["--dry-run=1"]).0,
            usage("argument --dry-run: ignored explicit argument '1'")
        );
        assert_eq!(
            parse(&["--help=x"]).0,
            usage("argument -h/--help: ignored explicit argument 'x'")
        );
        assert_eq!(
            parse(&["--layer", "a", "-h", "--layer", "bad"]).0,
            Err(Stop::Help)
        );
        assert_eq!(
            parse(&["--layer", "bad", "--help"]).0,
            usage("argument --layer: invalid int value: 'bad'")
        );
        // An unknown option before a positional leaves the positional to its parser.
        let (result, events) = parse(&["--bogus", "bad"]);
        assert_eq!(
            result,
            usage("argument operation: invalid int value: 'bad'")
        );
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
        let (result, _) = parse(&["run", "--dry-run", "--", "start"]);
        assert_eq!(result, Ok(strings(&["--", "start"])));
    }

    #[test]
    fn required_positionals_must_be_given() {
        let parser = Parser::new(&[], &[Spec::one("PATH")]);
        assert_eq!(
            parse_with(&parser, &[]).0,
            usage("the following arguments are required: PATH")
        );
        let (result, events) = parse_with(&parser, &["policy.json", "more"]);
        assert_eq!(result, Ok(strings(&["more"])));
        assert_eq!(events, taken(&[("PATH", Some("policy.json"))]));
        assert_eq!(
            parse_with(&parser, &["-x"]).0,
            usage("the following arguments are required: PATH")
        );
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
        assert_eq!(
            split(&["--root", "-x", "sandbox"]).0,
            Err(Stop::Usage(
                "argument --root: expected one argument".to_owned()
            ))
        );
        let required = Err(Stop::Usage(
            "the following arguments are required: command".to_owned(),
        ));
        assert_eq!(split(&["--root", "/r"]).0, required);
        assert_eq!(split(&["--"]).0, required);
        assert_eq!(split(&["-h", "sandbox"]).0, Err(Stop::Help));
        assert_eq!(
            split(&["--root", "bad"]).0,
            Err(Stop::Usage(
                "argument --root: invalid int value: 'bad'".to_owned()
            ))
        );
    }

    #[test]
    fn choices_and_values_are_described_as_argparse_describes_them() {
        assert_eq!(
            invalid_choice("x", &["auto", "whp"]),
            "invalid choice: 'x' (choose from 'auto', 'whp')"
        );
        assert_eq!(
            invalid_value("positive_float", "x'y"),
            "invalid positive_float value: \"x'y\""
        );
    }

    // The expected usages are what CPython 3.14's argparse prints for 80 columns.
    #[test]
    fn usages_wrap_as_argparse_wraps_them() {
        const OPTIONALS: &[&str] = &[
            "[-h]",
            "[--layer ROLE,PATH,EROFS_UUID]",
            "[--scratch SCRATCH]",
            "[--state-dir STATE_DIR]",
            "[--entrypoint ENTRYPOINT]",
            "[--network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]]",
        ];
        const POSITIONALS: &[&str] = &["[{run,provision}]"];
        let long = "a-program-whose-name-is-long-enough-to-take-its-own-line.py";
        let syntax = |command, optionals, positionals| Syntax {
            command,
            optionals,
            positionals,
            help: "",
        };
        // A usage that fits stays on one line.
        assert_eq!(
            syntax("egress compile", &["[-h]"], &["PATH"]).usage("nvx-rs"),
            "usage: nvx-rs egress compile [-h] PATH"
        );
        // A short name is followed by the options, and the positionals start a line of their own.
        assert_eq!(
            syntax("", OPTIONALS, POSITIONALS).usage("tool"),
            "usage: tool [-h] [--layer ROLE,PATH,EROFS_UUID] [--scratch SCRATCH]\n            \
             [--state-dir STATE_DIR] [--entrypoint ENTRYPOINT]\n            \
             [--network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]]\n            \
             [{run,provision}]"
        );
        // A long name takes a line of its own, and so do the options and the positionals if all
        // of them do not fit on one line.
        assert_eq!(
            syntax("", OPTIONALS, POSITIONALS).usage(long),
            format!(
                "usage: {long}\n       [-h] [--layer ROLE,PATH,EROFS_UUID] [--scratch SCRATCH]\n       \
                 [--state-dir STATE_DIR] [--entrypoint ENTRYPOINT]\n       \
                 [--network-egress-allow CIDR[:PROTOCOL[:PORT[-PORT]]]]\n       \
                 [{{run,provision}}]"
            )
        );
        assert_eq!(
            syntax("egress compile", &["[-h]"], &["PATH"]).usage(long),
            format!("usage: {long} egress compile\n       [-h] PATH")
        );
        assert_eq!(
            syntax("", &["[-h]", "[--root PATH]"], &["path"]).usage(long),
            format!("usage: {long}\n       [-h] [--root PATH] path")
        );
        let help = Syntax {
            help: "options:\n  -h, --help  show this help message and exit",
            ..syntax("artifacts", &["[-h]"], &[])
        };
        assert_eq!(help.prog("nvx.py"), "nvx.py artifacts");
        assert_eq!(
            help.help("nvx.py"),
            "usage: nvx.py artifacts [-h]\n\noptions:\n  -h, --help  show this help message and exit"
        );
    }
}
