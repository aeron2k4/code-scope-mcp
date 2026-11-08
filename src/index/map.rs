//! `codebase_map`: the symbols of a tree that the rest of its code relies on
//! most, as an outline that fits a token budget.
//!
//! Files and the names they define form a graph: a file that mentions a name
//! points at every other file defining it. Ranking the files by PageRank over
//! that graph puts the modules everything else builds on first, and the rank
//! that reaches a file through one of its names scores the symbols of that
//! name. The best symbols are then printed by file, each under the symbols
//! that enclose it, as many as fit `max_tokens`.
//!
//! Only names are compared, not resolved, but a mention counts for a
//! definition only where its language lets it reach that definition, and the
//! way the language needs it qualified. Names shared by many files are
//! weighted down, and long, specific names up, so the ranking follows the
//! names that say where they point.
//!
//! A tree with more source files than `max_files` is mapped from the
//! shallowest of them, which hold the core of most projects, rather than
//! from the first ones in path order.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::hash::{BuildHasherDefault, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::cache::{CachedFile, ParseCache};
use super::mentions::{self, Mentions};
use super::names::{Reach, SymbolNames};
use super::{DEFAULT_MAX_FILES, files_under, par_ranges, slash_path};
use crate::ast::parser::{Lang, Symbol, SymbolKind, is_ident_char};
use crate::ast::pruner::estimate_tokens;

/// Share of a file's rank that follows its references; the rest goes back
/// to the files picked by the personalization.
const DAMPING: f64 = 0.85;
const MAX_ITERATIONS: usize = 60;
/// Total change of the ranks, which sum to 1, below which they are final.
const TOLERANCE: f64 = 1e-10;
/// Share of the personalization that goes to the focus files, when any.
const FOCUS_SHARE: f64 = 0.9;
/// Names defined in more files than this are weighted down.
const COMMON_DEFINERS: usize = 5;
/// Score of a symbol nothing references, per unit of its file's rank: it
/// orders the symbols of files that are important for other names.
const UNREFERENCED: f64 = 0.01;
/// Score of test code's symbols, relative to the others'.
const TEST_WEIGHT: f64 = 0.1;
/// Score of a private symbol, a Go lower-case name, a Python `_name` or a
/// Rust item without `pub`, relative to the others': the map shows what
/// the rest of the code can use, and a helper only its own package sees
/// comes after it. Not C and C++, whose private header definitions are
/// what including files use.
const PRIVATE_WEIGHT: f64 = 0.4;
/// How fast repeated mentions of a name in one file stop counting more: the
/// BM25 `k1`.
const TERM_SATURATION: f64 = 1.2;
/// Idents or files per piece of parallel work in the rank iterations.
const RANK_CHUNK: usize = 4096;
/// Directories listed in the line about the files left out.
const FOOTER_GROUPS: usize = 10;
/// Fewest tokens a printed symbol line costs, to bound how many can fit.
const MIN_LINE_TOKENS: usize = 3;
/// Files listed per file mapped, to pick the shallowest source files of a
/// tree too large to map whole.
const LISTED_PER_MAPPED: usize = 8;

#[derive(Debug, Clone)]
pub struct MapOptions {
    pub max_tokens: usize,
    /// Files, directories or identifiers to rank around.
    pub focus: Vec<String>,
    /// Most source files mapped.
    pub max_files: usize,
}

impl Default for MapOptions {
    fn default() -> Self {
        MapOptions {
            max_tokens: 2000,
            focus: Vec::new(),
            max_files: DEFAULT_MAX_FILES,
        }
    }
}

/// The map of the supported files under `dir`; see the module docs.
pub fn codebase_map(root: &Path, dir: &Path, cache: &ParseCache, opts: &MapOptions) -> String {
    let prefix = dir.strip_prefix(root).unwrap_or(dir).to_path_buf();
    let shown_dir = match slash_path(&prefix) {
        p if p.is_empty() => ".".to_string(),
        p => p,
    };
    let listed = opts.max_files.saturating_mul(LISTED_PER_MAPPED);
    let (files, unlisted) = files_under(root, dir, listed);
    let files: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| Lang::from_path(p).is_some())
        .collect();
    let sources = files.len();
    let files = shallowest(files, &prefix, opts.max_files);
    let truncated_note = if files.len() < sources || unlisted {
        format!(
            "… mapped the {} shallowest of {}{} source files; narrow with `dir` or raise \
             ASTLENS_MAX_FILES\n",
            files.len(),
            sources,
            if unlisted { "+" } else { "" }
        )
    } else {
        String::new()
    };

    let incomplete = AtomicUsize::new(0);
    let loaded: Vec<Option<Arc<SymbolNames>>> = par_ranges(files.len(), 32, |range| {
        range
            .map(|i| match cache.symbol_names(&root.join(&files[i])) {
                Ok(names) => {
                    if names.incomplete() {
                        incomplete.fetch_add(1, Ordering::Relaxed);
                    }
                    Some(names)
                }
                Err(err) => {
                    tracing::debug!(%err, "skipping file in codebase map");
                    None
                }
            })
            .collect()
    });
    let (paths, names): (Vec<PathBuf>, Vec<Arc<SymbolNames>>) = files
        .into_iter()
        .zip(loaded)
        .filter_map(|(path, names)| names.map(|n| (path, n)))
        .unzip();
    let mut notes = String::new();
    let incomplete = incomplete.into_inner();
    if incomplete > 0 {
        let _ = writeln!(
            notes,
            "… {incomplete} files took too long to parse fully; some of their symbols are missing"
        );
    }
    notes.push_str(&truncated_note);
    if names.iter().all(|n| n.is_empty()) {
        return format!("no symbols in source files under {shown_dir}\n{notes}");
    }

    let slashed: Vec<String> = paths.iter().map(|p| slash_path(p)).collect();
    let focus = Focus::resolve(&opts.focus, &slashed);
    let tests = TestCode::new(&slashed);
    let graph = Graph::build(&names, &slashed, &tests, &focus.idents);
    let mut focus_files = focus.files.clone();
    for &h in &focus.idents {
        for id in graph.named(h) {
            focus_files.extend(graph.definers(id).iter().map(|&f| f as usize));
        }
    }
    for f in &focus.unmatched {
        if !(mentions::is_token(f) && graph.defines(f)) {
            let _ = writeln!(
                notes,
                "… focus {f:?} matched no file, directory or definition"
            );
        }
    }
    let personal = personalization(names.len(), &focus_files);
    let ranks = graph.rank(&personal);
    let candidates = graph.score_symbols(&names, &ranks, &tests, &focus_files);

    let groups: Vec<String> = paths.iter().map(|p| group_of(p, &prefix)).collect();
    let map = Renderer {
        root,
        cache,
        slashed: &slashed,
        groups: &groups,
        ranks: &ranks,
        has_symbols: names.iter().map(|n| !n.is_empty()).collect(),
        shown_dir: &shown_dir,
        notes: &notes,
    };
    map.fit(&paths, candidates, opts.max_tokens)
}

/// At most `keep` of the sorted `files` under `prefix`, still sorted: the
/// shallowest ones, since the core of a project sits near its top and its
/// leaves (drivers, platforms, plugins) deeper. Of the deepest level that
/// fits only in part, each top-level directory gives a file in turn, so no
/// directory crowds out the others by sorting first.
fn shallowest(files: Vec<PathBuf>, prefix: &Path, keep: usize) -> Vec<PathBuf> {
    if files.len() <= keep {
        return files;
    }
    let below = |p: &PathBuf| p.strip_prefix(prefix).unwrap_or(p).to_path_buf();
    let depth = |p: &PathBuf| below(p).components().count();
    let mut by_depth: Vec<usize> = files.iter().map(depth).collect();
    by_depth.sort_unstable();
    let level = by_depth[keep];
    let whole = by_depth.partition_point(|&d| d < level);

    let mut turns: HashMap<PathBuf, usize> = HashMap::new();
    let mut partial: Vec<(usize, usize)> = Vec::new();
    let mut kept = vec![false; files.len()];
    for (i, path) in files.iter().enumerate() {
        match depth(path).cmp(&level) {
            std::cmp::Ordering::Less => kept[i] = true,
            std::cmp::Ordering::Equal => {
                let top = below(path)
                    .components()
                    .next()
                    .map(|c| PathBuf::from(c.as_os_str()));
                let turn = turns.entry(top.unwrap_or_default()).or_default();
                partial.push((*turn, i));
                *turn += 1;
            }
            std::cmp::Ordering::Greater => {}
        }
    }
    partial.sort_unstable();
    for &(_, i) in partial.iter().take(keep - whole) {
        kept[i] = true;
    }
    files
        .into_iter()
        .zip(kept)
        .filter_map(|(p, k)| k.then_some(p))
        .collect()
}

/// Focus strings split into the files they name and the identifiers.
struct Focus {
    files: BTreeSet<usize>,
    idents: Vec<u32>,
    /// Strings that named no file, as given.
    unmatched: Vec<String>,
}

impl Focus {
    /// A string that is a file's path, or a directory holding files, focuses
    /// those files; otherwise one that is an identifier focuses the files
    /// defining it and weights its mentions up.
    fn resolve(focus: &[String], paths: &[String]) -> Self {
        let mut out = Focus {
            files: BTreeSet::new(),
            idents: Vec::new(),
            unmatched: Vec::new(),
        };
        for raw in focus {
            let f = raw.trim();
            let f = f.strip_prefix("./").unwrap_or(f).trim_end_matches('/');
            if f.is_empty() {
                continue;
            }
            let before = out.files.len();
            let under = format!("{f}/");
            out.files.extend(
                paths
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| *p == f || p.starts_with(&under))
                    .map(|(i, _)| i),
            );
            if out.files.len() > before {
                continue;
            }
            if mentions::is_token(f) {
                out.idents.push(mentions::hash(f));
                out.idents.push(mentions::macro_hash(f));
                out.idents.push(mentions::tag_hash(f));
            }
            out.unmatched.push(f.to_string());
        }
        out.idents.sort_unstable();
        out.idents.dedup();
        out
    }
}

/// The teleport distribution: uniform, or mostly on the focus files.
fn personalization(n: usize, focus: &BTreeSet<usize>) -> Vec<f64> {
    let uniform = 1.0 / n as f64;
    if focus.is_empty() {
        return vec![uniform; n];
    }
    let mut p = vec![(1.0 - FOCUS_SHARE) * uniform; n];
    let each = FOCUS_SHARE / focus.len() as f64;
    for &f in focus {
        p[f] += each;
    }
    p
}

/// Hasher for keys that are already hashes: a multiply spreads them over the
/// high bits the table uses.
#[derive(Default)]
struct PreHashed(u64);

