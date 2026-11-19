# 03 — Concurrency & Memory Reclamation

The daemon's poll loop ([01](01_wayland_streaming_io.md)) has to stay responsive to new Wayland events at all times. Disk writes, optional image conversion and hashing are exactly the kind of work that would stall it if they ran inline. This document covers how `y4p` keeps that work off the critical path, and how it keeps a long-lived process from accumulating memory it no longer needs.

Crates involved: `y4p-daemon` (`worker.rs`, `metrics.rs`, event loop), `y4p-storage` (connection handles), `y4p-core` (exit flag).

---

## 1. Why Every Database Write Goes Through One Worker Thread

SQLite allows exactly one writer at a time. A second connection that tries to write while another transaction is open gets `SQLITE_BUSY`. Retry-with-backoff turns that into latency spikes and, under enough contention, write failures.

`y4p` avoids the problem structurally: only one thread in the process ever holds a write connection.

```rust
// crates/daemon/src/worker.rs
pub struct DbWorker {
    tx: mpsc::SyncSender<ClipboardJob>,
    db: Arc<Mutex<ClipboardDb>>,
}

pub fn spawn(db: ClipboardDb, metrics: Arc<DaemonMetrics>, verbose: bool, max_history: usize, sanitize_images: bool) -> Self {
    let (tx, rx) = mpsc::sync_channel::<ClipboardJob>(INGEST_QUEUE_CAPACITY);
    let db = Arc::new(Mutex::new(db));
    let writer = Arc::clone(&db);

    std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            let ClipboardJob { mime, payload } = job;
            let stored = match writer.lock() {
                Ok(mut db) => store_materialised(&mut db, ClipboardJob::from_payload(mime, payload), max_history, sanitize_images),
                Err(_) => Err("database lock poisoned; payload dropped".to_string()),
            };
            /* metrics + logging */
        }
    });

    Self { tx, db }
}
```

The connection is not moved into the thread; it is shared with the event loop through the `DbWorker` (section 2.2). The mutex, not ownership, is what makes "one writer" true, and the worker takes it once per job: a job is materialised, optionally routed, hashed and persisted without releasing it, so two jobs can never interleave either.

Every other part of the daemon that wants something persisted builds a `ClipboardJob` and offers it to a cloned `mpsc::SyncSender` (bounded; see section 2.1). The event loop does not send jobs: for `delete` and `wipe` it calls the worker's own methods, which take the same mutex.

> **Invariant — every SQLite write happens under the writer's mutex.** Ingress jobs, IPC `delete` and IPC `wipe` all lock the one connection, so SQLite never sees a second writer and no write can overtake another. The trade-off is symmetry: a mutation waits behind whatever job is being persisted, and a job waits behind a mutation.

```text
   Wayland Selection event
        |
        v  (per-selection thread, at most 8: bounded read + normalise)
   ClipboardJob { mime, payload }
        |
        v
   bounded channel (64 slots)  -->  DbWorker thread  -->  SQLite (single writer, WAL)
        |                               ^
        |                               |
        +- process_job: optional        | readers never block the writer
           image route, SHA3-256        |
                                        +---- read_db (list / search / restore lookups)

   the main poll loop keeps running and never waits on this send
```

---

## 2. What the Worker Does Besides Writing

The worker owns the CPU-heavy half of ingestion. `process_job` ([`worker.rs`](../crates/daemon/src/worker.rs)) takes a normalised raw payload and returns `(mime, data, hash)`:

```text
   ClipboardJob { mime, payload }  --materialise (no copy for Memory)-->  (mime, data)
        |
        v
   if [image] sanitize = true:
       y4p_image::route::route_for_ingest(mime, data)   PNG/BMP/JPEG/WebP -> lossless PNG,
        |                                               everything else unchanged; original on failure
        v
   fingerprint(&data)                                   lowercase-hex SHA3-256 over the FINAL bytes
        |
        v
   db.insert_with_hash(&mime, &data, &hash, max_history)
```

| Property | Consequence |
| :--- | :--- |
| Sanitisation is opt-in | The default (`sanitize = false`) stores the raw bytes and never spawns `magick`; the hash then covers exactly what arrived |
| Routing before hashing (when enabled) | The fingerprint always describes what the database and cache really store |
| One consumer thread | Jobs are persisted strictly in arrival order; a fast text copy can no longer overtake a slow image copy |
| Conversion runs on the worker | The wayland ingestion thread never spawns `magick` or hashes; the cost is that a slow conversion delays the jobs queued behind it |
| Conversion is bounded | Timeout, process throttle and circuit breaker cap how long and how often ([06](06_local_image_pipeline.md)) |

### 2.1 Bounded Queue and Load Shedding

