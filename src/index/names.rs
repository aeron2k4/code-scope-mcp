//! Compact symbol names of one file, for `search_symbols`.
//!
//! A workspace-wide search only needs each symbol's name, parent, kind and
//! line to score and rank it. Keeping just those, about 40 bytes a symbol
//! instead of the whole parsed file with its source, lets the names of a
//! repository as large as the Linux kernel stay in memory, so a warm search
//! costs a `stat` per file and no re-read.
//!
//! The file's [`Mentions`] come along, for `codebase_map` and to let
//! `find_references` skip files without the name.

use memchr::memmem::Finder;

use crate::ast::parser::{Lang, ParsedFile, Symbol, SymbolKind, is_ident_char};
use crate::index::mentions::{self, Mentions};

/// No parent.
const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
struct NameRef {
    name: u32,
    name_len: u32,
    parent: u32,
    parent_len: u32,
    start_line: u32,
    kind: SymbolKind,
    reach: Reach,
}

/// How far the name of a definition reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// As far as the language lets any definition reach.
    Public,
    /// Not as far: see [`is_private`].
    Private,
    /// Not out of the function or method it is in.
    Local,
}

/// Names of a file's symbols, in the order of `ParsedFile::symbols`.
#[derive(Debug, Default)]
pub struct SymbolNames {
    text: String,
    syms: Vec<NameRef>,
    /// (symbol, hash of the trait's name) for each item of a Rust
    /// `impl Trait for Type`, by symbol; empty in most files.
    traits: Box<[(u32, u32)]>,
    /// The hash of the name in a Go file's `package` clause.
    package: Option<u32>,
    /// Whether a JavaScript or TypeScript file is a module, not a script.
    module: bool,
    /// The [`mentions::package_hash`] of the package a TypeScript script
    /// declares, `declare module 'vscode' { .. }`.
    ambient: Option<u32>,
    incomplete: bool,
    mentions: Mentions,
    lang: Option<Lang>,
}

impl SymbolNames {
    pub fn new(parsed: &ParsedFile, source: &str) -> Self {
        let mut names = Self::of(&parsed.symbols, Some(parsed.lang), source);
        names.incomplete = parsed.incomplete;
        names.mentions = Mentions::of(parsed, source);
        if parsed.lang == Lang::Go {
            names.package = go_package(source).map(mentions::hash);
        }
        names
    }

    /// The names of `symbols`; `source` is their file's text, which only
    /// JavaScript and TypeScript need, to tell what a module exports.
    fn of(symbols: &[Symbol], lang: Option<Lang>, source: &str) -> Self {
        let mut text = String::new();
        let mut push = |s: &str| {
            let at = u32::try_from(text.len()).unwrap_or(NONE);
            text.push_str(s);
            (at, u32::try_from(s.len()).unwrap_or(0))
        };
        let exports = match lang {
            Some(Lang::JavaScript | Lang::TypeScript | Lang::Tsx) => JsExports::of(source),
            _ => None,
        };
        // The symbols enclosing the current one, innermost last.
        let mut open: Vec<&Symbol> = Vec::new();
        let mut traits = Vec::new();
        let syms = symbols
            .iter()
            .enumerate()
            .map(|(i, s)| {
                while open.last().is_some_and(|o| o.depth >= s.depth) {
                    open.pop();
                }
                let enclosing = open.last().copied();
                let reach = if open.iter().any(|o| o.kind.is_callable()) {
                    Reach::Local
                } else if lang.is_some_and(|lang| is_private(lang, s, enclosing))
                    || (enclosing.is_none()
                        && exports.as_ref().is_some_and(|e| !e.exports(source, s)))
                {
                    Reach::Private
                } else {
                    Reach::Public
                };
                if lang == Some(Lang::Rust)
                    && let Some(e) = enclosing.filter(|e| e.kind == SymbolKind::Impl)
                    && let Some(token) = impl_trait(&e.signature).and_then(last_ident)
                {
                    traits.push((u32::try_from(i).unwrap_or(NONE), mentions::hash(token)));
                }
                open.push(s);
                let (name, name_len) = push(&s.name);
                let receiver = match (lang, s.kind) {
                    (Some(Lang::Go), SymbolKind::Method) if s.parent.is_none() => {
                        go_receiver(&s.signature)
                    }
                    _ => None,
                };
                let (parent, parent_len) = s
                    .parent
                    .as_deref()
                    .or(receiver)
                    .map_or((NONE, 0), &mut push);
                NameRef {
                    name,
                    name_len,
                    parent,
                    parent_len,
                    start_line: u32::try_from(s.start_line).unwrap_or(u32::MAX),
                    kind: s.kind,
                    reach,
                }
            })
            .collect();
        text.shrink_to_fit();
        SymbolNames {
            text,
            syms,
            traits: traits.into_boxed_slice(),
            package: None,
            module: exports.is_some(),
            ambient: match lang {
                Some(Lang::TypeScript | Lang::Tsx) if exports.is_none() => {
                    ambient_module(source).map(mentions::package_hash)
                }
                _ => None,
            },
            incomplete: false,
            mentions: Mentions::default(),
            lang,
        }
    }

