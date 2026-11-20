// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/storage/src/lib.rs

// Aliased to `core` so the `crate::core::...` paths in the modules below
// resolve exactly as they did inside the monolith (import resolution only).
use y4p_core as core;

mod cache;
mod db;
mod schema;

use rusqlite::{Connection, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use sha3::{Digest, Sha3_256};

use cache::{Adoption, FileCache};
use db::SqliteStore;
use schema::SchemaManager;

use crate::core::constants::{CACHE_QUOTA_BYTES, MAX_PAYLOAD_BYTES, SQLITE_TIMEOUT_MS};

/// A single metadata row, shared by `fetch_metadata` and `search_metadata` so
/// callers (and future callers) don't have to keep re-deriving the same
/// 6-tuple shape by hand. The trailing `bool` is `is_pinned` (G-10).
pub type MetaRow = (i64, i64, String, i64, Option<String>, bool);

/// Identifies where a record's binary payload currently lives.
///
/// `InlineBlob` means the bytes are in the row's `content` column and are
/// read through `get_content_by_id`. `CacheFile` means they are in
/// `<hash>.cache` on disk, which egress can stream with `sendfile(2)`
/// directly instead of materialising a `Vec<u8>`.
#[derive(Debug, PartialEq, Eq)]
pub enum ContentLocation {
    InlineBlob(i64),
    CacheFile(PathBuf),
}

/// Public façade: composes `SqliteStore` (pure SQL) and `FileCache` (on-disk
/// binary cache) behind the exact interface external callers already use.
/// `db.rs` never touches the filesystem beyond the SQLite file itself, and
/// `cache.rs` never touches SQLite — this is the only place that coordinates
/// both, so a caller only ever needs to know `ClipboardDb`.
pub struct ClipboardDb {
    store: SqliteStore,
    cache: FileCache,
}

impl ClipboardDb {
    /// Open the database with optimized configurations.
    /// Returns Result to allow the caller to handle connection failures gracefully.
    pub fn open() -> Result<Self, String> {
        let db_path = crate::core::get_db_path();
        let cache_dir = crate::core::get_cache_dir();

        let mut conn = Connection::open(&db_path)
            .map_err(|e| format!("sqlite connection failed: {}", e))?;

        // Secure file permissions
        if let Ok(metadata) = fs::metadata(&db_path) {
            let mut perms = metadata.permissions();
            if perms.mode() != 0o600 {
                perms.set_mode(0o600);
                let _ = fs::set_permissions(&db_path, perms);
            }
        }

        SchemaManager::initialize(&mut conn, SQLITE_TIMEOUT_MS)?;

        Ok(Self { store: SqliteStore::new(conn), cache: FileCache::new(cache_dir) })
    }

    /// Public wrapper for raw data insertion. Returns the persistent row ID
    /// of the record now representing this content (either newly inserted,
    /// or the pre-existing record if this was a duplicate by hash).
    pub fn insert_raw(&mut self, mime: &str, data: &[u8]) -> Result<i64, String> {
        let mut hasher = Sha3_256::new();
        hasher.update(data);
        let hash = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>();
        self.insert_with_hash(mime, data, &hash, crate::core::get_max_history())
    }

    /// Optimized insertion utilizing a pre-computed hash and atomic transactions.
    ///
    /// BUGFIX: previously returned `Result<()>`, forcing every caller that
    /// needed the inserted row's ID (e.g. `store` syncing the new entry back
    /// to the daemon) to run a *separate* `fetch_metadata(1)` query
    /// afterwards and assume "the most-recently-timestamped row is the one I
    /// just inserted". That assumption races against the daemon's own
    /// worker thread, which can insert a newer clipboard entry in the
    /// window between this call returning and that follow-up query running
    /// — causing the wrong entry to be restored to the clipboard. Returning
    /// the ID directly, computed inside the same transaction via
    /// `last_insert_rowid()`, removes the race entirely.
    ///
    /// Skipped inserts (empty payload, sensitive MIME) return `Ok(-1)` as an
    /// explicit "nothing to reference" sentinel — there is no record for a
    /// caller to act on in that case.
    ///
    /// A cache-class payload is written to `<hash>.cache` *before* its row,
    /// and a failed write fails the whole call: a row must never exist whose
    /// bytes the cache does not hold. If the row then fails to commit, a file
    /// this call placed is taken back out.
    pub fn insert_with_hash(&mut self, mime: &str, data: &[u8], hash: &str, max_history: usize) -> Result<i64, String> {
        // The 32 MiB cap is an ingestion invariant, but this is the single
        // storage entry point for every producer (daemon worker, CLI `store`,
        // tests), so it is enforced here as well rather than trusted.
        if data.len() as u64 > MAX_PAYLOAD_BYTES {
            return Err(format!(
                "payload of {} bytes exceeds the {}-byte limit",
                data.len(),
                MAX_PAYLOAD_BYTES
            ));
        }
        let cache_class = !db::is_skipped(mime, data.len() as u64) && db::is_cache_class(mime);
        let placed = if cache_class {
            Some(self.cache.store(hash, data).map_err(|e| format!("cache write failed: {}", e))?)
        } else {
            None
        };

        let outcome = match self.store.upsert_record(mime, data, hash, max_history) {
            Ok(outcome) => outcome,
            Err(e) => {
                if placed == Some(Adoption::Placed) {
                    self.cache.remove(hash);
                }
                return Err(e);
            }
        };
        if outcome.real_id == -1 {
            // Nothing was stored, so nothing may stay cached for it.
            if placed == Some(Adoption::Placed) {
                self.cache.remove(hash);
            }
            return Ok(-1);
        }
        self.cache.remove_many(&outcome.expired_hashes);
        if cache_class {
            // Best effort: the row is already committed, so a quota problem must
            // never turn a successful clipboard write into an error.
            if let Err(e) = self.enforce_cache_quota(CACHE_QUOTA_BYTES, outcome.real_id) {
                eprintln!("{}cache quota enforcement failed: {}", crate::core::constants::LOG_ERROR, e);
            }
        }

        Ok(outcome.real_id)
    }

    /// Evicts least-recently-used unpinned cache rows until the cache fits
    /// `quota_bytes`, sparing `keep_id`.
    ///
    /// The loop takes the oldest unpinned, cache-backed row
    /// (`SqliteStore::oldest_unpinned_cache_row`), deletes the row and removes
    /// its `<hash>.cache`. Pinned rows and `keep_id` are never candidates, so a
    /// history made only of pinned images may legitimately stay above the
    /// quota: the bound applies to disposable data, and a pin is never broken
    /// to satisfy it.
    fn enforce_cache_quota(&mut self, quota_bytes: u64, keep_id: i64) -> Result<(), String> {
        while self.cache.total_bytes() > quota_bytes {
            let Some((id, hash)) = self.store.oldest_unpinned_cache_row(keep_id) else { break };
            let (deleted, _) = self.store.delete_by_id(id).map_err(|e| e.to_string())?;
            if !deleted {
                break;
            }
            self.cache.remove(&hash);
        }
        Ok(())
    }

    /// Search metadata with protection against full BLOB scans.
    /// Optimized to only scan content when mime is text-based and preview is insufficient.
    ///
    /// BUGFIX: previously returned rows in isolation, and `cli/search.rs`
    /// assigned each hit a LOCAL index via `.enumerate()` over just the
    /// search results. That index space does *not* match the absolute
    /// history index `list`/`copy-to`/`delete`/`show` use (which is the
    /// position within the *full* MRU history). Running `search foo` and
    /// then feeding one of its displayed indices into `copy-to <n>` (without
    /// `--id`) could therefore restore a completely different entry than
    /// the one shown.
    ///
    /// Fixed by computing each row's absolute MRU index (count of rows with
    /// a strictly later timestamp, matching exactly how `list.rs` derives
    /// it) so a search result's displayed index is always consistent with
    /// `list`'s.
    pub fn search_metadata(&self, queries: &[String], limit: usize) -> Vec<(usize, MetaRow)> {
        if queries.is_empty() { return Vec::new(); }
        self.store.search_metadata(queries, limit)
    }

    /// Sorts keywords into (valid, invalid) so the CLI can drop typos before
    /// running the full AND search. See `SqliteStore::validate_keywords`.
    pub fn validate_keywords(&self, keywords: &[String]) -> (Vec<String>, Vec<String>) {
        self.store.validate_keywords(keywords)
    }

    pub fn fetch_metadata(&self, limit: usize) -> Vec<MetaRow> {
        self.store.fetch_metadata(limit)
    }

    pub fn get_content_by_id(&self, id: i64) -> Option<(String, Vec<u8>)> {
        let (mime, db_content, hash) = self.store.get_row_content(id)?;
        if let Some(data) = db_content {
            Some((mime, data))
        } else {
            self.cache.read(&hash).map(|data| (mime, data))
        }
    }

    /// See `ContentLocation` docs: a non-materializing counterpart to
    /// `get_content_by_id`. Used by the kernel-level egress path
    /// (`daemon::handle_restore_request` / `wayland::handlers::data_control::source`)
    /// to decide whether a `copy-to` can be served via `sendfile(2)`
    /// directly from the cache file, or must go through the existing
    /// in-memory path (small/inline payloads).
    ///
    /// The cache file is checked to be there before its path is handed out:
    /// egress would otherwise offer a descriptor that cannot be opened and
    /// lose the paste silently. A missing file is an `Err` naming the row and
    /// the expected path, never `Ok(Some(..))`.
    pub fn locate_content(&self, id: i64) -> Result<Option<(String, ContentLocation)>, String> {
        let Some((mime, has_content, hash)) = self.store.get_row_location(id) else {
            return Ok(None);
        };
        if has_content {
            return Ok(Some((mime, ContentLocation::InlineBlob(id))));
        }
        let path = self.cache.path(&hash);
        if !path.is_file() {
            return Err(format!("cache file for row {} is missing at {}", id, path.display()));
        }
        Ok(Some((mime, ContentLocation::CacheFile(path))))
    }

    /// Update record timestamp. Standardized to &mut self for state consistency.
    pub fn update_timestamp(&mut self, id: i64) -> Result<()> {
        self.store.update_timestamp(id)
    }

    /// Remove record by ID. Standardized to &mut self for state consistency.
    pub fn delete_by_id(&mut self, id: i64) -> Result<bool> {
        let (deleted, hash) = self.store.delete_by_id(id)?;
        if let Some(h) = hash {
            self.cache.remove(&h);
        }
        Ok(deleted)
    }

    /// Clear all history and reclaim disk space. Standardized to &mut self.
    ///
    /// Memory is reclaimed too: `VACUUM` frees pages inside SQLite and the cache
    /// directory's files are gone, but neither returns resident memory to the
    /// kernel by itself. The store drops its page cache (`PRAGMA shrink_memory`)
    /// and the allocator is then asked to give back what that freed
    /// (`malloc_trim(3)` where it exists).
    pub fn wipe(&mut self) -> Result<()> {
        self.store.wipe()?;
        let _ = self.cache.clear();
        crate::core::memory::release_unused_heap();
        Ok(())
    }

    /// Safely retrieve total record count. Removed unwrap() to prevent daemon panics.
    pub fn get_total_count(&self) -> usize {
        self.store.get_total_count()
    }

    /// G-10: pin/unpin a record by its immutable ID. Pinned records are
    /// exempt from `upsert_record`'s automatic rotation eviction; `delete`
    /// and `wipe` remain unaffected by pin state (see `cli::pin`/`cli::cleaning`).
    pub fn set_pin_by_id(&mut self, id: i64, is_pinned: bool) -> Result<bool, String> {
        self.store.set_pinned(id, is_pinned)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    /// A database on a real file (so a second connection can sabotage it) with
    /// its cache directory, all private to the test, as in the daemon.
    struct Fixture {
        db: ClipboardDb,
        root: PathBuf,
        cache: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("y4p-storage-{}-{}", std::process::id(), tag));
            let _ = fs::remove_dir_all(&root);
            let cache = root.join("cache");
            fs::create_dir_all(&cache).unwrap();
            let mut conn = Connection::open(root.join("y4p.db")).unwrap();
            SchemaManager::initialize(&mut conn, 1000).unwrap();
            let db = ClipboardDb { store: SqliteStore::new(conn), cache: FileCache::new(cache.clone()) };
            Self { db, root, cache }
        }

        fn cache_file(&self, bytes: &[u8]) -> PathBuf {
            self.cache.join(format!("{}.cache", hash_of(bytes)))
        }

        fn cache_files(&self) -> usize {
            fs::read_dir(&self.cache).unwrap().filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|x| x == "cache")).count()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + i / 5) % 253) as u8 ^ seed).collect()
    }

    fn hash_of(bytes: &[u8]) -> String {
        crate::core::payload::to_hex(Sha3_256::digest(bytes).as_slice())
    }

    fn inode(path: &Path) -> u64 {
        fs::metadata(path).unwrap().ino()
    }

    // --- the storage-side payload ceiling ---

    #[test]
    fn a_payload_over_the_limit_is_refused_before_any_store_is_touched() {
        let mut f = Fixture::new("oversize");
        let oversized = vec![0u8; usize::try_from(MAX_PAYLOAD_BYTES).unwrap() + 1];

        let err = f.db.insert_with_hash("text/plain", &oversized, "h-oversize", 100).unwrap_err();

        assert!(err.contains("exceeds"), "{err}");
        assert_eq!(f.db.get_total_count(), 0, "no row may be written");
        assert_eq!(f.cache_files(), 0, "no cache file may be written");
    }

    // --- cache deduplication and rotation through `insert_raw` ---

    #[test]
    fn a_duplicate_image_keeps_the_existing_cache_file() {
        let mut f = Fixture::new("dup");
        let bytes = pattern(50_000, 3);
        let first = f.db.insert_raw("image/png", &bytes).unwrap();
        let target = f.cache_file(&bytes);
        let ino = inode(&target);

        let second = f.db.insert_raw("image/png", &bytes).unwrap();

        assert_eq!(second, first, "deduplicated by hash");
        assert_eq!(f.db.get_total_count(), 1);
        assert_eq!(inode(&target), ino, "the existing cache file is not replaced");
        assert_eq!(f.cache_files(), 1);
    }

    #[test]
    fn a_row_whose_cache_file_went_missing_is_healed_by_the_next_copy() {
        let mut f = Fixture::new("heal");
        let bytes = pattern(20_000, 4);
        let id = f.db.insert_raw("image/png", &bytes).unwrap();
        fs::remove_file(f.cache_file(&bytes)).unwrap();
        assert!(f.db.get_content_by_id(id).is_none());

        assert_eq!(f.db.insert_raw("image/png", &bytes).unwrap(), id);
        assert_eq!(f.db.get_content_by_id(id).unwrap().1, bytes);
    }

    #[test]
    fn rotation_removes_the_cache_file_of_the_image_it_evicts() {
        let mut f = Fixture::new("rotate");
        let (old, new) = (pattern(10_000, 5), pattern(10_000, 6));
        let old_hash = hash_of(&old);
        f.db.insert_with_hash("image/png", &old, &old_hash, 1).unwrap();
        f.db.insert_with_hash("image/png", &new, &hash_of(&new), 1).unwrap();

        assert_eq!(f.db.get_total_count(), 1);
        assert!(!f.cache_file(&old).exists(), "the evicted image's file is gone");
        assert!(f.cache_file(&new).exists());
    }

    #[test]
    fn a_payload_storage_declines_leaves_nothing_behind() {
        let mut f = Fixture::new("declined");
        let bytes = pattern(9_000, 8);
        assert_eq!(f.db.insert_raw("image/x-secret", &bytes).unwrap(), -1);
        assert_eq!(f.db.insert_raw("application/x-secret", &bytes).unwrap(), -1);
        assert_eq!(f.db.insert_raw("image/png", b"").unwrap(), -1);

        assert_eq!(f.db.get_total_count(), 0);
        assert_eq!(f.cache_files(), 0, "a declined image must not be cached");
    }

    // --- cache quota: LRU eviction of unpinned cache rows ---

    fn insert_image(f: &mut Fixture, bytes: &[u8]) -> i64 {
        f.db.insert_with_hash("image/png", bytes, &hash_of(bytes), 100).unwrap()
    }

    #[test]
    fn the_cache_quota_evicts_the_oldest_unpinned_image_first() {
        let mut f = Fixture::new("quota-lru");
        let (a, b, c) = (pattern(10_000, 20), pattern(10_000, 21), pattern(10_000, 22));
        let id_a = insert_image(&mut f, &a);
        let id_b = insert_image(&mut f, &b);
        let id_c = insert_image(&mut f, &c);

        // Directly enforce a one-image quota: the two oldest are disposable.
        f.db.enforce_cache_quota(10_000, id_c).unwrap();

        assert!(f.db.get_content_by_id(id_a).is_none(), "the oldest image went first");
        assert!(!f.cache_file(&a).exists(), "its cache file went with it");
        assert!(f.db.get_content_by_id(id_b).is_none(), "the next-oldest followed");
        assert!(f.db.get_content_by_id(id_c).is_some(), "the newest is kept");
        assert!(f.cache_file(&c).exists());
        assert_eq!(f.cache_files(), 1);
    }

    #[test]
    fn recopying_an_image_refreshes_its_lru_position() {
        let mut f = Fixture::new("quota-recopy");
        let (a, b, c) = (pattern(10_000, 23), pattern(10_000, 24), pattern(10_000, 25));
        let id_a = insert_image(&mut f, &a);
        let id_b = insert_image(&mut f, &b);
        // A distinct millisecond, so the refreshed timestamp is unambiguous.
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(insert_image(&mut f, &a), id_a, "the copy is a duplicate, not a new row");
        let id_c = insert_image(&mut f, &c);

        // Room for two images: only the oldest unused one (b) is evicted.
        f.db.enforce_cache_quota(20_000, id_c).unwrap();

        assert!(f.db.get_content_by_id(id_b).is_none(), "b became the least recently used");
        assert!(f.db.get_content_by_id(id_a).is_some(), "a's recopy moved it out of the way");
        assert!(f.db.get_content_by_id(id_c).is_some());
    }

    #[test]
    fn the_cache_quota_never_evicts_a_pinned_image() {
        let mut f = Fixture::new("quota-pin");
        let (a, b, c) = (pattern(10_000, 26), pattern(10_000, 27), pattern(10_000, 28));
        let id_a = insert_image(&mut f, &a);
        f.db.set_pin_by_id(id_a, true).unwrap();
        let id_b = insert_image(&mut f, &b);
        let id_c = insert_image(&mut f, &c);

        f.db.enforce_cache_quota(10_000, id_c).unwrap();

        assert!(f.db.get_content_by_id(id_a).is_some(), "a pin is absolute, even over quota");
        assert!(f.cache_file(&a).exists());
        assert!(f.db.get_content_by_id(id_b).is_none(), "the oldest unpinned image is the victim");
        assert!(f.db.get_content_by_id(id_c).is_some(), "keep_id is spared");
    }

    #[test]
    fn the_cache_quota_never_evicts_the_row_that_triggered_it() {
        let mut f = Fixture::new("quota-keep");
        let (a, b) = (pattern(10_000, 29), pattern(10_000, 30));
        let id_a = insert_image(&mut f, &a);
        let id_b = insert_image(&mut f, &b);

        f.db.enforce_cache_quota(0, id_b).unwrap();

        assert!(f.db.get_content_by_id(id_a).is_none(), "everything disposable goes at a zero quota");
        assert!(f.db.get_content_by_id(id_b).is_some(), "keep_id survives even a zero quota");
        assert!(f.cache_file(&b).exists());
    }

    // --- insert_with_hash: the cache write precedes the row ---

    #[test]
    fn a_memory_image_is_cached_by_the_time_its_row_exists() {
        let mut f = Fixture::new("store-first");
        let bytes = pattern(12_000, 16);
        let id = f.db.insert_raw("image/png", &bytes).unwrap();

        assert_eq!(fs::read(f.cache_file(&bytes)).unwrap(), bytes);
        let Some((mime, ContentLocation::CacheFile(path))) = f.db.locate_content(id).unwrap() else { panic!("an image lives in a cache file") };
        assert_eq!((mime.as_str(), path), ("image/png", f.cache_file(&bytes)));
    }

    #[test]
    fn a_cache_write_that_fails_leaves_no_row() {
        let mut f = Fixture::new("store-fail");
        // Without the cache directory, writing `<hash>.cache` cannot succeed.
        fs::remove_dir_all(&f.cache).unwrap();
        let bytes = pattern(4_000, 17);

        let err = f.db.insert_raw("image/png", &bytes).unwrap_err();

        assert!(err.contains("cache write failed"), "{err}");
        assert_eq!(f.db.get_total_count(), 0, "no row may point at a payload the cache never held");
    }

    #[test]
    fn a_row_that_fails_to_commit_takes_the_stored_file_back_out() {
        let mut f = Fixture::new("store-rollback");
        // Sabotage from a second connection: the insert then fails after the
        // file has been written.
        Connection::open(f.root.join("y4p.db")).unwrap().execute_batch("DROP TABLE clipboard;").unwrap();
        let bytes = pattern(6_000, 18);

        assert!(f.db.insert_raw("image/png", &bytes).is_err());

        assert!(!f.cache_file(&bytes).exists(), "no cache file without a row");
    }

    #[test]
    fn locating_a_record_whose_cache_file_is_missing_is_an_error() {
        let mut f = Fixture::new("locate-missing");
        let bytes = pattern(3_000, 19);
        let id = f.db.insert_raw("image/png", &bytes).unwrap();
        fs::remove_file(f.cache_file(&bytes)).unwrap();

        let err = f.db.locate_content(id).unwrap_err();

        assert!(err.contains("missing"), "{err}");
        assert!(err.contains(&id.to_string()), "{err}");
    }

    #[test]
    fn locating_a_record_that_does_not_exist_is_none_not_an_error() {
        let f = Fixture::new("locate-absent");
        assert!(f.db.locate_content(999_999).unwrap().is_none());
    }

    // --- wipe ---

    #[test]
    fn wipe_empties_rows_and_cache_and_the_store_stays_usable() {
        let mut f = Fixture::new("wipe");
        let bytes = pattern(30_000, 12);
        f.db.insert_raw("image/png", &bytes).unwrap();
        f.db.insert_raw("text/plain", b"hello").unwrap();

        f.db.wipe().unwrap();

        assert_eq!(f.db.get_total_count(), 0);
        assert_eq!(f.cache_files(), 0);
        assert!(f.db.insert_raw("text/plain", b"after the wipe").unwrap() > 0, "the connection survives shrink_memory");
    }

    // --- large payloads through the facade: inline writes only ---

    #[test]
    fn a_large_non_image_is_stored_inline_and_served_whole_from_the_row() {
        let mut f = Fixture::new("chunked-spool");
        let bytes = pattern(500_000, 13);
        let id = f.db.insert_raw("application/pdf", &bytes).unwrap();

        assert!(matches!(f.db.locate_content(id).unwrap(), Some((_, ContentLocation::InlineBlob(_)))), "still inline: the bytes are in the database");
        assert_eq!(f.db.get_content_by_id(id).unwrap(), ("application/pdf".to_owned(), bytes));
        assert_eq!(f.cache_files(), 0);
        let usage = f.db.store.chunk_usage();
        assert_eq!((usage.chunks, usage.manifest), (0, 0), "no chunk or manifest row is written");
    }

    #[test]
    fn insert_raw_of_an_edited_text_stores_both_rows_inline() {
        let mut f = Fixture::new("chunked-edit");
        let original: String = (0..60_000u32).map(|i| format!("{:x} ", i.wrapping_mul(2_654_435_761))).collect();
        let mut edited = original.clone();
        edited.insert_str(original.len() / 2, "INSERTED ");
        let a = f.db.insert_raw("text/plain", original.as_bytes()).unwrap();
        let b = f.db.insert_raw("text/plain", edited.as_bytes()).unwrap();

        assert_ne!(a, b);
        assert!(original.len() > 300_000, "the payload must span many former chunk boundaries");
        assert_eq!(f.db.get_content_by_id(a).unwrap().1, original.into_bytes());
        assert_eq!(f.db.get_content_by_id(b).unwrap().1, edited.into_bytes());
        let usage = f.db.store.chunk_usage();
        assert_eq!((usage.chunks, usage.manifest), (0, 0), "each row keys its own hash, whole");
    }

    #[test]
    fn large_inline_rows_survive_reopening_the_database_and_are_searchable() {
        let mut f = Fixture::new("chunked-reopen");
        let mut text = vec![b'q'; 200_000];
        text[120_000..120_011].copy_from_slice(b"reopenedkey");
        let id = f.db.insert_raw("text/plain", &text).unwrap();

        let mut conn = Connection::open(f.root.join("y4p.db")).unwrap();
        SchemaManager::initialize(&mut conn, 1000).unwrap();
        let reopened = ClipboardDb { store: SqliteStore::new(conn), cache: FileCache::new(f.cache.clone()) };

        assert_eq!(reopened.get_content_by_id(id).unwrap().1, text);
        let hits = reopened.search_metadata(&["reopenedkey".to_string()], 10);
        assert_eq!(hits.iter().map(|h| h.1.0).collect::<Vec<_>>(), vec![id]);
    }

    #[test]
    fn deleting_and_wiping_through_the_facade_leave_the_chunk_tables_empty() {
        let mut f = Fixture::new("chunked-gc");
        let a = f.db.insert_raw("application/octet-stream", &pattern(300_000, 14)).unwrap();
        f.db.insert_raw("application/octet-stream", &pattern(300_000, 15)).unwrap();

        let written = f.db.store.chunk_usage();
        assert_eq!((written.chunks, written.manifest), (0, 0), "new rows never split their bytes");

        assert!(f.db.delete_by_id(a).unwrap());
        assert!(f.db.get_content_by_id(a).is_none());
        let after_delete = f.db.store.chunk_usage();
        assert_eq!((after_delete.chunks, after_delete.manifest), (0, 0));

        f.db.wipe().unwrap();
        let after_wipe = f.db.store.chunk_usage();
        assert_eq!((after_wipe.chunks, after_wipe.manifest), (0, 0));
    }
}
