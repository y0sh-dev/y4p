# Stable IDs & the Strict CLI

`y4p` is designed to sit inside other people's pipelines.

`fzf`, `rofi`, `awk`, shell scripts that select an entry and act on it. That audience changes what "correct" means for a CLI. A script can't ask a clarifying question when the input is ambiguous. And it can't tell "the tool did what I meant" apart from "the tool guessed wrong," unless the tool is willing to fail loudly instead of guessing at all.

---

## Why display order and identity are two different numbers

`y4p list` shows entries most-recent-first. That's the useful order for a human scanning history.

But "useful order for display" and "identity" want to be two different things — because the *order* isn't stable. Copying something new, or restoring an older entry (which bumps its timestamp via MRU promotion), shifts every index below it.

A script that captured "entry #3" a moment ago, and acts on "#3" a moment later, can end up acting on completely different clipboard content than the one it originally saw. A silent misfire, not a crash. Which is the worse kind of bug, for something a script relies on unattended.

`y4p` gives every record a second, immutable identifier: the SQLite `id` (`AUTOINCREMENT`), exposed via `--id`/`-i`.

The MRU position — what you see without `--id` — and the stable ID — what you get with it — are computed so they always agree with each other. `fetch_metadata` (backing plain `list`) derives a row's position from its rank in a simple `ORDER BY timestamp DESC`. `search_metadata` derives the exact same number for a search hit, via a per-row correlated count:

```sql
-- src/storage/db.rs — search_metadata
SELECT (SELECT COUNT(*) FROM clipboard c2 WHERE c2.timestamp > c1.timestamp) AS abs_idx,
       id, timestamp, mime, size, preview, is_pinned
FROM clipboard c1
WHERE (mime LIKE '%text%' OR mime LIKE '%UTF8%') AND (preview LIKE ? OR content LIKE ?)
ORDER BY timestamp DESC
```

`search`'s bugfix history is the concrete case this mattered for.

`search` used to number its hits `0, 1, 2, ...` by local position — *within the search results*. A completely different index space from `list`'s absolute history position. Piping a `search` hit's displayed index into `copy-to`, without `--id`, could silently restore the wrong entry. Index `1` meant "second search hit" in one command, and "second-most-recent history entry" in the other.

Computing each hit's *absolute* position — "how many rows are newer than this one" — instead of its position among just the search results, closed that gap. An index shown by `search` now means exactly what the same index means in `list`.

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

Either form works, once the index spaces agree. `copy-to 1` if the MRU position is genuinely what's wanted. `copy-to --id 41` when a script captured a stable ID earlier, and needs a guarantee that clipboard activity in between can't change what it points to.

---

## Why the window function was eliminated, not just optimized

Keeping `search`'s index space aligned with `list`'s used to mean literally sharing the same SQL idiom: both queries computed `ROW_NUMBER() OVER (ORDER BY timestamp DESC) - 1` across the whole table, then filtered.

That word "across the whole table, then filtered" is the part that stopped scaling. A window function like `ROW_NUMBER()` can't be evaluated lazily, one row at a time as they're found — SQLite has to gather every row the `ORDER BY` applies to, sort it in full, and only then hand out sequence numbers, before `search`'s keyword filter ever gets a chance to discard the rows that don't match. Concretely, this meant building a temporary sorted structure (SQLite calls it a Temp B-Tree) sized to your *entire* clipboard history — not to your search results — for every single `search` invocation, no matter how narrow the keyword.

At a few hundred rows, a Temp B-Tree materializes fast enough to be invisible. Past roughly 50,000 rows of history, users doing a keyword search that matched only a handful of entries were still paying tens of milliseconds building and sorting a structure representing every row they'd ever copied.

The fix already lives in the section above: a correlated `(SELECT COUNT(*) FROM clipboard c2 WHERE c2.timestamp > c1.timestamp)` per candidate row, instead of one window-function pass over everything. Because a correlated subquery is evaluated per-row rather than as a separate whole-table pass, SQLite's query planner is free to run the mime/keyword filter *first* — using the existing `idx_ts`/`idx_pinned_ts` indexes — and only pay the `COUNT(*)` cost for rows that survive the filter.

```text
   Before: ROW_NUMBER() window function      After: correlated COUNT(*)

   1. gather + sort ALL rows                 1. filter by mime + keyword
      (Temp B-Tree ~ full history size)          first (index-assisted)
   2. THEN apply mime/keyword filter          2. for each surviving row,
      to the numbered results                    COUNT(*) newer rows
                                                  (cost ~ match count, not
                                                   total history size)
```

The tradeoff is a small, deliberately accepted one: two rows with the exact same millisecond-resolution `timestamp` would have received distinct (if arbitrary) numbers from `ROW_NUMBER()`, but now collapse to an identical `abs_idx`. In practice this table's timestamps are granular enough, and clipboard events rare enough, that a genuine collision is not a case anyone has hit — and the index this produces was only ever a display/lookup convenience, never a uniqueness guarantee the schema relies on elsewhere.

---

