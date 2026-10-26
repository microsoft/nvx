//! Keeps the production dependency graph between the children of `nvx::host` acyclic.
//!
//! The children of `host` are the modules that `src/host/mod.rs` declares outside test-only code.
//! A child references another when its production code names a path into the other: a `crate::`,
//! `super::`, or `self::` path, or one from a name that `extern crate self as NAME;` gives the
//! crate root, in a `use` tree, an expression, an attribute, a macro's input, or a string argument
//! of a `serde` attribute, such as that of `with`, or a path through a name that a module outside
//! host's children imports, by name or with a glob, such as `crate::Artifacts`. Such a name
//! stands for everything that its imports name, under any configuration and through other such
//! names, and a child that imports one of those modules, by name or with a glob, references every
//! child that the module's names and modules stand for. Comments, literals outside the input of
//! macros, and code that only `cfg(test)` builds are not references, so intra-doc links and tests
//! may name any child. The references must form no cycle, so that the children stack in layers,
//! each of which references only those below it. The test prints the layers, lowest first.
//!
//! The test reads the crate's sources with the standard library alone, following the module
//! declarations from `src/lib.rs`. It tokenizes them, skips comments and test-only code, and
//! resolves each path from the module, inline or not, in which it appears. Test-only code is the
//! item, statement, field, variant, match arm, or parameter that a `cfg(test)` attribute marks, and
//! the rest of the module or item whose body an inner one opens. Where the test cannot tell where
//! test-only code ends, as when a `<` may open type arguments or compare, it may count test-only
//! code, but it never skips production code. It expands no macros: the input of a macro counts as
//! production code, whatever its `cfg` attributes, whose strings may hold paths and whose
//! declarations the test leaves to the macro where it cannot read them, and the template of a
//! `macro_rules!` names only what is in scope where it expands, which the test reads there. The
//! test fails on what it cannot follow, rather than miss the references that pass through it:
//! `#[path]` attributes, conditional or not; macro templates that declare or import, that name or
//! may join a path from a module, a metavariable, or a repetition, or hold one in a string, that
//! expand a token tree, or that complete a `serde` attribute with a metavariable; glob imports of
//! host's children; and imports of the crate root or of `host` itself, under any name, in host's
//! children.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::ops::Range;
use std::panic;
use std::path::Path;
use std::slice;

/// The children of `host` that each child references, with the first reference to each, by file
/// and line.
type Graph = BTreeMap<String, BTreeMap<String, Reference>>;

/// The modules that each module declares, by the module's path from the crate root.
type Modules = BTreeMap<Vec<String>, BTreeSet<String>>;

/// The questions whether a module provides a name, by the module's path and the name, that a
/// resolution is answering, so that a cycle of glob imports, which Rust allows, ends.
type Questions = BTreeSet<(Vec<String>, String)>;

/// The keywords that may precede a path but never continue one with `::`, unlike `crate`,
/// `self`, `Self`, and `super`, which are segments of paths.
const KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "do", "dyn",
    "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl", "in", "let",
    "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref", "return",
    "static", "struct", "trait", "true", "try", "type", "typeof", "unsafe", "unsized", "use",
    "virtual", "where", "while", "yield",
];

/// What the modules outside host's children, such as the crate root and `host`, import, by paths
/// from the crate root.
#[derive(Default)]
struct Imports {
    /// The paths that each name that they bind may stand for, under any configuration.
    names: BTreeMap<Vec<String>, BTreeSet<Vec<String>>>,
    /// The modules whose names each of them imports with a glob.
    globs: BTreeMap<Vec<String>, BTreeSet<Vec<String>>>,
}

impl Imports {
    /// Adds to `paths` every path that `path` may stand for, under any configuration: itself, and,
    /// for each name that it goes through, what the name stands for, and the same name in each
    /// module that the name's module imports with a glob, if that module provides it.
    fn expand(
        &self,
        modules: &Modules,
        path: &[String],
        paths: &mut BTreeSet<Vec<String>>,
        asking: &mut Questions,
    ) {
        // Rust refuses imports that cycle, so the paths of a crate that builds stay short.
        assert!(
            path.len() <= 1024,
            "the test follows paths of up to 1024 segments, which {} exceeds",
            path.join("::")
        );
        if !paths.insert(path.to_vec()) {
            return;
        }
        for length in 1..=path.len() {
            for target in self.names.get(&path[..length]).into_iter().flatten() {
                let alternative = [target.as_slice(), &path[length..]].concat();
                self.expand(modules, &alternative, paths, asking);
            }
            let name = &path[length - 1];
            for source in self.globs.get(&path[..length - 1]).into_iter().flatten() {
                if self.provides(modules, source, name, asking) {
                    let alternative = [source.as_slice(), &path[length - 1..]].concat();
                    self.expand(modules, &alternative, paths, asking);
                }
            }
        }
    }

    /// Returns whether `module`, or what it may stand for, declares or binds `name`, directly or
    /// through the modules that it imports with a glob. A question that `asking` holds already is
    /// part of a cycle, which provides nothing that its other answers do not.
    fn provides(
        &self,
        modules: &Modules,
        module: &[String],
        name: &String,
        asking: &mut Questions,
    ) -> bool {
        let question = (module.to_vec(), name.clone());
        if !asking.insert(question.clone()) {
            return false;
        }
        let mut aliases = BTreeSet::new();
        self.expand(modules, module, &mut aliases, asking);
        let provided = aliases.iter().any(|module| {
            modules
                .get(module)
                .is_some_and(|declared| declared.contains(name))
                || self
                    .names
                    .contains_key(&[module.as_slice(), slice::from_ref(name)].concat())
                || self
                    .globs
                    .get(module)
                    .into_iter()
                    .flatten()
                    .any(|source| self.provides(modules, source, name, asking))
        });
        asking.remove(&question);
        provided
    }
}

/// The paths that a `use` tree names: the segments of each, the name that it binds, `*` for a
/// glob, and the line of its last token.
type UsePaths = Vec<(Vec<String>, Option<String>, usize)>;

/// A token of a source file. Comments and whitespace are not tokens.
struct Token {
    /// The identifier or keyword, without the `r#` of a raw identifier, or the punctuation, with
    /// `::` as one token. Literals and lifetimes, whose text no scan needs, have none.
    text: String,
    /// Whether the token is an identifier or keyword.
    ident: bool,
    /// Whether the token is a raw identifier, which is never a keyword.
    raw: bool,
    /// The text between the quotes of a string literal, which an attribute may take as a path.
    string: Option<String>,
    /// The line on which the token starts, from 1.
    line: usize,
}

/// A cursor over the characters of a source file, which counts its lines.
struct Lexer {
    /// The characters of the source.
    chars: Vec<char>,
    /// The position of the next character.
    at: usize,
    /// The line of the next character, from 1.
    line: usize,
}

impl Lexer {
    /// Returns the character `offset` positions past the next one, if there is one.
    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.at + offset).copied()
    }

    /// Moves past `count` characters.
    fn bump(&mut self, count: usize) {
        for _ in 0..count {
            if self.peek(0) == Some('\n') {
                self.line += 1;
            }
            self.at += 1;
        }
    }

    /// Moves past the characters of an identifier, a keyword, a number, or a lifetime's name, and
    /// returns them.
    fn word(&mut self) -> String {
        let start = self.at;
        while self
            .peek(0)
            .is_some_and(|character| character == '_' || character.is_alphanumeric())
        {
            self.bump(1);
        }
        self.chars[start..self.at].iter().collect()
    }

    /// Moves past the literal that `quote` delimits, from its opening quote, and returns its
    /// value: the text between its quotes, with its escapes decoded.
    fn quoted(&mut self, quote: char) -> String {
        self.bump(1);
        let mut value = String::new();
        while let Some(character) = self.peek(0) {
            self.bump(1);
            if character == quote {
                return value;
            }
            if character != '\\' {
                value.push(character);
                continue;
            }
            let Some(escape) = self.peek(0) else {
                break;
            };
            self.bump(1);
            match escape {
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                '0' => value.push('\0'),
                'x' => {
                    let digits: String = (0..2).filter_map(|offset| self.peek(offset)).collect();
                    self.bump(digits.len());
                    value.extend(
                        u32::from_str_radix(&digits, 16)
                            .ok()
                            .and_then(char::from_u32),
                    );
                }
                'u' => {
                    let mut digits = String::new();
                    if self.peek(0) == Some('{') {
                        self.bump(1);
                        while let Some(digit) = self.peek(0) {
                            self.bump(1);
                            match digit {
                                '}' => break,
                                '_' => {}
                                _ => digits.push(digit),
                            }
                        }
                    }
                    value.extend(
                        u32::from_str_radix(&digits, 16)
                            .ok()
                            .and_then(char::from_u32),
                    );
                }
                // A line continuation skips the line break and the whitespace after it.
                '\n' | '\r' => {
                    while self.peek(0).is_some_and(char::is_whitespace) {
                        self.bump(1);
                    }
                }
                other => value.push(other),
            }
        }
        value
    }

    /// Moves past a raw string literal, from the `#` signs or the quote after its prefix, and
    /// returns its value: the text between its quotes.
    fn raw(&mut self) -> String {
        let mut hashes = 0;
        while self.peek(hashes) == Some('#') {
            hashes += 1;
        }
        self.bump(hashes + 1);
        let start = self.at;
        while let Some(character) = self.peek(0) {
            if character == '"' && (1..=hashes).all(|offset| self.peek(offset) == Some('#')) {
                let value = self.chars[start..self.at].iter().collect();
                self.bump(hashes + 1);
                return value;
            }
            self.bump(1);
        }
        self.chars[start.min(self.chars.len())..].iter().collect()
    }

    /// Moves past a block comment, which may nest others, from its `/*`.
    fn block_comment(&mut self) {
        let mut depth = 0;
        while let Some(character) = self.peek(0) {
            match (character, self.peek(1)) {
                ('/', Some('*')) => {
                    depth += 1;
                    self.bump(2);
                }
                ('*', Some('/')) => {
                    depth -= 1;
                    self.bump(2);
                    if depth == 0 {
                        return;
                    }
                }
                _ => self.bump(1),
            }
        }
    }
}

