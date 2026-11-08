//! Project-wide symbol search on top of the lazy parse cache.

pub mod cache;
pub mod changes;
pub mod map;
pub mod mentions;
pub mod names;
pub mod refs;
pub mod store_format;
pub mod text;

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as SeqOrdering};

use serde::Serialize;

use crate::ast::parser::{Lang, Symbol, SymbolKind};
use crate::git;
use cache::ParseCache;
use names::Query;

/// `path` with `/` separators on every platform, so tool output and the
/// paths a client sends back look the same on Windows.
pub fn slash_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' {
        return text.into_owned();
    }
    // `canonicalize` returns verbatim paths (\\?\C:\x, \\?\UNC\host\share\x),
    // whose prefix stops meaning anything once its separators change.
    let text = match text.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(unc) => format!(r"\\{unc}"),
            None => rest.to_owned(),
        },
        None => text.into_owned(),
    };
    text.replace(std::path::MAIN_SEPARATOR, "/")
}

/// Longest line excerpt returned by reference and text search.
const MAX_EXCERPT_CHARS: usize = 200;
/// Default upper bound on files walked, to keep huge repositories responsive.
pub const DEFAULT_MAX_FILES: usize = 20_000;

/// Hits of a workspace-wide search. `truncated` is set when the walk stopped
/// at `max_files`, so files past the limit were not searched.
#[derive(Debug, Clone)]
pub struct Found<T> {
    pub hits: Vec<T>,
    pub truncated: bool,
    /// Files whose symbol query ran out of time, so hits may be missing.
    pub incomplete: usize,
}

impl<T> Deref for Found<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.hits
    }
}

/// Apply `f` to every file on all cores; results keep the order of `files`.
///
/// A whole tree on one core would waste the others, but small pieces would
/// spend more time spawning threads than working, so the work is split in
/// chunks of at least [`MIN_FILES_PER_WORKER`] files, and one thread per core
/// otherwise.
fn par_flat_map<T, F>(files: &[PathBuf], f: F) -> Vec<T>
where
    T: Send,
    F: Fn(&Path) -> Vec<T> + Sync,
{
    par_groups(files, f).into_iter().flatten().collect()
}

/// Files a worker must get before another worker is worth spawning.
const MIN_FILES_PER_WORKER: usize = 32;

fn par_groups<T, F>(files: &[PathBuf], f: F) -> Vec<Vec<T>>
where
    T: Send,
    F: Fn(&Path) -> Vec<T> + Sync,
{
    par_ranges(files.len(), MIN_FILES_PER_WORKER, |range| {
        files[range].iter().map(|p| f(p)).collect()
    })
}

