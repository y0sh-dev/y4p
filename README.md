
<div align="center">

# y4p

**Unified Wayland Clipboard Infrastructure.**

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://rust-lang.org)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Wayland-lightgerm.svg)
[![Version](https://img.shields.io/badge/version-0.2.0-green.svg)](https://github.com/y0sh-dev/y4p/releases/latest)

`y4p` is a standalone clipboard manager, built natively for Wayland.

It runs as a single daemon — no separate monitor process, no separate provider process — that watches the clipboard, serves history back out, and persists everything to disk.

</div>

---

## Compatibility

`y4p` targets **wlroots-based compositors** — Sway, Hyprland, and others built on wlroots.

It depends directly on the `ext-data-control-v1` protocol, rather than any X11 compatibility layer. That's a deliberate choice, not an oversight.

It also means `y4p` will not work on compositors that don't implement this protocol — KWin being the notable example at the time of writing. If you're unsure whether your compositor supports it, check its Wayland protocol list before installing.

---

## Key Features

**Unified Standalone Architecture.**
One binary. One daemon. Monitoring, serving, and persistence all live in the same process, so there's nothing to keep in sync and nothing to leave behind as a zombie.

**High-Capacity Hybrid Storage.**
Text and metadata live in a fast, searchable SQLite database. Large binaries — screenshots, GIFs — live in a deduplicated filesystem cache instead. Each stays out of the other's way, even at 70MB+ per item.

**Script-First Stable IDs & Strict CLI.**
Every record gets an immutable ID, separate from its display position. Combined with a parser that rejects unrecognized flags outright, `y4p` is built to behave predictably inside `fzf`, `rofi`, and shell pipelines — not just at an interactive prompt.

**Zero-Loss Integrity & Pin Protection.**
History rotation never touches a record you've pinned. It's excluded permanently, no matter how much you copy afterward — and schema migrations are staged so existing history is never rewritten or lost across upgrades.

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

## Quick Start

Build it:

```bash
cargo build --release
sudo cp target/release/y4p /usr/local/bin/
```

Start the daemon:

```bash
y4p daemon
```

Enable Zsh completions (optional). Add the completions directory to your `fpath` before `compinit`:

```zsh
fpath+=(/path/to/y4p/completions)
```

That's it. For every command, its flags, and its examples, run:

```bash
y4p help
```

---

## Architecture & Design

This README is a quick-start guide, kept intentionally short.

The reasoning behind `y4p`'s internals — why the daemon multiplexes I/O on one thread, how Pin protection interacts with history rotation, why stable IDs exist — lives separately, in [`docs/00_overview.md`](docs/00_overview.md).

---

## License

GPL-3.0-or-later

Copyright (c) 2026 yosana (y0sh-dev)

---

## AI Usage Disclosure

For our policy on using Generative AI (LLMs), please refer to
the shared guidelines documented in [docs/AI_POLICY.md](docs/AI_POLICY.md).
