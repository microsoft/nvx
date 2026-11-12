//! The layering of `nvx::host`: its direct children depend on one another without cycles, so that
//! each builds only on those beneath it.
//!
//! The test reads the crate's sources as text, with the standard library alone, from `src/lib.rs`
//! through the files of the modules that it declares. It blanks their comments and literals, and
//! leaves out test-only code: what a `cfg` that requires `test` guards, with the attributes of that
//! code and the files of the modules that it declares, and `#[test]` functions. It leaves out
//! guarded code only as far as its tokens show where that code ends: an item, a statement that ends
//! with a `;`, a macro invocation, a block, `if`, `match`, or loop, where the start of the file or
//! a `{`, `}`, or `;` before it shows that one starts; a field, parameter, variant, or arm of the
//! list that declares it; and an element of another list, such as a field of a struct expression or
//! a parameter of generics or of a closure, up to its first `,`, `>`, `|`, or the end of its list,
//! past any `<` that compares or opens type arguments. The input of a macro, which need not be
//! Rust, and whose attributes the macro may keep or drop, is production code throughout, and every
//! path in its tokens counts, from any of them, past a `::` before it, however the test reads the
//! rest of it, as does every module that it declares in a file of its own. So the test may find a
//! dependency in test-only code, but never misses one in production code.
//!
//! A child depends on another when its production code names a path into the other: a `crate::`,
//! `super::`, or `self::` path, or one from a name that `extern crate self as NAME;` at the top
//! level of the crate root's file gives the crate root in the extern prelude, unless the module,
//! defined once, declares a module or crate of that name, or imports one of the crate's modules
//! under it, through modules, imports by name, and names that such a declaration without a `cfg`
//! gives, in every configuration. The path may be in a `use` tree, an expression, an attribute, or,
//! with its escapes decoded, a string of an attribute or of a macro's input, such as serde's
//! `with = "..."`, or a doc comment in a macro's input, of which Rust makes such a string, but not
//! in documentation outside the input of a macro, such as `#[doc = "..."]` or a doc comment. It may
//! go through the names that the modules outside the children, such as the crate root and `host`,
//! bind with their imports, by name, under an alias, or with a glob, in any order and under any
//! configuration: it depends on each child that any of those imports may take it from. A child's
//! import of such a module, which its own paths may then go through, depends on every child that
//! the module's names stand for. A macro that a child exports with `#[macro_export]` is an item of
//! the crate root, as `crate::NAME!`, which that child holds, among the names of macros alone: a
//! path names it with the `!` of an invocation, and an import of its name takes it beside other
//! items of that name. A raw identifier, such as `r#type`, names what `type` would, in paths, in
//! the names of attributes, and in those of metavariables and their fragments alike. A path within
//! a child, or to `host` itself, is no dependency. Since the test reads text, the code of every
//! target and feature counts.
//!
//! A cycle fails the test, which lists, for each dependency on one, the shortest cycle through it,
//! with the first reference along each of its dependencies, under how to break it. Without one, the
//! children stack in layers, each above those that it depends on, which the test prints with
//! `--nocapture`.
//!
//! Code whose dependencies the test cannot follow fails it too: a `#[path]` attribute anywhere; an
//! identifier that is not ASCII, in code or in a path that the test reads in a string or a doc
//! comment, which Rust takes for any spelling that Unicode's NFC normalization makes alike, while
//! the test compares spellings; a file beneath `src/host/` that no `mod` declares; code of another
//! file that `include!` puts in a module, under any name that imports may give it, or through a
//! path, which may name it whatever the crate defines; an import of `host` or the crate root
//! itself, under any name, in a child, such as `extern crate self as this;`, whose names would hide
//! the children that the code uses; a glob import of a child into a module outside them; a path
//! through imports that alias one another in a cycle that lengthens it, which imports of different
//! configurations may make; an inline module in the input of a macro, which may put the module's
//! code in another module, or an alias of the crate root in the input of one in the crate root,
//! which may join the extern prelude; a module that the input of a macro declares in a file of its
//! own, where the input of a macro or a template holds a `path` or a `macro_use` anywhere, of which
//! a template, maybe through other macros, may make an attribute that sets the module's file or
//! brings its macros into scope, which the test does not see; a `macro_export` anywhere in the
//! input of a macro or in a template, of which a template may make an attribute that exports a
//! macro at the crate root, from where the test cannot tell; a `#[macro_use]` attribute in `host`,
//! whose macros code would use without a path; and the template of a `macro_rules!` that declares
//! or imports, that holds an attribute that sets a module's file or brings macros into scope, that
//! aliases the crate root or a module above it, as `$kind {crate as this}` may, that names or may
//! join a path from a module or from the macro's input, past any `::` before it in the input of a
//! macro that it invokes, or that holds one in a string, or in a doc comment in the input of a
//! macro that it invokes, but documentation outside the input of a macro that it invokes, which
//! resolve where the macro expands, that puts another file's code in with `include!`, that invokes
//! a macro that its input names, which may be `include!`, that puts `extern` before an identifier
//! or a token tree of its input, which may complete `extern crate`, even past a visibility of its
//! input, which may be empty, that may make a keyword that declares of an identifier of its input,
//! after a visibility of its input or none, or an item, a statement, a block, or an expression of
//! its input, or at the end of a repetition, or anywhere in the input of a macro that it invokes,
//! which may take any part of it for an item, as `$vis $kind $name { ... }` may be
//! `mod name { ... }`, and before a group that may expand to the group of an import, as
//! `$kind { $path }` may to `use { crate::host }`, unlike `$name { field: value }` or
//! `$name { $($field: $value),* }`, or before the token that follows it when a repetition that it
//! ends repeats, as `$($kind)*` may be `use nvx`, or that invokes a macro by name under
//! `local_inner_macros`, which names one of the crate root. The test finds these in the input of a
//! macro wherever they are, however it reads the rest of the input.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// A file that the test reads.
struct Source {
    /// The file's path in the crate, with `/` separators, such as `src/host/platform/mod.rs`.
    path: String,
    /// The file's text.
    text: String,
}

/// A token of blanked source text.
#[derive(Clone, Copy, Debug)]
struct Token<'a> {
    /// An identifier, keyword, or number; `::` or `=>`; or another punctuation character.
    text: &'a str,
    /// The token's line, from 1.
    line: usize,
    /// The index of the token's first byte in the text.
    offset: usize,
}

/// A path that a file writes: in a `use` tree, or a `crate::`, `super::`, or `self::` path
/// elsewhere, also in an attribute or in a string of one.
#[derive(Debug)]
struct Reference {
    /// The module in which the path is written, from the crate root.
    module: Vec<String>,
    /// The path's segments, as written, with the prefixes of its `use` tree.
    segments: Vec<String>,
    /// How the path imports what it names, if a `use` declaration holds it.
    kind: Kind,
    /// The line of the path's last segment.
    line: usize,
    /// Whether test-only code of its file holds the path.
    test: bool,
    /// Whether a function body or another block holds the path, rather than modules alone.
    local: bool,
}

/// A path that code writes outside `use` trees: its segments, as written, and the line of its
/// last segment.
type WrittenPath = (Vec<String>, usize);

/// How a path imports what it names.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// It imports nothing: it is not in a `use` declaration.
    Path,
    /// A `use` declaration imports what the path names, under a name, or under none for `as _`.
    Import(Option<String>),
    /// A `use` declaration imports the contents of what the path names, through a glob.
    Glob,
}

/// What the test finds in a file.
#[derive(Debug, Default)]
struct Facts {
    /// The modules that `mod NAME;` declarations place in files of their own, each with whether
    /// test-only code declares it.
    declared: Vec<(Vec<String>, bool)>,
    /// The modules of the file's inline `mod NAME { ... }` blocks, each with whether test-only code
    /// declares it.
    inline: Vec<(Vec<String>, bool)>,
    /// The file's references.
    references: Vec<Reference>,
    /// The lines of the file's `#[path]` attributes.
    path_attributes: Vec<usize>,
    /// The lines of the file's `#[macro_use]` attributes, each with the module in which it is
    /// written.
    macro_uses: Vec<(Vec<String>, usize)>,
    /// The macros that the file exports at the crate root with `#[macro_export]`, each with the
    /// module that defines it and whether test-only code exports it.
    exported: Vec<(Vec<String>, String, bool)>,
    /// The lines of the templates of the file's `macro_rules!` that the test cannot follow, each
    /// with whether test-only code defines the macro.
    templates: Vec<(usize, bool)>,
    /// The lines of the file's `include!` invocations, each with whether test-only code holds it.
    includes: Vec<(usize, bool)>,
    /// The lines of the inline modules that the input of a macro declares, each with whether
    /// test-only code holds it.
    input_modules: Vec<(usize, bool)>,
    /// The lines of the aliases of the crate root that the input of a macro in the crate root
    /// declares, which may join the extern prelude, each with whether test-only code holds it.
    input_prelude: Vec<(usize, bool)>,
    /// The lines of the `macro_export`s that the input of a macro holds, wherever they are, of
    /// which a template may make an attribute that exports a macro, each with whether test-only
    /// code holds it.
    input_exports: Vec<(usize, bool)>,
    /// The lines of the modules in files of their own that the input of a macro declares, each
    /// with whether test-only code holds it.
    input_files: Vec<(usize, bool)>,
    /// The lines of the `path`s and `macro_use`s that the input of a macro or a template holds,
    /// of which a template, maybe through other macros, may make an attribute of a module that an
    /// input declares, each with whether test-only code holds it.
    attribute_names: Vec<(usize, bool)>,
    /// The line of the file's first identifier that is not ASCII, if any, in its code or in a path
    /// that the test reads in a string or a doc comment, which Rust may take for another spelling
    /// of it.
    non_ascii: Option<usize>,
    /// The names that each module of the file declares or imports in production code, in every
    /// configuration, outside blocks and the input of macros, each with the module and, for an
    /// import, the path that it imports, which may name a macro or another item alone.
    unconditional: Vec<(Vec<String>, String, Option<Vec<String>>)>,
    /// The inline modules whose own inner attributes hold a `cfg`, which exist only in some
    /// configurations.
    conditional: Vec<Vec<String>>,
    /// Whether the file's inner attributes hold a `cfg`, as for `conditional`.
    inner_cfg: bool,
}

impl Facts {
    /// Adds `paths`, each with its segments and the line of its last segment, which `module`
    /// writes outside `use` trees, to the references: as test-only ones if `test`, and as ones
    /// that a block holds if `local`. A segment that is not ASCII, which only a string spells
    /// here, fails the test as an identifier of the code does.
    fn add_paths(&mut self, module: &[String], paths: Vec<WrittenPath>, test: bool, local: bool) {
        for (segments, line) in paths {
            if !test && segments.iter().any(|segment| !segment.is_ascii()) {
                self.non_ascii = Some(self.non_ascii.map_or(line, |first| first.min(line)));
            }
            self.references.push(Reference {
                module: module.to_vec(),
                segments,
                kind: Kind::Path,
                line,
                test,
                local,
            });
        }
    }
}

/// A bracketed region of a file that the parser is in.
struct Frame {
    /// Whether the region's code is test-only.
    test: bool,
    /// The name of the inline module whose body the region is, if it is one.
    module: Option<String>,
    /// What the region lists.
    list: List,
    /// Whether the region is in the input of a macro, which need not be Rust, and whose
    /// attributes the macro may keep or drop as it pleases.
    input: bool,
    /// The names of the `macro_rules!` that the region defines in every configuration, which
    /// hide the macros of the same names from the rest of it.
    macros: BTreeSet<String>,
}

/// What a bracketed region lists, which tells where test-only code in it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum List {
    /// The fields of a struct, union, or variant, or the parameters of a function: each ends at a
    /// comma outside the angle brackets of its type.
    Fields,
    /// The variants of an enum: each ends at a comma.
    Variants,
    /// The arms of a `match`: each ends after the block that follows its `=>`, or at a comma.
    Arms,
    /// Items, statements, or expressions.
    Code,
}

/// The dependencies between the direct children of `host`.
#[derive(Debug, Default)]
struct Graph {
    /// The children of `host` that production code declares.
    children: BTreeSet<String>,
    /// For each dependency, from a child to another, the first reference that makes it, in the
    /// order of the files' paths and their lines.
    edges: BTreeMap<(String, String), Example>,
}

/// A reference that makes a dependency.
#[derive(Debug)]
struct Example {
    /// The file and line of the reference, such as `src/host/platform/mod.rs:242`.
    location: String,
    /// The path that the reference writes, such as `super::spawn::Launched`.
    path: String,
}

/// The modules of the crate, and what the modules outside the children of `host`, such as the
/// crate root and `host` itself, import, through which a path may name a child.
#[derive(Default)]
struct Modules {
    /// Every module that a file, an inline `mod` block, or a `mod` declaration makes, from the
    /// crate root, which is the empty path.
    paths: BTreeSet<Vec<String>>,
    /// The children of `host` that production code declares.
    children: BTreeSet<String>,
    /// The names that `extern crate self as NAME;` in the crate root's file gives the crate root
    /// in the extern prelude, where every module may name it.
    prelude: BTreeSet<String>,
    /// The names that each module declares or imports in every configuration, outside blocks and
    /// the input of macros, which hide those of the extern prelude.
    unconditional: BTreeMap<Vec<String>, BTreeSet<String>>,
    /// The number of definitions that production code makes of each module: more than one for
    /// alternatives under different configurations.
    definitions: BTreeMap<Vec<String>, usize>,
    /// The paths that each name that a module outside the children binds with an import, by the
    /// module's path and the name, may stand for, under any configuration. The name of a macro
    /// ends with a `!`, as `shared!` does, so that it hides no module or other item of its name.
    names: BTreeMap<Vec<String>, BTreeSet<Vec<String>>>,
    /// The modules whose names each module outside the children imports with a glob.
    globs: BTreeMap<Vec<String>, BTreeSet<Vec<String>>>,
    /// The first path that a rewrite that recurs would lengthen, as imports that alias one another
    /// in a cycle that grows, which no configuration holds but several may, would make it, which
    /// the test therefore cannot follow to an end.
    cut: RefCell<Option<Vec<String>>>,
}

/// The questions whether a module provides a name, by the module and the name, that an answer is
/// pending for, so that a cycle of glob imports, which Rust allows, ends.
type Questions = BTreeSet<(Vec<String>, String)>;

/// What tells the test the module that an import's path names in every configuration: modules
/// that every configuration has, and names that imports bind in every configuration, through which
/// it follows the path, but no glob, whose names depend on visibility, which the test does not
/// read.
struct Certain<'a> {
    /// The modules that every configuration has.
    always: &'a BTreeSet<Vec<String>>,
    /// The paths that the imports of each name that a module binds in every configuration take, by
    /// the module and the name.
    bound: &'a BTreeMap<(Vec<String>, String), Vec<Vec<String>>>,
    /// The names that each module may bind with a declaration or an import in some configuration,
    /// by the module and the name, which hide those of the extern prelude there.
    bindable: &'a BTreeSet<(Vec<String>, String)>,
    /// The names that `extern crate self as NAME;` without a `cfg` gives the crate root in the
    /// extern prelude, in every configuration. One that a `cfg` guards may name another crate in
    /// other configurations, where an import through it may take a macro alone.
    prelude: &'a BTreeSet<String>,
}

impl Certain<'_> {
    /// Returns the module that `segments`, which an import in `module` takes, names in every
    /// configuration, if the test can tell. `active` holds the names that the imports that lead
    /// here bind, which a crate that builds does not go through again.
    fn module(
        &self,
        module: &[String],
        segments: &[String],
        active: &mut Vec<(Vec<String>, String)>,
    ) -> Option<Vec<String>> {
        let (first, rest) = segments.split_first()?;
        let (mut named, rest) = match first.as_str() {
            "crate" => (Vec::new(), rest),
            "self" | "super" => {
                let rest = if first == "self" { rest } else { segments };
                let supers = rest
                    .iter()
                    .take_while(|segment| *segment == "super")
                    .count();
                let kept = module.len().checked_sub(supers)?;
                (module[..kept].to_vec(), &rest[supers..])
            }
            "::" => match rest.split_first() {
                Some((name, rest)) if self.prelude.contains(name) => (Vec::new(), rest),
                _ => return None,
            },
            // A name of the extern prelude that the module binds in no configuration is the
            // extern prelude's, as one that a glob of the module may bring would be ambiguous.
            _ if self.prelude.contains(first)
                && !self.bindable.contains(&(module.to_vec(), first.clone())) =>
            {
                (Vec::new(), rest)
            }
            _ => (module.to_vec(), segments),
        };
        for segment in rest {
            let next = [named.as_slice(), std::slice::from_ref(segment)].concat();
            if self.always.contains(&next) {
                named = next;
                continue;
            }
            let key = (named, segment.clone());
            if active.contains(&key) {
                return None;
            }
            let imports = self.bound.get(&key)?;
            let scope = key.0.clone();
            active.push(key);
            let found = imports
                .iter()
                .find_map(|imported| self.module(&scope, imported, active));
            active.pop();
            named = found?;
        }
        Some(named)
    }
}

/// Returns `path` as the path of a macro, whose last segment ends with a `!`.
fn as_macro(path: &[String]) -> Vec<String> {
    let mut path = path.to_vec();
    if let Some(name) = path.last_mut() {
        name.push('!');
    }
    path
}

/// Returns `segments` as code writes them, such as `::nvx::host` for `["::", "nvx", "host"]`, with
/// `::*` after them for a `glob`.
fn written(segments: &[String], glob: bool) -> String {
    let mut written = match segments.split_first() {
        Some((first, rest)) if first == "::" => format!("::{}", rest.join("::")),
        _ => segments.join("::"),
    };
    if glob {
        written.push_str("::*");
    }
    written
}

impl Modules {
    /// Returns the child of `host` whose code `module` is, if it is one of them or inside one.
    fn child_of(&self, module: &[String]) -> Option<String> {
        match module {
            [host, child, ..] if host == "host" && self.children.contains(child) => {
                Some(child.clone())
            }
            _ => None,
        }
    }

    /// Returns the paths from the crate root that `segments`, written in `module`, may name: one
    /// from `crate`; from `self` or `super`, with each `super` after them; from a name that the
    /// extern prelude gives the crate root, after `::`, or unless `module`, which a single
    /// definition makes, declares or imports that name in every configuration; and from a name
    /// that `module` declares, binds, or may take from a glob.
    fn resolve(&self, module: &[String], segments: &[String]) -> Vec<Vec<String>> {
        let Some((first, rest)) = segments.split_first() else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        match first.as_str() {
            "crate" => paths.push(rest.to_vec()),
            "self" | "super" => {
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
                    && self.prelude.contains(name)
                {
                    paths.push(rest.to_vec());
                }
            }
            _ => {
                let named = |name: String| {
                    let path = [module, &[name]].concat();
                    self.paths.contains(&path) || self.names.contains_key(&path)
                };
                if named(first.clone())
                    || named(format!("{first}!"))
                    || self.globs.contains_key(module)
                {
                    paths.push([module, segments].concat());
                }
                let shadowed = self.definitions.get(module).is_none_or(|&count| count == 1)
                    && self
                        .unconditional
                        .get(module)
                        .is_some_and(|names| names.contains(first));
                if self.prelude.contains(first) && !shadowed {
                    paths.push(rest.to_vec());
                }
            }
        }
        paths
    }

    /// Adds to `paths` every path that `path` may stand for: itself, and, for each name that it
    /// goes through, what the name stands for, and the same name in each module that the name's
    /// module imports with a glob, if that module provides it.
    fn expand(&self, path: &[String], paths: &mut BTreeSet<Vec<String>>, asking: &mut Questions) {
        self.expand_past(path, paths, asking, &mut Vec::new());
    }

    /// Implements [`expand`](Self::expand) past the rewrites that `active` holds, which those that
    /// lead to `path` made, each with the length of the path that it rewrote: by the name that it
    /// rewrote, or by its glob's module, `*`, and the name. A rewrite that recurs on a path no
    /// longer than the one that it last rewrote, as through a module's import of itself, is made
    /// again. The rewrites then end: each recurs on paths of lengths that never grow, and the test
    /// expands no path twice, of which there are only so many of each length. One that would
    /// lengthen the path, as imports of different configurations that alias one another in a cycle
    /// may make it forever, is not, and the test records the path in `cut`, which fails it.
    fn expand_past(
        &self,
        path: &[String],
        paths: &mut BTreeSet<Vec<String>>,
        asking: &mut Questions,
        active: &mut Vec<(Vec<String>, usize)>,
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
        let follows = |active: &[(Vec<String>, usize)], key: &[String]| {
            let lengthens = active
                .iter()
                .rev()
                .find(|(rewritten, _)| rewritten == key)
                .is_some_and(|&(_, length)| path.len() > length);
            if lengthens {
                self.cut.borrow_mut().get_or_insert_with(|| path.to_vec());
            }
            !lengthens
        };
        for length in 1..=path.len() {
            let name = &path[..length];
            if let Some(targets) = self.names.get(name)
                && follows(active, name)
            {
                active.push((name.to_vec(), path.len()));
                for target in targets {
                    let alternative = [target.as_slice(), &path[length..]].concat();
                    self.expand_past(&alternative, paths, asking, active);
                }
                active.pop();
            }
            let (module, last) = (&path[..length - 1], &path[length - 1]);
            for source in self.globs.get(module).into_iter().flatten() {
                let key = [module, &["*".to_owned(), last.clone()]].concat();
                if self.provides(source, last, asking) && follows(active, &key) {
                    active.push((key, path.len()));
                    let alternative = [source.as_slice(), &path[length - 1..]].concat();
                    self.expand_past(&alternative, paths, asking, active);
                    active.pop();
                }
            }
        }
    }

    /// Returns whether `module`, or what it may stand for, declares or binds `name`, directly or
    /// through the modules that it imports with a glob. A question that `asking` holds already is
    /// part of a cycle, which provides nothing that its other answers do not.
    fn provides(&self, module: &[String], name: &str, asking: &mut Questions) -> bool {
        let question = (module.to_vec(), name.to_owned());
        if !asking.insert(question.clone()) {
            return false;
        }
        let mut aliases = BTreeSet::new();
        self.expand(module, &mut aliases, asking);
        let provided = aliases.iter().any(|module| {
            let named = [module.as_slice(), &[name.to_owned()]].concat();
            self.paths.contains(&named)
                || self.names.contains_key(&named)
                || self
                    .globs
                    .get(module)
                    .into_iter()
                    .flatten()
                    .any(|source| self.provides(source, name, asking))
        });
        asking.remove(&question);
        provided
    }

    /// Returns the children of `host` that `path`, from the crate root, may name, through what it
    /// may stand for.
    fn targets(&self, path: &[String]) -> BTreeSet<String> {
        let mut paths = BTreeSet::new();
        self.expand(path, &mut paths, &mut Questions::new());
        paths
            .iter()
            .filter_map(|path| self.child_of(path))
            .collect()
    }

    /// Returns the children that the names under the module that `path` may name may stand for:
    /// what an import of the module, or a glob of it, brings into a child, whose paths through
    /// them the test does not follow. They are the children that the modules under it are or are
    /// in, those that the names that it and those modules bind stand for, and those under the
    /// modules that those names and its globs stand for in turn.
    fn names_under(&self, path: &[String]) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        let mut pending = vec![path.to_vec()];
        let mut seen = BTreeSet::new();
        while let Some(path) = pending.pop() {
            let mut modules = BTreeSet::new();
            self.expand(&path, &mut modules, &mut Questions::new());
            for module in modules {
                if !seen.insert(module.clone()) {
                    continue;
                }
                for name in self
                    .names
                    .keys()
                    .filter(|name| name.len() > module.len() && name.starts_with(&module))
                {
                    found.extend(self.targets(name));
                    pending.push(name.clone());
                }
                pending.extend(self.globs.get(&module).into_iter().flatten().cloned());
                for declared in self.paths.iter().filter(|declared| {
                    declared.len() == module.len() + 1 && declared.starts_with(&module)
                }) {
                    found.extend(self.child_of(declared));
                    pending.push(declared.clone());
                }
            }
        }
        found
    }
}

