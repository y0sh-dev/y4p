# 01 — Wayland Protocol & Streaming I/O

`y4p` talks to the compositor through `ext-data-control-v1`, which hands over clipboard data one file descriptor at a time. Every decision below follows from one constraint: a clipboard manager is background infrastructure. It has to move that data, sometimes 70 MB+ of image, without becoming the reason the desktop feels slow.

Crates involved: `y4p-wayland` (protocol handlers, pipe plumbing) and `y4p-daemon` (event loop, restore dispatch). The persistence side of the contract is in [02](02_hybrid_storage_and_pin.md) and [03](03_concurrency_and_memory.md).

---

## 1. One Thread Watches Both Wayland and IPC

The daemon has two independent sources of work: Wayland events (a new offer arrived) and IPC commands (someone ran `y4p copy-to 3`). It does not spawn a thread per source. Both descriptors are watched by one `poll(2)` call on the same thread:

```rust
// crates/daemon/src/lib.rs:L137-L144 — the daemon's main loop
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
| 500 ms timeout is a watchdog, not a busy-loop | `is_exiting()` and the seat-rebind self-heal (`bind_data_device`, [lib.rs:L133](../crates/daemon/src/lib.rs)) are re-evaluated even with zero I/O |
| Wayland and IPC handling share one thread | No lock between "event" and "command"; they can never run concurrently |
| Everything that *needs* concurrency is pushed off-thread | Reads of offered payloads, image conversion, hashing and SQLite writes never stall the loop |

> **Invariant — the poll loop never blocks on payload work.** Anything proportional to payload size runs on a spawned thread (ingestion read, egress write) or on the `DbWorker` (conversion, hashing, persistence).

### 1.1 When the compositor goes away (v0.7.8)

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
| `Io(WouldBlock)` (another reader already took the data, or a spurious wake-up) or `Io(Interrupted)` | `true` | keep serving |
| any other `Io` error, including the `EPIPE` the backend reports for end of file | `false`, logs `lost the compositor connection: ...` | stop |
| `Protocol` error | `false`, same log | stop (the compositor drops the client anyway) |

> **Invariant — a fatal read ends the loop and non-fatal ones never do.** The tests drive a real `UnixStream::pair` through `Connection::from_socket`: an idle connection keeps running, a transient error is not fatal, a dead one is, and a closed peer stops the loop.

---

## 2. Ingestion: Bounded Read, Normalise, Forward

Turning a compositor pipe into a stored record is a sequence of distinct decisions. `y4p-wayland` owns the first four; the rest belong to the daemon's worker (see section 6 for the full contract). Since v0.7.3 the *read* step has two roads (section 2.4): the in-memory one described in 2.1, and, for large non-text payloads, a kernel-level stream into a spool file.

```text
 Selection event
      |
      v
 [gate]      is_sensitive(mimes)?  provider_locks > 0?  paused?   -> return, no pipe opened
             reader_gate.try_acquire() fails (8 readers)?          -> shed, no pipe opened
             make_pipe() fails (EMFILE)?                           -> shed, counted, logged
      |
      v
 [select]    select_mime(mimes, target_mime, drop_rtf)
      |
      v
 [read]      offer.receive(mime, write_fd)  ->  drain_pipe(read_file, &mime, MAX_PAYLOAD_BYTES, spooler)
                 text, small, or no spooler : read_pipe_payload     -> Drained::Memory(Vec)
                 large non-text             : head + splice(2) spool -> Drained::Spooled(SpooledFile)
      |
      v
 [normalise] normalise_payload(mime, payload)            (Memory: uri-list, magic bytes)
             relabel_by_magic(mime, head)                (Spooled: magic bytes of the head only)
      |
      v
 [forward]   ingest::enqueue_bounded(job_tx, budget, ClipboardJob, 2 s)  (daemon mode; bounded, may shed)
             state.rx_buf = payload                        (Action Mode: `y4p paste-from`)
