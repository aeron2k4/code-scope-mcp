//! Find where a symbol name is used, by identifier tokens in the syntax tree.

use std::path::Path;

use serde::Serialize;

use super::cache::ParseCache;
use super::{enclosing_symbol, files_under, line_excerpt, par_flat_map};
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
            max_files: 20_000,
        }
    }
}

/// Every identifier spelled `name` in source files under `dir`, sorted by
/// path and position. `name` may be qualified (`Parser::new`); only the last
/// segment is matched.
pub fn find_references(
    root: &Path,
    dir: &Path,
    cache: &ParseCache,
    name: &str,
    opts: &RefOptions,
) -> Vec<RefHit> {
    let name = name.trim();
    let name = name.rsplit(['.', ':']).next().unwrap_or(name);
    if name.is_empty() {
        return Vec::new();
    }
    let files: Vec<_> = files_under(root, dir, opts.max_files)
        .into_iter()
        .filter(|p| Lang::from_path(p).is_some())
        .collect();

    par_flat_map(&files, |rel| {
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
    })
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
