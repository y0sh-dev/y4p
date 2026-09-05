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
    let val = input_str.parse::<i64>().unwrap_or(-1);
    let real_id = if ctx.use_id { val } else {
        let meta = db.fetch_metadata(crate::core::get_max_history());
        meta.get(val as usize).map(|m| m.0).unwrap_or(-1)
    };

    if real_id == -1 { eprintln!("{}invalid ID.", LOG_ERROR); return; }

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
                if ctx.verbose { println!("{}", log_restore(val as usize)); }
            }
        }
        Err(_) => {
            eprintln!("{}daemon is not running.", LOG_ERROR);
        }
    }
}
