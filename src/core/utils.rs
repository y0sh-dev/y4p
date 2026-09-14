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

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Original Image Fetcher (v0.3.0): extracts the first `<img ... src="...">`
/// (or `src='...'`) URL from a raw `text/html` clipboard payload.
///
/// Byte-slice scanning only — no HTML parser or regex crate, per the
/// project's no-extra-dependency policy for this feature. `<img` and `src=`
/// are matched case-insensitively via a lowercased copy; the returned URL is
/// sliced from the *original* bytes so its casing is preserved. The search
/// for `src=` is bounded to the first `<img...>` tag's own closing `>`, so a
/// later, unrelated `src=` elsewhere in the document is never mistaken for
/// this tag's attribute.
pub fn extract_image_url_from_html(html: &[u8]) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let img_pos = find_subslice(&lower, b"<img")?;
    let tag_end = find_subslice(&lower[img_pos..], b">")
        .map(|rel| img_pos + rel)
        .unwrap_or(lower.len());

    let src_rel = find_subslice(&lower[img_pos..tag_end], b"src=")?;
    let mut i = img_pos + src_rel + 4; // past "src="
    while lower.get(i).is_some_and(u8::is_ascii_whitespace) { i += 1; }

    let quote = *html.get(i)?;
    if quote != b'"' && quote != b'\'' { return None; }
    i += 1;
    let start = i;
    while html.get(i).is_some_and(|&b| b != quote) { i += 1; }

    (i < html.len()).then(|| String::from_utf8_lossy(&html[start..i]).into_owned())
}

/// Strict allow-list for the Original Image Fetcher's network fetch: only
/// `http://`/`https://` URLs are eligible. Rejects `data:image/...` (already
/// local — no fetch needed) and `file://`/relative paths (a "network fetch"
/// of a local path is never correct) alike, so a malformed or hostile
/// extraction can never reach `curl` with something other than a plain web URL.
pub fn is_valid_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Identifies an image payload's real format from its leading bytes,
/// independent of whatever MIME label a sender (or a `curl`-fetched HTTP
/// response's `Content-Type`, which this function never even looks at)
/// claims. Shared by the ordinary Wayland ingestion path and the Original
/// Image Fetcher, so both trust the bytes, never the label.
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

/// Image Hijacker quality fix: a `curl` download (or, less often, a
/// compositor transfer) can lose its final bytes to a dropped connection or
/// a truncating proxy without that failure ever surfacing as a non-zero
/// exit code or an empty response — the payload just silently ends early.
/// This detects the three formats with a fixed, well-known trailer and
/// appends it when missing, so a subtly-truncated download still decodes
/// instead of failing (or worse, half-rendering) in whatever application
/// receives it. Every other format (WebP, SVG, AVIF, ...) has no single
/// fixed byte sequence this function could safely reconstruct, so it passes
/// through untouched rather than risk corrupting a container it doesn't
/// actually understand.
pub fn sanitize_image_payload(mut data: Vec<u8>, mime: &str) -> Vec<u8> {
    const PNG_IEND: [u8; 12] = [0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82];

    match mime {
        "image/jpeg" if !data.ends_with(&[0xFF, 0xD9]) => data.extend_from_slice(&[0xFF, 0xD9]),
        "image/png" if !data.ends_with(&PNG_IEND) => data.extend_from_slice(&PNG_IEND),
        "image/gif" if data.last() != Some(&0x3B) => data.push(0x3B),
        _ => {}
    }
    data
}

