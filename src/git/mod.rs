//! Git integration: `.gitignore`-aware file walking and working-tree status.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result, bail};
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState};
use serde::Serialize;

/// List files under `root`, honouring `.gitignore`, `.ignore` and git
/// excludes. Paths are relative to `root` and sorted. Stops after `limit`.
///
/// The tree is walked on all cores. When it holds more than `limit` files,
/// which of them a parallel walk reaches first varies from run to run, so it
/// is walked again in order: the result is always the first `limit` files.
pub fn list_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    if let Some(mut files) = list_all_in_parallel(root, limit) {
        files.sort_by_cached_key(|p| sort_key(p));
        return files;
    }
    let mut files: Vec<PathBuf> = walker(root)
        .sort_by_file_path(|a, b| a.cmp(b))
        .build()
        .filter_map(|entry| file_path(root, entry))
        .take(limit)
        .collect();
    files.sort();
    files
}

/// A key whose byte order is the order of `Path::cmp`, component by
/// component, for the clean relative paths a walk gives: the bytes, with each
/// separator as the lowest. Comparing those is several times faster than
/// splitting both paths into components at every comparison.
fn sort_key(path: &Path) -> Vec<u8> {
    let mut key = path.as_os_str().as_encoded_bytes().to_vec();
    for c in &mut key {
        if *c == b'/' || *c == std::path::MAIN_SEPARATOR as u8 {
            *c = 0;
        }
    }
    key
}

fn walker(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        // Respect .gitignore even outside a git repository.
        .require_git(false);
    builder
}

/// The path relative to `root` of a walked entry that is a file.
fn file_path(root: &Path, entry: Result<DirEntry, ignore::Error>) -> Option<PathBuf> {
    let entry = entry
        .inspect_err(|err| tracing::debug!(%err, "skipping unreadable entry"))
        .ok()?;
    if !entry.file_type().is_some_and(|t| t.is_file()) {
        return None;
    }
    entry.path().strip_prefix(root).ok().map(Path::to_path_buf)
}

/// Every file under `root`, unsorted, or `None` once there are more than
/// `limit`.
fn list_all_in_parallel(root: &Path, limit: usize) -> Option<Vec<PathBuf>> {
    let shared = Collected {
        root,
        limit,
        count: AtomicUsize::new(0),
        files: Mutex::new(Vec::new()),
    };
    walker(root).build_parallel().visit(&mut &shared);
    if shared.count.into_inner() > limit {
        return None;
    }
    Some(
        shared
            .files
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner),
    )
}

struct Collected<'a> {
    root: &'a Path,
    limit: usize,
    count: AtomicUsize,
    files: Mutex<Vec<PathBuf>>,
}

impl<'s> ParallelVisitorBuilder<'s> for &'s Collected<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Collector {
            shared: self,
            local: Vec::new(),
        })
    }
}

/// One walker thread's files, handed over when the thread is done.
struct Collector<'s> {
    shared: &'s Collected<'s>,
    local: Vec<PathBuf>,
}

impl ParallelVisitor for Collector<'_> {
    fn visit(&mut self, entry: Result<DirEntry, ignore::Error>) -> WalkState {
        let Some(path) = file_path(self.shared.root, entry) else {
            return WalkState::Continue;
        };
        if self.shared.count.fetch_add(1, Ordering::Relaxed) >= self.shared.limit {
            return WalkState::Quit;
        }
        self.local.push(path);
        WalkState::Continue
    }
}

impl Drop for Collector<'_> {
    fn drop(&mut self) {
        let mut files = self
            .shared
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        files.append(&mut self.local);
    }
}

/// Git blob object id of `content`, identical to `git hash-object`.
pub fn blob_hash(content: &[u8]) -> String {
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(format!("blob {}\0", content.len()).as_bytes());
    hasher.update(content);
    hasher.digest().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileStatus {
    pub path: String,
    /// Two-letter porcelain code, e.g. `M `, ` M`, `??`, `R `.
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
}

/// Working-tree status of the files under `root`, with paths relative to it,
/// via `git status --porcelain`. Errors when `root` is not inside a git
/// repository or git is unavailable.
pub fn status(root: &Path) -> Result<Vec<FileStatus>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ])
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    // Porcelain paths start at the top of the repository, wherever git runs.
    let prefix = repo_prefix(root)?;
    let mut entries = parse_porcelain(&String::from_utf8_lossy(&output.stdout));
    for entry in &mut entries {
        entry.path = strip_repo_prefix(&entry.path, &prefix);
        entry.renamed_from = entry
            .renamed_from
            .as_deref()
            .map(|from| strip_repo_prefix(from, &prefix));
    }
    Ok(entries)
}

/// The path of `root` inside its repository: empty at the top, else `sub/dir/`.
fn repo_prefix(root: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-prefix"])
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        bail!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_string())
}

fn strip_repo_prefix(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or(path).to_string()
}

