// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/core/src/constants.rs

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
// by `crate::get_socket_path()`, since that decision depends on
// runtime environment, not just a fixed string.
pub const SOCKET_FILE_NAME: &str = "y4p.sock";

// --- Wayland Ingestion I/O ---
// Upper bound on a single clipboard payload read from a compositor pipe
// (`core::payload::read_bounded`). Enforced by reading one byte beyond
// this cap: a stream that is actually larger is thereby told apart from one
// that exactly fits, and discarded outright rather than silently truncated
// and persisted as corrupt data.
pub const MAX_PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;

// --- Payload Routing ---
// Boundary between the in-memory fast path and the kernel-streamed descriptor
// path (see `core::payload::route_payload`). One OS page: below it a payload
// costs less to copy through the heap than to set a descriptor transfer up
// for, and clipboard text is overwhelmingly this small.
pub const INLINE_PAYLOAD_THRESHOLD_BYTES: usize = 4096;
// Ceiling on what a length hint may pre-allocate in `core::payload::read_bounded`.
// A hint is advisory (a peer may lie), so it never reserves more than this
// up front; real data beyond it still arrives, through ordinary growth.
pub const PAYLOAD_PREALLOC_CAP_BYTES: usize = 16 * 1024 * 1024;

// --- Kernel Streaming (splice/tee) ---
// Bytes moved per `splice(2)`/`tee(2)` round, and the size of the fixed stack
// buffer the user-space fallback and the fingerprint tap read through. It is
// the default capacity of a Linux pipe (16 pages): the unit the fallback copy
// works in. The kernel path moves up to `SPLICE_ROUND_BYTES` per round.
pub const SPLICE_CHUNK_BYTES: usize = 64 * 1024;

// --- Pipe Capacity Tuning (core::pipe) ---
// What the kernel gives a new pipe (16 pages). Below it, tuning is a no-op.
pub const PIPE_DEFAULT_BYTES: usize = 64 * 1024;
// Capacity requested with `fcntl(F_SETPIPE_SZ)`: the unprivileged ceiling
// (`/proc/sys/fs/pipe-max-size`) on a stock kernel. Sixteen times the default
// means a writer blocks, and a context switch is paid, one sixteenth as often.
// A refusal is never an error: `core::pipe::grow` steps down towards what the
// system allows.
pub const PIPE_TARGET_BYTES: usize = 1024 * 1024;
// Bytes `tee(2)`/`splice(2)` move per round in `core::splice`. It follows the
// pipe target: on a grown pipe one round empties a full pipe; on a pipe that
// could not be grown a round simply moves what is there.
pub const SPLICE_ROUND_BYTES: usize = PIPE_TARGET_BYTES;

// --- Content-Defined Chunking (core::cdc) ---
// Bounds of a FastCDC chunk. The minimum is one SQLite page, so no chunk is
// smaller than the unit the database stores anyway; the maximum is the Linux
// pipe capacity (`SPLICE_CHUNK_BYTES`), so one chunk always moves in a single
// splice round; the average sits between them and is the point where the
// boundary test relaxes (normalised chunking).
pub const CDC_MIN_CHUNK_BYTES: usize = 4 * 1024;
pub const CDC_AVG_CHUNK_BYTES: usize = 16 * 1024;
pub const CDC_MAX_CHUNK_BYTES: usize = 64 * 1024;
// Directory (inside the cache directory, hence on the same filesystem as the
// final `<hash>.cache` files) where a payload is spooled while it streams in.
// Keeping it on one filesystem is what lets the finished file be adopted into
// the cache with an atomic `rename(2)` instead of a copy.
pub const SPOOL_DIR_NAME: &str = "spool";
// Extension of a spool file. A leftover with this extension is, by definition,
// debris of a previous run and is swept at daemon start.
pub const SPOOL_FILE_EXTENSION: &str = "part";
// Extension of a cached binary payload, `<hash>.<extension>`: the one rule
// `crates/storage` (`FileCache::path`) names cache files by.
pub const CACHE_FILE_EXTENSION: &str = "cache";
// Width of the payload fingerprint (SHA3-256) in bytes.
pub const DIGEST_LEN: usize = 32;

