//! The identifier tokens a file mentions, as hashes.
//!
//! `codebase_map` ranks files by the names they use that other files define,
//! and `find_references` skips files that cannot hold the name it looks for.
//! Both only need to know which tokens a file has and how often, so a file
//! keeps a sorted set of 32-bit token hashes with a count each: about 5 bytes
//! per distinct token, small enough to sit beside its symbol names.

use crate::ast::parser::{Lang, ParsedFile, is_ident_char};

/// FNV-1a over the bytes of `token`. Two tokens may share a hash; callers
/// treat a match as "maybe", never as proof.
pub fn hash(token: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in token.as_bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Hash of a Rust macro invocation `name!`, recorded beside `name`'s own:
/// a `macro_rules!` macro is only ever used that way, so a mention of the
/// bare name, such as `feature` in `#[cfg(feature = "x")]`, is not a use.
/// It is the hash of `name!`, which no token has.
pub fn macro_hash(name: &str) -> u32 {
    let mut h = hash(name);
    h ^= u32::from(b'!');
    h.wrapping_mul(0x0100_0193)
}

/// Hash of a token with hash `h` used as a field or method of a value,
/// `x.name` or `x->name`, recorded beside the token's own. It is the hash
/// of `name.`, which no token has.
pub fn member_hash(h: u32) -> u32 {
    (h ^ u32::from(b'.')).wrapping_mul(0x0100_0193)
}

/// Hash of the [`module_stem`] of a path a JavaScript or TypeScript file
/// imports: the hash of `stem/`, which no token has, so a module named like
/// one of its exports, `red` in `red.ts`, is told apart from it.
pub fn import_hash(stem: &str) -> u32 {
    import_hash_of(hash(stem))
}

/// The [`import_hash`] of a stem whose [`hash`] is `h`.
pub fn import_hash_of(h: u32) -> u32 {
    (h ^ u32::from(b'/')).wrapping_mul(0x0100_0193)
}

/// Hash of the [`module_stem`] of a relative path, `./a/state`, a
/// JavaScript or TypeScript file imports, recorded with the
/// [`import_hash`] of the whole [`relative_path`] in place of the stem's
/// own: which `state.ts` such a path names depends on the importing file's
/// directory. It is the hash of `stem~`, which no token has.
pub fn relative_hash(stem: &str) -> u32 {
    (hash(stem) ^ u32::from(b'~')).wrapping_mul(0x0100_0193)
}

/// The canonical form of a relative import path given by its `/`-separated
/// `parts`: `.` and `name/..` resolved, the extensions of the last part and
/// a last `index` dropped, as `./a/state` or `../state` (`./` for the
/// directory itself), so that `./a/../state.js` and `./state` are one.
pub fn relative_path<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for part in parts {
        match part {
            "" | "." => {}
            ".." if stack.last().is_some_and(|&last| last != "..") => {
                stack.pop();
            }
            part => stack.push(part),
        }
    }
    if let Some(last) = stack.last_mut()
        && *last != ".."
    {
        *last = last.split('.').next().unwrap_or(last);
    }
    if stack.last() == Some(&"index") {
        stack.pop();
    }
    let joined = stack.join("/");
    if stack.first() == Some(&"..") {
        joined
    } else {
        format!("./{joined}")
    }
}

/// Hash of the name of a package, `vscode` or `node:path`, a JavaScript or
/// TypeScript file imports, which a declaration file may declare in
/// `declare module 'vscode'`. It is the hash of `name@`, which no token has.
pub fn package_hash(name: &str) -> u32 {
    (hash(name) ^ u32::from(b'@')).wrapping_mul(0x0100_0193)
}

/// Whether an import path is relative to the importing file, `./x` or `../x`.
fn is_relative(path: &str) -> bool {
    path == "." || path == ".." || path.starts_with("./") || path.starts_with("../")
}

/// Hash of a C `struct`, `union` or `enum` tag `name` written after its
/// keyword, `struct name`, recorded beside the token's own: tags have a
/// namespace of their own, so a variable or field `name` is no use of one.
/// It is the hash of `name{`, which no token has.
pub fn tag_hash(name: &str) -> u32 {
    (hash(name) ^ u32::from(b'{')).wrapping_mul(0x0100_0193)
}

