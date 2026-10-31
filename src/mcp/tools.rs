//! MCP tool definitions.
//!
//! [`Workspace`] holds the synchronous, directly testable logic; [`CodeScope`]
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

use crate::ast::parser::{self, SymbolKind};
use crate::ast::pruner::{self, PruneOptions};
use crate::git;
use crate::index::cache::{ParseCache, PersistentStore};
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
                 CODE_SCOPE_MAX_FILES\n",
                self.max_files
            )
        } else {
            String::new()
        }
    }

    /// Persist parse results across restarts in `store`.
    pub fn with_store(mut self, store: PersistentStore) -> Self {
        self.cache = ParseCache::default().with_store(store);
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

    fn display<'a>(&self, path: &'a Path) -> std::borrow::Cow<'a, str> {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
    }

    pub fn outline(&self, rel: &str, max_depth: Option<usize>) -> Result<String> {
        let path = self.resolve(rel)?;
        let file = self.cache.get(&path)?;
        let parsed = &file.parsed;
        let mut out = format!(
            "{} ({}, {} lines, {} symbols{})\n",
            self.display(&path),
            parsed.lang.name(),
            file.source.lines().count(),
            parsed.symbols.len(),
            if parsed.has_errors {
                ", has syntax errors"
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
        let note = self.truncation_note(hits.truncated);
        if hits.is_empty() {
            return format!("no symbols matching {query:?}\n{note}");
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
            let _ = writeln!(
                out,
                "// {}:{}-{} [{}] {}",
                self.display(&path),
                sym.start_line,
                sym.end_line,
                sym.kind.as_str(),
                sym.qualified_name()
            );
            // Start at the beginning of the line so indentation is preserved.
            let line_start = file.source[..sym.start_byte]
                .rfind('\n')
                .map_or(0, |i| i + 1);
            for (i, line) in file.source[line_start..sym.end_byte].lines().enumerate() {
                let _ = writeln!(out, "{}\t{line}", sym.start_line + i);
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
    /// same lines `read_symbol` shows (from the start of the first line,
    /// indentation included; empty deletes the symbol). `line` picks one of
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
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", self.display(&path)))?;
        let parsed = parser::parse_path(&path, &source)?;
        let name = name.trim();
        let matches: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.name == name || s.qualified_name() == name)
            .filter(|s| line.is_none_or(|l| s.start_line == l))
            .collect();
        let sym = match matches.as_slice() {
            [one] => *one,
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
        };

        let line_start = source[..sym.start_byte].rfind('\n').map_or(0, |i| i + 1);
        let replacement = new_source.strip_suffix('\n').unwrap_or(new_source);
        let replacement = replacement.strip_suffix('\r').unwrap_or(replacement);
        let updated = format!(
            "{}{replacement}{}",
            &source[..line_start],
            &source[sym.end_byte..]
        );
        let reparsed = parser::parse_path(&path, &updated)?;
        if reparsed.has_errors && !parsed.has_errors {
            bail!(
                "not written: the new source of {} has syntax errors",
                sym.qualified_name()
            );
        }
        std::fs::write(&path, &updated)
            .with_context(|| format!("cannot write {}", self.display(&path)))?;
        self.cache.invalidate(&path);

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
            sym.start_line,
            sym.end_line,
            new_lines,
            sym.start_line
        ))
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

    /// Resolve an optional sub-directory, defaulting to the root.
    fn resolve_dir(&self, dir: Option<&str>) -> Result<PathBuf> {
        let base = match dir {
            Some(d) if !d.trim().is_empty() => self.resolve(d)?,
            _ => self.root.clone(),
        };
        if !base.is_dir() {
            bail!("not a directory: {}", self.display(&base));
        }
        Ok(base)
    }

    pub fn find_references(
        &self,
        name: &str,
        dir: Option<&str>,
        include_definitions: bool,
        limit: usize,
    ) -> Result<String> {
        let base = self.resolve_dir(dir)?;
        let opts = RefOptions {
            include_definitions,
            max_files: self.max_files,
        };
        let hits = refs::find_references(&self.root, &base, &self.cache, name, &opts);
        let note = self.truncation_note(hits.truncated);
        if hits.is_empty() {
            return Ok(format!("no references to {name:?}\n{note}"));
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
        let base = self.resolve_dir(dir)?;
        let source = if regex {
            pattern.to_string()
        } else {
            regex::escape(pattern)
        };
        let re = regex::RegexBuilder::new(&source)
            .case_insensitive(case_insensitive)
            .multi_line(true)
            .size_limit(MAX_REGEX_BYTES)
            .build()
            .with_context(|| format!("invalid pattern {pattern:?}"))?;
        let opts = TextOptions {
            max_files: self.max_files,
        };
        let hits = text::search_text(&self.root, &base, &self.cache, &re, &opts);
        let note = self.truncation_note(hits.truncated);
        if hits.is_empty() {
            return Ok(format!("no matches for {pattern:?}\n{note}"));
        }
        let mut out = String::new();
        for h in hits.iter().take(limit) {
            let _ = write!(out, "{}:{}: {}", h.path, h.line, h.text);
            if let Some(sym) = &h.enclosing {
                let _ = write!(out, "  [in {sym}]");
            }
            out.push('\n');
        }
        if hits.len() > limit {
            let _ = writeln!(out, "… {} more matching lines", hits.len() - limit);
        }
        out.push_str(&note);
        Ok(out)
    }

    pub fn list_files(&self, dir: Option<&str>, limit: usize) -> Result<String> {
        let base = self.resolve_dir(dir)?;
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
pub struct CodeScope {
    workspace: Arc<Workspace>,
    tool_router: ToolRouter<Self>,
}

impl CodeScope {
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
impl CodeScope {
    pub fn new(workspace: Workspace) -> Self {
        CodeScope {
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
        description = "Search file contents for a literal string or regex, respecting .gitignore. Returns path:line, the line and the enclosing symbol. Use instead of grep."
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

    #[tool(description = "Show git working-tree status (modified, staged, untracked files).")]
    async fn git_status(&self) -> Result<String, String> {
        self.run(move |ws| ws.git_status()).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CodeScope {
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
                 use replace_symbol instead of reading and editing the file.",
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
            "pub struct Cfg;\n\nimpl Cfg {\n\n}\n"
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
    fn searches_report_the_file_limit() {
        let (dir, ws) = workspace();
        fs::write(
            dir.path().join("src/main.rs"),
            "fn main() { Cfg::load(); }\n",
        )
        .unwrap();
        let note =
            "… searched only the first 1 files; narrow with `dir` or raise CODE_SCOPE_MAX_FILES\n";
        assert!(!ws.search("load", None, 10).contains("searched only"));

        let ws = ws.with_max_files(1);
        assert!(ws.search("load", None, 10).ends_with(note));
        let refs = ws.find_references("load", None, false, 10).unwrap();
        assert!(
            refs.starts_with("no references") && refs.ends_with(note),
            "{refs}"
        );
        assert!(
            ws.search_text("Cfg", false, false, None, 10)
                .unwrap()
                .ends_with(note)
        );
    }

    #[test]
    fn rejects_paths_outside_root() {
        let (_dir, ws) = workspace();
        let err = ws.outline("../../../../etc/passwd", None).unwrap_err();
        assert!(err.to_string().contains("escapes the workspace root"));
    }
}
