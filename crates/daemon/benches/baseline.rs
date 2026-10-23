// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/daemon/benches/baseline.rs

//! Quantitative baseline for the ingestion path, built on `std` alone
//! (`harness = false`; no benchmarking crate is added to the workspace).
//!
//! ```text
//! cargo bench -p y4p-daemon --bench baseline      # full measurement
//! cargo test  --workspace --all-targets           # smoke mode: one pass, no timing claims
//! ```
//!
//! What is measured, and why these eleven groups:
//!
//! | Group     | Subject                                                     |
//! | :-------- | :---------------------------------------------------------- |
//! | sha3      | digest and hex fingerprint over 64 B / 4 KiB / 64 KiB / 1 MiB |
//! | mime      | MIME comparison, magic-byte sniffing, `text/uri-list` parsing |
//! | queue     | bounded-channel hand-off, load shedding, reader gate, memory |
//! | payload   | routing decision, reader dispatch, bounded reads, pipe streams |
//! | splice    | pipe -> file: heap `Vec` road versus `splice(2)`/`tee(2)` and the fallback |
//! | ingest    | pipe -> database end to end, in memory versus spooled; memory bound |
//! | transcode | raster image: spool file -> converted bytes, heap road versus descriptor to descriptor |
//! | sqlite    | insert, insert + rotation, duplicate hit, metadata scans      |
//! | cdc       | FastCDC chunking throughput and allocations, 4 KiB - 16 MiB; chunking + SHA3; edit deduplication |
//! | pipe      | pipe capacity 64 KiB versus 1 MiB (`F_SETPIPE_SZ`): read loop, splice, splice + tee + sha3, sendfile egress; with context switches |
//! | chunked   | chunked payloads in SQLite: fresh insert, edited-copy insert (deduplicated), reassembly, deep search |
//!
//! Besides time, every row reports allocation calls and bytes per operation
//! (a counting global allocator wraps `System`), the quantity the zero-copy
//! milestones (v0.7.2 - v0.8.0) are meant to reduce. The `pipe` group also
//! reports context switches per operation (`getrusage(2)`, voluntary plus
//! involuntary, all threads), the cost that a larger pipe is meant to cut.
//!
//! **Safety of the environment.** The SQLite group opens the real storage
//! layer, which resolves its paths from `XDG_*`. The harness therefore never
//! runs in the caller's environment: on start it re-executes itself with
//! `XDG_*` pointing into a fresh private directory and deletes that directory
//! afterwards, and the child refuses to run unless the database path really
//! lies inside it. The user's clipboard history cannot be read or modified.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashSet;
use std::cell::Cell;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read, Write as _};
use std::hint::black_box;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicUsize, AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha3::{Digest, Sha3_256};
use y4p_core as core;
use y4p_core::payload::{read_bounded, route_payload, to_hex, Payload, PayloadReader, SpooledFile, StreamDescriptor};
use y4p_core::cdc::FastCdc;
use y4p_core::pipe::grow_to;
use y4p_core::splice::{copy_to_file, copy_to_file_buffered, ByteTap, NoTap};
use y4p_daemon::metrics::DaemonMetrics;
use y4p_daemon::spool::{transcode_spooled, FileSpooler};
use y4p_daemon::worker::DbWorker;
use y4p_image::transcode::{self, InputFormat, OutputFormat, Transcode};
use y4p_storage::ClipboardDb;
use y4p_wayland::spool::{PayloadSpooler, SpoolOutcome};
use y4p_wayland::ingest::{self, ByteBudget, Enqueue, ReaderGate};
use y4p_wayland::state::ClipboardJob;

// ---------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------

struct Counting;

static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

fn note_alloc(size: usize) {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    ALLOC_BYTES.fetch_add(size as u64, Ordering::Relaxed);
    let live = LIVE_BYTES.fetch_add(size, Ordering::Relaxed) + size;
    PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
}

fn note_free(size: usize) {
    LIVE_BYTES.fetch_sub(size, Ordering::Relaxed);
}

