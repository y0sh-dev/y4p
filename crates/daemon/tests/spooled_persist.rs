// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/daemon/tests/spooled_persist.rs

//! End to end, against a real database and cache directory: a large payload is
//! drained from a pipe by `FileSpooler`, queued as a `Payload::Spooled` job and
//! persisted by the real `DbWorker`.
//!
//! It is its own test binary with a single test, because it points the process's
//! `XDG_*` variables at a private directory (`ClipboardDb::open` resolves its
//! paths from them) and nothing else may read the environment meanwhile.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha3::{Digest, Sha3_256};
use y4p_core::payload::{to_hex, Payload};
use y4p_daemon::metrics::DaemonMetrics;
use y4p_daemon::spool::FileSpooler;
use y4p_daemon::worker::{stored_in_cache_file, DbWorker};
use y4p_storage::{ClipboardDb, ContentLocation};
use y4p_wayland::spool::{PayloadSpooler, SpoolOutcome};
use y4p_wayland::state::ClipboardJob;

const HEAD: usize = 4096;
const LIMIT: u64 = 64 << 20;

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i * 13 + i / 7) % 251) as u8 ^ seed).collect()
}

fn digest_of(bytes: &[u8]) -> String {
    to_hex(Sha3_256::digest(bytes).as_slice())
}

/// Spools `bytes` exactly as the ingestion thread would (a head, then a pipe)
/// and returns the job it would queue.
fn spooled_job(spooler: &FileSpooler, mime: &str, bytes: &[u8]) -> ClipboardJob {
    let (head, rest) = bytes.split_at(HEAD);
    let (reader, mut writer) = io::pipe().unwrap();
    let rest = rest.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = writer.write_all(&rest);
    });
    let outcome = spooler.spool(head, fs::File::from(std::os::fd::OwnedFd::from(reader)), LIMIT);
    feeder.join().unwrap();
    let SpoolOutcome::Spooled(file) = outcome else { panic!("payload was not spooled") };
    ClipboardJob::from_payload(mime.to_owned(), Payload::Spooled(Box::new(file)))
}

/// The inode behind a queued spooled job's file, to tell a rename from a copy.
fn spool_inode(job: &ClipboardJob) -> u64 {
    let Payload::Spooled(file) = &job.payload else { panic!("not a spooled job") };
    fs::metadata(file.path()).unwrap().ino()
}

fn wait_for<F: FnMut() -> bool>(what: &str, mut done: F) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

fn row_for(db: &ClipboardDb, mime: &str) -> Option<i64> {
    rows_for(db, mime).into_iter().next()
}

fn rows_for(db: &ClipboardDb, mime: &str) -> Vec<i64> {
    db.fetch_metadata(100).into_iter().filter(|row| row.2 == mime).map(|row| row.0).collect()
}

