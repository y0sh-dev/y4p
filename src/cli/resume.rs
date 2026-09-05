// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/resume.rs

use crate::cli::utils::ArgContext;
use crate::core::constants::*;
use std::io::Write;
use std::os::unix::net::UnixStream;

/// Request the daemon resume clipboard ingestion (IPC_CMD_RESUME).
pub fn run(args: &[String]) {
    let ctx = ArgContext::parse(args);

    // Strict validation: 'resume' takes no flags and no positional arguments.
    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force || ctx.verbose || ctx.use_id {
        eprintln!("{}command 'resume' does not support options.", LOG_ERROR);
        std::process::exit(1);
    }
    if !ctx.positionals.is_empty() {
        eprintln!("{}command 'resume' does not accept positional arguments.", LOG_ERROR);
        std::process::exit(1);
    }

    let mut stream = match UnixStream::connect(crate::core::get_socket_path()) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("{}daemon is not running.", LOG_ERROR);
            std::process::exit(1);
        }
    };

    if stream.write_all(&[IPC_CMD_RESUME, IPC_DELIMITER]).is_err() {
        eprintln!("{}daemon is not running.", LOG_ERROR);
        std::process::exit(1);
    }
    let _ = stream.flush();

    println!("{}{}", LOG_INFO, MSG_MONITOR_RESUMED);
}
