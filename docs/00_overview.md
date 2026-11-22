# 00 — Overview & Workspace Architecture

This is the entry point for `y4p`'s internal documentation.

The [README](../README.md) tells you *what* the tool does and how to run it. Everything under `docs/` tells you *why* it is built the way it is. Each file is a self-contained theme: read the one your question actually falls under.

---

## 1. Two Faces, One Binary

`y4p` is one binary that behaves differently depending on how it is invoked.

- `y4p daemon` becomes a long-lived background process. It owns the Wayland connection and the SQLite write path, and sits idle until something happens.
- Anything else (`list`, `copy-to`, `pin`, ...) is a short-lived CLI process. It usually reads the database directly and prints a result. For actions only the daemon can perform, it sends a one-shot command over a Unix socket and exits.
- Three commands observe the system rather than the history: `y4p paste-from -l` prints the current selection's advertised MIME types without opening a pipe; `y4p stats` reports entry counts, logical and on-disk volume plus the daemon's shed counters; `y4p doctor` walks environment, compositor, database, socket and quota, printing `[PASS]` / `[WARN]` / `[FAIL]` per check and exiting non-zero only when a check fails ([04](04_strict_cli_and_stable_id.md) section 7).

A user unit ships with the project as `contrib/systemd/y4p.service`. Its `Restart=on-failure` is only meaningful because `y4p daemon` reports a failed start and a lost compositor connection as a non-zero exit status ([01](01_wayland_streaming_io.md) section 1.2); a stop the user asked for is the only clean exit.

```text
                     y4p daemon  (long-lived background process)

  Wayland compositor                                    $XDG_RUNTIME_DIR
  (ext-data-control-v1)                                 /y4p/y4p.sock  (IPC)
          |                                                    |
          v                                                    v
   y4p-wayland ----(bounded: ClipboardJob {mime, payload})-->  y4p-daemon
     (Ingress: read, normalise)                     worker: optional image
          ^                                         routing, SHA3-256,
          |                                         single DB writer
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
                                          (copy-to / store sync / delete /
                                           wipe / status / pause / resume)
```

---

## 2. Workspace Topology

The code base is a Cargo workspace. The root package (`y4p`) is a thin composition root; every domain lives in its own crate under `crates/`.

```text
y4p/
├── Cargo.toml          workspace manifest, shared version / lints / pinned dependencies
├── src/main.rs         composition root: signal handler, DB open, command dispatch
├── crates/
│   ├── core/           y4p-core     constants, XDG paths, config parser, MIME utilities, Payload, pipe / kernel-copy primitives, exit flag
│   ├── storage/        y4p-storage  SQLite facade, schema migrations (v4), inline payload store, legacy chunk-row reads, LRU-bounded blob cache
│   ├── image/          y4p-image    sniffing, sandbox, opt-in transcode pipeline, breaker, throttle
│   ├── wayland/        y4p-wayland  ext-data-control-v1 handlers, bounded clipboard reads, ingestion queue
│   ├── daemon/         y4p-daemon   event loop, IPC socket, background worker
│   └── cli/            y4p-cli      strict argument parsing, command handlers, formatting
├── docs/               this directory (`history/` keeps the archived measurements)
├── contrib/
│   ├── systemd/        `y4p.service`, the user unit bundled with the project
│   ├── packaging/      `PKGBUILD`, the Arch Linux package recipe
│   └── examples/       `y4p-fzf.sh`, a minimal fzf history picker
├── completions/        `_y4p`, the Zsh completion definition
└── scripts/
    ├── check.sh        workspace-wide tests + strict clippy
    └── e2e_compositor.sh   live compositor matrix (headless, or the current session with `E2E_USE_LIVE_WAYLAND=1`)
```

### 2.1 Crate responsibility matrix

