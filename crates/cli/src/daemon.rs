// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/cli/src/daemon.rs

use crate::core::constants::*;
use crate::utils::ArgContext;

/// Validate `daemon` command arguments and print the pre-start banner.
/// Returns the resolved `verbose` flag on success, `None` on any validation
/// failure. Actually starting the daemon (`crate::daemon::start_daemon`) is
/// `main.rs`'s job — this frontend module must not depend on `daemon`.
pub fn run(args: &[String]) -> Option<bool> {
    let ctx = ArgContext::parse(args);

    // Strict validation: 'daemon' permits only --verbose/-v and zero positional arguments
    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force {
        eprintln!("{}command 'daemon' does not support specified options.", LOG_ERROR);
        return None;
    }

    // Arity enforcement: ensure no positional arguments are provided
    if !ctx.positionals.is_empty() {
        eprintln!("{}command 'daemon' does not accept positional arguments.", LOG_ERROR);
        println!("usage: y4p daemon [--verbose | -v]");
        return None;
    }

    // Notify initialisation start. (The "ready" confirmation — printed only
    // once the daemon has actually bound the compositor and its data device
    // — comes from crates/daemon/src/lib.rs; printing it here too was a duplicate.)
    println!("{}{}", LOG_INFO, MSG_DAEMON_START);

    if ctx.verbose {
        println!("{}extended event logging is active.", LOG_INFO);
    }

    Some(ctx.verbose)
}
