//! The command-line parser of `nvx-rs`: long options that take one value or none, and
//! positionals that take exactly one value or at most one.
//!
//! - Each argument before the first `--` is classified on its own: a known option, as `--option`
//!   or `--option=VALUE`, is an option; an argument that does not start with `-`, a lone `-`, one
//!   that starts like a negative number in ASCII digits, such as `-1` or `-.5`, and one that
//!   contains a space are positionals; and any other argument that starts with `-` is an unknown
//!   option. The first `--` ends the options: it is no argument itself, and every argument after
//!   it is a positional.
//! - The arguments are consumed from left to right. An option takes its value after `=`, or else
//!   the next argument if that is a positional, so a value that starts with `-` needs the
//!   `--option=VALUE` form. The positionals of the parser take the positional arguments in order,
//!   and an optional positional that none is left for takes no value.
//! - Each value is handed to the caller as it is consumed, so conversion and choice errors
//!   surface in command-line order, as `argument NAME: MESSAGE`, and end the parse.
//! - Unknown options, and positional arguments beyond those that the parser takes, are returned
//!   for the caller to report last, as `unrecognized arguments: ...`.
//! - `-h` and `--help` stop the parse for the help as soon as they are consumed.
//!
//! Option names are never abbreviated, so `--dry` is an unknown option rather than `--dry-run`.
//!
//! [`Syntax`] holds what a parser's usage and help show. The usage is one line, which names the
//! parser's few options, or `[OPTIONS]` for many, and the help lists every option.

use std::fmt;
use std::ops::RangeInclusive;
use std::str::FromStr;

use crate::ErrorCode;
use crate::host::sandbox::parse_decimal;

/// The name of the program in usage and error messages.
const PROGRAM: &str = "nvx-rs";
/// What usage messages start with.
const USAGE_PREFIX: &str = "usage: ";

/// What the usage and help of a parser show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Syntax {
    /// The command of the parser after the program's name, such as `sandbox`, or nothing for the
    /// top-level parser.
    pub(crate) command: &'static str,
    /// What the usage shows after the parser's name: its options, such as `[-h] [--root PATH]`,
    /// or `[OPTIONS]` for a parser with many, and its positionals, such as `PATH`.
    pub(crate) arguments: &'static str,
    /// The help after the usage and its blank line: the description and the sections of the
    /// arguments, without a final line end.
    pub(crate) help: &'static str,
}

impl Syntax {
    /// Returns the parser's name, such as `nvx-rs sandbox`, which starts its usage errors.
    pub(crate) fn prog(&self) -> String {
        if self.command.is_empty() {
            PROGRAM.to_owned()
        } else {
            format!("{PROGRAM} {}", self.command)
        }
    }

    /// Returns the usage, `usage: PROG ARGUMENTS`, on one line without a line end.
    pub(crate) fn usage(&self) -> String {
        format!("{USAGE_PREFIX}{} {}", self.prog(), self.arguments)
    }

    /// Returns the help: the usage, a blank line, and the rest.
    pub(crate) fn help(&self) -> String {
        format!("{}\n\n{}", self.usage(), self.help)
    }
}

/// How a parse ends early: with a usage error, or with the help that `-h` asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stop {
    /// `PROG: error[CODE]: MESSAGE` after the parser's usage, with status 2: the code, one of the
    /// `argument-*` codes, and the message.
    Usage(ErrorCode, String),
    /// The help text, on standard output, with status 0.
    Help,
}

/// How many values an option or positional takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arity {
    /// No value: a flag.
    Flag,
    /// Exactly one value.
    One,
    /// At most one value: an optional positional.
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

    /// Consumes this option, given `attached`, the value after `=` if any, and `next`, the
    /// argument after the option if it is a positional: hands the option's value to `receiver`,
    /// and returns how many arguments the option and its value take. `-h` and `--help` stop the
    /// parse for the help, and messages call them `-h/--help`.
    fn consume(
        self,
        attached: Option<&str>,
        next: Option<&str>,
        receiver: &mut dyn Receiver,
    ) -> Result<usize, Stop> {
        let help = self.name == "-h" || self.name == "--help";
        let label = if help { "-h/--help" } else { self.name };
        let mut take = |value: Option<&str>| take(receiver, self.name, label, value);
        match (self.arity, attached) {
            (Arity::Flag, Some(value)) => Err(Stop::Usage(
                ErrorCode::ArgumentInvalid,
                format!("argument {label}: ignored explicit argument {value:?}"),
            )),
            (Arity::Flag, None) if help => Err(Stop::Help),
            (Arity::Flag, None) => take(None).map(|()| 1),
            (_, Some(value)) => take(Some(value)).map(|()| 1),
            (_, None) => match next {
                Some(value) => take(Some(value)).map(|()| 2),
                None => Err(Stop::Usage(
                    ErrorCode::ArgumentMissing,
                    format!("argument {label}: expected one argument"),
                )),
            },
        }
    }
}

/// Receives each argument as the parser consumes it, in command-line order.
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

