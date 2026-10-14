// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/wayland/src/state.rs

use wayland_client::protocol::wl_seat::WlSeat;
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_source_v1::ExtDataControlSourceV1,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use crate::core::constants::INGEST_MAX_CONCURRENT_READERS;
use crate::ingest::{IngestStats, ReaderGate};

pub struct OfferData {
    pub mimes: Arc<Mutex<Vec<String>>>,
}

/// Where a `SourceMetadata`'s bytes actually live.
///
/// Added for kernel-level egress (PERFORMANCE.md "Next Frontier"): before
/// this, `SourceMetadata` always held a fully-materialised `Vec<u8>`, even
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

/// A normalised raw payload handed from the ingestion thread to the daemon's
/// worker. Deliberately carries no fingerprint: image transcoding rewrites the
/// bytes, so the digest is only meaningful once the worker has the final ones.
pub struct ClipboardJob {
    pub mime: String,
    pub data: Vec<u8>,
}

pub struct WaylandState {
    pub manager: Option<ExtDataControlManagerV1>,
    pub manager_id: Option<u32>,
    pub seat: Option<WlSeat>,
    pub seat_id: Option<u32>,
    pub device: Option<ExtDataControlDeviceV1>,
    /// Bounded queue to the daemon's worker (`None` in Action Mode). Never
    /// block on it from the event loop: see `crate::ingest::enqueue`.
    pub job_tx: Option<mpsc::SyncSender<ClipboardJob>>,
    /// Caps simultaneous ingestion readers; checked before a pipe is allocated.
    pub reader_gate: Arc<ReaderGate>,
    /// What overload has cost so far; read by the daemon for `y4p status`.
    pub ingest_stats: Arc<IngestStats>,
    pub verbose: bool,
    // Action-mode-only: the specific MIME `y4p paste-from <mime>` was asked
    // for. `None` in daemon mode, where no single MIME is being awaited —
    // the Selection handler falls back to `MIME_PRIORITY_ORDER` instead.
    pub target_mime: Option<String>,
    pub rx_buf: Vec<u8>,
    pub provider_locks: u32,
    pub selection_received: bool,
    pub current_source: Option<ExtDataControlSourceV1>,
    // Private mode: same single-thread-owned field as the rest of this
    // struct, toggled by IPC Pause/Resume.
    pub paused: bool,
    // Active configuration, loaded at startup and shared read-only
    // with ingestion threads via `Arc`.
    pub config: Arc<crate::core::config::Config>,
}

impl WaylandState {
    // `db` no longer lives here (architecture boundary: this I/O sensor
    // layer must not depend on `storage`) — the daemon keeps its read-side
    // `ClipboardDb` handle in its own scope and passes it explicitly to
    // whatever needs it (see `daemon::handle_restore_request`).
    pub fn new_daemon(job_tx: mpsc::SyncSender<ClipboardJob>, verbose: bool, config: Arc<crate::core::config::Config>) -> Self {
        Self {
            manager: None,
            manager_id: None,
            seat: None,
            seat_id: None,
            device: None,
            job_tx: Some(job_tx),
            reader_gate: Arc::new(ReaderGate::new(INGEST_MAX_CONCURRENT_READERS)),
            ingest_stats: Arc::new(IngestStats::default()),
            verbose,
            target_mime: None,
            rx_buf: Vec::new(),
            provider_locks: 0,
            selection_received: false,
            current_source: None,
            paused: false,
            config,
        }
    }

    pub fn new_action(target_mime: String, verbose: bool, config: Arc<crate::core::config::Config>) -> Self {
        Self {
            manager: None,
            manager_id: None,
            seat: None,
            seat_id: None,
            device: None,
            job_tx: None,
            reader_gate: Arc::new(ReaderGate::new(INGEST_MAX_CONCURRENT_READERS)),
            ingest_stats: Arc::new(IngestStats::default()),
            verbose,
            target_mime: Some(target_mime),
            rx_buf: Vec::new(),
            provider_locks: 0,
            selection_received: false,
            current_source: None,
            paused: false,
            config,
        }
    }
}