```

### 2.0 The pipe handed to the compositor (v0.7.7, v0.7.8)

`make_pipe` ([`crates/wayland/src/handlers/data_control/mod.rs`](../crates/wayland/src/handlers/data_control/mod.rs)) creates the pipe whose write end `offer.receive` passes to the compositor. Two properties matter:

- **Dynamic sizing.** `core::pipe::grow` raises the pipe from the kernel default of 64 KiB to 1 MiB (`PIPE_TARGET_BYTES`) with `fcntl(F_SETPIPE_SZ)`, the unprivileged ceiling (`/proc/sys/fs/pipe-max-size`). The request is a hint: a descriptor that is not a pipe is left alone, `EPERM` and `ENOMEM` halve the request and retry (at most 16 attempts), a pipe already large enough is untouched and none is ever shrunk. A pipe that stays at 64 KiB works exactly as before, in more rounds. The same helper sizes the paste pipe (`write_bytes` above 64 KiB, `transfer` for descriptor payloads), the converter child's stdin and stdout, and the `tee(2)` scratch pipe. The effect, measured with both ends in one process: context switches fall from 31 to 1 per MiB, and the paste direction is about 35 % faster ([`PERFORMANCE.md`](../PERFORMANCE.md) section 7.3).
- **Allocation can fail.** `pipe(2)` returns `EMFILE` or `ENFILE` once descriptors run out. Until v0.7.7 the handler returned silently; since v0.7.8 the selection is **counted** (`IngestStats::record_shed_reader`, shown as `shed_readers=` in `y4p status`) and logged as `could not allocate pipe descriptors (EMFILE)`, with the same power-of-two log throttling as every other shed. The reader permit taken a moment earlier is released by the early return, so a descriptor shortage cannot also leak a gate slot.

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

`device.rs` wraps it as `read_pipe_payload(reader, mime, limit)` (returning `Option`, as before) and supplies the hint: a Wayland pipe carries no length, so a text-like MIME reserves one page up front (`INLINE_PAYLOAD_THRESHOLD_BYTES`) and anything else starts empty. That turns the 3-9 allocations of a growing buffer into one for a typical payload ([`PERFORMANCE.md`](../PERFORMANCE.md) section 7.1).

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
| `Payload` | `Memory(Vec<u8>)`, `Stream(StreamDescriptor)`, `File(PathBuf)` or `Spooled(Box<SpooledFile>)`. `len_hint()` needs no I/O; `try_clone()` duplicates a source that is served more than once (not a `Spooled` one: it owns its file); `read_bounded(limit)` materialises it |
| `PayloadReader` | `Read` over the file- and memory-backed variants by `enum` dispatch: no `Box<dyn Read>`, no allocation. `fd()` / `raw_fd()` return `None` for an in-memory reader, never panic |
| `route_payload(mime, len_hint)` | `Inline` or `Stream`, described below |

Routing rule, cheapest evidence first:

1. A **length hint decides**: at most `INLINE_PAYLOAD_THRESHOLD_BYTES` (4096, one page) is `Inline`, anything larger is `Stream`, whatever the MIME says.
2. **No hint: the MIME is the prior.** Text-like types (`is_text_like_mime`) are `Inline`; images and unknown binaries are `Stream`.

> **Invariant — a hint is advisory, never trusted.** It pre-sizes a buffer and feeds the routing guess, but every reader still stops at its own limit and at EOF. A peer that under- or over-states a length can neither truncate a payload nor make the daemon over-read; a wrong guess only picks the slower of two correct paths.

`Payload::Memory` is handed over, not copied: `read_bounded` validates it and returns the same `Vec` (a unit test asserts pointer equality). Ingestion sends `Memory` for text and small payloads and `Spooled` (a fourth variant, section 2.4) for large non-text ones; `Stream` and `File` remain the vocabulary of egress and of any future producer, and the worker materialises them under the same `MAX_PAYLOAD_BYTES` bound.

### 2.4 Streaming ingestion: head, `splice(2)`, `tee(2)` and the spool (v0.7.3)

A screenshot or a GIF does not need to be a heap buffer: the daemon only has to *store* it, and the kernel can move a pipe's pages into a file without the bytes entering this process. The mechanism, end to end:

```text
 pipe (compositor client)                                   daemon worker
   |                                                              ^
   |  1. head: read_to_end(take(4096)) into one pre-sized Vec     |
   |       ends inside the head?  -> Memory(head)  (small: done)  |
   v                                                              |
 [PayloadSpooler::spool(head, pipe, limit)]   (trait in y4p-wayland, implemented by y4p-daemon)
   |  2. create spool/<pid>-<n>.part  (0600, O_EXCL, in a 0700 dir)
   |  3. write(head); sha3.update(head)
   |  4. core::splice::copy_to_file(pipe, file, limit - head, sha3):
   |        per 64 KiB round   tee(pipe -> scratch)      copy refs, nothing consumed
   |                           splice(pipe -> file)      the same bytes, kernel to page cache
   |                           read(scratch -> stack)    only to feed sha3
   v
 SpooledFile { path, len, digest }  --ClipboardJob--> bounded queue --> worker
                                                                         |
   route_file_for_ingest(mime, head512): would the converter rewrite it?
        --yes, and breaker + throttle allow--> magick: spool file as stdin, stdout spliced into a
        |                                      second spool file, sha3 over it (v0.7.4); the first is deleted
        --no, or skipped, or failed----------> the original spool file, untouched
                                    |
                                    v
                       map file, rename into the cache, insert from the map
