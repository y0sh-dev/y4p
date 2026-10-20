// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/storage/src/lib.rs

// Aliased to `core` so the `crate::core::...` paths in the modules below
// resolve exactly as they did inside the monolith (import resolution only).
use y4p_core as core;

mod cache;
mod db;
mod schema;

use rusqlite::{Connection, OpenFlags, Result};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use sha3::{Digest, Sha3_256};

use cache::{Adoption, FileCache};
use db::SqliteStore;
use schema::SchemaManager;

use crate::core::constants::{MAX_PAYLOAD_BYTES, SQLITE_TIMEOUT_MS};
use crate::core::payload::SpooledFile;

/// Reads a spool file that is stored in a row, checking it is what the spooler
/// recorded. Bounded by `MAX_PAYLOAD_BYTES` and by the recorded length, so a
/// file that grew since (or a bogus length) cannot make this allocate without
/// limit.
fn read_spooled(spooled: &SpooledFile) -> Result<Vec<u8>, String> {
    let len = spooled.len();
    if len > MAX_PAYLOAD_BYTES {
        return Err(format!("spooled payload of {} bytes exceeds the {}-byte limit", len, MAX_PAYLOAD_BYTES));
    }
    let file = File::open(spooled.path()).map_err(|e| format!("cannot open spool file: {}", e))?;
    let mut data = Vec::with_capacity(usize::try_from(len).map_err(|e| e.to_string())?);
    file.take(len + 1).read_to_end(&mut data).map_err(|e| format!("cannot read spool file: {}", e))?;
    if data.len() as u64 != len {
        return Err(format!("spool file holds {} bytes, expected {}", data.len(), len));
    }
    Ok(data)
}

/// A single metadata row, shared by `fetch_metadata` and `search_metadata` so
/// callers (and future callers) don't have to keep re-deriving the same
/// 6-tuple shape by hand. The trailing `bool` is `is_pinned` (G-10).
pub type MetaRow = (i64, i64, String, i64, Option<String>, bool);