/// Splits `source` into tokens, without its comments, doc comments included.
fn tokenize(source: &str) -> Vec<Token> {
    let mut lexer = Lexer {
        chars: source.chars().collect(),
        at: 0,
        line: 1,
    };
    let mut tokens = Vec::new();
    while let Some(character) = lexer.peek(0) {
        let line = lexer.line;
        let next = lexer.peek(1);
        let mut raw = false;
        let mut string = None;
        let (text, ident) = match character {
            _ if character.is_whitespace() => {
                lexer.bump(1);
                continue;
            }
            '/' if next == Some('/') => {
                while lexer.peek(0).is_some_and(|character| character != '\n') {
                    lexer.bump(1);
                }
                continue;
            }
            '/' if next == Some('*') => {
                lexer.block_comment();
                continue;
            }
            '"' => {
                string = Some(lexer.quoted('"'));
                (String::new(), false)
            }
            // Unlike a lifetime or a label, a character literal closes after one character or
            // escape.
            '\'' if next == Some('\\') || lexer.peek(2) == Some('\'') => {
                lexer.quoted('\'');
                (String::new(), false)
            }
            '\'' => {
                lexer.bump(1);
                lexer.word();
                (String::new(), false)
            }
            ':' if next == Some(':') => {
                lexer.bump(2);
                ("::".to_owned(), false)
            }
            _ if character.is_ascii_digit() => {
                lexer.word();
                while lexer.peek(0) == Some('.')
                    && lexer.peek(1).is_some_and(|digit| digit.is_ascii_digit())
                {
                    lexer.bump(1);
                    lexer.word();
                }
                (String::new(), false)
            }
            _ if character == '_' || character.is_alphabetic() => {
                let word = lexer.word();
                match (word.as_str(), lexer.peek(0), lexer.peek(1)) {
                    ("r", Some('#'), Some(start)) if start == '_' || start.is_alphabetic() => {
                        lexer.bump(1);
                        raw = true;
                        (lexer.word(), true)
                    }
                    ("r", Some('#' | '"'), _) => {
                        string = Some(lexer.raw());
                        (String::new(), false)
                    }
                    ("br" | "cr", Some('#' | '"'), _) => {
                        lexer.raw();
                        (String::new(), false)
                    }
                    ("b" | "c", Some('"'), _) => {
                        lexer.quoted('"');
                        (String::new(), false)
                    }
                    ("b", Some('\''), _) => {
                        lexer.quoted('\'');
                        (String::new(), false)
                    }
                    _ => (word, true),
                }
            }
            _ => {
                lexer.bump(1);
                (character.to_string(), false)
            }
        };
        tokens.push(Token {
            text,
            ident,
            raw,
            string,
            line,
        });
    }
    tokens
}

/// A source file of the crate, as tokens.
struct Source {
    /// The file's path from the crate's directory, with `/` separators.
    path: String,
    /// The file's tokens.
    tokens: Vec<Token>,
}

impl Source {
    /// Returns the text of the token at `at`, or nothing past the last token.
    fn text(&self, at: usize) -> &str {
        self.tokens.get(at).map_or("", |token| &token.text)
    }

    /// Returns the text of the token at `at` where a keyword or punctuation may stand: nothing for
    /// a raw identifier, which is never a keyword, or past the last token.
    fn keyword(&self, at: usize) -> &str {
        self.tokens
            .get(at)
            .filter(|token| !token.raw)
            .map_or("", |token| &token.text)
    }

    /// Returns whether the token at `at` is an identifier or keyword.
    fn is_ident(&self, at: usize) -> bool {
        self.tokens.get(at).is_some_and(|token| token.ident)
    }

    /// Returns the line of the token at `at`, or of the last token past it.
    fn line(&self, at: usize) -> usize {
        self.tokens
            .get(at)
            .or(self.tokens.last())
            .map_or(1, |token| token.line)
    }

    /// Fails the test at the token at `at`, with `problem`.
    fn fail(&self, at: usize, problem: &str) -> ! {
        panic!("{}:{}: {problem}", self.path, self.line(at))
    }

    /// Returns the position of the bracket that closes the one at `open`.
    fn closing(&self, open: usize) -> usize {
        let mut depth = 0;
        for (at, token) in self.tokens.iter().enumerate().skip(open) {
            match token.text.as_str() {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" if depth == 1 => return at,
                ")" | "]" | "}" => depth -= 1,
                _ => {}
            }
        }
        self.fail(open, "this bracket is not closed")
    }

    /// Returns whether the attribute whose name is at `start` is `name`, or a `cfg_attr` that
    /// applies one under some configuration.
    fn applies(&self, start: usize, name: &str) -> bool {
        match self.text(start) {
            text if text == name => true,
            "cfg_attr" if self.text(start + 1) == "(" => {
                // The attributes that it applies follow the commas at the top level of its
                // arguments, the first of which is its predicate.
                let close = self.closing(start + 1);
                let mut depth = 0usize;
                (start + 2..close).any(|position| {
                    let comma = depth == 0 && self.text(position) == ",";
                    match self.text(position) {
                        "(" | "[" | "{" => depth += 1,
                        ")" | "]" | "}" => depth -= 1,
                        _ => {}
                    }
                    comma && self.applies(position + 1, name)
                })
            }
            _ => false,
        }
    }

    /// Returns the position of the innermost bracket between `from` and `at` that opens before
    /// `at` and is still open there, if any.
    fn enclosing(&self, from: usize, at: usize) -> Option<usize> {
        let mut depth = 0usize;
        for position in (from..at).rev() {
            match self.text(position) {
                ")" | "]" | "}" => depth += 1,
                "(" | "[" | "{" if depth == 0 => return Some(position),
                "(" | "[" | "{" => depth -= 1,
                _ => {}
            }
        }
        None
    }

    /// Returns the first keyword that tells the kind of the group that opens at `open`, among
    /// those of items and `match`, in its header: the tokens before it at its level, back to the
    /// `;` or brace before them, or to `from`.
    fn header_keyword(&self, from: usize, open: usize) -> Option<&str> {
        let mut depth = 0usize;
        let mut first = None;
        for position in (from..open).rev() {
            match self.keyword(position) {
                ")" | "]" => depth += 1,
                "(" | "[" if depth > 0 => depth -= 1,
                ";" | "(" | "[" | "{" | "}" if depth == 0 => break,
                keyword @ ("enum" | "extern" | "fn" | "impl" | "match" | "mod" | "struct"
                | "trait" | "union")
                    if depth == 0 =>
                {
                    first = Some(keyword);
                }
                _ => {}
            }
        }
        first
    }

    /// Returns whether the construct that starts at `start`, past `from`, is a field of a struct,
    /// a union, or a variant, or a parameter of a function. Its `<` all open type arguments, and
    /// none of its keywords starts an item: `fn`, `unsafe`, and `extern` start the type of a
    /// function pointer there.
    fn in_fields(&self, from: usize, start: usize) -> bool {
        let Some(open) = self.enclosing(from, start) else {
            return false;
        };
        match (self.text(open), self.header_keyword(from, open)) {
            ("{", Some("struct" | "union")) | ("(", Some("fn" | "struct")) => true,
            // The fields of a variant.
            ("{" | "(", None) => self.enclosing(from, open).is_some_and(|parent| {
                self.text(parent) == "{" && self.header_keyword(from, parent) == Some("enum")
            }),
            _ => false,
        }
    }