/// Whether `root` is in a repository whose current branch has no commit yet.
fn lacks_commits(root: &Path) -> bool {
    let succeeds = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .is_ok_and(|output| output.status.success())
    };
    succeeds(&["rev-parse", "--git-dir"])
        && !succeeds(&["rev-parse", "--verify", "--quiet", "HEAD"])
}

fn parse_porcelain(raw: &str) -> Vec<FileStatus> {
    let mut out = Vec::new();
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue;
        }
        let (code, path) = entry.split_at(2);
        let path = path[1..].to_string();
        // Renames and copies carry the original path as the next field.
        let renamed_from = if code.contains('R') || code.contains('C') {
            fields.next().map(str::to_string)
        } else {
            None
        };
        out.push(FileStatus {
            path,
            code: code.to_string(),
            renamed_from,
        });
    }
    out
}

/// Changed line ranges of one file in a diff, as `(start, count)` pairs on
/// each side; a count of 0 marks a pure insertion or deletion point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Path on the new side relative to `root`, or the old path when deleted.
    pub path: String,
    pub old: Vec<(usize, usize)>,
    pub new: Vec<(usize, usize)>,
    pub added: bool,
    pub deleted: bool,
}

/// Line-level diff of the working tree (or the index when `staged`) against
/// `base`, limited to files under `root`, with paths relative to it.
pub fn diff(root: &Path, base: &str, staged: bool) -> Result<Vec<FileDiff>> {
    let mut cmd = Command::new("git");
    // Fixed prefixes and unquoted paths whatever the user's git config says.
    cmd.arg("-C").arg(root).args([
        "-c",
        "core.quotePath=false",
        "diff",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--no-color",
        "--no-ext-diff",
        "--no-renames",
        "--unified=0",
        "--relative",
    ]);
    if staged {
        cmd.arg("--cached");
    }
    let output = cmd
        .arg(base)
        .arg("--")
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        if lacks_commits(root) {
            bail!("the repository has no commits yet, so there is nothing to compare {base} with");
        }
        bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(parse_diff(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_diff(raw: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut old_path = None;
    // Lines left in the current hunk. They are file content, so `--- x` among
    // them is a removed `-- x`, not a header.
    let mut content_lines = 0;
    for line in raw.lines() {
        if content_lines > 0 && line.starts_with(['-', '+']) {
            content_lines -= 1;
        } else if line.starts_with("diff --git ") {
            old_path = None;
        } else if let Some(p) = line.strip_prefix("--- ") {
            old_path = header_path(p, "a/");
        } else if let Some(p) = line.strip_prefix("+++ ") {
            let new_path = header_path(p, "b/");
            let deleted = new_path.is_none();
            let Some(path) = new_path.or_else(|| old_path.take()) else {
                continue;
            };
            files.push(FileDiff {
                path,
                old: Vec::new(),
                new: Vec::new(),
                added: old_path.is_none() && !deleted,
                deleted,
            });
        } else if let Some(header) = line.strip_prefix("@@ ")
            && let Some(file) = files.last_mut()
        {
            // @@ -a[,b] +c[,d] @@ context
            let mut parts = header.split_whitespace();
            let range = |part: Option<&str>, sign: char| {
                let spec = part?.strip_prefix(sign)?;
                let (start, count) = spec.split_once(',').unwrap_or((spec, "1"));
                Some((start.parse().ok()?, count.parse().ok()?))
            };
            if let (Some(old), Some(new)) = (range(parts.next(), '-'), range(parts.next(), '+')) {
                file.old.push(old);
                file.new.push(new);
                content_lines = old.1 + new.1;
            }
        }
    }
    files
}

/// The path in a `---` or `+++` line. Git ends the line with a tab when the
/// path has a space; a path that itself holds a tab is quoted instead.
fn header_path(text: &str, prefix: &str) -> Option<String> {
    let path = text.strip_prefix(prefix)?;
    Some(path.strip_suffix('\t').unwrap_or(path).to_string())
}

/// Contents of `path` (relative to `root`) at `rev`, or in the index when
/// `rev` is empty; `None` when it does not exist there.
pub fn show(root: &Path, rev: &str, path: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("show")
        .arg(format!("{rev}:./{path}"))
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8(output.stdout).ok())
}

/// Untracked, non-ignored files under `root`, relative to it.
pub fn untracked(root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        bail!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn walk_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("target/out.rs"), "").unwrap();
        fs::write(root.join("debug.log"), "").unwrap();
        fs::write(root.join("README.md"), "").unwrap();

        let files = list_files(root, 100);
        assert_eq!(
            files,
            vec![PathBuf::from("README.md"), PathBuf::from("src/main.rs")]
        );
        assert_eq!(list_files(root, 1).len(), 1);
    }

    #[test]
    fn a_limited_walk_keeps_the_first_files_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // `a/z` sorts before `a-b` by component, after it by bytes.
        for name in ["a-b", "a/z", "a/b/c", "b", "a.x", "ab"] {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        let all = list_files(root, 100);
        let mut expected = all.clone();
        expected.sort();
        assert_eq!(all, expected);
        assert_eq!(all[..2], [PathBuf::from("a/b/c"), PathBuf::from("a/z")]);
        // Past the limit the walk is redone in order, not cut where the
        // parallel threads happened to be.
        for limit in 1..all.len() {
            assert_eq!(list_files(root, limit), all[..limit], "limit {limit}");
        }
    }

    #[test]
    fn blob_hash_matches_git() {
        assert_eq!(blob_hash(b""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(
            blob_hash(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn parses_porcelain_entries() {
        let raw = " M src/a.rs\0?? new.txt\0R  b.rs\0old_b.rs\0";
        let parsed = parse_porcelain(raw);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].code, " M");
        assert_eq!(parsed[0].path, "src/a.rs");
        assert_eq!(parsed[1].code, "??");
        assert_eq!(parsed[2].path, "b.rs");
        assert_eq!(parsed[2].renamed_from.as_deref(), Some("old_b.rs"));
    }

    #[test]
    fn parses_unified_zero_diff() {
        let raw = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -3 +3,2 @@ fn f() {\n-x\n+y\n+z\n@@ -10,2 +11,0 @@\n-a\n-b\ndiff --git a/new.py b/new.py\nnew file mode 100644\n--- /dev/null\n+++ b/new.py\n@@ -0,0 +1,2 @@\n+def f():\n+    pass\ndiff --git a/old.go b/old.go\ndeleted file mode 100644\n--- a/old.go\n+++ /dev/null\n@@ -1 +0,0 @@\n-package p\n";
        let files = parse_diff(raw);
        assert_eq!(
            files,
            vec![
                FileDiff {
                    path: "src/a.rs".into(),
                    old: vec![(3, 1), (10, 2)],
                    new: vec![(3, 2), (11, 0)],
                    added: false,
                    deleted: false,
                },
                FileDiff {
                    path: "new.py".into(),
                    old: vec![(0, 0)],
                    new: vec![(1, 2)],
                    added: true,
                    deleted: false,
                },
                FileDiff {
                    path: "old.go".into(),
                    old: vec![(1, 1)],
                    new: vec![(0, 0)],
                    added: false,
                    deleted: true,
                },
            ]
        );
    }

    #[test]
    fn diff_paths_with_spaces_lose_the_trailing_tab() {
        let raw = "diff --git a/my file.rs b/my file.rs\n\
                   --- a/my file.rs\t\n\
                   +++ b/my file.rs\t\n\
                   @@ -1 +1 @@\n";
        let files = parse_diff(raw);
        assert_eq!(files[0].path, "my file.rs");
        assert!(!files[0].added && !files[0].deleted);

        let raw = "diff --git a/my file.rs b/my file.rs\n\
                   --- a/my file.rs\t\n\
                   +++ /dev/null\n\
                   @@ -1 +0,0 @@\n";
        let files = parse_diff(raw);
        assert_eq!(files[0].path, "my file.rs");
        assert!(files[0].deleted);
    }

    #[test]
    fn diff_content_that_looks_like_a_header_stays_content() {
        let raw = "diff --git a/q.sql b/q.sql\n\
                   --- a/q.sql\n\
                   +++ b/q.sql\n\
                   @@ -2,2 +2,2 @@\n\
                   --- a/comment\n\
                   --- second\n\
                   +++ b/comment\n\
                   +++ second\n\
                   @@ -9 +9 @@\n\
                   -x\n\
                   \\ No newline at end of file\n\
                   +y\n\
                   diff --git a/z.sql b/z.sql\n\
                   --- a/z.sql\n\
                   +++ b/z.sql\n\
                   @@ -1 +1 @@\n\
                   -a\n\
                   +b\n";
        let files = parse_diff(raw);
        let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["q.sql", "z.sql"]);
        assert_eq!(files[0].old, [(2, 2), (9, 1)]);
        assert_eq!(files[0].new, [(2, 2), (9, 1)]);
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    #[test]
    fn status_in_a_subdirectory_is_scoped_and_relative() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        git(top, &["init", "-q"]);
        fs::create_dir_all(top.join("sub/in")).unwrap();
        fs::write(top.join("outside.rs"), "").unwrap();
        fs::write(top.join("sub/in/a.rs"), "").unwrap();
        fs::write(top.join("sub/old.rs"), "x\n").unwrap();
        git(top, &["add", "sub/old.rs"]);
        git(top, &["commit", "-qm", "x"]);
        git(top, &["mv", "sub/old.rs", "sub/new.rs"]);

        let sub = status(&top.join("sub")).unwrap();
        let paths: Vec<_> = sub.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["new.rs", "in/a.rs"]);
        assert_eq!(sub[0].renamed_from.as_deref(), Some("old.rs"));

        let all = status(top).unwrap();
        assert!(all.iter().any(|e| e.path == "outside.rs"));
    }

    #[test]
    fn diff_without_commits_says_so() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        let err = diff(dir.path(), "HEAD", false).unwrap_err().to_string();
        assert!(err.contains("no commits yet"), "{err}");
    }

    #[test]
    fn status_outside_repo_is_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(status(dir.path()).is_err());
    }
}
