//! Find where a symbol name is used, by identifier tokens in the syntax tree.

use std::path::Path;

use serde::Serialize;

use super::cache::ParseCache;
use super::{DEFAULT_MAX_FILES, Found, enclosing_symbol, files_under, line_excerpt, par_flat_map};
use crate::ast::parser::{self, Lang, SymbolKind};

#[derive(Debug, Clone, Serialize)]
pub struct RefHit {
    pub path: String,
    pub line: usize,
    pub column: usize,
    /// Trimmed source line.
    pub text: String,
    /// Qualified name of the innermost enclosing symbol.
    pub enclosing: Option<String>,
    /// The occurrence is the name of a definition of the symbol.
    pub is_definition: bool,
}

#[derive(Debug, Clone)]
pub struct RefOptions {
    pub include_definitions: bool,
    /// Upper bound on files walked, to keep huge repositories responsive.
    pub max_files: usize,
}

impl Default for RefOptions {
    fn default() -> Self {
        RefOptions {
            include_definitions: false,
            max_files: DEFAULT_MAX_FILES,
        }
    }
}

/// What the text right before an occurrence says about its owner.
#[derive(Debug, PartialEq, Eq)]
enum Owner<'a> {
    /// `Y::name`: a path, so `Y` is the owning type or module.
    Path(&'a str),
    /// `y.name` or `y->name`: a member of whatever `y` is.
    Member(&'a str),
    /// No usable prefix: a bare name or a complex receiver like `f().name`.
    Unknown,
}

fn owner_of(source: &str, start: usize) -> Owner<'_> {
    let before = source[..start].trim_end_matches([' ', '\t']);
    let (rest, is_path) = if let Some(r) = before.strip_suffix("::") {
        (r, true)
    } else if let Some(r) = before
        .strip_suffix('.')
        .or_else(|| before.strip_suffix("->"))
    {
        (r, false)
    } else {
        return Owner::Unknown;
    };
    let rest = rest.trim_end_matches([' ', '\t']);
    let ident_start = rest
        .char_indices()
        .rev()
        .take_while(|&(_, c)| c.is_alphanumeric() || c == '_' || c == '$')
        .last()
        .map_or(rest.len(), |(i, _)| i);
    let ident = &rest[ident_start..];
    match (ident.is_empty(), is_path) {
        (true, _) => Owner::Unknown,
        (false, true) => Owner::Path(ident),
        (false, false) => Owner::Member(ident),
    }
}

/// Whether qualified name `scope` (`a::Foo::bar`) contains segment `owner`.
fn in_scope(scope: Option<&str>, owner: &str) -> bool {
    scope.is_some_and(|s| s.split("::").any(|seg| seg == owner))
}

