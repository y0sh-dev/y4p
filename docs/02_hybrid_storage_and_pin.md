# Hybrid Persistence & Pin Protection

`y4p` keeps two kinds of state on disk.

A SQLite database, and a plain directory of cache files. It treats them as strictly separate concerns, and never lets one leak into the other.

This document explains why that split exists, how Pin protection — introduced in v0.2.0 — was layered on top of it without disturbing either side, and how v0.2.5's storage optimizations (O(1) rotation, `TEXT`-native text) sharpened both without changing that original split at all.

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
if version < 3 {
    Self::migrate_to_v3(conn)?;
}
if version < SCHEMA_VERSION {
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
}
```

Each `migrate_to_vN` is additive by construction. `CREATE TABLE IF NOT EXISTS`. `ALTER TABLE ... ADD COLUMN ... DEFAULT`. `CREATE INDEX IF NOT EXISTS`.

Re-running a migration that already applied is a safe no-op, not an error. And a brand-new install runs every migration in sequence, ending up byte-for-byte equivalent to an old install that migrated one version at a time.

This is the mechanism that made Pin protection possible to ship without a data-migration story of its own. `migrate_to_v2` adds `is_pinned INTEGER NOT NULL DEFAULT 0` to the existing `clipboard` table. Every row that existed before that version simply becomes "unpinned." No row gets rewritten. No row gets dropped. No row gets reinterpreted.

`migrate_to_v3` — the newest one — leans on the exact same guarantee for a very different kind of change: moving textual content from `BLOB` storage class to `TEXT` (the next section explains why that move matters). It runs one `UPDATE ... SET content = CAST(content AS TEXT)`, scoped to rows whose `mime` already looks textual. `CAST` on a value that's already the target storage class just relabels it — the bytes on disk don't change, so this is a metadata-only backfill, not a rewrite of anyone's clipboard history. A user upgrading from a v0.2.0 database gets the new, faster storage class for their existing rows automatically, the first time the new binary opens their database — no export/import step, no "your history was reset" surprise.

---

## Why pinned records are fully exempt, not just deprioritized

`Y4P_MAX_HISTORY` exists so the database doesn't grow forever.

Before Pin protection, "trim to N entries" meant one thing: keep the N most recent, drop the rest.

Once some entries are marked important enough to pin, "deprioritize eviction for pinned rows" isn't good enough. A snippet the user explicitly protected must never disappear — just because the history *rate* passed the limit while nobody was watching.

The chosen policy is stricter than priority-based eviction. Pinned rows are removed from the eviction pool entirely. And from the quota count entirely, too. Both halves of the rotation query — the query deciding what to evict, and the query defining what counts toward the `LIMIT` — are scoped to `is_pinned = 0`:

```sql
-- src/storage/db.rs — upsert_record's rotation step
DELETE FROM clipboard
WHERE id IN (
    SELECT id FROM clipboard
    WHERE is_pinned = 0
    ORDER BY timestamp DESC
    LIMIT -1 OFFSET ?1
)
```

The `is_pinned = 0` filter, present in *both* the outer delete and the inner row selection, is the detail that makes this correct, rather than just "mostly correct."

If only the outer `DELETE` were scoped — and the inner "keep the newest N" query ranked *all* rows, pinned included — then every pinned record would still occupy one of the N slots in that ranking. The unpinned history would silently shrink to `N minus pinned count`, instead of leaving a full `N` unpinned slots available.

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

---

## Why `NOT IN` was replaced with `LIMIT -1 OFFSET`

The rotation query above didn't always look like that. Until recently, both halves read `id NOT IN (SELECT id FROM clipboard WHERE is_pinned = 0 ORDER BY timestamp DESC LIMIT ?1)` — "delete anything that isn't in the kept set."

That phrasing is intuitive to write, and expensive to run at scale.

To evaluate a `NOT IN (subquery)`, SQLite first has to materialize the entire subquery — every one of the N kept rows — into a temporary lookup structure in memory (in practice, something functioning like a checklist it can test membership against quickly: think of it as SQLite building a giant paper checklist of "rows to keep", then walking every single unpinned row in the table one by one, asking "is your ID on this checklist?"). Then it walks *every unpinned row in the table*, checking each one against that checklist. The checklist-building cost is roughly proportional to how much history you keep; the checklist-walking cost is proportional to your *entire* unpinned history, no matter how few rows actually need to expire on this particular insert.

With a history capped at a few hundred entries, that's unnoticeable. With tens of thousands — the exact regime `Y4P_MAX_HISTORY` exists to make possible — every single clipboard copy was paying an O(total history) tax just to figure out which one or two rows had aged out.

`LIMIT -1 OFFSET ?1`, combined with the `idx_pinned_ts` index on `(is_pinned, timestamp DESC)`, asks a completely different question: "skip the first N rows in this index, and give me everything after that." SQLite can answer that by seeking directly to position N in the index — no checklist, no full walk. The cost now scales with *how many rows actually expired*, which on a steady-state daemon is almost always zero or one.

```text
   Before: NOT IN (subquery)              After: LIMIT -1 OFFSET

   1. build checklist of N kept rows      1. seek to position N in
      (memory cost ~ N)                      idx_pinned_ts (O(log N))
   2. walk ALL unpinned rows,              2. read forward from there —
      test each against checklist            only the expiring rows
      (cost ~ total history size)            (cost ~ rows actually expiring)
