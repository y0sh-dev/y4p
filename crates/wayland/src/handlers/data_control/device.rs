// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/wayland/src/handlers/data_control/device.rs

use wayland_client::{Dispatch, Connection, QueueHandle, Proxy};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_offer_v1::ExtDataControlOfferV1,
};
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use crate::state::{WaylandState, OfferData, ClipboardJob};
use crate::ingest::{self, Enqueue, IngestStats, ReaderPermit};
use crate::core::constants::*;
use crate::core::payload::{self, Payload, PayloadError, PayloadRoute};
use crate::core::pipe::DeadlineReader;
use super::{make_pipe, is_sensitive};

impl Dispatch<ExtDataControlDeviceV1, ()> for WaylandState {
    fn event(state: &mut Self, _: &ExtDataControlDeviceV1, ev: ext_data_control_device_v1::Event, _: &(), conn: &Connection, _: &QueueHandle<Self>) {
        if let ext_data_control_device_v1::Event::Selection { id } = ev {
            // Update synchronisation status
            state.selection_received = true;

            // A selection caused by our own restore is deliberately *not*
            // suppressed here: storage deduplicates by content hash, so
            // re-ingesting it merely touches the existing row (timestamp/MRU)
            // and never adds a duplicate, whereas a counter could not tell that
            // apart from a genuine copy taken right after a restore — which it
            // would drop.

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

            // Action Mode's `--list-types`: the offer is the answer. Record
            // the advertised types and stop before any candidate selection,
            // reader slot or pipe allocation.
            if capture_offered_mimes(state, &mimes) { return; }

            // Every MIME worth requesting for this selection, best first: the
            // ones matching an explicit request (Action Mode), or the
            // allowlisted chain `MIME_PRIORITY_ORDER` → `image/*` → `text/*`
            // (daemon ingestion). Offers outside that chain
            // (`application/octet-stream`, `TARGETS`, untyped blobs, ...) never
            // become candidates at all — see `select_mime_candidates`.
            let drop_rtf = state.config.should_drop_rtf();
            let candidates = select_mime_candidates(&mimes, state.target_mime.as_deref(), drop_rtf);
            if candidates.is_empty() { return; }

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

            // Offload ingestion and persistence to the worker thread, pipe
            // allocation included: a sender that does not implement a MIME
            // closes its end without writing a byte, so the next candidate is
            // tried on the same selection instead of losing the copy.
            if let Some(ref tx) = state.job_tx {
                let job_tx_clone = tx.clone();
                let stats = Arc::clone(&state.ingest_stats);
                let enqueue_wait = Duration::from_millis(INGEST_ENQUEUE_WAIT_MS);
                let read_timeout = Duration::from_millis(INGEST_READ_TIMEOUT_MS);
                // `offer` and `conn` are needed per candidate, so the thread
                // owns them: the proxy moves in, and the connection is cloned
                // (threads outlive this handler).
                let conn = conn.clone();
                // `Builder::spawn` reports a refused thread instead of
                // panicking like `thread::spawn`; the closure (and with it the
                // permit and the candidate list) is then dropped, which frees
                // everything.
                let spawned = std::thread::Builder::new().name("y4p-ingest".to_owned()).spawn(move || {
                    ingest_candidates(
                        &candidates,
                        &job_tx_clone,
                        &stats,
                        permit,
                        enqueue_wait,
                        read_timeout,
                        |mime| request_pipe(&offer, &conn, mime),
                    );
                });
                if spawned.is_err() {
                    let total = state.ingest_stats.record_shed_reader();
                    ingest::report_shed("could not start a reader thread", total);
                }
            } else {
                // Action Mode: synchronous read for immediate CLI processing.
                // The same candidate chain, boundary guard and normalisation as
                // the daemon path, so `paste-from` returns exactly the bytes
                // that would otherwise have been persisted.
                let deadline = Instant::now() + Duration::from_millis(INGEST_READ_TIMEOUT_MS);
                if let FallbackOutcome::Ready { mime, payload } =
                    read_first_candidate(&candidates, deadline, |mime| request_pipe(&offer, conn, mime))
                    && let Ok((_, bytes)) = ClipboardJob::from_payload(mime, payload).into_bytes(MAX_PAYLOAD_BYTES)
                {
                    state.rx_buf = bytes;
                }
            }
        }
    }

    // Bind OfferData to each new DataOffer instance for isolated MIME tracking
    wayland_client::event_created_child!(WaylandState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, OfferData { mimes: Arc::new(Mutex::new(Vec::new())) })
    ]);
}

