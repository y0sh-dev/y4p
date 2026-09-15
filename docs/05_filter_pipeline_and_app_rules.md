# Filter Pipeline, Dynamic Egress, & Application Rules

A clipboard manager cannot simply be a passive byte sink.

In modern desktop environments, clipboard payloads arrive in arbitrary formats, cluttered with tracking parameters, bloated by redundant rich-text duplicates, or degraded by browser re-encoding. Furthermore, sensitive applications (such as password managers) demand exclusion, while command-line utilities and API debuggers require raw, unmodified input.

This document explores how `y4p` filters, sanitises, and transforms clipboard data without introducing external dependencies, compromising memory safety, or stalling the Wayland event loop.

---

## 1. The Multi-Stage Ingress Pipeline

When a Wayland application sets a new clipboard selection, the compositor emits an `offer` event containing the list of available MIME types. `y4p` processes this offer through a strictly ordered, multi-stage pipeline:

```text
 Compositor Offer (MIME list)
             |
             v
 [Stage 1: Sensitive MIME Gate]  ---> Contains password hints? ---> Discard silently
             |
             v
 [Stage 2: Compositor App ID Resolution] (Out-of-band IPC: Sway / Hyprland)
             |
             +-------------------------> Matches [app.ignore]?  ---> Discard early
             |
             v
 [Stage 3: MIME Priority & Filtering]
             |-- Drop RTF if [mime.drop_rtf] enabled
             |-- Select richest format (MIME_PRIORITY_ORDER)
             |
             v
 [Stage 4: Payload Acquisition & Image Hijacking]
             |-- Image + HTML offer? Fetch original via curl (if enabled)
             \-- Standard Pipe Transfer: 64 KiB page-aligned streaming
             |
             v
 [Stage 5: Normalisation & Sanitisation]
             |-- URI-List: normalise percent-encoding & file:// schemes
             |-- Image: repair missing EOF/trailer markers (JPEG/PNG/GIF)
             |-- HTML Downgrade: strip markup if text/plain missing
             \-- Text Sanitisation: strip tracking query params (unless bypassed)
             |
             v
 [Stage 6: SHA3-256 Fingerprint & Worker Dispatch]
             \---> mpsc::Sender<ClipboardJob> ---> DbWorker (SQLite WAL)
```

### Why order matters

1. **Discard before allocation**: Checking sensitive hints and application exclusion rules occurs before opening pipes or allocating memory buffers. If an offer originates from a password manager or an excluded application, `y4p` drops it immediately at zero I/O cost.
2. **MIME priority over raw offers**: Electron and Chromium applications routinely offer `text/html` alongside `text/plain`, but only synthesise HTML on demand. Requesting `text/html` first can yield an empty transfer. `y4p` prioritises high-fidelity images, standard text, and structured fallbacks in a deterministic sequence (`MIME_PRIORITY_ORDER`).
3. **Normalisation before hashing**: Fingerprinting (SHA3-256) happens *after* payload repair and URL tracking removal. This guarantees that duplicate copies of the same link—whether copied with different tracking tokens or trailing whitespace—collapse into a single stable history record.

---

## 2. Image Hijacking & Dynamic Egress Transcoding

### The Browser Re-encoding Dilemma

When a user executes "Copy Image" in a web browser, the browser rarely places the original image asset onto the clipboard. Instead, it decodes the remote WebP, AVIF, or JPEG into an uncompressed bitmap, re-encodes it into an unoptimised PNG, and writes that PNG to the Wayland selection pipe.

This introduces two severe penalties:
- **Loss of fidelity & bloat**: A 200 KiB WebP image can balloon into a 15 MiB uncompressed PNG clipboard transfer.
- **Lost source metadata**: Animated GIFs or vectors lose their original format characteristics.

However, browsers almost universally advertise a companion `text/html` MIME type containing an `<img>` tag pointing directly to the source URL:

```html
<!-- Browser clipboard HTML payload -->
<meta http-equiv="content-type" content="text/html; charset=utf-8">
<img src="https://example.com/assets/original_illustration.webp" alt="...">
```

### The Image Hijacker (`[image] hijack_original`)

When `[image] hijack_original = true` is configured, `y4p` intercepts this pattern:

