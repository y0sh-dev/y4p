#!/usr/bin/env bash
# Copyright (C) 2026 yosana
# SPDX-License-Identifier: GPL-3.0-or-later
#
# scripts/e2e_compositor.sh — live compositor end-to-end matrix.
#
# Runs a real y4p daemon against a throw-away XDG environment and a Wayland
# compositor (cage, sway or Hyprland), then exercises the payload shapes real
# applications produce: plain and rich text, URI lists, blank selections, every
# raster format y4p understands (PNG, JPEG, GIF, WebP and APNG), the 32 MiB
# payload ceiling, burst ingestion, pin survival across a daemon restart and
# the `paste-from` type-discovery flags.
#
# The daemon, the database, the payload cache and the IPC socket all live
# under one temporary directory that `cleanup` removes.
#
# E2E_USE_LIVE_WAYLAND=1 reuses the caller's own Wayland session instead of
# starting a compositor. It is the only mode in which clipboard traffic reaches
# the developer's real seat, so it is opt-in and never implied: without it a
# headless compositor (or `[SKIP]`) is used.
#
# When no compositor is installed the matrix cannot say anything, so it prints
# [SKIP] with the missing tools and exits 0: a host without a compositor is not
# a failing build.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

PASSED=0
FAILED=0

pass() { printf '[PASS] %s\n' "$*"; PASSED=$((PASSED + 1)); }
fail() { printf '[FAIL] %s\n' "$*"; FAILED=$((FAILED + 1)); }

# Captured before the isolated session replaces XDG_RUNTIME_DIR: the live mode
# below still has to reach the caller's compositor socket.
INHERITED_RUNTIME_DIR="${XDG_RUNTIME_DIR:-}"
INHERITED_WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-}"
USE_LIVE_WAYLAND="${E2E_USE_LIVE_WAYLAND:-0}"

# --- 1. Preconditions -------------------------------------------------------
# A compositor is mandatory; wl-clipboard is what the cases use to publish
# payloads the way applications do, and magick both generates the raster
# fixtures and backs the opt-in image sanitisation.
MISSING=()
if [[ "$USE_LIVE_WAYLAND" == 1 ]]; then
    if [[ -z "$INHERITED_RUNTIME_DIR" || -z "$INHERITED_WAYLAND_DISPLAY" \
        || ! -S "$INHERITED_RUNTIME_DIR/$INHERITED_WAYLAND_DISPLAY" ]]; then
        fail "E2E_USE_LIVE_WAYLAND=1 but WAYLAND_DISPLAY is unset or unreachable"
        exit 1
    fi
    COMPOSITOR="live session ($INHERITED_WAYLAND_DISPLAY)"
elif command -v cage >/dev/null 2>&1; then
    COMPOSITOR=cage
elif command -v sway >/dev/null 2>&1; then
    COMPOSITOR=sway
elif command -v Hyprland >/dev/null 2>&1; then
    COMPOSITOR=Hyprland
