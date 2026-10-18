// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/storage/src/db.rs

use rusqlite::{params, Connection, Result};
use rusqlite::types::ValueRef;
use std::borrow::Cow;
use std::time::{SystemTime, UNIX_EPOCH};
use crate::core::constants::{SENSITIVE_MIME_HINTS, PREVIEW_CHARS};
use crate::core::utils::{is_html_mime, is_text_like_mime, strip_html_tags};
use super::MetaRow;

/// SQL predicate matching textual MIME types eligible for search indexing and
/// v3 storage migration, aligned with `core::utils::is_text_like_mime`.
///
/// Images (such as `image/svg+xml`) are explicitly excluded because their payloads
/// are offloaded to `FileCache` and stored with `content = NULL`.
pub const TEXT_MIME_SQL_PREDICATE: &str = "(mime NOT LIKE 'image/%' AND (mime LIKE '%text%' OR mime LIKE '%utf8%' OR mime LIKE '%json%' OR mime LIKE '%xml%' OR mime LIKE '%uri-list%' OR mime LIKE '%string%'))";

/// Result of `upsert_record`: `real_id == -1` means the payload was
/// deliberately skipped (empty / sensitive MIME) and no rotation ran.
pub struct UpsertOutcome {
    pub real_id: i64,
    pub is_image: bool,
    pub expired_hashes: Vec<String>,
}

impl UpsertOutcome {
    fn skipped() -> Self {
        Self { real_id: -1, is_image: false, expired_hashes: Vec::new() }
    }
}

/// A payload as the row builder sees it.
enum Body<'a> {
    /// The bytes themselves.
    Bytes(&'a [u8]),
    /// Only the length: the bytes are in a cache file and never touch the row.
    Detached(u64),
}

impl Body<'_> {
    fn len(&self) -> i64 {
        match self {
            Body::Bytes(data) => data.len() as i64,
            Body::Detached(len) => *len as i64,
        }
    }
}

