// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/image/src/breaker.rs

//! Circuit breaker for the external image converter.
//!
//! A converter that keeps failing (crashing, broken libraries, hanging past
//! its deadline) must not be re-spawned for every clipboard image: each
//! attempt burns CPU and delays the save. After `threshold` consecutive
//! failures the breaker opens and callers skip conversion entirely for the
//! cooldown; afterwards exactly one canary attempt decides whether to resume.
//!
//! ```text
//!            threshold consecutive failures
//!   Closed ───────────────────────────────────▶ Open (cooldown)
//!     ▲                                           │ cooldown elapsed
//!     │ canary succeeds                           ▼
//!     └────────────────────────────────────── HalfOpen ── canary fails ─▶ Open
//! ```
//!
//! The breaker only does bookkeeping; it never decides what counts as a
//! failure (the caller does) and never runs anything itself. All timing
//! methods take the current `Instant` so the state machine is testable
//! without sleeping.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Externally visible state, for logging and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

enum Inner {
    Closed { failures: u32 },
    Open { until: Instant },
    HalfOpen { probe_in_flight: bool },
}

pub struct CircuitBreaker {
    inner: Mutex<Inner>,
    threshold: u32,
    cooldown: Duration,
}

/// What a finished attempt reports back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Success,
    Failure,
    /// Says nothing about the converter's health (tool missing, caller-side
    /// stream error): the attempt is forgotten without touching any count.
    Neutral,
}

impl CircuitBreaker {
    /// `const` so a process-wide breaker can be a plain `static`. A
    /// `threshold` of 0 is treated as 1: a breaker that can never trip
    /// would silently defeat its purpose.
    pub const fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner::Closed { failures: 0 }),
            threshold: if threshold == 0 { 1 } else { threshold },
            cooldown,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // The state is a plain enum that is valid after any panic elsewhere,
        // so a poisoned lock is recovered instead of propagated.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn state(&self) -> BreakerState {
        match *self.lock() {
            Inner::Closed { .. } => BreakerState::Closed,
            Inner::Open { .. } => BreakerState::Open,
            Inner::HalfOpen { .. } => BreakerState::HalfOpen,
        }
    }

    /// Asks permission for one conversion attempt; `None` means "skip it".
    pub fn acquire(&self) -> Option<Permit<'_>> {
        self.acquire_at(Instant::now())
    }

    pub fn acquire_at(&self, now: Instant) -> Option<Permit<'_>> {
        let mut inner = self.lock();
        match *inner {
            Inner::Closed { .. } => Some(Permit::new(self, false)),
            Inner::Open { until } if now >= until => {
                *inner = Inner::HalfOpen { probe_in_flight: true };
                Some(Permit::new(self, true))
            }
            Inner::Open { .. } => None,
            Inner::HalfOpen { probe_in_flight: false } => {
                *inner = Inner::HalfOpen { probe_in_flight: true };
                Some(Permit::new(self, true))
            }
            // Single canary: while one probe is out, everyone else skips.
            Inner::HalfOpen { probe_in_flight: true } => None,
        }
    }

    /// Applies a finished attempt. Returns `(tripped, recovered)`.
    fn resolve(&self, canary: bool, outcome: Outcome, now: Instant) -> (bool, bool) {
        let mut inner = self.lock();
        match (&*inner, canary) {
            (Inner::Closed { failures }, false) => {
                let failures = *failures;
                match outcome {
                    Outcome::Success => *inner = Inner::Closed { failures: 0 },
                    Outcome::Neutral => {}
                    Outcome::Failure if failures.saturating_add(1) >= self.threshold => {
                        *inner = Inner::Open { until: now + self.cooldown };
                        return (true, false);
                    }
                    Outcome::Failure => *inner = Inner::Closed { failures: failures + 1 },
                }
            }
            (Inner::HalfOpen { probe_in_flight: true }, true) => match outcome {
                Outcome::Success => {
                    *inner = Inner::Closed { failures: 0 };
                    return (false, true);
                }
                Outcome::Failure => {
                    *inner = Inner::Open { until: now + self.cooldown };
                    return (true, false);
                }
                // Free the probe slot; no new cooldown, the next caller probes.
                Outcome::Neutral => *inner = Inner::HalfOpen { probe_in_flight: false },
            },
            // A late report from an attempt that started before the state
            // moved on (e.g. a regular attempt finishing after the breaker
            // tripped) describes a world that no longer exists: ignore it.
            _ => {}
        }
        (false, false)
    }
}

/// Explicit outcome reported when resolving a [`Permit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermitOutcome {
    /// Conversion succeeded and verified valid.
    Success,
    /// Conversion failed due to a genuine converter fault.
    Failure,
    /// Non-converter error or cancelled attempt (leaves counters neutral).
    Release,
}

