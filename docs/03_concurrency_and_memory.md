# 03 — Concurrency & Memory Reclamation

The daemon's poll loop ([01](01_wayland_streaming_io.md)) has to stay responsive to new Wayland events at all times. Disk writes, image conversion and hashing are exactly the kind of work that would stall it if they ran inline. This document covers how `y4p` keeps that work off the critical path, and how it keeps a long-lived process from accumulating memory it no longer needs.

Crates involved: `y4p-daemon` (`worker.rs`, `metrics.rs`, event loop), `y4p-storage` (connection handles), `y4p-core` (exit flag).

---

## 1. Why Every Database Write Goes Through One Worker Thread

SQLite allows exactly one writer at a time. A second connection that tries to write while another transaction is open gets `SQLITE_BUSY` ("database is locked"). Retry-with-backoff turns that into latency spikes and, under enough contention, write failures.

`y4p` avoids the problem structurally rather than defensively: only one thread in the process ever holds a write connection.

```rust
// crates/daemon/src/worker.rs:L30-L49
pub fn spawn(mut db: ClipboardDb, metrics: Arc<DaemonMetrics>, verbose: bool, max_history: usize) -> Self {
    let (tx, rx) = mpsc::sync_channel::<ClipboardJob>(INGEST_QUEUE_CAPACITY);

    std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            let (mime, data, hash) = process_job(job.mime, job.data);
            match db.insert_with_hash(&mime, &data, &hash, max_history) {
                Ok(_) => { metrics.record_ingress(); /* ... */ }
                Err(e) => eprintln!("{}worker failed to persist data: {}", LOG_ERROR, e),
            }
        }
    });

    Self { tx }
}
```

`ClipboardDb` moves *into* the spawned thread and is never shared. Rust's ownership model turns "only this thread can write" into a compile-time fact rather than a convention someone must remember at each call site. Every other part of the daemon that wants something persisted builds a `ClipboardJob` and offers it to a cloned `mpsc::SyncSender` (bounded; see section 2.1).

> **Invariant — the channel is the only coupling between "something happened on the clipboard" and "something was written to disk".** There is nothing on the other side of it to conflict with, so it cannot produce a lock conflict.

```text
   Wayland Selection event
        |
        v  (per-selection thread, at most 8: bounded read + normalise)
   ClipboardJob { mime, data }
        |
        v
   bounded channel (64)  -->  DbWorker thread  -->  SQLite (single writer, WAL)
        |                 |                         ^
        |                 +- process_job:           | readers never block the writer
        |                    image route, SHA3-256  |
        +- main poll loop keeps running,            |
           never waits on this send       read_db --+  (list / search / restore lookups)
```

---

## 2. What the Worker Does Besides Writing (v0.6.7)

Since v0.6.7 the worker owns the CPU-heavy half of ingestion. `process_job` ([`worker.rs:L71`](../crates/daemon/src/worker.rs)) takes a normalised raw payload and returns `(mime, data, hash)`:

```text
   ClipboardJob { mime, data }
        |
        v
   y4p_image::route::route_for_ingest(mime, data)   PNG/BMP/JPEG/WebP -> lossless PNG,
        |                                           everything else unchanged; original on any failure
        v
   fingerprint(&data)                               lowercase-hex SHA3-256 over the FINAL bytes
        |
        v
   db.insert_with_hash(&mime, &data, &hash, max_history)
```

| Property | Consequence |
| :--- | :--- |
| Routing before hashing | The fingerprint always describes what the database and cache really store |
| One consumer thread | Jobs are persisted strictly in arrival order; a fast text copy can no longer overtake a slow image copy |
| Conversion runs on the worker | The wayland ingestion thread never spawns `magick` or hashes; the cost is that a slow conversion delays the jobs queued behind it |
| Conversion is bounded | Timeout, process throttle and circuit breaker cap how long and how often ([06](06_local_image_pipeline.md)) |

### 2.1 Bounded Queue, Backpressure and Load Shedding (v0.7.1)