/// Accounts for a selection that could not be read because no pipe could be
/// allocated (descriptor exhaustion), and returns the running shed total.
fn shed_unallocatable_pipe(stats: &IngestStats) -> u64 {
    let total = stats.record_shed_reader();
    ingest::report_shed("could not allocate pipe descriptors (EMFILE)", total);
    total
}

/// Records an offer's MIME types when Action Mode asked for a list
/// (`y4p paste-from --list-types`).
///
/// Returns `true` when the selection is fully handled: the caller must then
/// return before selecting candidates, taking a `ReaderGate` permit or
/// allocating a pipe. `false` means the normal payload path continues.
fn capture_offered_mimes(state: &mut WaylandState, mimes: &[String]) -> bool {
    if !state.list_types {
        return false;
    }
    state.offered_mimes = mimes.to_vec();
    true
}

/// Chooses which of an offer's `mimes` to request from the compositor.
///
/// `target_mime` is `Some` only in Action Mode (`y4p paste-from <mime>`
/// waits for one specific MIME): the matching offers are found via
/// `core::utils::mime_base_eq` (case- and whitespace-insensitive, parameters
/// ignored), so a sender announcing e.g. "TEXT/Plain" or
/// "text/plain; charset=utf-8" still satisfies a request for "text/plain".
/// `drop_rtf` is not consulted there — an explicit request names exactly the
/// MIME the caller wants, RTF included, and that request is honoured as-is.
///
/// `target_mime` is `None` in daemon ingestion, where no single MIME is
/// being awaited. The result is then the **strict allowlist**, in the order it
/// should be tried:
///
/// 1. anything matching an entry of `MIME_PRIORITY_ORDER`, in that order;
/// 2. the remaining `image/*` offers;
/// 3. the remaining `text/*` offers (`drop_rtf` removes RTF from this step).
///
/// Anything outside those three groups — `application/octet-stream`,
/// `application/x-*`, `TARGETS`, an untyped blob — is dropped: a sender that
/// offers nothing y4p can actually use should cost no transfer attempt, and an
/// opaque binary of unknown meaning must never be persisted as if it were a
/// clipboard entry. There is deliberately no catch-all after the text step.
///
/// Offers are deduplicated by base type, so a sender announcing both
/// `text/plain` and `text/plain;charset=utf-8` yields one candidate: the same
/// bytes would otherwise be requested twice, and the second attempt could only
/// ever fail.
pub(crate) fn select_mime_candidates(mimes: &[String], target_mime: Option<&str>, drop_rtf: bool) -> Vec<String> {
    let mut candidates = Vec::new();

    if let Some(target) = target_mime {
        for mime in mimes {
            if crate::core::utils::mime_base_eq(mime, target) {
                push_candidate(&mut candidates, mime);
            }
        }
        return candidates;
    }

    for &preferred in MIME_PRIORITY_ORDER {
        for mime in mimes {
            if crate::core::utils::mime_base_eq(mime, preferred) {
                push_candidate(&mut candidates, mime);
            }
        }
    }
    for mime in mimes {
        if crate::core::utils::starts_with_ignore_ascii_case(mime, "image/") {
            push_candidate(&mut candidates, mime);
        }
    }
    for mime in mimes {
        if crate::core::utils::starts_with_ignore_ascii_case(mime, "text/")
            && (!drop_rtf || !crate::core::utils::is_rtf_mime(mime))
        {
            push_candidate(&mut candidates, mime);
        }
    }

    candidates
}

/// Appends `mime` unless an offer of the same base type is already listed.
///
/// The first offer of a base type wins, so the order of `mimes` decides between
/// e.g. `text/plain;charset=utf-8` and `text/plain` when both are present.
fn push_candidate(candidates: &mut Vec<String>, mime: &str) {
    if !candidates.iter().any(|listed| crate::core::utils::mime_base_eq(listed, mime)) {
        candidates.push(mime.to_owned());
    }
}

/// Allocates a pipe for one candidate and asks the compositor to fill it:
/// `offer.receive` on the write end, a flush, and the read end back. `None`
/// means no descriptor could be allocated (`EMFILE`/`ENFILE`).
fn request_pipe(offer: &ExtDataControlOfferV1, conn: &Connection, mime: &str) -> Option<std::fs::File> {
    let (read_file, write_fd) = make_pipe()?;
    offer.receive(mime.to_owned(), write_fd.as_fd());
    drop(write_fd);
    let _ = conn.flush();
    Some(read_file)
}

