//! AST token pruning: shrink a file's source to fit a token budget while
//! keeping as much structure as possible.
//!
//! Strategies are tried in order, stopping at the first that fits:
//! 1. `Full` — the source as-is.
//! 2. `ElidedBodies` — function/method bodies replaced by placeholders,
//!    largest first, only as many as needed.
//! 3. `Outline` — one line per symbol (signature + line range).
//! 4. `TopLevelOutline` — outline restricted to top-level symbols.
//! 5. `Truncated` — top-level outline cut off at the budget.

use serde::Serialize;

use super::parser::{Lang, Symbol};

/// Rough token estimate (~4 bytes per token for source code).
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Full,
    ElidedBodies,
    Outline,
    TopLevelOutline,
    Truncated,
}

#[derive(Debug, Clone, Default)]
pub struct PruneOptions {
    pub max_tokens: usize,
    /// Symbols (plain or qualified names) whose bodies must never be elided.
    pub keep: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PruneResult {
    pub text: String,
    pub estimated_tokens: usize,
    pub strategy: Strategy,
    /// Number of bodies elided (strategy `ElidedBodies`).
    pub elided_bodies: usize,
}

impl PruneResult {
    fn new(text: String, strategy: Strategy, elided_bodies: usize) -> Self {
        PruneResult {
            estimated_tokens: estimate_tokens(&text),
            text,
            strategy,
            elided_bodies,
        }
    }
}

pub fn prune(lang: Lang, source: &str, symbols: &[Symbol], opts: &PruneOptions) -> PruneResult {
    let budget = opts.max_tokens;
    if estimate_tokens(source) <= budget {
        return PruneResult::new(source.to_string(), Strategy::Full, 0);
    }

    if let Some(result) = elide_bodies(lang, source, symbols, opts) {
        return result;
    }

    let outline = render_outline(symbols, usize::MAX);
    if estimate_tokens(&outline) <= budget {
        return PruneResult::new(outline, Strategy::Outline, 0);
    }

    let top = render_outline(symbols, 0);
    if estimate_tokens(&top) <= budget {
        return PruneResult::new(top, Strategy::TopLevelOutline, 0);
    }

    PruneResult::new(truncate_lines(&top, budget), Strategy::Truncated, 0)
}

struct Candidate {
    start: usize,
    end: usize,
    placeholder: String,
    saved: usize,
}

fn elide_bodies(
    lang: Lang,
    source: &str,
    symbols: &[Symbol],
    opts: &PruneOptions,
) -> Option<PruneResult> {
    let is_kept = |s: &Symbol| {
        opts.keep
            .iter()
            .any(|k| *k == s.name || *k == s.qualified_name())
    };

    // Outermost callable bodies only: anything nested inside a body (or a
    // kept symbol) disappears with it or must stay intact.
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut covered_until = 0usize;
    for sym in symbols {
        if sym.start_byte < covered_until {
            continue;
        }
        if is_kept(sym) {
            covered_until = sym.end_byte;
            continue;
        }
        if !sym.kind.is_callable() {
            continue;
        }
        let Some((start, end)) = sym.body else {
            continue;
        };
        covered_until = end;
        let body = &source[start..end];
        let placeholder = placeholder(lang, body);
        let saved = estimate_tokens(body).saturating_sub(estimate_tokens(&placeholder));
        if saved > 0 {
            candidates.push(Candidate {
                start,
                end,
                placeholder,
                saved,
            });
        }
    }

    let needed = estimate_tokens(source).saturating_sub(opts.max_tokens);
    let total: usize = candidates.iter().map(|c| c.saved).sum();
    if candidates.is_empty() || total < needed {
        return None;
    }

    // Elide the largest bodies first so small helpers stay readable.
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by(|&a, &b| candidates[b].saved.cmp(&candidates[a].saved));
    let mut chosen = vec![false; candidates.len()];
    let mut saved = 0;
    for i in order {
        if saved >= needed {
            break;
        }
        chosen[i] = true;
        saved += candidates[i].saved;
    }

    let mut out = String::with_capacity(source.len());
    let mut pos = 0;
    let mut elided = 0;
    for (c, _) in candidates.iter().zip(&chosen).filter(|(_, on)| **on) {
        out.push_str(&source[pos..c.start]);
        out.push_str(&c.placeholder);
        pos = c.end;
        elided += 1;
    }
    out.push_str(&source[pos..]);

    // The estimate is additive per byte range, but verify rather than trust.
    let result = PruneResult::new(out, Strategy::ElidedBodies, elided);
    (result.estimated_tokens <= opts.max_tokens).then_some(result)
}

fn placeholder(lang: Lang, body: &str) -> String {
    let lines = body.lines().count().max(1);
    match lang {
        Lang::Python => format!("...  # {lines} lines elided"),
        _ => format!("{{ /* {lines} lines elided */ }}"),
    }
}

/// One line per symbol, indented by nesting depth, including symbols up to
/// `max_depth` (0 = top-level only).
pub fn render_outline(symbols: &[Symbol], max_depth: usize) -> String {
    let mut out = String::new();
    for sym in symbols.iter().filter(|s| s.depth <= max_depth) {
        let indent = "  ".repeat(sym.depth);
        out.push_str(&format!(
            "{indent}{} [{}] L{}-{}\n",
            sym.signature,
            sym.kind.as_str(),
            sym.start_line,
            sym.end_line
        ));
    }
    out
}

const TRUNCATION_MARKER: &str = "… (truncated to fit token budget)\n";

fn truncate_lines(text: &str, budget: usize) -> String {
    let marker_tokens = estimate_tokens(TRUNCATION_MARKER);
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        if estimate_tokens(&out) + estimate_tokens(line) + marker_tokens > budget {
            break;
        }
        out.push_str(line);
    }
    if estimate_tokens(&out) + marker_tokens <= budget {
        out.push_str(TRUNCATION_MARKER);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parser::parse_source;

    const SRC: &str = r#"use std::fmt;

pub struct Big;

impl Big {
    pub fn large(&self) -> u32 {
        let a = 1;
        let b = 2;
        let c = 3;
        let d = 4;
        let e = 5;
        let f = 6;
        a + b + c + d + e + f
    }

    pub fn small(&self) -> u32 {
        let x = 10;
        let y = 20;
        x + y
    }
}

fn other() {
    println!("one");
    println!("two");
    println!("three");
}
"#;

    fn run(max_tokens: usize, keep: &[&str]) -> PruneResult {
        let parsed = parse_source(Lang::Rust, SRC).unwrap();
        let opts = PruneOptions {
            max_tokens,
            keep: keep.iter().map(|s| s.to_string()).collect(),
        };
        prune(Lang::Rust, SRC, &parsed.symbols, &opts)
    }

    #[test]
    fn fits_returns_full_source() {
        let r = run(10_000, &[]);
        assert_eq!(r.strategy, Strategy::Full);
        assert_eq!(r.text, SRC);
    }

    #[test]
    fn elides_largest_body_first() {
        let full = estimate_tokens(SRC);
        let r = run(full - 20, &[]);
        assert_eq!(r.strategy, Strategy::ElidedBodies);
        assert_eq!(r.elided_bodies, 1);
        assert!(
            r.text
                .contains("pub fn large(&self) -> u32 { /* 9 lines elided */ }")
        );
        assert!(r.text.contains("let y = 20;"), "small body kept");
        assert!(r.estimated_tokens <= full - 20);
    }

    #[test]
    fn keep_protects_named_symbol() {
        let full = estimate_tokens(SRC);
        let r = run(full - 20, &["Big::large"]);
        assert_eq!(r.strategy, Strategy::ElidedBodies);
        assert!(r.text.contains("let f = 6;"));
        assert!(!r.text.contains("let y = 20;"));
    }

    #[test]
    fn falls_back_to_outline() {
        let r = run(45, &[]);
        assert_eq!(r.strategy, Strategy::Outline);
        assert!(
            r.text
                .contains("  pub fn small(&self) -> u32 [method] L16-20")
        );
        assert!(r.estimated_tokens <= 45);
    }

    #[test]
    fn top_level_outline_then_truncation() {
        let r = run(30, &[]);
        assert_eq!(r.strategy, Strategy::TopLevelOutline);
        assert!(!r.text.contains("small"));

        let r = run(12, &[]);
        assert_eq!(r.strategy, Strategy::Truncated);
        assert!(r.text.ends_with(TRUNCATION_MARKER));
        assert!(r.estimated_tokens <= 12);
    }

    #[test]
    fn never_exceeds_budget() {
        for budget in 0..=estimate_tokens(SRC) + 5 {
            let r = run(budget, &[]);
            assert!(
                r.estimated_tokens <= budget,
                "budget {budget}: got {} via {:?}",
                r.estimated_tokens,
                r.strategy
            );
        }
    }

    #[test]
    fn python_placeholder() {
        let src = "def f():\n    a = 1\n    b = 2\n    c = 3\n    return a + b + c\n";
        let parsed = parse_source(Lang::Python, src).unwrap();
        let opts = PruneOptions {
            max_tokens: estimate_tokens(src) - 5,
            keep: vec![],
        };
        let r = prune(Lang::Python, src, &parsed.symbols, &opts);
        assert_eq!(r.strategy, Strategy::ElidedBodies);
        assert_eq!(r.text, "def f():\n    ...  # 4 lines elided\n");
    }
}
