# 02 — Hybrid Storage & Pin Protection

`y4p` keeps two kinds of state on disk: a SQLite database, and a plain directory of cache files. It treats them as strictly separate concerns and never lets one leak into the other.

Crate involved: `y4p-storage` (`crates/storage/`). This document explains the split, how schema migrations stay safe across upgrades, how Pin protection is layered on top, how rotation reached near-O(1) cost, how legacy chunk rows remain readable, and how the cache directory is bounded.

| Location | Contents | Created with |
| :--- | :--- | :--- |
| `$XDG_DATA_HOME/y4p/y4p.sqlite` (default `~/.local/share/y4p/`) | Metadata, searchable text and every inline payload, at any size | directory `0700`, file `0600` |
| `$XDG_CACHE_HOME/y4p/<hash>.cache` (default `~/.cache/y4p/`) | Cache-class payloads (images and GIF-like types), bounded by a 512 MiB LRU quota | directory `0700` |

---

## 1. Why SQLite and the Filesystem Are Kept Apart

Every clipboard entry needs fast metadata lookup (`list`, `search`, MRU ordering) and durable storage for the payload. A single table could do both, with image bytes in a `BLOB` column. But then every large binary the user ever copies lands in the same B-tree pages that `list` and `search` must scan for queries that have nothing to do with that image.

```text
   clipboard event (final bytes, after optional image routing)
        |
        v
  +-----------------------+        +-----------------------------+
  |  storage::db  (SQL)   |        |  storage::cache  (files)    |
  |                       |  hash  |                             |
  |  id, timestamp, mime  |<------>|  ~/.cache/y4p/<hash>.cache  |
  |  size, preview, hash  |        |  (cache-class payloads)     |
  |  content, is_pinned   |        |                             |
  +-----------------------+        +-----------------------------+
        ^
        |
   list / search / show
   scan only this table;
   they never touch the cache files
```

The routing rule is exact (`upsert_record` / `row_parts`, [`crates/storage/src/db.rs`](../crates/storage/src/db.rs)):

| Payload | `content` column | Where the bytes live |
| :--- | :--- | :--- |
| MIME starts with `image/` (or contains `gif`) and is not text-like | `NULL` | `<hash>.cache` |
| Text-like MIME and valid UTF-8 | bound as `TEXT` | the row |
| Anything else | `BLOB` | the row |

Size is no longer part of the decision: every non-cache payload is stored whole in its row, so a 20 MiB PDF and a 20-byte note follow the same path. Keeping images out of SQLite leaves its page cache full of small metadata rows and searchable text, which keeps `list` and `search` close to O(1) in the size of the binary history.

> **Invariant — the facade is the only code that knows both stores.** `db.rs` is pure SQL with no filesystem I/O beyond the SQLite file; `cache.rs` is filesystem only with no SQLite. `ClipboardDb` ([`lib.rs`](../crates/storage/src/lib.rs)) is the sole coordinator, which is what stops the separation eroding as features are added.

### 1.1 Deduplication ties the stores together

Every payload carries a SHA3-256 fingerprint. That hash is both the SQLite `UNIQUE` key and the cache filename. Copy the same image twice and you update one row's timestamp; no second copy is written to either store (`FileCache::store` skips an existing path). `ClipboardDb::insert_with_hash` places the cache file **before** the row and propagates a failed write (`cache write failed: ...`), so a row never exists whose payload is missing; if the row then fails to commit, the placed file is removed again.

The daemon computes the hash **after** optional image routing (see [00](00_overview.md) section 5), so the key always describes the bytes actually stored. `ClipboardDb::insert_raw` (used by `y4p store`) computes the same digest itself.

`wipe` also gives memory back: after `VACUUM` and the cache clear, `PRAGMA shrink_memory` empties the connection's page cache and `core::memory::release_unused_heap` (`malloc_trim(0)` on glibc) returns the allocator's free pages to the kernel. Since v0.8.5 that work runs in the **daemon**, not the CLI process: `y4p wipe --force` sends `IPC_CMD_WIPE` over the socket and the daemon executes `ClipboardDb::wipe()` on its single writer connection ([03](03_concurrency_and_memory.md) section 2.2). When no daemon is listening, the CLI falls back to its own handle, which is safe because nothing else is writing.