/// Identifies where a record's binary payload currently lives.
///
/// STRUCTURAL PREP for the planned Incremental BLOB I/O and zero-copy egress
/// (sendfile(2)/splice(2)) milestones: today, `get_content_by_id` always
/// materializes the full payload into a `Vec<u8>` regardless of size or
/// where it lives, which is exactly what those two milestones need to stop
/// doing. Future streaming code should branch on `ContentLocation` instead
/// of re-deriving "is it inline or cached on disk" logic itself:
///   - `InlineBlob(rowid)`: open with `sqlite3_blob_open` against `rowid`
///     for fixed-size incremental reads, instead of `SELECT content`.
///   - `CacheFile(path)`: open the path directly and `sendfile(2)`/
///     `splice(2)` it straight to the destination fd, without ever copying
///     it through a userspace `Vec<u8>`.
///
/// WIRED IN: `daemon::handle_restore_request` uses `locate_content` to pick
/// between the two, and `wayland::handlers::data_control::source` sends a
/// `CacheFile` payload via `sendfile(2)` — see that module for the actual
/// zero-copy transfer. `InlineBlob` still goes through the pre-existing
/// `get_content_by_id` (`Vec<u8>`) path rather than `sqlite3_blob_open`
/// incremental reads: inline rows are, by construction (see
/// `insert_with_hash`), never the large-binary case this exists for, so
/// that half of the original plan was intentionally not pursued — see
/// PERFORMANCE.md for the cost/benefit writeup.
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

    /// Read-only counterpart to `open`, for callers that never write (the
    /// daemon's `read_db`, future read-only CLI commands): `SQLITE_OPEN_READ_ONLY`
    /// enforces the Single-Writer design at the engine level rather than by
    /// convention alone, and `SQLITE_OPEN_NO_MUTEX` skips connection-internal
    /// locking a connection only ever touched from one thread doesn't need.
    /// No `SQLITE_OPEN_CREATE` — the writer (`open`) must already have run
    /// once to create/migrate the database file this connects to.
    ///
    /// Not yet wired into any caller (daemon's `read_db`, the read-only CLI
    /// commands) by design — this task adds the constructor only, per spec.
    // TODO(v0.3.0): Integrate into CLI read commands and daemon read_db
    #[allow(dead_code)]
    pub fn open_read_only() -> Result<Self, String> {
        let db_path = crate::core::get_db_path();
        let cache_dir = crate::core::get_cache_dir();

        let conn = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ).map_err(|e| format!("sqlite read-only connection failed: {}", e))?;

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
    pub fn insert_with_hash(&mut self, mime: &str, data: &[u8], hash: &str, max_history: usize) -> Result<i64, String> {
        let outcome = self.store.upsert_record(mime, data, hash, max_history)?;
        if outcome.real_id == -1 { return Ok(-1); }

        if outcome.is_image {
            let _ = self.cache.store(hash, data);
        }
        self.cache.remove_many(&outcome.expired_hashes);

        Ok(outcome.real_id)
    }

    /// Stores a payload that is already a finished file (a *spool* file), keyed
    /// by the digest it carries, and takes ownership of that file. Returns what
    /// `insert_with_hash` returns: the row id, or `Ok(-1)` for a payload that is
    /// deliberately not stored (empty, sensitive MIME).
    ///
    /// The storage class decides what happens to the file:
    /// - An **image** (cache class, no text preview needed) is *adopted* into the
    ///   cache as `<hash>.cache` by `rename(2)`: the inode moves, no byte is
    ///   read or written, whatever the size. An identical cache file already
    ///   there is kept and the spool file is unlinked (dedup without overwrite).
    ///   The row is built from the length alone. The file is placed first and
    ///   the row second, so a row never points at a missing file; if the row
    ///   then fails to commit, a file this call placed is taken out again.
    /// - Anything else is read into memory once (it must fit the row) and
    ///   stored like any `insert_with_hash` payload, and the spool file is
    ///   unlinked.
    ///
    /// On every path the caller's `SpooledFile` is consumed: either its file is
    /// the cache entry now, or it has been unlinked. The caller never has to
    /// clean up. The digest is trusted: it was computed while the file was
    /// written (`core::splice`), which is what makes adoption O(1).
    pub fn insert_spooled(&mut self, mime: &str, spooled: SpooledFile, max_history: usize) -> Result<i64, String> {
        if db::is_skipped(mime, spooled.len()) {
            return Ok(-1);
        }
        let hash = spooled.hex_digest();

        if !db::is_detachable(mime) {
            let data = read_spooled(&spooled)?;
            return self.insert_with_hash(mime, &data, &hash, max_history);
        }

        let placed = self.cache.adopt(&hash, spooled.path()).map_err(|e| format!("cache adoption failed: {}", e))?;
        let len = spooled.len();
        match placed {
            // The file has moved: disarm the unlink-on-drop of its old name.
            Adoption::Placed => drop(spooled.into_path()),
            // The cache keeps its own copy: let the spool file be unlinked.
            Adoption::Present => drop(spooled),
        }

        let outcome = match self.store.upsert_detached(mime, len, &hash, max_history) {
            Ok(outcome) => outcome,
            Err(e) => {
                if placed == Adoption::Placed {
                    self.cache.remove(&hash);
                }
                return Err(e);
            }
        };
        self.cache.remove_many(&outcome.expired_hashes);
        Ok(outcome.real_id)
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
    pub fn locate_content(&self, id: i64) -> Option<(String, ContentLocation)> {
        let (mime, has_content, hash) = self.store.get_row_location(id)?;
        if has_content {
            Some((mime, ContentLocation::InlineBlob(id)))
        } else {
            Some((mime, ContentLocation::CacheFile(self.cache.path(&hash))))
        }
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
    use crate::core::constants::DIGEST_LEN;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    /// A database on a real file (so a second connection can sabotage it) with
    /// its cache directory and a spool directory inside it, all private to the
    /// test, as in the daemon.
    struct Fixture {
        db: ClipboardDb,
        root: PathBuf,
        cache: PathBuf,
        spool: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("y4p-storage-{}-{}", std::process::id(), tag));
            let _ = fs::remove_dir_all(&root);
            let cache = root.join("cache");
            let spool = cache.join("spool");
            fs::create_dir_all(&spool).unwrap();
            let mut conn = Connection::open(root.join("y4p.db")).unwrap();
            SchemaManager::initialize(&mut conn, 1000).unwrap();
            let db = ClipboardDb { store: SqliteStore::new(conn), cache: FileCache::new(cache.clone()) };
            Self { db, root, cache, spool }
        }

        fn spooled(&self, name: &str, bytes: &[u8]) -> SpooledFile {
            let path = self.spool.join(name);
            fs::write(&path, bytes).unwrap();
            let digest = <[u8; DIGEST_LEN]>::try_from(Sha3_256::digest(bytes).as_slice()).unwrap();
            SpooledFile::new(path, bytes.len() as u64, digest)
        }

        fn cache_file(&self, bytes: &[u8]) -> PathBuf {
            self.cache.join(format!("{}.cache", crate::core::payload::to_hex(Sha3_256::digest(bytes).as_slice())))
        }

        fn spool_is_empty(&self) -> bool {
            fs::read_dir(&self.spool).unwrap().count() == 0
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

    fn inode(path: &Path) -> u64 {
        fs::metadata(path).unwrap().ino()
    }

    // --- insert_spooled: images are adopted ---

    #[test]
    fn a_spooled_image_is_renamed_into_the_cache_and_its_row_needs_no_bytes() {
        let mut f = Fixture::new("adopt");
        let bytes = pattern(300_000, 1);
        let spooled = f.spooled("1-1.part", &bytes);
        let ino = inode(spooled.path());

        let id = f.db.insert_spooled("image/gif", spooled, 100).unwrap();

        assert!(id > 0);
        let target = f.cache_file(&bytes);
        assert_eq!(inode(&target), ino, "same inode: the spool file was renamed, not copied");
        assert!(f.spool_is_empty());
        let Some((mime, ContentLocation::CacheFile(path))) = f.db.locate_content(id) else { panic!("an image lives in a cache file") };
        assert_eq!((mime.as_str(), path), ("image/gif", target));
        assert_eq!(f.db.get_content_by_id(id).unwrap().1, bytes);
        let meta = f.db.fetch_metadata(10);
        assert_eq!((meta[0].2.as_str(), meta[0].3, meta[0].4.as_deref()), ("image/gif", 300_000, None));
    }

    #[test]
    fn a_spooled_image_makes_the_same_row_as_the_same_bytes_inserted_from_memory() {
        let mut spooled_db = Fixture::new("same-row-a");
        let mut memory_db = Fixture::new("same-row-b");
        let bytes = pattern(70_000, 2);
        let a = spooled_db.db.insert_spooled("image/png", spooled_db.spooled("1-1.part", &bytes), 100).unwrap();
        let b = memory_db.db.insert_raw("image/png", &bytes).unwrap();

        let (a_meta, b_meta) = (spooled_db.db.fetch_metadata(10), memory_db.db.fetch_metadata(10));
        // (id, timestamp, mime, size, preview, pinned): everything but the clock.
        assert_eq!((a, a_meta[0].2.clone(), a_meta[0].3, a_meta[0].4.clone(), a_meta[0].5), (b, b_meta[0].2.clone(), b_meta[0].3, b_meta[0].4.clone(), b_meta[0].5));
        assert_eq!(spooled_db.db.get_content_by_id(a).unwrap(), memory_db.db.get_content_by_id(b).unwrap());
        assert_eq!(fs::read(spooled_db.cache_file(&bytes)).unwrap(), fs::read(memory_db.cache_file(&bytes)).unwrap());
    }

    #[test]
    fn a_duplicate_image_keeps_the_existing_cache_file_and_unlinks_the_spool_file() {
        let mut f = Fixture::new("dup");
        let bytes = pattern(50_000, 3);
        let first = f.db.insert_spooled("image/png", f.spooled("1-1.part", &bytes), 100).unwrap();
        let target = f.cache_file(&bytes);
        let ino = inode(&target);

        let second = f.db.insert_spooled("image/png", f.spooled("1-2.part", &bytes), 100).unwrap();

        assert_eq!(second, first, "deduplicated by hash");
        assert_eq!(f.db.get_total_count(), 1);
        assert_eq!(inode(&target), ino, "the existing cache file is not replaced");
        assert!(f.spool_is_empty(), "the redundant spool file is unlinked");
        assert_eq!(f.cache_files(), 1);
    }

    #[test]
    fn a_row_whose_cache_file_went_missing_is_healed_by_the_next_copy() {
        let mut f = Fixture::new("heal");
        let bytes = pattern(20_000, 4);
        let id = f.db.insert_spooled("image/png", f.spooled("1-1.part", &bytes), 100).unwrap();
        fs::remove_file(f.cache_file(&bytes)).unwrap();
        assert!(f.db.get_content_by_id(id).is_none());

        assert_eq!(f.db.insert_spooled("image/png", f.spooled("1-2.part", &bytes), 100).unwrap(), id);
        assert_eq!(f.db.get_content_by_id(id).unwrap().1, bytes);
    }

    #[test]
    fn rotation_removes_the_cache_file_of_the_image_it_evicts() {
        let mut f = Fixture::new("rotate");
        let (old, new) = (pattern(10_000, 5), pattern(10_000, 6));
        f.db.insert_spooled("image/png", f.spooled("1-1.part", &old), 1).unwrap();
        f.db.insert_spooled("image/png", f.spooled("1-2.part", &new), 1).unwrap();

        assert_eq!(f.db.get_total_count(), 1);
        assert!(!f.cache_file(&old).exists(), "the evicted image's file is gone");
        assert!(f.cache_file(&new).exists());
        assert!(f.spool_is_empty());
    }

    // --- insert_spooled: everything else ---

    #[test]
    fn a_spooled_non_image_is_stored_in_its_row_and_the_spool_file_unlinked() {
        let mut f = Fixture::new("blob");
        let bytes = pattern(400_000, 7);
        let id = f.db.insert_spooled("application/pdf", f.spooled("1-1.part", &bytes), 100).unwrap();

        assert!(matches!(f.db.locate_content(id), Some((_, ContentLocation::InlineBlob(_)))));
        assert_eq!(f.db.get_content_by_id(id).unwrap().1, bytes);
        assert!(f.spool_is_empty());
        assert_eq!(f.cache_files(), 0, "an inline payload is not also cached");
    }

    #[test]
    fn a_gif_like_text_mime_is_stored_exactly_as_the_in_memory_road_stores_it() {
        // "text/x-gif" is cache class (contains "gif") and text-like: it must not
        // take the detached-row shortcut, or its text would be lost from search.
        let mut f = Fixture::new("quirk");
        let bytes = b"GIF-ish text that is valid UTF-8".to_vec();
        let id = f.db.insert_spooled("text/x-gif", f.spooled("1-1.part", &bytes), 100).unwrap();
        let reference = Fixture::new("quirk-ref").db.insert_raw("text/x-gif", &bytes).unwrap();

        let meta = f.db.fetch_metadata(10);
        assert_eq!(meta[0].4.as_deref(), Some("GIF-ish text that is valid UTF-8"), "the preview is built from the bytes");
        assert_eq!(id, reference);
        assert!(f.spool_is_empty());
    }

    #[test]
    fn a_payload_storage_declines_leaves_nothing_behind() {
        let mut f = Fixture::new("declined");
        let bytes = pattern(9_000, 8);
        assert_eq!(f.db.insert_spooled("image/x-secret", f.spooled("1-1.part", &bytes), 100).unwrap(), -1);
        assert_eq!(f.db.insert_spooled("application/x-secret", f.spooled("1-2.part", &bytes), 100).unwrap(), -1);
        assert_eq!(f.db.insert_spooled("image/png", f.spooled("1-3.part", b""), 100).unwrap(), -1);

        assert_eq!(f.db.get_total_count(), 0);
        assert_eq!(f.cache_files(), 0, "a declined image must not be cached");
        assert!(f.spool_is_empty(), "and its spool file is unlinked");
    }

    #[test]
    fn a_spool_file_that_is_not_the_recorded_size_is_refused_not_stored() {
        let mut f = Fixture::new("size");
        let bytes = pattern(5_000, 9);
        let spooled = f.spooled("1-1.part", &bytes);
        fs::write(spooled.path(), &bytes[..4_000]).unwrap();

        let err = f.db.insert_spooled("application/pdf", spooled, 100).unwrap_err();

        assert!(err.contains("expected 5000"), "{err}");
        assert_eq!(f.db.get_total_count(), 0);
        assert!(f.spool_is_empty());
    }

    #[test]
    fn a_spool_file_that_has_vanished_is_an_error_and_not_a_cache_entry() {
        let mut f = Fixture::new("vanished");
        let spooled = f.spooled("1-1.part", &pattern(5_000, 10));
        fs::remove_file(spooled.path()).unwrap();

        assert!(f.db.insert_spooled("image/png", spooled, 100).is_err());
        assert_eq!(f.db.get_total_count(), 0);
        assert_eq!(f.cache_files(), 0);
    }

    #[test]
    fn a_row_that_fails_to_commit_takes_the_adopted_file_back_out() {
        let mut f = Fixture::new("rollback");
        // Sabotage from a second connection: the insert then fails after the file
        // has been placed.
        Connection::open(f.root.join("y4p.db")).unwrap().execute_batch("DROP TABLE clipboard;").unwrap();
        let bytes = pattern(8_000, 11);

        assert!(f.db.insert_spooled("image/png", f.spooled("1-1.part", &bytes), 100).is_err());

        assert!(!f.cache_file(&bytes).exists(), "no cache file without a row");
        assert!(f.spool_is_empty());
    }

    // --- wipe ---

    #[test]
    fn wipe_empties_rows_and_cache_and_the_store_stays_usable() {
        let mut f = Fixture::new("wipe");
        let bytes = pattern(30_000, 12);
        f.db.insert_spooled("image/png", f.spooled("1-1.part", &bytes), 100).unwrap();
        f.db.insert_raw("text/plain", b"hello").unwrap();

        f.db.wipe().unwrap();

        assert_eq!(f.db.get_total_count(), 0);
        assert_eq!(f.cache_files(), 0);
        assert!(f.db.insert_raw("text/plain", b"after the wipe").unwrap() > 0, "the connection survives shrink_memory");
    }

    // --- chunked payloads through the facade ---

    #[test]
    fn a_large_spooled_non_image_is_chunked_and_served_whole_from_the_row() {
        let mut f = Fixture::new("chunked-spool");
        let bytes = pattern(500_000, 13);
        let id = f.db.insert_spooled("application/pdf", f.spooled("1-1.part", &bytes), 100).unwrap();

        assert!(matches!(f.db.locate_content(id), Some((_, ContentLocation::InlineBlob(_)))), "still inline: the bytes are in the database");
        assert_eq!(f.db.get_content_by_id(id).unwrap(), ("application/pdf".to_owned(), bytes));
        assert_eq!(f.cache_files(), 0);
        assert!(f.spool_is_empty());
        assert!(f.db.store.chunk_usage().manifest > 1);
    }

    #[test]
    fn insert_raw_of_an_edited_text_stores_the_shared_chunks_once() {
        let mut f = Fixture::new("chunked-edit");
        let original: String = (0..60_000u32).map(|i| format!("{:x} ", i.wrapping_mul(2_654_435_761))).collect();
        let mut edited = original.clone();
        edited.insert_str(original.len() / 2, "INSERTED ");
        let a = f.db.insert_raw("text/plain", original.as_bytes()).unwrap();
        let one = f.db.store.chunk_usage();
        let b = f.db.insert_raw("text/plain", edited.as_bytes()).unwrap();
        let two = f.db.store.chunk_usage();

        assert_ne!(a, b);
        assert!(original.len() > 300_000, "the payload must span many chunks");
        assert!(two.bytes < one.bytes * 3 / 2, "{} bytes after the edit, {} before", two.bytes, one.bytes);
        assert_eq!(f.db.get_content_by_id(a).unwrap().1, original.into_bytes());
        assert_eq!(f.db.get_content_by_id(b).unwrap().1, edited.into_bytes());
    }

    #[test]
    fn chunked_rows_survive_reopening_the_database_and_are_searchable() {
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
    fn deleting_and_wiping_through_the_facade_leave_no_chunks_behind() {
        let mut f = Fixture::new("chunked-gc");
        let a = f.db.insert_raw("application/octet-stream", &pattern(300_000, 14)).unwrap();
        f.db.insert_raw("application/octet-stream", &pattern(300_000, 15)).unwrap();

        assert!(f.db.delete_by_id(a).unwrap());
        assert!(f.db.get_content_by_id(a).is_none());
        let after_delete = f.db.store.chunk_usage();
        assert_eq!(after_delete.manifest, crate::core::cdc::FastCdc::new(&pattern(300_000, 15)).count());

        f.db.wipe().unwrap();
        let after_wipe = f.db.store.chunk_usage();
        assert_eq!((after_wipe.chunks, after_wipe.manifest), (0, 0));
    }
}
