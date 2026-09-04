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

mod core;
mod storage;
mod wayland;
mod daemon;
mod cli;

fn main() {
    // Ignore SIGPIPE: a reader disappearing mid-write (e.g. a Wayland client
    // closing the destination fd of an offer.receive()) must surface as an
    // EPIPE on the write, never as process termination.
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
    ctrlc::set_handler(move || {
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
    }).expect("failed to set signal handler");

    // Robustness: handle database open errors without panicking.
    let db = match storage::ClipboardDb::open() {
        Ok(database) => database,
        Err(e) => {
            eprintln!("{}critical: {}", crate::core::constants::LOG_ERROR, e);
            std::process::exit(1);
        }
    };

    cli::handle_command(&args, db);
}
