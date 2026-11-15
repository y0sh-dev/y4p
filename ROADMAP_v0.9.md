# Roadmap: v0.9.0 — Trust (The Hardened Baseline)

> **Release Cadence**: Formal release is fixed strictly at `v0.9.0`. Intermediate milestones `v0.8.1`–`v0.8.9` represent structured implementation iterations.

---

## Executive Summary

`v0.9.0` stops feature accretion entirely. Following the comprehensive systems audit of `v0.8.0`, this milestone focuses exclusively on **"Trust"**: eliminating hangs, preventing silent data loss, restoring pristine image fidelity, enforcing mutual exclusion, and providing deterministic daemon lifecycles.

y4p returns to its founding promise: **what is copied is stored verbatim, never lost, never stalled, and always ready to paste**.

---

## Micro-Milestones & Cadence

### Phase 1: Anti-Hang & Data Integrity Hardening (`v0.8.1` – `v0.8.4`)

- [ ] **v0.8.1: Ingestion Read Deadline & Anti-Hang Guard (P0)**
  - Enforce non-blocking pipe ingestion with a strict deadline (`poll(2)`).
  - Guarantee stalled or malicious sending applications cannot hold `ReaderGate` slots indefinitely (preventing the 8-reader deadlock).
  - Surface read timeouts explicitly as accounted sheds in `y4p status`.

- [ ] **v0.8.2: Cache Integrity & Silent Data Loss Eradication (P0)**
  - Propagate `cache.store` errors prior to database row insertion; never insert a metadata record if the binary payload write fails.
  - Verify file existence in `locate_content` before handing descriptors to egress, returning explicit errors on missing disk files.

- [ ] **v0.8.3: APNG Animation Preservation (P0)**
  - Detect `acTL` chunks in incoming PNG payloads (0-allocation header check).
  - Bypass ImageMagick transcoding for APNGs, preventing multi-frame sequences from being flattened into single-frame static PNGs.

- [ ] **v0.8.4: Daemon Lifecycle & Socket Hardening (P0)**
  - Ensure daemon exits with non-zero status upon compositor disconnect or unrecoverable error.
  - Eliminate socket unlink races during daemon restarts via abstract namespace sockets (`\0y4p.sock`) or advisory locking (`flock`).
  - Bundle standard `systemd` user service unit with `Restart=on-failure`.

### Phase 2: Unified IPC, Memory Purge & Ingress Recovery (`v0.8.5` – `v0.8.8`)

- [ ] **v0.8.5: IPC Daemon Unification & Memory Purge (P0)**
  - Route `wipe` and `delete` through daemon IPC to eliminate concurrent CLI SQLite mutations and file cache deletion races.
  - Trigger daemon-side `PRAGMA shrink_memory;` and `malloc_trim(0)` on `wipe`, shrinking idle daemon RSS back to process baseline (~4 MiB).

- [ ] **v0.8.6: Ingress Fallback Array & Allowlist Selection (P1)**
  - Replace single-MIME selection with an ordered candidate array; if a chosen offer returns EOF or fails, fallback to the next available candidate.
  - Replace the permissive catch-all in `select_mime` with a strict MIME allowlist, discarding untyped application blobs.

- [ ] **v0.8.7: Self-Ingestion Simplification & Sensitive Case Normalisation (P1)**
  - Deprecate fragile `provider_locks` counters; rely on content-hash deduplication for self-copy suppression without dropping user copies.
  - Enforce case-insensitive matching (`to_ascii_lowercase()`) across all sensitive MIME backstops.

- [ ] **v0.8.8: Live Compositor E2E Suite & System Observability (P1/P2)**
  - Establish a headless compositor (Sway / Cage) integration test matrix verifying real Chromium (plain/rich), Firefox, GIMP, and large payload copies.
  - Implement `y4p doctor` (environment, compositor, DB, socket, quota checks) and `y4p stats` (cache byte volume, entry breakdown, shed counts).

### Phase 3: Documentation Distillation & Architecture Freeze (`v0.8.9`)

- [ ] **v0.8.9: Documentation Distillation & Protocol Freeze**
  - Synchronise `docs/00`–`06` with deadline, IPC, and cache integrity guarantees.
  - Explicitly document operational boundaries ("What y4p is NOT": not an arbitrary image converter, not a forensic archive).
  - Archive deep performance experiments to `docs/history/` and freeze core invariants.

### Phase 4: Formal Release (`v0.9.0`)

- [ ] **v0.9.0: Official Major Release: Trust**
  - Bump workspace version across all crates to `0.9.0`.
  - Update version badges, verify 100% test pass on live compositor suite with 0 compiler/clippy warnings.
  - Cut release branch `release/v0.9.0`, publish tag and GitHub Release.

---

## Future Horizon: The Path to 1.0

- **v0.10.0: Subtraction**
  - Lower `MAX_PAYLOAD_BYTES` to 32 MiB, eliminating spool/splice ingestion complexity.
  - Halt new writes to FastCDC chunk tables; unify persistence on whole-payload SHA3 dedup.
  - Default image conversion to off (pristine raw byte storage); reduce production codebase by ~40%.
- **v1.0.0: Boring**
  - Complete feature freeze. Guaranteed zero hangs, zero silent data loss, zero unprompted mutations.
  - Long-term maintenance mode: a rock-solid, invisible Wayland utility.