/// Whether [`Mentions::of`] counts the members of a file in `lang` apart.
pub fn tracks_members(lang: Lang) -> bool {
    !matches!(lang, Lang::Java | Lang::CSharp | Lang::GdScript)
}

/// Whether the source `before` a token ends in one of `keywords`, as a word
/// of its own.
fn ends_in_keyword(before: &[u8], keywords: &[&[u8]]) -> bool {
    let before = before.trim_ascii_end();
    keywords.iter().any(|keyword| {
        before.strip_suffix(*keyword).is_some_and(|rest| {
            !rest
                .last()
                .is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'_')
        })
    })
}

/// Whether the Go source `before` a token ends in the keyword `package`.
fn is_package_clause(before: &[u8]) -> bool {
    ends_in_keyword(before, &[b"package"])
}

/// Whether the C source `before` a token ends in `struct`, `union` or
/// `enum`, so the token is a tag.
fn is_tag(before: &[u8]) -> bool {
    ends_in_keyword(before, &[b"struct", b"union", b"enum"])
}

/// Whether the source `before` a token of `lang` ends in a member access:
/// `.` (not the range or spread `..`), maybe followed by whitespace, as in a
/// method chain split over lines; in C and C++ also `->` (a return type
/// elsewhere); in Rust and C++, a path through a type, `Type::` or
/// `Vec::<T>::` in Rust (whose types are capitalized, unlike its modules),
/// any `name::` in C++.
fn is_member_access(lang: Lang, before: &[u8]) -> bool {
    let before = before.trim_ascii_end();
    match before {
        [.., b'.', b'.'] => false,
        [.., b'.'] => true,
        [.., b'-', b'>'] => matches!(lang, Lang::C | Lang::Cpp),
        [rest @ .., b':', b':'] if matches!(lang, Lang::Rust | Lang::Cpp) => {
            let rest = rest.trim_ascii_end();
            let ident_start = rest
                .iter()
                .rposition(|&b| !(b.is_ascii_alphanumeric() || b == b'_'))
                .map_or(0, |i| i + 1);
            let segment = &rest[ident_start..];
            rest.ends_with(b">")
                || match lang {
                    Lang::Rust => segment.first().is_some_and(u8::is_ascii_uppercase),
                    Lang::Cpp => !segment.is_empty(),
                    _ => false,
                }
        }
        _ => false,
    }
}

/// Hash of a name with hash `name` qualified by a token with hash `owner`,
/// `owner.name`, as a Go file names what another package exports and a
/// Python file what a module defines; a Python `from owner import name`
/// counts once too. It differs from every hash of a single name.
pub fn qualified_hash(owner: u32, name: u32) -> u32 {
    (owner.rotate_left(13) ^ name ^ u32::from(b':')).wrapping_mul(0x0100_0193)
}

/// The identifier right before the `.` that ends `before`, `a` in `a.b`.
fn qualifier(before: &[u8]) -> Option<&[u8]> {
    let rest = before.strip_suffix(b".")?;
    let start = rest
        .iter()
        .rposition(|&b| !(b.is_ascii_alphanumeric() || b == b'_'))
        .map_or(0, |i| i + 1);
    let ident = &rest[start..];
    ident
        .first()
        .is_some_and(|b| !b.is_ascii_digit())
        .then_some(ident)
}

/// The name hash a module or package is imported under: the module's own
/// for an alias of it, `np` in `import numpy as np`, else the token's.
fn unaliased(token: u32, aliases: &[(u32, u32)]) -> u32 {
    aliases
        .iter()
        .find(|&&(alias, _)| alias == token)
        .map_or(token, |&(_, name)| name)
}