/// Runs `curl` as a child process to fetch `url`'s raw bytes for the
/// Original Image Fetcher. `-sL` suppresses curl's own progress output and
/// follows redirects (a source URL captured from `text/html` is very often
/// a CDN redirect, not the final asset); `--max-time`/`--connect-timeout`
/// bound how long a stalled server can hold up the fetch; `--max-filesize`
/// aborts the transfer before a hostile response can allocate unbounded
/// memory; `--user-agent` avoids the hotlink-protection 403 some CDNs
/// (Cloudflare, Pixiv, ...) return to obviously non-browser clients.
///
/// `Command::output()` buffers curl's entire stdout in memory before this
/// returns — `--max-filesize` is what keeps that bounded, not this
/// function's own logic.
pub fn fetch_image_via_curl(url: &str) -> Result<Vec<u8>, String> {
    let output = std::process::Command::new("curl")
        .arg("-sL")
        .arg("--max-time")
        .arg(crate::core::constants::CURL_TIMEOUT_SECS.to_string())
        .arg("--connect-timeout")
        .arg("3")
        .arg("--max-filesize")
        .arg(crate::core::constants::MAX_IMAGE_FETCH_SIZE.to_string())
        .arg("--user-agent")
        .arg(crate::core::constants::FETCH_USER_AGENT)
        .arg(url)
        .output()
        .map_err(|e| format!("curl spawn failed: {}", e))?;

    if !output.status.success() {
        return Err(format!("curl exited with {}", output.status));
    }
    if output.stdout.is_empty() {
        return Err("curl returned an empty response".to_string());
    }
    // Belt-and-suspenders: `--max-filesize` should already have aborted the
    // transfer before this point, but a size ceiling enforced entirely by an
    // external process is never trusted without also checking it here.
    if output.stdout.len() > crate::core::constants::MAX_IMAGE_FETCH_SIZE {
        return Err("fetched payload exceeds MAX_IMAGE_FETCH_SIZE".to_string());
    }

    Ok(output.stdout)
}

/// True when `mime`'s base type (see `parse_mime`) matches one of the
/// plain-text alternatives y4p treats as Ingress-sanitizable prose —
/// `TEXT_MIME_ALTS` (`text/plain`, the raw X11/Wayland text atoms, ...).
/// Used to gate `sanitize_text_payload` so binary/rich payloads (images,
/// `text/html`, `application/json`, ...) are never run through URL/query
/// rewriting meant for plain prose.
pub fn is_text_mime(mime: &str) -> bool {
    crate::core::constants::TEXT_MIME_ALTS.iter().any(|&alt| mime_base_eq(mime, alt))
}

/// True when `mime`'s base type (see `parse_mime`) is HTML or XHTML markup
/// (`text/html` or `application/xhtml+xml`) eligible for forced plain-text fallback.
pub fn is_html_mime(mime: &str) -> bool {
    let base = parse_mime(mime).0;
    base == "text/html" || base == "application/xhtml+xml"
}

/// True when `mime`'s base type (see `parse_mime`) is RTF (`text/rtf` or `application/rtf`).
pub fn is_rtf_mime(mime: &str) -> bool {
    let base = parse_mime(mime).0;
    base == "text/rtf" || base == "application/rtf"
}

/// Case-insensitive `s.get(..prefix.len())` prefix check that never panics
/// on a short string or a multi-byte char boundary (`str::get` returns
/// `None` for either instead of slicing).
fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// True if `key` is one of y4p's known tracking-parameter names, given the
/// domain-specific context already resolved by the caller. `utm_*`/`ref_*`
/// are prefix rules (can't live in a fixed-string slice), so they're
/// checked here directly rather than added to `UNIVERSAL_TRACKING_KEYS`.
/// Amazon's bare `ref` is domain-gated (unlike `ref_*`, which is universal)
/// because plenty of non-tracking sites use a bare `ref` query key too.
fn is_tracking_key(key: &str, is_x_twitter: bool, is_amazon: bool) -> bool {
    if starts_with_ignore_case(key, "utm_") || starts_with_ignore_case(key, "ref_") {
        return true;
    }
    if crate::core::constants::UNIVERSAL_TRACKING_KEYS.iter().any(|&k| key.eq_ignore_ascii_case(k)) {
        return true;
    }
    if is_x_twitter && (key.eq_ignore_ascii_case("s") || key.eq_ignore_ascii_case("t")) {
        return true;
    }
    if is_amazon && key.eq_ignore_ascii_case("ref") {
        return true;
    }
    false
}