/// What trying a selection's candidate array produced.
enum FallbackOutcome {
    /// A candidate delivered a payload, under the MIME it should be stored as.
    Ready {
        mime: String,
        payload: Payload,
    },
    /// Every candidate failed to deliver anything: an empty transfer, a read
    /// error, or a payload that normalised away to nothing.
    Exhausted,
    /// A candidate's read hit the selection's deadline.
    TimedOut,
    /// No pipe could be allocated for the candidate being tried.
    NoPipe,
}

/// Tries each candidate in turn until one delivers a usable payload.
///
/// `open` produces the read end for one candidate: in production it allocates a
/// pipe and asks the compositor to transfer into it (`request_pipe`), in tests
/// it hands back a prepared pipe. That is the only part of the loop that needs
/// a live Wayland connection, which is what makes the fallback testable without
/// a compositor.
///
/// `deadline` is shared across the whole array: a selection has one patience
/// budget, not one per candidate, so a sender that stalls on the first MIME
/// cannot extend the read indefinitely by offering more of them. A `Failed`
/// candidate — 0-byte EOF included — moves on to the next, because a sender
/// that does not implement a MIME typically closes its end without writing a
/// byte; a timed-out one ends the selection, since the sender, not the MIME,
/// is the problem.
fn read_first_candidate<F>(
    candidates: &[String],
    deadline: Instant,
    mut open: F,
) -> FallbackOutcome
where
    F: FnMut(&str) -> Option<std::fs::File>,
{
    for mime in candidates {
        let Some(read_file) = open(mime) else { return FallbackOutcome::NoPipe };
        match drain_pipe(read_file, mime, MAX_PAYLOAD_BYTES, deadline) {
            DrainOutcome::Ready(bytes) => {
                // A payload that normalises away to nothing (an empty URI list,
                // say) is no better than an empty read: the next candidate may
                // still have something usable.
                if let Some((mime, data)) = normalise_payload(mime.clone(), bytes) {
                    return FallbackOutcome::Ready { mime, payload: Payload::memory(data) };
                }
            }
            DrainOutcome::TimedOut => return FallbackOutcome::TimedOut,
            DrainOutcome::Failed => {}
        }
    }
    FallbackOutcome::Exhausted
}

/// Tries a selection's `candidates` in order and forwards the first usable
/// payload to the daemon's worker.
///
/// Factored out of the Selection handler so it can be called from the
/// dedicated per-selection thread the handler spawns. This layer only drains
/// the pipes and normalises: image transcoding and fingerprinting are the
/// worker's concern, so this thread never spawns `magick` or hashes anything
/// itself.
///
/// Every payload is read into memory here, bounded by `MAX_PAYLOAD_BYTES`, so
/// the queue carries the bytes themselves. The pipe is emptied promptly and
/// under `_permit`, so the sending application is never left blocked behind
/// the worker queue.
///
/// `_permit` is held until the payload is queued or shed, so a full queue
/// keeps its readers' gate slots occupied: that is the backpressure which
/// makes new selections get refused instead of piling up payloads in memory.
/// A shed job is dropped here, which frees its buffer.
#[allow(clippy::too_many_arguments)]
fn ingest_candidates<F>(
    candidates: &[String],
    job_tx: &SyncSender<ClipboardJob>,
    stats: &IngestStats,
    _permit: Option<ReaderPermit>,
    enqueue_wait: Duration,
    read_timeout: Duration,
    open: F,
) where
    F: FnMut(&str) -> Option<std::fs::File>,
{
    let deadline = Instant::now() + read_timeout;
    let job = match read_first_candidate(candidates, deadline, open) {
        FallbackOutcome::Ready { mime, payload } => {
            // A selection that carries nothing but whitespace — the stray
            // empty line or run of blanks an editor or terminal copies — is
            // not worth a history entry. This is a deliberate ingestion
            // filter, not overload, so nothing is counted as shed; the permit
            // is simply released with this stack frame.
            //
            // Only this daemon path filters: Action Mode calls
            // `read_first_candidate` directly, so `y4p paste-from` still
            // returns such a selection verbatim.
            if payload.as_bytes().is_some_and(|data| is_whitespace_only_text(&mime, data)) {
                return;
            }
            ClipboardJob::from_payload(mime, payload)
        }
        FallbackOutcome::TimedOut => {
            let total = stats.record_shed_timeout();
            ingest::report_shed("pipe read timed out", total);
            return;
        }
        FallbackOutcome::NoPipe => {
            shed_unallocatable_pipe(stats);
            return;
        }
        FallbackOutcome::Exhausted => return,
    };

    if ingest::enqueue(job_tx, job, enqueue_wait) == Enqueue::Shed {
        let total = stats.record_shed_queue();
        ingest::report_shed("worker queue full", total);
    }
}