/// What goes into the `content` column.
enum Content<'a> {
    Text(&'a str),
    Blob(&'a [u8]),
    None,
}

/// Whether a payload of this MIME is kept in the on-disk cache (`FileCache`)
/// rather than in the row: images, and anything GIF-like.
pub fn is_cache_class(mime: &str) -> bool {
    mime.starts_with("image/") || mime.contains("gif")
}

/// Whether the row of a cache-class payload can be built without its bytes:
/// no text preview and no text content. Every real image qualifies; an odd
/// "text/x-gif" does not, and is stored from its bytes like any other.
pub fn is_detachable(mime: &str) -> bool {
    is_cache_class(mime) && !is_text_like_mime(mime)
}

/// Payloads that are never stored: empty ones and sensitive MIME types.
pub(crate) fn is_skipped(mime: &str, len: u64) -> bool {
    len == 0 || SENSITIVE_MIME_HINTS.iter().any(|&hint| mime.contains(hint))
}

/// Pure SQL execution and transaction handling — no filesystem I/O. Binary
/// cache placement is the caller's (facade's) responsibility.
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// Insert-or-touch a record by hash, then atomically rotate out anything
    /// beyond `max_history`. See `ClipboardDb::insert_with_hash` doc for why
    /// the ID is returned directly instead of via a follow-up query.
    pub fn upsert_record(&mut self, mime: &str, data: &[u8], hash: &str, max_history: usize) -> Result<UpsertOutcome, String> {
        if is_skipped(mime, data.len() as u64) {
            return Ok(UpsertOutcome::skipped());
        }
        self.upsert(mime, Body::Bytes(data), hash, max_history)
    }

    /// `upsert_record` for a payload whose bytes live (or are about to live) in
    /// a cache file and are not needed to build the row: an image has no
    /// preview, no text and no inline content, only a size. The caller must have
    /// checked `is_detachable`, so no byte of the payload ever enters memory.
    pub fn upsert_detached(&mut self, mime: &str, len: u64, hash: &str, max_history: usize) -> Result<UpsertOutcome, String> {
        if is_skipped(mime, len) {
            return Ok(UpsertOutcome::skipped());
        }
        self.upsert(mime, Body::Detached(len), hash, max_history)
    }

    fn upsert(&mut self, mime: &str, body: Body<'_>, hash: &str, max_history: usize) -> Result<UpsertOutcome, String> {
        let is_image = is_cache_class(mime);
        let size = body.len();

        // Was `.unwrap()`: a monotonic clock read before UNIX_EPOCH should
        // never happen on a real system, but `panic = "abort"` in the release
        // profile would still turn that near-impossible case into a full
        // process abort instead of degrading gracefully.
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
            let (preview, content) = match body {
                Body::Detached(_) => (None, Content::None),
                Body::Bytes(data) => Self::row_parts(mime, data, is_image),
            };
            match content {
                Content::Text(text) => tx.execute(
                    "INSERT INTO clipboard (timestamp, mime, size, preview, content, hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![ts, mime, size, preview, text, hash],
                ),
                Content::Blob(blob) => tx.execute(
                    "INSERT INTO clipboard (timestamp, mime, size, preview, content, hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![ts, mime, size, preview, blob, hash],
                ),
                Content::None => tx.execute(
                    "INSERT INTO clipboard (timestamp, mime, size, preview, content, hash) VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                    params![ts, mime, size, preview, hash],
                ),
            }.map_err(|e| e.to_string())?;

            tx.last_insert_rowid()
        };

        // G-10: rotation only ever counts/evicts unpinned rows, so both
        // queries below filter to `is_pinned = 0` before applying the
        // OFFSET — pinned rows are never candidates for eviction AND never
        // consume the unpinned history quota.
        //
        // PERF: `LIMIT -1 OFFSET ?1` seeks directly to the (max_history+1)th
        // unpinned row in `idx_pinned_ts` and scans only what's actually
        // expiring, instead of the previous `NOT IN (SELECT ... LIMIT ?1)`
        // form — that forced SQLite to build a Bloom filter from the kept
        // set and probe it against every unpinned row (cost scales with
        // total history size, not with how much actually expired).
        let expired_hashes: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT hash FROM clipboard
                 WHERE is_pinned = 0
                 ORDER BY timestamp DESC
                 LIMIT -1 OFFSET ?1"
            ).map_err(|e| e.to_string())?;
            let rows = stmt.query_map(params![max_history as i64], |row| row.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            rows.filter_map(|r| r.ok()).collect()
        };

        // Same expiring set as above, deleted by rowid (via `id IN (...)`)
        // rather than re-testing `NOT IN` against the kept rows.
        tx.execute(
            "DELETE FROM clipboard
             WHERE id IN (
                 SELECT id FROM clipboard
                 WHERE is_pinned = 0
                 ORDER BY timestamp DESC
                 LIMIT -1 OFFSET ?1
             )",
            params![max_history as i64]
        ).map_err(|e| e.to_string())?;

        tx.commit().map_err(|e| e.to_string())?;

        Ok(UpsertOutcome { real_id, is_image, expired_hashes })
    }

    /// The preview and stored content of a row built from its bytes.
    ///
    /// "application/xhtml+xml" doesn't contain "text" itself (unlike text/html),
    /// so it needs its own check to get a preview at all rather than falling
    /// through to `None`. The same check decides the `content` storage class, so
    /// search's `content LIKE ?` (see search_metadata/validate_keywords) always
    /// lines up with what actually got stored as TEXT vs BLOB.
    fn row_parts<'a>(mime: &str, data: &'a [u8], is_image: bool) -> (Option<String>, Content<'a>) {
        let is_markup = is_html_mime(mime);
        let is_text_like = is_text_like_mime(mime);

        let preview = if is_text_like {
            // Rich markup's raw tags aren't a readable preview: strip them first
            // so the preview column always shows plain, scannable text instead of
            // leaking `<div>`/`<strong>` etc.
            let text_data: Cow<[u8]> = if is_markup {
                Cow::Owned(strip_html_tags(data))
            } else {
                Cow::Borrowed(data)
            };
            let s = String::from_utf8_lossy(&text_data);
            Some(s.chars().take(PREVIEW_CHARS).collect::<String>().replace('\n', " "))
        } else { None };

        // TEXT storage class for textual content lets `content LIKE ?` match
        // directly, without a per-row `CAST(content AS TEXT)` at query time. Bytes
        // claiming a text MIME but not actually valid UTF-8 fall through to the
        // ordinary BLOB path rather than being rejected or corrupted.
        let content = match std::str::from_utf8(data) {
            Ok(text) if is_text_like => Content::Text(text),
            _ if is_image => Content::None,
            _ => Content::Blob(data),
        };
        (preview, content)
    }

    /// See `ClipboardDb::search_metadata` doc: each hit carries its absolute
    /// MRU index, matching `fetch_metadata`'s ordering. Assumes `queries` is
    /// non-empty — the facade returns early otherwise.
    ///
    /// AND-combines one `(preview LIKE ?i OR ...)` clause per keyword; the
    /// WHERE clause is built dynamically (clause count depends on N) but
    /// every value is still bound through a placeholder, never interpolated.
    ///
    /// BUGFIX: the content branch was previously guarded by `preview IS
    /// NULL`, but every text/uri-list record gets a non-null preview (see
    /// `upsert_record`), so that branch was dead and a match past the first
    /// `PREVIEW_CHARS` characters was silently missed. Checking `content`
    /// unconditionally makes the full body actually searchable. No more
    /// `CAST(content AS TEXT)` here either — `upsert_record` now binds
    /// textual content with TEXT storage class directly, so `content` is
    /// already text-comparable for every row this query's mime filter admits.
    ///
    /// PERF: absolute index is a correlated `COUNT(*)` per candidate row
    /// instead of `ROW_NUMBER() OVER (ORDER BY timestamp DESC)`. The window
    /// function forced SQLite to materialize and number every row in the
    /// table before the mime/keyword filter could run at all; this form lets
    /// the filter (and its `idx_ts`/`idx_pinned_ts` index usage) run first,
    /// so the `COUNT(*)` only pays for rows that actually matched. Ties
    /// (identical `timestamp`, effectively never seen at this table's
    /// millisecond granularity) would collapse to the same index rather than
    /// the arbitrary-but-distinct one `ROW_NUMBER()` assigned — an accepted
    /// difference given how the index is only ever a display/lookup key, not
    /// a uniqueness guarantee.
    pub fn search_metadata(&self, queries: &[String], limit: usize) -> Vec<(usize, MetaRow)> {
        let and_clauses: Vec<String> = (1..=queries.len())
            .map(|i| format!("(preview LIKE ?{i} OR content LIKE ?{i})"))
            .collect();
        let limit_idx = queries.len() + 1;

        let sql = format!(
            "SELECT (SELECT COUNT(*) FROM clipboard c2 WHERE c2.timestamp > c1.timestamp) AS abs_idx,
                    id, timestamp, mime, size, preview, is_pinned
             FROM clipboard c1
             WHERE {TEXT_MIME_SQL_PREDICATE}
               AND {}
             ORDER BY timestamp DESC LIMIT ?{}",
            and_clauses.join(" AND "), limit_idx
        );

        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let wildcarded: Vec<String> = queries.iter().map(|q| format!("%{}%", q)).collect();
        let limit_param = limit as i64;
        let mut bindings: Vec<&dyn rusqlite::ToSql> = wildcarded.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        bindings.push(&limit_param);

        let rows = match stmt.query_map(bindings.as_slice(), |row| {
            let abs_idx: i64 = row.get(0)?;
            Ok((abs_idx as usize, (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)))
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(|r| r.ok()).collect()
    }

    /// Pre-sorts keywords into (valid, invalid) by a cheap `LIMIT 1`
    /// existence check per word, so `search_metadata`'s AND query only ever
    /// runs against words actually present. Same text/mime filter as the
    /// real search, minus the absolute-index/ordering machinery it doesn't need.
    pub fn validate_keywords(&self, keywords: &[String]) -> (Vec<String>, Vec<String>) {
        let mut valid = Vec::new();
        let mut invalid = Vec::new();

        let sql = format!(
            "SELECT 1 FROM clipboard
             WHERE {TEXT_MIME_SQL_PREDICATE}
               AND (preview LIKE ?1 OR content LIKE ?1)
             LIMIT 1"
        );
        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return (valid, keywords.to_vec()),
        };

        for kw in keywords {
            let pattern = format!("%{}%", kw);
            let exists = stmt.query_row(params![pattern], |_| Ok(())).is_ok();

            if exists { valid.push(kw.clone()); } else { invalid.push(kw.clone()); }
        }

        (valid, invalid)
    }

    pub fn fetch_metadata(&self, limit: usize) -> Vec<MetaRow> {
        let mut stmt = match self.conn.prepare(
            "SELECT id, timestamp, mime, size, preview, is_pinned FROM clipboard ORDER BY timestamp DESC LIMIT ?1"
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params![limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    /// Row's inline content (if any) plus its hash, for the facade to fall
    /// back to the file cache when `content` is `NULL`.
    pub fn get_row_content(&self, id: i64) -> Option<(String, Option<Vec<u8>>, String)> {
        self.conn.query_row(
            "SELECT mime, content, hash FROM clipboard WHERE id = ?1",
            params![id],
            |row| {
                // BUGFIX: `content` can now be TEXT storage class (v3
                // textual rows) or BLOB (binary/legacy rows) — `row.get::<_,
                // Vec<u8>>` only accepts BLOB and errors on TEXT. Reading
                // via `ValueRef::as_bytes` accepts either storage class
                // uniformly as raw bytes.
                let content = match row.get_ref(1)? {
                    ValueRef::Null => None,
                    v => Some(v.as_bytes()?.to_vec()),
                };
                Ok((row.get(0)?, content, row.get(2)?))
            }
        ).ok()
    }

    /// Same shape as `get_row_content` but without materializing the BLOB —
    /// backs `ClipboardDb::locate_content`.
    pub fn get_row_location(&self, id: i64) -> Option<(String, bool, String)> {
        self.conn.query_row(
            "SELECT mime, content IS NOT NULL, hash FROM clipboard WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        ).ok()
    }

    pub fn update_timestamp(&mut self, id: i64) -> Result<()> {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
        self.conn.execute("UPDATE clipboard SET timestamp = ?1 WHERE id = ?2", params![ts, id])?;
        Ok(())
    }

    /// Returns whether a row was deleted plus its hash (if it had one), so
    /// the facade can clean up the matching cache file regardless of which
    /// bit fired — mirrors the pre-split behavior exactly.
    pub fn delete_by_id(&mut self, id: i64) -> Result<(bool, Option<String>)> {
        let hash: Option<String> = self.conn.query_row(
            "SELECT hash FROM clipboard WHERE id = ?1",
            params![id], |row| row.get(0)
        ).ok();

        let res = self.conn.execute("DELETE FROM clipboard WHERE id = ?1", params![id])?;
        Ok((res > 0, hash))
    }

    /// Clears rows and reclaims disk space. Cache directory cleanup is the
    /// facade's job (`FileCache::clear`) — kept out of here so this module
    /// never touches the filesystem outside the SQLite file itself.
    pub fn wipe(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM clipboard", [])?;
        let _ = self.conn.execute_batch(
            "PRAGMA journal_mode = DELETE;
             VACUUM;
             PRAGMA journal_mode = WAL;",
        );
        // VACUUM frees pages, but the connection keeps them in its page cache
        // and lookaside memory; release them so the process can shrink.
        let _ = self.conn.execute_batch("PRAGMA shrink_memory;");
        Ok(())
    }

    /// G-10: flips a single record's pin flag by its immutable ID. Returns
    /// whether a row was actually affected, so the CLI can distinguish
    /// "not found" from success rather than reporting a false positive.
    pub fn set_pinned(&mut self, id: i64, is_pinned: bool) -> Result<bool, String> {
        let affected = self.conn.execute(
            "UPDATE clipboard SET is_pinned = ?1 WHERE id = ?2",
            params![is_pinned, id],
        ).map_err(|e| e.to_string())?;
        Ok(affected > 0)
    }

    pub fn get_total_count(&self) -> usize {
        self.conn.query_row(
            "SELECT COUNT(*) FROM clipboard",
            [],
            |row| row.get::<_, i64>(0).map(|val| val as usize)
        ).unwrap_or(0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Fully isolated, schema-migrated in-memory store — same `initialize`
    /// path a real on-disk DB goes through, so these tests exercise the
    /// genuine production schema rather than a hand-rolled stand-in.
    fn test_store() -> SqliteStore {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::SchemaManager::initialize(&mut conn, 1000).unwrap();
        SqliteStore::new(conn)
    }

    /// Back-doors a row's `timestamp` directly, bypassing the `SystemTime::
    /// now()` wall clock `upsert_record`/`update_timestamp` use internally.
    /// Rotation and MRU-ordering tests need rows at deterministic, strictly
    /// distinct timestamps — relying on wall-clock granularity between
    /// back-to-back calls in the same test risks ties that would make
    /// `ORDER BY timestamp DESC` non-deterministic.
    fn set_timestamp(store: &mut SqliteStore, hash: &str, ts: i64) {
        store.conn.execute("UPDATE clipboard SET timestamp = ?1 WHERE hash = ?2", params![ts, hash]).unwrap();
    }

    // --- upsert_detached: a row built from a length alone ---

    #[test]
    fn upsert_detached_builds_the_row_upsert_record_builds_for_an_image() {
        let mut detached = test_store();
        let mut from_bytes = test_store();
        let data = vec![7u8; 4321];

        let a = detached.upsert_detached("image/png", data.len() as u64, "h-img", 100).unwrap();
        let b = from_bytes.upsert_record("image/png", &data, "h-img", 100).unwrap();

        assert_eq!((a.real_id, a.is_image), (b.real_id, b.is_image));
        let row = |s: &SqliteStore| s.conn.query_row(
            "SELECT mime, size, preview, content, hash FROM clipboard", [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<Vec<u8>>>(3)?, r.get::<_, String>(4)?)),
        ).unwrap();
        assert_eq!(row(&detached), row(&from_bytes));
        assert_eq!(row(&detached).1, 4321);
    }

    #[test]
    fn upsert_detached_skips_what_upsert_record_skips() {
        let mut store = test_store();
        assert_eq!(store.upsert_detached("image/png", 0, "h-empty", 100).unwrap().real_id, -1);
        assert_eq!(store.upsert_detached("image/x-secret", 10, "h-secret", 100).unwrap().real_id, -1);
        assert_eq!(store.get_total_count(), 0);
    }

    #[test]
    fn upsert_detached_touches_an_existing_row_and_rotates_like_any_insert() {
        let mut store = test_store();
        let first = store.upsert_detached("image/png", 10, "h1", 1).unwrap();
        let again = store.upsert_detached("image/png", 10, "h1", 1).unwrap();
        assert_eq!(first.real_id, again.real_id, "same hash, same row");

        set_timestamp(&mut store, "h1", 1);
        let second = store.upsert_detached("image/gif", 20, "h2", 1).unwrap();
        assert_ne!(second.real_id, first.real_id);
        assert_eq!(second.expired_hashes, vec!["h1".to_owned()], "the older image rotates out");
        assert_eq!(store.get_total_count(), 1);
    }

    #[test]
    fn only_images_without_text_are_detachable() {
        for mime in ["image/png", "image/gif", "image/svg+xml", "application/x-gif"] {
            assert!(is_detachable(mime), "{mime}");
        }
        for mime in ["text/x-gif", "text/plain", "application/pdf", ""] {
            assert!(!is_detachable(mime), "{mime}");
        }
        assert!(is_cache_class("text/x-gif"), "still cache class: its bytes are cached too");
    }

    #[test]
    fn wipe_releases_the_connections_memory_without_breaking_it() {
        let mut store = test_store();
        store.upsert_record("text/plain", &vec![b'x'; 200_000], "h-big", 100).unwrap();
        store.wipe().unwrap();
        assert!(store.conn.execute_batch("PRAGMA shrink_memory;").is_ok(), "the pragma is supported by the bundled SQLite");
        assert_eq!(store.get_total_count(), 0);
        assert!(store.upsert_record("text/plain", b"again", "h-again", 100).unwrap().real_id > 0);
    }

    // --- upsert_record: text vs. image storage ---

    #[test]
    fn upsert_record_text_payload_is_stored_as_text_and_readable() {
        let mut store = test_store();
        let outcome = store.upsert_record("text/plain", b"hello world", "h-text", 100).unwrap();

        assert!(!outcome.is_image);
        assert_ne!(outcome.real_id, -1);

        let (mime, content, hash) = store.get_row_content(outcome.real_id).unwrap();
        assert_eq!(mime, "text/plain");
        assert_eq!(content, Some(b"hello world".to_vec()));
        assert_eq!(hash, "h-text");
    }

    #[test]
    fn upsert_record_image_payload_leaves_sqlite_content_null_for_file_cache_delegation() {
        let mut store = test_store();
        let outcome = store.upsert_record("image/png", &[0x89, 0x50, 0x4E, 0x47, 1, 2, 3, 4], "h-img", 100).unwrap();

        assert!(outcome.is_image);
        let (mime, has_content, hash) = store.get_row_location(outcome.real_id).unwrap();
        assert_eq!(mime, "image/png");
        assert!(!has_content, "image content must be NULL in SQLite — binary lives in the FileCache instead");
        assert_eq!(hash, "h-img");
    }

    #[test]
    fn upsert_record_text_mime_with_invalid_utf8_falls_back_to_blob_without_corruption() {
        let mut store = test_store();
        let raw = vec![0xFF, 0xFE, 0xFD, 0x00, 0x01];
        let outcome = store.upsert_record("text/plain", &raw, "h-badutf8", 100).unwrap();

        let (_, content, _) = store.get_row_content(outcome.real_id).unwrap();
        assert_eq!(content, Some(raw), "non-UTF8 bytes must survive the BLOB fallback byte-for-byte");
    }

    #[test]
    fn upsert_record_empty_payload_is_skipped_without_inserting() {
        let mut store = test_store();
        let outcome = store.upsert_record("text/plain", b"", "h-empty", 100).unwrap();

        assert_eq!(outcome.real_id, -1);
        assert_eq!(store.get_total_count(), 0);
    }

    #[test]
    fn upsert_record_sensitive_mime_is_skipped_without_inserting() {
        let mut store = test_store();
        let outcome = store.upsert_record("x-kde-passwordManagerHint", b"secret value", "h-sensitive", 100).unwrap();

        assert_eq!(outcome.real_id, -1);
        assert_eq!(store.get_total_count(), 0);
    }

    #[test]
    fn upsert_record_duplicate_hash_updates_existing_row_instead_of_duplicating() {
        let mut store = test_store();
        let first = store.upsert_record("text/plain", b"same content", "h-dup", 100).unwrap();
        let second = store.upsert_record("text/plain", b"same content", "h-dup", 100).unwrap();

        assert_eq!(first.real_id, second.real_id);
        assert_eq!(store.get_total_count(), 1);
    }

    // --- upsert_record: rotation and pin protection (G-10) ---

    #[test]
    fn upsert_record_rotation_evicts_oldest_unpinned_row_when_over_capacity() {
        let mut store = test_store();
        let id1 = store.upsert_record("text/plain", b"one", "h1", 100).unwrap().real_id;
        set_timestamp(&mut store, "h1", 1000);
        let id2 = store.upsert_record("text/plain", b"two", "h2", 100).unwrap().real_id;
        set_timestamp(&mut store, "h2", 2000);

        // Third insert pushes the unpinned count to 3 against a max_history
        // of 2 — exactly one row, the oldest (h1), must expire.
        let outcome = store.upsert_record("text/plain", b"three", "h3", 2).unwrap();

        assert_eq!(outcome.expired_hashes, vec!["h1".to_string()]);
        assert_eq!(store.get_total_count(), 2);
        assert!(store.get_row_content(id1).is_none());
        assert!(store.get_row_content(id2).is_some());
    }

    #[test]
    fn upsert_record_rotation_protects_pinned_rows_regardless_of_age() {
        let mut store = test_store();
        let id1 = store.upsert_record("text/plain", b"one", "h1", 100).unwrap().real_id;
        set_timestamp(&mut store, "h1", 1000);
        let id2 = store.upsert_record("text/plain", b"two", "h2", 100).unwrap().real_id;
        set_timestamp(&mut store, "h2", 2000);
        let id3 = store.upsert_record("text/plain", b"three", "h3", 100).unwrap().real_id;
        set_timestamp(&mut store, "h3", 3000);

        // Pin the objectively oldest row — it must survive purely because
        // it's pinned, never merely because it happens to be recent.
        assert!(store.set_pinned(id1, true).unwrap());

        // max_history = 2: only 2 *unpinned* rows may survive. Before this
        // call there are 2 unpinned rows (h2, h3); adding a third newer
        // unpinned row means exactly one unpinned row — the oldest
        // unpinned one, h2 — must expire. The pinned h1, despite being
        // older than all of them, must not appear in `expired_hashes`.
        let outcome = store.upsert_record("text/plain", b"four", "h4", 2).unwrap();

        assert_eq!(outcome.expired_hashes, vec!["h2".to_string()]);
        assert_eq!(store.get_total_count(), 3);
        assert!(store.get_row_content(id1).is_some(), "pinned row must survive despite being the oldest");
        assert!(store.get_row_content(id2).is_none(), "oldest *unpinned* row must be evicted");
        assert!(store.get_row_content(id3).is_some());
    }

    #[test]
    fn upsert_record_rotation_is_a_noop_when_within_capacity() {
        let mut store = test_store();
        store.upsert_record("text/plain", b"one", "h1", 100).unwrap();
        let outcome = store.upsert_record("text/plain", b"two", "h2", 100).unwrap();

        assert!(outcome.expired_hashes.is_empty());
        assert_eq!(store.get_total_count(), 2);
    }

    // --- search_metadata / validate_keywords ---

    #[test]
    fn search_metadata_matches_preview_or_content_ordered_newest_first_with_absolute_index() {
        let mut store = test_store();
        let id1 = store.upsert_record("text/plain", b"alpha banana", "h1", 100).unwrap().real_id;
        set_timestamp(&mut store, "h1", 1000);
        store.upsert_record("text/plain", b"completely unrelated", "h2", 100).unwrap();
        set_timestamp(&mut store, "h2", 2000);
        let id3 = store.upsert_record("text/plain", b"banana split", "h3", 100).unwrap().real_id;
        set_timestamp(&mut store, "h3", 3000);

        let results = store.search_metadata(&["banana".to_string()], 10);
        let ids: Vec<i64> = results.iter().map(|(_, row)| row.0).collect();
        // Newest match first; the unrelated h2 row must not appear at all.
        assert_eq!(ids, vec![id3, id1]);

        let abs_indices: Vec<usize> = results.iter().map(|(idx, _)| *idx).collect();
        // id3 is the newest row in the whole table (0 rows are newer).
        // id1 has two strictly newer rows overall (id2 and id3).
        assert_eq!(abs_indices, vec![0, 2]);
    }

    #[test]
    fn search_metadata_empty_result_for_unmatched_keyword() {
        let mut store = test_store();
        store.upsert_record("text/plain", b"hello world", "h1", 100).unwrap();

        assert!(store.search_metadata(&["nonexistent".to_string()], 10).is_empty());
    }

    #[test]
    fn validate_keywords_splits_present_and_absent_terms() {
        let mut store = test_store();
        store.upsert_record("text/plain", b"hello world", "h1", 100).unwrap();

        let (valid, invalid) = store.validate_keywords(&["hello".to_string(), "nonexistent".to_string()]);
        assert_eq!(valid, vec!["hello".to_string()]);
        assert_eq!(invalid, vec!["nonexistent".to_string()]);
    }

    #[test]
    fn search_metadata_finds_json_payload() {
        let mut store = test_store();
        store.upsert_record("application/json", br#"{"needle": "found"}"#, "h-json", 100).unwrap();

        let results = store.search_metadata(&["needle".to_string()], 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.2, "application/json");
    }

    #[test]
    fn search_metadata_finds_xhtml_payload() {
        let mut store = test_store();
        store.upsert_record(
            "application/xhtml+xml",
            b"<html><body>findable xhtml content</body></html>",
            "h-xhtml",
            100,
        ).unwrap();

        let results = store.search_metadata(&["findable".to_string()], 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.2, "application/xhtml+xml");
    }

    #[test]
    fn search_metadata_finds_xml_payload() {
        let mut store = test_store();
        store.upsert_record("application/xml", b"<note><to>findable_recipient</to></note>", "h-xml", 100).unwrap();

        let results = store.search_metadata(&["findable_recipient".to_string()], 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.2, "application/xml");
    }

    #[test]
    fn search_metadata_finds_uri_list_payload() {
        let mime = crate::core::constants::MIME_URI_LIST;
        let mut store = test_store();
        store.upsert_record(mime, b"/home/user/findable-report.pdf", "h-uri", 100).unwrap();

        let results = store.search_metadata(&["findable-report".to_string()], 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.2, mime);
    }

    #[test]
    fn search_metadata_finds_utf8_string_payload() {
        let mut store = test_store();
        store.upsert_record("UTF8_STRING", b"findable utf8 selection", "h-utf8", 100).unwrap();

        let results = store.search_metadata(&["findable".to_string()], 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.2, "UTF8_STRING");
    }

    #[test]
    fn search_metadata_finds_mime_with_charset_parameter() {
        let mut store = test_store();
        store.upsert_record("text/plain;charset=utf-8", b"findable charset payload", "h-charset", 100).unwrap();

        let results = store.search_metadata(&["findable".to_string()], 10);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_metadata_does_not_match_image_payload_even_with_matching_bytes() {
        let mut store = test_store();
        store.upsert_record("image/png", b"needle-like-bytes-in-an-image", "h-img", 100).unwrap();
        store.upsert_record("image/svg+xml", b"<svg>needle-like-bytes-in-svg</svg>", "h-svg", 100).unwrap();

        assert!(store.search_metadata(&["needle".to_string()], 10).is_empty());
    }

    #[test]
    fn search_metadata_does_not_match_generic_binary_payload() {
        let mut store = test_store();
        store.upsert_record("application/octet-stream", b"needle-in-binary", "h-bin", 100).unwrap();

        assert!(store.search_metadata(&["needle".to_string()], 10).is_empty());
    }

    #[test]
    fn search_metadata_mixed_mimes_only_text_like_ones_are_in_scope() {
        let mut store = test_store();
        store.upsert_record("application/json", br#"{"k": "shared-term"}"#, "h-json", 100).unwrap();
        store.upsert_record("image/jpeg", b"shared-term as raw jpeg bytes", "h-jpeg", 100).unwrap();
        store.upsert_record("application/octet-stream", b"shared-term as raw binary", "h-bin", 100).unwrap();
        store.upsert_record("text/plain", b"shared-term as plain text", "h-txt", 100).unwrap();

        let results = store.search_metadata(&["shared-term".to_string()], 10);
        let mimes: std::collections::HashSet<String> = results.iter().map(|(_, row)| row.2.clone()).collect();
        assert_eq!(mimes, std::collections::HashSet::from(["application/json".to_string(), "text/plain".to_string()]));
    }

    #[test]
    fn validate_keywords_recognises_json_and_xhtml_terms() {
        let mut store = test_store();
        store.upsert_record("application/json", br#"{"term": "jsonterm"}"#, "h-json", 100).unwrap();
        store.upsert_record("application/xhtml+xml", b"<p>xhtmlterm</p>", "h-xhtml", 100).unwrap();

        let (valid, invalid) = store.validate_keywords(&["jsonterm".to_string(), "xhtmlterm".to_string()]);
        assert_eq!(valid, vec!["jsonterm".to_string(), "xhtmlterm".to_string()]);
        assert!(invalid.is_empty());
    }

    #[test]
    fn validate_keywords_rejects_term_only_present_in_an_image_payload() {
        let mut store = test_store();
        store.upsert_record("image/png", b"only-in-image-bytes", "h-img", 100).unwrap();

        let (valid, invalid) = store.validate_keywords(&["only-in-image-bytes".to_string()]);
        assert!(valid.is_empty());
        assert_eq!(invalid, vec!["only-in-image-bytes".to_string()]);
    }

    // --- delete_by_id / wipe ---

    #[test]
    fn delete_by_id_removes_the_row_and_returns_its_hash() {
        let mut store = test_store();
        let id = store.upsert_record("text/plain", b"to be deleted", "h-del", 100).unwrap().real_id;

        let (deleted, hash) = store.delete_by_id(id).unwrap();
        assert!(deleted);
        assert_eq!(hash, Some("h-del".to_string()));
        assert_eq!(store.get_total_count(), 0);
    }

    #[test]
    fn delete_by_id_missing_id_returns_false_without_touching_other_rows() {
        let mut store = test_store();
        store.upsert_record("text/plain", b"untouched", "h-keep", 100).unwrap();

        let (deleted, hash) = store.delete_by_id(999_999).unwrap();
        assert!(!deleted);
        assert_eq!(hash, None);
        assert_eq!(store.get_total_count(), 1);
    }

    #[test]
    fn wipe_clears_all_rows_including_pinned_ones() {
        let mut store = test_store();
        let id = store.upsert_record("text/plain", b"keep?", "h-wipe", 100).unwrap().real_id;
        store.set_pinned(id, true).unwrap();

        store.wipe().unwrap();
        assert_eq!(store.get_total_count(), 0);
    }

    // --- set_pinned ---

    #[test]
    fn set_pinned_on_missing_id_returns_false() {
        let mut store = test_store();
        assert!(!store.set_pinned(999_999, true).unwrap());
    }
}
