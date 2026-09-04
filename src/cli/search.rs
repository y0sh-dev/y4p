// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/search.rs

use crate::storage::ClipboardDb;
use crate::core::constants::*;
use crate::cli::utils::ArgContext;
use super::list;

/// Search through metadata history and render results using strict argument validation.
pub fn run(args: &[String], db: &ClipboardDb) {
    let ctx = ArgContext::parse(args);

    // Strict validation: 'search' only supports --raw/-R and --verbose/-v
    if !ctx.unknown_flags.is_empty() || ctx.full || ctx.force {
        eprintln!("{}command 'search' does not support specified options.", LOG_ERROR);
        return;
    }

    // Arity enforcement: exactly one positional argument (keyword) required
    if ctx.positionals.is_empty() {
        eprintln!("{}missing required search keyword.", LOG_ERROR);
        // BUGFIX: was "y1-clip" — leftover placeholder name.
        println!("usage: y4p search <keyword> [--raw | -R]");
        return;
    }

    if ctx.positionals.len() > 1 {
        eprintln!("{}command 'search' accepts only one keyword.", LOG_ERROR);
        return;
    }

    let query = &ctx.positionals[0];

    // Execute metadata-level search via indexed SQLite query. Each hit now
    // carries its ABSOLUTE position in the full MRU history (see the
    // `search_metadata` doc comment in storage/mod.rs) rather than a local
    // "Nth search hit" index, so a displayed index here means the same
    // thing it does in `list`, and can be safely fed into `copy-to`/
    // `delete`/`show` without `--id`.
    let results = db.search_metadata(query, MAX_HISTORY);
    let total_stored = db.get_total_count();

    if results.is_empty() {
        println!("{}no entries matching '{}' were found.", LOG_INFO, query);
        return;
    }

    let refs: Vec<(usize, &(i64, i64, String, i64, Option<String>))> =
        results.iter().map(|(abs_idx, item)| (*abs_idx, item)).collect();

    let title = format!("search: '{}' ({} hits)", query, results.len());
    
    list::render_list(&title, &refs, total_stored, ctx.raw, ctx.use_id);
}
