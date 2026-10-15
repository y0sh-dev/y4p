// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/wayland/src/state.rs

use wayland_client::protocol::wl_seat::WlSeat;
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_source_v1::ExtDataControlSourceV1,
};
use std::sync::{Arc, Mutex, mpsc};
use crate::core::constants::INGEST_MAX_CONCURRENT_READERS;
use crate::core::payload::{Payload, PayloadError};
use crate::ingest::{IngestStats, ReaderGate};
use crate::spool::PayloadSpooler;

pub struct OfferData {
    pub mimes: Arc<Mutex<Vec<String>>>,
}

/// What a data source serves: its advertised MIME and where the bytes live.
///
/// `payload` is the shared `core::payload::Payload`, so egress speaks the same
/// vocabulary as ingress (`ClipboardJob`): an in-memory buffer, a streaming
/// descriptor, or a file in the on-disk cache. The `Send` handler picks the
/// transfer from the variant (see `handlers/data_control/source.rs`); a `File`
/// or `Stream` payload never has to be materialised in this process's heap.
pub struct SourceMetadata {
    pub mime: String,
    pub payload: Payload,
}

/// A normalised raw payload handed from the ingestion thread to the daemon's
/// worker. Deliberately carries no fingerprint: image transcoding rewrites the
/// bytes, so the digest is only meaningful once the worker has the final ones.
///
/// The payload is a `core::payload::Payload`, not a bare `Vec<u8>`. The ingestion
/// thread hands over `Payload::Memory` for anything that fits the inline
/// threshold (and always for text, which it must normalise), and
/// `Payload::Spooled` for a larger one it has already drained to a spool file
/// (see `crate::spool`): the queue then carries a path and a digest, not the
/// bytes, so what a full queue pins is bounded by the number of slots, not by
/// the size of the payloads. Moving either through the queue moves a pointer.
pub struct ClipboardJob {
    pub mime: String,
    pub payload: Payload,
}

impl ClipboardJob {
    /// The common case, and the shape every caller had before payloads became
    /// polymorphic: a MIME and fully-materialised bytes.
    pub fn new(mime: String, data: Vec<u8>) -> Self {
        Self { mime, payload: Payload::memory(data) }
    }

    pub fn from_payload(mime: String, payload: Payload) -> Self {
        Self { mime, payload }
    }

    /// Length in bytes when known without I/O (see `Payload::len_hint`).
    pub fn len_hint(&self) -> Option<u64> {
        self.payload.len_hint()
    }

    /// Materialises the job as `(mime, bytes)` with at most `limit` bytes. An
    /// in-memory payload is validated and handed over without a copy.
    pub fn into_bytes(self, limit: u64) -> Result<(String, Vec<u8>), PayloadError> {
        let data = self.payload.read_bounded(limit)?;
        Ok((self.mime, data))
    }
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
    /// Where a large payload is drained to disk instead of into the heap
    /// (see `crate::spool`). `None`: every payload is read into memory.
    pub spooler: Option<Arc<dyn PayloadSpooler>>,
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
            spooler: None,
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

    /// Enables the streaming ingestion path (daemon mode only): payloads that
    /// outgrow the inline threshold are drained by `spooler` instead of being
    /// read into a `Vec`.
    pub fn with_spooler(mut self, spooler: Arc<dyn PayloadSpooler>) -> Self {
        self.spooler = Some(spooler);
        self
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
            spooler: None,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_job_built_from_bytes_is_an_in_memory_payload_with_a_known_length() {
        let job = ClipboardJob::new("text/plain".to_owned(), b"abc".to_vec());
        assert_eq!(job.len_hint(), Some(3));
        assert_eq!(job.payload.as_bytes(), Some(b"abc".as_slice()));
    }

    #[test]
    fn into_bytes_returns_the_mime_and_the_same_allocation() {
        let data = b"abc".to_vec();
        let ptr = data.as_ptr();
        let (mime, out) = ClipboardJob::new("text/html".to_owned(), data).into_bytes(16).unwrap();
        assert_eq!((mime.as_str(), out.as_slice()), ("text/html", b"abc".as_slice()));
        assert_eq!(out.as_ptr(), ptr);
    }

    #[test]
    fn into_bytes_enforces_the_bound_and_rejects_nothing() {
        assert!(ClipboardJob::new("text/plain".to_owned(), vec![0; 17]).into_bytes(16).is_err());
        assert!(ClipboardJob::new("text/plain".to_owned(), Vec::new()).into_bytes(16).is_err());
    }
}
