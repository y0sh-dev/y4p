# 01 — Wayland Protocol & Streaming I/O

`y4p` talks to the compositor through `ext-data-control-v1`, which hands over clipboard data one file descriptor at a time. Every decision below follows from one constraint: a clipboard manager is background infrastructure. It has to move that data without becoming the reason the desktop feels slow, and without letting a single payload grow without limit.

Crates involved: `y4p-wayland` (protocol handlers, pipe plumbing) and `y4p-daemon` (event loop, restore dispatch). The persistence side of the contract is in [02](02_hybrid_storage_and_pin.md) and [03](03_concurrency_and_memory.md).

---

## 1. One Thread Watches Both Wayland and IPC

The daemon has two independent sources of work: Wayland events (a new offer arrived) and IPC commands (someone ran `y4p copy-to 3`). It does not spawn a thread per source. Both descriptors are watched by one `poll(2)` call on the same thread:

```rust
// crates/daemon/src/lib.rs — the daemon's main loop
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
   |    GlobalRemove ...)       Pause / Resume / Delete / Wipe) |
   +-----------------------------------------------------------+
        never performs disk I/O, hashing or image conversion
```

| Property | Consequence |
| :--- | :--- |
| Kernel wakes the process only on real activity | 0 % CPU between clipboard events; no helper threads parked on `recv()` |
| 500 ms timeout is a watchdog, not a busy-loop | `is_exiting()` and the seat-rebind self-heal (`bind_data_device`) are re-evaluated even with zero I/O |
| Wayland and IPC handling share one thread | No lock between "event" and "command"; they can never run concurrently |
| Everything that *needs* concurrency is pushed off-thread | Reads of offered payloads, optional image conversion, hashing and SQLite writes never stall the loop |

> **Invariant — the poll loop never blocks on payload work.** Anything proportional to payload size runs on a spawned thread (ingestion read, egress write) or on the `DbWorker` (optional conversion, hashing, persistence).

### 1.1 When the compositor goes away

A daemon that outlives its compositor must stop, not spin. If the loop kept polling a Wayland socket whose peer is gone, `poll` would report it readable at once on every pass and the daemon would burn a full core on a connection that can never recover. Two checks end the loop, in this order ([`crates/daemon/src/lib.rs`](../crates/daemon/src/lib.rs), `start_daemon` and `read_compositor_events`):

```rust
if poll_fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 { break; }   // the socket says so
if poll_fds[0].revents & libc::POLLIN != 0
    && let Some(guard) = event_queue.prepare_read()
    && !read_compositor_events(guard)                                       // the read says so
{
    break;
}
```

`POLLHUP` is not the only way a connection ends. `wayland-backend` stores the **first fatal error** it meets (end of file, a protocol error) and fails every later read with it at once, whatever the socket looks like; if the socket stayed readable while that error was ignored, the loop would spin. The read's own result is therefore the authority:

| `guard.read()` result | `read_compositor_events` | Loop |
| :--- | :--- | :--- |
| `Ok` | `true` | keep serving |
| `Io(WouldBlock)` or `Io(Interrupted)` | `true` | keep serving |
| any other `Io` error, including the `EPIPE` the backend reports for end of file | `false`, logs `lost the compositor connection: ...` | stop |
| `Protocol` error | `false`, same log | stop |

> **Invariant — a fatal read ends the loop and non-fatal ones never do.** The tests drive a real `UnixStream::pair` through `Connection::from_socket`: an idle connection keeps running, a transient error is not fatal, a dead one is, and a closed peer stops the loop.

### 1.2 Start-up handover and exit status

Starting the daemon while one is already running replaces it. Two mechanisms make that safe ([`crates/core/src/lib.rs`](../crates/core/src/lib.rs), [`crates/daemon/src/lib.rs`](../crates/daemon/src/lib.rs)):

| Mechanism | Behaviour |
| :--- | :--- |
| `SocketLock` | An advisory `flock(2)` (`LOCK_EX \| LOCK_NB`) on `<socket_path>.lock`, created mode 0600 beside the socket. The descriptor is owned by the `SocketLock` value, so the lock is released exactly on drop; `EINTR` is retried, and a held lock surfaces as `ErrorKind::WouldBlock` |
| `acquire_start_lock` | Retries the lock every `LOCK_RETRY_MS` (25 ms) for at most `LOCK_WAIT_MS` (2000 ms). While waiting, it connects to the socket and sends one `IPC_CMD_EXIT`, so the running instance is asked to leave instead of being raced. An instance that does not let go within the deadline is a start failure |
| Drop order | `_lock` is declared **before** the socket guard, so the socket file is unlinked first and the lock released second; a starting daemon can never take the lock and then find a socket a live daemon still owns |

