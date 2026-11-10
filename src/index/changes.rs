//! Symbols touched by a git diff: which definitions were added, modified or
//! removed, instead of raw hunks.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use super::text;
use crate::ast::parser::{self, Lang, ParsedFile, SymbolKind};
use crate::git;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Added,
    Modified,
    Removed,
}

impl Change {
    pub fn as_str(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Modified => "modified",
            Change::Removed => "removed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolChange {
    pub change: Change,
    pub kind: SymbolKind,
    pub qualified_name: String,
    /// Line range on the new side, or on the old side for removed symbols.
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChanges {
    pub path: String,
    pub symbols: Vec<SymbolChange>,
    /// Changed lines that lie outside every symbol (imports, top-level code,
    /// comments), or every changed line when the file is not parsed.
    pub other_lines: usize,
    /// False when the language is unsupported or a side could not be read.
    pub parsed: bool,
}

/// Where the two sides of the comparison come from.
#[derive(Debug, Clone, Copy)]
pub struct DiffSpec<'a> {
    /// Revision of the old side, e.g. `HEAD` or `main`.
    pub base: &'a str,
    /// Compare the index instead of the working tree with `base`.
    pub staged: bool,
}

/// Symbols added, modified or removed under `root` between `spec.base` and
/// the working tree (or index), one entry per changed file, sorted by path.
/// Untracked files count as added when comparing the working tree.
pub fn changed_symbols(root: &Path, spec: DiffSpec<'_>) -> Result<Vec<FileChanges>> {
    let mut diffs = git::diff(root, spec.base, spec.staged)?;
    if !spec.staged {
        for path in git::untracked(root)? {
            let lines = count_lines(&root.join(&path));
            diffs.push(git::FileDiff {
                path,
                old: Vec::new(),
                new: vec![(1, lines)],
                added: true,
                deleted: false,
            });
        }
    }
    let mut out: Vec<FileChanges> = diffs
        .iter()
        .map(|d| file_changes(root, spec, d))
        .collect::<Result<_>>()?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The lines of the file at `path`, counted in pieces so that a large
/// untracked file, such as a data set, is never held in memory. 0 for a file
/// that cannot be read or is binary.
fn count_lines(path: &Path) -> usize {
    use std::io::Read as _;

    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };
    let mut buf = vec![0u8; 64 * 1024];
    let (mut lines, mut first, mut ends_in_break) = (0, true, true);
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return 0,
        };
        let chunk = &buf[..n];
        // The same sniff as a text search: a NUL near the start is a binary.
        if std::mem::take(&mut first) && chunk[..n.min(text::BINARY_SNIFF_BYTES)].contains(&0) {
            return 0;
        }
        lines += memchr::memchr_iter(b'\n', chunk).count();
        ends_in_break = chunk.ends_with(b"\n");
    }
    // A last line without a line break is a line too.
    lines + usize::from(!ends_in_break)
}