/// `f` over `0..len` in pieces of `chunk`, on all cores, as one result per
/// index in index order. Workers take the next piece from a shared counter,
/// so a slow piece holds up only its own worker. With a single piece the
/// work runs on the calling thread.
pub(crate) fn par_ranges<T, F>(len: usize, chunk: usize, f: F) -> Vec<T>
where
    T: Send,
    F: Fn(std::ops::Range<usize>) -> Vec<T> + Sync,
{
    let chunk = chunk.max(1);
    let batches = len.div_ceil(chunk);
    if batches <= 1 {
        return if len == 0 { Vec::new() } else { f(0..len) };
    }
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let workers = cores.min(batches);
    let next = AtomicUsize::new(0);
    let mut done: Vec<(usize, Vec<T>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let (f, next) = (&f, &next);
                s.spawn(move || {
                    let mut mine = Vec::new();
                    loop {
                        let batch = next.fetch_add(1, SeqOrdering::Relaxed);
                        let start = batch * chunk;
                        if start >= len {
                            break mine;
                        }
                        mine.push((batch, f(start..len.min(start + chunk))));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            // A panic becomes a tool error, not hits missing without a word.
            .flat_map(|h| {
                h.join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    done.sort_unstable_by_key(|(batch, _)| *batch);
    done.into_iter().flat_map(|(_, results)| results).collect()
}

/// Innermost symbol whose byte range contains `byte`.
fn enclosing_symbol(symbols: &[Symbol], byte: usize) -> Option<&Symbol> {
    symbols
        .iter()
        .filter(|s| s.start_byte <= byte && byte < s.end_byte)
        .max_by_key(|s| s.depth)
}

/// The trimmed line containing `byte`, cut to [`MAX_EXCERPT_CHARS`].
pub(crate) fn line_excerpt(source: &str, byte: usize) -> String {
    let start = source[..byte].rfind('\n').map_or(0, |i| i + 1);
    let end = source[byte..].find('\n').map_or(source.len(), |i| byte + i);
    let line = source[start..end].trim();
    match line.char_indices().nth(MAX_EXCERPT_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

/// Up to `max_files` files under `dir`, and whether more files were left out.
/// They are relative to `root` when `dir` is inside it, and absolute when
/// not, so `root.join` gives each file's path either way. A `dir` that is a
/// file gives that file alone, even when it is hidden or ignored.
fn files_under(root: &Path, dir: &Path, max_files: usize) -> (Vec<PathBuf>, bool) {
    let prefix = dir.strip_prefix(root).unwrap_or(dir);
    if dir.is_file() {
        return (vec![prefix.to_path_buf()], false);
    }
    let mut files = git::list_files(dir, max_files.saturating_add(1));
    let truncated = files.len() > max_files;
    files.truncate(max_files);
    let files = files.into_iter().map(|p| prefix.join(p)).collect();
    (files, truncated)
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolHit {
    pub path: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub signature: String,
    pub start_line: usize,
    pub end_line: usize,
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
            max_files: DEFAULT_MAX_FILES,
        }
    }
}

/// A match before its details are looked up: only the best `limit` of them
/// get a signature and end line, from the parsed file.
struct Candidate {
    score: u8,
    file: usize,
    start_line: usize,
    index: usize,
}

/// Search symbol names across all supported files under `root`. Files are
/// parsed lazily through `cache`, in parallel, on first search; later
/// searches score the compact names it keeps and read back only the files
/// of the hits they return.
pub fn search_symbols(
    root: &Path,
    cache: &ParseCache,
    query: &str,
    opts: &SearchOptions,
) -> Found<SymbolHit> {
    let query = query.trim();
    if query.is_empty() {
        return Found {
            hits: Vec::new(),
            truncated: false,
            incomplete: 0,
        };
    }
    let query = Query::new(query);
    let (files, truncated) = files_under(root, root, opts.max_files);
    let files: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| Lang::from_path(p).is_some())
        .collect();

    let incomplete = AtomicUsize::new(0);
    let per_file: Vec<Vec<(u8, usize, usize)>> = par_groups(&files, |rel| {
        // The cache is keyed by absolute paths: a relative one would miss
        // and parse the file again.
        let names = match cache.symbol_names(&root.join(rel)) {
            Ok(n) => n,
            Err(err) => {
                tracing::debug!(%err, "skipping file in search");
                return Vec::new();
            }
        };
        if names.incomplete() {
            incomplete.fetch_add(1, SeqOrdering::Relaxed);
        }
        names
            .matches(&query, opts.kind)
            .map(|(index, score, line)| (score, line, index))
            .collect()
    });

    // Paths only of the files with a match, to rank by.
    let mut paths: Vec<Option<String>> = vec![None; files.len()];
    let mut candidates = Vec::new();
    for (file, matches) in per_file.into_iter().enumerate() {
        if matches.is_empty() {
            continue;
        }
        paths[file] = Some(slash_path(&files[file]));
        candidates.extend(
            matches
                .into_iter()
                .map(|(score, start_line, index)| Candidate {
                    score,
                    file,
                    start_line,
                    index,
                }),
        );
    }
    let rank = |a: &Candidate, b: &Candidate| {
        a.score
            .cmp(&b.score)
            .then_with(|| paths[a.file].cmp(&paths[b.file]))
            .then(a.start_line.cmp(&b.start_line))
    };
    // The best `limit` are picked in linear time and only those are sorted.
    if candidates.len() > opts.limit {
        if opts.limit > 0 {
            candidates.select_nth_unstable_by(opts.limit - 1, rank);
        }
        candidates.truncate(opts.limit);
    }
    candidates.sort_by(rank);

    let mut hits = Vec::with_capacity(candidates.len());
    let mut current: Option<(usize, std::sync::Arc<cache::CachedFile>)> = None;
    for c in &candidates {
        if current.as_ref().is_none_or(|(file, _)| *file != c.file) {
            current = cache
                .get(&root.join(&files[c.file]))
                .ok()
                .map(|f| (c.file, f));
        }
        // A file that changed since its names were read may no longer hold
        // the symbol at that index; it is left out rather than misreported.
        let Some(sym) = current
            .as_ref()
            .filter(|(file, _)| *file == c.file)
            .and_then(|(_, f)| f.parsed.symbols.get(c.index))
            .filter(|s| s.start_line == c.start_line)
        else {
            continue;
        };
        hits.push(SymbolHit {
            path: paths[c.file].clone().unwrap_or_default(),
            name: sym.name.clone(),
            qualified_name: sym.qualified_name(),
            kind: sym.kind,
            signature: sym.signature.clone(),
            start_line: sym.start_line,
            end_line: sym.end_line,
        });
    }
    Found {
        hits,
        truncated,
        incomplete: incomplete.into_inner(),
    }
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
    fn the_limit_keeps_the_best_hits_not_the_first_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Files sort before `z.rs`, but only `z.rs` holds the exact name.
        for (name, code) in [
            ("a.rs", "fn search_all() {}\n"),
            ("b.py", "def unsearch():\n    pass\n"),
            ("z.rs", "fn search() {}\n"),
        ] {
            fs::write(root.join(name), code).unwrap();
        }
        let cache = ParseCache::default();
        let opts = SearchOptions {
            limit: 1,
            ..Default::default()
        };
        let hits = search_symbols(root, &cache, "search", &opts);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "search");
        assert_eq!(hits[0].path, "z.rs");

        let opts = SearchOptions {
            limit: 2,
            ..Default::default()
        };
        let hits = search_symbols(root, &cache, "search", &opts);
        let names: Vec<_> = hits.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["search", "search_all"]);

        let opts = SearchOptions {
            limit: 0,
            ..Default::default()
        };
        assert!(search_symbols(root, &cache, "search", &opts).is_empty());
    }

    #[test]
    fn parallel_work_keeps_the_order_of_the_files() {
        let many: Vec<PathBuf> = (0..MIN_FILES_PER_WORKER * 2)
            .map(|i| PathBuf::from(format!("{i}.rs")))
            .collect();
        let ordered = par_flat_map(&many, |p| vec![p.display().to_string()]);
        let expected: Vec<String> = many.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(ordered, expected);
    }

    #[test]
    #[should_panic(expected = "bad file")]
    fn a_panic_in_a_worker_reaches_the_caller() {
        let files = [PathBuf::from("a"), PathBuf::from("b")];
        par_flat_map::<(), _>(&files, |_| panic!("bad file"));
    }

    #[test]
    fn empty_query_returns_nothing() {
        let dir = fixture();
        let cache = ParseCache::default();
        assert!(search_symbols(dir.path(), &cache, "  ", &SearchOptions::default()).is_empty());
    }
}
