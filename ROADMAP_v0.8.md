# Roadmap: v0.7.x — The Zero-Copy Architecture (High-Throughput Streaming & Resource Bounds)

## 1. Vision & Architectural Philosophy

Following the formal completion of The Great Modularization (`v0.7.0`), the `v0.7.x` development cycle executes **The Zero-Copy Architecture**.

While `y4p` already achieves true kernel-level zero-copy on the egress path (pasting via `sendfile(2)` directly from cache files to client descriptors), the ingestion path currently drains compositor pipes into userspace heap buffers (`Vec<u8>`) across multiple crate boundaries. In `v0.8.0`, `y4p` establishes full bidirectional streaming symmetry by introducing kernel-level zero-copy ingestion (`splice(2)` / descriptor passing), bounded queue backpressure, and a quantitative benchmark suite.

---

## 2. Living Document & Evolutionary Revision Protocol

> [!NOTE]
> **Living Document Principle & Unknown Horizon Management**
> This roadmap represents an optimal macro plan constructed on the premise of smooth, frictionless execution. In practical systems programming, however, operating system limits (pipe buffer capacity, `splice(2)` descriptor requirements, signal handling, and external tool execution) may reveal nuances that cannot be fully known in advance.
>
> - **Malleability by Human Alignment**: This document is an evolving, living roadmap rather than an unyielding dogma. When unforeseen technical realities or new requirements emerge, milestones, crate scopes, and scheduling may be dynamically rewritten or adjusted upon mutual discussion and agreement with the human supervisor.
> - **Progressive Elaboration**: Prior to kicking off each individual milestone, requirements and invariants will be reviewed with the human supervisor, progressively enriching this roadmap with high-resolution specifications and edge-case boundaries.

---

## 3. Architecture Invariants & Design Principles

1. **Storage Invariant & Evolutionary Exception**:
   `crates/storage/` remained strictly untouched (0 lines changed) across `v0.7.1` through `v0.7.3`. In `v0.7.4 Part 2`, this invariant is formally lifted upon agreement with the human supervisor to execute extreme zero-copy storage optimization (direct file/descriptor adoption), eliminating userspace memory mapping whilst rigorously preserving data safety and schema invariants.
2. **Zero Panic Guarantee**:
   `clippy::unwrap_used`, `clippy::expect_used`, and `clippy::panic` remain denied across all production workspace members via workspace lints.
3. **Strict Crate Boundaries & Inward Flow**:
   Crate dependencies strictly flow inward towards `core`. Inter-crate interfaces communicate through clean domain types and descriptor abstractions rather than concrete leaks.
4. **Resource Bounds & Fault Tolerance**:
   Ingestion queues must be bounded to prevent heap exhaustion and file descriptor exhaustion (`EMFILE`) under burst clipboard activity.
5. **Bidirectional Symmetry**:
   Ingestion and egress must achieve symmetric zero-copy capability, with graceful fallbacks for unsupported filesystems or payload formats.
6. **Language Standard**:
   All code comments, Git commit messages, Pull Requests, and documentation must adhere strictly to **British English** (e.g. *optimise*, *behaviour*, *initialise*, *serialise*).

---

## 4. Progress Tracking & Milestone Cadence (v0.7.1 — v0.8.0)

Visual progress checklist across the zero-copy development cycle:

- [x] **v0.7.1: Hygiene, Queue Backpressure & Benchmark Baseline**
  - **Hygiene**:
    - Fix `crates/cli/src/help.rs` (`awk '{print $1}'` shell escaping in examples).
    - Remove unwired / dead code paths carried over from prior iterations (`open_read_only`).
    - Correct RTF wording in `y4p.toml.example`.
    - Fix badge colour typo (`lightgerm` -> `lightgreen`) in `README.md` and `README_ja.md`.
  - **Queue Backpressure**:
    - Replace the unbounded `mpsc::channel` between the Wayland ingestion thread and the database worker with a bounded channel.
    - Implement load-shedding / backpressure policy to protect memory and FD capacity under rapid clipboard churn.
  - **Benchmark Baseline**:
    - Implement a structured quantitative benchmarking suite (Criterion or lightweight harness).
    - Profile v0.7.0 baseline across text ingestion, raster image conversion, SHA3-256 fingerprinting, and SQLite transactions.
    - Establish `PERFORMANCE.md` to document baseline latency, memory allocations, and throughput.

- [x] **v0.7.2: Buffer & Stream Descriptor Abstraction**
  - Design zero-copy-capable payload abstractions in `crates/core` (`PayloadSource` / descriptor wrapper).
  - Define clear routing boundaries between small inline payloads (in-memory fast path, e.g. < 4 KiB text) and large streamed payloads (descriptor / file-backed streaming path).
  - Prepare interface contracts across `y4p-wayland`, `y4p-daemon`, and `y4p-image`.

