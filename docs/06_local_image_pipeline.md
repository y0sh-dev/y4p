# 06 — Local Image Pipeline & Defence-in-Depth Hardening

This document details the architecture, sandboxing and resource governance of `y4p`'s local image pipeline: the part of ingestion that handles untrusted raster images.

Crate involved: `y4p-image` (`crates/image/`), depending only on `std`, `libc` and `y4p-core`. It knows nothing about SQLite, Wayland or the daemon. Its sole consumer is the `y4p-daemon` worker (`process_job`, see [00](00_overview.md) section 5 and [03](03_concurrency_and_memory.md) section 2).

---

## 1. Architectural Motivation

Wayland clipboards routinely carry high-resolution screenshots, framebuffers and multi-megabyte assets next to plain text. Treating image bytes like text creates acute hazards:

1. **Metadata leakage.** Photos and screenshots carry EXIF (including GPS and device identifiers), XMP, IPTC and embedded text chunks. A clipboard history that persists them verbatim retains data the user never meant to keep.
2. **Decompression bombs.** A tiny file whose header advertises enormous dimensions can induce severe CPU or memory exhaustion in a decoder.
3. **Decoder attack surface.** ImageMagick auto-detects formats and can route attacker-chosen bytes into any of ~100 coders, including scriptable ones with a long vulnerability history.
4. **Shared-directory hazards.** Decoders spill working state to disk. In a shared `/tmp`, predictable names invite symlink races and leak a decoded clipboard image.
5. **Inline bloat.** Multi-megabyte binaries inside SQLite degrade B-tree locality and bloat the WAL (see [02](02_hybrid_storage_and_pin.md)): converted images are stored in `~/.cache/y4p/<hash>.cache` instead.

To address these, `y4p` routes raster images through an isolated pipeline before the SQLite row and cache file are written.

> **Invariant — never lose data, never make things worse.** Whatever goes wrong in this crate, the caller still receives a usable `(mime, bytes)` pair: the converted pair on success, the original pair **byte for byte** on bypass or on any failure.

---

## 2. Where It Sits, and What It Decides

```text
  y4p-wayland  --ClipboardJob{mime,payload}-->  DbWorker thread
                                                  |
                                                  v
                                   route::route_for_ingest(mime, data)
                                                  |
                         +------------------------+-------------------------+
                         |                                                  |
                  bypass (original)                                  convert via magick
        text, HTML, uri-list, SVG, AVIF,                  PNG, BMP, JPEG, static WebP
        GIF, animated WebP, empty, unknown                  -> lossless PNG (zlib level 1)
        breaker open, throttle saturated,
        magick missing, no scratch dir,
        conversion failed
                         \                                                  /
                          +-----------------> (mime, bytes) <---------------+
                                                  |
                                                  v
                                   SHA3-256 over the final bytes -> insert_with_hash
```

| Module | Role |
| :--- | :--- |
| `route.rs` | Classification and the conversion/bypass decision (`route_for_ingest`, testable core `route_with`) |
| `transcode.rs` | Closed-enum argument construction for `magick`; resource limits; the compile-time timeout assertion |
| `magick.rs` | Availability probe (cached once per process) and isolated runs |
| `sandbox.rs` | Private scratch directory (`ScratchDir`) and the environment whitelist (`child_env`) |
| `pipeline.rs` | Streaming relay to a child, process-group watchdog, `PipelineError` |
| `throttle.rs` | Counting semaphore bounding concurrent converter processes |
| `breaker.rs` | Three-state circuit breaker over converter faults |

### 2.1 What is converted, and why

| Input | Output | Reason |
| :--- | :--- | :--- |
| PNG, BMP, JPEG, static WebP | lossless PNG (`png:compression-level=1`) | Bit-exact pixels (a JPEG or WebP source keeps its decoded pixels, with no further generation loss), metadata removed, EXIF orientation baked in. PNG is the raster MIME every Wayland paste target requests |
| GIF, animated WebP | untouched | The transcoder keeps only the first frame, which would silently destroy an animation the user copied (animated WebP is detected via the `VP8X` `ANIM` flag) |
| SVG, AVIF, anything else | untouched | Vector and scriptable formats never reach a raster decoder; AVIF is not a supported input |

> **Invariant — the stored raster MIME is `image/png`.** Egress offers exactly the stored MIME ([03](03_concurrency_and_memory.md), [05](05_filter_pipeline_and_app_rules.md) section on compatibility), and `image/png` never matches `image/webp`. A stored WebP was reported as not pasteable on Hyprland into applications that request `image/png` (Discord, Slack, Firefox, Chrome, GIMP, LibreOffice); not re-verified against a live compositor here. Ingestion consequently never produces WebP; the WebP encoders (`OutputFormat::WebpLossless`, `WebpLossy`) remain in `transcode.rs` as tested but unused capability.

