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

1. **Storage Invariant**:
   `crates/storage/` remains untouched (0 lines changed) unless an unavoidable schema migration or storage layout change is formally proven and approved by the human supervisor.
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

- [ ] **v0.7.1: Hygiene, Queue Backpressure & Benchmark Baseline**
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

- [ ] **v0.7.2: Buffer & Stream Descriptor Abstraction**
  - Design zero-copy-capable payload abstractions in `crates/core` (`PayloadSource` / descriptor wrapper).
  - Define clear routing boundaries between small inline payloads (in-memory fast path, e.g. < 4 KiB text) and large streamed payloads (descriptor / file-backed streaming path).
  - Prepare interface contracts across `y4p-wayland`, `y4p-daemon`, and `y4p-image`.

- [ ] **v0.7.3: Zero-Copy Ingestion (`splice(2)` & Stream I/O)**
  - Implement direct pipe streaming from `y4p-wayland` into `y4p-daemon` worker.
  - Leverage Linux `splice(2)` to stream data directly into temporary cache files without intermediate userspace `Vec<u8>` heap allocation.
  - Integrate streaming SHA3-256 fingerprint calculation alongside kernel streaming (via `tee(2)` or streaming block-hashing).

- [ ] **v0.7.4: Zero-Copy Image & Scratch Pipeline Integration**
  - Connect descriptor / streaming payloads with `y4p-image` sandbox (`ScratchDir`).
  - Optimise image preprocessing invocation (`magick`) with direct pipe streaming where possible.
  - Complete bidirectional streaming symmetry with the existing `sendfile(2)` egress mechanism.

- [ ] **v0.7.5 — v0.7.8: Benchmarking, Profiling & Fault Tolerance**
  - Quantitative comparison against v0.7.0 baseline in `PERFORMANCE.md` (allocations, throughput, CPU time).
  - Harden against broken pipes (`EPIPE`), compositor disconnects, malformed descriptors, and burst-induced `EMFILE`.
  - Fine-tune pipe buffer sizes (`F_SETPIPE_SZ`) and splice chunk thresholds.

- [ ] **v0.7.9: Documentation Distillation & Architecture Freeze**
  - Distil zero-copy streaming contracts, pipe lifecycles, and failure matrices into `docs/` (`01` through `06`).
  - Update architecture diagrams and sync `status.md`.
  - Freeze workspace and prepare release candidate.

- [ ] **v0.8.0: Official Major Release: The Zero-Copy Architecture**
  - Conclude the zero-copy ingestion and bounded queue milestone.
  - Verify complete test suite (300+ tests, 0 warnings, strict invariants).
  - Tag and publish official `v0.8.0` release.