Because conversion runs on the worker, a slow image delays every job behind it. The queue therefore has two bounds, applied in this order ([`crates/wayland/src/ingest.rs`](../crates/wayland/src/ingest.rs)):

```text
   Selection event
        |
        v
   ReaderGate::try_acquire()  --- full (8 readers) ---> shed (counted), NO pipe, NO thread
        |  permit
        v
   make_pipe()  ->  reader thread: bounded read + normalise
        |
        v
   enqueue(): try_send into the sync_channel(64), polling every 5 ms for up to 2 s
        |                         |
      queued                    still full after 2 s
        |                         |
   permit released           shed (counted), payload dropped, permit released
```

| Mechanism | Constant | Effect |
| :--- | :--- | :--- |
| Reader gate | `INGEST_MAX_CONCURRENT_READERS` = 8 | Checked **before** the pipe is created, so a burst of selections can neither exhaust file descriptors (`EMFILE`) nor threads. A refused selection costs one atomic compare-and-swap |
| Bounded channel | `INGEST_QUEUE_CAPACITY` = 64 | `sync_channel`; a full queue hands the job back (`TrySendError::Full`), so retrying never clones the payload |
| Patience | `INGEST_ENQUEUE_WAIT_MS` = 2000 | A reader whose queue is full waits **while keeping its gate permit**: this is the backpressure, because it makes new selections hit the gate instead of piling payloads up in memory |
| Shedding | counters `shed_readers`, `shed_queue` | Once patience is spent the payload is dropped. Totals appear in `y4p status` and the first, second, fourth, eighth, ... shed is logged |

> **Invariant — overload loses the newest copy, never the queued ones and never the event loop.** Waiting happens on the reader's own thread; the Wayland poll loop only ever performs the gate's compare-and-swap. A shed is always counted and surfaced; a worker that has gone away is *not* counted as shedding. A thread that cannot be spawned (`Builder::spawn` returns an error instead of panicking) is also shed, and dropping its closure frees the pipe and the permit.

Why drop the newest rather than evict the oldest: `std` offers no way to remove from the middle of a channel, and keeping queued work intact preserves arrival order, which the worker guarantees.

> **Known trade-off — the bound is a count, not a byte budget.** 64 queued payloads of 256 MiB would still be 16 GiB; the measured and worst-case figures, and the proposed byte budget, are in [`PERFORMANCE.md`](../PERFORMANCE.md) section 4.

---

## 3. WAL Concurrency and the Read-Side Handle

The daemon opens a *second* `ClipboardDb` for reads served inside the daemon process itself (`read_db`, [`lib.rs:L89`](../crates/daemon/src/lib.rs)), rather than sharing the writer's connection. SQLite's WAL mode lets readers proceed concurrently with a writer without blocking either side. Splitting the handle costs nothing, and means a slow read (a restore lookup) can never, even in principle, block clipboard ingestion.

| Handle | Owner | Used for |
| :--- | :--- | :--- |
| Writer `ClipboardDb` | moved into the `DbWorker` thread | `insert_with_hash` only |
| `read_db` | the daemon's event-loop thread | `locate_content` / `get_content_by_id` for restores |
| CLI process handle | the short-lived CLI process | `list`, `search`, `show`, `pin`, `delete`, ... |

---

## 4. Why `open_read_only` Exists: From Convention to Enforcement

Everything above describes a *design*: only the worker's handle writes, and `read_db` exists purely to read. That guarantee lived in the structure of the Rust code. Nothing was wrong with it: ownership already makes sharing the writer's handle impossible, and `read_db`'s call path only runs `SELECT`. But it held because every call site happened to behave, not because SQLite would refuse otherwise. A future change that reaches for `UPDATE` on a read path would not be stopped.

`ClipboardDb::open_read_only()` ([`crates/storage/src/lib.rs:L105`](../crates/storage/src/lib.rs)) closes that gap beneath Rust's type system:

```rust
Connection::open_with_flags(
    &db_path,
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
)
```

