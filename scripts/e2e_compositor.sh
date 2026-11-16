#!/usr/bin/env bash
# Copyright (C) 2026 yosana
# SPDX-License-Identifier: GPL-3.0-or-later
#
# scripts/e2e_compositor.sh — headless compositor end-to-end matrix.
#
# Runs a real y4p daemon against a headless Wayland compositor (cage or sway)
# in a throw-away XDG environment, copies the payload shapes real applications
# use (plain text, rich HTML, an image, a >2 MiB binary) and asserts that
# `list`, `show`, `copy-to`, `stats` and `doctor` all see what was ingested.
#
# When no headless compositor is installed the matrix cannot say anything, so it
# prints [SKIP] with the missing tools and exits 0: a host without a compositor
# is not a failing build.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

PASSED=0
FAILED=0

pass() { printf '[PASS] %s\n' "$*"; PASSED=$((PASSED + 1)); }
fail() { printf '[FAIL] %s\n' "$*"; FAILED=$((FAILED + 1)); }

# --- 1. Preconditions -------------------------------------------------------
# A compositor is mandatory; the clipboard tools and magick are what the cases
# below use to put bytes on the clipboard the way the applications do.
MISSING=()
if ! command -v cage >/dev/null 2>&1 && ! command -v sway >/dev/null 2>&1; then
    MISSING+=("cage or sway (headless Wayland compositor)")
fi
command -v wl-copy >/dev/null 2>&1 || MISSING+=("wl-copy (wl-clipboard)")
command -v wl-paste >/dev/null 2>&1 || MISSING+=("wl-paste (wl-clipboard)")
command -v magick >/dev/null 2>&1 || MISSING+=("magick (ImageMagick 7)")

