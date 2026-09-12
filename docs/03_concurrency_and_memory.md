# Concurrency & Memory Reclamation

The daemon's poll loop, described in [01](01_wayland_streaming_io.md), has to stay responsive to new Wayland events at all times.

Disk writes and hashing are exactly the kind of work that would stall it, if they ran inline. This document covers how `y4p` keeps that work off the critical path — and how it keeps a long-lived process from quietly accumulating memory it no longer needs.

---

## Why every database write goes through one worker thread

SQLite allows exactly one writer at a time.

A second connection that tries to write while another transaction is open gets `SQLITE_BUSY` — "database is locked." A naive retry-with-backoff strategy for that error becomes a source of latency spikes, and under enough contention, outright write failures.

`y4p` avoids the problem structurally, instead of defensively. Only one thread in the entire process ever holds a write connection:

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

`ClipboardDb` moves *into* the spawned thread. It's never shared.

Rust's ownership model makes "only this thread can write" a compile-time fact — not a convention someone has to remember at every call site. Every other part of the daemon that wants something persisted (the Wayland ingestion handler, mainly) doesn't touch the database at all. It builds a `ClipboardJob`, and sends it down an `mpsc::Sender` clone.

The channel is the only coupling between "something happened on the clipboard" and "something got written to disk." And it's a coupling that can never produce a lock conflict — because structurally, there is nothing on the other side of that lock to conflict with.

This is also why the daemon opens a *second*, read-only `ClipboardDb` handle for reads served from within the daemon process itself — see `read_db` in `daemon::start_daemon` — rather than sharing the writer's connection. SQLite's WAL mode is specifically designed to let readers proceed concurrently with a writer, without blocking either side. Splitting the handle this way costs nothing, and it means a slow read query could never, even in principle, block clipboard ingestion.

```text
   Wayland event
        |
        v
   ClipboardJob --> mpsc channel --> DbWorker thread --> SQLite
        |                                                (single writer)
        |                                                     ^
        |                                                     | WAL mode:
        +-- main poll loop keeps running,                     | readers never
            never waits on this send                          | block the writer
                                                                |
                                              read_db (list / search) ---+
```

---

## Why `open_read_only` exists — from convention to enforcement

Everything above describes a *design*: only the worker thread's `ClipboardDb` handle ever writes, and `read_db` exists purely to read. But until v0.2.5, that guarantee lived entirely in the Rust code's structure — every `ClipboardDb`, `read_db` included, was opened through the same `Connection::open`, which hands back an ordinary read-write connection.

Nothing was actually wrong with that. Rust's ownership rules already make it impossible for two threads to *share* the writer's handle, and `read_db`'s own code never calls anything but `SELECT`. The single-writer property held — but it held because every call site happened to behave, not because SQLite itself would refuse to let it do otherwise.

That distinction matters for exactly the kind of bug code review can't catch by inspection alone: a future contributor adding one read-oriented feature to `read_db`'s call path, who reaches for `UPDATE` instead of `SELECT` because it's the fastest way to make a bug go away, and nothing stops the connection from accepting it.

`ClipboardDb::open_read_only()` closes that gap at the layer beneath Rust's own type system:

```rust
// src/storage/mod.rs
Connection::open_with_flags(
    &db_path,
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
)
```

`SQLITE_OPEN_READ_ONLY` isn't a Rust-level promise. It's instruction to the SQLite engine itself: any `INSERT`, `UPDATE`, or `DELETE` sent down this particular connection is rejected by SQLite before it ever touches a page on disk, regardless of what the calling Rust code intended to do. The single-writer rule stops being "true because nobody has broken it yet," and becomes "true because the database itself won't allow it to be broken." `SQLITE_OPEN_NO_MUTEX` is a smaller, complementary optimization riding along on the same call — it skips SQLite's own internal connection locking, which a connection that (by construction) only one thread will ever touch doesn't need to pay for.

```text
   Before (v0.2.0)                         After (open_read_only, v0.2.5)

   read_db: Connection::open(...)          read_db: Connection::open_with_flags(
     (read-write connection —                 ..., SQLITE_OPEN_READ_ONLY)
      "please only SELECT" is a
      convention, not a rule)                 a stray UPDATE/DELETE/INSERT
                                               is rejected by SQLite itself
```

As of v0.2.5, this constructor exists but is not yet the one `read_db` or any CLI read path actually calls — today they still open through the ordinary read-write `open()`, exactly as before. Wiring `read_db` and the read-only CLI commands (`list`, `search`, `show`) over to `open_read_only` is tracked for v0.3.0 (see the `TODO(v0.3.0)` marker in `storage/mod.rs`). The v0.2.5 change is deliberately scoped to landing the enforcement primitive on its own — a constructor whose only job is opening a connection SQLite itself will refuse to let write, ready for the call sites that will adopt it next.

---

## Why `malloc_trim(0)` is called explicitly

Rust's global allocator doesn't return freed memory to the OS right away, as a matter of course.

Like most general-purpose allocators, it keeps recently-freed pages around, on the assumption the next allocation will be a similar size and can reuse them. That's the right tradeoff for most workloads.

But for a daemon meant to sit resident for an entire login session, that assumption breaks down — for exactly the case `y4p` exists to handle well. A single 70MB image ingestion allocates a large buffer once. Without an explicit signal, the allocator has no particular reason to ever give that memory back. The process's Resident Set Size can end up permanently reflecting the largest thing it ever copied, rather than what it's actually holding right now.

`libc::malloc_trim(0)` is the explicit "give it back" instruction.

`y4p` calls it in both places a large payload's memory is freed shortly after use. Once in the ingestion handler, right after a job is handed off (`wayland/handlers/data_control/device.rs`). And once in the DB worker, right after a job is persisted (`daemon/worker.rs`).

Calling it unconditionally — after every job, including tiny text snippets — is deliberate, not an oversight. `malloc_trim` is cheap when there's little to trim. Gating it behind a size threshold would mean maintaining a second piece of state, just to avoid a call that's already inexpensive in the common case.

The `#[cfg(target_os = "linux")]` guard reflects what this really is: a glibc/Linux allocator API, specifically. It's not part of the semantics the rest of the daemon depends on. Just a periodic hint, to this one platform's allocator.
