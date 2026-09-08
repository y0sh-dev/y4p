// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/list.rs

use crate::storage::ClipboardDb;
use super::formatter;
use super::utils::{self, RangeSelection, ArgContext};
use crate::core::constants::*;

type ItemData = (i64, i64, String, i64, Option<String>, bool);
type IndexItem<'a> = (usize, &'a ItemData);

/// Entry point for the 'list' command.
pub fn run(args: &[String], db: &ClipboardDb) {
    let ctx = ArgContext::parse(args);

    // Strict validation: check for unknown flags
    if !ctx.unknown_flags.is_empty() {
        eprintln!("{}unknown option detected: '{}'", LOG_ERROR, ctx.unknown_flags[0]);
        return;
    }

    // Arity enforcement: list accepts at most one positional argument (the range)
    if ctx.positionals.len() > 1 {
        eprintln!("{}command 'list' accepts only one range argument.", LOG_ERROR);
        return;
    }

    // Parse the positional range argument strictly
    let selection = match utils::parse_range(ctx.positionals.first(), 25) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}argument error: {}", LOG_ERROR, e);
            return;
        }
    };
    
    let max_history = crate::core::get_max_history();
    let all_items = db.fetch_metadata(max_history);
    let total_stored = db.get_total_count();
    let len = all_items.len();

    // Bind metadata with their original indices to maintain ID consistency
    let target_items: Vec<IndexItem> = if ctx.full {
        all_items.iter().enumerate().collect()
    } else {
        match selection {
            RangeSelection::Single(n) => {
                if n < len { vec![(n, &all_items[n])] } else { vec![] }
            }
            RangeSelection::Range(start, end) => {
                if len == 0 {
                    vec![]
                } else {
                    let s = start.min(len - 1);
                    let e = end.min(len - 1);
                    if s <= e {
                        // Capture the original absolute index 'i'
                        all_items.iter().enumerate().skip(s).take(e - s + 1).collect()
                    } else {
                        vec![]
                    }
                }
            }
            RangeSelection::Latest(limit) => {
                all_items.iter().enumerate().take(limit).collect()
            }
        }
    };

    if target_items.is_empty() {
        if !ctx.raw { println!("{}no entries found matching the criteria.", LOG_INFO); }
        return;
    }

    render_list("Clipboard History", &target_items, total_stored, ctx.raw, ctx.use_id, max_history);
}

/// Render metadata items in a structured table layout.
/// Items are expected as a pair of (original_index, metadata_reference).
pub fn render_list(
    title: &str,
    items: &[IndexItem],
    total_stored: usize,
    is_raw: bool,
    use_id: bool,
    max_history: usize,
) {
    // G-10: +1 over the label's own width for a fixed one-char pin-indicator
    // slot prepended to the label (see formatter::pin_marker) — reserved for
    // every row, pinned or not, so the table's column alignment never shifts.
    let label_width = 7;
    let total_width = WIDTH_ID + WIDTH_WHEN + WIDTH_SIZE + PREVIEW_WIDTH + label_width + (TABLE_SEP.len() * 3);

    if !is_raw {
        println!("\n--- {} ---", title);
        println!(
            "{:>wid_id$}{sep}{:>wid_when$}{sep}{:>wid_size$}{sep}{}",
            LIST_HEADER_ID,
            LIST_HEADER_WHEN,
            LIST_HEADER_SIZE,
            LIST_HEADER_CONTENT,
            wid_id = WIDTH_ID,
            wid_when = WIDTH_WHEN,
            wid_size = WIDTH_SIZE,
            sep = TABLE_SEP
        );
        println!("{}", TABLE_LINE_CHAR.repeat(total_width));
    }

    for (abs_idx, item) in items {
        let (real_id, ts, mime, size, preview, is_pinned) = *item;
        let label = formatter::get_label(mime);

        // Use absolute history index 'abs_idx' instead of local loop counter
        let id_to_display = if use_id { real_id.to_string() } else { abs_idx.to_string() };

        let raw_preview = if mime.starts_with("image/") {
            format!("[{}] - {} bytes", mime.split('/').nth(1).unwrap_or(""), size)
        } else {
            preview.as_deref().unwrap_or("").to_string()
        };

        let formatted_preview = formatter::preview_content(&raw_preview);

        if is_raw {
            // G-10: pin marker is its own space-delimited field ("*"/"-"),
            // always present, so awk/fzf pipelines get a stable field count
            // regardless of pin state.
            println!(
                "[{:>wid_id$}] {} {} {}",
                id_to_display, formatter::pin_marker_raw(*is_pinned), label, formatted_preview,
                wid_id = WIDTH_ID / 2
            );
        } else {
            // G-10: the one-char pin marker is prepended directly to the
            // label, inside the label_width budget reserved above, so table
            // alignment is identical whether or not the row is pinned.
            println!(
                "[{:>wid_id$}]{sep}{:>wid_when$}{sep}{:>wid_size$} B{sep}{}{} {}",
                id_to_display,
                formatter::format_time(*ts as u64),
                size,
                formatter::pin_marker(*is_pinned),
                label,
                formatted_preview,
                wid_id = WIDTH_ID - 2,
                wid_when = WIDTH_WHEN,
                wid_size = WIDTH_SIZE - 2,
                sep = TABLE_SEP
            );
        }
    }
    
    if !is_raw {
        println!("{}", TABLE_LINE_CHAR.repeat(total_width));
        println!(
            "{}shown {} items | history: {} / {} entries", 
            LOG_INFO, items.len(), total_stored, max_history
        );
    }
}
