# Project Instructions — y4p-rs

## 0. Non-negotiables (Project-Specific)
- Global `~/.agent-protocols/CARD.md` is the authoritative safety set. On conflict, follow the more restrictive rule and report it.
- **Storage Layer Invariant**: Do not modify `crates/storage/` (0 lines changed).
- **Zero External Dependencies**: Do not add external crates to `Cargo.toml` without human approval.
- **Strict Panic-Free**: Do not introduce `unwrap()`, `expect()`, or `panic!()`. Keep Clippy warning-free under `-D warnings`.
- **Language**: Deliverables (code, comments, commits, PRs, docs) strictly in British English.

## 1. First 5 Minutes (Immediate Verification)
1. Verify working branch: `git status && git branch --show-current` (Never edit directly on `main`).
2. Run standard verification: `bash scripts/check.sh` (Runs workspace tests + strict Clippy; exit 0 required).
3. If the same failure signature appears twice, stop and evaluate (Hard limiter: 5 attempts max).

## 2. Roles & Archetypes
- **Orchestrator (Gemini)**: Macro specifications, Pre-Commit Deliberation reports, human-hub task delegation.
- **Implementer (OpenCode / Coder)**: Fast, minimal diffs, local test verification (`scripts/check.sh`), panic-free compliance.
- **Reasoner (Auditor)**: Deep logic audit on concurrency, memory bounds, error handling, and soundness.

## 3. Architecture & Workspace Topology
- Inward-only layered dependency: `core` <- `storage` / `image` / `wayland` <- `daemon` / `cli`.
- Persisted payloads: Hybrid storage (SQLite v4 metadata + SHA3-256 FastCDC chunked cache).
- Kernel egress: Zero-copy pipe streaming via `splice(2)` / `vmsplice(2)` / `sendfile(2)`.

## 4. Key Verification Commands
- **Primary Check**: `bash scripts/check.sh` (Standard gate: tests + strict linter).
- **Granular Test**: `cargo test -p y4p-<crate>` | Single test: `cargo test -p y4p-<crate> -- <test_name>`
- **Strict Lint**: `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets -- -D warnings`
- **Deep Sanitizers**: `cargo miri test -p y4p-core` | `RUSTFLAGS="-Zsanitizer=address" cargo test -p y4p-core`

## 5. Fallback Safety Set
If `~/.agent-protocols/CARD.md` is unavailable:
1. Never commit or push directly to `main`; always use topic branches.
2. Irreversible or outward actions (tag, release, PR merge, push) require explicit human approval.
3. Dialogue in Japanese; deliverables (code, commits, PRs, docs) in British English.
4. Stop on 5 consecutive failures of the same error signature. Report missing global SSOT.
