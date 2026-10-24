# 03 — Concurrency & Memory Reclamation

The daemon's poll loop ([01](01_wayland_streaming_io.md)) has to stay responsive to new Wayland events at all times. Disk writes, image conversion and hashing are exactly the kind of work that would stall it if they ran inline. This document covers how `y4p` keeps that work off the critical path, and how it keeps a long-lived process from accumulating memory it no longer needs.

Crates involved: `y4p-daemon` (`worker.rs`, `metrics.rs`, event loop), `y4p-storage` (connection handles), `y4p-core` (exit flag).

---

## 1. Why Every Database Write Goes Through One Worker Thread

SQLite allows exactly one writer at a time. A second connection that tries to write while another transaction is open gets `SQLITE_BUSY` ("database is locked"). Retry-with-backoff turns that into latency spikes and, under enough contention, write failures.

`y4p` avoids the problem structurally rather than defensively: only one thread in the process ever holds a write connection.

```rust
// crates/daemon/src/worker.rs:L52-L80
pub fn spawn(mut db: ClipboardDb, metrics: Arc<DaemonMetrics>, verbose: bool, max_history: usize) -> Self {
    let (tx, rx) = mpsc::sync_channel::<ClipboardJob>(INGEST_QUEUE_CAPACITY);

    std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            let ClipboardJob { mime, payload, permit } = job;
            let stored = match payload {
                Payload::Spooled(spooled) => store_spooled(&mut db, mime, *spooled, max_history),
                payload => store_materialised(&mut db, ClipboardJob::from_payload(mime, payload), max_history),
            };
            drop(permit); // persisted or discarded: the bytes leave the queue's budget
            match stored {
                Ok(Some((mime, len))) => { metrics.record_ingress(); /* ... */ }
                Ok(None) => {}
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
   ClipboardJob { mime, payload, permit }
        |
        v
   bounded channel (64 slots, 64 MiB)  -->  DbWorker thread  -->  SQLite (single writer, WAL)
        |                 |                         ^
        |                 +- process_job:           | readers never block the writer
        |                    image route, SHA3-256  |
        |                    (or, for a spooled     |
        |                     payload: adopt file)  |
        +- main poll loop keeps running,            |
           never waits on this send       read_db --+  (list / search / restore lookups)
```

---

## 2. What the Worker Does Besides Writing (v0.6.7)

Since v0.6.7 the worker owns the CPU-heavy half of ingestion. `process_job` ([`worker.rs`](../crates/daemon/src/worker.rs)) takes a normalised raw payload and returns `(mime, data, hash)`:

```text
   ClipboardJob { mime, payload, permit }  --materialise (no copy for Memory)-->  (mime, data)
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

A *spooled* payload (v0.7.3; a large non-text one the ingestion thread already drained to a file with `splice(2)`, [01](01_wayland_streaming_io.md) section 2.4) takes `store_spooled` instead. If the image router says the converter would rewrite it, `magick` is run on the file itself (v0.7.4 Part 1: the spool file is its stdin, its stdout is spliced into a second spool file and hashed on the way; the same timeout, process throttle and circuit breaker as in the table above, and any skip or failure keeps the original file); the digest of the file that results, computed while it was written, is the key, and the file is handed to the storage layer whole (`insert_spooled`): an image is renamed into the cache and its row built from its length, anything else is read into its row once and the file unlinked. Neither the queue nor the worker holds such a payload's bytes on the heap, converted or not, so the single worker thread also no longer spends its time hashing large payloads: the ingestion threads (up to 8, in parallel) hashed the stream, and `tee(2)` hashes `magick`'s output while `magick` is still producing it ([06](06_local_image_pipeline.md) section 6.4).

### 2.1 Bounded Queue, Dual Backpressure and Load Shedding (v0.7.1, byte budget v0.7.8)

Because conversion runs on the worker, a slow image delays every job behind it. The queue therefore has three bounds, applied in this order ([`crates/wayland/src/ingest.rs`](../crates/wayland/src/ingest.rs)): how many readers, then how many jobs, then how many **bytes**. A count alone is not a memory bound: 64 queued payloads of 256 MiB would be 16 GiB, in heap or in spool files.

```text
   Selection event
        |
        v
   ReaderGate::try_acquire()  --- full (8 readers) ---> shed (counted), NO pipe, NO thread
        |  permit
        v
   make_pipe()  --- EMFILE ---> shed (counted, logged), permit released
        |
        v  reader thread: bounded read + normalise
        |
        v
   enqueue_bounded(): payload > whole byte budget? --yes--> shed at once, no waiting
        |  no
        v
   poll every 5 ms, for up to 2 s, for BOTH:
        1. ByteBudget::try_reserve(len)   -> BytePermit        (one CAS loop)
        2. try_send into sync_channel(64)                      (the permit rides in the job)
        |                                  |
      queued                       still no room after 2 s
        |                                  |
   worker drops the job            shed (counted), job dropped:
   -> permit returns the bytes     the permit returns the bytes, the spool file is unlinked
