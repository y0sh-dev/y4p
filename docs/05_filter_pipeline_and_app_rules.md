# 05 — Filter Pipeline & Dynamic Egress

A clipboard manager cannot simply be a passive byte sink. Modern desktops hand it payloads in arbitrary formats, bloated by redundant rich-text duplicates. Sensitive applications (password managers) demand exclusion, while command-line utilities and API debuggers require raw, unmodified input.

This document explains how `y4p` filters and transforms clipboard data without stalling the Wayland event loop and without any network communication: a clipboard manager stores and returns what the compositor gave it, nothing more, apart from the explicit, opt-in, bounded image hardening of [06](06_local_image_pipeline.md).

Crates involved: `y4p-wayland` (stages 1-5, egress), `y4p-daemon` (stages 6-8), `y4p-core` (constants, MIME helpers, `y4p.toml` parser).

---

## 1. The Multi-Stage Ingress Pipeline

When a Wayland application sets a new selection, the compositor emits an `offer` carrying the list of available MIME types. `y4p` processes it through a strictly ordered pipeline:

```text
 Compositor Offer (MIME list)
             |
             v
 [Stage 1: Gate]  --------------------> sensitive hint? paused?                  -> discard silently
             |                          reader limit (8) reached?                 -> shed, counted
             |                          list-types Action Mode?                    -> record MIMEs, no pipe
             |                          (all checked before any pipe is opened)
             v
 [Stage 2: MIME Allowlist]              strict: no catch-all
             |-- drop RTF in the text step if [mime] drop_rtf is enabled
             |-- candidates: MIME_PRIORITY_ORDER -> image/* -> text/*
             v
 [Stage 3: Payload Acquisition]         crates/wayland
             \-- bounded read: DeadlineReader + read_to_end, capped at MAX_PAYLOAD_BYTES (32 MiB), +1 probe byte
             |
             v
 [Stage 4: Normalisation]
             \-- uri-list: percent-decode, strip file://   |   mislabelled image: re-identify by magic bytes
             |
             v
 [Stage 5: Forward]
             \-- bounded SyncSender<ClipboardJob { mime, payload }>  (queue full for 2 s -> shed, counted)
  ===========================================================  crate boundary: wayland | daemon
             |
             v
 [Stage 6: Image Routing (opt-in)]      crates/daemon (DbWorker thread)
             \-- sanitize = true: raster images privacy-stripped + re-encoded via y4p-image;
                 sanitize = false (default): bytes pass through untouched
             |
             v
 [Stage 7: SHA3-256 Fingerprint]
             \-- over the FINAL bytes
             |
             v
 [Stage 8: Persist]
             \-- ClipboardDb::insert_with_hash  ->  SQLite (WAL) + cache directory (+ LRU quota)
```

> **Action Mode `paste-from -l`** stops at stage 1: the Selection handler records the offer's advertised MIME types and returns before candidate selection, a reader slot or a pipe ([01](01_wayland_streaming_io.md) section 5).

### 1.1 Stage 1: the gate

| Check | Source | Effect |
| :--- | :--- | :--- |
| `paused` | IPC `pause` (private mode) | return; no `offer.receive`, no job |
| `is_sensitive(mimes)` | `core::utils::is_sensitive_mime` | return |
| empty offer list | | return |
| `list_types` Action Mode | `paste-from -l` | copy the MIME list into `offered_mimes` and return before any reader slot or pipe |

A restore of the daemon's own does **not** appear here: since v0.8.7 there is no `provider_locks` counter, and a re-offer of what was just served is deduplicated by content hash in storage ([01](01_wayland_streaming_io.md) section 3).

`is_sensitive` now delegates to `core::utils::is_sensitive_mime(mime)`, which lowercases **both** the MIME and the hint before the substring test. Hints mix case (`x-kde-passwordManagerHint`); lowercasing only the haystack would make that hint impossible to match, silently defeating the KDE password-manager filter it exists for. The hint list covers KDE and GNOME markers plus `keepass`, `1password`, `bitwarden`, `concealed`, `password`, `secret`.