`SQLITE_OPEN_READ_ONLY` is not a Rust-level promise; it instructs the engine to reject any `INSERT`, `UPDATE` or `DELETE` on this connection before it touches a page. `SQLITE_OPEN_NO_MUTEX` skips SQLite's internal connection locking, which a connection only ever used by one thread does not need. There is deliberately no `SQLITE_OPEN_CREATE`: the writer (`open`) must already have created and migrated the file.

```text
   open()                                   open_read_only()

   read-write connection:                   READ_ONLY | NO_MUTEX:
   "please only SELECT" is a convention     a stray UPDATE / DELETE / INSERT
                                            is rejected by SQLite itself
```

> **Current status.** The constructor exists but is **not yet called** by `read_db` or any CLI read path; they still open through the ordinary read-write `open()`. Wiring them over is tracked by the `TODO(v0.3.0)` marker at [`crates/storage/src/lib.rs:L103`](../crates/storage/src/lib.rs). Until then, the single-writer property is enforced by ownership and review, not yet by the engine.

---

## 5. Memory Lifecycle of a Payload

A long-lived process must not retain what it no longer needs. A payload's buffer has a short, linear life:

| Step | Owner | Allocation behaviour |
| :--- | :--- | :--- |
| 1. Read | per-selection ingestion thread | One `Vec<u8>` filled by `read_to_end`, at most `MAX_PAYLOAD_BYTES + 1` read; discarded outright if oversized |
| 2. Normalise | same thread | `text/uri-list` produces one new `Vec`; everything else is moved, not copied |
| 3. Hand-off | bounded channel | The `ClipboardJob` is **moved**; the bytes are never cloned. A full queue sheds instead of growing (section 2.1) |
| 4. Route | `DbWorker` | Text and unsupported types: moved straight through. Images: output buffer pre-sized from the input length (capped at `IMAGE_OUTPUT_HINT_CAP_BYTES`, 16 MiB) to avoid repeated doubling; on success the original `Vec` is dropped |
| 5. Persist | `DbWorker` | `insert_with_hash` borrows `&data`; text is bound as `&str` with no copy |
| 6. Reclaim | `DbWorker` | `mime`, `data`, `hash` drop at the end of the loop iteration |

Egress is lighter still: a cached image goes through `sendfile(2)` and never enters the heap ([01](01_wayland_streaming_io.md) section 4).

> **Invariant — no ingested payload outlives its job.** The daemon holds no clipboard history in memory. Everything durable is in SQLite or the cache directory; process memory returns to its idle baseline once the queue drains. The one deliberate exception is egress: while the daemon owns the selection after a `copy-to`, the restored payload (an `Owned` text buffer, or just a path for a cached image) stays attached to the active data source until the compositor cancels it.

During a conversion the original and the converted image coexist briefly (the peak is roughly the sum of the two), bounded by the 256 MiB read cap and the converter's own memory limits.

---

## 6. Lock-Free Counters and Exit Coordination

**Metrics.** `DaemonMetrics` ([`crates/daemon/src/metrics.rs`](../crates/daemon/src/metrics.rs)) holds `AtomicU64` ingress/egress counters and an `AtomicI64` last-event timestamp, written with `Ordering::Relaxed` from the worker and read from the event loop when IPC `Status` arrives. No mutex anywhere in the type: the counters are independent statistics, so no ordering between them is required. The status line also carries `shed_readers=` and `shed_queue=`, the overload totals of [section 2.1](#21-bounded-queue-backpressure-and-load-shedding-v071), owned by `y4p-wayland` (`IngestStats`) and passed in by the event loop because the daemon's metrics type cannot see the sensor layer's state.

**Exit.** `SIG_EXIT` is a single `AtomicBool` in `y4p-core` shared by `main.rs`'s `ctrlc` handler and the daemon loop. The handler only sets the flag (a second Ctrl+C force-exits with status 1). The loop re-checks `is_exiting()` at least every 500 ms because of its `poll` timeout, so no socket round-trip is needed to wake it. A non-daemon invocation exits with status 130 on SIGINT and never touches the daemon's socket.

> **Invariant — interrupting a one-shot command never stops the resident daemon.**