    /// How far the name of the symbol at `index` reaches.
    pub fn reach(&self, index: usize) -> Reach {
        self.syms.get(index).map_or(Reach::Public, |s| s.reach)
    }

    /// The hash of the package name of a Go file.
    pub fn package(&self) -> Option<u32> {
        self.package
    }

    /// Whether a JavaScript or TypeScript file is a module, whose
    /// definitions others import, not a script, whose are global.
    pub fn is_module(&self) -> bool {
        self.module
    }

    /// The [`mentions::package_hash`] of the package a TypeScript script
    /// declares with `declare module 'name'` at the start of a line, whose
    /// declarations are for the files importing it.
    pub fn ambient(&self) -> Option<u32> {
        self.ambient
    }

    /// For an item of a Rust `impl Trait for Type`, the hash of the
    /// trait's name, `Display` for `impl fmt::Display for Foo`.
    pub fn trait_of(&self, index: usize) -> Option<u32> {
        let index = u32::try_from(index).ok()?;
        let at = self.traits.binary_search_by_key(&index, |&(s, _)| s).ok()?;
        Some(self.traits[at].1)
    }

    /// The identifier tokens of the file.
    pub fn mentions(&self) -> &Mentions {
        &self.mentions
    }

    /// The file's language, when built from a parse.
    pub fn lang(&self) -> Option<Lang> {
        self.lang
    }

    /// Parent name of the symbol at `index`, if it has one.
    pub fn parent(&self, index: usize) -> Option<&str> {
        let s = self.syms.get(index)?;
        (s.parent != NONE).then(|| self.slice(s.parent, s.parent_len))
    }

    /// Kind and start line of the symbol at `index`.
    pub fn kind_and_line(&self, index: usize) -> Option<(SymbolKind, usize)> {
        let s = self.syms.get(index)?;
        Some((s.kind, s.start_line as usize))
    }

    /// Whether the file's symbol query ran out of time; see
    /// `ParsedFile::incomplete`.
    pub fn incomplete(&self) -> bool {
        self.incomplete
    }

    /// Name of the symbol at `index`, to check a hit against a re-read file.
    pub fn name(&self, index: usize) -> Option<&str> {
        let s = self.syms.get(index)?;
        Some(self.slice(s.name, s.name_len))
    }

    /// Approximate heap size, for the memory budget.
    pub fn weight(&self) -> usize {
        const OVERHEAD: usize = 64;
        OVERHEAD
            + self.text.len()
            + self.syms.len() * size_of::<NameRef>()
            + self.traits.len() * size_of::<(u32, u32)>()
            + self.mentions.weight()
    }

