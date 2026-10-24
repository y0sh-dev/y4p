# 00 — Overview & Workspace Architecture

This is the entry point for `y4p`'s internal documentation.

The [README](../README.md) tells you *what* the tool does and how to run it. Everything under `docs/` tells you *why* it is built the way it is. Each file is a self-contained theme: read the one your question actually falls under.

---

## 1. Two Faces, One Binary

`y4p` is one binary that behaves differently depending on how it is invoked.

- `y4p daemon` becomes a long-lived background process. It owns the Wayland connection and the SQLite write path, and sits idle until something happens.
- Anything else (`list`, `copy-to`, `pin`, ...) is a short-lived CLI process. It usually reads the database directly and prints a result. For actions only the daemon can perform, it sends a one-shot command over a Unix socket and exits.

```text
                     y4p daemon  (long-lived background process)

  Wayland compositor                                    $XDG_RUNTIME_DIR
  (ext-data-control-v1)                                 /y4p/y4p.sock  (IPC)
          |                                                    |
          v                                                    v
   y4p-wayland ----(bounded: ClipboardJob {mime, payload, permit})-->  y4p-daemon
     (Ingress: read, normalise)                     worker: image route, SHA3-256,
          ^                                         single DB writer
          |                                                    |
          |                                                    v
          |                                          y4p-storage::ClipboardDb
          |                                           SQLite (WAL) + ~/.cache/y4p/
          |                                                    |
          +----(Egress: handle_restore_request)----------------+
                data_control::source (sendfile)


                     y4p <command>  (short-lived CLI process)

          y4p-cli  ------------------->  y4p-storage::ClipboardDb
                                          (direct read for
                                           list / search / show)

          y4p-cli  --(IPC, one-shot)-->  the daemon above
                                          (copy-to / store sync /
                                           status / pause / resume)
```

---

## 2. Workspace Topology

Since v0.6.x the code base is a Cargo workspace. The root package (`y4p`) is a thin composition root; every domain lives in its own crate under `crates/`.

```text
y4p/
├── Cargo.toml          workspace manifest, shared version / lints / pinned dependencies
├── src/main.rs         composition root: signal handler, DB open, command dispatch
├── crates/
│   ├── core/           y4p-core     constants, XDG paths, config parser, MIME utilities, Payload, splice / pipe / FastCDC primitives, exit flag
│   ├── storage/        y4p-storage  SQLite facade, schema migrations (v4), chunked content store, on-disk blob cache
│   ├── image/          y4p-image    sniffing, sandbox, transcode pipeline, breaker, throttle
│   ├── wayland/        y4p-wayland  ext-data-control-v1 handlers, streaming clipboard I/O, ingestion backpressure
│   ├── daemon/         y4p-daemon   event loop, IPC socket, background worker, payload spooler
│   └── cli/            y4p-cli      strict argument parsing, command handlers, formatting
├── docs/               this directory
└── scripts/check.sh    workspace-wide tests + strict clippy
```

### 2.1 Crate responsibility matrix

| Crate | Single responsibility | Workspace dependencies | Third-party dependencies | Tests |
| :--- | :--- | :--- | :--- | ---: |
| `y4p-core` | Shared plumbing: constants, XDG paths, `y4p.toml` parser, MIME helpers, the `Payload` vocabulary, `splice(2)` / pipe-sizing / FastCDC primitives, exit flag | none | `libc`, `percent-encoding` | 140 |
| `y4p-storage` | Persistence: SQL store, schema gate (`SCHEMA_VERSION = 4`), chunked content store, blob cache, `ClipboardDb` facade | core | `rusqlite` (bundled SQLite), `sha3` | 84 |
| `y4p-image` | Untrusted-image handling: routing, sandboxed `magick` runs, breaker, throttle | core | `libc` | 192 |
| `y4p-wayland` | Compositor I/O: offers, bounded reads, normalisation, egress; ingestion backpressure (reader gate, `ByteBudget`, `enqueue_bounded`) | core | `wayland-client`, `wayland-protocols`, `libc` | 110 |
| `y4p-daemon` | Process lifecycle: poll loop (with compositor-disconnect handling), IPC, `FileSpooler`, `DbWorker` (image route + hash + write) | core, storage, wayland, image | `wayland-client`, `libc`, `sha3` | 43 |
| `y4p-cli` | Argument parsing, validation, output formatting; returns `CliAction` to the root | core, storage | `unicode-width` | 0 |
| `y4p` (root) | Wiring only | all of the above | `libc`, `ctrlc` | 0 |