fn file_changes(root: &Path, spec: DiffSpec<'_>, diff: &git::FileDiff) -> Result<FileChanges> {
    let changed_lines: usize = diff.old.iter().chain(&diff.new).map(|&(_, n)| n).sum();
    let unparsed = FileChanges {
        path: diff.path.clone(),
        symbols: Vec::new(),
        other_lines: changed_lines,
        parsed: false,
    };
    let path = Path::new(&diff.path);
    if Lang::from_path(path).is_none() {
        return Ok(unparsed);
    }
    let old = if diff.added {
        Some(String::new())
    } else {
        git::show(root, spec.base, &diff.path)?
    };
    let new = if diff.deleted {
        Some(String::new())
    } else if spec.staged {
        git::show(root, "", &diff.path)?
    } else {
        text::read_text(&root.join(&diff.path)).ok()
    };
    let (Some(old), Some(new)) = (old, new) else {
        return Ok(unparsed);
    };
    // The same bound as every other tool: past it parsing costs too much.
    if old.len().max(new.len()) as u64 > text::MAX_FILE_BYTES {
        return Ok(unparsed);
    }
    let (old_source, new_source) = (old, new);
    let (Ok(old), Ok(new)) = (
        parser::parse_path(path, &old_source),
        parser::parse_path(path, &new_source),
    ) else {
        return Ok(unparsed);
    };
    // A symbol missing from a cut-off list would read as removed or added.
    if old.incomplete || new.incomplete {
        return Ok(unparsed);
    }

    let (old_touched, old_other) = touched(&old, &old_source, &diff.old);
    let (new_touched, new_other) = touched(&new, &new_source, &diff.new);
    let key =
        |p: &ParsedFile, i: usize| (p.symbols[i].qualified_name(), p.symbols[i].kind.as_str());
    let find = |p: &'_ ParsedFile, k: &(String, &str)| {
        p.symbols
            .iter()
            .position(|s| s.kind.as_str() == k.1 && s.qualified_name() == k.0)
    };

    // Keyed by name and kind so a symbol touched on both sides shows once.
    let mut found: BTreeMap<(String, &str), SymbolChange> = BTreeMap::new();
    for i in new_touched {
        let k = key(&new, i);
        let change = if find(&old, &k).is_some() {
            Change::Modified
        } else {
            Change::Added
        };
        let s = &new.symbols[i];
        found.entry(k.clone()).or_insert(SymbolChange {
            change,
            kind: s.kind,
            qualified_name: k.0,
            start_line: s.start_line,
            end_line: s.end_line,
        });
    }
    for i in old_touched {
        let k = key(&old, i);
        if found.contains_key(&k) {
            continue;
        }
        let (change, s) = match find(&new, &k) {
            Some(j) => (Change::Modified, &new.symbols[j]),
            None => (Change::Removed, &old.symbols[i]),
        };
        found.insert(
            k.clone(),
            SymbolChange {
                change,
                kind: s.kind,
                qualified_name: k.0,
                start_line: s.start_line,
                end_line: s.end_line,
            },
        );
    }
    // A symbol added or removed as a whole implies its members were too.
    let whole: Vec<String> = found
        .values()
        .filter(|c| c.change != Change::Modified)
        .map(|c| format!("{}::", c.qualified_name))
        .collect();
    let mut symbols: Vec<SymbolChange> = found
        .into_values()
        .filter(|c| {
            !whole
                .iter()
                .any(|w| c.qualified_name.starts_with(w.as_str()))
        })
        .collect();
    symbols.sort_by(|a, b| {
        a.change
            .cmp(&b.change)
            .then(a.start_line.cmp(&b.start_line))
    });
    Ok(FileChanges {
        path: diff.path.clone(),
        symbols,
        other_lines: old_other + new_other,
        parsed: true,
    })
}

/// Indices of the innermost symbols holding a changed line, and the number
/// of changed lines outside every symbol. A symbol's lines include the doc
/// comments and attributes directly above it; blank and closing-delimiter
/// lines are ignored.
fn touched(parsed: &ParsedFile, source: &str, ranges: &[(usize, usize)]) -> (Vec<usize>, usize) {
    let lines: Vec<&str> = source.lines().collect();
    let first_lines: Vec<usize> = parsed
        .symbols
        .iter()
        .map(|s| parser::line_number(source, parser::symbol_start(parsed, source, s)))
        .collect();
    let mut hit = vec![false; parsed.symbols.len()];
    let mut other = 0;
    for &(start, count) in ranges {
        for line in start..start + count {
            if lines
                .get(line.wrapping_sub(1))
                .is_none_or(|l| insignificant(l))
            {
                continue;
            }
            let innermost = parsed
                .symbols
                .iter()
                .enumerate()
                .filter(|&(i, s)| first_lines[i] <= line && line <= s.end_line)
                .max_by_key(|(_, s)| s.depth);
            match innermost {
                Some((i, _)) => hit[i] = true,
                None => other += 1,
            }
        }
    }
    let indices = hit
        .iter()
        .enumerate()
        .filter_map(|(i, &h)| h.then_some(i))
        .collect();
    (indices, other)
}