// --- Ingestion Backpressure & Concurrency Bounds ---
// Capacity of the bounded channel between the Wayland ingestion threads and
// the database worker. A count, not a byte budget: the worst-case resident
// payload memory is (INGEST_QUEUE_CAPACITY + INGEST_MAX_CONCURRENT_READERS)
// * MAX_PAYLOAD_BYTES, reached only if every slot holds a maximal *in-memory*
// payload (text, or anything while no spool directory is usable). A payload
// that streams in past INLINE_PAYLOAD_THRESHOLD_BYTES is spooled to disk and
// queued as a path and a digest: it costs a spool file, not heap, so the same
// bound then applies to disk space, and the heap stays flat.
pub const INGEST_QUEUE_CAPACITY: usize = 64;
// Simultaneous ingestion readers (one thread, one pipe read end and one
// payload buffer each). Checked *before* the pipe is allocated, so a burst of
// selection changes can neither exhaust file descriptors (`EMFILE`) nor spawn
// an unbounded number of threads.
pub const INGEST_MAX_CONCURRENT_READERS: usize = 8;
// How long a reader waits for room in a full queue before its payload is shed.
// The worker may be busy with a slow image conversion (bounded by
// IMAGE_TRANSCODE_TIMEOUT_SECS), so this is a patience budget, not a deadline
// for the worker.
pub const INGEST_ENQUEUE_WAIT_MS: u64 = 2000;
// Polling interval while waiting for queue room (`std` offers no timed send).
pub const INGEST_ENQUEUE_POLL_MS: u64 = 5;
// A zero-capacity `sync_channel` is a rendezvous channel: `try_send` succeeds
// only while the worker is already blocked in `recv`, so nearly every payload
// would be shed. A zero reader limit would admit nobody. Both are build errors.
const _: () = assert!(
    INGEST_QUEUE_CAPACITY >= 1 && INGEST_MAX_CONCURRENT_READERS >= 1,
    "the ingestion bounds must admit at least one reader and one queued payload",
);

// --- Local Image Pipeline ---
// External converter executable, resolved through `PATH` (ImageMagick 7).
pub const MAGICK_PROGRAM: &str = "magick";
// Private scratch space for the converter's disk spill (`-limit disk`). A
// per-user directory under `$XDG_RUNTIME_DIR` when that is a private (0700)
// directory of the current user, otherwise `<SCRATCH_FALLBACK_ROOT>/
// <SCRATCH_DIR_NAME>-<uid>`; either way created with mode 0700 and verified
// before use. Each conversion gets its own subdirectory, removed afterwards.
pub const SCRATCH_DIR_NAME: &str = "y4p-magick";
pub const SCRATCH_FALLBACK_ROOT: &str = "/tmp";
// Chunk size for relaying bytes between caller streams and a child's pipes.
// Matches the default Linux pipe capacity (64KiB) so one read/write pair
// moves at most one full pipe buffer.
pub const PIPELINE_CHUNK_BYTES: usize = 64 * 1024;
// Upper bound on the child's stderr text retained for error reporting. The
// stream is always drained to EOF regardless, so a chatty child can never
// block on a full stderr pipe; only the retained excerpt is capped.
pub const PIPELINE_STDERR_CAP_BYTES: usize = 8 * 1024;

// Resource ceilings handed to ImageMagick (`-limit`) when it decodes an
// untrusted clipboard image, so a small file that expands enormously (a
// decompression bomb) is rejected or spilled within bounds instead of
// exhausting memory. Width/height are pixels; 16384 comfortably covers an
// 8K screenshot (7680x4320). Memory/map/disk are ImageMagick size strings;
// time is seconds before the child aborts itself. It is the inner layer of a
// two-layer defence and must stay below `IMAGE_TRANSCODE_TIMEOUT_SECS` (a
// compile-time assertion in `image::transcode` enforces it): a runaway
// conversion should end with ImageMagick's own diagnostic, and only a child
// that ignores its limit (blocked, deadlocked) meets the outer SIGKILL.
pub const IMAGE_LIMIT_WIDTH_PX: &str = "16384";
pub const IMAGE_LIMIT_HEIGHT_PX: &str = "16384";
pub const IMAGE_LIMIT_MEMORY: &str = "256MiB";
pub const IMAGE_LIMIT_MAP: &str = "512MiB";
pub const IMAGE_LIMIT_DISK: &str = "1GiB";
pub const IMAGE_LIMIT_TIME_SECS: &str = "10";
// Encoder quality bounds (inclusive) and the default used when a caller has
// no preference. Out-of-range requests are clamped, never rejected.
pub const IMAGE_QUALITY_MIN: u8 = 1;
pub const IMAGE_QUALITY_MAX: u8 = 100;
pub const IMAGE_QUALITY_DEFAULT: u8 = 80;

