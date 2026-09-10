// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/wayland/mod.rs

pub mod state;
pub mod handlers;

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
    let Some((conn, mut event_queue)) = create_connection() else { return Vec::new(); };
    let qh = event_queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    // Initialize in action mode (DB-less)
    let mut state = WaylandState::new_action(mime.to_string(), false);

    // 1. Synchronize to bind initial protocols (Manager & Seat)
    let _ = event_queue.roundtrip(&mut state);

    if let (Some(manager), Some(seat)) = (&state.manager, &state.seat) {
        // zwlr_data_control_manager_v1 is used here to bypass focus requirements
        state.device = Some(manager.get_data_device(seat, &qh, ()));
        let _ = conn.flush();
    } else {
        return Vec::new();
    }
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
    // Return the resulting buffer; empty if no data was captured within the window
    state.rx_buf
}
