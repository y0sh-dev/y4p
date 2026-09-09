// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/utils.rs

use crate::storage::ClipboardDb;

/// Categories for history range selection.
#[derive(Debug, PartialEq)]
pub enum RangeSelection {
    /// Specific single index.
    Single(usize),
    /// Bound range of indices (start, end).
    Range(usize, usize),
    /// N most recent entries.
    Latest(usize),
}

/// Strict command line argument container.
pub struct ArgContext {
    pub raw: bool,
    pub full: bool,
    pub force: bool,
    pub verbose: bool,
    pub help: bool,
    pub version: bool,
    pub use_id: bool,
    /// Ordered list of non-flag arguments.
    pub positionals: Vec<String>,
    /// List of flags not recognized by the global parser.
    pub unknown_flags: Vec<String>,
}

impl ArgContext {
    /// Dissects raw arguments skipping executable and subcommand identifiers.
    pub fn parse(args: &[String]) -> Self {
        let mut ctx = ArgContext {
            raw: false,
            full: false,
            force: false,
            verbose: false,
            help: false,
            version: false,
            use_id: false,
            positionals: Vec::new(),
            unknown_flags: Vec::new(),
        };

        for arg in args.iter().skip(2) {
            if arg.starts_with("--") {
                match arg.as_str() {
                    "--raw" => ctx.raw = true,
                    "--full" => ctx.full = true,
                    "--force" => ctx.force = true,
                    "--verbose" => ctx.verbose = true,
                    "--help" => ctx.help = true,
                    "--version" => ctx.version = true,
                    "--id" => ctx.use_id = true,
                    _ => ctx.unknown_flags.push(arg.clone()),
                }
            } else if arg.starts_with('-') && arg.len() > 1 {
                for c in arg.chars().skip(1) {
                    match c {
                        'R' => ctx.raw = true,
                        'A' => ctx.full = true,
                        'f' => ctx.force = true,
                        'v' => ctx.verbose = true,
                        'h' => ctx.help = true,
                        'V' => ctx.version = true,
                        'i' => ctx.use_id = true,
                        _ => ctx.unknown_flags.push(format!("-{}", c)),
                    }
                }
            } else {
                ctx.positionals.push(arg.clone());
            }
        }
        ctx
    }
}

/// Validates if a string adheres to the option prefix convention.
pub fn is_option(arg: &str) -> bool {
    arg.starts_with('-')
}

/// Global flag lookup utility for early-stage routing.
pub fn has_flag(args: &[String], long: &str, short: &str) -> bool {
    args.iter().any(|a| a == long || a == short)
}

/// Parses a string into a RangeSelection variant.
pub fn parse_range(arg: Option<&String>, default_limit: usize) -> Result<RangeSelection, String> {
    let s = match arg {
        Some(s) => s.trim(),
        None => return Ok(RangeSelection::Latest(default_limit)),
    };

    if s.contains('-') {
        let parts: Vec<&str> = s.split('-').collect();
        let n1_s = parts.first().unwrap_or(&"").trim();
        let n2_s = parts.get(1).unwrap_or(&"").trim();

        let n1 = if n1_s.is_empty() { None } else { Some(n1_s.parse::<usize>().map_err(|_| format!("invalid index: {}", n1_s))?) };
        let n2 = if n2_s.is_empty() { None } else { Some(n2_s.parse::<usize>().map_err(|_| format!("invalid index: {}", n2_s))?) };

        return match (n1, n2) {
            (Some(start), Some(end)) => Ok(RangeSelection::Range(start.min(end), start.max(end))),
            // BUGFIX: was `start + default_limit`, an unchecked usize
            // addition. `parse::<usize>()` happily accepts values close to
            // usize::MAX (e.g. "y4p list 18446744073709551615-"),
            // so this could overflow: a panic in debug builds, silent
            // wraparound in release. `saturating_add` makes the ceiling
            // "however far a usize can go" instead of undefined/panicking
            // behavior — the subsequent `.min(len - 1)` clamp in
            // `cli/list.rs` already bounds it to something sane in practice.
            (Some(start), None) => Ok(RangeSelection::Range(start, start.saturating_add(default_limit))),
            (None, Some(end)) => Ok(RangeSelection::Range(0, end)),
            _ => Err(format!("invalid range: {}", s)),
        };
    }

    let n = s.parse::<usize>().map_err(|_| format!("invalid ID: {}", s))?;
    Ok(RangeSelection::Single(n))
}

/// Parses a single CLI positional argument into a target's persistent
/// database ID, shared by every subcommand that accepts "an MRU index, or
/// `--id`/`-i` for a stable ID" (`pin`, `unpin`, `show`, `delete`, `copy-to`).
///
/// Parses as `i64` (not `usize`) specifically so a negative value like "-1"
/// is caught and rejected here — every call site used to `as usize` its own
/// parsed value directly, which silently underflows a negative number into
/// something near `usize::MAX` instead of failing.
///
/// `use_id` selects the interpretation of the parsed value: taken directly
/// as the immutable ID when true, otherwise resolved as an offset into the
/// current MRU history via `fetch_metadata`.
pub fn resolve_target_id(input: &str, use_id: bool, db: &ClipboardDb) -> Result<i64, String> {
    let val = input.parse::<i64>().map_err(|_| format!("invalid numerical value: '{}'", input))?;
    if val < 0 {
        return Err(format!("index cannot be negative: {}", val));
    }

    if use_id {
        return Ok(val);
    }

    let meta = db.fetch_metadata(crate::core::get_max_history());
    match meta.get(val as usize) {
        Some(&(id, ..)) => Ok(id),
        None => Err(format!("index [{}] is out of bounds.", val)),
    }
}
