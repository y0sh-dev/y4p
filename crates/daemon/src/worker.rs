// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/daemon/src/worker.rs

use crate::core::constants::*;
use crate::metrics::DaemonMetrics;
use crate::storage::ClipboardDb;
use crate::wayland::state::ClipboardJob;
use sha3::{Digest, Sha3_256};
use std::sync::{mpsc, Arc};

/// Owns the single writer connection and its background thread. All
/// persistence goes through this one channel, so SQLite only ever sees one
/// writer.
///
/// Image transcoding and fingerprinting happen on this thread too (see
/// `process_job`): the Wayland layer hands over normalised raw payloads and
/// never blocks on `magick`. Jobs are therefore also persisted strictly in
/// arrival order.
///
/// A job's payload is materialised on this thread, just before `process_job`,
/// by `ClipboardJob::into_bytes` under the same `MAX_PAYLOAD_BYTES` bound as
/// ingestion. Ingestion currently always sends an in-memory payload, which that
/// call hands over without a copy; a descriptor- or file-backed one is read
/// here, where blocking is expected. A payload that cannot be materialised is
/// logged and dropped; it never stops the worker.
///
/// The queue is bounded (`INGEST_QUEUE_CAPACITY`): when persistence, image
/// conversion included, falls behind, producers wait and then shed rather than
/// letting payloads accumulate (see `y4p_wayland::ingest`).
pub struct DbWorker {
    tx: mpsc::SyncSender<ClipboardJob>,
}

impl DbWorker {
    pub fn spawn(mut db: ClipboardDb, metrics: Arc<DaemonMetrics>, verbose: bool, max_history: usize) -> Self {
        let (tx, rx) = mpsc::sync_channel::<ClipboardJob>(INGEST_QUEUE_CAPACITY);

        std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                let Some((mime, data)) = materialise(job) else { continue };
                let (mime, data, hash) = process_job(mime, data);
                match db.insert_with_hash(&mime, &data, &hash, max_history) {
                    Ok(_) => {
                        metrics.record_ingress();
                        if verbose {
                            println!("{}", log_save(&mime, data.len()));
                        }
                    }
                    Err(e) => eprintln!("{}worker failed to persist data: {}", LOG_ERROR, e),
                }
            }
        });

        Self { tx }
    }

    /// Clone of the submission channel, handed to the ingestion path so jobs
    /// can be queued without touching the worker thread directly.
    pub fn sender(&self) -> mpsc::SyncSender<ClipboardJob> {
        self.tx.clone()
    }
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

/// Turns a normalised raw payload into what will actually be persisted, in a
/// fixed order: route through the image pipeline, then hash.
///
/// The order matters. Raster images are privacy-stripped and re-encoded by
/// `image::route::route_for_ingest`, which may change both the MIME and every
/// byte; the fingerprint must describe what the database and the cache file
/// really store, so it is computed last, over the final bytes. (Hashing
/// earlier would key the row by bytes that are never saved, and the same
/// image copied twice would still dedupe only by accident.) Routing falls
/// back to the untouched original on any failure, so the hash then covers
/// the original instead — consistent either way.
///
/// Returns `(mime, data, hash)`.
fn process_job(mime: String, data: Vec<u8>) -> (String, Vec<u8>, String) {
    let (mime, data) = crate::image::route::route_for_ingest(mime, data);
    let hash = fingerprint(&data);
    (mime, data, hash)
}

