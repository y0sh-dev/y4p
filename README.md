
<div align="center">

# y4p

**Unified Wayland Clipboard Infrastructure.**

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://rust-lang.org)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Wayland-lightgerm.svg)
[![Version](https://img.shields.io/badge/version-0.2.0-green.svg)](https://github.com/y0sh-dev/y4p/releases/latest)

`y4p` is a high-performance, standalone clipboard manager engineered natively for Wayland's `ext-data-control-v1`. It consolidates monitoring, serving, and persistence into a single binary — zero-copy `sendfile(2)` egress and a SQLite WAL-backed history mean it handles everything from a one-line snippet to a 70MB+ lossless image without becoming the reason your desktop stutters.

</div>

---

## Key Features

- **Unified Daemon**: One background process owns clipboard monitoring, serving, and storage — no synchronization drift between separate tools.
- **Hybrid Storage**: Metadata and text live in a fast, searchable SQLite database; large binaries are offloaded to a deduplicated cache at `~/.cache/y4p/`.
- **Pin Protection**: `pin`/`unpin` records to exempt them from automatic history rotation, permanently — even when `Y4P_MAX_HISTORY` is exceeded.
- **Stable ID System**: Persistent database identifiers, independent of display order, for race-free integration with external scripts (Rofi, Fzf, `awk`).
- **Strict CLI**: A "Prosecutor-style" argument parser that rejects malformed or unrecognized flags outright instead of guessing — safer for scripted, unattended use.
- **Security Focused**: Enforced filesystem permissions (700/600) and automatic filtering of sensitive clipboard MIME types (password manager offers, etc.).

---

## Quick Start

### 1. Build from Source
```bash
cargo build --release
sudo cp target/release/y4p /usr/local/bin/
```

### 2. Start the Daemon
Initialize the monitor and IPC listener:
```bash
y4p daemon
```

### 3. Basic Operations
```bash
y4p list 0-50 --id      # List history with persistent IDs
y4p copy-to --id 42     # Restore a specific item via IPC
y4p pin 3               # Protect the most recent-but-2 entry from rotation
y4p search "invoice" -i # Keyword search, printing stable IDs
```

### 4. Shell Completions
Zsh completion is provided at `completions/_y4p`. Add its directory to your `fpath` before `compinit`:
```zsh
fpath+=(/path/to/y4p/completions)
```

---

## Command Reference

| Command | Description |
| :--- | :--- |
| `daemon` | Start background monitor and IPC socket listener. |
| `status` | Query the running daemon via IPC and print its status. |
| `pause` / `resume` | Suspend/resume clipboard monitoring (private mode). |
| `list` | Display history metadata. Supports ranges, `--raw`, `--id`. |
| `search` | Keyword scan across history via SQLite indexing. Multi-keyword AND search. |
| `copy-to` | Restore a record to the clipboard via IPC. Accepts index or `--id`. |
| `show` | Inspect record content. `--raw` extracts exact binary data. |
| `store` | Ingest stdin to the database and sync with the active daemon. |
| `paste-from` | Direct OS clipboard access, bypassing the database. |
| `pin` / `unpin` | Protect/unprotect a record from automatic history rotation. |
| `delete` | Physically remove a specific record from storage (works on pinned records too). |
| `wipe` | Purge **all** history (including pinned) and run SQLite `VACUUM`. Requires `--force`/`-f`. |

Run `y4p help` for the full flag reference per command.

---

## Environment Variables

| Variable | Default | Description |
| :--- | :--- | :--- |
| `Y4P_MAX_HISTORY` | `256` | Maximum number of unpinned clipboard records retained. Invalid, zero, or negative values fall back to the default. |

```bash
export Y4P_MAX_HISTORY=500
y4p daemon
```

---

## Architecture & Design

This README is a quick-start guide. The reasoning behind `y4p`'s internals — why the daemon multiplexes I/O on one thread, how Pin protection interacts with history rotation, why stable IDs exist, and more — lives in [`docs/00_overview.md`](docs/00_overview.md).

---

## License

GPL-3.0-or-later

Copyright (c) 2026 yosana (y0sh-dev)

---

## AI Usage Disclosure

For our policy on using Generative AI (LLMs), please refer to
the shared guidelines documented in [docs/AI_POLICY.md](docs/AI_POLICY.md).