Because conversion (when enabled) runs on the worker, a slow image delays every job behind it. Ingestion therefore applies two bounds, in this order ([`crates/wayland/src/ingest.rs`](../crates/wayland/src/ingest.rs)): how many readers, then how many jobs.

```text
   Selection event
        |
        v
   ReaderGate::try_acquire()  --- full (8 readers) ---> shed (counted), NO pipe, NO thread
        |  permit
        v
   make_pipe()  --- EMFILE ---> shed (counted, logged), permit released
        |
        v  reader thread: DeadlineReader + bounded read + normalise
        |
        v
   enqueue(): poll every 5 ms, for up to 2 s, for a queue slot
        |                                  |
     queued                       still full after 2 s
        |                                  |
   worker persists the job      shed (counted), job dropped
```

| Mechanism | Constant | Effect |
| :--- | :--- | :--- |
| Reader gate | `INGEST_MAX_CONCURRENT_READERS` = 8 | Checked **before** the pipe is created, so a burst of selections can neither exhaust file descriptors (`EMFILE`) nor threads. A refused selection costs one atomic compare-and-swap. A pipe that still cannot be allocated is counted as a shed reader and logged |
| Bounded channel | `INGEST_QUEUE_CAPACITY` = 64 | `sync_channel`; a full queue hands the job back (`TrySendError::Full`), so retrying never clones the payload |
| Patience | `INGEST_ENQUEUE_WAIT_MS` = 2000 | A reader that finds no room waits **while keeping its gate permit**: this is the backpressure, because it makes new selections hit the gate instead of piling payloads up in memory |
| Shedding | counters `shed_readers`, `shed_queue`, `shed_timeout` | Once patience is spent the payload is dropped. Totals appear in `y4p status` and the first, second, fourth, eighth, ... shed is logged, with a reason: "too many concurrent readers", "could not allocate pipe descriptors (EMFILE)", "pipe read timed out" or "worker queue full" |

The queue is a count bound, and the count is a memory bound by construction: every payload is at most `MAX_PAYLOAD_BYTES` (32 MiB), so the worst case is 64 queued buffers. The earlier byte-budget mechanism (`ByteBudget` / `BytePermit` / `enqueue_bounded`) was removed with the spool path in v0.9.1; there is no longer a second admission rule to keep in step.

> **Invariant — overload loses the newest copy, never the queued ones and never the event loop.** Waiting happens on the reader's own thread; the Wayland poll loop only ever performs the gate's compare-and-swap. A shed is always counted and surfaced; a worker that has gone away is *not* counted as shedding. A thread that cannot be spawned (`Builder::spawn` returns an error instead of panicking) is also shed, and dropping its closure frees the pipe and the permit.

Why drop the newest rather than evict the oldest: `std` offers no way to remove from the middle of a channel, and keeping queued work intact preserves arrival order, which the worker guarantees.

### 2.2 IPC Mutations Share the Writer

`delete` and `wipe` run in the daemon process, against the writer connection ([`crates/daemon/src/worker.rs`](../crates/daemon/src/worker.rs)):

| Piece | Behaviour |
| :--- | :--- |
| `DbWorker::delete_by_id(id)` | `Ok(true)` / `Ok(false)` for "removed" / "no such row"; runs `ClipboardDb::delete_by_id` under the writer's mutex |
| `DbWorker::wipe()` | Runs the whole `ClipboardDb::wipe()` under the same mutex, so `VACUUM`, `PRAGMA shrink_memory;`, the cache clear and `malloc_trim(0)` all happen in the daemon process, on the daemon's connection, with no ingestion job in flight |
| Poisoned lock | Mapped to `Err(String)` (`database lock poisoned; ...`) and reported as `ERR:<message>`; never `unwrap()`/`expect()` |

The event loop executes a mutation inside `ipc::accept_and_dispatch`, on the poll thread, and writes the reply back on the same `UnixStream`: `OK:true`, `OK:false` or `OK` on success, `ERR:<message>` otherwise (one line, terminated by `IPC_DELIMITER`). The CLI reads one line under `IPC_MUTATION_TIMEOUT_MS` (10 000 ms), much longer than `IPC_STATUS_TIMEOUT_MS` (1000 ms) because a `VACUUM` over a large database legitimately takes seconds.

Only a *failed connect* makes the CLI act on its own handle. Once connected, a timeout or an `ERR:` reply is reported and the command exits non-zero: falling back to a local write while the daemon owns the connection would reintroduce the race this section removes.

> **Invariant — one writer, and its memory is the memory that shrinks.** A mutation either runs in the daemon under the writer's mutex or, when no daemon exists, in the only process that has the file open. `wipe` can never race an ingestion job, and the heap it releases is always the heap that held the pages.

