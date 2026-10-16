// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/daemon/src/worker.rs

use crate::core::constants::*;
use crate::core::mapped::MappedFile;
use crate::core::payload::{to_hex, Payload, SpooledFile};
use crate::image::route::{route_file_for_ingest, FileRoute, SNIFF_HEAD_BYTES};
use crate::metrics::DaemonMetrics;
use crate::spool;
use crate::storage::ClipboardDb;
use crate::wayland::state::ClipboardJob;
use sha3::{Digest, Sha3_256};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
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
/// ingestion. An in-memory payload is handed over without a copy; a descriptor-
/// or file-backed one is read here, where blocking is expected. A payload that
/// cannot be materialised is logged and dropped; it never stops the worker.
///
/// A *spooled* payload (a large one the ingestion thread already drained to a
/// file, fingerprinting it on the way) takes a different road, `store_spooled`:
/// its bytes never enter the heap at all. Not even a raster image the image
/// pipeline rewrites: `magick` reads the spool file directly and writes a new
/// one, fingerprinted as it goes.
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
                let ClipboardJob { mime, payload } = job;
                let stored = match payload {
                    Payload::Spooled(spooled) => store_spooled(&mut db, mime, *spooled, max_history),
                    payload => store_materialised(&mut db, ClipboardJob::from_payload(mime, payload), max_history),
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

        Self { tx }
    }

    /// Clone of the submission channel, handed to the ingestion path so jobs
    /// can be queued without touching the worker thread directly.
    pub fn sender(&self) -> mpsc::SyncSender<ClipboardJob> {
        self.tx.clone()
    }
}

/// What was saved: the MIME it was recorded under and its size in bytes.
type Saved = (String, usize);

/// The in-memory road: materialise, route through the image pipeline, hash,
/// insert. `Ok(None)` means the payload could not be materialised (logged).
fn store_materialised(db: &mut ClipboardDb, job: ClipboardJob, max_history: usize) -> Result<Option<Saved>, String> {
    let Some((mime, data)) = materialise(job) else { return Ok(None) };
    let (mime, data, hash) = process_job(mime, data);
    db.insert_with_hash(&mime, &data, &hash, max_history)?;
    Ok(Some((mime, data.len())))
}

/// The streaming road, for a payload already on disk with its fingerprint.
///
/// 1. The image pipeline gets its say first ([`convert_if_wanted`]): a raster
///    image it would rewrite is converted file to file, and the converted
///    spool file (with the digest of *its* bytes) replaces the original, which
///    is deleted. Anything else, and any conversion that is skipped or fails,
///    carries on with the original untouched.
/// 2. The payload is then stored exactly as that file holds it, and the digest
///    computed while it streamed in is the database key: nothing is hashed
///    again. The storage layer wants a `&[u8]`, so the spool file is mapped
///    (page cache, no heap, and untouched pages are never even read). A
///    payload that storage keeps in a cache file is *adopted*: the spool file
///    is renamed to its final `<hash>.cache` name before the insert, so
///    storage finds it in place and writes nothing. Everything else is
///    inserted from the mapping, and the spool file is then deleted.
///
/// A mapping that cannot be made falls back to reading the file; a failed or
/// skipped insert takes an adopted file back out of the cache again.
fn store_spooled(db: &mut ClipboardDb, mime: String, spooled: SpooledFile, max_history: usize) -> Result<Option<Saved>, String> {
    let (mime, spooled) = convert_if_wanted(mime, spooled);

    let hash = spooled.hex_digest();
    // SAFETY: the spool file is private to this process: created `0600` in a
    // `0700` directory under a unique name by `spool::FileSpooler`, written only
    // by that copy (which has finished: the payload was queued only afterwards),
    // and from here on renamed or unlinked, never written or truncated, by this
    // thread alone. `MappedFile::map`'s contract is met.
    let mapped = unsafe { MappedFile::map(spooled.path()) };
    let mapped = match mapped {
        Ok(m) if m.len() as u64 == spooled.len() => m,
        // Unmappable (or not the recorded size): read it, keeping the digest.
        _ => {
            let data = Payload::Spooled(Box::new(spooled)).read_bounded(MAX_PAYLOAD_BYTES).map_err(|e| e.to_string())?;
            db.insert_with_hash(&mime, &data, &hash, max_history)?;
            return Ok(Some((mime, data.len())));
        }
    };

    let adopted = adopt_into_cache(&mime, spooled.path(), &crate::core::cache_file_path(&hash));
    if adopted.is_some() {
        let _ = spooled.into_path();
    }
    let inserted = db.insert_with_hash(&mime, &mapped, &hash, max_history);
    // `Ok(-1)` is storage declining the payload (a sensitive MIME): no row
    // references the file, so an adopted one must not stay behind as debris.
    if let Some(path) = adopted
        && !matches!(inserted, Ok(id) if id >= 0)
    {
        let _ = fs::remove_file(path);
    }
    inserted.map(|_| Some((mime, mapped.len())))
}

