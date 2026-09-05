// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/pause.rs

use crate::cli::utils::ArgContext;
use crate::core::constants::*;
use std::io::Write;
use std::os::unix::net::UnixStream;

/// Request the daemon suspend clipboard ingestion (IPC_CMD_PAUSE).
pub fn run(args: &[String]) {
    let ctx = ArgContext::parse(args);

    // Strict validation: 'pause' takes no flags and no positional arguments.
    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force || ctx.verbose || ctx.use_id {
        eprintln!("{}command 'pause' does not support options.", LOG_ERROR);
        std::process::exit(1);
    }
    if !ctx.positionals.is_empty() {
        eprintln!("{}command 'pause' does not accept positional arguments.", LOG_ERROR);
        std::process::exit(1);
    }

    let mut stream = match UnixStream::connect(crate::core::get_socket_path()) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("{}daemon is not running.", LOG_ERROR);
            std::process::exit(1);
        }
    };

    if stream.write_all(&[IPC_CMD_PAUSE, IPC_DELIMITER]).is_err() {
        eprintln!("{}daemon is not running.", LOG_ERROR);
        std::process::exit(1);
    }
    let _ = stream.flush();

    println!("{}{}", LOG_INFO, MSG_MONITOR_PAUSED);
}