    /// Returns the position of the bracket that opens the group that closes at `close`, past
    /// `from`, if any.
    fn opening(&self, from: usize, close: usize) -> Option<usize> {
        let mut depth = 0usize;
        for position in (from..=close).rev() {
            match self.text(position) {
                ")" | "]" | "}" => depth += 1,
                "(" | "[" | "{" if depth == 1 => return Some(position),
                "(" | "[" | "{" => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        None
    }

    /// Returns the keyword or punctuation before the attributes that precede `start`, past
    /// `from`, or nothing if they start at `from`.
    fn before_attributes(&self, from: usize, start: usize) -> &str {
        let mut at = start;
        while at > from && self.text(at - 1) == "]" {
            let Some(open) = self.opening(from, at - 1) else {
                break;
            };
            match open.checked_sub(1).map(|position| self.text(position)) {
                Some("#") if open > from => at = open - 1,
                Some("!") if open > from + 1 && self.text(open - 2) == "#" => at = open - 2,
                _ => break,
            }
        }
        if at > from { self.keyword(at - 1) } else { "" }
    }

    /// Returns whether an outer attribute that precedes the item whose keyword is at `keyword`,
    /// past `from` and the item's visibility, is a `cfg` attribute, or a `cfg_attr` that applies
    /// one, which may leave the item out.
    fn conditional(&self, from: usize, keyword: usize) -> bool {
        let mut at = keyword;
        if at > from + 1
            && self.text(at - 1) == ")"
            && let Some(open) = self.opening(from, at - 1)
            && open > from
            && self.keyword(open - 1) == "pub"
        {
            at = open - 1;
        } else if at > from && self.keyword(at - 1) == "pub" {
            at -= 1;
        }
        while at > from + 1 && self.text(at - 1) == "]" {
            let Some(open) = self.opening(from, at - 1) else {
                break;
            };
            if open <= from || self.text(open - 1) != "#" {
                break;
            }
            if self.applies(open + 1, "cfg") {
                return true;
            }
            at = open - 1;
        }
        false
    }

    /// Returns whether the inner attributes at the start of the module body or file that starts
    /// at `start` include a `cfg` attribute, or a `cfg_attr` that applies one, which may leave
    /// the module out.
    fn inner_conditional(&self, start: usize) -> bool {
        let mut at = start;
        while self.text(at) == "#" && self.text(at + 1) == "!" && self.text(at + 2) == "[" {
            if self.applies(at + 3, "cfg") {
                return true;
            }
            at = self.closing(at + 2) + 1;
        }
        false
    }

    /// Returns the end of the construct that starts at `start`, past `from` and before `end`: past
    /// the `;` or the closing brace of an item or statement, past the `,` of a field, variant, or
    /// match arm, or at the bracket that closes the group around it.
    ///
    /// A generic or closure parameter, after a `<`, `|`, or `,`, also ends at a `>` or `|` that
    /// may close its list. So does any construct after a `,` in a list other than fields,
    /// variants, and match arms, such as the fields of a struct expression.
    ///
    /// The `,` and braces between the `<` and `>` of type or generic arguments do not end a
    /// construct: those of a `::<`, and, in items, fields, and parameters, those of every `<`. A
    /// construct whose `<` never close ends past its first `,`, if any.
    fn construct_end(&self, from: usize, start: usize, end: usize) -> usize {
        let mut head = start;
        if self.keyword(head) == "pub" {
            head += 1;
            if self.keyword(head) == "(" {
                head = self.closing(head) + 1;
            }
        }
        let before = self.before_attributes(from, start);
        let fields = self.in_fields(from, start);
        let listed = fields
            || self.enclosing(from, start).is_some_and(|open| {
                self.text(open) == "{"
                    && matches!(self.header_keyword(from, open), Some("enum" | "match"))
            });
        let parameter = matches!(before, "<" | "|") || before == "," && !listed;
        let item = !fields
            && !parameter
            && before != "("
            && matches!(
                self.keyword(head),
                "async"
                    | "const"
                    | "enum"
                    | "extern"
                    | "fn"
                    | "impl"
                    | "let"
                    | "macro_rules"
                    | "mod"
                    | "static"
                    | "struct"
                    | "trait"
                    | "type"
                    | "union"
                    | "unsafe"
                    | "use"
            );
        // These end at their semicolon, whatever blocks their expressions hold.
        let semicolon = item
            && (matches!(self.keyword(head), "let" | "static" | "type" | "use")
                || self.keyword(head) == "const"
                    && !matches!(
                        self.keyword(head + 1),
                        "async" | "extern" | "fn" | "unsafe" | "{"
                    ));
        let types = item || fields || matches!(before, "<" | "|") || self.keyword(start) == "pub";
        let mut depth = 0;
        let mut angles = 0usize;
        let mut first_comma = None;
        for at in start..end {
            let previous = self.keyword(at.saturating_sub(1));
            match self.keyword(at) {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" if depth == 0 => {
                    return if angles == 0 {
                        at
                    } else {
                        first_comma.unwrap_or(at)
                    };
                }
                "}" if depth == 1
                    && angles == 0
                    && !semicolon
                    && !matches!(self.keyword(at + 1), "else" | "." | "?") =>
                {
                    return at + 1;
                }
                ")" | "]" | "}" => depth -= 1,
                ";" if depth == 0 => return at + 1,
                "|" if parameter && depth == 0 && angles == 0 => return at,
                ">" if parameter && depth == 0 && angles == 0 && !matches!(previous, "-" | "=") => {
                    return at;
                }
                "<" if depth == 0 && (types || previous == "::") => angles += 1,
                // The `>` of `->` and `=>` closes no `<`.
                ">" if depth == 0 && angles > 0 && !matches!(previous, "-" | "=") => angles -= 1,
                "," if depth == 0 && !item => {
                    if angles == 0 {
                        return at + 1;
                    }
                    first_comma.get_or_insert(at + 1);
                }
                _ => {}
            }
        }
        if angles == 0 {
            end
        } else {
            first_comma.unwrap_or(end)
        }
    }

    /// Returns the first segment of the `crate::`, `super::`, `self::`, or `$crate::` path that
    /// starts at `at`, if one does, and the position of the `::` after it.
    fn path_start(&self, at: usize) -> Option<(&str, usize)> {
        let (first, after) = match self.keyword(at) {
            "$" if self.keyword(at + 1) == "crate" => ("crate", at + 2),
            first @ ("crate" | "super" | "self") => (first, at + 1),
            _ => return None,
        };
        (self.text(after) == "::").then_some((first, after))
    }

    /// Returns the segments of the path from a module of the crate that starts at `at`, if one
    /// does, and the position past it: a `crate::`, `super::`, `self::`, or `$crate::` path, or a
    /// path from a name of `prelude`, which the extern prelude gives the crate root, with `::`
    /// first if `::` starts it.
    fn path_at(&self, at: usize, prelude: &BTreeSet<String>) -> Option<(Vec<String>, usize)> {
        if let Some((first, next)) = self.path_start(at) {
            return Some(self.segments(first, next));
        }
        if !prelude.contains(self.text(at)) || self.text(at + 1) != "::" {
            return None;
        }
        // After a segment, the name continues a path. A `>` may close a type or compare, so the
        // name after it may start one.
        let global = at >= 1 && self.text(at - 1) == "::";
        if global && at >= 2 && self.is_ident(at - 2) && !KEYWORDS.contains(&self.keyword(at - 2)) {
            return None;
        }
        let (mut segments, next) = self.segments(self.text(at), at + 1);
        if global {
            segments.insert(0, "::".to_owned());
        }
        Some((segments, next))
    }

    /// Returns the segments of the path whose first segment is `first`, and whose others follow
    /// the `::` at `next`, and the position past them.
    fn segments(&self, first: &str, mut next: usize) -> (Vec<String>, usize) {
        let mut segments = vec![first.to_owned()];
        while self.text(next) == "::" && self.is_ident(next + 1) {
            segments.push(self.text(next + 1).to_owned());
            next += 2;
        }
        (segments, next)
    }

    /// Returns the paths from modules of the crate in `text`, such as a path that a string literal
    /// holds, as [`Source::path_at`] finds them with `prelude`.
    fn paths_in(text: &str, prelude: &BTreeSet<String>) -> Vec<Vec<String>> {
        let source = Self {
            path: String::new(),
            tokens: tokenize(text),
        };
        let mut paths = Vec::new();
        let mut at = 0;
        while at < source.tokens.len() {
            match source.path_at(at, prelude) {
                Some((segments, next)) => {
                    paths.push(segments);
                    at = next;
                }
                None => at += 1,
            }
        }
        paths
    }

    /// Returns the position of the first construct of the transcribers of a `macro_rules!`, whose
    /// rules are `self.tokens[range]`, that names what only the macro's expansions resolve, if
    /// any: a `use`, `mod`, or `extern crate` declaration; a path from a module of the crate, as
    /// [`Source::path_at`] finds them with `prelude`, or a string that holds one, as the argument
    /// of a `serde` attribute would; a metavariable or repetition that may join a path from the
    /// input, before `::` or a segment and `::`, after `::` or the start of a path from a module;
    /// a metavariable of a token tree, which may hold any of these; or a `serde` attribute that a
    /// metavariable completes, whose string the input holds. Matchers only match the input of the
    /// macro.
    fn unfollowed_in_template(
        &self,
        range: Range<usize>,
        prelude: &BTreeSet<String>,
    ) -> Option<usize> {
        let mut at = range.start;
        while at < range.end {
            // A rule is a matcher, `=>`, and a transcriber, each a delimited group.
            if !matches!(self.text(at), "(" | "[" | "{") {
                at += 1;
                continue;
            }
            let matcher = self.closing(at);
            let open = matcher + 3;
            if (self.text(matcher + 1), self.text(matcher + 2)) != ("=", ">")
                || !matches!(self.text(open), "(" | "[" | "{")
            {
                at = matcher + 1;
                continue;
            }
            let close = self.closing(open);
            // The metavariables that match a token tree, such as `crate` or `::`.
            let trees: BTreeSet<&str> = (at + 1..matcher)
                .filter(|&position| {
                    self.text(position) == "$"
                        && self.is_ident(position + 1)
                        && self.text(position + 2) == ":"
                        && self.text(position + 3) == "tt"
                })
                .map(|position| self.text(position + 1))
                .collect();
            let unfollowed = (open + 1..close).find(|&position| {
                let declares = match self.keyword(position) {
                    "use" => self.text(position + 1) != "<",
                    "mod" => true,
                    "extern" => self.keyword(position + 1) == "crate",
                    _ => false,
                };
                // The token after a metavariable, or after a repetition and its separator, and
                // whether that separator, or the first token of the repetition, is `::`. Either
                // may expand to `::` or to the start of a path, such as `crate ::`.
                let expansion = match (self.text(position), self.text(position + 1)) {
                    ("$", "(") => {
                        let group = self.closing(position + 1);
                        let separator = !matches!(self.text(group + 1), "*" | "+" | "?");
                        let joined = (separator && self.text(group + 1) == "::")
                            || self.text(position + 2) == "::";
                        Some((group + 2 + usize::from(separator), joined))
                    }
                    ("$", _) if self.is_ident(position + 1) => Some((position + 2, false)),
                    _ => None,
                };
                let joins = expansion.is_some_and(|(after, joined)| {
                    joined
                        || self.text(after) == "::"
                        || (self.is_ident(after)
                            && !KEYWORDS.contains(&self.keyword(after))
                            && self.text(after + 1) == "::")
                });
                let tree = self.text(position) == "$" && trees.contains(self.text(position + 1));
                let start = (matches!(self.keyword(position), "crate" | "self" | "super")
                    || prelude.contains(self.text(position)))
                    && self.text(position + 1) == "$";
                let string = self.tokens[position]
                    .string
                    .as_deref()
                    .is_some_and(|text| !Self::paths_in(text, prelude).is_empty());
                let serde = self.text(position) == "#" && {
                    let open = position + 1 + usize::from(self.text(position + 1) == "!");
                    self.text(open) == "[" && {
                        let end = self.closing(open);
                        let has =
                            |text: &str| (open + 1..end).any(|index| self.text(index) == text);
                        has("serde") && has("$")
                    }
                };
                declares
                    || joins
                    || tree
                    || start
                    || string
                    || serde
                    || self.path_at(position, prelude).is_some()
            });
            if unfollowed.is_some() {
                return unfollowed;
            }
            at = close + 1;
        }
        None
    }

    /// Expands the `use` tree at `*at`, under `prefix`, into `paths`, moves `*at` past it, and
    /// returns whether it could read the tree, which the input of a macro need not be.
    fn use_tree(&self, at: &mut usize, prefix: &[String], paths: &mut UsePaths) -> bool {
        let mut path = prefix.to_vec();
        if self.text(*at) == "::" {
            // A path from the extern prelude, which names this crate only by a name that
            // `extern crate self` gives it.
            path.push("::".to_owned());
            *at += 1;
        }
        loop {
            let line = self.line(*at);
            match self.text(*at) {
                "{" => {
                    *at += 1;
                    while self.text(*at) != "}" {
                        if !self.use_tree(at, &path, paths) {
                            return false;
                        }
                        if self.text(*at) == "," {
                            *at += 1;
                        }
                    }
                    *at += 1;
                    return true;
                }
                "*" => {
                    *at += 1;
                    paths.push((path, Some("*".to_owned()), line));
                    return true;
                }
                "$" if self.keyword(*at + 1) == "crate" => {
                    path.push("crate".to_owned());
                    *at += 2;
                }
                // `self` at the start of a group names the group's prefix.
                "self"
                    if self.keyword(*at) == "self"
                        && !prefix.is_empty()
                        && path.len() == prefix.len() =>
                {
                    *at += 1;
                }
                segment if self.is_ident(*at) => {
                    path.push(segment.to_owned());
                    *at += 1;
                }
                _ => return false,
            }
            if self.text(*at) == "::" {
                *at += 1;
                continue;
            }
            let binding = if self.keyword(*at) == "as" {
                *at += 2;
                Some(self.text(*at - 1).to_owned()).filter(|alias| alias != "_")
            } else {
                path.last().cloned()
            };
            paths.push((path, binding, line));
            return true;
        }
    }
}

/// Reads the `cfg` predicate at `*at`, moves `*at` past it, and returns whether it holds only in
/// test builds: `test`, `all` of predicates of which one does, or `any` of predicates that all do.
fn needs_test(source: &Source, at: &mut usize) -> bool {
    let name = source.text(*at);
    *at += 1;
    match source.text(*at) {
        "=" => {
            // An option with a value, such as `feature = "cli"`.
            *at += 2;
            false
        }
        "(" => {
            *at += 1;
            let mut operands = Vec::new();
            while !matches!(source.text(*at), ")" | "") {
                operands.push(needs_test(source, at));
                if source.text(*at) == "," {
                    *at += 1;
                }
            }
            *at += 1;
            match name {
                "all" => operands.contains(&true),
                "any" => !operands.is_empty() && !operands.contains(&false),
                _ => false,
            }
        }
        _ => name == "test",
    }
}

/// A path that production code names.
#[derive(Clone)]
struct Reference {
    /// The module in which the path appears, from the crate root, such as `["host", "store"]`.
    module: Vec<String>,
    /// The file in which the path appears, from the crate's directory.
    file: String,
    /// The line of the path's last segment.
    line: usize,
    /// The path's segments, as its code or its `use` tree writes them, with `::` first if `::`
    /// starts it.
    segments: Vec<String>,
    /// The name that a `use` declaration binds to the path, `*` for a glob, if any.
    binding: Option<String>,
    /// Whether the declaration binds the name only in a block, such as the body of a function,
    /// rather than in `module`.
    local: bool,
}

/// What a scan found in the production code of the crate's modules.
#[derive(Default)]
struct Scan {
    /// The modules that each module declares, by the module's path from the crate root.
    modules: Modules,
    /// The names that each module declares or imports by name in every configuration, outside
    /// the input of a macro, which shadow those of the extern prelude.
    unconditional: Modules,
    /// The number of definitions of each module that a declaration defines: alternatives under
    /// different configurations, if more than one.
    definitions: BTreeMap<Vec<String>, usize>,
    /// The names that `extern crate self as NAME;` at the crate root gives the crate root in the
    /// extern prelude, where every module can name it.
    extern_prelude: BTreeSet<String>,
    /// The paths that the code names.
    references: Vec<Reference>,
}

impl Scan {
    /// Scans the crate's production code from its root file, `root`, from the crate's directory,
    /// and the files of the modules that it declares, reading each with `read`.
    fn walk(&mut self, read: &dyn Fn(&str) -> Option<String>, root: &str) {
        let text = read(root).unwrap_or_else(|| panic!("cannot read {root}"));
        let source = Source {
            path: root.to_owned(),
            tokens: tokenize(&text),
        };
        // Every module may name the crate root by the names that `extern crate self as NAME;`
        // gives it at the crate root, wherever the root's code declares them. Counting one that
        // an inline module declares, for that module alone, only adds references.
        for at in 0..source.tokens.len() {
            if (0..4)
                .map(|offset| source.keyword(at + offset))
                .eq(["extern", "crate", "self", "as"])
                && source.is_ident(at + 4)
                && source.text(at + 4) != "_"
            {
                self.extern_prelude.insert(source.text(at + 4).to_owned());
            }
        }
        let dir = root
            .rsplit_once('/')
            .map_or_else(String::new, |(dir, _)| format!("{dir}/"));
        let mut files = vec![(source, Vec::new(), dir)];
        while let Some((source, module, dir)) = files.pop() {
            let mut declared = Vec::new();
            let range = 0..source.tokens.len();
            self.code(&source, range, &module, &dir, false, &mut declared);
            for (child, dir, input) in declared {
                let name = child.join("::");
                let base = &dir[..dir.len() - 1];
                let mut found = [format!("{base}.rs"), format!("{dir}mod.rs")]
                    .into_iter()
                    .filter_map(|path| read(&path).map(|text| (path, text)));
                let Some((path, text)) = found.next() else {
                    // A macro need not declare the modules that its input does.
                    assert!(input, "module {name} has no file in {base}");
                    continue;
                };
                assert!(found.next().is_none(), "module {name} has two files");
                let source = Source {
                    path,
                    tokens: tokenize(&text),
                };
                // A file whose inner attributes may leave its module out declares it only in
                // some configurations.
                if source.inner_conditional(0)
                    && let Some((last, parent)) = child.split_last()
                    && let Some(names) = self.unconditional.get_mut(parent)
                {
                    names.remove(last);
                }
                files.push((source, child, dir));
            }
        }
    }

