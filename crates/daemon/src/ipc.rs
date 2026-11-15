// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/daemon/src/ipc.rs

use crate::core::constants::*;
use crate::worker::DbWorker;
use std::io::{BufRead, Write};
use std::os::unix::net::{UnixListener, UnixStream};

/// Decoded IPC request; wire values match `IPC_CMD_*`.
pub enum Command {
    Restore(i64),
    Exit,
    Status,
    Pause,
    Resume,
    Delete(i64),
    Wipe,
}

impl Command {
    fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < 2 { return None; }
        let n = buf.len() - 1; // trailing IPC_DELIMITER
        match buf[0] {
            IPC_CMD_EXIT => Some(Command::Exit),
            IPC_CMD_STATUS => Some(Command::Status),
            IPC_CMD_PAUSE => Some(Command::Pause),
            IPC_CMD_RESUME => Some(Command::Resume),
            IPC_CMD_WIPE => Some(Command::Wipe),
            IPC_CMD_RESTORE => String::from_utf8_lossy(&buf[1..n])
                .trim()
                .parse::<i64>()
                .ok()
                .map(Command::Restore),
            // A `delete` names its target as decimal ASCII after the command
            // byte; anything else is a malformed request.
            IPC_CMD_DELETE => String::from_utf8_lossy(&buf[1..n])
                .trim()
                .parse::<i64>()
                .ok()
                .map(Command::Delete),
            _ => None,
        }
    }
}

/// Accept one pending connection, if any, and decode its command.
///
/// `Status`, `Delete` and `Wipe` are the bidirectional cases: each writes its
/// reply back on the same connection before returning, so a client never has to
/// wait for a second connection. Mutations go through the daemon's single
/// writer connection (`DbWorker`), which keeps every SQLite write serialised and
/// makes `wipe` reclaim memory in the process that actually holds it. Everything
/// else is fire-and-forget, as before.
pub fn accept_and_dispatch(
    listener: &UnixListener,
    writer: &DbWorker,
    status: impl FnOnce() -> String,
) -> Option<Command> {
    let (stream, _) = listener.accept().ok()?;
    let mut reader = std::io::BufReader::new(stream);
    let mut buf = Vec::new();
    reader.read_until(IPC_DELIMITER, &mut buf).ok()?;

    let cmd = Command::decode(&buf)?;
    match &cmd {
        Command::Status => {
            let mut stream = reader.into_inner();
            let _ = stream.write_all(status().as_bytes());
            let _ = stream.flush();
        }
        Command::Delete(id) => answer(reader.into_inner(), writer.delete_by_id(*id).map(|deleted| format!("OK:{}", deleted))),
        Command::Wipe => answer(reader.into_inner(), writer.wipe().map(|()| "OK".to_owned())),
        _ => {}
    }
    Some(cmd)
}

