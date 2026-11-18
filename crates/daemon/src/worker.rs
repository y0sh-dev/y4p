// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/daemon/src/worker.rs

use crate::core::constants::*;
use crate::core::payload::to_hex;
use crate::metrics::DaemonMetrics;
use crate::storage::ClipboardDb;
use crate::wayland::state::ClipboardJob;
use sha3::{Digest, Sha3_256};
use std::sync::{mpsc, Arc, Mutex};

/// Owns the single writer connection and its background thread. All
/// persistence goes through this one channel, so SQLite only ever sees one
/// writer.
///
/// Fingerprinting happens on this thread too (see `process_job`): the Wayland
/// layer hands over normalised raw payloads and never blocks on `magick`.
/// Image sanitisation is opt-in (`[image] sanitize = true`); when it is off,
/// the raw bytes are stored unchanged and no converter runs. Jobs are therefore
/// also persisted strictly in arrival order.
///
/// A job's payload is materialised on this thread, just before `process_job`,
/// by `ClipboardJob::into_bytes` under the same `MAX_PAYLOAD_BYTES` bound as
/// ingestion. An in-memory payload is handed over without a copy; a
/// file-backed one (egress or a synthetic job, never the ingestion path) is
/// read here, where blocking is expected. A payload that cannot be materialised
/// is logged and dropped; it never stops the worker.
///
/// The queue is bounded by `INGEST_QUEUE_CAPACITY` payloads: when persistence,
/// image conversion included, falls behind, producers wait and then shed
/// rather than letting payloads accumulate (see `y4p_wayland::ingest`).
///
/// The connection itself is behind an `Arc<Mutex<..>>` rather than owned by the
/// thread, because the daemon's event loop must be able to `delete` and `wipe`
/// on the very same connection: SQLite sees a strictly serialised writer, and a
/// `wipe` releases memory (`PRAGMA shrink_memory`, `malloc_trim(3)`) in the
/// process that actually holds the pages. Every mutation therefore waits behind
/// whatever job the worker is persisting, and vice versa.
pub struct DbWorker {
    tx: mpsc::SyncSender<ClipboardJob>,
    db: Arc<Mutex<ClipboardDb>>,
}

impl DbWorker {
    pub fn spawn(db: ClipboardDb, metrics: Arc<DaemonMetrics>, verbose: bool, max_history: usize, sanitize_images: bool) -> Self {
        let (tx, rx) = mpsc::sync_channel::<ClipboardJob>(INGEST_QUEUE_CAPACITY);
        let db = Arc::new(Mutex::new(db));
        let writer = Arc::clone(&db);

        std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                let ClipboardJob { mime, payload } = job;
                let stored = match writer.lock() {
                    Ok(mut db) => store_materialised(&mut db, ClipboardJob::from_payload(mime, payload), max_history, sanitize_images),
                    // Only reachable if a writer panicked mid-transaction. The
                    // job is dropped rather than the worker stopped, exactly as
                    // an unstorable payload is.
                    Err(_) => Err("database lock poisoned; payload dropped".to_string()),
                };
                match stored {
                    Ok(Some((mime, len))) => {
                        metrics.record_ingress();
                        if verbose {
                            println!("{}", log_save(&mime, len));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("{}worker failed to persist data: {}", LOG_ERROR, e),
                }
            }
        });

        Self { tx, db }
    }

    /// Clone of the submission channel, handed to the ingestion path so jobs
    /// can be queued without touching the worker thread directly.
    pub fn sender(&self) -> mpsc::SyncSender<ClipboardJob> {
        self.tx.clone()
    }

    /// Removes one record on the writer connection, used by the IPC `delete`
    /// command. `Ok(false)` means no such row.
    pub fn delete_by_id(&self, id: i64) -> Result<bool, String> {
        self.with_writer(|db| db.delete_by_id(id).map_err(|e| e.to_string()))
    }

    /// Purges the whole history on the writer connection, used by the IPC
    /// `wipe` command, so the `VACUUM` and heap release happen in the daemon:
    /// the process whose memory is actually holding the cache.
    pub fn wipe(&self) -> Result<(), String> {
        self.with_writer(|db| db.wipe().map_err(|e| e.to_string()))
    }

    /// Runs `f` with exclusive access to the writer connection. `Mutex::lock`
    /// is fallible, and a poisoned lock is reported rather than unwrapped: the
    /// daemon must not abort because one writer panicked.
    fn with_writer<T>(&self, f: impl FnOnce(&mut ClipboardDb) -> Result<T, String>) -> Result<T, String> {
        let mut db = self.db.lock().map_err(|_| "database lock poisoned; mutation not applied".to_string())?;
        f(&mut db)
    }
}

