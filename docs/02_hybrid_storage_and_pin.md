# Hybrid Persistence & Pin Protection

`y4p` keeps two kinds of state on disk.

A SQLite database, and a plain directory of cache files. It treats them as strictly separate concerns, and never lets one leak into the other.

This document explains why that split exists. And how Pin protection — new in v0.2.0 — was layered on top of it, without disturbing either side.

---

## Why SQLite and the filesystem are kept apart

Every clipboard entry needs two things. Fast metadata lookup — `list`, `search`, MRU ordering. And durable storage for the actual payload.

A single SQLite table could technically do both. Store the image bytes in a BLOB column, right alongside everything else. But then every large binary the user ever copies gets written directly into the same B-Tree pages that `list` and `search` need to scan — for queries that have nothing to do with that image at all.

```text
   clipboard event
        |
        v
  +-----------------------+        +---------------------------+
  |  storage::db  (SQL)   |        |  storage::cache  (files)  |
  |                       |        |                           |
  |  id, timestamp, mime  |  hash  |  ~/.cache/y4p/<hash>.cache |
  |  size, preview, hash  |<------>|  (large binaries only)    |
  |  is_pinned            |        |                           |
  +-----------------------+        +---------------------------+
        ^
        |
   list / search / show
   scan only this table —
   never touch the cache files
```

Routing large binaries to `~/.cache/y4p/` instead keeps SQLite's page cache full of exactly what `list` and `search` need. Small rows of metadata. Searchable text. Nothing else.

That's what keeps those operations close to O(1) regardless of total history size. Twenty text snippets, or twenty text snippets plus a few hundred megabytes of screenshots sitting in the cache directory — the query cost barely moves.

`storage/db.rs` (pure SQL, no filesystem I/O) and `storage/cache.rs` (filesystem only, no SQLite) enforce this split at the module boundary. The facade in `storage/mod.rs` is the *only* code allowed to know both exist. That's what stops the separation from eroding over time, as new features get added.

Deduplication is what ties the two stores together. Every payload gets fingerprinted with SHA3-256 on ingest. That hash becomes both the SQLite `UNIQUE` key, and the cache filename. Copy the same image twice, and you update one row's timestamp — you never write a second copy to disk, in either store.

---

## Why `PRAGMA user_version` gates schema changes

A clipboard database outlives any single version of the binary.

Someone who's had `y4p` running for months has real history, sitting in whatever schema shape the binary had when they installed it. Any migration has one non-negotiable requirement: it must never be able to touch a row it doesn't need to.

`schema.rs` reads SQLite's built-in `PRAGMA user_version` once, at startup. It runs only the migrations the on-disk file is actually missing:

```rust
// src/storage/schema.rs
if version < 1 {
    Self::migrate_to_v1(conn)?;
}
if version < 2 {
    Self::migrate_to_v2(conn)?;
}
if version < SCHEMA_VERSION {
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
}
```

Each `migrate_to_vN` is additive by construction. `CREATE TABLE IF NOT EXISTS`. `ALTER TABLE ... ADD COLUMN ... DEFAULT`. `CREATE INDEX IF NOT EXISTS`.

Re-running a migration that already applied is a safe no-op, not an error. And a brand-new install runs every migration in sequence, ending up byte-for-byte equivalent to an old install that migrated one version at a time.

This is the mechanism that made Pin protection possible to ship without a data-migration story of its own. `migrate_to_v2` adds `is_pinned INTEGER NOT NULL DEFAULT 0` to the existing `clipboard` table. Every row that existed before v0.2.0 simply becomes "unpinned." No row gets rewritten. No row gets dropped. No row gets reinterpreted.

---

## Why pinned records are fully exempt, not just deprioritized

`Y4P_MAX_HISTORY` exists so the database doesn't grow forever.

Before Pin protection, "trim to N entries" meant one thing: keep the N most recent, drop the rest.

Once some entries are marked important enough to pin, "deprioritize eviction for pinned rows" isn't good enough. A snippet the user explicitly protected must never disappear — just because the history *rate* passed the limit while nobody was watching.

The chosen policy is stricter than priority-based eviction. Pinned rows are removed from the eviction pool entirely. And from the quota count entirely, too. Both halves of the rotation query — the query deciding what to evict, and the query defining what counts toward the `LIMIT` — are scoped to `is_pinned = 0`:

```sql
-- src/storage/db.rs — upsert_record's rotation step
DELETE FROM clipboard
WHERE is_pinned = 0
  AND id NOT IN (
    SELECT id FROM clipboard WHERE is_pinned = 0 ORDER BY timestamp DESC LIMIT ?1
  )
```

The inner subquery's own `is_pinned = 0` filter is the detail that makes this correct, rather than just "mostly correct."

If only the outer `DELETE` were scoped — and the inner "keep the newest N" subquery ranked *all* rows, pinned included — then every pinned record would still occupy one of the N slots in that ranking. The unpinned history would silently shrink to `N minus pinned count`, instead of leaving a full `N` unpinned slots available.

Filtering both halves is what delivers the real guarantee. Pin ten records, and you neither risk their deletion, nor lose ten slots of your normal history.

```text
              upsert_record() rotation step
              ------------------------------

   +----------+   +----------+   +----------+   +----------+
   | pinned   |   | unpinned |   | unpinned |   |   ...    |
   | (excluded|   | slot 1   |   | slot 2   |   | slot N   |
   | forever) |   |          |   |          |   |          |
   +----------+   +----------+   +----------+   +----------+
        ^
        |
   never evicted, never counted —
   no matter how old it gets
```

`delete <target>` and `wipe --force` deliberately sit outside this protection.

Pin guards against *automatic* rotation quietly discarding something you wanted kept. It was never meant to override an *explicit* instruction to remove that exact record — or a maintenance command whose entire purpose is a full reset. Making pin state block those would turn a safety net for passive history churn into an obstacle for something you typed on purpose.