impl Hasher for PreHashed {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn write_u32(&mut self, n: u32) {
        self.0 = u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

type HashIds = HashMap<u32, u32, BuildHasherDefault<PreHashed>>;

/// Owner hash of a definition that has none.
const NO_OWNER: u32 = 0;

/// Files and the names they define, with the references between them.
///
/// An ident is a defined name, together with its owner when it is a member
/// of a type or module (`ParseCache` for `ParseCache::get`): a file mentions
/// a member only when it mentions both its name and its owner's, so a call
/// of some other type's `get` does not count for it. For each ident, `defs`
/// lists the files defining it and `refs` the files that mention it without
/// defining it, with the share of their out-weight each sends. A file's rank
/// thus flows through each name it mentions, and does not define, to that
/// name's definers: to the one nearest it, if one is (see [`nearest`]),
/// else evenly to all.
struct Graph {
    /// Name hash to its first ident; idents of one name are consecutive.
    by_name: HashIds,
    /// Per ident, its name hash, its owner's (or [`NO_OWNER`]) and the
    /// scope key of its definers (or [`PUBLIC`]).
    ident_name: Vec<u32>,
    ident_owner: Vec<u32>,
    ident_scope: Vec<u32>,
    /// Per file, see [`Unit`].
    units: Vec<Unit>,
    /// See [`type_names`].
    types: Vec<u32>,
    /// Per ident, `def_start[i]..def_start[i + 1]` into `def_file`.
    def_start: Vec<u32>,
    def_file: Vec<u32>,
    /// Per ident, `ref_start[i]..ref_start[i + 1]` into `ref_file`, `ref_w`
    /// and `ref_near`, the definer the file most likely means as an index
    /// into the ident's definers, or [`ALL`] (see [`nearest`]).
    ref_start: Vec<u32>,
    ref_file: Vec<u32>,
    ref_w: Vec<f32>,
    ref_near: Vec<u32>,
    /// Per file, `file_start[f]..file_start[f + 1]` into `file_defs`, the
    /// idents it defines, and `file_slots`, its place among the definers of
    /// each as an index into `def_file`.
    file_start: Vec<u32>,
    file_defs: Vec<u32>,
    file_slots: Vec<u32>,
    /// (file, share of its rank that follows the teleport instead of its
    /// references): 1 for a file that mentions no name another file
    /// defines, less for one whose mentions say little (see [`MIN_OUT`]).
    dangling: Vec<(u32, f32)>,
}

/// The weight of the uses a file needs to pass all its rank on along them.
/// A file whose only uses are weak, a one-letter name or one that many files
/// define, keeps most of its rank out of them, rather than piling it on
/// whatever they happen to match.
const MIN_OUT: f64 = 1.0;

/// The identifier of the type or module `parent` names, as its mentions
/// spell it: `Renderer` for `Renderer<'_>`, `Class` for `ns::Class`.
fn owner_token(parent: &str) -> Option<&str> {
    let end = parent.find(['<', '(', '[']).unwrap_or(parent.len());
    parent[..end]
        .rsplit(|c: char| !is_ident_char(c))
        .find(|s| !s.is_empty())
        .filter(|s| mentions::is_token(s))
}

/// The (name, owner) hashes symbol `s` is defined under, or `None` for a
/// symbol that takes no rank: `impl` blocks, which name a type defined
/// elsewhere, modules, which hold the symbols that do, and names that are
/// not plain identifiers. A Rust macro is named as it is called, `name!`,
/// and a C `struct`, `union` or `enum` as its tag is written, `struct name`.
///
/// The owner of an item of a Rust `impl Trait for Type` is `Type` when the
/// workspace defines a type of that name (in `types`, sorted hashes), and
/// otherwise the trait: a method of `impl Storage for Vec<T>` is called
/// with `Storage` in scope, while `Vec` says nothing. A top-level
/// definition with no owner of its own has `module` (see
/// [`Unit::module_owner`]).
fn name_key(n: &SymbolNames, s: usize, types: &[u32], module: u32) -> Option<(u32, u32)> {
    let (kind, _) = n.kind_and_line(s)?;
    let name = n.name(s)?;
    if matches!(kind, SymbolKind::Impl | SymbolKind::Module) || !mentions::is_token(name) {
        return None;
    }
    if kind == SymbolKind::Macro && n.lang() == Some(Lang::Rust) {
        return Some((mentions::macro_hash(name), NO_OWNER));
    }
    if matches!(kind, SymbolKind::Struct | SymbolKind::Enum) && n.lang() == Some(Lang::C) {
        return Some((mentions::tag_hash(name), NO_OWNER));
    }
    let owner = n
        .parent(s)
        .and_then(owner_token)
        .map_or(module, mentions::hash);
    let owner = match n.trait_of(s) {
        Some(t) if types.binary_search(&owner).is_err() => t,
        _ => owner,
    };
    Some((mentions::hash(name), owner))
}

/// The sorted name hashes of the types `names` define.
fn type_names(names: &[Arc<SymbolNames>]) -> Vec<u32> {
    use SymbolKind::*;
    let mut types: Vec<u32> = par_ranges(names.len(), 256, |range| {
        range
            .flat_map(|f| {
                let n = &names[f];
                (0..n.len()).filter_map(move |s| {
                    let (kind, _) = n.kind_and_line(s)?;
                    matches!(kind, Class | Struct | Enum | Interface | Trait | Type)
                        .then(|| n.name(s).map(mentions::hash))
                        .flatten()
                })
            })
            .collect()
    });
    types.sort_unstable();
    types.dedup();
    types
}

/// An ident: the name and owner hashes of [`name_key`], and the scope key
/// of its definers: [`Unit::public_key`], or [`Unit::private_key`] for a
/// private definition.
type Key = (u32, u32, u32);

/// The key of symbol `s` of a file of `unit`, or `None` when it takes no
/// rank, also when no other file can mention it: it is local to a function,
/// or private to its file.
fn def_key(n: &SymbolNames, s: usize, unit: &Unit, types: &[u32]) -> Option<Key> {
    let (name, owner) = name_key(n, s, types, unit.module_owner())?;
    Some((name, owner, unit.key(n.reach(s))?))
}

/// The `ref_near` of a file that may mean any of the definers of a name.
const ALL: u32 = u32::MAX;

/// The most definers of a name among which [`nearest`] picks one.
const NEAREST_OF: usize = 64;

/// Which of `definers` (files, at `paths`) a file at `path` most likely means
/// by a name they all define, as an index into them: the one sharing the
/// most directories with it, as a vendored copy of a library is used by the
/// code around it; [`ALL`] when several share as many, or there are too many
/// to tell. With `alternates`, tied definers in one directory are builds of
/// one name for different platforms, and the most common build stands for
/// them (see [`build_rank`]).
fn nearest(definers: &[u32], paths: &[String], path: &str, alternates: bool) -> u32 {
    if definers.len() < 2 || definers.len() > NEAREST_OF {
        return ALL;
    }
    let mut best: Option<(usize, u32)> = None;
    let mut tied = false;
    for (k, &d) in (0u32..).zip(definers) {
        let shared = shared_dirs(&paths[d as usize], path);
        match best {
            Some((most, _)) if shared < most => {}
            Some((most, _)) if shared == most => tied = true,
            _ => {
                best = Some((shared, k));
                tied = false;
            }
        }
    }
    let Some((most, k)) = best else {
        return ALL;
    };
    if !tied {
        return k;
    }
    if !alternates {
        return ALL;
    }
    let dir = |d: u32| {
        paths[d as usize]
            .rsplit_once('/')
            .map_or("", |(dir, _)| dir)
    };
    let near: Vec<(u32, &str)> = (0u32..)
        .zip(definers)
        .filter(|&(_, &d)| shared_dirs(&paths[d as usize], path) == most)
        .map(|(k, &d)| (k, paths[d as usize].as_str()))
        .collect();
    let first_dir = dir(definers[near[0].0 as usize]);
    if near
        .iter()
        .any(|&(k, _)| dir(definers[k as usize]) != first_dir)
    {
        return ALL;
    }
    near.iter()
        .min_by_key(|&&(k, p)| (build_rank(p), k))
        .map_or(ALL, |&(k, _)| k)
}

/// How rarely the file at `path` is built, by the platform words in its
/// name: 0 for a file built everywhere or on the most common platforms
/// (`os.go`, `os_unix.go`, `os_linux_amd64.go`), more for each other one
/// (`os_plan9.go`, `os_js.go`) and for a fallback (`cpu_no_name.go`).
fn build_rank(path: &str) -> usize {
    const COMMON: &[&str] = &[
        "unix", "linux", "amd64", "x86", "x86_64", "64bit", "generic",
    ];
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.split('.').next().unwrap_or(file);
    stem.split('_')
        .skip(1)
        .filter(|w| !w.is_empty() && !COMMON.contains(w))
        .count()
}

/// Whether files of `lang` in one directory define a name in turn, one for
/// each platform or build: Go by build constraints, Rust by `cfg`.
fn has_alternates(lang: Option<Lang>) -> bool {
    matches!(lang, Some(Lang::Go | Lang::Rust))
}

/// The [`mentions::relative_path`] a file at `from` imports the file at `to`
/// by.
fn import_path(from: &str, to: &str) -> String {
    let dir: Vec<&str> = from
        .rsplit_once('/')
        .map_or(Vec::new(), |(dir, _)| dir.split('/').collect());
    let target: Vec<&str> = to.split('/').collect();
    let shared = dir
        .iter()
        .zip(&target)
        .take_while(|(a, b)| a == b)
        .count()
        .min(target.len() - 1);
    let ups = std::iter::repeat_n("..", dir.len() - shared);
    mentions::relative_path(ups.chain(target[shared..].iter().copied()))
}

/// How many leading directories the paths `a` and `b` share.
fn shared_dirs(a: &str, b: &str) -> usize {
    a.bytes()
        .zip(b.bytes())
        .take_while(|(x, y)| x == y)
        .filter(|&(x, _)| x == b'/')
        .count()
}

/// How many of the `uses` of name `h` (mentions besides the file's own
/// definitions of it, at `path`, whose mentions are `mentions`) are uses of
/// the definitions owned by `owner` of `unit`, the file at `own`, and a
/// factor on their weight.
fn uses_of(
    (unit, own): (&Unit, &str),
    user: &Unit,
    mentions: &Mentions,
    path: &str,
    h: u32,
    owner: u32,
    uses: u8,
) -> (u8, f64) {
    // A saturated count may hide uses, so it stays.
    let saturated = uses == u8::MAX;
    let tracked = unit.lang.is_some_and(mentions::tracks_members) && !saturated;
    let members = mentions.count(mentions::member_hash(h));
    if owner == NO_OWNER {
        if unit.module == NO_OWNER {
            // A definition with no owner is not a field or method of a
            // value, `x.name`.
            return (
                if tracked {
                    uses.saturating_sub(members)
                } else {
                    uses
                },
                1.0,
            );
        }
        // A JavaScript module's export is used by importing the module by
        // its path, also as `module.name`. A global of a script needs no
        // import, yet counts less without one, and a module sees only those
        // a declaration file declares.
        return if unit.imported_by(mentions, own, path) {
            (uses, 1.0)
        } else if unit.js_module || ((user.js_module || user.declaration) && !unit.declaration) {
            (0, 1.0)
        } else {
            (
                if tracked {
                    uses.saturating_sub(members)
                } else {
                    uses
                },
                UNIMPORTED,
            )
        };
    }
    if owner == unit.module_owner() {
        // Outside its Go package, a definition is named with the package,
        // `pkg.Name`; a Python one with its module, or imported from it.
        let qualified = match unit.lang {
            Some(Lang::Go) => !unit.scope.reaches(path),
            Some(Lang::Python) => true,
            _ => false,
        };
        if qualified {
            let named = mentions.count(mentions::qualified_hash(unit.module, h));
            let all = unit.lang == Some(Lang::Python)
                && mentions.contains(mentions::qualified_hash(unit.module, mentions::hash("*")));
            // (A Python value may share its name with a module, `parser`.)
            let imported = unit.lang == Some(Lang::Go)
                || mentions.contains(mentions::import_hash_of(unit.module));
            if (named == 0 && !all) || !imported {
                return (0, 1.0);
            }
            if unit.lang == Some(Lang::Go) {
                return (uses.min(named), 1.0);
            }
        } else if !unit.is_used_by(mentions, own, path) {
            return (0, 1.0);
        }
        // `copy.copy(x)` names the module of `copy` as well as it, while
        // `s.copy()` copies some value.
        let uses = if h == unit.module && tracked {
            uses.saturating_sub(members)
        } else {
            uses
        };
        return (uses, 1.0);
    }
    if !mentions.contains(owner) {
        return (0, 1.0);
    }
    // A member is mentioned with its owner, and called or read as one:
    // `x.name`, `Type::name`; in C++ also plainly, on `this`.
    let as_member = tracked && unit.lang != Some(Lang::Cpp);
    (if as_member { uses.min(members) } else { uses }, 1.0)
}

/// The weight factor of uses of a JavaScript script's global by a file that
/// does not name the script.
const UNIMPORTED: f64 = 0.1;

/// Scope keys below this are those of public definitions, one per
/// [`family`] of languages.
const PUBLIC_END: u32 = 16;
/// The scope key of definitions no other file can mention.
const FILE_ONLY: u32 = u32::MAX;

/// Whether the slash-separated `path` is that of a C or C++ header.
fn is_header(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, ext)| {
        matches!(
            ext,
            "h" | "hh" | "hpp" | "hxx" | "h++" | "inl" | "ipp" | "tcc" | "tpp"
        )
    })
}