/// What was saved: the MIME it was recorded under and its size in bytes.
type Saved = (String, usize);

/// The in-memory road: materialise, optionally route through the image
/// pipeline, hash, insert. `Ok(None)` means the payload could not be
/// materialised (logged).
fn store_materialised(db: &mut ClipboardDb, job: ClipboardJob, max_history: usize, sanitize_images: bool) -> Result<Option<Saved>, String> {
    let Some((mime, data)) = materialise(job) else { return Ok(None) };
    let (mime, data, hash) = process_job(mime, data, sanitize_images);
    db.insert_with_hash(&mime, &data, &hash, max_history)?;
    Ok(Some((mime, data.len())))
}

/// Reads a job's payload into memory under the ingestion bound, or logs why it
/// cannot and yields `None` so the worker moves on to the next job.
fn materialise(job: ClipboardJob) -> Option<(String, Vec<u8>)> {
    match job.into_bytes(MAX_PAYLOAD_BYTES) {
        Ok(materialised) => Some(materialised),
        Err(e) => {
            eprintln!("{}worker discarded a payload: {}", LOG_ERROR, e);
            None
        }
    }
}

/// Turns a normalised raw payload into what will actually be persisted:
/// optionally route through the image pipeline, then hash.
///
/// When `sanitize_images` is set, raster images are privacy-stripped and
/// re-encoded by `image::route::route_for_ingest`, which may change both the
/// MIME and every byte; the fingerprint must describe what the database and
/// the cache file really store, so it is computed last, over the final bytes.
/// (Hashing earlier would key the row by bytes that are never saved, and the
/// same image copied twice would still dedupe only by accident.) Routing falls
/// back to the untouched original on any failure, so the hash then covers the
/// original instead — consistent either way.
///
/// When it is off (the default), the raw bytes are persisted exactly as they
/// arrived: no converter is spawned and the hash keys those bytes.
///
/// Returns `(mime, data, hash)`.
fn process_job(mime: String, data: Vec<u8>, sanitize_images: bool) -> (String, Vec<u8>, String) {
    let (mime, data) = if sanitize_images {
        crate::image::route::route_for_ingest(mime, data)
    } else {
        (mime, data)
    };
    let hash = fingerprint(&data);
    (mime, data, hash)
}

