//! Benchmark report over real codebases.
//!
//! ```sh
//! BENCH_CORPORA="go=/usr/lib/go/src,python=/usr/lib/python3.14" \
//! BENCH_OUT=BENCHMARKS.md cargo bench --bench report
//! ```
//!
//! Without `BENCH_CORPORA` it measures this crate's own `src/`. Token counts
//! use the server's estimator (~4 bytes per token), applied identically to
//! the full text and to the reduced output.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use code_scope_mcp::ast::parser::Lang;
use code_scope_mcp::ast::pruner::{self, PruneOptions, estimate_tokens};
use code_scope_mcp::git;
use code_scope_mcp::index::cache::{ParseCache, PersistentStore};
use code_scope_mcp::index::refs::{self, RefOptions};
use code_scope_mcp::index::text::{self, TextOptions};
use code_scope_mcp::index::{self, SearchOptions};
use regex::Regex;

const MAX_FILES: usize = 50_000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const PRUNE_BUDGET: usize = 2_000;
const QUERIES: [&str; 6] = ["new", "parse", "read", "error", "close", "test"];
/// Regex for `search_text` besides the per-query literals.
const TEXT_REGEX: &str = r"TODO|FIXME|XXX";
const WARM_ROUNDS: usize = 5;
const STARTUP_ROUNDS: usize = 10;

fn median(mut xs: Vec<f64>) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(f64::total_cmp);
    xs[xs.len() / 2]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn pct_saved(full: usize, reduced: usize) -> f64 {
    if full == 0 {
        0.0
    } else {
        100.0 * (1.0 - reduced as f64 / full as f64)
    }
}

struct CorpusReport {
    name: String,
    files: usize,
    bytes: u64,
    symbols: usize,
    skipped: usize,
    parse_1t: Duration,
    search_cold: Duration,
    search_warm_ms: f64,
    search_restart: Duration,
    restart_parses: u64,
    restart_store_hits: u64,
    full_tokens: usize,
    outline_tokens: usize,
    large_files: usize,
    large_full_tokens: usize,
    large_pruned_tokens: usize,
    symbol_ratio_median: f64,
    refs_ms_median: f64,
    /// Distinct lines holding a reference, summed over `QUERIES`.
    refs_lines: usize,
    /// Lines with a whole-word text match, summed over `QUERIES`.
    word_lines: usize,
    text_ms_median: f64,
    text_regex_ms: f64,
}