How the run ended is reported, not guessed:

```rust
pub enum DaemonExit { Clean, StartFailed, ConnectionLost }
```

| Outcome | Meaning | `y4p daemon` exit status |
| :--- | :--- | :--- |
| `Clean` | SIGINT/SIGTERM or an IPC `Exit`: the daemon was asked to stop | `0` |
| `StartFailed` | The start-up lock, socket bind, compositor connection, read-side database or protocol bind failed | `1` |
| `ConnectionLost` | The compositor connection ended while no exit had been requested | `1` |

Only an exit that was actually asked for is clean. This is what makes `contrib/systemd/y4p.service`'s `Restart=on-failure` do something useful.

### 1.3 IPC requests cannot hold the loop either

`accept_and_dispatch` ([`crates/daemon/src/ipc.rs`](../crates/daemon/src/ipc.rs)) reads exactly one request line per connection, under the same two bounds that keep a hostile or broken client from stalling the poll thread:

| Bound | Value | Effect |
| :--- | :--- | :--- |
| Socket read timeout | `IPC_STATUS_TIMEOUT_MS` = 1000 ms (`SO_RCVTIMEO`) | A client that connects and then stalls, or never sends the delimiter, is disconnected instead of parking the daemon in `read` |
| Request line length | `MAX_REQUEST_BYTES` = 128 bytes | The longest legitimate request is one command byte plus a 20-digit `i64` and the delimiter, so 128 is generous; a longer line without a delimiter is cut off rather than growing the daemon's buffer |
| Decoding | `Command::decode` | A line with an unknown command byte or a non-numeric id dispatches nothing: the connection closes |

The same constant bounds the CLI's read of a `status` reply, so neither side can wait longer than the other is prepared to answer.

---

## 2. Ingestion: Bounded Read, Normalise, Forward

Turning a compositor pipe into a stored record is a sequence of distinct decisions. `y4p-wayland` owns the first four. There is exactly one read road: a bounded in-memory read under a wall-clock deadline.

```text
 Selection event
      |
      v
 [gate]      is_sensitive(mimes)?  paused?                         -> return, no pipe opened
             reader_gate.try_acquire() fails (8 readers)?          -> shed, no pipe opened
      |
      v
 [select]    select_mime_candidates(mimes, target_mime, drop_rtf)  -> empty: return (nothing allowed)
      |
      v  (one deadline for the whole selection, section 2.4)
 [read]      request_pipe(mime) -> offer.receive(...) -> drain_pipe(read_file, &mime, 32 MiB, deadline)
                DeadlineReader + read_bounded -> Vec<u8>
                nothing delivered, or normalised away? -> next candidate
                timed out? -> shed_timeout, stop      no pipe (EMFILE)? -> shed_readers, stop
      |
      v
 [normalise] normalise_payload(mime, bytes)   (uri-list decode, magic-byte relabel)
      |
      v
 [filter]    daemon ingestion path: text-like payload that trims to nothing -> return (not a shed)
      |
      v
 [forward]   ingest::enqueue(job_tx, ClipboardJob, 2 s)   (daemon mode; bounded, may shed)
             state.rx_buf = bytes                          (Action Mode: `y4p paste-from`)
             state.offered_mimes = mimes                   (Action Mode: `y4p paste-from -l`)
```

### 2.0 The pipe handed to the compositor

`make_pipe` ([`crates/wayland/src/handlers/data_control/mod.rs`](../crates/wayland/src/handlers/data_control/mod.rs)) creates the pipe whose write end `offer.receive` passes to the compositor. Two properties matter:

