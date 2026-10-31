//! Regex search over workspace files, annotated with the enclosing symbol.

use std::path::Path;

use regex::Regex;
use serde::Serialize;

use super::cache::ParseCache;
use super::{enclosing_symbol, files_under, line_excerpt, par_flat_map};
use crate::ast::parser::Lang;

/// Files larger than this are skipped (same bound as the parse cache).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Bytes inspected for a NUL to detect binary files.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct TextHit {
    pub path: String,
    pub line: usize,
    /// Trimmed source line.
    pub text: String,
    /// Qualified name of the innermost enclosing symbol (source files only).
    pub enclosing: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TextOptions {
    /// Upper bound on files walked, to keep huge repositories responsive.
    pub max_files: usize,
}

impl Default for TextOptions {
    fn default() -> Self {
        TextOptions { max_files: 20_000 }
    }
}

/// Lines matching `re` in text files under `dir`, one hit per line, sorted by
/// path and line. Binary, non-UTF-8 and oversized files are skipped.
pub fn search_text(
    root: &Path,
    dir: &Path,
    cache: &ParseCache,
    re: &Regex,
    opts: &TextOptions,
) -> Vec<TextHit> {
    let files = files_under(root, dir, opts.max_files);
    par_flat_map(&files, |rel| {
        let abs = root.join(rel);
        let Some(source) = read_text(&abs) else {
            return Vec::new();
        };

        // One hit per line: the first match on it.
        let mut starts: Vec<(usize, usize)> = Vec::new();
        let mut line = 1;
        let mut counted = 0;
        for m in re.find_iter(&source) {
            line += source[counted..m.start()].matches('\n').count();
            counted = m.start();
            if starts.last().is_none_or(|&(l, _)| l != line) {
                starts.push((line, m.start()));
            }
        }
        if starts.is_empty() {
            return Vec::new();
        }

        // Symbols only for source files that matched, so the walk stays cheap.
        let parsed = Lang::from_path(rel).and_then(|_| cache.get(&abs).ok());
        let path = rel.to_string_lossy().into_owned();
        starts
            .into_iter()
            .map(|(line, byte)| TextHit {
                path: path.clone(),
                line,
                text: line_excerpt(&source, byte),
                enclosing: parsed
                    .as_ref()
                    .and_then(|f| enclosing_symbol(&f.parsed.symbols, byte))
                    .map(|s| s.qualified_name()),
            })
            .collect()
    })
}

fn read_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_lines_with_enclosing_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "// TODO: top\nfn run() {\n    // TODO inside TODO\n}\n",
        )
        .unwrap();
        fs::write(root.join("notes.md"), "intro\n- TODO docs\n").unwrap();
        fs::write(root.join("blob.bin"), b"TODO\0\0").unwrap();
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::write(root.join("ignored/x.rs"), "// TODO\n").unwrap();

        let cache = ParseCache::default();
        let re = Regex::new("TODO").unwrap();
        let hits = search_text(root, root, &cache, &re, &TextOptions::default());
        let found: Vec<_> = hits
            .iter()
            .map(|h| (h.path.as_str(), h.line, h.enclosing.as_deref()))
            .collect();
        assert_eq!(
            found,
            [
                ("notes.md", 2, None),
                ("src/lib.rs", 1, None),
                ("src/lib.rs", 3, Some("run")),
            ]
        );
        assert_eq!(hits[2].text, "// TODO inside TODO");

        let only_src = search_text(
            root,
            &root.join("src"),
            &cache,
            &re,
            &TextOptions::default(),
        );
        assert_eq!(only_src.len(), 2);
    }
}