> **Invariant — one sensitive test, two layers.** The storage backstop is the *same* function: `is_skipped` in [`crates/storage/src/db.rs`](../crates/storage/src/db.rs) is `len == 0 || crate::core::utils::is_sensitive_mime(mime)`, so a MIME the Wayland layer lets through cannot be classified differently once it reaches `upsert_record`. That matters for paths that never pass through a selection at all (`y4p store`, `y4p copy-to`), which the storage check covers on its own. A manually stored `X-KDE-PASSWORDMANAGERHINT` payload is skipped (`upsert_record` returns `-1`) just like an ingested one.

### 1.2 Stage 2: MIME selection

`select_mime_candidates` ([`crates/wayland/src/handlers/data_control/device.rs`](../crates/wayland/src/handlers/data_control/device.rs)) returns an ordered candidate array, not a single choice; the fallback loop that walks it is described in [01](01_wayland_streaming_io.md) section 2.4:

| Mode | `target_mime` | Rule |
| :--- | :--- | :--- |
| Action Mode (`y4p paste-from <mime>`) | `Some` | Every offer matching by `mime_base_eq` (case, whitespace and parameters ignored). `drop_rtf` is **not** consulted: an explicit request is honoured, RTF included |
| Daemon ingestion | `None` | The **strict allowlist**, in this order: every match in `MIME_PRIORITY_ORDER` (in that order), then the remaining `image/*`, then the remaining `text/*` (`drop_rtf` removes RTF from this last step). Offers are deduplicated by base type |

`MIME_PRIORITY_ORDER` ([`crates/core/src/constants.rs`](../crates/core/src/constants.rs)) ranks images first (PNG, WebP, JPEG, GIF, SVG, AVIF, BMP), then `text/uri-list`, then plain text and the legacy string atoms, and rich markup (`text/html`, XHTML, Markdown, JSON, XML) last. `text/rtf` deliberately has no entry.

> **Invariant — there is no catch-all (v0.8.6).** An offer outside `MIME_PRIORITY_ORDER`, `image/*` and `text/*` — `application/octet-stream`, `application/x-*`, `TARGETS`, an untyped blob — never becomes a candidate: it is neither requested nor stored. Until v0.8.6 the chain ended in "else anything", which meant every opaque binary a sender announced was read and persisted as a history entry of unknown meaning. The strict list is also why an app that offers only a blob is now silently ignored rather than half-supported.

### 1.3 Why the order matters

1. **Discard before allocation.** Sensitive hints are checked before opening pipes or allocating buffers. An offer from a password manager is dropped at zero I/O cost.
2. **Plain text outranks rich markup, and an empty attempt costs only one candidate.** Electron and Chromium applications routinely announce `text/html` next to `text/plain` but synthesise HTML only on request; a non-rich selection returns an empty transfer for `text/html`. Trying plain text first makes that failure structurally impossible, while `text/html` stays reachable when no plain-text alternative exists. Since v0.8.6 a 0-byte transfer is not the end of the selection either: the fallback loop moves to the next candidate ([01](01_wayland_streaming_io.md) section 2.4).
3. **Hash after every transformation.** Normalisation and image routing may change bytes; the fingerprint is computed last so the persisted hash always matches the persisted bytes.

> **Invariant — what may be modified before storage.** Text and markup are stored byte for byte, with exactly one lossless exception: `text/uri-list` is decoded to plain paths. No repair, no rewriting, and no markup stripping: a `text/html` offer is persisted verbatim. `strip_html_tags` is reserved for preview generation and plain-text egress, never applied at ingress. **Raster images are rewritten only when `[image] sanitize = true`**, by the sandboxed, bounded, metadata-stripping pipeline of [06](06_local_image_pipeline.md); by default (and for GIF, SVG, AVIF and non-image types in either mode) the bytes pass through untouched. When sanitisation is enabled, PNG, BMP, JPEG and WebP leave as `image/png`.

---

## 2. Dynamic Egress