- **Dynamic sizing.** `core::pipe::grow` raises the pipe from the kernel default of 64 KiB to 1 MiB (`PIPE_TARGET_BYTES`) with `fcntl(F_SETPIPE_SZ)`, the unprivileged ceiling (`/proc/sys/fs/pipe-max-size`). The request is a hint: a descriptor that is not a pipe is left alone, `EPERM` and `ENOMEM` halve the request and retry (at most 16 attempts), a pipe already large enough is untouched and none is ever shrunk. The same helper sizes the paste pipe. The effect, measured with both ends in one process: context switches fall from 31 to 1 per MiB, and the paste direction is about 35 % faster ([`history/PERFORMANCE.md`](history/PERFORMANCE.md) section 7.3).
- **Allocation can fail.** `pipe(2)` returns `EMFILE` or `ENFILE` once descriptors run out. The selection is **counted** (`IngestStats::record_shed_reader`, shown as `shed_readers=` in `y4p status`) and logged as `could not allocate pipe descriptors (EMFILE)`.

### 2.1 Bounded read: `read_bounded`

The whole payload is read in one bounded call, capped by `MAX_PAYLOAD_BYTES` (32 MiB, [`crates/core/src/constants.rs`](../crates/core/src/constants.rs)). The primitive lives in `y4p-core`, so ingestion, the worker and egress share one rule:

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

`device.rs` wraps it as `read_pipe_payload(reader, mime, limit)` and supplies the hint: a Wayland pipe carries no length, so a text-like MIME reserves one page up front (`INLINE_PAYLOAD_THRESHOLD_BYTES`) and anything else starts empty. That turns the 3-9 allocations of a growing buffer into one for a typical payload ([`history/PERFORMANCE.md`](history/PERFORMANCE.md) section 7.1).

`Read::take` stops *silently* at its cap: no error, no flag, just `Ok(_)`. Reading one extra byte removes the ambiguity:

| Stream | Result |
| :--- | :--- |
| `len <= limit` | `Ok(payload)` |
| `len == limit` exactly | `Ok(payload)` (still passes) |
| `len > limit` | `Err(TooLarge)`: the whole payload is discarded, never truncated |
| I/O error or empty read | `Err(Io)` / `Err(Empty)` |

> **Invariant — no partial payloads.** Every failure mode of the read ends the same way: nothing is stored.

The read is wrapped in `core::pipe::DeadlineReader`, which bounds how long a stalled or slow-drip sender can hold a reader slot. `INGEST_READ_TIMEOUT_MS` (5000 ms) is one deadline for the whole selection, shared across candidates.

### 2.2 Normalisation: `normalise_payload`

Where the offered label cannot be trusted outright, the payload is re-identified from its own bytes:

| Input | Treatment |
| :--- | :--- |
| `text/uri-list` (matched by `mime_base_eq`: case, whitespace and parameters ignored) | `core::utils::normalize_uri_list`: percent-decoded, `file:` scheme and authority stripped, one path per line. `None` if nothing valid remains |
| Anything else | `core::utils::detect_image_mime` re-labels by magic bytes (PNG, JPEG, GIF, WebP, AVIF, SVG); bytes are never altered |

The result is a plain `(mime, data)` pair. Normalisation is lossless and sits **before** hashing, so two offers that normalise to the same paths collapse into the same fingerprint.

### 2.3 The payload vocabulary

`y4p-core` ([`crates/core/src/payload.rs`](../crates/core/src/payload.rs)) gives ingress and egress one type for "where the bytes live":

| Type | Role |
| :--- | :--- |
| `Payload` | `Memory(Vec<u8>)` or `File(PathBuf)`. `len_hint()` needs no I/O for memory; `try_clone()` duplicates a source that is served more than once; `read_bounded(limit)` materialises it |
| `PayloadReader` | `Read` over the file- and memory-backed variants by `enum` dispatch: no `Box<dyn Read>`, no allocation. `fd()` / `raw_fd()` return `None` for an in-memory reader, never panic |
| `route_payload(mime, len_hint)` | Picks a buffer pre-allocation for a pipe read: `Inline` (one page) or `Stream` (start empty). It is a heuristic only |

`Payload::Memory` is handed over, not copied: `read_bounded` validates it and returns the same `Vec`. Ingestion only ever produces `Memory`; `File` is the egress vocabulary for a cached binary.

### 2.4 Which MIME to Ask For: the Allowlist and the Fallback

A single offer can announce many MIME types, and a sender is free to announce types it cannot actually serve (Chromium does exactly that with `text/html`). Two functions, both in [`device.rs`](../crates/wayland/src/handlers/data_control/device.rs), decide what to ask for and what to do when a request comes back empty.

**`select_mime_candidates(mimes, target_mime, drop_rtf) -> Vec<String>`** returns every MIME worth trying, best first:

