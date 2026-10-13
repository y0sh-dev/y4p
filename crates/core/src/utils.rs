// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/core/src/utils.rs

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

/// Normalises a raw `text/uri-list` payload (RFC 2483) into a plain,
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

/// Extracts a MIME/media-type string's base type/subtype — everything
/// before the first `;`-separated parameter — trimming ASCII whitespace
/// around it. Zero allocations: the result is a slice into `mime` itself,
/// and case is left exactly as given (comparisons use `eq_ignore_ascii_case`
/// rather than an upfront `to_ascii_lowercase` copy — see `mime_base_eq`).
#[inline]
pub fn mime_base(mime: &str) -> &str {
    mime.split(';').next().unwrap_or("").trim()
}

/// True if `a` and `b` name the same base MIME type once both are run
/// through `mime_base` — case- and whitespace-insensitive, and indifferent
/// to any parameters trailing either side. Zero allocations.
pub fn mime_base_eq(a: &str, b: &str) -> bool {
    mime_base(a).eq_ignore_ascii_case(mime_base(b))
}

/// Case-insensitive, ASCII-only, allocation-free `s.starts_with(prefix)` —
/// no `to_ascii_lowercase` copy of either side. `s.get(..prefix.len())`
/// never panics on a string shorter than `prefix` or on a multi-byte char
/// boundary (it returns `None` for either instead of slicing).
pub fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// Identifies an image payload's real format from its leading bytes,
/// independent of whatever MIME label a sender claims — the ingestion path
/// trusts the bytes, never the label.
pub fn detect_image_mime(data: &[u8]) -> Option<&'static str> {
    if data.len() >= 4 {
        match &data[0..4] {
            [0x89, 0x50, 0x4E, 0x47] => return Some("image/png"),
            [0xFF, 0xD8, 0xFF, _] => return Some("image/jpeg"),
            [0x47, 0x49, 0x46, 0x38] => return Some("image/gif"),
            b"RIFF" if data.len() >= 12 && &data[8..12] == b"WEBP" => return Some("image/webp"),
            _ => {}
        }
        // AVIF: ISOBMFF container — bytes 4..8 are literally "ftyp",
        // followed by a 4-byte major brand naming the specific format.
        if data.len() >= 12 && &data[4..8] == b"ftyp" && matches!(&data[8..12], b"avif" | b"avis") {
            return Some("image/avif");
        }
    }

    // SVG has no fixed magic bytes (it's XML text), so it's only checked
    // once every binary signature above has missed.
    let head = data.trim_ascii_start();
    (head.starts_with(b"<?xml") || head.starts_with(b"<svg")).then_some("image/svg+xml")
}

/// Returns whether a byte stream represents an animated WebP image.
///
/// Under the WebP container specification, animated images use the extended
/// format (`VP8X`) with the `Animation` flag (bit 1, mask 0x02) set at offset 20.
/// Simple lossy (`VP8 `) and lossless (`VP8L`) containers cannot be animated.
pub fn is_animated_webp(data: &[u8]) -> bool {
    data.len() >= 21
        && &data[0..4] == b"RIFF"
        && &data[8..12] == b"WEBP"
        && &data[12..16] == b"VP8X"
        && (data[20] & 0x02) != 0
}

/// True when `mime`'s base type (see `mime_base`) is HTML or XHTML markup
/// (`text/html` or `application/xhtml+xml`) eligible for forced plain-text fallback.
pub fn is_html_mime(mime: &str) -> bool {
    let base = mime_base(mime);
    base.eq_ignore_ascii_case("text/html") || base.eq_ignore_ascii_case("application/xhtml+xml")
}

/// True when `mime`'s base type (see `mime_base`) is RTF (`text/rtf` or `application/rtf`).
pub fn is_rtf_mime(mime: &str) -> bool {
    let base = mime_base(mime);
    base.eq_ignore_ascii_case("text/rtf") || base.eq_ignore_ascii_case("application/rtf")
}