// Wall-clock budget for one image transcode. Unlike `IMAGE_LIMIT_TIME_SECS`
// (ImageMagick's own limit, which a blocked or deadlocked process never
// reaches, and which only fires at points where ImageMagick checks it), this
// is enforced from outside: the child's whole process group is SIGKILLed when
// it elapses.
pub const IMAGE_TRANSCODE_TIMEOUT_SECS: u64 = 15;
// Circuit breaker around image conversion: this many consecutive failures
// suspend conversion (images are stored unmodified, no process is spawned)
// for the cooldown, after which a single canary conversion decides whether
// to resume.
pub const IMAGE_BREAKER_THRESHOLD: u32 = 3;
pub const IMAGE_BREAKER_COOLDOWN_SECS: u64 = 30;
// Process throttle around image conversion: at most this many converter
// processes run at once, so a burst of copied images cannot fork-bomb the
// machine. A request that finds every slot busy waits up to the wait budget
// and then stores the image unmodified; saturation is load, not a converter
// fault, so it never counts towards the circuit breaker.
pub const IMAGE_CONCURRENCY_LIMIT: usize = 2;
pub const IMAGE_THROTTLE_WAIT_MS: u64 = 1000;
// Upper bound on the output buffer reserved up front for one conversion.
// Every ingest conversion re-encodes to lossless PNG. The input size is a fair
// estimate of the output for PNG and BMP sources; a JPEG or WebP source grows
// several times, which is harmless (the buffer then grows as usual). The cap
// only stops a huge input from reserving address space up front.
pub const IMAGE_OUTPUT_HINT_CAP_BYTES: usize = 16 * 1024 * 1024;

// --- Security & Privacy Configuration ---
// Clipboard security: MIME types to exclude from persistent storage
pub const SENSITIVE_MIME_HINTS: &[&str] = &[
    "x-kde-passwordManagerHint", 
    "password", 
    "secret",
    "x-gnome-cliptrace",
    // Password managers and concealed markers
    "keepass",
    "1password",
    "bitwarden",
    "concealed",
];

// --- Clipboard & Preview Settings ---
pub const DEFAULT_MIME: &str = "text/plain;charset=utf-8";
pub const PREVIEW_CHARS: usize = 100;

// Substrings that mark a MIME as "text-like" for storage and search purposes.
// Consumed by `core::utils::is_text_like_mime` and `storage::db::TEXT_MIME_SQL_PREDICATE`.
pub const TEXT_LIKE_MIME_HINTS: &[&str] = &["text", "json", "xml", "xhtml", "utf8", "string", "uri-list"];

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
//
// Plain text ranks above rich markup: Electron/Chromium apps (Discord,
// Slack, ...) routinely announce text/html alongside text/plain even for a
// plain-text selection, but only serialise actual HTML on request — a
// non-rich selection gets an empty payload for text/html, which used to
// make ingestion discard the whole clipboard event (see device.rs's
// `if payload.is_empty()` check). Plain text is present whenever anything
// is, so trying it first make that failure mode structurally impossible;
// text/html is still reachable as a fallback when no plain-text alternative
// was offered at all (e.g. a deliberate rich-text/source copy).
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
    // Standard plain text.
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
    // Rich text / structured markup — fallback only.
    //
    // "text/rtf" deliberately has no entry here. RTF contains control words
    // unsafe to persist as plain text and is dropped or handled separately.
    "text/html",
    "application/xhtml+xml",
    "text/markdown",
    "application/json",
    "application/xml",
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
// Shown once per process when an image arrives but `magick` is unusable:
// the image is still saved, but unmodified, i.e. with its metadata intact.
pub const MSG_IMAGE_TOOL_MISSING: &str = "`magick` (ImageMagick) is unavailable; images are saved unmodified, metadata included.";

pub fn log_image_breaker_open(failures: u32, cooldown_secs: u64) -> String {
    format!("{}image conversion failed {} times in a row; suspended for {}s (images are saved unmodified)", LOG_WARN, failures, cooldown_secs)
}

pub fn log_image_breaker_closed() -> String {
    format!("{}image conversion recovered; resumed", LOG_INFO)
}

pub fn log_image_scratch_unavailable(reason: &str) -> String {
    format!("{}no private scratch directory for the image converter ({}); images are saved unmodified, metadata included", LOG_WARN, reason)
}

pub fn log_image_throttled(wait_ms: u64) -> String {
    format!("{}image converter saturated for {}ms; image saved unmodified", LOG_WARN, wait_ms)
}

/// Logged for the 1st, 2nd, 4th, 8th, ... shed, so a sustained overload does
/// not flood the log while the first occurrence is never silent.
pub fn log_ingest_shed(reason: &str, total: u64) -> String {
    format!("{}ingestion overloaded ({}); clipboard payload dropped (total shed: {})", LOG_WARN, reason, total)
}

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
