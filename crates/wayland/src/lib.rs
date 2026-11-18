// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/wayland/src/lib.rs

// Aliased to `core` so the `crate::core::...` paths in the modules below
// resolve exactly as they did inside the monolith (import resolution only).
use y4p_core as core;

pub mod state;
pub mod handlers;
pub mod ingest;

use wayland_client::{Connection, EventQueue};
pub use self::state::WaylandState;
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Instant, Duration};

/// Establish a connection to the Wayland compositor. `None` when no
/// compositor is reachable (e.g. `DISPLAY`/`WAYLAND_DISPLAY` unset) — the
/// caller decides how to report that instead of this sensor layer panicking.
pub fn create_connection() -> Option<(Connection, EventQueue<WaylandState>)> {
    let conn = Connection::connect_to_env().ok()?;
    let event_queue = conn.new_event_queue();
    Some((conn, event_queue))
}

/// Extract data from the system clipboard with strict timeout and lifecycle management.
/// Prevents indefinite hangs by using poll-based non-blocking dispatch.
pub fn paste_from_os(mime: &str) -> Vec<u8> {
    run_action_mode(|config| WaylandState::new_action(mime.to_string(), false, config))
        .map(|state| state.rx_buf)
        .unwrap_or_default()
}

/// The MIME types the current selection advertises, in the order the
/// compositor reported them.
///
/// This is the read-only counterpart of [`paste_from_os`] behind
/// `y4p paste-from --list-types`: it shares the same Action Mode connection and
/// poll loop, but the Selection handler records the offer's types instead of
/// receiving any bytes, so no pipe is ever opened. An empty vector means no
/// compositor, no selection, or a timeout.
pub fn list_offer_types() -> Vec<String> {
    run_action_mode(|config| WaylandState::new_list_types(false, config))
        .map(|state| state.offered_mimes)
        .unwrap_or_default()
}

/// Runs one Action Mode handshake and poll loop, returning the final state.
///
/// Shared by [`paste_from_os`] and [`list_offer_types`] so the connection,
/// manager/seat binding and timeout handling exist once. `configure` picks the
/// Action Mode variant; what the Selection handler stores in that state
/// (`rx_buf` or `offered_mimes`) is the caller's answer.
///
/// `None` means no compositor connection could be made, or the compositor did
/// not advertise both the data-control manager and a seat.
fn run_action_mode<F>(configure: F) -> Option<WaylandState>
where
    F: FnOnce(std::sync::Arc<crate::core::config::Config>) -> WaylandState,
{
    let (conn, mut event_queue) = create_connection()?;
    let qh = event_queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    // Initialise in action mode (DB-less). Loading configuration here
    // keeps one-shot paste-from consistent with the daemon's ingestion pipeline.
    let config = std::sync::Arc::new(crate::core::config::Config::load());
    let mut state = configure(config);

    // 1. Synchronise to bind initial protocols (Manager & Seat)
    let _ = event_queue.roundtrip(&mut state);

    let (Some(manager), Some(seat)) = (&state.manager, &state.seat) else {
        return None;
    };
    // zwlr_data_control_manager_v1 is used here to bypass focus requirements
    state.device = Some(manager.get_data_device(seat, &qh, ()));
    let _ = conn.flush();

    // 2. Poll-based acquisition loop with timeout protection
    let start_time = Instant::now();
    let timeout = Duration::from_secs(1); // 1-second absolute watchdog timeout

    let wayland_fd = conn.as_fd().as_raw_fd();
    let mut poll_fds = [libc::pollfd {
        fd: wayland_fd,
        events: libc::POLLIN,
        revents: 0
    }];

    while !state.selection_received {
        // Check if absolute timeout has been reached
        if start_time.elapsed() >= timeout {
            break;
        }
        // Push outgoing requests (e.g., receive) to the compositor
        let _ = conn.flush();

        // Wait for FD activity with a 200ms sub-timeout
        // SAFETY: `poll_fds` is a valid, correctly-sized array of `pollfd`
        // for `poll(2)` to read from and write `revents` back into.
        let poll_res = unsafe { libc::poll(poll_fds.as_mut_ptr(), 1, 200) };

        if poll_res > 0 {
            // Data is available on the socket; read into user-space buffer
            if let Some(guard) = event_queue.prepare_read() {
                match guard.read() {
                    Ok(_) => {
                        // Dispatch the events now residing in the internal queue
                        let _ = event_queue.dispatch_pending(&mut state);
                    }
                    Err(wayland_client::backend::WaylandError::Io(ie)) if ie.kind() == std::io::ErrorKind::WouldBlock => {
                        // Ignore transient EAGAIN
                    }
                    Err(_) => break, // Connection severed
                }
            }
        } else if poll_res < 0 {
            // Handle critical poll errors (excluding interruptions)
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                break;
            }
        }
        // Final attempt to dispatch any pending events in the library queue
        let _ = event_queue.dispatch_pending(&mut state);
    }

    Some(state)
}