> **Known cost.** The poll thread blocks while a mutation waits for the mutex, and that mutex may be held by a job running `magick` (bounded by `IMAGE_TRANSCODE_TIMEOUT_SECS`, 15 s). Clipboard events queue in the compositor's socket meanwhile; nothing is lost, but a `wipe` issued during a slow conversion is felt as a delay, not as an immediate reply.

---

## 3. WAL Concurrency and the Read-Side Handle

The daemon opens a *second* `ClipboardDb` for reads served inside the daemon process itself (`read_db`, [`lib.rs`](../crates/daemon/src/lib.rs)), rather than sharing the writer's connection. SQLite's WAL mode lets readers proceed concurrently with a writer without blocking either side. Splitting the handle costs nothing, and means a slow read (a restore lookup) can never, even in principle, block clipboard ingestion.

| Handle | Owner | Used for |
| :--- | :--- | :--- |
| Writer `ClipboardDb` | owned by `DbWorker`, locked by its thread and by the event loop (section 2.2) | `insert_with_hash`, `delete_by_id`, `wipe` |
| `read_db` | the daemon's event-loop thread | `locate_content` / `get_content_by_id` for restores |
| CLI process handle | the short-lived CLI process | `list`, `search`, `show`, `pin`, `delete`, ... |

A read-only constructor (`open_read_only`) existed briefly but was never wired into any caller, and was removed in v0.9.3. The single-writer property is upheld by ownership and the mutex above, and WAL keeps reads from blocking the writer.

---

## 4. Memory Lifecycle of a Payload

A long-lived process must not retain what it no longer needs. A payload's buffer has a short, linear life:

| Step | Owner | Allocation behaviour |
| :--- | :--- | :--- |
| 1. Read | per-selection ingestion thread | One `Vec<u8>` filled by `read_to_end` under `DeadlineReader`, at most `MAX_PAYLOAD_BYTES + 1` read; discarded outright if oversized. A text-like MIME reserves one page up front (one allocation instead of 3-9; [`history/PERFORMANCE.md`](history/PERFORMANCE.md) section 7.1) |
| 2. Normalise | same thread | `text/uri-list` produces one new `Vec`; everything else is moved, not copied |
| 3. Hand-off | bounded channel | The `ClipboardJob` is **moved**; its `Payload::Memory` is handed to the worker without a copy; the bytes are never cloned. A full queue sheds instead of growing (section 2.1) |
| 4. Route (opt-in) | `DbWorker` | Text and unsupported types: moved straight through. Images when `sanitize = true`: output buffer pre-sized from the input length (capped at `IMAGE_OUTPUT_HINT_CAP_BYTES`, 16 MiB); on success the original `Vec` is dropped. When `sanitize = false`, this step is skipped entirely |
| 5. Persist | `DbWorker` | `insert_with_hash` borrows `&data`; text is bound as `&str` with no copy |
| 6. Reclaim | `DbWorker` | `mime`, `data`, `hash` drop at the end of the loop iteration |

Egress is lighter still: a cached image goes through `sendfile(2)` and never enters the heap ([01](01_wayland_streaming_io.md) section 4).

> **Invariant — no ingested payload outlives its job.** The daemon holds no clipboard history in memory. Everything durable is in SQLite or the cache directory; process memory returns to its idle baseline once the queue drains. The one deliberate exception is egress: while the daemon owns the selection after a `copy-to`, the restored payload (a text buffer, or just a path for a cached image) stays attached to the active data source until the compositor cancels it.

During a sanitising conversion the original and the converted image coexist briefly (the peak is roughly the sum of the two), bounded by the 32 MiB read cap and the converter's own memory limits.

---

## 5. Lock-Free Counters and Exit Coordination

**Metrics.** `DaemonMetrics` ([`crates/daemon/src/metrics.rs`](../crates/daemon/src/metrics.rs)) holds `AtomicU64` ingress/egress counters and an `AtomicI64` last-event timestamp, written with `Ordering::Relaxed` from the worker and read from the event loop when IPC `Status` arrives. No mutex anywhere in the type: the counters are independent statistics, so no ordering between them is required. The status line also carries `shed_readers=`, `shed_queue=` and `shed_timeout=`, owned by `y4p-wayland` (`IngestStats`) and passed in by the event loop.

**Exit.** `SIG_EXIT` is a single `AtomicBool` in `y4p-core` shared by `main.rs`'s `ctrlc` handler and the daemon loop. The handler only sets the flag (a second Ctrl+C force-exits with status 1). The loop re-checks `is_exiting()` at least every 500 ms because of its `poll` timeout, so no socket round-trip is needed to wake it. A non-daemon invocation exits with status 130 on SIGINT and never touches the daemon's socket.

> **Invariant — interrupting a one-shot command never stops the resident daemon.**