/// Runs a spooled payload through the image pipeline without reading it into
/// memory: the file-backed twin of `process_job`'s routing step.
///
/// The decision is made from the first bytes alone. If a conversion is due and
/// the breaker and the throttle allow it, `magick` reads the spool file as its
/// stdin and writes a new spool file, hashed on the way (`spool::transcode_spooled`);
/// the payload then *is* that new file, under the converter's MIME, and the
/// original is unlinked here. In every other case (nothing to convert, circuit
/// open, no free slot, `magick` missing, a decode failure, a timeout) the pair
/// comes back unchanged and the original file is stored as it arrived: the same
/// "never lose data, never make things worse" contract as the in-memory road,
/// with the same breaker, throttle and limits (`y4p_image::route`).
fn convert_if_wanted(mime: String, spooled: SpooledFile) -> (String, SpooledFile) {
    let head = read_head(spooled.path());
    let routed = route_file_for_ingest(&mime, &head, |job| spool::transcode_spooled(&spooled, job, MAX_PAYLOAD_BYTES));
    match routed {
        // `spooled` is dropped on return, which unlinks the superseded original.
        FileRoute::Converted { mime: converted, output } => (converted.to_owned(), output),
        FileRoute::Original => (mime, spooled),
    }
}

/// Whether `crates/storage` keeps a payload of this MIME in a cache file rather
/// than in the database row (`SqliteStore::upsert_record`'s `is_image`). The
/// storage layer is frozen and keeps the rule private; an integration test pins
/// this copy to what storage actually does.
pub fn stored_in_cache_file(mime: &str) -> bool {
    mime.starts_with("image/") || mime.contains("gif")
}

/// Renames `spool` to `target` when `mime` is a cache-file payload and nothing
/// is there yet. Returns `target` when the file was adopted, so a failed insert
/// can take it back out; `None` when it was not (the caller then lets the spool
/// file be deleted, and storage finds or writes its own cache file).
///
/// Never replaces an existing file: that one is the same content under the same
/// hash and may already back a row.
fn adopt_into_cache(mime: &str, spool: &Path, target: &Path) -> Option<PathBuf> {
    if !stored_in_cache_file(mime) || target.exists() {
        return None;
    }
    fs::rename(spool, target).ok().map(|()| target.to_path_buf())
}