- [x] **v0.7.3: Zero-Copy Ingestion (`splice(2)` & Stream I/O)**
  - Implement direct pipe streaming from `y4p-wayland` into `y4p-daemon` worker.
  - Leverage Linux `splice(2)` to stream data directly into temporary cache files without intermediate userspace `Vec<u8>` heap allocation.
  - Integrate streaming SHA3-256 fingerprint calculation alongside kernel streaming (via `tee(2)` or streaming block-hashing).

- [x] **v0.7.4: Zero-Copy Image, Scratch Pipeline & Extreme Storage Optimization**
  - **Part 1 (PR 1) — Zero-Copy Image & Scratch Pipeline Integration**:
    - Connect descriptor / spooled payloads with `y4p-image` sandbox (`ScratchDir`).
    - Optimise image preprocessing invocation (`magick`) with direct pipe streaming where possible.
    - Complete bidirectional streaming symmetry with the existing `sendfile(2)` egress mechanism.
  - **Part 2 (PR 2) — Extreme Storage Optimization (Zero-Copy & File-Backed Adoption)**:
    - Lift legacy `&[u8]` slice constraints in `crates/storage` to support direct file-backed / descriptor adoption (`insert_file_with_hash`).
    - Eliminate `MappedFile` (`mmap`/`munmap` unsafe blocks) and external mirrored cache layout rules.
    - Investigate SQLite incremental blob streaming (`sqlite3_blob_open`) for high-throughput zero-heap persistence.
    - Maintain strict schema compatibility, zero corruptions, and unpinned rotation invariants.

- [x] **v0.7.5: FastCDC Chunking Engine**
  - `core::cdc::FastCdc`, a content-defined chunker (4 / 16 / 64 KiB minimum / average / maximum, normalised chunking) that yields borrowed sub-slices.
  - **3.23 GiB/s with zero heap allocation** at every payload size; chunking is 4.5 % of the chunk-and-hash cost.
  - An edited 16 MiB text adds only 0.11-0.14 % new bytes to a store (`PERFORMANCE.md` section 7.5).

- [x] **v0.7.6: Schema v4 Chunked Storage**
  - `SCHEMA_VERSION = 4`: purely additive migration (`clipboard.is_chunked`, `chunks`, `manifest`); every existing row stays inline.
  - Payloads above 4 KiB that are not images are stored as reference-counted chunks, released in the same transaction that deletes a row (rotation, `delete`, `wipe`). Reassembly is verified against the recorded size.
  - Storage of an edited 16 MiB text: 33.55 MB to 16.80 MB. Cost, named openly: inserting a large non-image payload is 7-12x more CPU, hash-bound (`PERFORMANCE.md` section 9, item 5).

- [x] **v0.7.7: Pipe Buffer Tuning & Benchmarking**
  - `core::pipe::grow`: `F_SETPIPE_SZ` to 1 MiB with a tested fallback (`EPERM`/`ENOMEM` halve and retry; non-pipes untouched); `splice`/`tee` rounds of up to 1 MiB.
  - Context switches per MiB through a pipe fall from 31 to 1; the paste direction is about 35 % faster. Hash-bound paths are unchanged, as predicted.
  - Benchmark harness groups `cdc`, `pipe` and `chunked`, with context-switch counting and a same-day control run.

- [x] **v0.7.8: Fault Tolerance & Resource Hardening**
  - **Queue byte budget**: `INGEST_QUEUE_BYTE_BUDGET` = 64 MiB. Lock-free `ByteBudget` (CAS, overflow-safe) and RAII `BytePermit` carried in `ClipboardJob`; `enqueue_bounded` waits for room in the queue and the budget within the patience budget, and sheds a payload above the whole budget at once. The worker drops the permit once persistence returns. About 16 ns per enqueue, no extra allocation.
  - **One splice module**: `try_splice` and `splice_all` moved to `core::splice`, re-exported from `image::pipeline`.
  - **EMFILE diagnostics**: a pipe that cannot be allocated is counted (`record_shed_reader`) and logged.
  - **Compositor disconnection**: a fatal `guard.read()` error ends the poll loop instead of spinning on `POLLIN`.

- [x] **v0.7.9: Documentation Distillation & Architecture Freeze**
  - Zero-copy streaming contracts, pipe lifecycles and failure matrices distilled into `docs/` (`00`, `01`, `02`, `03`, `06`): the byte budget, compositor-disconnect handling, `EMFILE` shed, dynamic pipe sizing, schema v4 chunk storage, unified splice.
  - `PERFORMANCE.md` reorganised by domain, with a navigation table, an executive summary, the evolution table, memory bounds and the copy audit; resolved open questions removed.
  - READMEs simplified; workspace version synchronised to `0.7.9` (README badges stay at `0.7.0` until the formal release).

- [ ] **v0.8.0: Official Major Release: The Zero-Copy Architecture**
  - Conclude the zero-copy ingestion and bounded queue milestone.
  - Verify complete test suite (300+ tests, 0 warnings, strict invariants).
  - Tag and publish official `v0.8.0` release.