fn bench_corpus(name: &str, root: &Path) -> CorpusReport {
    let files: Vec<PathBuf> = git::list_files(root, MAX_FILES)
        .into_iter()
        .filter(|p| Lang::from_path(p).is_some())
        .collect();

    // 1. Single-threaded cold parse of every file.
    let cache = ParseCache::new(usize::MAX, MAX_FILE_BYTES);
    let start = Instant::now();
    let mut parsed = Vec::new();
    let mut skipped = 0;
    for rel in &files {
        match cache.get(&root.join(rel)) {
            Ok(file) => parsed.push(file),
            Err(_) => skipped += 1,
        }
    }
    let parse_1t = start.elapsed();

    // 2. Token accounting on the parsed files.
    let mut bytes = 0u64;
    let mut symbols = 0;
    let (mut full_tokens, mut outline_tokens) = (0, 0);
    let (mut large_files, mut large_full, mut large_pruned) = (0, 0, 0);
    let mut symbol_ratios = Vec::new();
    for file in &parsed {
        let syms = &file.parsed.symbols;
        bytes += file.source.len() as u64;
        symbols += syms.len();
        let full = estimate_tokens(&file.source);
        full_tokens += full;
        outline_tokens += estimate_tokens(&pruner::render_outline(syms, usize::MAX));
        if full > PRUNE_BUDGET {
            let opts = PruneOptions {
                max_tokens: PRUNE_BUDGET,
                keep: vec![],
            };
            let r = pruner::prune(file.parsed.lang, &file.source, syms, &opts);
            large_files += 1;
            large_full += full;
            large_pruned += r.estimated_tokens;
        }
        for s in syms.iter().filter(|s| s.kind.is_callable()) {
            let t = estimate_tokens(&file.source[s.start_byte..s.end_byte]);
            symbol_ratios.push(t as f64 / full.max(1) as f64);
        }
    }
    drop(parsed);
    drop(cache);

    // 3. Project-wide search: cold (parallel parse), warm, after restart.
    let store_dir = tempfile::tempdir().expect("tempdir");
    let opts = SearchOptions {
        max_files: MAX_FILES,
        ..Default::default()
    };
    let open_store = || PersistentStore::open(store_dir.path()).expect("open store");

    let cache = ParseCache::new(usize::MAX, MAX_FILE_BYTES).with_store(open_store());
    let start = Instant::now();
    index::search_symbols(root, &cache, QUERIES[0], &opts);
    let search_cold = start.elapsed();
    cache.flush();

    let mut warm = Vec::new();
    for _ in 0..WARM_ROUNDS {
        for q in QUERIES {
            let start = Instant::now();
            index::search_symbols(root, &cache, q, &opts);
            warm.push(ms(start.elapsed()));
        }
    }
    drop(cache);

    let cache = ParseCache::new(usize::MAX, MAX_FILE_BYTES).with_store(open_store());
    let start = Instant::now();
    index::search_symbols(root, &cache, QUERIES[0], &opts);
    let search_restart = start.elapsed();
    let stats = cache.stats();

    // 4. find_references vs whole-word text search, on the warm cache.
    let ref_opts = RefOptions {
        include_definitions: true,
        max_files: MAX_FILES,
    };
    let text_opts = TextOptions {
        max_files: MAX_FILES,
    };
    let (mut refs_times, mut text_times) = (Vec::new(), Vec::new());
    let (mut refs_lines, mut word_lines) = (0, 0);
    for q in QUERIES {
        let start = Instant::now();
        let hits = refs::find_references(root, root, &cache, q, &ref_opts);
        refs_times.push(ms(start.elapsed()));
        let mut lines: Vec<_> = hits.iter().map(|h| (&h.path, h.line)).collect();
        lines.dedup();
        refs_lines += lines.len();

        let re = Regex::new(&format!(r"\b{q}\b")).expect("word regex");
        let start = Instant::now();
        word_lines += text::search_text(root, root, &cache, &re, &text_opts).len();
        text_times.push(ms(start.elapsed()));
    }
    let re = Regex::new(TEXT_REGEX).expect("text regex");
    let start = Instant::now();
    text::search_text(root, root, &cache, &re, &text_opts);
    let text_regex_ms = ms(start.elapsed());

    CorpusReport {
        name: name.to_string(),
        files: files.len(),
        bytes,
        symbols,
        skipped,
        parse_1t,
        search_cold,
        search_warm_ms: median(warm),
        search_restart,
        restart_parses: stats.parses,
        restart_store_hits: stats.store_hits,
        full_tokens,
        outline_tokens,
        large_files,
        large_full_tokens: large_full,
        large_pruned_tokens: large_pruned,
        symbol_ratio_median: median(symbol_ratios),
        refs_ms_median: median(refs_times),
        refs_lines,
        word_lines,
        text_ms_median: median(text_times),
        text_regex_ms,
    }
}

/// Time from spawning the server to receiving its `initialize` response.
fn startup_ms(root: &Path) -> f64 {
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"bench","version":"0"}}}"#;
    let mut samples = Vec::new();
    for _ in 0..STARTUP_ROUNDS {
        let start = Instant::now();
        let mut child = Command::new(env!("CARGO_BIN_EXE_code-scope-mcp"))
            .arg(root)
            .env("CODE_SCOPE_NO_CACHE", "1")
            .env("CODE_SCOPE_LOG", "off")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn server");
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, "{request}").expect("write request");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("read response");
        samples.push(ms(start.elapsed()));
        assert!(line.contains("\"serverInfo\""), "bad response: {line}");
        drop(stdin);
        let _ = child.wait();
    }
    median(samples)
}

fn corpora() -> Vec<(String, PathBuf)> {
    match std::env::var("BENCH_CORPORA") {
        Ok(spec) if !spec.trim().is_empty() => spec
            .split(',')
            .filter_map(|item| {
                let (name, path) = item.split_once('=')?;
                Some((name.trim().to_string(), PathBuf::from(path.trim())))
            })
            .collect(),
        _ => vec![(
            "code-scope-mcp".into(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
        )],
    }
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find_map(|l| l.strip_prefix("model name"))
                .map(|l| l.trim_start_matches([' ', '\t', ':']).to_string())
        })
        .unwrap_or_else(|| "unknown CPU".into())
}