    pub fn len(&self) -> usize {
        self.syms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.syms.is_empty()
    }

    fn slice(&self, at: u32, len: u32) -> &str {
        let at = at as usize;
        self.text.get(at..at + len as usize).unwrap_or_default()
    }

    /// Symbols matching `query`, as (index, score, start line); see
    /// [`Query::score`].
    pub fn matches<'a>(
        &'a self,
        query: &'a Query,
        kind: Option<SymbolKind>,
    ) -> impl Iterator<Item = (usize, u8, usize)> + 'a {
        self.syms
            .iter()
            .enumerate()
            .filter(move |(_, s)| kind.is_none_or(|k| k == s.kind))
            .filter_map(move |(i, s)| {
                let name = self.slice(s.name, s.name_len);
                let parent = (s.parent != NONE).then(|| self.slice(s.parent, s.parent_len));
                query
                    .score(name, parent)
                    .map(|score| (i, score, s.start_line as usize))
            })
    }
}

/// Longest name lower-cased on the stack; longer ones allocate.
const STACK_NAME: usize = 128;

/// A search query, lower-cased once, with a searcher for it.
#[derive(Debug)]
pub struct Query {
    raw: String,
    lower: String,
    finder: Finder<'static>,
}

impl Query {
    pub fn new(query: &str) -> Self {
        let lower = query.to_lowercase();
        Query {
            raw: query.to_string(),
            finder: Finder::new(lower.as_bytes()).into_owned(),
            lower,
        }
    }

    /// Lower is better; `None` means no match. 0: the name or qualified name
    /// is the query; 1: the name is, ignoring case; 2: it starts with it; 3: it
    /// contains it; 4: the qualified name contains it.
    pub fn score(&self, name: &str, parent: Option<&str>) -> Option<u8> {
        if name == self.raw || parent.is_some_and(|p| is_qualified(p, name, &self.raw)) {
            return Some(0);
        }
        match self.find_lower(name) {
            Some((0, len)) if len == self.lower.len() => return Some(1),
            Some((0, _)) => return Some(2),
            Some(_) => return Some(3),
            None => {}
        }
        let parent = parent?;
        // Without a ':' the query cannot span the `::` between parent and
        // name, and the name alone did not match.
        let found = if self.lower.contains(':') {
            self.find_lower(&format!("{parent}::{name}")).is_some()
        } else {
            self.find_lower(parent).is_some()
        };
        found.then_some(4)
    }

    /// Where the query is in `hay` lower-cased, and that lower-cased length.
    fn find_lower(&self, hay: &str) -> Option<(usize, usize)> {
        let bytes = hay.as_bytes();
        if hay.is_ascii() && bytes.len() <= STACK_NAME {
            let mut buf = [0u8; STACK_NAME];
            let lower = &mut buf[..bytes.len()];
            lower.copy_from_slice(bytes);
            lower.make_ascii_lowercase();
            return self.finder.find(lower).map(|at| (at, bytes.len()));
        }
        let lower = hay.to_lowercase();
        self.finder
            .find(lower.as_bytes())
            .map(|at| (at, lower.len()))
    }
}

/// Whether `query` is `parent::name`.
fn is_qualified(parent: &str, name: &str, query: &str) -> bool {
    query.len() == parent.len() + 2 + name.len()
        && query.starts_with(parent)
        && query[parent.len()..].starts_with("::")
        && query.ends_with(name)
}

