// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/daemon/src/lib.rs

// Aliased to `core` / `storage` / `wayland` / `image` so the `crate::core::...`,
// `crate::storage::...`, `crate::wayland::...` and `crate::image::...` paths in
// the modules below resolve exactly as they did inside the monolith (import
// resolution only).
use y4p_core as core;
use y4p_image as image;
use y4p_storage as storage;
use y4p_wayland as wayland;

mod ipc;
pub mod metrics;
pub mod spool;
#[cfg(test)]
pub(crate) mod test_support;
pub mod worker;

use crate::storage::{ClipboardDb, ContentLocation};
use crate::core::payload::Payload;
use crate::wayland::state::{WaylandState, SourceMetadata};
use crate::core::constants::*;
use crate::core::utils::is_html_mime;
use crate::core::SocketGuard;
use crate::core::SocketLock;
use ipc::Command;
use metrics::DaemonMetrics;
use worker::DbWorker;
use std::os::unix::net::{UnixListener, UnixStream};
use std::io::{ErrorKind, Write};
use std::fs;
use std::os::fd::{AsFd, AsRawFd};
use std::path::Path;
use std::time::{Duration, Instant};
use std::sync::Arc;
use wayland_client::backend::{ReadEventsGuard, WaylandError};

/// How the daemon's run ended, so the caller can choose an exit status.
///
/// `systemd`'s `Restart=on-failure` restarts a unit only on a non-zero exit,
/// so a compositor that vanished must not be reported as a clean shutdown:
/// doing so would leave the user with no clipboard manager until the next
/// login. Only an exit that was actually asked for is clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonExit {
    /// SIGINT/SIGTERM, or an IPC `Exit` request: the daemon was asked to stop.
    Clean,
    /// The daemon never reached its serving state: the start-up lock, the IPC
    /// socket, the compositor or the database connection failed.
    StartFailed,
    /// The compositor connection ended while no exit had been requested.
    ConnectionLost,
}

/// How long a starting daemon waits for an already-running one to release the
/// start-up lock after being asked to exit. It must cover the exiting loop's
/// attention to the IPC command (its `poll` timeout is 500 ms) plus its guards,
/// and still be short enough that an unwedged `y4p daemon` does not hang.
const LOCK_WAIT_MS: u64 = 2000;
/// How often the start-up lock is retried while that wait runs.
const LOCK_RETRY_MS: u64 = 25;

/// Takes the daemon start-up lock, asking whoever holds it to exit first.
///
/// Starting the daemon while one is running replaces it, as it always has. The
/// running instance is therefore sent `IPC_CMD_EXIT` once its socket accepts
/// the connection, and the lock is retried until that instance has dropped its
/// `SocketGuard` (unlinked the socket) and its lock. The wait is bounded: a
/// daemon that does not get out of the way within `wait` is reported as a start
/// failure instead of being left to race on the socket.
fn acquire_start_lock(socket_path: &Path, lock_path: &Path, wait: Duration) -> Result<SocketLock, String> {
    let started = Instant::now();
    let mut asked = false;
    loop {
        match SocketLock::try_acquire(lock_path) {
            Ok(lock) => return Ok(lock),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // Ask once the socket is reachable: a daemon that is still
                // binding has no listener yet, so the attempt is retried with
                // the lock rather than counted as "asked".
                if !asked
                    && let Ok(mut stream) = UnixStream::connect(socket_path)
                {
                    let _ = stream.write_all(&[IPC_CMD_EXIT, IPC_DELIMITER]);
                    asked = true;
                }
                if started.elapsed() >= wait {
                    return Err(format!(
                        "another daemon still held {} after {} ms; giving up",
                        lock_path.display(),
                        wait.as_millis()
                    ));
                }
                std::thread::sleep(Duration::from_millis(LOCK_RETRY_MS));
            }
            Err(e) => return Err(format!("cannot lock {}: {}", lock_path.display(), e)),
        }
    }
}

/// Maps how the event loop ended to the reported outcome: a loop that stopped
/// without an exit request was cut short by the compositor connection, and that
/// is what the caller must be able to see in the exit status.
fn classify_loop_end(requested_exit: bool) -> DaemonExit {
    if requested_exit { DaemonExit::Clean } else { DaemonExit::ConnectionLost }
}

