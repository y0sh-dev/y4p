// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/wayland/src/handlers/data_control/device.rs

use wayland_client::{Dispatch, Connection, QueueHandle, Proxy};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_offer_v1::ExtDataControlOfferV1,
};
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::{mpsc, Arc, Mutex};
use sha3::{Digest, Sha3_256};
use crate::state::{WaylandState, OfferData, ClipboardJob};
use crate::core::constants::*;
use super::{make_pipe, is_sensitive};

impl Dispatch<ExtDataControlDeviceV1, ()> for WaylandState {
    fn event(state: &mut Self, _: &ExtDataControlDeviceV1, ev: ext_data_control_device_v1::Event, _: &(), conn: &Connection, _: &QueueHandle<Self>) {
        if let ext_data_control_device_v1::Event::Selection { id } = ev {
            // Update synchronization status
            state.selection_received = true;

            // Prevent self-ingestion by checking active provider locks
            if state.provider_locks > 0 {
                state.provider_locks -= 1;
                return;
            }

            // Private mode: bypass ingestion entirely (no offer.receive, no DB Worker job).
            if state.paused {
                return;
            }

            let Some(offer) = id else { return };

            // Extract all available MIME types for this specific offer
            let mimes: Vec<String> = offer
                .data::<OfferData>()
                .and_then(|d| d.mimes.lock().ok())
                .map(|g| g.clone())
                .unwrap_or_default();

            if mimes.is_empty() || is_sensitive(&mimes) { return; }

            // Determine which offered MIME to request: a specific one if
            // `y4p paste-from <mime>` is waiting for it (Action Mode), or the
            // richest/most-reproducible one by `MIME_PRIORITY_ORDER`
            // otherwise (daemon ingestion) — see `select_mime`.
            let drop_rtf = state.config.should_drop_rtf();
            let Some(mime_to_get) = select_mime(&mimes, state.target_mime.as_deref(), drop_rtf) else { return; };

            // Initialize data transfer pipe
            let (read_file, write_fd) = match make_pipe() {
                Some(p) => p,
                None => return,
            };

            // Request data transmission from the compositor
            offer.receive(mime_to_get.clone(), write_fd.as_fd());
            drop(write_fd);
            let _ = conn.flush();

            // Offload ingestion and persistence to the worker thread
            if let Some(ref tx) = state.job_tx {
                let job_tx_clone = tx.clone();
                std::thread::spawn(move || {
                    ingest_and_send(read_file, mime_to_get, &job_tx_clone);
                });
            } else {
                // Action Mode: synchronous read for immediate CLI processing.
                // Applies the same boundary guard and normalisation as the
                // daemon path (`ingest_and_send`) so `paste-from` returns
                // exactly the bytes that would otherwise have been persisted.
                if let Some(payload) = read_bounded_payload(read_file, MAX_PAYLOAD_BYTES)
                    && let Some((_, payload)) = normalise_payload(mime_to_get, payload)
                {
                    state.rx_buf = payload;
                }
            }
        }
    }

    // Bind OfferData to each new DataOffer instance for isolated MIME tracking
    wayland_client::event_created_child!(WaylandState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, OfferData { mimes: Arc::new(Mutex::new(Vec::new())) })
    ]);
}

