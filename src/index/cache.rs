//! Two-level parse cache.
//!
//! 1. In memory: files are parsed on first request and served from memory
//!    while their modification time and size are unchanged.
//! 2. On disk (optional): a [`PersistentStore`] backed by LMDB (memory-mapped
//!    via `heed`) keeps parse results across restarts, keyed by absolute path
//!    and validated by the file's git blob hash, so an unchanged file is never
//!    re-parsed even after the server restarts or the mtime is touched.
//!
//! Writes to the store are buffered and committed in one transaction by
//! [`ParseCache::flush`], so a project-wide search costs a single fsync.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use heed::types::{Bytes, DecodeIgnore, Str};
use heed::{Database, Env, EnvOpenOptions};
use serde::{Deserialize, Serialize};

use crate::ast::parser::{self, ParseError, ParsedFile};
use crate::git;

/// Changes with the crate version so stale formats or grammar changes never
/// leak into results. Bump the `-s` suffix whenever [`ParsedFile`]'s stored
/// format or a symbol query changes between releases.
const DB_NAME: &str = concat!("parsed-v", env!("CARGO_PKG_VERSION"), "-s2");
/// Every database this store has ever created starts with this prefix.
const DB_PREFIX: &str = "parsed-v";
const MAP_SIZE: usize = 1 << 30;
/// Pending writes that trigger an automatic flush.
const FLUSH_THRESHOLD: usize = 512;

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

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot create cache directory {path}: {source}")]
    Dir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Lmdb(#[from] heed::Error),
}

#[derive(Serialize, Deserialize)]
struct StoredFile {
    blob: String,
    parsed: ParsedFile,
}

/// Persistent, memory-mapped store of parse results.
pub struct PersistentStore {
    env: Env,
    db: Database<Bytes, Bytes>,
}

impl PersistentStore {
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        Self::open_sized(dir, MAP_SIZE)
    }

    fn open_sized(dir: &Path, map_size: usize) -> Result<Self, StoreError> {
        std::fs::create_dir_all(dir).map_err(|source| StoreError::Dir {
            path: dir.to_path_buf(),
            source,
        })?;
        // SAFETY: the map is only accessed through heed transactions and the
        // files are never modified other than through LMDB, which handles
        // multi-process locking itself. Opening the same path more than once
        // per process is supported by heed.
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(map_size)
                .max_dbs(4)
                .open(dir)?
        };
        let mut wtxn = env.write_txn()?;
        let db = env.create_database(&mut wtxn, Some(DB_NAME))?;
        Self::drop_stale(&env, &mut wtxn)?;
        wtxn.commit()?;
        Ok(PersistentStore { env, db })
    }

    /// Remove databases written by other versions, so they neither use up
    /// `max_dbs` slots nor space in the map.
    fn drop_stale(env: &Env, wtxn: &mut heed::RwTxn) -> Result<(), StoreError> {
        let names: Database<Str, DecodeIgnore> = env
            .open_database(wtxn, None)?
            .ok_or(heed::Error::Mdb(heed::MdbError::NotFound))?;
        let stale: Vec<String> = names
            .iter(wtxn)?
            .filter_map(|item| item.ok().map(|(name, ())| name.to_owned()))
            .filter(|name| name.starts_with(DB_PREFIX) && name != DB_NAME)
            .collect();
        for name in stale {
            if let Some(db) = env.open_database::<Bytes, Bytes>(wtxn, Some(&name))? {
                tracing::info!(%name, "removing parse store from another version");
                // SAFETY: the database was opened just now in this transaction
                // and nothing in this process has modified it.
                unsafe { db.remove(wtxn)? };
            }
        }
        Ok(())
    }

    fn key(path: &Path) -> &[u8] {
        path.as_os_str().as_encoded_bytes()
    }

    /// Stored parse for `path` if its blob hash still matches. Any storage
    /// or decoding problem is treated as a miss.
    fn load(&self, path: &Path, blob: &str) -> Option<ParsedFile> {
        let read = || -> Result<Option<StoredFile>, StoreError> {
            let rtxn = self.env.read_txn()?;
            Ok(self
                .db
                .get(&rtxn, Self::key(path))?
                .and_then(|raw| serde_json::from_slice(raw).ok()))
        };
        match read() {
            Ok(Some(stored)) if stored.blob == blob => Some(stored.parsed),
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(%err, "parse store read failed");
                None
            }
        }
    }

    /// Persist a batch. The store is only a cache, so once real usage passes
    /// 3/4 of the map it is wiped (which also drops entries of deleted
    /// files). Clearing early matters: LMDB cannot reuse the pages freed by
    /// the clear until a later transaction, so the next write needs headroom.
    fn save(&self, items: &[(PathBuf, String, Arc<CachedFile>)]) -> Result<(), StoreError> {
        let map_size = self.env.info().map_size as u64;
        if self.env.non_free_pages_size()? > map_size / 4 * 3 {
            self.clear()?;
        }
        match self.write_batch(items) {
            Err(StoreError::Lmdb(heed::Error::Mdb(heed::MdbError::MapFull))) => {
                self.clear()?;
                self.write_batch(items)
            }
            other => other,
        }
    }

    fn clear(&self) -> Result<(), StoreError> {
        tracing::info!("parse store nearly full, clearing it");
        let mut wtxn = self.env.write_txn()?;
        self.db.clear(&mut wtxn)?;
        wtxn.commit()?;
        Ok(())
    }

    fn write_batch(&self, items: &[(PathBuf, String, Arc<CachedFile>)]) -> Result<(), StoreError> {
        let mut wtxn = self.env.write_txn()?;
        for (path, blob, file) in items {
            #[derive(Serialize)]
            struct StoredRef<'a> {
                blob: &'a str,
                parsed: &'a ParsedFile,
            }
            let Ok(raw) = serde_json::to_vec(&StoredRef {
                blob,
                parsed: &file.parsed,
            }) else {
                continue;
            };
            self.db.put(&mut wtxn, Self::key(path), &raw)?;
        }
        wtxn.commit()?;
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CacheStats {
    pub memory_hits: u64,
    pub store_hits: u64,
    pub parses: u64,
}

