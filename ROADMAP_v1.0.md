# Roadmap: v1.0.0 — Boring

> **Note:** Formal release is fixed at `.N.0` exclusively. Intermediate `v0.10.1`–`v0.10.6` are development iterations.

---

## Executive Summary
Following `v0.10.0` (**Subtraction**), `v1.0.0` (**Boring**) freezes all new features and turns reliability into the product. The release upholds three guarantees: **never crash or hang** (zero hangs), **never lose or block data silently** (zero silent data loss), and **never mutate payloads unprompted** (opt-in image sanitisation).

---

## Micro-Milestones & Cadence

### Phase 1: Feature Freeze & Final Static Audit (`v0.10.1` – `v0.10.2`)

- [x] **v0.10.1: Feature Freeze, Final Static Audit (`report_finally.md`), & P0/P1 Fixes**
  - Freeze all new feature additions; accept bug fixes and reliability hardening only.
  - Clear all P0/P1 findings from `report_finally.md`:
    1. Bound `accept_and_dispatch` IPC socket reads (`SO_RCVTIMEO` + line cap) so a stalled client cannot hang the daemon poll loop (`crates/daemon/src/ipc.rs`).
    2. Replace `std::thread::spawn` in `DbWorker::spawn` with fallible `std::thread::Builder::spawn` returning `DaemonExit::StartFailed` on failure (`crates/daemon/src/worker.rs`, `crates/daemon/src/lib.rs`).
    3. Enforce `MAX_PAYLOAD_BYTES` (32 MiB) in `y4p store` (`read_bounded`) and `ClipboardDb::insert_with_hash` (`crates/cli/src/store.rs`, `crates/storage/src/lib.rs`).
    4. Align `IPC_STATUS_TIMEOUT_MS` / `IPC_MUTATION_TIMEOUT_MS` above the maximum `IMAGE_TRANSCODE_TIMEOUT_SECS` (15 s) writer lock hold (`crates/core/src/constants.rs`).
  - **Clearance Criteria**: `P0 = 0` and `P1 = 0` (P2 items deferred under 1.0 feature freeze).

- [x] **v0.10.2: `y4p doctor` & `systemd` Live Verification**
  - Verify `y4p doctor` passes across live Sway and Hyprland sessions (verified on live Hyprland: 17 passed, 0 warnings, 0 failed; Sway unverified locally as `sway` is not installed).
  - Verify `contrib/systemd/y4p.service` startup, restart, and socket hand-over behaviour (`systemd-analyze verify` exit 0; `systemctl --user status y4p.service` active/running).

### Phase 2: Live Compositor E2E Matrix (`v0.10.3` – `v0.10.4`)

- [x] **v0.10.3: Expand Live Compositor E2E Suite (`scripts/e2e_compositor.sh`)**
  - Cover the minimal real-world matrix: plain and rich text (`text/html`, `text/uri-list`), raster and animated images (`PNG`, `JPEG`, `GIF`, `APNG`, `WebP` raw by default and sanitised when enabled), `32 MiB` cap and oversized rejection, burst backpressure, `512 MiB` cache LRU eviction (with pin protection), daemon restart persistence, and `y4p paste-from -l` / `--list-types`.

- [x] **v0.10.4: 100% E2E Pass Across Sway & Hyprland**
  - Confirm the expanded E2E suite passes cleanly on headless Sway and Hyprland compositors (`E2E_USE_LIVE_WAYLAND=1 bash scripts/e2e_compositor.sh` passed `22 passed, 0 failed` on live Hyprland; Sway unverified locally as `sway` is not installed).

### Phase 3: Packaging, Helpers, & Final Documentation (`v0.10.5` – `v0.10.6`)

- [ ] **v0.10.5: Packaging & Helper Scripts**
  - Bundle `contrib/packaging/PKGBUILD` in Git (AUR registration remains out of scope whilst new AUR registrations are suspended; Nix is out of scope).
  - Bundle minimal `fzf` helper script(s) under `contrib/examples/` and reference `y4p-rofi` as the companion Rofi integration.

- [ ] **v0.10.6: Final 1.0 Documentation Synchronisation**
  - Synchronise `README.md`, `README_ja.md`, and `docs/` with installation, `y4p.toml`, `systemd`, `PKGBUILD`, and helper/companion references.

### Phase 4: Formal Release (`v1.0.0`)

- [ ] **v1.0.0: Official Major Release: Boring**
  - Bump workspace version and badges to `1.0.0`.
  - Verify `bash scripts/check.sh` (100% tests passing, 0 Clippy warnings), baseline smoke bench, and the live E2E suite.
  - Cut `release/v1.0.0` (retaining the `release/*` branch), push `v1.0.0` tag, and publish GitHub Release (`RELEASE_TEMPLATE.md` format).
