// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/core/utils.rs

use percent_encoding::percent_decode;

/// Strips a `file:` URI down to its path, handling a non-empty Authority
/// (e.g. `file://localhost/path`) by skipping to its own next `/` instead
/// of leaking the host into the path like a naive prefix strip would.
fn strip_file_scheme(line: &[u8]) -> &[u8] {
    let Some(rest) = line.strip_prefix(b"file:") else { return line; };

    match rest.strip_prefix(b"//") {
        // Empty authority ("file:///path") — already rooted.
        Some(after) if after.starts_with(b"/") => after,
        // Non-empty authority ("file://host/path") — skip past it to its own '/'.
        Some(after) => after.iter().position(|&b| b == b'/').map(|i| &after[i..]).unwrap_or(b""),
        // No "//" at all ("file:/path" or "file:path") — path's own leading
        // '/', if any, is preserved since only "file:" itself was consumed.
        None => rest,
    }
}

/// Normalizes a raw `text/uri-list` payload (RFC 2483) into a plain,
/// newline-joined list of filesystem paths: comment/blank lines dropped,
/// the `file:` scheme (and Authority) stripped, percent-decoded.
///
/// Stays on raw bytes end to end — no `url` crate, no `String`/
/// `decode_utf8_lossy` — since a Linux path is an arbitrary byte sequence
/// and isn't guaranteed to be valid UTF-8; lossy-decoding it would silently
/// corrupt it (replace the offending bytes with U+FFFD).
pub fn normalize_uri_list(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut wrote_any = false;

    for raw_line in data.split(|&b| b == b'\n') {
        let line = raw_line.trim_ascii();
        if line.is_empty() || line[0] == b'#' { continue; }

        let decoded: Vec<u8> = percent_decode(strip_file_scheme(line)).collect();
        if decoded.is_empty() { continue; }

        if wrote_any { out.push(b'\n'); }
        out.extend_from_slice(&decoded);
        wrote_any = true;
    }

    out
}
