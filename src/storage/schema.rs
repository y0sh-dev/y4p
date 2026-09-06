// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/storage/schema.rs

use rusqlite::Connection;

/// Bump this and add a `migrate_to_vN` below whenever the schema changes.
const SCHEMA_VERSION: i64 = 1;

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
        // if version < 2 { Self::migrate_to_v2(conn)?; } // e.g. is_pinned column

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
}