/// Reads the crate's sources: the files beneath `src/`, but for those of its binaries, in
/// `src/bin/`, which are crates of their own.
fn read_sources() -> Vec<Source> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    add_files(&root.join("src"), &root.join("src").join("bin"), &mut paths);
    paths
        .iter()
        .map(|path| Source {
            path: path
                .strip_prefix(root)
                .unwrap()
                .components()
                .map(|component| component.as_os_str().to_str().unwrap())
                .collect::<Vec<_>>()
                .join("/"),
            text: fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display())),
        })
        .collect()
}

/// Adds the paths of the `.rs` files beneath `directory`, but not beneath `skipped`, to `paths`.
fn add_files(directory: &Path, skipped: &Path, paths: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path != skipped {
                add_files(&path, skipped, paths);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            paths.push(path);
        }
    }
}

/// The keywords, which may precede a path, or a `!` that is no macro's, but never name one,
/// unlike `crate`, `self`, `Self`, and `super`, which are segments of paths.
const KEYWORDS: [&str; 48] = [
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "do", "dyn",
    "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl", "in", "let",
    "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref", "return",
    "static", "struct", "trait", "true", "try", "type", "typeof", "unsafe", "unsized", "use",
    "virtual", "where", "while", "yield",
];

/// Returns whether the token `text` is an identifier that no keyword is, such as a macro's name
/// or a path's segment. A raw identifier, such as `r#if`, is one.
fn is_name(text: &str) -> bool {
    is_identifier(text) && !KEYWORDS.contains(&text)
}

/// Returns whether `byte` can be part of an identifier, a keyword, or a number.
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || !byte.is_ascii()
}

/// Returns whether `text`, a token, is an identifier or a keyword.
fn is_identifier(text: &str) -> bool {
    text.bytes()
        .next()
        .is_some_and(|byte| is_word_byte(byte) && !byte.is_ascii_digit())
}

/// Returns `text` with its comments and literals blanked: every byte of them but line feeds
/// becomes a space, so that the code around them keeps its lines.
fn blank(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut blanked = bytes.to_vec();
    let mut at = 0;
    while at < bytes.len() {
        match literal_end(bytes, at) {
            Some(end) => {
                for byte in &mut blanked[at..end] {
                    if *byte != b'\n' {
                        *byte = b' ';
                    }
                }
                at = end;
            }
            None => at += 1,
        }
    }
    String::from_utf8(blanked).expect("blanking replaces whole characters")
}

/// Returns the end of the comment, string, or character literal that starts at `at`, if one
/// does: a line, doc, or nested block comment; a string, byte string, or C string, with escapes,
/// or a raw one of them with any number of `#`; or a character or byte literal, but not a
/// lifetime or a label.
fn literal_end(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = &bytes[at..];
    if rest.starts_with(b"//") {
        return Some(find(bytes, at, b"\n").unwrap_or(bytes.len()));
    }
    if rest.starts_with(b"/*") {
        return Some(block_comment_end(bytes, at));
    }
    match rest {
        [b'"', ..] => Some(quoted_end(bytes, at + 1)),
        [b'\'', ..] => character_end(bytes, at),
        // A prefix such as `b` or `r` starts a literal only where no identifier ends.
        _ if at > 0 && is_word_byte(bytes[at - 1]) => None,
        [b'b' | b'c', b'"', ..] => Some(quoted_end(bytes, at + 2)),
        [b'b', b'\'', ..] => character_end(bytes, at + 1),
        [b'r', ..] => raw_end(bytes, at + 1),
        [b'b' | b'c', b'r', ..] => raw_end(bytes, at + 2),
        _ => None,
    }
}

/// Returns the position of the first `needle` at or after `from` in `bytes`.
fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| from + position)
}

/// Returns the end of the block comment that starts at `start`, after the `*/` that closes it,
/// with the comments nested in it.
fn block_comment_end(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0;
    let mut at = start;
    while at < bytes.len() {
        if bytes[at..].starts_with(b"/*") {
            depth += 1;
            at += 2;
        } else if bytes[at..].starts_with(b"*/") {
            depth -= 1;
            at += 2;
            if depth == 0 {
                return at;
            }
        } else {
            at += 1;
        }
    }
    bytes.len()
}

/// Returns the end of the string whose text starts at `start`, after its closing quote, which
/// no backslash escapes.
fn quoted_end(bytes: &[u8], start: usize) -> usize {
    let mut at = start;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return at + 1,
            _ => at += 1,
        }
    }
    bytes.len()
}

/// Returns the end of the character literal whose opening quote is at `quote`, or `None` if the
/// quote starts a lifetime or a label, such as `'a` or `'outer`.
fn character_end(bytes: &[u8], quote: usize) -> Option<usize> {
    let first = *bytes.get(quote + 1)?;
    if first == b'\\' {
        // The escaped character follows the backslash, so the closing quote comes after it.
        return Some(find(bytes, quote + 3, b"'").map_or(bytes.len(), |close| close + 1));
    }
    let width = match first {
        0xf0..=0xff => 4,
        0xe0..=0xef => 3,
        0xc0..=0xdf => 2,
        _ => 1,
    };
    (bytes.get(quote + 1 + width) == Some(&b'\'')).then_some(quote + 2 + width)
}

/// Returns the end of the raw string whose `#` signs, if any, start at `start`, or `None` if no
/// raw string starts there, as in a raw identifier such as `r#type`.
fn raw_end(bytes: &[u8], start: usize) -> Option<usize> {
    let hashes = bytes[start..]
        .iter()
        .take_while(|&&byte| byte == b'#')
        .count();
    let open = start + hashes;
    if bytes.get(open) != Some(&b'"') {
        return None;
    }
    let mut close = vec![b'"'];
    close.resize(1 + hashes, b'#');
    Some(find(bytes, open + 1, &close).map_or(bytes.len(), |at| at + close.len()))
}

/// Splits blanked text into tokens. A raw identifier, such as `r#type`, is one token, and so are a
/// lifetime and a label, such as `'static` and `'outer`, whose quotes blanking left.
fn tokenize(text: &str) -> Vec<Token<'_>> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut line = 1;
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'\n' {
            line += 1;
            at += 1;
            continue;
        }
        if byte.is_ascii_whitespace() {
            at += 1;
            continue;
        }
        let word = |from: usize| {
            bytes[from..]
                .iter()
                .take_while(|&&byte| is_word_byte(byte))
                .count()
        };
        let raw = bytes[at..].starts_with(b"r#")
            && bytes
                .get(at + 2)
                .is_some_and(|&next| is_word_byte(next) && !next.is_ascii_digit());
        let length = if raw {
            2 + word(at + 2)
        } else if byte == b'\'' && bytes.get(at + 1).copied().is_some_and(is_word_byte) {
            1 + word(at + 1)
        } else if is_word_byte(byte) {
            word(at)
        } else if bytes[at..].starts_with(b"::") || bytes[at..].starts_with(b"=>") {
            2
        } else {
            1
        };
        tokens.push(Token {
            text: &text[at..at + length],
            line,
            offset: at,
        });
        at += length;
    }
    tokens
}

/// Returns the name that the identifier `token` makes, without the `r#` of a raw identifier.
fn unraw(token: &str) -> String {
    plain(token).to_owned()
}

/// Returns the name that the identifier `token` makes, as [`unraw`] does, borrowed.
fn plain(token: &str) -> &str {
    token.strip_prefix("r#").unwrap_or(token)
}

/// Returns the text of `tokens[at]`, or an empty text past the end.
fn text_at<'a>(tokens: &[Token<'a>], at: usize) -> &'a str {
    tokens.get(at).map_or("", |token| token.text)
}

