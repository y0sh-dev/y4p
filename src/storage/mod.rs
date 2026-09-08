// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/storage/mod.rs

mod cache;
mod db;
mod schema;

use rusqlite::{Connection, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use sha3::{Digest, Sha3_256};

use cache::FileCache;
use db::SqliteStore;
use schema::SchemaManager;

use crate::core::constants::SQLITE_TIMEOUT_MS;

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
    /// Fixed by computing each row's absolute MRU index (`ROW_NUMBER() OVER
    /// (ORDER BY timestamp DESC) - 1`, matching exactly how `list.rs`
    /// derives it) over the *whole* table before filtering, so a search
    /// result's displayed index is always consistent with `list`'s.
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

    pub fn get_latest_data(&self) -> Option<Vec<u8>> {
        let id = self.store.latest_id()?;
        self.get_content_by_id(id).map(|(_, data)| data)
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
    pub fn wipe(&mut self) -> Result<()> {
        self.store.wipe()?;
        let _ = self.cache.clear();
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
