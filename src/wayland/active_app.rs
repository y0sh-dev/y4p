// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/wayland/active_app.rs

//! Out-of-band active application detection via compositor IPC.
//!
//! Because `ext-data-control-v1` intentionally abstracts away client identity,
//! this module queries compositor control sockets (Hyprland IPC or Sway/i3-ipc)
//! to determine the currently focused window's App ID. The result is advisory
//! and kept only in memory during ingestion, never persisted to storage.
//!
//! Every code path here is a best-effort lookup, never a hard requirement:
//! an unsupported compositor, a socket that doesn't exist, a slow/hung
//! compositor, or a malformed reply all resolve to `None` rather than an
//! error. Callers only ever reach this from an already-spawned ingestion
//! thread (see device.rs), and every blocking I/O step is additionally
//! capped by its own 50ms socket timeout, so nothing here can ever stall
//! the daemon's main Wayland dispatch loop.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Read/write timeout applied to every compositor IPC socket. Both sides of
/// this connection are local-only (a Unix domain socket, never network
/// I/O), so a healthy compositor replies in well under a millisecond —
/// 50ms is purely a safety ceiling against a wedged/overloaded compositor,
/// not an expected steady-state latency.
const IPC_TIMEOUT: Duration = Duration::from_millis(50);

/// Ceiling on how much of a compositor's reply this will ever buffer, so a
/// compositor that (maliciously or by bug) never stops writing can't turn
/// this lookup into an unbounded memory allocation.
const MAX_RESPONSE_BYTES: usize = 1_048_576;

/// Best-effort identification of the app that owns the currently focused
/// window. Tries Hyprland's control socket first, then Sway/i3's `i3-ipc`
/// protocol; `None` if neither compositor's socket is reachable (including
/// every other compositor entirely, e.g. plain wlroots reference
/// compositors, or KWin/GNOME which don't implement either IPC).
pub fn detect_active_app() -> Option<String> {
    detect_hyprland().or_else(detect_sway)
}

/// Connects to a local IPC socket with the shared 50ms read/write timeout
/// already applied, so every caller inherits the same hang-proofing without
/// having to repeat it.
fn connect_with_timeout(path: &Path) -> Option<UnixStream> {
    let stream = UnixStream::connect(path).ok()?;
    stream.set_read_timeout(Some(IPC_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IPC_TIMEOUT)).ok()?;
    Some(stream)
}

/// Reads `stream` to EOF into `buf`, bounded by `MAX_RESPONSE_BYTES` and by
/// each individual `read()` call's own socket timeout. A timeout or any
/// other I/O error simply ends the read early — whatever was collected so
/// far (possibly nothing) is still handed back rather than treated as fatal,
/// since a truncated-but-still-parseable reply is common under load and the
/// downstream JSON scanning is already tolerant of malformed/partial input.
fn read_bounded(stream: &mut UnixStream, buf: &mut Vec<u8>) {
    let mut chunk = [0u8; 4096];
    while buf.len() < MAX_RESPONSE_BYTES {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
}

// --- Hyprland IPC ---

/// Builds `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`
/// from its two components directly (rather than reading the env vars
/// itself) so the path-building logic stays a pure, independently testable
/// function.
fn hyprland_socket_path(runtime_dir: &str, signature: &str) -> PathBuf {
    let mut path = PathBuf::from(runtime_dir);
    path.push("hypr");
    path.push(signature);
    path.push(".socket.sock");
    path
}

/// Hyprland's control socket is request/response but unframed: a client
/// writes a command string and the compositor writes back a plain-text (or,
/// for a `j/`-prefixed command like the one used here, JSON) reply with no
/// length prefix, then leaves its side of the connection to be closed —
/// reading to EOF is the correct/only way to know the reply is complete.
fn detect_hyprland() -> Option<String> {
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let mut stream = connect_with_timeout(&hyprland_socket_path(&runtime_dir, &signature))?;

    stream.write_all(b"j/activewindow").ok()?;

    let mut buf = Vec::new();
    read_bounded(&mut stream, &mut buf);

    let text = std::str::from_utf8(&buf).ok()?;
    find_json_string_value(text, "class")
        .or_else(|| find_json_string_value(text, "initialClass"))
        .filter(|s| !s.is_empty())
}

// --- Sway / i3 IPC ---

const SWAY_IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const SWAY_IPC_HEADER_LEN: usize = 14; // 6-byte magic + u32 length + u32 type
const SWAY_IPC_GET_TREE: u32 = 4;

/// Sway/i3's `i3-ipc` wire format: a fixed 14-byte header (`"i3-ipc"` magic,
/// then a payload length and message-type pair, each a `u32` in the host's
/// *native* byte order per the protocol's own spec — not portable across
/// architectures, but every realistic y4p target is little-endian) followed
/// by exactly that many bytes of JSON payload. `GET_TREE` (type 4) takes no
/// request payload.
fn detect_sway() -> Option<String> {
    let sock_path = std::env::var("SWAYSOCK").ok()?;
    let mut stream = connect_with_timeout(Path::new(&sock_path))?;

    let mut request = Vec::with_capacity(SWAY_IPC_HEADER_LEN);
    request.extend_from_slice(SWAY_IPC_MAGIC);
    request.extend_from_slice(&0u32.to_ne_bytes());
    request.extend_from_slice(&SWAY_IPC_GET_TREE.to_ne_bytes());
    stream.write_all(&request).ok()?;

    let mut header = [0u8; SWAY_IPC_HEADER_LEN];
    stream.read_exact(&mut header).ok()?;
    if &header[..6] != SWAY_IPC_MAGIC { return None; }

    let payload_len = u32::from_ne_bytes(header[6..10].try_into().ok()?) as usize;
    if payload_len == 0 || payload_len > MAX_RESPONSE_BYTES { return None; }

    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).ok()?;

    let text = std::str::from_utf8(&payload).ok()?;
    let node = focused_node_span(text)?;

    // Wayland-native clients report `app_id`; XWayland clients report
    // `app_id: null` and carry their identity in `window_properties.class`
    // instead (still textually inside this same node's span).
    find_json_string_value(node, "app_id")
        .filter(|s| !s.is_empty())
        .or_else(|| find_json_string_value(node, "class").filter(|s| !s.is_empty()))
}

