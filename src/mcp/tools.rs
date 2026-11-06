//! MCP tool definitions.
//!
//! [`Workspace`] holds the synchronous, directly testable logic; [`AstLens`]
//! exposes it as `#[tool]` functions and runs each call on a blocking thread
//! so file I/O and parsing never stall the async runtime.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::ast::parser::{self, ParsedFile, Symbol, SymbolKind, line_number, symbol_start};
use crate::ast::pruner::{self, PruneOptions};
use crate::atomic::write_atomic;
use crate::git;
use crate::index::cache::{ParseCache, PersistentStore};
use crate::index::changes::{self, Change, DiffSpec};
use crate::index::refs::{self, RefOptions};
use crate::index::text::{self, TextOptions};
use crate::index::{self, SearchOptions};

const DEFAULT_MAX_TOKENS: usize = 4000;
const DEFAULT_SEARCH_LIMIT: usize = 50;
const DEFAULT_LIST_LIMIT: usize = 500;
const DEFAULT_HIT_LIMIT: usize = 100;
/// Compiled-regex size cap, so a pathological pattern fails fast.
const MAX_REGEX_BYTES: usize = 1 << 20;
const MAX_SYMBOL_MATCHES: usize = 5;
/// Says what an empty result of a symbol search did not cover.
const UNSEARCHED_NOTE: &str = "… not searched: hidden files, non-UTF-8 files, files over 2 MB\n";

