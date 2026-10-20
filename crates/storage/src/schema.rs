// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/storage/src/schema.rs

use rusqlite::Connection;

/// Bump this and add a `migrate_to_vN` below whenever the schema changes.
const SCHEMA_VERSION: i64 = 4;

/// Schema initialization and versioned migrations, gated by `PRAGMA user_version`
/// so an existing on-disk DB only ever runs the migrations it's missing.
pub struct SchemaManager;

impl SchemaManager {
    pub fn initialize(conn: &mut Connection, timeout_ms: u64) -> Result<(), String> {
        conn.busy_timeout(std::time::Duration::from_millis(timeout_ms)).ok();
        let _ = conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             PRAGMA mmap_size = 268435456;
             PRAGMA cache_size = -64000;",
        );

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;

        // Pre-migration DBs have the table already but version 0 — migrate_to_v1
        // uses IF NOT EXISTS, so re-running it against them is a no-op.
        if version < 1 {
            Self::migrate_to_v1(conn)?;
        }
        if version < 2 {
            Self::migrate_to_v2(conn)?;
        }
        if version < 3 {
            Self::migrate_to_v3(conn)?;
        }
        if version < 4 {
            Self::migrate_to_v4(conn)?;
        }

        if version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|e| e.to_string())?;
        }

        // Deliberately outside the `version` gate, unlike the migrations
        // above: a DB that already reached version 2 before idx_pinned_ts
        // existed would otherwise keep the stale single-column idx_pinned
        // forever, since `migrate_to_v2` never runs again for it. Both
        // statements are cheap no-ops once the index is already correct, so
        // running them unconditionally on every startup costs nothing and
        // self-heals regardless of migration history.
        conn.execute("DROP INDEX IF EXISTS idx_pinned", []).ok();
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_pinned_ts ON clipboard(is_pinned, timestamp DESC)",
            [],
        ).ok();

        Ok(())
    }

    fn migrate_to_v1(conn: &mut Connection) -> Result<(), String> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS clipboard (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                mime TEXT NOT NULL,
                size INTEGER NOT NULL,
                preview TEXT,
                content BLOB,
                hash TEXT UNIQUE
            )",
            [],
        )
        .map_err(|e| format!("schema initialization failed: {}", e))?;

        conn.execute("CREATE INDEX IF NOT EXISTS idx_ts ON clipboard(timestamp)", []).ok();
        Ok(())
    }

    /// G-10: adds the pin/favorite flag. `ADD COLUMN ... DEFAULT 0` backfills
    /// every existing row as unpinned in place — no data is rewritten or lost.
    fn migrate_to_v2(conn: &mut Connection) -> Result<(), String> {
        let has_column: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('clipboard') WHERE name = 'is_pinned'")
            .and_then(|mut stmt| stmt.exists([]))
            .unwrap_or(false);

        if !has_column {
            conn.execute("ALTER TABLE clipboard ADD COLUMN is_pinned INTEGER NOT NULL DEFAULT 0", [])
                .map_err(|e| format!("v2 migration failed: {}", e))?;
        }

        // Index creation for is_pinned lives in `initialize` (unconditional,
        // outside the version gate) rather than here — see its comment.
        Ok(())
    }

    /// `content` for textual MIMEs moves from BLOB to TEXT storage class
    /// (see `db::SqliteStore::upsert_record`), so search can `content LIKE
    /// ?` directly instead of paying a `CAST(content AS TEXT)` per row.
    /// Backfills existing rows in place: `CAST(x AS TEXT)` on an
    /// already-BLOB value reproduces the identical bytes, just relabeled
    /// under TEXT storage class — no data is rewritten or lost. Same mime
    /// predicate `upsert_record` uses to decide TEXT vs BLOB at insert time.
    fn migrate_to_v3(conn: &mut Connection) -> Result<(), String> {
        conn.execute(
            &format!(
                "UPDATE clipboard
                 SET content = CAST(content AS TEXT)
                 WHERE content IS NOT NULL
                   AND {}",
                crate::db::TEXT_MIME_SQL_PREDICATE
            ),
            [],
        )
        .map_err(|e| format!("v3 migration failed: {}", e))?;
        Ok(())
    }

    /// Content-defined chunk storage (see `db::SqliteStore::store_chunks`).
    ///
    /// Purely additive, so every v1-v3 row stays exactly as it is:
    /// - `clipboard.is_chunked` (default 0) marks a row whose bytes live in
    ///   `chunks` instead of `content`; all existing rows backfill as inline.
    /// - `chunks` holds each distinct chunk once, keyed by its SHA3-256 digest,
    ///   with the number of manifest entries that reference it.
    /// - `manifest` maps a row to its chunks in order; a chunk may appear more
    ///   than once in one payload, hence `seq` rather than the digest in the key.
    ///
    /// No foreign keys: SQLite enforces them per connection and only when asked,
    /// so reference counting is kept in the same transaction as the row change
    /// instead (`db::SqliteStore::release_chunks`).
    fn migrate_to_v4(conn: &mut Connection) -> Result<(), String> {
        let has_column: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('clipboard') WHERE name = 'is_chunked'")
            .and_then(|mut stmt| stmt.exists([]))
            .unwrap_or(false);

        let tx = conn.transaction().map_err(|e| format!("v4 migration failed: {}", e))?;
        if !has_column {
            tx.execute("ALTER TABLE clipboard ADD COLUMN is_chunked INTEGER NOT NULL DEFAULT 0", [])
                .map_err(|e| format!("v4 migration failed: {}", e))?;
        }
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS chunks (
                hash BLOB PRIMARY KEY,
                data BLOB NOT NULL,
                refcount INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS manifest (
                clipboard_id INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                chunk_hash BLOB NOT NULL,
                PRIMARY KEY (clipboard_id, seq)
            ) WITHOUT ROWID;",
        )
        .map_err(|e| format!("v4 migration failed: {}", e))?;
        tx.commit().map_err(|e| format!("v4 migration failed: {}", e))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn index_exists(conn: &Connection, name: &str) -> bool {
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1")
            .and_then(|mut stmt| stmt.exists([name]))
            .unwrap_or(false)
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
        conn.prepare(&format!("SELECT 1 FROM pragma_table_info('{}') WHERE name = ?1", table))
            .and_then(|mut stmt| stmt.exists([column]))
            .unwrap_or(false)
    }

    // --- Fresh initialisation ---

    #[test]
    fn initialize_on_fresh_db_builds_current_schema_with_is_pinned_column() {
        let mut conn = Connection::open_in_memory().unwrap();
        SchemaManager::initialize(&mut conn, 1000).unwrap();

        assert!(column_exists(&conn, "clipboard", "is_pinned"));

        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn initialize_on_fresh_db_creates_idx_pinned_ts() {
        let mut conn = Connection::open_in_memory().unwrap();
        SchemaManager::initialize(&mut conn, 1000).unwrap();

        assert!(index_exists(&conn, "idx_pinned_ts"));
    }

    #[test]
    fn initialize_is_idempotent_when_rerun_on_an_already_migrated_db() {
        let mut conn = Connection::open_in_memory().unwrap();
        SchemaManager::initialize(&mut conn, 1000).unwrap();
        // A second run against an already-fully-migrated DB (the real
        // startup path for every run after the first) must not error or
        // regress the schema/version.
        SchemaManager::initialize(&mut conn, 1000).unwrap();

        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(column_exists(&conn, "clipboard", "is_pinned"));
    }

    // --- Legacy migration ---

    #[test]
    fn migration_from_legacy_v1_db_adds_is_pinned_and_preserves_existing_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        // A pre-G-10 on-disk DB: the original v1 table shape, no
        // `is_pinned` column, and `user_version` still at SQLite's own
        // default of 0 (this migration machinery is what introduces
        // `user_version` tracking in the first place).
        conn.execute_batch(
            "CREATE TABLE clipboard (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                mime TEXT NOT NULL,
                size INTEGER NOT NULL,
                preview TEXT,
                content BLOB,
                hash TEXT UNIQUE
            );
            INSERT INTO clipboard (timestamp, mime, size, preview, content, hash)
            VALUES (1000, 'text/plain', 5, 'hello', 'hello', 'legacy-hash');",
        )
        .unwrap();

        SchemaManager::initialize(&mut conn, 1000).unwrap();

        assert!(column_exists(&conn, "clipboard", "is_pinned"));

        let (mime, content, is_pinned): (String, String, i64) = conn
            .query_row(
                "SELECT mime, content, is_pinned FROM clipboard WHERE hash = 'legacy-hash'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(mime, "text/plain");
        assert_eq!(content, "hello");
        assert_eq!(is_pinned, 0, "pre-existing rows must backfill as unpinned");

        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn initialize_self_heals_legacy_idx_pinned_into_idx_pinned_ts() {
        let mut conn = Connection::open_in_memory().unwrap();
        // A DB that already reached v2 before `idx_pinned_ts` existed:
        // `migrate_to_v2` already ran (so the version gate skips it again),
        // but the index it's paired with is still the old single-column one.
        conn.execute_batch(
            "CREATE TABLE clipboard (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                mime TEXT NOT NULL,
                size INTEGER NOT NULL,
                preview TEXT,
                content BLOB,
                hash TEXT UNIQUE,
                is_pinned INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX idx_pinned ON clipboard(is_pinned);
            PRAGMA user_version = 2;",
        )
        .unwrap();

        SchemaManager::initialize(&mut conn, 1000).unwrap();

        assert!(!index_exists(&conn, "idx_pinned"), "stale single-column index must be dropped");
        assert!(index_exists(&conn, "idx_pinned_ts"));
    }

    // --- v4: chunk storage ---

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")
            .and_then(|mut stmt| stmt.exists([name]))
            .unwrap_or(false)
    }

    #[test]
    fn initialize_on_fresh_db_creates_the_chunk_tables_and_flag() {
        let mut conn = Connection::open_in_memory().unwrap();
        SchemaManager::initialize(&mut conn, 1000).unwrap();

        assert!(table_exists(&conn, "chunks"));
        assert!(table_exists(&conn, "manifest"));
        assert!(column_exists(&conn, "clipboard", "is_chunked"));
        assert_eq!(SCHEMA_VERSION, 4);
    }

    #[test]
    fn migration_from_v3_keeps_every_row_byte_for_byte_and_marks_it_inline() {
        let mut conn = Connection::open_in_memory().unwrap();
        // A v3 database: pin flag present, textual content already TEXT class,
        // a binary row, and an image row whose bytes live in the file cache.
        conn.execute_batch(
            "CREATE TABLE clipboard (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                mime TEXT NOT NULL,
                size INTEGER NOT NULL,
                preview TEXT,
                content BLOB,
                hash TEXT UNIQUE,
                is_pinned INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO clipboard (timestamp, mime, size, preview, content, hash, is_pinned)
            VALUES (1, 'text/plain', 5, 'hello', 'hello', 'h-text', 1),
                   (2, 'application/pdf', 3, NULL, x'010203', 'h-bin', 0),
                   (3, 'image/png', 9, NULL, NULL, 'h-img', 0);
            PRAGMA user_version = 3;",
        )
        .unwrap();

        SchemaManager::initialize(&mut conn, 1000).unwrap();

        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, 4);
        type Row = (String, i64, Option<Vec<u8>>, i64, i64);
        let rows: Vec<Row> = conn
            .prepare("SELECT hash, size, CAST(content AS BLOB), is_pinned, is_chunked FROM clipboard ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows, vec![
            ("h-text".to_owned(), 5, Some(b"hello".to_vec()), 1, 0),
            ("h-bin".to_owned(), 3, Some(vec![1, 2, 3]), 0, 0),
            ("h-img".to_owned(), 9, None, 0, 0),
        ]);
        let chunk_rows: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0)).unwrap();
        assert_eq!(chunk_rows, 0);
    }

    #[test]
    fn migration_from_v1_reaches_v4_in_one_initialisation() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE clipboard (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                mime TEXT NOT NULL,
                size INTEGER NOT NULL,
                preview TEXT,
                content BLOB,
                hash TEXT UNIQUE
            );
            INSERT INTO clipboard (timestamp, mime, size, preview, content, hash)
            VALUES (1, 'text/plain', 2, 'v1', 'v1', 'h-v1');
            PRAGMA user_version = 1;",
        )
        .unwrap();

        SchemaManager::initialize(&mut conn, 1000).unwrap();

        assert!(column_exists(&conn, "clipboard", "is_pinned"));
        assert!(column_exists(&conn, "clipboard", "is_chunked"));
        assert!(table_exists(&conn, "manifest"));
        let (content, chunked): (String, i64) = conn
            .query_row("SELECT content, is_chunked FROM clipboard WHERE hash = 'h-v1'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((content.as_str(), chunked), ("v1", 0));
    }
}