---

## 2. Why `PRAGMA user_version` Gates Schema Changes

A clipboard database outlives any single version of the binary. `SchemaManager::initialize` ([`crates/storage/src/schema.rs`](../crates/storage/src/schema.rs)) reads SQLite's built-in `user_version` once, at open, and runs only the migrations the file is missing (`SCHEMA_VERSION = 4`):

```rust
if version < 1 { Self::migrate_to_v1(conn)?; }
if version < 2 { Self::migrate_to_v2(conn)?; }
if version < 3 { Self::migrate_to_v3(conn)?; }
if version < 4 { Self::migrate_to_v4(conn)?; }
if version < SCHEMA_VERSION { conn.pragma_update(None, "user_version", SCHEMA_VERSION)?; }
```

| Migration | Change | Why it is safe |
| :--- | :--- | :--- |
| v1 | `CREATE TABLE IF NOT EXISTS clipboard (...)`, `idx_ts` | Re-running is a no-op |
| v2 | `ADD COLUMN is_pinned INTEGER NOT NULL DEFAULT 0` (only if absent) | Existing rows simply become "unpinned"; none is rewritten or dropped |
| v3 | `UPDATE ... SET content = CAST(content AS TEXT)` scoped by `TEXT_MIME_SQL_PREDICATE` | A metadata-only backfill of the storage class |
| v4 | `ADD COLUMN is_chunked INTEGER NOT NULL DEFAULT 0` (only if absent); `CREATE TABLE IF NOT EXISTS chunks`, `manifest` | Purely additive: every existing row backfills as inline and keeps its `content` |

> **Invariant — migrations are additive and idempotent.** A brand-new install runs every migration in sequence and ends equivalent to an old install that migrated one version at a time. No export/import step, no "your history was reset" surprise.

One statement deliberately sits **outside** the version gate: `DROP INDEX IF EXISTS idx_pinned` followed by `CREATE INDEX IF NOT EXISTS idx_pinned_ts ON clipboard(is_pinned, timestamp DESC)`. A database that reached v2 before `idx_pinned_ts` existed would otherwise keep the stale single-column index forever. Both statements are cheap no-ops once correct, so they self-heal on every start.

Connection pragmas applied at open: `journal_mode = WAL`, `synchronous = NORMAL`, `temp_store = MEMORY`, `mmap_size = 256 MiB`, `cache_size = -64000` (about 64 MB), plus a `busy_timeout` of `SQLITE_TIMEOUT_MS`.

---

## 3. Why Pinned Records Are Fully Exempt, Not Just Deprioritised

`Y4P_MAX_HISTORY` (or `[general] max_history` in `y4p.toml`) exists so the database does not grow forever. Once entries can be marked important, "deprioritise eviction for pinned rows" is not enough: a snippet the user explicitly protected must never vanish because the copy *rate* passed the limit while nobody was watching.

Pinned rows are removed from the eviction pool **and** from the quota count. The selection that defines what expires is scoped to `is_pinned = 0`:

```sql
-- crates/storage/src/db.rs — upsert's rotation step
DELETE FROM clipboard
WHERE id IN (
    SELECT id FROM clipboard
    WHERE is_pinned = 0
    ORDER BY timestamp DESC
    LIMIT -1 OFFSET ?1
)
```

The same scoped selection (returning `hash` instead of `id`) runs first, so the matching cache files can be removed after commit.

The `is_pinned = 0` inside the subquery is what makes this correct rather than "mostly correct". If the inner query ranked *all* rows, every pinned record would still occupy one of the N slots, and the unpinned history would silently shrink to `N - pinned_count`. Scoped, pinning ten records neither risks their deletion nor costs ten slots of ordinary history.

```text
              upsert() rotation step

   +----------+   +----------+   +----------+   +----------+
   | pinned   |   | unpinned |   | unpinned |   |   ...    |
   | excluded |   | slot 1   |   | slot 2   |   | slot N   |
   +----------+   +----------+   +----------+   +----------+
        ^
        |
   never evicted, never counted,
   however old it gets
```

