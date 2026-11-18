/*
 * y4p: A Wayland clipboard manager for power users.
 * Copyright (C) 2026  yosana
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

// src/main.rs

// The domain core lives in the `y4p-core` workspace crate. Aliasing it to
// `core` keeps every `crate::core::...` path in the sibling modules resolving
// unchanged. Domain crates are aliased for the same reason.
use y4p_core as core;
use y4p_storage as storage;
use y4p_wayland as wayland;
use y4p_cli as cli;

use y4p_daemon as daemon;

use std::io::Write;
use cli::CliAction;
use crate::core::constants::*;

fn main() {
    // Ignore SIGPIPE: a reader disappearing mid-write (e.g. a Wayland client
    // closing the destination fd of an offer.receive()) must surface as an
    // EPIPE on the write, never as process termination.
    // SAFETY: `signal` is called with a valid signal number and one of the
    // two `SIG_*` constants, per its documented contract.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN); }

    let args: Vec<String> = std::env::args().collect();
    let is_daemon_invocation = args.get(1).map(String::as_str) == Some("daemon");

    // BUGFIX (critical): the previous handler unconditionally connected to the
    // daemon's IPC socket and sent IPC_CMD_EXIT on *every* Ctrl+C, regardless
    // of which subcommand was running. Interrupting an unrelated one-shot
    // command such as `list`, `search`, or `paste-from` therefore killed the
    // persistent background daemon as a side effect.
    //
    // The daemon's own event loop already re-checks `is_exiting()` every
    // 500ms (bounded by its poll() timeout), so no socket round-trip is
    // needed to wake it up — setting the flag is sufficient. Non-daemon
    // invocations must simply exit themselves and must never touch the
    // daemon's socket.
    // Startup precondition, not resident event-loop code (the Non-fatal
    // Isolation Scope covers core/storage/wayland/daemon, not this
    // entry point): a process that can't install its own signal handler
    // has nothing safe left to fall back to, so failing fast here is correct.
    // The failure is reported and turned into a plain exit code 1 rather than
    // a panic, keeping the non-test crate free of `expect()`/`unwrap()`.
    if let Err(e) = ctrlc::set_handler(move || {
        if crate::core::is_exiting() {
            // Second Ctrl+C: force-exit immediately regardless of role.
            std::process::exit(1);
        }
        crate::core::request_exit();

        if !is_daemon_invocation {
            // Standard SIGINT exit convention (128 + SIGINT(2)); the daemon
            // process, if any, is left completely untouched.
            std::process::exit(130);
        }
    }) {
        eprintln!("{}failed to set signal handler: {}", LOG_ERROR, e);
        std::process::exit(1);
    }

    // Robustness: handle database open errors without panicking.
    let db = match storage::ClipboardDb::open() {
        Ok(database) => database,
        Err(e) => {
            // `doctor` exists to explain exactly this kind of failure, so it
            // reports the check rather than bailing out with a bare fatal error.
            if args.get(1).map(String::as_str) == Some("doctor") {
                cli::doctor::report_unopenable_database(&e);
                std::process::exit(1);
            }
            eprintln!("{}critical: {}", LOG_ERROR, e);
            std::process::exit(1);
        }
    };

    // `cli` only parses/validates "daemon" and "paste-from"; as the sole
    // composition root allowed to depend on every module, `main` performs
    // their actual backend calls.
    match cli::handle_command(&args, db) {
        CliAction::None => {}
        CliAction::RunDaemon(db, verbose) => {
            // Only an exit the user asked for is a normal one. The other two
            // leave a non-zero status so `systemd`'s `Restart=on-failure` (see
            // `contrib/systemd/y4p.service`) can act on them: previously every
            // path fell through to status 0, so a daemon that never started or
            // lost its compositor looked like a clean shutdown.
            match daemon::start_daemon(db, verbose) {
                daemon::DaemonExit::Clean => {
                    eprintln!("{}{}", LOG_ERROR, MSG_DAEMON_STOP);
                }
                daemon::DaemonExit::StartFailed => {
                    eprintln!("{}{}", LOG_ERROR, MSG_DAEMON_START_FAILED);
                    std::process::exit(1);
                }
                daemon::DaemonExit::ConnectionLost => {
                    eprintln!("{}{}", LOG_ERROR, MSG_DAEMON_STOP);
                    std::process::exit(1);
                }
            }
        }
        CliAction::Doctor(db) => {
            // The compositor handshake is the one check that needs the Wayland
            // backend; probing it here and handing the outcome over keeps every
            // line of the report in one place, in a fixed order.
            let compositor = wayland::create_connection().is_some();
            if cli::doctor::report(&db, compositor) {
                std::process::exit(1);
            }
        }
        CliAction::ListTypes => {
            // Observation only: the compositor's advertised types, one per
            // line. Nothing is stored and no pipe is opened.
            let mimes = wayland::list_offer_types();
            if mimes.is_empty() {
                eprintln!("{}no MIME types were advertised by the current selection", LOG_ERROR);
                std::process::exit(1);
            }

            let mut out = String::new();
            for mime in &mimes {
                out.push_str(mime);
                out.push('\n');
            }

            let mut stdout = std::io::stdout();
            if let Err(e) = stdout.write_all(out.as_bytes()) {
                eprintln!("{}standard output stream failure: {}", LOG_ERROR, e);
                return;
            }
            let _ = stdout.flush();
        }
        CliAction::PasteFrom(mime) => {
            // Synchronous data extraction from the Wayland compositor.
            let raw = wayland::paste_from_os(&mime);

            if raw.is_empty() {
                eprintln!("{}null or empty payload retrieved for MIME: {}", LOG_ERROR, mime);
                std::process::exit(1);
            }

            // Direct byte-stream output to the standard output buffer.
            let mut stdout = std::io::stdout();
            if let Err(e) = stdout.write_all(&raw) {
                eprintln!("{}standard output stream failure: {}", LOG_ERROR, e);
                return;
            }

            // Explicit flush to ensure complete data transmission before process exit.
            let _ = stdout.flush();
        }
    }
}