**Why zlib level 1.** PNG is lossless at every zlib level; the level trades encode time against file size only. Measured with ImageMagick 7.1 on a 1920x1080 image: photo-like content 137 ms at level 1 against 257 ms at level 9; flat screenshot 65 ms against 81 ms; pixels identical in both (`compare -metric AE` = 0). The cost is size on flat content (the screenshot grew from 22 KB to 93 KB), and a JPEG or WebP source grows several times when stored losslessly. The clipboard path is latency-bound, so speed wins. No `-quality` flag is passed for PNG: it would override the level.

**Animation preservation.** GIF and animated WebP (`VP8X` container with the `ANIM` flag set) bypass transcoding entirely so their multi-frame sequences are not flattened into a single frame.

---

## 3. Ingestion Boundaries & Format Sniffing

MIME types advertised by Wayland sources cannot be trusted unconditionally. An application advertising `image/png` may supply corrupted bytes, or a different format entirely. So the **bytes outrank the label**.

```text
 [ Incoming payload ]
          |
          v
 [ Magic-byte sniff of the first 512 bytes ]   core::utils::detect_image_mime
          |-- 89 50 4E 47            -> image/png
          |-- FF D8 FF               -> image/jpeg
          |-- 47 49 46 38 ("GIF8")   -> image/gif
          |-- "RIFF" .... "WEBP"     -> image/webp
          |-- .... "ftyp" avif/avis  -> image/avif
          |-- "<?xml" | "<svg"       -> image/svg+xml   (after leading whitespace)
          |
          +-- no signature -> the declared MIME decides (BMP has no short signature)
```

When the leading bytes identify an image, that identification is final: an SVG announced as `image/png` is recognised as SVG and bypassed instead of being handed to a raster decoder. Payloads that classify to nothing cost nothing: no process, no allocation beyond the input.

> **Invariant — the decoder is pinned, never guessed.** `transcode.rs` passes the input as `png:-[0]`, `jpeg:-[0]`, `bmp:-[0]` and so on. A pinned coder makes a mislabelled or disguised payload fail to decode instead of reaching a different parser, and `[0]` selects the first frame so an animated input cannot expand into a frame sequence. Vector and scriptable formats are deliberately not representable in `InputFormat`.

---

## 4. Argument Construction

Every argument handed to `magick` is derived from closed enums and compile-time constants, never from clipboard-controlled text, so no option, `coder:` prefix or filename can be smuggled in. The vector is built in the one safe order ([`crates/image/src/transcode.rs:L212`](../crates/image/src/transcode.rs)):

| # | Tokens | Why this position |
| :--- | :--- | :--- |
| 1 | `-limit width\|height\|memory\|map\|disk\|time <value>` | Ceilings are active before any byte is decoded, so a bomb is rejected at the header, not after allocation |
| 2 | pinned decoder `fmt:-[0]` | See section 3 |
| 3 | `-auto-orient` | Orientation lives in EXIF; stripping first would turn a rotated photo sideways. The rotation is baked into the pixels, then the tag is dropped |
| 4 | `-strip` | Removes every profile and comment: EXIF incl. GPS, XMP, IPTC, ICC, text chunks. **Not a switch: every transcode strips** |
| 5 | encoder settings, output `png:-` | ingestion always uses `-define png:compression-level=1` (a borrowed `'static` token: no allocation). The WebP variants (`webp:lossless=true` with quality 100, or `webp:lossless=false` with the clamped quality) are not selected by ingestion |

`Quality` (WebP only) clamps to 1..=100 on construction, so no input can make `magick` reject the option. In lossless WebP mode ImageMagick maps any quality below 100 onto libwebp's near-lossless pre-processing, which *does* alter pixels (measured: 59 of 16 384 pixels differ at quality 75), so the lossless variant is pinned to 100.

---

## 5. Sandboxing & Workspace Isolation (`ScratchDir`)

### 5.1 Why not `/tmp`

With `-limit disk` set, ImageMagick moves its pixel cache to temporary files once memory and map limits are exhausted. In a shared `/tmp` those files are readable-by-name side channels and a symlink-race target, and they contain a decoded clipboard image.

### 5.2 Resolution and verification ([`crates/image/src/sandbox.rs`](../crates/image/src/sandbox.rs))

The converter is pointed at a directory only the current user can enter:

1. `$XDG_RUNTIME_DIR/y4p-magick`, provided `$XDG_RUNTIME_DIR` is an absolute path to a real directory (not a symlink) owned by the current user with no group/other permissions; otherwise
2. `/tmp/y4p-magick-<uid>`.

