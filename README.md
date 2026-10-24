
<div align="center">

# y4p

**Unified Wayland Clipboard Infrastructure.**

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://rust-lang.org)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Wayland-lightgreen.svg)](https://wayland.freedesktop.org)
[![Version](https://img.shields.io/badge/version-0.7.0-green.svg)](https://github.com/y0sh-dev/y4p/releases/latest)

`y4p` is a standalone clipboard manager, built natively for Wayland.

It runs as a single daemon — no separate monitor process, no separate provider process — that watches the clipboard, serves history back out, and persists everything to disk.
Engineered with strict zero-loss integrity and defence-in-depth resource controls, it seamlessly handles text streams, binary payloads, and high-resolution images with deterministic performance.

</div>

---

## Targets

`y4p` targets Wayland compositors that implement the `ext-data-control-v1` protocol natively:
- **Supported**: wlroots-based compositors (Sway, Hyprland, river, and derivatives).
- **Unsupported**: Compositors without `ext-data-control-v1` support (e.g. standard GNOME/Mutter, KWin).

---

## Features

- **Unified Daemon**: Single-process architecture managing clipboard monitoring, IPC serving, and disk persistence without external daemon orchestration.
- **Hybrid Storage**: Fast, searchable SQLite engine for text metadata paired with content-addressed filesystem caching for binary blobs.
- **Script-First CLI**: Deterministic command interface with immutable entry IDs and strict argument parsing for shell pipeline integration (`fzf`, `rofi`).
- **Zero-Loss Integrity**: Pinned entries are strictly immune to ring buffer rotation; zero schema corruption across migrations.
- **Local Image Pipeline**: Privacy-stripping image transcoding with isolated scratch directories, a process-group timeout watchdog, resource limits, bounded concurrency and a self-healing circuit breaker.
- **Robust by Construction**: Panic-free execution path (`unwrap`, `expect` and `panic!` are denied workspace-wide by lints) and a deliberately small, pinned dependency set.

---

## Command List

A quick look at what's available. Full flags and examples live in `y4p help`.

| Command | What it does |
| :--- | :--- |
| `daemon` | Start the background monitor and IPC listener. |
| `list` / `search` | Browse or search clipboard history. |
| `copy-to` | Restore a history entry to the live clipboard. |
| `show` | Inspect a record's content directly. |
| `store` / `paste-from` | Manually ingest stdin, or read the OS clipboard directly. |
| `pin` / `unpin` | Protect or release a record from automatic rotation. |
| `delete` / `wipe` | Remove one record, or clear all history. |
| `status` / `pause` / `resume` | Check or control the daemon's monitoring state. |

---

## Get Started

### Quick Digest

```bash
# Clone repository
git clone https://github.com/y0sh-dev/y4p.git
cd y4p

# Build and install binary
cargo build --release
sudo cp target/release/y4p /usr/local/bin/

# Start background daemon
y4p daemon &

# Verify status and store clipboard content
y4p status
echo "Wayland native clipboard" | y4p store
y4p list
```

Enable Zsh completions (optional). Add the completions directory to your `fpath` before `compinit`:

```zsh
fpath+=(/path/to/y4p/completions)
```

For every command, its flags, and detailed examples, run:

```bash
y4p help
```

<details>
<summary><strong>Verification & Tests</strong></summary>

```bash
# Run unit tests across every workspace crate
cargo test --workspace

# Strict linter checks
cargo clippy --workspace --all-targets -- -D warnings

# Full automated verification suite (both of the above)
bash scripts/check.sh
```

</details>

---

## Configuration

`y4p` works out of the box with sensible defaults and zero mandatory configuration.

To customise behaviour — such as history limits or MIME filtering — place a `y4p.toml` file at `$XDG_CONFIG_HOME/y4p/y4p.toml` (defaulting to `~/.config/y4p/y4p.toml`).

A documented reference template is provided in [`y4p.toml.example`](y4p.toml.example):

```bash
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/y4p"
cp y4p.toml.example "${XDG_CONFIG_HOME:-$HOME/.config}/y4p/y4p.toml"
```

---

## Architecture at a Glance

`y4p` is a Cargo workspace. The root package is a thin composition root (`src/main.rs`); every domain lives in its own crate, and dependencies flow strictly inward towards `y4p-core`.

- **[`crates/core`](crates/core) (`y4p-core`)**: constants, XDG paths, `y4p.toml` parser, MIME helpers, the `Payload` vocabulary, `splice(2)` / pipe-sizing / FastCDC primitives, exit flag.
- **[`crates/storage`](crates/storage) (`y4p-storage`)**: SQLite store, schema migrations, deduplicated chunk storage, filesystem blob cache.
- **[`crates/image`](crates/image) (`y4p-image`)**: image sniffing, sandboxed transcoding, circuit breaker, throttle.
- **[`crates/wayland`](crates/wayland) (`y4p-wayland`)**: `ext-data-control-v1` handlers, streaming clipboard I/O, bounded ingestion queue.
- **[`crates/daemon`](crates/daemon) (`y4p-daemon`)**: event loop, IPC socket, background worker (image routing, hashing, persistence).
- **[`crates/cli`](crates/cli) (`y4p-cli`)**: strict argument parsing, command handlers, output formatting.

---

## Documentation

The technical architecture lives in [`docs/00_overview.md`](docs/00_overview.md): workspace topology, crate responsibilities, dependency direction and the ingestion stream contract. It is the hub for the rest of `docs/`, and each document it links is a self-contained theme.

Project governance, planning and references:

- [`.github/AI_POLICY.md`](.github/AI_POLICY.md): Shared policies and guidelines on Generative AI (LLMs) usage.
- [`PERFORMANCE.md`](PERFORMANCE.md): Quantitative baseline (latency, allocations, memory bounds) and targets for the zero-copy cycle; reproduce with `cargo bench -p y4p-daemon --bench baseline`.
- [`ROADMAP_v0.8.md`](ROADMAP_v0.8.md): The v0.7.x zero-copy roadmap towards v0.8.0.
- [`README_ja.md`](README_ja.md): Japanese README (日本語版).

---

## License

GPL-3.0-or-later

Copyright (c) 2026 yosana (y0sh-dev)