    /// Scans the tokens `range` of `source`, the code of `module`, whose child modules' files are
    /// in the directory `dir`, and adds the modules that it declares in files to `declared`, with
    /// their own directories and whether a macro's input declares them. In the input of a macro,
    /// as `input` tells, which need not be Rust, paths count, but declarations that the test
    /// cannot read are left to the macro.
    fn code(
        &mut self,
        source: &Source,
        range: Range<usize>,
        module: &[String],
        dir: &str,
        input: bool,
        declared: &mut Vec<(Vec<String>, String, bool)>,
    ) {
        let mut at = range.start;
        // Whether the attributes since the last construct make the next one test-only.
        let mut test_only = false;
        // The paths that the outer attributes since the last construct name, which count unless
        // the next construct is test-only.
        let mut pending = Vec::new();
        while at < range.end {
            let text = source.keyword(at);
            if text == "#" && matches!(source.text(at + 1), "[" | "!") {
                let inner = source.text(at + 1) == "!";
                let open = at + 1 + usize::from(inner);
                let close = source.closing(open);
                let mut next = close + 1;
                // An attribute names the paths in its tokens, and a `serde` attribute those in
                // its string arguments too, such as that of `with`, unless only test builds apply
                // it. A macro may keep or drop the attributes of its input as it pleases, so there
                // no `cfg` marks anything test-only.
                let serde = (open + 1..close)
                    .any(|position| source.is_ident(position) && source.text(position) == "serde");
                let test_attribute = !input
                    && source.text(open + 1) == "cfg_attr"
                    && source.text(open + 2) == "("
                    && needs_test(source, &mut (open + 3));
                let mut found = Vec::new();
                let mut position = open + 1;
                while !test_attribute && position < close {
                    let mut after = position + 1;
                    let paths = if let Some((segments, end)) =
                        source.path_at(position, &self.extern_prelude)
                    {
                        after = end;
                        vec![segments]
                    } else if serde && let Some(text) = source.tokens[position].string.as_deref() {
                        Source::paths_in(text, &self.extern_prelude)
                    } else {
                        Vec::new()
                    };
                    for segments in paths {
                        found.push(Reference {
                            module: module.to_vec(),
                            file: source.path.clone(),
                            line: source.line(after - 1),
                            segments,
                            binding: None,
                            local: false,
                        });
                    }
                    position = after;
                }
                // An inner attribute is the enclosing construct's, which is production code here.
                if inner {
                    self.references.append(&mut found);
                } else {
                    pending.append(&mut found);
                }
                match (source.text(open + 1), source.text(open + 2)) {
                    _ if source.applies(open + 1, "path") => {
                        source.fail(at, "the test does not follow #[path] attributes");
                    }
                    ("cfg", "(") if !input && needs_test(source, &mut (open + 3)) => {
                        if !inner {
                            test_only = true;
                        } else if let Some(group) = source.enclosing(range.start, at) {
                            // The rest of the item whose body the attribute opens is test-only.
                            next = source.closing(group);
                        } else {
                            // The rest of the module is test-only.
                            return;
                        }
                    }
                    _ => {}
                }
                at = next;
            } else if test_only {
                pending.clear();
                at = source.construct_end(range.start, at, range.end);
                test_only = false;
            } else if !pending.is_empty() {
                self.references.append(&mut pending);
            } else if text == "macro_rules"
                && source.text(at + 1) == "!"
                && source.is_ident(at + 2)
                && matches!(source.text(at + 3), "{" | "(" | "[")
            {
                // A template names nothing that the expansions of the macro do not name where
                // they expand, unless it declares, imports, or names a path from a module.
                let close = source.closing(at + 3);
                let unfollowed = source.unfollowed_in_template(at + 4..close, &self.extern_prelude);
                if let Some(position) = unfollowed {
                    source.fail(
                        position,
                        "the test does not follow macro templates that declare, import, or name \
                         paths from a module or from their input, which resolve where the macro \
                         expands, possibly in host's children",
                    );
                }
                at = close + 1;
            } else if text == "!"
                && at > range.start
                && source.is_ident(at - 1)
                && !KEYWORDS.contains(&source.keyword(at - 1))
                && matches!(source.text(at + 1), "(" | "[" | "{")
            {
                let close = source.closing(at + 1);
                self.code(source, at + 2..close, module, dir, true, declared);
                at = close + 1;
            } else if text == "mod"
                && source.is_ident(at + 1)
                && matches!(source.text(at + 2), ";" | "{")
            {
                let name = source.text(at + 1).to_owned();
                let mut child = module.to_vec();
                child.push(name.clone());
                *self.definitions.entry(child.clone()).or_default() += 1;
                let child_dir = format!("{dir}{name}/");
                let inline = source.text(at + 2) == "{";
                if !input
                    && source.enclosing(range.start, at).is_none()
                    && !source.conditional(range.start, at)
                    && (!inline || !source.inner_conditional(at + 3))
                {
                    self.unconditional
                        .entry(module.to_vec())
                        .or_default()
                        .insert(name.clone());
                }
                self.modules
                    .entry(module.to_vec())
                    .or_default()
                    .insert(name);
                if source.text(at + 2) == ";" {
                    declared.push((child, child_dir, input));
                    at += 3;
                } else {
                    let close = source.closing(at + 2);
                    self.code(source, at + 3..close, &child, &child_dir, input, declared);
                    at = close + 1;
                }
            } else if text == "use" && source.text(at + 1) != "<" {
                let mut paths = Vec::new();
                let mut next = at + 1;
                if source.use_tree(&mut next, &[], &mut paths) && source.text(next) == ";" {
                    // In a block, such as the body of a function, the declaration binds its
                    // names for the block alone. The input of a macro may expand anywhere.
                    let local = !input && source.enclosing(range.start, at).is_some();
                    let unconditional = !input && !local && !source.conditional(range.start, at);
                    for (segments, binding, line) in paths {
                        if unconditional
                            && let Some(name) = binding.as_ref().filter(|name| *name != "*")
                        {
                            self.unconditional
                                .entry(module.to_vec())
                                .or_default()
                                .insert(name.clone());
                        }
                        self.references.push(Reference {
                            module: module.to_vec(),
                            file: source.path.clone(),
                            line,
                            segments,
                            binding,
                            local,
                        });
                    }
                    at = next + 1;
                } else if input {
                    at += 1;
                } else {
                    source.fail(at, "the test cannot read this use declaration");
                }
            } else if text == "extern"
                && source.keyword(at + 1) == "crate"
                && source.is_ident(at + 2)
            {
                // `extern crate self as NAME;` names the crate root, and another crate's name
                // names none of this crate's modules.
                let name = source.text(at + 2);
                let segments = if source.keyword(at + 2) == "self" {
                    vec!["crate".to_owned()]
                } else {
                    vec!["::".to_owned(), name.to_owned()]
                };
                let (binding, next) = if source.keyword(at + 3) == "as" {
                    (source.text(at + 4), at + 5)
                } else {
                    (name, at + 3)
                };
                self.references.push(Reference {
                    module: module.to_vec(),
                    file: source.path.clone(),
                    line: source.line(at),
                    segments,
                    binding: Some(binding.to_owned()).filter(|binding| binding != "_"),
                    local: !input && source.enclosing(range.start, at).is_some(),
                });
                at = next;
            } else if input && let Some(text) = source.tokens[at].string.as_deref() {
                // A macro may put a string of its input where a path is read, such as in a
                // `serde` attribute that it completes.
                for segments in Source::paths_in(text, &self.extern_prelude) {
                    self.references.push(Reference {
                        module: module.to_vec(),
                        file: source.path.clone(),
                        line: source.line(at),
                        segments,
                        binding: None,
                        local: false,
                    });
                }
                at += 1;
            } else if let Some((segments, next)) = source.path_at(at, &self.extern_prelude) {
                self.references.push(Reference {
                    module: module.to_vec(),
                    file: source.path.clone(),
                    line: source.line(next - 1),
                    segments,
                    binding: None,
                    local: false,
                });
                at = next;
            } else {
                at += 1;
            }
        }
    }