/// Returns the path from a module of the crate that starts at `tokens[at]`, if one does: a
/// `crate::`, `super::`, or `self::` path, or one from a name of `prelude`, which the extern
/// prelude gives the crate root, with `::` as its first segment if `::` starts it. Returns its
/// segments, the index of its last segment, and the index after it. The last segment of a macro's
/// path, which a `!` and the macro's arguments follow, ends with that `!`, since macros have names
/// of their own, apart from those of modules and other items.
fn qualified_path(
    tokens: &[Token<'_>],
    at: usize,
    prelude: &BTreeSet<String>,
) -> Option<(Vec<String>, usize, usize)> {
    let first = text_at(tokens, at);
    let named = |at: usize| {
        is_name(text_at(tokens, at))
            && prelude.contains(plain(text_at(tokens, at)))
            && text_at(tokens, at + 1) == "::"
    };
    let (mut segments, mut last, mut end) =
        if matches!(first, "crate" | "super" | "self") && text_at(tokens, at + 1) == "::" {
            (vec![first.to_owned()], at, at + 1)
        // A `::` after a name continues a path. A `>` may close a type or compare, so the
        // name after it may start one.
        } else if first == "::" && named(at + 1) && !is_name(text_at(tokens, at.wrapping_sub(1))) {
            let name = unraw(text_at(tokens, at + 1));
            (vec!["::".to_owned(), name], at + 1, at + 2)
        } else if named(at) && text_at(tokens, at.wrapping_sub(1)) != "::" {
            (vec![unraw(first)], at, at + 1)
        } else {
            return None;
        };
    while text_at(tokens, end) == "::" && is_identifier(text_at(tokens, end + 1)) {
        segments.push(unraw(tokens[end + 1].text));
        last = end + 1;
        end += 2;
    }
    if text_at(tokens, end) == "!"
        && matches!(text_at(tokens, end + 1), "(" | "[" | "{")
        && let Some(name) = segments.last_mut()
    {
        name.push('!');
    }
    Some((segments, last, end))
}

/// Returns the paths from modules of the crate in `tokens`, as [`qualified_path`] finds them with
/// `prelude`, each with its segments and the line that `line_of` gives for its last segment.
fn qualified_paths(
    tokens: &[Token<'_>],
    prelude: &BTreeSet<String>,
    line_of: impl Fn(&Token<'_>) -> usize,
) -> Vec<WrittenPath> {
    let mut paths = Vec::new();
    let mut at = 0;
    while at < tokens.len() {
        match qualified_path(tokens, at, prelude) {
            Some((segments, last, end)) => {
                paths.push((segments, line_of(&tokens[last])));
                at = end;
            }
            None => at += 1,
        }
    }
    paths
}

/// Returns the index of the token after the visibility, such as `pub(crate)`, that starts at
/// `tokens[at]`, or `at` if no visibility starts there.
fn after_visibility(tokens: &[Token<'_>], mut at: usize) -> usize {
    if text_at(tokens, at) == "pub" {
        at += 1;
        if text_at(tokens, at) == "(" {
            while !matches!(text_at(tokens, at), ")" | "") {
                at += 1;
            }
            at += 1;
        }
    }
    at
}

/// Returns the attribute that starts at `tokens[at]`, a `#`, if one does: the index after it,
/// whether it is an inner attribute, `#![...]`, and the tokens within its brackets.
fn attribute<'t, 'a>(tokens: &'t [Token<'a>], at: usize) -> Option<(usize, bool, &'t [Token<'a>])> {
    let inner = text_at(tokens, at + 1) == "!";
    let open = at + 1 + usize::from(inner);
    if text_at(tokens, open) != "[" {
        return None;
    }
    let mut depth = 0;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token.text {
            "[" => depth += 1,
            "]" => {
                depth -= 1;
                if depth == 0 {
                    return Some((index + 1, inner, &tokens[open + 1..index]));
                }
            }
            _ => {}
        }
    }
    Some((tokens.len(), inner, &tokens[open + 1..]))
}

/// Returns whether an attribute, by the tokens within its brackets, makes what it guards
/// test-only: `#[test]`, or a `cfg` whose predicate requires `test`. Raw identifiers, such as
/// `r#cfg`, name what they would without `r#`, here and in the other checks of attributes.
fn guards_test(attribute: &[Token<'_>]) -> bool {
    match attribute {
        [only] => plain(only.text) == "test",
        [name, open, predicate @ ..] if plain(name.text) == "cfg" && open.text == "(" => {
            requires_test(predicate, &mut 0)
        }
        _ => false,
    }
}

/// Returns whether the `cfg` predicate at `tokens[*at]` holds only where `test` does, and moves
/// `*at` past it. Such a predicate is `test`, an `all` of predicates of which one is such, or an
/// `any` of predicates that all are; `not(test)` is not.
fn requires_test(tokens: &[Token<'_>], at: &mut usize) -> bool {
    let name = plain(text_at(tokens, *at));
    *at += 1;
    match text_at(tokens, *at) {
        // The value of `key = "value"` is a string, which blanking removed.
        "=" => {
            *at += 1;
            false
        }
        "(" => {
            *at += 1;
            let mut predicates = Vec::new();
            while !matches!(text_at(tokens, *at), ")" | "") {
                predicates.push(requires_test(tokens, at));
                if text_at(tokens, *at) == "," {
                    *at += 1;
                }
            }
            *at += 1;
            match name {
                "all" => predicates.contains(&true),
                "any" => !predicates.is_empty() && !predicates.contains(&false),
                _ => false,
            }
        }
        _ => name == "test",
    }
}

/// Returns the attributes that an attribute, by the tokens within its brackets, may apply: itself,
/// or, for a `cfg_attr`, those that it lists after its predicate, under any predicate, and theirs
/// in turn.
fn applied<'t, 'a>(attribute: &'t [Token<'a>]) -> Vec<&'t [Token<'a>]> {
    let [name, open, ..] = attribute else {
        return vec![attribute];
    };
    if plain(name.text) != "cfg_attr" || open.text != "(" {
        return vec![attribute];
    }
    let close = group_end(attribute, 1).map_or(attribute.len(), |end| end - 1);
    let mut at = 2;
    requires_test(attribute, &mut at);
    let mut found = Vec::new();
    while at < close {
        if attribute[at].text == "," {
            at += 1;
            continue;
        }
        let end = scan(attribute, at, false, |text| text == ",")
            .unwrap_or_else(|end| end)
            .min(close);
        found.extend(applied(&attribute[at..end]));
        at = end;
    }
    found
}

/// Returns whether an attribute, by the tokens within its brackets, sets the file of a module:
/// `#[path = "..."]`, directly or through `cfg_attr`.
fn sets_path(attribute: &[Token<'_>]) -> bool {
    applied(attribute).iter().any(|applied| {
        matches!(applied, [name, equals, ..] if plain(name.text) == "path" && equals.text == "=")
    })
}

/// Returns whether an attribute, by the tokens within its brackets, is or may be `name`, such as
/// `macro_export`: directly, or through `cfg_attr` under any predicate.
fn may_be(attribute: &[Token<'_>], name: &str) -> bool {
    applied(attribute).iter().any(|applied| {
        applied
            .first()
            .is_some_and(|first| plain(first.text) == name)
    })
}

/// A string literal of a file.
struct Literal {
    /// The index of its first byte in the file's text.
    offset: usize,
    /// Its value, with its escapes decoded.
    value: String,
    /// The line in the file of each byte of `value`.
    lines: Vec<usize>,
}

/// Returns the string literals of `text[from..to]`, which starts on line `line` and outside any
/// comment or literal: plain, byte, and C strings, raw or not, but no character literal.
fn strings(text: &str, from: usize, to: usize, line: usize) -> Vec<Literal> {
    let mut literals = Vec::new();
    let mut at = from;
    while at < to {
        let Some(end) = literal_end(text.as_bytes(), at) else {
            at += 1;
            continue;
        };
        let literal = &text[at..end];
        // A comment or a character literal holds no string.
        if !literal.starts_with('/')
            && let (Some(first), Some(last)) = (literal.find('"'), literal.rfind('"'))
            && first < last
        {
            let start = line + text[from..at].matches('\n').count();
            let body = &literal[first + 1..last];
            // A raw string, whose prefix has an `r`, has no escapes.
            let (value, feeds) = if literal[..first].contains('r') {
                let feeds = body
                    .bytes()
                    .scan(0, |feeds, byte| {
                        let before = *feeds;
                        *feeds += usize::from(byte == b'\n');
                        Some(before)
                    })
                    .collect();
                (body.to_owned(), feeds)
            } else {
                unescape(body)
            };
            literals.push(Literal {
                offset: at,
                value,
                lines: feeds
                    .into_iter()
                    .map(|feeds: usize| start + feeds)
                    .collect(),
            });
        }
        at = end;
    }
    literals
}

/// Returns the paths from modules of the crate in the value of `literal`, as [`qualified_path`]
/// finds them with `prelude`, each with its segments and the line of its last segment, and, as a
/// path of its own, each identifier that is not ASCII beside a `::`, which Rust may take for
/// another spelling of a name of the crate, as `\u{212A}::host` for `K::host`.
fn literal_paths(literal: &Literal, prelude: &BTreeSet<String>) -> Vec<WrittenPath> {
    let tokens = tokenize(&literal.value);
    let mut paths = qualified_paths(&tokens, prelude, |token| literal.lines[token.offset]);
    paths.extend(
        (0..tokens.len())
            .filter(|&at| {
                !tokens[at].text.is_ascii()
                    && (text_at(&tokens, at + 1) == "::" || (at > 0 && tokens[at - 1].text == "::"))
            })
            .map(|at| {
                (
                    vec![tokens[at].text.to_owned()],
                    literal.lines[tokens[at].offset],
                )
            }),
    );
    paths
}

/// Returns the documentation of the doc comments of `text[from..to]`, which starts on line `line`
/// and outside any comment or literal, as the strings of the `#[doc = "..."]` attributes that Rust
/// makes of them before a macro matches its input: of `///` and `//!` comments, but not `////`
/// ones, and of `/**` and `/*!` comments, but not `/***` or `/**/` ones.
fn doc_comments(text: &str, from: usize, to: usize, line: usize) -> Vec<Literal> {
    let mut literals = Vec::new();
    let mut at = from;
    while at < to {
        let Some(end) = literal_end(text.as_bytes(), at) else {
            at += 1;
            continue;
        };
        let comment = &text[at..end];
        let body = if (comment.starts_with("///") && !comment.starts_with("////"))
            || comment.starts_with("//!")
        {
            comment.get(3..)
        } else if (comment.starts_with("/**") && !comment.starts_with("/***") && comment != "/**/")
            || comment.starts_with("/*!")
        {
            comment.get(3..comment.len().saturating_sub(2).max(3))
        } else {
            None
        };
        if let Some(body) = body {
            let start = line + text[from..at].matches('\n').count();
            let mut feeds = 0;
            literals.push(Literal {
                offset: at,
                value: body.to_owned(),
                lines: body
                    .bytes()
                    .map(|byte| {
                        let before = feeds;
                        feeds += usize::from(byte == b'\n');
                        start + before
                    })
                    .collect(),
            });
        }
        at = end;
    }
    literals
}

/// Adds to `found` the ranges of `tokens` of the attributes that the attribute in
/// `tokens[from..to]`, the tokens within its brackets, applies, each with whether only where `test`
/// holds, as `test` says of the attribute itself: the attribute, or, for a `cfg_attr`, those that
/// it applies after its predicate, in turn. The tokens of what another attribute takes as its
/// arguments apply nothing, though they hold a `cfg_attr` or a `doc`.
fn applications(
    tokens: &[Token<'_>],
    from: usize,
    to: usize,
    test: bool,
    found: &mut Vec<(Range<usize>, bool)>,
) {
    if plain(text_at(tokens, from)) != "cfg_attr" || text_at(tokens, from + 1) != "(" {
        found.push((from..to, test));
        return;
    }
    let close = group_end(tokens, from + 1)
        .map_or(to, |end| end - 1)
        .min(to);
    let mut at = from + 2;
    let tested = test || requires_test(tokens, &mut at);
    while at < close {
        if tokens[at].text == "," {
            at += 1;
            continue;
        }
        let end = scan(tokens, at, false, |text| text == ",")
            .unwrap_or_else(|end| end)
            .min(close);
        applications(tokens, at, end, tested, found);
        at = end.max(at + 1);
    }
}

/// Returns the paths from modules of the crate that the attribute whose brackets open at
/// `tokens[open]` and close at `tokens[close]`, or at the end, writes, as [`qualified_path`] finds
/// them with `prelude`, each with its segments and the line of its last segment, in the order of
/// their lines: those among its tokens, such as a derive's, and those in its strings, with their
/// escapes decoded, such as serde's `with = "..."`, which a macro turns into code. The strings of
/// an attribute of documentation that it applies, such as `doc = "..."` or `doc(alias = "...")`,
/// name none, as a doc comment does not. Neither does an attribute that a `cfg_attr` applies only
/// where `test` holds, unless the attribute is in the `input` of a macro, which may drop its
/// `cfg_attr`s. `text` is the text that blanking turned into the text of `tokens`.
fn attribute_paths(
    text: &str,
    tokens: &[Token<'_>],
    open: usize,
    close: usize,
    input: bool,
    prelude: &BTreeSet<String>,
) -> Vec<WrittenPath> {
    let offset = |index: usize| tokens.get(index).map_or(text.len(), |token| token.offset);
    let mut applied = Vec::new();
    applications(tokens, open + 1, close, false, &mut applied);
    let tested: Vec<Range<usize>> = applied
        .iter()
        .filter(|(_, test)| *test && !input)
        .map(|(range, _)| range.clone())
        .collect();
    let documentation: Vec<Range<usize>> = applied
        .iter()
        .filter(|(range, _)| {
            plain(text_at(tokens, range.start)) == "doc"
                && matches!(text_at(tokens, range.start + 1), "=" | "(")
        })
        .map(|(range, _)| range.clone())
        .collect();
    let mut paths = Vec::new();
    let mut at = open + 1;
    while at < close {
        match qualified_path(&tokens[..close], at, prelude) {
            Some((segments, last, end)) => {
                if !tested.iter().any(|range| range.contains(&at)) {
                    paths.push((segments, tokens[last].line));
                }
                at = end;
            }
            None => at += 1,
        }
    }
    let within = |ranges: &[Range<usize>], at: usize| {
        ranges
            .iter()
            .any(|range| (offset(range.start)..offset(range.end)).contains(&at))
    };
    for literal in strings(text, tokens[open].offset, offset(close), tokens[open].line) {
        if !within(&documentation, literal.offset) && !within(&tested, literal.offset) {
            paths.extend(literal_paths(&literal, prelude));
        }
    }
    paths.sort_by_key(|&(_, line)| line);
    paths
}

/// Returns the paths from modules of the crate in the input of a macro, which `tokens[open]` and
/// `tokens[close]` enclose: those of its tokens, as [`qualified_path`] finds them with `prelude`
/// from each of them, past any `::` before it, however the parser reads the input, which need not
/// be Rust, as in a `use` tree that the macro takes apart; and those of its strings, and of its
/// doc comments, which Rust makes `#[doc = "..."]` attributes of, as [`literal_paths`] finds them:
/// a macro may put such a string where a path is read, as in a `serde` attribute that it
/// completes, even one after `doc =`, which the macro need not take for documentation. `text` is
/// the text that blanking turned into the text of `tokens`.
fn input_paths(
    text: &str,
    tokens: &[Token<'_>],
    open: usize,
    close: usize,
    prelude: &BTreeSet<String>,
) -> Vec<WrittenPath> {
    let to = tokens.get(close).map_or(text.len(), |token| token.offset);
    // A macro may take any suffix of a path apart from what precedes it, as `ignored::nvx::host`
    // may give `nvx::host`, so the test reads a path from each token, past any `::` before it.
    let mut paths: Vec<WrittenPath> = (open + 1..close.min(tokens.len()))
        .filter_map(|from| {
            qualified_path(&tokens[from..close.min(tokens.len())], 0, prelude)
                .map(|(segments, last, _)| (segments, tokens[from + last].line))
        })
        .collect();
    paths.extend(
        strings(text, tokens[open].offset, to, tokens[open].line)
            .iter()
            .chain(&doc_comments(
                text,
                tokens[open].offset,
                to,
                tokens[open].line,
            ))
            .flat_map(|literal| literal_paths(literal, prelude)),
    );
    paths
}

/// Returns what the `extern crate` declaration at `tokens[at]` imports, as the segments of a path,
/// and the name, if any, that it binds: `self` the crate root, and another crate none of the
/// crate's modules.
fn extern_crate(tokens: &[Token<'_>], at: usize) -> (Vec<String>, Option<String>) {
    let name = text_at(tokens, at + 2);
    let segments = if name == "self" {
        vec!["crate".to_owned()]
    } else {
        vec!["::".to_owned(), unraw(name)]
    };
    let binding = if text_at(tokens, at + 3) == "as" {
        unraw(text_at(tokens, at + 4))
    } else {
        unraw(name)
    };
    (segments, Some(binding).filter(|binding| binding != "_"))
}

/// Where a macro is invoked, in `module`, from test-only code if `test`, and in a block if
/// `local`: records in `facts` what the input of the macro, which `tokens[open]` and
/// `tokens[close]` enclose, imports, declares, or invokes, wherever it is in the input, which need
/// not be Rust, however the parser reads the rest of it, as in a `use` tree that the macro takes
/// apart: the imports of its `use` trees and `extern crate` declarations, its inline modules, its
/// attributes that set a module's file or bring macros into scope, the templates of its
/// `macro_rules!`, which `includes` may name for `include!`, and its invocations of `include!`, but
/// those by a name that `hidden` says a `macro_rules!` of the same name hides.
fn input_declarations(
    (text, tokens): (&str, &[Token<'_>]),
    (open, close): (usize, usize),
    (module, test, local): (&[String], bool, bool),
    (includes, prelude): (&BTreeSet<String>, &BTreeSet<String>),
    hidden: impl Fn(&str) -> bool,
    facts: &mut Facts,
) {
    let inner_macros = tokens[open + 1..close.min(tokens.len())]
        .iter()
        .any(|token| plain(token.text) == "local_inner_macros");
    // A `path` or a `macro_use` anywhere in the input, of which a template, maybe through other
    // macros, may make an attribute that sets the file of a module that the input of a macro
    // declares, or brings its macros into scope, as `#[$attr] $item` may, which the test cannot
    // tell. One after a `$` too, which a macro may drop, as `$d:tt $attr:ident` does.
    if let Some(token) = tokens[open + 1..close.min(tokens.len())]
        .iter()
        .find(|token| matches!(plain(token.text), "path" | "macro_use"))
    {
        facts.attribute_names.push((token.line, test));
    }
    // A `macro_export` anywhere in the input, of which a template may make an attribute that
    // exports a macro at the crate root, from where the test cannot tell: one of the input's own
    // attributes, one in another attribute, as `#[unwrap(macro_export)]`, or one that the parser's
    // reading of the input applies to nothing, as in `#[macro_export], ...` or in a `use` tree.
    if let Some(token) = tokens[open + 1..close.min(tokens.len())]
        .iter()
        .find(|token| plain(token.text) == "macro_export")
    {
        facts.input_exports.push((token.line, test));
    }
    for at in open + 1..close.min(tokens.len()) {
        let token = tokens[at];
        let reference = |segments, kind, line| Reference {
            module: module.to_vec(),
            segments,
            kind,
            line,
            test,
            local,
        };
        match token.text {
            "#" => {
                if let Some((_, _, inside)) = attribute(tokens, at) {
                    if sets_path(inside) {
                        facts.path_attributes.push(token.line);
                    }
                    if may_be(inside, "macro_use") {
                        facts.macro_uses.push((module.to_vec(), token.line));
                    }
                }
            }
            "use" if text_at(tokens, at + 1) != "<" => {
                let mut end = at + 1;
                let mut imports = Vec::new();
                expand(tokens, &mut end, &[], &mut imports);
                for (segments, kind, line) in imports {
                    facts.references.push(reference(segments, kind, line));
                }
            }
            "extern"
                if text_at(tokens, at + 1) == "crate" && is_identifier(text_at(tokens, at + 2)) =>
            {
                let (segments, binding) = extern_crate(tokens, at);
                // At the crate root, the alias may join the extern prelude, if the macro puts it
                // there, which the test cannot tell.
                if module.is_empty() && segments == ["crate"] && binding.is_some() {
                    facts.input_prelude.push((token.line, test));
                }
                facts
                    .references
                    .push(reference(segments, Kind::Import(binding), token.line));
            }
            "mod" if is_identifier(text_at(tokens, at + 1)) && text_at(tokens, at + 2) == "{" => {
                facts.input_modules.push((token.line, test));
            }
            // A module in a file of its own, whose code the test reads as the module's, unless a
            // template may give it an attribute that the test does not see.
            "mod" if is_identifier(text_at(tokens, at + 1)) && text_at(tokens, at + 2) == ";" => {
                let declared = [module, &[unraw(text_at(tokens, at + 1))]].concat();
                facts.declared.push((declared, test));
                facts.input_files.push((token.line, test));
            }
            "macro_rules"
                if text_at(tokens, at + 1) == "!"
                    && is_identifier(text_at(tokens, at + 2))
                    && matches!(text_at(tokens, at + 3), "{" | "(" | "[") =>
            {
                let template = Template {
                    open: at + 3,
                    end: group_end(tokens, at + 3).unwrap_or(close),
                    inner_macros,
                    includes,
                };
                // The scan of the input finds its every `path` and `macro_use` already.
                if let Some(position) =
                    unfollowed_in_template(text, tokens, &template, prelude, &mut None)
                {
                    facts.templates.push((tokens[position].line, test));
                }
            }
            name if includes.contains(plain(name))
                && text_at(tokens, at + 1) == "!"
                && matches!(text_at(tokens, at + 2), "(" | "[" | "{")
                && (text_at(tokens, at - 1) == "::" || !hidden(plain(name))) =>
            {
                facts.includes.push((token.line, test));
            }
            _ => {}
        }
    }
}

/// Returns the index of the first token at the level of `tokens[at]` that may end an element of a
/// list that starts there, which ends there or later: a `,`, `;`, `=>`, `|`, or `>` but that of
/// `->`, or the end of the list. A parameter of generics or of a closure, or a field of a struct
/// expression, ends at one of them, past any `<` that compares or opens type arguments.
fn element_end(tokens: &[Token<'_>], at: usize) -> usize {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(at) {
        match token.text {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" if depth == 0 => return index,
            ")" | "]" | "}" => depth -= 1,
            "," | ";" | "=>" | "|" if depth == 0 => return index,
            ">" if depth == 0 && text_at(tokens, index.wrapping_sub(1)) != "-" => return index,
            _ => {}
        }
    }
    tokens.len()
}

/// The rules of a `macro_rules!`, which a parse reads for what the test cannot follow.
struct Template<'s> {
    /// The index of the token that opens the rules.
    open: usize,
    /// The index after the token that closes them.
    end: usize,
    /// Whether `#[macro_export(local_inner_macros)]` applies to the macro, which makes each macro
    /// that its transcribers invoke by name one of the crate root.
    inner_macros: bool,
    /// The names that may stand for `include!` where the macro is defined.
    includes: &'s BTreeSet<String>,
}

/// Returns, for the tokens `tokens[from..to]` at the top level of a group of a template, whether
/// their every expansion that the group of an import may hold is empty, `Some(false)`, or one may
/// hold something, `Some(true)`, or `None` if none may: a group of an import holds names, `as`,
/// `::`, `*`, `,`, metavariables, and groups alone. A repetition, by the index past it in
/// `repetitions`, after the index of the `)` that closes it, may expand to nothing, as one must
/// that holds another token at its own top level, such as `$($field: $value),*`.
fn import_content(
    tokens: &[Token<'_>],
    (from, to): (usize, usize),
    repetitions: &BTreeMap<usize, (usize, Option<usize>)>,
) -> Option<bool> {
    let mut content = false;
    let mut at = from;
    while at < to {
        let token = text_at(tokens, at);
        at = match token {
            "$" if text_at(tokens, at + 1) == "(" => {
                let ended = group_end(tokens, at + 1).unwrap_or(to);
                content |= import_content(tokens, (at + 2, ended - 1), repetitions) == Some(true);
                repetitions.get(&(ended - 1)).map_or(to, |&(past, _)| past)
            }
            "$" if is_identifier(text_at(tokens, at + 1)) => {
                content = true;
                at + 2
            }
            "{" => {
                content = true;
                group_end(tokens, at).unwrap_or(to)
            }
            "$" | "::" | "*" | "," | "as" => {
                content = true;
                at + 1
            }
            _ if is_name(token) => {
                content = true;
                at + 1
            }
            _ => return None,
        };
    }
    Some(content)
}

/// Returns the index of the first token of the rules of `template`, a `macro_rules!`, whose
/// transcriber names what only the macro's expansions resolve, where they expand, if any: a `use`,
/// `mod`, or `extern crate` declaration; a path from a module of the crate, as [`qualified_path`]
/// finds them with `prelude`, past any `::` before it in the input of a macro that the transcriber
/// invokes, or a string that holds one; a metavariable or repetition that may join a path from the
/// input, before `::` or a segment and `::`, or after the start of a path from a module; a
/// metavariable of a token tree, which may hold any of these, as the macro's matcher or that of one
/// that it defines binds it; an identifier that the input gives, where an item or a statement
/// starts, or anywhere in the input of a macro that the transcriber invokes, and a name follows,
/// which may be a keyword that declares; a `serde` attribute that a metavariable completes, whose
/// string the input holds; an `include!` of another file's code, or of a macro that the input
/// names, as `$reader!`, which may be `include!`; or, under `local_inner_macros`, a macro that it
/// invokes by name. Matchers, the macro's and those of the macros that its transcribers define,
/// only match input, but those in the input of a macro that a transcriber invokes, which may take
/// them for anything. Sets `named`, unless it is set, to the index of the first `path` or
/// `macro_use` of the transcribers, of which one may make an attribute of a module that the input
/// of a macro declares. `text` is the text that blanking turned into the text of `tokens`.
fn unfollowed_in_template(
    text: &str,
    tokens: &[Token<'_>],
    template: &Template<'_>,
    prelude: &BTreeSet<String>,
    named: &mut Option<usize>,
) -> Option<usize> {
    let end = template.end;
    let mut at = template.open + 1;
    while at + 1 < end {
        // A rule is a matcher, `=>`, and a transcriber, each a bracketed group.
        if !matches!(text_at(tokens, at), "(" | "[" | "{") {
            at += 1;
            continue;
        }
        let matcher = group_end(tokens, at).unwrap_or(end);
        let transcriber = matcher + 1;
        if text_at(tokens, matcher) != "=>"
            || !matches!(text_at(tokens, transcriber), "(" | "[" | "{")
        {
            at = matcher;
            continue;
        }
        let after = group_end(tokens, transcriber).unwrap_or(end);
        // The rules of the `macro_rules!` that the transcriber defines, at any depth, outer ones
        // first: the range of each matcher, which only matches input, and that of its
        // transcriber, whose metavariables the matcher binds.
        let mut rules: Vec<(Range<usize>, Range<usize>)> = Vec::new();
        let mut nested = transcriber + 1;
        while nested + 3 < after {
            if tokens[nested].text != "macro_rules"
                || text_at(tokens, nested + 1) != "!"
                || !matches!(text_at(tokens, nested + 3), "(" | "[" | "{")
            {
                nested += 1;
                continue;
            }
            let body = group_end(tokens, nested + 3).unwrap_or(after);
            let mut rule = nested + 4;
            while rule + 1 < body {
                if !matches!(text_at(tokens, rule), "(" | "[" | "{") {
                    rule += 1;
                    continue;
                }
                let matched = group_end(tokens, rule).unwrap_or(body);
                let transcribed = if text_at(tokens, matched) == "=>" {
                    matched + 1..group_end(tokens, matched + 1).unwrap_or(body)
                } else {
                    matched..matched
                };
                let next = transcribed.end;
                rules.push((rule..matched, transcribed));
                rule = next;
            }
            // The transcribers of the nested macro may define macros in turn.
            nested += 4;
        }
        // The fragment that the metavariable at `position`, a `$`, binds: by the macro's matcher
        // first, since the macro's expansions substitute its metavariables everywhere, those of
        // the macros that it defines too, and then by the matcher of each rule of those macros
        // whose transcriber holds the position, outer ones first.
        let binding = |matched: &Range<usize>, name: &str| {
            matched.clone().find_map(|at| {
                // `$r#name` and `$name` are one metavariable, as `r#tt` and `tt` are one fragment.
                (text_at(tokens, at) == "$"
                    && plain(text_at(tokens, at + 1)) == plain(name)
                    && text_at(tokens, at + 2) == ":")
                    .then(|| plain(text_at(tokens, at + 3)))
            })
        };
        // The fragment that the metavariable at `position`, a `$`, binds, if any.
        let kind_of = |position: usize| {
            let name = text_at(tokens, position + 1);
            (text_at(tokens, position) == "$")
                .then(|| {
                    binding(&(at + 1..matcher), name).or_else(|| {
                        rules
                            .iter()
                            .filter(|(_, transcribed)| transcribed.contains(&position))
                            .find_map(|(matched, _)| binding(matched, name))
                    })
                })
                .flatten()
        };
        let is = |position: usize, kinds: &[&str]| {
            kind_of(position).is_some_and(|kind| kinds.contains(&kind))
        };
        // By the index of the `)` that closes each repetition of the transcriber: the index past
        // the repetition, its separator, if any, and its operator, where what follows its last
        // token follows, and, unless the operator is `?`, which never repeats, that of the token
        // that follows its last token when it repeats, its separator, or else its first token.
        let repetitions: BTreeMap<usize, (usize, Option<usize>)> = (transcriber + 1..after)
            .filter(|&position| {
                text_at(tokens, position) == "$" && text_at(tokens, position + 1) == "("
            })
            .filter_map(|position| {
                let close = group_end(tokens, position + 1)? - 1;
                Some(match text_at(tokens, close + 1) {
                    "?" => (close, (close + 2, None)),
                    "*" | "+" => (close, (close + 2, Some(position + 2))),
                    _ => (close, (close + 3, Some(close + 1))),
                })
            })
            .collect();
        // Whether a repetition of the transcriber ends right before `at`.
        let ends_repetition = |at: usize| repetitions.values().any(|&(past, _)| past == at);
        // The input of a macro that the transcriber invokes follows the `!` after its name, or
        // after a metavariable or a repetition that may give it, unlike the block that `!`
        // negates in `= !{ ... }` or `1 + !{ ... }`. The macro may take it apart as it pleases.
        let inputs: Vec<Range<usize>> = (transcriber + 1..after)
            .filter(|&position| {
                text_at(tokens, position) == "!"
                    && matches!(text_at(tokens, position + 1), "(" | "[" | "{")
                    && (is_name(text_at(tokens, position - 1)) || ends_repetition(position))
            })
            .filter_map(|position| Some(position + 1..group_end(tokens, position + 1)?))
            .collect();
        let in_input = |at: usize| inputs.iter().any(|input| input.contains(&at));
        // The matchers of the macros that the transcriber defines only match input, but those in
        // the input of a macro that it invokes, which may take them for anything, as
        // `call!(macro_rules! ignored { (crate::host::b::g) => {} })` may take the path.
        let matchers: Vec<&Range<usize>> = rules
            .iter()
            .map(|(matched, _)| matched)
            .filter(|matched| !in_input(matched.start))
            .collect();
        // A `path` or a `macro_use`, of which the transcriber, maybe through other macros, may
        // make an attribute that sets the file of a module that the input of a macro declares, or
        // brings its macros into scope: any but one in a matcher that only matches input, or a
        // metavariable of that name that a matcher binds, as `$path` in
        // `($path:path) => { $path() }`. One that none binds expands to itself, whose `$` a macro
        // may drop.
        if named.is_none() {
            *named = (transcriber + 1..after).find(|&position| {
                matches!(plain(tokens[position].text), "path" | "macro_use")
                    && !matchers.iter().any(|matched| matched.contains(&position))
                    && !(tokens[position - 1].text == "$" && kind_of(position - 1).is_some())
            });
        }
        let unfollowed = (transcriber + 1..after - 1).find(|&position| {
            if matchers.iter().any(|matched| matched.contains(&position)) {
                return false;
            }
            let token = text_at(tokens, position);
            // A declaration, or `extern` before an identifier or a token tree of the input, or a
            // repetition, which may complete it as `extern crate`, past visibilities of the input,
            // which may be empty, as in `extern $vis crate self as this;`.
            let completes = |at: usize| {
                text_at(tokens, at) == "$"
                    && (text_at(tokens, at + 1) == "(" || is(at, &["ident", "tt"]))
            };
            let declares = match token {
                "use" => text_at(tokens, position + 1) != "<",
                "mod" => true,
                "extern" => {
                    let mut next = position + 1;
                    while is(next, &["vis"]) {
                        next += 2;
                    }
                    text_at(tokens, next) == "crate" || completes(next)
                }
                _ => false,
            };
            // The token after a metavariable, or after a repetition and its separator, and
            // whether that separator, or the first token of the repetition, is `::`. Either may
            // expand to `::` or to the start of a path, such as `crate ::`.
            let expansion = match (token, text_at(tokens, position + 1)) {
                ("$", "(") => {
                    let group = group_end(tokens, position + 1).unwrap_or(after);
                    let separator = !matches!(text_at(tokens, group), "*" | "+" | "?");
                    let joined = (separator && text_at(tokens, group) == "::")
                        || text_at(tokens, position + 2) == "::";
                    Some((group + 1 + usize::from(separator), joined))
                }
                ("$", name) if is_identifier(name) => Some((position + 2, false)),
                _ => None,
            };
            let joins = expansion.is_some_and(|(next, joined)| {
                joined
                    || text_at(tokens, next) == "::"
                    || (is_name(text_at(tokens, next)) && text_at(tokens, next + 1) == "::")
            });
            let tree = is(position, &["tt"]);
            // An identifier that the input gives, where an item or a statement starts and a name
            // or another metavariable follows, may be `mod`, `use`, or `extern`, which declare or
            // import, as `$kind $name { ... }` may be `mod name { ... }`. A visibility that the
            // input gives, which may be empty, may stand before it, and an item, a statement, or a
            // block that the input gives ends one before it. In the input of a macro that the
            // template invokes, which may take any part of it for an item, as
            // `forward!(@tag $kind $name { $item })` may, one starts anywhere.
            let mut first = position;
            while first >= 2 && first - 2 > transcriber && is(first - 2, &["vis"]) {
                first -= 2;
            }
            let before = text_at(tokens, first - 1);
            let item_start = first - 1 == transcriber
                || matches!(before, "{" | "}" | ";" | "]" | ")" | "pub")
                || (before == "(" && text_at(tokens, first - 2) == "$")
                || ends_repetition(first)
                || (first >= 2 && is(first - 2, &["item", "stmt", "block", "expr", "expr_2021"]))
                || in_input(position);
            // What may follow the identifier: the token after it, or, where it ends repetitions,
            // the token that follows it when each repeats, and the token after them.
            let mut follows = Vec::new();
            let mut next = position + 2;
            while let Some(&(past, again)) = repetitions.get(&next) {
                follows.extend(again);
                next = past;
            }
            follows.push(next);
            // A token that may start the tree of an import: a name, `::`, another metavariable,
            // or a group that may hold one, as `$kind { $path }` may, unlike the struct
            // expressions `$name { field: value }`, `$name { ..base }`, and
            // `$name { $($field: $value),* }`, and the blocks `$kind { break; }` and
            // `$kind { break }`.
            let imports = |at: usize| match text_at(tokens, at) {
                "$" | "::" => true,
                "{" => group_end(tokens, at).is_some_and(|end| {
                    import_content(tokens, (at + 1, end - 1), &repetitions) == Some(true)
                }),
                named => is_identifier(named) && named != "as",
            };
            let keyword =
                is(position, &["ident"]) && item_start && follows.into_iter().any(imports);
            let start = (matches!(token, "crate" | "self" | "super")
                || (is_name(token) && prelude.contains(plain(token))))
                && text_at(tokens, position + 1) == "$";
            // An alias of the crate root, or of a module above, which only an import may make,
            // unlike `self as u8` or a variable of a name of the extern prelude, which cast.
            let aliases = matches!(token, "crate" | "super")
                && text_at(tokens, position + 1) == "as"
                && text_at(tokens, position.wrapping_sub(1)) != "::";
            let serde = token == "#"
                && attribute(tokens, position).is_some_and(|(_, _, inside)| {
                    inside.iter().any(|token| plain(token.text) == "serde")
                        && inside.iter().any(|token| token.text == "$")
                });
            // An attribute that sets the file of a module or brings macros into scope, which the
            // test reads nowhere in a template.
            let attributes_module = token == "#"
                && attribute(tokens, position)
                    .is_some_and(|(_, _, inside)| sets_path(inside) || may_be(inside, "macro_use"));
            // A `macro_export` anywhere, of which the template, or a macro that it invokes, may
            // make an attribute that exports a macro at the crate root, from where the test
            // cannot tell.
            let exports = plain(token) == "macro_export";
            // A macro that the template invokes: `include!`, under any of its names, one that the
            // input names, as `$reader!` or `$($reader)+!`, which may be `include!`, and, under
            // `local_inner_macros`, any by name.
            let invokes = text_at(tokens, position + 1) == "!"
                && (template.includes.contains(plain(token))
                    || (text_at(tokens, position - 1) == "$" && is_identifier(token))
                    || (ends_repetition(position + 1)
                        && matches!(text_at(tokens, position + 2), "(" | "[" | "{"))
                    || (template.inner_macros
                        && is_name(token)
                        && matches!(text_at(tokens, position + 2), "(" | "[" | "{")));
            declares
                || joins
                || tree
                || keyword
                || start
                || aliases
                || serde
                || attributes_module
                || exports
                || invokes
                || qualified_path(tokens, position, prelude).is_some()
                // A macro that the template invokes may take any suffix of a path in its input
                // apart from what precedes it, as `call!(ignored::nvx::host::b::g)` may.
                || (in_input(position) && qualified_path(&tokens[position..], 0, prelude).is_some())
        });
        if unfollowed.is_some() {
            return unfollowed;
        }
        let to = tokens
            .get(after - 1)
            .map_or(text.len(), |token| token.offset);
        // The strings of the matchers of the macros that the transcriber defines only match input,
        // and those of documentation that its attributes apply name nothing, but in the input of
        // a macro that it invokes, which may take them for anything.
        let offset = |at: usize| tokens.get(at).map_or(text.len(), |token| token.offset);
        let mut matched: Vec<Range<usize>> = matchers
            .iter()
            .map(|matched| offset(matched.start)..offset(matched.end))
            .collect();
        for position in transcriber + 1..after {
            if text_at(tokens, position) == "#"
                && !in_input(position)
                && let Some((end, inner, _)) = attribute(tokens, position)
            {
                let mut applied = Vec::new();
                applications(
                    tokens,
                    position + 2 + usize::from(inner),
                    end - 1,
                    false,
                    &mut applied,
                );
                matched.extend(
                    applied
                        .iter()
                        .filter(|(range, _)| {
                            plain(text_at(tokens, range.start)) == "doc"
                                && matches!(text_at(tokens, range.start + 1), "=" | "(")
                        })
                        .map(|(range, _)| offset(range.start)..offset(range.end)),
                );
            }
        }
        let literals = strings(
            text,
            tokens[transcriber].offset,
            to,
            tokens[transcriber].line,
        );
        // The doc comments of the input of a macro that the transcriber invokes, too, of which
        // Rust makes `#[doc = "..."]` attributes, whose strings the macro may take for anything.
        let documentation: Vec<Literal> = doc_comments(
            text,
            tokens[transcriber].offset,
            to,
            tokens[transcriber].line,
        )
        .into_iter()
        .filter(|literal| {
            inputs
                .iter()
                .any(|input| (offset(input.start)..offset(input.end)).contains(&literal.offset))
        })
        .collect();
        if let Some(literal) = literals.iter().chain(&documentation).find(|literal| {
            !matched.iter().any(|range| range.contains(&literal.offset))
                && !literal_paths(literal, prelude).is_empty()
        }) {
            return Some(tokens.partition_point(|token| token.offset < literal.offset));
        }
        at = after;
    }
    None
}

/// What the crate's files tell of one another, which the parse of each needs.
struct Context {
    /// The names that `extern crate self as NAME;` gives the crate root in the extern prelude.
    prelude: BTreeSet<String>,
    /// The names of `prelude` that such a declaration without a `cfg` gives, in every
    /// configuration, where no other crate may take them.
    unconditional_prelude: BTreeSet<String>,
    /// The names that may stand for the `include!` macro, which puts the code of another file in a
    /// module: its own, and those that imports give it, through any chain of them.
    includes: BTreeSet<String>,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            prelude: BTreeSet::new(),
            unconditional_prelude: BTreeSet::new(),
            includes: BTreeSet::from(["include".to_owned()]),
        }
    }
}

impl Context {
    /// Returns the context of the crate whose files are `sources`. An import may give `include!`
    /// another name, under which other imports may take it in turn.
    fn of(sources: &[&Source]) -> Self {
        let (prelude, unconditional_prelude) = sources
            .iter()
            .find(|source| source.path == "src/lib.rs")
            .map(|source| extern_prelude(&source.text))
            .unwrap_or_default();
        let mut context = Self {
            prelude,
            unconditional_prelude,
            ..Self::default()
        };
        let mut renames = Vec::new();
        for source in sources {
            let blanked = blank(&source.text);
            let tokens = tokenize(&blanked);
            for at in 0..tokens.len() {
                if tokens[at].text != "use" || text_at(&tokens, at + 1) == "<" {
                    continue;
                }
                let mut end = at + 1;
                let mut imports = Vec::new();
                expand(&tokens, &mut end, &[], &mut imports);
                for (segments, kind, _) in imports {
                    if let (Some(name), Kind::Import(Some(alias))) = (segments.last(), kind) {
                        renames.push((name.clone(), alias));
                    }
                }
            }
        }
        loop {
            let before = context.includes.len();
            for (name, alias) in &renames {
                if context.includes.contains(name) {
                    context.includes.insert(alias.clone());
                }
            }
            if context.includes.len() == before {
                break;
            }
        }
        context
    }
}

/// Returns the names that `extern crate self as NAME;` gives the crate root in the extern prelude,
/// where `text`, the crate root's file, declares them at its top level, outside the brackets of
/// inline modules, blocks, and the input of macros, whose declarations bind names there alone, and
/// those of them that a declaration without a `cfg`, directly or through `cfg_attr`, gives.
fn extern_prelude(text: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let blanked = blank(text);
    let tokens = tokenize(&blanked);
    let mut depth = 0usize;
    let mut names = BTreeSet::new();
    let mut unconditional = BTreeSet::new();
    // Whether a `cfg` applies to the item after the outer attributes that the scan is past.
    let mut conditional = false;
    let mut at = 0;
    while at < tokens.len() {
        if depth == 0
            && tokens[at].text == "#"
            && let Some((end, inner, inside)) = attribute(&tokens, at)
        {
            conditional |= !inner && may_be(inside, "cfg");
            at = end;
            continue;
        }
        match tokens[at].text {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth = depth.saturating_sub(1),
            "extern"
                if depth == 0
                    && (1..4)
                        .map(|offset| text_at(&tokens, at + offset))
                        .eq(["crate", "self", "as"])
                    && is_identifier(text_at(&tokens, at + 4))
                    && text_at(&tokens, at + 4) != "_" =>
            {
                let name = unraw(text_at(&tokens, at + 4));
                if !conditional {
                    unconditional.insert(name.clone());
                }
                names.insert(name);
            }
            _ => {}
        }
        // An item ends at a `;` or a `}` at the top level, past which the attributes before it
        // apply to nothing.
        if depth == 0 && matches!(tokens[at].text, ";" | "}") {
            conditional = false;
        }
        at += 1;
    }
    (names, unconditional)
}

/// Returns the text that `body`, the body of a string literal that is not raw, stands for, with
/// its escapes decoded, and, for each byte of that text, the number of line feeds in `body` before
/// the byte's source. A `\` that ends a line joins the line to the next one, past the whitespace
/// that starts it, as in Rust.
fn unescape(body: &str) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut lines = Vec::new();
    let mut line = 0;
    let mut characters = body.chars().peekable();
    while let Some(character) = characters.next() {
        let decoded = if character == '\\' {
            match characters.next() {
                Some('x') => {
                    let digits: String = characters.by_ref().take(2).collect();
                    u8::from_str_radix(&digits, 16).ok().map(char::from)
                }
                Some('u') => {
                    let digits: String = characters
                        .by_ref()
                        .skip(1)
                        .take_while(|&digit| digit != '}')
                        .filter(|&digit| digit != '_')
                        .collect();
                    u32::from_str_radix(&digits, 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                Some('n') => Some('\n'),
                Some('r') => Some('\r'),
                Some('t') => Some('\t'),
                Some('0') => Some('\0'),
                Some(feed @ ('\n' | '\r')) => {
                    line += usize::from(feed == '\n');
                    while let Some(&next) = characters.peek()
                        && next.is_ascii_whitespace()
                    {
                        line += usize::from(next == '\n');
                        characters.next();
                    }
                    None
                }
                other => other,
            }
        } else {
            Some(character)
        };
        if let Some(decoded) = decoded {
            text.push(decoded);
            lines.resize(text.len(), line);
        }
        line += usize::from(character == '\n');
    }
    (text, lines)
}

/// Scans the region from `tokens[from]`, within whose own brackets only `stop` looks, outside the
/// angle brackets of generic arguments if `angles`. Returns `Ok` with the index of the first token
/// at the region's level that `stop` accepts, or `Err` with the index at which the region ends.
fn scan(
    tokens: &[Token<'_>],
    from: usize,
    angles: bool,
    stop: impl Fn(&str) -> bool,
) -> Result<usize, usize> {
    let mut depth = 0usize;
    let mut open = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(from) {
        if depth == 0 && open == 0 && stop(token.text) {
            return Ok(index);
        }
        match token.text {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" if depth == 0 => return Err(index),
            ")" | "]" | "}" => depth -= 1,
            "<" if angles && depth == 0 => open += 1,
            // The `>` of an arrow, as in `fn() -> T`, closes no angle bracket.
            ">" if angles && depth == 0 && text_at(tokens, index.wrapping_sub(1)) != "-" => {
                open = open.saturating_sub(1);
            }
            _ => {}
        }
    }
    Err(tokens.len())
}

/// Returns the index after the bracketed group that opens at `tokens[open]`, if it closes.
fn group_end(tokens: &[Token<'_>], open: usize) -> Option<usize> {
    scan(tokens, open + 1, false, |_| false)
        .err()
        .filter(|&close| close < tokens.len())
        .map(|close| close + 1)
}

/// Returns the index after the macro invocation whose bracketed arguments open at `tokens[open]`:
/// after their braces, or after the `;` that follows their parentheses or brackets.
fn macro_end(tokens: &[Token<'_>], open: usize) -> Option<usize> {
    let end = group_end(tokens, open)?;
    match text_at(tokens, open) {
        "{" => Some(end),
        "(" | "[" if text_at(tokens, end) == ";" => Some(end + 1),
        _ => None,
    }
}

/// Returns whether an expression with a block, which a match arm needs no comma after, starts at
/// `tokens[at]`: a block; a `const`, `unsafe`, or `async` block; a loop, labeled or not; a
/// `match`; or an `if`.
fn starts_block(tokens: &[Token<'_>], at: usize) -> bool {
    let (first, second) = (text_at(tokens, at), text_at(tokens, at + 1));
    // A label, as in `'outer: loop`, or a block, as in `async move {` but not the async closure of
    // `async move ||`.
    (first.starts_with('\'') && second == ":")
        || matches!(first, "{" | "if" | "match" | "while" | "for" | "loop")
        || (matches!(first, "const" | "unsafe" | "async") && second == "{")
        || (first == "async" && second == "move" && text_at(tokens, at + 2) == "{")
}

/// Returns the index after the expression with a block that starts at `tokens[at]`: after the
/// first block at its own level, or after the blocks of the `else` branches that follow that
/// block. The first block may be the condition of an `if`, in which case the expression's block
/// is left out, but never more than the expression.
fn block_end(tokens: &[Token<'_>], at: usize) -> Option<usize> {
    let mut from = at;
    loop {
        let open = scan(tokens, from, false, |text| matches!(text, "{" | ";")).ok()?;
        if text_at(tokens, open) != "{" {
            return None;
        }
        let end = group_end(tokens, open)?;
        if text_at(tokens, end) != "else" {
            return Some(end);
        }
        from = end + 1;
    }
}

/// The tokens that start an item, or the attribute before one, which neither the header of an
/// item nor an expression statement holds at its own level: a scan of either that meets one has
/// run past it, as through an item that the test does not know.
const ITEM_STARTS: [&str; 11] = [
    "#",
    "pub",
    "struct",
    "enum",
    "trait",
    "mod",
    "use",
    "let",
    "static",
    "type",
    "macro_rules",
];

/// Returns the index after the code that starts at `tokens[at]`, in a region that holds `list`,
/// if its tokens tell where it ends: a field, parameter, variant, or arm of such a list; an item;
/// a statement that ends with a `;`; a macro invocation; or a block, `if`, `match`, or loop, with
/// the blocks of the `else` branches of an `if`. `None` for anything else, such as a field of a
/// struct expression, whose references then count as production code: the test never takes code
/// for test-only beyond where it can tell that the test-only code ends.
fn extent(tokens: &[Token<'_>], at: usize, list: List) -> Option<usize> {
    let start = after_visibility(tokens, at);
    // The end of an element of a list: its comma, or the end of the list. An attribute or a
    // visibility starts the next element, so the scan has run past this one.
    let element = |from, angles| match scan(tokens, from, angles, |text| {
        matches!(text, "," | "#" | "pub")
    }) {
        Ok(end) => (text_at(tokens, end) == ",").then_some(end),
        Err(end) => Some(end),
    };
    match list {
        List::Fields => return element(start, true),
        List::Variants => return element(start, false),
        List::Arms => {
            // An arm whose expression has a block may end without a comma; any other ends with
            // one, unless it is the last arm.
            let arrow = scan(tokens, start, false, |text| matches!(text, "=>" | "#")).ok()?;
            let body = (text_at(tokens, arrow) == "=>").then_some(arrow + 1)?;
            if !starts_block(tokens, body) {
                return element(body, false);
            }
            let end = block_end(tokens, body)?;
            return Some(end + usize::from(text_at(tokens, end) == ","));
        }
        List::Code => {}
    }
    let mut at = start;
    while matches!(
        (text_at(tokens, at), text_at(tokens, at + 1)),
        (
            "default" | "async" | "unsafe" | "const" | "extern",
            "fn" | "unsafe" | "async" | "extern" | "impl" | "trait"
        )
    ) {
        at += 1;
    }
    // The body of an item, past the angle brackets of its generics, or the `;` that ends an item
    // without one.
    let item = || {
        let stop = scan(tokens, at + 1, true, |text| {
            matches!(text, ";" | "{" | "=" | "const") || ITEM_STARTS.contains(&text)
        })
        .ok()?;
        match text_at(tokens, stop) {
            ";" => Some(stop + 1),
            "{" => group_end(tokens, stop),
            _ => None,
        }
    };
    match (text_at(tokens, at), text_at(tokens, at + 1)) {
        _ if starts_block(tokens, at) => block_end(tokens, at),
        ("use" | "const" | "static" | "type" | "let", _) | ("extern", "crate") => {
            scan(tokens, at, false, |text| text == ";")
                .ok()
                .map(|end| end + 1)
        }
        ("fn" | "impl" | "trait" | "struct" | "enum" | "extern" | "mod", _) => item(),
        ("union", name) if is_identifier(name) => item(),
        ("macro_rules", "!") => macro_end(tokens, at + 3),
        _ => {
            let mut path = at;
            while is_identifier(text_at(tokens, path))
                || matches!(text_at(tokens, path), "::" | "$")
            {
                path += 1;
            }
            if path > at
                && text_at(tokens, path) == "!"
                && let Some(end) = macro_end(tokens, path + 1)
            {
                return Some(end);
            }
            // An expression statement, which ends with a `;`. A `,` or the end of the region shows
            // that the code is an element of a list instead, such as a field of a struct
            // expression, which ends there or later. A `=>`, or an item that the scan runs into,
            // shows nothing.
            match scan(tokens, at, false, |text| {
                matches!(
                    text,
                    ";" | "," | "=>" | "fn" | "impl" | "const" | "extern" | "union" | "macro"
                ) || ITEM_STARTS.contains(&text)
            }) {
                Ok(end) if text_at(tokens, end) == ";" => Some(end + 1),
                Ok(end) if text_at(tokens, end) == "," => Some(end),
                Err(end) if end < tokens.len() => Some(end),
                _ => None,
            }
        }
    }
}

/// Returns what the brackets that open lists hold, by their indices in `tokens`: the fields of the
/// structs and unions and the parameters of the functions that `tokens` declare, the variants of
/// their enums, and the arms of their `match` expressions. The fields of a variant, which open
/// after its name within the variants, are not among them.
fn lists(tokens: &[Token<'_>]) -> BTreeMap<usize, List> {
    let mut lists = BTreeMap::new();
    for (index, token) in tokens.iter().enumerate() {
        let declares = is_identifier(text_at(tokens, index + 1));
        let open = match token.text {
            "struct" | "union" | "enum" | "fn" if declares => {
                scan(tokens, index + 2, true, |text| {
                    matches!(text, "(" | "{" | ";")
                })
            }
            "match" => scan(tokens, index + 1, false, |text| matches!(text, "{" | ";")),
            _ => continue,
        };
        let Ok(open) = open else {
            continue;
        };
        let list = match (token.text, text_at(tokens, open)) {
            ("struct" | "union", "(" | "{") | ("fn", "(") => List::Fields,
            ("enum", "{") => List::Variants,
            ("match", "{") => List::Arms,
            _ => continue,
        };
        lists.insert(open, list);
    }
    lists
}

/// Expands the `use` tree at `tokens[*at]`, beneath `prefix`, into `imports`, each with its
/// segments, kind, and line, and moves `*at` past the tree.
fn expand(
    tokens: &[Token<'_>],
    at: &mut usize,
    prefix: &[String],
    imports: &mut Vec<(Vec<String>, Kind, usize)>,
) {
    let mut segments = prefix.to_vec();
    while let Some(token) = tokens.get(*at) {
        match token.text {
            // A `::` that starts the tree starts a path from the extern prelude.
            "::" if segments.is_empty() => {
                segments.push("::".to_owned());
                *at += 1;
            }
            "::" => *at += 1,
            "*" => {
                *at += 1;
                imports.push((segments, Kind::Glob, token.line));
                return;
            }
            "{" => {
                *at += 1;
                // An empty group imports nothing, but its prefix must still name something.
                if text_at(tokens, *at) == "}" {
                    imports.push((segments.clone(), Kind::Path, token.line));
                }
                while !matches!(text_at(tokens, *at), "}" | "") {
                    if text_at(tokens, *at) == "," {
                        *at += 1;
                        continue;
                    }
                    let before = *at;
                    expand(tokens, at, &segments, imports);
                    if *at == before {
                        *at += 1;
                    }
                }
                *at += 1;
                return;
            }
            text if is_identifier(text) => {
                *at += 1;
                // In a list, `self` names the module that the list is in.
                if text != "self" || segments.is_empty() {
                    segments.push(unraw(text));
                }
                if text_at(tokens, *at) == "::" {
                    continue;
                }
                let mut imported = segments.last().cloned();
                if text_at(tokens, *at) == "as" {
                    imported = Some(unraw(text_at(tokens, *at + 1))).filter(|alias| alias != "_");
                    *at += 2;
                }
                imports.push((segments, Kind::Import(imported), token.line));
                return;
            }
            _ => return,
        }
    }
}

/// Finds what the test reads in the file of `module`, whose text is `text`, in the crate whose
/// `context` it is.
fn parse(module: &[String], text: &str, context: &Context) -> Facts {
    let prelude = &context.prelude;
    let includes = &context.includes;
    let blanked = blank(text);
    let tokens = tokenize(&blanked);
    let lists = lists(&tokens);
    let mut facts = Facts {
        // Blanking leaves no literal, so a byte that is not ASCII is one of an identifier.
        non_ascii: tokens
            .iter()
            .find(|token| !token.text.is_ascii())
            .map(|token| token.line),
        ..Facts::default()
    };
    let mut frames: Vec<Frame> = Vec::new();
    let mut file_test = false;
    // The index of the token after the test-only code that the parser is in, if it is in any.
    let mut test_until = 0;
    let mut attributes_test = false;
    // Whether a `cfg` applies to what the attributes that the parser is past apply to.
    let mut attributes_cfg = false;
    // The index of the token after the visibility of what the last `cfg` applies to, such as the
    // `use` of `#[cfg(unix)] pub use ...;`, which the `cfg` makes conditional.
    let mut cfg_at = None;
    // The index of the first of the attributes that the parser is past, if it is past any.
    let mut attributes_from = None;
    // The paths of the attributes that the parser is past, each with whether its attribute is an
    // inner one, `#![...]`, which applies to the region that it is in, rather than to what follows.
    let mut pending_paths: Vec<(bool, Vec<WrittenPath>)> = Vec::new();
    let mut macro_export = false;
    // Whether `#[macro_export(local_inner_macros)]` applies to the next `macro_rules!`, whose
    // template's macro invocations then name macros of the crate root.
    let mut inner_macros = false;
    // The names of the `macro_rules!` that the file defines in every configuration outside its
    // brackets, which hide the macros of the same names from the rest of it.
    let mut file_macros = BTreeSet::new();
    let mut body_module = None;
    let mut at = 0;
    while at < tokens.len() {
        let token = tokens[at];
        let input = frames.last().is_some_and(|frame| frame.input);
        let scope = || {
            let mut scope = module.to_vec();
            scope.extend(frames.iter().filter_map(|frame| frame.module.clone()));
            scope
        };
        if token.text == "#"
            && let Some((end, inner, inside)) = attribute(&tokens, at)
        {
            attributes_from.get_or_insert(at);
            let open = at + 1 + usize::from(inner);
            let close = open + 1 + inside.len();
            let paths = attribute_paths(text, &tokens, open, close, input, prelude);
            pending_paths.push((inner, paths));
            if sets_path(inside) {
                facts.path_attributes.push(token.line);
            }
            if may_be(inside, "macro_use") {
                facts.macro_uses.push((scope(), token.line));
            }
            macro_export |= may_be(inside, "macro_export");
            inner_macros |= may_be(inside, "macro_export")
                && inside
                    .iter()
                    .any(|token| plain(token.text) == "local_inner_macros");
            let cfg = may_be(inside, "cfg");
            if inner && cfg {
                match frames.last() {
                    Some(frame) if frame.module.is_some() => facts.conditional.push(scope()),
                    None => facts.inner_cfg = true,
                    Some(_) => {}
                }
            }
            attributes_cfg |= !inner && cfg;
            // A macro may keep or drop the attributes of its input as it pleases, so there no
            // `cfg` makes anything test-only.
            if !input && guards_test(inside) {
                match (inner, frames.last_mut()) {
                    (false, _) => attributes_test = true,
                    (true, Some(frame)) => frame.test = true,
                    (true, None) => file_test = true,
                }
            }
            at = end;
            continue;
        }
        // What precedes the attributes: where the region holds code, only the start of the file
        // or a `{`, `}`, or `;` shows that an item or a statement starts after them, and a `<`,
        // `,`, `|`, `(`, or `[` that an element of a list does, such as a parameter of generics or
        // of a closure, or an argument.
        let first = attributes_from.take();
        let before = first.map_or("", |first: usize| text_at(&tokens, first.wrapping_sub(1)));
        let starts_statement = first.is_some() && matches!(before, "" | "{" | "}" | ";");
        if std::mem::take(&mut attributes_cfg) {
            cfg_at = Some(after_visibility(&tokens, at));
        }
        let conditional = cfg_at == Some(at);
        let local_inner = std::mem::take(&mut inner_macros);
        // Whether the region is test-only, as its inner attributes are.
        let region = file_test || at < test_until || frames.last().is_some_and(|frame| frame.test);
        if std::mem::take(&mut attributes_test) {
            let list = frames.last().map_or(List::Code, |frame| frame.list);
            let end = match list {
                List::Code if starts_statement => extent(&tokens, at, list),
                List::Code if matches!(before, "<" | "," | "|" | "(" | "[") => {
                    Some(element_end(&tokens, at))
                }
                List::Code => None,
                _ => extent(&tokens, at, list),
            };
            if let Some(end) = end {
                test_until = test_until.max(end);
            }
        }
        let test = region || at < test_until;
        // A block, such as the body of a function, binds the names that its imports bind for
        // itself alone. The input of a macro may expand anywhere.
        let local = !input && frames.iter().any(|frame| frame.module.is_none());
        // Whether a declaration or an import here binds its name in every configuration.
        let unconditional = !test && !input && !local && !conditional;
        let exported = std::mem::take(&mut macro_export);
        // An outer attribute is test-only where what it applies to is.
        for (inner, paths) in pending_paths.drain(..) {
            facts.add_paths(&scope(), paths, if inner { region } else { test }, local);
        }
        // A template names what only the macro's expansions resolve, where they expand, so the
        // test reads the template no further, unless it cannot follow what the template names.
        if token.text == "macro_rules"
            && text_at(&tokens, at + 1) == "!"
            && is_identifier(text_at(&tokens, at + 2))
            && matches!(text_at(&tokens, at + 3), "{" | "(" | "[")
        {
            // `#[macro_export]` puts the macro at the crate root, as `crate::NAME!`.
            let name = unraw(text_at(&tokens, at + 2));
            if exported {
                facts.exported.push((scope(), name.clone(), test));
            }
            // The macro hides others of its name from the rest of its region, a block too, where
            // every configuration defines it.
            if !test && !input && !conditional {
                frames
                    .last_mut()
                    .map_or(&mut file_macros, |frame| &mut frame.macros)
                    .insert(name);
            }
            let end = group_end(&tokens, at + 3).unwrap_or(tokens.len());
            let template = Template {
                open: at + 3,
                end,
                inner_macros: local_inner,
                includes,
            };
            let mut named = None;
            if let Some(position) =
                unfollowed_in_template(text, &tokens, &template, prelude, &mut named)
            {
                facts.templates.push((tokens[position].line, test));
            }
            if let Some(position) = named {
                facts.attribute_names.push((tokens[position].line, test));
            }
            at = end;
            continue;
        }
        if let Some((segments, last, end)) = qualified_path(&tokens, at, prelude) {
            facts.add_paths(&scope(), vec![(segments, tokens[last].line)], test, local);
            at = end;
            continue;
        }
        match token.text {
            "use" if text_at(&tokens, at + 1) != "<" => {
                let mut end = at + 1;
                let mut imports = Vec::new();
                expand(&tokens, &mut end, &[], &mut imports);
                for (segments, kind, line) in imports {
                    if unconditional && let Kind::Import(Some(name)) = &kind {
                        let imported = Some(segments.clone());
                        facts.unconditional.push((scope(), name.clone(), imported));
                    }
                    facts.references.push(Reference {
                        module: scope(),
                        segments,
                        kind,
                        line,
                        test,
                        local,
                    });
                }
                // The `;` that ends the declaration is next.
                at = end.max(at + 1);
                continue;
            }
            "mod" if is_identifier(text_at(&tokens, at + 1)) => {
                let declared_name = unraw(text_at(&tokens, at + 1));
                let mut declared = scope();
                declared.push(declared_name.clone());
                match text_at(&tokens, at + 2) {
                    // The scan of the macro's input, which reads its declarations wherever they
                    // are, records one in an input.
                    ";" if !input => facts.declared.push((declared, test)),
                    "{" => {
                        if input {
                            facts.input_modules.push((token.line, test));
                        }
                        body_module = Some(declared_name.clone());
                    }
                    _ => {}
                }
                if unconditional && matches!(text_at(&tokens, at + 2), ";" | "{") {
                    facts.unconditional.push((scope(), declared_name, None));
                }
            }
            // `extern crate self as NAME;` binds the crate root, and another crate's name names
            // none of the crate's modules.
            "extern"
                if text_at(&tokens, at + 1) == "crate"
                    && is_identifier(text_at(&tokens, at + 2)) =>
            {
                let (segments, binding) = extern_crate(&tokens, at);
                if unconditional && let Some(binding) = &binding {
                    facts.unconditional.push((scope(), binding.clone(), None));
                }
                facts.references.push(Reference {
                    module: scope(),
                    segments,
                    kind: Kind::Import(binding),
                    line: token.line,
                    test,
                    local,
                });
            }
            "{" | "(" | "[" => {
                let name = if token.text == "{" {
                    body_module.take()
                } else {
                    None
                };
                if let Some(name) = &name {
                    let mut inline = scope();
                    inline.push(name.clone());
                    facts.inline.push((inline, test));
                }
                // The fields of a variant follow its name, which starts the variant, among the
                // variants of an enum, unlike the brackets of an expression that sets a
                // discriminant.
                let list = match lists.get(&at) {
                    Some(&list) => list,
                    None if token.text != "["
                        && frames
                            .last()
                            .is_some_and(|frame| frame.list == List::Variants)
                        && is_identifier(text_at(&tokens, at.wrapping_sub(1)))
                        && matches!(text_at(&tokens, at.wrapping_sub(2)), "{" | "," | "]") =>
                    {
                        List::Fields
                    }
                    None => List::Code,
                };
                // The input of a macro, after `NAME!`, but not after `if !`, which negates.
                let invoked = text_at(&tokens, at.wrapping_sub(1)) == "!"
                    && is_name(text_at(&tokens, at.wrapping_sub(2)));
                if invoked && !input {
                    let close = group_end(&tokens, at).map_or(tokens.len(), |end| end - 1);
                    let paths = input_paths(text, &tokens, at, close, prelude);
                    facts.add_paths(&scope(), paths, test, local);
                    let hidden = |name: &str| {
                        file_macros.contains(name)
                            || frames.iter().any(|frame| frame.macros.contains(name))
                    };
                    input_declarations(
                        (text, &tokens),
                        (at, close),
                        (&scope(), test, local),
                        (includes, prelude),
                        hidden,
                        &mut facts,
                    );
                }
                // `include!` puts the code of another file here, which the test does not read,
                // unless a `macro_rules!` of the same name in scope hides it. Through a path, it
                // may be the builtin whatever the crate defines, so the test refuses it there.
                let macro_name = plain(text_at(&tokens, at.wrapping_sub(2)));
                let hidden = text_at(&tokens, at.wrapping_sub(3)) != "::"
                    && (file_macros.contains(macro_name)
                        || frames.iter().any(|frame| frame.macros.contains(macro_name)));
                if invoked && includes.contains(macro_name) && !hidden {
                    facts.includes.push((token.line, test));
                }
                frames.push(Frame {
                    test,
                    module: name,
                    list,
                    input: input || invoked,
                    macros: BTreeSet::new(),
                });
            }
            "}" | ")" | "]" => {
                frames.pop();
            }
            _ => {}
        }
        at += 1;
    }
    // Attributes that nothing follows, such as the inner attributes of a file without items.
    let mut scope = module.to_vec();
    scope.extend(frames.iter().filter_map(|frame| frame.module.clone()));
    let region = file_test || frames.last().is_some_and(|frame| frame.test);
    let input = frames.last().is_some_and(|frame| frame.input);
    let local = !input && frames.iter().any(|frame| frame.module.is_none());
    for (_, paths) in pending_paths {
        facts.add_paths(&scope, paths, region, local);
    }
    facts
}

/// Returns the module of the file at `path` in the crate, such as `["host", "platform"]` for
/// `src/host/platform/mod.rs`, or `None` for a path that is not a source file.
fn module_of(path: &str) -> Option<Vec<String>> {
    let relative = path.strip_prefix("src/")?.strip_suffix(".rs")?;
    let mut module: Vec<String> = relative.split('/').map(str::to_owned).collect();
    if module == ["lib"] || module.last().is_some_and(|last| last == "mod") {
        module.pop();
    }
    Some(module)
}

/// Returns whether the file of `module` holds test-only code alone: whether every declaration of
/// it is test-only, or is in a file that holds test-only code alone. `declarations` holds, for
/// each module that a `mod NAME;` declaration places in a file, the module of each file that
/// declares it and whether the declaration is test-only.
fn file_is_test(
    module: &[String],
    declarations: &BTreeMap<Vec<String>, Vec<(Vec<String>, bool)>>,
) -> bool {
    match declarations.get(module) {
        Some(declared) => declared
            .iter()
            .all(|(parent, test)| *test || file_is_test(parent, declarations)),
        // The crate root, which no file declares.
        None => false,
    }
}

/// Builds the dependencies between the direct children of `host` from `sources`, the crate's
/// files, or fails if their modules cannot be followed.
fn analyze(sources: &[Source]) -> Result<Graph, String> {
    let mut sources: Vec<&Source> = sources.iter().collect();
    sources.sort_by(|one, other| one.path.cmp(&other.path));
    let context = Context::of(&sources);
    let mut files = Vec::new();
    for source in sources {
        let module = module_of(&source.path)
            .ok_or_else(|| format!("{} is not a source file of the crate", source.path))?;
        let facts = parse(&module, &source.text, &context);
        files.push((source, module, facts));
    }
    // The modules that the crate builds in files: the crate root's, and those that the files of
    // such modules declare. The others are no part of it.
    let mut built = BTreeSet::from([Vec::new()]);
    loop {
        let before = built.len();
        for (_, module, facts) in &files {
            if built.contains(module) {
                built.extend(facts.declared.iter().map(|(declared, _)| declared.clone()));
            }
        }
        if built.len() == before {
            break;
        }
    }
    for (source, module, facts) in &files {
        if built.contains(module)
            && let Some(line) = facts.path_attributes.first()
        {
            return Err(format!(
                "{}:{line} places a module in a file with #[path], which this test cannot \
                 follow: let the module's file be the one that its name implies",
                source.path
            ));
        }
    }
    for (source, module, _) in &files {
        if !built.contains(module) && module.first().is_some_and(|first| first == "host") {
            return Err(format!(
                "no `mod` declaration names {}, so the crate does not build it: declare it or \
                 delete it",
                source.path
            ));
        }
    }
    files.retain(|(_, module, _)| built.contains(module));
    let mut declarations: BTreeMap<Vec<String>, Vec<(Vec<String>, bool)>> = BTreeMap::new();
    for (_, module, facts) in &files {
        for (declared, test) in &facts.declared {
            declarations
                .entry(declared.clone())
                .or_default()
                .push((module.clone(), *test));
        }
    }
    let production_file = |module: &Vec<String>| !file_is_test(module, &declarations);
    let in_host = |scope: &[String]| scope.first().is_some_and(|first| first == "host");
    for (source, module, facts) in &files {
        if production_file(module)
            && let Some(line) = facts.non_ascii
        {
            return Err(format!(
                "{}:{line} holds an identifier that is not ASCII, in its code or in a path that \
                 this test reads in a string or a doc comment, which Rust takes for every spelling \
                 that Unicode's NFC normalization makes alike, while this test compares \
                 identifiers as they are spelled: spell the identifier in ASCII",
                source.path
            ));
        }
        if let Some((_, line)) = facts.macro_uses.iter().find(|(scope, _)| in_host(scope)) {
            return Err(format!(
                "{}:{line} brings macros into scope with #[macro_use], whose uses name no path \
                 that this test can follow: import each macro by its path instead",
                source.path
            ));
        }
        if production_file(module)
            && let Some((line, _)) = facts.templates.iter().find(|(_, test)| !test)
        {
            return Err(format!(
                "{}:{line} defines a macro whose template declares or imports, names a path from \
                 a module or from the macro's input, puts another file's code in with include!, \
                 or invokes a macro under local_inner_macros, which this test cannot follow: \
                 name the paths in the input, or move the code out of the macro",
                source.path
            ));
        }
        if production_file(module)
            && let Some((line, _)) = facts.includes.iter().find(|(_, test)| !test)
        {
            return Err(format!(
                "{}:{line} puts the code of another file here with include!, which this test \
                 does not read: declare a module of that code instead",
                source.path
            ));
        }
        if production_file(module)
            && let Some((line, _)) = facts.input_modules.iter().find(|(_, test)| !test)
        {
            return Err(format!(
                "{}:{line} declares a module in the input of a macro, which may put the module's \
                 code in another module, where its paths name other modules, so this test cannot \
                 follow them: declare the module outside the macro",
                source.path
            ));
        }
        if production_file(module)
            && let Some((line, _)) = facts.input_prelude.iter().find(|(_, test)| !test)
        {
            return Err(format!(
                "{}:{line} gives the crate root a name in the input of a macro, which may put the \
                 name in the extern prelude, so this test cannot tell the paths that start with \
                 it: give the name outside the macro",
                source.path
            ));
        }
        if production_file(module)
            && let Some((line, _)) = facts.input_exports.iter().find(|(_, test)| !test)
        {
            return Err(format!(
                "{}:{line} holds a `macro_export` in the input of a macro, of which a template may \
                 make an attribute that exports a macro at the crate root from a module that this \
                 test cannot tell: export the macro with `#[macro_export]` outside the input",
                source.path
            ));
        }
    }
    // A module that the input of a macro declares in a file of its own, while the input of a macro
    // or a template holds a `path` or a `macro_use`, of which a template, maybe through other
    // macros, may make an attribute of the module that the test does not see.
    let first = |lines: fn(&Facts) -> &[(usize, bool)]| {
        files.iter().find_map(|(source, module, facts)| {
            lines(facts)
                .iter()
                .find(|(_, test)| !test && production_file(module))
                .map(|(line, _)| (source.path.as_str(), *line))
        })
    };
    if let (Some((file, line)), Some((named, named_line))) = (
        first(|facts| facts.input_files.as_slice()),
        first(|facts| facts.attribute_names.as_slice()),
    ) {
        return Err(format!(
            "{file}:{line} declares a module in a file of its own in the input of a macro, while \
             {named}:{named_line} gives a macro a `path` or a `macro_use`, of which a template may \
             make an attribute that sets the module's file or brings its macros into scope, which \
             this test does not see: declare the module outside the macro"
        ));
    }
    let mut modules = Modules {
        prelude: context.prelude,
        ..Modules::default()
    };
    for (_, module, facts) in &files {
        modules.paths.insert(module.clone());
        modules
            .paths
            .extend(facts.inline.iter().map(|(inline, _)| inline.clone()));
        modules
            .paths
            .extend(facts.declared.iter().map(|(declared, _)| declared.clone()));
    }
    let mut unconditional: BTreeMap<Vec<String>, BTreeSet<String>> = BTreeMap::new();
    let mut definitions: BTreeMap<Vec<String>, usize> = BTreeMap::new();
    let mut children = BTreeSet::new();
    // The modules that production code declares in every configuration, what the names that its
    // imports bind in every configuration, by their modules and names, take, and the names that
    // its declarations and imports may bind in some configuration.
    let mut declared = BTreeSet::new();
    let mut bound: BTreeMap<(Vec<String>, String), Vec<Vec<String>>> = BTreeMap::new();
    let mut bindable = BTreeSet::new();
    for (_, module, facts) in &files {
        if !production_file(module) {
            continue;
        }
        for (path, _) in facts
            .declared
            .iter()
            .chain(&facts.inline)
            .filter(|(_, test)| !test)
        {
            *definitions.entry(path.clone()).or_default() += 1;
            if let [host, child] = path.as_slice()
                && host == "host"
            {
                children.insert(child.clone());
            }
            if let Some((name, parent)) = path.split_last() {
                bindable.insert((parent.to_vec(), name.clone()));
            }
        }
        for reference in &facts.references {
            if let Kind::Import(Some(name)) = &reference.kind
                && !reference.test
                && !reference.local
            {
                bindable.insert((reference.module.clone(), name.clone()));
            }
        }
        for (scope, name, imported) in &facts.unconditional {
            match imported {
                Some(imported) => bound
                    .entry((scope.clone(), name.clone()))
                    .or_default()
                    .push(imported.clone()),
                None => {
                    declared.insert([scope.as_slice(), std::slice::from_ref(name)].concat());
                    unconditional
                        .entry(scope.clone())
                        .or_default()
                        .insert(name.clone());
                }
            }
        }
    }
    // A module whose own inner attributes hold a `cfg` exists only in some configurations, so its
    // name hides no other in every one.
    for (_, module, facts) in &files {
        for conditional in facts
            .conditional
            .iter()
            .chain(facts.inner_cfg.then_some(module))
        {
            declared.remove(conditional);
            if let Some((name, parent)) = conditional.split_last()
                && let Some(names) = unconditional.get_mut(parent)
            {
                names.remove(name);
            }
        }
    }
    // The modules that every configuration has: the crate root, and each module that one of them
    // declares, once, in every configuration. A module sorts after the one that declares it.
    let mut always = BTreeSet::from([Vec::new()]);
    for path in declared {
        if modules.paths.contains(&path)
            && definitions.get(&path) == Some(&1)
            && path
                .split_last()
                .is_some_and(|(_, parent)| always.contains(parent))
        {
            always.insert(path);
        }
    }
    // An import hides a name of the extern prelude only where it surely takes a module that every
    // configuration has: it may take a macro alone, which hides none.
    let certain = Certain {
        always: &always,
        bound: &bound,
        bindable: &bindable,
        prelude: &context.unconditional_prelude,
    };
    for ((scope, name), imported) in &bound {
        if imported
            .iter()
            .any(|imported| certain.module(scope, imported, &mut Vec::new()).is_some())
        {
            unconditional
                .entry(scope.clone())
                .or_default()
                .insert(name.clone());
        }
    }
    modules.unconditional = unconditional;
    modules.definitions = definitions;
    modules.children = children;
    let production =
        |module: &Vec<String>, reference: &Reference| !reference.test && production_file(module);
    // The macros that the crate exports at its root, as `crate::NAME!`, each from the module,
    // maybe an inline one, that defines it.
    for (_, module, facts) in &files {
        if !production_file(module) {
            continue;
        }
        for (scope, name, _) in facts.exported.iter().filter(|(_, _, test)| !test) {
            let name = format!("{name}!");
            modules
                .names
                .entry(vec![name.clone()])
                .or_default()
                .insert([scope.as_slice(), &[name]].concat());
        }
    }
    // What the modules outside the children of `host` import, which a path may go through: what
    // each name that they bind stands for, and the modules whose names their globs import. An
    // import may name what another one imports, in any of them and in any order, so the imports
    // are read again until they add nothing.
    loop {
        let mut grew = false;
        for (source, module, facts) in &files {
            for reference in &facts.references {
                if modules.child_of(&reference.module).is_some()
                    || reference.local
                    || !production(module, reference)
                {
                    continue;
                }
                for path in modules.resolve(&reference.module, &reference.segments) {
                    match &reference.kind {
                        // An import takes its name in each namespace that has it, a macro's too.
                        Kind::Import(Some(name)) => {
                            for (name, path) in [
                                (name.clone(), path.clone()),
                                (format!("{name}!"), as_macro(&path)),
                            ] {
                                let key = [reference.module.as_slice(), &[name]].concat();
                                grew |= modules.names.entry(key).or_default().insert(path);
                            }
                        }
                        Kind::Glob => {
                            if !modules.targets(&path).is_empty() {
                                return Err(format!(
                                    "{}:{} imports everything of {} into {}, so this test \
                                     cannot tell which child of host a path through it names: \
                                     import the items by name",
                                    source.path,
                                    reference.line,
                                    written(&reference.segments, false),
                                    written(
                                        &[["crate".to_owned()].as_slice(), &reference.module]
                                            .concat(),
                                        false
                                    ),
                                ));
                            }
                            grew |= modules
                                .globs
                                .entry(reference.module.clone())
                                .or_default()
                                .insert(path);
                        }
                        Kind::Import(None) | Kind::Path => {}
                    }
                }
                if let Some(cut) = modules.cut.take() {
                    return Err(cut_error(
                        &format!("{}:{}", source.path, reference.line),
                        &cut,
                    ));
                }
            }
        }
        if !grew {
            break;
        }
    }
    let mut graph = Graph {
        children: modules.children.clone(),
        ..Graph::default()
    };
    for (source, module, facts) in &files {
        for reference in &facts.references {
            let Some(from) = modules.child_of(&reference.module) else {
                continue;
            };
            if !production(module, reference) {
                continue;
            }
            let location = format!("{}:{}", source.path, reference.line);
            let mut found = BTreeSet::new();
            for path in modules.resolve(&reference.module, &reference.segments) {
                // A child's paths through the names that its imports bind count only through those
                // imports, so an import of the crate root or of `host` itself could hide any
                // dependency, and that of another module outside the children depends on every
                // child that the module's names stand for.
                if reference.kind != Kind::Path {
                    let mut alternatives = BTreeSet::new();
                    modules.expand(&path, &mut alternatives, &mut Questions::new());
                    if let Some(itself) = alternatives.iter().find(|alternative| {
                        matches!(alternative.as_slice(), [] | [_])
                            && alternative.first().is_none_or(|only| only == "host")
                    }) {
                        return Err(format!(
                            "{location} imports {} itself, which hides the children of host that \
                             the code that uses it depends on: import what it uses from those \
                             children",
                            if itself.is_empty() {
                                "the crate root"
                            } else {
                                "host"
                            }
                        ));
                    }
                    found.extend(modules.names_under(&path));
                    if matches!(reference.kind, Kind::Import(_)) {
                        found.extend(modules.targets(&as_macro(&path)));
                    }
                }
                found.extend(modules.targets(&path));
            }
            if let Some(cut) = modules.cut.take() {
                return Err(cut_error(&location, &cut));
            }
            let path = written(&reference.segments, reference.kind == Kind::Glob);
            for to in found.into_iter().filter(|to| *to != from) {
                graph
                    .edges
                    .entry((from.clone(), to))
                    .or_insert_with(|| Example {
                        location: location.clone(),
                        path: path.clone(),
                    });
            }
        }
    }
    Ok(graph)
}

/// Returns the failure of the reference at `location`, which goes through imports that alias one
/// another in a cycle that lengthens `cut`, which the test cannot follow to an end.
fn cut_error(location: &str, cut: &[String]) -> String {
    format!(
        "{location} goes through imports that alias one another in a cycle that lengthens {}, as \
         imports of different configurations may, so this test cannot follow it to an end: name \
         what it names by a path through no such cycle",
        written(cut, false)
    )
}

/// Returns the shortest path of dependencies in `edges` from `from` to `to`, with both, if there
/// is one.
fn shortest_path(
    edges: &BTreeMap<(String, String), Example>,
    from: &str,
    to: &str,
) -> Option<Vec<String>> {
    let mut previous: BTreeMap<&str, &str> = BTreeMap::new();
    let mut queue = VecDeque::from([from]);
    while let Some(child) = queue.pop_front() {
        if child == to {
            let mut path = vec![to.to_owned()];
            let mut current = to;
            while current != from {
                current = previous[current];
                path.push(current.to_owned());
            }
            path.reverse();
            return Some(path);
        }
        for (_, next) in edges.keys().filter(|(source, _)| source == child) {
            if next != from && !previous.contains_key(next.as_str()) {
                previous.insert(next, child);
                queue.push_back(next);
            }
        }
    }
    None
}

/// Returns the cycles of `edges`, shortest first: for each dependency that lies on a cycle, the
/// shortest cycle through it, from the cycle's alphabetically first child.
fn cycles(edges: &BTreeMap<(String, String), Example>) -> Vec<Vec<String>> {
    let mut cycles = BTreeSet::new();
    for (from, to) in edges.keys() {
        let Some(back) = shortest_path(edges, to, from) else {
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
    let mut cycles: Vec<_> = cycles.into_iter().collect();
    cycles.sort_by_key(Vec::len);
    cycles
}

/// Describes `cycles`, the cycles of `graph`, with the first reference along each of their
/// dependencies, under how to break them.
fn report(graph: &Graph, cycles: &[Vec<String>]) -> String {
    let mut text = String::from(
        "the direct children of nvx::host depend on one another in cycles. Break each one by \
         moving the item that a lower child takes from a higher one into the lower child, or \
         into a new child beneath both, and re-exporting it at its old public path, so that the \
         public API stays the same:\n",
    );
    for cycle in cycles {
        writeln!(text, "\n{} -> {}", cycle.join(" -> "), cycle[0]).unwrap();
        for (index, from) in cycle.iter().enumerate() {
            let to = &cycle[(index + 1) % cycle.len()];
            let example = &graph.edges[&(from.clone(), to.clone())];
            writeln!(
                text,
                "    {from} -> {to}: {}: {}",
                example.location, example.path
            )
            .unwrap();
        }
    }
    text
}

/// Returns the children of `host` in layers, lowest first, given the acyclic `graph`: each child
/// is one layer above the highest child that it depends on.
fn layers(graph: &Graph) -> Vec<Vec<&str>> {
    let mut layer: BTreeMap<&str, usize> = BTreeMap::new();
    while layer.len() < graph.children.len() {
        let placed = layer.len();
        for child in &graph.children {
            let dependencies = graph
                .edges
                .keys()
                .filter(|(from, _)| from == child)
                .map(|(_, to)| to.as_str());
            if !layer.contains_key(child.as_str())
                && dependencies.clone().all(|to| layer.contains_key(to))
            {
                let above = dependencies.map(|to| layer[to] + 1).max();
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

/// Returns the sources of a crate whose files are `files`, by their paths and texts.
fn crate_of(files: &[(&str, &str)]) -> Vec<Source> {
    files
        .iter()
        .map(|&(path, text)| Source {
            path: path.to_owned(),
            text: text.to_owned(),
        })
        .collect()
}

/// Returns the dependencies of the crate whose files are `files`, as `FROM -> TO`.
fn dependencies(files: &[(&str, &str)]) -> Vec<String> {
    let graph = analyze(&crate_of(files)).unwrap_or_else(|error| panic!("{error}"));
    graph
        .edges
        .keys()
        .map(|(from, to)| format!("{from} -> {to}"))
        .collect()
}

/// Returns the dependencies of the crate whose files are `files`, as `FROM -> TO at FILE:LINE`,
/// with the first reference that makes each.
fn located_dependencies(files: &[(&str, &str)]) -> Vec<String> {
    let graph = analyze(&crate_of(files)).unwrap_or_else(|error| panic!("{error}"));
    graph
        .edges
        .iter()
        .map(|((from, to), example)| format!("{from} -> {to} at {}", example.location))
        .collect()
}

/// Returns the tokens that remain of `text` once it is blanked, separated by spaces, after
/// checking that blanking kept the length and the lines of `text`.
fn remains(text: &str) -> String {
    let blanked = blank(text);
    assert_eq!(blanked.len(), text.len(), "{text}");
    assert!(
        blanked
            .bytes()
            .zip(text.bytes())
            .all(|(kept, original)| (kept == b'\n') == (original == b'\n')),
        "{text}"
    );
    let tokens: Vec<&str> = tokenize(&blanked).iter().map(|token| token.text).collect();
    tokens.join(" ")
}

/// Returns the paths that the `use` declarations in `text` import, written as
/// `PATH as NAME`, `PATH as _`, or `PATH::*` for a glob.
fn imports(text: &str) -> Vec<String> {
    let module = ["host".to_owned(), "a".to_owned()];
    parse(&module, text, &Context::default())
        .references
        .iter()
        .map(|reference| {
            let path = written(&reference.segments, reference.kind == Kind::Glob);
            match &reference.kind {
                Kind::Import(Some(name)) => format!("{path} as {name}"),
                Kind::Import(None) => format!("{path} as _"),
                Kind::Glob => path,
                Kind::Path => format!("{path} (not imported)"),
            }
        })
        .collect()
}

#[test]
fn the_children_of_host_depend_on_one_another_without_cycles() {
    let graph = analyze(&read_sources()).unwrap_or_else(|error| panic!("{error}"));
    let cycles = cycles(&graph.edges);
    assert!(cycles.is_empty(), "{}", report(&graph, &cycles));
    // The children stack in layers, which `--nocapture` shows.
    for (index, children) in layers(&graph).iter().enumerate() {
        println!("layer {index}: {}", children.join(", "));
    }
}

#[test]
fn the_graph_holds_well_known_dependencies_of_the_sources() {
    let graph = analyze(&read_sources()).unwrap_or_else(|error| panic!("{error}"));
    for (from, to) in [("vm", "spawn"), ("cli", "sandbox"), ("spawn", "platform")] {
        assert!(
            graph.edges.contains_key(&(from.to_owned(), to.to_owned())),
            "the graph lacks {from} -> {to}"
        );
    }
}

#[test]
fn blanking_keeps_code_and_lines_but_not_comments_or_literals() {
    assert_eq!(remains("a // crate::x\nb"), "a b");
    assert_eq!(remains("a /// crate::x\n//! crate::y\nb"), "a b");
    assert_eq!(remains("a /** crate::x */ b"), "a b");
    assert_eq!(remains("a /* x /* crate::y */ crate::z */ b"), "a b");
    assert_eq!(remains(r#"a "x \" crate::y \\" b"#), "a b");
    assert_eq!(remains("a \"x\ncrate::y\n\" b"), "a b");
    assert_eq!(remains(r#"a b"crate::x" c"crate::y" b"#), "a b");
    assert_eq!(remains(r#"a r"x\" b"#), "a b");
    assert_eq!(remains(r##"a r#"x "crate::y" z"# b"##), "a b");
    assert_eq!(remains(r###"a r##"x "# crate::y"## b"###), "a b");
    assert_eq!(remains(r##"a br#"crate::x"# cr"crate::y" b"##), "a b");
    assert_eq!(
        remains(r#"a '"' crate::y '\'' '\\' '\u{1F600}' 'é' b'"' b'\'' b"#),
        "a crate :: y b"
    );
    assert_eq!(
        remains("fn f<'a>(x: &'a str) { 'outer: loop { break 'outer; } crate::y }"),
        "fn f < 'a > ( x : & 'a str ) { 'outer : loop { break 'outer ; } crate :: y }"
    );
    assert_eq!(remains("r#type crate::y"), "r#type crate :: y");
    assert_eq!(
        remains("crate::x(br, b, r) b'x'"),
        "crate :: x ( br , b , r )"
    );
}

#[test]
fn use_trees_expand_into_the_paths_that_they_import() {
    assert_eq!(
        imports(
            "pub(crate) use crate::host::{\n    self,\n    b::{One, two as three},\n    c::*,\n    \
             d::{self as e, f::{self}},\n    g::G as _,\n};"
        ),
        [
            "crate::host as host",
            "crate::host::b::One as One",
            "crate::host::b::two as three",
            "crate::host::c::*",
            "crate::host::d as e",
            "crate::host::d::f as f",
            "crate::host::g::G as _",
        ]
    );
    assert_eq!(
        imports("use ::std::fmt;\nuse super::super::x::Y;\nuse self::z;"),
        [
            "::std::fmt as fmt",
            "super::super::x::Y as Y",
            "self::z as z"
        ]
    );
    // Precise capturing, `use<'a>`, imports nothing.
    assert_eq!(
        imports("fn f<'a>() -> impl Sized + use<'a> { crate::x::y() }"),
        ["crate::x::y (not imported)"]
    );
    let lines: Vec<usize> = parse(
        &[],
        "use crate::{\n    a,\n    b::c,\n};",
        &Context::default(),
    )
    .references
    .iter()
    .map(|reference| reference.line)
    .collect();
    assert_eq!(lines, [2, 3]);
}

#[test]
fn test_only_code_is_not_a_dependency() {
    let a = r#"
#[cfg(test)]
use crate::host::b::One;
#[cfg(all(test, unix))]
pub(crate) fn helper<T, U>() -> T where T: Default, U: Default { crate::host::b::two() }
#[cfg(not(test))]
use crate::host::c::Three;
#[cfg(any(test, feature = "cli"))]
use crate::host::d::Four;
#[cfg(any(test, all(test, unix)))]
use crate::host::b::Five;
#[test]
fn check() { crate::host::b::six(); }
mod tests { use crate::host::m::Seven; }
#[cfg(test)]
impl Item { fn new() -> Self { crate::host::b::eight() } }
#[cfg(test)]
static TABLE: Map<u8, u8> = crate::host::b::NINE;
static OTHER: u8 = crate::host::e::TEN;
#[cfg(test)]
mod helpers;
fn production(value: u8) -> u8 {
    #[cfg(test)]
    crate::host::b::eleven();
    #[cfg(test)]
    if value == 0 {
        crate::host::b::twenty_six();
    } else if value == 1 {
        crate::host::b::twenty_seven();
    } else {
        crate::host::b::twenty_eight();
    }
    crate::host::j::twenty_nine();
    match value {
        #[cfg(test)]
        0 => { crate::host::b::twelve() }
        #[cfg(test)]
        1 => loop { crate::host::b::thirty_three(); }
        #[cfg(test)]
        2 => async move || crate::host::b::thirty_five(),
        3 => { crate::host::l::thirty_six(); }
        _ => crate::host::f::thirteen(),
    }
}
struct Fields {
    #[cfg(test)]
    value: Result<crate::host::b::Eighteen, crate::host::b::Nineteen>,
    #[cfg(test)]
    callback: fn(u8) -> Result<crate::host::b::Twenty, Vec<crate::host::b::TwentyOne>>,
    other: crate::host::i::TwentyTwo,
}
fn parameters(
    #[cfg(test)] value: Map<crate::host::b::TwentyThree, crate::host::b::TwentyFour>,
    other: crate::host::i::TwentyFive,
) {}
pub struct Tuple(
    #[cfg(test)] Result<crate::host::b::TwentySix, crate::host::b::TwentySeven>,
    crate::host::k::TwentyEight,
);
#[cfg(test)]
crate::make_tests! { crate::host::b::twenty_nine() }
pub fn after_a_macro() {
    crate::host::k::thirty();
}
#[cfg(test)]
fn bounded<T: Bound<{ 1 }>>() {
    crate::host::b::thirty_one();
}
pub fn after_a_bound() {
    crate::host::k::thirty_two();
}
#[cfg(test)]
fn label() -> &'static str {
    crate::host::b::thirty_four()
}
"#;
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            (
                "src/host/mod.rs",
                "pub mod a;\npub mod b;\npub mod c;\npub mod d;\npub mod e;\npub mod f;\n\
                 pub mod g;\n#[cfg(test)]\nmod h;\npub mod i;\npub mod j;\npub mod k;\npub mod l;\n\
                 pub mod m;"
            ),
            ("src/host/a.rs", a),
            ("src/host/a/helpers.rs", "use crate::host::b::Fourteen;"),
            ("src/host/b.rs", ""),
            ("src/host/c.rs", ""),
            ("src/host/d.rs", ""),
            ("src/host/e.rs", ""),
            ("src/host/f.rs", ""),
            (
                "src/host/g.rs",
                "#![cfg(test)]\nuse crate::host::b::Fifteen;"
            ),
            ("src/host/h.rs", "use crate::host::b::Sixteen;\nmod nested;"),
            ("src/host/h/nested.rs", "use crate::host::b::Seventeen;"),
            ("src/host/i.rs", ""),
            ("src/host/j.rs", ""),
            ("src/host/k.rs", ""),
            ("src/host/l.rs", ""),
            ("src/host/m.rs", ""),
        ]),
        [
            "a -> c", "a -> d", "a -> e", "a -> f", "a -> i", "a -> j", "a -> k", "a -> l",
            "a -> m"
        ]
    );
}

#[test]
fn guarded_code_is_test_only_no_further_than_its_tokens_show() {
    // A guarded field of a struct expression, an argument, or a parameter of generics or of a
    // closure ends at its first `,`, `>`, `|`, or the end of its list, or later, past a `<` that
    // compares or opens type arguments, so what follows it counts. A guarded `if` whose condition
    // is a block ends with that block, and an item of a kind that the test does not know ends
    // nowhere that its tokens show, short of the next item. In the input of a macro, which may
    // drop its attributes, no `cfg` makes anything test-only.
    let a = r#"
fn build() -> Data {
    #[cfg(test)]
    if { true } {
        crate::host::b::one();
    }
    Data {
        #[cfg(test)]
        debug: crate::host::c::two() < 2,
        value: crate::host::d::three(),
    }
}
enum Kind {
    A = Data {
        #[cfg(test)]
        debug: 1 < 2,
        value: crate::host::e::FOUR,
    }
    .value,
    B = pick(#[cfg(test)] 1 < 2, crate::host::f::FIVE),
}
#[cfg(test)]
auto trait Marker {}
pub fn after_a_marker() {
    crate::host::g::six();
}
const AFTER: u8 = 1;
fn generic<#[cfg(test)] 'a, T>() {
    crate::host::h::seven();
}
fn constant<#[cfg(test)] const N: usize, T>() {}
use crate::host::i::Eight;
fn closure() {
    let callback = |#[cfg(test)] unused: ()| {
        crate::host::j::nine();
    };
}
cfg_if::cfg_if! {
    if #[cfg(test)] {
        use crate::host::b::Ten;
    } else {
        use crate::host::k::Eleven;
    }
}
"#;
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            (
                "src/host/mod.rs",
                "pub mod a;\npub mod b;\npub mod c;\npub mod d;\npub mod e;\npub mod f;\n\
                 pub mod g;\npub mod h;\npub mod i;\npub mod j;\npub mod k;"
            ),
            ("src/host/a.rs", a),
            ("src/host/b.rs", ""),
            ("src/host/c.rs", ""),
            ("src/host/d.rs", ""),
            ("src/host/e.rs", ""),
            ("src/host/f.rs", ""),
            ("src/host/g.rs", ""),
            ("src/host/h.rs", ""),
            ("src/host/i.rs", ""),
            ("src/host/j.rs", ""),
            ("src/host/k.rs", ""),
        ]),
        [
            "a -> b", "a -> d", "a -> e", "a -> f", "a -> g", "a -> h", "a -> i", "a -> j",
            "a -> k"
        ]
    );
}

#[test]
fn paths_in_attributes_count_where_what_the_attributes_apply_to_does() {
    // What another attribute takes as its arguments applies nothing, though it holds a `cfg_attr`
    // that requires `test`, or a `doc`.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            ("src/host/mod.rs", "pub mod a;\npub mod b;\npub mod c;"),
            (
                "src/host/a.rs",
                "#[custom(cfg_attr(test, crate::host::b::f))]\nfn g() {}\n\
                 #[custom(doc = \"crate::host::c::f\")]\nfn h() {}",
            ),
            ("src/host/b.rs", ""),
            ("src/host/c.rs", ""),
        ]),
        ["a -> b", "a -> c"]
    );
    // A derive that names a trait which `b` re-exports, while `b` uses the type that derives it,
    // makes a cycle.
    let graph = analyze(&crate_of(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        (
            "src/host/a.rs",
            "#[derive(Debug, crate::host::b::Serialize)]\npub struct A;",
        ),
        (
            "src/host/b.rs",
            "pub use serde::Serialize;\npub fn f(_: super::a::A) {}",
        ),
    ]))
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(cycles(&graph.edges), [["a", "b"]]);
    // The paths of an attribute and its strings, with their escapes decoded, count where what it
    // applies to does: an outer attribute, as the item, field, or other code that follows it, and
    // an inner one, as the region that it is in.
    let a = r#"#[derive(crate::host::b::Trait)]
struct One;
#[cfg_attr(unix, derive(crate::host::c::Trait))]
struct Two;
struct Three {
    #[serde(with = "super::d::codec")]
    value: u8,
    #[serde(
        default,
        deserialize_with = r"crate::host::e::parse",
    )]
    other: u8,
    #[cfg(test)]
    #[serde(with = "super::f::codec")]
    debug: u8,
}
#[derive(crate::host::f::Trait)]
#[cfg(test)]
struct Four;
mod inner {
    #![crate::host::g::attribute]
    #[cfg(test)]
    fn check() {}
}
#[cfg(test)]
mod checks {
    #![crate::host::f::attribute]
}
struct Five {
    #[serde(with = "crate::host::\x68::codec")]
    value: u8,
    #[serde(deserialize_with = "crate::host::\u{69}::parse")]
    other: u8,
    #[serde(serialize_with = "crate::host::\
        j::write")]
    third: u8,
}
"#;
    assert_eq!(
        located_dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            (
                "src/host/mod.rs",
                "pub mod a;\npub mod b;\npub mod c;\npub mod d;\npub mod e;\npub mod f;\n\
                 pub mod g;\npub mod h;\npub mod i;\npub mod j;"
            ),
            ("src/host/a.rs", a),
            ("src/host/b.rs", ""),
            ("src/host/c.rs", ""),
            ("src/host/d.rs", ""),
            ("src/host/e.rs", ""),
            ("src/host/f.rs", ""),
            ("src/host/g.rs", ""),
            ("src/host/h.rs", ""),
            ("src/host/i.rs", ""),
            ("src/host/j.rs", ""),
        ]),
        [
            "a -> b at src/host/a.rs:1",
            "a -> c at src/host/a.rs:3",
            "a -> d at src/host/a.rs:6",
            "a -> e at src/host/a.rs:10",
            "a -> g at src/host/a.rs:21",
            "a -> h at src/host/a.rs:30",
            "a -> i at src/host/a.rs:32",
            "a -> j at src/host/a.rs:35",
        ]
    );
    // Documentation names no dependency, also in a `doc` attribute, as in a doc comment.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "#[doc = \"[`spawn`](crate::host::b::spawn)\"]\n\
                 #[cfg_attr(unix, doc = \"See crate::host::b::spawn.\")]\n\
                 #[doc(alias = \"crate::host::b::Alias\")]\npub struct A;"
            ),
            ("src/host/b.rs", "pub fn spawn(_: super::a::A) {}"),
        ]),
        ["b -> a"]
    );
}

#[test]
fn paths_name_the_children_that_hold_what_they_name() {
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\nmod protocol;\npub use host::a::Item;\n\
                 pub use self::host::b::{other as renamed};"
            ),
            (
                "src/host/mod.rs",
                "pub mod a;\npub mod b;\npub mod c;\nmod d;\npub mod e;\npub mod f;\n\
                 pub use self::f::Reexported;"
            ),
            ("src/host/a.rs", "pub struct Item;"),
            ("src/host/b/mod.rs", "mod inner;\npub fn other() {}"),
            (
                "src/host/b/inner.rs",
                "use super::super::c::Thing;\n\
                 fn f() { self::g(); super::super::d::h(); super::other(); }\nfn g() {}"
            ),
            (
                "src/host/c.rs",
                "mod nested {\n    fn f() { super::super::e::y(); }\n}\n\
                 pub(in crate::host) fn g() -> crate::Item {\n    crate::renamed();\n    \
                 crate::host::Reexported;\n    crate::host::b::inner::Thing\n}"
            ),
            (
                "src/host/d.rs",
                "pub fn h() { crate::host::d::own(); self::own(); crate::protocol::X; }\n\
                 fn own() {}"
            ),
            (
                "src/host/e.rs",
                "use super::a::{self, Item as Alias};\nuse std::fmt;\npub fn y() {}"
            ),
            ("src/host/f.rs", "pub struct Reexported;"),
        ]),
        [
            "b -> c", "b -> d", "c -> a", "c -> b", "c -> e", "c -> f", "e -> a"
        ]
    );
}

#[test]
fn imports_of_the_root_and_host_resolve_through_chains_and_every_alternative() {
    // `host` imports what the crate root imports from a child, which the root's file, read after
    // `host`'s, declares.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;\npub use host::a::Item;"),
            (
                "src/host/mod.rs",
                "pub mod a;\npub mod b;\npub use crate::Item;"
            ),
            ("src/host/a.rs", "pub struct Item;"),
            ("src/host/b.rs", "fn f() -> crate::host::Item { todo!() }"),
        ]),
        ["b -> a"]
    );
    // `cfg`s choose between imports of one name from different children: a path through it
    // depends on both, also through a chain.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\n#[cfg(unix)]\npub use host::a::Thing;\n#[cfg(windows)]\n\
                 pub use host::b::Thing;"
            ),
            (
                "src/host/mod.rs",
                "pub mod a;\npub mod b;\npub mod c;\npub mod d;\npub use crate::Thing as Other;"
            ),
            ("src/host/a.rs", ""),
            ("src/host/b.rs", ""),
            ("src/host/c.rs", "fn f(_: crate::Thing) {}"),
            ("src/host/d.rs", "use super::Other;"),
        ]),
        ["c -> a", "c -> b", "d -> a", "d -> b"]
    );
    // An import renames a name that its module imports, without a path to that module.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\npub use host::a::Item;\npub use Item as Alias;"
            ),
            (
                "src/host/mod.rs",
                "pub mod a;\npub mod b;\npub mod c;\npub use c::Thing;\npub use Thing as Other;"
            ),
            ("src/host/a.rs", "pub struct Item;"),
            ("src/host/b.rs", "fn f(_: crate::Alias, _: super::Other) {}"),
            ("src/host/c.rs", "pub struct Thing;"),
        ]),
        ["b -> a", "b -> c"]
    );
    // A module of the crate root hides no import of the same name, which other `cfg`s choose.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\n#[cfg(unix)]\npub mod backend {\n    pub fn run() {}\n}\n\
                 #[cfg(windows)]\npub use host::b as backend;"
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "pub fn f() {\n    crate::backend::run();\n}"
            ),
            ("src/host/b.rs", "pub fn run() {\n    super::a::f();\n}"),
        ]),
        ["a -> b", "b -> a"]
    );
    // The crate root's name for itself, from `extern crate self as NAME;`, names it in a path from
    // the crate root, as the extern prelude's name does elsewhere.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;\nextern crate self as this;"),
            ("src/host/mod.rs", "pub mod a;\npub mod b;\npub mod c;"),
            (
                "src/host/a.rs",
                "pub fn forth() {\n    crate::this::host::b::back();\n}"
            ),
            (
                "src/host/b.rs",
                "pub fn back() {\n    super::a::forth();\n}"
            ),
            ("src/host/c.rs", "fn f() {\n    this::host::a::forth();\n}"),
        ]),
        ["a -> b", "b -> a", "c -> a"]
    );
    // An import in a function body, of the crate root or `host`, names nothing for their modules.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\npub use host::a::Item;\nfn helper() {\n    \
                 use crate::host::c::Item;\n}"
            ),
            (
                "src/host/mod.rs",
                "pub mod a;\npub mod b;\npub mod c;\npub mod d;\npub use self::a::Thing;\n\
                 fn local() {\n    use self::d::Thing;\n}"
            ),
            ("src/host/a.rs", "pub struct Item;\npub struct Thing;"),
            ("src/host/b.rs", "fn f(_: crate::Item, _: super::Thing) {}"),
            ("src/host/c.rs", "pub struct Item;"),
            ("src/host/d.rs", "pub struct Thing;"),
        ]),
        ["b -> a"]
    );
    // A macro that a child exports is an item of the crate root, which the child holds, also
    // when a `cfg_attr` may export it, or the child is inline.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "#[macro_export]\nmacro_rules! shared {\n    () => {};\n}"
            ),
            ("src/host/b.rs", "fn f() {\n    crate::shared!();\n}"),
        ]),
        ["b -> a"]
    );
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            (
                "src/host/mod.rs",
                "pub mod a {\n    #[cfg_attr(unix, macro_export)]\n    \
                 macro_rules! shared {\n        () => {};\n    }\n    \
                 fn f() {\n        super::b::g();\n    }\n}\npub mod b;"
            ),
            ("src/host/b.rs", "pub fn g() {\n    crate::shared!();\n}"),
        ]),
        ["a -> b", "b -> a"]
    );
    // Macros have names of their own: a macro named `host` hides no path through `host`, while
    // an invocation of it and an import of its name name it.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            (
                "src/host/mod.rs",
                "pub mod a;\npub mod b;\npub mod c;\npub mod d;"
            ),
            (
                "src/host/a.rs",
                "#[macro_export]\nmacro_rules! host {\n    () => {};\n}\n\
                 #[r#macro_export]\nmacro_rules! shared {\n    () => {};\n}\n\
                 fn f() {\n    crate::host::b::g();\n}"
            ),
            (
                "src/host/b.rs",
                "pub fn g() {\n    crate::host::b::h();\n}\npub fn h() {}"
            ),
            ("src/host/c.rs", "fn f() {\n    crate::host!();\n}"),
            (
                "src/host/d.rs",
                "use crate::shared;\nfn f() {\n    shared!();\n}"
            ),
        ]),
        ["a -> b", "c -> a", "d -> a"]
    );
    // So an import of a macro as `_` takes it too, and an import of a macro alone hides no name
    // of the extern prelude, which names modules. An empty `use` group still names its prefix.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;\nextern crate self as nvx;"),
            (
                "src/host/mod.rs",
                "pub mod a;\npub mod b;\npub mod c;\npub mod d;"
            ),
            ("src/host/a.rs", "use crate::helper as _;"),
            (
                "src/host/b.rs",
                "#[macro_export]\nmacro_rules! helper {\n    () => {};\n}"
            ),
            (
                "src/host/c.rs",
                "use crate::helper as nvx;\nfn f() {\n    nvx::host::d::f();\n}"
            ),
            ("src/host/d.rs", "use crate::host::a::{};"),
        ]),
        ["a -> b", "c -> b", "c -> d", "d -> a"]
    );
    // Neither does an import of another crate's macro, or of one that a re-export renames.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\nextern crate self as nvx;\npub use helper as reexported;"
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;\npub mod c;"),
            (
                "src/host/a.rs",
                "use std::println as nvx;\nfn f() {\n    nvx::host::b::f();\n}"
            ),
            (
                "src/host/b.rs",
                "#[macro_export]\nmacro_rules! helper {\n    () => {};\n}"
            ),
            (
                "src/host/c.rs",
                "use crate::reexported as nvx;\nfn f() {\n    nvx::host::a::f();\n}"
            ),
        ]),
        ["a -> b", "c -> a", "c -> b"]
    );
    // An import of a module hides it, through other imports too, but not one of a module that
    // only some configurations have, which others may give a macro's name.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "extern crate self as nvx;\npub mod local {\n    pub mod host {\n        \
                 pub mod b {\n            pub fn f() {}\n        }\n    }\n}\npub mod host;"
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "use crate::local as alias;\nuse alias as nvx;\nfn f() {\n    nvx::host::b::f();\n}"
            ),
            ("src/host/b.rs", ""),
        ]),
        Vec::<String>::new()
    );
    // So does one from a name of the crate root in the extern prelude, with or without `::`, but
    // not one through a glob, whose names depend on visibility, which the test does not read: it
    // counts the path from the extern prelude too.
    for import in [
        "use ::this::local as nvx;",
        "use this::local as nvx;",
        "use crate::bridge::*;\nuse alias as nvx;",
    ] {
        let edges = dependencies(&[
            (
                "src/lib.rs",
                "extern crate self as this;\nextern crate self as nvx;\npub mod local {\n    \
                 pub mod host {\n        pub mod b {\n            pub fn f() {}\n        }\n    \
                 }\n}\npub mod bridge {\n    pub use crate::local as alias;\n}\npub mod host;",
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                &format!("{import}\nfn f() {{\n    nvx::host::b::f();\n}}"),
            ),
            ("src/host/b.rs", ""),
        ]);
        assert_eq!(
            edges.is_empty(),
            !import.contains('*'),
            "{import}: {edges:?}"
        );
    }
    // But a name of the extern prelude that the module may bind itself, in some configuration, may
    // name what the module binds, which hides the extern prelude's.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "extern crate self as ext;\nextern crate self as nvx;\npub mod thing {\n    \
                 pub mod host {\n        pub mod b {\n            pub fn f() {}\n        }\n    \
                 }\n}\npub mod host;",
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "#[cfg(feature = \"local\")]\nmod ext {\n    macro_rules! thing {\n        \
                 () => {};\n    }\n    pub(crate) use thing;\n}\nuse ext::thing as nvx;\n\
                 fn f() {\n    nvx::host::b::f();\n}",
            ),
            ("src/host/b.rs", "pub fn f() {}"),
        ]),
        ["a -> b"]
    );
    // Only the crate root's own `extern crate self as NAME;` gives a name of the extern prelude,
    // not one of an inline module or a block of its file.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "extern crate self as nvx;\npub mod println {}\nmod probe {\n    \
                 extern crate self as std;\n}\npub mod host;",
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "use std::println as nvx;\nfn f() {\n    nvx::host::b::f();\n}",
            ),
            ("src/host/b.rs", "pub fn f() {}"),
        ]),
        ["a -> b"]
    );
    // Nor does a name that a `cfg` guards, which other configurations may give another crate,
    // through which an import may take a macro alone, as `ext::println` does on Unix here.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "extern crate self as nvx;\n#[cfg(unix)]\nextern crate std as ext;\n\
                 #[cfg(windows)]\nextern crate self as ext;\npub mod println {\n    pub mod host \
                 {\n        pub mod b {\n            pub fn f() {}\n        }\n    }\n}\n\
                 pub mod host;",
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "use ext::println as nvx;\nfn f() {\n    nvx::host::b::f();\n}",
            ),
            ("src/host/b.rs", "pub fn f() {}"),
        ]),
        ["a -> b"]
    );
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "extern crate self as nvx;\n#[cfg(unix)]\npub mod local {\n    pub mod host {\n        \
                 pub mod b {\n            pub fn f() {}\n        }\n    }\n}\n\
                 #[cfg(windows)]\npub use std::println as local;\npub mod host;"
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "use crate::local as nvx;\nfn f() {\n    nvx::host::b::f();\n}"
            ),
            ("src/host/b.rs", ""),
        ]),
        ["a -> b"]
    );
    // Nor does an import or a module of the name that only some configurations have, past the
    // visibility that the `cfg` applies to with the rest of it.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "extern crate self as nvx;\npub mod local {}\npub mod host;"
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;\npub mod c;"),
            (
                "src/host/a.rs",
                "#[cfg(unix)]\npub(crate) use crate::local as nvx;\nfn f() {\n    \
                 nvx::host::b::f();\n}"
            ),
            ("src/host/b.rs", ""),
            (
                "src/host/c.rs",
                "#[cfg(unix)]\npub mod nvx {}\nfn f() {\n    nvx::host::b::f();\n}"
            ),
        ]),
        ["a -> b", "c -> b"]
    );
    // Imports of different configurations may alias one another in a cycle that lengthens a path
    // without end, which no configuration holds, but which the test cannot follow to an end, so
    // it fails, as it does where a path's own aliases lengthen it as they recur.
    let cut = analyze(&crate_of(&[
        (
            "src/lib.rs",
            "pub mod host;\n#[cfg(unix)]\npub mod b {\n    pub mod x {\n        \
             pub struct Thing;\n    }\n}\n#[cfg(unix)]\npub use b::x as a;\n#[cfg(windows)]\n\
             pub mod a {\n    pub struct Thing;\n}\n#[cfg(windows)]\npub use a as b;",
        ),
        ("src/host/mod.rs", "pub mod left;\npub mod right;"),
        (
            "src/host/left.rs",
            "#[cfg(unix)]\nuse crate::a::Thing;\n#[cfg(windows)]\nuse crate::b::Thing;",
        ),
        ("src/host/right.rs", ""),
    ]))
    .unwrap_err();
    assert!(cut.starts_with("src/host/left.rs:2 goes through"), "{cut}");
    let cut = analyze(&crate_of(&[
        (
            "src/lib.rs",
            "pub mod host;\npub use crate::bridge as renamed;\npub mod bridge {\n    \
             pub use crate::renamed::inner::next as again;\n    pub mod inner {\n        \
             pub mod next {\n            pub use crate::host::b::B;\n        }\n    }\n}",
        ),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        ("src/host/a.rs", "fn f(_: crate::renamed::again::B) {}"),
        ("src/host/b.rs", "pub struct B;"),
    ]))
    .unwrap_err();
    assert!(cut.starts_with("src/host/a.rs:1 goes through"), "{cut}");
    // A module that imports itself names itself under the alias as often as a path goes through
    // it, through other aliases or globs too.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\npub mod bridge {\n    pub use crate::bridge as again;\n    \
                 pub use crate::host::b::B;\n}",
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "fn f(_: crate::bridge::again::again::B) {}"
            ),
            ("src/host/b.rs", "pub struct B;"),
        ]),
        ["a -> b"]
    );
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\npub use crate::bridge as renamed;\npub mod bridge {\n    \
                 pub use crate::renamed as again;\n    pub use crate::host::b::B;\n}",
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "fn f(_: crate::bridge::again::again::B) {}"
            ),
            ("src/host/b.rs", "pub struct B;"),
        ]),
        ["a -> b"]
    );
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\npub mod x {\n    pub use crate::y::*;\n}\npub mod y {\n    \
                 pub use crate::x as w;\n    pub use crate::host::b::B;\n}",
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            ("src/host/a.rs", "fn f(_: crate::x::w::w::B) {}"),
            ("src/host/b.rs", "pub struct B;"),
        ]),
        ["a -> b"]
    );
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\npub use crate::bridge as renamed;\npub mod bridge {\n    \
                 pub use crate::renamed::inner as again;\n    pub mod inner {\n        \
                 pub use crate::host::b::B;\n    }\n}",
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            ("src/host/a.rs", "fn f(_: crate::renamed::again::B) {}"),
            ("src/host/b.rs", "pub struct B;"),
        ]),
        ["a -> b"]
    );
}

