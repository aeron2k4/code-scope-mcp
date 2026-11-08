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
//!
//! Beside the parsed files, the cache keeps each file's [`SymbolNames`] under
//! a budget of their own, a fraction of the size, so symbol search over a
//! tree too large for the file budget still runs from memory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use heed::types::{Bytes, DecodeIgnore, Str};
use heed::{Database, Env, EnvOpenOptions};
use serde::Serialize;

use crate::ast::parser::{self, Lang, ParseError, ParsedFile};
use crate::git;
use crate::index::names::SymbolNames;
use crate::index::store_format;

/// Changes with the crate version so stale formats or grammar changes never
/// leak into results. Bump the `-s` suffix whenever [`ParsedFile`]'s stored
/// format or a symbol query changes between releases.
const DB_NAME: &str = concat!("parsed-v", env!("CARGO_PKG_VERSION"), "-s10");
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
    #[error("{0} is not a regular file")]
    NotAFile(PathBuf),
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

/// What the store keeps per file: the blob hash that validates it, a header
/// for the flags JSON would otherwise have to hold, and the compact parse.
/// It borrows its bytes, so a read can check the hash in place and only
/// decode an entry that is still valid.
#[derive(Debug, PartialEq, Eq)]
struct Stored<'a> {
    blob: &'a str,
    lang: Lang,
    has_errors: bool,
    incomplete: bool,
    parsed: &'a [u8],
}

/// The bytes of the header: the language, a byte of flags and the length of
/// the blob hash that follows it.
const HEADER_BYTES: usize = 3;

impl<'a> Stored<'a> {
    /// The header, then the bytes of the blob hash, then the bytes of
    /// [`store_format`].
    fn encode(&self) -> Vec<u8> {
        // A blob hash is 40 or 64 bytes. A longer one is cut, which can only
        // make the entry miss, never match another blob.
        let blob = &self.blob.as_bytes()[..self.blob.len().min(u8::MAX as usize)];
        let mut out = Vec::with_capacity(HEADER_BYTES + blob.len() + self.parsed.len());
        out.push(self.lang.index() as u8);
        out.push(u8::from(self.has_errors) | u8::from(self.incomplete) << 1);
        out.push(blob.len() as u8);
        out.extend_from_slice(blob);
        out.extend_from_slice(self.parsed);
        out
    }

    /// None when `bytes` is too short or holds an unknown language or a
    /// hash that is not text, which the caller treats as a miss.
    fn decode(bytes: &'a [u8]) -> Option<Self> {
        let (&[lang, flags, hash_len], rest) = bytes.split_first_chunk::<HEADER_BYTES>()?;
        let (hash, parsed) = rest.split_at_checked(hash_len as usize)?;
        Some(Stored {
            blob: std::str::from_utf8(hash).ok()?,
            lang: Lang::from_index(lang as usize)?,
            has_errors: flags & 1 != 0,
            incomplete: flags & 2 != 0,
            parsed,
        })
    }
}