fn render(reports: &[CorpusReport], startup: f64) -> String {
    let mut out = String::new();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let cpu = cpu_model();
    out.push_str("# Benchmarks\n\n");
    out.push_str(&format!(
        "Generated by `cargo bench --bench report` (release profile) on {cpu}, {threads} threads, \
         {os}, with source files in the OS page cache.\n\
         Tokens are estimated at ~4 bytes/token for both the full text and the reduced output.\n\n",
        os = std::env::consts::OS,
    ));

    out.push_str("## Throughput & latency\n\n");
    out.push_str("| Corpus | Files | Size | Symbols | Cold parse (1 thread) | Search: cold | Search: warm (median) | Search: after restart |\n");
    out.push_str("|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for r in reports {
        let mb = r.bytes as f64 / 1_048_576.0;
        let secs = r.parse_1t.as_secs_f64();
        out.push_str(&format!(
            "| {} | {} | {:.1} MB | {} | {:.0} ms ({:.0} files/s, {:.1} MB/s) | {:.1} ms | {:.1} ms | {:.1} ms ({} from store, {} re-parsed) |\n",
            r.name,
            r.files,
            mb,
            r.symbols,
            ms(r.parse_1t),
            (r.files - r.skipped) as f64 / secs,
            mb / secs,
            ms(r.search_cold),
            r.search_warm_ms,
            ms(r.search_restart),
            r.restart_store_hits,
            r.restart_parses,
        ));
    }

    out.push_str("\n## Token savings\n\n");
    out.push_str(&format!(
        "| Corpus | Outline vs full source | Files > {PRUNE_BUDGET} tokens | Pruned to budget vs full | Median `read_symbol` / whole file |\n"
    ));
    out.push_str("|---|---:|---:|---:|---:|\n");
    for r in reports {
        out.push_str(&format!(
            "| {} | {:.1}% fewer ({} → {} tokens) | {} | {:.1}% fewer | {:.1}% |\n",
            r.name,
            pct_saved(r.full_tokens, r.outline_tokens),
            r.full_tokens,
            r.outline_tokens,
            r.large_files,
            pct_saved(r.large_full_tokens, r.large_pruned_tokens),
            r.symbol_ratio_median * 100.0,
        ));
    }

    out.push_str("\n## References & text search\n\n");
    out.push_str(&format!(
        "Queries: {}. `search_text` runs the whole-word regex `\\bQUERY\\b`, like `grep -w`.\n\n",
        QUERIES.map(|q| format!("`{q}`")).join(", ")
    ));
    out.push_str("| Corpus | `find_references` (median) | Lines with a reference | Lines with a word match | Word matches dropped | `search_text` word (median) | `search_text` regex |\n");
    out.push_str("|---|---:|---:|---:|---:|---:|---:|\n");
    for r in reports {
        out.push_str(&format!(
            "| {} | {:.1} ms | {} | {} | {:.1}% | {:.1} ms | {:.1} ms |\n",
            r.name,
            r.refs_ms_median,
            r.refs_lines,
            r.word_lines,
            pct_saved(r.word_lines, r.refs_lines),
            r.text_ms_median,
            r.text_regex_ms,
        ));
    }

    out.push_str(&format!(
        "\n## Startup\n\nSpawn → `initialize` response: **{startup:.1} ms** (median of {STARTUP_ROUNDS}; no indexing at startup).\n"
    ));
    out.push_str(&format!(
        "\n### Columns\n\n\
         - **Cold parse**: read and tree-sitter-parse every supported file on one thread.\n\
         - **Search: cold**: first `search_symbols` on an empty cache (walks the tree, parses in parallel, persists results).\n\
         - **Search: warm**: later searches served from the in-memory cache (still walks and `stat`s every file).\n\
         - **Search: after restart**: new process state with the LMDB store; files are validated by git blob hash instead of re-parsed.\n\
         - **Pruned to budget**: `read_file_pruned` output for files that exceed the budget.\n\
         - **`read_symbol` / whole file**: tokens for one function/method versus reading its entire file.\n\
         - **`find_references`**: identifier tokens named QUERY, definitions included; looked up in the identifier offsets recorded at parse time (warm cache).\n\
         - **Word matches dropped**: whole-word text matches that are not references (comments, strings, docs, non-source files).\n\
         - **`search_text` regex**: one run of `{TEXT_REGEX}` over every text file.\n",
    ));
    out
}

fn main() {
    // `cargo bench` passes `--bench`; ignore harness arguments.
    let mut reports = Vec::new();
    for (name, path) in corpora() {
        eprintln!("benchmarking {name} at {}", path.display());
        reports.push(bench_corpus(&name, &path));
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let startup = startup_ms(&root);
    let report = render(&reports, startup);
    print!("{report}");
    if let Ok(out) = std::env::var("BENCH_OUT") {
        std::fs::write(&out, &report).expect("write report");
        eprintln!("wrote {out}");
    }
}
