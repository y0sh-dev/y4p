# 04 — Stable IDs & the Strict CLI

`y4p` is designed to sit inside other people's pipelines: `fzf`, `rofi`, `awk`, shell scripts that select an entry and act on it. That audience changes what "correct" means for a CLI. A script cannot ask a clarifying question when input is ambiguous, and it cannot tell "the tool did what I meant" from "the tool guessed wrong" unless the tool is willing to fail loudly instead of guessing.

Crates involved: `y4p-cli` (`utils.rs`, `list.rs`, `search.rs`, `pin.rs`, ...) and `y4p-storage` (`db.rs`: `fetch_metadata`, `search_metadata`).

---

## 1. Why Display Order and Identity Are Two Different Numbers

`y4p list` shows entries most-recent-first, which is the useful order for a human. But that order is not stable: copying something new, or restoring an older entry (which bumps its timestamp via MRU promotion), shifts every index below it. A script that captured "entry #3" a moment ago and acts on "#3" now can act on completely different content: a silent misfire, not a crash.

So every record carries two coordinates:

| Coordinate | Source | Selected by | Stable across clipboard activity? |
| :--- | :--- | :--- | :--- |
| MRU position | rank in `ORDER BY timestamp DESC` | default | **No** |
| Stable ID | SQLite `id` (`AUTOINCREMENT`, immutable) | `--id` / `-i` | **Yes** |

The two are computed so they always agree with each other. `fetch_metadata` (plain `list`) derives a row's position from its rank in `ORDER BY timestamp DESC`. `search_metadata` derives the identical number for a search hit with a per-row correlated count ([`crates/storage/src/db.rs:L186`](../crates/storage/src/db.rs)):

```sql
SELECT (SELECT COUNT(*) FROM clipboard c2 WHERE c2.timestamp > c1.timestamp) AS abs_idx,
       id, timestamp, mime, size, preview, is_pinned
FROM clipboard c1
WHERE {TEXT_MIME_SQL_PREDICATE}              -- shared constant, see 02
  AND (preview LIKE ?1 OR content LIKE ?1)   -- one clause per keyword, joined with AND
ORDER BY timestamp DESC LIMIT ?N
```

The `mime` filter is the shared `TEXT_MIME_SQL_PREDICATE`, derived from `TEXT_LIKE_MIME_HINTS`, so `search`, keyword validation and the on-disk `TEXT` storage class agree on what counts as text (see [02](02_hybrid_storage_and_pin.md) section 5).

`search` once numbered its hits `0, 1, 2, ...` by local position within the *results*, an index space unrelated to `list`'s absolute history position. Piping a `search` hit's index into `copy-to` without `--id` could silently restore the wrong entry. Computing each hit's *absolute* position ("how many rows are newer than this one") closed that gap.

```text
  list  (MRU order)              search "rust"  (same abs_idx space)

  +----+-------------+           +-----+-------------+
  |  0 | id=42 (new) |           | idx |             |
  |  1 | id=41       |----+      |  1  | id=41       |  <-- same index,
  |  2 | id=40       |    |      +-----+-------------+      same record
  |  3 | id=39       |    |
  +----+-------------+    |
                           +----------------------------> copy-to 1
                                                            copy-to --id 41
```

Either form works once the index spaces agree: `copy-to 1` when the MRU position is what is wanted, `copy-to --id 41` when a script captured a stable ID earlier and needs a guarantee that intervening clipboard activity cannot change what it points to.

> **Invariant — one index space.** An index displayed by `search` means exactly what the same index means in `list`.

---

## 2. Why the Window Function Was Eliminated, Not Just Optimised

Keeping `search`'s index space aligned with `list`'s used to mean sharing one SQL idiom: both computed `ROW_NUMBER() OVER (ORDER BY timestamp DESC) - 1` across the whole table, then filtered. A window function cannot be evaluated lazily row by row: SQLite gathers every row the `ORDER BY` applies to, sorts it in full (a temporary B-tree sized to the *entire* history, not to the results), and only then hands out sequence numbers, before the keyword filter can discard anything. Past roughly 50,000 rows, a keyword matching a handful of entries still paid tens of milliseconds to build and sort a structure representing everything ever copied.

The correlated `COUNT(*)` of section 1 is evaluated per candidate row, so the planner is free to run the mime/keyword filter **first** (index-assisted via `idx_ts` / `idx_pinned_ts`) and pay the count only for survivors.

