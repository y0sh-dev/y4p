# Concurrency & Memory Reclamation

The daemon's poll loop ([01](01_wayland_streaming_io.md)) has to stay responsive to new Wayland events at all times. Disk writes and hashing are exactly the kind of work that would stall it if they ran inline — this file covers how `y4p` keeps that work off the critical path, and how it keeps a long-lived process from quietly accumulating memory it isn't using.

---

## Why every database write goes through one worker thread

SQLite allows exactly one writer at a time; a second connection attempting to write while another transaction is open gets `SQLITE_BUSY` ("database is locked"), and a naive retry-with-backoff strategy for that error is a source of both latency spikes and, under enough contention, outright write failures. `y4p` avoids the problem structurally instead of defensively: only one thread in the entire process ever holds a write connection.

```rust
// src/daemon/worker.rs
pub fn spawn(mut db: ClipboardDb, metrics: Arc<DaemonMetrics>, verbose: bool, max_history: usize) -> Self {
    let (tx, rx) = mpsc::channel::<ClipboardJob>();

    std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            match db.insert_with_hash(&job.mime, &job.data, &job.hash, max_history) {
                Ok(_) => metrics.record_ingress(),
                Err(e) => eprintln!("worker failed to persist data: {}", e),
            }
            #[cfg(target_os = "linux")]
            unsafe { libc::malloc_trim(0); }
        }
    });

    Self { tx }
}
```

`ClipboardDb` moves *into* the spawned thread and is never shared — Rust's ownership model makes "only this thread can write" a compile-time fact rather than a convention that has to be remembered at every call site. Every other part of the daemon that wants something persisted (the Wayland ingestion handler, chiefly) doesn't touch the database at all: it builds a `ClipboardJob` and sends it down an `mpsc::Sender` clone. The channel is the only coupling between "something happened on the clipboard" and "something got written to disk," and it's a coupling that can never produce a lock conflict, because there is structurally nothing on the other side of that lock to conflict with.

This is also why the daemon opens a *second*, read-only `ClipboardDb` handle for `list`/`search`-style reads served from within the daemon process (see `read_db` in `daemon::start_daemon`) rather than sharing the writer's connection. SQLite's WAL mode is specifically designed to let readers proceed concurrently with a writer without blocking either side, so splitting the handle this way costs nothing and means a slow `list` query — however unlikely — could never in principle be able to block clipboard ingestion.

```
   Wayland event ──▶ ClipboardJob ──▶ mpsc channel ──▶ DbWorker thread ──▶ SQLite (single writer)
        │                                                                        ▲
        │                                                                        │ WAL: readers never block writer
        └──▶ (main poll loop keeps running — never waits on this send)   read_db (list/search) ─┘
```

## Why `malloc_trim(0)` is called explicitly

Rust's global allocator (like most general-purpose allocators) doesn't return freed memory to the OS immediately as a matter of course — it keeps recently-freed pages around on the assumption that the next allocation will be a similar size and can reuse them, which is the right tradeoff for most workloads. For a daemon that's supposed to sit resident for the lifetime of a login session, though, that assumption breaks down for exactly the case `y4p` exists to handle well: a single 70MB image ingestion allocates a large buffer once, and without an explicit signal, the allocator has no particular reason to ever give that memory back — the process's Resident Set Size (RSS) can end up permanently reflecting the largest thing it ever copied, rather than what it's actually holding onto right now.

`libc::malloc_trim(0)` is the explicit "actually give it back" instruction. `y4p` calls it in both places a large payload's memory is freed shortly after being used: once in the ingestion handler right after a job is handed off (`wayland/handlers/data_control/device.rs`), and once in the DB worker right after a job is persisted (`daemon/worker.rs`). Calling it unconditionally after every job, including tiny text snippets, is deliberate rather than an oversight — `malloc_trim` is cheap when there's little to trim, and gating it behind a size threshold would mean maintaining a second piece of state (and a second thing to get wrong) purely to avoid a call that's already inexpensive in the common case. The Linux-only `#[cfg(target_os = "linux")]` guard reflects that this is a glibc/Linux allocator API specifically — it's not part of the semantics the rest of the daemon depends on, just a periodic hint to this platform's allocator.