/// The trait of a Rust `impl` block's signature, `ops::BitOrAssign<Ready>`
/// for `impl ops::BitOrAssign<Ready> for Ready`, or `None` for an inherent
/// `impl Type`.
fn impl_trait(signature: &str) -> Option<&str> {
    let mut rest = signature;
    loop {
        let at = rest.find("impl")?;
        let before_ok = rest[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident_char(c));
        rest = &rest[at + 4..];
        if before_ok && rest.chars().next().is_none_or(|c| !is_ident_char(c)) {
            break;
        }
    }
    let rest = rest.trim_start();
    // Skip the generics of the impl, `impl<T: for<'a> Fn(&'a T)>`.
    let rest = if rest.starts_with('<') {
        &rest[balanced_end(rest)?..]
    } else {
        rest
    };
    let mut depth = 0i32;
    let mut prev = ' ';
    for (i, c) in rest.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' if prev != '-' => depth -= 1,
            ')' | ']' => depth -= 1,
            ' ' if depth == 0 && rest[i..].starts_with(" for ") => {
                return Some(rest[..i].trim().trim_start_matches('!'));
            }
            _ => {}
        }
        prev = c;
    }
    None
}

/// The byte just past the `>` closing the `<` that `s` starts with.
fn balanced_end(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut prev = ' ';
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' if prev != '-' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        prev = c;
    }
    None
}

/// The last identifier of a path such as `ops::BitOrAssign<Ready>`, before
/// its generic arguments: `BitOrAssign`.
fn last_ident(path: &str) -> Option<&str> {
    let end = path.find(['<', '(', '[']).unwrap_or(path.len());
    path[..end]
        .rsplit(|c: char| !is_ident_char(c))
        .find(|s| !s.is_empty())
}

/// The name in the `package` clause of Go `source`.
fn go_package(source: &str) -> Option<&str> {
    source.lines().find_map(|line| {
        let name = line.strip_prefix("package")?;
        if !name.starts_with([' ', '\t']) {
            return None;
        }
        let name = name.trim_start();
        let end = name.find(|c: char| !is_ident_char(c)).unwrap_or(name.len());
        Some(&name[..end]).filter(|n| !n.is_empty())
    })
}

/// The type of the receiver of a Go method from its `signature`, `T` in
/// `func (t *T[K]) name()`.
fn go_receiver(signature: &str) -> Option<&str> {
    let rest = signature
        .strip_prefix("func")?
        .trim_start()
        .strip_prefix('(')?;
    let receiver = rest[..rest.find(')')?].trim();
    // A receiver with no name, `(Pair[K, V])`, starts with its type.
    let ty = match receiver.split_once(char::is_whitespace) {
        Some((name, ty)) if !name.contains(['[', '*']) => ty.trim_start(),
        _ => receiver,
    }
    .trim_start_matches('*');
    let ty = ty.split('[').next().unwrap_or(ty);
    mentions::is_token(ty).then_some(ty)
}

/// The name of the package in the first `declare module 'name'` at the
/// start of a line of `source`.
fn ambient_module(source: &str) -> Option<&str> {
    source.lines().find_map(|line| {
        let rest = line.strip_prefix("declare module")?.trim_start();
        let quote = rest.chars().next().filter(|&c| c == '\'' || c == '"')?;
        let body = &rest[1..];
        body.find(quote).map(|end| &body[..end])
    })
}

/// Whether `source` is a CommonJS module: it calls `require(...)` or sets
/// `module.exports` or `exports.name`. Its top-level declarations, like an
/// ES module's, are its own unless exported.
fn is_commonjs(source: &str) -> bool {
    let word_at = |at: usize| {
        source[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident_char(c) && c != '.')
    };
    let calls = |key: &str| source.match_indices(key).any(|(at, _)| word_at(at));
    calls("require(") || calls("module.exports") || calls("exports.")
}

/// What a JavaScript or TypeScript module exports. A file with an `import`
/// or `export` at the start of a line, or a CommonJS one, is a module, whose top-level
/// declarations stay in it unless exported; a script's are global.
struct JsExports<'a> {
    /// The words in `export { a, b as c }` lists and after `export default`.
    listed: Vec<&'a str>,
}

