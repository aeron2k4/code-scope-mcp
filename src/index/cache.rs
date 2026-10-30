//! Lazy parse cache. Files are parsed on first request and re-parsed only
//! when their modification time or size changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use crate::ast::parser::{self, ParseError, ParsedFile};

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is {len} bytes, above the {max} byte limit")]
    TooLarge { path: PathBuf, len: u64, max: u64 },
    #[error("{0} is not valid UTF-8")]
    NotUtf8(PathBuf),
    #[error(transparent)]
    Parse(#[from] ParseError),
}

#[derive(Debug)]
pub struct CachedFile {
    pub source: String,
    pub parsed: ParsedFile,
}

struct Entry {
    mtime: SystemTime,
    len: u64,
    last_used: AtomicU64,
    file: Arc<CachedFile>,
}

pub struct ParseCache {
    entries: RwLock<HashMap<PathBuf, Entry>>,
    clock: AtomicU64,
    max_entries: usize,
    max_file_bytes: u64,
}

impl Default for ParseCache {
    fn default() -> Self {
        Self::new(4096, 2 * 1024 * 1024)
    }
}

impl ParseCache {
    pub fn new(max_entries: usize, max_file_bytes: u64) -> Self {
        ParseCache {
            entries: RwLock::new(HashMap::new()),
            clock: AtomicU64::new(0),
            max_entries: max_entries.max(1),
            max_file_bytes,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.read().map(|e| e.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }

    /// Parsed contents of `path`, served from cache when still fresh.
    pub fn get(&self, path: &Path) -> Result<Arc<CachedFile>, CacheError> {
        let io_err = |source| CacheError::Io {
            path: path.to_path_buf(),
            source,
        };
        let meta = std::fs::metadata(path).map_err(io_err)?;
        let len = meta.len();
        let mtime = meta.modified().map_err(io_err)?;

        if let Ok(entries) = self.entries.read()
            && let Some(entry) = entries.get(path)
            && entry.mtime == mtime
            && entry.len == len
        {
            entry.last_used.store(self.tick(), Ordering::Relaxed);
            return Ok(Arc::clone(&entry.file));
        }

        if len > self.max_file_bytes {
            return Err(CacheError::TooLarge {
                path: path.to_path_buf(),
                len,
                max: self.max_file_bytes,
            });
        }
        let bytes = std::fs::read(path).map_err(io_err)?;
        let source = String::from_utf8(bytes).map_err(|_| CacheError::NotUtf8(path.into()))?;
        let parsed = parser::parse_path(path, &source)?;
        if parsed.has_errors {
            tracing::debug!(path = %path.display(), "parsed with syntax errors");
        }
        let file = Arc::new(CachedFile { source, parsed });

        if let Ok(mut entries) = self.entries.write() {
            if entries.len() >= self.max_entries && !entries.contains_key(path) {
                evict_oldest(&mut entries);
            }
            entries.insert(
                path.to_path_buf(),
                Entry {
                    mtime,
                    len,
                    last_used: AtomicU64::new(self.tick()),
                    file: Arc::clone(&file),
                },
            );
        }
        Ok(file)
    }
}

fn evict_oldest(entries: &mut HashMap<PathBuf, Entry>) {
    let oldest = entries
        .iter()
        .min_by_key(|(_, e)| e.last_used.load(Ordering::Relaxed))
        .map(|(p, _)| p.clone());
    if let Some(path) = oldest {
        entries.remove(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn caches_until_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        fs::write(&path, "fn a() {}").unwrap();

        let cache = ParseCache::default();
        let first = cache.get(&path).unwrap();
        let second = cache.get(&path).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.parsed.symbols[0].name, "a");

        fs::write(&path, "fn bb() {}").unwrap();
        let third = cache.get(&path).unwrap();
        assert!(!Arc::ptr_eq(&first, &third));
        assert_eq!(third.parsed.symbols[0].name, "bb");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn evicts_least_recently_used() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..3)
            .map(|i| {
                let p = dir.path().join(format!("f{i}.py"));
                fs::write(&p, format!("def f{i}(): pass\n")).unwrap();
                p
            })
            .collect();

        let cache = ParseCache::new(2, 1024);
        let f0 = cache.get(&paths[0]).unwrap();
        cache.get(&paths[1]).unwrap();
        // Touch f0 so f1 becomes the eviction victim.
        assert!(Arc::ptr_eq(&f0, &cache.get(&paths[0]).unwrap()));
        cache.get(&paths[2]).unwrap();
        assert_eq!(cache.len(), 2);
        assert!(Arc::ptr_eq(&f0, &cache.get(&paths[0]).unwrap()));
    }

    #[test]
    fn rejects_large_and_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.rs");
        fs::write(&big, "x".repeat(100)).unwrap();
        let bin = dir.path().join("bin.rs");
        fs::write(&bin, [0xff, 0xfe, 0x00]).unwrap();

        let cache = ParseCache::new(8, 50);
        assert!(matches!(cache.get(&big), Err(CacheError::TooLarge { .. })));
        assert!(matches!(cache.get(&bin), Err(CacheError::NotUtf8(_))));
        assert!(matches!(
            cache.get(&dir.path().join("missing.rs")),
            Err(CacheError::Io { .. })
        ));
        assert!(cache.is_empty());
    }
}