| Mode | Rule |
| :--- | :--- |
| Action Mode (`paste-from <mime>`) | Every offer that matches the request by `mime_base_eq`, in the order the sender announced them. `drop_rtf` is not consulted: an explicit request is honoured as given |
| Daemon ingestion | The **strict allowlist**: first every offer matching `MIME_PRIORITY_ORDER` (in that order), then the remaining `image/*` offers, then the remaining `text/*` offers (`drop_rtf` removes RTF from this step). Offers are deduplicated by base type, so `text/plain` and `text/plain;charset=utf-8` cost one attempt, not two |

There is deliberately **no catch-all**. An offer outside those three groups (`application/octet-stream`, `application/x-*`, `TARGETS`, an untyped blob) never becomes a candidate, so it is never requested and never stored.

**`read_first_candidate(candidates, deadline, open) -> FallbackOutcome`** walks that array. `open` produces the read end for one candidate; in production it is `request_pipe`, which allocates the pipe, calls `offer.receive(mime, write_fd.as_fd())`, flushes and drops the write end, so the compositor sees EOF. Tests pass a closure that returns a prepared pipe, which is what makes the fallback testable without a compositor.

| What the attempt produced | Next |
| :--- | :--- |
| A payload that normalises to something (`Ready`) | Stop; hand it to `enqueue` |
| `Failed` — a 0-byte transfer or an I/O error — or a payload that `normalise_payload` reduced to nothing (an empty URI list) | Try the next candidate |
| `TimedOut` | Record `shed_timeout`, stop immediately: the sender, not the MIME, is the problem |
| No allocatable pipe (`EMFILE`/`ENFILE`) | Record a shed reader, stop |

> **Invariant — one selection, one deadline.** `deadline = Instant::now() + INGEST_READ_TIMEOUT_MS` is computed once per selection and shared across every candidate. A compositor that stalls on the first MIME cannot extend the read by offering more of them.

> **Invariant — one reader slot, however many candidates.** The `ReaderPermit` is acquired once on the event loop, before the ingest thread is spawned, and is held until a payload has been queued or shed. Trying three candidates does not take three of the eight slots.

### 2.5 Whitespace-only text never becomes history (daemon ingestion path only)

An editor, a terminal or a "copy line" action happily sets a selection that is nothing but spaces, tabs and newlines. Left alone, such a selection consumes a history slot and evicts something the user actually copied, so daemon ingestion drops it before the job is queued ([`device.rs`](../crates/wayland/src/handlers/data_control/device.rs)):

```rust
fn is_whitespace_only_text(mime: &str, data: &[u8]) -> bool {
    crate::core::utils::is_text_like_mime(mime) && std::str::from_utf8(data).is_ok_and(|s| s.trim().is_empty())
}
```

The rule is deliberately narrow:

| Input | Outcome |
| :--- | :--- |
| Text-like MIME whose bytes are valid UTF-8 and `trim()` to nothing | Dropped before `ingest::enqueue`; the reader permit is released with the payload and **nothing is counted as shed**, because this is an intentional filter, not overload |
| Binary MIME (`image/png`, `application/octet-stream`, ...) | Kept untouched: `is_text_like_mime` gates the check, so a binary payload is never decoded in search of spaces |
| Invalid UTF-8 | Kept: blankness cannot be proven, so the bytes are stored rather than silently lost |

Action Mode is untouched by construction: `y4p paste-from <mime>` calls `read_first_candidate` directly, so an explicit request still returns a blank selection verbatim (section 5).

---

## 3. Self-Ingestion Without a Counter

`ext-data-control-v1` cannot tell "the user copied something new" apart from "the compositor is re-offering content the daemon itself just set". `y4p` does not keep a counter for it, for two reasons: a counter cannot tell a re-offer from a genuine copy made a moment later, and a restore that never results in a selection leaves it permanently ahead.

The store already solves the problem. Rows are keyed by a SHA3-256 fingerprint, and `upsert_record` looks the hash up first: re-ingesting the same bytes runs `UPDATE clipboard SET timestamp = ?1 WHERE id = ?2` on the existing row, so the restore's own re-offer touches MRU order and nothing else.

> **Invariant — identity is content.** Re-offering the payload the daemon just served is a no-op by construction (same bytes, same hash, same row), and a *different* payload copied right after a restore is stored normally.

The cost is one extra read-and-hash per restore, bounded by the same reader gate and queue as every other selection.

