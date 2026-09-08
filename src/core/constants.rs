// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/core/constants.rs

// --- System & Storage Configuration ---
pub const DB_DIR_NAME:  &str = "y4p";
pub const DB_FILE_NAME: &str = "y4p.sqlite";
pub const DEFAULT_MAX_HISTORY: usize = 256;
// Back-compat alias: kept so any external/out-of-scope reference to the old
// name still resolves to the same default.
#[allow(dead_code)]
pub const MAX_HISTORY: usize = DEFAULT_MAX_HISTORY;
// G-04: env var that overrides DEFAULT_MAX_HISTORY at runtime.
pub const ENV_MAX_HISTORY: &str = "Y4P_MAX_HISTORY";
pub const SQLITE_TIMEOUT_MS: u64 = 5000;

// --- IPC Protocol ---
pub const IPC_CMD_RESTORE: u8 = 0x01;
pub const IPC_CMD_EXIT:    u8 = 0x02;
pub const IPC_CMD_STATUS:  u8 = 0x03;
pub const IPC_CMD_PAUSE:   u8 = 0x04;
pub const IPC_CMD_RESUME:  u8 = 0x05;
pub const IPC_DELIMITER:   u8 = b'\n';

pub const RECONNECT_DELAY_MS: u64 = 500;
// Bounds the CLI's read of a `status` response so a stuck/misbehaving
// daemon can't hang the client indefinitely.
pub const IPC_STATUS_TIMEOUT_MS: u64 = 1000;

// Filename of the IPC control socket. Resolution of the *directory* it lives
// in (XDG_RUNTIME_DIR preferred, /tmp as a last-resort fallback) is handled
// by `crate::core::get_socket_path()`, since that decision depends on
// runtime environment, not just a fixed string.
pub const SOCKET_FILE_NAME: &str = "y4p.sock";

// --- Security & Privacy Configuration ---
// Clipboard security: MIME types to exclude from persistent storage
pub const SENSITIVE_MIME_HINTS: &[&str] = &[
    "x-kde-passwordManagerHint", 
    "password", 
    "secret",
    "x-gnome-cliptrace"
];

// --- Clipboard & Preview Settings ---
pub const DEFAULT_MIME: &str = "text/plain;charset=utf-8";
pub const PREVIEW_CHARS: usize = 100;
pub const TEXT_MIME_ALTS: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

pub const MIME_URI_LIST: &str = "text/uri-list";

// Ingress selection priority (highest first) when a compositor offers
// several MIME types for one selection — see device.rs's `mime_to_get`.
pub const MIME_PRIORITY_ORDER: &[&str] = &[
    // Lossless / high-fidelity images first.
    "image/png",
    "image/webp",
    "image/jpeg",
    "image/gif",
    "image/svg+xml",
    "image/avif",
    "image/bmp",
    // File lists.
    MIME_URI_LIST,
    // Rich text / structured markup.
    "text/html",
    "application/xhtml+xml",
    "text/rtf",
    "text/markdown",
    "application/json",
    "application/xml",
    // Plain text.
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

// Egress Broadcaster groups: MIMEs offered alongside the stored one so the
// paste target can pick whichever it understands — see
// daemon::handle_restore_request.
pub const HTML_MIME_ALTS: &[&str] = &[
    "text/html",
    "application/xhtml+xml",
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

pub const IMAGE_MIME_ALTS: &[&str] = &[
    "image/png",
    "image/webp",
    "image/jpeg",
    "image/gif",
];

// --- UI Layout & Formatting Settings ---
pub const WIDTH_ID: usize      = 6;
pub const WIDTH_WHEN: usize    = 8;
pub const WIDTH_SIZE: usize    = 11;
pub const PREVIEW_WIDTH: usize = 42;
pub const ELLIPSIS: &str       = "...";
pub const TABLE_SEP: &str       = " | ";
pub const TABLE_LINE_CHAR: &str = "-";

// --- UI Labels & Headers ---
pub const LABEL_IMAGE: &str = "[IMG]";
pub const LABEL_TEXT:  &str = "[TXT]";
pub const LABEL_DATA:  &str = "[BIN]";
pub const LABEL_FILE:  &str = "[FIL]";

pub const LIST_HEADER_ID: &str      = "ID";
pub const LIST_HEADER_WHEN: &str    = "WHEN";
pub const LIST_HEADER_SIZE: &str    = "SIZE";
pub const LIST_HEADER_CONTENT: &str = "CONTENT";

pub const TIME_UNIT_SEC:  &str = "s ago";
pub const TIME_UNIT_MIN:  &str = "m ago";
pub const TIME_UNIT_HOUR: &str = "h ago";

// --- Wayland Protocol Configuration ---
pub const INTERFACE_MANAGER: &str = "ext_data_control_manager_v1";
pub const INTERFACE_SEAT:    &str = "wl_seat";

// --- Logging & Notification Messages ---
pub const LOG_INFO:  &str = "info: ";
pub const LOG_WARN:  &str = "warn: ";
pub const LOG_ERROR: &str = "error: ";

pub const MSG_DAEMON_START: &str = "starting y4p daemon...";
// Emitted once the daemon has actually bound the Wayland data-control
// manager + seat and entered its serving loop. Distinct from
// MSG_DAEMON_START so a startup *failure* (bad socket bind, no compositor,
// missing protocol) is never misreported as "daemon started".
pub const MSG_DAEMON_READY: &str = "daemon operational; listening for clipboard and IPC events.";
pub const MSG_DAEMON_STOP:  &str = "daemon process terminated.";
pub const MSG_DAEMON_START_FAILED: &str = "daemon failed to start (see error above).";
pub const MSG_WAYLAND_CONN_FAIL: &str = "failed to connect to wayland compositor. is DISPLAY/WAYLAND_DISPLAY set?";
pub const MSG_MONITOR_PAUSED:  &str = "clipboard monitoring paused.";
pub const MSG_MONITOR_RESUMED: &str = "clipboard monitoring resumed.";

pub fn log_save(mime: &str, size: usize) -> String {
    format!("{}saved: {} ({} bytes)", LOG_INFO, mime, size)
}

pub fn log_restore(idx: usize) -> String {
    format!("{}restored ID [{}] to clipboard", LOG_INFO, idx)
}

pub fn log_seat_detected(name: &str, caps: &str) -> String {
    format!("{}wayland seat detected: {} (capabilities: {})", LOG_INFO, name, caps)
}

pub fn log_protocol_bound(interface: &str) -> String {
    format!("{}bound to wayland interface: {}", LOG_INFO, interface)
}
