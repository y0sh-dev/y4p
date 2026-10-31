# Project Instructions — y4p-rs

## 0. Non-negotiables (Project-Specific)
- Global `~/.agent-protocols/CARD.md` is the authoritative safety set. If this file conflicts with CARD, follow the more restrictive rule and report it.
- **Zero External Dependencies**: Do not add external crates to `Cargo.toml` without human approval.
- **Storage Layer Invariant**: Do not modify `crates/storage/` (0 lines changed).
- **Strict Panic-Free**: Do not introduce `unwrap()`, `expect()`, or `panic!()`.

## 1. First 5 Minutes (Immediate Verification)
1. Verify working branch: `git status && git branch --show-current` (Do not edit directly on `main`).
2. Run standard verification: `bash scripts/check.sh` (Tests workspace + strict Clippy; exit 0).
3. If the same failure appears twice, stop and report (Hard limiter: 5 attempts max).

## 2. Commands & Detailed Rules
- Full verification command reference: `.protocols/commands.md`
- Detailed project invariants & exceptions: `.protocols/project_invariants.md`

## 3. Architecture & Workspace Topology
- Inward-only layered dependency: `core` <- `storage` / `image` / `wayland` <- `daemon` / `cli`.
- Persisted payloads: Hybrid storage (SQLite v4 metadata + SHA3-256 chunked cache).
- Kernel egress: Zero-copy pipe streaming via `splice(2)` / `vmsplice(2)` / `sendfile(2)`.

## 4. Load When (Dynamic Routing)
| Situation / Task | File to Load |
| :--- | :--- |
| Multi-agent collaboration / Hand-off | `.protocols/collaboration.md` |
| Deep testing / Sanitizers / Miri | `.protocols/commands.md` |
| Roadmap planning / Distillation | `docs/templates/ROADMAP_TEMPLATE.md` |

## 5. Fallback Safety Set
If `~/.agent-protocols/CARD.md` is unavailable:
1. Never commit or push directly to `main`; always use topic branches.
2. Irreversible or outward actions (tag, release, PR merge, push) require explicit human approval.
3. Dialogue in Japanese; deliverables (code, commits, PRs, docs) in British English.
4. Stop on 5 consecutive failures of the same error signature. Report missing global SSOT.
