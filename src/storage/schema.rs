// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/storage/schema.rs

use rusqlite::Connection;

/// Bump this and add a `migrate_to_vN` below whenever the schema changes.
const SCHEMA_VERSION: i64 = 2;

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

        if version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|e| e.to_string())?;
        }

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

        conn.execute("CREATE INDEX IF NOT EXISTS idx_pinned ON clipboard(is_pinned)", []).ok();
        Ok(())
    }
}
