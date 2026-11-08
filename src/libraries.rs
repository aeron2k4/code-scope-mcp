//! Directories of dependency sources, such as package caches and toolchain
//! sources, that the read-only tools may open besides the workspace. The
//! tools that write stay inside the workspace.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Environment variable naming more library directories, separated like
/// `PATH`.
pub const LIBRARY_PATHS_VAR: &str = "ASTLENS_LIBRARY_PATHS";

/// The library directories that exist on this machine, canonicalized: the
/// usual dependency source locations and those in [`LIBRARY_PATHS_VAR`].
pub fn default_roots() -> Vec<PathBuf> {
    existing(candidates(|name| std::env::var_os(name)))
}

/// Where dependency sources usually are, given the environment `var`.
fn candidates(var: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let path = |name: &str| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = path("HOME").or_else(|| path("USERPROFILE"));
    let in_home = |rel: &str| home.as_ref().map(|h| h.join(rel));
    let mut dirs = Vec::new();

    if let Some(cargo) = path("CARGO_HOME").or_else(|| in_home(".cargo")) {
        dirs.push(cargo.join("registry").join("src"));
        dirs.push(cargo.join("git").join("checkouts"));
    }
    // Standard library sources live under each toolchain's lib/rustlib/src.
    dirs.extend(
        path("RUSTUP_HOME")
            .or_else(|| in_home(".rustup"))
            .map(|r| r.join("toolchains")),
    );
    let go_path = var("GOPATH").and_then(|p| std::env::split_paths(&p).next());
    dirs.extend(
        path("GOMODCACHE")
            .or_else(|| go_path.map(|p| p.join("pkg").join("mod")))
            .or_else(|| in_home("go/pkg/mod")),
    );
    if cfg!(unix) {
        for dir in ["/usr/include", "/usr/local/include"] {
            dirs.push(PathBuf::from(dir));
        }
        for lib in ["/usr/lib", "/usr/local/lib"] {
            dirs.push(Path::new(lib).join("node_modules"));
            dirs.extend(pythons(Path::new(lib)));
        }
        dirs.extend(in_home(".local/lib").iter().flat_map(|lib| pythons(lib)));
    }
    if let Some(extra) = var(LIBRARY_PATHS_VAR) {
        dirs.extend(std::env::split_paths(&extra).filter(|p| !p.as_os_str().is_empty()));
    }
    dirs
}

/// The `python3*` directories in `lib`, which hold the standard library and
/// `site-packages`.
fn pythons(lib: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(lib) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("python3"))
        .map(|entry| entry.path())
        .collect()
}

/// The directories among `dirs` that exist, canonicalized, without
/// duplicates or directories inside another one.
pub fn existing(dirs: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = dirs
        .into_iter()
        .filter_map(|dir| dir.canonicalize().ok())
        .filter(|dir| dir.is_dir())
        .collect();
    // Shorter paths first, so a directory comes before those inside it.
    found.sort_by_key(|dir| dir.components().count());
    let mut roots: Vec<PathBuf> = Vec::new();
    for dir in found {
        if !roots.iter().any(|root| dir.starts_with(root)) {
            roots.push(dir);
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn candidates_follow_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let env = |name: &str| -> Option<OsString> {
            match name {
                "HOME" => Some(base.join("home").into()),
                "CARGO_HOME" => Some(base.join("cargo").into()),
                "GOPATH" => {
                    Some(std::env::join_paths([base.join("go1"), base.join("go2")]).unwrap())
                }
                LIBRARY_PATHS_VAR => {
                    Some(std::env::join_paths([base.join("extra"), PathBuf::new()]).unwrap())
                }
                _ => None,
            }
        };
        let dirs = candidates(env);
        for want in [
            base.join("cargo/registry/src"),
            base.join("cargo/git/checkouts"),
            base.join("home/.rustup/toolchains"),
            base.join("go1/pkg/mod"),
            base.join("extra"),
        ] {
            assert!(dirs.contains(&want), "{want:?} not in {dirs:?}");
        }
        assert!(!dirs.contains(&PathBuf::new()));
    }

    #[test]
    fn existing_keeps_real_directories_once() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        fs::create_dir_all(base.join("a/b")).unwrap();
        fs::create_dir_all(base.join("c")).unwrap();
        fs::write(base.join("file"), "").unwrap();
        let roots = existing([
            base.join("a/b"),
            base.join("a"),
            base.join("c/../c"),
            base.join("c"),
            base.join("missing"),
            base.join("file"),
        ]);
        assert_eq!(roots, [base.join("a"), base.join("c")]);
    }
}
