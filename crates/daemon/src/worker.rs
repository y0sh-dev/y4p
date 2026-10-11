// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/daemon/src/worker.rs

use crate::core::constants::*;
use crate::metrics::DaemonMetrics;
use crate::storage::ClipboardDb;
use crate::wayland::state::ClipboardJob;
use sha3::{Digest, Sha3_256};
use std::sync::{mpsc, Arc};

/// Owns the single writer connection and its background thread. All
/// persistence goes through this one channel, so SQLite only ever sees one
/// writer.
///
/// Image transcoding and fingerprinting happen on this thread too (see
/// `process_job`): the Wayland layer hands over normalised raw payloads and
/// never blocks on `magick`. Jobs are therefore also persisted strictly in
/// arrival order.
pub struct DbWorker {
    tx: mpsc::Sender<ClipboardJob>,
}

impl DbWorker {
    pub fn spawn(mut db: ClipboardDb, metrics: Arc<DaemonMetrics>, verbose: bool, max_history: usize) -> Self {
        let (tx, rx) = mpsc::channel::<ClipboardJob>();

        std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                let (mime, data, hash) = process_job(job.mime, job.data);
                match db.insert_with_hash(&mime, &data, &hash, max_history) {
                    Ok(_) => {
                        metrics.record_ingress();
                        if verbose {
                            println!("{}", log_save(&mime, data.len()));
                        }
                    }
                    Err(e) => eprintln!("{}worker failed to persist data: {}", LOG_ERROR, e),
                }
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

/// Turns a normalised raw payload into what will actually be persisted, in a
/// fixed order: route through the image pipeline, then hash.
///
/// The order matters. Raster images are privacy-stripped and re-encoded by
/// `image::route::route_for_ingest`, which may change both the MIME and every
/// byte; the fingerprint must describe what the database and the cache file
/// really store, so it is computed last, over the final bytes. (Hashing
/// earlier would key the row by bytes that are never saved, and the same
/// image copied twice would still dedupe only by accident.) Routing falls
/// back to the untouched original on any failure, so the hash then covers
/// the original instead — consistent either way.
///
/// Returns `(mime, data, hash)`.
fn process_job(mime: String, data: Vec<u8>) -> (String, Vec<u8>, String) {
    let (mime, data) = crate::image::route::route_for_ingest(mime, data);
    let hash = fingerprint(&data);
    (mime, data, hash)
}

/// Lowercase-hex SHA3-256 fingerprint of `data`.
fn fingerprint(data: &[u8]) -> String {
    let mut hasher = Sha3_256::new();
    hasher.update(data);
    hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>()
}
