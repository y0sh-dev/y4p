// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/storage/mod.rs

use rusqlite::{params, Connection, Result};
use std::time::{SystemTime, UNIX_EPOCH};
use std::fs;
use std::path::PathBuf;
use std::os::unix::fs::PermissionsExt;
use sha3::{Sha3_256, Digest}; 
use crate::core::constants::*;

/// A single metadata row, shared by `fetch_metadata` and `search_metadata` so
/// callers (and future callers) don't have to keep re-deriving the same
/// 5-tuple shape by hand.
pub type MetaRow = (i64, i64, String, i64, Option<String>);

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

pub struct ClipboardDb {
    conn: Connection,
}

impl ClipboardDb {
    /// Open the database with optimized configurations. 
    /// Returns Result to allow the caller to handle connection failures gracefully.
    pub fn open() -> Result<Self, String> {
        let db_path = crate::core::get_db_path();
        let _ = crate::core::get_cache_dir(); 

        // Attempt connection with error mapping
        let conn = Connection::open(&db_path)
            .map_err(|e| format!("sqlite connection failed: {}", e))?;

        // Secure file permissions
        if let Ok(metadata) = fs::metadata(&db_path) {
            let mut perms = metadata.permissions();
            if perms.mode() != 0o600 {
                perms.set_mode(0o600);
                let _ = fs::set_permissions(&db_path, perms);
            }
        }

        // Apply PRAGMA settings
        conn.busy_timeout(std::time::Duration::from_millis(SQLITE_TIMEOUT_MS)).ok();
        let _ = conn.execute_batch("
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA temp_store = MEMORY;
            PRAGMA mmap_size = 268435456;
            PRAGMA cache_size = -64000;
        ");

        // Schema initialization
        conn.execute(
            "CREATE TABLE IF NOT EXISTS clipboard (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                mime TEXT NOT NULL,
                size INTEGER NOT NULL,
                preview TEXT,
                content BLOB,
                hash TEXT UNIQUE
            )", [],
        ).map_err(|e| format!("schema initialization failed: {}", e))?;

        conn.execute("CREATE INDEX IF NOT EXISTS idx_ts ON clipboard(timestamp)", []).ok();

        Ok(Self { conn })
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
        if data.is_empty() { return Ok(-1); }
        if SENSITIVE_MIME_HINTS.iter().any(|&hint| mime.contains(hint)) { return Ok(-1); }

        let is_image = mime.starts_with("image/") || mime.contains("gif");
        
        if is_image {
            let mut cache_path = crate::core::get_cache_dir();
            cache_path.push(format!("{}.cache", hash));
            if !cache_path.exists() {
                let _ = fs::write(cache_path, data);
            }
        }

        // Was `.unwrap()`: a monotonic clock read before UNIX_EPOCH should
        // never happen on a real system, but with `panic = "abort"` in the
        // release profile, even that near-impossible case would abort the
        // *entire* daemon process instead of degrading gracefully. `0`
        // (UNIX_EPOCH) is a safe, inert fallback for a timestamp field used
        // only for MRU ordering/display.
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;

        let existing: Option<i64> = tx.query_row(
            "SELECT id FROM clipboard WHERE hash = ?1 LIMIT 1",
            params![hash], |row| row.get(0)
        ).ok();

        let real_id = if let Some(id) = existing {
            tx.execute("UPDATE clipboard SET timestamp = ?1 WHERE id = ?2", params![ts, id])
                .map_err(|e| e.to_string())?;
            id
        } else {
            let preview = if mime.contains("text") || mime.contains("uri-list") {
                let s = String::from_utf8_lossy(data);
                Some(s.chars().take(PREVIEW_CHARS).collect::<String>().replace('\n', " "))
            } else { None };

            let db_content = if is_image { None } else { Some(data) };

            tx.execute(
                "INSERT INTO clipboard (timestamp, mime, size, preview, content, hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![ts, mime, data.len() as i64, preview, db_content, hash],
            ).map_err(|e| e.to_string())?;

            tx.last_insert_rowid()
        };

        let expired_hashes: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT hash FROM clipboard WHERE id NOT IN (SELECT id FROM clipboard ORDER BY timestamp DESC LIMIT ?1)"
            ).map_err(|e| e.to_string())?;
            let rows = stmt.query_map(params![max_history as i64], |row| row.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            rows.filter_map(|r| r.ok()).collect()
        };

        tx.execute(
            "DELETE FROM clipboard WHERE id NOT IN (SELECT id FROM clipboard ORDER BY timestamp DESC LIMIT ?1)",
            params![max_history as i64]
        ).map_err(|e| e.to_string())?;

        tx.commit().map_err(|e| e.to_string())?;

        for h in expired_hashes {
            let mut cache_path = crate::core::get_cache_dir();
            cache_path.push(format!("{}.cache", h));
            let _ = fs::remove_file(cache_path);
        }

