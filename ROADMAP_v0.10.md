# Roadmap: v0.10.0 — Subtraction

> **Note:** Formal release is fixed at `.N.0` exclusively. Intermediate `.N.1`–`.N.9` are development iterations.

---

## Executive Summary
Following `v0.9.0` (**Trust**), `v0.10.0` (**Subtraction**) removes over-engineered complexity identified in the architectural audit (`report.md`), cutting production code by roughly 40% whilst preserving every user-observable reliability guarantee. The core remains simple: one bounded ingestion path (`32 MiB` cap), whole-payload SHA3 deduplication, pristine raw image storage by default (with opt-in sanitisation), and `sendfile(2)` egress for cached binaries.

---

## Micro-Milestones & Cadence

### Phase 1: Ingestion & Speculative Debt Subtraction (`v0.9.1` – `v0.9.3`)

- [x] **v0.9.1: Ingestion Simplification (`MAX_PAYLOAD_BYTES = 32 MiB`)**
  - Lower `MAX_PAYLOAD_BYTES` from `256 MiB` to `32 MiB`.
  - Remove `spool`, `splice(2)` / `tee(2)` ingestion plumbing, `ByteBudget`, and `Payload::Spooled` / `Stream` polymorphism; unify ingestion on bounded `DeadlineReader` (`read_bounded`) into `Vec<u8>`.
  - Retain `sendfile(2)` zero-copy egress for `<hash>.cache` files.

- [x] **v0.9.2: FastCDC Write Halt & Whole-Payload Dedup**
  - Halt all new writes to `chunks` and `manifest` tables (`cdc.rs` persistence path); unify persistence on whole-payload SHA3 deduplication (inline payloads + `<hash>.cache`).
  - Preserve read, `delete`, and `wipe` compatibility for legacy `chunks` / `manifest` rows.
  - Update `AGENTS.md` to replace the frozen `crates/storage/` prohibition with schema backwards-compatibility invariants.

- [x] **v0.9.3: Dead & Speculative Code Removal**
  - Remove `ClipboardDb::open_read_only`, `core::splice::splice_all` / `try_splice`, unused `OutputFormat::WebpLossless` / `WebpLossy` variants, and speculative `ContentLocation` notes.

### Phase 2: Image Opt-In & Cache Quota (`v0.9.4` – `v0.9.5`)

- [x] **v0.9.4: Image Sanitisation Opt-In (`[image] sanitize = false` Default)**
  - Default image conversion to off (`[image] sanitize = false` in `y4p.toml`), storing raw image bytes verbatim without requiring `magick` at runtime.
  - Simplify `crates/image` by removing file-to-file spool transcode paths and `#![allow(dead_code)]`, keeping opt-in sanitisation clean.
  - Update `y4p doctor` to warn about missing `magick` only when `sanitize = true`.

- [x] **v0.9.5: Cache Quota & Operational Bounds**
  - Enforce a bounded total disk quota for `<hash>.cache` (LRU eviction of unpinned cache files) so repeated `32 MiB` image copies cannot grow disk usage unboundedly.

- [x] **v0.9.6: Selection Type Inspection (`y4p paste-from -l` / `--list-types`)**
  - Add `-l` / `--list-types` to `y4p paste-from` using the existing Wayland Action Mode (`OfferData.mimes`) to print advertised MIME types one per line without opening a pipe.

### Phase 3: Documentation Distillation (`v0.9.9`)

- [x] **v0.9.9: Documentation Subtraction & Consolidation**
  - Synchronise `docs/`, `README.md`, and `README_ja.md` with the simplified architecture (`32 MiB` cap, whole-payload dedup, opt-in `magick`), consolidating redundant chapters where subtraction makes them obsolete.

### Phase 4: Formal Release (`v0.10.0`)

- [x] **v0.10.0: Official Major Release: Subtraction**
  - Bump workspace version and badges to `0.10.0`.
  - Verify production line-count reduction, 100% test pass, and 0 clippy warnings.
  - Cut `release/v0.10.0`, push `v0.10.0` tag, and publish GitHub Release (`RELEASE_TEMPLATE.md` format).

---

## Future Horizon: The Path to 1.0

- **v1.0.0: Boring**
  - Complete feature freeze: zero hangs, zero silent data loss, zero unprompted mutations.
  - Live Wayland compositor E2E suite passing across Sway / Hyprland; packaging & helper scripts bundled.