## Why `--raw` output is an unbreakable contract, not just a flag

Most of `y4p`'s output is for a person reading a terminal. `--raw` output is not.

`y4p list --raw` and `y4p search --raw` exist specifically so `y4p-rofi`, `fzf` pipelines, and hand-written shell scripts have something stable to parse — `[ID] [LABEL] PREVIEW`, one record per line, with nothing decorative mixed in. The moment that format is treated as "just how we currently print things," it becomes a moving target external tooling has already built assumptions on top of.

That's the distinction worth naming explicitly: `--raw`'s format isn't an implementation detail that happens to be visible. It's a published interface, in the same sense a function's public signature is — except unlike a Rust signature, nothing at compile time stops someone from reflowing a column, adding a helpful extra space, or reordering fields "for readability." Any of those looks like a harmless cosmetic tweak from inside the codebase, and is a breaking change to every external script that has ever run `awk '{print $1}'` against this output.

```text
   y4p list --raw
        |
        v
   42  text/plain  "curl example.com | jq ..."
   41  image/png   [gif]
   40  text/plain  "git commit -m ..."
        |
        +--> fzf  --> awk '{print $1}'  --> xargs y4p copy-to --id
                        ^
                        assumes field 1 is always, and only, the ID —
                        forever, across every y4p version
```

There is no compiler enforcing this contract the way Rust enforces a function's types. The discipline is entirely human: any change touching `--raw`'s field order, delimiter, or spacing is a breaking change to the ecosystem around `y4p`, full stop, and has to be reasoned about at that level — not as a cosmetic diff to a `println!` call.

---

## Why a negative index is rejected outright, not clamped or reinterpreted

`resolve_target_id` (`cli/utils.rs`) is the single function every index-accepting subcommand — `pin`, `unpin`, `show`, `delete`, `copy-to` — routes its positional argument through.

It parses that argument as `i64`, not `usize`, and that specific choice is what makes rejection possible at all. Parsing straight to `usize` — which is what an index "should" be, semantically — means a value like `-1` doesn't fail to parse. Rust's own numeric conversion rules would silently reinterpret it as `usize::MAX` (`18446744073709551615`), a colossal, syntactically-valid-looking index that then goes on to fail somewhere else, far from where the actual mistake was made, with an error message that no longer mentions the `-1` a user or a script actually typed.

```rust
// src/cli/utils.rs — resolve_target_id
let val = input.parse::<i64>().map_err(|_| format!("invalid numerical value: '{}'", input))?;
if val < 0 {
    return Err(format!("index cannot be negative: {}", val));
}
```

Parsing as `i64` first keeps the sign information intact long enough to check it deliberately, before any cast to `usize` — the type every downstream `Vec` index actually needs — ever happens. A negative value is rejected at the door, with an error that names the exact bad input, rather than surfacing later as an inexplicable "index out of bounds" against a history vector that was never anywhere near `usize::MAX` entries long.

This is the same underlying philosophy as the unknown-flag handling below: `y4p` is built to be driven by scripts as often as by hands on a keyboard, and a script cannot ask a clarifying question. Given an ambiguous or malformed instruction, guessing what was probably meant is worse than refusing outright — a refusal fails loudly, at the exact place the bad value entered the system, which is the only place anyone can actually act on it.

---

## Why unknown flags are a hard error, not a best guess

Most CLI parsers, faced with a flag they don't recognize, do one of two things. Silently ignore it. Or treat it as a positional argument.

Both are dangerous here, because the positional argument is often "which history entry to permanently delete." An unrecognized flag being swallowed, instead of rejected, means a typo — `y4p delete --Id 5` instead of `--id 5` — doesn't fail. It just quietly does something other than what was intended, on a command whose entire purpose is destructive.

`ArgContext::parse` (`cli/utils.rs`) takes the opposite stance.

Every flag the CLI understands is enumerated explicitly. Anything else — long or short form — gets collected into `unknown_flags`, rather than dropped on the floor:

```rust
// src/cli/utils.rs
match arg.as_str() {
    "--raw" => ctx.raw = true,
    "--full" => ctx.full = true,
    // ... every recognized flag, explicitly ...
    _ => ctx.unknown_flags.push(arg.clone()),
}
```

Each subcommand then declares which of the *globally* parsed flags actually apply to it, and rejects the rest.

`pin` and `unpin`, for instance, reject `--raw`, `--full`, `--force`, and `--verbose` — even though `ArgContext` parses all four without complaint. Those flags mean something for `list` or `wipe`. They mean nothing for pinning a record:

```rust
// src/cli/pin.rs
if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force || ctx.verbose {
    eprintln!("command '{}' does not support specified options.", cmd_name);
    return;
}
```

The result is deterministic by construction. A command either runs with exactly the arguments it was given a defined meaning for, or it refuses to run at all — and says so, on stderr.

For an interactive user, that's a slightly less forgiving CLI.

For a script — which is the primary audience this design targets — it's the difference between a typo that fails fast and obviously, and a typo that fails silently, weeks later, as a gap in the clipboard history nobody can explain.
