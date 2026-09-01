
<div align="center">

# y4-clipboardMN

**Unified Wayland Clipboard Infrastructure.**

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://rust-lang.org)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Wayland-lightgerm.svg)
[![Version](https://img.shields.io/badge/version-1.0.0-green.svg)](https://github.com/y0sh-dev/y4-clipboardMN/releases/latest)

`y4-clipboardMN` is a high-performance, standalone clipboard manager engineered for Wayland. It consolidates monitoring (Ingress), serving (Egress), and persistence into a single binary, eliminating the instability inherent in fragmented toolchains.

Built with a focus on **Absolute Integrity** and **Resource Efficiency**, it handles everything from tiny text snippets to massive 70MB+ lossless images with zero-latency response.

</div>

---

## Design Philosophy

### 1. Unified Lifecycle Management
By integrating the monitor and the provider into a single daemon process, `y4-clipboardMN` eliminates synchronization drift and zombie processes. Communication is handled via a strict IPC model over Unix Domain Sockets.

### 2. High-Capacity Resilience
Engineered to handle extreme payloads. Utilizing page-aligned memory buffers and single-pass SHA3-256 hashing, the system processes large binary data at near-kernel speeds while maintaining a minimal memory footprint.

### 3. Ironclad Persistence
Powered by SQLite in WAL mode. The hybrid storage strategy ensures that metadata remains searchable and fast, while large binary assets are offloaded to a dedicated, deduplicated filesystem cache.

---

## Key Features

- **Unified Daemon**: Centralized management of all clipboard operations.
- **Hybrid Storage**: Metadata and text in SQLite; large binaries in `~/.cache/y4-clipboard/`.
- **Stable ID System**: Persistent database identifiers for seamless integration with external scripts (e.g., Rofi, Fzf).
- **Strict CLI**: A "Prosecutor-style" argument parser that rejects malformed or unauthorized inputs.
- **Security Focused**: Enforced filesystem permissions (700/600) and sensitive MIME type filtering.

---

## Quick Start

### 1. Build from Source
```bash
cargo build --release
sudo cp target/release/y4-clipboard /usr/local/bin/
```

### 2. Start the Daemon
Initialize the monitor and IPC listener:
```bash
y4-clipboard daemon
```

### 3. Basic Operations
```bash
y4-clipboard list 0-50 --id    # List history with persistent IDs
y4-clipboard copy-to --id 42   # Restore a specific item via IPC
```

---

## Command Reference

| Command | Description |
| :--- | :--- |
| `daemon` | Start background monitor and IPC socket listener. |
| `list` | Display history metadata. Supports ranges and raw output. |
| `copy-to` | Restore a record to the clipboard via IPC. Supports MRU logic. |
| `show` | Inspect record content. Supports `--raw` for binary extraction. |
| `store` | Ingest stdin to database and sync with the active daemon. |
| `search` | Keyword scan across history using SQLite indexing. |
| `paste-from` | Direct OS clipboard access, bypassing the database. |
| `delete` | Physically remove a specific record from storage. |
| `wipe` | Purge all history and optimize storage via VACUUM. Non-interactive (no confirmation prompt) — requires `--force`/`-f`; runs without it fail with exit code 1. |

---

## Technical Specifications

- **Language**: Rust (Zero-cost abstractions)
- **Storage**: SQLite 3 (WAL mode, Memory-mapped I/O)
- **Hashing**: SHA3-256 (FIPS 202)
- **Protocol**: Wayland `ext-data-control-v1`
- **Memory**: Page-aligned I/O, `malloc_trim` optimization

---

## License

GPL-3.0-or-later

Copyright (c) 2026 yosana (y0sh-dev)

---

## AI Usage Disclosure

For our policy on using Generative AI (LLMs), please refer to 
the shared guidelines documented in [AI.md](AI.md).

