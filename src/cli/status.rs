// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/status.rs

use crate::cli::utils::ArgContext;
use crate::core::constants::*;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

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

    let mut stream = match UnixStream::connect(crate::core::get_socket_path()) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("{}daemon is not running.", LOG_ERROR);
            std::process::exit(1);
        }
    };

    // The daemon closes the connection right after writing its response, so
    // a bounded timeout is enough to avoid hanging on a stuck daemon.
    let timeout = Duration::from_millis(IPC_STATUS_TIMEOUT_MS);
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    if stream.write_all(&[IPC_CMD_STATUS, IPC_DELIMITER]).is_err() {
        eprintln!("{}failed to send status request.", LOG_ERROR);
        std::process::exit(1);
    }
    let _ = stream.flush();

    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() || response.is_empty() {
        eprintln!("{}no response from daemon.", LOG_ERROR);
        std::process::exit(1);
    }

    print!("{}", response);
}
