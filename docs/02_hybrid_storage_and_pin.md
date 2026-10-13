# 02 — Hybrid Storage & Pin Protection

`y4p` keeps two kinds of state on disk: a SQLite database, and a plain directory of cache files. It treats them as strictly separate concerns and never lets one leak into the other.

Crate involved: `y4p-storage` (`crates/storage/`). This document explains the split, how schema migrations stay safe across upgrades, how Pin protection was layered on top without disturbing either store, and how rotation reached near-O(1) cost.

| Location | Contents | Created with |
| :--- | :--- | :--- |
| `$XDG_DATA_HOME/y4p/y4p.sqlite` (default `~/.local/share/y4p/`) | Metadata, searchable text, small inline payloads | directory `0700`, file `0600` |
| `$XDG_CACHE_HOME/y4p/<hash>.cache` (default `~/.cache/y4p/`) | Image payloads | directory `0700` |

---

## 1. Why SQLite and the Filesystem Are Kept Apart

Every clipboard entry needs fast metadata lookup (`list`, `search`, MRU ordering) and durable storage for the payload. A single table could do both, with image bytes in a `BLOB` column. But then every large binary the user ever copies lands in the same B-tree pages that `list` and `search` must scan for queries that have nothing to do with that image.

```text
   clipboard event (final bytes, after image routing)
        |
        v
  +-----------------------+        +-----------------------------+
  |  storage::db  (SQL)   |        |  storage::cache  (files)    |
  |                       |  hash  |                             |
  |  id, timestamp, mime  |<------>|  ~/.cache/y4p/<hash>.cache  |
  |  size, preview, hash  |        |  (image payloads only)      |
  |  content, is_pinned   |        |                             |
  +-----------------------+        +-----------------------------+
        ^
        |
   list / search / show
   scan only this table;
   they never touch the cache files
```

The routing rule is exact (`upsert_record`, [`crates/storage/src/db.rs:L48`](../crates/storage/src/db.rs)):

| Payload | `content` column | Cache file |
| :--- | :--- | :--- |
| MIME starts with `image/` (or contains `gif`) | `NULL` | `<hash>.cache` |
| Text-like MIME and valid UTF-8 | bound as `TEXT` | none |
| Anything else | `BLOB` | none |

Keeping images out of SQLite leaves its page cache full of exactly what `list` and `search` need: small metadata rows and searchable text. That keeps those operations close to O(1) in the size of the binary history. Twenty snippets, or twenty snippets plus hundreds of megabytes of screenshots in the cache directory: the query cost barely moves.

> **Invariant — the facade is the only code that knows both stores.** `db.rs` is pure SQL with no filesystem I/O beyond the SQLite file; `cache.rs` is filesystem only with no SQLite. `ClipboardDb` ([`lib.rs:L64`](../crates/storage/src/lib.rs)) is the sole coordinator, which is what stops the separation eroding as features are added.

### 1.1 Deduplication ties the stores together

Every payload carries a SHA3-256 fingerprint. That hash is both the SQLite `UNIQUE` key and the cache filename. Copy the same image twice and you update one row's timestamp; no second copy is written to either store (`FileCache::store` skips an existing path).

The daemon computes the hash **after** image routing (see [00](00_overview.md) section 5), so the key always describes the bytes actually stored. `ClipboardDb::insert_raw` (used by `y4p store`) computes the same digest itself.

---

## 2. Why `PRAGMA user_version` Gates Schema Changes

A clipboard database outlives any single version of the binary. Anyone who has run `y4p` for months has real history in whatever schema shape their installed binary used. A migration must never be able to touch a row it does not need to.

`SchemaManager::initialize` ([`crates/storage/src/schema.rs:L16`](../crates/storage/src/schema.rs)) reads SQLite's built-in `user_version` once, at open, and runs only the migrations the file is missing (`SCHEMA_VERSION = 3`):