Total: 569 tests (568 unit tests and one integration test, `crates/daemon/tests/spooled_persist.rs`), plus the benchmark harness in smoke mode, all run by `bash scripts/check.sh`. `y4p-cli` and the root have no tests of their own, and nothing runs against a live compositor: the daemon's event loop is covered through real socket pairs (`read_compositor_events`), and the rest by black-box comparison and manual runs.

### 2.2 Dependency direction

```text
  rank 0    y4p (root)
              |
  rank 1    y4p-cli          y4p-daemon
              |             /     |     \
  rank 2    y4p-storage   y4p-wayland   y4p-image
              |               |             |
  rank 3    y4p-core  <-------+-------------+     (every crate depends on it)

  Edges, exactly as declared in the manifests:
    cli     -> storage, core
    storage -> core
    image   -> core
    wayland -> core
    daemon  -> wayland, storage, image, core
    root    -> cli, daemon, wayland, storage, core
```

> **Invariant — acyclic, inward-flowing dependencies.** Every edge points towards `core`. Cargo rejects a cycle at build time, so this is enforced by the compiler rather than by convention.

> **Invariant — `wayland` knows nothing about storage or images.** `y4p-wayland` depends on neither `y4p-storage`, `y4p-image` nor `sha3`. It speaks the protocol, drains pipes and normalises. Everything that interprets or persists a payload happens in `y4p-daemon`.

> **Invariant — the daemon is the only translator.** `y4p-daemon` is the one crate allowed to see both `storage::ContentLocation` and `core::payload::Payload`, and it is where one is re-expressed as the other (see section 4).

---

## 3. Runtime Layers

| Layer | Crate | Job | Document |
| :--- | :--- | :--- | :--- |
| Frontend | `y4p-cli` | Parse and validate arguments, print results. Never touches Wayland. Reads storage directly; uses IPC for daemon-owned actions. | [04](04_strict_cli_and_stable_id.md) |
| Process | `y4p-daemon` | Lifecycle, IPC socket, `DbWorker`: the one thread allowed to hold a write connection. | [03](03_concurrency_and_memory.md) |
| Sensor | `y4p-wayland` | `ext-data-control-v1`: ingest offers, serve history back out, pipe plumbing. | [01](01_wayland_streaming_io.md), [05](05_filter_pipeline_and_app_rules.md) |
| Persistence | `y4p-storage` | `ClipboardDb` facade over `db.rs` (pure SQL) and `cache.rs` (files only). | [02](02_hybrid_storage_and_pin.md) |
| Hardening | `y4p-image` | Sandboxed, bounded image transcoding with a circuit breaker. | [06](06_local_image_pipeline.md) |
| Shared | `y4p-core` | XDG paths, config, constants, MIME helpers, the `Payload` vocabulary, exit flag. Depends on nothing in the project. | all |

---

## 4. Crossing the Wayland / Storage Boundary

The only place the two vocabularies meet is `handle_restore_request` in [`crates/daemon/src/lib.rs:L246`](../crates/daemon/src/lib.rs):

```text
  storage::ClipboardDb::locate_content(id)
        |
        v
  storage::ContentLocation                 core::payload::Payload
    InlineBlob(rowid)  -- read_db ---->      Memory(Vec<u8>)
    CacheFile(path)    ----------------->    File(PathBuf)   --> sendfile(2)
```

`wayland` receives a payload it can hand to the compositor. It never sees `ContentLocation` and never imports from `storage`. That is what lets `storage` be tested without a Wayland connection, and `wayland` without a database.

---

## 5. The Ingestion Stream Contract (v0.6.7, byte budget v0.7.8)

The hand-off from sensor to worker is deliberately minimal:

```rust
// crates/wayland/src/state.rs
pub struct ClipboardJob {
    pub mime: String,
    pub payload: Payload, // core::payload: Memory(Vec<u8>) | Stream(StreamDescriptor) | File(PathBuf) | Spooled(Box<SpooledFile>)
    pub permit: Option<BytePermit>, // v0.7.8: this job's share of the queue's byte budget; dropped by the worker
}
```

Ingestion sends `Payload::Memory` for text and for payloads that end inside one page (built with `ClipboardJob::new(mime, data)`), and, since v0.7.3, `Payload::Spooled` for a large non-text payload: the ingestion thread drains the pipe into a spool file with `splice(2)`, fingerprinting it on the way, and the queue carries the file's path and digest instead of the bytes. Egress uses the same `Payload` type in `SourceMetadata`. See [01](01_wayland_streaming_io.md) sections 2.3 and 2.4.