| Crate | Single responsibility | Workspace dependencies | Third-party dependencies | Tests |
| :--- | :--- | :--- | :--- | ---: |
| `y4p-core` | Shared plumbing: constants, XDG paths, `y4p.toml` parser, MIME helpers, the `Payload` vocabulary, pipe-sizing / kernel-copy primitives, exit flag | none | `libc`, `percent-encoding` | 138 |
| `y4p-storage` | Persistence: SQL store, schema gate (`SCHEMA_VERSION = 4`), inline payloads, legacy chunk reads, blob cache and its LRU quota, `ClipboardDb` facade | core | `rusqlite` (bundled SQLite), `sha3` | 88 |
| `y4p-image` | Untrusted-image handling: routing, sandboxed `magick` runs, breaker, throttle | core | `libc` | 157 |
| `y4p-wayland` | Compositor I/O: offers, MIME candidates, bounded reads, normalisation, egress; ingestion backpressure (reader gate, bounded queue) | core | `wayland-client`, `wayland-protocols`, `libc` | 101 |
| `y4p-daemon` | Process lifecycle: poll loop (with compositor-disconnect handling), IPC, `DbWorker` (optional image route + hash + write) | core, storage, wayland, image | `wayland-client`, `libc`, `sha3` | 39 |
| `y4p-cli` | Argument parsing, validation, output formatting, `stats` and `doctor` reports; returns `CliAction` to the root | core, storage | `unicode-width` | 26 |
| `y4p` (root) | Wiring only | all of the above | `libc`, `ctrlc` | 0 |

Total: **549** tests across the six crates, plus the benchmark harness in smoke mode, all run by `bash scripts/check.sh`. The root has no tests of its own. Nothing in the test suite talks to a live compositor: the daemon's event loop is covered through real socket pairs (`read_compositor_events`), ingestion through real pipes and a real database, and the live matrix is a separate script (`scripts/e2e_compositor.sh`) that starts a headless compositor, reuses the current session with `E2E_USE_LIVE_WAYLAND=1`, or prints `[SKIP]` and exits 0 when the compositor, wl-clipboard or ImageMagick is missing.

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

### 2.3 Core invariants

These are the properties the rest of the documentation assumes. They are deliberate design commitments, not incidental behaviour, and changing one is an architecture change rather than a patch.

| # | Invariant | Enforced by |
| :--- | :--- | :--- |
| 1 | **Inward-only topology.** Every crate dependency points towards `y4p-core`; no crate sees a layer above it (`wayland` knows neither `storage` nor `image`). | Cargo manifests; a cycle is a build error |
| 2 | **Schema backwards compatibility.** New rows are stored inline (`clipboard.content`, `is_chunked = 0`) or in `<hash>.cache`; nothing writes `chunks` / `manifest` or sets `is_chunked = 1`. Legacy v4 chunked rows stay readable, searchable, rotatable, deletable and wipable. | [02](02_hybrid_storage_and_pin.md) sections 1, 2 and 6 |
| 3 | **Panic-free resident layers.** `daemon`, `wayland`, `storage` (and `image`, `core`) build with `clippy::unwrap_used` / `expect_used` / `panic` denied; `panic = "abort"` in the release profile makes an unwrap a whole-process abort, so there are none. Only `#[cfg(test)]` code and the CLI's exit-on-error paths carve out an exception. | `[workspace.lints.clippy]` in the root manifest, `bash scripts/check.sh` |
| 4 | **Bounded ingestion and cache.** At most `INGEST_MAX_CONCURRENT_READERS` readers, at most `INGEST_QUEUE_CAPACITY` queued jobs, no payload above `MAX_PAYLOAD_BYTES` (32 MiB), and no cache directory above `CACHE_QUOTA_BYTES` (512 MiB) once evictable rows exist. Overload is shed, counted and reported, never absorbed. | [01](01_wayland_streaming_io.md) section 2, [03](03_concurrency_and_memory.md) section 2, [02](02_hybrid_storage_and_pin.md) section 8 |
| 5 | **One writer inside the daemon, serialised over IPC.** Daemon-side writes — ingress jobs and IPC `delete` / `wipe` alike — run on the single `DbWorker` connection (`Arc<Mutex<ClipboardDb>>`), so the daemon never has two writers and `wipe` releases memory in the process that holds it. Short-lived CLI commands that mutate the store (`pin`, `unpin`, `store`, and the MRU bump in `copy-to`) use their own handle; WAL plus `SQLITE_TIMEOUT_MS` serialises those cross-process writers. | [03](03_concurrency_and_memory.md) sections 1, 2.2 and 3 |

