// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/paste_from.rs

use crate::core::constants::*;
use crate::cli::utils::ArgContext;

/// Validate `paste-from` arguments and resolve the target MIME type.
/// Returns `None` on validation failure. The actual Wayland fetch and
/// stdout write happen in `main.rs` — this frontend module must not depend
/// on `wayland`.
pub fn run(args: &[String]) -> Option<String> {
    let ctx = ArgContext::parse(args);

    // Strict validation: reject all flags as paste-from is a direct stream operation
    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force || ctx.verbose {
        eprintln!("{}command 'paste-from' does not support options.", LOG_ERROR);
        return None;
    }

    // Arity enforcement: ensure no more than one positional (MIME) is provided
    if ctx.positionals.len() > 1 {
        eprintln!("{}command 'paste-from' accepts at most one MIME type argument.", LOG_ERROR);
        return None;
    }

    // Resolve target MIME from the first positional argument or use system default
    let mime = ctx.positionals.first().map(|s| s.as_str()).unwrap_or(DEFAULT_MIME);

    Some(mime.to_string())
}
