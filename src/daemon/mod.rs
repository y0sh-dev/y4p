// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/daemon/mod.rs

mod ipc;
mod metrics;
mod worker;

use crate::storage::{ClipboardDb, ContentLocation};
use crate::wayland;
use crate::wayland::state::{WaylandState, SourceMetadata, SourcePayload};
use crate::core::constants::*;
use crate::core::SocketGuard;
use ipc::Command;
use metrics::DaemonMetrics;
use worker::DbWorker;
use std::os::unix::net::{UnixListener, UnixStream};
use std::io::Write;
use std::fs;
use std::os::fd::{AsFd, AsRawFd};
use std::time::Duration;
use std::sync::Arc;

/// Initialize and run the clipboard daemon with a unified, high-performance event loop.
///
/// Returns `true` if the daemon actually reached its serving state (bound
/// the socket, connected to the compositor, obtained the data-control
/// manager and a seat) at least once before its loop exited. Returns
/// `false` if it failed to start at all, so the caller can report an
/// accurate status instead of the previous unconditional "daemon process
/// terminated." (which was printed even on a startup failure that never
/// served a single request).
// `mut` is unused post-refactor (ownership moves straight into `DbWorker`)
// but kept to preserve this fn's exact external signature.
#[allow(unused_mut)]
pub fn start_daemon(mut db: ClipboardDb, verbose: bool) -> bool {
    let socket_path = crate::core::get_socket_path();

    if let Ok(mut stream) = UnixStream::connect(&socket_path) {
        let _ = stream.write_all(&[IPC_CMD_EXIT]);
        std::thread::sleep(Duration::from_millis(RECONNECT_DELAY_MS));
    }

    let _ = fs::remove_file(&socket_path);

    // BUGFIX: was `.expect(...)`, which — combined with `panic = "abort"` in
    // the release profile — turned an ordinary, recoverable failure (e.g.
    // another daemon instance genuinely still holding the socket, or a
    // permissions problem in the runtime directory) into a hard process
    // abort with a generic panic message instead of a clear diagnostic.
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{}failed to bind IPC socket at {}: {}", LOG_ERROR, socket_path.display(), e);
            return false;
        }
    };
    let _ = listener.set_nonblocking(true);
    let _guard = SocketGuard::new(socket_path);

    let metrics = Arc::new(DaemonMetrics::new());
    let max_history = crate::core::get_max_history();
    let writer = DbWorker::spawn(db, metrics.clone(), verbose, max_history);

    let Some((conn, mut event_queue)) = wayland::create_connection() else {
        eprintln!("{}{}", LOG_ERROR, MSG_WAYLAND_CONN_FAIL);
        return false;
    };
    let qh = event_queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    // Read-side connection, separate from the worker's writer connection.
    // WAL mode lets the two run concurrently without any in-process lock.
    // Kept as a local here (rather than inside `WaylandState`, per the
    // module-boundary fix) and threaded explicitly to whatever needs it.
    let read_db = match ClipboardDb::open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}failed to open read-side database handle: {}", LOG_ERROR, e);
            return false;
        }
    };
    let mut state = WaylandState::new_daemon(writer.sender(), verbose);

    // Pre-load last data for deduplication.
    state.last_data = read_db.get_latest_data().unwrap_or_default();
    state.target_mime = DEFAULT_MIME.to_string();

    if event_queue.roundtrip(&mut state).is_err() {
        eprintln!("{}{}", LOG_ERROR, MSG_WAYLAND_CONN_FAIL);
        return false;
    }
    if !bind_data_device(&mut state, &qh, &conn) {
        eprintln!("{}compositor does not advertise {} and/or {}; cannot serve clipboard.", LOG_ERROR, INTERFACE_MANAGER, INTERFACE_SEAT);
        return false;
    }

    println!("{}{}", LOG_INFO, MSG_DAEMON_READY);

    while !crate::core::is_exiting() {
        let _ = event_queue.dispatch_pending(&mut state);
        let _ = conn.flush();

        // EDGE CASE FIX: if the seat (or, in principle, the data-control
        // manager) was removed by the compositor — e.g. a session
        // suspend/resume or a seat hot-unplug — `wayland/handlers/mod.rs`
        // clears `state.device` on `GlobalRemove`. Previously nothing ever
        // re-created it: the device was only ever bound once, before this
        // loop started. If the seat later reappeared, the daemon would
        // silently sit in a broken state (bound to the socket, but unable
        // to monitor or serve the clipboard) until manually restarted.
        // Retrying this cheap, idempotent check every iteration lets the
        // daemon self-heal instead.
        if state.device.is_none() {
            bind_data_device(&mut state, &qh, &conn);
        }

        let mut poll_fds = [
            libc::pollfd { fd: conn.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: listener.as_fd().as_raw_fd(),  events: libc::POLLIN, revents: 0 },
        ];

        // SAFETY: `poll_fds` is a valid, correctly-sized array of `pollfd`
        // for `poll(2)` to read from and write `revents` back into.
        if unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, 500) } < 0 { continue; }

        // IPC Ingress Handling: Status replies inline via `accept_and_dispatch`;
        // Exit/Restore are dispatched here, same as before.
        if poll_fds[1].revents & libc::POLLIN != 0
            && let Some(cmd) = ipc::accept_and_dispatch(&listener, || metrics.format_status(state.paused)) {
                match cmd {
                    Command::Exit => crate::core::request_exit(),
                    Command::Restore(real_id) => handle_restore_request(&mut state, &qh, real_id, &conn, &metrics, &read_db),
                    Command::Status => {}
                    Command::Pause => {
                        state.paused = true;
                        if state.verbose { println!("{}{}", LOG_INFO, MSG_MONITOR_PAUSED); }
                    }
                    Command::Resume => {
                        state.paused = false;
                        if state.verbose { println!("{}{}", LOG_INFO, MSG_MONITOR_RESUMED); }
                    }
                }
        }

        if poll_fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 { break; }
        if poll_fds[0].revents & libc::POLLIN != 0
        && let Some(guard) = event_queue.prepare_read() {
            let _ = guard.read();
        }
    }

    true
}

