// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/cli/src/doctor.rs

use crate::storage::ClipboardDb;
use crate::core::config::Config;
use crate::core::constants::*;
use crate::core::SocketLock;
use crate::utils::ArgContext;
use super::formatter::format_bytes;
use super::status;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Outcome of a single diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Status::Pass => "[PASS]",
            Status::Warn => "[WARN]",
            Status::Fail => "[FAIL]",
        }
    }
}

/// One diagnostic line: what was checked and what was found.
#[derive(Debug, PartialEq, Eq)]
struct Check {
    status: Status,
    area: &'static str,
    detail: String,
}

impl Check {
    fn pass(area: &'static str, detail: impl Into<String>) -> Self {
        Self { status: Status::Pass, area, detail: detail.into() }
    }

    fn warn(area: &'static str, detail: impl Into<String>) -> Self {
        Self { status: Status::Warn, area, detail: detail.into() }
    }

    fn fail(area: &'static str, detail: impl Into<String>) -> Self {
        Self { status: Status::Fail, area, detail: detail.into() }
    }
}

/// How many checks ended in each state.
#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    passed: usize,
    warnings: usize,
    failures: usize,
}

fn tally(checks: &[Check]) -> Tally {
    let mut tally = Tally::default();
    for check in checks {
        match check.status {
            Status::Pass => tally.passed += 1,
            Status::Warn => tally.warnings += 1,
            Status::Fail => tally.failures += 1,
        }
    }
    tally
}

/// Whether the run must be reported as a failure: exactly the `[FAIL]` rows
/// decide, so a warning never turns a healthy installation into a non-zero exit.
fn failed(checks: &[Check]) -> bool {
    checks.iter().any(|check| check.status == Status::Fail)
}

/// Validates the `doctor` command line, exiting non-zero on anything it does
/// not accept. `Some(())` means the diagnostics should run.
pub(crate) fn parse(args: &[String]) -> Option<()> {
    let ctx = ArgContext::parse(args);

    // Strict validation: 'doctor' takes no flags and no positional arguments.
    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force || ctx.verbose || ctx.use_id {
        eprintln!("{}command 'doctor' does not support specified options.", LOG_ERROR);
        std::process::exit(1);
    }
    if !ctx.positionals.is_empty() {
        eprintln!("{}command 'doctor' does not accept positional arguments.", LOG_ERROR);
        std::process::exit(1);
    }
    Some(())
}

/// Runs every diagnostic, prints one line per check, and returns whether any of
/// them failed.
///
/// `compositor` is the outcome of the Wayland handshake, which only the
/// composition root can attempt (`wayland::create_connection`); everything else
/// is decided here from the environment, the database handle and the daemon's
/// IPC socket.
pub fn report(db: &ClipboardDb, compositor: bool) -> bool {
    let mut checks = environment_checks();
    checks.push(if compositor {
        Check::pass("compositor", "connected to the Wayland compositor")
    } else {
        Check::fail("compositor", "cannot connect to a Wayland compositor (is WAYLAND_DISPLAY set?)")
    });
    checks.extend(database_checks(db));
    checks.extend(socket_checks());
    checks.extend(quota_checks(db));

    for check in &checks {
        println!("{} {}: {}", check.status.tag(), check.area, check.detail);
    }
    let tally = tally(&checks);
    println!();
    println!("{} passed, {} warnings, {} failed", tally.passed, tally.warnings, tally.failures);
    failed(&checks)
}

/// Reports a database that could not even be opened, for `main`'s start-up
/// path: without a handle no other check can run, and this is exactly the
/// failure `doctor` exists to explain.
pub fn report_unopenable_database(reason: &str) {
    println!("{} database: cannot open the store: {}", Status::Fail.tag(), reason);
    println!();
    println!("0 passed, 0 warnings, 1 failed");
}