/// Scans a Sway/i3 `GET_TREE` JSON reply for the node with `"focused":
/// true` and returns the byte span of that node's own JSON object — just
/// enough structural awareness (brace-depth tracking that correctly skips
/// over braces inside string values, e.g. a window title) to isolate the
/// right object without a full JSON parser. Sound because `"focused"` can
/// only ever appear as a direct key of the container node it describes —
/// its value is a bare boolean, never itself an object — so the smallest
/// `{...}` enclosing that key textually *is* that node.
fn focused_node_span(json: &str) -> Option<&str> {
    let bytes = json.as_bytes();
    let mut stack: Vec<usize> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    let mut target_start: Option<usize> = None;
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        match b {
            b'"' => {
                in_string = true;
                if target_start.is_none() && bytes[i..].starts_with(b"\"focused\"") {
                    let mut j = i + 9; // past the closing quote of "focused"
                    while bytes.get(j).is_some_and(u8::is_ascii_whitespace) { j += 1; }
                    if bytes.get(j) == Some(&b':') {
                        j += 1;
                        while bytes.get(j).is_some_and(u8::is_ascii_whitespace) { j += 1; }
                        if bytes[j..].starts_with(b"true") {
                            target_start = stack.last().copied();
                        }
                    }
                }
            }
            b'{' => stack.push(i),
            b'}' => {
                let start = stack.pop();
                if target_start.is_some() && start == target_start {
                    return start.map(|s| &json[s..=i]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() { return None; }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Minimal, non-recursive `"key": "value"` extractor shared by both
/// compositors' JSON replies. Not a JSON parser: it locates the literal key
/// text, skips whitespace and a `:`, and — only when the value is itself a
/// quoted string (an unquoted `null`/number/object value is treated as "not
/// present" rather than matched) — returns everything up to the next
/// unescaped `"`, without unescaping backslash sequences (an app ID/class
/// name containing an escaped character is vanishingly unlikely, and this
/// is advisory context, not data ever written back out as JSON).
fn find_json_string_value(json: &str, key: &str) -> Option<String> {
    let bytes = json.as_bytes();
    let mut needle = Vec::with_capacity(key.len() + 2);
    needle.push(b'"');
    needle.extend_from_slice(key.as_bytes());
    needle.push(b'"');

    let mut search_from = 0;
    while let Some(rel) = find_subslice(&bytes[search_from..], &needle) {
        let key_start = search_from + rel;
        let mut j = key_start + needle.len();
        while bytes.get(j).is_some_and(u8::is_ascii_whitespace) { j += 1; }

        if bytes.get(j) != Some(&b':') {
            search_from = key_start + needle.len();
            continue;
        }
        j += 1;
        while bytes.get(j).is_some_and(u8::is_ascii_whitespace) { j += 1; }

        if bytes.get(j) != Some(&b'"') {
            // Non-string value (null, number, nested object/array, ...) —
            // not a match for this extractor; keep scanning in case the
            // same key name recurs later in the document.
            search_from = key_start + needle.len();
            continue;
        }
        j += 1;
        let start = j;
        let mut escape = false;
        while let Some(&b) = bytes.get(j) {
            if escape { escape = false; }
            else if b == b'\\' { escape = true; }
            else if b == b'"' { break; }
            j += 1;
        }

        return (j < bytes.len())
            .then(|| std::str::from_utf8(&bytes[start..j]).ok())
            .flatten()
            .map(str::to_string);
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // --- hyprland_socket_path ---

    #[test]
    fn hyprland_socket_path_joins_components() {
        let path = hyprland_socket_path("/run/user/1000", "abcd1234_0");
        assert_eq!(path, PathBuf::from("/run/user/1000/hypr/abcd1234_0/.socket.sock"));
    }

    // --- find_json_string_value ---

    #[test]
    fn find_json_string_value_basic() {
        let json = r#"{"class":"kitty","title":"~"}"#;
        assert_eq!(find_json_string_value(json, "class").as_deref(), Some("kitty"));
    }

    #[test]
    fn find_json_string_value_missing_key_returns_none() {
        let json = r#"{"class":"kitty"}"#;
        assert_eq!(find_json_string_value(json, "initialClass"), None);
    }

    #[test]
    fn find_json_string_value_null_value_is_not_a_match() {
        let json = r#"{"app_id":null,"window_properties":{"class":"firefox"}}"#;
        assert_eq!(find_json_string_value(json, "app_id"), None);
        assert_eq!(find_json_string_value(json, "class").as_deref(), Some("firefox"));
    }

    #[test]
    fn find_json_string_value_tolerates_whitespace() {
        let json = "{ \"class\" : \"kitty\" }";
        assert_eq!(find_json_string_value(json, "class").as_deref(), Some("kitty"));
    }

    #[test]
    fn find_json_string_value_stops_at_unescaped_quote_and_skips_escapes() {
        let json = r#"{"title":"say \"hi\"","class":"kitty"}"#;
        assert_eq!(find_json_string_value(json, "title").as_deref(), Some(r#"say \"hi\""#));
        assert_eq!(find_json_string_value(json, "class").as_deref(), Some("kitty"));
    }

    #[test]
    fn find_json_string_value_empty_json_returns_none() {
        assert_eq!(find_json_string_value("{}", "class"), None);
    }

    #[test]
    fn find_json_string_value_unterminated_string_returns_none() {
        let json = r#"{"class":"kitty"#;
        assert_eq!(find_json_string_value(json, "class"), None);
    }

    // --- focused_node_span ---

    #[test]
    fn focused_node_span_finds_wayland_native_leaf() {
        let json = r#"{"type":"root","nodes":[{"type":"output","nodes":[{"type":"con","focused":false,"app_id":"firefox"},{"type":"con","focused":true,"app_id":"kitty"}]}]}"#;
        let node = focused_node_span(json).expect("focused node should be found");
        assert_eq!(find_json_string_value(node, "app_id").as_deref(), Some("kitty"));
    }

    #[test]
    fn focused_node_span_finds_xwayland_leaf_via_window_properties() {
        let json = r#"{"nodes":[{"focused":true,"app_id":null,"window_properties":{"class":"Firefox","instance":"Navigator"}}]}"#;
        let node = focused_node_span(json).expect("focused node should be found");
        assert_eq!(find_json_string_value(node, "app_id"), None);
        assert_eq!(find_json_string_value(node, "class").as_deref(), Some("Firefox"));
    }

    #[test]
    fn focused_node_span_searches_floating_nodes_too() {
        let json = r#"{"nodes":[{"focused":false,"app_id":"waybar"}],"floating_nodes":[{"focused":true,"app_id":"pavucontrol"}]}"#;
        let node = focused_node_span(json).expect("focused node should be found");
        assert_eq!(find_json_string_value(node, "app_id").as_deref(), Some("pavucontrol"));
    }

    #[test]
    fn focused_node_span_no_focused_node_returns_none() {
        let json = r#"{"nodes":[{"focused":false,"app_id":"waybar"}]}"#;
        assert_eq!(focused_node_span(json), None);
    }

    #[test]
    fn focused_node_span_empty_tree_returns_none() {
        assert_eq!(focused_node_span("{}"), None);
    }

    #[test]
    fn focused_node_span_ignores_braces_inside_string_values() {
        // A window title containing literal '{'/'}' must not desynchronize
        // the brace-depth tracker.
        let json = r#"{"nodes":[{"focused":true,"app_id":"kitty","name":"curly {braces} in title"}]}"#;
        let node = focused_node_span(json).expect("focused node should be found");
        assert_eq!(find_json_string_value(node, "app_id").as_deref(), Some("kitty"));
    }

    #[test]
    fn focused_node_span_does_not_match_focused_false() {
        let json = r#"{"nodes":[{"focused":false,"app_id":"only-one-node"}]}"#;
        assert_eq!(focused_node_span(json), None);
    }
}