/// True when `data` is a text-like payload whose whole content is whitespace.
///
/// `mime` gates the check: a binary payload must never be decoded as UTF-8 in
/// search of spaces, and an image of near-blank pixels is still an image. Only
/// bytes that are valid UTF-8 and trim to nothing (spaces, tabs, newlines,
/// carriage returns and other Unicode whitespace alike) qualify, so an
/// undecodable byte sequence is always kept rather than silently dropped.
fn is_whitespace_only_text(mime: &str, data: &[u8]) -> bool {
    crate::core::utils::is_text_like_mime(mime) && std::str::from_utf8(data).is_ok_and(|s| s.trim().is_empty())
}

enum DrainOutcome {
    /// The whole payload, in memory.
    Ready(Vec<u8>),
    TimedOut,
    Failed,
}

fn classify_payload_error(err: PayloadError) -> DrainOutcome {
    match err {
        PayloadError::Io(ref e) if e.kind() == io::ErrorKind::TimedOut => DrainOutcome::TimedOut,
        _ => DrainOutcome::Failed,
    }
}

/// Drains `reader` completely into memory, under the selection's deadline and
/// the payload byte limit.
///
/// There is one road: [`DeadlineReader`] bounds the wall-clock time a stalled
/// sender can hold the reader, and `read_pipe_payload` bounds the size. Text is
/// normalised and indexed as bytes anyway, and every payload is small enough to
/// live in a `Vec` now that `MAX_PAYLOAD_BYTES` is 32 MiB.
fn drain_pipe(reader: std::fs::File, mime: &str, limit: u64, deadline: Instant) -> DrainOutcome {
    let Ok(deadline_reader) = DeadlineReader::new(reader, deadline) else {
        return DrainOutcome::Failed;
    };
    match read_pipe_payload(deadline_reader, mime, limit) {
        Ok(bytes) => DrainOutcome::Ready(bytes),
        Err(err) => classify_payload_error(err),
    }
}

