// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/cli/src/stats.rs

use crate::storage::{ClipboardDb, MetaRow};
use crate::core::constants::*;
use crate::utils::ArgContext;
use super::formatter::format_bytes;
use super::status;
use std::path::Path;

/// Entry count and logical payload bytes for one MIME category.
#[derive(Debug, Default, PartialEq, Eq)]
struct Category {
    entries: usize,
    bytes: u64,
}

/// What `y4p stats` reports about the stored history, aggregated from the
/// metadata rows alone (no payload is read: a cache-class entry's logical size
/// is the size column).
#[derive(Debug, Default, PartialEq, Eq)]
struct Breakdown {
    entries: usize,
    pinned: usize,
    bytes: u64,
    images: Category,
    text: Category,
    other: Category,
}

/// Folds metadata rows into the per-category totals.
///
/// The categories are checked in this order: `image/*` first (so an SVG is an
/// image, not text), then anything `core::utils::is_text_like_mime` accepts,
/// then everything else — matching how ingress and search classify a row.
fn summarise(rows: &[MetaRow]) -> Breakdown {
    let mut breakdown = Breakdown::default();
    for row in rows {
        let size = u64::try_from(row.3).unwrap_or(0);
        breakdown.entries += 1;
        breakdown.bytes = breakdown.bytes.saturating_add(size);
        if row.5 {
            breakdown.pinned += 1;
        }
        let category = if crate::core::utils::starts_with_ignore_ascii_case(&row.2, "image/") {
            &mut breakdown.images
        } else if crate::core::utils::is_text_like_mime(&row.2) {
            &mut breakdown.text
        } else {
            &mut breakdown.other
        };
        category.entries += 1;
        category.bytes = category.bytes.saturating_add(size);
    }
    breakdown
}

/// File count and total bytes of a directory, or `None` when it cannot be read
/// (absent, or not a directory). Unreadable entries are skipped rather than
/// failing the whole report.
#[derive(Debug, Default, PartialEq, Eq)]
struct DirUsage {
    files: usize,
    bytes: u64,
}

fn dir_usage(dir: &Path) -> Option<DirUsage> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut usage = DirUsage::default();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else { continue };
        if metadata.is_file() {
            usage.files += 1;
            usage.bytes = usage.bytes.saturating_add(metadata.len());
        }
    }
    Some(usage)
}

/// Th size of a regular file, or `None` when it is absent or not one.
fn file_size(path: &Path) -> Option<u64> {
    let metadata = std::fs::metadata(path).ok()?;
    metadata.is_file().then_some(metadata.len())
}

/// The daemon fields `y4p stats` reports, decoded from its single `status` line.
#[derive(Debug, Default, PartialEq, Eq)]
struct DaemonStats {
    uptime_ms: u64,
    paused: bool,
    ingress: u64,
    egress: u64,
    shed_readers: u64,
    shed_queue: u64,
    shed_timeout: u64,
}

// One bit per field, so a line with a missing key is rejected rather than
// silently reported as zero.
const UPTIME: u8 = 1 << 0;
const PAUSED: u8 = 1 << 1;
const INGRESS: u8 = 1 << 2;
const EGRESS: u8 = 1 << 3;
const SHED_READERS: u8 = 1 << 4;
const SHED_QUEUE: u8 = 1 << 5;
const SHED_TIMEOUT: u8 = 1 << 6;
const ALL_FIELDS: u8 = UPTIME | PAUSED | INGRESS | EGRESS | SHED_READERS | SHED_QUEUE | SHED_TIMEOUT;

/// Decodes `key=value` pairs out of the daemon's status line (see
/// `daemon::metrics::DaemonMetrics::format_status`). Unknown keys — `last_event_ms_ago`
/// included — are ignored; `None` means the line did not carry every field this
/// command reports, so nothing is shown half-parsed.
fn parse_status_line(line: &str) -> Option<DaemonStats> {
    let mut stats = DaemonStats::default();
    let mut seen = 0u8;
    for token in line.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else { continue };
        match key {
            "uptime_ms" => {
                stats.uptime_ms = value.parse().ok()?;
                seen |= UPTIME;
            }
            "paused" => {
                stats.paused = value.parse().ok()?;
                seen |= PAUSED;
            }
            "ingress" => {
                stats.ingress = value.parse().ok()?;
                seen |= INGRESS;
            }
            "egress" => {
                stats.egress = value.parse().ok()?;
                seen |= EGRESS;
            }
            "shed_readers" => {
                stats.shed_readers = value.parse().ok()?;
                seen |= SHED_READERS;
            }
            "shed_queue" => {
                stats.shed_queue = value.parse().ok()?;
                seen |= SHED_QUEUE;
            }
            "shed_timeout" => {
                stats.shed_timeout = value.parse().ok()?;
                seen |= SHED_TIMEOUT;
            }
            _ => {}
        }
    }
    (seen == ALL_FIELDS).then_some(stats)
}

/// Report storage volume, on-disk footprint and daemon shed counters.
pub fn run(args: &[String], db: &ClipboardDb) {
    let ctx = ArgContext::parse(args);

    // Strict validation: 'stats' takes no flags and no positional arguments.
    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force || ctx.verbose || ctx.use_id {
        eprintln!("{}command 'stats' does not support specified options.", LOG_ERROR);
        std::process::exit(1);
    }
    if !ctx.positionals.is_empty() {
        eprintln!("{}command 'stats' does not accept positional arguments.", LOG_ERROR);
        std::process::exit(1);
    }

    let breakdown = summarise(&db.fetch_metadata(usize::MAX));
    print_breakdown(&breakdown);
    print_disk_usage();
    print_daemon(status::query_status().ok().as_deref());
}