```text
   Before: ROW_NUMBER() window function      After: correlated COUNT(*)

   1. gather + sort ALL rows                 1. filter by mime + keyword
      (temp B-tree ~ full history size)         first (index-assisted)
   2. THEN apply mime/keyword filter          2. for each surviving row,
      to the numbered results                    COUNT(*) newer rows
                                                 (cost ~ match count, not
                                                  total history size)
```

Accepted trade-off: two rows with the exact same millisecond `timestamp` used to receive distinct (if arbitrary) numbers and now collapse to an identical `abs_idx`. At this table's granularity and with clipboard events this rare, a collision is not a case anyone has hit, and the index was only ever a display/lookup convenience, never a uniqueness guarantee the schema relies on.

---

## 3. Why `--raw` Output Is an Unbreakable Contract

Most output is for a person reading a terminal. `--raw` is not. `y4p list --raw` and `y4p search --raw` exist so `y4p-rofi`, `fzf` pipelines and shell scripts have something stable to parse, with nothing decorative mixed in. The moment that format is "just how we currently print things", it becomes a moving target that external tooling has already built on.

### 3.1 The format

One record per line, produced by `render_list` ([`crates/cli/src/list.rs:L85`](../crates/cli/src/list.rs)):

```text
[  0] - [FIL] /home/u/a.txt                             
[  1] * [TXT] second entry: rust                        
[  2] - [TXT] hello world from y4p                      
 ^^^^^ ^ ^^^^^ ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |   |   |                  preview, padded to 42 columns
   |   |   label: [TXT] text, [IMG] image, [FIL] uri-list, [BIN] other
   |   pin marker: "*" pinned, "-" not pinned (always present)
   index (MRU position, or stable ID with --id), right-aligned in brackets, width 3
```

| Field | Rule |
| :--- | :--- |
| Index | `[` + right-aligned number, width 3 + `]`; the MRU position, or the stable ID with `--id` |
| Pin marker | Always one non-space token (`*` / `-`), so the field count never depends on pin state |
| Label | `[TXT]`, `[IMG]`, `[FIL]`, `[BIN]` |
| Preview | Newlines, carriage returns and tabs replaced by spaces; truncated by display width with `...`; right-padded to 42 columns; images show `[subtype] - N bytes` |

### 3.2 Parsing it

The index is padded **inside** the brackets, so whitespace splitting does not isolate it: `awk '{print $1}'` returns `[` for IDs below 100 and `[123]` (brackets included) otherwise. Extract the number explicitly:

```bash
y4p list 0-100 --raw --id | fzf | sed -E 's/^\[ *([0-9]+)\].*/\1/' | xargs -r y4p copy-to --id
```

> **Invariant — `--raw` is a published interface.** Any change to its field order, delimiters, padding or marker tokens is a breaking change for every script that parses it, in the same sense as changing a public function signature, except that no compiler will flag it. Such a change has to be reasoned about at that level, never as a cosmetic diff to a `println!`.

`y4p show <target> --raw` is the binary counterpart: it writes the stored payload bytes to stdout with nothing added, so `y4p show 12 --id --raw > recovered_asset.png` round-trips content.

---

## 4. Negative and Malformed Indices

`resolve_target_id` ([`crates/cli/src/utils.rs:L139`](../crates/cli/src/utils.rs)) is the single function every index-accepting subcommand (`pin`, `unpin`, `show`, `delete`, `copy-to`) routes its positional argument through. It parses as `i64`, not `usize`, so the sign survives long enough to be checked deliberately:

```rust
let val = input.parse::<i64>().map_err(|_| format!("invalid numerical value: '{}'", input))?;
if val < 0 {
    return Err(format!("index cannot be negative: {}", val));
}
```

Parsing straight to `usize` would let a cast reinterpret `-1` as `usize::MAX`, a syntactically valid but absurd index that fails far from where the mistake was made. Rejecting at the door names the exact bad input.

> **What actually happens on the command line.** A leading `-` makes the argument a flag cluster, so `y4p show -1` never reaches `resolve_target_id`: it is collected as the unknown flag `-1` and refused with `command 'show' does not support specified options.` The `i64` check therefore stands as defence in depth for any caller that does not go through argument parsing, and guarantees that no negative value can ever become a `usize` index.

---

## 5. Why Unknown Flags Are a Hard Error, Not a Best Guess