/// The first bytes of a spool file, enough for the image router to classify it.
/// Empty if the file cannot be read, which classifies as "nothing to convert".
fn read_head(path: &Path) -> Vec<u8> {
    let mut head = Vec::with_capacity(SNIFF_HEAD_BYTES);
    if let Ok(file) = File::open(path) {
        let _ = file.take(SNIFF_HEAD_BYTES as u64).read_to_end(&mut head);
    }
    head
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
    to_hex(hasher.finalize().as_slice())
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

    // --- spooled road: classification, adoption, head sniffing ---

    fn scratch(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("y4p-worker-spool-{}-{}", std::process::id(), tag));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn only_image_like_mimes_live_in_cache_files() {
        for mime in ["image/png", "image/gif", "image/svg+xml", "image/webp", "application/x-gif"] {
            assert!(stored_in_cache_file(mime), "{mime}");
        }
        for mime in ["text/plain", "application/pdf", "application/octet-stream", "text/uri-list", ""] {
            assert!(!stored_in_cache_file(mime), "{mime}");
        }
    }

    #[test]
    fn an_image_spool_file_is_renamed_into_place_not_copied() {
        let spool = scratch("adopt-src");
        let target = scratch("adopt-dst");
        fs::write(&spool, b"gif bytes").unwrap();
        let ino = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&spool).unwrap());

        assert_eq!(adopt_into_cache("image/gif", &spool, &target), Some(target.clone()));

        assert!(!spool.exists(), "the spool name is gone");
        assert_eq!(fs::read(&target).unwrap(), b"gif bytes");
        assert_eq!(std::os::unix::fs::MetadataExt::ino(&fs::metadata(&target).unwrap()), ino, "same inode: renamed, not copied");
        fs::remove_file(target).unwrap();
    }

    #[test]
    fn adoption_never_replaces_an_existing_cache_file() {
        let spool = scratch("keep-src");
        let target = scratch("keep-dst");
        fs::write(&spool, b"new").unwrap();
        fs::write(&target, b"existing").unwrap();

        assert_eq!(adopt_into_cache("image/gif", &spool, &target), None);

        assert_eq!(fs::read(&target).unwrap(), b"existing");
        assert!(spool.exists(), "an unadopted spool file stays for its owner to delete");
        fs::remove_file(spool).unwrap();
        fs::remove_file(target).unwrap();
    }

    #[test]
    fn a_payload_storage_keeps_in_the_database_is_not_adopted() {
        let spool = scratch("inline-src");
        let target = scratch("inline-dst");
        fs::write(&spool, b"%PDF").unwrap();
        assert_eq!(adopt_into_cache("application/pdf", &spool, &target), None);
        assert!(spool.exists() && !target.exists());
        fs::remove_file(spool).unwrap();
    }

    #[test]
    fn a_failed_rename_is_not_an_adoption() {
        let spool = scratch("norename-src");
        fs::write(&spool, b"x").unwrap();
        let target = Path::new("/definitely/not/a/directory/y4p.cache");
        assert_eq!(adopt_into_cache("image/gif", &spool, target), None);
        fs::remove_file(spool).unwrap();
    }

    #[test]
    fn the_sniffed_head_is_bounded_and_a_missing_file_is_empty() {
        let path = scratch("head");
        fs::write(&path, vec![9u8; 10_000]).unwrap();
        assert_eq!(read_head(&path).len(), SNIFF_HEAD_BYTES);
        fs::write(&path, b"short").unwrap();
        assert_eq!(read_head(&path), b"short");
        fs::remove_file(&path).unwrap();
        assert!(read_head(&path).is_empty());
    }

    // --- convert_if_wanted: the image pipeline over a spool file ---
    //
    // These go through the process-wide breaker and throttle like the
    // `process_job` tests below. At most two tests here fail a conversion on
    // purpose, and the breaker needs three failures in a row, so none of them can
    // open the circuit under another test.

    fn spool_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("y4p-worker-convert-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn spooled_at(dir: &Path, name: &str, bytes: &[u8]) -> SpooledFile {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        let digest = <[u8; DIGEST_LEN]>::try_from(Sha3_256::digest(bytes).as_slice()).unwrap();
        SpooledFile::new(path, bytes.len() as u64, digest)
    }

    fn entries_in(dir: &Path) -> usize {
        fs::read_dir(dir).unwrap().count()
    }

    #[test]
    fn a_spooled_png_is_converted_in_place_of_the_original_which_is_deleted() {
        if !magick_available() { return; }
        let dir = spool_dir("png");
        let png = generated(&["-size", "64x64", "plasma:fractal", "-depth", "8", "-set", "comment", "SECRETCOMMENT", "png:-"]);
        let original = spooled_at(&dir, &format!("{}-1.part", std::process::id()), &png);
        let original_path = original.path().to_path_buf();

        let (mime, converted) = convert_if_wanted("image/png".to_owned(), original);

        assert_eq!(mime, "image/png");
        assert_ne!(converted.path(), original_path);
        assert!(!original_path.exists(), "the superseded original must be unlinked");
        let stored = fs::read(converted.path()).unwrap();
        assert_ne!(stored, png);
        assert!(!stored.windows(13).any(|w| w == b"SECRETCOMMENT"));
        assert_eq!(converted.hex_digest(), fingerprint(&stored), "the key must describe the stored bytes");
        // The in-memory road must agree to the byte, so a payload keys the same row on either.
        let (memory_mime, memory_bytes, memory_hash) = process_job("image/png".to_owned(), png);
        assert_eq!((mime.as_str(), stored.as_slice()), (memory_mime.as_str(), memory_bytes.as_slice()));
        assert_eq!(converted.hex_digest(), memory_hash);
        assert_eq!(entries_in(&dir), 1, "exactly the converted file remains");
        drop(converted);
        assert_eq!(entries_in(&dir), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_converted_jpeg_is_stored_under_the_png_mime() {
        if !magick_available() { return; }
        let dir = spool_dir("jpeg");
        let jpeg = generated(&["-size", "64x64", "plasma:fractal", "jpg:-"]);
        let (mime, converted) = convert_if_wanted("image/jpeg".to_owned(), spooled_at(&dir, &format!("{}-2.part", std::process::id()), &jpeg));
        assert_eq!(mime, "image/png");
        assert_eq!(&fs::read(converted.path()).unwrap()[..4], b"\x89PNG");
        drop(converted);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn what_the_pipeline_does_not_rewrite_is_returned_untouched() {
        let dir = spool_dir("bypass");
        let mut gif = b"GIF89a".to_vec();
        gif.extend((0..5000u32).map(|i| (i % 251) as u8));
        let cases: [(&str, &[u8]); 3] = [
            ("image/gif", &gif),
            ("text/plain", b"plain text that happens to be spooled"),
            ("application/octet-stream", b"\x00\x01\x02\x03 not an image at all"),
        ];
        for (n, (mime, bytes)) in cases.into_iter().enumerate() {
            let original = spooled_at(&dir, &format!("{}-{}.part", std::process::id(), 10 + n), bytes);
            let path = original.path().to_path_buf();
            let (out_mime, out) = convert_if_wanted(mime.to_owned(), original);
            assert_eq!(out_mime, mime);
            assert_eq!(out.path(), path, "{mime}: the very same file");
            assert_eq!(fs::read(out.path()).unwrap(), bytes);
            drop(out);
        }
        assert_eq!(entries_in(&dir), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_image_that_does_not_decode_keeps_its_original_file_and_leaves_nothing_else() {
        if !magick_available() { return; }
        let dir = spool_dir("corrupt");
        let mut corrupt = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        corrupt.extend_from_slice(&[0xA5; 4096]);
        let original = spooled_at(&dir, &format!("{}-20.part", std::process::id()), &corrupt);
        let path = original.path().to_path_buf();

        let (mime, out) = convert_if_wanted("image/png".to_owned(), original);

        assert_eq!(mime, "image/png");
        assert_eq!(out.path(), path, "the original is stored as it arrived");
        assert_eq!(fs::read(out.path()).unwrap(), corrupt);
        assert_eq!(out.hex_digest(), fingerprint(&corrupt), "and keeps its own digest");
        assert_eq!(entries_in(&dir), 1, "no half-written converted file");
        drop(out);
        fs::remove_dir_all(dir).unwrap();
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