else
    MISSING+=("cage, sway or Hyprland (Wayland compositor)")
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
WL_COPY_OWNED=0
cleanup() {
    local status=$?
    # Releasing the selection is what makes the forked wl-copy servers exit.
    if (( WL_COPY_OWNED == 1 )); then
        timeout 5 wl-copy --clear >/dev/null 2>&1 || true
    fi
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

# --- 4. Compositor ----------------------------------------------------------
# cage and sway both honour wlroots' headless backend; cage needs a client to
# stay alive, sway a minimal config. Hyprland runs on aquamarine, which in the
# absence of a DRM master relies on its own headless backend: the headless
# variables below keep it away from the caller's session, and the crash
# reporter is silenced so a failure only reaches our own log.
if [[ "$USE_LIVE_WAYLAND" == 1 ]]; then
    # The daemon and the IPC socket stay isolated in $WORK, but the compositor
    # socket is the caller's, so it is linked in by name for every client this
    # script starts (y4p, wl-copy and wl-paste included).
    ln -s "$INHERITED_RUNTIME_DIR/$INHERITED_WAYLAND_DISPLAY" "$XDG_RUNTIME_DIR/$INHERITED_WAYLAND_DISPLAY"
    export WAYLAND_DISPLAY="$INHERITED_WAYLAND_DISPLAY"
    pass "compositor: reusing the live session at $WAYLAND_DISPLAY"
elif [[ "$COMPOSITOR" == cage ]]; then
    WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
        cage -- sleep 3600 >"$WORK/log/compositor.log" 2>&1 &
    COMPOSITOR_PID=$!
elif [[ "$COMPOSITOR" == sway ]]; then
    mkdir -p "$XDG_CONFIG_HOME/sway"
    printf 'output * bg #000000 solid_color\n' > "$XDG_CONFIG_HOME/sway/config"
    WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
        sway --unsupported-gpu -c "$XDG_CONFIG_HOME/sway/config" >"$WORK/log/compositor.log" 2>&1 &
    COMPOSITOR_PID=$!
else
    mkdir -p "$XDG_CONFIG_HOME/hypr"
    printf 'monitor = , 1280x720@60, 0x0, 1\n' > "$XDG_CONFIG_HOME/hypr/hyprland.conf"
    env -u WAYLAND_DISPLAY -u DISPLAY \
        HYPRLAND_NO_CRASHREPORTER=1 AQ_BACKENDS=headless AQ_HEADLESS_OUTPUTS=1 XDG_BACKEND=headless \
        Hyprland -c "$XDG_CONFIG_HOME/hypr/hyprland.conf" >"$WORK/log/compositor.log" 2>&1 &
    COMPOSITOR_PID=$!
fi

if [[ -n "$COMPOSITOR_PID" ]]; then
    # Bash would otherwise print the compositor's own signal death in the
    # middle of the matrix output; the log tail below carries that detail.
    disown "$COMPOSITOR_PID" 2>/dev/null || true
fi

if [[ "$USE_LIVE_WAYLAND" != 1 ]]; then
    # Hyprland nests its socket under hypr/<instance>/, so the search is
    # recursive; the value handed to clients is the path relative to
    # $XDG_RUNTIME_DIR, which is exactly what libwayland joins back together.
    compositor_socket=""
    for _ in $(seq 1 150); do
        compositor_socket="$(find "$XDG_RUNTIME_DIR" -maxdepth 4 -type s -name '*wayland-*' -print -quit 2>/dev/null || true)"
        [[ -n "$compositor_socket" ]] && break
        if [[ -n "$COMPOSITOR_PID" ]] && ! kill -0 "$COMPOSITOR_PID" 2>/dev/null; then break; fi
        sleep 0.1
    done
    if [[ -z "$compositor_socket" ]]; then
        fail "$COMPOSITOR did not create a Wayland socket under $XDG_RUNTIME_DIR"
        if [[ "$COMPOSITOR" == Hyprland ]]; then
            printf '[ .. ] aquamarine needs a DRM master or an outer compositor to start; retry with E2E_USE_LIVE_WAYLAND=1 to reuse the current session\n'
        fi
        tail -20 "$WORK/log/compositor.log" || true
        exit 1
    fi
    WAYLAND_DISPLAY="${compositor_socket#"$XDG_RUNTIME_DIR"/}"
    export WAYLAND_DISPLAY
    pass "compositor: $COMPOSITOR is serving $WAYLAND_DISPLAY"
fi

# --- 5. Daemon lifecycle ----------------------------------------------------
# Factored out because the persistence case has to stop and restart the very
# same daemon, against the same database and cache.
start_daemon() {
    "$Y4P" daemon --verbose >>"$WORK/log/daemon.log" 2>&1 &
    DAEMON_PID=$!
    local _
    for _ in $(seq 1 150); do
        if "$Y4P" status >/dev/null 2>&1; then return 0; fi
        if ! kill -0 "$DAEMON_PID" 2>/dev/null; then return 1; fi
        sleep 0.1
    done
    return 1
}

stop_daemon() {
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    DAEMON_PID=""
}

if ! start_daemon; then
    fail "y4p daemon did not answer IPC status"
    tail -20 "$WORK/log/daemon.log" || true
    exit 1
fi
pass "daemon: answering IPC status"

# --- 6. Helpers -------------------------------------------------------------
# `list` output is always captured into a variable first: piping it straight
# into `grep -q` would close the pipe early and make y4p panic on the write end.
# The raw view carries the stable ID, the pin marker, the label and a preview.
history_rows() { "$Y4P" list --raw --id 0-500 2>/dev/null || true; }
entry_count() { history_rows | wc -l; }
newest_row() { history_rows | sed -n '1p'; }
newest_id() { history_rows | sed -n '1s/^\[ *\([0-9][0-9]*\)\].*/\1/p'; }

# Prints the history row of `id` only when it carries the pin marker.
pinned_row() {
    local id=$1 row
    while IFS= read -r row; do
        if [[ "$row" =~ ^\[\ *$id\]\ \*\  ]]; then
            printf '%s\n' "$row"
            return 0
        fi
    done < <(history_rows)
    return 1
}

# Waits until the history holds at least `target` rows: ingestion is
# asynchronous, so a case must not read a payload before its row exists.
wait_for_entries_at_least() {
    local target=$1 deadline=$((SECONDS + 30))
    while (( SECONDS < deadline )); do
        if (( $(entry_count) >= target )); then return 0; fi
        sleep 0.2
    done
    return 1
}

# Waits for an ingested entry whose raw row contains `marker`.
wait_for_row() {
    local marker=$1 deadline=$((SECONDS + 30)) rows
    while (( SECONDS < deadline )); do
        rows=$(history_rows)
        if [[ "$rows" == *"$marker"* ]]; then return 0; fi
        sleep 0.2
    done
    return 1
}

copy_text() {
    local payload=$1 mime=${2:-text/plain}
    printf '%s' "$payload" | timeout 20 wl-copy --type "$mime"
}

copy_file() {
    local mime=$1 file=$2
    timeout 60 wl-copy --type "$mime" < "$file"
}

show_payload() {
    local id=$1 destination=$2
    "$Y4P" show "$id" --id --raw > "$destination"
}

# Creates a raster fixture whose pixels differ from run to run, so a case can
# never be satisfied by a row an earlier run left in the same history.
generated() {
    local destination=$1
    shift
    magick -seed "$((RANDOM * 1000 + $$))" "$@" "$destination"
}

SETTLE_SECONDS=2

# --- 7. Case 1: plain text (Chromium / Firefox style) -----------------------
PLAIN_MARKER="e2e-plain-text-$$"
if copy_text "$PLAIN_MARKER" && wait_for_row "$PLAIN_MARKER"; then
    round_trip="$("$Y4P" show 0 --raw 2>/dev/null || true)"
    if [[ "$round_trip" == "$PLAIN_MARKER" ]]; then
        pass "case 1: plain text ingested and shown byte-for-byte"
    else
        fail "case 1: plain text round-trip differs ('$round_trip')"
    fi
else
    fail "case 1: plain text was not ingested"
fi

# --- 8. Case 2: rich HTML (multi-MIME, no text/plain offered) ---------------
HTML_MARKER="<b>e2e-rich-html-$$</b>"
if copy_text "$HTML_MARKER" text/html && wait_for_row "e2e-rich-html-$$"; then
    html_id="$(newest_id)"
    if [[ -z "$html_id" ]]; then
        fail "case 2: the rich HTML row carries no stable ID"
    else
        html_body="$("$Y4P" show "$html_id" --id --raw 2>/dev/null || true)"
        if [[ "$html_body" == "$HTML_MARKER" ]]; then
            pass "case 2: rich HTML ingested as markup and shown unchanged"
        else
            fail "case 2: rich HTML round-trip differs ('$html_body')"
        fi
    fi
else
    fail "case 2: rich HTML was not ingested"
fi

# --- 9. Case 3: URI list (file manager / drag and drop) ---------------------
# `file://` URIs are percent-decoded and rewritten as one plain path per line,
# so the row must hold paths, not the wire form.
URI_MARKER="e2e-uri-$$"
URI_LIST="file:///tmp/${URI_MARKER}-a.txt
file:///tmp/${URI_MARKER}-b.txt"
URI_EXPECTED="/tmp/${URI_MARKER}-a.txt
/tmp/${URI_MARKER}-b.txt"
if copy_text "$URI_LIST" text/uri-list && wait_for_row "$URI_MARKER"; then
    uri_row="$(newest_row)"
    uri_id="$(newest_id)"
    if [[ "$uri_row" != *"[FIL]"* ]]; then
        fail "case 3: the URI list was not labelled as a file list ('$uri_row')"
    elif show_payload "$uri_id" "$WORK/uri.out" \
        && printf '%s' "$URI_EXPECTED" > "$WORK/uri.expected" \
        && cmp -s "$WORK/uri.expected" "$WORK/uri.out"; then
        pass "case 3: text/uri-list was normalised to one path per line"
    else
        fail "case 3: the stored URI list differs from the normalised paths"
    fi
else
    fail "case 3: the URI list was not ingested"
fi

# --- 10. Case 4: whitespace-only selection (daemon only) --------------------
# A blank line copied from an editor must not become history, while
# `paste-from` — an explicit CLI request — still returns it verbatim.
before_blank=$(entry_count)
if printf '   \n\t  ' | timeout 20 wl-copy --type text/plain; then
    WL_COPY_OWNED=1
    sleep "$SETTLE_SECONDS"
    after_blank=$(entry_count)
    if (( after_blank == before_blank )); then
        pass "case 4: a whitespace-only selection was not ingested"
    else
        fail "case 4: a whitespace-only selection reached the history ($before_blank -> $after_blank)"
    fi
    printf '   \n\t  ' > "$WORK/blank.expected"
    if timeout 20 "$Y4P" paste-from text/plain > "$WORK/blank.out" 2>/dev/null \
        && cmp -s "$WORK/blank.expected" "$WORK/blank.out"; then
        pass "case 4: paste-from still returns the whitespace-only selection"
    else
        fail "case 4: paste-from did not return the whitespace-only selection"
    fi
else
    fail "case 4: wl-copy could not publish the whitespace-only selection"
fi

# --- 11. Case 5: PNG stored raw (sanitisation off by default) --------------
generated "$WORK/case5.png" -size 160x160 plasma:fractal
before_png=$(entry_count)
if copy_file image/png "$WORK/case5.png" && wait_for_entries_at_least $((before_png + 1)); then
    WL_COPY_OWNED=1
    png_id="$(newest_id)"
    if show_payload "$png_id" "$WORK/case5.out" && cmp -s "$WORK/case5.png" "$WORK/case5.out"; then
        pass "case 5: PNG stored and read back byte-for-byte with sanitisation off"
    else
        fail "case 5: the stored PNG differs from the original"
    fi
else
    fail "case 5: PNG was not ingested"
fi

# --- 12. Case 6: JPEG stored raw -------------------------------------------
generated "$WORK/case6.jpg" -size 160x160 plasma:fractal
before_jpeg=$(entry_count)
if copy_file image/jpeg "$WORK/case6.jpg" && wait_for_entries_at_least $((before_jpeg + 1)); then
    jpeg_id="$(newest_id)"
    jpeg_row="$(newest_row)"
    if [[ "$jpeg_row" != *"[jpeg]"* ]]; then
        fail "case 6: the JPEG was not stored under its own MIME ('$jpeg_row')"
    elif show_payload "$jpeg_id" "$WORK/case6.out" && cmp -s "$WORK/case6.jpg" "$WORK/case6.out"; then
        pass "case 6: JPEG stored raw and read back byte-for-byte"
    else
        fail "case 6: the stored JPEG differs from the original"
    fi
else
    fail "case 6: JPEG was not ingested"
fi

# --- 13. Case 7: animated GIF (every frame kept) ---------------------------
generated "$WORK/case7-a.png" -size 80x80 plasma:fractal
generated "$WORK/case7-b.png" -size 80x80 plasma:fractal
magick -delay 10 -loop 0 "$WORK/case7-a.png" "$WORK/case7-b.png" "$WORK/case7.gif"
before_gif=$(entry_count)
if copy_file image/gif "$WORK/case7.gif" && wait_for_entries_at_least $((before_gif + 1)); then
    gif_id="$(newest_id)"
    if show_payload "$gif_id" "$WORK/case7.out" && cmp -s "$WORK/case7.gif" "$WORK/case7.out"; then
        pass "case 7: animated GIF kept its frames and every byte"
    else
        fail "case 7: the stored GIF differs from the original"
    fi
else
    fail "case 7: GIF was not ingested"
fi

# --- 14. Case 8: WebP stored raw -------------------------------------------
generated "$WORK/case8.webp" -size 120x120 plasma:fractal
before_webp=$(entry_count)
if copy_file image/webp "$WORK/case8.webp" && wait_for_entries_at_least $((before_webp + 1)); then
    webp_id="$(newest_id)"
    if show_payload "$webp_id" "$WORK/case8.out" && cmp -s "$WORK/case8.webp" "$WORK/case8.out"; then
        pass "case 8: WebP stored raw and read back byte-for-byte"
    else
        fail "case 8: the stored WebP differs from the original"
    fi
else
    fail "case 8: WebP was not ingested"
fi

# --- 15. Case 9: APNG (the acTL chunk must survive) ------------------------
# ImageMagick writes a real APNG through the `apng:` coder: PNG signature,
# IHDR, then acTL before the first IDAT, which is what y4p detects. An
# animation must never be re-encoded to its first frame.
generated "$WORK/case9-a.png" -size 64x64 plasma:fractal
generated "$WORK/case9-b.png" -size 64x64 plasma:fractal
magick -delay 20 -loop 0 "$WORK/case9-a.png" "$WORK/case9-b.png" "apng:$WORK/case9.png"
if ! LC_ALL=C grep -q 'acTL' "$WORK/case9.png"; then
    fail "case 9: the generated fixture carries no acTL chunk"
else
    before_apng=$(entry_count)
    if copy_file image/png "$WORK/case9.png" && wait_for_entries_at_least $((before_apng + 1)); then
        apng_id="$(newest_id)"
        if show_payload "$apng_id" "$WORK/case9.out" && cmp -s "$WORK/case9.png" "$WORK/case9.out"; then
            pass "case 9: APNG kept its acTL chunk and every byte"
        else
            fail "case 9: the stored APNG differs from the original"
        fi
    else
        fail "case 9: APNG was not ingested"
    fi
fi

# --- 16. Case 10: pin and daemon restart persistence -----------------------
# A PNG payload is cache-backed by MIME rather than by size: it lives in its
# own <hash>.cache file (direct ingestion, no spool) and is streamed back out
# with sendfile(2). It is pinned, then the daemon is stopped and started
# again: the row, its pin marker and the cache file must all still be there,
# and copy-to must restore the exact bytes.
generated "$WORK/case10.png" -size 320x320 plasma:fractal
before_pin=$(entry_count)
if copy_file image/png "$WORK/case10.png" && wait_for_entries_at_least $((before_pin + 1)); then
    pin_id="$(newest_id)"
    pin_output="$("$Y4P" pin "$pin_id" --id 2>&1 || true)"
    if [[ "$pin_output" != *pinned* ]] || [[ -z "$(pinned_row "$pin_id")" ]]; then
        fail "case 10: entry $pin_id was not pinned ('$pin_output')"
    else
        stop_daemon
        if ! start_daemon; then
            fail "case 10: the daemon did not come back after a restart"
        else
            # The restarted daemon re-ingests the current selection, so give
            # that event time to settle before judging the history.
            sleep "$SETTLE_SECONDS"
            if [[ -z "$(pinned_row "$pin_id")" ]]; then
                fail "case 10: pinned entry $pin_id lost its pin marker across the restart"
            elif ! show_payload "$pin_id" "$WORK/case10.out" || ! cmp -s "$WORK/case10.png" "$WORK/case10.out"; then
                fail "case 10: the cached payload did not survive the restart"
            elif "$Y4P" copy-to "$pin_id" --id >"$WORK/log/copy-to.txt" 2>&1 \
                && timeout 20 wl-paste --type image/png > "$WORK/case10-restored.png" \
                && cmp -s "$WORK/case10.png" "$WORK/case10-restored.png"; then
                pass "case 10: a pinned, cache-backed entry survived a daemon restart and copy-to"
            else
                fail "case 10: copy-to did not restore the pinned payload after the restart"
            fi
        fi
    fi
else
    fail "case 10: the pinned fixture was not ingested"
fi

# --- 17. Case 11: 32 MiB payload ceiling -----------------------------------
# One byte over the ceiling must be rejected outright: no truncated row, no
# daemon death. `ingress` only counts persisted payloads, so liveness is proved
# by the follow-up marker below instead.
head -c $((32 * 1024 * 1024 + 1)) /dev/zero | tr '\0' 'A' > "$WORK/oversize.txt"
before_oversize=$(entry_count)
if timeout 120 wl-copy --type text/plain < "$WORK/oversize.txt"; then
    WL_COPY_OWNED=1
    sleep "$SETTLE_SECONDS"
    after_oversize=$(entry_count)
    if (( after_oversize == before_oversize )) && kill -0 "$DAEMON_PID" 2>/dev/null; then
        pass "case 11: a 32 MiB + 1 byte payload was rejected, not truncated"
    else
        fail "case 11: an oversized payload changed the history ($before_oversize -> $after_oversize)"
    fi
else
    fail "case 11: wl-copy could not publish the oversized payload"
fi
after_oversize_marker="e2e-after-oversize-$$"
if copy_text "$after_oversize_marker" && wait_for_row "$after_oversize_marker"; then
    pass "case 11: the daemon kept ingesting after the oversized selection"
else
    fail "case 11: the daemon stopped ingesting after the oversized selection"
fi

# --- 18. Case 12: burst of copies ------------------------------------------
# Ten selections in quick succession must not kill the daemon, and the last
# one must be the one that is available afterwards.
BURST_MARKER="e2e-burst-$$"
burst_ok=1
for i in $(seq 1 10); do
    if ! copy_text "${BURST_MARKER}-$(printf '%02d' "$i")"; then
        burst_ok=0
        break
    fi
done
WL_COPY_OWNED=1
if (( burst_ok == 0 )); then
    fail "case 12: wl-copy failed during the burst"
elif ! wait_for_row "${BURST_MARKER}-10"; then
    fail "case 12: the last copy of the burst was not ingested"
elif ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    fail "case 12: the daemon died during the burst"
elif [[ "$("$Y4P" show 0 --raw 2>/dev/null || true)" != "${BURST_MARKER}-10" ]]; then
    fail "case 12: the newest entry is not the last copy of the burst"
else
    pass "case 12: ten rapid copies left the daemon alive with the last one on top"
fi

# --- 19. Case 13: paste-from type discovery --------------------------------
# A text/html publisher advertises text/plain as well, which is exactly the
# multi-MIME shape `-l` and `--list-types` must report one type per line.
if copy_text "<i>e2e-types-$$</i>" text/html; then
    WL_COPY_OWNED=1
    short_list="$(timeout 20 "$Y4P" paste-from -l 2>/dev/null || true)"
    long_list="$(timeout 20 "$Y4P" paste-from --list-types 2>/dev/null || true)"
    if [[ "$short_list" != "$long_list" ]]; then
        fail "case 13: -l and --list-types disagree"
    elif [[ "$short_list" != *"text/html"* ]] || [[ "$short_list" != *"text/plain"* ]]; then
        fail "case 13: the listed types do not cover text/html and text/plain ('$short_list')"
    elif (( $(printf '%s\n' "$short_list" | wc -l) < 2 )); then
        fail "case 13: the types were not printed one per line"
    else
        pass "case 13: paste-from -l/--list-types report every offered MIME, one per line"
    fi
else
    fail "case 13: wl-copy could not publish the multi-MIME selection"
fi

# --- 20. Case 14: opt-in sanitisation --------------------------------------
# A separate daemon session with `sanitize = true`: a JPEG is transcoded to
# PNG, while an APNG is still bypassed untouched because flattening an
# animation would destroy it.
stop_daemon
mkdir -p "$XDG_CONFIG_HOME/y4p"
printf '[image]\nsanitize = true\n' > "$XDG_CONFIG_HOME/y4p/y4p.toml"
if ! start_daemon; then
    fail "case 14: the daemon did not start with sanitisation enabled"
else
    sleep "$SETTLE_SECONDS"
    generated "$WORK/case14.jpg" -size 100x100 plasma:fractal
    before_sanitize=$(entry_count)
    if copy_file image/jpeg "$WORK/case14.jpg" && wait_for_entries_at_least $((before_sanitize + 1)); then
        WL_COPY_OWNED=1
        sanitized_id="$(newest_id)"
        sanitized_row="$(newest_row)"
        show_payload "$sanitized_id" "$WORK/case14.out" || true
        sanitized_magic="$(head -c 4 "$WORK/case14.out" 2>/dev/null | od -An -tx1 | tr -d ' \n' || true)"
        if [[ "$sanitized_magic" == "89504e47" ]] && [[ "$sanitized_row" == *"[png]"* ]]; then
            pass "case 14: sanitisation transcoded the JPEG to PNG"
        else
            fail "case 14: sanitisation did not transcode the JPEG (magic $sanitized_magic)"
        fi
    else
        fail "case 14: the JPEG was not ingested with sanitisation enabled"
    fi

    generated "$WORK/case14-a.png" -size 64x64 plasma:fractal
    generated "$WORK/case14-b.png" -size 64x64 plasma:fractal
    magick -delay 20 -loop 0 "$WORK/case14-a.png" "$WORK/case14-b.png" "apng:$WORK/case14.png"
    before_sanitized_apng=$(entry_count)
    if copy_file image/png "$WORK/case14.png" && wait_for_entries_at_least $((before_sanitized_apng + 1)); then
        sanitized_apng_id="$(newest_id)"
        if show_payload "$sanitized_apng_id" "$WORK/case14-apng.out" \
            && cmp -s "$WORK/case14.png" "$WORK/case14-apng.out"; then
            pass "case 14: sanitisation left the APNG untouched"
        else
            fail "case 14: sanitisation altered the APNG"
        fi
    else
        fail "case 14: the APNG was not ingested with sanitisation enabled"
    fi
fi

# --- 21. Observability ------------------------------------------------------
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

# `list` must still reflect the ingested rows, oldest text first.
if [[ "$(history_rows)" == *"$PLAIN_MARKER"* ]]; then
    pass "list: the ingested text is still in the history"
else
    fail "list: the ingested text vanished from the history"
fi

# --- 22. Summary ------------------------------------------------------------
printf '\n%d passed, %d failed\n' "$PASSED" "$FAILED"
if (( FAILED > 0 )); then
    # The daemon log is the only place a swallowed ingestion error would show.
    printf '\n--- daemon log (last 40 lines) ---\n'
    tail -n 40 "$WORK/log/daemon.log" || true
    exit 1
fi
exit 0
