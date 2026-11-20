#!/usr/bin/env bash
# Copyright (C) 2026 yosana
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Minimal fzf clipboard history picker for y4p.
# Uses stable database IDs (--id) so background clipboard updates cannot shift
# the selected entry between listing and restoration.
set -euo pipefail

Y4P_BIN="${Y4P_BIN:-y4p}"
LIMIT="${Y4P_FZF_RANGE:-0-200}"

selection="$(
    "$Y4P_BIN" list "$LIMIT" --raw --id \
        | fzf \
            --no-sort \
            --prompt='y4p> ' \
            --preview="$Y4P_BIN show \$(printf '%s' {} | sed -E 's/^\\[ *([0-9]+)\\].*/\\1/') --id" \
            --preview-window='right:50%:wrap' \
        || true
)"

[[ -z "$selection" ]] && exit 0

id="$(printf '%s\n' "$selection" | sed -E 's/^\[ *([0-9]+)\].*/\1/')"
if [[ -n "$id" ]]; then
    exec "$Y4P_BIN" copy-to "$id" --id
fi
