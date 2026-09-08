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

/// Central command dispatcher. Standardized to use mutable references for database
/// operations to prevent ownership move conflicts across match arms.
pub fn handle_command(args: &[String], mut db: ClipboardDb) {
    // 1. Intercept global flags and help requests early
    if args.len() < 2 || utils::has_flag(args, "--help", "-h") {
        help::print_help();
        return;
    }

    if utils::has_flag(args, "--version", "-V") {
        help::print_version();
        return;
    }

    let cmd = args[1].as_str();

    // 2. Prevent option-formatted strings from being interpreted as commands
    if utils::is_option(cmd) {
        eprintln!("{}invalid command format: '{}'", LOG_ERROR, cmd);
        // BUGFIX: was "y1-clip" — leftover placeholder name, inconsistent
        // with the actual product name used everywhere else.
        println!("usage: y4p <command> [options]");
        std::process::exit(1);
    }

    // 3. Dispatch execution to specific command modules
    // Using references (&db / &mut db) allows mod.rs to retain ownership 
    // and ensures clean resource management.
    match cmd {
        // --- System Operations ---
        "daemon"     => daemon::run(args, db), // daemon consumes db as it is the final owner
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
        "paste-from" => paste_from::run(args),
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
}