/// Persistent, memory-mapped store of parse results. Clones share the same
/// environment.
#[derive(Clone)]
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
        let read = || -> Result<Option<ParsedFile>, StoreError> {
            let rtxn = self.env.read_txn()?;
            // The hash is compared on the mapped bytes, so a stale entry is
            // neither copied nor decoded.
            let Some(stored) = self
                .db
                .get(&rtxn, Self::key(path))?
                .and_then(Stored::decode)
                .filter(|stored| stored.blob == blob)
            else {
                return Ok(None);
            };
            match store_format::decode(
                stored.parsed,
                stored.lang,
                stored.has_errors,
                stored.incomplete,
            ) {
                Ok(parsed) => Ok(Some(parsed)),
                Err(err) => {
                    tracing::debug!(%err, path = %path.display(), "ignoring stored parse");
                    Ok(None)
                }
            }
        };
        read().unwrap_or_else(|err| {
            tracing::warn!(%err, "parse store read failed");
            None
        })
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

    /// Delete entries for files under `root` that no longer exist, so
    /// deleted and renamed files do not keep their results until the store
    /// fills up. Returns the number of entries removed. Paths that are not
    /// valid UTF-8 are left alone.
    pub fn prune_missing(&self, root: &Path) -> Result<usize, StoreError> {
        let mut prefix = Self::key(root).to_vec();
        prefix.push(std::path::MAIN_SEPARATOR as u8);
        let missing: Vec<Vec<u8>> = {
            let rtxn = self.env.read_txn()?;
            self.db
                .prefix_iter(&rtxn, &prefix)?
                .filter_map(|item| item.ok().map(|(key, _)| key))
                .filter(|key| std::str::from_utf8(key).is_ok_and(|p| !Path::new(p).exists()))
                .map(<[u8]>::to_vec)
                .collect()
        };
        if missing.is_empty() {
            return Ok(0);
        }
        let mut wtxn = self.env.write_txn()?;
        for key in &missing {
            self.db.delete(&mut wtxn, key)?;
        }
        wtxn.commit()?;
        Ok(missing.len())
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
            let parsed = store_format::encode(&file.parsed);
            let stored = Stored {
                blob,
                lang: file.parsed.lang,
                has_errors: file.parsed.has_errors,
                incomplete: file.parsed.incomplete,
                parsed: &parsed,
            };
            self.db.put(&mut wtxn, Self::key(path), &stored.encode())?;
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

impl CachedFile {
    /// Approximate heap size, for the memory budget.
    fn weight(&self) -> usize {
        const SYMBOL_OVERHEAD: usize = 160;
        let symbols: usize = self
            .parsed
            .symbols
            .iter()
            .map(|s| SYMBOL_OVERHEAD + s.name.len() + s.signature.len())
            .sum();
        self.source.len() + self.parsed.idents.len() * 4 + symbols
    }
}

struct Entry {
    mtime: SystemTime,
    len: u64,
    last_used: AtomicU64,
    weight: usize,
    file: Arc<CachedFile>,
}

struct NamesEntry {
    mtime: SystemTime,
    len: u64,
    weight: usize,
    names: Arc<SymbolNames>,
}

/// Default memory budget for cached files: enough for the 82 MB Go standard
/// library, so a workspace-wide search does not evict what it just parsed.
pub const DEFAULT_MAX_BYTES: usize = 512 << 20;

pub struct ParseCache {
    entries: RwLock<HashMap<PathBuf, Entry>>,
    clock: AtomicU64,
    max_entries: usize,
    max_bytes: usize,
    /// Sum of the weights of `entries`; changed only under its write lock.
    used_bytes: AtomicU64,
    max_file_bytes: u64,
    store: Option<PersistentStore>,
    pending: Mutex<Vec<(PathBuf, String, Arc<CachedFile>)>>,
    counters: Counters,
    names: RwLock<HashMap<PathBuf, NamesEntry>>,
    /// Sum of the weights of `names`; changed only under its write lock.
    names_bytes: AtomicU64,
}

impl Default for ParseCache {
    fn default() -> Self {
        Self::new(usize::MAX, 2 * 1024 * 1024)
    }
}

impl ParseCache {
    pub fn new(max_entries: usize, max_file_bytes: u64) -> Self {
        ParseCache {
            entries: RwLock::new(HashMap::new()),
            clock: AtomicU64::new(0),
            max_entries: max_entries.max(1),
            max_bytes: DEFAULT_MAX_BYTES,
            used_bytes: AtomicU64::new(0),
            max_file_bytes,
            store: None,
            pending: Mutex::new(Vec::new()),
            counters: Counters::default(),
            names: RwLock::new(HashMap::new()),
            names_bytes: AtomicU64::new(0),
        }
    }

    /// Memory [`SymbolNames`] may take out of the budget: half of it, which
    /// holds the names of a tree about ten times larger than the files that
    /// fit in the other half.
    fn max_names_bytes(&self) -> usize {
        self.max_bytes / 2
    }

    /// Memory left for cached files once the names have theirs.
    fn max_file_cache_bytes(&self) -> usize {
        self.max_bytes
            .saturating_sub(self.names_bytes.load(Ordering::Relaxed) as usize)
    }

    /// Keep cached files and symbol names within about `max_bytes` of memory,
    /// evicting the least recently used files beyond that.
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Approximate memory held by cached files.
    pub fn used_bytes(&self) -> usize {
        self.used_bytes.load(Ordering::Relaxed) as usize
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
        if let Ok(mut entries) = self.entries.write()
            && let Some(old) = entries.remove(path)
        {
            self.used_bytes
                .fetch_sub(old.weight as u64, Ordering::Relaxed);
        }
        if let Ok(mut names) = self.names.write()
            && let Some(old) = names.remove(path)
        {
            self.names_bytes
                .fetch_sub(old.weight as u64, Ordering::Relaxed);
        }
    }

    /// The symbol names of `path`, from memory while its modification time
    /// and size are unchanged, otherwise from [`Self::get`]. Once the names
    /// budget is full, the names of further files are not kept: a search that
    /// walks more files than fit then still finds the first ones in memory,
    /// where evicting the oldest would leave it none.
    pub fn symbol_names(&self, path: &Path) -> Result<Arc<SymbolNames>, CacheError> {
        let (len, mtime) = stat(path)?;
        if let Ok(names) = self.names.read()
            && let Some(entry) = names.get(path)
            && entry.mtime == mtime
            && entry.len == len
        {
            self.counters.memory_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(Arc::clone(&entry.names));
        }
        let file = self.get(path)?;
        let names = Arc::new(SymbolNames::new(&file.parsed));
        if let Ok(mut map) = self.names.write() {
            if let Some(old) = map.remove(path) {
                self.names_bytes
                    .fetch_sub(old.weight as u64, Ordering::Relaxed);
            }
            let weight = names.weight();
            let used = self.names_bytes.load(Ordering::Relaxed) as usize;
            if used + weight <= self.max_names_bytes() {
                self.names_bytes.fetch_add(weight as u64, Ordering::Relaxed);
                map.insert(
                    path.to_path_buf(),
                    NamesEntry {
                        mtime,
                        len,
                        weight,
                        names: Arc::clone(&names),
                    },
                );
            }
        }
        Ok(names)
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
        let (len, mtime) = stat(path)?;

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
                // A timeout depends on the machine and its load, not on the
                // content, so the next session gets another try.
                if let Some(blob) = blob
                    && !file.parsed.incomplete
                {
                    self.queue_write(path, blob, &file);
                }
                file
            }
        };

        if let Ok(mut entries) = self.entries.write() {
            let weight = file.weight();
            if let Some(old) = entries.remove(path) {
                self.used_bytes
                    .fetch_sub(old.weight as u64, Ordering::Relaxed);
            }
            self.make_room(&mut entries, weight);
            self.used_bytes.fetch_add(weight as u64, Ordering::Relaxed);
            entries.insert(
                path.to_path_buf(),
                Entry {
                    mtime,
                    len,
                    last_used: AtomicU64::new(self.tick()),
                    weight,
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

/// Size and modification time of `path`, which must be a regular file:
/// reading a FIFO would block until something writes to it.
fn stat(path: &Path) -> Result<(u64, SystemTime), CacheError> {
    let io_err = |source| CacheError::Io {
        path: path.to_path_buf(),
        source,
    };
    let meta = std::fs::metadata(path).map_err(io_err)?;
    let mtime = meta.modified().map_err(io_err)?;
    if !meta.is_file() {
        return Err(CacheError::NotAFile(path.to_path_buf()));
    }
    Ok((meta.len(), mtime))
}

impl ParseCache {
    /// Evict the least recently used entries until one of `incoming` bytes
    /// fits. Past the budget, it evicts down to 7/8 of it at once: a search
    /// walking more files than fit would otherwise scan the whole map for
    /// every file it adds, under the write lock.
    fn make_room(&self, entries: &mut HashMap<PathBuf, Entry>, incoming: usize) {
        let over = |entries: &HashMap<PathBuf, Entry>, max_entries: usize, max_bytes: usize| {
            entries.len() >= max_entries || self.used_bytes() + incoming > max_bytes
        };
        let budget = self.max_file_cache_bytes();
        if entries.is_empty() || !over(entries, self.max_entries, budget) {
            return;
        }
        let max_entries = self.max_entries - self.max_entries / 8;
        let max_bytes = budget - budget / 8;
        let mut by_age: Vec<(u64, PathBuf)> = entries
            .iter()
            .map(|(p, e)| (e.last_used.load(Ordering::Relaxed), p.clone()))
            .collect();
        by_age.sort_unstable_by_key(|(used, _)| *used);
        for (_, path) in by_age {
            if entries.is_empty() || !over(entries, max_entries, max_bytes) {
                break;
            }
            if let Some(old) = entries.remove(&path) {
                self.used_bytes
                    .fetch_sub(old.weight as u64, Ordering::Relaxed);
            }
        }
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
    fn evicts_to_stay_within_the_memory_budget() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..4)
            .map(|i| {
                let p = dir.path().join(format!("f{i}.py"));
                fs::write(&p, format!("def f{i}(): pass\n{}", "#\n".repeat(500))).unwrap();
                p
            })
            .collect();
        let one = ParseCache::default();
        one.get(&paths[0]).unwrap();
        let weight = one.used_bytes();
        assert!(weight > 1000, "{weight}");

        let cache = ParseCache::default().with_max_bytes(weight * 2 + weight / 2);
        for p in &paths {
            cache.get(p).unwrap();
        }
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.used_bytes(), weight * 2);
        cache.invalidate(&paths[3]);
        assert_eq!((cache.len(), cache.used_bytes()), (1, weight));
    }

    #[test]
    fn a_stored_file_round_trips_through_its_header() {
        let parsed = parser::parse_source(parser::Lang::Rust, "struct S;\nfn f() {}\n").unwrap();
        let blob = "ce013625030ba8dba906f756967f9e9ca394464a";
        let encoded = store_format::encode(&parsed);
        for (lang, has_errors, incomplete) in [
            (parser::Lang::Rust, false, false),
            (parser::Lang::Tsx, true, false),
            (parser::Lang::CSharp, false, true),
            (parser::Lang::Cpp, true, true),
        ] {
            let stored = Stored {
                blob,
                lang,
                has_errors,
                incomplete,
                parsed: &encoded,
            };
            assert_eq!(Stored::decode(&stored.encode()), Some(stored));
        }

        // Too short for a header, a hash length longer than the record, and
        // an unknown language: all are a miss, never a partial file.
        for damaged in [
            &[0x01][..],
            &[0x01, 0x02],
            &[0x01, 0x00, 0xff, 0x00],
            &[0xff, 0x00, 0x01, b'a'],
        ] {
            assert_eq!(Stored::decode(damaged), None);
        }
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
    fn prune_removes_only_missing_files_under_root() {
        let dir = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(&dir.path().join("store")).unwrap();
        let root = dir.path().join("ws");
        let other = dir.path().join("ws-other");
        for d in [&root, &other] {
            fs::create_dir_all(d).unwrap();
        }
        let (kept, gone, outside) = (root.join("a.rs"), root.join("b.rs"), other.join("c.rs"));
        let cache = ParseCache::default().with_store(store.clone());
        for p in [&kept, &gone, &outside] {
            fs::write(p, "fn f() {}\n").unwrap();
            cache.get(p).unwrap();
        }
        cache.flush();
        fs::remove_file(&gone).unwrap();
        fs::remove_file(&outside).unwrap();

        assert_eq!(store.prune_missing(&root).unwrap(), 1);
        assert_eq!(store.prune_missing(&root).unwrap(), 0);
        let rtxn = store.env.read_txn().unwrap();
        let key = |p: &Path| {
            store
                .db
                .get(&rtxn, PersistentStore::key(p))
                .unwrap()
                .is_some()
        };
        assert!(key(&kept));
        assert!(!key(&gone));
        // Outside the root, even though `ws` is a prefix of `ws-other`.
        assert!(key(&outside));
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

    #[test]
    fn symbol_names_keep_the_first_files_that_fit() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..4)
            .map(|i| {
                let p = dir.path().join(format!("f{i}.py"));
                fs::write(&p, format!("def f{i}(): pass\n")).unwrap();
                p
            })
            .collect();
        let weight =
            SymbolNames::new(&ParseCache::default().get(&paths[0]).unwrap().parsed).weight();
        // Half the budget goes to names: room for two files' names.
        let cache = ParseCache::default().with_max_bytes(4 * weight + 4);
        let names_bytes = || cache.names_bytes.load(Ordering::Relaxed) as usize;
        let first: Vec<_> = paths
            .iter()
            .map(|p| cache.symbol_names(p).unwrap())
            .collect();
        assert_eq!(first[3].name(0), Some("f3"));
        // A second scan finds the first two in memory, rather than each file
        // evicting the one the scan needs next.
        for (i, p) in paths.iter().enumerate() {
            let again = cache.symbol_names(p).unwrap();
            assert_eq!(Arc::ptr_eq(&first[i], &again), i < 2, "file {i}");
        }
        assert_eq!(names_bytes(), 2 * weight);

        // A changed file gets new names in place of the old ones.
        fs::write(&paths[0], "def gg0(): pass\n").unwrap();
        assert_eq!(cache.symbol_names(&paths[0]).unwrap().name(0), Some("gg0"));
        assert_eq!(names_bytes(), 2 * weight + 1);
        cache.invalidate(&paths[0]);
        assert_eq!(names_bytes(), weight);
    }
}