/// Every identifier spelled `name` in source files under `dir`, sorted by
/// path and position. `name` may be qualified (`Parser::new`): the last
/// segment is matched, and occurrences that provably belong to another owner
/// are dropped, i.e. paths like `Other::new` and definitions inside another
/// type. Member accesses on receivers of unknown type (`x.new()`) are kept.
pub fn find_references(
    root: &Path,
    dir: &Path,
    cache: &ParseCache,
    name: &str,
    opts: &RefOptions,
) -> Found<RefHit> {
    let mut segments = name.trim().rsplit(['.', ':']).filter(|s| !s.is_empty());
    let name = segments.next().unwrap_or_default();
    let owner = segments.next();
    if name.is_empty() {
        return Found {
            hits: Vec::new(),
            truncated: false,
        };
    }
    let (files, truncated) = files_under(root, dir, opts.max_files);
    let files: Vec<_> = files
        .into_iter()
        .filter(|p| Lang::from_path(p).is_some())
        .collect();

    let hits = par_flat_map(&files, |rel| {
        let abs = root.join(rel);
        let file = match cache.get(&abs) {
            Ok(f) => f,
            Err(err) => {
                tracing::debug!(%err, "skipping file in reference search");
                return Vec::new();
            }
        };
        let occurrences = parser::identifier_occurrences(&file.parsed, &file.source, name);
        if occurrences.is_empty() {
            return Vec::new();
        }

        // A definition's name is the first occurrence inside its range.
        // `impl Foo` blocks count as uses of `Foo`, not definitions.
        let symbols = &file.parsed.symbols;
        let definitions: Vec<usize> = symbols
            .iter()
            .filter(|s| s.name == name && s.kind != SymbolKind::Impl)
            .filter_map(|s| {
                occurrences
                    .iter()
                    .find(|o| o.start_byte >= s.start_byte && o.start_byte < s.end_byte)
                    .map(|o| o.start_byte)
            })
            .collect();

        occurrences
            .iter()
            .filter_map(|o| {
                let is_definition = definitions.contains(&o.start_byte);
                if is_definition && !opts.include_definitions {
                    return None;
                }
                // A definition is enclosed by its own symbol; report the parent.
                let innermost = enclosing_symbol(symbols, o.start_byte);
                let enclosing = match innermost {
                    Some(s) if is_definition && s.name == name => s.parent.clone(),
                    other => other.map(|s| s.qualified_name()),
                };
                if let Some(owner) = owner {
                    let foreign = if is_definition {
                        enclosing.is_some() && !in_scope(enclosing.as_deref(), owner)
                    } else {
                        matches!(
                            owner_of(&file.source, o.start_byte),
                            Owner::Path(p)
                                if p != owner && !matches!(p, "Self" | "self" | "super" | "crate")
                        )
                    };
                    if foreign {
                        return None;
                    }
                }
                Some(RefHit {
                    path: rel.to_string_lossy().into_owned(),
                    line: o.line,
                    column: o.column,
                    text: line_excerpt(&file.source, o.start_byte),
                    enclosing,
                    is_definition,
                })
            })
            .collect()
    });
    Found { hits, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn load() {}\n\n// load is documented here\nfn run() {\n    load();\n    let s = \"load\";\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("app.py"),
            "from lib import load\n\ndef main():\n    load()\n",
        )
        .unwrap();
        fs::write(root.join("notes.txt"), "load").unwrap();
        dir
    }

    #[test]
    fn finds_uses_and_skips_comments_strings_and_definitions() {
        let dir = fixture();
        let root = dir.path();
        let cache = ParseCache::default();
        let hits = find_references(root, root, &cache, "load", &RefOptions::default());
        let found: Vec<_> = hits
            .iter()
            .map(|h| (h.path.as_str(), h.line, h.enclosing.as_deref()))
            .collect();
        assert_eq!(
            found,
            [
                ("app.py", 1, None),
                ("app.py", 4, Some("main")),
                ("src/lib.rs", 5, Some("run")),
            ]
        );
        assert_eq!(hits[2].text, "load();");
    }

    #[test]
    fn reads_the_owner_before_an_occurrence() {
        let src = "Foo::new(); x.new(); p->new; f().new; new; Foo :: bar::new";
        let at = |needle: &str| owner_of(src, src.find(needle).unwrap() + needle.len() - 3);
        assert_eq!(at("Foo::new"), Owner::Path("Foo"));
        assert_eq!(at("x.new"), Owner::Member("x"));
        assert_eq!(at("p->new"), Owner::Member("p"));
        assert_eq!(at("().new"), Owner::Unknown);
        assert_eq!(at("; new"), Owner::Unknown);
        assert_eq!(at("bar::new"), Owner::Path("bar"));
    }

    #[test]
    fn qualified_name_drops_other_owners() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("lib.rs"),
            "struct A;\nstruct B;\nimpl A {\n    fn new() -> Self { A }\n    fn make() -> Self { Self::new() }\n}\nimpl B {\n    fn new() -> Self { B }\n}\nfn f(x: A) {\n    A::new();\n    B::new();\n    x.new();\n}\n",
        )
        .unwrap();
        let cache = ParseCache::default();
        let opts = RefOptions {
            include_definitions: true,
            ..Default::default()
        };
        let lines = |name: &str| -> Vec<usize> {
            find_references(root, root, &cache, name, &opts)
                .iter()
                .map(|h| h.line)
                .collect()
        };
        assert_eq!(lines("new"), [4, 5, 8, 11, 12, 13]);
        assert_eq!(lines("A::new"), [4, 5, 11, 13]);
        assert_eq!(lines("B::new"), [5, 8, 12, 13]);
    }

    #[test]
    fn includes_definitions_on_request_and_filters_by_dir() {
        let dir = fixture();
        let root = dir.path();
        let cache = ParseCache::default();
        let opts = RefOptions {
            include_definitions: true,
            ..Default::default()
        };
        let hits = find_references(root, &root.join("src"), &cache, "lib::load", &opts);
        let found: Vec<_> = hits.iter().map(|h| (h.line, h.is_definition)).collect();
        assert_eq!(found, [(1, true), (5, false)]);
        assert_eq!(hits[0].enclosing, None);
    }
}
