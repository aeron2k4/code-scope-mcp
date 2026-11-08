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
    (hash(stem) ^ u32::from(b'/')).wrapping_mul(0x0100_0193)
}

/// Whether [`Mentions::of`] counts the members of a file in `lang` apart.
pub fn tracks_members(lang: Lang) -> bool {
    !matches!(lang, Lang::Java | Lang::CSharp | Lang::GdScript)
}

/// Whether the Go source `before` a token ends in the keyword `package`.
fn is_package_clause(before: &[u8]) -> bool {
    before
        .trim_ascii_end()
        .strip_suffix(b"package")
        .is_some_and(|rest| {
            !rest
                .last()
                .is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'_')
        })
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

/// The [`module_stem`]s of the paths a JavaScript or TypeScript file imports
/// in `from '...'`, `import '...'`, `import('...')` and `require('...')`.
/// They are strings, which hold no mentions, yet they name the modules whose
/// exports the file uses.
fn imported_modules(source: &str) -> Vec<&str> {
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
                    out.extend(module_stem(&body[..end]));
                }
            }
        }
    }
    out
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
    /// clause is no mention. A JavaScript or TypeScript file mentions the
    /// [`import_hash`] of each path it imports.
    pub fn of(parsed: &ParsedFile, source: &str) -> Self {
        let rust = parsed.lang == Lang::Rust;
        let go = parsed.lang == Lang::Go;
        let members = tracks_members(parsed.lang);
        let bytes = source.as_bytes();
        let mut all: Vec<u32> = Vec::with_capacity(parsed.idents.len());
        for &start in &parsed.idents {
            let start = start as usize;
            let Some(token) = token_at(source, start) else {
                continue;
            };
            if go && is_package_clause(&bytes[..start]) {
                continue;
            }
            all.push(hash(token));
            if rust {
                let after = &bytes[start + token.len()..];
                if after.first() == Some(&b'!') && after.get(1) != Some(&b'=') {
                    all.push(macro_hash(token));
                }
            }
            if members && is_member_access(parsed.lang, &bytes[..start]) {
                all.push(member_hash(hash(token)));
            }
        }
        if matches!(parsed.lang, Lang::JavaScript | Lang::TypeScript | Lang::Tsx) {
            all.extend(imported_modules(source).into_iter().map(import_hash));
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
    fn a_js_file_mentions_the_modules_it_imports() {
        let m = mentions(
            "a.ts",
            "import { a } from './util/strings.js';\nimport '../polyfill';\n\
             import {\n  b,\n} from \"vs/base/index\";\nconst c = require('lodash');\n\
             const d = await import(`./lazy`);\nconst e = 'from ./not_a_module';\n\
             const g = transform('./skipped');\n",
        );
        for name in ["strings", "polyfill", "base", "lodash", "lazy"] {
            assert!(m.contains(import_hash(name)), "{name}");
        }
        assert!(!m.contains(hash("strings")), "no token");
        assert!(!m.contains(import_hash("skipped")));
        assert!(!m.contains(import_hash("not_a_module")));
        // Only JavaScript and TypeScript import by path.
        let python = mentions("a.py", "from strings import x\ny = 'from ./strings'\n");
        assert!(!python.contains(import_hash("strings")));
        assert_eq!(module_stem("a/strings.test.ts"), Some("strings"));
        assert_eq!(module_stem("a/index.ts"), Some("a"));
        assert_eq!(module_stem("./dir/"), Some("dir"));
        assert_eq!(module_stem("index.ts"), None);
        assert_eq!(module_stem("lodash-es"), Some("lodash-es"));
        assert_eq!(module_stem("../"), None);
    }

    #[test]
    fn fnv1a_matches_the_reference_values() {
        assert_eq!(hash(""), 0x811c_9dc5);
        assert_eq!(hash("a"), 0xe40c_292c);
        assert_eq!(hash("foobar"), 0xbf9c_f968);
    }
}