/// Chooses which of an offer's `mimes` to request from the compositor.
///
/// `target_mime` is `Some` only in Action Mode (`y4p paste-from <mime>`
/// waits for one specific MIME): the matching offer is found via
/// `core::utils::mime_base_eq` (case- and whitespace-insensitive, parameters
/// ignored), so a sender announcing e.g. "TEXT/Plain" or
/// "text/plain; charset=utf-8" still satisfies a request for "text/plain".
/// `drop_rtf` is not consulted here — an explicit request names exactly the
/// MIME the caller wants, RTF included, and that request is honoured as-is.
///
/// `target_mime` is `None` in daemon ingestion, where no single MIME is
/// being awaited: the richest/most-reproducible offer is chosen via
/// `MIME_PRIORITY_ORDER`, falling back through images, then plain text,
/// then anything at all. `drop_rtf` (`[mime] drop_rtf` in `y4p.toml`) is
/// applied at every step of this fallback chain, so RTF is never selected
/// when it's enabled, even as the last-resort catch-all.
pub(crate) fn select_mime(mimes: &[String], target_mime: Option<&str>, drop_rtf: bool) -> Option<String> {
    if let Some(target) = target_mime {
        return mimes.iter().find(|m| crate::core::utils::mime_base_eq(m, target)).cloned();
    }

    MIME_PRIORITY_ORDER.iter()
        .find_map(|&p| mimes.iter().find(|m| crate::core::utils::mime_base_eq(m, p)))
        .cloned()
        .or_else(|| mimes.iter().find(|m| crate::core::utils::starts_with_ignore_ascii_case(m, "image/")).cloned())
        .or_else(|| mimes.iter().find(|m| crate::core::utils::starts_with_ignore_ascii_case(m, "text/") && (!drop_rtf || !crate::core::utils::is_rtf_mime(m))).cloned())
        .or_else(|| mimes.iter().find(|m| !drop_rtf || !crate::core::utils::is_rtf_mime(m)).cloned())
}

/// Reads a MIME payload from an already-`offer.receive()`d pipe, turns it
/// into a finished `ClipboardJob` (see `build_job`) and forwards it to the
/// DbWorker.
///
/// Factored out of the Selection handler so it can be called from the
/// dedicated per-selection thread the handler spawns. That thread is also
/// why image transcoding may block here (it spawns `magick`): neither the
/// daemon's poll loop nor the Wayland event queue is ever held up by it.
fn ingest_and_send(read_file: std::fs::File, mime_to_get: String, job_tx: &mpsc::Sender<ClipboardJob>) {
    let Some(payload) = read_bounded_payload(read_file, MAX_PAYLOAD_BYTES) else { return; };
    let Some(job) = build_job(mime_to_get, payload) else { return; };

    // Send the completed payload and its SHA3 fingerprint to the persistent worker.
    let _ = job_tx.send(job);
}

/// Turns a raw payload into the job that will actually be persisted, in a
/// fixed order: normalise, then route through the image pipeline, then hash.
///
/// The order matters. Raster images are privacy-stripped and re-encoded by
/// `image::route::route_for_ingest`, which may change both the MIME and every
/// byte; the fingerprint must describe what `DbWorker` and the cache file
/// really store, so it is computed last, over the final bytes. (Hashing
/// earlier would key the row by bytes that are never saved, and the same
/// image copied twice would still dedupe only by accident.) Routing falls
/// back to the untouched original on any failure, so the hash then covers
/// the original instead — consistent either way.
///
/// Returns `None` only where `normalise_payload` does.
fn build_job(mime_to_get: String, payload: Vec<u8>) -> Option<ClipboardJob> {
    let (normalised_mime, normalised) = normalise_payload(mime_to_get, payload)?;
    let (mime, data) = crate::image::route::route_for_ingest(normalised_mime, normalised);
    let hash = fingerprint(&data);
    Some(ClipboardJob { mime, data, hash })
}

/// Lowercase-hex SHA3-256 fingerprint of `data`.
fn fingerprint(data: &[u8]) -> String {
    let mut hasher = Sha3_256::new();
    hasher.update(data);
    hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>()
}

/// Reads at most `limit` bytes from an already-`offer.receive()`d pipe.
/// Takes one byte more than `limit` so a stream that genuinely exceeds it
/// can be told apart from one that exactly fits: `Read::take` stops
/// silently at its cap with no error, so without this probe byte a
/// truncated transfer and a complete one would both return `Ok(_)` with no
/// way to distinguish them — and the truncated bytes already read would
/// otherwise be persisted as if they were the whole payload (the
/// Truncation vulnerability this guards against). Returns `None` on an I/O
/// error, an empty read, or a stream that exceeded `limit`; in every case
/// the caller discards whatever bytes were read rather than keeping a
/// partial payload.
fn read_bounded_payload<R: Read>(reader: R, limit: u64) -> Option<Vec<u8>> {
    let mut payload = Vec::new();
    let mut bounded = reader.take(limit + 1);
    if bounded.read_to_end(&mut payload).is_err() {
        return None;
    }
    if payload.is_empty() || payload.len() as u64 > limit {
        return None;
    }
    Some(payload)
}