---

## 3. Runtime Layers

| Layer | Crate | Job | Document |
| :--- | :--- | :--- | :--- |
| Frontend | `y4p-cli` | Parse and validate arguments, print results (`stats`, `doctor` included). Never touches Wayland itself: the compositor handshake `doctor` and `paste-from` need is performed by the composition root. Reads storage directly; uses IPC for daemon-owned actions. | [04](04_strict_cli_and_stable_id.md) |
| Process | `y4p-daemon` | Lifecycle, IPC socket, `DbWorker`: the one thread allowed to hold a write connection. | [03](03_concurrency_and_memory.md) |
| Sensor | `y4p-wayland` | `ext-data-control-v1`: ingest offers, serve history back out, pipe plumbing. | [01](01_wayland_streaming_io.md), [05](05_filter_pipeline_and_app_rules.md) |
| Persistence | `y4p-storage` | `ClipboardDb` facade over `db.rs` (pure SQL) and `cache.rs` (files only). | [02](02_hybrid_storage_and_pin.md) |
| Hardening | `y4p-image` | Opt-in, sandboxed, bounded image transcoding with a circuit breaker. | [06](06_local_image_pipeline.md) |
| Shared | `y4p-core` | XDG paths, config, constants, MIME helpers, the `Payload` vocabulary, exit flag. Depends on nothing in the project. | all |

---

## 4. Crossing the Wayland / Storage Boundary

The only place the two vocabularies meet is `handle_restore_request` in [`crates/daemon/src/lib.rs`](../crates/daemon/src/lib.rs):

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

## 5. The Ingestion Stream Contract

The hand-off from sensor to worker is deliberately minimal:

```rust
// crates/wayland/src/state.rs
pub struct ClipboardJob {
    pub mime: String,
    pub payload: Payload, // core::payload: Memory(Vec<u8>) | File(PathBuf)
}
```

Ingestion has one road: the compositor pipe is drained into a bounded `Vec<u8>` by `DeadlineReader` + `payload::read_bounded`, normalised, and queued as `Payload::Memory`. The queue is a bounded `sync_channel` of `INGEST_QUEUE_CAPACITY` (64) jobs; there is no byte budget, no spool file and no descriptor hand-over. Egress uses the same `Payload` type in `SourceMetadata`, where `Payload::File` keeps the `sendfile(2)` zero-copy path for cached images. See [01](01_wayland_streaming_io.md) section 2.

| Stage | Where | What happens |
| :--- | :--- | :--- |
| 1. Gate | `y4p-wayland` | Sensitive hints rejected and the concurrent-reader limit (8) enforced, both before any pipe is opened. A pipe that still cannot be allocated (`EMFILE`) is shed, counted and logged |
| 2. Select | `y4p-wayland` | Allowlisted candidate array (`select_mime_candidates`: `MIME_PRIORITY_ORDER`, then `image/*`, then `text/*`), tried in order until one delivers |
| 3. Read | `y4p-wayland` | One bounded read per selection (`MAX_PAYLOAD_BYTES` = 32 MiB, one probe byte) under a single 5 s deadline |
| 4. Normalise | `y4p-wayland` | `text/uri-list` decoded; mislabelled images re-identified by magic bytes; on the daemon path, whitespace-only text is dropped before the queue ([01](01_wayland_streaming_io.md) section 2.5) |
| 5. Forward | `y4p-wayland` -> `y4p-daemon` | `ClipboardJob { mime, payload }` over a bounded `sync_channel` (64 slots); overload is shed and counted |
| 6. Route (opt-in) | `y4p-daemon` worker | When `[image] sanitize = true`, raster images are privacy-stripped and re-encoded by `y4p-image` (original kept on any failure). By default the bytes pass through untouched |
| 7. Fingerprint | `y4p-daemon` worker | SHA3-256 over the **final** bytes |
| 8. Persist | `y4p-daemon` worker | `ClipboardDb::insert_with_hash`: whole-payload deduplication, inline `content` for every non-cache row, `<hash>.cache` for cache-class payloads, then the cache quota check |

