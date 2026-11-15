// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/cli/src/cleaning.rs

use crate::storage::ClipboardDb;
use crate::core::constants::*;
use crate::utils::ArgContext;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// What came of asking the running daemon to mutate its store.
enum DaemonReply {
    /// Nothing is listening on the control socket, so the caller may act on
    /// the database directly.
    Unavailable,
    /// The daemon's single-line answer, with its trailing line break removed.
    Line(String),
    /// The exchange began but could not be completed.
    Failed(String),
}

/// Sends `request` (which must not include the delimiter) to the daemon and
/// reads the single line it answers with.
///
/// Only `UnixStream::connect` failing means "no daemon": once connected, the
/// daemon owns the writer connection, so a failure here is reported rather than
/// retried against the database locally, which would race the daemon.
fn request_daemon(request: &[u8]) -> DaemonReply {
    let mut stream = match UnixStream::connect(crate::core::get_socket_path()) {
        Ok(stream) => stream,
        Err(_) => return DaemonReply::Unavailable,
    };

    // `wipe` runs `VACUUM` and drops the cache directory, so the mutation
    // timeout is much longer than the one a `status` reply is given.
    let timeout = Duration::from_millis(IPC_MUTATION_TIMEOUT_MS);
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let exchange = (|| -> std::io::Result<String> {
        stream.write_all(request)?;
        stream.write_all(&[IPC_DELIMITER])?;
        stream.flush()?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        Ok(line.trim_end().to_string())
    })();

    match exchange {
        Ok(line) if !line.is_empty() => DaemonReply::Line(line),
        Ok(_) => DaemonReply::Failed("empty response from the daemon".to_string()),
        // A read or write timeout arrives as `WouldBlock`/`TimedOut`; spelling
        // it out is friendlier than the raw errno.
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
            DaemonReply::Failed(format!("no answer from the daemon within {} ms", IPC_MUTATION_TIMEOUT_MS))
        }
        Err(e) => DaemonReply::Failed(e.to_string()),
    }
}

/// The payload of an `ERR:<message>` reply, if that is what came back.
fn error_message(reply: &str) -> Option<&str> {
    reply.strip_prefix("ERR:")
}

/// Remove a history record by its MRU index or persistent database ID.
///
/// A running daemon owns the writer connection, so the removal is asked of it
/// over IPC; only when no daemon is listening is the database mutated here.
pub fn delete_run(args: &[String], db: &mut ClipboardDb) {
    let ctx = ArgContext::parse(args);

    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force || ctx.verbose {
        eprintln!("{}command 'delete' does not support specified options.", LOG_ERROR);
        return;
    }

    let input_str = match ctx.positionals.first() {
        Some(s) => s,
        None => {
            eprintln!("{}missing required identifier.", LOG_ERROR);
            return;
        }
    };

    let real_id = match crate::utils::resolve_target_id(input_str, ctx.use_id, db) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("{}{}", LOG_ERROR, e);
            return;
        }
    };

    let mut request = vec![IPC_CMD_DELETE];
    request.extend_from_slice(real_id.to_string().as_bytes());

    match request_daemon(&request) {
        DaemonReply::Unavailable => match db.delete_by_id(real_id) {
            Ok(true) => println!("{}removed entry [ID: {}].", LOG_INFO, real_id),
            Ok(false) => eprintln!("{}record with ID {} not found.", LOG_ERROR, real_id),
            Err(e) => eprintln!("{}storage transaction failure: {}", LOG_ERROR, e),
        },
        DaemonReply::Line(reply) => match reply.as_str() {
            "OK:true" => println!("{}removed entry [ID: {}].", LOG_INFO, real_id),
            "OK:false" => eprintln!("{}record with ID {} not found.", LOG_ERROR, real_id),
            other => {
                let message = error_message(other).unwrap_or(other);
                eprintln!("{}storage transaction failure: {}", LOG_ERROR, message);
                std::process::exit(1);
            }
        },
        DaemonReply::Failed(e) => {
            eprintln!("{}failed to reach the daemon: {}", LOG_ERROR, e);
            std::process::exit(1);
        }
    }
}

/// Purge the entire database and optimise file structure.
///
/// As with `delete`, a running daemon performs the wipe on its own writer
/// connection — so the `VACUUM` and the heap release happen in the process that
/// actually holds the pages — and the local store is only touched when no
/// daemon is listening.
pub fn wipe_run(args: &[String], db: &mut ClipboardDb) {
    let ctx = ArgContext::parse(args);

    // Strict validation: 'wipe' accepts ONLY --force/-f and 0 positional arguments.
    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.verbose {
        eprintln!("{}command 'wipe' does not support specified options.", LOG_ERROR);
        return;
    }

    if !ctx.positionals.is_empty() {
        eprintln!("{}command 'wipe' does not accept positional arguments.", LOG_ERROR);
        return;
    }

    // --force is mandatory; wipe never prompts (script-safe).
    if !ctx.force {
        eprintln!("{}refusing to wipe without confirmation.", LOG_ERROR);
        eprintln!("usage: y4p wipe --force");
        std::process::exit(1);
    }

    match request_daemon(&[IPC_CMD_WIPE]) {
        DaemonReply::Unavailable => match db.wipe() {
            Ok(_) => println!("{}storage purged and optimized (VACUUM completed).", LOG_INFO),
            Err(e) => eprintln!("{}database reset failure: {}", LOG_ERROR, e),
        },
        DaemonReply::Line(reply) => match reply.as_str() {
            "OK" => println!("{}storage purged and optimized (VACUUM completed).", LOG_INFO),
            other => {
                let message = error_message(other).unwrap_or(other);
                eprintln!("{}database reset failure: {}", LOG_ERROR, message);
                std::process::exit(1);
            }
        },
        DaemonReply::Failed(e) => {
            eprintln!("{}failed to reach the daemon: {}", LOG_ERROR, e);
            std::process::exit(1);
        }
    }
}