/// Normalises a raw payload against the MIME it was requested as, applying
/// the same treatment regardless of whether the caller is the daemon's
/// ingestion thread or Action Mode's synchronous read (see both call
/// sites in the `Selection` handler above):
/// - `text/uri-list` (matched via `mime_base_eq`, so parameters/case on the
///   offer don't matter) is percent-decoded and validated into one path per
///   line — see `core::utils::normalize_uri_list` — since the fingerprint
///   persisted downstream has to match what's actually stored, not the raw
///   wire bytes.
/// - Anything else is re-identified by magic bytes in case the sender
///   mislabelled an image (see `core::utils::detect_image_mime`); the bytes
///   themselves are never altered in this branch.
///
/// Returns `None` only when a `text/uri-list` payload normalises away to
/// nothing (no valid paths) — never for any other MIME, since a non-empty
/// raw read is always usable as-is.
fn normalise_payload(mime_to_get: String, payload: Vec<u8>) -> Option<(String, Vec<u8>)> {
    if crate::core::utils::mime_base_eq(&mime_to_get, MIME_URI_LIST) {
        let normalised = crate::core::utils::normalize_uri_list(&payload);
        if normalised.is_empty() {
            return None;
        }
        return Some((mime_to_get, normalised));
    }

    if let Some(m) = crate::core::utils::detect_image_mime(&payload) {
        return Some((m.to_string(), payload));
    }

    Some((mime_to_get, payload))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn select_mime_requested_match_is_returned_verbatim() {
        let offers = mimes(&["text/plain", "text/html"]);
        assert_eq!(select_mime(&offers, Some("text/html"), true), Some("text/html".to_string()));
    }

    #[test]
    fn select_mime_absent_requested_mime_returns_none() {
        let offers = mimes(&["text/plain", "text/html"]);
        assert_eq!(select_mime(&offers, Some("application/pdf"), true), None);
    }

    #[test]
    fn select_mime_requested_match_is_case_and_param_insensitive() {
        let offers = mimes(&["TEXT/Plain; charset=UTF-8"]);
        assert_eq!(select_mime(&offers, Some("text/plain"), true), Some("TEXT/Plain; charset=UTF-8".to_string()));
    }

    #[test]
    fn select_mime_requested_rtf_is_honoured_even_when_drop_rtf_enabled() {
        // An explicit `paste-from text/rtf` request names exactly what the
        // caller wants; `drop_rtf` only governs the daemon's own fallback
        // chain (target_mime = None), never an explicit request.
        let offers = mimes(&["text/rtf"]);
        assert_eq!(select_mime(&offers, Some("text/rtf"), true), Some("text/rtf".to_string()));
    }

    #[test]
    fn select_mime_daemon_fallback_prefers_priority_order() {
        let offers = mimes(&["text/html", "image/png", "text/plain"]);
        assert_eq!(select_mime(&offers, None, true), Some("image/png".to_string()));
    }

    #[test]
    fn select_mime_daemon_fallback_matches_lower_ranked_priority_entry() {
        // "text/html" is still in MIME_PRIORITY_ORDER, just ranked below
        // plain text and images — with nothing higher-ranked on offer, it's
        // still reached via the priority list itself, not the catch-all.
        let offers = mimes(&["text/html"]);
        assert_eq!(select_mime(&offers, None, true), Some("text/html".to_string()));
    }

    #[test]
    fn select_mime_daemon_fallback_catch_all_for_unlisted_mime() {
        // Not in MIME_PRIORITY_ORDER, and neither an "image/" nor "text/"
        // prefix — only the final unconditional catch-all reaches it.
        let offers = mimes(&["application/octet-stream"]);
        assert_eq!(select_mime(&offers, None, true), Some("application/octet-stream".to_string()));
    }

    #[test]
    fn select_mime_drop_rtf_excludes_rtf_from_daemon_fallback() {
        let offers = mimes(&["text/rtf"]);
        assert_eq!(select_mime(&offers, None, true), None);
    }

    #[test]
    fn select_mime_rtf_allowed_in_daemon_fallback_when_drop_rtf_disabled() {
        let offers = mimes(&["text/rtf"]);
        assert_eq!(select_mime(&offers, None, false), Some("text/rtf".to_string()));
    }

    #[test]
    fn select_mime_drop_rtf_skips_rtf_in_favour_of_other_text() {
        let offers = mimes(&["text/rtf", "text/markdown"]);
        assert_eq!(select_mime(&offers, None, true), Some("text/markdown".to_string()));
    }

    #[test]
    fn select_mime_empty_offer_list_returns_none() {
        let offers: Vec<String> = Vec::new();
        assert_eq!(select_mime(&offers, None, true), None);
        assert_eq!(select_mime(&offers, Some("text/plain"), true), None);
    }

    // --- read_bounded_payload ---

    #[test]
    fn read_bounded_payload_within_limit_returns_payload() {
        let data = b"hello".as_slice();
        assert_eq!(read_bounded_payload(data, 10), Some(b"hello".to_vec()));
    }

    #[test]
    fn read_bounded_payload_exactly_at_limit_returns_payload() {
        let data = b"abcde".as_slice();
        assert_eq!(read_bounded_payload(data, 5), Some(b"abcde".to_vec()));
    }

    #[test]
    fn read_bounded_payload_exceeding_limit_by_one_byte_is_discarded() {
        let data = b"abcdef".as_slice();
        assert_eq!(read_bounded_payload(data, 5), None);
    }

    #[test]
    fn read_bounded_payload_massively_oversized_stream_is_discarded_not_truncated() {
        // The crux of the truncation vulnerability: a stream far larger
        // than the limit must be rejected outright, never silently cut
        // down to `limit` bytes and handed back as if it were complete.
        let data = vec![b'x'; 1000];
        assert_eq!(read_bounded_payload(data.as_slice(), 5), None);
    }

    #[test]
    fn read_bounded_payload_empty_input_returns_none() {
        let data = b"".as_slice();
        assert_eq!(read_bounded_payload(data, 10), None);
    }

    // --- normalise_payload ---

    #[test]
    fn normalise_payload_plain_text_passes_through_unchanged() {
        let result = normalise_payload("text/plain".to_string(), b"hello world".to_vec());
        assert_eq!(result, Some(("text/plain".to_string(), b"hello world".to_vec())));
    }

    #[test]
    fn normalise_payload_uri_list_is_normalised() {
        let raw = b"file:///path/to/a.txt\nfile:///path/to/b.png".to_vec();
        let result = normalise_payload(MIME_URI_LIST.to_string(), raw);
        assert_eq!(
            result,
            Some((MIME_URI_LIST.to_string(), b"/path/to/a.txt\n/path/to/b.png".to_vec()))
        );
    }

    #[test]
    fn normalise_payload_uri_list_mime_with_parameters_is_still_recognised() {
        // A sender announcing "text/uri-list; charset=utf-8" (or any case
        // variant) must still be routed through URI-list normalisation —
        // mirrors `select_mime`'s own use of `mime_base_eq` elsewhere.
        let raw = b"file:///a.txt".to_vec();
        let result = normalise_payload("TEXT/URI-LIST; charset=UTF-8".to_string(), raw);
        assert_eq!(
            result,
            Some(("TEXT/URI-LIST; charset=UTF-8".to_string(), b"/a.txt".to_vec()))
        );
    }

    #[test]
    fn normalise_payload_uri_list_with_no_valid_paths_returns_none() {
        let raw = b"# just a comment\n\n".to_vec();
        assert_eq!(normalise_payload(MIME_URI_LIST.to_string(), raw), None);
    }

    #[test]
    fn normalise_payload_reidentifies_image_by_magic_bytes() {
        let png_bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let result = normalise_payload("application/octet-stream".to_string(), png_bytes.clone());
        assert_eq!(result, Some(("image/png".to_string(), png_bytes)));
    }

    #[test]
    fn normalise_payload_non_image_non_uri_list_mime_is_untouched() {
        let raw = b"# just text, not uri-list or an image".to_vec();
        let result = normalise_payload("text/markdown".to_string(), raw.clone());
        assert_eq!(result, Some(("text/markdown".to_string(), raw)));
    }

    // --- build_job (normalise -> image routing -> hash) ---

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
    fn build_job_plain_text_is_unchanged_and_hashed_as_is() {
        let raw = b"hello clipboard".to_vec();
        let job = build_job("text/plain".to_string(), raw.clone()).unwrap();
        assert_eq!(job.mime, "text/plain");
        assert_eq!(job.data, raw);
        assert_eq!(job.hash, fingerprint(&raw));
    }

    #[test]
    fn build_job_svg_bypasses_the_image_pipeline_unchanged() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"5\" height=\"5\"/>".to_vec();
        let job = build_job("image/svg+xml".to_string(), svg.clone()).unwrap();
        assert_eq!(job.mime, "image/svg+xml");
        assert_eq!(job.data, svg);
        assert_eq!(job.hash, fingerprint(&svg));
    }

    #[test]
    fn build_job_uri_list_is_normalised_then_hashed_without_image_routing() {
        let job = build_job(MIME_URI_LIST.to_string(), b"file:///a.png".to_vec()).unwrap();
        assert_eq!(job.mime, MIME_URI_LIST);
        assert_eq!(job.data, b"/a.png".to_vec());
        assert_eq!(job.hash, fingerprint(b"/a.png"));
    }

    #[test]
    fn build_job_png_becomes_webp_and_the_hash_covers_the_converted_bytes() {
        if !magick_available() { return; }
        let png = generated(&["-size", "32x32", "plasma:fractal", "-depth", "8", "png:-"]);
        let job = build_job("image/png".to_string(), png.clone()).unwrap();
        assert_eq!(job.mime, "image/webp");
        assert_eq!(&job.data[8..12], b"WEBP");
        assert_eq!(job.hash, fingerprint(&job.data), "fingerprint must describe the stored bytes");
        assert_ne!(job.hash, fingerprint(&png), "fingerprint must not be that of the discarded original");
    }

    #[test]
    fn build_job_same_image_copied_twice_yields_the_same_fingerprint() {
        if !magick_available() { return; }
        let png = generated(&["-size", "32x32", "plasma:fractal", "-depth", "8", "png:-"]);
        let first = build_job("image/png".to_string(), png.clone()).unwrap();
        let second = build_job("image/png".to_string(), png).unwrap();
        assert_eq!(first.hash, second.hash, "deduplication relies on a deterministic conversion");
    }

    #[test]
    fn build_job_corrupt_png_falls_back_to_the_original_bytes_mime_and_hash() {
        if !magick_available() { return; }
        let mut corrupt = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        corrupt.extend_from_slice(&[0xA5; 2048]);
        let job = build_job("image/png".to_string(), corrupt.clone()).unwrap();
        assert_eq!(job.mime, "image/png");
        assert_eq!(job.data, corrupt);
        assert_eq!(job.hash, fingerprint(&corrupt));
    }

    #[test]
    fn build_job_mislabelled_image_is_reidentified_before_routing() {
        if !magick_available() { return; }
        let png = generated(&["-size", "16x16", "xc:red", "png:-"]);
        let job = build_job("application/octet-stream".to_string(), png).unwrap();
        assert_eq!(job.mime, "image/webp");
    }
}