/// What the Python imports of `source` name: the [`import_hash`] of each
/// part of each module path and of each name imported from one, which may
/// be a module, and the [`qualified_hash`] of each `from m import a` (of
/// `a.b.m`, its last part, and `*` for all names), pushed on `out`, and the (alias, name) hashes of each
/// `import a.m as n` and `from a import m as n`, pushed on `aliases`.
fn python_imports(source: &str, out: &mut Vec<u32>, aliases: &mut Vec<(u32, u32)>) {
    let mut lines = source.lines();
    while let Some(line) = lines.next() {
        let line = line.split('#').next().unwrap_or(line).trim();
        if let Some(rest) = line.strip_prefix("from ") {
            let Some((module, names)) = rest.split_once(" import ") else {
                continue;
            };
            let module = module.trim();
            out.extend(module.split('.').filter(|m| !m.is_empty()).map(import_hash));
            let module = module.rsplit('.').next().unwrap_or("");
            let mut names = names.trim().to_string();
            if let Some(open) = names.strip_prefix('(') {
                names = open.to_string();
                while !names.contains(')') {
                    let Some(next) = lines.next() else { break };
                    names.push(',');
                    names.push_str(next.split('#').next().unwrap_or(next));
                }
            }
            let names = names.split(')').next().unwrap_or("");
            for item in names.split(',') {
                let mut words = item.split_whitespace();
                let Some(name) = words.next() else { continue };
                out.push(import_hash(name));
                if !module.is_empty() {
                    out.push(qualified_hash(hash(module), hash(name)));
                }
                if words.next() == Some("as")
                    && let Some(alias) = words.next()
                {
                    aliases.push((hash(alias), hash(name)));
                }
            }
        } else if let Some(rest) = line.strip_prefix("import ") {
            for item in rest.split(',') {
                let mut words = item.split_whitespace();
                let Some(path) = words.next() else { continue };
                out.extend(path.split('.').map(import_hash));
                if words.next() == Some("as")
                    && let Some(alias) = words.next()
                {
                    let module = path.rsplit('.').next().unwrap_or(path);
                    aliases.push((hash(alias), hash(module)));
                }
            }
        }
    }
}

/// The (alias, package name) hashes of the Go imports of `source` that name
/// their package, `foo "a/b"`, the name being the last part of the path
/// not a major version, `b` for `a/b/v2`.
fn go_aliases(source: &str, aliases: &mut Vec<(u32, u32)>) {
    let mut block = false;
    for line in source.lines() {
        let line = line.trim();
        let spec = if block {
            if line.starts_with(')') {
                block = false;
                continue;
            }
            line
        } else if let Some(rest) = line.strip_prefix("import") {
            let rest = rest.trim_start();
            if rest.starts_with('(') {
                block = true;
                continue;
            }
            rest
        } else {
            continue;
        };
        let Some((alias, path)) = spec.split_once(char::is_whitespace) else {
            continue;
        };
        let path = path.trim().trim_matches('"');
        if alias == "_" || alias == "." || !is_token(alias) {
            continue;
        }
        let is_version = |part: &str| {
            part.strip_prefix('v')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        };
        if let Some(name) = path.rsplit('/').find(|part| !is_version(part)) {
            aliases.push((hash(alias), hash(name)));
        }
    }
}

/// Whether `name` is a whole identifier token, as the tokens of
/// [`Mentions::of`] are. Only such a name can be looked up in them.
pub fn is_token(name: &str) -> bool {
    !name.is_empty() && name.chars().all(is_ident_char)
}

/// The identifier token starting at byte `start` of `source`: the longest run
/// of identifier characters there, or `None` when there is none.
fn token_at(source: &str, start: usize) -> Option<&str> {
    let rest = source.get(start..)?;
    let end = rest
        .char_indices()
        .find(|&(_, c)| !is_ident_char(c))
        .map_or(rest.len(), |(i, _)| i);
    (end > 0).then(|| &rest[..end])
}

/// The name a JavaScript or TypeScript module is imported by: the last part
/// of its path without extensions, `strings` for `a/strings.test.ts`, or the
/// directory of an `index`. It need not be an identifier, `lodash-es`, as
/// long as it is not empty.
pub fn module_stem(path: &str) -> Option<&str> {
    let mut parts = path.trim_end_matches('/').rsplit('/');
    let stem = parts.next()?.split('.').next()?;
    let stem = if stem == "index" {
        parts.next()?.split('.').next()?
    } else {
        stem
    };
    (!stem.is_empty()).then_some(stem)
}

/// The paths a JavaScript or TypeScript file imports in `from '...'`, `import '...'`, `import('...')` and `require('...')`.
/// They are strings, which hold no mentions, yet they name the modules whose
/// exports the file uses.
fn imported_paths(source: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in source.lines() {
        for key in ["from", "import", "require"] {
            let mut rest = line;
            while let Some(at) = rest.find(key) {
                let word_start = rest[..at]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !is_ident_char(c));
                let after = rest[at + key.len()..].trim_start();
                rest = &rest[at + key.len()..];
                let after = after.strip_prefix('(').unwrap_or(after).trim_start();
                let Some(quote) = after
                    .chars()
                    .next()
                    .filter(|c| matches!(c, '\'' | '"' | '`'))
                else {
                    continue;
                };
                let body = &after[1..];
                if let Some(end) = body.find(quote)
                    && word_start
                {
                    out.push(&body[..end]);
                }
            }
        }
    }
    out
}

