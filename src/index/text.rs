//! Regex search over workspace files, annotated with the enclosing symbol.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use regex::Regex;
use serde::Serialize;

use super::cache::ParseCache;
use super::{
    DEFAULT_MAX_FILES, MAX_EXCERPT_CHARS, enclosing_symbol, files_under, line_excerpt,
    par_flat_map, slash_path,
};
use crate::ast::parser::Lang;

/// Files larger than this are skipped (same bound as the parse cache).
pub(crate) const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Bytes inspected for a NUL to detect binary files.
pub(super) const BINARY_SNIFF_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct TextHit {
    pub path: String,
    pub line: usize,
    /// Trimmed source line.
    pub text: String,
    /// Qualified name of the innermost enclosing symbol (source files only).
    pub enclosing: Option<String>,
    /// With [`TextOptions::context`], the numbered lines around the hit and
    /// the hit's own line, indentation kept, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<(usize, String)>,
}

#[derive(Debug, Clone)]
pub struct TextOptions {
    /// Upper bound on files walked, to keep huge repositories responsive.
    pub max_files: usize,
    /// Number of hits to keep; the others are only counted.
    pub limit: usize,
    /// Lines to return before and after each kept hit.
    pub context: usize,
}

impl Default for TextOptions {
    fn default() -> Self {
        TextOptions {
            max_files: DEFAULT_MAX_FILES,
            limit: usize::MAX,
            context: 0,
        }
    }
}

/// Result of [`search_text`].
#[derive(Debug, Clone)]
pub struct TextHits {
    /// The first `limit` hits.
    pub hits: Vec<TextHit>,
    /// Matching lines in all files, kept or not.
    pub total: usize,
    /// The walk stopped at `max_files`, so later files were not searched.
    pub truncated: bool,
    /// Files skipped for not being valid UTF-8.
    pub not_utf8: usize,
    /// Files skipped for exceeding the size limit.
    pub oversized: usize,
}

/// Lines matching `re` in text files under `dir`, one hit per line, sorted by
/// path and line. Binary, non-UTF-8 and oversized files are skipped. Only the
/// first `limit` hits are kept and annotated; the rest are just counted.
pub fn search_text(
    root: &Path,
    dir: &Path,
    cache: &ParseCache,
    re: &Regex,
    opts: &TextOptions,
) -> TextHits {
    let (files, truncated) = files_under(root, dir, opts.max_files);
    let total = AtomicUsize::new(0);
    let not_utf8 = AtomicUsize::new(0);
    let oversized = AtomicUsize::new(0);
    let mut found = par_flat_map(&files, |rel| {
        let source = match read_text(&root.join(rel)) {
            Ok(source) => source,
            Err(skipped) => {
                match skipped {
                    Unread::NotUtf8 => not_utf8.fetch_add(1, Ordering::Relaxed),
                    Unread::Oversized => oversized.fetch_add(1, Ordering::Relaxed),
                    Unread::Other => 0,
                };
                return Vec::new();
            }
        };

        // One hit per line: the first match on it. No file can hold more than
        // `limit` of the hits kept overall, so the others need not be stored.
        // Their text waits until the kept hits are known: one short line per
        // hit over thousands of files adds up.
        let path: Arc<str> = slash_path(rel).into();
        let mut kept: Vec<(Arc<str>, usize, usize)> = Vec::new();
        let mut matched = 0;
        let mut last_line = 0;
        let mut line = 1;
        let mut counted = 0;
        for m in re.find_iter(&source) {
            line += source[counted..m.start()].matches('\n').count();
            counted = m.start();
            if last_line != line {
                last_line = line;
                matched += 1;
                if kept.len() < opts.limit {
                    kept.push((Arc::clone(&path), line, m.start()));
                }
            }
        }
        total.fetch_add(matched, Ordering::Relaxed);
        kept
    });
    found.truncate(opts.limit);

    // Text and symbols only for the hits returned, so the walk stays cheap.
    let mut source = None;
    let mut parsed = None;
    let mut current = None;
    let hits = found
        .into_iter()
        .filter_map(|(path, line, byte)| {
            if current.as_ref() != Some(&path) {
                let abs = root.join(&*path);
                source = read_text(&abs).ok();
                parsed = Lang::from_path(&abs).and_then(|_| cache.get(&abs).ok());
                current = Some(Arc::clone(&path));
            }
            // Not valid here means the file changed since the search read it.
            let text = source.as_deref().filter(|s| s.is_char_boundary(byte))?;
            Some(TextHit {
                path: path.to_string(),
                line,
                text: line_excerpt(text, byte),
                enclosing: parsed
                    .as_ref()
                    .and_then(|f| enclosing_symbol(&f.parsed.symbols, byte))
                    .map(|s| s.qualified_name()),
                context: context_lines(text, byte, line, opts.context),
            })
        })
        .collect();
    TextHits {
        hits,
        total: total.into_inner(),
        truncated,
        not_utf8: not_utf8.into_inner(),
        oversized: oversized.into_inner(),
    }
}