/// Writes a mutation's answer as exactly one line, terminated by
/// `IPC_DELIMITER`, so the CLI can read it with a single `read_line` under a
/// bounded timeout.
fn answer(mut stream: UnixStream, outcome: Result<String, String>) {
    let line = match outcome {
        Ok(payload) => payload,
        // A newline inside a diagnostic would split the reply into several
        // lines; the wire format is one line, so fold them.
        Err(e) => format!("ERR:{}", e.replace(['\n', '\r'], " ")),
    };
    let _ = stream.write_all(line.as_bytes());
    let _ = stream.write_all(&[IPC_DELIMITER]);
    let _ = stream.flush();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::metrics::DaemonMetrics;
    use crate::test_support::PrivateStore;
    use std::io::Read;
    use std::sync::Arc;

    /// Appends the frame delimiter to a request body.
    fn framed(bytes: &[u8]) -> Vec<u8> {
        let mut framed = bytes.to_vec();
        framed.push(IPC_DELIMITER);
        framed
    }

    // --- decoding ---

    #[test]
    fn a_delete_carries_its_target_as_decimal_ascii() {
        assert!(matches!(Command::decode(&framed(&[IPC_CMD_DELETE, b'4', b'2'])), Some(Command::Delete(42))));
        assert!(matches!(Command::decode(&framed(&[IPC_CMD_DELETE, b'7'])), Some(Command::Delete(7))));
    }

    #[test]
    fn a_wipe_needs_nothing_but_its_command_byte() {
        assert!(matches!(Command::decode(&framed(&[IPC_CMD_WIPE])), Some(Command::Wipe)));
    }

    #[test]
    fn a_malformed_delete_is_rejected_rather_than_guessed_at() {
        assert!(Command::decode(&framed(&[IPC_CMD_DELETE])).is_none(), "no target");
        assert!(Command::decode(&framed(&[IPC_CMD_DELETE, b'x'])).is_none(), "not a number");
        assert!(Command::decode(&framed(&[IPC_CMD_DELETE, b'1', b'.', b'5'])).is_none(), "not an integer");
        assert!(Command::decode(&[IPC_CMD_WIPE]).is_none(), "no delimiter means an incomplete request");
        assert!(Command::decode(&[0x7f, IPC_DELIMITER]).is_none(), "unknown command byte");
    }

    // --- dispatch round trip ---

    /// A real writer worker over a private store seeded with `rows` text
    /// records, listening on the private socket path.
    fn daemon(store: &PrivateStore, rows: &[&[u8]]) -> (UnixListener, DbWorker) {
        let mut db = store.open();
        for row in rows {
            db.insert_raw("text/plain", row).unwrap();
        }
        let listener = UnixListener::bind(store.socket_path()).unwrap();
        let worker = DbWorker::spawn(db, Arc::new(DaemonMetrics::new()), false, 100);
        (listener, worker)
    }

    /// Sends `request` as one client, dispatches it, and returns the single
    /// line the daemon answered with.
    fn exchange(store: &PrivateStore, listener: &UnixListener, worker: &DbWorker, request: &[u8]) -> (Command, String) {
        let mut client = UnixStream::connect(store.socket_path()).unwrap();
        client.write_all(request).unwrap();
        let cmd = accept_and_dispatch(listener, worker, String::new).unwrap();
        let mut reply = String::new();
        std::io::BufReader::new(client).read_line(&mut reply).unwrap();
        (cmd, reply)
    }

    /// `IPC_CMD_DELETE` followed by the ID in decimal ASCII.
    fn delete_request(id: i64) -> Vec<u8> {
        let mut request = vec![IPC_CMD_DELETE];
        request.extend_from_slice(id.to_string().as_bytes());
        request.push(IPC_DELIMITER);
        request
    }

    #[test]
    fn a_delete_is_applied_and_answered_on_the_same_connection() {
        let store = PrivateStore::new("ipc-delete");
        let (listener, worker) = daemon(&store, &[b"stays", b"goes"]);
        let target = store.rows()[0];

        let (cmd, reply) = exchange(&store, &listener, &worker, &delete_request(target));

        assert!(matches!(cmd, Command::Delete(id) if id == target));
        assert_eq!(reply, "OK:true\n");
        assert!(!store.rows().contains(&target), "the row is gone, on the writer connection");
    }

    #[test]
    fn a_delete_of_a_missing_row_answers_false_and_touches_nothing() {
        let store = PrivateStore::new("ipc-delete-missing");
        let (listener, worker) = daemon(&store, &[b"only row"]);
        let before = store.rows();

        let (_, reply) = exchange(&store, &listener, &worker, &delete_request(9999));

        assert_eq!(reply, "OK:false\n");
        assert_eq!(store.rows(), before);
    }

    #[test]
    fn a_wipe_empties_the_store_and_answers_ok() {
        let store = PrivateStore::new("ipc-wipe");
        let (listener, worker) = daemon(&store, &[b"first", b"second"]);
        assert_eq!(store.rows().len(), 2);

        let (cmd, reply) = exchange(&store, &listener, &worker, &framed(&[IPC_CMD_WIPE]));

        assert!(matches!(cmd, Command::Wipe));
        assert_eq!(reply, "OK\n");
        assert!(store.rows().is_empty(), "the history is empty");
    }

    #[test]
    fn a_status_request_still_answers_through_the_same_door() {
        let store = PrivateStore::new("ipc-status");
        let (listener, worker) = daemon(&store, &[]);

        let mut client = UnixStream::connect(store.socket_path()).unwrap();
        client.write_all(&framed(&[IPC_CMD_STATUS])).unwrap();
        let cmd = accept_and_dispatch(&listener, &worker, || "uptime: 1s\n".to_owned()).unwrap();
        assert!(matches!(cmd, Command::Status));

        let mut reply = String::new();
        std::io::BufReader::new(client).read_to_string(&mut reply).unwrap();
        assert_eq!(reply, "uptime: 1s\n");
    }

    #[test]
    fn a_fire_and_forget_command_gets_no_reply_and_still_dispatches() {
        let store = PrivateStore::new("ipc-exit");
        let (listener, worker) = daemon(&store, &[]);

        let mut client = UnixStream::connect(store.socket_path()).unwrap();
        client.write_all(&framed(&[IPC_CMD_EXIT])).unwrap();
        let cmd = accept_and_dispatch(&listener, &worker, || panic!("an exit must not be answered with a status")).unwrap();
        assert!(matches!(cmd, Command::Exit));

        let mut reply = String::new();
        std::io::BufReader::new(client).read_to_string(&mut reply).unwrap();
        assert!(reply.is_empty(), "nothing is written back");
    }
}
