// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/cli/src/store.rs

use crate::storage::ClipboardDb;
use crate::core::constants::*;
use crate::core::payload::PayloadError;
use crate::utils::ArgContext;
use std::io::{self, Read, Write};

/// Ingest data from stdin, persist to database, and synchronise to system clipboard.
pub fn run(args: &[String], db: &mut ClipboardDb) {
    let ctx = ArgContext::parse(args);

    // Strict validation: 'store' only permits --verbose/-v
    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force {
        eprintln!("{}command 'store' does not support specified options.", LOG_ERROR);
        return;
    }

    // Arity enforcement: ensure no more than one positional (MIME) is provided
    if ctx.positionals.len() > 1 {
        eprintln!("{}command 'store' accepts at most one MIME type argument.", LOG_ERROR);
        return;
    }

    // Resolve target MIME from positional arguments or use system default
    let mime = ctx.positionals.first().map(|s| s.as_str()).unwrap_or(DEFAULT_MIME);

    // Read the payload under the same bound as compositor ingestion: one byte
    // beyond the cap, never a truncated buffer.
    let buffer = match read_payload(io::stdin().lock(), MAX_PAYLOAD_BYTES) {
        Ok(buffer) => buffer,
        Err(PayloadError::Empty) => {
            if ctx.verbose {
                println!("{}null payload detected; skipping storage.", LOG_INFO);
            }
            return;
        }
        Err(PayloadError::TooLarge { limit }) => {
            eprintln!("{}payload exceeds the {}-byte limit.", LOG_ERROR, limit);
            return;
        }
        Err(PayloadError::Io(e)) => {
            eprintln!("{}standard input stream read failure: {}", LOG_ERROR, e);
            return;
        }
    };

    // Execute atomic persistence with internal deduplication.
    //
    // BUGFIX: previously `insert_raw` returned `Result<()>`, so the ID of
    // the record to sync back to the daemon was obtained via a *separate*
    // `db.fetch_metadata(1)` call afterwards, assuming "the newest
    // timestamp is the one I just wrote". That's racy against the daemon's
    // own worker thread inserting a fresh clipboard entry in the gap
    // between the two calls, which could make `store` restore the wrong
    // item to the system clipboard. `insert_raw` now returns the ID
    // directly (computed inside the same transaction), so there is no gap.
    match db.insert_raw(mime, &buffer) {
        Ok(real_id) if real_id >= 0 => {
            if ctx.verbose {
                println!("{}", log_save(mime, buffer.len()));
            }

            // IPC: Notify the running daemon to synchronise this new ID
            use std::os::unix::net::UnixStream;
            if let Ok(mut stream) = UnixStream::connect(crate::core::get_socket_path()) {
                let mut payload = vec![IPC_CMD_RESTORE];
                payload.extend_from_slice(real_id.to_string().as_bytes());
                payload.push(IPC_DELIMITER);
                let _ = stream.write_all(&payload);
                let _ = stream.flush();
            }

            if ctx.verbose {
                println!("{}system clipboard synchronized.", LOG_INFO);
            }
        }
        Ok(_) => {
            // Sentinel -1: insert_raw deliberately skipped storage (e.g. a
            // sensitive MIME hint). Nothing was persisted, so there is
            // nothing to synchronise back to the daemon.
            if ctx.verbose {
                println!("{}payload skipped by storage policy; not synchronized.", LOG_INFO);
            }
        }
        Err(e) => {
            eprintln!("{}database transaction failure: {}", LOG_ERROR, e);
        }
    }
}

/// Reads one payload from `reader` under `limit`, with the same
/// never-truncate probe-byte rule as ingestion.
fn read_payload<R: Read>(reader: R, limit: u64) -> Result<Vec<u8>, PayloadError> {
    crate::core::payload::read_bounded(reader, limit, None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_payload_within_the_limit_is_read_whole() {
        assert_eq!(read_payload(&b"hello"[..], 8).unwrap(), b"hello");
        assert_eq!(read_payload(&b"hello"[..], 5).unwrap(), b"hello");
    }

    #[test]
    fn an_empty_stdin_is_reported_as_empty_not_as_an_empty_payload() {
        assert!(matches!(read_payload(&b""[..], 8), Err(PayloadError::Empty)));
    }

    #[test]
    fn an_oversized_stdin_is_rejected_rather_than_truncated() {
        assert!(matches!(read_payload(&b"hello!"[..], 5), Err(PayloadError::TooLarge { limit: 5 })));
    }

    #[test]
    fn a_read_error_is_surfaced() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("boom"))
            }
        }
        assert!(matches!(read_payload(Failing, 8), Err(PayloadError::Io(_))));
    }
}