Most CLI parsers, facing a flag they do not recognise, either ignore it or treat it as a positional argument. Both are dangerous here, because the positional argument is often "which history entry to permanently delete". A swallowed typo such as `y4p delete --Id 5` for `--id 5` does not fail; it quietly does something other than intended, on a command whose entire purpose is destructive.

`ArgContext::parse` ([`crates/cli/src/utils.rs:L36`](../crates/cli/src/utils.rs)) takes the opposite stance. Every recognised flag is enumerated; anything else, long or short, is collected rather than dropped:

```rust
match arg.as_str() {
    "--raw" => ctx.raw = true,
    "--full" => ctx.full = true,
    // ... every recognised flag, explicitly ...
    _ => ctx.unknown_flags.push(arg.clone()),
}
```

| Long | Short | Meaning |
| :--- | :--- | :--- |
| `--raw` | `-R` | Machine-readable output |
| `--full` | `-A` | All entries (`list`) |
| `--force` | `-f` | Confirm destructive action (`wipe`) |
| `--verbose` | `-v` | Detailed logging |
| `--id` | `-i` | Interpret the target as a stable ID |
| `--list-types` | `-l` | `paste-from` only: print the offered MIME types |
| `--help` / `--version` | `-h` / `-V` | Help / version |

Parsing is global; **each subcommand then declares which of the parsed flags apply to it and rejects the rest**:

| Command | Rejects (besides unknown flags) |
| :--- | :--- |
| `list` | unknown flags only |
| `search`, `show` | `--full`, `--force` |
| `copy-to`, `store`, `daemon` | `--raw`, `--full`, `--force` |
| `delete`, `pin`, `unpin` | `--raw`, `--full`, `--force`, `--verbose` |
| `wipe` | `--raw`, `--full`, `--verbose`; `--force` is **required** |
| `paste-from` | `--raw`, `--full`, `--force`, `--verbose`, `--id`; accepts `-l` / `--list-types`, which is an error together with a MIME argument |
| `status`, `pause`, `resume`, `stats`, `doctor` | every flag, including `--id` and `--verbose` |

`-l` / `--list-types` is deliberately parsed inside `paste_from.rs` rather than added to the shared `ArgContext`: the global parser collects it in `unknown_flags`, and only `paste-from` opts in. Every other subcommand therefore keeps rejecting it exactly as before.

```rust
// crates/cli/src/pin.rs:L24
if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force || ctx.verbose {
    eprintln!("{}command '{}' does not support specified options.", LOG_ERROR, cmd_name);
    return;
}
```

> **Invariant — deterministic by construction.** A command either runs with exactly the arguments it was given a defined meaning for, or refuses to run at all, and says so on stderr. `wipe` never prompts: without `--force` it refuses and exits non-zero, which keeps it script-safe.

For an interactive user this is a slightly less forgiving CLI. For a script, the primary audience, it is the difference between a typo that fails fast and obviously, and one that fails silently weeks later as a gap in the clipboard history nobody can explain.

---

## 6. Process Boundary: Read Directly, Act Through IPC

| Command | Path |
| :--- | :--- |
| `list`, `search`, `show`, `pin`, `unpin`, `store` (insert), `stats` | Direct `ClipboardDb` access from the CLI process |
| `copy-to`, `store` (sync) | Promote/insert in the database, then send `IPC_CMD_RESTORE` + decimal ID + `\n` to the daemon socket |
| `status`, `pause`, `resume` | One-shot IPC command (`0x03`, `0x04`, `0x05`) |
| `delete`, `wipe` | IPC mutation (`IPC_CMD_DELETE` `0x06` + decimal ID, `IPC_CMD_WIPE` `0x07`), answered on the same connection. **Only when `UnixStream::connect` fails** does the CLI fall back to its own `ClipboardDb`; once connected, a timeout or an `ERR:` reply is an error and the command exits non-zero, never a second attempt against the file ([03](03_concurrency_and_memory.md) section 2.2) |
| `daemon`, `paste-from`, `doctor` | `y4p-cli` only validates and returns a `CliAction` (`RunDaemon`, `PasteFrom` / `ListTypes`, `Doctor`); the composition root (`src/main.rs`) performs the actual daemon / Wayland call, so `y4p-cli` depends on neither crate |