`y4p` never fetches data over the network, and egress never transcodes: whatever bytes were stored are served back unmodified. Images are stored in `~/.cache/y4p/<hash>.cache` and served via `sendfile(2)`, a zero-copy kernel-side transfer that never routes the payload through this process's heap:

```text
 Target app requests a MIME type
              |
              v
   mime_is_compatible(requested, stored)?  --no-->  fd closed, nothing sent
              | yes
              v
   File(path)  -> sendfile(2): cache file -> destination pipe
   Memory(data) -> write_all on a spawned thread
                  (HTML stored, plain text requested -> strip_html_tags first)
```

A bounded userspace copy is the only fallback, used only if `sendfile(2)` reports `EINVAL`/`ENOSYS` for that destination (or `ESPIPE` for a source that cannot be addressed by offset). It keeps a `copy-to` of a large payload from silently producing empty or partial content on an exotic kernel or target.

### 2.1 MIME compatibility (`mime_is_compatible`)

Rules are applied **symmetrically**: `compatible(a, b) == compatible(b, a)`. An earlier asymmetric rule made `text/plain` match `application/xhtml+xml` in one direction only.

| Rule | Example |
| :--- | :--- |
| Same base type (case, whitespace, parameters ignored) | `TEXT/PLAIN; charset=UTF-8` ~ `text/plain` |
| Both in the `text/*` category | `text/plain` ~ `text/markdown` |
| Both HTML-family | `text/html` ~ `application/xhtml+xml` |
| A legacy alias (`UTF8_STRING`, `STRING`, `TEXT`, `COMPOUND_TEXT`, `text/plain`) against any text-compatible type | `UTF8_STRING` ~ `text/html` |
| Images | **exact base type only**: `image/png` never matches `image/jpeg` or `image/webp` |

> **Invariant — no cross-format image matching.** Without transcoding at egress, `y4p` cannot turn one encoding into another on request. Cross-matching would let the `Send` handler believe it can satisfy a request it will never fulfil, sending the wrong bytes or crashing a strict decoder, instead of failing cleanly.

On restore, alternates are advertised next to the stored MIME (`HTML_MIME_ALTS` for markup, `TEXT_MIME_ALTS` for other text) so the paste target can pick whichever it understands.

---

## 3. Zero-Dependency Configuration (`y4p.toml`)

### 3.1 A forgiving, standard-library TOML parser

To honour the project's no-new-crates invariant, `y4p` avoids parsing frameworks (`serde`, `toml`). `core::config::Config::parse` ([`crates/core/src/config.rs:L70`](../crates/core/src/config.rs)) is a small line-oriented parser built on standard-library slice operations.

| Aspect | Behaviour |
| :--- | :--- |
| Grammar | `[section]` headers, `key = value`, `#` comments (a `#` inside a quoted string is not a comment; no `\"` escapes) |
| Value types | unsigned integers and booleans only |
| Recognised keys | `[general] max_history` (positive integer), `[mime] drop_rtf` (`true`/`false`), `[image] sanitize` (`true`/`false`) |
| Anything else | unknown section, unknown key, malformed line, or a value that fails its type: **skipped silently**, leaving the default |

> **Invariant — a config problem never stops the daemon.** A missing, unreadable, non-UTF-8 or syntactically broken file yields `Config::default()`. A malformed file can never crash the daemon or prevent it from monitoring the clipboard.

### 3.2 Precedence

```text
 1. Environment variable           Y4P_MAX_HISTORY  (positive integer; applies to max_history only)
                  |
                  v
 2. User configuration file        $XDG_CONFIG_HOME/y4p/y4p.toml   (falls back to ~/.config/y4p/y4p.toml)
                  |
                  v
 3. Built-in defaults              DEFAULT_MAX_HISTORY = 256, drop_rtf = true, sanitize = false
```

`core::get_max_history()` ([`crates/core/src/lib.rs:L27`](../crates/core/src/lib.rs)) implements this chain. There is no command-line override. The result is full operation in minimal containers where no configuration file exists, while desktop users keep control over retention and MIME handling. A documented template ships as [`y4p.toml.example`](../y4p.toml.example).
