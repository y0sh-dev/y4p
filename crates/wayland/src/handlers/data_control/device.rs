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
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use crate::state::{WaylandState, OfferData, ClipboardJob};
use crate::ingest::{self, Enqueue, IngestStats, ReaderPermit};
use crate::core::constants::*;
use crate::core::payload::{self, PayloadRoute};
use super::{make_pipe, is_sensitive};

impl Dispatch<ExtDataControlDeviceV1, ()> for WaylandState {
    fn event(state: &mut Self, _: &ExtDataControlDeviceV1, ev: ext_data_control_device_v1::Event, _: &(), conn: &Connection, _: &QueueHandle<Self>) {
        if let ext_data_control_device_v1::Event::Selection { id } = ev {
            // Update synchronisation status
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

            // Daemon mode only: admit this reader *before* any descriptor is
            // allocated. A refusal costs nothing and cannot hit `EMFILE`;
            // the permit then lives until the payload has been queued.
            let permit = if state.job_tx.is_some() {
                match state.reader_gate.try_acquire() {
                    Some(permit) => Some(permit),
                    None => {
                        let total = state.ingest_stats.record_shed_reader();
                        ingest::report_shed("too many concurrent readers", total);
                        return;
                    }
                }
            } else {
                None
            };

            // Initialise data transfer pipe
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
                let stats = Arc::clone(&state.ingest_stats);
                let enqueue_wait = Duration::from_millis(INGEST_ENQUEUE_WAIT_MS);
                // `Builder::spawn` reports a refused thread instead of
                // panicking like `thread::spawn`; the closure (and with it the
                // pipe and the permit) is then dropped, which frees everything.
                let spawned = std::thread::Builder::new().name("y4p-ingest".to_owned()).spawn(move || {
                    ingest_and_send(read_file, mime_to_get, &job_tx_clone, &stats, permit, enqueue_wait);
                });
                if spawned.is_err() {
                    let total = state.ingest_stats.record_shed_reader();
                    ingest::report_shed("could not start a reader thread", total);
                }
            } else {
                // Action Mode: synchronous read for immediate CLI processing.
                // Applies the same boundary guard and normalisation as the
                // daemon path (`ingest_and_send`) so `paste-from` returns
                // exactly the bytes that would otherwise have been persisted.
                if let Some(payload) = read_pipe_payload(read_file, &mime_to_get, MAX_PAYLOAD_BYTES)
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

/// Reads a MIME payload from an already-`offer.receive()`d pipe, normalises
/// it (see `normalise_payload`) and forwards the raw `(mime, data)` pair to
/// the daemon's worker.
///
/// Factored out of the Selection handler so it can be called from the
/// dedicated per-selection thread the handler spawns. This layer only drains
/// the pipe and normalises: image transcoding and fingerprinting are the
/// worker's concern, so this thread never spawns `magick` or hashes anything.
///
/// `_permit` is held until the payload is queued or shed, so a full queue
/// keeps its readers' gate slots occupied: that is the backpressure which
/// makes new selections get refused instead of piling up payloads in memory.
fn ingest_and_send(
    read_file: std::fs::File,
    mime_to_get: String,
    job_tx: &SyncSender<ClipboardJob>,
    stats: &IngestStats,
    _permit: Option<ReaderPermit>,
    enqueue_wait: Duration,
) {
    let Some(payload) = read_pipe_payload(read_file, &mime_to_get, MAX_PAYLOAD_BYTES) else { return; };
    let Some((mime, data)) = normalise_payload(mime_to_get, payload) else { return; };

    if ingest::enqueue(job_tx, ClipboardJob::new(mime, data), enqueue_wait) == Enqueue::Shed {
        let total = stats.record_shed_queue();
        ingest::report_shed("worker queue full", total);
    }
}

/// Reads at most `limit` bytes from an already-`offer.receive()`d pipe, or
/// `None` on an I/O error, an empty read, or a stream that exceeded `limit`
/// (the partial bytes are discarded, never persisted as if complete: see
/// `core::payload::read_bounded` for the probe-byte rule that makes an
/// over-long stream distinguishable from one that exactly fits).
///
/// A Wayland pipe carries no length, so the buffer is pre-sized from the MIME
/// alone (`core::payload::route_payload`): a text-like payload is almost always
/// below one page, so reserving a page up front replaces the read loop's
/// repeated regrowth by one allocation. Anything else starts empty and grows,
/// since it may be a multi-megabyte image.
fn read_pipe_payload<R: Read>(reader: R, mime: &str, limit: u64) -> Option<Vec<u8>> {
    let capacity_hint = match payload::route_payload(mime, None) {
        PayloadRoute::Inline => Some(INLINE_PAYLOAD_THRESHOLD_BYTES as u64),
        PayloadRoute::Stream => None,
    };
    payload::read_bounded(reader, limit, capacity_hint).ok()
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

    // --- ingest_and_send: bounded hand-off, shedding and permit lifetime ---

    use crate::ingest::ReaderGate;
    use std::io::Write;
    use std::sync::mpsc::sync_channel;
    use std::time::Instant;

    /// A pipe whose write end already carries `bytes` and is closed, so the
    /// read end yields exactly them and then EOF (what a finished transfer looks like).
    fn filled_pipe(bytes: &[u8]) -> std::fs::File {
        let (read_file, write_fd) = make_pipe().unwrap();
        let mut writer = std::fs::File::from(write_fd);
        writer.write_all(bytes).unwrap();
        read_file
    }

    fn gate_with_one_permit() -> (Arc<ReaderGate>, Option<ReaderPermit>) {
        let gate = Arc::new(ReaderGate::new(1));
        let permit = gate.try_acquire();
        assert!(permit.is_some());
        assert_eq!(gate.active(), 1);
        (gate, permit)
    }

    #[test]
    fn ingest_and_send_queues_the_normalised_payload_and_releases_the_permit() {
        let (tx, rx) = sync_channel::<ClipboardJob>(2);
        let stats = IngestStats::default();
        let (gate, permit) = gate_with_one_permit();

        ingest_and_send(filled_pipe(b"hello"), "text/plain".to_owned(), &tx, &stats, permit, Duration::ZERO);

        let queued = rx.try_recv().unwrap();
        assert_eq!((queued.mime.as_str(), queued.payload.as_bytes()), ("text/plain", Some(b"hello".as_slice())));
        assert_eq!(stats.shed_queue(), 0);
        assert_eq!(gate.active(), 0, "the permit must be returned once the payload is queued");
    }

    #[test]
    fn ingest_and_send_sheds_the_newest_payload_when_the_queue_stays_full() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = IngestStats::default();
        tx.send(ClipboardJob::new("text/plain".to_owned(), b"older".to_vec())).unwrap();
        let (gate, permit) = gate_with_one_permit();

        let wait = Duration::from_millis(30);
        let started = Instant::now();
        ingest_and_send(filled_pipe(b"newer"), "text/plain".to_owned(), &tx, &stats, permit, wait);

        assert!(started.elapsed() >= wait, "backpressure: the reader must wait before shedding");
        assert_eq!(stats.shed_queue(), 1);
        assert_eq!(rx.try_recv().unwrap().payload.as_bytes(), Some(b"older".as_slice()), "queued work is never displaced");
        assert!(rx.try_recv().is_err(), "the shed payload must not have been queued");
        assert_eq!(gate.active(), 0, "a shed reader still returns its permit");
    }

    #[test]
    fn ingest_and_send_holds_its_permit_while_it_waits_for_queue_room() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = Arc::new(IngestStats::default());
        tx.send(ClipboardJob::new("text/plain".to_owned(), b"older".to_vec())).unwrap();
        let (gate, permit) = gate_with_one_permit();

        let reader = {
            let stats = Arc::clone(&stats);
            let pipe = filled_pipe(b"newer");
            std::thread::spawn(move || {
                ingest_and_send(pipe, "text/plain".to_owned(), &tx, &stats, permit, Duration::from_secs(10));
            })
        };

        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(gate.active(), 1, "a blocked reader keeps its slot: that is the backpressure");
        assert!(gate.try_acquire().is_none(), "so a further selection is refused instead of piling up");

        assert_eq!(rx.recv().unwrap().payload.as_bytes(), Some(b"older".as_slice()));
        reader.join().unwrap();
        assert_eq!(rx.recv().unwrap().payload.as_bytes(), Some(b"newer".as_slice()), "the waiting payload is delivered once room appears");
        assert_eq!(stats.shed_queue(), 0);
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn ingest_and_send_discards_empty_transfers_without_counting_them_as_shed() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = IngestStats::default();
        let (gate, permit) = gate_with_one_permit();

        ingest_and_send(filled_pipe(b""), "text/plain".to_owned(), &tx, &stats, permit, Duration::ZERO);

        assert!(rx.try_recv().is_err());
        assert_eq!(stats.shed_queue(), 0, "an empty read is not overload");
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn ingest_and_send_with_a_gone_worker_is_not_counted_as_overload() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        drop(rx);
        let stats = IngestStats::default();
        let (gate, permit) = gate_with_one_permit();

        ingest_and_send(filled_pipe(b"hello"), "text/plain".to_owned(), &tx, &stats, permit, Duration::from_secs(10));

        assert_eq!(stats.shed_queue(), 0, "shutdown is not shedding");
        assert_eq!(gate.active(), 0);
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

    // --- read_pipe_payload ---

    #[test]
    fn read_pipe_payload_within_limit_returns_payload() {
        let data = b"hello".as_slice();
        assert_eq!(read_pipe_payload(data, "text/plain", 10), Some(b"hello".to_vec()));
    }

    #[test]
    fn read_pipe_payload_presizes_text_to_one_page_and_leaves_binary_to_grow() {
        let text = read_pipe_payload(b"hello".as_slice(), "text/plain", 1 << 20).unwrap();
        assert_eq!(text.capacity(), INLINE_PAYLOAD_THRESHOLD_BYTES, "text is reserved one page up front");

        let image = read_pipe_payload(b"hello".as_slice(), "image/png", 1 << 20).unwrap();
        assert!(image.capacity() < INLINE_PAYLOAD_THRESHOLD_BYTES, "an unhinted binary must not pre-reserve a page");
    }

    #[test]
    fn read_pipe_payload_presize_never_exceeds_the_limit() {
        let tiny = read_pipe_payload(b"hi".as_slice(), "text/plain", 8).unwrap();
        assert!(tiny.capacity() < INLINE_PAYLOAD_THRESHOLD_BYTES, "the reservation is clamped to the limit");
        assert_eq!(tiny, b"hi");
    }

    #[test]
    fn read_pipe_payload_exactly_at_limit_returns_payload() {
        let data = b"abcde".as_slice();
        assert_eq!(read_pipe_payload(data, "text/plain", 5), Some(b"abcde".to_vec()));
    }

    #[test]
    fn read_pipe_payload_exceeding_limit_by_one_byte_is_discarded() {
        let data = b"abcdef".as_slice();
        assert_eq!(read_pipe_payload(data, "text/plain", 5), None);
    }

    #[test]
    fn read_pipe_payload_massively_oversized_stream_is_discarded_not_truncated() {
        // The crux of the truncation vulnerability: a stream far larger
        // than the limit must be rejected outright, never silently cut
        // down to `limit` bytes and handed back as if it were complete.
        let data = vec![b'x'; 1000];
        assert_eq!(read_pipe_payload(data.as_slice(), "text/plain", 5), None);
    }

    #[test]
    fn read_pipe_payload_empty_input_returns_none() {
        let data = b"".as_slice();
        assert_eq!(read_pipe_payload(data, "text/plain", 10), None);
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
}