#[test]
fn raw_identifiers_name_what_their_names_do() {
    let graph = analyze(&crate_of(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod r#b;"),
        ("src/host/a.rs", "fn f() {\n    super::r#b::r#type();\n}"),
        (
            "src/host/b.rs",
            "pub fn r#type() {}\nuse crate::host::a as r#a;",
        ),
    ]))
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(cycles(&graph.edges), [["a", "b"]]);
    let report = report(&graph, &cycles(&graph.edges));
    assert!(
        report.contains("    a -> b: src/host/a.rs:2: super::b::type"),
        "{report}"
    );
}

#[test]
fn modules_that_the_test_cannot_follow_fail_it() {
    let error = |files: &[(&str, &str)]| analyze(&crate_of(files)).unwrap_err();
    let placed = error(&[
        ("src/lib.rs", "pub mod host;"),
        (
            "src/host/mod.rs",
            "pub mod a;\n#[path = \"other.rs\"]\nmod b;",
        ),
        ("src/host/a.rs", ""),
        ("src/host/other.rs", ""),
    ]);
    assert!(placed.starts_with("src/host/mod.rs:2 "), "{placed}");
    // An identifier that is not ASCII, which Rust takes for any spelling that Unicode's NFC
    // normalization makes alike, as the metavariable `$\u{e9}` and `$e\u{301}` are one, and the
    // names of the extern prelude `caf\u{e9}` and `cafe\u{301}`.
    let spelled = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "mod a;\nmod b;"),
        (
            "src/host/a.rs",
            "macro_rules! wrap {\n    ($\u{e9}:tt, $item:item) => {\n        $e\u{301} inner { \
             $item }\n    };\n}\nwrap!(mod, struct X;);",
        ),
        ("src/host/b.rs", ""),
    ]);
    assert!(spelled.starts_with("src/host/a.rs:2 "), "{spelled}");
    let spelled = error(&[
        (
            "src/lib.rs",
            "extern crate self as caf\u{e9};\npub mod host;",
        ),
        ("src/host/mod.rs", "mod a;\nmod b;"),
        (
            "src/host/a.rs",
            "macro_rules! call {\n    ($p:path) => {\n        fn f() {\n            $p();\n        \
             }\n    };\n}\ncall!(cafe\u{301}::host::b::g);",
        ),
        ("src/host/b.rs", "pub fn g() {}"),
    ]);
    assert!(
        spelled.contains(" holds an identifier that is not ASCII"),
        "{spelled}"
    );
    // So does one in a path of a string, escaped or not, which an attribute may read, as serde
    // reads `K::host::b::serialize` in `"\u{212A}::host::b::serialize"`.
    for value in [
        "\\u{212A}::host::b::serialize",
        "\u{212A}::host::b::serialize",
    ] {
        let spelled = error(&[
            ("src/lib.rs", "extern crate self as K;\npub mod host;"),
            ("src/host/mod.rs", "mod a;\nmod b;"),
            (
                "src/host/a.rs",
                &format!("struct S {{\n    #[serde(serialize_with = \"{value}\")]\n    x: u8,\n}}"),
            ),
            ("src/host/b.rs", "pub fn serialize() {}"),
        ]);
        assert!(
            spelled.starts_with("src/host/a.rs:2 "),
            "{value}: {spelled}"
        );
    }
    // But a string that names no code, and documentation outside the input of a macro, which the
    // test reads for no path, may spell one.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "extern crate self as K;\npub mod host;"),
            ("src/host/mod.rs", "mod a;\nmod b;"),
            (
                "src/host/a.rs",
                "/// See \u{212A}::host::b::Thing.\nconst TEXT: &str = \"\\u{212A}::not_a_path\";",
            ),
            ("src/host/b.rs", ""),
        ]),
        Vec::<String>::new()
    );
    let undeclared = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        ("src/host/a.rs", ""),
        ("src/host/b.rs", ""),
    ]);
    assert!(undeclared.contains(" src/host/b.rs,"), "{undeclared}");
    let hidden = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        ("src/host/a.rs", "use super::*;"),
        ("src/host/b.rs", ""),
    ]);
    assert!(hidden.starts_with("src/host/a.rs:1 "), "{hidden}");
    let glob = error(&[
        ("src/lib.rs", "pub mod host;\npub use host::a::*;"),
        ("src/host/mod.rs", "pub mod a;"),
        ("src/host/a.rs", ""),
    ]);
    assert!(glob.starts_with("src/lib.rs:2 "), "{glob}");
    // An import of the crate root or of `host` itself, under any name, in a child, whose paths
    // through the name the test does not follow.
    let alias = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        ("src/host/a.rs", "extern crate self as this;"),
    ]);
    assert!(
        alias.starts_with("src/host/a.rs:1 imports the crate root itself"),
        "{alias}"
    );
    let alias = error(&[
        ("src/lib.rs", "pub mod host;\nuse host as aliased;"),
        ("src/host/mod.rs", "pub mod a;"),
        ("src/host/a.rs", "use crate::aliased as h;"),
    ]);
    assert!(
        alias.starts_with("src/host/a.rs:1 imports host itself"),
        "{alias}"
    );
    // A `#[path]` attribute anywhere in the crate, which could give any module the file of
    // another.
    let placed = error(&[
        (
            "src/lib.rs",
            "pub mod host;\n#[path = \"other.rs\"]\nmod protocol;",
        ),
        ("src/host/mod.rs", "pub mod a;"),
        ("src/host/a.rs", ""),
    ]);
    assert!(placed.starts_with("src/lib.rs:2 "), "{placed}");
    // Code of another file, which `include!` puts in a module, but not the data that
    // `include_str!` reads, nor what only tests include.
    let included = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "const TEXT: &str = include_str!(\"a.txt\");\n#[cfg(test)]\ninclude!(\"x.rs\");\n\
             std::include!(\"y.rs\");",
        ),
    ]);
    assert!(included.starts_with("src/host/a.rs:4 "), "{included}");
    // The same, through a name that an import gives `include!`, or from a macro's template, but
    // not where a `macro_rules!` of the same name hides it.
    let included = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "use std::include as inc;\ninc!(\"../included.rs\");",
        ),
    ]);
    assert!(included.starts_with("src/host/a.rs:2 "), "{included}");
    let included = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        ("src/host/a.rs", "pub use std::include as inc;"),
        (
            "src/host/b.rs",
            "use crate::host::a::inc as twice;\ntwice!(\"../included.rs\");",
        ),
    ]);
    assert!(included.starts_with("src/host/b.rs:2 "), "{included}");
    let included = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "macro_rules! wrapper {\n    () => { include!(\"../included.rs\"); };\n}",
        ),
    ]);
    assert!(included.starts_with("src/host/a.rs:2 "), "{included}");
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            ("src/host/mod.rs", "pub mod a;"),
            (
                "src/host/a.rs",
                "macro_rules! include {\n    ($name:literal) => {};\n}\ninclude!(\"a.rs\");"
            ),
        ]),
        Vec::<String>::new()
    );
    // An `include!` through a path may be the builtin whatever the crate defines, even where a
    // macro of the crate that the path may name has its name.
    let included = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "#[macro_export]\nmacro_rules! include {\n    ($name:literal) => {};\n}\n\
             crate::include!(\"a.rs\");",
        ),
    ]);
    assert!(included.starts_with("src/host/a.rs:5 "), "{included}");
    let included = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "pub use std::include;\nmod nested {\n    macro_rules! include {\n        () => {};\n    \
             }\n}\nself::include!(\"../included.rs\");",
        ),
    ]);
    assert!(included.starts_with("src/host/a.rs:7 "), "{included}");
    // A cast names no macro, though it has an `as`.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\ntype inc = u8;\nfn cast(include: u8) {\n    let _ = include as inc;\n}"
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "use crate::custom as inc;\ninc!(\"not-a-file.rs\");"
            ),
            (
                "src/host/b.rs",
                "#[macro_export]\nmacro_rules! custom {\n    ($name:literal) => {};\n}"
            ),
        ]),
        ["a -> b"]
    );
    // A `macro_rules!` of a block hides `include!` from the rest of the block, but no further.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            ("src/host/mod.rs", "pub mod a;"),
            (
                "src/host/a.rs",
                "fn f() {\n    macro_rules! include {\n        ($name:literal) => {};\n    }\n    \
                 include!(\"unused.rs\");\n}"
            ),
        ]),
        Vec::<String>::new()
    );
    let included = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "fn f() {\n    macro_rules! include {\n        ($name:literal) => {};\n    }\n}\n\
             include!(\"../included.rs\");",
        ),
    ]);
    assert!(included.starts_with("src/host/a.rs:6 "), "{included}");
    // A template that invokes a macro that its input names, which may be `include!`, and an inline
    // module in the input of a macro, which may put the module's code in another module.
    let invoked = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "macro_rules! read {\n    ($reader:ident) => { $reader!(\"../generated.rs\"); };\n}",
        ),
    ]);
    assert!(invoked.starts_with("src/host/a.rs:2 "), "{invoked}");
    let invoked = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "macro_rules! read {\n    ($($reader:ident)+) => { $($reader)+!(\"../generated.rs\"); \
             };\n}",
        ),
    ]);
    assert!(invoked.starts_with("src/host/a.rs:2 "), "{invoked}");
    // A template whose identifier may be a keyword that declares, and one that defines a macro
    // whose own token trees may join a path.
    let declared = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "macro_rules! wrap {\n    ($kind:ident, $name:ident, $item:item) => { $kind $name { \
             $item } };\n}",
        ),
    ]);
    assert!(declared.starts_with("src/host/a.rs:2 "), "{declared}");
    let declared = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "macro_rules! wrap {\n    ($vis:vis, $kind:ident, $name:ident, $item:item) => {\n        \
             $vis $kind $name { $item }\n    };\n}",
        ),
    ]);
    assert!(declared.starts_with("src/host/a.rs:3 "), "{declared}");
    let declared = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "macro_rules! wrap {\n    ($($kind:ident)*; $name:ident, $item:item) => { $($kind)* \
             $name { $item } };\n}",
        ),
    ]);
    assert!(declared.starts_with("src/host/a.rs:2 "), "{declared}");
    let declared = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "macro_rules! alias {\n    ($kind:ident) => { extern $kind self as this; };\n}",
        ),
    ]);
    assert!(declared.starts_with("src/host/a.rs:2 "), "{declared}");
    // An item that the input gives ends one, after which a keyword may start another, and so does
    // a repetition; an identifier may also be `use` before a group that an import may hold,
    // whatever it names, and before the token that follows it when a repetition that it ends
    // repeats.
    for template in [
        "($prefix:item, $kind:ident, $name:ident, $body:item) => { $prefix $kind $name { $body } };",
        "($prefix:item, $kind:ident) => { $prefix $kind crate as this; };",
        "($($prefix:item),*; $kind:ident, $name:ident, $body:item) => { $($prefix),* $kind $name \
         { $body } };",
        "($kind:ident) => { $kind {crate as this}; };",
        "($kind:ident) => { $kind { self as root }; };",
        "($kind:ident, $path:path) => { $kind { $path }; };",
        "($kind:ident) => { $kind { nvx }; };",
        "($($kind:ident),*) => { $($kind)* ; };",
        "($vis:vis $ignored:ident) => { extern $vis crate self as this; };",
        "($r#root:tt, $r#sep:tt, $r#name:tt) => { pub fn f(_: $root $sep $name) {} };",
        "($root:r#tt, $sep:r#tt, $name:r#tt) => { pub fn f(_: $root $sep $name) {} };",
        "($prefix:expr, $kind:ident, $name:ident, $body:item) => { $prefix $kind $name { $body } };",
    ] {
        let declared = error(&[
            ("src/lib.rs", "pub mod host;"),
            ("src/host/mod.rs", "pub mod a;"),
            (
                "src/host/a.rs",
                &format!("macro_rules! wrap {{\n    {template}\n}}"),
            ),
        ]);
        assert!(
            declared.starts_with("src/host/a.rs:2 "),
            "{template}: {declared}"
        );
    }
    // An attribute of a template, or of an input that declares a module in a file of its own,
    // that may set the module's file.
    let placed = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "macro_rules! place {\n    ($item:item) => { #[path = \"other.rs\"] $item };\n}",
        ),
    ]);
    assert!(placed.starts_with("src/host/a.rs:2 "), "{placed}");
    let placed = error(&[
        (
            "src/lib.rs",
            "pub mod host;\nmacro_rules! annotate {\n    ($attr:meta, $item:item) => { #[$attr] \
             $item };\n}\nannotate!(path = \"bridge_impl.rs\", pub mod bridge;);",
        ),
        ("src/bridge_impl.rs", "pub use crate::host::b::B;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        ("src/host/a.rs", "fn f(_: crate::bridge::B) {}"),
        ("src/host/b.rs", "pub struct B;"),
    ]);
    assert!(placed.starts_with("src/lib.rs:5 "), "{placed}");
    // A `macro_export` anywhere in a template or in the input of a macro, of which a template may
    // make an attribute that exports a macro that the test cannot tell the module of: one that a
    // template writes, in an attribute or for another macro to place, and one that an input
    // gives, wherever it is and whatever the template does with it, as when an empty visibility
    // or repetition stands between the attribute and the `macro_rules!`.
    let exported = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        (
            "src/host/a.rs",
            "macro_rules! outer {\n    () => {\n        #[macro_export]\n        \
             macro_rules! generated { () => {}; }\n    };\n}\nouter!();",
        ),
        ("src/host/b.rs", "crate::generated!();"),
    ]);
    assert!(exported.starts_with("src/host/a.rs:3 "), "{exported}");
    let exported = error(&[
        (
            "src/lib.rs",
            "#![allow(macro_expanded_macro_exports_accessed_by_absolute_paths)]\npub mod host;",
        ),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        (
            "src/host/a.rs",
            "macro_rules! annotate {\n    ($attr:meta, $item:item) => { #[$attr] $item };\n}\n\
             macro_rules! outer {\n    () => {\n        annotate!(macro_export, macro_rules! \
             generated { () => {}; });\n    };\n}\nouter!();",
        ),
        ("src/host/b.rs", "crate::generated!();"),
    ]);
    assert!(exported.starts_with("src/host/a.rs:6 "), "{exported}");
    for (template, input) in [
        (
            "($attr:meta, $item:item) => { #[$attr] $item };",
            "macro_export, macro_rules! helper { () => {}; }",
        ),
        (
            "($attr:meta) => { #[$attr] macro_rules! helper { () => {}; } };",
            "macro_export",
        ),
        (
            "(#[unwrap($attr:meta)] $item:item) => { #[$attr] $item };",
            "#[unwrap(macro_export)] macro_rules! helper { () => {}; }",
        ),
        (
            "($(#[$attr:meta])*, $item:item) => { $(#[$attr])* $item };",
            "#[macro_export], macro_rules! helper { () => {}; }",
        ),
        (
            "(use ignored::{$item:item}) => { $item };",
            "use ignored::{#[macro_export] macro_rules! helper { () => {}; }}",
        ),
        (
            "($attr:meta, $vis:vis) => { #[$attr] $vis macro_rules! helper { () => {}; } };",
            "macro_export,",
        ),
        (
            "($attr:meta $(, $prefix:literal)*) => { #[$attr] $($prefix)* macro_rules! helper { \
             () => {}; } };",
            "macro_export",
        ),
    ] {
        let exported = error(&[
            (
                "src/lib.rs",
                "#![allow(macro_expanded_macro_exports_accessed_by_absolute_paths)]\npub mod host;",
            ),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                &format!("macro_rules! forward {{\n    {template}\n}}\nforward!({input});"),
            ),
            ("src/host/b.rs", "crate::helper!();"),
        ]);
        assert!(
            exported.starts_with("src/host/a.rs:4 "),
            "{template}: {exported}"
        );
    }
    // An attribute that a template makes of an identifier of an input that declares a module in
    // a file of its own, whatever gives its value.
    for (template, input) in [
        (
            "($attr:ident, $value:literal, $item:item) => { #[$attr = $value] $item };",
            "path, \"bridge_impl.rs\", pub mod bridge;",
        ),
        (
            "($attr:ident, $item:item) => { #[$attr = \"bridge_impl.rs\"] $item };",
            "path, pub mod bridge;",
        ),
        (
            "($d:tt $attr:ident, $file:literal, $item:item) => { #[$attr = $file] $item };",
            "$path, \"bridge_impl.rs\", pub mod bridge;",
        ),
    ] {
        let placed = error(&[
            (
                "src/lib.rs",
                &format!(
                    "pub mod host;\nmacro_rules! annotate {{\n    {template}\n}}\n\
                     annotate!({input});"
                ),
            ),
            ("src/bridge_impl.rs", "pub use crate::host::b::B;"),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            ("src/host/a.rs", "fn f(_: crate::bridge::B) {}"),
            ("src/host/b.rs", "pub struct B;"),
        ]);
        assert!(placed.starts_with("src/lib.rs:5 "), "{template}: {placed}");
    }
    // A `path` that a template, or another macro's input, gives a macro for a module that the
    // input of a third declares, through as many macros as it takes.
    let placed = error(&[
        (
            "src/lib.rs",
            "pub mod host;\nmacro_rules! annotate {\n    ($attr:meta, $item:item) => { #[$attr] \
             $item };\n}\nmacro_rules! place {\n    ($item:item) => { annotate!(path = \
             \"bridge_impl.rs\", $item); };\n}\nplace!(pub mod bridge;);",
        ),
        ("src/bridge_impl.rs", "pub use crate::host::b::B;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        ("src/host/a.rs", "fn f(_: crate::bridge::B) {}"),
        ("src/host/b.rs", "pub struct B;"),
    ]);
    assert!(placed.starts_with("src/lib.rs:8 "), "{placed}");
    let placed = error(&[
        (
            "src/lib.rs",
            "pub mod host;\nmacro_rules! annotate {\n    ($attr:meta, $item:item) => { #[$attr] \
             $item };\n}\nmacro_rules! define {\n    ($attr:ident) => {\n        macro_rules! \
             place {\n            ($item:item) => { annotate!($attr = \"bridge_impl.rs\", $item); \
             };\n        }\n    };\n}\ndefine!(path);\nplace!(pub mod bridge;);",
        ),
        ("src/bridge_impl.rs", "pub use crate::host::b::B;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        ("src/host/a.rs", "fn f(_: crate::bridge::B) {}"),
        ("src/host/b.rs", "pub struct B;"),
    ]);
    assert!(placed.starts_with("src/lib.rs:13 "), "{placed}");
    // A metavariable that no matcher binds expands to itself, whose `$` a macro may drop.
    let placed = error(&[
        (
            "src/lib.rs",
            "pub mod host;\nmacro_rules! annotate {\n    ($d:tt $attr:ident, $file:literal, \
             $item:item) => { #[$attr = $file] $item };\n}\nmacro_rules! place {\n    \
             ($item:item) => { annotate!($path, \"bridge_impl.rs\", $item); };\n}\n\
             place!(pub mod bridge;);",
        ),
        ("src/bridge_impl.rs", "pub use crate::host::b::B;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        ("src/host/a.rs", "fn f(_: crate::bridge::B) {}"),
        ("src/host/b.rs", "pub struct B;"),
    ]);
    assert!(placed.starts_with("src/lib.rs:8 "), "{placed}");
    // But a matcher that a template defines matches its own input alone, and a metavariable that a
    // matcher binds names what the input gives.
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\nmacro_rules! identity {\n    ($item:item) => { $item };\n}\n\
                 identity!(mod bridge;);\nmacro_rules! outer {\n    () => {\n        \
                 macro_rules! inner {\n            (path) => {};\n            ($path:path) => { \
                 $path() };\n        }\n    };\n}",
            ),
            ("src/bridge.rs", ""),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            ("src/host/a.rs", ""),
            ("src/host/b.rs", ""),
        ]),
        Vec::<String>::new()
    );
    // A matcher of a macro that a template defines in the input of a macro that it invokes, which
    // may take the matcher for anything, as `call!` takes the path for a callee, and an identifier
    // of the input there, which may take it for the keyword of an item that it forms, as `forward!`
    // may form `mod inner { ... }`.
    let called = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        (
            "src/host/a.rs",
            "macro_rules! call {\n    (macro_rules! $name:ident { ($callee:path) => {} }) => {\n        \
             pub fn run() {\n            $callee();\n        }\n    };\n}\nmacro_rules! outer {\n    \
             () => {\n        call!(macro_rules! ignored { (crate::host::b::g) => {} });\n    };\n}\n\
             outer!();",
        ),
        (
            "src/host/b.rs",
            "pub fn g() {}\nfn h() {\n    super::a::run();\n}",
        ),
    ]);
    assert!(called.starts_with("src/host/a.rs:10 "), "{called}");
    let called = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        (
            "src/host/a.rs",
            "macro_rules! outer {\n    () => {\n        call!(macro_rules! ignored { \
             (\"crate::host::b::g\") => {} });\n    };\n}",
        ),
        ("src/host/b.rs", "pub fn g() {}"),
    ]);
    assert!(called.starts_with("src/host/a.rs:3 "), "{called}");
    // A path in the input of a macro that a template invokes, past a `::` before it, which the
    // macro may take apart from what precedes it.
    let called = error(&[
        ("src/lib.rs", "extern crate self as nvx;\npub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        (
            "src/host/a.rs",
            "macro_rules! call {\n    (ignored :: $callee:path) => {\n        pub fn run() {\n            \
             $callee();\n        }\n    };\n}\nmacro_rules! outer {\n    () => {\n        \
             call!(ignored::nvx::host::b::g);\n    };\n}\nouter!();",
        ),
        (
            "src/host/b.rs",
            "pub fn g() {}\nfn h() {\n    super::a::run();\n}",
        ),
    ]);
    assert!(called.starts_with("src/host/a.rs:10 "), "{called}");
    let declared = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        (
            "src/host/a.rs",
            "macro_rules! forward {\n    (@tag $item:item) => { $item };\n}\nmacro_rules! outer {\n    \
             ($kind:ident, $name:ident, $item:item) => { forward!(@tag $kind $name { $item }); };\n}\n\
             outer!(mod, inner, pub fn h() { super::super::b::g(); });",
        ),
        (
            "src/host/b.rs",
            "pub fn g() {}\nfn h() {\n    super::a::inner::h();\n}",
        ),
    ]);
    assert!(declared.starts_with("src/host/a.rs:5 "), "{declared}");
    // Documentation in the input of a macro that a template invokes, which the macro may take
    // for anything.
    let documented = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        (
            "src/host/a.rs",
            "macro_rules! outer {\n    () => {\n        \
             field!(serde, #[doc = \"crate::host::b::format\"]);\n    };\n}",
        ),
        ("src/host/b.rs", ""),
    ]);
    assert!(documented.starts_with("src/host/a.rs:3 "), "{documented}");
    let documented = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        (
            "src/host/a.rs",
            "macro_rules! outer {\n    () => {\n        \
             field!(serde, /**crate::host::b::format*/);\n    };\n}",
        ),
        ("src/host/b.rs", ""),
    ]);
    assert!(documented.starts_with("src/host/a.rs:3 "), "{documented}");
    // A name of the extern prelude before `as` in a template casts a variable.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "extern crate self as nvx;\npub mod host;"),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "macro_rules! cast {\n    () => {\n        fn f(nvx: u8) {\n            \
                 let _ = nvx as usize;\n        }\n    };\n}",
            ),
            ("src/host/b.rs", ""),
        ]),
        Vec::<String>::new()
    );
    // Struct expressions and a block that a template makes of an identifier of its input, which
    // no import holds, and documentation in blocks that `!` negates, which no macro takes.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            (
                "src/host/a.rs",
                "struct S {\n    x: u8,\n}\nmacro_rules! update {\n    ($name:ident, $value:expr) \
                 => {\n        impl $name {\n            fn reset(self) -> Self {\n                \
                 $name { x: $value }\n            }\n\n            fn updated(self) -> Self {\n                \
                 $name { ..self }\n            }\n        }\n    };\n}\nupdate!(S, 0);\n\
                 macro_rules! control {\n    ($kind:ident) => {\n        fn g() {\n            \
                 $kind { break; }\n        }\n\n        fn h() {\n            $kind { break }\n        \
                 }\n    };\n}\ncontrol!(loop);\n\
                 macro_rules! maybe {\n    ($($kind:ident)?) => {\n        fn m() {\n            \
                 $($kind)? { break }\n        }\n    };\n}\nmaybe!(loop);\n\
                 macro_rules! make {\n    ($name:ident, $($field:ident),*) => {\n        \
                 fn made() -> $name {\n            $name { $($field: 1),* }\n        }\n    };\n}\n\
                 make!(S, x);\n\
                 macro_rules! outer {\n    () => {\n        fn f() {\n            let _ = !{\n                \
                 #[doc = \"See crate::host::b\"]\n                struct T;\n                \
                 false\n            };\n            let _ = 1 + !{\n                \
                 #[doc = \"See crate::host::b\"]\n                struct U;\n                \
                 false\n            } as i32;\n        }\n    };\n}\nouter!();",
            ),
            ("src/host/b.rs", ""),
        ]),
        Vec::<String>::new()
    );
    // An alias of the crate root in the input of a macro in the crate root, which may join the
    // extern prelude.
    let aliased = error(&[
        (
            "src/lib.rs",
            "pub mod host;\nmacro_rules! pass {\n    ($item:item) => { $item };\n}\n\
             pass!(extern crate self as nvx;);",
        ),
        ("src/host/mod.rs", "pub mod a;"),
        ("src/host/a.rs", ""),
    ]);
    assert!(aliased.starts_with("src/lib.rs:5 "), "{aliased}");
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "macro_rules! identity {\n    ($item:item) => { $item };\n}\n\
                 identity!(extern crate self as _;);\npub mod host;",
            ),
            ("src/host/mod.rs", "pub mod a;"),
            ("src/host/a.rs", ""),
        ]),
        Vec::<String>::new()
    );
    let joined = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "macro_rules! outer {\n    () => {\n        macro_rules! inner {\n            \
             ($r:tt, $s:tt, $n:tt) => { pub fn f(_: $r $s $n) {} };\n        }\n    };\n}",
        ),
    ]);
    assert!(joined.starts_with("src/host/a.rs:4 "), "{joined}");
    // An `include!` in the input of a macro, wherever the parser's reading of the input puts it.
    let included = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "macro_rules! pass {\n    (use $ignored:ident :: { $body:item }) => { $body };\n}\n\
             pass!(use ignored::{include!(\"../generated.rs\");});",
        ),
    ]);
    assert!(included.starts_with("src/host/a.rs:4 "), "{included}");
    let stripped = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;"),
        (
            "src/host/a.rs",
            "macro_rules! strip {\n    (mod $name:ident { $item:item }) => { $item };\n}\n\
             strip! {\n    mod inner {\n        pub fn f() {\n            super::b::g();\n        \
             }\n    }\n}",
        ),
        ("src/host/b.rs", "pub fn g() {}"),
    ]);
    assert!(stripped.starts_with("src/host/a.rs:5 "), "{stripped}");
    // A template under `local_inner_macros`, whose macro invocations name macros of the crate
    // root, which the test cannot tell apart from those of where the macro expands, but not the
    // matcher of a macro that the template defines, which invokes nothing.
    let inner = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;"),
        (
            "src/host/a.rs",
            "#[macro_export(local_inner_macros)]\nmacro_rules! outer {\n    \
             () => { helper!(); };\n}",
        ),
    ]);
    assert!(inner.starts_with("src/host/a.rs:3 "), "{inner}");
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            ("src/host/mod.rs", "pub mod a;"),
            (
                "src/host/a.rs",
                "#[macro_export(local_inner_macros)]\nmacro_rules! outer {\n    () => {\n        \
                 macro_rules! nested {\n            (helper!()) => {};\n        }\n    };\n}"
            ),
        ]),
        Vec::<String>::new()
    );
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            ("src/host/mod.rs", "pub mod a;"),
            (
                "src/host/a.rs",
                "#[macro_export(local_inner_macros)]\nmacro_rules! outer {\n    () => {\n        \
                 macro_rules! middle {\n            () => {\n                \
                 macro_rules! nested {\n                    (helper!()) => {};\n                \
                 }\n            };\n        }\n    };\n}"
            ),
        ]),
        Vec::<String>::new()
    );
    // Nor does a string of such a matcher name anything, and the metavariables of the template
    // bind what its own matcher binds them to, whatever a nested matcher binds the same names to.
    // Documentation that a template writes names nothing either.
    for template in [
        "macro_rules! outer {\n    () => {\n        macro_rules! inner {\n            \
         (\"crate::host::b\") => {};\n        }\n    };\n}",
        "macro_rules! outer {\n    ($x:expr) => {\n        const _: () = { let _ = $x; };\n        \
         macro_rules! inner {\n            ($x:tt) => { $x };\n        }\n    };\n}\nouter!(1);",
        "macro_rules! documented {\n    () => {\n        #[doc = \"See crate::host::b\"]\n        \
         struct S;\n    };\n}",
    ] {
        assert_eq!(
            dependencies(&[
                ("src/lib.rs", "pub mod host;"),
                ("src/host/mod.rs", "pub mod a;\npub mod b;"),
                ("src/host/a.rs", template),
                ("src/host/b.rs", ""),
            ]),
            Vec::<String>::new(),
            "{template}"
        );
    }
    let scoped = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "#[macro_use]\nmod a;\nmod b;"),
        ("src/host/a.rs", ""),
        ("src/host/b.rs", ""),
    ]);
    assert!(scoped.starts_with("src/host/mod.rs:1 "), "{scoped}");
    let maybe_scoped = error(&[
        ("src/lib.rs", "pub mod host;"),
        (
            "src/host/mod.rs",
            "#[cfg_attr(unix, macro_use)]\nmod a;\nmod b;",
        ),
        ("src/host/a.rs", ""),
        ("src/host/b.rs", ""),
    ]);
    assert!(
        maybe_scoped.starts_with("src/host/mod.rs:1 "),
        "{maybe_scoped}"
    );
    // The same, in a `host` that the crate root holds inline.
    let inline = error(&[(
        "src/lib.rs",
        "pub mod host {\n    #[macro_use]\n    mod a {}\n    mod b {}\n}",
    )]);
    assert!(inline.starts_with("src/lib.rs:2 "), "{inline}");
    let inline = error(&[(
        "src/lib.rs",
        "pub mod host {\n    pub mod a {}\n    #[path = \"other.rs\"]\n    pub mod b;\n}",
    )]);
    assert!(inline.starts_with("src/lib.rs:3 "), "{inline}");
    // The same, with raw identifiers.
    let raw = error(&[
        ("src/lib.rs", "pub mod host;"),
        (
            "src/host/mod.rs",
            "pub mod a;\n#[r#cfg_attr(unix, r#path = \"other.rs\")]\nmod b;",
        ),
        ("src/host/a.rs", ""),
        ("src/host/other.rs", ""),
    ]);
    assert!(raw.starts_with("src/host/mod.rs:2 "), "{raw}");
    let raw = error(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "#[r#macro_use]\nmod a;\nmod b;"),
        ("src/host/a.rs", ""),
        ("src/host/b.rs", ""),
    ]);
    assert!(raw.starts_with("src/host/mod.rs:1 "), "{raw}");
}

