# 01 — Wayland Protocol & Streaming I/O

`y4p` talks to the compositor through `ext-data-control-v1`, which hands over clipboard data one file descriptor at a time. Every decision below follows from one constraint: a clipboard manager is background infrastructure. It has to move that data, sometimes 70 MB+ of image, without becoming the reason the desktop feels slow.

Crates involved: `y4p-wayland` (protocol handlers, pipe plumbing) and `y4p-daemon` (event loop, restore dispatch). The persistence side of the contract is in [02](02_hybrid_storage_and_pin.md) and [03](03_concurrency_and_memory.md).

---

## 1. One Thread Watches Both Wayland and IPC

The daemon has two independent sources of work: Wayland events (a new offer arrived) and IPC commands (someone ran `y4p copy-to 3`). It does not spawn a thread per source. Both descriptors are watched by one `poll(2)` call on the same thread:

```rust
// crates/daemon/src/lib.rs:L128-L135 — the daemon's main loop
let mut poll_fds = [
    libc::pollfd { fd: conn.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
    libc::pollfd { fd: listener.as_fd().as_raw_fd(),  events: libc::POLLIN, revents: 0 },
];

// SAFETY: `poll_fds` is a valid, correctly-sized array of `pollfd` ...
if unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, 500) } < 0 { continue; }
```

```text
   +-----------------------------------------------------------+
   |  daemon main thread                                       |
   |                                                           |
   |   poll(wayland_fd, ipc_listener_fd, 500 ms)               |
   |        |                       |                          |
   |        v                       v                          |
   |   dispatch events         accept_and_dispatch             |
   |   (Selection, Offer,      (Exit / Restore / Status /      |
   |    GlobalRemove ...)       Pause / Resume)                |
   +-----------------------------------------------------------+
        never performs disk I/O, hashing or image conversion
```

| Property | Consequence |
| :--- | :--- |
| Kernel wakes the process only on real activity | 0 % CPU between clipboard events; no helper threads parked on `recv()` |
| 500 ms timeout is a watchdog, not a busy-loop | `is_exiting()` and the seat-rebind self-heal (`bind_data_device`, [lib.rs:L170](../crates/daemon/src/lib.rs)) are re-evaluated even with zero I/O |
| Wayland and IPC handling share one thread | No lock between "event" and "command"; they can never run concurrently |
| Everything that *needs* concurrency is pushed off-thread | Reads of offered payloads, image conversion, hashing and SQLite writes never stall the loop |

> **Invariant — the poll loop never blocks on payload work.** Anything proportional to payload size runs on a spawned thread (ingestion read, egress write) or on the `DbWorker` (conversion, hashing, persistence).

---

## 2. Ingestion: Bounded Read, Normalise, Forward

Turning a compositor pipe into a stored record is a sequence of distinct decisions. `y4p-wayland` owns the first four; the rest belong to the daemon's worker (see section 6 for the full contract).

```text
 Selection event
      |
      v
 [gate]      is_sensitive(mimes)?  provider_locks > 0?  paused?   -> return, no pipe opened
             reader_gate.try_acquire() fails (8 readers)?          -> shed, no pipe opened
      |
      v
 [select]    select_mime(mimes, target_mime, drop_rtf)
      |
      v
 [read]      offer.receive(mime, write_fd)  ->  read_pipe_payload(read_file, &mime, MAX_PAYLOAD_BYTES)
      |
      v
 [normalise] normalise_payload(mime, payload)
      |
      v
 [forward]   ingest::enqueue(job_tx, ClipboardJob::new(mime, data), 2 s)   (daemon mode; bounded, may shed)
             state.rx_buf = payload                        (Action Mode: `y4p paste-from`)
```

### 2.1 Bounded read: `read_bounded`

The whole payload is read in one bounded call, capped by `MAX_PAYLOAD_BYTES` (256 MiB, [`crates/core/src/constants.rs:L43`](../crates/core/src/constants.rs)). Since v0.7.2 the primitive lives in `y4p-core`, so ingestion, the worker (`Payload::read_bounded`) and egress share one rule:

```rust
// crates/core/src/payload.rs
pub fn read_bounded<R: Read>(reader: R, limit: u64, capacity_hint: Option<u64>) -> Result<Vec<u8>, PayloadError> {
    let mut payload = Vec::new();
    // (reserve min(hint, limit, PAYLOAD_PREALLOC_CAP_BYTES); a failed reserve is ignored)
    reader.take(limit.saturating_add(1)).read_to_end(&mut payload)?; // one probe byte beyond the cap
    if payload.is_empty() { Err(PayloadError::Empty) }
    else if payload.len() as u64 > limit { Err(PayloadError::TooLarge { limit }) }
    else { Ok(payload) }
}
```

`device.rs` wraps it as `read_pipe_payload(reader, mime, limit)` (returning `Option`, as before) and supplies the hint: a Wayland pipe carries no length, so a text-like MIME reserves one page up front (`INLINE_PAYLOAD_THRESHOLD_BYTES`) and anything else starts empty. That turns the 3-9 allocations of a growing buffer into one for a typical payload ([`PERFORMANCE.md`](../PERFORMANCE.md) section 3.5).