if (( ${#MISSING[@]} > 0 )); then
    for item in "${MISSING[@]}"; do
        printf '[SKIP] %s is not installed\n' "$item"
    done
    printf '[SKIP] the live compositor matrix needs these tools; nothing was exercised\n'
    exit 0
fi

# --- 2. Binary --------------------------------------------------------------
Y4P="${Y4P_BIN:-$ROOT_DIR/target/debug/y4p}"
if [[ ! -x "$Y4P" ]]; then
    printf '[ .. ] building %s\n' "$Y4P"
    ( cd "$ROOT_DIR" && cargo build --quiet )
fi
if [[ ! -x "$Y4P" ]]; then
    fail "y4p binary not found at $Y4P"
    exit 1
fi

# --- 3. Isolated session ----------------------------------------------------
WORK="$(mktemp -d "${TMPDIR:-/tmp}/y4p-e2e.XXXXXX")"
chmod 700 "$WORK"
export XDG_RUNTIME_DIR="$WORK/run"
export XDG_DATA_HOME="$WORK/data"
export XDG_CACHE_HOME="$WORK/cache"
export XDG_CONFIG_HOME="$WORK/config"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$WORK/log"
chmod 700 "$XDG_RUNTIME_DIR"

DAEMON_PID=""
COMPOSITOR_PID=""
cleanup() {
    local status=$?
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    if [[ -n "$COMPOSITOR_PID" ]] && kill -0 "$COMPOSITOR_PID" 2>/dev/null; then
        kill "$COMPOSITOR_PID" 2>/dev/null || true
        wait "$COMPOSITOR_PID" 2>/dev/null || true
    fi
    rm -rf "$WORK"
    exit "$status"
}
trap cleanup EXIT

# --- 4. Headless compositor -------------------------------------------------
# Both compositors honour wlroots' headless backend; cage needs a client to stay
# alive, sway a minimal config. Either way the Wayland socket lands in
# $XDG_RUNTIME_DIR, which is where clients look for it.
if command -v cage >/dev/null 2>&1; then
    COMPOSITOR=cage
    WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
        cage -- sleep 3600 >"$WORK/log/compositor.log" 2>&1 &
    COMPOSITOR_PID=$!
else
    COMPOSITOR=sway
    mkdir -p "$XDG_CONFIG_HOME/sway"
    printf 'output * bg #000000 solid_color\n' > "$XDG_CONFIG_HOME/sway/config"
    WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
        sway --unsupported-gpu -c "$XDG_CONFIG_HOME/sway/config" >"$WORK/log/compositor.log" 2>&1 &
    COMPOSITOR_PID=$!
fi

WAYLAND_DISPLAY=""
for _ in $(seq 1 100); do
    for socket in "$XDG_RUNTIME_DIR"/wayland-*; do
        if [[ -S "$socket" ]]; then
            WAYLAND_DISPLAY="$(basename "$socket")"
            break
        fi
    done
    [[ -n "$WAYLAND_DISPLAY" ]] && break
    sleep 0.1
done
if [[ -z "$WAYLAND_DISPLAY" ]]; then
    fail "$COMPOSITOR did not create a Wayland socket under $XDG_RUNTIME_DIR"
    tail -20 "$WORK/log/compositor.log" || true
    exit 1
fi
export WAYLAND_DISPLAY
pass "compositor: $COMPOSITOR is serving $WAYLAND_DISPLAY"

# --- 5. Daemon --------------------------------------------------------------
"$Y4P" daemon --verbose >"$WORK/log/daemon.log" 2>&1 &
DAEMON_PID=$!

daemon_ready=false
for _ in $(seq 1 150); do
    if "$Y4P" status >/dev/null 2>&1; then daemon_ready=true; break; fi
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then break; fi
    sleep 0.1
done
if [[ "$daemon_ready" != true ]]; then
    fail "y4p daemon did not answer IPC status"
    tail -20 "$WORK/log/daemon.log" || true
    exit 1
fi
pass "daemon: answering IPC status"

# Waits for an ingested entry whose raw row contains `marker`.
wait_for_row() {
    local marker=$1 deadline=$((SECONDS + 20))
    while (( SECONDS < deadline )); do
        if "$Y4P" list --raw 0-200 2>/dev/null | grep -qF -- "$marker"; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# --- 6. Case 1: plain text (Chromium / Firefox style) -----------------------
PLAIN_MARKER="e2e-plain-text-$$"
printf '%s' "$PLAIN_MARKER" | wl-copy
if wait_for_row "$PLAIN_MARKER"; then
    round_trip="$("$Y4P" show 0 --raw)"
    if [[ "$round_trip" == "$PLAIN_MARKER" ]]; then
        pass "case 1: plain text ingested and shown byte-for-byte"
    else
        fail "case 1: plain text round-trip differs ('$round_trip')"
    fi
else
    fail "case 1: plain text was not ingested"
fi

# --- 7. Case 2: rich HTML (multi-MIME, no text/plain offered) ---------------
HTML_MARKER="<b>e2e-rich-html-$$</b>"
printf '%s' "$HTML_MARKER" | wl-copy --type text/html
if wait_for_row "e2e-rich-html-$$"; then
    html_id="$("$Y4P" list --raw --id 0-200 2>/dev/null | grep -F -- "e2e-rich-html-$$" | head -n 1 | sed -E 's/^\[ *([0-9]+)\].*/\1/' || true)"
    if [[ -z "$html_id" ]]; then
        fail "case 2: the rich HTML row carries no stable ID"
    else
        html_body="$("$Y4P" show "$html_id" --id --raw)"
        if [[ "$html_body" == "$HTML_MARKER" ]]; then
            pass "case 2: rich HTML ingested as markup and shown unchanged"
        else
            fail "case 2: rich HTML round-trip differs ('$html_body')"
        fi
    fi
else
    fail "case 2: rich HTML was not ingested"
fi

# --- 8. Case 3: image (GIMP / screenshot style) -----------------------------
magick -size 64x64 xc:red "$WORK/red.png"
wl-copy --type image/png < "$WORK/red.png"
if wait_for_row '[IMG]'; then
    pass "case 3: PNG ingested and listed as an image"
else
    fail "case 3: PNG was not ingested"
fi

# --- 9. Case 4: >2 MiB binary, spooled, round-tripped through copy-to -------
# A GIF survives ingestion untouched (no re-encode), so the bytes that come back
# out of the daemon's own data source can be compared with the ones that went in.
BIG_GIF="$WORK/big.gif"
magick -size 1024x1024 plasma:fractal -depth 8 "$BIG_GIF"
if (( $(stat -c %s "$BIG_GIF") < 2 * 1024 * 1024 )); then
    magick -size 2048x2048 plasma:fractal -depth 8 "$BIG_GIF"
fi
big_size=$(stat -c %s "$BIG_GIF")
if (( big_size < 2 * 1024 * 1024 )); then
    fail "case 4: could not generate a >2 MiB payload (got $big_size bytes)"
else
    wl-copy --type image/gif < "$BIG_GIF"
    if wait_for_row '[gif]'; then
        if "$Y4P" copy-to 0 >/dev/null 2>&1; then
            if wl-paste --type image/gif > "$WORK/roundtrip.gif"; then
                if cmp -s "$BIG_GIF" "$WORK/roundtrip.gif"; then
                    pass "case 4: $big_size-byte GIF ingested through the spooler and copied back byte-for-byte"
                else
                    fail "case 4: copied-back GIF differs from the original"
                fi
            else
                fail "case 4: wl-paste could not read the restored GIF"
            fi
        else
            fail "case 4: copy-to refused to restore the large payload"
        fi
    else
        fail "case 4: the >2 MiB GIF was not ingested"
    fi
fi

# --- 10. Observability ------------------------------------------------------
if "$Y4P" stats > "$WORK/log/stats.txt" 2>&1; then
    if grep -q -- '--- Entries ---' "$WORK/log/stats.txt" \
        && grep -q -- '--- On-disk ---' "$WORK/log/stats.txt" \
        && grep -q '^ingress: ' "$WORK/log/stats.txt"; then
        pass "stats: entries, on-disk footprint and daemon counters all reported"
    else
        fail "stats: report is missing a section"
        tail -20 "$WORK/log/stats.txt" || true
    fi
else
    fail "stats: exited non-zero"
fi

if "$Y4P" doctor > "$WORK/log/doctor.txt" 2>&1; then
    pass "doctor: every check passed in the live session"
else
    fail "doctor: at least one check failed"
    cat "$WORK/log/doctor.txt" || true
fi

# `list` must reflect the ingested rows, oldest text first.
if "$Y4P" list --raw 0-200 | grep -qF -- "$PLAIN_MARKER"; then
    pass "list: the ingested text is still in the history"
else
    fail "list: the ingested text vanished from the history"
fi

# --- 11. Summary ------------------------------------------------------------
printf '\n%d passed, %d failed\n' "$PASSED" "$FAILED"
if (( FAILED > 0 )); then
    # The daemon log is the only place a swallowed ingestion error would show.
    printf '\n--- daemon log (last 40 lines) ---\n'
    tail -n 40 "$WORK/log/daemon.log" || true
    exit 1
fi
exit 0