    /// Returns the paths from the crate root that the path `segments`, which the code of `module`
    /// names, may stand for: one from `crate`; from `self` or `super`; from a module that `module`
    /// declares, or a name that it binds or may import with a glob, as `imports` records them;
    /// and from a name that the extern prelude gives the crate root, which `::` may precede,
    /// unless `module`, which a single definition defines, declares or imports that name in
    /// every configuration.
    fn resolve(
        &self,
        module: &[String],
        segments: &[String],
        imports: &Imports,
    ) -> Vec<Vec<String>> {
        let Some((first, rest)) = segments.split_first() else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        match first.as_str() {
            "crate" => paths.push(rest.to_vec()),
            "self" | "super" => {
                // `self` names the module, and each `super` after it, or at the start, the parent
                // of the module before.
                let mut path = module.to_vec();
                let mut rest = if first == "self" { rest } else { segments };
                while let Some((segment, after)) = rest.split_first()
                    && segment == "super"
                {
                    if path.pop().is_none() {
                        return Vec::new();
                    }
                    rest = after;
                }
                path.extend_from_slice(rest);
                paths.push(path);
            }
            "::" => {
                if let Some((name, rest)) = rest.split_first()
                    && self.extern_prelude.contains(name)
                {
                    paths.push(rest.to_vec());
                }
            }
            _ => {
                if self
                    .modules
                    .get(module)
                    .is_some_and(|declared| declared.contains(first))
                    || imports
                        .names
                        .contains_key(&[module, slice::from_ref(first)].concat())
                    || imports.globs.contains_key(module)
                {
                    paths.push([module, segments].concat());
                }
                // Alternative definitions of `module` may each shadow the name or not.
                let shadowed = self.definitions.get(module).is_none_or(|&count| count == 1)
                    && self
                        .unconditional
                        .get(module)
                        .is_some_and(|declared| declared.contains(first));
                if self.extern_prelude.contains(first) && !shadowed {
                    paths.push(rest.to_vec());
                }
            }
        }
        paths
    }
}

/// Returns the production graph between host's children in the crate whose files `read` reads,
/// by their paths from the crate's directory.
fn graph(read: &dyn Fn(&str) -> Option<String>) -> Graph {
    let mut scan = Scan::default();
    scan.walk(read, "src/lib.rs");
    let host = ["host".to_owned()];
    let children = scan
        .modules
        .get(host.as_slice())
        .cloned()
        .unwrap_or_default();
    // The child whose code a module is, if it is one of host's children or inside one.
    let child_of = |module: &[String]| match module {
        [first, child, ..] if *first == host[0] && children.contains(child) => Some(child.clone()),
        _ => None,
    };
    // The children that a path from the crate root may name: directly, or through what the
    // modules outside host's children import.
    let targets = |path: &[String], imports: &Imports| -> BTreeSet<String> {
        let mut paths = BTreeSet::new();
        imports.expand(&scan.modules, path, &mut paths, &mut Questions::new());
        paths.iter().filter_map(|path| child_of(path)).collect()
    };
    // The children that the names under the module that a path from the crate root may name may
    // stand for: what an import of the module, or a glob of it, can bring into a child. They are
    // the children that the modules under it are or are in, the names that the modules outside
    // host's children import there, and those under the modules that those names, their glob
    // imports, and their modules stand for in turn.
    let names_under = |path: &[String], imports: &Imports| -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        let mut pending = vec![path.to_vec()];
        let mut seen = BTreeSet::new();
        while let Some(path) = pending.pop() {
            let mut modules = BTreeSet::new();
            imports.expand(&scan.modules, &path, &mut modules, &mut Questions::new());
            for module in modules {
                if !seen.insert(module.clone()) {
                    continue;
                }
                for name in imports
                    .names
                    .keys()
                    .filter(|name| name.len() > module.len() && name.starts_with(&module))
                {
                    found.extend(targets(name, imports));
                    pending.push(name.clone());
                }
                pending.extend(imports.globs.get(&module).into_iter().flatten().cloned());
                for declared in scan.modules.get(&module).into_iter().flatten() {
                    let declared = [module.as_slice(), slice::from_ref(declared)].concat();
                    found.extend(child_of(&declared));
                    pending.push(declared);
                }
            }
        }
        found
    };
    // What the modules outside host's children import, which code can name instead, until no
    // import adds a path. An import in a block binds its name for the block alone.
    let mut imports = Imports::default();
    loop {
        let mut added = false;
        for reference in scan
            .references
            .iter()
            .filter(|reference| child_of(&reference.module).is_none() && !reference.local)
        {
            let Some(binding) = &reference.binding else {
                continue;
            };
            for path in scan.resolve(&reference.module, &reference.segments, &imports) {
                if binding == "*" {
                    assert!(
                        targets(&path, &imports).is_empty(),
                        "{}:{}: the test cannot resolve the names of glob imports of host's \
                         children",
                        reference.file,
                        reference.line
                    );
                    let globs = imports.globs.entry(reference.module.clone()).or_default();
                    added |= globs.insert(path);
                } else {
                    let name = [reference.module.as_slice(), slice::from_ref(binding)].concat();
                    added |= imports.names.entry(name).or_default().insert(path);
                }
            }
        }
        if !added {
            break;
        }
    }
    let mut graph: Graph = children
        .iter()
        .map(|child| (child.clone(), BTreeMap::new()))
        .collect();
    for reference in &scan.references {
        let Some(from) = child_of(&reference.module) else {
            continue;
        };
        let mut found = BTreeSet::new();
        for path in scan.resolve(&reference.module, &reference.segments, &imports) {
            // A child's relative paths count only through the imports that bind them, so an
            // import of the crate root or of `host` itself, under any name, could hide any
            // reference.
            if reference.binding.is_some() {
                let mut alternatives = BTreeSet::new();
                imports.expand(
                    &scan.modules,
                    &path,
                    &mut alternatives,
                    &mut Questions::new(),
                );
                assert!(
                    alternatives
                        .iter()
                        .all(|alternative| !alternative.is_empty() && alternative != &host),
                    "{}:{}: the test cannot follow imports of the crate root or of host in host's \
                     children: name the children with crate::host::CHILD paths",
                    reference.file,
                    reference.line
                );
                found.extend(names_under(&path, &imports));
            }
            found.extend(targets(&path, &imports));
        }
        for to in found {
            if to == from {
                continue;
            }
            let references = graph
                .get_mut(&from)
                .expect("a scanned module of host is in a child");
            let first = references
                .get(&to)
                .is_none_or(|first| (&reference.file, reference.line) < (&first.file, first.line));
            if first {
                references.insert(to, reference.clone());
            }
        }
    }
    graph
}

