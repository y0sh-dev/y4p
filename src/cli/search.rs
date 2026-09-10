// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/search.rs

use crate::storage::ClipboardDb;
use crate::core::constants::*;
use crate::cli::utils::ArgContext;
use std::collections::HashSet;
use super::list::{self, IndexItem};

/// Search through metadata history and render results using strict argument validation.
pub fn run(args: &[String], db: &ClipboardDb) {
    let ctx = ArgContext::parse(args);

    // Strict validation: 'search' only supports --raw/-R and --verbose/-v
    if !ctx.unknown_flags.is_empty() || ctx.full || ctx.force {
        eprintln!("{}command 'search' does not support specified options.", LOG_ERROR);
        return;
    }

    // G-01: multi-keyword AND search. Positionals may span several args
    // and/or a single quoted string — split on whitespace and dedupe so
    // both `search rust wayland` and `search "rust wayland"` behave alike.
    let mut seen = HashSet::new();
    let keywords: Vec<String> = ctx.positionals.iter()
        .flat_map(|s| s.split_whitespace())
        .map(str::to_string)
        .filter(|kw| seen.insert(kw.clone()))
        .collect();

    if keywords.is_empty() {
        eprintln!("{}missing required search keyword.", LOG_ERROR);
        println!("usage: y4p search <keywords...> [--raw | -R] [--id | -i]");
        return;
    }

    // Smart AND search: drop keywords absent from any record up front so a
    // single typo doesn't zero out an otherwise-good query, and tell the
    // user (on stderr, so --raw/--id piping stays clean) which ones it ignored.
    let (valid, invalid) = db.validate_keywords(&keywords);

    if valid.is_empty() {
        println!("{}no entries matching '{}' were found.", LOG_INFO, keywords.join(" "));
        return;
    }

    if !invalid.is_empty() {
        eprintln!("{}ignored non-existent keywords: '{}'", LOG_WARN, invalid.join(" "));
    }

    // Execute metadata-level search via indexed SQLite query. Each hit now
    // carries its ABSOLUTE position in the full MRU history (see the
    // `search_metadata` doc comment in storage/mod.rs) rather than a local
    // "Nth search hit" index, so a displayed index here means the same
    // thing it does in `list`, and can be safely fed into `copy-to`/
    // `delete`/`show` without `--id`.
    let max_history = crate::core::get_max_history();
    let results = db.search_metadata(&valid, max_history);
    let total_stored = db.get_total_count();

    if results.is_empty() {
        println!("{}no entries matching '{}' were found.", LOG_INFO, valid.join(" "));
        return;
    }

    let refs: Vec<IndexItem> = results.iter().map(|(abs_idx, item)| (*abs_idx, item)).collect();

    let title = format!("search: '{}' ({} hits)", valid.join(" AND "), results.len());

    list::render_list(&title, &refs, total_stored, ctx.raw, ctx.use_id, max_history);
}