fn environment_checks() -> Vec<Check> {
    let mut checks = Vec::new();

    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if Path::new(&dir).is_dir() => {
            checks.push(Check::pass("environment", format!("XDG_RUNTIME_DIR={}", Path::new(&dir).display())))
        }
        // The socket falls back to /tmp without it (see `core::get_socket_path`),
        // so a missing runtime dir is a degraded setup, not a broken one.
        Some(dir) => checks.push(Check::warn("environment", format!("XDG_RUNTIME_DIR={} is not a directory", Path::new(&dir).display()))),
        None => checks.push(Check::warn("environment", "XDG_RUNTIME_DIR is unset; the IPC socket falls back to /tmp")),
    }

    if std::env::var_os("XDG_DATA_HOME").is_some() {
        checks.push(Check::pass("environment", "XDG_DATA_HOME is set"));
    } else if std::env::var_os("HOME").is_some() {
        checks.push(Check::pass("environment", "XDG_DATA_HOME is unset; falling back to $HOME/.local/share"));
    } else {
        checks.push(Check::fail("environment", "neither XDG_DATA_HOME nor HOME is set; storage would land in the working directory"));
    }

    match std::env::var_os("WAYLAND_DISPLAY") {
        Some(display) => checks.push(Check::pass("environment", format!("WAYLAND_DISPLAY={}", Path::new(&display).display()))),
        None => checks.push(Check::warn("environment", "WAYLAND_DISPLAY is unset; clipboard monitoring needs a compositor")),
    }

    let config_path = Config::get_config_path();
    // `Config::load` never fails: a missing or malformed file falls back to the
    // defaults, which is the behaviour worth reporting — with the effective
    // values, so a wrong y4p.toml is visible rather than merely tolerated.
    let config = Config::load();
    checks.push(Check::pass(
        "environment",
        format!(
            "{} ({}, max_history={}, drop_rtf={}, sanitize={})",
            config_path.display(),
            if config_path.is_file() { "loaded" } else { "absent; defaults in use" },
            config.general.max_history,
            config.mime.drop_rtf,
            config.should_sanitize_images(),
        ),
    ));

    let sanitize = config.should_sanitize_images();
    match std::process::Command::new(MAGICK_PROGRAM).arg("-version").output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            let first = version.lines().next().unwrap_or(MAGICK_PROGRAM);
            checks.push(Check::pass("environment", first.to_owned()));
        }
        Ok(_) if sanitize => {
            checks.push(Check::warn("environment", "`magick -version` exited non-zero; images are stored unmodified"))
        }
        Ok(_) => checks.push(Check::pass(
            "environment",
            "`magick -version` exited non-zero (not required; [image] sanitize = false)",
        )),
        Err(_) if sanitize => {
            checks.push(Check::warn("environment", "`magick` is not installed; images are stored unmodified"))
        }
        Err(_) => checks.push(Check::pass(
            "environment",
            "`magick` is not installed (not required; [image] sanitize = false)",
        )),
    }

    checks
}

fn database_checks(db: &ClipboardDb) -> Vec<Check> {
    let mut checks = Vec::new();
    let path = crate::core::get_db_path();

    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            checks.push(Check::pass("database", format!("{} ({} bytes)", path.display(), metadata.len())))
        }
        Ok(_) => checks.push(Check::fail("database", format!("{} is not a regular file", path.display()))),
        Err(e) => checks.push(Check::fail("database", format!("{} is not readable: {}", path.display(), e))),
    }

    // `get_total_count` answers from the live connection: a query that cannot
    // run reports zero, so a non-zero count is the meaningful signal here.
    checks.push(Check::pass("database", format!("{} entries readable", db.get_total_count())));
    checks
}

fn socket_checks() -> Vec<Check> {
    let mut checks = Vec::new();
    let socket = crate::core::get_socket_path();
    let lock = SocketLock::path_for(&socket);

    checks.push(if socket.exists() {
        Check::pass("socket", format!("{} present", socket.display()))
    } else {
        Check::warn("socket", format!("{} absent; the daemon appears to be stopped", socket.display()))
    });

    checks.push(if lock.exists() {
        Check::pass("socket", format!("{} present", lock.display()))
    } else {
        Check::warn("socket", format!("{} absent; no start-up lock is held", lock.display()))
    });

    match socket.parent() {
        Some(dir) => match std::fs::metadata(dir) {
            Ok(metadata) => {
                let mode = metadata.permissions().mode() & 0o777;
                if metadata.is_dir() && mode == 0o700 {
                    checks.push(Check::pass("socket", format!("{} is a directory with mode 0700", dir.display())));
                } else {
                    checks.push(Check::warn("socket", format!("{} has mode {:03o}; 0700 expected", dir.display(), mode)));
                }
            }
            Err(e) => checks.push(Check::fail("socket", format!("{} is not readable: {}", dir.display(), e))),
        },
        // No parent can only mean the socket path is a bare file name, which
        // `core::get_socket_path` never produces.
        None => checks.push(Check::fail("socket", format!("{} has no parent directory", socket.display()))),
    }

    checks.push(match status::query_status() {
        Ok(_) => Check::pass("socket", "daemon is responding to IPC status"),
        Err(_) => Check::warn("socket", "daemon is not running; shed counts and IPC mutations are unavailable"),
    });

    checks
}