/// Returns the shortest path of references from `from` to `to` in `graph`, with both, if there is
/// one.
fn shortest_path<'a>(graph: &'a Graph, from: &'a str, to: &'a str) -> Option<Vec<String>> {
    let mut previous: BTreeMap<&str, &str> = BTreeMap::new();
    let mut queue = VecDeque::from([from]);
    while let Some(child) = queue.pop_front() {
        if child == to {
            let mut path = vec![to.to_owned()];
            let mut current = to;
            while current != from {
                current = previous[&current];
                path.push(current.to_owned());
            }
            path.reverse();
            return Some(path);
        }
        for next in graph.get(child).into_iter().flat_map(BTreeMap::keys) {
            if next != from && !previous.contains_key(next.as_str()) {
                previous.insert(next, child);
                queue.push_back(next);
            }
        }
    }
    None
}

/// Returns the cycles of `graph`, shortest first: for each reference between two children that
/// lies on a cycle, the shortest cycle through it, from its alphabetically first child.
fn cycles(graph: &Graph) -> Vec<Vec<String>> {
    let mut cycles = BTreeSet::new();
    for (from, references) in graph {
        for to in references.keys() {
            let Some(back) = shortest_path(graph, to, from) else {
                continue;
            };
            let mut cycle = vec![from.clone()];
            cycle.extend_from_slice(&back[..back.len() - 1]);
            let first = (0..cycle.len())
                .min_by_key(|&index| &cycle[index])
                .unwrap_or(0);
            cycle.rotate_left(first);
            cycles.insert(cycle);
        }
    }
    let mut cycles: Vec<_> = cycles.into_iter().collect();
    cycles.sort_by_key(Vec::len);
    cycles
}

/// Describes `cycles` of `graph`, with the first reference along each of their edges.
fn report(graph: &Graph, cycles: &[Vec<String>]) -> String {
    let mut report = String::from(
        "host's children reference each other in cycles. Break each one, for example by moving \
         the items that one of its references names into a child below the others:\n",
    );
    for cycle in cycles {
        let _ = writeln!(report, "\n{} -> {}", cycle.join(" -> "), cycle[0]);
        for (index, from) in cycle.iter().enumerate() {
            let to = &cycle[(index + 1) % cycle.len()];
            let reference = &graph[from][to];
            let _ = writeln!(
                report,
                "    {from} -> {to}: {}:{}: {}",
                reference.file,
                reference.line,
                reference.segments.join("::").replacen("::::", "::", 1)
            );
        }
    }
    report
}

/// Returns host's children in layers, lowest first, given an acyclic `graph`: each child is one
/// layer above the highest child that it references.
fn layers(graph: &Graph) -> Vec<Vec<&str>> {
    let mut layer: BTreeMap<&str, usize> = BTreeMap::new();
    while layer.len() < graph.len() {
        let placed = layer.len();
        for (child, references) in graph {
            if !layer.contains_key(child.as_str())
                && references.keys().all(|to| layer.contains_key(to.as_str()))
            {
                let above = references.keys().map(|to| layer[to.as_str()] + 1).max();
                layer.insert(child, above.unwrap_or(0));
            }
        }
        assert!(layer.len() > placed, "the graph has a cycle");
    }
    let mut layers = vec![Vec::new(); layer.values().max().map_or(0, |top| top + 1)];
    for (child, index) in layer {
        layers[index].push(child);
    }
    layers
}

/// Returns the graph of the crate whose files `files` holds, by their paths.
fn graph_of(files: &[(&str, &str)]) -> Graph {
    let files: BTreeMap<&str, &str> = files.iter().copied().collect();
    graph(&|path| files.get(path).map(|text| (*text).to_owned()))
}

/// Returns the references between children that `graph` holds, as `FROM -> TO`.
fn edges(graph: &Graph) -> Vec<String> {
    graph
        .iter()
        .flat_map(|(from, references)| references.keys().map(move |to| format!("{from} -> {to}")))
        .collect()
}

#[test]
fn the_children_of_host_reference_each_other_without_cycles() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"));
    let graph = graph(&|path| fs::read_to_string(directory.join(path)).ok());
    assert!(
        !edges(&graph).is_empty(),
        "the test found no reference between host's children"
    );
    let cycles = cycles(&graph);
    assert!(cycles.is_empty(), "{}", report(&graph, &cycles));
    for (index, children) in layers(&graph).iter().enumerate() {
        println!("layer {index}: {}", children.join(", "));
    }
}

#[test]
fn only_production_paths_into_other_children_are_references() {
    let graph = graph_of(&[
        (
            "src/lib.rs",
            "pub mod host;\npub use host::e::{Exported, other as Renamed};\n",
        ),
        (
            "src/host/mod.rs",
            "mod a;\nmod b;\nmod c;\nmod d;\nmod e;\n#[cfg(test)]\nmod tests;\n",
        ),
        (
            "src/host/a.rs",
            r##"//! [`crate::host::b`] and [`super::b`] are links.
/* A block comment, /* nested */, names crate::host::b. */
const TEXT: &str = "crate::host::b, \" super::b";
const RAW: &str = r#"self::b " super::b"#;
const QUOTE: char = '"';
#[cfg(test)]
use crate::host::b::Hidden;
#[cfg(all(test, unix))]
fn hidden<A, B>() -> u8 { crate::host::b::VALUE }
#[cfg(any(test, feature = "cli"))]
use super::c::Counted;
fn shown<'a>(text: &'a str) -> &'a str { self::helper(crate::host::d::VALUE) }
"##,
        ),
        (
            "src/host/b/mod.rs",
            "mod inner;\nmod imp {\n    use super::super::{c::{self, Item}, e};\n}\n",
        ),
        (
            "src/host/b/inner.rs",
            "fn value() -> u8 { super::super::d::VALUE }\n",
        ),
        ("src/host/c.rs", "use crate::Renamed;\n"),
        (
            "src/host/d.rs",
            "pub(crate) fn helper() { crate::host::d::helper(); self::super::e::f(); }\n",
        ),
        ("src/host/e.rs", "use std::fs;\n"),
    ]);
    assert_eq!(
        edges(&graph),
        [
            "a -> c", "a -> d", "b -> c", "b -> d", "b -> e", "c -> e", "d -> e"
        ]
    );
}