        Ok(real_id)
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
    pub fn search_metadata(&self, query: &str, limit: usize) -> Vec<(usize, MetaRow)> {
        let mut stmt = match self.conn.prepare(
            "SELECT abs_idx, id, timestamp, mime, size, preview FROM (
                SELECT id, timestamp, mime, size, preview, content,
                       ROW_NUMBER() OVER (ORDER BY timestamp DESC) - 1 AS abs_idx
                FROM clipboard
             ) WHERE (mime LIKE '%text%' OR mime LIKE '%UTF8%')
               AND (preview LIKE ?1 OR (preview IS NULL AND CAST(content AS TEXT) LIKE ?1))
             ORDER BY timestamp DESC LIMIT ?2"
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let query_param = format!("%{}%", query);
        let rows = match stmt.query_map(params![query_param, limit as i64], |row| {
            let abs_idx: i64 = row.get(0)?;
            Ok((abs_idx as usize, (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(|r| r.ok()).collect()
    }

    pub fn fetch_metadata(&self, limit: usize) -> Vec<MetaRow> {
        // Was `.unwrap()` in two places here: a transient `prepare`/
        // `query_map` failure (e.g. a locked or momentarily-busy database)
        // would previously abort the whole process under `panic = "abort"`.
        // `fetch_metadata` is read-only display data — degrading to an
        // empty result on failure is always safe and keeps the caller (and,
        // for future callers, the daemon) alive.
        let mut stmt = match self.conn.prepare(
            "SELECT id, timestamp, mime, size, preview FROM clipboard ORDER BY timestamp DESC LIMIT ?1"
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params![limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    pub fn get_content_by_id(&self, id: i64) -> Option<(String, Vec<u8>)> {
        let (mime, db_content, hash): (String, Option<Vec<u8>>, String) = self.conn.query_row(
            "SELECT mime, content, hash FROM clipboard WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        ).ok()?;

        if let Some(data) = db_content {
            Some((mime, data))
        } else {
            let mut cache_path = crate::core::get_cache_dir();
            cache_path.push(format!("{}.cache", hash));
            let data = fs::read(cache_path).ok()?;
            Some((mime, data))
        }
    }

    /// See `ContentLocation` docs: a non-materializing counterpart to
    /// `get_content_by_id`. Used by the kernel-level egress path
    /// (`daemon::handle_restore_request` / `wayland::handlers::data_control::source`)
    /// to decide whether a `copy-to` can be served via `sendfile(2)`
    /// directly from the cache file, or must go through the existing
    /// in-memory path (small/inline payloads).
    pub fn locate_content(&self, id: i64) -> Option<(String, ContentLocation)> {
        let (mime, has_content, hash): (String, bool, String) = self.conn.query_row(
            "SELECT mime, content IS NOT NULL, hash FROM clipboard WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        ).ok()?;

        if has_content {
            Some((mime, ContentLocation::InlineBlob(id)))
        } else {
            let mut cache_path = crate::core::get_cache_dir();
            cache_path.push(format!("{}.cache", hash));
            Some((mime, ContentLocation::CacheFile(cache_path)))
        }
    }

    pub fn get_latest_data(&self) -> Option<Vec<u8>> {
        // Reuse get_content_by_id logic for consistency
        let id: i64 = self.conn.query_row(
            "SELECT id FROM clipboard ORDER BY timestamp DESC LIMIT 1",
            [], |row| row.get(0)
        ).ok()?;
        self.get_content_by_id(id).map(|(_, data)| data)
    }

    /// Update record timestamp. Standardized to &mut self for state consistency.
    pub fn update_timestamp(&mut self, id: i64) -> Result<()> {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
        self.conn.execute("UPDATE clipboard SET timestamp = ?1 WHERE id = ?2", params![ts, id])?;
        Ok(())
    }

    /// Remove record by ID. Standardized to &mut self for state consistency.
    pub fn delete_by_id(&mut self, id: i64) -> Result<bool> {
        let hash: Option<String> = self.conn.query_row(
            "SELECT hash FROM clipboard WHERE id = ?1",
            params![id], |row| row.get(0)
        ).ok();

        let res = self.conn.execute("DELETE FROM clipboard WHERE id = ?1", params![id])?;
        
        if let Some(h) = hash {
            let mut cache_path = crate::core::get_cache_dir();
            cache_path.push(format!("{}.cache", h));
            let _ = fs::remove_file(cache_path);
        }

        Ok(res > 0)
    }

    /// Clear all history and reclaim disk space. Standardized to &mut self.
    pub fn wipe(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM clipboard", [])?;

        let cache_dir = crate::core::get_cache_dir();
        let _ = fs::remove_dir_all(&cache_dir);
        let _ = fs::create_dir_all(&cache_dir);

        let _ = self.conn.execute_batch("
            PRAGMA journal_mode = DELETE;
            VACUUM;
            PRAGMA journal_mode = WAL;
        ");

        Ok(())
    }

    /// Safely retrieve total record count. Removed unwrap() to prevent daemon panics.
    pub fn get_total_count(&self) -> usize {
        self.conn.query_row(
            "SELECT COUNT(*) FROM clipboard",
            [],
            |row| row.get::<_, i64>(0).map(|val| val as usize)
        ).unwrap_or(0)
    }
}
