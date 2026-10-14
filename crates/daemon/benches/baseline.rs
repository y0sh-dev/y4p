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
//! What is measured, and why these four groups:
//!
//! | Group   | Subject                                                     |
//! | :------ | :---------------------------------------------------------- |
//! | sha3    | digest and hex fingerprint over 64 B / 4 KiB / 64 KiB / 1 MiB |
//! | mime    | MIME comparison, magic-byte sniffing, `text/uri-list` parsing |
//! | queue   | bounded-channel hand-off, load shedding, reader gate, memory |
//! | sqlite  | insert, insert + rotation, duplicate hit, metadata scans      |
//!
//! Besides time, every row reports allocation calls and bytes per operation
//! (a counting global allocator wraps `System`), the quantity the zero-copy
//! milestones (v0.7.2 - v0.8.0) are meant to reduce.
//!
//! **Safety of the environment.** The SQLite group opens the real storage
//! layer, which resolves its paths from `XDG_*`. The harness therefore never
//! runs in the caller's environment: on start it re-executes itself with
//! `XDG_*` pointing into a fresh private directory and deletes that directory
//! afterwards, and the child refuses to run unless the database path really
//! lies inside it. The user's clipboard history cannot be read or modified.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicUsize, AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha3::{Digest, Sha3_256};
use y4p_core as core;
use y4p_storage::ClipboardDb;
use y4p_wayland::ingest::{self, Enqueue, ReaderGate};
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
            return Row { group, name: name.into(), median_ns: 0.0, min_ns: 0.0, payload_bytes, allocs_per_op: allocs, alloc_bytes_per_op: alloc_bytes };
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
        Row { group, name: name.into(), median_ns: median(&mut samples), min_ns, payload_bytes, allocs_per_op: allocs, alloc_bytes_per_op: alloc_bytes }
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
        Ok(Row { group, name: name.into(), median_ns, min_ns: if self.full { min_ns } else { 0.0 }, payload_bytes, allocs_per_op: allocs, alloc_bytes_per_op: alloc_bytes })
    }
}

fn print_header() {
    println!("| group | case | median ns/op | min ns/op | MiB/s | allocs/op | alloc B/op |");
    println!("| :--- | :--- | ---: | ---: | ---: | ---: | ---: |");
}

fn print_row(row: &Row) {
    let rate = row.mib_per_s().map_or_else(|| "-".to_owned(), |v| format!("{v:.0}"));
    println!(
        "| {} | {} | {:.1} | {:.1} | {} | {:.1} | {:.0} |",
        row.group, row.name, row.median_ns, row.min_ns, rate, row.allocs_per_op, row.alloc_bytes_per_op
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
    ClipboardJob { mime: "text/plain".to_owned(), data: vec![0x61; len] }
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
    bench_queue(&mode, &mut rows)?;
    bench_sqlite(&mode, &mut rows)?;

    print_header();
    rows.iter().for_each(print_row);
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