#[test]
fn test_only_code_ends_where_its_construct_does() {
    // The test-only code of `a` names `t`, and the production code after each construct names
    // another child.
    let graph = graph_of(&[
        ("src/lib.rs", "pub mod host;\n"),
        (
            "src/host/mod.rs",
            "mod a;\nmod b;\nmod c;\nmod d;\nmod e;\nmod f;\nmod g;\nmod h;\nmod i;\nmod t;\n",
        ),
        (
            "src/host/a.rs",
            r#"pub(crate) struct Fields {
    #[cfg(test)]
    probe: BTreeMap<crate::host::t::T, crate::host::t::T>,
    #[cfg(test)]
    sized: Probe<{ 1 }, crate::host::t::T>,
    #[cfg(test)]
    r#type: u8,
    shown: crate::host::b::B,
}
pub(crate) struct Pointers(
    #[cfg(test)] fn(),
    #[cfg(test)] unsafe extern "C" fn(crate::host::t::T),
    crate::host::c::C,
);
#[cfg(test)]
fn sized() -> Probe<{ 1 }> { crate::host::t::probe() }
fn test_only() {
    #![cfg(test)]
    crate::host::t::probe();
}
fn after() -> u8 { crate::host::d::VALUE }
fn arm(value: u8) -> u8 {
    match value {
        #[cfg(test)]
        small if small < 2 => crate::host::t::VALUE,
        large if large > 10 => crate::host::e::VALUE,
        _ => 0,
    }
}
fn generic<#[cfg(test)] const N: usize, T>() { crate::host::f::value(); }
fn bounded<T, #[cfg(test)] U: Into<crate::host::t::T>>() { crate::host::g::value(); }
fn closure() { let _ = |#[cfg(test)] probe: crate::host::t::T, value: u8| crate::host::h::f(value); }
fn literal() -> S {
    S { #[cfg(test)] probe: crate::host::t::VALUE < 2, shown: crate::host::i::VALUE > 1 }
}
"#,
        ),
        ("src/host/b.rs", ""),
        ("src/host/c.rs", ""),
        ("src/host/d.rs", ""),
        ("src/host/e.rs", ""),
        ("src/host/f.rs", ""),
        ("src/host/g.rs", ""),
        ("src/host/h.rs", ""),
        ("src/host/i.rs", ""),
        ("src/host/t.rs", ""),
    ]);
    assert_eq!(
        edges(&graph),
        [
            "a -> b", "a -> c", "a -> d", "a -> e", "a -> f", "a -> g", "a -> h", "a -> i"
        ]
    );
}

#[test]
fn paths_in_attributes_are_references() {
    // The test-only code of `a` names `t`.
    let graph = graph_of(&[
        ("src/lib.rs", "pub mod host;\n"),
        (
            "src/host/mod.rs",
            "mod a;\nmod b;\nmod c;\nmod d;\nmod e;\nmod f;\nmod t;\n",
        ),
        (
            "src/host/a.rs",
            r##"#[derive(Serialize)]
pub(crate) struct Serialized {
    #[cfg(test)]
    #[serde(with = "crate::host::t::format")]
    probe: u8,
    #[serde(serialize_with = "crate::host::b::serialize")]
    cooked: u8,
    #[serde(deserialize_with = r#"crate::host::c::deserialize"#)]
    raw: u8,
    #[serde(default = "crate::host::\u{64}::default")]
    escaped: u8,
    #[arg(value_parser = crate::host::t::parse)]
    #[cfg(test)]
    tested: u8,
    #[cfg_attr(all(test, unix), arg(value_parser = crate::host::t::parse))]
    #[arg(value_parser = crate::host::e::parse)]
    parsed: u8,
}
mod inner {
    #![attribute(crate::host::f::VALUE)]
    #[cfg(test)]
    fn probe() {}
}
"##,
        ),
        ("src/host/b.rs", ""),
        ("src/host/c.rs", ""),
        ("src/host/d.rs", ""),
        ("src/host/e.rs", ""),
        ("src/host/f.rs", ""),
        ("src/host/t.rs", ""),
    ]);
    assert_eq!(
        edges(&graph),
        ["a -> b", "a -> c", "a -> d", "a -> e", "a -> f"]
    );
}

#[test]
fn the_paths_in_the_input_of_a_macro_are_references() {
    // The input of a macro need not be Rust, may hold paths in strings that the macro puts in
    // attributes, and may lose its `cfg` attributes, while a template that names no path from a
    // module names only what is in scope where it expands, whatever its matcher matches.
    let graph = graph_of(&[
        ("src/lib.rs", "pub mod host;\n"),
        (
            "src/host/mod.rs",
            "mod a;\nmod b;\nmod c;\nmod d;\nmod e;\n",
        ),
        (
            "src/host/a.rs",
            r#"macro_rules! ignore {
    ($($tokens:tt)*) => {};
}
macro_rules! matches_a_path {
    (crate::host::b) => {};
}
macro_rules! field {
    ($attr:meta) => { struct S { #[$attr] value: u8 } };
}
macro_rules! strip {
    (#[cfg(test)] $item:item) => { $item };
}
macro_rules! keep {
    (#[cfg_attr(test, $attr:meta)] $item:item) => { #[$attr] $item };
}
ignore!(mod missing; use ?; crate::host::b::f());
field!(serde(serialize_with = "crate::host::c::serialize"));
strip!(#[cfg(test)] fn f() { crate::host::d::f(); });
keep!(#[cfg_attr(test, arg(value_parser = crate::host::e::parse))] struct T;);
"#,
        ),
        ("src/host/b.rs", ""),
        ("src/host/c.rs", ""),
        ("src/host/d.rs", ""),
        ("src/host/e.rs", ""),
    ]);
    assert_eq!(edges(&graph), ["a -> b", "a -> c", "a -> d", "a -> e"]);
}

#[test]
fn paths_through_the_imports_of_the_crate_root_and_host_reach_every_child_they_may_name() {
    let graph = graph_of(&[
        (
            "src/lib.rs",
            r#"pub mod host;
pub use host::Forwarded;
#[cfg(unix)]
pub use host::a::Either;
#[cfg(windows)]
pub use host::b::Either;
use host::c as aliased;
pub use aliased::Through;
mod other {
    mod inner {
        fn f() { crate::host::a::f(); }
    }
    pub(crate) use crate::host::c::Nested;
}
mod bridge {
    pub use crate::host::a::Bridged;
    pub use crate::host::b::Other;
    pub(crate) use crate::host::e as x;
}
pub use bridge::*;
use self::bridge as renamed;
pub use renamed::Other as Twice;
#[cfg(debug_assertions)]
use other as x;
"#,
        ),
        (
            "src/host/mod.rs",
            r#"mod a;
mod b;
mod c;
mod d;
mod e;
#[cfg(unix)]
mod g;
#[cfg(windows)]
use self::b as g;
mod h;
mod i;
mod j;
mod k;
mod l;
mod m;
mod n;
pub use self::a::Forwarded;
"#,
        ),
        ("src/host/a.rs", ""),
        ("src/host/b.rs", ""),
        ("src/host/c.rs", ""),
        ("src/host/d.rs", "use crate::{Either, Forwarded};\n"),
        ("src/host/e.rs", "fn f() { crate::Through::g(); }\n"),
        ("src/host/g.rs", ""),
        ("src/host/h.rs", "use super::g::T;\n"),
        ("src/host/i.rs", "use crate::other::Nested;\n"),
        ("src/host/j.rs", "use crate::Bridged;\n"),
        ("src/host/k.rs", "use crate::Twice;\n"),
        // In release builds, `crate::x` is the glob's `bridge::x`, and in debug builds `other`.
        ("src/host/l.rs", "fn f(_: crate::x::Nested) {}\n"),
        (
            "src/host/m.rs",
            "use crate::bridge::*;\nfn f(_: self::Bridged) {}\n",
        ),
        (
            "src/host/n.rs",
            "use crate::other;\nfn f(_: other::Nested) {}\n",
        ),
    ]);
    assert_eq!(
        edges(&graph),
        [
            "d -> a", "d -> b", "e -> c", "h -> b", "h -> g", "i -> c", "j -> a", "k -> b",
            "l -> c", "l -> e", "m -> a", "m -> b", "m -> e", "n -> c"
        ]
    );
}

#[test]
fn paths_through_aliases_and_the_crate_roots_other_modules_are_references() {
    let graph = graph_of(&[
        (
            "src/lib.rs",
            r#"pub mod host;
pub mod prelude;
extern crate self as nvx;
use host as aliased_host;
pub use aliased_host::a::ThroughHost;
pub use nvx::host::b::ThroughSelf;
mod bridge {
    pub(crate) use crate::host::c::C;
}
use bridge as alias;
use alias::*;
pub(crate) mod other {
    pub(crate) use crate::host::d::D;
}
mod nested {
    pub(crate) use crate::other as alias;
}
pub struct Alias;
fn helper() {
    use crate::host::a::Alias;
}
pub struct Negated;
fn negated() -> bool {
    if !{ use crate::host::b::Negated; true } {}
    false
}
mod outer {
    pub(crate) mod inner {
        pub(crate) use crate::bridge::*;
    }
}
"#,
        ),
        ("src/prelude.rs", "pub use crate::host::e::E;\n"),
        (
            "src/host/mod.rs",
            "mod a;\nmod b;\nmod c;\nmod d;\nmod e;\nmod f;\nmod g;\nmod h;\nmod i;\nmod j;\nmod k;\nmod \
             l;\nmod m;\n",
        ),
        ("src/host/a.rs", ""),
        ("src/host/b.rs", ""),
        ("src/host/c.rs", ""),
        ("src/host/d.rs", ""),
        ("src/host/e.rs", ""),
        ("src/host/f.rs", "use crate::ThroughHost;\n"),
        ("src/host/g.rs", "use crate::ThroughSelf;\n"),
        ("src/host/h.rs", "fn f(_: crate::C) {}\n"),
        (
            "src/host/i.rs",
            "use crate::nested;\nfn f(_: nested::alias::D) {}\n",
        ),
        ("src/host/j.rs", "use crate::prelude::*;\n"),
        // The root's `Alias` is its own: its function imports another for its body alone.
        ("src/host/k.rs", "fn f(_: crate::Alias) {}\n"),
        (
            "src/host/l.rs",
            "use crate::outer;\nfn f(_: outer::inner::C) {}\n",
        ),
        // So is its `Negated`, which a block that `!` negates, and no macro, imports.
        ("src/host/m.rs", "fn f(_: crate::Negated) {}\n"),
    ]);
    assert_eq!(
        edges(&graph),
        ["f -> a", "g -> b", "h -> c", "i -> d", "j -> e", "l -> c"]
    );
    // A module that imports host's children with a glob stands for every one of them.
    let graph = graph_of(&[
        (
            "src/lib.rs",
            "pub mod host;\nmod everything {\n    pub(crate) use crate::host::*;\n}\n",
        ),
        ("src/host/mod.rs", "mod a;\nmod b;\nmod c;\n"),
        (
            "src/host/a.rs",
            "use crate::everything;\nfn f() {\n    everything::b::f();\n}\n",
        ),
        ("src/host/b.rs", ""),
        ("src/host/c.rs", ""),
    ]);
    assert_eq!(edges(&graph), ["a -> b", "a -> c"]);
}

#[test]
fn paths_from_the_names_of_the_crate_root_in_the_extern_prelude_are_references() {
    // `a` names `nvx` before the crate root declares it, `host` may import any name with its
    // glob, `l` and `p` declare their own `nvx`, which `::` or some configurations bypass, `r`,
    // `s`, and `t` declare or import their own in every configuration, `u` only in a block, `w`
    // and `y` in test builds alone, and `alpha` in one of two definitions of a module.
    let graph = graph_of(&[
        (
            "src/lib.rs",
            r#"pub mod host {
    pub mod a {
        pub(crate) fn f() { nvx::host::b::f() }
    }
    mod b;
    mod c;
    mod d;
    mod e;
    mod f;
    mod g;
    mod h;
    mod i;
    mod j;
    mod k;
    mod l;
    mod m;
    mod n;
    mod o;
    mod p;
    mod q;
    mod r;
    mod s;
    mod t;
    mod u;
    mod v;
    mod w;
    mod x;
    mod y;
    mod z;
    mod alpha;
    mod beta;
    use crate::sys::*;
    pub(crate) use nvx::host::k::K as Alias;
}
pub mod sys {}
extern crate self as nvx;
"#,
        ),
        ("src/host/b.rs", ""),
        ("src/host/c.rs", "use nvx::host::d::D;\n"),
        ("src/host/d.rs", ""),
        (
            "src/host/e.rs",
            "pub(crate) fn make() -> ::nvx::host::f::F {\n    return ::nvx::host::g::make();\n}\n",
        ),
        ("src/host/f.rs", ""),
        ("src/host/g.rs", ""),
        (
            "src/host/h.rs",
            "pub(crate) struct H {\n    #[serde(with = \"nvx::host::i\")]\n    field: u8,\n}\n",
        ),
        ("src/host/i.rs", ""),
        ("src/host/j.rs", "fn f(_: crate::host::Alias) {}\n"),
        ("src/host/k.rs", ""),
        (
            "src/host/l.rs",
            "mod nvx {}\nfn f(_: ::nvx::host::m::M) {}\n",
        ),
        ("src/host/m.rs", ""),
        (
            "src/host/n.rs",
            "fn f(x: i32) -> bool { x > ::nvx::host::o::VALUE }\n",
        ),
        ("src/host/o.rs", ""),
        (
            "src/host/p.rs",
            "#[cfg(debug_assertions)]\nmod nvx {}\n#[cfg(not(debug_assertions))]\nfn f(_: \
             nvx::host::q::Q) {}\n",
        ),
        ("src/host/q.rs", ""),
        (
            "src/host/r.rs",
            "mod nvx {\n    pub(super) struct Local;\n}\nuse nvx as local;\nfn f(_: local::Local) {}\n",
        ),
        (
            "src/host/s.rs",
            "#[cfg_attr(debug_assertions, allow(dead_code))]\npub(crate) mod nvx {}\nuse nvx as \
             local;\n",
        ),
        (
            "src/host/t.rs",
            "use crate::sys as nvx;\nuse nvx as local;\n",
        ),
        (
            "src/host/u.rs",
            "fn f() {\n    use crate::sys as nvx;\n}\nfn g(_: nvx::host::v::V) {}\n",
        ),
        ("src/host/v.rs", ""),
        (
            "src/host/w.rs",
            "mod nvx {\n    #![cfg(test)]\n}\nfn f(_: nvx::host::x::X) {}\n",
        ),
        ("src/host/x.rs", ""),
        ("src/host/y.rs", "mod nvx;\nfn f(_: nvx::host::z::Z) {}\n"),
        ("src/host/y/nvx.rs", "#![cfg(test)]\n"),
        ("src/host/z.rs", ""),
        (
            "src/host/alpha.rs",
            "#[cfg(unix)]\nmod imp {\n    mod nvx {}\n}\n#[cfg(windows)]\nmod imp {\n    fn f() {\n        \
             nvx::host::beta::f();\n    }\n}\n",
        ),
        ("src/host/beta.rs", ""),
    ]);
    assert_eq!(
        edges(&graph),
        [
            "a -> b",
            "alpha -> beta",
            "c -> d",
            "e -> f",
            "e -> g",
            "h -> i",
            "j -> k",
            "l -> m",
            "n -> o",
            "p -> q",
            "u -> v",
            "w -> x",
            "y -> z"
        ]
    );
}

#[test]
fn long_paths_are_followed() {
    let modules = (0..70).fold("pub(crate) struct T;".to_owned(), |inner, _| {
        format!("pub(crate) mod m {{ {inner} }}")
    });
    let path = format!("fn f(_: crate::host::b::{}T) {{}}", "m::".repeat(70));
    let graph = graph_of(&[
        ("src/lib.rs", "pub mod host;\n"),
        ("src/host/mod.rs", "mod a;\nmod b;\n"),
        ("src/host/a.rs", path.as_str()),
        ("src/host/b.rs", modules.as_str()),
    ]);
    assert_eq!(edges(&graph), ["a -> b"]);
}

#[test]
fn what_the_test_cannot_follow_is_refused() {
    let cases: &[(&[(&str, &str)], &str)] = &[
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                (
                    "src/host/mod.rs",
                    "#[cfg_attr(windows, path = \"a_windows.rs\")]\nmod a;\n",
                ),
                ("src/host/a.rs", ""),
            ],
            "src/host/mod.rs:1: the test does not follow #[path] attributes",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                ("src/host/mod.rs", "#[r#path = \"renamed.rs\"]\nmod a;\n"),
                ("src/host/a.rs", ""),
            ],
            "src/host/mod.rs:1: the test does not follow #[path] attributes",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\npub use host::b::*;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                ("src/host/a.rs", ""),
                ("src/host/b.rs", ""),
            ],
            "src/lib.rs:2: the test cannot resolve the names of glob imports of host's children",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "use crate::host as aliased;\nfn f(_: aliased::b::B) {}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:1: the test cannot follow imports of the crate root or of host in \
             host's children",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\nextern crate self as nvx;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "use crate::nvx as root;\nfn f(_: root::host::b::B) {}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:1: the test cannot follow imports of the crate root or of host in \
             host's children",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "macro_rules! call_b {\n    () => { super::super::b::value(); };\n}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:2: the test does not follow macro templates",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\nextern crate self as nvx;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "macro_rules! call_b {\n    () => { nvx::host::b::value(); };\n}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:2: the test does not follow macro templates",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "macro_rules! call {\n    ($root:ident) => { $root::host::b::value(); };\n}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:2: the test does not follow macro templates",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "macro_rules! call {\n    ($($root:tt)*) => { $($root)*::host::b::value(); \
                     };\n}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:2: the test does not follow macro templates",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "macro_rules! call {\n    ($($segment:ident)*) => { $($segment)::*(); };\n}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:2: the test does not follow macro templates",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "macro_rules! call {\n    ($separator:tt) => { crate $separator host::b::value(); \
                     };\n}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:2: the test does not follow macro templates",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "macro_rules! call {\n    ($($root:tt)*) => { $($root)* host::b::value(); };\n}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:2: the test does not follow macro templates",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "macro_rules! call {\n    ($root:tt, $($tail:tt)*) => { $root $($tail)*(); };\n}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:2: the test does not follow macro templates",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "macro_rules! call {\n    ($root:ident, $($segment:ident)*) => { $root \
                     $(::$segment)*(); };\n}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:2: the test does not follow macro templates",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "macro_rules! field {\n    ($with:literal) => { struct S { #[serde(with = \
                     $with)] value: u8 } };\n}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:2: the test does not follow macro templates",
        ),
        (
            &[
                ("src/lib.rs", "pub mod host;\n"),
                ("src/host/mod.rs", "mod a;\nmod b;\n"),
                (
                    "src/host/a.rs",
                    "macro_rules! field {\n    ($attr:ident) => { #[$attr(with = \
                     \"crate::host::b\")] struct S; };\n}\n",
                ),
                ("src/host/b.rs", ""),
            ],
            "src/host/a.rs:2: the test does not follow macro templates",
        ),
    ];
    for (files, expected) in cases {
        let Err(payload) = panic::catch_unwind(|| graph_of(files)) else {
            panic!("the test followed the crate that it should refuse with {expected:?}");
        };
        let refusal = payload.downcast_ref::<String>().map_or("", String::as_str);
        assert!(
            refusal.starts_with(expected),
            "{refusal:?} does not start with {expected:?}"
        );
    }
}

#[test]
fn each_cycle_is_listed_with_one_reference_per_edge() {
    let graph = graph_of(&[
        ("src/lib.rs", "pub mod host;\nextern crate self as nvx;\n"),
        ("src/host/mod.rs", "mod a;\nmod b;\nmod c;\n"),
        ("src/host/a.rs", "use super::b::{B, Other};\n"),
        (
            "src/host/b.rs",
            "use super::c::C;\nfn f() {\n    crate::host::a::f();\n}\n",
        ),
        ("src/host/c.rs", "use ::nvx::host::a::A;\n"),
    ]);
    let cycles = cycles(&graph);
    assert_eq!(cycles, [vec!["a", "b"], vec!["a", "b", "c"]]);
    let report = report(&graph, &cycles);
    for line in [
        "a -> b -> a",
        "    a -> b: src/host/a.rs:1: super::b::B",
        "    b -> a: src/host/b.rs:3: crate::host::a::f",
        "a -> b -> c -> a",
        "    b -> c: src/host/b.rs:1: super::c::C",
        "    c -> a: src/host/c.rs:1: ::nvx::host::a::A",
    ] {
        assert!(
            report.lines().any(|reported| reported == line),
            "{line:?} is not in:\n{report}"
        );
    }
}