/// True if `host` is exactly `domain`, or a subdomain of it (`sub.domain`
/// — never a look-alike like `notdomain.com`).
fn host_matches(host: &str, domain: &str) -> bool {
    if host.eq_ignore_ascii_case(domain) {
        return true;
    }
    let Some(suffix_start) = host.len().checked_sub(domain.len() + 1) else { return false; };
    host.as_bytes()[suffix_start] == b'.'
        && host.get(suffix_start + 1..).is_some_and(|suf| suf.eq_ignore_ascii_case(domain))
}

/// Extracts the host (Authority, minus any `user:pass@`/port) from a URL's
/// pre-query portion. Byte-slice scanning only: every split point comes
/// from `find`/`split` on an ASCII delimiter, which — by UTF-8's
/// self-synchronizing design — always lands on a char boundary, so none of
/// this can panic even on a URL with non-ASCII path/host segments.
fn extract_host(before_query: &str) -> &str {
    let after_scheme = before_query.find("://").map_or(before_query, |i| &before_query[i + 3..]);
    let end = after_scheme.find(['/', '#']).unwrap_or(after_scheme.len());
    let authority = &after_scheme[..end];
    let host_and_port = authority.rsplit('@').next().unwrap_or(authority);
    host_and_port.split(':').next().unwrap_or(host_and_port)
}

/// Rebuilds `url` with every tracking query parameter (see module docs on
/// `UNIVERSAL_TRACKING_KEYS` and the domain-specific/YouTube-`t` exception
/// rules) removed, preserving parameter order, an untouched fragment, and
/// dropping the `?` entirely when nothing legitimate survives. Fast-path:
/// a URL with no `?` at all has no query to touch and is returned as-is.
pub fn clean_url_tracking_params(url: &str) -> String {
    let Some(q_pos) = url.find('?') else { return url.to_string(); };

    let before_query = &url[..q_pos];
    let after_query = &url[q_pos + 1..];
    let (query, fragment) = match after_query.find('#') {
        Some(h) => (&after_query[..h], &after_query[h..]),
        None => (after_query, ""),
    };

    let host = extract_host(before_query);
    let is_x_twitter = host_matches(host, "x.com") || host_matches(host, "twitter.com");
    let is_amazon = host.split('.').any(|label| label.eq_ignore_ascii_case("amazon"));
    let is_youtube = host_matches(host, "youtube.com") || host_matches(host, "youtu.be");

    let mut kept = String::with_capacity(query.len());
    for pair in query.split('&') {
        if pair.is_empty() { continue; }
        let key = pair.split('=').next().unwrap_or("");

        // YouTube's `t` is a playback-start-second, never a tracking token —
        // protected even though x.com/twitter.com's `t` (a share token) is
        // stripped, since the two never share a host.
        let protected = is_youtube && key.eq_ignore_ascii_case("t");
        if protected || !is_tracking_key(key, is_x_twitter, is_amazon) {
            if !kept.is_empty() { kept.push('&'); }
            kept.push_str(pair);
        }
    }

    let mut result = String::with_capacity(before_query.len() + 1 + kept.len() + fragment.len());
    result.push_str(before_query);
    if !kept.is_empty() {
        result.push('?');
        result.push_str(&kept);
    }
    result.push_str(fragment);
    result
}

