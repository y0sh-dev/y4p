// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/wayland/handlers/data_control/device.rs

use wayland_client::{Dispatch, Connection, QueueHandle, Proxy};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_offer_v1::ExtDataControlOfferV1,
};
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::{mpsc, Arc, Mutex};
use sha3::{Digest, Sha3_256};
use crate::wayland::state::{WaylandState, OfferData, ClipboardJob};
use crate::core::constants::*;
use super::{make_pipe, is_sensitive, AlignedBuffer};

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

            // Determine optimal MIME type based on MIME_PRIORITY_ORDER
            // (richest/most-reproducible first). Matching is case- and
            // whitespace-insensitive on the base type (see
            // core::utils::mime_base_eq), so a sender announcing e.g.
            // "TEXT/Plain" or "text/plain; charset=utf-8" (space after ';')
            // still hits its intended priority entry instead of falling
            // through to a generic category fallback. `.cloned()` always
            // takes the string as the sender actually offered it — the
            // compositor request below needs that exact original form, not
            // the normalized one.
            let mime_to_get = MIME_PRIORITY_ORDER.iter()
                .find_map(|&p| mimes.iter().find(|m| crate::core::utils::mime_base_eq(m, p)))
                .cloned()
                .or_else(|| mimes.iter().find(|m| m.to_ascii_lowercase().starts_with("image/")).cloned())
                .or_else(|| mimes.iter().find(|m| m.to_ascii_lowercase().starts_with("text/")).cloned())
                .or_else(|| mimes.first().cloned())
                .unwrap_or_else(|| DEFAULT_MIME.to_string());

            let is_image = mime_to_get.to_ascii_lowercase().starts_with("image/");

            // Original Image Fetcher (v0.3.0): a browser "copy image" nearly
            // always offers image/* alongside text/html carrying an <img>
            // tag pointing at the original asset. When both are present (and
            // the daemon has somewhere to send a resulting job), try to
            // recover that original — typically a much higher-fidelity
            // WebP/JPEG than the re-encoded PNG the browser also put on the
            // clipboard — before falling back to the ordinary receive path.
            if is_image
                && FETCH_ORIGINAL_IMAGES
                && mimes.iter().any(|m| crate::core::utils::mime_base_eq(m, "text/html"))
                && let Some(tx) = state.job_tx.clone()
            {
                let conn = conn.clone();
                std::thread::spawn(move || {
                    fetch_original_image(offer, conn, mime_to_get, tx);
                });
                return;
            }

            // Initialize data transfer pipe
            let (read_file, write_fd) = match make_pipe(is_image) {
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
                // S-07: uri-list must be fully buffered and normalized before
                // hashing (the fingerprint has to match what's actually
                // persisted), so it can't share the other MIMEs' single-pass
                // hash-while-read below.
                let is_uri_list = mime_to_get == MIME_URI_LIST;

                std::thread::spawn(move || {
                    ingest_and_send(read_file, mime_to_get, is_uri_list, &job_tx_clone);
                });
            } else {
                // Action Mode: Synchronous read for immediate CLI processing
                let mut buf = Vec::new();
                let mut reader = read_file.take(268435456);
                let _ = reader.read_to_end(&mut buf);
                state.rx_buf = buf;
            }
        }
    }

    // Bind OfferData to each new DataOffer instance for isolated MIME tracking
    wayland_client::event_created_child!(WaylandState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, OfferData { mimes: Arc::new(Mutex::new(Vec::new())) })
    ]);
}

/// Reads a MIME payload from an already-`offer.receive()`d pipe, hashes it
/// (re-identifying images by magic bytes along the way — see
/// `core::utils::detect_image_mime`), and forwards the finished
/// `ClipboardJob` to the DbWorker.
///
/// Factored out of the Selection handler so both the ordinary ingestion
/// path (which spawns a dedicated thread per selection) and the Original
/// Image Fetcher's fallback (`fallback_receive_image`, called from a thread
/// it's already running on) reach identical read/hash/send behavior without
/// duplicating it — this function itself never spawns anything.
fn ingest_and_send(read_file: std::fs::File, mime_to_get: String, is_uri_list: bool, job_tx: &mpsc::Sender<ClipboardJob>) {
    let mut payload = Vec::with_capacity(1048576);
    let mut reader = read_file.take(268435456);

    // Malformed size/align is unreachable with these fixed literals, but
    // skip this one ingestion job rather than unwind if it ever weren't
    // (see `AlignedBuffer::new`).
    let Some(mut chunk_buffer) = AlignedBuffer::new(65536, 4096) else { return; };
    let chunk = chunk_buffer.as_mut_slice();

    while let Ok(n) = reader.read(chunk) {
        if n == 0 { break; }
        payload.extend_from_slice(&chunk[..n]);
    }

    if payload.is_empty() { return; }
    let mut final_mime = mime_to_get;

    if is_uri_list {
        payload = crate::core::utils::normalize_uri_list(&payload);
        if payload.is_empty() { return; }
    } else if let Some(m) = crate::core::utils::detect_image_mime(&payload) {
        final_mime = m.to_string();
        // A compositor transfer can be cut short the same way a curl
        // download can — repair before this payload's hash is ever computed.
        payload = crate::core::utils::sanitize_image_payload(payload, &final_mime);
    } else if crate::core::utils::is_text_mime(&final_mime) {
        payload = crate::core::utils::sanitize_text_payload(&payload);
        if payload.is_empty() { return; }
    }

    // SHA3-256 fingerprint of the final normalized/sanitized payload actually being persisted.
    let mut hasher = Sha3_256::new();
    hasher.update(&payload);
    let hash = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>();


    // Send the completed payload and its SHA3 fingerprint to the persistent worker.
    let _ = job_tx.send(ClipboardJob { mime: final_mime, data: payload, hash });

    // SAFETY: `malloc_trim(0)` only requests the allocator release free
    // pages back to the OS; it doesn't touch any live allocation this
    // thread holds.
    #[cfg(target_os = "linux")]
    unsafe { libc::malloc_trim(0); }
}