/// Blank lines and lines of closing delimiters only (`}`, `});`, `]`).
/// Diffs often match such a line from one block with an identical line of
/// another, which would mark the wrong symbol as changed.
fn insignificant(line: &str) -> bool {
    line.trim()
        .chars()
        .all(|c| matches!(c, '}' | ')' | ']' | ';' | ','))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn count_lines_matches_str_lines_and_skips_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        // More than one piece, so a count across their boundary is covered.
        let long = "line\n".repeat(40_000);
        for text in ["", "a", "a\n", "a\n\nb", "a\r\nb\r\n", long.as_str()] {
            fs::write(&path, text).unwrap();
            assert_eq!(count_lines(&path), text.lines().count(), "{text:?}");
        }
        fs::write(&path, b"\x7fELF\0\0\n\n\n").unwrap();
        assert_eq!(count_lines(&path), 0);
        assert_eq!(count_lines(&dir.path().join("missing")), 0);
    }

    fn summary(files: &[FileChanges]) -> Vec<(String, String, String, usize)> {
        files
            .iter()
            .flat_map(|f| {
                f.symbols.iter().map(|s| {
                    (
                        f.path.clone(),
                        s.change.as_str().to_string(),
                        s.qualified_name.clone(),
                        s.start_line,
                    )
                })
            })
            .collect()
    }

    #[test]
    fn reports_added_modified_and_removed_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "use std::fmt;\n\nfn keep() {}\n\nfn edit() {\n    1;\n}\n\nfn gone() {}\n",
        )
        .unwrap();
        fs::write(root.join("notes.md"), "a\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);

        fs::write(
            root.join("src/lib.rs"),
            "use std::io;\n\nfn keep() {}\n\nfn edit() {\n    2;\n}\n\nfn fresh() {}\n",
        )
        .unwrap();
        fs::write(root.join("notes.md"), "b\n").unwrap();
        fs::write(root.join("src/new.py"), "def hello():\n    pass\n").unwrap();

        let spec = DiffSpec {
            base: "HEAD",
            staged: false,
        };
        let files = changed_symbols(root, spec).unwrap();
        assert_eq!(
            summary(&files),
            [
                ("src/lib.rs".into(), "added".into(), "fresh".into(), 9),
                ("src/lib.rs".into(), "modified".into(), "edit".into(), 5),
                ("src/lib.rs".into(), "removed".into(), "gone".into(), 9),
                ("src/new.py".into(), "added".into(), "hello".into(), 1),
            ]
        );
        let notes = files.iter().find(|f| f.path == "notes.md").unwrap();
        assert!(!notes.parsed);
        assert_eq!(notes.other_lines, 2);
        let lib = files.iter().find(|f| f.path == "src/lib.rs").unwrap();
        assert_eq!(lib.other_lines, 2); // the `use` line on each side

        // Only staged changes: nothing staged yet.
        let staged = DiffSpec {
            base: "HEAD",
            staged: true,
        };
        assert!(changed_symbols(root, staged).unwrap().is_empty());
        git(root, &["add", "src/new.py"]);
        assert_eq!(
            summary(&changed_symbols(root, staged).unwrap()),
            [("src/new.py".into(), "added".into(), "hello".into(), 1)]
        );

        let bad = DiffSpec {
            base: "no-such-ref",
            staged: false,
        };
        assert!(changed_symbols(root, bad).is_err());
    }

    #[test]
    fn ignores_blank_lines_and_members_of_added_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        fs::write(
            root.join("lib.rs"),
            "struct S;\n\nimpl S {\n    fn a(&self) {}\n}\n",
        )
        .unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);

        // A documented method added after a blank line inside `impl S`, and a
        // new function whose nested constant is new by definition.
        fs::write(
            root.join("lib.rs"),
            "struct S;\n\nimpl S {\n    fn a(&self) {}\n\n    /// Docs.\n    #[inline]\n    fn b(&self) {}\n}\n\nfn c() {\n    const N: u8 = 1;\n}\n",
        )
        .unwrap();
        let spec = DiffSpec {
            base: "HEAD",
            staged: false,
        };
        let files = changed_symbols(root, spec).unwrap();
        assert_eq!(
            summary(&files),
            [
                ("lib.rs".into(), "added".into(), "S::b".into(), 8),
                ("lib.rs".into(), "added".into(), "c".into(), 11),
            ]
        );
        assert_eq!(files[0].other_lines, 0);
    }
}