```

**Why the Wayland crate does not hash or touch the cache.** `PayloadSpooler` ([`crates/wayland/src/spool.rs`](../crates/wayland/src/spool.rs)) is the whole interface: "drain this pipe into a file, return where it is and what it hashes to". The implementation, `FileSpooler` ([`crates/daemon/src/spool.rs`](../crates/daemon/src/spool.rs)), owns the directory, the file names and SHA3-256; `y4p-wayland` still depends on neither `y4p-storage`, `y4p-image` nor `sha3`. Without a spooler (Action Mode, tests) the ingestion thread behaves exactly as in v0.7.2.

**Why a head is read first.** A Wayland pipe has no length, so the heuristic of 2.3 cannot see a size. The ingestion thread reads one inline threshold (4096 bytes) into a single pre-sized buffer: if the stream ends inside it the payload is small and travels as `Memory` (no file, no extra syscall beyond that read, one allocation); only a stream that fills the head is spooled. Text-like MIMEs never take this road: they are normalised and indexed as bytes anyway. The head is written to the spool file first and fed to the digest, so the file holds the whole stream and the limit (`MAX_PAYLOAD_BYTES`) counts the head too.

**The two copy paths** (`core::splice::copy_to_file`, [`crates/core/src/splice.rs`](../crates/core/src/splice.rs)):

| Path | When | What moves the bytes | User-space copy of the payload |
| :--- | :--- | :--- | :--- |
| Kernel | the source is a FIFO (checked with `fstat`) | `tee(2)` + `splice(2)` into the file; the digest reads the `tee`d duplicate through a 64 KiB stack buffer | none that is stored; one read-out for the hash |
| Buffered | any other source, or the kernel refuses (`EINVAL`, `ENOSYS`, `EOPNOTSUPP`: a socket, an `O_APPEND` file, a filesystem without splice) | `read` into a fixed 64 KiB stack buffer, hash, `write` | one, through the stack, never the heap |

The switch happens only *between* rounds or before a round has consumed anything: `tee` does not consume its source, so when the `splice` after it is refused the duplicate is discarded and the buffered path resumes at the same position. The digest therefore sees every byte exactly once and in order on every path (tested by forcing exactly that refusal with an `O_APPEND` destination). `ByteTap::OBSERVES` is an associated constant, so with `NoTap` the scratch pipe and the `tee` are not even created.

> **Invariant — bounded, never truncated.** One byte beyond `limit` is let through as a probe, as in `read_bounded`: an exact fit succeeds, an overflow is `TooLarge` and the partial file is deleted. Whatever fails, `FileSpooler` unlinks the file it created, and a completed `SpooledFile` unlinks itself on drop: a payload that is shed from a full queue, rejected by the worker or abandoned by an error leaves nothing on disk. Only `SpooledFile::into_path` (adoption) disarms that.

**What the worker does with it** (`store_spooled`, [`crates/daemon/src/worker.rs`](../crates/daemon/src/worker.rs)):

1. The first 512 bytes go to `y4p_image::route::route_file_for_ingest`, which decides with the same classification as the in-memory road (one private function, `plan`, serves both, so they cannot disagree) and runs under the same circuit breaker and process throttle. A raster image the converter would rewrite is converted **file to file** (v0.7.4 Part 1, [06](06_local_image_pipeline.md) section 6.4): `magick` reads the spool file as its stdin and its stdout is spliced into a new spool file next to it (`<pid>-<n>-t.part`) while SHA3-256 runs over the output. The new file, with the digest of *its* bytes and the converter's MIME, replaces the original, which is unlinked. If the conversion is skipped (open circuit, no free slot, no `magick`) or fails (undecodable image, timeout, output over the limit), the original file carries on untouched and the half-written one is unlinked. Nothing in this step is read into the heap.
2. The file that results, converted or not, is handed to storage whole (`ClipboardDb::insert_spooled`, v0.7.4 Part 2), keyed by the digest computed while it streamed in: it is **not hashed again**. Storage alone decides what the file becomes, so the worker holds no memory mapping of it and no copy of storage's layout rules.
3. A payload storage keeps in a cache file (`image/*`, `*gif*`, and no text preview needed) is **adopted** by `FileCache::adopt`: the spool file is renamed to `<cache>/<hash>.cache` (an integration test compares inodes), and its row is built from the length alone, so no byte is read. Never over an existing file: if the cache already holds the hash the entry is kept and the spool file is unlinked. The file is placed before the row, so a row never points at a missing file; if the row then fails to commit, the file this call placed is removed again, and a sensitive or empty payload is declined before any file is touched. If the rename is refused (`EXDEV`), the bytes are copied in the kernel and the source unlinked.
4. Every other payload (a PDF, an archive) is read once into memory, checked against its recorded length, stored in its row, and the spool file unlinked. This is the one place a large spooled payload meets the heap again: the row needs the bytes, and the earlier mapping only moved the copy into SQLite's own buffer.

`ClipboardDb::insert_spooled` consumes the `SpooledFile` on every path: it ends up as the cache entry or unlinked, and the caller never cleans up. The worker no longer mirrors any storage rule (`core::cache_file_path` and `worker::stored_in_cache_file` are gone, and so is `core::mapped`, the tree's only `mmap`/`munmap` `unsafe`).

**Resource accounting.** A reader that spools holds a pipe read end, the spool file and the two ends of the scratch pipe: four descriptors, at most `INGEST_MAX_CONCURRENT_READERS` (8) readers, so 32. The queue carries paths, not descriptors: no descriptor waits in it. The heap cost of a queued spooled job is the box and the path (about 160 bytes, measured), whatever the payload's size; the cost moves to disk. Since v0.7.8 the **queued** part of that is bounded in bytes: a spool file counts against the 64 MiB queue byte budget exactly as a heap buffer would (section 6, [03](03_concurrency_and_memory.md) section 2.1). The readers' own files (8 x 256 MiB) and one more file while the worker converts an image (the converted output; the original is unlinked as soon as it is in place) come on top, so the disk ceiling is 64 MiB + 8 x 256 MiB + 256 MiB, not the former (64 + 8 + 1) x 256 MiB ([`PERFORMANCE.md`](../PERFORMANCE.md) section 5.2). A cache wipe removes the spool directory with the rest of the cache; the spooler re-creates it on the next payload, and a directory that cannot be created hands the pipe back for an in-memory read, so spooling is an optimisation that can fail without losing a clipboard entry. Leftover `.part` files of a dead process are swept when the daemon starts; a live process's files, and any other file, are never touched.

---

## 3. Why `provider_locks` Exists

`ext-data-control-v1` cannot tell "the user copied something new" apart from "the compositor is re-offering content the daemon itself just set". Without a way to distinguish them, `copy-to` would immediately trigger the daemon's own ingestion handler, re-hash and re-store the content it just placed on the clipboard: a feedback loop that only touches a timestamp on every restore.

`y4p` closes the loop with a plain counter. Before handing data to the compositor, restore increments it:

```rust
// crates/daemon/src/lib.rs:L223 — handle_restore_request
state.provider_locks += 1;
```

and the ingestion handler checks it first, before doing any work:

```rust
// crates/wayland/src/handlers/data_control/device.rs:L30
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

`paste_from_os` ([`crates/wayland/src/lib.rs:L30`](../crates/wayland/src/lib.rs)) uses the same `WaylandState` with `job_tx = None` and a `target_mime`. It runs a poll-based loop with a 200 ms sub-timeout under a 1 s absolute watchdog, and performs the same bounded read and normalisation synchronously into `state.rx_buf`.

> **Invariant — one boundary guard, two consumers.** `paste-from` returns exactly the bytes the daemon would have forwarded to the worker (before image routing, which is a persistence concern).

---

## 6. The Stream I/O Contract Between Crates

| Hand-off | From -> To | Type | Guarantee |
| :--- | :--- | :--- | :--- |
| Ingress | `y4p-wayland` -> `y4p-daemon` | `mpsc::SyncSender<ClipboardJob>` (capacity `INGEST_QUEUE_CAPACITY` = 64) and a `ByteBudget` (`INGEST_QUEUE_BYTE_BUDGET` = 64 MiB), `ClipboardJob { mime, payload: Payload, permit: Option<BytePermit> }` | the payload is non-empty and at most `MAX_PAYLOAD_BYTES` (`Memory`: once materialised, `ClipboardJob::into_bytes`, a no-copy hand-over; `Spooled`: a complete file with its SHA3-256, owned and unlinked on drop), already normalised or relabelled; no conversion. A queued job holds a `BytePermit` for its length, returned when the job is dropped; a payload above the whole budget never enters the queue |
| Spool | `y4p-wayland` -> `y4p-daemon` | `PayloadSpooler::spool(head, pipe, limit) -> SpoolOutcome` | `Spooled` (complete, fingerprinted), `Rejected` (nothing left on disk) or `Unavailable(pipe)` (untouched beyond the head, read it into memory) |
| Persistence | `y4p-daemon` -> `y4p-storage` | `insert_with_hash(mime, data, hash, max_history)`, `insert_spooled(...)` | `hash` is SHA3-256 over the final bytes (see [03](03_concurrency_and_memory.md)); the worker drops the job's byte permit once the call has returned |
| Egress | `y4p-storage` -> `y4p-daemon` -> `y4p-wayland` | `ContentLocation` -> `core::payload::Payload` (`SourceMetadata.payload`) | Large payloads are passed as a path, never materialised |

> **Invariant — `y4p-wayland` is a pure sensor.** It depends on neither `y4p-storage`, `y4p-image` nor `sha3`; it never converts, fingerprints or persists a payload.
