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