1. **Asynchronous inspection**: The daemon spawns a dedicated worker thread, keeping the main `libc::poll` loop completely non-blocking.
2. **Tag extraction**: A zero-dependency byte scanner extracts the target URL from the `src` attribute of the first `<img>` tag.
3. **Strict URL validation**: Only explicit `http://` and `https://` schemes are accepted. Local `file://`, relative paths, and embedded `data:` URIs are rejected outright.
4. **Bounded fetch**: The thread invokes `curl` with rigorous defensive limits:
   - `--max-time 5`: Prevents slow-loris server attacks from tying up threads.
   - `--max-filesize 104857600` (100 MiB): Guards against memory exhaustion.
   - `--proto =http,https`: Disables dangerous protocol redirects (e.g. `gopher://`, `file://`).
5. **Safe fallback**: If the network request fails, times out, or returns a non-image content type, the worker seamlessly falls back to requesting the standard image pipe from the Wayland offer.

### Dynamic Egress Transcoding

Rather than transcoding images upon ingestion—which burns CPU cycles and alters original files—`y4p` stores the ingested binary payload *exactly as received* in its deduplicated filesystem cache (`~/.cache/y4p/`).

Transcoding is deferred entirely to egress (paste time):

```text
 Target App requests 'image/png'
              |
              v
 Is stored asset already PNG?
        /           \
     (Yes)          (No: JPEG, WebP, GIF)
      /               \
 sendfile(2)     Invoke 'convert' / 'magick' (stdout pipe)
  zero-copy           |
                      v
                 Stream transcoded PNG to target FD
```

- **Native consumers**: If an application requests `image/webp` for a stored WebP image, `y4p` transfers it directly using the Linux `sendfile(2)` system call, bypassing user-space memory entirely.
- **Legacy consumers**: If an application requests `image/png` for a non-PNG image, `y4p` dynamically spawns ImageMagick (`convert` or `magick`), streaming the transcoded PNG directly into the target's file descriptor. The on-disk cache remains bit-for-bit pristine.

---

## 3. URL Tracking Parameter Sanitisation

URLs copied from modern social networks and web applications are heavily contaminated with telemetry parameters (`utm_source`, `fbclid`, `gbraid`). 

Naively stripping parameters using regular expressions or third-party URL parsers carries substantial risks: URL parsers often normalise escapes unpredictably, while regex engines can exhibit catastrophic backtracking on maliciously crafted inputs.

`y4p` employs an allocation-conscious, deterministic single-pass scanner implemented purely in `core::utils`.

### Universal vs Domain-Specific Rules

Sanitisation enforces two distinct tiers of query filtering:

| Category | Keys / Prefixes | Behaviour |
| :--- | :--- | :--- |
| **Universal Prefixes** | `utm_*`, `ref_*` | Stripped across all domains and hosts. |
| **Universal Keys** | `fbclid`, `gclid`, `gbraid`, `wbraid`, `msclkid`, `igshid`, `mc_cid`, `mc_eid`, `si` | Stripped universally on any URL. |
| **Domain-Specific** | `s`, `t` on `x.com` / `twitter.com`<br>`ref` on `*.amazon.*` | Stripped only when host matches the target platform. |
| **Protected Exceptions** | `t` on `youtube.com` / `youtu.be` | **Preserved**. YouTube video timestamps (`?t=120s`) are never stripped. |

### The Boundary & Parenthesis Dilemma

A common failure mode in clipboard sanitisation occurs when URLs are copied from plain prose or Markdown:

1. **Sentence punctuation**: `Have you seen https://example.com?utm_source=news.`
   - A naive scanner swallows the trailing period `.` as part of the URL, corrupting the link.
2. **Balanced parentheses**: `https://en.wikipedia.org/wiki/Rust_(programming_language)`
   - Stripping trailing punctuation must not truncate the closing parenthesis `)` belonging to the URL.
3. **Nested parentheses in prose**: `(see https://example.com?id=42 for details)`
   - Here, the trailing `)` belongs to the surrounding sentence, not the URL.

`y4p` resolves this via an ASCII state machine that tracks parenthesis depth:

```rust
// core::utils::sanitize_text_payload
let mut open_parens: usize = 0;
// URL scanning tracks matched '(' and ')' pairs.
// Trailing punctuation characters ('.', ',', ';', ':', '!', '?')
// and unmatched closing parentheses ')' are peeled off and restored
// to the prose boundary outside the sanitized URL.
```

