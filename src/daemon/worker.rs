// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/daemon/worker.rs

use crate::core::constants::*;
use crate::daemon::metrics::DaemonMetrics;
use crate::storage::ClipboardDb;
use crate::wayland::state::ClipboardJob;
use std::sync::{mpsc, Arc};

/// Owns the single writer connection and its background thread. All
/// persistence goes through this one channel, so SQLite only ever sees one
/// writer.
pub struct DbWorker {
    tx: mpsc::Sender<ClipboardJob>,
}

impl DbWorker {
    pub fn spawn(mut db: ClipboardDb, metrics: Arc<DaemonMetrics>, verbose: bool, max_history: usize) -> Self {
        let (tx, rx) = mpsc::channel::<ClipboardJob>();

        std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                match db.insert_with_hash(&job.mime, &job.data, &job.hash, max_history) {
                    Ok(_) => {
                        metrics.record_ingress();
                        if verbose { println!("{}", log_save(&job.mime, job.data.len())); }
                    }
                    Err(e) => eprintln!("{}worker failed to persist data: {}", LOG_ERROR, e),
                }
                // Return freed heap to the OS after each large payload.
                // SAFETY: `malloc_trim(0)` only requests the allocator
                // release free pages back to the OS; it doesn't touch any
                // live allocation this thread holds.
                #[cfg(target_os = "linux")]
                unsafe { libc::malloc_trim(0); }
            }
        });

        Self { tx }
    }

    /// Clone of the submission channel, handed to the ingestion path so jobs
    /// can be queued without touching the worker thread directly.
    pub fn sender(&self) -> mpsc::Sender<ClipboardJob> {
        self.tx.clone()
    }
}