/// Initialise and run the clipboard daemon with a unified, high-performance event loop.
///
/// Returns how the run ended. `DaemonExit::StartFailed` means the daemon never
/// reached its serving state, and `DaemonExit::ConnectionLost` means the
/// compositor connection ended without an exit request; only
/// `DaemonExit::Clean` is a shutdown the user asked for. The caller turns the
/// first two into a non-zero process status so `systemd`'s
/// `Restart=on-failure` can act on them.
// `mut` is unused post-refactor (ownership moves straight into `DbWorker`)
// but kept to preserve this fn's exact external signature.
#[allow(unused_mut)]
pub fn start_daemon(mut db: ClipboardDb, verbose: bool) -> DaemonExit {
    let socket_path = crate::core::get_socket_path();
    let lock_path = SocketLock::path_for(&socket_path);

    // Declared before `_guard` on purpose: locals drop in reverse declaration
    // order, so the socket file is unlinked by the guard *before* the lock is
    // released here. A starting daemon that has taken the lock can therefore
    // never observe a socket that a live daemon still owns.
    let _lock = match acquire_start_lock(&socket_path, &lock_path, Duration::from_millis(LOCK_WAIT_MS)) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("{}{}", LOG_ERROR, e);
            return DaemonExit::StartFailed;
        }
    };

    // No other daemon can be serving now, so a socket still at this path is a
    // leftover from one that is gone.
    let _ = fs::remove_file(&socket_path);

    // BUGFIX: was `.expect(...)`, which — combined with `panic = "abort"` in
    // the release profile — turned an ordinary, recoverable failure (e.g. a
    // permissions problem in the runtime directory) into a hard process abort
    // with a generic panic message instead of a clear diagnostic.
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{}failed to bind IPC socket at {}: {}", LOG_ERROR, socket_path.display(), e);
            return DaemonExit::StartFailed;
        }
    };
    let _ = listener.set_nonblocking(true);
    let _guard = SocketGuard::new(socket_path);

    let metrics = Arc::new(DaemonMetrics::new());
    let max_history = crate::core::get_max_history();
    let writer = DbWorker::spawn(db, metrics.clone(), verbose, max_history);

    // Active configuration loaded once at startup and shared read-only across handlers.
    let config = Arc::new(crate::core::config::Config::load());

    let Some((conn, mut event_queue)) = wayland::create_connection() else {
        eprintln!("{}{}", LOG_ERROR, MSG_WAYLAND_CONN_FAIL);
        return DaemonExit::StartFailed;
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
            return DaemonExit::StartFailed;
        }
    };
    let mut state = WaylandState::new_daemon(writer.sender(), verbose, config);
    // Large payloads are drained to disk by the kernel instead of into the heap.
    // Without a usable spool directory the daemon still works: every payload is
    // then read into memory, as before.
    match spool::FileSpooler::open(crate::core::spool_dir_path()) {
        Ok(spooler) => state = state.with_spooler(Arc::new(spooler)),
        Err(e) => eprintln!("{}spool directory unavailable, large payloads will be buffered in memory: {}", LOG_WARN, e),
    }

    if event_queue.roundtrip(&mut state).is_err() {
        eprintln!("{}{}", LOG_ERROR, MSG_WAYLAND_CONN_FAIL);
        return DaemonExit::StartFailed;
    }
    if !bind_data_device(&mut state, &qh, &conn) {
        eprintln!("{}compositor does not advertise {} and/or {}; cannot serve clipboard.", LOG_ERROR, INTERFACE_MANAGER, INTERFACE_SEAT);
        return DaemonExit::StartFailed;
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

        // IPC Ingress Handling: Status, Delete and Wipe reply inline via
        // `accept_and_dispatch` (the mutations on the worker's own writer
        // connection, so every SQLite write stays serialised); Exit, Restore,
        // Pause and Resume are dispatched here, same as before.
        if poll_fds[1].revents & libc::POLLIN != 0
            && let Some(cmd) = ipc::accept_and_dispatch(&listener, &writer, || {
                metrics.format_status(
                    state.paused,
                    state.ingest_stats.shed_readers(),
                    state.ingest_stats.shed_queue(),
                    state.ingest_stats.shed_timeout(),
                )
            }) {
                match cmd {
                    Command::Exit => crate::core::request_exit(),
                    Command::Restore(real_id) => handle_restore_request(&mut state, &qh, real_id, &conn, &metrics, &read_db),
                    // Answered inside `accept_and_dispatch`, on the writer
                    // connection; nothing left to do here.
                    Command::Status | Command::Delete(_) | Command::Wipe => {}
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

        if poll_fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            // An exit that was already requested is not a lost connection; the
            // compositor may well be tearing down because the session is.
            if !crate::core::is_exiting() {
                eprintln!("{}compositor connection closed (poll reported POLLHUP/POLLERR).", LOG_ERROR);
            }
            break;
        }
        // `POLLHUP` is not the only way a connection ends. The backend stores
        // the first fatal error it meets (end of file, a protocol error, ...)
        // and fails every later read with it at once, whatever the socket
        // looks like. If the socket stays readable (`POLLIN` without `POLLHUP`)
        // while that error is ignored, `poll` returns immediately and this loop
        // spins at 100 % of a core. The read's own result is therefore the
        // authority: a fatal one ends the loop.
        if poll_fds[0].revents & libc::POLLIN != 0
            && let Some(guard) = event_queue.prepare_read()
            && !read_compositor_events(guard)
        {
            break;
        }
    }

    classify_loop_end(crate::core::is_exiting())
}

