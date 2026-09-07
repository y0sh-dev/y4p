// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/core/utils.rs

use percent_encoding::percent_decode_str;

/// Normalizes a raw `text/uri-list` payload (RFC 2483) into a plain,
/// newline-joined list of filesystem paths: comment/blank lines dropped,
/// the `file:` scheme stripped, and percent-encoding decoded.
///
/// Strips `file://` (authority form) first, then falls back to bare `file:`
/// (no slash consumed) rather than a literal `file:/` — a `file:/path` URI's
/// own leading slash is part of the path, not the prefix, so this keeps the
/// result rooted (`/home/user/x`) instead of losing the leading `/`.
pub fn normalize_uri_list(data: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(data);

    let paths: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let path = line.strip_prefix("file://").or_else(|| line.strip_prefix("file:")).unwrap_or(line);
            percent_decode_str(path).decode_utf8_lossy().into_owned()
        })
        .collect();

    paths.join("\n").into_bytes()
}
