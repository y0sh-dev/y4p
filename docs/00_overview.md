# y4p Internals — Overview

This is the entry point for `y4p`'s internal documentation.

The [README](../README.md) tells you *what* the tool does, and how to run it. Everything under `docs/` tells you *why* it's built the way it is.

Each file below is a self-contained theme. Read the one your question actually falls under — you don't need to read all four in order.

---

## 1. Two Faces, One Binary

`y4p` is one binary that behaves differently depending on how you invoke it.

Run it as `y4p daemon`, and it becomes a long-lived background process. It owns the Wayland connection. It owns the SQLite write path. It just sits there, quietly, until something happens.

Run it as anything else — `list`, `copy-to`, `pin`, whatever — and it's a short-lived CLI process. Most of the time it just reads the database directly and prints something. Occasionally, for actions only the daemon can perform, it sends a one-shot command over a Unix socket and exits.

Here's roughly how that looks:

```text
                     y4p daemon  (long-lived background process)

  Wayland compositor                                    $XDG_RUNTIME_DIR
  (ext-data-control-v1)                                 /y4p.sock  (IPC)
          |                                                    |
          v                                                    v
   wayland::handlers  ---(mpsc)-->  daemon::worker      daemon::ipc
     (Ingress)                    (single DB writer)   (Exit / Restore / Status)
          ^                              |                     |
          |                              v                     |
          |                     storage::ClipboardDb <---------+
          |                       SQLite (WAL)  +
          |                       ~/.cache/y4p/
          |                              |
          +----(Egress: handle_restore_request)-------+
                data_control::source


                     y4p <command>  (short-lived CLI process)

          cli::*  ------------------->  storage::ClipboardDb
                                          (direct read for
                                           list / search / show)

          cli::*  --(IPC, one-shot)-->  the daemon above
                                          (for copy-to / pause /
                                           anything daemon-owned)
```

Four layers. Each with one job.

`cli/` handles argument parsing, validation, and human-readable output. It never touches Wayland directly. For a plain read — `list`, `search`, `show` — it talks to `storage` on its own. For an action only the daemon can perform — restoring to the live clipboard, pausing ingestion — it goes through IPC instead.

`daemon/` owns process lifecycle, the IPC socket, and `DbWorker` — the one thread allowed to hold a write connection to SQLite. More on why that matters in [03](03_concurrency_and_memory.md).

`wayland/` speaks `ext-data-control-v1` to the compositor. Ingesting new offers, serving history back out, and the pipe/buffer plumbing in between. See [01](01_wayland_streaming_io.md).

`storage/` is a facade — `ClipboardDb` — over two things kept deliberately apart: `db.rs` (pure SQL, no filesystem I/O) and `cache.rs` (the on-disk binary cache, no SQLite at all). Schema migrations live in `schema.rs`. See [02](02_hybrid_storage_and_pin.md).

`core/` sits underneath all four, as shared plumbing — XDG paths, the history-limit env var, the exit flag. It depends on nothing else in the project. That's what keeps it usable from any layer, without ever creating a cycle back into one.

---

## 2. How the Modules Depend on Each Other

```text
                              main
                 (wires everything together at startup)
                  /        |          |          \
                 v         v          v           v
              core     storage     wayland      daemon
                          ^           |            |
                          |           |            |
                          |     storage::           |
                          |   ContentLocation        |
                          |     (one type)           |
                          +-----------<--------------+

              cli  ------------------>  storage
              cli  ------------------>  core
              daemon  ---------------->  wayland
```

The one direction worth noticing is `wayland --> storage::ContentLocation`.

The egress path needs to know whether a record's payload lives inline in SQLite, or out in the file cache. So it depends on that one type — not on the rest of storage's internals.

Nothing under `storage/` or `core/` ever imports from `wayland/`, `daemon/`, or `cli/`. Dependencies only flow inward, toward `core`.

That's a deliberate constraint. It's what lets `storage` be tested and reasoned about without ever having to stand up a real Wayland connection.

---

## 3. What to Read Next

Pick the row that matches what's actually on your mind. Each one is a self-contained theme — there's no need to read the others first.

| What you want to know | File to read |
| :--- | :--- |
| Why the daemon multiplexes Wayland and IPC on a single thread, why ingestion hashes and buffers in one pass, and how `provider_locks` stops the daemon from re-ingesting its own restores | [01 — Wayland Protocol & Streaming I/O](01_wayland_streaming_io.md) |
| Why text and large binaries live in two different storage engines, how schema migrations stay safe across upgrades, and how Pin protection carves pinned records out of the rotation limit | [02 — Hybrid Storage & Pin Protection](02_hybrid_storage_and_pin.md) |
| Why every database write funnels through one worker thread, and why the daemon explicitly returns memory to the OS after a large payload | [03 — Concurrency & Memory Reclamation](03_concurrency_and_memory.md) |
| Why display order (MRU) and identity (stable ID) are deliberately two different numbers, and why the CLI treats an unrecognized flag as an error, never a guess | [04 — Stable IDs & the Strict CLI](04_strict_cli_and_stable_id.md) |