/// The pre-v0.3.0 image ingestion path, factored out so both the ordinary
/// case (no `text/html` offered alongside the image) and the Original Image
/// Fetcher's fallback reach it identically: request `image_mime` from
/// `offer` over a fresh pipe, then read/hash/send exactly as before.
fn fallback_receive_image(offer: ExtDataControlOfferV1, conn: Connection, image_mime: String, job_tx: &mpsc::Sender<ClipboardJob>) {
    let Some((read_file, write_fd)) = make_pipe(true) else { return; };
    offer.receive(image_mime.clone(), write_fd.as_fd());
    drop(write_fd);
    let _ = conn.flush();
    ingest_and_send(read_file, image_mime, false, job_tx);
}

/// Original Image Fetcher (v0.3.0): when a selection offers both an image
/// and `text/html` — the common shape of a browser's "copy image" — this
/// tries to recover the *original* asset from its source URL via `curl`,
/// instead of settling for whatever bitmap (often a lossily downscaled
/// `image/png`) the browser itself wrote to the clipboard.
///
/// Runs entirely on its own thread (spawned by the caller), so neither the
/// `text/html` pipe read nor `curl`'s network I/O ever blocks the daemon's
/// main `libc::poll` loop. Sending Wayland requests from this thread —
/// `offer.receive`/`conn.flush`, used for the `text/html` request below and,
/// on the fallback path, inside `fallback_receive_image` — is safe:
/// `wayland-backend`'s connection state is `Send + Sync` by design, and
/// nothing here waits on a reply; any resulting event still dispatches on
/// the compositor's usual event-loop thread.
fn fetch_original_image(offer: ExtDataControlOfferV1, conn: Connection, image_mime: String, job_tx: mpsc::Sender<ClipboardJob>) {
    let Some((html_read, html_write)) = make_pipe(false) else {
        fallback_receive_image(offer, conn, image_mime, &job_tx);
        return;
    };
    offer.receive("text/html".to_string(), html_write.as_fd());
    drop(html_write);
    let _ = conn.flush();

    let mut html_payload = Vec::new();
    let mut reader = html_read.take(268435456);
    let _ = reader.read_to_end(&mut html_payload);

    let fetched = crate::core::utils::extract_image_url_from_html(&html_payload)
        .filter(|url| crate::core::utils::is_valid_http_url(url))
        .and_then(|url| crate::core::utils::fetch_image_via_curl(&url).ok())
        .and_then(|bytes| crate::core::utils::detect_image_mime(&bytes).map(|mime| (bytes, mime)));

    match fetched {
        // Original bytes recovered — done. `offer.receive(image_mime, ...)`
        // is never called in this branch, so the browser's own re-encoded
        // bitmap is never separately received or stored alongside it (no
        // duplicate save — see the requirement doc's section 2.3.1).
        Some((data, mime)) => {
            // Sanitize before hashing: the hash persisted to SQLite, the
            // cache filename, and the bytes served back on `copy-to` must
            // all be computed from the exact same (possibly-repaired) data.
            let data = crate::core::utils::sanitize_image_payload(data, mime);
            let mut hasher = Sha3_256::new();
            hasher.update(&data);
            let hash = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>();
            let _ = job_tx.send(ClipboardJob { mime: mime.to_string(), data, hash });

            // SAFETY: `malloc_trim(0)` only requests the allocator release
            // free pages back to the OS; it doesn't touch any live
            // allocation this thread holds.
            #[cfg(target_os = "linux")]
            unsafe { libc::malloc_trim(0); }
        }
        // No <img> URL, a non-http(s) URL (relative path, data:image/...),
        // a failed/timed-out/non-zero curl run, or a response that isn't
        // actually an image — every one of these falls back identically,
        // with nothing surfaced to the user: the clipboard event is still
        // serviced, just via the ordinary Wayland receive path.
        None => fallback_receive_image(offer, conn, image_mime, &job_tx),
    }
}
