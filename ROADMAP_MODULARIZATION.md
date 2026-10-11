# Roadmap: v0.6.x — The Great Modularization (Workspace & Separation of Concerns)

## 1. Vision & Architectural Philosophy

Following the formal completion of the Local Image Pipeline (`v0.6.0`), the `v0.6.x` development cycle executes **The Great Modularization**.
The core goal is to transition the monolithic `src/` layout into a clean **Cargo Workspace**, physically enforcing dependency acyclicity, reducing incremental build latency, isolating regression blast radiuses, and preparing stream boundaries for zero-copy primitives.

---

## 2. Living Document & Evolutionary Revision Protocol

> [!NOTE]
> **Living Document Principle & Unknown Horizon Management**
> This roadmap represents an optimal macro plan constructed on the premise of smooth, frictionless execution. In practical software engineering, however, refactoring monolithic code into isolated crates may expose compiler nuances, cyclic dependencies, or OS-level subtleties that cannot be fully known in advance.
>
> - **Malleability by Human Alignment**: This document is an evolving, living roadmap rather than an unyielding dogma. When unforeseen technical realities or new requirements emerge, milestones, crate scopes, and scheduling may be dynamically rewritten or adjusted upon mutual discussion and agreement with the human supervisor.
> - **Progressive Elaboration**: Prior to kicking off each individual milestone, requirements and invariants will be reviewed with the human supervisor, progressively enriching this roadmap with high-resolution specifications and edge-case boundaries.

---

## 3. Target Workspace Topology

```text
y4p/ (Project Root)
├── Cargo.toml          <- Root workspace manifest & common build configurations
├── src/
│   └── main.rs         <- Thin binary entrypoint (wires CLI and daemon commands)
├── crates/
│   ├── core/           <- y4p-core: Shared domain types, constants, error primitives, XDG resolution
│   ├── storage/        <- y4p-storage: SQLite database facade, file cache, and schema migrations
│   ├── wayland/        <- y4p-wayland: ext-data-control-v1 protocol handlers and streaming I/O
│   ├── image/          <- y4p-image: Magic sniffing, scratch directory sandbox, and transcode engine
│   ├── daemon/         <- y4p-daemon: Resident lifecycle, IPC socket listener, and background worker
│   └── cli/            <- y4p-cli: Strict command-line argument parsing and human-readable formatting
├── docs/
└── scripts/
```

### Invariants:
1. **Root Binary Compatibility**: `src/main.rs` remains at the root level as a thin wiring entrypoint. Standard CLI commands (`cargo run`, `cargo build --release`, `y4p <cmd>`) retain 100% operational compatibility.
2. **Crate Autonomy**: Each workspace member under `crates/` maintains its own `Cargo.toml`, internal `src/lib.rs`, and isolated `tests/`.
3. **Strict Inward Dependency Flow**: Crate dependencies strictly flow inward towards `core`. Circular dependencies are physically prohibited at the compiler level.
4. **Zero Panic Guarantee**: All crates retain `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`.

---

## 4. Progress Tracking & Milestone Cadence (v0.6.1 — v0.7.0)

Visual progress checklist across the modularization development cycle:

- [x] **v0.6.0: Local Image Pipeline (Baseline Release)**
  - Hardened Wayland ingestion, isolated `ScratchDir` sandbox, hierarchical watchdogs, distilled documentation.

- [x] **v0.6.1: Workspace Foundation & `crates/core` Extraction**
  - Initialise root `[workspace]` manifest in `Cargo.toml`.
  - Physically extract `src/core/` into `crates/core/` (`y4p-core`).
  - Wire path dependency (`y4p-core = { path = "crates/core" }`) while preserving binary entrypoint in `src/main.rs`.

- [x] **v0.6.2: Decoupling of `crates/storage`**
  - Extract SQLite facade and filesystem blob cache into `crates/storage/` (`y4p-storage`).
  - Purify DB / Cache boundaries and schema migration isolation.

- [x] **v0.6.3: Decoupling of `crates/image`**
  - Extract image sniffing, `ScratchDir` sandbox, and transcode engine into `crates/image/` (`y4p-image`).
  - Ordering (agreed): `wayland` ingestion calls into the image pipeline (`route_for_ingest`), so `image` must become a crate first for the `wayland` boundary to compile in the next step.

- [x] **v0.6.4: Decoupling of `crates/wayland`**
  - Extract Wayland protocol handlers and streaming I/O into `crates/wayland/` (`y4p-wayland`), depending on `y4p-image`.

- [x] **v0.6.5: Decoupling of `crates/cli`**
  - Extract the command-line frontend (argument parsing, dispatch, formatting) into `crates/cli/` (`y4p-cli`), depending on `y4p-core` and `y4p-storage`.
  - Ordering (agreed, "Plan A"): CLI and daemon are extracted in separate milestones for incremental safety. `cli` has no dependency on `daemon`/`wayland` (it hands back `CliAction` to the composition root), so it goes first.

- [ ] **v0.6.6: Decoupling of `crates/daemon`**
  - Extract the daemon event loop into `crates/daemon/` (`y4p-daemon`), depending on `y4p-wayland`, `y4p-storage` and `y4p-core`; `src/main.rs` becomes the thin composition root.

- [ ] **v0.6.7: Test Externalisation & Zero-Copy Preparation Stage 2 (Stream I/O Contract)**
  - Relocate inline unit tests to dedicated `tests/` integration directories (`src/` purification).
  - Refine inter-crate streaming I/O and zero-allocation contracts.

- [ ] **v0.6.8: Pre-Distillation Hardening & Feature Polish**
  - System-wide regression hardening / reserved slot for future capability insertion.
  - Zero-Copy Preparation Stage 2 (Buffer Lifecycle Unification), carried over from the former v0.6.7: align buffer lifetimes and descriptor passing mechanisms across crate boundaries. *(Placement pending confirmation with the human supervisor.)*

- [ ] **v0.6.9: Documentation Distillation & Workspace Architecture Freeze**
  - Distil workspace layout, crate dependencies, and architecture invariants into `docs/`.
  - Prepare for `v0.7.0` release branch.

- [ ] **v0.7.0: Official Major Release: The Great Modularization**
  - Tag and publish official `v0.7.0` release.