If the URL itself contains balanced internal parentheses (e.g. Wikipedia articles), `open_parens` tracks them; when the URL closes with a balanced `)`, it is preserved. If the URL was wrapped inside prose parentheses, the unbalanced trailing `)` is excluded from the URL and retained in the prose.

Invalid UTF-8 sequences are detected up-front and bypassed without alteration, guaranteeing zero panics.

---

## 4. Out-of-Band Compositor IPC ("The App ID Hack")

### The Wayland Protocol Boundary

The Wayland `ext-data-control-v1` protocol is intentionally designed without any concept of client identification. A clipboard manager receiving a selection offer receives MIME types and file descriptors, but the compositor strictly omits which window or application generated the data.

This design preserves client sandboxing, but poses a major challenge for clipboard managers: users cannot exclude password managers (like KeePassXC or Bitwarden) if those applications fail to set proprietary selection tags (like `x-kde-passwordManagerHint`).

### Ambient Window Discovery

To overcome this protocol limitation without patching compositors or breaking Wayland invariants, `y4p` queries the compositor's ambient control IPC out-of-band:

```text
 Ingress Selection Triggered
             |
             v
 Check $HYPRLAND_INSTANCE_SIGNATURE?
   |-- (Found) --> Connect to $XDG_RUNTIME_DIR/hypr/<sig>/.socket.sock
   |               Send 'j/activewindow' (JSON response)
   |               Extract "class" / "initialClass"
   |
   \-- (Not Found) --> Check $SWAYSOCK?
         |-- (Found) --> Connect to $SWAYSOCK (i3-ipc)
         |               Send IPC_GET_TREE (Type 4)
         |               Locate focused leaf node
         |               Extract "app_id" (native) or "window_properties.class" (XWayland)
         |
         \-- (Not Found) --> Resolve to None (graceful fallback)
```

### Fault Tolerance & Safety Invariants

1. **Non-blocking socket timeouts**: Every compositor socket connection enforces a strict 50ms read/write timeout (`SO_RCVTIMEO` / `SO_SNDTIMEO`). If a compositor is frozen, hung, or overloaded, the lookup aborts immediately and returns `None`.
2. **Buffer ceilings**: Replies are read into a bounded buffer capped at 1 MiB (`MAX_RESPONSE_BYTES`), preventing malicious or runaway compositor processes from causing unbounded heap growth.
3. **Zero external JSON dependencies**: Tree traversals and key extractions use a bespoke, panic-free byte scanner (`find_json_string_value`, `focused_node_span`) that respects JSON string escape sequences (`\"`) without allocating an AST.
4. **Advisory lifecycle (Zero Storage Footprint)**: The resolved `source_app` string exists solely in transient memory (`ClipboardJob.source_app`). It is checked against `[app.ignore]` and `[app.bypass_sanitize]`, logged if `--verbose` is set, and dropped. It is **never persisted to SQLite**, ensuring `src/storage/` schema compatibility remains absolute and immutable.

---

## 5. Zero-Dependency Configuration (`y4p.toml`)

### Lenient Standard-Library TOML Parser

To adhere to the project's zero-dependency invariant, `y4p` avoids heavy parsing frameworks (`serde`, `toml`). Instead, `core::config::Config` implements a dedicated streaming parser using standard library slice operations alone.

The parser is designed to be forgiving:
- **Resilience**: Stray syntax errors, comments (`#`), trailing commas, and unrecognised sections are ignored.
- **Fail-safe defaults**: Any unparseable line leaves the corresponding field initialised to its documented default value. A malformed config file will never crash the daemon or prevent it from monitoring the clipboard.
- **Multi-line folding**: Array values spanning multiple lines (such as `[app.ignore]`) are folded and parsed cleanly.

### Configuration Precedence

History limits and application behaviours resolve through a deterministic hierarchy:

```text
 1. Command-Line Arguments / Environment Variables (e.g. Y4P_MAX_HISTORY)
                         |
                         v
 2. User Configuration File ($XDG_CONFIG_HOME/y4p/y4p.toml)
                         |
                         v
 3. Hard-Coded Built-In Defaults (DEFAULT_MAX_HISTORY = 100, drop_rtf = true, etc.)
```

This ensures full operational capability in minimal containerised environments where no configuration file exists, while granting desktop users total control over their clipboard policies.