---

## 4. Zero-Copy Egress for Cached Payloads

Restoring a *text* entry is cheap: it is a small `Vec<u8>`. Restoring an *image* from `~/.cache/y4p/<hash>.cache` is different. A naive `read()`/`write()` would cross userspace twice for a transfer that never needed to touch this process's heap.

`storage::ContentLocation` routes around that. A record's payload is either `InlineBlob` (in the SQLite row) or `CacheFile` (on disk), and egress branches on it:

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
      Payload::Memory(Vec)               v
                |              spawned thread: serve_descriptor_payload
                |                        -> transfer: cache file -> pipe, in-kernel
                v
      spawned thread: write_bytes
```

| Aspect | Behaviour |
| :--- | :--- |
| Transfer | `sendfile(2)` copies file-to-pipe inside the kernel; the image never enters this process's heap |
| Chunking | At most `1 << 30` bytes per call, defensively below any kernel ceiling; the transfer runs to EOF |
| Addressing | `sendfile(2)` is given an explicit offset, so a seekable source's own position is untouched and the same cache file can be served again |
| Interruption | `EINTR` retries; `EPIPE`/`ECONNRESET` (receiver left) end quietly |
| Fallback | `copy_remaining` (userspace `io::copy`) if the kernel reports `EINVAL`/`ENOSYS`/`ESPIPE`. A reader with no descriptor (in memory) takes that same copy |
| Isolation | Own thread, so a slow reader can never stall the poll loop |

> **Invariant — egress never re-encodes.** The stored bytes go out exactly as saved. Cross-format image matching is deliberately refused in `mime_is_compatible`, because `y4p` cannot turn one encoding into another on request.

For every payload kind the only transformation is markup stripping: if the stored MIME is HTML/XHTML and the requester asked for a non-markup type, `strip_html_tags` runs (a file payload is first read under the `MAX_PAYLOAD_BYTES` bound); asking for the markup MIME itself returns the original bytes. Alternate MIMEs (`HTML_MIME_ALTS`, `TEXT_MIME_ALTS`) are advertised alongside the stored one.

---

## 5. Action Mode (`y4p paste-from`)

`paste_from_os` ([`crates/wayland/src/lib.rs`](../crates/wayland/src/lib.rs)) uses the same `WaylandState` with `job_tx = None` and a `target_mime`. It runs a poll-based loop with a 200 ms sub-timeout under a 1 s absolute watchdog, and performs the same candidate fallback, bounded read and normalisation synchronously into `state.rx_buf`.

`list_offer_types` shares that connection and poll loop through one private `run_action_mode` helper, but configures `WaylandState::new_list_types`: the Selection handler records `OfferData.mimes` in `offered_mimes` and returns before selecting candidates, taking a reader slot or opening a pipe. `y4p paste-from -l` prints them one per line.

> **Invariant — one boundary guard, two consumers.** `paste-from` returns exactly the bytes the daemon would have forwarded to the worker (before image routing, which is a persistence concern) — with one deliberate exception: the daemon drops whitespace-only text (section 2.5) that an explicit request still receives. `paste-from -l` returns exactly the MIME types the compositor advertised, in its order.

---

## 6. The Stream I/O Contract Between Crates

| Hand-off | From -> To | Type | Guarantee |
| :--- | :--- | :--- | :--- |
| Ingress | `y4p-wayland` -> `y4p-daemon` | `mpsc::SyncSender<ClipboardJob>` (capacity `INGEST_QUEUE_CAPACITY` = 64), `ClipboardJob { mime, payload: Payload }` | the payload is non-empty and at most `MAX_PAYLOAD_BYTES` (32 MiB), always `Payload::Memory`, already normalised or relabelled; no conversion |
| Persistence | `y4p-daemon` -> `y4p-storage` | `insert_with_hash(mime, data, hash, max_history)` | `hash` is SHA3-256 over the final bytes (see [03](03_concurrency_and_memory.md)) |
| Egress | `y4p-storage` -> `y4p-daemon` -> `y4p-wayland` | `ContentLocation` -> `core::payload::Payload` (`SourceMetadata.payload`) | Large payloads are passed as a path, never materialised |

> **Invariant — `y4p-wayland` is a pure sensor.** It depends on neither `y4p-storage`, `y4p-image` nor `sha3`; it never converts, fingerprints or persists a payload.
