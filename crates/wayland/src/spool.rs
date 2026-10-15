// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/wayland/src/spool.rs

//! The seam through which a large payload leaves the compositor pipe without
//! ever becoming a heap buffer.
//!
//! The Wayland layer is a pure sensor: it must not depend on storage, on the
//! image pipeline, or on a hash implementation. Spooling a payload to disk and
//! fingerprinting it as it streams needs all three kinds of knowledge (where
//! the cache lives, what the digest is), so the layer only defines *what* it
//! needs, as [`PayloadSpooler`], and the daemon supplies the implementation
//! (`y4p-daemon`'s `spool` module). Without a spooler (Action Mode, tests) the
//! ingestion path is exactly the in-memory one.

use std::fs::File;

use crate::core::payload::SpooledFile;

/// What became of a pipe offered to a [`PayloadSpooler`].
#[derive(Debug)]
pub enum SpoolOutcome {
    /// The whole stream is on disk, complete and fingerprinted.
    Spooled(SpooledFile),
    /// The stream was drained but is not a payload (empty, over the limit, or
    /// the transfer failed). Nothing is left on disk and nothing is to be kept.
    Rejected,
    /// The spooler could not even start (no spool directory, no descriptor to
    /// spare). The pipe has not been touched beyond the head the caller
    /// already read, so it comes back for an in-memory read.
    Unavailable(File),
}

/// Drains the rest of a compositor pipe into a spool file.
///
/// Implementations are shared by every ingestion thread (hence `Send + Sync`)
/// and must be bounded: at most `limit` bytes in total, `head` included.
pub trait PayloadSpooler: Send + Sync {
    /// `head` is what the caller has already read from `src` (the stream's first
    /// bytes, in order); the implementation writes it first, then copies the
    /// remainder of `src` after it. The spooled file therefore holds the whole
    /// stream, and `limit` bounds the whole stream.
    fn spool(&self, head: &[u8], src: File, limit: u64) -> SpoolOutcome;
}
