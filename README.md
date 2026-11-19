
<div align="center">

# y4p

**Unified Wayland Clipboard Infrastructure.**

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://rust-lang.org)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Wayland-lightgreen.svg)](https://wayland.freedesktop.org)
[![Version](https://img.shields.io/badge/version-0.10.0-green.svg)](https://github.com/y0sh-dev/y4p/releases/latest)

`y4p` is a standalone clipboard manager, built natively for Wayland.

It runs as a single daemon — no separate monitor process, no separate provider process — that watches the clipboard, serves history back out, and persists everything to disk.
Engineered with strict zero-loss integrity and defence-in-depth resource controls, it handles text, binaries and high-resolution images under fixed memory and disk bounds.

</div>

---

## Targets

`y4p` targets Wayland compositors that implement the `ext-data-control-v1` protocol natively:
- **Supported**: wlroots-based compositors (Sway, Hyprland, river, and derivatives).
- **Unsupported**: Compositors without `ext-data-control-v1` support (e.g. standard GNOME/Mutter, KWin).

---

## Features

- **Unified Daemon**: Single-process architecture managing clipboard monitoring, IPC serving, and disk persistence without external daemon orchestration.
- **Hybrid Storage**: Fast, searchable SQLite metadata with whole-payload SHA3 deduplication; large binaries live in a content-addressed cache directory bounded by a 512 MiB LRU quota.
- **Pristine by Default**: Copied images are stored byte for byte. Metadata-stripping image sanitisation is an explicit opt-in (`[image] sanitize = true`) and requires ImageMagick only then.
- **Script-First CLI**: Deterministic command interface with immutable entry IDs and strict argument parsing for shell pipeline integration (`fzf`, `rofi`).
- **Zero-Loss Integrity**: Pinned entries are strictly immune to history rotation and cache eviction; schema migrations are additive and idempotent.
- **Robust by Construction**: Panic-free resident layers (`unwrap`, `expect` and `panic!` are denied by lints) and a deliberately small, pinned dependency set.
- **Built-in Observability**: `y4p stats` reports entry, on-disk and daemon shed figures; `y4p doctor` walks environment, compositor, database, socket and quota with `[PASS]` / `[WARN]` / `[FAIL]` per check.

---

## Command List

A quick look at what's available. Full flags and examples live in `y4p help`.

| Command | What it does |
| :--- | :--- |
| `daemon` | Start the background monitor and IPC listener. |
| `list` / `search` | Browse or search clipboard history. |
| `copy-to` | Restore a history entry to the live clipboard. |
| `show` | Inspect a record's content directly. |
| `store` | Ingest standard input manually. |
| `paste-from [-l\|--list-types \| mime]` | Read the OS clipboard directly; `-l` prints the advertised MIME types only. |
| `pin` / `unpin` | Protect or release a record from automatic rotation. |
| `delete` / `wipe` | Remove one record, or clear all history. |
| `status` / `pause` / `resume` | Check or control the daemon's monitoring state. |
| `stats` | Report entry counts, on-disk footprint and the daemon's shed counters. |
| `doctor` | Diagnose environment, compositor, database, socket and quota; exits non-zero only on a failed check. |

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

# What is stored, and is the setup healthy?
y4p stats
y4p doctor
```

### Run as a user service

A `systemd` user unit ships in [`contrib/systemd/y4p.service`](contrib/systemd/y4p.service). It uses `Restart=on-failure`, which only works because `y4p daemon` reports a failed start or a lost compositor connection as a non-zero exit and a requested stop as `0`:

```bash
mkdir -p ~/.config/systemd/user
cp contrib/systemd/y4p.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now y4p.service
```

`y4p status` and `y4p doctor` then talk to that instance, and `journalctl --user -u y4p` carries its log.

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

# Live compositor matrix (cage or sway + wl-clipboard + ImageMagick);
# prints [SKIP] and exits 0 when no headless compositor is installed
bash scripts/e2e_compositor.sh
```

</details>

---

## Configuration

`y4p` works out of the box with sensible defaults and zero mandatory configuration.

To customise behaviour — history limits, MIME filtering, or opt-in image sanitisation — place a `y4p.toml` file at `$XDG_CONFIG_HOME/y4p/y4p.toml` (defaulting to `~/.config/y4p/y4p.toml`).

A documented reference template is provided in [`y4p.toml.example`](y4p.toml.example):

```bash
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/y4p"
cp y4p.toml.example "${XDG_CONFIG_HOME:-$HOME/.config}/y4p/y4p.toml"
```

---

## Architecture at a Glance

`y4p` is a Cargo workspace. The root package is a thin composition root (`src/main.rs`); every domain lives in its own crate, and dependencies flow strictly inward towards `y4p-core`.

- **[`crates/core`](crates/core) (`y4p-core`)**: constants, XDG paths, `y4p.toml` parser, MIME helpers, the `Payload` vocabulary, pipe sizing and kernel-copy primitives, exit flag.
- **[`crates/storage`](crates/storage) (`y4p-storage`)**: SQLite v4 facade, schema migrations, inline payload storage, legacy chunk-row compatibility, filesystem cache with the LRU quota.
- **[`crates/image`](crates/image) (`y4p-image`)**: image sniffing, opt-in sanitisation, sandboxed transcoding, circuit breaker, throttle.
- **[`crates/wayland`](crates/wayland) (`y4p-wayland`)**: `ext-data-control-v1` handlers, bounded clipboard reads, ingestion queue, egress.
- **[`crates/daemon`](crates/daemon) (`y4p-daemon`)**: event loop, IPC socket, background worker (optional image routing, hashing, persistence).
- **[`crates/cli`](crates/cli) (`y4p-cli`)**: strict argument parsing, command handlers, output formatting.

---

## Documentation

The technical architecture lives in [`docs/00_overview.md`](docs/00_overview.md): workspace topology, crate responsibilities, dependency direction and the ingestion stream contract. It is the hub for the rest of `docs/`, and each document it links is a self-contained theme.

Project governance, planning and references:

- [`.github/POLICY.md`](.github/POLICY.md): Shared contribution policy and engineering standards.
- [`docs/history/PERFORMANCE.md`](docs/history/PERFORMANCE.md): Quantitative baseline (latency, allocations, memory bounds) and historical targets; reproduce with `cargo bench -p y4p-daemon --bench baseline`.
- [`ROADMAP_v0.10.md`](ROADMAP_v0.10.md): The v0.9.x subtraction milestones towards v0.10.0.
- [`README_ja.md`](README_ja.md): Japanese README (日本語版).

---

## License

GPL-3.0-or-later

Copyright (c) 2026 yosana (y0sh-dev)