/// Lowercase-hex SHA3-256 fingerprint of `data`.
fn fingerprint(data: &[u8]) -> String {
    let mut hasher = Sha3_256::new();
    hasher.update(data);
    hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // --- materialise (payload -> bytes) ---

    use crate::core::payload::{Payload, StreamDescriptor};
    use std::io::Write;
    use std::os::fd::OwnedFd;

    fn stream_job(mime: &str, bytes: &[u8], hint: Option<u64>) -> ClipboardJob {
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(bytes).unwrap();
        drop(writer);
        ClipboardJob::from_payload(mime.to_owned(), Payload::Stream(StreamDescriptor::new(OwnedFd::from(reader), hint)))
    }

    #[test]
    fn an_in_memory_job_is_handed_over_without_a_copy() {
        let data = b"hello clipboard".to_vec();
        let ptr = data.as_ptr();
        let (mime, out) = materialise(ClipboardJob::new("text/plain".to_owned(), data)).unwrap();
        assert_eq!(mime, "text/plain");
        assert_eq!(out.as_ptr(), ptr, "the worker must not copy an in-memory payload");
    }

    #[test]
    fn a_streamed_job_is_read_in_full_then_processed_like_any_other() {
        let (mime, data) = materialise(stream_job("text/plain", b"streamed text", Some(13))).unwrap();
        let (mime2, data2, hash) = process_job(mime, data);
        assert_eq!((mime2.as_str(), data2.as_slice()), ("text/plain", b"streamed text".as_slice()));
        assert_eq!(hash, fingerprint(b"streamed text"));
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
        assert!(materialise(stream_job("text/plain", b"", None)).is_none(), "an empty stream is not a payload");
        assert!(materialise(ClipboardJob::new("text/plain".to_owned(), Vec::new())).is_none());
        let missing = ClipboardJob::from_payload("image/png".to_owned(), Payload::File("/definitely/not/there".into()));
        assert!(materialise(missing).is_none());
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
        let (mime, data, hash) = process_job("text/plain".to_string(), raw.clone());
        assert_eq!(mime, "text/plain");
        assert_eq!(data, raw);
        assert_eq!(hash, fingerprint(&raw));
    }

    #[test]
    fn process_job_svg_bypasses_the_image_pipeline_unchanged() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"5\" height=\"5\"/>".to_vec();
        let (mime, data, hash) = process_job("image/svg+xml".to_string(), svg.clone());
        assert_eq!(mime, "image/svg+xml");
        assert_eq!(data, svg);
        assert_eq!(hash, fingerprint(&svg));
    }

    #[test]
    fn process_job_uri_list_bypasses_image_routing_and_is_hashed() {
        // Already normalised upstream: one path per line. A path that merely
        // *names* a PNG is still text and must never reach the transcoder.
        let (mime, data, hash) = process_job(MIME_URI_LIST.to_string(), b"/a.png".to_vec());
        assert_eq!(mime, MIME_URI_LIST);
        assert_eq!(data, b"/a.png".to_vec());
        assert_eq!(hash, fingerprint(b"/a.png"));
    }

    #[test]
    fn process_job_png_is_rewritten_as_png_and_the_hash_covers_the_converted_bytes() {
        if !magick_available() { return; }
        let png = generated(&["-size", "32x32", "plasma:fractal", "-depth", "8", "png:-"]);
        let (mime, data, hash) = process_job("image/png".to_string(), png.clone());
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
        let (_, _, first) = process_job("image/png".to_string(), png.clone());
        let (_, _, second) = process_job("image/png".to_string(), png);
        assert_eq!(first, second, "deduplication relies on a deterministic conversion");
    }

    #[test]
    fn process_job_corrupt_png_falls_back_to_the_original_bytes_mime_and_hash() {
        if !magick_available() { return; }
        let mut corrupt = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        corrupt.extend_from_slice(&[0xA5; 2048]);
        let (mime, data, hash) = process_job("image/png".to_string(), corrupt.clone());
        assert_eq!(mime, "image/png");
        assert_eq!(data, corrupt);
        assert_eq!(hash, fingerprint(&corrupt));
    }

    #[test]
    fn process_job_mislabelled_image_is_reidentified_before_routing() {
        if !magick_available() { return; }
        let png = generated(&["-size", "16x16", "xc:red", "png:-"]);
        let (mime, data, _) = process_job("application/octet-stream".to_string(), png.clone());
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
            let (out_mime, data, hash) = process_job(mime.to_string(), payload);
            assert_eq!(out_mime, "image/png", "{label}");
            assert_eq!(&data[..4], b"\x89PNG", "{label}");
            assert_eq!(hash, fingerprint(&data), "{label}");
        }
    }

    #[test]
    fn process_job_gif_keeps_its_bytes_and_mime_so_animation_survives() {
        let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec();
        let (mime, data, hash) = process_job("image/gif".to_string(), gif.clone());
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
        let (mime, data, hash) = process_job("image/webp".to_string(), webp.clone());
        assert_eq!(mime, "image/webp");
        assert_eq!(data, webp);
        assert_eq!(hash, fingerprint(&webp));
    }
}