// SAFETY: every method forwards its arguments unchanged to `System`, which
// upholds the `GlobalAlloc` contract; the bookkeeping only touches atomics and
// never allocates, so it cannot recurse into the allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` comes straight from the caller, who must uphold `alloc`'s contract.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            note_alloc(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as in `alloc`.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            note_alloc(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note_free(layout.size());
        // SAFETY: `ptr` and `layout` are the pair the caller received from this allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `ptr`/`layout` were obtained from this allocator and `new_size` is the caller's.
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            note_free(layout.size());
            note_alloc(new_size);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn alloc_snapshot() -> (u64, u64) {
    (ALLOC_CALLS.load(Ordering::Relaxed), ALLOC_BYTES.load(Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// Measurement core
// ---------------------------------------------------------------------------

/// A pass takes at least this long during calibration, so timer resolution and
/// loop overhead stay negligible against the work being timed.
const TARGET_PASS: Duration = Duration::from_millis(40);
const ROUNDS: usize = 9;

struct Mode {
    /// `false` under `cargo test`: every case runs exactly once, nothing is timed.
    full: bool,
}

struct Row {
    group: &'static str,
    name: String,
    /// Median over rounds, nanoseconds per operation.
    median_ns: f64,
    min_ns: f64,
    /// Payload bytes per operation, for the MiB/s column (0 = not applicable).
    payload_bytes: u64,
    allocs_per_op: f64,
    alloc_bytes_per_op: f64,
    /// Context switches per operation, for the groups that measure them.
    switches_per_op: Option<f64>,
}

impl Row {
    fn mib_per_s(&self) -> Option<f64> {
        (self.payload_bytes > 0 && self.median_ns > 0.0)
            .then(|| self.payload_bytes as f64 / (1024.0 * 1024.0) / (self.median_ns / 1e9))
    }
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|a, b| a.total_cmp(b));
    samples.get(samples.len() / 2).copied().unwrap_or(0.0)
}

fn minimum(samples: &[f64]) -> f64 {
    samples.iter().copied().fold(f64::INFINITY, f64::min)
}

impl Mode {
    /// Times `f` as a tight loop. Allocation counts come from one extra
    /// isolated call, so they describe a single operation exactly.
    fn micro<R>(&self, group: &'static str, name: impl Into<String>, payload_bytes: u64, mut f: impl FnMut() -> R) -> Row {
        black_box(f());
        let (calls0, bytes0) = alloc_snapshot();
        black_box(f());
        let (calls1, bytes1) = alloc_snapshot();
        let allocs = (calls1 - calls0) as f64;
        let alloc_bytes = (bytes1 - bytes0) as f64;

        if !self.full {
            return Row { group, name: name.into(), median_ns: 0.0, min_ns: 0.0, payload_bytes, allocs_per_op: allocs, alloc_bytes_per_op: alloc_bytes, switches_per_op: None };
        }

        let mut pass = |iters: u64| -> Duration {
            let started = Instant::now();
            for _ in 0..iters {
                black_box(f());
            }
            started.elapsed()
        };
        let mut iters = 1u64;
        while pass(iters) < TARGET_PASS && iters < (1 << 28) {
            iters *= 2;
        }
        let mut samples: Vec<f64> = (0..ROUNDS).map(|_| pass(iters).as_nanos() as f64 / iters as f64).collect();
        let min_ns = minimum(&samples);
        Row { group, name: name.into(), median_ns: median(&mut samples), min_ns, payload_bytes, allocs_per_op: allocs, alloc_bytes_per_op: alloc_bytes, switches_per_op: None }
    }

    /// Times a stateful batch of `ops` operations per round on a shared
    /// context. `setup` is not timed and runs before *every* round, so each
    /// round starts from the same state; `body` is timed.
    #[allow(clippy::too_many_arguments)]
    fn batched<C>(
        &self,
        group: &'static str,
        name: impl Into<String>,
        ops: u64,
        payload_bytes: u64,
        ctx: &mut C,
        mut setup: impl FnMut(&mut C) -> Result<(), String>,
        mut body: impl FnMut(&mut C) -> Result<(), String>,
    ) -> Result<Row, String> {
        let rounds = if self.full { ROUNDS } else { 1 };
        let ops = if self.full { ops } else { 1 };
        let mut samples = Vec::with_capacity(rounds);
        let mut allocs = 0.0;
        let mut alloc_bytes = 0.0;
        for round in 0..rounds {
            setup(ctx)?;
            let (calls0, bytes0) = alloc_snapshot();
            let started = Instant::now();
            for _ in 0..ops {
                body(ctx)?;
            }
            let elapsed = started.elapsed();
            let (calls1, bytes1) = alloc_snapshot();
            samples.push(elapsed.as_nanos() as f64 / ops as f64);
            if round == 0 {
                allocs = (calls1 - calls0) as f64 / ops as f64;
                alloc_bytes = (bytes1 - bytes0) as f64 / ops as f64;
            }
        }
        let min_ns = minimum(&samples);
        let median_ns = if self.full { median(&mut samples) } else { 0.0 };
        Ok(Row { group, name: name.into(), median_ns, min_ns: if self.full { min_ns } else { 0.0 }, payload_bytes, allocs_per_op: allocs, alloc_bytes_per_op: alloc_bytes, switches_per_op: None })
    }
}

fn print_header() {
    println!("| group | case | median ns/op | min ns/op | MiB/s | allocs/op | alloc B/op | ctx sw/op |");
    println!("| :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |");
}

fn print_row(row: &Row) {
    let rate = row.mib_per_s().map_or_else(|| "-".to_owned(), |v| format!("{v:.0}"));
    let switches = row.switches_per_op.map_or_else(|| "-".to_owned(), |v| format!("{v:.1}"));
    println!(
        "| {} | {} | {:.1} | {:.1} | {} | {:.1} | {:.0} | {} |",
        row.group, row.name, row.median_ns, row.min_ns, rate, row.allocs_per_op, row.alloc_bytes_per_op, switches
    );
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Deterministic, incompressible-enough filler (xorshift) so no case benefits
/// from a zero-filled buffer.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// The worker's fingerprint (`crates/daemon/src/worker.rs`): lowercase-hex SHA3-256.
fn fingerprint(data: &[u8]) -> String {
    let mut hasher = Sha3_256::new();
    hasher.update(data);
    to_hex(hasher.finalize().as_slice())
}

/// The encoder the worker used up to v0.7.2 (one `format!` per byte), kept so the
/// improvement is measured in the same run rather than against a stored number.
fn fingerprint_formatted(data: &[u8]) -> String {
    let mut hasher = Sha3_256::new();
    hasher.update(data);
    hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>()
}

fn size_label(len: usize) -> String {
    match len {
        n if n >= 1024 * 1024 => format!("{} MiB", n / (1024 * 1024)),
        n if n >= 1024 => format!("{} KiB", n / 1024),
        n => format!("{n} B"),
    }
}

fn job(len: usize) -> ClipboardJob {
    ClipboardJob::new("text/plain".to_owned(), vec![0x61; len])
}

fn uri_list(paths: usize) -> Vec<u8> {
    let mut out = String::new();
    for i in 0..paths {
        out.push_str(&format!("file:///home/user/My%20Documents/report%20{i:05}.txt\r\n"));
    }
    out.into_bytes()
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

fn bench_sha3(mode: &Mode, rows: &mut Vec<Row>) {
    for len in [64usize, 4 * 1024, 64 * 1024, 1024 * 1024] {
        let data = payload(len, 0x5eed);
        let label = size_label(len);
        rows.push(mode.micro("sha3", format!("digest only, {label}"), len as u64, || {
            let mut hasher = Sha3_256::new();
            hasher.update(black_box(&data));
            hasher.finalize()
        }));
        rows.push(mode.micro("sha3", format!("fingerprint (hex), {label}"), len as u64, || fingerprint(black_box(&data))));
        if len == 64 {
            rows.push(mode.micro("sha3", format!("fingerprint (hex, format! per byte, to v0.7.2), {label}"), len as u64, || {
                fingerprint_formatted(black_box(&data))
            }));
        }
    }
}

fn bench_mime(mode: &Mode, rows: &mut Vec<Row>) {
    use core::utils::{detect_image_mime, is_html_mime, is_text_like_mime, mime_base_eq, normalize_uri_list};

    rows.push(mode.micro("mime", "mime_base_eq (parameters + case)", 0, || {
        mime_base_eq(black_box("Text/Plain; charset=UTF-8"), black_box("text/plain"))
    }));
    rows.push(mode.micro("mime", "is_text_like_mime", 0, || is_text_like_mime(black_box("application/xhtml+xml"))));
    rows.push(mode.micro("mime", "is_html_mime", 0, || is_html_mime(black_box("text/html; charset=utf-8"))));

    let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D];
    let text = payload(512, 7);
    rows.push(mode.micro("mime", "detect_image_mime (PNG header)", 0, || detect_image_mime(black_box(&png))));
    rows.push(mode.micro("mime", "detect_image_mime (non-image, 512 B)", 0, || detect_image_mime(black_box(&text))));

    for paths in [1usize, 100, 1000] {
        let data = uri_list(paths);
        rows.push(mode.micro(
            "mime",
            format!("normalize_uri_list, {paths} path(s), {}", size_label(data.len())),
            data.len() as u64,
            || normalize_uri_list(black_box(&data)),
        ));
    }
}

fn bench_payload(mode: &Mode, rows: &mut Vec<Row>) {
    let limit = core::constants::MAX_PAYLOAD_BYTES;
    let inline = core::constants::INLINE_PAYLOAD_THRESHOLD_BYTES as u64;

    // The routing decision itself: a comparison with a hint, one MIME scan without.
    rows.push(mode.micro("payload", "route_payload (length hint)", 0, || route_payload(black_box("image/png"), black_box(Some(10_000)))));
    rows.push(mode.micro("payload", "route_payload (no hint, text-like)", 0, || route_payload(black_box("text/plain"), black_box(None))));
    rows.push(mode.micro("payload", "route_payload (no hint, binary)", 0, || route_payload(black_box("image/png"), black_box(None))));

    // Reader dispatch: static `enum` versus the `Box<dyn Read>` it replaces. Both
    // build a reader and read 4 KiB; the buffer moves in and out, so only the
    // dispatch mechanism allocates (or not).
    let data = payload(4096, 3);
    let mut scratch = vec![0u8; 4096];
    let mut owned = data.clone();
    rows.push(mode.micro("payload", "PayloadReader::Memory build + read, 4 KiB (enum)", 4096, || {
        let mut reader = Payload::Memory(std::mem::take(&mut owned)).into_reader();
        let n = match &mut reader {
            Ok(r) => r.read(&mut scratch).unwrap_or(0),
            Err(_) => 0,
        };
        if let Ok(PayloadReader::Memory(cursor)) = reader {
            owned = cursor.into_inner();
        }
        n
    }));
    rows.push(mode.micro("payload", "Box<dyn Read> build + read, 4 KiB (avoided)", 4096, || {
        let mut reader: Box<dyn Read + '_> = Box::new(&data[..]);
        reader.read(&mut scratch).unwrap_or(0)
    }));

    // Bounded reads. The hint is what `device.rs` passes for a text-like MIME:
    // one page reserved up front instead of the read loop's regrowth.
    for len in [64usize, 300, 4096] {
        let bytes = payload(len, 9);
        rows.push(mode.micro("payload", format!("read_bounded, {}, no hint", size_label(len)), len as u64, || {
            read_bounded(black_box(&bytes[..]), limit, None).map(|v| v.len())
        }));
        rows.push(mode.micro("payload", format!("read_bounded, {}, 4 KiB hint", size_label(len)), len as u64, || {
            read_bounded(black_box(&bytes[..]), limit, Some(inline)).map(|v| v.len())
        }));
    }
    rows.push(mode.micro("payload", "Payload::Memory read_bounded, 4 KiB (hand-over)", 4096, || {
        match Payload::Memory(std::mem::take(&mut owned)).read_bounded(limit) {
            Ok(v) => {
                owned = v;
                true
            }
            Err(_) => false,
        }
    }));

    // A real descriptor: pipe, write, close, read to EOF. The syscall floor that
    // the v0.7.3 splice path has to beat; pipe set-up is part of every operation.
    for len in [4096usize, 32 * 1024] {
        let bytes = payload(len, 5);
        rows.push(mode.micro("payload", format!("pipe stream read_bounded, {} (incl. pipe set-up)", size_label(len)), len as u64, || {
            let Ok((reader, mut writer)) = std::io::pipe() else { return 0 };
            if writer.write_all(&bytes).is_err() {
                return 0;
            }
            drop(writer);
            let stream = StreamDescriptor::new(OwnedFd::from(reader), Some(len as u64));
            Payload::Stream(stream).read_bounded(limit).map(|v| v.len()).unwrap_or(0)
        }));
    }
}

// ---------------------------------------------------------------------------
// Stream-to-file: the v0.7.3 kernel path against the heap road it replaces
// ---------------------------------------------------------------------------

/// SHA3-256 as a stream observer, exactly what the daemon's spooler uses.
struct Sha3Tap(Sha3_256);

impl ByteTap for Sha3Tap {
    fn update(&mut self, chunk: &[u8]) {
        Digest::update(&mut self.0, chunk);
    }
}

/// A pipe carrying `data` and then EOF, as a compositor client would provide.
/// Anything beyond the pipe buffer needs a writer thread, as in production.
fn fed_pipe(data: &Arc<Vec<u8>>) -> Option<(File, Option<std::thread::JoinHandle<()>>)> {
    let (reader, mut writer) = std::io::pipe().ok()?;
    let reader = File::from(OwnedFd::from(reader));
    if data.len() <= 32 * 1024 {
        writer.write_all(data).ok()?;
        return Some((reader, None));
    }
    let data = Arc::clone(data);
    let handle = std::thread::spawn(move || {
        let _ = writer.write_all(&data);
    });
    Some((reader, Some(handle)))
}

/// A fresh, private destination file (unlinked again by the caller).
fn bench_file() -> Option<(File, PathBuf)> {
    static N: AtomicU64 = AtomicU64::new(0);
    let mut path = core::get_cache_dir();
    path.push(format!("bench-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).ok()?;
    Some((file, path))
}

fn bench_splice(mode: &Mode, rows: &mut Vec<Row>) {
    let limit = core::constants::MAX_PAYLOAD_BYTES;
    for len in [32 * 1024usize, 1024 * 1024, 16 * 1024 * 1024] {
        let data = Arc::new(payload(len, 0xfeed));
        let label = size_label(len);
        let finish = |handle: Option<std::thread::JoinHandle<()>>, path: PathBuf, copied: u64| {
            if let Some(handle) = handle {
                let _ = handle.join();
            }
            let _ = std::fs::remove_file(path);
            copied
        };

        rows.push(mode.micro("splice", format!("pipe -> file {label}: heap Vec + sha3 + write (v0.7.2 road)"), len as u64, || {
            let (Some((src, handle)), Some((mut dst, path))) = (fed_pipe(&data), bench_file()) else { return 0 };
            let copied = match read_bounded(src, limit, None) {
                Ok(bytes) => {
                    let mut hasher = Sha3_256::new();
                    Digest::update(&mut hasher, &bytes);
                    black_box(hasher.finalize());
                    dst.write_all(&bytes).map_or(0, |()| bytes.len() as u64)
                }
                Err(_) => 0,
            };
            finish(handle, path, copied)
        }));
        rows.push(mode.micro("splice", format!("pipe -> file {label}: buffered copy + sha3 (fallback path)"), len as u64, || {
            let (Some((mut src, handle)), Some((mut dst, path))) = (fed_pipe(&data), bench_file()) else { return 0 };
            let copied = copy_to_file_buffered(&mut src, &mut dst, limit, &mut Sha3Tap(Sha3_256::new())).map_or(0, |c| c.len);
            finish(handle, path, copied)
        }));
        rows.push(mode.micro("splice", format!("pipe -> file {label}: splice only"), len as u64, || {
            let (Some((mut src, handle)), Some((mut dst, path))) = (fed_pipe(&data), bench_file()) else { return 0 };
            let copied = copy_to_file(&mut src, &mut dst, limit, &mut NoTap).map_or(0, |c| c.len);
            finish(handle, path, copied)
        }));
        rows.push(mode.micro("splice", format!("pipe -> file {label}: splice + tee + sha3 (v0.7.3 road)"), len as u64, || {
            let (Some((mut src, handle)), Some((mut dst, path))) = (fed_pipe(&data), bench_file()) else { return 0 };
            let tap = &mut Sha3Tap(Sha3_256::new());
            let copied = copy_to_file(&mut src, &mut dst, limit, tap).map_or(0, |c| c.len);
            black_box(std::mem::replace(&mut tap.0, Sha3_256::new()).finalize());
            finish(handle, path, copied)
        }));
    }
}

// ---------------------------------------------------------------------------
// Ingest: pipe -> queue -> real worker -> real database, and the memory bound
// ---------------------------------------------------------------------------

/// Persists `len`-byte GIFs end to end, one at a time, and reports what the
/// heap did while each one was in flight. `spooled` selects the road: the
/// v0.7.2 one (the ingestion thread reads the pipe into a `Vec`) or the v0.7.3
/// one (`FileSpooler` splices it to a file, the worker adopts the file).
fn ingest_once(
    tx: &std::sync::mpsc::SyncSender<ClipboardJob>,
    spooler: &FileSpooler,
    metrics: &DaemonMetrics,
    data: &mut Arc<Vec<u8>>,
    counter: &mut u64,
    spooled: bool,
) -> Result<(), String> {
    *counter += 1;
    // A unique payload per operation, so every one is an insert, never a duplicate.
    if let Some(bytes) = Arc::get_mut(data) {
        for (slot, byte) in bytes.iter_mut().skip(6).zip(counter.to_le_bytes()) {
            *slot = byte;
        }
    }
    let persisted_before = metrics.ingress_count();
    let (mut src, handle) = fed_pipe(data).ok_or("pipe set-up failed")?;
    let job = if spooled {
        let head_len = core::constants::INLINE_PAYLOAD_THRESHOLD_BYTES.min(data.len());
        let mut head = vec![0u8; head_len];
        src.read_exact(&mut head).map_err(|e| format!("head read failed: {e}"))?;
        match spooler.spool(&head, src, core::constants::MAX_PAYLOAD_BYTES) {
            SpoolOutcome::Spooled(file) => ClipboardJob::from_payload("image/gif".to_owned(), Payload::Spooled(Box::new(file))),
            _ => return Err("the spooler did not spool the payload".into()),
        }
    } else {
        let bytes = read_bounded(src, core::constants::MAX_PAYLOAD_BYTES, None).map_err(|e| e.to_string())?;
        ClipboardJob::new("image/gif".to_owned(), bytes)
    };
    if let Some(handle) = handle {
        let _ = handle.join();
    }
    tx.send(job).map_err(|_| "the worker is gone".to_owned())?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while metrics.ingress_count() <= persisted_before {
        if Instant::now() > deadline {
            return Err("timed out waiting for the worker".into());
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    Ok(())
}

fn bench_ingest(mode: &Mode, rows: &mut Vec<Row>) -> Result<(), String> {
    let spooler = FileSpooler::open(core::spool_dir_path()).map_err(|e| format!("spool directory: {e}"))?;
    let metrics = Arc::new(DaemonMetrics::new());
    let worker = DbWorker::spawn(ClipboardDb::open()?, Arc::clone(&metrics), false, 8);
    let tx = worker.sender();

    let ops: u64 = if mode.full { 12 } else { 1 };
    let mut notes = Vec::new();
    for len in [1024 * 1024usize, 16 * 1024 * 1024] {
        let mut data = {
            let mut bytes = payload(len, 0xbeef);
            bytes.splice(0..6, *b"GIF89a");
            Arc::new(bytes)
        };
        let mut counter = 0u64;
        for spooled in [false, true] {
            let road = if spooled { "spooled, splice + adopt (v0.7.3)" } else { "heap Vec (v0.7.2)" };
            let before = LIVE_BYTES.load(Ordering::Relaxed);
            PEAK_BYTES.store(before, Ordering::Relaxed);
            let row = mode.batched(
                "ingest",
                format!("pipe -> database, {} GIF, {road}", size_label(len)),
                ops,
                len as u64,
                &mut (),
                |_| Ok(()),
                |_| ingest_once(&tx, &spooler, &metrics, &mut data, &mut counter, spooled),
            )?;
            let peak = PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(before);
            notes.push(format!(
                "ingest peak heap growth, {} GIF, {road}: {:.2} MiB",
                size_label(len),
                peak as f64 / (1024.0 * 1024.0)
            ));
            rows.push(row);
        }
    }
    for note in &notes {
        println!("\n{note}");
    }

    // Memory bound with spooled payloads: offer twice the queue's capacity of
    // 1 MiB payloads. The queue holds paths and digests, so what it pins on the
    // heap is independent of the payloads' size; the surplus is shed (and its
    // files unlinked).
    let capacity = core::constants::INGEST_QUEUE_CAPACITY;
    let (qtx, qrx) = sync_channel::<ClipboardJob>(capacity);
    let before = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(before, Ordering::Relaxed);
    let (mut queued, mut shed) = (0usize, 0usize);
    let mut dir = core::get_cache_dir();
    dir.push("bench-bound");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for i in 0..capacity * 2 {
        let path = dir.join(format!("{i}.part"));
        let file = File::create(&path).map_err(|e| e.to_string())?;
        file.set_len(1024 * 1024).map_err(|e| e.to_string())?;
        let spooled = SpooledFile::new(path, 1024 * 1024, [0u8; core::constants::DIGEST_LEN]);
        let job = ClipboardJob::from_payload("image/gif".to_owned(), Payload::Spooled(Box::new(spooled)));
        match ingest::enqueue(&qtx, job, Duration::ZERO) {
            Enqueue::Queued => queued += 1,
            Enqueue::Shed => shed += 1,
            Enqueue::Closed => return Err("queue closed unexpectedly".into()),
        }
    }
    let held = PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(before);
    let files_left = std::fs::read_dir(&dir).map(|rd| rd.count()).unwrap_or(0);
    println!(
        "\nmemory bound (spooled): offered {} x 1 MiB into a queue of {capacity}: queued {queued}, shed {shed}, peak heap growth {:.3} MiB, spool files left on disk {files_left}",
        capacity * 2,
        held as f64 / (1024.0 * 1024.0)
    );
    if queued != capacity || shed != capacity || files_left != capacity {
        return Err(format!("spooled queue bound violated: queued {queued}, shed {shed}, files {files_left}"));
    }
    drop(qrx);
    drop(qtx);
    let files_after = std::fs::read_dir(&dir).map(|rd| rd.count()).unwrap_or(0);
    let _ = std::fs::remove_dir_all(&dir);
    if files_after != 0 {
        return Err(format!("{files_after} spool files outlived their queue"));
    }
    drop(tx);
    drop(worker);
    Ok(())
}

// ---------------------------------------------------------------------------
// Transcode: a spooled raster image through `magick`, heap road versus descriptors
// ---------------------------------------------------------------------------

/// A spooled raster image, converted two ways: the v0.7.3 road reads the whole
/// file into a `Vec`, feeds `magick` through the chunk-buffered relay, collects
/// its output in a second `Vec` and hashes that; the v0.7.4 road hands the file
/// to `magick` as its stdin and splices its stdout into a new file, hashing on
/// the way (`transcode_spooled`). Both start from the same spool file and end
/// with the converted bytes' SHA3-256; only the second also leaves them on disk
/// ready for adoption, so the first is not charged for a write it would still owe.
///
/// Incompressible noise on purpose: the PNG is as large as the pixels, so the
/// byte volume is what the label says and `magick` has real work to do.
fn bench_transcode(mode: &Mode, rows: &mut Vec<Row>) -> Result<(), String> {
    if !y4p_image::magick::is_available() {
        println!("\ntranscode group skipped: `magick` is not available on this system");
        return Ok(());
    }
    let dir = core::spool_dir_path();
    std::fs::create_dir_all(&dir).map_err(|e| format!("spool directory: {e}"))?;
    let job = Transcode { input: InputFormat::Png, output: OutputFormat::Png };
    let limit = core::constants::MAX_PAYLOAD_BYTES;

    // Smoke mode (`cargo test`) converts the small image only.
    let sides: &[u32] = if mode.full { &[512, 2048] } else { &[512] };
    let mut notes = Vec::new();
    for &side in sides {
        let geometry = format!("{side}x{side}");
        let mut png = Vec::new();
        y4p_image::magick::run(&["-size", &geometry, "xc:", "+noise", "Random", "-depth", "8", "png:-"], std::io::empty(), &mut png)
            .map_err(|e| format!("cannot generate the fixture image: {e}"))?;
        let path = dir.join(format!("{}-{side}.part", std::process::id()));
        std::fs::write(&path, &png).map_err(|e| e.to_string())?;
        let digest = <[u8; core::constants::DIGEST_LEN]>::try_from(Sha3_256::digest(&png).as_slice()).map_err(|e| e.to_string())?;
        let source = SpooledFile::new(path.clone(), png.len() as u64, digest);
        let label = size_label(png.len());
        let len = png.len() as u64;
        drop(png);

        let memory_road = || -> Option<[u8; core::constants::DIGEST_LEN]> {
            let bytes = std::fs::read(&path).ok()?;
            let mut out = Vec::with_capacity(bytes.len());
            transcode::transcode(job, std::io::Cursor::new(&bytes), &mut out).ok()?;
            <[u8; core::constants::DIGEST_LEN]>::try_from(Sha3_256::digest(&out).as_slice()).ok()
        };
        let descriptor_road = || transcode_spooled(&source, job, limit).ok().map(|converted| *converted.digest());

        // Same input, same arguments: the two roads must produce the same digest,
        // or deduplication would key one image two ways.
        let (a, b) = (memory_road(), descriptor_road());
        if a.is_none() || a != b {
            return Err(format!("the two transcode roads disagree on the {label} image: {a:?} vs {b:?}"));
        }

        for (road, run) in [
            ("read into Vec + buffered relay + sha3 (v0.7.3 road)", &memory_road as &dyn Fn() -> Option<[u8; core::constants::DIGEST_LEN]>),
            ("spool file -> spool file, stdin handed over, stdout spliced + tee + sha3 (v0.7.4 road)", &descriptor_road),
        ] {
            let before = LIVE_BYTES.load(Ordering::Relaxed);
            PEAK_BYTES.store(before, Ordering::Relaxed);
            black_box(run());
            let peak = PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(before);
            notes.push(format!("transcode peak heap growth, {label} PNG, {road}: {:.2} MiB", peak as f64 / (1024.0 * 1024.0)));
            rows.push(mode.micro("transcode", format!("raster {label} PNG -> PNG: {road}"), len, run));
        }
        drop(source);
    }
    for note in &notes {
        println!("\n{note}");
    }
    Ok(())
}

fn bench_queue(mode: &Mode, rows: &mut Vec<Row>) -> Result<(), String> {
    let capacity = core::constants::INGEST_QUEUE_CAPACITY;

    // Uncontended hand-off: one enqueue and one dequeue on one thread. The job is
    // moved through the channel, never copied; its construction is part of the cost.
    for len in [64usize, 4 * 1024] {
        let (tx, rx) = sync_channel::<ClipboardJob>(capacity);
        rows.push(mode.micro("queue", format!("enqueue + dequeue, {} payload", size_label(len)), 0, || {
            let outcome = ingest::enqueue(&tx, job(len), Duration::ZERO);
            (outcome, rx.try_recv().is_ok())
        }));
    }

    // The same hand-off under the byte budget: one compare-and-swap to reserve,
    // one atomic subtraction when the dequeued job (and its permit) is dropped.
    let budget = Arc::new(ByteBudget::new(core::constants::INGEST_QUEUE_BYTE_BUDGET));
    for len in [64usize, 4 * 1024] {
        let (tx, rx) = sync_channel::<ClipboardJob>(capacity);
        rows.push(mode.micro("queue", format!("enqueue_bounded + dequeue, {} payload", size_label(len)), 0, || {
            let outcome = ingest::enqueue_bounded(&tx, &budget, job(len), Duration::ZERO);
            (outcome, rx.try_recv().is_ok())
        }));
    }
    // Cost of refusing work for want of bytes: the queue has room, the budget has none.
    let tight = Arc::new(ByteBudget::new(64));
    let held = tight.try_reserve(64).ok_or("could not fill the benchmark byte budget")?;
    let (tx, _rx) = sync_channel::<ClipboardJob>(capacity);
    rows.push(mode.micro("queue", "shed on a full byte budget (zero patience)", 0, || ingest::enqueue_bounded(&tx, &tight, job(64), Duration::ZERO)));
    drop(held);

    // Cost of refusing work: the queue is full, nobody drains it, patience is zero.
    let (tx, rx) = sync_channel::<ClipboardJob>(capacity);
    for _ in 0..capacity {
        if ingest::enqueue(&tx, job(64), Duration::ZERO) != Enqueue::Queued {
            return Err("could not fill the benchmark queue".into());
        }
    }
    rows.push(mode.micro("queue", "shed on a full queue (zero patience)", 0, || ingest::enqueue(&tx, job(64), Duration::ZERO)));
    drop(rx);
    drop(tx);

    // Admission control.
    let gate = Arc::new(ReaderGate::new(core::constants::INGEST_MAX_CONCURRENT_READERS));
    rows.push(mode.micro("queue", "reader gate acquire + release", 0, || gate.try_acquire().is_some()));

    // Sustained producer -> consumer throughput through the bounded queue.
    let messages: u64 = if mode.full { 50_000 } else { 100 };
    for len in [64usize, 4 * 1024] {
        rows.push(throughput(mode, len, messages)?);
    }

    // Memory bound: fill the queue with 1 MiB payloads, offer as many again, and
    // observe how much the heap actually grew. The surplus must be shed, not held.
    let (tx, rx) = sync_channel::<ClipboardJob>(capacity);
    let before = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(before, Ordering::Relaxed);
    let (mut queued, mut shed) = (0usize, 0usize);
    for _ in 0..capacity * 2 {
        match ingest::enqueue(&tx, job(1024 * 1024), Duration::ZERO) {
            Enqueue::Queued => queued += 1,
            Enqueue::Shed => shed += 1,
            Enqueue::Closed => return Err("queue closed unexpectedly".into()),
        }
    }
    let held = PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(before);
    println!(
        "\nmemory bound: offered {} x 1 MiB into a queue of {capacity}: queued {queued}, shed {shed}, peak heap growth {:.1} MiB",
        capacity * 2,
        held as f64 / (1024.0 * 1024.0)
    );
    if queued != capacity || shed != capacity {
        return Err(format!("queue bound violated: queued {queued}, shed {shed}, capacity {capacity}"));
    }
    drop(rx);
    drop(tx);
    Ok(())
}

/// One producer thread feeds one consumer thread through the bounded queue with
/// generous patience (so nothing is shed); returns nanoseconds per message and
/// the allocation counters spent on both threads.
fn throughput_pass(len: usize, messages: u64) -> Result<(f64, f64, f64), String> {
    let (tx, rx) = sync_channel::<ClipboardJob>(core::constants::INGEST_QUEUE_CAPACITY);
    let (calls0, bytes0) = alloc_snapshot();
    let started = Instant::now();

    let consumer = std::thread::spawn(move || {
        let mut received = 0u64;
        while rx.recv().is_ok() {
            received += 1;
        }
        received
    });
    for _ in 0..messages {
        if ingest::enqueue(&tx, job(len), Duration::from_secs(30)) != Enqueue::Queued {
            return Err("producer lost a job during the throughput run".into());
        }
    }
    drop(tx);
    let received = consumer.join().map_err(|_| "consumer thread panicked".to_owned())?;
    let elapsed = started.elapsed();
    let (calls1, bytes1) = alloc_snapshot();
    if received != messages {
        return Err(format!("consumer saw {received} of {messages} jobs"));
    }
    Ok((
        elapsed.as_nanos() as f64 / messages as f64,
        (calls1 - calls0) as f64 / messages as f64,
        (bytes1 - bytes0) as f64 / messages as f64,
    ))
}

fn throughput(mode: &Mode, len: usize, messages: u64) -> Result<Row, String> {
    let rounds = if mode.full { ROUNDS } else { 1 };
    let mut samples = Vec::with_capacity(rounds);
    let (mut allocs, mut alloc_bytes) = (0.0, 0.0);
    for round in 0..rounds {
        let (ns, calls, bytes) = throughput_pass(len, messages)?;
        samples.push(ns);
        if round == 0 {
            (allocs, alloc_bytes) = (calls, bytes);
        }
    }
    let min_ns = minimum(&samples);
    Ok(Row {
        group: "queue",
        name: format!("producer/consumer throughput, {} payload", size_label(len)),
        median_ns: if mode.full { median(&mut samples) } else { 0.0 },
        min_ns: if mode.full { min_ns } else { 0.0 },
        payload_bytes: len as u64,
        allocs_per_op: allocs,
        alloc_bytes_per_op: alloc_bytes,
        switches_per_op: None,
    })
}

const BIG: usize = 1024 * 1024;

/// Reusable payload and hash buffers, so that building a unique row never
/// allocates inside a timed region: the allocation columns then describe the
/// storage layer alone, not the harness.
struct Fixture {
    data: Vec<u8>,
    hash: String,
}

impl Fixture {
    fn new() -> Self {
        Self { data: Vec::new(), hash: String::with_capacity(64) }
    }

    /// Sizes the payload buffer (untimed; call from `setup`).
    fn prepare(&mut self, len: usize) {
        self.data.clear();
        self.data.resize(len, 0x61);
    }

    /// Makes the payload and hash unique to `n` without allocating.
    fn stamp(&mut self, n: u64) {
        for (slot, byte) in self.data.iter_mut().zip(n.to_le_bytes()) {
            *slot = byte;
        }
        self.hash.clear();
        let _ = write!(self.hash, "{n:064x}");
    }
}

struct Ctx {
    db: ClipboardDb,
    fx: Fixture,
    counter: u64,
}

impl Ctx {
    fn wipe(&mut self) -> Result<(), String> {
        self.db.wipe().map_err(|e| format!("wipe failed: {e}"))
    }

    /// Inserts one row that no earlier row duplicates.
    fn insert_unique(&mut self, mime: &str, max_history: usize) -> Result<(), String> {
        self.counter += 1;
        self.fx.stamp(self.counter);
        self.db.insert_with_hash(mime, &self.fx.data, &self.fx.hash, max_history).map(|_| ())
    }

    fn expect_rows(&self, expected: usize, what: &str) -> Result<(), String> {
        let actual = self.db.get_total_count();
        if actual == expected { Ok(()) } else { Err(format!("{what}: expected {expected} rows, found {actual}")) }
    }
}

fn bench_sqlite(mode: &Mode, rows: &mut Vec<Row>) -> Result<(), String> {
    let mut ctx = Ctx { db: ClipboardDb::open()?, fx: Fixture::new(), counter: 0 };
    let ops: u64 = if mode.full { 2000 } else { 1 };
    let big_ops: u64 = if mode.full { 40 } else { 1 };
    let limit = 256usize;
    const NO_ROTATION: usize = 1_000_000;

    // Insert without rotation (history limit far above the row count).
    rows.push(mode.batched(
        "sqlite", "insert 64 B text (no rotation)", ops, 64, &mut ctx,
        |c| {
            c.wipe()?;
            c.fx.prepare(64);
            Ok(())
        },
        |c| c.insert_unique("text/plain", NO_ROTATION),
    )?);
    ctx.expect_rows(ops as usize, "insert without rotation")?;

    // Steady-state rotation: the table starts at its limit, so every insert also evicts.
    rows.push(mode.batched(
        "sqlite", format!("insert 64 B text + rotation (limit {limit})"), ops, 64, &mut ctx,
        |c| {
            c.wipe()?;
            c.fx.prepare(64);
            (0..limit).try_for_each(|_| c.insert_unique("text/plain", limit))
        },
        |c| c.insert_unique("text/plain", limit),
    )?);
    ctx.expect_rows(limit, "rotation must hold the table at its limit")?;

    // Duplicate hit: identical hash, so the insert degenerates to a timestamp refresh.
    rows.push(mode.batched(
        "sqlite", "duplicate hit (same hash)", ops, 64, &mut ctx,
        |c| {
            c.wipe()?;
            c.fx.prepare(64);
            c.fx.stamp(424_242);
            c.db.insert_with_hash("text/plain", &c.fx.data, &c.fx.hash, limit).map(|_| ())
        },
        |c| c.db.insert_with_hash("text/plain", &c.fx.data, &c.fx.hash, limit).map(|_| ()),
    )?);
    ctx.expect_rows(1, "duplicate hits must not add rows")?;

    // Metadata scans over a populated history (read-only, so one fill is enough).
    ctx.wipe()?;
    let population = if mode.full { 1000u64 } else { 10 };
    for n in 0..population {
        let preview = format!("entry {n:08} some searchable preview text");
        ctx.fx.stamp(n + 1_000_000);
        ctx.db.insert_with_hash("text/plain", preview.as_bytes(), &ctx.fx.hash, NO_ROTATION)?;
    }
    ctx.expect_rows(population as usize, "scan fixture")?;
    let scan_ops: u64 = if mode.full { 200 } else { 1 };
    let scanned = Cell::new(0usize);
    for take in [100usize, 1000] {
        rows.push(mode.batched(
            "sqlite", format!("fetch_metadata({take}) over {population} rows"), scan_ops, 0, &mut ctx,
            |_| Ok(()),
            |c| {
                scanned.set(scanned.get() + c.db.fetch_metadata(black_box(take)).len());
                Ok(())
            },
        )?);
    }
    let query = vec!["searchable".to_owned()];
    rows.push(mode.batched(
        "sqlite", format!("search_metadata(1 keyword, 100) over {population} rows"), scan_ops, 0, &mut ctx,
        |_| Ok(()),
        |c| {
            scanned.set(scanned.get() + c.db.search_metadata(black_box(&query), 100).len());
            Ok(())
        },
    )?);
    if scanned.get() == 0 {
        return Err("metadata scans returned nothing; the fixture is wrong".into());
    }

    // Large payloads: a binary BLOB stays in the database, an image goes to the cache file.
    for (mime, label) in [("application/octet-stream", "1 MiB BLOB insert"), ("image/png", "1 MiB image insert (cache file)")] {
        rows.push(mode.batched(
            "sqlite", label, big_ops, BIG as u64, &mut ctx,
            |c| {
                c.wipe()?;
                c.fx.prepare(BIG);
                Ok(())
            },
            |c| c.insert_unique(mime, NO_ROTATION),
        )?);
        ctx.expect_rows(big_ops as usize, label)?;
    }
    ctx.wipe()
}

// ---------------------------------------------------------------------------
// cdc: the FastCDC engine on its own (v0.7.5)
// ---------------------------------------------------------------------------

/// Printable ASCII, so a payload is valid UTF-8 and takes the text road in the
/// storage layer, while still being content-rich enough for boundaries to fall
/// where the content puts them.
fn prose(len: usize, seed: u64) -> Vec<u8> {
    payload(len, seed).into_iter().map(|b| b' ' + b % 95).collect()
}

/// Bytes of distinct chunks among `payloads`: what a deduplicating store keeps.
fn unique_chunk_bytes(payloads: &[&[u8]]) -> (usize, usize, usize) {
    let mut seen = HashSet::new();
    let (mut total, mut unique_bytes, mut chunks) = (0usize, 0usize, 0usize);
    for data in payloads {
        for chunk in FastCdc::new(data) {
            total += chunk.len();
            chunks += 1;
            if seen.insert(Sha3_256::digest(chunk).to_vec()) {
                unique_bytes += chunk.len();
            }
        }
    }
    (total, unique_bytes, chunks)
}

/// Largest payload of the v0.7.5 - v0.7.7 groups. Smoke mode (`cargo test`, an
/// unoptimised build) stays at 1 MiB: it only has to prove each case runs.
fn top_size(mode: &Mode) -> usize {
    if mode.full { 16 * 1024 * 1024 } else { 1024 * 1024 }
}

fn bench_cdc(mode: &Mode, rows: &mut Vec<Row>, facts: &mut Vec<String>) {
    let top = top_size(mode);
    let mut walk_sizes = vec![4 * 1024usize, 64 * 1024, 1024 * 1024];
    if top > 1024 * 1024 {
        walk_sizes.push(top);
    }
    for len in walk_sizes {
        let data = prose(len, 0xcdc0);
        let (count, min, max) = FastCdc::new(&data).fold((0usize, usize::MAX, 0usize), |(n, lo, hi), c| (n + 1, lo.min(c.len()), hi.max(c.len())));
        let label = size_label(len);
        facts.push(format!("cdc {label}: {count} chunks, mean {} B, min {min} B, max {max} B", len / count.max(1)));
        rows.push(mode.micro("cdc", format!("FastCdc walk {label} ({count} chunks)"), len as u64, || {
            FastCdc::new(black_box(&data)).map(<[u8]>::len).sum::<usize>()
        }));
    }
    let hashed_sizes: &[usize] = if top > 1024 * 1024 { &[1024 * 1024, 16 * 1024 * 1024] } else { &[1024 * 1024] };
    for &len in hashed_sizes {
        let data = prose(len, 0xcdc2);
        rows.push(mode.micro("cdc", format!("FastCdc + SHA3-256 per chunk {} (storage write path minus SQL)", size_label(len)), len as u64, || {
            FastCdc::new(black_box(&data)).map(|c| Sha3_256::digest(c)[0]).fold(0u8, u8::wrapping_add)
        }));
    }

    // Deduplication after an edit: what a store keeps for original + edited copy.
    // Hashes ~100 MiB, so it belongs to the full measurement only.
    if !mode.full {
        return;
    }
    let original = prose(16 * 1024 * 1024, 0xcdc3);
    for (label, position, inserted) in [("1 byte inserted mid-way", original.len() / 2, 1usize), ("1 byte inserted at the front", 0, 1), ("4 KiB inserted mid-way", original.len() / 2, 4096)] {
        let mut edited = original.clone();
        let filler = prose(inserted, 0xed17);
        edited.splice(position..position, filler);
        let (total, kept, _) = unique_chunk_bytes(&[&original, &edited]);
        let first = unique_chunk_bytes(&[&original]).1;
        facts.push(format!(
            "cdc dedup, 16 MiB original + edited copy ({label}): {total} B offered, {kept} B kept; the copy added {} B ({:.3} % of it)",
            kept - first,
            (kept - first) as f64 * 100.0 / edited.len() as f64
        ));
    }
}

// ---------------------------------------------------------------------------
// pipe: F_SETPIPE_SZ, 64 KiB (the kernel default) against 1 MiB (v0.7.7)
// ---------------------------------------------------------------------------

/// Context switches of this process so far (voluntary + involuntary, all threads).
fn context_switches() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is a valid, writable `rusage` for `getrusage(2)` to fill;
    // the all-zero pattern is a valid value of that plain-integer C struct, so
    // reading it back is sound even if the call were to fail.
    let usage = unsafe {
        libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
        usage.assume_init()
    };
    u64::try_from(usage.ru_nvcsw).unwrap_or(0) + u64::try_from(usage.ru_nivcsw).unwrap_or(0)
}

/// Context switches per call of `f`, averaged over `reps` calls.
fn switches_per_op<R>(mode: &Mode, f: &mut impl FnMut() -> R) -> f64 {
    let reps = if mode.full { 20u64 } else { 1 };
    let before = context_switches();
    for _ in 0..reps {
        black_box(f());
    }
    (context_switches().saturating_sub(before)) as f64 / reps as f64
}

/// Same as `fed_pipe`, with the pipe's capacity set first: `None` leaves the
/// kernel default (64 KiB), `Some(n)` asks for `n` (the 1 MiB target).
fn fed_pipe_sized(data: &Arc<Vec<u8>>, capacity: Option<usize>) -> Option<(File, std::thread::JoinHandle<()>)> {
    let (reader, mut writer) = std::io::pipe().ok()?;
    if let Some(bytes) = capacity {
        grow_to(&writer, bytes)?;
    }
    let reader = File::from(OwnedFd::from(reader));
    let data = Arc::clone(data);
    let handle = std::thread::spawn(move || {
        let _ = writer.write_all(&data);
    });
    Some((reader, handle))
}

/// A file of `len` bytes to serve with `sendfile(2)`, kept open for the run.
fn egress_source(data: &[u8]) -> Option<(File, PathBuf)> {
    let (mut file, path) = bench_file()?;
    file.write_all(data).ok()?;
    drop(file);
    Some((File::open(&path).ok()?, path))
}

/// `sendfile(2)` of the whole of `src` into a pipe whose capacity is `capacity`,
/// drained by a reader thread in 64 KiB reads (a paste target such as `wl-paste`).
fn sendfile_egress(src: &File, len: usize, capacity: Option<usize>) -> usize {
    let Ok((mut reader, writer)) = std::io::pipe() else { return 0 };
    if let Some(bytes) = capacity
        && grow_to(&writer, bytes).is_none()
    {
        return 0;
    }
    let drain = std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0usize;
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            total += n;
        }
        total
    });
    let mut offset: libc::off_t = 0;
    while (offset as usize) < len {
        // SAFETY: both descriptors are open for the whole call (`src` is borrowed
        // and `writer` is a local), and `offset` is a valid `&mut off_t`.
        let n = unsafe { libc::sendfile(writer.as_raw_fd(), src.as_raw_fd(), &mut offset, 1 << 30) };
        if n <= 0 {
            break;
        }
    }
    drop(writer);
    drain.join().unwrap_or(0)
}

fn bench_pipe(mode: &Mode, rows: &mut Vec<Row>, facts: &mut Vec<String>) {
    let limit = core::constants::MAX_PAYLOAD_BYTES;
    let target = core::constants::PIPE_TARGET_BYTES;
    let granted = std::io::pipe().ok().and_then(|(_, w)| grow_to(&w, target));
    facts.push(format!("pipe: requested {target} B, this system granted {} B (default {} B)", granted.unwrap_or(0), core::constants::PIPE_DEFAULT_BYTES));

    let top = top_size(mode);
    let sizes: &[usize] = if top == 1024 * 1024 { &[1024 * 1024] } else { &[1024 * 1024, 16 * 1024 * 1024] };
    for &len in sizes {
        let data = Arc::new(payload(len, 0xb1be));
        let label = size_label(len);
        for (cap_label, capacity) in [("64 KiB pipe", None), ("1 MiB pipe", Some(target))] {
            let mut cases: Vec<(String, Box<dyn FnMut() -> u64 + '_>)> = Vec::new();
            cases.push((format!("write -> read(64 KiB) loop {label}, {cap_label}"), Box::new(|| {
                let Some((mut src, handle)) = fed_pipe_sized(&data, capacity) else { return 0 };
                let mut buf = vec![0u8; 64 * 1024];
                let mut total = 0u64;
                while let Ok(n) = src.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    total += n as u64;
                }
                let _ = handle.join();
                total
            })));
            cases.push((format!("pipe -> file {label}: splice only, {cap_label}"), Box::new(|| {
                let (Some((mut src, handle)), Some((mut dst, path))) = (fed_pipe_sized(&data, capacity), bench_file()) else { return 0 };
                let copied = copy_to_file(&mut src, &mut dst, limit, &mut NoTap).map_or(0, |c| c.len);
                let _ = handle.join();
                let _ = std::fs::remove_file(path);
                copied
            })));
            cases.push((format!("pipe -> file {label}: splice + tee + sha3, {cap_label}"), Box::new(|| {
                let (Some((mut src, handle)), Some((mut dst, path))) = (fed_pipe_sized(&data, capacity), bench_file()) else { return 0 };
                let tap = &mut Sha3Tap(Sha3_256::new());
                let copied = copy_to_file(&mut src, &mut dst, limit, tap).map_or(0, |c| c.len);
                black_box(std::mem::replace(&mut tap.0, Sha3_256::new()).finalize());
                let _ = handle.join();
                let _ = std::fs::remove_file(path);
                copied
            })));
            for (name, mut case) in cases {
                let mut row = mode.micro("pipe", name, len as u64, &mut case);
                row.switches_per_op = Some(switches_per_op(mode, &mut case));
                rows.push(row);
            }
        }

        // Egress: the paste target's pipe, filled with sendfile(2).
        if let Some((src, path)) = egress_source(&data) {
            for (cap_label, capacity) in [("64 KiB pipe", None), ("1 MiB pipe", Some(target))] {
                let mut case = || sendfile_egress(&src, len, capacity);
                let mut row = mode.micro("pipe", format!("egress sendfile file -> pipe -> read(64 KiB) {label}, {cap_label}"), len as u64, &mut case);
                row.switches_per_op = Some(switches_per_op(mode, &mut case));
                rows.push(row);
            }
            let _ = std::fs::remove_file(path);
        }
    }
}

// ---------------------------------------------------------------------------
// chunked: content-defined chunk storage in SQLite (v0.7.6)
// ---------------------------------------------------------------------------

/// Payloads built before the timed region, consumed one per operation, so every
/// insert is unique by content and none pays for generating its bytes.
struct ChunkCtx {
    db: ClipboardDb,
    payloads: Vec<Vec<u8>>,
    next: usize,
    hash: String,
    counter: u64,
    base_id: i64,
}

impl ChunkCtx {
    fn insert_next(&mut self) -> Result<(), String> {
        let data = self.payloads.get(self.next).ok_or("ran out of prepared payloads")?;
        self.next += 1;
        self.counter += 1;
        self.hash.clear();
        let _ = write!(self.hash, "{:064x}", self.counter);
        self.db.insert_with_hash("text/plain", data, &self.hash, 1_000_000).map(|_| ())
    }
}

fn bench_chunked(mode: &Mode, rows: &mut Vec<Row>) -> Result<(), String> {
    let mut ctx = ChunkCtx { db: ClipboardDb::open()?, payloads: Vec::new(), next: 0, hash: String::with_capacity(64), counter: 0, base_id: 0 };
    let wipe = |c: &mut ChunkCtx| c.db.wipe().map_err(|e| format!("wipe failed: {e}"));

    let top = top_size(mode);
    for (len, full_ops) in [(64 * 1024usize, 100u64), (1024 * 1024, 20), (16 * 1024 * 1024, 3)] {
        if len > top {
            continue;
        }
        let ops = if mode.full { full_ops } else { 1 };
        let label = size_label(len);
        let seed = std::cell::Cell::new(0x5eed_u64);

        // Fresh content: every chunk is new, so every chunk is written.
        rows.push(mode.batched(
            "chunked", format!("insert {label} text, all chunks new"), ops, len as u64, &mut ctx,
            |c| {
                wipe(c)?;
                c.next = 0;
                c.payloads = (0..ops).map(|_| { seed.set(seed.get() + 1); prose(len, seed.get()) }).collect();
                Ok(())
            },
            ChunkCtx::insert_next,
        )?);

        // An edited copy of a stored payload: almost every chunk already exists.
        let base = prose(len, 0xba5e);
        rows.push(mode.batched(
            "chunked", format!("insert {label} text, edited copy of a stored one (chunks shared)"), ops, len as u64, &mut ctx,
            |c| {
                wipe(c)?;
                c.next = 0;
                c.hash.clear();
                let _ = write!(c.hash, "{:064x}", u64::MAX);
                c.db.insert_with_hash("text/plain", &base, &c.hash, 1_000_000)?;
                c.payloads = (0..ops as usize).map(|k| {
                    let mut edited = base.clone();
                    edited.insert(len / 4 + k * 97, b'#');
                    edited
                }).collect();
                Ok(())
            },
            ChunkCtx::insert_next,
        )?);

        // Reassembly of a stored payload.
        rows.push(mode.batched(
            "chunked", format!("reassemble {label} (get_content_by_id)"), ops.max(1) * 10, len as u64, &mut ctx,
            |c| {
                wipe(c)?;
                c.hash.clear();
                let _ = write!(c.hash, "{:064x}", u64::MAX - 1);
                c.base_id = c.db.insert_with_hash("text/plain", &base, &c.hash, 1_000_000)?;
                Ok(())
            },
            |c| match c.db.get_content_by_id(c.base_id) {
                Some((_, bytes)) if bytes.len() == len => Ok(()),
                _ => Err("reassembly returned the wrong payload".to_owned()),
            },
        )?);
    }

    // Deep search: a keyword that appears nowhere forces a pass over every chunked body.
    let search_ops = if mode.full { 20u64 } else { 1 };
    let query = vec!["zqxjk-not-in-the-text".to_owned()];
    let bodies = if mode.full { 16u64 } else { 2 };
    rows.push(mode.batched(
        "chunked", format!("search_metadata over {bodies} x 1 MiB chunked text (no hit)"), search_ops, bodies * 1024 * 1024, &mut ctx,
        |c| {
            wipe(c)?;
            for n in 0..bodies {
                c.hash.clear();
                let _ = write!(c.hash, "{:064x}", n + 7_000_000);
                c.db.insert_with_hash("text/plain", &prose(1024 * 1024, 0x5ea0 + n), &c.hash, 1_000_000)?;
            }
            Ok(())
        },
        |c| if c.db.search_metadata(black_box(&query), 10).is_empty() { Ok(()) } else { Err("the absent keyword matched".to_owned()) },
    )?);
    wipe(&mut ctx)
}

// ---------------------------------------------------------------------------
// Sandbox and entry point
// ---------------------------------------------------------------------------

const SANDBOX_VAR: &str = "Y4P_BENCH_ROOT";

fn sandbox_root(base: &Path) -> PathBuf {
    base.join(format!("y4p-bench-{}", std::process::id()))
}

/// Re-executes this binary inside a private `XDG_*` tree and waits for it.
fn run_in_sandbox() -> Result<ExitCode, String> {
    let base = std::env::var_os("Y4P_BENCH_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let root = sandbox_root(&base);
    for sub in ["data", "cache", "config", "run"] {
        let dir = root.join(sub);
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .map_err(|e| format!("cannot restrict {}: {e}", dir.display()))?;
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this binary: {e}"))?;
    let status = Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(SANDBOX_VAR, &root)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env_remove("Y4P_MAX_HISTORY")
        .status();
    let _ = std::fs::remove_dir_all(&root);
    let status = status.map_err(|e| format!("cannot start the sandboxed run: {e}"))?;
    Ok(if status.success() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

fn run_measurements(root: &Path) -> Result<(), String> {
    let db_path = core::get_db_path();
    if !db_path.starts_with(root) {
        return Err(format!("refusing to run: {} lies outside the sandbox {}", db_path.display(), root.display()));
    }

    let full = std::env::args().any(|arg| arg == "--bench");
    let mode = Mode { full };
    println!(
        "y4p baseline ({}, {}) - queue capacity {}, reader limit {}",
        if full { "full measurement" } else { "smoke: one pass, timings are not meaningful" },
        if cfg!(debug_assertions) { "unoptimised build" } else { "optimised build" },
        core::constants::INGEST_QUEUE_CAPACITY,
        core::constants::INGEST_MAX_CONCURRENT_READERS,
    );
    println!("sandbox: {}\n", root.display());

    let mut rows = Vec::new();
    bench_sha3(&mode, &mut rows);
    bench_mime(&mode, &mut rows);
    bench_payload(&mode, &mut rows);
    bench_queue(&mode, &mut rows)?;
    bench_sqlite(&mode, &mut rows)?;
    // Last on purpose: these groups start threads and move tens of megabytes
    // through the allocator, which would otherwise perturb the allocator state
    // the earlier, older rows were recorded in (a 4 KiB enqueue measured 14 %
    // slower when they ran first).
    bench_splice(&mode, &mut rows);
    bench_ingest(&mode, &mut rows)?;
    bench_transcode(&mode, &mut rows)?;
    let mut facts = Vec::new();
    bench_cdc(&mode, &mut rows, &mut facts);
    bench_pipe(&mode, &mut rows, &mut facts);
    bench_chunked(&mode, &mut rows)?;

    print_header();
    rows.iter().for_each(print_row);
    println!();
    facts.iter().for_each(|fact| println!("- {fact}"));
    Ok(())
}

fn main() -> ExitCode {
    let outcome = match std::env::var_os(SANDBOX_VAR) {
        Some(root) => run_measurements(Path::new(&root)).map(|()| ExitCode::SUCCESS),
        None => run_in_sandbox(),
    };
    match outcome {
        Ok(code) => code,
        Err(message) => {
            eprintln!("bench error: {message}");
            ExitCode::FAILURE
        }
    }
}
