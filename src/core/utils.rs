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

/// Minimal `<tag>` remover for rich markup (e.g. `text/html`) that needs to
/// be shown or matched as plain text — not a parser, just a `<`/`>` toggle
/// over the raw bytes, per the project's no-extra-crates policy. Only `<`
/// opens tag-mode; a `>` encountered while NOT already inside a tag is
/// ordinary text and passes through untouched, so a stray `>` in plain
/// prose (e.g. `if x > 3`) isn't silently eaten. Angle brackets inside a
/// quoted attribute value aren't special-cased; fine for a readable
/// fallback/preview, not a renderer.
pub fn strip_html_tags(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut in_tag = false;
    for &b in data {
        match b {
            b'<' => in_tag = true,
            b'>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(b),
            _ => {}
        }
    }
    out
}

/// Splits a MIME/media-type string into its base type/subtype and any
/// trailing `;`-separated parameters, trimming ASCII whitespace around each
/// piece and lowercasing the base (ASCII-only) for case-insensitive
/// comparison. RFC 2045 type/subtype names are case-insensitive, and real
/// senders vary — `TEXT/PLAIN`, `text/plain; charset=utf-8` (space after
/// `;`), `text/plain;charset=UTF-8` — none of which a bare `==`/
/// `starts_with` check catches reliably. Parameter segments are trimmed but
/// not case-folded: a param *value* like `charset=UTF-8` can meaningfully
/// carry case, only the base type/subtype doesn't. Standard library only —
/// `split`/`trim`/`to_ascii_lowercase`, no external crate.
pub fn parse_mime(mime: &str) -> (String, Vec<&str>) {
    let mut segments = mime.split(';');
    let base = segments.next().unwrap_or("").trim().to_ascii_lowercase();
    let params = segments.map(str::trim).filter(|p| !p.is_empty()).collect();
    (base, params)
}

/// True if `a` and `b` name the same base MIME type once both are run
/// through `parse_mime` — case- and whitespace-insensitive, and indifferent
/// to any parameters trailing either side.
pub fn mime_base_eq(a: &str, b: &str) -> bool {
    parse_mime(a).0 == parse_mime(b).0
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_tags_basic() {
        assert_eq!(strip_html_tags(b"<b>Hello</b> <i>World</i>"), b"Hello World");
    }

    #[test]
    fn strip_html_tags_utf8_multibyte() {
        let input = "<p>こんにちは、<strong>世界</strong>！🦀</p>".as_bytes();
        let expected = "こんにちは、世界！🦀".as_bytes();
        assert_eq!(strip_html_tags(input), expected);
    }

    #[test]
    fn strip_html_tags_attributes() {
        let input = br#"<div class="main" style="color: red;">Content</div>"#;
        assert_eq!(strip_html_tags(input), b"Content");
    }

    #[test]
    fn strip_html_tags_angle_bracket_in_attribute() {
        // The naive </> toggle has no notion of quoted attribute values, so
        // a literal '<' inside one (invalid HTML, but real-world markup
        // isn't always well-formed) still opens tag-mode early — the
        // deterministic, documented behavior rather than a full HTML parse.
        let input = br#"<span title="a < b">text</span>"#;
        assert_eq!(strip_html_tags(input), b"text");
    }

    #[test]
    fn strip_html_tags_unclosed_tag() {
        assert_eq!(strip_html_tags(b"Hello <strong world"), b"Hello ");
    }

    #[test]
    fn strip_html_tags_lone_closing_bracket_passes_through() {
        // Only '<' opens tag-mode; a '>' outside of one is ordinary text.
        assert_eq!(strip_html_tags(b"5 > 3 & 2 < 4"), b"5 > 3 & 2 ");
    }

    #[test]
    fn strip_html_tags_empty_and_consecutive_tags() {
        assert_eq!(strip_html_tags(b"<><p></p><br/>"), b"");
    }

    #[test]
    fn strip_html_tags_plain_text_untouched() {
        let input: &[u8] = b"Plain text without any tags.";
        assert_eq!(strip_html_tags(input), input);
    }

    #[test]
    fn strip_html_tags_empty_input() {
        assert_eq!(strip_html_tags(b""), b"");
    }

    #[test]
    fn normalize_uri_list_standard() {
        let input = b"file:///path/to/a.txt\nfile:///path/to/b.png";
        assert_eq!(normalize_uri_list(input), b"/path/to/a.txt\n/path/to/b.png");
    }

    #[test]
    fn normalize_uri_list_crlf() {
        let input = b"file:///a.txt\r\nfile:///b.txt\r\n";
        assert_eq!(normalize_uri_list(input), b"/a.txt\n/b.txt");
    }

    #[test]
    fn normalize_uri_list_non_empty_authority() {
        let input = b"file://localhost/etc/hosts\nfile://myhost/var/log";
        assert_eq!(normalize_uri_list(input), b"/etc/hosts\n/var/log");
    }

    #[test]
    fn normalize_uri_list_single_slash_and_relative() {
        let input = b"file:/var/log/syslog\nfile:local.txt";
        assert_eq!(normalize_uri_list(input), b"/var/log/syslog\nlocal.txt");
    }

    #[test]
    fn normalize_uri_list_percent_decoding() {
        let input = b"file:///home/user/My%20Documents/test%231.txt";
        assert_eq!(normalize_uri_list(input), "/home/user/My Documents/test#1.txt".as_bytes());
    }

    #[test]
    fn normalize_uri_list_utf8_percent_decoding() {
        let input = b"file:///home/%E3%83%86%E3%82%B9%E3%83%88.txt";
        assert_eq!(normalize_uri_list(input), "/home/テスト.txt".as_bytes());
    }

    #[test]
    fn normalize_uri_list_non_utf8_raw_bytes_preserved() {
        // Must NOT lossy-decode: a naive `String`-based implementation would
        // replace 0xFF/0xFE/0xFD with U+FFFD and corrupt the path.
        let input = b"file:///%FF%FE%FD";
        assert_eq!(normalize_uri_list(input), vec![0x2F, 0xFF, 0xFE, 0xFD]);
    }

    #[test]
    fn normalize_uri_list_comments_and_blank_lines_ignored() {
        let input = b"# Comment\n\nfile:///path/a\n# Another\nfile:///path/b";
        assert_eq!(normalize_uri_list(input), b"/path/a\n/path/b");
    }

    #[test]
    fn mime_base_eq_ignores_case_and_param_whitespace() {
        assert!(mime_base_eq("TEXT/PLAIN", "text/plain"));
        assert!(mime_base_eq("text/plain; charset=utf-8", "text/plain"));
        assert!(mime_base_eq("text/plain;charset=UTF-8", "text/plain;charset=utf-8"));
        assert!(!mime_base_eq("text/html", "text/plain"));
    }

    #[test]
    fn parse_mime_splits_base_and_params() {
        let (base, params) = parse_mime("Text/Plain ; charset=UTF-8 ; foo=bar");
        assert_eq!(base, "text/plain");
        assert_eq!(params, vec!["charset=UTF-8", "foo=bar"]);
    }
}
