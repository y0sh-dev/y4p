// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/daemon/ipc.rs

use crate::core::constants::*;
use std::io::{BufRead, Write};
use std::os::unix::net::UnixListener;

/// Decoded IPC request; wire values match `IPC_CMD_*`.
pub enum Command {
    Restore(i64),
    Exit,
    Status,
    Pause,
    Resume,
}

impl Command {
    fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() <= 1 { return None; }
        let n = buf.len() - 1; // trailing IPC_DELIMITER
        match buf[0] {
            IPC_CMD_EXIT => Some(Command::Exit),
            IPC_CMD_STATUS => Some(Command::Status),
            IPC_CMD_PAUSE => Some(Command::Pause),
            IPC_CMD_RESUME => Some(Command::Resume),
            IPC_CMD_RESTORE => String::from_utf8_lossy(&buf[1..n])
                .trim()
                .parse::<i64>()
                .ok()
                .map(Command::Restore),
            _ => None,
        }
    }
}

/// Accept one pending connection, if any, and decode its command. `Status`
/// is the only bidirectional case: it writes `status()`'s result back
/// before returning. Everything else is fire-and-forget, as before.
pub fn accept_and_dispatch(listener: &UnixListener, status: impl FnOnce() -> String) -> Option<Command> {
    let (stream, _) = listener.accept().ok()?;
    let mut reader = std::io::BufReader::new(stream);
    let mut buf = Vec::new();
    reader.read_until(IPC_DELIMITER, &mut buf).ok()?;

    let cmd = Command::decode(&buf)?;
    if let Command::Status = cmd {
        let mut stream = reader.into_inner();
        let _ = stream.write_all(status().as_bytes());
        let _ = stream.flush();
    }
    Some(cmd)
}