Since v0.7.8 the job also carries a `BytePermit`: a reservation of its payload's length in a lock-free `ByteBudget` (`INGEST_QUEUE_BYTE_BUDGET`, 64 MiB) that is returned when the job is dropped, wherever that happens (persisted or discarded by the worker, shed from a full queue, or abandoned with the receiver). Heap buffers and spool files count alike. See [03](03_concurrency_and_memory.md) section 2.1.

| Stage | Where | What happens |
| :--- | :--- | :--- |
| 1. Gate | `y4p-wayland` | Sensitive hints rejected and the concurrent-reader limit (8) enforced, both before any pipe is opened. A pipe that still cannot be allocated (`EMFILE`) is shed, counted and logged (v0.7.8) |
| 2. Select | `y4p-wayland` | Richest MIME chosen by `MIME_PRIORITY_ORDER` |
| 3. Read | `y4p-wayland` | Bounded read (`MAX_PAYLOAD_BYTES`, one probe byte). Large non-text payloads are spooled to a file with `splice(2)` and fingerprinted on the way (v0.7.3) |
| 4. Normalise | `y4p-wayland` | `text/uri-list` decoded; mislabelled images re-identified by magic bytes |
| 5. Forward | `y4p-wayland` -> `y4p-daemon` | `ClipboardJob { mime, payload, permit }` over a bounded `sync_channel` (64 slots) and a 64 MiB byte budget (`enqueue_bounded`); overload is shed and counted. A payload above the whole budget is shed at once |
| 6. Route | `y4p-daemon` worker | Raster images privacy-stripped and re-encoded by `y4p-image` (original kept on any failure). A spooled image is converted file to file, with no heap copy (v0.7.4); a spooled payload the converter would not touch skips this and the next step |
| 7. Fingerprint | `y4p-daemon` worker | SHA3-256 over the **final** bytes (a spooled payload arrives with it, computed while it streamed in; a converted one, while `magick`'s output was written) |
| 8. Persist | `y4p-daemon` worker | `ClipboardDb::insert_with_hash`; a spooled payload goes to `ClipboardDb::insert_spooled`, which renames an image's file into the cache (no byte read) and stores anything else from the file. Text and other payloads above 4 KiB are stored as reference-counted FastCDC chunks (schema v4, [02](02_hybrid_storage_and_pin.md) section 7). The worker then drops the job's byte permit |

> **Invariant — the hash describes what is stored.** The fingerprint is computed last, after image routing, so a deduplicated row and its cache file are always keyed by the bytes actually saved.

> **Invariant — the sensor never blocks on a converter.** Image transcoding and hashing no longer run on the per-selection ingestion thread. They run on the single `DbWorker` thread, so jobs are also persisted strictly in arrival order. The cost: a slow conversion (bounded by the timeout, throttle and breaker in [06](06_local_image_pipeline.md)) delays the jobs queued behind it, and the queue behind it is bounded: see [03](03_concurrency_and_memory.md) section 2.1.

---

## 6. What to Read Next

| What you want to know | File |
| :--- | :--- |
| Why one thread multiplexes Wayland and IPC, how bounded reads avoid silent truncation, how `provider_locks` prevents self-ingestion, and how egress uses `sendfile(2)` | [01 — Wayland Protocol & Streaming I/O](01_wayland_streaming_io.md) |
| Why text and large images live in different stores, how migrations stay safe, how rotation became near-O(1), and how pinned records are carved out of the quota | [02 — Hybrid Storage & Pin Protection](02_hybrid_storage_and_pin.md) |
| Why every write funnels through one worker, how WAL lets readers run alongside it, and what `open_read_only` enforces | [03 — Concurrency & Memory Reclamation](03_concurrency_and_memory.md) |
| Why display position (MRU) and identity (stable ID) are different numbers, why search avoids a window function, what the `--raw` contract promises, and why the CLI rejects unknown flags | [04 — Stable IDs & the Strict CLI](04_strict_cli_and_stable_id.md) |
| How ingress filters sensitive and redundant offers, what is and is not modified before storage, and how `y4p.toml` is parsed | [05 — Filter Pipeline & Dynamic Egress](05_filter_pipeline_and_app_rules.md) |
| How images are sniffed, sandboxed, time-boxed and circuit-broken | [06 — Local Image Pipeline & Defence-in-Depth](06_local_image_pipeline.md) |

Repository governance (not architecture) lives outside this directory: [`.github/AI_POLICY.md`](../.github/AI_POLICY.md).
