// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/daemon/metrics.rs

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

    pub fn record_egress(&self) {
        self.egress_count.fetch_add(1, Ordering::Relaxed);
        self.last_event_at_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Structured status text for IPC `Command::Status` responses.
    pub fn format_status(&self) -> String {
        let last = self.last_event_at_ms.load(Ordering::Relaxed);
        format!(
            "uptime_ms={} ingress={} egress={} last_event_ms_ago={}\n",
            now_ms() - self.started_at_ms,
            self.ingress_count.load(Ordering::Relaxed),
            self.egress_count.load(Ordering::Relaxed),
            if last == 0 { -1 } else { now_ms() - last },
        )
    }
}
