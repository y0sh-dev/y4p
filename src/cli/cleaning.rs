// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/cleaning.rs

use crate::storage::ClipboardDb;
use crate::core::constants::*;
use crate::cli::utils::ArgContext;

/// Remove a history record by its MRU index or persistent database ID.
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

    let val = match input_str.parse::<i64>() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("{}invalid numerical value: '{}'", LOG_ERROR, input_str);
            return;
        }
    };

    // Resolve real_id: directly from input or via metadata offset
    let real_id = if ctx.use_id {
        val
    } else {
        let meta = db.fetch_metadata(MAX_HISTORY);
        match meta.get(val as usize) {
            Some(&(id, ..)) => id,
            None => {
                eprintln!("{}index [{}] is out of bounds.", LOG_ERROR, val);
                return;
            }
        }
    };

    match db.delete_by_id(real_id) {
        Ok(true) => println!("{}removed entry [ID: {}].", LOG_INFO, real_id),
        Ok(false) => eprintln!("{}record with ID {} not found.", LOG_ERROR, real_id),
        Err(e) => eprintln!("{}storage transaction failure: {}", LOG_ERROR, e),
    }
}

/// Purge the entire database and optimize file structure.
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

    // Non-interactive by design: 'wipe' never prompts for confirmation, so it
    // is safe to call from scripts/pipelines without stdin attached.
    // --force/-f is mandatory instead — it is the only way to authorize this
    // irreversible operation.
    if !ctx.force {
        eprintln!("{}refusing to wipe without confirmation.", LOG_ERROR);
        eprintln!("usage: y4-clipboard wipe --force");
        std::process::exit(1);
    }

    match db.wipe() {
        Ok(_) => println!("{}storage purged and optimized (VACUUM completed).", LOG_INFO),
        Err(e) => eprintln!("{}database reset failure: {}", LOG_ERROR, e),
    }
}
