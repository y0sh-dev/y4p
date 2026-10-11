// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/wayland/src/handlers/seat.rs

use wayland_client::{protocol::wl_seat, Connection, Dispatch, QueueHandle};
use crate::state::WaylandState;
use crate::core::constants::*;

impl Dispatch<wl_seat::WlSeat, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // Stability: Handle notifications for seat capabilities (pointer, keyboard, touch)
            wl_seat::Event::Capabilities { capabilities } => {
                let caps = capabilities.into_result();
                
                if state.verbose {
                    // Process events exclusively when capabilities are parsed successfully
                    if let Ok(real_caps) = caps {
                        let mut cap_list = Vec::new();
                        if real_caps.contains(wl_seat::Capability::Pointer)  { cap_list.push("pointer"); }
                        if real_caps.contains(wl_seat::Capability::Keyboard) { cap_list.push("keyboard"); }
                        if real_caps.contains(wl_seat::Capability::Touch)    { cap_list.push("touch"); }
                        
                        let caps_str = cap_list.join(", ");
                        
                        // Output structured runtime metrics via standard print macros
                        println!("Seat capabilities changed: [{}]", caps_str);
                    } else {
                        println!("Failed to parse seat capabilities.");
                    }
                }
            }

            // Robustness: Handle seat identification name strings (e.g., "seat0")
            wl_seat::Event::Name { name } if state.verbose => {
                    // Pinpoint active tracking targets to streamline diagnostic processes
                    // and eliminate silent ingestion deadlocks in multi-seat profiles.
                    println!("{}", log_seat_detected(&name, "identified"));
            }
            _ => {}
        }
    }
}