> **Invariant — the hash describes what is stored.** The fingerprint is computed last, after optional image routing, so a deduplicated row and its cache file are always keyed by the bytes actually saved.

> **Invariant — the sensor never blocks on a converter.** Optional image transcoding and hashing run on the single `DbWorker` thread, so jobs are also persisted strictly in arrival order. A slow conversion (bounded by the timeout, throttle and breaker in [06](06_local_image_pipeline.md)) delays the jobs queued behind it, and that queue is bounded: see [03](03_concurrency_and_memory.md) section 2.1.

---

## 6. What y4p Is Not

Stating the boundaries is part of stating the design: several plausible features are deliberately absent, and a patch that adds one is an architecture change, not an enhancement.

| Not this | Why not, and what happens instead |
| :--- | :--- |
| **An image converter or editor** | The local pipeline exists only as an opt-in metadata-stripping step for untrusted rasters (`[image] sanitize = true`), not to transcode on demand. When enabled it produces `image/png`; egress never re-encodes and never matches one image format against another ([06](06_local_image_pipeline.md), [05](05_filter_pipeline_and_app_rules.md) section 2.1) |
| **A forensic or auditing archive** | History is bounded by `max_history` (256 by default), rotation evicts the oldest unpinned rows, and the cache is bounded by a 512 MiB LRU quota; `delete` and `wipe --force` remove records outright. There is no append-only mode, no WORM guarantee and no tamper-evident log ([02](02_hybrid_storage_and_pin.md) sections 3, 4 and 8) |
| **A cross-device sync daemon** | Nothing leaves the machine: no network code, no sync protocol, no remote store. The IPC socket is a per-user Unix socket on the local filesystem ([04](04_strict_cli_and_stable_id.md) section 6) |
| **A secret manager** | Sensitive MIME types are *excluded* from storage rather than encrypted and retained. `SENSITIVE_MIME_HINTS` is a filter, not a vault; there is no master password, no per-entry encryption and no unlock state ([05](05_filter_pipeline_and_app_rules.md) section 1.1) |
| **A clipboard *history* of everything, forever** | Pin protects records from automatic rotation and cache eviction, but it is not an unlimited quota: pinned rows are exempt from the count, unpinned ones are not, and disk usage follows what is actually copied ([02](02_hybrid_storage_and_pin.md) sections 3 and 8) |

---

## 7. What to Read Next

| What you want to know | File |
| :--- | :--- |
| Why one thread multiplexes Wayland and IPC, how the start-up lock hands over from one daemon to the next, how the MIME allowlist and per-selection fallback work, why self-ingestion needs no counter, how egress uses `sendfile(2)`, and how `paste-from -l` lists types | [01 — Wayland Protocol & Streaming I/O](01_wayland_streaming_io.md) |
| Why text and images live in different stores, how migrations stay safe, how legacy chunk rows remain readable, how rotation became near-O(1), how pinned records are carved out of the quota, and how the cache LRU works | [02 — Hybrid Storage & Pin Protection](02_hybrid_storage_and_pin.md) |
| Why every write funnels through one worker, how WAL lets readers run alongside it, and how the ingestion queue sheds under overload | [03 — Concurrency & Memory Reclamation](03_concurrency_and_memory.md) |
| Why display position (MRU) and identity (stable ID) are different numbers, why search avoids a window function, what the `--raw` contract promises, and why the CLI rejects unknown flags | [04 — Stable IDs & the Strict CLI](04_strict_cli_and_stable_id.md) |
| How ingress filters sensitive and redundant offers, what is and is not modified before storage, and how `y4p.toml` is parsed | [05 — Filter Pipeline & Dynamic Egress](05_filter_pipeline_and_app_rules.md) |
| How opt-in image sanitisation sniffs, sandboxes, time-boxes and circuit-breaks the converter | [06 — Local Image Pipeline & Defence-in-Depth](06_local_image_pipeline.md) |

Repository governance (not architecture) lives outside this directory: [`.github/POLICY.md`](../.github/POLICY.md).
