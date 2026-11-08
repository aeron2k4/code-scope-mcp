//! Compact symbol names of one file, for `search_symbols`.
//!
//! A workspace-wide search only needs each symbol's name, parent, kind and
//! line to score and rank it. Keeping just those, about 40 bytes a symbol
//! instead of the whole parsed file with its source, lets the names of a
//! repository as large as the Linux kernel stay in memory, so a warm search
//! costs a `stat` per file and no re-read.

use memchr::memmem::Finder;

use crate::ast::parser::{ParsedFile, Symbol, SymbolKind};

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
}

/// Names of a file's symbols, in the order of `ParsedFile::symbols`.
#[derive(Debug, Default)]
pub struct SymbolNames {
    text: String,
    syms: Vec<NameRef>,
    incomplete: bool,
}

impl SymbolNames {
    pub fn new(parsed: &ParsedFile) -> Self {
        let mut names = Self::of(&parsed.symbols);
        names.incomplete = parsed.incomplete;
        names
    }

    fn of(symbols: &[Symbol]) -> Self {
        let mut text = String::new();
        let mut push = |s: &str| {
            let at = u32::try_from(text.len()).unwrap_or(NONE);
            text.push_str(s);
            (at, u32::try_from(s.len()).unwrap_or(0))
        };
        let syms = symbols
            .iter()
            .map(|s| {
                let (name, name_len) = push(&s.name);
                let (parent, parent_len) = s.parent.as_deref().map_or((NONE, 0), &mut push);
                NameRef {
                    name,
                    name_len,
                    parent,
                    parent_len,
                    start_line: u32::try_from(s.start_line).unwrap_or(u32::MAX),
                    kind: s.kind,
                }
            })
            .collect();
        text.shrink_to_fit();
        SymbolNames {
            text,
            syms,
            incomplete: false,
        }
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
        OVERHEAD + self.text.len() + self.syms.len() * size_of::<NameRef>()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, name: &str, parent: Option<&str>) -> Option<u8> {
        Query::new(query).score(name, parent)
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
        let names = SymbolNames::of(&[
            sym("Cfg", None, SymbolKind::Struct, 1),
            sym("load", Some("Cfg"), SymbolKind::Method, 3),
            sym("loader", None, SymbolKind::Function, 9),
        ]);
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