#[derive(Default)]
struct Counters {
    memory_hits: AtomicU64,
    store_hits: AtomicU64,
    parses: AtomicU64,
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
    store: Option<PersistentStore>,
    pending: Mutex<Vec<(PathBuf, String, Arc<CachedFile>)>>,
    counters: Counters,
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
            store: None,
            pending: Mutex::new(Vec::new()),
            counters: Counters::default(),
        }
    }

    /// Back this cache with a persistent store.
    pub fn with_store(mut self, store: PersistentStore) -> Self {
        self.store = Some(store);
        self
    }

    pub fn stats(&self) -> CacheStats {
        let c = &self.counters;
        CacheStats {
            memory_hits: c.memory_hits.load(Ordering::Relaxed),
            store_hits: c.store_hits.load(Ordering::Relaxed),
            parses: c.parses.load(Ordering::Relaxed),
        }
    }

    /// Commit buffered parse results to the persistent store, if any.
    pub fn flush(&self) {
        let Some(store) = &self.store else {
            return;
        };
        let items = match self.pending.lock() {
            Ok(mut pending) => std::mem::take(&mut *pending),
            Err(_) => return,
        };
        if items.is_empty() {
            return;
        }
        if let Err(err) = store.save(&items) {
            tracing::warn!(%err, count = items.len(), "failed to persist parse results");
        }
    }

    fn queue_write(&self, path: &Path, blob: String, file: &Arc<CachedFile>) {
        let full = match self.pending.lock() {
            Ok(mut pending) => {
                pending.push((path.to_path_buf(), blob, Arc::clone(file)));
                pending.len() >= FLUSH_THRESHOLD
            }
            Err(_) => false,
        };
        if full {
            self.flush();
        }
    }

    pub fn len(&self) -> usize {
        self.entries.read().map(|e| e.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forget the in-memory entry for `path`, e.g. after writing to it, so a
    /// rewrite that keeps the same size and mtime is never served stale.
    pub fn invalidate(&self, path: &Path) {
        if let Ok(mut entries) = self.entries.write() {
            entries.remove(path);
        }
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
            self.counters.memory_hits.fetch_add(1, Ordering::Relaxed);
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
        let blob = self.store.as_ref().map(|_| git::blob_hash(&bytes));
        let source = String::from_utf8(bytes).map_err(|_| CacheError::NotUtf8(path.into()))?;

        let stored = match (&self.store, &blob) {
            (Some(store), Some(blob)) => store.load(path, blob),
            _ => None,
        };
        let file = match stored {
            Some(parsed) => {
                self.counters.store_hits.fetch_add(1, Ordering::Relaxed);
                Arc::new(CachedFile { source, parsed })
            }
            None => {
                let parsed = parser::parse_path(path, &source)?;
                self.counters.parses.fetch_add(1, Ordering::Relaxed);
                if parsed.has_errors {
                    tracing::debug!(path = %path.display(), "parsed with syntax errors");
                }
                let file = Arc::new(CachedFile { source, parsed });
                if let Some(blob) = blob {
                    self.queue_write(path, blob, &file);
                }
                file
            }
        };

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

impl Drop for ParseCache {
    fn drop(&mut self) {
        self.flush();
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
    fn persistent_store_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let path = dir.path().join("lib.rs");
        fs::write(&path, "struct S;\nfn f() {}\n").unwrap();

        let first = ParseCache::default().with_store(PersistentStore::open(&store_dir).unwrap());
        let parsed = first.get(&path).unwrap().parsed.symbols.clone();
        assert_eq!(first.stats().parses, 1);
        drop(first); // flushes pending writes

        let second = ParseCache::default().with_store(PersistentStore::open(&store_dir).unwrap());
        let file = second.get(&path).unwrap();
        assert_eq!(
            second.stats(),
            CacheStats {
                memory_hits: 0,
                store_hits: 1,
                parses: 0
            }
        );
        assert_eq!(file.parsed.symbols, parsed);
        second.get(&path).unwrap();
        assert_eq!(second.stats().memory_hits, 1);
    }

    #[test]
    fn opening_removes_databases_of_other_versions() {
        let dir = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(dir.path()).unwrap();
        let mut wtxn = store.env.write_txn().unwrap();
        for name in ["parsed-v0.0.1", "parsed-v0.1.0-s1", "unrelated"] {
            let db: Database<Bytes, Bytes> =
                store.env.create_database(&mut wtxn, Some(name)).unwrap();
            db.put(&mut wtxn, b"k", b"v").unwrap();
        }
        wtxn.commit().unwrap();
        drop(store);

        let store = PersistentStore::open(dir.path()).unwrap();
        let rtxn = store.env.read_txn().unwrap();
        let names: Database<Str, DecodeIgnore> =
            store.env.open_database(&rtxn, None).unwrap().unwrap();
        let mut left: Vec<_> = names
            .iter(&rtxn)
            .unwrap()
            .map(|item| item.unwrap().0.to_owned())
            .collect();
        left.sort();
        assert_eq!(left, [DB_NAME, "unrelated"]);
    }

    #[test]
    fn persistent_store_detects_content_change() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let path = dir.path().join("a.py");
        fs::write(&path, "def old(): pass\n").unwrap();
        let cache = ParseCache::default().with_store(PersistentStore::open(&store_dir).unwrap());
        cache.get(&path).unwrap();
        cache.flush();
        drop(cache);

        fs::write(&path, "def new(): pass\n").unwrap();
        let cache = ParseCache::default().with_store(PersistentStore::open(&store_dir).unwrap());
        let file = cache.get(&path).unwrap();
        assert_eq!(file.parsed.symbols[0].name, "new");
        assert_eq!(cache.stats().parses, 1);
        assert_eq!(cache.stats().store_hits, 0);
    }

    #[test]
    fn full_store_is_cleared_and_keeps_working() {
        let dir = tempfile::tempdir().unwrap();
        let store = PersistentStore::open_sized(&dir.path().join("store"), 1024 * 1024).unwrap();
        // ~8 KB per entry, so the 1 MiB map fills several times over.
        let source: String = (0..50)
            .map(|i| format!("fn function_{i}() {{}}\n"))
            .collect();
        let parsed = parser::parse_source(parser::Lang::Rust, &source).unwrap();
        let file = Arc::new(CachedFile { source, parsed });

        for i in 0..300 {
            let path = PathBuf::from(format!("/virtual/f{i}.rs"));
            store
                .save(&[(path, format!("blob{i}"), Arc::clone(&file))])
                .unwrap_or_else(|e| panic!("batch {i}: {e}"));
        }
        // The latest entry survives; the oldest was dropped by the wipe.
        assert!(
            store
                .load(Path::new("/virtual/f299.rs"), "blob299")
                .is_some()
        );
        assert!(store.load(Path::new("/virtual/f0.rs"), "blob0").is_none());
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