/// The lines from `n` before the one holding `byte` (number `line`) to `n`
/// after it, without their line breaks and cut to [`MAX_EXCERPT_CHARS`].
fn context_lines(source: &str, byte: usize, line: usize, n: usize) -> Vec<(usize, String)> {
    if n == 0 {
        return Vec::new();
    }
    let start = source[..byte].rfind('\n').map_or(0, |i| i + 1);
    let mut first = start;
    let mut first_line = line;
    while first_line > 1 && line - first_line < n {
        first = source[..first - 1].rfind('\n').map_or(0, |i| i + 1);
        first_line -= 1;
    }
    source[first..]
        .split_inclusive('\n')
        .take(line - first_line + n + 1)
        .enumerate()
        .map(|(i, text)| {
            let text = text.trim_end_matches(['\n', '\r']);
            let text = match text.char_indices().nth(MAX_EXCERPT_CHARS) {
                Some((cut, _)) => format!("{}…", &text[..cut]),
                None => text.to_string(),
            };
            (first_line + i, text)
        })
        .collect()
}

/// Why a file was left out of a search. Binary files are not worth reporting,
/// so they count as `Other`, like files that cannot be read.
pub(super) enum Unread {
    NotUtf8,
    Oversized,
    Other,
}

pub(super) fn read_text(path: &Path) -> Result<String, Unread> {
    let meta = std::fs::metadata(path).map_err(|_| Unread::Other)?;
    if !meta.is_file() {
        return Err(Unread::Other);
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(Unread::Oversized);
    }
    let bytes = std::fs::read(path).map_err(|_| Unread::Other)?;
    if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
        return Err(Unread::Other);
    }
    String::from_utf8(bytes).map_err(|_| Unread::NotUtf8)
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
            .hits
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
        assert_eq!(hits.hits[2].text, "// TODO inside TODO");

        let only_src = search_text(
            root,
            &root.join("src"),
            &cache,
            &re,
            &TextOptions::default(),
        );
        assert_eq!(only_src.total, 2);
    }

    #[test]
    fn limit_keeps_the_first_hits_and_counts_all() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn x() {\n    // hit\n    // hit\n}\n").unwrap();
        fs::write(root.join("b.rs"), "// hit\n").unwrap();

        let opts = TextOptions {
            limit: 2,
            ..Default::default()
        };
        let cache = ParseCache::default();
        let found = search_text(root, root, &cache, &Regex::new("hit").unwrap(), &opts);
        assert_eq!(found.total, 3);
        let kept: Vec<_> = found
            .hits
            .iter()
            .map(|h| (h.path.as_str(), h.line, h.enclosing.as_deref()))
            .collect();
        assert_eq!(kept, [("a.rs", 2, Some("x")), ("a.rs", 3, Some("x"))]);
        // The file past the limit was not parsed.
        assert_eq!(cache.stats().parses, 1);
    }

    #[test]
    fn context_stops_at_the_ends_of_the_file() {
        let src = "one\r\ntwo\r\nthree";
        assert_eq!(
            context_lines(src, 0, 1, 3),
            [(1, "one".into()), (2, "two".into()), (3, "three".into())]
        );
        assert_eq!(
            context_lines(src, src.len() - 2, 3, 1),
            [(2, "two".into()), (3, "three".into())]
        );
        assert!(context_lines(src, 0, 1, 0).is_empty());
    }
}