/// Bind the data-control device from the currently-known manager + seat, if
/// both are available and a device isn't already bound. Returns whether a
/// device is bound after the call (regardless of whether this call itself
/// created it), so both the initial startup path and the per-iteration
/// self-healing check in `start_daemon` can share the same logic.
fn bind_data_device(
    state: &mut WaylandState,
    qh: &wayland_client::QueueHandle<WaylandState>,
    conn: &wayland_client::Connection,
) -> bool {
    if state.device.is_some() { return true; }

    if let (Some(manager), Some(seat)) = (&state.manager, &state.seat) {
        state.device = Some(manager.get_data_device(seat, qh, ()));
        let _ = conn.flush();
        true
    } else {
        false
    }
}

/// Serve a historical record with broad MIME compatibility.
///
/// Kernel-level egress: uses `locate_content` instead of always
/// materializing the payload into memory. Large binaries (images) living in
/// the on-disk cache are handed to the data source as a `SourcePayload::File`,
/// so `wayland/handlers/data_control/source.rs` can transfer them straight
/// from disk to the requesting client's pipe via `sendfile(2)` — this
/// process's heap never holds the full payload at all. Small/inline entries
/// (text) still go through the existing `get_content_by_id` (`Vec<u8>`) path,
/// since by construction (see `insert_with_hash`) they're never the
/// large-payload case this exists for.
///
/// `read_db` is passed explicitly rather than stored on `WaylandState` (the
/// wayland sensor layer must not depend on `storage`).
fn handle_restore_request(
    state: &mut WaylandState,
    qh: &wayland_client::QueueHandle<WaylandState>,
    real_id: i64,
    conn: &wayland_client::Connection,
    metrics: &DaemonMetrics,
    read_db: &ClipboardDb,
) {
    let resolved = read_db.locate_content(real_id);

    if let Some((mime, location)) = resolved
        && let Some(ref manager) = state.manager {
        state.provider_locks += 1;

        let payload = match location {
            ContentLocation::InlineBlob(id) => {
                let data = read_db.get_content_by_id(id)
                    .map(|(_, d)| d)
                    .unwrap_or_default();
                SourcePayload::Owned(data)
            }
            ContentLocation::CacheFile(path) => SourcePayload::File(path),
        };

        let meta = SourceMetadata { mime: mime.clone(), payload };

        let source = manager.create_data_source(qh, meta);

        // Broadcaster Strategy: advertise compatible MIMEs alongside the
        // stored one so the paste target can pick whichever it understands.
        source.offer(mime.clone());

        if mime.starts_with("image/") {
            if mime == "image/png" {
                for alt in IMAGE_MIME_ALTS {
                    if *alt != mime { source.offer(alt.to_string()); }
                }
            } else {
                // Non-PNG images: PNG is the broadest-compatibility fallback.
                source.offer("image/png".to_string());
            }
        } else if mime == "text/html" || mime == "application/xhtml+xml" {
            for alt in HTML_MIME_ALTS {
                if *alt != mime { source.offer(alt.to_string()); }
            }
        } else if mime.contains("text") || mime.contains("UTF8") {
            // Covers the text/plain family and text/uri-list (already
            // contains "text") with the same plain-text alternates.
            for alt in TEXT_MIME_ALTS {
                if *alt != mime { source.offer(alt.to_string()); }
            }
        }

        if let Some(ref device) = state.device {
            device.set_selection(Some(&source));
            let _ = conn.flush();
        }

        state.current_source = Some(source);
        metrics.record_egress();
        if state.verbose {
            println!("{}", log_restore(real_id as usize));
        }
    }
}