> **Invariant — pin guards against automatic rotation and cache eviction only.** `delete <target>` and `wipe --force` deliberately sit outside the protection. Pin is a safety net for passive churn; it was never meant to override an explicit instruction to remove that exact record.

---

## 4. Why `NOT IN` Was Replaced with `LIMIT -1 OFFSET`

Rotation used to read `id NOT IN (SELECT id FROM clipboard WHERE is_pinned = 0 ORDER BY timestamp DESC LIMIT ?1)`: "delete anything that is not in the kept set". SQLite must materialise the kept set (cost ~ N), then walk **every unpinned row**, testing each against it (cost ~ total history), however few rows actually need to expire.

`LIMIT -1 OFFSET ?1`, combined with `idx_pinned_ts` on `(is_pinned, timestamp DESC)`, asks a different question: "skip the first N rows of this index and give me everything after". SQLite seeks straight to position N; the cost scales with how many rows actually expired, which on a steady-state daemon is almost always zero or one.

```text
   Before: NOT IN (subquery)              After: LIMIT -1 OFFSET

   1. build set of N kept rows            1. seek to position N in
      (memory cost ~ N)                      idx_pinned_ts (O(log N))
   2. walk ALL unpinned rows,              2. read forward from there:
      test each against the set               only the expiring rows
      (cost ~ total history size)             (cost ~ rows actually expiring)
```

The `-1` is not a typo: it is SQLite's spelling of "no limit", used purely to unlock the `OFFSET` clause.

---

## 5. Why Text Is Stored as `TEXT`, Not `BLOB`

Every payload once landed in a `content BLOB` column. That was convenient for the schema and costly for `search`: `BLOB` carries no encoding assumption, so each `content LIKE ?` first needed `CAST(content AS TEXT)`.

The fix removes the guesswork rather than speeding it up. At insert time `upsert_record` already knows the MIME. If it is text-like and the bytes decode as valid UTF-8, `content` is bound as a Rust `&str`, which SQLite stores as `TEXT` with no cast on the way in or out. Bytes that merely *claim* a textual MIME but fail UTF-8 validation fall back to ordinary `BLOB` storage.

Because SQLite's column affinity does not force a single storage class per column, `TEXT` and `BLOB` rows coexist in `content` without altering `CREATE TABLE`; `migrate_to_v3` only backfills.

### 5.1 One definition of "textual", shared by every code path

Four places must agree on which MIME types count as text: `upsert_record` (TEXT vs BLOB), `search_metadata` and `validate_keywords` (which rows `search` may match), and `migrate_to_v3` (which legacy rows to backfill). If any used a narrower or wider rule, the failure would be silent.

There is one source of truth, `TEXT_LIKE_MIME_HINTS` ([`crates/core/src/constants.rs`](../crates/core/src/constants.rs)): `text`, `json`, `xml`, `xhtml`, `utf8`, `string`, `uri-list`. It is consumed in two forms kept in step:

| Form | Where | Behaviour |
| :--- | :--- | :--- |
| `core::utils::is_text_like_mime` | `y4p-core`, used by `upsert_record` | Case-insensitive substring tests; parameters ignored; `image/*`, `audio/*`, `video/*` rejected first, so `image/svg+xml` never counts as text |
| `TEXT_MIME_SQL_PREDICATE` ([`db.rs`](../crates/storage/src/db.rs)) | `y4p-storage`, interpolated by `search_metadata`, `validate_keywords`, `migrate_to_v3` | `mime NOT LIKE 'image/%'` AND any hint as `mime LIKE '%hint%'` |

> **Invariant — images are excluded on both sides for a structural reason.** Their payloads live in `FileCache` with `content = NULL`, so there is nothing for `content LIKE ?` to match.

---

## 6. Legacy Chunk Rows (Schema v4, read-only compatibility)

SQLite v4 added `chunks` / `manifest` tables and a per-row `is_chunked` flag, and an older `y4p` wrote large non-image payloads as content-defined chunks there. **New writes no longer use them.** Since v0.9.2 every non-cache payload is stored inline and `is_chunked` is always `0`; a database opened from an older version keeps its chunked rows exactly as they are.