Nothing is taken on trust. Every candidate is `lstat`ed, must be a real directory owned by the effective uid, and is tightened to `0700` if it was ours but looser. A symlink, a plain file or someone else's directory is never used and never followed. If **neither** location qualifies, creation fails and the converter is **not run at all**: falling back to the shared default would defeat the purpose.

Each conversion gets its own fresh subdirectory (`run-<pid>-<n>`, mode `0700`), removed by the RAII `Drop` of `ScratchDir`, which also cleans up after a child killed by the watchdog and keeps concurrent conversions out of each other's files.

### 5.3 Child environment whitelist (`child_env`)

A child that inherits the full environment can be steered by `MAGICK_CONFIGURE_PATH`, `MAGICK_HOME`, `MAGICK_CODER_MODULE_PATH`, `HOME`, `XDG_CONFIG_HOME` (any of which can substitute ImageMagick's security policy or coders), `LD_PRELOAD` or `LD_LIBRARY_PATH`. `ChildEnv::apply` first clears the environment, then sets exactly:

| Variable | Value |
| :--- | :--- |
| `PATH` | inherited, so `magick` can be found |
| `MAGICK_TEMPORARY_PATH` | the active `ScratchDir` |
| `TMPDIR` | the active `ScratchDir` |

Everything else is stripped. The system-wide policy in `/etc/ImageMagick-*` still applies.

---

## 6. Defence-in-Depth Resource Controls

Concentric boundaries make sure a rogue image cannot stall or destabilise the daemon.

```text
+-----------------------------------------------------------------+
| Parent watchdog: IMAGE_TRANSCODE_TIMEOUT_SECS = 15 s            |
| kill(-pgid, SIGKILL) terminates the whole process group         |
|                                                                 |
|   +-----------------------------------------------------------+ |
|   | Child limit: IMAGE_LIMIT_TIME_SECS = 10 s                 | |
|   | `-limit time 10` -> ImageMagick ends itself with a        | |
|   | diagnostic                                                | |
|   |                                                           | |
|   |   +-----------------------------------------------------+ | |
|   |   | Resource quotas (`-limit`)                          | | |
|   |   | width/height 16384 px, memory 256MiB, map 512MiB,   | | |
|   |   | disk 1GiB                                           | | |
|   |   +-----------------------------------------------------+ | |
|   +-----------------------------------------------------------+ |
+-----------------------------------------------------------------+
```

16384 px comfortably covers an 8K screenshot (7680x4320). The inner layers handle the cooperative case (a bomb is rejected with ImageMagick's own diagnostic); the outer watchdog handles a child that ignores its limits (blocked, deadlocked).

### 6.1 Process-group kill

The child starts as the leader of its own process group (`process_group(0)`). On timeout the watchdog SIGKILLs the **group**, which reaches any helper the child spawned. That matters beyond tidiness: a surviving grandchild would hold the stdout/stderr pipes open, the reader threads would never see EOF, and the pipeline would hang even after the "kill". The watchdog only signals while the child is unreaped, so the pid cannot name an unrelated process.

### 6.2 Compile-time timeout assertion

`IMAGE_LIMIT_TIME_SECS` is a string (it is handed to `magick` as an argument), so the assertion parses it in a `const fn` and fails the build on anything but a whole number of seconds below the outer budget:

```rust
// crates/image/src/transcode.rs:L164
const _: () = assert!(
    parse_secs(IMAGE_LIMIT_TIME_SECS) >= 1 && parse_secs(IMAGE_LIMIT_TIME_SECS) < IMAGE_TRANSCODE_TIMEOUT_SECS,
    "IMAGE_LIMIT_TIME_SECS must be a whole number of seconds below IMAGE_TRANSCODE_TIMEOUT_SECS",
);
```

> **Invariant — the child's own CPU limit always fires first.** A runaway conversion ends with the converter's structured diagnostic; only a child that ignores its limit meets the SIGKILL.

### 6.3 Streaming without deadlock

`run_filter_in_env` relays through three cooperating threads: one feeds the child's stdin in 64 KiB chunks, one drains stderr to EOF while keeping only the first 8 KiB (a chatty child can never block on a full stderr pipe), and the calling thread relays stdout into the output buffer. Each pipe always has a live peer, whatever order the child reads and writes in. A `BrokenPipe` on the feed is not an error (the child stopped reading); the exit status is the authoritative verdict. On any `Err` the output buffer may be partial and is discarded.

The output `Vec` is reserved up front from the input length (capped at `IMAGE_OUTPUT_HINT_CAP_BYTES`, 16 MiB), as a fair estimate for PNG and BMP sources; a JPEG or WebP source grows several times as lossless PNG, and outgrowing the hint is harmless (the buffer then grows as usual).

---

## 7. Concurrency Throttling & the Self-Healing Circuit Breaker

### 7.1 Bounded concurrency (`throttle.rs`)

A burst of copied images (a held-down shortcut, a script injecting into the clipboard) would otherwise fork one `magick` per image. `ProcessThrottle` is a counting semaphore (`Mutex` + `Condvar`) admitting at most `IMAGE_CONCURRENCY_LIMIT = 2` conversions. A caller that finds every slot busy waits up to `IMAGE_THROTTLE_WAIT_MS = 1000`, then keeps the **original** bytes. Saturation is load, not a converter fault, so it never counts towards the breaker.

### 7.2 Circuit breaker (`breaker.rs`)

A converter that keeps failing (crashing, broken libraries, hanging) must not be re-spawned for every image: each attempt burns CPU and delays the save.

```text
            IMAGE_BREAKER_THRESHOLD (3) consecutive faults
   Closed ----------------------------------------------> Open (cooldown: 30 s)
     ^                                                        |
     |                                                        | cooldown elapsed
     |  canary succeeds                                       v
     +------------------------------------------------- Half-Open
                                                    one canary conversion;
                                       canary fails  ----->  back to Open
```

- **Closed:** normal operation.
- **Open:** conversion is skipped entirely; images are stored unmodified and no process is spawned.
- **Half-Open:** exactly one canary attempt decides whether to resume.

The breaker only does bookkeeping: the caller decides what counts as a fault. In `route.rs`:

| Outcome | Breaker accounting |
| :--- | :--- |
| `Failed` (non-zero exit / signal), `Timeout`, `Child` I/O error | **Fault** |
| `Ok` but the output is empty or has the wrong signature | **Fault** (a "success" producing no usable file) |
| `Spawn` (`magick` missing) | Neutral: a static condition handled by the availability probe |
| `Isolation` (no private scratch directory) | Neutral: a property of the host, not of the converter |
| `Source` / `Sink` (the caller's own streams) | Neutral |
| Throttle saturated | Neutral (permit released) |

> **Invariant — environmental faults never trip the breaker.** Directory-isolation failures and missing tools are classified separately from transcode failures, eliminating false-positive circuit trips.

A missing `magick` is reported once per process (`MSG_IMAGE_TOOL_MISSING`) and images are saved unmodified, metadata included.

---

## 8. Failure Semantics at a Glance

| Situation | Result stored | Process spawned? |
| :--- | :--- | :--- |
| Text, HTML, uri-list, SVG, AVIF, GIF | original | no |
| Empty payload | original | no |
| Breaker open | original | no |
| Throttle wait expired | original | no |
| `magick` missing / no private scratch directory | original (one warning) | no |
| Corrupt or over-limit image | original | yes, then discarded |
| Conversion exceeds 15 s | original | yes, group killed |
| Converted output invalid | original | yes, then discarded |
| Success | converted PNG; hash covers the converted bytes | yes |

In every row the fingerprint is computed over what is stored, so deduplication stays consistent whether or not conversion happened. A deterministic conversion also means the same image copied twice yields the same fingerprint.

---

## 9. Zero-Copy Trajectory

The chunked `Read`/`Write` relay is the working implementation. `pipeline.rs` already carries a `splice(2)` primitive (`try_splice` / `splice_all`) with a capability-aware fallback, deliberately not wired into the converter path yet; the `Read`/`Write` boundary is the seam where it can replace the chunked copy without changing any caller.

Since v0.7.3 the *ingestion* side has its own bounded, fingerprinting counterpart in `y4p-core` (`core::splice::copy_to_file`, with `tee(2)`; [01](01_wayland_streaming_io.md) section 2.4), and the worker asks this crate one question about a spooled payload, `route::wants_transcode(mime, head)`: would the converter touch it? Only a "yes" reads the file back into memory, because `magick` is still fed from a `Vec`. Connecting the converter to a descriptor directly, and letting `pipeline.rs` share `core::splice`'s primitive instead of its own, is the v0.7.4 task.

The v0.6.x cycle (The Great Modularization) physically decoupled `y4p-core`, `y4p-storage`, `y4p-image`, `y4p-wayland`, `y4p-daemon` and `y4p-cli`, and in v0.6.7 moved conversion and hashing out of the Wayland sensor into the daemon worker.

**Deferred beyond v0.7.0:** Zero-Copy Stage 2 (Buffer Lifecycle Unification: aligning buffer lifetimes and descriptor passing across crate boundaries), including a descriptor-to-descriptor path from the compositor pipe to the converter. It is out of scope for the v0.7.0 release.
