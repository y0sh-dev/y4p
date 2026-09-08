# Stable IDs & the Strict CLI

`y4p` is designed to sit inside other people's pipelines — `fzf`, `rofi`, `awk`, shell scripts that select an entry and act on it. That audience changes what "correct" means for a CLI: a script can't ask a clarifying question when the input is ambiguous, and it can't tell the difference between "the tool did what I meant" and "the tool guessed wrong" unless the tool is willing to fail loudly instead of guessing at all.

---

## Why display order and identity are two different numbers

`y4p list` shows entries most-recent-first — that's the useful order for a human scanning history. But "useful order for display" and "identity" want to be two different things, because the *order* is not stable: copying something new, or restoring an older entry (which bumps its timestamp via MRU promotion), shifts every index below it. A script that captured "entry #3" a moment ago and acts on "#3" a moment later can end up acting on a completely different piece of clipboard content than the one it saw — a silent misfire, not a crash, which is the worse kind of bug for something a script relies on unattended.

`y4p` gives every record a second, immutable identifier: the SQLite `id` (`AUTOINCREMENT`), exposed via `--id`/`-i`. The MRU position (what you see without `--id`) and the stable ID (what you get with it) are computed from the same query so they're always consistent with each other:

```sql
-- src/storage/db.rs — search_metadata; fetch_metadata's plain ORDER BY is the same ranking
SELECT id, timestamp, mime, size, preview, is_pinned,
       ROW_NUMBER() OVER (ORDER BY timestamp DESC) - 1 AS abs_idx
FROM clipboard
```

`search`'s bugfix history is the concrete case this matters for: `search` used to number its hits `0, 1, 2, ...` by local position *within the search results* — a completely different index space from `list`'s absolute history position. Piping a `search` hit's displayed index into `copy-to` (without `--id`) could silently restore the wrong entry, because index `1` meant "second search hit" in one command and "second-most-recent history entry" in the other. Computing the same `ROW_NUMBER() OVER (ORDER BY timestamp DESC)` in both `search_metadata` and `fetch_metadata` — over the *entire* table before filtering — closed that gap: an index shown by `search` now means exactly what the same index means in `list`, and either can be fed straight into `copy-to`/`delete`/`pin` with no `--id` if the MRU position is genuinely what's wanted, or with `--id` when the script captured a stable ID up front and needs a guarantee that clipboard activity in between can't change what it points to.

```
  list (MRU order)          search "rust" (same abs_idx space)
  ┌────┬────────────┐       ┌────┬────────────┐
  │ 0  │ id=42 (new)│       │ abs│            │
  │ 1  │ id=41      │──┐    │ 1  │ id=41      │  ◀── same index, same record,
  │ 2  │ id=40      │  │    └────┴────────────┘      whichever command showed it
  │ 3  │ id=39      │  └───────────────────────▶  copy-to 1  /  copy-to --id 41
  └────┴────────────┘
```

## Why unknown flags are a hard error, not a best guess

Most CLI parsers, faced with a flag they don't recognize, do one of two things: silently ignore it, or treat it as a positional argument. Both are dangerous for a tool where the positional argument is often "which history entry to permanently delete." An unrecognized flag being swallowed instead of rejected means a typo (`y4p delete --Id 5` instead of `--id 5`) doesn't fail — it just quietly does something other than what was intended, on a command whose entire purpose is destructive.

`ArgContext::parse` (`cli/utils.rs`) takes the opposite stance: every flag the CLI understands is enumerated explicitly, and anything else — long or short form — is collected into `unknown_flags` rather than dropped:

```rust
// src/cli/utils.rs
match arg.as_str() {
    "--raw" => ctx.raw = true,
    "--full" => ctx.full = true,
    // ... every recognized flag, explicitly ...
    _ => ctx.unknown_flags.push(arg.clone()),
}
```

Each subcommand then declares which of the *globally* parsed flags actually apply to it and rejects the rest — `pin`/`unpin`, for instance, reject `--raw`/`--full`/`--force`/`--verbose` even though `ArgContext` parses them without complaint, because those flags mean something for `list` or `wipe` but nothing for pinning a record:

```rust
// src/cli/pin.rs
if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force || ctx.verbose {
    eprintln!("command '{}' does not support specified options.", cmd_name);
    return;
}
```

The result is deterministic by construction: a command either runs with exactly the arguments it was given a defined meaning for, or it refuses to run at all and says so on stderr. For an interactive user that's a slightly less forgiving CLI. For a script — which is the primary audience this design targets — it's the difference between a typo that fails fast in an obvious way and a typo that fails silently, weeks later, as a gap in the clipboard history nobody can explain.
