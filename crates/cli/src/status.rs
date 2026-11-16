// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/cli/src/status.rs

use crate::utils::ArgContext;
use crate::core::constants::*;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Why a `status` query did not produce a response.
pub(crate) enum StatusError {
    /// The socket could not be connected to: nothing is listening.
    Unreachable,
    /// Connected, but the request could not be written.
    SendFailed,
    /// The request was sent, but no (or an empty) response came back.
    NoResponse,
}

/// Queries the daemon over IPC (`IPC_CMD_STATUS`), returning its raw response
/// line.
///
/// Shared by `status`, `stats` and `doctor` so all three agree on the
/// protocol, the bounded timeout and what "the daemon is not running" means.
pub(crate) fn query_status() -> Result<String, StatusError> {
    let mut stream = UnixStream::connect(crate::core::get_socket_path()).map_err(|_| StatusError::Unreachable)?;

    // The daemon closes the connection right after writing its response, so
    // a bounded timeout is enough to avoid hanging on a stuck daemon.
    let timeout = Duration::from_millis(IPC_STATUS_TIMEOUT_MS);
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    if stream.write_all(&[IPC_CMD_STATUS, IPC_DELIMITER]).is_err() {
        return Err(StatusError::SendFailed);
    }
    let _ = stream.flush();

    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() || response.is_empty() {
        return Err(StatusError::NoResponse);
    }
    Ok(response)
}

/// Query the running daemon over IPC (`IPC_CMD_STATUS`) and print its response.
pub fn run(args: &[String]) {
    let ctx = ArgContext::parse(args);

    // Strict validation: 'status' takes no flags and no positional arguments.
    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force || ctx.verbose || ctx.use_id {
        eprintln!("{}command 'status' does not support specified options.", LOG_ERROR);
        return;
    }
    if !ctx.positionals.is_empty() {
        eprintln!("{}command 'status' does not accept positional arguments.", LOG_ERROR);
        return;
    }

    match query_status() {
        Ok(response) => print!("{}", response),
        Err(StatusError::Unreachable) => {
            eprintln!("{}daemon is not running.", LOG_ERROR);
            std::process::exit(1);
        }
        Err(StatusError::SendFailed) => {
            eprintln!("{}failed to send status request.", LOG_ERROR);
            std::process::exit(1);
        }
        Err(StatusError::NoResponse) => {
            eprintln!("{}no response from daemon.", LOG_ERROR);
            std::process::exit(1);
        }
    }
}