/// True when `mime` represents a text-like payload (plain text, rich markup,
/// JSON, XML, URI lists, or legacy string atoms) eligible for preview generation,
/// inline text storage, and full-text search indexing.
///
/// Binary media (images, audio, video) are explicitly excluded, ensuring types like
/// `image/svg+xml` are not treated as text-like payloads.
pub fn is_text_like_mime(mime: &str) -> bool {
    let base = mime_base(mime);
    if starts_with_ignore_ascii_case(base, "image/")
        || starts_with_ignore_ascii_case(base, "audio/")
        || starts_with_ignore_ascii_case(base, "video/")
    {
        return false;
    }

    let base_bytes = base.as_bytes();
    crate::constants::TEXT_LIKE_MIME_HINTS.iter().any(|&hint| {
        let hint_bytes = hint.as_bytes();
        base_bytes.len() >= hint_bytes.len()
            && base_bytes.windows(hint_bytes.len()).any(|w| w.eq_ignore_ascii_case(hint_bytes))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn is_text_like_mime_identifies_text_markup_and_data_formats() {
        assert!(is_text_like_mime("text/plain"));
        assert!(is_text_like_mime("text/plain; charset=utf-8"));
        assert!(is_text_like_mime("TEXT/PLAIN"));
        assert!(is_text_like_mime("text/html"));
        assert!(is_text_like_mime("text/markdown"));
        assert!(is_text_like_mime("text/uri-list"));
        assert!(is_text_like_mime("application/json"));
        assert!(is_text_like_mime("application/ld+json"));
        assert!(is_text_like_mime("application/xml"));
        assert!(is_text_like_mime("application/xhtml+xml"));
        assert!(is_text_like_mime("application/atom+xml"));
        assert!(is_text_like_mime("UTF8_STRING"));
        assert!(is_text_like_mime("utf8_string"));
        assert!(is_text_like_mime("STRING"));
        assert!(is_text_like_mime("TEXT"));
        assert!(is_text_like_mime("compound_text"));
        assert!(is_text_like_mime("application/x-uri-list"));
    }

    #[test]
    fn is_text_like_mime_rejects_binary_media_and_non_text_types() {
        assert!(!is_text_like_mime("image/png"));
        assert!(!is_text_like_mime("image/jpeg"));
        assert!(!is_text_like_mime("image/svg+xml"));
        assert!(!is_text_like_mime("audio/mpeg"));
        assert!(!is_text_like_mime("video/mp4"));
        assert!(!is_text_like_mime("application/octet-stream"));
        assert!(!is_text_like_mime("application/pdf"));
        assert!(!is_text_like_mime("application/zip"));
    }

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
        // deterministic, documented behaviour rather than a full HTML parse.
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
    fn mime_base_strips_trailing_parameters() {
        assert_eq!(mime_base("Text/Plain ; charset=UTF-8 ; foo=bar"), "Text/Plain");
    }

    #[test]
    fn mime_base_trims_surrounding_whitespace() {
        assert_eq!(mime_base("  text/plain  ;charset=utf-8"), "text/plain");
    }

    #[test]
    fn mime_base_no_parameters_returns_whole_string_trimmed() {
        assert_eq!(mime_base("text/html"), "text/html");
    }

    #[test]
    fn mime_base_preserves_case() {
        // Case-folding is the caller's job (via `eq_ignore_ascii_case`),
        // not `mime_base`'s — it only isolates the base, verbatim.
        assert_eq!(mime_base("TEXT/PLAIN"), "TEXT/PLAIN");
    }

    #[test]
    fn starts_with_ignore_ascii_case_matches_same_case() {
        assert!(starts_with_ignore_ascii_case("image/png", "image/"));
    }

    #[test]
    fn starts_with_ignore_ascii_case_matches_different_case() {
        assert!(starts_with_ignore_ascii_case("IMAGE/PNG", "image/"));
        assert!(starts_with_ignore_ascii_case("Text/Html", "TEXT/"));
    }

    #[test]
    fn starts_with_ignore_ascii_case_rejects_non_prefix() {
        assert!(!starts_with_ignore_ascii_case("text/plain", "image/"));
    }

    #[test]
    fn starts_with_ignore_ascii_case_shorter_than_prefix_never_panics() {
        assert!(!starts_with_ignore_ascii_case("im", "image/"));
        assert!(!starts_with_ignore_ascii_case("", "image/"));
    }

    #[test]
    fn detect_image_mime_png() {
        assert_eq!(detect_image_mime(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A]), Some("image/png"));
    }

    #[test]
    fn detect_image_mime_jpeg() {
        assert_eq!(detect_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
    }

    #[test]
    fn detect_image_mime_gif() {
        assert_eq!(detect_image_mime(b"GIF89a...."), Some("image/gif"));
    }

    #[test]
    fn detect_image_mime_webp() {
        let mut data = b"RIFF".to_vec();
        data.extend_from_slice(&[0, 0, 0, 0]); // chunk size, irrelevant here
        data.extend_from_slice(b"WEBP");
        assert_eq!(detect_image_mime(&data), Some("image/webp"));
    }

    #[test]
    fn detect_image_mime_avif() {
        let mut data = vec![0, 0, 0, 0x20];
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(b"avif");
        assert_eq!(detect_image_mime(&data), Some("image/avif"));
    }

    #[test]
    fn detect_image_mime_avif_sequence_brand() {
        let mut data = vec![0, 0, 0, 0x20];
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(b"avis");
        assert_eq!(detect_image_mime(&data), Some("image/avif"));
    }

    #[test]
    fn detect_image_mime_svg_xml_declaration() {
        assert_eq!(detect_image_mime(b"<?xml version=\"1.0\"?><svg></svg>"), Some("image/svg+xml"));
    }

    #[test]
    fn detect_image_mime_svg_bare() {
        assert_eq!(detect_image_mime(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>"), Some("image/svg+xml"));
    }

    #[test]
    fn detect_image_mime_unrecognized_returns_none() {
        assert_eq!(detect_image_mime(b"not an image at all"), None);
        assert_eq!(detect_image_mime(b""), None);
    }

    // --- is_html_mime ---

    #[test]
    fn is_html_mime_accepts_html_and_xhtml() {
        assert!(is_html_mime("text/html"));
        assert!(is_html_mime("text/html; charset=utf-8"));
        assert!(is_html_mime("TEXT/HTML;charset=UTF-8"));
        assert!(is_html_mime("application/xhtml+xml"));
        assert!(is_html_mime("APPLICATION/XHTML+XML; charset=utf-8"));
    }

    #[test]
    fn is_html_mime_rejects_non_html() {
        assert!(!is_html_mime("text/plain"));
        assert!(!is_html_mime("text/rtf"));
        assert!(!is_html_mime("image/png"));
        assert!(!is_html_mime("application/json"));
        assert!(!is_html_mime("application/xml"));
    }

    // --- is_rtf_mime ---

    #[test]
    fn is_rtf_mime_accepts_rtf_variants() {
        assert!(is_rtf_mime("text/rtf"));
        assert!(is_rtf_mime("Text/RTF"));
        assert!(is_rtf_mime("TEXT/RTF; charset=utf-8"));
        assert!(is_rtf_mime("application/rtf"));
        assert!(is_rtf_mime("APPLICATION/RTF"));
    }

    #[test]
    fn is_rtf_mime_rejects_non_rtf() {
        assert!(!is_rtf_mime("text/plain"));
        assert!(!is_rtf_mime("text/html"));
        assert!(!is_rtf_mime("image/png"));
    }

    // --- is_animated_webp ---

    #[test]
    fn is_animated_webp_detects_vp8x_with_animation_flag() {
        let mut header = vec![0u8; 24];
        header[0..4].copy_from_slice(b"RIFF");
        header[8..12].copy_from_slice(b"WEBP");
        header[12..16].copy_from_slice(b"VP8X");
        header[20] = 0x02; // Animation flag
        assert!(is_animated_webp(&header));

        // Mixed flags including animation (e.g. ICC + Animation: 0x20 | 0x02 = 0x22)
        header[20] = 0x22;
        assert!(is_animated_webp(&header));
    }

    #[test]
    fn is_animated_webp_rejects_static_webp_and_other_formats() {
        let mut header = vec![0u8; 24];
        header[0..4].copy_from_slice(b"RIFF");
        header[8..12].copy_from_slice(b"WEBP");

        // Simple lossy (VP8 )
        header[12..16].copy_from_slice(b"VP8 ");
        assert!(!is_animated_webp(&header));

        // Simple lossless (VP8L)
        header[12..16].copy_from_slice(b"VP8L");
        assert!(!is_animated_webp(&header));

        // Extended (VP8X) without animation flag (e.g. only alpha: 0x10)
        header[12..16].copy_from_slice(b"VP8X");
        header[20] = 0x10;
        assert!(!is_animated_webp(&header));

        // Truncated data
        assert!(!is_animated_webp(&header[..20]));
        assert!(!is_animated_webp(b"RIFFWEBP"));
        assert!(!is_animated_webp(b""));

        // Non-WebP RIFF (e.g. WAV audio)
        let mut wav = vec![0u8; 24];
        wav[0..4].copy_from_slice(b"RIFF");
        wav[8..12].copy_from_slice(b"WAVE");
        assert!(!is_animated_webp(&wav));
    }
}