pub struct Workspace {
    root: PathBuf,
    cache: ParseCache,
    max_files: usize,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("invalid workspace root {}", root.display()))?;
        Ok(Workspace {
            root,
            cache: ParseCache::default(),
            max_files: index::DEFAULT_MAX_FILES,
        })
    }

    /// Walk at most `max_files` files per workspace-wide search.
    pub fn with_max_files(mut self, max_files: usize) -> Self {
        self.max_files = max_files.max(1);
        self
    }

    /// Note for output when a search stopped at the file limit.
    fn truncation_note(&self, truncated: bool) -> String {
        if truncated {
            format!(
                "… searched only the first {} files; narrow with `dir` or raise \
                 ASTLENS_MAX_FILES\n",
                self.max_files
            )
        } else {
            String::new()
        }
    }

    /// Persist parse results across restarts in `store`.
    pub fn with_store(mut self, store: PersistentStore) -> Self {
        self.cache = std::mem::take(&mut self.cache).with_store(store);
        self
    }

    /// Keep parsed files within about `max_bytes` of memory.
    pub fn with_cache_bytes(mut self, max_bytes: usize) -> Self {
        self.cache = std::mem::take(&mut self.cache).with_max_bytes(max_bytes);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a user-supplied path, refusing anything outside the root.
    fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let joined = self.root.join(rel.trim());
        let path = joined
            .canonicalize()
            .with_context(|| format!("no such path: {rel}"))?;
        if !path.starts_with(&self.root) {
            bail!("path escapes the workspace root: {rel}");
        }
        Ok(path)
    }

    fn display(&self, path: &Path) -> String {
        index::slash_path(path.strip_prefix(&self.root).unwrap_or(path))
    }

    pub fn outline(&self, rel: &str, max_depth: Option<usize>) -> Result<String> {
        let path = self.resolve(rel)?;
        let file = self.cache.get(&path)?;
        let parsed = &file.parsed;
        let mut out = format!(
            "{} ({}, {} lines, {} symbols{}{})\n",
            self.display(&path),
            parsed.lang.name(),
            file.source.lines().count(),
            parsed.symbols.len(),
            if parsed.has_errors {
                ", has syntax errors"
            } else {
                ""
            },
            if parsed.incomplete {
                ", symbol search timed out, list incomplete"
            } else {
                ""
            }
        );
        out.push_str(&pruner::render_outline(
            &parsed.symbols,
            max_depth.unwrap_or(usize::MAX),
        ));
        Ok(out)
    }

    pub fn search(&self, query: &str, kind: Option<SymbolKind>, limit: usize) -> String {
        let opts = SearchOptions {
            kind,
            limit,
            max_files: self.max_files,
        };
        let hits = index::search_symbols(&self.root, &self.cache, query, &opts);
        let mut note = self.truncation_note(hits.truncated);
        if hits.incomplete > 0 {
            let _ = writeln!(
                note,
                "… symbol search timed out, some symbols are missing (files: {})",
                hits.incomplete
            );
        }
        if hits.is_empty() {
            return format!("no symbols matching {query:?}\n{note}{UNSEARCHED_NOTE}");
        }
        let mut out = String::new();
        for h in hits.iter() {
            let _ = writeln!(
                out,
                "{}:{}-{} [{}] {} — {}",
                h.path,
                h.start_line,
                h.end_line,
                h.kind.as_str(),
                h.qualified_name,
                h.signature
            );
        }
        out.push_str(&note);
        out
    }

    pub fn read_symbol(&self, rel: &str, name: &str) -> Result<String> {
        let path = self.resolve(rel)?;
        let file = self.cache.get(&path)?;
        let name = name.trim();
        let matches: Vec<_> = file
            .parsed
            .symbols
            .iter()
            .filter(|s| s.name == name || s.qualified_name() == name)
            .collect();
        if matches.is_empty() {
            let mut known: Vec<_> = file
                .parsed
                .symbols
                .iter()
                .map(|s| s.qualified_name())
                .collect();
            known.truncate(30);
            return Err(anyhow!(
                "symbol {name:?} not found in {rel}; available: {}",
                known.join(", ")
            ));
        }
        let mut out = String::new();
        for sym in matches.iter().take(MAX_SYMBOL_MATCHES) {
            let start = symbol_start(&file.parsed, &file.source, sym);
            let first_line = line_number(&file.source, start);
            let _ = writeln!(
                out,
                "// {}:{}-{} [{}] {}",
                self.display(&path),
                first_line,
                sym.end_line,
                sym.kind.as_str(),
                sym.qualified_name()
            );
            for (i, line) in file.source[start..sym.end_byte].lines().enumerate() {
                let _ = writeln!(out, "{}\t{line}", first_line + i);
            }
        }
        if matches.len() > MAX_SYMBOL_MATCHES {
            let _ = writeln!(
                out,
                "… {} more matches; use a qualified name to narrow down",
                matches.len() - MAX_SYMBOL_MATCHES
            );
        }
        Ok(out)
    }

    /// Replace the source of one symbol with `new_source`, which covers the
    /// same lines `read_symbol` shows (from the start of the first line, with
    /// doc comments, attributes and decorators directly above and indentation
    /// included; empty deletes the symbol). Other code sharing those lines is
    /// left alone. `line` picks one of
    /// several symbols sharing the name. The file is re-read rather than
    /// taken from the cache, and the write is refused if it would introduce
    /// syntax errors.
    pub fn replace_symbol(
        &self,
        rel: &str,
        name: &str,
        line: Option<usize>,
        new_source: &str,
    ) -> Result<String> {
        let path = self.resolve(rel)?;
        let (source, parsed) = self.read_fresh(&path)?;
        let sym = unique_symbol(&parsed, rel, name, line)?;

        let start = symbol_start(&parsed, &source, sym);
        let first_line = line_number(&source, start);
        let replacement = with_line_ending(
            new_source.trim_end_matches(['\n', '\r']),
            line_ending(&source),
        );
        let updated = if replacement.is_empty() {
            without_span(&source, start, sym.end_byte)
        } else {
            format!(
                "{}{replacement}{}",
                &source[..start],
                &source[sym.end_byte..]
            )
        };
        self.write_checked(&path, &parsed, &updated, || {
            format!("the new source of {}", sym.qualified_name())
        })?;

        let new_lines = if replacement.is_empty() {
            0
        } else {
            replacement.lines().count()
        };
        Ok(format!(
            "replaced {} [{}] in {}: lines {}-{} are now {} lines starting at line {}\n",
            sym.qualified_name(),
            sym.kind.as_str(),
            self.display(&path),
            first_line,
            sym.end_line,
            new_lines,
            first_line
        ))
    }

    /// Insert `new_source` as a new definition after or before an existing
    /// symbol, separated by a blank line, or at the end of the file when no
    /// anchor is given. Inserting before a symbol keeps its doc comments,
    /// attributes and decorators attached to it. Like `replace_symbol`, the
    /// write is refused if it would introduce syntax errors.
    pub fn insert_symbol(
        &self,
        rel: &str,
        new_source: &str,
        anchor: Option<Anchor<'_>>,
        line: Option<usize>,
    ) -> Result<String> {
        let path = self.resolve(rel)?;
        let (source, parsed) = self.read_fresh(&path)?;
        let body = new_source
            .trim_start_matches(['\n', '\r'])
            .trim_end_matches(['\n', '\r']);
        if body.trim().is_empty() {
            bail!("new_source must not be empty");
        }

        let (pos, text, place) = match anchor {
            Some(Anchor::After(name)) => {
                let sym = unique_symbol(&parsed, rel, name, line)?;
                // A symbol that follows, or the closing of its parent, keeps
                // the rest of the line from being part of this symbol.
                let line_end = source[sym.end_byte..].find('\n').map(|i| sym.end_byte + i);
                let until = line_end.unwrap_or(source.len());
                let code_follows = parsed.symbols.iter().any(|s| {
                    (s.start_byte >= sym.end_byte && s.start_byte < until)
                        || (s.start_byte < sym.start_byte
                            && s.end_byte > sym.end_byte
                            && s.end_byte <= until)
                });
                let (pos, text) = match line_end {
                    _ if code_follows => (sym.end_byte, format!("\n{body}\n")),
                    Some(end) => (end + 1, format!("\n{body}\n")),
                    None => (source.len(), format!("\n\n{body}\n")),
                };
                (pos, text, format!("after {}", sym.qualified_name()))
            }
            Some(Anchor::Before(name)) => {
                let sym = unique_symbol(&parsed, rel, name, line)?;
                let pos = symbol_start(&parsed, &source, sym);
                // Other code precedes the symbol on its line: start a new one.
                let sep = if pos == 0 || source[..pos].ends_with('\n') {
                    ""
                } else {
                    "\n"
                };
                (
                    pos,
                    format!("{sep}{body}\n\n"),
                    format!("before {}", sym.qualified_name()),
                )
            }
            None => {
                let sep = match source.as_str() {
                    "" => "",
                    s if s.ends_with('\n') => "\n",
                    _ => "\n\n",
                };
                (
                    source.len(),
                    format!("{sep}{body}\n"),
                    "at the end".to_string(),
                )
            }
        };
        let text = with_line_ending(&text, line_ending(&source));
        let updated = format!("{}{text}{}", &source[..pos], &source[pos..]);
        self.write_checked(&path, &parsed, &updated, || {
            "the inserted source".to_string()
        })?;

        // Leading newlines of `text` are separators, not inserted code.
        let code = text.trim_start_matches(['\r', '\n']);
        let blank = text[..text.len() - code.len()].matches('\n').count();
        let first_line = line_number(&source, pos) + blank;
        Ok(format!(
            "inserted {} lines {place} in {}, starting at line {first_line}\n",
            body.lines().count(),
            self.display(&path),
        ))
    }

    /// Read and parse a file from disk, bypassing the cache, before a write.
    fn read_fresh(&self, path: &Path) -> Result<(String, ParsedFile)> {
        if !path.is_file() {
            bail!("{} is not a regular file", self.display(path));
        }
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", self.display(path)))?;
        let parsed = parser::parse_path(path, &source)?;
        Ok((source, parsed))
    }

    /// Write `updated` unless it has syntax errors the original did not.
    fn write_checked(
        &self,
        path: &Path,
        original: &ParsedFile,
        updated: &str,
        what: impl FnOnce() -> String,
    ) -> Result<()> {
        let reparsed = parser::parse_path(path, updated)?;
        if reparsed.has_errors && !original.has_errors {
            bail!("not written: {} has syntax errors", what());
        }
        write_atomic(path, updated)
            .with_context(|| format!("cannot write {}", self.display(path)))?;
        self.cache.invalidate(path);
        Ok(())
    }

    pub fn read_pruned(&self, rel: &str, max_tokens: usize, keep: Vec<String>) -> Result<String> {
        let path = self.resolve(rel)?;
        let file = self.cache.get(&path)?;
        let opts = PruneOptions { max_tokens, keep };
        let result = pruner::prune(file.parsed.lang, &file.source, &file.parsed.symbols, &opts);
        let mut out = format!(
            "// {} | strategy={} | ~{}/{} tokens",
            self.display(&path),
            serde_json::to_value(result.strategy)?
                .as_str()
                .unwrap_or_default(),
            result.estimated_tokens,
            max_tokens
        );
        if result.elided_bodies > 0 {
            let _ = write!(out, " | {} bodies elided", result.elided_bodies);
        }
        out.push('\n');
        out.push_str(&result.text);
        Ok(out)
    }

    /// Resolve an optional sub-directory, defaulting to the root. It comes
    /// back relative to the root, as the file walkers expect: an absolute path
    /// makes their results absolute too, and the cache, which is keyed by
    /// `root.join(rel)`, then misses every file.
    fn resolve_dir(&self, dir: Option<&str>) -> Result<PathBuf> {
        let base = match dir {
            Some(d) if !d.trim().is_empty() => self.resolve(d)?,
            _ => self.root.clone(),
        };
        if !base.is_dir() {
            bail!("not a directory: {}", self.display(&base));
        }
        Ok(base
            .strip_prefix(&self.root)
            .map(Path::to_path_buf)
            .unwrap_or_default())
    }

    pub fn find_references(
        &self,
        name: &str,
        dir: Option<&str>,
        include_definitions: bool,
        limit: usize,
    ) -> Result<String> {
        let base = self.root.join(self.resolve_dir(dir)?);
        let opts = RefOptions {
            include_definitions,
            max_files: self.max_files,
        };
        let hits = refs::find_references(&self.root, &base, &self.cache, name, &opts);
        let note = self.truncation_note(hits.truncated);
        if hits.is_empty() {
            return Ok(format!(
                "no references to {name:?}\n{note}{UNSEARCHED_NOTE}"
            ));
        }
        let mut out = String::new();
        for h in hits.iter().take(limit) {
            let _ = write!(out, "{}:{}:{}: {}", h.path, h.line, h.column + 1, h.text);
            if h.is_definition {
                out.push_str("  [definition]");
            }
            if let Some(sym) = &h.enclosing {
                let _ = write!(out, "  [in {sym}]");
            }
            out.push('\n');
        }
        if hits.len() > limit {
            let _ = writeln!(out, "… {} more references", hits.len() - limit);
        }
        out.push_str(&note);
        Ok(out)
    }

    pub fn search_text(
        &self,
        pattern: &str,
        regex: bool,
        case_insensitive: bool,
        dir: Option<&str>,
        limit: usize,
    ) -> Result<String> {
        if pattern.is_empty() {
            bail!("pattern must not be empty");
        }
        let base = self.root.join(self.resolve_dir(dir)?);
        let source = if regex {
            pattern.to_string()
        } else {
            regex::escape(pattern)
        };
        let re = regex::RegexBuilder::new(&source)
            .case_insensitive(case_insensitive)
            .multi_line(true)
            .crlf(true)
            .size_limit(MAX_REGEX_BYTES)
            .build()
            .with_context(|| format!("invalid pattern {pattern:?}"))?;
        let opts = TextOptions {
            max_files: self.max_files,
            limit,
        };
        let found = text::search_text(&self.root, &base, &self.cache, &re, &opts);
        let mut note = self.truncation_note(found.truncated);
        if found.not_utf8 + found.oversized > 0 {
            let _ = writeln!(
                note,
                "… skipped {} not UTF-8 and {} over 2 MB files",
                found.not_utf8, found.oversized
            );
        }
        if found.total == 0 {
            return Ok(format!(
                "no matches for {pattern:?}\n{note}… hidden files were not searched; \
                 `dir` can name a hidden directory\n"
            ));
        }
        let mut out = String::new();
        for h in &found.hits {
            let _ = write!(out, "{}:{}: {}", h.path, h.line, h.text);
            if let Some(sym) = &h.enclosing {
                let _ = write!(out, "  [in {sym}]");
            }
            out.push('\n');
        }
        if found.total > found.hits.len() {
            let _ = writeln!(
                out,
                "… {} more matching lines",
                found.total - found.hits.len()
            );
        }
        out.push_str(&note);
        Ok(out)
    }

    pub fn list_files(&self, dir: Option<&str>, limit: usize) -> Result<String> {
        let base = self.root.join(self.resolve_dir(dir)?);
        let files = git::list_files(&base, limit);
        let mut out = String::new();
        for f in &files {
            let _ = writeln!(out, "{}", self.display(&base.join(f)));
        }
        if files.len() == limit {
            let _ = writeln!(out, "… limit of {limit} files reached");
        }
        if out.is_empty() {
            out.push_str("no files\n");
        }
        Ok(out)
    }

    /// Symbols added, modified or removed between `base` and the working
    /// tree (or the index when `staged`), grouped by file.
    pub fn changed_symbols(&self, base: &str, staged: bool) -> Result<String> {
        let base = base.trim();
        if base.is_empty() || base.starts_with('-') {
            bail!("invalid base revision {base:?}");
        }
        let files = changes::changed_symbols(&self.root, DiffSpec { base, staged })?;
        let side = if staged {
            "the index"
        } else {
            "the working tree"
        };
        if files.is_empty() {
            return Ok(format!("no changes between {base} and {side}\n"));
        }
        let mut out = String::new();
        for f in &files {
            if !f.parsed {
                let _ = writeln!(
                    out,
                    "{} (not parsed): {} changed lines",
                    f.path, f.other_lines
                );
                continue;
            }
            let _ = writeln!(out, "{}", f.path);
            for c in &f.symbols {
                let range = format!("L{}-{}", c.start_line, c.end_line);
                let range = if c.change == Change::Removed {
                    format!("(was {range})")
                } else {
                    range
                };
                let _ = writeln!(
                    out,
                    "  {:<8}  [{}] {}  {range}",
                    c.change.as_str(),
                    c.kind.as_str(),
                    c.qualified_name
                );
            }
            if f.other_lines > 0 {
                let _ = writeln!(out, "  {} changed lines outside symbols", f.other_lines);
            }
        }
        Ok(out)
    }

    pub fn git_status(&self) -> Result<String> {
        let entries = git::status(&self.root)?;
        if entries.is_empty() {
            return Ok("working tree clean\n".into());
        }
        let mut out = String::new();
        for e in entries {
            match e.renamed_from {
                Some(from) => {
                    let _ = writeln!(out, "{} {from} -> {}", e.code, e.path);
                }
                None => {
                    let _ = writeln!(out, "{} {}", e.code, e.path);
                }
            }
        }
        Ok(out)
    }
}

