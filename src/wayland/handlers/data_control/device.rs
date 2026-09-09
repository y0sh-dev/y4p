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
use std::sync::{Arc, Mutex};
use crate::wayland::state::{WaylandState, OfferData, ClipboardJob};
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

            // Initialize data transfer pipe
            let is_image = mime_to_get.to_ascii_lowercase().starts_with("image/");
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
                    use sha3::{Sha3_256, Digest};

                    let mut payload = Vec::with_capacity(1048576);
                    let mut reader = read_file.take(268435456);

                    let mut chunk_buffer = super::AlignedBuffer::new(65536, 4096);
                    let chunk = chunk_buffer.as_mut_slice();

                    let mut hasher = (!is_uri_list).then(Sha3_256::new);

                    while let Ok(n) = reader.read(chunk) {
                        if n == 0 { break; }
                        let data = &chunk[..n];
                        if let Some(h) = hasher.as_mut() { h.update(data); }
                        payload.extend_from_slice(data);
                    }

                    if payload.is_empty() { return; }
                    let mut final_mime = mime_to_get;

                    if is_uri_list {
                        payload = crate::core::utils::normalize_uri_list(&payload);
                        if payload.is_empty() { return; }
                    } else {
                        // Heuristic MIME identification via magic bytes
                        let detected = if payload.len() >= 4 {
                            match &payload[0..4] {
                                [0x89, 0x50, 0x4E, 0x47] => Some("image/png"),
                                [0xFF, 0xD8, 0xFF, _]    => Some("image/jpeg"),
                                [0x47, 0x49, 0x46, 0x38] => Some("image/gif"),
                                b"RIFF" if payload.len() >= 12 && &payload[8..12] == b"WEBP" => Some("image/webp"),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        // SVG has no fixed magic bytes (it's XML text), so it
                        // only gets checked once the binary signatures above miss.
                        let detected = detected.or_else(|| {
                            let head = payload.trim_ascii_start();
                            (head.starts_with(b"<?xml") || head.starts_with(b"<svg")).then_some("image/svg+xml")
                        });
                        if let Some(m) = detected { final_mime = m.to_string(); }
                    }

                    // SHA3-256 finalize() returns a GenericArray.
                    let hash = match hasher {
                        Some(h) => h.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>(),
                        // uri-list: fingerprint the normalized bytes actually being persisted.
                        None => {
                            let mut h = Sha3_256::new();
                            h.update(&payload);
                            h.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>()
                        }
                    };

                    // Send the completed payload and its SHA3 fingerprint to the persistent worker.
                    let _ = job_tx_clone.send(ClipboardJob {
                        mime: final_mime,
                        data: payload,
                        hash,
                    });

                    #[cfg(target_os = "linux")]
                    unsafe { libc::malloc_trim(0); }
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
