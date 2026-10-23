// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/wayland/src/ingest.rs

//! Resource bounds of the ingestion path: how many readers may drain
//! compositor pipes at once, and what happens when the database worker cannot
//! keep up.
//!
//! Three independent bounds, applied in this order:
//!
//! 1. [`ReaderGate`] is consulted *before* a pipe is allocated. At most
//!    `INGEST_MAX_CONCURRENT_READERS` readers (thread + pipe + payload buffer)
//!    exist, so a burst of selection changes cannot exhaust file descriptors
//!    (`EMFILE`) or threads. A selection that finds the gate full is shed.
//! 2. [`enqueue`] hands a finished payload to the bounded worker queue. A full
//!    queue makes the reader wait (backpressure: it keeps its gate permit, so
//!    new selections are refused meanwhile) and, once its patience is spent,
//!    sheds the payload instead of growing the heap.
//! 3. [`enqueue_bounded`] adds a byte ceiling on top of the count: the payload
//!    must also fit [`ByteBudget`], the total of queued, not yet persisted
//!    bytes (heap and spool files alike). The reservation travels with the job
//!    as a [`BytePermit`] and is returned when the job is dropped wherever that
//!    happens: persisted, discarded by the worker, or shed here.
//!
//! Shedding is the deliberate loss of the *newest* copy; it is counted in
//! [`IngestStats`] and surfaced through `y4p status`, never silent.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::core::constants::{INGEST_ENQUEUE_POLL_MS, log_ingest_shed};
use crate::state::ClipboardJob;

/// Counting gate over simultaneous ingestion readers. Lock-free: admission is
/// one compare-and-swap, so it can never block the Wayland event loop.
pub struct ReaderGate {
    active: AtomicUsize,
    limit: usize,
}

impl ReaderGate {
    pub fn new(limit: usize) -> Self {
        Self { active: AtomicUsize::new(0), limit }
    }

    /// Admits one more reader, or `None` when `limit` readers already exist.
    /// The returned permit frees its slot when dropped, on every exit path.
    pub fn try_acquire(self: &Arc<Self>) -> Option<ReaderPermit> {
        let mut current = self.active.load(Ordering::Acquire);
        loop {
            if current >= self.limit {
                return None;
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(ReaderPermit { gate: Arc::clone(self) }),
                Err(actual) => current = actual,
            }
        }
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }
}

/// Proof of admission; releases its slot on drop.
pub struct ReaderPermit {
    gate: Arc<ReaderGate>,
}