/// The slash-terminated prefix of the files that may include the headers in
/// `dir` (`""` for all): those under the directory holding the last
/// `include` directory on the way, whose headers are for the code around it
/// (`include/linux/` for the whole tree, `arch/x86/include/asm/` for
/// `arch/x86/`), or else those under `dir` itself, whose headers are for the
/// code beside them.
fn include_root(dir: &str) -> String {
    let parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    let end = parts
        .iter()
        .rposition(|p| p.eq_ignore_ascii_case("include"))
        .unwrap_or(parts.len());
    parts[..end].iter().map(|p| format!("{p}/")).collect()
}

/// Whether `path` is that of a TypeScript declaration file.
fn is_declaration(path: &str) -> bool {
    [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// The prefix of the files a declaration file in `dir` declares globals for,
/// those of the project holding its `typings`, `types` or `@types` directory
/// (`ext/src/` for `ext/src/typings`), or `None` outside such a directory.
fn typings_root(dir: &str) -> Option<String> {
    let parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    let end = parts.iter().rposition(|p| {
        ["typings", "types", "@types"]
            .iter()
            .any(|t| p.eq_ignore_ascii_case(t))
    })?;
    Some(parts[..end].iter().map(|p| format!("{p}/")).collect())
}

fn is_public(scope: u32) -> bool {
    scope < PUBLIC_END
}

/// The languages whose files can mention each other's definitions: C and
/// C++ include each other's headers, JavaScript and TypeScript import each
/// other's modules, and the rest keep to themselves. A Go file mentioning
/// `p` does not call a JavaScript `function p`.
fn family(lang: Option<Lang>) -> u32 {
    match lang {
        Some(Lang::Rust) => 1,
        Some(Lang::C | Lang::Cpp) => 2,
        Some(Lang::Go) => 3,
        Some(Lang::Python) => 4,
        Some(Lang::JavaScript | Lang::TypeScript | Lang::Tsx) => 5,
        Some(Lang::Java) => 6,
        Some(Lang::CSharp) => 7,
        Some(Lang::GdScript) => 8,
        None => 9,
    }
}

/// What the definitions of one file are keyed under, and which files can
/// mention them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Unit {
    lang: Option<Lang>,
    /// The reach of its private definitions.
    scope: Scope,
    /// The scope key of its public definitions: the [`family`] of its
    /// language.
    public_key: u32,
    /// The scope key of its private definitions.
    private_key: u32,
    /// The hash of the name other files qualify its top-level definitions
    /// with, or import them by: its Go package or Python module (whose
    /// top-level definitions it owns, see [`Unit::module_owner`]), or the
    /// [`mentions::import_hash`] of its JavaScript module (`strings` for
    /// `strings.ts`, as in `from './strings'`). [`NO_OWNER`] for none.
    module: u32,
    /// The hash of the name of the Python package holding the module, which
    /// may import its definitions for others; [`NO_OWNER`] for none.
    package: u32,
    /// Whether it is a JavaScript module, whose exports only the files
    /// importing it use.
    js_module: bool,
    /// For a C or C++ header, the [`mentions::import_hash`] of its stem,
    /// which the files including it mention; [`NO_OWNER`] for other files.
    header: u32,
    /// For a JavaScript module, the [`mentions::relative_hash`] of its stem,
    /// which the files importing it by a relative path mention;
    /// [`NO_OWNER`] for other files.
    relative: u32,
    /// Whether it is a TypeScript declaration file, `x.d.ts`, whose globals
    /// are declared for modules too.
    declaration: bool,
    /// Whether its public definitions reach only its `scope`, as those of a
    /// header or of a declaration file in a `typings` directory do.
    confined: bool,
}

impl Unit {
    fn of(n: &SymbolNames, path: &str) -> Self {
        let lang = n.lang();
        let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
        let js = matches!(lang, Some(Lang::JavaScript | Lang::TypeScript | Lang::Tsx));
        let declaration = js && is_declaration(path);
        // A script declaring a package's module is that module.
        let ambient = n.ambient().filter(|_| js);
        let js_module = n.is_module() || ambient.is_some();
        // A declaration file's globals are for the project holding it.
        let typings = if declaration && !js_module {
            typings_root(dir)
        } else {
            None
        };
        let header = if matches!(lang, Some(Lang::C | Lang::Cpp)) && is_header(path) {
            let stem = file.split('.').next().unwrap_or(file);
            mentions::import_hash(stem)
        } else {
            NO_OWNER
        };
        let confined = header != NO_OWNER || typings.is_some();
        let scope = match typings {
            Some(prefix) => Scope::Within {
                prefix,
                nested: true,
            },
            None => Scope::of(lang, path),
        };
        let public_key = family(lang);
        let private_key = match &scope {
            Scope::Public => public_key,
            Scope::File => FILE_ONLY,
            Scope::Within { prefix, nested } => {
                (mentions::hash(prefix) ^ u32::from(*nested) ^ public_key.rotate_left(16))
                    .clamp(PUBLIC_END, FILE_ONLY - 1)
            }
        };
        fn base(dir: &str) -> &str {
            dir.rsplit('/').next().unwrap_or(dir)
        }
        let parent = dir.rsplit_once('/').map_or("", |(parent, _)| parent);
        let token = |s: &str| {
            if mentions::is_token(s) {
                mentions::hash(s)
            } else {
                NO_OWNER
            }
        };
        let stem = file.split('.').next().unwrap_or(file);
        let (module, package) = match lang {
            Some(Lang::Go) => (n.package().unwrap_or_else(|| token(base(dir))), NO_OWNER),
            Some(Lang::Python) if stem == "__init__" => (token(base(dir)), token(base(parent))),
            Some(Lang::Python) => (token(stem), token(base(dir))),
            Some(Lang::JavaScript | Lang::TypeScript | Lang::Tsx) => (
                ambient.unwrap_or_else(|| {
                    mentions::module_stem(path).map_or(NO_OWNER, mentions::import_hash)
                }),
                NO_OWNER,
            ),
            _ => (NO_OWNER, NO_OWNER),
        };
        Unit {
            lang,
            scope,
            public_key,
            private_key,
            module,
            package,
            js_module,
            header,
            relative: if js && ambient.is_none() {
                mentions::module_stem(path).map_or(NO_OWNER, mentions::relative_hash)
            } else {
                NO_OWNER
            },
            declaration,
            confined,
        }
    }

    /// The scope key of the definitions of the file that `reach` other
    /// files: a C or C++ header's public ones reach no further than its
    /// private ones, the files including it.
    fn key(&self, reach: Reach) -> Option<u32> {
        match reach {
            Reach::Public if !self.confined => Some(self.public_key),
            Reach::Public | Reach::Private => {
                (self.private_key != FILE_ONLY).then_some(self.private_key)
            }
            Reach::Local => None,
        }
    }

    /// Whether the file at `path`, whose mentions are `mentions`, can
    /// mention the private definitions of this file: it is in their
    /// [`Scope`], or includes this header by name.
    fn reaches(&self, mentions: &Mentions, path: &str) -> bool {
        self.scope.reaches(path) || (self.header != NO_OWNER && mentions.contains(self.header))
    }

    /// The owner of the file's top-level definitions: its Go package, Python
    /// module or JavaScript module, which other files name or import to use
    /// them, so that the definitions of one module are told apart from
    /// another's of the same name; [`NO_OWNER`] for the other languages and
    /// for a JavaScript script, whose globals need no import.
    fn module_owner(&self) -> u32 {
        match self.lang {
            Some(Lang::Go | Lang::Python) => self.module,
            Some(Lang::JavaScript | Lang::TypeScript | Lang::Tsx) if self.js_module => self.module,
            _ => NO_OWNER,
        }
    }

    /// Whether a file whose mentions are `mentions`, at `path`, uses the
    /// definitions this file's module owns: it names the module (or the
    /// Python package holding it), or is in the same Go package.
    fn is_used_by(&self, mentions: &Mentions, own: &str, path: &str) -> bool {
        self.imported_by(mentions, own, path)
            || (self.package != NO_OWNER && mentions.contains(self.package))
            || (self.lang == Some(Lang::Go) && self.scope.reaches(path))
    }

    /// Whether a file whose mentions are `mentions`, at `path`, names the
    /// module of this file, at `own`: a JavaScript one by the stem of a path
    /// from some base directory, or by its path relative to the importer.
    fn imported_by(&self, mentions: &Mentions, own: &str, path: &str) -> bool {
        mentions.contains(self.module)
            || (self.relative != NO_OWNER
                && mentions.contains(self.relative)
                && mentions.contains(mentions::import_hash(&import_path(path, own))))
    }
}

/// Which files can mention the private definitions of a file (see
/// [`Reach::Private`]), by its language.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    /// Every file: the language has no private definitions.
    Public,
    /// Only the file itself: C and C++ `static`, Java and C# `private`, what
    /// a JavaScript module does not export.
    File,
    /// The files under the slash-terminated `prefix` (`""` for all): a Rust
    /// module and the modules under it, a Python package, the files that may
    /// include a C or C++ header (see [`include_root`]). With `nested` false,
    /// only the files right in it: a Go package.
    Within { prefix: String, nested: bool },
}

impl Scope {
    fn of(lang: Option<Lang>, path: &str) -> Self {
        let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
        let dir_prefix = || {
            if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            }
        };
        let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
        match lang {
            Some(Lang::Rust) => Scope::Within {
                // `a/mod.rs` and `lib.rs` hold the module of their directory,
                // `a/b.rs` the module whose children are in `a/b/`.
                prefix: if matches!(stem, "mod" | "lib" | "main") {
                    dir_prefix()
                } else {
                    format!("{}{stem}/", dir_prefix())
                },
                nested: true,
            },
            Some(Lang::Go) => Scope::Within {
                prefix: dir_prefix(),
                nested: false,
            },
            Some(Lang::Python | Lang::GdScript) => Scope::Within {
                prefix: dir_prefix(),
                nested: true,
            },
            Some(Lang::C | Lang::Cpp) if is_header(path) => Scope::Within {
                prefix: include_root(dir),
                nested: true,
            },
            Some(
                Lang::C
                | Lang::Cpp
                | Lang::Java
                | Lang::CSharp
                | Lang::JavaScript
                | Lang::TypeScript
                | Lang::Tsx,
            ) => Scope::File,
            None => Scope::Public,
        }
    }

    /// Whether the file at `path` can mention a private definition of a
    /// file with this scope (the file itself aside).
    fn reaches(&self, path: &str) -> bool {
        match self {
            Scope::Public => true,
            Scope::File => false,
            Scope::Within { prefix, nested } => path
                .strip_prefix(prefix.as_str())
                .is_some_and(|rest| *nested || !rest.contains('/')),
        }
    }
}

