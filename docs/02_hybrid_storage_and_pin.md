# Hybrid Persistence & Pin Protection

`y4p` keeps two kinds of state on disk — a SQLite database and a plain directory of cache files — and treats them as strictly separate concerns. This file explains why that split exists, and how Pin protection (v0.2.0) was layered onto it without disturbing either side.

---

## Why SQLite and the filesystem are separate stores

Every clipboard history entry needs the same two capabilities: fast metadata lookup (`list`, `search`, MRU ordering) and durable storage of the actual payload. A single SQLite table could technically do both — store the image bytes in a BLOB column — but that would mean every large binary the user ever copies gets written directly into the same B-Tree pages that `list`/`search` need to scan to answer a query that has nothing to do with that image.

```
                 ┌───────────────────────┐         ┌──────────────────────────┐
 clipboard event │   storage::db (SQL)   │         │  storage::cache (files)  │
 ───────────────▶│  id · timestamp · mime│         │  ~/.cache/y4p/<hash>.cache│
                 │  size · preview · hash│ ◀──────▶│   (large binaries only)  │
                 │  is_pinned            │  hash    └──────────────────────────┘
                 └───────────────────────┘
                    ▲ list / search / show ▲
                    scans only this table — never touches the cache files
```

Routing large binaries to `~/.cache/y4p/` instead keeps SQLite's page cache full of exactly what `list` and `search` actually need — small rows of metadata and searchable text — so those operations stay close to O(1) with respect to total history size, whether the user has 20 text snippets or 20 text snippets plus a few hundred megabytes of screenshots sitting in the cache directory. `storage/db.rs` (pure SQL, no filesystem I/O) and `storage/cache.rs` (filesystem only, no SQLite) enforce this split at the module boundary — the facade in `storage/mod.rs` is the *only* code that's allowed to know both exist, which is what stops the separation from eroding over time as new features get added.

Deduplication is what ties the two together: every payload is fingerprinted with SHA3-256 on ingest, and that hash is both the SQLite `UNIQUE` key and the cache filename. Copying the same image twice updates one row's timestamp rather than writing a second copy to disk in either store.

## Why `PRAGMA user_version` gates schema changes

A clipboard manager's database outlives any single version of the binary — a user who's had `y4p` running for months has real history sitting in whatever schema the binary had when they installed it. Any migration has to satisfy one non-negotiable requirement: it must never be able to touch a row it doesn't need to.

`schema.rs` reads SQLite's built-in `PRAGMA user_version` once at startup and runs only the migrations the on-disk file is actually missing:

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

Each `migrate_to_vN` is additive by construction — `CREATE TABLE IF NOT EXISTS`, `ALTER TABLE ... ADD COLUMN ... DEFAULT`, `CREATE INDEX IF NOT EXISTS` — so re-running a migration that already applied is a safe no-op rather than an error, and a brand-new install runs every migration in sequence and ends up byte-for-byte equivalent to an old install that migrated one version at a time. This is the mechanism that made Pin protection possible to ship without a data-migration story of its own: `migrate_to_v2` adds `is_pinned INTEGER NOT NULL DEFAULT 0` to the existing `clipboard` table, and every row that existed before v0.2.0 simply becomes "unpinned" — no row is rewritten, dropped, or reinterpreted.

## Why pinned records are fully exempt from rotation — not just deprioritized

`Y4P_MAX_HISTORY` exists so the database doesn't grow forever. Before Pin protection, "trim to N entries" meant one thing: keep the N most recent, drop the rest. Once some entries are marked important enough to pin, "deprioritize eviction for pinned rows" isn't good enough — a snippet the user explicitly protected must never disappear just because the history *rate* passed the limit while it wasn't looking.

The chosen policy is stricter than priority-based eviction: pinned rows are removed from the eviction pool *and* from the quota count entirely. Both halves of the rotation query — the query that decides what to evict, and the query that defines what counts toward the `LIMIT` — are scoped to `is_pinned = 0`:

```sql
-- src/storage/db.rs — upsert_record's rotation step
DELETE FROM clipboard
WHERE is_pinned = 0
  AND id NOT IN (SELECT id FROM clipboard WHERE is_pinned = 0 ORDER BY timestamp DESC LIMIT ?1)
```

The inner subquery's own `is_pinned = 0` filter is the detail that makes this work correctly rather than just "mostly." If only the outer `DELETE` were scoped and the inner "keep the newest N" subquery ranked *all* rows including pinned ones, then every pinned record would still occupy one of the N slots in that ranking — silently shrinking the unpinned history to `N - (pinned count)` instead of leaving a full `N` unpinned slots available. Filtering both halves is what delivers the actual guarantee: pinning ten records neither risks their deletion nor costs the user ten slots of their normal history.

`delete <target>` and `wipe --force` deliberately sit outside this protection. Pin guards against *automatic* rotation silently discarding something the user wanted kept — it was never meant to override an *explicit* instruction to remove that exact record, or a maintenance command whose whole purpose is a full reset. Making pin state block those would turn a safety net for passive history churn into an obstacle for a command the user typed on purpose.

```
                     upsert_record() rotation step
                     ─────────────────────────────
   ┌───────────┐   ┌───────────┐   ┌───────────┐   ┌───────────┐
   │ pinned #1 │   │ unpinned  │   │ unpinned  │   │  ...N     │   ◀── counts toward
   │ (excluded)│   │  (slot 1) │   │  (slot 2) │   │ unpinned  │       Y4P_MAX_HISTORY
   └───────────┘   └───────────┘   └───────────┘   │  slots    │
        ▲                                          └───────────┘
        │
   never evicted, never consumes a slot — regardless of how old it gets
```