fn print_breakdown(breakdown: &Breakdown) {
    println!("--- Entries ---");
    println!(
        "total: {} (pinned {}, unpinned {})",
        breakdown.entries,
        breakdown.pinned,
        breakdown.entries.saturating_sub(breakdown.pinned)
    );
    println!("logical bytes: {}", format_bytes(breakdown.bytes));
    for (label, category) in [
        ("image/*", &breakdown.images),
        ("text/*", &breakdown.text),
        ("other", &breakdown.other),
    ] {
        println!("{label}: {} entries, {}", category.entries, format_bytes(category.bytes));
    }
}

fn print_disk_usage() {
    println!("--- On-disk ---");

    let db_path = crate::core::get_db_path();
    match file_size(&db_path) {
        Some(size) => println!("database: {} ({})", format_bytes(size), db_path.display()),
        None => println!("database: absent ({})", db_path.display()),
    }

    // The payload cache lives in `core::get_cache_dir()`
    // (`$XDG_CACHE_HOME/y4p`), not under the database directory.
    let cache_dir = crate::core::get_cache_dir();
    match dir_usage(&cache_dir) {
        Some(usage) => println!("cache: {} files, {} ({})", usage.files, format_bytes(usage.bytes), cache_dir.display()),
        None => println!("cache: absent ({})", cache_dir.display()),
    }

    let spool_dir = crate::core::spool_dir_path();
    match dir_usage(&spool_dir) {
        Some(usage) => println!("spool: {} files, {} ({})", usage.files, format_bytes(usage.bytes), spool_dir.display()),
        None => println!("spool: absent ({})", spool_dir.display()),
    }
}

fn print_daemon(response: Option<&str>) {
    println!("--- Daemon ---");
    match response.and_then(parse_status_line) {
        Some(stats) => {
            println!("uptime_ms: {}", stats.uptime_ms);
            println!("paused: {}", stats.paused);
            println!("ingress: {}", stats.ingress);
            println!("egress: {}", stats.egress);
            println!("shed_readers: {}", stats.shed_readers);
            println!("shed_queue: {}", stats.shed_queue);
            println!("shed_timeout: {}", stats.shed_timeout);
        }
        // A stopped daemon is not an error for `stats`: the storage figures
        // are still complete, so this exits 0 with the shed counts marked out.
        None if response.is_none() => println!("daemon: not running (shed counts unavailable)"),
        None => println!("daemon: running (status line could not be parsed)"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn row(mime: &str, size: i64, pinned: bool) -> MetaRow {
        (1, 0, mime.to_string(), size, None, pinned)
    }

    #[test]
    fn summarise_counts_entries_bytes_and_pins_per_category() {
        let rows = vec![
            row("image/png", 1000, false),
            row("image/svg+xml", 20, true),
            row("text/plain", 300, false),
            row("application/json", 40, false),
            row("text/uri-list", 7, false),
            row("application/octet-stream", 3, false),
        ];

        let breakdown = summarise(&rows);

        assert_eq!((breakdown.entries, breakdown.pinned, breakdown.bytes), (6, 1, 1370));
        assert_eq!(breakdown.images, Category { entries: 2, bytes: 1020 });
        assert_eq!(breakdown.text, Category { entries: 3, bytes: 347 });
        assert_eq!(breakdown.other, Category { entries: 1, bytes: 3 });
    }

    #[test]
    fn summarise_of_an_empty_history_is_all_zero() {
        assert_eq!(summarise(&[]), Breakdown::default());
    }

    #[test]
    fn format_bytes_scales_on_1024_boundaries() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MiB");
    }

    #[test]
    fn parse_status_line_reads_every_field_it_reports() {
        let line = "uptime_ms=1234 ingress=5 egress=2 paused=true last_event_ms_ago=10 shed_readers=1 shed_queue=2 shed_timeout=3\n";
        let stats = parse_status_line(line).unwrap();
        assert_eq!(
            stats,
            DaemonStats { uptime_ms: 1234, paused: true, ingress: 5, egress: 2, shed_readers: 1, shed_queue: 2, shed_timeout: 3 }
        );
    }

    #[test]
    fn parse_status_line_rejects_incomplete_or_malformed_lines() {
        let complete = "uptime_ms=1 ingress=0 egress=0 paused=false shed_readers=0 shed_queue=0 shed_timeout=0\n";
        assert!(parse_status_line(complete).is_some());

        let missing_timeout = complete.replace(" shed_timeout=0", "");
        assert!(parse_status_line(&missing_timeout).is_none(), "a missing counter must not be shown as zero");
        assert!(parse_status_line("uptime_ms=soon ingress=0 egress=0 paused=false shed_readers=0 shed_queue=0 shed_timeout=0").is_none());
        assert!(parse_status_line("").is_none());
    }

    #[test]
    fn dir_usage_reports_files_only_and_ignores_absent_directories() {
        let dir = std::env::temp_dir().join(format!("y4p-stats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.cache"), b"12345").unwrap();
        std::fs::write(dir.join("b.cache"), b"123").unwrap();
        std::fs::create_dir(dir.join("nested")).unwrap();

        assert_eq!(dir_usage(&dir), Some(DirUsage { files: 2, bytes: 8 }), "directories are not payload files");
        assert_eq!(dir_usage(&dir.join("missing")), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