/// One admitted conversion attempt. It must be resolved with
/// [`Permit::resolve`], [`Permit::success`], [`Permit::failure`] or [`Permit::release`].
///
/// Dropping it unresolved (e.g. an early return, error exit, or panic)
/// automatically records an [`Outcome::Failure`] as a safety net, guaranteeing
/// that an abandoned canary probe can never wedge the breaker in `HalfOpen`.
#[must_use = "an unresolved permit is recorded as a failure"]
pub struct Permit<'a> {
    breaker: &'a CircuitBreaker,
    canary: bool,
    resolved: bool,
}

impl<'a> Permit<'a> {
    fn new(breaker: &'a CircuitBreaker, canary: bool) -> Self {
        Self { breaker, canary, resolved: false }
    }

    /// True for the single half-open probe.
    pub fn is_canary(&self) -> bool {
        self.canary
    }

    /// Resolves the permit with an explicit [`PermitOutcome`].
    /// Returns `(tripped, recovered)`.
    pub fn resolve(self, outcome: PermitOutcome) -> (bool, bool) {
        self.resolve_at(outcome, Instant::now())
    }

    /// Resolves the permit at a specific `Instant` (useful for deterministic tests).
    pub fn resolve_at(self, outcome: PermitOutcome, now: Instant) -> (bool, bool) {
        let internal = match outcome {
            PermitOutcome::Success => Outcome::Success,
            PermitOutcome::Failure => Outcome::Failure,
            PermitOutcome::Release => Outcome::Neutral,
        };
        self.finish(internal, now)
    }

    /// Reports a healthy attempt. Returns true if it closed an open circuit.
    pub fn success(self) -> bool {
        self.finish(Outcome::Success, Instant::now()).1
    }

    /// Reports a failed attempt. Returns true if it (re)opened the circuit.
    pub fn failure(self) -> bool {
        self.failure_at(Instant::now())
    }

    pub fn failure_at(self, now: Instant) -> bool {
        self.finish(Outcome::Failure, now).0
    }

    /// Gives the permit back without judging the converter.
    pub fn release(self) {
        let _ = self.finish(Outcome::Neutral, Instant::now());
    }

