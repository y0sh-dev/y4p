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
use crate::ingest::{self, ByteBudget, Enqueue, IngestStats, ReaderPermit};
use crate::spool::{PayloadSpooler, SpoolOutcome};
use crate::core::constants::*;
use crate::core::payload::{self, Payload, PayloadError, PayloadRoute, SpooledFile};
use crate::core::pipe::DeadlineReader;
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
                let budget = Arc::clone(&state.byte_budget);
                let spooler = state.spooler.clone();
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
                        &budget,
                        permit,
                        enqueue_wait,
                        read_timeout,
                        spooler.as_deref(),
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
                    read_first_candidate(&candidates, deadline, None, |mime| request_pipe(&offer, conn, mime))
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
    spooler: Option<&dyn PayloadSpooler>,
    mut open: F,
) -> FallbackOutcome
where
    F: FnMut(&str) -> Option<std::fs::File>,
{
    for mime in candidates {
        let Some(read_file) = open(mime) else { return FallbackOutcome::NoPipe };
        match drain_pipe(read_file, mime, MAX_PAYLOAD_BYTES, deadline, spooler) {
            DrainOutcome::Ready(Drained::Memory(bytes)) => {
                // A payload that normalises away to nothing (an empty URI list,
                // say) is no better than an empty read: the next candidate may
                // still have something usable.
                if let Some((mime, data)) = normalise_payload(mime.clone(), bytes) {
                    return FallbackOutcome::Ready { mime, payload: Payload::memory(data) };
                }
            }
            DrainOutcome::Ready(Drained::Spooled { file, head }) => {
                return FallbackOutcome::Ready {
                    mime: relabel_by_magic(mime.clone(), &head),
                    payload: Payload::Spooled(Box::new(file)),
                };
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
/// worker's concern (or the `spooler`'s, which computes the digest while the
/// bytes stream past), so this thread never spawns `magick` or hashes anything
/// itself.
///
/// What is handed over depends on the payload (see `drain_pipe`): a small one,
/// and any text, as an in-memory `Payload::Memory`; a larger non-text one as a
/// `Payload::Spooled` file the pipe was drained into without a heap copy. Either
/// way the pipe is emptied here, promptly and under `_permit`, so the sending
/// application is never left blocked behind the worker queue.
///
/// `_permit` is held until the payload is queued or shed, so a full queue
/// keeps its readers' gate slots occupied: that is the backpressure which
/// makes new selections get refused instead of piling up payloads in memory.
/// A shed job is dropped here, which also unlinks its spool file.
///
/// The hand-over is [`ingest::enqueue_bounded`]: besides a queue slot, the
/// payload needs room in `budget`, and the bytes it reserves travel with the
/// job until the worker is done with it.
#[allow(clippy::too_many_arguments)]
fn ingest_candidates<F>(
    candidates: &[String],
    job_tx: &SyncSender<ClipboardJob>,
    stats: &IngestStats,
    budget: &Arc<ByteBudget>,
    _permit: Option<ReaderPermit>,
    enqueue_wait: Duration,
    read_timeout: Duration,
    spooler: Option<&dyn PayloadSpooler>,
    open: F,
) where
    F: FnMut(&str) -> Option<std::fs::File>,
{
    let deadline = Instant::now() + read_timeout;
    let job = match read_first_candidate(candidates, deadline, spooler, open) {
        FallbackOutcome::Ready { mime, payload } => ClipboardJob::from_payload(mime, payload),
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

    let bytes = job.accounted_bytes();
    if ingest::enqueue_bounded(job_tx, budget, job, enqueue_wait) == Enqueue::Shed {
        let total = stats.record_shed_queue();
        // A payload above the whole budget will never fit: say so, because
        // retrying the copy cannot help, unlike a merely busy worker.
        let reason = if bytes > budget.limit() { "payload exceeds the queue byte budget" } else { "worker queue or byte budget full" };
        ingest::report_shed(reason, total);
    }
}

/// What an ingestion thread took out of a compositor pipe.
enum Drained {
    /// The whole payload, in memory.
    Memory(Vec<u8>),
    /// The whole payload, on disk. `head` is its first bytes (at most
    /// `INLINE_PAYLOAD_THRESHOLD_BYTES`), kept for content sniffing.
    Spooled { file: SpooledFile, head: Vec<u8> },
}

enum DrainOutcome {
    Ready(Drained),
    TimedOut,
    Failed,
}

fn classify_payload_error(err: PayloadError) -> DrainOutcome {
    match err {
        PayloadError::Io(ref e) if e.kind() == io::ErrorKind::TimedOut => DrainOutcome::TimedOut,
        _ => DrainOutcome::Failed,
    }
}

/// Drains `reader` completely, choosing the cheapest bounded way.
///
/// - Text-like MIMEs, and every payload when there is no `spooler`, are read
///   into memory (`read_pipe_payload`): text must be normalised and indexed as
///   bytes anyway, and it is small.
/// - Anything else is read one inline threshold's worth first, into one
///   pre-sized buffer. If the stream ends inside it the payload is small and is
///   returned as memory, so the common small image never touches the disk. If
///   more is coming, the rest is handed to the `spooler`, which copies it to a
///   file in the kernel (`splice(2)`) while fingerprinting it.
/// - A spooler that cannot start gives the pipe back, and the payload is read
///   into memory after all: spooling is an optimisation, never a requirement.
fn drain_pipe(
    reader: std::fs::File,
    mime: &str,
    limit: u64,
    deadline: Instant,
    spooler: Option<&dyn PayloadSpooler>,
) -> DrainOutcome {
    let Ok(mut deadline_reader) = DeadlineReader::new(reader, deadline) else {
        return DrainOutcome::Failed;
    };
    let Some(spooler) = spooler.filter(|_| payload::route_payload(mime, None) == PayloadRoute::Stream) else {
        return match read_pipe_payload(deadline_reader, mime, limit) {
            Ok(bytes) => DrainOutcome::Ready(Drained::Memory(bytes)),
            Err(err) => classify_payload_error(err),
        };
    };

    let mut head = Vec::with_capacity(INLINE_PAYLOAD_THRESHOLD_BYTES);
    match (&mut deadline_reader).take(INLINE_PAYLOAD_THRESHOLD_BYTES as u64).read_to_end(&mut head) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::TimedOut => return DrainOutcome::TimedOut,
        Err(_) => return DrainOutcome::Failed,
    }
    if head.len() < INLINE_PAYLOAD_THRESHOLD_BYTES {
        return if head.is_empty() {
            DrainOutcome::Failed
        } else {
            DrainOutcome::Ready(Drained::Memory(head))
        };
    }

    match spooler.spool_with_deadline(&head, deadline_reader.into_inner(), limit, deadline) {
        SpoolOutcome::Spooled(file) => DrainOutcome::Ready(Drained::Spooled { file, head }),
        SpoolOutcome::Rejected => DrainOutcome::Failed,
        SpoolOutcome::TimedOut => DrainOutcome::TimedOut,
        SpoolOutcome::Unavailable(rest) => {
            let Ok(rest_reader) = DeadlineReader::new(rest, deadline) else {
                return DrainOutcome::Failed;
            };
            // The head is already out of the pipe: read the remainder behind it.
            let chained = std::io::Cursor::new(head).chain(rest_reader);
            match payload::read_bounded(chained, limit, None) {
                Ok(bytes) => DrainOutcome::Ready(Drained::Memory(bytes)),
                Err(err) => classify_payload_error(err),
            }
        }
    }
}

/// The MIME to record for a spooled payload: the one announced, unless its
/// leading bytes identify an image, in which case the bytes win (the same rule
/// `normalise_payload` applies to an in-memory payload).
fn relabel_by_magic(announced: String, head: &[u8]) -> String {
    match crate::core::utils::detect_image_mime(head) {
        Some(detected) => detected.to_owned(),
        None => announced,
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

    /// A budget no test payload can exhaust, for the tests that are not about it.
    fn roomy() -> Arc<ByteBudget> {
        Arc::new(ByteBudget::new(usize::MAX))
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
    #[allow(clippy::too_many_arguments)]
    fn ingest_one(
        pipe: std::fs::File,
        mime: String,
        job_tx: &SyncSender<ClipboardJob>,
        stats: &IngestStats,
        budget: &Arc<ByteBudget>,
        permit: Option<ReaderPermit>,
        enqueue_wait: Duration,
        spooler: Option<&dyn PayloadSpooler>,
    ) {
        ingest_one_with_timeout(
            pipe,
            mime,
            job_tx,
            stats,
            budget,
            permit,
            enqueue_wait,
            Duration::from_millis(INGEST_READ_TIMEOUT_MS),
            spooler,
        );
    }

    /// The same, with an explicit read deadline (`Duration::ZERO` for a queue
    /// test that must not wait on the pipe).
    #[allow(clippy::too_many_arguments)]
    fn ingest_one_with_timeout(
        pipe: std::fs::File,
        mime: String,
        job_tx: &SyncSender<ClipboardJob>,
        stats: &IngestStats,
        budget: &Arc<ByteBudget>,
        permit: Option<ReaderPermit>,
        enqueue_wait: Duration,
        read_timeout: Duration,
        spooler: Option<&dyn PayloadSpooler>,
    ) {
        let mut pipe = Some(pipe);
        ingest_candidates(&[mime], job_tx, stats, budget, permit, enqueue_wait, read_timeout, spooler, |_| pipe.take());
    }

    #[test]
    fn ingest_one_queues_the_normalised_payload_and_releases_the_permit() {
        let (tx, rx) = sync_channel::<ClipboardJob>(2);
        let stats = IngestStats::default();
        let (gate, permit) = gate_with_one_permit();

        ingest_one(filled_pipe(b"hello"), "text/plain".to_owned(), &tx, &stats, &roomy(), permit, Duration::ZERO, None);

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
        ingest_one(filled_pipe(b"newer"), "text/plain".to_owned(), &tx, &stats, &roomy(), permit, wait, None);

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
                ingest_one(pipe, "text/plain".to_owned(), &tx, &stats, &roomy(), permit, Duration::from_secs(10), None);
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

        ingest_one(filled_pipe(b""), "text/plain".to_owned(), &tx, &stats, &roomy(), permit, Duration::ZERO, None);

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

        ingest_one(filled_pipe(b"hello"), "text/plain".to_owned(), &tx, &stats, &roomy(), permit, Duration::from_secs(10), None);

        assert_eq!(stats.shed_queue(), 0, "shutdown is not shedding");
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn ingest_one_charges_the_queued_payload_to_the_byte_budget_until_the_job_is_dropped() {
        let (tx, rx) = sync_channel::<ClipboardJob>(2);
        let budget = Arc::new(ByteBudget::new(1024));
        ingest_one(filled_pipe(b"hello"), "text/plain".to_owned(), &tx, &IngestStats::default(), &budget, None, Duration::ZERO, None);

        assert_eq!(budget.used(), 5);
        drop(rx.try_recv().unwrap());
        assert_eq!(budget.used(), 0, "the worker dropping the job returns the bytes");
    }

    #[test]
    fn ingest_one_sheds_a_payload_that_exceeds_the_whole_budget_and_counts_it() {
        let (tx, rx) = sync_channel::<ClipboardJob>(2);
        let stats = IngestStats::default();
        let budget = Arc::new(ByteBudget::new(4));
        let started = Instant::now();

        ingest_one(filled_pipe(b"hello"), "text/plain".to_owned(), &tx, &stats, &budget, None, Duration::from_secs(10), None);

        assert!(started.elapsed() < Duration::from_secs(5), "an oversized payload must not be waited on");
        assert_eq!(stats.shed_queue(), 1);
        assert!(rx.try_recv().is_err());
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn ingest_one_sheds_when_the_byte_budget_stays_full() {
        let (tx, rx) = sync_channel::<ClipboardJob>(2);
        let stats = IngestStats::default();
        let budget = Arc::new(ByteBudget::new(8));
        let held = budget.try_reserve(6).unwrap();

        ingest_one(filled_pipe(b"hello"), "text/plain".to_owned(), &tx, &stats, &budget, None, Duration::from_millis(30), None);

        assert_eq!(stats.shed_queue(), 1);
        assert!(rx.try_recv().is_err());
        assert_eq!(budget.used(), 6, "only the unrelated reservation remains");
        drop(held);
    }

    #[test]
    fn a_spooled_job_is_charged_for_its_file_and_a_shed_one_frees_it() {
        let spooler = FakeSpooler::new(Behaviour::Spool);
        let (tx, rx) = sync_channel::<ClipboardJob>(2);
        let stats = IngestStats::default();
        let budget = Arc::new(ByteBudget::new(150_000));

        let (read_file, feeder) = fed_pipe(pattern(100_000));
        ingest_one(read_file, "application/octet-stream".to_owned(), &tx, &stats, &budget, None, Duration::ZERO, Some(&spooler));
        feeder.join().unwrap();
        assert_eq!(budget.used(), 100_000, "a spool file counts as much as a buffer");

        let (read_file, feeder) = fed_pipe(pattern(100_000));
        ingest_one(read_file, "application/octet-stream".to_owned(), &tx, &stats, &budget, None, Duration::ZERO, Some(&spooler));
        feeder.join().unwrap();
        assert_eq!(stats.shed_queue(), 1, "the second file does not fit next to the first");
        assert_eq!(budget.used(), 100_000, "the shed job returned what it had reserved");

        drop(rx.try_recv().unwrap());
        assert_eq!(budget.used(), 0);
        assert_eq!(spooler.leftovers(), 0);
    }

    #[test]
    fn a_missing_pipe_is_counted_as_a_shed_reader() {
        let stats = IngestStats::default();
        assert_eq!(shed_unallocatable_pipe(&stats), 1);
        assert_eq!(shed_unallocatable_pipe(&stats), 2);
        assert_eq!((stats.shed_readers(), stats.shed_queue()), (2, 0));
    }

    // --- spooled ingestion: head read, spooler hand-off, fallbacks ---

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Clone, Copy)]
    enum Behaviour {
        Spool,
        Reject,
        Unavailable,
    }

    /// Stands in for the daemon's spooler: writes the head and the rest of the
    /// pipe to a real file, and records what it was asked to do.
    struct FakeSpooler {
        behaviour: Behaviour,
        calls: Mutex<Vec<(usize, u64)>>,
        /// Private to this instance, so a test can count what is left in it
        /// without seeing files of tests running beside it.
        dir: PathBuf,
    }

    impl FakeSpooler {
        fn new(behaviour: Behaviour) -> Self {
            let dir = unique_path("dir");
            std::fs::create_dir(&dir).unwrap();
            Self { behaviour, calls: Mutex::new(Vec::new()), dir }
        }

        fn calls(&self) -> Vec<(usize, u64)> {
            self.calls.lock().unwrap().clone()
        }

        fn leftovers(&self) -> usize {
            std::fs::read_dir(&self.dir).unwrap().count()
        }
    }

    impl Drop for FakeSpooler {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn unique_path(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!("y4p-device-spool-{}-{}-{}", tag, std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        p
    }

    impl FakeSpooler {
        fn spool_inner(&self, head: &[u8], mut src: std::fs::File, limit: u64, deadline: Option<Instant>) -> SpoolOutcome {
            self.calls.lock().unwrap().push((head.len(), limit));
            match self.behaviour {
                Behaviour::Reject => SpoolOutcome::Rejected,
                Behaviour::Unavailable => SpoolOutcome::Unavailable(src),
                Behaviour::Spool => {
                    let path = self.dir.join("payload");
                    let mut file = std::fs::File::create(&path).unwrap();
                    file.write_all(head).unwrap();
                    let rest = match deadline {
                        Some(dl) => {
                            let mut reader = DeadlineReader::new(src, dl).unwrap();
                            std::io::copy(&mut reader, &mut file).unwrap()
                        }
                        None => std::io::copy(&mut src, &mut file).unwrap(),
                    };
                    SpoolOutcome::Spooled(SpooledFile::new(path, head.len() as u64 + rest, [7; DIGEST_LEN]))
                }
            }
        }
    }

    impl PayloadSpooler for FakeSpooler {
        fn spool(&self, head: &[u8], src: std::fs::File, limit: u64) -> SpoolOutcome {
            self.spool_inner(head, src, limit, None)
        }

        fn spool_with_deadline(&self, head: &[u8], src: std::fs::File, limit: u64, deadline: Instant) -> SpoolOutcome {
            self.spool_inner(head, src, limit, Some(deadline))
        }
    }

    /// A pipe fed by a thread, so payloads larger than the pipe buffer work.
    fn fed_pipe(bytes: Vec<u8>) -> (std::fs::File, std::thread::JoinHandle<()>) {
        let (read_file, write_fd) = make_pipe().unwrap();
        let handle = std::thread::spawn(move || {
            let mut writer = std::fs::File::from(write_fd);
            let _ = writer.write_all(&bytes);
        });
        (read_file, handle)
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 249) as u8).collect()
    }

    fn only_job(rx: &std::sync::mpsc::Receiver<ClipboardJob>) -> ClipboardJob {
        let job = rx.try_recv().unwrap();
        assert!(rx.try_recv().is_err(), "exactly one job expected");
        job
    }

    #[test]
    fn text_never_goes_through_the_spooler() {
        let spooler = FakeSpooler::new(Behaviour::Spool);
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let (read_file, feeder) = fed_pipe(pattern(300_000));
        ingest_one(read_file, "text/plain".to_owned(), &tx, &IngestStats::default(), &roomy(), None, Duration::ZERO, Some(&spooler));
        feeder.join().unwrap();
        assert!(spooler.calls().is_empty());
        let job = only_job(&rx);
        assert_eq!(job.payload.as_bytes().map(<[u8]>::len), Some(300_000), "text is normalised in memory");
    }

    #[test]
    fn a_small_non_text_payload_stays_on_the_in_memory_fast_path() {
        let spooler = FakeSpooler::new(Behaviour::Spool);
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let data = pattern(INLINE_PAYLOAD_THRESHOLD_BYTES - 1);
        ingest_one(filled_pipe(&data), "application/octet-stream".to_owned(), &tx, &IngestStats::default(), &roomy(), None, Duration::ZERO, Some(&spooler));
        assert!(spooler.calls().is_empty(), "a payload that ends inside the head is never spooled");
        let job = only_job(&rx);
        assert_eq!(job.payload.as_bytes(), Some(data.as_slice()));
    }

    #[test]
    fn a_payload_that_fills_the_head_is_spooled_whole() {
        let spooler = FakeSpooler::new(Behaviour::Spool);
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let data = pattern(1 << 20);
        let (read_file, feeder) = fed_pipe(data.clone());
        ingest_one(read_file, "application/octet-stream".to_owned(), &tx, &IngestStats::default(), &roomy(), None, Duration::ZERO, Some(&spooler));
        feeder.join().unwrap();

        assert_eq!(spooler.calls(), vec![(INLINE_PAYLOAD_THRESHOLD_BYTES, MAX_PAYLOAD_BYTES)]);
        let job = only_job(&rx);
        assert_eq!(job.mime, "application/octet-stream");
        assert_eq!(job.len_hint(), Some(data.len() as u64));
        assert!(job.payload.as_bytes().is_none(), "the bytes must not be on the heap");
        assert_eq!(job.into_bytes(MAX_PAYLOAD_BYTES).unwrap().1, data, "head and tail must join without a gap");
    }

    #[test]
    fn a_spooled_payload_is_relabelled_when_its_bytes_are_an_image() {
        let spooler = FakeSpooler::new(Behaviour::Spool);
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let mut data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        data.extend(pattern(100_000));
        let (read_file, feeder) = fed_pipe(data);
        ingest_one(read_file, "application/octet-stream".to_owned(), &tx, &IngestStats::default(), &roomy(), None, Duration::ZERO, Some(&spooler));
        feeder.join().unwrap();
        assert_eq!(only_job(&rx).mime, "image/png", "the bytes outrank the announced label");
    }

    #[test]
    fn an_unavailable_spooler_falls_back_to_reading_everything_into_memory() {
        let spooler = FakeSpooler::new(Behaviour::Unavailable);
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let data = pattern(200_000);
        let (read_file, feeder) = fed_pipe(data.clone());
        ingest_one(read_file, "application/octet-stream".to_owned(), &tx, &IngestStats::default(), &roomy(), None, Duration::ZERO, Some(&spooler));
        feeder.join().unwrap();
        assert_eq!(spooler.calls().len(), 1);
        assert_eq!(only_job(&rx).payload.as_bytes(), Some(data.as_slice()), "no byte lost between head and remainder");
    }

    #[test]
    fn a_rejected_stream_produces_no_job_and_frees_the_gate() {
        let spooler = FakeSpooler::new(Behaviour::Reject);
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let (gate, permit) = gate_with_one_permit();
        let stats = IngestStats::default();
        let (read_file, feeder) = fed_pipe(pattern(100_000));
        ingest_one(read_file, "application/octet-stream".to_owned(), &tx, &stats, &roomy(), permit, Duration::ZERO, Some(&spooler));
        let _ = feeder.join();
        assert!(rx.try_recv().is_err());
        assert_eq!(stats.shed_queue(), 0);
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn a_spooled_job_shed_from_a_full_queue_leaves_no_file_behind() {
        let spooler = FakeSpooler::new(Behaviour::Spool);
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        tx.send(ClipboardJob::new("text/plain".to_owned(), vec![1])).unwrap();
        let stats = IngestStats::default();
        let (gate, permit) = gate_with_one_permit();
        let (read_file, feeder) = fed_pipe(pattern(100_000));

        ingest_one(read_file, "application/octet-stream".to_owned(), &tx, &stats, &roomy(), permit, Duration::ZERO, Some(&spooler));
        feeder.join().unwrap();

        assert_eq!(stats.shed_queue(), 1);
        assert_eq!(gate.active(), 0, "the permit is released even though the payload was shed");
        assert_eq!(rx.try_iter().count(), 1, "only the job that was already queued remains");
        assert_eq!(spooler.leftovers(), 0, "the shed job's spool file must have been unlinked");
    }

    #[test]
    fn a_spooled_job_for_a_gone_worker_leaves_no_file_behind() {
        let spooler = FakeSpooler::new(Behaviour::Spool);
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        drop(rx);
        let stats = IngestStats::default();
        let (read_file, feeder) = fed_pipe(pattern(100_000));
        ingest_one(read_file, "application/octet-stream".to_owned(), &tx, &stats, &roomy(), None, Duration::from_secs(10), Some(&spooler));
        feeder.join().unwrap();
        assert_eq!(stats.shed_queue(), 0);
        assert_eq!(spooler.leftovers(), 0);
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
            &roomy(),
            permit,
            Duration::ZERO,
            Duration::from_millis(30),
            None,
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

        ingest_candidates(&candidates, &tx, &stats, &roomy(), None, Duration::ZERO, Duration::from_secs(5), None, |mime| {
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

        ingest_candidates(&candidates, &tx, &stats, &roomy(), None, Duration::ZERO, Duration::from_millis(30), None, |mime| {
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

        ingest_candidates(&candidates, &tx, &IngestStats::default(), &roomy(), None, Duration::ZERO, Duration::from_secs(5), None, |mime| {
            Some(filled_pipe(if mime == "text/uri-list" { b"# only a comment" } else { b"usable text" }))
        });

        assert_eq!(only_job(&rx).payload.as_bytes(), Some(b"usable text".as_slice()), "an empty URI list is no payload; the next candidate is");
    }

    #[test]
    fn no_allocatable_pipe_ends_the_selection_and_counts_a_shed_reader() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = IngestStats::default();
        let candidates = mimes(&["image/png", "text/plain"]);

        ingest_candidates(&candidates, &tx, &stats, &roomy(), None, Duration::ZERO, Duration::from_secs(5), None, |_| None);

        assert_eq!(stats.shed_readers(), 1, "EMFILE is a shed reader, as before");
        assert_eq!(stats.shed_timeout(), 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_selection_no_candidate_can_satisfy_is_not_counted_as_overload() {
        let (tx, rx) = sync_channel::<ClipboardJob>(1);
        let stats = IngestStats::default();
        let candidates = mimes(&["image/png", "text/plain"]);

        ingest_candidates(&candidates, &tx, &stats, &roomy(), None, Duration::ZERO, Duration::from_secs(5), None, |_| Some(filled_pipe(b"")));

        assert!((stats.shed_readers(), stats.shed_timeout(), stats.shed_queue()) == (0, 0, 0), "an unusable selection is not overload");
        assert!(rx.try_recv().is_err());
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
