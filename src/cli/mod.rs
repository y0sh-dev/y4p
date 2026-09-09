// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/mod.rs

mod daemon;
mod list;
mod show;
mod copy_to;
mod paste_from;
mod store;
mod search;
mod cleaning;
mod pin;
mod unpin;
mod help;
mod status;
mod pause;
mod resume;
pub mod formatter;
mod utils;

use crate::storage::ClipboardDb;
use crate::core::constants::*;

/// Signal handed back to `main.rs` for the two commands whose real backend
/// call (`daemon::start_daemon`, `wayland::paste_from_os`) would otherwise
/// pull `daemon`/`wayland` into this storage+core-only frontend module.
/// `cli` only parses/validates; `main.rs` (the composition root) performs
/// the actual call.
pub enum CliAction {
    None,
    RunDaemon(ClipboardDb, bool),
    PasteFrom(String),
}

/// Central command dispatcher. Standardized to use mutable references for database
/// operations to prevent ownership move conflicts across match arms.
pub fn handle_command(args: &[String], mut db: ClipboardDb) -> CliAction {
    // 1. Intercept global flags and help requests early
    if args.len() < 2 || utils::has_flag(args, "--help", "-h") {
        help::print_help();
        return CliAction::None;
    }

    if utils::has_flag(args, "--version", "-V") {
        help::print_version();
        return CliAction::None;
    }

    let cmd = args[1].as_str();

    // 2. Prevent option-formatted strings from being interpreted as commands
    if utils::is_option(cmd) {
        eprintln!("{}invalid command format: '{}'", LOG_ERROR, cmd);
        println!("usage: y4p <command> [options]");
        std::process::exit(1);
    }

    // 3. Dispatch execution to specific command modules
    // Using references (&db / &mut db) allows mod.rs to retain ownership
    // and ensures clean resource management.
    match cmd {
        // --- System Operations ---
        // "daemon"/"paste-from" only resolve their arguments here; starting
        // the daemon / talking to wayland is deferred to `main.rs`.
        "daemon"     => return match daemon::run(args) {
            Some(verbose) => CliAction::RunDaemon(db, verbose),
            None => CliAction::None,
        },
        "list"       => list::run(args, &db),
        "search"     => search::run(args, &db),
        "show"       => show::run(args, &db),
        "copy-to"    => copy_to::run(args, &mut db),
        "store"      => store::run(args, &mut db),

        // --- Management ---
        "delete"     => cleaning::delete_run(args, &mut db),
        "wipe"       => cleaning::wipe_run(args, &mut db),
        "pin"        => pin::run(args, &mut db),
        "unpin"      => unpin::run(args, &mut db),

        // --- Utilities ---
        "paste-from" => return match paste_from::run(args) {
            Some(mime) => CliAction::PasteFrom(mime),
            None => CliAction::None,
        },
        "status"     => status::run(args),
        "pause"      => pause::run(args),
        "resume"     => resume::run(args),
        "help"       => help::print_help(),
        "version"    => help::print_version(),

        _ => {
            eprintln!("{}unknown command: '{}'", LOG_ERROR, cmd);
            println!("consult 'y4p help' for valid operations.");
            std::process::exit(1);
        }
    }

    CliAction::None
}
