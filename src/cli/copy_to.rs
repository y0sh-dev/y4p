// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/copy_to.rs

use crate::storage::ClipboardDb;
use crate::core::constants::*;
use crate::cli::utils::ArgContext;
use std::os::unix::net::UnixStream;
use std::io::Write;

/// Re-broadcast an entry to the system clipboard using MRU index or database ID.
pub fn run(args: &[String], db: &mut ClipboardDb) {
    let ctx = ArgContext::parse(args);

    if !ctx.unknown_flags.is_empty() || ctx.raw || ctx.full || ctx.force {
        eprintln!("{}command 'copy-to' does not support specified options.", LOG_ERROR);
        return;
    }

    let input_str = match ctx.positionals.first() {
        Some(s) => s,
        None => { eprintln!("{}missing ID.", LOG_ERROR); return; }
    };

    let real_id = match crate::cli::utils::resolve_target_id(input_str, ctx.use_id, db) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("{}{}", LOG_ERROR, e);
            return;
        }
    };

    // 3. Update MRU in DB
    let _ = db.update_timestamp(real_id);

    // 4. One-shot IPC: Send ID to daemon via socket
    match UnixStream::connect(crate::core::get_socket_path()) {
        Ok(mut stream) => {
            let mut payload = vec![IPC_CMD_RESTORE];
            payload.extend_from_slice(real_id.to_string().as_bytes());
            payload.push(IPC_DELIMITER);
            
            if stream.write_all(&payload).is_ok() {
                let _ = stream.flush();
                // Logs the resolved persistent ID (not the raw CLI input,
                // which may have been an MRU offset rather than an ID at all).
                if ctx.verbose { println!("{}", log_restore(real_id as usize)); }
            }
        }
        Err(_) => {
            eprintln!("{}daemon is not running.", LOG_ERROR);
        }
    }
}