`Read::take` stops *silently* at its cap: no error, no flag, just `Ok(_)`. If the reader were capped at exactly `limit`, an oversized stream would be cut off mid-transfer and look identical to one that happened to be exactly `limit` bytes long, and the truncated bytes would be hashed and persisted as if they were the whole entry: silent corruption that restores "successfully".

Reading one extra byte removes the ambiguity:

| Stream | Result |
| :--- | :--- |
| `len <= limit` | `Ok(payload)` |
| `len == limit` exactly | `Ok(payload)` (still passes) |
| `len > limit` | `Err(TooLarge)`: the whole payload is discarded, never truncated |
| I/O error or empty read | `Err(Io)` / `Err(Empty)` |

> **Invariant — no partial payloads.** Every failure mode of the read ends the same way: nothing is stored.

`read_to_end` is the standard library's own fully-buffered read. There is no bespoke chunk loop or page-aligned buffer, because nothing on this path needs one: the read already runs off the poll loop, on its own per-selection thread.

### 2.2 Normalisation: `normalise_payload`

Where the offered label cannot be trusted outright, the payload is re-identified from its own bytes:

| Input | Treatment |
| :--- | :--- |
| `text/uri-list` (matched by `mime_base_eq`: case, whitespace and parameters ignored) | `core::utils::normalize_uri_list`: percent-decoded, `file:` scheme and authority stripped, one path per line. `None` if nothing valid remains |
| Anything else | `core::utils::detect_image_mime` re-labels by magic bytes (PNG, JPEG, GIF, WebP, AVIF, SVG); bytes are never altered |

The result is a plain `(mime, data)` pair. Normalisation is lossless and sits **before** hashing, so two offers that normalise to the same paths (different `file://` escaping, say) collapse into the same fingerprint.

### 2.3 The payload vocabulary and routing (v0.7.2)

`y4p-core` ([`crates/core/src/payload.rs`](../crates/core/src/payload.rs)) gives ingress and egress one type for "where the bytes live":

| Type | Role |
| :--- | :--- |
| `StreamDescriptor` | An owned, readable descriptor (pipe, socket, memfd) with an advisory `len_hint`. Std's `File` owns the fd, so it is closed exactly once on every path and `Read`/`dup(2)` need no `unsafe` |
| `Payload` | `Memory(Vec<u8>)`, `Stream(StreamDescriptor)` or `File(PathBuf)`. `len_hint()` needs no I/O; `try_clone()` duplicates a source that is served more than once; `read_bounded(limit)` materialises it |
| `PayloadReader` | `Read` over the three by `enum` dispatch: no `Box<dyn Read>`, no allocation. `fd()` / `raw_fd()` return `None` for an in-memory reader, never panic |
| `route_payload(mime, len_hint)` | `Inline` or `Stream`, described below |

Routing rule, cheapest evidence first:

1. A **length hint decides**: at most `INLINE_PAYLOAD_THRESHOLD_BYTES` (4096, one page) is `Inline`, anything larger is `Stream`, whatever the MIME says.
2. **No hint: the MIME is the prior.** Text-like types (`is_text_like_mime`) are `Inline`; images and unknown binaries are `Stream`.

> **Invariant — a hint is advisory, never trusted.** It pre-sizes a buffer and feeds the routing guess, but every reader still stops at its own limit and at EOF. A peer that under- or over-states a length can neither truncate a payload nor make the daemon over-read; a wrong guess only picks the slower of two correct paths.

`Payload::Memory` is handed over, not copied: `read_bounded` validates it and returns the same `Vec` (a unit test asserts pointer equality). Ingestion still sends only `Memory` (it must normalise the bytes first, and the pipe has to be drained while its permit is held); the `Stream` and `File` variants are the seam for the streamed path of v0.7.3, and the worker already materialises them under the same `MAX_PAYLOAD_BYTES` bound.

---

## 3. Why `provider_locks` Exists

`ext-data-control-v1` cannot tell "the user copied something new" apart from "the compositor is re-offering content the daemon itself just set". Without a way to distinguish them, `copy-to` would immediately trigger the daemon's own ingestion handler, re-hash and re-store the content it just placed on the clipboard: a feedback loop that only touches a timestamp on every restore.

`y4p` closes the loop with a plain counter. Before handing data to the compositor, restore increments it:

```rust
// crates/daemon/src/lib.rs:L215 — handle_restore_request
state.provider_locks += 1;
```

and the ingestion handler checks it first, before doing any work:

```rust
// crates/wayland/src/handlers/data_control/device.rs:L29
if state.provider_locks > 0 {
    state.provider_locks -= 1;
    return;
}
```

> **Invariant — the counter is single-threaded by construction.** Both sides run on the one event-loop thread of section 1, so there is no race between "increment" and "check". A plain `u32` field of `WaylandState` is enough: no atomics, no mutex. Use the simplest tool that fits the concurrency model you actually have.

---

## 4. Zero-Copy Egress for Cached Payloads