/// Hands `value` to `receiver` for the option or positional `name`, turning its error into the
/// usage error `argument LABEL: MESSAGE`.
fn take(
    receiver: &mut dyn Receiver,
    name: &'static str,
    label: &str,
    value: Option<&str>,
) -> Result<(), Stop> {
    receiver.take(name, value).map_err(|message| {
        Stop::Usage(
            ErrorCode::ArgumentInvalid,
            format!("argument {label}: {message}"),
        )
    })
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
enum Token<'a> {
    /// A known option, with the value attached after `=`, if any.
    Option(Spec, Option<&'a str>),
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

    /// Classifies one argument before the options end, as the [module](self) describes.
    fn token<'a>(&self, argument: &'a str) -> Token<'a> {
        if argument == "--" {
            return Token::End;
        }
        if !argument.starts_with('-') || argument == "-" {
            return Token::Positional;
        }
        if let Some(spec) = self.option(argument) {
            return Token::Option(spec, None);
        }
        if let Some((name, value)) = argument.split_once('=')
            && let Some(spec) = self.option(name)
        {
            return Token::Option(spec, Some(value));
        }
        if looks_like_negative_number(argument) || argument.contains(' ') {
            return Token::Positional;
        }
        Token::Unknown
    }

    /// Returns the option whose string is `name`.
    fn option(&self, name: &str) -> Option<Spec> {
        self.options.iter().find(|spec| spec.name == name).copied()
    }

    /// Returns `argument`, the argument after an option, if the option may take it as its value:
    /// if it is a positional.
    fn value<'a>(&self, argument: Option<&'a String>) -> Option<&'a str> {
        argument
            .map(String::as_str)
            .filter(|&value| self.token(value) == Token::Positional)
    }

    /// Walks `arguments` up to the command that the parser's last positional names, for a parser
    /// whose command takes all the arguments after it: the options before the command go to
    /// `receiver`, unknown ones are collected, and the arguments from the command on are the
    /// command's. Returns the index of the command, which `--` may precede, and the unknown
    /// arguments.
    pub(crate) fn split_command(
        &self,
        arguments: &[String],
        receiver: &mut dyn Receiver,
    ) -> Result<(usize, Vec<String>), Stop> {
        let command = self.positionals.last().map_or("command", |spec| spec.name);
        let missing = || {
            Stop::Usage(
                ErrorCode::ArgumentMissing,
                format!("the following arguments are required: {command}"),
            )
        };
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
                Token::Option(spec, attached) => {
                    let next = self.value(arguments.get(index + 1));
                    index += spec.consume(attached, next, receiver)?;
                }
            }
        }
    }

    /// Parses `arguments`, handing each value to `receiver` as the [module](self) describes, and
    /// returns the arguments that the parser does not recognize, for the caller to report.
    pub(crate) fn parse(
        &self,
        arguments: &[String],
        receiver: &mut dyn Receiver,
    ) -> Result<Vec<String>, Stop> {
        let mut extras = Vec::new();
        let mut positionals = self.positionals.iter();
        let mut ended = false;
        let mut index = 0;
        while let Some(argument) = arguments.get(index) {
            let token = if ended {
                Token::Positional
            } else {
                self.token(argument)
            };
            index += match token {
                Token::End => {
                    ended = true;
                    1
                }
                Token::Option(spec, attached) => {
                    spec.consume(attached, self.value(arguments.get(index + 1)), receiver)?
                }
                Token::Unknown => {
                    extras.push(argument.clone());
                    1
                }
                Token::Positional => {
                    match positionals.next() {
                        Some(spec) => take(receiver, spec.name, spec.name, Some(argument))?,
                        None => extras.push(argument.clone()),
                    }
                    1
                }
            };
        }
        let mut missing = Vec::new();
        for spec in positionals {
            match spec.arity {
                Arity::One => missing.push(spec.name),
                Arity::Optional | Arity::Flag => take(receiver, spec.name, spec.name, None)?,
            }
        }
        if !missing.is_empty() {
            return Err(Stop::Usage(
                ErrorCode::ArgumentMissing,
                format!(
                    "the following arguments are required: {}",
                    missing.join(", ")
                ),
            ));
        }
        Ok(extras)
    }
}

/// Returns whether `argument` starts like a negative number: `-` and an ASCII digit, or `-.` and
/// an ASCII digit.
fn looks_like_negative_number(argument: &str) -> bool {
    let rest = argument.strip_prefix('-').unwrap_or(argument);
    let rest = rest.strip_prefix('.').unwrap_or(rest);
    rest.starts_with(|character: char| character.is_ascii_digit())
}

/// Returns the message for `value` that is not one of `choices`, each quoted with Rust's debug
/// formatting, such as `invalid choice: "x" (choose from "auto", "whp")`.
pub(crate) fn invalid_choice(value: &str, choices: &[&str]) -> String {
    let listed: Vec<String> = choices.iter().map(|choice| format!("{choice:?}")).collect();
    format!(
        "invalid choice: {value:?} (choose from {})",
        listed.join(", ")
    )
}

/// Returns the choice that `value` names among `choices`, which it must equal.
pub(crate) fn choice(
    value: &str,
    choices: &[&'static str],
) -> std::result::Result<&'static str, String> {
    choices
        .iter()
        .find(|choice| **choice == value)
        .copied()
        .ok_or_else(|| invalid_choice(value, choices))
}

/// Returns the integer that `value` spells in ASCII decimal digits, if it lies in `range`.
///
/// `value` holds one or more ASCII digits, leading zeros included, and nothing else: no sign,
/// whitespace, underscore, or digit of another script. Fails with `expected an ASCII decimal
/// integer between MIN and MAX, not "VALUE"`, whose bounds are those of `range`.
pub(crate) fn integer<T>(value: &str, range: RangeInclusive<T>) -> std::result::Result<T, String>
where
    T: FromStr + PartialOrd + fmt::Display,
{
    parse_decimal(value)
        .filter(|number| range.contains(number))
        .ok_or_else(|| {
            format!(
                "expected an ASCII decimal integer between {} and {}, not {value:?}",
                range.start(),
                range.end()
            )
        })
}

#[cfg(test)]
mod tests;
