//! Project-wide symbol search on top of the lazy parse cache.

pub mod cache;

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::ast::parser::{Lang, SymbolKind};
use crate::git;
use cache::ParseCache;

#[derive(Debug, Clone, Serialize)]
pub struct SymbolHit {
    pub path: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub signature: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip)]
    score: u8,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub kind: Option<SymbolKind>,
    pub limit: usize,
    /// Upper bound on files walked, to keep huge repositories responsive.
    pub max_files: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            kind: None,
            limit: 50,
            max_files: 20_000,
        }
    }
}

/// Lower is better; `None` means no match.
fn score(query: &str, query_lower: &str, name: &str, qualified: &str) -> Option<u8> {
    if name == query || qualified == query {
        return Some(0);
    }
    let name_lower = name.to_lowercase();
    if name_lower == query_lower {
        Some(1)
    } else if name_lower.starts_with(query_lower) {
        Some(2)
    } else if name_lower.contains(query_lower) {
        Some(3)
    } else if qualified.to_lowercase().contains(query_lower) {
        Some(4)
    } else {
        None
    }
}

/// Search symbol names across all supported files under `root`. Files are
/// parsed lazily through `cache`, in parallel, on first search.
pub fn search_symbols(
    root: &Path,
    cache: &ParseCache,
    query: &str,
    opts: &SearchOptions,
) -> Vec<SymbolHit> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let query_lower = query.to_lowercase();
    let files: Vec<PathBuf> = git::list_files(root, opts.max_files)
        .into_iter()
        .filter(|p| Lang::from_path(p).is_some())
        .collect();

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(files.len().max(1));
    let chunk = files.len().div_ceil(workers).max(1);

    let mut hits: Vec<SymbolHit> = std::thread::scope(|s| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|chunk| {
                let query_lower = &query_lower;
                s.spawn(move || {
                    let mut local = Vec::new();
                    for rel in chunk {
                        let file = match cache.get(&root.join(rel)) {
                            Ok(f) => f,
                            Err(err) => {
                                tracing::debug!(%err, "skipping file in search");
                                continue;
                            }
                        };
                        for sym in &file.parsed.symbols {
                            if opts.kind.is_some_and(|k| k != sym.kind) {
                                continue;
                            }
                            let qualified = sym.qualified_name();
                            if let Some(score) = score(query, query_lower, &sym.name, &qualified) {
                                local.push(SymbolHit {
                                    path: rel.to_string_lossy().into_owned(),
                                    name: sym.name.clone(),
                                    qualified_name: qualified,
                                    kind: sym.kind,
                                    signature: sym.signature.clone(),
                                    start_line: sym.start_line,
                                    end_line: sym.end_line,
                                    score,
                                });
                            }
                        }
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });

    hits.sort_by(|a, b| {
        a.score
            .cmp(&b.score)
            .then_with(|| a.path.cmp(&b.path))
            .then(a.start_line.cmp(&b.start_line))
    });
    hits.truncate(opts.limit);
    hits
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
            "pub struct Parser;\nimpl Parser { fn parse(&self) {} }\nfn parse_all() {}\n",
        )
        .unwrap();
        fs::write(root.join("tool.py"), "def reparse():\n    pass\n").unwrap();
        fs::write(root.join("notes.txt"), "parse").unwrap();
        dir
    }

    #[test]
    fn ranks_exact_then_prefix_then_substring() {
        let dir = fixture();
        let cache = ParseCache::default();
        let hits = search_symbols(dir.path(), &cache, "parse", &SearchOptions::default());
        let names: Vec<_> = hits.iter().map(|h| h.qualified_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Parser::parse", "Parser", "Parser", "parse_all", "reparse"]
        );
        assert_eq!(hits[0].kind, SymbolKind::Method);
        assert_eq!(hits[4].path, "tool.py");
    }

    #[test]
    fn filters_by_kind_and_limit() {
        let dir = fixture();
        let cache = ParseCache::default();
        let opts = SearchOptions {
            kind: Some(SymbolKind::Function),
            limit: 1,
            ..Default::default()
        };
        let hits = search_symbols(dir.path(), &cache, "PARSE", &opts);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "parse_all");
    }

    #[test]
    fn empty_query_returns_nothing() {
        let dir = fixture();
        let cache = ParseCache::default();
        assert!(search_symbols(dir.path(), &cache, "  ", &SearchOptions::default()).is_empty());
    }
}