```rust
if version < 1 { Self::migrate_to_v1(conn)?; }
if version < 2 { Self::migrate_to_v2(conn)?; }
if version < 3 { Self::migrate_to_v3(conn)?; }
if version < SCHEMA_VERSION { conn.pragma_update(None, "user_version", SCHEMA_VERSION)?; }
```

| Migration | Change | Why it is safe |
| :--- | :--- | :--- |
| v1 | `CREATE TABLE IF NOT EXISTS clipboard (...)`, `idx_ts` | Re-running is a no-op |
| v2 | `ADD COLUMN is_pinned INTEGER NOT NULL DEFAULT 0` (only if the column is absent) | Existing rows simply become "unpinned"; none is rewritten, dropped or reinterpreted |
| v3 | `UPDATE ... SET content = CAST(content AS TEXT)` scoped by `TEXT_MIME_SQL_PREDICATE` | `CAST` to the class a value already has just relabels it; a metadata-only backfill |

> **Invariant — migrations are additive and idempotent.** A brand-new install runs every migration in sequence and ends byte-for-byte equivalent to an old install that migrated one version at a time. No export/import step, no "your history was reset" surprise.

One statement deliberately sits **outside** the version gate: `DROP INDEX IF EXISTS idx_pinned` followed by `CREATE INDEX IF NOT EXISTS idx_pinned_ts ON clipboard(is_pinned, timestamp DESC)`. A database that reached v2 before `idx_pinned_ts` existed would otherwise keep the stale single-column index forever, since `migrate_to_v2` never runs again for it. Both statements are cheap no-ops once correct, so they self-heal on every start.

Connection pragmas applied at open: `journal_mode = WAL`, `synchronous = NORMAL`, `temp_store = MEMORY`, `mmap_size = 256 MiB`, `cache_size = -64000` (about 64 MB), plus a `busy_timeout` of `SQLITE_TIMEOUT_MS`.

---

## 3. Why Pinned Records Are Fully Exempt, Not Just Deprioritised

`Y4P_MAX_HISTORY` (or `[general] max_history` in `y4p.toml`) exists so the database does not grow forever. Before Pin protection, "trim to N entries" meant "keep the N most recent". Once entries can be marked important, "deprioritise eviction for pinned rows" is not enough: a snippet the user explicitly protected must never vanish because the copy *rate* passed the limit while nobody was watching.

The chosen policy is stricter than priority-based eviction. Pinned rows are removed from the eviction pool **and** from the quota count. The selection that defines what expires is scoped to `is_pinned = 0`:

```sql
-- crates/storage/src/db.rs:L135-L144 — upsert_record's rotation step
DELETE FROM clipboard
WHERE id IN (
    SELECT id FROM clipboard
    WHERE is_pinned = 0
    ORDER BY timestamp DESC
    LIMIT -1 OFFSET ?1
)
```

The same scoped selection (returning `hash` instead of `id`) runs first, so the matching cache files can be removed after commit.

The `is_pinned = 0` inside the subquery is what makes this correct rather than "mostly correct". If the inner query ranked *all* rows, every pinned record would still occupy one of the N slots, and the unpinned history would silently shrink to `N - pinned_count`. Scoped, pinning ten records neither risks their deletion nor costs you ten slots of ordinary history.

```text
              upsert_record() rotation step

   +----------+   +----------+   +----------+   +----------+
   | pinned   |   | unpinned |   | unpinned |   |   ...    |
   | excluded |   | slot 1   |   | slot 2   |   | slot N   |
   +----------+   +----------+   +----------+   +----------+
        ^
        |
   never evicted, never counted,
   however old it gets
```

> **Invariant — pin guards against automatic rotation only.** `delete <target>` and `wipe --force` deliberately sit outside the protection. Pin is a safety net for passive history churn; it was never meant to override an explicit instruction to remove that exact record, or a maintenance command whose purpose is a full reset.

---

## 4. Why `NOT IN` Was Replaced with `LIMIT -1 OFFSET`