impl Graph {
    /// The graph of `names`, the symbol names of the files at `paths`.
    /// Definitions in test code (`tests`) take no rank; mentions of the
    /// `focus` names count more.
    fn build(
        names: &[Arc<SymbolNames>],
        paths: &[String],
        tests: &TestCode,
        focus: &[u32],
    ) -> Self {
        let units: Vec<Unit> = names
            .iter()
            .zip(paths)
            .map(|(n, path)| Unit::of(n, path))
            .collect();
        let units_ref = &units;
        let type_list = type_names(names);
        let types = &type_list;
        // (name hash, owner hash, scope key, file, symbol) of every
        // definition that can take rank.
        let mut defs: Vec<(Key, u32, u32)> = par_ranges(names.len(), 256, |range| {
            range
                .flat_map(|f| {
                    let n = &names[f];
                    let unit = &units_ref[f];
                    (0..n.len()).filter_map(move |s| {
                        let key = def_key(n, s, unit, types)?;
                        (!tests.is_test(n, f, s)).then_some((key, f as u32, s as u32))
                    })
                })
                .collect()
        });
        defs.sort_unstable();
        defs.dedup_by_key(|&mut (key, f, _)| (key, f));

        let mut by_name = HashIds::default();
        let mut ident_name = Vec::new();
        let mut ident_owner = Vec::new();
        let mut ident_scope = Vec::new();
        let mut def_start = Vec::new();
        let mut def_file = Vec::with_capacity(defs.len());
        let mut multiplier = Vec::new();
        // Definitions of the current name, with any owner: `len` or `new`,
        // defined for many types, says little about which one a file uses.
        let mut name_defs = (None, 0);
        let mut at = 0;
        while at < defs.len() {
            let (key, first_file, first_sym) = defs[at];
            let (h, owner, scope) = key;
            if name_defs.0 != Some(h) {
                name_defs = (
                    Some(h),
                    defs[at..].iter().take_while(|d| d.0.0 == h).count(),
                );
            }
            let end = at + defs[at..].iter().take_while(|d| d.0 == key).count();
            // A Go package's or Python module's own definition is used by
            // naming the module, which tells it apart from those of others.
            let module_owned =
                owner != NO_OWNER && owner == units[first_file as usize].module_owner();
            let spread = if module_owned { end - at } else { name_defs.1 };
            by_name.entry(h).or_insert(ident_name.len() as u32);
            ident_name.push(h);
            ident_owner.push(owner);
            ident_scope.push(scope);
            def_start.push(def_file.len() as u32);
            def_file.extend(defs[at..end].iter().map(|d| d.1));
            let name = names[first_file as usize]
                .name(first_sym as usize)
                .unwrap_or_default();
            multiplier.push(multiplier_of(name, spread, focus.binary_search(&h).is_ok()));
            at = end;
        }
        def_start.push(def_file.len() as u32);
        drop(defs);
        let idents = multiplier.len();
        let definers = |i: usize| &def_file[def_start[i] as usize..def_start[i + 1] as usize];
        let (by_name_ref, ident_names) = (&by_name, &ident_name);
        let variants = |h: u32| {
            let first = by_name_ref.get(&h).map_or(idents, |&i| i as usize);
            (first..idents).take_while(move |&i| ident_names[i] == h)
        };

        // Each file's mentions of names only other files define, weighted
        // before the spread of each name is known. A file that defines a
        // name itself, even in test code or privately, most likely means its
        // own definition, so those mentions are left out, as are those of
        // private definitions the file cannot reach.
        let edges: Vec<Vec<(u32, f32, u32)>> = par_ranges(names.len(), 256, |range| {
            range
                .map(|f| {
                    let n = &names[f];
                    let mentions = n.mentions();
                    let path = paths[f].as_str();
                    let module = units[f].module_owner();
                    let mut own: Vec<(u32, u32)> = (0..n.len())
                        .filter_map(|s| name_key(n, s, types, module))
                        .collect();
                    own.sort_unstable();
                    // The name token of each definition, which is no use.
                    let mut defined: Vec<u32> = (0..n.len())
                        .filter(|&s| {
                            n.kind_and_line(s)
                                .is_some_and(|(k, _)| k != SymbolKind::Impl)
                        })
                        .filter_map(|s| n.name(s).map(mentions::hash))
                        .collect();
                    defined.sort_unstable();
                    let mut out = Vec::new();
                    let mut found_here = Vec::new();
                    for (h, count) in mentions.iter() {
                        // (A saturated count may hide uses, so it stays.)
                        let saturated = count == u8::MAX;
                        let uses = if saturated {
                            count
                        } else {
                            let at = defined.partition_point(|&d| d < h);
                            let names = defined[at..].iter().take_while(|&&d| d == h).count();
                            count.saturating_sub(u8::try_from(names).unwrap_or(u8::MAX))
                        };
                        if uses == 0 {
                            continue;
                        }
                        for id in variants(h) {
                            let owner = ident_owner[id];
                            let scope = ident_scope[id];
                            let defs = definers(id);
                            // The definers of an ident share their language
                            // family, scope and module stem, but not their
                            // header or path, so each is checked apart, up to
                            // a point.
                            let first = &units[defs[0] as usize];
                            if first.public_key != units[f].public_key
                                || own.binary_search(&(h, owner)).is_ok()
                            {
                                continue;
                            }
                            let checked = if defs.len() > NEAREST_OF {
                                &defs[..1]
                            } else {
                                defs
                            };
                            let mut found = (0, 1.0);
                            let mut used = 0;
                            let mut only = ALL;
                            for (k, &d) in (0u32..).zip(checked) {
                                let unit = &units[d as usize];
                                if !is_public(scope)
                                    && defs.len() <= NEAREST_OF
                                    && !unit.reaches(mentions, path)
                                {
                                    continue;
                                }
                                let got = uses_of(
                                    (unit, &paths[d as usize]),
                                    &units[f],
                                    mentions,
                                    path,
                                    h,
                                    owner,
                                    uses,
                                );
                                if got.0 > 0 {
                                    if used == 0 {
                                        found = got;
                                    }
                                    used += 1;
                                    only = k;
                                }
                            }
                            if defs.len() > NEAREST_OF
                                && !is_public(scope)
                                && !defs
                                    .iter()
                                    .any(|&d| units[d as usize].reaches(mentions, path))
                            {
                                continue;
                            }
                            let (uses, factor) = found;
                            if uses == 0 {
                                continue;
                            }
                            // The one definer the file uses, else the nearest.
                            let near = if used == 1 && defs.len() > 1 && checked.len() > 1 {
                                only
                            } else {
                                nearest(defs, paths, path, has_alternates(first.lang))
                            };
                            let w = term_weight(uses) * multiplier[id] * factor;
                            found_here.push(((id as u32, w as f32, near), factor == 1.0));
                        }
                        // A name the file imports, or otherwise names the
                        // owner of, hides a global of the same name.
                        let owned = found_here.iter().any(|&(_, owned)| owned);
                        out.extend(
                            found_here
                                .drain(..)
                                .filter(|&(_, o)| o || !owned)
                                .map(|(edge, _)| edge),
                        );
                    }
                    out
                })
                .collect()
        });

        let mut ref_count = vec![0u32; idents + 1];
        for out in &edges {
            for &(id, ..) in out {
                ref_count[id as usize + 1] += 1;
            }
        }
        // Inverse document frequency: a name nearly every file mentions,
        // such as `len` or `Result`, says little about which definition a
        // file depends on, so it carries little rank.
        let files = names.len() as f64;
        let idf: Vec<f32> = (0..idents)
            .map(|i| {
                let mentioned_by = f64::from(ref_count[i + 1]) + definers(i).len() as f64;
                ((files + 1.0) / mentioned_by).ln() as f32
            })
            .collect();
        let mut ref_start = ref_count;
        for i in 1..ref_start.len() {
            ref_start[i] += ref_start[i - 1];
        }
        let total_refs = ref_start[idents] as usize;
        let mut ref_file = vec![0u32; total_refs];
        let mut ref_w = vec![0f32; total_refs];
        let mut ref_near = vec![ALL; total_refs];
        let mut fill = ref_start.clone();
        let mut dangling = Vec::new();
        for (f, out) in edges.into_iter().enumerate() {
            let total: f64 = out
                .iter()
                .map(|&(id, w, _)| f64::from(w) * f64::from(idf[id as usize]))
                .sum();
            let raw: f64 = out.iter().map(|&(_, w, _)| f64::from(w)).sum();
            let kept = (raw / MIN_OUT).min(1.0);
            if total <= 0.0 || kept <= 0.0 {
                dangling.push((f as u32, 1.0));
                continue;
            }
            if kept < 1.0 {
                dangling.push((f as u32, (1.0 - kept) as f32));
            }
            for (id, w, near) in out {
                let at = &mut fill[id as usize];
                ref_file[*at as usize] = f as u32;
                ref_w[*at as usize] =
                    (kept * f64::from(w) * f64::from(idf[id as usize]) / total) as f32;
                ref_near[*at as usize] = near;
                *at += 1;
            }
        }

        let mut file_count = vec![0u32; names.len() + 1];
        for &f in &def_file {
            file_count[f as usize + 1] += 1;
        }
        let mut file_start = file_count;
        for i in 1..file_start.len() {
            file_start[i] += file_start[i - 1];
        }
        let mut file_defs = vec![0u32; def_file.len()];
        let mut file_slots = vec![0u32; def_file.len()];
        let mut fill = file_start.clone();
        for (ident, range) in def_start.windows(2).enumerate() {
            for slot in range[0]..range[1] {
                let at = &mut fill[def_file[slot as usize] as usize];
                file_defs[*at as usize] = ident as u32;
                file_slots[*at as usize] = slot;
                *at += 1;
            }
        }

        Graph {
            by_name,
            ident_name,
            ident_owner,
            ident_scope,
            units,
            types: type_list,
            def_start,
            def_file,
            ref_start,
            ref_file,
            ref_w,
            ref_near,
            file_start,
            file_defs,
            file_slots,
            dangling,
        }
    }

    fn idents(&self) -> usize {
        self.def_start.len() - 1
    }

    fn files(&self) -> usize {
        self.file_start.len() - 1
    }

    fn definers(&self, ident: usize) -> &[u32] {
        &self.def_file[self.def_start[ident] as usize..self.def_start[ident + 1] as usize]
    }

    /// The idents `file` defines, ascending.
    fn defs_of(&self, file: usize) -> &[u32] {
        &self.file_defs[self.file_start[file] as usize..self.file_start[file + 1] as usize]
    }

    /// The idents named by the hash `h`, with any owner.
    fn named(&self, h: u32) -> impl Iterator<Item = usize> + '_ {
        let first = self.by_name.get(&h).map_or(self.idents(), |&i| i as usize);
        (first..self.idents()).take_while(move |&i| self.ident_name[i] == h)
    }

    /// The ident of a definition keyed by [`def_key`].
    fn ident(&self, (name, owner, scope): Key) -> Option<usize> {
        self.named(name)
            .find(|&i| self.ident_owner[i] == owner && self.ident_scope[i] == scope)
    }

    /// Whether some file defines `name`, a Rust macro `name!` or a C tag
    /// `struct name` (up to hash collisions).
    fn defines(&self, name: &str) -> bool {
        [
            mentions::hash(name),
            mentions::macro_hash(name),
            mentions::tag_hash(name),
        ]
        .iter()
        .any(|h| self.by_name.contains_key(h))
    }

    /// The places of the definitions `file` makes among the definers of
    /// each, in the order of [`Graph::defs_of`].
    fn slots_of(&self, file: usize) -> &[u32] {
        &self.file_slots[self.file_start[file] as usize..self.file_start[file + 1] as usize]
    }

    /// Rank each definition receives from the files that mention its name,
    /// per place in `def_file`: what a file sends to the definer it most
    /// likely means, and an even share of what it sends to all definers.
    fn flows(&self, ranks: &[f64]) -> Vec<f64> {
        par_ranges(self.idents(), RANK_CHUNK, |range| {
            let mut out = Vec::new();
            for i in range {
                let base = out.len();
                let definers = self.definers(i).len();
                out.resize(base + definers, 0.0);
                let mut even = 0.0;
                for k in self.ref_start[i] as usize..self.ref_start[i + 1] as usize {
                    let sent = ranks[self.ref_file[k] as usize] * f64::from(self.ref_w[k]);
                    match self.ref_near[k] {
                        ALL => even += sent,
                        near => out[base + near as usize] += sent,
                    }
                }
                let even = even / definers as f64;
                out[base..].iter_mut().for_each(|x| *x += even);
            }
            out
        })
    }

    /// PageRank of the files, teleporting by `personal` (which sums to 1).
    /// The rank of files with no references follows the teleport too.
    fn rank(&self, personal: &[f64]) -> Vec<f64> {
        let mut ranks = personal.to_vec();
        for _ in 0..MAX_ITERATIONS {
            let flows = self.flows(&ranks);
            let dangling: f64 = self
                .dangling
                .iter()
                .map(|&(f, share)| ranks[f as usize] * f64::from(share))
                .sum();
            let next: Vec<f64> = par_ranges(self.files(), RANK_CHUNK, |range| {
                range
                    .map(|f| {
                        let inflow: f64 = self.slots_of(f).iter().map(|&k| flows[k as usize]).sum();
                        (1.0 - DAMPING) * personal[f] + DAMPING * (inflow + dangling * personal[f])
                    })
                    .collect()
            });
            let change: f64 = next.iter().zip(&ranks).map(|(a, b)| (a - b).abs()).sum();
            ranks = next;
            if change < TOLERANCE {
                break;
            }
        }
        ranks
    }

    /// Every symbol but `impl` blocks, scored by the rank its file receives
    /// through its name, split among the file's symbols of that name, and
    /// less for a private one (see [`PRIVATE_WEIGHT`]). The
    /// symbols of `focus` files count in full even when nothing uses them;
    /// those of test code count a tenth.
    fn score_symbols(
        &self,
        names: &[Arc<SymbolNames>],
        ranks: &[f64],
        tests: &TestCode,
        focus: &BTreeSet<usize>,
    ) -> Vec<Candidate> {
        let flows = self.flows(ranks);
        par_ranges(names.len(), 256, |range| {
            let mut out = Vec::new();
            for f in range {
                let n = &names[f];
                let defs = self.defs_of(f);
                let slots = self.slots_of(f);
                let ident_of = |s: usize| {
                    let id = self.ident(def_key(n, s, &self.units[f], &self.types)?)?;
                    defs.binary_search(&(id as u32)).ok()
                };
                let own = if focus.contains(&f) {
                    1.0
                } else {
                    UNREFERENCED
                };
                let narrow_private = !matches!(self.units[f].lang, Some(Lang::C | Lang::Cpp));
                let mut same_name = vec![0u32; defs.len()];
                for s in 0..n.len() {
                    if let Some(at) = ident_of(s) {
                        same_name[at] += 1;
                    }
                }
                for s in 0..n.len() {
                    let Some((kind, line)) = n.kind_and_line(s) else {
                        continue;
                    };
                    if kind == SymbolKind::Impl {
                        continue;
                    }
                    let narrow = if narrow_private && n.reach(s) == Reach::Private {
                        PRIVATE_WEIGHT
                    } else {
                        1.0
                    };
                    let referenced = ident_of(s).map_or(0.0, |at| {
                        flows[slots[at] as usize] / f64::from(same_name[at])
                    }) * narrow;
                    let nested = if n.parent(s).is_some() { 0.5 } else { 1.0 };
                    let test = if tests.is_test(n, f, s) {
                        TEST_WEIGHT
                    } else {
                        1.0
                    };
                    let score = referenced + own * ranks[f] * kind_weight(kind) * nested * test;
                    out.push(Candidate {
                        score,
                        file: f as u32,
                        symbol: s as u32,
                        line: line as u32,
                    });
                }
            }
            out
        })
    }
}

