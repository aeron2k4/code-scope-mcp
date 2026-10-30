//! Git integration: `.gitignore`-aware file walking and working-tree status.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use serde::Serialize;

/// List files under `root`, honouring `.gitignore`, `.ignore` and git
/// excludes. Paths are relative to `root` and sorted. Stops after `limit`.
pub fn list_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        // Respect .gitignore even outside a git repository.
        .require_git(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .build()
        .filter_map(|entry| match entry {
            Ok(e) => Some(e),
            Err(err) => {
                tracing::debug!(%err, "skipping unreadable entry");
                None
            }
        })
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| e.path().strip_prefix(root).ok().map(Path::to_path_buf))
        .take(limit)
        .collect();
    files.sort();
    files
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileStatus {
    pub path: String,
    /// Two-letter porcelain code, e.g. `M `, ` M`, `??`, `R `.
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
}

/// Working-tree status via `git status --porcelain`. Errors when `root` is
/// not inside a git repository or git is unavailable.
pub fn status(root: &Path) -> Result<Vec<FileStatus>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(parse_porcelain(&String::from_utf8_lossy(&output.stdout)))
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
    fn status_outside_repo_is_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(status(dir.path()).is_err());
    }
}