/// Whether an import path names a package or a builtin module, `lodash`,
/// `@scope/name` or `node:path`, rather than a file of the tree: the stem of
/// `import * as path from 'path'` is no use of a `path.ts` of its own.
fn names_a_package(path: &str) -> bool {
    let slashes = path.matches('/').count();
    !is_relative(path)
        && (path.contains(':') || slashes == 0 || (path.starts_with('@') && slashes == 1))
}

/// The stems of the headers a C or C++ file includes, `list` for
/// `#include <linux/list.h>`. Like a JavaScript import path, the header's
/// name is a string, yet it names the file whose definitions the file uses.
fn included_headers(source: &str) -> Vec<&str> {
    source
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix('#')?.trim_start();
            let rest = rest.strip_prefix("include")?.trim_start();
            let close = match rest.chars().next()? {
                '"' => '"',
                '<' => '>',
                _ => return None,
            };
            let path = &rest[1..1 + rest[1..].find(close)?];
            let file = path.rsplit('/').next()?;
            let stem = file.split('.').next()?;
            (!stem.is_empty()).then_some(stem)
        })
        .collect()
}

/// Distinct identifier tokens of a file, as sorted hashes with how often each
/// occurs (at most 255).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Mentions {
    hashes: Box<[u32]>,
    counts: Box<[u8]>,
}

impl Mentions {
    /// The tokens at the identifier offsets recorded at parse time, so
    /// comments and strings are left out. A definition's own name is one of
    /// them.
    ///
    /// In Rust, a token right before `!` (but not `!=`) also counts as the
    /// macro invocation [`macro_hash`]. A token right after `.` (a field or
    /// method of a value, or a name in a Go package or Python or JavaScript
    /// module), or after `->` or a path through a type ([`is_member_access`]),
    /// also counts as [`member_hash`]. Java, C# and GDScript have no free
    /// functions, so they leave that out. The name of a Go file's `package`
    /// clause is no mention. In Go and Python, a token right after `name.`
    /// also counts as the [`qualified_hash`] of the two, `name` being
    /// unaliased as the imports say, and a Python file mentions that of
    /// each name it imports from a module. A JavaScript or TypeScript file mentions the
    /// [`import_hash`] of the stem of each path it imports, or for a
    /// relative path the [`relative_hash`] of its stem and the
    /// [`import_hash`] of its [`relative_path`], or for a package the
    /// [`package_hash`] of its name, and a C or C++ file that of
    /// each header it includes. In C, a token after `struct`, `union` or
    /// `enum` also counts as [`tag_hash`].
    pub fn of(parsed: &ParsedFile, source: &str) -> Self {
        let rust = parsed.lang == Lang::Rust;
        let go = parsed.lang == Lang::Go;
        let c = parsed.lang == Lang::C;
        let members = tracks_members(parsed.lang);
        let qualifies = matches!(parsed.lang, Lang::Go | Lang::Python);
        let bytes = source.as_bytes();
        let mut all: Vec<u32> = Vec::with_capacity(parsed.idents.len());
        let mut aliases = Vec::new();
        match parsed.lang {
            Lang::Python => python_imports(source, &mut all, &mut aliases),
            Lang::Go => go_aliases(source, &mut aliases),
            _ => {}
        }
        for &start in &parsed.idents {
            let start = start as usize;
            let Some(token) = token_at(source, start) else {
                continue;
            };
            if go && is_package_clause(&bytes[..start]) {
                continue;
            }
            let h = hash(token);
            all.push(h);
            if qualifies
                && let Some(owner) = qualifier(&bytes[..start])
                && let Ok(owner) = std::str::from_utf8(owner)
            {
                all.push(qualified_hash(unaliased(hash(owner), &aliases), h));
            }
            if rust {
                let after = &bytes[start + token.len()..];
                if after.first() == Some(&b'!') && after.get(1) != Some(&b'=') {
                    all.push(macro_hash(token));
                }
            }
            if members && is_member_access(parsed.lang, &bytes[..start]) {
                all.push(member_hash(hash(token)));
            }
            if c && is_tag(&bytes[..start]) {
                all.push(tag_hash(token));
            }
        }
        if matches!(parsed.lang, Lang::JavaScript | Lang::TypeScript | Lang::Tsx) {
            for path in imported_paths(source) {
                if names_a_package(path) {
                    all.push(package_hash(path));
                    continue;
                }
                let Some(stem) = module_stem(path) else {
                    continue;
                };
                if is_relative(path) {
                    all.push(relative_hash(stem));
                    all.push(import_hash(&relative_path(path.split('/'))));
                } else {
                    all.push(import_hash(stem));
                }
            }
        }
        if matches!(parsed.lang, Lang::C | Lang::Cpp) {
            all.extend(included_headers(source).into_iter().map(import_hash));
        }
        all.sort_unstable();
        let mut hashes = Vec::new();
        let mut counts: Vec<u8> = Vec::new();
        for h in all {
            match (hashes.last(), counts.last_mut()) {
                (Some(&last), Some(count)) if last == h => {
                    *count = count.saturating_add(1);
                }
                _ => {
                    hashes.push(h);
                    counts.push(1);
                }
            }
        }
        Mentions {
            hashes: hashes.into_boxed_slice(),
            counts: counts.into_boxed_slice(),
        }
    }

