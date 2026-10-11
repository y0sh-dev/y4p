// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/image/src/throttle.rs

//! Process throttle for the external image converter.
//!
//! Every conversion spawns a `magick` process group. A burst of copied images
//! (a held-down shortcut, a script injecting into the clipboard) would
//! otherwise fork one per image: CPU pinned at 100%, memory exhausted, the
//! daemon starved. The throttle is a counting semaphore built from `Mutex` +
//! `Condvar` that admits at most `limit` conversions at a time.
//!
//! A caller that finds every slot busy waits for one to free up, but only for
//! a bounded time. Past that, [`ProcessThrottle::acquire`] returns `None` and
//! the caller is expected to keep the original data rather than fail: the
//! bound turns "too much work at once" into "this image is stored unmodified".
//!
//! Like the breaker, the throttle only does bookkeeping. It does not know what
//! a conversion is, and whether saturation counts for anything elsewhere (it
//! must not count as a converter fault) is the caller's decision.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

pub struct ProcessThrottle {
    /// Number of slots currently held.
    in_use: Mutex<usize>,
    freed: Condvar,
    limit: usize,
    wait: Duration,
}

impl ProcessThrottle {
    /// `const` so a process-wide throttle can be a plain `static`. A `limit`
    /// of 0 is treated as 1: a throttle that admits nothing would silently
    /// disable conversion for good.
    pub const fn new(limit: usize, wait: Duration) -> Self {
        Self {
            in_use: Mutex::new(0),
            freed: Condvar::new(),
            limit: if limit == 0 { 1 } else { limit },
            wait,
        }
    }

    /// How long [`acquire`](Self::acquire) waits for a slot before giving up.
    pub fn wait(&self) -> Duration {
        self.wait
    }

    fn lock(&self) -> MutexGuard<'_, usize> {
        // The state is a plain counter that is valid after any panic elsewhere
        // (a `Slot` releases on unwind), so a poisoned lock is recovered
        // instead of propagated: one panicking thread must not jam every
        // later conversion.
        self.in_use.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Slots currently held, for tests and diagnostics.
    pub fn in_use(&self) -> usize {
        *self.lock()
    }

    /// Takes a slot, waiting at most the configured `wait` for one to free up.
    /// `None` means the throttle stayed saturated for the whole budget.
    ///
    /// The slot is held until the returned guard is dropped.
    pub fn acquire(&self) -> Option<Slot<'_>> {
        let guard = self.lock();
        // `wait_timeout_while` re-checks the predicate after every wake-up
        // (spurious ones included) and measures the budget as a whole, so
        // neither can stretch or shorten the wait.
        let (mut guard, _) = self
            .freed
            .wait_timeout_while(guard, self.wait, |held| *held >= self.limit)
            .unwrap_or_else(PoisonError::into_inner);
        // Decide on the counter itself, not on the timeout flag: a slot that
        // freed up in the very instant the budget ran out is still a slot.
        if *guard >= self.limit {
            return None;
        }
        *guard += 1;
        Some(Slot { throttle: self })
    }

    fn release(&self) {
        {
            let mut held = self.lock();
            *held = held.saturating_sub(1);
        }
        // One slot freed, so one waiter can proceed. Notified after the guard
        // is dropped so the woken thread does not immediately block on it.
        self.freed.notify_one();
    }
}

/// One admitted conversion. Dropping it frees the slot, on every exit path
/// including an unwinding panic, so a slot can never leak.
#[must_use = "dropping the slot immediately frees it"]
pub struct Slot<'a> {
    throttle: &'a ProcessThrottle,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.throttle.release();
    }
}

