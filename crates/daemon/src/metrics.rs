// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/daemon/src/metrics.rs

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// Lock-free daemon counters: written from the worker thread, read from the
/// event loop on an IPC `Status` request. No mutex anywhere in this type.
pub struct DaemonMetrics {
    // Immutable after construction, so a plain field is already thread-safe.
    started_at_ms: i64,
    ingress_count: AtomicU64,
    egress_count: AtomicU64,
    last_event_at_ms: AtomicI64,
}

impl Default for DaemonMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonMetrics {
    pub fn new() -> Self {
        Self {
            started_at_ms: now_ms(),
            ingress_count: AtomicU64::new(0),
            egress_count: AtomicU64::new(0),
            last_event_at_ms: AtomicI64::new(0),
        }
    }

    pub fn record_ingress(&self) {
        self.ingress_count.fetch_add(1, Ordering::Relaxed);
        self.last_event_at_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Payloads persisted so far. Allocation-free, so a benchmark can wait on
    /// it without disturbing its own allocation counters.
    pub fn ingress_count(&self) -> u64 {
        self.ingress_count.load(Ordering::Relaxed)
    }

    pub fn record_egress(&self) {
        self.egress_count.fetch_add(1, Ordering::Relaxed);
        self.last_event_at_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Structured status text for IPC `Command::Status` responses. `paused`
    /// and the shed totals come from `WaylandState` (private mode, ingestion
    /// overload), which this type has no access to on its own.
    pub fn format_status(&self, paused: bool, shed_readers: u64, shed_queue: u64) -> String {
        let last = self.last_event_at_ms.load(Ordering::Relaxed);
        format!(
            "uptime_ms={} ingress={} egress={} paused={} last_event_ms_ago={} shed_readers={} shed_queue={}\n",
            now_ms() - self.started_at_ms,
            self.ingress_count.load(Ordering::Relaxed),
            self.egress_count.load(Ordering::Relaxed),
            paused,
            if last == 0 { -1 } else { now_ms() - last },
            shed_readers,
            shed_queue,
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn status_reports_the_shed_totals_as_stable_key_value_pairs() {
        let metrics = DaemonMetrics::new();
        let line = metrics.format_status(true, 3, 7);
        assert!(line.ends_with("shed_readers=3 shed_queue=7\n"), "{line}");
        assert!(line.contains("paused=true"));
        assert_eq!(line.lines().count(), 1, "status stays a single line");
    }

    #[test]
    fn the_ingress_count_follows_the_recorded_payloads() {
        let metrics = DaemonMetrics::new();
        assert_eq!(metrics.ingress_count(), 0);
        metrics.record_ingress();
        metrics.record_ingress();
        assert_eq!(metrics.ingress_count(), 2);
    }

    #[test]
    fn status_keeps_its_original_fields_in_order() {
        let metrics = DaemonMetrics::new();
        metrics.record_ingress();
        metrics.record_egress();
        let line = metrics.format_status(false, 0, 0);
        let keys: Vec<&str> = line.split_whitespace().filter_map(|kv| kv.split('=').next()).collect();
        assert_eq!(keys, ["uptime_ms", "ingress", "egress", "paused", "last_event_ms_ago", "shed_readers", "shed_queue"]);
        assert!(line.contains("ingress=1 egress=1"));
    }
}