/// Reads at most `limit` bytes from an already-`offer.receive()`d pipe, or
/// returns `PayloadError` on an I/O error, timeout, empty read, or an
/// over-long stream (the partial bytes are discarded, never persisted as if
/// complete: see `core::payload::read_bounded` for the probe-byte rule that
/// makes an over-long stream distinguishable from one that exactly fits).
///
/// A Wayland pipe carries no length, so the buffer is pre-sized from the MIME
/// alone (`core::payload::route_payload`): a text-like payload is almost always
/// below one page, so reserving a page up front replaces the read loop's
/// repeated regrowth by one allocation. Anything else starts empty and grows,
/// since it may be a multi-megabyte image.
fn read_pipe_payload<R: Read>(reader: R, mime: &str, limit: u64) -> Result<Vec<u8>, PayloadError> {
    let capacity_hint = match payload::route_payload(mime, None) {
        PayloadRoute::Inline => Some(INLINE_PAYLOAD_THRESHOLD_BYTES as u64),
        PayloadRoute::Stream => None,
    };
    payload::read_bounded(reader, limit, capacity_hint)
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

    fn config() -> Arc<crate::core::config::Config> {
        Arc::new(crate::core::config::Config::default())
    }

    // --- Action Mode: listing offered types instead of a payload ---

    #[test]
    fn list_types_mode_records_the_offered_mimes_and_stops() {
        let mut state = WaylandState::new_list_types(false, config());
        let offers = mimes(&["image/png", "text/plain", "TARGETS"]);
        assert!(capture_offered_mimes(&mut state, &offers), "the offer ends the selection");
        assert_eq!(state.offered_mimes, offers);
        assert_eq!(state.reader_gate.active(), 0, "no reader slot may be taken");
    }

    #[test]
    fn the_payload_path_leaves_the_offered_mimes_alone() {
        let mut state = WaylandState::new_action("text/plain".to_owned(), false, config());
        let offers = mimes(&["text/plain"]);
        assert!(!capture_offered_mimes(&mut state, &offers), "the payload path continues");
        assert!(state.offered_mimes.is_empty());
    }

    /// The best candidate only, for the assertions that are about a single
    /// choice rather than the whole array.
    fn select_mime(mimes: &[String], target_mime: Option<&str>, drop_rtf: bool) -> Option<String> {
        select_mime_candidates(mimes, target_mime, drop_rtf).into_iter().next()
    }

    // --- one-pipe ingestion: bounded hand-off, shedding and permit lifetime ---

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

    #[test]
    fn make_pipe_hands_out_an_enlarged_pipe_when_the_system_allows_it() {
        let (read_file, write_fd) = make_pipe().unwrap();
        // SAFETY: `F_GETPIPE_SZ` takes no argument and only reads a property of
        // the open descriptor it is given.
        let capacity = unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&read_file), libc::F_GETPIPE_SZ) };
        let capacity = usize::try_from(capacity).unwrap();
        let max = std::fs::read_to_string("/proc/sys/fs/pipe-max-size").ok().and_then(|s| s.trim().parse::<usize>().ok());

        assert!(capacity >= PIPE_DEFAULT_BYTES);
        if max.is_none_or(|max| max > PIPE_DEFAULT_BYTES) {
            assert!(capacity > PIPE_DEFAULT_BYTES, "the ingest pipe should be enlarged, got {capacity}");
        }
        // Either way it works: the bytes come out as they went in.
        let mut writer = std::fs::File::from(write_fd);
        writer.write_all(b"through the tuned pipe").unwrap();
        drop(writer);
        let mut got = String::new();
        std::io::Read::read_to_string(&mut &read_file, &mut got).unwrap();
        assert_eq!(got, "through the tuned pipe");
    }

    fn gate_with_one_permit() -> (Arc<ReaderGate>, Option<ReaderPermit>) {
        let gate = Arc::new(ReaderGate::new(1));
        let permit = gate.try_acquire();
        assert!(permit.is_some());
        assert_eq!(gate.active(), 1);
        (gate, permit)
    }

    /// Runs the production ingestion entry point over one already-filled pipe:
    /// the shape most of these tests use, now that a selection is a candidate
    /// array rather than a single MIME.
    fn ingest_one(
        pipe: std::fs::File,
        mime: String,
        job_tx: &SyncSender<ClipboardJob>,
        stats: &IngestStats,
        permit: Option<ReaderPermit>,
        enqueue_wait: Duration,
    ) {
        ingest_one_with_timeout(
            pipe,
            mime,
            job_tx,
            stats,
            permit,
            enqueue_wait,
            Duration::from_millis(INGEST_READ_TIMEOUT_MS),
        );
    }

    /// The same, with an explicit read deadline (`Duration::ZERO` for a queue
    /// test that must not wait on the pipe).
    fn ingest_one_with_timeout(
        pipe: std::fs::File,
        mime: String,
        job_tx: &SyncSender<ClipboardJob>,
        stats: &IngestStats,
        permit: Option<ReaderPermit>,
        enqueue_wait: Duration,
        read_timeout: Duration,
    ) {
        let mut pipe = Some(pipe);
        ingest_candidates(&[mime], job_tx, stats, permit, enqueue_wait, read_timeout, |_| pipe.take());
    }

    /// The single job a test expects in a queue that holds one payload.
    fn only_job(rx: &std::sync::mpsc::Receiver<ClipboardJob>) -> ClipboardJob {
        let job = rx.try_recv().unwrap();
        assert!(rx.try_recv().is_err(), "exactly one job expected");
        job
    }

    #[test]
    fn ingest_one_queues_the_normalised_payload_and_releases_the_permit() {
        let (tx, rx) = sync_channel::<ClipboardJob>(2);
        let stats = IngestStats::default();
        let (gate, permit) = gate_with_one_permit();

        ingest_one(filled_pipe(b"hello"), "text/plain".to_owned(), &tx, &stats, permit, Duration::ZERO);

        let queued = rx.try_recv().unwrap();
        assert_eq!((queued.mime.as_str(), queued.payload.as_bytes()), ("text/plain", Some(b"hello".as_slice())));
        assert_eq!(stats.shed_queue(), 0);
        assert_eq!(gate.active(), 0, "the permit must be returned once the payload is queued");
    }

    #[test]
    fn ingest_one_sheds_the_newest_payload_when_the_queue_stays_full() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = IngestStats::default();
        tx.send(ClipboardJob::new("text/plain".to_owned(), b"older".to_vec())).unwrap();
        let (gate, permit) = gate_with_one_permit();

        let wait = Duration::from_millis(30);
        let started = Instant::now();
        ingest_one(filled_pipe(b"newer"), "text/plain".to_owned(), &tx, &stats, permit, wait);

        assert!(started.elapsed() >= wait, "backpressure: the reader must wait before shedding");
        assert_eq!(stats.shed_queue(), 1);
        assert_eq!(rx.try_recv().unwrap().payload.as_bytes(), Some(b"older".as_slice()), "queued work is never displaced");
        assert!(rx.try_recv().is_err(), "the shed payload must not have been queued");
        assert_eq!(gate.active(), 0, "a shed reader still returns its permit");
    }

    #[test]
    fn ingest_one_holds_its_permit_while_it_waits_for_queue_room() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = Arc::new(IngestStats::default());
        tx.send(ClipboardJob::new("text/plain".to_owned(), b"older".to_vec())).unwrap();
        let (gate, permit) = gate_with_one_permit();

        let reader = {
            let stats = Arc::clone(&stats);
            let pipe = filled_pipe(b"newer");
            std::thread::spawn(move || {
                ingest_one(pipe, "text/plain".to_owned(), &tx, &stats, permit, Duration::from_secs(10));
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
    fn ingest_one_discards_empty_transfers_without_counting_them_as_shed() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = IngestStats::default();
        let (gate, permit) = gate_with_one_permit();

        ingest_one(filled_pipe(b""), "text/plain".to_owned(), &tx, &stats, permit, Duration::ZERO);

        assert!(rx.try_recv().is_err());
        assert_eq!(stats.shed_queue(), 0, "an empty read is not overload");
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn ingest_one_with_a_gone_worker_is_not_counted_as_overload() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        drop(rx);
        let stats = IngestStats::default();
        let (gate, permit) = gate_with_one_permit();

        ingest_one(filled_pipe(b"hello"), "text/plain".to_owned(), &tx, &stats, permit, Duration::from_secs(10));

        assert_eq!(stats.shed_queue(), 0, "shutdown is not shedding");
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn a_large_non_text_payload_is_read_whole_into_memory() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let (read_file, write_fd) = make_pipe().unwrap();
        let data: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
        let sent = data.clone();
        let feeder = std::thread::spawn(move || {
            let mut writer = std::fs::File::from(write_fd);
            let _ = writer.write_all(&sent);
        });

        ingest_one(read_file, "application/octet-stream".to_owned(), &tx, &IngestStats::default(), None, Duration::ZERO);
        feeder.join().unwrap();

        let job = rx.try_recv().unwrap();
        assert_eq!(job.mime, "application/octet-stream", "the announced label is kept");
        assert_eq!(job.payload.as_bytes(), Some(data.as_slice()), "every byte must arrive in order");
    }

    #[test]
    fn a_missing_pipe_is_counted_as_a_shed_reader() {
        let stats = IngestStats::default();
        assert_eq!(shed_unallocatable_pipe(&stats), 1);
        assert_eq!(shed_unallocatable_pipe(&stats), 2);
        assert_eq!((stats.shed_readers(), stats.shed_queue()), (2, 0));
    }

    #[test]
    fn a_stalled_pipe_times_out_releases_the_permit_and_records_a_timeout_shed() {
        let (gate, permit) = gate_with_one_permit();
        let (read_pipe, _write_pipe) = std::io::pipe().unwrap();
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = IngestStats::default();

        ingest_one_with_timeout(
            std::fs::File::from(std::os::fd::OwnedFd::from(read_pipe)),
            "text/plain".to_owned(),
            &tx,
            &stats,
            permit,
            Duration::ZERO,
            Duration::from_millis(30),
        );

        assert_eq!(stats.shed_timeout(), 1);
        assert_eq!(stats.shed_queue(), 0);
        assert_eq!(gate.active(), 0, "timed-out reader must release its ReaderGate slot");
        assert!(rx.try_recv().is_err(), "partial or timed-out read must not be queued");
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
    fn select_mime_daemon_fallback_rejects_unlisted_blobs() {
        // Not in MIME_PRIORITY_ORDER, and neither an "image/" nor a "text/"
        // prefix: the strict allowlist has no catch-all, so an opaque blob is
        // never requested (and so never persisted).
        for blob in ["application/octet-stream", "application/x-custom", "TARGETS", "unknown"] {
            let offers = mimes(&[blob]);
            assert_eq!(select_mime(&offers, None, true), None, "{blob} must not be selected");
            assert!(select_mime_candidates(&offers, None, true).is_empty(), "{blob} must not become a candidate");
        }
    }

    #[test]
    fn select_mime_candidates_lists_every_allowlisted_offer_in_priority_order() {
        // Deliberately announced in the opposite order to the one they must be
        // tried in. No duplicates, and nothing unlisted leaks in.
        let offers = mimes(&[
            "application/octet-stream",
            "text/markdown",
            "image/jpeg",
            "text/plain",
            "image/png",
            "application/x-custom",
        ]);
        assert_eq!(
            select_mime_candidates(&offers, None, true),
            mimes(&["image/png", "image/jpeg", "text/plain", "text/markdown"]),
            "priority entries first, then the remaining images, then the remaining text"
        );
    }

    #[test]
    fn select_mime_candidates_deduplicates_offers_of_the_same_base_type() {
        let offers = mimes(&["text/plain;charset=utf-8", "text/plain", "TEXT/Plain"]);
        let candidates = select_mime_candidates(&offers, None, true);
        assert_eq!(candidates.len(), 1, "one base type, one attempt: {candidates:?}");
        assert!(crate::core::utils::mime_base_eq(&candidates[0], "text/plain"));
    }

    #[test]
    fn select_mime_candidates_in_action_mode_returns_every_match() {
        // An explicit request returns the offers that satisfy it — and only
        // those — without consulting `drop_rtf`.
        let offers = mimes(&["text/plain;charset=utf-8", "text/html", "text/plain"]);
        let candidates = select_mime_candidates(&offers, Some("TEXT/PLAIN"), true);
        assert_eq!(candidates, mimes(&["text/plain;charset=utf-8"]), "the first offer of the base type, then no duplicate");
    }

    #[test]
    fn select_mime_drop_rtf_excludes_rtf_from_daemon_fallback() {
        let offers = mimes(&["text/rtf"]);
        assert_eq!(select_mime(&offers, None, true), None);
        assert!(select_mime_candidates(&offers, None, true).is_empty());
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

    // --- candidate fallback loop ---

    #[test]
    fn a_candidate_that_delivers_nothing_falls_back_to_the_next_one() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = IngestStats::default();
        let candidates = mimes(&["image/png", "text/plain"]);
        let mut tried = Vec::new();

        ingest_candidates(&candidates, &tx, &stats, None, Duration::ZERO, Duration::from_secs(5), |mime| {
            tried.push(mime.to_owned());
            // The sender announces image/png but has nothing to say for it —
            // the same shape as a Chromium client's empty text/html.
            Some(filled_pipe(if mime == "image/png" { b"" } else { b"the fallback text" }))
        });

        assert_eq!(tried, candidates, "the empty candidate is tried, then the next");
        let job = only_job(&rx);
        assert_eq!((job.mime.as_str(), job.payload.as_bytes()), ("text/plain", Some(b"the fallback text".as_slice())));
        assert_eq!((stats.shed_timeout(), stats.shed_queue()), (0, 0), "falling back is not overload");
    }

    #[test]
    fn a_candidate_that_times_out_ends_the_selection_and_counts_it() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = IngestStats::default();
        let candidates = mimes(&["image/png", "text/plain"]);
        // The write end stays open, so the pipe never reaches EOF: the read can
        // only end at the selection's deadline.
        let (stalled, _write_end) = std::io::pipe().unwrap();
        let mut stalled = Some(std::fs::File::from(std::os::fd::OwnedFd::from(stalled)));
        let mut tried = Vec::new();

        ingest_candidates(&candidates, &tx, &stats, None, Duration::ZERO, Duration::from_millis(30), |mime| {
            tried.push(mime.to_owned());
            match stalled.take() {
                Some(pipe) => Some(pipe),
                None => Some(filled_pipe(b"never reached")),
            }
        });

        assert_eq!(tried, mimes(&["image/png"]), "a stalled sender ends the selection; later candidates are not tried");
        assert_eq!(stats.shed_timeout(), 1);
        assert_eq!(stats.shed_queue(), 0);
        assert!(rx.try_recv().is_err(), "nothing is queued for a timed-out selection");
    }

    #[test]
    fn a_candidate_that_normalises_away_falls_back_to_the_next_one() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let candidates = mimes(&["text/uri-list", "text/plain"]);

        ingest_candidates(&candidates, &tx, &IngestStats::default(), None, Duration::ZERO, Duration::from_secs(5), |mime| {
            Some(filled_pipe(if mime == "text/uri-list" { b"# only a comment" } else { b"usable text" }))
        });

        assert_eq!(only_job(&rx).payload.as_bytes(), Some(b"usable text".as_slice()), "an empty URI list is no payload; the next candidate is");
    }

    #[test]
    fn no_allocatable_pipe_ends_the_selection_and_counts_a_shed_reader() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = IngestStats::default();
        let candidates = mimes(&["image/png", "text/plain"]);

        ingest_candidates(&candidates, &tx, &stats, None, Duration::ZERO, Duration::from_secs(5), |_| None);

        assert_eq!(stats.shed_readers(), 1, "EMFILE is a shed reader, as before");
        assert_eq!(stats.shed_timeout(), 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_selection_no_candidate_can_satisfy_is_not_counted_as_overload() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = IngestStats::default();
        let candidates = mimes(&["image/png", "text/plain"]);

        ingest_candidates(&candidates, &tx, &stats, None, Duration::ZERO, Duration::from_secs(5), |_| Some(filled_pipe(b"")));

        assert!((stats.shed_readers(), stats.shed_timeout(), stats.shed_queue()) == (0, 0, 0), "an unusable selection is not overload");
        assert!(rx.try_recv().is_err());
    }

    // --- whitespace-only ingestion filter ---

    #[test]
    fn whitespace_only_text_is_neither_queued_nor_counted_as_shed() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = IngestStats::default();
        let (gate, permit) = gate_with_one_permit();

        ingest_one(filled_pipe(b"   \n\t  "), "text/plain".to_owned(), &tx, &stats, permit, Duration::ZERO);

        assert!(rx.try_recv().is_err(), "a blank selection must not reach the worker");
        assert_eq!((stats.shed_readers(), stats.shed_timeout(), stats.shed_queue()), (0, 0, 0), "filtering is not overload");
        assert_eq!(gate.active(), 0, "the permit is released with the dropped payload");
    }

    #[test]
    fn whitespace_only_legacy_string_atoms_are_filtered_too() {
        for mime in ["UTF8_STRING", "STRING", "text/plain; charset=utf-8"] {
            let (tx, rx) = sync_channel::<ClipboardJob>(1);
            ingest_one(filled_pipe(b"\n\r\n \t"), mime.to_owned(), &tx, &IngestStats::default(), None, Duration::ZERO);
            assert!(rx.try_recv().is_err(), "{mime} whitespace must not be queued");
        }
    }

    #[test]
    fn text_with_one_visible_character_is_still_queued() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        ingest_one(filled_pipe(b" a "), "text/plain".to_owned(), &tx, &IngestStats::default(), None, Duration::ZERO);
        assert_eq!(only_job(&rx).payload.as_bytes(), Some(b" a ".as_slice()));
    }

    #[test]
    fn whitespace_bytes_under_a_binary_mime_are_still_queued() {
        // `is_text_like_mime` gates the filter: a binary payload is never
        // decoded as UTF-8 in search of spaces, so these bytes stay.
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        ingest_one(filled_pipe(b"   \n"), "image/png".to_owned(), &tx, &IngestStats::default(), None, Duration::ZERO);
        assert_eq!(only_job(&rx).payload.as_bytes(), Some(b"   \n".as_slice()));
    }

    #[test]
    fn an_undecodable_text_payload_is_still_queued() {
        // Invalid UTF-8 cannot be proven blank, so it is kept rather than
        // silently dropped.
        let bytes = b"\xff\xfe   \n";
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        ingest_one(filled_pipe(bytes), "text/plain".to_owned(), &tx, &IngestStats::default(), None, Duration::ZERO);
        assert_eq!(only_job(&rx).payload.as_bytes(), Some(bytes.as_slice()));
    }

    #[test]
    fn action_mode_still_returns_a_whitespace_only_selection() {
        // `y4p paste-from` is an explicit request, so it gets exactly what the
        // selection holds — blank lines and all.
        let candidates = mimes(&["text/plain"]);
        let outcome = read_first_candidate(&candidates, Instant::now() + Duration::from_secs(5), |_| Some(filled_pipe(b"   \n")));

        let FallbackOutcome::Ready { mime, payload } = outcome else {
            panic!("Action Mode must receive the payload verbatim");
        };
        assert_eq!(mime, "text/plain");
        assert_eq!(payload.as_bytes(), Some(b"   \n".as_slice()));
    }

    // --- read_pipe_payload ---

    #[test]
    fn read_pipe_payload_within_limit_returns_payload() {
        let data = b"hello".as_slice();
        assert_eq!(read_pipe_payload(data, "text/plain", 10).ok(), Some(b"hello".to_vec()));
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
        assert_eq!(read_pipe_payload(data, "text/plain", 5).ok(), Some(b"abcde".to_vec()));
    }

    #[test]
    fn read_pipe_payload_exceeding_limit_by_one_byte_is_discarded() {
        let data = b"abcdef".as_slice();
        assert_eq!(read_pipe_payload(data, "text/plain", 5).ok(), None);
    }

    #[test]
    fn read_pipe_payload_massively_oversized_stream_is_discarded_not_truncated() {
        // The crux of the truncation vulnerability: a stream far larger
        // than the limit must be rejected outright, never silently cut
        // down to `limit` bytes and handed back as if it were complete.
        let data = vec![b'x'; 1000];
        assert_eq!(read_pipe_payload(data.as_slice(), "text/plain", 5).ok(), None);
    }

    #[test]
    fn read_pipe_payload_empty_input_returns_none() {
        let data = b"".as_slice();
        assert_eq!(read_pipe_payload(data, "text/plain", 10).ok(), None);
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