impl<'a> JsExports<'a> {
    /// The exports of `source`, or `None` for a script.
    fn of(source: &'a str) -> Option<Self> {
        let starts = |line: &str, word: &str| {
            line.strip_prefix(word)
                .is_some_and(|rest| rest.starts_with([' ', '{', '*', '\t', '"', '\'']))
        };
        let commonjs = is_commonjs(source);
        if !commonjs
            && !source
                .lines()
                .any(|l| starts(l, "import") || starts(l, "export"))
        {
            return None;
        }
        let mut listed = Vec::new();
        let words = |s: &'a str| {
            s.split(|c: char| !is_ident_char(c))
                .filter(|w| !w.is_empty())
        };
        let mut rest = source;
        while let Some(at) = rest.find("export") {
            let after = rest[at + 6..].trim_start();
            if let Some(list) = after.strip_prefix('{') {
                let end = list.find('}').unwrap_or(list.len());
                listed.extend(words(&list[..end]));
            } else if let Some(name) = after.strip_prefix("default") {
                listed.extend(words(name).next());
            } else if let Some(name) = after.strip_prefix('=') {
                listed.extend(words(name).next());
            }
            rest = &rest[at + 6..];
        }
        if commonjs {
            // `module.exports = { a, b }`, `module.exports = a`,
            // `exports.a = ...` and `module.exports.a = ...`.
            let mut rest = source;
            while let Some(at) = rest.find("exports") {
                let after = rest[at + 7..].trim_start();
                if let Some(value) = after.strip_prefix('=').map(str::trim_start) {
                    if let Some(list) = value.strip_prefix('{') {
                        let end = list.find('}').unwrap_or(list.len());
                        listed.extend(words(&list[..end]));
                    } else {
                        listed.extend(words(value).next());
                    }
                } else if let Some(member) = after.strip_prefix('.') {
                    listed.extend(words(member).next());
                }
                rest = &rest[at + 7..];
            }
        }
        listed.sort_unstable();
        listed.dedup();
        Some(JsExports { listed })
    }

    /// Whether the module exports the top-level `sym`: `export` on its line
    /// before its name (the symbol may start at the `export`), or its name
    /// in a list.
    fn exports(&self, source: &str, sym: &Symbol) -> bool {
        let start = sym.start_byte.min(source.len());
        let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
        let line_end = source[start..]
            .find('\n')
            .map_or(source.len(), |i| start + i);
        // The name as a whole word: `e` is not the one in `export const e`.
        let name = sym.name.as_str();
        let name_at = source
            .get(start..line_end)
            .and_then(|line| {
                line.match_indices(name).map(|(i, _)| i).find(|&i| {
                    !line[..i].ends_with(is_ident_char)
                        && !line[i + name.len()..].starts_with(is_ident_char)
                })
            })
            .map_or(start, |i| start + i);
        source.get(line_start..name_at).is_some_and(|before| {
            before
                .split(|c: char| !is_ident_char(c))
                .any(|w| w == "export")
        }) || self.listed.binary_search(&sym.name.as_str()).is_ok()
    }
}