The socket lives at `$XDG_RUNTIME_DIR/y4p/y4p.sock`, falling back to `/tmp/y4p.<uid>.sock` only when no session runtime directory exists ([`crates/core/src/lib.rs`](../crates/core/src/lib.rs)).

---

## 7. Observability: `stats` and `doctor` (v0.8.8)

Both commands exist so a user (or a bug report) can answer "what is in here?" and "why is it not working?" without reading the source. They are ordinary CLI commands: `y4p-cli` holds all of their logic, and the one check that needs a compositor connection is delegated to the composition root through `CliAction::Doctor`.

### 7.1 `y4p stats`

Read-only, takes no arguments, and exits `0` for any readable store — a stopped daemon is a normal state, not an error. Only a rejected argument (a flag or a positional) makes it exit `1`. Three sections:

| Section | Content | Source |
| :--- | :--- | :--- |
| Entries | total / pinned / unpinned counts, total logical payload bytes, and a per-category breakdown: `image/*` first, then anything `core::utils::is_text_like_mime` accepts, then everything else, each with its own count and bytes | `ClipboardDb::fetch_metadata(usize::MAX)`: metadata only, no payload is read |
| On-disk | database file size, cache file count and bytes, and the (now unused) spool directory's count and bytes | `core::get_db_path()`, `core::get_cache_dir()`, `core::spool_dir_path()` |
| Daemon | `uptime_ms`, `paused`, `ingress`, `egress`, `shed_readers`, `shed_queue`, `shed_timeout` | an IPC `status` query; the line is only shown when every field parsed, and a stopped daemon prints `daemon: not running (shed counts unavailable)` |

The categories are a classification of what is stored, not a filter: they always sum to the total. `stats` never mutates anything and never opens a write transaction.

### 7.2 `y4p doctor`

Prints exactly one line per check, tagged `[PASS]`, `[WARN]` or `[FAIL]`, then a tally. The exit status is `1` if **any** check failed and `0` otherwise — warnings alone never fail the run, because a fresh profile legitimately has no socket, no daemon and no spool directory.

| Group | Checks | Severity policy |
| :--- | :--- | :--- |
| Environment | `XDG_RUNTIME_DIR`, `XDG_DATA_HOME` (or `HOME`), `WAYLAND_DISPLAY`, `Config::get_config_path()` / `Config::load()` (effective `max_history`, `drop_rtf` and `sanitize`), `magick -version` | Only a missing *data* home (neither variable set) is a `[FAIL]`; `magick` absent or broken is a `[WARN]` only when `[image] sanitize = true`, and a `[PASS]` (`not required; [image] sanitize = false`) otherwise |
| Compositor | the Wayland handshake itself (`wayland::create_connection()`) | `[FAIL]` when unreachable: without it the daemon cannot start |
| Database | `core::get_db_path()` exists and is a regular file, its size, and `db.get_total_count()` | `[FAIL]` if the file cannot be read |
| Socket & daemon | socket path, `<socket>.lock`, the parent directory's mode (`0700` expected), and an IPC `status` round trip | Absent socket/lock or a stopped daemon is a `[WARN]`; a wrong directory mode is a `[WARN]`; an unreadable parent is a `[FAIL]` |
| Quota & directories | `db.get_total_count()` against `core::get_max_history()`, the `MAX_PAYLOAD_BYTES` ceiling (32 MiB), the `CACHE_QUOTA_BYTES` ceiling (512 MiB), and that the cache and spool directories exist and are writable | Exceeding `max_history` is a `[WARN]` (rotation catches up); an absent directory is normal (`created on first use`); an unwritable one is a `[FAIL]`, tested by creating and removing a probe file |

If the database cannot even be opened, `main` reports that one line as a `[FAIL] database: ...` and exits `1`, rather than the bare `critical:` line every other command would print: the command whose job is to explain a broken setup should not be the one that dies without explaining it.

### 7.3 `y4p paste-from [-l|--list-types | mime]`

`paste-from` is the observation counterpart to daemon ingestion. With a MIME it reads the current selection synchronously and writes the bytes to stdout, exactly as the daemon would have stored them (before optional image routing). With `-l` / `--list-types` it opens no pipe at all: the Wayland Action Mode records `OfferData.mimes` and `src/main.rs` prints one MIME per line, in compositor order. An empty result (no compositor, no selection, a sensitive selection, or the one-second timeout) exits `1`; a payload fetch that returns nothing does the same.
