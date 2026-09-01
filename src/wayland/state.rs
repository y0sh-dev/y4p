// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/wayland/state.rs

use crate::storage::ClipboardDb;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_source_v1::ExtDataControlSourceV1,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

pub struct OfferData {
    pub mimes: Arc<Mutex<Vec<String>>>,
}

/// Where a `SourceMetadata`'s bytes actually live.
///
/// Added for kernel-level egress (PERFORMANCE.md "Next Frontier"): before
/// this, `SourceMetadata` always held a fully-materialized `Vec<u8>`, even
/// for large cached binaries (images) -- meaning a `copy-to` of a 70MB image
/// read the whole file into memory in `get_content_by_id`, then the old
/// `source.rs` Send handler unconditionally `.clone()`d it again before
/// writing it out. `File` lets the Send handler skip both copies entirely
/// and hand the destination pipe straight to `sendfile(2)`.
pub enum SourcePayload {
    /// Small/inline payload (text, or anything the DB stored directly in
    /// its BLOB column) held in memory, same as before.
    Owned(Vec<u8>),
    /// Large binary payload living in the on-disk cache. The Send handler
    /// opens this path itself and transfers it kernel-side.
    File(PathBuf),
}

pub struct SourceMetadata {
    pub mime: String,
    pub payload: SourcePayload,
}

pub struct ClipboardJob {
    pub mime: String,
    pub data: Vec<u8>,
    pub hash: String,
}

pub struct WaylandState {
    pub manager: Option<ExtDataControlManagerV1>,
    pub manager_id: Option<u32>,
    pub seat: Option<WlSeat>,
    pub seat_id: Option<u32>,
    pub device: Option<ExtDataControlDeviceV1>,
    // Only ever touched from the thread that owns `WaylandState` (the main
    // event loop), so no Arc<Mutex<_>> is needed here — see daemon::mod for
    // the separate, single-writer connection that runs concurrently under
    // SQLite's WAL mode.
    pub db: Option<ClipboardDb>,
    pub job_tx: Option<mpsc::Sender<ClipboardJob>>,
    pub verbose: bool,
    pub target_mime: String,
    pub rx_buf: Vec<u8>,
    pub last_data: Vec<u8>,
    pub provider_locks: u32,
    pub selection_received: bool,
    pub current_source: Option<ExtDataControlSourceV1>,
}

impl WaylandState {
    pub fn new_daemon(db: ClipboardDb, job_tx: mpsc::Sender<ClipboardJob>, verbose: bool) -> Self {
        Self {
            manager: None,
            manager_id: None,
            seat: None,
            seat_id: None,
            device: None,
            db: Some(db),
            job_tx: Some(job_tx),
            verbose,
            target_mime: String::new(),
            rx_buf: Vec::new(),
            last_data: Vec::new(),
            provider_locks: 0,
            selection_received: false,
            current_source: None,
        }
    }

    pub fn new_action(target_mime: String, verbose: bool) -> Self {
        Self {
            manager: None,
            manager_id: None,
            seat: None,
            seat_id: None,
            device: None,
            db: None,
            job_tx: None,
            verbose,
            target_mime,
            rx_buf: Vec::new(),
            last_data: Vec::new(),
            provider_locks: 0,
            selection_received: false,
            current_source: None,
        }
    }
}