/// Which code is test code, whose definitions take no rank: files under a
/// test directory or named as tests, and Rust `mod tests` blocks. When every
/// file is a test file, as for a map of `tests/`, nothing is.
struct TestCode {
    files: Vec<bool>,
    any: bool,
}

impl TestCode {
    fn new(paths: &[String]) -> Self {
        let files: Vec<bool> = paths.iter().map(|p| is_test_path(p)).collect();
        let any = files.iter().any(|&t| !t);
        TestCode { files, any }
    }

    /// No test code at all.
    #[cfg(test)]
    fn none(files: usize) -> Self {
        TestCode {
            files: vec![false; files],
            any: false,
        }
    }

    fn is_test(&self, n: &SymbolNames, file: usize, s: usize) -> bool {
        self.any
            && (self.files[file]
                || matches!(n.parent(s), Some("tests" | "test"))
                || (matches!(n.name(s), Some("tests" | "test"))
                    && n.kind_and_line(s)
                        .is_some_and(|(k, _)| k == SymbolKind::Module)))
    }
}

/// Whether the slash-separated `path` is test or benchmark code by the usual
/// conventions of the supported languages.
fn is_test_path(path: &str) -> bool {
    const DIRS: &[&str] = &[
        "test",
        "tests",
        "testing",
        "testdata",
        "selftests",
        "__tests__",
        "__mocks__",
        "fixtures",
        "e2e",
        "bench",
        "benches",
        "benchmarks",
    ];
    let (dirs, file) = path.rsplit_once('/').unwrap_or(("", path));
    if dirs.split('/').any(|d| DIRS.contains(&d)) {
        return true;
    }
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let camel_test = ["Test", "Tests"].iter().any(|suffix| {
        stem.strip_suffix(suffix)
            .and_then(|head| head.chars().last())
            .is_some_and(|c| c.is_lowercase() || c.is_ascii_digit())
    });
    camel_test
        || stem == "conftest"
        || stem.starts_with("test_")
        || [
            "_test", "_tests", ".test", ".spec", "_spec", "-test", "-spec",
        ]
        .iter()
        .any(|suffix| stem.ends_with(suffix))
}

/// How much `count` mentions of a name in one file count: the first counts
/// most, and repeats less and less (the BM25 term weight).
fn term_weight(count: u8) -> f64 {
    let c = f64::from(count);
    c * (TERM_SATURATION + 1.0) / (c + TERM_SATURATION)
}

/// How much a mention of `name` counts, defined `definers` times (in as many
/// files, or for as many owners).
fn multiplier_of(name: &str, definers: usize, focus: bool) -> f64 {
    let mut m = 1.0;
    let mixed_case = name.chars().any(char::is_uppercase) && name.chars().any(char::is_lowercase);
    if name.chars().count() >= 8 && (name.contains('_') || mixed_case) {
        m *= 10.0;
    }
    // A one-letter name, `i` or `p`, is more likely some other variable.
    if name.starts_with('_') || name.chars().count() == 1 {
        m *= 0.1;
    }
    if definers > COMMON_DEFINERS {
        m *= 0.1;
    }
    if focus {
        m *= 10.0;
    }
    m
}

/// Order of symbols that nothing references: types say most about a file.
/// Modules count little: their members are shown with them.
fn kind_weight(kind: SymbolKind) -> f64 {
    use SymbolKind::*;
    match kind {
        Class | Struct | Enum | Interface | Trait | Type => 1.0,
        Function | Macro => 0.7,
        Method | Constant => 0.5,
        Variable => 0.3,
        Module => 0.2,
        Impl => 0.0,
    }
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    score: f64,
    file: u32,
    symbol: u32,
    line: u32,
}

/// The first path component of `path` under `prefix`: a directory with a
/// trailing `/`, or the file itself.
fn group_of(path: &Path, prefix: &Path) -> String {
    let rel = path.strip_prefix(prefix).unwrap_or(path);
    let mut parts = rel.components();
    let first = parts
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_default();
    if parts.next().is_some() {
        format!("{first}/")
    } else {
        first
    }
}

struct Renderer<'a> {
    root: &'a Path,
    cache: &'a ParseCache,
    slashed: &'a [String],
    groups: &'a [String],
    ranks: &'a [f64],
    has_symbols: Vec<bool>,
    shown_dir: &'a str,
    notes: &'a str,
}

/// A candidate checked against its parsed file.
#[derive(Debug, Clone, Copy)]
struct Picked {
    file: u32,
    symbol: u32,
}

impl Renderer<'_> {
    /// The most symbols, best first, whose rendering fits `max_tokens`.
    fn fit(&self, paths: &[PathBuf], mut candidates: Vec<Candidate>, max_tokens: usize) -> String {
        let order = |a: &Candidate, b: &Candidate| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| self.slashed[a.file as usize].cmp(&self.slashed[b.file as usize]))
                .then(a.line.cmp(&b.line))
        };
        let most = (max_tokens / MIN_LINE_TOKENS + 1).min(candidates.len());
        if candidates.len() > most && most > 0 {
            candidates.select_nth_unstable_by(most - 1, order);
        }
        candidates.truncate(most);
        candidates.sort_by(order);

        // Only the files of the best candidates are read, for signatures.
        let mut wanted: Vec<u32> = candidates.iter().map(|c| c.file).collect();
        wanted.sort_unstable();
        wanted.dedup();
        let read: Vec<Option<Arc<CachedFile>>> = par_ranges(wanted.len(), 8, |range| {
            range
                .map(|i| {
                    self.cache
                        .get(&self.root.join(&paths[wanted[i] as usize]))
                        .ok()
                })
                .collect()
        });
        let files: HashMap<u32, Arc<CachedFile>> = wanted
            .into_iter()
            .zip(read)
            .filter_map(|(f, file)| file.map(|file| (f, file)))
            .collect();
        // A file changed since its names were read may no longer hold the
        // symbol at that index; it is left out rather than misreported.
        let picked: Vec<Picked> = candidates
            .iter()
            .filter(|c| {
                files
                    .get(&c.file)
                    .and_then(|f| f.parsed.symbols.get(c.symbol as usize))
                    .is_some_and(|s| s.start_line == c.line as usize)
            })
            .map(|c| Picked {
                file: c.file,
                symbol: c.symbol,
            })
            .collect();

        let fits = |n: usize| estimate_tokens(&self.render(&picked[..n], &files)) <= max_tokens;
        let (mut lo, mut hi) = (0, picked.len());
        if fits(hi) {
            lo = hi;
        } else {
            // `lo` always fits (or is 0), `hi` never does.
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if fits(mid) {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
        }
        let mut text = self.render(&picked[..lo], &files);
        let max_bytes = max_tokens.saturating_mul(4);
        if text.len() > max_bytes {
            let mut cut = max_bytes;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        text
    }

    fn render(&self, picked: &[Picked], files: &HashMap<u32, Arc<CachedFile>>) -> String {
        let mut by_file: Vec<(u32, Vec<usize>)> = Vec::new();
        let mut sorted = picked.to_vec();
        sorted.sort_by(|a, b| {
            self.slashed[a.file as usize]
                .cmp(&self.slashed[b.file as usize])
                .then(a.symbol.cmp(&b.symbol))
        });
        for p in sorted {
            match by_file.last_mut() {
                Some((f, syms)) if *f == p.file => syms.push(p.symbol as usize),
                _ => by_file.push((p.file, vec![p.symbol as usize])),
            }
        }

        let mut body = String::new();
        for (f, syms) in &by_file {
            let Some(file) = files.get(f) else {
                continue;
            };
            let symbols = &file.parsed.symbols;
            let mut show = BTreeSet::new();
            for &s in syms {
                show.insert(s);
                ancestors(symbols, s, &mut show);
            }
            let _ = writeln!(body, "{}", self.slashed[*f as usize]);
            for s in show {
                let sym = &symbols[s];
                let indent = "  ".repeat(sym.depth + 1);
                let _ = writeln!(
                    body,
                    "{indent}{} [{}] L{}-{}",
                    sym.signature,
                    sym.kind.as_str(),
                    sym.start_line,
                    sym.end_line
                );
            }
        }

        let total = self.has_symbols.iter().filter(|&&s| s).count();
        let mut out = format!(
            "{}: {} in {} of {}, the ones other files use most\n",
            self.shown_dir,
            counted(picked.len(), "symbol"),
            by_file.len(),
            counted(total, "file"),
        );
        out.push_str(&body);
        out.push_str(&self.left_out(&by_file, total));
        out.push_str(self.notes);
        out
    }

    /// One line on the files with symbols that are not shown, by the top
    /// directories holding them, most important first.
    fn left_out(&self, by_file: &[(u32, Vec<usize>)], total: usize) -> String {
        let shown: BTreeSet<u32> = by_file.iter().map(|(f, _)| *f).collect();
        let hidden = total - shown.len();
        if hidden == 0 {
            return String::new();
        }
        let mut groups: HashMap<&str, (usize, f64)> = HashMap::new();
        for (f, has) in self.has_symbols.iter().enumerate() {
            if *has && !shown.contains(&(f as u32)) {
                let g = groups.entry(self.groups[f].as_str()).or_default();
                g.0 += 1;
                g.1 += self.ranks[f];
            }
        }
        let mut groups: Vec<(&str, usize, f64)> =
            groups.into_iter().map(|(g, (n, r))| (g, n, r)).collect();
        groups.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(b.0)));
        let mut line = format!("… {} not shown, most used first:", counted(hidden, "file"));
        for (i, (g, n, _)) in groups.iter().take(FOOTER_GROUPS).enumerate() {
            let sep = if i == 0 { " " } else { ", " };
            let _ = write!(line, "{sep}{g} ({n})");
        }
        if groups.len() > FOOTER_GROUPS {
            let _ = write!(line, " and {} more", groups.len() - FOOTER_GROUPS);
        }
        line.push_str("; narrow with dir or focus\n");
        line
    }
}