#[test]
fn each_dependency_on_a_cycle_lists_the_shortest_cycle_through_it() {
    let graph = |pairs: &[(&str, &str)]| {
        let edges: BTreeMap<(String, String), Example> = pairs
            .iter()
            .map(|&(from, to)| {
                let example = Example {
                    location: String::new(),
                    path: String::new(),
                };
                ((from.to_owned(), to.to_owned()), example)
            })
            .collect();
        cycles(&edges)
    };
    assert_eq!(
        graph(&[
            ("a", "b"),
            ("b", "c"),
            ("c", "a"),
            ("c", "d"),
            ("d", "e"),
            ("e", "d"),
            ("f", "a"),
            ("e", "g"),
        ]),
        [vec!["d", "e"], vec!["a", "b", "c"]]
    );
    assert_eq!(
        graph(&[("a", "b"), ("b", "a"), ("b", "c"), ("c", "a")]),
        [vec!["a", "b"], vec!["a", "b", "c"]]
    );
    assert!(graph(&[("a", "b"), ("b", "c"), ("a", "c")]).is_empty());
}

#[test]
fn only_production_paths_into_other_children_are_references() {
    let edges = dependencies(&[
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
        edges,
        [
            "a -> c", "a -> d", "b -> c", "b -> d", "b -> e", "c -> e", "d -> e"
        ]
    );
}

#[test]
fn test_only_code_ends_where_its_construct_does() {
    // The test-only code of `a` names `t`, and the production code after each construct names
    // another child.
    let edges = dependencies(&[
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
        edges,
        [
            "a -> b", "a -> c", "a -> d", "a -> e", "a -> f", "a -> g", "a -> h", "a -> i"
        ]
    );
}

#[test]
fn paths_in_attributes_are_references() {
    // The test-only code of `a` names `t`.
    let edges = dependencies(&[
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
    assert_eq!(edges, ["a -> b", "a -> c", "a -> d", "a -> e", "a -> f"]);
}

#[test]
fn the_paths_in_the_input_of_a_macro_are_references() {
    // The input of a macro need not be Rust, may hold paths in strings that the macro puts in
    // attributes, and may lose its `cfg` attributes, while a template that names no path from a
    // module names only what is in scope where it expands, whatever its matcher matches.
    let edges = dependencies(&[
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
    assert_eq!(edges, ["a -> b", "a -> c", "a -> d", "a -> e"]);
    // The macro need not take a string after `doc =` for documentation, or a `use` tree for an
    // import.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            ("src/host/mod.rs", "mod a;\nmod b;"),
            (
                "src/host/a.rs",
                "macro_rules! field {\n    ($attr:ident, doc = $path:literal) => {\n        \
                 struct S {\n            #[$attr(with = $path)]\n            value: u8,\n        \
                 }\n    };\n}\nfield!(serde, doc = \"crate::host::b::format\");",
            ),
            ("src/host/b.rs", ""),
        ]),
        ["a -> b"]
    );
    // Nor a doc comment for documentation, of which Rust makes a `#[doc = "..."]` attribute before
    // the macro matches its input.
    for comment in ["/**crate::host::b::format*/", "///crate::host::b::format\n"] {
        assert_eq!(
            dependencies(&[
                ("src/lib.rs", "pub mod host;"),
                ("src/host/mod.rs", "mod a;\nmod b;"),
                (
                    "src/host/a.rs",
                    &format!(
                        "macro_rules! field {{\n    ($attr:ident, #[doc = $path:literal]) => {{\n        \
                         struct S {{\n            #[$attr(with = $path)]\n            value: u8,\n        \
                         }}\n    }};\n}}\nfield!(serde, {comment});"
                    ),
                ),
                ("src/host/b.rs", ""),
            ]),
            ["a -> b"],
            "{comment}"
        );
    }
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "pub mod host;"),
            ("src/host/mod.rs", "mod a;\nmod b;"),
            (
                "src/host/a.rs",
                "macro_rules! call {\n    (use $unused:ident :: { $callee:path }) => {\n        \
                 pub fn run() {\n            $callee();\n        }\n    };\n}\n\
                 call!(use ignored::{crate::host::b::g});",
            ),
            ("src/host/b.rs", "pub fn g() {}"),
        ]),
        ["a -> b"]
    );
    // Nor a path for one that follows a `::`, or a module for an import.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "extern crate self as nvx;\npub mod host;"),
            ("src/host/mod.rs", "mod a;\nmod b;"),
            (
                "src/host/a.rs",
                "macro_rules! call {\n    (ignored :: $callee:path) => {\n        \
                 pub fn f() {\n            $callee();\n        }\n    };\n}\n\
                 call!(ignored::nvx::host::b::g);",
            ),
            ("src/host/b.rs", "pub fn g() {}"),
        ]),
        ["a -> b"]
    );
    assert_eq!(
        dependencies(&[
            (
                "src/lib.rs",
                "pub mod host;\nmacro_rules! pass {\n    (use $ignored:ident :: { $body:item }) => \
                 { $body };\n}\npass!(use ignored::{pub mod bridge;});",
            ),
            ("src/bridge.rs", "pub use crate::host::b::B;"),
            ("src/host/mod.rs", "pub mod a;\npub mod b;"),
            ("src/host/a.rs", "fn f(_: crate::bridge::B) {}"),
            ("src/host/b.rs", "pub struct B;"),
        ]),
        ["a -> b"]
    );
    // A module that an input declares counts once, so its own names hide those of the extern
    // prelude.
    assert_eq!(
        dependencies(&[
            ("src/lib.rs", "extern crate self as nvx;\npub mod host;"),
            ("src/host/mod.rs", "mod a;\nmod b;"),
            (
                "src/host/a.rs",
                "macro_rules! identity {\n    ($item:item) => { $item };\n}\nidentity!(mod inner;);",
            ),
            (
                "src/host/a/inner.rs",
                "pub mod nvx {\n    pub mod host {\n        pub mod b {\n            \
                 pub fn f() {}\n        }\n    }\n}\nfn f() {\n    nvx::host::b::f();\n}",
            ),
            ("src/host/b.rs", ""),
        ]),
        Vec::<String>::new()
    );
}