/// Locates the nearer of the next `"http://"`/`"https://"` occurrence in
/// `s`, if any — the entry point `sanitize_text_payload` treats as the
/// start of an embedded URL.
fn find_url_start(s: &str) -> Option<usize> {
    match (s.find("https://"), s.find("http://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// True if `prefix` (`token` up to but not including the trailing `close`
/// byte) contains an unmatched opening bracket for it — i.e. the trailing
/// bracket belongs to the URL itself (e.g. a Wikipedia
/// `(disambiguation)`-style path segment) rather than to surrounding prose.
fn has_matching_open(prefix: &str, close: u8) -> bool {
    let open = match close {
        b')' => b'(',
        b']' => b'[',
        b'}' => b'{',
        _ => return false,
    };
    let mut depth: i32 = 0;
    for &b in prefix.as_bytes() {
        if b == open { depth += 1; } else if b == close { depth -= 1; }
    }
    depth > 0
}

/// Returns the byte length of `token` with trailing sentence punctuation
/// (`.`, `,`, `;`, `:`, `!`, `?`, quotes) and any *unbalanced* closing
/// bracket trimmed off, so a URL embedded in prose (`"...?si=1. cool!"`)
/// doesn't swallow the sentence's own closing punctuation. Every trimmed
/// byte is ASCII, so slicing at the resulting boundary can never split a
/// multi-byte character.
fn trim_trailing_punctuation(token: &str) -> usize {
    let bytes = token.as_bytes();
    let mut end = bytes.len();

    while end > 0 {
        let c = bytes[end - 1];
        let trim = matches!(c, b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'"')
            || (matches!(c, b')' | b']' | b'}') && !has_matching_open(&token[..end - 1], c));
        if !trim { break; }
        end -= 1;
    }
    end
}

/// Ingress-stage sanitizer for plain-text clipboard payloads: strips
/// privacy-invasive tracking query parameters from every URL embedded in
/// `data`, leaving everything else — prose, code, non-URL text — byte-for-
/// byte untouched.
///
/// Two-stage fast path so ordinary text (the overwhelming majority of
/// clipboard traffic) never pays for the UTF-8 decode + tokenize/rebuild
/// below: (1) no `"://"` anywhere means no URL at all; (2) a URL with no
/// `?` anywhere has no query string to strip. Both bail out with nothing
/// but the mandatory `Vec<u8>` copy the return type requires. Invalid
/// UTF-8 (rare for a `text/*` payload, but not impossible) also bails out
/// unsanitized rather than risk corrupting it via lossy decoding.
pub fn sanitize_text_payload(data: &[u8]) -> Vec<u8> {
    if find_subslice(data, b"://").is_none() || !data.contains(&b'?') {
        return data.to_vec();
    }

    let Ok(text) = std::str::from_utf8(data) else { return data.to_vec(); };

    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(scheme_pos) = find_url_start(rest) {
        out.push_str(&rest[..scheme_pos]);
        let candidate = &rest[scheme_pos..];

        let token_end = candidate.find(char::is_whitespace).unwrap_or(candidate.len());
        let token = &candidate[..token_end];
        let trim_end = trim_trailing_punctuation(token);

        out.push_str(&clean_url_tracking_params(&token[..trim_end]));
        out.push_str(&token[trim_end..]);

        rest = &candidate[token_end..];
    }
    out.push_str(rest);

    out.into_bytes()
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

    #[test]
    fn extract_image_url_double_quoted() {
        let html = br#"<img src="https://example.com/a.png" alt="cat">"#;
        assert_eq!(extract_image_url_from_html(html).unwrap(), "https://example.com/a.png");
    }

    #[test]
    fn extract_image_url_single_quoted() {
        let html = br#"<img src='https://example.com/a.png'>"#;
        assert_eq!(extract_image_url_from_html(html).unwrap(), "https://example.com/a.png");
    }

    #[test]
    fn extract_image_url_case_insensitive_tag_and_attr() {
        let html = br#"<IMG SRC="https://Example.com/A.png">"#;
        // Extraction is case-insensitive when *finding* `<img`/`src=`, but
        // the returned URL is sliced from the original bytes, so its own
        // casing must come through untouched.
        assert_eq!(extract_image_url_from_html(html).unwrap(), "https://Example.com/A.png");
    }

    #[test]
    fn extract_image_url_attribute_order_irrelevant() {
        let html = br#"<img alt="x" width="10" src="https://example.com/b.jpg" height="10">"#;
        assert_eq!(extract_image_url_from_html(html).unwrap(), "https://example.com/b.jpg");
    }

    #[test]
    fn extract_image_url_stops_at_tag_boundary() {
        // A `src=` belonging to a second, later tag must never be picked up
        // for the first <img> — the search is bounded by the first tag's
        // own closing '>'.
        let html = br#"<img alt="no src here"><a src="https://wrong.example/">text</a>"#;
        assert_eq!(extract_image_url_from_html(html), None);
    }

    #[test]
    fn extract_image_url_no_img_tag() {
        assert_eq!(extract_image_url_from_html(b"<p>no image here</p>"), None);
    }

    #[test]
    fn extract_image_url_missing_src() {
        assert_eq!(extract_image_url_from_html(br#"<img alt="no source">"#), None);
    }

    #[test]
    fn extract_image_url_unterminated_quote() {
        assert_eq!(extract_image_url_from_html(br#"<img src="https://example.com/a.png>"#), None);
    }

    #[test]
    fn extract_image_url_data_uri_passes_through_unfiltered() {
        // Extraction itself doesn't judge the scheme — that's
        // `is_valid_http_url`'s job (see the safety-net tests below).
        let html = br#"<img src="data:image/png;base64,AAAA">"#;
        assert_eq!(extract_image_url_from_html(html).unwrap(), "data:image/png;base64,AAAA");
    }

    #[test]
    fn is_valid_http_url_accepts_http_and_https() {
        assert!(is_valid_http_url("http://example.com/a.png"));
        assert!(is_valid_http_url("https://example.com/a.png"));
    }

    #[test]
    fn is_valid_http_url_rejects_non_network_schemes() {
        assert!(!is_valid_http_url("data:image/png;base64,AAAA"));
        assert!(!is_valid_http_url("file:///etc/passwd"));
        assert!(!is_valid_http_url("/relative/path.png"));
        assert!(!is_valid_http_url(""));
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

    #[test]
    fn sanitize_jpeg_appends_missing_eoi() {
        let truncated = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x01, 0x02];
        let fixed = sanitize_image_payload(truncated, "image/jpeg");
        assert!(fixed.ends_with(&[0xFF, 0xD9]));
    }

    #[test]
    fn sanitize_jpeg_leaves_intact_payload_unchanged() {
        let intact = vec![0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xD9];
        let result = sanitize_image_payload(intact.clone(), "image/jpeg");
        assert_eq!(result, intact);
    }

    #[test]
    fn sanitize_png_appends_missing_iend() {
        let truncated = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x01, 0x02];
        let fixed = sanitize_image_payload(truncated, "image/png");
        let iend: [u8; 12] = [0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82];
        assert!(fixed.ends_with(&iend));
    }

    #[test]
    fn sanitize_png_leaves_intact_payload_unchanged() {
        let mut intact = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        intact.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82]);
        let result = sanitize_image_payload(intact.clone(), "image/png");
        assert_eq!(result, intact);
    }

    #[test]
    fn sanitize_gif_appends_missing_trailer() {
        let truncated = vec![b'G', b'I', b'F', b'8', b'9', b'a', 0x01, 0x02];
        let fixed = sanitize_image_payload(truncated, "image/gif");
        assert_eq!(fixed.last(), Some(&0x3B));
    }

    #[test]
    fn sanitize_gif_leaves_intact_payload_unchanged() {
        let intact = vec![b'G', b'I', b'F', b'8', b'9', b'a', 0x3B];
        let result = sanitize_image_payload(intact.clone(), "image/gif");
        assert_eq!(result, intact);
    }

    #[test]
    fn sanitize_unknown_format_passes_through_untouched() {
        let webp = vec![b'R', b'I', b'F', b'F', 0, 0, 0, 0, b'W', b'E', b'B', b'P', 0xAB];
        let result = sanitize_image_payload(webp.clone(), "image/webp");
        assert_eq!(result, webp);
    }

    // --- is_text_mime ---

    #[test]
    fn is_text_mime_accepts_plain_text_alternatives() {
        assert!(is_text_mime("text/plain"));
        assert!(is_text_mime("text/plain;charset=utf-8"));
        assert!(is_text_mime("TEXT/Plain; charset=UTF-8"));
        assert!(is_text_mime("UTF8_STRING"));
        assert!(is_text_mime("STRING"));
        assert!(is_text_mime("TEXT"));
    }

    #[test]
    fn is_text_mime_rejects_non_text_alternatives() {
        assert!(!is_text_mime("text/html"));
        assert!(!is_text_mime("image/png"));
        assert!(!is_text_mime("application/json"));
        assert!(!is_text_mime(crate::core::constants::MIME_URI_LIST));
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

    // --- clean_url_tracking_params ---

    #[test]
    fn clean_url_no_query_returns_unchanged() {
        assert_eq!(clean_url_tracking_params("https://example.com/page"), "https://example.com/page");
    }

    #[test]
    fn clean_url_strips_utm_case_insensitive() {
        let url = "https://example.com/page?UTM_Source=news&UTM_Medium=email&foo=bar";
        assert_eq!(clean_url_tracking_params(url), "https://example.com/page?foo=bar");
    }

    #[test]
    fn clean_url_strips_all_universal_keys() {
        let url = "https://example.com/?fbclid=1&gclid=2&gbraid=3&wbraid=4&msclkid=5&igshid=6&mc_cid=7&mc_eid=8&si=9";
        assert_eq!(clean_url_tracking_params(url), "https://example.com/");
    }

    #[test]
    fn clean_url_strips_ref_prefix_universally() {
        let url = "https://example.com/?ref_src=twsrc&ref_url=abc&keep=1";
        assert_eq!(clean_url_tracking_params(url), "https://example.com/?keep=1");
    }

    #[test]
    fn clean_url_drops_question_mark_when_nothing_survives() {
        assert_eq!(clean_url_tracking_params("https://example.com/page?utm_source=x"), "https://example.com/page");
    }

    #[test]
    fn clean_url_preserves_fragment() {
        let url = "https://example.com/page?utm_campaign=abc&foo=bar#section";
        assert_eq!(clean_url_tracking_params(url), "https://example.com/page?foo=bar#section");
    }

    #[test]
    fn clean_url_preserves_fragment_when_query_fully_removed() {
        let url = "https://example.com/page?utm_source=x#top";
        assert_eq!(clean_url_tracking_params(url), "https://example.com/page#top");
    }

    #[test]
    fn clean_url_preserves_order_of_kept_params() {
        let url = "https://example.com/?a=1&utm_source=x&b=2&fbclid=y&c=3";
        assert_eq!(clean_url_tracking_params(url), "https://example.com/?a=1&b=2&c=3");
    }

    #[test]
    fn clean_url_x_and_twitter_strip_s_and_t() {
        let x = "https://x.com/user/status/123?s=20&t=abcXYZ";
        assert_eq!(clean_url_tracking_params(x), "https://x.com/user/status/123");
        let tw = "https://twitter.com/user/status/123?s=20&t=abcXYZ&lang=en";
        assert_eq!(clean_url_tracking_params(tw), "https://twitter.com/user/status/123?lang=en");
    }

    #[test]
    fn clean_url_s_and_t_not_stripped_off_x_twitter_domain() {
        let url = "https://example.com/page?s=20&t=abcXYZ";
        assert_eq!(clean_url_tracking_params(url), "https://example.com/page?s=20&t=abcXYZ");
    }

    #[test]
    fn clean_url_amazon_strips_bare_ref() {
        let url = "https://www.amazon.co.jp/dp/XXXX?ref=sr_1_1&ref_=sr_1_1&qid=123";
        assert_eq!(clean_url_tracking_params(url), "https://www.amazon.co.jp/dp/XXXX?qid=123");
    }

    #[test]
    fn clean_url_amazon_subdomain_matches() {
        let url = "https://smile.amazon.com/dp/XXXX?ref=abc&qid=1";
        assert_eq!(clean_url_tracking_params(url), "https://smile.amazon.com/dp/XXXX?qid=1");
    }

    #[test]
    fn clean_url_bare_ref_not_stripped_off_amazon_domain() {
        let url = "https://github.com/rust-lang/rust?ref=readme";
        assert_eq!(clean_url_tracking_params(url), "https://github.com/rust-lang/rust?ref=readme");
    }

    #[test]
    fn clean_url_youtube_t_param_protected() {
        let watch = "https://www.youtube.com/watch?v=abc123&t=42s";
        assert_eq!(clean_url_tracking_params(watch), "https://www.youtube.com/watch?v=abc123&t=42s");
        let short = "https://youtu.be/abc123?si=XYZ&t=10";
        assert_eq!(clean_url_tracking_params(short), "https://youtu.be/abc123?t=10");
    }

    #[test]
    fn clean_url_t_param_stripped_when_not_youtube() {
        // Sanity check that the YouTube exception is domain-gated, not a
        // blanket "never touch `t`" rule.
        let url = "https://x.com/user/status/123?t=abc";
        assert_eq!(clean_url_tracking_params(url), "https://x.com/user/status/123");
    }

    #[test]
    fn clean_url_host_lookalike_not_matched() {
        // "notyoutube.com" must not be treated as a youtube.com subdomain.
        let url = "https://notyoutube.com/watch?t=42s&utm_source=x";
        assert_eq!(clean_url_tracking_params(url), "https://notyoutube.com/watch?t=42s");
    }

    // --- sanitize_text_payload ---

    #[test]
    fn sanitize_text_payload_plain_prose_untouched() {
        let input = b"Just a normal sentence with no links at all.";
        assert_eq!(sanitize_text_payload(input), input);
    }

    #[test]
    fn sanitize_text_payload_code_snippet_untouched() {
        let input = b"fn main() { let x: Option<i32> = None; }";
        assert_eq!(sanitize_text_payload(input), input);
    }

    #[test]
    fn sanitize_text_payload_url_without_query_untouched() {
        let input = b"See https://example.com/docs for details.";
        assert_eq!(sanitize_text_payload(input), input);
    }

    #[test]
    fn sanitize_text_payload_single_url_strips_tracking() {
        let input = b"https://example.com/page?utm_source=x&foo=bar";
        let expected = b"https://example.com/page?foo=bar";
        assert_eq!(sanitize_text_payload(input), expected);
    }

    #[test]
    fn sanitize_text_payload_embedded_url_preserves_sentence_punctuation() {
        let input = b"Check: https://example.com/?si=123. cool!";
        let expected = b"Check: https://example.com/. cool!";
        assert_eq!(sanitize_text_payload(input), expected);
    }

    #[test]
    fn sanitize_text_payload_multiple_urls() {
        let input = b"first https://a.example/?utm_source=x second https://b.example/?ref_src=y done";
        let expected = b"first https://a.example/ second https://b.example/ done";
        assert_eq!(sanitize_text_payload(input), expected);
    }

    #[test]
    fn sanitize_text_payload_trailing_paren_in_prose_not_swallowed() {
        let input = b"link (https://example.com/?utm_source=x) end";
        let expected = b"link (https://example.com/) end";
        assert_eq!(sanitize_text_payload(input), expected);
    }

    #[test]
    fn sanitize_text_payload_balanced_paren_in_url_kept() {
        let input = b"https://en.wikipedia.org/wiki/Rust_(programming_language)?utm_source=x";
        let expected = b"https://en.wikipedia.org/wiki/Rust_(programming_language)";
        assert_eq!(sanitize_text_payload(input), expected);
    }

    #[test]
    fn sanitize_text_payload_invalid_utf8_bypassed_unsanitized() {
        let mut input = b"https://example.com/?".to_vec();
        input.extend_from_slice(&[0xFF, 0xFE]);
        let result = sanitize_text_payload(&input);
        assert_eq!(result, input);
    }

    #[test]
    fn sanitize_text_payload_empty_input() {
        assert_eq!(sanitize_text_payload(b""), b"");
    }
}