/// `n` and `word`, plural unless `n` is 1.
fn counted(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Add the symbols enclosing `symbols[at]` to `show`. Symbols are in source
/// order, so each encloser comes before what it encloses.
fn ancestors(symbols: &[Symbol], at: usize, show: &mut BTreeSet<usize>) {
    let target = &symbols[at];
    let mut depth = target.depth;
    for (i, s) in symbols[..at].iter().enumerate().rev() {
        if depth == 0 {
            break;
        }
        if s.depth < depth && s.start_byte <= target.start_byte && target.end_byte <= s.end_byte {
            show.insert(i);
            depth = s.depth;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, path: &str, text: &str) {
        let p = dir.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    /// `core.py` is used by every other file, `leaf_*.py` by none.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pkg/core.py",
            "class ConnectionPool:\n    def acquire_slot(self):\n        return 1\n\n\
             def unused_helper():\n    return 2\n",
        );
        for i in 0..6 {
            write(
                dir.path(),
                &format!("pkg/leaf_{i}.py"),
                &format!(
                    "from core import ConnectionPool\n\ndef leaf_task_{i}():\n    \
                     return ConnectionPool().acquire_slot()\n"
                ),
            );
        }
        write(
            dir.path(),
            "tools/report.py",
            "def render_report():\n    return leaf_task_0()\n",
        );
        dir
    }

    fn map(dir: &Path, opts: &MapOptions) -> String {
        let cache = ParseCache::default();
        codebase_map(dir, dir, &cache, opts)
    }

    fn opts(max_tokens: usize) -> MapOptions {
        MapOptions {
            max_tokens,
            ..MapOptions::default()
        }
    }

    #[test]
    fn the_most_used_symbols_come_first() {
        let dir = fixture();
        let out = map(dir.path(), &opts(60));
        assert!(
            out.contains("pkg/core.py\n  class ConnectionPool [class] L1-3\n"),
            "{out}"
        );
        assert!(
            out.contains("    def acquire_slot(self) [method] L2-3\n"),
            "{out}"
        );
        assert!(!out.contains("unused_helper"), "{out}");
        assert!(
            out.contains("… 7 files not shown, most used first: pkg/ (6), tools/ (1)"),
            "{out}"
        );
    }

    #[test]
    fn a_large_budget_shows_every_symbol_in_path_order() {
        let dir = fixture();
        let out = map(dir.path(), &opts(4000));
        assert!(out.starts_with(".: 10 symbols in 8 of 8 files"), "{out}");
        let core = out.find("pkg/core.py").unwrap();
        let leaf = out.find("pkg/leaf_0.py").unwrap();
        let tools = out.find("tools/report.py").unwrap();
        assert!(core < leaf && leaf < tools, "{out}");
        assert!(
            out.contains("  def unused_helper() [function] L5-6\n"),
            "{out}"
        );
        assert!(!out.contains("not shown"), "{out}");
    }

    #[test]
    fn the_output_fits_the_budget_and_does_not_change() {
        let dir = fixture();
        for budget in [1, 20, 45, 80, 150, 400] {
            let out = map(dir.path(), &opts(budget));
            assert!(estimate_tokens(&out) <= budget, "{budget}: {out}");
            assert_eq!(out, map(dir.path(), &opts(budget)), "{budget}");
        }
    }

    #[test]
    fn focus_ranks_around_a_file_or_an_identifier() {
        let dir = fixture();
        let plain = map(dir.path(), &opts(60));
        assert!(!plain.contains("render_report"), "{plain}");
        for focus in ["tools/report.py", "tools", "render_report"] {
            let out = map(
                dir.path(),
                &MapOptions {
                    focus: vec![focus.into()],
                    ..opts(60)
                },
            );
            assert!(
                out.contains("tools/report.py\n  def render_report()"),
                "{focus}: {out}"
            );
        }
        let out = map(
            dir.path(),
            &MapOptions {
                focus: vec!["no_such_thing".into()],
                ..opts(400)
            },
        );
        assert!(
            out.contains("focus \"no_such_thing\" matched no file"),
            "{out}"
        );
    }

    #[test]
    fn a_directory_maps_only_the_files_under_it() {
        let dir = fixture();
        let cache = ParseCache::default();
        let out = codebase_map(dir.path(), &dir.path().join("tools"), &cache, &opts(400));
        assert!(out.starts_with("tools: 1 symbol in 1 of 1 file,"), "{out}");
        assert!(!out.contains("pkg/"), "{out}");
    }

    #[test]
    fn ranks_sum_to_one_and_favour_the_shared_module() {
        let dir = fixture();
        let cache = ParseCache::default();
        let (files, _) = files_under(dir.path(), dir.path(), 100);
        let names: Vec<_> = files
            .iter()
            .map(|p| cache.symbol_names(&dir.path().join(p)).unwrap())
            .collect();
        let tests = TestCode::none(names.len());
        let slashed: Vec<String> = files.iter().map(|p| slash_path(p)).collect();
        let graph = Graph::build(&names, &slashed, &tests, &[]);
        let ranks = graph.rank(&personalization(names.len(), &BTreeSet::new()));
        // Edge weights are f32.
        let sum: f64 = ranks.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "{sum}");
        let candidates = graph.score_symbols(&names, &ranks, &tests, &BTreeSet::new());
        assert_eq!(candidates.len(), 10);
        let best = (0..ranks.len())
            .max_by(|&a, &b| ranks[a].total_cmp(&ranks[b]))
            .unwrap();
        assert_eq!(slash_path(&files[best]), "pkg/core.py");
    }

    #[test]
    fn a_private_symbol_scores_below_a_public_one_used_as_much() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("p/a.go", "package p\n\nfunc Open() {}\nfunc open() {}\n"),
            ("p/b.go", "package p\n\nfunc use() { Open(); open() }\n"),
        ];
        for (path, text) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let cache = ParseCache::default();
        let (paths, _) = files_under(dir.path(), dir.path(), 100);
        let names: Vec<_> = paths
            .iter()
            .map(|p| cache.symbol_names(&dir.path().join(p)).unwrap())
            .collect();
        let tests = TestCode::none(names.len());
        let slashed: Vec<String> = paths.iter().map(|p| slash_path(p)).collect();
        let graph = Graph::build(&names, &slashed, &tests, &[]);
        let ranks = graph.rank(&personalization(names.len(), &BTreeSet::new()));
        let candidates = graph.score_symbols(&names, &ranks, &tests, &BTreeSet::new());
        let a = slashed.iter().position(|p| p == "p/a.go").unwrap() as u32;
        let score = |s: u32| {
            candidates
                .iter()
                .find(|c| c.file == a && c.symbol == s)
                .unwrap()
                .score
        };
        assert!(score(1) < score(0) * 0.5, "{} {}", score(1), score(0));
    }

    fn graph_of(dir: &Path, files: &[(&str, &str)]) -> (Graph, Vec<f64>) {
        for (path, text) in files {
            write(dir, path, text);
        }
        let cache = ParseCache::default();
        let names: Vec<_> = files
            .iter()
            .map(|(p, _)| cache.symbol_names(&dir.join(p)).unwrap())
            .collect();
        let paths: Vec<String> = files.iter().map(|(p, _)| p.to_string()).collect();
        let graph = Graph::build(&names, &paths, &TestCode::new(&paths), &[]);
        let ranks = graph.rank(&personalization(files.len(), &BTreeSet::new()));
        assert!((ranks.iter().sum::<f64>() - 1.0).abs() < 1e-6, "{ranks:?}");
        (graph, ranks)
    }

    /// The files `files[f]` sends rank to, by name of the file.
    fn targets(graph: &Graph, files: &[(&str, &str)], f: usize) -> BTreeSet<String> {
        (0..graph.idents())
            .flat_map(|i| {
                let refs = graph.ref_start[i] as usize..graph.ref_start[i + 1] as usize;
                let definers = graph.definers(i);
                refs.filter(move |&k| graph.ref_file[k] == f as u32)
                    .flat_map(move |k| match graph.ref_near[k] {
                        ALL => definers,
                        near => std::slice::from_ref(&definers[near as usize]),
                    })
            })
            .map(|&d| files[d as usize].0.to_string())
            .collect()
    }

    #[test]
    fn a_member_counts_only_with_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            (
                "cache.rs",
                "pub struct Cache;\nimpl Cache {\n    pub fn get_entry(&self) {}\n}\n",
            ),
            ("other.rs", "pub fn get_entry() {}\n"),
            ("uses_cache.rs", "fn f(c: Cache) { c.get_entry() }\n"),
            ("uses_value.rs", "fn g(m: Map) { m.get_entry() }\n"),
            ("uses_free.rs", "fn h() { get_entry() }\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        assert_eq!(targets(&graph, &files, 2), ["cache.rs".to_string()].into());
        // A method of some other type: neither `Cache`'s nor the free one.
        assert!(targets(&graph, &files, 3).is_empty());
        assert_eq!(targets(&graph, &files, 4), ["other.rs".to_string()].into());
    }

    #[test]
    fn a_rust_macro_is_used_by_calling_it() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("macros.rs", "macro_rules! feature { () => {} }\n"),
            ("attr.rs", "#[cfg(feature = \"x\")]\npub fn a() {}\n"),
            ("call.rs", "pub fn b() { feature!(); }\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        assert!(targets(&graph, &files, 1).is_empty());
        assert_eq!(targets(&graph, &files, 2), ["macros.rs".to_string()].into());
    }

    #[test]
    fn the_name_of_a_definition_is_no_use_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("queue.rs", "pub fn make_pair() {}\n"),
            (
                "impl.rs",
                "pub struct S;\nimpl S {\n    pub fn make_pair() {}\n}\n",
            ),
            ("call.rs", "fn f() { queue::make_pair() }\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        // `impl.rs` names `make_pair` only to define its own.
        assert!(!targets(&graph, &files, 1).contains("queue.rs"));
        assert!(targets(&graph, &files, 2).contains("queue.rs"));
    }

    #[test]
    fn an_item_of_a_trait_impl_on_a_foreign_type_is_owned_by_the_trait() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            (
                "store.rs",
                "pub trait Store { fn each_item(&self); }\nimpl Store for Vec<u8> {\n    fn each_item(&self) {}\n}\n",
            ),
            ("vec_user.rs", "fn f(v: Vec<u8>) { v.each_item() }\n"),
            (
                "store_user.rs",
                "use crate::Store;\nfn g(v: Vec<u8>) { v.each_item() }\n",
            ),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        // `Vec` is not the workspace's, so naming it says nothing.
        assert!(targets(&graph, &files, 1).is_empty());
        assert_eq!(targets(&graph, &files, 2), ["store.rs".to_string()].into());
    }

    #[test]
    fn private_definitions_reach_only_their_scope() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("src/lib.rs", "mod a;\nfn root_helper() {}\n"),
            (
                "src/a.rs",
                "mod b;\nfn a_helper() {}\nfn f() { root_helper() }\n",
            ),
            ("src/a/b.rs", "fn g() { a_helper(); root_helper() }\n"),
            ("src/c.rs", "fn h() { a_helper() }\n"),
            ("go/p/x.go", "package p\n\nfunc helper() {}\n"),
            ("go/p/y.go", "package p\n\nfunc Y() { helper() }\n"),
            ("go/p/q/z.go", "package q\n\nfunc Z() { helper() }\n"),
            ("c/x.c", "static int helper(void) { return 0; }\n"),
            ("c/y.c", "int y(void) { return helper(); }\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        let to = |f| targets(&graph, &files, f);
        assert_eq!(to(1), ["src/lib.rs".to_string()].into());
        assert_eq!(
            to(2),
            ["src/a.rs".to_string(), "src/lib.rs".to_string()].into()
        );
        assert!(to(3).is_empty(), "a_helper is private to the module a");
        assert_eq!(to(5), ["go/p/x.go".to_string()].into());
        assert!(to(6).is_empty(), "helper is private to the package p");
        assert!(to(8).is_empty(), "a static function is private to its file");
    }

    #[test]
    fn uses_follow_languages_modules_and_exports() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("go/p/p.go", "package p\n\nfunc ExportedFn() {}\n"),
            ("go/q/q.go", "package q\n\nfunc Q() { p.ExportedFn() }\n"),
            ("go/r/r.go", "package r\n\nfunc R() { ExportedFn() }\n"),
            ("go/p/same.go", "package p\n\nfunc S() { ExportedFn() }\n"),
            ("go/p/use.go", "package p\n\nfunc U() { sharedApi() }\n"),
            (
                "web/strings.js",
                "export function sharedApi() {}\nfunction hiddenOne() {}\n",
            ),
            (
                "web/a.js",
                "import * as strings from './strings';\nstrings.sharedApi();\n",
            ),
            ("web/b.js", "x.sharedApi();\n"),
            ("web/c.js", "hiddenOne();\n"),
            ("py/pkg/mod.py", "def py_helper():\n    pass\n"),
            (
                "py/user.py",
                "from pkg.mod import py_helper\n\npy_helper()\n",
            ),
            ("py/other.py", "py_helper()\n"),
            (
                "py/loc.py",
                "def outer():\n    def inner_fn():\n        pass\n",
            ),
            ("py/loc_user.py", "from loc import inner_fn\n\ninner_fn()\n"),
            ("c/x.c", "#define LOCAL_MAC 1\n"),
            ("c/y.c", "int y = LOCAL_MAC;\n"),
            ("c/h.h", "#define HDR_MAC 1\n"),
            ("c/z.c", "int z = HDR_MAC;\n"),
            (
                "py/shape.py",
                "class Shape:\n    def area_of(self):\n        pass\n",
            ),
            (
                "py/area.py",
                "from shape import Shape\n\nShape().area_of()\n",
            ),
            ("py/plain.py", "print(Shape, area_of)\n"),
            ("py/copy.py", "def copy(x):\n    return x\n"),
            ("py/val.py", "def f(s):\n    return s.copy()\n"),
            ("py/dup.py", "import copy\n\ncopy.copy(1)\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        let to = |f| targets(&graph, &files, f);
        let only = |path: &str| BTreeSet::from([path.to_string()]);
        assert_eq!(to(1), only("go/p/p.go"), "qualified by its package");
        assert!(to(2).is_empty(), "another package's, unqualified");
        assert_eq!(to(3), only("go/p/p.go"), "in the same package");
        assert!(to(4).is_empty(), "a JavaScript function is not Go's");
        assert_eq!(to(6), only("web/strings.js"), "a member of its module");
        assert!(to(7).is_empty(), "a member of some value");
        assert!(to(8).is_empty(), "not exported");
        assert_eq!(to(10), only("py/pkg/mod.py"), "imported from its module");
        assert!(to(11).is_empty(), "its module is never named");
        assert!(to(13).is_empty(), "local to a function");
        assert!(to(15).is_empty(), "a macro of a source file");
        assert_eq!(to(17), only("c/h.h"), "a macro of a header");
        assert_eq!(to(19), only("py/shape.py"));
        assert!(to(20).is_empty(), "a method not called on a value");
        assert!(to(22).is_empty(), "a method of some value");
        assert_eq!(to(23), only("py/copy.py"), "named with its module");
    }

    #[test]
    fn a_package_or_module_names_what_it_defines() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            (
                "go/unsafe/unsafe.go",
                "package unsafe\n\nfunc String() {}\n",
            ),
            (
                "go/a/a.go",
                "package a\n\nimport \"unsafe\"\n\nfunc f() { unsafe.Pointer(x); x.String() }\n",
            ),
            (
                "go/b/b.go",
                "package b\n\nimport u \"unsafe\"\n\nfunc f() { u.String(p) }\n",
            ),
            ("py/types.py", "GenericAlias = type(list[int])\n"),
            ("py/os.py", "GenericAlias = type(list[int])\n"),
            (
                "py/user.py",
                "import os\nfrom types import GenericAlias\nx = GenericAlias\n",
            ),
            ("py/parser.py", "def parse_args():\n    pass\n"),
            (
                "py/cli.py",
                "import argparse\nparser = argparse.ArgumentParser()\nparser.parse_args()\n",
            ),
            ("py/star.py", "from parser import *\nparse_args()\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        let only = |path: &str| BTreeSet::from([path.to_string()]);
        assert!(
            targets(&graph, &files, 1).is_empty(),
            "a method, not the package's"
        );
        assert_eq!(targets(&graph, &files, 2), only("go/unsafe/unsafe.go"));
        assert_eq!(targets(&graph, &files, 5), only("py/types.py"));
        assert!(
            targets(&graph, &files, 7).is_empty(),
            "a variable named parser"
        );
        assert_eq!(targets(&graph, &files, 8), only("py/parser.py"));
    }

    #[test]
    fn build_alternatives_take_one_definition() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            (
                "syscall/syscall_js.go",
                "package syscall\n\ntype Errno uintptr\n",
            ),
            (
                "syscall/syscall_unix.go",
                "package syscall\n\ntype Errno uintptr\n",
            ),
            (
                "syscall/syscall_windows.go",
                "package syscall\n\ntype Errno uintptr\n",
            ),
            (
                "os/error.go",
                "package os\n\nimport \"syscall\"\n\nvar e syscall.Errno\n",
            ),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        let to = targets(&graph, &files, 3);
        assert_eq!(
            to.into_iter().collect::<Vec<_>>(),
            ["syscall/syscall_unix.go"],
            "the common build"
        );
        assert_eq!(build_rank("syscall/syscall_unix.go"), 0);
        assert_eq!(build_rank("internal/cpu/cpu_no_name.go"), 2);
    }

    #[test]
    fn an_export_counts_where_its_module_is_imported() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            (
                "web/schemes.d.ts",
                "declare const fileScheme: string;\ndeclare const otherScheme: string;\n",
            ),
            ("web/other.ts", "export const otherScheme = 'other';\n"),
            (
                "web/user.ts",
                "import { otherScheme } from './other';\n\n\
                 f(otherScheme, fileScheme, modScheme, pageScheme);\n",
            ),
            ("web/mod-scheme.ts", "export const modScheme = 1;\n"),
            ("web/page.js", "const pageScheme = 'page';\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        let js = family(Some(Lang::TypeScript));
        let weight_of = |name: &str, module: &str| {
            let owner = if module.is_empty() {
                NO_OWNER
            } else {
                mentions::import_hash(module)
            };
            let id = graph.ident((mentions::hash(name), owner, js)).unwrap();
            let refs = graph.ref_start[id] as usize..graph.ref_start[id + 1] as usize;
            refs.filter(|&k| graph.ref_file[k] == 2)
                .map(|k| graph.ref_w[k])
                .sum::<f32>()
        };
        // A declared global counts less unimported; a module's export, or
        // the global of a script that declares nothing, not at all.
        let imported = weight_of("otherScheme", "other");
        let global = weight_of("fileScheme", "");
        assert!(
            imported > 5.0 * global && global > 0.0,
            "{imported} vs {global}"
        );
        assert_eq!(weight_of("modScheme", "mod-scheme"), 0.0);
        assert_eq!(weight_of("pageScheme", ""), 0.0);
        assert_eq!(weight_of("otherScheme", ""), 0.0, "hidden by the import");
    }

    #[test]
    fn an_import_picks_one_of_the_modules_exporting_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("a/state.ts", "export type URI = string;\n"),
            ("base/uri.ts", "export class URI {}\n"),
            (
                "x/user.ts",
                "import { URI } from '../base/uri';\nlet u: URI;\n",
            ),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        assert_eq!(
            targets(&graph, &files, 2),
            BTreeSet::from(["base/uri.ts".to_string()])
        );
    }

    #[test]
    fn a_relative_import_names_one_of_the_modules_with_its_stem() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("p/v1/state.ts", "export interface State {}\n"),
            ("p/v2/state.ts", "export interface State {}\n"),
            (
                "p/v1/user.ts",
                "import { State } from '../v2/state.js';\nlet s: State;\n",
            ),
            ("p/v3/state.ts", "export interface State {}\n"),
            (
                "p/v3/other.ts",
                "import { x } from './a/state';\nlet s: State;\n",
            ),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        let only = |path: &str| BTreeSet::from([path.to_string()]);
        assert_eq!(targets(&graph, &files, 2), only("p/v2/state.ts"));
        assert!(targets(&graph, &files, 4).is_empty(), "another state.ts");
        assert_eq!(import_path("p/v1/user.ts", "p/v2/state.ts"), "../v2/state");
        assert_eq!(import_path("p/user.ts", "p/a/index.ts"), "./a");
        assert_eq!(import_path("user.ts", "state.ts"), "./state");
    }

    #[test]
    fn a_declared_module_is_used_where_its_package_is_imported() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            (
                "dts/vscode.d.ts",
                "declare module 'vscode' {\n\texport interface Variable {}\n}\n",
            ),
            (
                "ext/a.ts",
                "import * as vscode from 'vscode';\nlet v: vscode.Variable;\n",
            ),
            ("src/b.ts", "import { x } from './x';\nlet v: Variable;\n"),
            ("src/c.ts", "let v: Variable;\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        let only = |path: &str| BTreeSet::from([path.to_string()]);
        assert_eq!(targets(&graph, &files, 1), only("dts/vscode.d.ts"));
        assert!(targets(&graph, &files, 2).is_empty(), "not imported");
        assert!(
            targets(&graph, &files, 3).is_empty(),
            "a script, not imported"
        );
    }

    #[test]
    fn a_name_means_the_definition_nearest_its_use() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("src/base/uri.rs", "pub struct Uri;\n"),
            ("vendor/copy/base/uri.rs", "pub struct Uri;\n"),
            ("src/app/user.rs", "fn f(u: Uri) {}\n"),
            ("vendor/copy/user.rs", "fn f(u: Uri) {}\n"),
            ("other/user.rs", "fn f(u: Uri) {}\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        let set = |paths: &[&str]| paths.iter().map(|p| p.to_string()).collect::<BTreeSet<_>>();
        assert_eq!(targets(&graph, &files, 2), set(&["src/base/uri.rs"]));
        assert_eq!(
            targets(&graph, &files, 3),
            set(&["vendor/copy/base/uri.rs"])
        );
        assert_eq!(
            targets(&graph, &files, 4),
            set(&["src/base/uri.rs", "vendor/copy/base/uri.rs"]),
            "as near to both"
        );
    }

    #[test]
    fn typings_declare_globals_for_their_project() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            (
                "ext/src/typings/node.d.ts",
                "declare interface Property {}\n",
            ),
            ("ext/src/a.ts", "export const a = (p: Property) => p;\n"),
            ("other/b.ts", "export const b = (p: Property) => p;\n"),
            ("other/c.js", "var c = new Property();\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        let only = BTreeSet::from(["ext/src/typings/node.d.ts".to_string()]);
        assert_eq!(targets(&graph, &files, 1), only);
        assert!(targets(&graph, &files, 2).is_empty());
        assert!(targets(&graph, &files, 3).is_empty());
        assert_eq!(typings_root("ext/src/typings"), Some("ext/src/".into()));
        assert_eq!(typings_root("@types/node"), Some(String::new()));
        assert_eq!(typings_root("src/vscode-dts"), None);
    }

    #[test]
    fn short_names_count_less() {
        assert!(multiplier_of("i", 1, false) < multiplier_of("ix", 1, false));
        assert!(multiplier_of("_x", 1, false) < multiplier_of("ix", 1, false));
        assert!(multiplier_of("parse_config", 1, false) > multiplier_of("parse", 1, false));
    }

    #[test]
    fn test_files_follow_each_language() {
        for path in [
            "tests/jsonrpc.rs",
            "src/a/__tests__/x.ts",
            "net/http/server_test.go",
            "test_os.py",
            "conftest.py",
            "a/strings.spec.ts",
            "a/button.test.tsx",
            "src/FooTest.java",
            "src/FooTests.cs",
            "benches/report.rs",
            "tools/testing/selftests/x.c",
        ] {
            assert!(is_test_path(path), "{path}");
        }
        for path in [
            "src/latest.rs",
            "src/attest.go",
            "src/Test.java",
            "src/contest_entry.py",
            "src/TestUtils/main.ts",
        ] {
            assert!(!is_test_path(path), "{path}");
        }
    }

    #[test]
    fn test_code_takes_no_rank() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("src/lib.rs", "pub fn real_helper() {}\n"),
            (
                "src/other.rs",
                "#[cfg(test)]\nmod tests {\n    pub fn test_helper() {}\n}\n",
            ),
            (
                "src/user.rs",
                "fn f() { real_helper(); test_helper(); fixture_one(); }\n",
            ),
            ("tests/common.rs", "pub fn fixture_one() {}\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        assert_eq!(
            targets(&graph, &files, 2),
            BTreeSet::from(["src/lib.rs".to_string()])
        );
    }

    #[test]
    fn a_c_struct_is_used_by_its_tag() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("lib.h", "struct buf { int a; };\n"),
            ("var.c", "int f(void) { int buf = 0; return buf; }\n"),
            ("tag.c", "int g(struct buf *b) { return b->a; }\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        assert!(targets(&graph, &files, 1).is_empty());
        assert_eq!(
            targets(&graph, &files, 2),
            BTreeSet::from(["lib.h".to_string()])
        );
    }

    #[test]
    fn a_header_reaches_the_code_around_it_and_its_includers() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("drivers/a/a.h", "#define A_LIMIT 4\n"),
            ("include/linux/l.h", "#define L_MAX 1\n"),
            ("drivers/a/a.c", "int f(void) { return A_LIMIT; }\n"),
            ("fs/b.c", "int g(void) { return A_LIMIT + L_MAX; }\n"),
            (
                "fs/c.c",
                "#include \"../drivers/a/a.h\"\nint h(void) { return A_LIMIT; }\n",
            ),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        let set = |paths: &[&str]| paths.iter().map(|p| p.to_string()).collect::<BTreeSet<_>>();
        assert_eq!(targets(&graph, &files, 2), set(&["drivers/a/a.h"]));
        assert_eq!(targets(&graph, &files, 3), set(&["include/linux/l.h"]));
        assert_eq!(targets(&graph, &files, 4), set(&["drivers/a/a.h"]));
    }

    #[test]
    fn a_large_tree_is_mapped_from_its_shallowest_files() {
        let files: Vec<PathBuf> = [
            "src/arch/a/x.c",
            "src/arch/b/y.c",
            "src/drivers/a/x.c",
            "src/drivers/a/y.c",
            "src/drivers/b/x.c",
            "src/include/linux/list.h",
            "src/kernel/fork.c",
            "src/kernel/sched/core.c",
            "src/main.c",
            "src/mm/mm.c",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let kept = |keep| -> Vec<String> {
            shallowest(files.clone(), Path::new("src"), keep)
                .iter()
                .map(|p| slash_path(p))
                .collect()
        };
        assert_eq!(kept(10).len(), 10);
        assert_eq!(kept(3), ["src/kernel/fork.c", "src/main.c", "src/mm/mm.c"]);
        assert_eq!(
            kept(6),
            [
                "src/arch/a/x.c",
                "src/drivers/a/x.c",
                "src/include/linux/list.h",
                "src/kernel/fork.c",
                "src/main.c",
                "src/mm/mm.c",
            ]
        );
    }

    #[test]
    fn scopes_follow_modules_packages_and_headers() {
        let within = |prefix: &str, nested| Scope::Within {
            prefix: prefix.into(),
            nested,
        };
        assert_eq!(
            Scope::of(Some(Lang::Rust), "src/lib.rs"),
            within("src/", true)
        );
        assert_eq!(
            Scope::of(Some(Lang::Rust), "src/a/mod.rs"),
            within("src/a/", true)
        );
        assert_eq!(
            Scope::of(Some(Lang::Rust), "src/a/b.rs"),
            within("src/a/b/", true)
        );
        assert_eq!(Scope::of(Some(Lang::Rust), "main.rs"), within("", true));
        assert_eq!(Scope::of(Some(Lang::Go), "p/x.go"), within("p/", false));
        assert_eq!(
            Scope::of(Some(Lang::Python), "pkg/m.py"),
            within("pkg/", true)
        );
        assert_eq!(Scope::of(Some(Lang::C), "inc/x.h"), within("inc/", true));
        assert_eq!(
            Scope::of(Some(Lang::C), "include/linux/x.h"),
            within("", true)
        );
        assert_eq!(
            Scope::of(Some(Lang::Cpp), "arch/x86/include/asm/x.hpp"),
            within("arch/x86/", true)
        );
        assert_eq!(
            Scope::of(Some(Lang::C), "Python/Include/internal/x.h"),
            within("Python/", true)
        );
        assert_eq!(Scope::of(Some(Lang::C), "x.c"), Scope::File);
        assert_eq!(Scope::of(Some(Lang::TypeScript), "x.ts"), Scope::File);
        assert!(within("p/", false).reaches("p/y.go"));
        assert!(!within("p/", false).reaches("p/q/z.go"));
        assert!(within("src/", true).reaches("src/a/b.rs"));
        assert!(!within("src/a/", true).reaches("src/ab.rs"));
        assert!(within("", false).reaches("x.go"));
        assert!(!Scope::File.reaches("x.c"));
    }

    #[test]
    fn units_key_each_language_and_name_its_module() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("p/x.go", "package p\n"),
            ("p/x.py", "x = 1\n"),
            ("p/__init__.py", "y = 1\n"),
            ("main.rs", "fn main() {}\n"),
            ("x.c", "int x;\n"),
            ("x.h", "int x;\n"),
            ("src/strings.test.ts", "export const s = 1;\n"),
            ("src/index.ts", "export const t = 1;\n"),
            ("v/w.go", "package other_name\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        let unit = |path: &str| {
            let f = files.iter().position(|(p, _)| *p == path).unwrap();
            &graph.units[f]
        };
        let h = mentions::hash;
        assert_ne!(unit("p/x.go").private_key, unit("p/x.py").private_key);
        assert_ne!(unit("p/x.go").public_key, unit("p/x.py").public_key);
        let rust = unit("main.rs").private_key;
        assert!(!is_public(rust) && rust != FILE_ONLY);
        assert_eq!(unit("x.c").private_key, FILE_ONLY);
        assert_eq!(unit("x.h").public_key, unit("x.c").public_key);
        assert_eq!(
            unit("x.h").key(Reach::Public),
            Some(unit("x.h").private_key)
        );
        assert!(!is_public(unit("x.h").private_key));
        assert_eq!(unit("x.h").header, mentions::import_hash("x"));
        assert_eq!(unit("x.c").header, NO_OWNER);
        assert_eq!(unit("src/strings.test.ts").private_key, FILE_ONLY);
        assert_eq!(
            unit("src/strings.test.ts").public_key,
            unit("src/index.ts").public_key
        );

        assert_eq!(unit("p/x.go").module, h("p"));
        assert_eq!(unit("v/w.go").module, h("other_name"));
        assert_eq!(
            (unit("p/x.py").module, unit("p/x.py").package),
            (h("x"), h("p"))
        );
        assert_eq!(
            (unit("p/__init__.py").module, unit("p/__init__.py").package),
            (h("p"), NO_OWNER)
        );
        let imported = mentions::import_hash;
        assert_eq!(unit("src/strings.test.ts").module, imported("strings"));
        assert_eq!(unit("src/index.ts").module, imported("src"));
        assert_eq!(unit("src/index.ts").module_owner(), imported("src"));
        assert_eq!(unit("main.rs").module, NO_OWNER);
    }

    #[test]
    fn a_file_using_a_name_it_defines_means_its_own_definition() {
        let dir = tempfile::tempdir().unwrap();
        let (_, ranks) = graph_of(
            dir.path(),
            &[
                ("a.js", "function shared_name() { return shared_name(); }\n"),
                ("b.js", "function shared_name() { return 1; }\n"),
                ("c.js", "function other() { return 2; }\n"),
            ],
        );
        // Nothing reaches b.py from a.py, so every file keeps an even rank.
        for r in &ranks {
            assert!((r - 1.0 / 3.0).abs() < 1e-9, "{ranks:?}");
        }

        let dir = tempfile::tempdir().unwrap();
        let (_, ranks) = graph_of(
            dir.path(),
            &[
                ("a.js", "function shared_name() { return shared_name(); }\n"),
                ("b.js", "function shared_name() { return 1; }\n"),
                ("c.js", "function other() { return shared_name(); }\n"),
            ],
        );
        // c.py's mention is split evenly between the two definers.
        assert!((ranks[0] - ranks[1]).abs() < 1e-9, "{ranks:?}");
        assert!(ranks[0] > ranks[2], "{ranks:?}");
    }

    #[test]
    fn a_name_every_file_mentions_carries_less_than_a_rare_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = vec![
            (
                "lib.js",
                "export function everywhere() {}\nexport function rare() {}\n",
            ),
            (
                "user.js",
                "import { everywhere, rare } from './lib';\n\
                 function main() { everywhere(); rare(); }\n",
            ),
        ];
        let others: Vec<String> = (0..6).map(|i| format!("other_{i}.js")).collect();
        for path in &others {
            files.push((
                path,
                "import { everywhere } from './lib';\nfunction f() { everywhere(); }\n",
            ));
        }
        let (graph, _) = graph_of(dir.path(), &files);
        let (js, lib) = (family(Some(Lang::JavaScript)), mentions::import_hash("lib"));
        let weight_of = |name: &str| {
            let id = graph.ident((mentions::hash(name), lib, js)).unwrap();
            let refs = graph.ref_start[id] as usize..graph.ref_start[id + 1] as usize;
            refs.filter(|&k| graph.ref_file[k] == 1)
                .map(|k| graph.ref_w[k])
                .sum::<f32>()
        };
        let (everywhere, rare) = (weight_of("everywhere"), weight_of("rare"));
        assert!((everywhere + rare - 1.0).abs() < 1e-6);
        assert!(rare > 4.0 * everywhere, "{everywhere} vs {rare}");
    }

    #[test]
    fn a_file_with_only_weak_uses_keeps_most_of_its_rank() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("z.js", "var z = 1;\n"),
            ("weak.js", "f(z);\n"),
            ("names.js", "var wellNamedThing = 1;\n"),
            ("strong.js", "f(wellNamedThing);\n"),
        ];
        let (graph, _) = graph_of(dir.path(), &files);
        let share = |f: u32| {
            graph
                .dangling
                .iter()
                .find(|d| d.0 == f)
                .map_or(0.0, |d| d.1)
        };
        assert!(share(1) > 0.8, "{:?}", graph.dangling);
        assert_eq!(share(3), 0.0, "{:?}", graph.dangling);
    }

    #[test]
    fn ancestors_are_shown_for_context() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "lib.rs",
            "pub mod outer {\n    pub struct Holder;\n    impl Holder {\n        \
             pub fn deep_method_name(&self) {}\n    }\n}\n",
        );
        write(
            dir.path(),
            "user.rs",
            "fn main() { outer::Holder.deep_method_name(); deep_method_name(); }\n",
        );
        let out = map(dir.path(), &opts(4000));
        assert!(
            out.contains("lib.rs\n  pub mod outer [module] L1-6\n"),
            "{out}"
        );
        assert!(
            out.contains(
                "    impl Holder [impl] L3-5\n      pub fn deep_method_name(&self) [method] L4-4\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn no_source_files_says_so() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "notes.txt", "hello\n");
        let out = map(dir.path(), &opts(400));
        assert_eq!(out, "no symbols in source files under .\n");
    }
}