#[test]
fn paths_through_the_imports_of_the_crate_root_and_host_reach_every_child_they_may_name() {
    let edges = dependencies(&[
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
        edges,
        [
            "d -> a", "d -> b", "e -> c", "h -> b", "h -> g", "i -> c", "j -> a", "k -> b",
            "l -> c", "l -> e", "m -> a", "m -> b", "m -> e", "n -> c"
        ]
    );
}

#[test]
fn paths_through_aliases_and_the_crate_roots_other_modules_are_references() {
    let edges = dependencies(&[
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
        edges,
        ["f -> a", "g -> b", "h -> c", "i -> d", "j -> e", "l -> c"]
    );
    // A module that imports host's children with a glob stands for every one of them.
    let edges = dependencies(&[
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
    assert_eq!(edges, ["a -> b", "a -> c"]);
}

#[test]
fn paths_from_the_names_of_the_crate_root_in_the_extern_prelude_are_references() {
    // `a` names `nvx` before the crate root declares it, `host` may import any name with its
    // glob, `l` and `p` declare their own `nvx`, which `::` or some configurations bypass, `r`,
    // `s`, and `t` declare or import their own in every configuration, `u` only in a block, `w`
    // and `y` in test builds alone, and `alpha` in one of two definitions of a module.
    let edges = dependencies(&[
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
        edges,
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
    let edges = dependencies(&[
        ("src/lib.rs", "pub mod host;\n"),
        ("src/host/mod.rs", "mod a;\nmod b;\n"),
        ("src/host/a.rs", path.as_str()),
        ("src/host/b.rs", modules.as_str()),
    ]);
    assert_eq!(edges, ["a -> b"]);
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
        let error = analyze(&crate_of(files)).unwrap_err();
        let location = expected.split(": ").next().unwrap_or_default();
        assert!(
            error.starts_with(&format!("{location} ")),
            "{error:?} does not start with {location:?}"
        );
    }
}

#[test]
fn each_cycle_is_listed_with_one_reference_per_edge() {
    let graph = analyze(&crate_of(&[
        ("src/lib.rs", "pub mod host;\nextern crate self as nvx;\n"),
        ("src/host/mod.rs", "mod a;\nmod b;\nmod c;\n"),
        ("src/host/a.rs", "use super::b::{B, Other};\n"),
        (
            "src/host/b.rs",
            "use super::c::C;\nfn f() {\n    crate::host::a::f();\n}\n",
        ),
        ("src/host/c.rs", "use ::nvx::host::a::A;\n"),
    ]))
    .unwrap_or_else(|error| panic!("{error}"));
    let cycles = cycles(&graph.edges);
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
    assert!(
        report.contains("re-exporting it at its old public path"),
        "{report}"
    );
    // A reference names the line of its path's last segment, and no dependency off a cycle is
    // listed.
    let graph = analyze(&crate_of(&[
        ("src/lib.rs", "pub mod host;"),
        ("src/host/mod.rs", "pub mod a;\npub mod b;\npub mod c;"),
        (
            "src/host/a.rs",
            "use crate::host::b::{\n    One,\n    Two,\n};\nuse crate::host::c::Three;",
        ),
        ("src/host/b.rs", "fn f() {\n    super::a::four();\n}"),
        ("src/host/c.rs", ""),
    ]))
    .unwrap_or_else(|error| panic!("{error}"));
    let text = self::report(&graph, &self::cycles(&graph.edges));
    assert!(
        text.contains(
            "\na -> b -> a\n    a -> b: src/host/a.rs:2: crate::host::b::One\n    \
             b -> a: src/host/b.rs:2: super::a::four\n"
        ),
        "{text}"
    );
    assert!(!text.contains("a -> c"), "{text}");
}