/// Lowercase-hex SHA3-256 fingerprint of `data`.
fn fingerprint(data: &[u8]) -> String {
    let mut hasher = Sha3_256::new();
    hasher.update(data);
    to_hex(hasher.finalize().as_slice())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // --- materialise (payload -> bytes) ---

    use crate::core::payload::Payload;

    #[test]
    fn an_in_memory_job_is_handed_over_without_a_copy() {
        let data = b"hello clipboard".to_vec();
        let ptr = data.as_ptr();
        let (mime, out) = materialise(ClipboardJob::new("text/plain".to_owned(), data)).unwrap();
        assert_eq!(mime, "text/plain");
        assert_eq!(out.as_ptr(), ptr, "the worker must not copy an in-memory payload");
    }

    #[test]
    fn a_file_backed_job_is_read_from_disk() {
        let mut path = std::env::temp_dir();
        path.push(format!("y4p-worker-test-{}", std::process::id()));
        std::fs::write(&path, b"on disk").unwrap();
        let job = ClipboardJob::from_payload("text/plain".to_owned(), Payload::File(path.clone()));
        assert_eq!(materialise(job).unwrap().1, b"on disk");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_job_that_cannot_be_read_is_dropped_so_the_worker_carries_on() {
        assert!(materialise(ClipboardJob::new("text/plain".to_owned(), Vec::new())).is_none(), "an empty buffer is not a payload");
        let missing = ClipboardJob::from_payload("image/png".to_owned(), Payload::File("/definitely/not/there".into()));
        assert!(materialise(missing).is_none());
    }

    #[test]
    fn the_hex_fingerprint_matches_the_formatted_reference_for_every_byte_value() {
        // The previous implementation formatted each byte with `{:02x}`; the
        // single-allocation encoder must produce the same text.
        let data: Vec<u8> = (0..=255u8).collect();
        let mut hasher = Sha3_256::new();
        hasher.update(&data);
        let reference = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>();
        assert_eq!(fingerprint(&data), reference);
        assert_eq!(fingerprint(b"").len(), 64);
    }

    // --- process_job (image routing -> hash) ---
    //
    // Payloads reach `process_job` already normalised by the Wayland layer
    // (URI lists decoded, mislabelled images re-identified by magic bytes);
    // the image router additionally sniffs the bytes itself, so a mislabelled
    // image is still recognised here.

    fn magick_available() -> bool {
        if crate::image::magick::is_available() {
            true
        } else {
            eprintln!("skipping: `magick` is not available on this system");
            false
        }
    }

    fn generated(args: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        crate::image::magick::run(args, std::io::empty(), &mut out).unwrap();
        out
    }

    #[test]
    fn process_job_plain_text_is_unchanged_and_hashed_as_is() {
        let raw = b"hello clipboard".to_vec();
        let (mime, data, hash) = process_job("text/plain".to_string(), raw.clone(), true);
        assert_eq!(mime, "text/plain");
        assert_eq!(data, raw);
        assert_eq!(hash, fingerprint(&raw));
    }

    #[test]
    fn process_job_with_sanitisation_off_stores_a_raster_image_byte_for_byte() {
        if !magick_available() { return; }
        let png = generated(&["-size", "32x32", "plasma:fractal", "-depth", "8", "png:-"]);
        let (mime, data, hash) = process_job("image/png".to_string(), png.clone(), false);
        assert_eq!(mime, "image/png");
        assert_eq!(data, png, "sanitisation is off: no byte may change");
        assert_eq!(hash, fingerprint(&png), "the hash keys the raw bytes");
    }

    #[test]
    fn process_job_svg_bypasses_the_image_pipeline_unchanged() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"5\" height=\"5\"/>".to_vec();
        let (mime, data, hash) = process_job("image/svg+xml".to_string(), svg.clone(), true);
        assert_eq!(mime, "image/svg+xml");
        assert_eq!(data, svg);
        assert_eq!(hash, fingerprint(&svg));
    }

    #[test]
    fn process_job_uri_list_bypasses_image_routing_and_is_hashed() {
        // Already normalised upstream: one path per line. A path that merely
        // *names* a PNG is still text and must never reach the transcoder.
        let (mime, data, hash) = process_job(MIME_URI_LIST.to_string(), b"/a.png".to_vec(), true);
        assert_eq!(mime, MIME_URI_LIST);
        assert_eq!(data, b"/a.png".to_vec());
        assert_eq!(hash, fingerprint(b"/a.png"));
    }

    #[test]
    fn process_job_png_is_rewritten_as_png_and_the_hash_covers_the_converted_bytes() {
        if !magick_available() { return; }
        let png = generated(&["-size", "32x32", "plasma:fractal", "-depth", "8", "png:-"]);
        let (mime, data, hash) = process_job("image/png".to_string(), png.clone(), true);
        assert_eq!(mime, "image/png");
        assert_eq!(&data[..8], b"\x89PNG\r\n\x1a\n");
        assert_ne!(data, png, "the stored bytes are the stripped re-encode, not the original");
        assert_eq!(hash, fingerprint(&data), "fingerprint must describe the stored bytes");
        assert_ne!(hash, fingerprint(&png), "fingerprint must not be that of the discarded original");
    }

    #[test]
    fn process_job_same_image_copied_twice_yields_the_same_fingerprint() {
        if !magick_available() { return; }
        let png = generated(&["-size", "32x32", "plasma:fractal", "-depth", "8", "png:-"]);
        let (_, _, first) = process_job("image/png".to_string(), png.clone(), true);
        let (_, _, second) = process_job("image/png".to_string(), png, true);
        assert_eq!(first, second, "deduplication relies on a deterministic conversion");
    }

    #[test]
    fn process_job_corrupt_png_falls_back_to_the_original_bytes_mime_and_hash() {
        if !magick_available() { return; }
        let mut corrupt = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        corrupt.extend_from_slice(&[0xA5; 2048]);
        let (mime, data, hash) = process_job("image/png".to_string(), corrupt.clone(), true);
        assert_eq!(mime, "image/png");
        assert_eq!(data, corrupt);
        assert_eq!(hash, fingerprint(&corrupt));
    }

    #[test]
    fn process_job_mislabelled_image_is_reidentified_before_routing() {
        if !magick_available() { return; }
        let png = generated(&["-size", "16x16", "xc:red", "png:-"]);
        let (mime, data, _) = process_job("application/octet-stream".to_string(), png.clone(), true);
        assert_eq!(mime, "image/png", "an unlabelled image must leave as a PNG");
        assert_ne!(data, png, "...and only the converter's output carries that label");
    }

    #[test]
    fn process_job_every_raster_input_but_gif_is_stored_as_png_and_never_as_webp() {
        if !magick_available() { return; }
        let png = generated(&["-size", "16x16", "plasma:fractal", "-depth", "8", "png:-"]);
        let jpeg = generated(&["-size", "16x16", "plasma:fractal", "jpg:-"]);
        let webp = generated(&["-size", "16x16", "plasma:fractal", "webp:-"]);
        for (label, mime, payload) in [("png", "image/png", png), ("jpeg", "image/jpeg", jpeg), ("webp", "image/webp", webp)] {
            let (out_mime, data, hash) = process_job(mime.to_string(), payload, true);
            assert_eq!(out_mime, "image/png", "{label}");
            assert_eq!(&data[..4], b"\x89PNG", "{label}");
            assert_eq!(hash, fingerprint(&data), "{label}");
        }
    }

    #[test]
    fn process_job_gif_keeps_its_bytes_and_mime_so_animation_survives() {
        let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec();
        let (mime, data, hash) = process_job("image/gif".to_string(), gif.clone(), true);
        assert_eq!(mime, "image/gif");
        assert_eq!(data, gif);
        assert_eq!(hash, fingerprint(&gif));
    }

    #[test]
    fn process_job_animated_webp_keeps_its_bytes_and_mime_so_animation_survives() {
        let mut webp = vec![0u8; 24];
        webp[0..4].copy_from_slice(b"RIFF");
        webp[8..12].copy_from_slice(b"WEBP");
        webp[12..16].copy_from_slice(b"VP8X");
        webp[20] = 0x02; // Animation flag
        let (mime, data, hash) = process_job("image/webp".to_string(), webp.clone(), true);
        assert_eq!(mime, "image/webp");
        assert_eq!(data, webp);
        assert_eq!(hash, fingerprint(&webp));
    }

    // --- serialised mutations on the writer connection ---

    use crate::test_support::PrivateStore;

    /// A worker over a private store seeded through `seed`, returning it
    /// alongside the worker (the store must outlive the worker's paths).
    fn worker_over(store: &PrivateStore, seed: &[&[u8]]) -> DbWorker {
        let mut db = store.open();
        for payload in seed {
            db.insert_raw("text/plain", payload).unwrap();
        }
        DbWorker::spawn(db, Arc::new(DaemonMetrics::new()), false, 100, false)
    }

    #[test]
    fn delete_by_id_runs_on_the_workers_own_connection() {
        let store = PrivateStore::new("worker-delete");
        let worker = worker_over(&store, &[b"kept", b"removed"]);
        let (kept, target) = match store.rows().as_slice() {
            [first, second, ..] => (*first, *second),
            other => panic!("expected two rows, found {}", other.len()),
        };

        assert!(worker.delete_by_id(target).unwrap(), "the row was there and is now deleted");
        assert!(!worker.delete_by_id(target).unwrap(), "and a second delete finds nothing");
        assert_eq!(store.rows(), vec![kept], "only the other row survives");
    }

    #[test]
    fn wipe_empties_the_store_through_the_worker() {
        let store = PrivateStore::new("worker-wipe");
        let worker = worker_over(&store, &[b"first", b"second"]);
        assert_eq!(store.rows().len(), 2);

        worker.wipe().unwrap();

        assert!(store.rows().is_empty(), "every row is gone");
        // The connection survives the `VACUUM` and stays usable, which is what
        // lets the daemon keep ingesting after a wipe.
        let mut db = store.open();
        assert!(db.insert_raw("text/plain", b"after the wipe").unwrap() > 0);
    }

    #[test]
    fn a_queued_job_and_a_mutation_never_interleave_on_the_connection() {
        let store = PrivateStore::new("worker-serialised");
        let metrics = Arc::new(DaemonMetrics::new());
        let worker = DbWorker::spawn(store.open(), Arc::clone(&metrics), false, 100, false);
        let tx = worker.sender();

        // The store is empty, so the queued payload will be row 1. Deleting it
        // concurrently pins down the race: either order is legitimate, but the
        // mutation must never observe a half-written row, and the connection
        // must stay usable afterwards.
        tx.send(ClipboardJob::new("text/plain".to_owned(), b"queued".to_vec())).unwrap();
        let deleted = worker.delete_by_id(1).unwrap();

        // The worker records the payload only after committing it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while metrics.ingress_count() == 0 {
            assert!(std::time::Instant::now() < deadline, "the queued job never completed");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let rows = store.rows();
        assert_eq!(rows.len(), usize::from(!deleted), "the row survives exactly when the delete came first");
        assert!(rows.iter().all(|id| *id == 1), "the worker assigned one row, not two");
        drop(tx);
    }
}