/// Test support: poisons the throttle's lock the way a panicking thread
/// would, by panicking while holding its guard.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) fn poison_for_test(throttle: &ProcessThrottle) {
    let joined = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _guard = throttle.in_use.lock().unwrap();
                panic!("poison the throttle");
            })
            .join()
    });
    assert!(joined.is_err());
    assert!(throttle.in_use.is_poisoned(), "precondition: the lock really is poisoned");
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    const NO_WAIT: Duration = Duration::ZERO;
    const GENEROUS: Duration = Duration::from_secs(30);

    #[test]
    fn admits_exactly_limit_slots_and_then_refuses() {
        let t = ProcessThrottle::new(2, NO_WAIT);
        let a = t.acquire();
        let b = t.acquire();
        assert!(a.is_some() && b.is_some());
        assert_eq!(t.in_use(), 2);
        assert!(t.acquire().is_none(), "third request must be refused when saturated");
        assert_eq!(t.in_use(), 2, "a refused request must not leak a count");
    }

    #[test]
    fn a_limit_of_zero_is_treated_as_one() {
        let t = ProcessThrottle::new(0, NO_WAIT);
        let first = t.acquire();
        assert!(first.is_some(), "a throttle that admits nothing would disable conversion");
        assert!(t.acquire().is_none());
    }

    #[test]
    fn dropping_a_slot_frees_it_for_reuse() {
        let t = ProcessThrottle::new(1, NO_WAIT);
        drop(t.acquire());
        assert_eq!(t.in_use(), 0);
        assert!(t.acquire().is_some());
    }

    #[test]
    fn a_saturated_throttle_waits_the_full_budget_before_giving_up() {
        let budget = Duration::from_millis(120);
        let t = ProcessThrottle::new(1, budget);
        let _held = t.acquire();
        let started = Instant::now();
        assert!(t.acquire().is_none());
        assert!(started.elapsed() >= budget, "gave up after {:?}, before the budget", started.elapsed());
        assert_eq!(t.in_use(), 1);
    }

    #[test]
    fn a_waiter_is_woken_as_soon_as_a_slot_frees_up() {
        let t = ProcessThrottle::new(1, GENEROUS);
        let held = t.acquire();
        let (entered_tx, entered_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                entered_tx.send(()).unwrap();
                let started = Instant::now();
                let slot = t.acquire();
                (slot.is_some(), started.elapsed())
            });
            entered_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
            let (got_it, waited) = waiter.join().unwrap();
            assert!(got_it, "the waiter must receive the freed slot");
            assert!(waited < Duration::from_secs(10), "woken by the release, not by the {GENEROUS:?} timeout");
        });
        assert_eq!(t.in_use(), 0);
    }

    #[test]
    fn a_panicking_holder_still_frees_its_slot() {
        let t = ProcessThrottle::new(1, NO_WAIT);
        let result = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _slot = t.acquire();
                    panic!("converter blew up");
                })
                .join()
        });
        assert!(result.is_err());
        assert_eq!(t.in_use(), 0, "unwinding must release the slot");
        assert!(t.acquire().is_some());
    }

    #[test]
    fn a_poisoned_lock_is_recovered_and_the_count_stays_correct() {
        let t = ProcessThrottle::new(2, NO_WAIT);
        let kept = t.acquire();
        poison_for_test(&t);

        assert_eq!(t.in_use(), 1, "the count survives the poisoning");
        let second = t.acquire();
        assert!(second.is_some(), "acquire must keep working on a poisoned lock");
        assert!(t.acquire().is_none(), "the cap is still enforced");
        drop(second);
        drop(kept);
        assert_eq!(t.in_use(), 0, "release must keep working on a poisoned lock");
    }

    #[test]
    fn a_waiter_survives_the_lock_being_poisoned_while_it_waits() {
        let t = ProcessThrottle::new(1, GENEROUS);
        let held = t.acquire();
        let (entered_tx, entered_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                entered_tx.send(()).unwrap();
                t.acquire().is_some()
            });
            entered_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(50));
            let _ = scope
                .spawn(|| {
                    let _guard = t.in_use.lock().unwrap();
                    panic!("poison while a waiter is parked");
                })
                .join();
            drop(held);
            assert!(waiter.join().unwrap(), "the parked waiter must still get the slot");
        });
        assert_eq!(t.in_use(), 0);
    }

    #[test]
    fn concurrency_never_exceeds_the_limit_under_contention() {
        const LIMIT: usize = 3;
        const THREADS: usize = 24;
        let t = ProcessThrottle::new(LIMIT, GENEROUS);
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let completed = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    let Some(_slot) = t.acquire() else { return };
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    running.fetch_sub(1, Ordering::SeqCst);
                    completed.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= LIMIT, "peak {} exceeded the cap", peak.load(Ordering::SeqCst));
        assert_eq!(completed.load(Ordering::SeqCst), THREADS, "with a generous budget nobody is turned away");
        assert_eq!(t.in_use(), 0);
    }

    #[test]
    fn a_burst_behind_a_held_slot_is_refused_entirely() {
        // One slot held for the whole burst, so every other request must time out.
        let t = ProcessThrottle::new(1, Duration::from_millis(30));
        let _held = t.acquire();
        let admitted = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    if t.acquire().is_some() {
                        admitted.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(admitted.load(Ordering::SeqCst), 0);
        assert_eq!(t.in_use(), 1);
    }
}