The read, search, rotation, delete and wipe paths still understand them:

| Path | Behaviour for a legacy `is_chunked = 1` row |
| :--- | :--- |
| Read (`get_content_by_id`) | `reassemble` joins `manifest` to `chunks` in `seq` order into one buffer. It is served only if it adds up to exactly the recorded `size`; a missing chunk or a size mismatch yields `None` and an error log, never a shorter payload |
| Search (`search_metadata`, `validate_keywords`) | `BODY_TEXT_SQL` is a `CASE` on `is_chunked`: inline rows match `content` directly, chunked rows match the SQL-side concatenation, so a keyword may straddle a boundary |
| Rotation, `delete_by_id` | `release_chunks` runs inside the transaction that deletes the row: it subtracts each chunk's reference count, deletes chunks that reach zero, then deletes the manifest. Pinned rows are exempt exactly as before |
| `wipe` | Deletes `manifest`, `chunks` and `clipboard` in one transaction |

> **Invariant — no new chunk writes, and no legacy row left unreadable.** `schema.rs` keeps the v4 table definitions; `store_chunks` is gone. The compatibility tests insert v4-shaped rows directly through SQL and exercise read, search, rotation, deletion, wipe and the missing/size-mismatch refusals.

FastCDC itself (`core::cdc`) and the `CDC_*` constants remain in the tree only because the baseline benchmark harness compares against the historical storage shape; no persistence path calls them.

---

## 7. The Egress Handle: `ContentLocation`

`locate_content` ([`crates/storage/src/lib.rs`](../crates/storage/src/lib.rs)) answers "where does this record's payload live?" without materialising it. A cache-class record is only reported once its file is confirmed on disk (`Path::is_file`): a missing `<hash>.cache` is an `Err` naming the row and the expected path, so the daemon logs it and serves nothing instead of offering a descriptor that cannot be opened:

| Variant | Meaning | Served by |
| :--- | :--- | :--- |
| `InlineBlob(rowid)` | Payload is in the SQLite database: in the row, or reassembled from legacy chunks | `get_content_by_id` into a `Vec<u8>` |
| `CacheFile(path)` | Payload is `<hash>.cache` on disk | `sendfile(2)` straight from the file (see [01](01_wayland_streaming_io.md) section 4) |

> **Invariant — large payloads are never copied through the heap on restore.** The daemon translates `ContentLocation` into `core::payload::Payload`; `y4p-wayland` never sees storage types. (A legacy chunked row is the exception by design: it is reassembled into one buffer, the same copy the inline read makes.)

---

## 8. The Cache Quota (v0.9.5)

History rotation bounds the number of rows, not the bytes they point at: `max_history * MAX_PAYLOAD_BYTES` is still a very large number. The cache directory therefore has its own ceiling, `CACHE_QUOTA_BYTES` = 512 MiB.

After a cache-class insert, `ClipboardDb::insert_with_hash` calls `enforce_cache_quota(CACHE_QUOTA_BYTES, keep_id)`. While `FileCache::total_bytes()` exceeds the quota:

1. `SqliteStore::oldest_unpinned_cache_row(keep_id)` returns the candidate: `is_pinned = 0 AND content IS NULL AND is_chunked = 0 AND hash IS NOT NULL AND id != keep_id`, ordered `timestamp ASC, id ASC` (the least recently used).
2. The row is deleted through the ordinary `delete_by_id`, and its `<hash>.cache` is removed.

| Protected | Why |
| :--- | :--- |
| Pinned rows | Pin is absolute: a pinned image is never evicted, even if that leaves the cache above the quota |
| The row that triggered the insert (`keep_id`) | A successful clipboard write must never immediately delete what it just stored |
| Inline and legacy chunk rows | They hold no cache file, so evicting them would reclaim nothing |

A re-copied payload has its `timestamp` refreshed by `upsert`, so it naturally moves to the back of the LRU order. Enforcement is best effort after the row commits; a failure is logged and never turns a successful clipboard write into an error. Rows that are already over the quota when no new cache-class insert arrives are left until the next one: the quota bounds growth, it is not a background sweeper.