impl Drop for ReaderPermit {
    fn drop(&mut self) {
        self.gate.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Lock-free ceiling on the payload bytes that are queued but not yet
/// persisted. Like [`ReaderGate`], admission is one compare-and-swap loop, so a
/// reservation never blocks the thread that asks.
#[derive(Debug)]
pub struct ByteBudget {
    used: AtomicUsize,
    limit: usize,
}

impl ByteBudget {
    pub fn new(limit: usize) -> Self {
        Self { used: AtomicUsize::new(0), limit }
    }

    /// Reserves `bytes`, or `None` when they do not fit next to what is
    /// already reserved. The permit returns them when dropped, on every exit
    /// path. A refusal leaves the counter untouched.
    pub fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<BytePermit> {
        let mut current = self.used.load(Ordering::Acquire);
        loop {
            // `checked_add` keeps a pathological `bytes` from wrapping past the limit.
            let next = current.checked_add(bytes).filter(|&next| next <= self.limit)?;
            match self.used.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(BytePermit { budget: Arc::clone(self), bytes }),
                Err(actual) => current = actual,
            }
        }
    }

    /// Bytes reserved right now.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    /// The whole budget: a payload above it can never be admitted.
    pub fn limit(&self) -> usize {
        self.limit
    }
}

/// Proof that a payload's bytes fit the [`ByteBudget`]; returns them on drop.
#[derive(Debug)]
pub struct BytePermit {
    budget: Arc<ByteBudget>,
    bytes: usize,
}

impl BytePermit {
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for BytePermit {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Lock-free counters of what overload cost, shared with the daemon's status.
#[derive(Default)]
pub struct IngestStats {
    shed_readers: AtomicU64,
    shed_queue: AtomicU64,
}

impl IngestStats {
    /// Counts a selection refused by the full [`ReaderGate`]; returns the new total.
    pub fn record_shed_reader(&self) -> u64 {
        self.shed_readers.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Counts a payload dropped from a saturated queue; returns the new total.
    pub fn record_shed_queue(&self) -> u64 {
        self.shed_queue.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn shed_readers(&self) -> u64 {
        self.shed_readers.load(Ordering::Relaxed)
    }

    pub fn shed_queue(&self) -> u64 {
        self.shed_queue.load(Ordering::Relaxed)
    }
}

/// Warns on the 1st, 2nd, 4th, 8th, ... shed: a sustained overload cannot
/// flood the log, and the first occurrence is never silent.
pub fn report_shed(reason: &str, total: u64) {
    if should_report(total) {
        eprintln!("{}", log_ingest_shed(reason, total));
    }
}

fn should_report(total: u64) -> bool {
    total.is_power_of_two()
}

/// What became of a payload offered to the worker queue.
#[derive(Debug, PartialEq, Eq)]
pub enum Enqueue {
    Queued,
    /// The queue stayed full for the whole patience budget; the payload was dropped.
    Shed,
    /// The worker is gone; nothing will ever consume the payload.
    Closed,
}

/// Offers `job` to the bounded queue, waiting up to `wait` for room.
///
/// `std` has no timed `send`, so this polls `try_send`; a full queue hands the
/// job back untouched (`TrySendError::Full`), so retrying never clones the
/// payload. The caller's thread is the only thing that waits: the Wayland
/// event loop is never involved.
pub fn enqueue(tx: &SyncSender<ClipboardJob>, job: ClipboardJob, wait: Duration) -> Enqueue {
    let started = Instant::now();
    let mut job = job;
    loop {
        match tx.try_send(job) {
            Ok(()) => return Enqueue::Queued,
            Err(TrySendError::Disconnected(_)) => return Enqueue::Closed,
            Err(TrySendError::Full(returned)) => {
                if started.elapsed() >= wait {
                    return Enqueue::Shed;
                }
                job = returned;
                std::thread::sleep(Duration::from_millis(INGEST_ENQUEUE_POLL_MS));
            }
        }
    }
}

/// [`enqueue`] under both bounds: room in the queue *and* in `budget`, within
/// the same patience `wait`.
///
/// A payload larger than the whole budget could never be admitted, so it is
/// shed at once rather than waited on. Otherwise the job's bytes are reserved
/// first (polling, like the queue slot) and the reservation then rides inside
/// the job, so whoever finally drops it, the worker or this function, returns
/// the bytes. A reservation held while the queue is full is released when the
/// patience runs out and the job is shed.
///
/// A job whose size is unknown without I/O (a descriptor or cache-file payload,
/// which ingestion never produces) is accounted as zero bytes.
pub fn enqueue_bounded(tx: &SyncSender<ClipboardJob>, budget: &Arc<ByteBudget>, job: ClipboardJob, wait: Duration) -> Enqueue {
    let bytes = job.accounted_bytes();
    if bytes > budget.limit() {
        return Enqueue::Shed;
    }

    let started = Instant::now();
    let mut job = job;
    loop {
        if job.permit.is_none() {
            job.permit = budget.try_reserve(bytes);
        }
        if job.permit.is_some() {
            match tx.try_send(job) {
                Ok(()) => return Enqueue::Queued,
                Err(TrySendError::Disconnected(_)) => return Enqueue::Closed,
                Err(TrySendError::Full(returned)) => job = returned,
            }
        }
        if started.elapsed() >= wait {
            return Enqueue::Shed;
        }
        std::thread::sleep(Duration::from_millis(INGEST_ENQUEUE_POLL_MS));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    fn job(tag: u8) -> ClipboardJob {
        ClipboardJob::new("text/plain".to_owned(), vec![tag])
    }

    fn drain(rx: &std::sync::mpsc::Receiver<ClipboardJob>) -> Vec<u8> {
        let mut tags = Vec::new();
        while let Ok(j) = rx.try_recv() {
            tags.push(j.payload.as_bytes().unwrap()[0]);
        }
        tags
    }

    // --- ReaderGate ---

    #[test]
    fn gate_admits_exactly_its_limit_then_refuses() {
        let gate = Arc::new(ReaderGate::new(3));
        let permits: Vec<_> = (0..3).map(|_| gate.try_acquire().unwrap()).collect();
        assert_eq!(gate.active(), 3);
        assert!(gate.try_acquire().is_none(), "the fourth reader must be refused");
        assert_eq!(gate.active(), 3, "a refusal must not leak a slot");
        drop(permits);
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn dropping_a_permit_frees_a_slot_for_the_next_reader() {
        let gate = Arc::new(ReaderGate::new(1));
        let first = gate.try_acquire().unwrap();
        assert!(gate.try_acquire().is_none());
        drop(first);
        assert!(gate.try_acquire().is_some());
    }

    #[test]
    fn a_zero_limit_gate_admits_nobody() {
        let gate = Arc::new(ReaderGate::new(0));
        assert!(gate.try_acquire().is_none());
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn concurrent_readers_never_exceed_the_limit() {
        const LIMIT: usize = 4;
        let gate = Arc::new(ReaderGate::new(LIMIT));
        let peak = Arc::new(AtomicUsize::new(0));
        let admitted = Arc::new(AtomicUsize::new(0));

        let threads: Vec<_> = (0..16)
            .map(|_| {
                let (gate, peak, admitted) = (Arc::clone(&gate), Arc::clone(&peak), Arc::clone(&admitted));
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        if let Some(_permit) = gate.try_acquire() {
                            admitted.fetch_add(1, Ordering::SeqCst);
                            peak.fetch_max(gate.active(), Ordering::SeqCst);
                            std::thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= LIMIT, "peak {} exceeded {LIMIT}", peak.load(Ordering::SeqCst));
        assert!(admitted.load(Ordering::SeqCst) > 0);
        assert_eq!(gate.active(), 0, "every permit must have been returned");
    }

    // --- enqueue: backpressure and load shedding ---

    #[test]
    fn a_job_with_room_is_queued_immediately() {
        let (tx, rx) = sync_channel(2);
        assert_eq!(enqueue(&tx, job(1), Duration::ZERO), Enqueue::Queued);
        assert_eq!(drain(&rx), [1]);
    }

    #[test]
    fn a_saturated_queue_sheds_after_the_patience_budget_and_never_grows() {
        let capacity = 4;
        let (tx, rx) = sync_channel(capacity);
        for tag in 0..capacity as u8 {
            assert_eq!(enqueue(&tx, job(tag), Duration::ZERO), Enqueue::Queued);
        }

        let started = Instant::now();
        let wait = Duration::from_millis(40);
        assert_eq!(enqueue(&tx, job(99), wait), Enqueue::Shed);
        assert!(started.elapsed() >= wait, "must have waited for room before shedding");

        // Load shedding drops the newest payload and keeps the queued ones.
        assert_eq!(drain(&rx), [0, 1, 2, 3]);
    }

    #[test]
    fn a_zero_patience_shed_is_immediate() {
        let (tx, _rx) = sync_channel(1);
        assert_eq!(enqueue(&tx, job(1), Duration::ZERO), Enqueue::Queued);
        let started = Instant::now();
        assert_eq!(enqueue(&tx, job(2), Duration::ZERO), Enqueue::Shed);
        assert!(started.elapsed() < Duration::from_millis(INGEST_ENQUEUE_POLL_MS * 10));
    }

    #[test]
    fn backpressure_delivers_when_the_consumer_catches_up_within_the_budget() {
        let (tx, rx) = sync_channel(1);
        assert_eq!(enqueue(&tx, job(1), Duration::ZERO), Enqueue::Queued);

        let consumer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let mut tags = Vec::new();
            while let Ok(j) = rx.recv_timeout(Duration::from_millis(500)) {
                tags.push(j.payload.as_bytes().unwrap()[0]);
                if tags.len() == 2 {
                    break;
                }
            }
            tags
        });

        // The queue is full right now; the producer must wait, not drop.
        assert_eq!(enqueue(&tx, job(2), Duration::from_secs(10)), Enqueue::Queued);
        assert_eq!(consumer.join().unwrap(), [1, 2], "order preserved, nothing lost");
    }

    #[test]
    fn a_producer_burst_through_a_tiny_queue_loses_nothing_while_the_consumer_drains() {
        let (tx, rx) = sync_channel::<ClipboardJob>(2);
        let consumer = std::thread::spawn(move || {
            let mut tags = Vec::new();
            while let Ok(j) = rx.recv() {
                tags.push(j.payload.as_bytes().unwrap()[0]);
                std::thread::sleep(Duration::from_millis(1));
            }
            tags
        });

        let producers: Vec<_> = (0..4u8)
            .map(|p| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for i in 0..10u8 {
                        let outcome = enqueue(&tx, job(p * 10 + i), Duration::from_secs(10));
                        assert_eq!(outcome, Enqueue::Queued);
                    }
                })
            })
            .collect();
        for p in producers {
            p.join().unwrap();
        }
        drop(tx);

        let mut received = consumer.join().unwrap();
        received.sort_unstable();
        assert_eq!(received, (0..40u8).collect::<Vec<_>>(), "all 40 jobs delivered exactly once");
    }

    #[test]
    fn a_gone_worker_reports_closed_instead_of_waiting() {
        let (tx, rx) = sync_channel(1);
        drop(rx);
        let started = Instant::now();
        assert_eq!(enqueue(&tx, job(1), Duration::from_secs(10)), Enqueue::Closed);
        assert!(started.elapsed() < Duration::from_secs(1), "Closed must not consume the patience budget");
    }

    #[test]
    fn a_closed_queue_is_reported_even_when_it_was_full() {
        let (tx, rx) = sync_channel(1);
        assert_eq!(enqueue(&tx, job(1), Duration::ZERO), Enqueue::Queued);
        drop(rx);
        assert_eq!(enqueue(&tx, job(2), Duration::from_secs(10)), Enqueue::Closed);
    }

    // --- ByteBudget / BytePermit ---

    fn big_job(tag: u8, len: usize) -> ClipboardJob {
        ClipboardJob::new("application/octet-stream".to_owned(), vec![tag; len])
    }

    #[test]
    fn the_budget_admits_up_to_its_limit_then_refuses_and_a_refusal_leaks_nothing() {
        let budget = Arc::new(ByteBudget::new(100));
        let a = budget.try_reserve(60).unwrap();
        let b = budget.try_reserve(40).unwrap();
        assert_eq!(budget.used(), 100);
        assert!(budget.try_reserve(1).is_none(), "the budget is exactly full");
        assert_eq!(budget.used(), 100, "a refusal must not move the counter");
        drop(a);
        assert_eq!(budget.used(), 40);
        assert!(budget.try_reserve(60).is_some(), "freed bytes are reusable at once");
        drop(b);
    }

    #[test]
    fn a_permit_returns_exactly_its_bytes_on_every_exit_path() {
        let budget = Arc::new(ByteBudget::new(1000));
        let permit = budget.try_reserve(300).unwrap();
        assert_eq!((permit.bytes(), budget.used()), (300, 300));
        let moved = std::thread::spawn(move || permit.bytes()).join().unwrap();
        assert_eq!(moved, 300);
        assert_eq!(budget.used(), 0, "dropping on another thread returns the bytes too");
    }

    #[test]
    fn a_reservation_near_usize_max_cannot_wrap_past_the_limit() {
        let budget = Arc::new(ByteBudget::new(usize::MAX));
        let held = budget.try_reserve(usize::MAX - 1).unwrap();
        assert!(budget.try_reserve(usize::MAX).is_none(), "the sum overflows and must be refused");
        assert!(budget.try_reserve(2).is_none());
        assert!(budget.try_reserve(1).is_some());
        drop(held);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn a_zero_byte_reservation_is_always_admitted() {
        let budget = Arc::new(ByteBudget::new(0));
        assert!(budget.try_reserve(0).is_some());
        assert!(budget.try_reserve(1).is_none());
    }

    #[test]
    fn concurrent_reservations_never_exceed_the_limit() {
        const LIMIT: usize = 1000;
        let budget = Arc::new(ByteBudget::new(LIMIT));
        let peak = Arc::new(AtomicUsize::new(0));
        let granted = Arc::new(AtomicUsize::new(0));

        let threads: Vec<_> = (0..16)
            .map(|t| {
                let (budget, peak, granted) = (Arc::clone(&budget), Arc::clone(&peak), Arc::clone(&granted));
                std::thread::spawn(move || {
                    for i in 0..200 {
                        if let Some(_permit) = budget.try_reserve(100 + (t * 7 + i) % 150) {
                            granted.fetch_add(1, Ordering::SeqCst);
                            peak.fetch_max(budget.used(), Ordering::SeqCst);
                            std::thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= LIMIT, "peak {} exceeded {LIMIT}", peak.load(Ordering::SeqCst));
        assert!(granted.load(Ordering::SeqCst) > 0);
        assert_eq!(budget.used(), 0, "every permit must have been returned");
    }

    // --- enqueue_bounded: the byte ceiling on top of the count ---

    #[test]
    fn a_job_with_room_in_both_bounds_is_queued_and_carries_its_reservation() {
        let (tx, rx) = sync_channel(2);
        let budget = Arc::new(ByteBudget::new(100));
        assert_eq!(enqueue_bounded(&tx, &budget, big_job(1, 40), Duration::ZERO), Enqueue::Queued);
        assert_eq!(budget.used(), 40, "queued bytes stay reserved");

        let queued = rx.try_recv().unwrap();
        assert_eq!(queued.permit.as_ref().map(BytePermit::bytes), Some(40));
        drop(queued);
        assert_eq!(budget.used(), 0, "dropping the job (the worker's last act) frees the bytes");
    }

    #[test]
    fn a_payload_larger_than_the_whole_budget_is_shed_at_once_without_waiting() {
        let (tx, rx) = sync_channel(2);
        let budget = Arc::new(ByteBudget::new(100));
        let started = Instant::now();
        assert_eq!(enqueue_bounded(&tx, &budget, big_job(1, 101), Duration::from_secs(10)), Enqueue::Shed);
        assert!(started.elapsed() < Duration::from_secs(1), "it can never fit, so waiting is pointless");
        assert!(rx.try_recv().is_err());
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn a_payload_exactly_the_size_of_the_budget_is_admitted() {
        let (tx, rx) = sync_channel(1);
        let budget = Arc::new(ByteBudget::new(100));
        assert_eq!(enqueue_bounded(&tx, &budget, big_job(1, 100), Duration::ZERO), Enqueue::Queued);
        assert_eq!(budget.used(), 100);
        drop(rx);
    }

    #[test]
    fn a_full_byte_budget_sheds_after_the_patience_budget_even_with_queue_slots_free() {
        let (tx, rx) = sync_channel(8);
        let budget = Arc::new(ByteBudget::new(100));
        assert_eq!(enqueue_bounded(&tx, &budget, big_job(1, 80), Duration::ZERO), Enqueue::Queued);

        let started = Instant::now();
        let wait = Duration::from_millis(40);
        assert_eq!(enqueue_bounded(&tx, &budget, big_job(2, 30), wait), Enqueue::Shed);
        assert!(started.elapsed() >= wait, "must have waited for bytes before shedding");

        assert_eq!(budget.used(), 80, "the shed job holds nothing");
        assert_eq!(drain(&rx), [1], "queued work is never displaced");
    }

    #[test]
    fn a_full_queue_sheds_and_returns_the_bytes_it_had_reserved() {
        let (tx, rx) = sync_channel(1);
        let budget = Arc::new(ByteBudget::new(1000));
        assert_eq!(enqueue_bounded(&tx, &budget, big_job(1, 10), Duration::ZERO), Enqueue::Queued);

        let wait = Duration::from_millis(30);
        assert_eq!(enqueue_bounded(&tx, &budget, big_job(2, 500), wait), Enqueue::Shed);
        assert_eq!(budget.used(), 10, "the 500 reserved while waiting for a slot must be released");
        assert_eq!(drain(&rx), [1]);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn backpressure_on_bytes_delivers_when_the_consumer_frees_room_within_the_budget() {
        let (tx, rx) = sync_channel(8);
        let budget = Arc::new(ByteBudget::new(100));
        assert_eq!(enqueue_bounded(&tx, &budget, big_job(1, 80), Duration::ZERO), Enqueue::Queued);

        let consumer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(first); // the worker is done with it: bytes come back
            let second = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            second.payload.as_bytes().unwrap()[0]
        });

        assert_eq!(enqueue_bounded(&tx, &budget, big_job(2, 80), Duration::from_secs(10)), Enqueue::Queued);
        assert_eq!(consumer.join().unwrap(), 2, "order preserved, nothing lost");
    }

    #[test]
    fn a_gone_worker_is_reported_closed_and_leaves_no_reservation() {
        let (tx, rx) = sync_channel(1);
        drop(rx);
        let budget = Arc::new(ByteBudget::new(100));
        let started = Instant::now();
        assert_eq!(enqueue_bounded(&tx, &budget, big_job(1, 10), Duration::from_secs(10)), Enqueue::Closed);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn dropping_the_receiver_with_queued_jobs_returns_their_bytes() {
        let (tx, rx) = sync_channel(4);
        let budget = Arc::new(ByteBudget::new(100));
        for tag in 0..3 {
            assert_eq!(enqueue_bounded(&tx, &budget, big_job(tag, 20), Duration::ZERO), Enqueue::Queued);
        }
        assert_eq!(budget.used(), 60);
        drop(rx);
        assert_eq!(budget.used(), 0, "a worker that exits must not strand the budget");
    }

    #[test]
    fn many_producers_never_hold_more_than_the_budget_and_lose_nothing() {
        const LIMIT: usize = 300;
        let (tx, rx) = sync_channel::<ClipboardJob>(4);
        let budget = Arc::new(ByteBudget::new(LIMIT));
        let peak = Arc::new(AtomicUsize::new(0));

        let consumer = {
            let (budget, peak) = (Arc::clone(&budget), Arc::clone(&peak));
            std::thread::spawn(move || {
                let mut tags = Vec::new();
                while let Ok(job) = rx.recv() {
                    peak.fetch_max(budget.used(), Ordering::SeqCst);
                    tags.push(job.payload.as_bytes().unwrap()[0]);
                    std::thread::sleep(Duration::from_millis(1));
                }
                tags
            })
        };
        let producers: Vec<_> = (0..4u8)
            .map(|p| {
                let (tx, budget) = (tx.clone(), Arc::clone(&budget));
                std::thread::spawn(move || {
                    for i in 0..10u8 {
                        let outcome = enqueue_bounded(&tx, &budget, big_job(p * 10 + i, 100), Duration::from_secs(10));
                        assert_eq!(outcome, Enqueue::Queued);
                    }
                })
            })
            .collect();
        for p in producers {
            p.join().unwrap();
        }
        drop(tx);

        let mut received = consumer.join().unwrap();
        received.sort_unstable();
        assert_eq!(received, (0..40u8).collect::<Vec<_>>(), "all 40 jobs delivered exactly once");
        assert!(peak.load(Ordering::SeqCst) <= LIMIT);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn plain_enqueue_stays_unaccounted() {
        let (tx, rx) = sync_channel(1);
        assert_eq!(enqueue(&tx, big_job(1, 10), Duration::ZERO), Enqueue::Queued);
        assert!(rx.try_recv().unwrap().permit.is_none(), "the original entry point does not touch a budget");
    }

    // --- IngestStats ---

    #[test]
    fn shed_counters_are_independent_and_return_running_totals() {
        let stats = IngestStats::default();
        assert_eq!((stats.record_shed_reader(), stats.record_shed_reader()), (1, 2));
        assert_eq!(stats.record_shed_queue(), 1);
        assert_eq!((stats.shed_readers(), stats.shed_queue()), (2, 1));
    }

    #[test]
    fn the_shed_warning_fires_on_powers_of_two_only() {
        let logged: Vec<u64> = (0..=20u64).filter(|&n| should_report(n)).collect();
        assert_eq!(logged, [1, 2, 4, 8, 16]);
    }
}