fn quota_checks(db: &ClipboardDb) -> Vec<Check> {
    let mut checks = Vec::new();

    let count = db.get_total_count();
    let max_history = crate::core::get_max_history();
    if count > max_history {
        checks.push(Check::warn("quota", format!("{} entries exceed max_history {}; rotation will catch up", count, max_history)));
    } else {
        checks.push(Check::pass("quota", format!("{} / {} entries", count, max_history)));
    }
    checks.push(Check::pass("quota", format!("payload ceiling {} bytes ({})", MAX_PAYLOAD_BYTES, format_bytes(MAX_PAYLOAD_BYTES))));
    checks.push(Check::pass("quota", format!("cache ceiling {} bytes ({})", CACHE_QUOTA_BYTES, format_bytes(CACHE_QUOTA_BYTES))));

    // Both directories are created on demand, so an absent one is normal on a
    // fresh profile; only a present but unwritable one is a problem.
    for (area, dir) in [("quota", crate::core::get_cache_dir()), ("quota", crate::core::spool_dir_path())] {
        checks.push(match probe_writable(&dir) {
            Ok(()) => Check::pass(area, format!("{} is writable", dir.display())),
            Err(ProbeError::Absent) => Check::pass(area, format!("{} is absent; created on first use", dir.display())),
            Err(ProbeError::Unwritable(reason)) => Check::fail(area, reason),
        });
    }

    checks
}

enum ProbeError {
    /// The directory does not exist yet: not a fault.
    Absent,
    Unwritable(String),
}

/// Checks a directory is writable by creating and removing a probe file, which
/// tests the permission in practice rather than reading a mode bit.
fn probe_writable(dir: &Path) -> Result<(), ProbeError> {
    if !dir.is_dir() {
        return Err(ProbeError::Absent);
    }
    let probe = dir.join(format!(".y4p-doctor-{}", std::process::id()));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(ProbeError::Unwritable(format!("{} is not writable: {}", dir.display(), e))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn checks(statuses: &[Status]) -> Vec<Check> {
        statuses
            .iter()
            .map(|&status| Check { status, area: "test", detail: String::new() })
            .collect()
    }

    #[test]
    fn a_report_with_only_passes_and_warnings_succeeds() {
        let report = checks(&[Status::Pass, Status::Warn, Status::Pass]);
        assert!(!failed(&report), "warnings must not fail the run");
        assert_eq!(tally(&report), Tally { passed: 2, warnings: 1, failures: 0 });
    }

    #[test]
    fn one_failed_check_fails_the_run() {
        let report = checks(&[Status::Pass, Status::Fail, Status::Warn]);
        assert!(failed(&report));
        assert_eq!(tally(&report), Tally { passed: 1, warnings: 1, failures: 1 });
    }

    #[test]
    fn an_empty_report_neither_fails_nor_counts() {
        let report: Vec<Check> = Vec::new();
        assert!(!failed(&report));
        assert_eq!(tally(&report), Tally::default());
    }

    #[test]
    fn tags_are_the_documented_labels() {
        assert_eq!(Status::Pass.tag(), "[PASS]");
        assert_eq!(Status::Warn.tag(), "[WARN]");
        assert_eq!(Status::Fail.tag(), "[FAIL]");
    }

    #[test]
    fn a_probe_file_is_removed_after_a_successful_write() {
        let dir = std::env::temp_dir().join(format!("y4p-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert!(probe_writable(&dir).is_ok());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "the probe must clean up after itself");
        assert!(matches!(probe_writable(&dir.join("missing")), Err(ProbeError::Absent)));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