/// Reads what the compositor has sent. `false` means the connection is gone
/// and the event loop must stop; `true` means keep serving.
///
/// `WouldBlock` (another thread already read, or the wake-up was spurious) and
/// `Interrupted` are not failures. Anything else is: an I/O error, including
/// the `EPIPE` the backend reports for end of file, or a protocol error, after
/// which the compositor drops the client anyway.
fn read_compositor_events(guard: ReadEventsGuard) -> bool {
    match guard.read() {
        Ok(_) => true,
        Err(error) if !read_failure_is_fatal(&error) => true,
        Err(error) => {
            eprintln!("{}lost the compositor connection: {}", LOG_ERROR, error);
            false
        }
    }
}

fn read_failure_is_fatal(error: &WaylandError) -> bool {
    match error {
        WaylandError::Io(io) => !matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted),
        WaylandError::Protocol(_) => true,
    }
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
/// materialising the payload into memory. Large binaries (images) living in
/// the on-disk cache are handed to the data source as a `Payload::File`,
/// so `wayland/handlers/data_control/source.rs` can transfer them straight
/// from disk to the requesting client's pipe via `sendfile(2)` — this
/// process's heap never holds the full payload at all. Small/inline entries
/// (text) still go through the existing `get_content_by_id` (`Vec<u8>`) path,
/// since by construction (see `insert_with_hash`) they're never the
/// large-payload case this exists for.
///
/// A record whose cache file has vanished cannot be served: `locate_content`
/// then reports an error, which is logged and ends the attempt before a
/// provider is reserved or a data source created, so the failure is visible
/// rather than a paste of nothing.
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
    let resolved = match read_db.locate_content(real_id) {
        Ok(resolved) => resolved,
        Err(e) => {
            // Never offer a descriptor that cannot be opened: a record whose
            // payload is gone is left unserved, and nothing is reserved for it.
            eprintln!("{}cannot resolve record {} for egress: {}", LOG_ERROR, real_id, e);
            return;
        }
    };

    if let Some((mime, location)) = resolved
        && let Some(ref manager) = state.manager {
        let payload = match location {
            ContentLocation::InlineBlob(id) => {
                let data = read_db.get_content_by_id(id)
                    .map(|(_, d)| d)
                    .unwrap_or_default();
                Payload::Memory(data)
            }
            ContentLocation::CacheFile(path) => Payload::File(path),
        };

        let meta = SourceMetadata { mime: mime.clone(), payload };

        let source = manager.create_data_source(qh, meta);

        // Broadcaster Strategy: advertise compatible MIMEs alongside the
        // stored one so the paste target can pick whichever it understands.
        source.offer(mime.clone());

        if is_html_mime(&mime) {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use wayland_client::backend::protocol::ProtocolError;
    use wayland_client::Connection;

    fn io_error(kind: ErrorKind) -> WaylandError {
        WaylandError::Io(std::io::Error::from(kind))
    }

    #[test]
    fn a_transient_read_failure_is_not_fatal() {
        assert!(!read_failure_is_fatal(&io_error(ErrorKind::WouldBlock)));
        assert!(!read_failure_is_fatal(&io_error(ErrorKind::Interrupted)));
    }

    #[test]
    fn a_dead_connection_is_fatal() {
        for kind in [ErrorKind::BrokenPipe, ErrorKind::ConnectionReset, ErrorKind::UnexpectedEof, ErrorKind::NotConnected] {
            assert!(read_failure_is_fatal(&io_error(kind)), "{kind:?} must end the event loop");
        }
        let protocol = ProtocolError { code: 0, object_id: 0, object_interface: String::new(), message: String::new() };
        assert!(read_failure_is_fatal(&WaylandError::Protocol(protocol)));
    }

    /// A client connection over a socketpair whose "compositor" end we hold.
    fn connection() -> (Connection, UnixStream) {
        let (client, server) = UnixStream::pair().unwrap();
        (Connection::from_socket(client).unwrap(), server)
    }

    fn poll_in(conn: &Connection) -> libc::c_short {
        let mut fds = [libc::pollfd { fd: conn.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        // SAFETY: `fds` is a valid, correctly-sized array of one `pollfd`;
        // a zero timeout makes the call return immediately.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) };
        assert!(ready >= 0);
        fds[0].revents
    }

    #[test]
    fn an_idle_connection_keeps_the_loop_running() {
        let (conn, _server) = connection();
        let queue = conn.new_event_queue::<()>();
        assert_eq!(poll_in(&conn) & libc::POLLIN, 0);
        // Nothing to read: a spurious wake-up is `WouldBlock`, which is fine.
        assert!(read_compositor_events(queue.prepare_read().unwrap()));
    }

    #[test]
    fn a_compositor_that_closed_its_end_stops_the_loop_instead_of_spinning() {
        let (conn, server) = connection();
        let queue = conn.new_event_queue::<()>();
        drop(server);

        // Readable at once (end of file), and the read has to report it.
        assert_ne!(poll_in(&conn) & libc::POLLIN, 0, "a closed peer makes the socket readable");
        assert!(!read_compositor_events(queue.prepare_read().unwrap()));
    }

    // --- daemon lifecycle: exit classification and start-up lock ---

    #[test]
    fn only_a_requested_exit_is_reported_as_clean() {
        assert_eq!(classify_loop_end(true), DaemonExit::Clean);
        assert_eq!(classify_loop_end(false), DaemonExit::ConnectionLost);
        assert_ne!(DaemonExit::StartFailed, DaemonExit::ConnectionLost);
    }

    #[test]
    fn a_lock_held_by_a_running_daemon_fails_the_start_at_the_deadline() {
        let dir = std::env::temp_dir().join(format!("y4p-start-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("y4p.sock");
        let lock_path = SocketLock::path_for(&socket);
        let wait = Duration::from_millis(60);

        let held = SocketLock::try_acquire(&lock_path).unwrap();
        let started = Instant::now();
        let err = acquire_start_lock(&socket, &lock_path, wait).unwrap_err();
        assert!(started.elapsed() >= wait, "the start must wait for the deadline before giving up");
        assert!(err.contains("still held"), "{err}");

        // Once the holder is gone the same call succeeds immediately.
        drop(held);
        assert!(acquire_start_lock(&socket, &lock_path, wait).is_ok());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unlocked_socket_starts_without_asking_anyone_to_exit() {
        let dir = std::env::temp_dir().join(format!("y4p-start-free-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("y4p.sock");

        let started = Instant::now();
        let lock = acquire_start_lock(&socket, &SocketLock::path_for(&socket), Duration::from_millis(2000)).unwrap();
        assert!(started.elapsed() < Duration::from_millis(500), "the common path must not wait");

        drop(lock);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_running_daemon_is_asked_to_exit_and_the_lock_is_taken_once_it_goes() {
        use std::io::Read;

        let dir = std::env::temp_dir().join(format!("y4p-start-handover-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("y4p.sock");
        let lock_path = SocketLock::path_for(&socket);

        // Stand in for the running daemon: it owns the socket and the lock.
        let listener = UnixListener::bind(&socket).unwrap();
        let held = SocketLock::try_acquire(&lock_path).unwrap();

        std::thread::scope(|scope| {
            let starter = scope.spawn(|| acquire_start_lock(&socket, &lock_path, Duration::from_secs(5)).is_ok());

            // It asks over the IPC socket rather than unlinking it...
            let (mut stream, _) = listener.accept().unwrap();
            let mut command = [0u8; 2];
            stream.read_exact(&mut command).unwrap();
            assert_eq!(
                command,
                [IPC_CMD_EXIT, IPC_DELIMITER],
                "a replacement daemon must send a delimited EXIT frame to the old one"
            );

            // ...and only takes the lock once the old daemon's guards have run.
            drop(held);
            assert!(starter.join().unwrap(), "the lock must be handed over");
        });

        fs::remove_dir_all(dir).unwrap();
    }
}
