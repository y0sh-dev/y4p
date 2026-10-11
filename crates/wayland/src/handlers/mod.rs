// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/wayland/src/handlers/mod.rs

pub mod data_control;
pub mod seat;

use wayland_client::{protocol::wl_registry, Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_client::protocol::wl_seat::WlSeat;
use crate::state::WaylandState;
use crate::core::constants::*;

impl Dispatch<wl_registry::WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match ev {
            wl_registry::Event::Global { name, interface, version } => {
                // Strict Version Negotiation: Ensure requested version <= advertised version.
                // Our implementation is compatible with version 1 of ext-data-control.
                if interface == INTERFACE_MANAGER && version >= 1 {
                    let manager = reg.bind::<ExtDataControlManagerV1, _, _>(name, 1, qh, ());
                    state.manager = Some(manager);
                    state.manager_id = Some(name);
                    
                    if state.verbose { println!("{}", log_protocol_bound(INTERFACE_MANAGER)); }
                }

                // Standard seat binding: Use the reported version for maximum feature set (within v7 capability).
                if interface == INTERFACE_SEAT && state.seat.is_none() {
                    let bind_version = version.min(7); // Max supported version by common client libs
                    let seat = reg.bind::<WlSeat, _, _>(name, bind_version, qh, ());
                    state.seat = Some(seat);
                    state.seat_id = Some(name);

                    if state.verbose { println!("{}", log_protocol_bound(INTERFACE_SEAT)); }
                }
            }

            wl_registry::Event::GlobalRemove { name } => {
                // Handle manager removal (rare, typically indicates compositor shutdown)
                if Some(name) == state.manager_id {
                    state.manager = None;
                    state.manager_id = None;
                }

                // Handle seat removal (occurs during re-log, suspend/resume, or hardware change)
                if Some(name) == state.seat_id {
                    state.seat = None;
                    state.seat_id = None;
                    // Critical: Clear the dependent device to trigger re-initialization
                    state.device = None;

                    if state.verbose { 
                        eprintln!("{}active wayland seat removed (ID: {}).", LOG_INFO, name); 
                    }
                }
            }
            _ => {}
        }
    }
}