    /// Whether a token with hash `h` may occur in the file. `false` is
    /// certain: no token of the file has that hash.
    pub fn contains(&self, h: u32) -> bool {
        self.hashes.binary_search(&h).is_ok()
    }

    /// How often a token with hash `h` occurs (at most 255).
    pub fn count(&self, h: u32) -> u8 {
        self.hashes
            .binary_search(&h)
            .map_or(0, |at| self.counts[at])
    }

    /// (hash, count) of each distinct token, by ascending hash.
    pub fn iter(&self) -> impl Iterator<Item = (u32, u8)> + '_ {
        self.hashes.iter().copied().zip(self.counts.iter().copied())
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Heap size, for the memory budget.
    pub fn weight(&self) -> usize {
        self.hashes.len() * (size_of::<u32>() + size_of::<u8>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parser;
    use std::path::Path;

    fn mentions(path: &str, source: &str) -> Mentions {
        let parsed = parser::parse_path(Path::new(path), source).unwrap();
        Mentions::of(&parsed, source)
    }

    #[test]
    fn counts_identifier_tokens_but_not_comments_or_strings() {
        let m = mentions(
            "a.rs",
            "// helper in a comment\nfn run() { helper(); helper(); let s = \"helper\"; }\n",
        );
        let count = |name| m.iter().find(|&(h, _)| h == hash(name)).map(|(_, c)| c);
        assert_eq!(count("helper"), Some(2));
        assert_eq!(count("run"), Some(1));
        assert_eq!(count("s"), Some(1));
        assert_eq!(count("comment"), None);
        assert!(m.contains(hash("helper")));
        assert!(!m.contains(hash("nothing_here")));
        assert_eq!(m.len(), 3);
        assert_eq!(m.weight(), 15);
    }

    #[test]
    fn hashes_are_sorted_and_counts_saturate() {
        let body = "x();".repeat(300);
        let m = mentions("a.rs", &format!("fn f() {{ {body} }}\n"));
        let hashes: Vec<u32> = m.iter().map(|(h, _)| h).collect();
        assert!(hashes.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(
            m.iter().find(|&(h, _)| h == hash("x")).map(|(_, c)| c),
            Some(u8::MAX)
        );
    }

    #[test]
    fn a_token_is_the_whole_run_of_identifier_characters() {
        assert_eq!(token_at("foo_bar(1)", 0), Some("foo_bar"));
        assert_eq!(token_at("$el.x", 0), Some("$el"));
        assert_eq!(token_at("#priv", 0), None);
        assert_eq!(token_at("été+1", 0), Some("été"));
        assert_eq!(token_at("ab", 5), None);
        assert!(is_token("Parser"));
        assert!(!is_token("r#type"));
        assert!(!is_token(""));
    }

    #[test]
    fn a_rust_macro_call_counts_as_the_macro() {
        let m = mentions(
            "a.rs",
            "#[cfg(feature = \"x\")]\nfn f() { doc!(1); if a!=b {} }\n",
        );
        assert!(m.contains(hash("doc")));
        assert!(m.contains(macro_hash("doc")));
        assert!(m.contains(hash("feature")));
        assert!(!m.contains(macro_hash("feature")));
        assert!(!m.contains(macro_hash("a")));
        // No token ends in `!`, so no token has this hash.
        assert_eq!(macro_hash("doc"), hash("doc!"));
    }

    #[test]
    fn a_field_or_method_of_a_value_counts_as_a_member() {
        let m = mentions(
            "a.c",
            "int f(struct s *p) { return len(p->len) + p\n  .len + p.size; }\n",
        );
        assert_eq!(m.count(hash("len")), 3);
        let member = |name| m.count(member_hash(hash(name)));
        assert_eq!(member("len"), 2);
        assert_eq!(member("size"), 1);
        assert_eq!(member("p"), 0);
        assert_eq!(member_hash(hash("len")), hash("len."));
        let m = mentions(
            "a.rs",
            "fn f() { for i in 0..len { v.push(i) }\n\
             Vec::<u8>::with_capacity(1); Self::make(); crate::a::Cache::get();\n\
             super::helper(); <T as Tr>::build(); }\n",
        );
        let member = |name| m.count(member_hash(hash(name)));
        assert_eq!(member("len"), 0);
        assert_eq!(member("push"), 1);
        for name in ["with_capacity", "make", "get", "build"] {
            assert_eq!(member(name), 1, "{name}");
        }
        assert_eq!(member("helper"), 0);
        assert_eq!(member("Cache"), 0);
        // A return type is no member.
        let m = mentions("a.rs", "fn f() -> Config { g() }\n");
        assert_eq!(m.count(member_hash(hash("Config"))), 0);
        let m = mentions("a.cpp", "int f() { return ns::g() + ::h(); }\n");
        assert_eq!(m.count(member_hash(hash("g"))), 1);
        assert_eq!(m.count(member_hash(hash("h"))), 0);
        let m = mentions("a.py", "def f() -> Path:\n    return os.path.join(a)\n");
        assert_eq!(m.count(member_hash(hash("join"))), 1);
        assert_eq!(m.count(member_hash(hash("Path"))), 0);
        let m = mentions("a.ts", "const a = [...items, x?.y, strings.trim(s)];\n");
        for (name, want) in [("items", 0), ("y", 1), ("trim", 1)] {
            assert_eq!(m.count(member_hash(hash(name))), want, "{name}");
        }
        let m = mentions("A.java", "class A { int f() { return b.size(); } }\n");
        assert_eq!(m.count(member_hash(hash("size"))), 0, "no free functions");
    }

    #[test]
    fn a_go_package_clause_is_no_mention() {
        let m = mentions(
            "a.go",
            "package syscall\n\nfunc f() { syscall(1); fmt.Println() }\n",
        );
        assert_eq!(m.count(hash("syscall")), 1);
        assert_eq!(m.count(member_hash(hash("Println"))), 1);
    }

    #[test]
    fn a_c_tag_counts_apart_from_its_name() {
        let m = mentions(
            "a.c",
            "struct buf *b = buf; union\n  u x; enum kind k; mystruct buf2;\n",
        );
        assert_eq!(m.count(tag_hash("buf")), 1);
        assert_eq!(m.count(hash("buf")), 2);
        assert_eq!(m.count(tag_hash("u")), 1);
        assert_eq!(m.count(tag_hash("kind")), 1);
        assert_eq!(m.count(tag_hash("buf2")), 0);
        assert_eq!(
            mentions("a.cpp", "struct buf *b;\n").count(tag_hash("buf")),
            0
        );
    }

    #[test]
    fn a_c_file_mentions_the_headers_it_includes() {
        let m = mentions(
            "a.c",
            "#include <linux/list.h>\n  #  include \"local.h\"\n#include MACRO\n\
             #define X \"no.h\"\nint x;\n",
        );
        assert!(m.contains(import_hash("list")));
        assert!(m.contains(import_hash("local")));
        assert!(!m.contains(import_hash("no")));
        assert!(!m.contains(import_hash("MACRO")));
    }

    #[test]
    fn a_js_file_mentions_the_modules_it_imports() {
        let m = mentions(
            "a.ts",
            "import { a } from './util/strings.js';\nimport '../polyfill';\n\
             import {\n  b,\n} from \"vs/base/index\";\nconst c = require('lodash/fp');\n\
             const d = await import(`./lazy`);\nconst e = 'from ./not_a_module';\n\
             const g = transform('./skipped');\nimport * as path from 'path';\n\
             import * as os from 'node:os';\nimport { x } from '@scope/pkg';\n",
        );
        for name in ["base", "fp"] {
            assert!(m.contains(import_hash(name)), "{name}");
        }
        // A relative path names one file, by where the importer is.
        for (stem, path) in [
            ("strings", "./util/strings"),
            ("polyfill", "../polyfill"),
            ("lazy", "./lazy"),
        ] {
            assert!(m.contains(relative_hash(stem)), "{stem}");
            assert!(!m.contains(import_hash(stem)), "{stem}");
            assert!(m.contains(import_hash(path)), "{path}");
        }
        assert!(!m.contains(hash("strings")), "no token");
        assert!(!m.contains(relative_hash("skipped")));
        assert!(!m.contains(relative_hash("not_a_module")));
        assert_eq!(relative_path("./a/../state.js".split('/')), "./state");
        assert_eq!(relative_path("../../x/index.ts".split('/')), "../../x");
        assert_eq!(relative_path(".".split('/')), "./");
        assert_eq!(relative_path("..".split('/')), "..");
        for package in ["path", "os", "pkg"] {
            assert!(!m.contains(import_hash(package)), "{package}");
        }
        for package in ["path", "node:os", "@scope/pkg"] {
            assert!(m.contains(package_hash(package)), "{package}");
        }
        // Only JavaScript and TypeScript import by path.
        let python = mentions("a.py", "y = 'from ./strings'\n");
        assert!(!python.contains(import_hash("strings")));
        assert!(!python.contains(relative_hash("strings")));
        assert_eq!(module_stem("a/strings.test.ts"), Some("strings"));
        assert_eq!(module_stem("a/index.ts"), Some("a"));
        assert_eq!(module_stem("./dir/"), Some("dir"));
        assert_eq!(module_stem("index.ts"), None);
        assert_eq!(module_stem("lodash-es"), Some("lodash-es"));
        assert_eq!(module_stem("../"), None);
    }

    #[test]
    fn go_and_python_names_are_qualified_by_their_package() {
        let q = |owner: &str, name: &str| qualified_hash(hash(owner), hash(name));
        let go = mentions(
            "a.go",
            "package a\n\nimport (\n\tstr \"strings\"\n\t\"math/rand/v2\"\n\tr2 \"math/rand/v2\"\n)\n\
             import u \"unsafe\"\n\nfunc f() { str.Contains(x, y); rand.N(3); r2.Int(); u.Sizeof(x); x.y.Z() }\n",
        );
        for (owner, name) in [
            ("strings", "Contains"),
            ("rand", "N"),
            ("rand", "Int"),
            ("unsafe", "Sizeof"),
            ("y", "Z"),
        ] {
            assert!(go.contains(q(owner, name)), "{owner}.{name}");
        }
        assert!(!go.contains(q("str", "Contains")), "unaliased");
        let py = mentions(
            "a.py",
            "import os.path as osp\nfrom types import GenericAlias\nfrom .shape import (\n    Circle,  # c\n    Square as Sq,\n)\n\
             from . import parser\nfrom typing import *\nimport numpy as np\n\
             def f():\n    osp.join(a)\n    np.array(b)\n    parser.parse_args()\n",
        );
        for (owner, name) in [
            ("types", "GenericAlias"),
            ("shape", "Circle"),
            ("shape", "Square"),
            ("typing", "*"),
            ("path", "join"),
            ("numpy", "array"),
            ("parser", "parse_args"),
        ] {
            assert!(py.contains(q(owner, name)), "{owner}.{name}");
        }
        assert!(!py.contains(q("shape", "Sq")));
        // Other languages qualify nothing.
        let js = mentions("a.js", "strings.Contains(x);\n");
        assert!(!js.contains(q("strings", "Contains")));
    }

    #[test]
    fn fnv1a_matches_the_reference_values() {
        assert_eq!(hash(""), 0x811c_9dc5);
        assert_eq!(hash("a"), 0xe40c_292c);
        assert_eq!(hash("foobar"), 0xbf9c_f968);
    }
}
