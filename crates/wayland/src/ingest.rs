// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/wayland/src/ingest.rs

//! Resource bounds of the ingestion path: how many readers may drain
//! compositor pipes at once, and what happens when the database worker cannot
//! keep up.
//!
//! Two independent bounds, applied in this order:
//!
//! 1. [`ReaderGate`] is consulted *before* a pipe is allocated. At most
//!    `INGEST_MAX_CONCURRENT_READERS` readers (thread + pipe + payload buffer)
//!    exist, so a burst of selection changes cannot exhaust file descriptors
//!    (`EMFILE`) or threads. A selection that finds the gate full is shed.
//! 2. [`enqueue`] hands a finished payload to the bounded worker queue. A full
//!    queue makes the reader wait (backpressure: it keeps its gate permit, so
//!    new selections are refused meanwhile) and, once its patience is spent,
//!    sheds the payload instead of growing the heap.
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    fn job(tag: u8) -> ClipboardJob {
        ClipboardJob { mime: "text/plain".to_owned(), data: vec![tag] }
    }

    fn drain(rx: &std::sync::mpsc::Receiver<ClipboardJob>) -> Vec<u8> {
        let mut tags = Vec::new();
        while let Ok(j) = rx.try_recv() {
            tags.push(j.data[0]);
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
                tags.push(j.data[0]);
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
                tags.push(j.data[0]);
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