```

The `-1` in `LIMIT -1 OFFSET ?1` isn't a typo — it's SQLite's syntax for "no limit," used here purely to unlock the `OFFSET` clause. `SELECT ... LIMIT -1 OFFSET N` means "every row after skipping the first N," which is exactly the "everything beyond the kept quota" set this needed all along.

---

## Why text is stored as `TEXT`, not `BLOB`, from the start

Every piece of clipboard content used to land in the same `content BLOB` column, regardless of whether it was a screenshot or a line of copied shell output.

That uniformity was convenient for the schema, and costly for search. SQLite's `BLOB` storage class carries no assumption about encoding — it's just bytes. So every time `search`'s `content LIKE ?` needed to compare a row's content against a keyword, it first had to run `CAST(content AS TEXT)`: allocate a new buffer, walk the raw bytes, and reinterpret them as UTF-8 text, on every single matching candidate row, every time a search ran.

For a screenshot that never gets searched, that cost never mattered — the mime filter excludes it long before the `CAST` would run. For a text snippet, which is exactly the kind of thing `search` exists to find, it meant paying an encoding-detection tax on content that was textual from the moment it was copied.

The fix removes the guesswork instead of speeding it up. At insert time, `upsert_record` already knows the mime type. If it looks textual (`text/*`, `*uri-list*`, `*json*`, `text/html`, `*xhtml*`) and the bytes genuinely decode as valid UTF-8, the row is inserted with `content` bound as a Rust `&str` rather than `&[u8]` — which hands SQLite a value that adopts `TEXT` storage class directly, no cast required on the way in, and none required on the way back out.

```text
   Before                                  After

   copy text  --> content: BLOB            copy text  --> content: TEXT
                       |                                        |
   search "foo"        v                   search "foo"         v
        |         CAST(content AS TEXT)         |          content LIKE '%foo%'
        |         LIKE '%foo%'                  |          (no cast — already
        +-------> (cast on every candidate)     +--------> comparable as-is)
```

Bytes that merely *claim* a textual mime but fail UTF-8 validation still fall back to ordinary `BLOB` storage — nothing about this optimization risks corrupting or misrepresenting content that isn't actually text. And because SQLite's column *type affinity* doesn't force a single storage class per column, `TEXT` and `BLOB` rows coexist in the same `content` column without any change to the table's `CREATE TABLE` definition — `migrate_to_v3` only had to backfill existing rows, never alter the schema itself.