```

| Mechanism | Constant | Effect |
| :--- | :--- | :--- |
| Reader gate | `INGEST_MAX_CONCURRENT_READERS` = 8 | Checked **before** the pipe is created, so a burst of selections can neither exhaust file descriptors (`EMFILE`) nor threads. A refused selection costs one atomic compare-and-swap. A pipe that still cannot be allocated is counted as a shed reader and logged (v0.7.8) |
| Bounded channel | `INGEST_QUEUE_CAPACITY` = 64 | `sync_channel`; a full queue hands the job back (`TrySendError::Full`), so retrying never clones the payload |
| Byte budget | `INGEST_QUEUE_BYTE_BUDGET` = 64 MiB | Total length of the queued payloads, heap buffers and spool files alike (`ClipboardJob::accounted_bytes`, from `Payload::len_hint`; a payload whose length is unknown without I/O counts as zero, and ingestion only produces `Memory` and `Spooled`). A payload larger than the whole budget could never be admitted, so it is shed immediately instead of waited on |
| Patience | `INGEST_ENQUEUE_WAIT_MS` = 2000 | A reader that finds no room (slot or bytes) waits **while keeping its gate permit**: this is the backpressure, because it makes new selections hit the gate instead of piling payloads up in memory |
| Shedding | counters `shed_readers`, `shed_queue` | Once patience is spent the payload is dropped. Totals appear in `y4p status` and the first, second, fourth, eighth, ... shed is logged, with a reason: "too many concurrent readers", "could not allocate pipe descriptors (EMFILE)", "payload exceeds the queue byte budget" (retrying cannot help) or "worker queue or byte budget full" |

#### The byte budget and its permit

`ByteBudget` is an `AtomicUsize` and a limit; `BytePermit` is the reservation, an RAII guard holding an `Arc` of the budget and its length.

```rust
// crates/wayland/src/ingest.rs
pub fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<BytePermit> {
    let mut current = self.used.load(Ordering::Acquire);
    loop {
        let next = current.checked_add(bytes).filter(|&next| next <= self.limit)?;   // no wrap-around
        match self.used.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(BytePermit { budget: Arc::clone(self), bytes }),
            Err(actual) => current = actual,
        }
    }
}
// impl Drop for BytePermit { fn drop(&mut self) { self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel); } }
```

| Property | Why it holds |
| :--- | :--- |
| Lock-free, O(1) | One compare-and-swap loop; a reservation never blocks the thread that asks, and a refusal leaves the counter untouched. Next to an ingest that takes milliseconds the cost is about 16 ns and no allocation ([`PERFORMANCE.md`](../PERFORMANCE.md) section 7.6) |
| No overflow | `checked_add` turns an absurd length into a refusal instead of a wrap-around that would admit it (tested with `usize::MAX`) |
| No leak, no double return | The only way bytes come back is `Drop`, once per permit. The permit lives in `ClipboardJob`, so *wherever* the job ends, the bytes return: the worker finishes it (persisted, duplicate, declined), the queue sheds it, or the receiver is dropped with jobs still inside. A permit held while waiting for a queue slot is released when the patience runs out |
| Never above the limit | Concurrent producers either win the exchange or retry with the new total; the many-producers test checks that the peak never exceeds the limit |
| `enqueue` unchanged | The plain `enqueue` (no budget) stays for callers that do not account bytes; its jobs carry no permit |

The worker drops the permit right after the store call returns, **whatever the result** (`worker.rs`, section 1). By then the payload no longer occupies the budget: a buffer is dropped, a spool file was renamed into the cache or unlinked. Dropping it any earlier would let a reader in while the payload still exists; any later would hold the budget through logging.

> **Invariant — overload loses the newest copy, never the queued ones and never the event loop.** Waiting happens on the reader's own thread; the Wayland poll loop only ever performs the gate's compare-and-swap. A shed is always counted and surfaced; a worker that has gone away is *not* counted as shedding. A thread that cannot be spawned (`Builder::spawn` returns an error instead of panicking) is also shed, and dropping its closure frees the pipe and the permit.

> **Invariant — queued bytes never exceed `INGEST_QUEUE_BYTE_BUDGET`.** Payloads are admitted by reservation before they are queued, and a reservation outlives the job only if the job is still alive.

Why drop the newest rather than evict the oldest: `std` offers no way to remove from the middle of a channel, and keeping queued work intact preserves arrival order, which the worker guarantees.

> **Known trade-offs — what the budget does not bound.**
> - A reader is not budgeted until it has finished reading and offers the payload, so the eight readers can each hold up to `MAX_PAYLOAD_BYTES` (256 MiB) beforehand. The ceiling is 64 MiB queued + 8 x 256 MiB in flight, about 2.1 GiB, down from (64 + 8 + 1) x 256 MiB ([`PERFORMANCE.md`](../PERFORMANCE.md) section 5.2).
> - `MAX_PAYLOAD_BYTES` (256 MiB) is above the budget (64 MiB), so a payload between 64 and 256 MiB is read in full (or spooled in full) and then shed. Reading only up to the budget would shed it earlier but would change the effective maximum payload size ([`PERFORMANCE.md`](../PERFORMANCE.md) section 9, item 8).

---

## 3. WAL Concurrency and the Read-Side Handle

The daemon opens a *second* `ClipboardDb` for reads served inside the daemon process itself (`read_db`, [`lib.rs:L91`](../crates/daemon/src/lib.rs)), rather than sharing the writer's connection. SQLite's WAL mode lets readers proceed concurrently with a writer without blocking either side. Splitting the handle costs nothing, and means a slow read (a restore lookup) can never, even in principle, block clipboard ingestion.

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
| 1. Read | per-selection ingestion thread | One `Vec<u8>` filled by `read_to_end`, at most `MAX_PAYLOAD_BYTES + 1` read; discarded outright if oversized. A text-like MIME reserves one page up front (one allocation instead of 3-9; [`PERFORMANCE.md`](../PERFORMANCE.md) section 7.1). **A large non-text payload does not get a `Vec` at all (v0.7.3):** one 4 KiB head buffer, then `splice(2)` into a spool file with a 64 KiB stack buffer for the digest ([01](01_wayland_streaming_io.md) section 2.4); steps 2-6 below then concern what happens to a `Memory` payload |
| 2. Normalise | same thread | `text/uri-list` produces one new `Vec`; everything else is moved, not copied |
| 3. Hand-off | bounded channel | The `ClipboardJob` is **moved** with its `BytePermit` and its `Payload::Memory` is handed to the worker without a copy; the bytes are never cloned. A full queue or an exhausted byte budget sheds instead of growing (section 2.1) |
| 4. Route | `DbWorker` | Text and unsupported types: moved straight through. Images: output buffer pre-sized from the input length (capped at `IMAGE_OUTPUT_HINT_CAP_BYTES`, 16 MiB) to avoid repeated doubling; on success the original `Vec` is dropped |
| 5. Persist | `DbWorker` | `insert_with_hash` borrows `&data`; text is bound as `&str` with no copy |
| 6. Reclaim | `DbWorker` | The job's `BytePermit` is dropped as soon as persistence has returned, freeing its share of the byte budget; `mime`, `data`, `hash` drop at the end of the loop iteration |

Egress is lighter still: a cached image goes through `sendfile(2)` and never enters the heap ([01](01_wayland_streaming_io.md) section 4).

> **Invariant — no ingested payload outlives its job.** The daemon holds no clipboard history in memory. Everything durable is in SQLite or the cache directory; process memory returns to its idle baseline once the queue drains. The same holds on disk for a spooled payload: its file is owned by a `SpooledFile` that unlinks it on drop, so a job that is shed, rejected or abandoned cannot leave one behind (only adoption into the cache, by `rename(2)`, keeps it). The one deliberate exception is egress: while the daemon owns the selection after a `copy-to`, the restored payload (an `Owned` text buffer, or just a path for a cached image) stays attached to the active data source until the compositor cancels it.

During a conversion the original and the converted image coexist briefly (the peak is roughly the sum of the two), bounded by the 256 MiB read cap and the converter's own memory limits.

---

## 6. Lock-Free Counters and Exit Coordination

**Metrics.** `DaemonMetrics` ([`crates/daemon/src/metrics.rs`](../crates/daemon/src/metrics.rs)) holds `AtomicU64` ingress/egress counters and an `AtomicI64` last-event timestamp, written with `Ordering::Relaxed` from the worker and read from the event loop when IPC `Status` arrives. No mutex anywhere in the type: the counters are independent statistics, so no ordering between them is required. The status line also carries `shed_readers=` and `shed_queue=`, the overload totals of [section 2.1](#21-bounded-queue-dual-backpressure-and-load-shedding-v071-byte-budget-v078), owned by `y4p-wayland` (`IngestStats`) and passed in by the event loop because the daemon's metrics type cannot see the sensor layer's state.

**Exit.** `SIG_EXIT` is a single `AtomicBool` in `y4p-core` shared by `main.rs`'s `ctrlc` handler and the daemon loop. The handler only sets the flag (a second Ctrl+C force-exits with status 1). The loop re-checks `is_exiting()` at least every 500 ms because of its `poll` timeout, so no socket round-trip is needed to wake it. A non-daemon invocation exits with status 130 on SIGINT and never touches the daemon's socket.

> **Invariant — interrupting a one-shot command never stops the resident daemon.**
