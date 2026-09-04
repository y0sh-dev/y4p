// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/daemon.rs

use crate::storage::ClipboardDb;
use crate::daemon;
use crate::core::constants::*;
use crate::cli::utils::ArgContext;

/// Initialize and execute the background monitoring service.
pub fn run(args: &[String], db: ClipboardDb) {
    let ctx = ArgContext::parse(args);

    // Strict validation: 'daemon' permits only --verbose/-v and zero positional arguments
    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force {
        eprintln!("{}command 'daemon' does not support specified options.", LOG_ERROR);
        return;
    }

    // Arity enforcement: ensure no positional arguments are provided
    if !ctx.positionals.is_empty() {
        eprintln!("{}command 'daemon' does not accept positional arguments.", LOG_ERROR);
        // BUGFIX: was "y1-clip", a leftover placeholder inconsistent with
        // the actual binary/package name used everywhere else (README,
        // --version, DB_DIR_NAME, ...).
        println!("usage: y4p daemon [--verbose | -v]");
        return;
    }

    // Notify initialization start. (The "ready" confirmation — printed only
    // once the daemon has actually bound the compositor and its data device
    // — comes from src/daemon/mod.rs; printing it here too was a duplicate.)
    println!("{}{}", LOG_INFO, MSG_DAEMON_START);
    
    if ctx.verbose {
        println!("{}extended event logging is active.", LOG_INFO);
    }

    // Transfer execution to the core monitor logic (src/daemon/mod.rs)
    // This blocks the current thread until the process is interrupted, a
    // fatal error occurs, or startup itself fails.
    let started = daemon::start_daemon(db, ctx.verbose);

    // BUGFIX: previously this message was printed unconditionally, even
    // when `start_daemon` failed before ever binding the socket or
    // reaching the compositor — misleadingly implying a daemon had been
    // running and then stopped, when in fact it never started.
    if started {
        eprintln!("{}{}", LOG_ERROR, MSG_DAEMON_STOP);
    } else {
        eprintln!("{}{}", LOG_ERROR, MSG_DAEMON_START_FAILED);
    }
}