Rotation used to read `id NOT IN (SELECT id FROM clipboard WHERE is_pinned = 0 ORDER BY timestamp DESC LIMIT ?1)`: "delete anything that is not in the kept set". That is intuitive to write and expensive at scale. SQLite must materialise the kept set (cost ~ N), then walk **every unpinned row**, testing each against it (cost ~ total history), however few rows actually need to expire on this insert. With tens of thousands of entries, every clipboard copy paid an O(total history) tax.

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

Every payload once landed in a `content BLOB` column. That was convenient for the schema and costly for `search`: `BLOB` carries no encoding assumption, so each `content LIKE ?` first needed `CAST(content AS TEXT)`, allocating and reinterpreting bytes as UTF-8 on every candidate row, on every search.

The fix removes the guesswork rather than speeding it up. At insert time `upsert_record` already knows the MIME. If it is text-like and the bytes decode as valid UTF-8, `content` is bound as a Rust `&str`, which SQLite stores as `TEXT` with no cast on the way in or out.

```text
   Before                                  After

   copy text  --> content: BLOB            copy text  --> content: TEXT
   search "foo"                            search "foo"
        |                                       |
   CAST(content AS TEXT) LIKE '%foo%'      content LIKE '%foo%'   (already comparable)
```

Bytes that merely *claim* a textual MIME but fail UTF-8 validation fall back to ordinary `BLOB` storage, so nothing here can corrupt or misrepresent non-text content. Because SQLite's column affinity does not force a single storage class per column, `TEXT` and `BLOB` rows coexist in `content` without altering `CREATE TABLE`; `migrate_to_v3` only backfills.

### 5.1 One definition of "textual", shared by every code path

Four places must agree on which MIME types count as text: `upsert_record` (TEXT vs BLOB), `search_metadata` and `validate_keywords` (which rows `search` may match), and `migrate_to_v3` (which legacy rows to backfill). If any used a narrower or wider rule, the failure would be silent: a `BLOB` row `search` still tries to `LIKE`, or a searchable row `search` skips. Earlier versions did drift this way: `application/json` and `application/xhtml+xml` fell out of search.

There is now one source of truth, `TEXT_LIKE_MIME_HINTS` ([`crates/core/src/constants.rs:L132`](../crates/core/src/constants.rs)): `text`, `json`, `xml`, `xhtml`, `utf8`, `string`, `uri-list`. It is consumed in two forms kept in step:

| Form | Where | Behaviour |
| :--- | :--- | :--- |
| `core::utils::is_text_like_mime` | `y4p-core`, used by `upsert_record` | Case-insensitive substring tests; parameters such as `; charset=utf-8` ignored; `image/*`, `audio/*`, `video/*` rejected first, so `image/svg+xml` never counts as text |
| `TEXT_MIME_SQL_PREDICATE` ([`db.rs:L19`](../crates/storage/src/db.rs)) | `y4p-storage`, interpolated by `search_metadata`, `validate_keywords`, `migrate_to_v3` | `mime NOT LIKE 'image/%'` AND any hint as `mime LIKE '%hint%'` |

> **Invariant — images are excluded on both sides for a structural reason.** Their payloads live in `FileCache` with `content = NULL`, so there is nothing for `content LIKE ?` to match.

---

## 6. The Egress Handle: `ContentLocation`

`locate_content` ([`lib.rs:L201`](../crates/storage/src/lib.rs)) answers "where does this record's payload live?" without materialising it:

| Variant | Meaning | Served by |
| :--- | :--- | :--- |
| `InlineBlob(rowid)` | Payload is in the SQLite row | `get_content_by_id` into a `Vec<u8>` (small by construction) |
| `CacheFile(path)` | Payload is `<hash>.cache` on disk | `sendfile(2)` straight from the file (see [01](01_wayland_streaming_io.md) section 4) |

> **Invariant — large payloads are never copied through the heap on restore.** The daemon translates `ContentLocation` into `wayland::state::SourcePayload`; `y4p-wayland` never sees storage types.