#[test]
fn spooled_payloads_reach_storage_without_the_heap() {
    let root: PathBuf = std::env::temp_dir().join(format!("y4p-spooled-persist-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (var, sub) in [("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"), ("XDG_CONFIG_HOME", "config"), ("XDG_RUNTIME_DIR", "run")] {
        fs::create_dir_all(root.join(sub)).unwrap();
        // SAFETY: this is the only test in this binary, so no other test thread
        // reads or writes the environment, and the worker and spooler threads
        // below are started only after the variables are set.
        unsafe { std::env::set_var(var, root.join(sub)) };
    }

    let cache_dir = y4p_core::get_cache_dir();
    let spool_dir = y4p_core::spool_dir_path();
    let spooler = FileSpooler::open(spool_dir.clone()).unwrap();
    let metrics = Arc::new(DaemonMetrics::new());
    let worker = DbWorker::spawn(ClipboardDb::open().unwrap(), metrics, false, 100);
    let tx = worker.sender();
    let read_db = ClipboardDb::open().unwrap();

    // --- 1. A large GIF: bypasses transcoding, so it is adopted into the cache ---
    let mut gif = b"GIF89a".to_vec();
    gif.extend(pattern(3 * 1024 * 1024, 1));
    let gif_hash = digest_of(&gif);
    let gif_job = spooled_job(&spooler, "image/gif", &gif);
    let spool_ino = spool_inode(&gif_job);
    tx.send(gif_job).unwrap();
    wait_for("the GIF row", || row_for(&read_db, "image/gif").is_some());

    let id = row_for(&read_db, "image/gif").unwrap();
    let Some((_, ContentLocation::CacheFile(path))) = read_db.locate_content(id) else { panic!("a GIF must live in a cache file") };
    assert_eq!(path, y4p_core::cache_file_path(&gif_hash), "adoption must land exactly where storage looks");
    assert_eq!(fs::read(&path).unwrap(), gif, "the adopted file is the spooled stream, byte for byte");
    assert_eq!(fs::metadata(&path).unwrap().ino(), spool_ino, "the cache file is the spool file renamed, not a copy storage wrote");
    assert_eq!(read_db.get_content_by_id(id).unwrap().1, gif);
    assert_eq!(entries(&spool_dir), Vec::<String>::new(), "nothing is left in the spool directory");

    // --- 2. The same GIF again: deduplicated, no second file, no debris ---
    let before = read_db.fetch_metadata(100).len();
    tx.send(spooled_job(&spooler, "image/gif", &gif)).unwrap();
    // A later, different payload proves the duplicate has been processed (one worker, in order).
    let mut blob = pattern(2 * 1024 * 1024, 2);
    blob[0] = 0x7f;
    let blob_hash = digest_of(&blob);
    tx.send(spooled_job(&spooler, "application/octet-stream", &blob)).unwrap();
    wait_for("the blob row", || row_for(&read_db, "application/octet-stream").is_some());
    assert_eq!(read_db.fetch_metadata(100).len(), before + 1, "the duplicate GIF added no row");
    assert_eq!(entries(&spool_dir), Vec::<String>::new());

    // --- 3. A large non-image: stored in the row from the mapping, never cached ---
    let id = row_for(&read_db, "application/octet-stream").unwrap();
    assert!(matches!(read_db.locate_content(id), Some((_, ContentLocation::InlineBlob(_)))));
    assert_eq!(read_db.get_content_by_id(id).unwrap().1, blob);
    assert!(!y4p_core::cache_file_path(&blob_hash).exists(), "an inline payload must not also be cached");

    // --- 4. A payload storage refuses: nothing may stay behind in the cache ---
    let secret = pattern(1024 * 1024, 3);
    let secret_hash = digest_of(&secret);
    tx.send(spooled_job(&spooler, "image/x-secret", &secret)).unwrap();
    let marker = pattern(10_000, 4);
    tx.send(spooled_job(&spooler, "application/x-marker", &marker)).unwrap();
    wait_for("the marker row", || row_for(&read_db, "application/x-marker").is_some());
    assert!(row_for(&read_db, "image/x-secret").is_none(), "storage must have declined the sensitive MIME");
    assert!(!y4p_core::cache_file_path(&secret_hash).exists(), "a declined payload must not remain as a cache file");
    assert_eq!(entries(&spool_dir), Vec::<String>::new());

    // --- 5. A raster image the converter wants: converted file to file ---
    // `magick` reads the spool file and writes a second one, hashed as it goes;
    // the second file replaces the first and is adopted into the cache.
    if y4p_image::magick::is_available() {
        let mut png = Vec::new();
        y4p_image::magick::run(
            &["-size", "256x256", "plasma:fractal", "-depth", "8", "-set", "comment", "SECRETCOMMENT", "png:-"],
            io::empty(),
            &mut png,
        )
        .unwrap();
        assert!(png.len() > HEAD && png.windows(13).any(|w| w == b"SECRETCOMMENT"), "the fixture must be spooled and carry metadata");
        // What the in-memory road makes of the same image: the file road must agree
        // to the byte, or the same copy would key two different rows.
        let (_, converted) = y4p_image::route::route_for_ingest("image/png".to_owned(), png.clone());
        assert_ne!(converted, png, "the fixture must actually be rewritten");
        let converted_hash = digest_of(&converted);

        tx.send(spooled_job(&spooler, "image/png", &png)).unwrap();
        wait_for("the converted PNG row", || row_for(&read_db, "image/png").is_some());

        let id = row_for(&read_db, "image/png").unwrap();
        let Some((_, ContentLocation::CacheFile(path))) = read_db.locate_content(id) else { panic!("a PNG must live in a cache file") };
        assert_eq!(path, y4p_core::cache_file_path(&converted_hash), "the row is keyed by the digest of the converted bytes");
        assert_eq!(fs::read(&path).unwrap(), converted, "the cache file is the converter's output, byte for byte");
        assert!(!y4p_core::cache_file_path(&digest_of(&png)).exists(), "the original must not be cached as well");
        assert_eq!(entries(&spool_dir), Vec::<String>::new(), "both the original and the converted spool file are gone");
    }

    // --- 5b. An image that does not decode: the original is stored untouched ---
    let mut corrupt = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    corrupt.extend(pattern(200_000, 5));
    let corrupt_hash = digest_of(&corrupt);
    let known_png_rows = rows_for(&read_db, "image/png");
    tx.send(spooled_job(&spooler, "image/png", &corrupt)).unwrap();
    wait_for("the undecodable PNG row", || rows_for(&read_db, "image/png").len() > known_png_rows.len());
    let id = rows_for(&read_db, "image/png").into_iter().find(|id| !known_png_rows.contains(id)).unwrap();
    let Some((_, ContentLocation::CacheFile(path))) = read_db.locate_content(id) else { panic!("a PNG must live in a cache file") };
    assert_eq!(path, y4p_core::cache_file_path(&corrupt_hash), "falling back keys the row by the original's digest");
    assert_eq!(fs::read(&path).unwrap(), corrupt, "no data may be lost when the conversion fails");
    assert_eq!(entries(&spool_dir), Vec::<String>::new(), "a failed conversion leaves no half-written file");

    // --- 6. The cache-file rule copied into the worker matches the storage layer's ---
    let mut writer_db = ClipboardDb::open().unwrap();
    for (n, mime) in ["image/png", "image/gif", "image/svg+xml", "image/webp", "application/x-gif", "application/pdf", "text/plain", "application/octet-stream"]
        .into_iter()
        .enumerate()
    {
        let data = pattern(512 + n, 9);
        let id = writer_db.insert_raw(&format!("{mime}; drift-{n}"), &data).unwrap();
        let in_cache = matches!(read_db.locate_content(id), Some((_, ContentLocation::CacheFile(_))));
        assert_eq!(in_cache, stored_in_cache_file(&format!("{mime}; drift-{n}")), "{mime}: worker and storage disagree");
    }

    drop(tx);
    drop(worker);
    let _ = cache_dir;
    let _ = fs::remove_dir_all(root);
}