/// Where `insert_symbol` puts new code, relative to a named symbol.
#[derive(Debug, Clone, Copy)]
pub enum Anchor<'a> {
    After(&'a str),
    Before(&'a str),
}

/// The one symbol called `name` (plain or qualified), optionally starting at
/// `line`; errors name the candidates when there are several.
fn unique_symbol<'p>(
    parsed: &'p ParsedFile,
    rel: &str,
    name: &str,
    line: Option<usize>,
) -> Result<&'p Symbol> {
    let name = name.trim();
    let matches: Vec<_> = parsed
        .symbols
        .iter()
        .filter(|s| s.name == name || s.qualified_name() == name)
        .filter(|s| line.is_none_or(|l| s.start_line == l))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!(
            "symbol {name:?}{} not found in {rel}",
            line.map(|l| format!(" starting at line {l}"))
                .unwrap_or_default()
        ),
        many => {
            let at: Vec<_> = many
                .iter()
                .map(|s| format!("{} (line {})", s.qualified_name(), s.start_line))
                .collect();
            bail!(
                "{name:?} matches {} symbols: {}; pass `line` or a qualified name",
                many.len(),
                at.join(", ")
            )
        }
    }
}

/// `\r\n` when the file already uses it, so edits keep its line endings.
fn line_ending(source: &str) -> &'static str {
    if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn with_line_ending(text: &str, eol: &str) -> String {
    let text = text.replace("\r\n", "\n");
    if eol == "\n" {
        text
    } else {
        text.replace('\n', eol)
    }
}

/// `source` without the bytes `start..end`. When they were all that was on
/// their lines, the line break goes too, and so does one blank line if that
/// would leave blank lines on both sides or at the end of the file.
fn without_span(source: &str, start: usize, end: usize) -> String {
    let before = &source[..start];
    let mut after = &source[end..];
    if before.is_empty() || before.ends_with('\n') {
        if let Some(rest) = strip_line_break(after) {
            after = rest;
        }
        let blank_before =
            before.is_empty() || before.ends_with("\n\n") || before.ends_with("\n\r\n");
        if blank_before && (after.is_empty() || strip_line_break(after).is_some()) {
            return match (strip_line_break(after), before.is_empty()) {
                (Some(rest), _) => format!("{before}{rest}"),
                // Nothing follows, so the blank line before is the one to drop.
                (None, false) => before[..before.len() - line_break_len(before)].to_string(),
                (None, true) => String::new(),
            };
        }
    }
    format!("{before}{after}")
}

