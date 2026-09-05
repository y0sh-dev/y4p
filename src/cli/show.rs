// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/show.rs

use crate::storage::ClipboardDb;
use crate::core::constants::*;
use crate::cli::utils::ArgContext;
use std::io::{self, Write};

/// Inspect entry metadata and payload using index or persistent database ID.
pub fn run(args: &[String], db: &ClipboardDb) {
    let ctx = ArgContext::parse(args);

    if !ctx.unknown_flags.is_empty() || ctx.full || ctx.force {
        eprintln!("{}command 'show' does not support specified options.", LOG_ERROR);
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

    let (mime, payload) = match db.get_content_by_id(real_id) {
        Some(res) => res,
        None => {
            eprintln!("{}failed to fetch payload for ID {}.", LOG_ERROR, real_id);
            return;
        }
    };

    if ctx.raw {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(&payload);
        let _ = stdout.flush();
        return;
    }

    println!("--- DETAILS ---");
    println!("DB_ID:    {}", real_id);
    println!("MIME:     {}", mime);
    println!("SIZE:     {} bytes", payload.len());
    println!("---------------");

    if mime.contains("text") || mime.contains("UTF8") {
        println!("{}", String::from_utf8_lossy(&payload));
    } else {
        println!("[binary payload: terminal display suppressed]");
        println!("hint: utilize '--raw' to pipe data to a file.");
    }
    println!("---------------");
}