    fn finish(mut self, outcome: Outcome, now: Instant) -> (bool, bool) {
        self.resolved = true;
        self.breaker.resolve(self.canary, outcome, now)
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if !self.resolved {
            let _ = self.breaker.resolve(self.canary, Outcome::Failure, Instant::now());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    const COOLDOWN: Duration = Duration::from_secs(30);

    fn breaker() -> CircuitBreaker {
        CircuitBreaker::new(3, COOLDOWN)
    }

    /// Fails `n` regular attempts at `now`; returns whether the last one tripped.
    fn fail_n(b: &CircuitBreaker, n: u32, now: Instant) -> bool {
        let mut tripped = false;
        for _ in 0..n {
            tripped = b.acquire_at(now).unwrap().failure_at(now);
        }
        tripped
    }

    #[test]
    fn starts_closed_and_admits_regular_attempts() {
        let b = breaker();
        assert_eq!(b.state(), BreakerState::Closed);
        let permit = b.acquire_at(Instant::now()).unwrap();
        assert!(!permit.is_canary());
        permit.release();
    }

    #[test]
    fn trips_open_exactly_at_the_threshold() {
        let b = breaker();
        let t0 = Instant::now();
        assert!(!fail_n(&b, 2, t0));
        assert_eq!(b.state(), BreakerState::Closed);
        assert!(fail_n(&b, 1, t0), "the third consecutive failure must report tripping");
        assert_eq!(b.state(), BreakerState::Open);
    }

    #[test]
    fn open_rejects_everything_until_the_cooldown_has_elapsed() {
        let b = breaker();
        let t0 = Instant::now();
        fail_n(&b, 3, t0);
        assert!(b.acquire_at(t0).is_none());
        assert!(b.acquire_at(t0 + COOLDOWN - Duration::from_nanos(1)).is_none());
        assert_eq!(b.state(), BreakerState::Open);
    }

    #[test]
    fn after_the_cooldown_exactly_one_canary_is_admitted() {
        let b = breaker();
        let t0 = Instant::now();
        fail_n(&b, 3, t0);
        let later = t0 + COOLDOWN;
        let canary = b.acquire_at(later).unwrap();
        assert!(canary.is_canary());
        assert_eq!(b.state(), BreakerState::HalfOpen);
        assert!(b.acquire_at(later).is_none(), "no second probe while one is in flight");
        assert!(b.acquire_at(later + COOLDOWN).is_none(), "still none, however long the probe takes");
        canary.release();
    }

    #[test]
    fn canary_success_closes_the_circuit_and_resets_the_failure_count() {
        let b = breaker();
        let t0 = Instant::now();
        fail_n(&b, 3, t0);
        let later = t0 + COOLDOWN;
        assert!(b.acquire_at(later).unwrap().success(), "recovery must be reported");
        assert_eq!(b.state(), BreakerState::Closed);
        // A full threshold of fresh failures is needed again.
        assert!(!fail_n(&b, 2, later));
        assert_eq!(b.state(), BreakerState::Closed);
        assert!(fail_n(&b, 1, later));
    }

    #[test]
    fn canary_failure_reopens_for_a_fresh_full_cooldown() {
        let b = breaker();
        let t0 = Instant::now();
        fail_n(&b, 3, t0);
        let t1 = t0 + COOLDOWN;
        assert!(b.acquire_at(t1).unwrap().failure_at(t1), "a failed canary re-trips the circuit");
        assert_eq!(b.state(), BreakerState::Open);
        assert!(b.acquire_at(t1 + COOLDOWN - Duration::from_nanos(1)).is_none());
        assert!(b.acquire_at(t1 + COOLDOWN).unwrap().is_canary());
    }

    #[test]
    fn a_success_between_failures_resets_the_consecutive_count() {
        let b = breaker();
        let t0 = Instant::now();
        fail_n(&b, 2, t0);
        assert!(!b.acquire_at(t0).unwrap().success());
        fail_n(&b, 2, t0);
        assert_eq!(b.state(), BreakerState::Closed, "failures 1,2 + success + failures 1,2 are not 3 in a row");
    }

    #[test]
    fn neutral_releases_never_change_the_failure_count() {
        let b = breaker();
        let t0 = Instant::now();
        fail_n(&b, 2, t0);
        for _ in 0..10 {
            b.acquire_at(t0).unwrap().release();
        }
        assert_eq!(b.state(), BreakerState::Closed);
        assert!(fail_n(&b, 1, t0), "neutral attempts neither reset nor advanced the count");
    }

    #[test]
    fn releasing_the_canary_frees_the_probe_slot_without_a_new_cooldown() {
        let b = breaker();
        let t0 = Instant::now();
        fail_n(&b, 3, t0);
        let later = t0 + COOLDOWN;
        b.acquire_at(later).unwrap().release();
        assert_eq!(b.state(), BreakerState::HalfOpen);
        assert!(b.acquire_at(later).unwrap().is_canary(), "the very next caller becomes the probe");
    }

    #[test]
    fn an_unresolved_permit_counts_as_a_failure() {
        let b = CircuitBreaker::new(1, COOLDOWN);
        drop(b.acquire_at(Instant::now()).unwrap());
        assert_eq!(b.state(), BreakerState::Open);
    }

    #[test]
    fn an_abandoned_canary_cannot_wedge_the_breaker_half_open() {
        let b = breaker();
        let t0 = Instant::now();
        fail_n(&b, 3, t0);
        drop(b.acquire_at(t0 + COOLDOWN).unwrap());
        assert_eq!(b.state(), BreakerState::Open, "dropping the canary re-trips, it does not hang");
    }

    #[test]
    fn a_late_report_from_before_the_trip_is_ignored() {
        let b = breaker();
        let t0 = Instant::now();
        let stale = b.acquire_at(t0).unwrap(); // started while Closed
        fail_n(&b, 3, t0);
        assert_eq!(b.state(), BreakerState::Open);
        assert!(!stale.success(), "a stale success must not close an open circuit");
        assert_eq!(b.state(), BreakerState::Open);
    }

    #[test]
    fn zero_threshold_is_treated_as_one() {
        let b = CircuitBreaker::new(0, COOLDOWN);
        assert!(b.acquire_at(Instant::now()).unwrap().failure());
        assert_eq!(b.state(), BreakerState::Open);
    }

    #[test]
    fn concurrent_callers_get_exactly_one_canary() {
        let b = CircuitBreaker::new(1, Duration::ZERO);
        b.acquire().unwrap().failure();
        assert_eq!(b.state(), BreakerState::Open);

        let threads = 16;
        let barrier = Barrier::new(threads);
        let admitted = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        // Keep the permit alive until every thread has tried.
                        let permit = b.acquire();
                        barrier.wait();
                        let got = permit.is_some();
                        if let Some(p) = permit {
                            p.release();
                        }
                        got
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).filter(|&got| got).count()
        });
        assert_eq!(admitted, 1);
    }

    #[test]
    fn permit_resolve_dispatches_outcomes() {
        let b = breaker();
        let (tripped, recovered) = b.acquire().unwrap().resolve(PermitOutcome::Success);
        assert!(!tripped);
        assert!(!recovered);
        assert_eq!(b.state(), BreakerState::Closed);

        // Fail threshold (3) times via resolve(PermitOutcome::Failure)
        assert!(!b.acquire().unwrap().resolve(PermitOutcome::Failure).0);
        assert!(!b.acquire().unwrap().resolve(PermitOutcome::Failure).0);
        let (tripped, _) = b.acquire().unwrap().resolve(PermitOutcome::Failure);
        assert!(tripped);
        assert_eq!(b.state(), BreakerState::Open);
    }
}