Restoring a *text* entry is cheap either way: it is a small `Vec<u8>`. Restoring an *image* from `~/.cache/y4p/<hash>.cache` is different. Naively, `read()` the whole file into a `Vec<u8>`, then `write()` it to the requesting client's pipe: the payload crosses userspace twice for a transfer that never needed to touch this process's heap.

`storage::ContentLocation` routes around that. A record's payload is either `InlineBlob` (small, in the SQLite row) or `CacheFile` (on disk), and egress branches on it:

```text
  copy-to  --IPC-->  handle_restore_request
                          |
                          v
                 read_db.locate_content(id)
                    /                \
          InlineBlob(id)          CacheFile(path)
                |                        |
      get_content_by_id           Payload::File(path)
                |                        |
      Payload::Memory(Vec)               |
                |                        v
                v              spawned thread: serve_descriptor_payload
      spawned thread: write_bytes        -> transfer: cache file -> pipe, in-kernel
```

```rust
// crates/wayland/src/handlers/data_control/source.rs:L44
if let Some(data) = meta.payload.as_bytes() {
    // in memory: one copy (or a fresh buffer when markup is stripped)
    spawn_egress(move || write_bytes(fd, &data_to_send));
} else {
    // File or Stream: duplicated handle, bytes never enter this heap
    match meta.payload.try_clone() {
        Ok(payload) => spawn_egress(move || serve_descriptor_payload(payload, strip_markup, fd)),
        Err(e) => eprintln!("{}egress: could not duplicate the payload source: {}", LOG_ERROR, e),
    }
}
```

| Aspect | Behaviour |
| :--- | :--- |
| Transfer | `sendfile(2)` copies file-to-pipe inside the kernel; the image never enters this process's heap |
| Chunking | At most `1 << 30` bytes per call, defensively below any kernel ceiling; the transfer runs to EOF, so a wrong length hint can neither truncate nor pad it |
| Addressing | `sendfile(2)` is given an explicit offset, so a seekable source's own position is untouched and the same cache file can be served again |
| Interruption | `EINTR` retries; `EPIPE`/`ECONNRESET` (receiver left) end quietly |
| Fallback | `copy_remaining` (userspace `io::copy`, skipping the bytes `sendfile` already delivered) if the kernel reports `EINVAL`/`ENOSYS`, or `ESPIPE` for a pipe/socket source that cannot be addressed by offset. A reader with no descriptor (in memory) takes the same copy |
| Isolation | Own thread (`Builder::spawn`: a refused thread is logged, not a panic), so a slow reader can never stall the poll loop |

> **Invariant — egress never re-encodes.** The stored bytes go out exactly as saved. Cross-format image matching is deliberately refused in `mime_is_compatible`, because `y4p` cannot turn one encoding into another on request.

For every payload kind the only transformation is markup stripping: if the stored MIME is HTML/XHTML and the requester asked for a non-markup type, `strip_html_tags` runs (a `File` or `Stream` payload is first read under the `MAX_PAYLOAD_BYTES` bound, since stripping needs the bytes); asking for the markup MIME itself returns the original bytes. A `Stream` source is *consume-once* on a pipe or socket (its `dup(2)`ed clones share one read position); a seekable one is replayable. Alternate MIMEs (`HTML_MIME_ALTS`, `TEXT_MIME_ALTS`) are advertised alongside the stored one so the paste target can pick what it understands.

---

## 5. Action Mode (`y4p paste-from`)

`paste_from_os` ([`crates/wayland/src/lib.rs:L29`](../crates/wayland/src/lib.rs)) uses the same `WaylandState` with `job_tx = None` and a `target_mime`. It runs a poll-based loop with a 200 ms sub-timeout under a 1 s absolute watchdog, and performs the same bounded read and normalisation synchronously into `state.rx_buf`.

> **Invariant — one boundary guard, two consumers.** `paste-from` returns exactly the bytes the daemon would have forwarded to the worker (before image routing, which is a persistence concern).

---

## 6. The Stream I/O Contract Between Crates

| Hand-off | From -> To | Type | Guarantee |
| :--- | :--- | :--- | :--- |
| Ingress | `y4p-wayland` -> `y4p-daemon` | `mpsc::SyncSender<ClipboardJob>` (capacity `INGEST_QUEUE_CAPACITY`), `ClipboardJob { mime, payload: Payload }` | the payload is non-empty and at most `MAX_PAYLOAD_BYTES` once materialised (`ClipboardJob::into_bytes`, a no-copy hand-over for `Memory`), already normalised; no hash, no conversion |
| Persistence | `y4p-daemon` -> `y4p-storage` | `insert_with_hash(mime, data, hash, max_history)` | `hash` is SHA3-256 over the final bytes (see [03](03_concurrency_and_memory.md)) |
| Egress | `y4p-storage` -> `y4p-daemon` -> `y4p-wayland` | `ContentLocation` -> `core::payload::Payload` (`SourceMetadata.payload`) | Large payloads are passed as a path, never materialised |

> **Invariant — `y4p-wayland` is a pure sensor.** It depends on neither `y4p-storage`, `y4p-image` nor `sha3`; it never converts, fingerprints or persists a payload.