fn strip_line_break(text: &str) -> Option<&str> {
    text.strip_prefix("\r\n")
        .or_else(|| text.strip_prefix('\n'))
}

fn line_break_len(text: &str) -> usize {
    if text.ends_with("\r\n") { 2 } else { 1 }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OutlineParams {
    /// File path relative to the workspace root.
    pub path: String,
    /// Maximum nesting depth to include (0 = top-level only). Default: all.
    #[serde(default)]
    pub max_depth: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// Symbol name or fragment; case-insensitive, exact matches rank first.
    pub query: String,
    /// Restrict results to one symbol kind.
    #[serde(default)]
    pub kind: Option<SymbolKind>,
    /// Maximum number of results (default 50).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadSymbolParams {
    /// File path relative to the workspace root.
    pub path: String,
    /// Symbol name, plain (`new`) or qualified (`Parser::new`).
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReplaceSymbolParams {
    /// File path relative to the workspace root.
    pub path: String,
    /// Symbol name, plain (`new`) or qualified (`Parser::new`).
    pub name: String,
    /// Complete new source for the lines `read_symbol` shows, without line
    /// numbers and with indentation. Empty deletes the symbol.
    pub new_source: String,
    /// Start line of the symbol, to choose among several with the same name.
    #[serde(default)]
    pub line: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InsertSymbolParams {
    /// File path relative to the workspace root.
    pub path: String,
    /// Source of the new definition(s), with indentation, without line numbers.
    pub new_source: String,
    /// Insert after this symbol (plain or qualified name).
    #[serde(default)]
    pub after: Option<String>,
    /// Insert before this symbol, above its doc comments and attributes.
    #[serde(default)]
    pub before: Option<String>,
    /// Start line of the anchor symbol, to choose among several with the same name.
    #[serde(default)]
    pub line: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChangedSymbolsParams {
    /// Revision to compare against, e.g. `main` or `HEAD~3` (default `HEAD`).
    #[serde(default)]
    pub base: Option<String>,
    /// Compare the staged changes (the index) instead of the working tree.
    #[serde(default)]
    pub staged: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadPrunedParams {
    /// File path relative to the workspace root.
    pub path: String,
    /// Token budget for the returned text (default 4000).
    #[serde(default)]
    pub max_tokens: Option<usize>,
    /// Symbol names whose bodies must be kept intact.
    #[serde(default)]
    pub keep: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListFilesParams {
    /// Sub-directory relative to the workspace root. Default: the root.
    #[serde(default)]
    pub dir: Option<String>,
    /// Maximum number of files (default 500).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindReferencesParams {
    /// Symbol name, plain (`load`) or qualified (`Config::load`): the last segment is
    /// matched, and uses provably on another owner (`Other::load`) are dropped.
    pub name: String,
    /// Sub-directory relative to the workspace root to search in. Default: the root.
    #[serde(default)]
    pub dir: Option<String>,
    /// Also list the definitions themselves (default false).
    #[serde(default)]
    pub include_definitions: Option<bool>,
    /// Maximum number of results (default 100).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchTextParams {
    /// Text to find; a literal string unless `regex` is true.
    pub pattern: String,
    /// Treat `pattern` as a regular expression (Rust regex syntax). Default false.
    #[serde(default)]
    pub regex: Option<bool>,
    /// Ignore case (default false).
    #[serde(default)]
    pub case_insensitive: Option<bool>,
    /// Sub-directory relative to the workspace root to search in. Default: the root.
    #[serde(default)]
    pub dir: Option<String>,
    /// Maximum number of matching lines (default 100).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Clone)]
pub struct AstLens {
    workspace: Arc<Workspace>,
    tool_router: ToolRouter<Self>,
}

impl AstLens {
    /// Run `f` on the blocking pool, persist any new parse results, and
    /// flatten errors into tool error text.
    async fn run<F>(&self, f: F) -> Result<String, String>
    where
        F: FnOnce(&Workspace) -> Result<String> + Send + 'static,
    {
        let ws = Arc::clone(&self.workspace);
        let task = tokio::task::spawn_blocking(move || {
            let result = f(&ws);
            ws.cache.flush();
            tracing::debug!(stats = ?ws.cache.stats(), "tool call finished");
            result
        });
        match task.await {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(err)) => Err(format!("{err:#}")),
            Err(join) => Err(format!("internal error: {join}")),
        }
    }
}

#[tool_router]
impl AstLens {
    pub fn new(workspace: Workspace) -> Self {
        AstLens {
            workspace: Arc::new(workspace),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List the symbols (functions, classes, structs, ...) defined in a file with their signatures and line ranges. Much cheaper than reading the file."
    )]
    async fn get_file_outline(
        &self,
        Parameters(p): Parameters<OutlineParams>,
    ) -> Result<String, String> {
        self.run(move |ws| ws.outline(&p.path, p.max_depth)).await
    }

    #[tool(
        description = "Search symbol definitions by name across the workspace. Returns path:lines, kind, qualified name and signature."
    )]
    async fn search_symbols(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<String, String> {
        self.run(move |ws| Ok(ws.search(&p.query, p.kind, p.limit.unwrap_or(DEFAULT_SEARCH_LIMIT))))
            .await
    }

    #[tool(
        description = "Return the exact source of one symbol (function, class, ...) in a file, with line numbers."
    )]
    async fn read_symbol(
        &self,
        Parameters(p): Parameters<ReadSymbolParams>,
    ) -> Result<String, String> {
        self.run(move |ws| ws.read_symbol(&p.path, &p.name)).await
    }

    #[tool(
        description = "Replace the whole source of one symbol (function, class, ...) in a file, without reading the file first. new_source covers exactly the lines read_symbol shows; empty deletes the symbol. Refused if the result has syntax errors."
    )]
    async fn replace_symbol(
        &self,
        Parameters(p): Parameters<ReplaceSymbolParams>,
    ) -> Result<String, String> {
        self.run(move |ws| ws.replace_symbol(&p.path, &p.name, p.line, &p.new_source))
            .await
    }

    #[tool(
        description = "Insert a new function, class, method, ... into a file without reading it first: after or before a named symbol (a blank line apart), or at the end of the file when neither is given. Indent new_source for its place, e.g. inside a class. Refused if the result has syntax errors."
    )]
    async fn insert_symbol(
        &self,
        Parameters(p): Parameters<InsertSymbolParams>,
    ) -> Result<String, String> {
        self.run(move |ws| {
            let anchor = match (p.after.as_deref(), p.before.as_deref()) {
                (Some(_), Some(_)) => bail!("pass `after` or `before`, not both"),
                (Some(a), None) => Some(Anchor::After(a)),
                (None, Some(b)) => Some(Anchor::Before(b)),
                (None, None) => None,
            };
            ws.insert_symbol(&p.path, &p.new_source, anchor, p.line)
        })
        .await
    }

    #[tool(
        description = "Read a file shrunk to a token budget: the full text if it fits, otherwise function bodies are elided (largest first), falling back to an outline."
    )]
    async fn read_file_pruned(
        &self,
        Parameters(p): Parameters<ReadPrunedParams>,
    ) -> Result<String, String> {
        self.run(move |ws| {
            ws.read_pruned(&p.path, p.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS), p.keep)
        })
        .await
    }

    #[tool(
        description = "Find where a symbol is used across the workspace: identifier tokens in the syntax tree, so mentions in comments and strings are skipped. Returns path:line:col, the line and the enclosing symbol."
    )]
    async fn find_references(
        &self,
        Parameters(p): Parameters<FindReferencesParams>,
    ) -> Result<String, String> {
        self.run(move |ws| {
            ws.find_references(
                &p.name,
                p.dir.as_deref(),
                p.include_definitions.unwrap_or(false),
                p.limit.unwrap_or(DEFAULT_HIT_LIMIT),
            )
        })
        .await
    }

    #[tool(
        description = "Search file contents for a literal string or regex, respecting .gitignore and skipping hidden files unless `dir` names a hidden directory. Returns path:line, the line and the enclosing symbol. Use instead of grep."
    )]
    async fn search_text(
        &self,
        Parameters(p): Parameters<SearchTextParams>,
    ) -> Result<String, String> {
        self.run(move |ws| {
            ws.search_text(
                &p.pattern,
                p.regex.unwrap_or(false),
                p.case_insensitive.unwrap_or(false),
                p.dir.as_deref(),
                p.limit.unwrap_or(DEFAULT_HIT_LIMIT),
            )
        })
        .await
    }

    #[tool(description = "List files in the workspace, respecting .gitignore.")]
    async fn list_files(
        &self,
        Parameters(p): Parameters<ListFilesParams>,
    ) -> Result<String, String> {
        self.run(move |ws| ws.list_files(p.dir.as_deref(), p.limit.unwrap_or(DEFAULT_LIST_LIMIT)))
            .await
    }

    #[tool(
        description = "List the functions, classes, ... added, modified or removed by uncommitted changes (or since `base`, e.g. a branch), per file, instead of reading the raw diff. Untracked files count as added."
    )]
    async fn changed_symbols(
        &self,
        Parameters(p): Parameters<ChangedSymbolsParams>,
    ) -> Result<String, String> {
        self.run(move |ws| {
            ws.changed_symbols(
                p.base.as_deref().unwrap_or("HEAD"),
                p.staged.unwrap_or(false),
            )
        })
        .await
    }

    #[tool(description = "Show git working-tree status (modified, staged, untracked files).")]
    async fn git_status(&self) -> Result<String, String> {
        self.run(move |ws| ws.git_status()).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AstLens {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "AST-aware code navigation. Prefer get_file_outline and search_symbols to locate \
                 code, read_symbol to read exactly one definition, and read_file_pruned to read \
                 a whole file within a token budget. Use find_references to see where a symbol \
                 is used and search_text instead of grep. To rewrite a whole function or class, \
                 use replace_symbol, and insert_symbol to add one, instead of reading and \
                 editing the file. changed_symbols summarises a git diff by symbol.",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "pub struct Cfg;\n\nimpl Cfg {\n    pub fn load() -> Self {\n        Cfg\n    }\n}\n",
        )
        .unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, ws)
    }

    #[test]
    fn outline_lists_symbols() {
        let (_dir, ws) = workspace();
        let out = ws.outline("src/lib.rs", None).unwrap();
        assert!(out.starts_with("src/lib.rs (rust, 7 lines, 3 symbols)"));
        assert!(out.contains("  pub fn load() -> Self [method] L4-6"));
    }

    #[test]
    fn read_symbol_returns_numbered_source() {
        let (_dir, ws) = workspace();
        let out = ws.read_symbol("src/lib.rs", "Cfg::load").unwrap();
        assert!(out.contains("4\t    pub fn load() -> Self {"));
        assert!(out.contains("6\t    }"));
        let err = ws.read_symbol("src/lib.rs", "nope").unwrap_err();
        assert!(err.to_string().contains("available: Cfg"));
    }

    #[test]
    fn search_and_list() {
        let (_dir, ws) = workspace();
        let out = ws.search("load", None, 10);
        assert!(out.starts_with("src/lib.rs:4-6 [method] Cfg::load"));
        assert_eq!(ws.list_files(None, 10).unwrap(), "src/lib.rs\n");
    }

    #[test]
    fn pruned_read_reports_strategy() {
        let (_dir, ws) = workspace();
        let out = ws.read_pruned("src/lib.rs", 1000, vec![]).unwrap();
        assert!(out.starts_with("// src/lib.rs | strategy=full |"));
    }

    #[test]
    fn find_references_reports_uses() {
        let (dir, ws) = workspace();
        fs::write(
            dir.path().join("src/main.rs"),
            "fn main() {\n    Cfg::load();\n}\n",
        )
        .unwrap();
        let out = ws.find_references("Cfg::load", None, false, 10).unwrap();
        assert_eq!(out, "src/main.rs:2:10: Cfg::load();  [in main]\n");
        let out = ws.find_references("load", Some("src"), true, 1).unwrap();
        assert!(
            out.starts_with("src/lib.rs:4:12: pub fn load() -> Self {  [definition]  [in Cfg]\n")
        );
        assert!(out.ends_with("… 1 more references\n"));
        assert!(
            ws.find_references("nope", None, false, 10)
                .unwrap()
                .starts_with("no references")
        );
    }

    #[test]
    fn search_text_matches_line_ends_of_crlf_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.py"), "x = bar\r\ny = 1\r\n").unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let out = ws.search_text("bar$", true, false, None, 10).unwrap();
        assert_eq!(out, "a.py:1: x = bar\n");
        let out = ws.search_text("^y", true, false, None, 10).unwrap();
        assert_eq!(out, "a.py:2: y = 1\n");
    }

    #[test]
    fn empty_searches_say_what_they_did_not_cover() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".github")).unwrap();
        fs::write(dir.path().join(".github/h.py"), "def needle(): pass\n").unwrap();
        fs::write(dir.path().join("l.py"), b"# caf\xe9 needle\n").unwrap();
        let big = format!("# needle\n{}", "x".repeat(2 * 1024 * 1024 + 1));
        fs::write(dir.path().join("big.py"), big).unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = ws.search_text("needle", false, false, None, 10).unwrap();
        assert!(out.starts_with("no matches"), "{out}");
        assert!(
            out.contains("skipped 1 not UTF-8 and 1 over 2 MB files"),
            "{out}"
        );
        assert!(out.contains("hidden files were not searched"), "{out}");
        let out = ws.search("needle", None, 10);
        assert!(out.contains("not searched: hidden files"), "{out}");
        let out = ws.find_references("needle", None, false, 10).unwrap();
        assert!(out.contains("not searched: hidden files"), "{out}");

        let out = ws
            .search_text("needle", false, false, Some(".github"), 10)
            .unwrap();
        assert!(out.starts_with(".github/h.py:1:"), "{out}");
    }

    #[test]
    fn search_text_limit_counts_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.py"), "hit\nhit\nhit\n").unwrap();
        fs::write(dir.path().join("b.py"), "hit\nhit\n").unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        assert_eq!(
            ws.search_text("hit", false, false, None, 4).unwrap(),
            "a.py:1: hit\na.py:2: hit\na.py:3: hit\nb.py:1: hit\n… 1 more matching lines\n"
        );
        assert_eq!(
            ws.search_text("hit", false, false, None, 0).unwrap(),
            "… 5 more matching lines\n"
        );
    }

    #[test]
    fn searches_under_a_subdirectory_keep_cache_hits() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("src/a.rs"), "fn run() {\n    load\n}\n").unwrap();
        for sub in [None, Some("src"), Some("src/")] {
            let out = ws.search_text("load", false, false, sub, 10).unwrap();
            assert_eq!(
                out.lines().find(|l| l.starts_with("src/a.rs")),
                Some("src/a.rs:2: load  [in run]"),
                "dir = {sub:?}"
            );
            let out = ws.find_references("load", sub, false, 10).unwrap();
            assert_eq!(out, "src/a.rs:2:5: load  [in run]\n", "dir = {sub:?}");
            assert!(ws.list_files(sub, 10).unwrap().starts_with("src/a.rs\n"));
        }
        // Every path went through the cache that holds them.
        assert_eq!(ws.cache.stats().parses, 2);
    }

    #[test]
    fn a_subdirectory_search_reports_paths_relative_to_the_root() {
        let (dir, ws) = workspace();
        fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        fs::write(
            dir.path().join("src/deep/a.rs"),
            "fn run() {\n    load;\n}\n",
        )
        .unwrap();
        // Nested, and written both ways a client may spell it.
        for sub in ["src/deep", "src/deep/", "./src/deep"] {
            let text = ws.search_text("load", false, false, Some(sub), 10).unwrap();
            assert_eq!(text, "src/deep/a.rs:2: load;  [in run]\n", "dir = {sub}");
            let refs = ws.find_references("load", Some(sub), false, 10).unwrap();
            assert_eq!(refs, "src/deep/a.rs:2:5: load;  [in run]\n", "dir = {sub}");
            let listed = ws.list_files(Some(sub), 10).unwrap();
            assert_eq!(listed, "src/deep/a.rs\n", "dir = {sub}");
        }
        // Parsed once: neither search re-reads or re-parses it.
        assert_eq!(ws.cache.stats().parses, 1);
    }

    #[test]
    fn search_text_literal_and_regex() {
        let (_dir, ws) = workspace();
        let out = ws.search_text("Cfg", false, false, None, 10).unwrap();
        assert!(out.contains("src/lib.rs:5: Cfg  [in Cfg::load]\n"));
        let out = ws.search_text(r"fn \w+\(", true, false, None, 10).unwrap();
        assert_eq!(
            out,
            "src/lib.rs:4: pub fn load() -> Self {  [in Cfg::load]\n"
        );
        let out = ws.search_text("CFG;", false, true, None, 10).unwrap();
        assert_eq!(out, "src/lib.rs:1: pub struct Cfg;  [in Cfg]\n");
        assert!(ws.search_text("(", true, false, None, 10).is_err());
        assert!(ws.search_text("(", false, false, Some("nope"), 10).is_err());
    }

    #[test]
    fn replace_symbol_rewrites_one_definition() {
        let (dir, ws) = workspace();
        let file = dir.path().join("src/lib.rs");
        // Warm the cache so the rewrite must invalidate it.
        ws.outline("src/lib.rs", None).unwrap();
        let out = ws
            .replace_symbol(
                "src/lib.rs",
                "Cfg::load",
                None,
                "    pub fn load() -> Self {\n        let c = Cfg;\n        c\n    }\n",
            )
            .unwrap();
        assert_eq!(
            out,
            "replaced Cfg::load [method] in src/lib.rs: lines 4-6 are now 4 lines starting at line 4\n"
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "pub struct Cfg;\n\nimpl Cfg {\n    pub fn load() -> Self {\n        let c = Cfg;\n        c\n    }\n}\n"
        );
        assert!(ws.outline("src/lib.rs", None).unwrap().contains("L4-7"));

        let err = ws
            .replace_symbol("src/lib.rs", "load", None, "    pub fn load( {\n")
            .unwrap_err();
        assert!(err.to_string().contains("syntax errors"), "{err}");
        let err = ws
            .replace_symbol("src/lib.rs", "load", Some(1), "")
            .unwrap_err();
        assert!(err.to_string().contains("starting at line 1"), "{err}");

        ws.replace_symbol("src/lib.rs", "load", None, "").unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "pub struct Cfg;\n\nimpl Cfg {\n}\n"
        );
    }

    #[test]
    fn replace_symbol_needs_a_unique_match() {
        let (dir, ws) = workspace();
        fs::write(
            dir.path().join("src/two.py"),
            "class A:\n    def f(self):\n        pass\n\nclass B:\n    def f(self):\n        pass\n",
        )
        .unwrap();
        let err = ws.replace_symbol("src/two.py", "f", None, "").unwrap_err();
        assert!(err.to_string().contains("matches 2 symbols"), "{err}");
        ws.replace_symbol(
            "src/two.py",
            "f",
            Some(6),
            "    def g(self):\n        return 1\n",
        )
        .unwrap();
        ws.replace_symbol(
            "src/two.py",
            "A::f",
            None,
            "    def h(self):\n        pass\n",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("src/two.py")).unwrap(),
            "class A:\n    def h(self):\n        pass\n\nclass B:\n    def g(self):\n        return 1\n"
        );
    }

    #[test]
    fn insert_symbol_after_before_and_at_end() {
        let (dir, ws) = workspace();
        let file = dir.path().join("src/lib.rs");
        let out = ws
            .insert_symbol(
                "src/lib.rs",
                "    pub fn save(&self) {}\n",
                Some(Anchor::After("Cfg::load")),
                None,
            )
            .unwrap();
        assert_eq!(
            out,
            "inserted 1 lines after Cfg::load in src/lib.rs, starting at line 8\n"
        );
        ws.insert_symbol(
            "src/lib.rs",
            "/// Marker.\npub struct Before;",
            Some(Anchor::Before("Cfg")),
            Some(1), // `impl Cfg` shares the name
        )
        .unwrap();
        ws.insert_symbol("src/lib.rs", "fn tail() {}", None, None)
            .unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "/// Marker.\npub struct Before;\n\npub struct Cfg;\n\nimpl Cfg {\n    pub fn load() -> Self {\n        Cfg\n    }\n\n    pub fn save(&self) {}\n}\n\nfn tail() {}\n"
        );
        assert!(
            ws.outline("src/lib.rs", None)
                .unwrap()
                .contains("pub fn save(&self) [method] L11-11")
        );

        let err = ws
            .insert_symbol("src/lib.rs", "fn broken( {", None, None)
            .unwrap_err();
        assert!(err.to_string().contains("syntax errors"), "{err}");
        assert!(ws.insert_symbol("src/lib.rs", "\n", None, None).is_err());
        assert!(
            ws.insert_symbol("src/lib.rs", "fn x() {}", Some(Anchor::After("nope")), None)
                .is_err()
        );
    }

    #[test]
    fn insert_before_keeps_comments_and_decorators_attached() {
        let (dir, ws) = workspace();
        let path = dir.path().join("src/m.py");
        fs::write(
            &path,
            "import os\n\n# Loads things.\n@cache\ndef load():\n    pass\n",
        )
        .unwrap();
        ws.insert_symbol(
            "src/m.py",
            "def first():\n    pass\n",
            Some(Anchor::Before("load")),
            None,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "import os\n\ndef first():\n    pass\n\n# Loads things.\n@cache\ndef load():\n    pass\n"
        );
    }

    #[test]
    fn searches_report_the_file_limit() {
        let (dir, ws) = workspace();
        fs::write(
            dir.path().join("src/main.rs"),
            "fn main() { Cfg::load(); }\n",
        )
        .unwrap();
        let note =
            "… searched only the first 1 files; narrow with `dir` or raise ASTLENS_MAX_FILES\n";
        assert!(!ws.search("load", None, 10).contains("searched only"));

        let ws = ws.with_max_files(1);
        assert!(ws.search("load", None, 10).ends_with(note));
        let refs = ws.find_references("load", None, false, 10).unwrap();
        assert!(
            refs.starts_with("no references") && refs.contains(note),
            "{refs}"
        );
        assert!(
            ws.search_text("Cfg", false, false, None, 10)
                .unwrap()
                .ends_with(note)
        );
    }

    #[test]
    fn symbol_lines_include_doc_comments_and_attributes() {
        let (dir, ws) = workspace();
        let file = dir.path().join("src/attrs.rs");
        fs::write(
            &file,
            "#[inline] fn one() {}\n/// Two.\n#[derive(Debug)]\nstruct Two;\n\n// Three.\nfn three() {}\n",
        )
        .unwrap();
        // `one` is a one-line symbol with its attribute: it is not part of `Two`.
        assert_eq!(
            ws.read_symbol("src/attrs.rs", "Two").unwrap(),
            "// src/attrs.rs:2-4 [struct] Two\n2\t/// Two.\n3\t#[derive(Debug)]\n4\tstruct Two;\n"
        );
        let out = ws
            .replace_symbol(
                "src/attrs.rs",
                "Two",
                None,
                "#[derive(Clone)]\nstruct Two;\n",
            )
            .unwrap();
        assert!(out.contains("lines 2-4 are now 2 lines"), "{out}");
        ws.replace_symbol("src/attrs.rs", "three", None, "fn three() { one() }")
            .unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "#[inline] fn one() {}\n#[derive(Clone)]\nstruct Two;\n\nfn three() { one() }\n"
        );
    }

    /// (file, source, symbol, new source, expected file)
    const SAME_LINE_REPLACES: &[(&str, &str, &str, &str, &str)] = &[
        (
            "a.js",
            "const keep = 1; function b() { return 1; }\n",
            "b",
            "function b() { return 2; }",
            "const keep = 1; function b() { return 2; }\n",
        ),
        (
            "del.js",
            "const keep = 1; function b() { return 1; }\n",
            "b",
            "",
            "const keep = 1; \n",
        ),
        (
            "a.rs",
            "use std::fs; fn b() {}\nfn c() {}\n",
            "b",
            "fn b() { 1; }",
            "use std::fs; fn b() { 1; }\nfn c() {}\n",
        ),
        (
            "a.go",
            "package p; func b() {}\n",
            "b",
            "func b() { _ = 1 }",
            "package p; func b() { _ = 1 }\n",
        ),
        (
            "sib.rs",
            "fn a() {} fn b() {}\nfn c() {}\n",
            "b",
            "fn b2() {}",
            "fn a() {} fn b2() {}\nfn c() {}\n",
        ),
        (
            "min.js",
            "function a(){}function b(){}\n",
            "b",
            "function b2(){}",
            "function a(){}function b2(){}\n",
        ),
        (
            "A.java",
            "class J { void f() {} }\n",
            "f",
            "void f() { int a; }",
            "class J { void f() { int a; } }\n",
        ),
        (
            "m.rs",
            "impl P { fn m1(&self) {} fn m2(&self) {} }\n",
            "m1",
            "fn m1(&self) { 1; }",
            "impl P { fn m1(&self) { 1; } fn m2(&self) {} }\n",
        ),
    ];

    /// Prefixes the grammar keeps outside the definition: `read_symbol` shows
    /// them, and `new_source` replaces them. (file, source, symbol, shown)
    const ATTACHED_PREFIXES: &[(&str, &str, &str, &str)] = &[
        (
            "e1.js",
            "export function foo() {}\n",
            "foo",
            "export function foo() {}",
        ),
        (
            "e2.js",
            "export const bar = () => 2;\n",
            "bar",
            "export const bar = () => 2;",
        ),
        (
            "e3.js",
            "export default class Baz {}\n",
            "Baz",
            "export default class Baz {}",
        ),
        (
            "e4.ts",
            "export declare function f(): void;\n",
            "f",
            "export declare function f(): void;",
        ),
        (
            "d.ts",
            "class C {\n  @Get() m() {}\n}\n",
            "m",
            "  @Get() m() {}",
        ),
        (
            "t.go",
            "package p\n\ntype T struct{}\n",
            "T",
            "type T struct{}",
        ),
        (
            "t.cpp",
            "template <typename T> class C {};\n",
            "C",
            "template <typename T> class C {}",
        ),
        (
            "l.cpp",
            "extern \"C\" void f() {}\n",
            "f",
            "extern \"C\" void f() {}",
        ),
        ("at.rs", "#[inline] fn a() {}\n", "a", "#[inline] fn a() {}"),
    ];

    #[test]
    fn replace_keeps_other_code_on_the_line() {
        let dir = tempfile::tempdir().unwrap();
        for (file, source, _, _, _) in SAME_LINE_REPLACES {
            fs::write(dir.path().join(file), source).unwrap();
        }
        let ws = Workspace::new(dir.path()).unwrap();
        for (file, _, name, new_source, expected) in SAME_LINE_REPLACES {
            ws.replace_symbol(file, name, None, new_source)
                .unwrap_or_else(|e| panic!("{file}: {e}"));
            let got = fs::read_to_string(dir.path().join(file)).unwrap();
            assert_eq!(&got, expected, "{file}");
        }
    }

    #[test]
    fn read_symbol_includes_prefixes_of_the_declaration() {
        let dir = tempfile::tempdir().unwrap();
        for (file, source, _, _) in ATTACHED_PREFIXES {
            fs::write(dir.path().join(file), source).unwrap();
        }
        let ws = Workspace::new(dir.path()).unwrap();
        for (file, source, name, shown) in ATTACHED_PREFIXES {
            let out = ws.read_symbol(file, name).unwrap();
            assert!(out.contains(shown), "{file}: {out}");
            // What was read can be written back unchanged.
            ws.replace_symbol(file, name, None, shown)
                .unwrap_or_else(|e| panic!("{file}: {e}"));
            let got = fs::read_to_string(dir.path().join(file)).unwrap();
            assert_eq!(&got, source, "{file}");
        }
    }

    #[test]
    fn read_symbol_of_a_second_symbol_on_a_line() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "struct A; struct B;\n").unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let out = ws.read_symbol("a.rs", "B").unwrap();
        assert!(out.ends_with("1\tstruct B;\n"), "{out}");
    }

    #[test]
    fn insert_lands_next_to_the_anchor_on_a_shared_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        let ws = Workspace::new(dir.path()).unwrap();

        fs::write(&file, "struct A; struct B;\n").unwrap();
        let out = ws
            .insert_symbol("a.rs", "struct N;", Some(Anchor::Before("B")), None)
            .unwrap();
        assert!(out.contains("starting at line 2"), "{out}");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "struct A; \nstruct N;\n\nstruct B;\n"
        );

        fs::write(&file, "struct A; struct B;\n").unwrap();
        let out = ws
            .insert_symbol("a.rs", "struct N;", Some(Anchor::After("A")), None)
            .unwrap();
        assert!(out.contains("starting at line 2"), "{out}");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "struct A;\nstruct N;\n struct B;\n"
        );

        // Stays inside the parent that closes on the same line.
        fs::write(&file, "impl P { fn m(&self) {} }\n").unwrap();
        ws.insert_symbol("a.rs", "fn n(&self) {}", Some(Anchor::After("m")), None)
            .unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "impl P { fn m(&self) {}\nfn n(&self) {}\n }\n"
        );

        // A trailing comment stays with the anchor.
        fs::write(&file, "fn a() {} // note\nfn b() {}\n").unwrap();
        ws.insert_symbol("a.rs", "fn n() {}", Some(Anchor::After("a")), None)
            .unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "fn a() {} // note\n\nfn n() {}\nfn b() {}\n"
        );
    }

    #[test]
    fn deleting_a_symbol_leaves_no_blank_lines() {
        let cases = [
            (
                "fn a() {}\n\nfn b() {}\n\nfn c() {}\n",
                "b",
                "fn a() {}\n\nfn c() {}\n",
            ),
            ("fn a() {}\n\nfn b() {}\n", "b", "fn a() {}\n"),
            ("fn a() {}\n\nfn b() {}\n", "a", "fn b() {}\n"),
            (
                "fn a() {}\nfn b() {}\nfn c() {}\n",
                "b",
                "fn a() {}\nfn c() {}\n",
            ),
            (
                "fn a() {}\r\n\r\nfn b() {}\r\n\r\nfn c() {}\r\n",
                "b",
                "fn a() {}\r\n\r\nfn c() {}\r\n",
            ),
            ("fn a() {}\n", "a", ""),
            ("fn a() {} fn b() {}\n", "b", "fn a() {} \n"),
        ];
        let (dir, ws) = workspace();
        let file = dir.path().join("src/del.rs");
        for (source, name, expected) in cases {
            fs::write(&file, source).unwrap();
            ws.replace_symbol("src/del.rs", name, None, "").unwrap();
            assert_eq!(fs::read_to_string(&file).unwrap(), expected, "{source:?}");
        }
    }

    #[test]
    fn read_only_files_are_not_overwritten() {
        let (dir, ws) = workspace();
        let file = dir.path().join("src/ro.rs");
        fs::write(&file, "fn a() {}\n").unwrap();
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions).unwrap();
        let err = ws
            .replace_symbol("src/ro.rs", "a", None, "fn a() { 1; }\n")
            .unwrap_err();
        assert!(format!("{err:#}").contains("read-only"), "{err:#}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "fn a() {}\n");
    }

    #[cfg(unix)]
    #[test]
    fn fifos_are_refused_without_blocking() {
        let (dir, ws) = workspace();
        let status = std::process::Command::new("mkfifo")
            .arg(dir.path().join("src/pipe.rs"))
            .status()
            .unwrap();
        assert!(status.success());
        let err = ws.outline("src/pipe.rs", None).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
        let err = ws.replace_symbol("src/pipe.rs", "a", None, "").unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[test]
    fn edits_keep_crlf_line_endings() {
        let (dir, ws) = workspace();
        let file = dir.path().join("src/win.rs");
        fs::write(&file, "fn a() {\r\n}\r\n").unwrap();
        ws.replace_symbol("src/win.rs", "a", None, "fn a() {\n    b();\n}\n")
            .unwrap();
        ws.insert_symbol("src/win.rs", "fn b() {}\n", Some(Anchor::After("a")), None)
            .unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "fn a() {\r\n    b();\r\n}\r\n\r\nfn b() {}\r\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn edits_replace_the_file_atomically_and_keep_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, ws) = workspace();
        let file = dir.path().join("src/run.py");
        fs::write(&file, "def main():\n    pass\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        ws.replace_symbol("src/run.py", "main", None, "def main():\n    return 0\n")
            .unwrap();
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        let leftovers: Vec<_> = fs::read_dir(dir.path().join("src"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn changed_symbols_does_not_parse_huge_files() {
        let (dir, ws) = workspace();
        let root = dir.path();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        };
        fs::write(root.join("big.js"), "function a() {}\n").unwrap();
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "x"]);
        let big = format!("function a() {{}}\n{}", "// x\n".repeat(500_000));
        fs::write(root.join("big.js"), big).unwrap();
        let out = ws.changed_symbols("HEAD", false).unwrap();
        assert!(out.contains("big.js (not parsed)"), "{out}");
    }

    #[test]
    fn changed_symbols_groups_a_diff_by_symbol() {
        let (dir, ws) = workspace();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        assert_eq!(
            ws.changed_symbols("HEAD", false).unwrap(),
            "no changes between HEAD and the working tree\n"
        );

        ws.replace_symbol(
            "src/lib.rs",
            "Cfg::load",
            None,
            "    pub fn load() -> Self {\n        Self\n    }",
        )
        .unwrap();
        fs::write(dir.path().join("TODO.txt"), "x\n").unwrap();
        assert_eq!(
            ws.changed_symbols("HEAD", false).unwrap(),
            "TODO.txt (not parsed): 1 changed lines\nsrc/lib.rs\n  modified  [method] Cfg::load  L4-6\n"
        );
        assert!(ws.changed_symbols("--output=x", false).is_err());
    }

    #[test]
    fn rejects_paths_outside_root() {
        let (dir, ws) = workspace();
        // A real file next to the workspace, so the path resolves on every OS.
        let outside = tempfile::NamedTempFile::new_in(dir.path().parent().unwrap()).unwrap();
        let name = outside.path().file_name().unwrap().to_string_lossy();
        let err = ws.outline(&format!("../{name}"), None).unwrap_err();
        assert!(
            err.to_string().contains("escapes the workspace root"),
            "{err}"
        );
        let err = ws.outline("../no-such-file.rs", None).unwrap_err();
        assert!(err.to_string().contains("no such path"), "{err}");
    }
}