/// Whether `lang` keeps `sym` out of reach of most other files, nested in
/// `enclosing`:
/// - Rust: an item without `pub` (or with `pub(self)`), seen only in its
///   module and the modules under it. Macros, and the items of a trait or
///   of a trait's `impl`, which have the trait's reach, are not.
/// - Go: a name starting in lower case, seen only in its package.
/// - Python and GDScript: a `_name` but not a `__dunder__`, private to its
///   module by convention.
/// - C and C++: a `static` function or variable outside any type, and any
///   macro or type outside one, seen only in its file (or the files
///   including it, for a header).
/// - Java and C#: a `private` member, seen only in its class; in C# also a
///   member of a class or struct with no access modifier.
/// - JavaScript, TypeScript and Ruby: nothing.
fn is_private(lang: Lang, sym: &Symbol, enclosing: Option<&Symbol>) -> bool {
    use SymbolKind::*;
    if matches!(sym.kind, Impl | Module) {
        return false;
    }
    // The words of the signature before the name: modifiers and types.
    let modifiers = || {
        sym.signature
            .split(|c: char| !is_ident_char(c))
            .filter(|w| !w.is_empty())
            .take_while(|&w| w != sym.name)
    };
    match lang {
        Lang::Rust => {
            let in_trait = enclosing.is_some_and(|e| {
                e.kind == Trait || (e.kind == Impl && impl_trait(&e.signature).is_some())
            });
            let sig = sym.signature.as_str();
            let public = sig
                .strip_prefix("pub")
                .is_some_and(|rest| !rest.starts_with(is_ident_char))
                && !sig.starts_with("pub(self)");
            sym.kind != Macro && !in_trait && !public
        }
        Lang::Go => sym.name.starts_with(|c: char| c.is_lowercase() || c == '_'),
        Lang::Python | Lang::GdScript => {
            sym.name.starts_with('_') && !(sym.name.starts_with("__") && sym.name.ends_with("__"))
        }
        Lang::C | Lang::Cpp => {
            enclosing.is_none_or(|e| e.kind == Module)
                && (matches!(sym.kind, Macro | Struct | Class | Enum | Type)
                    || (matches!(sym.kind, Function | Variable | Constant)
                        && modifiers().any(|w| w == "static")))
        }
        Lang::Java => modifiers().any(|w| w == "private"),
        Lang::CSharp => {
            // A member of a class or struct with no access modifier is
            // private, unless it implements an interface's member by its
            // full name, `void IDisposable.Dispose()`.
            let access = |w: &str| matches!(w, "public" | "private" | "protected" | "internal");
            let member = enclosing.is_some_and(|e| matches!(e.kind, Class | Struct));
            let explicit = sym.signature.contains(&format!(".{}", sym.name));
            modifiers().any(|w| w == "private") || (member && !explicit && !modifiers().any(access))
        }
        // A Ruby method is made private by a `private` call before it or
        // around it, which its signature does not show.
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx | Lang::Ruby => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, name: &str, parent: Option<&str>) -> Option<u8> {
        Query::new(query).score(name, parent)
    }

    fn names_of(path: &str, source: &str) -> SymbolNames {
        let parsed = crate::ast::parser::parse_path(std::path::Path::new(path), source).unwrap();
        SymbolNames::new(&parsed, source)
    }

    /// The names of the symbols of `source` that reach as far as `reach`.
    fn reaching(path: &str, source: &str, reach: Reach) -> Vec<String> {
        let names = names_of(path, source);
        (0..names.len())
            .filter(|&s| names.reach(s) == reach)
            .map(|s| names.name(s).unwrap().to_string())
            .collect()
    }

    /// The names of the symbols of `source` the language keeps private.
    fn private(path: &str, source: &str) -> Vec<String> {
        reaching(path, source, Reach::Private)
    }

    #[test]
    fn the_trait_of_an_impl_is_found_past_generics() {
        let cases = [
            (
                "impl<P> AsyncRead for Pin<P> where P: DerefMut",
                Some("AsyncRead"),
            ),
            ("unsafe impl<T: Send> Sync for Foo<T>", Some("Sync")),
            (
                "impl ops::BitOrAssign<Ready> for Ready",
                Some("ops::BitOrAssign<Ready>"),
            ),
            ("impl<F: for<'a> Fn(&'a u8) -> u8> Tr for F", Some("Tr")),
            ("impl<F> From<fn() -> F> for X", Some("From<fn() -> F>")),
            ("impl !Send for X", Some("Send")),
            ("impl<'a> ReadBuf<'a>", None),
            ("impl Formatter", None),
        ];
        for (sig, want) in cases {
            assert_eq!(impl_trait(sig), want, "{sig}");
        }
        assert_eq!(last_ident("ops::BitOrAssign<Ready>"), Some("BitOrAssign"));
        let src = "struct S;\nimpl fmt::Display for S {\n    fn fmt(&self) {}\n}\nimpl S {\n    fn own(&self) {}\n}\n";
        let names = names_of("a.rs", src);
        let of = |name: &str| {
            (0..names.len())
                .find(|&s| names.name(s) == Some(name))
                .unwrap()
        };
        assert_eq!(names.trait_of(of("fmt")), Some(mentions::hash("Display")));
        assert_eq!(names.trait_of(of("own")), None);
    }

    #[test]
    fn private_symbols_follow_each_language() {
        let rust = "pub struct S;\nstruct P;\nimpl S {\n    pub fn a(&self) {}\n    fn b(&self) {}\n}\n\
                    impl Clone for S {\n    fn clone(&self) -> Self { S }\n}\n\
                    pub trait T {\n    fn t(&self);\n}\npub(crate) fn c() {}\npub(self) fn d() {}\n\
                    fn e() {}\nmacro_rules! m { () => {} }\n";
        assert_eq!(private("a.rs", rust), ["P", "b", "d", "e"]);
        let go = "package p\n\nfunc Pub() {}\nfunc priv() {}\ntype t struct{}\nfunc (x t) Run() {}\nfunc (x t) stop() {}\n";
        assert_eq!(private("a.go", go), ["priv", "t", "stop"]);
        let py = "def _h(): pass\ndef pub(): pass\nclass C:\n    def __init__(self): pass\n    def _x(self): pass\n";
        assert_eq!(private("a.py", py), ["_h", "_x"]);
        let c = "static int helper(int x) { return x; }\nint api(void) { return 0; }\n\
                 static inline struct s *get(void) { return 0; }\n#define N 1\nstruct t { int a; };\n";
        assert_eq!(private("a.c", c), ["helper", "get", "N", "t"]);
        let cpp = "struct S {\n    static int make() { return 1; }\n};\nstatic void local() {}\n";
        assert_eq!(private("a.cpp", cpp), ["S", "local"]);
        let java =
            "class A {\n    private int f() { return 1; }\n    public int g() { return 2; }\n}\n";
        assert_eq!(private("A.java", java), ["f"]);
        // A C# member is private unless it says otherwise; a type outside
        // any other is not.
        let cs = "class A : IDisposable {\n    void M() {}\n    public void P() {}\n    internal int I { get; }\n    \
                  private int F() { return 1; }\n    void IDisposable.Dispose() {}\n}\n\
                  interface I2 {\n    void N();\n}\n";
        assert_eq!(private("A.cs", cs), ["M", "F"]);
    }

    #[test]
    fn a_js_module_keeps_what_it_does_not_export() {
        let module = "import { x } from './x';\nfunction f() {}\nexport function g() {}\n\
                      const h = 1;\nclass K {\n    m() {}\n}\nexport { h };\nexport default K;\n";
        assert_eq!(private("a.ts", module), ["f"]);
        // A name that is part of the word `export` is still found after it.
        let short =
            "import { x } from './x';\nexport const e = 1;\nexport const port = 2;\nconst t = 3;\n";
        assert_eq!(private("a.ts", short), ["t"]);
        // A script's declarations are global.
        assert!(private("a.js", "function f() {}\nvar v = 1;\n").is_empty());
        assert!(
            private(
                "a.d.ts",
                "declare function clearTimeout(t: number): void;\n"
            )
            .is_empty()
        );
        // So are a CommonJS module's, unless exported.
        let commonjs = "const path = require('path');\nconst root = 1;\nfunction a() {}\n\
                        function b() {}\nfunction c() {}\nmodule.exports = { a };\n\
                        exports.b = b;\n";
        assert_eq!(private("a.js", commonjs), ["root", "c"]);
        let single = "const fs = require('fs');\nconst v = 1;\nfunction main() {}\n\
                      module.exports = main;\n";
        assert_eq!(private("b.js", single), ["v"]);
        // `x.require(...)` is no CommonJS call.
        assert!(private("c.js", "var v = loader.require('x');\n").is_empty());
    }

    #[test]
    fn what_a_function_holds_stays_in_it() {
        let ts = "export function outer() {\n    const inner = () => 1;\n    function helper() {}\n}\n\
                  export class C {\n    m() {\n        let i = 0;\n    }\n}\n";
        let local = reaching("a.ts", ts, Reach::Local);
        assert!(!local.contains(&"outer".to_string()) && !local.contains(&"m".to_string()));
        assert!(local.contains(&"helper".to_string()), "{local:?}");
        let py = "def f():\n    def g():\n        pass\n    class H:\n        def k(self): pass\n";
        assert_eq!(reaching("a.py", py, Reach::Local), ["g", "H", "k"]);
    }

    #[test]
    fn a_go_file_names_its_package() {
        let names = names_of(
            "a.go",
            "// Package p.\npackage p_q // import \"x/p\"\n\nfunc F() {}\n",
        );
        assert_eq!(names.package(), Some(mentions::hash("p_q")));
        assert_eq!(names_of("a.rs", "fn f() {}\n").package(), None);
    }

    #[test]
    fn a_go_method_belongs_to_its_receiver() {
        let names = names_of(
            "a.go",
            "package p\n\nfunc (l *logWriter) byte(x byte) {}\nfunc (Pair[K, V]) Key() K {}\nfunc (*T) M() {}\nfunc F() {}\n",
        );
        let parents: Vec<_> = (0..names.len()).map(|s| names.parent(s)).collect();
        assert_eq!(parents, [Some("logWriter"), Some("Pair"), Some("T"), None]);
    }

    #[test]
    fn scores_like_the_ranking_says() {
        assert_eq!(score("load", "load", None), Some(0));
        assert_eq!(score("Cfg::load", "load", Some("Cfg")), Some(0));
        assert_eq!(score("LOAD", "load", None), Some(1));
        assert_eq!(score("lo", "LoadAll", None), Some(2));
        assert_eq!(score("all", "LoadAll", None), Some(3));
        assert_eq!(score("cfg", "load", Some("app::Cfg")), Some(4));
        assert_eq!(score("g::lo", "load", Some("Cfg")), Some(4));
        assert_eq!(score("zzz", "load", Some("Cfg")), None);
        // Non-ASCII names fall back to full lower-casing.
        assert_eq!(score("ÉTÉ", "été", None), Some(1));
        assert_eq!(score("té", "Été_x", None), Some(3));
        // Longer than the stack buffer.
        let long = format!("{}Tail", "x".repeat(STACK_NAME));
        assert_eq!(score("tail", &long, None), Some(3));
        assert_eq!(score(&long.to_uppercase(), &long, None), Some(1));
    }

    #[test]
    fn names_keep_the_order_kind_and_line_of_the_symbols() {
        let sym = |name: &str, parent: Option<&str>, kind, line| Symbol {
            name: name.into(),
            kind,
            parent: parent.map(Into::into),
            signature: String::new(),
            start_line: line,
            end_line: line,
            start_byte: 0,
            end_byte: 0,
            body: None,
            depth: 0,
        };
        let names = SymbolNames::of(
            &[
                sym("Cfg", None, SymbolKind::Struct, 1),
                sym("load", Some("Cfg"), SymbolKind::Method, 3),
                sym("loader", None, SymbolKind::Function, 9),
            ],
            None,
            "",
        );
        let q = Query::new("load");
        let all: Vec<_> = names.matches(&q, None).collect();
        assert_eq!(all, [(1, 0, 3), (2, 2, 9)]);
        let fns: Vec<_> = names.matches(&q, Some(SymbolKind::Function)).collect();
        assert_eq!(fns, [(2, 2, 9)]);
        assert_eq!(names.len(), 3);
        assert_eq!(names.name(2), Some("loader"));
        assert_eq!(names.name(3), None);
    }
}
