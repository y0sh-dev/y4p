// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/pin.rs

use crate::storage::ClipboardDb;
use crate::core::constants::*;
use crate::cli::utils::ArgContext;

/// Mark a history record as pinned. Pinned records are exempt from
/// `upsert_record`'s automatic rotation eviction (see storage/db.rs), but
/// remain fully subject to explicit `delete` and `wipe --force`.
pub fn run(args: &[String], db: &mut ClipboardDb) {
    set_pin_state(args, db, true, "pin");
}

/// Shared by `pin::run` and `unpin::run`: identical argument validation and
/// target resolution (MRU index by default, immutable ID via `--id`/`-i`,
/// same as `cleaning::delete_run`), differing only in the target pin state
/// and the command name used in messages.
pub(crate) fn set_pin_state(args: &[String], db: &mut ClipboardDb, is_pinned: bool, cmd_name: &str) {
    let ctx = ArgContext::parse(args);

    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force || ctx.verbose {
        eprintln!("{}command '{}' does not support specified options.", LOG_ERROR, cmd_name);
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

    // Resolve real_id: directly from input or via metadata offset (identical
    // to cleaning::delete_run's resolution logic).
    let real_id = if ctx.use_id {
        val
    } else {
        let meta = db.fetch_metadata(crate::core::get_max_history());
        match meta.get(val as usize) {
            Some(&(id, ..)) => id,
            None => {
                eprintln!("{}index [{}] is out of bounds.", LOG_ERROR, val);
                return;
            }
        }
    };

    // Idempotent by construction: re-applying the same pin state is just
    // another successful UPDATE, so re-running pin/unpin on an
    // already-(un)pinned record safely reports success rather than erroring.
    match db.set_pin_by_id(real_id, is_pinned) {
        Ok(true) => {
            let verb = if is_pinned { "pinned" } else { "unpinned" };
            println!("{}{} entry [ID: {}].", LOG_INFO, verb, real_id);
        }
        Ok(false) => eprintln!("{}record with ID {} not found.", LOG_ERROR, real_id),
        Err(e) => eprintln!("{}storage transaction failure: {}", LOG_ERROR, e),
    }
}
